/*
File: crates/ms-ai-api/src/connection_view.rs

Purpose:
The shared AI API connection widget: service picker, base URL field (compatible services only),
key status + refresh, password field (labelled with a ✓/✗ key-presence line once the key state is
known; a neutral "optional" line for a keyless compatible service) with save/delete, request
status, model picker + manual model id + the model's image-input status, account status, system
instruction. Used by the translation tab's OCR and machine-translation panels.

Key functions:
- draw_connection()  : draws the block, returns the user's `AiApiConnectionActions`.
- compact_middle()   : private, middle-ellipsizes the selected model id for the combo button.
- draw_image_input_status(): private, the "Images: ..." line under the model field.

Notes:
The widget draws straight into the caller's `Ui` (no `push_id`, no `vertical`, no
`ScrollArea`, no width clamp), so the caller decides scrolling and width and the
`WheelComboBox` ids stay `ui.id()`-scoped `"{id_salt}_service"` / `"{id_salt}_model"` strings
(persisted widget-state keys; `String` and `&str` salts hash identically); the base URL field is
salted `"{id_salt}_base_url"`. The first draw of a
state, every service change, and a committed base URL edit (focus lost or Enter, with a changed
value) request a metadata refresh, so a stored key is verified and the model list loaded without
pressing "Refresh"; typing in the URL field only marks the options changed. It never spawns
work: buttons only set flags that the consumer turns into requests via `AiApiTaskRunner`.
*/

use ms_widgets::WheelComboBox;

use crate::connection::{AiApiConnectionActions, AiApiConnectionState};
use crate::model_caps::{ImageInputSupport, image_input_support};
use crate::service::AiApiService;

/// Longest model id (in chars) shown on the model combo button before middle-ellipsizing; keeps
/// a long provider-prefixed id from widening the side panel.
const MODEL_LABEL_MAX_CHARS: usize = 42;

/// Draws the connection block for `state` into `ui` and returns what the user requested.
///
/// `id_salt` prefixes the two combo ids (`"{id_salt}_service"`, `"{id_salt}_model"`) and the base
/// URL field's id (`"{id_salt}_base_url"`); it must be
/// unique among the widget instances in one parent `Ui`. `max_width` is the desired width of
/// the text fields. A service change resets the per-service state (`reset_for_service_change`)
/// and sets both `options_changed` and `refresh`; the first call for a given state also sets
/// `refresh` (`take_initial_refresh`); a committed, changed base URL sets `refresh`. Never blocks.
#[must_use]
pub fn draw_connection(ui: &mut egui::Ui, id_salt: &str, max_width: f32, state: &mut AiApiConnectionState) -> AiApiConnectionActions {
    let mut actions = AiApiConnectionActions::default();

    let old_service = state.service;
    ui.label(t!("ai_api.connection.service_label"));
    WheelComboBox::from_id_salt(format!("{id_salt}_service"))
        .selected_text(state.service.label())
        .show_ui(ui, |ui| {
            for service in AiApiService::ALL {
                actions.options_changed |= ui.selectable_value(&mut state.service, service, service.label()).changed();
            }
        });
    if old_service != state.service {
        state.reset_for_service_change();
        actions.options_changed = true;
        actions.refresh = true;
    }
    if state.service.uses_base_url() {
        ui.label(t!("ai_api.connection.base_url_label"));
        // A stable salt: the field appears and disappears with the service, which would otherwise
        // shift the auto ids of the key and model fields below it.
        let response = ui.add(egui::TextEdit::singleline(&mut state.base_url).id_salt(format!("{id_salt}_base_url")).desired_width(max_width).hint_text(t!("ai_api.connection.base_url_hint")));
        actions.options_changed |= response.changed();
        // A singleline `TextEdit` surrenders focus on Enter, so `lost_focus` covers both "Enter"
        // and "clicked elsewhere"; the refresh waits for that instead of firing per keystroke.
        if response.lost_focus() && state.commit_base_url_edit() {
            actions.refresh = true;
        }
    }
    // First show of this state: verify the stored key and load the models automatically (the
    // service change above already refreshes, so this only matters for the persisted service).
    if state.take_initial_refresh() {
        actions.refresh = true;
    }

    ui.horizontal_wrapped(|ui| {
        let key_state = match state.key_configured {
            Some(true) => t!("ai_api.connection.key_saved_status"),
            Some(false) => t!("ai_api.connection.key_not_set_status"),
            None => t!("ai_api.connection.key_unverified_status"),
        };
        ui.small(key_state);
        if ui.small_button(t!("ai_api.connection.refresh_button")).clicked() {
            actions.refresh = true;
        }
    });

    ui.horizontal_wrapped(|ui| {
        ui.label(t!("ai_api.connection.api_key_label"));
        // Only a verified state is shown: `None` (not refreshed yet) has no reliable answer.
        match state.key_configured {
            Some(true) => {
                ui.colored_label(ms_theme::status::SUCCESS, t!("ai_api.connection.key_present_status"));
            }
            Some(false) if state.service.requires_key() => {
                ui.colored_label(ms_theme::status::ERROR, t!("ai_api.connection.key_missing_status"));
            }
            Some(false) => {
                ui.weak(t!("ai_api.connection.key_optional_status"));
            }
            None => {}
        }
    });
    ui.add(egui::TextEdit::singleline(&mut state.key_edit).password(true).desired_width(max_width));
    ui.horizontal_wrapped(|ui| {
        if ui.small_button(t!("ai_api.connection.save_key_button")).clicked() {
            actions.save_key = true;
        }
        if ui.small_button(t!("ai_api.connection.delete_key_button")).clicked() {
            actions.clear_key = true;
        }
    });
    if !state.status.trim().is_empty() {
        ui.small(state.status.clone());
    }

    ui.label(t!("ai_api.connection.model_label"));
    WheelComboBox::from_id_salt(format!("{id_salt}_model"))
        .selected_text(compact_middle(&state.model, MODEL_LABEL_MAX_CHARS))
        .show_ui(ui, |ui| {
            // Built only while the popup is open: a fetched list can be long, so it is not
            // cloned on every frame the panel is visible.
            for model in state.model_choices() {
                actions.options_changed |= ui.selectable_value(&mut state.model, model.clone(), model).changed();
            }
        });
    actions.options_changed |= ui
        .add(egui::TextEdit::singleline(&mut state.model).desired_width(max_width).hint_text(t!("ai_api.connection.model_id_hint")))
        .changed();
    draw_image_input_status(ui, &state.model);

    ui.label(t!("ai_api.connection.balance_limits_label"));
    ui.small(state.account_status.clone());

    ui.label(t!("ai_api.connection.system_instruction_label"));
    actions.options_changed |= ui
        .add(egui::TextEdit::multiline(&mut state.system_instruction).desired_width(max_width).desired_rows(4))
        .changed();

    actions
}

/// Draws whether `model` accepts images (`image_input_support`), coloured by certainty; nothing
/// for a blank model id.
fn draw_image_input_status(ui: &mut egui::Ui, model: &str) {
    if model.trim().is_empty() {
        return;
    }
    let (color, text) = match image_input_support(model) {
        ImageInputSupport::Supported => (ms_theme::status::SUCCESS, t!("ai_api.connection.images_supported_status")),
        ImageInputSupport::NotSupported => (ms_theme::status::ERROR, t!("ai_api.connection.images_not_supported_status")),
        ImageInputSupport::Unknown => (ms_theme::status::WARNING, t!("ai_api.connection.images_unknown_status")),
    };
    ui.colored_label(color, text);
}

/// Shortens `text` to at most `max_chars` chars as `"<head>...<tail>"` (equal halves). Text
/// that already fits, or a `max_chars` below 8 (too small for a useful head and tail), is
/// returned unchanged.
fn compact_middle(text: &str, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars || max_chars < 8 {
        return text.to_string();
    }
    // `max_chars >= 8` here, so the subtraction cannot underflow; 3 chars go to the "...".
    let keep = (max_chars - 3) / 2;
    let start = chars.iter().take(keep).collect::<String>();
    let end = chars.iter().skip(chars.len().saturating_sub(keep)).collect::<String>();
    format!("{start}...{end}")
}

#[cfg(test)]
mod tests {
    use super::compact_middle;

    #[test]
    fn compact_middle_never_exceeds_max() {
        let long = "open_router::provider/some-extremely-long-model-name-with-a-suffix-0123456789";
        for max in 8..=60 {
            let shortened = compact_middle(long, max);
            assert!(shortened.chars().count() <= max, "max {max}: {shortened}");
            assert!(shortened.starts_with("op") && shortened.contains("..."), "{shortened}");
            assert!(shortened.ends_with('9'), "{shortened}");
        }
    }

    #[test]
    fn compact_middle_keeps_short_text_and_tiny_limits() {
        assert_eq!(compact_middle("gpt-4o", 42), "gpt-4o");
        let long = "a".repeat(50);
        assert_eq!(compact_middle(&long, 7), long);
        assert_eq!(compact_middle(&"é".repeat(42), 42), "é".repeat(42));
    }

    // The combo ids are persisted widget-state keys: a `format!`-built `String` salt must hash to
    // the same `Id` as the literal the panels used before the widget was shared.
    #[test]
    fn formatted_salt_matches_literal_salt() {
        let prefix = "translation_ocr_ai_api";
        assert_eq!(egui::Id::new(format!("{prefix}_service")), egui::Id::new("translation_ocr_ai_api_service"));
        assert_eq!(egui::Id::new(format!("{prefix}_model")), egui::Id::new("translation_ocr_ai_api_model"));
    }
}
