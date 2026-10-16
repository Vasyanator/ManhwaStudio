/*
File: panel/custom_font_select.rs

Purpose:
The panel's ONE owner of "which identity is persisted, which name is rendered and which
ad-hoc recipe travels with it" for the selected font. For every real font and every saved
custom font the three coincide (identity = render name, no recipe). The ad-hoc custom entry
is different: every layer using it persists the same identity (`MSCustom-adhoc-hybrid`)
next to its OWN recipe, while the renderer — whose `FontProvider::resolve` is name-only —
must get a per-recipe render name (`custom_fonts::adhoc::render_name`).

It also owns what BLOCKS the ad-hoc entry: a recipe not configured yet, a recipe whose
source font is not in the panel list, or a stored recipe that could not be read, puts the
panel into `missing_font` with a hybrid-specific caption (`adhoc_block_caption`), re-derived
after every pick, layer load, preset apply and font reload — such a selection is never
rendered (the create preview is not even attempted) and never rendered with a substitute.
An ad-hoc selection without a recipe is never PERSISTED either
(`selected_adhoc_without_recipe`: no layer, no global or local preset), so a stored ad-hoc
identity without its recipe always means a damaged document.

And it owns the ad-hoc entry's small UI under the font combo (both panels): the row with
«Настроить гибридный шрифт» + the recipe summary + the dock editor's commit status
(`draw_custom_font_row`), and the refusal of the ad-hoc entry as an inline span font
(`refuse_inline_adhoc_pick`).

Key structures:
- `CustomFontSelection`: the ad-hoc recipe held by one panel (installed by layer load, by a
  preset apply and by the hybrid editor's commit), whether the stored one was damaged, and
  the panel's ad-hoc UI flags (configure request, refused inline pick, row status).
- `HybridRowStatus`: the dock editor's commit state as the row shows it.
- `SelectedFontRef`: the triple handed to the three write sites.

Key functions:
- `TypingCreatePanelState::selected_font_ref`.
- `TypingCreatePanelState::select_adhoc_font` (stored recipe -> selection + block state),
  `install_adhoc_recipe` (a recipe the user configured), `refresh_adhoc_missing_font`,
  `selected_adhoc_without_recipe` / `adhoc_unconfigured_status` (the persistence refusal).
- `TypingCreatePanelState::draw_custom_font_row`, `refuse_inline_adhoc_pick`,
  `take_hybrid_configure_request`.
- `hybrid_summary`: `Font A 60% + Font B 40%`.
- `missing_font_label`: the name a missing font is shown under (a saved custom font by its
  stored name).

Notes:
Consumers: `create_apply::build_render_params_for` (render name), `create_render_data::
build_render_data_json_for` (identity + `font_recipe`), `create_apply::editor_font_spec`
(the ad-hoc entry draws the on-canvas editor in the interface font) and the advanced-form
metric resolve (`create_advanced::poll_advanced_form_font`). None of them may branch on the
ad-hoc entry themselves.
*/

use super::*;
use crate::custom_fonts::adhoc;
use crate::custom_fonts::identity::{ADHOC_CUSTOM_FONT_IDENTITY, is_saved_identity};
use crate::hybrid_editor::weight_shares;
use ms_custom_fonts::CustomFontKind;

/// The ad-hoc custom font state of ONE panel.
#[derive(Debug, Clone, Default)]
pub(super) struct CustomFontSelection {
    /// The recipe of the ad-hoc entry while it is the selection's font; `None` = not
    /// configured yet (blocked with the "not configured" caption, never rendered or
    /// persisted).
    pub(super) adhoc_recipe: Option<Arc<CustomFontRecipe>>,
    /// The ad-hoc recipe a loaded layer or preset stored could not be read (`adhoc_recipe`
    /// is then `None`): the entry is blocked as damaged until the user configures it anew,
    /// instead of passing for merely "not configured yet".
    pub(super) adhoc_recipe_damaged: bool,
    /// «Настроить гибридный шрифт» was clicked this frame; drained by the facade
    /// (`hybrid_dock::drain_hybrid_dock_request`), which toggles the dock tab.
    pub(super) configure_requested: bool,
    /// The user picked the ad-hoc entry as an inline span font and it was refused; the
    /// explanation stays under the combo until the next accepted pick or the selection ends.
    pub(super) inline_pick_refused: bool,
    /// The dock editor's commit state for THIS panel, pushed every frame by the facade
    /// (`hybrid_dock::sync_hybrid_dock`); `Idle` when the editor edits the other panel.
    pub(super) row_status: HybridRowStatus,
}

/// The hybrid dock editor's full-face commit as the row under the font combo shows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum HybridRowStatus {
    /// Nothing to show.
    #[default]
    Idle,
    /// The full face is being built; `(done, total)` glyphs once the compiler reported.
    Compiling(Option<(usize, usize)>),
    /// The last commit failed (logged by the editor).
    Failed,
}

/// The selected font as the write sites need it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SelectedFontRef {
    /// What a layer / preset persists under `text_params.font`.
    pub(super) identity: String,
    /// What `TextRenderParams.font_name` carries — the provider's lookup key. Equal to
    /// `identity` except for a configured ad-hoc entry, whose render name is a pure
    /// function of its recipe.
    pub(super) render_name: String,
    /// The ad-hoc recipe persisted next to the identity (`text_params.font_recipe`);
    /// `None` for every other font.
    pub(super) recipe: Option<Arc<CustomFontRecipe>>,
}

impl CustomFontSelection {
    /// The triple for `font` under this selection state.
    #[must_use]
    pub(super) fn font_ref(&self, font: &FontEntry) -> SelectedFontRef {
        if !font.is_adhoc_custom() {
            let identity = font.render_identity_name();
            return SelectedFontRef { render_name: identity.clone(), identity, recipe: None };
        }
        match &self.adhoc_recipe {
            Some(recipe) => SelectedFontRef {
                identity: ADHOC_CUSTOM_FONT_IDENTITY.to_owned(),
                render_name: adhoc::render_name(recipe),
                recipe: Some(Arc::clone(recipe)),
            },
            // Not configured: the persisted identity is the only name there is. The provider
            // does not register it, so a render reports the font as missing instead of
            // silently drawing something else.
            None => SelectedFontRef {
                identity: ADHOC_CUSTOM_FONT_IDENTITY.to_owned(),
                render_name: ADHOC_CUSTOM_FONT_IDENTITY.to_owned(),
                recipe: None,
            },
        }
    }
}

impl TypingCreatePanelState {
    /// The selected font's identity / render name / ad-hoc recipe, or `None` when no font
    /// is selected (an empty list).
    #[must_use]
    pub(super) fn selected_font_ref(&self) -> Option<SelectedFontRef> {
        let font = self.fonts.get(self.selected_font_idx)?;
        Some(self.custom_font.font_ref(font))
    }

    /// Whether the selected entry is the ad-hoc custom font.
    #[must_use]
    pub(super) fn selected_font_is_adhoc(&self) -> bool {
        self.fonts.get(self.selected_font_idx).is_some_and(FontEntry::is_adhoc_custom)
    }

    /// Selects the ad-hoc entry for a layer / preset that stores the ad-hoc identity, with
    /// the stored recipe as `custom_font_use` read it (`Err` = missing or damaged, carrying
    /// the log-ready reason), and derives the block state (`refresh_adhoc_missing_font`).
    ///
    /// The ad-hoc entry is on every panel list (`fonts::prepend_synthetic_entries`); should
    /// it ever be absent, the panel lands in `missing_font` like any unknown identity.
    pub(super) fn select_adhoc_font(&mut self, stored: Result<Arc<CustomFontRecipe>, String>) {
        let Some(idx) = self.find_font_idx_by_identity(ADHOC_CUSTOM_FONT_IDENTITY) else {
            self.missing_font = Some(ADHOC_CUSTOM_FONT_IDENTITY.to_owned());
            return;
        };
        self.selected_font_idx = idx;
        self.active_font_identity = self.current_font_identity();
        match stored {
            Ok(recipe) => {
                self.custom_font.adhoc_recipe = Some(recipe);
                self.custom_font.adhoc_recipe_damaged = false;
            }
            Err(reason) => {
                ms_log::runtime_log::log_warn(format!(
                    "typing: the ad-hoc custom font recipe of the loaded layer or preset is \
                     unusable ({reason}); the layer is shown as using a missing font and is not \
                     re-rendered until a font is chosen."
                ));
                self.custom_font.adhoc_recipe = None;
                self.custom_font.adhoc_recipe_damaged = true;
            }
        }
        self.refresh_adhoc_missing_font();
    }

    /// Installs `recipe` as this panel's ad-hoc recipe — one the user configured (the hybrid
    /// editor's commit) or a preset carried — clearing any damaged state, and re-derives the
    /// block state when the ad-hoc entry is selected. `None` = not configured.
    pub(super) fn install_adhoc_recipe(&mut self, recipe: Option<Arc<CustomFontRecipe>>) {
        self.custom_font.adhoc_recipe = recipe;
        self.custom_font.adhoc_recipe_damaged = false;
        self.refresh_adhoc_missing_font();
    }

    /// What blocks rendering of the selected ad-hoc entry, as the `missing_font` caption:
    /// a damaged stored recipe, a recipe not configured yet (the entry was just picked: there
    /// is no face to render, and the row's «Настроить…» button is the way out), or the
    /// recipe's source fonts that are not among the panel's non-custom entries (the bundled
    /// interface font included). `None` when the ad-hoc entry is not selected or every source
    /// of its recipe is present.
    ///
    /// Sources are matched by IDENTITY only, the panel's one selection key; the hybrid
    /// editor always stores identities.
    #[must_use]
    pub(super) fn adhoc_block_caption(&self) -> Option<String> {
        if !self.selected_font_is_adhoc() {
            return None;
        }
        if self.custom_font.adhoc_recipe_damaged {
            return Some(t!("typing.hybrid_font.invalid_recipe_caption").to_string());
        }
        let Some(recipe) = self.custom_font.adhoc_recipe.as_ref() else {
            return Some(t!("typing.hybrid_font.not_configured_caption").to_string());
        };
        let missing: Vec<&str> = recipe
            .source_identities()
            .into_iter()
            .filter(|identity| !self.is_available_source(identity))
            .collect();
        (!missing.is_empty())
            .then(|| tf!("typing.hybrid_font.missing_source_caption", font = missing.join(", ")).to_string())
    }

    /// Whether the selected font is the ad-hoc entry WITHOUT a recipe (not configured yet, or
    /// its stored recipe was damaged). Nothing may be persisted from such a selection — no
    /// layer, no global or local preset — because the ad-hoc identity without its recipe
    /// describes no font (it would read back as damaged). Callers refuse with
    /// [`Self::adhoc_unconfigured_status`].
    #[must_use]
    pub(super) fn selected_adhoc_without_recipe(&self) -> bool {
        self.selected_font_is_adhoc() && self.custom_font.adhoc_recipe.is_none()
    }

    /// The status line of an action refused by [`Self::selected_adhoc_without_recipe`]: it
    /// names the row's configure button by its label.
    #[must_use]
    pub(super) fn adhoc_unconfigured_status() -> String {
        tf!("typing.hybrid_font.not_configured_blocked_status", button = t!("typing.hybrid_font.configure_button"))
    }

    /// Re-derives `missing_font` from [`Self::adhoc_block_caption`] when the ad-hoc entry is
    /// selected (setting or clearing it); leaves it alone for every other font, whose missing
    /// state is decided by the identity lookup that selected it.
    pub(super) fn refresh_adhoc_missing_font(&mut self) {
        if self.selected_font_is_adhoc() {
            self.missing_font = self.adhoc_block_caption();
        }
    }

    /// Whether `identity` names a font a hybrid may take as a source in this list: a real or
    /// the bundled entry, never a custom one (decision D4).
    fn is_available_source(&self, identity: &str) -> bool {
        self.find_font_idx_by_identity(identity)
            .and_then(|idx| self.fonts.get(idx))
            .is_some_and(|font| !matches!(font.kind, FontEntryKind::Custom(_)))
    }

    /// The name a recipe source is shown under: the list entry's display label, or the bare
    /// identity when the source is not in this list (it is then also named by the
    /// missing-source caption).
    fn source_label(&self, identity: &str) -> String {
        self.find_font_idx_by_identity(identity)
            .and_then(|idx| self.fonts.get(idx))
            .map_or_else(|| identity.to_owned(), |font| font.display_label().to_owned())
    }

    /// Draws the ad-hoc entry's row under the font combo — only while that entry is the
    /// selected font: «Настроить гибридный шрифт» (raises `configure_requested`, the facade
    /// toggles the dock tab), the recipe summary (`hybrid_summary`, or «Шрифты не выбраны»)
    /// and the dock editor's commit status. A missing source or a damaged recipe is shown
    /// by the panel's missing-font UI (`adhoc_block_caption`), not repeated here. GUI
    /// thread, no I/O.
    pub(super) fn draw_custom_font_row(&mut self, ui: &mut egui::Ui) {
        if !self.selected_font_is_adhoc() {
            return;
        }
        let summary = self.custom_font.adhoc_recipe.as_deref().map_or_else(
            || t!("typing.hybrid_font.not_configured_status").to_owned(),
            |recipe| hybrid_summary(recipe, |identity| self.source_label(identity)),
        );
        ui.horizontal_wrapped(|ui| {
            if ui.button(t!("typing.hybrid_font.configure_button")).clicked() {
                self.custom_font.configure_requested = true;
            }
            ui.label(egui::RichText::new(summary).weak());
        });
        match self.custom_font.row_status {
            HybridRowStatus::Idle => {}
            HybridRowStatus::Compiling(progress) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    // Before the compiler's first report there are no counts to show; the
                    // spinner alone says the build is running.
                    if let Some((done, total)) = progress {
                        ui.label(tf!("typing.hybrid_font.compile_progress_status", done = done, total = total));
                    }
                });
            }
            HybridRowStatus::Failed => {
                ui.colored_label(ms_theme::status::ERROR, t!("typing.hybrid_font.compile_failed_error"));
            }
        }
    }

    /// Decides an inline-selection font pick of entry `picked`: the ad-hoc entry cannot be a
    /// span font (a span tag carries an identity only, and the ad-hoc identity means
    /// nothing without the layer's recipe), so it is REFUSED — returns `true` and keeps the
    /// explanation on screen (`draw_inline_adhoc_refusal`). Any other pick (saved custom
    /// fonts included) is accepted and clears the explanation.
    #[must_use]
    pub(super) fn refuse_inline_adhoc_pick(&mut self, picked: usize) -> bool {
        let refused = self.fonts.get(picked).is_some_and(FontEntry::is_adhoc_custom);
        self.custom_font.inline_pick_refused = refused;
        refused
    }

    /// Draws why the last inline pick was refused while the inline selection lasts; leaving
    /// the selection mode forgets the refusal.
    pub(super) fn draw_inline_adhoc_refusal(&mut self, ui: &mut egui::Ui, selection_mode: bool) {
        if !selection_mode {
            self.custom_font.inline_pick_refused = false;
            return;
        }
        if self.custom_font.inline_pick_refused {
            ui.colored_label(ms_theme::status::WARNING, t!("typing.hybrid_font.inline_unsupported_hint"));
        }
    }

    /// Draws the custom-font fallback hint (`create_presets::custom_font_fallback_hint`) of
    /// the create panel's last preview render. Only the create panel renders a preview, so
    /// the edit panel never draws it.
    pub(super) fn draw_custom_font_fallback_hint(&self, ui: &mut egui::Ui) {
        if !self.preview_enabled {
            return;
        }
        let selected_is_custom = self.fonts.get(self.selected_font_idx).is_some_and(|font| font.custom_font().is_some());
        if let Some(hint) = create_presets::custom_font_fallback_hint(&self.preview_font_fallbacks, selected_is_custom) {
            ui.label(egui::RichText::new(hint).weak().small());
        }
    }

    /// The font index the non-inline combo writeback adopts: `combo_font_idx` (the combo's
    /// marked row) unless that row is the ad-hoc entry and the user did not pick it
    /// (`user_pick` is `None`). The ad-hoc entry belongs to every group (`font_in_group`), so
    /// the combo's display clamp — "a font outside the active group marks the first visible
    /// row" — can land on it; it must never become the layer's font that way. The current
    /// selection is kept instead.
    #[must_use]
    pub(super) fn combo_font_idx_to_adopt(&self, combo_font_idx: usize, user_pick: Option<usize>) -> usize {
        let clamped_onto_adhoc =
            user_pick.is_none() && self.fonts.get(combo_font_idx).is_some_and(FontEntry::is_adhoc_custom);
        if clamped_onto_adhoc { self.selected_font_idx } else { combo_font_idx }
    }

    /// Takes the «Настроить гибридный шрифт» click of this frame (`true` once).
    pub(super) fn take_hybrid_configure_request(&mut self) -> bool {
        std::mem::take(&mut self.custom_font.configure_requested)
    }
}

/// The ad-hoc recipe's one-line summary under the font combo: every source as
/// `typing.hybrid_font.summary_part` («{font} {percent}%») joined by ` + `, the share being
/// the weight's normalized percentage (`hybrid_editor::weight_shares`, the editor's own
/// rule; a weight without a valid share shows 0). With separate property weights the share
/// is the SHAPE share (`HybridParams::effective_weights`), as in the editor's rows. `label`
/// names a source identity.
#[must_use]
pub(super) fn hybrid_summary(recipe: &CustomFontRecipe, label: impl Fn(&str) -> String) -> String {
    match &recipe.kind {
        CustomFontKind::Hybrid(params) => {
            // `effective_weights` is the one owner of which weight applies where.
            let weights = params.effective_weights().shape;
            params
                .sources
                .iter()
                .zip(weight_shares(&weights))
                .map(|(source, share)| {
                    let percent = format!("{:.0}", share.unwrap_or(0.0));
                    tf!("typing.hybrid_font.summary_part", font = label(&source.identity), percent = percent)
                })
                .collect::<Vec<_>>()
                // A math sign, not a word: the same in every locale (like the `", "` joins of
                // the missing-source caption above).
                .join(" + ")
        }
    }
}

/// The name a missing font `identity` is shown under in `missing_font`: a saved custom font
/// by its stored user-visible name when the store still knows it (the opaque identity would
/// mean nothing to the user), every other font by its identity.
#[must_use]
pub(super) fn missing_font_label(identity: &str) -> String {
    if is_saved_identity(identity)
        && let Some(name) = font_settings_store::custom_font_name(identity)
    {
        return name;
    }
    identity.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_fonts::identity::is_adhoc_render_name;
    use crate::panel::custom_font_entry::adhoc_custom_font_entry;
    use ms_custom_fonts::{CustomFontKind, HybridParams, HybridSource, MissingGlyphPolicy};

    fn recipe() -> Arc<CustomFontRecipe> {
        Arc::new(CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: vec![
                    HybridSource { identity: "Alpha-Regular".to_owned(), weight: 1.0, missing_override: None },
                    HybridSource { identity: "Beta-Regular".to_owned(), weight: 3.0, missing_override: None },
                ],
                missing_default: MissingGlyphPolicy::NoInfluence,
                algorithm: ms_custom_fonts::BlendAlgorithm::Auto,
                property_weights: None,
            }),
        })
    }

    fn file_entry(identity: &str) -> FontEntry {
        FontEntry {
            kind: FontEntryKind::File,
            label: identity.to_owned(),
            path: PathBuf::from(format!("/fonts/{identity}.ttf")),
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces: default_single_face(),
            coverage: FontLanguageCoverage::default(),
            original_name: identity.to_owned(),
            post_script_name: identity.to_owned(),
            content_hash: 1,
            display_name: None,
            identity_name: identity.to_owned(),
            custom_kerning: Vec::new(),
            virtual_group_aliases: BTreeMap::new(),
        }
    }

    #[test]
    fn a_real_font_renders_and_persists_its_identity() {
        let selection = CustomFontSelection { adhoc_recipe: Some(recipe()), ..CustomFontSelection::default() };
        let font_ref = selection.font_ref(&file_entry("Alpha-Regular"));
        assert_eq!(font_ref.identity, "Alpha-Regular");
        assert_eq!(font_ref.render_name, "Alpha-Regular");
        assert!(font_ref.recipe.is_none(), "a recipe never travels with a real font");
    }

    #[test]
    fn the_adhoc_entry_persists_the_reserved_identity_and_renders_by_recipe() {
        let adhoc_entry = adhoc_custom_font_entry();
        let configured = CustomFontSelection { adhoc_recipe: Some(recipe()), ..CustomFontSelection::default() }.font_ref(&adhoc_entry);
        assert_eq!(configured.identity, ADHOC_CUSTOM_FONT_IDENTITY);
        assert!(is_adhoc_render_name(&configured.render_name));
        assert_eq!(configured.recipe.as_deref(), Some(&*recipe()));
        assert_eq!(
            adhoc::recipe_for_render_name(&configured.render_name).as_deref(),
            Some(&*recipe()),
            "the render name must resolve back to the recipe through the interner"
        );

        let unconfigured = CustomFontSelection::default().font_ref(&adhoc_entry);
        assert_eq!(unconfigured.identity, ADHOC_CUSTOM_FONT_IDENTITY);
        assert_eq!(unconfigured.render_name, ADHOC_CUSTOM_FONT_IDENTITY);
        assert!(unconfigured.recipe.is_none());
    }

    #[test]
    fn the_panel_selection_goes_through_the_same_rule() {
        let mut state = TypingCreatePanelState::new(false);
        state.fonts = vec![file_entry("Alpha-Regular"), adhoc_custom_font_entry()];
        state.custom_font.adhoc_recipe = Some(recipe());
        state.selected_font_idx = 0;
        assert_eq!(state.selected_font_ref().map(|r| r.render_name), Some("Alpha-Regular".to_owned()));
        state.selected_font_idx = 1;
        let selected = state.selected_font_ref().expect("the ad-hoc entry is selected");
        assert!(is_adhoc_render_name(&selected.render_name));
        state.fonts.clear();
        assert!(state.selected_font_ref().is_none());
    }
}
