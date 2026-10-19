/*
File: crates/ms-ai-api/src/neurolate/view.rs

Purpose:
The egui view of the NeuroLate key panel: the shared API-key block (`draw_key_block`, disabled
while a key operation runs), the key status line and one limit line ("Remaining X of Y per
period · status", the renewal date, "Check limit").

Key functions:
- draw_neurolate_key_panel(): draws the panel, returns the user's `NeurolateKeyActions`.

Notes:
Draws straight into the caller's `Ui` (no `ScrollArea`, no width clamp). The key block runs in
its own id scope `"{id_salt}_key"` because it uses auto ids; the caller's `id_salt` must be
unique among the panels in one parent `Ui`. Never blocks and never touches the credential store
or the network: buttons only set flags the owner hands to `NeurolateKeyPanel::pump`. Colours
come from `ms_theme::status`; every text from `ms-i18n`.
*/

use super::account::{NeurolateKeyActions, NeurolateKeyPanel};
use super::protocol::{KeyInfo, KeyStatus, LimitPeriod};
use crate::connection_view::draw_key_block;
use crate::image_edit::ImageEditKeyStore;

/// Draws the `NeuroLate` key panel for `panel` into `ui` and returns what the user requested.
/// `id_salt` scopes the key block's ids (`"{id_salt}_key"`) and must be unique among the panels
/// in one parent `Ui`; `max_width` is the password field's width. Never blocks.
#[must_use]
pub fn draw_neurolate_key_panel<S: ImageEditKeyStore>(ui: &mut egui::Ui, id_salt: &str, max_width: f32, panel: &mut NeurolateKeyPanel<S>) -> NeurolateKeyActions {
    let mut actions = NeurolateKeyActions::default();
    let busy = panel.key_state().is_busy();
    let view = panel.key_state().block_view(true);
    actions.key = ui
        .push_id(format!("{id_salt}_key"), |ui| {
            if busy {
                ui.disable();
            }
            draw_key_block(ui, max_width, view, &mut panel.key_state_mut().key_edit)
        })
        .inner;
    let status = panel.key_state().status();
    if !status.trim().is_empty() {
        ui.small(status.to_string());
    }

    match panel.limit() {
        Some(Ok(info)) => draw_limit(ui, info),
        Some(Err(error)) => {
            ui.colored_label(ms_theme::status::WARNING, egui::RichText::new(tf!("ai_api.neurolate.limit_failed_status", err = error)).small());
        }
        None => {}
    }
    // No limit to check without a stored key; the key block already says it is missing.
    if panel.key_configured() != Some(false) {
        ui.horizontal_wrapped(|ui| {
            if panel.is_checking_limit() {
                ui.spinner();
                ui.small(t!("ai_api.neurolate.checking_limit_status"));
            } else if ui.small_button(t!("ai_api.neurolate.check_limit_button")).clicked() {
                actions.check_limit = true;
            }
        });
    }
    actions
}

/// The limit line, in the warning colour when the key cannot pay (not active, nothing left),
/// and the renewal date when the limit renews.
fn draw_limit(ui: &mut egui::Ui, info: &KeyInfo) {
    let text = tf!(
        "ai_api.neurolate.limit_line_label",
        remaining = format!("{:.2}", info.remaining_value),
        limit = format!("{:.2}", info.limit_value),
        period = period_label(&info.limit_period),
        status = status_label(&info.status)
    );
    let exhausted = info.status != KeyStatus::Active || info.remaining_value <= 0.0;
    if exhausted {
        ui.colored_label(ms_theme::status::WARNING, egui::RichText::new(text).small());
    } else {
        ui.small(text);
    }
    if let Some(date) = &info.resets_on {
        ui.small(tf!("ai_api.neurolate.limit_resets_label", date = date));
    }
}

/// The localized period; an unknown one shows `NeuroLate`'s own code.
fn period_label(period: &LimitPeriod) -> String {
    match period {
        LimitPeriod::Total => t!("ai_api.neurolate.period_total_label").to_string(),
        LimitPeriod::Day => t!("ai_api.neurolate.period_day_label").to_string(),
        LimitPeriod::Week => t!("ai_api.neurolate.period_week_label").to_string(),
        LimitPeriod::Unknown(code) => code.clone(),
    }
}

/// The localized key status; an unknown one shows `NeuroLate`'s own code.
fn status_label(status: &KeyStatus) -> String {
    match status {
        KeyStatus::Active => t!("ai_api.neurolate.status_active_label").to_string(),
        KeyStatus::Disabled => t!("ai_api.neurolate.status_disabled_label").to_string(),
        KeyStatus::Expired => t!("ai_api.neurolate.status_expired_label").to_string(),
        KeyStatus::Exhausted => t!("ai_api.neurolate.status_exhausted_label").to_string(),
        KeyStatus::Unknown(code) => code.clone(),
    }
}
