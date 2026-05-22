/*
FILE HEADER (cleaning/tools/lama_mpe.rs)
- Назначение: инструмент "AI удаление (Lama MPE)" для вкладки cleaning на базе
  `RegionMaskInpaintToolBase`, с вызовом Python backend endpoint `/inpaint/lama_mpe`.
- Ключевые сущности:
  - `LamaMpeInpaintTool`: wiring инструмента (выделение, UI параметров, routing ввода).
  - `LamaMpeParams`: параметры LaMa MPE для backend (`inpaint_size`).
  - `run_lama_mpe(...)`: конвертация region image + mask в PNG/base64, HTTP POST в backend и
    разбор результата `image_png_base64`.
- Поведение:
  - Shift+ЛКМ по canvas открывает region mask editor (через `RegionMaskInpaintToolBase`).
  - В окне рисуется маска; кнопка `Обработать` запускает LaMa MPE через backend.
  - Параметр inpaint_size и кнопка выгрузки рендерятся прямо в окне region editor.
- Важно:
  - При пустой маске обработка не запускается и возвращается исходный регион без изменений.
  - Ошибки backend пробрасываются пользователю в статус окна region editor.
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, StrokePoint};
use crate::canvas::CanvasView;
use crate::project::ProjectData;
use crate::tabs::translation::backend_health::{ai_backend_addr_text, ensure_ai_backend_healthy};
use crate::widgets::WheelSlider;
use crate::{ai_models, config};
use eframe::egui;
use image::{ColorType, ImageEncoder};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const LAMA_MPE_ENDPOINT: &str = "/inpaint/lama_mpe";
const LAMA_MPE_UNLOAD_ENDPOINT: &str = "/inpaint/lama_mpe/unload";
const LAMA_MPE_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const LAMA_MPE_READ_TIMEOUT: Duration = Duration::from_secs(300);
const LAMA_MPE_WRITE_TIMEOUT: Duration = Duration::from_secs(20);

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
            return Err("Размер изображения и маски не совпадает.".to_string());
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
        let payload = json!({
            "image_base64": base64_encode(&image_png),
            "mask_base64": base64_encode(&mask_png),
            "params": {
                "inpaint_size": params.inpaint_size,
            }
        })
        .to_string();

        let response = post_json(LAMA_MPE_ENDPOINT, &payload)?;
        let out_b64 = response
            .get("image_png_base64")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "AI backend не вернул image_png_base64.".to_string())?;
        let out_bytes = decode_base64_ascii(out_b64)?;
        let out_rgba = image::load_from_memory(&out_bytes)
            .map_err(|err| format!("AI backend вернул повреждённый PNG: {err}"))?
            .to_rgba8();
        let out_w = out_rgba.width() as usize;
        let out_h = out_rgba.height() as usize;
        if out_w != width || out_h != height {
            return Err(format!(
                "AI backend вернул неожиданный размер: {}x{} (ожидалось {}x{}).",
                out_w, out_h, width, height
            ));
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
        "AI удаление (Lama MPE)"
    }

    fn pytorch_required(&self) -> bool {
        true
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small("Обработка через Python AI backend: /inpaint/lama_mpe.");
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
            "AI удаление (Lama MPE)",
            {
                let params = *params;
                move |image, mask| Self::run_lama_mpe(image, mask, params)
            },
            move |ui| {
                ui.separator();
                ui.collapsing("Параметры Lama MPE", |ui| {
                    ui.add(
                        WheelSlider::new(&mut params.inpaint_size, 512..=4096).text("inpaint_size"),
                    );
                });
                if ui.small_button("Выгрузить Lama MPE из backend").clicked() {
                    *unload_status = match post_json(LAMA_MPE_UNLOAD_ENDPOINT, "{}") {
                        Ok(_) => Some("Запрошена выгрузка Lama MPE из памяти backend.".to_string()),
                        Err(err) => Some(format!("Ошибка выгрузки: {err}")),
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
        u32::try_from(width).map_err(|_| "Ширина изображения слишком большая.".to_string())?;
    let height_u32 =
        u32::try_from(height).map_err(|_| "Высота изображения слишком большая.".to_string())?;

    let mut raw = Vec::<u8>::with_capacity(width.saturating_mul(height).saturating_mul(4));
    for px in &image.pixels {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        raw.extend_from_slice(&[r, g, b, a]);
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, width_u32, height_u32, ColorType::Rgba8.into())
        .map_err(|err| format!("Не удалось закодировать PNG изображения: {err}"))?;
    Ok(out)
}

fn encode_mask_png_luma(mask: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let width = mask.size[0];
    let height = mask.size[1];
    let width_u32 =
        u32::try_from(width).map_err(|_| "Ширина маски слишком большая.".to_string())?;
    let height_u32 =
        u32::try_from(height).map_err(|_| "Высота маски слишком большая.".to_string())?;

    let mut raw = Vec::<u8>::with_capacity(width.saturating_mul(height));
    for px in &mask.pixels {
        raw.push(if px.a() > 0 { 255 } else { 0 });
    }
    let mut out = Vec::<u8>::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&raw, width_u32, height_u32, ColorType::L8.into())
        .map_err(|err| format!("Не удалось закодировать PNG маски: {err}"))?;
    Ok(out)
}

fn post_json(path: &str, payload: &str) -> Result<Value, String> {
    ensure_ai_backend_healthy()?;

    let addr = ai_backend_addr_text();
    let socket_addr = addr
        .to_socket_addrs()
        .map_err(|err| format!("Ошибка резолва AI backend {addr}: {err}"))?
        .next()
        .ok_or_else(|| format!("Не найден сокет AI backend: {addr}"))?;

    let mut stream = TcpStream::connect_timeout(&socket_addr, LAMA_MPE_CONNECT_TIMEOUT)
        .map_err(|err| format!("Не удалось подключиться к AI backend: {err}"))?;
    stream
        .set_read_timeout(Some(LAMA_MPE_READ_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить read timeout AI backend: {err}"))?;
    stream
        .set_write_timeout(Some(LAMA_MPE_WRITE_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить write timeout AI backend: {err}"))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        ai_backend_addr_text(),
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("Не удалось отправить HTTP-заголовки AI backend: {err}"))?;
    stream
        .write_all(payload.as_bytes())
        .map_err(|err| format!("Не удалось отправить HTTP-body AI backend: {err}"))?;

    let (status_code, body) = read_http_response(&mut stream)?;
    let body_text = String::from_utf8_lossy(&body).to_string();
    let json_value: Value = serde_json::from_str(&body_text)
        .map_err(|err| format!("AI backend вернул не-JSON ({status_code}): {err}"))?;

    if status_code >= 400 {
        let msg = json_value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Ошибка AI backend.");
        return Err(format!("AI backend HTTP {status_code}: {msg}"));
    }
    Ok(json_value)
}

fn read_http_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), String> {
    let mut raw = Vec::with_capacity(8 * 1024);
    let mut header_end: Option<usize> = None;
    let mut scratch = [0u8; 4096];

    while header_end.is_none() {
        let n = stream
            .read(&mut scratch)
            .map_err(|err| format!("Не удалось прочитать ответ AI backend: {err}"))?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&scratch[..n]);
        header_end = raw.windows(4).position(|chunk| chunk == b"\r\n\r\n");
    }
    if raw.is_empty() {
        return Err("AI backend вернул пустой HTTP-ответ.".to_string());
    }
    let Some(header_end) = header_end else {
        return Err("Некорректный HTTP-ответ AI backend (нет заголовков).".to_string());
    };

    let headers_end_idx = header_end + 4;
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let status_line = head.lines().next().unwrap_or("HTTP/1.1 500");
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|part| part.parse::<u16>().ok())
        .ok_or_else(|| format!("Не удалось распарсить HTTP-статус AI backend: {status_line}"))?;

    let content_length = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.trim().eq_ignore_ascii_case("content-length") {
            return None;
        }
        value.trim().parse::<usize>().ok()
    });

    if let Some(content_length) = content_length {
        let mut body = raw[headers_end_idx..].to_vec();
        while body.len() < content_length {
            let n = stream
                .read(&mut scratch)
                .map_err(|err| format!("Не удалось дочитать body AI backend: {err}"))?;
            if n == 0 {
                return Err(format!(
                    "HTTP body обрезан: получено {} байт из {}.",
                    body.len(),
                    content_length
                ));
            }
            body.extend_from_slice(&scratch[..n]);
        }
        body.truncate(content_length);
        return Ok((status_code, body));
    }

    let mut body = raw[headers_end_idx..].to_vec();
    stream
        .read_to_end(&mut body)
        .map_err(|err| format!("Не удалось дочитать body AI backend: {err}"))?;
    Ok((status_code, body))
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if data.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0usize;
    while i + 3 <= data.len() {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | data[i + 2] as u32;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = data.len() - i;
    if rem == 1 {
        let n = (data[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

fn decode_base64_ascii(input: &str) -> Result<Vec<u8>, String> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let mut cleaned = Vec::<u8>::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        cleaned.push(b);
    }
    if !cleaned.len().is_multiple_of(4) {
        return Err("Невалидная base64-строка (длина не кратна 4).".to_string());
    }
    let mut out = Vec::<u8>::with_capacity(cleaned.len() / 4 * 3);
    let mut i = 0usize;
    while i < cleaned.len() {
        let c0 = cleaned[i];
        let c1 = cleaned[i + 1];
        let c2 = cleaned[i + 2];
        let c3 = cleaned[i + 3];
        let v0 = base64_val(c0).ok_or_else(|| "Невалидный символ base64.".to_string())?;
        let v1 = base64_val(c1).ok_or_else(|| "Невалидный символ base64.".to_string())?;
        let pad2 = c2 == b'=';
        let pad3 = c3 == b'=';
        let v2 = if pad2 {
            0
        } else {
            base64_val(c2).ok_or_else(|| "Невалидный символ base64.".to_string())?
        };
        let v3 = if pad3 {
            0
        } else {
            base64_val(c3).ok_or_else(|| "Невалидный символ base64.".to_string())?
        };
        let n = ((v0 as u32) << 18) | ((v1 as u32) << 12) | ((v2 as u32) << 6) | (v3 as u32);
        out.push(((n >> 16) & 0xff) as u8);
        if !pad2 {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if !pad3 {
            out.push((n & 0xff) as u8);
        }
        i += 4;
    }
    Ok(out)
}

fn base64_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}
