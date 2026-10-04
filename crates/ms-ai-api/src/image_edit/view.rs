/*
File: crates/ms-ai-api/src/image_edit/view.rs

Purpose:
The image-edit service picker widget: provider picker (with the Russia availability badge in
the Russian UI), region selector or server-address field, the provider's API-key block, model
picker (the catalogue's offers plus a free model id), a link to the provider's API
documentation and the selected offer's notes (how well
its output size is evidenced, mask support, announced shutdown date). Consumer: the cleaning
tab's API editing tool.

Key structures:
- ImageEditSelection    : the picked provider plus each provider's own model id and endpoint
                          (region id or base URL), so switching provider restores its choice.
- ProviderChoice        : one provider's model id and endpoint.
- ImageEditPickerActions: what the user changed or requested in one frame.
- RussiaBadge           : colour, text and reason of one provider's availability badge.

Key functions:
- draw_image_edit_picker(): the whole widget; returns `ImageEditPickerActions`.
- russia_badge() (pure) / active_russia_badge() / draw_russia_badge().
- ImageEditSelection::fill_defaults(): fills an EMPTY model id / region, never replaces one.

Notes:
The widget never does I/O and never spawns work: key buttons come back as
`KeyBlockActions` for `ImageEditKeyRunner::pump`, and the consumer re-resolves the key slot
when `key_slot_changed` is set. Every widget id is derived from the caller's `id_salt` prefix
(`"{id_salt}_provider"`, `_region`, `_base_url`, `_key`, `_model`, `_model_id`), never from a
localized text. The Russia badge exists only when the UI language is Russian and only for
providers with a known status (none for the user's own server).
*/

use std::collections::HashMap;

use egui::Color32;
use ms_widgets::{RowLayout, SearchableComboBox, SearchableComboItem, WheelComboBox};

use super::catalog::{MaskSupport, ModelOffer, lookup, offers};
use super::error::ImageEditError;
use super::key_state::{ImageEditKeyState, ImageEditKeyStore};
use super::provider::{EndpointKind, EndpointRegion, ImageEditProvider, ProviderKeySlot, RussiaAccess, RussiaStatus};
use super::request::EndpointChoice;
use super::size_rule::SizeEvidence;
use crate::connection_view::{KeyBlockActions, draw_key_block};

/// The UI language whose users see the Russia availability badge.
const RUSSIAN_LANGUAGE: &str = "ru";

/// One provider's choice: its model id and endpoint, kept per provider by `ImageEditSelection`
/// (persisted by the consumer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderChoice {
    /// The model id as the provider's API expects it (catalogue id or typed by hand).
    pub model_id: String,
    /// The frozen region id for a provider with a region list, the typed base URL for the
    /// user's own server, unused (empty) for a fixed endpoint.
    pub endpoint: String,
}

/// The picker's selection: the current provider plus every provider's own choice. Persisted by
/// the consumer (provider by `ImageEditProvider::key()`, the choices per provider key).
#[derive(Debug, Clone)]
pub struct ImageEditSelection {
    /// The selected provider.
    pub provider: ImageEditProvider,
    /// Each visited provider's choice; a missing entry means "nothing chosen yet".
    pub choices: HashMap<ImageEditProvider, ProviderChoice>,
}

impl ImageEditSelection {
    /// A selection of `provider` with its defaults filled (`fill_defaults`).
    #[must_use]
    pub fn new(provider: ImageEditProvider) -> Self {
        let mut selection = Self { provider, choices: HashMap::new() };
        selection.fill_defaults();
        selection
    }

    /// The selected provider's choice, if it has one.
    #[must_use]
    pub fn choice(&self) -> Option<&ProviderChoice> {
        self.choices.get(&self.provider)
    }

    /// The selected provider's choice, created empty on first use.
    fn choice_mut(&mut self) -> &mut ProviderChoice {
        self.choices.entry(self.provider).or_default()
    }

    /// The selected provider's model id as typed (untrimmed); empty when none is chosen.
    #[must_use]
    pub fn model_id(&self) -> &str {
        self.choice().map_or("", |choice| choice.model_id.as_str())
    }

    /// The selected region, for a provider with a region list. `None` for other providers and
    /// for a stored region id the provider does not have (never silently replaced by another
    /// region: a region-bound key must not be sent elsewhere).
    #[must_use]
    pub fn region(&self) -> Option<&'static EndpointRegion> {
        match self.provider.info().endpoint {
            EndpointKind::Regions(_) => self.choice().and_then(|choice| self.provider.region(choice.endpoint.trim())),
            EndpointKind::Fixed(_) | EndpointKind::UserBaseUrl => None,
        }
    }

    /// The typed base URL of the user's own server (as typed); empty for other providers.
    #[must_use]
    pub fn base_url(&self) -> &str {
        match self.provider.info().endpoint {
            EndpointKind::UserBaseUrl => self.choice().map_or("", |choice| choice.endpoint.as_str()),
            EndpointKind::Fixed(_) | EndpointKind::Regions(_) => "",
        }
    }

    /// The selected endpoint as the request and key-slot layer take it: `Default` for a fixed
    /// endpoint, the chosen `Region`, or the typed `BaseUrl` (validated by `keys::key_slot` and
    /// the executor).
    ///
    /// # Errors
    /// `ImageEditError::InvalidEndpoint` (detail: the stored id) when the stored region id is
    /// not one of the provider's regions; it is never replaced by the default region.
    pub fn endpoint_choice(&self) -> Result<EndpointChoice, ImageEditError> {
        match self.provider.info().endpoint {
            EndpointKind::Fixed(_) => Ok(EndpointChoice::Default),
            EndpointKind::Regions(_) => self.region().map(|region| EndpointChoice::Region(region.id)).ok_or_else(|| ImageEditError::InvalidEndpoint { detail: self.choice().map(|choice| choice.endpoint.trim().to_string()).unwrap_or_default() }),
            EndpointKind::UserBaseUrl => Ok(EndpointChoice::BaseUrl(self.base_url().to_string())),
        }
    }

    /// The catalogue offer of the selected model.
    ///
    /// # Errors
    /// `ImageEditError::UnknownModel` for an empty id or one the provider's catalogue lacks.
    pub fn offer(&self) -> Result<&'static ModelOffer, ImageEditError> {
        lookup(self.provider, self.model_id())
    }

    /// Fills the selected provider's EMPTY fields with defaults: the first catalogue model and,
    /// for a provider with a region list, its first (default) region. A non-empty model id or
    /// endpoint is never replaced, even when the catalogue no longer lists it. Returns whether
    /// anything changed (the consumer then saves its settings).
    pub fn fill_defaults(&mut self) -> bool {
        let provider = self.provider;
        let default_model = offers(provider).map(|offer| offer.model_id).find(|id| !id.is_empty());
        let default_region = match provider.info().endpoint {
            EndpointKind::Regions(regions) => regions.first().map(|region| region.id),
            EndpointKind::Fixed(_) | EndpointKind::UserBaseUrl => None,
        };
        let choice = self.choice_mut();
        let mut changed = false;
        if let Some(model) = default_model
            && choice.model_id.trim().is_empty()
        {
            choice.model_id = model.to_string();
            changed = true;
        }
        if let Some(region) = default_region
            && choice.endpoint.trim().is_empty()
        {
            choice.endpoint = region.to_string();
            changed = true;
        }
        changed
    }

    /// Selects `provider`, restoring its own choice and filling its empty defaults. Returns
    /// whether the provider changed.
    pub fn select_provider(&mut self, provider: ImageEditProvider) -> bool {
        if self.provider == provider {
            return false;
        }
        self.provider = provider;
        self.fill_defaults();
        true
    }
}

/// What the user changed or requested in one `draw_image_edit_picker` call.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageEditPickerActions {
    /// A persisted field changed (provider, model id, region, base URL): save settings.
    pub selection_changed: bool,
    /// The key slot may have changed (provider, region, or a committed base URL edit): resolve
    /// it again and pass it to `ImageEditKeyState::select_slot`.
    pub key_slot_changed: bool,
    /// The key block's buttons, for `ImageEditKeyRunner::pump`.
    pub key: KeyBlockActions,
}

/// One provider's Russia availability badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RussiaBadge {
    /// Status colour (`ms_theme::status`).
    pub color: Color32,
    /// Localized status text.
    pub text: &'static str,
    /// Localized reason behind the status (tooltip).
    pub note: &'static str,
}

impl RussiaBadge {
    /// The tooltip: the reason plus the date the status was researched.
    #[must_use]
    pub fn tooltip(&self) -> String {
        format!("{}\n{}", self.note, t!("ai_api.image_edit.russia.as_of_note"))
    }
}

/// The Russia badge for UI language subtag `language` (`LocaleTag::language`, e.g. `"ru"` for
/// `"ru-RU"`): `Some` only for Russian and a provider with a known status. Works = success
/// colour, payment issues = warning, blocked = error.
#[must_use]
pub fn russia_badge(language: &str, access: Option<RussiaAccess>) -> Option<RussiaBadge> {
    if language != RUSSIAN_LANGUAGE {
        return None;
    }
    let access = access?;
    let color = match access.status {
        RussiaStatus::Works => ms_theme::status::SUCCESS,
        RussiaStatus::PaymentIssues => ms_theme::status::WARNING,
        RussiaStatus::Blocked => ms_theme::status::ERROR,
    };
    Some(RussiaBadge { color, text: access.status.label(), note: access.note.text() })
}

/// `russia_badge` for `provider` in the active UI locale.
#[must_use]
pub fn active_russia_badge(provider: ImageEditProvider) -> Option<RussiaBadge> {
    russia_badge(ms_i18n::active_locale().language(), provider.info().russia)
}

/// Draws `badge` as a coloured label with its reason on hover.
pub fn draw_russia_badge(ui: &mut egui::Ui, badge: RussiaBadge) {
    ui.colored_label(badge.color, badge.text).on_hover_text(badge.tooltip());
}

/// Draws the picker for `selection` and its key block for `key` into `ui` and returns what the
/// user changed or requested.
///
/// `id_salt` prefixes every widget id and must be unique among the picker instances in one
/// parent `Ui`; `max_width` is the width of the combos and text fields. Choosing a provider
/// restores that provider's choice and fills its empty defaults (`select_provider`). The key
/// block is disabled while `key` has an operation in flight. Never blocks, never does I/O.
#[must_use]
pub fn draw_image_edit_picker<S: ImageEditKeyStore>(ui: &mut egui::Ui, id_salt: &str, max_width: f32, selection: &mut ImageEditSelection, key: &mut ImageEditKeyState<S>) -> ImageEditPickerActions {
    let mut actions = ImageEditPickerActions::default();
    draw_provider_picker(ui, id_salt, max_width, selection, &mut actions);
    draw_endpoint_field(ui, id_salt, max_width, selection, &mut actions);
    actions.key = draw_provider_key_block(ui, id_salt, max_width, selection.provider, key);
    draw_model_picker(ui, id_salt, max_width, selection, &mut actions);
    draw_offer_notes(ui, selection);
    actions
}

/// The provider label, the provider `SearchableComboBox` (Russia status as a coloured second
/// line in the Russian UI), then one row with the selected provider's badge and a small link to
/// its API documentation (`ProviderInfo::docs_url`; the hyperlink shows the URL on hover).
fn draw_provider_picker(ui: &mut egui::Ui, id_salt: &str, max_width: f32, selection: &mut ImageEditSelection, actions: &mut ImageEditPickerActions) {
    ui.label(t!("ai_api.image_edit.provider.picker_label"));
    let rows: Vec<(&'static str, Option<RussiaBadge>, String)> = ImageEditProvider::ALL
        .iter()
        .map(|provider| {
            let badge = active_russia_badge(*provider);
            (provider.info().label, badge, badge.map(|badge| badge.tooltip()).unwrap_or_default())
        })
        .collect();
    let items: Vec<SearchableComboItem<'_>> = rows
        .iter()
        .map(|(label, badge, tooltip)| match badge {
            Some(badge) => SearchableComboItem::with_secondary(label, badge.text).primary_color(badge.color).tooltip(tooltip),
            None => SearchableComboItem::new(label),
        })
        .collect();
    // `ALL` lists every provider, so the position always exists; 0 only guards the type.
    let mut index = ImageEditProvider::ALL.iter().position(|provider| *provider == selection.provider).unwrap_or(0);
    let response = SearchableComboBox::new(format!("{id_salt}_provider")).width(max_width).row_layout(RowLayout::Wide).show(ui, &mut index, &items);
    if response.changed
        && let Some(provider) = ImageEditProvider::ALL.get(index)
        && selection.select_provider(*provider)
    {
        actions.selection_changed = true;
        actions.key_slot_changed = true;
    }
    ui.horizontal_wrapped(|ui| {
        if let Some(badge) = active_russia_badge(selection.provider) {
            draw_russia_badge(ui, badge);
        }
        ui.hyperlink_to(egui::RichText::new(t!("ai_api.image_edit.provider.docs_link_label")).small(), selection.provider.info().docs_url);
    });
}

/// The region combo (region-list providers) or the server-address field (the user's own
/// server); nothing for a fixed endpoint.
fn draw_endpoint_field(ui: &mut egui::Ui, id_salt: &str, max_width: f32, selection: &mut ImageEditSelection, actions: &mut ImageEditPickerActions) {
    match selection.provider.info().endpoint {
        EndpointKind::Fixed(_) => {}
        EndpointKind::Regions(regions) => {
            ui.label(t!("ai_api.image_edit.endpoint.region_label"));
            // A stored id the provider no longer has is shown raw, not replaced.
            let caption = selection.region().map_or_else(|| selection.choice().map(|choice| choice.endpoint.clone()).unwrap_or_default(), |region| (region.label)().to_string());
            let mut changed = false;
            let choice = selection.choice_mut();
            WheelComboBox::from_id_salt(format!("{id_salt}_region")).width(max_width).selected_text(caption).show_ui(ui, |ui| {
                for region in regions {
                    changed |= ui.selectable_value(&mut choice.endpoint, region.id.to_string(), (region.label)()).changed();
                }
            });
            if changed {
                actions.selection_changed = true;
                actions.key_slot_changed = true;
            }
        }
        EndpointKind::UserBaseUrl => {
            ui.label(t!("ai_api.image_edit.endpoint.base_url_label"));
            let response = ui.add(egui::TextEdit::singleline(&mut selection.choice_mut().endpoint).id_salt(format!("{id_salt}_base_url")).desired_width(max_width).hint_text(t!("ai_api.image_edit.endpoint.base_url_hint")));
            actions.selection_changed |= response.changed();
            // A singleline field surrenders focus on Enter too; the slot (a per-URL key) is
            // re-resolved then, not per keystroke.
            actions.key_slot_changed |= response.lost_focus();
        }
    }
}

/// The key block (`draw_key_block`, disabled while an operation is in flight), the shared-key
/// hint and the key status line.
fn draw_provider_key_block<S: ImageEditKeyStore>(ui: &mut egui::Ui, id_salt: &str, max_width: f32, provider: ImageEditProvider, key: &mut ImageEditKeyState<S>) -> KeyBlockActions {
    let busy = key.is_busy();
    let view = key.block_view(provider.requires_key());
    // Own id scope: the key block uses auto ids, which must not shift with the endpoint field
    // above appearing or disappearing.
    let actions = ui
        .push_id(format!("{id_salt}_key"), |ui| {
            if busy {
                ui.disable();
            }
            draw_key_block(ui, max_width, view, &mut key.key_edit)
        })
        .inner;
    match provider.info().key_slot {
        ProviderKeySlot::SharedChat(service) => {
            ui.weak(tf!("ai_api.image_edit.key.shared_chat_hint", service = service.label()));
        }
        ProviderKeySlot::SharedCompatible => {
            ui.weak(t!("ai_api.image_edit.key.shared_compatible_hint"));
        }
        ProviderKeySlot::NamedPerRegion => {
            ui.weak(t!("ai_api.image_edit.key.per_region_hint"));
        }
        ProviderKeySlot::Named => {}
    }
    if !key.status().trim().is_empty() {
        ui.small(key.status());
    }
    actions
}

/// The model label, the catalogue combo (skipped when the provider lists no named model) and
/// the free model-id field. Picking a row writes its id; typing edits the id directly.
fn draw_model_picker(ui: &mut egui::Ui, id_salt: &str, max_width: f32, selection: &mut ImageEditSelection, actions: &mut ImageEditPickerActions) {
    ui.label(t!("ai_api.image_edit.model.picker_label"));
    let listed: Vec<&'static ModelOffer> = offers(selection.provider).filter(|offer| !offer.model_id.is_empty()).collect();
    if !listed.is_empty() {
        let items: Vec<SearchableComboItem<'_>> = listed
            .iter()
            .map(|offer| {
                let item = SearchableComboItem::with_secondary(offer.label, offer.model_id).tooltip(evidence_note(offer.evidence).1);
                // Only the doubtful rows are marked; a green list would drown the warning.
                if offer.evidence == SizeEvidence::Unverified { item.primary_color(ms_theme::status::WARNING) } else { item }
            })
            .collect();
        let current = selection.model_id().trim().to_string();
        let found = listed.iter().position(|offer| offer.model_id == current);
        // An id the catalogue lacks keeps an out-of-range index (drawn clamped, never written
        // back by the widget) and the typed id as the caption.
        let mut index = found.unwrap_or(listed.len());
        let mut combo = SearchableComboBox::new(format!("{id_salt}_model")).width(max_width).row_layout(RowLayout::Wide);
        if found.is_none() {
            combo = combo.selected_text(if current.is_empty() { t!("ai_api.image_edit.model.none_selected_label").to_string() } else { current });
        }
        let response = combo.show(ui, &mut index, &items);
        let picked = response.picked.or_else(|| response.changed.then_some(index));
        if let Some(offer) = picked.and_then(|picked| listed.get(picked)) {
            let choice = selection.choice_mut();
            if choice.model_id != offer.model_id {
                choice.model_id = offer.model_id.to_string();
                actions.selection_changed = true;
            }
        }
    }
    let response = ui.add(egui::TextEdit::singleline(&mut selection.choice_mut().model_id).id_salt(format!("{id_salt}_model_id")).desired_width(max_width).hint_text(t!("ai_api.image_edit.model.manual_id_hint")));
    actions.selection_changed |= response.changed();
}

/// The selected offer's notes: size evidence, mask support and an announced shutdown date; the
/// lookup error for an id the catalogue lacks; a hint while no model is chosen.
fn draw_offer_notes(ui: &mut egui::Ui, selection: &ImageEditSelection) {
    if selection.model_id().trim().is_empty() {
        ui.weak(t!("ai_api.image_edit.model.choose_hint"));
        return;
    }
    match selection.offer() {
        Ok(offer) => {
            match evidence_note(offer.evidence) {
                (Some(color), text) => ui.colored_label(color, text),
                (None, text) => ui.small(text),
            };
            ui.small(mask_note(offer.mask));
            if let Some(date) = offer.retires_on {
                ui.colored_label(ms_theme::status::WARNING, tf!("ai_api.image_edit.model.retires_warning", date = retire_date_text(date)));
            }
        }
        Err(error) => {
            ui.colored_label(ms_theme::status::WARNING, error.to_string());
        }
    }
}

/// The note on how well an offer's output size is evidenced, with its colour (`None`: plain
/// small text).
fn evidence_note(evidence: SizeEvidence) -> (Option<Color32>, &'static str) {
    match evidence {
        SizeEvidence::Documented => (Some(ms_theme::status::SUCCESS), t!("ai_api.image_edit.evidence.documented_status")),
        SizeEvidence::ParamOnly => (None, t!("ai_api.image_edit.evidence.param_only_status")),
        SizeEvidence::Unverified => (Some(ms_theme::status::WARNING), t!("ai_api.image_edit.evidence.unverified_status")),
    }
}

/// The note on an offer's native mask support.
fn mask_note(mask: MaskSupport) -> &'static str {
    match mask {
        MaskSupport::None => t!("ai_api.image_edit.mask.none_status"),
        MaskSupport::Soft => t!("ai_api.image_edit.mask.soft_status"),
        MaskSupport::Hard => t!("ai_api.image_edit.mask.hard_status"),
        MaskSupport::HardRequired => t!("ai_api.image_edit.mask.hard_required_status"),
    }
}

/// An announced shutdown date as ISO `YYYY-MM-DD` (locale-neutral).
fn retire_date_text((year, month, day): (u16, u8, u8)) -> String {
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::{ImageEditSelection, ProviderChoice, draw_image_edit_picker, retire_date_text, russia_badge};
    use crate::error::AiApiError;
    use crate::image_edit::catalog::offers;
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::key_state::{ImageEditKeyState, ImageEditKeyStore};
    use crate::image_edit::provider::{ImageEditProvider, RussiaAccess, RussiaNote, RussiaStatus};
    use crate::image_edit::request::EndpointChoice;

    /// A key slot that is never executed (the view only reads the state).
    #[derive(Debug, Clone, PartialEq)]
    struct NoStore;

    impl ImageEditKeyStore for NoStore {
        fn label(&self) -> &'static str {
            "none"
        }
        fn read_key(&self) -> Result<String, ImageEditError> {
            Err(ImageEditError::Key(AiApiError::KeyStoreWebUnavailable))
        }
        fn store_key(&self, _key: &str) -> Result<(), ImageEditError> {
            Err(ImageEditError::Key(AiApiError::KeyStoreWebUnavailable))
        }
        fn clear_key(&self) -> Result<(), ImageEditError> {
            Err(ImageEditError::Key(AiApiError::KeyStoreWebUnavailable))
        }
    }

    #[test]
    fn russia_badge_table() {
        let access = |status| Some(RussiaAccess { status, note: RussiaNote::ForeignCardRequired });
        let cases = [
            (RussiaStatus::Works, ms_theme::status::SUCCESS),
            (RussiaStatus::PaymentIssues, ms_theme::status::WARNING),
            (RussiaStatus::Blocked, ms_theme::status::ERROR),
        ];
        for (status, color) in cases {
            let badge = russia_badge("ru", access(status)).unwrap_or_else(|| panic!("{status:?}: badge in the Russian UI"));
            assert_eq!((badge.color, badge.text, badge.note), (color, status.label(), RussiaNote::ForeignCardRequired.text()));
            assert!(badge.tooltip().starts_with(badge.note), "{status:?}");
            assert_eq!(russia_badge("en", access(status)), None, "{status:?}: no badge outside the Russian UI");
        }
        let regional = ms_i18n::LocaleTag::parse("ru-RU").unwrap_or_else(|error| panic!("ru-RU parses: {error:?}"));
        assert!(russia_badge(regional.language(), access(RussiaStatus::Works)).is_some(), "a regional Russian tag gets the badge");
        assert_eq!(russia_badge("ru", None), None, "no status, no badge");
        assert_eq!(russia_badge("ru", ImageEditProvider::OpenAiCompatible.info().russia), None, "the user's own server has no badge");
    }

    #[test]
    fn default_fill_never_replaces_a_non_empty_id() {
        let first_bfl = offers(ImageEditProvider::Bfl).map(|offer| offer.model_id).find(|id| !id.is_empty()).unwrap_or_else(|| panic!("BFL has offers"));
        let fresh = ImageEditSelection::new(ImageEditProvider::Bfl);
        assert_eq!(fresh.model_id(), first_bfl);
        assert_eq!(fresh.region().map(|region| region.id), Some("global"), "the default region");

        let mut kept = ImageEditSelection { provider: ImageEditProvider::Bfl, choices: std::collections::HashMap::new() };
        kept.choices.insert(ImageEditProvider::Bfl, ProviderChoice { model_id: "retired-model".to_string(), endpoint: "eu".to_string() });
        assert!(!kept.fill_defaults(), "nothing empty, nothing filled");
        assert_eq!(kept.model_id(), "retired-model");
        assert_eq!(kept.region().map(|region| region.id), Some("eu"));

        let mut blank_model = kept.clone();
        blank_model.choices.insert(ImageEditProvider::Bfl, ProviderChoice { model_id: "  ".to_string(), endpoint: "us".to_string() });
        assert!(blank_model.fill_defaults());
        assert_eq!(blank_model.model_id(), first_bfl);
        assert_eq!(blank_model.region().map(|region| region.id), Some("us"), "a chosen region is kept");

        // The user's own server has no catalogue id and no region: nothing to fill.
        let mut own = ImageEditSelection::new(ImageEditProvider::OpenAiCompatible);
        assert_eq!(own.model_id(), "");
        assert!(!own.fill_defaults());
    }

    #[test]
    fn switching_provider_restores_its_own_choice() {
        let mut selection = ImageEditSelection::new(ImageEditProvider::OpenAi);
        selection.choices.insert(ImageEditProvider::OpenAi, ProviderChoice { model_id: "my-openai-id".to_string(), endpoint: String::new() });
        assert!(selection.select_provider(ImageEditProvider::OpenAiCompatible));
        selection.choices.insert(ImageEditProvider::OpenAiCompatible, ProviderChoice { model_id: "local-model".to_string(), endpoint: "http://127.0.0.1:8080".to_string() });
        assert_eq!(selection.base_url(), "http://127.0.0.1:8080");
        assert!(selection.select_provider(ImageEditProvider::OpenAi));
        assert!(!selection.select_provider(ImageEditProvider::OpenAi), "same provider is no change");
        assert_eq!(selection.model_id(), "my-openai-id");
        assert_eq!(selection.base_url(), "", "a hosted provider has no base URL");
        assert!(selection.select_provider(ImageEditProvider::OpenAiCompatible));
        assert_eq!(selection.model_id(), "local-model");
    }

    #[test]
    fn an_unknown_stored_region_is_not_replaced() {
        let mut selection = ImageEditSelection::new(ImageEditProvider::DashScope);
        selection.choices.insert(ImageEditProvider::DashScope, ProviderChoice { model_id: String::new(), endpoint: "mars".to_string() });
        assert!(selection.region().is_none());
        assert!(selection.fill_defaults(), "the empty model is filled");
        assert!(selection.region().is_none(), "the stored region id is kept, not swapped");
        assert!(matches!(selection.endpoint_choice(), Err(ImageEditError::InvalidEndpoint { detail }) if detail == "mars"));
        selection.choices.insert(ImageEditProvider::DashScope, ProviderChoice { model_id: String::new(), endpoint: "cn".to_string() });
        assert_eq!(selection.endpoint_choice().ok(), Some(EndpointChoice::Region("cn")));
        assert_eq!(ImageEditSelection::new(ImageEditProvider::Fal).endpoint_choice().ok(), Some(EndpointChoice::Default));
        let mut own = ImageEditSelection::new(ImageEditProvider::OpenAiCompatible);
        own.choices.insert(ImageEditProvider::OpenAiCompatible, ProviderChoice { model_id: String::new(), endpoint: "http://127.0.0.1:8080".to_string() });
        assert_eq!(own.endpoint_choice().ok(), Some(EndpointChoice::BaseUrl("http://127.0.0.1:8080".to_string())));
    }

    #[test]
    fn retire_dates_are_iso() {
        assert_eq!(retire_date_text((2026, 12, 1)), "2026-12-01");
    }

    // Every provider draws (catalogue combo or not, every endpoint kind, a busy key block) and
    // an idle frame reports nothing.
    #[test]
    fn every_provider_draws_and_an_idle_frame_requests_nothing() {
        for provider in ImageEditProvider::ALL {
            let ctx = egui::Context::default();
            let mut selection = ImageEditSelection::new(provider);
            let mut key: ImageEditKeyState<NoStore> = ImageEditKeyState::default();
            key.select_slot(Ok(NoStore));
            let busy = key.begin(crate::connection_view::KeyBlockActions::default());
            assert!(busy.is_some() && key.is_busy(), "{provider:?}: owed check in flight");
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 1400.0))), ..Default::default() };
            let mut flags = None;
            let output = ctx.run_ui(input, |ui| {
                let actions = draw_image_edit_picker(ui, "test_image_edit", 240.0, &mut selection, &mut key);
                flags = Some((actions.selection_changed, actions.key_slot_changed, actions.key.refresh, actions.key.save_key, actions.key.clear_key));
            });
            output.drop_without_applying_deltas();
            assert_eq!(flags, Some((false, false, false, false, false)), "{provider:?}");
        }
    }
}
