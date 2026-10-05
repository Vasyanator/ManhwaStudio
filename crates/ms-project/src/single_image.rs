/*
File: crates/ms-project/src/single_image.rs

Purpose:
The storage side of the single-image editing mode (open one picture file, edit it in the
studio, write it back). The picture is never edited where it lies: it is decoded into a
hidden, throwaway scratch chapter, and the REGULAR `ProjectData::load` runs on that chapter,
so every tab, writer and loader keeps working against an ordinary chapter tree while the
user's file and folder stay untouched until an explicit Save / Save As (owned elsewhere).

Scratch layout (fixed ASCII names, never derived from the user's file name):
    <base>/<session-id>/                  session root: `.ms-single-image` marker, `.lock` (held)
    <base>/<session-id>/title/            ProjectPaths::title_dir (`settings.json` seeded here)
    <base>/<session-id>/title/chapter/src/000.png   the decoded, upright source

Key items:
- `SingleImageScratch`: one reserved session root. `reserve` creates it in the order
  create_dir -> create + exclusively lock `.lock` -> write marker, so a half-created session is
  never swept. The lock is held for the scratch's whole life. `Drop` does NO I/O (a crash or a
  plain drop leaves the directory for the next startup sweep); `remove` deletes it.
- `sweep_stale_sessions`: deletes every session root under a base that carries the marker and
  whose `.lock` can be acquired (its owner process is gone). Unmarked entries are never touched.
- `prepare_session`: source file -> format check against the readable-types table -> ICC /
  EXIF orientation / animation probe -> decode (default `image::Limits`) -> orientation baked
  in -> PNG `000.png` in the scratch chapter -> `comic_type = pages` seeded.
- `open_single_image`: `prepare_session` + `ProjectData::load` on the scratch chapter + the
  `SessionKind::SingleImage` session set on the result.
- `SingleImageError`: typed failure; `user_message()` is localized
  (`project.single_image.*_error`), `Display` is the technical text for logs.

Notes:
Native only (`cfg(not(target_arch = "wasm32"))` on the `mod` line): it needs the OS temp dir
and file locks. Like `project_scan.rs` it uses `std::fs` directly rather than the
`ms_storage` seam: the scratch and the user's file are real native paths, and the desktop
seam is a passthrough anyway. Every function here blocks on disk I/O and decoding: call it on
the load worker or after the window closed, never on the GUI thread. Pixels and ICC bytes are
never logged.
*/

use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::metadata::Orientation;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use ms_config::single_image::{INPUT_FILE_TYPES, SCRATCH_LOCK_FILE, SCRATCH_MARKER_FILE, input_extensions};
use ms_log::runtime_log;
use serde_json::Value;

use crate::{ComicType, ProjectData, SessionKind, SingleImageSession, SourceImageFormat};

/// Title directory name inside a session root.
const SCRATCH_TITLE_DIR: &str = "title";
/// Chapter directory name inside the scratch title.
const SCRATCH_CHAPTER_DIR: &str = "chapter";
/// File name of the one page inside the scratch chapter's source directory.
const SCRATCH_PAGE_FILE: &str = "000.png";
/// Session-root name attempts before `reserve` gives up on name collisions. A collision needs
/// the same pid, nanosecond timestamp and counter, so more than one retry means a broken clock
/// or a hostile directory; the bound keeps `reserve` from spinning.
const RESERVE_MAX_ATTEMPTS: u32 = 16;

/// Process-wide counter that disambiguates session ids created within one clock tick.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Failure of a single-image operation.
///
/// `Display` is the technical text for logs (paths, operation, OS error);
/// [`SingleImageError::user_message`] is the localized text for the user.
#[derive(Debug)]
pub enum SingleImageError {
    /// The source path does not exist.
    NotFound { path: PathBuf },
    /// The source path exists but is not a regular file (a directory, a socket, ...).
    NotAFile { path: PathBuf },
    /// The content is not one of the readable types (`ms_config::single_image::INPUT_FILE_TYPES`).
    /// `detected` names what was recognized, or `unknown`.
    UnsupportedFormat { path: PathBuf, detected: String },
    /// The file has a readable type but could not be decoded (damaged, truncated, over the
    /// decoder memory limits).
    Decode { path: PathBuf, reason: String },
    /// A filesystem operation failed. `op` is a short technical description.
    Io { op: &'static str, path: PathBuf, source: io::Error },
    /// The session lock file could not be created or locked.
    Lock { path: PathBuf, reason: String },
    /// The prepared scratch chapter did not load as a one-page project.
    ProjectLoad { reason: String },
}

impl SingleImageError {
    /// Localized message for the user: what failed and what to check. Technical detail stays
    /// in the `Display` text that callers log.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::NotFound { path } => tf!("project.single_image.not_found_error", path = path.display()),
            Self::NotAFile { path } => tf!("project.single_image.not_a_file_error", path = path.display()),
            Self::UnsupportedFormat { path, .. } => {
                let types = input_extensions().collect::<Vec<_>>().join(", ");
                tf!("project.single_image.unsupported_format_error", path = path.display(), types = types)
            }
            Self::Decode { path, .. } => tf!("project.single_image.decode_error", path = path.display()),
            Self::Io { path, .. } => tf!("project.single_image.io_error", path = path.display()),
            Self::Lock { path, .. } => tf!("project.single_image.lock_error", path = path.display()),
            Self::ProjectLoad { .. } => t!("project.single_image.project_load_error").to_string(),
        }
    }
}

impl fmt::Display for SingleImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => write!(f, "single image not found: {}", path.display()),
            Self::NotAFile { path } => write!(f, "single image path is not a regular file: {}", path.display()),
            Self::UnsupportedFormat { path, detected } => {
                write!(f, "unsupported single image format '{detected}': {}", path.display())
            }
            Self::Decode { path, reason } => write!(f, "failed to decode single image {}: {reason}", path.display()),
            Self::Io { op, path, source } => write!(f, "single image I/O failed ({op}) at {}: {source}", path.display()),
            Self::Lock { path, reason } => write!(f, "single image scratch lock failed at {}: {reason}", path.display()),
            Self::ProjectLoad { reason } => write!(f, "single image scratch chapter failed to load: {reason}"),
        }
    }
}

impl std::error::Error for SingleImageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::NotFound { .. }
            | Self::NotAFile { .. }
            | Self::UnsupportedFormat { .. }
            | Self::Decode { .. }
            | Self::Lock { .. }
            | Self::ProjectLoad { .. } => None,
        }
    }
}

/// Builds an [`SingleImageError::Io`].
fn io_error(op: &'static str, path: &Path, source: io::Error) -> SingleImageError {
    SingleImageError::Io { op, path: path.to_path_buf(), source }
}

/// One reserved scratch session root, alive while this value holds its `.lock`.
///
/// Dropping it performs NO I/O (the directory stays, its lock is released, and the next
/// startup sweep removes it); [`SingleImageScratch::remove`] deletes it explicitly.
#[derive(Debug)]
pub struct SingleImageScratch {
    root: PathBuf,
    /// Exclusively locked `.lock` handle; holding it is what marks the session as live.
    lock: File,
}

impl SingleImageScratch {
    /// Creates a new uniquely named session root `<base>/<pid>-<nanos>-<n>` (creating `base`
    /// as needed), locks its `.lock` file exclusively and writes the marker, in that order.
    /// Blocking but cheap (a few small file operations).
    ///
    /// # Errors
    /// `Io` when `base` or the session root cannot be created or the marker cannot be written;
    /// `Lock` when the lock file cannot be created or locked. A partly created root is removed
    /// again (a failure there is logged).
    pub fn reserve(base: &Path) -> Result<Self, SingleImageError> {
        std::fs::create_dir_all(base).map_err(|err| io_error("create scratch base", base, err))?;
        let root = create_unique_session_dir(base)?;
        match lock_and_mark(&root) {
            Ok(lock) => {
                runtime_log::log_info(format!("single image: reserved scratch session {}", root.display()));
                Ok(Self { root, lock })
            }
            Err(err) => {
                // The marker is written last, so a root that failed here carries no marker and
                // would never be swept: remove it now.
                if let Err(cleanup) = std::fs::remove_dir_all(&root) {
                    runtime_log::log_warn(format!(
                        "single image: could not remove half-created scratch {}: {cleanup}",
                        root.display()
                    ));
                }
                Err(err)
            }
        }
    }

    /// The session root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The scratch chapter directory (`<root>/title/chapter`), the directory handed to
    /// `ProjectData::load`. It exists only after [`prepare_session`].
    #[must_use]
    pub fn chapter_dir(&self) -> PathBuf {
        self.root.join(SCRATCH_TITLE_DIR).join(SCRATCH_CHAPTER_DIR)
    }

    /// Releases the lock and deletes the whole session root. Blocking recursive delete:
    /// NEVER call it on the GUI thread (the app calls it after its window has closed). A root
    /// that is already gone (a concurrent sweep raced the unlock) counts as success.
    ///
    /// # Errors
    /// `Io` when the directory exists but cannot be deleted; the next startup sweep retries.
    pub fn remove(self) -> Result<(), SingleImageError> {
        let Self { root, lock } = self;
        // Close the handle first: Windows cannot delete a file that is still open.
        drop(lock);
        match std::fs::remove_dir_all(&root) {
            Ok(()) => {
                runtime_log::log_info(format!("single image: removed scratch session {}", root.display()));
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_error("remove scratch session", &root, err)),
        }
    }
}

/// Creates a fresh session directory under `base`; retries on a name collision.
fn create_unique_session_dir(base: &Path) -> Result<PathBuf, SingleImageError> {
    let pid = std::process::id();
    let mut last_collision = base.to_path_buf();
    for _ in 0..RESERVE_MAX_ATTEMPTS {
        // A clock before the epoch only weakens uniqueness; the counter and `create_dir`'s
        // AlreadyExists check still keep two sessions apart.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let counter = SESSION_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let candidate = base.join(format!("{pid}-{nanos}-{counter}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => last_collision = candidate,
            Err(err) => return Err(io_error("create scratch session", &candidate, err)),
        }
    }
    Err(io_error(
        "create scratch session",
        &last_collision,
        io::Error::new(io::ErrorKind::AlreadyExists, "no free session name"),
    ))
}

/// Creates and exclusively locks `<root>/.lock`, then writes the marker. Returns the held
/// lock handle.
fn lock_and_mark(root: &Path) -> Result<File, SingleImageError> {
    let lock_path = root.join(SCRATCH_LOCK_FILE);
    let lock = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(|err| SingleImageError::Lock { path: lock_path.clone(), reason: err.to_string() })?;
    lock.try_lock().map_err(|err| SingleImageError::Lock { path: lock_path.clone(), reason: err.to_string() })?;
    let marker_path = root.join(SCRATCH_MARKER_FILE);
    std::fs::write(&marker_path, b"").map_err(|err| io_error("write scratch marker", &marker_path, err))?;
    Ok(lock)
}

/// Outcome of [`sweep_stale_sessions`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Stale session roots deleted.
    pub removed: usize,
    /// Marked session roots skipped because another live session holds their lock.
    pub live: usize,
    /// One technical line per entry that could not be inspected or deleted (also logged).
    pub errors: Vec<String>,
}

/// Deletes every stale session root under `base`: a directory that carries the marker file
/// and whose `.lock` can be acquired (its owner exited or crashed). Unmarked entries,
/// symlinks and plain files are never touched; a missing `base` is an empty report. A marked
/// root whose lock file is gone is a half-removed dead session and is deleted too. Blocking:
/// run it at startup before any window exists, or on a worker.
#[must_use]
pub fn sweep_stale_sessions(base: &Path) -> SweepReport {
    let mut report = SweepReport::default();
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return report,
        Err(err) => {
            push_sweep_error(&mut report, format!("read scratch base {}: {err}", base.display()));
            return report;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                push_sweep_error(&mut report, format!("read entry of {}: {err}", base.display()));
                continue;
            }
        };
        let root = entry.path();
        // `DirEntry::file_type` does not follow symlinks, so a link into a foreign tree is
        // never treated as a session root.
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => {}
            Ok(_) => continue,
            Err(err) => {
                push_sweep_error(&mut report, format!("stat {}: {err}", root.display()));
                continue;
            }
        }
        match std::fs::symlink_metadata(root.join(SCRATCH_MARKER_FILE)) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => {
                push_sweep_error(&mut report, format!("stat marker in {}: {err}", root.display()));
                continue;
            }
        }
        match session_is_stale(&root) {
            Ok(true) => match std::fs::remove_dir_all(&root) {
                Ok(()) => report.removed += 1,
                // Another instance swept it first.
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => push_sweep_error(&mut report, format!("remove {}: {err}", root.display())),
            },
            Ok(false) => report.live += 1,
            Err(err) => push_sweep_error(&mut report, format!("lock {}: {err}", root.display())),
        }
    }
    if report.removed > 0 {
        runtime_log::log_info(format!(
            "single image: swept {} stale scratch session(s) under {}",
            report.removed,
            base.display()
        ));
    }
    report
}

/// `true` when nobody holds the session's `.lock`. The probe handle is dropped before the
/// caller deletes the directory (Windows cannot delete an open file).
fn session_is_stale(root: &Path) -> io::Result<bool> {
    let lock_path = root.join(SCRATCH_LOCK_FILE);
    let lock = match OpenOptions::new().write(true).open(&lock_path) {
        Ok(lock) => lock,
        // Marker present but lock gone: a remove that was interrupted mid-way.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err),
    };
    match lock.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

/// Records and logs one sweep problem.
fn push_sweep_error(report: &mut SweepReport, message: String) {
    runtime_log::log_warn(format!("single image: scratch sweep: {message}"));
    report.errors.push(message);
}

/// Result of [`prepare_session`].
#[derive(Debug, Clone)]
pub struct PreparedSingleImage {
    /// The scratch chapter directory to hand to `ProjectData::load`.
    pub chapter_dir: PathBuf,
    /// Session facts to attach to the loaded `ProjectData`.
    pub session: Arc<SingleImageSession>,
    /// Width of the upright (orientation-applied) page, in pixels.
    pub width_px: u32,
    /// Height of the upright (orientation-applied) page, in pixels.
    pub height_px: u32,
}

/// Maps a recognized `image` format onto the readable single-image types, `None` for any
/// format outside `ms_config::single_image::INPUT_FILE_TYPES`.
fn source_format_of(format: ImageFormat) -> Option<SourceImageFormat> {
    match format {
        ImageFormat::Png => Some(SourceImageFormat::Png),
        ImageFormat::Jpeg => Some(SourceImageFormat::Jpeg),
        ImageFormat::WebP => Some(SourceImageFormat::WebP),
        ImageFormat::Gif => Some(SourceImageFormat::Gif),
        ImageFormat::Bmp => Some(SourceImageFormat::Bmp),
        ImageFormat::Tiff => Some(SourceImageFormat::Tiff),
        ImageFormat::Tga => Some(SourceImageFormat::Tga),
        ImageFormat::Qoi => Some(SourceImageFormat::Qoi),
        // `ImageFormat` is non-exhaustive; every other format (AVIF, PNM, ICO, ...) is
        // deliberately not an input type of this mode.
        _ => None,
    }
}

/// `true` when `extension` (any case) is listed for `format` in `INPUT_FILE_TYPES`.
fn extension_names_format(extension: Option<&str>, format: SourceImageFormat) -> bool {
    let Some(extension) = extension.map(str::to_ascii_lowercase) else {
        return false;
    };
    INPUT_FILE_TYPES
        .iter()
        .filter(|file_type| file_type.extensions.contains(&extension.as_str()))
        .filter_map(|file_type| file_type.extensions.first().copied().and_then(ImageFormat::from_extension))
        .any(|listed| source_format_of(listed) == Some(format))
}

/// `true` when the source holds more than one frame. Only the first frame is ever opened.
fn probe_animated(bytes: &[u8], format: SourceImageFormat, path: &Path) -> Result<bool, SingleImageError> {
    let decode_err = |err: image::ImageError| SingleImageError::Decode { path: path.to_path_buf(), reason: err.to_string() };
    match format {
        SourceImageFormat::Png => PngDecoder::new(Cursor::new(bytes)).and_then(|decoder| decoder.is_apng()).map_err(decode_err),
        SourceImageFormat::WebP => WebPDecoder::new(Cursor::new(bytes)).map(|decoder| decoder.has_animation()).map_err(decode_err),
        SourceImageFormat::Gif => {
            let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(decode_err)?;
            // Any second item counts, a failing one included: it proves there is more than
            // one frame, and treating it as animated only disables the in-place save.
            Ok(decoder.into_frames().take(2).count() > 1)
        }
        SourceImageFormat::Jpeg | SourceImageFormat::Bmp | SourceImageFormat::Tiff | SourceImageFormat::Tga | SourceImageFormat::Qoi => Ok(false),
    }
}

/// Decodes `source` into the scratch chapter of `scratch` and seeds its comic type.
///
/// The format is detected from the content (the extension decides only for TGA, which has no
/// signature) and must be a readable type. The ICC profile and EXIF orientation come from the
/// decoder; the orientation is baked into the pixels, which are reduced to RGBA8 and written
/// as `<chapter>/src/000.png`; `comic_type = pages` is written to the scratch title's
/// `settings.json`. The source file and its folder are only READ. Decoding uses the default
/// `image::Limits`. Call it once per freshly reserved scratch. Blocking: worker thread only.
///
/// # Errors
/// `NotFound` / `NotAFile` for a bad path, `UnsupportedFormat` for content outside the
/// readable types, `Decode` for a damaged or over-limit image, `Io` for filesystem failures
/// (including the comic-type seed).
pub fn prepare_session(source: &Path, scratch: &SingleImageScratch) -> Result<PreparedSingleImage, SingleImageError> {
    let source_path = std::path::absolute(source).map_err(|err| io_error("resolve source path", source, err))?;
    match std::fs::metadata(&source_path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(SingleImageError::NotAFile { path: source_path }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(SingleImageError::NotFound { path: source_path }),
        Err(err) => return Err(io_error("stat source image", &source_path, err)),
    }
    // One read serves the animation probe and the decode.
    let bytes = std::fs::read(&source_path).map_err(|err| io_error("read source image", &source_path, err))?;

    // The CONTENT signature decides the format. TGA is the one readable type without a
    // signature, so only there the extension decides; any other unrecognized content is
    // unsupported, whatever the file is named (a `.png` holding garbage is not "a PNG").
    let extension = source_path.extension().and_then(|ext| ext.to_str());
    let content_format = ImageReader::new(Cursor::new(bytes.as_slice()))
        .with_guessed_format()
        .map_err(|err| io_error("probe source image format", &source_path, err))?
        .format();
    let (source_format, image_format) = match content_format {
        Some(format) => match source_format_of(format) {
            Some(source_format) => (source_format, format),
            None => {
                let detected = format!("{format:?}").to_ascii_lowercase();
                return Err(SingleImageError::UnsupportedFormat { path: source_path, detected });
            }
        },
        None if extension_names_format(extension, SourceImageFormat::Tga) => (SourceImageFormat::Tga, ImageFormat::Tga),
        None => return Err(SingleImageError::UnsupportedFormat { path: source_path, detected: "unknown".to_string() }),
    };
    // Without a content signature (TGA) the extension decided the format, so it agrees.
    let extension_matches_content = content_format.is_none() || extension_names_format(extension, source_format);
    let source_animated = probe_animated(&bytes, source_format, &source_path)?;
    let reader = ImageReader::with_format(Cursor::new(bytes.as_slice()), image_format);

    let decode_err = |err: image::ImageError| SingleImageError::Decode { path: source_path.clone(), reason: err.to_string() };
    let mut decoder = reader.into_decoder().map_err(decode_err)?;
    // ICC and orientation are metadata: an unreadable block is logged and the pixels still
    // open (without the profile / unrotated) rather than refusing the whole image.
    let icc_profile = decoder.icc_profile().unwrap_or_else(|err| {
        runtime_log::log_warn(format!("single image: ICC profile unreadable in {}: {err}", source_path.display()));
        None
    });
    let orientation = decoder.orientation().unwrap_or_else(|err| {
        runtime_log::log_warn(format!("single image: EXIF orientation unreadable in {}: {err}", source_path.display()));
        Orientation::NoTransforms
    });
    let mut image = DynamicImage::from_decoder(decoder).map_err(decode_err)?;
    image.apply_orientation(orientation);
    let exif_orientation_applied = orientation != Orientation::NoTransforms;
    let (width_px, height_px) = (image.width(), image.height());

    let chapter_dir = scratch.chapter_dir();
    let src_dir = chapter_dir.join(ms_config::SRC_DIR);
    std::fs::create_dir_all(&src_dir).map_err(|err| io_error("create scratch chapter", &src_dir, err))?;
    let page_path = src_dir.join(SCRATCH_PAGE_FILE);
    // The shared service-format PNG writer (RGBA8, fast compression): 16-bit and float
    // sources are reduced to RGBA8 here, like every page the app works on.
    crate::write_png_fast(&image, &page_path).map_err(|err| io_error("write scratch page", &page_path, io::Error::other(format!("{err:#}"))))?;
    drop(image);

    // D4: seed `comic_type` so the load yields `Some(Pages)` and no comic-type prompt opens.
    let settings_file = scratch.root().join(SCRATCH_TITLE_DIR).join(ms_config::PROJECT_SETTINGS_FILE);
    crate::save_comic_type_to_project_file(&settings_file, ComicType::Pages, ms_docstore::Durability::Contents)
        .map_err(|err| io_error("seed scratch comic type", &settings_file, io::Error::other(err)))?;

    runtime_log::log_info(format!(
        "single image: prepared {} (format {}, {width_px}x{height_px}, animated {source_animated}, orientation applied {exif_orientation_applied}, icc {}, extension matches content {extension_matches_content})",
        source_path.display(),
        source_format.as_str(),
        if icc_profile.is_some() { "present" } else { "absent" },
    ));

    let session = Arc::new(SingleImageSession {
        source_path,
        source_format,
        extension_matches_content,
        source_animated,
        exif_orientation_applied,
        icc_profile,
        scratch_root: scratch.root().to_path_buf(),
    });
    Ok(PreparedSingleImage { chapter_dir, session, width_px, height_px })
}

/// Opens `source` as a single-image session: [`prepare_session`], then the regular
/// `ProjectData::load` on the scratch chapter, then `session` set to
/// `SessionKind::SingleImage`. Blocking: the studio load worker only.
///
/// # Errors
/// Every [`prepare_session`] error, and `ProjectLoad` when the scratch chapter does not load
/// or does not come out as exactly one page.
pub fn open_single_image(source: &Path, scratch: &SingleImageScratch, user_settings: &Value) -> Result<ProjectData, SingleImageError> {
    let prepared = prepare_session(source, scratch)?;
    let mut data = ProjectData::load(&prepared.chapter_dir, user_settings)
        .map_err(|err| SingleImageError::ProjectLoad { reason: format!("{err:#}") })?;
    if data.pages.len() != 1 {
        return Err(SingleImageError::ProjectLoad {
            reason: format!("scratch chapter {} has {} pages, expected 1", prepared.chapter_dir.display(), data.pages.len()),
        });
    }
    data.session = SessionKind::SingleImage(prepared.session);
    Ok(data)
}

#[cfg(test)]
#[path = "single_image_tests.rs"]
mod tests;
