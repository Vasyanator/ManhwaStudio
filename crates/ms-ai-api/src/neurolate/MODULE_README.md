# Module: crates/ms-ai-api/src/neurolate

## Purpose
The client of the NeuroLate cloud OCR / text-detection API (`https://api.neurolate.com`): the
key slot, the typed request / response model, the paid OCR call, the free key / limit call, and
the key + limit panel (GUI-free state and its egui view). It decides nothing about pages,
masks or bubbles: consumers (the translation tab's OCR engine and detector mode) build an
`OcrRequest`, run `recognize` on their worker and convert the typed answer themselves (polygons
to masks happen in `ms-text-detect`, which this crate must not depend on).

## Architecture
```text
consumer worker: read_key() -> recognize(key, OcrRequest, abandon)
  protocol::ocr_form            -> multipart: mode, language, type (each once), images...
  client: vet origin (auth_allowed) -> detached ms_thread: HttpTransport::send (ONE POST)
          caller waits in 100 ms slices; abandon() -> Err(Cancelled) at once
          detached thread: parse_ocr_response -> log (request id, blocks, cost) -> send result
                           receiver gone -> log "result discarded" with the cost
  -> Ok(OcrResponse) | Err(NeurolateError)

GUI: NeurolateKeyPanel (one per screen)
  draw_neurolate_key_panel(ui, id_salt, max_width, &mut panel) -> NeurolateKeyActions
  panel.pump(actions)  -> ImageEditKeyRunner (key check / save / delete, single flight)
                       -> key op answered "stored" or "Check limit" -> limit worker:
                          read_key + fetch_key_info (GET /ext/v1/key, retried)
                       -> Vec<AiApiNotice> (toasts)
```

## Files and submodules
- `mod.rs`: wiring, re-exports, `LABEL`, `key_slot()`, `read_key()`.
- `protocol.rs`: `OcrMode`, `NeurolateLanguage`, `NeurolateComicType`, `OcrParams`,
  `OcrRequest`, `OcrResponse` / `OcrImageResult` / `OcrBlock` (`has_usable_mask`) /
  `OcrPolygon` / `OcrBox` / `TextStatus` / `JobError`, `KeyInfo` / `LimitPeriod` /
  `KeyStatus`, `BASE_URL`, `ocr_form`, `parse_ocr_response`, `parse_key_info`, `status_error`;
  fixture-driven tests.
- `error.rs`: `NeurolateError` (localized `Display`, `From<HttpError>`, `From<AiApiError>`).
- `client.rs`: native `recognize` / `fetch_key_info` and their transport seams
  (`recognize_with`, `fetch_key_info_with`, `NeurolateLimits`, `PaidFlights`); wasm stubs; fake-transport
  tests.
- `account.rs`: `NeurolateKeyPanel`, `NeurolateKeyActions`.
- `view.rs`: `draw_neurolate_key_panel`.
- `../../fixtures/neurolate/*.json`: sanitized live answers (zero request ids, fake key prefix
  and name, no images) read by the tests through `include_str!`.

## Contracts and invariants
- **Key slot**: the crate's credential-store service, named user `"neurolate:api"`
  (`NamedKeyUser::neurolate`), a frozen persistence contract guarded by tests. The key is
  driven through the image-edit key machinery unchanged; it is never logged, put in an error or
  a URL, and goes only to `BASE_URL`'s origin as `Authorization: Bearer`.
- **Wire**: every OCR parameter is a multipart FORM field, sent exactly once (`kind` only when
  the request sets it; `NeurolateLanguage` = the API's `Language` enum without `UND`, 58
  2-letter codes); the query string is never used (an ~8 KB request-line limit; a field sent twice is comma-joined and rejected).
  `regions` / `readingDirection` are not sent today (additive later). A request without images
  is refused locally: the server answers a missing field with 401, which would read as a bad key.
- **Server rules the consumers rely on** (current spec): without `kind` an image is a BLOCK
  (one crop, one block) when both sides are at most 690 px, otherwise a PAGE (an explicit
  `kind=BLOCK` may be up to 1,000,000 px; not sent today). `TEXT` on a PAGE runs as
  `MASK_AND_TEXT` and is charged as such (`mode_applied`). Masks a client sends to the server
  have no holes (only `outer`); answers may still carry `exclude` holes.
- **Paid POST is never retried**, whatever failed, and carries a fresh v4 `Idempotency-Key`
  per `recognize` call (logged with the request id; not a secret): should the same request
  ever reach the server twice, it answers 409 `DUPLICATE_REQUEST` ->
  `DuplicateRequest { first_request_id }` instead of charging again. The free `GET /key` retries network
  failures, timeouts and 5xx after 1 / 2 / 4 s.
- **Abandon (C4)**: `recognize` returns `Cancelled` within ~100 ms of `abandon()` turning true;
  the detached request is not aborted (blocking `ureq`), may still be charged, and logs its
  request id and cost with "result discarded". Consumers that join their workers on teardown
  pass their cancel flag as `abandon`.
- **Bounded paid flights**: at most 2 paid requests are out process-wide, an abandoned one
  counting until its detached send returns (slot freed by an RAII guard, also on panic); a
  `recognize` over the cap sends nothing and fails with `TooManyInFlight`. Abandon never waits.
- **Answer integrity**: `parse_ocr_response(status, body, expected_images)` requires exactly
  one result per sent image (indices `0..n`, each once) and returns them ordered by index;
  `cost` is `None` (logged "unknown") unless `usage.cost` is a finite non-negative number;
  `/key` quota amounts must be finite and non-negative, else `Decode`.
- **Tolerant parsing**: null `mask` / `bbox` / `text`, unknown enum values (`Unknown(code)` /
  `None`) and per-image or per-block `error`s are data, never errors; malformed geometry or a
  missing `id` / `images` is `Decode`. `has_usable_mask` is false for `EMPTY` / `FAILED` /
  unknown region statuses (an EMPTY region returns its own rectangle as the mask) and for
  blocks with an error or no polygon.
- **Renamed wire fields** are read under both names, current first: `requestId` / `id`,
  `maskIndex` / `region` (the Rust field stays `OcrBlock::region`). `ocr_page_request_id.json`
  is the current-format fixture; the older fixtures cover the fallback.
- **Statuses**: 400 `InvalidRequest{code,message,index}`, 401/403 `KeyRejected` (logged with
  the note that 401 also means a missing form field), 402 `OutOfCredits{code}`, 413
  `TooLarge`, 409 `DUPLICATE_REQUEST` `DuplicateRequest` (other 409s `Http`), 502 `ServiceFailed` (nothing charged), other `Http`.
- **Logs**: one info line per OCR call (mode, image count, request id, blocks per image,
  cost, duration); failing bodies through `http::log_body`. Never the key, never image bytes.
- **Panel threading**: `NeurolateKeyPanel` lives on the GUI thread; `pump` never blocks. The
  limit fetch is single flight; key changes bump an epoch so an answer for an older key is
  dropped; a delete clears the limit. The view disables the key block while a key operation
  runs and scopes it with `push_id("{id_salt}_key")`.
- **Web build**: `recognize` / `fetch_key_info` answer `WebUnavailable`; the rest compiles.

## Editing map
- New request field (regions, reading direction) or the `/ext/v1/clean` request: `protocol.rs`
  (types + form), then a `client.rs` function over the same detached-POST path.
- New status mapping or error: `protocol::status_error` + `error.rs` + `ai_api.neurolate.*`
  keys in `crates/ms-i18n/locales/en.json` and `ru.json`.
- Timeouts, caps, retry delays: `NeurolateLimits` in `client.rs`.
- Panel behaviour: `account.rs`; its look: `view.rs`.
