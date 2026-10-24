/*
File: crates/ms-project/src/single_image_tests.rs

Purpose:
Unit tests of `single_image.rs`: scratch reserve / remove / sweep, the per-file decode (format
detection, orientation, animation probe, exact PNG round trip, the user's folder never
written), `open_image_set` on top of the regular project load (order, skips, cancel, digests,
the one-image case) and the size estimate / confirmation rule.

Notes:
Every test works inside its own `tempfile` directory (removed on drop): one subdirectory
stands in for the user's folder, another for the scratch base. Nothing touches the real
`scratch_base()` or any document of the running program.
*/

use super::*;
use crate::{BuiltinLayoutPreset, CanvasLayout, ComicType, LayoutPresetSelection};
use image::codecs::gif::GifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::{Delay, Frame, ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

/// A test layout: `<tmp>/user` plays the user's folder, `<tmp>/scratch` the scratch base.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(dir.path().join("user")).expect("user dir");
        Self { dir }
    }

    fn user_dir(&self) -> PathBuf {
        self.dir.path().join("user")
    }

    fn scratch_base(&self) -> PathBuf {
        self.dir.path().join("scratch")
    }

    /// Writes `bytes` as `<user>/<name>` and returns its path.
    fn user_file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.user_dir().join(name);
        std::fs::write(&path, bytes).expect("write user file");
        path
    }
}

/// Encodes `image` into `format` in memory.
fn encode(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).expect("encode sample");
    bytes.into_inner()
}

/// A small RGB sample with distinct pixels.
fn rgb_sample() -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(5, 3, |x, y| Rgb([u8::try_from(x * 40).unwrap_or(0), u8::try_from(y * 70).unwrap_or(0), 128])))
}

/// Opens `sources` as an image set into a fresh scratch under the fixture (no prior skips, no
/// progress observer, never cancelled).
fn open_set(fixture: &Fixture, sources: &[PathBuf]) -> (Result<ProjectData, SingleImageError>, SingleImageScratch) {
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let result = open_image_set(sources, Vec::new(), &scratch, &serde_json::json!({}), &mut |_, _| {}, &AtomicBool::new(false));
    (result, scratch)
}

/// The image-set session of an opened `data`.
fn session_of(data: &ProjectData) -> &SingleImageSession {
    match data.session() {
        SessionKind::SingleImage(session) => session,
        SessionKind::Project => panic!("expected an image-set session"),
    }
}

/// Decodes scratch page `page_idx` of an opened `data`.
fn scratch_page(data: &ProjectData, page_idx: usize) -> RgbaImage {
    image::open(&data.pages[page_idx].path).expect("scratch page decodes").to_rgba8()
}

/// Opens the one file `path` and returns the data, its only source's facts and the scratch.
fn open_one(fixture: &Fixture, path: &Path) -> (ProjectData, SourceImageFacts, SingleImageScratch) {
    let (result, scratch) = open_set(fixture, &[path.to_path_buf()]);
    let data = result.unwrap_or_else(|err| panic!("open {}: {err}", path.display()));
    let facts = session_of(&data).first_source().clone();
    (data, facts, scratch)
}

/// A distinct solid-colour PNG for page-order tests.
fn solid_png(red: u8) -> Vec<u8> {
    encode(&DynamicImage::ImageRgba8(RgbaImage::from_pixel(3, 2, Rgba([red, 7, 9, 255]))), ImageFormat::Png)
}

/// Name -> (len, mtime) of every entry under `dir`, recursively.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, SystemTime)> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("dir entry");
        let meta = entry.metadata().expect("metadata");
        out.insert(entry.path(), (meta.len(), meta.modified().expect("mtime")));
        if meta.is_dir() {
            out.extend(snapshot(&entry.path()));
        }
    }
    out
}

/// A little-endian EXIF (TIFF) chunk holding only `Orientation = value`.
fn exif_orientation_chunk(value: u8) -> Vec<u8> {
    vec![
        0x49, 0x49, 0x2A, 0x00, // "II", 42
        0x08, 0x00, 0x00, 0x00, // IFD0 at offset 8
        0x01, 0x00, // one entry
        0x12, 0x01, 0x03, 0x00, // tag 0x0112 (Orientation), type SHORT
        0x01, 0x00, 0x00, 0x00, // count 1
        value, 0x00, 0x00, 0x00, // value + padding
        0x00, 0x00, 0x00, 0x00, // no next IFD
    ]
}

#[test]
fn reserve_creates_marker_and_lock_and_remove_deletes_the_root() {
    let fixture = Fixture::new();
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let root = scratch.root().to_path_buf();
    assert!(root.starts_with(fixture.scratch_base()));
    assert!(root.join(SCRATCH_MARKER_FILE).is_file());
    assert!(root.join(SCRATCH_LOCK_FILE).is_file());
    assert_eq!(scratch.chapter_dir(), root.join("title").join("chapter"));

    let second = SingleImageScratch::reserve(&fixture.scratch_base()).expect("second reserve");
    assert_ne!(second.root(), root.as_path());

    scratch.remove().expect("remove");
    assert!(!root.exists());
    second.remove().expect("remove second");
}

#[test]
fn sweep_removes_only_marked_unlocked_sessions() {
    let fixture = Fixture::new();
    let base = fixture.scratch_base();

    // Live: reserved and still held by this value.
    let live = SingleImageScratch::reserve(&base).expect("reserve live");
    // Stale: reserved, then dropped (lock released, directory left behind).
    let stale_root = {
        let stale = SingleImageScratch::reserve(&base).expect("reserve stale");
        stale.root().to_path_buf()
    };
    assert!(stale_root.exists(), "Drop must not delete the scratch");
    // Foreign: no marker, even with an unlocked lock file.
    let foreign = base.join("foreign");
    std::fs::create_dir(&foreign).expect("foreign dir");
    std::fs::write(foreign.join(SCRATCH_LOCK_FILE), b"").expect("foreign lock");
    // A plain file at the base is never touched either.
    std::fs::write(base.join("note.txt"), b"keep").expect("plain file");

    let report = sweep_stale_sessions(&base);
    assert_eq!(report, SweepReport { removed: 1, live: 1, errors: Vec::new() });
    assert!(!stale_root.exists());
    assert!(live.root().exists());
    assert!(foreign.exists());
    assert!(base.join("note.txt").exists());

    live.remove().expect("remove live");
}

#[test]
fn exclusive_lock_is_refused_through_a_second_handle_until_released() {
    let fixture = Fixture::new();
    let path = fixture.dir.path().join("probe.lock");
    let first = OpenOptions::new().write(true).create_new(true).open(&path).expect("create lock file");
    let second = OpenOptions::new().write(true).open(&path).expect("reopen lock file");
    try_lock_exclusive(&first).expect("first lock");
    match try_lock_exclusive(&second) {
        Err(TryLockError::WouldBlock) => {}
        Ok(()) => panic!("second handle must not acquire a held lock"),
        Err(TryLockError::Error(err)) => panic!("second handle failed instead of WouldBlock: {err}"),
    }
    drop(first);
    try_lock_exclusive(&second).expect("lock after release");
}

#[test]
fn lock_error_text_carries_the_inner_os_error() {
    let err = TryLockError::Error(io::Error::new(io::ErrorKind::Unsupported, "try_lock() not supported"));
    let text = try_lock_error_text(&err);
    assert!(text.contains("try_lock() not supported"), "{text}");
    assert!(text.contains("Unsupported"), "{text}");
}

#[test]
fn sweep_of_a_missing_base_is_empty() {
    let fixture = Fixture::new();
    assert_eq!(sweep_stale_sessions(&fixture.scratch_base().join("absent")), SweepReport::default());
}

#[test]
fn png_rgba_round_trip_is_exact_and_user_folder_is_untouched() {
    let fixture = Fixture::new();
    let source = RgbaImage::from_fn(7, 4, |x, y| Rgba([u8::try_from(x * 30).unwrap_or(0), u8::try_from(y * 60).unwrap_or(0), 200, u8::try_from(x * 20 + y).unwrap_or(0)]));
    let path = fixture.user_file("page.png", &encode(&DynamicImage::ImageRgba8(source.clone()), ImageFormat::Png));
    let before = snapshot(&fixture.user_dir());

    let (data, facts, scratch) = open_one(&fixture, &path);

    assert_eq!(snapshot(&fixture.user_dir()), before, "the user's folder must not change");
    assert_eq!(scratch_page(&data, 0), source);
    assert_eq!(facts.source_format, SourceImageFormat::Png);
    assert!(facts.extension_matches_content);
    assert!(!facts.source_animated);
    assert!(!facts.exif_orientation_applied);
    assert_eq!(facts.source_path, path);
    assert_eq!(session_of(&data).scratch_root(), scratch.root());
    scratch.remove().expect("remove");
}

#[test]
fn jpeg_exif_orientation_is_baked_into_the_page() {
    let fixture = Fixture::new();
    // 16x8: left half black, right half white. Orientation 6 = rotate 90 degrees clockwise,
    // so the upright page is 8x16 with black on top and white below.
    let source = RgbImage::from_fn(16, 8, |x, _| if x < 8 { Rgb([0, 0, 0]) } else { Rgb([255, 255, 255]) });
    let mut bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut bytes, 95);
    encoder.set_exif_metadata(exif_orientation_chunk(6)).expect("jpeg accepts exif");
    encoder.write_image(source.as_raw(), 16, 8, image::ExtendedColorType::Rgb8).expect("encode jpeg");
    let path = fixture.user_file("photo.jpg", &bytes);

    let (data, facts, scratch) = open_one(&fixture, &path);
    assert!(facts.exif_orientation_applied);
    assert_eq!(facts.source_format, SourceImageFormat::Jpeg);
    let page = scratch_page(&data, 0);
    assert_eq!(page.dimensions(), (8, 16));
    assert!(page.get_pixel(4, 3)[0] < 64, "top must be the black half");
    assert!(page.get_pixel(4, 12)[0] > 192, "bottom must be the white half");
    // The digest is of the UPRIGHT page, the buffer every later flatten composes over.
    assert_eq!(facts.opened_digest, PixelDigest::of_rgba8(8, 16, page.as_raw()));
    assert_ne!(facts.opened_digest, PixelDigest::of_rgba8(16, 8, page.as_raw()));
    scratch.remove().expect("remove");
}

#[test]
fn two_frame_gif_is_marked_animated_and_opens_the_first_frame() {
    let fixture = Fixture::new();
    let first = RgbaImage::from_pixel(4, 4, Rgba([255, 0, 0, 255]));
    let second = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 255, 255]));
    let mut bytes = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder
            .encode_frames([Frame::from_parts(first, 0, 0, Delay::from_numer_denom_ms(100, 1)), Frame::from_parts(second, 0, 0, Delay::from_numer_denom_ms(100, 1))])
            .expect("encode gif");
    }
    let path = fixture.user_file("anim.gif", &bytes);

    let (data, facts, scratch) = open_one(&fixture, &path);
    assert!(facts.source_animated);
    assert_eq!(facts.source_format, SourceImageFormat::Gif);
    let pixel = *scratch_page(&data, 0).get_pixel(1, 1);
    assert!(pixel[0] > 200 && pixel[2] < 50, "first (red) frame expected, got {pixel:?}");
    scratch.remove().expect("remove");
}

#[test]
fn single_frame_gif_is_not_animated() {
    let fixture = Fixture::new();
    let path = fixture.user_file("still.gif", &encode(&rgb_sample(), ImageFormat::Gif));
    let (_data, facts, scratch) = open_one(&fixture, &path);
    assert!(!facts.source_animated);
    scratch.remove().expect("remove");
}

#[test]
fn unsupported_and_unrecognized_content_is_rejected() {
    let fixture = Fixture::new();

    let garbage = fixture.user_file("garbage.png", b"definitely not an image at all");
    match decode_source(&garbage).map(|_| ()) {
        Err(SingleImageError::UnsupportedFormat { detected, .. }) => assert_eq!(detected, "unknown"),
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
    // A recognized but non-input format (ICO) is unsupported too.
    let ico = fixture.user_file("icon.ico", &encode(&DynamicImage::ImageRgba8(RgbaImage::new(4, 4)), ImageFormat::Ico));
    match decode_source(&ico).map(|_| ()) {
        Err(SingleImageError::UnsupportedFormat { detected, .. }) => assert_eq!(detected, "ico"),
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
    let err = decode_source(&ico).map(|_| ()).expect_err("still unsupported");
    assert!(!err.user_message().is_empty());
    assert!(err.to_string().contains("icon.ico"));
}

#[test]
fn missing_path_and_directory_are_rejected() {
    let fixture = Fixture::new();
    assert!(matches!(decode_source(&fixture.user_dir().join("absent.png")), Err(SingleImageError::NotFound { .. })));
    assert!(matches!(decode_source(&fixture.user_dir()), Err(SingleImageError::NotAFile { .. })));
}

#[test]
fn extension_content_mismatch_is_flagged() {
    let fixture = Fixture::new();
    let path = fixture.user_file("actually_png.jpg", &encode(&rgb_sample(), ImageFormat::Png));
    let (_data, facts, scratch) = open_one(&fixture, &path);
    assert_eq!(facts.source_format, SourceImageFormat::Png);
    assert!(!facts.extension_matches_content);
    scratch.remove().expect("remove");
}

#[test]
fn every_input_file_type_decodes_a_generated_sample() {
    // All of them opened as ONE set: every type decodes, and the page order is the input order.
    let fixture = Fixture::new();
    let sample = rgb_sample();
    let mut sources = Vec::new();
    let mut formats = Vec::new();
    for file_type in INPUT_FILE_TYPES {
        for ext in file_type.extensions {
            let format = extension_format(ext);
            sources.push(fixture.user_file(&format!("sample.{ext}"), &encode(&sample, format)));
            formats.push((*ext, format));
        }
    }
    let (result, scratch) = open_set(&fixture, &sources);
    let data = result.unwrap_or_else(|err| panic!("open every input type: {err}"));
    let session = session_of(&data);
    assert!(session.skipped().is_empty(), "{:?}", session.skipped());
    assert_eq!(session.page_count(), formats.len());
    for (page_idx, (ext, format)) in formats.iter().enumerate() {
        let facts = session.source(page_idx).expect("one source per page");
        assert_eq!(facts.source_path, sources[page_idx], "{ext}");
        assert_eq!(source_format_of(*format), Some(facts.source_format), "{ext}");
        assert!(facts.extension_matches_content, "{ext}");
        let page = scratch_page(&data, page_idx);
        assert_eq!(page.dimensions(), (sample.width(), sample.height()), "{ext}");
        assert_eq!(facts.opened_digest, PixelDigest::of_rgba8(page.width(), page.height(), page.as_raw()), "{ext}");
        // Every lossless input keeps the pixels exactly (GIF quantizes, JPEG is lossy).
        if !matches!(format, ImageFormat::Gif | ImageFormat::Jpeg) {
            assert_eq!(page, sample.to_rgba8(), "{ext}");
        }
    }
    scratch.remove().expect("remove");
}

/// The `image` format an input-table extension stands for (the table's canonical spelling
/// covers aliases `image` does not know, such as `jpe`).
fn extension_format(ext: &str) -> ImageFormat {
    let file_type = INPUT_FILE_TYPES.iter().find(|file_type| file_type.extensions.contains(&ext)).expect("listed extension");
    let canonical = file_type.extensions.first().copied().expect("non-empty extensions");
    ImageFormat::from_extension(canonical).expect("canonical extension known to image")
}

#[test]
fn one_image_set_loads_one_page_inside_the_scratch() {
    let fixture = Fixture::new();
    let path = fixture.user_file("page.webp", &encode(&rgb_sample(), ImageFormat::WebP));
    let before = snapshot(&fixture.user_dir());
    let (result, scratch) = open_set(&fixture, std::slice::from_ref(&path));
    let data = result.expect("open");

    assert_eq!(data.pages.len(), 1);
    assert_eq!(data.comic_type, Some(ComicType::Pages));
    assert!(data.is_single_image());
    let canonical_root = scratch.root().canonicalize().expect("canonical root");
    assert!(data.project_dir.starts_with(&canonical_root));
    assert!(data.paths.title_dir.starts_with(&canonical_root));
    assert!(data.paths.unsaved_dir.starts_with(&canonical_root));
    assert!(data.pages[0].path.starts_with(&canonical_root));
    assert_eq!(data.user_facing_dir(), fixture.user_dir());
    let session = session_of(&data);
    assert_eq!(session.page_count(), 1);
    assert_eq!(session.first_source().source_format, SourceImageFormat::WebP);
    assert!(session.skipped().is_empty());
    assert_eq!(data.pages[0].path.file_name().and_then(|name| name.to_str()), Some("000.png"));
    assert_eq!(snapshot(&fixture.user_dir()), before, "the user's folder must not change");
    scratch.remove().expect("remove");
}

/// An image set takes the whole layout from the «Pages» preset: a legacy global layout value in
/// the user config (which an unmigrated title would still read) does not reach it.
#[test]
fn image_set_layout_is_the_pages_preset() {
    let fixture = Fixture::new();
    let path = fixture.user_file("page.png", &solid_png(40));
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let user_settings = serde_json::json!({"Canvas": {"aside_min_width_px": 600, "aside_compact_mode": "strong", "scale_bubbles": false}});
    let data = open_image_set(std::slice::from_ref(&path), Vec::new(), &scratch, &user_settings, &mut |_, _| {}, &AtomicBool::new(false)).expect("open");
    assert_eq!(data.comic_type, Some(ComicType::Pages));
    assert_eq!(CanvasLayout::of(&data.canvas_settings), BuiltinLayoutPreset::Pages.factory());
    assert_eq!(data.canvas_settings.layout_preset, LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Pages));
    scratch.remove().expect("remove");
}

#[test]
fn session_debug_never_dumps_icc_bytes() {
    let facts = SourceImageFacts {
        source_path: PathBuf::from("/home/u/page.png"),
        source_format: SourceImageFormat::Png,
        extension_matches_content: true,
        source_animated: false,
        exif_orientation_applied: false,
        icc_profile: Some(vec![0xAB; 64]),
        opened_digest: PixelDigest::of_rgba8(0, 0, &[]),
    };
    let session = SingleImageSession::new(vec![facts], Vec::new(), PathBuf::from("/home/u/scratch")).expect("one source");
    let text = format!("{session:?}");
    assert!(text.contains("icc_profile_len: Some(64)"));
    assert!(!text.contains("171"));
}

#[test]
fn session_requires_at_least_one_source() {
    assert!(SingleImageSession::new(Vec::new(), Vec::new(), PathBuf::from("/home/u/scratch")).is_none());
}

#[test]
fn three_sources_open_in_the_given_order() {
    let fixture = Fixture::new();
    // Names deliberately NOT in alphabetical order: the received order wins.
    let sources = vec![fixture.user_file("b.png", &solid_png(30)), fixture.user_file("a.png", &solid_png(10)), fixture.user_file("c.png", &solid_png(20))];
    let before = snapshot(&fixture.user_dir());
    let mut seen = Vec::new();
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let data = open_image_set(&sources, Vec::new(), &scratch, &serde_json::json!({}), &mut |done, total| seen.push((done, total)), &AtomicBool::new(false)).expect("open");

    assert_eq!(seen, [(0, 3), (1, 3), (2, 3), (3, 3)]);
    assert_eq!(data.pages.len(), 3);
    assert_eq!(data.comic_type, Some(ComicType::Pages));
    let session = session_of(&data);
    for (page_idx, red) in [30_u8, 10, 20].into_iter().enumerate() {
        assert_eq!(data.pages[page_idx].idx, page_idx);
        assert_eq!(scratch_page(&data, page_idx).get_pixel(0, 0)[0], red, "page {page_idx}");
        assert_eq!(session.source(page_idx).map(|facts| facts.source_path.clone()), Some(sources[page_idx].clone()));
    }
    assert!(session.source(3).is_none());
    assert_eq!(data.user_facing_dir(), fixture.user_dir());
    assert_eq!(snapshot(&fixture.user_dir()), before, "the user's folder must not change");
    scratch.remove().expect("remove");
}

#[test]
fn a_bad_middle_file_is_skipped_and_the_rest_keep_their_order() {
    let fixture = Fixture::new();
    let prior = SkippedSource { path: PathBuf::from("/home/u/lost.png"), message: "copy failed".to_owned() };
    let sources = vec![fixture.user_file("first.png", &solid_png(1)), fixture.user_file("broken.png", b"not an image"), fixture.user_dir().join("missing.png"), fixture.user_file("last.png", &solid_png(2))];
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let data = open_image_set(&sources, vec![prior.clone()], &scratch, &serde_json::json!({}), &mut |_, _| {}, &AtomicBool::new(false)).expect("open");

    assert_eq!(data.pages.len(), 2);
    assert_eq!(scratch_page(&data, 0).get_pixel(0, 0)[0], 1);
    assert_eq!(scratch_page(&data, 1).get_pixel(0, 0)[0], 2);
    let session = session_of(&data);
    assert_eq!(session.source(1).map(|facts| facts.source_path.clone()), Some(sources[3].clone()));
    let skipped: Vec<&Path> = session.skipped().iter().map(|entry| entry.path.as_path()).collect();
    assert_eq!(skipped, [prior.path.as_path(), sources[1].as_path(), sources[2].as_path()], "prior skips first, then request order");
    assert_eq!(session.skipped()[0], prior);
    assert!(session.skipped().iter().all(|entry| !entry.message.is_empty()));
    scratch.remove().expect("remove");
}

#[test]
fn nothing_opened_lists_every_skipped_file() {
    let fixture = Fixture::new();
    let sources = vec![fixture.user_file("x.png", b"garbage"), fixture.user_dir().join("y.png")];
    let (result, scratch) = open_set(&fixture, &sources);
    match result {
        Err(SingleImageError::NothingOpened { skipped }) => {
            assert_eq!(skipped.len(), 2);
            // The catalog is not installed in unit tests, so the listed names are checked on the
            // pure helper the message is built from.
            assert_eq!(listed_file_names(&skipped), (vec!["x.png".to_owned(), "y.png".to_owned()], 0));
            assert!(!SingleImageError::NothingOpened { skipped }.user_message().is_empty());
        }
        other => panic!("expected NothingOpened, got {:?}", other.map(|data| data.pages.len())),
    }
    scratch.remove().expect("remove");
}

#[test]
fn a_failed_one_image_open_shows_that_files_own_reason() {
    // One-image parity: the text is exactly what a single-image open has always shown.
    let fixture = Fixture::new();
    let missing = fixture.user_dir().join("absent.png");
    let (result, scratch) = open_set(&fixture, std::slice::from_ref(&missing));
    let err = result.map(|data| data.pages.len()).expect_err("nothing to open");
    let expected = decode_source(&missing).map(|_| ()).expect_err("missing").user_message();
    assert_eq!(err.user_message(), expected);
    scratch.remove().expect("remove");
}

#[test]
fn nothing_opened_lists_at_most_ten_names() {
    let skipped: Vec<SkippedSource> = (0..13).map(|n| SkippedSource { path: PathBuf::from(format!("/home/u/file{n:02}.png")), message: String::new() }).collect();
    let (names, rest) = listed_file_names(&skipped);
    assert_eq!(names.len(), NOTHING_OPENED_LISTED_NAMES);
    assert_eq!(names.first().map(String::as_str), Some("file00.png"));
    assert_eq!(names.last().map(String::as_str), Some("file09.png"));
    assert_eq!(rest, 3);
    // A lone entry shows its own reason, not a list.
    let only = SkippedSource { path: PathBuf::from("/home/u/a.png"), message: "own reason".to_owned() };
    assert_eq!(nothing_opened_message(std::slice::from_ref(&only)), "own reason");
}

#[test]
fn a_set_cancel_flag_stops_the_open() {
    let fixture = Fixture::new();
    let sources = vec![fixture.user_file("a.png", &solid_png(1)), fixture.user_file("b.png", &solid_png(2))];
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let cancel = AtomicBool::new(false);
    let mut calls = 0;
    let result = open_image_set(
        &sources,
        Vec::new(),
        &scratch,
        &serde_json::json!({}),
        &mut |done, _| {
            calls += 1;
            // Cancel once the first file is done: the second one must not be decoded.
            if done == 1 {
                cancel.store(true, AtomicOrdering::Relaxed);
            }
        },
        &cancel,
    );
    assert!(matches!(result, Err(SingleImageError::Cancelled)));
    assert_eq!(calls, 2, "progress (0, 2) and (1, 2) only");
    assert!(!scratch.chapter_dir().join(ms_config::SRC_DIR).join("001.png").exists());
    scratch.remove().expect("remove");
}

#[test]
fn page_names_past_999_keep_numeric_order_in_the_loader_sort() {
    // Pure, on names: `scratch_page_file_name` and the loader's `image_sort_key` agree for a
    // set larger than three digits.
    let mut names: Vec<PathBuf> = (0..1001).map(|idx| PathBuf::from(scratch_page_file_name(idx))).collect();
    let expected = names.clone();
    names.reverse();
    names.sort_by(|a, b| crate::image_sort_key(a, b));
    assert_eq!(names, expected);
    assert_eq!(scratch_page_file_name(7), "007.png");
    assert_eq!(scratch_page_file_name(1000), "1000.png");
}

#[test]
fn estimate_reads_headers_and_counts_unreadable_files_as_zero() {
    let fixture = Fixture::new();
    let sources = vec![
        fixture.user_file("a.png", &encode(&rgb_sample(), ImageFormat::Png)),
        fixture.user_file("b.jpg", &encode(&rgb_sample(), ImageFormat::Jpeg)),
        fixture.user_file("bad.png", b"garbage"),
        fixture.user_dir().join("missing.png"),
    ];
    let estimate = estimate_image_set(&sources);
    assert_eq!(estimate, ImageSetEstimate { count: 4, decoded_bytes: 2 * 5 * 3 * 4 });
}

#[test]
fn size_confirmation_edges() {
    let at = |count, decoded_bytes| needs_size_confirmation(&ImageSetEstimate { count, decoded_bytes });
    assert!(!at(IMAGE_SET_CONFIRM_COUNT, IMAGE_SET_CONFIRM_DECODED_BYTES));
    assert!(at(IMAGE_SET_CONFIRM_COUNT + 1, 0));
    assert!(at(1, IMAGE_SET_CONFIRM_DECODED_BYTES + 1));
    assert!(!at(0, 0));
    assert_eq!(IMAGE_SET_CONFIRM_DECODED_BYTES, 2 * 1024 * 1024 * 1024);
}
