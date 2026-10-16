/*
File: hybrid_editor/mod.rs

Purpose:
The shared hybrid-font editor (`dev-docs/hybrid_fonts_plan.md` §3.3): one widget body drawn
by every host — the typing dock panel (a layer's ad-hoc recipe) and Settings › Тайп (create
dialog, properties window of a saved font). The body decides nothing host-specific: the host
only says what its commit button is (`CommitButton`) and what a committed recipe means.

Key structures:
- `HybridEditorCx`: what the host lends per frame (source catalog, commit button mode).
- `CommitButton`: `Apply` (the editor draws «Применить») or `HostDrawn` (the host draws
  its own Save and calls `HybridEditorState::start_commit`).
- `HybridEditorOutcome`: `Idle` / `Committed(CommittedRecipe)` / `CommitFailed`.

Key functions:
- `draw_hybrid_editor`: source rows (font picker in each font's own typeface, weight +
  normalized share — or, with separate weights, a second line of shape / spacing / kerning
  weights and the shape share — per-source missing-glyph override, remove), «+ Добавить
  шрифт» (2..=4, decision D3), the default policy, the blend algorithm (each option's
  trade-off on hover and under the row), the transfer base font, the «Раздельные веса»
  toggle, the coverage summary, the dropped-features hint, the preview (sample text,
  subset-hybrid render with its skeleton-fallback / base-substitution status, one comparison
  row per source), and the commit progress / cancel / status.

Submodules: `state` (draft, commit), `catalog` (allowed sources), `preview` (worker),
`coverage` (summary). See `MODULE_README.md`.

Notes:
Nothing here blocks the GUI thread: previews, coverage and the full compile run on workers;
this file only reads their results and uploads textures. Widgets follow
`egui-docs/04-widgets.md` (no stock Slider / ComboBox / DragValue) and every localized
widget carries a stable `id_salt`.
*/

mod catalog;
mod coverage;
mod preview;
mod state;

use eframe::egui;
use ms_custom_fonts::{MAX_HYBRID_SOURCES, MIN_HYBRID_SOURCES, MissingGlyphPolicy};
use ms_widgets::{PreviewFontFamily, RowLayout, SearchableComboBox, SearchableComboItem, WheelComboBox, WheelSpinBox};

pub use catalog::SourceCatalog;
pub use state::{CommittedRecipe, DraftError, HybridEditorState};
// The one owner of "a weight's share in percent", also used by the typing panel's recipe
// summary under the font combo (`panel::custom_font_select::hybrid_summary`).
pub(crate) use state::weight_shares;

use preview::{PreviewSlot, TickInput};
use state::{
    ALGORITHM_CHOICES, BlendAlgorithmChoice, CommitPoll, DEFAULT_POLICY_CHOICES, DraftSource, MAX_SOURCE_WEIGHT,
    MIN_SOURCE_WEIGHT, OVERRIDE_CHOICES, WEIGHT_WHEEL_STEP, algorithm_from_index, algorithm_index, can_add_row, can_remove_row, cycle_index,
    default_policy_index, override_from_index, override_index, skeleton_fallback_counts,
};

/// Width of a source row's font picker (both of its buttons), in points.
const SOURCE_PICKER_WIDTH_PT: f32 = 230.0;
/// Width of the per-source policy combo, in points.
const POLICY_COMBO_WIDTH_PT: f32 = 120.0;
/// Width of the algorithm and base-font combos, in points.
const ALGORITHM_COMBO_WIDTH_PT: f32 = 180.0;
/// Point size of a picker row's main line (the font's own typeface).
const SOURCE_PICKER_PRIMARY_PT: f32 = 15.0;
/// The remove-row icon: a glyph chosen for its shape, not a word
/// (`dev-docs/i18n_exclusions.md` §A8 rule); its meaning is the localized tooltip.
const REMOVE_ICON: &str = "✕";

/// What the host's commit control is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitButton {
    /// The editor draws «Применить» (enabled for a valid, changed draft).
    Apply,
    /// The host draws its own control (e.g. the settings dialog's «Сохранить») and calls
    /// `HybridEditorState::start_commit`; the editor still shows progress and cancel.
    HostDrawn,
}

/// What the host lends the editor for one frame.
#[derive(Debug, Clone, Copy)]
pub struct HybridEditorCx<'a> {
    /// The fonts offered as sources and their resolver.
    pub catalog: &'a SourceCatalog,
    /// The commit control mode.
    pub commit: CommitButton,
}

/// What one frame of the editor reports to the host.
#[derive(Debug)]
pub enum HybridEditorOutcome {
    /// Nothing to act on.
    Idle,
    /// The full face of the draft is built; the host persists `recipe` as its commit means.
    Committed(CommittedRecipe),
    /// The commit failed (logged; the editor shows the error line).
    CommitFailed,
}

/// Draws one frame of the editor and reports a finished commit.
///
/// GUI thread; never blocks. Requests repaints itself while a preview or a commit is
/// pending.
pub fn draw_hybrid_editor(ui: &mut egui::Ui, state: &mut HybridEditorState, cx: &HybridEditorCx<'_>) -> HybridEditorOutcome {
    let outcome = match state.poll_commit() {
        CommitPoll::Nothing => HybridEditorOutcome::Idle,
        CommitPoll::Committed(committed) => HybridEditorOutcome::Committed(committed),
        CommitPoll::Failed => HybridEditorOutcome::CommitFailed,
    };
    let id = state.id;
    ui.push_id(id, |ui| {
        draw_sources(ui, state, cx.catalog);
        ui.add_space(4.0);
        draw_default_policy(ui, state);
        draw_algorithm(ui, state, cx.catalog);
        draw_split_toggle(ui, state);
        if let Some(summary) = state.preview.coverage.summary() {
            ui.label(summary);
        }
        ui.label(egui::RichText::new(t!("typing.hybrid_font.dropped_features_hint")).weak().small());
        ui.separator();
        draw_preview(ui, state, cx.catalog);
        ui.separator();
        draw_commit(ui, state, cx);
    });
    if state.is_committing() {
        // The compile ticket has no wake-up of its own: poll its progress and result.
        ui.ctx().request_repaint_after(preview::POLL_INTERVAL);
    }
    outcome
}

/// The source rows and «+ Добавить шрифт».
fn draw_sources(ui: &mut egui::Ui, state: &mut HybridEditorState, catalog: &SourceCatalog) {
    ui.strong(t!("typing.hybrid_font.sources_heading"));
    ui.label(
        egui::RichText::new(tf!("typing.hybrid_font.sources_limit_hint", min = MIN_HYBRID_SOURCES, max = MAX_HYBRID_SOURCES))
            .weak()
            .small(),
    );
    let items: Vec<SearchableComboItem<'_>> =
        catalog.items().iter().map(|item| SearchableComboItem::with_secondary(item.label(), item.identity())).collect();
    // With separate weights the share shown is the SHAPE share: it is what the glyphs look
    // like, the part of the blend a user judges by eye.
    let split = state.split_weights;
    let weights: Vec<f64> = state.rows.iter().map(|row| f64::from(if split { row.shape } else { row.weight })).collect();
    let shares = weight_shares(&weights);
    let removable = can_remove_row(state.rows.len());
    let mut changed = false;
    let mut remove = None;
    for (index, (row, share)) in state.rows.iter_mut().zip(shares).enumerate() {
        ui.push_id(index, |ui| {
            ui.horizontal(|ui| {
                changed |= draw_source_picker(ui, row, catalog, &items);
                if !split {
                    changed |= ui.add(weight_spin_box(&mut row.weight, t!("typing.hybrid_font.source_weight_label"))).changed();
                }
                let percent = share.map_or_else(String::new, |share| format!("{share:.0}"));
                ui.label(tf!("typing.hybrid_font.source_weight_share_label", percent = percent));
                changed |= draw_source_policy(ui, row);
                let remove_button = ui
                    .add_enabled(removable, egui::Button::new(REMOVE_ICON).small())
                    .on_hover_text(t!("typing.hybrid_font.source_remove_tooltip"))
                    .on_disabled_hover_text(tf!(
                        "typing.hybrid_font.sources_limit_hint",
                        min = MIN_HYBRID_SOURCES,
                        max = MAX_HYBRID_SOURCES
                    ));
                if remove_button.clicked() {
                    remove = Some(index);
                }
            });
            if split {
                // Its own id scope: the line appears and disappears with the toggle, which
                // must not shift the ids of the widgets after it.
                ui.push_id("typing.hybrid_font.split_weights_label", |ui| {
                    ui.horizontal(|ui| {
                        changed |= ui.add(weight_spin_box(&mut row.shape, t!("typing.hybrid_font.weight_shape_label"))).changed();
                        changed |= ui.add(weight_spin_box(&mut row.spacing, t!("typing.hybrid_font.weight_spacing_label"))).changed();
                        changed |= ui.add(weight_spin_box(&mut row.kerning, t!("typing.hybrid_font.weight_kerning_label"))).changed();
                    });
                });
            }
        });
    }
    if let Some(index) = remove.filter(|_| removable) {
        // `remove_row` bumps the revision and keeps the transfer base on the same font.
        state.remove_row(index);
    }
    let add = ui
        .add_enabled(can_add_row(state.rows.len()), egui::Button::new(t!("typing.hybrid_font.add_source_button")))
        .on_disabled_hover_text(tf!("typing.hybrid_font.sources_limit_hint", min = MIN_HYBRID_SOURCES, max = MAX_HYBRID_SOURCES));
    if add.clicked() && can_add_row(state.rows.len()) {
        // `add_row` bumps the revision and seeds the property weights while split is on.
        state.add_row();
    }
    if changed {
        state.touch();
    }
}

/// A weight field (main weight or one property weight): the same range, step and decimals
/// for all four, prefixed with its localized name.
fn weight_spin_box<'a>(value: &'a mut f32, name: &str) -> WheelSpinBox<'a> {
    WheelSpinBox::new(value)
        .range(MIN_SOURCE_WEIGHT..=MAX_SOURCE_WEIGHT)
        .speed(0.1)
        .wheel_step(WEIGHT_WHEEL_STEP)
        .max_decimals(1)
        .prefix(format!("{name} "))
}

/// One row's font picker; returns whether the chosen font changed.
fn draw_source_picker(ui: &mut egui::Ui, row: &mut DraftSource, catalog: &SourceCatalog, items: &[SearchableComboItem<'_>]) -> bool {
    let current = row.identity.as_deref().and_then(|identity| catalog.position(identity));
    // An unfilled slot shows the placeholder; a chosen font that is no longer offered (its
    // file went away) shows its identity rather than silently another row.
    let caption = match (&row.identity, current) {
        (None, _) => Some(t!("typing.hybrid_font.source_pick_placeholder").to_owned()),
        (Some(identity), None) => Some(identity.clone()),
        (Some(_), Some(_)) => None,
    };
    let mut position = current.unwrap_or(usize::MAX);
    // The resolver runs while the widget holds `&mut Ui`: it gets an owned `Context`.
    let ctx = ui.ctx().clone();
    let mut resolve_family = |index: usize| -> Option<egui::FontFamily> {
        let entry = catalog.items().get(index)?.entry();
        match entry.byte_source().preview_source().request_family(
            &ctx,
            &entry.render_identity_name(),
            entry.content_hash(),
            entry.representative_face_index(),
        ) {
            PreviewFontFamily::Ready(family) => Some(family),
            // Both draw the row in the interface font; `Pending` retries on a later frame.
            PreviewFontFamily::Pending | PreviewFontFamily::Unavailable => None,
        }
    };
    let mut combo = SearchableComboBox::new("typing.hybrid_font.source_pick_placeholder")
        .row_layout(RowLayout::Wide)
        .primary_size(SOURCE_PICKER_PRIMARY_PT)
        .width(SOURCE_PICKER_WIDTH_PT)
        .item_font(&mut resolve_family);
    if let Some(caption) = caption {
        combo = combo.selected_text(caption);
    }
    let shown = combo.show(ui, &mut position, items);
    if !shown.changed && shown.picked.is_none() {
        return false;
    }
    let picked = shown.picked.unwrap_or(position);
    match catalog.items().get(picked) {
        Some(item) if row.identity.as_deref() != Some(item.identity()) => {
            row.identity = Some(item.identity().to_owned());
            true
        }
        Some(_) | None => false,
    }
}

/// The label of a per-source override choice.
fn override_label(choice: Option<MissingGlyphPolicy>) -> &'static str {
    match choice {
        None => t!("typing.hybrid_font.source_policy_inherit_option"),
        Some(MissingGlyphPolicy::Disable) => t!("typing.hybrid_font.source_policy_disable_option"),
        Some(MissingGlyphPolicy::NoInfluence) => t!("typing.hybrid_font.source_policy_no_influence_option"),
    }
}

/// One row's missing-glyph override combo; returns whether it changed.
fn draw_source_policy(ui: &mut egui::Ui, row: &mut DraftSource) -> bool {
    let mut index = override_index(row.missing_override);
    let response = WheelComboBox::from_id_salt("typing.hybrid_font.source_policy_tooltip")
        .width(POLICY_COMBO_WIDTH_PT)
        .show_index(ui, &mut index, OVERRIDE_CHOICES.len(), |choice| override_label(override_from_index(choice)))
        .on_hover_text(t!("typing.hybrid_font.source_policy_tooltip"));
    let chosen = override_from_index(index);
    if response.changed() && chosen != row.missing_override {
        row.missing_override = chosen;
        return true;
    }
    false
}

/// The label and tooltip of a default-policy choice.
fn default_policy_texts(policy: MissingGlyphPolicy) -> (&'static str, &'static str) {
    match policy {
        MissingGlyphPolicy::Disable => {
            (t!("typing.hybrid_font.missing_policy_disable_option"), t!("typing.hybrid_font.missing_policy_disable_tooltip"))
        }
        MissingGlyphPolicy::NoInfluence => (
            t!("typing.hybrid_font.missing_policy_no_influence_option"),
            t!("typing.hybrid_font.missing_policy_no_influence_tooltip"),
        ),
    }
}

/// The hybrid's default missing-glyph policy (each option explains itself on hover).
fn draw_default_policy(ui: &mut egui::Ui, state: &mut HybridEditorState) {
    let mut chosen = state.missing_default;
    ui.horizontal(|ui| {
        ui.label(t!("typing.hybrid_font.missing_policy_label"));
        let (current_label, current_tooltip) = default_policy_texts(chosen);
        let combo = WheelComboBox::from_id_salt("typing.hybrid_font.missing_policy_label").selected_text(current_label).show_ui_with_wheel(
            ui,
            |ui| {
                for policy in DEFAULT_POLICY_CHOICES {
                    let (label, tooltip) = default_policy_texts(policy);
                    if ui.selectable_label(policy == chosen, label).on_hover_text(tooltip).clicked() {
                        chosen = policy;
                    }
                }
            },
        );
        combo.inner.response.on_hover_text(current_tooltip);
        if let Some(steps) = combo.wheel_steps {
            let index = cycle_index(default_policy_index(chosen), DEFAULT_POLICY_CHOICES.len(), steps);
            if let Some(policy) = DEFAULT_POLICY_CHOICES.get(index) {
                chosen = *policy;
            }
        }
    });
    if chosen != state.missing_default {
        state.missing_default = chosen;
        state.touch();
    }
}

/// The label and the one-line trade-off hint of an algorithm choice.
fn algorithm_texts(choice: BlendAlgorithmChoice) -> (&'static str, &'static str) {
    match choice {
        BlendAlgorithmChoice::Auto => (t!("typing.hybrid_font.algorithm_auto_option"), t!("typing.hybrid_font.algorithm_auto_hint")),
        BlendAlgorithmChoice::Sdf => (t!("typing.hybrid_font.algorithm_sdf_option"), t!("typing.hybrid_font.algorithm_sdf_hint")),
        BlendAlgorithmChoice::NoBlend => {
            (t!("typing.hybrid_font.algorithm_no_blend_option"), t!("typing.hybrid_font.algorithm_no_blend_hint"))
        }
        BlendAlgorithmChoice::Skeleton => {
            (t!("typing.hybrid_font.algorithm_skeleton_option"), t!("typing.hybrid_font.algorithm_skeleton_hint"))
        }
        BlendAlgorithmChoice::Transfer => {
            (t!("typing.hybrid_font.algorithm_transfer_option"), t!("typing.hybrid_font.algorithm_transfer_hint"))
        }
    }
}

/// The blend-algorithm combo (each option explains itself on hover, the current one also
/// under the row) and, for parameter transfer, the base-font combo. A change only re-renders
/// the preview (D5); the full face is built on commit.
fn draw_algorithm(ui: &mut egui::Ui, state: &mut HybridEditorState, catalog: &SourceCatalog) {
    let mut chosen = state.algorithm;
    ui.horizontal(|ui| {
        ui.label(t!("typing.hybrid_font.algorithm_label"));
        let (current_label, current_hint) = algorithm_texts(chosen);
        let combo = WheelComboBox::from_id_salt("typing.hybrid_font.algorithm_label")
            .width(ALGORITHM_COMBO_WIDTH_PT)
            .selected_text(current_label)
            .show_ui_with_wheel(ui, |ui| {
                for choice in ALGORITHM_CHOICES {
                    let (label, hint) = algorithm_texts(choice);
                    if ui.selectable_label(choice == chosen, label).on_hover_text(hint).clicked() {
                        chosen = choice;
                    }
                }
            });
        combo.inner.response.on_hover_text(current_hint);
        if let Some(steps) = combo.wheel_steps {
            chosen = algorithm_from_index(cycle_index(algorithm_index(chosen), ALGORITHM_CHOICES.len(), steps));
        }
    });
    if chosen != state.algorithm {
        state.algorithm = chosen;
        state.touch();
    }
    ui.label(egui::RichText::new(algorithm_texts(state.algorithm).1).weak().small());
    if state.algorithm == BlendAlgorithmChoice::Transfer {
        draw_base_source(ui, state, catalog);
    }
}

/// The transfer base-font combo over the source rows (recipe index = row index; an unfilled
/// row shows the pick placeholder).
fn draw_base_source(ui: &mut egui::Ui, state: &mut HybridEditorState, catalog: &SourceCatalog) {
    let labels: Vec<String> = state
        .rows
        .iter()
        .map(|row| match row.identity.as_deref() {
            Some(identity) => catalog.item(identity).map_or(identity, |item| item.label()).to_owned(),
            None => t!("typing.hybrid_font.source_pick_placeholder").to_owned(),
        })
        .collect();
    let mut index = state.clamped_base_source();
    let changed = ui
        .horizontal(|ui| {
            ui.label(t!("typing.hybrid_font.base_source_label"));
            WheelComboBox::from_id_salt("typing.hybrid_font.base_source_label")
                .width(ALGORITHM_COMBO_WIDTH_PT)
                .show_index(ui, &mut index, labels.len(), |choice| labels.get(choice).cloned().unwrap_or_default())
                .on_hover_text(t!("typing.hybrid_font.base_source_tooltip"))
                .changed()
        })
        .inner;
    if changed && index != state.base_source {
        state.base_source = index;
        state.touch();
    }
}

/// The «Раздельные веса» toggle (seeds the property weights when it turns on).
fn draw_split_toggle(ui: &mut egui::Ui, state: &mut HybridEditorState) {
    let mut split = state.split_weights;
    ui.checkbox(&mut split, t!("typing.hybrid_font.split_weights_label"))
        .on_hover_text(t!("typing.hybrid_font.split_weights_tooltip"));
    state.set_split_weights(split);
}

/// The status lines of the preview compile that matter for the chosen algorithm: skeleton
/// fallbacks (Skeleton) and base substitutions (Transfer). Transfer measurement gaps are
/// font-wide (a neutral parameter by design), so they are not reported per glyph here.
fn draw_preview_stats(ui: &mut egui::Ui, state: &HybridEditorState) {
    let Some(stats) = state.preview.stats.as_ref() else { return };
    match state.algorithm {
        BlendAlgorithmChoice::Skeleton => {
            if let Some((count, total)) = skeleton_fallback_counts(stats) {
                ui.label(
                    egui::RichText::new(tf!("typing.hybrid_font.skeleton_fallback_status", count = count, total = total)).weak().small(),
                );
            }
        }
        BlendAlgorithmChoice::Transfer => {
            if stats.transfer_base_substituted > 0 {
                ui.label(
                    egui::RichText::new(tf!("typing.hybrid_font.transfer_substituted_status", count = stats.transfer_base_substituted))
                        .weak()
                        .small(),
                );
            }
        }
        BlendAlgorithmChoice::Auto | BlendAlgorithmChoice::Sdf | BlendAlgorithmChoice::NoBlend => {}
    }
}

/// The sample text, the hybrid render and one comparison row per chosen source.
fn draw_preview(ui: &mut egui::Ui, state: &mut HybridEditorState, catalog: &SourceCatalog) {
    ui.label(t!("typing.hybrid_font.preview_text_label"));
    let mut text = state.preview_text.clone();
    let edited = ui
        .add(
            egui::TextEdit::multiline(&mut text)
                .id_salt("typing.hybrid_font.preview_text_label")
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .char_limit(state::PREVIEW_TEXT_MAX_CHARS),
        )
        .changed();
    if edited {
        state.set_preview_text(&text);
    }

    let draft = state.draft_recipe();
    let valid = draft.is_ok();
    if !valid {
        ui.label(egui::RichText::new(t!("typing.hybrid_font.invalid_draft_hint")).weak());
    }
    let tick = TickInput {
        revision: state.revision,
        recipe: draft.ok().map(std::sync::Arc::new),
        rows: state.rows.iter().map(|row| row.identity.clone()).collect(),
        text: &state.preview_text,
        resolver: catalog.resolver(),
        catalog_serial: catalog.serial(),
        width_pt: ui.available_width(),
    };
    let ctx = ui.ctx().clone();
    state.preview.tick(&ctx, tick);

    let tint = ui.visuals().text_color();
    let pending = state.preview.pending();
    if valid {
        draw_slot(ui, &state.preview.hybrid, tint, pending);
        draw_preview_stats(ui, state);
    }
    for (row, slot) in state.rows.iter().zip(&state.preview.sources) {
        let Some(identity) = row.identity.as_deref() else { continue };
        let label = catalog.item(identity).map_or(identity, |item| item.label());
        ui.label(egui::RichText::new(label).weak().small());
        draw_slot(ui, slot, tint, pending);
    }
}

/// One preview image area: the texture (tinted glyphs, shown at its pixel size), or a status
/// line while it is being rendered / when it failed.
fn draw_slot(ui: &mut egui::Ui, slot: &PreviewSlot, tint: egui::Color32, pending: bool) {
    match slot {
        PreviewSlot::Ready { texture, .. } => {
            let pixels_per_point = ui.ctx().pixels_per_point();
            let size = texture.size_vec2() / pixels_per_point;
            // Shrink (never grow) a render wider than the panel, keeping its aspect.
            let available = ui.available_width().max(1.0);
            let size = if size.x > available { size * (available / size.x) } else { size };
            ui.add(egui::Image::new(egui::load::SizedTexture::from_handle(texture)).fit_to_exact_size(size).tint(tint));
        }
        PreviewSlot::Failed if !pending => {
            ui.colored_label(ms_theme::status::ERROR, t!("typing.hybrid_font.preview_failed_status"));
        }
        PreviewSlot::None | PreviewSlot::Failed => {
            if pending {
                ui.label(egui::RichText::new(t!("typing.hybrid_font.preview_pending_status")).weak());
            }
        }
    }
}

/// «Применить» (when the editor owns the button), the build progress + «Отменить», and the
/// cancelled / failed status line.
fn draw_commit(ui: &mut egui::Ui, state: &mut HybridEditorState, cx: &HybridEditorCx<'_>) {
    if state.is_committing() {
        ui.horizontal(|ui| {
            match state.commit_progress() {
                Some((done, total)) if total > 0 => {
                    let fraction = f32::from(u16::try_from(done.min(total) * 1000 / total).unwrap_or(1000)) / 1000.0;
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_width(200.0)
                            .text(tf!("typing.hybrid_font.compile_progress_status", done = done, total = total)),
                    );
                }
                Some(_) | None => {
                    ui.spinner();
                }
            }
            if ui.button(t!("typing.hybrid_font.compile_cancel_button")).clicked() {
                state.cancel_commit();
            }
        });
        return;
    }
    if cx.commit == CommitButton::Apply {
        let enabled = state.is_dirty() && state.draft_recipe().is_ok();
        if ui.add_enabled(enabled, egui::Button::new(t!("typing.hybrid_font.apply_button"))).clicked()
            && let Err(err) = state.start_commit(cx.catalog)
        {
            // The button is enabled only for a valid draft; a refusal here is a race with an
            // edit in the same frame and simply leaves nothing started.
            ms_log::runtime_log::log_warn(format!("typing: hybrid font apply refused: {err}"));
        }
    }
    if state.commit_cancelled() {
        ui.label(egui::RichText::new(t!("typing.hybrid_font.compile_cancelled_status")).weak());
    } else if state.commit_failed() {
        ui.colored_label(ms_theme::status::ERROR, t!("typing.hybrid_font.compile_failed_error"));
    }
}
