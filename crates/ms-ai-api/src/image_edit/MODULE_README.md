# Module: crates/ms-ai-api/src/image_edit

## Purpose
The cloud image-edit layer: send a region of a page (plus a prompt and an optional mask) to a
hosted image model, or to the user's own OpenAI-compatible images server, and get back EXACTLY
the region's size, changed only inside the mask. Consumer: the cleaning tab's API editing tool
(through the region-edit host). It owns the provider / model catalogue, the size-rule data, the
size-exact pipeline and the HTTP request descriptions the adapters produce.

## Architecture
```text
caller: k = cleaning geometry upscale_factor_for(W, H, frame_constraints(offer.rule))
  -> pipeline::prepare(request, offer)      validate, upscale_replicate xk, RGB PNG, native mask,
                                            size labels, endpoint -> PreparedCall { EditCall }
  -> adapter (EditProtocol, per ApiShape)   EditCall -> HttpRequestSpec; response -> NextStep
  -> native executor                        HTTP, polling, cancel, download, key injection
  -> pipeline::finish(request, prepared, bytes)
                                            bounded decode; size != (kW, kH) -> SizeMismatch;
                                            downscale_box /k; composite_feathered; alpha 255; W x H
```
`pipeline::run_image_edit` is the whole run on a worker thread: `KeyMissing` check, `prepare`,
`adapters::protocol_for(shape)`, `executor::execute`, a cancel check (an answer that arrived
after a cancel is dropped), `finish`; one `runtime_log` line per run. Everything in this
directory is target-neutral except `executor.rs` (native only; `run_image_edit` is a
`WebUnavailable` stub on wasm); the network lives only in the executor, the credential store
only behind `keys.rs` / `key_state.rs`.

## Files and submodules
- `mod.rs`: the public surface (re-exports).
- `provider.rs`: `ImageEditProvider` (frozen `key()`), `ProviderInfo` (label, `ApiShape`,
  `EndpointKind` with `EndpointRegion`s, `ProviderKeySlot`, `RussiaAccess`, docs URL),
  `named_key_user`, `RussiaStatus` / `RussiaNote` localized texts.
- `size_rule.rs`: `ImageSizeRule` data (1:1 with the cleaning frame's `FrameConstraints`),
  `SizeEvidence`, `AspectTierEntry` tables (Gemini), the named rules.
- `catalog.rs`: `ModelOffer` table (`offers`, `all_offers`, `lookup`), `MaskSupport`,
  `SizeParamStyle`, retirement dates, the deliberate-exclusion list (file header).
- `request.rs`: `RgbaRegion` (validated), `ImageEditRequest`, `EndpointChoice`, `MaskBlend`,
  `ImageEditOutcome`, `ImageEditStage`, `CancelFlag`.
- `codec.rs`: RGB PNG encode, native-mask PNG per `MaskPolarity`, bounded decode.
- `composite.rs`: `composite_feathered`.
- `pipeline.rs`: `prepare` / `finish`, endpoint resolution, `run_image_edit` (native run over
  the executor; `run_with` takes the transport so tests script it).
- `executor.rs` (native, crate-private): `execute` drives the `EditProtocol` steps over the
  `HttpTransport` seam (`UreqTransport`); `ExecutorLimits` (timeouts, run deadline, poll
  schedule, retry backoff, body caps) and the pure policy `retry_allowed`, `auth_allowed`,
  `poll_delay`, `retry_delay`, `log_body`.
- `keys.rs`: `ImageEditKeySlot` (`Chat(AiApiTarget)` | `Named { user, label }`), `key_slot`
  (provider + `EndpointChoice` -> slot), the one dispatch `ImageEditKeySlot::{read, store,
  clear}` onto `crate::keys`, and `read_key` / `store_key` / `clear_key`.
- `adapters/`: one pure `EditProtocol` per `ApiShape` plus `protocol_for`; see its
  `MODULE_README.md`.
- `protocol.rs`: `EditProtocol`, `EditCall`, `HttpRequestSpec`, `HttpResponse`, `NextStep`,
  `StepCtx`, `JobRef` (job id plus the provider-issued poll / cancel URLs the adapters reuse),
  `AuthScheme`.
- `multipart.rs`: `MultipartForm` -> `MultipartBody`.
- `error.rs`: `ImageEditError` (localized `Display`, `Clone`).
- `key_state.rs`: `ImageEditKeyState` (the key block's state; pure single-flight `select_slot`
  / `begin` / `apply`), `ImageEditKeyRunner` (runs the one in-flight check / store / delete on
  an `ms_thread` worker; `pump` per frame), the `ImageEditKeyStore` seam and its impl for
  `keys::ImageEditKeySlot`.
- `view.rs`: `draw_image_edit_picker` (provider `SearchableComboBox` + Russia badge + docs link, region
  combo or base-URL field, the shared `draw_key_block`, model combo + free id, size-evidence /
  mask / shutdown notes) over `ImageEditSelection` (per-provider `ProviderChoice`);
  `russia_badge` (pure).

## Contracts and invariants
- **One owner of the size decision.** Validity, the upscale factor `k` and snapping belong to
  the cleaning frame's geometry; this module only carries the rule DATA and never evaluates
  it. `prepare` checks only `k in 1..=rule.max_upscale` and, for table-labelled offers, that
  `(kW, kH)` is a table entry (to pick its labels).
- **No resampling except the integer pair.** Upscale = `ms_raster::upscale_replicate`, back =
  `ms_raster::downscale_box`; `box(replicate(x)) == x`. A decoded size other than exactly
  `(kW, kH)` is `SizeMismatch`, never resized. `finish` returns exactly `W x H`, alpha 255
  (`SizeContractViolated` guards it).
- **Composite.** alpha = `box_blur_u8(dilate_square(mask, dilate), feather)`; pixels farther
  than `dilate + feather` from paint are bit-identical to the source; an empty mask (`None` or
  all zero) means the whole region. Applied for every provider, mask-capable ones included.
- **Sent image** is RGB PNG (alpha dropped: `OpenAI` reads image alpha as a mask). The native
  mask (`EditCall::mask`, 255 = editable) goes only to `Soft` / `Hard` offers when something
  is painted; `HardRequired` offers get a full mask when nothing is. Polarity is the adapter's
  (`codec::MaskPolarity`: BFL / fal / Runware / Recraft white = edit, Ideogram black = edit,
  `OpenAI` alpha 0 = edit).
- **Decode bounds**: 16384 px per side, 512 MiB allocations.
- **Persistence contracts**: `ImageEditProvider::key()`, region ids, model ids and the named
  key user names `image_edit:{key}` / `image_edit:{key}@{region}` (built by `NamedKeyUser`)
  are persisted; never change one. OpenAI, Gemini, OpenRouter and xAI share the chat key of
  their `AiApiService`; the user's own server shares the per-URL `openai_compatible` key.
- **Adapters never see a key**: they name an `AuthScheme`; the executor attaches the key only
  to the exact origin (scheme, host, port) of the call's resolved base URL, or over https:443
  to a host the provider's data trusts (`ImageEditProvider::auth_host_suffixes`, dot boundary;
  only BFL, whose regional `polling_url` must be used as returned). A request asking for it
  elsewhere is a `RequestBuild` error. It sends no auth header for an empty key (the user's
  own server), never follows redirects on keyed requests, and gives signed download URLs no
  auth.
- **Executor policy**: a POST (the paid submit) is NEVER retried; a GET (poll, download) is
  retried on IO failure or 5xx with 1 / 2 / 4 s backoff. Timeouts: connect 15 s, submit 5 min,
  poll 30 s, download 2 min, whole run 15 min; poll delay `max(adapter's, 1 s + 0.5 s per poll,
  capped at 3 s)`. Caps: API body 128 MiB, downloaded image 64 MiB (`DownloadTooLarge`).
  Cancel is checked between steps and every 100 ms of a wait; on a cancel AND on the run
  deadline the adapter's cancel request is sent best effort (a submit answer that arrived after
  the cancel is still parsed for its job). A failing body reaches `runtime_log` only as the
  provider's error message (`failing_body_summary`: bodies may echo the prompt); the 2 KB
  digest, base64 runs elided, goes to the trace log.
- **Logging**: never a key; the prompt only by length in `runtime_log`, its text only in the
  opt-in trace log (category `IMAGE_EDIT`).
- **Russia status is per provider** (as of 2026-10-04), a reseller has its own; the user's
  own server has none (no badge).
- **Russia badge** only when the UI language subtag is `ru` (`russia_badge`): Works =
  `status::SUCCESS`, payment issues = `WARNING`, blocked = `ERROR`; tooltip = `RussiaNote` +
  the as-of date.
- **Key block is single-flight**: at most one credential-store operation per
  `ImageEditKeyState` (priority save > delete > check), the block is disabled meanwhile, so a
  save then a delete reach the store in that order. A result for a slot no longer selected
  changes no state (its toast still shows). The plaintext key is redacted from every `Debug`.
- **The picker never does I/O**: it returns `ImageEditPickerActions`; the consumer re-resolves
  the slot on `key_slot_changed` and feeds `key` to `ImageEditKeyRunner::pump`. Default fill
  (`fill_defaults`) only fills an EMPTY model id / region; an unknown stored region is an
  `InvalidEndpoint`, never swapped for another. Widget ids derive from the caller's `id_salt`.
- **Errors**: every fallible function returns `ImageEditError`; `to_string()` is the UI text.
  No error carries a key or the prompt.

## Editing map
- Add a provider: `provider.rs` (enum, `ALL`, `key`, `info`), its offers in `catalog.rs`, its
  adapter for the `ApiShape`; i18n only if a label is localized.
- Add or correct a model / size rule: `catalog.rs` row (+ `size_rule.rs` constant with its
  source); keep `SizeEvidence` honest; update the pinned tests.
- Change the size pipeline: `pipeline.rs` (with `codec.rs`, `composite.rs`).
- Add an API shape: `adapters/` (file + `protocol_for` arm); HTTP behaviour (timeouts,
  retries, auth host rule, caps): `executor.rs`; a provider host that may receive the key
  besides its base URL: `provider.rs` `auth_host_suffixes`; where a key lives: `keys.rs`.
- Opt-in live check of a provider: `crates/ms-ai-api/tests/live_image_edit.rs`.
- Change the request contract for adapters: `protocol.rs`, `multipart.rs`.
- Add an error: `error.rs` variant + `ai_api.image_edit.error.*` key in
  `crates/ms-i18n/locales/en.json` and `ru.json`.
- Change the picker's layout or notes: `view.rs` (texts `ai_api.image_edit.{provider,endpoint,
  model,evidence,mask,russia}.*`); the key block's flow: `key_state.rs` (`ai_api.image_edit.key.*`).
