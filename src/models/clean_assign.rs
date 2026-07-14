/*
FILE OVERVIEW: src/models/clean_assign.rs
GUI-free worker API for inspecting and managing clean-overlay files.

Main items:
- `scan_orphan_cleans`: discovers unassigned, mismatched, and unreadable clean images.
- `attach_fit` / `load_clean_for_attach`: validate and prepare an image for a page.
- `trash_clean_file`: discards staging files or preserves committed files in chapter trash.

Threading:
Every public operation that reads or writes files is synchronous and must run outside the GUI
thread. The pure `attach_fit` helper is safe to call anywhere.
*/

// This worker API is intentionally landed before its page-manager UI consumer.
#![allow(dead_code)]

use crate::project::{Page, ProjectPaths};
use crate::runtime_log;
use image::imageops::FilterType;
use image::RgbaImage;
use std::collections::HashMap;
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
    /// No source page has the same file stem.
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

/// Describes whether an image can be attached without distortion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachFit {
    /// Dimensions already match exactly.
    Exact,
    /// Dimensions differ, but relative aspect-ratio error is at most 1%.
    ScaleSameAspect,
}

/// Scans committed and staging clean folders for unassigned or invalid files.
///
/// This performs synchronous directory and image-header I/O and must run on a worker thread.
/// Detector masks named `{index:05}_mask.png` are service artifacts and are excluded.
#[must_use]
pub fn scan_orphan_cleans(paths: &ProjectPaths, pages: &[Page]) -> Vec<OrphanClean> {
    let pages_by_stem: HashMap<&str, &Page> = pages
        .iter()
        .filter_map(|page| page.path.file_stem()?.to_str().map(|stem| (stem, page)))
        .collect();
    let page_sizes: HashMap<usize, Result<[u32; 2], String>> = pages
        .iter()
        .map(|page| {
            let size = image::image_dimensions(&page.path)
                .map(|(width, height)| [width, height])
                .map_err(|err| format!("could not read source page '{}': {err}", page.path.display()));
            (page.idx, size)
        })
        .collect();
    let mut orphans = Vec::new();
    scan_clean_dir(
        &paths.clean_layers_dir,
        CleanFileLocation::Committed,
        &pages_by_stem,
        &page_sizes,
        &mut orphans,
    );
    scan_clean_dir(
        &paths.unsaved_clean_layers_dir,
        CleanFileLocation::Unsaved,
        &pages_by_stem,
        &page_sizes,
        &mut orphans,
    );
    orphans.sort_by(|left, right| left.path.cmp(&right.path));
    orphans
}

fn scan_clean_dir(
    dir: &Path,
    location: CleanFileLocation,
    pages_by_stem: &HashMap<&str, &Page>,
    page_sizes: &HashMap<usize, Result<[u32; 2], String>>,
    orphans: &mut Vec<OrphanClean>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[clean-assign] could not scan clean directory '{}': {err}",
                dir.display()
            ));
            return;
        }
    };
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
        if !path.is_file() || is_service_mask(&path) {
            continue;
        }
        let size = match image::image_dimensions(&path) {
            Ok((width, height)) => [width, height],
            Err(err) => {
                orphans.push(OrphanClean {
                    path,
                    location,
                    size: [0, 0],
                    reason: OrphanReason::Unreadable { error: err.to_string() },
                });
                continue;
            }
        };
        let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
            orphans.push(OrphanClean {
                path,
                location,
                size,
                reason: OrphanReason::NoMatchingPage,
            });
            continue;
        };
        let Some(page) = pages_by_stem.get(stem) else {
            orphans.push(OrphanClean {
                path,
                location,
                size,
                reason: OrphanReason::NoMatchingPage,
            });
            continue;
        };
        match page_sizes.get(&page.idx) {
            Some(Ok(page_size)) if *page_size == size => {}
            Some(Ok(page_size)) => orphans.push(OrphanClean {
                path,
                location,
                size,
                reason: OrphanReason::SizeMismatch {
                    page_idx: page.idx,
                    page_size: *page_size,
                },
            }),
            Some(Err(error)) => orphans.push(OrphanClean {
                path,
                location,
                size,
                reason: OrphanReason::Unreadable {
                    error: error.clone(),
                },
            }),
            None => orphans.push(OrphanClean {
                path,
                location,
                size,
                reason: OrphanReason::NoMatchingPage,
            }),
        }
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
            project_dir: project_dir.clone(), title_dir: title_dir.clone(), notes_file: project_dir.join("notes.json"),
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
}
