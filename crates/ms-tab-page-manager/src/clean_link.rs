/*
File: crates/ms-tab-page-manager/src/clean_link.rs

Purpose:
GUI-free, I/O-free link state between a page card and its clean card: what the page's clean
is (none / OK / problem) and where its thumbnail comes from, plus the "bind to …" menu split.

Key items:
- ModelPageClean / model_page_materialized(): the per-page model facts snapshotted under a short
  lock once per model revision (a page beyond the model's count is NOT materialized).
- PageCleanLink / CleanLinkProblem / CleanThumbSource: the per-page link state.
- page_clean_link(): THE rule combining the model snapshot with the worker-scanned
  `PageCleanEntry` (materialized model wins; otherwise the loader's view of the disk).
- bind_targets() / bind_fit(): the two "bind to …" menu groups and the size fit of an unassigned
  clean against one page (used for the mismatch warning before binding as-is).

Notes:
Pure functions over snapshots; unit-tested as a table. Every disk fact arrives through
`ms_models::clean_assign` scans on the clean worker.
*/

use std::path::PathBuf;

use ms_models::clean_assign::{classify_clean_fit, CleanPageFit, PageCleanEntry, PageCleanResolution, UnassignedClean, LOADER_CLEAN_SCOPE};
use ms_models::clean_overlays_model::CleanOverlaysModel;

/// What `CleanOverlaysModel` holds for one page, snapshotted on the GUI thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ModelPageClean {
    /// `!is_overlay_virtual_absent(idx)`: the model holds pixels (or unsaved changes) for the page,
    /// which the canvas shows and the autosave will write under the page's canonical name.
    pub(crate) materialized: bool,
    /// The model's remembered overlay size (`overlay_size`), `[width, height]`, when known.
    pub(crate) size: Option<[usize; 2]>,
}

impl ModelPageClean {
    /// Snapshots page `idx` of `model` (the caller holds the model lock).
    pub(crate) fn of(model: &CleanOverlaysModel, idx: usize) -> Self {
        Self { materialized: model_page_materialized(model, idx), size: model.overlay_size(idx) }
    }
}

/// Whether the model holds pixels (or unsaved changes) for page `idx`. A page beyond the model's
/// page count is NOT materialized: `is_overlay_virtual_absent` answers `false` there (it only
/// knows the pages it was built with), which would otherwise read as "the model owns this page's
/// clean" for a page it has never seen (e.g. a model shorter than the page list).
pub(crate) fn model_page_materialized(model: &CleanOverlaysModel, idx: usize) -> bool {
    idx < model.count() && !model.is_overlay_virtual_absent(idx)
}

/// Where a clean card's thumbnail comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CleanThumbSource {
    /// The in-memory model (unsaved edits live only there): `thumbs.rs` model-clean thumbnails.
    Model,
    /// A clean file on disk: the path thumbnail pipeline.
    File(PathBuf),
}

/// Why a page's canonical clean file does not bind (the overlay loader skips or fails on it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CleanLinkProblem {
    /// The clean's `[width, height]` differs from the page's.
    SizeMismatch { clean: [u32; 2], page: [u32; 2] },
    /// The clean file's header could not be read.
    CleanUnreadable(String),
    /// The clean is readable but the page's header could not be read.
    PageUnreadable(String),
}

/// The link state of one page card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PageCleanLink {
    /// No clean in the model and no canonical file in either tree.
    None,
    /// The page has a working clean (model pixels, or a bound file the loader will load).
    Ok {
        thumb: CleanThumbSource,
        /// Canonical clean file name of the page (display only).
        file_name: String,
        /// `[width, height]` when known.
        size: Option<[u32; 2]>,
    },
    /// A canonical clean file exists but does not bind; `file` is the one the loader looks at.
    Problem { problem: CleanLinkProblem, file: PathBuf, size: Option<[u32; 2]> },
}

impl PageCleanLink {
    /// Whether the page has any clean (OK or problematic) that an unlink would detach and a bind
    /// would replace.
    #[must_use]
    pub(crate) fn has_clean(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Computes page link state from the model snapshot and the page's scanned entry.
///
/// Rule: (1) a materialized model page is `Ok { Model }` — that is what the canvas shows and the
/// autosave writes, whatever the disk holds (a mismatched committed file under it is overwritten
/// at save; known limitation); (2) otherwise the entry is resolved in `LOADER_CLEAN_SCOPE`
/// (staged shadows committed): absent -> `None`, bound -> `Ok { File }`, any non-binding file ->
/// `Problem`. A missing `entry` (no scan yet, or a non-UTF-8 page stem) resolves as absent.
#[must_use]
pub(crate) fn page_clean_link(entry: Option<&PageCleanEntry>, model: ModelPageClean, canonical_name: &str) -> PageCleanLink {
    if model.materialized {
        let size = model.size.and_then(|[width, height]| Some([u32::try_from(width).ok()?, u32::try_from(height).ok()?]));
        return PageCleanLink::Ok { thumb: CleanThumbSource::Model, file_name: canonical_name.to_string(), size };
    }
    let Some(entry) = entry else {
        return PageCleanLink::None;
    };
    match entry.resolve(LOADER_CLEAN_SCOPE) {
        PageCleanResolution::Absent => PageCleanLink::None,
        PageCleanResolution::Bound { file, size } => PageCleanLink::Ok { thumb: CleanThumbSource::File(file), file_name: canonical_name.to_string(), size: Some(size) },
        PageCleanResolution::SizeMismatch { file, clean, page } => {
            PageCleanLink::Problem { problem: CleanLinkProblem::SizeMismatch { clean, page }, file, size: Some(clean) }
        }
        PageCleanResolution::CleanUnreadable { file, error } => PageCleanLink::Problem { problem: CleanLinkProblem::CleanUnreadable(error), file, size: None },
        PageCleanResolution::PageUnreadable { file, error } => {
            // The clean header was readable (the fit rule reads it first), so the scan has its size.
            let size = [entry.staged.as_ref(), entry.committed.as_ref()].into_iter().flatten().find(|probe| probe.path == file).and_then(|probe| probe.size.clone().ok());
            PageCleanLink::Problem { problem: CleanLinkProblem::PageUnreadable(error), file, size }
        }
    }
}

/// The two groups of the "bind to …" menu, page indices in ascending order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct BindTargets {
    /// Pages without any clean («Без клина»): binding just attaches.
    pub(crate) without_clean: Vec<usize>,
    /// Pages that already have a clean, OK or problematic («Есть клин, будет замена»): binding
    /// first detaches that clean into an unassigned file.
    pub(crate) with_clean: Vec<usize>,
}

/// Splits pages by [`PageCleanLink::has_clean`]; `links[i]` is page `i`'s link.
#[must_use]
pub(crate) fn bind_targets(links: &[PageCleanLink]) -> BindTargets {
    let (with_clean, without_clean): (Vec<usize>, Vec<usize>) = (0..links.len()).partition(|&idx| links[idx].has_clean());
    BindTargets { without_clean, with_clean }
}

/// The binding fit of unassigned clean `source` (its surviving copy) against page `entry`: the
/// same exact-size rule the loader applies after the bind renames it. `None` when the source has
/// no copy. A non-`Matches` answer means the bind proceeds as-is and the page shows a problem.
#[must_use]
pub(crate) fn bind_fit(source: &UnassignedClean, entry: &PageCleanEntry) -> Option<CleanPageFit> {
    let probe = source.effective()?;
    Some(classify_clean_fit(probe.size.clone(), || entry.page_size.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_models::clean_assign::{CleanFileLocation, CleanFileProbe};
    use std::ffi::OsString;

    fn probe(path: &str, location: CleanFileLocation, size: Result<[u32; 2], String>) -> CleanFileProbe {
        CleanFileProbe { path: PathBuf::from(path), location, size }
    }

    fn entry(committed: Option<CleanFileProbe>, staged: Option<CleanFileProbe>, page_size: Result<[u32; 2], String>) -> PageCleanEntry {
        PageCleanEntry { page_idx: 0, page_size, committed, staged }
    }

    const VIRTUAL: ModelPageClean = ModelPageClean { materialized: false, size: None };

    /// A page index beyond the model's page count reads as virtual, not as "the model owns it".
    #[test]
    fn model_snapshot_treats_out_of_range_pages_as_virtual() {
        let mut model = CleanOverlaysModel::new_from_pages(&[PathBuf::from("001.png")]);
        assert!(!model.is_overlay_virtual_absent(1), "the model's own answer out of range (why the guard exists)");
        assert_eq!(ModelPageClean::of(&model, 1), VIRTUAL);
        assert_eq!(ModelPageClean::of(&model, 0), VIRTUAL);
        model.replace_from_rgba(0, image::RgbaImage::new(2, 3));
        assert_eq!(ModelPageClean::of(&model, 0), ModelPageClean { materialized: true, size: Some([2, 3]) });
    }

    #[test]
    fn link_state_table() {
        let committed_ok = || Some(probe("c/001.png", CleanFileLocation::Committed, Ok([10, 20])));
        let staged_bad = || Some(probe("u/001.png", CleanFileLocation::Unsaved, Ok([5, 5])));
        let ok_file = |path: &str| PageCleanLink::Ok { thumb: CleanThumbSource::File(PathBuf::from(path)), file_name: "001.png".into(), size: Some([10, 20]) };
        let cases: Vec<(&str, Option<PageCleanEntry>, ModelPageClean, PageCleanLink)> = vec![
            ("no scan, virtual", None, VIRTUAL, PageCleanLink::None),
            ("nothing on disk", Some(entry(None, None, Ok([10, 20]))), VIRTUAL, PageCleanLink::None),
            ("committed binds", Some(entry(committed_ok(), None, Ok([10, 20]))), VIRTUAL, ok_file("c/001.png")),
            (
                "staged mismatch shadows a fitting committed file",
                Some(entry(committed_ok(), staged_bad(), Ok([10, 20]))),
                VIRTUAL,
                PageCleanLink::Problem { problem: CleanLinkProblem::SizeMismatch { clean: [5, 5], page: [10, 20] }, file: PathBuf::from("u/001.png"), size: Some([5, 5]) },
            ),
            (
                "unreadable clean",
                Some(entry(Some(probe("c/001.png", CleanFileLocation::Committed, Err("bad".into()))), None, Ok([10, 20]))),
                VIRTUAL,
                PageCleanLink::Problem { problem: CleanLinkProblem::CleanUnreadable("bad".into()), file: PathBuf::from("c/001.png"), size: None },
            ),
            (
                "unreadable page",
                Some(entry(committed_ok(), None, Err("page".into()))),
                VIRTUAL,
                PageCleanLink::Problem { problem: CleanLinkProblem::PageUnreadable("page".into()), file: PathBuf::from("c/001.png"), size: Some([10, 20]) },
            ),
            (
                "materialized model wins over a problem file",
                Some(entry(None, staged_bad(), Ok([10, 20]))),
                ModelPageClean { materialized: true, size: Some([10, 20]) },
                PageCleanLink::Ok { thumb: CleanThumbSource::Model, file_name: "001.png".into(), size: Some([10, 20]) },
            ),
            (
                "materialized model without a known size",
                None,
                ModelPageClean { materialized: true, size: None },
                PageCleanLink::Ok { thumb: CleanThumbSource::Model, file_name: "001.png".into(), size: None },
            ),
        ];
        for (name, entry, model, expected) in cases {
            assert_eq!(page_clean_link(entry.as_ref(), model, "001.png"), expected, "{name}");
        }
    }

    #[test]
    fn bind_targets_split_by_presence_of_any_clean() {
        let problem = PageCleanLink::Problem { problem: CleanLinkProblem::CleanUnreadable(String::new()), file: PathBuf::new(), size: None };
        let ok = PageCleanLink::Ok { thumb: CleanThumbSource::Model, file_name: String::new(), size: None };
        let targets = bind_targets(&[PageCleanLink::None, ok, problem, PageCleanLink::None]);
        assert_eq!(targets, BindTargets { without_clean: vec![0, 3], with_clean: vec![1, 2] });
    }

    #[test]
    fn bind_fit_uses_the_surviving_copy_and_the_exact_rule() {
        let source = UnassignedClean {
            file_name: OsString::from("x.png"),
            committed: Some(probe("c/x.png", CleanFileLocation::Committed, Ok([10, 20]))),
            staged: Some(probe("u/x.png", CleanFileLocation::Unsaved, Ok([4, 4]))),
        };
        assert_eq!(bind_fit(&source, &entry(None, None, Ok([4, 4]))), Some(CleanPageFit::Matches { size: [4, 4] }));
        assert_eq!(bind_fit(&source, &entry(None, None, Ok([10, 20]))), Some(CleanPageFit::SizeMismatch { clean: [4, 4], page: [10, 20] }));
        let empty = UnassignedClean { file_name: OsString::from("y.png"), committed: None, staged: None };
        assert_eq!(bind_fit(&empty, &entry(None, None, Ok([4, 4]))), None);
    }
}
