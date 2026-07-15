/*
File: panel/tests.rs

Purpose:
The `#[cfg(test)]` unit-test module for `panel.rs`, extracted verbatim from the
inline `mod tests` block. `super` still resolves to the `panel` root, so no
paths change.
*/
    use super::*;

    #[test]
    fn color_field_serializes_straight_alpha_rgba() {
        let color = ColorField::new(Color32::from_rgba_unmultiplied(255, 255, 255, 128));

        assert_eq!(color.rgba(), [255, 255, 255, 128]);
    }

    #[test]
    fn machine_tag_round_trips_through_build_and_parse() {
        let style = TypingInlineTagStyle {
            faux_bold: None,
            faux_italic_slant: None,
            bold: true,
            italic: false,
            no_break: true,
            align: Some(HorizontalAlign::RIGHT),
            font_label: Some("My Font".to_string()),
            font_size_px: Some(36.0),
            text_color: Some(Color32::from_rgb(0x11, 0x22, 0x33)),
            line_spacing: Some(PxOrPercent::percent(50.0)),
            kerning: Some(PxOrPercent::px(10.0)),
            glyph_stretching: Some([PxOrPercent::percent(120.0), PxOrPercent::px(80.0)]),
            glyph_offset: Some(TypingInlineOffsetStyle {
                global_x: PxOrPercent::px(3.0),
                global_y: PxOrPercent::percent(0.0),
                line: PxOrPercent::px(12.0),
                shift_following: true,
                group_rotation_deg: 30.0,
                glyph_rotation_deg: 0.0,
            }),
        };

        let tag = build_inline_machine_tag(&style);
        assert!(tag.starts_with("<m ") && tag.ends_with('>'));
        let inner = &tag[1..tag.len() - 1];
        let parsed = parse_machine_tag_style(inner).expect("machine tag should parse");

        assert_eq!(parsed, style);
    }

    #[test]
    fn empty_machine_tag_is_not_emitted() {
        assert!(build_inline_machine_tag(&TypingInlineTagStyle::default()).is_empty());
    }

    #[test]
    fn faux_inline_tags_round_trip_through_panel_grammar() {
        let style = TypingInlineTagStyle {
            bold: true,
            italic: true,
            faux_bold: Some(FauxBoldParams {
                thicken_percent: 5.0,
                expand_percent: 2.0,
                sharp_corners: false,
                outward_only: false,
            }),
            faux_italic_slant: Some(-10.0),
            ..TypingInlineTagStyle::default()
        };

        let machine = build_inline_machine_tag(&style);
        let parsed_machine = parse_machine_tag_style(&machine[1..machine.len() - 1])
            .unwrap_or_default();
        assert_eq!(parsed_machine.faux_bold, style.faux_bold);
        assert_eq!(parsed_machine.faux_italic_slant, style.faux_italic_slant);
        assert!(matches!(parse_opening_inline_tag("b=5,round,both,2"), Some(TypingInlineTagKind::FauxBold(_))));
        assert!(matches!(parse_opening_inline_tag("i=-10"), Some(TypingInlineTagKind::FauxItalic(-10.0))));
    }

    #[test]
    fn inline_tag_editor_colors_dim_tags_and_whiten_content() {
        let colors = build_inline_tag_editor_text_colors("<b>Пример</b>");

        assert_eq!(
            colors,
            vec![
                TextEditPlusTextColor::new(3..9, INLINE_TAG_CONTENT_TEXT_COLOR),
                TextEditPlusTextColor::new(0..3, INLINE_TAG_DIM_TEXT_COLOR),
                TextEditPlusTextColor::new(9..13, INLINE_TAG_DIM_TEXT_COLOR),
            ]
        );
    }

    #[test]
    fn inline_tag_editor_colors_keep_nested_tags_dimmed() {
        let colors = build_inline_tag_editor_text_colors("<b>А<i>Б</i></b>");
        let outer_content = 3..12;
        let inner_opening_tag = 4..7;

        assert!(
            colors
                .iter()
                .position(|style| style.char_range == outer_content
                    && style.color == INLINE_TAG_CONTENT_TEXT_COLOR)
                .is_some_and(|content_idx| {
                    colors.iter().skip(content_idx + 1).any(|style| {
                        style.char_range == inner_opening_tag
                            && style.color == INLINE_TAG_DIM_TEXT_COLOR
                    })
                })
        );
    }

    fn raw_font(path: &str, group: Option<&str>, hash: u64) -> RawFontFile {
        RawFontFile {
            path: PathBuf::from(path),
            stem: PathBuf::from(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string(),
            group: group.map(ToOwned::to_owned),
            content_hash: hash,
            faces: default_single_face(),
            coverage: FontLanguageCoverage::default(),
            original_name: String::new(),
        }
    }

    #[test]
    fn identical_fonts_merge_and_union_groups() {
        // Одинаковое имя + одинаковый хэш в корне и в группе → один шрифт.
        let entries = merge_duplicate_fonts(vec![
            raw_font("/fonts/Разговор.ttf", None, 42),
            raw_font("/fonts/groups/A/Разговор.ttf", Some("A"), 42),
        ]);
        assert_eq!(entries.len(), 1);
        let font = &entries[0];
        assert_eq!(font.label, "Разговор");
        assert!(font.groups.contains(&None));
        assert!(font.groups.contains(&Some("A".to_string())));
        // Альтернативный путь сохранён для сопоставления.
        assert!(font_matches_path(font, "/fonts/groups/A/Разговор.ttf"));
        assert!(font_in_group(font, "A"));
    }

    #[test]
    fn same_name_different_content_stays_separate_and_disambiguated() {
        let mut entries = merge_duplicate_fonts(vec![
            raw_font("/fonts/groups/A/Разговор.ttf", Some("A"), 1),
            raw_font("/fonts/groups/B/Разговор.ttf", Some("B"), 2),
        ]);
        assert_eq!(entries.len(), 2);
        assign_font_disambiguators(&mut entries);
        let suffixes: Vec<Option<String>> =
            entries.iter().map(|font| font.disambig.clone()).collect();
        assert!(suffixes.contains(&Some("A".to_string())));
        assert!(suffixes.contains(&Some("B".to_string())));
    }

    #[test]
    fn unique_name_gets_no_disambiguator() {
        let mut entries = merge_duplicate_fonts(vec![raw_font(
            "/fonts/Уникальный.ttf",
            None,
            7,
        )]);
        assign_font_disambiguators(&mut entries);
        assert_eq!(entries[0].disambig, None);
    }

    #[test]
    fn selecting_missing_overlay_font_sets_warning_and_clears_on_found() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = merge_duplicate_fonts(vec![raw_font("/fonts/Доступный.ttf", None, 11)]);
        state.selected_font_idx = 0;

        // Шрифт оверлея отсутствует среди доступных → запоминаем его имя.
        state.select_font_by_path_or_label(Some("/fonts/Пропавший.ttf"), Some("Пропавший"));
        assert_eq!(state.missing_font.as_deref(), Some("Пропавший"));

        // Без метки берём имя файла из пути.
        state.select_font_by_path_or_label(Some("/fonts/ДругойПропавший.otf"), None);
        assert_eq!(state.missing_font.as_deref(), Some("ДругойПропавший.otf"));

        // Найденный шрифт снимает блокировку рендера.
        state.select_font_by_path_or_label(Some("/fonts/Доступный.ttf"), Some("Доступный"));
        assert!(state.missing_font.is_none());
        assert_eq!(state.selected_font_idx, 0);
    }

    /// Строит выбранный текстовый оверлей без `render_data`, чтобы
    /// `load_from_selected_overlay` не запускал тяжёлый разбор JSON в тесте.
    fn text_overlay_for_edit(idx: usize) -> TypingSelectedOverlayForEdit {
        TypingSelectedOverlayForEdit {
            overlay_idx: idx,
            overlay_kind: TypingOverlayKind::Text,
            render_data_json: None,
            width_px_hint: 100,
            user_scale: 1.0,
            rotation_deg: 0.0,
            target: TypingEditTarget::Overlay(idx),
        }
    }

    #[test]
    fn inline_text_selection_is_scoped_to_a_single_layer() {
        let mut state = TypingTopPanelState::default();

        // Выбираем слой 0 и запоминаем выделение в поле редактирования.
        state.sync_selected_overlay_for_edit(Some(text_overlay_for_edit(0)));
        state.edit_panel.text_selection_char_range = Some(2..5);

        // Повторный выбор того же слоя сохраняет выделение.
        state.sync_selected_overlay_for_edit(Some(text_overlay_for_edit(0)));
        assert_eq!(state.edit_panel.text_selection_char_range, Some(2..5));

        // Выбор другого слоя сбрасывает выделение прошлого слоя.
        state.sync_selected_overlay_for_edit(Some(text_overlay_for_edit(1)));
        assert_eq!(state.edit_panel.text_selection_char_range, None);
        assert_eq!(state.edit_panel.pending_text_selection_restore, None);
    }

    #[test]
    fn inline_text_selection_survives_deselect_and_reselect_of_same_layer() {
        let mut state = TypingTopPanelState::default();

        state.sync_selected_overlay_for_edit(Some(text_overlay_for_edit(0)));
        state.edit_panel.text_selection_char_range = Some(1..4);

        // Снятие выбора (потеря фокуса) не должно терять выделение слоя.
        state.sync_selected_overlay_for_edit(None);
        assert_eq!(state.edit_panel.text_selection_char_range, Some(1..4));

        // Повторный выбор того же слоя сохраняет выделение.
        state.sync_selected_overlay_for_edit(Some(text_overlay_for_edit(0)));
        assert_eq!(state.edit_panel.text_selection_char_range, Some(1..4));

        // Но переход на другой слой через снятие выбора всё равно сбрасывает.
        state.sync_selected_overlay_for_edit(None);
        state.sync_selected_overlay_for_edit(Some(text_overlay_for_edit(1)));
        assert_eq!(state.edit_panel.text_selection_char_range, None);
    }

    /// Unique temp path for an imported-fonts test so parallel tests never collide and the
    /// real user config / fonts folder are never touched.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("ms_test_imported_fonts_{tag}_{nanos}"))
    }

    #[test]
    fn load_imported_system_fonts_skips_missing_and_unparseable_files() {
        let dir = unique_temp_dir("skip");
        fs::create_dir_all(&dir).expect("create temp dir");
        // A file that exists but is not a valid font must be skipped, not turned into a fake
        // entry; a path that does not exist at all must also be skipped.
        let garbage = dir.join("not_a_font.ttf");
        fs::write(&garbage, b"this is not a font").expect("write garbage file");
        let missing = dir.join("does_not_exist.ttf");

        let entries = load_imported_system_fonts(&[garbage.clone(), missing]);
        assert_eq!(
            entries.len(),
            0,
            "missing and unparseable imported paths must be skipped"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Build a minimal selection context whose only meaningful field is `style`;
    /// the ranges are dummies (the tested functions read only `selection.style`).
    fn selection_with_style(style: TypingInlineTagStyle) -> TypingInlineSelectionContext {
        TypingInlineSelectionContext {
            char_range: 0..1,
            text_byte_range: 0..1,
            opening_wrapper_range: 0..0,
            closing_wrapper_range: 1..1,
            style,
        }
    }

    /// A state carrying one selectable font so `effective`/`normalize` filter the
    /// overlay-default font label, size, color, etc. down to nothing.
    fn state_with_font() -> TypingCreatePanelState {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = merge_duplicate_fonts(vec![raw_font("/fonts/Test.ttf", None, 1)]);
        state.selected_font_idx = 0;
        state
    }

    // Finding 10 (a): every faux field must be pinned into the built render_data.
    #[test]
    fn faux_params_pin_all_seven_text_params_keys() {
        let mut state = TypingCreatePanelState::new(false);
        state.force_bold = true;
        state.faux_bold = true;
        state.faux_bold_thicken_percent = 7.5;
        state.faux_bold_expand_percent = 4.0;
        state.faux_bold_sharp_corners = false;
        state.faux_bold_outward_only = false;
        state.force_italic = true;
        state.faux_italic = true;
        state.faux_italic_slant_deg = -30.0;

        let render_data = state.build_render_data_json_with_font(
            "Hi".to_string(),
            100,
            Some("/fonts/Test.ttf".to_string()),
            Some("Test".to_string()),
            None,
        );
        let tp = render_data
            .get("text_params")
            .and_then(Value::as_object)
            .expect("text_params object");
        assert_eq!(tp.get("faux_bold").and_then(Value::as_bool), Some(true));
        assert_eq!(
            tp.get("faux_bold_thicken_percent").and_then(value_as_f32),
            Some(7.5)
        );
        assert_eq!(
            tp.get("faux_bold_expand_percent").and_then(value_as_f32),
            Some(4.0)
        );
        assert_eq!(
            tp.get("faux_bold_sharp_corners").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            tp.get("faux_bold_outward_only").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(tp.get("faux_italic").and_then(Value::as_bool), Some(true));
        assert_eq!(
            tp.get("faux_italic_slant_deg").and_then(value_as_f32),
            Some(-30.0)
        );
    }

    // Finding 10 (b): the read path round-trips the seven fields and clamps them.
    #[test]
    fn faux_params_round_trip_through_apply_with_clamping() {
        let render_data = serde_json::json!({
            "text_params": {
                "text": "Hi",
                "force_bold": true,
                "faux_bold": true,
                "faux_bold_thicken_percent": 99.0,
                "faux_bold_expand_percent": 4.0,
                "faux_bold_sharp_corners": false,
                "faux_bold_outward_only": false,
                "force_italic": true,
                "faux_italic": true,
                "faux_italic_slant_deg": -90.0,
            },
            "effects": [],
        });
        let mut state = TypingCreatePanelState::new(false);
        state.apply_render_data_json_with_options(&render_data, false);
        assert!(state.faux_bold);
        assert_eq!(state.faux_bold_thicken_percent, 25.0); // 99 clamps to 25
        assert_eq!(state.faux_bold_expand_percent, 4.0);
        assert!(!state.faux_bold_sharp_corners);
        assert!(!state.faux_bold_outward_only);
        assert!(state.faux_italic);
        assert_eq!(state.faux_italic_slant_deg, -45.0); // -90 clamps to -45
    }

    // Finding 10 (c): the built TextRenderParams gate faux on the force_* flags.
    #[test]
    fn faux_render_params_gate_on_force_flags() {
        let mut state = state_with_font();
        state.faux_bold = true;
        state.faux_bold_thicken_percent = 7.5;
        state.faux_italic = true;
        state.faux_italic_slant_deg = -30.0;

        // force_* off -> None even though faux_* is on.
        state.force_bold = false;
        state.force_italic = false;
        let params = state.build_render_params().expect("render params");
        assert!(params.faux_bold.is_none());
        assert!(params.faux_italic_slant_deg.is_none());

        // force_* on + faux_* on -> Some with the pinned values.
        state.force_bold = true;
        state.force_italic = true;
        let params = state.build_render_params().expect("render params");
        assert_eq!(params.faux_bold.map(|f| f.thicken_percent), Some(7.5));
        assert_eq!(params.faux_italic_slant_deg, Some(-30.0));
    }

    // Finding 2: a bare `<b>` span under a faux overlay reports REAL bold (faux
    // None), and normalization re-emits the span verbatim (round-trips to `<m b>`).
    #[test]
    fn bare_bold_span_under_overlay_faux_reports_real_bold_and_round_trips() {
        let mut state = state_with_font();
        state.force_bold = true;
        state.faux_bold = true;
        state.faux_bold_thicken_percent = 6.0;

        let selection = selection_with_style(TypingInlineTagStyle {
            bold: true,
            faux_bold: None,
            ..TypingInlineTagStyle::default()
        });

        let effective = state.effective_inline_tag_style(&selection);
        assert!(effective.bold);
        assert_eq!(effective.faux_bold, None, "bare <b> stays real bold");

        let normalized = state.normalize_desired_inline_tag_style(effective);
        assert!(normalized.bold);
        assert_eq!(normalized.faux_bold, None);
        assert_eq!(build_inline_machine_tag(&normalized), "<m b>");
    }

    // Finding 1: a selection whose faux state differs from the overlay's under
    // force_bold=true still emits a parameterized tag (not silently dropped).
    #[test]
    fn selection_faux_differing_from_overlay_emits_parameterized_tag() {
        let mut state = state_with_font();
        // Overlay: forced REAL bold (faux off).
        state.force_bold = true;
        state.faux_bold = false;

        let selection = selection_with_style(TypingInlineTagStyle::default());
        let mut desired = state.effective_inline_tag_style(&selection);
        // Simulate the panel edit: enable faux bold on this selection (thicken 8).
        desired.faux_bold = Some(FauxBoldParams {
            thicken_percent: 8.0,
            ..FauxBoldParams::default()
        });

        let normalized = state.normalize_desired_inline_tag_style(desired);
        assert!(normalized.bold);
        assert_eq!(
            normalized.faux_bold.map(|f| f.thicken_percent),
            Some(8.0),
            "differing faux must be emitted under overlay force+real bold"
        );
        assert_eq!(
            build_inline_machine_tag(&normalized),
            "<m b=8.00,sharp,out,0.00>"
        );
    }

    // Finding 1/2: selecting a plain span with no edits under a faux overlay is a
    // no-op — the overlay already provides the faux bold, so no span tag is emitted.
    #[test]
    fn plain_span_under_overlay_faux_is_a_noop() {
        let mut state = state_with_font();
        state.force_bold = true;
        state.faux_bold = true;
        state.faux_bold_thicken_percent = 6.0;

        let selection = selection_with_style(TypingInlineTagStyle::default());
        let effective = state.effective_inline_tag_style(&selection);
        let normalized = state.normalize_desired_inline_tag_style(effective);
        assert!(!normalized.bold);
        assert_eq!(normalized.faux_bold, None);
        assert!(build_inline_machine_tag(&normalized).is_empty());
    }

    #[test]
    fn load_fonts_with_no_imported_paths_matches_dir_only_loading() {
        // On an empty fonts dir, `load_fonts` with no imported paths must equal the plain
        // folder load (both empty) — the imported-paths merge is purely additive.
        let dir = unique_temp_dir("empty");
        fs::create_dir_all(&dir).expect("create temp dir");
        let via_load_fonts = load_fonts(&dir, &[]);
        let via_dir = load_fonts_from_dir(&dir);
        assert!(via_load_fonts.is_empty());
        assert_eq!(via_load_fonts.len(), via_dir.len());
        let _ = fs::remove_dir_all(&dir);
    }
