/*
File: tab/helpers.rs

Purpose:
Free-function helpers for the typing tab: selection/page geometry resolution,
bubble seed-text selection, overlay runtime materialization/merge, and the
overlay/layout image file IO and legacy `text_info.json` loader routines.

Main responsibilities:
- resolve drawn selections to a page and compute page-pixel geometry;
- pick and prefer seed text for a selection from nearby bubble anchors;
- build overlay runtimes from doc nodes and decoded legacy entries, and merge
  freshly-loaded legacy overlays into an existing set;
- name/save overlay and adjacent layout images, read RGBA from file/clipboard,
  and load typing overlays/page sizes from a directory.

Notes:
Extracted verbatim from `tab.rs`. Free fns are `pub(super)` so `tab.rs` and
sibling submodules of `tab` can use them. `use super::*;` pulls in the parent
module's types and imports.
*/

use super::*;

pub(super) fn contains_any_page(canvas: &CanvasView, project: &ProjectData, pos: Pos2) -> bool {
    project.pages.iter().any(|page| {
        canvas
            .page_scene_rect(page.idx)
            .map(|rect| rect.contains(pos))
            .unwrap_or(false)
    })
}

pub(super) fn viewport_center_page_px_for_page(
    canvas_rect: Rect,
    canvas: &CanvasView,
    project: &ProjectData,
) -> [f32; 2] {
    let current_idx = canvas.current_page_idx();
    let page_rect = canvas.page_scene_rect(current_idx).or_else(|| {
        project
            .pages
            .first()
            .and_then(|p| canvas.page_scene_rect(p.idx))
    });
    let Some(page_rect) = page_rect else {
        return [0.5, 0.5];
    };
    if !page_rect.is_positive() {
        return [0.5, 0.5];
    }
    let center_scene = canvas_rect.center();
    let clamped_scene = Pos2::new(
        center_scene.x.clamp(page_rect.left(), page_rect.right()),
        center_scene.y.clamp(page_rect.top(), page_rect.bottom()),
    );
    page_px_from_scene(page_rect, canvas.zoom(), clamped_scene)
}

pub(super) fn resolve_selection_to_page(
    canvas: &CanvasView,
    project: &ProjectData,
    selection_rect: Rect,
) -> Option<(usize, Rect, Rect)> {
    let mut best_area = 0.0_f32;
    let mut best_page: Option<(usize, Rect)> = None;

    for page in &project.pages {
        let Some(page_rect) = canvas.page_scene_rect(page.idx) else {
            continue;
        };
        let intersection = page_rect.intersect(selection_rect);
        if !intersection.is_positive() {
            continue;
        }
        let area = intersection.width() * intersection.height();
        if area > best_area {
            best_area = area;
            best_page = Some((page.idx, page_rect));
        }
    }

    let (page_idx, page_rect) = best_page?;
    let scene_rect = selection_rect.intersect(page_rect);
    if !scene_rect.is_positive() {
        return None;
    }
    Some((page_idx, page_rect, scene_rect))
}

pub(super) fn selection_width_in_source_px(
    canvas: &CanvasView,
    page_idx: usize,
    page_rect: Rect,
    scene_rect: Rect,
) -> u32 {
    if !page_rect.is_positive() || !scene_rect.is_positive() {
        return 0;
    }

    let source_w = canvas
        .overlay_size(page_idx)
        .map(|size| size[0] as f32)
        .unwrap_or_else(|| {
            let zoom = canvas.state.zoom.max(f32::EPSILON);
            (page_rect.width() / zoom).max(1.0)
        });
    let ratio = (scene_rect.width() / page_rect.width().max(1.0)).clamp(0.0, 1.0);
    (source_w * ratio).round().max(1.0) as u32
}

pub(super) fn selection_center_page_px(page_rect: Rect, scene_rect: Rect, zoom: f32) -> [f32; 2] {
    page_px_from_scene(page_rect, zoom, scene_rect.center())
}

pub(super) fn is_font_family_bound(ctx: &egui::Context, family: &egui::FontFamily) -> bool {
    ctx.fonts(|fonts| fonts.definitions().families.contains_key(family))
}

/// Picks the seed text for a freshly drawn typing selection from the bubble anchor closest to the
/// selection center whose anchor falls inside the selection rectangle.
///
/// A multi-area `ImageBubble` is a single `Bubble` in the data model but splits into one read-only
/// aside per text area, each with its own anchor. To match what the user sees, every image text
/// area is treated as an independent anchor candidate here; a plain text bubble contributes its one
/// `img_u`/`img_v` anchor. Returns `None` when no eligible anchor with non-empty text overlaps.
pub(super) fn pick_bubble_text_for_selection(
    bubbles: &[Bubble],
    page_idx: usize,
    scene_rect: Rect,
    page_rect: Rect,
) -> Option<String> {
    let selection_center = scene_rect.center();
    let mut best: Option<(f32, String)> = None;

    let mut consider = |anchor_uv: (f32, f32), text: String| {
        if text.is_empty() {
            return;
        }
        let anchor_pos = scene_from_uv(page_rect, anchor_uv.0, anchor_uv.1);
        if !scene_rect.contains(anchor_pos) {
            return;
        }
        let dist_sq = selection_center.distance_sq(anchor_pos);
        let should_replace = best
            .as_ref()
            .is_none_or(|(best_dist, _)| dist_sq < *best_dist);
        if should_replace {
            best = Some((dist_sq, text));
        }
    };

    for bubble in bubbles.iter().filter(|bubble| bubble.img_idx == page_idx) {
        // Image bubbles expose one anchor per text area (matching the split read-only asides); text
        // bubbles expose a single anchor at `img_u`/`img_v`.
        let areas = parse_image_text_areas(bubble);
        if areas.is_empty() {
            consider(
                (bubble.img_u, bubble.img_v),
                preferred_bubble_seed_text(bubble),
            );
        } else {
            for area in &areas {
                consider(
                    (area.anchor.x, area.anchor.y),
                    preferred_area_seed_text(area),
                );
            }
        }
    }

    best.map(|(_, text)| text)
}

/// Seed text for a plain text bubble: the translation when present, otherwise the original.
pub(super) fn preferred_bubble_seed_text(bubble: &crate::project::Bubble) -> String {
    let translated = bubble.text.trim();
    if !translated.is_empty() {
        return translated.to_string();
    }
    bubble.original_text.trim().to_string()
}

/// Seed text for one image text area: the translation when present, otherwise the original. The
/// description is intentionally excluded so a selection never seeds editor text with a note.
pub(super) fn preferred_area_seed_text(area: &crate::canvas::ImageTextArea) -> String {
    let translated = area.translation.trim();
    if !translated.is_empty() {
        return translated.to_string();
    }
    area.original.trim().to_string()
}

/// Flattens a `ColorImage` into a row-major STRAIGHT (un-premultiplied) RGBA byte buffer (4 bytes/pixel),
/// the `source_rgba` layout every consumer expects. egui `Color32` stores PREMULTIPLIED bytes, so we use
/// `to_srgba_unmultiplied()` (NOT `to_array()`, which would return premultiplied). Every consumer of
/// `source_rgba` treats it as straight alpha — the display upload and effects/mask-clip paths feed it
/// back through `ColorImage::from_rgba_unmultiplied`, and the export composite blends it as straight — so
/// emitting premultiplied here would premultiply the text TWICE, darkening semi-transparent (antialiased
/// stroke) edges to gray.
pub(super) fn color_image_to_rgba(image: &ColorImage) -> Vec<u8> {
    let mut out = Vec::with_capacity(image.pixels.len() * 4);
    for px in &image.pixels {
        out.extend_from_slice(&px.to_srgba_unmultiplied());
    }
    out
}

/// Materializes a typing overlay runtime from a doc Text node's projected fields. `is_image` selects
/// the runtime image kind for placed-PNG overlays. Used by
/// `sync_from_doc` when a doc Text node has no local runtime (the migrated-chapter case, where the
/// legacy `text_info.json` loader populated nothing). The rendered-PNG `file_name` is reconstructed
/// deterministically from `page_idx`+`uid` via [`persist::text_image_file_name`] — the SAME name the
/// doc's text flush (`write_text_image`) writes — so a later placement-save/flush round-trips. Pure
/// (no egui), so it is unit-testable. The new runtime starts with no GPU texture and is stale, so the
/// caller queues it for upload.
#[allow(clippy::too_many_arguments)]
pub(super) fn text_runtime_from_doc_node(
    uid: &str,
    page_idx: usize,
    center_page_px: [f32; 2],
    user_scale: f32,
    angle_deg: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    mask_clip_enabled: bool,
    is_image: bool,
    layer_idx: usize,
    render_data_json: Option<Value>,
    size_px: [usize; 2],
    source_rgba: Vec<u8>,
) -> TypingOverlayRuntime {
    TypingOverlayRuntime {
        uid: uid.to_string(),
        kind: if is_image {
            TypingOverlayKind::Image
        } else {
            TypingOverlayKind::Text
        },
        page_idx,
        center_page_px,
        mask_clip_enabled,
        layer_idx,
        user_scale,
        angle_deg,
        deform_mesh,
        file_name: crate::models::layer_model::persist::text_image_file_name(page_idx, uid),
        original_file_name: None,
        render_data_json,
        size_px,
        source_rgba,
        // A doc-materialized overlay carries no text-center info (the doc image has none by design); it
        // is recomputed only on a re-render with the "Отладка центра" flag on.
        extra: RenderedTextExtraInfo::default(),
        texture: None,
        display_texture_stale: true,
        last_texture_used_frame: 0,
    }
}

/// Builds an overlay runtime from a freshly-decoded legacy `text_info.json` entry. Fresh runtimes
/// start with no GPU texture and are stale, so the caller queues them for upload.
pub(super) fn runtime_from_decoded(entry: TypingOverlayDecoded) -> TypingOverlayRuntime {
    TypingOverlayRuntime {
        uid: entry.uid,
        kind: entry.kind,
        page_idx: entry.page_idx,
        center_page_px: entry.center_page_px,
        mask_clip_enabled: entry.mask_clip_enabled,
        layer_idx: entry.layer_idx,
        user_scale: entry.user_scale,
        angle_deg: entry.angle_deg,
        deform_mesh: entry.deform_mesh,
        file_name: entry.file_name,
        original_file_name: entry.original_file_name,
        render_data_json: entry.render_data_json,
        size_px: entry.size_px,
        source_rgba: entry.rgba,
        // TEMPORARY debug-only: carry the decoded overlay's mean/median centers for the center markers.
        extra: entry.extra,
        texture: None,
        display_texture_stale: true,
        last_texture_used_frame: 0,
    }
}

/// MERGES freshly-loaded legacy overlays (`decoded`) INTO `existing` by `(uid, page_idx)` instead of
/// wholesale-replacing. CRITICAL for migrated chapters: their `text_info.json` is retired, so the loader
/// returns an EMPTY set; `sync_from_doc` may have already MATERIALIZED text runtimes from the doc on an
/// earlier frame (that path is not gated on `loading_rx`). A wholesale `self.overlays = decoded` would
/// then WIPE those doc-created runtimes the instant the loader completes → the user's intermittent
/// "text shows then vanishes" symptom. Merge semantics: a loaded entry whose (uid, page) already exists
/// REPLACES that entry (legacy data is authoritative for a legacy chapter); a new one is APPENDED; an
/// existing runtime whose uid is ABSENT from the loaded set is KEPT (doc-created on a migrated chapter).
/// Cross-chapter reset is handled separately by `ensure_loader_started`, which clears `overlays` at the
/// START of a chapter open — so a stale chapter's overlays never linger; this merge only governs the
/// COMPLETION within one open. Returns the indices of entries that need a texture upload (replaced or
/// appended), so the caller can queue exactly those.
pub(super) fn merge_loaded_overlays(
    existing: &mut Vec<TypingOverlayRuntime>,
    decoded: Vec<TypingOverlayDecoded>,
) -> Vec<usize> {
    let mut touched: Vec<usize> = Vec::with_capacity(decoded.len());
    for entry in decoded {
        let runtime = runtime_from_decoded(entry);
        let idx = existing
            .iter()
            .position(|o| o.uid == runtime.uid && o.page_idx == runtime.page_idx);
        match idx {
            Some(i) => {
                existing[i] = runtime;
                touched.push(i);
            }
            None => {
                existing.push(runtime);
                touched.push(existing.len() - 1);
            }
        }
    }
    touched
}

/// Mints the in-session `file_name` handle for a newly created overlay, from its uid.
///
/// The name is `typing_overlay_p{page+1:04}_{uid}.png`. `uid` must be the overlay's own uid (a fresh
/// v4 UUID from the create worker), which makes uniqueness STRUCTURAL — no filesystem probe, and no
/// dependence on wall-clock resolution. (The previous shape stamped `unix_ms` and probed the dir to
/// disambiguate; the probe could not do that job once the create path stopped writing a PNG under
/// this name, since then nothing it looked for existed and two creates in one millisecond collided.)
///
/// This is a RUNTIME handle, not the overlay's persisted image name: the doc's text flush writes the
/// rendered pixels under its own uid-keyed `ps_p{page:04}_{uid}_text.png`, and a reloaded overlay's
/// `file_name` is rebuilt from the uid (`text_runtime_from_doc_node`). The only disk artifact keyed to
/// this name is the `CustomRasterLines` `_layout.png` sibling.
///
/// The `typing_overlay_p{page:04}_` prefix is preserved: `page_ops` documents it as the typing overlay
/// PNG shape in `text_images/`.
#[must_use]
pub(super) fn created_overlay_file_name(page_idx: usize, uid: &str) -> String {
    let page_token = page_idx.saturating_add(1);
    format!("typing_overlay_p{page_token:04}_{uid}.png")
}

pub(super) fn layout_image_file_name_for_overlay(file_name: &str) -> String {
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|raw| raw.to_str())
        .filter(|raw| !raw.is_empty())
        .unwrap_or(file_name);
    let extension = path
        .extension()
        .and_then(|raw| raw.to_str())
        .filter(|raw| !raw.is_empty())
        .unwrap_or("png");
    format!("{stem}{TEXT_LAYOUT_IMAGE_SUFFIX}.{extension}")
}

pub(super) fn render_params_with_adjacent_layout_path(
    text_images_dir: &Path,
    overlay_file_name: &str,
    render_params: &TextRenderParams,
) -> TextRenderParams {
    let mut out = render_params.clone();
    if out.text_layout_mode == TextLayoutMode::CustomRasterLines {
        out.drawn_lines_layout.image_path =
            Some(text_images_dir.join(layout_image_file_name_for_overlay(overlay_file_name)));
    }
    out
}

pub(super) fn save_drawn_lines_layout_image_if_needed(
    text_images_dir: &Path,
    overlay_file_name: &str,
    render_params: &TextRenderParams,
    width: u32,
    height: u32,
) -> Result<Option<PathBuf>, String> {
    if render_params.text_layout_mode != TextLayoutMode::CustomRasterLines {
        return Ok(None);
    }
    let pixel_count = usize::try_from(width)
        .ok()
        .and_then(|width_usize| {
            usize::try_from(height)
                .ok()
                .map(|height_usize| width_usize.saturating_mul(height_usize))
        })
        .ok_or_else(|| t!("typing.errors.layout_image_too_large").to_string())?;
    let layout_path = text_images_dir.join(layout_image_file_name_for_overlay(overlay_file_name));
    if layout_path.is_file() {
        return Ok(Some(layout_path));
    }
    let rgba = vec![0u8; pixel_count.saturating_mul(4)];
    image::save_buffer(
        &layout_path,
        rgba.as_slice(),
        width.max(1),
        height.max(1),
        image::ColorType::Rgba8,
    )
    .map_err(|err| {
        tf!(
            "typing.errors.save_layout_error",
            layout_path = layout_path.display(),
            err = err
        )
    })?;
    Ok(Some(layout_path))
}

pub(super) fn read_image_rgba_from_file(path: &Path) -> Result<(Vec<u8>, usize, usize), String> {
    let img = image::open(path)
        .map_err(|err| {
            tf!(
                "typing.errors.open_file_error",
                path = path.display(),
                err = err
            )
        })?
        .to_rgba8();
    let width = img.width() as usize;
    let height = img.height() as usize;
    Ok((img.into_raw(), width, height))
}

pub(super) fn read_image_rgba_from_clipboard() -> Result<(Vec<u8>, usize, usize), String> {
    let image = paste_image::read_image_from_clipboard()?;
    Ok((image.rgba, image.width, image.height))
}

pub(super) fn parse_effects_json_array(raw: &str) -> Vec<Value> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

pub(super) fn load_typing_page_sizes(
    page_paths: &[(usize, PathBuf)],
) -> HashMap<usize, [usize; 2]> {
    let mut out = HashMap::with_capacity(page_paths.len());
    for (page_idx, path) in page_paths {
        let size = image::image_dimensions(path)
            .ok()
            .map(|(w, h)| [w as usize, h as usize])
            .unwrap_or([1, 1]);
        out.insert(*page_idx, size);
    }
    out
}

// The cross-entry legacy migration (absolute ribbon `x`/`y`+`region_w`/`region_h`, top-left
// `u`/`v`) and its helpers now live in the shared `text_payload::migrate_overlay_entries` codec,
// so the typing loader and the doc loader normalize old chapters identically before per-entry
// decode. The former `overlay_entry_is_modern` / `legacy_overlay_page_*` / `legacy_overlay_png_size`
// / `migrate_legacy_text_overlays` here were removed.

pub(super) fn load_typing_overlays_from_dir(
    text_images_dir: &Path,
    fallback_dirs: &[&Path],
    page_sizes: &HashMap<usize, [usize; 2]>,
) -> Result<Vec<TypingOverlayDecoded>, String> {
    let text_info_path = text_images_dir.join(TEXT_INFO_FILE_NAME);
    if !text_info_path.is_file() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(&text_info_path).map_err(|err| {
        tf!(
            "typing.errors.read_text_info_error",
            text_info_path = text_info_path.display(),
            err = err
        )
    })?;
    let parsed: Value = serde_json::from_str(&raw).map_err(|err| {
        tf!(
            "typing.errors.parse_file_error",
            text_info_path = text_info_path.display(),
            err = err
        )
    })?;
    let Some(items) = parsed.as_array() else {
        return Err(tf!(
            "typing.errors.text_info_not_array_error",
            text_info_path = text_info_path.display()
        ));
    };

    // Migrate the cross-entry legacy placement families (absolute ribbon x/y, top-left u/v) up front
    // via the SHARED codec so the per-entry decode below — and the doc loader — see modern
    // center-anchored `img_idx`/`img_u`/`img_v`. The PNG footprint (top-left case) is resolved from the
    // text dirs (the model codec owns no image IO).
    let fallback_png_dir = fallback_dirs.first().copied();
    let migrated_items = crate::models::layer_model::text_payload::migrate_overlay_entries(
        items,
        page_sizes,
        |obj| {
            obj.get("file")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .and_then(|file| Path::new(file).file_name().and_then(|n| n.to_str()))
                .map(|name| {
                    let dims = image::image_dimensions(text_images_dir.join(name))
                        .ok()
                        .or_else(|| {
                            fallback_png_dir
                                .and_then(|d| image::image_dimensions(d.join(name)).ok())
                        });
                    match dims {
                        Some((w, h)) => (w as f32, h as f32),
                        None => (0.0, 0.0),
                    }
                })
                .unwrap_or((0.0, 0.0))
        },
    );

    let mut decoded_out = Vec::new();

    for item in migrated_items.iter() {
        let page_idx = item
            .as_object()
            .and_then(|obj| obj.get("img_idx"))
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        let page_size = page_sizes.get(&page_idx).copied().unwrap_or([1, 1]);
        let normalized = item
            .as_object()
            .and_then(|obj| normalize_overlay_storage_entry(obj, page_size))
            .unwrap_or_else(|| item.clone());

        if let Some(decoded) = normalized.as_object().and_then(|obj| {
            // Try the primary dir first, then each fallback in order — covering PNGs left in the
            // committed `layers/` dir or the legacy `text_images/` dir after a metadata migration.
            decode_overlay_from_storage_entry(text_images_dir, obj, page_size).or_else(|| {
                fallback_dirs
                    .iter()
                    .find_map(|d| decode_overlay_from_storage_entry(d, obj, page_size))
            })
        }) {
            decoded_out.push(decoded);
        }
    }

    // NOTE: `text_info.json` is now READ-ONLY legacy. The in-memory normalization above feeds the
    // session; it is NOT written back. The doc persists the overlays inline into `layers.json` on the
    // next flush, after which this legacy file is no longer read (the doc loads from the inline payload).
    Ok(decoded_out)
}
