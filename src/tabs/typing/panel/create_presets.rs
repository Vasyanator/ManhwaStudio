/*
File: panel/create_presets.rs

Purpose:
Part of `impl TypingCreatePanelState` extracted verbatim from `panel.rs`:
create-panel preset and formula-preset apply/save UI, the shared typing font
combo, the initial preview request, and the face-index clamp.

Main responsibilities:
- draw and apply/save named create presets and formula-layout presets;
- own the ONE font combo both typing panels draw (`draw_font_combo`): its rows,
  its own-typeface previews, its caption, its display clamp and its pick edge;
- issue the initial preview render request and clamp the selected face index.

It is also the ONE place that maps font diagnostics to colors and wording: the
STATIC per-font coverage classification (`font_coverage.rs`, the combo rows'
`primary_color` + `font_coverage_tooltip`) and the FACTUAL per-render fallback
report the renderer returns (`font_fallback_status_lines`, next to the preview).

Key items of the font combo:
- FontComboSpec / FontComboOutcome: what a panel lends the combo and what it gets
  back (the shown font index, the genuine user pick, the button response).
- FontComboRow (private): one owned row, built before the widget runs so the
  panel borrow ends before `SearchableComboBox` takes `&mut Ui`.
- font_combo_selected_position / font_combo_button_width: the pure display clamp
  and the button width the widget needs (its width is exact, unlike ComboBox's).

Notes:
Extracted verbatim from `panel.rs`. Methods are `pub(super)` so the `panel`
module root and its sibling submodules can call them. `use super::*;` pulls in
the parent module's types and imports.
*/

use super::*;
use crate::widgets::{RowLayout, SearchableComboBox, SearchableComboItem};

/// Maximum characters listed in one user-facing character list before it is
/// truncated with a "+N more" suffix. Shared by the static coverage tooltip and
/// the per-render fallback status so a long text can never blow up the panel.
const MAX_SHOWN_CHARS: usize = 15;

/// "Works, but not the way you asked": a font that only partially covers the
/// typesetting language, or a character drawn by a fallback font instead of the
/// selected one. Deliberately not red — both cases still render.
pub(super) const FONT_DIAGNOSTIC_WARNING_COLOR: egui::Color32 =
    egui::Color32::from_rgb(240, 200, 60);

/// "This will not be readable": a font that lacks the writing system entirely, or
/// a character no font in the render base could draw (tofu).
pub(super) const FONT_DIAGNOSTIC_ERROR_COLOR: egui::Color32 =
    egui::Color32::from_rgb(230, 96, 92);

/// Point size of the font combo's row previews, and of its own-typeface closed caption.
///
/// Pinned instead of inherited from `SearchableComboBox`'s default so the rows keep exactly
/// the size the hand-drawn options used before the widget swap (a 14 pt
/// `Style::override_font_id` per row).
const FONT_COMBO_PREVIEW_SIZE_PT: f32 = 14.0;

/// Cap on the height of the font combo's drop-down list, in points.
///
/// Larger than the 200 pt `Spacing::combo_height` the old popup was bounded by
/// (`egui-0.35.0/src/style.rs:1466`): one `RowLayout::Wide` row is a single text line, so the
/// taller list is what makes a font catalog browsable without scrolling.
const FONT_COMBO_MAX_POPUP_HEIGHT_PT: f32 = 320.0;

/// Width, in points, the edit panel keeps on the font combo's row for the face combo that
/// follows the combo on the same row.
///
/// [`FONT_COMBO_MIN_WIDTH_PT`] for the face button itself plus room for its label and the
/// spacing between the two — the font combo may take everything else. The create panel needs
/// no such reserve: its face combo is a row of its own.
pub(super) const FONT_COMBO_FACE_ROW_RESERVE_PT: f32 = FONT_COMBO_MIN_WIDTH_PT + 50.0;

/// Smallest width, in points, the font combo's BUTTON is ever given — the square search
/// button that follows it is budgeted on top of this.
///
/// The default `Spacing::combo_width` (`egui-0.35.0/src/style.rs:1457`), which is what the
/// old `egui::ComboBox`-based button used as its MINIMUM width.
const FONT_COMBO_MIN_WIDTH_PT: f32 = 100.0;

/// Everything one frame of the typing font combo needs from its caller.
///
/// The two panels differ only in these five values; everything else about the combo — the
/// rows, the caption, the display clamp, the pick edge — is identical and lives in
/// [`TypingCreatePanelState::draw_font_combo`].
#[derive(Debug, Clone, Copy)]
pub(super) struct FontComboSpec<'a> {
    /// STABLE, language-independent id salt — never a localized caption
    /// (`egui-docs/05-ids-and-i18n.md` §2). It is what the widget's popup state hangs off.
    pub(super) id_salt: &'static str,
    /// The already-localized label drawn AFTER the button, the way `egui::ComboBox` drew it
    /// (`egui-0.35.0/src/containers/combo_box.rs:252-255`).
    pub(super) label: &'a str,
    /// TOTAL width in points of the combo — its button, the gap, and the square search
    /// button — which the popup inherits. See [`font_combo_button_width`]: the widget's width
    /// is EXACT, so a caller that passes nothing would visibly shrink the row.
    pub(super) width: f32,
    /// The inline span's RAW render label while a text selection is being styled, `None`
    /// outside inline-selection mode. Display resolution only: it is never written back here.
    pub(super) inline_font_label: Option<&'a str>,
    /// `true` when the layer's font is not loaded: the button then names the MISSING font
    /// instead of a row (create panel only; the edit panel shows a red banner instead).
    pub(super) font_missing: bool,
}

/// What one frame of the typing font combo decided.
#[derive(Debug)]
pub(super) struct FontComboOutcome {
    /// Index into `TypingCreatePanelState::fonts` the combo SHOWS as selected, after the
    /// display clamp. Reproduces the value the old call sites wrote into
    /// `selected_font_idx`, including the empty-list case (the resolved index survives).
    pub(super) font_idx: usize,
    /// The font the user genuinely PICKED this frame — a popup commit (even on the already
    /// selected row) or a wheel step that moved — else `None`. The ONLY value allowed to
    /// write an inline span's font label; the per-frame `font_idx` must never do that.
    pub(super) user_pick: Option<usize>,
    /// The closed button's response, for hover-driven caller logic.
    pub(super) response: egui::Response,
}

/// One row of the typing font combo, materialized for one frame.
///
/// Owned on purpose. [`crate::widgets::SearchableComboBox`] borrows both the row texts and
/// the per-row font resolver while it holds `&mut Ui`; building the rows FIRST is what lets
/// the `&self` borrow of the panel end before the widget runs, which is the only way the
/// resolver can be a closure that touches neither `self` nor `ui`.
#[derive(Debug, Clone)]
struct FontComboRow {
    /// Index into `TypingCreatePanelState::fonts`.
    font_idx: usize,
    /// DISPLAY ONLY (`font_display_label`): the row's main line and the closed caption.
    label: String,
    /// The render identity (`FontEntry::render_identity_name`): the row's second line AND
    /// the key its own-typeface preview registration is derived from.
    identity: String,
    /// `FontEntry::content_hash` — the byte discriminant of that registration, so a replaced
    /// file is never previewed from stale bytes. `0` = content unknown.
    content_hash: u64,
    /// Where the preview's bytes are read from; never part of the registration key.
    path: PathBuf,
    /// The face of `path` the preview is rendered from.
    face_index: usize,
    /// Coverage colour of the main line; `None` for a font that fully covers the language.
    color: Option<egui::Color32>,
    /// Already-localized coverage tooltip; `None` for full coverage.
    tooltip: Option<String>,
}

/// Position the font combo SHOWS as selected: where `font_idx` sits among `font_indices`, or
/// `0` when it is not among them.
///
/// That fallback is the historical DISPLAY clamp: a font outside the active group marks the
/// first visible row instead of no row at all. Pure and total — an empty list yields `0`,
/// which the caller maps back to "no row exists", so the clamp can never write a font the
/// user did not pick.
#[must_use]
pub(super) fn font_combo_selected_position(
    font_indices: impl IntoIterator<Item = usize>,
    font_idx: usize,
) -> usize {
    font_indices
        .into_iter()
        .position(|idx| idx == font_idx)
        .unwrap_or(0)
}

/// TOTAL width, in points, to give the font combo on the current row — its button, the gap
/// and the square search button together, which is what `SearchableComboBox::width` means.
///
/// That width is EXACT and the caption is elided, whereas the old `egui::ComboBox` treated
/// `Spacing::combo_width` as a MINIMUM and grew to fit its caption
/// (`egui-0.35.0/src/containers/combo_box.rs:345-361`) — so passing nothing would visibly
/// shrink both panels. `reserved` is the width the caller still needs on the same row AFTER
/// the combo and its label (the edit panel shares its row with the face combo). The floor is
/// [`FONT_COMBO_MIN_WIDTH_PT`] for the button PLUS whatever the widget says its search button
/// costs, so a cramped row shrinks the drop-down and never the magnifier out of existence.
#[must_use]
pub(super) fn font_combo_button_width(ui: &egui::Ui, label: &str, reserved: f32) -> f32 {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    // The colour is irrelevant to a measurement: this galley is never painted.
    let label_width = ui.ctx().fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(label.to_string(), font_id, egui::Color32::WHITE)
            .size()
            .x
    });
    // Asked of the widget rather than re-derived here: the square's side follows the row
    // height, which follows `FONT_COMBO_PREVIEW_SIZE_PT` and the active style.
    let search_button = SearchableComboBox::search_button_overhang(ui, FONT_COMBO_PREVIEW_SIZE_PT);
    (ui.available_width() - label_width - ui.spacing().item_spacing.x - reserved)
        .max(FONT_COMBO_MIN_WIDTH_PT + search_button)
}

impl TypingCreatePanelState {
    /// Draws the create-only presets section of the «Параметры» dock tab.
    ///
    /// `extras` is that tab's persisted state; the section's expanded/collapsed
    /// flag lives in it (see [`collapsing_param_section`]). Draws nothing on the
    /// edit panel.
    pub(super) fn draw_create_presets_section(
        &mut self,
        ui: &mut egui::Ui,
        extras: &mut TabExtras,
    ) {
        if !self.preview_enabled {
            return;
        }
        // The section title comes from the collapsing header (below); the summary
        // is the currently selected preset display name (or the "none" label).
        let preview_enabled = self.preview_enabled;
        let preset_summary = self
            .selected_preset_name
            .clone()
            .unwrap_or_else(|| text_preset_none_label().to_string());
        collapsing_param_section(
            ui,
            ParamSectionId::in_tab("typing.section.presets", preview_enabled, extras),
            t!("typing.presets.section_heading"),
            false,
            Some(preset_summary.as_str()),
            |ui| {
                self.draw_create_presets_body(ui);
            },
        );
    }

    /// Body of the create-presets section (moved verbatim from the former
    /// `ui.group(...)`): the preset selector combo plus the save-preset name
    /// input and button. The strong section title is now shown in the collapsing
    /// header, so it is no longer drawn inline here.
    fn draw_create_presets_body(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.horizontal(|ui| {
                let mut names: Vec<String> = self.presets_by_name.keys().cloned().collect();
                names.sort();
                let selected_text = self
                    .selected_preset_name
                    .as_deref()
                    .unwrap_or(text_preset_none_label());
                let prev_selected = self.selected_preset_name.clone();
                let preset_len = names.len() + 1;
                let mut preset_idx = self
                    .selected_preset_name
                    .as_ref()
                    .and_then(|selected| names.iter().position(|name| name == selected))
                    .map(|idx| idx + 1)
                    .unwrap_or(0);
                let preset_combo = WheelComboBox::from_label(t!("typing.presets.current_preset_combo_id")).id_salt("typing.presets.current_preset_combo_id")
                    .selected_text(selected_text)
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(preset_idx == 0, text_preset_none_label())
                            .clicked()
                        {
                            preset_idx = 0;
                        }
                        for (idx, name) in names.iter().enumerate() {
                            if ui.selectable_label(preset_idx == idx + 1, name).clicked() {
                                preset_idx = idx + 1;
                            }
                        }
                    });
                if let Some(steps) = preset_combo.wheel_steps {
                    cycle_wrapped_index(&mut preset_idx, preset_len, steps);
                }
                self.selected_preset_name = if preset_idx == 0 {
                    None
                } else {
                    names.get(preset_idx - 1).cloned()
                };
                if self.selected_preset_name != prev_selected
                    && let Some(name) = self.selected_preset_name.clone()
                {
                    self.apply_preset_by_name(name);
                    self.queue_preview_render();
                }
            });
            ui.horizontal(|ui| {
                let preset_name_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.preset_name_input)
                        .id_salt("typing_preset_name_input")
                        .hint_text(t!("typing.presets.save_preset_button"))
                        .desired_width((ui.available_width() - 96.0).max(120.0)),
                );
                self.track_text_input(&preset_name_resp);
                if ui.button(t!("typing.presets.save_button")).clicked() {
                    self.save_current_preset();
                }
            });
        });
    }

    /// Applies a saved create preset: its per-font profile memory and its primary font.
    ///
    /// The preset names its font ONCE, by identity (`TypingCreatePreset.font`). A value the
    /// migration could not resolve survives there in its legacy spelling, so this stays a
    /// READ path: the profile map is re-keyed to IDENTITIES in memory and the primary font
    /// is resolved through the one legacy door
    /// (`dev-docs/font_identity_postscript_plan.md`, fixed decision 5). A key that resolves
    /// to no loaded font is kept VERBATIM rather than dropped — it is the only remaining
    /// clue about which font it meant, and the user may install that font later.
    ///
    /// MISSING PRIMARY FONT. When the preset NAMES a primary font that no loaded font
    /// matches BY NAME, the panel enters the same `missing_font` state an overlay load
    /// produces (`create_apply::select_font_by_identity`): the selection is left where it
    /// was and no profile is applied to it, so the preset is never silently applied to a
    /// DIFFERENT font than the one it was saved for. A legacy value that only matches a
    /// loaded font by PATH counts as missing too — the file at a remembered path is not
    /// proof of identity. A preset that names no font at all (an empty `font`, only
    /// reachable for a preset saved with an empty font list) keeps the current selection
    /// and is not a missing font.
    pub(super) fn apply_preset_by_name(&mut self, name: String) {
        let Some(preset) = self.presets_by_name.get(&name).cloned() else {
            return;
        };
        // Marked applied BEFORE any profile is stored: from here on every parameter write
        // belongs to THIS preset's working set, not to the font's persisted default
        // (`create_render_data::store_current_font_profile_by_idx`, variant A).
        self.selected_preset_name = Some(name);
        // Applying a preset replaces the SESSION memory only; each font's persisted default
        // profile is left alone (a preset is an independent overlay, not a rewrite of what
        // every font remembers on disk).
        self.font_profiles_by_identity =
            FontProfileMemory::from_map(self.font_profiles_keyed_by_identity(preset.font_profiles));

        let primary = preset.font.trim();
        let names_a_font = !primary.is_empty();
        // The stored value is an identity; a leftover legacy value may be a name form or a
        // path, so it goes through the legacy door — where only NAME evidence may select.
        let target_idx = self.find_font_idx_by_identity(primary).or_else(|| {
            match self.match_font_by_legacy_reference(Some(primary), &[primary]) {
                Some(LegacyFontMatch::ByName(idx)) => Some(idx),
                Some(LegacyFontMatch::PathOnly(_)) | None => None,
            }
        });
        match target_idx {
            Some(idx) => {
                self.selected_font_idx = idx;
                self.missing_font = None;
            }
            None if names_a_font => {
                // The preset's own font is not loaded. Record it and stop: applying its
                // parameters to whatever font happens to be selected would show the user a
                // preset "applied" to a font it was never saved for.
                self.missing_font = Some(
                    Path::new(primary)
                        .file_name()
                        .and_then(|file| file.to_str())
                        .filter(|_| primary.contains(std::path::is_separator))
                        .unwrap_or(primary)
                        .to_string(),
                );
                return;
            }
            None => {}
        }
        self.active_font_identity = self.current_font_identity();
        if let Some(identity) = self.current_font_identity() {
            if let Some(profile) = self.font_profiles_by_identity.get(&identity).cloned() {
                self.apply_render_data_json_with_options(&profile, false);
            } else {
                self.selected_face_idx = 0;
                self.sync_current_font_profile_memory();
            }
        }
        self.clamp_face_index();
    }

    /// Re-keys a stored profile map to font IDENTITIES.
    ///
    /// Every key that resolves to a loaded font is replaced by that font's identity
    /// STRING (so a key differing only in case stops shadowing the profile it means);
    /// a key that resolves to nothing survives unchanged, so no user data is lost — it is
    /// the only remaining clue about which font it meant, and the font may be installed
    /// later. Such a key can never collide with a converted one, since a key that matches
    /// a loaded identity in any casing resolves by definition.
    ///
    /// COLLISION PRIORITY (deterministic, and NOT the `HashMap` iteration order — that is
    /// randomized per process, so the surviving profile used to be a coin toss). Several
    /// legacy keys can name one font (`/old/fonts/Regular.ttf` and `Regular`); the winner
    /// is the key with the strongest claim, and ties are broken lexicographically:
    ///
    /// 1. the key IS the identity, byte for byte — the current form;
    /// 2. the key is the identity up to case;
    /// 3. a legacy NAME (family / label / stem) — still a name for the font;
    /// 4. a legacy PATH — the weakest form, and the one the plan is removing.
    ///
    /// A PATH is deliberately still accepted HERE, unlike in font SELECTION: the stored key
    /// is the only reference a legacy preset ever had for a profile (it was literally
    /// `path.to_string_lossy()`), so refusing it would strand every profile the user has,
    /// while the worst case is remembered PARAMETERS attached to a font whose file was
    /// replaced — not a layer re-rendered in the wrong typeface. Ranking it last is what
    /// keeps a name from ever losing to a path.
    ///
    /// Every displaced profile is logged with the key that won, so a merge is visible
    /// rather than silent.
    fn font_profiles_keyed_by_identity(
        &self,
        profiles: HashMap<String, Value>,
    ) -> HashMap<String, Value> {
        // (winning rank, winning key, profile) per target key.
        let mut out: HashMap<String, (u8, String, Value)> = HashMap::with_capacity(profiles.len());
        // Sorting makes both the rank comparison and its lexicographic tie-break
        // independent of the map's randomized iteration order.
        let mut incoming: Vec<(String, Value)> = profiles.into_iter().collect();
        incoming.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (key, profile) in incoming {
            // One lookup decides both the target and the rank: the legacy door reports
            // WHICH kind of evidence matched, so a name can no longer lose to a path (it
            // used to, because the key was handed in as a path AND as a name, and the path
            // was tried first).
            let matched = self
                .find_font_idx_by_identity(&key)
                .map(LegacyFontMatch::ByName)
                .or_else(|| self.match_font_by_legacy_reference(Some(&key), &[&key]));
            let resolved = matched
                .map(LegacyFontMatch::font_idx)
                .and_then(|idx| self.font_identity_name_by_idx(idx));
            let (target, rank) = match resolved {
                Some(identity) => {
                    let rank = if identity == key {
                        0
                    } else if identity.eq_ignore_ascii_case(&key) {
                        1
                    } else if matches!(matched, Some(LegacyFontMatch::ByName(_))) {
                        2
                    } else {
                        3
                    };
                    (identity, rank)
                }
                // Unresolvable: kept verbatim, and alone under that key.
                None => (key.clone(), 0),
            };
            match out.entry(target) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert((rank, key, profile));
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    let (loser_key, winner_key) = if rank < slot.get().0 {
                        let previous = slot.insert((rank, key.clone(), profile));
                        (previous.1, key)
                    } else {
                        (key, slot.get().1.clone())
                    };
                    crate::runtime_log::log_info(format!(
                        "typing presets: profile keys '{loser_key}' and '{winner_key}' both name \
                         the font '{}'; keeping the profile stored under '{winner_key}' (the \
                         stronger key form) and dropping the other.",
                        slot.key(),
                    ));
                }
            }
        }
        out.into_iter()
            .map(|(identity, (_, _, profile))| (identity, profile))
            .collect()
    }

    /// Saves the panel's current parameters as the named create preset and persists the
    /// whole preset document off the GUI thread.
    ///
    /// The preset carries the SESSION profile memory ONLY — the fonts the user actually
    /// touched here. It used to additionally copy the CURRENT font's profile into every
    /// other loaded font's key, which is what turned 67 real profiles into 162 stored ones
    /// (87 % of `user_config.json`) and, worse, made a preset claim parameters for fonts it
    /// was never configured for. Each font's own remembered parameters live in
    /// `fonts_data.fonts.<identity>.profile` and need no copy.
    pub(super) fn save_current_preset(&mut self) {
        if !self.preview_enabled {
            return;
        }
        let preset_name = self.preset_name_input.trim().to_string();
        if preset_name.is_empty() {
            return;
        }

        self.sync_current_font_profile_memory();

        self.presets_by_name.insert(
            preset_name.clone(),
            TypingCreatePreset {
                font: self.current_font_identity().unwrap_or_default(),
                font_profiles: self.font_profiles_by_identity.to_map(),
            },
        );
        self.selected_preset_name = Some(preset_name);
        self.spawn_presets_save(false);
    }

    /// Persists the whole preset document to `fonts/presets.json` off the GUI thread.
    ///
    /// `then_clean_user_config` additionally deletes the migrated legacy `TextTab` keys —
    /// only ever passed by the migration, and only AFTER the new document is safely on
    /// disk, so a failed write can never lose the presets it was supposed to replace.
    ///
    /// A failed save (or a failed thread spawn) is logged AND pushed to `preset_store_tx`,
    /// which the GUI thread turns into a visible status line: a preset the user just saved
    /// must never disappear silently, which is exactly what the two `let _ =` this replaced
    /// allowed.
    ///
    /// Under `#[cfg(test)]` the body early-returns before spawning, so unit tests never
    /// touch the real fonts directory; the write itself is covered by `presets_store`'s own
    /// tests and by `run_presets_save`, which a test drives synchronously (same precedent as
    /// `font_settings_store::persist_off_thread`).
    fn spawn_presets_save(&self, then_clean_user_config: bool) {
        if cfg!(test) {
            return;
        }
        let presets = self.presets_by_name.clone();
        let fonts_dir = self.fonts_dir.clone();
        let events = self.preset_store_tx.clone();
        // Ticket taken HERE, where the snapshot is: it is what keeps a slow writer from
        // putting an older state of the document back over a newer one.
        let ticket = presets_store::next_save_ticket();
        let spawn_result = thread::Builder::new()
            .name("typing-save-create-presets".to_string())
            .spawn(move || {
                // The config path is resolved HERE, off the GUI thread, and handed down
                // explicitly so the whole chain can be tested against a temp file.
                let clean_config = then_clean_user_config.then(config::user_config_path);
                run_presets_save(&fonts_dir, &presets, ticket, clean_config.as_deref(), &events);
            });
        if let Err(err) = spawn_result {
            report_preset_save_failure(
                &self.preset_store_tx,
                &format!("cannot spawn the presets.json writer thread: {err}"),
            );
        }
    }

    /// Drains everything the background `fonts/presets.json` workers have to say, once per
    /// frame: the off-thread seed, the one-shot `user_config` migration, presets another app
    /// instance wrote, and save failures. A no-op when the channel is empty.
    ///
    /// The migration is finished HERE, on the GUI thread, because re-keying the legacy font
    /// references needs this panel's font list — the same reason `fonts_data`'s v1 migration
    /// is deferred to the end of a font-list build.
    ///
    /// And it waits for the AUTHORITATIVE (combined) list. The preset read and the font load
    /// are two independent background jobs, so "the fonts are usually there by now" is a
    /// race, not an ordering: when the reader wins, a migration run here would resolve no
    /// IMPORTED system font, keep those references verbatim, delete the legacy
    /// `user_config` key and never retry — `presets.json` and `fonts_data.json` would
    /// disagree about the same font forever. The payload is therefore PARKED until
    /// `poll_font_reload_results` reports the combined list.
    pub(super) fn poll_preset_store_events(&mut self) {
        loop {
            match self.preset_store_rx.try_recv() {
                Ok(PresetStoreEvent::Seeded { presets, legacy }) => {
                    self.install_seeded_presets(presets);
                    if let Some(legacy) = legacy {
                        if self.font_list_is_authoritative {
                            self.finish_legacy_presets_migration(legacy);
                        } else {
                            self.pending_legacy_presets_migration = Some(legacy);
                        }
                    }
                }
                Ok(PresetStoreEvent::MergedFromDisk(presets)) => {
                    // Written by another app instance and already part of the document on
                    // disk; adopting them keeps the next snapshot from dropping them again.
                    // Ours wins a name clash — it is what is on screen.
                    for (name, preset) in presets {
                        self.presets_by_name.entry(name).or_insert(preset);
                    }
                }
                Ok(PresetStoreEvent::SaveFailed(reason)) => {
                    self.status_line = tf!("typing.presets.save_error_status", err = reason);
                }
                // The senders live in the panel itself, so the channel cannot be
                // disconnected while the panel exists; both idle cases end the drain.
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Installs the presets the startup read found. A preset the user saved BEFORE the read
    /// landed wins over its stored namesake: it is the fresher one, and the save that wrote
    /// it already merged the document it did not know about (`presets_store::save`).
    fn install_seeded_presets(&mut self, presets: HashMap<String, TypingCreatePreset>) {
        for (name, preset) in presets {
            self.presets_by_name.entry(name).or_insert(preset);
        }
    }

    /// Completes the one-shot migration out of `user_config.TextTab.create_presets`
    /// (`dev-docs/font_identity_postscript_plan.md` phase 5) with the payload the seed read
    /// off the GUI thread, then persists the result and cleans the legacy keys.
    ///
    /// A migrated preset whose name is ALREADY taken is kept under a suffixed name instead
    /// of being dropped: two presets are the user's data twice over, and "the newer one
    /// wins" would silently delete years-old parameters the user never asked to lose.
    pub(super) fn finish_legacy_presets_migration(
        &mut self,
        legacy: Vec<presets_store::LegacyPresetEntry>,
    ) {
        if legacy.is_empty() {
            // Nothing to migrate. The dead keys may still be lying around, so the cleanup
            // pass runs anyway (it rewrites nothing when they are absent).
            spawn_user_config_cleanup(&self.fonts_dir);
            return;
        }
        let migrated = self.migrate_legacy_presets(legacy);
        let migrated_count = migrated.len();
        for (name, preset) in migrated {
            let free_name = self.free_preset_name(name);
            self.presets_by_name.insert(free_name, preset);
        }
        crate::runtime_log::log_info(format!(
            "typing: migrated {migrated_count} create preset(s) from user_config.json into \
             fonts/presets.json"
        ));
        self.spawn_presets_save(true);
    }

    /// `name` itself when no preset holds it, otherwise the first free `"{name} (N)"`
    /// (N from 2). A rename is logged, since the user will see a name they did not type.
    fn free_preset_name(&self, name: String) -> String {
        if !self.presets_by_name.contains_key(&name) {
            return name;
        }
        // Bounded by the number of presets plus one, so a free slot always exists.
        let taken = self.presets_by_name.len() + 2;
        for suffix in 2..=taken {
            let candidate = format!("{name} ({suffix})");
            if !self.presets_by_name.contains_key(&candidate) {
                crate::runtime_log::log_warn(format!(
                    "typing presets: a preset named '{name}' already exists, so the migrated \
                     one was kept as '{candidate}' rather than dropped."
                ));
                return candidate;
            }
        }
        // Unreachable by the bound above; keeping the original name would overwrite, so the
        // fallback appends the count instead of losing the preset.
        format!("{name} ({taken})")
    }

    /// Converts legacy presets into the current form, resolving every stored font
    /// reference against THIS panel's font list. Pure with respect to `self` (nothing is
    /// stored here), so the whole migration rule is unit-testable.
    ///
    /// Per preset:
    /// - the three competing primary references collapse into one `font`. Resolution is by
    ///   NAME (`primary_font_key` as an identity first, then the label, then the key, then
    ///   the path's own name forms); a match that exists ONLY as a file path is refused,
    ///   because a file sitting at a remembered location is not proof of identity.
    /// - the profile map is re-keyed by [`Self::font_profiles_keyed_by_identity`], where a
    ///   path key IS accepted (it is the only reference a legacy profile ever had) but
    ///   ranks below every name.
    /// - every profile body is upgraded to the current `text_params` schema.
    ///
    /// Anything that resolves to nothing is KEPT VERBATIM under its legacy string and
    /// logged — never dropped: it is the only surviving clue about the font it meant, and
    /// it resolves again once the user reinstalls that font.
    pub(super) fn migrate_legacy_presets(
        &self,
        legacy: Vec<presets_store::LegacyPresetEntry>,
    ) -> Vec<(String, TypingCreatePreset)> {
        legacy
            .into_iter()
            .map(|(name, preset)| {
                let font = self.migrate_legacy_primary_font(&name, &preset);
                let profiles = self.font_profiles_keyed_by_identity(preset.font_profiles);
                let font_profiles = profiles
                    .into_iter()
                    .map(|(key, profile)| (key, self.upgrade_profile_to_current_schema(profile)))
                    .collect();
                (name, TypingCreatePreset { font, font_profiles })
            })
            .collect()
    }

    /// Resolves the three legacy primary-font references of one preset into a single
    /// identity, or returns the strongest legacy string VERBATIM when nothing resolves by
    /// name. See [`Self::migrate_legacy_presets`] for the rule; this logs what it kept.
    fn migrate_legacy_primary_font(
        &self,
        preset_name: &str,
        preset: &presets_store::LegacyCreatePreset,
    ) -> String {
        let key = preset.primary_font_key.trim();
        let label = preset.primary_font_label.as_deref().unwrap_or_default().trim();
        let path = preset.primary_font_path.as_deref().unwrap_or_default().trim();
        if key.is_empty() && label.is_empty() && path.is_empty() {
            // The preset names no font at all; it selects nothing and reports nothing.
            return String::new();
        }
        // Identity first (what a late build wrote), then every remaining NAME form: the
        // stored key and label as written, then the FILE STEM of each stored path — the
        // last name candidate of the historical chain
        // (`text_params_schema::legacy_font_name_candidates`), so a preset and a layer
        // written by the same build resolve the same way. The paths themselves are only
        // offered to the path pass, whose match is refused below.
        let stem_of = |value: &str| {
            Path::new(value)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| !stem.is_empty() && *stem != value)
                .map(ToOwned::to_owned)
        };
        let stems: Vec<String> = [key, path].into_iter().filter_map(stem_of).collect();
        let names: Vec<&str> = [key, label]
            .into_iter()
            .chain(stems.iter().map(String::as_str))
            .filter(|value| !value.is_empty())
            .collect();
        let matched = self
            .find_font_idx_by_identity(key)
            .map(LegacyFontMatch::ByName)
            .or_else(|| self.match_font_by_legacy_reference(Some(path), &names));
        if let Some(LegacyFontMatch::ByName(idx)) = matched
            && let Some(identity) = self.font_identity_name_by_idx(idx)
        {
            return identity;
        }
        // Keep the strongest legacy spelling so the user can still see (and repair) what
        // the preset meant; `apply_preset_by_name` reports it as a missing font.
        let kept = names.first().copied().unwrap_or_default().to_string();
        crate::runtime_log::log_warn(format!(
            "typing presets: the primary font of preset '{preset_name}' ('{kept}') matches no \
             loaded font by name; it is KEPT VERBATIM and will resolve again if that font is \
             reinstalled."
        ));
        kept
    }

    /// Upgrades one stored profile body (`{ "effects": [...], "text_params": {...} }`) to
    /// the current `text_params` schema, reusing the ONE conversion the tab-side codec owns
    /// so a preset and a layer can never disagree about what a legacy key meant.
    ///
    /// A body whose font does not resolve is returned UNCHANGED (schema 1) — the conversion
    /// refuses to drop legacy keys it cannot replace, and so does this.
    fn upgrade_profile_to_current_schema(&self, profile: Value) -> Value {
        let Some(text_params) = profile
            .get("text_params")
            .and_then(Value::as_object)
            .cloned()
        else {
            return profile;
        };
        let upgraded = crate::tabs::typing::tab::codec::upgrade_text_params_to_v2(
            &text_params,
            &|path, name| self.resolve_legacy_font_identity(path, name),
        );
        match upgraded {
            crate::tabs::typing::tab::codec::TextParamsUpgrade::Converted(value) => {
                let mut profile = profile;
                if let Some(obj) = profile.as_object_mut() {
                    obj.insert("text_params".to_string(), value);
                }
                profile
            }
            // Already current, or a font this build cannot resolve: leave the body alone.
            crate::tabs::typing::tab::codec::TextParamsUpgrade::AlreadyCurrent
            | crate::tabs::typing::tab::codec::TextParamsUpgrade::UnresolvedFont { .. }
            | crate::tabs::typing::tab::codec::TextParamsUpgrade::PathOnlyFont { .. } => profile,
        }
    }


    pub(super) fn apply_formula_preset_by_name(&mut self, name: String) -> bool {
        let Some(preset) = self.formula_presets_by_name.get(&name).cloned() else {
            return false;
        };
        self.formula_layout = preset.layout;
        self.selected_formula_preset_name = Some(name);
        true
    }

    pub(super) fn save_current_formula_preset(&mut self) {
        let preset_name = self.formula_preset_name_input.trim().to_string();
        if preset_name.is_empty() {
            return;
        }
        self.formula_presets_by_name.insert(
            preset_name.clone(),
            TypingFormulaPreset {
                layout: self.formula_layout.clone(),
            },
        );
        self.selected_formula_preset_name = Some(preset_name);
        let presets = self.formula_presets_by_name.clone();
        let _ = thread::Builder::new()
            .name("typing-save-formula-presets".to_string())
            .spawn(move || {
                let _ = save_text_tab_formula_presets(&presets);
            });
    }

    pub(super) fn swap_formula_xy_expressions(&mut self) {
        std::mem::swap(
            &mut self.formula_layout.x_expr,
            &mut self.formula_layout.y_expr,
        );
        self.selected_formula_preset_name = None;
    }

    pub(super) fn sync_selected_formula_preset_by_layout(&mut self) {
        self.selected_formula_preset_name =
            self.formula_presets_by_name
                .iter()
                .find_map(|(name, preset)| {
                    if formula_layout_approx_eq(&self.formula_layout, &preset.layout) {
                        Some(name.clone())
                    } else {
                        None
                    }
                });
    }

    /// Materializes one owned row per font of the ACTIVE group, in catalog order.
    ///
    /// The rows are the combo's index space: `FontComboRow::font_idx` maps a row back to
    /// `self.fonts`, so a row list built this way can never drift from the positions the
    /// widget reports (which a separately-kept `filtered_font_indices()` could).
    ///
    /// There is deliberately NO cap on how many rows may register an own-typeface preview.
    /// This list is the project's own `fonts/` plus the fonts the user imported — not the OS
    /// catalog — and the previous combo registered EVERY filtered font on every frame its
    /// popup was open. `SearchableComboBox` resolves only the rows it actually draws, so the
    /// registrations are bounded by the visible rows rather than by the list length.
    fn build_font_combo_rows(&self) -> Vec<FontComboRow> {
        self.filtered_font_indices()
            .into_iter()
            .filter_map(|font_idx| {
                let font = self.fonts.get(font_idx)?;
                // Highlight fonts that do not fully support the typesetting language. The
                // wording lives in `font_coverage_tooltip`, which returns `None` for `Full`.
                let color = match font.coverage.support {
                    FontLanguageSupport::Full => None,
                    FontLanguageSupport::Partial => Some(FONT_DIAGNOSTIC_WARNING_COLOR),
                    FontLanguageSupport::Unsupported => Some(FONT_DIAGNOSTIC_ERROR_COLOR),
                };
                Some(FontComboRow {
                    font_idx,
                    label: self.font_display_label(font),
                    identity: font.render_identity_name(),
                    content_hash: font.content_hash(),
                    path: font.path.clone(),
                    face_index: font.faces.first().map(|face| face.face_index).unwrap_or(0),
                    color,
                    tooltip: font_coverage_tooltip(&font.coverage),
                })
            })
            .collect()
    }

    /// The text the CLOSED font combo has to show, in every case the old combo covered.
    ///
    /// `font_indices` is the combo's row index space (see [`Self::build_font_combo_rows`]);
    /// it makes the inline label resolve group-preferringly, exactly as the row marking does.
    fn font_combo_caption(&self, spec: &FontComboSpec<'_>, font_indices: &[usize]) -> String {
        if spec.font_missing {
            // Шрифт оверлея не найден: показываем его имя, чтобы было понятно,
            // какой именно шрифт отсутствует и какой надо заменить.
            return self
                .missing_font
                .as_ref()
                .map(|name| tf!("typing.params.font_not_found_option", name = name))
                .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string());
        }
        spec.inline_font_label
            .map(|label| {
                // DISPLAY ONLY: resolve the raw render label to its display label (a user
                // rename override) when a font matches, so the CLOSED combo shows the same
                // name as the popup rows. The span style's render key is never touched.
                self.find_font_idx_by_label_preferring_indices(Some(label), font_indices)
                    .and_then(|idx| self.fonts.get(idx))
                    .map(|font| self.font_display_label(font))
                    .unwrap_or_else(|| label.to_string())
            })
            .or_else(|| {
                self.fonts
                    .get(self.selected_font_idx)
                    .map(|font| self.font_display_label(font))
            })
            .unwrap_or_else(|| t!("typing.params.font_placeholder").to_string())
    }

    /// Draws ONE frame of the typing font combo — button, label and searchable drop-down —
    /// and reports what it decided.
    ///
    /// Shared verbatim by the create panel and the edit panel: both resolve the same fonts,
    /// clamp the same way and detect the same pick edge. What they do with
    /// [`FontComboOutcome::user_pick`] / [`FontComboOutcome::font_idx`] afterwards is NOT
    /// shared — the two writeback branches genuinely differ and stay at their call sites.
    ///
    /// Rows are `RowLayout::Wide`: the display label in the font's OWN face, followed by its
    /// render identity in the interface font, coloured and explained by the language-coverage
    /// diagnostics. The closed caption keeps the row's own face too, unless the text it must
    /// show is not a row at all (a missing font, an unresolvable inline label, or an inline
    /// font outside the active group) — then it is drawn in the interface font.
    ///
    /// Registering the own-typeface preview goes through the shared
    /// `widgets::font_preview`, keyed by `(identity, content hash, face index)`, so the two
    /// panels sharing one egui `Context` share one registration. The first frames of a row
    /// are drawn in the interface font: the file is read in the BACKGROUND and only
    /// `Context::add_font` happens on the GUI thread.
    pub(super) fn draw_font_combo(
        &self,
        ui: &mut egui::Ui,
        spec: &FontComboSpec<'_>,
    ) -> FontComboOutcome {
        let rows = self.build_font_combo_rows();
        let font_indices: Vec<usize> = rows.iter().map(|row| row.font_idx).collect();
        // Resolve the selection's/overlay's current font from its label. When a group is
        // active this PREFERS the in-group copy over a same-named font outside the group, so
        // an ambiguous label (e.g. an imported system font colliding with a group member)
        // does not silently resolve to the wrong entry.
        let resolved_font_idx = self
            .find_font_idx_by_label_preferring_indices(spec.inline_font_label, &font_indices)
            .unwrap_or(self.selected_font_idx);
        let caption = self.font_combo_caption(spec, &font_indices);
        // DISPLAY-ONLY clamp: a font outside the active group marks the first visible row, so
        // a valid row is always shown as selected. In inline-selection mode this clamped
        // value is NEVER written back into the span style (the caller's writeback is gated on
        // `user_pick`) — otherwise merely selecting text would bounce the label to a
        // different font and re-insert a `<font>` tag every frame.
        let mut position =
            font_combo_selected_position(font_indices.iter().copied(), resolved_font_idx);
        // The widget's own caption is the marked row's main line in that row's face, which
        // equals `caption` in the common case. Override it only when the two DISAGREE — the
        // missing-font text, an inline label that resolved to nothing, an inline font outside
        // the active group, or an empty list — so the common case keeps its own typeface.
        let caption_is_marked_row = rows.get(position).is_some_and(|row| row.label == caption);
        let items: Vec<SearchableComboItem<'_>> = rows
            .iter()
            .map(|row| {
                // The identity is shown on EVERY row, including the ones where it repeats the
                // display label. That duplicate is deliberate and was chosen over suppressing
                // it: a row that sometimes carries a second line and sometimes does not makes
                // the list ragged, and the reader loses the fixed place to look for the
                // PostScript name. The two lines still differ visibly — the label is drawn in
                // the font's own typeface, the identity in the interface font.
                let mut item = SearchableComboItem::with_secondary(&row.label, &row.identity);
                if let Some(color) = row.color {
                    item = item.primary_color(color);
                }
                if let Some(tooltip) = row.tooltip.as_deref() {
                    item = item.tooltip(tooltip);
                }
                item
            })
            .collect();
        // The resolver runs while the widget holds `&mut Ui`, so it may touch neither `ui`
        // nor `self`: it gets an owned `Context` handle and the owned rows, and nothing else.
        let ctx = ui.ctx().clone();
        let mut resolve_family = |index: usize| -> Option<egui::FontFamily> {
            let row = rows.get(index)?;
            match crate::widgets::request_font_family(
                &ctx,
                &row.identity,
                row.content_hash,
                &row.path,
                row.face_index,
            ) {
                crate::widgets::PreviewFontFamily::Ready(family) => Some(family),
                // Both non-ready states draw the row in the interface font, which is what the
                // widget does for a `None`. `Pending` retries by itself on a later frame.
                crate::widgets::PreviewFontFamily::Pending
                | crate::widgets::PreviewFontFamily::Unavailable => None,
            }
        };
        let mut combo = SearchableComboBox::new(spec.id_salt)
            .row_layout(RowLayout::Wide)
            .primary_size(FONT_COMBO_PREVIEW_SIZE_PT)
            .max_popup_height(FONT_COMBO_MAX_POPUP_HEIGHT_PT)
            .width(spec.width)
            .item_font(&mut resolve_family);
        if !caption_is_marked_row {
            combo = combo.selected_text(caption);
        }
        // The widget draws no label of its own; `egui::ComboBox` used to draw one after the
        // button inside its own horizontal row, and dropping it would silently remove the
        // word «Шрифт» from both panels.
        let before = position;
        let (response, picked, changed) = ui
            .horizontal(|ui| {
                let outcome = combo.show(ui, &mut position, &items);
                let label = ui.label(spec.label);
                (
                    outcome.response.labelled_by(label.id),
                    outcome.picked,
                    outcome.changed,
                )
            })
            .inner;
        let font_idx = font_indices
            .get(position)
            .copied()
            .unwrap_or(resolved_font_idx);
        // `changed` is exactly "the widget WROTE the selection this frame" — a click on
        // another row, `Enter`, or a wheel step — so pairing it with the pre-show position
        // gives the edge detector the same `(before, after)` the old wheel handling produced.
        let user_pick =
            create_main_text::font_combo_user_pick(picked, changed.then_some((before, position)))
                .and_then(|pos| font_indices.get(pos).copied());
        FontComboOutcome {
            font_idx,
            user_pick,
            response,
        }
    }

    pub(super) fn ensure_initial_preview_request(&mut self) {
        if !self.preview_enabled {
            return;
        }
        if !self.needs_initial_preview {
            return;
        }
        self.needs_initial_preview = false;
        self.queue_preview_render();
    }

    pub(super) fn clamp_face_index(&mut self) {
        if let Some(font) = self.fonts.get(self.selected_font_idx) {
            let max_idx = font.faces.len().saturating_sub(1);
            self.selected_face_idx = self.selected_face_idx.min(max_idx);
        } else {
            self.selected_face_idx = 0;
        }
    }
}

/// Starts the OFF-GUI-THREAD seeding of a create panel's preset state.
///
/// Reading `fonts/presets.json` (and, when a migration is owed, the up-to-half-a-megabyte
/// `user_config.json`) is file I/O and must not happen while a panel is being constructed on
/// the GUI thread (CLAUDE.md §5). The worker sends exactly one
/// [`PresetStoreEvent::Seeded`], which `poll_preset_store_events` installs.
///
/// The panel starts with NO presets and the writer's baseline set to "the document is
/// absent", so a save issued before the seed lands cannot blindly overwrite the document it
/// has not read yet: `presets_store::save` sees the mismatch, merges the file in and
/// retries.
///
/// Under `#[cfg(test)]` nothing is spawned and no disk is touched: the store is covered by
/// `presets_store`'s tests and the migration RULE by `migrate_legacy_presets`' own tests,
/// while a unit test must never read the developer's real `fonts/` or `user_config.json`.
pub(super) fn spawn_presets_seed(fonts_dir: &Path, events: &Sender<PresetStoreEvent>) {
    if cfg!(test) {
        return;
    }
    presets_store::set_baseline(fonts_dir, doc_store::SaveBaseline::Absent);
    let fonts_dir = fonts_dir.to_path_buf();
    let events = events.clone();
    let spawn_result = thread::Builder::new()
        .name("typing-read-create-presets".to_string())
        .spawn(move || {
            let (event, clean_config_now) = read_presets_seed(&fonts_dir);
            // A closed channel means the panel is already gone; there is nobody left to
            // hand the payload to and nothing has been modified, so the send result is
            // deliberately ignored.
            let _ = events.send(event);
            if clean_config_now {
                clean_migrated_user_config_keys(&fonts_dir, &config::user_config_path());
            }
        });
    if let Err(err) = spawn_result {
        crate::runtime_log::log_warn(format!(
            "typing: could not spawn the create-preset reader; presets stay unloaded for this \
             session and the read is retried on the next launch: {err}"
        ));
    }
}

/// Reads the preset document and, when one is owed, the legacy payload the migration needs.
/// Returns the event to hand to the GUI thread plus whether the legacy `user_config` keys
/// may be cleaned right away.
///
/// - `Loaded`: use the document and remember its bytes as the writer's baseline. The legacy
///   keys are obsolete, but an earlier run may have died between writing the document and
///   rewriting the config, so that half is retried NOW (nothing is rewritten when the keys
///   are already gone).
/// - `Missing` / `Invalid` (the corrupt file is quarantined first, so the next save cannot
///   destroy a recoverable document): read the legacy `user_config.TextTab.create_presets`
///   payload for the one-shot migration, which is finished on the GUI thread, where the font
///   list exists. The config keys may only be dropped once the new document is written.
pub(super) fn read_presets_seed(fonts_dir: &Path) -> (PresetStoreEvent, bool) {
    match presets_store::load_outcome(fonts_dir) {
        presets_store::LoadOutcome::Loaded {
            presets,
            fingerprint,
        } => {
            presets_store::set_baseline(
                fonts_dir,
                doc_store::SaveBaseline::Matching(fingerprint),
            );
            (
                PresetStoreEvent::Seeded {
                    presets,
                    legacy: None,
                },
                true,
            )
        }
        presets_store::LoadOutcome::Missing => (
            PresetStoreEvent::Seeded {
                presets: HashMap::new(),
                legacy: Some(presets_store::load_legacy_presets()),
            },
            false,
        ),
        presets_store::LoadOutcome::Invalid => {
            // The baseline follows what the quarantine achieved. `Failed` needs no baseline
            // at all: `quarantine_bad_file` has already disabled persistence for this file,
            // because the corrupt document is then the only copy of the user's presets.
            match presets_store::quarantine_bad_file(fonts_dir) {
                // The corrupt file is gone; the next save creates a fresh document.
                presets_store::QuarantineOutcome::Moved => {
                    presets_store::set_baseline(fonts_dir, doc_store::SaveBaseline::Absent);
                }
                // The corrupt file is still in place but its content is preserved in the
                // `.bad` copy, so replacing it is safe — and its bytes are not our baseline.
                presets_store::QuarantineOutcome::Copied
                | presets_store::QuarantineOutcome::Failed => {
                    presets_store::set_baseline(fonts_dir, doc_store::SaveBaseline::Unchecked);
                }
            }
            (
                PresetStoreEvent::Seeded {
                    presets: HashMap::new(),
                    legacy: Some(presets_store::load_legacy_presets()),
                },
                false,
            )
        }
    }
}

/// Writes one preset snapshot and reports the outcome to the GUI thread. The body of the
/// background writer, split out so a test can drive a REAL save (and a real failure) without
/// a thread.
///
/// ORDERING CONTRACT: the legacy `user_config` keys are deleted only after `save` returned
/// `Ok`, and `save` returns only once the document AND its directory entry are durable
/// (`doc_store::Durability::ContentsAndDirectory`). Without that a power loss between the
/// two could leave the presets in neither file.
pub(super) fn run_presets_save(
    fonts_dir: &Path,
    presets: &HashMap<String, TypingCreatePreset>,
    ticket: u64,
    clean_user_config: Option<&Path>,
    events: &Sender<PresetStoreEvent>,
) {
    match presets_store::save(fonts_dir, presets, ticket) {
        Ok(report) => {
            if !report.merged_from_disk.is_empty() {
                // Same reasoning as above: a closed channel means the panel is gone.
                let _ = events.send(PresetStoreEvent::MergedFromDisk(report.merged_from_disk));
            }
            if let Some(user_settings_file) = clean_user_config {
                clean_migrated_user_config_keys(fonts_dir, user_settings_file);
            }
        }
        Err(err) => report_preset_save_failure(events, &err.to_string()),
    }
}

/// Deletes the migrated (and dead) legacy `TextTab` keys off the GUI thread, without writing
/// `presets.json` first. Used when the document is already there (an earlier run wrote it but
/// could not rewrite the config) or when there was nothing to migrate at all; the pass
/// rewrites nothing when no legacy key is present, so it is a cheap no-op from the second
/// launch on. Test-gated like `spawn_presets_save`: a unit test must not touch the real
/// `user_config.json`.
fn spawn_user_config_cleanup(fonts_dir: &Path) {
    if cfg!(test) {
        return;
    }
    let fonts_dir = fonts_dir.to_path_buf();
    let spawn_result = thread::Builder::new()
        .name("typing-clean-legacy-presets-config".to_string())
        .spawn(move || clean_migrated_user_config_keys(&fonts_dir, &config::user_config_path()));
    if let Err(err) = spawn_result {
        crate::runtime_log::log_warn(format!(
            "typing: could not spawn the user_config cleanup thread; the legacy preset keys \
             stay in place and the cleanup retries next launch: {err}"
        ));
    }
}

/// Deletes the legacy `TextTab` keys the preset migration made obsolete and logs the
/// outcome. Which keys those are — in particular whether the imported-system-fonts list may
/// go — is decided by `presets_store::drop_migrated_user_config_keys` from the CONTENT of
/// `fonts_data.json`, never from its mere existence (see that function).
fn clean_migrated_user_config_keys(fonts_dir: &Path, user_settings_file: &Path) {
    match presets_store::drop_migrated_user_config_keys(fonts_dir, user_settings_file) {
        Ok(removed) if removed.is_empty() => {}
        Ok(removed) => crate::runtime_log::log_info(format!(
            "typing: removed migrated legacy keys from user_config.json TextTab: {removed:?}"
        )),
        Err(err) => crate::runtime_log::log_warn(format!(
            "typing: could not remove the migrated legacy preset keys from user_config.json; \
             they stay in place and the cleanup retries next launch: {err}"
        )),
    }
}

/// Logs a preset-save failure and hands the technical reason to the GUI thread, which turns
/// it into a visible status line. The localization happens THERE, not here: `tf!` is a
/// catalog lookup and the message belongs to the frame that shows it.
pub(super) fn report_preset_save_failure(events: &Sender<PresetStoreEvent>, reason: &str) {
    crate::runtime_log::log_error(format!("typing: failed to save fonts/presets.json: {reason}"));
    // A closed channel means the panel that would show the message is gone; the log line
    // above is then the whole record, so the send result is deliberately ignored.
    let _ = events.send(PresetStoreEvent::SaveFailed(reason.to_string()));
}

/// Build the hover tooltip for a font dropdown item, or `None` when the font
/// fully supports the selected typesetting language (no highlight, no tooltip).
///
/// The writing-system name and language name are derived from the currently
/// selected `TextLanguage` (`ms_text_util::language::text_language()`), so the
/// wording is factually correct for any typesetting language, not just Russian.
/// This matches the language `coverage` was classified against: `facade.rs`
/// reloads coverage whenever the typesetting language changes.
fn font_coverage_tooltip(coverage: &FontLanguageCoverage) -> Option<String> {
    let language = ms_text_util::language::text_language();
    // The crate hands us catalog keys (it is GUI-free); resolve them here.
    let language_name = crate::i18n_resolve::resolve_key(language.name_key());
    let script_name = crate::i18n_resolve::resolve_key(language.group().script_name_key());
    match coverage.support {
        FontLanguageSupport::Full => None,
        FontLanguageSupport::Unsupported => Some(tf!("typing.font_coverage.unsupported_tooltip", script_name = script_name, language_name = language_name)),
        FontLanguageSupport::Partial => {
            let list = truncated_char_list(coverage.missing.as_slice());
            Some(tf!("typing.font_coverage.partial_tooltip", language_name = language_name, list = list))
        }
    }
}

/// Renders `chars` as a space-separated list, truncated to [`MAX_SHOWN_CHARS`]
/// with a "+N more" suffix.
///
/// The suffix reuses `typing.font_coverage.more_chars_tooltip`: it is a pure
/// "{shown} … (and N more)" fragment with exactly this meaning, and duplicating
/// the literal into a second key would only let the two drift per locale.
fn truncated_char_list(chars: &[char]) -> String {
    let shown: String = chars
        .iter()
        .take(MAX_SHOWN_CHARS)
        .map(|ch| ch.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let extra = chars.len().saturating_sub(MAX_SHOWN_CHARS);
    if extra > 0 {
        tf!("typing.font_coverage.more_chars_tooltip", shown = shown, extra = extra)
    } else {
        shown
    }
}

/// One user-facing status row of the per-render font diagnostic: the text, the
/// color it is painted in, and the tooltip explaining what it means.
#[derive(Debug)]
pub(super) struct FontFallbackStatusLine {
    pub(super) text: String,
    pub(super) color: egui::Color32,
    pub(super) tooltip: &'static str,
}

/// Turns the renderer's factual fallback report into at most two status rows.
///
/// Row 1 (warning color) lists the characters the deterministic fallback chain
/// drew and the font that drew each group — INFORMATION, not an error: the result
/// is correct and identical on every machine, it just is not the selected
/// typeface. Row 2 (error color) lists characters nothing could draw, which the
/// reader really does lose (a tofu box).
///
/// Returns an empty vector when the selected font served the whole text, so the
/// caller draws nothing at all. Both character lists are truncated by
/// [`truncated_char_list`].
///
/// This is the FACTUAL counterpart of [`font_coverage_tooltip`]: that one judges a
/// FONT against the typesetting LANGUAGE before anything is typed, this one reports
/// what happened to THIS text. Both are kept; they answer different questions.
pub(super) fn font_fallback_status_lines(
    report: &FontFallbackReport,
) -> Vec<FontFallbackStatusLine> {
    let mut lines = Vec::new();
    if !report.fallbacks.is_empty() {
        let list = report
            .fallbacks
            .iter()
            .map(|used| {
                tf!(
                    "typing.font_fallback.entry_label",
                    chars = truncated_char_list(used.chars.as_slice()),
                    font = used.family
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        lines.push(FontFallbackStatusLine {
            text: tf!("typing.font_fallback.used_status", list = list),
            color: FONT_DIAGNOSTIC_WARNING_COLOR,
            tooltip: t!("typing.font_fallback.used_tooltip"),
        });
    }
    if !report.missing.is_empty() {
        lines.push(FontFallbackStatusLine {
            text: tf!(
                "typing.font_fallback.missing_status",
                chars = truncated_char_list(report.missing.as_slice())
            ),
            color: FONT_DIAGNOSTIC_ERROR_COLOR,
            tooltip: t!("typing.font_fallback.missing_tooltip"),
        });
    }
    lines
}
