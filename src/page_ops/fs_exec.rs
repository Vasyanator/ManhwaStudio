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
   renamed to a unique temp in its own directory. Staging happens FIRST, so a
   composed page (a stitch) or a cropped part (a split) reads its source images
   at their original paths. A
   failure rolls phase A back and removes the journal.
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
- encode_composed_png(): the pixel work of a stitch (crop, scale, blend,
  encode), of a split (one source, crop == destination, so a bit-exact copy) and
  of a crop (rotate into the canvas, then crop). Recovery NEVER re-runs it: a
  lost staged file fails the transaction closed, exactly like a vanished
  external insert source.
- rotate_page_raster(): the crop's rotation — an exact pixel permutation for a
  quarter turn, an inverse-mapped resample for a fine angle (premultiplied
  bilinear for images, nearest neighbour for masks).

Notes:
Uses `std::fs` directly (not the `crate::storage` seam): the transaction needs
fsync and same-volume renames, which the seam does not model. The page manager
is a native-desktop feature; on wasm the journal never exists and `recover` is
an inert no-op. Directory fsync is best-effort and Unix-only, mirroring
`tabs/settings/mod.rs::fsync_parent_dir_best_effort`.
*/

use super::crop_geometry::{PageRotation, QuarterTurns, RotatedPage};
use super::plan::{
    self, ChapterSnapshot, ComposeSource, DetectionBlocks, DetectionFiles, JOURNAL_B_FILE_NAME,
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
/// v2 added `NewPageContent::ComposedPng` (stitched pages); v3 added
/// `ComposeSource::rotation` (the crop's rotate-then-crop recipe), which an
/// older binary would deserialize while silently ignoring the rotation — it
/// would then roll a half-finished crop forward with UNROTATED pixels, so the
/// version guard must reject such a journal rather than accept it.
const JOURNAL_SCHEMA_VERSION: u32 = 3;

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
    let snapshot = scan_chapter(paths, pages, op)?;
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
        // A stitch, a split and a crop read only chapter-owned files, all of
        // them already listed in the snapshot; there is no external source to
        // pre-validate.
        PageOpKind::Move { .. }
        | PageOpKind::CreateBlank { .. }
        | PageOpKind::Delete { .. }
        | PageOpKind::Stitch { .. }
        | PageOpKind::Split { .. }
        | PageOpKind::Crop { .. } => Ok(()),
    }
}

/// Builds the plan input snapshot from the chapter on disk.
///
/// Page pixel sizes are probed (an image-header read, not a decode) only for
/// operations that need page geometry — a stitch or a split — so the ordinary
/// rename operations keep costing zero extra I/O per page. A split additionally
/// probes the layer PNGs of the page it cuts (see [`scan_tree`]).
///
/// # Errors
/// - [`PageOpError::InvalidOp`] when the in-memory page list disagrees with
///   `src/` (a page file is missing) or the layout has no usable title dir.
/// - [`PageOpError::Image`] when a page image's header cannot be read while
///   probing sizes for a stitch or a split.
/// - [`PageOpError::Json`] when an authoritative page-keyed document
///   (`translation_bubbles.json`, `layers.json`, `text_info.json`) is not
///   parseable — remapping it blindly would corrupt the chapter.
fn scan_chapter(
    paths: &ProjectPaths,
    pages: &[Page],
    op: &PageOpKind,
) -> Result<ChapterSnapshot, PageOpError> {
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

    // A split and a crop need the pixel size of the layer PNGs of the page they
    // touch: a TEXT layer record stores none, and without it the exact-area rule
    // that routes a layer to a part — or decides whether the crop keeps it —
    // cannot be evaluated.
    let layer_probe_page_idx = match op {
        PageOpKind::Split { page_idx, .. } | PageOpKind::Crop { page_idx, .. } => Some(*page_idx),
        PageOpKind::Move { .. }
        | PageOpKind::InsertFiles { .. }
        | PageOpKind::CreateBlank { .. }
        | PageOpKind::Delete { .. }
        | PageOpKind::Stitch { .. } => None,
    };
    let committed = scan_tree(
        chapter_rel.clone(),
        &paths.clean_layers_dir,
        &paths.layers_dir,
        &paths.text_images_dir,
        &paths.bubbles_file,
        layer_probe_page_idx,
    )?;
    let unsaved = scan_tree(
        unsaved_rel,
        &paths.unsaved_clean_layers_dir,
        &paths.unsaved_layers_dir,
        &paths.unsaved_text_images_dir,
        &paths.unsaved_bubbles_file,
        layer_probe_page_idx,
    )?;
    let detection = scan_detection(&paths.text_detection_dir)?;

    // Exhaustive, NOT a `matches!`: a new pixel-identity operation that forgets
    // to ask for page sizes here fails deep inside `build_plan` with a
    // confusing "page N has no known pixel size", far from the cause.
    let needs_sizes = match op {
        PageOpKind::Stitch { .. } | PageOpKind::Split { .. } | PageOpKind::Crop { .. } => true,
        PageOpKind::Move { .. }
        | PageOpKind::InsertFiles { .. }
        | PageOpKind::CreateBlank { .. }
        | PageOpKind::Delete { .. } => false,
    };
    let mut page_sizes = Vec::new();
    if needs_sizes {
        page_sizes.reserve(page_file_names.len());
        for name in &page_file_names {
            let path = paths.src_dir.join(name);
            let (width, height) = image::image_dimensions(&path).map_err(|err| {
                PageOpError::Image(format!(
                    "could not read the pixel size of page image '{}': {err}",
                    path.display()
                ))
            })?;
            page_sizes.push([width, height]);
        }
    }

    Ok(ChapterSnapshot {
        chapter_rel,
        page_file_names,
        page_sizes,
        has_alt_vers: !list_dir_entry_names(&paths.alt_vers_dir)?.is_empty(),
        committed,
        unsaved,
        detection,
    })
}

/// Scans one tree (committed or unsaved). Missing directories/files yield
/// empty sets / `None`; unparseable authoritative JSON is an error.
///
/// `layer_probe_page_idx` is `Some` only for a split or a crop, and then the
/// pixel size of every layer PNG of THAT page is probed (an image-header read
/// per file of one page, never a decode). A PNG whose header cannot be read is
/// simply absent from the map; the planner degrades to a centre-point decision
/// and warns.
fn scan_tree(
    tree_rel: String,
    clean_layers_dir: &Path,
    layers_dir: &Path,
    text_images_dir: &Path,
    bubbles_file: &Path,
    layer_probe_page_idx: Option<usize>,
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

    let mut layer_png_sizes: std::collections::BTreeMap<String, [u32; 2]> =
        std::collections::BTreeMap::new();
    if let Some(page_idx) = layer_probe_page_idx {
        for name in &layers_files {
            if !name.ends_with(".png")
                || plan::parse_layers_png_page_idx(name) != Some(page_idx)
            {
                continue;
            }
            let path = layers_dir.join(name);
            match image::image_dimensions(&path) {
                Ok((width, height)) => {
                    layer_png_sizes.insert(name.clone(), [width, height]);
                }
                Err(err) => runtime_log::log_warn(format!(
                    "[page-ops] could not read the pixel size of layer image '{}' ({err});                      the operation will judge that layer by its centre point",
                    path.display()
                )),
            }
        }
    }

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
        layer_png_sizes,
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

/// Lists every entry name (files AND subdirectories) in `dir`; a missing
/// directory yields an empty list. Used to detect a non-empty `alt_vers/`,
/// whose content sits one level deeper than its per-version folders.
fn list_dir_entry_names(dir: &Path) -> Result<Vec<String>, PageOpError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let entries =
        fs::read_dir(dir).map_err(|err| io_ctx(&err, format!("read dir {}", dir.display())))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| io_ctx(&err, format!("read dir {}", dir.display())))?;
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
        NewPageContent::ComposedPng {
            width,
            height,
            background,
            sources,
        } => {
            encode_composed_png(title_dir, &temp, *width, *height, *background, sources)?;
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
    write_rgba_png(path, &img)
}

/// Composes `sources` onto a `width` x `height` background and writes the
/// result as a straight-RGBA PNG.
///
/// A SPLIT part is the degenerate one-source case: its crop equals its
/// destination, so the resize branch below is skipped and the pixels are copied
/// bit-exactly out of the source page.
///
/// A CROP is the same one-source case with a `rotation`: the decoded page is
/// first mapped into its ROTATED CANVAS (see [`rotate_page_raster`]) and the
/// crop then indexes THAT canvas, not the page. With `angle_deg == 0.0` the
/// rotation is an exact integer pixel permutation and the crop still copies
/// pixels, so the whole operation stays bit-exact; only a fine angle resamples.
///
/// Runs in phase A, BEFORE any rename, so every source is read at its original
/// chapter path. Sources are painted in list order with straight-alpha "over"
/// blending, so overlapping placements behave like stacked pages and a page
/// without a clean overlay leaves the background showing through. A source is
/// resized to the page size its crop is expressed in when the two disagree
/// (a clean overlay may have been attached with a same-aspect resize), and a
/// placement whose destination differs from its crop is resampled.
///
/// # Errors
/// - [`PageOpError::Image`] when a source cannot be decoded, its rotation is
///   out of range, or its crop or destination does not fit (the plan is then
///   internally inconsistent and the transaction must not proceed).
/// - [`PageOpError::Io`] when the PNG cannot be written.
fn encode_composed_png(
    title_dir: &Path,
    path: &Path,
    width: u32,
    height: u32,
    background: [u8; 4],
    sources: &[ComposeSource],
) -> Result<(), PageOpError> {
    // Bounded by the canvas limits the planner validates (200 MPx => ~800 MB).
    let mut canvas = image::RgbaImage::from_pixel(width, height, image::Rgba(background));
    for source in sources {
        let source_path = title_dir.join(&source.path);
        let decoded = image::open(&source_path)
            .map_err(|err| {
                PageOpError::Image(format!(
                    "could not decode '{}' while composing {}: {err}",
                    source_path.display(),
                    path.display()
                ))
            })?
            .to_rgba8();
        let [page_w, page_h] = source.page_size;
        let decoded = if decoded.width() == page_w && decoded.height() == page_h {
            decoded
        } else {
            // The crop rectangle is expressed in PAGE pixels; a page-keyed
            // raster of a different size (a same-aspect attached clean overlay)
            // must be brought to that size before it can be cropped.
            runtime_log::log_warn(format!(
                "[page-ops] '{}' is {}x{} but its page is {page_w}x{page_h}; \
                 resizing it before composing",
                source_path.display(),
                decoded.width(),
                decoded.height()
            ));
            // Same policy as every other resampling step of this source: a
            // CATEGORICAL raster must not be smoothed here either, or the
            // nearest-neighbour rotation below would already be handed a
            // greyscale input and the binarity guarantee would be gone before
            // it ran.
            image::imageops::resize(&decoded, page_w, page_h, source_filter(source))
        };
        // A rotating source reads its crop in the ROTATED CANVAS, so the page
        // must be mapped into that canvas before anything is cropped out of it.
        let (decoded, [source_w, source_h]) = match source.rotation {
            Some(rotation) => {
                let rotated = rotate_page_raster(
                    &decoded,
                    source.page_size,
                    rotation,
                    source.nearest,
                )
                .map_err(|err| {
                    PageOpError::Image(format!(
                        "could not rotate '{}' while composing {}: {err}",
                        source_path.display(),
                        path.display()
                    ))
                })?;
                let size = [rotated.width(), rotated.height()];
                (rotated, size)
            }
            None => (decoded, [page_w, page_h]),
        };
        let [crop_x, crop_y, crop_w, crop_h] = source.crop;
        let [dest_x, dest_y, dest_w, dest_h] = source.dest;
        let fits = crop_x.checked_add(crop_w).is_some_and(|r| r <= source_w)
            && crop_y.checked_add(crop_h).is_some_and(|b| b <= source_h)
            && dest_x.checked_add(dest_w).is_some_and(|r| r <= width)
            && dest_y.checked_add(dest_h).is_some_and(|b| b <= height)
            && crop_w > 0
            && crop_h > 0
            && dest_w > 0
            && dest_h > 0;
        if !fits {
            return Err(PageOpError::Image(format!(
                "compose recipe for {} places '{}' crop [{crop_x}, {crop_y}, {crop_w}, \
                 {crop_h}] of a {source_w}x{source_h} source (page {page_w}x{page_h}) at \
                 [{dest_x}, {dest_y}, {dest_w}, {dest_h}] of a {width}x{height} canvas, \
                 which does not fit",
                path.display(),
                source_path.display()
            )));
        }
        let cropped = image::imageops::crop_imm(&decoded, crop_x, crop_y, crop_w, crop_h);
        let placed = if crop_w == dest_w && crop_h == dest_h {
            cropped.to_image()
        } else {
            image::imageops::resize(&*cropped, dest_w, dest_h, source_filter(source))
        };
        image::imageops::overlay(&mut canvas, &placed, i64::from(dest_x), i64::from(dest_y));
    }
    write_rgba_png(path, &canvas)
}

/// The resampling filter one compose source may be scaled with.
///
/// A source flagged [`ComposeSource::nearest`] holds CATEGORICAL values (a
/// binary mask) and must never be smoothed at ANY step; everything else is
/// continuous imagery and uses Lanczos3.
#[must_use]
fn source_filter(source: &ComposeSource) -> image::imageops::FilterType {
    if source.nearest {
        image::imageops::FilterType::Nearest
    } else {
        image::imageops::FilterType::Lanczos3
    }
}

/// Maps a page-sized RGBA raster into its ROTATED CANVAS.
///
/// The canvas is the axis-aligned bounding box of the rotated page with the page
/// centred in it — computed by [`RotatedPage`], never re-derived here, so the
/// pixels and the plan's coordinates cannot disagree. Area of the canvas the
/// rotated page does not cover is left fully TRANSPARENT; the compose step then
/// paints the result over its own background (transparent for a page or clean
/// overlay, opaque black for a mask), so an uncovered corner reads correctly for
/// each category.
///
/// Exactness:
/// - an IDENTITY rotation returns the input unchanged;
/// - a pure QUARTER TURN is an integer pixel permutation performed by
///   `image::imageops::rotate90/180/270`, so it is bit-exact and `nearest` is
///   irrelevant;
/// - only a non-zero fine angle resamples, by inverse mapping every canvas pixel
///   centre back into the page ([`RotatedPage::unmap_point`]) and sampling
///   there. `nearest` picks the covering source pixel outright; otherwise the
///   four neighbours are interpolated on PREMULTIPLIED alpha and the result is
///   unpremultiplied, because bilinear interpolation of STRAIGHT alpha pulls the
///   (arbitrary) colour of fully transparent pixels into the visible edge and
///   produces a coloured halo.
///
/// `page_size` must be the raster's own size — the caller resizes a mismatched
/// source to it first.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when the raster does not have `page_size`, or when
/// the rotation request is outside its documented ranges (a corrupt journal).
fn rotate_page_raster(
    source: &image::RgbaImage,
    page_size: [u32; 2],
    rotation: plan::ComposeRotation,
    nearest: bool,
) -> Result<image::RgbaImage, PageOpError> {
    if source.width() != page_size[0] || source.height() != page_size[1] {
        return Err(PageOpError::InvalidOp(format!(
            "rotation recipe expects a {}x{} raster but got {}x{}",
            page_size[0],
            page_size[1],
            source.width(),
            source.height()
        )));
    }
    let page_rotation = PageRotation::new(rotation.quarter_turns, rotation.angle_deg)?;
    if page_rotation.is_identity() {
        return Ok(source.clone());
    }
    let rotated = RotatedPage::new(page_size, page_rotation)?;
    if !page_rotation.is_fine() {
        // Exact integer pixel permutation — no sampling, no rounding.
        return Ok(match page_rotation.quarter() {
            QuarterTurns::Zero => source.clone(),
            QuarterTurns::Cw90 => image::imageops::rotate90(source),
            QuarterTurns::Cw180 => image::imageops::rotate180(source),
            QuarterTurns::Cw270 => image::imageops::rotate270(source),
        });
    }
    let [canvas_w, canvas_h] = rotated.canvas_size();
    let mut out = image::RgbaImage::new(canvas_w, canvas_h);
    for y in 0..canvas_h {
        for x in 0..canvas_w {
            // Corner-based continuous coordinates: the centre of canvas pixel
            // (x, y) is at (x + 0.5, y + 0.5), and `unmap_point` answers in the
            // same convention on the page side.
            let (sx, sy) = rotated.unmap_point(f64::from(x) + 0.5, f64::from(y) + 0.5);
            let pixel = if nearest {
                sample_nearest(source, sx, sy)
            } else {
                sample_bilinear_premultiplied(source, sx, sy)
            };
            out.put_pixel(x, y, image::Rgba(pixel));
        }
    }
    Ok(out)
}

/// The source pixel covering the continuous page point `(x, y)`, or a fully
/// transparent pixel when that point lies outside the page.
///
/// Used for CATEGORICAL rasters (typing and detection masks): the loaders
/// threshold them, so any interpolation would turn a hard edge into a band of
/// half-masked pixels.
#[must_use]
fn sample_nearest(source: &image::RgbaImage, x: f64, y: f64) -> [u8; 4] {
    // A non-finite coordinate names no pixel at all. Saying so explicitly keeps
    // a degenerate geometry from landing on pixel (0, 0), which is a REAL pixel
    // and would silently smear the page's corner across the canvas.
    if !x.is_finite() || !y.is_finite() {
        return [0, 0, 0, 0];
    }
    // `floor` of the corner-based coordinate IS the covering pixel index.
    let Some(pixel) = pixel_at(source, floor_to_i64(x), floor_to_i64(y)) else {
        return [0, 0, 0, 0];
    };
    pixel
}

/// Bilinear sample of the continuous page point `(x, y)`, interpolated on
/// PREMULTIPLIED alpha and returned as straight (non-premultiplied) RGBA.
///
/// Straight-alpha interpolation would mix in the colour channels of fully
/// transparent pixels — which carry no meaningful colour — and fringe every
/// transparent edge. Samples outside the page contribute a premultiplied zero,
/// so the page's border fades into transparency instead of duplicating its edge
/// row.
#[must_use]
fn sample_bilinear_premultiplied(source: &image::RgbaImage, x: f64, y: f64) -> [u8; 4] {
    // See `sample_nearest`: a non-finite coordinate is outside the source, not
    // pixel (0, 0). Checked here so the arithmetic below only ever sees finite
    // values and `floor` cannot produce a NaN index.
    if !x.is_finite() || !y.is_finite() {
        return [0, 0, 0, 0];
    }
    // Pixel CENTRES sit at integer + 0.5, so shifting by half a pixel turns the
    // corner-based coordinate into an index-space one.
    let fx = x - 0.5;
    let fy = y - 0.5;
    let x0 = floor_to_i64(fx);
    let y0 = floor_to_i64(fy);
    // `x0` is the floor of `fx`, so both weights are in [0, 1].
    let tx = fx - fx.floor();
    let ty = fy - fy.floor();
    let mut acc = [0.0_f32; 4];
    for (dx, dy, weight) in [
        (0_i64, 0_i64, (1.0 - tx) * (1.0 - ty)),
        (1, 0, tx * (1.0 - ty)),
        (0, 1, (1.0 - tx) * ty),
        (1, 1, tx * ty),
    ] {
        if weight <= 0.0 {
            continue;
        }
        // Outside the page: a premultiplied zero, i.e. "nothing here".
        let Some(pixel) = pixel_at(source, x0 + dx, y0 + dy) else {
            continue;
        };
        // `weight` is a bilinear weight in [0, 1]; the conversion is exact.
        let weight = weight as f32;
        let alpha = f32::from(pixel[3]);
        let premul = alpha / 255.0;
        acc[0] += f32::from(pixel[0]) * premul * weight;
        acc[1] += f32::from(pixel[1]) * premul * weight;
        acc[2] += f32::from(pixel[2]) * premul * weight;
        acc[3] += alpha * weight;
    }
    if acc[3] <= 0.0 {
        return [0, 0, 0, 0];
    }
    // Unpremultiply: the stored channels are `colour * alpha / 255`.
    let inv = 255.0 / acc[3];
    [
        round_to_u8(acc[0] * inv),
        round_to_u8(acc[1] * inv),
        round_to_u8(acc[2] * inv),
        round_to_u8(acc[3]),
    ]
}

/// The RGBA pixel at signed index `(x, y)`, or `None` when it lies outside the
/// image. Bounds are checked with `try_from`, so no negative index can wrap.
#[must_use]
fn pixel_at(source: &image::RgbaImage, x: i64, y: i64) -> Option<[u8; 4]> {
    let (Ok(ux), Ok(uy)) = (u32::try_from(x), u32::try_from(y)) else {
        return None;
    };
    if ux >= source.width() || uy >= source.height() {
        return None;
    }
    Some(source.get_pixel(ux, uy).0)
}

/// Floors an `f64` to `i64`, saturating far outside any real image so the caller
/// simply reads it as "outside the source".
///
/// PRECONDITION: `value` is finite — both callers reject a non-finite coordinate
/// before any arithmetic, because "outside the source" is the honest answer for
/// one and pixel 0 is not. A non-finite value that reaches here anyway saturates
/// to `i64::MIN`, which `pixel_at` also reads as outside.
///
/// The `as` conversion is reached only for a value already proven finite and
/// inside `i64::MIN ..= i64::MAX`, where it is exact for an integral input.
#[must_use]
fn floor_to_i64(value: f64) -> i64 {
    if !value.is_finite() {
        return i64::MIN;
    }
    let floored = value.floor();
    if floored <= -9.0e18 {
        return i64::MIN;
    }
    if floored >= 9.0e18 {
        return i64::MAX;
    }
    floored as i64
}

/// Rounds an accumulated channel value to `u8`, clamping at both ends.
///
/// The `as` conversion is reached only for a value already proven to be an
/// integral `f32` inside `0 ..= 255`, where it is exact.
#[must_use]
fn round_to_u8(value: f32) -> u8 {
    let rounded = value.round();
    if !rounded.is_finite() || rounded <= 0.0 {
        return 0;
    }
    if rounded >= 255.0 {
        return 255;
    }
    rounded as u8
}

/// Writes an RGBA image as a straight (non-premultiplied) PNG with the
/// project's fast service settings (`project.rs::write_png_fast`).
fn write_rgba_png(path: &Path, image: &image::RgbaImage) -> Result<(), PageOpError> {
    let file = fs::File::create(path)
        .map_err(|err| io_ctx(&err, format!("create {}", path.display())))?;
    let mut writer = BufWriter::new(file);
    let encoder =
        PngEncoder::new_with_quality(&mut writer, CompressionType::Fast, FilterType::NoFilter);
    image::ImageEncoder::write_image(
        encoder,
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|err| PageOpError::Image(format!("encode {}: {err}", path.display())))?;
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

    /// Whether the fixture's page-sized rasters are opaque marker bytes (cheap,
    /// and exactly what the rename-only operations need to track identity) or
    /// genuinely encoded PNGs (required by anything that DECODES them, i.e. a
    /// stitch).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FixturePixels {
        Fake,
        Real,
    }

    /// Page sizes of the decodable fixture. All different, so a remap that
    /// confuses two pages' coordinate spaces produces a wrong number instead of
    /// an accidental identity.
    const REAL_PAGE_SIZES: [[u32; 2]; 4] = [[8, 6], [4, 10], [6, 6], [5, 5]];

    /// Distinct opaque colour per page, so a composed canvas says which page
    /// painted which pixel.
    fn page_color(page_idx: usize) -> [u8; 4] {
        let base = u8::try_from(page_idx).expect("small page index");
        [10 + base * 10, 20, 30, 255]
    }

    fn write_png(path: &Path, size: [u32; 2], rgba: [u8; 4]) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        image::RgbaImage::from_pixel(size[0], size[1], image::Rgba(rgba))
            .save(path)
            .expect("write fixture png");
    }

    fn read_png(path: &Path) -> image::RgbaImage {
        image::open(path)
            .unwrap_or_else(|err| panic!("decode {}: {err}", path.display()))
            .to_rgba8()
    }

    fn project_paths(title: &Path, chapter: &str) -> ProjectPaths {
        let project_dir = title.join(chapter);
        let unsaved_dir = title.join(format!("{chapter}_unsaved"));
        ProjectPaths {
            project_dir: project_dir.clone(),
            title_dir: title.to_path_buf(),
            notes_file: title.join("notes.txt"),
            char_favorites_file: title.join(crate::config::CHAR_FAVORITES_FILE),
            color_presets_file: title.join(crate::config::COLOR_PRESETS_FILE),
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
        build_fixture_with(FixturePixels::Fake)
    }

    /// The same chapter with real PNGs wherever a stitch decodes pixels.
    fn build_decodable_fixture() -> Fixture {
        build_fixture_with(FixturePixels::Real)
    }

    fn build_fixture_with(pixels: FixturePixels) -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let title = tmp.path().join("title");
        let paths = project_paths(&title, CHAPTER);
        let real = pixels == FixturePixels::Real;

        for (i, size) in REAL_PAGE_SIZES.iter().enumerate() {
            let path = paths.src_dir.join(format!("{i:03}.png"));
            if real {
                write_png(&path, *size, page_color(i));
            } else {
                write(&path, format!("SRC-PAGE-{i}").as_bytes());
            }
        }
        if real {
            // Clean overlays are page-sized; each carries its own red channel.
            write_png(&paths.clean_layers_dir.join("000.png"), REAL_PAGE_SIZES[0], [100, 0, 0, 255]);
            write_png(&paths.clean_layers_dir.join("002.png"), REAL_PAGE_SIZES[2], [102, 0, 0, 255]);
            write_png(
                &paths.unsaved_clean_layers_dir.join("001.png"),
                REAL_PAGE_SIZES[1],
                [201, 0, 0, 255],
            );
        } else {
            write(&paths.clean_layers_dir.join("000.png"), b"CLEAN-0");
            write(&paths.clean_layers_dir.join("002.png"), b"CLEAN-2");
            write(&paths.unsaved_clean_layers_dir.join("001.png"), b"UNSAVED-CLEAN-1");
        }

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
        if real {
            // Typing masks are page-sized, fully "masked" (white) here.
            write_png(
                &paths.text_images_dir.join("mask_page_1.png"),
                REAL_PAGE_SIZES[1],
                [255, 255, 255, 255],
            );
        } else {
            write(&paths.text_images_dir.join("mask_page_1.png"), b"TMASK-1");
        }
        write_json(
            &paths.text_images_dir.join("text_info.json"),
            &json!([
                {"img_idx": 1, "file": "ov1.png", "img_u": 0.5, "img_v": 0.5},
                {"img_idx": 3, "file": "ov3.png", "img_u": 0.4, "img_v": 0.6}
            ]),
        );
        if real {
            write_png(
                &paths.unsaved_text_images_dir.join("mask_page_2.png"),
                REAL_PAGE_SIZES[2],
                [255, 255, 255, 255],
            );
        } else {
            write(&paths.unsaved_text_images_dir.join("mask_page_2.png"), b"UTMASK-2");
        }

        // The detector's document must describe the page it belongs to: a
        // stitch refuses to merge one that does not.
        let detection_size = if real {
            REAL_PAGE_SIZES[1]
        } else {
            [100, 200]
        };
        write_json(
            &paths.text_detection_dir.join("00001_blocks.json"),
            &json!({
                "page_idx": 1,
                "source_size": detection_size,
                "mask_size": detection_size,
                "blocks": [{"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 4.0}],
                "mask_file": "00001_mask.png"
            }),
        );
        if real {
            write_png(
                &paths.text_detection_dir.join("00001_mask.png"),
                REAL_PAGE_SIZES[1],
                [200, 200, 200, 255],
            );
        } else {
            write(&paths.text_detection_dir.join("00001_mask.png"), b"DMASK-1");
        }

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
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
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
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
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
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
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
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
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
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
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
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
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
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
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

    // -----------------------------------------------------------------------
    // Stitch: pages 1 (4x10) and 2 (6x6) side by side on a 10x10 canvas.
    // Page 1 lands at (0, 0), page 2 at (4, 0); the bottom-right corner stays
    // uncovered, so the background colour must show there.
    // -----------------------------------------------------------------------

    const STITCH_BACKGROUND: [u8; 4] = [7, 8, 9, 255];

    fn stitch_op() -> PageOpKind {
        PageOpKind::Stitch {
            placements: vec![
                crate::page_ops::StitchPlacement {
                    page_idx: 1,
                    crop: [0, 0, 4, 10],
                    scale: 1.0,
                    dx: 0,
                    dy: 0,
                },
                crate::page_ops::StitchPlacement {
                    page_idx: 2,
                    crop: [0, 0, 6, 6],
                    scale: 1.0,
                    dx: 4,
                    dy: 0,
                },
            ],
            width: 10,
            height: 10,
            background: STITCH_BACKGROUND,
        }
    }

    /// Compares a stored coordinate with a tolerance: the remap runs in f64 and
    /// the exact bit pattern of a normalized value is not part of the contract.
    fn approx(value: &Value, expected: f64) {
        let actual = value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"));
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    /// Full expected state after `stitch_op()` on the decodable fixture; shared
    /// by the direct-execute and the roll-forward tests.
    fn assert_stitched_layout(fx: &Fixture) {
        // The merged page: page 1 on the left, page 2 top-right, background in
        // the uncovered corner.
        let merged = read_png(&fx.paths.src_dir.join("001.png"));
        assert_eq!(merged.dimensions(), (10, 10));
        assert_eq!(merged.get_pixel(0, 0), &image::Rgba(page_color(1)));
        assert_eq!(merged.get_pixel(3, 9), &image::Rgba(page_color(1)));
        assert_eq!(merged.get_pixel(4, 0), &image::Rgba(page_color(2)));
        assert_eq!(merged.get_pixel(9, 5), &image::Rgba(page_color(2)));
        assert_eq!(merged.get_pixel(9, 9), &image::Rgba(STITCH_BACKGROUND));
        // The other pages kept their pixels and compacted around it.
        assert_eq!(
            read_png(&fx.paths.src_dir.join("000.png")).get_pixel(0, 0),
            &image::Rgba(page_color(0))
        );
        assert_eq!(
            read_png(&fx.paths.src_dir.join("002.png")).get_pixel(0, 0),
            &image::Rgba(page_color(3))
        );
        assert!(!fx.paths.src_dir.join("003.png").exists());

        // Clean overlays: page 2's overlay lands at its placement, the rest of
        // the merged overlay stays transparent so the page shows through.
        let clean = read_png(&fx.paths.clean_layers_dir.join("001.png"));
        assert_eq!(clean.dimensions(), (10, 10));
        assert_eq!(clean.get_pixel(4, 0), &image::Rgba([102, 0, 0, 255]));
        assert_eq!(clean.get_pixel(0, 0), &image::Rgba([0, 0, 0, 0]));
        // Page 0's overlay is untouched.
        assert_eq!(
            read_png(&fx.paths.clean_layers_dir.join("000.png")).dimensions(),
            (8, 6)
        );
        let unsaved_clean = read_png(&fx.paths.unsaved_clean_layers_dir.join("001.png"));
        assert_eq!(unsaved_clean.get_pixel(0, 0), &image::Rgba([201, 0, 0, 255]));
        assert_eq!(unsaved_clean.get_pixel(9, 9), &image::Rgba([0, 0, 0, 0]));

        // Typing masks compose over black ("not masked").
        let mask = read_png(&fx.paths.text_images_dir.join("mask_page_1.png"));
        assert_eq!(mask.dimensions(), (10, 10));
        assert_eq!(mask.get_pixel(0, 0), &image::Rgba([255, 255, 255, 255]));
        assert_eq!(mask.get_pixel(9, 9), &image::Rgba([0, 0, 0, 255]));
        let unsaved_mask =
            read_png(&fx.paths.unsaved_text_images_dir.join("mask_page_1.png"));
        assert_eq!(unsaved_mask.get_pixel(4, 0), &image::Rgba([255, 255, 255, 255]));
        assert_eq!(unsaved_mask.get_pixel(0, 0), &image::Rgba([0, 0, 0, 255]));

        // Layer PNGs of the merged page carry the primary's prefix.
        assert_eq!(
            fs::read(fx.paths.layers_dir.join("ps_p0001_u2_text.png")).expect("layer png"),
            b"L-TEXT-2"
        );
        assert!(fx.paths.layers_dir.join("ps_p0000_u1.png").exists());

        // Manifests: page 2's entry became the merged entry at index 1.
        let manifest = read_json(&fx.paths.layers_dir.join("layers.json"));
        let pages = manifest["pages"].as_array().expect("pages");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0]["img_idx"], json!(0));
        assert_eq!(pages[1]["img_idx"], json!(1));
        assert_eq!(
            pages[1]["tree"][0]["rendered_file"],
            json!("ps_p0001_u2_text.png")
        );
        let unsaved_manifest = read_json(&fx.paths.unsaved_layers_dir.join("layers.json"));
        assert_eq!(unsaved_manifest["pages"][0]["img_idx"], json!(1));

        // Bubbles: page-1 anchors renormalize onto the wider canvas, the crop
        // rect follows the CROPPED page (1), and page 0 is untouched.
        let bubbles = read_json(&fx.paths.bubbles_file);
        let bubbles = bubbles.as_array().expect("bubbles");
        assert_eq!(bubbles.len(), 3);
        assert_eq!(bubbles[0]["img_idx"], json!(0));
        approx(&bubbles[0]["img_u"], 0.5);
        assert_eq!(bubbles[1]["img_idx"], json!(1));
        approx(&bubbles[1]["img_u"], 0.2);
        approx(&bubbles[1]["img_v"], 0.5);
        assert_eq!(bubbles[2]["img_idx"], json!(2));
        assert_eq!(bubbles[2]["crop_page_idx"], json!(1));
        let crop = bubbles[2]["crop_rect"].as_array().expect("crop rect");
        approx(&crop[0], 0.04);
        approx(&crop[1], 0.1);
        approx(&crop[2], 0.36);
        approx(&crop[3], 0.9);
        let unsaved_bubbles = read_json(&fx.paths.unsaved_bubbles_file);
        assert_eq!(unsaved_bubbles[0]["img_idx"], json!(1));
        approx(&unsaved_bubbles[0]["img_u"], 0.52);
        approx(&unsaved_bubbles[0]["img_v"], 0.18);

        // Legacy typing metadata follows the same rules.
        let text_info = read_json(&fx.paths.text_images_dir.join("text_info.json"));
        let entries = text_info.as_array().expect("entries");
        assert_eq!(entries[0]["img_idx"], json!(1));
        approx(&entries[0]["img_u"], 0.2);
        assert_eq!(entries[0]["file"], json!("ov1.png"));
        assert_eq!(entries[1]["img_idx"], json!(2));
        approx(&entries[1]["img_u"], 0.4);

        // Detection merged onto the canvas, mask composed.
        let blocks = read_json(&fx.paths.text_detection_dir.join("00001_blocks.json"));
        assert_eq!(blocks["page_idx"], json!(1));
        assert_eq!(blocks["source_size"], json!([10, 10]));
        assert_eq!(blocks["mask_size"], json!([10, 10]));
        assert_eq!(blocks["mask_file"], json!("00001_mask.png"));
        approx(&blocks["blocks"][0]["x1"], 1.0);
        approx(&blocks["blocks"][0]["y2"], 4.0);
        assert_eq!(
            read_png(&fx.paths.text_detection_dir.join("00001_mask.png")).dimensions(),
            (10, 10)
        );

        // Both source pages are recoverable from the trash.
        let trash_base = fx.paths.project_dir.join(super::super::plan::TRASH_DIR_NAME);
        let ids: Vec<_> = fs::read_dir(&trash_base)
            .expect("trash exists")
            .flatten()
            .collect();
        assert_eq!(ids.len(), 1, "one transaction trash folder");
        let trash = ids[0].path();
        assert!(trash.join("ch1/src/001.png").exists());
        assert!(trash.join("ch1/src/002.png").exists());
        assert!(trash.join("ch1/clean_layers/002.png").exists());
        assert!(trash.join("ch1_unsaved/clean_layers/001.png").exists());
        // Nothing was archived-and-dropped: a stitch never deletes JSON entries.
        assert!(!trash.join("ch1/deleted_bubbles.json").exists());

        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn stitch_merges_two_pages_into_one_in_both_trees() {
        let fx = build_decodable_fixture();
        let outcome =
            super::execute(&fx.paths, &fx.pages, &stitch_op()).expect("stitch executes");
        assert_eq!(outcome.old_to_new, vec![Some(0), Some(1), Some(1), Some(2)]);
        assert_eq!(outcome.new_page_count, 3);
        assert_stitched_layout(&fx);
    }

    #[test]
    fn scan_detects_a_non_empty_alt_vers_directory() {
        let fx = build_decodable_fixture();
        let op = stitch_op();
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
        assert!(!snapshot.has_alt_vers);
        assert_eq!(snapshot.page_sizes, REAL_PAGE_SIZES.to_vec());
        // Alternate versions live one level deeper, in per-version folders.
        write(&fx.paths.alt_vers_dir.join("v1").join("a.png"), b"ALT");
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
        assert!(snapshot.has_alt_vers);
        // A non-geometric operation still pays nothing for page sizes.
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &PageOpKind::Move { from: 0, to: 1 })
            .expect("scan");
        assert!(snapshot.page_sizes.is_empty());
    }

    #[test]
    fn stitch_trashes_detection_it_cannot_remap() {
        let fx = build_decodable_fixture();
        // A mask smaller than its page (the detector's downscaled form) cannot
        // be remapped: the whole group degrades to the trash instead.
        write_json(
            &fx.paths.text_detection_dir.join("00001_blocks.json"),
            &json!({
                "page_idx": 1,
                "source_size": [4, 10],
                "mask_size": [2, 5],
                "blocks": [{"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 4.0}],
                "mask_file": "00001_mask.png"
            }),
        );
        write_png(
            &fx.paths.text_detection_dir.join("00001_mask.png"),
            [2, 5],
            [200, 200, 200, 255],
        );
        super::execute(&fx.paths, &fx.pages, &stitch_op()).expect("stitch executes");
        assert!(!fx.paths.text_detection_dir.join("00001_blocks.json").exists());
        assert!(!fx.paths.text_detection_dir.join("00001_mask.png").exists());
        let trash_base = fx.paths.project_dir.join(super::super::plan::TRASH_DIR_NAME);
        let trash = fs::read_dir(&trash_base)
            .expect("trash exists")
            .flatten()
            .next()
            .expect("one trash folder")
            .path();
        assert!(trash.join("ch1/text_detection/00001_blocks.json").exists());
        assert!(trash.join("ch1/text_detection/00001_mask.png").exists());
        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn stitch_crash_after_phase_a_rolls_back_to_original_state() {
        let fx = build_decodable_fixture();
        let before = walk(&fx.title);
        let op = stitch_op();

        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 5150).expect("plan");
        let journal_paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&journal_paths, &plan, JournalPhase::A, &op).expect("journal");
        run_phase_a(&fx.title, &plan).expect("phase A");
        assert_ne!(before, walk(&fx.title), "phase A must change the tree");

        super::recover(&fx.paths.project_dir).expect("rollback");
        assert_eq!(before, walk(&fx.title), "rollback restores the exact state");
        assert!(!journal_paths.a.exists());
    }

    #[test]
    fn stitch_crash_mid_phase_b_rolls_forward_to_final_state() {
        let fx = build_decodable_fixture();
        let op = stitch_op();
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 5151).expect("plan");
        let journal_paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&journal_paths, &plan, JournalPhase::A, &op).expect("journal a");
        run_phase_a(&fx.title, &plan).expect("phase A");
        write_journal(&journal_paths, &plan, JournalPhase::B, &op).expect("journal b");

        // Only part of phase B ran before the "crash": half the final renames,
        // no created page committed and no JSON written.
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
        assert!(!journal_paths.b.exists());
        assert_stitched_layout(&fx);
    }

    #[test]
    fn stitch_recovery_never_recomposes_a_lost_staged_page() {
        let fx = build_decodable_fixture();
        let op = stitch_op();
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 5152).expect("plan");
        let journal_paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&journal_paths, &plan, JournalPhase::A, &op).expect("journal a");
        run_phase_a(&fx.title, &plan).expect("phase A");
        write_journal(&journal_paths, &plan, JournalPhase::B, &op).expect("journal b");
        // The composed page is gone and its sources have already been renamed
        // away: re-running the composition would read half-moved inputs.
        fs::remove_file(fx.title.join(&plan.creates[0].temp)).expect("lose staged page");

        let err = super::recover(&fx.paths.project_dir).expect_err("must not recompose");
        assert!(matches!(err, PageOpError::Journal(_)), "got: {err}");
        assert!(journal_paths.b.exists(), "journal remains for inspection");
    }

    // -----------------------------------------------------------------------
    // Split: page 1 (4x10) cut horizontally at y = 4 into a 4x4 top part
    // (keeping index 1) and a 4x6 bottom part (index 2). Pages 2 and 3 shift
    // up by one.
    // -----------------------------------------------------------------------

    const SPLIT_TOP_COLOR: [u8; 4] = [200, 10, 10, 255];
    const SPLIT_BOTTOM_COLOR: [u8; 4] = [10, 10, 200, 255];

    fn split_op() -> PageOpKind {
        PageOpKind::Split {
            page_idx: 1,
            axis: crate::page_ops::SplitAxis::Horizontal,
            cuts: vec![4],
            order: vec![0, 1],
            deleted: vec![false, false],
        }
    }

    /// Paints page 1 in two horizontal bands (so the cut is observable in the
    /// pixels) and gives it a real layer stack in the unsaved tree, one layer
    /// per side of the cut, so the layer PNGs fan out onto two prefixes.
    fn prepare_split_fixture(fx: &Fixture) {
        let [width, height] = REAL_PAGE_SIZES[1];
        let mut page = image::RgbaImage::new(width, height);
        for (_, y, pixel) in page.enumerate_pixels_mut() {
            *pixel = image::Rgba(if y < 4 {
                SPLIT_TOP_COLOR
            } else {
                SPLIT_BOTTOM_COLOR
            });
        }
        page.save(fx.paths.src_dir.join("001.png"))
            .expect("write banded split page");

        write(&fx.paths.unsaved_layers_dir.join("ps_p0001_top.png"), b"UL-TOP");
        write(&fx.paths.unsaved_layers_dir.join("ps_p0001_bot.png"), b"UL-BOT");
        write_json(
            &fx.paths.unsaved_layers_dir.join("layers.json"),
            &json!({
                "schema_version": 4,
                "pages": [{
                    "img_idx": 1,
                    "groups": [{"uid": "g1", "name": "G", "visible": true, "opacity": 1.0}],
                    "tree": [
                        {"uid": "top", "name": "T", "kind": "raster", "z": 0,
                         "visible": true, "opacity": 1.0, "group_uid": "g1",
                         "base_file": "ps_p0001_top.png", "image_size": [2, 2],
                         "transform": {"cx": 2.0, "cy": 1.0, "rotation": 0.0, "scale": 1.0}},
                        {"uid": "bot", "name": "B", "kind": "raster", "z": 1,
                         "visible": true, "opacity": 1.0, "group_uid": "g1",
                         "base_file": "ps_p0001_bot.png", "image_size": [2, 2],
                         "transform": {"cx": 2.0, "cy": 8.0, "rotation": 0.0, "scale": 1.0}}
                    ]
                }]
            }),
        );
    }

    /// Full expected state after `split_op()` on the prepared fixture; shared
    /// by the direct-execute and the roll-forward tests.
    fn assert_split_layout(fx: &Fixture) {
        // The two parts carry exactly their band of the source pixels.
        let top = read_png(&fx.paths.src_dir.join("001.png"));
        assert_eq!(top.dimensions(), (4, 4));
        assert_eq!(top.get_pixel(0, 0), &image::Rgba(SPLIT_TOP_COLOR));
        assert_eq!(top.get_pixel(3, 3), &image::Rgba(SPLIT_TOP_COLOR));
        let bottom = read_png(&fx.paths.src_dir.join("002.png"));
        assert_eq!(bottom.dimensions(), (4, 6));
        assert_eq!(bottom.get_pixel(0, 0), &image::Rgba(SPLIT_BOTTOM_COLOR));
        assert_eq!(bottom.get_pixel(3, 5), &image::Rgba(SPLIT_BOTTOM_COLOR));
        // The other pages kept their pixels and shifted up by one.
        assert_eq!(
            read_png(&fx.paths.src_dir.join("000.png")).get_pixel(0, 0),
            &image::Rgba(page_color(0))
        );
        assert_eq!(
            read_png(&fx.paths.src_dir.join("003.png")).get_pixel(0, 0),
            &image::Rgba(page_color(2))
        );
        assert_eq!(
            read_png(&fx.paths.src_dir.join("004.png")).get_pixel(0, 0),
            &image::Rgba(page_color(3))
        );

        // Page-sized rasters are cut the same way, in both trees.
        let unsaved_top = read_png(&fx.paths.unsaved_clean_layers_dir.join("001.png"));
        assert_eq!(unsaved_top.dimensions(), (4, 4));
        assert_eq!(unsaved_top.get_pixel(0, 0), &image::Rgba([201, 0, 0, 255]));
        assert_eq!(
            read_png(&fx.paths.unsaved_clean_layers_dir.join("002.png")).dimensions(),
            (4, 6)
        );
        assert_eq!(
            read_png(&fx.paths.text_images_dir.join("mask_page_1.png")).dimensions(),
            (4, 4)
        );
        assert_eq!(
            read_png(&fx.paths.text_images_dir.join("mask_page_2.png")).dimensions(),
            (4, 6)
        );
        // A mask of an untouched page follows the ordinary index shift.
        assert!(
            fx.paths
                .unsaved_text_images_dir
                .join("mask_page_3.png")
                .exists()
        );

        // Layer PNGs of ONE page landed on TWO prefixes.
        assert_eq!(
            fs::read(fx.paths.unsaved_layers_dir.join("ps_p0001_top.png")).expect("top layer"),
            b"UL-TOP"
        );
        assert_eq!(
            fs::read(fx.paths.unsaved_layers_dir.join("ps_p0002_bot.png")).expect("bot layer"),
            b"UL-BOT"
        );
        // An orphan PNG no record claims follows the part keeping the index.
        assert!(fx.paths.unsaved_layers_dir.join("ps_p0001_uu.png").exists());
        // The committed tree's page-2 layer followed the index shift.
        assert!(fx.paths.layers_dir.join("ps_p0003_u2_text.png").exists());

        // The unsaved manifest page became TWO entries, one per part.
        let manifest = read_json(&fx.paths.unsaved_layers_dir.join("layers.json"));
        let pages = manifest["pages"].as_array().expect("pages");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0]["img_idx"], json!(1));
        assert_eq!(pages[0]["tree"][0]["uid"], json!("top"));
        assert_eq!(pages[0]["tree"][0]["transform"]["cy"], json!(1.0));
        assert_eq!(pages[0]["tree"][0]["z"], json!(0));
        assert_eq!(pages[1]["img_idx"], json!(2));
        assert_eq!(pages[1]["tree"][0]["uid"], json!("bot"));
        // Page px 8 of the source is px 4 of the bottom part, and the band
        // axis is re-ranked densely inside each part.
        assert_eq!(pages[1]["tree"][0]["transform"]["cy"], json!(4.0));
        assert_eq!(pages[1]["tree"][0]["z"], json!(0));
        assert_eq!(pages[1]["tree"][0]["base_file"], json!("ps_p0002_bot.png"));
        // The shared PS group is duplicated into both parts.
        assert_eq!(pages[0]["groups"][0]["uid"], json!("g1"));
        assert_eq!(pages[1]["groups"][0]["uid"], json!("g1"));

        // Bubbles: the page-1 bubble follows its anchor into the bottom part,
        // and the page-crop bubble's crop is routed by area and clamped.
        let bubbles = read_json(&fx.paths.bubbles_file);
        let bubbles = bubbles.as_array().expect("bubbles");
        assert_eq!(bubbles.len(), 3);
        assert_eq!(bubbles[0]["img_idx"], json!(0));
        assert_eq!(bubbles[1]["img_idx"], json!(2));
        approx(&bubbles[1]["img_u"], 0.5);
        approx(&bubbles[1]["img_v"], 1.0 / 6.0);
        assert_eq!(bubbles[2]["img_idx"], json!(4));
        assert_eq!(bubbles[2]["crop_page_idx"], json!(2));
        let crop = bubbles[2]["crop_rect"].as_array().expect("crop rect");
        approx(&crop[0], 0.1);
        approx(&crop[1], 0.0);
        approx(&crop[2], 0.9);
        approx(&crop[3], 5.0 / 6.0);
        assert_eq!(read_json(&fx.paths.unsaved_bubbles_file)[0]["img_idx"], json!(3));

        // Legacy typing metadata follows the same routing.
        let text_info = read_json(&fx.paths.text_images_dir.join("text_info.json"));
        let entries = text_info.as_array().expect("entries");
        assert_eq!(entries[0]["img_idx"], json!(2));
        approx(&entries[0]["img_v"], 1.0 / 6.0);
        assert_eq!(entries[0]["file"], json!("ov1.png"));
        assert_eq!(entries[1]["img_idx"], json!(4));

        // Detection: one document and one mask per part, blocks routed by area.
        let first = read_json(&fx.paths.text_detection_dir.join("00001_blocks.json"));
        assert_eq!(first["page_idx"], json!(1));
        assert_eq!(first["source_size"], json!([4, 4]));
        assert_eq!(first["mask_size"], json!([4, 4]));
        assert_eq!(first["mask_file"], json!("00001_mask.png"));
        assert_eq!(first["blocks"].as_array().expect("blocks").len(), 1);
        approx(&first["blocks"][0]["y2"], 4.0);
        let second = read_json(&fx.paths.text_detection_dir.join("00002_blocks.json"));
        assert_eq!(second["source_size"], json!([4, 6]));
        assert!(second["blocks"].as_array().expect("blocks").is_empty());
        assert_eq!(
            read_png(&fx.paths.text_detection_dir.join("00001_mask.png")).dimensions(),
            (4, 4)
        );
        assert_eq!(
            read_png(&fx.paths.text_detection_dir.join("00002_mask.png")).dimensions(),
            (4, 6)
        );

        // The source page's own files are recoverable from the trash.
        let trash_base = fx.paths.project_dir.join(super::super::plan::TRASH_DIR_NAME);
        let ids: Vec<_> = fs::read_dir(&trash_base)
            .expect("trash exists")
            .flatten()
            .collect();
        assert_eq!(ids.len(), 1, "one transaction trash folder");
        let trash = ids[0].path();
        assert!(trash.join("ch1/src/001.png").exists());
        assert!(trash.join("ch1_unsaved/clean_layers/001.png").exists());
        assert!(trash.join("ch1/text_images/mask_page_1.png").exists());
        // A split never deletes a JSON entry.
        assert!(!trash.join("ch1/deleted_bubbles.json").exists());

        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn split_cuts_one_page_into_two_in_both_trees() {
        let fx = build_decodable_fixture();
        prepare_split_fixture(&fx);
        let outcome =
            super::execute(&fx.paths, &fx.pages, &split_op()).expect("split executes");
        assert_eq!(outcome.old_to_new, vec![Some(0), Some(1), Some(3), Some(4)]);
        assert_eq!(outcome.new_page_count, 5);
        assert_split_layout(&fx);
    }

    #[test]
    fn split_crash_after_phase_a_rolls_back_to_original_state() {
        let fx = build_decodable_fixture();
        prepare_split_fixture(&fx);
        let before = walk(&fx.title);
        let op = split_op();

        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 6150).expect("plan");
        let journal_paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&journal_paths, &plan, JournalPhase::A, &op).expect("journal");
        run_phase_a(&fx.title, &plan).expect("phase A");
        assert_ne!(before, walk(&fx.title), "phase A must change the tree");

        super::recover(&fx.paths.project_dir).expect("rollback");
        assert_eq!(before, walk(&fx.title), "rollback restores the exact state");
        assert!(!journal_paths.a.exists());
    }

    #[test]
    fn split_crash_mid_phase_b_rolls_forward_to_final_state() {
        let fx = build_decodable_fixture();
        prepare_split_fixture(&fx);
        let op = split_op();
        let snapshot = scan_chapter(&fx.paths, &fx.pages, &op).expect("scan");
        let plan = plan::build_plan(&snapshot, &op, 6151).expect("plan");
        let journal_paths = JournalPaths::new(&fx.paths.project_dir);
        write_journal(&journal_paths, &plan, JournalPhase::A, &op).expect("journal a");
        run_phase_a(&fx.title, &plan).expect("phase A");
        write_journal(&journal_paths, &plan, JournalPhase::B, &op).expect("journal b");

        // Only part of phase B ran before the "crash": half the final renames,
        // no created part committed and no JSON written.
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
        assert!(!journal_paths.b.exists());
        assert_split_layout(&fx);
    }

    // -----------------------------------------------------------------------
    // Crop: page 1 (4x10) rotated and cropped in place. The page count never
    // changes, so every other page's files must stay exactly where they are.
    // -----------------------------------------------------------------------

    /// A per-pixel unique opaque colour, so a rotation can be verified pixel by
    /// pixel instead of by a single corner.
    fn crop_pixel(x: u32, y: u32) -> [u8; 4] {
        let r = u8::try_from(10 + x * 50).expect("fits u8");
        let g = u8::try_from(10 + y * 20).expect("fits u8");
        [r, g, 200, 255]
    }

    /// Paints page 1 with [`crop_pixel`] so that every source pixel is
    /// distinguishable, leaving the rest of the decodable fixture untouched.
    fn prepare_crop_fixture(fx: &Fixture) {
        let [width, height] = REAL_PAGE_SIZES[1];
        let mut page = image::RgbaImage::new(width, height);
        for (x, y, pixel) in page.enumerate_pixels_mut() {
            *pixel = image::Rgba(crop_pixel(x, y));
        }
        page.save(fx.paths.src_dir.join("001.png"))
            .expect("write per-pixel crop page");
    }

    fn crop_op(quarter_turns: u8, angle_deg: f64, rect: [u32; 4]) -> PageOpKind {
        PageOpKind::Crop {
            page_idx: 1,
            quarter_turns,
            angle_deg,
            rect,
        }
    }

    /// Source pixel a quarter turn puts at destination `(dx, dy)`, for a
    /// `width` x `height` source. Mirrors `image::imageops::rotate90/180/270`,
    /// which is what the executor uses for the exact path.
    fn unturned(quarter_turns: u8, width: u32, height: u32, dx: u32, dy: u32) -> (u32, u32) {
        match quarter_turns {
            0 => (dx, dy),
            1 => (dy, height - 1 - dx),
            2 => (width - 1 - dx, height - 1 - dy),
            3 => (width - 1 - dy, dx),
            other => panic!("quarter turns {other} is outside 0..=3"),
        }
    }

    #[test]
    fn crop_quarter_turns_are_bit_exact_pixel_permutations() {
        let [width, height] = REAL_PAGE_SIZES[1];
        for quarter_turns in 0..=3u8 {
            let fx = build_decodable_fixture();
            prepare_crop_fixture(&fx);
            let canvas = if quarter_turns % 2 == 0 {
                [width, height]
            } else {
                [height, width]
            };
            let outcome = super::execute(
                &fx.paths,
                &fx.pages,
                &crop_op(quarter_turns, 0.0, [0, 0, canvas[0], canvas[1]]),
            )
            .expect("crop executes");
            // The page count and every index are unchanged.
            assert_eq!(
                outcome.old_to_new,
                vec![Some(0), Some(1), Some(2), Some(3)]
            );
            assert_eq!(outcome.new_page_count, 4);

            let page = read_png(&fx.paths.src_dir.join("001.png"));
            assert_eq!(page.dimensions(), (canvas[0], canvas[1]));
            for dy in 0..canvas[1] {
                for dx in 0..canvas[0] {
                    let (sx, sy) = unturned(quarter_turns, width, height, dx, dy);
                    assert_eq!(
                        page.get_pixel(dx, dy),
                        &image::Rgba(crop_pixel(sx, sy)),
                        "turns {quarter_turns}: dst ({dx}, {dy}) must be src ({sx}, {sy})"
                    );
                }
            }
            // The untouched pages kept their exact pixels.
            assert_eq!(
                read_png(&fx.paths.src_dir.join("000.png")).get_pixel(0, 0),
                &image::Rgba(page_color(0))
            );
            assert_eq!(
                read_png(&fx.paths.src_dir.join("003.png")).get_pixel(0, 0),
                &image::Rgba(page_color(3))
            );
            assert_no_transaction_residue(&fx.title);
        }
    }

    #[test]
    fn crop_at_zero_angle_copies_a_sub_rectangle_bit_exactly() {
        let fx = build_decodable_fixture();
        prepare_crop_fixture(&fx);
        let [width, height] = REAL_PAGE_SIZES[1];
        // 90 CW gives a 10x4 canvas; keep a 5x2 window inside it.
        super::execute(&fx.paths, &fx.pages, &crop_op(1, 0.0, [2, 1, 5, 2]))
            .expect("crop executes");
        let page = read_png(&fx.paths.src_dir.join("001.png"));
        assert_eq!(page.dimensions(), (5, 2));
        for dy in 0..2u32 {
            for dx in 0..5u32 {
                let (sx, sy) = unturned(1, width, height, dx + 2, dy + 1);
                assert_eq!(
                    page.get_pixel(dx, dy),
                    &image::Rgba(crop_pixel(sx, sy)),
                    "dst ({dx}, {dy}) must be src ({sx}, {sy}) — no resampling at angle 0"
                );
            }
        }
    }

    #[test]
    fn crop_full_layout_rotates_every_artifact_of_the_page() {
        let fx = build_decodable_fixture();
        prepare_crop_fixture(&fx);
        let [width, height] = REAL_PAGE_SIZES[1];
        // A full 90 CW turn of page 1: a 10x4 page.
        super::execute(&fx.paths, &fx.pages, &crop_op(1, 0.0, [0, 0, height, width]))
            .expect("crop executes");

        // Every page-sized raster of the page took the same transform, so they
        // all came out exactly the new page's size.
        assert_eq!(
            read_png(&fx.paths.src_dir.join("001.png")).dimensions(),
            (10, 4)
        );
        assert_eq!(
            read_png(&fx.paths.unsaved_clean_layers_dir.join("001.png")).dimensions(),
            (10, 4)
        );
        assert_eq!(
            read_png(&fx.paths.text_images_dir.join("mask_page_1.png")).dimensions(),
            (10, 4)
        );
        assert_eq!(
            read_png(&fx.paths.text_detection_dir.join("00001_mask.png")).dimensions(),
            (10, 4)
        );
        // A raster of another page is untouched.
        assert_eq!(
            read_png(&fx.paths.clean_layers_dir.join("000.png")).dimensions(),
            (REAL_PAGE_SIZES[0][0], REAL_PAGE_SIZES[0][1])
        );

        // Bubbles: the page-1 bubble keeps its page and its centre anchor, and
        // the page-crop bubble's rect is mapped through the SAME transform.
        let bubbles = read_json(&fx.paths.bubbles_file);
        let bubbles = bubbles.as_array().expect("bubbles");
        assert_eq!(bubbles.len(), 3, "a crop archives nothing that overlaps");
        assert_eq!(bubbles[1]["img_idx"], json!(1));
        approx(&bubbles[1]["img_u"], 0.5);
        approx(&bubbles[1]["img_v"], 0.5);
        assert_eq!(bubbles[2]["crop_page_idx"], json!(1));
        let crop = bubbles[2]["crop_rect"].as_array().expect("crop rect");
        approx(&crop[0], 0.1);
        approx(&crop[1], 0.1);
        approx(&crop[2], 0.9);
        approx(&crop[3], 0.9);

        // Legacy typing metadata keeps its page and GAINS the page angle.
        let text_info = read_json(&fx.paths.text_images_dir.join("text_info.json"));
        let entries = text_info.as_array().expect("entries");
        assert_eq!(entries[0]["img_idx"], json!(1));
        approx(&entries[0]["img_u"], 0.5);
        approx(&entries[0]["rotation_deg"], 90.0);
        // The other page's entry is untouched, angle included.
        assert_eq!(entries[1]["img_idx"], json!(3));
        assert!(entries[1].get("rotation_deg").is_none());

        // Detection is REMAPPED under a quarter turn, not trashed: the block at
        // page (1, 2)-(3, 4) becomes canvas (6, 1)-(8, 3).
        let blocks = read_json(&fx.paths.text_detection_dir.join("00001_blocks.json"));
        assert_eq!(blocks["page_idx"], json!(1));
        assert_eq!(blocks["source_size"], json!([10, 4]));
        assert_eq!(blocks["mask_size"], json!([10, 4]));
        assert_eq!(blocks["mask_file"], json!("00001_mask.png"));
        let items = blocks["blocks"].as_array().expect("blocks");
        assert_eq!(items.len(), 1);
        approx(&items[0]["x1"], 6.0);
        approx(&items[0]["y1"], 1.0);
        approx(&items[0]["x2"], 8.0);
        approx(&items[0]["y2"], 3.0);

        // The layer PNG of the page keeps its prefix (the index never moved).
        assert_eq!(
            fs::read(fx.paths.unsaved_layers_dir.join("ps_p0001_uu.png")).expect("layer"),
            b"UL-BASE-1"
        );

        // The page's originals stay recoverable in the trash.
        let trash_base = fx.paths.project_dir.join(super::super::plan::TRASH_DIR_NAME);
        let ids: Vec<_> = fs::read_dir(&trash_base)
            .expect("trash exists")
            .flatten()
            .collect();
        assert_eq!(ids.len(), 1, "one transaction trash folder");
        let trash = ids[0].path();
        assert!(trash.join("ch1/src/001.png").exists());
        assert!(trash.join("ch1_unsaved/clean_layers/001.png").exists());
        assert!(trash.join("ch1/text_images/mask_page_1.png").exists());
        // Nothing was archived: every entry still overlapped the kept region.
        assert!(!trash.join("ch1/deleted_bubbles.json").exists());
        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn crop_with_a_fine_angle_resamples_the_page_and_its_overlays_alike() {
        let fx = build_decodable_fixture();
        prepare_crop_fixture(&fx);
        // 4x10 at 10 deg: 4*cos10 + 10*sin10 = 5.68 -> 6,
        //                 4*sin10 + 10*cos10 = 10.55 -> 11.
        super::execute(&fx.paths, &fx.pages, &crop_op(0, 10.0, [0, 0, 6, 11]))
            .expect("crop executes");

        let page = read_png(&fx.paths.src_dir.join("001.png"));
        assert_eq!(page.dimensions(), (6, 11));
        // Every page-sized raster followed the SAME transform, so the stored
        // "page-sized" invariant still holds after the operation.
        assert_eq!(
            read_png(&fx.paths.unsaved_clean_layers_dir.join("001.png")).dimensions(),
            (6, 11)
        );
        assert_eq!(
            read_png(&fx.paths.text_images_dir.join("mask_page_1.png")).dimensions(),
            (6, 11)
        );

        // The rotated page does not reach the canvas corners, which must stay
        // fully transparent rather than showing a smeared edge pixel.
        assert_eq!(page.get_pixel(0, 0).0[3], 0, "top-left corner");
        assert_eq!(page.get_pixel(5, 10).0[3], 0, "bottom-right corner");
        // The centre of the canvas is the centre of the page, fully opaque and
        // interpolated between the source's neighbouring colours.
        let centre = page.get_pixel(3, 5);
        assert_eq!(centre.0[3], 255, "the page centre must stay opaque");
        assert!(
            centre.0[0] >= 10 && centre.0[0] <= 160,
            "interpolated red {centre:?} must stay inside the source range"
        );
        // Premultiplied interpolation: no transparent pixel's colour bleeds in,
        // so nothing on the page turns black at its edge.
        let mut visible_black = 0usize;
        for pixel in page.pixels() {
            if pixel.0[3] > 200 && pixel.0[2] < 100 {
                visible_black += 1;
            }
        }
        assert_eq!(
            visible_black, 0,
            "a straight-alpha blend would darken the rotated page's edge"
        );
        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn crop_keeps_a_mask_binary_through_a_fine_rotation() {
        let fx = build_decodable_fixture();
        prepare_crop_fixture(&fx);
        super::execute(&fx.paths, &fx.pages, &crop_op(0, 10.0, [0, 0, 6, 11]))
            .expect("crop executes");
        // The fixture's typing mask is fully white ("masked everywhere") and the
        // uncovered canvas is opaque black ("not masked"). Nearest-neighbour
        // sampling must leave exactly those two values — a bilinear pass would
        // produce a grey band the loader's luma threshold would read as a
        // half-masked border.
        let mask_path = fx.paths.text_images_dir.join("mask_page_1.png");
        let mask = read_png(&mask_path);
        let mut masked = 0usize;
        let mut clear = 0usize;
        for (x, y, pixel) in mask.enumerate_pixels() {
            let [r, g, b, a] = pixel.0;
            assert_eq!(a, 255, "({x}, {y}) must stay opaque");
            match (r, g, b) {
                (255, 255, 255) => masked += 1,
                (0, 0, 0) => clear += 1,
                _ => panic!("({x}, {y}) is {pixel:?}, not a binary mask value"),
            }
        }
        // Both values must actually occur, or the assertion above would pass on
        // a mask the rotation left untouched.
        assert!(masked > 0 && clear > 0, "masked {masked}, clear {clear}");

        // The DETECTION mask is not here to be checked: a fine angle trashes the
        // page's detection artifacts rather than remapping an axis-aligned block
        // list onto a freely rotated page.
        assert!(
            !fx.paths.text_detection_dir.join("00001_mask.png").exists(),
            "a fine angle must trash the detection artifacts, not rotate them"
        );
        assert!(
            !fx.paths
                .text_detection_dir
                .join("00001_blocks.json")
                .exists()
        );
        let trash_base = fx.paths.project_dir.join(super::super::plan::TRASH_DIR_NAME);
        let ids: Vec<_> = fs::read_dir(&trash_base)
            .expect("trash exists")
            .flatten()
            .collect();
        let trash = ids[0].path();
        assert!(trash.join("ch1/text_detection/00001_blocks.json").exists());
        assert!(trash.join("ch1/text_detection/00001_mask.png").exists());
        assert_no_transaction_residue(&fx.title);
    }

    #[test]
    fn an_off_size_mask_is_pre_resized_without_losing_its_binarity() {
        let fx = build_decodable_fixture();
        prepare_crop_fixture(&fx);
        // A mask attached at HALF the page's size, with a hard edge across it.
        // Bringing it to page size is the one resampling step that happens
        // BEFORE the nearest-neighbour rotation, so a smooth filter here would
        // hand that rotation a greyscale input and the binarity guarantee would
        // already be gone by the time it ran.
        let [page_w, page_h] = REAL_PAGE_SIZES[1];
        let mut small = image::RgbaImage::new(page_w / 2, page_h / 2);
        for (_, y, pixel) in small.enumerate_pixels_mut() {
            *pixel = image::Rgba(if y < page_h / 4 {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 255]
            });
        }
        small
            .save(fx.paths.text_images_dir.join("mask_page_1.png"))
            .expect("write half-size mask");

        super::execute(&fx.paths, &fx.pages, &crop_op(0, 10.0, [0, 0, 6, 11]))
            .expect("crop executes");
        let mask = read_png(&fx.paths.text_images_dir.join("mask_page_1.png"));
        assert_eq!(mask.dimensions(), (6, 11));
        let mut masked = 0usize;
        let mut clear = 0usize;
        for (x, y, pixel) in mask.enumerate_pixels() {
            match pixel.0 {
                [255, 255, 255, 255] => masked += 1,
                [0, 0, 0, 255] => clear += 1,
                other => panic!(
                    "({x}, {y}) is {other:?}: the page-size pre-resize smoothed a \
                     categorical raster"
                ),
            }
        }
        assert!(masked > 0 && clear > 0, "masked {masked}, clear {clear}");
    }

    #[test]
    fn rotate_page_raster_revalidates_a_corrupt_recipe() {
        let raster = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
        let sane = plan::ComposeRotation {
            quarter_turns: 1,
            angle_deg: 0.0,
        };
        // A raster that is not the size the recipe claims: the crop rectangle
        // would then index a canvas computed from the wrong dimensions.
        assert!(matches!(
            rotate_page_raster(&raster, [8, 8], sane, false),
            Err(PageOpError::InvalidOp(_))
        ));
        // The journal is on-disk data, so its rotation is re-validated here
        // rather than trusted: neither value can reach the pixel loop unchecked.
        for bad in [
            plan::ComposeRotation {
                quarter_turns: 7,
                angle_deg: 0.0,
            },
            plan::ComposeRotation {
                quarter_turns: 0,
                angle_deg: 90.0,
            },
            plan::ComposeRotation {
                quarter_turns: 0,
                angle_deg: f64::NAN,
            },
        ] {
            assert!(
                matches!(
                    rotate_page_raster(&raster, [4, 4], bad, false),
                    Err(PageOpError::InvalidOp(_))
                ),
                "must be refused: {bad:?}"
            );
        }
        // The identity returns the input unchanged, bit for bit.
        let identity = plan::ComposeRotation {
            quarter_turns: 0,
            angle_deg: 0.0,
        };
        let out = rotate_page_raster(&raster, [4, 4], identity, false).expect("identity");
        assert_eq!(out, raster);
    }

    #[test]
    fn a_non_finite_sample_coordinate_reads_as_outside_the_source() {
        // Pixel (0, 0) is a REAL pixel, so a degenerate coordinate must not land
        // on it and smear the page's corner across the canvas.
        let raster = image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 9, 9, 255]));
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(sample_nearest(&raster, bad, 1.0), [0, 0, 0, 0]);
            assert_eq!(sample_nearest(&raster, 1.0, bad), [0, 0, 0, 0]);
            assert_eq!(sample_bilinear_premultiplied(&raster, bad, 1.0), [0, 0, 0, 0]);
            assert_eq!(sample_bilinear_premultiplied(&raster, 1.0, bad), [0, 0, 0, 0]);
        }
        // A coordinate genuinely inside still reads the pixel.
        assert_eq!(sample_nearest(&raster, 1.5, 1.5), [9, 9, 9, 255]);
        assert_eq!(
            sample_bilinear_premultiplied(&raster, 1.5, 1.5),
            [9, 9, 9, 255]
        );
        // And one past the edge is transparent, not the nearest edge pixel.
        assert_eq!(sample_nearest(&raster, 4.5, 1.5), [0, 0, 0, 0]);
    }

    /// Writes a HALF-SIZE binary mask with a hard edge across it, at the path a
    /// page-sized mask is expected at.
    ///
    /// The size mismatch forces `encode_composed_png`'s page-size pre-resize —
    /// the one resampling step every op performs on a mask, whatever else it
    /// does to it — and the hard edge is what a smooth filter turns grey.
    fn write_off_size_binary_mask(path: &Path, page_size: [u32; 2]) {
        let (w, h) = (page_size[0] / 2, page_size[1] / 2);
        let mut mask = image::RgbaImage::new(w.max(1), h.max(1));
        for (_, y, pixel) in mask.enumerate_pixels_mut() {
            // Deliberately NOT at the midpoint, so the edge cannot coincide with
            // a split cut and hide inside one part.
            *pixel = image::Rgba(if y * 5 < h * 3 {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 255]
            });
        }
        mask.save(path).expect("write off-size mask");
    }

    /// Asserts every pixel of `path` is still one of the two binary mask values,
    /// and returns how many of each it found.
    fn assert_mask_is_binary(path: &Path) -> (usize, usize) {
        let mask = read_png(path);
        let (mut masked, mut clear) = (0usize, 0usize);
        for (x, y, pixel) in mask.enumerate_pixels() {
            match pixel.0 {
                [255, 255, 255, 255] => masked += 1,
                [0, 0, 0, 255] => clear += 1,
                other => panic!(
                    "{}: ({x}, {y}) is {other:?} — a mask was resampled with a smooth \
                     filter, and the `!= 0 -> 255` load rule turns that grey into MORE \
                     masked area",
                    path.display()
                ),
            }
        }
        (masked, clear)
    }

    #[test]
    fn a_split_pre_resizes_an_off_size_mask_without_smoothing_it() {
        let fx = build_decodable_fixture();
        prepare_split_fixture(&fx);
        write_off_size_binary_mask(
            &fx.paths.text_images_dir.join("mask_page_1.png"),
            REAL_PAGE_SIZES[1],
        );
        // The blocks document declares a page-sized mask, so the trust gate
        // still passes; only the FILE is off-size, which is exactly the
        // "attached with a resize" case the pre-resize exists for.
        write_off_size_binary_mask(
            &fx.paths.text_detection_dir.join("00001_mask.png"),
            REAL_PAGE_SIZES[1],
        );

        super::execute(&fx.paths, &fx.pages, &split_op()).expect("split executes");

        let mut masked = 0usize;
        let mut clear = 0usize;
        for part in [
            fx.paths.text_images_dir.join("mask_page_1.png"),
            fx.paths.text_images_dir.join("mask_page_2.png"),
            fx.paths.text_detection_dir.join("00001_mask.png"),
            fx.paths.text_detection_dir.join("00002_mask.png"),
        ] {
            let (m, c) = assert_mask_is_binary(&part);
            masked += m;
            clear += c;
        }
        // Both values must really occur, or the assertion above would pass on a
        // mask the operation happened to leave untouched.
        assert!(masked > 0 && clear > 0, "masked {masked}, clear {clear}");
    }

    #[test]
    fn a_stitch_pre_resizes_an_off_size_mask_without_smoothing_it() {
        let fx = build_decodable_fixture();
        write_off_size_binary_mask(
            &fx.paths.text_images_dir.join("mask_page_1.png"),
            REAL_PAGE_SIZES[1],
        );
        write_off_size_binary_mask(
            &fx.paths.text_detection_dir.join("00001_mask.png"),
            REAL_PAGE_SIZES[1],
        );

        super::execute(&fx.paths, &fx.pages, &stitch_op()).expect("stitch executes");

        // Pages 1 and 2 merge onto the 10x10 canvas at the primary's index.
        let (typing_masked, typing_clear) =
            assert_mask_is_binary(&fx.paths.text_images_dir.join("mask_page_1.png"));
        let (det_masked, det_clear) =
            assert_mask_is_binary(&fx.paths.text_detection_dir.join("00001_mask.png"));
        assert!(
            typing_masked > 0 && typing_clear > 0,
            "typing masked {typing_masked}, clear {typing_clear}"
        );
        assert!(
            det_masked > 0 && det_clear > 0,
            "detection masked {det_masked}, clear {det_clear}"
        );
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
