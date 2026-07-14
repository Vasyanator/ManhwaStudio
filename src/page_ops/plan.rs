/*
File: page_ops/plan.rs

Purpose:
Pure planning layer for structural page operations: turns a `PageOpKind` plus a
snapshot of the chapter's page-keyed artifacts into (a) the full old-order ->
new-order permutation and (b) a journal-serializable action plan (renames,
JSON rewrites, new-file creations, trash moves) that `fs_exec` executes as a
crash-safe transaction.

Key structures:
- Permutation / NewPage: pure index math result for an operation.
- ChapterSnapshot / TreeSnapshot / DetectionFiles: input describing what exists
  on disk (built by `fs_exec::scan_chapter`; tests build it directly).
- PageOpPlan / PlannedMove / PlannedCreate / PlannedJsonWrite /
  PlannedTrashWrite: the action plan persisted verbatim into the journal.

Key functions:
- permutation_for_op(): op -> permutation + validation (pure).
- build_plan(): snapshot + op -> PageOpPlan (pure; no filesystem access).
- canonical page-keyed file-name helpers shared with the scanner.

Notes:
All plan paths are strings relative to the TITLE directory (the parent of the
chapter dir) using '/' separators, because the transaction spans both the
committed chapter tree and its sibling `{chapter}_unsaved` staging tree.
No function in this file touches the filesystem.
*/

use super::{PageOpError, PageOpKind};
use crate::config;
use crate::page_ops::json_remap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

/// Journal file written into the chapter dir for the duration of a transaction.
pub(crate) const JOURNAL_FILE_NAME: &str = "page_ops_journal.json";
/// Phase-B journal slot. It is created durably before the phase-A slot is
/// removed, so the commit marker never requires replacing an existing file.
pub(crate) const JOURNAL_B_FILE_NAME: &str = "page_ops_journal.b.json";
/// Chapter-local trash directory; each transaction uses one `{id}` subfolder.
pub(crate) const TRASH_DIR_NAME: &str = ".pageop_trash";
/// Prefix of every transaction temp file (phase A renames / staged creations).
pub(crate) const TEMP_PREFIX: &str = "__ms_pageop_";
/// Temp suffix chosen so temps never match the image-extension filters of
/// `project::collect_images` or the overlay/mask loaders mid-transaction.
const TEMP_SUFFIX: &str = ".mstmp";
/// Copy of the bubbles removed together with deleted pages (per tree, in trash).
pub(crate) const DELETED_BUBBLES_FILE: &str = "deleted_bubbles.json";
/// Copy of `text_info.json` entries removed together with deleted pages.
pub(crate) const DELETED_TEXT_INFO_FILE: &str = "deleted_text_info.json";
/// Copy of `layers.json` page entries removed together with deleted pages.
pub(crate) const DELETED_LAYERS_PAGES_FILE: &str = "deleted_layers_pages.json";

/// Inclusive bounds for `CreateBlank` dimensions, in pixels.
const BLANK_MIN_SIDE_PX: u32 = 1;
const BLANK_MAX_SIDE_PX: u32 = 20_000;

// ---------------------------------------------------------------------------
// Canonical page-keyed file names.
//
// These formats are the on-disk contract of other modules; each helper cites
// the authoritative definition. They are duplicated here (rather than imported)
// because the originals are private functions of tab modules this engine must
// not depend on.
// ---------------------------------------------------------------------------

/// Canonical zero-based page stem, `000`, `001`, ... — must stay byte-identical
/// to `project::normalize_page_filenames` (`format!("{:03}", page.idx)`) so a
/// reopened chapter needs no renames.
#[must_use]
pub(crate) fn canonical_page_stem(idx: usize) -> String {
    format!("{idx:03}")
}

/// Per-page layer-PNG prefix `ps_p{page:04}_` — mirrors
/// `models/layer_model/persist.rs::page_file_prefix`.
#[must_use]
pub(crate) fn layers_png_prefix(idx: usize) -> String {
    format!("ps_p{idx:04}_")
}

/// Typing-tab page mask `mask_page_{idx}.png` (no zero padding) — mirrors
/// `tabs/typing/mask.rs::mask_file_name_for_page`.
#[must_use]
pub(crate) fn typing_mask_file_name(idx: usize) -> String {
    format!("mask_page_{idx}.png")
}

/// Text-detector blocks file `{idx:05}_blocks.json` — mirrors
/// `tabs/translation/tab.rs::text_detection_blocks_file_path`.
#[must_use]
pub(crate) fn detection_blocks_file_name(idx: usize) -> String {
    format!("{idx:05}_blocks.json")
}

/// Text-detector mask file `{idx:05}_mask.png` — mirrors
/// `tabs/translation/tab.rs::text_detection_mask_file_name` (the cleaning tab
/// uses the same format).
#[must_use]
pub(crate) fn detection_mask_file_name(idx: usize) -> String {
    format!("{idx:05}_mask.png")
}

/// Parses the page index out of a layer PNG name (`ps_p{page:04}_...`).
/// Returns `None` for names that do not match the pattern.
#[must_use]
pub(crate) fn parse_layers_png_page_idx(name: &str) -> Option<usize> {
    let rest = name.strip_prefix("ps_p")?;
    let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    // The canonical form pads to 4 digits; wider indices keep growing digits.
    if digits_len < 4 {
        return None;
    }
    let (digits, tail) = rest.split_at(digits_len);
    if !tail.starts_with('_') {
        return None;
    }
    digits.parse::<usize>().ok()
}

/// Parses the page index out of `mask_page_{idx}.png`.
#[must_use]
pub(crate) fn parse_typing_mask_page_idx(name: &str) -> Option<usize> {
    let stem = name.strip_prefix("mask_page_")?.strip_suffix(".png")?;
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse::<usize>().ok()
}

/// Parses the page index out of `{idx}_blocks.json` (5+ digit zero padding).
#[must_use]
pub(crate) fn parse_detection_blocks_page_idx(name: &str) -> Option<usize> {
    parse_detection_page_idx(name, "_blocks.json")
}

/// Parses the page index out of `{idx}_mask.png` (5+ digit zero padding).
#[must_use]
pub(crate) fn parse_detection_mask_page_idx(name: &str) -> Option<usize> {
    parse_detection_page_idx(name, "_mask.png")
}

fn parse_detection_page_idx(name: &str, suffix: &str) -> Option<usize> {
    let digits = name.strip_suffix(suffix)?;
    // The canonical writer pads to 5 digits; accept 5 or more so indices past
    // 99999 (which widen naturally) still parse.
    if digits.len() < 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<usize>().ok()
}

// ---------------------------------------------------------------------------
// Permutation math.
// ---------------------------------------------------------------------------

/// Content of a page created by the operation (journal-serializable so
/// recovery can re-stage it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum NewPageContent {
    /// Copy this absolute source file into `src/`.
    CopyFile { source: PathBuf },
    /// Encode a solid-fill PNG (straight, non-premultiplied RGBA).
    BlankPng {
        width: u32,
        height: u32,
        rgba: [u8; 4],
    },
}

/// One page the operation adds, keyed by its index in the NEW order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewPage {
    pub new_idx: usize,
    /// Lower-cased extension of the created file (`png`/`jpg`/`jpeg`).
    pub extension: String,
    pub content: NewPageContent,
}

/// Full permutation produced by an operation over `old_page_count` pages.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Permutation {
    /// `old_to_new[i]` is the new index of old page `i`; `None` = deleted.
    pub old_to_new: Vec<Option<usize>>,
    pub new_page_count: usize,
    /// Pages created by the operation, ordered by `new_idx`.
    pub new_pages: Vec<NewPage>,
}

/// Computes the old->new permutation for `op` over `old_page_count` pages and
/// validates the request (index ranges, non-empty inputs, supported insert
/// extensions, blank dimensions, "at least one page must remain").
///
/// Pure: never touches the filesystem — file readability is validated by the
/// executor before planning.
///
/// # Errors
/// Returns [`PageOpError::InvalidOp`] when the request does not apply to a
/// chapter with `old_page_count` pages.
pub(crate) fn permutation_for_op(
    op: &PageOpKind,
    old_page_count: usize,
) -> Result<Permutation, PageOpError> {
    match op {
        PageOpKind::Move { from, to } => {
            if *from >= old_page_count || *to >= old_page_count {
                return Err(PageOpError::InvalidOp(format!(
                    "move {from} -> {to} is out of range for {old_page_count} page(s)"
                )));
            }
            // New order = old order with `from` removed and re-inserted at `to`.
            let mut order: Vec<usize> = (0..old_page_count).filter(|i| i != from).collect();
            order.insert(*to, *from);
            let mut old_to_new = vec![None; old_page_count];
            for (new_idx, old_idx) in order.iter().enumerate() {
                old_to_new[*old_idx] = Some(new_idx);
            }
            Ok(Permutation {
                old_to_new,
                new_page_count: old_page_count,
                new_pages: Vec::new(),
            })
        }
        PageOpKind::InsertFiles { at, files } => {
            if files.is_empty() {
                return Err(PageOpError::InvalidOp(
                    "insert requested with an empty file list".to_string(),
                ));
            }
            if *at > old_page_count {
                return Err(PageOpError::InvalidOp(format!(
                    "insert position {at} is out of range for {old_page_count} page(s)"
                )));
            }
            let mut new_pages = Vec::with_capacity(files.len());
            for (offset, file) in files.iter().enumerate() {
                let extension = file
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                // Must match the extension filter of `project::collect_images`,
                // otherwise the inserted page would be invisible on next load.
                if !matches!(extension.as_str(), "png" | "jpg" | "jpeg") {
                    return Err(PageOpError::InvalidOp(format!(
                        "unsupported page image extension '{extension}' for '{}' \
                         (supported: png, jpg, jpeg)",
                        file.display()
                    )));
                }
                new_pages.push(NewPage {
                    new_idx: at + offset,
                    extension,
                    content: NewPageContent::CopyFile {
                        source: file.clone(),
                    },
                });
            }
            Ok(insert_permutation(old_page_count, *at, new_pages))
        }
        PageOpKind::CreateBlank {
            at,
            width,
            height,
            rgba,
        } => {
            if *at > old_page_count {
                return Err(PageOpError::InvalidOp(format!(
                    "insert position {at} is out of range for {old_page_count} page(s)"
                )));
            }
            let side_ok = |v: u32| (BLANK_MIN_SIDE_PX..=BLANK_MAX_SIDE_PX).contains(&v);
            if !side_ok(*width) || !side_ok(*height) {
                return Err(PageOpError::InvalidOp(format!(
                    "blank page dimensions {width}x{height} are outside \
                     [{BLANK_MIN_SIDE_PX}, {BLANK_MAX_SIDE_PX}]"
                )));
            }
            let new_pages = vec![NewPage {
                new_idx: *at,
                extension: "png".to_string(),
                content: NewPageContent::BlankPng {
                    width: *width,
                    height: *height,
                    rgba: *rgba,
                },
            }];
            Ok(insert_permutation(old_page_count, *at, new_pages))
        }
        PageOpKind::Delete { indices } => {
            if indices.is_empty() {
                return Err(PageOpError::InvalidOp(
                    "delete requested with an empty index list".to_string(),
                ));
            }
            let deleted: BTreeSet<usize> = indices.iter().copied().collect();
            if let Some(max) = deleted.iter().next_back()
                && *max >= old_page_count
            {
                return Err(PageOpError::InvalidOp(format!(
                    "delete index {max} is out of range for {old_page_count} page(s)"
                )));
            }
            if deleted.len() >= old_page_count {
                return Err(PageOpError::InvalidOp(
                    "cannot delete every page: a chapter must keep at least one page".to_string(),
                ));
            }
            let mut old_to_new = Vec::with_capacity(old_page_count);
            let mut kept = 0usize;
            for i in 0..old_page_count {
                if deleted.contains(&i) {
                    old_to_new.push(None);
                } else {
                    old_to_new.push(Some(kept));
                    kept += 1;
                }
            }
            Ok(Permutation {
                old_to_new,
                new_page_count: kept,
                new_pages: Vec::new(),
            })
        }
    }
}

/// Shared shift math for `InsertFiles` / `CreateBlank`: old pages before `at`
/// keep their index, pages at or after `at` shift up by the insertion count.
fn insert_permutation(old_page_count: usize, at: usize, new_pages: Vec<NewPage>) -> Permutation {
    let inserted = new_pages.len();
    let old_to_new = (0..old_page_count)
        .map(|i| Some(if i < at { i } else { i + inserted }))
        .collect();
    Permutation {
        old_to_new,
        new_page_count: old_page_count + inserted,
        new_pages,
    }
}

// ---------------------------------------------------------------------------
// Chapter snapshot (plan input).
// ---------------------------------------------------------------------------

/// Which directory a `text_info.json` file was found in. Modern chapters keep
/// it in `layers/`; legacy chapters in `text_images/` (see
/// `tabs/typing/tab/render_jobs.rs` read order). Both are remapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextInfoLocation {
    LayersDir,
    TextImagesDir,
}

/// One parsed `text_info.json` (top-level array entries).
#[derive(Debug, Clone)]
pub(crate) struct TextInfoFile {
    pub location: TextInfoLocation,
    pub entries: Vec<Value>,
}

/// Page-keyed artifacts of ONE tree (committed chapter dir or `_unsaved`).
/// Only files that actually exist are listed; the plan never renames a file
/// that is not in the snapshot.
#[derive(Debug, Clone, Default)]
pub(crate) struct TreeSnapshot {
    /// Tree root relative to the title dir (`{chapter}` or `{chapter}_unsaved`).
    pub tree_rel: String,
    /// Stems of `clean_layers/*.png` files.
    pub clean_overlay_stems: BTreeSet<String>,
    /// Every file name in `layers/`.
    pub layers_files: BTreeSet<String>,
    /// Parsed `layers/layers.json`, when present.
    pub layers_manifest: Option<Value>,
    /// Every file name in `text_images/`.
    pub text_images_files: BTreeSet<String>,
    /// Parsed `text_info.json` files found in this tree.
    pub text_info: Vec<TextInfoFile>,
    /// Parsed `translation_bubbles.json` entries, when the file exists.
    pub bubbles: Option<Vec<Value>>,
}

/// Parsed state of a text-detection blocks file.
#[derive(Debug, Clone)]
pub(crate) enum DetectionBlocks {
    /// Valid JSON: content is rewritten (`mask_file`) during remap.
    Parsed(Value),
    /// Unparseable JSON: renamed as an opaque file (its optional `mask_file`
    /// reference resolves gracefully to the per-page default on load).
    Opaque,
}

/// Text-detection artifacts for one page index (committed tree only — the
/// `text_detection/` dir has no unsaved mirror, see `ProjectPaths`).
#[derive(Debug, Clone)]
pub(crate) struct DetectionFiles {
    pub page_idx: usize,
    pub blocks: Option<DetectionBlocks>,
    pub has_mask: bool,
}

/// Everything the planner needs to know about the chapter on disk.
#[derive(Debug, Clone)]
pub(crate) struct ChapterSnapshot {
    /// Chapter dir relative to the title dir (its file name).
    pub chapter_rel: String,
    /// File name of each current page in `src/`, index-aligned with the
    /// current page order.
    pub page_file_names: Vec<String>,
    pub committed: TreeSnapshot,
    pub unsaved: TreeSnapshot,
    pub detection: Vec<DetectionFiles>,
}

// ---------------------------------------------------------------------------
// Action plan (journal payload).
// ---------------------------------------------------------------------------

/// Where a phase-A temp ends up in phase B.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MoveDest {
    /// Renamed to this final path (a page-keyed file surviving under a new key).
    Final { path: String },
    /// Moved into the transaction trash (deleted page artifacts).
    Trash { path: String },
    /// Deleted at commit (original of a JSON document that phase B rewrites;
    /// its content survives remapped in the corresponding `PlannedJsonWrite`).
    Discard,
}

/// One two-phase file move: phase A renames `from` -> `temp` (reversible),
/// phase B resolves `temp` per `dest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedMove {
    pub from: String,
    pub temp: String,
    pub dest: MoveDest,
}

/// One file the operation creates: staged at `temp` during phase A, renamed to
/// `target` in phase B.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedCreate {
    pub temp: String,
    pub target: String,
    pub content: NewPageContent,
}

/// One JSON document rewritten by the transaction. `content` is the complete
/// new file body, computed at plan time and journaled so recovery can re-apply
/// it without re-reading (possibly already-moved) inputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedJsonWrite {
    pub target: String,
    pub content: String,
}

/// An extra file written into the trash (copies of deleted JSON entries).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedTrashWrite {
    pub target: String,
    pub content: String,
}

/// Complete journaled plan of one page operation. All paths are relative to
/// the title dir with '/' separators.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PageOpPlan {
    pub old_to_new: Vec<Option<usize>>,
    pub new_page_count: usize,
    /// Trash root for this transaction, `{chapter}/.pageop_trash/{id}`.
    pub trash_root: String,
    pub moves: Vec<PlannedMove>,
    pub creates: Vec<PlannedCreate>,
    pub json_writes: Vec<PlannedJsonWrite>,
    pub trash_writes: Vec<PlannedTrashWrite>,
    /// Plan-time diagnostics (stale indices, opaque files). Logged by the
    /// executor; not part of the journal.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl PageOpPlan {
    /// True when the operation changes nothing on disk (identity permutation
    /// over already-canonical names with no content changes).
    #[must_use]
    pub(crate) fn is_noop(&self) -> bool {
        self.moves.is_empty()
            && self.creates.is_empty()
            && self.json_writes.is_empty()
            && self.trash_writes.is_empty()
    }
}

/// Splits `file.ext` into (`file`, `Some("ext")`); no dot yields (`name`, `None`).
fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    }
}

/// Internal accumulator that owns temp-name numbering and `from`-path dedup.
struct PlanBuilder {
    trash_root: String,
    temp_id: u128,
    temp_counter: usize,
    planned_from: HashSet<String>,
    moves: Vec<PlannedMove>,
    creates: Vec<PlannedCreate>,
    json_writes: Vec<PlannedJsonWrite>,
    trash_writes: Vec<PlannedTrashWrite>,
    warnings: Vec<String>,
}

impl PlanBuilder {
    fn new(trash_root: String, temp_id: u128) -> Self {
        Self {
            trash_root,
            temp_id,
            temp_counter: 0,
            planned_from: HashSet::new(),
            moves: Vec::new(),
            creates: Vec::new(),
            json_writes: Vec::new(),
            trash_writes: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Unique temp path in the same directory as `sibling_rel` (same-volume
    /// rename) that no chapter loader recognizes as an image/JSON artifact.
    fn next_temp(&mut self, sibling_rel: &str) -> String {
        let n = self.temp_counter;
        self.temp_counter += 1;
        let dir = match sibling_rel.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        };
        let name = format!("{TEMP_PREFIX}{}_{n}{TEMP_SUFFIX}", self.temp_id);
        if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        }
    }

    /// Plans `from` -> `to`, skipping identity renames and duplicate sources.
    fn rename(&mut self, from: String, to: String) {
        if from == to || !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Final { path: to },
        });
    }

    /// Plans moving `from` into the trash, preserving its title-relative path.
    fn trash(&mut self, from: String) {
        if !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        let path = format!("{}/{from}", self.trash_root);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Trash { path },
        });
    }

    /// Plans discarding the file at `from` at commit time (phase A still moves
    /// it to a temp first, so the step stays reversible until commit).
    fn discard(&mut self, from: String) {
        if !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Discard,
        });
    }

    /// Plans rewriting the JSON document at `target` with `content`; when the
    /// file currently exists (`had_original`) its original is discarded at
    /// commit (the remapped content supersedes it).
    fn rewrite_json(&mut self, target: String, content: String, had_original: bool) {
        if had_original {
            self.discard(target.clone());
        }
        self.json_writes.push(PlannedJsonWrite { target, content });
    }

    fn write_trash_extra(&mut self, rel_inside_trash: String, content: String) {
        let target = format!("{}/{rel_inside_trash}", self.trash_root);
        self.trash_writes.push(PlannedTrashWrite { target, content });
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }
}

/// Serializes a JSON value the way the app writes its project documents
/// (pretty, matching `ProjectData::autosave_bubbles` / layer-manifest writes).
fn to_pretty(value: &Value) -> Result<String, PageOpError> {
    serde_json::to_string_pretty(value)
        .map_err(|err| PageOpError::Json(format!("serialize remapped document: {err}")))
}

/// Builds the full action plan for `op` from the chapter snapshot.
///
/// `trash_id` names both the trash subfolder and the temp-file namespace of
/// this transaction (the executor derives it from `SystemTime`).
///
/// # Errors
/// - [`PageOpError::InvalidOp`] for requests that do not apply to the snapshot
///   (bad indices, unsupported extensions, un-remappable legacy documents).
/// - [`PageOpError::Json`] when a page-keyed document cannot be re-serialized.
pub(crate) fn build_plan(
    snapshot: &ChapterSnapshot,
    op: &PageOpKind,
    trash_id: u128,
) -> Result<PageOpPlan, PageOpError> {
    let old_page_count = snapshot.page_file_names.len();
    let permutation = permutation_for_op(op, old_page_count)?;
    let map = &permutation.old_to_new;

    let trash_root = format!("{}/{TRASH_DIR_NAME}/{trash_id}", snapshot.chapter_rel);
    let mut b = PlanBuilder::new(trash_root.clone(), trash_id);

    plan_src_pages(&mut b, snapshot, map)?;
    for tree in [&snapshot.committed, &snapshot.unsaved] {
        plan_clean_overlays(&mut b, snapshot, tree, map);
        plan_layer_pngs(&mut b, tree, map);
        plan_layers_manifest(&mut b, tree, map)?;
        plan_text_info(&mut b, tree, map)?;
        plan_typing_masks(&mut b, tree, map);
        plan_bubbles(&mut b, tree, map)?;
    }
    plan_detection(&mut b, snapshot, map)?;
    plan_creates(&mut b, snapshot, &permutation);

    Ok(PageOpPlan {
        old_to_new: permutation.old_to_new,
        new_page_count: permutation.new_page_count,
        trash_root,
        moves: b.moves,
        creates: b.creates,
        json_writes: b.json_writes,
        trash_writes: b.trash_writes,
        warnings: b.warnings,
    })
}

/// Source page files: rename surviving pages onto the canonical stem of their
/// NEW index (extension preserved), move deleted pages to the trash.
fn plan_src_pages(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    map: &[Option<usize>],
) -> Result<(), PageOpError> {
    for (old_idx, name) in snapshot.page_file_names.iter().enumerate() {
        let from = format!("{}/{}/{name}", snapshot.chapter_rel, config::SRC_DIR);
        match map[old_idx] {
            Some(new_idx) => {
                let (_, ext) = split_name(name);
                let ext = ext.ok_or_else(|| {
                    PageOpError::InvalidOp(format!("page file '{name}' has no extension"))
                })?;
                let target = format!(
                    "{}/{}/{}.{ext}",
                    snapshot.chapter_rel,
                    config::SRC_DIR,
                    canonical_page_stem(new_idx)
                );
                b.rename(from, target);
            }
            None => b.trash(from),
        }
    }
    Ok(())
}

/// Clean overlays are keyed by the PAGE'S CURRENT STEM (`{stem}.png`), in both
/// the committed and unsaved `clean_layers/` dirs.
fn plan_clean_overlays(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
) {
    for (old_idx, name) in snapshot.page_file_names.iter().enumerate() {
        let (stem, _) = split_name(name);
        if !tree.clean_overlay_stems.contains(stem) {
            continue;
        }
        let from = format!(
            "{}/{}/{stem}.png",
            tree.tree_rel,
            config::CLEAN_LAYERS_DIR
        );
        match map[old_idx] {
            Some(new_idx) => {
                let target = format!(
                    "{}/{}/{}.png",
                    tree.tree_rel,
                    config::CLEAN_LAYERS_DIR,
                    canonical_page_stem(new_idx)
                );
                b.rename(from, target);
            }
            None => b.trash(from),
        }
    }
}

/// Layer PNGs are keyed by the page index embedded in their name
/// (`ps_p{page:04}_...`); the index prefix is load-bearing because
/// `persist.rs::prune_orphan_pngs` deletes by that prefix.
fn plan_layer_pngs(b: &mut PlanBuilder, tree: &TreeSnapshot, map: &[Option<usize>]) {
    for name in &tree.layers_files {
        let Some(old_idx) = parse_layers_png_page_idx(name) else {
            continue;
        };
        if !name.ends_with(".png") {
            continue;
        }
        let from = format!("{}/{}/{name}", tree.tree_rel, config::LAYERS_DIR);
        if old_idx >= map.len() {
            b.warn(format!(
                "layer PNG '{}' references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                from,
                map.len()
            ));
            continue;
        }
        match map[old_idx] {
            Some(new_idx) => {
                if let Some(new_name) =
                    json_remap::remap_layers_png_name(name, old_idx, new_idx)
                {
                    let target =
                        format!("{}/{}/{new_name}", tree.tree_rel, config::LAYERS_DIR);
                    b.rename(from, target);
                }
            }
            None => b.trash(from),
        }
    }
}

/// `layers/layers.json`: remap `img_idx` and the embedded `ps_p...` file
/// references; page entries of deleted pages are removed and archived in the
/// trash as `deleted_layers_pages.json`.
fn plan_layers_manifest(
    b: &mut PlanBuilder,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
) -> Result<(), PageOpError> {
    let Some(manifest) = &tree.layers_manifest else {
        return Ok(());
    };
    let remap = json_remap::remap_layers_manifest(manifest, map)?;
    for warning in remap.warnings {
        b.warn(format!("{}/layers/layers.json: {warning}", tree.tree_rel));
    }
    if !remap.deleted_pages.is_empty() {
        b.write_trash_extra(
            format!(
                "{}/{}/{DELETED_LAYERS_PAGES_FILE}",
                tree.tree_rel,
                config::LAYERS_DIR
            ),
            to_pretty(&Value::Array(remap.deleted_pages))?,
        );
    }
    if remap.changed {
        let target = format!("{}/{}/layers.json", tree.tree_rel, config::LAYERS_DIR);
        b.rewrite_json(target, to_pretty(&remap.manifest)?, true);
    }
    Ok(())
}

/// `text_info.json` (legacy typing metadata, possibly present in both the
/// `layers/` and `text_images/` dirs of each tree): remap `img_idx`, drop
/// entries of deleted pages (archived as `deleted_text_info.json`), and move
/// the overlay PNGs referenced only by dropped entries to the trash.
fn plan_text_info(
    b: &mut PlanBuilder,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
) -> Result<(), PageOpError> {
    for file in &tree.text_info {
        let (dir_name, dir_files) = match file.location {
            TextInfoLocation::LayersDir => (config::LAYERS_DIR, &tree.layers_files),
            TextInfoLocation::TextImagesDir => {
                (config::TEXT_IMAGES_DIR, &tree.text_images_files)
            }
        };
        let remap = json_remap::remap_text_info(&file.entries, map)?;
        let surviving_files: HashSet<&str> = remap
            .kept
            .iter()
            .filter_map(|entry| entry.get("file").and_then(Value::as_str))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        for warning in remap.warnings {
            b.warn(format!(
                "{}/{dir_name}/text_info.json: {warning}",
                tree.tree_rel
            ));
        }
        if !remap.deleted.is_empty() {
            b.write_trash_extra(
                format!("{}/{dir_name}/{DELETED_TEXT_INFO_FILE}", tree.tree_rel),
                to_pretty(&Value::Array(remap.deleted))?,
            );
        }
        // Overlay PNGs referenced by dropped entries (plus their optional
        // `*_layout.png` companion) become unreferenced: keep them recoverable
        // in the trash instead of leaving orphans behind.
        for file_name in remap.deleted_files {
            let mut candidates = vec![file_name.clone()];
            if let Some(stem) = file_name.strip_suffix(".png") {
                candidates.push(format!("{stem}_layout.png"));
            }
            for candidate in candidates {
                if dir_files.contains(&candidate) && !surviving_files.contains(candidate.as_str()) {
                    b.trash(format!("{}/{dir_name}/{candidate}", tree.tree_rel));
                }
            }
        }
        if remap.changed {
            let target = format!("{}/{dir_name}/text_info.json", tree.tree_rel);
            b.rewrite_json(target, to_pretty(&Value::Array(remap.kept))?, true);
        }
    }
    Ok(())
}

/// Typing-tab page masks `text_images/mask_page_{idx}.png`.
fn plan_typing_masks(b: &mut PlanBuilder, tree: &TreeSnapshot, map: &[Option<usize>]) {
    for name in &tree.text_images_files {
        let Some(old_idx) = parse_typing_mask_page_idx(name) else {
            continue;
        };
        let from = format!("{}/{}/{name}", tree.tree_rel, config::TEXT_IMAGES_DIR);
        if old_idx >= map.len() {
            b.warn(format!(
                "typing mask '{}' references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                from,
                map.len()
            ));
            continue;
        }
        match map[old_idx] {
            Some(new_idx) => {
                let target = format!(
                    "{}/{}/{}",
                    tree.tree_rel,
                    config::TEXT_IMAGES_DIR,
                    typing_mask_file_name(new_idx)
                );
                b.rename(from, target);
            }
            None => b.trash(from),
        }
    }
}

/// `translation_bubbles.json`: remap `img_idx` + `crop_page_idx`; bubbles of
/// deleted pages are removed and archived in the trash as
/// `deleted_bubbles.json`.
fn plan_bubbles(
    b: &mut PlanBuilder,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
) -> Result<(), PageOpError> {
    let Some(entries) = &tree.bubbles else {
        return Ok(());
    };
    let remap = json_remap::remap_bubbles(entries, map)?;
    for warning in remap.warnings {
        b.warn(format!(
            "{}/{}: {warning}",
            tree.tree_rel,
            config::BUBBLES_FILE
        ));
    }
    if !remap.deleted.is_empty() {
        b.write_trash_extra(
            format!("{}/{DELETED_BUBBLES_FILE}", tree.tree_rel),
            to_pretty(&Value::Array(remap.deleted))?,
        );
    }
    if remap.changed {
        let target = format!("{}/{}", tree.tree_rel, config::BUBBLES_FILE);
        b.rewrite_json(target, to_pretty(&Value::Array(remap.kept))?, true);
    }
    Ok(())
}

/// `text_detection/` (committed tree only): `{idx:05}_blocks.json` +
/// `{idx:05}_mask.png`, with the `mask_file` reference inside a parsed blocks
/// file rewritten to the new default mask name.
fn plan_detection(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    map: &[Option<usize>],
) -> Result<(), PageOpError> {
    for det in &snapshot.detection {
        let old_idx = det.page_idx;
        let dir = format!("{}/{}", snapshot.chapter_rel, config::TEXT_DETECTION_DIR);
        let blocks_from = format!("{dir}/{}", detection_blocks_file_name(old_idx));
        let mask_from = format!("{dir}/{}", detection_mask_file_name(old_idx));
        if old_idx >= map.len() {
            b.warn(format!(
                "text-detection files for page {old_idx} reference a page beyond the \
                 current {} page(s); left untouched",
                map.len()
            ));
            continue;
        }
        match map[old_idx] {
            Some(new_idx) => {
                if det.has_mask {
                    b.rename(
                        mask_from,
                        format!("{dir}/{}", detection_mask_file_name(new_idx)),
                    );
                }
                let blocks_target = format!("{dir}/{}", detection_blocks_file_name(new_idx));
                match &det.blocks {
                    Some(DetectionBlocks::Parsed(value)) => {
                        let (remapped, changed) =
                            json_remap::remap_detection_blocks(value, old_idx, new_idx);
                        if changed {
                            // Content changed (`mask_file` reference): journal
                            // the remapped body at the NEW path and discard the
                            // superseded original.
                            let content = to_pretty(&remapped)?;
                            b.discard(blocks_from);
                            b.json_writes.push(PlannedJsonWrite {
                                target: blocks_target,
                                content,
                            });
                        } else if blocks_from != blocks_target {
                            b.rename(blocks_from, blocks_target);
                        }
                    }
                    Some(DetectionBlocks::Opaque) if blocks_from != blocks_target => {
                        b.warn(format!(
                            "{blocks_from}: not valid JSON; renamed without rewriting \
                             its mask_file reference"
                        ));
                        b.rename(blocks_from, blocks_target);
                    }
                    Some(DetectionBlocks::Opaque) | None => {}
                }
            }
            None => {
                if det.blocks.is_some() {
                    b.trash(blocks_from);
                }
                if det.has_mask {
                    b.trash(mask_from);
                }
            }
        }
    }
    Ok(())
}

/// New pages (insert / blank) staged into `src/` under their canonical stems.
fn plan_creates(b: &mut PlanBuilder, snapshot: &ChapterSnapshot, permutation: &Permutation) {
    for page in &permutation.new_pages {
        let target = format!(
            "{}/{}/{}.{}",
            snapshot.chapter_rel,
            config::SRC_DIR,
            canonical_page_stem(page.new_idx),
            page.extension
        );
        let temp = b.next_temp(&target);
        b.creates.push(PlannedCreate {
            temp,
            target,
            content: page.content.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn map_of(perm: &Permutation) -> Vec<Option<usize>> {
        perm.old_to_new.clone()
    }

    #[test]
    fn move_forward_and_backward_permutations() {
        // Move page 0 to the end of a 4-page chapter.
        let perm = permutation_for_op(&PageOpKind::Move { from: 0, to: 3 }, 4).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(3), Some(0), Some(1), Some(2)]);
        assert_eq!(perm.new_page_count, 4);

        // Move page 3 to the front.
        let perm = permutation_for_op(&PageOpKind::Move { from: 3, to: 0 }, 4).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(1), Some(2), Some(3), Some(0)]);

        // Move to the same position is the identity.
        let perm = permutation_for_op(&PageOpKind::Move { from: 2, to: 2 }, 4).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2), Some(3)]);

        // Adjacent swap forward: `to` is an index into the order WITHOUT `from`.
        let perm = permutation_for_op(&PageOpKind::Move { from: 1, to: 2 }, 4).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(2), Some(1), Some(3)]);
    }

    #[test]
    fn move_rejects_out_of_range() {
        assert!(matches!(
            permutation_for_op(&PageOpKind::Move { from: 4, to: 0 }, 4),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(&PageOpKind::Move { from: 0, to: 4 }, 4),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn insert_at_start_and_end() {
        let files = vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.JPG")];
        // At the start: every old page shifts by 2.
        let perm = permutation_for_op(
            &PageOpKind::InsertFiles {
                at: 0,
                files: files.clone(),
            },
            3,
        )
        .expect("valid");
        assert_eq!(map_of(&perm), vec![Some(2), Some(3), Some(4)]);
        assert_eq!(perm.new_page_count, 5);
        assert_eq!(perm.new_pages[0].new_idx, 0);
        assert_eq!(perm.new_pages[1].new_idx, 1);
        // Extensions are lower-cased.
        assert_eq!(perm.new_pages[1].extension, "jpg");

        // At the end: old pages keep their indices.
        let perm = permutation_for_op(&PageOpKind::InsertFiles { at: 3, files }, 3).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2)]);
        assert_eq!(perm.new_pages[0].new_idx, 3);
        assert_eq!(perm.new_pages[1].new_idx, 4);
    }

    #[test]
    fn insert_rejects_empty_list_bad_position_and_bad_extension() {
        assert!(matches!(
            permutation_for_op(&PageOpKind::InsertFiles { at: 0, files: vec![] }, 3),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(
                &PageOpKind::InsertFiles {
                    at: 4,
                    files: vec![PathBuf::from("/tmp/a.png")]
                },
                3
            ),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(
                &PageOpKind::InsertFiles {
                    at: 0,
                    files: vec![PathBuf::from("/tmp/a.webp")]
                },
                3
            ),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn create_blank_validates_dimensions() {
        let ok = permutation_for_op(
            &PageOpKind::CreateBlank {
                at: 1,
                width: 800,
                height: 1200,
                rgba: [255, 255, 255, 255],
            },
            2,
        )
        .expect("valid");
        assert_eq!(map_of(&ok), vec![Some(0), Some(2)]);
        assert_eq!(ok.new_pages[0].extension, "png");

        for (w, h) in [(0, 100), (100, 0), (20_001, 100), (100, 20_001)] {
            assert!(matches!(
                permutation_for_op(
                    &PageOpKind::CreateBlank {
                        at: 0,
                        width: w,
                        height: h,
                        rgba: [0, 0, 0, 255],
                    },
                    2
                ),
                Err(PageOpError::InvalidOp(_))
            ));
        }
    }

    #[test]
    fn delete_multiple_pages() {
        let perm = permutation_for_op(
            &PageOpKind::Delete {
                // Unsorted with a duplicate: the engine sorts and dedups.
                indices: vec![3, 1, 1],
            },
            5,
        )
        .expect("valid");
        assert_eq!(
            map_of(&perm),
            vec![Some(0), None, Some(1), None, Some(2)]
        );
        assert_eq!(perm.new_page_count, 3);
    }

    #[test]
    fn delete_rejects_empty_all_and_out_of_range() {
        assert!(matches!(
            permutation_for_op(&PageOpKind::Delete { indices: vec![] }, 3),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(
                &PageOpKind::Delete {
                    indices: vec![0, 1, 2]
                },
                3
            ),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(&PageOpKind::Delete { indices: vec![3] }, 3),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn page_keyed_name_parsers_roundtrip() {
        assert_eq!(parse_layers_png_page_idx("ps_p0007_ab12.png"), Some(7));
        assert_eq!(
            parse_layers_png_page_idx("ps_p0007_ab12_text.png"),
            Some(7)
        );
        assert_eq!(parse_layers_png_page_idx("ps_p12_x.png"), None);
        assert_eq!(parse_layers_png_page_idx("other.png"), None);

        assert_eq!(parse_typing_mask_page_idx("mask_page_12.png"), Some(12));
        assert_eq!(parse_typing_mask_page_idx("mask_page_.png"), None);
        assert_eq!(parse_typing_mask_page_idx("mask_page_1.jpg"), None);

        assert_eq!(parse_detection_blocks_page_idx("00012_blocks.json"), Some(12));
        assert_eq!(parse_detection_mask_page_idx("00012_mask.png"), Some(12));
        assert_eq!(parse_detection_blocks_page_idx("012_blocks.json"), None);
    }

    fn snapshot_for_plan() -> ChapterSnapshot {
        let committed = TreeSnapshot {
            tree_rel: "ch1".to_string(),
            clean_overlay_stems: ["000", "002"].iter().map(ToString::to_string).collect(),
            layers_files: ["layers.json", "ps_p0000_u1.png", "ps_p0002_u2_text.png"]
                .iter()
                .map(ToString::to_string)
                .collect(),
            layers_manifest: Some(serde_json::json!({
                "schema_version": 3,
                "pages": [
                    {"img_idx": 0, "tree": [
                        {"uid": "u1", "name": "L", "z": 0, "visible": true,
                         "opacity": 1.0, "base_file": "ps_p0000_u1.png"}
                    ]},
                    {"img_idx": 2, "tree": [
                        {"uid": "u2", "name": "T", "z": 0, "visible": true,
                         "opacity": 1.0, "rendered_file": "ps_p0002_u2_text.png"}
                    ]}
                ]
            })),
            text_images_files: ["mask_page_1.png", "typing_overlay_p0001_1.png"]
                .iter()
                .map(ToString::to_string)
                .collect(),
            text_info: vec![TextInfoFile {
                location: TextInfoLocation::TextImagesDir,
                entries: vec![serde_json::json!({
                    "img_idx": 1, "file": "typing_overlay_p0001_1.png"
                })],
            }],
            bubbles: Some(vec![serde_json::json!({
                "id": 1, "img_idx": 3, "img_u": 0.5, "img_v": 0.5,
                "side": "left", "text": "t", "original_text": "o"
            })]),
        };
        let unsaved = TreeSnapshot {
            tree_rel: "ch1_unsaved".to_string(),
            ..TreeSnapshot::default()
        };
        ChapterSnapshot {
            chapter_rel: "ch1".to_string(),
            page_file_names: vec![
                "000.png".to_string(),
                "001.png".to_string(),
                "002.jpg".to_string(),
                "003.png".to_string(),
            ],
            committed,
            unsaved,
            detection: vec![DetectionFiles {
                page_idx: 1,
                blocks: Some(DetectionBlocks::Parsed(serde_json::json!({
                    "source_size": [100, 200],
                    "blocks": [],
                    "mask_file": "00001_mask.png"
                }))),
                has_mask: true,
            }],
        }
    }

    #[test]
    fn build_plan_move_covers_every_category_with_canonical_names() {
        let snapshot = snapshot_for_plan();
        // Move page 0 to the end: 0->3, 1->0, 2->1, 3->2.
        let plan = build_plan(&snapshot, &PageOpKind::Move { from: 0, to: 3 }, 42)
            .expect("plan builds");
        assert_eq!(
            plan.old_to_new,
            vec![Some(3), Some(0), Some(1), Some(2)]
        );

        let final_targets: Vec<(&str, &str)> = plan
            .moves
            .iter()
            .filter_map(|m| match &m.dest {
                MoveDest::Final { path } => Some((m.from.as_str(), path.as_str())),
                MoveDest::Trash { .. } | MoveDest::Discard => None,
            })
            .collect();
        // Source pages keep their extension under the new canonical stem.
        assert!(final_targets.contains(&("ch1/src/000.png", "ch1/src/003.png")));
        assert!(final_targets.contains(&("ch1/src/002.jpg", "ch1/src/001.jpg")));
        // Clean overlays follow the page stem.
        assert!(final_targets.contains(&(
            "ch1/clean_layers/000.png",
            "ch1/clean_layers/003.png"
        )));
        assert!(final_targets.contains(&(
            "ch1/clean_layers/002.png",
            "ch1/clean_layers/001.png"
        )));
        // Layer PNGs get the new `ps_p{page:04}_` prefix.
        assert!(final_targets.contains(&(
            "ch1/layers/ps_p0000_u1.png",
            "ch1/layers/ps_p0003_u1.png"
        )));
        assert!(final_targets.contains(&(
            "ch1/layers/ps_p0002_u2_text.png",
            "ch1/layers/ps_p0001_u2_text.png"
        )));
        // Typing mask follows the raw index format.
        assert!(final_targets.contains(&(
            "ch1/text_images/mask_page_1.png",
            "ch1/text_images/mask_page_0.png"
        )));
        // Detection mask follows the 5-digit format.
        assert!(final_targets.contains(&(
            "ch1/text_detection/00001_mask.png",
            "ch1/text_detection/00000_mask.png"
        )));

        // JSON rewrites: bubbles + layers manifest + text_info + blocks.
        let json_targets: Vec<&str> =
            plan.json_writes.iter().map(|w| w.target.as_str()).collect();
        assert!(json_targets.contains(&"ch1/translation_bubbles.json"));
        assert!(json_targets.contains(&"ch1/layers/layers.json"));
        assert!(json_targets.contains(&"ch1/text_images/text_info.json"));
        assert!(json_targets.contains(&"ch1/text_detection/00000_blocks.json"));

        // The rewritten blocks file references the NEW mask name.
        let blocks = plan
            .json_writes
            .iter()
            .find(|w| w.target == "ch1/text_detection/00000_blocks.json")
            .expect("blocks rewrite present");
        assert!(blocks.content.contains("00000_mask.png"));

        // No trash content for a pure move.
        assert!(plan.trash_writes.is_empty());
        assert!(
            plan.moves
                .iter()
                .all(|m| !matches!(m.dest, MoveDest::Trash { .. }))
        );
        // Every temp lives in the same directory as its source.
        for m in &plan.moves {
            let from_dir = m.from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let temp_dir = m.temp.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            assert_eq!(from_dir, temp_dir, "temp of {} in same dir", m.from);
        }
    }

    #[test]
    fn build_plan_delete_moves_artifacts_to_trash() {
        let snapshot = snapshot_for_plan();
        let plan = build_plan(&snapshot, &PageOpKind::Delete { indices: vec![1] }, 7)
            .expect("plan builds");
        assert_eq!(plan.trash_root, "ch1/.pageop_trash/7");
        let trash_targets: Vec<&str> = plan
            .moves
            .iter()
            .filter_map(|m| match &m.dest {
                MoveDest::Trash { path } => Some(path.as_str()),
                MoveDest::Final { .. } | MoveDest::Discard => None,
            })
            .collect();
        assert!(trash_targets.contains(&"ch1/.pageop_trash/7/ch1/src/001.png"));
        assert!(
            trash_targets.contains(&"ch1/.pageop_trash/7/ch1/text_images/mask_page_1.png")
        );
        assert!(trash_targets
            .contains(&"ch1/.pageop_trash/7/ch1/text_detection/00001_blocks.json"));
        assert!(trash_targets
            .contains(&"ch1/.pageop_trash/7/ch1/text_detection/00001_mask.png"));
        // The deleted page's overlay PNG (referenced by its text_info entry)
        // is archived too.
        assert!(trash_targets
            .contains(&"ch1/.pageop_trash/7/ch1/text_images/typing_overlay_p0001_1.png"));
        // Deleted text_info entries are archived.
        assert!(plan.trash_writes.iter().any(|w| w.target
            == "ch1/.pageop_trash/7/ch1/text_images/deleted_text_info.json"));
    }

    #[test]
    fn delete_keeps_text_overlay_referenced_by_surviving_entry() {
        let mut snapshot = snapshot_for_plan();
        snapshot.committed.text_info[0].entries = vec![
            serde_json::json!({"img_idx": 1, "file": "shared.png"}),
            serde_json::json!({"img_idx": 2, "file": "shared.png"}),
        ];
        snapshot.committed.text_images_files.insert("shared.png".to_string());
        let plan = build_plan(&snapshot, &PageOpKind::Delete { indices: vec![1] }, 8)
            .expect("plan builds");
        assert!(!plan.moves.iter().any(|planned| {
            planned.from == "ch1/text_images/shared.png"
                && matches!(planned.dest, MoveDest::Trash { .. })
        }));
    }

    #[test]
    fn build_plan_identity_move_is_noop() {
        // A snapshot whose names are already canonical and an identity move.
        let mut snapshot = snapshot_for_plan();
        snapshot.detection.clear();
        let plan = build_plan(&snapshot, &PageOpKind::Move { from: 1, to: 1 }, 9)
            .expect("plan builds");
        assert!(plan.is_noop(), "identity op should plan no actions");
    }
}
