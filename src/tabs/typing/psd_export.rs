/*
FILE HEADER (tabs/typing/psd_export.rs)
- Назначение: экспорт страницы вкладки «Текст» в формат .psd через крейт `ag-psd`.
- Стек слоёв (снизу вверх): «Источник» (растр страницы) → «Клин» (если есть) →
  текстовые оверлеи. Каждый оверлей превращается в один или два слоя (см. ниже).
- composite (image_data документа) строится тем же кодом, что и PNG-экспорт
  (`flatten_typing_export_page_rgba`), чтобы превью PSD совпадало с PNG.

Случай A (overlay.deform_mesh.is_none(), чистый аффин): один ВИДИМЫЙ текстовый слой
  с растровым превью (запечённый аффинный вид) + редактируемые данные текста (TySh).
Случай B (overlay.deform_mesh.is_some(), не-аффинная деформация): два слоя —
  СКРЫТЫЙ текстовый слой (аффинное превью + редактируемый текст) и ВИДИМЫЙ растровый
  слой с полностью деформированным по мешу видом (без текстовых данных).
*/

use ag_psd::psd::{
    BlendMode, Color, ColorMode, Font, Justification, Layer, LayerAdditionalInfo, LayerTextData,
    ParagraphStyle, PixelData, Psd, Rgb, TextStyle, WriteOptions,
};
use ag_psd::write_psd;
use serde_json::Value;
use std::path::Path;

use super::render_next::{resolve_font_family_name, resolve_font_postscript_name};

use super::tab::{
    composite_overlay_at_page_position_over, composite_overlay_mesh_over_page,
    direct_overlay_blit_top_left_px, export_overlay_clipped_rgba,
    export_overlay_deform_mesh_for_page, flatten_typing_export_page_rgba,
    TypingExportOverlaySnapshot, TypingExportPageJob, TypingOverlayDeformMesh,
};

/// Публичная точка входа: собирает `Psd` для одной страницы и возвращает байты файла.
pub(super) fn export_typing_single_page_psd(job: &TypingExportPageJob) -> Result<Vec<u8>, String> {
    // Источник страницы (RGBA8) на полном разрешении.
    let source = image::open(&job.page_path)
        .map_err(|err| {
            format!(
                "Не удалось открыть страницу {}: {err}",
                job.page_path.display()
            )
        })?
        .to_rgba8();
    let page_w = source.width() as usize;
    let page_h = source.height() as usize;
    let source_rgba = source.into_raw();

    // Клин (если есть) растеризуем на полностраничный прозрачный буфер, чтобы
    // получить отдельный слой клина на разрешении страницы.
    let clean_rgba = job.clean_overlay_rgba.as_ref().map(|clean| {
        let mut buf = vec![0u8; page_w * page_h * 4];
        super::tab::composite_overlay_full_image_over(
            &mut buf,
            [page_w, page_h],
            clean.as_raw(),
            [clean.width() as usize, clean.height() as usize],
        );
        buf
    });

    // composite (плоский финальный кадр, как у PNG-экспорта).
    let (composite, comp_w, comp_h) = flatten_typing_export_page_rgba(job)?;
    debug_assert_eq!((comp_w, comp_h), (page_w, page_h));

    let psd = build_typing_page_psd(job, page_w, page_h, source_rgba, clean_rgba, composite);

    let options = WriteOptions {
        // Сохраняем наши превью-пиксели текстовых слоёв, чтобы Photoshop не
        // перерисовывал текст сразу при открытии.
        invalidate_text_layers: Some(false),
        ..Default::default()
    };
    Ok(write_psd(&psd, &options))
}

/// Чистая сборка `Psd` из подготовленных входов — без обращения к диску, чтобы
/// функцию можно было покрыть unit-тестом.
pub(super) fn build_typing_page_psd(
    job: &TypingExportPageJob,
    page_w: usize,
    page_h: usize,
    source_rgba: Vec<u8>,
    clean_rgba: Option<Vec<u8>>,
    composite: Vec<u8>,
) -> Psd {
    let mut layers: Vec<Layer> = Vec::new();

    // 1. Слой-источник (самый нижний).
    layers.push(full_page_layer(
        "Источник",
        page_w,
        page_h,
        source_rgba,
        None,
    ));

    // 2. Слой-клин (если присутствует).
    if let Some(clean) = clean_rgba {
        layers.push(full_page_layer("Клин", page_w, page_h, clean, None));
    }

    // 3. Текстовые оверлеи группируются по слою в группы «Слой текста {N}».
    // `job.overlays` уже отсортированы по (layer_idx, вертикаль), поэтому оверлеи
    // одного слоя идут подряд, а внутри слоя — снизу вверх (нижний на картинке
    // оказывается сверху в стопке).
    let mut text_index = 0usize;
    let mut current_layer: Option<usize> = None;
    let mut current_group: Vec<Layer> = Vec::new();
    let flush_group = |layers: &mut Vec<Layer>,
                       layer_idx: Option<usize>,
                       group: &mut Vec<Layer>| {
        if let Some(layer_idx) = layer_idx
            && !group.is_empty()
        {
            layers.push(text_group_layer(
                &format!("Слой текста {layer_idx}"),
                std::mem::take(group),
            ));
        }
    };
    for overlay in &job.overlays {
        if overlay.page_idx != job.page_idx {
            continue;
        }
        if current_layer != Some(overlay.layer_idx) {
            flush_group(&mut layers, current_layer, &mut current_group);
            current_layer = Some(overlay.layer_idx);
        }
        text_index += 1;
        let deform_mesh = export_overlay_deform_mesh_for_page(overlay, [page_w, page_h]);
        let clipped_rgba = export_overlay_clipped_rgba(job, overlay, &deform_mesh);
        let text_data = build_layer_text_data(overlay);

        if overlay.deform_mesh.is_none() {
            // CASE A: один видимый текстовый слой с запечённым аффинным превью.
            let baked = bake_affine_overlay(overlay, &clipped_rgba, page_w, page_h, &deform_mesh);
            let layer = make_baked_layer(
                &format!("Текст {text_index}"),
                page_w,
                page_h,
                &baked,
                Some(false),
                Some(text_data),
            );
            current_group.push(layer);
        } else {
            // CASE B: скрытый текстовый слой (снизу) + видимый растровый (сверху).
            let affine_baked =
                bake_affine_overlay(overlay, &clipped_rgba, page_w, page_h, &deform_mesh);
            let hidden_text_layer = make_baked_layer(
                &format!("Текст {text_index} (текст)"),
                page_w,
                page_h,
                &affine_baked,
                Some(true),
                Some(text_data),
            );
            current_group.push(hidden_text_layer);

            // Видимый растровый слой с полной деформацией по мешу.
            let mut mesh_buf = vec![0u8; page_w * page_h * 4];
            composite_overlay_mesh_over_page(
                &mut mesh_buf,
                [page_w, page_h],
                clipped_rgba.as_slice(),
                overlay.size_px,
                &deform_mesh,
            );
            let raster_layer = make_baked_layer(
                &format!("Текст {text_index} (растр)"),
                page_w,
                page_h,
                &mesh_buf,
                Some(false),
                None,
            );
            current_group.push(raster_layer);
        }
    }
    flush_group(&mut layers, current_layer, &mut current_group);

    Psd {
        width: page_w as f64,
        height: page_h as f64,
        color_mode: Some(ColorMode::Rgb),
        channels: Some(4.0),
        bits_per_channel: Some(8.0),
        children: Some(layers),
        image_data: Some(PixelData {
            width: page_w as u32,
            height: page_h as u32,
            data: composite,
        }),
        ..Default::default()
    }
}

/// Слой-группа, объединяющая переданные текстовые слои.
fn text_group_layer(name: &str, children: Vec<Layer>) -> Layer {
    Layer {
        additional_info: LayerAdditionalInfo {
            name: Some(name.to_string()),
            ..Default::default()
        },
        blend_mode: Some(BlendMode::PassThrough),
        opacity: Some(1.0),
        hidden: Some(false),
        children: Some(children),
        opened: Some(true),
        ..Default::default()
    }
}

/// Слой во весь размер страницы.
fn full_page_layer(
    name: &str,
    page_w: usize,
    page_h: usize,
    rgba: Vec<u8>,
    text: Option<LayerTextData>,
) -> Layer {
    Layer {
        additional_info: LayerAdditionalInfo {
            name: Some(name.to_string()),
            text,
            ..Default::default()
        },
        top: Some(0.0),
        left: Some(0.0),
        bottom: Some(page_h as f64),
        right: Some(page_w as f64),
        blend_mode: Some(BlendMode::Normal),
        opacity: Some(1.0),
        hidden: Some(false),
        image_data: Some(PixelData {
            width: page_w as u32,
            height: page_h as u32,
            data: rgba,
        }),
        ..Default::default()
    }
}

/// Слой из полностраничного запечённого буфера, обрезанный до непрозрачного bbox.
fn make_baked_layer(
    name: &str,
    page_w: usize,
    page_h: usize,
    page_buf: &[u8],
    hidden: Option<bool>,
    text: Option<LayerTextData>,
) -> Layer {
    let (data, left, top, right, bottom) = trim_to_bbox(page_buf, page_w, page_h);
    Layer {
        additional_info: LayerAdditionalInfo {
            name: Some(name.to_string()),
            text,
            ..Default::default()
        },
        top: Some(top as f64),
        left: Some(left as f64),
        bottom: Some(bottom as f64),
        right: Some(right as f64),
        blend_mode: Some(BlendMode::Normal),
        opacity: Some(1.0),
        hidden,
        image_data: Some(PixelData {
            width: (right - left) as u32,
            height: (bottom - top) as u32,
            data,
        }),
        ..Default::default()
    }
}

/// Запекает аффинный (без меша) вид оверлея на полностраничный прозрачный буфер.
/// Если оверлей «прямой» (угол≈0, масштаб≈1) — прямой блит, иначе меш-растеризация.
fn bake_affine_overlay(
    overlay: &TypingExportOverlaySnapshot,
    clipped_rgba: &[u8],
    page_w: usize,
    page_h: usize,
    deform_mesh: &TypingOverlayDeformMesh,
) -> Vec<u8> {
    let mut buf = vec![0u8; page_w * page_h * 4];
    if let Some(top_left_px) = direct_overlay_blit_top_left_px(overlay) {
        composite_overlay_at_page_position_over(
            &mut buf,
            [page_w, page_h],
            clipped_rgba,
            overlay.size_px,
            top_left_px,
        );
    } else {
        // Для случая A здесь deform_mesh — это дефолтный меш из аффина (rotate/scale).
        composite_overlay_mesh_over_page(
            &mut buf,
            [page_w, page_h],
            clipped_rgba,
            overlay.size_px,
            deform_mesh,
        );
    }
    buf
}

/// Обрезает полностраничный RGBA8 буфер до bbox непрозрачных пикселей.
/// Возвращает (data, left, top, right, bottom). Если всё прозрачно — слой 1×1.
fn trim_to_bbox(page_buf: &[u8], page_w: usize, page_h: usize) -> (Vec<u8>, usize, usize, usize, usize) {
    let mut min_x = page_w;
    let mut min_y = page_h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut found = false;
    for y in 0..page_h {
        for x in 0..page_w {
            let a = page_buf[(y * page_w + x) * 4 + 3];
            if a != 0 {
                found = true;
                if x < min_x {
                    min_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if x > max_x {
                    max_x = x;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }
    if !found {
        // Полностью прозрачный слой: отдаём минимальный 1×1 пиксель.
        return (vec![0u8; 4], 0, 0, 1, 1);
    }
    let left = min_x;
    let top = min_y;
    let right = max_x + 1;
    let bottom = max_y + 1;
    let out_w = right - left;
    let out_h = bottom - top;
    let mut data = vec![0u8; out_w * out_h * 4];
    for y in 0..out_h {
        let src_row = ((top + y) * page_w + left) * 4;
        let dst_row = (y * out_w) * 4;
        data[dst_row..dst_row + out_w * 4]
            .copy_from_slice(&page_buf[src_row..src_row + out_w * 4]);
    }
    (data, left, top, right, bottom)
}

/// Строит редактируемые данные текстового слоя (TySh) из render_data_json оверлея.
fn build_layer_text_data(overlay: &TypingExportOverlaySnapshot) -> LayerTextData {
    let params = overlay
        .render_data_json
        .as_ref()
        .and_then(|v| v.get("text_params"));

    // Сформированный текст (если задан и непуст) идёт в слой вместо исходного —
    // так же, как он подставляется в рендер (см. `text_render_params_from_render_data`).
    let text = params
        .and_then(|p| p.get("formed_text"))
        .and_then(Value::as_str)
        .filter(|formed| !formed.trim().is_empty())
        .or_else(|| params.and_then(|p| p.get("text")).and_then(Value::as_str))
        .unwrap_or("")
        .to_string();

    // font_size_px трактуем как пункты — приемлемая аппроксимация (px ≈ pt).
    let font_size = params
        .and_then(|p| p.get("font_size_px"))
        .and_then(Value::as_f64)
        .unwrap_or(24.0);

    // Цвет: [r,g,b,a] 0..255. fill_color ожидает 0..255 (encode делит на 255).
    let fill_color = params
        .and_then(|p| p.get("text_color"))
        .and_then(Value::as_array)
        .map(|arr| {
            let comp = |i: usize| arr.get(i).and_then(Value::as_f64).unwrap_or(0.0);
            Color::Rgb(Rgb {
                r: comp(0),
                g: comp(1),
                b: comp(2),
            })
        })
        .unwrap_or(Color::Rgb(Rgb {
            r: 0.0,
            g: 0.0,
            b: 0.0,
        }));

    // Имя шрифта для Photoshop. PS сопоставляет шрифт текстового слоя по его
    // PostScript-имени (OpenType name table id 6, напр. `ArialMT`). Цепочка
    // фолбэков (побеждает первый непустой):
    //   1. реальное PostScript-имя (id 6), прочитанное из файла шрифта через fontdb;
    //   2. имя семейства (id 1), если PostScript-имя недоступно;
    //   3. последний `|`-сегмент `font_label` (в нём уже лежит PostScript-имя);
    //   4. file stem пути шрифта;
    //   5. `"MyriadPro-Regular"`.
    let font_path = params.and_then(|p| p.get("font_path")).and_then(Value::as_str);
    let face_index = params
        .and_then(|p| p.get("selected_face_index"))
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(0);

    let font_name = font_path
        // 1. PostScript-имя из файла шрифта.
        .and_then(|path| resolve_font_postscript_name(path, face_index))
        // 2. имя семейства из файла шрифта.
        .or_else(|| {
            font_path.and_then(|path| {
                resolve_font_family_name(path, face_index)
            })
        })
        // 3. последний `|`-сегмент font_label (PostScript-имя в UI-метке).
        .or_else(|| {
            params
                .and_then(|p| p.get("font_label"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|label| {
                    label
                        .rsplit('|')
                        .next()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(label)
                        .to_string()
                })
        })
        // 4. file stem пути шрифта.
        .or_else(|| {
            font_path.and_then(|raw| {
                Path::new(raw)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
        })
        // 5. последний резерв.
        .unwrap_or_else(|| "MyriadPro-Regular".to_string());

    let justification = match params
        .and_then(|p| p.get("align"))
        .and_then(Value::as_str)
        .unwrap_or("left")
    {
        "center" => Justification::Center,
        "right" => Justification::Right,
        "justify" => Justification::JustifyAll,
        _ => Justification::Left,
    };

    // Аффинное преобразование [a,b,c,d,tx,ty].
    let theta = (overlay.angle_deg as f64).to_radians();
    let s = (overlay.user_scale as f64).max(0.01);
    let (sin_t, cos_t) = theta.sin_cos();
    let center_x = overlay.center_page_px[0] as f64;
    let center_y = overlay.center_page_px[1] as f64;
    let transform = vec![
        s * cos_t,
        s * sin_t,
        -s * sin_t,
        s * cos_t,
        center_x,
        center_y,
    ];

    // Локальные границы текстового блока вокруг центра (без масштаба — масштаб в transform).
    let half_w = overlay.size_px[0] as f64 * 0.5;
    let half_h = overlay.size_px[1] as f64 * 0.5;

    LayerTextData {
        text,
        transform: Some(transform),
        left: Some(-half_w),
        top: Some(-half_h),
        right: Some(half_w),
        bottom: Some(half_h),
        style: Some(TextStyle {
            font: Some(Font {
                name: font_name,
                ..Default::default()
            }),
            font_size: Some(font_size),
            fill_color: Some(fill_color),
            fill_flag: Some(true),
            ..Default::default()
        }),
        paragraph_style: Some(ParagraphStyle {
            justification: Some(justification),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::typing::tab::{
        TypingExportFormat, TypingExportOverlaySnapshot, TypingExportPageJob,
        TypingOverlayDeformMesh,
    };
    use ag_psd::read_psd;
    use ag_psd::psd::ReadOptions;
    use serde_json::json;
    use std::path::PathBuf;

    fn solid_overlay_rgba(w: usize, h: usize, color: [u8; 4]) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for px in v.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        v
    }

    /// Реальный шрифт из репозитория с известным PostScript-именем `MaybugMSRegular`
    /// (семейство — `Maybug MS`, т.е. PostScript-имя отличается от family и file stem,
    /// что делает проверку осмысленной).
    fn maybug_font_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fonts")
            .join("MaybugMSRegular.ttf")
    }

    #[test]
    fn build_psd_layers_and_roundtrip() {
        let page_w = 32usize;
        let page_h = 24usize;

        let font_path = maybug_font_path();

        // Оверлей A: чистый аффин с текстом.
        let ov_a = TypingExportOverlaySnapshot {
            page_idx: 0,
            center_page_px: [10.0, 8.0],
            mask_clip_enabled: false,
            layer_idx: 0,
            user_scale: 1.0,
            angle_deg: 0.0,
            deform_mesh: None,
            size_px: [6, 4],
            source_rgba: solid_overlay_rgba(6, 4, [255, 0, 0, 255]),
            render_data_json: Some(json!({
                "text_params": {
                    "text": "Привет",
                    "text_color": [10, 20, 30, 255],
                    "font_size_px": 18.0,
                    "align": "center",
                    // Декорированная UI-метка не должна использоваться как имя шрифта,
                    // пока font_path указывает на реальный файл.
                    "font_label": "#0 Maybug | Normal | w400 | WRONG-LABEL-NAME",
                    "font_path": font_path.to_string_lossy(),
                    "selected_face_index": 0
                }
            })),
            uid: "ov-a".into(),
            band_z: 0,
        };

        // Оверлей B: с деформирующим мешем.
        let mesh = TypingOverlayDeformMesh::new(
            2,
            2,
            vec![[18.0, 14.0], [28.0, 14.0], [18.0, 22.0], [30.0, 24.0]],
            [page_w, page_h],
        )
        .expect("mesh");
        let ov_b = TypingExportOverlaySnapshot {
            page_idx: 0,
            center_page_px: [23.0, 18.0],
            mask_clip_enabled: false,
            layer_idx: 0,
            user_scale: 1.0,
            angle_deg: 0.0,
            deform_mesh: Some(mesh),
            size_px: [6, 4],
            source_rgba: solid_overlay_rgba(6, 4, [0, 255, 0, 255]),
            render_data_json: Some(json!({
                "text_params": {
                    "text": "Мир",
                    "text_color": [200, 100, 50, 255],
                    "font_size_px": 12.0,
                    "align": "left",
                    "font_label": "Comic Sans"
                }
            })),
            uid: "ov-b".into(),
            band_z: 0,
        };

        let job = TypingExportPageJob {
            page_idx: 0,
            page_path: PathBuf::from("unused.png"),
            output_path: PathBuf::from("unused.psd"),
            clean_overlay_path: None,
            clean_overlay_rgba: None,
            overlays: vec![ov_a, ov_b],
            rasters: Vec::new(),
            mask: None,
            export_format: TypingExportFormat::Psd,
            layers_primary_dir: None,
            layers_fallback_dir: None,
        };

        let source_rgba = solid_overlay_rgba(page_w, page_h, [255, 255, 255, 255]);
        let clean_rgba = Some(solid_overlay_rgba(page_w, page_h, [0, 0, 0, 128]));
        let composite = solid_overlay_rgba(page_w, page_h, [128, 128, 128, 255]);

        let psd = build_typing_page_psd(
            &job,
            page_w,
            page_h,
            source_rgba,
            clean_rgba,
            composite,
        );

        // Базовые свойства документа.
        assert_eq!(psd.width as usize, page_w);
        assert_eq!(psd.height as usize, page_h);

        let children = psd.children.as_ref().expect("children");
        // source + clean + группа «Слой текста 0» = 3 верхнеуровневых слоя.
        assert_eq!(children.len(), 3);

        // Нижний слой — источник.
        assert_eq!(children[0].additional_info.name.as_deref(), Some("Источник"));
        assert!(children[0].additional_info.text.is_none());
        // Слой клина присутствует.
        assert_eq!(children[1].additional_info.name.as_deref(), Some("Клин"));

        // Группа текстового слоя 0 поверх клина.
        let group = &children[2];
        assert_eq!(
            group.additional_info.name.as_deref(),
            Some("Слой текста 0")
        );
        let text_layers = group.children.as_ref().expect("group children");
        // (A: 1) + (B: 2) = 3 слоя внутри группы.
        assert_eq!(text_layers.len(), 3);

        // Слой A — видимый текстовый (выше A на картинке → ниже в стопке группы).
        let a = &text_layers[0];
        assert_eq!(a.hidden, Some(false));
        let a_text = a.additional_info.text.as_ref().expect("A text");
        assert_eq!(a_text.text, "Привет");
        // Имя шрифта взято из реального файла (PostScript id 6), а не из UI-метки.
        let a_font_name = a_text
            .style
            .as_ref()
            .and_then(|s| s.font.as_ref())
            .map(|f| f.name.as_str())
            .expect("A font");
        assert_eq!(a_font_name, "MaybugMSRegular");

        // Слой B: скрытый текстовый + видимый растр.
        let b_text = &text_layers[1];
        assert_eq!(b_text.hidden, Some(true));
        assert!(b_text.additional_info.text.is_some());
        assert_eq!(
            b_text.additional_info.text.as_ref().unwrap().text,
            "Мир"
        );
        let b_raster = &text_layers[2];
        assert_eq!(b_raster.hidden, Some(false));
        assert!(b_raster.additional_info.text.is_none());

        // Запись + перечитывание (round-trip).
        let bytes = write_psd(
            &psd,
            &WriteOptions {
                invalidate_text_layers: Some(false),
                ..Default::default()
            },
        );
        let read = read_psd(&bytes, &ReadOptions::default()).expect("read_psd");
        assert_eq!(read.width as usize, page_w);
        assert_eq!(read.height as usize, page_h);
        let read_children = read.children.as_ref().expect("read children");
        assert_eq!(read_children.len(), 3);
        assert_eq!(
            read_children[0].additional_info.name.as_deref(),
            Some("Источник")
        );
        // Группа текста пережила round-trip вместе с вложенными слоями.
        let read_group = &read_children[2];
        assert_eq!(
            read_group.additional_info.name.as_deref(),
            Some("Слой текста 0")
        );
        let read_text_layers = read_group.children.as_ref().expect("read group children");
        assert_eq!(read_text_layers.len(), 3);
        // Текстовые данные пережили round-trip.
        let read_a_text = read_text_layers[0]
            .additional_info
            .text
            .as_ref()
            .expect("read A text");
        assert_eq!(read_a_text.text, "Привет");
    }

    /// Резолвер читает реальное PostScript-имя из файла шрифта.
    #[test]
    fn resolver_reads_real_postscript_name() {
        let path = maybug_font_path();
        let resolved = resolve_font_postscript_name(&path.to_string_lossy(), 0);
        assert_eq!(resolved.as_deref(), Some("MaybugMSRegular"));
        // Семейство отличается — фолбэк id 1.
        let family = resolve_font_family_name(&path.to_string_lossy(), 0);
        assert_eq!(family.as_deref(), Some("Maybug MS"));
    }

    /// При неверном пути цепочка фолбэков доходит до PostScript-сегмента font_label.
    #[test]
    fn font_name_falls_back_to_label_postscript_segment() {
        let overlay = TypingExportOverlaySnapshot {
            page_idx: 0,
            center_page_px: [0.0, 0.0],
            mask_clip_enabled: false,
            layer_idx: 0,
            user_scale: 1.0,
            angle_deg: 0.0,
            deform_mesh: None,
            size_px: [4, 4],
            source_rgba: solid_overlay_rgba(4, 4, [0, 0, 0, 255]),
            render_data_json: Some(json!({
                "text_params": {
                    "text": "x",
                    "font_label": "#0 Arial | Normal | w400 | ArialMT",
                    "font_path": "/definitely/not/a/real/font.ttf"
                }
            })),
            uid: "ov-c".into(),
            band_z: 0,
        };
        let td = build_layer_text_data(&overlay);
        let name = td
            .style
            .and_then(|s| s.font)
            .map(|f| f.name)
            .expect("font name");
        assert_eq!(name, "ArialMT");
    }

    /// Без font_label и с неверным путём — фолбэк на file stem пути.
    #[test]
    fn font_name_falls_back_to_path_stem() {
        let overlay = TypingExportOverlaySnapshot {
            page_idx: 0,
            center_page_px: [0.0, 0.0],
            mask_clip_enabled: false,
            layer_idx: 0,
            user_scale: 1.0,
            angle_deg: 0.0,
            deform_mesh: None,
            size_px: [4, 4],
            source_rgba: solid_overlay_rgba(4, 4, [0, 0, 0, 255]),
            render_data_json: Some(json!({
                "text_params": {
                    "text": "x",
                    "font_path": "/nope/SomeFont-Bold.ttf"
                }
            })),
            uid: "ov-d".into(),
            band_z: 0,
        };
        let td = build_layer_text_data(&overlay);
        let name = td
            .style
            .and_then(|s| s.font)
            .map(|f| f.name)
            .expect("font name");
        assert_eq!(name, "SomeFont-Bold");
    }
}
