/*
File: crates/ms-ai-api/src/connection.rs

Purpose:
GUI-free state and logic of the AI API connection widget (service, key, model list, account
status, system instruction): which requests a button press starts and how a finished request
changes the state.

Key structures:
- AiApiConnectionState  : the widget's state; three persisted fields, five transient ones.
- AiApiConnectionActions: what the user asked for in one frame (returned by `draw_connection`).
- AiApiEventOutcome     : what applying one `AiApiEvent` produced.
- AiApiNotice           : a toast the consumer shows (text, severity, duration).

Key functions:
- AiApiConnectionState::new / begin_requests / apply_event

Notes:
The consumer owns persistence: it reads and writes `service`, `model` and
`system_instruction` under its own settings keys; this module never touches a file. The key
buffer `key_edit` is redacted from `Debug`.
*/

use std::fmt;

use ms_theme::Severity;

use crate::metadata::AiApiMetadata;
use crate::service::AiApiService;
use crate::tasks::{AiApiEvent, AiApiRequest};

/// Toast duration of the "key saved" notice, seconds.
const KEY_STORED_NOTICE_S: f64 = 2.2;
/// Toast duration of a failed request notice, seconds.
const REQUEST_FAILED_NOTICE_S: f64 = 3.0;

/// State of one AI API connection widget instance.
///
/// `service`, `model` and `system_instruction` are persisted by the consumer; the other fields
/// are transient session state. `Debug` redacts `key_edit`.
#[derive(Clone)]
pub struct AiApiConnectionState {
    /// Selected provider (persisted by the consumer as `AiApiService::key()`).
    pub service: AiApiService,
    /// Selected model id in the persisted UI form (persisted by the consumer).
    pub model: String,
    /// System prompt sent with every request (persisted by the consumer).
    pub system_instruction: String,
    /// Password-field buffer for a key to store; cleared once the key is stored or deleted.
    pub key_edit: String,
    /// Whether a key is stored; `None` until the first metadata refresh.
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
}

/// User requests collected from one `draw_connection` call.
#[derive(Debug, Clone, Copy, Default)]
#[expect(clippy::struct_excessive_bools, reason = "independent per-frame flags that can be set together (a service change sets both `options_changed` and `refresh`), matching the panels' action structs; not a state machine")]
pub struct AiApiConnectionActions {
    /// A persisted field (service, model, system instruction) changed: save settings.
    pub options_changed: bool,
    /// "Save" pressed: store `key_edit`.
    pub save_key: bool,
    /// "Delete" pressed: delete the stored key.
    pub clear_key: bool,
    /// "Refresh" pressed or the service changed: reload metadata.
    pub refresh: bool,
}

/// What `AiApiConnectionState::apply_event` produced for one event.
#[derive(Debug, Default)]
pub struct AiApiEventOutcome {
    /// `model` was replaced by the first listed model (persisted field changed).
    pub model_changed: bool,
    /// A request to submit next (the metadata refresh after a stored key).
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
            model: service.default_model().to_string(),
            system_instruction: default_system_instruction.into(),
            key_edit: String::new(),
            key_configured: None,
            models: Vec::new(),
            account_status: t!("ai_api.connection.press_refresh_status").to_string(),
            status: String::new(),
            initial_refresh_requested: false,
        }
    }

    /// Returns `true` exactly once per state: the first time the widget is shown, so it can
    /// verify the stored key and load the model list without a manual "Refresh". A refresh
    /// without a stored key does no network I/O (`load_metadata` only reads the key store).
    pub(crate) fn take_initial_refresh(&mut self) -> bool {
        !std::mem::replace(&mut self.initial_refresh_requested, true)
    }

    /// Resets everything tied to the previous service after the user picked a new `service`:
    /// default model, empty key buffer and model list, unverified key, cleared statuses.
    pub(crate) fn reset_for_service_change(&mut self) {
        self.model = self.service.default_model().to_string();
        self.key_edit.clear();
        self.key_configured = None;
        self.models.clear();
        self.account_status = t!("ai_api.connection.press_refresh_status").to_string();
        self.status.clear();
    }

    /// The model ids the model picker offers: the fetched list, or the service's default
    /// model before any refresh.
    pub(crate) fn model_choices(&self) -> Vec<String> {
        if self.models.is_empty() {
            vec![self.service.default_model().to_string()]
        } else {
            self.models.clone()
        }
    }

    /// Turns the pressed buttons into requests for the selected service, in the order save,
    /// clear, refresh, and sets `status` to the matching "in progress" text (the last one
    /// wins). `options_changed` starts nothing. `key_edit` is copied, not cleared: it is
    /// cleared when the key is confirmed stored.
    #[must_use]
    pub fn begin_requests(&mut self, actions: AiApiConnectionActions) -> Vec<AiApiRequest> {
        let service = self.service;
        let mut requests = Vec::new();
        if actions.save_key {
            self.status = t!("ai_api.connection.saving_key_status").to_string();
            requests.push(AiApiRequest::StoreKey { service, key: self.key_edit.clone() });
        }
        if actions.clear_key {
            self.status = t!("ai_api.connection.deleting_key_status").to_string();
            requests.push(AiApiRequest::ClearKey { service });
        }
        if actions.refresh {
            self.status = t!("ai_api.connection.refreshing_status").to_string();
            requests.push(AiApiRequest::RefreshMetadata { service });
        }
        requests
    }

    /// Applies a finished request. State changes only when the event's service is still the
    /// selected one; the "key saved" and "request failed" notices are produced regardless,
    /// because the user started that request. A metadata refresh replaces a model the
    /// provider no longer lists with the first listed one (`model_changed`).
    pub fn apply_event(&mut self, event: AiApiEvent) -> AiApiEventOutcome {
        let mut outcome = AiApiEventOutcome::default();
        match event {
            AiApiEvent::KeyStored { service } => {
                let text = tf!("ai_api.connection.api_key_saved_status", service = service.label());
                if self.service == service {
                    self.key_edit.clear();
                    self.key_configured = Some(true);
                    self.status.clone_from(&text);
                    outcome.follow_up = Some(AiApiRequest::RefreshMetadata { service });
                }
                outcome.notice = Some(AiApiNotice { text, severity: Severity::Success, duration_s: KEY_STORED_NOTICE_S });
            }
            AiApiEvent::KeyCleared { service } => {
                if self.service == service {
                    self.key_edit.clear();
                    self.key_configured = Some(false);
                    self.models.clear();
                    self.account_status = t!("ai_api.metadata.api_key_not_set_status").to_string();
                    self.status = tf!("ai_api.connection.api_key_deleted_status", service = service.label());
                }
            }
            AiApiEvent::MetadataLoaded(metadata) => {
                if self.service == metadata.service {
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

    /// Stores fetched metadata; returns whether `model` was replaced because the list does
    /// not contain it.
    fn apply_metadata(&mut self, metadata: AiApiMetadata) -> bool {
        self.key_configured = Some(metadata.key_configured);
        self.models = metadata.models;
        self.account_status = metadata.account_status;
        if !self.models.iter().any(|model| model == &self.model)
            && let Some(first) = self.models.first()
        {
            self.model.clone_from(first);
            return true;
        }
        false
    }
}

// Manual `Debug`: `key_edit` holds a plaintext API key while the user types it.
impl fmt::Debug for AiApiConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key_edit = if self.key_edit.is_empty() { "" } else { "<redacted>" };
        f.debug_struct("AiApiConnectionState")
            .field("service", &self.service)
            .field("model", &self.model)
            .field("system_instruction", &self.system_instruction)
            .field("key_edit", &key_edit)
            .field("key_configured", &self.key_configured)
            .field("models", &self.models)
            .field("account_status", &self.account_status)
            .field("status", &self.status)
            .field("initial_refresh_requested", &self.initial_refresh_requested)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{AiApiConnectionActions, AiApiConnectionState};
    use crate::error::AiApiError;
    use crate::metadata::AiApiMetadata;
    use crate::service::AiApiService;
    use crate::tasks::{AiApiEvent, AiApiRequest};
    use ms_theme::Severity;

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
        let outcome = state.apply_event(AiApiEvent::KeyStored { service: AiApiService::Groq });
        assert!(state.key_edit.is_empty());
        assert_eq!(state.key_configured, Some(true));
        assert!(matches!(outcome.follow_up, Some(AiApiRequest::RefreshMetadata { service: AiApiService::Groq })));
        let notice = outcome.notice.expect("key stored always notifies");
        assert_eq!(notice.severity, Severity::Success);
        assert_eq!(state.status, notice.text);
        assert!(!outcome.model_changed);
    }

    #[test]
    fn key_stored_for_other_service_only_notifies() {
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::KeyStored { service: AiApiService::OpenAi });
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
        let outcome = state.apply_event(AiApiEvent::KeyCleared { service: AiApiService::Groq });
        assert!(state.key_edit.is_empty() && state.models.is_empty());
        assert_eq!(state.key_configured, Some(false));
        assert_ne!(state.status, "before");
        assert!(outcome.notice.is_none() && outcome.follow_up.is_none());
    }

    #[test]
    fn metadata_replaces_unlisted_model() {
        let mut state = state();
        let outcome = state.apply_event(AiApiEvent::MetadataLoaded(metadata(AiApiService::Groq, &["groq::a", "groq::b"])));
        assert!(outcome.model_changed);
        assert_eq!(state.model, "groq::a");
        assert_eq!(state.models, vec!["groq::a".to_string(), "groq::b".to_string()]);
        assert_eq!(state.account_status, "account");
        assert_eq!(state.key_configured, Some(true));
        assert!(outcome.notice.is_none());
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
            AiApiRequest::StoreKey { service: AiApiService::Groq, key },
            AiApiRequest::ClearKey { service: AiApiService::Groq },
            AiApiRequest::RefreshMetadata { service: AiApiService::Groq },
        ] if key == "sk-secret-123"));
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
}
