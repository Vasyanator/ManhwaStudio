/*
File: page_ops/mod.rs

Purpose:
GUI-free engine for STRUCTURAL page operations on a loaded chapter: reordering,
inserting (from files or generated blank pages) and deleting pages.

Main responsibilities:
- define the `PageOpKind` request model shared by the page-manager tab and the app;
- plan (`plan.rs`) and execute (`fs_exec.rs`, JSON rewrites in `json_remap.rs`)
  the operation on disk as a journaled, crash-safe transaction that keeps the
  committed chapter folder and the `_unsaved` staging mirror consistent with
  each other.

Key structures:
- PageOpKind: one structural operation, indices in the CURRENT page order.
- PageOpOutcome: old->new index mapping produced by a successful operation.
- PageOpError: typed failure of planning or execution.

Key functions:
- execute_page_op(): run one operation as a crash-safe transaction.
- recover_pending_page_op(): resolve an interrupted transaction at project load.

Notes:
Structural operations are applied immediately to BOTH trees (committed and
`_unsaved`) — they are not staged and are not undone by discarding unsaved
changes. Callers must quiesce all writers (layer saver barrier, bubble flush,
overlay autosave pause) before executing an operation, and must reload the
project afterwards. Must never run on the GUI thread.
*/

mod fs_exec;
mod json_remap;
mod plan;

use std::path::PathBuf;

/// One structural page operation over the loaded chapter.
///
/// All indices refer to the CURRENT page order (`ProjectData::pages`) at the
/// moment the operation is requested; the engine converts them into a full
/// old-order -> new-order permutation.
#[derive(Debug, Clone, PartialEq, Eq)]
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
///   documents, stale page list).
/// - [`PageOpError::Image`] — an inserted file is not a readable image or a
///   blank page failed to encode.
/// - [`PageOpError::Json`] — an authoritative page-keyed document could not be
///   parsed or re-serialized (nothing is changed on disk in that case).
/// - [`PageOpError::Io`] / [`PageOpError::Journal`] — filesystem failure; the
///   chapter is either rolled back (before the commit point) or completes on
///   the next load via the journal (after it).
pub fn execute_page_op(
    paths: &crate::project::ProjectPaths,
    pages: &[crate::project::Page],
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
