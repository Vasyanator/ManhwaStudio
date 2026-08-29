/*
File: panel/create_state.rs

Purpose:
Holds part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
create-panel lifecycle/construction, focus and eyedropper tracking, font-group
management, and font-index lookup helpers.

Main responsibilities:
- construct the create-panel state with an EMPTY font list — the constructor reads no
  font file at all (CLAUDE.md §5); the list arrives from a background load; the create
  panel's parameter identity mode comes from `user_config.TextTab`;
- track focused text inputs and eyedropper activation per frame;
- manage the selected font group and pending group requests;
- spawn and poll background font reloads (folder fonts + imported system-font paths),
  picking up live settings-side import/remove via `poll_font_settings_changes`;
- resolve fonts by IDENTITY (the one selection key) and, on the legacy READ path
  only, by a persisted path/name reference; filter fonts by group.

Key functions:
- `TypingCreatePanelState::new` — construction; no font I/O.
- `spawn_shared_font_reload` (free fn) — ONE background load serving BOTH panels.
- `TypingCreatePanelState::poll_font_reload_results` — installs a finished load,
  performs the INITIAL selection, marks the list authoritative and releases whatever
  was waiting for it (the one-shot legacy-preset migration).
- `panel_fonts_dir` / `set_test_fonts_dir` — the fonts directory a panel binds to;
  injectable under `#[cfg(test)]`, and never the checkout's own `fonts/` there.

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so sibling child
modules under `panel/` can call them. `use super::*;` pulls in the parent
module's types and imports.
*/

use super::*;

/// Fonts directory a freshly constructed panel binds to.
///
/// In a normal build this is `fonts::resolve_fonts_dir()`. Under `#[cfg(test)]` it is an
/// INJECTED directory (`set_test_fonts_dir`), defaulting to a per-thread path that does not
/// exist: a unit test must never depend on — or be timed by — whatever font bundle happens
/// to sit next to the developer's checkout. Tests that DO want real font files create a temp
/// dir, copy the fixtures they need into it, and inject that.
#[must_use]
fn panel_fonts_dir() -> PathBuf {
    #[cfg(test)]
    {
        test_fonts_dir_override().unwrap_or_else(|| {
            // A path that is deliberately absent: every loader treats a missing fonts dir
            // as "no fonts", which is the state these tests want and already support.
            std::env::temp_dir().join("manhwastudio-tests-no-fonts-dir")
        })
    }
    #[cfg(not(test))]
    {
        resolve_fonts_dir()
    }
}

#[cfg(test)]
thread_local! {
    /// Per-THREAD so parallel tests cannot see each other's injection.
    static TEST_FONTS_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Reads this thread's injected panel fonts directory, if any.
#[cfg(test)]
fn test_fonts_dir_override() -> Option<PathBuf> {
    TEST_FONTS_DIR.with(|dir| dir.borrow().clone())
}

/// Points every panel constructed on THIS thread at `dir` (`None` restores the
/// nonexistent default). Test-only; the production panel always resolves the real
/// `fonts/` directory.
#[cfg(test)]
pub(super) fn set_test_fonts_dir(dir: Option<PathBuf>) {
    TEST_FONTS_DIR.with(|slot| *slot.borrow_mut() = dir);
}

impl TypingCreatePanelState {
    /// Builds a create/edit panel whose font list is EMPTY and is filled by a background
    /// load the caller starts (`spawn_shared_font_reload`, or this panel's own
    /// `spawn_font_reload`).
    ///
    /// NOTHING here touches the font directory. Scanning, reading, hashing and parsing
    /// every font file is exactly the work CLAUDE.md §5 forbids on the GUI thread, and the
    /// constructor used to do it TWICE per session (once per panel). The empty list is an
    /// already-supported state: the combo shows nothing, the status line says the list is
    /// loading, and `poll_font_reload_results` performs the initial selection when the
    /// list lands.
    pub(super) fn new(preview_enabled: bool) -> Self {
        let fonts_dir = panel_fonts_dir();
        // Snapshot the runtime-global imported system-font paths (seeded at startup from
        // config), so the background load merges exactly what the store knows now.
        let imported_system_fonts = super::font_settings_store::imported_system_fonts();
        let imported_fonts_revision = super::font_settings_store::imported_fonts_revision();
        // The bundled-stack entry belongs to the PANEL list only (the settings
        // font-administration list must not get it), so it is injected here and again by
        // `load_fonts` in the reload worker. It carries `'static` bytes and reads no file,
        // so it is the one entry a fresh panel may already hold.
        let mut fonts: Vec<FontEntry> = Vec::new();
        prepend_bundled_ui_font(&mut fonts);
        // Font groups (real folder groups + virtual ones) arrive with the same background
        // load; an empty list means "no group filter offered yet", which the combo already
        // renders correctly.
        let font_groups: Vec<String> = Vec::new();
        // Presets live in `fonts/presets.json` and belong to the CREATE panel only. Reading
        // that document (and the legacy `user_config.json` payload behind a pending one-shot
        // migration) is file I/O, so it runs on a worker and lands through
        // `poll_preset_store_events`; the panel simply starts empty.
        let (preset_store_tx, preset_store_rx) = mpsc::channel::<PresetStoreEvent>();
        if preview_enabled {
            super::create_presets::spawn_presets_seed(&fonts_dir, &preset_store_tx);
        }
        let formula_presets_by_name = load_text_tab_formula_presets();
        // The parameter-identity mode is a per-panel preference of the CREATE panel and is
        // read from `user_config.TextTab` (a small document the constructor already reads
        // for the formula presets and the effect defaults). The edit panel never offers the
        // switch, so it stays on the default `Font` mode without touching the config at all.
        let identity_mode = if preview_enabled {
            load_text_tab_param_identity_mode()
        } else {
            ParamIdentityMode::Font
        };
        let (request_tx, result_rx) = spawn_preview_render_worker();
        // The list is not loaded yet, so the honest status is "loading", not "no fonts".
        let status_line = t!("typing.fonts.reloading_status").to_string();
        let font_provider: Arc<dyn FontProvider> = Arc::new(TabFontProvider::from_fonts(&fonts));
        let mut state = Self {
            fonts_dir,
            fonts,
            font_provider,
            font_groups,
            selected_font_group: None,
            imported_system_fonts,
            imported_fonts_revision,
            pending_font_group_request: None,
            pending_settings_link_request: None,
            font_reload_rx: None,
            latest_font_reload_token: 0,
            fonts_reload_in_flight: false,
            font_list_is_authoritative: false,
            pending_legacy_presets_migration: None,
            font_profiles_by_identity: FontProfileMemory::default(),
            active_font_identity: None,
            missing_font: None,
            presets_by_name: HashMap::new(),
            preset_store_tx,
            preset_store_rx,
            selected_preset_name: None,
            preset_name_input: String::new(),
            selected_preset_dirty: false,
            preset_delete_armed: false,
            identity_mode,
            // The local-preset set arrives with the off-thread `presets.json` seed, exactly
            // like the global presets: the panel starts with none.
            local_presets: Vec::new(),
            selected_local_preset: None,
            local_preset_name_input: String::new(),
            default_local_set: presets_store::DefaultLocalSet::default(),
            local_presets_dirty_since: None,
            local_presets_generation: 0,
            local_presets_saved_generation: 0,
            local_presets_save_retries: 0,
            local_preset_previews: local_preset_preview::LocalPresetPreviewCache::new(),
            formula_presets_by_name,
            selected_formula_preset_name: None,
            formula_preset_name_input: String::new(),
            preview_enabled,
            selected_font_idx: 0,
            selected_face_idx: 0,
            text: default_preview_text().to_string(),
            text_color: Color32::BLACK,
            text_color_selector: ViewportColorSelector::default(),
            font_size_px: 24.0,
            line_spacing: PxOrPercent::percent(0.0),
            // Default keeps font-pair kerning (byte-identical to the historical
            // `Metric` default), now named `Auto`.
            kerning_mode: KerningMode::Auto,
            kerning: PxOrPercent::percent(0.0),
            glyph_height: PxOrPercent::percent(100.0),
            glyph_width: PxOrPercent::percent(100.0),
            width_px: DEFAULT_PREVIEW_WIDTH_PX,
            align: HorizontalAlign::CENTER,
            global_rotation_deg: 0.0,
            line_placement_percent: 0.0,
            // New text uses the shared-line-box anchoring (clean curved string);
            // legacy per-glyph anchoring is opt-out via the panel toggle.
            line_placement_reference: LinePlacementReference::LineBox,
            pending_raster_transform: None,
            text_line_mode: TextLineMode::Horizontal,
            vertical_line_direction: VerticalLineDirection::RightToLeft,
            text_layout_mode: TextLayoutMode::Normal,
            formula_layout: TextFormulaLayoutParams::default(),
            drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
            vector_lines_layout: TextVectorLinesLayoutParams::default(),
            shape_layout_kind: TypingShapeLayoutKind::Arc,
            arc_shape_layout: TypingArcShapeLayoutParams::default(),
            circle_shape_layout: TypingCircleShapeLayoutParams::default(),
            spiral_shape_layout: TypingSpiralShapeLayoutParams::default(),
            polygon_shape_layout: TypingPolygonShapeLayoutParams::default(),
            zigzag_shape_layout: TypingZigzagShapeLayoutParams::default(),
            s_curve_shape_layout: TypingSCurveShapeLayoutParams::default(),
            formula_help_open: false,
            text_shape: TextShape::Free,
            text_wrap_mode: TextWrapMode::Aggressive,
            anti_aliasing: AntiAliasingMode::Strong,
            allow_moderate_trees: false,
            shape_min_width_percent: 50.0,
            shape_variant: 5,
            force_bold: false,
            force_italic: false,
            faux_bold: false,
            faux_bold_thicken_percent: 3.0,
            faux_bold_expand_percent: 0.0,
            faux_bold_sharp_corners: true,
            // NEW overlays get the uniform-weight mode (every boundary moves by `d`, every
            // stem by `2*d`). Deliberately NOT the frozen schema-2 default, which stays
            // `true` for already-saved documents — see `text_params_schema.rs`.
            faux_bold_outward_only: false,
            faux_italic: false,
            faux_italic_slant_deg: 14.0,
            uppercase_text: false,
            trim_extra_spaces: true,
            replace_ellipsis_with_dots: true,
            // Off by default: patching the font's GSUB table is a deliberate opt-in for
            // fonts that re-ligate the three dots back into an ellipsis glyph.
            force_remove_ellipsis_glyph: false,
            hanging_punctuation: true,
            new_line_after_sentence: false,
            enable_inline_style_tags: false,
            use_legacy_inline_tags: load_text_tab_use_legacy_inline_tags(),
            overlay_scale: 1.0,
            overlay_rotation_deg: 0.0,
            effect_to_add: AvailableEffectKind::Stroke,
            effects: Vec::new(),
            request_tx,
            result_rx,
            latest_token: 0,
            render_in_flight: false,
            needs_initial_preview: true,
            status_line,
            preview_font_fallbacks: FontFallbackReport::default(),
            preview_texture: None,
            preview_size: [1, 1],
            tracked_text_input_ids: Vec::new(),
            text_selection_char_range: None,
            pending_text_selection_restore: None,
            inline_text_target: InlineTextTarget::Source,
            advanced_form_open: false,
            advanced_form_preset: TextFormPreset::FreeNoTree,
            advanced_form_group: None,
            advanced_form_cache: None,
            advanced_form_search: None,
            advanced_form_search_debounce: None,
            advanced_form_params_save_pending: None,
            advanced_form_font: None,
            advanced_form_font_request: None,
            formed_text: String::new(),
            advanced_form_tags_lost: false,
            advanced_text_show_formed: false,
            advanced_form_line_range: None,
            advanced_form_width_range: None,
            advanced_form_peak_max: 0,
            advanced_form_peak_base: PeakBase::Min,
            advanced_form_uneven_max: 0,
            advanced_form_conservatism_max: Conservatism::Safe,
            advanced_form_centered: false,
            // Closed and unloaded: the character table reads nothing from disk
            // until the user first opens its window.
            char_table: super::char_table::CharTableState::new(),
        };
        // No font has been CHOSEN yet — the list is still loading. Leaving this `None` is
        // what tells `poll_font_reload_results` to perform the initial selection (the
        // first of the user's OWN fonts) instead of "restoring" the bundled entry that
        // happens to occupy index 0 in the meantime.
        state.active_font_identity = None;
        state.sync_selected_formula_preset_by_layout();
        state
    }

    /// Shared font source for renders built from this panel's current font list.
    /// Cheap to clone (Arc); hand it to every background render worker.
    pub(in crate::tabs::typing) fn font_provider(&self) -> Arc<dyn FontProvider> {
        Arc::clone(&self.font_provider)
    }

    pub(super) fn reset_text_input_focus_tracking(&mut self) {
        self.tracked_text_input_ids.clear();
    }

    pub(super) fn track_text_input(&mut self, response: &egui::Response) {
        self.tracked_text_input_ids.push(response.id);
    }

    pub(super) fn has_focused_text_input(&self, ctx: &egui::Context) -> bool {
        let Some(focused) = ctx.memory(|mem| mem.focused()) else {
            return false;
        };
        self.tracked_text_input_ids.contains(&focused)
    }

    pub(super) fn eyedropper_active(&self) -> bool {
        if self.text_color_selector.eyedropper_active() {
            return true;
        }
        self.effects.iter().any(EffectCard::eyedropper_active)
    }

    pub(super) fn eyedropper_consumed_primary_click_this_frame(&self) -> bool {
        if self.text_color_selector.primary_click_consumed_this_frame() {
            return true;
        }
        self.effects
            .iter()
            .any(EffectCard::eyedropper_consumed_primary_click_this_frame)
    }

    /// Picks up a settings-side import/remove of system fonts and applies it LIVE to this
    /// open panel: when the store's revision advanced since the last check, refreshes the
    /// snapshot of imported paths and spawns a background font reload so the new list takes
    /// effect without reopening the panel. Cheap no-op when the revision is unchanged.
    pub(super) fn poll_font_settings_changes(&mut self) {
        let revision = super::font_settings_store::imported_fonts_revision();
        if revision == self.imported_fonts_revision {
            return;
        }
        self.imported_fonts_revision = revision;
        self.imported_system_fonts = super::font_settings_store::imported_system_fonts();
        self.spawn_font_reload();
    }

    pub(super) fn take_font_group_request(&mut self) -> Option<Option<String>> {
        self.pending_font_group_request.take()
    }

    /// Drains the pending settings deep-link request raised by the font-group "?" help
    /// icon (`Some` once until taken). The facade forwards it up to the app tab-switch.
    pub(super) fn take_settings_link_request(
        &mut self,
    ) -> Option<crate::settings_shared::SettingsDeepLink> {
        self.pending_settings_link_request.take()
    }

    /// Применяет выбранную группу шрифтов (для синхронизации между панелями).
    /// Возвращает `true`, если группа изменилась.
    ///
    /// The requested group name is stored VERBATIM and is NOT validated against this
    /// panel's current `font_groups` here: the two create/edit panels reload their font
    /// lists independently, so a group just created in the other panel may not yet be in
    /// this panel's list. Immediately dropping it (via `sync_selected_font_group`) would
    /// silently reset the selection to "All groups" until re-picked. Validation happens on
    /// every reload result (`poll_font_reload_results` calls `sync_selected_font_group`),
    /// which clears a truly nonexistent name once the fresh list lands.
    /// `ensure_selected_font_in_group` still runs against the current list so the font
    /// selection stays valid for whatever group members are already loaded.
    pub(super) fn set_font_group(&mut self, group: Option<String>) -> bool {
        if self.selected_font_group == group {
            return false;
        }
        self.selected_font_group = group;
        self.ensure_selected_font_in_group();
        if self.preview_enabled {
            self.queue_preview_render();
        }
        true
    }

    /// Arms this panel for a background font reload and hands back the token the worker
    /// must stamp its result with, plus the channel to send it on.
    ///
    /// Split out of [`Self::spawn_font_reload`] so ONE worker can serve BOTH panels
    /// (see [`spawn_shared_font_reload`]): the token is per panel (a later reload of one
    /// panel must still supersede this one for that panel only), the work is not.
    fn arm_font_reload(&mut self) -> (u64, Sender<FontReloadResult>) {
        self.latest_font_reload_token = self.latest_font_reload_token.wrapping_add(1);
        let (tx, rx) = mpsc::channel::<FontReloadResult>();
        self.font_reload_rx = Some(rx);
        self.fonts_reload_in_flight = true;
        self.status_line = t!("typing.fonts.reloading_status").to_string();
        (self.latest_font_reload_token, tx)
    }

    /// Reloads this panel's font list on a worker thread (folder fonts + imported system
    /// fonts, groups, virtual groups). The result lands in `poll_font_reload_results`.
    ///
    /// Under `#[cfg(test)]` the body early-returns WITHOUT arming anything, so no unit test
    /// scans the developer's real `fonts/` tree (or depends on what it contains); tests
    /// drive the restore contract by handing `poll_font_reload_results` a list directly.
    pub(super) fn spawn_font_reload(&mut self) {
        if cfg!(test) {
            return;
        }
        let (token, tx) = self.arm_font_reload();
        let fonts_dir = self.fonts_dir.clone();
        let imported = self.imported_system_fonts.clone();
        let _ = thread::Builder::new()
            .name("typing-font-reload-worker".to_string())
            .spawn(move || {
                let result = build_font_reload_result(token, &fonts_dir, &imported);
                let _ = tx.send(result);
            });
    }

    /// Installs a finished background font reload: the combined list, the group list, the
    /// rebuilt provider, and the selection restored BY IDENTITY.
    ///
    /// Also the point where the list becomes AUTHORITATIVE, which releases anything that
    /// had to wait for it — today the one-shot legacy-preset migration, whose font
    /// references only resolve once the imported system fonts are in the list.
    pub(super) fn poll_font_reload_results(&mut self) {
        let Some(rx) = self.font_reload_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                if result.token == self.latest_font_reload_token {
                    // A panel that has never held a real list has nothing to RESTORE: the
                    // only entry it can be sitting on is the synthetic bundled one, which
                    // is not the historical default. `None` here routes the selection
                    // through the initial-default branch below instead of "restoring" it.
                    let previous_identity = if self.font_list_is_authoritative {
                        self.active_font_identity
                            .clone()
                            .or_else(|| self.current_font_identity())
                    } else {
                        self.active_font_identity.clone()
                    };
                    self.fonts = result.fonts;
                    // Rebuild the render font source from the new list so renders and
                    // inline `<font=...>` tags resolve against the reloaded fonts.
                    self.font_provider = Arc::new(TabFontProvider::from_fonts(&self.fonts));
                    // The local-preset row previews were rendered through the PREVIOUS
                    // provider and their cache key does not carry it, so a reload (an
                    // imported, replaced or removed font) must invalidate them wholesale.
                    self.local_preset_previews.clear();
                    // The width-metric bytes were resolved by the PREVIOUS provider, so a
                    // reload (a replaced, moved or re-imported font file) invalidates them
                    // even when the identity is unchanged. Dropping them re-asks the new
                    // provider; the form window falls back to per-character widths for the
                    // frames in between, exactly as on first open.
                    self.advanced_form_font = None;
                    self.advanced_form_font_request = None;
                    self.font_groups = result.font_groups;
                    self.sync_selected_font_group();
                    // Selection survives a reload BY IDENTITY. When the identity is gone
                    // (the file was removed or its PostScript name changed), the panel
                    // enters the honest `missing_font` state instead of guessing: the old
                    // positional `min(idx, len - 1)` fallback silently handed the user a
                    // DIFFERENT font under the same slot and re-rendered with it.
                    let restored = previous_identity
                        .as_deref()
                        .and_then(|identity| self.find_font_idx_by_identity(identity));
                    match restored {
                        Some(idx) => {
                            self.selected_font_idx = idx;
                            // The font is back: the block it caused must go with it,
                            // otherwise the panel stays in the missing state forever.
                            self.missing_font = None;
                        }
                        None => {
                            if let Some(identity) = previous_identity.clone() {
                                self.missing_font = Some(identity);
                            } else {
                                // Nothing was selected yet (this is the panel's FIRST
                                // list): apply the historical default. The built-in
                                // interface font heads the list for discoverability but
                                // must not become the default, so pick the first of the
                                // user's OWN fonts and fall back to index 0 — the built-in
                                // entry — only when the user has none.
                                self.selected_font_idx = self
                                    .fonts
                                    .iter()
                                    .position(|font| font.bundled_stack_font().is_none())
                                    .unwrap_or(0);
                            }
                            // Keep the index inside the list so the rest of the panel
                            // (face combo, group filter) still has a valid anchor; the
                            // `missing_font` flag is what blocks rendering.
                            self.selected_font_idx = self
                                .selected_font_idx
                                .min(self.fonts.len().saturating_sub(1));
                        }
                    }
                    self.ensure_selected_font_in_group();
                    self.clamp_face_index();
                    // The SOUGHT identity outlives a failed restore. Overwriting it with
                    // `current_font_identity()` here would replace it with the identity of
                    // the NEIGHBOUR the clamped index landed on, so the next reload — the
                    // one where the user has put the font back — would restore that
                    // neighbour instead of the font the panel is still waiting for.
                    self.active_font_identity = match restored {
                        Some(_) => self.current_font_identity(),
                        None => previous_identity.clone().or_else(|| self.current_font_identity()),
                    };
                    self.status_line = if self.fonts.is_empty() {
                        tf!("typing.errors.no_fonts_found_reload", arg = self.fonts_dir.display())
                    } else {
                        t!("typing.preview.ready_status").to_string()
                    };
                    // Profile memory follows the RESTORED font only. After a failed restore
                    // the index points at a neighbour the user did not choose: applying its
                    // profile would overwrite the panel with a stranger's parameters, and
                    // `sync_current_font_profile_memory` would both store the missing font's
                    // parameters under the neighbour's identity and re-anchor
                    // `active_font_identity` to it — the very substitution `missing_font`
                    // exists to prevent.
                    if previous_identity.is_none() {
                        // The panel's FIRST font: there is no earlier selection whose
                        // parameters could be substituted, so only SEED the memory from the
                        // panel's current parameters — exactly what the constructor did
                        // before the font list moved off the GUI thread. Deliberately NOT
                        // the "apply the persisted default" branch below: opening a panel is
                        // not the user re-selecting the font, and applying it here would
                        // change what a fresh panel shows.
                        self.sync_current_font_profile_memory();
                    } else if self.preview_enabled
                        // In local-preset mode the font owns NOTHING: restoring its profile
                        // here would overwrite the selected local preset's parameters with a
                        // per-font snapshot the panel is not supposed to read at all
                        // (`dev-docs/local_presets_plan.md` §5).
                        && self.identity_mode == ParamIdentityMode::Font
                        && restored.is_some()
                        && let Some(identity) = self.current_font_identity()
                    {
                        if let Some(profile) = self.font_profiles_by_identity.get(&identity).cloned()
                        {
                            self.apply_render_data_json_with_options(&profile, false);
                            self.clamp_face_index();
                        } else {
                            self.sync_current_font_profile_memory();
                        }
                    }
                    // The list is now the COMBINED one, so anything that had to wait for an
                    // authoritative list may run.
                    self.font_list_is_authoritative = true;
                    if let Some(legacy) = self.pending_legacy_presets_migration.take() {
                        self.finish_legacy_presets_migration(legacy);
                    }
                    self.queue_preview_render();
                }
                self.font_reload_rx = None;
                self.fonts_reload_in_flight = false;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.font_reload_rx = None;
                self.fonts_reload_in_flight = false;
                self.status_line = t!("typing.fonts.reload_error_status").to_string();
                // A parked migration stays parked and is NOT run against the list this
                // panel happens to hold: without the imported system fonts it would resolve
                // none of their references, keep them verbatim and delete the legacy key.
                // Nothing is lost — the legacy `user_config` payload is still there and the
                // next launch retries the whole seed.
                if self.pending_legacy_presets_migration.is_some() {
                    crate::runtime_log::log_warn(
                        "typing presets: the font reload failed, so the one-shot \
                         user_config -> fonts/presets.json migration stays deferred; the \
                         legacy keys are kept and the migration is retried on the next \
                         launch.",
                    );
                }
            }
        }
    }

    pub(super) fn fonts_reload_in_flight(&self) -> bool {
        self.fonts_reload_in_flight
    }

    /// IDENTITY of the currently selected font — the panel's one selection key.
    ///
    /// `None` only when `selected_font_idx` is out of range (an empty font list). The
    /// value is the same string the renderer resolves and the document persists; a file
    /// PATH is never a key (`dev-docs/font_identity_postscript_plan.md`).
    pub(super) fn current_font_identity(&self) -> Option<String> {
        self.font_identity_name_by_idx(self.selected_font_idx)
    }

    /// Canonical render/inline-tag IDENTITY name of the font at `idx`
    /// (`render_identity_name`: the representative face's PostScript name, `%hash`-suffixed
    /// only on a same-name/different-bytes contest). This is the value that reaches the renderer and is
    /// emitted in `<font=...>` tags — NOT a display string (use `font_display_label`
    /// for the UI).
    pub(super) fn font_identity_name_by_idx(&self, idx: usize) -> Option<String> {
        self.fonts.get(idx).map(FontEntry::render_identity_name)
    }

    /// Имя шрифта для показа в списке шрифтов (комбобокс выбора шрифта): с уточнением
    /// в скобках только когда выбраны «Все группы» и имя неоднозначно; при конкретной
    /// группе — без скобок, но с учётом псевдонима шрифта в этой ВИРТУАЛЬНОЙ группе.
    ///
    /// The base name comes from `display_label_in_group(active_group)`: while a virtual
    /// group is active and this font carries a per-group alias, the alias is shown; the
    /// `(корень)/(группа)` disambiguation suffix is added ONLY when «All groups» is
    /// selected (no active group), matching the historical behavior. DISPLAY ONLY: the
    /// `label`/identity stays the render/inline-tag key — this affects only what the
    /// combo SHOWS.
    pub(super) fn font_display_label(&self, font: &FontEntry) -> String {
        let active_group = self.selected_font_group.as_deref();
        let base = font.display_label_in_group(active_group);
        match (active_group.is_none(), font.disambig.as_deref()) {
            (true, Some(suffix)) => format!("{base} ({suffix})"),
            _ => base.to_string(),
        }
    }

    /// Resolves a font by its IDENTITY (`FontEntry::render_identity_name`), compared
    /// case-insensitively. This is the strict, non-legacy lookup: it accepts nothing but
    /// the identity, so a stale selection key can never silently land on a different font.
    /// Returns `None` when no loaded font carries that identity.
    pub(super) fn find_font_idx_by_identity(&self, identity: &str) -> Option<usize> {
        let identity_norm = fonts::normalize_font_identity(identity);
        if identity_norm.is_empty() {
            return None;
        }
        self.fonts
            .iter()
            .position(|font| font_matches_identity_name(font, &identity_norm))
    }

    pub(super) fn filtered_font_indices(&self) -> Vec<usize> {
        self.fonts
            .iter()
            .enumerate()
            .filter_map(|(idx, font)| {
                if self
                    .selected_font_group
                    .as_deref()
                    .is_none_or(|group_name| font_in_group(font, group_name))
                {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    pub(super) fn sync_selected_font_group(&mut self) {
        if self
            .selected_font_group
            .as_ref()
            .is_some_and(|selected| !self.font_groups.iter().any(|group| group == selected))
        {
            self.selected_font_group = None;
        }
    }

    pub(super) fn ensure_selected_font_in_group(&mut self) {
        if self.selected_font_group.as_deref().is_none() {
            return;
        }

        let selected_group_matches = self
            .selected_font_group
            .as_deref()
            .zip(self.fonts.get(self.selected_font_idx))
            .is_some_and(|(group, font)| font_in_group(font, group));
        if selected_group_matches {
            return;
        }

        if let Some(filtered_idx) = self.filtered_font_indices().into_iter().next() {
            self.selected_font_idx = filtered_idx;
            self.selected_face_idx = 0;
        }
    }

    /// Resolves a reference persisted by an OLDER build — every stored NAME in order, then
    /// the stored `font_path` — and reports WHICH KIND of evidence matched. THE ONLY
    /// sanctioned place a path may take part in resolution.
    ///
    /// **Names decide; a path never does.** Each candidate of `font_names` is offered to
    /// [`Self::find_font_idx_by_name_forms`] (which accepts the identity and every legacy
    /// alias) before the path is looked at, and a path-only hit comes back as
    /// [`LegacyFontMatch::PathOnly`] so the caller can refuse to act on it. That is the
    /// same rule the document conversion follows (`codec::upgrade_text_params_to_v2`,
    /// safety rule D): a file that still sits at a remembered path is not proof that it is
    /// the same font — replacing `dialogue.ttf` with another face used to make the panel
    /// silently SELECT the new font, clear `missing_font`, and re-render the layer in it.
    ///
    /// Everything else in the panel keys on the identity alone; this helper exists for the
    /// conversion/read path only — a legacy `render_data` blob, an old preset key, and an
    /// inline `<font=…>` tag written before the identity became the PostScript name.
    /// Returns `None` when neither form matches any loaded font.
    pub(super) fn match_font_by_legacy_reference(
        &self,
        font_path: Option<&str>,
        font_names: &[&str],
    ) -> Option<LegacyFontMatch> {
        for name in font_names {
            if let Some(idx) = self.find_font_idx_by_name_forms(name) {
                return Some(LegacyFontMatch::ByName(idx));
            }
        }
        let path_raw = font_path?;
        self.fonts
            .iter()
            .position(|font| font_matches_path(font, path_raw))
            .map(LegacyFontMatch::PathOnly)
    }

    /// [`Self::match_font_by_legacy_reference`] for the callers that accept EITHER kind of
    /// evidence: the codec's identity resolver (which is handed a path and a name
    /// separately and classifies the outcome itself) and the preset profile-key ladder
    /// (where the stored key is the only reference that ever existed, so refusing a path
    /// would drop the profile entirely — it merely ranks below a name).
    ///
    /// A caller that SELECTS a font must use `match_font_by_legacy_reference` and reject
    /// `PathOnly` instead.
    pub(super) fn find_font_idx_by_legacy_reference(
        &self,
        font_path: Option<&str>,
        font_name: Option<&str>,
    ) -> Option<usize> {
        let names: Vec<&str> = font_name.into_iter().collect();
        self.match_font_by_legacy_reference(font_path, &names)
            .map(LegacyFontMatch::font_idx)
    }

    /// Resolves a font NAME in every form `TabFontProvider` accepts, with the SAME
    /// precedence, so the combo highlights exactly the font the renderer resolves.
    ///
    /// Ordered whole-list passes, mirroring `TabFontProvider::from_fonts`'s key insertion
    /// order (each pass scans the full list before the next form is tried):
    ///
    /// 1. the collision-aware IDENTITY (`identity_name`) — the only non-legacy form;
    /// 2. `fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY` → the synthetic bundled entry;
    /// 3. each font's own `{base}%{content hash}` stability alias;
    /// 4. the bare (unsuffixed) base name, won by the LOWEST content hash, so a
    ///    document written before the name became contested resolves the same way on
    ///    both sides; reserved bundled spellings never fall back to a user font;
    /// 5. the original family name, 6. the file-stem `label`, 7. the path stem.
    ///
    /// Forms 2-7 are READ-ONLY legacy aliases; nothing writes them any more. Returns
    /// `None` for an empty name or no match.
    pub(super) fn find_font_idx_by_name_forms(&self, font_name: &str) -> Option<usize> {
        self.find_font_idx_by_name_forms_in(font_name, None)
    }

    /// [`Self::find_font_idx_by_name_forms`] restricted to `allowed` indices when given.
    ///
    /// The restriction applies per PASS, not after the fact: an in-group font matching a
    /// weaker form still wins over an out-of-group font matching a stronger one, which is
    /// what the group-preferring inline-tag lookup wants.
    fn find_font_idx_by_name_forms_in(
        &self,
        font_name: &str,
        allowed: Option<&[usize]>,
    ) -> Option<usize> {
        let name_norm = fonts::normalize_font_identity(font_name);
        if name_norm.is_empty() {
            return None;
        }
        // One candidate scope drives every pass, so the group restriction cannot drift
        // between forms. The unrestricted case walks the list directly — this runs once
        // per drawn frame for an inline `<font=…>` span, so it must not allocate.
        let first = |predicate: &dyn Fn(&FontEntry) -> bool| match allowed {
            Some(list) => list
                .iter()
                .copied()
                .find(|&idx| self.fonts.get(idx).is_some_and(predicate)),
            None => self.fonts.iter().position(predicate),
        };

        if let Some(idx) = first(&|font| font_matches_identity_name(font, &name_norm)) {
            return Some(idx);
        }
        if fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY
            .trim()
            .eq_ignore_ascii_case(&name_norm)
            && let Some(idx) = first(&|font| font.bundled_stack_font().is_some())
        {
            return Some(idx);
        }
        if let Some(idx) = first(&|font| font_matches_own_hash_identity(font, &name_norm)) {
            return Some(idx);
        }
        // Bare contested name: the lowest content hash wins, deterministically and
        // independently of list order (the index only breaks an exact hash tie).
        let claims_base = |idx: usize| {
            self.fonts
                .get(idx)
                .is_some_and(|font| font_matches_base_identity(font, &name_norm))
        };
        // `content_hash` is read through `get`, so an out-of-range index (only possible
        // in the restricted case) sorts last instead of panicking.
        let hash_of = |idx: usize| {
            (
                self.fonts.get(idx).map_or(u64::MAX, |font| font.content_hash),
                idx,
            )
        };
        let lowest_hash_claimant = match allowed {
            Some(list) => list
                .iter()
                .copied()
                .filter(|&idx| claims_base(idx))
                .min_by_key(|&idx| hash_of(idx)),
            None => (0..self.fonts.len())
                .filter(|&idx| claims_base(idx))
                .min_by_key(|&idx| hash_of(idx)),
        };
        if let Some(idx) = lowest_hash_claimant {
            return Some(idx);
        }
        if let Some(idx) = first(&|font| font_matches_original_name(font, &name_norm)) {
            return Some(idx);
        }
        if let Some(idx) = first(&|font| font_label_matches(font, &name_norm)) {
            return Some(idx);
        }
        first(&|font| font_matches_stem(font, &name_norm))
    }

    /// Resolves a font NAME, PREFERRING a match among `allowed_indices` (the active font
    /// group) before falling back to the whole font list.
    ///
    /// Inline `<font=…>` tags may still carry an ambiguous legacy label (a file stem can
    /// appear both inside a group and globally, e.g. an imported system font colliding
    /// with a group member). When a group is selected, the in-group copy is the one the
    /// user sees and expects, so it must win; only when no group member matches does this
    /// fall back to the global lookup. Both steps use the provider-aligned form
    /// precedence. Returns `None` when the name is empty or matches nothing. A path is
    /// intentionally NOT accepted here — a tag never carried one.
    pub(super) fn find_font_idx_by_label_preferring_indices(
        &self,
        font_label: Option<&str>,
        allowed_indices: &[usize],
    ) -> Option<usize> {
        let font_label = font_label?;
        self.find_font_idx_by_name_forms_in(font_label, Some(allowed_indices))
            .or_else(|| self.find_font_idx_by_name_forms(font_label))
    }
}

/// Builds ONE background font-reload payload: the combined font list (folder fonts +
/// imported system fonts, merged, sorted and identity-assigned), with the real folder
/// groups and the user-defined virtual groups injected.
///
/// Free, and takes everything it needs by reference, so the SAME result can be produced
/// once and delivered to both panels ([`spawn_shared_font_reload`]). Runs on a worker
/// thread only: it reads, hashes and parses every font file.
fn build_font_reload_result(
    token: u64,
    fonts_dir: &Path,
    imported_system_paths: &[PathBuf],
) -> FontReloadResult {
    let mut fonts = load_fonts(fonts_dir, imported_system_paths);
    let real_font_groups = load_font_groups(fonts_dir);
    // Read the process-global virtual groups off the GUI thread (cheap) and inject them
    // into the freshly-loaded combined list.
    let virtual_groups = super::font_settings_store::virtual_groups();
    let font_groups = apply_virtual_groups(&mut fonts, &real_font_groups, &virtual_groups);
    FontReloadResult {
        token,
        fonts,
        font_groups,
    }
}

/// Starts the panels' INITIAL font load: ONE worker builds the list once and delivers a
/// clone to each panel under that panel's own reload token.
///
/// Both panels resolve the same fonts dir and the same imported-font snapshot, so loading
/// twice meant scanning, reading, hashing and parsing every font file twice for one
/// startup. Cloning the finished list is a memcpy of already-parsed metadata; re-reading
/// the files is I/O.
///
/// Each panel keeps its OWN token, so a later per-panel reload (a settings-side import, a
/// typesetting-language change) still supersedes this one for that panel alone. Under
/// `#[cfg(test)]` nothing is armed or spawned, for the same reason as
/// [`TypingCreatePanelState::spawn_font_reload`].
pub(super) fn spawn_shared_font_reload(
    create_panel: &mut TypingCreatePanelState,
    edit_panel: &mut TypingCreatePanelState,
) {
    if cfg!(test) {
        return;
    }
    let fonts_dir = create_panel.fonts_dir.clone();
    let imported = create_panel.imported_system_fonts.clone();
    let (create_token, create_tx) = create_panel.arm_font_reload();
    let (edit_token, edit_tx) = edit_panel.arm_font_reload();
    let spawn_result = thread::Builder::new()
        .name("typing-font-initial-load-worker".to_string())
        .spawn(move || {
            let result = build_font_reload_result(create_token, &fonts_dir, &imported);
            let for_edit = FontReloadResult {
                token: edit_token,
                fonts: result.fonts.clone(),
                font_groups: result.font_groups.clone(),
            };
            // A closed channel means the panel is already gone; there is nothing to hand
            // the list to and nothing was modified, so the send result is deliberately
            // ignored.
            let _ = create_tx.send(result);
            let _ = edit_tx.send(for_edit);
        });
    if let Err(err) = spawn_result {
        // Both panels stay armed with a receiver nobody will ever send on, which
        // `poll_font_reload_results` reports as a disconnected reload (a visible error
        // status), so the failure cannot pass as an empty font folder.
        crate::runtime_log::log_warn(format!(
            "typing fonts: could not spawn the initial font-load worker; both panels stay \
             without a font list until a reload is triggered again: {err}"
        ));
    }
}
