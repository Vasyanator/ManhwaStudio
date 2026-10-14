/*
File: src/shell/target.rs

Purpose:
What the main window is asked to open, before anything is opened: the routing target shared by
startup routing (`src/lib.rs`), the launcher exit (`screen.rs`), external delivery
(`incoming.rs`) and the studio open worker (`studio/open.rs`). Pure data plus the one blocking
helper that releases a target's Android inbox batches.

Key structures:
- `StartupTarget`: a chapter directory or an ordered image set; also derives the studio window
  title.
- `ImageSources`: an ordered image set before it is opened: the files, the files a deliverer
  already rejected (`SkippedSource`), and the Android inbox batches backing the files.

Key functions:
- `received_count(target)`: how many images a delivered target holds (for log and question texts).
- `release_inbox_batches_blocking(batches)`: releases Android inbox batch folders (workers only).

Notes:
Native-only like the rest of the shell host: `StartupTarget::window_title` uses
`crate::studio::session::image_set::image_window_title`, which is not compiled on wasm.
*/

use crate::project::SkippedSource;
use std::path::{Path, PathBuf};

/// An ordered set of picture files to open as ONE image-set session, before it is opened.
///
/// Invariant (enforced by [`ImageSources::new`]): `files` and `skipped` are not both empty.
/// `files` may be empty when every delivered document was already rejected (the open then fails
/// with the list of skipped files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSources {
    /// Readable paths in page order (as received; the launcher sorts its pick first).
    files: Vec<PathBuf>,
    /// Documents the deliverer could not turn into a file (e.g. a failed Android copy), listed
    /// before the files the open itself skips.
    skipped: Vec<SkippedSource>,
    /// Android inbox batch folders holding `files`; released with the session
    /// (`StudioSession::release`). Empty for desktop paths.
    inbox_batches: Vec<PathBuf>,
}

impl ImageSources {
    /// A set over `files` (page order), the already-rejected `skipped` documents and the inbox
    /// batch folders backing the files. `None` when `files` and `skipped` are both empty: there
    /// is nothing to open or to report.
    #[must_use]
    pub fn new(files: Vec<PathBuf>, skipped: Vec<SkippedSource>, inbox_batches: Vec<PathBuf>) -> Option<Self> {
        if files.is_empty() && skipped.is_empty() {
            return None;
        }
        Some(Self { files, skipped, inbox_batches })
    }

    /// The paths to open, in page order.
    #[must_use]
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// The documents rejected before the open, in delivery order.
    #[must_use]
    pub fn skipped(&self) -> &[SkippedSource] {
        &self.skipped
    }

    /// The inbox batch folders the files live in (Android; empty elsewhere).
    #[must_use]
    pub fn inbox_batches(&self) -> &[PathBuf] {
        &self.inbox_batches
    }

    /// This set followed by `later`: files, skipped documents and inbox batches are each
    /// concatenated in order, so pages delivered later come after the earlier ones.
    #[must_use]
    pub fn merged(mut self, later: Self) -> Self {
        self.files.extend(later.files);
        self.skipped.extend(later.skipped);
        self.inbox_batches.extend(later.inbox_batches);
        self
    }

    /// The path that names the set in the window title: the first file, else the first skipped
    /// document (a set of rejected documents only).
    pub(super) fn title_path(&self) -> Option<&Path> {
        self.files.first().map(PathBuf::as_path).or_else(|| self.skipped.first().map(|skipped| skipped.path.as_path()))
    }
}

/// What the user asked the studio to open, before any scratch session exists: the output of
/// startup routing (CLI) or of the launcher, turned into a `StudioSession` by the open
/// worker (`studio/open.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupTarget {
    /// A chapter directory (`--project` or a launcher pick).
    Project(PathBuf),
    /// An ordered image set to edit in image-set (single-image) mode (`--image`, positional
    /// paths, launcher multi-pick). One file is the classic single-image mode.
    Images(ImageSources),
}

impl StartupTarget {
    /// Studio window title: `ManhwaStudio v{version} - {chapter path}` for a project; for an
    /// image set the one owner `crate::studio::session::image_set::image_window_title` (first file name, plus
    /// the count of the others). `version` is the display version (`MS_APP_VERSION`).
    #[must_use]
    pub fn window_title(&self, version: &str) -> String {
        match self {
            Self::Project(project_dir) => format!("ManhwaStudio v{version} - {}", project_dir.display()),
            Self::Images(sources) => match sources.title_path() {
                Some(first) => crate::studio::session::image_set::image_window_title(version, first, sources.files().len().max(1)),
                // Unreachable: `ImageSources::new` refuses a set with neither files nor skips.
                None => format!("ManhwaStudio v{version}"),
            },
        }
    }

    /// `true` for an image-set target (a session that never reloads from disk).
    #[must_use]
    pub fn is_image_set(&self) -> bool {
        match self {
            Self::Project(_) => false,
            Self::Images(_) => true,
        }
    }
}

/// How many images a delivered target holds (files plus documents already rejected), for the
/// "close and open N images?" texts. A project counts as one.
#[must_use]
pub(super) fn received_count(target: &StartupTarget) -> usize {
    match target {
        StartupTarget::Project(_) => 1,
        StartupTarget::Images(sources) => sources.files().len() + sources.skipped().len(),
    }
}

/// Releases Android inbox batch folders, in order (`ms_platform::incoming::release_batch_blocking`:
/// blocking JNI + file I/O, so worker threads only). A failure is logged by the platform layer;
/// leftovers are swept at the next process start.
pub(super) fn release_inbox_batches_blocking(batches: &[PathBuf]) {
    for batch in batches {
        ms_platform::incoming::release_batch_blocking(batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image-set target over `files` (no skips, no inbox).
    fn images(files: &[&str]) -> StartupTarget {
        StartupTarget::Images(ImageSources::new(files.iter().map(PathBuf::from).collect(), Vec::new(), Vec::new()).expect("non-empty set"))
    }

    fn skipped(path: &str) -> SkippedSource {
        SkippedSource { path: PathBuf::from(path), message: format!("cannot open {path}") }
    }

    #[test]
    fn image_sources_need_a_file_or_a_skip_and_merge_in_order() {
        assert_eq!(ImageSources::new(Vec::new(), Vec::new(), Vec::new()), None);
        let only_skipped = ImageSources::new(Vec::new(), vec![skipped("/home/u/x.heic")], Vec::new()).expect("a skip alone is a set");
        assert!(only_skipped.files().is_empty());
        assert_eq!(StartupTarget::Images(only_skipped.clone()).window_title("1.2.3"), "ManhwaStudio v1.2.3 - x.heic");

        let first = ImageSources::new(vec![PathBuf::from("/in/1/b.png"), PathBuf::from("/in/1/a.png")], vec![skipped("/in/1/z.heic")], vec![PathBuf::from("/in/1")]).expect("set");
        let later = ImageSources::new(vec![PathBuf::from("/in/2/c.png")], Vec::new(), vec![PathBuf::from("/in/2")]).expect("set");
        let merged = first.merged(later).merged(only_skipped);
        assert_eq!(merged.files(), [PathBuf::from("/in/1/b.png"), PathBuf::from("/in/1/a.png"), PathBuf::from("/in/2/c.png")].as_slice());
        assert_eq!(merged.skipped(), [skipped("/in/1/z.heic"), skipped("/home/u/x.heic")].as_slice());
        assert_eq!(merged.inbox_batches(), [PathBuf::from("/in/1"), PathBuf::from("/in/2")].as_slice());
    }

    #[test]
    fn several_images_name_the_first_file_and_count_the_others() {
        let one = images(&["/home/u/b.png"]).window_title("1.2.3");
        let three = images(&["/home/u/b.png", "/home/u/a.jpg", "/home/u/c.webp"]).window_title("1.2.3");
        assert_eq!(one, "ManhwaStudio v1.2.3 - b.png");
        // The suffix text is a localized template (`single_image.window_title_more`); the prefix
        // is the fixed program name, and N files must not read like one.
        assert!(three.starts_with("ManhwaStudio v1.2.3 - "), "{three}");
        assert_ne!(three, one);
    }
}
