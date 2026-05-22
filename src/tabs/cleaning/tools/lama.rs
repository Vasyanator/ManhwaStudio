/*
FILE HEADER (cleaning/tools/lama.rs)
- Назначение: инструмент "AI удаление (Lama)" для вкладки cleaning на базе
  `RegionMaskInpaintToolBase`, с вызовом Python backend endpoint `/inpaint/lama_v2`.
- Ключевые сущности:
  - `LamaInpaintTool`: wiring инструмента (выделение, UI параметров refine, routing ввода).
  - `LamaParams`: параметры refine для backend (`refine`, `n_iters`, `max_scales`, `px_budget`).
  - `LamaModelListState`: фоновое состояние проверки наличия предустановленных моделей
    в `ManhwaStudio_AI_Models/Torch/LaMa/models` без блокировки GUI.
  - `LamaModelSpec`: фиксированный каталог поддерживаемых Lama-моделей с локальными
    именами, пользовательскими названиями и optional URL для автоскачивания.
  - `run_lama(...)`: конвертация region image + mask в PNG/base64, HTTP POST в backend и
    разбор результата `image_png_base64`.
- Поведение:
  - Shift+ЛКМ по canvas открывает region mask editor (через `RegionMaskInpaintToolBase`).
  - В окне рисуется маска; кнопка `Обработать` запускает LaMa V2 через backend.
  - Параметры refine, выбор одной из предопределённых моделей и кнопка выгрузки модели
    рендерятся прямо в окне region editor.
- Важно:
  - При пустой маске обработка не запускается и возвращается исходный регион без изменений.
  - `best.ckpt` считается уже установленной инсталлятором; anime-модели при отсутствии
    докачиваются автоматически перед запуском inpaint.
  - Ошибки backend пробрасываются пользователю в статус окна region editor.
*/
use super::base::{CleaningTool, RegionMaskInpaintToolBase, StrokePoint};
use crate::ai_models;
use crate::canvas::CanvasView;
use crate::config;
use crate::project::ProjectData;
use crate::tabs::translation::backend_health::{ai_backend_addr_text, ensure_ai_backend_healthy};
use crate::widgets::{WheelComboBox, WheelSlider};
use eframe::egui;
use image::{ColorType, ImageEncoder};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

const LAMA_ENDPOINT: &str = "/inpaint/lama_v2";
const LAMA_UNLOAD_ENDPOINT: &str = "/inpaint/lama_v2/unload";
const LAMA_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
const LAMA_READ_TIMEOUT: Duration = Duration::from_secs(300);
const LAMA_WRITE_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_LAMA_MODEL_FILENAME: &str = "anime-manga-big-lama.pt";

#[derive(Debug, Clone, Copy)]
struct LamaModelSpec {
    file_name: &'static str,
    display_name: &'static str,
}

const LAMA_MODEL_SPECS: [LamaModelSpec; 3] = [
    LamaModelSpec {
        file_name: "best.ckpt",
        display_name: "Базовая",
    },
    LamaModelSpec {
        file_name: "lama_large_512px.ckpt",
        display_name: "Аниме и комиксы V1",
    },
    LamaModelSpec {
        file_name: "anime-manga-big-lama.pt",
        display_name: "Аниме и комиксы V2",
    },
];

#[derive(Debug, Clone, Copy)]
struct LamaParams {
    refine: bool,
    n_iters: u8,
    max_scales: u8,
    px_budget: u32,
}

impl Default for LamaParams {
    fn default() -> Self {
        Self {
            refine: false,
            n_iters: 15,
            max_scales: 3,
            px_budget: 1_000_000,
        }
    }
}

#[derive(Debug)]
enum LamaModelListState {
    Idle,
    Loading(Receiver<Result<Vec<String>, String>>),
    Ready(Vec<String>),
    Error(String),
}

pub struct LamaInpaintTool {
    inpaint_base: RegionMaskInpaintToolBase,
    params: LamaParams,
    model_list_state: LamaModelListState,
    selected_model: Option<String>,
    unload_status: Option<String>,
}

impl Default for LamaInpaintTool {
    fn default() -> Self {
        let mut tool = Self {
            inpaint_base: RegionMaskInpaintToolBase::new("lama_inpaint", Some(8)),
            params: LamaParams::default(),
            model_list_state: LamaModelListState::Idle,
            selected_model: Some(DEFAULT_LAMA_MODEL_FILENAME.to_string()),
            unload_status: None,
        };
        tool.request_model_scan();
        tool
    }
}

impl LamaInpaintTool {
    fn request_model_scan(&mut self) {
        if matches!(self.model_list_state, LamaModelListState::Loading(_)) {
            return;
        }

        let (tx, rx) = mpsc::channel();
        self.model_list_state = LamaModelListState::Loading(rx);
        thread::spawn(move || {
            let _send_result = tx.send(scan_lama_models());
        });
    }

    fn poll_model_scan(&mut self) {
        if matches!(self.model_list_state, LamaModelListState::Idle) {
            self.request_model_scan();
            return;
        }

        let next_state = match &self.model_list_state {
            LamaModelListState::Loading(rx) => match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Фоновая загрузка списка моделей Lama была прервана.".to_string(),
                )),
            },
            LamaModelListState::Idle
            | LamaModelListState::Ready(_)
            | LamaModelListState::Error(_) => None,
        };

        if let Some(result) = next_state {
            match result {
                Ok(models) => {
                    self.model_list_state = LamaModelListState::Ready(models);
                    normalize_selected_model(&mut self.selected_model);
                }
                Err(err) => {
                    self.model_list_state = LamaModelListState::Error(err);
                    normalize_selected_model(&mut self.selected_model);
                }
            }
        }
    }

    fn run_lama(
        image: &egui::ColorImage,
        mask: &egui::ColorImage,
        params: LamaParams,
        selected_model: Option<&str>,
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

        let resolved_model = ensure_selected_lama_model_ready(selected_model)?;

        let image_png = encode_color_image_png_rgba(image)?;
        let mask_png = encode_mask_png_luma(mask)?;
        let payload = json!({
            "image_base64": base64_encode(&image_png),
            "mask_base64": base64_encode(&mask_png),
            "params": {
                "refine": params.refine,
                "n_iters": params.n_iters,
                "max_scales": params.max_scales,
                "px_budget": params.px_budget,
                "model_name": resolved_model,
            }
        })
        .to_string();

        let response = post_json(LAMA_ENDPOINT, &payload)?;
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

impl CleaningTool for LamaInpaintTool {
    fn tool_id(&self) -> &'static str {
        "lama_inpaint"
    }

    fn title(&self) -> &'static str {
        "AI удаление (Lama)"
    }

    fn pytorch_required(&self) -> bool {
        true
    }

    fn deactivate(&mut self, _canvas: &mut CanvasView) {
        self.inpaint_base.cancel_selection();
    }

    fn draw_ui(&mut self, ui: &mut egui::Ui) {
        self.inpaint_base.draw_ui_hint(ui);
        ui.small("Обработка через Python AI backend: /inpaint/lama_v2 (LaMa V2).");
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
        self.poll_model_scan();
        let (inpaint_base, params, model_list_state, selected_model, unload_status) = (
            &mut self.inpaint_base,
            &mut self.params,
            &mut self.model_list_state,
            &mut self.selected_model,
            &mut self.unload_status,
        );
        let selected_model_for_run = selected_model.clone();
        inpaint_base.draw_overlay_ui_custom(
            ctx,
            canvas,
            project,
            "AI удаление (Lama)",
            {
                let params = *params;
                move |image, mask| {
                    Self::run_lama(image, mask, params, selected_model_for_run.as_deref())
                }
            },
            move |ui| {
                ui.separator();
                let lama_section_id = ui.make_persistent_id("cleaning_lama_params_section");
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    lama_section_id,
                    false,
                )
                .show_header(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Параметры Lama");
                        ui.add_space(8.0);
                        draw_model_picker_header_ui(ui, model_list_state, selected_model);
                    });
                })
                .body(|ui| {
                    ui.checkbox(&mut params.refine, "Refine");
                    ui.add_enabled_ui(params.refine, |ui| {
                        ui.add(WheelSlider::new(&mut params.n_iters, 5..=50).text("n_iters"));
                        ui.add(WheelSlider::new(&mut params.max_scales, 1..=5).text("max_scales"));
                        ui.add(
                            WheelSlider::new(&mut params.px_budget, 500_000..=4_000_000)
                                .text("px_budget"),
                        );
                    });
                    draw_model_picker_refresh_ui(ui, model_list_state);
                });
                if ui.small_button("Выгрузить Lama из backend").clicked() {
                    *unload_status = match post_json(LAMA_UNLOAD_ENDPOINT, "{}") {
                        Ok(_) => Some("Запрошена выгрузка Lama из памяти backend.".to_string()),
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

    let mut stream = TcpStream::connect_timeout(&socket_addr, LAMA_CONNECT_TIMEOUT)
        .map_err(|err| format!("Не удалось подключиться к AI backend: {err}"))?;
    stream
        .set_read_timeout(Some(LAMA_READ_TIMEOUT))
        .map_err(|err| format!("Не удалось выставить read timeout AI backend: {err}"))?;
    stream
        .set_write_timeout(Some(LAMA_WRITE_TIMEOUT))
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

fn scan_lama_models() -> Result<Vec<String>, String> {
    let models_dir = lama_models_dir();
    if !models_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(&models_dir).map_err(|err| {
        format!(
            "Не удалось прочитать папку моделей Lama '{}': {err}",
            models_dir.display()
        )
    })?;
    let mut models = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "Не удалось прочитать запись в папке моделей Lama '{}': {err}",
                models_dir.display()
            )
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if is_supported_lama_model_path(&path) {
            models.push(entry.file_name().to_string_lossy().to_string());
        }
    }

    models.sort();
    Ok(models)
}

fn lama_models_dir() -> PathBuf {
    config::lama_models_dir()
}

fn lama_model_spec_by_name(model_name: &str) -> Option<&'static LamaModelSpec> {
    LAMA_MODEL_SPECS
        .iter()
        .find(|spec| spec.file_name == model_name.trim())
}

fn normalize_selected_model(selected_model: &mut Option<String>) {
    if selected_model
        .as_deref()
        .and_then(lama_model_spec_by_name)
        .is_some()
    {
        return;
    }

    *selected_model = Some(DEFAULT_LAMA_MODEL_FILENAME.to_string());
}

fn is_supported_lama_model_path(path: &std::path::Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ckpt") || ext.eq_ignore_ascii_case("pt"))
}

fn is_lama_model_present(models: &[String], model_name: &str) -> bool {
    models.iter().any(|present| present == model_name)
}

fn lama_model_status_text(model_name: &str, model_list_state: &LamaModelListState) -> &'static str {
    match model_list_state {
        LamaModelListState::Idle | LamaModelListState::Loading(_) => "Проверка файлов...",
        LamaModelListState::Ready(models) => {
            if is_lama_model_present(models, model_name) {
                "Файл найден"
            } else if lama_model_spec_by_name(model_name).is_some() {
                "Будет скачана перед запуском"
            } else {
                "Ожидается от установщика"
            }
        }
        LamaModelListState::Error(_) => "Статус недоступен",
    }
}

fn ensure_selected_lama_model_ready(selected_model: Option<&str>) -> Result<&'static str, String> {
    let selected_name = selected_model.unwrap_or(DEFAULT_LAMA_MODEL_FILENAME);
    let spec = lama_model_spec_by_name(selected_name)
        .ok_or_else(|| format!("Выбрана неподдерживаемая модель Lama: {selected_name}"))?;
    let local_path = lama_models_dir().join(spec.file_name);
    if local_path.exists() {
        return Ok(spec.file_name);
    }

    ai_models::ensure_lama_model(&config::models_dir(), spec.file_name)?;
    Ok(spec.file_name)
}

fn draw_model_picker_header_ui(
    ui: &mut egui::Ui,
    model_list_state: &mut LamaModelListState,
    selected_model: &mut Option<String>,
) {
    normalize_selected_model(selected_model);
    match model_list_state {
        LamaModelListState::Idle => {
            ui.add_enabled_ui(false, |ui| {
                WheelComboBox::from_id_salt("cleaning_lama_model_picker")
                    .selected_text("Проверка моделей...")
                    .show_ui(ui, |_ui| {});
            });
        }
        LamaModelListState::Loading(_) => {
            let selected_text = selected_model
                .as_deref()
                .and_then(lama_model_spec_by_name)
                .map_or("Выберите модель", |spec| spec.display_name);
            WheelComboBox::from_id_salt("cleaning_lama_model_picker")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for spec in &LAMA_MODEL_SPECS {
                        let _changed = ui
                            .selectable_value(
                                selected_model,
                                Some(spec.file_name.to_string()),
                                spec.display_name,
                            )
                            .changed();
                    }
                });
            if let Some(selected_name) = selected_model.as_deref() {
                ui.small(lama_model_status_text(selected_name, model_list_state));
            }
        }
        LamaModelListState::Ready(_) | LamaModelListState::Error(_) => {
            let selected_text = selected_model
                .as_deref()
                .and_then(lama_model_spec_by_name)
                .map_or("Выберите модель", |spec| spec.display_name);
            WheelComboBox::from_id_salt("cleaning_lama_model_picker")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for spec in &LAMA_MODEL_SPECS {
                        let _changed = ui
                            .selectable_value(
                                selected_model,
                                Some(spec.file_name.to_string()),
                                spec.display_name,
                            )
                            .changed();
                    }
                });
            if let Some(selected_name) = selected_model.as_deref() {
                ui.small(lama_model_status_text(selected_name, model_list_state));
            }
            if let LamaModelListState::Error(err) = model_list_state {
                ui.colored_label(egui::Color32::from_rgb(208, 84, 62), err.as_str());
            }
        }
    }
}

fn draw_model_picker_refresh_ui(ui: &mut egui::Ui, model_list_state: &mut LamaModelListState) {
    if ui.small_button("Проверить модели").clicked() {
        *model_list_state = LamaModelListState::Idle;
    }
}
