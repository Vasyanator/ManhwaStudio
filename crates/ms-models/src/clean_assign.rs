/*
FILE OVERVIEW: crates/ms-models/src/clean_assign.rs
GUI-free worker API for inspecting and managing clean-overlay files: the I/O half of the single
owner of "which file is page N's clean, and does it fit" (the pure rule lives in
`ms_page_ops::clean_binding` and is re-exported here).

Main items:
- `PageCleanPaths` / `probe_page_clean` / `PageCleanResolution`: one page's clean on disk in a
  `CleanTreeScope` (the overlay loader uses `LOADER_CLEAN_SCOPE` = `StagedOverCommitted`, the
  save-merge's view; every consumer that must agree with the canvas names that constant).
- `scan_clean_inventory` -> `CleanInventory` (`PageCleanEntry` per page + `UnassignedClean`):
  both trees scanned once; `PageCleanEntry::resolve` applies the same rule without I/O. Found
  names are compared as the filesystem resolves them (`clean_binding::clean_name_key`).
- `scan_orphan_cleans` / `orphans_from_inventory`: the orphan list (unassigned, mismatched and
  unreadable clean images) projected from the inventory.
- `attach_fit` / `load_clean_for_attach`: OPERATION policy for attaching a picked file (1% aspect
  tolerance, rescales) - deliberately not part of the binding rule.
- `trash_clean_file`: discards staging files or preserves committed files in chapter trash.
- `DETACHED_CLEAN_SUFFIX` / `is_detached_clean_file`: naming convention for deliberately detached cleans.
- `detached_clean_name` / `allocate_detached_clean_path`: collision-safe `<stem>[_<n>]_detached.png`
  names, free in BOTH trees and never a page's canonical name (compared by `clean_name_key`).
- `write_new_clean_png` / `move_clean_file` / `delete_unassigned_clean` (`CleanFileOpError`): the
  page manager's immediate clean-file operations. They never replace an existing file (temp +
  fsync + re-checked rename; check-then-rename relies on the caller's single serial worker), stay
  inside the two managed clean folders, and delete an unassigned clean from BOTH trees.

Threading:
Every public operation that reads or writes files is synchronous and must run outside the GUI
thread. The pure helpers (`attach_fit`, `PageCleanPaths` constructors, `PageCleanEntry::resolve`,
`orphans_from_inventory`) are safe to call anywhere.
*/

use ms_project::{Page, ProjectPaths};
use ms_log::runtime_log;
use image::imageops::FilterType;
use image::RgbaImage;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Identifies which persistence tree contains an orphan clean file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanFileLocation {
    /// The saved chapter tree.
    Committed,
    /// The disposable `_unsaved` staging tree.
    Unsaved,
}

/// Explains why a clean file cannot currently be attached by its stem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanReason {
    /// The file is not the canonical clean name (`<stem>.png`) of any page.
    NoMatchingPage,
    /// A page has the same stem, but its source dimensions differ.
    SizeMismatch {
        /// Index of the page selected by the stem.
        page_idx: usize,
        /// Header dimensions of that source page.
        page_size: [u32; 2],
    },
    /// The clean file header could not be decoded; `size` is `[0, 0]`.
    Unreadable {
        /// Diagnostic suitable for structured logging or a UI details view.
        error: String,
    },
}

/// A clean image that is not safely assigned to a source page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanClean {
    pub path: PathBuf,
    pub location: CleanFileLocation,
    /// Header-only dimensions, or `[0, 0]` for [`OrphanReason::Unreadable`].
    pub size: [u32; 2],
    pub reason: OrphanReason,
}

/// File-stem suffix that marks a clean image as deliberately detached from its page
/// (`003_detached.png`). Such a file is kept on purpose, so passive "unassigned file" warnings
/// must skip it; the page manager's orphan list still shows it so it can be re-attached.
pub const DETACHED_CLEAN_SUFFIX: &str = "_detached";

/// Returns whether `path`'s file stem ends with [`DETACHED_CLEAN_SUFFIX`] (ASCII case-insensitive).
/// Pure; never touches the filesystem.
#[must_use]
pub fn is_detached_clean_file(path: &Path) -> bool {
    path.file_stem().and_then(|stem| stem.to_str()).is_some_and(|stem| {
        stem.len() >= DETACHED_CLEAN_SUFFIX.len()
            && stem.is_char_boundary(stem.len() - DETACHED_CLEAN_SUFFIX.len())
            && stem[stem.len() - DETACHED_CLEAN_SUFFIX.len()..].eq_ignore_ascii_case(DETACHED_CLEAN_SUFFIX)
    })
}

/// Describes whether an image can be attached without distortion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachFit {
    /// Dimensions already match exactly.
    Exact,
    /// Dimensions differ, but relative aspect-ratio error is at most 1%.
    ScaleSameAspect,
}

// The binding rule itself (canonical `<stem>.png` name, exact-size fit) is owned by
// `ms_page_ops::clean_binding`; re-exported so crates above `ms-models` (the canvas autosave,
// the tabs) reach it without a direct `ms-page-ops` dependency.
pub use ms_page_ops::clean_binding::{
    classify_clean_fit, clean_overlay_file_name, clean_overlay_stem, page_clean_stem, writer_clean_stem, CleanPageFit,
};
use ms_page_ops::clean_binding::{clean_name_key, CLEAN_NAMES_CASE_INSENSITIVE};

/// Which clean tree(s) a per-page resolution looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanTreeScope {
    /// Only the committed `clean_layers/<stem>.png`: what a saved chapter holds on its own. No
    /// production consumer yet (the resolver tests pin its contract); remove it if the page-manager
    /// clean cards do not adopt it.
    CommittedOnly,
    /// The staged `_unsaved/clean_layers/<stem>.png` when present, else the committed one: what the
    /// committed tree will hold after the next save-merge, and what the overlay loader reads.
    StagedOverCommitted,
}

/// The scope the overlay loader resolves a page's clean in. Every consumer that must agree with
/// what the canvas shows (the loader in `src/app.rs`, the Cleaning tab's status messages, the
/// typing export's disk fallback) names this constant instead of restating the variant.
pub const LOADER_CLEAN_SCOPE: CleanTreeScope = CleanTreeScope::StagedOverCommitted;

/// The two persistence paths that may back one page's clean layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCleanPaths {
    /// `clean_layers/<stem>.png` in the committed chapter tree.
    pub committed: PathBuf,
    /// `_unsaved/clean_layers/<stem>.png` in the staging tree.
    pub staged: PathBuf,
}

impl PageCleanPaths {
    /// Paths keyed by the page's raw (`OsStr`) file stem; `None` only when the page path has no
    /// file stem at all. Pure.
    #[must_use]
    pub fn for_page(paths: &ProjectPaths, page: &Page) -> Option<Self> {
        let name = ms_page_ops::clean_binding::clean_overlay_os_file_name(page.path.file_stem()?);
        Some(Self {
            committed: paths.clean_layers_dir.join(&name),
            staged: paths.unsaved_clean_layers_dir.join(name),
        })
    }

    /// Paths of the file the clean model writes for `page` ([`writer_clean_stem`], including its
    /// non-UTF-8 fallback stem), i.e. the file the overlay loader reads back. Pure.
    #[must_use]
    pub fn for_writer(paths: &ProjectPaths, page: &Page) -> Self {
        let name = clean_overlay_file_name(writer_clean_stem(&page.path));
        Self {
            committed: paths.clean_layers_dir.join(&name),
            staged: paths.unsaved_clean_layers_dir.join(name),
        }
    }
}

/// One clean file found (or probed) in one clean tree, with its header size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanFileProbe {
    pub path: PathBuf,
    pub location: CleanFileLocation,
    /// Header-only `[width, height]`, or the header read error.
    pub size: Result<[u32; 2], String>,
}

/// Which file is a page's clean layer in one [`CleanTreeScope`], and whether it fits the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageCleanResolution {
    /// No canonical clean file exists in the scope.
    Absent,
    /// The canonical file binds: its size equals the page's.
    Bound { file: PathBuf, size: [u32; 2] },
    /// The canonical file exists but its size differs from the page's (`[width, height]`).
    SizeMismatch { file: PathBuf, clean: [u32; 2], page: [u32; 2] },
    /// The canonical file's header could not be read.
    CleanUnreadable { file: PathBuf, error: String },
    /// The canonical file is readable but the page's header could not be read.
    PageUnreadable { file: PathBuf, error: String },
}

impl PageCleanResolution {
    /// The file the overlay loader must attempt to decode, if any: a bound file, and also an
    /// unreadable clean or an unreadable page (decode reports its own detailed failure, and an
    /// unreadable page must not hide an otherwise decodable clean). `None` for `Absent` and
    /// `SizeMismatch`.
    #[must_use]
    pub fn loadable_file(&self) -> Option<&Path> {
        match self {
            Self::Bound { file, .. } | Self::CleanUnreadable { file, .. } | Self::PageUnreadable { file, .. } => Some(file),
            Self::Absent | Self::SizeMismatch { .. } => None,
        }
    }
}

/// Maps the binding owner's fit verdict for `file` onto a [`PageCleanResolution`].
fn resolution_from_fit(file: &Path, clean: Result<[u32; 2], String>, page: impl FnOnce() -> Result<[u32; 2], String>) -> PageCleanResolution {
    let file = file.to_path_buf();
    match classify_clean_fit(clean, page) {
        CleanPageFit::Matches { size } => PageCleanResolution::Bound { file, size },
        CleanPageFit::SizeMismatch { clean, page } => PageCleanResolution::SizeMismatch { file, clean, page },
        CleanPageFit::CleanUnreadable(error) => PageCleanResolution::CleanUnreadable { file, error },
        CleanPageFit::PageUnreadable { error, .. } => PageCleanResolution::PageUnreadable { file, error },
    }
}

/// Header size of an image file as `[width, height]`, or the reader's error text.
fn header_size(path: &Path) -> Result<[u32; 2], String> {
    image::image_dimensions(path)
        .map(|(width, height)| [width, height])
        .map_err(|err| err.to_string())
}

/// Header size of a SOURCE PAGE as `[width, height]`, or a diagnostic that names the page.
fn page_header_size(page_path: &Path) -> Result<[u32; 2], String> {
    image::image_dimensions(page_path)
        .map(|(width, height)| [width, height])
        .map_err(|err| format!("could not read source page '{}': {err}", page_path.display()))
}

/// Resolves page `page_path`'s clean layer at `clean_paths` in `scope` by probing the disk.
///
/// Synchronous stat + image-header I/O: worker threads only. Reads only what the scope needs, in
/// this order: `is_file` of the scoped candidate(s) (`CommittedOnly` never touches the staged
/// path), the clean header, and the page header only when the clean header was readable.
#[must_use]
pub fn probe_page_clean(clean_paths: &PageCleanPaths, page_path: &Path, scope: CleanTreeScope) -> PageCleanResolution {
    let file = match scope {
        CleanTreeScope::CommittedOnly => Some(&clean_paths.committed).filter(|path| path.is_file()),
        CleanTreeScope::StagedOverCommitted => Some(&clean_paths.staged)
            .filter(|path| path.is_file())
            .or_else(|| Some(&clean_paths.committed).filter(|path| path.is_file())),
    };
    let Some(file) = file else {
        return PageCleanResolution::Absent;
    };
    resolution_from_fit(file, header_size(file), || page_header_size(page_path))
}

/// One page's canonical clean files as found by [`scan_clean_inventory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCleanEntry {
    pub page_idx: usize,
    /// Header size of the source page, or a diagnostic naming the page.
    pub page_size: Result<[u32; 2], String>,
    /// The committed canonical file, when it exists.
    pub committed: Option<CleanFileProbe>,
    /// The staged canonical file, when it exists.
    pub staged: Option<CleanFileProbe>,
}

impl PageCleanEntry {
    /// Resolves this page's clean in `scope` from the scanned headers (no I/O), with the same
    /// rule and precedence as [`probe_page_clean`].
    #[must_use]
    pub fn resolve(&self, scope: CleanTreeScope) -> PageCleanResolution {
        let probe = match scope {
            CleanTreeScope::CommittedOnly => self.committed.as_ref(),
            CleanTreeScope::StagedOverCommitted => self.staged.as_ref().or(self.committed.as_ref()),
        };
        let Some(probe) = probe else {
            return PageCleanResolution::Absent;
        };
        resolution_from_fit(&probe.path, probe.size.clone(), || self.page_size.clone())
    }
}

/// A clean file whose name is not the canonical clean name of any page, merged across both trees
/// by file name (a staged file shadows a committed one of the same name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnassignedClean {
    pub file_name: OsString,
    pub committed: Option<CleanFileProbe>,
    pub staged: Option<CleanFileProbe>,
}

impl UnassignedClean {
    /// The copy that will survive a save-merge: the staged one when present, else the committed
    /// one. `None` never occurs for values built by [`scan_clean_inventory`].
    #[must_use]
    pub fn effective(&self) -> Option<&CleanFileProbe> {
        self.staged.as_ref().or(self.committed.as_ref())
    }
}

/// Everything in both clean trees, split into per-page canonical files and the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanInventory {
    /// One entry per page, in the order of the `pages` slice given to the scan.
    pub pages: Vec<PageCleanEntry>,
    /// Non-canonical files, sorted by file name.
    pub unassigned: Vec<UnassignedClean>,
}

/// Scans both clean trees into a [`CleanInventory`].
///
/// Synchronous directory and image-header I/O: worker threads only. Reads every page header and
/// every clean file header exactly once. Skips non-files and detector service masks
/// (`{index:05}_mask.png`). A file whose name is exactly a page's canonical clean name
/// ([`clean_overlay_file_name`] of its UTF-8 stem) is recorded on EVERY page with that stem;
/// everything else goes to `unassigned`. Names are compared through
/// `clean_binding::clean_name_key` with `CLEAN_NAMES_CASE_INSENSITIVE`, so on Windows a
/// `004.PNG` — which the loader opens as `004.png` — is page 004's clean, not an orphan.
/// Directory errors are logged and the tree is treated as empty.
#[must_use]
pub fn scan_clean_inventory(paths: &ProjectPaths, pages: &[Page]) -> CleanInventory {
    scan_clean_inventory_with(paths, pages, CLEAN_NAMES_CASE_INSENSITIVE)
}

/// [`scan_clean_inventory`] with the name-case rule given explicitly, so the case-insensitive
/// (Windows) matching is testable on every platform.
fn scan_clean_inventory_with(paths: &ProjectPaths, pages: &[Page], case_insensitive: bool) -> CleanInventory {
    let mut entries: Vec<PageCleanEntry> = pages
        .iter()
        .map(|page| PageCleanEntry {
            page_idx: page.idx,
            page_size: page_header_size(&page.path),
            committed: None,
            staged: None,
        })
        .collect();
    // Canonical name key -> positions in `entries` (several pages may share a stem, and on a
    // case-insensitive filesystem several stems may share a key).
    let mut canonical: HashMap<String, Vec<usize>> = HashMap::new();
    for (position, page) in pages.iter().enumerate() {
        if let Some(stem) = page_clean_stem(&page.path) {
            let key = clean_name_key(&clean_overlay_file_name(stem), case_insensitive).into_owned();
            canonical.entry(key).or_default().push(position);
        }
    }
    let mut unassigned: BTreeMap<OsString, UnassignedClean> = BTreeMap::new();
    for (dir, location) in [
        (&paths.clean_layers_dir, CleanFileLocation::Committed),
        (&paths.unsaved_clean_layers_dir, CleanFileLocation::Unsaved),
    ] {
        for path in list_clean_files(dir) {
            let Some(file_name) = path.file_name().map(OsStr::to_os_string) else {
                continue;
            };
            let probe = CleanFileProbe { size: header_size(&path), path, location };
            if let Some(positions) = file_name.to_str().and_then(|name| canonical.get(clean_name_key(name, case_insensitive).as_ref())) {
                for &position in positions {
                    let slot = match location {
                        CleanFileLocation::Committed => &mut entries[position].committed,
                        CleanFileLocation::Unsaved => &mut entries[position].staged,
                    };
                    *slot = Some(probe.clone());
                }
                continue;
            }
            let item = unassigned.entry(file_name.clone()).or_insert_with(|| UnassignedClean {
                file_name,
                committed: None,
                staged: None,
            });
            match location {
                CleanFileLocation::Committed => item.committed = Some(probe),
                CleanFileLocation::Unsaved => item.staged = Some(probe),
            }
        }
    }
    CleanInventory { pages: entries, unassigned: unassigned.into_values().collect() }
}

/// Lists the regular files of one clean tree, excluding detector service masks and crash-leftover
/// temp files of the clean writers ([`temp_sibling`]). A missing
/// directory yields nothing; any other listing error is logged and yields what was read.
fn list_clean_files(dir: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            runtime_log::log_warn(format!(
                "[clean-assign] could not scan clean directory '{}': {err}",
                dir.display()
            ));
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[clean-assign] could not read an entry in '{}': {err}",
                    dir.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        // A `.{name}.{pid}.tmp` is a crash leftover of `write_bytes_no_replace`, not a clean.
        if path.is_file() && !is_service_mask(&path) && !ms_docstore::is_temp_artifact(&path) {
            files.push(path);
        }
    }
    files
}

/// Scans committed and staging clean folders for unassigned or invalid files.
///
/// This performs synchronous directory and image-header I/O and must run on a worker thread.
/// Detector masks named `{index:05}_mask.png` are service artifacts and are excluded.
/// Equivalent to [`orphans_from_inventory`] over [`scan_clean_inventory`].
#[must_use]
pub fn scan_orphan_cleans(paths: &ProjectPaths, pages: &[Page]) -> Vec<OrphanClean> {
    orphans_from_inventory(&scan_clean_inventory(paths, pages))
}

/// Projects an inventory onto the orphan list: every scanned file (each tree separately) that
/// does not bind, sorted by path. Pure.
///
/// A canonical file is reported under the LAST page sharing its stem. A non-canonical file (any
/// name other than a page's `<stem>.png`, including a same-stem file of another extension such as
/// `001.webp`) belongs to no page: it is `NoMatchingPage`, or `Unreadable` when its header cannot
/// be read. An unreadable clean is `Unreadable` with size `[0, 0]`; a readable canonical clean
/// whose page is unreadable is `Unreadable` with the clean's size.
#[must_use]
pub fn orphans_from_inventory(inventory: &CleanInventory) -> Vec<OrphanClean> {
    let mut orphans = Vec::new();
    let mut reported: HashSet<&Path> = HashSet::new();
    // Reverse order: a canonical file shared by several same-stem pages is judged against the
    // LAST of them, exactly once.
    for entry in inventory.pages.iter().rev() {
        for probe in [&entry.committed, &entry.staged].into_iter().flatten() {
            if reported.insert(probe.path.as_path()) {
                orphans.extend(orphan_for_probe(probe, Some((entry.page_idx, &entry.page_size))));
            }
        }
    }
    for item in &inventory.unassigned {
        for probe in [&item.committed, &item.staged].into_iter().flatten() {
            orphans.extend(orphan_for_probe(probe, None));
        }
    }
    orphans.sort_by(|left, right| left.path.cmp(&right.path));
    orphans
}

/// The orphan report for one probed file judged against `page` (`(page_idx, page header size)`),
/// or `None` when the file binds to that page.
fn orphan_for_probe(probe: &CleanFileProbe, page: Option<(usize, &Result<[u32; 2], String>)>) -> Option<OrphanClean> {
    let orphan = |size, reason| Some(OrphanClean { path: probe.path.clone(), location: probe.location, size, reason });
    let Some((page_idx, page_size)) = page else {
        return match &probe.size {
            Ok(size) => orphan(*size, OrphanReason::NoMatchingPage),
            Err(error) => orphan([0, 0], OrphanReason::Unreadable { error: error.clone() }),
        };
    };
    match classify_clean_fit(probe.size.clone(), || page_size.clone()) {
        CleanPageFit::Matches { .. } => None,
        CleanPageFit::SizeMismatch { clean, page } => orphan(clean, OrphanReason::SizeMismatch { page_idx, page_size: page }),
        CleanPageFit::CleanUnreadable(error) => orphan([0, 0], OrphanReason::Unreadable { error }),
        CleanPageFit::PageUnreadable { clean, error } => orphan(clean, OrphanReason::Unreadable { error }),
    }
}

fn is_service_mask(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some(index) = stem.strip_suffix("_mask") else {
        return false;
    };
    path.extension().and_then(|value| value.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
        && index.len() == 5
        && index.bytes().all(|byte| byte.is_ascii_digit())
}

/// Returns the distortion-free attachment mode, using a 1% relative aspect tolerance.
///
/// The comparison is EXACT integer arithmetic (u128 cross-multiplication, no floating
/// point), and the tolerance is normalized on the PAGE aspect ratio:
/// `|orphan_w/orphan_h - page_w/page_h| / (page_w/page_h) <= 1%`, i.e.
/// `|orphan_w*page_h - page_w*orphan_h| * 100 <= page_w*orphan_h`. Exactly 1% passes.
/// Because the tolerance is page-normalized, the function is intentionally asymmetric
/// near the boundary: `attach_fit(a, b)` and `attach_fit(b, a)` may disagree for
/// ratios about 1% apart (covered by a unit test).
#[must_use]
pub fn attach_fit(orphan_size: [u32; 2], page_size: [u32; 2]) -> Option<AttachFit> {
    if orphan_size.contains(&0) || page_size.contains(&0) {
        return None;
    }
    if orphan_size == page_size {
        return Some(AttachFit::Exact);
    }
    // Cross-multiplied form of the page-normalized relative error above; u128 keeps
    // `u32 * u32 * 100` exact with no overflow.
    let orphan_cross = u128::from(orphan_size[0]) * u128::from(page_size[1]);
    let page_cross = u128::from(page_size[0]) * u128::from(orphan_size[1]);
    (orphan_cross.abs_diff(page_cross) * 100 <= page_cross).then_some(AttachFit::ScaleSameAspect)
}

/// Decodes an image and prepares it for attachment to a page.
///
/// This performs synchronous decode/resize work and must run on a worker thread. Exact images are
/// returned unchanged; same-aspect images are resized with Lanczos3. Distorting and zero-sized
/// attachments are rejected.
pub fn load_clean_for_attach(path: &Path, page_size: [u32; 2]) -> Result<RgbaImage, String> {
    let image = image::open(path)
        .map_err(|err| format!("could not decode clean image '{}': {err}", path.display()))?
        .to_rgba8();
    match attach_fit([image.width(), image.height()], page_size) {
        Some(AttachFit::Exact) => Ok(image),
        Some(AttachFit::ScaleSameAspect) => Ok(image::imageops::resize(
            &image,
            page_size[0],
            page_size[1],
            FilterType::Lanczos3,
        )),
        None => Err(format!(
            "clean image '{}' size {}x{} does not fit page {}x{}",
            path.display(), image.width(), image.height(), page_size[0], page_size[1]
        )),
    }
}

/// Typed failure of [`trash_clean_file`]. Unless a variant says otherwise, no
/// filesystem change has happened when it is returned.
#[derive(Debug, thiserror::Error)]
pub enum TrashCleanError {
    /// The input path is not strictly inside either managed clean folder after
    /// lexical component validation (absolute escapes and any `..`/root/prefix
    /// re-anchoring in the relative part are rejected). Nothing was touched.
    #[error("clean file '{path}' is outside the managed clean folders")]
    OutsideManagedRoots { path: PathBuf },
    /// Deleting a staged (unsaved) clean file failed.
    #[error("could not delete staged clean '{path}': {source}")]
    RemoveStaged {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A committed clean file did not resolve under the title tree, so its
    /// trash-relative layout cannot be derived. Nothing was touched.
    #[error("clean file '{path}' is outside the title tree")]
    OutsideTitleTree { path: PathBuf },
    /// The system clock reported a time before the Unix epoch.
    #[error("system clock is before the Unix epoch")]
    ClockBeforeEpoch,
    /// No free trash destination could be derived for the file. Nothing was touched.
    #[error("could not allocate a trash destination for '{path}'")]
    TrashDestinationUnavailable { path: PathBuf },
    /// Creating the `.pageop_trash` destination directory failed.
    #[error("could not create clean trash directory '{path}': {source}")]
    CreateTrashDir {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Moving the committed file into trash failed.
    #[error("could not move clean '{path}' to trash '{destination}': {source}")]
    MoveToTrash {
        path: PathBuf,
        destination: PathBuf,
        source: std::io::Error,
    },
}

/// Returns the part of `path` relative to `root` when the path stays strictly
/// inside `root` under lexical component rules: the relative part must be
/// non-empty and every component a plain name. A `..`, root, or prefix
/// component re-anchors the path outside the managed folder and yields `None`
/// (`Path::starts_with` alone would accept `root/../../victim`).
fn managed_relative<'p>(path: &'p Path, root: &Path) -> Option<&'p Path> {
    let relative = path.strip_prefix(root).ok()?;
    let mut components = relative.components().peekable();
    components.peek()?;
    components
        .all(|component| matches!(component, Component::Normal(_)))
        .then_some(relative)
}

/// Removes a clean file without permanently deleting committed chapter data.
///
/// This performs synchronous filesystem I/O and must run on a worker thread. Files inside the
/// disposable unsaved clean folder are deleted outright. Committed files are moved into
/// `{chapter}/.pageop_trash/{millis}/`, preserving their title-relative tree layout.
///
/// Security contract: `path` must resolve strictly inside one of the two managed clean
/// folders under lexical component validation (see [`managed_relative`]); any `..`/root
/// re-anchoring or a path outside both roots is rejected with
/// [`TrashCleanError::OutsideManagedRoots`] before any filesystem access.
///
/// # Errors
/// Returns a [`TrashCleanError`] describing the exact failure; validation errors are
/// returned without touching the filesystem.
pub fn trash_clean_file(paths: &ProjectPaths, path: &Path) -> Result<(), TrashCleanError> {
    if managed_relative(path, &paths.unsaved_clean_layers_dir).is_some() {
        return fs::remove_file(path).map_err(|source| TrashCleanError::RemoveStaged {
            path: path.to_path_buf(),
            source,
        });
    }
    if managed_relative(path, &paths.clean_layers_dir).is_none() {
        return Err(TrashCleanError::OutsideManagedRoots {
            path: path.to_path_buf(),
        });
    }
    let relative =
        path.strip_prefix(&paths.title_dir)
            .map_err(|_| TrashCleanError::OutsideTitleTree {
                path: path.to_path_buf(),
            })?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TrashCleanError::ClockBeforeEpoch)?
        .as_millis();
    let trash_parent = paths.project_dir.join(".pageop_trash");
    // `find` on this effectively unbounded suffix range always yields a free slot in
    // practice; the fallback error keeps the code total without a panic path.
    let destination = (0_u32..)
        .map(|suffix| {
            let id = if suffix == 0 {
                millis.to_string()
            } else {
                format!("{millis}-{suffix}")
            };
            trash_parent.join(id).join(relative)
        })
        .find(|candidate| !candidate.exists())
        .ok_or_else(|| TrashCleanError::TrashDestinationUnavailable {
            path: path.to_path_buf(),
        })?;
    let parent =
        destination
            .parent()
            .ok_or_else(|| TrashCleanError::TrashDestinationUnavailable {
                path: path.to_path_buf(),
            })?;
    fs::create_dir_all(parent).map_err(|source| TrashCleanError::CreateTrashDir {
        path: parent.to_path_buf(),
        source,
    })?;
    fs::rename(path, &destination).map_err(|source| TrashCleanError::MoveToTrash {
        path: path.to_path_buf(),
        destination: destination.clone(),
        source,
    })
}

/// Upper bound on the detached-name counter `n` tried by [`allocate_detached_clean_path`]. A
/// chapter never holds this many detached cleans of one page; the bound keeps the search total.
pub const MAX_DETACHED_CLEAN_NAMES: u32 = 10_000;

/// The file name a clean detached from the page with stem `page_stem` is stored under, for
/// collision counter `n`: `<stem>_detached.png` for `n == 0`, `<stem>_<n>_detached.png` otherwise.
/// The stem always ends with [`DETACHED_CLEAN_SUFFIX`], so [`is_detached_clean_file`] holds for
/// every result. Pure.
#[must_use]
pub fn detached_clean_name(page_stem: &str, n: u32) -> String {
    if n == 0 {
        clean_overlay_file_name(&format!("{page_stem}{DETACHED_CLEAN_SUFFIX}"))
    } else {
        clean_overlay_file_name(&format!("{page_stem}_{n}{DETACHED_CLEAN_SUFFIX}"))
    }
}

/// Whether `file_name` is the canonical clean name of any page in `pages`, compared the way the
/// filesystem resolves names (`clean_name_key` with `CLEAN_NAMES_CASE_INSENSITIVE`). A file with
/// such a name BINDS to that page, so it must never be treated as an unassigned clean. Pure.
#[must_use]
pub fn is_canonical_clean_name(pages: &[Page], file_name: &OsStr) -> bool {
    canonical_name_keys(pages, CLEAN_NAMES_CASE_INSENSITIVE).contains(&name_key_of(file_name, CLEAN_NAMES_CASE_INSENSITIVE))
}

/// Keys of every page's canonical clean name (UTF-8 stems only, like the inventory scan).
fn canonical_name_keys(pages: &[Page], case_insensitive: bool) -> HashSet<String> {
    pages
        .iter()
        .filter_map(|page| page_clean_stem(&page.path))
        .map(|stem| clean_name_key(&clean_overlay_file_name(stem), case_insensitive).into_owned())
        .collect()
}

/// The comparison key of a found file name; a non-UTF-8 name keys by its lossy form (it can never
/// equal a canonical name, which is built from a UTF-8 stem).
fn name_key_of(file_name: &OsStr, case_insensitive: bool) -> String {
    let name = file_name.to_string_lossy();
    clean_name_key(&name, case_insensitive).into_owned()
}

/// Typed failure of the clean-file operations below ([`allocate_detached_clean_path`],
/// [`write_new_clean_png`], [`move_clean_file`], [`delete_unassigned_clean`]). Unless a variant
/// says otherwise, the destination was not created and the source (if any) is untouched.
#[derive(Debug, thiserror::Error)]
pub enum CleanFileOpError {
    /// Every detached name up to [`MAX_DETACHED_CLEAN_NAMES`] is taken for this page stem.
    #[error("no free detached clean name for page stem '{page_stem}'")]
    NoFreeName { page_stem: String },
    /// A path is not strictly inside one of the two managed clean folders. Nothing was touched.
    #[error("clean file '{path}' is outside the managed clean folders")]
    OutsideManagedRoots { path: PathBuf },
    /// A clean tree exists but could not be listed, so a free name cannot be proven.
    #[error("could not list clean directory '{path}': {source}")]
    ListDir { path: PathBuf, source: std::io::Error },
    /// The named file is a page's canonical clean name (a BOUND clean), or not one plain file-name
    /// component, so it is not an unassigned clean. Nothing was touched.
    #[error("'{file_name}' is not an unassigned clean file")]
    NotUnassigned { file_name: String },
    /// The destination already exists; the operation never replaces a file.
    #[error("clean destination '{path}' already exists")]
    DestinationExists { path: PathBuf },
    /// The source file of a move does not exist.
    #[error("clean source '{path}' does not exist")]
    SourceMissing { path: PathBuf },
    /// Creating the destination directory failed.
    #[error("could not create clean directory '{path}': {source}")]
    CreateDir { path: PathBuf, source: std::io::Error },
    /// PNG encoding of the pixels failed (before any file was created).
    #[error("could not encode clean '{path}' as PNG: {source}")]
    Encode { path: PathBuf, source: image::ImageError },
    /// Writing or syncing the temporary file failed; the temporary file was removed.
    #[error("could not write clean temp file '{path}': {source}")]
    Write { path: PathBuf, source: std::io::Error },
    /// The final rename (or the cross-device copy standing in for it) failed; the source is
    /// untouched and any temporary file was removed.
    #[error("could not move clean '{from}' to '{to}': {source}")]
    Rename { from: PathBuf, to: PathBuf, source: std::io::Error },
    /// A cross-device move COPIED the file to `to`, but removing the source failed: the clean now
    /// exists at both paths.
    #[error("clean copied to '{to}' but source '{from}' could not be removed: {source}")]
    SourceNotRemoved { from: PathBuf, to: PathBuf, source: std::io::Error },
    /// Deleting an unassigned clean left at least one of its copies in place. Each entry names
    /// the copy and why it could not be trashed; copies not listed were trashed.
    #[error("could not trash every copy of the unassigned clean: {}", format_trash_failures(.failures))]
    TrashIncomplete { failures: Vec<(PathBuf, TrashCleanError)> },
}

/// Joins trash failures for [`CleanFileOpError::TrashIncomplete`]'s message.
fn format_trash_failures(failures: &[(PathBuf, TrashCleanError)]) -> String {
    failures.iter().map(|(path, error)| format!("'{}': {error}", path.display())).collect::<Vec<_>>().join("; ")
}

/// Whether `path` stays strictly inside one of the two managed clean folders.
fn is_managed_clean_path(paths: &ProjectPaths, path: &Path) -> bool {
    managed_relative(path, &paths.clean_layers_dir).is_some() || managed_relative(path, &paths.unsaved_clean_layers_dir).is_some()
}

/// Name keys of every entry (files and anything else) in `dir`; a missing directory is empty.
///
/// # Errors
/// Any listing error other than `NotFound` is returned, because treating an unreadable tree as
/// empty could hand out a name that is already taken.
fn existing_name_keys(dir: &Path, case_insensitive: bool) -> std::io::Result<HashSet<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => return Err(err),
    };
    let mut keys = HashSet::new();
    for entry in entries {
        keys.insert(name_key_of(&entry?.file_name(), case_insensitive));
    }
    Ok(keys)
}

/// Picks the committed-tree path a clean detached from the page with stem `page_stem` is stored
/// at: the first [`detached_clean_name`] (`n = 0, 1, …`) that is free in BOTH clean trees and is
/// not the canonical clean name of any page in `pages`, compared by `clean_name_key` (so on
/// Windows `003_detached.PNG` occupies `003_detached.png`).
///
/// Synchronous directory listing: worker threads only. The answer is advisory — the writers
/// ([`write_new_clean_png`], [`move_clean_file`]) re-check the destination and never replace.
///
/// # Errors
/// `NoFreeName` when all [`MAX_DETACHED_CLEAN_NAMES`] names are taken; `ListDir` when a clean
/// tree exists but cannot be listed.
pub fn allocate_detached_clean_path(paths: &ProjectPaths, pages: &[Page], page_stem: &str) -> Result<PathBuf, CleanFileOpError> {
    allocate_detached_clean_path_with(paths, pages, page_stem, CLEAN_NAMES_CASE_INSENSITIVE)
}

/// [`allocate_detached_clean_path`] with the name-case rule explicit (testable on any platform).
fn allocate_detached_clean_path_with(paths: &ProjectPaths, pages: &[Page], page_stem: &str, case_insensitive: bool) -> Result<PathBuf, CleanFileOpError> {
    let mut taken = canonical_name_keys(pages, case_insensitive);
    for dir in [&paths.clean_layers_dir, &paths.unsaved_clean_layers_dir] {
        let keys = existing_name_keys(dir, case_insensitive).map_err(|source| CleanFileOpError::ListDir { path: dir.clone(), source })?;
        taken.extend(keys);
    }
    (0..MAX_DETACHED_CLEAN_NAMES)
        .map(|n| detached_clean_name(page_stem, n))
        .find(|name| !taken.contains(clean_name_key(name, case_insensitive).as_ref()))
        .map(|name| paths.clean_layers_dir.join(name))
        .ok_or_else(|| CleanFileOpError::NoFreeName { page_stem: page_stem.to_string() })
}

/// Whether anything (file, directory, dangling symlink) exists at `path`.
fn path_occupied(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Temporary sibling of `destination` used while a new file is written: `.{name}.{pid}.tmp` in the
/// same directory, so the final rename never crosses a filesystem. Delegates to
/// `ms_docstore::temp_path_for`, so a crash leftover is exactly what `ms_docstore::is_temp_artifact`
/// recognises and [`list_clean_files`] skips; the two cannot drift apart.
fn temp_sibling(destination: &Path) -> PathBuf {
    ms_docstore::temp_path_for(destination)
}

/// Writes `bytes` to a fresh temporary sibling of `destination`, fsyncs it, and renames it onto
/// `destination` after re-checking that `destination` is still free. Removes the temporary file on
/// every failure.
///
/// The check-then-rename is not atomic against a concurrent creator; it relies on the caller's
/// serialization (the page manager's single clean worker, with page operations and saves refused
/// while it runs).
fn write_bytes_no_replace(destination: &Path, bytes: &[u8]) -> Result<(), CleanFileOpError> {
    use std::io::Write as _;
    let temp = temp_sibling(destination);
    let write = || -> std::io::Result<()> {
        let mut file = fs::File::create_new(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    if let Err(source) = write() {
        remove_temp(&temp);
        return Err(CleanFileOpError::Write { path: temp, source });
    }
    if path_occupied(destination) {
        remove_temp(&temp);
        return Err(CleanFileOpError::DestinationExists { path: destination.to_path_buf() });
    }
    fs::rename(&temp, destination).map_err(|source| {
        remove_temp(&temp);
        CleanFileOpError::Rename { from: temp.clone(), to: destination.to_path_buf(), source }
    })
}

/// Best-effort removal of a temporary file after a failed write; a failure is logged (the file is
/// a hidden `.tmp` that no scan binds or lists, so leaving it is harmless beyond disk use).
fn remove_temp(temp: &Path) {
    if let Err(err) = fs::remove_file(temp)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        runtime_log::log_warn(format!("[clean-assign] could not remove temp file '{}': {err}", temp.display()));
    }
}

/// Creates `destination`'s parent directory when missing.
fn ensure_parent(destination: &Path) -> Result<(), CleanFileOpError> {
    let Some(parent) = destination.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|source| CleanFileOpError::CreateDir { path: parent.to_path_buf(), source })
}

/// Encodes `image` as PNG and writes it to the NEW file `destination` (inside a managed clean
/// folder) through a same-directory temporary file, fsync, and a re-checked no-replace rename.
///
/// Synchronous encode + I/O: worker threads only. Used to persist a detached clean's CURRENT model
/// pixels (unsaved edits included).
///
/// # Errors
/// `OutsideManagedRoots` / `DestinationExists` before anything is written; `Encode`, `CreateDir`,
/// `Write`, `Rename` otherwise. On any error `destination` does not exist and no temp file remains.
pub fn write_new_clean_png(paths: &ProjectPaths, destination: &Path, image: &RgbaImage) -> Result<(), CleanFileOpError> {
    if !is_managed_clean_path(paths, destination) {
        return Err(CleanFileOpError::OutsideManagedRoots { path: destination.to_path_buf() });
    }
    if path_occupied(destination) {
        return Err(CleanFileOpError::DestinationExists { path: destination.to_path_buf() });
    }
    let mut bytes = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|source| CleanFileOpError::Encode { path: destination.to_path_buf(), source })?;
    ensure_parent(destination)?;
    write_bytes_no_replace(destination, &bytes)
}

/// Moves the clean file `from` to the NEW path `to`, both inside the managed clean folders (within
/// one tree, or staged -> committed). Never replaces an existing `to`. A plain rename is used; when
/// the two trees live on different filesystems the file is copied (temp + fsync + no-replace
/// rename) and the source removed afterwards. File bytes are preserved exactly.
///
/// Synchronous I/O: worker threads only. Check-then-rename, relying on the caller's serialization
/// (see [`write_bytes_no_replace`]).
///
/// # Errors
/// `OutsideManagedRoots`, `SourceMissing`, `DestinationExists` before anything is touched;
/// `CreateDir`, `Rename`, `Write` with the source untouched; `SourceNotRemoved` when a cross-device
/// copy succeeded but the source could not be removed (the file then exists at both paths).
pub fn move_clean_file(paths: &ProjectPaths, from: &Path, to: &Path) -> Result<(), CleanFileOpError> {
    for path in [from, to] {
        if !is_managed_clean_path(paths, path) {
            return Err(CleanFileOpError::OutsideManagedRoots { path: path.to_path_buf() });
        }
    }
    if !from.is_file() {
        return Err(CleanFileOpError::SourceMissing { path: from.to_path_buf() });
    }
    if path_occupied(to) {
        return Err(CleanFileOpError::DestinationExists { path: to.to_path_buf() });
    }
    ensure_parent(to)?;
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            let bytes = fs::read(from).map_err(|source| CleanFileOpError::Rename { from: from.to_path_buf(), to: to.to_path_buf(), source })?;
            write_bytes_no_replace(to, &bytes)?;
            fs::remove_file(from).map_err(|source| CleanFileOpError::SourceNotRemoved { from: from.to_path_buf(), to: to.to_path_buf(), source })
        }
        Err(source) => Err(CleanFileOpError::Rename { from: from.to_path_buf(), to: to.to_path_buf(), source }),
    }
}

/// The two tree paths an unassigned clean named `file_name` may occupy, `(committed, staged)`, or
/// `None` when `file_name` is not a single plain path component (it would escape the folders).
#[must_use]
pub fn unassigned_clean_paths(paths: &ProjectPaths, file_name: &OsStr) -> Option<(PathBuf, PathBuf)> {
    let mut components = Path::new(file_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Some((paths.clean_layers_dir.join(file_name), paths.unsaved_clean_layers_dir.join(file_name))),
        _ => None,
    }
}

/// Deletes the unassigned clean named `file_name` from BOTH trees: the staged copy is removed and
/// the committed copy moved to `.pageop_trash` ([`trash_clean_file`]). Both copies must go, or the
/// surviving one resurfaces on the next scan. Copies that do not exist are skipped; if neither
/// exists the call succeeds (already gone).
///
/// Refuses a name that is a page's canonical clean name (`pages`), which is a bound clean, not an
/// unassigned one. Synchronous I/O: worker threads only.
///
/// # Errors
/// `NotUnassigned` for a name that is not one plain component or is canonical (nothing
/// touched); `TrashIncomplete` listing each copy that could not be trashed (the others were).
pub fn delete_unassigned_clean(paths: &ProjectPaths, pages: &[Page], file_name: &OsStr) -> Result<(), CleanFileOpError> {
    let Some((committed, staged)) = unassigned_clean_paths(paths, file_name).filter(|_| !is_canonical_clean_name(pages, file_name)) else {
        return Err(CleanFileOpError::NotUnassigned { file_name: file_name.to_string_lossy().into_owned() });
    };
    let failures: Vec<(PathBuf, TrashCleanError)> = [staged, committed]
        .into_iter()
        .filter(|path| path.is_file())
        .filter_map(|path| trash_clean_file(paths, &path).err().map(|error| (path, error)))
        .collect();
    if failures.is_empty() { Ok(()) } else { Err(CleanFileOpError::TrashIncomplete { failures }) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn write_image(path: &Path, size: [u32; 2]) -> Result<(), Box<dyn std::error::Error>> {
        RgbaImage::from_pixel(size[0], size[1], Rgba([1, 2, 3, 255])).save(path)?;
        Ok(())
    }

    fn paths(root: &Path) -> ProjectPaths {
        let title_dir = root.join("title");
        let project_dir = title_dir.join("chapter");
        let unsaved_dir = title_dir.join("chapter_unsaved");
        ProjectPaths {
            project_dir: project_dir.clone(), title_dir: title_dir.clone(), notes_file: project_dir.join("notes.json"), char_favorites_file: title_dir.join("char_favorites.json"), color_presets_file: title_dir.join("color_presets.json"),
            bubbles_file: project_dir.join("bubbles.json"), src_dir: project_dir.join("src"), clean_layers_dir: project_dir.join("clean_layers"),
            cleaned_dir: project_dir.join("cleaned"), alt_vers_dir: project_dir.join("alt_vers"), saved_dir: project_dir.join("saved"),
            image_bubbles_dir: project_dir.join("image_bubbles"), text_images_dir: project_dir.join("text_images"), layers_dir: project_dir.join("layers"),
            text_detection_dir: project_dir.join("text_detection"), characters_dir: title_dir.join("characters"), terms_file: title_dir.join("terms.json"),
            settings_file: project_dir.join("settings.json"), unsaved_dir: unsaved_dir.clone(), unsaved_bubbles_file: unsaved_dir.join("bubbles.json"),
            unsaved_clean_layers_dir: unsaved_dir.join("clean_layers"), unsaved_image_bubbles_dir: unsaved_dir.join("image_bubbles"),
            unsaved_text_images_dir: unsaved_dir.join("text_images"), unsaved_layers_dir: unsaved_dir.join("layers"),
        }
    }

    #[test]
    fn attach_fit_covers_tolerance_and_degenerate_sizes() {
        assert_eq!(attach_fit([100, 200], [100, 200]), Some(AttachFit::Exact));
        assert_eq!(attach_fit([100, 100], [201, 200]), Some(AttachFit::ScaleSameAspect));
        assert_eq!(attach_fit([100, 100], [202, 200]), Some(AttachFit::ScaleSameAspect));
        assert_eq!(attach_fit([100, 100], [203, 200]), None);
        assert_eq!(attach_fit([0, 100], [100, 100]), None);
    }

    #[test]
    fn attach_fit_boundary_is_exact_and_page_normalized() {
        // Exactly 1% relative to the page ratio passes (integer comparison, no
        // float rounding at the boundary)…
        assert_eq!(attach_fit([101, 100], [100, 100]), Some(AttachFit::ScaleSameAspect));
        assert_eq!(attach_fit([1010, 1000], [1000, 1000]), Some(AttachFit::ScaleSameAspect));
        // …and 1.1% is rejected.
        assert_eq!(attach_fit([1011, 1000], [1000, 1000]), None);
        // Documented semantics: the tolerance is normalized on the PAGE ratio, so
        // the comparison is intentionally asymmetric near the boundary (this pair's
        // ratios differ by ~1.0% of one side and ~1.01% of the other).
        assert_eq!(attach_fit([10101, 100], [100, 1]), None);
        assert_eq!(attach_fit([100, 1], [10101, 100]), Some(AttachFit::ScaleSameAspect));
    }

    #[test]
    fn scans_name_size_mask_unreadable_and_unsaved() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.src_dir)?;
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        let page_path = paths.src_dir.join("001.png");
        write_image(&page_path, [20, 10])?;
        write_image(&paths.clean_layers_dir.join("001.png"), [10, 10])?;
        write_image(&paths.clean_layers_dir.join("orphan.png"), [3, 4])?;
        write_image(&paths.clean_layers_dir.join("00001_mask.png"), [3, 4])?;
        fs::write(paths.clean_layers_dir.join("broken.png"), b"broken")?;
        write_image(&paths.unsaved_clean_layers_dir.join("staged.png"), [5, 6])?;
        let pages = [Page { idx: 0, path: page_path }];
        let found = scan_orphan_cleans(&paths, &pages);
        assert_eq!(found.len(), 4);
        assert!(found.iter().any(|item| matches!(item.reason, OrphanReason::SizeMismatch { page_idx: 0, page_size: [20, 10] })));
        assert!(found.iter().any(|item| item.path.ends_with("orphan.png") && item.reason == OrphanReason::NoMatchingPage));
        assert!(found.iter().any(|item| matches!(item.reason, OrphanReason::Unreadable { .. })));
        assert!(found.iter().any(|item| item.location == CleanFileLocation::Unsaved));
        Ok(())
    }

    /// A crash between the temp write and the rename of `write_bytes_no_replace` leaves its
    /// `.{name}.{pid}.tmp` sibling behind; the scan must not surface it as an unassigned clean, and
    /// the leftover must be exactly the name the writer produces (any pid).
    #[test]
    fn scan_skips_crash_leftover_temp_files() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.src_dir)?;
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        let page_path = paths.src_dir.join("001.png");
        write_image(&page_path, [10, 10])?;
        let own_leftover = temp_sibling(&paths.clean_layers_dir.join("001_detached.png"));
        fs::write(&own_leftover, b"partial")?;
        fs::write(paths.unsaved_clean_layers_dir.join(".001.png.4242.tmp"), b"partial")?;
        write_image(&paths.clean_layers_dir.join("orphan.png"), [3, 4])?;
        let pages = [Page { idx: 0, path: page_path }];
        let inventory = scan_clean_inventory(&paths, &pages);
        let names: Vec<&OsStr> = inventory.unassigned.iter().map(|item| item.file_name.as_os_str()).collect();
        assert_eq!(names, [OsStr::new("orphan.png")]);
        assert!(scan_orphan_cleans(&paths, &pages).iter().all(|item| item.path.ends_with("orphan.png")));
        Ok(())
    }

    /// On a case-insensitive filesystem (Windows) the loader opening `004.png` / `page1.png` opens
    /// `004.PNG` / `PAGE1.png`, so the scan must record those as the pages' cleans; on a
    /// case-sensitive one they are unassigned. The rule is exercised through the explicit
    /// parameter, so both halves run on every platform.
    #[test]
    fn scan_matches_canonical_names_by_platform_case_rule() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.src_dir)?;
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        write_image(&paths.src_dir.join("004.bmp"), [10, 10])?;
        write_image(&paths.src_dir.join("page1.bmp"), [10, 10])?;
        write_image(&paths.clean_layers_dir.join("004.PNG"), [10, 10])?;
        write_image(&paths.unsaved_clean_layers_dir.join("PAGE1.png"), [10, 10])?;
        write_image(&paths.clean_layers_dir.join("004.WEBP"), [10, 10])?;
        let pages = [
            Page { idx: 0, path: paths.src_dir.join("004.bmp") },
            Page { idx: 1, path: paths.src_dir.join("page1.bmp") },
        ];
        let unassigned_names = |inventory: &CleanInventory| -> Vec<OsString> {
            inventory.unassigned.iter().map(|item| item.file_name.clone()).collect()
        };

        let insensitive = scan_clean_inventory_with(&paths, &pages, true);
        assert_eq!(
            insensitive.pages[0].resolve(LOADER_CLEAN_SCOPE),
            PageCleanResolution::Bound { file: paths.clean_layers_dir.join("004.PNG"), size: [10, 10] }
        );
        assert_eq!(
            insensitive.pages[1].resolve(LOADER_CLEAN_SCOPE),
            PageCleanResolution::Bound { file: paths.unsaved_clean_layers_dir.join("PAGE1.png"), size: [10, 10] }
        );
        // Another extension stays unassigned whatever its case.
        assert_eq!(unassigned_names(&insensitive), vec![OsString::from("004.WEBP")]);

        let sensitive = scan_clean_inventory_with(&paths, &pages, false);
        assert_eq!(sensitive.pages[0].resolve(LOADER_CLEAN_SCOPE), PageCleanResolution::Absent);
        assert_eq!(sensitive.pages[1].resolve(LOADER_CLEAN_SCOPE), PageCleanResolution::Absent);
        assert_eq!(
            unassigned_names(&sensitive),
            ["004.PNG", "004.WEBP", "PAGE1.png"].map(OsString::from).to_vec()
        );
        Ok(())
    }

    /// Characterization fixture of `scan_orphan_cleans`: every binding case the scan
    /// distinguishes, in both trees. Returns the paths and the page list.
    fn characterization_fixture(root: &Path) -> Result<(ProjectPaths, Vec<Page>), Box<dyn std::error::Error>> {
        let paths = paths(root);
        fs::create_dir_all(&paths.src_dir)?;
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        let committed = &paths.clean_layers_dir;
        let staged = &paths.unsaved_clean_layers_dir;
        // Page 0: canonical committed clean of the exact size -> bound, not reported.
        write_image(&paths.src_dir.join("000.png"), [10, 10])?;
        write_image(&committed.join("000.png"), [10, 10])?;
        // Page 1: committed canonical clean of the wrong size, staged twin of the right size.
        write_image(&paths.src_dir.join("001.png"), [10, 10])?;
        write_image(&committed.join("001.png"), [12, 10])?;
        write_image(&staged.join("001.png"), [10, 10])?;
        // Page 2: unreadable source page with a readable canonical clean.
        fs::write(paths.src_dir.join("002.png"), b"not an image")?;
        write_image(&committed.join("002.png"), [5, 5])?;
        // Page 3: unreadable canonical clean.
        write_image(&paths.src_dir.join("003.png"), [10, 10])?;
        fs::write(committed.join("003.png"), b"broken clean")?;
        // Pages 4 and 5 share the stem "004"; the LAST page owns stem matches.
        write_image(&paths.src_dir.join("004.png"), [10, 10])?;
        write_image(&paths.src_dir.join("004.bmp"), [8, 8])?;
        write_image(&committed.join("004.png"), [10, 10])?;
        // Same stem, other extension: never a page's clean, whatever its size.
        write_image(&committed.join("000.webp"), [10, 10])?;
        write_image(&staged.join("003.webp"), [7, 7])?;
        // Service mask, deliberately detached clean, plain orphans in both trees, a sub-directory.
        write_image(&committed.join("00001_mask.png"), [3, 3])?;
        write_image(&committed.join("001_detached.png"), [10, 10])?;
        write_image(&staged.join("orphan.png"), [4, 4])?;
        // The same non-canonical name in BOTH trees: the orphan list reports each copy.
        write_image(&committed.join("orphan.png"), [9, 9])?;
        fs::write(staged.join("orphan_broken.png"), b"broken orphan")?;
        fs::create_dir_all(committed.join("nested.png"))?;
        let pages = vec![
            Page { idx: 0, path: paths.src_dir.join("000.png") },
            Page { idx: 1, path: paths.src_dir.join("001.png") },
            Page { idx: 2, path: paths.src_dir.join("002.png") },
            Page { idx: 3, path: paths.src_dir.join("003.png") },
            Page { idx: 4, path: paths.src_dir.join("004.png") },
            Page { idx: 5, path: paths.src_dir.join("004.bmp") },
        ];
        Ok((paths, pages))
    }

    /// Golden output of `scan_orphan_cleans`, captured against the pre-refactor scan and kept
    /// byte-identical through the binding-owner refactor, except for the deliberately dropped
    /// any-extension stem pairing (marked below).
    #[test]
    fn scan_orphan_cleans_characterization() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (paths, pages) = characterization_fixture(temp.path())?;
        let committed = &paths.clean_layers_dir;
        let staged = &paths.unsaved_clean_layers_dir;
        let header_error = |path: &Path| image::image_dimensions(path).err().map(|err| err.to_string()).unwrap_or_default();
        let page2 = paths.src_dir.join("002.png");
        let page2_error = format!("could not read source page '{}': {}", page2.display(), header_error(&page2));
        let mut expected = vec![
            OrphanClean {
                path: committed.join("001.png"),
                location: CleanFileLocation::Committed,
                size: [12, 10],
                reason: OrphanReason::SizeMismatch { page_idx: 1, page_size: [10, 10] },
            },
            OrphanClean {
                path: committed.join("001_detached.png"),
                location: CleanFileLocation::Committed,
                size: [10, 10],
                reason: OrphanReason::NoMatchingPage,
            },
            OrphanClean {
                path: committed.join("002.png"),
                location: CleanFileLocation::Committed,
                size: [5, 5],
                reason: OrphanReason::Unreadable { error: page2_error },
            },
            OrphanClean {
                path: committed.join("003.png"),
                location: CleanFileLocation::Committed,
                size: [0, 0],
                reason: OrphanReason::Unreadable { error: header_error(&committed.join("003.png")) },
            },
            OrphanClean {
                path: committed.join("004.png"),
                location: CleanFileLocation::Committed,
                size: [10, 10],
                reason: OrphanReason::SizeMismatch { page_idx: 5, page_size: [8, 8] },
            },
            // INTENDED CHANGE (user decision): the any-extension stem pairing was dropped, so a
            // same-stem non-`.png` file is a plain unassigned clean. Before, `000.webp` (exact
            // size) was not reported and `003.webp` was `SizeMismatch { page_idx: 3 }`.
            OrphanClean {
                path: committed.join("000.webp"),
                location: CleanFileLocation::Committed,
                size: [10, 10],
                reason: OrphanReason::NoMatchingPage,
            },
            OrphanClean {
                path: staged.join("003.webp"),
                location: CleanFileLocation::Unsaved,
                size: [7, 7],
                reason: OrphanReason::NoMatchingPage,
            },
            OrphanClean {
                path: committed.join("orphan.png"),
                location: CleanFileLocation::Committed,
                size: [9, 9],
                reason: OrphanReason::NoMatchingPage,
            },
            OrphanClean {
                path: staged.join("orphan.png"),
                location: CleanFileLocation::Unsaved,
                size: [4, 4],
                reason: OrphanReason::NoMatchingPage,
            },
            OrphanClean {
                path: staged.join("orphan_broken.png"),
                location: CleanFileLocation::Unsaved,
                size: [0, 0],
                reason: OrphanReason::Unreadable { error: header_error(&staged.join("orphan_broken.png")) },
            },
        ];
        expected.sort_by(|left, right| left.path.cmp(&right.path));
        assert_eq!(scan_orphan_cleans(&paths, &pages), expected);
        Ok(())
    }

    #[test]
    fn probe_page_clean_follows_loader_rule_in_both_scopes() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (paths, pages) = characterization_fixture(temp.path())?;
        let probe = |idx: usize, scope| {
            let page = &pages[idx];
            probe_page_clean(&PageCleanPaths::for_writer(&paths, page), &page.path, scope)
        };
        let committed = &paths.clean_layers_dir;
        let staged = &paths.unsaved_clean_layers_dir;
        use CleanTreeScope::{CommittedOnly, StagedOverCommitted};

        assert_eq!(probe(0, CommittedOnly), PageCleanResolution::Bound { file: committed.join("000.png"), size: [10, 10] });
        // Committed twin mismatched, staged twin fits: the scopes disagree.
        let mismatch = probe(1, CommittedOnly);
        assert_eq!(mismatch, PageCleanResolution::SizeMismatch { file: committed.join("001.png"), clean: [12, 10], page: [10, 10] });
        assert_eq!(mismatch.loadable_file(), None);
        assert_eq!(probe(1, StagedOverCommitted), PageCleanResolution::Bound { file: staged.join("001.png"), size: [10, 10] });
        // Unreadable page or clean: the loader still attempts the decode.
        let page_unreadable = probe(2, CommittedOnly);
        assert!(matches!(page_unreadable, PageCleanResolution::PageUnreadable { .. }));
        assert_eq!(page_unreadable.loadable_file(), Some(committed.join("002.png").as_path()));
        let clean_unreadable = probe(3, CommittedOnly);
        assert!(matches!(clean_unreadable, PageCleanResolution::CleanUnreadable { .. }));
        assert_eq!(clean_unreadable.loadable_file(), Some(committed.join("003.png").as_path()));
        // A same-stem `.webp` is never the page's clean.
        fs::remove_file(committed.join("000.png"))?;
        assert_eq!(probe(0, CommittedOnly), PageCleanResolution::Absent);
        assert_eq!(PageCleanResolution::Absent.loadable_file(), None);
        Ok(())
    }

    #[test]
    fn inventory_splits_canonical_and_unassigned_with_shadowing() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let (paths, pages) = characterization_fixture(temp.path())?;
        let inventory = scan_clean_inventory(&paths, &pages);
        assert_eq!(inventory.pages.len(), pages.len());
        // Entry-level resolution agrees with the disk probe.
        for (entry, page) in inventory.pages.iter().zip(&pages) {
            for scope in [CleanTreeScope::CommittedOnly, CleanTreeScope::StagedOverCommitted] {
                let clean_paths = PageCleanPaths::for_writer(&paths, page);
                let from_disk = probe_page_clean(&clean_paths, &page.path, scope);
                assert_eq!(from_disk, entry.resolve(scope));
            }
        }
        // Both same-stem pages see the canonical `004.png`.
        assert!(inventory.pages[4].committed.is_some() && inventory.pages[5].committed.is_some());
        let names: Vec<&OsStr> = inventory.unassigned.iter().map(|item| item.file_name.as_os_str()).collect();
        assert_eq!(names, ["000.webp", "001_detached.png", "003.webp", "orphan.png", "orphan_broken.png"].map(OsStr::new));
        // `orphan.png` exists in both trees and merges into one entry; staged shadows committed.
        let orphan = &inventory.unassigned[3];
        assert!(orphan.committed.is_some());
        assert_eq!(orphan.effective().map(|probe| probe.location), Some(CleanFileLocation::Unsaved));
        Ok(())
    }

    #[test]
    fn loads_exact_and_resized_clean() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("clean.png");
        write_image(&path, [20, 10])?;
        assert_eq!(load_clean_for_attach(&path, [20, 10])?.dimensions(), (20, 10));
        assert_eq!(load_clean_for_attach(&path, [40, 20])?.dimensions(), (40, 20));
        assert!(load_clean_for_attach(&path, [40, 40]).is_err());
        Ok(())
    }

    #[test]
    fn trash_preserves_committed_and_deletes_unsaved() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        let committed = paths.clean_layers_dir.join("lost.png");
        let unsaved = paths.unsaved_clean_layers_dir.join("staged.png");
        fs::write(&committed, b"saved")?;
        fs::write(&unsaved, b"staged")?;
        trash_clean_file(&paths, &committed)?;
        trash_clean_file(&paths, &unsaved)?;
        assert!(!committed.exists() && !unsaved.exists());
        let trash = fs::read_dir(paths.project_dir.join(".pageop_trash"))?
            .next()
            .ok_or("missing trash")??
            .path();
        assert_eq!(fs::read(trash.join("chapter/clean_layers/lost.png"))?, b"saved");
        Ok(())
    }

    #[test]
    fn trash_rejects_escaping_and_unmanaged_paths() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        let victim = paths.project_dir.join("victim.png");
        fs::write(&victim, b"victim")?;
        let external = temp.path().join("external.png");
        fs::write(&external, b"external")?;

        // Lexical `..` escape below a managed root: `Path::starts_with` would accept
        // it, but component validation must reject it before touching the FS.
        let escape = paths
            .unsaved_clean_layers_dir
            .join("..")
            .join("..")
            .join("chapter")
            .join("victim.png");
        assert!(matches!(
            trash_clean_file(&paths, &escape),
            Err(TrashCleanError::OutsideManagedRoots { .. })
        ));
        let committed_escape = paths.clean_layers_dir.join("..").join("victim.png");
        assert!(matches!(
            trash_clean_file(&paths, &committed_escape),
            Err(TrashCleanError::OutsideManagedRoots { .. })
        ));
        // Absolute path outside the project tree.
        assert!(matches!(
            trash_clean_file(&paths, &external),
            Err(TrashCleanError::OutsideManagedRoots { .. })
        ));
        // A managed-tree file outside both clean folders.
        assert!(matches!(
            trash_clean_file(&paths, &victim),
            Err(TrashCleanError::OutsideManagedRoots { .. })
        ));
        // The managed roots themselves (empty relative part) are rejected too.
        assert!(matches!(
            trash_clean_file(&paths, &paths.clean_layers_dir),
            Err(TrashCleanError::OutsideManagedRoots { .. })
        ));

        // Nothing was deleted, moved, or staged into trash.
        assert_eq!(fs::read(&victim)?, b"victim");
        assert_eq!(fs::read(&external)?, b"external");
        assert!(!paths.project_dir.join(".pageop_trash").exists());
        Ok(())
    }

    #[test]
    fn detached_names_always_carry_the_suffix() {
        assert_eq!(detached_clean_name("003", 0), "003_detached.png");
        assert_eq!(detached_clean_name("003", 1), "003_1_detached.png");
        assert_eq!(detached_clean_name("003", 42), "003_42_detached.png");
        for n in [0, 1, 7, MAX_DETACHED_CLEAN_NAMES - 1] {
            let name = detached_clean_name("page", n);
            assert!(is_detached_clean_file(Path::new(&name)), "{name}");
            assert_eq!(clean_overlay_stem(&name).map(|stem| stem.ends_with(DETACHED_CLEAN_SUFFIX)), Some(true));
        }
    }

    #[test]
    fn allocation_skips_names_taken_in_either_tree_and_page_names() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        let pages = vec![Page { idx: 0, path: paths.src_dir.join("003.png") }];
        assert_eq!(allocate_detached_clean_path_with(&paths, &pages, "003", false)?, paths.clean_layers_dir.join("003_detached.png"));
        fs::write(paths.clean_layers_dir.join("003_detached.png"), b"x")?;
        fs::write(paths.unsaved_clean_layers_dir.join("003_1_detached.png"), b"x")?;
        // A page literally named `003_2_detached` owns that canonical name: never hand it out.
        let pages = vec![pages[0].clone(), Page { idx: 1, path: paths.src_dir.join("003_2_detached.jpg") }];
        assert_eq!(allocate_detached_clean_path_with(&paths, &pages, "003", false)?, paths.clean_layers_dir.join("003_3_detached.png"));
        Ok(())
    }

    #[test]
    fn allocation_is_case_aware_under_the_insensitive_rule() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::write(paths.clean_layers_dir.join("003_DETACHED.PNG"), b"x")?;
        assert_eq!(allocate_detached_clean_path_with(&paths, &[], "003", true)?, paths.clean_layers_dir.join("003_1_detached.png"));
        assert_eq!(allocate_detached_clean_path_with(&paths, &[], "003", false)?, paths.clean_layers_dir.join("003_detached.png"));
        Ok(())
    }

    #[test]
    fn write_new_clean_png_never_replaces_and_leaves_no_temp() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        let target = paths.clean_layers_dir.join("003_detached.png");
        let image = RgbaImage::from_pixel(3, 2, Rgba([9, 8, 7, 128]));
        write_new_clean_png(&paths, &target, &image)?;
        assert_eq!(image::open(&target)?.to_rgba8(), image);
        assert!(matches!(write_new_clean_png(&paths, &target, &image), Err(CleanFileOpError::DestinationExists { .. })));
        let outside = temp.path().join("outside.png");
        assert!(matches!(write_new_clean_png(&paths, &outside, &image), Err(CleanFileOpError::OutsideManagedRoots { .. })));
        assert!(!outside.exists());
        let names: Vec<_> = fs::read_dir(&paths.clean_layers_dir)?.map(|entry| entry.map(|entry| entry.file_name())).collect::<Result<_, _>>()?;
        assert_eq!(names, vec![OsString::from("003_detached.png")], "no temp file may remain");
        Ok(())
    }

    #[test]
    fn move_clean_file_is_byte_exact_cross_tree_and_no_replace() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        let staged = paths.unsaved_clean_layers_dir.join("x.png");
        fs::write(&staged, b"exact bytes")?;
        let target = paths.clean_layers_dir.join("003.png");
        move_clean_file(&paths, &staged, &target)?;
        assert_eq!(fs::read(&target)?, b"exact bytes");
        assert!(!staged.exists());
        fs::write(&staged, b"other")?;
        assert!(matches!(move_clean_file(&paths, &staged, &target), Err(CleanFileOpError::DestinationExists { .. })));
        assert_eq!(fs::read(&staged)?, b"other", "source untouched on refusal");
        assert!(matches!(move_clean_file(&paths, &paths.unsaved_clean_layers_dir.join("missing.png"), &paths.clean_layers_dir.join("m.png")), Err(CleanFileOpError::SourceMissing { .. })));
        assert!(matches!(move_clean_file(&paths, &staged, &temp.path().join("escape.png")), Err(CleanFileOpError::OutsideManagedRoots { .. })));
        Ok(())
    }

    #[test]
    fn delete_unassigned_removes_both_copies_and_refuses_bound_names() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let paths = paths(temp.path());
        fs::create_dir_all(&paths.clean_layers_dir)?;
        fs::create_dir_all(&paths.unsaved_clean_layers_dir)?;
        fs::write(paths.clean_layers_dir.join("003_detached.png"), b"committed")?;
        fs::write(paths.unsaved_clean_layers_dir.join("003_detached.png"), b"staged")?;
        fs::write(paths.clean_layers_dir.join("003.png"), b"bound")?;
        let pages = vec![Page { idx: 0, path: paths.src_dir.join("003.png") }];
        delete_unassigned_clean(&paths, &pages, OsStr::new("003_detached.png"))?;
        assert!(!paths.clean_layers_dir.join("003_detached.png").exists());
        assert!(!paths.unsaved_clean_layers_dir.join("003_detached.png").exists());
        assert!(paths.project_dir.join(".pageop_trash").exists(), "the committed copy is preserved in trash");
        assert!(matches!(delete_unassigned_clean(&paths, &pages, OsStr::new("003.png")), Err(CleanFileOpError::NotUnassigned { .. })));
        assert!(matches!(delete_unassigned_clean(&paths, &pages, OsStr::new("../003.png")), Err(CleanFileOpError::NotUnassigned { .. })));
        assert_eq!(fs::read(paths.clean_layers_dir.join("003.png"))?, b"bound");
        // Already gone: success.
        delete_unassigned_clean(&paths, &pages, OsStr::new("003_detached.png"))?;
        Ok(())
    }
}
