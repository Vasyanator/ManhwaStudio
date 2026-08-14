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

    /// Extracts the single gradient2 card a fixture JSON array must parse to.
    ///
    /// Test-only helper: the fixture invariant is "this array holds exactly one
    /// gradient2 card", so any deviation panics — the panic IS the failure.
    fn single_gradient2_card(effects: &[Value]) -> Gradient2EffectCard {
        let cards = parse_effect_cards(effects, Color32::WHITE);
        assert_eq!(cards.len(), 1, "fixture must hold exactly one card");
        match cards.into_iter().next() {
            Some(EffectCard::Gradient2(card)) => card,
            _ => panic!("fixture must parse to a Gradient2 card"),
        }
    }

    #[test]
    fn gradient_area_and_tolerance_round_trip_through_card_json() {
        let mut card = match TypingCreatePanelState::default_effect_card(
            AvailableEffectKind::Gradient2,
            Color32::WHITE,
        ) {
            EffectCard::Gradient2(card) => card,
            _ => panic!("Gradient2 kind must build a Gradient2 card"),
        };
        card.fill_mode = Gradient2FillMode::SpecificColor;
        card.color_tolerance_percent = 17.5;
        card.area_mode = GradientAreaMode::AffectedArea;

        let value = effect_card_to_value(&EffectCard::Gradient2(card));
        let restored = single_gradient2_card(&[value]);

        assert!((restored.color_tolerance_percent - 17.5).abs() < 1e-6);
        assert!(restored.area_mode == GradientAreaMode::AffectedArea);
    }

    #[test]
    fn gradient_card_without_new_keys_keeps_legacy_defaults() {
        let restored = single_gradient2_card(&[json!({"effect": "gradient2"})]);

        assert!((restored.color_tolerance_percent - 0.0).abs() < 1e-6);
        assert!(restored.area_mode == GradientAreaMode::FullImage);
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

    /// The panel's mirror of the renderer's `<b=...>` payload grammar must decode a
    /// bare-magnitude payload EXACTLY as the renderer does.
    ///
    /// The renderer's own parser (`ms_text_render::inline_styles::parse_faux_bold_value`)
    /// lives in a private module and cannot be called from here, so the pin is on the
    /// shared contract both sides implement: every token the payload omits keeps its
    /// `FauxBoldParams::default()` value, and the leading number is SIGNED and clamped to
    /// the renderer's exported bounds. Restating any default here (as the mirror once
    /// did) is what made the two drift apart when the crate default changed.
    #[test]
    fn panel_faux_bold_mirror_matches_the_renderer_defaults() {
        assert_eq!(
            parse_faux_bold_value("8"),
            Some(FauxBoldParams { thicken_percent: 8.0, ..FauxBoldParams::default() }),
            "an omitted token must mean the renderer's default, not a restated literal"
        );
        assert_eq!(parse_faux_bold_value("default"), Some(FauxBoldParams::default()));
        assert_eq!(parse_faux_bold_value(""), Some(FauxBoldParams::default()));
        // Signed magnitude, clamped to the renderer's own bounds on both ends.
        assert_eq!(
            parse_faux_bold_value("-3").map(|faux| faux.thicken_percent),
            Some(-3.0)
        );
        assert_eq!(
            parse_faux_bold_value("-99").map(|faux| faux.thicken_percent),
            Some(FAUX_THICKEN_PERCENT_MIN)
        );
        assert_eq!(
            parse_faux_bold_value("99").map(|faux| faux.thicken_percent),
            Some(FAUX_THICKEN_PERCENT_MAX)
        );
        // Unreadable payloads stay unreadable (the tag then survives as literal text).
        assert_eq!(parse_faux_bold_value("8,zzz"), None);
        assert_eq!(parse_faux_bold_value("8,1,2"), None);
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
        raw_font_named(path, group, hash, "")
    }

    /// Like `raw_font` but with an explicit ORIGINAL family name and NO PostScript name
    /// (an unparsable file), so tests can exercise the label≠family case and the
    /// family-or-label identity FALLBACK.
    fn raw_font_named(path: &str, group: Option<&str>, hash: u64, original_name: &str) -> RawFontFile {
        raw_font_ps(path, group, hash, original_name, "")
    }

    /// Full raw-file fixture: family name AND the representative face's PostScript name,
    /// which is what the render identity is derived from.
    fn raw_font_ps(
        path: &str,
        group: Option<&str>,
        hash: u64,
        original_name: &str,
        post_script_name: &str,
    ) -> RawFontFile {
        let faces = if post_script_name.is_empty() {
            default_single_face()
        } else {
            vec![FontFaceEntry {
                label: format!("#0 {original_name} | Normal | w400 | {post_script_name}"),
                face_index: 0,
                post_script_name: post_script_name.to_string(),
            }]
        };
        RawFontFile {
            path: PathBuf::from(path),
            stem: PathBuf::from(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string(),
            group: group.map(ToOwned::to_owned),
            content_hash: hash,
            faces,
            coverage: FontLanguageCoverage::default(),
            original_name: original_name.to_string(),
        }
    }

    /// Same intent as the former `font_matches_label` union test — every historical form
    /// of a font's name must map back to that font, and an unrelated name must not — but
    /// asserted through the ORDERED resolver that replaced the union predicate
    /// (`find_font_idx_by_name_forms`), which is what the panel actually uses.
    #[test]
    fn name_forms_resolve_identity_family_label_and_stem() {
        // Stem/label "основной", real family "Anime Ace v05", PostScript "AnimeAcev05".
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![raw_font_ps(
            "/fonts/основной.ttf",
            None,
            1,
            "Anime Ace v05",
            "AnimeAcev05",
        )]);
        // The IDENTITY (PostScript name) resolves — the form written from now on.
        assert_eq!(state.find_font_idx_by_name_forms("AnimeAcev05"), Some(0));
        // Case folding is part of the identity contract.
        assert_eq!(state.find_font_idx_by_name_forms("animeacev05"), Some(0));
        // A persisted family name must map back to this font (no spurious missing_font).
        assert_eq!(state.find_font_idx_by_name_forms("anime ace v05"), Some(0));
        // Legacy label/stem forms still resolve.
        assert_eq!(state.find_font_idx_by_name_forms("основной"), Some(0));
        // The strict identity lookup accepts ONLY the identity, so a legacy form cannot
        // pose as a selection key.
        assert_eq!(state.find_font_idx_by_identity("AnimeAcev05"), Some(0));
        assert_eq!(state.find_font_idx_by_identity("основной"), None);
        // An unrelated name matches nothing.
        assert_eq!(state.find_font_idx_by_name_forms("helvetica"), None);
    }

    #[test]
    fn inline_font_tag_uses_post_script_name_and_legacy_forms_still_resolve() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![raw_font_ps(
            "/fonts/основной.ttf",
            None,
            1,
            "Anime Ace v05",
            "AnimeAcev05",
        )]);
        state.selected_font_idx = 0;

        // Identity used for tags/render is the POSTSCRIPT NAME — neither the family name
        // nor the stem.
        assert_eq!(
            state.font_identity_name_by_idx(0).as_deref(),
            Some("AnimeAcev05")
        );
        let tag = build_inline_opening_tags(&TypingInlineTagStyle {
            font_label: state.font_identity_name_by_idx(0),
            ..TypingInlineTagStyle::default()
        });
        assert!(
            tag.contains("<font=AnimeAcev05>"),
            "a newly emitted tag names the PostScript name, got: {tag}"
        );

        // The new identity AND both legacy forms (family name, file stem) resolve to the
        // same font index.
        for name in ["AnimeAcev05", "Anime Ace v05", "основной"] {
            assert_eq!(
                state.find_font_idx_by_name_forms(name),
                Some(0),
                "'{name}' must still resolve to the only loaded font"
            );
        }

        // A legacy `<font=основной>` span on the base font is redundant (resolves to the
        // same font as the base identity) and must be STRIPPED — no duplicate tag.
        let legacy_span = state.normalize_desired_inline_tag_style(TypingInlineTagStyle {
            font_label: Some("основной".to_string()),
            ..TypingInlineTagStyle::default()
        });
        assert!(
            legacy_span.font_label.is_none(),
            "a legacy stem tag naming the base font must be stripped"
        );

        // A genuinely different (unresolvable here) name is PRESERVED, never silently dropped.
        let other_span = state.normalize_desired_inline_tag_style(TypingInlineTagStyle {
            font_label: Some("Some Other Family".to_string()),
            ..TypingInlineTagStyle::default()
        });
        assert_eq!(other_span.font_label.as_deref(), Some("Some Other Family"));
    }

    /// Builds a panel font list from raw files and runs the collision-aware identity
    /// assignment, so identity/provider tests see the SAME identities the panel uses.
    fn fonts_with_identities(raws: Vec<RawFontFile>) -> Vec<FontEntry> {
        let mut fonts = merge_duplicate_fonts(raws);
        assign_font_identity_names(&mut fonts);
        fonts
    }

    // Identity = the representative face's PostScript name. A Regular+Bold pair shipped
    // as two files shares one FAMILY but not one PostScript name, so each file keeps its
    // own identity and neither can render as the other — the same guarantee the old
    // family-vs-stem rule was built to provide, now by construction.
    #[test]
    fn assign_identity_uses_the_post_script_name_of_each_file() {
        let fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/myfont-regular.ttf", None, 1, "MyFont", "MyFont-Regular"),
            raw_font_ps("/fonts/myfont-bold.ttf", None, 2, "MyFont", "MyFont-Bold"),
            raw_font_ps("/fonts/solo.ttf", None, 3, "Solo Family", "SoloFamily-Regular"),
        ]);
        let reg = fonts.iter().find(|f| f.label == "myfont-regular").unwrap();
        let bold = fonts.iter().find(|f| f.label == "myfont-bold").unwrap();
        let solo = fonts.iter().find(|f| f.label == "solo").unwrap();
        // Shared family "MyFont" -> distinct PostScript names -> distinct identities.
        assert_eq!(reg.render_identity_name(), "MyFont-Regular");
        assert_eq!(bold.render_identity_name(), "MyFont-Bold");
        // A font nobody else claims keeps its bare PostScript name, never a suffix.
        assert_eq!(solo.render_identity_name(), "SoloFamily-Regular");
    }

    // A file with NO readable PostScript name (unparsable, so no face at all) must still
    // get a meaningful identity: the documented family-or-label FALLBACK.
    #[test]
    fn assign_identity_without_a_post_script_name_falls_back_to_family_then_label() {
        let fonts = fonts_with_identities(vec![
            raw_font_named("/fonts/broken-with-family.ttf", None, 1, "Rescued Family"),
            raw_font_named("/fonts/broken-nameless.ttf", None, 2, ""),
        ]);
        let with_family = fonts
            .iter()
            .find(|f| f.label == "broken-with-family")
            .expect("fixture entry");
        let nameless = fonts
            .iter()
            .find(|f| f.label == "broken-nameless")
            .expect("fixture entry");
        assert!(
            with_family.post_script_name().is_empty() && nameless.post_script_name().is_empty(),
            "the fixture must have no PostScript name, or it is not testing the fallback"
        );
        assert_eq!(
            with_family.render_identity_name(),
            "Rescued Family",
            "with no PostScript name the family name is the identity"
        );
        assert_eq!(
            nameless.render_identity_name(),
            "broken-nameless",
            "with neither name the file-stem label is the identity"
        );
    }

    // Two DIFFERENT files claiming one PostScript name: each gets a `%hash` identity
    // derived from its OWN bytes, so neither can render as the other and neither shifts
    // when the other claimant comes or goes.
    #[test]
    fn assign_identity_same_post_script_name_different_bytes_gets_stable_hash_suffixes() {
        let dup_a = || raw_font_ps("/fonts/groups/A/dup.ttf", Some("A"), 0x1100_0000_0000_0000, "Fam", "FamPS");
        let dup_b = || raw_font_ps("/fonts/groups/B/dup.ttf", Some("B"), 0x2200_0000_0000_0000, "Fam", "FamPS");
        let dup_c = || raw_font_ps("/fonts/groups/C/dup.ttf", Some("C"), 0x3300_0000_0000_0000, "Fam", "FamPS");

        let pair = fonts_with_identities(vec![dup_a(), dup_b()]);
        assert_eq!(pair.len(), 2, "same name + different content stay separate");
        assert_eq!(pair[0].render_identity_name(), "FamPS%1100000000000000");
        assert_eq!(pair[1].render_identity_name(), "FamPS%2200000000000000");

        // Adding or removing ANOTHER claimant of the same name must not rewrite the
        // identity of the ones already there (an ordinal suffix would have renumbered
        // them and invalidated everything persisted before).
        let trio = fonts_with_identities(vec![dup_a(), dup_b(), dup_c()]);
        assert_eq!(trio[0].render_identity_name(), "FamPS%1100000000000000");
        assert_eq!(trio[1].render_identity_name(), "FamPS%2200000000000000");
        assert_eq!(trio[2].render_identity_name(), "FamPS%3300000000000000");

        // Reordering the list does not change any identity either.
        let reordered = fonts_with_identities(vec![dup_c(), dup_a()]);
        let c = reordered.iter().find(|f| f.groups.contains(&Some("C".to_string()))).unwrap();
        let a = reordered.iter().find(|f| f.groups.contains(&Some("A".to_string()))).unwrap();
        assert_eq!(c.render_identity_name(), "FamPS%3300000000000000");
        assert_eq!(a.render_identity_name(), "FamPS%1100000000000000");

        // The bare name — the form already persisted in old documents — resolves to the
        // LOWEST-hash claimant, deterministically.
        let provider = TabFontProvider::from_fonts(&trio);
        assert_eq!(
            provider.resolved_path_for("FamPS"),
            Some(Path::new("/fonts/groups/A/dup.ttf")),
            "the bare contested name resolves to the lowest-hash claimant"
        );
    }

    /// The content hash is a PERSISTED contract (it is spelled into collision-suffixed
    /// identities), so this pins its VALUE, not just its shape: the first 8 bytes of the
    /// SHA-256 digest, big-endian. Swapping the algorithm — which is exactly what a
    /// toolchain upgrade did silently while this was `DefaultHasher` — must fail here.
    #[test]
    fn the_content_hash_is_a_pinned_sha256_prefix() {
        // Golden values, independently reproducible:
        //   python3 -c "import hashlib;print(hashlib.sha256(b'...').digest()[:8].hex())"
        assert_eq!(font_content_hash(b""), 0xe3b0_c442_98fc_1c14);
        assert_eq!(
            font_content_hash(b"ManhwaStudio font identity"),
            0x6f39_61c1_24e0_a7c3
        );
        assert_eq!(font_content_hash(b"not a font"), 0xaf4d_9eef_04c1_24f5);

        // And the identity spelled from it, end to end: bytes -> hash -> suffixed
        // identity. A changed hash or a changed suffix format breaks this line.
        let hash = font_content_hash(b"ManhwaStudio font identity");
        assert_eq!(
            suffixed_font_identity_name("Acme-Regular", hash),
            "Acme-Regular%6f3961c124e0a7c3"
        );
    }

    /// A PostScript name that the specification forbids is treated as ABSENT — the rule
    /// that keeps the identity namespace clean, because it is what guarantees no real
    /// name can contain the collision-suffix separator.
    #[test]
    fn an_invalid_post_script_name_counts_as_no_name_at_all() {
        for valid in ["Acme-Regular", "A", "CCWildWordsLower-Italic", "a1_2.3-4"] {
            assert!(
                is_valid_post_script_name(valid),
                "'{valid}' is a spec-valid PostScript name"
            );
        }
        for invalid in [
            "",
            "   ",
            "Two Words",              // interior space
            "Acme%1122334455667788",  // the identity separator
            "Acme/Bold",              // PostScript delimiter
            "Acme(Bold)",
            "Acme[1]",
            "Acme{x}",
            "Acme<x>",
            "Кириллица",              // non-ASCII
            "Acme\u{7f}",             // non-printable
        ] {
            assert!(
                !is_valid_post_script_name(invalid),
                "'{invalid}' must NOT be accepted as a PostScript name"
            );
        }
        // 63 chars is the OpenType limit; 64 is over it.
        assert!(is_valid_post_script_name(&"a".repeat(63)));
        assert!(!is_valid_post_script_name(&"a".repeat(64)));
        // Surrounding whitespace is trimmed before validating (every identity comparison
        // trims), but an interior space still invalidates the name.
        assert!(is_valid_post_script_name("  Acme-Regular  "));

        // End to end: a file declaring an invalid name falls back to its family name,
        // exactly like a file that declares none.
        let fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/spaced.ttf", None, 1, "Spaced Family", "Bad Name"),
            raw_font_ps("/fonts/sep.ttf", None, 2, "Sep Family", "Acme%1122334455667788"),
        ]);
        assert_eq!(fonts[0].render_identity_name(), "Spaced Family");
        assert_eq!(fonts[1].render_identity_name(), "Sep Family");
        assert!(
            fonts.iter().all(|font| font.post_script_name().is_empty()),
            "an invalid name is not stored on the entry either"
        );
    }

    /// A collision suffix can never be mistaken for some font's REAL name, and no font
    /// can imitate another font's suffixed identity.
    ///
    /// Two mechanisms are pinned together, because either alone is insufficient: the
    /// separator is a character the PostScript spec forbids (so a valid name cannot
    /// contain it), and a base identity that reaches the fallback carrying the separator
    /// anyway is suffixed unconditionally (so it cannot collide with a suffixed form
    /// either).
    #[test]
    fn a_suffixed_identity_cannot_collide_with_a_real_font_name() {
        // Two different files claiming "Acme": both get suffixed identities.
        let contested_a = || raw_font_ps("/fonts/a.ttf", None, 0x1111_1111_1111_1111, "Fam", "Acme");
        let contested_b = || raw_font_ps("/fonts/b.ttf", None, 0x2222_2222_2222_2222, "Fam2", "Acme");
        // A third file whose FAMILY name (its identity fallback — no valid PostScript
        // name) is literally the suffixed identity of the first one.
        let impostor = || {
            raw_font_ps(
                "/fonts/impostor.ttf",
                None,
                0x3333_3333_3333_3333,
                "Acme%1111111111111111",
                "",
            )
        };

        let fonts = fonts_with_identities(vec![contested_a(), contested_b(), impostor()]);
        assert_eq!(fonts[0].render_identity_name(), "Acme%1111111111111111");
        assert_eq!(fonts[1].render_identity_name(), "Acme%2222222222222222");
        assert_eq!(
            fonts[2].render_identity_name(),
            "Acme%1111111111111111%3333333333333333",
            "a fallback base carrying the separator is suffixed too, so it cannot BE \
             another font's suffixed identity"
        );
        assert_ne!(
            fonts[0].render_identity_name(),
            fonts[2].render_identity_name()
        );

        // Panel and provider resolve the contested identity to the real font, never to
        // the impostor.
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts;
        let provider = TabFontProvider::from_fonts(&state.fonts);
        assert_eq!(
            provider.resolved_path_for("Acme%1111111111111111"),
            Some(Path::new("/fonts/a.ttf"))
        );
        assert_eq!(
            state.find_font_idx_by_name_forms("Acme%1111111111111111"),
            Some(0)
        );

        // And a font may not declare the suffixed spelling as its own PostScript name at
        // all: the separator makes the name invalid, so it never becomes an identity.
        let declared = fonts_with_identities(vec![raw_font_ps(
            "/fonts/declared.ttf",
            None,
            0x4444_4444_4444_4444,
            "Declared Family",
            "Acme%1111111111111111",
        )]);
        assert_eq!(declared[0].render_identity_name(), "Declared Family");
    }

    /// The suffix must distinguish exactly what contest detection distinguishes. Two
    /// files whose hashes agree in their HIGH 32 bits are different files, are detected as
    /// contesting the name — and used to receive the SAME truncated suffix, i.e. one
    /// identity for two fonts.
    #[test]
    fn hashes_sharing_their_high_bits_still_get_distinct_identities() {
        let fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/a.ttf", None, 0x1234_5678_0000_0001, "Fam", "FamPS"),
            raw_font_ps("/fonts/b.ttf", None, 0x1234_5678_0000_0002, "Fam", "FamPS"),
        ]);
        assert_eq!(fonts[0].render_identity_name(), "FamPS%1234567800000001");
        assert_eq!(fonts[1].render_identity_name(), "FamPS%1234567800000002");
        assert_ne!(
            fonts[0].render_identity_name(),
            fonts[1].render_identity_name(),
            "a truncated suffix would have given both files the identity 'FamPS~12345678'"
        );
        let provider = TabFontProvider::from_fonts(&fonts);
        assert_eq!(
            provider.resolved_path_for("FamPS%1234567800000002"),
            Some(Path::new("/fonts/b.ttf"))
        );
    }

    /// Files that claim NO identity (no valid PostScript name, or no computed content
    /// hash) never merge: two different unreadable files that happen to share a file stem
    /// are two files, and folding them hid one of them from the user entirely.
    #[test]
    fn files_without_an_identity_never_merge() {
        // Same stem, same "hash" (0 = not computed), different folders: two entries.
        let broken = merge_duplicate_fonts(vec![
            raw_font_named("/fonts/groups/a/Broken.ttf", Some("a"), 0, ""),
            raw_font_named("/fonts/groups/b/broken.ttf", Some("b"), 0, ""),
        ]);
        assert_eq!(
            broken.len(),
            2,
            "two unreadable files are two files, whatever their stems"
        );
        assert!(broken.iter().all(|entry| entry.alt_paths.is_empty()));

        // Even identical BYTES do not merge them: without a name there is nothing to say
        // they are the same font rather than two copies of the same garbage the user
        // still wants to see listed separately.
        let same_bytes = merge_duplicate_fonts(vec![
            raw_font_named("/fonts/groups/a/Broken.ttf", Some("a"), 77, ""),
            raw_font_named("/fonts/groups/b/broken.ttf", Some("b"), 77, ""),
        ]);
        assert_eq!(same_bytes.len(), 2);

        // A file WITH a valid name but no computed hash (unreadable) does not merge either.
        let no_hash = merge_duplicate_fonts(vec![
            raw_font_ps("/fonts/a/X.ttf", Some("a"), 0, "Fam", "FamPS"),
            raw_font_ps("/fonts/b/X.ttf", Some("b"), 0, "Fam", "FamPS"),
        ]);
        assert_eq!(no_hash.len(), 2);
    }

    /// The same, through the real folder loader: two DIFFERENT unparsable files sharing a
    /// stem must both stay in the list.
    #[test]
    fn two_unparsable_files_with_one_stem_are_both_listed() {
        let dir = unique_temp_dir("broken_stems");
        let group_a = dir.join("groups").join("a");
        let group_b = dir.join("groups").join("b");
        fs::create_dir_all(&group_a).expect("create temp dir");
        fs::create_dir_all(&group_b).expect("create temp dir");
        fs::write(group_a.join("Broken.ttf"), b"garbage one").expect("write garbage");
        fs::write(group_b.join("broken.ttf"), b"garbage two").expect("write garbage");

        let entries = folder_font_entries(&dir);
        assert_eq!(
            entries.len(),
            2,
            "neither broken file may disappear from the list"
        );
        assert!(
            entries.iter().all(|entry| entry.alt_paths.is_empty()),
            "nothing merged, so no entry may claim the other's file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A merged cluster of byte-identical copies has exactly ONE display-name override slot
    /// now that overrides are keyed by IDENTITY, so the override cannot depend on WHICH copy
    /// became the representative.
    ///
    /// This replaces the former `first_display_name_override` scan (phase 4 of
    /// `dev-docs/font_identity_postscript_plan.md` removed it): with a path key each copy had
    /// its own slot and the loader had to scan every path to find the user's rename.
    #[test]
    fn byte_identical_copies_share_one_display_name_override_slot() {
        let mut fonts = merge_duplicate_fonts(vec![
            raw_font_ps("/fonts/b.ttf", None, 42, "Alpha", "Alpha-Regular"),
            raw_font_ps("/fonts/a.ttf", None, 42, "Alpha", "Alpha-Regular"),
        ]);
        assert_eq!(fonts.len(), 1, "the byte-identical copies merge into one entry");
        assign_font_identity_names(&mut fonts);
        // Whichever file sorted first, the settings key is the shared identity.
        assert_eq!(fonts[0].path, PathBuf::from("/fonts/a.ttf"));
        assert_eq!(fonts[0].alt_paths, vec![PathBuf::from("/fonts/b.ttf")]);
        assert_eq!(fonts[0].render_identity_name(), "Alpha-Regular");
    }

    // Byte-identical copies of one font under DIFFERENT file names are one font: keying
    // the merge on the PostScript name folds them into a single entry (six such pairs
    // ship under `fonts/`) with the union of their folder groups.
    #[test]
    fn identical_bytes_under_different_file_names_merge_into_one_entry() {
        let entries = merge_duplicate_fonts(vec![
            raw_font_ps("/fonts/groups/ВВД/Мысли.ttf", Some("ВВД"), 42, "CCWildWordsLower", "CCWildWordsLower-Italic"),
            raw_font_ps(
                "/fonts/groups/Империя/Мысли-Italic.ttf",
                Some("Империя"),
                42,
                "CCWildWordsLower",
                "CCWildWordsLower-Italic",
            ),
        ]);
        assert_eq!(entries.len(), 1, "one font, two file names -> one entry");
        let font = &entries[0];
        assert!(font.groups.contains(&Some("ВВД".to_string())));
        assert!(font.groups.contains(&Some("Империя".to_string())));
        assert!(
            font_matches_path(font, "/fonts/groups/Империя/Мысли-Italic.ttf"),
            "the folded copy stays matchable by its own path"
        );
        assert_eq!(font.render_identity_name(), "CCWildWordsLower-Italic");
    }

    // Write-site identity: editing a legacy blob that named the Regular file by its old
    // stem re-persists the POSTSCRIPT-NAME identity, and a Bold span over a Regular base
    // emits its own font tag (identities differ -> no no-op).
    #[test]
    fn legacy_reference_write_identity_is_the_post_script_name_and_span_emits_tag() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/myfont-regular.ttf", None, 1, "MyFont", "MyFont-Regular"),
            raw_font_ps("/fonts/myfont-bold.ttf", None, 2, "MyFont", "MyFont-Bold"),
        ]);
        let reg = state
            .fonts
            .iter()
            .position(|f| f.label == "myfont-regular")
            .unwrap();
        let bold = state
            .fonts
            .iter()
            .position(|f| f.label == "myfont-bold")
            .unwrap();

        // (b) A legacy blob font_label:"myfont-regular" (the old file-stem identity)
        // resolves to the Regular file, and what gets persisted from now on is its
        // PostScript name.
        state.select_font_by_legacy_reference(None, &["myfont-regular"]);
        assert_eq!(state.selected_font_idx, reg);
        assert!(state.missing_font.is_none());
        assert_eq!(
            state.font_identity_name_by_idx(reg).as_deref(),
            Some("MyFont-Regular")
        );

        // (c) base = Regular, span = Bold -> identities differ -> a font tag is emitted.
        state.selected_font_idx = reg;
        let span = state.normalize_desired_inline_tag_style(TypingInlineTagStyle {
            font_label: state.font_identity_name_by_idx(bold),
            ..TypingInlineTagStyle::default()
        });
        assert_eq!(
            span.font_label.as_deref(),
            Some("MyFont-Bold"),
            "a Bold span over a Regular base must keep its own font tag"
        );
    }

    // Precedence alignment: a name that is one font's IDENTITY and another's legacy alias
    // resolves to the SAME font in the panel lookup and the provider.
    #[test]
    fn identity_and_legacy_alias_collision_resolves_to_same_font_in_panel_and_provider() {
        let mut state = TypingCreatePanelState::new(false);
        // Font A: file stem/label "beta", PostScript name "AlphaFamily-Regular".
        // Font B: file stem/label "gamma", PostScript name (and identity) "beta".
        state.fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/beta.ttf", None, 1, "Alpha Family", "AlphaFamily-Regular"),
            raw_font_ps("/fonts/gamma.ttf", None, 2, "Gamma Family", "beta"),
        ]);
        let b = state
            .fonts
            .iter()
            .position(|f| f.post_script_name() == "beta")
            .expect("fixture defines a font whose PostScript name is 'beta'");

        // Panel: the provider-aligned ordered lookup picks B by its identity.
        assert_eq!(
            state.find_font_idx_by_name_forms("beta"),
            Some(b),
            "panel must resolve 'beta' to the font whose IDENTITY is 'beta'"
        );
        // Provider: the same name resolves to the same font.
        let provider = TabFontProvider::from_fonts(&state.fonts);
        assert_eq!(
            provider.resolved_path_for("beta"),
            Some(state.fonts[b].path.as_path()),
            "provider must resolve 'beta' to the SAME font the panel picks"
        );
    }

    #[test]
    fn identical_fonts_merge_and_union_groups() {
        // Одна идентичность (PostScript-имя) + одинаковый хэш в корне и в группе → один
        // шрифт. Фикстуры несут PostScript-имя, потому что слияние теперь ТРЕБУЕТ
        // идентичности: файлы, которые нечем опознать, не сливаются вовсе (иначе два
        // разных нечитаемых файла с одним stem схлопывались в один пункт).
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = merge_duplicate_fonts(vec![
            raw_font_ps("/fonts/Разговор.ttf", None, 42, "Разговор Fam", "Razgovor-Regular"),
            raw_font_ps(
                "/fonts/groups/A/Разговор.ttf",
                Some("A"),
                42,
                "Разговор Fam",
                "Razgovor-Regular",
            ),
        ]);
        assert_eq!(state.fonts.len(), 1);
        let font = &state.fonts[0];
        assert_eq!(font.label, "Разговор");
        assert!(font.groups.contains(&None));
        assert!(font.groups.contains(&Some("A".to_string())));
        // Оба пути копий сохранены: представитель + `alt_paths`.
        assert_eq!(font.alt_paths.len(), 1, "the folded copy keeps its own path");
        assert!(font_in_group(font, "A"));
        // Путь больше не ключ выбора, но документ, записанный старой сборкой, обязан
        // по-прежнему находить слитый шрифт по ЛЮБОЙ из его копий.
        for path in ["/fonts/Разговор.ttf", "/fonts/groups/A/Разговор.ttf"] {
            assert_eq!(
                state.find_font_idx_by_legacy_reference(Some(path), None),
                Some(0),
                "a legacy path reference to '{path}' must resolve to the merged entry"
            );
        }
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

    /// The legacy READ path (a persisted `font_path` + name from an older build) still
    /// selects a loaded font and still degrades to `missing_font` when it cannot — the
    /// original intent, now asserted against the one sanctioned path-accepting helper.
    #[test]
    fn selecting_missing_overlay_font_sets_warning_and_clears_on_found() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![raw_font_ps(
            "/fonts/Доступный.ttf",
            None,
            11,
            "Доступный Fam",
            "Dostupny-Regular",
        )]);
        state.selected_font_idx = 0;

        // Шрифт оверлея отсутствует среди доступных → запоминаем его имя.
        state.select_font_by_legacy_reference(Some("/fonts/Пропавший.ttf"), &["Пропавший"]);
        assert_eq!(state.missing_font.as_deref(), Some("Пропавший"));

        // Без метки берём имя файла из пути.
        state.select_font_by_legacy_reference(Some("/fonts/ДругойПропавший.otf"), &[]);
        assert_eq!(state.missing_font.as_deref(), Some("ДругойПропавший.otf"));

        // Найденный шрифт снимает блокировку рендера. Легаси-ссылка попадает в шрифт и по
        // пути, и по устаревшему имени-стему...
        state.select_font_by_legacy_reference(Some("/fonts/Доступный.ttf"), &["Доступный"]);
        assert!(state.missing_font.is_none());
        assert_eq!(state.selected_font_idx, 0);
        // ...а ключом выбора при этом становится ИДЕНТИЧНОСТЬ, а не путь.
        assert_eq!(
            state.active_font_identity.as_deref(),
            Some("Dostupny-Regular")
        );
    }

    /// Hands `state` a finished background font reload synchronously: builds the channel
    /// the worker would have used, sends the fresh list with the current token, and polls.
    /// Lets the reload-restore contract be tested without a worker thread.
    fn deliver_font_reload(state: &mut TypingCreatePanelState, fonts: Vec<FontEntry>) {
        let (tx, rx) = mpsc::channel::<FontReloadResult>();
        state.latest_font_reload_token = state.latest_font_reload_token.wrapping_add(1);
        tx.send(FontReloadResult {
            token: state.latest_font_reload_token,
            fonts,
            font_groups: Vec::new(),
        })
        .expect("the receiver is alive in this test");
        state.font_reload_rx = Some(rx);
        state.poll_font_reload_results();
    }

    /// After a background font reload the selection follows the font's IDENTITY, not its
    /// slot and not its path: a moved file and a reordered list keep the same font
    /// selected. When the identity is gone from the new list, the panel says so
    /// (`missing_font`) instead of guessing positionally — the old
    /// `min(selected_idx, len - 1)` fallback silently selected a DIFFERENT font.
    #[test]
    fn font_reload_restores_the_selection_by_identity_and_flags_a_vanished_font() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/a.ttf", None, 1, "Alpha", "Alpha-Regular"),
            raw_font_ps("/fonts/b.ttf", None, 2, "Beta", "Beta-Regular"),
        ]);
        state.selected_font_idx = 1;
        state.active_font_identity = state.current_font_identity();
        assert_eq!(state.active_font_identity.as_deref(), Some("Beta-Regular"));

        // Same two fonts, opposite order, and the selected one MOVED to another folder.
        deliver_font_reload(
            &mut state,
            fonts_with_identities(vec![
                raw_font_ps("/fonts/moved/b.ttf", None, 2, "Beta", "Beta-Regular"),
                raw_font_ps("/fonts/a.ttf", None, 1, "Alpha", "Alpha-Regular"),
            ]),
        );
        assert_eq!(state.selected_font_idx, 0);
        assert_eq!(
            state.current_font_identity().as_deref(),
            Some("Beta-Regular"),
            "the selection follows the identity through a reorder and a file move"
        );
        assert!(state.missing_font.is_none());

        // The selected font disappears: no positional guess, an explicit missing state.
        deliver_font_reload(
            &mut state,
            fonts_with_identities(vec![raw_font_ps(
                "/fonts/a.ttf",
                None,
                1,
                "Alpha",
                "Alpha-Regular",
            )]),
        );
        assert_eq!(
            state.missing_font.as_deref(),
            Some("Beta-Regular"),
            "a vanished identity must leave the panel in the missing-font state"
        );
    }

    /// The SOUGHT identity must survive a failed restore and be used again by the NEXT
    /// reload: the panel used to overwrite `active_font_identity` with
    /// `current_font_identity()` — i.e. with the identity of the NEIGHBOUR the clamped index
    /// landed on — so putting the font back restored the neighbour instead, permanently.
    /// A successful restore must also CLEAR the missing-font block.
    #[test]
    fn a_vanished_font_is_restored_when_it_comes_back() {
        let alpha = || raw_font_ps("/fonts/a.ttf", None, 1, "Alpha", "Alpha-Regular");
        let beta = || raw_font_ps("/fonts/b.ttf", None, 2, "Beta", "Beta-Regular");

        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![alpha(), beta()]);
        state.selected_font_idx = 1;
        state.active_font_identity = state.current_font_identity();
        assert_eq!(state.active_font_identity.as_deref(), Some("Beta-Regular"));

        // Beta disappears: the panel says so and keeps LOOKING FOR Beta.
        deliver_font_reload(&mut state, fonts_with_identities(vec![alpha()]));
        assert_eq!(state.missing_font.as_deref(), Some("Beta-Regular"));
        assert_eq!(
            state.active_font_identity.as_deref(),
            Some("Beta-Regular"),
            "the sought identity must not be replaced by the neighbour the index clamped onto"
        );

        // A second reload while it is still gone must not drift onto the neighbour either.
        deliver_font_reload(&mut state, fonts_with_identities(vec![alpha()]));
        assert_eq!(state.active_font_identity.as_deref(), Some("Beta-Regular"));

        // The user puts the file back: the very next reload restores the SELECTION and
        // lifts the block, with no intermediate substitution.
        deliver_font_reload(&mut state, fonts_with_identities(vec![alpha(), beta()]));
        assert_eq!(
            state.current_font_identity().as_deref(),
            Some("Beta-Regular"),
            "the returning font must be re-selected, not the neighbour"
        );
        assert!(
            state.missing_font.is_none(),
            "a successful restore must clear the missing-font block"
        );
        assert_eq!(state.active_font_identity.as_deref(), Some("Beta-Regular"));
    }

    /// Builds a create preset naming `primary` (its font IDENTITY — the one font key a
    /// preset carries since phase 5) with the given per-font profile map.
    fn preset_named(primary: &str, font_profiles: HashMap<String, Value>) -> TypingCreatePreset {
        TypingCreatePreset {
            font: primary.to_string(),
            font_profiles,
        }
    }

    /// A minimal font profile whose only distinguishing content is the font size, so a
    /// test can see WHICH profile was applied (or that none was).
    fn profile_with_font_size(size_px: f32) -> Value {
        serde_json::json!({ "text_params": { "font_size_px": size_px } })
    }

    /// A preset whose PRIMARY font is not loaded must not be applied to whatever font
    /// happens to be selected: the panel takes the same `missing_font` state an overlay
    /// load produces, keeps the selection, and changes no parameter. It used to silently
    /// re-anchor `active_font_identity` to the CURRENT font and apply the preset to it.
    #[test]
    fn a_preset_naming_an_unavailable_font_flags_it_instead_of_switching_fonts() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/a.ttf", None, 1, "Alpha", "Alpha-Regular"),
            raw_font_ps("/fonts/b.ttf", None, 2, "Beta", "Beta-Regular"),
        ]);
        state.selected_font_idx = 0;
        state.active_font_identity = state.current_font_identity();
        let size_before = state.font_size_px;

        // The preset even carries a profile for the CURRENTLY selected font — applying it
        // is exactly the silent substitution being forbidden.
        let mut profiles = HashMap::new();
        profiles.insert("Alpha-Regular".to_string(), profile_with_font_size(99.0));
        state.presets_by_name.insert(
            "Пропавший".to_string(),
            preset_named("Gamma-Regular", profiles),
        );
        state.apply_preset_by_name("Пропавший".to_string());

        assert_eq!(
            state.missing_font.as_deref(),
            Some("Gamma-Regular"),
            "the preset's own font is not loaded and must be reported as missing"
        );
        assert_eq!(
            state.selected_font_idx, 0,
            "the selection must not move to a font the preset never named"
        );
        assert_eq!(
            state.active_font_identity.as_deref(),
            Some("Alpha-Regular"),
            "the profile anchor must not be re-pointed by a preset that was not applied"
        );
        assert!(
            (state.font_size_px - size_before).abs() < f32::EPSILON,
            "no parameter of the current font may be overwritten by an unapplied preset"
        );
        assert_eq!(state.selected_preset_name.as_deref(), Some("Пропавший"));

        // A preset naming a LOADED font applies normally and lifts the block.
        let mut profiles = HashMap::new();
        profiles.insert("Beta-Regular".to_string(), profile_with_font_size(42.0));
        state
            .presets_by_name
            .insert("Есть".to_string(), preset_named("Beta-Regular", profiles));
        state.apply_preset_by_name("Есть".to_string());
        assert!(state.missing_font.is_none());
        assert_eq!(state.selected_font_idx, 1);
        assert!((state.font_size_px - 42.0).abs() < f32::EPSILON);
    }

    /// Two LEGACY profile keys naming one font must collapse deterministically. The
    /// conversion used to resolve the clash by `HashMap` iteration order — randomized per
    /// process, so which profile survived was a coin toss between runs.
    #[test]
    fn colliding_legacy_profile_keys_collapse_by_a_fixed_priority() {
        // The same font is named twice: once by its file PATH (the weakest legacy form)
        // and once by its file-stem NAME. Repeated so a lucky iteration order cannot pass.
        for _ in 0..32 {
            let mut state = TypingCreatePanelState::new(false);
            state.fonts = fonts_with_identities(vec![raw_font_ps(
                "/fonts/Regular.ttf",
                None,
                7,
                "Alpha",
                "Alpha-Regular",
            )]);
            state.selected_font_idx = 0;

            let mut profiles = HashMap::new();
            profiles.insert(
                "/fonts/Regular.ttf".to_string(),
                profile_with_font_size(11.0),
            );
            profiles.insert("Regular".to_string(), profile_with_font_size(77.0));
            // An unresolvable key must survive untouched, whatever the collision did.
            profiles.insert("Ghost-Regular".to_string(), profile_with_font_size(5.0));
            state
                .presets_by_name
                .insert("P".to_string(), preset_named("Alpha-Regular", profiles));
            state.apply_preset_by_name("P".to_string());

            let kept = state
                .font_profiles_by_identity
                .get("Alpha-Regular")
                .expect("both legacy keys convert onto the loaded font's identity");
            assert_eq!(
                kept.pointer("/text_params/font_size_px").and_then(Value::as_f64),
                Some(77.0),
                "the NAME key must beat the PATH key, every time"
            );
            assert!(
                state.font_profiles_by_identity.contains_key("Ghost-Regular"),
                "a key that resolves to nothing is kept verbatim, not dropped"
            );
            assert_eq!(
                state.font_profiles_by_identity.stored_count(),
                2,
                "the two colliding keys become one entry; the unresolvable one stays"
            );
        }
    }

    /// A stored key that differs from the identity only in CASE must land on the identity's
    /// own spelling, otherwise the profile sits under a key no lookup ever asks for.
    #[test]
    fn a_case_differing_profile_key_is_canonicalized_to_the_identity() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![raw_font_ps(
            "/fonts/a.ttf",
            None,
            1,
            "Alpha",
            "Alpha-Regular",
        )]);
        state.selected_font_idx = 0;

        let mut profiles = HashMap::new();
        profiles.insert("alpha-regular".to_string(), profile_with_font_size(33.0));
        state
            .presets_by_name
            .insert("P".to_string(), preset_named("Alpha-Regular", profiles));
        state.apply_preset_by_name("P".to_string());

        assert!(
            state.font_profiles_by_identity.contains_key("Alpha-Regular"),
            "the profile must be reachable under the identity the panel looks up"
        );
        assert!((state.font_size_px - 33.0).abs() < f32::EPSILON);
    }

    // -----------------------------------------------------------------------------
    // "A path is not proof of identity" — the SELECTION side of safety rule D
    // (`dev-docs/font_identity_postscript_plan.md` phase 5, defect E). The document
    // CONVERSION already refused a path-only match; the panel's own legacy selection
    // still let a supplied path beat every stored name.
    // -----------------------------------------------------------------------------

    /// A schema-1 overlay whose stored NAME resolves must select THAT font, even when its
    /// stored `font_path` now holds a different, installed font. The path used to win
    /// outright, so replacing a font file under its old name silently re-pointed every
    /// layer that remembered the path — and the next edit re-rendered them in the new
    /// typeface without a word.
    #[test]
    fn a_stored_name_beats_a_stored_path_when_they_name_different_fonts() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/x.ttf", None, 1, "X Family", "XPS-Regular"),
            raw_font_ps("/fonts/dialogue.ttf", None, 2, "Y Family", "YPS-Regular"),
        ]);
        let x = state
            .fonts
            .iter()
            .position(|font| font.render_identity_name() == "XPS-Regular")
            .expect("font X is loaded");
        state.selected_font_idx = state.fonts.len() - 1;

        // The layer was written when `/fonts/dialogue.ttf` held font X; that file now
        // holds font Y, while X itself is still installed elsewhere.
        let legacy = serde_json::json!({
            "text_params": {
                "text": "t",
                "font_label": "XPS-Regular",
                "font_path": "/fonts/dialogue.ttf",
            }
        });
        state.apply_render_data_json_with_options(&legacy, true);
        assert_eq!(
            state.selected_font_idx, x,
            "the stored NAME identifies the font; the stored path is only a hint"
        );
        assert!(state.missing_font.is_none());
    }

    /// When NO stored name resolves and only the stored path still points at an installed
    /// font, nothing is selected: the panel reports the overlay's font as missing instead
    /// of quietly adopting whatever file now sits there.
    #[test]
    fn a_path_only_match_selects_nothing_and_flags_the_font_missing() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/other.ttf", None, 1, "Other Family", "Other-Regular"),
            raw_font_ps("/fonts/dialogue.ttf", None, 2, "New Family", "NewDisplay"),
        ]);
        state.selected_font_idx = 0;
        state.missing_font = None;

        // The names the layer remembers are installed nowhere; only its path resolves.
        // (The full read path additionally offers the path's file STEM as the last NAME
        // candidate — `text_params_schema::legacy_font_name_candidates` — which is exactly
        // how the codec resolves, so panel and conversion keep agreeing.)
        state.select_font_by_legacy_reference(Some("/fonts/dialogue.ttf"), &["OldDialogue"]);
        assert_eq!(
            state.missing_font.as_deref(),
            Some("OldDialogue"),
            "a file at a remembered path is not proof that it is the same font"
        );
        assert_eq!(
            state.selected_font_idx, 0,
            "the selection must not move to the font that now occupies the path"
        );
    }

    /// A preset profile key that reads BOTH as a font NAME and as another font's file PATH
    /// must land on the NAME. The key used to be handed to the resolver as a path AND as a
    /// name at once, and the path was tried first, so the profile was attached to whichever
    /// font happened to live at that location.
    ///
    /// The fixture is deliberately synthetic (font B's file is literally named after font
    /// A's FAMILY, so the key reads as A's legacy family alias and as B's path): it is the
    /// smallest situation in which the two readings of ONE key disagree, which is precisely
    /// what the priority rule decides.
    #[test]
    fn a_profile_key_that_is_also_another_fonts_path_resolves_by_name() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![
            raw_font_ps("/fonts/a.ttf", None, 1, "Alpha Family", "Alpha-Regular"),
            raw_font_ps("Alpha Family", None, 2, "Beta Family", "Beta-Regular"),
        ]);
        state.selected_font_idx = 0;

        let mut profiles = HashMap::new();
        profiles.insert("Alpha Family".to_string(), profile_with_font_size(21.0));
        state
            .presets_by_name
            .insert("P".to_string(), preset_named("Alpha-Regular", profiles));
        state.apply_preset_by_name("P".to_string());

        assert!(
            state.font_profiles_by_identity.contains_key("Alpha-Regular"),
            "the key is font A's legacy family NAME, which outranks its reading as font B's path"
        );
        assert!(
            !state.font_profiles_by_identity.contains_key("Beta-Regular"),
            "a path reading must never capture a profile a name already claims"
        );
    }

    // -----------------------------------------------------------------------------
    // Presets move to `fonts/presets.json` (phase 5): the one-shot migration out of
    // `user_config.TextTab.create_presets` and the end of the profile fan-out on save.
    // -----------------------------------------------------------------------------

    /// A panel whose font list mirrors the real one behind the user's stored presets:
    /// three fonts inside the project's `fonts/` tree (one of them in a group folder) —
    /// and nothing at all for the paths pointing outside the project.
    fn state_with_migration_fonts() -> TypingCreatePanelState {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![
            raw_font_ps(
                "/proj/fonts/groups/ВВД/Крик.ttf",
                Some("ВВД"),
                1,
                "Krik Family",
                "Krik-Regular",
            ),
            raw_font_ps(
                "/proj/fonts/groups/Империя/Разговор.otf",
                Some("Империя"),
                2,
                "Razgovor Family",
                "Razgovor-Regular",
            ),
            raw_font_ps("/proj/fonts/простой V2.ttf", None, 3, "Arimo", "Arimo-Regular"),
            raw_font_ps("/proj/fonts/ANTQUAB.ttf", None, 4, "Book Antiqua", "BookAntiqua-Bold"),
        ]);
        state.selected_font_idx = 0;
        state
    }

    /// A schema-1 profile BODY as the user's `user_config.json` actually stores it: a
    /// `render_data` object whose `text_params` carries the legacy font keys and the
    /// legacy px/percent pairs.
    fn legacy_profile_body(font_stem: &str, font_path: &str, size_px: f32) -> Value {
        serde_json::json!({
            "effects": [],
            "text_params": {
                "text": "Текст будет выглядеть так",
                "font_label": font_stem,
                "font_path": font_path,
                "font_size_px": size_px,
                "kerning_px": 0.0,
                "kerning_percent": 0.0,
                "width_px": 300,
            }
        })
    }

    /// One legacy preset in the exact shape the user's config holds.
    fn legacy_preset(
        key: &str,
        label: &str,
        profiles: Vec<(&str, Value)>,
    ) -> presets_store::LegacyCreatePreset {
        presets_store::LegacyCreatePreset {
            primary_font_key: key.to_string(),
            primary_font_path: Some(key.to_string()),
            primary_font_label: Some(label.to_string()),
            font_profiles: profiles
                .into_iter()
                .map(|(key, body)| (key.to_string(), body))
                .collect(),
        }
    }

    /// CLAUDE.md §5. Constructing a create/edit panel must not touch the font directory:
    /// scanning it, reading every file, hashing it and parsing it is exactly the work
    /// forbidden on the GUI thread — and the constructor used to do it once PER PANEL, so
    /// every session paid for it twice.
    ///
    /// Pinned by the parse JOURNAL, not by timing: a font file sitting in the panel's own
    /// fonts directory must have no recorded read after the constructor returns. The list
    /// itself is empty (bar the synthetic built-in entry, which reads no file) and is
    /// filled by the background load.
    #[test]
    fn constructing_a_panel_reads_no_font_file() {
        let dir = unique_temp_dir("panel_ctor_no_font_io");
        fs::create_dir_all(&dir).expect("create temp fonts dir");
        let fixture = advanced_form_fixture_font_path();
        let planted = dir.join("Никогда не читается.ttf");
        fs::copy(&fixture, &planted).expect("copy fixture");

        create_state::set_test_fonts_dir(Some(dir.clone()));
        let state = TypingCreatePanelState::new(false);
        create_state::set_test_fonts_dir(None);

        assert_eq!(
            font_file_parses(&planted),
            Vec::<usize>::new(),
            "the constructor must not read a single font file"
        );
        assert!(
            state
                .fonts
                .iter()
                .all(|font| font.bundled_stack_font().is_some()),
            "the list holds nothing but the synthetic built-in entry until the load lands"
        );
        assert!(
            state.font_groups.is_empty(),
            "font groups arrive with the same background load"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// TEST ISOLATION. A panel built in a unit test binds to an INJECTED fonts directory,
    /// and without an injection to a path that does not exist — never to whatever font
    /// bundle happens to sit next to the developer's checkout. Dozens of panel tests
    /// construct a panel; their results and their runtime must not depend on that bundle.
    #[test]
    fn the_panel_constructor_binds_to_the_injected_fonts_dir() {
        // Default: not the real `fonts/`, and not a directory at all.
        create_state::set_test_fonts_dir(None);
        let default_dir = TypingCreatePanelState::new(false).fonts_dir;
        assert!(
            !default_dir.is_dir(),
            "the default test fonts dir must not exist: {}",
            default_dir.display()
        );
        assert_ne!(
            default_dir,
            fonts::resolve_fonts_dir(),
            "a unit test must never bind to the checkout's own fonts directory"
        );

        // Injected: exactly what the test asked for.
        let dir = unique_temp_dir("panel_ctor_injected_dir");
        fs::create_dir_all(&dir).expect("create temp fonts dir");
        create_state::set_test_fonts_dir(Some(dir.clone()));
        assert_eq!(TypingCreatePanelState::new(false).fonts_dir, dir);
        create_state::set_test_fonts_dir(None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// THE RACE. The one-shot legacy-preset migration must run against the AUTHORITATIVE
    /// (combined) font list, never against whatever the panel happens to hold at the moment
    /// the preset reader finishes.
    ///
    /// Both jobs are started at construction and run concurrently: reading `presets.json`
    /// (one small file, plus the legacy `user_config` payload) easily beats scanning,
    /// reading and hashing a whole font directory. Migrating on arrival therefore resolved
    /// no IMPORTED system font — those references were kept verbatim, the legacy
    /// `user_config` key was deleted, and nothing ever retried — while `fonts_data.json`
    /// knew the very same font by identity. The two stores disagreed forever.
    #[test]
    fn the_legacy_preset_migration_waits_for_the_authoritative_font_list() {
        let mut state = TypingCreatePanelState::new(false);
        state.preview_enabled = true;
        assert!(
            state
                .fonts
                .iter()
                .all(|font| font.bundled_stack_font().is_some()),
            "a fresh panel holds no user font: its list is still being built off-thread"
        );

        // The preset reader wins the race, carrying a preset whose primary font is an
        // IMPORTED system font — a font only the combined list can contain.
        let legacy = vec![(
            "Импортированный".to_string(),
            legacy_preset("/home/u/.fonts/Крик.ttf", "Крик", Vec::new()),
        )];
        state
            .preset_store_tx
            .send(PresetStoreEvent::Seeded {
                presets: HashMap::new(),
                legacy: Some(legacy),
            })
            .expect("the receiving end lives in the panel");
        state.poll_preset_store_events();
        assert!(
            state.presets_by_name.is_empty(),
            "nothing may be migrated against a list that cannot see the imported fonts"
        );

        // Now the combined list lands, WITH the imported font in it.
        deliver_font_reload(
            &mut state,
            fonts_with_identities(vec![raw_font_ps(
                "/home/u/.fonts/Крик.ttf",
                None,
                5,
                "Krik Family",
                "Krik-Regular",
            )]),
        );

        assert_eq!(
            state
                .presets_by_name
                .get("Импортированный")
                .map(|preset| preset.font.as_str()),
            Some("Krik-Regular"),
            "the parked migration must resolve the imported font once the authoritative \
             list is in, instead of freezing the legacy path string"
        );
    }

    /// The migration re-keys the user's REAL preset shape — four presets, absolute file
    /// paths as profile keys, some of them outside the project — onto font identities,
    /// collapses the three primary references into one, converts each profile body to the
    /// current schema, and keeps every unresolvable reference VERBATIM.
    #[test]
    fn the_legacy_preset_migration_rekeys_the_real_user_shape() {
        let state = state_with_migration_fonts();
        // Paths outside the project: the fonts they name are not installed here.
        let outside = "/home/u/Рабочий стол/MangaFucker/fonts/Дёрганный.ttf";
        let system_font = "/home/u/.fonts/Roboto-Medium.ttf";
        let legacy = vec![
            (
                "ВВД".to_string(),
                legacy_preset(
                    "/proj/fonts/groups/ВВД/Крик.ttf",
                    "Крик",
                    vec![
                        (
                            "/proj/fonts/ANTQUAB.ttf",
                            legacy_profile_body("ANTQUAB", "/proj/fonts/ANTQUAB.ttf", 33.0),
                        ),
                        (outside, legacy_profile_body("Дёрганный", outside, 44.0)),
                    ],
                ),
            ),
            (
                "Рао-кун".to_string(),
                legacy_preset(
                    "/proj/fonts/groups/Империя/Разговор.otf",
                    "Разговор",
                    vec![(
                        "/proj/fonts/простой V2.ttf",
                        legacy_profile_body("простой V2", "/proj/fonts/простой V2.ttf", 12.0),
                    )],
                ),
            ),
            (
                "Стандартный".to_string(),
                legacy_preset(
                    "/proj/fonts/простой V2.ttf",
                    // The real data stores the FAMILY name here, not the file stem.
                    "Arimo",
                    vec![(
                        system_font,
                        legacy_profile_body("Roboto-Medium", system_font, 20.0),
                    )],
                ),
            ),
            (
                "звук".to_string(),
                legacy_preset(
                    "/home/u/Рабочий стол/MangaFucker/fonts/звук.otf",
                    "звук",
                    vec![],
                ),
            ),
        ];

        let migrated: HashMap<String, TypingCreatePreset> =
            state.migrate_legacy_presets(legacy).into_iter().collect();
        assert_eq!(migrated.len(), 4, "every preset survives the migration");

        // (a) The three primary references collapse into ONE identity, resolved by NAME.
        let vvd = migrated.get("ВВД").expect("preset ВВД");
        assert_eq!(vvd.font, "Krik-Regular", "resolved through the stored label");
        assert_eq!(
            migrated.get("Стандартный").map(|p| p.font.as_str()),
            Some("Arimo-Regular"),
            "a stored FAMILY name is a name form too"
        );
        // (b) A preset whose font is not installed keeps its legacy spelling verbatim, so
        // the clue survives and `apply_preset_by_name` can report it as missing.
        assert_eq!(
            migrated.get("звук").map(|p| p.font.as_str()),
            Some("/home/u/Рабочий стол/MangaFucker/fonts/звук.otf")
        );
        // (c) Profile keys become identities; an unresolvable key is kept verbatim.
        assert!(
            vvd.font_profiles.contains_key("BookAntiqua-Bold"),
            "an in-project path key is re-keyed to the font's identity"
        );
        assert!(
            vvd.font_profiles.contains_key(outside),
            "an out-of-project key resolves to nothing and is kept verbatim, never dropped"
        );
        assert_eq!(vvd.font_profiles.len(), 2, "no key is lost or invented");
        // (d) The profile BODY of a resolvable font is converted to the current schema and
        // stops carrying any path; an unresolvable one is left completely alone.
        let converted = vvd
            .font_profiles
            .get("BookAntiqua-Bold")
            .and_then(|body| body.pointer("/text_params"))
            .and_then(Value::as_object)
            .expect("converted text_params");
        assert_eq!(
            converted.get("schema").and_then(Value::as_u64),
            Some(u64::from(text_params_schema::TEXT_PARAMS_SCHEMA_VERSION))
        );
        assert_eq!(
            converted.get("font").and_then(Value::as_str),
            Some("BookAntiqua-Bold")
        );
        for legacy_key in ["font_path", "font_label", "kerning_px", "kerning_percent"] {
            assert!(
                !converted.contains_key(legacy_key),
                "'{legacy_key}' must not survive the conversion"
            );
        }
        let untouched = vvd
            .font_profiles
            .get(outside)
            .and_then(|body| body.pointer("/text_params"))
            .and_then(Value::as_object)
            .expect("unconverted text_params");
        assert!(
            !untouched.contains_key("schema") && untouched.contains_key("font_path"),
            "a body whose font is not installed keeps every legacy key — it is the only \
             surviving record of the font it was set in"
        );
    }

    /// Saving a preset stores ONLY the fonts this session actually touched. The old code
    /// copied the current font's profile into every other loaded font's key, which is what
    /// turned 67 real profiles into 162 stored ones and made a preset claim parameters for
    /// fonts it was never configured for.
    #[test]
    fn saving_a_preset_does_not_copy_the_current_profile_into_every_font() {
        let mut state = state_with_migration_fonts();
        state.preview_enabled = true;
        state.selected_font_idx = 0;
        state.font_size_px = 51.0;
        state.preset_name_input = "P".to_string();
        state.save_current_preset();

        let saved = state.presets_by_name.get("P").expect("the preset is stored");
        assert_eq!(saved.font, "Krik-Regular");
        assert_eq!(
            saved.font_profiles.keys().collect::<Vec<_>>(),
            vec!["Krik-Regular"],
            "only the font whose parameters are on screen may be captured"
        );
    }

    /// Unique temp directory for a preset-store test. Never the real `fonts/` and never the
    /// real `user_config.json`.
    fn unique_preset_dir(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ms_panel_presets_{tag}_{nanos}"));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// A background save that REALLY fails reaches the panel's status line: the failure is
    /// produced by an actual `run_presets_save` against an unwritable location and picked up
    /// by the same per-frame poll the GUI calls. The previous version of this test only
    /// exercised the reporting helper, so a broken writer→channel→poll chain would have
    /// passed it.
    #[test]
    fn a_failed_background_save_is_shown_in_the_panel_status_line() {
        let dir = unique_preset_dir("save_failure");
        // A regular FILE where the fonts directory must be: every write below fails.
        let blocked = dir.join("blocked");
        fs::write(&blocked, "not a directory").expect("seed blocker file");

        let mut state = TypingCreatePanelState::new(false);
        state.preview_enabled = true;
        state.status_line = "всё хорошо".to_string();
        let mut presets = HashMap::new();
        presets.insert("P".to_string(), preset_named("Alpha-Regular", HashMap::new()));

        create_presets::run_presets_save(
            &blocked,
            &presets,
            presets_store::next_save_ticket(),
            // No config cleanup: a failed save must not reach it, and a test must never
            // touch the real `user_config.json`.
            None,
            &state.preset_store_tx,
        );
        state.poll_preset_store_events();

        assert_ne!(
            state.status_line, "всё хорошо",
            "a failed save must replace the status line"
        );
        assert!(
            state.status_line.contains(&blocked.display().to_string()),
            "the message must name the location that failed: {}",
            state.status_line
        );
        assert!(
            !presets_store::data_path(&blocked).is_file(),
            "nothing may have been written"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The legacy `user_config` keys are deleted ONLY after a save that actually succeeded
    /// (and, inside `presets_store::save`, only after the new document and its directory
    /// entry are durable — `doc_store`'s own journal test pins that half).
    #[test]
    fn the_legacy_config_keys_are_dropped_only_after_a_successful_save() {
        let dir = unique_preset_dir("cleanup_order");
        let config = dir.join("user_config.json");
        let seed = json!({
            "TextTab": { "create_presets": {"A": {}}, "formula_presets": {} }
        });
        fs::write(&config, seed.to_string()).expect("seed config");

        let state = TypingCreatePanelState::new(false);
        let mut presets = HashMap::new();
        presets.insert("P".to_string(), preset_named("Alpha-Regular", HashMap::new()));

        // 1. A save that FAILS leaves the legacy keys exactly where they are.
        let blocked = dir.join("blocked");
        fs::write(&blocked, "not a directory").expect("seed blocker file");
        create_presets::run_presets_save(
            &blocked,
            &presets,
            presets_store::next_save_ticket(),
            Some(&config),
            &state.preset_store_tx,
        );
        let after_failure: Value =
            serde_json::from_str(&fs::read_to_string(&config).expect("read config"))
                .expect("valid JSON");
        assert!(
            after_failure.pointer("/TextTab/create_presets").is_some(),
            "a failed save must not delete the presets it failed to replace"
        );

        // 2. A save that SUCCEEDS writes the document durably and only then cleans up.
        create_presets::run_presets_save(
            &dir,
            &presets,
            presets_store::next_save_ticket(),
            Some(&config),
            &state.preset_store_tx,
        );
        assert_eq!(
            doc_store::recorded_steps(&presets_store::data_path(&dir)),
            vec![
                doc_store::WriteStep::Renamed,
                doc_store::WriteStep::DirectoryDurable
            ],
            "presets.json must be durable before the legacy source is deleted"
        );
        let after_success: Value =
            serde_json::from_str(&fs::read_to_string(&config).expect("read config"))
                .expect("valid JSON");
        assert!(
            after_success.pointer("/TextTab/create_presets").is_none(),
            "the migrated key goes only once the new document is safely on disk"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The preset document is read OFF the GUI thread: the worker body produces a typed
    /// event, and the panel only installs it. A preset the user saved while the read was in
    /// flight wins over its stored namesake.
    #[test]
    fn presets_are_read_off_the_gui_thread_and_installed_by_the_poll() {
        let dir = unique_preset_dir("off_thread_seed");
        let mut stored = HashMap::new();
        stored.insert("Сохранённый".to_string(), preset_named("Beta-Regular", HashMap::new()));
        stored.insert("С диска".to_string(), preset_named("Gamma-Regular", HashMap::new()));
        presets_store::save(&dir, &stored, presets_store::next_save_ticket()).expect("seed file");

        let mut state = TypingCreatePanelState::new(false);
        state.preview_enabled = true;
        // The user saved a preset of the same name before the read landed.
        state.presets_by_name.insert(
            "Сохранённый".to_string(),
            preset_named("Alpha-Regular", HashMap::new()),
        );

        // The worker body — this is what runs on the reader thread.
        let (event, clean_config_now) = create_presets::read_presets_seed(&dir);
        assert!(
            clean_config_now,
            "an existing document means the legacy keys may be retried right away"
        );
        state
            .preset_store_tx
            .send(event)
            .expect("the panel owns the receiver");
        state.poll_preset_store_events();

        assert_eq!(
            state.presets_by_name.get("Сохранённый").map(|p| p.font.as_str()),
            Some("Alpha-Regular"),
            "what the user just saved wins over the stored namesake"
        );
        assert_eq!(
            state.presets_by_name.get("С диска").map(|p| p.font.as_str()),
            Some("Gamma-Regular"),
            "everything else from the document is installed"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A MIGRATED preset whose name is already taken is kept under a distinguishing name
    /// instead of being dropped: both are the user's data, and neither may vanish silently.
    #[test]
    fn a_migrated_preset_never_overwrites_a_saved_namesake() {
        let mut state = state_with_migration_fonts();
        state.preview_enabled = true;
        state.presets_by_name.insert(
            "ВВД".to_string(),
            preset_named("Krik-Regular", HashMap::new()),
        );

        let legacy = vec![(
            "ВВД".to_string(),
            presets_store::LegacyCreatePreset {
                primary_font_key: "/proj/fonts/простой V2.ttf".to_string(),
                ..presets_store::LegacyCreatePreset::default()
            },
        )];
        state.finish_legacy_presets_migration(legacy);

        assert_eq!(
            state.presets_by_name.get("ВВД").map(|p| p.font.as_str()),
            Some("Krik-Regular"),
            "the preset the user has on screen keeps its name"
        );
        assert_eq!(
            state.presets_by_name.get("ВВД (2)").map(|p| p.font.as_str()),
            Some("Arimo-Regular"),
            "the migrated namesake is kept under a free name, not dropped"
        );
    }

    /// Presets another running app instance wrote and this one merged in are adopted by the
    /// panel, so its next snapshot cannot drop them again.
    #[test]
    fn presets_merged_from_another_instance_are_adopted_by_the_panel() {
        let mut state = TypingCreatePanelState::new(false);
        state.preview_enabled = true;
        state.presets_by_name.insert(
            "Наш".to_string(),
            preset_named("Alpha-Regular", HashMap::new()),
        );
        let mut merged = HashMap::new();
        merged.insert(
            "Чужой".to_string(),
            preset_named("Beta-Regular", HashMap::new()),
        );
        merged.insert(
            "Наш".to_string(),
            preset_named("Gamma-Regular", HashMap::new()),
        );

        state
            .preset_store_tx
            .send(PresetStoreEvent::MergedFromDisk(merged))
            .expect("the panel owns the receiver");
        state.poll_preset_store_events();

        assert_eq!(
            state.presets_by_name.get("Чужой").map(|p| p.font.as_str()),
            Some("Beta-Regular")
        );
        assert_eq!(
            state.presets_by_name.get("Наш").map(|p| p.font.as_str()),
            Some("Alpha-Regular"),
            "ours is what is on screen and wins the name clash"
        );
    }

    /// VARIANT A. A parameter edit made while a PRESET is applied belongs to that preset: it
    /// must not be written into the font's persisted DEFAULT profile, or preset A's
    /// parameters silently become what every fresh, preset-less panel opens that font with.
    #[test]
    fn an_edit_under_an_applied_preset_never_rewrites_the_font_default() {
        let mut state = state_with_migration_fonts();
        state.preview_enabled = true;
        state.selected_font_idx = 0;
        state.active_font_identity = state.current_font_identity();

        // Control: without a preset, an edit DOES update the font's default profile.
        let _ = take_persisted_default_writes();
        state.font_size_px = 31.0;
        state.sync_current_font_profile_memory();
        assert_eq!(
            take_persisted_default_writes(),
            vec!["Krik-Regular".to_string()],
            "outside a preset the font must remember the parameters on screen"
        );

        let mut profiles = HashMap::new();
        profiles.insert("Krik-Regular".to_string(), profile_with_font_size(80.0));
        state
            .presets_by_name
            .insert("A".to_string(), preset_named("Krik-Regular", profiles));
        state.apply_preset_by_name("A".to_string());
        let _ = take_persisted_default_writes();

        // The edit the user makes after applying the preset.
        state.font_size_px = 99.0;
        state.sync_current_font_profile_memory();

        assert!(
            take_persisted_default_writes().is_empty(),
            "an edit under an applied preset must not touch the font's persisted default"
        );
        assert_eq!(
            state
                .font_profiles_by_identity
                .get("Krik-Regular")
                .and_then(|profile| profile.pointer("/text_params/font_size_px"))
                .and_then(Value::as_f64),
            Some(99.0),
            "the preset's own working set still records the edit"
        );
    }

    /// A document naming a CONTESTED font (two files, one PostScript name, different
    /// bytes) must select the very font the renderer resolves — the gap phase 1 left
    /// open, when the provider knew the `%hash` and bare-name forms and the panel did not.
    #[test]
    fn a_document_naming_a_contested_font_selects_the_same_font_as_the_renderer() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = fonts_with_identities(vec![
            raw_font_ps(
                "/fonts/groups/A/dup.ttf",
                Some("A"),
                0x1100_0000_0000_0000,
                "Fam",
                "FamPS",
            ),
            raw_font_ps(
                "/fonts/groups/B/dup.ttf",
                Some("B"),
                0x2200_0000_0000_0000,
                "Fam",
                "FamPS",
            ),
        ]);
        let provider = TabFontProvider::from_fonts(&state.fonts);

        // Each claimant is selectable by its own suffixed identity.
        assert_eq!(state.find_font_idx_by_identity("FamPS%1100000000000000"), Some(0));
        assert_eq!(state.find_font_idx_by_identity("FamPS%2200000000000000"), Some(1));
        // The BARE name — what a document written before the contest carries — selects
        // the lowest-hash claimant, exactly like the renderer.
        assert_eq!(state.find_font_idx_by_name_forms("FamPS"), Some(0));
        state.select_font_by_legacy_reference(None, &["FamPS"]);
        assert_eq!(state.selected_font_idx, 0);
        assert!(
            state.missing_font.is_none(),
            "a contested name must not read as a missing font in the panel"
        );

        // Panel and provider agree on EVERY form, which is the actual contract.
        for name in [
            "FamPS",
            "FamPS%1100000000000000",
            "FamPS%2200000000000000",
            "Fam",
            "dup",
            "famps%2200000000000000",
        ] {
            let panel_path = state
                .find_font_idx_by_name_forms(name)
                .map(|idx| state.fonts[idx].path.clone());
            assert_eq!(
                panel_path.as_deref(),
                provider.resolved_path_for(name),
                "panel and provider must resolve '{name}' to the same font"
            );
        }
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

    /// The `C` hotkey path (`toggle_clean_overlays_visible`) must behave exactly like the
    /// «Показывать клин» checkbox: flip the panel state AND queue a canvas request each time.
    #[test]
    fn toggle_clean_overlays_visible_flips_state_and_queues_request() {
        let mut state = TypingTopPanelState::default();
        // Seed from the canvas once, as the tab does before the hotkey is handled.
        state.sync_clean_overlays_visible_from_canvas(true);

        state.toggle_clean_overlays_visible();
        assert_eq!(state.take_clean_overlays_visible_request(), Some(false));
        // The request is drained once per frame; the panel state itself keeps the new value.
        assert_eq!(state.take_clean_overlays_visible_request(), None);

        state.toggle_clean_overlays_visible();
        assert_eq!(state.take_clean_overlays_visible_request(), Some(true));
    }

    /// The one-shot canvas seed must not clobber a value the `C` hotkey (or the checkbox) already
    /// set: after the first sync the panel is the source of truth.
    #[test]
    fn clean_overlays_canvas_sync_does_not_override_a_toggle() {
        let mut state = TypingTopPanelState::default();
        state.sync_clean_overlays_visible_from_canvas(true);
        state.toggle_clean_overlays_visible();

        state.sync_clean_overlays_visible_from_canvas(true);
        state.toggle_clean_overlays_visible();
        assert_eq!(state.take_clean_overlays_visible_request(), Some(true));
    }

    /// The `H` hotkey path (`toggle_centering_assist`) must flip the same flag the checkbox does,
    /// in both directions.
    #[test]
    fn toggle_centering_assist_flips_the_panel_flag() {
        let mut state = TypingTopPanelState::default();
        assert!(!state.centering_assist_enabled());

        state.toggle_centering_assist();
        assert!(state.centering_assist_enabled());

        state.toggle_centering_assist();
        assert!(!state.centering_assist_enabled());
    }

    /// Turning centering assist ON must re-emit the edit request only while a layer is being
    /// edited (that is the checkbox's side effect); turning it OFF never emits one, and with no
    /// edit target there is nothing to re-render.
    #[test]
    fn toggle_centering_assist_emits_edit_request_only_when_editing() {
        let mut state = TypingTopPanelState::default();
        state.sync_selected_overlay_for_edit(Some(text_overlay_for_edit(0)));
        assert_eq!(state.mode, TypingTopPanelMode::EditText);
        // Drain whatever the selection sync itself queued so the assertions below are unambiguous.
        let _ = state.take_edit_request();

        state.toggle_centering_assist();
        assert!(
            state.take_edit_request().is_some(),
            "turning the assist on while editing must re-render for the mean/median centers"
        );

        state.toggle_centering_assist();
        assert!(state.take_edit_request().is_none());
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

    /// Defect 2: an imported system font whose file is missing or unusable must still produce
    /// a ROW — it is the only thing that can carry the "remove" action for a document entry
    /// that nothing else prunes. It must NOT produce a usable font entry.
    ///
    /// Every hint here fails, so the loader falls through to the by-name lookup and touches
    /// the PROCESS-GLOBAL system-font index; the store test lock is what keeps that from
    /// racing the tests that assert on the index build count.
    #[test]
    fn unusable_imported_system_fonts_stay_visible_as_unavailable_rows() {
        let _lock = super::font_settings_store::test_lock();
        test_reset_system_font_index();
        let dir = unique_temp_dir("skip");
        fs::create_dir_all(&dir).expect("create temp dir");
        // A file that exists but is not a valid font, and a path that does not exist at all.
        let garbage = dir.join("not_a_font.ttf");
        fs::write(&garbage, b"this is not a font").expect("write garbage file");
        let missing = dir.join("does_not_exist.ttf");

        let refs = vec![
            fonts_data::SystemFontRef {
                font: "Garbage-Regular".to_string(),
                last_path: Some(garbage.clone()),
            },
            fonts_data::SystemFontRef {
                font: "Missing-Regular".to_string(),
                last_path: Some(missing),
            },
            fonts_data::SystemFontRef {
                font: "NoHint-Regular".to_string(),
                last_path: None,
            },
        ];
        let rows = load_imported_system_font_rows(&refs);
        assert_eq!(rows.len(), 3, "every stored entry must produce a row");
        assert!(
            rows.iter().all(|row| row.entry.is_none()),
            "none of these files may become a usable font entry"
        );
        assert_eq!(
            rows[0].unavailable,
            Some(ImportedFontUnavailable::Unparsable)
        );
        assert!(matches!(
            rows[1].unavailable,
            Some(ImportedFontUnavailable::Unreadable(_))
        ));
        assert_eq!(
            rows[2].unavailable,
            Some(ImportedFontUnavailable::NoPathHint)
        );
        // The stored identity survives on every row: it is the key the remove button uses.
        assert_eq!(
            rows.iter()
                .map(|row| row.stored_identity.as_str())
                .collect::<Vec<_>>(),
            vec!["Garbage-Regular", "Missing-Regular", "NoHint-Regular"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The duplicate merge walks CLUSTERS and must not be able to panic on an index while
    /// doing so; an empty input is the degenerate case of that walk.
    #[test]
    fn merging_an_empty_font_list_yields_an_empty_list() {
        assert!(merge_duplicate_fonts(Vec::new()).is_empty());
    }

    /// An imported system font whose byte-identical twin is folded into a FOLDER entry must
    /// not leave a numbering gap behind: the surviving `"{stem} [system]"` label used to
    /// keep the ` (2)` it was given before the fold, with no `(1)`/unsuffixed sibling in the
    /// list at all.
    #[test]
    fn imported_system_font_labels_are_renumbered_after_the_cross_source_merge() {
        let Some(fixture) = committed_font_fixture() else {
            return;
        };
        let other = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fonts/ui/ext/10-NotoSansMath-Regular.ttf");
        if !other.is_file() {
            return;
        }
        let bytes = fs::read(&fixture).expect("the committed fixture must be readable");
        let other_bytes = fs::read(&other).expect("the second fixture must be readable");

        let root = unique_temp_dir("renumber");
        let fonts_dir = root.join("fonts");
        let imported_a = root.join("a");
        let imported_b = root.join("b");
        for dir in [&fonts_dir, &imported_a, &imported_b] {
            fs::create_dir_all(dir).expect("create temp dir");
        }
        // The folder copy and the FIRST imported copy are byte-identical, so the imported
        // one folds away; the second imported file shares its STEM but not its bytes.
        fs::write(fonts_dir.join("Folder.ttf"), &bytes).expect("write folder font");
        let first = imported_a.join("Shared.ttf");
        let second = imported_b.join("Shared.ttf");
        fs::write(&first, &bytes).expect("write the first imported copy");
        fs::write(&second, &other_bytes).expect("write the second imported copy");

        let fonts = load_fonts(&fonts_dir, &[first.clone(), second.clone()]);
        let survivor = fonts
            .iter()
            .find(|font| font.path == second)
            .expect("the non-duplicate imported font stays in the list");
        assert_eq!(
            survivor.label, "Shared [system]",
            "the only surviving copy must not carry a duplicate suffix"
        );
        assert!(
            !fonts.iter().any(|font| font.path == first),
            "the byte-identical imported copy folds into the folder entry"
        );
        let _ = fs::remove_dir_all(&root);
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

        // Schema 2 takes ONE font key: the identity (phase 3 of
        // `dev-docs/font_identity_postscript_plan.md`); path/label/family are gone.
        let render_data =
            state.build_render_data_json_with_font("Hi".to_string(), 100, Some("Test".to_string()));
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

        // `thicken` is SIGNED: a negative value is a THINNING request, so it must
        // survive the read path instead of being clamped away at zero, and only an
        // out-of-range magnitude is clamped — to the renderer's own lower bound.
        let thinning = |percent: f64| {
            let render_data = serde_json::json!({
                "text_params": {
                    "text": "Hi",
                    "force_bold": true,
                    "faux_bold": true,
                    "faux_bold_thicken_percent": percent,
                },
                "effects": [],
            });
            let mut state = TypingCreatePanelState::new(false);
            state.apply_render_data_json_with_options(&render_data, false);
            state.faux_bold_thicken_percent
        };
        assert_eq!(thinning(-99.0), -5.0, "-99 clamps to the renderer's minimum");
        assert_eq!(thinning(-3.0), -3.0, "a legitimate thinning survives verbatim");
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
            // `both` — the counter token follows `FauxBoldParams::default()`, whose
            // `outward_only` is the uniform-weight `false`.
            "<m b=8.00,sharp,both,0.00>"
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

    /// Char range (Unicode scalar offsets) of the first occurrence of `needle`
    /// in `haystack`, for building a realistic inline text selection in tests.
    /// The `expect` documents the setup invariant: the caller always embeds
    /// `needle` in `haystack` just above the call.
    fn char_range_of(haystack: &str, needle: &str) -> Range<usize> {
        let byte_start = haystack
            .find(needle)
            .expect("test setup: needle must be embedded in haystack");
        let char_start = haystack[..byte_start].chars().count();
        char_start..char_start + needle.chars().count()
    }

    // Bug fix (font-label collision + selected group): an ambiguous label must
    // resolve to the IN-GROUP copy, and staying on that copy must emit NO font
    // token (so merely selecting text can't insert a `<font>` tag).
    #[test]
    fn ambiguous_label_resolves_to_in_group_font_and_emits_no_token() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = merge_duplicate_fonts(vec![
            // "Разговор" exists both inside group "A" and globally (distinct content
            // → two separate entries sharing the label), plus a global-only font.
            raw_font("/fonts/groups/A/Разговор.ttf", Some("A"), 1),
            raw_font("/fonts/Разговор.ttf", None, 2),
            raw_font("/fonts/Уникальный.ttf", None, 3),
        ]);
        // Invariant: the fixture above builds exactly one group-A "Разговор" and
        // one global "Уникальный", so both lookups below are guaranteed to hit.
        let in_group_idx = state
            .fonts
            .iter()
            .position(|f| font_in_group(f, "A"))
            .expect("fixture defines a group-A Разговор");
        let unique_idx = state
            .fonts
            .iter()
            .position(|f| f.label == "Уникальный")
            .expect("fixture defines a global Уникальный");

        // Group "A" active → filtered indices contain only the in-group copy.
        state.selected_font_group = Some("A".to_string());
        let filtered = state.filtered_font_indices();
        assert_eq!(filtered, vec![in_group_idx]);

        // The ambiguous label resolves to the in-group copy, not the global twin.
        assert_eq!(
            state.find_font_idx_by_label_preferring_indices(Some("Разговор"), &filtered),
            Some(in_group_idx),
        );
        // A label with no in-group match falls back to the global lookup.
        assert_eq!(
            state.find_font_idx_by_label_preferring_indices(Some("Уникальный"), &filtered),
            Some(unique_idx),
        );

        // With the base font resolved to the in-group copy, an unchanged span
        // carrying that same label emits no font token (nothing to write back).
        state.selected_font_idx = in_group_idx;
        let selection = selection_with_style(TypingInlineTagStyle {
            font_label: Some("Разговор".to_string()),
            ..TypingInlineTagStyle::default()
        });
        let effective = state.effective_inline_tag_style(&selection);
        let normalized = state.normalize_desired_inline_tag_style(effective);
        assert_eq!(normalized.font_label, None);
        assert!(build_inline_machine_tag(&normalized).is_empty());
    }

    // Item 6 (edit panel): a legacy `<font=Comic>` tag with an active virtual group
    // whose member is an imported ~/Comic.ttf, while a folder Comic.ttf also exists,
    // must resolve group-preferring to the IN-GROUP (imported) copy — the same
    // discipline the create panel already follows. Merely selecting text must never
    // rewrite the span to the folder twin; only an explicit user pick may (guarded by
    // the edge-triggered `font_combo_user_pick`, tested separately).
    #[test]
    fn edit_panel_font_label_resolves_group_preferring() {
        let mut state = TypingCreatePanelState::new(false);
        // Two distinct "Comic" files: an imported system font inside group "A" and a
        // folder copy at the root (distinct content → two entries sharing the stem).
        state.fonts = merge_duplicate_fonts(vec![
            raw_font("/home/user/Comic.ttf", Some("A"), 1),
            raw_font("/fonts/Comic.ttf", None, 2),
        ]);
        let in_group_idx = state
            .fonts
            .iter()
            .position(|f| font_in_group(f, "A"))
            .expect("fixture defines a group-A Comic");

        // Group "A" active → the filtered list holds only the in-group copy.
        state.selected_font_group = Some("A".to_string());
        let filtered = state.filtered_font_indices();
        assert_eq!(filtered, vec![in_group_idx]);

        // The ambiguous legacy label resolves to the in-group copy, not the folder twin.
        assert_eq!(
            state.find_font_idx_by_label_preferring_indices(Some("Comic"), &filtered),
            Some(in_group_idx),
        );
        // And staying on that copy emits no font token (nothing to write back), so a
        // mere selection cannot rewrite the span to the folder font.
        state.selected_font_idx = in_group_idx;
        let selection = selection_with_style(TypingInlineTagStyle {
            font_label: Some("Comic".to_string()),
            ..TypingInlineTagStyle::default()
        });
        let effective = state.effective_inline_tag_style(&selection);
        let normalized = state.normalize_desired_inline_tag_style(effective);
        assert_eq!(normalized.font_label, None);
    }

    // Edge-trigger contract (Sol finding 3): the pure decision that gates the
    // inline font-label writeback. Only a popup click or a wheel step that MOVED
    // the index counts; a bare per-frame resolve (no input) never does.
    #[test]
    fn font_combo_user_pick_is_edge_triggered() {
        // No input this frame → nothing is written.
        assert_eq!(create_main_text::font_combo_user_pick(None, None), None);
        // A wheel step that changes the index is a pick.
        assert_eq!(create_main_text::font_combo_user_pick(None, Some((0, 2))), Some(2));
        // A wheel event that does not move the index is NOT a pick.
        assert_eq!(create_main_text::font_combo_user_pick(None, Some((1, 1))), None);
        // A popup click counts even on the already-highlighted row.
        assert_eq!(create_main_text::font_combo_user_pick(Some(3), None), Some(3));
        // A popup click takes priority over a same-frame no-op wheel.
        assert_eq!(create_main_text::font_combo_user_pick(Some(1), Some((0, 0))), Some(1));
        // Every subsequent no-input frame keeps returning None (no re-write).
        assert_eq!(create_main_text::font_combo_user_pick(None, None), None);
        assert_eq!(create_main_text::font_combo_user_pick(None, None), None);
    }

    // `selected_text_contains_inline_tag` detects tags strictly INSIDE the range
    // and ignores an out-of-range / non-boundary range without panicking.
    #[test]
    fn selected_text_contains_inline_tag_detects_internal_tags() {
        let text = "a<m f=\"X\">b</m>c";
        // Range covering only "a" (before the tag) → no internal tag.
        assert!(!selected_text_contains_inline_tag(text, &(0..1)));
        // Range covering the whole string → the internal tag is found.
        assert!(selected_text_contains_inline_tag(text, &(0..text.len())));
        // Plain text never reports a tag.
        assert!(!selected_text_contains_inline_tag("abc", &(0..3)));
        // Out-of-range slice is treated as "no tag" (never panics).
        assert!(!selected_text_contains_inline_tag("abc", &(0..99)));
    }

    // Idempotency fast path: a plain uniform selection with nothing to apply is a
    // no-op, built through the REAL selection-context path (no hand-forged state).
    #[test]
    fn apply_inline_style_noop_on_plain_uniform_selection() {
        let mut state = state_with_font();
        state.text = "abc".to_string();
        state.text_selection_char_range = Some(char_range_of(&state.text, "abc"));
        let selection = state
            .inline_selection_context()
            .expect("a non-empty selection over 'abc' yields a context");
        // No wrapper, no internal tags → the effective style is empty.
        assert!(selection.opening_wrapper_range.is_empty());
        assert!(selection.closing_wrapper_range.is_empty());
        let desired = state.effective_inline_tag_style(&selection);
        let changed = state.apply_inline_style_to_selection(selection, desired);
        assert!(!changed, "an empty style on a plain selection is a no-op");
        assert_eq!(state.text, "abc", "no tag may be inserted");
    }

    // Regression (Sol finding 1): the conservative fast path must NOT suppress a
    // legitimate rewrite. A redundant `<m f=Base>` wrapper (font == base) has an
    // empty NORMALIZED style, yet re-applying must STRIP it — the earlier
    // style-equality guard wrongly early-returned here and left the wrapper.
    #[test]
    fn redundant_adjacent_font_wrapper_is_stripped_not_suppressed() {
        let mut state = state_with_font(); // one font "Test", selected_font_idx = 0
        let base = state
            .font_identity_name_by_idx(0)
            .expect("the single fixture font has a render identity");
        let open = build_inline_machine_tag(&TypingInlineTagStyle {
            font_label: Some(base),
            ..TypingInlineTagStyle::default()
        });
        state.text = format!("{open}abc</m>");
        state.text_selection_char_range = Some(char_range_of(&state.text, "abc"));
        let selection = state
            .inline_selection_context()
            .expect("selection inside the wrapper yields a context");
        // The redundant wrapper IS detected as adjacent to the selection, so the
        // conservative fast path is skipped and the real rewrite runs.
        assert!(!selection.opening_wrapper_range.is_empty());
        let desired = state.effective_inline_tag_style(&selection);
        let changed = state.apply_inline_style_to_selection(selection, desired);
        assert!(changed, "a redundant font wrapper must be stripped, not suppressed");
        assert_eq!(state.text, "abc", "the redundant <m f=…> wrapper is removed");
    }

    // Regression (Sol finding 2): a selection spanning an INTERNAL tag re-applied
    // with no change must be a no-op that leaves the internal tag intact — the
    // guard must neither strip nor duplicate it.
    #[test]
    fn internal_tag_selection_is_left_intact_on_reapply() {
        let mut state = state_with_font();
        let other = build_inline_machine_tag(&TypingInlineTagStyle {
            font_label: Some("Other".to_string()),
            ..TypingInlineTagStyle::default()
        });
        state.text = format!("a{other}b</m>c");
        let text_before = state.text.clone();
        state.text_selection_char_range = Some(0..state.text.chars().count());
        let selection = state
            .inline_selection_context()
            .expect("whole-text selection yields a context");
        assert!(selected_text_contains_inline_tag(
            &state.text,
            &selection.text_byte_range
        ));
        let desired = state.effective_inline_tag_style(&selection);
        let changed = state.apply_inline_style_to_selection(selection, desired);
        assert!(!changed, "re-applying with no change is a no-op");
        assert_eq!(state.text, text_before, "the internal tag is left intact");
    }

    // Unique-name case: picking a DIFFERENT font applies the change exactly once,
    // then the next frame (no new pick) is a no-op — no per-frame tag growth.
    #[test]
    fn distinct_font_pick_applies_once_then_is_idempotent() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = merge_duplicate_fonts(vec![
            raw_font("/fonts/Alpha.ttf", None, 1),
            raw_font("/fonts/Beta.ttf", None, 2),
        ]);
        // Invariant: the fixture builds exactly these two uniquely-named fonts.
        let alpha = state
            .fonts
            .iter()
            .position(|f| f.label == "Alpha")
            .expect("fixture defines Alpha");
        let beta = state
            .fonts
            .iter()
            .position(|f| f.label == "Beta")
            .expect("fixture defines Beta");
        state.selected_font_idx = alpha; // base font is Alpha
        state.text = "abc".to_string();
        state.text_selection_char_range = Some(char_range_of(&state.text, "abc"));

        // Frame 1: the edge-triggered writeback sets the span font label to Beta.
        let selection = state
            .inline_selection_context()
            .expect("plain selection over 'abc' yields a context");
        let mut desired = state.effective_inline_tag_style(&selection);
        desired.font_label = state.font_identity_name_by_idx(beta);
        let changed = state.apply_inline_style_to_selection(selection, desired);
        assert!(changed, "picking a different font inserts a font tag once");
        assert!(
            state.text.contains("Beta"),
            "text must carry the Beta font span, got: {}",
            state.text
        );

        // The frame loop moves the restored selection into the active selection.
        state.text_selection_char_range = state.pending_text_selection_restore.take();

        // Frame 2: no new pick → re-applying the effective style is a no-op.
        let selection2 = state
            .inline_selection_context()
            .expect("restored selection yields a context");
        let effective2 = state.effective_inline_tag_style(&selection2);
        let text_before = state.text.clone();
        let changed2 = state.apply_inline_style_to_selection(selection2, effective2);
        assert!(!changed2, "re-applying without a new pick must not duplicate the tag");
        assert_eq!(state.text, text_before, "text unchanged on the second frame");
    }

    // --- character-table insertion (`create_edit::insert_text_at_caret`) -------
    //
    // The insertion point is the stored `text_selection_char_range` in the ACTIVE
    // buffer; a non-empty range is REPLACED, a collapsed one is the caret, and no
    // range at all appends. The caret must land right AFTER the inserted text and
    // be published through `pending_text_selection_restore` for the next frame's
    // `sync_text_selection_from_text_edit`.

    #[test]
    fn insert_at_caret_handles_start_middle_and_end() {
        for (caret, expected) in [(0usize, "→abc"), (1, "a→bc"), (3, "abc→")] {
            let mut state = state_with_font();
            state.text = "abc".to_string();
            state.text_selection_char_range = Some(caret..caret);
            assert!(state.insert_text_at_caret("→"), "a real insertion changed the buffer");
            assert_eq!(state.text, expected, "caret {caret}");
            assert_eq!(
                state.pending_text_selection_restore,
                Some(caret + 1..caret + 1),
                "the caret lands after the inserted text"
            );
            assert_eq!(
                state.text_selection_char_range,
                Some(caret + 1..caret + 1),
                "the live caret is advanced too, so a second insertion follows the first"
            );
        }
    }

    #[test]
    fn insert_at_caret_replaces_a_non_empty_selection() {
        let mut state = state_with_font();
        state.text = "abcdef".to_string();
        state.text_selection_char_range = Some(char_range_of(&state.text, "bcd"));
        assert!(state.insert_text_at_caret("★"));
        assert_eq!(state.text, "a★ef", "the selection is replaced, not wrapped");
        assert_eq!(state.pending_text_selection_restore, Some(2..2));
    }

    #[test]
    fn insert_at_caret_without_a_recorded_caret_appends() {
        let mut state = state_with_font();
        state.text = "abc".to_string();
        state.text_selection_char_range = None;
        assert!(state.insert_text_at_caret("→"));
        assert_eq!(state.text, "abc→", "nothing focused yet => append at the end");
        assert_eq!(state.pending_text_selection_restore, Some(4..4));
    }

    #[test]
    fn insert_at_caret_into_an_empty_buffer_and_multichar_insert() {
        let mut state = state_with_font();
        state.text = String::new();
        state.text_selection_char_range = None;
        // A tagged insertion is what the character table emits for a font that
        // differs from the base one; the caret must count CHARACTERS, not bytes.
        let tagged = "<font=Beta>→</font>";
        assert!(state.insert_text_at_caret(tagged));
        assert_eq!(state.text, tagged);
        let chars = tagged.chars().count();
        assert_eq!(state.pending_text_selection_restore, Some(chars..chars));
        // An empty insertion is a no-op and must not move the caret.
        let before = state.text.clone();
        assert!(!state.insert_text_at_caret(""));
        assert_eq!(state.text, before);
    }

    #[test]
    fn insert_at_caret_targets_the_formed_buffer_when_it_is_active() {
        let mut state = state_with_font();
        state.text = "source".to_string();
        state.formed_text = "formed".to_string();
        state.inline_text_target = InlineTextTarget::Formed;
        state.text_selection_char_range = Some(0..0);
        assert!(state.insert_text_at_caret("→"));
        assert_eq!(state.formed_text, "→formed");
        assert_eq!(state.text, "source", "the inactive buffer is untouched");
    }

    #[test]
    fn insert_at_caret_clamps_a_stale_out_of_range_caret() {
        // A caret recorded against a longer buffer (the layer was switched) must
        // clamp to the end instead of panicking or losing the insertion.
        let mut state = state_with_font();
        state.text = "ab".to_string();
        state.text_selection_char_range = Some(99..99);
        assert!(state.insert_text_at_caret("→"));
        assert_eq!(state.text, "ab→");
        assert_eq!(state.pending_text_selection_restore, Some(3..3));
    }

    #[test]
    fn load_fonts_with_no_imported_paths_matches_dir_only_loading() {
        // On an empty fonts dir, `load_fonts` with no imported paths must invent no user
        // font — the imported-paths merge is purely additive. The ONLY entry it may add
        // is the synthetic built-in one, which `load_fonts` prepends for the PANEL list
        // (the folder-only pass also feeds the settings font-administration list and
        // therefore must not get it).
        //
        // `load_fonts` merges the PROCESS-GLOBAL store's imported system fonts on top of the
        // directory scan, so this test has to hold the store lock: a sibling test seeding an
        // imported font would otherwise be observed here as a font "invented" for an empty dir.
        let _lock = super::font_settings_store::test_lock();
        let dir = unique_temp_dir("empty");
        fs::create_dir_all(&dir).expect("create temp dir");
        let via_load_fonts = load_fonts(&dir, &[]);
        let via_dir = folder_font_entries(&dir);
        assert!(via_dir.is_empty(), "an empty fonts dir holds no user fonts");
        assert!(
            via_load_fonts
                .iter()
                .all(|font| font.bundled_stack_font().is_some()),
            "no user font may be invented for an empty fonts dir"
        );
        assert!(
            via_load_fonts.len() <= 1,
            "at most the single built-in entry may be added"
        );
        assert!(
            via_load_fonts
                .first()
                .is_none_or(|font| font.bundled_stack_font().is_some()),
            "when present, the built-in entry heads the list"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A folder font and an IMPORTED system font that are byte-identical copies of the
    /// same font must become ONE list entry.
    ///
    /// They carry one identity (same PostScript name, same bytes, so nothing is
    /// contested), and `TabFontProvider` is first-wins on the identity key: as two
    /// entries, one of them was silently unreachable through the renderer while still
    /// occupying a row in the font combo. The folder merge alone cannot catch this — it
    /// only sees files under the fonts dir — so the fold happens on the COMBINED list.
    #[test]
    fn a_folder_font_and_a_byte_identical_imported_copy_are_one_entry() {
        let Some(fixture) = committed_font_fixture() else {
            eprintln!("шрифт-фикстура репозитория недоступна, пропускаем тест");
            return;
        };
        let dir = unique_temp_dir("combined_merge");
        let system_dir = unique_temp_dir("combined_merge_system");
        let group_dir = dir.join("groups").join("Диалоги");
        fs::create_dir_all(&group_dir).expect("create temp dir");
        fs::create_dir_all(&system_dir).expect("create temp dir");
        // Same bytes, different file names, different places: one font.
        let folder_copy = group_dir.join("Разговор.ttf");
        let imported_copy = system_dir.join("noto-sans-copy.ttf");
        fs::copy(&fixture, &folder_copy).expect("copy fixture");
        fs::copy(&fixture, &imported_copy).expect("copy fixture");

        let entries = load_fonts(&dir, std::slice::from_ref(&imported_copy));
        let user: Vec<&FontEntry> = entries
            .iter()
            .filter(|font| font.bundled_stack_font().is_none())
            .collect();
        assert_eq!(
            user.len(),
            1,
            "a folder font and a byte-identical imported copy are ONE font"
        );
        let font = user[0];
        assert_eq!(
            font.label, "Разговор",
            "the folder entry stays the representative, so the '[system]' label of the \
             folded copy disappears"
        );
        assert!(
            font.alt_paths.contains(&imported_copy),
            "the imported copy keeps its path on the merged entry"
        );
        assert_eq!(
            font.groups,
            vec![Some("Диалоги".to_string())],
            "an imported file has no folder group, so its placeholder root membership \
             must NOT be unioned into the folder font's groups"
        );
        assert!(
            !font.render_identity_name().contains('%'),
            "identical bytes are not a contest, so the identity stays unsuffixed: {}",
            font.render_identity_name()
        );

        // The renderer's view agrees: one identity, one file, and both copies still
        // resolve through the panel's legacy path door.
        let provider = TabFontProvider::from_fonts(&entries);
        assert_eq!(
            provider.resolved_path_for(&font.render_identity_name()),
            Some(folder_copy.as_path()),
            "the identity resolves to the folder copy"
        );
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = entries;
        let user_idx = state
            .fonts
            .iter()
            .position(|entry| entry.bundled_stack_font().is_none())
            .expect("the user font is in the list");
        for path in [&folder_copy, &imported_copy] {
            assert_eq!(
                state.find_font_idx_by_legacy_reference(Some(&path.to_string_lossy()), None),
                Some(user_idx),
                "a legacy reference to {} must resolve to the merged entry",
                path.display()
            );
        }

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&system_dir);
    }

    // ---- Single-pass font-file parsing (`fonts::read_font_file`) ----------------------

    /// A font file that ships WITH the repository, so these tests do not depend on the
    /// machine's installed fonts. `CARGO_MANIFEST_DIR` (not the process CWD) anchors it,
    /// so the fixture is found no matter where the test binary is run from.
    fn committed_font_fixture() -> Option<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fonts")
            .join("ui")
            .join("core")
            .join("00-NotoSans-Regular.ttf");
        path.is_file().then_some(path)
    }

    /// Per-read parse counts recorded for exactly this path, in call order: one element
    /// per `read_font_file` call, holding how many `fontdb` databases that call built.
    ///
    /// The list LENGTH pins "one read per file" and each ELEMENT pins "one parse per
    /// read" — the second half is what a plain call counter cannot see, and what the
    /// phase-0 regression (two throwaway databases inside one read) would break.
    /// Filtering by path is what makes the counts immune to other tests parsing their own
    /// fonts in parallel.
    fn font_file_parses(path: &Path) -> Vec<usize> {
        let journal = match FONT_FILE_PARSE_JOURNAL.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        journal
            .iter()
            .filter(|(recorded, _)| recorded.as_path() == path)
            .map(|(_, parses)| *parses)
            .collect()
    }

    /// Assembles a VALID TrueType collection (`.ttc`) holding `faces` faces out of the
    /// bytes of a single-face `.ttf`, so the "every face of a collection carries its
    /// PostScript name" contract can be pinned WITHOUT shipping a binary fixture and
    /// WITHOUT depending on the machine having a `.ttc` installed (the previous test
    /// silently skipped on such machines, i.e. possibly never ran at all).
    ///
    /// Layout: a `ttcf` header, then one table directory per face, then the whole source
    /// file appended verbatim. All directories are copies of the source's own directory
    /// with every table offset shifted by where the source file was appended, so all
    /// faces SHARE one set of tables — which is legal, is what real collections do for
    /// common tables, and gives every face the same (non-empty) PostScript name.
    /// Checksums are not recalculated; `ttf_parser` does not verify them.
    ///
    /// Returns `None` when `ttf` is not a parsable single-file sfnt (truncated header or
    /// a truncated table record), so a broken fixture fails the test loudly instead of
    /// being silently skipped.
    fn synthesize_font_collection(ttf: &[u8], faces: usize) -> Option<Vec<u8>> {
        /// Big-endian `u16` at `offset`, or `None` when the slice is too short.
        fn be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
            let raw: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
            Some(u16::from_be_bytes(raw))
        }
        /// Big-endian `u32` at `offset`, or `None` when the slice is too short.
        fn be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
            let raw: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
            Some(u32::from_be_bytes(raw))
        }

        if faces == 0 {
            return None;
        }
        let num_tables = usize::from(be_u16(ttf, 4)?);
        let directory_len = 12 + 16 * num_tables;
        ttf.get(..directory_len)?;

        // TTC header: tag + major/minor version + numFonts + one offset per face.
        let header_len = 12 + 4 * faces;
        // Every directory is the same size; the source file follows them, 4-byte aligned
        // because sfnt table offsets must stay 4-byte aligned after the shift.
        let unaligned_base = header_len + directory_len * faces;
        let source_base = unaligned_base.next_multiple_of(4);

        let mut out = Vec::with_capacity(source_base + ttf.len());
        out.extend_from_slice(b"ttcf");
        out.extend_from_slice(&1u16.to_be_bytes()); // majorVersion
        out.extend_from_slice(&0u16.to_be_bytes()); // minorVersion
        out.extend_from_slice(&u32::try_from(faces).ok()?.to_be_bytes());
        for index in 0..faces {
            let offset = header_len + directory_len * index;
            out.extend_from_slice(&u32::try_from(offset).ok()?.to_be_bytes());
        }

        // One table directory per face: the source's sfnt header verbatim, then its table
        // records with the offsets rebased onto the appended copy of the file.
        let shift = u32::try_from(source_base).ok()?;
        for _ in 0..faces {
            out.extend_from_slice(ttf.get(..12)?);
            for table in 0..num_tables {
                let record = 12 + 16 * table;
                out.extend_from_slice(ttf.get(record..record + 8)?); // tag + checksum
                let offset = be_u32(ttf, record + 8)?.checked_add(shift)?;
                out.extend_from_slice(&offset.to_be_bytes());
                out.extend_from_slice(ttf.get(record + 12..record + 16)?); // length
            }
        }
        out.resize(source_base, 0);
        out.extend_from_slice(ttf);
        Some(out)
    }

    /// Every parsed face carries its own PostScript name, and the entry-level name is
    /// the representative (first) face's — so nothing has to recover it by splitting the
    /// decorated face label.
    #[test]
    fn read_font_file_captures_the_post_script_name_of_every_face() {
        let Some(path) = committed_font_fixture() else {
            eprintln!("шрифт-фикстура репозитория недоступна, пропускаем тест");
            return;
        };
        let data = read_font_file(&path).expect("committed fixture must be readable");
        assert!(data.parsed, "the committed fixture must parse");
        assert!(!data.faces.is_empty(), "a parsed file has at least one face");
        for (idx, face) in data.faces.iter().enumerate() {
            assert_eq!(face.face_index, idx, "face_index is the position in the file");
            assert!(
                !face.post_script_name.is_empty(),
                "face #{idx} must carry its PostScript name"
            );
            assert!(
                face.label.trim_end().ends_with(&face.post_script_name),
                "the display label is derived from the same name, got {}",
                face.label
            );
        }
        assert_eq!(
            data.post_script_name(),
            data.faces[0].post_script_name,
            "the file-level PostScript name is the representative face's"
        );
        // The same name reaches the finalized entry (via the merge), not just the face.
        let entry_ps = merge_duplicate_fonts(vec![RawFontFile {
            path: path.clone(),
            stem: "fixture".to_string(),
            group: None,
            content_hash: data.content_hash,
            faces: data.faces.clone(),
            coverage: data.coverage.clone(),
            original_name: data.original_name.clone(),
        }])
        .first()
        .map(|entry| entry.post_script_name().to_string())
        .expect("one raw font yields one entry");
        assert_eq!(entry_ps, data.post_script_name());
    }

    /// A `.ttc` collection: EVERY face of the file gets its own PostScript name from the
    /// single parse, and the collection is read and parsed exactly once.
    ///
    /// The collection is BUILT in the test from the committed single-face fixture
    /// (`synthesize_font_collection`). The previous version of this test looked for an
    /// installed `.ttc` and skipped when the machine had none, so the multi-face half of
    /// the phase-0 contract could go entirely unverified; a synthesized fixture makes it
    /// deterministic without adding a binary file to the repository.
    #[test]
    fn read_font_file_captures_a_post_script_name_for_every_face_of_a_collection() {
        let Some(fixture) = committed_font_fixture() else {
            eprintln!("шрифт-фикстура репозитория недоступна, пропускаем тест");
            return;
        };
        let single_face = fs::read(&fixture).expect("the committed fixture must be readable");
        let collection = synthesize_font_collection(&single_face, 2)
            .expect("the committed fixture must be a parsable sfnt");

        let dir = unique_temp_dir("collection");
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("synthetic.ttc");
        fs::write(&path, &collection).expect("write the synthesized collection");

        let data = read_font_file(&path).expect("the synthesized .ttc must be readable");
        assert!(
            data.parsed,
            "the synthesized .ttc must parse; the fixture builder is part of the contract"
        );
        assert_eq!(
            data.faces.len(),
            2,
            "the single pass must see every face of the collection"
        );
        for (idx, face) in data.faces.iter().enumerate() {
            assert_eq!(face.face_index, idx, "face_index is the position in the file");
            assert!(
                !face.post_script_name.is_empty(),
                "face #{idx} of the collection must carry its PostScript name"
            );
        }
        assert_eq!(
            data.post_script_name(),
            data.faces[0].post_script_name,
            "the file-level name is the representative (first) face's"
        );
        assert_eq!(
            font_file_parses(&path),
            vec![1],
            "a collection is read once and parsed once, however many faces it holds"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A file that cannot be parsed is still LISTED (the user put it in the fonts
    /// folder) as a placeholder entry with no invented names.
    #[test]
    fn the_folder_pass_keeps_an_unparsable_file_as_a_placeholder_entry() {
        let dir = unique_temp_dir("unparsable");
        fs::create_dir_all(&dir).expect("create temp dir");
        let garbage = dir.join("broken.ttf");
        fs::write(&garbage, b"this is not a font").expect("write garbage file");

        let entries = folder_font_entries(&dir);
        assert_eq!(entries.len(), 1, "the unparsable file still yields an entry");
        let entry = &entries[0];
        assert_eq!(entry.label, "broken");
        assert_eq!(
            entry.original_name, "broken",
            "no family name to read: the file stem stands in"
        );
        assert!(
            entry.post_script_name().is_empty(),
            "no face parsed: no PostScript name may be invented"
        );
        assert_eq!(entry.faces.len(), 1, "exactly the placeholder face");
        assert!(entry.faces[0].post_script_name.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Phase-0 contract: building a font list reads and parses each font FILE exactly
    /// once — including the imported-system-font path, which used to probe the file with
    /// one throwaway database and then parse it twice more.
    #[test]
    fn each_font_file_is_parsed_exactly_once_per_load() {
        let Some(fixture) = committed_font_fixture() else {
            eprintln!("шрифт-фикстура репозитория недоступна, пропускаем тест");
            return;
        };
        let dir = unique_temp_dir("single_parse");
        fs::create_dir_all(&dir).expect("create temp dir");
        let alpha = dir.join("alpha.ttf");
        let beta = dir.join("beta.ttf");
        let broken = dir.join("broken.ttf");
        fs::copy(&fixture, &alpha).expect("copy fixture");
        fs::copy(&fixture, &beta).expect("copy fixture");
        fs::write(&broken, b"this is not a font").expect("write garbage file");

        let entries = folder_font_entries(&dir);
        // Two byte-identical copies under DIFFERENT stems ARE one font (the merge key is
        // PostScript name + content hash), so they fold into a single entry; the
        // unparsable file — which claims no PostScript name — is listed separately.
        assert_eq!(entries.len(), 2, "three files, two distinct fonts");
        let merged = entries
            .iter()
            .find(|entry| !entry.alt_paths.is_empty())
            .expect("the byte-identical copies must have merged");
        assert_eq!(merged.alt_paths, vec![beta.clone()], "the second copy folds in as an alt path");
        // Each file is still read exactly once (one journal entry) AND parsed exactly
        // once inside that read (the recorded `fontdb` parse count), merge or no merge.
        // The parse count is what catches a regression that re-introduces a second
        // throwaway database inside the same read — invisible to a call counter.
        for path in [&alpha, &beta, &broken] {
            assert_eq!(
                font_file_parses(path),
                vec![1],
                "{} must be read once and parsed once within that read",
                path.display()
            );
        }

        // The imported-fonts loader parses its file once too (probe + faces + name used
        // to be three passes over the same bytes).
        let imported: Vec<FontEntry> =
            load_imported_system_font_rows(&[fonts_data::SystemFontRef {
                font: String::new(),
                last_path: Some(alpha.clone()),
            }])
            .into_iter()
            .filter_map(|row| row.entry)
            .collect();
        assert_eq!(imported.len(), 1, "a parsable imported path yields one entry");
        assert!(
            !imported[0].post_script_name().is_empty(),
            "the imported entry carries the PostScript name of its representative face"
        );
        assert_eq!(
            font_file_parses(&alpha),
            vec![1, 1],
            "the imported load adds exactly one more read of the same file, itself a \
             single parse"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- Virtual font group injection (`fonts::apply_virtual_groups`) -----------------

    /// Finalizes a raw-font list exactly like `fonts::folder_font_entries` does up to the point
    /// `apply_virtual_groups` expects: merge duplicates, assign disambiguators, assign the
    /// collision-aware identity. (No display-name overrides — irrelevant to these tests.)
    fn finalize_fonts(raws: Vec<RawFontFile>) -> Vec<FontEntry> {
        let mut fonts = merge_duplicate_fonts(raws);
        assign_font_disambiguators(&mut fonts);
        assign_font_identity_names(&mut fonts);
        fonts
    }

    /// Convenience constructor for a virtual group with the given members.
    fn vgroup(name: &str, members: Vec<fonts_data::VirtualFontGroupMember>) -> fonts_data::VirtualFontGroup {
        fonts_data::VirtualFontGroup {
            name: name.to_string(),
            members,
        }
    }

    /// Convenience constructor for a virtual-group member (font IDENTITY + optional alias).
    fn vmember(font: &str, alias: Option<&str>) -> fonts_data::VirtualFontGroupMember {
        fonts_data::VirtualFontGroupMember {
            font: font.to_string(),
            alias: alias.map(ToOwned::to_owned),
        }
    }

    // Members are stored (and matched) by font IDENTITY since phase 4. `raw_font` fixtures
    // carry no PostScript name, so their identity falls back to the family-or-stem rule —
    // "Comic" for `/fonts/Comic.ttf`.

    #[test]
    fn apply_virtual_groups_matches_by_identity_and_attaches_alias() {
        let mut fonts = finalize_fonts(vec![raw_font("/fonts/Comic.ttf", None, 1)]);
        let virtual_groups = vec![vgroup(
            "Экшн",
            vec![vmember("Comic", Some("Экшн-жирный"))],
        )];
        let merged = apply_virtual_groups(&mut fonts, &[], &virtual_groups);

        assert_eq!(merged, vec!["Экшн".to_string()], "merged list gains the virtual group");
        let font = &fonts[0];
        assert!(font_in_group(font, "Экшн"), "the font is now a member of the virtual group");
        assert_eq!(
            font.display_label_in_group(Some("Экшн")),
            "Экшн-жирный",
            "the per-group alias is shown while the group is active"
        );
        assert_eq!(
            font.display_label_in_group(None),
            "Comic",
            "with no active group the plain display label is shown"
        );
    }

    // ---- Per-font profile memory (`FontProfileMemory`) -------------------------------

    /// The SESSION layer answers first and the persisted layer is not consulted at all when
    /// it does — a preset's in-memory override must never be shadowed by a stored default.
    #[test]
    fn profile_memory_session_value_wins_over_the_persisted_default() {
        let mut memory = FontProfileMemory::from_map(HashMap::from([(
            "Comic-Regular".to_string(),
            profile_with_font_size(11.0),
        )]));
        let mut loads = 0;
        let found = memory
            .get_with("Comic-Regular", |_| {
                loads += 1;
                Some(profile_with_font_size(99.0))
            })
            .cloned();
        assert_eq!(
            found
                .as_ref()
                .and_then(|v| v.pointer("/text_params/font_size_px"))
                .and_then(Value::as_f64),
            Some(11.0)
        );
        assert_eq!(loads, 0, "a session hit must not touch the persisted store");
    }

    /// A session MISS falls back to the font's persisted default and CACHES it, so the next
    /// lookup (and a preset saved afterwards) sees the font the user actually configured.
    #[test]
    fn profile_memory_falls_back_to_the_persisted_default_and_caches_it() {
        let mut memory = FontProfileMemory::default();
        let mut loads = 0;
        assert!(
            memory
                .get_with("Comic-Regular", |_| {
                    loads += 1;
                    Some(profile_with_font_size(42.0))
                })
                .is_some()
        );
        assert_eq!(loads, 1);
        assert!(memory.contains_key("Comic-Regular"), "the default is cached in session");
        // A second lookup is served from the session map.
        memory.get_with("Comic-Regular", |_| {
            loads += 1;
            None
        });
        assert_eq!(loads, 1, "the cached value must not be re-loaded");
        assert_eq!(
            memory
                .to_map()
                .get("Comic-Regular")
                .and_then(|v| v.pointer("/text_params/font_size_px"))
                .and_then(Value::as_f64),
            Some(42.0)
        );
    }

    /// A font with no memory anywhere stays unknown, and nothing is cached for it.
    #[test]
    fn profile_memory_reports_nothing_for_an_unknown_font() {
        let mut memory = FontProfileMemory::default();
        assert!(memory.get_with("Ghost-Regular", |_| None).is_none());
        assert!(!memory.contains_key("Ghost-Regular"));
        assert_eq!(memory.stored_count(), 0);
    }

    /// A store writes BOTH layers: the session map (what a preset would capture) and the
    /// font's persisted default (what the next session restores).
    #[test]
    fn profile_memory_insert_writes_through_to_the_persisted_default() {
        let mut memory = FontProfileMemory::default();
        let mut saved: Option<(String, f64)> = None;
        memory.insert_with(
            "Comic-Regular".to_string(),
            profile_with_font_size(7.0),
            |identity, profile| {
                saved = Some((
                    identity.to_string(),
                    profile
                        .pointer("/text_params/font_size_px")
                        .and_then(Value::as_f64)
                        .unwrap_or_default(),
                ));
            },
        );
        assert_eq!(saved, Some(("Comic-Regular".to_string(), 7.0)));
        assert!(memory.contains_key("Comic-Regular"));
    }

    /// A user rename (display-name override) is keyed by IDENTITY, so renaming or moving the
    /// FONT FILE must not lose it. Before phase 4 the key was the file path and the override
    /// silently vanished with the old name.
    ///
    /// Drives the real loader against a temp fonts dir, and holds the store test lock because
    /// the font-settings store is process-global.
    #[test]
    fn display_name_override_survives_a_font_file_rename() {
        let _lock = super::font_settings_store::test_lock();
        super::font_settings_store::test_reset();

        let dir = unique_temp_dir("override_rename");
        fs::create_dir_all(&dir).expect("create temp fonts dir");
        let fixture = advanced_form_fixture_font_path();
        let original = dir.join("Original.ttf");
        fs::copy(&fixture, &original).expect("copy fixture");

        let before = fonts::build_combined_font_list(&dir, &[]).entries;
        assert_eq!(before.len(), 1, "the temp fonts dir holds exactly one font");
        let identity = before[0].render_identity_name();
        assert!(before[0].display_name.is_none(), "no override yet");

        assert!(super::font_settings_store::set_font_display_name_override(
            &identity,
            Some("Мой шрифт".to_string())
        ));
        let named = fonts::build_combined_font_list(&dir, &[]).entries;
        assert_eq!(named[0].display_name.as_deref(), Some("Мой шрифт"));

        // Rename the FILE. The face — and therefore the identity — is untouched.
        let renamed = dir.join("Совсем другое имя.ttf");
        fs::rename(&original, &renamed).expect("rename the font file");
        let after = fonts::build_combined_font_list(&dir, &[]).entries;
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].render_identity_name(),
            identity,
            "the identity is a property of the FACE, not of the file name"
        );
        assert_eq!(
            after[0].display_name.as_deref(),
            Some("Мой шрифт"),
            "the display-name override must follow the font across a file rename"
        );

        super::font_settings_store::test_reset();
        let _ = fs::remove_dir_all(&dir);
    }

    /// DEFECT 1, the scenario in full. A group member points at `groups/ВВД/Основа.ttf`, whose
    /// real font is `NotoSans-Regular`, and the file happens to be unreadable during THIS
    /// launch. The loader still lists it (placeholder), and its fallback "identity" is the file
    /// stem `Основа`.
    ///
    /// That guess must not be allowed to resolve the legacy key: re-keying the member to
    /// `Основа` and declaring the migration finished detached the member from its font
    /// FOREVER — the document became v2, so nothing ever retried. The placeholder must
    /// therefore contribute nothing, the reference must stay verbatim, and the migration must
    /// stay pending so the next run (where the file reads) finishes the job correctly.
    #[test]
    fn an_unreadable_font_file_cannot_resolve_a_legacy_reference() {
        let _lock = super::font_settings_store::test_lock();
        super::font_settings_store::test_reset();

        let dir = unique_temp_dir("placeholder_migration");
        let group_dir = dir.join("groups").join("ВВД");
        fs::create_dir_all(&group_dir).expect("create temp fonts dir");
        let member_file = group_dir.join("Основа.ttf");
        // Unparsable this run: bytes that no font parser accepts.
        fs::write(&member_file, b"not a font at all").expect("write the broken font file");

        let legacy_key = "groups/ВВД/Основа.ttf";
        super::font_settings_store::test_seed(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts: std::collections::BTreeMap::from([(
                legacy_key.to_string(),
                fonts_data::FontSettingsRecord {
                    display_name: Some("Основа".to_string()),
                    profile: None,
                },
            )]),
            virtual_groups: vec![fonts_data::VirtualFontGroup {
                name: "Возлюбленная".to_string(),
                members: vec![fonts_data::VirtualFontGroupMember {
                    font: legacy_key.to_string(),
                    alias: Some("Основа".to_string()),
                }],
            }],
            pending_migration: true,
        });

        let placeholder = fonts::build_combined_font_list(&dir, &[]).entries;
        assert_eq!(placeholder.len(), 1, "the unreadable file is still listed");
        assert_eq!(
            placeholder[0].render_identity_name(),
            "Основа",
            "its identity is the file-stem fallback — a guess, not the font's real name"
        );
        assert!(
            super::font_settings_store::migration_pending(),
            "an unresolved reference must keep the migration pending"
        );
        let groups = super::font_settings_store::virtual_groups();
        assert_eq!(
            groups[0].members[0].font, legacy_key,
            "the guessed identity must NOT claim the legacy reference"
        );
        assert_eq!(
            super::font_settings_store::font_display_name_override("Основа"),
            None,
            "nor may it claim the legacy per-font record"
        );

        // Next run: the file reads. The retry re-keys everything to the REAL identity.
        let fixture = advanced_form_fixture_font_path();
        fs::copy(&fixture, &member_file).expect("replace with a readable font");
        let readable = fonts::build_combined_font_list(&dir, &[]).entries;
        assert_eq!(readable.len(), 1);
        let identity = readable[0].render_identity_name();
        assert_ne!(identity, "Основа", "the real PostScript name is not the stem");
        assert!(
            !super::font_settings_store::migration_pending(),
            "everything resolved, so the migration is finally finished"
        );
        let groups = super::font_settings_store::virtual_groups();
        assert_eq!(
            groups[0].members[0].font, identity,
            "the retry attaches the member to the font it always meant"
        );
        assert_eq!(groups[0].members[0].alias.as_deref(), Some("Основа"));
        assert_eq!(
            super::font_settings_store::font_display_name_override(&identity).as_deref(),
            Some("Основа"),
            "the per-font record follows the same retry"
        );

        super::font_settings_store::test_reset();
        let _ = fs::remove_dir_all(&dir);
    }

    /// DEFECT 4. The settings font lists and the typing panel must speak the SAME identities.
    ///
    /// Two files claim one PostScript name with DIFFERENT bytes, one in the fonts folder and
    /// one imported — a contest, so the panel gives both a `%hash`-suffixed identity. Building
    /// the settings categories from two INDEPENDENT passes hid the contest from each of them:
    /// the folder-only pass saw a single uncontested claimant and showed the BARE name, so a
    /// group membership or a display-name override written there matched no panel entry and
    /// silently did nothing.
    #[test]
    fn the_settings_font_lists_carry_the_panel_identities_for_a_contested_name() {
        let _lock = super::font_settings_store::test_lock();
        super::font_settings_store::test_reset();

        let Some(fixture) = committed_font_fixture() else {
            eprintln!("шрифт-фикстура репозитория недоступна, пропускаем тест");
            return;
        };
        let bytes = fs::read(&fixture).expect("the committed fixture must be readable");
        let dir = unique_temp_dir("settings_identity");
        let system_dir = unique_temp_dir("settings_identity_system");
        fs::create_dir_all(&dir).expect("create temp fonts dir");
        fs::create_dir_all(&system_dir).expect("create temp system dir");
        let folder_copy = dir.join("Folder.ttf");
        let imported_copy = system_dir.join("Imported.ttf");
        fs::write(&folder_copy, &bytes).expect("write the folder copy");
        // Same tables (so the same PostScript name) but different bytes: trailing padding is
        // ignored by the parser and changes the content hash, which is exactly a "contest".
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0u8; 64]);
        fs::write(&imported_copy, &padded).expect("write the contesting copy");

        let post_script = {
            let folder_only = folder_font_entries(&dir);
            assert_eq!(folder_only.len(), 1);
            let bare = folder_only[0].render_identity_name();
            assert!(
                !bare.contains('%'),
                "the folder-ONLY list cannot see the contest, so it shows the bare name: {bare}"
            );
            bare
        };

        let refs = vec![fonts_data::SystemFontRef {
            font: post_script.clone(),
            last_path: Some(imported_copy.clone()),
        }];
        let admin = fonts::build_combined_font_list(&dir, &refs);
        let panel: Vec<FontEntry> = load_fonts(&dir, std::slice::from_ref(&imported_copy))
            .into_iter()
            .filter(|font| font.bundled_stack_font().is_none())
            .collect();

        let mut panel_identities: Vec<String> = panel
            .iter()
            .map(FontEntry::render_identity_name)
            .collect();
        let mut admin_identities: Vec<String> = admin
            .entries
            .iter()
            .map(FontEntry::render_identity_name)
            .collect();
        panel_identities.sort();
        admin_identities.sort();
        assert_eq!(
            admin_identities, panel_identities,
            "the settings list must carry exactly the identities the panel resolves"
        );
        assert_eq!(admin_identities.len(), 2, "the two files stay separate fonts");
        assert!(
            admin_identities.iter().all(|identity| identity.contains('%')),
            "a contested name suffixes BOTH claimants: {admin_identities:?}"
        );

        // The imported row keeps its DOCUMENT key (the unsuffixed name) so the remove button
        // still matches the store entry, while its font carries the suffixed panel identity.
        assert_eq!(admin.imported_rows.len(), 1);
        let row = &admin.imported_rows[0];
        assert_eq!(row.stored_identity, post_script);
        let row_font = row.entry.as_ref().expect("the imported file loaded");
        assert!(
            row_font.render_identity_name().contains('%'),
            "the row's font must carry the contested identity: {}",
            row_font.render_identity_name()
        );
        assert!(
            panel_identities.contains(&row_font.render_identity_name()),
            "and that identity must be one the panel actually has"
        );

        super::font_settings_store::test_reset();
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&system_dir);
    }

    /// The FOLDER-ONLY pass must never finalize the `fonts_data.json` v1 migration.
    ///
    /// A folder font and an IMPORTED system font declare one PostScript name with different
    /// bytes — a contest, so the authoritative (combined) list suffixes BOTH identities.
    /// The folder-only subset cannot see the imported file, so at that moment the folder
    /// font looks uncontested and its identity is the BARE name. Re-keying the legacy
    /// document there wrote that pre-collision identity: the combined pass then suffixed
    /// both claimants, while the migration — one-way, and with the bare name already
    /// counted as "already migrated" — never redid the re-key. The user's display-name
    /// override and virtual-group membership were left hanging on an identity that no final
    /// entry carries.
    #[test]
    fn the_folder_only_pass_never_finalizes_the_fonts_data_migration() {
        let _lock = super::font_settings_store::test_lock();
        super::font_settings_store::test_reset();

        let Some(fixture) = committed_font_fixture() else {
            eprintln!("шрифт-фикстура репозитория недоступна, пропускаем тест");
            return;
        };
        let bytes = fs::read(&fixture).expect("the committed fixture must be readable");
        let dir = unique_temp_dir("folder_only_migration");
        let system_dir = unique_temp_dir("folder_only_migration_system");
        fs::create_dir_all(&dir).expect("create temp fonts dir");
        fs::create_dir_all(&system_dir).expect("create temp system dir");
        let folder_copy = dir.join("Folder.ttf");
        let imported_copy = system_dir.join("Imported.ttf");
        fs::write(&folder_copy, &bytes).expect("write the folder copy");
        // Same tables (so the same declared PostScript name) but different bytes: trailing
        // padding is ignored by the parser and changes the content hash — a real contest.
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0u8; 64]);
        fs::write(&imported_copy, &padded).expect("write the contesting copy");

        // A v1 document keyed by the folder font's PATH: one per-font setting and one
        // virtual-group membership, both pointing at the same legacy key.
        let legacy_key = "Folder.ttf";
        super::font_settings_store::test_seed(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts: std::collections::BTreeMap::from([(
                legacy_key.to_string(),
                fonts_data::FontSettingsRecord {
                    display_name: Some("Основной".to_string()),
                    profile: None,
                },
            )]),
            virtual_groups: vec![fonts_data::VirtualFontGroup {
                name: "Возлюбленная".to_string(),
                members: vec![fonts_data::VirtualFontGroupMember {
                    font: legacy_key.to_string(),
                    alias: Some("Осн".to_string()),
                }],
            }],
            pending_migration: true,
        });

        // The folder-only pass runs and sees ONE uncontested claimant with a bare identity.
        let folder_only = folder_font_entries(&dir);
        assert_eq!(folder_only.len(), 1, "the temp fonts dir holds one file");
        let bare = folder_only[0].render_identity_name();
        assert!(
            !bare.contains('%'),
            "the folder-only list cannot see the contest, so it shows the bare name: {bare}"
        );
        assert!(
            super::font_settings_store::migration_pending(),
            "the folder-only pass must NOT finish the migration"
        );
        assert_eq!(
            super::font_settings_store::virtual_groups()[0].members[0].font,
            legacy_key,
            "nor may it re-key anything to the pre-collision identity"
        );
        assert_eq!(
            super::font_settings_store::font_display_name_override(&bare),
            None,
            "and the per-font record must not move to the bare name either"
        );

        // The AUTHORITATIVE pass sees both files, suffixes both identities, and only THEN
        // re-keys the document.
        let refs = vec![fonts_data::SystemFontRef {
            font: bare.clone(),
            last_path: Some(imported_copy.clone()),
        }];
        let combined = fonts::build_combined_font_list(&dir, &refs);
        let folder_entry = combined
            .entries
            .iter()
            .find(|entry| entry.path == folder_copy)
            .expect("the folder font is in the combined list");
        let suffixed = folder_entry.render_identity_name();
        assert!(
            suffixed.contains('%'),
            "a contested name suffixes the folder claimant too: {suffixed}"
        );
        assert!(
            !super::font_settings_store::migration_pending(),
            "everything resolved against the authoritative list, so the migration is done"
        );
        assert_eq!(
            super::font_settings_store::font_display_name_override(&suffixed).as_deref(),
            Some("Основной"),
            "the per-font setting must land on the identity the final list actually carries"
        );
        let groups = super::font_settings_store::virtual_groups();
        assert_eq!(
            groups[0].members[0].font, suffixed,
            "and so must the virtual-group membership"
        );
        assert_eq!(groups[0].members[0].alias.as_deref(), Some("Осн"));

        super::font_settings_store::test_reset();
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&system_dir);
    }

    /// The POINT of phase 4: membership follows the FONT, not the file. Renaming (or moving)
    /// the file leaves the PostScript name — and therefore the identity — untouched, so the
    /// stored member still resolves. With the old path key this membership was silently lost.
    #[test]
    fn apply_virtual_groups_membership_survives_a_font_file_rename() {
        let virtual_groups = vec![vgroup("Экшн", vec![vmember("Comic-Regular", Some("Крик"))])];

        // Same font, same PostScript name, DIFFERENT file name and folder.
        for path in ["/fonts/Comic.ttf", "/fonts/groups/X/Renamed-2024.ttf"] {
            let group = (path != "/fonts/Comic.ttf").then_some("X");
            let mut fonts =
                finalize_fonts(vec![raw_font_ps(path, group, 7, "Comic", "Comic-Regular")]);
            let _merged = apply_virtual_groups(&mut fonts, &[], &virtual_groups);
            assert!(
                font_in_group(&fonts[0], "Экшн"),
                "membership must survive the file living at {path}"
            );
            assert_eq!(fonts[0].display_label_in_group(Some("Экшн")), "Крик");
        }
    }

    /// Two byte-identical copies fold into ONE entry carrying ONE identity, so a single
    /// member reference covers the cluster — the alt-path scan the path-keyed era needed is
    /// gone. (Previously this test keyed the member by the folded copy's path.)
    #[test]
    fn apply_virtual_groups_merged_duplicate_matches_by_its_shared_identity() {
        let mut fonts = finalize_fonts(vec![
            raw_font_ps("/fonts/Comic.ttf", None, 7, "Comic", "Comic-Regular"),
            raw_font_ps("/fonts/groups/X/Comic.ttf", Some("X"), 7, "Comic", "Comic-Regular"),
        ]);
        assert_eq!(fonts.len(), 1, "the byte-identical copies merged into one entry");
        let virtual_groups = vec![vgroup("Диалоги", vec![vmember("Comic-Regular", None)])];
        let _merged = apply_virtual_groups(&mut fonts, &["X".to_string()], &virtual_groups);
        assert!(
            font_in_group(&fonts[0], "Диалоги"),
            "the merged entry matches under the identity both copies share"
        );
    }

    /// An identity is compared case-insensitively everywhere else, so membership must fold
    /// case too — a member stored before a font's own spelling changed must still resolve.
    #[test]
    fn apply_virtual_groups_matches_identity_case_insensitively() {
        let mut fonts = finalize_fonts(vec![raw_font_ps(
            "/fonts/Comic.ttf",
            None,
            7,
            "Comic",
            "Comic-Regular",
        )]);
        let virtual_groups = vec![vgroup("G", vec![vmember("comic-REGULAR", None)])];
        let _merged = apply_virtual_groups(&mut fonts, &[], &virtual_groups);
        assert!(font_in_group(&fonts[0], "G"));
    }

    #[test]
    fn apply_virtual_groups_skips_missing_member_but_keeps_group() {
        let mut fonts = finalize_fonts(vec![raw_font("/fonts/Comic.ttf", None, 1)]);
        let virtual_groups = vec![vgroup("Пусто", vec![vmember("Absent-Regular", Some("N/A"))])];
        let merged = apply_virtual_groups(&mut fonts, &[], &virtual_groups);
        // The group name survives (a virtual group may have zero loaded members) but the
        // unrelated font gains neither membership nor an alias.
        assert_eq!(merged, vec!["Пусто".to_string()]);
        assert!(!font_in_group(&fonts[0], "Пусто"));
        assert_eq!(fonts[0].display_label_in_group(Some("Пусто")), "Comic");
    }

    #[test]
    fn apply_virtual_groups_skips_collision_with_real_folder_group() {
        let mut fonts = finalize_fonts(vec![raw_font("/fonts/Comic.ttf", None, 1)]);
        // Virtual name "a" collides case-insensitively with the real folder group "A".
        let virtual_groups = vec![vgroup("a", vec![vmember("Comic", None)])];
        let merged = apply_virtual_groups(&mut fonts, &["A".to_string()], &virtual_groups);
        assert_eq!(merged, vec!["A".to_string()], "the colliding virtual group is excluded");
        assert!(
            !font_in_group(&fonts[0], "a"),
            "a skipped virtual group must not add membership"
        );
    }

    #[test]
    fn apply_virtual_groups_merged_list_is_case_insensitively_sorted() {
        let mut fonts = finalize_fonts(vec![raw_font("/fonts/Comic.ttf", None, 1)]);
        let virtual_groups = vec![vgroup("apple", vec![]), vgroup("Mango", vec![])];
        let merged = apply_virtual_groups(&mut fonts, &["Zebra".to_string()], &virtual_groups);
        assert_eq!(
            merged,
            vec!["apple".to_string(), "Mango".to_string(), "Zebra".to_string()],
            "real + virtual groups sort case-insensitively"
        );
    }

    #[test]
    fn apply_virtual_groups_font_in_multiple_virtual_groups() {
        let mut fonts = finalize_fonts(vec![raw_font("/fonts/Comic.ttf", None, 1)]);
        let virtual_groups = vec![
            vgroup("G1", vec![vmember("Comic", Some("Один"))]),
            vgroup("G2", vec![vmember("Comic", Some("Два"))]),
        ];
        let _merged = apply_virtual_groups(&mut fonts, &[], &virtual_groups);
        let font = &fonts[0];
        assert!(font_in_group(font, "G1") && font_in_group(font, "G2"));
        assert_eq!(font.display_label_in_group(Some("G1")), "Один");
        assert_eq!(font.display_label_in_group(Some("G2")), "Два");
    }

    #[test]
    fn apply_virtual_groups_does_not_disturb_disambiguators() {
        // Two distinct files sharing the stem "Comic" get bracketed disambiguators computed
        // from their REAL folder locations (root vs. group "A").
        let mut fonts = finalize_fonts(vec![
            raw_font("/fonts/Comic.ttf", None, 1),
            raw_font("/fonts/groups/A/Comic.ttf", Some("A"), 2),
        ]);
        let before: Vec<Option<String>> = fonts.iter().map(|f| f.disambig.clone()).collect();
        assert!(
            before.iter().all(Option::is_some),
            "the shared-stem fixture must produce disambiguators to test against"
        );
        // Add the ROOT copy to a virtual group; its folder-derived disambiguator must not move.
        // Both files fall back to the stem identity "Comic", so the first entry is the match.
        let root_identity = fonts[0].render_identity_name();
        let virtual_groups = vec![vgroup("V", vec![vmember(&root_identity, None)])];
        let _merged = apply_virtual_groups(&mut fonts, &["A".to_string()], &virtual_groups);
        let after: Vec<Option<String>> = fonts.iter().map(|f| f.disambig.clone()).collect();
        assert_eq!(before, after, "virtual membership must not change disambiguators");
    }

    #[test]
    fn display_label_in_group_prefers_alias_only_for_that_group() {
        let mut fonts = finalize_fonts(vec![raw_font("/fonts/Comic.ttf", None, 1)]);
        let virtual_groups = vec![vgroup("Aliased", vec![vmember("Comic", Some("Псевдо"))])];
        let _merged = apply_virtual_groups(&mut fonts, &[], &virtual_groups);
        let font = &fonts[0];
        // Alias only applies for the exact active group; any other/absent group -> plain label.
        assert_eq!(font.display_label_in_group(Some("Aliased")), "Псевдо");
        assert_eq!(font.display_label_in_group(Some("Other")), "Comic");
        assert_eq!(font.display_label_in_group(None), "Comic");
    }

    #[test]
    fn empty_virtual_group_selection_is_safe_and_leaves_index() {
        // An active virtual group with no loaded members must not corrupt the selection or
        // panic: `filtered_font_indices` is empty and `ensure_selected_font_in_group` leaves
        // `selected_font_idx` untouched.
        let mut state = TypingCreatePanelState::new(false);
        let mut fonts = finalize_fonts(vec![raw_font("/fonts/Comic.ttf", None, 1)]);
        let virtual_groups = vec![vgroup("Empty", vec![vmember("Absent-Regular", None)])];
        let merged = apply_virtual_groups(&mut fonts, &[], &virtual_groups);
        state.fonts = fonts;
        state.font_groups = merged;
        state.selected_font_group = Some("Empty".to_string());
        state.selected_font_idx = 0;

        assert!(
            state.filtered_font_indices().is_empty(),
            "an empty virtual group filters out every font"
        );
        state.ensure_selected_font_in_group();
        assert_eq!(
            state.selected_font_idx, 0,
            "an empty group must not move the selection to an invalid index"
        );
    }

    /// Repo fixture font used by the advanced-form width-metric tests: a single-face
    /// Regular file, so a real Bold/Italic face request has nothing to match.
    fn advanced_form_fixture_font_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
    }

    /// A create panel whose only font is the fixture file, selected face 0.
    fn advanced_form_fixture_panel() -> TypingCreatePanelState {
        let mut state = advanced_form_fixture_panel_without_metric_font();
        seed_advanced_form_metric_font(&mut state);
        state
    }

    /// Resolves the SELECTED font's bytes into the width-metric slot synchronously.
    ///
    /// The real panel does this on a worker thread (`poll_advanced_form_font`), because
    /// a provider cache miss is an `fs::read` and the form window is drawn on the GUI
    /// thread. A test may resolve inline; that is the only difference between the paths.
    fn seed_advanced_form_metric_font(state: &mut TypingCreatePanelState) {
        let identity = state.fonts[state.selected_font_idx].render_identity_name();
        let content = state.font_provider.resolve(&identity);
        assert!(
            content.is_some(),
            "the fixture font must resolve through the panel's own provider"
        );
        state.advanced_form_font = Some(AdvancedFormFont { identity, content });
    }

    /// The same fixture panel with NO width-metric font resolved yet — the state a freshly
    /// opened form window is in, before its background resolve lands.
    fn advanced_form_fixture_panel_without_metric_font() -> TypingCreatePanelState {
        let path = advanced_form_fixture_font_path();
        assert!(path.is_file(), "fixture font missing: {}", path.display());
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = finalize_fonts(vec![raw_font(path.to_string_lossy().as_ref(), None, 1)]);
        // The provider is built from the font list in `new`; the fixture replaced that
        // list, so it has to be rebuilt exactly as `poll_font_reload_results` does.
        state.font_provider = Arc::new(TabFontProvider::from_fonts(&state.fonts));
        state.selected_font_idx = 0;
        state.selected_face_idx = 0;
        state
    }

    /// The width-metric cache signature separates two entries that share a FILE but not an
    /// IDENTITY — the built-in interface entry (measured with the whole bundled `core`
    /// chain in its database) and a user import of that very `core[0]` file (measured with
    /// that file alone). Keying on the path made them collide, which is the only reason
    /// the deleted `bundled_ui_stack: bool` field existed.
    #[test]
    fn advanced_form_metric_signature_separates_the_built_in_entry_from_an_import_of_its_file() {
        let probe = TypingCreatePanelState::new(false);
        let Some(bundled_path) = probe
            .fonts
            .first()
            .filter(|font| font.bundled_stack_font().is_some())
            .map(|font| font.path.clone())
        else {
            eprintln!(
                "skipping advanced_form_metric_signature_separates_the_built_in_entry_from_an_import_of_its_file: \
                 no fonts/ui stack"
            );
            return;
        };
        let mut state = TypingCreatePanelState::new(false);
        let mut fonts = finalize_fonts(vec![raw_font_ps(
            bundled_path.to_string_lossy().as_ref(),
            None,
            7,
            "Noto Sans",
            "NotoSans-Regular",
        )]);
        prepend_bundled_ui_font(&mut fonts);
        state.fonts = fonts;
        assert_eq!(
            state.fonts[0].path, state.fonts[1].path,
            "the fixture only proves anything while both entries share one FILE"
        );

        state.selected_font_idx = 0;
        let bundled_signature = state.advanced_form_metric_signature();
        state.selected_font_idx = 1;
        let imported_signature = state.advanced_form_metric_signature();

        assert_eq!(
            bundled_signature.font_identity.as_deref(),
            Some(BUNDLED_UI_FONT_IDENTITY)
        );
        assert_eq!(
            imported_signature.font_identity.as_deref(),
            Some("NotoSans-Regular")
        );
        assert_ne!(
            bundled_signature, imported_signature,
            "two entries sharing a file but not an identity must not share a metric cache"
        );
    }

    /// egui preview families are keyed by IDENTITY plus CONTENT, so two fonts are told
    /// apart even when they share a file stem (or, for the bundled entry, the whole file),
    /// one font keeps ONE deterministic family across the independent create/edit panels,
    /// and REPLACING the bytes behind an identity produces a new family.
    #[test]
    fn preview_font_family_names_key_on_identity_and_content() {
        use crate::widgets::combo_font_family_name;

        const HASH_A: u64 = 0x1100_0000_0000_0000;
        const HASH_B: u64 = 0x2200_0000_0000_0000;

        let first = combo_font_family_name("Dup-Regular%1100000000000000", HASH_A, 0);
        let second = combo_font_family_name("Dup-Regular%2200000000000000", HASH_B, 0);
        assert_ne!(
            first, second,
            "two files with one file stem but different identities must not share a family"
        );
        assert_ne!(
            combo_font_family_name(BUNDLED_UI_FONT_IDENTITY, 0, 0),
            combo_font_family_name("NotoSans-Regular", HASH_A, 0),
            "the built-in entry and a user import of its file are different fonts"
        );
        assert_eq!(
            first,
            combo_font_family_name("Dup-Regular%1100000000000000", HASH_A, 0),
            "the name must be deterministic: both panels share one egui Context"
        );
        assert_ne!(
            first,
            combo_font_family_name("Dup-Regular%1100000000000000", HASH_A, 1),
            "faces of one font stay separate families"
        );
        // THE BYTE-SOURCE PROPERTY: an UNCONTESTED font keeps its PostScript name when its
        // file is replaced, and egui never re-reads a registered font — so only the content
        // hash can retire the stale binding.
        assert_ne!(
            combo_font_family_name("Plain-Regular", HASH_A, 0),
            combo_font_family_name("Plain-Regular", HASH_B, 0),
            "replacing the bytes behind one identity must produce a new egui family"
        );
    }

    #[test]
    fn advanced_form_metric_gate_mirrors_renderer_faux_face_contract() {
        // A distinctive incoming weight stands in for the selected face's own metadata
        // (`RegisteredFontFace::apply_to_attrs`), so a pass-through is distinguishable
        // from cosmic-text's defaults.
        let face_weight = cosmic_text::Weight(350);
        let seed = || {
            Attrs::new()
                .weight(face_weight)
                .style(cosmic_text::Style::Normal)
        };

        let all = create_advanced::MetricRealFaceAvailability::ALL;

        // force_* WITHOUT faux -> the REAL Bold/Italic faces are requested.
        let real =
            create_advanced::apply_metric_real_bold_italic(seed(), true, false, true, false, all);
        assert_eq!(real.weight, cosmic_text::Weight::BOLD);
        assert_eq!(real.style, cosmic_text::Style::Italic);

        // force_* WITH faux -> the selected face is kept, exactly like the renderer's
        // `base_attrs_real_bold_italic`: the style is synthesized geometrically, so the
        // metric must measure the face that is actually drawn.
        let faux =
            create_advanced::apply_metric_real_bold_italic(seed(), true, true, true, true, all);
        assert_eq!(faux.weight, face_weight);
        assert_eq!(faux.style, cosmic_text::Style::Normal);

        // faux_* without force_* is ignored on both sides.
        let unforced =
            create_advanced::apply_metric_real_bold_italic(seed(), false, true, false, true, all);
        assert_eq!(unforced.weight, face_weight);
        assert_eq!(unforced.style, cosmic_text::Style::Normal);
    }

    #[test]
    fn advanced_form_metric_gate_skips_faces_the_font_file_lacks() {
        let face_weight = cosmic_text::Weight(350);
        let seed = || {
            Attrs::new()
                .weight(face_weight)
                .style(cosmic_text::Style::Normal)
        };

        // A real request that the metric's font database cannot satisfy must leave the
        // selected face's attrs untouched: handing cosmic-text an unsatisfiable style
        // filter empties the fallback iterator and panics.
        let none = create_advanced::MetricRealFaceAvailability {
            bold: false,
            italic: false,
        };
        let gated =
            create_advanced::apply_metric_real_bold_italic(seed(), true, false, true, false, none);
        assert_eq!(gated.weight, face_weight);
        assert_eq!(gated.style, cosmic_text::Style::Normal);

        // Availability is per-axis: an italic-capable file still keeps the face weight.
        let italic_only = create_advanced::MetricRealFaceAvailability {
            bold: false,
            italic: true,
        };
        let partial = create_advanced::apply_metric_real_bold_italic(
            seed(),
            true,
            false,
            true,
            false,
            italic_only,
        );
        assert_eq!(partial.weight, face_weight);
        assert_eq!(partial.style, cosmic_text::Style::Italic);
    }

    #[test]
    fn advanced_form_metric_availability_probe_reads_the_loaded_faces() {
        let path = advanced_form_fixture_font_path();
        assert!(path.is_file(), "fixture font missing: {}", path.display());
        // The metric's database: empty, then the ONE selected file — the exact set
        // cosmic-text can match in `build_advanced_form_glyph_widths`.
        let mut db = fontdb::Database::new();
        db.load_font_file(&path)
            .expect("the fixture font must parse");

        let available = create_advanced::metric_real_face_availability(
            &db,
            cosmic_text::Style::Normal,
            cosmic_text::Stretch::Normal,
            true,
        );
        assert!(
            !available.italic,
            "LiberationSans-Regular ships no Italic face"
        );
        assert!(
            !available.bold,
            "LiberationSans-Regular ships no Bold-weight face"
        );
    }

    #[test]
    fn advanced_form_glyph_widths_keep_selected_face_under_faux_bold() {
        use forms::LineWidthMetric;
        const TEXT: &str = "Hello world";
        let mut state = advanced_form_fixture_panel();

        let plain = state
            .build_advanced_form_glyph_widths(TEXT)
            .expect("the fixture font must load")
            .line_width(TEXT);
        assert!(plain > 0, "the fixture font must produce real advances");

        // Faux bold: the renderer thickens the Regular face geometrically, so the
        // metric must report the Regular face's widths, not the real Bold face's.
        state.force_bold = true;
        state.faux_bold = true;
        let faux = state
            .build_advanced_form_glyph_widths(TEXT)
            .expect("the fixture font must load")
            .line_width(TEXT);
        assert_eq!(
            faux, plain,
            "faux bold must measure the same face the renderer draws"
        );

        // Real bold: the metric FontSystem holds an empty fontdb plus the one selected
        // FILE, so a Regular-only file has no Bold face to match and cosmic-text falls
        // back to it (weight is a ranking key, not a `Attrs::matches` filter). The
        // widths are therefore the Regular ones here too — documented, not asserted as
        // desirable; see this module's note on the metric FontSystem.
        state.faux_bold = false;
        let real = state
            .build_advanced_form_glyph_widths(TEXT)
            .expect("the fixture font must load")
            .line_width(TEXT);
        assert_eq!(
            real, plain,
            "a Regular-only file has no Bold face in the metric's empty fontdb"
        );
    }

    /// Regression: a REAL italic request against an upright-only font FILE used to reach
    /// cosmic-text with an empty match set (style is an `Attrs::matches` filter, unlike
    /// weight) and panic on the GUI thread at `shape.rs` `expect("no default font found")`.
    #[test]
    fn advanced_form_glyph_widths_survive_real_italic_on_upright_only_font() {
        use forms::LineWidthMetric;
        const TEXT: &str = "Hello world";
        let mut state = advanced_form_fixture_panel();

        let plain = state
            .build_advanced_form_glyph_widths(TEXT)
            .expect("the fixture font must load")
            .line_width(TEXT);
        assert!(plain > 0, "the fixture font must produce real advances");

        // Faux italic: the renderer shears the upright face, so the metric must keep it.
        state.force_italic = true;
        state.faux_italic = true;
        let faux = state
            .build_advanced_form_glyph_widths(TEXT)
            .expect("the fixture font must load")
            .line_width(TEXT);
        assert_eq!(
            faux, plain,
            "faux italic must measure the same face the renderer draws"
        );

        // Real italic («Курсив» WITHOUT «Принудительно»): the fixture file ships one
        // upright face, so the request is skipped and the upright face is measured
        // instead of handing cosmic-text an unsatisfiable style filter.
        state.faux_italic = false;
        let real = state
            .build_advanced_form_glyph_widths(TEXT)
            .expect("the fixture font must load")
            .line_width(TEXT);
        assert_eq!(
            real, plain,
            "an upright-only file has no Italic face for the metric to measure"
        );
    }

    /// The built-in interface font is offered as the FIRST entry of the panel font
    /// list, is shown under its localized name, and resolves by the reserved,
    /// non-localized identity that is what actually gets persisted.
    #[test]
    fn built_in_font_heads_the_panel_list_and_resolves_by_its_reserved_identity() {
        let state = TypingCreatePanelState::new(false);
        let Some(built_in) = state.fonts.first().filter(|font| font.bundled_stack_font().is_some())
        else {
            eprintln!(
                "skipping built_in_font_heads_the_panel_list_and_resolves_by_its_reserved_identity: \
                 no fonts/ui stack next to this checkout"
            );
            return;
        };

        assert_eq!(
            built_in.render_identity_name(),
            BUNDLED_UI_FONT_IDENTITY,
            "the persisted identity is the reserved, language-independent name"
        );
        assert_eq!(
            built_in.display_label(),
            t!("typing.fonts.bundled_ui_font_label"),
            "the SHOWN name comes from the catalog, not from the persisted identity"
        );
        assert!(
            built_in.path.is_file(),
            "the entry must point at a real font file so previews, the advanced-form \
             metric and PSD export keep working: {}",
            built_in.path.display()
        );
        assert_eq!(
            state.find_font_idx_by_identity(BUNDLED_UI_FONT_IDENTITY),
            Some(0),
            "the reserved identity must select the built-in entry"
        );
        // Projects saved before the reserved name was renamed persisted the previous
        // spelling; it must keep selecting the same entry in the PANEL, not only in the
        // renderer (the entry parks it in `original_name`, the family-name alias).
        assert_eq!(
            state.find_font_idx_by_name_forms(BUNDLED_UI_FONT_LEGACY_IDENTITY),
            Some(0),
            "the legacy spelling of the reserved identity must still select the built-in entry"
        );
        // And through the READ path a legacy document actually takes: no missing-font.
        let mut state = state;
        state.selected_font_idx = state.fonts.len().saturating_sub(1);
        state.select_font_by_legacy_reference(None, &[BUNDLED_UI_FONT_LEGACY_IDENTITY]);
        assert_eq!(state.selected_font_idx, 0);
        assert!(state.missing_font.is_none());
    }

    /// Degradation contract: a project saved with the built-in font, opened by a build
    /// that does NOT offer it, must land in the normal `missing_font` state — never on
    /// a silently substituted font. Modeled by a font list without the built-in entry
    /// (which is exactly what an older build produces: `should_skip_font_dir` keeps the
    /// whole `fonts/ui` subtree out of the panel list).
    #[test]
    fn a_build_without_the_built_in_font_reports_it_missing() {
        let mut state = TypingCreatePanelState::new(false);
        let bundled_path = state
            .fonts
            .first()
            .filter(|font| font.bundled_stack_font().is_some())
            .map(|font| font.path.to_string_lossy().to_string());
        // Only the user's own fonts, as an older build would list them.
        state.fonts = finalize_fonts(vec![raw_font_named(
            "/fonts/основной.ttf",
            None,
            1,
            "Anime Ace v05",
        )]);
        state.missing_font = None;

        state.select_font_by_legacy_reference(
            bundled_path.as_deref(),
            &[BUNDLED_UI_FONT_IDENTITY],
        );
        assert_eq!(
            state.missing_font.as_deref(),
            Some(BUNDLED_UI_FONT_IDENTITY),
            "the built-in font must degrade to 'font not found', naming it"
        );
    }

    /// The built-in entry must not disturb the identities of the user's own fonts: even
    /// a user copy of the very font the bundled stack points at keeps its own PostScript
    /// name as its identity, because the synthetic entry claims no name of its own and
    /// is excluded from the collision pass.
    #[test]
    fn the_built_in_entry_does_not_change_user_font_identities() {
        let mut fonts = finalize_fonts(vec![raw_font_ps(
            "/fonts/NotoSans-Regular.ttf",
            None,
            1,
            "Noto Sans",
            "NotoSans-Regular",
        )]);
        prepend_bundled_ui_font(&mut fonts);
        let user = fonts
            .iter()
            .find(|font| font.bundled_stack_font().is_none())
            .expect("the user font must stay in the list");
        assert_eq!(
            user.render_identity_name(),
            "NotoSans-Regular",
            "a user font's persisted identity must not change because of the built-in entry"
        );
    }

    /// The advanced-form width metric of the built-in entry must measure the whole
    /// bundled chain, not just the one `core` FILE the entry points at.
    ///
    /// Selecting the built-in font advertises full coverage, and the renderer delivers it
    /// through `MsFallback::common_fallback`; the metric used to build a throwaway
    /// `fontdb` holding only `core[0]`, so an ideograph was sized against the `.notdef`
    /// box of Noto Sans and every enumerated form came out systematically wrong.
    ///
    /// Asserted WITHOUT magic advance numbers: the same file selected as a plain user
    /// font is the "no chain" control. Latin (which the file itself covers) must measure
    /// identically in both — the chain must not change what the selected face already
    /// serves — while the ideograph must not.
    #[test]
    fn the_built_in_font_measures_form_widths_through_the_bundled_chain() {
        use forms::LineWidthMetric;
        const LATIN: &str = "A";
        const IDEOGRAPH: &str = "漢";

        let mut chained = TypingCreatePanelState::new(false);
        let Some(core_first) = chained
            .fonts
            .first()
            .and_then(FontEntry::bundled_stack_font)
        else {
            eprintln!(
                "skipping the_built_in_font_measures_form_widths_through_the_bundled_chain: \
                 no fonts/ui stack next to this checkout"
            );
            return;
        };
        let core_path = core_first.path.clone();
        chained.selected_font_idx = 0;
        chained.selected_face_idx = 0;
        seed_advanced_form_metric_font(&mut chained);

        // Control: the very same FILE, but as an ordinary user font, so nothing but that
        // file is in the metric database — the behavior this test pins as fixed.
        let mut single = TypingCreatePanelState::new(false);
        single.fonts = finalize_fonts(vec![raw_font(
            core_path.to_string_lossy().as_ref(),
            None,
            1,
        )]);
        single.font_provider = Arc::new(TabFontProvider::from_fonts(&single.fonts));
        single.selected_font_idx = 0;
        single.selected_face_idx = 0;
        seed_advanced_form_metric_font(&mut single);

        let measure = |state: &TypingCreatePanelState, text: &str| {
            state
                .build_advanced_form_glyph_widths(text)
                .expect("the bundled core file must load")
                .line_width(text)
        };

        assert_eq!(
            measure(&chained, LATIN),
            measure(&single, LATIN),
            "the chain must not change the advances the selected face itself provides"
        );
        assert_ne!(
            measure(&chained, IDEOGRAPH),
            measure(&single, IDEOGRAPH),
            "an ideograph the selected core FILE lacks must be measured through the \
             bundled chain, not as its .notdef box"
        );
    }

    /// End-to-end promise of the built-in entry: selecting it renders real pixels,
    /// including for a character the selected core FILE does not itself cover — that
    /// glyph comes from the rest of the bundled chain via the renderer's
    /// `common_fallback`, which is the whole reason this entry points at `core[0]`
    /// instead of inventing a "font chain" type.
    #[test]
    fn the_built_in_font_renders_through_the_bundled_fallback_chain() {
        let mut state = TypingCreatePanelState::new(false);
        if state
            .fonts
            .first()
            .and_then(FontEntry::bundled_stack_font)
            .is_none()
        {
            eprintln!(
                "skipping the_built_in_font_renders_through_the_bundled_fallback_chain: \
                 no fonts/ui stack next to this checkout"
            );
            return;
        }
        state.selected_font_idx = 0;
        state.selected_face_idx = 0;
        // Latin (covered by the core file itself) + a CJK ideograph (covered only by
        // the next font of the core chain).
        state.text = "Aa 漢".to_string();

        let params = state
            .build_render_params()
            .expect("the built-in font must produce render params");
        assert_eq!(params.font_name, BUNDLED_UI_FONT_IDENTITY);
        let provider = state.font_provider();
        let image = crate::tabs::typing::render_next::render_text_to_image(
            &params,
            provider.as_ref(),
            None,
        )
        .expect("the built-in font must render");
        assert!(
            image.rgba.iter().skip(3).step_by(4).any(|alpha| *alpha > 0),
            "the render must contain visible ink"
        );
    }

    /// Builds a report with one fallback font and no lost characters.
    fn fallback_report(family: &str, chars: &[char]) -> FontFallbackReport {
        FontFallbackReport {
            fallbacks: vec![ms_text_render::types::FontFallbackUse {
                family: family.to_string(),
                chars: chars.to_vec(),
            }],
            missing: Vec::new(),
        }
    }

    #[test]
    fn a_clean_render_shows_no_font_diagnostic_rows() {
        let lines = create_presets::font_fallback_status_lines(&FontFallbackReport::default());
        assert!(
            lines.is_empty(),
            "a text fully served by the selected font must add nothing to the panel"
        );
    }

    #[test]
    fn a_fallback_is_informational_and_a_lost_character_is_not() {
        // Fallback only: one row, warning color (it rendered, just not in the
        // selected typeface).
        let used = create_presets::font_fallback_status_lines(&fallback_report(
            "Source Han Sans K",
            &['漢'],
        ));
        assert_eq!(used.len(), 1);
        assert_eq!(
            used[0].color,
            create_presets::FONT_DIAGNOSTIC_WARNING_COLOR,
            "a fallback must not be painted like an error"
        );
        assert!(used[0].text.contains('漢'));
        assert!(used[0].text.contains("Source Han Sans K"));

        // Lost characters only: one row, error color.
        let lost = create_presets::font_fallback_status_lines(&FontFallbackReport {
            fallbacks: Vec::new(),
            missing: vec!['\u{e000}'],
        });
        assert_eq!(lost.len(), 1);
        assert_eq!(lost[0].color, create_presets::FONT_DIAGNOSTIC_ERROR_COLOR);

        // Both: the informational row comes first, the alarming one last.
        let mut both = fallback_report("Noto Sans Arabic", &['ب']);
        both.missing = vec!['\u{e000}'];
        let rows = create_presets::font_fallback_status_lines(&both);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].color, create_presets::FONT_DIAGNOSTIC_WARNING_COLOR);
        assert_eq!(rows[1].color, create_presets::FONT_DIAGNOSTIC_ERROR_COLOR);
    }

    #[test]
    fn a_long_character_list_is_truncated_so_it_cannot_blow_up_the_panel() {
        // 40 distinct characters, well over the shared display cap.
        let many: Vec<char> = ('\u{4e00}'..).take(40).collect();
        let rows = create_presets::font_fallback_status_lines(&fallback_report("Some Font", &many));
        assert_eq!(rows.len(), 1);
        let shown = many
            .iter()
            .filter(|ch| rows[0].text.contains(**ch))
            .count();
        assert!(
            shown < many.len(),
            "the list must be truncated, but all {} characters are present",
            many.len()
        );
    }

    // ---- Locating an imported system font BY NAME (identity plan, phase 6) -------------

    /// One fake INSTALLED face for the system-font name index. Unit tests never enumerate the
    /// machine's real fonts (the enumerator is stubbed in test builds), so what is "installed"
    /// is exactly what a test says is installed — the only way these tests can be reproducible
    /// on a machine other than this one.
    fn installed_face(post_script_name: &str, path: &Path) -> SystemFaceRecord {
        SystemFaceRecord {
            post_script_name: post_script_name.to_string(),
            path: path.to_path_buf(),
        }
    }

    /// The identity (PostScript name) a font FILE claims, read through the very loader the
    /// store uses — so a fixture change can never desynchronize the expectation from reality.
    fn identity_of_font_file(path: &Path) -> String {
        let rows = load_imported_system_font_rows(&[fonts_data::SystemFontRef {
            font: String::new(),
            last_path: Some(path.to_path_buf()),
        }]);
        rows.into_iter()
            .next()
            .map(|row| row.stored_identity)
            .expect("the fixture file must produce a row")
    }

    /// Seeds the store with exactly one imported system font.
    fn seed_one_imported_font(identity: &str, last_path: Option<PathBuf>) {
        super::font_settings_store::test_seed(fonts_data::FontsData {
            system_fonts: vec![fonts_data::SystemFontRef {
                font: identity.to_string(),
                last_path,
            }],
            fonts: std::collections::BTreeMap::new(),
            virtual_groups: Vec::new(),
            pending_migration: false,
        });
    }

    /// STEP 1 of the phase-6 resolution order: a recorded path hint whose file still claims the
    /// recorded name is used AS IS. The whole-OS scan behind step 2 is expensive, so it must
    /// not run at all in this case — proven by the index build counter, not by timing.
    #[test]
    fn a_valid_path_hint_resolves_an_imported_font_without_scanning_the_system() {
        let _lock = super::font_settings_store::test_lock();
        super::font_settings_store::test_reset();
        test_reset_system_font_index();

        let dir = unique_temp_dir("hint_hit");
        fs::create_dir_all(&dir).expect("create temp dir");
        let file = dir.join("Imported.ttf");
        fs::copy(advanced_form_fixture_font_path(), &file).expect("copy fixture");
        let identity = identity_of_font_file(&file);
        seed_one_imported_font(&identity, Some(file.clone()));

        let rows =
            load_imported_system_font_rows(&super::font_settings_store::imported_system_font_refs());
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].unavailable.is_none(),
            "the recorded file still holds that font"
        );
        assert_eq!(
            rows[0].entry.as_ref().map(|entry| entry.path.clone()),
            Some(file.clone())
        );
        assert_eq!(
            test_system_font_index_builds(),
            0,
            "a hint that resolves must never trigger the whole-OS font scan"
        );

        super::font_settings_store::test_reset();
        test_reset_system_font_index();
        let _ = fs::remove_dir_all(&dir);
    }

    /// STEP 2, the point of phase 6: the system font was MOVED (a package update, a different
    /// directory, a different file name). The recorded hint is dead, the font is found by its
    /// PostScript name among the installed ones, and the hint is rewritten so the next launch
    /// resolves at step 1 again.
    #[test]
    fn a_moved_system_font_is_located_by_name_and_the_recorded_hint_follows_it() {
        let _lock = super::font_settings_store::test_lock();
        super::font_settings_store::test_reset();
        test_reset_system_font_index();

        let dir = unique_temp_dir("relocate");
        let old_dir = dir.join("old");
        let new_dir = dir.join("new");
        fs::create_dir_all(&old_dir).expect("create temp dir");
        fs::create_dir_all(&new_dir).expect("create temp dir");
        let old_path = old_dir.join("Imported.ttf");
        let new_path = new_dir.join("Imported-v2.ttf");
        fs::copy(advanced_form_fixture_font_path(), &old_path).expect("copy fixture");
        let identity = identity_of_font_file(&old_path);

        // The very same font now lives somewhere else, under another file name.
        fs::rename(&old_path, &new_path).expect("move the font file");
        seed_one_imported_font(&identity, Some(old_path.clone()));
        test_install_system_faces(vec![installed_face(&identity, &new_path)]);

        let rows =
            load_imported_system_font_rows(&super::font_settings_store::imported_system_font_refs());
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].unavailable.is_none(),
            "the moved font must be located by NAME, not reported as missing"
        );
        assert_eq!(
            rows[0].entry.as_ref().map(|entry| entry.path.clone()),
            Some(new_path.clone()),
            "the entry must be loaded from where the font actually is now"
        );
        assert_eq!(rows[0].last_path.as_deref(), Some(new_path.as_path()));
        assert_eq!(
            super::font_settings_store::imported_system_font_refs()[0]
                .last_path
                .as_deref(),
            Some(new_path.as_path()),
            "the stored hint must follow the font, so the next launch needs no scan"
        );
        assert_eq!(
            test_system_font_index_builds(),
            1,
            "one scan is enough to relocate every entry of one load"
        );

        super::font_settings_store::test_reset();
        test_reset_system_font_index();
        let _ = fs::remove_dir_all(&dir);
    }

    /// STEP 3: a font that is installed NOWHERE under its recorded name must not be dropped.
    /// The document entry is the user's record that they imported it, and the row is the only
    /// thing that can ever carry the remove action for it.
    #[test]
    fn an_imported_font_installed_nowhere_stays_as_a_removable_unavailable_row() {
        let _lock = super::font_settings_store::test_lock();
        super::font_settings_store::test_reset();
        test_reset_system_font_index();

        let dir = unique_temp_dir("vanished");
        fs::create_dir_all(&dir).expect("create temp dir");
        let gone = dir.join("Gone.ttf");
        seed_one_imported_font("Vanished-Regular", Some(gone.clone()));
        // The machine has fonts, just not this one.
        test_install_system_faces(vec![installed_face(
            "Unrelated-Regular",
            Path::new("/nowhere/unrelated.ttf"),
        )]);

        let rows =
            load_imported_system_font_rows(&super::font_settings_store::imported_system_font_refs());
        assert_eq!(rows.len(), 1, "the entry must still produce a row");
        assert!(rows[0].entry.is_none());
        assert!(matches!(
            rows[0].unavailable,
            Some(ImportedFontUnavailable::Unreadable(_))
        ));
        assert_eq!(
            rows[0].stored_identity, "Vanished-Regular",
            "the DOCUMENT key survives — it is what the remove button matches"
        );
        assert_eq!(
            super::font_settings_store::imported_system_font_refs().len(),
            1,
            "a font that could not be located is never pruned from the document"
        );
        assert_eq!(test_system_font_index_builds(), 1);

        super::font_settings_store::test_reset();
        test_reset_system_font_index();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Several installed FILES can declare one PostScript name. The choice among them must be
    /// deterministic AND independent of the order the faces were enumerated in: the winner is
    /// the lowest content hash, the same rule the identity contract uses to decide which
    /// contested claimant a bare name means.
    #[test]
    fn a_system_font_name_claimed_by_several_files_resolves_to_the_lowest_content_hash() {
        let _lock = super::font_settings_store::test_lock();
        super::font_settings_store::test_reset();
        test_reset_system_font_index();

        let dir = unique_temp_dir("name_collision");
        fs::create_dir_all(&dir).expect("create temp dir");
        let bytes = fs::read(advanced_form_fixture_font_path()).expect("read the fixture font");
        // Trailing bytes past the last table change the CONTENT without touching any table,
        // so the two files are two different files claiming one PostScript name — exactly the
        // shape of a real variable-vs-static collision, without depending on what is installed.
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0_u8; 64]);
        let plain = dir.join("plain.ttf");
        let extended = dir.join("extended.ttf");
        fs::write(&plain, &bytes).expect("write the plain copy");
        fs::write(&extended, &padded).expect("write the padded copy");

        let identity = identity_of_font_file(&plain);
        assert_eq!(
            identity_of_font_file(&extended),
            identity,
            "both files must claim the SAME PostScript name for this to be a collision"
        );
        let plain_hash = font_content_hash(&bytes);
        let extended_hash = font_content_hash(&padded);
        assert_ne!(plain_hash, extended_hash, "the two files must differ in bytes");
        let expected = if plain_hash < extended_hash {
            plain.clone()
        } else {
            extended.clone()
        };

        // Both enumeration orders must produce the same winner.
        for faces in [
            vec![
                installed_face(&identity, &plain),
                installed_face(&identity, &extended),
            ],
            vec![
                installed_face(&identity, &extended),
                installed_face(&identity, &plain),
            ],
        ] {
            super::font_settings_store::test_reset();
            seed_one_imported_font(&identity, None);
            test_install_system_faces(faces);
            let rows = load_imported_system_font_rows(
                &super::font_settings_store::imported_system_font_refs(),
            );
            assert_eq!(
                rows[0].entry.as_ref().map(|entry| entry.path.clone()),
                Some(expected.clone()),
                "the lowest-content-hash claimant must win regardless of enumeration order"
            );
        }

        super::font_settings_store::test_reset();
        test_reset_system_font_index();
        let _ = fs::remove_dir_all(&dir);
    }

    /// The index is PROCESS-global: the whole-OS scan behind it happens once, off the GUI
    /// thread, and every later worker shares the very same snapshot instead of scanning again.
    #[test]
    fn the_system_font_name_index_is_built_once_and_shared_by_every_thread() {
        let _lock = super::font_settings_store::test_lock();
        test_reset_system_font_index();
        test_install_system_faces(vec![
            installed_face("Alpha-Regular", Path::new("/fake/alpha.ttf")),
            installed_face("Beta-Regular", Path::new("/fake/beta.ttf")),
            // A `.ttc`-shaped duplicate: the same file under the same name twice.
            installed_face("Beta-Regular", Path::new("/fake/beta.ttf")),
        ]);

        let handles: Vec<std::thread::JoinHandle<Arc<SystemFontNameIndex>>> = (0..4)
            .map(|_| std::thread::spawn(system_font_name_index))
            .collect();
        let indexes: Vec<Arc<SystemFontNameIndex>> = handles
            .into_iter()
            .map(|handle| handle.join().expect("index build thread"))
            .collect();

        assert_eq!(
            test_system_font_index_builds(),
            1,
            "the whole-OS scan must happen once per process, not once per worker thread"
        );
        let first = &indexes[0];
        assert!(
            indexes.iter().all(|index| Arc::ptr_eq(index, first)),
            "every thread must observe the one cached index"
        );
        assert_eq!(
            first.name_count(),
            2,
            "duplicate (name, path) records collapse into one candidate"
        );

        test_reset_system_font_index();
    }


    /// Builds one row of the system-font PICKER catalog exactly the way
    /// `fonts::load_system_fonts` does: the base identity (no collision pass runs on that
    /// list) and the `0` "content unknown" hash, because the catalog enumerates faces
    /// through `fontdb` without reading whole files.
    fn catalog_entry(path: &Path, family: &str, post_script_name: &str) -> FontEntry {
        let label = format!(
            "{} [system]",
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("system font")
        );
        FontEntry {
            kind: FontEntryKind::File,
            label: label.clone(),
            path: path.to_path_buf(),
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces: vec![FontFaceEntry {
                label: format!("#0 {family} | Normal | w400 | {post_script_name}"),
                face_index: 0,
                post_script_name: post_script_name.to_string(),
            }],
            coverage: FontLanguageCoverage::default(),
            original_name: family.to_string(),
            post_script_name: post_script_name.to_string(),
            content_hash: 0,
            display_name: None,
            identity_name: base_font_identity_name(post_script_name, family, &label),
            virtual_group_aliases: std::collections::BTreeMap::new(),
        }
    }

    /// Two DIFFERENT installed files declaring ONE PostScript name must not share the
    /// own-typeface preview registration of the import picker: the egui family is named
    /// after `(identity, content hash, face index)`, so with the catalog's `0` sentinel on
    /// both rows the second row was drawn in the FIRST file's typeface.
    ///
    /// An uncontested row keeps the sentinel — proof that its file was never read, which
    /// is what keeps the catalog affordable (thousands of installed files).
    #[test]
    fn contested_catalog_identities_do_not_share_a_preview_registration() {
        let dir = std::env::temp_dir().join(format!("ms_catalog_hash_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the test temp directory must be creatable");
        let variable = dir.join("Ubuntu-variable.ttf");
        let static_cut = dir.join("Ubuntu-static.ttf");
        let unique = dir.join("Solo.ttf");
        std::fs::write(&variable, b"variable cut bytes").expect("fixture must be writable");
        std::fs::write(&static_cut, b"static cut bytes, different").expect("fixture writable");
        std::fs::write(&unique, b"lonely font bytes").expect("fixture must be writable");

        let mut entries = vec![
            catalog_entry(&variable, "Ubuntu", "Ubuntu"),
            catalog_entry(&static_cut, "Ubuntu", "Ubuntu"),
            catalog_entry(&unique, "Solo", "Solo-Regular"),
        ];
        assert_eq!(
            entries[0].identity_name, entries[1].identity_name,
            "the fixture must reproduce the real collision: one identity, two files"
        );

        fonts::resolve_contested_catalog_content_hashes(&mut entries);

        let family_of = |entry: &FontEntry| {
            crate::widgets::combo_font_family_name(
                &entry.render_identity_name(),
                entry.content_hash(),
                entry.representative_face_index(),
            )
        };
        assert_ne!(
            family_of(&entries[0]),
            family_of(&entries[1]),
            "two files claiming one name must register two different preview families"
        );
        assert_eq!(
            entries[2].content_hash, 0,
            "an uncontested row must keep the sentinel — its file is never read"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test `FontProvider` that BLOCKS inside `resolve` until it is released, so a test
    /// can prove the call does not happen on the calling (GUI) thread.
    ///
    /// `finished` flips only after `resolve` returns, and `gate` is what holds it. Mirrors
    /// `tab/create_upload.rs`'s provider of the same shape — the editor font and the
    /// width-metric font are the two places the panel resolves bytes off-thread.
    struct GatedFontProvider {
        gate: std::sync::Mutex<Receiver<()>>,
        finished: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FontProvider for GatedFontProvider {
        fn resolve(&self, name: &str) -> Option<FontContent> {
            // A poisoned lock cannot happen here (the test never panics while holding it),
            // and recovering keeps the test failure readable instead of a second panic.
            let gate = match self.gate.lock() {
                Ok(gate) => gate,
                Err(poisoned) => poisoned.into_inner(),
            };
            let _ = gate.recv();
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Some(FontContent {
                name: name.to_string(),
                original_name: name.to_string(),
                data: Arc::new(Vec::new()),
                face_index: 0,
                content_id: 7,
            })
        }
    }

    /// Drives `poll_advanced_form_font` until the request is consumed, without blocking
    /// forever if it never is.
    fn poll_form_metric_font_until_settled(
        state: &mut TypingCreatePanelState,
        ctx: &egui::Context,
    ) -> bool {
        for _ in 0..2000 {
            state.poll_advanced_form_font(ctx);
            if state.advanced_form_font_request.is_none() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    /// Obtaining the width-metric font's BYTES is `fs::read` on a provider cache miss, so
    /// it must never run on the GUI thread (`CLAUDE.md` §5) — the form window is drawn
    /// there. `poll_advanced_form_font` therefore only dispatches: with the provider held
    /// inside `resolve`, the call still returns, nothing is cached, and the metric falls
    /// back to per-character widths until the bytes land.
    #[test]
    fn polling_the_form_metric_font_does_not_resolve_on_the_calling_thread() {
        let ctx = egui::Context::default();
        let (release, gate) = mpsc::channel::<()>();
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut state = advanced_form_fixture_panel_without_metric_font();
        state.font_provider = Arc::new(GatedFontProvider {
            gate: std::sync::Mutex::new(gate),
            finished: Arc::clone(&finished),
        });

        state.poll_advanced_form_font(&ctx);
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "the resolve must not have run on the calling thread"
        );
        assert!(
            state.advanced_form_font_request.is_some(),
            "an unfinished resolve must leave the request in flight, not block the caller"
        );
        assert!(
            state.advanced_form_font.is_none(),
            "nothing may be cached before the bytes arrive"
        );
        assert!(
            state.build_advanced_form_glyph_widths("Hello world").is_none(),
            "without bytes the window must fall back to the per-character metric, never \
             read the file itself"
        );
        assert!(
            state.advanced_form_metric_signature().font_content_id.is_none(),
            "the signature must record that the forms were enumerated without glyph widths"
        );

        release.send(()).expect("the worker holds the receiver");
        assert!(
            poll_form_metric_font_until_settled(&mut state, &ctx),
            "the result must land"
        );
        assert_eq!(
            state.advanced_form_metric_signature().font_content_id,
            Some(7),
            "the arrival of the bytes must change the signature, which is what rebuilds \
             the form cache with real glyph widths"
        );
    }
