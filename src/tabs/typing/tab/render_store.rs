/*
File: tab/render_store.rs

Purpose:
Worker-side render-and-store helpers for the typing tab: rendering created and
edited text/image overlays to disk, rendering raster effect chains, and building
the shape-variant preview grid (layout, checkerboard, and per-tile render).

Main responsibilities:
- render newly created text/image overlays and created rasters, persisting their
  PNGs / layer nodes;
- re-render edited text overlays and image-effect overlays to their staging files;
- render a raster's non-destructive effects chain from its base PNG;
- compute shape-variant grid geometry, paint its checkerboard, render its preview
  tiles, and build the apply payload for a chosen variant.

Notes:
Extracted verbatim from `tab.rs`. Free fns are `pub(super)` so `tab.rs` and
sibling submodules of `tab` can use them. `use super::*;` pulls in the parent
module's types and imports. `struct TypingEditImageEffectsRequest` stays in
`tab.rs` and is used here through `use super::*;`.
*/

use super::*;

pub(super) fn render_and_store_created_overlay(
    request: TypingCreateOverlayRequest,
) -> Result<TypingOverlayDecoded, String> {
    fs::create_dir_all(&request.text_images_dir).map_err(|err| {
        format!(
            "Не удалось создать папку {}: {err}",
            request.text_images_dir.display()
        )
    })?;
    let file_name = next_created_overlay_file_name(&request.text_images_dir, request.page_idx);
    let render_params = render_params_with_adjacent_layout_path(
        &request.text_images_dir,
        &file_name,
        &request.render_params,
    );
    let rendered = render_text_to_image(&render_params, request.font_provider.as_ref(), None).map_err(|err| {
        eprintln!(
            "ERROR typing::create_overlay_render layout={:?} shape={:?} wrap={:?} line_mode={:?} width_px={} page_idx={} err={}",
            render_params.text_layout_mode,
            render_params.text_shape,
            render_params.text_wrap_mode,
            render_params.text_line_mode,
            render_params.width_px,
            request.page_idx,
            err
        );
        err
    })?;
    if rendered.width == 0 || rendered.height == 0 {
        return Err("Рендер вернул изображение нулевого размера.".to_string());
    }

    let image_path = request.text_images_dir.join(&file_name);
    image::save_buffer(
        &image_path,
        rendered.rgba.as_slice(),
        rendered.width,
        rendered.height,
        image::ColorType::Rgba8,
    )
    .map_err(|err| format!("Не удалось сохранить {}: {err}", image_path.display()))?;
    let layout_image_path = save_drawn_lines_layout_image_if_needed(
        &request.text_images_dir,
        &file_name,
        &render_params,
        rendered.width,
        rendered.height,
    )?;

    // Для нового оверлея не подгоняем PNG под выделение: показываем в исходном масштабе.
    let user_scale = 1.0_f32;
    let overlay_uid = uuid::Uuid::new_v4().to_string();
    // Persistence is owned by the shared doc: the caller adds this overlay as a doc Text node and the
    // following placement save flushes the INLINE v3 payload to `layers.json`. The create path no
    // longer writes `text_info.json` (the doc is the sole text writer). The rendered PNG above is kept
    // on disk only as the create-job artifact; the doc flush writes its own uid-keyed `_text.png`.
    let _ = &layout_image_path;

    Ok(TypingOverlayDecoded {
        uid: overlay_uid,
        kind: TypingOverlayKind::Text,
        page_idx: request.page_idx,
        center_page_px: request.center_page_px,
        mask_clip_enabled: true,
        layer_idx: 0,
        user_scale,
        angle_deg: 0.0,
        deform_mesh: None,
        file_name,
        original_file_name: None,
        render_data_json: Some(request.render_data_json),
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
        warnings: rendered.warnings,
    })
}

// Superseded by `render_and_store_created_raster` (external images are now raster layers). Kept for
// reference / potential "insert image as overlay" path.
#[allow(dead_code)]
pub(super) fn render_and_store_created_image_overlay(
    request: TypingCreateImageOverlayRequest,
) -> Result<TypingOverlayDecoded, String> {
    let (rgba, width, height) = match request.source {
        TypingCreateImageSource::Clipboard => read_image_rgba_from_clipboard()?,
        TypingCreateImageSource::File(path) => read_image_rgba_from_file(path.as_path())?,
    };
    if width == 0 || height == 0 {
        return Err("Изображение нулевого размера.".to_string());
    }
    if rgba.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("Некорректный буфер RGBA изображения.".to_string());
    }

    fs::create_dir_all(&request.text_images_dir).map_err(|err| {
        format!(
            "Не удалось создать папку {}: {err}",
            request.text_images_dir.display()
        )
    })?;
    let file_name = next_created_overlay_file_name(&request.text_images_dir, request.page_idx);
    let image_path = request.text_images_dir.join(&file_name);
    image::save_buffer(
        &image_path,
        rgba.as_slice(),
        width as u32,
        height as u32,
        image::ColorType::Rgba8,
    )
    .map_err(|err| format!("Не удалось сохранить {}: {err}", image_path.display()))?;

    let render_data_json = default_render_data_for_image();
    let overlay_uid = uuid::Uuid::new_v4().to_string();
    // (Superseded path.) Persistence is owned by the shared doc; no `text_info.json` write here.
    let _ = &image_path;

    Ok(TypingOverlayDecoded {
        uid: overlay_uid,
        kind: TypingOverlayKind::Image,
        page_idx: request.page_idx,
        center_page_px: request.center_page_px,
        mask_clip_enabled: true,
        layer_idx: 0,
        user_scale: 1.0,
        angle_deg: 0.0,
        deform_mesh: None,
        file_name,
        original_file_name: None,
        render_data_json: Some(render_data_json),
        size_px: [width, height],
        rgba,
        warnings: Vec::new(),
    })
}

/// Стартовые render-data для image-оверлея: только пустой список эффектов.
/// Эффекты к сторонним картинкам применяются тем же pipeline, что и к растрированному тексту.
#[allow(dead_code)]
pub(super) fn default_render_data_for_image() -> Value {
    json!({ "effects": [] })
}

/// Worker: loads an external image (clipboard/file) and persists it as a NEW raster layer node in
/// `layers.json` (via `persist::add_page_raster`), centered at `center_page_px`. Returns the page +
/// uid so the tab reloads its raster cache from disk and selects it. No text/image overlay is made.
pub(super) fn render_and_store_created_raster(
    request: TypingCreateRasterRequest,
) -> Result<TypingCreatedRaster, String> {
    let (rgba, width, height) = match &request.source {
        TypingCreateImageSource::Clipboard => read_image_rgba_from_clipboard()?,
        TypingCreateImageSource::File(path) => read_image_rgba_from_file(path.as_path())?,
    };
    if width == 0 || height == 0 {
        return Err("Изображение нулевого размера.".to_string());
    }
    if rgba.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err("Некорректный буфер RGBA изображения.".to_string());
    }
    let image = ColorImage::from_rgba_unmultiplied([width, height], &rgba);
    let name = match &request.source {
        TypingCreateImageSource::File(path) => path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Картинка".to_string()),
        TypingCreateImageSource::Clipboard => "Картинка".to_string(),
    };
    let uid = uuid::Uuid::new_v4().to_string();
    let transform = crate::models::layer_model::manifest::TransformRec {
        cx: request.center_page_px[0],
        cy: request.center_page_px[1],
        rotation: 0.0,
        scale: 1.0,
    };
    crate::models::layer_model::persist::add_page_raster(
        &request.layers_dir,
        request.fallback_dir.as_deref(),
        request.page_idx,
        &uid,
        &name,
        true,
        1.0,
        transform,
        &image,
    )?;
    Ok(TypingCreatedRaster {
        page_idx: request.page_idx,
        uid,
    })
}

/// Worker: renders a raster's effects chain from its ORIGINAL base PNG (non-destructive). Returns the
/// display image to show (the rendered result, or the base unchanged when the chain is empty) plus
/// the chain. The base is never modified, so effects stay reversible.
pub(super) fn render_raster_effects(
    page_idx: usize,
    uid: String,
    base_file: String,
    primary: Option<PathBuf>,
    fallback: Option<PathBuf>,
    effects: Vec<Value>,
    base_in_memory: Option<ColorImage>,
) -> Result<TypingRasterEffectsResult, String> {
    // Prefer the resident doc's in-memory base; fall back to the on-disk base PNG when absent.
    let (base, source) = match base_in_memory {
        Some(img) => (img, "memory"),
        None => {
            let img = load_raster_base_png(&base_file, primary.as_deref(), fallback.as_deref())
                .ok_or_else(|| format!("Не найден исходный PNG растра «{base_file}»."))?;
            (img, "disk")
        }
    };
    crate::trace_log!(
        crate::trace::cat::RENDER,
        "render_raster_effects base source={} uid={} base_file={}",
        source,
        uid,
        base_file
    );
    if effects.is_empty() {
        return Ok(TypingRasterEffectsResult {
            page_idx,
            uid,
            display_image: base,
            effects,
        });
    }
    let effects_json = serde_json::to_string(&Value::Array(effects.clone()))
        .map_err(|e| format!("Эффекты растра: {e}"))?;
    let (rendered, _origin) =
        crate::models::layer_model::effects::apply_effects_to_color_image(&base, &effects_json)
            .map_err(|e| format!("Эффекты растра: {e}"))?;
    Ok(TypingRasterEffectsResult {
        page_idx,
        uid,
        display_image: rendered,
        effects,
    })
}

/// Loads a raster's base PNG by name, trying the unsaved dir then the committed fallback.
pub(super) fn load_raster_base_png(file: &str, primary: Option<&Path>, fallback: Option<&Path>) -> Option<ColorImage> {
    for dir in [primary, fallback].into_iter().flatten() {
        let path = dir.join(file);
        if path.is_file()
            && let Ok(decoded) = image::open(&path)
        {
            let rgba = decoded.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            return Some(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()));
        }
    }
    None
}

/// Извлекает `effects_json` (как массив) из render-data оверлея для подачи в `apply_effects_to_image`.
pub(super) fn effects_json_from_render_data(render_data: &Value) -> String {
    render_data
        .as_object()
        .and_then(|obj| obj.get("effects"))
        .and_then(Value::as_array)
        .map(|effects| Value::Array(effects.clone()))
        .and_then(|effects| serde_json::to_string(&effects).ok())
        .unwrap_or_default()
}

pub(super) fn render_and_store_edited_overlay(
    request: TypingEditOverlayRequest,
) -> Result<Option<TypingEditOverlayResult>, String> {
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }

    let render_params = render_params_with_adjacent_layout_path(
        &request.text_images_dir,
        &request.file_name,
        &request.render_params,
    );
    let rendered = match render_text_to_image(
        &render_params,
        request.font_provider.as_ref(),
        Some((&request.latest_token, request.token)),
    ) {
        Ok(rendered) => rendered,
        Err(err) if err == "render_next render cancelled" => return Ok(None),
        Err(err) => {
            eprintln!(
                "ERROR typing::edit_overlay_render layout={:?} shape={:?} wrap={:?} line_mode={:?} width_px={} token={} err={}",
                render_params.text_layout_mode,
                render_params.text_shape,
                render_params.text_wrap_mode,
                render_params.text_line_mode,
                render_params.width_px,
                request.token,
                err
            );
            return Err(err);
        }
    };
    if rendered.width == 0 || rendered.height == 0 {
        return Err("Рендер редактирования вернул изображение нулевого размера.".to_string());
    }

    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }

    fs::create_dir_all(&request.text_images_dir).map_err(|err| {
        format!(
            "Не удалось создать папку {}: {err}",
            request.text_images_dir.display()
        )
    })?;
    let image_path = request.text_images_dir.join(&request.file_name);
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }
    image::save_buffer(
        &image_path,
        rendered.rgba.as_slice(),
        rendered.width,
        rendered.height,
        image::ColorType::Rgba8,
    )
    .map_err(|err| format!("Не удалось сохранить {}: {err}", image_path.display()))?;
    save_drawn_lines_layout_image_if_needed(
        &request.text_images_dir,
        &request.file_name,
        &render_params,
        rendered.width,
        rendered.height,
    )?;

    Ok(Some(TypingEditOverlayResult {
        token: request.token,
        overlay_idx: request.overlay_idx,
        file_name: request.file_name,
        image_original_file_name: None,
        is_image_effects: false,
        user_scale: request.user_scale.max(0.05),
        rotation_deg: request.rotation_deg,
        render_data_json: request.render_data_json,
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
        warnings: rendered.warnings,
    }))
}

/// Renders the UN-WARPED base image for a text overlay's live vector-transform preview (Phase 3b).
///
/// The request's `render_params` must already have `raster_transform` cleared, so the render is the
/// overlay WITHOUT its mesh warp — texturing it onto the working mesh applies the warp exactly once.
/// This is a transient GPU-cache preview: it does NOT write to disk. Returns `Ok(None)` when the render
/// is superseded (its token no longer matches `latest_token`) or was cancelled mid-flight.
pub(super) fn render_vector_transform_base(
    request: TypingVectorBaseRenderRequest,
) -> Result<Option<TypingVectorBaseRenderResult>, String> {
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }
    // Vector transform is only offered for Normal/Shape/CustomVectorLines layouts, none of which read
    // an adjacent raster layout PNG, so no `render_params_with_adjacent_layout_path` fix-up is needed.
    // The CLEARED warp is honored by the renderer as identity (no-op).
    let rendered = match render_text_to_image(
        &request.render_params,
        request.font_provider.as_ref(),
        Some((&request.latest_token, request.token)),
    ) {
        Ok(rendered) => rendered,
        Err(err) if err == "render_next render cancelled" => return Ok(None),
        Err(err) => {
            eprintln!(
                "ERROR typing::vector_transform_base_render overlay_idx={} width_px={} token={} err={}",
                request.overlay_idx, request.render_params.width_px, request.token, err
            );
            return Err(err);
        }
    };
    if rendered.width == 0 || rendered.height == 0 {
        return Err("Рендер базового изображения векторной трансформации вернул нулевой размер.".to_string());
    }
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }
    Ok(Some(TypingVectorBaseRenderResult {
        token: request.token,
        overlay_idx: request.overlay_idx,
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
    }))
}

/// Re-рендер image-оверлея: грузит исходник, применяет post-effects тем же pipeline, что и текст,
/// и сохраняет результат отдельным `_fx`-файлом, сохраняя исходную картинку нетронутой.
pub(super) fn render_and_store_image_effects_overlay(
    request: TypingEditImageEffectsRequest,
) -> Result<Option<TypingEditOverlayResult>, String> {
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }

    // Исходник: отдельный original-файл, если он есть; иначе текущий показываемый файл является
    // исходным (эффекты ещё не применялись).
    let source_name = request
        .original_file_name
        .clone()
        .unwrap_or_else(|| request.file_name.clone());
    let primary_source_path = request.text_images_dir.join(&source_name);
    // Исходник предпочтительно из staging; если его там ещё нет — из сохранённой main-папки.
    let source_path = if primary_source_path.is_file() {
        primary_source_path
    } else if let Some(fallback) = request
        .fallback_text_images_dir
        .as_ref()
        .map(|dir| dir.join(&source_name))
        .filter(|path| path.is_file())
    {
        fallback
    } else {
        primary_source_path
    };
    let decoded = image::open(&source_path)
        .map_err(|err| {
            format!(
                "Не удалось открыть исходную картинку {}: {err}",
                source_path.display()
            )
        })?
        .to_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Err("Исходная картинка нулевого размера.".to_string());
    }

    let effects_json = effects_json_from_render_data(&request.render_data_json);
    let has_effects = !effects_json_array_is_empty(&effects_json);

    let rendered = match apply_effects_to_image(
        decoded.into_raw(),
        width,
        height,
        effects_json.as_str(),
        Some((&request.latest_token, request.token)),
    ) {
        Ok(rendered) => rendered,
        Err(err) if err == "render_next render cancelled" => return Ok(None),
        Err(err) => return Err(err),
    };
    if rendered.width == 0 || rendered.height == 0 {
        return Err("Рендер эффектов вернул изображение нулевого размера.".to_string());
    }
    if request.latest_token.load(Ordering::Acquire) != request.token {
        return Ok(None);
    }

    // Когда эффекты есть — пишем отдельный `_fx`-файл, исходник остаётся как original-файл.
    // Когда эффектов нет — показываем исходник напрямую и подчищаем устаревший `_fx`-файл.
    let (display_file_name, new_original_file_name) = if has_effects {
        let fx_name = image_effects_fx_file_name(&source_name);
        let fx_path = request.text_images_dir.join(&fx_name);
        image::save_buffer(
            &fx_path,
            rendered.rgba.as_slice(),
            rendered.width,
            rendered.height,
            image::ColorType::Rgba8,
        )
        .map_err(|err| format!("Не удалось сохранить {}: {err}", fx_path.display()))?;
        (fx_name, Some(source_name))
    } else {
        // Если раньше был отдельный `_fx`-файл — удаляем его, возвращаясь к исходнику.
        if request.original_file_name.is_some() && request.file_name != source_name {
            let _ = fs::remove_file(request.text_images_dir.join(&request.file_name));
        }
        (source_name, None)
    };

    Ok(Some(TypingEditOverlayResult {
        token: request.token,
        overlay_idx: request.overlay_idx,
        file_name: display_file_name,
        image_original_file_name: new_original_file_name,
        is_image_effects: true,
        user_scale: request.user_scale.max(0.05),
        rotation_deg: request.rotation_deg,
        render_data_json: request.render_data_json,
        size_px: [rendered.width as usize, rendered.height as usize],
        rgba: rendered.rgba,
        warnings: rendered.warnings,
    }))
}

/// Имя `_fx`-файла, производное от имени исходной картинки (`name.png` -> `name_fx.png`).
pub(super) fn image_effects_fx_file_name(source_name: &str) -> String {
    let path = Path::new(source_name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("png");
    format!("{stem}_fx.{ext}")
}

/// Истина, когда сериализованный массив эффектов пуст или отсутствует.
pub(super) fn effects_json_array_is_empty(effects_json: &str) -> bool {
    let trimmed = effects_json.trim();
    if trimmed.is_empty() {
        return true;
    }
    serde_json::from_str::<Value>(trimmed)
        .ok()
        .and_then(|value| value.as_array().map(|arr| arr.is_empty()))
        .unwrap_or(true)
}

pub(super) fn shape_variant_slot_size(current_size_px: [usize; 2]) -> Vec2 {
    fit_size_to_box(
        current_size_px,
        Vec2::new(
            TEXT_SHAPE_VARIANT_TILE_MAX_WIDTH_PX,
            TEXT_SHAPE_VARIANT_TILE_MAX_HEIGHT_PX,
        ),
    )
}

pub(super) fn shape_variant_panel_size(slot_size: Vec2, gap_px: f32, padding_px: f32) -> Vec2 {
    let grid_side = TEXT_SHAPE_VARIANT_GRID_SIDE as f32;
    Vec2::new(
        padding_px * 2.0 + slot_size.x * grid_side + gap_px * (grid_side - 1.0),
        padding_px * 2.0 + slot_size.y * grid_side + gap_px * (grid_side - 1.0),
    )
}

pub(super) fn shape_variant_panel_pos(
    menu_rect: Rect,
    panel_size: Vec2,
    viewport_rect: Rect,
    place_above: bool,
) -> Pos2 {
    let viewport_center_x = viewport_rect.center().x;
    let x = if menu_rect.center().x >= viewport_center_x {
        menu_rect.right() - panel_size.x
    } else {
        menu_rect.left()
    };
    let y = if place_above {
        menu_rect.top() - panel_size.y - TEXT_SHAPE_VARIANT_PANEL_MENU_GAP_PX
    } else {
        menu_rect.bottom() + TEXT_SHAPE_VARIANT_PANEL_MENU_GAP_PX
    };
    Pos2::new(x, y)
}

pub(super) fn use_dark_shape_variant_checkerboard(text_color: [u8; 4]) -> bool {
    let r = f32::from(text_color[0]);
    let g = f32::from(text_color[1]);
    let b = f32::from(text_color[2]);
    let a = f32::from(text_color[3]) / 255.0;
    let luminance = (0.2126 * r + 0.7152 * g + 0.0722 * b) * a + 255.0 * (1.0 - a);
    luminance >= 140.0
}

pub(super) fn paint_shape_variant_checkerboard(
    painter: &egui::Painter,
    rect: Rect,
    rounding: f32,
    dark: bool,
) {
    let (base_color, alternate_color, stroke_color) = if dark {
        (
            Color32::from_rgb(64, 64, 64),
            Color32::from_rgb(88, 88, 88),
            Color32::from_rgb(115, 115, 115),
        )
    } else {
        (
            Color32::from_rgb(232, 232, 232),
            Color32::from_rgb(198, 198, 198),
            Color32::from_rgb(150, 150, 150),
        )
    };

    painter.rect_filled(rect, rounding, base_color);
    let clip_rect = rect.shrink(1.0);
    let clipped = painter.with_clip_rect(clip_rect);
    let side = TEXT_SHAPE_VARIANT_CHECKER_SIDE_PX.max(1.0);
    let cols = (rect.width() / side).ceil().max(1.0) as usize;
    let rows = (rect.height() / side).ceil().max(1.0) as usize;

    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                continue;
            }
            let min = Pos2::new(
                rect.left() + col as f32 * side,
                rect.top() + row as f32 * side,
            );
            let cell = Rect::from_min_size(min, Vec2::splat(side)).intersect(rect);
            clipped.rect_filled(cell, 0.0, alternate_color);
        }
    }

    painter.rect_stroke(
        rect,
        rounding,
        Stroke::new(1.0, stroke_color),
        egui::StrokeKind::Inside,
    );
}

pub(super) fn fit_size_to_box(source_size: [usize; 2], box_size: Vec2) -> Vec2 {
    let src_w = source_size[0].max(1) as f32;
    let src_h = source_size[1].max(1) as f32;
    let scale = (box_size.x.max(1.0) / src_w)
        .min(box_size.y.max(1.0) / src_h)
        .min(1.0);
    Vec2::new((src_w * scale).max(1.0), (src_h * scale).max(1.0))
}

pub(super) fn build_shape_variant_grid(base_params: &TextRenderParams) -> Vec<TypingShapeVariant> {
    const WRAP_MODES: [TextWrapMode; 3] = [
        TextWrapMode::Minimal,
        TextWrapMode::Moderate,
        TextWrapMode::Aggressive,
    ];
    const SOFT_PEAK_VARIANTS: [u8; 3] = [3, 9, 6];
    let min_width_available = shape_min_width_available(base_params.text_shape);
    let mut out = Vec::with_capacity(TEXT_SHAPE_VARIANT_GRID_SIDE * TEXT_SHAPE_VARIANT_GRID_SIDE);

    for row in 0..TEXT_SHAPE_VARIANT_GRID_SIDE {
        for (col, text_wrap_mode) in WRAP_MODES.iter().copied().enumerate() {
            let (width_px, shape_min_width_percent) = if min_width_available {
                let percent = match row {
                    0 => 50.0,
                    1 => 75.0,
                    2 => 90.0,
                    _ => base_params.shape_min_width_percent,
                };
                (base_params.width_px.max(1), percent)
            } else if base_params.text_shape == TextShape::SoftPeak {
                (
                    base_params.width_px.max(1),
                    base_params.shape_min_width_percent,
                )
            } else {
                let scale = match row {
                    0 => 0.95,
                    1 => 1.0,
                    2 => 1.05,
                    _ => 1.0,
                };
                (
                    ((base_params.width_px.max(1) as f32) * scale)
                        .round()
                        .max(1.0) as u32,
                    base_params.shape_min_width_percent,
                )
            };
            out.push(TypingShapeVariant {
                row,
                col,
                width_px,
                text_wrap_mode,
                shape_min_width_percent,
                shape_variant: if base_params.text_shape == TextShape::SoftPeak {
                    SOFT_PEAK_VARIANTS
                        .get(row)
                        .copied()
                        .unwrap_or(base_params.shape_variant)
                } else {
                    base_params.shape_variant
                },
            });
        }
    }

    out
}

pub(super) fn shape_variant_preview_available(overlay_kind: TypingOverlayKind) -> bool {
    overlay_kind == TypingOverlayKind::Text
}

pub(super) fn render_shape_variant_preview_tiles(
    base_params: TextRenderParams,
    variants: Vec<TypingShapeVariant>,
    fonts: &Arc<dyn FontProvider>,
    cancel_render: &Arc<AtomicBool>,
) -> Vec<TypingShapeVariantPreviewTile> {
    let mut indexed_variants = variants.into_iter().enumerate();
    let mut indexed_tiles = Vec::<(usize, Option<TypingShapeVariantPreviewTile>)>::new();

    loop {
        if cancel_render.load(Ordering::Relaxed) {
            break;
        }
        let batch = indexed_variants
            .by_ref()
            .take(TEXT_SHAPE_VARIANT_GRID_SIDE)
            .collect::<Vec<_>>();
        if batch.is_empty() {
            break;
        }

        let (tx, rx) = mpsc::channel::<(usize, Option<TypingShapeVariantPreviewTile>)>();
        let mut handles = Vec::with_capacity(batch.len());

        for (index, variant) in batch {
            let tx = tx.clone();
            let base_params = base_params.clone();
            let cancel_render = Arc::clone(cancel_render);
            let fonts = Arc::clone(fonts);
            let worker_name = format!(
                "typing-shape-variant-render-{}-{}",
                variant.row, variant.col
            );
            match thread::Builder::new().name(worker_name).spawn(move || {
                if cancel_render.load(Ordering::Relaxed) {
                    return;
                }
                let tile = render_shape_variant_preview_tile(base_params, variant, fonts.as_ref());
                if cancel_render.load(Ordering::Relaxed) {
                    return;
                }
                if let Err(err) = tx.send((index, tile)) {
                    eprintln!(
                        "ERROR typing::shape_variant_preview_render_send index={} err={}",
                        index, err
                    );
                }
            }) {
                Ok(handle) => handles.push(handle),
                Err(err) => {
                    eprintln!(
                        "ERROR typing::shape_variant_preview_spawn index={} err={}",
                        index, err
                    );
                }
            }
        }
        drop(tx);

        indexed_tiles.extend(rx);
        for handle in handles {
            if handle.join().is_err() {
                eprintln!("ERROR typing::shape_variant_preview_worker_panicked");
            }
        }
    }
    indexed_tiles.sort_by_key(|(index, _)| *index);
    indexed_tiles
        .into_iter()
        .filter_map(|(_, tile)| tile)
        .collect()
}

pub(super) fn render_shape_variant_preview_tile(
    base_params: TextRenderParams,
    variant: TypingShapeVariant,
    fonts: &dyn FontProvider,
) -> Option<TypingShapeVariantPreviewTile> {
    let mut params = base_params.clone();
    params.width_px = variant.width_px;
    params.text_wrap_mode = variant.text_wrap_mode;
    params.shape_min_width_percent = variant.shape_min_width_percent;
    params.shape_variant = variant.shape_variant;
    params.compare_shape_with = Some(TextRenderShapeCompareParams {
        width_px: base_params.width_px,
        text_wrap_mode: base_params.text_wrap_mode,
        shape_min_width_percent: base_params.shape_min_width_percent,
        shape_variant: base_params.shape_variant,
        cancel_render_if_layout_text_unchanged: true,
    });

    match render_text_to_image(&params, fonts, None) {
        Ok(rendered) if rendered.width > 0 && rendered.height > 0 => {
            let width = match usize::try_from(rendered.width) {
                Ok(width) => width,
                Err(err) => {
                    eprintln!(
                        "ERROR typing::shape_variant_preview_width row={} col={} width={} err={}",
                        variant.row, variant.col, rendered.width, err
                    );
                    return None;
                }
            };
            let height = match usize::try_from(rendered.height) {
                Ok(height) => height,
                Err(err) => {
                    eprintln!(
                        "ERROR typing::shape_variant_preview_height row={} col={} height={} err={}",
                        variant.row, variant.col, rendered.height, err
                    );
                    return None;
                }
            };
            Some(TypingShapeVariantPreviewTile {
                variant,
                size_px: [width, height],
                rgba: Some(rendered.rgba),
                texture: None,
            })
        }
        Ok(_) => None,
        Err(err) => {
            eprintln!(
                "ERROR typing::shape_variant_preview_render row={} col={} err={}",
                variant.row, variant.col, err
            );
            None
        }
    }
}

pub(super) fn build_shape_variant_apply_payload(
    render_data: &Value,
    variant: &TypingShapeVariant,
) -> Option<(TextRenderParams, Value)> {
    let mut updated = render_data.clone();
    let text_params = updated
        .as_object_mut()?
        .get_mut("text_params")?
        .as_object_mut()?;
    text_params.insert(
        "text_wrap_mode".to_string(),
        Value::String(text_wrap_mode_to_config_str(variant.text_wrap_mode).to_string()),
    );
    text_params.insert("width_px".to_string(), Value::from(variant.width_px));
    text_params.insert(
        "shape_min_width_percent".to_string(),
        Value::from(variant.shape_min_width_percent),
    );
    text_params.insert(
        "shape_variant".to_string(),
        Value::from(variant.shape_variant),
    );
    let render_params = text_render_params_from_render_data(&updated)?;
    Some((render_params, updated))
}

pub(super) fn shape_min_width_available(shape: TextShape) -> bool {
    matches!(shape, TextShape::Oval | TextShape::Hexagon)
}
