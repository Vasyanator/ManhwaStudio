/*
File: panel/local_presets.rs

Purpose:
The LOCAL-PRESET parameter identity mode of the create panel
(`dev-docs/local_presets_plan.md`). Owns the one dispatch point every parameter edit
funnels through, the local-preset operations behind the combo/name row, and the
debounced persistence of the document-level DEFAULT local set.

Main responsibilities:
- route a parameter snapshot to its owner (`store_current_params_snapshot`);
- create / select / deselect / rename / delete a local preset;
- own the LIVE-SET INVARIANT (`default_local_set_snapshot`): the live set is the selected
  GLOBAL preset's set when one is selected, and the DEFAULT set otherwise;
- persist the DEFAULT set off the GUI thread after a debounce, and keep it dirty until a
  write actually SUCCEEDS;
- install a default local set the store read or another app instance wrote;
- carry the mode and the set into and out of a saved GLOBAL preset.

Key functions:
- `TypingCreatePanelState::store_current_params_snapshot` — THE ownership dispatch.
- `TypingCreatePanelState::create_local_preset` / `select_local_preset` /
  `deselect_local_preset` / `rename_local_preset` / `delete_local_preset`.
- `TypingCreatePanelState::default_local_set_snapshot` / `park_default_local_set_for_global_preset`
  / `restore_default_local_set_after_deselect` — the live-set invariant.
- `TypingCreatePanelState::tick_local_presets_save` — the per-frame debounce tick.
- `TypingCreatePanelState::flush_pending_local_presets_save` — the app-exit flush.
- `TypingCreatePanelState::set_param_identity_mode` — the mode switch (persisted).

Notes:
`use super::*;` pulls in the parent module's types and imports. Methods are `pub(super)`
because `TypingCreatePanelState` lives in `panel.rs`. NOTHING here may read or write
`fonts_data.fonts.<identity>.profile` or `font_profiles_by_identity` — in local-preset
mode the font owns nothing (plan §5).
*/

use super::*;

/// How long the DEFAULT local set may stay dirty before it is written to
/// `fonts/presets.json`.
///
/// Same rationale as `font_settings_store::PROFILE_SAVE_DEBOUNCE`, and the same length: a
/// snapshot is rewritten on EVERY parameter edit, so an atomic document write per keystroke
/// (or per slider frame) is pure write amplification against a file that also holds every
/// global preset. The window is a bounded loss: the debounce runs from the FIRST dirtying
/// edit, not from the last, so a continuous drag still reaches disk within it.
const LOCAL_PRESETS_SAVE_DEBOUNCE: Duration = Duration::from_secs(3);

/// How many times in a row a FAILED default-local-set save re-arms itself automatically.
///
/// A retryable failure (a full disk, a locked directory, another instance rewriting the
/// document) re-arms the debounce so the edit is not lost the moment a writer dies. Without
/// a cap that is a retry every [`LOCAL_PRESETS_SAVE_DEBOUNCE`] for the rest of the session,
/// each one re-logging an error and re-writing the status line, for a condition that is not
/// getting better on its own. Reaching the cap loses NOTHING: the set stays dirty, so the
/// next edit re-arms it and the app-exit flush still writes it.
const LOCAL_PRESETS_SAVE_MAX_RETRIES: u8 = 3;

impl TypingCreatePanelState {
    /// THE ownership dispatch: stores the panel's current parameters + effect chain with
    /// whoever owns them in the current [`ParamIdentityMode`] (`dev-docs/local_presets_plan.md` §5).
    ///
    /// Every parameter-edit site funnels through here, so the two-axis rule lives in exactly
    /// one place:
    ///
    /// | mode | global preset applied | destination |
    /// |---|---|---|
    /// | `Font` | no | session profile memory + the font's PERSISTED default |
    /// | `Font` | yes | session profile memory only |
    /// | `LocalPreset` | no | the selected local preset **and** the DEFAULT set on disk (debounced) |
    /// | `LocalPreset` | yes | the selected local preset only (it reaches disk with the global preset) |
    ///
    /// With no local preset selected in `LocalPreset` mode the edit goes NOWHERE: the panel
    /// is a scratch pad. No-op without the create panel's per-font memory (`preview_enabled`).
    pub(super) fn store_current_params_snapshot(&mut self) {
        if !self.preview_enabled {
            return;
        }
        match self.identity_mode {
            ParamIdentityMode::Font => {
                self.store_current_font_profile_by_idx(self.selected_font_idx);
            }
            ParamIdentityMode::LocalPreset => self.store_current_local_preset_snapshot(),
        }
    }

    /// Stores the parameters on screen into the SELECTED local preset, and marks the default
    /// set dirty when that set is the one being edited (no global preset applied).
    ///
    /// Refuses to store while `missing_font` is set: the panel is then sitting on a
    /// NEIGHBOUR of the font it could not resolve, and writing the snapshot would silently
    /// replace the preset's own font with that neighbour — the substitution the missing-font
    /// state exists to prevent (`create_apply::select_font_by_identity`).
    fn store_current_local_preset_snapshot(&mut self) {
        let Some(idx) = self.selected_local_preset else {
            return;
        };
        if self.missing_font.is_some() {
            return;
        }
        let snapshot = self.build_font_profile_json_for_idx(self.selected_font_idx);
        let Some(preset) = self.local_presets.get_mut(idx) else {
            // The index outlived its preset (only reachable through a bug elsewhere): drop
            // the dangling selection rather than keep writing into nothing.
            self.selected_local_preset = None;
            return;
        };
        if preset.profile() == &snapshot {
            return;
        }
        preset.set_profile(snapshot);
        if self.owns_default_local_set() {
            self.mark_default_local_set_dirty();
        }
    }

    /// Whether the LIVE local set is the document-level DEFAULT set — true exactly when no
    /// global preset is applied (THE LIVE-SET INVARIANT, see
    /// [`Self::default_local_set_snapshot`]). With one applied the live set belongs to that
    /// preset and reaches disk only when the preset is saved.
    #[must_use]
    fn owns_default_local_set(&self) -> bool {
        self.selected_preset_name.is_none()
    }

    /// Records that the DEFAULT local set changed and starts (or keeps) the debounce window
    /// that `tick_local_presets_save` closes.
    ///
    /// COPIES NOTHING. This runs on every parameter edit, i.e. on every frame of a slider
    /// drag, and it used to deep-clone the whole set — every preset's full render-data JSON
    /// — on each of those frames. The set is snapshotted where it is actually needed
    /// ([`Self::default_local_set_snapshot`]), which is once per spawned write.
    fn mark_default_local_set_dirty(&mut self) {
        self.local_presets_generation = self.local_presets_generation.saturating_add(1);
        // A fresh edit restores the automatic-retry budget: the condition that failed the
        // last write may well be gone by now.
        self.local_presets_save_retries = 0;
        // Anchored at the FIRST dirtying edit of the window, so a continuous drag cannot
        // push the write out indefinitely.
        if self.local_presets_dirty_since.is_none() {
            self.local_presets_dirty_since = Some(Instant::now());
        }
    }

    /// Records the change and writes it NOW, closing any open debounce window.
    ///
    /// Used by the STRUCTURAL operations (create, delete) — they happen once per user
    /// gesture, so there is no burst to coalesce, and a set the user just built must not be
    /// lost if the app closes inside the debounce window. Parameter edits and renames stay
    /// debounced; they are the high-frequency half.
    fn flush_default_local_set_now(&mut self) {
        self.local_presets_generation = self.local_presets_generation.saturating_add(1);
        self.local_presets_save_retries = 0;
        self.local_presets_dirty_since = None;
        self.spawn_presets_save(false);
    }

    /// Whether the DEFAULT local set still owes a write.
    ///
    /// THE CLEAN/DIRTY CONTRACT: the set becomes clean only when a write actually SUCCEEDED
    /// ([`Self::note_default_local_set_saved`]), never when a writer was merely spawned. The
    /// debounce anchor cannot answer this — it is cleared at spawn time — and treating a
    /// spawn as a save is what used to drop the edits of every failed write for good.
    #[must_use]
    pub(super) fn default_local_set_is_unsaved(&self) -> bool {
        self.local_presets_saved_generation < self.local_presets_generation
    }

    /// Marks the DEFAULT local set clean up to `generation` — the generation the snapshot
    /// that just reached disk carried.
    ///
    /// `max` rather than assignment because save workers finish out of order; a stale-ticket
    /// save reports the generation of ITS snapshot, which is never newer than the one the
    /// winning write carried (both the ticket and the generation are taken on the GUI thread,
    /// in the same order).
    pub(super) fn note_default_local_set_saved(&mut self, generation: u64) {
        self.local_presets_saved_generation = self.local_presets_saved_generation.max(generation);
        if !self.default_local_set_is_unsaved() {
            self.local_presets_dirty_since = None;
            self.local_presets_save_retries = 0;
        }
    }

    /// Re-arms the debounce after a save that FAILED, so the edits it was carrying are
    /// written again instead of being silently dropped.
    ///
    /// `generation` is what the lost snapshot carried and `retryable` comes from
    /// [`presets_store::PresetsStoreError::is_retryable`]. Nothing is re-armed when a later
    /// save already put a newer state on disk, when the failure is PERMANENT (persistence
    /// disabled for the session, or a newer on-disk schema — retrying those spins forever
    /// without ever writing), or when the automatic-retry budget
    /// ([`LOCAL_PRESETS_SAVE_MAX_RETRIES`]) is spent. In all three cases the set simply stays
    /// dirty: the next edit and the app-exit flush still try.
    pub(super) fn rearm_default_local_set_after_failed_save(
        &mut self,
        generation: u64,
        retryable: bool,
    ) {
        if generation <= self.local_presets_saved_generation || !self.default_local_set_is_unsaved()
        {
            return;
        }
        if !retryable {
            crate::runtime_log::log_error(
                "typing presets: the default local preset set could not be written and the \
                 failure cannot be retried; it stays in memory only and is written again on \
                 the next edit or at exit.",
            );
            return;
        }
        if self.local_presets_save_retries >= LOCAL_PRESETS_SAVE_MAX_RETRIES {
            crate::runtime_log::log_error(format!(
                "typing presets: {LOCAL_PRESETS_SAVE_MAX_RETRIES} consecutive writes of the \
                 default local preset set failed; stopping the automatic retries. The set \
                 stays in memory and is written again on the next edit or at exit."
            ));
            return;
        }
        self.local_presets_save_retries += 1;
        if self.local_presets_dirty_since.is_none() {
            self.local_presets_dirty_since = Some(Instant::now());
        }
    }

    /// Per-frame tick of the DEFAULT local-set save: writes the document once the debounce
    /// window has elapsed. Called by `TypingTopPanelState::begin_frame`; a no-op when
    /// nothing is owed.
    ///
    /// The write itself runs off the GUI thread (`spawn_presets_save`); only the clock is
    /// read here. The anchor is cleared, but the set stays DIRTY until the write reports
    /// success — see [`Self::default_local_set_is_unsaved`].
    pub(super) fn tick_local_presets_save(&mut self) {
        let Some(since) = self.local_presets_dirty_since else {
            return;
        };
        if since.elapsed() < LOCAL_PRESETS_SAVE_DEBOUNCE {
            return;
        }
        self.local_presets_dirty_since = None;
        self.spawn_presets_save(false);
    }

    /// Writes a still-owed DEFAULT local-set save immediately, on the CALLING thread.
    /// Returns whether there was anything to flush.
    ///
    /// This is the app-exit flush (`MangaApp::on_exit`), modelled on
    /// `font_settings_store::flush_pending_saves`: the debounced writer is a detached thread
    /// that dies with the process, so renaming a local preset and closing the app a second
    /// later used to lose the rename outright.
    ///
    /// SYNCHRONOUS ON THE GUI THREAD, AND DELIBERATELY SO. This is the one accepted exception
    /// to the "never block the GUI thread" rule (CLAUDE.md §5), and it is not new: the
    /// PRE-EXISTING `font_admin::flush_pending_saves` has the identical shape at
    /// `src/app.rs:3421`, one step above this one's call site. No GUI frame follows `on_exit`
    /// and no thread outlives it, so there is no responsiveness left to protect, and the work
    /// is ONE atomic write of the same small document the debounced writer would have
    /// written. A deadline would not make it safer: it trades a hang the user can see for a
    /// SILENT loss of the edit just made, on exactly the pathological (network/FUSE) fonts
    /// directory where the write matters most, and it cannot cancel a `write`/`fsync` already
    /// in flight. The residual risk — a stalled fonts directory holding the process open at
    /// quit — is accepted and recorded in the module README.
    ///
    /// Under `#[cfg(test)]` the anchor is cleared but nothing is written (a unit test must
    /// never touch the real fonts directory); the write recipe is covered by `presets_store`
    /// and by `create_presets::run_presets_save`, which tests drive synchronously.
    pub(super) fn flush_pending_local_presets_save(&mut self) -> bool {
        if !self.preview_enabled || !self.default_local_set_is_unsaved() {
            return false;
        }
        self.local_presets_dirty_since = None;
        if cfg!(test) {
            return true;
        }
        let document = presets_store::StoredDocument {
            presets: self.presets_by_name.clone(),
            default_local: self.default_local_set_snapshot(),
        };
        create_presets::run_presets_save(
            &self.fonts_dir,
            &document,
            presets_store::next_save_ticket(),
            self.local_presets_generation,
            // No legacy-config cleanup: that ordering belongs to the migration alone.
            None,
            &self.preset_store_tx,
        );
        true
    }

    /// Rendered preview of one local-preset COMBO ROW, requesting it from the off-thread
    /// renderer on first sight. `None` for an out-of-range index.
    ///
    /// `row_height_px` is the row's height in PHYSICAL pixels — the preview is downscaled to
    /// it off the GUI thread, so it must be the size the row will actually paint at. The
    /// drawn text is the preset's display name capped to `PREVIEW_NAME_MAX_CHARS`; a caller
    /// that needs the same string for the text fallback gets it from
    /// [`local_preset_preview::preview_label`] over [`Self::local_preset_display_name`].
    ///
    /// Fields are split out of `self` on purpose: the cache is borrowed mutably while the
    /// preset's profile and the font provider are borrowed shared, which the borrow checker
    /// only accepts as three DISJOINT field borrows.
    pub(super) fn local_preset_row_preview(
        &mut self,
        index: usize,
        row_height_px: f32,
    ) -> Option<local_preset_preview::LocalPresetPreview<'_>> {
        let Self {
            local_presets,
            local_preset_previews,
            font_provider,
            ..
        } = self;
        let preset = local_presets.get(index)?;
        let name = if preset.name.is_empty() {
            t!("typing.local_presets.unnamed").to_string()
        } else {
            preset.name.clone()
        };
        Some(local_preset_previews.preview(
            &name,
            preset.profile(),
            preset.profile_hash(),
            font_provider,
            row_height_px,
        ))
    }

    /// Per-frame pump of the local-preset row previews: uploads whatever the off-thread
    /// renderer finished into egui textures. Must run BEFORE the combo rows are drawn, so a
    /// preview that landed this frame is already available to them.
    pub(super) fn poll_local_preset_previews(&mut self, ctx: &egui::Context) {
        self.local_preset_previews.poll(ctx);
    }

    /// Switches the parameter identity mode and persists the choice
    /// (`user_config.TextTab.param_identity_mode`, plan §3 D7).
    ///
    /// NOTHING ON SCREEN CHANGES (plan §2, fixed decision 4) — the mode only decides who owns
    /// the NEXT edit. Entering `LocalPreset` mode therefore selects no local preset: the
    /// parameters on screen came from the font and belong to no preset, and auto-selecting
    /// one would attribute them to it on the next keystroke.
    pub(super) fn set_param_identity_mode(&mut self, mode: ParamIdentityMode) {
        if self.identity_mode == mode {
            return;
        }
        // The outgoing owner keeps everything edited so far.
        self.store_current_params_snapshot();
        self.identity_mode = mode;
        if mode == ParamIdentityMode::LocalPreset {
            self.selected_local_preset = None;
            self.local_preset_name_input.clear();
        }
        self.persist_param_identity_mode();
    }

    /// Writes the current mode to `user_config.json` off the GUI thread.
    ///
    /// A failed spawn or a failed write is LOGGED, never swallowed: the loss is one
    /// remembered preference, so it must not interrupt the user, but it must be
    /// diagnosable. Test-gated like every other config writer in this module — a unit test
    /// must not touch the developer's real `user_config.json`.
    pub(super) fn persist_param_identity_mode(&self) {
        if cfg!(test) {
            return;
        }
        let mode = self.identity_mode;
        let spawn_result = thread::Builder::new()
            .name("typing-save-param-identity-mode".to_string())
            .spawn(move || {
                if let Err(err) = save_text_tab_param_identity_mode(mode) {
                    crate::runtime_log::log_warn(format!(
                        "typing: could not persist the parameter identity mode \
                         '{}': {err}",
                        mode.as_config_str()
                    ));
                }
            });
        if let Err(err) = spawn_result {
            crate::runtime_log::log_warn(format!(
                "typing: could not spawn the param_identity_mode writer thread; the mode \
                 stays session-only: {err}"
            ));
        }
    }

    /// Creates a local preset carrying nothing but DEFAULTS, appends it, selects it and
    /// stores it (plan §2 fixed decision 7, §3 D1/D2).
    ///
    /// "Defaults" is `text_params_schema::frozen_v2_defaults()`, reached by applying a bare
    /// `{"text_params": {"schema": 2}}`: the schema reader fills every frozen default and
    /// the absent `effects` key clears the whole chain. NOTHING is carried over from the
    /// previously selected preset — that is an explicit user requirement.
    ///
    /// The CONTENT is not a parameter (D2): the current `text` and `formed_text` survive,
    /// although their frozen default is the empty string. The font selection is NOT applied
    /// from the defaults payload (there is no font in it); the preset simply records
    /// whichever font is selected, which is what makes the font an ordinary parameter here.
    pub(super) fn create_local_preset(&mut self) {
        if !self.preview_enabled {
            return;
        }
        // A LOCAL PRESET MUST NEVER BE STORED WITHOUT A SNAPSHOT. While `missing_font` is
        // set the panel sits on a NEIGHBOUR of a font it could not resolve, so
        // `store_current_local_preset_snapshot` refuses to write — and the preset created
        // here would be persisted with a null profile: a row that renders nothing, restores
        // nothing and cannot be repaired. Refusing, and saying so in the status line the
        // missing font is already reported through, is the only honest answer.
        if self.missing_font.is_some() {
            self.status_line = t!("typing.local_presets.create_blocked_missing_font").to_string();
            return;
        }
        // The outgoing preset keeps whatever was edited since its last store.
        self.store_current_params_snapshot();
        let text = self.text.clone();
        let formed_text = self.formed_text.clone();
        self.apply_render_data_json_with_options(&json!({"text_params": {"schema": 2}}), false);
        self.text = text;
        self.formed_text = formed_text;
        self.clamp_face_index();

        let name = self.free_local_preset_default_name();
        // The snapshot is filled by the store below, which is the ONE place one is built;
        // the guard at the top of this function is what guarantees that store cannot refuse.
        self.local_presets
            .push(LocalPreset::new(name.clone(), Value::Null));
        self.selected_local_preset = Some(self.local_presets.len().saturating_sub(1));
        self.local_preset_name_input = name;
        self.store_current_params_snapshot();
        if self.owns_default_local_set() {
            self.flush_default_local_set_now();
        }
        self.queue_preview_render();
    }

    /// Default name for a new local preset: `typing.local_presets.default_name` with the
    /// lowest 1-based index that no existing local preset already carries.
    ///
    /// Names are user data and may repeat (D3), so the uniqueness is a courtesy for the
    /// COMBO, not an invariant — nothing downstream depends on it.
    #[must_use]
    fn free_local_preset_default_name(&self) -> String {
        let mut index = self.local_presets.len() + 1;
        loop {
            let candidate = tf!("typing.local_presets.default_name", index = index);
            if !self
                .local_presets
                .iter()
                .any(|preset| preset.name == candidate)
            {
                return candidate;
            }
            index += 1;
        }
    }

    /// Selects the local preset at `index` and applies its whole snapshot — parameters,
    /// effect chain, FONT and face.
    ///
    /// The outgoing preset is stored first, so nothing edited since its last store is lost.
    /// The font selection IS applied here (unlike a font-profile restore, where the font is
    /// the key rather than a value): a preset naming a font this machine does not have lands
    /// in the same `missing_font` state an overlay load produces
    /// (`create_apply::select_font_by_identity`, and the MISSING PRIMARY FONT rule of
    /// `apply_preset_by_name`) — never a silent substitution.
    ///
    /// Out-of-range indices and re-selecting the current preset are no-ops.
    pub(super) fn select_local_preset(&mut self, index: usize) {
        if !self.preview_enabled || self.selected_local_preset == Some(index) {
            return;
        }
        let Some(profile) = self
            .local_presets
            .get(index)
            .map(|preset| preset.profile().clone())
        else {
            return;
        };
        self.store_current_params_snapshot();
        self.selected_local_preset = Some(index);
        self.local_preset_name_input = self
            .local_presets
            .get(index)
            .map(|preset| preset.name.clone())
            .unwrap_or_default();
        self.apply_render_data_json_with_options(&profile, true);
        self.clamp_face_index();
        if self.owns_default_local_set() {
            self.mark_default_local_set_dirty();
        }
        self.queue_preview_render();
    }

    /// Clears the local-preset selection. The parameters ON SCREEN do not change — the panel
    /// simply becomes a scratch pad whose edits belong to nobody.
    pub(super) fn deselect_local_preset(&mut self) {
        if self.selected_local_preset.is_none() {
            return;
        }
        // The outgoing preset keeps what is on screen; only the OWNERSHIP is dropped.
        self.store_current_params_snapshot();
        self.selected_local_preset = None;
        self.local_preset_name_input.clear();
        if self.owns_default_local_set() {
            self.mark_default_local_set_dirty();
        }
    }

    /// Renames the local preset at `index`, VERBATIM.
    ///
    /// No trimming, no case folding, no duplicate check and no empty-name check: a local
    /// preset is addressed by its INDEX (D3), so its name is pure user data. Out-of-range
    /// indices are a no-op. Does NOT touch `local_preset_name_input` — that field is the
    /// text box this call comes FROM.
    pub(super) fn rename_local_preset(&mut self, index: usize, name: String) {
        let Some(preset) = self.local_presets.get_mut(index) else {
            return;
        };
        if preset.name == name {
            return;
        }
        preset.name = name;
        if self.owns_default_local_set() {
            self.mark_default_local_set_dirty();
        }
    }

    /// Deletes the local preset at `index` and fixes the selection up.
    ///
    /// THE SELECTION IS AN INDEX, so removing an entry shifts everything behind it:
    /// - deleting the SELECTED preset clears the selection (`None`). The parameters on
    ///   screen are left exactly as they are, and the panel becomes a scratch pad — any
    ///   other choice would silently re-attribute them to a preset the user did not pick
    ///   and overwrite it on the next keystroke;
    /// - deleting a preset BEFORE the selected one moves the selection one step back, so it
    ///   keeps pointing at the SAME preset;
    /// - deleting a preset after it leaves the selection alone.
    ///
    /// An out-of-range index is a no-op.
    pub(super) fn delete_local_preset(&mut self, index: usize) {
        if index >= self.local_presets.len() {
            return;
        }
        self.local_presets.remove(index);
        self.selected_local_preset = match self.selected_local_preset {
            Some(selected) if selected == index => {
                self.local_preset_name_input.clear();
                None
            }
            Some(selected) if selected > index => Some(selected - 1),
            other => other,
        };
        if self.owns_default_local_set() {
            self.flush_default_local_set_now();
        }
    }

    /// Display name of the local preset at `index` for the combo and the row previews: its
    /// own name, or the `typing.local_presets.unnamed` placeholder when the user left it
    /// empty. `None` for an out-of-range index.
    #[must_use]
    pub(super) fn local_preset_display_name(&self, index: usize) -> Option<String> {
        self.local_presets.get(index).map(|preset| {
            if preset.name.is_empty() {
                t!("typing.local_presets.unnamed").to_string()
            } else {
                preset.name.clone()
            }
        })
    }

    /// The snapshot a `presets.json` write must carry as the document-level DEFAULT local
    /// set.
    ///
    /// THE LIVE-SET INVARIANT, in one place: the LIVE set (`local_presets` +
    /// `selected_local_preset`) is the selected GLOBAL preset's set when one is selected, and
    /// the DEFAULT set otherwise. So with no global preset applied the live set IS what a
    /// write must carry; with one applied the default set is PARKED in `default_local_set`
    /// (see [`Self::park_default_local_set_for_global_preset`]) and writing the live set here
    /// would overwrite the user's own default set with a preset's.
    ///
    /// The clone happens HERE, once per spawned write, and deliberately nowhere else — see
    /// [`Self::mark_default_local_set_dirty`].
    #[must_use]
    pub(super) fn default_local_set_snapshot(&self) -> presets_store::DefaultLocalSet {
        if self.owns_default_local_set() {
            return presets_store::DefaultLocalSet {
                local_presets: self.local_presets.clone(),
                selected_local_preset: self.selected_local_preset,
            };
        }
        self.default_local_set.clone()
    }

    /// Parks the DEFAULT local set before a GLOBAL preset takes ownership of the live set.
    ///
    /// Must be called on every `no global preset` → `global preset` transition, in BOTH
    /// identity modes, and BEFORE `selected_preset_name` becomes `Some` — the two call sites
    /// are `create_presets::apply_preset_by_name` and `create_presets::save_current_preset`.
    /// A no-op while a preset is already applied: the live set is that preset's by then, and
    /// parking it would overwrite the user's default set with a preset's.
    pub(super) fn park_default_local_set_for_global_preset(&mut self) {
        if !self.owns_default_local_set() {
            return;
        }
        self.default_local_set = presets_store::DefaultLocalSet {
            local_presets: self.local_presets.clone(),
            selected_local_preset: self.selected_local_preset,
        };
    }

    /// Installs the DEFAULT local set the startup read found.
    ///
    /// Follows `install_seeded_presets`' discipline: what the PANEL already has wins, so a
    /// set the user built before the read landed is never replaced by the stored one. An
    /// empty incoming set is nothing to install. Which slot is the default set follows the
    /// live-set invariant of [`Self::default_local_set_snapshot`].
    ///
    /// THE STORED SELECTION IS DELIBERATELY NOT RESTORED. Restoring it would leave a preset
    /// selected while the panel shows its own fresh defaults (applying the snapshot is
    /// impossible here — the font list is still loading, so the font it names could not be
    /// resolved), and the first parameter edit would then overwrite that preset with the
    /// defaults. The set is listed, nothing is selected, and one click restores everything.
    pub(super) fn install_seeded_default_local_set(
        &mut self,
        seeded: presets_store::DefaultLocalSet,
    ) {
        if seeded.local_presets.is_empty() {
            return;
        }
        if self.owns_default_local_set() {
            if !self.local_presets.is_empty() {
                return;
            }
            self.local_presets = seeded.local_presets;
            self.selected_local_preset = None;
            self.local_preset_name_input.clear();
            return;
        }
        // A global preset is applied, so the default set is the PARKED one.
        if self.default_local_set.local_presets.is_empty() {
            self.default_local_set = presets_store::DefaultLocalSet {
                local_presets: seeded.local_presets,
                selected_local_preset: None,
            };
        }
    }

    /// Adopts DEFAULT local presets a save found on disk and APPENDED to the document
    /// (`presets_store::SaveReport::appended_default_local`).
    ///
    /// They are already part of the document that was just written, so the panel must take
    /// them over or its next snapshot would drop them again — the same rule the merged
    /// GLOBAL presets follow. Appended at the END, in the order the save appended them, so
    /// the user's own order and selection index are untouched. Nothing is marked dirty: the
    /// panel is catching up WITH disk, not diverging from it.
    ///
    /// The ID comparison goes through `presets_store::same_local_preset`, the same rule the
    /// merge used: the panel may already hold the very row that was appended (it adopted it
    /// after an earlier save), and a row is the same row when its stable id matches, however
    /// its name or snapshot has since diverged. Ours is then kept as it is — the panel is the
    /// newer writer and the user is looking at it.
    pub(super) fn adopt_appended_default_local_presets(&mut self, appended: Vec<LocalPreset>) {
        if appended.is_empty() {
            return;
        }
        let into_live_set = self.owns_default_local_set();
        let target = if into_live_set {
            &mut self.local_presets
        } else {
            &mut self.default_local_set.local_presets
        };
        for preset in appended {
            if !target
                .iter()
                .any(|ours| presets_store::same_local_preset(ours, &preset))
            {
                target.push(preset);
            }
        }
    }

    /// Installs the local-preset payload of a GLOBAL preset (called by
    /// `apply_preset_by_name` for EVERY preset, whichever identity mode it was saved in).
    ///
    /// The live set becomes that preset's set — the EMPTY set of a font-mode preset
    /// included, which is what keeps the live-set invariant of
    /// [`Self::default_local_set_snapshot`] true in both modes. The preset's remembered
    /// selection — validated against the set's length — is applied whole, font included,
    /// exactly as [`Self::select_local_preset`] would. A `None` selection changes NOTHING on
    /// screen: the global preset simply hands the panel a list to pick from.
    pub(super) fn apply_local_preset_payload(
        &mut self,
        local_presets: Vec<LocalPreset>,
        selected: Option<usize>,
    ) {
        // Installed WHOLE, empty payload included: a font-mode global preset carries no local
        // presets, and the live-set invariant says the live set is the applied preset's set
        // whatever mode it was saved in. Leaving the previous set in place would hand the
        // panel the DEFAULT set while a global preset owns it.
        self.local_presets = local_presets;
        self.selected_local_preset = selected.filter(|idx| *idx < self.local_presets.len());
        self.local_preset_name_input = self
            .selected_local_preset
            .and_then(|idx| self.local_presets.get(idx))
            .map(|preset| preset.name.clone())
            .unwrap_or_default();
        if let Some(profile) = self
            .selected_local_preset
            .and_then(|idx| self.local_presets.get(idx))
            .map(|preset| preset.profile().clone())
        {
            self.apply_render_data_json_with_options(&profile, true);
            self.clamp_face_index();
        }
    }

    /// Restores the panel's OWN local set after a global preset was deselected («Нет»).
    ///
    /// Called in BOTH identity modes, and unconditionally — the live-set invariant of
    /// [`Self::default_local_set_snapshot`] does not mention the identity mode, and gating
    /// this on it was a real data-loss path: apply a local-mode preset, switch the mode to
    /// «Шрифт», deselect, switch back — the panel then owned the DEFAULT set while holding
    /// the PRESET's, and the first edit persisted the preset's set over the user's own.
    ///
    /// THE PARKED SELECTION COMES BACK WITH THE ENTRIES. Parking stores both halves of the
    /// default set (`park_default_local_set_for_global_preset`), so restoring only the
    /// entries did not restore the set: it dropped the selection, marked the set dirty and
    /// PERSISTED that loss — a user who had a local preset selected, applied a global preset
    /// and dropped it again found `selected_local_preset: null` on disk. The index is
    /// validated against the restored list, because that list is the one it indexes.
    ///
    /// IN `LocalPreset` MODE THE RESTORED PRESET'S SNAPSHOT IS APPLIED, exactly as
    /// [`Self::select_local_preset`] would apply it, which is what makes this the true
    /// inverse of the park: the panel goes back to the preset it was on. Restoring the
    /// selection WITHOUT its parameters would be worse than dropping it — the global
    /// preset's parameters would be sitting on screen owned by a default local preset, and
    /// the next keystroke would overwrite that preset with them.
    ///
    /// In `Font` mode nothing is applied and NOTHING ON SCREEN CHANGES (plan §2, decision 4:
    /// the mode owns the parameters, and local presets own nothing there). The selection is
    /// still restored — it is the set's own state, it is what the document holds, and it is
    /// inert until the mode changes; entering `LocalPreset` mode clears it anyway
    /// ([`Self::set_param_identity_mode`]).
    ///
    /// Nothing is marked dirty: the set is restored exactly as it was parked, so it matches
    /// what is on disk and owes no write.
    pub(super) fn restore_default_local_set_after_deselect(&mut self) {
        let parked = std::mem::take(&mut self.default_local_set);
        self.local_presets = parked.local_presets;
        self.selected_local_preset = parked
            .selected_local_preset
            .filter(|idx| *idx < self.local_presets.len());
        self.local_preset_name_input = self
            .selected_local_preset
            .and_then(|idx| self.local_presets.get(idx))
            .map(|preset| preset.name.clone())
            .unwrap_or_default();
        if self.identity_mode != ParamIdentityMode::LocalPreset {
            return;
        }
        let Some(profile) = self
            .selected_local_preset
            .and_then(|idx| self.local_presets.get(idx))
            .map(|preset| preset.profile().clone())
        else {
            return;
        };
        self.apply_render_data_json_with_options(&profile, true);
        self.clamp_face_index();
        self.queue_preview_render();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `RawFontFile` carrying exactly one face with the given PostScript name — the
    /// minimum a panel needs to resolve a font IDENTITY without touching a real file.
    fn raw_font(path: &str, original_name: &str, post_script_name: &str, hash: u64) -> RawFontFile {
        RawFontFile {
            path: PathBuf::from(path),
            stem: PathBuf::from(path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string(),
            group: None,
            content_hash: hash,
            faces: vec![FontFaceEntry {
                label: format!("#0 {original_name} | Normal | w400 | {post_script_name}"),
                face_index: 0,
                post_script_name: post_script_name.to_string(),
            }],
            coverage: FontLanguageCoverage::default(),
            original_name: original_name.to_string(),
        }
    }

    /// A create panel with two resolvable fonts and no disk behind it.
    fn panel_with_fonts() -> TypingCreatePanelState {
        let mut state = TypingCreatePanelState::new(false);
        // `preview_enabled` is what makes it the CREATE panel; the constructor argument also
        // spawns the preset seed, which a test must not do.
        state.preview_enabled = true;
        let mut fonts = merge_duplicate_fonts(vec![
            raw_font("/fonts/alpha.ttf", "Alpha", "Alpha-Regular", 1),
            raw_font("/fonts/beta.ttf", "Beta", "Beta-Regular", 2),
        ]);
        assign_font_identity_names(&mut fonts);
        state.fonts = fonts;
        state.selected_font_idx = 0;
        state.active_font_identity = state.current_font_identity();
        state
    }

    /// Font size stored in a snapshot, for asserting WHICH snapshot was written.
    fn snapshot_font_size(profile: &Value) -> Option<f64> {
        profile
            .get("text_params")
            .and_then(Value::as_object)
            .and_then(|params| params.get("font_size_px"))
            .and_then(Value::as_f64)
    }

    /// `Font` mode, no global preset: the edit reaches the font's session profile AND is
    /// marked for the font's persisted default — the historical behaviour, unchanged.
    #[test]
    fn font_mode_without_a_global_preset_writes_the_font_profile() {
        let mut state = panel_with_fonts();
        state.font_size_px = 41.0;

        state.store_current_params_snapshot();

        let identity = state.current_font_identity().expect("a font is selected");
        let stored = state
            .font_profiles_by_identity
            .get(&identity)
            .expect("the font profile must be stored");
        assert_eq!(snapshot_font_size(stored), Some(41.0));
        assert!(
            state.local_presets.is_empty(),
            "font mode must not create local presets"
        );
    }

    /// `Font` mode with a global preset applied: the session map still takes the snapshot
    /// (the preset's working set), and nothing local is touched.
    #[test]
    fn font_mode_with_a_global_preset_still_writes_the_session_map() {
        let mut state = panel_with_fonts();
        state.selected_preset_name = Some("П".to_string());
        state.font_size_px = 23.0;

        state.store_current_params_snapshot();

        let identity = state.current_font_identity().expect("a font is selected");
        assert_eq!(
            state
                .font_profiles_by_identity
                .get(&identity)
                .as_ref()
                .and_then(|profile| snapshot_font_size(profile)),
            Some(23.0),
        );
    }

    /// `LocalPreset` mode, no global preset: the edit lands in the SELECTED local preset and
    /// in the mirrored default set, and the per-font memory is left completely alone.
    #[test]
    fn local_preset_mode_without_a_global_preset_writes_the_preset_and_the_default_set() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        state.font_size_px = 77.0;

        state.store_current_params_snapshot();

        let selected = state.selected_local_preset.expect("create selects its preset");
        assert_eq!(
            snapshot_font_size(state.local_presets[selected].profile()),
            Some(77.0),
        );
        assert_eq!(
            state
                .default_local_set_snapshot()
                .local_presets
                .get(selected)
                .and_then(|preset| snapshot_font_size(preset.profile())),
            Some(77.0),
            "with no global preset applied the LIVE set is what a write carries",
        );
        let identity = state.current_font_identity().expect("a font is selected");
        assert!(
            state.font_profiles_by_identity.get(&identity).is_none(),
            "local-preset mode must never touch the per-font memory",
        );
    }

    /// `LocalPreset` mode with a global preset applied: the local preset takes the snapshot,
    /// but the DEFAULT set does not — it reaches disk only when the global preset is saved.
    #[test]
    fn local_preset_mode_with_a_global_preset_leaves_the_default_set_alone() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        state.font_size_px = 7.0;
        state.store_current_params_snapshot();
        // The real transition: parking the default set is what a global preset application
        // does before it takes ownership of the live set.
        state.park_default_local_set_for_global_preset();
        state.selected_preset_name = Some("П".to_string());
        let default_before = state.default_local_set_snapshot();
        state.local_presets_dirty_since = None;
        state.font_size_px = 12.0;

        state.store_current_params_snapshot();

        let selected = state.selected_local_preset.expect("create selects its preset");
        assert_eq!(
            snapshot_font_size(state.local_presets[selected].profile()),
            Some(12.0),
        );
        assert_eq!(
            state
                .default_local_set_snapshot()
                .local_presets
                .get(selected)
                .and_then(|preset| snapshot_font_size(preset.profile())),
            default_before
                .local_presets
                .get(selected)
                .and_then(|preset| snapshot_font_size(preset.profile())),
            "a global preset owns the live set; the default set must not follow it",
        );
        assert!(
            state.local_presets_dirty_since.is_none(),
            "no default-set write may be owed while a global preset is applied",
        );
    }

    /// With NO local preset selected the panel is a scratch pad: the edit is stored nowhere.
    #[test]
    fn local_preset_mode_without_a_selection_stores_nothing() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.local_presets.push(LocalPreset::new(
            "П".to_string(),
            json!({"text_params": {"font_size_px": 5.0}}),
        ));
        state.font_size_px = 99.0;

        state.store_current_params_snapshot();

        assert_eq!(
            snapshot_font_size(state.local_presets[0].profile()),
            Some(5.0),
            "an unselected preset must not absorb the scratch pad",
        );
        let identity = state.current_font_identity().expect("a font is selected");
        assert!(state.font_profiles_by_identity.get(&identity).is_none());
    }

    /// A new local preset starts from the FROZEN schema defaults, never from whatever the
    /// previously selected preset carried (plan §2, fixed decision 7).
    #[test]
    fn a_new_local_preset_carries_defaults_not_the_previous_selection() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        state.font_size_px = 123.0;
        state.uppercase_text = true;
        state.store_current_params_snapshot();

        state.create_local_preset();

        let defaults = text_params_schema::frozen_v2_defaults();
        let default_size = defaults
            .get("font_size_px")
            .and_then(Value::as_f64)
            .expect("the frozen defaults name a font size");
        assert!(
            (f64::from(state.font_size_px) - default_size).abs() < 1e-6,
            "a new preset starts from the frozen defaults, not from {}",
            state.font_size_px,
        );
        assert!(
            !state.uppercase_text,
            "nothing may be carried over from the previous selection",
        );
        assert_eq!(state.local_presets.len(), 2);
        assert_eq!(state.selected_local_preset, Some(1));
    }

    /// The CONTENT is not a parameter (D2): creating a preset keeps the text on screen even
    /// though the frozen default for both text fields is the empty string.
    #[test]
    fn creating_a_local_preset_keeps_the_text_and_the_formed_text() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.text = "Живой текст".to_string();
        state.formed_text = "Форма".to_string();

        state.create_local_preset();

        assert_eq!(state.text, "Живой текст");
        assert_eq!(state.formed_text, "Форма");
    }

    /// Creating a preset also clears the effect chain — it is a parameter, not content.
    #[test]
    fn creating_a_local_preset_clears_the_effect_chain() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.effects = vec![TypingCreatePanelState::default_effect_card(
            AvailableEffectKind::Stroke,
            Color32::BLACK,
        )];

        state.create_local_preset();

        assert!(state.effects.is_empty());
    }

    /// Deleting shifts every index behind the removed entry, so the selection has to follow.
    #[test]
    fn deleting_a_local_preset_fixes_the_selection_index_up() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        for _ in 0..3 {
            state.create_local_preset();
        }
        // Deleting a preset BEFORE the selected one keeps the selection on the SAME preset.
        state.select_local_preset(2);
        let selected_name = state.local_presets[2].name.clone();
        state.delete_local_preset(0);
        assert_eq!(state.selected_local_preset, Some(1));
        assert_eq!(state.local_presets[1].name, selected_name);

        // Deleting a preset AFTER it leaves the selection where it is.
        state.select_local_preset(0);
        state.delete_local_preset(1);
        assert_eq!(state.selected_local_preset, Some(0));

        // Deleting the SELECTED preset clears the selection.
        state.delete_local_preset(0);
        assert_eq!(state.selected_local_preset, None);
        assert!(state.local_presets.is_empty());
        assert!(state.local_preset_name_input.is_empty());

        // An out-of-range index is a no-op.
        state.delete_local_preset(7);
        assert!(state.local_presets.is_empty());
    }

    /// Selecting applies the incoming preset WHOLE and stores the outgoing one first.
    #[test]
    fn selecting_a_local_preset_stores_the_outgoing_one_and_applies_the_incoming_one() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        state.font_size_px = 31.0;
        state.store_current_params_snapshot();
        state.create_local_preset();
        state.font_size_px = 64.0;

        state.select_local_preset(0);

        assert!(
            (state.font_size_px - 31.0).abs() < 1e-6,
            "the incoming preset's parameters must be on screen",
        );
        assert_eq!(
            snapshot_font_size(state.local_presets[1].profile()),
            Some(64.0),
            "the outgoing preset must keep what was edited into it",
        );
    }

    /// A name is user data: stored verbatim, duplicates and empty names allowed.
    #[test]
    fn renaming_a_local_preset_is_verbatim() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        state.create_local_preset();

        state.rename_local_preset(0, "  Пробелы  ".to_string());
        state.rename_local_preset(1, "  Пробелы  ".to_string());

        assert_eq!(state.local_presets[0].name, "  Пробелы  ");
        assert_eq!(state.local_presets[1].name, "  Пробелы  ");

        state.rename_local_preset(0, String::new());
        assert!(state.local_presets[0].name.is_empty());
        assert_eq!(
            state.local_preset_display_name(0),
            Some(t!("typing.local_presets.unnamed").to_string()),
            "an empty name shows the placeholder, it is not rejected",
        );
    }

    /// Deselecting drops the ownership without changing anything on screen.
    #[test]
    fn deselecting_a_local_preset_keeps_the_parameters_on_screen() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        state.font_size_px = 55.0;

        state.deselect_local_preset();

        assert!((state.font_size_px - 55.0).abs() < 1e-6);
        assert_eq!(state.selected_local_preset, None);
        assert_eq!(
            snapshot_font_size(state.local_presets[0].profile()),
            Some(55.0),
            "the outgoing preset keeps the edit that was made while it was selected",
        );
    }

    /// Switching the identity mode changes NOTHING on screen and selects no local preset
    /// (plan §2, fixed decision 4).
    #[test]
    fn switching_the_identity_mode_changes_nothing_on_screen() {
        let mut state = panel_with_fonts();
        state.font_size_px = 44.0;
        state.text = "Текст".to_string();

        state.set_param_identity_mode(ParamIdentityMode::LocalPreset);

        assert!((state.font_size_px - 44.0).abs() < 1e-6);
        assert_eq!(state.text, "Текст");
        assert_eq!(state.selected_local_preset, None);
        assert_eq!(state.identity_mode, ParamIdentityMode::LocalPreset);
    }

    /// The stored default set is installed, but nothing is SELECTED by it — see
    /// `install_seeded_default_local_set`.
    #[test]
    fn a_seeded_default_local_set_is_installed_without_a_selection() {
        let mut state = panel_with_fonts();
        state.install_seeded_default_local_set(presets_store::DefaultLocalSet {
            local_presets: vec![LocalPreset::new(
                "С диска".to_string(),
                json!({"text_params": {"font_size_px": 9.0}}),
            )],
            selected_local_preset: Some(0),
        });

        assert_eq!(state.local_presets.len(), 1);
        assert_eq!(state.local_presets[0].name, "С диска");
        assert_eq!(state.selected_local_preset, None);
        assert_eq!(state.default_local_set.selected_local_preset, None);
    }

    /// A GLOBAL preset saved in local-preset mode carries the mode, the whole set and the
    /// selection — and NO font and NO per-font profiles, which are the other mode's payload.
    /// Applying it puts all three back and restores the selected preset's parameters.
    #[test]
    fn a_global_preset_round_trips_the_mode_and_the_local_set() {
        let mut state = panel_with_fonts();
        state.set_param_identity_mode(ParamIdentityMode::LocalPreset);
        state.create_local_preset();
        state.font_size_px = 88.0;
        state.store_current_params_snapshot();
        state.preset_name_input = "Локальный".to_string();

        state.save_current_preset();

        let saved = state
            .presets_by_name
            .get("Локальный")
            .expect("the preset must be saved under the typed name");
        assert_eq!(saved.identity_mode, ParamIdentityMode::LocalPreset);
        assert!(
            saved.font.is_empty() && saved.font_profiles.is_empty(),
            "the font-mode payload must stay empty, or apply would take the missing-font path",
        );
        assert_eq!(saved.local_presets.len(), 1);
        assert_eq!(saved.selected_local_preset, Some(0));

        // Wipe the panel back to a different state and re-apply the preset.
        state.selected_preset_name = None;
        state.identity_mode = ParamIdentityMode::Font;
        state.local_presets.clear();
        state.selected_local_preset = None;
        state.font_size_px = 10.0;

        state.apply_preset_by_name("Локальный".to_string());

        assert_eq!(state.identity_mode, ParamIdentityMode::LocalPreset);
        assert_eq!(state.local_presets.len(), 1);
        assert_eq!(state.selected_local_preset, Some(0));
        assert!(
            (state.font_size_px - 88.0).abs() < 1e-6,
            "the selected local preset's parameters must be applied",
        );
    }

    /// The `Font` mode half is unchanged: a preset saved there carries the font and the
    /// per-font profiles, and an EMPTY local set — the other mode's payload is not written.
    #[test]
    fn a_font_mode_global_preset_stores_no_local_presets() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        state.identity_mode = ParamIdentityMode::Font;
        state.font_size_px = 17.0;
        state.preset_name_input = "Шрифтовой".to_string();

        state.save_current_preset();

        let saved = state
            .presets_by_name
            .get("Шрифтовой")
            .expect("the preset must be saved under the typed name");
        assert_eq!(saved.identity_mode, ParamIdentityMode::Font);
        assert_eq!(saved.font, "Alpha-Regular");
        assert_eq!(
            saved.font_profiles.len(),
            1,
            "the session profile memory is the font-mode payload",
        );
        assert!(
            saved.local_presets.is_empty() && saved.selected_local_preset.is_none(),
            "font mode must not smuggle the local set into the preset",
        );
    }

    /// A GLOBAL preset carrying its own local set, for the ownership-transition tests.
    fn global_preset_with_local_set(names: &[&str]) -> TypingCreatePreset {
        TypingCreatePreset {
            identity_mode: ParamIdentityMode::LocalPreset,
            local_presets: names
                .iter()
                .map(|name| {
                    LocalPreset::new(
                        (*name).to_string(),
                        json!({"text_params": {"schema": 2, "font_size_px": 5.0}}),
                    )
                })
                .collect(),
            ..TypingCreatePreset::default()
        }
    }

    /// A PARAMETER EDIT COPIES NOTHING. While the panel owns the default set the live set IS
    /// that set, so the parked slot stays untouched and the snapshot a write carries is
    /// derived on demand — the deep clone this used to do ran on every frame of a drag.
    #[test]
    fn a_parameter_edit_does_not_copy_the_default_local_set() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();

        for size in [11.0_f32, 12.0, 13.0] {
            state.font_size_px = size;
            state.store_current_params_snapshot();
        }

        assert!(
            state.default_local_set.local_presets.is_empty(),
            "the parked slot is written only when a GLOBAL preset takes the live set",
        );
        let snapshot = state.default_local_set_snapshot();
        assert_eq!(snapshot.local_presets.len(), 1);
        assert_eq!(
            snapshot
                .local_presets
                .first()
                .and_then(|preset| snapshot_font_size(preset.profile())),
            Some(13.0),
            "the derived snapshot is the live set, so it carries the last edit",
        );
    }

    /// THE LIVE-SET INVARIANT, in the exact order that used to destroy the user's default
    /// set: apply a local-mode global preset, switch the identity mode to «Шрифт», deselect
    /// the global preset, switch back. The restore must not depend on the identity mode, or
    /// the panel ends up owning the DEFAULT set while holding the PRESET's and the first
    /// edit persists it over the user's own.
    #[test]
    fn deselecting_a_global_preset_restores_the_default_set_in_font_mode_too() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        let own_name = state.local_presets[0].name.clone();
        state
            .presets_by_name
            .insert("А".to_string(), global_preset_with_local_set(&["Чужой"]));

        state.apply_preset_by_name("А".to_string());
        assert_eq!(
            state.local_presets.len(),
            1,
            "the applied preset owns the live set"
        );
        assert_eq!(state.local_presets[0].name, "Чужой");

        // The identity mode is a panel convenience and must not move ownership.
        state.set_param_identity_mode(ParamIdentityMode::Font);
        state.deselect_global_preset();
        state.set_param_identity_mode(ParamIdentityMode::LocalPreset);

        assert_eq!(
            state
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec![own_name.as_str()],
            "the live set must be the user's own default set again",
        );
        assert_eq!(
            state
                .default_local_set_snapshot()
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec![own_name.as_str()],
            "and that is what the next write carries — never the global preset's set",
        );
    }

    /// THE PARKED SELECTION COMES BACK. The default set D has row 0 selected; a global
    /// preset takes ownership of the live set and is then dropped again. Row 0 must be
    /// selected once more, its parameters must be back on screen, and — the defect this
    /// pins — the next write must NOT carry `selected_local_preset: null`, which is what
    /// the restore used to persist.
    #[test]
    fn deselecting_a_global_preset_restores_the_parked_selection() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        let own_name = state.local_presets[0].name.clone();
        state.font_size_px = 42.0;
        state.store_current_params_snapshot();
        assert_eq!(state.selected_local_preset, Some(0));
        state
            .presets_by_name
            .insert("А".to_string(), global_preset_with_local_set(&["Чужой"]));

        state.apply_preset_by_name("А".to_string());
        state.font_size_px = 5.0;
        state.deselect_global_preset();

        assert_eq!(
            state.selected_local_preset,
            Some(0),
            "the selection was parked with the set and must come back with it",
        );
        assert_eq!(
            state.local_preset_name_input, own_name,
            "the rename box follows the restored selection",
        );
        assert_eq!(
            state.default_local_set_snapshot().selected_local_preset,
            Some(0),
            "the next write must not persist a lost selection",
        );
        assert!(
            (state.font_size_px - 42.0).abs() < f32::EPSILON,
            "the restored preset's own parameters are back on screen, so the next edit \
             cannot overwrite it with the global preset's",
        );
        assert_eq!(
            snapshot_font_size(state.local_presets[0].profile()),
            Some(42.0),
            "and the preset itself is untouched by the round trip",
        );
    }

    /// The same restore in `Font` mode: the selection still comes back (it is the set's own
    /// state and it is what the document holds), but NOTHING ON SCREEN CHANGES — in that
    /// mode the font owns the parameters and a local preset owns nothing.
    #[test]
    fn the_font_mode_restore_takes_the_selection_but_not_the_parameters() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        state.font_size_px = 42.0;
        state.store_current_params_snapshot();
        state
            .presets_by_name
            .insert("А".to_string(), global_preset_with_local_set(&["Чужой"]));
        state.apply_preset_by_name("А".to_string());

        state.set_param_identity_mode(ParamIdentityMode::Font);
        state.font_size_px = 5.0;
        state.deselect_global_preset();

        assert_eq!(state.selected_local_preset, Some(0));
        assert!(
            (state.font_size_px - 5.0).abs() < f32::EPSILON,
            "the identity mode owns the parameters; the restore must not move them",
        );
    }

    /// The other half of the same invariant: a FONT-mode global preset owns an EMPTY local
    /// set, so applying it must install that emptiness. Otherwise switching to «Пресет»
    /// under it hands the panel the DEFAULT set to edit while the preset owns it.
    #[test]
    fn applying_a_font_mode_global_preset_installs_its_empty_local_set() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        let own_name = state.local_presets[0].name.clone();
        state.presets_by_name.insert(
            "Ш".to_string(),
            TypingCreatePreset {
                font: "Alpha-Regular".to_string(),
                identity_mode: ParamIdentityMode::Font,
                ..TypingCreatePreset::default()
            },
        );

        state.apply_preset_by_name("Ш".to_string());

        assert!(
            state.local_presets.is_empty() && state.selected_local_preset.is_none(),
            "the applied preset's (empty) set is the live one",
        );
        assert_eq!(
            state
                .default_local_set_snapshot()
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec![own_name.as_str()],
            "the user's default set is parked, not overwritten with the preset's emptiness",
        );

        state.deselect_global_preset();
        assert_eq!(
            state
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec![own_name.as_str()],
        );
    }

    /// A local preset must always carry a valid snapshot. Under `missing_font` the store
    /// refuses to build one, so creating a preset would have persisted `"profile": null` —
    /// a row that renders nothing and restores nothing. The creation is refused instead and
    /// said so in the status line.
    #[test]
    fn creating_a_local_preset_is_refused_while_the_font_is_missing() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.missing_font = Some("Пропавший".to_string());
        state.status_line = "всё хорошо".to_string();

        state.create_local_preset();

        assert!(
            state.local_presets.is_empty(),
            "no preset may be created without a snapshot to put in it",
        );
        assert_eq!(state.selected_local_preset, None);
        assert_eq!(
            state.status_line,
            t!("typing.local_presets.create_blocked_missing_font").to_string(),
            "the refusal must be visible, not silent",
        );
    }

    /// THE CLEAN/DIRTY CONTRACT. A save that FAILED must leave the set dirty and re-arm the
    /// debounce: marking it clean when the writer was merely SPAWNED lost those edits for
    /// good.
    #[test]
    fn a_failed_save_keeps_the_default_local_set_dirty_and_re_arms_the_debounce() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        let generation = state.local_presets_generation;
        // What a spawned writer leaves behind: the anchor is consumed, the set is not clean.
        state.local_presets_dirty_since = None;
        assert!(state.default_local_set_is_unsaved());

        state
            .preset_store_tx
            .send(PresetStoreEvent::SaveFailed {
                reason: "диск полон".to_string(),
                default_local_generation: generation,
                retryable: true,
            })
            .expect("the panel owns the receiver");
        state.poll_preset_store_events();

        assert!(
            state.local_presets_dirty_since.is_some(),
            "a retryable failure must re-arm the debounce",
        );
        assert!(state.default_local_set_is_unsaved());
        assert!(state.status_line.contains("диск полон"));
    }

    /// The hot-loop guards: a PERMANENT failure never re-arms (retrying it can only fail
    /// again, every debounce window, for the rest of the session), and a retryable one gives
    /// up after `LOCAL_PRESETS_SAVE_MAX_RETRIES`. Neither loses the data: the set stays
    /// dirty, so the next edit and the exit flush still write it.
    #[test]
    fn a_permanent_failure_never_re_arms_and_retries_are_capped() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        let generation = state.local_presets_generation;

        state.local_presets_dirty_since = None;
        state.rearm_default_local_set_after_failed_save(generation, false);
        assert!(
            state.local_presets_dirty_since.is_none(),
            "persistence disabled for the session has nothing to retry",
        );
        assert!(state.default_local_set_is_unsaved(), "and nothing is lost");

        for attempt in 0..LOCAL_PRESETS_SAVE_MAX_RETRIES {
            state.local_presets_dirty_since = None;
            state.rearm_default_local_set_after_failed_save(generation, true);
            assert!(
                state.local_presets_dirty_since.is_some(),
                "retry {attempt} is still within the budget",
            );
        }
        state.local_presets_dirty_since = None;
        state.rearm_default_local_set_after_failed_save(generation, true);
        assert!(
            state.local_presets_dirty_since.is_none(),
            "the automatic retries must stop at the cap",
        );
        assert!(
            state.default_local_set_is_unsaved(),
            "the set stays dirty, so the next edit and the exit flush still write it",
        );

        // A new edit restores the budget.
        state.font_size_px = 21.0;
        state.store_current_params_snapshot();
        assert!(state.local_presets_dirty_since.is_some());
    }

    /// Only a save that actually WROTE marks the set clean, and only up to the generation it
    /// carried: an edit made while the writer ran is still owed afterwards.
    #[test]
    fn only_a_successful_save_marks_the_default_local_set_clean() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        let spawned_generation = state.local_presets_generation;
        state.local_presets_dirty_since = None;
        // An edit that landed while that writer was running.
        state.font_size_px = 64.0;
        state.store_current_params_snapshot();

        state
            .preset_store_tx
            .send(PresetStoreEvent::Saved {
                default_local_generation: spawned_generation,
            })
            .expect("the panel owns the receiver");
        state.poll_preset_store_events();
        assert!(
            state.default_local_set_is_unsaved(),
            "the edit made after the snapshot is still owed",
        );

        let current = state.local_presets_generation;
        state
            .preset_store_tx
            .send(PresetStoreEvent::Saved {
                default_local_generation: current,
            })
            .expect("the panel owns the receiver");
        state.poll_preset_store_events();
        assert!(!state.default_local_set_is_unsaved());
        assert!(state.local_presets_dirty_since.is_none());
    }

    /// THE EXIT FLUSH (`MangaApp::on_exit`): an edit made inside the debounce window is
    /// written on the way out instead of dying with the detached writer thread. Reports
    /// nothing to flush when the set is clean.
    #[test]
    fn the_exit_flush_finds_a_still_owed_default_local_set() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        assert!(
            !state.flush_pending_local_presets_save(),
            "a clean panel owes nothing",
        );

        state.create_local_preset();
        state.rename_local_preset(0, "Переименован".to_string());
        assert!(state.local_presets_dirty_since.is_some());

        assert!(
            state.flush_pending_local_presets_save(),
            "the rename made inside the debounce window must be flushed",
        );
        assert!(state.local_presets_dirty_since.is_none());
    }

    /// The panel adopts what a save appended from another instance's document, WITHOUT
    /// duplicating an entry it already has — the (name, profile) rule of
    /// `presets_store::same_local_preset`.
    #[test]
    fn appended_default_local_presets_are_adopted_without_duplicates() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        let ours = state.local_presets[0].clone();
        // The creation itself owed a write; pretend it landed, so the assertion below is
        // about the ADOPTION and nothing else.
        let generation = state.local_presets_generation;
        state.note_default_local_set_saved(generation);
        assert!(!state.default_local_set_is_unsaved());

        state.adopt_appended_default_local_presets(vec![
            ours.clone(),
            LocalPreset::new(
                "Чужой".to_string(),
                json!({"text_params": {"schema": 2, "font_size_px": 3.0}}),
            ),
        ]);

        assert_eq!(
            state
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec![ours.name.as_str(), "Чужой"],
            "ours is kept once, theirs is appended after it",
        );
        assert!(
            !state.default_local_set_is_unsaved(),
            "adopting what is already on disk owes no write",
        );
    }

    /// What the panel already has wins over the seeded set, exactly like the global presets.
    #[test]
    fn a_seeded_default_local_set_never_replaces_what_the_panel_has() {
        let mut state = panel_with_fonts();
        state.identity_mode = ParamIdentityMode::LocalPreset;
        state.create_local_preset();
        let own = state.local_presets[0].name.clone();

        state.install_seeded_default_local_set(presets_store::DefaultLocalSet {
            local_presets: vec![LocalPreset::new("С диска".to_string(), Value::Null)],
            selected_local_preset: None,
        });

        assert_eq!(state.local_presets.len(), 1);
        assert_eq!(state.local_presets[0].name, own);
    }
}
