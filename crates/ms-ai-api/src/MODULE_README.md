# Module: crates/ms-ai-api/src

## Purpose
The provider-neutral "AI API" layer over the `genai` multi-provider LLM client: which hosted
services exist, where their API keys live, how an authenticated client is built and driven
from a worker thread, which models a service offers, and a few provider-agnostic helpers.
It also owns the shared connection widget (service, key, model, account status, system
instruction) with its GUI-free state and background request runner. Every feature that talks to
a hosted model goes through it; today that is the translation tab's AI API OCR engine and AI API
machine translation (`crates/ms-tab-translation`).

Layer: one level above `ms-widgets`; it uses `ms-i18n` (localized texts), `ms-log`
(diagnostics), `ms-thread` (workers), `ms-theme` (`Severity`), `ms-widgets` (`WheelComboBox`)
and `egui`. It must never depend on a tab crate, `ms-models`, `ms-canvas` or `ms-config`
directly.

## Architecture
The crate owns everything up to the authenticated client; consumers own their requests. A
consumer reads the key (`keys`), builds a client (`client::build_client`) and the request
identity (`model_id::model_iden`), assembles its own `genai::chat::ChatRequest` with the
re-exported `ms_ai_api::genai` types, and runs it on a worker thread through
`client::block_on`.

Connection UI flow (per widget instance; the consumer owns one `AiApiConnectionState` and one
`AiApiTaskRunner`):

```text
draw_connection(ui, id_salt, max_width, &mut state) -> AiApiConnectionActions
                                                     // refresh on first draw + service change
  -> runner.submit_actions(&mut state, actions)      // begin_requests: status text + requests
  -> the runner's serial ms_thread worker, FIFO       // keys::* / load_metadata (blocking)
  -> runner.poll_and_apply(&mut state) each frame     // apply_event; KeyStored -> RefreshMetadata
  -> AiApiPollSummary { model_changed, notices }      // consumer: save settings, show toasts
```

## Files and submodules
- `lib.rs`: crate root, module wiring, root re-exports, `pub use genai` (native).
- `service.rs`: `AiApiService` — ids, labels, default models, `genai` adapter mapping.
- `error.rs`: `AiApiError` — typed error, `Display` is the localized message.
- `keys.rs`: credential-store CRUD (`read_api_key`, `store_api_key`, `clear_api_key`),
  `KEYRING_SERVICE`, wasm stubs.
- `client.rs` (native): `build_client`, `block_on`.
- `model_id.rs`: UI model-id prefix rules (`model_iden`, crate-private `ui_model_name`) and
  `is_likely_multimodal_model`.
- `metadata.rs`: `AiApiMetadata`, `load_metadata` (wasm stub), private model listing.
- `openrouter.rs` (native): `fetch_account_status` + pure `format_account_status`.
- `quota.rs`: `is_probable_quota_or_limit_error`.
- `encoding.rs`: `base64_encode`, the one encoder for binary chat parts.
- `connection.rs`: `AiApiConnectionState` (+ redacted `Debug`), `AiApiConnectionActions`,
  `AiApiEventOutcome`, `AiApiNotice`; `begin_requests` / `apply_event` (GUI-free, unit-tested).
- `connection_view.rs`: `draw_connection`, the egui widget; private `compact_middle`.
- `tasks.rs`: `AiApiRequest` (redacted `Debug`), `AiApiEvent`, `AiApiTaskRunner`,
  `AiApiPollSummary`.

## Contracts and invariants
- **Keys live only in the OS credential store**, never in a project or user settings file.
  Entry = (`KEYRING_SERVICE` = `"ManhwaStudio AI API OCR"`, `AiApiService::key()`); both are
  persistence contracts shared by OCR and MT (the "OCR" in the name is historical). A key is
  never logged, never put in an error, never put in a URL (OpenRouter gets it in the
  `Authorization` header only). `read_api_key` returns `Ok("")` when no key is stored.
- **`AiApiService::key()` values and `from_key` aliases are persisted** in the title
  `settings.json` by consumers; never change a value.
- **Model-id prefixes are persisted**: OpenRouter and Groq UI ids carry `open_router::` /
  `groq::`; `model_iden` strips any `prefix::` and binds the adapter explicitly.
- **The model list is filtered to likely-multimodal models for every consumer**, sorted,
  de-duplicated, and falls back to `default_model()`; the heuristic is
  `is_likely_multimodal_model`.
- **Blocking calls never run on the GUI thread**: key-store I/O, `load_metadata`,
  `fetch_account_status` and `block_on`. `block_on` builds a fresh multi-thread tokio runtime
  per call and must not be called from inside a tokio runtime.
- **Errors**: every fallible fn returns `AiApiError`; `to_string()` is the exact localized
  text the UI shows. Failures are logged here (`runtime_log`, service + error text) at the
  point they are created; consumers may add their own run-context logs.
- **Web build**: `genai`, `tokio`, `keyring`, `ureq`, `serde_json` are native-only. On wasm
  the key functions return `KeyStoreWebUnavailable` and `load_metadata` returns
  `MetadataWebUnavailable`; `client`, `openrouter` and `model_iden` do not exist.
- **Connection widget ids**: `draw_connection` draws straight into the caller's `Ui` — no
  `push_id`, `vertical`, `ScrollArea` or width clamp — and names its two combos
  `"{id_salt}_service"` / `"{id_salt}_model"` (string salts, `ui.id()`-scoped). Consumers'
  salts are widget-state keys: never change them. Scrolling and width are the caller's.
- **The consumer owns persistence** of `service` (as `AiApiService::key()`), `model` and
  `system_instruction` under its own settings keys; the other state fields are transient and the
  key buffer is never persisted or logged (`Debug` redacts it).
- **Runner threading**: `AiApiTaskRunner` lives on the GUI thread (`!Sync`). Each runner owns
  ONE detached `ms_thread` worker, spawned on the first `submit` and fed through a channel, that
  executes requests strictly in submission order, so events arrive in that order (a stale
  refresh never overwrites a newer one; save-then-delete reaches the credential store in that
  order). The worker exits once the runner is dropped, after finishing what was already queued;
  nobody joins it. `submit` never blocks; `poll` is a non-blocking drain. A spawn failure is
  logged and surfaces as a `Failed { TaskSpawn }` event; a worker that died (a panicking request)
  is logged and replaced on the next `submit`. Runners are independent of each other and of the
  consumers' own run workers. Events for a service that is no longer selected change no state,
  but "key saved" / "request failed" notices are still produced.
- **Not localized on purpose**: provider labels (brand names), the `"OpenRouter: "` status
  prefix and the `"{requests} req/{interval}"` rate-limit part.

## Future seams (boundaries, no code yet)
- `image_edit/`: provider-neutral image-editing requests, built on the same client/key layer.
- `model_db/`: a model capability database that replaces the `is_likely_multimodal_model`
  heuristic and the metadata model filter.

## Editing map
- Add a provider: `service.rs` (every match), then `model_id.rs` if its ids need a prefix.
- Change key storage: `keys.rs` (keep `KEYRING_SERVICE` and the user names).
- Change the model list or account status: `metadata.rs`, `openrouter.rs`.
- Change the connection widget layout: `connection_view.rs`; its state transitions and status
  texts: `connection.rs`; how requests run: `tasks.rs`.
- Add an error: `error.rs` variant + `ai_api.*` key in `crates/ms-i18n/locales/en.json` and
  `ru.json`.
