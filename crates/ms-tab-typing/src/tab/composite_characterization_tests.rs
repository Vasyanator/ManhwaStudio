/*
File: tab/composite_characterization_tests.rs

Purpose:
Characterization tests that PIN the typing tab's current layer ordering and export flatten output
before the composite-plan refactor moves ordering into `ms_models::layer_model::ordering`
(`dev-docs/single_image_mode_plan.md`, Phase 0, WP-0.1). Every assertion here stayed unchanged
through that refactor (WP-0.4a only adapted the fixture helpers to the job's `bands`); the one
behaviour change is B1 — a raster inside a hidden PS group is absent from the flatten — pinned by
`flatten_omits_a_raster_inside_a_hidden_group`, plus the group-fold tests WP-0.4a added.

Covered:
- `flatten_typing_export_page_rgba` golden probes: band-Z interleave of rasters and overlays, the
  raster-below-overlay tie at an equal Z, invisible / half-opacity rasters, a mask-clipped overlay,
  the disk-band fallback with no layers dir (snapshot order wins, the job's in-memory bands are
  ignored), a translucent source keeping its alpha, and the PS group fold (B1: hidden / dimmed groups
  for rasters and text, a hidden text node, a NaN group opacity).
- `build_export_overlay_snapshots` page grouping, skipping and stable band-Z order.
- `raster_band_z` / `overlay_band_z` lookup rules (uid first, then text group, missing => top).
- the canvas fill-pass order (WP-0.1's `merged_fill_order` table, now asserted through
  `merged_fill_plan` with one band per item — expected orders unchanged).
- B2 (WP-0.4b, the typing canvas honours the PS group fold): `merged_fill_plan` / `page_fill_plan`
  step opacity for a dimmed group, hidden-group items left out of the draw, and the unified canvas
  hit-test letting a hidden-group overlay or raster fall through; `composite_step_tint`.
The `unified_topmost_pointer_target` tie table is already pinned in `tab/tests.rs`
(`unified_topmost_pointer_target_picks_by_z_overlay_wins_ties`) and is not duplicated here.

Notes:
The snapshot helpers return the snapshot together with its band (`Banded`), and `export_job` gathers
those bands into the job, so a test states each layer's band-Z where it creates the layer.
Scratch files live in a uniquely named directory under `std::env::temp_dir()`, removed by `Drop`.
Nothing is written inside the repository or into any owned document.
*/

use super::doc_layers::{composite_step_tint, raster_composite_item, text_composite_item};
use super::draw_page::{MergedFillItem, MergedFillStep, merged_fill_plan};
use super::*;
use crate::mask::TypingMaskExportPage;
use ms_models::layer_model::manifest::TransformRec;
use ms_models::layer_model::ordering::Band;

/// Edge of the square test page, in pixels.
const PAGE_PX: usize = 20;

/// A uniquely named scratch directory under the system temp dir, removed on drop (also on panic).
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Creates `<temp>/typ_compchar_<label>_<pid>_<nanos>`; panics with context if it cannot.
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("typ_compchar_{label}_{}_{nanos}", std::process::id()));
        if let Err(err) = std::fs::create_dir_all(&path) {
            panic!("could not create scratch dir {}: {err}", path.display());
        }
        Self { path }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("test cleanup: could not remove {}: {err}", self.path.display());
        }
    }
}

/// Writes a `PAGE_PX`-square source page filled with the straight-RGBA `px` and returns its path.
fn write_source_page(dir: &ScratchDir, px: [u8; 4]) -> PathBuf {
    let path = dir.path.join("page.png");
    let side = u32::try_from(PAGE_PX).expect("test page edge fits u32");
    let bytes: Vec<u8> = px.repeat(PAGE_PX * PAGE_PX);
    if let Err(err) = image::save_buffer(&path, &bytes, side, side, image::ColorType::Rgba8) {
        panic!("could not write the source page {}: {err}", path.display());
    }
    path
}

/// A layer snapshot together with the band that places it on the page's Z axis.
struct Banded<T> {
    snap: T,
    band: Band,
}

/// A process-unique raster uid, so every raster snapshot owns its own band.
fn next_raster_uid() -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    format!("raster-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// An affine, ungrouped raster snapshot of a solid `px` square of `size` px centered at `center`,
/// with a `Raster` band at `band_z`.
fn raster_snapshot(center: [f32; 2], size: usize, px: [u8; 4], band_z: u32, visible: bool, opacity: f32) -> Banded<TypingExportRasterSnapshot> {
    let uid = next_raster_uid();
    let band = Band::Raster { uid: uid.clone(), z: band_z };
    let snap = TypingExportRasterSnapshot {
        uid,
        group_uid: None,
        visible,
        opacity,
        transform: TransformRec {
            cx: center[0],
            cy: center[1],
            rotation: 0.0,
            scale: 1.0,
        },
        deform: None,
        rgba: px.repeat(size * size),
        size_px: [size, size],
        mask_clip_enabled: false,
    };
    Banded { snap, band }
}

/// An undeformed, unrotated, unscaled, visible, ungrouped page-0 overlay snapshot (direct-blit path)
/// of a solid `px` square of `size` px centered at `center`, with a `PinnedText` band at `band_z`.
fn overlay_snapshot(uid: &str, center: [f32; 2], size: usize, px: [u8; 4], band_z: u32, mask_clip_enabled: bool) -> Banded<TypingExportOverlaySnapshot> {
    let band = Band::PinnedText { uid: uid.to_string(), z: band_z };
    let snap = TypingExportOverlaySnapshot {
        page_idx: 0,
        center_page_px: center,
        mask_clip_enabled,
        layer_idx: 0,
        user_scale: 1.0,
        angle_deg: 0.0,
        deform_mesh: None,
        size_px: [size, size],
        source_rgba: px.repeat(size * size),
        render_data_json: None,
        uid: uid.to_string(),
        group_uid: None,
        visible: true,
    };
    Banded { snap, band }
}

/// A page-0 PNG export job over `page_path` with no clean overlay, no PS groups and no font index;
/// its `bands` are the bands of the given rasters and overlays.
fn export_job(
    page_path: PathBuf,
    overlays: Vec<Banded<TypingExportOverlaySnapshot>>,
    rasters: Vec<Banded<TypingExportRasterSnapshot>>,
    mask: Option<TypingMaskExportPage>,
    layers_primary_dir: Option<PathBuf>,
) -> TypingExportPageJob {
    let mut bands: Vec<Band> = Vec::new();
    let rasters: Vec<TypingExportRasterSnapshot> = rasters
        .into_iter()
        .map(|banded| {
            bands.push(banded.band);
            banded.snap
        })
        .collect();
    let overlays: Vec<TypingExportOverlaySnapshot> = overlays
        .into_iter()
        .map(|banded| {
            bands.push(banded.band);
            banded.snap
        })
        .collect();
    TypingExportPageJob {
        page_idx: 0,
        page_path,
        output_path: None,
        clean_paths: None,
        clean_overlay_rgba: None,
        overlays,
        rasters,
        bands,
        groups: Vec::new(),
        mask,
        export_format: TypingExportFormat::Png,
        layers_primary_dir,
        layers_fallback_dir: None,
        font_post_script_names: Default::default(),
    }
}

/// Runs the flatten and returns the page's straight RGBA, asserting the page size is unchanged.
fn flatten(job: &TypingExportPageJob) -> Vec<u8> {
    let (rgba, w, h) = match flatten_typing_export_page_rgba(job) {
        Ok(out) => out,
        Err(err) => panic!("flatten failed: {err}"),
    };
    assert_eq!([w, h], [PAGE_PX, PAGE_PX], "the flatten keeps the source page size");
    rgba
}

/// The straight-RGBA pixel at `(x, y)` of a flattened `PAGE_PX`-square page.
fn probe(rgba: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * PAGE_PX + x) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

const BLACK: [u8; 4] = [0, 0, 0, 255];
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];

#[test]
fn flatten_interleaves_two_rasters_and_an_overlay_by_band_z() {
    // (a) Rasters at z 0 and 2, the overlay at z 1 between them. The z=2 raster is listed FIRST in the
    // snapshot, so the result proves band Z (not snapshot order) decides the stack.
    let dir = ScratchDir::new("a");
    let page = write_source_page(&dir, BLACK);
    let red_low = raster_snapshot([8.0, 10.0], 10, RED, 0, true, 1.0); // x 3..13, y 5..15
    let blue_top = raster_snapshot([12.0, 10.0], 4, BLUE, 2, true, 1.0); // x 10..14, y 8..12
    let green_mid = overlay_snapshot("g", [10.0, 10.0], 6, GREEN, 1, false); // x 7..13, y 7..13
    let rgba = flatten(&export_job(page, vec![green_mid], vec![blue_top, red_low], None, None));
    assert_eq!(probe(&rgba, 4, 10), RED, "only the bottom raster covers x=4");
    assert_eq!(probe(&rgba, 8, 10), GREEN, "the z=1 overlay sits above the z=0 raster");
    assert_eq!(probe(&rgba, 11, 10), BLUE, "the z=2 raster sits above the z=1 overlay");
    assert_eq!(probe(&rgba, 16, 10), BLACK, "nothing covers x=16");
}

#[test]
fn flatten_draws_an_overlay_above_a_raster_at_the_same_band_z() {
    // (b) Equal Z => overlay above raster; a higher-Z raster still covers the overlay.
    let dir = ScratchDir::new("b");
    let page = write_source_page(&dir, BLACK);
    let tie = flatten(&export_job(
        page.clone(),
        vec![overlay_snapshot("g", [10.0, 10.0], 6, GREEN, 3, false)],
        vec![raster_snapshot([10.0, 10.0], 10, RED, 3, true, 1.0)],
        None,
        None,
    ));
    assert_eq!(probe(&tie, 10, 10), GREEN, "equal band Z: the overlay draws above the raster");
    assert_eq!(probe(&tie, 6, 10), RED, "the raster shows where the overlay does not reach");

    let raster_above = flatten(&export_job(
        page,
        vec![overlay_snapshot("g", [10.0, 10.0], 6, GREEN, 3, false)],
        vec![raster_snapshot([10.0, 10.0], 10, RED, 4, true, 1.0)],
        None,
        None,
    ));
    assert_eq!(probe(&raster_above, 10, 10), RED, "a higher-Z raster covers the overlay");
}

#[test]
fn flatten_skips_an_invisible_raster_and_scales_alpha_by_raster_opacity() {
    // (c) A hidden green raster on top is absent; a 0.5-opacity red raster under it blends over the
    // opaque black page: alpha round(255 * 0.5) = 128 => red channel 128.
    let dir = ScratchDir::new("c");
    let page = write_source_page(&dir, BLACK);
    let rgba = flatten(&export_job(
        page,
        Vec::new(),
        vec![
            raster_snapshot([10.0, 10.0], 10, RED, 0, true, 0.5),
            raster_snapshot([10.0, 10.0], 10, GREEN, 1, false, 1.0),
        ],
        None,
        None,
    ));
    assert_eq!(probe(&rgba, 10, 10), [128, 0, 0, 255], "half-opacity red over black, hidden green absent");
    assert_eq!(probe(&rgba, 1, 1), BLACK, "untouched page pixel");
}

#[test]
fn flatten_clips_a_mask_clip_overlay_to_the_page_mask() {
    // (d) Mask active only on the left half (x < 10). A mask-clipped overlay keeps its left half; the
    // same overlay without mask clip covers both halves.
    let dir = ScratchDir::new("d");
    let page = write_source_page(&dir, BLACK);
    let mask = TypingMaskExportPage {
        width: PAGE_PX,
        height: PAGE_PX,
        data: (0..PAGE_PX * PAGE_PX).map(|i| if (i % PAGE_PX) < 10 { 255 } else { 0 }).collect(),
    };
    let clipped = flatten(&export_job(
        page.clone(),
        vec![overlay_snapshot("g", [10.0, 10.0], 10, GREEN, 0, true)], // x 5..15
        Vec::new(),
        Some(mask.clone()),
        None,
    ));
    assert_eq!(probe(&clipped, 7, 10), GREEN, "kept where the mask is active");
    assert_eq!(probe(&clipped, 13, 10), BLACK, "clipped where the mask is inactive");

    let unclipped = flatten(&export_job(
        page,
        vec![overlay_snapshot("g", [10.0, 10.0], 10, GREEN, 0, false)],
        Vec::new(),
        Some(mask),
        None,
    ));
    assert_eq!(probe(&unclipped, 13, 10), GREEN, "mask clip off: the overlay ignores the mask");
}

#[test]
fn flatten_without_raster_snapshot_or_layers_dir_keeps_overlay_snapshot_order() {
    // (e) `rasters` empty => the disk-band path. With `layers_primary_dir: None` there are no disk bands,
    // every overlay resolves to Z 0 and the job's in-memory bands are IGNORED: the stable sort keeps
    // snapshot order, so the LAST overlay ends on top even though its in-memory band-Z is lower.
    let dir = ScratchDir::new("e");
    let page = write_source_page(&dir, BLACK);
    let rgba = flatten(&export_job(
        page,
        vec![
            overlay_snapshot("first", [10.0, 10.0], 6, RED, 9, false),
            overlay_snapshot("second", [10.0, 10.0], 6, GREEN, 0, false),
        ],
        Vec::new(),
        None,
        None,
    ));
    assert_eq!(probe(&rgba, 10, 10), GREEN, "snapshot order wins: the second overlay is on top");
}

#[test]
fn flatten_keeps_the_alpha_of_a_translucent_source_page() {
    // (f) A translucent source keeps its exact bytes outside the overlay; under a half-transparent
    // overlay the result is source-over of straight RGBA (alpha 64 under alpha 128 => 160).
    let dir = ScratchDir::new("f");
    let source_px = [10, 20, 30, 64];
    let page = write_source_page(&dir, source_px);
    let rgba = flatten(&export_job(
        page,
        vec![overlay_snapshot("g", [10.0, 10.0], 4, [0, 255, 0, 128], 0, false)], // x 8..12
        Vec::new(),
        None,
        None,
    ));
    assert_eq!(probe(&rgba, 2, 2), source_px, "an uncovered pixel keeps the source bytes and alpha");
    assert_eq!(probe(&rgba, 9, 9), [2, 208, 6, 160], "source-over onto a translucent page");
}

#[test]
fn flatten_omits_a_raster_inside_a_hidden_group() {
    // B1 (NEW behaviour, `dev-docs/single_image_mode_plan.md` §3.2): the flatten honours PS group
    // visibility. Disk path (`rasters` empty): the page's only raster belongs to a hidden group, so the
    // page stays black. Today the group is ignored and the raster is composited.
    use ms_models::layer_model::persist;
    let dir = ScratchDir::new("b1");
    let page = write_source_page(&dir, BLACK);
    let layers = dir.path.join("layers");
    if let Err(err) = std::fs::create_dir_all(&layers) {
        panic!("could not create {}: {err}", layers.display());
    }
    let red = ColorImage::filled([10, 10], Color32::from_rgba_unmultiplied(255, 0, 0, 255));
    let raster = persist::RasterLayerOut {
        uid: "r0".into(),
        name: "R".into(),
        visible: true,
        opacity: 1.0,
        transform: TransformRec {
            cx: 10.0,
            cy: 10.0,
            rotation: 0.0,
            scale: 1.0,
        },
        deform: None,
        group_uid: Some("g1".into()),
        image: Some(&red),
        image_size: [10, 10],
        pixels_dirty: true,
        mask_clip: None,
    };
    let group = persist::GroupMeta {
        uid: "g1".into(),
        name: "G".into(),
        visible: false,
        opacity: 1.0,
        collapsed: false,
    };
    if let Err(err) = persist::save_page_rasters(&layers, 0, &[raster], &[group], &[]) {
        panic!("could not seed the hidden-group raster: {err}");
    }
    let rgba = flatten(&export_job(page, Vec::new(), Vec::new(), None, Some(layers)));
    assert_eq!(probe(&rgba, 10, 10), BLACK, "a raster in a hidden group is not composited");
}

/// A PS unified group with the given visibility and opacity.
fn group(uid: &str, visible: bool, opacity: f32) -> ms_models::layer_model::persist::GroupMeta {
    ms_models::layer_model::persist::GroupMeta {
        uid: uid.into(),
        name: uid.into(),
        visible,
        opacity,
        collapsed: false,
    }
}

#[test]
fn flatten_dims_a_text_inside_a_dimmed_group() {
    // B1: a group at opacity 0.5 dims its text: alpha round(255 * 0.5) = 128 over the opaque black page.
    // Overlay-only job => disk path without a layers dir, so the job's in-memory groups fold the text.
    let dir = ScratchDir::new("grp_text_dim");
    let page = write_source_page(&dir, BLACK);
    let mut text = overlay_snapshot("g", [10.0, 10.0], 6, GREEN, 0, false); // x 7..13
    text.snap.group_uid = Some("dim".into());
    let mut job = export_job(page, vec![text], Vec::new(), None, None);
    job.groups = vec![group("dim", true, 0.5)];
    let rgba = flatten(&job);
    assert_eq!(probe(&rgba, 10, 10), [0, 128, 0, 255], "the text is drawn at the group's opacity");
    assert_eq!(probe(&rgba, 2, 2), BLACK, "untouched page pixel");
}

#[test]
fn flatten_omits_a_text_inside_a_hidden_group_or_with_a_hidden_node() {
    // B1: a hidden group hides its text; a text whose own doc node is hidden is omitted too. An
    // unknown group uid folds as visible / opacity 1.
    let dir = ScratchDir::new("grp_text_hidden");
    let page = write_source_page(&dir, BLACK);
    let mut grouped = overlay_snapshot("grouped", [5.0, 10.0], 4, GREEN, 0, false); // x 3..7
    grouped.snap.group_uid = Some("hidden".into());
    let mut node_hidden = overlay_snapshot("node_hidden", [10.0, 10.0], 4, RED, 1, false); // x 8..12
    node_hidden.snap.visible = false;
    let mut stray = overlay_snapshot("stray", [15.0, 10.0], 4, BLUE, 2, false); // x 13..17
    stray.snap.group_uid = Some("no-such-group".into());
    let mut job = export_job(page, vec![grouped, node_hidden, stray], Vec::new(), None, None);
    job.groups = vec![group("hidden", false, 1.0)];
    let rgba = flatten(&job);
    assert_eq!(probe(&rgba, 5, 10), BLACK, "a text in a hidden group is not composited");
    assert_eq!(probe(&rgba, 10, 10), BLACK, "a text whose node is hidden is not composited");
    assert_eq!(probe(&rgba, 15, 10), BLUE, "an unknown group folds as visible, opacity 1");
}

#[test]
fn flatten_folds_group_opacity_into_snapshot_raster_opacity() {
    // B1 on the snapshot path: raster opacity 0.5 inside a 0.5 group => alpha round(255 * 0.25) = 64;
    // a full-opacity raster inside a hidden group is absent.
    let dir = ScratchDir::new("grp_raster");
    let page = write_source_page(&dir, BLACK);
    let mut dimmed = raster_snapshot([5.0, 10.0], 4, RED, 0, true, 0.5); // x 3..7
    dimmed.snap.group_uid = Some("half".into());
    let mut hidden = raster_snapshot([15.0, 10.0], 4, GREEN, 1, true, 1.0); // x 13..17
    hidden.snap.group_uid = Some("off".into());
    let mut job = export_job(page, Vec::new(), vec![dimmed, hidden], None, None);
    job.groups = vec![group("half", true, 0.5), group("off", false, 1.0)];
    let rgba = flatten(&job);
    assert_eq!(probe(&rgba, 5, 10), [64, 0, 0, 255], "item and group opacity multiply");
    assert_eq!(probe(&rgba, 15, 10), BLACK, "a raster in a hidden group is not composited");
}

#[test]
fn flatten_omits_a_layer_whose_group_opacity_is_nan() {
    // A NaN group opacity is never a panic: the plan omits the layer (and the flatten logs it).
    let dir = ScratchDir::new("grp_nan");
    let page = write_source_page(&dir, BLACK);
    let mut raster = raster_snapshot([10.0, 10.0], 4, RED, 0, true, 1.0);
    raster.snap.group_uid = Some("nan".into());
    let mut job = export_job(page, Vec::new(), vec![raster], None, None);
    job.groups = vec![group("nan", true, f32::NAN)];
    let rgba = flatten(&job);
    assert_eq!(probe(&rgba, 10, 10), BLACK, "a NaN-opacity group leaves its layer out");
}

#[test]
fn raster_band_z_matches_by_uid_else_top_of_stack() {
    let mut layer = TypingTextOverlayLayer::default();
    assert_eq!(layer.raster_band_z(0, "r"), 0, "a page without bands => 0");
    layer.bands_by_page.insert(0, Vec::new());
    assert_eq!(layer.raster_band_z(0, "r"), 0, "empty bands => bands.len() == 0");
    layer.bands_by_page.insert(
        0,
        vec![
            Band::Raster { uid: "r".into(), z: 4 },
            Band::PinnedText { uid: "t".into(), z: 7 },
            Band::Raster { uid: "q".into(), z: 1 },
        ],
    );
    assert_eq!(layer.raster_band_z(0, "r"), 4);
    assert_eq!(layer.raster_band_z(0, "q"), 1);
    assert_eq!(layer.raster_band_z(0, "unknown"), 3, "unknown uid => bands.len()");
    assert_eq!(layer.raster_band_z(0, "t"), 3, "a text band with the uid does not match a raster");
    assert_eq!(layer.raster_band_z(1, "r"), 0, "bands of another page are not consulted");
}

#[test]
fn overlay_band_z_matches_pinned_uid_first_then_text_group_else_top() {
    let mut layer = TypingTextOverlayLayer::default();
    assert_eq!(layer.overlay_band_z(0, "t", 0), 0, "a page without bands => 0");
    layer.bands_by_page.insert(0, Vec::new());
    assert_eq!(layer.overlay_band_z(0, "t", 0), 0, "empty bands => bands.len() == 0");
    layer.bands_by_page.insert(
        0,
        vec![
            Band::TextGroup {
                layer_idx: 2,
                z: 1,
                member_uids: vec!["m".into()],
            },
            Band::PinnedText { uid: "t".into(), z: 5 },
            Band::Raster { uid: "r".into(), z: 6 },
        ],
    );
    assert_eq!(layer.overlay_band_z(0, "t", 2), 5, "a pinned uid wins over a matching text group");
    assert_eq!(layer.overlay_band_z(0, "m", 2), 1, "no pinned band => the group of its layer_idx");
    assert_eq!(
        layer.overlay_band_z(0, "stranger", 2),
        1,
        "the group lookup is by layer_idx only, membership is not checked"
    );
    assert_eq!(layer.overlay_band_z(0, "m", 0), 3, "no pinned band, no group => bands.len()");
    assert_eq!(layer.overlay_band_z(0, "r", 0), 3, "a raster band with the uid does not match a text");
}

/// A text runtime for the snapshot tests: `size` x `size` px of opaque white on page `page_idx`, its
/// rgba buffer length overridden by `rgba_len` when given (to model a malformed buffer).
fn text_runtime(uid: &str, page_idx: usize, size: [usize; 2], rgba_len: Option<usize>) -> TypingOverlayRuntime {
    let len = rgba_len.unwrap_or(size[0] * size[1] * 4);
    text_runtime_from_doc_node(uid, page_idx, [10.0, 10.0], 1.0, 0.0, None, false, false, 0, None, size, vec![255; len])
}

#[test]
fn export_overlay_snapshots_group_by_page_skip_malformed_and_stable_sort_by_band_z() {
    let mut layer = TypingTextOverlayLayer::default();
    layer.bands_by_page.insert(
        0,
        vec![
            Band::PinnedText { uid: "b".into(), z: 1 },
            Band::PinnedText { uid: "a".into(), z: 5 },
        ],
    );
    layer.overlays = vec![
        text_runtime("a", 0, [2, 2], None),
        text_runtime("u1", 0, [2, 2], None),
        text_runtime("p1", 1, [2, 2], None),
        text_runtime("b", 0, [2, 2], None),
        text_runtime("zero", 0, [0, 2], None),
        text_runtime("u2", 0, [2, 2], None),
        text_runtime("bad", 0, [2, 2], Some(3)),
    ];
    let snapshots = layer.build_export_overlay_snapshots();
    let mut pages: Vec<usize> = snapshots.keys().copied().collect();
    pages.sort_unstable();
    assert_eq!(pages, vec![0, 1], "one entry per page with overlays");
    let band_z_of = |page: usize, s: &TypingExportOverlaySnapshot| layer.overlay_band_z(page, &s.uid, s.layer_idx);
    let page0: Vec<(&str, u32)> = snapshots[&0].iter().map(|s| (s.uid.as_str(), band_z_of(0, s))).collect();
    // Unknown uids take `bands.len()` (2) and keep their `self.overlays` order among themselves; the
    // zero-size and wrong-length overlays are skipped.
    assert_eq!(page0, vec![("b", 1), ("u1", 2), ("u2", 2), ("a", 5)]);
    let page1: Vec<(&str, u32)> = snapshots[&1].iter().map(|s| (s.uid.as_str(), band_z_of(1, s))).collect();
    assert_eq!(page1, vec![("p1", 0)], "a page without bands => band_z 0");
}

/// The bottom-to-top fill order of `merged_fill_plan` for ungrouped, visible, opaque rasters at
/// `raster_zs` and overlays at `overlay_zs` (each item owns a band at its Z). Keeps the WP-0.1
/// `merged_fill_order(raster_zs, overlay_zs)` table expressible after the owner took the ordering.
fn fill_order(raster_zs: &[u32], overlay_zs: &[u32]) -> Vec<MergedFillItem> {
    use ms_models::layer_model::ordering::CompositeItem;
    let raster_uids: Vec<String> = (0..raster_zs.len()).map(|i| format!("r{i}")).collect();
    let overlay_uids: Vec<String> = (0..overlay_zs.len()).map(|i| format!("o{i}")).collect();
    let mut bands: Vec<Band> = Vec::new();
    for (uid, z) in raster_uids.iter().zip(raster_zs) {
        bands.push(Band::Raster { uid: uid.clone(), z: *z });
    }
    for (uid, z) in overlay_uids.iter().zip(overlay_zs) {
        bands.push(Band::PinnedText { uid: uid.clone(), z: *z });
    }
    let rasters: Vec<CompositeItem<'_>> =
        raster_uids.iter().map(|uid| raster_composite_item(uid, None, true, 1.0)).collect();
    let overlays: Vec<CompositeItem<'_>> =
        overlay_uids.iter().map(|uid| text_composite_item(uid, 0, None, true)).collect();
    let plan = merged_fill_plan(&bands, &[], &rasters, &overlays);
    assert!(plan.iter().all(|step| step.opacity == 1.0), "opaque ungrouped items keep opacity 1.0");
    plan.into_iter().map(|step| step.item).collect()
}

#[test]
fn merged_fill_order_sorts_by_band_z_raster_below_overlay_stable_within_kind() {
    use MergedFillItem::{Overlay, Raster};
    assert_eq!(fill_order(&[], &[]), Vec::new());
    assert_eq!(fill_order(&[2, 0], &[1]), vec![Raster(1), Overlay(0), Raster(0)]);
    assert_eq!(
        fill_order(&[3, 3], &[3, 3]),
        vec![Raster(0), Raster(1), Overlay(0), Overlay(1)],
        "equal Z: rasters first, input order kept within each kind"
    );
    assert_eq!(fill_order(&[5], &[4, 6]), vec![Overlay(0), Raster(0), Overlay(1)]);
    assert_eq!(fill_order(&[], &[2, 1, 2]), vec![Overlay(1), Overlay(0), Overlay(2)]);
}

#[test]
fn merged_fill_plan_folds_group_opacity_and_omits_hidden_group_items() {
    use ms_models::layer_model::ordering::GroupFold;
    use MergedFillItem::{Overlay, Raster};
    let bands = vec![
        Band::Raster { uid: "r_dim".into(), z: 0 },
        Band::Raster { uid: "r_hidden".into(), z: 1 },
        Band::PinnedText { uid: "t_dim".into(), z: 2 },
        Band::PinnedText { uid: "t_hidden".into(), z: 3 },
    ];
    let groups = [
        GroupFold { uid: "dim", visible: true, opacity: 0.5 },
        GroupFold { uid: "hidden", visible: false, opacity: 1.0 },
    ];
    let rasters = [
        raster_composite_item("r_dim", Some("dim"), true, 1.0),
        raster_composite_item("r_hidden", Some("hidden"), true, 1.0),
    ];
    let overlays = [
        text_composite_item("t_dim", 0, Some("dim"), true),
        text_composite_item("t_hidden", 0, Some("hidden"), true),
    ];
    assert_eq!(
        merged_fill_plan(&bands, &groups, &rasters, &overlays),
        vec![
            MergedFillStep { item: Raster(0), opacity: 0.5 },
            MergedFillStep { item: Overlay(0), opacity: 0.5 },
        ],
        "a dimmed group yields step opacity 0.5; hidden-group items are left out"
    );
}

#[test]
fn composite_step_tint_is_a_premultiplied_white_over_all_four_channels() {
    assert_eq!(composite_step_tint(1.0), Color32::WHITE);
    assert_eq!(composite_step_tint(0.5).to_array(), [128, 128, 128, 128]);
    assert_eq!(composite_step_tint(0.0), Color32::TRANSPARENT);
    assert_eq!(composite_step_tint(f32::NAN), Color32::TRANSPARENT);
    assert_eq!(composite_step_tint(2.0), Color32::WHITE, "above 1.0 saturates");
}

/// A visible, opaque, untransformed `size` x `size` raster layer with node uid `uid` (no texture).
fn raster_runtime(uid: &str, size: usize, group_uid: Option<&str>) -> TypingRasterLayer {
    TypingRasterLayer {
        uid: uid.into(),
        name: uid.into(),
        visible: true,
        opacity: 1.0,
        transform: TransformRec {
            cx: 10.0,
            cy: 10.0,
            rotation: 0.0,
            scale: 1.0,
        },
        image: ColorImage::filled([size, size], Color32::WHITE),
        base_file: String::new(),
        effects: Vec::new(),
        deform: None,
        mask_clip_enabled: false,
        group_uid: group_uid.map(str::to_owned),
        clipped_image: None,
        texture: None,
    }
}

/// A page-0 layer with a raster `r` at band Z `raster_z` and a text `t` at band Z `text_z`, both
/// covering the page centre, plus a hidden PS group `hidden` (no member yet).
fn raster_and_text_page(ctx: &egui::Context, raster_z: u32, text_z: u32) -> TypingTextOverlayLayer {
    let mut layer = TypingTextOverlayLayer::default();
    layer.bands_by_page.insert(
        0,
        vec![
            Band::Raster { uid: "r".into(), z: raster_z },
            Band::PinnedText { uid: "t".into(), z: text_z },
        ],
    );
    layer.groups_by_page.insert(0, vec![group("hidden", false, 1.0)]);
    layer.raster_layers_by_page.insert(0, vec![raster_runtime("r", 8, None)]);
    let mut text = text_runtime("t", 0, [4, 4], None);
    text.texture = Some(ctx.load_texture("hit_t", ColorImage::filled([4, 4], Color32::WHITE), egui::TextureOptions::LINEAR));
    layer.overlays = vec![text];
    layer
}

/// The unified canvas hit-test of `layer`'s page 0 at the page centre, as `interact_page_rasters`
/// composes it: the topmost kept raster's band Z against the topmost kept overlay's.
fn unified_hit(layer: &TypingTextOverlayLayer) -> TypingPointerTarget {
    let view = PageView {
        page_idx: 0,
        image_rect: Rect::from_min_size(Pos2::ZERO, Vec2::splat(20.0)),
        zoom: 1.0,
    };
    let pointer = Some(Pos2::new(10.0, 10.0));
    let flags = layer.composited_raster_flags(0);
    let raster_z = layer.raster_layers_by_page.get(&0).and_then(|rasters| {
        rasters
            .iter()
            .enumerate()
            .rev()
            .find(|(i, _)| flags.get(*i).copied().unwrap_or(false))
            .map(|(_, raster)| layer.raster_band_z(0, &raster.uid))
    });
    let overlay_z = layer.topmost_overlay_at(view, pointer).map(|(_, z)| z);
    unified_topmost_pointer_target(overlay_z, raster_z)
}

#[test]
fn hidden_group_overlay_above_a_raster_is_not_hit() {
    let ctx = egui::Context::default();
    let mut layer = raster_and_text_page(&ctx, 0, 1);
    assert_eq!(unified_hit(&layer), TypingPointerTarget::Overlay, "visible text above the raster wins");
    layer.overlays[0].group_uid = Some("hidden".into());
    assert_eq!(unified_hit(&layer), TypingPointerTarget::Raster, "hidden-group text falls through to the raster");
    let plan = layer.page_fill_plan(0, &[0]);
    assert_eq!(
        plan.iter().map(|step| step.item).collect::<Vec<_>>(),
        vec![MergedFillItem::Raster(0)],
        "the hidden-group text is not drawn either"
    );
}

#[test]
fn hidden_group_raster_above_a_text_is_not_hit() {
    let ctx = egui::Context::default();
    let mut layer = raster_and_text_page(&ctx, 1, 0);
    assert_eq!(layer.composited_raster_flags(0), vec![true]);
    assert_eq!(unified_hit(&layer), TypingPointerTarget::Raster, "visible raster above the text wins");
    if let Some(raster) = layer.raster_layers_by_page.get_mut(&0).and_then(|rasters| rasters.first_mut()) {
        raster.group_uid = Some("hidden".into());
    }
    assert_eq!(layer.composited_raster_flags(0), vec![false], "the hidden-group raster is not composited");
    assert_eq!(unified_hit(&layer), TypingPointerTarget::Overlay, "hidden-group raster falls through to the text");
    let plan = layer.page_fill_plan(0, &[0]);
    assert_eq!(
        plan.iter().map(|step| step.item).collect::<Vec<_>>(),
        vec![MergedFillItem::Overlay(0)],
        "the hidden-group raster is not drawn either"
    );
}

#[test]
fn page_fill_plan_dims_a_raster_in_a_dimmed_group() {
    let ctx = egui::Context::default();
    let mut layer = raster_and_text_page(&ctx, 0, 1);
    layer.groups_by_page.insert(0, vec![group("dim", true, 0.5)]);
    if let Some(raster) = layer.raster_layers_by_page.get_mut(&0).and_then(|rasters| rasters.first_mut()) {
        raster.group_uid = Some("dim".into());
        raster.opacity = 0.5;
    }
    assert_eq!(
        layer.page_fill_plan(0, &[0]),
        vec![
            MergedFillStep { item: MergedFillItem::Raster(0), opacity: 0.25 },
            MergedFillStep { item: MergedFillItem::Overlay(0), opacity: 1.0 },
        ],
        "raster opacity 0.5 x group 0.5; the ungrouped text stays opaque"
    );
}

#[test]
fn topmost_overlay_at_skips_overlays_the_composite_plan_omits() {
    // Two overlapping texts on page 0: the higher one (z 1) sits in a hidden PS group, so the lower one
    // (z 0) is the topmost HIT; once its own doc node is hidden as well, nothing is hit.
    let ctx = egui::Context::default();
    let mut layer = TypingTextOverlayLayer::default();
    layer.bands_by_page.insert(
        0,
        vec![
            Band::PinnedText { uid: "low".into(), z: 0 },
            Band::PinnedText { uid: "high".into(), z: 1 },
        ],
    );
    layer.groups_by_page.insert(0, vec![group("hidden", false, 1.0)]);
    let mut low = text_runtime("low", 0, [4, 4], None);
    let mut high = text_runtime("high", 0, [4, 4], None);
    high.group_uid = Some("hidden".into());
    for runtime in [&mut low, &mut high] {
        runtime.texture = Some(ctx.load_texture(
            format!("hit_{}", runtime.uid),
            ColorImage::filled([4, 4], Color32::WHITE),
            egui::TextureOptions::LINEAR,
        ));
    }
    layer.overlays = vec![low, high];
    let view = PageView {
        page_idx: 0,
        image_rect: Rect::from_min_size(Pos2::ZERO, Vec2::splat(20.0)),
        zoom: 1.0,
    };
    let pointer = Some(Pos2::new(10.0, 10.0));
    assert_eq!(layer.topmost_overlay_at(view, pointer), Some((0, 0)), "the hidden-group text is not hit");
    layer.overlays[0].visible = false;
    assert_eq!(layer.topmost_overlay_at(view, pointer), None, "a hidden text node is not hit");
    layer.overlays[1].group_uid = None;
    assert_eq!(layer.topmost_overlay_at(view, pointer), Some((1, 1)), "ungrouped again: the higher text wins");
}
