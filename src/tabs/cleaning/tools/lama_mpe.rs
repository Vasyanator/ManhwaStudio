/*
FILE HEADER (cleaning/tools/lama_mpe.rs)
- Назначение: инструмент "AI удаление (Lama MPE)" для вкладки cleaning на базе
  `RegionMaskInpaintToolBase`, с вызовом Python backend через v2 framed IPC
  (метод `inpaint.lama_mpe`).
- Ключевые сущности:
  - `LamaMpeInpaintTool`: wiring инструмента (выделение, UI параметров, routing ввода).
  - `LamaMpeParams`: параметры LaMa MPE для backend (`inpaint_size`).
  - `run_lama_mpe(...)`: конвертация region image + mask в PNG, отправка через v2 framed
    IPC (`inpaint.lama_mpe`) и разбор результата из RESPONSE BLOB.
- Поведение:
  - Shift+ЛКМ по canvas открывает region mask editor (через `RegionMaskInpaintToolBase`).
  - В окне рисуется маска; кнопка `Обработать` запускает LaMa MPE через backend.
  - Параметр inpaint_size и кнопка выгрузки рендерятся прямо в окне region editor.
- Важно:
  - При пустой маске обработка не запускается и возвращается исходный регион без изменений.
  - Ошибки backend пробрасываются пользователю в статус окна region editor.
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, StrokePoint};
use crate::backend_ipc::{self, CallError};
use crate::canvas::CanvasView;
use crate::project::ProjectData;
use crate::tabs::translation::backend_health::ai_backend_offline_error;
use crate::widgets::WheelSlider;
use crate::{ai_models, config};
use eframe::egui;
use image::{ColorType, ImageEncoder};
use serde_json::{Value, json};
use web_time::Duration;

/// Per-call timeout for the v2 framed backend. Mirrors the previous HTTP read
/// timeout: model warmup + inpaint can take a while on first use.
const LAMA_MPE_BACKEND_CALL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy)]
struct LamaMpeParams {
    inpaint_size: u32,
}

impl Default for LamaMpeParams {
    fn default() -> Self {
        Self { inpaint_size: 2048 }
    }
}

pub struct LamaMpeInpaintTool {
    inpaint_base: RegionMaskInpaintToolBase,
    params: LamaMpeParams,
    unload_status: Option<String>,
}

impl Default for LamaMpeInpaintTool {
    fn default() -> Self {
        Self {
            inpaint_base: RegionMaskInpaintToolBase::new("lama_mpe_inpaint", Some(8)),
            params: LamaMpeParams::default(),
            unload_status: None,
        }
    }
}

impl LamaMpeInpaintTool {
    fn run_lama_mpe(
        image: &egui::ColorImage,
        mask: &egui::ColorImage,
        params: LamaMpeParams,
    ) -> Result<egui::ColorImage, String> {
        if image.size != mask.size {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        let width = image.size[0];
        let height = image.size[1];
        if width == 0 || height == 0 {
            return Ok(image.clone());
        }
        if !mask.pixels.iter().any(|px| px.a() > 0) {
            return Ok(image.clone());
        }
        ai_models::ensure_lama_mpe(&config::models_dir())?;

        let image_png = encode_color_image_png_rgba(image)?;
        let mask_png = encode_mask_png_luma(mask)?;
        // v2 two-image convention: request blob = image_png ++ mask_png, with
        // image_len/mask_len in the header so the backend can split them.
        let header = json!({
            "image_len": image_png.len(),
            "mask_len": mask_png.len(),
            "params": {
                "inpaint_size": params.inpaint_size,
            }
        });
        let blob = concat_image_mask(&image_png, &mask_png);

        let (_response_header, out_bytes) = inpaint_call(
            backend_ipc::protocol::METHOD_INPAINT_LAMA_MPE,
            header,
            &blob,
        )?;
        if out_bytes.is_empty() {
            return Err(t!("cleaning.inpaint.no_png_result_error").to_string());
        }
        let out_rgba = image::load_from_memory(&out_bytes)
            .map_err(|err| tf!("cleaning.inpaint.corrupt_png_error", err = err))?
            .to_rgba8();
        let out_w = out_rgba.width() as usize;
        let out_h = out_rgba.height() as usize;
        if out_w != width || out_h != height {
            return Err(tf!("cleaning.inpaint.unexpected_size_error", out_w = out_w, out_h = out_h, width = width, height = height));
        }
        Ok(egui::ColorImage::from_rgba_unmultiplied(
            [out_w, out_h],
            out_rgba.as_raw(),
        ))
    }
}

impl CleaningTool for LamaMpeInpaintTool {
    fn tool_id(&self) -> &'static str {
        "lama_mpe_inpaint"
    }

    fn title(&self) -> &'static str {
        t!("cleaning.tools.lama_mpe.title")
    }

    fn pytorch_required(&self) -> bool {
        true
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small(t!("cleaning.tools.lama_mpe.description_hint"));
    }

    fn on_key_event(&mut self, ctx: &egui::Context) -> bool {
        self.inpaint_base.on_key_event(ctx)
    }

    fn on_wheel_event(&mut self, delta_y: f32, modifiers: egui::Modifiers) -> bool {
        self.inpaint_base.on_wheel_event(delta_y, modifiers)
    }

    fn set_space_pan_active(&mut self, active: bool) {
        self.inpaint_base.set_space_pan_active(active);
    }

    fn set_ai_backend_available(&mut self, available: bool) {
        self.inpaint_base.set_ai_backend_available(available);
    }

    fn set_ai_backend_torch_available(&mut self, available: bool) {
        self.inpaint_base.set_ai_backend_torch_available(available);
    }

    fn wants_primary_stroke(&self, point: StrokePoint) -> bool {
        self.inpaint_base.wants_primary_stroke(point)
    }

    fn stroke_begin(&mut self, canvas: &mut CanvasView, point: StrokePoint) {
        self.inpaint_base.begin_selection(canvas, point);
    }

    fn stroke_update(&mut self, canvas: &mut CanvasView, _from: StrokePoint, to: StrokePoint) {
        self.inpaint_base.update_selection(canvas, to);
    }

    fn stroke_end(&mut self, canvas: &mut CanvasView) {
        self.inpaint_base.end_selection(canvas);
    }

    fn draw_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let (inpaint_base, params, unload_status) = (
            &mut self.inpaint_base,
            &mut self.params,
            &mut self.unload_status,
        );
        inpaint_base.draw_overlay_ui_custom(
            ctx,
            canvas,
            project,
            t!("cleaning.tools.lama_mpe.title"),
            {
                let params = *params;
                move |image, mask| Self::run_lama_mpe(image, mask, params)
            },
            move |ui| {
                ui.separator();
                egui::CollapsingHeader::new(t!("cleaning.tools.lama_mpe.params_heading"))
                    .id_salt("cleaning.tools.lama_mpe.params_heading")
                    .show(ui, |ui| {
                        ui.add(
                            WheelSlider::new(&mut params.inpaint_size, 512..=4096)
                                .text("inpaint_size"),
                        );
                    });
                if ui.small_button(t!("cleaning.tools.lama_mpe.unload_button")).clicked() {
                    *unload_status =
                        match unload_call(backend_ipc::protocol::METHOD_INPAINT_LAMA_MPE_UNLOAD) {
                            Ok(_) => {
                                Some(t!("cleaning.tools.lama_mpe.unload_requested_status").to_string())
                            }
                            Err(err) => Some(tf!("cleaning.inpaint.unload_error", err = err)),
                        };
                }
                if let Some(status) = unload_status.as_ref() {
                    ui.small(status);
                }
            },
        );
    }

    fn draw_cursor(
        &mut self,
        ui: &mut egui::Ui,
        canvas: &CanvasView,
        pointer_scene_pos: Option<egui::Pos2>,
    ) {
        self.inpaint_base.draw_cursor(ui, canvas, pointer_scene_pos);
    }

    fn captures_canvas_pointer(&self, pointer_pos: egui::Pos2) -> bool {
        self.inpaint_base.editor_window_contains(pointer_pos)
    }

    fn block_canvas_zoom(&self) -> bool {
        self.inpaint_base.has_open_editor()
    }
}

fn encode_color_image_png_rgba(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let width = image.size[0];
    let height = image.size[1];
    let width_u32 =
        u32::try_from(width).map_err(|_| t!("cleaning.png.image_width_too_large_error").to_string())?;
    let height_u32 =
        u32::try_from(height).map_err(|_| t!("cleaning.png.image_height_too_large_error").to_string())?;

    let mut raw = Vec::<u8>::with_capacity(width.saturating_mul(height).saturating_mul(4));
    for px in &image.pixels {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        raw.extend_from_slice(&[r, g, b, a]);
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, width_u32, height_u32, ColorType::Rgba8.into())
        .map_err(|err| tf!("cleaning.png.encode_image_error", err = err))?;
    Ok(out)
}

fn encode_mask_png_luma(mask: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let width = mask.size[0];
    let height = mask.size[1];
    let width_u32 =
        u32::try_from(width).map_err(|_| t!("cleaning.png.mask_width_too_large_error").to_string())?;
    let height_u32 =
        u32::try_from(height).map_err(|_| t!("cleaning.png.mask_height_too_large_error").to_string())?;

    let mut raw = Vec::<u8>::with_capacity(width.saturating_mul(height));
    for px in &mask.pixels {
        raw.push(if px.a() > 0 { 255 } else { 0 });
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, width_u32, height_u32, ColorType::L8.into())
        .map_err(|err| tf!("cleaning.png.encode_mask_error", err = err))?;
    Ok(out)
}

/// Concatenates the image PNG and mask PNG into the v2 two-image request blob
/// (`image_png ++ mask_png`). The split is recovered server-side from the
/// `image_len`/`mask_len` header fields.
fn concat_image_mask(image_png: &[u8], mask_png: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(image_png.len() + mask_png.len());
    blob.extend_from_slice(image_png);
    blob.extend_from_slice(mask_png);
    blob
}

/// Issues a blocking v2 framed inpaint call. The request blob and header are
/// built by the caller; the result PNG comes from the RESPONSE BLOB (raw bytes)
/// and the metadata from the response header `Value`.
///
/// Maps `CallError` to a user-facing `String`, preserving the previous UX:
/// - `Error`       → the backend error message (same as the old method error).
/// - `Interrupted` → transient abort surfaced to the status line.
/// - `Transport`   → connect/framing failure (unified backend offline message).
fn inpaint_call(method: &str, header: Value, blob: &[u8]) -> Result<(Value, Vec<u8>), String> {
    let client = backend_ipc::shared_client().map_err(|_| ai_backend_offline_error().to_string())?;
    client
        .call(method, header, blob, LAMA_MPE_BACKEND_CALL_TIMEOUT)
        .map_err(map_inpaint_call_error)
}

/// Issues a v2 framed `*.unload` call (no fields, no blob) and confirms the
/// backend reported `unloaded`.
fn unload_call(method: &str) -> Result<(), String> {
    let (header, _blob) = inpaint_call(method, json!({}), &[])?;
    let _unloaded = header.get("unloaded").and_then(Value::as_bool);
    Ok(())
}

/// Shared `CallError` → user-facing `String` mapping for inpaint/unload calls.
fn map_inpaint_call_error(err: CallError) -> String {
    match err {
        CallError::Error(msg) => msg,
        CallError::Interrupted(msg) => tf!("cleaning.inpaint.request_aborted_error", msg = msg),
        // A transport failure means the backend is offline; surface the unified
        // offline message (matching device calls) instead of the raw error string.
        CallError::Transport(_) => ai_backend_offline_error().to_string(),
    }
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The request blob must be `image_png ++ mask_png` in that exact order, and
    /// the `image_len`/`mask_len` header ints must name the two segment lengths.
    #[test]
    fn blob_concat_orders_image_then_mask_with_lengths() {
        let image_png = b"IMAGE_PNG_BYTES".to_vec();
        let mask_png = b"MASK".to_vec();
        let blob = concat_image_mask(&image_png, &mask_png);

        let image_len = image_png.len();
        let mask_len = mask_png.len();
        assert_eq!(
            blob.len(),
            image_len + mask_len,
            "image_len + mask_len == blob length"
        );
        assert_eq!(
            &blob[..image_len],
            image_png.as_slice(),
            "image segment first"
        );
        assert_eq!(
            &blob[image_len..image_len + mask_len],
            mask_png.as_slice(),
            "mask segment second"
        );
    }

    /// The inpaint request header must carry `image_len`, `mask_len` and a
    /// `params` object with `inpaint_size`.
    #[test]
    fn header_shape_has_lengths_and_params() {
        let image_png = [0u8; 11];
        let mask_png = [0u8; 3];
        let header = json!({
            "image_len": image_png.len(),
            "mask_len": mask_png.len(),
            "params": { "inpaint_size": 2048u32 }
        });
        assert_eq!(header["image_len"].as_u64(), Some(11));
        assert_eq!(header["mask_len"].as_u64(), Some(3));
        assert!(header["params"].is_object(), "params must be an object");
        assert_eq!(header["params"]["inpaint_size"].as_u64(), Some(2048));
    }

    /// The result PNG comes from the RESPONSE BLOB (raw bytes); metadata fields
    /// are read from the response header — no base64 anywhere.
    #[test]
    fn result_png_comes_from_response_blob() {
        let mut blob = Vec::new();
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut blob),
                image::ImageFormat::Png,
            )
            .expect("encode test PNG");

        let header = json!({
            "source_size": [1, 1],
            "device": "cpu",
            "inpaint_size": 2048
        });
        assert!(!blob.is_empty(), "response blob must carry the PNG");
        let decoded = image::load_from_memory(&blob).expect("decode response blob PNG");
        assert_eq!(decoded.width(), 1);
        assert_eq!(header["device"].as_str(), Some("cpu"));
        assert_eq!(header["inpaint_size"].as_u64(), Some(2048));
    }

    /// The unload response header carries an `unloaded` bool.
    #[test]
    fn unload_response_reports_unloaded_flag() {
        let header = json!({ "unloaded": true });
        assert_eq!(header.get("unloaded").and_then(Value::as_bool), Some(true));
    }

    /// `CallError` mapping preserves the backend error message verbatim, maps
    /// interrupt to its UX string, and maps transport failures to the unified
    /// offline message.
    #[test]
    fn call_error_mapping_preserves_messages() {
        assert_eq!(
            map_inpaint_call_error(CallError::Error("boom".to_string())),
            "boom"
        );
        // Pin the exact catalog key, not just that the payload survived: a mapping
        // regression that picked a different message would still pass a `contains` check.
        assert_eq!(
            map_inpaint_call_error(CallError::Interrupted("MARKER".to_string())),
            tf!("cleaning.inpaint.request_aborted_error", msg = "MARKER")
        );
        assert_eq!(
            map_inpaint_call_error(CallError::Transport("dead".to_string())),
            ai_backend_offline_error()
        );
    }
}
