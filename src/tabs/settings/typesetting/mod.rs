/*
FILE OVERVIEW: src/tabs/settings/typesetting/mod.rs
"Тайп" settings pane orchestrator: typesetting-related options that affect text
rendering and form iteration across the app.

Main responsibilities:
- Render the "Поворот Ctrl+колесо" chooser and persist `TextTab.rotation_ctrl_wheel_mode`.
- Render the shared typesetting-language selector.
- Render and persist the app-wide hanging-punctuation list, applied live via
  `crate::text_punctuation`.
- Host two collapsed self-contained blocks: the per-effect-kind default parameters editor
  (`EffectDefaultsEditorState`, a typing-panel widget) and the font-settings block
  (`FontSettingsEditorState`, owned by this submodule's `font_settings`).
- Serve the `TypesettingFontGroups` deep-link reveal: the pane body is wrapped in a
  `ScrollArea`; on the reveal frame the font-settings + nested groups headers are force-opened,
  scrolled into view, and the groups block is highlighted for ~2 s (`paint_reveal_highlight`).
  See this module's `MODULE_README.md`.

Submodules:
- `font_settings`: the "Настройки шрифтов" widget (categories + system-font import picker).
- `font_properties_window`: the per-font properties window opened from the font rows.
- `font_groups`: the "Группы" section (virtual font groups) + its group-editor window.

Key types:
- `SettingsTabState` (methods only; the type lives in the parent `settings` module)

Notes:
- Persistence is delegated to background threads to avoid blocking the GUI thread;
  the live set is updated synchronously so new renders pick up the change immediately.
- The font MODEL stays in `crate::tabs::typing`; this submodule reaches it ONLY through
  the `crate::tabs::typing::font_admin` facade (UI here, model there).
*/

mod font_groups;
mod font_properties_window;
mod font_settings;

pub(super) use font_settings::FontSettingsEditorState;

use super::{SettingsTabState, save_hanging_punctuation, save_rotation_ctrl_wheel_mode};
use crate::runtime_log;
use crate::settings_shared::SettingsDeepLink;
use crate::tabs::typing::rotation_ctrl_wheel::{
    RotationCtrlWheelMode, rotation_ctrl_wheel_mode, set_rotation_ctrl_wheel_mode,
};
use crate::text_punctuation;
use ms_thread as thread;
use web_time::{Duration, Instant};

/// Total lifetime of the deep-link reveal highlight around the groups block.
const REVEAL_HIGHLIGHT_TOTAL: Duration = Duration::from_millis(2000);
/// Duration of the final fade-out of the reveal highlight (part of the total).
const REVEAL_HIGHLIGHT_FADE_SECS: f32 = 0.6;
/// How long a pending deep-link reveal may wait for the async font-category load
/// to produce the groups block before it is abandoned (so a failed load can never
/// leave the headers force-opened indefinitely).
const REVEAL_PENDING_TIMEOUT: Duration = Duration::from_secs(5);

impl SettingsTabState {
    /// Renders the "Тайп" settings pane.
    ///
    /// Currently exposes the app-wide hanging-punctuation editor: saving applies
    /// the value to the live `text_punctuation` set and persists it to
    /// `user_config.json` (`TextTab.hanging_punctuation`) on a background thread.
    pub(super) fn draw_typesetting(&mut self, ui: &mut egui::Ui) {
        // Deep-link reveal: force-open the font headers while the request is pending. The
        // request is consumed only once the groups block actually DRAWS (first visit loads
        // the font categories asynchronously, so the nested "Группы" header may not exist
        // for a few frames — consuming the flag earlier would silently lose the reveal).
        // Once consumed, the force-open stops, so the user can freely collapse the blocks.
        let force_reveal = matches!(
            self.pending_reveal,
            Some(SettingsDeepLink::TypesettingFontGroups)
        );

        // Wrap the whole pane in a scroll area so `scroll_to_me` targets (the reveal scroll)
        // have an ancestor ScrollArea to consume them; the pane had none before, so long
        // content was simply cut off. `auto_shrink` off so it fills the section.
        let scroll_output = egui::ScrollArea::vertical()
            .id_salt("settings.typesetting.scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| self.draw_typesetting_body(ui, force_reveal));
        let groups_rect = scroll_output.inner;

        if force_reveal {
            if groups_rect.is_some() {
                // The groups block rendered: the reveal (force-open + scroll_to_me) landed
                // this frame — consume the request and start the highlight window.
                self.pending_reveal = None;
                self.pending_reveal_expires = None;
                self.reveal_highlight_until = Some(Instant::now() + REVEAL_HIGHLIGHT_TOTAL);
            } else {
                // Still waiting for the async font-category load; give up after a bounded
                // window so a failed load cannot pin the headers open forever.
                let expires = *self
                    .pending_reveal_expires
                    .get_or_insert_with(|| Instant::now() + REVEAL_PENDING_TIMEOUT);
                if Instant::now() >= expires {
                    self.pending_reveal = None;
                    self.pending_reveal_expires = None;
                }
            }
            // Keep frames coming while waiting/animating so the load result and the scroll
            // are picked up promptly even with no user input.
            ui.ctx().request_repaint();
        }

        // Reveal highlight: paint a fading rounded outline around the groups block until the
        // deadline, on a top layer (no hitbox), clipped to the scroll viewport so it never
        // paints over the section tab bar or neighboring UI while scrolling. Request
        // repaints so the fade/expiry advances.
        if let Some(deadline) = self.reveal_highlight_until {
            let now = Instant::now();
            if now >= deadline {
                self.reveal_highlight_until = None;
            } else {
                if let Some(rect) = groups_rect {
                    paint_reveal_highlight(ui, rect, scroll_output.inner_rect, now, deadline);
                }
                ui.ctx().request_repaint();
            }
        }
    }

    /// Draws the "Тайп" pane body inside the scroll area. Returns the font-groups block
    /// rect (header+body union) when it was drawn, for the reveal highlight. When
    /// `force_reveal_groups` is set (the deep-link frame), the font-settings and nested
    /// groups headers are force-opened and scrolled into view.
    fn draw_typesetting_body(&mut self, ui: &mut egui::Ui, force_reveal_groups: bool) -> Option<egui::Rect> {
        ui.heading(t!("settings.typesetting.heading"));

        ui.add_space(8.0);
        self.draw_rotation_ctrl_wheel_setting(ui);

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        // Shared typesetting-language selector (same widget the general-settings panel
        // renders); the id-salt prefix keeps this instance's egui ids distinct.
        crate::general_settings_panel::draw_text_language_setting(
            ui,
            "settings.typesetting.text_language",
        );

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(t!("settings.typesetting.hanging_punctuation_label"));
        ui.small(
            t!("settings.typesetting.hanging_punctuation_hint"),
        );

        let mut should_save_punctuation = false;
        ui.horizontal_wrapped(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.hanging_punctuation_input)
                    .font(egui::FontId::new(16.0, egui::FontFamily::Monospace))
                    .desired_width(520.0)
                    .hint_text(text_punctuation::DEFAULT_HANGING_PUNCTUATION),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                should_save_punctuation = true;
            }
            if ui
                .add_enabled(
                    self.hanging_punctuation_input != self.saved_hanging_punctuation,
                    egui::Button::new(t!("settings.typesetting.hanging_punctuation_save_button")),
                )
                .clicked()
            {
                should_save_punctuation = true;
            }
            if ui
                .add_enabled(
                    self.hanging_punctuation_input != text_punctuation::DEFAULT_HANGING_PUNCTUATION,
                    egui::Button::new(t!("settings.typesetting.hanging_punctuation_reset_button")),
                )
                .clicked()
            {
                self.hanging_punctuation_input =
                    text_punctuation::DEFAULT_HANGING_PUNCTUATION.to_string();
                should_save_punctuation = true;
            }
        });

        if should_save_punctuation {
            let punctuation = self.hanging_punctuation_input.clone();
            self.saved_hanging_punctuation = punctuation.clone();
            // Сразу применяем к живому набору, чтобы новые рендеры учли изменение.
            text_punctuation::set_hanging_punctuation(&punctuation);
            let path = self.user_settings_file.clone();
            if let Err(err) = thread::Builder::new()
                .name("settings-hanging-punctuation-save".to_string())
                .spawn(move || {
                    if let Err(err) = save_hanging_punctuation(&path, &punctuation) {
                        runtime_log::log_error(format!(
                            "[settings] failed to persist hanging punctuation to {}; error={err}",
                            path.display()
                        ));
                    }
                })
            {
                runtime_log::log_error(format!(
                    "[settings] failed to start hanging punctuation save thread; error={err}"
                ));
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        // Per-effect-kind default parameters, collapsed by default. Self-contained
        // typing-panel widget; it owns its own live-apply + background persistence to
        // `TextTab.effect_defaults`.
        egui::CollapsingHeader::new(t!("settings.typesetting.effect_defaults_header")).id_salt("settings.typesetting.effect_defaults_header")
            .default_open(false)
            .show(ui, |ui| {
                self.effect_defaults_editor.ui(ui);
            });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        // Font-settings block, collapsed by default. Self-contained widget; it loads the
        // font category lists off-thread and drives the runtime-global imported-fonts store
        // for system-font import/removal — all through `crate::tabs::typing::font_admin`.
        // On the deep-link reveal frame this header is force-opened; `.open(None)` otherwise
        // leaves its persisted collapsed/expanded state untouched. The nested groups block
        // rect bubbles up from the editor's body closure for the reveal highlight.
        egui::CollapsingHeader::new(t!("settings.typesetting.font_settings_header")).id_salt("settings.typesetting.font_settings_header")
            .open(force_reveal_groups.then_some(true))
            .default_open(false)
            .show(ui, |ui| self.font_settings_editor.ui(ui, force_reveal_groups))
            .body_returned
            .flatten()
    }

    /// Renders the "Поворот Ctrl+колесо" chooser: which rotation mechanism the typing
    /// tab's Ctrl+wheel gesture drives. Applies the choice to the runtime global
    /// immediately (so the typing tab picks it up on the next wheel event) and persists
    /// it to `user_config.json` (`TextTab.rotation_ctrl_wheel_mode`) on a background
    /// thread.
    fn draw_rotation_ctrl_wheel_setting(&self, ui: &mut egui::Ui) {
        ui.label(t!("settings.typesetting.rotation_ctrl_wheel_label"));
        ui.small(t!("settings.typesetting.rotation_ctrl_wheel_hint"));

        let mut mode = rotation_ctrl_wheel_mode();
        let previous = mode;
        ui.horizontal_wrapped(|ui| {
            ui.radio_value(&mut mode, RotationCtrlWheelMode::Vector, t!("settings.typesetting.rotation_mode_vector"))
                .on_hover_text(
                    t!("settings.typesetting.rotation_mode_vector_hint"),
                );
            ui.radio_value(&mut mode, RotationCtrlWheelMode::Raster, t!("settings.typesetting.rotation_mode_raster"))
                .on_hover_text(t!("settings.typesetting.rotation_mode_raster_hint"));
        });

        if mode != previous {
            // Apply live first so the change takes effect even if the disk write fails.
            set_rotation_ctrl_wheel_mode(mode);
            let path = self.user_settings_file.clone();
            if let Err(err) = thread::Builder::new()
                .name("settings-rotation-ctrl-wheel-save".to_string())
                .spawn(move || {
                    if let Err(err) = save_rotation_ctrl_wheel_mode(&path, mode) {
                        runtime_log::log_error(format!(
                            "[settings] failed to persist rotation ctrl-wheel mode to {}; \
                             error={err}",
                            path.display()
                        ));
                    }
                })
            {
                runtime_log::log_error(format!(
                    "[settings] failed to start rotation ctrl-wheel mode save thread; error={err}"
                ));
            }
        }
    }
}

/// Paints the deep-link reveal highlight: a rounded accent-colored outline around `rect`
/// on a top (Tooltip-order) layer, so it sits over the pane content and registers NO
/// hitbox (pure painter, per the overlay contract). The alpha fades to zero across the
/// final [`REVEAL_HIGHLIGHT_FADE_SECS`] before `deadline`.
///
/// `rect` is the groups block's absolute (screen-space) rect; `clip_rect` is the scroll
/// viewport the outline is clipped to (the block may extend past it mid-scroll, and the
/// top layer would otherwise paint over unrelated UI); `now`/`deadline` bound the
/// remaining highlight lifetime.
fn paint_reveal_highlight(
    ui: &egui::Ui,
    rect: egui::Rect,
    clip_rect: egui::Rect,
    now: Instant,
    deadline: Instant,
) {
    // Fade factor 1.0 while far from the deadline, ramping to 0.0 over the final fade window.
    let remaining = deadline.saturating_duration_since(now).as_secs_f32();
    let fade = (remaining / REVEAL_HIGHLIGHT_FADE_SECS).clamp(0.0, 1.0);
    let accent = ui.visuals().selection.bg_fill.gamma_multiply(fade);
    let painter = ui
        .ctx()
        .layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("settings.typesetting.font_groups_reveal_highlight"),
        ))
        // The outline expands slightly past the block; clip to the scroll viewport so a
        // partially-scrolled-out block never draws over the tab bar or neighboring panes.
        .with_clip_rect(clip_rect.expand(4.0));
    // Expand slightly so the outline sits just outside the block instead of clipping it.
    let outline = rect.expand(3.0);
    painter.rect_stroke(
        outline,
        egui::CornerRadius::same(6),
        egui::Stroke::new(2.0, accent),
        egui::StrokeKind::Outside,
    );
}
