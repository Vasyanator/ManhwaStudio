/*
File: crates/ms-ai-api/src/connection.rs

Purpose:
GUI-free state and logic of the AI API connection widget (service, base URL of a compatible
service, key, model list, account status, system instruction): which requests a button press
starts and how a finished request changes the state.

Key structures:
- AiApiConnectionState  : the widget's state; four persisted fields, the rest transient.
- AiApiConnectionActions: what the user asked for in one frame (returned by `draw_connection`).
- AiApiEventOutcome     : what applying one `AiApiEvent` produced.
- AiApiNotice           : a toast the consumer shows (text, severity, duration).

Key functions:
- AiApiConnectionState::new / begin_requests / apply_event / target

Notes:
The consumer owns persistence: it reads and writes `service`, `base_url`, `model` and
`system_instruction` under its own settings keys; this module never touches a file. The key
buffer `key_edit` is redacted from `Debug`. Key and metadata requests are only queued for a valid
target: an invalid base URL of a compatible service becomes the status line instead of a request.
A compatible service's key is bound to its normalized base URL (`keys`), so key results apply
only while the same URL is selected, and a committed URL change resets the key state. A refresh
never replaces a non-empty model; it only fills an empty one (`apply_metadata`).
*/

use std::fmt;

use ms_theme::Severity;

use crate::error::AiApiError;
use crate::metadata::AiApiMetadata;
use crate::model_caps::is_non_chat_model;
use crate::service::AiApiService;
use crate::target::AiApiTarget;
use crate::tasks::{AiApiEvent, AiApiRequest};

/// Toast duration of the "key saved" notice, seconds.
const KEY_STORED_NOTICE_S: f64 = 2.2;
/// Toast duration of a failed request notice, seconds.
const REQUEST_FAILED_NOTICE_S: f64 = 3.0;

/// State of one AI API connection widget instance.
///
/// `service`, `base_url`, `model` and `system_instruction` are persisted by the consumer; the
/// other fields are transient session state. `Debug` redacts `key_edit`.
#[derive(Clone)]
pub struct AiApiConnectionState {
    /// Selected provider (persisted by the consumer as `AiApiService::key()`).
    pub service: AiApiService,
    /// Server address typed for a compatible service (persisted by the consumer as typed;
    /// normalized by `target()`). Ignored by hosted services and kept across service changes.
    pub base_url: String,
    /// Selected model id in the persisted UI form (persisted by the consumer).
    pub model: String,
    /// System prompt sent with every request (persisted by the consumer).
    pub system_instruction: String,
    /// Password-field buffer for a key to store; cleared once the key is stored or deleted.
    pub key_edit: String,
    /// Whether a key is stored for the current target (for a compatible service: for the current
    /// base URL); `None` until a metadata refresh for that target answered.
    pub key_configured: Option<bool>,
    /// Models offered by the last metadata refresh; empty before it.
    pub models: Vec<String>,
    /// Localized account status line (balance / limits).
    pub account_status: String,
    /// Localized status of the last request; empty hides the line.
    pub status: String,
    /// Whether the automatic first-show metadata refresh was already requested
    /// (`take_initial_refresh`). Private so the state is only built through `new`.
    initial_refresh_requested: bool,
    /// The trimmed `base_url` the last refresh was started for; a committed URL edit that
    /// differs from it invalidates the model list (`commit_base_url_edit`).
    refreshed_base_url: String,
}

/// User requests collected from one `draw_connection` call.
#[derive(Debug, Clone, Copy, Default)]
#[expect(clippy::struct_excessive_bools, reason = "independent per-frame flags that can be set together (a service change sets both `options_changed` and `refresh`), matching the panels' action structs; not a state machine")]
pub struct AiApiConnectionActions {
    /// A persisted field (service, base URL, model, system instruction) changed: save settings.
    pub options_changed: bool,
    /// "Save" pressed: store `key_edit`.
    pub save_key: bool,
    /// "Delete" pressed: delete the stored key.
    pub clear_key: bool,
    /// "Refresh" pressed, the service changed or a changed base URL was committed: reload
    /// metadata.
    pub refresh: bool,
}

/// What `AiApiConnectionState::apply_event` produced for one event.
#[derive(Debug, Default)]
pub struct AiApiEventOutcome {
    /// An empty `model` was filled from the fetched list (persisted field changed).
    pub model_changed: bool,
    /// A request to submit next (the metadata refresh after a stored key, or after a deleted
    /// key of a compatible service, whose models stay listable without one).
    pub follow_up: Option<AiApiRequest>,
    /// A toast to show.
    pub notice: Option<AiApiNotice>,
}

/// A toast the consumer shows: localized text, severity and display duration in seconds.
#[derive(Debug, Clone)]
pub struct AiApiNotice {
    pub text: String,
    pub severity: Severity,
    pub duration_s: f64,
}

impl AiApiConnectionState {
    /// Fresh state: `OpenAi` with its default model, the given system instruction, nothing
    /// verified yet and the "press refresh" account status.
    #[must_use]
    pub fn new(default_system_instruction: impl Into<String>) -> Self {
        let service = AiApiService::OpenAi;
        Self {
            service,
            base_url: String::new(),
            model: service.default_model().to_string(),
            system_instruction: default_system_instruction.into(),
            key_edit: String::new(),
            key_configured: None,
            models: Vec::new(),
            account_status: t!("ai_api.connection.press_refresh_status").to_string(),
            status: String::new(),
            initial_refresh_requested: false,
            refreshed_base_url: String::new(),
        }
    }

    /// The validated request target of the current selection (service + normalized base URL).
    ///
    /// # Errors
    /// `BaseUrlMissing` / `BaseUrlInvalid` for a compatible service without a usable base URL.
    pub fn target(&self) -> Result<AiApiTarget, AiApiError> {
        AiApiTarget::new(self.service, &self.base_url)
    }

    /// Called when the base URL field lost focus (or Enter was pressed). Returns `true` when the
    /// trimmed URL differs from the one the last refresh used; the model list and the key state
    /// of the old server (a key is bound to its URL) are then dropped and the caller requests a
    /// refresh, which answers the key state for the new URL.
    pub(crate) fn commit_base_url_edit(&mut self) -> bool {
        if self.base_url.trim() == self.refreshed_base_url {
            return false;
        }
        self.models.clear();
        self.key_configured = None;
        true
    }

    /// Whether `target` is the current selection (same service and, for a compatible service,
    /// the same normalized base URL); key results for any other target change no state.
    fn is_current_target(&self, target: &AiApiTarget) -> bool {
        self.target().is_ok_and(|current| &current == target)
    }

    /// A metadata refresh for the current target, or `None` with the target error as `status`
    /// when a compatible service has no usable base URL (shown in place, no toast: the user is
    /// still typing it). Records the URL the refresh was started for.
    fn refresh_request(&mut self) -> Option<AiApiRequest> {
        self.refreshed_base_url = self.base_url.trim().to_string();
        match self.target() {
            Ok(target) => Some(AiApiRequest::RefreshMetadata { target }),
            Err(error) => {
                self.status = error.to_string();
                None
            }
        }
    }

    /// Whether `metadata` was loaded for the current selection: same service and, for a
    /// compatible service, the same normalized base URL.
    fn metadata_matches_selection(&self, metadata: &AiApiMetadata) -> bool {
        self.service == metadata.service && self.target().ok().and_then(|target| target.endpoint().map(str::to_string)) == metadata.endpoint
    }

    /// Returns `true` exactly once per state: the first time the widget is shown, so it can
    /// verify the stored key and load the model list without a manual "Refresh". For a hosted
    /// service without a stored key that refresh does no network I/O (`load_metadata` only reads
    /// the key store); a compatible service lists its server's models with or without a key.
    pub(crate) fn take_initial_refresh(&mut self) -> bool {
        !std::mem::replace(&mut self.initial_refresh_requested, true)
    }

    /// Resets everything tied to the previous service after the user picked a new `service`:
    /// default model, empty key buffer and model list, unverified key, cleared statuses. The base
    /// URL is kept (switching between the two compatible protocols usually targets one server).
    pub(crate) fn reset_for_service_change(&mut self) {
        self.model = self.service.default_model().to_string();
        self.key_edit.clear();
        self.key_configured = None;
        self.models.clear();
        self.account_status = t!("ai_api.connection.press_refresh_status").to_string();
        self.status.clear();
    }

    /// The model ids the model picker offers: the fetched list, or the service's default
    /// model before any refresh (nothing for a compatible service, which has no default).
    pub(crate) fn model_choices(&self) -> Vec<String> {
        if !self.models.is_empty() {
            return self.models.clone();
        }
        let default_model = self.service.default_model();
        if default_model.is_empty() { Vec::new() } else { vec![default_model.to_string()] }
    }

    /// Turns the pressed buttons into requests for the current target, in the order save,
    /// clear, refresh, and sets `status` to the matching "in progress" text (the last one
    /// wins). For a compatible service without a valid base URL nothing is queued (a key is
    /// stored and deleted per URL, so there is nothing to address) and `status` is the URL error
    /// instead. `options_changed` starts nothing. `key_edit` is copied, not cleared: it is
    /// cleared when the key is confirmed stored.
    #[must_use]
    pub fn begin_requests(&mut self, actions: AiApiConnectionActions) -> Vec<AiApiRequest> {
        let mut requests = Vec::new();
        if actions.save_key || actions.clear_key {
            match self.target() {
                Ok(target) => {
                    if actions.save_key {
                        self.status = t!("ai_api.connection.saving_key_status").to_string();
                        requests.push(AiApiRequest::StoreKey { target: target.clone(), key: self.key_edit.clone() });
                    }
                    if actions.clear_key {
                        self.status = t!("ai_api.connection.deleting_key_status").to_string();
                        requests.push(AiApiRequest::ClearKey { target });
                    }
                }
                Err(error) => self.status = error.to_string(),
            }
        }
        if actions.refresh {
            self.status = t!("ai_api.connection.refreshing_status").to_string();
            requests.extend(self.refresh_request());
        }
        requests
    }

    /// Applies a finished request. State changes only when the event's target (key events and
    /// metadata: service AND normalized base URL; failures: service) is still the selected one;
    /// the "key saved" and "request failed" notices are produced regardless, because the user
    /// started that request. A metadata refresh fills an EMPTY model from the fetched list
    /// (`model_changed`, see `apply_metadata`); a non-empty model is never replaced.
    pub fn apply_event(&mut self, event: AiApiEvent) -> AiApiEventOutcome {
        let mut outcome = AiApiEventOutcome::default();
        match event {
            AiApiEvent::KeyStored { target } => {
                let text = tf!("ai_api.connection.api_key_saved_status", service = target.service().label());
                if self.is_current_target(&target) {
                    self.key_edit.clear();
                    self.key_configured = Some(true);
                    self.status.clone_from(&text);
                    outcome.follow_up = self.refresh_request();
                }
                outcome.notice = Some(AiApiNotice { text, severity: Severity::Success, duration_s: KEY_STORED_NOTICE_S });
            }
            AiApiEvent::KeyCleared { target } => {
                if self.is_current_target(&target) {
                    let service = target.service();
                    self.key_edit.clear();
                    self.key_configured = Some(false);
                    self.status = tf!("ai_api.connection.api_key_deleted_status", service = service.label());
                    if service.requires_key() {
                        self.models.clear();
                        self.account_status = t!("ai_api.metadata.api_key_not_set_status").to_string();
                    } else {
                        // A compatible server may still answer without a key: re-list its models.
                        outcome.follow_up = self.refresh_request();
                    }
                }
            }
            AiApiEvent::MetadataLoaded(metadata) => {
                if self.metadata_matches_selection(&metadata) {
                    outcome.model_changed = self.apply_metadata(metadata);
                    self.status = t!("ai_api.connection.updated_status").to_string();
                }
            }
            AiApiEvent::Failed { service, error } => {
                if self.service == service {
                    self.status = error.to_string();
                }
                outcome.notice = Some(AiApiNotice {
                    text: tf!("ai_api.connection.request_failed_error", error = error),
                    severity: Severity::Error,
                    duration_s: REQUEST_FAILED_NOTICE_S,
                });
            }
        }
        outcome
    }

    /// Stores fetched metadata; returns whether `model` changed. A non-empty model (typed by
    /// hand, saved, or retired by the provider) is kept even when the list lacks it: the user
    /// chose it, and an automatic pick could land on a model that cannot serve the request. An
    /// empty model is filled with the service default if listed, else the first listed model
    /// that is not a non-chat model (`model_caps::is_non_chat_model`), else the first listed.
    fn apply_metadata(&mut self, metadata: AiApiMetadata) -> bool {
        self.key_configured = Some(metadata.key_configured);
        self.models = metadata.models;
        self.account_status = metadata.account_status;
        if !self.model.trim().is_empty() {
            return false;
        }
        let default_model = self.service.default_model();
        let picked = self
            .models
            .iter()
            .find(|model| !default_model.is_empty() && model.as_str() == default_model)
            .or_else(|| self.models.iter().find(|model| !is_non_chat_model(model)))
            .or_else(|| self.models.first());
        match picked {
            Some(picked) if *picked != self.model => {
                self.model.clone_from(picked);
                true
            }
            Some(_) | None => false,
        }
    }
}

// Manual `Debug`: `key_edit` holds a plaintext API key while the user types it.
impl fmt::Debug for AiApiConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_edit = if self.key_edit.is_empty() { "" } else { "<redacted>" };
        f.debug_struct("AiApiConnectionState")
            .field("service", &self.service)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("system_instruction", &self.system_instruction)
            .field("key_edit", &key_edit)
            .field("key_configured", &self.key_configured)
            .field("models", &self.models)
            .field("account_status", &self.account_status)
            .field("status", &self.status)
            .field("initial_refresh_requested", &self.initial_refresh_requested)
            .field("refreshed_base_url", &self.refreshed_base_url)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{AiApiConnectionActions, AiApiConnectionState};
    use crate::error::AiApiError;
    use crate::metadata::AiApiMetadata;
    use crate::service::AiApiService;
    use crate::target::AiApiTarget;
    use crate::tasks::{AiApiEvent, AiApiRequest};
    use ms_theme::Severity;

    /// The validated target of `service` at `base_url` (ignored for a hosted service).
    fn target(service: AiApiService, base_url: &str) -> AiApiTarget {
        AiApiTarget::new(service, base_url).unwrap_or_else(|error| panic!("test target {service:?} {base_url:?}: {error:?}"))
    }

    // Localized texts are not asserted (no UI locale is installed in this test binary); the
    // tests pin which fields change and which requests/notices are produced.

    fn state() -> AiApiConnectionState {
        let mut state = AiApiConnectionState::new("instruction");
        state.service = AiApiService::Groq;
        state.model = "groq::kept".to_string();
        state.key_edit = "sk-secret-123".to_string();
        state.status = "before".to_string();
        state
    }

    #[test]
    fn initial_refresh_is_requested_exactly_once() {
        let mut state = AiApiConnectionState::new("instruction");
        assert!(state.take_initial_refresh());
        assert!(!state.take_initial_refresh());
        // A clone keeps the flag: re-drawing a copied state must not re-trigger the refresh.
        assert!(!state.clone().take_initial_refresh());
    }

    fn metadata(service: AiApiService, models: &[&str]) -> AiApiMetadata {
        AiApiMetadata {
            service,
            endpoint: None,
            key_configured: true,
            models: models.iter().map(|model| (*model).to_string()).collect(),
            account_status: "account".to_string(),
        }
    }

    #[test]
    fn new_uses_given_instruction_and_openai_default() {
        let state = AiApiConnectionState::new("prompt text");
        assert_eq!(state.system_instruction, "prompt text");
        assert_eq!(state.service, AiApiService::OpenAi);
        assert_eq!(state.model, AiApiService::OpenAi.default_model());
        assert_eq!(state.key_configured, None);
        assert!(state.models.is_empty() && state.key_edit.is_empty() && state.status.is_empty());
    }

    #[test]
    fn key_stored_for_selected_service_refreshes_and_notifies() {
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::KeyStored { target: target(AiApiService::Groq, "") });
        assert!(state.key_edit.is_empty());
        assert_eq!(state.key_configured, Some(true));
        assert!(matches!(&outcome.follow_up, Some(AiApiRequest::RefreshMetadata { target }) if target.service() == AiApiService::Groq && target.endpoint().is_none()));
        let notice = outcome.notice.expect("key stored always notifies");
        assert_eq!(notice.severity, Severity::Success);
        assert_eq!(state.status, notice.text);
        assert!(!outcome.model_changed);
    }

    #[test]
    fn key_stored_for_other_service_only_notifies() {
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::KeyStored { target: target(AiApiService::OpenAi, "") });
        assert_eq!(state.key_edit, "sk-secret-123");
        assert_eq!(state.key_configured, None);
        assert_eq!(state.status, "before");
        assert!(outcome.follow_up.is_none());
        assert!(outcome.notice.is_some());
    }

    #[test]
    fn key_cleared_resets_key_state_without_notice() {
        let mut state = state();
        state.models = vec!["groq::a".to_string()];
        let outcome = state.apply_event(AiApiEvent::KeyCleared { target: target(AiApiService::Groq, "") });
        assert!(state.key_edit.is_empty() && state.models.is_empty());
        assert_eq!(state.key_configured, Some(false));
        assert_ne!(state.status, "before");
        assert!(outcome.notice.is_none() && outcome.follow_up.is_none());
    }

    #[test]
    fn metadata_keeps_a_non_empty_unlisted_model() {
        // A hand-typed, saved or retired model is the user's choice: never auto-replaced.
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(metadata(AiApiService::Groq, &["groq::a", "groq::b"])));
        assert!(!outcome.model_changed);
        assert_eq!(state.model, "groq::kept");
        assert_eq!(state.models, vec!["groq::a".to_string(), "groq::b".to_string()]);
        assert_eq!(state.account_status, "account");
        assert_eq!(state.key_configured, Some(true));
        assert!(outcome.notice.is_none());
    }

    #[test]
    fn metadata_fills_an_empty_model_with_the_listed_service_default() {
        let mut state = AiApiConnectionState::new("instruction");
        state.model.clear();
        let default_model = AiApiService::OpenAi.default_model();
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(metadata(AiApiService::OpenAi, &["babbage-002", "chatgpt-4o-latest", default_model])));
        assert!(outcome.model_changed);
        assert_eq!(state.model, default_model);
    }

    #[test]
    fn metadata_fills_an_empty_model_with_the_first_chat_model() {
        // Sorted OpenAI-style list without the default: the non-chat models in front are skipped.
        let mut state = AiApiConnectionState::new("instruction");
        state.model = "  ".to_string();
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(metadata(AiApiService::OpenAi, &["babbage-002", "dall-e-2", "gpt-4o-mini-tts-2025-03-20", "local-chat", "whisper-1"])));
        assert!(outcome.model_changed);
        assert_eq!(state.model, "local-chat");

        // Only non-chat models listed: the first one, rather than leaving the field empty.
        let mut state = AiApiConnectionState::new("instruction");
        state.model.clear();
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(metadata(AiApiService::OpenAi, &["text-embedding-3-small", "tts-1"])));
        assert!(outcome.model_changed);
        assert_eq!(state.model, "text-embedding-3-small");
    }

    #[test]
    fn metadata_keeps_listed_model() {
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(metadata(AiApiService::Groq, &["groq::a", "groq::kept"])));
        assert!(!outcome.model_changed);
        assert_eq!(state.model, "groq::kept");
    }

    #[test]
    fn metadata_with_empty_list_keeps_model() {
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(metadata(AiApiService::Groq, &[])));
        assert!(!outcome.model_changed);
        assert_eq!(state.model, "groq::kept");
    }

    #[test]
    fn metadata_for_other_service_is_ignored() {
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(metadata(AiApiService::OpenAi, &["gpt-x"])));
        assert!(!outcome.model_changed);
        assert_eq!(state.model, "groq::kept");
        assert!(state.models.is_empty());
        assert_eq!(state.status, "before");
    }

    #[test]
    fn failed_sets_status_only_for_selected_service_but_always_notifies() {
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::Failed { service: AiApiService::OpenAi, error: AiApiError::EmptyKey });
        assert_eq!(state.status, "before");
        assert_eq!(outcome.notice.as_ref().map(|notice| notice.severity), Some(Severity::Error));

        let outcome = state.apply_event(AiApiEvent::Failed { service: AiApiService::Groq, error: AiApiError::EmptyKey });
        assert_eq!(state.status, AiApiError::EmptyKey.to_string());
        assert!(outcome.notice.is_some());
    }

    #[test]
    fn begin_requests_orders_save_clear_refresh_and_keeps_key_buffer() {
        let mut state = state();
        let actions = AiApiConnectionActions { options_changed: true, save_key: true, clear_key: true, refresh: true };
        let requests = state.begin_requests(actions);
        assert!(matches!(&requests[..], [
            AiApiRequest::StoreKey { target: store, key },
            AiApiRequest::ClearKey { target: clear },
            AiApiRequest::RefreshMetadata { target },
        ] if key == "sk-secret-123" && store.service() == AiApiService::Groq && clear.service() == AiApiService::Groq && target.service() == AiApiService::Groq));
        assert_eq!(state.key_edit, "sk-secret-123");

        // The last started request decides the status text.
        let mut refresh_only = self::state();
        let _requests = refresh_only.begin_requests(AiApiConnectionActions { refresh: true, ..AiApiConnectionActions::default() });
        assert_eq!(state.status, refresh_only.status);
        assert_ne!(state.status, "before");
    }

    #[test]
    fn options_changed_alone_starts_nothing() {
        let mut state = state();
        let requests = state.begin_requests(AiApiConnectionActions { options_changed: true, ..AiApiConnectionActions::default() });
        assert!(requests.is_empty());
        assert_eq!(state.status, "before");
    }

    #[test]
    fn debug_redacts_key_buffer() {
        let debug = format!("{:?}", state());
        assert!(!debug.contains("sk-secret-123"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn service_change_reset_and_model_choices() {
        let mut state = state();
        state.models = vec!["groq::a".to_string()];
        assert_eq!(state.model_choices(), vec!["groq::a".to_string()]);
        state.service = AiApiService::Anthropic;
        state.reset_for_service_change();
        assert_eq!(state.model, AiApiService::Anthropic.default_model());
        assert!(state.key_edit.is_empty() && state.models.is_empty() && state.status.is_empty());
        assert_eq!(state.key_configured, None);
        assert_eq!(state.model_choices(), vec![AiApiService::Anthropic.default_model().to_string()]);
    }

    /// A state on a compatible service with the given typed base URL.
    fn compatible_state(base_url: &str) -> AiApiConnectionState {
        let mut state = AiApiConnectionState::new("instruction");
        state.service = AiApiService::OpenAiCompatible;
        state.reset_for_service_change();
        state.base_url = base_url.to_string();
        state
    }

    fn refresh() -> AiApiConnectionActions {
        AiApiConnectionActions { refresh: true, ..AiApiConnectionActions::default() }
    }

    #[test]
    fn compatible_service_has_no_default_model_choice() {
        let state = compatible_state("http://127.0.0.1:8080");
        assert!(state.model.is_empty());
        assert!(state.model_choices().is_empty());
    }

    #[test]
    fn compatible_refresh_without_valid_url_queues_nothing_and_shows_the_error() {
        for (url, expected) in [("", AiApiError::BaseUrlMissing { service: AiApiService::OpenAiCompatible }), ("localhost:8080", AiApiError::BaseUrlInvalid { url: "localhost:8080".to_string() })] {
            let mut state = compatible_state(url);
            assert!(state.begin_requests(refresh()).is_empty(), "{url:?}");
            assert_eq!(state.status, expected.to_string(), "{url:?}");
        }
    }

    #[test]
    fn compatible_refresh_carries_the_normalized_url() {
        let mut state = compatible_state(" http://127.0.0.1:8080 ");
        let requests = state.begin_requests(refresh());
        assert!(matches!(&requests[..], [AiApiRequest::RefreshMetadata { target }] if target.endpoint() == Some("http://127.0.0.1:8080/v1/")));
        // The same URL committed again is no change; a different one drops the stale list.
        state.models = vec!["local".to_string()];
        assert!(!state.commit_base_url_edit());
        assert_eq!(state.models, vec!["local".to_string()]);
        state.base_url = "http://127.0.0.1:9090".to_string();
        assert!(state.commit_base_url_edit());
        assert!(state.models.is_empty());
    }

    #[test]
    fn metadata_from_another_base_url_is_ignored() {
        let mut state = compatible_state("http://127.0.0.1:9090");
        let mut old_server = metadata(AiApiService::OpenAiCompatible, &["old-server-model"]);
        old_server.endpoint = Some("http://127.0.0.1:8080/v1/".to_string());
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(old_server));
        assert!(!outcome.model_changed && state.models.is_empty());

        let mut current = metadata(AiApiService::OpenAiCompatible, &["new-server-model"]);
        current.endpoint = Some("http://127.0.0.1:9090/v1/".to_string());
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(current));
        assert!(outcome.model_changed);
        assert_eq!(state.model, "new-server-model");
    }

    #[test]
    fn deleting_the_key_of_a_compatible_service_relists_its_models() {
        let mut state = compatible_state("http://127.0.0.1:8080");
        state.models = vec!["local".to_string()];
        let outcome = state.apply_event(AiApiEvent::KeyCleared { target: target(AiApiService::OpenAiCompatible, "http://127.0.0.1:8080/v1/") });
        assert_eq!(state.key_configured, Some(false));
        assert_eq!(state.models, vec!["local".to_string()]);
        assert!(matches!(&outcome.follow_up, Some(AiApiRequest::RefreshMetadata { target }) if target.service() == AiApiService::OpenAiCompatible));
    }

    #[test]
    fn compatible_key_requests_are_bound_to_the_normalized_url() {
        let mut state = compatible_state(" HTTP://127.0.0.1:8080 ");
        let requests = state.begin_requests(AiApiConnectionActions { save_key: true, clear_key: true, ..AiApiConnectionActions::default() });
        assert!(matches!(&requests[..], [
            AiApiRequest::StoreKey { target: store, .. },
            AiApiRequest::ClearKey { target: clear },
        ] if store.endpoint() == Some("http://127.0.0.1:8080/v1/") && clear == store));
    }

    #[test]
    fn compatible_key_requests_without_valid_url_queue_nothing_and_show_the_error() {
        for url in ["", "http://user:pw@127.0.0.1:8080"] {
            let mut state = compatible_state(url);
            state.key_edit = "sk-secret-123".to_string();
            let requests = state.begin_requests(AiApiConnectionActions { save_key: true, clear_key: true, ..AiApiConnectionActions::default() });
            assert!(requests.is_empty(), "{url:?}");
            assert_eq!(state.status, state.target().err().map(|error| error.to_string()).unwrap_or_default(), "{url:?}");
            assert_eq!(state.key_edit, "sk-secret-123");
        }
    }

    #[test]
    fn key_events_for_another_url_change_no_state() {
        let mut state = compatible_state("http://127.0.0.1:9090");
        state.key_edit = "sk-secret-123".to_string();
        let old_server = target(AiApiService::OpenAiCompatible, "http://127.0.0.1:8080");
        let outcome = state.apply_event(AiApiEvent::KeyStored { target: old_server.clone() });
        assert_eq!(state.key_configured, None);
        assert_eq!(state.key_edit, "sk-secret-123");
        assert!(outcome.follow_up.is_none() && outcome.notice.is_some());
        let outcome = state.apply_event(AiApiEvent::KeyCleared { target: old_server });
        assert_eq!(state.key_configured, None);
        assert!(outcome.follow_up.is_none());

        let outcome = state.apply_event(AiApiEvent::KeyStored { target: target(AiApiService::OpenAiCompatible, "http://127.0.0.1:9090/v1") });
        assert_eq!(state.key_configured, Some(true));
        assert!(state.key_edit.is_empty());
        assert!(matches!(&outcome.follow_up, Some(AiApiRequest::RefreshMetadata { target }) if target.endpoint() == Some("http://127.0.0.1:9090/v1/")));
    }

    #[test]
    fn committing_a_new_url_forgets_the_old_key_state() {
        let mut state = compatible_state("http://127.0.0.1:8080");
        let _requests = state.begin_requests(refresh());
        state.key_configured = Some(true);
        assert!(!state.commit_base_url_edit());
        assert_eq!(state.key_configured, Some(true));
        state.base_url = "http://127.0.0.1:9090".to_string();
        assert!(state.commit_base_url_edit());
        assert_eq!(state.key_configured, None);
    }
}
