#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/*
FILE OVERVIEW: src/bin/text_render_test.rs
GUI-тестер рендера текста в PNG через cosmic-text.

Основной поток:
- Загружает список шрифтов из папки `fonts` в корне проекта.
- Рисует окно с параметрами: текст, шрифт/face, базовый цвет текста, размер, line spacing (px/%), align (включая justify), форма, минимальная ширина формы (%), строгий режим формы, ширина.
- Поддерживает глобальные переключатели `bold/italic` и опциональный rich-text парсинг тегов `<b>/<i>` (включается чекбоксом).
- Дает панель эффектов: добавление карточек, порядок сверху-вниз (up/down), сериализация в JSON-пайплайн рендера.
- Поддерживает эффекты `stroke`, `shadow`, `glow_v1`, `glow_v2`, `gradient2`, `gradient4`, `reflect`, `shake`.
- Отправляет тяжёлый рендер в background worker и обновляет preview по готовности.
- Позволяет сохранить последний рендер в PNG.
*/

#[path = "text_render_test/render.rs"]
mod text_render_impl;

use cosmic_text::fontdb;
use eframe::egui;
use egui::{Color32, ColorImage, Stroke, TextureHandle, TextureOptions, Vec2};
use image::RgbaImage;
use serde_json::json;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use text_render_impl::{
    HorizontalAlign, RenderedTextImage, TextRenderParams, TextShape, render_text_to_image,
};

const APP_TITLE: &str = "text_render_test (cosmic-text)";
const DEFAULT_TEXT: &str = "Привет, мир!\nЭто тест длинной строки для переносов и центровки.";

#[derive(Clone)]
struct FontEntry {
    label: String,
    path: PathBuf,
    faces: Vec<FontFaceEntry>,
}

#[derive(Clone)]
struct FontFaceEntry {
    label: String,
    face_index: usize,
}

#[derive(Clone)]
struct RenderJob {
    token: u64,
    params: TextRenderParams,
}

struct RenderResult {
    token: u64,
    image: Result<RenderedTextImage, String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AvailableEffectKind {
    Stroke,
    Shadow,
    GlowV1,
    GlowV2,
    Gradient2,
    Gradient4,
    Reflect,
    Shake,
}

#[derive(Clone)]
enum EffectCard {
    Stroke(StrokeEffectCard),
    Shadow(ShadowEffectCard),
    Glow(GlowEffectCard),
    Gradient2(Gradient2EffectCard),
    Gradient4(Gradient4EffectCard),
    Reflect(ReflectEffectCard),
    Shake(ShakeEffectCard),
}

#[derive(Clone)]
struct StrokeEffectCard {
    width_px: f32,
    color: Color32,
    opacity_mode: StrokeOpacityMode,
    transparency_percent: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StrokeOpacityMode {
    Static,
    FromContour,
}

#[derive(Clone)]
struct ShadowEffectCard {
    offset_x_px: i32,
    offset_y_px: i32,
    transparency_percent: f32,
    blur_radius_px: f32,
    color_mode: ShadowColorMode,
    color: Color32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShadowColorMode {
    SingleColor,
    SourceColors,
}

#[derive(Clone)]
struct GlowEffectCard {
    version: GlowEffectVersion,
    radius_px: f32,
    color: Color32,
    opacity_mode: StrokeOpacityMode,
    transparency_percent: f32,
    fade_strength: f32,
    fade_shift: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GlowEffectVersion {
    V1,
    V2,
}

#[derive(Clone)]
struct Gradient2EffectCard {
    color1: Color32,
    color2: Color32,
    angle_deg: f32,
    respect_source_alpha: bool,
    fill_mode: Gradient2FillMode,
    target_color: Color32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gradient2FillMode {
    AllOpaque,
    SpecificColor,
}

#[derive(Clone)]
struct Gradient4EffectCard {
    color_top_left: Color32,
    color_top_right: Color32,
    color_bottom_left: Color32,
    color_bottom_right: Color32,
    respect_source_alpha: bool,
    fill_mode: Gradient4FillMode,
    target_color: Color32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gradient4FillMode {
    AllOpaque,
    SpecificColor,
}

#[derive(Clone)]
struct ReflectEffectCard {
    axis: ReflectAxis,
}

#[derive(Clone)]
struct ShakeEffectCard {
    angle_deg: f32,
    up_px: f32,
    down_px: f32,
    steps: u32,
    base_fade: f32,
    decay: f32,
    blur_px: u32,
    autogrow: bool,
    grow_margin_px: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReflectAxis {
    X,
    Y,
}

struct TextRenderTestApp {
    fonts_dir: PathBuf,
    fonts: Vec<FontEntry>,
    selected_font_idx: usize,
    selected_face_idx: usize,
    text: String,
    text_color: Color32,
    font_size_px: f32,
    line_spacing_px: f32,
    line_spacing_percent: f32,
    width_px: u32,
    align: HorizontalAlign,
    text_shape: TextShape,
    shape_min_width_percent: f32,
    shape_variant: u8,
    strict_shape_fit: bool,
    force_bold: bool,
    force_italic: bool,
    enable_inline_style_tags: bool,
    effect_to_add: AvailableEffectKind,
    effects: Vec<EffectCard>,
    status_line: String,
    request_tx: Sender<RenderJob>,
    result_rx: Receiver<RenderResult>,
    latest_token: u64,
    render_in_flight: bool,
    needs_initial_render: bool,
    preview_texture: Option<TextureHandle>,
    preview_size: [usize; 2],
    last_rendered_image: Option<RenderedTextImage>,
}

impl TextRenderTestApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let fonts_dir = resolve_fonts_dir();
        let fonts = load_fonts_from_dir(&fonts_dir);
        let (request_tx, result_rx) = spawn_render_worker();
        let status_line = if fonts.is_empty() {
            format!("Не найдено шрифтов в {}", fonts_dir.display())
        } else {
            "Готово к рендеру".to_string()
        };

        Self {
            fonts_dir,
            fonts,
            selected_font_idx: 0,
            selected_face_idx: 0,
            text: DEFAULT_TEXT.to_string(),
            text_color: Color32::WHITE,
            font_size_px: 48.0,
            line_spacing_px: 0.0,
            line_spacing_percent: 0.0,
            width_px: 800,
            align: HorizontalAlign::Center,
            text_shape: TextShape::Free,
            shape_min_width_percent: 50.0,
            shape_variant: 5,
            strict_shape_fit: false,
            force_bold: false,
            force_italic: false,
            enable_inline_style_tags: false,
            effect_to_add: AvailableEffectKind::Stroke,
            effects: Vec::new(),
            status_line,
            request_tx,
            result_rx,
            latest_token: 0,
            render_in_flight: false,
            needs_initial_render: true,
            preview_texture: None,
            preview_size: [1, 1],
            last_rendered_image: None,
        }
    }

    fn queue_render(&mut self) {
        let Some(font) = self.fonts.get(self.selected_font_idx) else {
            self.status_line = format!("Шрифты не найдены в {}", self.fonts_dir.display());
            self.render_in_flight = false;
            return;
        };
        let selected_face_index = font
            .faces
            .get(self.selected_face_idx)
            .map(|face| face.face_index)
            .unwrap_or(0usize);

        self.latest_token = self.latest_token.saturating_add(1);
        let job = RenderJob {
            token: self.latest_token,
            params: TextRenderParams {
                text: self.text.clone(),
                text_color: [
                    self.text_color.r(),
                    self.text_color.g(),
                    self.text_color.b(),
                    self.text_color.a(),
                ],
                font_path: font.path.clone(),
                font_size_px: self.font_size_px.max(1.0),
                width_px: self.width_px.max(1),
                line_spacing_px: self.line_spacing_px,
                line_spacing_percent: self.line_spacing_percent,
                align: self.align,
                selected_face_index,
                force_bold: self.force_bold,
                force_italic: self.force_italic,
                enable_inline_style_tags: self.enable_inline_style_tags,
                text_shape: self.text_shape,
                shape_min_width_percent: self.shape_min_width_percent,
                shape_variant: self.shape_variant,
                strict_shape_fit: self.strict_shape_fit,
                effects_json: self.effects_json(),
            },
        };

        match self.request_tx.send(job) {
            Ok(()) => {
                self.render_in_flight = true;
                self.status_line = "Рендер в фоне...".to_string();
            }
            Err(err) => {
                self.render_in_flight = false;
                self.status_line = format!("Не удалось отправить задачу рендера: {err}");
            }
        }
    }

    fn poll_render_results(&mut self, ctx: &egui::Context) {
        let mut got_latest = false;
        while let Ok(result) = self.result_rx.try_recv() {
            if result.token != self.latest_token {
                continue;
            }

            self.render_in_flight = false;
            got_latest = true;
            match result.image {
                Ok(image) => {
                    self.preview_size = [image.width as usize, image.height as usize];
                    let color_image = ColorImage::from_rgba_unmultiplied(
                        self.preview_size,
                        image.rgba.as_slice(),
                    );

                    if let Some(texture) = &mut self.preview_texture {
                        texture.set(color_image, TextureOptions::LINEAR);
                    } else {
                        self.preview_texture = Some(ctx.load_texture(
                            "text-render-test-preview",
                            color_image,
                            TextureOptions::LINEAR,
                        ));
                    }

                    self.last_rendered_image = Some(image);
                    self.status_line = "Рендер завершён".to_string();
                }
                Err(err) => {
                    self.status_line = format!("Ошибка рендера: {err}");
                }
            }
        }

        if got_latest {
            ctx.request_repaint();
        }
    }

    fn save_last_png(&mut self) {
        let Some(image) = self.last_rendered_image.as_ref() else {
            self.status_line = "Сначала выполните рендер".to_string();
            return;
        };

        let Some(path) = rfd::FileDialog::new()
            .set_file_name("render.png")
            .add_filter("PNG image", &["png"])
            .save_file()
        else {
            return;
        };

        let Some(rgba) = RgbaImage::from_raw(image.width, image.height, image.rgba.clone()) else {
            self.status_line = "Не удалось подготовить данные PNG".to_string();
            return;
        };

        match rgba.save(&path) {
            Ok(()) => {
                self.status_line = format!("Сохранено: {}", path.display());
            }
            Err(err) => {
                self.status_line = format!("Ошибка сохранения PNG: {err}");
            }
        }
    }

    fn effects_json(&self) -> String {
        let mut out = Vec::with_capacity(self.effects.len());
        for effect in self.effects.iter() {
            match effect {
                EffectCard::Stroke(stroke) => {
                    out.push(json!({
                        "effect": "stroke",
                        "enabled": true,
                        "width": stroke.width_px,
                        "color": [stroke.color.r(), stroke.color.g(), stroke.color.b(), stroke.color.a()],
                        "opacity_mode": if stroke.opacity_mode == StrokeOpacityMode::FromContour { "from_contour" } else { "static" },
                        "transparency": stroke.transparency_percent,
                        "opacity": 100.0 - stroke.transparency_percent,
                    }));
                }
                EffectCard::Shadow(shadow) => {
                    out.push(json!({
                        "effect": "shadow",
                        "enabled": true,
                        "offset_x": shadow.offset_x_px,
                        "offset_y": shadow.offset_y_px,
                        "transparency": shadow.transparency_percent,
                        "opacity": 100.0 - shadow.transparency_percent,
                        "blur": shadow.blur_radius_px,
                        "blur_radius": shadow.blur_radius_px,
                        "blur_px": shadow.blur_radius_px,
                        "mode": if shadow.color_mode == ShadowColorMode::SourceColors { "source" } else { "single" },
                        "use_source_color": shadow.color_mode == ShadowColorMode::SourceColors,
                        "color": [shadow.color.r(), shadow.color.g(), shadow.color.b(), shadow.color.a()],
                    }));
                }
                EffectCard::Glow(glow) => {
                    let effect_name = match glow.version {
                        GlowEffectVersion::V1 => "glow_v1",
                        GlowEffectVersion::V2 => "glow_v2",
                    };
                    out.push(json!({
                        "effect": effect_name,
                        "enabled": true,
                        "radius": glow.radius_px,
                        "color": [glow.color.r(), glow.color.g(), glow.color.b(), glow.color.a()],
                        "opacity_mode": if glow.opacity_mode == StrokeOpacityMode::FromContour { "from_contour" } else { "static" },
                        "transparency": glow.transparency_percent,
                        "opacity": 100.0 - glow.transparency_percent,
                        "fade_strength": glow.fade_strength,
                        "fade_shift": glow.fade_shift,
                    }));
                }
                EffectCard::Gradient2(gradient) => {
                    out.push(json!({
                        "effect": "gradient2",
                        "enabled": true,
                        "color1": [gradient.color1.r(), gradient.color1.g(), gradient.color1.b(), gradient.color1.a()],
                        "color2": [gradient.color2.r(), gradient.color2.g(), gradient.color2.b(), gradient.color2.a()],
                        "angle_deg": gradient.angle_deg,
                        "respect_source_alpha": gradient.respect_source_alpha,
                        "fill_mode": match gradient.fill_mode {
                            Gradient2FillMode::AllOpaque => "all_opaque",
                            Gradient2FillMode::SpecificColor => "specific_color",
                        },
                        "target_color": [gradient.target_color.r(), gradient.target_color.g(), gradient.target_color.b(), gradient.target_color.a()],
                    }));
                }
                EffectCard::Gradient4(gradient) => {
                    out.push(json!({
                        "effect": "gradient4",
                        "enabled": true,
                        "color_top_left": [gradient.color_top_left.r(), gradient.color_top_left.g(), gradient.color_top_left.b(), gradient.color_top_left.a()],
                        "color_top_right": [gradient.color_top_right.r(), gradient.color_top_right.g(), gradient.color_top_right.b(), gradient.color_top_right.a()],
                        "color_bottom_left": [gradient.color_bottom_left.r(), gradient.color_bottom_left.g(), gradient.color_bottom_left.b(), gradient.color_bottom_left.a()],
                        "color_bottom_right": [gradient.color_bottom_right.r(), gradient.color_bottom_right.g(), gradient.color_bottom_right.b(), gradient.color_bottom_right.a()],
                        "respect_source_alpha": gradient.respect_source_alpha,
                        "fill_mode": match gradient.fill_mode {
                            Gradient4FillMode::AllOpaque => "all_opaque",
                            Gradient4FillMode::SpecificColor => "specific_color",
                        },
                        "target_color": [gradient.target_color.r(), gradient.target_color.g(), gradient.target_color.b(), gradient.target_color.a()],
                    }));
                }
                EffectCard::Reflect(reflect) => {
                    out.push(json!({
                        "effect": "reflect",
                        "enabled": true,
                        "axis": match reflect.axis {
                            ReflectAxis::X => "x",
                            ReflectAxis::Y => "y",
                        },
                    }));
                }
                EffectCard::Shake(shake) => {
                    out.push(json!({
                        "effect": "shake",
                        "enabled": true,
                        "angle_deg": shake.angle_deg,
                        "up": shake.up_px,
                        "down": shake.down_px,
                        "steps": shake.steps,
                        "base_fade": shake.base_fade,
                        "decay": shake.decay,
                        "blur": shake.blur_px,
                        "autogrow": shake.autogrow,
                        "grow_margin": shake.grow_margin_px,
                    }));
                }
            }
        }
        serde_json::to_string_pretty(&out).unwrap_or_else(|_| "[]".to_string())
    }
}

impl eframe::App for TextRenderTestApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_render_results(ctx);

        if self.needs_initial_render {
            self.needs_initial_render = false;
            self.queue_render();
        }
        if let Some(font) = self.fonts.get(self.selected_font_idx) {
            let max_face_idx = font.faces.len().saturating_sub(1);
            self.selected_face_idx = self.selected_face_idx.min(max_face_idx);
        } else {
            self.selected_face_idx = 0;
        }

        egui::SidePanel::left("controls_panel")
            .min_width(340.0)
            .show(ctx, |ui| {
                ui.heading("Text Render Test");
                ui.label(format!("Fonts dir: {}", self.fonts_dir.display()));
                ui.separator();

                ui.label("Text");
                ui.add(
                    egui::TextEdit::multiline(&mut self.text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(10),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Цвет текста:");
                    ui.color_edit_button_srgba(&mut self.text_color);
                });

                ui.add_space(4.0);
                let selected_text = self
                    .fonts
                    .get(self.selected_font_idx)
                    .map(|f| f.label.clone())
                    .unwrap_or_else(|| "<нет шрифта>".to_string());
                let prev_font_idx = self.selected_font_idx;

                egui::ComboBox::from_label("Шрифт")
                    .selected_text(selected_text)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        for (idx, font) in self.fonts.iter().enumerate() {
                            ui.selectable_value(&mut self.selected_font_idx, idx, &font.label);
                        }
                    });
                if self.selected_font_idx != prev_font_idx {
                    self.selected_face_idx = 0;
                }

                let selected_face_text = self
                    .fonts
                    .get(self.selected_font_idx)
                    .and_then(|font| font.faces.get(self.selected_face_idx))
                    .map(|face| face.label.clone())
                    .unwrap_or_else(|| "<face>".to_string());
                egui::ComboBox::from_label("Face")
                    .selected_text(selected_face_text)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        if let Some(font) = self.fonts.get(self.selected_font_idx) {
                            for (idx, face) in font.faces.iter().enumerate() {
                                ui.selectable_value(&mut self.selected_face_idx, idx, &face.label);
                            }
                        }
                    });

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.force_bold, "Bold");
                    ui.checkbox(&mut self.force_italic, "Italic");
                });
                ui.checkbox(
                    &mut self.enable_inline_style_tags,
                    "Распознавать теги <b>/<i>",
                );

                ui.add(egui::Slider::new(&mut self.font_size_px, 1.0..=256.0).text("Размер (px)"));
                ui.add(
                    egui::Slider::new(&mut self.line_spacing_px, -300.0..=300.0)
                        .text("Межстрочный интервал (px)"),
                );
                ui.add(
                    egui::Slider::new(&mut self.line_spacing_percent, -300.0..=300.0)
                        .text("Межстрочный интервал (%)"),
                );
                ui.add(egui::Slider::new(&mut self.width_px, 50..=4000).text("Ширина (px)"));
                egui::ComboBox::from_label("Выравнивание")
                    .selected_text(match self.align {
                        HorizontalAlign::Left => "left",
                        HorizontalAlign::Center => "center",
                        HorizontalAlign::Right => "right",
                        HorizontalAlign::Justify => "justify",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.align, HorizontalAlign::Left, "left");
                        ui.selectable_value(&mut self.align, HorizontalAlign::Center, "center");
                        ui.selectable_value(&mut self.align, HorizontalAlign::Right, "right");
                        ui.selectable_value(&mut self.align, HorizontalAlign::Justify, "justify");
                    });
                egui::ComboBox::from_label("Форма текста")
                    .selected_text(match self.text_shape {
                        TextShape::Free => "free",
                        TextShape::Rectangle => "rectangle",
                        TextShape::Oval => "oval",
                        TextShape::Hexagon => "hexagon",
                        TextShape::SoftPeak => "soft peak",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.text_shape, TextShape::Free, "free");
                        ui.selectable_value(
                            &mut self.text_shape,
                            TextShape::Rectangle,
                            "rectangle",
                        );
                        ui.selectable_value(&mut self.text_shape, TextShape::Oval, "oval");
                        ui.selectable_value(&mut self.text_shape, TextShape::Hexagon, "hexagon");
                        ui.selectable_value(&mut self.text_shape, TextShape::SoftPeak, "soft peak");
                    });
                ui.add_enabled(
                    self.text_shape == TextShape::SoftPeak,
                    egui::Slider::new(&mut self.shape_variant, 1..=9).text("variant"),
                );
                ui.add_enabled(
                    matches!(self.text_shape, TextShape::Oval | TextShape::Hexagon),
                    egui::Slider::new(&mut self.shape_min_width_percent, 1.0..=100.0)
                        .text("Минимальная ширина (%)"),
                );
                ui.add_enabled(
                    self.text_shape != TextShape::Free,
                    egui::Checkbox::new(&mut self.strict_shape_fit, "Строго соблюдать форму"),
                );

                ui.separator();
                ui.heading("Эффекты");
                ui.label("Порядок применения: сверху вниз");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_label("Добавить эффект")
                        .selected_text(match self.effect_to_add {
                            AvailableEffectKind::Stroke => "stroke",
                            AvailableEffectKind::Shadow => "shadow",
                            AvailableEffectKind::GlowV1 => "glow_v1",
                            AvailableEffectKind::GlowV2 => "glow_v2",
                            AvailableEffectKind::Gradient2 => "gradient2",
                            AvailableEffectKind::Gradient4 => "gradient4",
                            AvailableEffectKind::Reflect => "reflect",
                            AvailableEffectKind::Shake => "shake",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.effect_to_add,
                                AvailableEffectKind::Stroke,
                                "stroke",
                            );
                            ui.selectable_value(
                                &mut self.effect_to_add,
                                AvailableEffectKind::Shadow,
                                "shadow",
                            );
                            ui.selectable_value(
                                &mut self.effect_to_add,
                                AvailableEffectKind::GlowV1,
                                "glow_v1",
                            );
                            ui.selectable_value(
                                &mut self.effect_to_add,
                                AvailableEffectKind::GlowV2,
                                "glow_v2",
                            );
                            ui.selectable_value(
                                &mut self.effect_to_add,
                                AvailableEffectKind::Gradient2,
                                "gradient2",
                            );
                            ui.selectable_value(
                                &mut self.effect_to_add,
                                AvailableEffectKind::Gradient4,
                                "gradient4",
                            );
                            ui.selectable_value(
                                &mut self.effect_to_add,
                                AvailableEffectKind::Reflect,
                                "reflect",
                            );
                            ui.selectable_value(
                                &mut self.effect_to_add,
                                AvailableEffectKind::Shake,
                                "shake",
                            );
                        });
                    if ui.button("+ Добавить").clicked() {
                        match self.effect_to_add {
                            AvailableEffectKind::Stroke => {
                                self.effects.push(EffectCard::Stroke(StrokeEffectCard {
                                    width_px: 2.0,
                                    color: Color32::BLACK,
                                    opacity_mode: StrokeOpacityMode::FromContour,
                                    transparency_percent: 0.0,
                                }));
                            }
                            AvailableEffectKind::Shadow => {
                                self.effects.push(EffectCard::Shadow(ShadowEffectCard {
                                    offset_x_px: 4,
                                    offset_y_px: 4,
                                    transparency_percent: 40.0,
                                    blur_radius_px: 0.0,
                                    color_mode: ShadowColorMode::SingleColor,
                                    color: Color32::BLACK,
                                }));
                            }
                            AvailableEffectKind::GlowV1 => {
                                self.effects.push(EffectCard::Glow(GlowEffectCard {
                                    version: GlowEffectVersion::V1,
                                    radius_px: 16.0,
                                    color: Color32::BLACK,
                                    opacity_mode: StrokeOpacityMode::FromContour,
                                    transparency_percent: 0.0,
                                    fade_strength: 0.0,
                                    fade_shift: 0.0,
                                }));
                            }
                            AvailableEffectKind::GlowV2 => {
                                self.effects.push(EffectCard::Glow(GlowEffectCard {
                                    version: GlowEffectVersion::V2,
                                    radius_px: 16.0,
                                    color: Color32::BLACK,
                                    opacity_mode: StrokeOpacityMode::FromContour,
                                    transparency_percent: 0.0,
                                    fade_strength: 0.0,
                                    fade_shift: 0.0,
                                }));
                            }
                            AvailableEffectKind::Gradient2 => {
                                self.effects
                                    .push(EffectCard::Gradient2(Gradient2EffectCard {
                                        color1: Color32::WHITE,
                                        color2: Color32::BLACK,
                                        angle_deg: 90.0,
                                        respect_source_alpha: true,
                                        fill_mode: Gradient2FillMode::AllOpaque,
                                        target_color: self.text_color,
                                    }));
                            }
                            AvailableEffectKind::Gradient4 => {
                                self.effects
                                    .push(EffectCard::Gradient4(Gradient4EffectCard {
                                        color_top_left: Color32::WHITE,
                                        color_top_right: Color32::WHITE,
                                        color_bottom_left: Color32::BLACK,
                                        color_bottom_right: Color32::BLACK,
                                        respect_source_alpha: true,
                                        fill_mode: Gradient4FillMode::AllOpaque,
                                        target_color: self.text_color,
                                    }));
                            }
                            AvailableEffectKind::Reflect => {
                                self.effects.push(EffectCard::Reflect(ReflectEffectCard {
                                    axis: ReflectAxis::Y,
                                }));
                            }
                            AvailableEffectKind::Shake => {
                                self.effects.push(EffectCard::Shake(ShakeEffectCard {
                                    angle_deg: 90.0,
                                    up_px: 0.0,
                                    down_px: 40.0,
                                    steps: 12,
                                    base_fade: 0.30,
                                    decay: 0.15,
                                    blur_px: 2,
                                    autogrow: true,
                                    grow_margin_px: 0,
                                }));
                            }
                        }
                    }
                });

                let mut move_up: Option<usize> = None;
                let mut move_down: Option<usize> = None;
                let mut remove_idx: Option<usize> = None;
                if self.effects.is_empty() {
                    ui.label("Эффекты не добавлены.");
                } else {
                    let effects_list_max_height =
                        (ui.available_height() - 220.0).clamp(120.0, 420.0);
                    egui::ScrollArea::vertical()
                        .id_salt("effects_cards_scroll")
                        .max_height(effects_list_max_height)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for idx in 0..self.effects.len() {
                                ui.group(|ui| {
                                    ui.push_id(("effect_card", idx), |ui| {
                                        ui.horizontal(|ui| {
                                            let title = match &self.effects[idx] {
                                                EffectCard::Stroke(_) => "stroke",
                                                EffectCard::Shadow(_) => "shadow",
                                                EffectCard::Glow(glow) => match glow.version {
                                                    GlowEffectVersion::V1 => "glow_v1",
                                                    GlowEffectVersion::V2 => "glow_v2",
                                                },
                                                EffectCard::Gradient2(_) => "gradient2",
                                                EffectCard::Gradient4(_) => "gradient4",
                                                EffectCard::Reflect(_) => "reflect",
                                                EffectCard::Shake(_) => "shake",
                                            };
                                            ui.label(format!("#{} {title}", idx + 1));
                                            if ui
                                                .add_enabled(idx > 0, egui::Button::new("Up"))
                                                .on_hover_text("Переместить вверх")
                                                .clicked()
                                            {
                                                move_up = Some(idx);
                                            }
                                            if ui
                                                .add_enabled(
                                                    idx + 1 < self.effects.len(),
                                                    egui::Button::new("Down"),
                                                )
                                                .on_hover_text("Переместить вниз")
                                                .clicked()
                                            {
                                                move_down = Some(idx);
                                            }
                                            if ui
                                                .button("X")
                                                .on_hover_text("Удалить эффект")
                                                .clicked()
                                            {
                                                remove_idx = Some(idx);
                                            }
                                        });

                                        match &mut self.effects[idx] {
                                            EffectCard::Stroke(stroke) => {
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut stroke.width_px,
                                                        0.0..=24.0,
                                                    )
                                                    .text("Ширина (px)"),
                                                );
                                                ui.horizontal(|ui| {
                                                    ui.label("Цвет:");
                                                    ui.color_edit_button_srgba(&mut stroke.color);
                                                });
                                                egui::ComboBox::from_label("Прозрачность контура")
                                                    .selected_text(match stroke.opacity_mode {
                                                        StrokeOpacityMode::Static => "Статическая",
                                                        StrokeOpacityMode::FromContour => {
                                                            "Прозрачность от контура"
                                                        }
                                                    })
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut stroke.opacity_mode,
                                                            StrokeOpacityMode::Static,
                                                            "Статическая",
                                                        );
                                                        ui.selectable_value(
                                                            &mut stroke.opacity_mode,
                                                            StrokeOpacityMode::FromContour,
                                                            "Прозрачность от контура",
                                                        );
                                                    });
                                                ui.add_enabled_ui(
                                                    stroke.opacity_mode
                                                        == StrokeOpacityMode::Static,
                                                    |ui| {
                                                        ui.add(
                                                            egui::Slider::new(
                                                                &mut stroke.transparency_percent,
                                                                0.0..=100.0,
                                                            )
                                                            .text("Прозрачность (%)"),
                                                        );
                                                    },
                                                );
                                            }
                                            EffectCard::Shadow(shadow) => {
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut shadow.offset_x_px,
                                                        -400..=400,
                                                    )
                                                    .text("Смещение X (px)"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut shadow.offset_y_px,
                                                        -400..=400,
                                                    )
                                                    .text("Смещение Y (px)"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut shadow.blur_radius_px,
                                                        0.0..=128.0,
                                                    )
                                                    .text("Размытие тени (px)"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut shadow.transparency_percent,
                                                        0.0..=100.0,
                                                    )
                                                    .text("Прозрачность тени (%)"),
                                                );
                                                egui::ComboBox::from_label("Режим цвета")
                                                    .selected_text(match shadow.color_mode {
                                                        ShadowColorMode::SingleColor => "Один цвет",
                                                        ShadowColorMode::SourceColors => {
                                                            "Исходные цвета"
                                                        }
                                                    })
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut shadow.color_mode,
                                                            ShadowColorMode::SingleColor,
                                                            "Один цвет",
                                                        );
                                                        ui.selectable_value(
                                                            &mut shadow.color_mode,
                                                            ShadowColorMode::SourceColors,
                                                            "Исходные цвета",
                                                        );
                                                    });
                                                ui.add_enabled_ui(
                                                    shadow.color_mode
                                                        == ShadowColorMode::SingleColor,
                                                    |ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label("Цвет тени:");
                                                            ui.color_edit_button_srgba(
                                                                &mut shadow.color,
                                                            );
                                                        });
                                                    },
                                                );
                                            }
                                            EffectCard::Glow(glow) => {
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut glow.radius_px,
                                                        0.0..=300.0,
                                                    )
                                                    .text("Радиус (px)"),
                                                );
                                                ui.horizontal(|ui| {
                                                    ui.label("Цвет:");
                                                    ui.color_edit_button_srgba(&mut glow.color);
                                                });
                                                egui::ComboBox::from_label("Прозрачность контура")
                                                    .selected_text(match glow.opacity_mode {
                                                        StrokeOpacityMode::Static => "Статическая",
                                                        StrokeOpacityMode::FromContour => {
                                                            "Прозрачность от контура"
                                                        }
                                                    })
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut glow.opacity_mode,
                                                            StrokeOpacityMode::Static,
                                                            "Статическая",
                                                        );
                                                        ui.selectable_value(
                                                            &mut glow.opacity_mode,
                                                            StrokeOpacityMode::FromContour,
                                                            "Прозрачность от контура",
                                                        );
                                                    });
                                                ui.add_enabled_ui(
                                                    glow.opacity_mode == StrokeOpacityMode::Static,
                                                    |ui| {
                                                        ui.add(
                                                            egui::Slider::new(
                                                                &mut glow.transparency_percent,
                                                                0.0..=100.0,
                                                            )
                                                            .text("Прозрачность (%)"),
                                                        );
                                                    },
                                                );
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut glow.fade_strength,
                                                        -100.0..=100.0,
                                                    )
                                                    .text("Сила затухания"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut glow.fade_shift,
                                                        -100.0..=100.0,
                                                    )
                                                    .text("Смещение затухания"),
                                                );
                                            }
                                            EffectCard::Gradient2(gradient) => {
                                                ui.horizontal(|ui| {
                                                    ui.label("Цвет 1:");
                                                    ui.color_edit_button_srgba(
                                                        &mut gradient.color1,
                                                    );
                                                });
                                                ui.horizontal(|ui| {
                                                    ui.label("Цвет 2:");
                                                    ui.color_edit_button_srgba(
                                                        &mut gradient.color2,
                                                    );
                                                });
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut gradient.angle_deg,
                                                        -360.0..=360.0,
                                                    )
                                                    .text("Угол поворота (°)"),
                                                );
                                                ui.checkbox(
                                                    &mut gradient.respect_source_alpha,
                                                    "Учитывать прозрачность",
                                                );
                                                egui::ComboBox::from_label("Тип заполнения")
                                                    .selected_text(match gradient.fill_mode {
                                                        Gradient2FillMode::AllOpaque => {
                                                            "Всё непрозрачное"
                                                        }
                                                        Gradient2FillMode::SpecificColor => {
                                                            "Конкретный цвет"
                                                        }
                                                    })
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut gradient.fill_mode,
                                                            Gradient2FillMode::AllOpaque,
                                                            "Всё непрозрачное",
                                                        );
                                                        ui.selectable_value(
                                                            &mut gradient.fill_mode,
                                                            Gradient2FillMode::SpecificColor,
                                                            "Конкретный цвет",
                                                        );
                                                    });
                                                ui.add_enabled_ui(
                                                    gradient.fill_mode
                                                        == Gradient2FillMode::SpecificColor,
                                                    |ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label("Заменяемый цвет:");
                                                            ui.color_edit_button_srgba(
                                                                &mut gradient.target_color,
                                                            );
                                                        });
                                                    },
                                                );
                                            }
                                            EffectCard::Gradient4(gradient) => {
                                                ui.horizontal(|ui| {
                                                    ui.label("Левый верх:");
                                                    ui.color_edit_button_srgba(
                                                        &mut gradient.color_top_left,
                                                    );
                                                });
                                                ui.horizontal(|ui| {
                                                    ui.label("Правый верх:");
                                                    ui.color_edit_button_srgba(
                                                        &mut gradient.color_top_right,
                                                    );
                                                });
                                                ui.horizontal(|ui| {
                                                    ui.label("Левый низ:");
                                                    ui.color_edit_button_srgba(
                                                        &mut gradient.color_bottom_left,
                                                    );
                                                });
                                                ui.horizontal(|ui| {
                                                    ui.label("Правый низ:");
                                                    ui.color_edit_button_srgba(
                                                        &mut gradient.color_bottom_right,
                                                    );
                                                });
                                                ui.checkbox(
                                                    &mut gradient.respect_source_alpha,
                                                    "Учитывать прозрачность",
                                                );
                                                egui::ComboBox::from_label("Тип заполнения")
                                                    .selected_text(match gradient.fill_mode {
                                                        Gradient4FillMode::AllOpaque => {
                                                            "Всё непрозрачное"
                                                        }
                                                        Gradient4FillMode::SpecificColor => {
                                                            "Конкретный цвет"
                                                        }
                                                    })
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut gradient.fill_mode,
                                                            Gradient4FillMode::AllOpaque,
                                                            "Всё непрозрачное",
                                                        );
                                                        ui.selectable_value(
                                                            &mut gradient.fill_mode,
                                                            Gradient4FillMode::SpecificColor,
                                                            "Конкретный цвет",
                                                        );
                                                    });
                                                ui.add_enabled_ui(
                                                    gradient.fill_mode
                                                        == Gradient4FillMode::SpecificColor,
                                                    |ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label("Заменяемый цвет:");
                                                            ui.color_edit_button_srgba(
                                                                &mut gradient.target_color,
                                                            );
                                                        });
                                                    },
                                                );
                                            }
                                            EffectCard::Reflect(reflect) => {
                                                egui::ComboBox::from_label("Ось отражения")
                                                    .selected_text(match reflect.axis {
                                                        ReflectAxis::X => "X (верх-низ)",
                                                        ReflectAxis::Y => "Y (лево-право)",
                                                    })
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut reflect.axis,
                                                            ReflectAxis::X,
                                                            "X (верх-низ)",
                                                        );
                                                        ui.selectable_value(
                                                            &mut reflect.axis,
                                                            ReflectAxis::Y,
                                                            "Y (лево-право)",
                                                        );
                                                    });
                                            }
                                            EffectCard::Shake(shake) => {
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut shake.angle_deg,
                                                        -360.0..=360.0,
                                                    )
                                                    .text("Угол (°)"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut shake.up_px,
                                                        0.0..=1000.0,
                                                    )
                                                    .text("Амплитуда вверх (px)"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut shake.down_px,
                                                        0.0..=1000.0,
                                                    )
                                                    .text("Амплитуда вниз (px)"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(&mut shake.steps, 0..=128)
                                                        .text("Шаги"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(
                                                        &mut shake.base_fade,
                                                        0.0..=1.0,
                                                    )
                                                    .text("Базовое затухание"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(&mut shake.decay, 0.0..=1.0)
                                                        .text("Спад шага"),
                                                );
                                                ui.add(
                                                    egui::Slider::new(&mut shake.blur_px, 0..=64)
                                                        .text("Blur (px)"),
                                                );
                                                ui.checkbox(
                                                    &mut shake.autogrow,
                                                    "Auto-grow canvas",
                                                );
                                                ui.add_enabled_ui(shake.autogrow, |ui| {
                                                    ui.add(
                                                        egui::Slider::new(
                                                            &mut shake.grow_margin_px,
                                                            0..=1024,
                                                        )
                                                        .text("Доп. отступ (px)"),
                                                    );
                                                });
                                            }
                                        }
                                    });
                                });
                            }
                        });
                }

                if let Some(idx) = remove_idx {
                    self.effects.remove(idx);
                }
                if let Some(idx) = move_up {
                    self.effects.swap(idx - 1, idx);
                }
                if let Some(idx) = move_down {
                    self.effects.swap(idx, idx + 1);
                }

                ui.collapsing("JSON эффектов", |ui| {
                    let mut json_preview = self.effects_json();
                    ui.add(
                        egui::TextEdit::multiline(&mut json_preview)
                            .desired_rows(8)
                            .desired_width(f32::INFINITY)
                            .interactive(false),
                    );
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Render").clicked() {
                        self.queue_render();
                    }

                    if ui.button("Save PNG...").clicked() {
                        self.save_last_png();
                    }
                });

                if self.render_in_flight {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Рендерится...");
                    });
                }

                ui.separator();
                ui.label(&self.status_line);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(texture) = &self.preview_texture {
                        let response = ui.image((
                            texture.id(),
                            Vec2::new(self.preview_size[0] as f32, self.preview_size[1] as f32),
                        ));
                        ui.painter().rect_stroke(
                            response.rect,
                            0.0,
                            Stroke::new(1.0, Color32::RED),
                            egui::StrokeKind::Middle,
                        );
                    } else {
                        ui.label("Нет изображения. Нажмите Render.");
                    }
                });
        });

        if self.render_in_flight {
            ctx.request_repaint();
        }
    }
}

fn spawn_render_worker() -> (Sender<RenderJob>, Receiver<RenderResult>) {
    let (request_tx, request_rx) = mpsc::channel::<RenderJob>();
    let (result_tx, result_rx) = mpsc::channel::<RenderResult>();

    let _ = thread::Builder::new()
        .name("text-render-worker".to_string())
        .spawn(move || {
            while let Ok(mut job) = request_rx.recv() {
                while let Ok(newer_job) = request_rx.try_recv() {
                    job = newer_job;
                }

                let result = render_text_to_image(&job.params);
                if result_tx
                    .send(RenderResult {
                        token: job.token,
                        image: result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

    (request_tx, result_rx)
}

fn resolve_fonts_dir() -> PathBuf {
    if let Ok(cwd) = env::current_dir() {
        let candidate = cwd.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }

    if let Ok(exe_path) = env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }

    PathBuf::from("fonts")
}

fn load_fonts_from_dir(fonts_dir: &Path) -> Vec<FontEntry> {
    let mut files = Vec::<PathBuf>::new();
    collect_font_files_recursive(fonts_dir, &mut files);
    files.sort_by_key(|path| path.to_string_lossy().to_lowercase());

    let mut entries = Vec::<FontEntry>::with_capacity(files.len());
    let mut used_labels = std::collections::HashMap::<String, usize>::new();
    for path in files {
        let stem = path
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("font")
            .to_string();
        let count = used_labels.entry(stem.clone()).or_insert(0);
        *count += 1;
        let label = if *count > 1 {
            format!("{stem} ({count})")
        } else {
            stem
        };
        let faces = load_font_faces(&path);
        entries.push(FontEntry { label, path, faces });
    }

    entries
}

fn load_font_faces(path: &Path) -> Vec<FontFaceEntry> {
    let Ok(bytes) = fs::read(path) else {
        return vec![FontFaceEntry {
            label: "Face 0".to_string(),
            face_index: 0,
        }];
    };

    let mut db = fontdb::Database::new();
    let ids = db.load_font_source(fontdb::Source::Binary(Arc::new(bytes)));
    if ids.is_empty() {
        return vec![FontFaceEntry {
            label: "Face 0".to_string(),
            face_index: 0,
        }];
    }

    let mut faces = Vec::with_capacity(ids.len());
    for (idx, id) in ids.iter().enumerate() {
        let label = if let Some(face) = db.face(*id) {
            let family = face
                .families
                .first()
                .map(|(name, _)| name.as_str())
                .unwrap_or("Unknown");
            let style = match face.style {
                fontdb::Style::Normal => "Normal",
                fontdb::Style::Italic => "Italic",
                fontdb::Style::Oblique => "Oblique",
            };
            format!(
                "#{idx} {family} | {style} | w{} | {}",
                face.weight.0, face.post_script_name
            )
        } else {
            format!("#{idx} Face")
        };
        faces.push(FontFaceEntry {
            label,
            face_index: idx,
        });
    }

    if faces.is_empty() {
        faces.push(FontFaceEntry {
            label: "Face 0".to_string(),
            face_index: 0,
        });
    }
    faces
}

fn collect_font_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };

    for entry_result in read_dir {
        let Ok(entry) = entry_result else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            collect_font_files_recursive(&path, out);
            continue;
        }

        let ext = path
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "ttf" | "otf" | "ttc") {
            out.push(path);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([980.0, 620.0]),
        ..Default::default()
    };

    eframe::run_native(
        APP_TITLE,
        native_options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(TextRenderTestApp::new(cc)))
        }),
    )
    .map_err(|err| anyhow::anyhow!(err.to_string()))
}
