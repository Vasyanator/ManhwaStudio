/*
File: crates/ms-project/src/single_image_tests.rs

Purpose:
Unit tests of `single_image.rs`: scratch reserve / remove / sweep, the prepare step (format
detection, orientation, animation probe, exact PNG round trip, the user's folder never
written) and `open_single_image` on top of the regular project load.

Notes:
Every test works inside its own `tempfile` directory (removed on drop): one subdirectory
stands in for the user's folder, another for the scratch base. Nothing touches the real
`scratch_base()` or any document of the running program.
*/

use super::*;
use image::codecs::gif::GifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::{Delay, Frame, ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};
use std::collections::BTreeMap;
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

/// Decodes the prepared scratch page.
fn scratch_page(prepared: &PreparedSingleImage) -> RgbaImage {
    image::open(prepared.chapter_dir.join(ms_config::SRC_DIR).join(SCRATCH_PAGE_FILE)).expect("scratch page decodes").to_rgba8()
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

    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let prepared = prepare_session(&path, &scratch).expect("prepare");

    assert_eq!(snapshot(&fixture.user_dir()), before, "the user's folder must not change");
    assert_eq!(scratch_page(&prepared), source);
    assert_eq!((prepared.width_px, prepared.height_px), (7, 4));
    let session = &prepared.session;
    assert_eq!(session.source_format, SourceImageFormat::Png);
    assert!(session.extension_matches_content);
    assert!(!session.source_animated);
    assert!(!session.exif_orientation_applied);
    assert_eq!(session.source_path, path);
    assert_eq!(session.scratch_root, scratch.root());
    assert!(prepared.chapter_dir.starts_with(scratch.root()));
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

    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let prepared = prepare_session(&path, &scratch).expect("prepare");
    assert!(prepared.session.exif_orientation_applied);
    assert_eq!(prepared.session.source_format, SourceImageFormat::Jpeg);
    assert_eq!((prepared.width_px, prepared.height_px), (8, 16));
    let page = scratch_page(&prepared);
    assert_eq!(page.dimensions(), (8, 16));
    assert!(page.get_pixel(4, 3)[0] < 64, "top must be the black half");
    assert!(page.get_pixel(4, 12)[0] > 192, "bottom must be the white half");
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

    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let prepared = prepare_session(&path, &scratch).expect("prepare");
    assert!(prepared.session.source_animated);
    assert_eq!(prepared.session.source_format, SourceImageFormat::Gif);
    let pixel = *scratch_page(&prepared).get_pixel(1, 1);
    assert!(pixel[0] > 200 && pixel[2] < 50, "first (red) frame expected, got {pixel:?}");
    scratch.remove().expect("remove");
}

#[test]
fn single_frame_gif_is_not_animated() {
    let fixture = Fixture::new();
    let path = fixture.user_file("still.gif", &encode(&rgb_sample(), ImageFormat::Gif));
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let prepared = prepare_session(&path, &scratch).expect("prepare");
    assert!(!prepared.session.source_animated);
    scratch.remove().expect("remove");
}

#[test]
fn unsupported_and_unrecognized_content_is_rejected() {
    let fixture = Fixture::new();
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");

    let garbage = fixture.user_file("garbage.png", b"definitely not an image at all");
    match prepare_session(&garbage, &scratch) {
        Err(SingleImageError::UnsupportedFormat { detected, .. }) => assert_eq!(detected, "unknown"),
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
    // A recognized but non-input format (ICO) is unsupported too.
    let ico = fixture.user_file("icon.ico", &encode(&DynamicImage::ImageRgba8(RgbaImage::new(4, 4)), ImageFormat::Ico));
    match prepare_session(&ico, &scratch) {
        Err(SingleImageError::UnsupportedFormat { detected, .. }) => assert_eq!(detected, "ico"),
        other => panic!("expected UnsupportedFormat, got {other:?}"),
    }
    let err = prepare_session(&ico, &scratch).expect_err("still unsupported");
    assert!(!err.user_message().is_empty());
    assert!(err.to_string().contains("icon.ico"));
    scratch.remove().expect("remove");
}

#[test]
fn missing_path_and_directory_are_rejected() {
    let fixture = Fixture::new();
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    assert!(matches!(prepare_session(&fixture.user_dir().join("absent.png"), &scratch), Err(SingleImageError::NotFound { .. })));
    assert!(matches!(prepare_session(&fixture.user_dir(), &scratch), Err(SingleImageError::NotAFile { .. })));
    scratch.remove().expect("remove");
}

#[test]
fn extension_content_mismatch_is_flagged() {
    let fixture = Fixture::new();
    let path = fixture.user_file("actually_png.jpg", &encode(&rgb_sample(), ImageFormat::Png));
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
    let prepared = prepare_session(&path, &scratch).expect("prepare");
    assert_eq!(prepared.session.source_format, SourceImageFormat::Png);
    assert!(!prepared.session.extension_matches_content);
    scratch.remove().expect("remove");
}

#[test]
fn every_input_file_type_decodes_a_generated_sample() {
    let fixture = Fixture::new();
    let sample = rgb_sample();
    for file_type in INPUT_FILE_TYPES {
        for ext in file_type.extensions {
            let format = extension_format(ext);
            let path = fixture.user_file(&format!("sample.{ext}"), &encode(&sample, format));
            let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");
            let prepared = prepare_session(&path, &scratch).unwrap_or_else(|err| panic!("{ext}: {err}"));
            assert_eq!(source_format_of(format), Some(prepared.session.source_format), "{ext}");
            assert!(prepared.session.extension_matches_content, "{ext}");
            assert_eq!((prepared.width_px, prepared.height_px), (sample.width(), sample.height()), "{ext}");
            // Every lossless input keeps the pixels exactly (GIF quantizes, JPEG is lossy).
            if !matches!(format, ImageFormat::Gif | ImageFormat::Jpeg) {
                assert_eq!(scratch_page(&prepared), sample.to_rgba8(), "{ext}");
            }
            scratch.remove().expect("remove");
        }
    }
}

/// The `image` format an input-table extension stands for (the table's canonical spelling
/// covers aliases `image` does not know, such as `jpe`).
fn extension_format(ext: &str) -> ImageFormat {
    let file_type = INPUT_FILE_TYPES.iter().find(|file_type| file_type.extensions.contains(&ext)).expect("listed extension");
    let canonical = file_type.extensions.first().copied().expect("non-empty extensions");
    ImageFormat::from_extension(canonical).expect("canonical extension known to image")
}

#[test]
fn open_single_image_loads_one_page_inside_the_scratch() {
    let fixture = Fixture::new();
    let path = fixture.user_file("page.webp", &encode(&rgb_sample(), ImageFormat::WebP));
    let before = snapshot(&fixture.user_dir());
    let scratch = SingleImageScratch::reserve(&fixture.scratch_base()).expect("reserve");

    let data = open_single_image(&path, &scratch, &serde_json::json!({})).expect("open");

    assert_eq!(data.pages.len(), 1);
    assert_eq!(data.comic_type, Some(ComicType::Pages));
    assert!(data.is_single_image());
    let canonical_root = scratch.root().canonicalize().expect("canonical root");
    assert!(data.project_dir.starts_with(&canonical_root));
    assert!(data.paths.title_dir.starts_with(&canonical_root));
    assert!(data.paths.unsaved_dir.starts_with(&canonical_root));
    assert!(data.pages[0].path.starts_with(&canonical_root));
    assert_eq!(data.user_facing_dir(), fixture.user_dir());
    match data.session() {
        SessionKind::SingleImage(session) => assert_eq!(session.source_format, SourceImageFormat::WebP),
        SessionKind::Project => panic!("expected a single-image session"),
    }
    assert_eq!(snapshot(&fixture.user_dir()), before, "the user's folder must not change");
    scratch.remove().expect("remove");
}

#[test]
fn session_debug_never_dumps_icc_bytes() {
    let session = SingleImageSession {
        source_path: PathBuf::from("/home/u/page.png"),
        source_format: SourceImageFormat::Png,
        extension_matches_content: true,
        source_animated: false,
        exif_orientation_applied: false,
        icc_profile: Some(vec![0xAB; 64]),
        scratch_root: PathBuf::from("/tmp/scratch"),
    };
    let text = format!("{session:?}");
    assert!(text.contains("icc_profile_len: Some(64)"));
    assert!(!text.contains("171"));
}
