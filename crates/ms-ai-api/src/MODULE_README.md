# Module: crates/ms-ai-api/src

## Purpose
The provider-neutral "AI API" layer over the `genai` multi-provider LLM client: which services
exist (hosted providers plus "OpenAI-compatible" / "Anthropic-compatible" servers at a
user-given base URL), where their API keys live, how an authenticated client is built and driven
from a worker thread, which models a service offers, what a model can do (the model capability
database), and a few provider-agnostic helpers. It also owns the shared connection widget
(service, base URL, key, model, account status, system instruction) with its GUI-free state and
background request runner. Every feature that talks to
a hosted model goes through it; today that is the translation tab's AI API OCR engine and AI API
machine translation (`crates/ms-tab-translation`). The cloud image-edit layer (`image_edit/`:
hosted image-editing models with a size-exact pipeline) lives here too.

Layer: one level above `ms-widgets`; it uses `ms-i18n` (localized texts), `ms-log`
(diagnostics), `ms-thread` (workers), `ms-theme` (`Severity`), `ms-widgets` (`WheelComboBox`),
`ms-raster` and `image` (the image-edit pipeline) and `egui`. It must never depend on a tab crate, `ms-models`, `ms-canvas` or `ms-config`
directly.

## Architecture
The crate owns everything up to the authenticated client; consumers own their requests. A
consumer validates the destination (`AiApiTarget::new(service, base_url)`), reads the key of
that target (`keys::read_api_key(&target)`; optional when `!service.requires_key()`), builds a client (`client::build_client`) and
the request identity (`model_id::model_iden`), assembles its own `genai::chat::ChatRequest` with
the re-exported `ms_ai_api::genai` types, and runs it on a worker thread through
`client::block_on`.

Connection UI flow (per widget instance; the consumer owns one `AiApiConnectionState` and one
`AiApiTaskRunner`):

```text
draw_connection(ui, id_salt, max_width, &mut state) -> AiApiConnectionActions
                     // refresh on first draw, service change, committed base-URL change
  -> runner.submit_actions(&mut state, actions)      // begin_requests: status text + requests
                                                     // (invalid base URL: status only)
  -> the runner's serial ms_thread worker, FIFO       // keys::* / load_metadata (blocking)
  -> runner.poll_and_apply(&mut state) each frame     // apply_event; KeyStored -> RefreshMetadata
  -> AiApiPollSummary { model_changed, notices }      // consumer: save settings, show toasts
```

## Files and submodules
- `lib.rs`: crate root, module wiring, root re-exports, `pub use genai` (native).
- `service.rs`: `AiApiService` — ids, labels, default models, `genai` adapter mapping,
  `uses_base_url` / `requires_key`.
- `target.rs`: `AiApiTarget` (service + normalized base URL) and the pure `normalize_base_url`.
- `error.rs`: `AiApiError` — typed error, `Display` is the localized message.
- `keys.rs`: credential-store CRUD per `AiApiTarget` (`read_api_key`, `store_api_key`,
  `clear_api_key`) and per named slot (`NamedKeyUser`, `read_named_key`, `store_named_key`,
  `clear_named_key`), `KEYRING_SERVICE`, the private user-name rule, wasm stubs.
- `client.rs` (native): `build_client` (auth + base-URL routing for one target), `block_on`.
- `model_id.rs` (native items): UI model-id prefix rules (`model_iden`, crate-private
  `ui_model_name`).
- `model_caps.rs`: the LLM model capability database; today `ImageInputSupport` /
  `image_input_support` over an ordered, hard-coded table, and `is_non_chat_model` (the
  non-chat markers alone).
- `metadata.rs`: `AiApiMetadata`, `load_metadata` (wasm stub), private model listing.
- `openrouter.rs` (native): `fetch_account_status` + pure `format_account_status`.
- `quota.rs`: `is_probable_quota_or_limit_error`.
- `encoding.rs`: `base64_encode` / strict `base64_decode` (`Base64Error`), the one base64 codec
  (chat parts, image-edit bodies and responses).
- `image_edit/`: the cloud image-edit layer (provider and model catalogue, size-rule data, the
  size-exact pipeline, one adapter per API shape in `adapters/`, the native HTTP executor, the
  provider / model picker `view.rs` and its single-flight key state `key_state.rs`); see its
  `MODULE_README.md`.
- `connection.rs`: `AiApiConnectionState` (+ redacted `Debug`), `AiApiConnectionActions`,
  `AiApiEventOutcome`, `AiApiNotice`; `begin_requests` / `apply_event` (GUI-free, unit-tested).
- `connection_view.rs`: `draw_connection`, the egui widget; `draw_key_block` (`KeyBlockView`,
  `KeyBlockActions`), the key block it draws in place and other key owners reuse; private
  `compact_middle`.
- `tasks.rs`: `AiApiRequest` (redacted `Debug`), `AiApiEvent`, `AiApiTaskRunner`,
  `AiApiPollSummary`.
- `../tests/live_compatible.rs`: opt-in live check of both compatible services against a real
  server (`MS_AI_API_LIVE_URL`; skips when unset): model list, text chat, image chat, empty key.
- `../tests/live_image_edit.rs`: opt-in live image edit through `image_edit::run_image_edit`
  (`MS_IMAGE_EDIT_LIVE_{PROVIDER,KEY,MODEL}`, optional base URL / region / size / k; skips when
  unset; key from the environment only, never the credential store): checks the exact-size
  guarantee end to end. A real run is paid.

## Contracts and invariants
- **Keys live only in the OS credential store**, never in a project or user settings file.
  Entry = (`KEYRING_SERVICE` = `"ManhwaStudio AI API OCR"`, user name). The user name of a hosted
  service is `AiApiService::key()`; of a compatible service `"{key()}@{normalized base URL}"`
  (e.g. `openai_compatible@http://127.0.0.1:8080/v1/`), so a stored key is only ever sent to
  the exact server it was saved for and a URL without its own key reads as "no key". Both forms
  are persistence contracts shared by OCR and MT (the "OCR" in the name is historical). A key
  bound to no target (an image-edit provider) lives in a named slot of the same service, user
  name `"image_edit:{provider_key}"` or `"image_edit:{provider_key}@{region}"` (region-bound
  keys), built only by `NamedKeyUser` — also a frozen persistence contract; the `:` keeps it
  disjoint from every target user name. A key is
  never logged, never put in an error, never put in a URL (OpenRouter gets it in the
  `Authorization` header only). `read_api_key` returns `Ok("")` when no key is stored. Key
  requests and events carry the `AiApiTarget`; storing or clearing needs a valid target.
- **`AiApiService::key()` values and `from_key` aliases are persisted** in the title
  `settings.json` by consumers; never change a value.
- **Model-id prefixes are persisted**: OpenRouter and Groq UI ids carry `open_router::` /
  `groq::`; `model_iden` strips any `prefix::` and binds the adapter explicitly.
- **The model list is every model the provider lists** (not filtered by capability), sorted,
  de-duplicated, and falls back to a non-empty `default_model()`; compatible services have no
  default model. Capability gating is the consumer's, via `image_input_support`.
- **A refresh never replaces a non-empty model** (hand-typed, saved or retired ids are kept even
  when unlisted). An empty model is filled with the listed service default, else the first
  listed model that is not `is_non_chat_model`, else the first listed (`model_changed`).
- **Image input is tri-state**: `Supported` / `NotSupported` only for models in the
  `model_caps` table (exact ids or documented family prefixes, first match wins, matched after
  lowercasing and dropping the `prefix::` namespace, the vendor path up to the last `/` and a
  `:variant` suffix); non-chat markers (embedding, tts, whisper, ...) are `NotSupported` and win
  over every family entry; everything else is `Unknown`. No substring guessing on family names.
  Consumers block image features only for `NotSupported`.
- **Compatible services** (`uses_base_url()`): requests and the model listing go to the
  normalized base URL (`http(s)://host[:port][/path]`, scheme and host lowercased, IPv6 literals
  in brackets, no userinfo, `\` / `?` / `#` / whitespace rejected, `/v1/` appended to a bare host,
  always ending in `/`); a missing or malformed URL is `BaseUrlMissing` / `BaseUrlInvalid`, never a silent
  fallback to an official endpoint. A key is optional (`requires_key() == false`): without one
  the model listing sends no auth header and chat sends an empty key. `build_client` always
  answers auth for its adapter, so `genai` never reads an `*_API_KEY` environment variable.
- **An empty model id is `EmptyModel`** from `model_iden`, never a request.
- **Blocking calls never run on the GUI thread**: key-store I/O, `load_metadata`,
  `fetch_account_status` and `block_on`. `block_on` builds a fresh multi-thread tokio runtime
  per call and must not be called from inside a tokio runtime.
- **Errors**: every fallible fn returns `AiApiError`; `to_string()` is the exact localized
  text the UI shows. Failures are logged here (`runtime_log`, service + error text) at the
  point they are created; consumers may add their own run-context logs.
- **Web build**: `genai`, `tokio`, `keyring`, `ureq` are native-only (`serde_json`, `image` and
  `ms-raster` are target-neutral; the `image_edit` core compiles on wasm). On wasm
  the key functions return `KeyStoreWebUnavailable` and `load_metadata` returns
  `MetadataWebUnavailable`; `client`, `openrouter` and `model_iden` do not exist.
- **Connection widget ids**: `draw_connection` draws straight into the caller's `Ui` — no
  `push_id`, `vertical`, `ScrollArea` or width clamp — and names its two combos
  `"{id_salt}_service"` / `"{id_salt}_model"` and the base URL field `"{id_salt}_base_url"`
  (string salts, `ui.id()`-scoped). Consumers'
  salts are widget-state keys: never change them. Scrolling and width are the caller's.
  `draw_key_block` uses the caller's auto ids (no `push_id`), which is what keeps
  `draw_connection`'s id sequence unchanged; a caller with several blocks in one `Ui` scopes each.
- **The consumer owns persistence** of `service` (as `AiApiService::key()`), `base_url` (as
  typed), `model` and `system_instruction` under its own settings keys; the other state fields are transient and the
  key buffer is never persisted or logged (`Debug` redacts it).
- **Runner threading**: `AiApiTaskRunner` lives on the GUI thread (`!Sync`). Each runner owns
  ONE detached `ms_thread` worker, spawned on the first `submit` and fed through a channel, that
  executes requests strictly in submission order, so events arrive in that order (a stale
  refresh never overwrites a newer one; save-then-delete reaches the credential store in that
  order). The worker exits once the runner is dropped, after finishing what was already queued;
  nobody joins it. `submit` never blocks; `poll` is a non-blocking drain. A spawn failure is
  logged and surfaces as a `Failed { TaskSpawn }` event; a worker that died (a panicking request)
  is logged and replaced on the next `submit`. Runners are independent of each other and of the
  consumers' own run workers. Events for a service that is no longer selected (or key events
  and metadata for a base URL that is no longer current) change no state, but "key saved" / "request failed"
  notices are still produced.
- **Not localized on purpose**: hosted provider labels (brand names; the two compatible labels
  ARE localized), the `"OpenRouter: "` status prefix and the `"{requests} req/{interval}"`
  rate-limit part.

## Editing map
- Add a provider: `service.rs` (every match), then `model_id.rs` if its ids need a prefix.
- Add or correct a model capability: `model_caps.rs` (keep carve-outs before their family
  prefix; add a test per group).
- Change base-URL rules: `target.rs`; how the URL reaches `genai`: `client.rs`, `metadata.rs`.
- Change key storage: `keys.rs` (keep `KEYRING_SERVICE`, both target user-name forms and the
  named `image_edit:` forms).
- Change the model list or account status: `metadata.rs`, `openrouter.rs`.
- Change the connection widget layout: `connection_view.rs` (the key block is
  `draw_key_block`, shared with other key owners); its state transitions and status
  texts: `connection.rs`; how requests run: `tasks.rs`.
- Add an error: `error.rs` variant + `ai_api.*` key in `crates/ms-i18n/locales/en.json` and
  `ru.json`.
- Cloud image editing (providers, models, size rules, pipeline, adapters, executor, picker and
  key block): `image_edit/`.
