/*
File: crates/ms-ai-api/src/lib.rs

Purpose:
Crate root of `ms-ai-api`, the provider-neutral layer over the `genai` multi-provider LLM
client ("AI API") shared by every feature that talks to a hosted model (OCR and machine
translation in the translation tab today), including the shared connection widget.

Modules:
- `service`   : `AiApiService`, the persisted provider catalogue (keys, labels, default models),
                including the two "compatible" services that need a base URL.
- `target`    : `AiApiTarget` (service + validated base URL), `normalize_base_url`.
- `error`     : `AiApiError`, the typed error whose `Display` is the localized user message.
- `keys`      : API keys in the OS credential store (read / store / clear), wasm stubs.
- `client`    : authenticated `genai::Client` and the blocking async bridge (native only).
- `model_id`  : model-id prefix rules (persisted UI ids <-> `genai::ModelIden`).
- `model_caps`: the model capability database (image input: `image_input_support`).
- `metadata`  : `AiApiMetadata` and `load_metadata` (key state, model list, account status).
- `openrouter`: OpenRouter account status fetch + its pure formatter (native only).
- `quota`     : provider-agnostic "out of credits / rate limited" error classifier.
- `encoding`  : the one base64 encoder for binary chat parts.
- `connection`: `AiApiConnectionState` (widget state), its actions, `apply_event`.
- `connection_view`: `draw_connection`, the shared egui connection widget.
- `tasks`     : `AiApiRequest` / `AiApiEvent` / `AiApiTaskRunner` (one serial `ms_thread`
                worker per runner, FIFO).

Notes:
`genai` is re-exported (native only) so consumers build chat requests against the same
crate version without a manifest edge of their own. Blocking calls (`client::block_on`,
key-store I/O, `load_metadata`) must never run on the GUI thread.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

// The `ms-i18n` UI-string macros (`t!` / `tf!`), mounted crate-wide: the key-validation
// test scans for these bare macro names.
#[macro_use]
extern crate ms_i18n;

#[cfg(not(target_arch = "wasm32"))]
pub mod client;
pub mod connection;
pub mod connection_view;
pub mod encoding;
pub mod error;
pub mod keys;
pub mod metadata;
pub mod model_caps;
pub mod model_id;
#[cfg(not(target_arch = "wasm32"))]
pub mod openrouter;
pub mod quota;
pub mod service;
pub mod target;
pub mod tasks;

pub use connection::{AiApiConnectionActions, AiApiConnectionState, AiApiEventOutcome, AiApiNotice};
pub use connection_view::draw_connection;
pub use encoding::base64_encode;
pub use error::AiApiError;
pub use metadata::{AiApiMetadata, load_metadata};
pub use model_caps::{ImageInputSupport, image_input_support};
pub use quota::is_probable_quota_or_limit_error;
pub use service::AiApiService;
pub use target::AiApiTarget;
pub use tasks::{AiApiEvent, AiApiPollSummary, AiApiRequest, AiApiTaskRunner};

/// The `genai` crate itself, so consumers build chat requests (`genai::chat::*`) against the
/// one version this crate's client uses. Native only: `genai` is not compiled for wasm.
#[cfg(not(target_arch = "wasm32"))]
pub use genai;
