/*
FILE HEADER (tabs/typing/psd_export.rs)
- Назначение: экспорт страницы вкладки «Текст» в формат .psd через крейт `ag-psd`.
- Стек слоёв (снизу вверх): «Источник» (растр страницы) → «Клин» (если есть) →
  текстовые оверлеи. Каждый оверлей превращается в один или два слоя (см. ниже).
- composite (image_data документа) строится тем же кодом, что и PNG-экспорт
  (`flatten_typing_export_page_rgba`), чтобы превью PSD совпадало с PNG.
- Имя шрифта для Photoshop берётся из ИДЕНТИЧНОСТИ оверлея через снимок
  `FontPostScriptNames` (`identity -> PostScript name по начертаниям`), который панель
  кладёт в задание экспорта: файл шрифта здесь НЕ открывается. КАКОЙ хранимый ключ даёт
  идентичность, решает схема САМОГО документа — ровно как в кодеке: схема 2 читает только
  `font`, схема 1 идёт по исторической цепочке из владельца схемы
  (`text_params_schema::legacy_font_name_candidates`).
- ОГРАНИЧЕНИЕ ФОРМАТА (устранить нельзя, поэтому О НЁМ СООБЩАЕТСЯ): `.psd` хранит ИМЯ
  шрифта, а идентичность приложения различает шрифты по СОДЕРЖИМОМУ. Два установленных
  файла с одним объявленным PostScript-именем и разными байтами — это два шрифта
  (`X%1111…` / `X%9999…`), растр запечён байтами выбранного, но в PSD оба уходят под
  голым `X`, и Photoshop может привязать РЕДАКТИРУЕМЫЙ слой к другому файлу. Имя всё
  равно записывается (потерять шрифт хуже), а факт неоднозначности возвращается наверх
  как `AmbiguousExportFont`: пишется в лог с контекстом и доходит до пользователя
  предупреждением в строке состояния экспорта.

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
use std::collections::{BTreeSet, HashMap};

use super::panel::text_params_schema;

use super::tab::{
    composite_overlay_at_page_position_over, composite_overlay_mesh_over_page,
    direct_overlay_blit_top_left_px, export_overlay_clipped_rgba,
    export_overlay_deform_mesh_for_page, flatten_typing_export_page_rgba,
    TypingExportOverlaySnapshot, TypingExportPageJob, TypingOverlayDeformMesh,
};

/// Snapshot of `font identity -> PostScript name per face` for the typing panel's
/// current font list.
///
/// Photoshop matches a text layer's font by its PostScript name (`name` table id 6), so
/// the export needs a real PostScript name — but the export job carries neither the font
/// list nor the provider, and re-reading the font file per overlay (what the removed
/// `resolve_font_postscript_name`/`resolve_font_family_name` did, twice per layer) is
/// both slow and a path dependency the identity work removed. The panel therefore hands
/// this index to the export job.
///
/// Keys are identities normalized the same way `fonts::normalize_font_identity` does
/// (`trim` + ASCII lowercase), which is the one comparison rule for identities app-wide.
/// Faces are indexed by POSITION in the font's face list — the value
/// `text_params.selected_face_index` stores.
#[derive(Debug, Clone, Default)]
pub(in crate::tabs::typing) struct FontPostScriptNames {
    by_identity: HashMap<String, Vec<String>>,
    /// Normalized PostScript name -> the normalized IDENTITIES that claim it.
    ///
    /// More than one owner means the name PSD writes cannot address a single font (see
    /// [`FontPostScriptNames::name_is_ambiguous`]).
    identities_by_name: HashMap<String, BTreeSet<String>>,
}

/// One exported text layer whose font could not be named unambiguously in the PSD.
///
/// A `.psd` records a text layer's font by NAME. Our identity is content-discriminated:
/// two files declaring one PostScript name with different bytes stay two fonts,
/// `Shared-Regular%1111…` and `Shared-Regular%9999…`, and the raster we bake is drawn with
/// the bytes of the one the layer selected. Photoshop, given the bare `Shared-Regular`,
/// re-binds the editable text to whichever file it resolves that name to — possibly the
/// other one. The format cannot express the distinction, so the export WRITES THE NAME
/// ANYWAY (dropping it would lose the font outright) and reports this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tabs::typing) struct AmbiguousExportFont {
    /// The identity as stored in the layer's `text_params` (usually `%hash`-suffixed).
    pub(in crate::tabs::typing) identity: String,
    /// The PostScript name actually written into the PSD text layer.
    pub(in crate::tabs::typing) post_script_name: String,
    /// How many distinct font identities claim that PostScript name in this export.
    pub(in crate::tabs::typing) claimant_count: usize,
}

impl FontPostScriptNames {
    /// Records one font's per-face PostScript names under its identity.
    ///
    /// `representative` is the entry's own PostScript name, used when the font exposes no
    /// faces at all (the placeholder entry of an unparsable file). Both may legitimately
    /// be EMPTY: a face whose declared PostScript name fails validation is treated as
    /// having none, and such a font still has a (family- or label-derived) identity. An
    /// empty identity is ignored; a first insertion wins, mirroring `TabFontProvider`'s
    /// FIRST-wins rule for a residual identity collision.
    pub(in crate::tabs::typing) fn insert_font(
        &mut self,
        identity: &str,
        face_names: Vec<String>,
        representative: &str,
    ) {
        let key = normalize_identity(identity);
        if key.is_empty() {
            return;
        }
        let names = if face_names.iter().any(|name| !name.trim().is_empty()) {
            face_names
        } else {
            vec![representative.to_string()]
        };
        // Record WHO claims each PostScript name before the first-wins insert, so the
        // ambiguity survives even if two identities somehow normalize to one key.
        for name in &names {
            let name_key = normalize_identity(name);
            if name_key.is_empty() {
                continue;
            }
            self.identities_by_name
                .entry(name_key)
                .or_default()
                .insert(key.clone());
        }
        self.by_identity.entry(key).or_insert(names);
    }

    /// `true` when `post_script_name` is claimed by MORE THAN ONE font identity in this
    /// snapshot — i.e. writing it into a PSD cannot say which font was meant.
    ///
    /// This is exactly the "contested PostScript name" case of the identity contract: the
    /// claimants got distinct `%hash` identities in the app, but the PSD format has only
    /// the shared declared name to record.
    #[must_use]
    pub(in crate::tabs::typing) fn name_is_ambiguous(&self, post_script_name: &str) -> bool {
        self.claimant_count(post_script_name) > 1
    }

    /// How many distinct font identities claim `post_script_name` here (0 when unknown).
    #[must_use]
    fn claimant_count(&self, post_script_name: &str) -> usize {
        self.identities_by_name
            .get(&normalize_identity(post_script_name))
            .map_or(0, BTreeSet::len)
    }

    /// PostScript name of `identity`'s face at position `face_pos`.
    ///
    /// Falls back to the font's first face carrying a NON-EMPTY name, because a face may
    /// have no usable PostScript name at all (an invalid one is treated as absent at load
    /// time). `None` when the identity is unknown here (the font is not installed) or no
    /// face of it carries a name — the caller then falls back to the identity itself.
    #[must_use]
    pub(in crate::tabs::typing) fn face_name(&self, identity: &str, face_pos: usize) -> Option<&str> {
        let faces = self.by_identity.get(&normalize_identity(identity))?;
        let non_empty = |name: &&String| !name.trim().is_empty();
        faces
            .get(face_pos)
            .filter(non_empty)
            .or_else(|| faces.iter().find(non_empty))
            .map(|name| name.trim())
    }
}

/// Identity comparison rule, mirroring `panel::fonts::normalize_font_identity` (which is
/// private to the panel module): `trim` + ASCII lowercase.
fn normalize_identity(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// One page's `.psd` bytes plus the user-facing warnings its assembly produced.
pub(super) struct TypingPagePsdBytes {
    /// The serialized `.psd` document.
    pub(super) bytes: Vec<u8>,
    /// Localized, already deduplicated warning lines for the export status line. Empty in
    /// the normal case.
    pub(super) warnings: Vec<String>,
}

/// Публичная точка входа: собирает `Psd` для одной страницы и возвращает байты файла.
///
/// Also reports every font the PSD format could not name unambiguously
/// ([`AmbiguousExportFont`]): each one is logged with its identity, the written name and
/// the number of claimants, and returned as a localized line for the export status. The
/// document itself always carries the name — a warning is never a reason to drop data.
pub(super) fn export_typing_single_page_psd(
    job: &TypingExportPageJob,
) -> Result<TypingPagePsdBytes, String> {
    // Источник страницы (RGBA8) на полном разрешении.
    let source = image::open(&job.page_path)
        .map_err(|err| {
            tf!("typing.errors.open_page_error", job = job.page_path.display(), err = err)
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

    let built = build_typing_page_psd(job, page_w, page_h, source_rgba, clean_rgba, composite);
    let warnings = report_ambiguous_export_fonts(job, &built.ambiguous_fonts);

    let options = WriteOptions {
        // Сохраняем наши превью-пиксели текстовых слоёв, чтобы Photoshop не
        // перерисовывал текст сразу при открытии.
        invalidate_text_layers: Some(false),
        ..Default::default()
    };
    Ok(TypingPagePsdBytes {
        bytes: write_psd(&built.psd, &options),
        warnings,
    })
}

/// Logs every ambiguous export font with full context and turns it into ONE localized
/// user-facing line per PostScript NAME (not per layer — a page can hold dozens of layers
/// in the same font, and repeating the line would drown the status area).
///
/// Runs on an export worker thread; both `runtime_log` and the `tf!` catalog are
/// process-global and safe there.
fn report_ambiguous_export_fonts(
    job: &TypingExportPageJob,
    ambiguous: &[AmbiguousExportFont],
) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut warnings: Vec<String> = Vec::new();
    for font in ambiguous {
        if !seen.insert(normalize_identity(&font.post_script_name)) {
            continue;
        }
        crate::runtime_log::log_warn(format!(
            "typing PSD export: page {} ({}): the text layer's font identity '{}' is written \
             into the .psd as the PostScript name '{}', which {} installed fonts declare with \
             DIFFERENT content. The baked raster uses the selected font's bytes, but Photoshop \
             matches an editable text layer by NAME and may bind it to another file. The .psd \
             format cannot carry our content-discriminated identity, so the name is written as \
             is and nothing is dropped.",
            job.page_idx,
            job.output_path.display(),
            font.identity,
            font.post_script_name,
            font.claimant_count,
        ));
        warnings.push(tf!(
            "typing.export.psd_ambiguous_font_warning",
            font = font.post_script_name,
            count = font.claimant_count
        ));
    }
    warnings
}

/// One page's assembled `Psd` plus what could not be expressed in the format.
///
/// The warnings travel WITH the document rather than being logged and forgotten: the
/// export UI shows them, and a silent approximation of the user's fonts is exactly the
/// kind of thing they must be able to see.
pub(super) struct TypingPagePsd {
    /// The document, always complete — an ambiguity never removes data from it.
    pub(super) psd: Psd,
    /// One entry per text layer whose font name could not address a single font.
    pub(super) ambiguous_fonts: Vec<AmbiguousExportFont>,
}

/// Чистая сборка `Psd` из подготовленных входов — без обращения к диску, чтобы
/// функцию можно было покрыть unit-тестом.
///
/// Returns the document together with every [`AmbiguousExportFont`] it had to write (see
/// that type: a PSD names a font, our identity discriminates by content).
pub(super) fn build_typing_page_psd(
    job: &TypingExportPageJob,
    page_w: usize,
    page_h: usize,
    source_rgba: Vec<u8>,
    clean_rgba: Option<Vec<u8>>,
    composite: Vec<u8>,
) -> TypingPagePsd {
    let mut layers: Vec<Layer> = Vec::new();
    let mut ambiguous_fonts: Vec<AmbiguousExportFont> = Vec::new();

    // 1. Слой-источник (самый нижний).
    // NOTE (i18n §A5): PSD layer names are DATA written into the exported .psd, not UI
    // labels — keep them as stable Russian literals so the export format is deterministic
    // and independent of the interface language.
    layers.push(full_page_layer("Источник", page_w, page_h, source_rgba, None));

    // 2. Слой-клин (если присутствует).
    if let Some(clean) = clean_rgba {
        // i18n §A5: PSD layer name is exported data, not a UI label — keep it literal.
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
            // i18n §A5: exported PSD group name is data, not a UI label — keep it literal.
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
        let mut ambiguous: Option<AmbiguousExportFont> = None;
        let text_data =
            build_layer_text_data(overlay, &job.font_post_script_names, &mut ambiguous);
        if let Some(ambiguous) = ambiguous {
            ambiguous_fonts.push(ambiguous);
        }

        if overlay.deform_mesh.is_none() {
            // CASE A: один видимый текстовый слой с запечённым аффинным превью.
            let baked = bake_affine_overlay(overlay, &clipped_rgba, page_w, page_h, &deform_mesh);
            let layer = make_baked_layer(
                // i18n §A5: exported PSD layer name is data, not a UI label — keep literal.
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
                // i18n §A5: exported PSD layer name is data, not a UI label — keep literal.
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
                // i18n §A5: exported PSD layer name is data, not a UI label — keep literal.
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

    TypingPagePsd {
        psd: Psd {
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
        },
        ambiguous_fonts,
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
///
/// `font_names` maps the overlay's font IDENTITY to a real PostScript name; it is
/// snapshotted from the panel's font list into the export job (see
/// [`FontPostScriptNames`]). Stored parameters are read through the schema module, so a
/// schema-2 payload's omitted defaults are materialized before anything is read.
fn build_layer_text_data(
    overlay: &TypingExportOverlaySnapshot,
    font_names: &FontPostScriptNames,
    ambiguous: &mut Option<AmbiguousExportFont>,
) -> LayerTextData {
    let stored = overlay
        .render_data_json
        .as_ref()
        .and_then(|v| v.get("text_params"))
        .and_then(Value::as_object);
    let filled = stored.map(text_params_schema::read_text_params);
    let params = filled.as_deref();

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
    // PostScript-имени (OpenType name table id 6, напр. `ArialMT`).
    //
    // Идентичность шрифта в проекте — это и есть его PostScript-имя, поэтому файл
    // шрифта здесь больше НЕ открывается (прежние `resolve_font_postscript_name` /
    // `resolve_font_family_name` читали один и тот же файл дважды на каждый текстовый
    // слой).
    //
    // Сначала берём ХРАНИМОЕ имя по правилам схемы САМОГО документа (см. ниже), затем:
    //   1. PostScript-имя ВЫБРАННОГО начертания из снимка списка шрифтов панели
    //      (`FontPostScriptNames`) — только так не теряется начертание `.ttc`;
    //   2. само хранимое имя: для установленного шрифта это идентичность (равна
    //      PostScript-имени представительного face), а для схемы 1 — легаси-форма имени,
    //      которая всё же ближе к правде, чем последний резерв;
    //   3. `"MyriadPro-Regular"` — последний резерв, когда имени нет вовсе.
    let face_index = params
        .and_then(|p| p.get("selected_face_index"))
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    // The stored name is picked by the DOCUMENT'S OWN schema, exactly as the codec picks
    // it — the export must not disagree with what the app renders:
    //   * schema 2 names the font once, under `font`; no other key may override it (a
    //     stale `font_original_name` left over from an older tool would otherwise win and
    //     hand Photoshop the wrong font);
    //   * schema 1 uses the historical chain `font_original_name -> font_label ->
    //     font_family -> font -> file stem of font_path`, taken from the schema owner
    //     (`text_params_schema::legacy_font_name_candidates`, which the codec's conversion
    //     also uses) so there is one order, not two.
    let stored_font_name: Option<String> = params.and_then(|p| {
        if text_params_schema::text_params_schema_version(p)
            >= text_params_schema::TEXT_PARAMS_SCHEMA_VERSION
        {
            p.get("font")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(ToOwned::to_owned)
        } else {
            text_params_schema::legacy_font_name_candidates(p)
                .into_iter()
                .next()
        }
    });
    let font_name = stored_font_name
        .as_deref()
        .and_then(|identity| font_names.face_name(identity, face_index))
        .map(ToOwned::to_owned)
        .or_else(|| stored_font_name.clone())
        .unwrap_or_else(|| "MyriadPro-Regular".to_string());

    // FORMAT LIMITATION, reported rather than hidden. When two installed files declare the
    // same PostScript name with different bytes, the app keeps them apart by identity
    // (`{name}%{hash}`) and this layer's RASTER was baked with the selected file's bytes —
    // but a `.psd` addresses a font by NAME only, so Photoshop may re-bind the editable
    // text to the other file. The name is still written: it is the closest thing the format
    // can carry, and omitting it would lose the font entirely.
    if font_names.name_is_ambiguous(&font_name) {
        *ambiguous = Some(AmbiguousExportFont {
            identity: stored_font_name.unwrap_or_else(|| font_name.clone()),
            claimant_count: font_names.claimant_count(&font_name),
            post_script_name: font_name.clone(),
        });
    }

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
    use ag_psd::psd::ReadOptions;
    use ag_psd::read_psd;
    use serde_json::json;
    use std::path::PathBuf;

    fn solid_overlay_rgba(w: usize, h: usize, color: [u8; 4]) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for px in v.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        v
    }

    /// The index the panel hands the export job for a two-face font whose identity is
    /// its representative face's PostScript name.
    fn two_face_index() -> FontPostScriptNames {
        let mut index = FontPostScriptNames::default();
        index.insert_font(
            "Maybug-Regular",
            vec!["Maybug-Regular".to_string(), "Maybug-Bold".to_string()],
            "Maybug-Regular",
        );
        index
    }

    #[test]
    fn build_psd_layers_and_roundtrip() {
        let page_w = 32usize;
        let page_h = 24usize;

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
                    "schema": 2,
                    "text": "Привет",
                    "text_color": [10, 20, 30, 255],
                    "font_size_px": 18.0,
                    "align": "center",
                    "font": "Maybug-Regular",
                    // Второе начертание файла: имя для Photoshop должно быть ЕГО
                    // PostScript-именем, а не именем представительного face.
                    "selected_face_index": 1
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
                    "schema": 2,
                    "text": "Мир",
                    "text_color": [200, 100, 50, 255],
                    "font_size_px": 12.0,
                    "align": "left",
                    "font": "ComicSansMS"
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
            font_post_script_names: two_face_index(),
        };

        let source_rgba = solid_overlay_rgba(page_w, page_h, [255, 255, 255, 255]);
        let clean_rgba = Some(solid_overlay_rgba(page_w, page_h, [0, 0, 0, 128]));
        let composite = solid_overlay_rgba(page_w, page_h, [128, 128, 128, 255]);

        let built = build_typing_page_psd(&job, page_w, page_h, source_rgba, clean_rgba, composite);
        assert!(
            built.ambiguous_fonts.is_empty(),
            "a font no other file contests is written without a warning"
        );
        let psd = built.psd;

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
        assert_eq!(group.additional_info.name.as_deref(), Some("Слой текста 0"));
        let text_layers = group.children.as_ref().expect("group children");
        // (A: 1) + (B: 2) = 3 слоя внутри группы.
        assert_eq!(text_layers.len(), 3);

        // Слой A — видимый текстовый (выше A на картинке → ниже в стопке группы).
        let a = &text_layers[0];
        assert_eq!(a.hidden, Some(false));
        let a_text = a.additional_info.text.as_ref().expect("A text");
        assert_eq!(a_text.text, "Привет");
        // Имя шрифта — PostScript-имя ВЫБРАННОГО начертания из снимка списка шрифтов.
        let a_font_name = a_text
            .style
            .as_ref()
            .and_then(|s| s.font.as_ref())
            .map(|f| f.name.as_str())
            .expect("A font");
        assert_eq!(a_font_name, "Maybug-Bold");

        // Слой B: скрытый текстовый + видимый растр.
        let b_text = &text_layers[1];
        assert_eq!(b_text.hidden, Some(true));
        assert!(b_text.additional_info.text.is_some());
        assert_eq!(b_text.additional_info.text.as_ref().unwrap().text, "Мир");
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

    /// Builds the text data of a one-off overlay carrying `text_params`, discarding any
    /// ambiguity report (the tests that care use [`text_data_and_ambiguity_for`]).
    fn text_data_for(text_params: Value, index: &FontPostScriptNames) -> LayerTextData {
        text_data_and_ambiguity_for(text_params, index).0
    }

    /// Builds the text data of a one-off overlay carrying `text_params`, together with the
    /// PSD-format ambiguity the font name produced (if any).
    fn text_data_and_ambiguity_for(
        text_params: Value,
        index: &FontPostScriptNames,
    ) -> (LayerTextData, Option<AmbiguousExportFont>) {
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
            render_data_json: Some(json!({ "text_params": text_params })),
            uid: "ov".into(),
            band_z: 0,
        };
        let mut ambiguous = None;
        let data = build_layer_text_data(&overlay, index, &mut ambiguous);
        (data, ambiguous)
    }

    fn font_name_of(text_data: LayerTextData) -> String {
        text_data
            .style
            .and_then(|style| style.font)
            .map(|font| font.name)
            .expect("font name")
    }

    /// The index the panel hands the export job when TWO installed files declare ONE
    /// PostScript name with different bytes: each got a `%hash`-suffixed identity, and
    /// both identities map back to the same declared name.
    fn contested_name_index() -> FontPostScriptNames {
        let mut index = FontPostScriptNames::default();
        index.insert_font(
            "Shared-Regular%1111111111111111",
            vec!["Shared-Regular".to_string()],
            "Shared-Regular",
        );
        index.insert_font(
            "Shared-Regular%9999999999999999",
            vec!["Shared-Regular".to_string()],
            "Shared-Regular",
        );
        index
    }

    /// FORMAT LIMITATION, reported instead of hidden. A `.psd` names a text layer's font;
    /// our identity discriminates by CONTENT. When two installed files declare one
    /// PostScript name with different bytes, the two identities collapse onto that single
    /// name in the export, and Photoshop may bind the editable layer to the file the
    /// raster was NOT drawn with.
    ///
    /// The contract this pins: the name is still WRITTEN (a warning must never cost the
    /// user data), and the ambiguity is REPORTED so it can reach the log and the export
    /// status line.
    #[test]
    fn a_contested_font_identity_is_written_and_reported() {
        let index = contested_name_index();
        let (text_data, ambiguous) = text_data_and_ambiguity_for(
            json!({
                "schema": text_params_schema::TEXT_PARAMS_SCHEMA_VERSION,
                "font": "Shared-Regular%9999999999999999",
                "text": "Привет",
            }),
            &index,
        );

        // The data is intact: the closest name the format can carry is written.
        assert_eq!(
            font_name_of(text_data),
            "Shared-Regular",
            "the PostScript name must be written even though it cannot say WHICH file"
        );

        let ambiguous = ambiguous.expect("a contested identity must be reported");
        assert_eq!(ambiguous.identity, "Shared-Regular%9999999999999999");
        assert_eq!(ambiguous.post_script_name, "Shared-Regular");
        assert_eq!(ambiguous.claimant_count, 2);
    }

    /// The reverse case, so the warning cannot become noise: a font no other file contests
    /// is written with no report at all.
    #[test]
    fn an_uncontested_font_is_written_without_a_warning() {
        let (text_data, ambiguous) = text_data_and_ambiguity_for(
            json!({
                "schema": text_params_schema::TEXT_PARAMS_SCHEMA_VERSION,
                "font": "Maybug-Regular",
                "text": "Привет",
            }),
            &two_face_index(),
        );
        assert_eq!(font_name_of(text_data), "Maybug-Regular");
        assert_eq!(ambiguous, None);
    }

    /// The whole-page assembly reports the ambiguity ONCE per layer and still produces a
    /// complete document — the warning travels beside the `Psd`, never instead of it.
    #[test]
    fn the_page_build_reports_every_contested_layer_and_still_writes_the_names() {
        let page_w = 8usize;
        let page_h = 8usize;
        let overlay = |uid: &str, identity: &str| TypingExportOverlaySnapshot {
            page_idx: 0,
            center_page_px: [4.0, 4.0],
            mask_clip_enabled: false,
            layer_idx: 0,
            user_scale: 1.0,
            angle_deg: 0.0,
            deform_mesh: None,
            size_px: [4, 4],
            source_rgba: solid_overlay_rgba(4, 4, [0, 0, 0, 255]),
            render_data_json: Some(json!({
                "text_params": {
                    "schema": text_params_schema::TEXT_PARAMS_SCHEMA_VERSION,
                    "font": identity,
                    "text": "Т",
                }
            })),
            uid: uid.into(),
            band_z: 0,
        };
        let job = TypingExportPageJob {
            page_idx: 0,
            page_path: PathBuf::from("unused.png"),
            output_path: PathBuf::from("unused.psd"),
            clean_overlay_path: None,
            clean_overlay_rgba: None,
            overlays: vec![
                overlay("a", "Shared-Regular%1111111111111111"),
                overlay("b", "Shared-Regular%9999999999999999"),
            ],
            rasters: Vec::new(),
            mask: None,
            export_format: TypingExportFormat::Psd,
            layers_primary_dir: None,
            layers_fallback_dir: None,
            font_post_script_names: contested_name_index(),
        };
        let built = build_typing_page_psd(
            &job,
            page_w,
            page_h,
            solid_overlay_rgba(page_w, page_h, [255, 255, 255, 255]),
            None,
            solid_overlay_rgba(page_w, page_h, [128, 128, 128, 255]),
        );

        assert_eq!(
            built.ambiguous_fonts.len(),
            2,
            "both contested layers must be reported"
        );
        assert!(
            built
                .ambiguous_fonts
                .iter()
                .all(|font| font.post_script_name == "Shared-Regular"),
            "both collapse onto the one name the format can carry"
        );

        // The document is complete: both text layers carry the name.
        let children = built.psd.children.as_ref().expect("children");
        let group = children.last().expect("the text group");
        let text_layers = group.children.as_ref().expect("group children");
        assert_eq!(text_layers.len(), 2);
        for layer in text_layers {
            let name = layer
                .additional_info
                .text
                .as_ref()
                .and_then(|text| text.style.as_ref())
                .and_then(|style| style.font.as_ref())
                .map(|font| font.name.as_str())
                .expect("every text layer keeps a font name");
            assert_eq!(name, "Shared-Regular");
        }
    }

    /// The index resolves an identity case-insensitively and falls back to the first
    /// face when the stored face position is out of range.
    #[test]
    fn index_resolves_identity_case_insensitively_and_clamps_the_face() {
        let index = two_face_index();
        assert_eq!(index.face_name("maybug-regular", 0), Some("Maybug-Regular"));
        assert_eq!(index.face_name("  Maybug-Regular ", 1), Some("Maybug-Bold"));
        // Out-of-range face position -> the first face, never a panic.
        assert_eq!(index.face_name("Maybug-Regular", 9), Some("Maybug-Regular"));
        // An unknown identity is not invented.
        assert_eq!(index.face_name("Nope", 0), None);
    }

    /// A schema-1 overlay is exported through its LEGACY font keys: the name they carry
    /// is resolved through the identity index, so the export uses the font's real
    /// PostScript name and never re-opens the font file (it has no path to open).
    ///
    /// Replaces `font_name_falls_back_to_label_postscript_segment`, whose `rsplit('|')`
    /// step decorated-label unpacking was removed: `font_label` has never held a
    /// decorated label in recorded history.
    #[test]
    fn schema_one_overlay_resolves_its_legacy_name_through_the_index() {
        let name = font_name_of(text_data_for(
            json!({
                "text": "x",
                "font_label": "Maybug-Regular",
                "font_path": "/definitely/not/a/real/font.ttf",
                "selected_face_index": 1
            }),
            &two_face_index(),
        ));
        assert_eq!(name, "Maybug-Bold");
    }

    /// A font that is NOT in the list (uninstalled) exports under the name the overlay
    /// stores — the best remaining clue — instead of a made-up one. Replaces
    /// `font_name_falls_back_to_path_stem`: the file path is no longer a name source.
    #[test]
    fn an_unknown_font_exports_under_its_stored_name() {
        let name = font_name_of(text_data_for(
            json!({ "schema": 2, "text": "x", "font": "SomeFont-Bold" }),
            &FontPostScriptNames::default(),
        ));
        assert_eq!(name, "SomeFont-Bold");
    }

    /// A font whose declared PostScript name failed validation at load time carries an
    /// EMPTY one, while its identity is derived from the family/label instead. The export
    /// must then fall back to the identity, never to an empty font name.
    #[test]
    fn a_font_without_a_valid_post_script_name_exports_under_its_identity() {
        let mut index = FontPostScriptNames::default();
        index.insert_font("Broken Family", vec![String::new()], "");
        assert_eq!(index.face_name("Broken Family", 0), None);
        let name = font_name_of(text_data_for(
            json!({ "schema": 2, "text": "x", "font": "Broken Family" }),
            &index,
        ));
        assert_eq!(name, "Broken Family");
    }

    /// A `.ttc` whose SELECTED face has no valid PostScript name still exports under a
    /// real one — the first face that has any — instead of falling through to the
    /// identity.
    #[test]
    fn an_unnamed_face_falls_back_to_the_first_named_face() {
        let mut index = FontPostScriptNames::default();
        index.insert_font(
            "Half-Named",
            vec!["Half-Named".to_string(), String::new()],
            "Half-Named",
        );
        assert_eq!(index.face_name("Half-Named", 1), Some("Half-Named"));
    }

    /// An overlay naming no font at all falls back to the historical default.
    #[test]
    fn a_nameless_overlay_falls_back_to_the_default_font() {
        let name = font_name_of(text_data_for(
            json!({ "schema": 2, "text": "x" }),
            &FontPostScriptNames::default(),
        ));
        assert_eq!(name, "MyriadPro-Regular");
    }

    /// REVIEW FINDING 4. The export must pick the stored font name by the DOCUMENT'S OWN
    /// schema, exactly like the codec:
    ///
    /// - a schema-2 payload names the font once, under `font`; a stale legacy key left in
    ///   the object by an older tool must NOT win, or Photoshop is handed a font the app
    ///   itself does not render with;
    /// - a schema-1 payload uses the historical chain, whose FIRST link is
    ///   `font_original_name` — the export used to read `font` first there too, which for
    ///   a v1 document with conflicting keys picked the weakest name in the chain.
    #[test]
    fn conflicting_legacy_font_keys_follow_the_documents_own_schema() {
        let mut index = FontPostScriptNames::default();
        index.insert_font("Current-Identity", vec!["Current-PS".to_string()], "Current-PS");
        index.insert_font("Stale-Family", vec!["Stale-PS".to_string()], "Stale-PS");

        // Schema 2: `font` is the ONLY font key; the leftover legacy keys are ignored.
        let name = font_name_of(text_data_for(
            json!({
                "schema": 2,
                "text": "x",
                "font": "Current-Identity",
                "font_original_name": "Stale-Family",
                "font_label": "Stale-Family",
                "font_family": "Stale-Family",
            }),
            &index,
        ));
        assert_eq!(
            name, "Current-PS",
            "schema 2 names the font once, under `font`"
        );

        // Schema 1: the historical chain, `font_original_name` first — the same order the
        // codec resolves and converts with.
        let name = font_name_of(text_data_for(
            json!({
                "text": "x",
                "font_original_name": "Current-Identity",
                "font_label": "Stale-Family",
                "font_family": "Stale-Family",
                "font": "Stale-Family",
                "font_path": "/fonts/Stale-Family.ttf",
            }),
            &index,
        ));
        assert_eq!(
            name, "Current-PS",
            "schema 1 follows the codec's chain: font_original_name -> font_label -> \
             font_family -> font -> path stem"
        );
    }

    /// A schema-2 payload omits every default, so the export must read alignment and
    /// size through the schema module rather than falling back to its own historical
    /// defaults (which said `left` / 24pt).
    #[test]
    fn schema_two_defaults_are_materialized_for_the_exported_layer() {
        let text_data = text_data_for(
            json!({ "schema": 2, "text": "x", "font": "Maybug-Regular" }),
            &two_face_index(),
        );
        assert_eq!(
            text_data
                .paragraph_style
                .and_then(|style| style.justification),
            Some(Justification::Center),
            "the frozen schema-2 default alignment is `center`"
        );
    }
}
