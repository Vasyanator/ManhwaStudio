/*
File: page_ops/fs_exec.rs

Purpose:
Filesystem side of structural page operations: scans the chapter into a
`plan::ChapterSnapshot`, executes a `plan::PageOpPlan` as a journaled,
crash-safe two-phase transaction over BOTH trees (committed chapter dir and
the `_unsaved` staging mirror), and recovers an interrupted transaction from
the on-disk journal at project load.

Transaction protocol:
1. The full plan is written to `{chapter}/page_ops_journal.json` (the A slot,
   atomic temp+rename, fsync'd) BEFORE any other filesystem change.
2. Phase A (reversible): new files are staged as temps, every affected file is
   renamed to a unique temp in its own directory. A failure rolls phase A back
   and removes the journal.
3. A separate fsync'd `page_ops_journal.b.json` is created (the commit point),
   then the A slot is removed. If both survive a crash, recovery trusts B.
4. Phase B (idempotent roll-forward): temps are renamed to final names or
   moved into the chapter trash, journaled JSON bodies are written, discards
   removed, trash extras written.
5. Both journal slots are deleted (fsync'd directory) — the transaction is complete.

Recovery (`recover`): journal phase "a" -> roll BACK (the chapter returns to
its pre-op state); phase "b" -> roll FORWARD to completion. Both are
idempotent. Every phase-A rename is followed by a directory fsync on Unix, so
the B marker cannot become durable ahead of the rename set; recovery still
recognizes `from`/`temp`/`dest` states and fails closed on a missing artifact.

Key functions:
- execute(): scan + plan + run the transaction (worker thread only).
- recover(): resolve a pending journal (called from `ProjectData::load_internal`).

Notes:
Uses `std::fs` directly (not the `crate::storage` seam): the transaction needs
fsync and same-volume renames, which the seam does not model. The page manager
is a native-desktop feature; on wasm the journal never exists and `recover` is
an inert no-op. Directory fsync is best-effort and Unix-only, mirroring
`tabs/settings/mod.rs::fsync_parent_dir_best_effort`.
*/

use super::plan::{
    self, ChapterSnapshot, DetectionBlocks, DetectionFiles, JOURNAL_B_FILE_NAME,
    JOURNAL_FILE_NAME, MoveDest, NewPageContent, PageOpPlan, PlannedCreate, PlannedMove,
    TextInfoFile, TextInfoLocation, TreeSnapshot,
};
use super::{PageOpError, PageOpKind, PageOpOutcome};
use crate::project::{Page, ProjectPaths};
use crate::runtime_log;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Journal schema version; bump together with any incompatible plan change.
const JOURNAL_SCHEMA_VERSION: u32 = 1;

/// Transaction phase recorded in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum JournalPhase {
    /// Phase A (staging + renames to temps) may be in progress: roll back.
    #[serde(rename = "a")]
    A,
    /// Phase A completed; phase B (commit) may be in progress: roll forward.
    #[serde(rename = "b")]
    B,
}

/// On-disk journal: the complete plan plus the phase marker.
#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    schema_version: u32,
    phase: JournalPhase,
    /// Human-readable description of the operation, for diagnostics only.
    op_debug: String,
    plan: PageOpPlan,
}

/// The two durable journal slots used during the commit-point transition.
struct JournalPaths {
    a: std::path::PathBuf,
    b: std::path::PathBuf,
}

impl JournalPaths {
    fn new(project_dir: &Path) -> Self {
        Self {
            a: project_dir.join(JOURNAL_FILE_NAME),
            b: project_dir.join(JOURNAL_B_FILE_NAME),
        }
    }

    fn any_exists(&self) -> bool {
        self.a.exists() || self.b.exists()
    }

    /// Phase B wins when both slots exist: its durable creation is the commit
    /// point, while removal of the older A slot is only cleanup.
    fn recovery_path(&self) -> Option<&Path> {
        if self.b.exists() {
            Some(&self.b)
        } else if self.a.exists() {
            Some(&self.a)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points (called from mod.rs).
// ---------------------------------------------------------------------------

/// Executes `op` on disk as a journaled transaction. See
/// [`super::execute_page_op`] for the public contract.
pub(crate) fn execute(
    paths: &ProjectPaths,
    pages: &[Page],
    op: &PageOpKind,
) -> Result<PageOpOutcome, PageOpError> {
    let title_dir = &paths.title_dir;
    let journal_paths = JournalPaths::new(&paths.project_dir);
    if journal_paths.any_exists() {
        return Err(PageOpError::Journal(format!(
            "a previous page operation left an unresolved journal in {}; \
             reopen the project so it can be recovered first",
            paths.project_dir.display()
        )));
    }

    validate_insert_sources(op)?;
    let snapshot = scan_chapter(paths, pages)?;
    let trash_id = current_trash_id();
    let plan = plan::build_plan(&snapshot, op, trash_id)?;
    for warning in &plan.warnings {
        runtime_log::log_warn(format!("[page-ops] {warning}"));
    }
    let outcome = PageOpOutcome {
        old_to_new: plan.old_to_new.clone(),
        new_page_count: plan.new_page_count,
    };
    verify_targets_free(title_dir, &plan)?;
    validate_plan(&plan)?;
    if plan.is_noop() {
        runtime_log::log_info(format!(
            "[page-ops] {op:?}: nothing to change on disk (no-op plan)"
        ));
        return Ok(outcome);
    }

    runtime_log::log_info(format!(
        "[page-ops] executing {op:?}: {} move(s), {} create(s), {} json rewrite(s), \
         {} trash extra(s); journal {}",
        plan.moves.len(),
        plan.creates.len(),
        plan.json_writes.len(),
        plan.trash_writes.len(),
        journal_paths.a.display()
    ));

    write_journal(&journal_paths, &plan, JournalPhase::A, op)?;
    if let Err(err) = run_phase_a(title_dir, &plan) {
        runtime_log::log_error(format!(
            "[page-ops] phase A failed ({err}); rolling back"
        ));
        return finish_failed_phase_a(&journal_paths, title_dir, &plan, err);
    }
    if let Err(err) = write_journal(&journal_paths, &plan, JournalPhase::B, op) {
        // The commit point was never reached: undo phase A completely.
        runtime_log::log_error(format!(
            "[page-ops] could not advance the journal to phase B ({err}); rolling back"
        ));
        return finish_failed_phase_a(&journal_paths, title_dir, &plan, err);
    }
    // Commit point passed: from here on the operation only rolls FORWARD. On
    // error the journal is intentionally left in place — the next project
    // load completes the transaction via `recover`.
    run_phase_b(title_dir, &plan, false).map_err(|err| {
        runtime_log::log_error(format!(
            "[page-ops] phase B failed ({err}); the journal at {} stays for \
             roll-forward on the next project load",
            journal_paths.b.display()
        ));
        err
    })?;
    remove_journals(&journal_paths)?;
    runtime_log::log_info(format!(
        "[page-ops] {op:?} committed: {} -> {} page(s)",
        plan.old_to_new.len(),
        plan.new_page_count
    ));
    Ok(outcome)
}

/// Resolves a pending journal in `project_dir`. See
/// [`super::recover_pending_page_op`] for the public contract.
pub(crate) fn recover(project_dir: &Path) -> Result<(), PageOpError> {
    let journal_paths = JournalPaths::new(project_dir);
    let Some(journal_path) = journal_paths.recovery_path() else {
        return Ok(());
    };
    let raw = fs::read_to_string(journal_path).map_err(|err| {
        io_ctx(&err, format!("read journal {}", journal_path.display()))
    })?;
    let journal: Journal = serde_json::from_str(&raw).map_err(|err| {
        PageOpError::Journal(format!(
            "journal {} is not readable ({err}); it was left in place for manual \
             inspection",
            journal_path.display()
        ))
    })?;
    if journal.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(PageOpError::Journal(format!(
            "journal {} has unsupported schema version {} (expected \
             {JOURNAL_SCHEMA_VERSION}); it was left in place for manual inspection",
            journal_path.display(),
            journal.schema_version
        )));
    }
    let expected_phase = if journal_path == journal_paths.b {
        JournalPhase::B
    } else {
        JournalPhase::A
    };
    if journal.phase != expected_phase {
        return Err(PageOpError::Journal(format!(
            "journal slot {} contains phase {:?}, expected {:?}; it was left in place",
            journal_path.display(), journal.phase, expected_phase
        )));
    }
    validate_plan(&journal.plan)?;
    // Plan paths are relative to the TITLE dir; mirror the fallback used when
    // `ProjectPaths` is built (`load_internal`).
    let title_dir = project_dir.parent().unwrap_or(project_dir);
    match journal.phase {
        JournalPhase::A => {
            runtime_log::log_warn(format!(
                "[page-ops] rolling BACK interrupted page operation ({}) in {}",
                journal.op_debug,
                project_dir.display()
            ));
            rollback_phase_a(title_dir, &journal.plan)?;
        }
        JournalPhase::B => {
            runtime_log::log_warn(format!(
                "[page-ops] rolling FORWARD interrupted page operation ({}) in {}",
                journal.op_debug,
                project_dir.display()
            ));
            run_phase_b(title_dir, &journal.plan, true)?;
        }
    }
    remove_journals(&journal_paths)?;
    runtime_log::log_info(format!(
        "[page-ops] interrupted page operation resolved in {}",
        project_dir.display()
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Scanning.
// ---------------------------------------------------------------------------

/// Verifies each `InsertFiles` source exists and decodes as an image (header
/// probe via `image::image_dimensions`). Extension validity is checked by the
/// pure planner.
fn validate_insert_sources(op: &PageOpKind) -> Result<(), PageOpError> {
    match op {
        PageOpKind::InsertFiles { files, .. } => {
            for file in files {
                image::image_dimensions(file).map_err(|err| {
                    PageOpError::Image(format!(
                        "inserted file '{}' is not a readable image: {err}",
                        file.display()
                    ))
                })?;
            }
            Ok(())
        }
        PageOpKind::Move { .. }
        | PageOpKind::CreateBlank { .. }
        | PageOpKind::Delete { .. } => Ok(()),
    }
}

/// Builds the plan input snapshot from the chapter on disk.
///
/// # Errors
/// - [`PageOpError::InvalidOp`] when the in-memory page list disagrees with
///   `src/` (a page file is missing) or the layout has no usable title dir.
/// - [`PageOpError::Json`] when an authoritative page-keyed document
///   (`translation_bubbles.json`, `layers.json`, `text_info.json`) is not
///   parseable — remapping it blindly would corrupt the chapter.
fn scan_chapter(paths: &ProjectPaths, pages: &[Page]) -> Result<ChapterSnapshot, PageOpError> {
    let title_dir = &paths.title_dir;
    let chapter_rel = rel_string(&paths.project_dir, title_dir)?;
    let unsaved_rel = rel_string(&paths.unsaved_dir, title_dir)?;

    let mut page_file_names = Vec::with_capacity(pages.len());
    for (pos, page) in pages.iter().enumerate() {
        if page.idx != pos {
            // The engine keys everything by list POSITION; a diverging stored
            // idx would mean the caller's snapshot is stale.
            return Err(PageOpError::InvalidOp(format!(
                "page list is inconsistent: entry #{pos} carries idx {}",
                page.idx
            )));
        }
        let name = page
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                PageOpError::InvalidOp(format!(
                    "page #{pos} has an unusable file name: {}",
                    page.path.display()
                ))
            })?;
        let on_disk = paths.src_dir.join(name);
        if !on_disk.is_file() {
            return Err(PageOpError::InvalidOp(format!(
                "page #{pos} file '{}' does not exist in src/ — reload the project \
                 before running page operations",
                on_disk.display()
            )));
        }
        page_file_names.push(name.to_string());
    }
    // `src/` must hold EXACTLY the images the caller's page list knows about:
    // an untracked image means the in-memory list is stale, and renaming a
    // page onto an untracked file's name would silently overwrite it.
    let known: BTreeSet<&str> = page_file_names.iter().map(String::as_str).collect();
    for name in list_file_names(&paths.src_dir)? {
        let ext = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        // Same image filter as `project::collect_images`.
        if matches!(ext.as_str(), "png" | "jpg" | "jpeg") && !known.contains(name.as_str()) {
            return Err(PageOpError::InvalidOp(format!(
                "src/ contains an image '{name}' that is not in the loaded page list — \
                 reload the project before running page operations"
            )));
        }
    }

    let committed = scan_tree(
        chapter_rel.clone(),
        &paths.clean_layers_dir,
        &paths.layers_dir,
        &paths.text_images_dir,
        &paths.bubbles_file,
    )?;
    let unsaved = scan_tree(
        unsaved_rel,
        &paths.unsaved_clean_layers_dir,
        &paths.unsaved_layers_dir,
        &paths.unsaved_text_images_dir,
        &paths.unsaved_bubbles_file,
    )?;
    let detection = scan_detection(&paths.text_detection_dir)?;

    Ok(ChapterSnapshot {
        chapter_rel,
        page_file_names,
        committed,
        unsaved,
        detection,
    })
}

/// Scans one tree (committed or unsaved). Missing directories/files yield
/// empty sets / `None`; unparseable authoritative JSON is an error.
fn scan_tree(
    tree_rel: String,
    clean_layers_dir: &Path,
    layers_dir: &Path,
    text_images_dir: &Path,
    bubbles_file: &Path,
) -> Result<TreeSnapshot, PageOpError> {
    let clean_overlay_stems = list_file_names(clean_layers_dir)?
        .into_iter()
        .filter_map(|name| {
            name.strip_suffix(".png")
                .map(str::to_string)
                .filter(|stem| !stem.is_empty())
        })
        .collect();
    let layers_files: BTreeSet<String> = list_file_names(layers_dir)?.into_iter().collect();
    let text_images_files: BTreeSet<String> =
        list_file_names(text_images_dir)?.into_iter().collect();

    let layers_manifest = read_json_if_exists(&layers_dir.join("layers.json"))?;

    let mut text_info = Vec::new();
    for (location, dir) in [
        (TextInfoLocation::LayersDir, layers_dir),
        (TextInfoLocation::TextImagesDir, text_images_dir),
    ] {
        let path = dir.join("text_info.json");
        if let Some(value) = read_json_if_exists(&path)? {
            let Value::Array(entries) = value else {
                return Err(PageOpError::Json(format!(
                    "{} is not a JSON array",
                    path.display()
                )));
            };
            text_info.push(TextInfoFile { location, entries });
        }
    }

    let bubbles = match read_json_if_exists(bubbles_file)? {
        Some(Value::Array(entries)) => Some(entries),
        Some(_) => {
            return Err(PageOpError::Json(format!(
                "{} is not a JSON array",
                bubbles_file.display()
            )));
        }
        None => None,
    };

    Ok(TreeSnapshot {
        tree_rel,
        clean_overlay_stems,
        layers_files,
        layers_manifest,
        text_images_files,
        text_info,
        bubbles,
    })
}

/// Scans `text_detection/` for per-page `{idx:05}_blocks.json` /
/// `{idx:05}_mask.png` pairs. An unparseable blocks file degrades to an
/// opaque rename (its dangling `mask_file` resolves gracefully on load).
fn scan_detection(dir: &Path) -> Result<Vec<DetectionFiles>, PageOpError> {
    let mut by_idx: std::collections::BTreeMap<usize, DetectionFiles> =
        std::collections::BTreeMap::new();
    for name in list_file_names(dir)? {
        if let Some(idx) = plan::parse_detection_blocks_page_idx(&name) {
            let path = dir.join(&name);
            let raw = fs::read_to_string(&path)
                .map_err(|err| io_ctx(&err, format!("read {}", path.display())))?;
            let blocks = match serde_json::from_str::<Value>(&raw) {
                Ok(value) => DetectionBlocks::Parsed(value),
                Err(_) => DetectionBlocks::Opaque,
            };
            by_idx
                .entry(idx)
                .or_insert_with(|| DetectionFiles {
                    page_idx: idx,
                    blocks: None,
                    has_mask: false,
                })
                .blocks = Some(blocks);
        } else if let Some(idx) = plan::parse_detection_mask_page_idx(&name) {
            by_idx
                .entry(idx)
                .or_insert_with(|| DetectionFiles {
                    page_idx: idx,
                    blocks: None,
                    has_mask: false,
                })
                .has_mask = true;
        }
    }
    Ok(by_idx.into_values().collect())
}

/// Lists regular-file names in `dir`; a missing directory yields an empty list.
fn list_file_names(dir: &Path) -> Result<Vec<String>, PageOpError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let entries =
        fs::read_dir(dir).map_err(|err| io_ctx(&err, format!("read dir {}", dir.display())))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|err| io_ctx(&err, format!("read dir {}", dir.display())))?;
        let file_type = entry
            .file_type()
            .map_err(|err| io_ctx(&err, format!("stat {}", entry.path().display())))?;
        if !file_type.is_file() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// Reads and parses a JSON file, `Ok(None)` when it does not exist.
fn read_json_if_exists(path: &Path) -> Result<Option<Value>, PageOpError> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)
        .map_err(|err| io_ctx(&err, format!("read {}", path.display())))?;
    let value = serde_json::from_str::<Value>(&raw).map_err(|err| {
        PageOpError::Json(format!("{} is not valid JSON: {err}", path.display()))
    })?;
    Ok(Some(value))
}

/// Title-relative path with '/' separators (the journal path format).
fn rel_string(path: &Path, base: &Path) -> Result<String, PageOpError> {
    let rel = path.strip_prefix(base).map_err(|_| {
        PageOpError::InvalidOp(format!(
            "'{}' is not under the title directory '{}'",
            path.display(),
            base.display()
        ))
    })?;
    let mut out = String::new();
    for component in rel.components() {
        let Some(part) = component.as_os_str().to_str() else {
            return Err(PageOpError::InvalidOp(format!(
                "path '{}' contains a non-UTF8 component",
                path.display()
            )));
        };
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(part);
    }
    if out.is_empty() {
        return Err(PageOpError::InvalidOp(format!(
            "chapter directory '{}' has no name relative to '{}'",
            path.display(),
            base.display()
        )));
    }
    Ok(out)
}

/// Pre-flight guard run before the journal is written: every plan destination
/// must be free or vacated by the plan itself (i.e. also a planned `from`).
/// A destination occupied by an UNTRACKED file means the caller's snapshot is
/// stale — committing would silently overwrite that file on Unix.
fn verify_targets_free(title_dir: &Path, plan: &PageOpPlan) -> Result<(), PageOpError> {
    let vacated: BTreeSet<&str> = plan.moves.iter().map(|m| m.from.as_str()).collect();
    let check = |target: &str| -> Result<(), PageOpError> {
        if !vacated.contains(target) && title_dir.join(target).exists() {
            return Err(PageOpError::InvalidOp(format!(
                "operation target '{target}' is already occupied by a file the \
                 operation does not track — reload the project and retry"
            )));
        }
        Ok(())
    };
    for planned in &plan.moves {
        match &planned.dest {
            MoveDest::Final { path } | MoveDest::Trash { path } => check(path)?,
            MoveDest::Discard => {}
        }
    }
    for create in &plan.creates {
        check(&create.target)?;
    }
    for write in &plan.json_writes {
        check(&write.target)?;
    }
    Ok(())
}

/// Rejects malformed or conflicting journal paths before recovery touches the
/// filesystem. Generated plans pass the same check before their first write.
fn validate_plan(plan: &PageOpPlan) -> Result<(), PageOpError> {
    fn validate_path(path: &str) -> Result<(), PageOpError> {
        let parsed = Path::new(path);
        if path.is_empty()
            || parsed.is_absolute()
            || parsed
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(PageOpError::Journal(format!(
                "journal contains unsafe non-relative path '{path}'"
            )));
        }
        Ok(())
    }

    let mut sources = HashSet::new();
    let mut temps = HashSet::new();
    let mut outputs = HashSet::new();
    validate_path(&plan.trash_root)?;
    for planned in &plan.moves {
        validate_path(&planned.from)?;
        validate_path(&planned.temp)?;
        if !sources.insert(planned.from.as_str()) {
            return Err(PageOpError::Journal(format!(
                "journal contains duplicate move source '{}'",
                planned.from
            )));
        }
        if !temps.insert(planned.temp.as_str()) {
            return Err(PageOpError::Journal(format!(
                "journal contains duplicate temp path '{}'",
                planned.temp
            )));
        }
        if let MoveDest::Final { path } | MoveDest::Trash { path } = &planned.dest {
            validate_path(path)?;
            if !outputs.insert(path.as_str()) {
                return Err(PageOpError::Journal(format!(
                    "journal contains duplicate output path '{path}'"
                )));
            }
        }
    }
    for create in &plan.creates {
        validate_path(&create.temp)?;
        validate_path(&create.target)?;
        if !temps.insert(create.temp.as_str()) || !outputs.insert(create.target.as_str()) {
            return Err(PageOpError::Journal(format!(
                "journal contains conflicting create paths '{}' -> '{}'",
                create.temp, create.target
            )));
        }
    }
    for write in &plan.json_writes {
        validate_path(&write.target)?;
        if !outputs.insert(write.target.as_str()) {
            return Err(PageOpError::Journal(format!(
                "journal contains duplicate output path '{}'",
                write.target
            )));
        }
    }
    for write in &plan.trash_writes {
        validate_path(&write.target)?;
        if !outputs.insert(write.target.as_str()) {
            return Err(PageOpError::Journal(format!(
                "journal contains duplicate output path '{}'",
                write.target
            )));
        }
    }
    if temps.iter().any(|path| sources.contains(path) || outputs.contains(path)) {
        return Err(PageOpError::Journal(
            "journal temp path conflicts with a source or output path".to_string(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Journal I/O.
// ---------------------------------------------------------------------------

/// Atomically writes/overwrites the journal (temp + rename + fsync of both the
/// file and, best-effort, its directory). Called before ANY other filesystem
/// change and again at the phase A -> B commit point.
fn write_journal(
    paths: &JournalPaths,
    plan: &PageOpPlan,
    phase: JournalPhase,
    op: &PageOpKind,
) -> Result<(), PageOpError> {
    let journal_path = match phase {
        JournalPhase::A => &paths.a,
        JournalPhase::B => &paths.b,
    };
    let journal = Journal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        phase,
        op_debug: format!("{op:?}"),
        plan: plan.clone(),
    };
    let payload = serde_json::to_string_pretty(&journal)
        .map_err(|err| PageOpError::Journal(format!("serialize journal: {err}")))?;
    atomic_write(journal_path, payload.as_bytes())?;
    fsync_dir_best_effort(journal_path.parent());
    if phase == JournalPhase::B && paths.a.exists() {
        fs::remove_file(&paths.a)
            .map_err(|err| io_ctx(&err, format!("remove phase-A journal {}", paths.a.display())))?;
        fsync_dir_best_effort(paths.a.parent());
    }
    Ok(())
}

/// Deletes both journal slots and flushes the directory entry (best-effort on
/// non-Unix). Completing this is what marks the transaction resolved.
fn remove_journals(paths: &JournalPaths) -> Result<(), PageOpError> {
    for journal_path in [&paths.b, &paths.a] {
        if journal_path.exists() {
            fs::remove_file(journal_path).map_err(|err| {
                io_ctx(&err, format!("remove journal {}", journal_path.display()))
            })?;
        }
    }
    fsync_dir_best_effort(paths.a.parent());
    Ok(())
}

/// Rolls back a pre-commit failure. The phase-A journal is removed only when
/// every reverse action succeeds; otherwise recovery evidence remains intact.
fn finish_failed_phase_a<T>(
    journal_paths: &JournalPaths,
    title_dir: &Path,
    plan: &PageOpPlan,
    primary: PageOpError,
) -> Result<T, PageOpError> {
    match rollback_phase_a(title_dir, plan) {
        Ok(()) => {
            remove_journals(journal_paths)?;
            Err(primary)
        }
        Err(rollback) => Err(PageOpError::Journal(format!(
            "{primary}; phase-A rollback also failed: {rollback}; journal retained for retry"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Phase A.
// ---------------------------------------------------------------------------

/// Stages created files and renames every affected file to its temp name.
/// Fully reversible until the journal advances to phase B.
fn run_phase_a(title_dir: &Path, plan: &PageOpPlan) -> Result<(), PageOpError> {
    // Stage creations first: they can fail (unreadable source, disk full)
    // without any original file having moved yet.
    for create in &plan.creates {
        stage_create(title_dir, create)?;
    }
    for planned in &plan.moves {
        let from = title_dir.join(&planned.from);
        let temp = title_dir.join(&planned.temp);
        if temp.exists() {
            return Err(PageOpError::Journal(format!(
                "temp path {} already exists; refusing to overwrite",
                temp.display()
            )));
        }
        fs::rename(&from, &temp).map_err(|err| {
            io_ctx(
                &err,
                format!("rename '{}' -> '{}'", from.display(), temp.display()),
            )
        })?;
        fsync_dir_best_effort(from.parent());
    }
    Ok(())
}

/// Undoes phase A: temps are renamed back to their original names, staged
/// creations are deleted. Idempotent (safe on a partially executed phase A).
fn rollback_phase_a(title_dir: &Path, plan: &PageOpPlan) -> Result<(), PageOpError> {
    let mut failures = Vec::new();
    for planned in &plan.moves {
        let from = title_dir.join(&planned.from);
        let temp = title_dir.join(&planned.temp);
        if !temp.exists() {
            continue; // This rename never happened (or was already undone).
        }
        if from.exists() {
            // Nothing else may claim original names during phase A; a conflict
            // means external interference — keep both files and report it.
            failures.push(format!(
                "both original '{}' and temp '{}' exist",
                from.display(), temp.display()
            ));
            continue;
        }
        if let Err(err) = fs::rename(&temp, &from) {
            failures.push(format!(
                "rollback '{}' -> '{}': {err}", temp.display(), from.display()
            ));
        } else {
            fsync_dir_best_effort(from.parent());
        }
    }
    for create in &plan.creates {
        let temp = title_dir.join(&create.temp);
        if temp.exists() {
            if let Err(err) = fs::remove_file(&temp) {
                failures.push(format!("remove staged file {}: {err}", temp.display()));
            } else {
                fsync_dir_best_effort(temp.parent());
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(PageOpError::Journal(failures.join("; ")))
    }
}

/// Writes the content of one created page to its staged temp path and flushes
/// it to stable storage.
fn stage_create(title_dir: &Path, create: &PlannedCreate) -> Result<(), PageOpError> {
    let temp = title_dir.join(&create.temp);
    if let Some(parent) = temp.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| io_ctx(&err, format!("create dir {}", parent.display())))?;
    }
    match &create.content {
        NewPageContent::CopyFile { source } => {
            fs::copy(source, &temp).map_err(|err| {
                io_ctx(
                    &err,
                    format!("copy '{}' -> '{}'", source.display(), temp.display()),
                )
            })?;
        }
        NewPageContent::BlankPng {
            width,
            height,
            rgba,
        } => {
            encode_blank_png(&temp, *width, *height, *rgba)?;
        }
    }
    fsync_file(&temp)
        .map_err(|err| io_ctx(&err, format!("fsync staged file {}", temp.display())))?;
    fsync_dir_best_effort(temp.parent());
    Ok(())
}

/// Encodes a solid-fill straight-RGBA PNG (fast service settings, matching
/// `project.rs::write_png_fast`).
fn encode_blank_png(path: &Path, width: u32, height: u32, rgba: [u8; 4]) -> Result<(), PageOpError> {
    let img = image::RgbaImage::from_pixel(width, height, image::Rgba(rgba));
    let file = fs::File::create(path)
        .map_err(|err| io_ctx(&err, format!("create {}", path.display())))?;
    let mut writer = BufWriter::new(file);
    let encoder =
        PngEncoder::new_with_quality(&mut writer, CompressionType::Fast, FilterType::NoFilter);
    image::ImageEncoder::write_image(
        encoder,
        img.as_raw(),
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|err| PageOpError::Image(format!("encode blank page {}: {err}", path.display())))?;
    writer
        .flush()
        .map_err(|err| io_ctx(&err, format!("flush {}", path.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase B.
// ---------------------------------------------------------------------------

/// Commits the plan. With `redo_a` (recovery), phase A is first re-applied
/// idempotently: a rename whose source still sits at its original path is
/// re-staged, one already resolved is skipped. Every step tolerates having
/// already run, so a crashed phase B can be re-driven to completion.
fn run_phase_b(title_dir: &Path, plan: &PageOpPlan, redo_a: bool) -> Result<(), PageOpError> {
    if redo_a {
        redo_phase_a(title_dir, plan)?;
    }
    // 1. Surviving files to their final names (targets freed by phase A).
    for planned in &plan.moves {
        if let MoveDest::Final { path } = &planned.dest {
            resolve_move(title_dir, planned, path)?;
        }
    }
    // 2. Created pages into place.
    for create in &plan.creates {
        let temp = title_dir.join(&create.temp);
        let target = title_dir.join(&create.target);
        if target.exists() {
            continue; // Already committed by a previous attempt.
        }
        fs::rename(&temp, &target).map_err(|err| {
            io_ctx(
                &err,
                format!("commit '{}' -> '{}'", temp.display(), target.display()),
            )
        })?;
    }
    // 3. Deleted artifacts into the trash.
    for planned in &plan.moves {
        if let MoveDest::Trash { path } = &planned.dest {
            let target = title_dir.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    io_ctx(&err, format!("create trash dir {}", parent.display()))
                })?;
            }
            resolve_move(title_dir, planned, path)?;
        }
    }
    // 4. Remapped JSON documents (bodies journaled at plan time).
    for write in &plan.json_writes {
        let target = title_dir.join(&write.target);
        atomic_write(&target, write.content.as_bytes())?;
    }
    // 5. Superseded originals.
    for planned in &plan.moves {
        if matches!(planned.dest, MoveDest::Discard) {
            let temp = title_dir.join(&planned.temp);
            if temp.exists() {
                fs::remove_file(&temp).map_err(|err| {
                    io_ctx(&err, format!("discard {}", temp.display()))
                })?;
            }
        }
    }
    // 6. Trash archives of deleted JSON entries.
    for write in &plan.trash_writes {
        let target = title_dir.join(&write.target);
        atomic_write(&target, write.content.as_bytes())?;
    }
    Ok(())
}

/// Recovery-only verification of the durable phase-A state. The executor
/// fsyncs every phase-A directory entry before creating B, so an original path
/// here is evidence of external interference or violated storage guarantees;
/// recovery fails closed instead of guessing file identity from path occupancy.
fn redo_phase_a(title_dir: &Path, plan: &PageOpPlan) -> Result<(), PageOpError> {
    for planned in &plan.moves {
        let from = title_dir.join(&planned.from);
        let temp = title_dir.join(&planned.temp);
        if temp.exists() {
            continue;
        }
        let resolved = match &planned.dest {
            MoveDest::Final { path } | MoveDest::Trash { path } => title_dir.join(path).exists(),
            MoveDest::Discard => true,
        };
        if !resolved {
            return Err(PageOpError::Journal(format!(
                "required transactional file '{}' is not staged or resolved{}",
                planned.from,
                if from.exists() { "; it unexpectedly remains at its original path" } else { "" }
            )));
        }
    }
    for create in &plan.creates {
        let temp = title_dir.join(&create.temp);
        let target = title_dir.join(&create.target);
        if !temp.exists() && !target.exists() {
            return Err(PageOpError::Journal(format!(
                "staged new page '{}' and destination '{}' are both missing; external \
                 insert sources are not trusted during recovery",
                create.temp, create.target
            )));
        }
    }
    Ok(())
}

/// Renames one temp to its destination, tolerating already-resolved and
/// externally-vanished files (the latter is logged loudly, never silent).
fn resolve_move(
    title_dir: &Path,
    planned: &PlannedMove,
    dest_rel: &str,
) -> Result<(), PageOpError> {
    let temp = title_dir.join(&planned.temp);
    let dest = title_dir.join(dest_rel);
    if temp.exists() {
        fs::rename(&temp, &dest).map_err(|err| {
            io_ctx(
                &err,
                format!("commit '{}' -> '{}'", temp.display(), dest.display()),
            )
        })?;
        return Ok(());
    }
    if dest.exists() {
        return Ok(()); // Already resolved by a previous attempt.
    }
    Err(PageOpError::Journal(format!(
        "required transactional file '{}' is missing from original '{}', temp '{}', \
         and destination '{}'",
        planned.from, planned.from, planned.temp, dest_rel
    )))
}

// ---------------------------------------------------------------------------
// Low-level durability helpers.
// ---------------------------------------------------------------------------

/// Millisecond-precision transaction id used for the trash subfolder and temp
/// names. A pre-1970 system clock degrades to 0 (temp collisions are then
/// still caught by the explicit existence check in phase A).
fn current_trash_id() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Writes `bytes` to `path` atomically: sibling temp file, fsync, rename. If a
/// stale destination blocks the rename (Windows), it is removed and the rename
/// retried — callers guarantee the destination is either absent or a
/// previous-attempt artifact of this same write.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PageOpError> {
    let parent = path.parent().ok_or_else(|| {
        PageOpError::Journal(format!("'{}' has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent)
        .map_err(|err| io_ctx(&err, format!("create dir {}", parent.display())))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            PageOpError::Journal(format!("'{}' has no usable file name", path.display()))
        })?;
    let temp = parent.join(format!("{file_name}.pageop-write.tmp"));
    {
        let mut file = fs::File::create(&temp)
            .map_err(|err| io_ctx(&err, format!("create {}", temp.display())))?;
        file.write_all(bytes)
            .map_err(|err| io_ctx(&err, format!("write {}", temp.display())))?;
        file.sync_all()
            .map_err(|err| io_ctx(&err, format!("fsync {}", temp.display())))?;
    }
    if let Err(first_err) = fs::rename(&temp, path) {
        if path.exists() {
            fs::remove_file(path).map_err(|err| {
                io_ctx(&err, format!("replace stale {}", path.display()))
            })?;
            fs::rename(&temp, path).map_err(|err| {
                io_ctx(
                    &err,
                    format!("rename '{}' -> '{}'", temp.display(), path.display()),
                )
            })?;
        } else {
            return Err(io_ctx(
                &first_err,
                format!("rename '{}' -> '{}'", temp.display(), path.display()),
            ));
        }
    }
    Ok(())
}

/// Reopens `path` and fsyncs its contents (precedent:
/// `tabs/settings/mod.rs::write_ort_load_state`).
fn fsync_file(path: &Path) -> std::io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// Best-effort directory fsync so renamed/created directory entries are
/// durable. Unix-only: std cannot fsync a directory handle on Windows
/// (mirrors `tabs/settings/mod.rs::fsync_parent_dir_best_effort`).
fn fsync_dir_best_effort(dir: Option<&Path>) {
    #[cfg(unix)]
    {
        let Some(dir) = dir else {
            return;
        };
        if dir.as_os_str().is_empty() {
            return;
        }
        match fs::File::open(dir) {
            Ok(handle) => {
                if let Err(err) = handle.sync_all() {
                    runtime_log::log_warn(format!(
                        "[page-ops] directory fsync failed for {} ({err})",
                        dir.display()
                    ));
                }
            }
            Err(err) => runtime_log::log_warn(format!(
                "[page-ops] could not open directory {} for fsync ({err})",
                dir.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        // No portable directory fsync outside Unix; the journal file itself is
        // still fsync'd and recovery tolerates lost directory entries.
        let _ = dir;
    }
}

/// Wraps an io error with operation context while keeping its `ErrorKind`.
fn io_ctx(err: &std::io::Error, context: String) -> PageOpError {
    PageOpError::Io(std::io::Error::new(err.kind(), format!("{context}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A disposable on-disk chapter with committed + unsaved trees populated
    /// with every artifact category the engine remaps.
    struct Fixture {
        _tmp: tempfile::TempDir,
        title: PathBuf,
        paths: ProjectPaths,
        pages: Vec<Page>,
    }

    const CHAPTER: &str = "ch1";

    fn project_paths(title: &Path, chapter: &str) -> ProjectPaths {
        let project_dir = title.join(chapter);
        let unsaved_dir = title.join(format!("{chapter}_unsaved"));
        ProjectPaths {
            project_dir: project_dir.clone(),
            title_dir: title.to_path_buf(),
            notes_file: title.join("notes.txt"),
            bubbles_file: project_dir.join(crate::config::BUBBLES_FILE),
            src_dir: project_dir.join(crate::config::SRC_DIR),
            clean_layers_dir: project_dir.join(crate::config::CLEAN_LAYERS_DIR),
            cleaned_dir: project_dir.join(crate::config::CLEANED_DIR),
            alt_vers_dir: project_dir.join(crate::config::ALT_VERS_DIR),
            saved_dir: project_dir.join(crate::config::SAVED_DIR),
            image_bubbles_dir: project_dir.join("image_bubbles"),
            text_images_dir: project_dir.join(crate::config::TEXT_IMAGES_DIR),
            layers_dir: project_dir.join(crate::config::LAYERS_DIR),
            text_detection_dir: project_dir.join(crate::config::TEXT_DETECTION_DIR),
            characters_dir: title.join("characters"),
            terms_file: title.join("terms.json"),
            settings_file: title.join("settings.json"),
            unsaved_dir: unsaved_dir.clone(),
            unsaved_bubbles_file: unsaved_dir.join(crate::config::BUBBLES_FILE),
            unsaved_clean_layers_dir: unsaved_dir.join(crate::config::CLEAN_LAYERS_DIR),
            unsaved_image_bubbles_dir: unsaved_dir.join("image_bubbles"),
            unsaved_text_images_dir: unsaved_dir.join(crate::config::TEXT_IMAGES_DIR),
            unsaved_layers_dir: unsaved_dir.join(crate::config::LAYERS_DIR),
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, bytes).expect("write fixture file");
    }

    fn write_json(path: &Path, value: &Value) {
        write(
            path,
            serde_json::to_string_pretty(value)
                .expect("serialize fixture json")
                .as_bytes(),
        );
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).expect("read json"))
            .expect("parse json")
    }

    /// Builds a 4-page chapter covering every remapped category:
    /// - `src/000.png .. 003.png` (distinct bytes so identity is trackable);
    /// - committed clean overlays for pages 0 and 2, unsaved overlay for page 1;
    /// - committed `layers/` with pages 0 and 2 (base/fx/text PNGs), unsaved
    ///   `layers/` with page 1;
    /// - committed `text_images/` with `text_info.json` (pages 1 and 3),
    ///   overlay PNG + `_layout.png` companion, typing mask for page 1;
    ///   unsaved typing mask for page 2;
    /// - `text_detection/` blocks+mask for page 1;
    /// - bubbles: committed pages 0/1/3 (page-crop bubble cropping page 1),
    ///   unsaved page 2.
    fn build_fixture() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let title = tmp.path().join("title");
        let paths = project_paths(&title, CHAPTER);

        for i in 0..4usize {
            write(
                &paths.src_dir.join(format!("{i:03}.png")),
                format!("SRC-PAGE-{i}").as_bytes(),
            );
        }
        write(&paths.clean_layers_dir.join("000.png"), b"CLEAN-0");
        write(&paths.clean_layers_dir.join("002.png"), b"CLEAN-2");
        write(&paths.unsaved_clean_layers_dir.join("001.png"), b"UNSAVED-CLEAN-1");

        write(&paths.layers_dir.join("ps_p0000_u1.png"), b"L-BASE-0");
        write(&paths.layers_dir.join("ps_p0000_u1_fx.png"), b"L-FX-0");
        write(&paths.layers_dir.join("ps_p0002_u2_text.png"), b"L-TEXT-2");
        write_json(
            &paths.layers_dir.join("layers.json"),
            &json!({
                "schema_version": 3,
                "pages": [
                    {"img_idx": 0, "tree": [
                        {"uid": "u1", "name": "L", "kind": "raster", "z": 0,
                         "visible": true, "opacity": 1.0,
                         "base_file": "ps_p0000_u1.png",
                         "rendered_file": "ps_p0000_u1_fx.png"}
                    ]},
                    {"img_idx": 2, "tree": [
                        {"uid": "u2", "name": "T", "kind": "text", "z": 0,
                         "visible": true, "opacity": 1.0,
                         "rendered_file": "ps_p0002_u2_text.png"}
                    ]}
                ]
            }),
        );
        write(&paths.unsaved_layers_dir.join("ps_p0001_uu.png"), b"UL-BASE-1");
        write_json(
            &paths.unsaved_layers_dir.join("layers.json"),
            &json!({
                "schema_version": 3,
                "pages": [
                    {"img_idx": 1, "tree": [
                        {"uid": "uu", "name": "U", "kind": "raster", "z": 0,
                         "visible": true, "opacity": 1.0,
                         "base_file": "ps_p0001_uu.png"}
                    ]}
                ]
            }),
        );

        write(&paths.text_images_dir.join("ov1.png"), b"OV-1");
        write(&paths.text_images_dir.join("ov1_layout.png"), b"OV-1-LAYOUT");
        write(&paths.text_images_dir.join("ov3.png"), b"OV-3");
        write(&paths.text_images_dir.join("mask_page_1.png"), b"TMASK-1");
        write_json(
            &paths.text_images_dir.join("text_info.json"),
            &json!([
                {"img_idx": 1, "file": "ov1.png", "img_u": 0.5, "img_v": 0.5},
                {"img_idx": 3, "file": "ov3.png", "img_u": 0.4, "img_v": 0.6}
            ]),
        );
        write(&paths.unsaved_text_images_dir.join("mask_page_2.png"), b"UTMASK-2");

        write_json(
            &paths.text_detection_dir.join("00001_blocks.json"),
            &json!({
                "source_size": [100, 200],
                "mask_size": [100, 200],
                "blocks": [{"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 4.0}],
                "mask_file": "00001_mask.png"
            }),
        );
        write(&paths.text_detection_dir.join("00001_mask.png"), b"DMASK-1");

        write_json(
            &paths.bubbles_file,
            &json!([
                {"id": 1, "img_idx": 0, "img_u": 0.5, "img_v": 0.5, "side": "left",
                 "text": "b1", "original_text": "o1"},
                {"id": 2, "img_idx": 1, "img_u": 0.5, "img_v": 0.5, "side": "left",
                 "text": "b2", "original_text": "o2"},
                {"id": 3, "img_idx": 3, "img_u": 0.5, "img_v": 0.5, "side": "right",
                 "text": "b3", "original_text": "o3",
                 "bubble_class": "image", "image_source_type": "page_crop",
                 "crop_page_idx": 1, "crop_rect": [0.1, 0.1, 0.9, 0.9]}
            ]),
        );
        write_json(
            &paths.unsaved_bubbles_file,
            &json!([
                {"id": 9, "img_idx": 2, "img_u": 0.2, "img_v": 0.3, "side": "left",
                 "text": "u9", "original_text": "uo9"}
            ]),
        );

        let pages = (0..4usize)
            .map(|i| Page {
                idx: i,
                path: paths.src_dir.join(format!("{i:03}.png")),
            })
            .collect();
        Fixture {
            _tmp: tmp,
            title,
            paths,
            pages,
        }
    }

    /// Recursively collects `rel-path -> file bytes` under `root`.
    fn walk(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn rec(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rec(root, &path, out);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .expect("under root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.insert(rel, fs::read(&path).expect("read file"));
                }
            }
        }
        let mut out = BTreeMap::new();
        rec(root, root, &mut out);
        out
    }

    fn assert_no_transaction_residue(title: &Path) {
        for (rel, _) in walk(title) {
            assert!(
                !rel.contains(super::super::plan::TEMP_PREFIX)
                    && !rel.contains(".pageop-write.tmp")
                    && !rel.ends_with(JOURNAL_FILE_NAME)
                    && !rel.ends_with(JOURNAL_B_FILE_NAME),
                "leftover transaction artifact: {rel}"
            );
        }
    }

    /// Shared assertions for `Move {{ from: 0, to: 3 }}` on the fixture
    /// (mapping 0->3, 1->0, 2->1, 3->2), used by both the direct-execute test
    /// and the mid-phase-B crash-recovery test.
    fn assert_moved_layout(fx: &Fixture) {
        // Source pages carry their content to the new canonical names.
        let src = |name: &str| fs::read(fx.paths.src_dir.join(name)).expect("src page");
        assert_eq!(src("003.png"), b"SRC-PAGE-0");
        assert_eq!(src("000.png"), b"SRC-PAGE-1");
        assert_eq!(src("001.png"), b"SRC-PAGE-2");
        assert_eq!(src("002.png"), b"SRC-PAGE-3");

        // Clean overlays follow their pages in both trees.
        assert_eq!(
            fs::read(fx.paths.clean_layers_dir.join("003.png")).expect("overlay"),
            b"CLEAN-0"
        );
        assert_eq!(
            fs::read(fx.paths.clean_layers_dir.join("001.png")).expect("overlay"),
            b"CLEAN-2"
        );
        assert!(!fx.paths.clean_layers_dir.join("000.png").exists());
        assert_eq!(
            fs::read(fx.paths.unsaved_clean_layers_dir.join("000.png")).expect("overlay"),
            b"UNSAVED-CLEAN-1"
        );
        assert!(!fx.paths.unsaved_clean_layers_dir.join("001.png").exists());

        // Layer PNGs carry the new page prefix in both trees.
        assert_eq!(
            fs::read(fx.paths.layers_dir.join("ps_p0003_u1.png")).expect("layer png"),
            b"L-BASE-0"
        );
        assert_eq!(
            fs::read(fx.paths.layers_dir.join("ps_p0003_u1_fx.png")).expect("layer png"),
            b"L-FX-0"
        );
        assert_eq!(
            fs::read(fx.paths.layers_dir.join("ps_p0001_u2_text.png")).expect("layer png"),
            b"L-TEXT-2"
        );
        assert_eq!(
            fs::read(fx.paths.unsaved_layers_dir.join("ps_p0000_uu.png"))
                .expect("layer png"),
            b"UL-BASE-1"
        );

        // Layer manifests: img_idx remapped, file references rewritten, sorted.
        let manifest = read_json(&fx.paths.layers_dir.join("layers.json"));
        let pages = manifest["pages"].as_array().expect("pages");
        assert_eq!(pages[0]["img_idx"], json!(1));
        assert_eq!(
            pages[0]["tree"][0]["rendered_file"],
            json!("ps_p0001_u2_text.png")
        );
        assert_eq!(pages[1]["img_idx"], json!(3));
        assert_eq!(pages[1]["tree"][0]["base_file"], json!("ps_p0003_u1.png"));
        assert_eq!(
            pages[1]["tree"][0]["rendered_file"],
            json!("ps_p0003_u1_fx.png")
        );
        let unsaved_manifest = read_json(&fx.paths.unsaved_layers_dir.join("layers.json"));
        assert_eq!(unsaved_manifest["pages"][0]["img_idx"], json!(0));
        assert_eq!(
            unsaved_manifest["pages"][0]["tree"][0]["base_file"],
            json!("ps_p0000_uu.png")
        );

        // text_info entries remapped; overlay PNG names untouched.
        let text_info = read_json(&fx.paths.text_images_dir.join("text_info.json"));
        let entries = text_info.as_array().expect("entries");
        assert_eq!(entries[0]["img_idx"], json!(0));
        assert_eq!(entries[0]["file"], json!("ov1.png"));
        assert_eq!(entries[1]["img_idx"], json!(2));
        assert!(fx.paths.text_images_dir.join("ov1.png").exists());
        assert!(fx.paths.text_images_dir.join("ov3.png").exists());

        // Typing masks in both trees.
        assert_eq!(
            fs::read(fx.paths.text_images_dir.join("mask_page_0.png")).expect("mask"),
            b"TMASK-1"
        );
        assert_eq!(
            fs::read(fx.paths.unsaved_text_images_dir.join("mask_page_1.png"))
                .expect("mask"),
            b"UTMASK-2"
        );

        // Text-detection pair renamed with the mask_file reference rewritten.
        let blocks = read_json(&fx.paths.text_detection_dir.join("00000_blocks.json"));
        assert_eq!(blocks["mask_file"], json!("00000_mask.png"));
        assert_eq!(blocks["source_size"], json!([100, 200]));
        assert_eq!(
            fs::read(fx.paths.text_detection_dir.join("00000_mask.png")).expect("dmask"),
            b"DMASK-1"
        );
        assert!(!fx.paths.text_detection_dir.join("00001_mask.png").exists());

        // Bubbles remapped in both trees (including the crop link 1 -> 0).
        let bubbles = read_json(&fx.paths.bubbles_file);
        let bubbles = bubbles.as_array().expect("bubbles");
        assert_eq!(bubbles[0]["id"], json!(1));
        assert_eq!(bubbles[0]["img_idx"], json!(3));
        assert_eq!(bubbles[1]["img_idx"], json!(0));
        assert_eq!(bubbles[2]["img_idx"], json!(2));
        assert_eq!(bubbles[2]["crop_page_idx"], json!(0));
        assert_eq!(bubbles[2]["crop_rect"], json!([0.1, 0.1, 0.9, 0.9]));
        let unsaved_bubbles = read_json(&fx.paths.unsaved_bubbles_file);
        assert_eq!(unsaved_bubbles[0]["img_idx"], json!(1));

        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn move_page_updates_every_artifact_in_both_trees() {
        let fx = build_fixture();
        let outcome = super::execute(&fx.paths, &fx.pages, &PageOpKind::Move { from: 0, to: 3 })
            .expect("move executes");
        assert_eq!(
            outcome.old_to_new,
            vec![Some(3), Some(0), Some(1), Some(2)]
        );
        assert_eq!(outcome.new_page_count, 4);
        assert_moved_layout(&fx);
    }

    #[test]
    fn delete_page_moves_artifacts_to_trash_and_prunes_json() {
        let fx = build_fixture();
        let outcome = super::execute(&fx.paths, &fx.pages, &PageOpKind::Delete {
            indices: vec![1],
        })
        .expect("delete executes");
        assert_eq!(
            outcome.old_to_new,
            vec![Some(0), None, Some(1), Some(2)]
        );
        assert_eq!(outcome.new_page_count, 3);

        // Surviving pages compacted onto canonical stems.
        assert_eq!(
            fs::read(fx.paths.src_dir.join("000.png")).expect("page"),
            b"SRC-PAGE-0"
        );
        assert_eq!(
            fs::read(fx.paths.src_dir.join("001.png")).expect("page"),
            b"SRC-PAGE-2"
        );
        assert_eq!(
            fs::read(fx.paths.src_dir.join("002.png")).expect("page"),
            b"SRC-PAGE-3"
        );
        assert!(!fx.paths.src_dir.join("003.png").exists());

        // The trash holds every artifact of the deleted page, with its
        // title-relative structure preserved.
        let trash_base = fx.paths.project_dir.join(super::super::plan::TRASH_DIR_NAME);
        let ids: Vec<_> = fs::read_dir(&trash_base)
            .expect("trash exists")
            .flatten()
            .collect();
        assert_eq!(ids.len(), 1, "one transaction trash folder");
        let trash = ids[0].path();
        let t = |rel: &str| trash.join(rel);
        assert_eq!(
            fs::read(t("ch1/src/001.png")).expect("trashed page"),
            b"SRC-PAGE-1"
        );
        assert_eq!(
            fs::read(t("ch1/text_images/ov1.png")).expect("trashed overlay"),
            b"OV-1"
        );
        assert_eq!(
            fs::read(t("ch1/text_images/ov1_layout.png")).expect("trashed layout"),
            b"OV-1-LAYOUT"
        );
        assert_eq!(
            fs::read(t("ch1/text_images/mask_page_1.png")).expect("trashed mask"),
            b"TMASK-1"
        );
        assert!(t("ch1/text_detection/00001_blocks.json").exists());
        assert!(t("ch1/text_detection/00001_mask.png").exists());
        assert_eq!(
            fs::read(t("ch1_unsaved/clean_layers/001.png")).expect("trashed overlay"),
            b"UNSAVED-CLEAN-1"
        );
        assert_eq!(
            fs::read(t("ch1_unsaved/layers/ps_p0001_uu.png")).expect("trashed png"),
            b"UL-BASE-1"
        );
        // Removed JSON entries are archived.
        let deleted_bubbles = read_json(&t("ch1/deleted_bubbles.json"));
        assert_eq!(deleted_bubbles[0]["id"], json!(2));
        let deleted_text_info = read_json(&t("ch1/text_images/deleted_text_info.json"));
        assert_eq!(deleted_text_info[0]["file"], json!("ov1.png"));
        let deleted_layers = read_json(&t("ch1_unsaved/layers/deleted_layers_pages.json"));
        assert_eq!(deleted_layers[0]["img_idx"], json!(1));

        // Committed bubbles: the deleted page's bubble is gone; the page-crop
        // bubble lost its crop link (its crop target was deleted).
        let bubbles = read_json(&fx.paths.bubbles_file);
        let bubbles = bubbles.as_array().expect("bubbles");
        assert_eq!(bubbles.len(), 2);
        assert_eq!(bubbles[0]["id"], json!(1));
        assert_eq!(bubbles[0]["img_idx"], json!(0));
        assert_eq!(bubbles[1]["id"], json!(3));
        assert_eq!(bubbles[1]["img_idx"], json!(2));
        assert!(bubbles[1].get("crop_page_idx").is_none());
        assert!(bubbles[1].get("crop_rect").is_none());

        // text_info keeps only the surviving entry, remapped.
        let text_info = read_json(&fx.paths.text_images_dir.join("text_info.json"));
        let entries = text_info.as_array().expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["img_idx"], json!(2));
        assert_eq!(entries[0]["file"], json!("ov3.png"));

        // Unsaved manifest lost its only (deleted) page.
        let unsaved_manifest = read_json(&fx.paths.unsaved_layers_dir.join("layers.json"));
        assert_eq!(
            unsaved_manifest["pages"].as_array().expect("pages").len(),
            0
        );

        // Detection artifacts of the deleted page left no renamed residue.
        assert!(!fx.paths.text_detection_dir.join("00001_blocks.json").exists());
        assert!(!fx.paths.text_detection_dir.join("00000_blocks.json").exists());

        // Typing mask of unsaved page 2 compacted to page 1.
        assert!(
            fx.paths
                .unsaved_text_images_dir
                .join("mask_page_1.png")
                .exists()
        );

        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn insert_files_at_start_shifts_everything() {
        let fx = build_fixture();
        // Real decodable images: `execute` header-probes insert sources.
        let ins_dir = fx.title.join("incoming");
        fs::create_dir_all(&ins_dir).expect("mkdir");
        let a = ins_dir.join("a.png");
        let b = ins_dir.join("b.PNG");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([1, 2, 3, 255]))
            .save(&a)
            .expect("save a");
        image::RgbaImage::from_pixel(2, 3, image::Rgba([9, 8, 7, 255]))
            .save(&b)
            .expect("save b");

        let outcome = super::execute(&fx.paths, &fx.pages, &PageOpKind::InsertFiles {
            at: 0,
            files: vec![a.clone(), b.clone()],
        })
        .expect("insert executes");
        assert_eq!(
            outcome.old_to_new,
            vec![Some(2), Some(3), Some(4), Some(5)]
        );
        assert_eq!(outcome.new_page_count, 6);

        // New pages sit at the canonical stems (lower-cased extension).
        assert_eq!(
            fs::read(fx.paths.src_dir.join("000.png")).expect("new page"),
            fs::read(&a).expect("src a")
        );
        assert_eq!(
            fs::read(fx.paths.src_dir.join("001.png")).expect("new page"),
            fs::read(&b).expect("src b")
        );
        // Old pages shifted.
        assert_eq!(
            fs::read(fx.paths.src_dir.join("002.png")).expect("page"),
            b"SRC-PAGE-0"
        );
        assert_eq!(
            fs::read(fx.paths.src_dir.join("005.png")).expect("page"),
            b"SRC-PAGE-3"
        );
        // A page-keyed sample from each category.
        assert!(fx.paths.clean_layers_dir.join("002.png").exists());
        assert!(fx.paths.layers_dir.join("ps_p0002_u1.png").exists());
        assert!(fx.paths.text_images_dir.join("mask_page_3.png").exists());
        assert!(fx.paths.text_detection_dir.join("00003_blocks.json").exists());
        let bubbles = read_json(&fx.paths.bubbles_file);
        assert_eq!(bubbles[0]["img_idx"], json!(2));
        assert_eq!(bubbles[2]["crop_page_idx"], json!(3));

        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn create_blank_at_end_writes_solid_png() {
        let fx = build_fixture();
        let outcome = super::execute(&fx.paths, &fx.pages, &PageOpKind::CreateBlank {
            at: 4,
            width: 4,
            height: 3,
            rgba: [10, 20, 30, 255],
        })
        .expect("blank executes");
        assert_eq!(
            outcome.old_to_new,
            vec![Some(0), Some(1), Some(2), Some(3)]
        );
        assert_eq!(outcome.new_page_count, 5);

        let blank = image::open(fx.paths.src_dir.join("004.png"))
            .expect("decode blank")
            .to_rgba8();
        assert_eq!((blank.width(), blank.height()), (4, 3));
        assert_eq!(blank.get_pixel(2, 1), &image::Rgba([10, 20, 30, 255]));

        // Nothing else moved (insert at the end is index-stable).
        assert_eq!(
            fs::read(fx.paths.src_dir.join("000.png")).expect("page"),
            b"SRC-PAGE-0"
        );
        let bubbles = read_json(&fx.paths.bubbles_file);
        assert_eq!(bubbles[0]["img_idx"], json!(0));

        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn stale_page_list_is_rejected_before_any_change() {
        let fx = build_fixture();
        // An image in src/ that the caller's page list does not know about
        // means the snapshot is stale: refuse before touching anything.
        write(&fx.paths.src_dir.join("004.png"), b"UNTRACKED");
        let before = walk(&fx.title);
        let err = super::execute(&fx.paths, &fx.pages, &PageOpKind::Move { from: 0, to: 3 })
            .expect_err("stale list must be rejected");
        assert!(matches!(err, PageOpError::InvalidOp(_)), "got: {err}");
        assert_eq!(before, walk(&fx.title), "nothing may change on rejection");
    }

    #[test]
    fn recover_is_a_noop_without_journal() {
        let fx = build_fixture();
        let before = walk(&fx.title);
        super::recover(&fx.paths.project_dir).expect("no-op recover");
        assert_eq!(before, walk(&fx.title));
    }

    #[test]
    fn crash_after_phase_a_rolls_back_to_original_state() {
        let fx = build_fixture();
        let before = walk(&fx.title);
        let op = PageOpKind::Move { from: 0, to: 3 };

        // Simulate a crash right after phase A: journal (phase "a") + all
        // phase-A renames done, phase B never started.
        let snapshot = scan_chapter(&fx.paths, &fx.pages).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 12345).expect("plan");
        let journal_paths = JournalPaths::new(&fx.paths.project_dir);
        let journal_path = journal_paths.a.clone();
        write_journal(&journal_paths, &plan, JournalPhase::A, &op).expect("journal");
        run_phase_a(&fx.title, &plan).expect("phase A");
        assert_ne!(before, walk(&fx.title), "phase A must change the tree");

        super::recover(&fx.paths.project_dir).expect("rollback");
        assert_eq!(before, walk(&fx.title), "rollback restores the exact state");
        assert!(!journal_path.exists());
    }

    #[test]
    fn crash_mid_phase_b_rolls_forward_to_final_state() {
        let fx = build_fixture();
        let op = PageOpKind::Move { from: 0, to: 3 };

        // Journal at phase "b" with phase A applied...
        let snapshot = scan_chapter(&fx.paths, &fx.pages).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 777).expect("plan");
        let journal_paths = JournalPaths::new(&fx.paths.project_dir);
        let journal_path = journal_paths.b.clone();
        write_journal(&journal_paths, &plan, JournalPhase::A, &op).expect("journal a");
        run_phase_a(&fx.title, &plan).expect("phase A");
        write_journal(&journal_paths, &plan, JournalPhase::B, &op).expect("journal b");

        // ...then only PART of phase B ran before the "crash": resolve just
        // the first half of the final moves, no JSON writes.
        let finals: Vec<&PlannedMove> = plan
            .moves
            .iter()
            .filter(|m| matches!(m.dest, MoveDest::Final { .. }))
            .collect();
        for planned in finals.iter().take(finals.len() / 2) {
            if let MoveDest::Final { path } = &planned.dest {
                resolve_move(&fx.title, planned, path).expect("partial B");
            }
        }
        super::recover(&fx.paths.project_dir).expect("roll forward");
        assert!(!journal_path.exists());
        // The chapter must be in the exact committed state of the operation.
        assert_moved_layout(&fx);
    }

    #[test]
    fn adjacent_move_stages_both_interdependent_renames_before_b_marker() {
        let fx = build_fixture();
        let op = PageOpKind::Move { from: 0, to: 1 };
        let snapshot = scan_chapter(&fx.paths, &fx.pages).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 901).expect("plan");
        let journal_paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&journal_paths, &plan, JournalPhase::A, &op).expect("journal a");
        run_phase_a(&fx.title, &plan).expect("phase A");
        let src_moves: Vec<_> = plan.moves.iter().filter(|planned| {
            planned.from == "ch1/src/000.png" || planned.from == "ch1/src/001.png"
        }).collect();
        assert_eq!(src_moves.len(), 2);
        assert!(src_moves.iter().all(|planned| fx.title.join(&planned.temp).exists()));
        assert!(src_moves.iter().all(|planned| !fx.title.join(&planned.from).exists()));
        write_journal(&journal_paths, &plan, JournalPhase::B, &op).expect("journal b");
        super::recover(&fx.paths.project_dir).expect("roll forward");
        assert_eq!(fs::read(fx.paths.src_dir.join("000.png")).expect("page 0"), b"SRC-PAGE-1");
        assert_eq!(fs::read(fx.paths.src_dir.join("001.png")).expect("page 1"), b"SRC-PAGE-0");
    }

    #[test]
    fn recovery_prefers_durable_b_slot_when_a_slot_also_exists() {
        let fx = build_fixture();
        let op = PageOpKind::Move { from: 0, to: 3 };
        let snapshot = scan_chapter(&fx.paths, &fx.pages).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 902).expect("plan");
        let paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&paths, &plan, JournalPhase::A, &op).expect("journal a");
        run_phase_a(&fx.title, &plan).expect("phase A");
        let journal = Journal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            phase: JournalPhase::B,
            op_debug: format!("{op:?}"),
            plan: plan.clone(),
        };
        let payload = serde_json::to_vec_pretty(&journal).expect("serialize b");
        atomic_write(&paths.b, &payload).expect("durable b slot");
        assert!(paths.a.exists() && paths.b.exists());

        super::recover(&fx.paths.project_dir).expect("B wins");
        assert_moved_layout(&fx);
    }

    #[test]
    fn failed_rollback_retains_journal_and_retry_finishes() {
        let fx = build_fixture();
        let before = walk(&fx.title);
        let op = PageOpKind::Move { from: 0, to: 3 };
        let snapshot = scan_chapter(&fx.paths, &fx.pages).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 903).expect("plan");
        let paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&paths, &plan, JournalPhase::A, &op).expect("journal a");
        run_phase_a(&fx.title, &plan).expect("phase A");
        let blocked = fx.title.join(&plan.moves[0].from);
        write(&blocked, b"external conflict");

        let err = finish_failed_phase_a::<()>(&paths, &fx.title, &plan, PageOpError::Journal("injected".to_string()))
            .expect_err("rollback conflict");
        assert!(matches!(err, PageOpError::Journal(_)));
        assert!(paths.a.exists(), "journal must survive partial rollback");
        fs::remove_file(blocked).expect("remove injected conflict");
        super::recover(&fx.paths.project_dir).expect("retry rollback");
        assert_eq!(before, walk(&fx.title));
    }

    #[test]
    fn recovery_does_not_recopy_missing_external_insert_source() {
        let fx = build_fixture();
        let source = fx.title.join("incoming.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&source)
            .expect("source");
        let op = PageOpKind::InsertFiles { at: 4, files: vec![source.clone()] };
        let snapshot = scan_chapter(&fx.paths, &fx.pages).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 904).expect("plan");
        let paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&paths, &plan, JournalPhase::A, &op).expect("journal a");
        run_phase_a(&fx.title, &plan).expect("phase A");
        write_journal(&paths, &plan, JournalPhase::B, &op).expect("journal b");
        fs::remove_file(fx.title.join(&plan.creates[0].temp)).expect("lose staged page");
        fs::remove_file(source).expect("external source disappears");

        let err = super::recover(&fx.paths.project_dir).expect_err("must not recopy source");
        assert!(matches!(err, PageOpError::Journal(_)), "got: {err}");
        assert!(paths.b.exists(), "B journal remains for inspection/retry");
    }

    #[test]
    fn unsafe_journal_path_is_rejected_without_filesystem_changes() {
        let fx = build_fixture();
        let op = PageOpKind::Move { from: 0, to: 3 };
        let snapshot = scan_chapter(&fx.paths, &fx.pages).expect("scan");
        let mut plan = plan::build_plan(&snapshot, &op, 905).expect("plan");
        plan.moves[0].from = "../outside.png".to_string();
        let paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&paths, &plan, JournalPhase::A, &op).expect("write adversarial journal");
        let before = walk(&fx.title);

        let err = super::recover(&fx.paths.project_dir).expect_err("unsafe path rejected");
        assert!(matches!(err, PageOpError::Journal(_)), "got: {err}");
        assert_eq!(before, walk(&fx.title), "recovery must not mutate the tree");
        assert!(paths.a.exists());
    }

    #[test]
    fn insert_at_end_then_create_blank_at_start_full_fixture() {
        let fx = build_fixture();
        let source = fx.title.join("tail.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([7, 8, 9, 255]))
            .save(&source)
            .expect("source");
        super::execute(&fx.paths, &fx.pages, &PageOpKind::InsertFiles {
            at: 4,
            files: vec![source.clone()],
        })
        .expect("append");
        let pages: Vec<Page> = (0..5).map(|idx| Page {
            idx,
            path: fx.paths.src_dir.join(format!("{idx:03}.png")),
        }).collect();
        super::execute(&fx.paths, &pages, &PageOpKind::CreateBlank {
            at: 0,
            width: 3,
            height: 2,
            rgba: [11, 12, 13, 255],
        })
        .expect("prepend blank");
        assert_eq!(image::open(fx.paths.src_dir.join("000.png")).expect("blank").to_rgba8().get_pixel(1, 1), &image::Rgba([11, 12, 13, 255]));
        assert_eq!(fs::read(fx.paths.src_dir.join("001.png")).expect("old first"), b"SRC-PAGE-0");
        assert_eq!(fs::read(fx.paths.src_dir.join("005.png")).expect("tail"), fs::read(source).expect("source bytes"));
        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn delete_multiple_non_adjacent_pages_full_fixture() {
        let fx = build_fixture();
        let outcome = super::execute(&fx.paths, &fx.pages, &PageOpKind::Delete {
            indices: vec![3, 1],
        }).expect("delete non-adjacent");
        assert_eq!(outcome.old_to_new, vec![Some(0), None, Some(1), None]);
        assert_eq!(fs::read(fx.paths.src_dir.join("000.png")).expect("page 0"), b"SRC-PAGE-0");
        assert_eq!(fs::read(fx.paths.src_dir.join("001.png")).expect("page 1"), b"SRC-PAGE-2");
        assert!(!fx.paths.src_dir.join("002.png").exists());
        assert_no_transaction_residue(&fx.title);
    }
}
