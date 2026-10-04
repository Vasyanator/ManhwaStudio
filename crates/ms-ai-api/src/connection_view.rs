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
- draw_key_block()   : the key status / refresh / password / save-delete block alone
                       (`KeyBlockView` in, `KeyBlockActions` out); `draw_connection` draws it in
                       place, and other key owners (image-edit providers) reuse it.
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

    let key_actions = draw_key_block(ui, max_width, KeyBlockView { key_configured: state.key_configured, key_required: state.service.requires_key() }, &mut state.key_edit);
    actions.refresh |= key_actions.refresh;
    actions.save_key |= key_actions.save_key;
    actions.clear_key |= key_actions.clear_key;
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

/// What the key block shows: the verified key state and whether the slot needs a key at all.
#[derive(Debug, Clone, Copy)]
pub struct KeyBlockView {
    /// Whether a key is stored for the slot; `None` until a refresh for that slot answered.
    pub key_configured: Option<bool>,
    /// Whether the slot needs a key (`false`: an absent key reads as "optional", not "missing").
    pub key_required: bool,
}

/// User requests collected from one `draw_key_block` call.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyBlockActions {
    /// "Refresh" was clicked: re-verify the stored key (and whatever else the owner reloads).
    pub refresh: bool,
    /// "Save key" was clicked: store the trimmed `key_edit` buffer.
    pub save_key: bool,
    /// "Delete key" was clicked: delete the stored key.
    pub clear_key: bool,
}

/// Draws the API-key block into `ui` and returns what the user requested: the key status line
/// with "Refresh", the "API key" label with a coloured presence line (only once `view` holds a
/// verified state), the password field over `key_edit` (`max_width` wide) and the save/delete
/// buttons. It uses `ui`'s auto ids (no `push_id`), so drawn inside `draw_connection` it keeps
/// that widget's id sequence; a caller drawing several blocks in one `Ui` wraps each in its own
/// `push_id`. Never blocks and never touches the credential store: the owner turns the actions
/// into requests and keeps `key_edit` out of every log and settings file.
#[must_use]
pub fn draw_key_block(ui: &mut egui::Ui, max_width: f32, view: KeyBlockView, key_edit: &mut String) -> KeyBlockActions {
    let mut actions = KeyBlockActions::default();
    ui.horizontal_wrapped(|ui| {
        let key_state = match view.key_configured {
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
        match view.key_configured {
            Some(true) => {
                ui.colored_label(ms_theme::status::SUCCESS, t!("ai_api.connection.key_present_status"));
            }
            Some(false) if view.key_required => {
                ui.colored_label(ms_theme::status::ERROR, t!("ai_api.connection.key_missing_status"));
            }
            Some(false) => {
                ui.weak(t!("ai_api.connection.key_optional_status"));
            }
            None => {}
        }
    });
    ui.add(egui::TextEdit::singleline(key_edit).password(true).desired_width(max_width));
    ui.horizontal_wrapped(|ui| {
        if ui.small_button(t!("ai_api.connection.save_key_button")).clicked() {
            actions.save_key = true;
        }
        if ui.small_button(t!("ai_api.connection.delete_key_button")).clicked() {
            actions.clear_key = true;
        }
    });
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
    use super::{KeyBlockActions, KeyBlockView, compact_middle, draw_connection, draw_key_block};
    use crate::connection::{AiApiConnectionActions, AiApiConnectionState};
    use crate::service::AiApiService;

    /// One drawn widget of the headless harness: its rect, type and label.
    type WidgetRow = (egui::Rect, Option<egui::WidgetType>, Option<String>);

    /// `(save_key, clear_key, refresh)` of one frame's requests.
    type KeyFlags = (bool, bool, bool);

    fn connection_flags(actions: AiApiConnectionActions) -> KeyFlags {
        (actions.save_key, actions.clear_key, actions.refresh)
    }

    fn block_flags(actions: KeyBlockActions) -> KeyFlags {
        (actions.save_key, actions.clear_key, actions.refresh)
    }

    /// A headless context that records `WidgetInfo` (debug builds only), so a test can find a
    /// widget by its label.
    fn headless() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.debug.show_interactive_widgets = true);
        ctx
    }

    /// Runs one frame of `draw` at `time` with `events`; returns its flags and every widget of
    /// the frame in paint order.
    fn frame(ctx: &egui::Context, time: f64, events: Vec<egui::Event>, mut draw: impl FnMut(&mut egui::Ui) -> KeyFlags) -> (KeyFlags, Vec<WidgetRow>) {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 1400.0))), time: Some(time), events, ..Default::default() };
        let mut result = None;
        let output = ctx.run_ui(input, |ui| {
            let flags = draw(ui);
            let widgets = ui.ctx().viewport(|viewport| {
                let rects = &viewport.this_pass.widgets;
                rects.get_layer(egui::LayerId::background()).map(|widget| (widget.rect, rects.info(widget.id).map(|info| info.typ), rects.info(widget.id).and_then(|info| info.label.clone()))).collect::<Vec<_>>()
            });
            result = Some((flags, widgets));
        });
        output.drop_without_applying_deltas();
        result.unwrap_or_else(|| panic!("run_ui did not call the frame closure"))
    }

    /// Clicks the centre of `rect` (press frame, release frame, then a frame carrying `extra`
    /// events) and returns the flags of those three frames OR-ed together.
    fn click(ctx: &egui::Context, rect: egui::Rect, extra: Vec<egui::Event>, mut draw: impl FnMut(&mut egui::Ui) -> KeyFlags) -> KeyFlags {
        let pos = rect.center();
        let press = vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }];
        let release = vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }];
        let (a, _) = frame(ctx, 1.0, press, &mut draw);
        let (b, _) = frame(ctx, 1.05, release, &mut draw);
        let (c, _) = frame(ctx, 1.1, extra, &mut draw);
        (a.0 | b.0 | c.0, a.1 | b.1 | c.1, a.2 | b.2 | c.2)
    }

    /// The rect of the widget labelled `label`.
    fn rect_of(widgets: &[WidgetRow], label: &str) -> egui::Rect {
        widgets.iter().find(|row| row.2.as_deref() == Some(label)).map_or_else(|| panic!("no widget labelled {label:?}"), |row| row.0)
    }

    fn has_label(widgets: &[WidgetRow], label: &str) -> bool {
        widgets.iter().any(|row| row.2.as_deref() == Some(label))
    }

    // The chat widget keeps its key-block behaviour: each button sets exactly its own flag
    // (the warm-up frame, which carries the automatic first refresh, is excluded).
    #[test]
    fn connection_key_buttons_set_their_own_flag() {
        let buttons = [(t!("ai_api.connection.refresh_button"), (false, false, true)), (t!("ai_api.connection.save_key_button"), (true, false, false)), (t!("ai_api.connection.delete_key_button"), (false, true, false))];
        for service in [AiApiService::OpenAi, AiApiService::OpenAiCompatible] {
            for (label, expected) in buttons {
                let ctx = headless();
                let mut state = AiApiConnectionState::new("sys");
                state.service = service;
                state.base_url = "http://127.0.0.1:8080".to_string();
                let mut draw = |ui: &mut egui::Ui| connection_flags(draw_connection(ui, "test_ai_api", 240.0, &mut state));
                let (warm_up, widgets) = frame(&ctx, 0.0, Vec::new(), &mut draw);
                assert_eq!(warm_up, (false, false, true), "{service:?}: first draw requests the initial refresh");
                assert_eq!(click(&ctx, rect_of(&widgets, label), Vec::new(), &mut draw), expected, "{service:?} {label}");
            }
        }
    }

    #[test]
    fn key_block_buttons_set_their_own_flag_and_field_edits_the_buffer() {
        let view = KeyBlockView { key_configured: None, key_required: true };
        let buttons = [(t!("ai_api.connection.refresh_button"), (false, false, true)), (t!("ai_api.connection.save_key_button"), (true, false, false)), (t!("ai_api.connection.delete_key_button"), (false, true, false))];
        for (label, expected) in buttons {
            let ctx = headless();
            let mut key_edit = String::new();
            let mut draw = |ui: &mut egui::Ui| block_flags(draw_key_block(ui, 240.0, view, &mut key_edit));
            let (idle, widgets) = frame(&ctx, 0.0, Vec::new(), &mut draw);
            assert_eq!(idle, (false, false, false));
            assert_eq!(click(&ctx, rect_of(&widgets, label), Vec::new(), &mut draw), expected, "{label}");
        }
        let ctx = headless();
        let mut key_edit = String::new();
        let mut draw = |ui: &mut egui::Ui| block_flags(draw_key_block(ui, 240.0, view, &mut key_edit));
        let (_, widgets) = frame(&ctx, 0.0, Vec::new(), &mut draw);
        let field = widgets.iter().find(|row| row.1 == Some(egui::WidgetType::TextEdit)).map_or_else(|| panic!("no key field"), |row| row.0);
        assert_eq!(click(&ctx, field, vec![egui::Event::Text("sk-test".to_string())], &mut draw), (false, false, false));
        assert_eq!(key_edit, "sk-test");
    }

    // The presence line appears only for a verified state; a missing key is an error only when
    // the slot requires one.
    #[test]
    fn key_block_presence_line_follows_the_view() {
        let present = t!("ai_api.connection.key_present_status");
        let missing = t!("ai_api.connection.key_missing_status");
        let optional = t!("ai_api.connection.key_optional_status");
        let cases = [
            (Some(true), true, Some(present)),
            (Some(false), true, Some(missing)),
            (Some(false), false, Some(optional)),
            (None, true, None),
            (None, false, None),
        ];
        for (key_configured, key_required, shown) in cases {
            let ctx = headless();
            let mut key_edit = String::new();
            let (_, widgets) = frame(&ctx, 0.0, Vec::new(), |ui| block_flags(draw_key_block(ui, 240.0, KeyBlockView { key_configured, key_required }, &mut key_edit)));
            for line in [present, missing, optional] {
                assert_eq!(has_label(&widgets, line), shown == Some(line), "{key_configured:?} {key_required}: {line}");
            }
        }
    }

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
