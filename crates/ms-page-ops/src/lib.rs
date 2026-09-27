/*
File: crates/ms-page-ops/src/lib.rs
Crate root of `ms-page-ops`, re-exported by the binary as `crate::page_ops`.

Purpose:
GUI-free engine for STRUCTURAL page operations on a loaded chapter: reordering,
inserting (from files or generated blank pages), deleting, stitching several
pages into one, splitting one page into several, and rotating + cropping one
page in place.

Main responsibilities:
- define the `PageOpKind` request model shared by the page-manager tab and the app;
- plan (`plan.rs`) and execute (`fs_exec.rs`, JSON rewrites in `json_remap.rs`)
  the operation on disk as a journaled, crash-safe transaction that keeps the
  committed chapter folder and the `_unsaved` staging mirror consistent with
  each other.

Key structures:
- PageOpKind: one structural operation, indices in the CURRENT page order.
- StitchPlacement: one source page's affine placement inside a stitched canvas.
- SplitAxis: orientation of the parallel cut lines of a split.
- PageRotation / RotatedPage: the page-rotation model (quarter turns + a fine
  straightening angle) with its canvas size and point mappings, shared with the
  page-manager UI so both sides use one set of formulas.
- PageOpOutcome: old->new index mapping produced by a successful operation.
- PageOpError: typed failure of planning or execution.
- Page / ProjectPaths: the chapter page list and file layout the transaction
  operates on. Declared here and re-exported by `ms-project` — see the note at
  their declaration for why the cycle points this way.

Key functions:
- execute_page_op(): run one operation as a crash-safe transaction.
- recover_pending_page_op(): resolve an interrupted transaction at project load.
- chapter_docs::chapter_doc_for_write(): the `DocRef` (with the chapter-format hint) every
  writer of a chapter's `layers` / bubbles document uses; chapter_doc_durability(): its
  staging-vs-committed durability.

Notes:
Structural operations are applied immediately to BOTH trees (committed and
`_unsaved`) — they are not staged and are not undone by discarding unsaved
changes. Callers must quiesce all writers (layer saver barrier, bubble flush,
overlay autosave pause) before executing an operation, and must reload the
project afterwards. Must never run on the GUI thread.
*/

#![warn(clippy::all)]

// Exposed as a MODULE rather than through item re-exports: the page-manager UI
// imports it directly (`crate::page_ops::crop_geometry::…`) so that a rotation
// preview uses the engine's own canvas size, point mappings and crop legality
// rule instead of a second copy of the formulas.
pub mod crop_geometry;
// The chapter's owned documents as `ms_docstore::DocRef`s and the rule-B.3 "new chapter
// document joins the chapter's format" decision, shared by the save merge (`ms-project`)
// and the staging savers (`ms-models`).
pub mod chapter_docs;
mod fs_exec;
mod json_remap;
mod plan;

// The stitch UI pre-validates a layout before it can request the operation. It
// must use the engine's own bounds, not a second copy of the numbers.
pub use plan::{STITCH_MAX_SCALE, STITCH_MAX_SIDE_PX, STITCH_MAX_TOTAL_PX};
// Same rule for a page ROTATION: its canvas size, its point mappings and the
// legality of a crop rectangle are the engine's formulas, and a UI preview
// imports them instead of restating them.
pub use crop_geometry::MAX_FINE_ANGLE_DEG;

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Chapter-tree data types.
//
// `Page` and `ProjectPaths` are the two plain data types this transaction reads
// and rewrites, and they are declared HERE rather than in `ms-project` for one
// structural reason: `ProjectData::load` must resolve a pending journal through
// `recover_pending_page_op` BEFORE any reconcile pass touches the chapter files,
// while this engine needs the page list and the path set to operate on. That is
// a cycle, and only one of its two edges can be removed without weakening a
// crash-safety invariant — so the types moved down to the layer that already
// owns the chapter layout (every `ProjectPaths` field is a `ms_config` name
// joined onto a chapter or title directory).
//
// `ms-project` re-exports both, so `crate::project::{Page, ProjectPaths}` stays
// a valid path in the binary and no call site changed.
// ---------------------------------------------------------------------------

/// One source page of a chapter: its zero-based position in the CURRENT page
/// order and the absolute path of its image file in `{chapter}/src/`.
#[derive(Debug, Clone)]
pub struct Page {
    pub idx: usize,
    pub path: PathBuf,
}

/// Every file and directory of one loaded chapter, resolved once at project
/// load. Title-scoped entries live next to the chapter (`{title_dir}/...`),
/// chapter-scoped ones inside it; the `unsaved_*` entries mirror the committed
/// tree inside `{title_dir}/{chapter}_unsaved/`, which is where in-session
/// mutations are written until an explicit "save to project".
///
/// Paths are absolute and are NOT guaranteed to exist — a chapter that never
/// used a subsystem simply has no such folder.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ProjectPaths {
    pub project_dir: PathBuf,
    pub title_dir: PathBuf,
    pub notes_file: PathBuf,
    /// Title-scoped favorite characters of the typing tab's character table
    /// (`{title_dir}/char_favorites.json`). Title- rather than chapter-scoped on
    /// purpose: every chapter of one manga shares one list.
    pub char_favorites_file: PathBuf,
    /// Title-scoped color presets of the typing tab's color pickers
    /// (`{title_dir}/color_presets.json`). Title- rather than chapter-scoped for
    /// the same reason as `char_favorites_file`: one manga is typeset with one
    /// set of colors.
    pub color_presets_file: PathBuf,
    pub bubbles_file: PathBuf,
    pub src_dir: PathBuf,
    pub clean_layers_dir: PathBuf,
    pub cleaned_dir: PathBuf,
    pub alt_vers_dir: PathBuf,
    pub saved_dir: PathBuf,
    pub image_bubbles_dir: PathBuf,
    pub text_images_dir: PathBuf,
    pub layers_dir: PathBuf,
    pub text_detection_dir: PathBuf,
    pub characters_dir: PathBuf,
    pub terms_file: PathBuf,
    pub settings_file: PathBuf,
    // Unsaved staging folder: {title_dir}/{chapter_name}_unsaved/
    // All in-session mutations are written here; the main folder is only
    // updated on an explicit "save to project" action.
    pub unsaved_dir: PathBuf,
    pub unsaved_bubbles_file: PathBuf,
    pub unsaved_clean_layers_dir: PathBuf,
    pub unsaved_image_bubbles_dir: PathBuf,
    pub unsaved_text_images_dir: PathBuf,
    pub unsaved_layers_dir: PathBuf,
}

/// Where one source page lands inside a stitched canvas.
///
/// The placement is a pure affine map from the source page's OWN pixels to the
/// new canvas pixels, and it is the single geometric truth every remapped
/// artifact of that page is routed through:
///
/// ```text
/// map_point(x, y) = ((x - crop.x) * scale + dx, (y - crop.y) * scale + dy)
/// map_extent(m)   = m * scale
/// placed size     = (round(crop.w * scale), round(crop.h * scale))
/// ```
///
/// `crop` is `[x, y, w, h]` in the source page's own pixels and must lie inside
/// that page; `scale` is uniform and must be in `(0, 16]`; `dx`/`dy` are the
/// top-left of the placed image inside the new canvas, and the whole placed
/// rectangle must lie inside it. A STITCH placement never rotates: merging N
/// pages onto one canvas has no rotation to express, so the request type
/// carries none and the engine builds it with [`PageRotation::IDENTITY`]. The
/// affine itself is rotation-capable — a rotating placement first maps the page
/// through a [`RotatedPage`], and its `crop` is then read in that rotated
/// canvas' pixels rather than the page's own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StitchPlacement {
    /// Index of the source page in the CURRENT page order.
    pub page_idx: usize,
    /// `[x, y, w, h]` region of the source page that is placed, in its own px.
    pub crop: [u32; 4],
    /// Uniform scale factor applied to the cropped region, in `(0, 16]`.
    pub scale: f32,
    /// X of the placed image's top-left corner in the new canvas, in new px.
    pub dx: i64,
    /// Y of the placed image's top-left corner in the new canvas, in new px.
    pub dy: i64,
}

/// Orientation of the parallel cut lines of a [`PageOpKind::Split`].
///
/// `Horizontal` means horizontal lines, i.e. the cut coordinates are Y values
/// and the resulting parts are stacked top to bottom. `Vertical` is the
/// transpose. The two are never mixed in one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    /// Horizontal cut lines; cuts are Y coordinates, parts run top to bottom.
    Horizontal,
    /// Vertical cut lines; cuts are X coordinates, parts run left to right.
    Vertical,
}

/// One structural page operation over the loaded chapter.
///
/// All indices refer to the CURRENT page order (`ProjectData::pages`) at the
/// moment the operation is requested; the engine converts them into a full
/// old-order -> new-order permutation.
///
/// `Eq` is deliberately NOT implemented: `Stitch` carries float geometry, for
/// which reflexive equality does not hold.
#[derive(Debug, Clone, PartialEq)]
pub enum PageOpKind {
    /// Move the page currently at `from` so that it occupies index `to` in the
    /// new order (`to` is an index into the new order, i.e. after `from` was
    /// removed).
    Move { from: usize, to: usize },
    /// Copy the given image files into `src/` as new pages; the first inserted
    /// page gets index `at` in the new order, the rest follow in list order.
    InsertFiles { at: usize, files: Vec<PathBuf> },
    /// Generate a solid-color page of `width` x `height` pixels filled with
    /// `rgba` (straight, non-premultiplied) and insert it at index `at`.
    CreateBlank {
        at: usize,
        width: u32,
        height: u32,
        rgba: [u8; 4],
    },
    /// Delete the pages at these current indices (the engine sorts and dedups).
    /// All page artifacts are moved into the chapter-local trash directory, not
    /// destroyed, so the operation is manually recoverable.
    Delete { indices: Vec<usize> },
    /// Cut ONE page into `cuts.len() + 1` parts along parallel cut lines and
    /// replace it with those parts, in the order given by `order`.
    ///
    /// Each part is a crop of the source page; its new page image is the
    /// cropped pixels, always encoded as PNG. Page-sized rasters (clean
    /// overlays, typing masks, trustworthy detection masks) are cut the same
    /// way. Layers are NEVER cut: a layer crossed by a cut moves whole to the
    /// part holding the largest share of its on-page area (a tie goes to the
    /// top/left part), so its geometry may legitimately hang off the new page's
    /// edge. A bubble goes to the part containing its anchor point.
    ///
    /// A part may also be DELETED instead of becoming a page (`deleted[k]`):
    /// its pixels are never written and every entry routed to it (bubbles,
    /// layers, layer PNGs, detection blocks, `text_info` overlays) is archived
    /// or trashed exactly as it would be by [`PageOpKind::Delete`] — never
    /// relocated onto a surviving part.
    ///
    /// The surviving part the user ordered FIRST takes the source page's index;
    /// every page after it shifts up by `kept - 1`, where `kept` is the number
    /// of parts not deleted. The source page's files are moved into the
    /// chapter-local trash, so the operation is manually recoverable.
    Split {
        /// Current index of the page to cut.
        page_idx: usize,
        /// Orientation of every cut line.
        axis: SplitAxis,
        /// Cut positions in SOURCE pixels along the cut axis: strictly
        /// increasing, strictly inside the page, every part at least 1 px.
        cuts: Vec<u32>,
        /// `order[k]` is the position of GEOMETRIC part `k` (`k == 0` is the
        /// topmost / leftmost part) among ALL parts, deleted ones included: a
        /// permutation of `0..cuts.len() + 1`. Deleted parts keep a position so
        /// that the relative order of the survivors is read off directly; the
        /// engine compacts the surviving positions into consecutive page
        /// indices.
        order: Vec<usize>,
        /// `deleted[k] == true` drops geometric part `k` instead of turning it
        /// into a page. Same length as `order`; at least one part must survive
        /// (deleting every part is a `Delete` of the page and is refused).
        deleted: Vec<bool>,
    },
    /// Rotate ONE page and replace it with a rectangular region of the
    /// rotated result. The page COUNT is unchanged and `old_to_new` is the
    /// identity: only this page's size and content change.
    ///
    /// The rotation is applied FIRST, producing a ROTATED CANVAS which is the
    /// axis-aligned bounding box of the rotated page with the page centred in
    /// it; `rect` then selects the kept region of that canvas. Every
    /// page-sized raster (clean overlay, typing mask, trustworthy detection
    /// mask) is rotated and cropped identically, and every page-keyed document
    /// has its geometry mapped through the same transform.
    ///
    /// EXACTNESS: with `angle_deg == 0.0` the operation is bit-exact — a
    /// quarter turn is an integer pixel permutation and the crop copies pixels
    /// unchanged, so no resampling happens anywhere. A non-zero `angle_deg`
    /// resamples, and additionally DEGRADES two things that cannot represent a
    /// rotated rectangle: the axis-aligned rects stored by bubbles
    /// (`rect_coords`, `text_areas`) become the bounding box of the rotated
    /// rect, and detection documents are moved to the trash with a warning
    /// rather than remapped wrongly (they are regenerable). Both degradations
    /// are reported as runtime warnings.
    ///
    /// CONTENT OUTSIDE THE FRAME: an entry that still overlaps the kept region
    /// survives and may hang off the new page's edge, exactly as a layer
    /// crossed by a split cut does. An entry lying ENTIRELY outside the kept
    /// region is archived the way [`PageOpKind::Delete`] archives a page's
    /// entries, never silently dropped. The source page's files are moved into
    /// the chapter-local trash, so the operation is manually recoverable.
    Crop {
        /// Current index of the page to crop.
        page_idx: usize,
        /// Clockwise 90-degree steps applied before the fine angle; `0..=3`.
        quarter_turns: u8,
        /// Fine straightening angle in DEGREES, clockwise-positive, applied
        /// about the centre of the quarter-turned page. Must lie strictly
        /// inside `(-45.0, 45.0)`; a larger rotation belongs in
        /// `quarter_turns`.
        angle_deg: f64,
        /// Kept region `[x, y, w, h]` in the ROTATED CANVAS's pixel space.
        /// Must be fully inside the canvas with `w >= 1` and `h >= 1`.
        rect: [u32; 4],
    },
    /// Merge >= 2 pages into ONE page that takes the position of the lowest
    /// source index (`primary = min(page_idx)`); the other sources disappear
    /// from the order and every page after them shifts down.
    ///
    /// The new page image is a `width` x `height` PNG filled with `background`
    /// (straight, non-premultiplied RGBA; `[0, 0, 0, 0]` = transparent) onto
    /// which each source page is drawn per its [`StitchPlacement`], in list
    /// order (later entries paint over earlier ones). Page-sized rasters (clean
    /// overlays, typing masks) are composed the same way, and every page-keyed
    /// JSON document is merged with its geometry mapped through the placements.
    ///
    /// `placements` must hold at least two entries with unique `page_idx`; the
    /// source page files themselves are moved into the chapter-local trash, so
    /// the operation is manually recoverable.
    Stitch {
        placements: Vec<StitchPlacement>,
        width: u32,
        height: u32,
        /// Straight (non-premultiplied) RGBA fill of the uncovered canvas.
        background: [u8; 4],
    },
}

/// Result of a successfully executed page operation.
#[derive(Debug, Clone)]
pub struct PageOpOutcome {
    /// Mapping from old page index to its index in the new order; `None` means
    /// the page was deleted. `old_to_new.len()` equals the old page count.
    pub old_to_new: Vec<Option<usize>>,
    /// Total number of pages after the operation.
    pub new_page_count: usize,
}

/// Typed failure of a page operation. Messages are technical (log/English);
/// UI layers map the variants to localized user-facing text.
#[derive(Debug, thiserror::Error)]
pub enum PageOpError {
    /// The request does not apply to the current page list (index out of range,
    /// empty file list, zero dimensions, ...).
    #[error("invalid page operation: {0}")]
    InvalidOp(String),
    /// A filesystem step failed; the transaction was rolled back or is
    /// recoverable from the journal on next load.
    #[error("filesystem error during page operation: {0}")]
    Io(#[from] std::io::Error),
    /// Reading/encoding an image failed (inserted file unreadable, blank page
    /// encode failure, ...).
    #[error("image error during page operation: {0}")]
    Image(String),
    /// Rewriting one of the page-keyed JSON documents failed.
    #[error("json rewrite failed during page operation: {0}")]
    Json(String),
    /// The transaction journal is unusable (unresolved previous transaction,
    /// unreadable/unsupported journal file). The journal is never deleted on
    /// this error, so the on-disk evidence stays available for inspection.
    #[error("page operation journal error: {0}")]
    Journal(String),
}

/// Executes `op` on disk as a journaled crash-safe transaction over BOTH trees
/// (committed chapter dir and the `_unsaved` staging mirror).
///
/// `pages` is the CURRENT page order (`ProjectData::pages`, position-keyed);
/// `paths` must belong to the same loaded chapter. On success the chapter is
/// fully consistent under the new order — source pages and clean overlays sit
/// on the canonical `000, 001, …` stems of the new order (so the next project
/// load's `normalize_page_filenames` is a no-op), layer PNGs carry the new
/// `ps_p{page:04}_` prefixes, and every page-keyed JSON (`translation_bubbles`,
/// `layers.json`, `text_info.json`, text-detection blocks) is remapped in both
/// trees. Deleted page artifacts are moved (not destroyed) into
/// `{chapter}/.pageop_trash/{id}/`, with removed bubble/text/layer JSON entries
/// archived next to them.
///
/// Callers must quiesce all chapter writers first and reload the project
/// afterwards. Synchronous disk I/O — worker thread only, never the GUI thread.
///
/// # Errors
/// - [`PageOpError::InvalidOp`] — the request does not apply (bad indices,
///   unsupported insert extension, deleting every page, un-migrated legacy
///   documents, stale page list, a stitch whose crops/placements fall outside
///   their page or canvas or whose merged pages share a layer uid, a split
///   whose cuts are not strictly increasing inside the page, whose `order` is
///   not a permutation of its parts, whose `deleted` has the wrong length or
///   which deletes every part).
/// - [`PageOpError::Image`] — an inserted file is not a readable image, a
///   blank page failed to encode, or a stitched/split source page could not be
///   decoded, cropped or composed.
/// - [`PageOpError::Json`] — an authoritative page-keyed document could not be
///   parsed or re-serialized (nothing is changed on disk in that case).
/// - [`PageOpError::Io`] / [`PageOpError::Journal`] — filesystem failure; the
///   chapter is either rolled back (before the commit point) or completes on
///   the next load via the journal (after it).
pub fn execute_page_op(
    paths: &crate::ProjectPaths,
    pages: &[crate::Page],
    op: &PageOpKind,
) -> Result<PageOpOutcome, PageOpError> {
    fs_exec::execute(paths, pages, op)
}

/// Called early in project load: completes (roll-forward) or rolls back an
/// interrupted transaction using the on-disk journal. No-op when no journal.
///
/// Must run BEFORE any reconcile/normalize pass touches the chapter files:
/// until the journal is resolved, the transaction owns the page keying of
/// every artifact. Synchronous disk I/O — worker/load thread only.
///
/// # Errors
/// [`PageOpError::Io`] / [`PageOpError::Journal`] when the journal exists but
/// cannot be read or replayed; the journal file is left in place so the state
/// stays inspectable and a later load can retry.
pub fn recover_pending_page_op(project_dir: &std::path::Path) -> Result<(), PageOpError> {
    fs_exec::recover(project_dir)
}
