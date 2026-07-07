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
        let mut state = TypingCreatePanelState::new(false, false);
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
