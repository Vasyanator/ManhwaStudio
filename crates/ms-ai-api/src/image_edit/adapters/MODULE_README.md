# Module: crates/ms-ai-api/src/image_edit/adapters

## Purpose
One pure adapter per provider API SHAPE (`provider::ApiShape`): it turns a prepared
`protocol::EditCall` into the HTTP request description of that shape and each HTTP response
into the `NextStep` (poll, download, final image) or a typed `ImageEditError`. Providers are
data (`provider.rs`); every provider of a shape shares its adapter.

## Architecture
```text
pipeline::run_image_edit -> protocol_for(provider.info().shape) -> &dyn EditProtocol
  executor: submit(call) -> HTTP -> next(step, response) -> Poll | Download | Image
```
Adapters perform no I/O, never see the API key (they name an `AuthScheme`) and keep no state
between steps (the executor hands back the `JobRef` they returned). Target-neutral.

## Files and submodules
- `mod.rs`: `protocol_for` (shape -> adapter, exhaustive), shared helpers: `classify_error` (status + body
  markers -> typed error), `provider_message`, `images_data_step` (`data[0].b64_json` | signed
  `data[0].url`), `json_value`, `decode_base64_image`, `png_data_url`; `test_support` (canned
  calls) for the adapter tests.
- `openai_images.rs`: `POST {base}/images/edits` multipart (`OpenAI`, DeepInfra, aimlapi,
  AITunnel, ProxyAPI, the user's own server).
- `openrouter_images.rs`: `POST {base}/images` JSON with `input_references` (`OpenRouter`,
  RouterAI).
- `gemini_generate.rs`: `models/{id}:generateContent` with `imageConfig`, thought images skipped.
- `ark_images.rs`: `BytePlus` `ModelArk` `POST {base}/images/generations` (Seedream), data-URL
  `image`, `size` "WxH", inline `b64_json`.
- `together_images.rs`: Together `POST {base}/images/generations` with `reference_images`,
  `width` / `height`, inline base64.
- `xai_images.rs`: xAI `POST {base}/images/edits` as JSON (`image.url` data URL), inline
  `b64_json`, no size field.
- `recraft_edit.rs`: Recraft `POST {base}/images/inpaint` (V3 only) with a required white = edit
  mask, inline `b64_json`.
- `dashscope_multimodal.rs`: Alibaba Model Studio multimodal generation (Qwen-Image edit / 2.0 /
  3.0, Wan 2.7) with `size` "W*H"; per-family `parameters`; result URL downloaded.
- `tencent_tokenhub.rs`: `TokenHub` Hunyuan image, two protocols keyed on the model id (v3
  classic, v3.5 Chat/Messages); result URL downloaded.
- Submit-then-poll shapes (each file header states its request, statuses and sources):
  - `bfl_async.rs`: BFL `POST /v1/{model}` -> poll the returned regional `polling_url` (the
    only foreign host that receives the key, via `auth_host_suffixes`) -> `result.sample`.
  - `fal_queue.rs`: fal queue submit -> `status_url` -> `response_url` -> `images[0].url`;
    cancel `PUT cancel_url`.
  - `replicate_predictions.rs`: official-model predictions -> `urls.get` -> `output`; cancel
    `POST urls.cancel`.
  - `ideogram_edit.rs`: precise-edit multipart with `async=true` -> `/v2/generations/{id}`.
  - `runware_tasks.rs`: one `imageInference` task (synchronous delivery; `getResponse` follows
    a `processing` answer); mints the task UUID.
  - `polza_media.rs`: `/v1/media` with `async: true` -> `/v1/media/{id}` -> `data.url`.
  - `genapi_async.rs`: `/networks/{id}` -> `/request/get/{id}` -> `result[0]`.
  - `luma_generations.rs`: Luma Agents `type: image_edit` -> `/generations/{id}` -> `output[0]`.
  - `runway_tasks.rs`: `/text_to_image` with a data-URI reference -> `/tasks/{id}` ->
    `output[0]`; cancel `DELETE /tasks/{id}`.
  - `kling_image.rs`: Kling `/v1/images/omni-image` (plain API key, plain base64) ->
    `/v1/images/omni-image/{id}` -> `task_result.images[0].url`; no cancel.

## Verification status
- Confirmed from official reference pages: BFL, fal, Replicate, Ideogram, Runware, Polza, Luma
  (Agents API), Runway, `ModelArk`, xAI, Recraft, `DashScope`, Tencent `TokenHub`, Kling, Together.
- Request signing is not needed anywhere: Tencent `TokenHub` and Kling take a plain Bearer API
  key (Kling's AK/SK JWT is its legacy scheme).
- UNVERIFIED input encodings: Together `reference_images` as a data URL (documented as URLs);
  Tencent v3 `images` as plain base64 (documented as "Base64" without a format); BFL FLUX.2
  `input_image` as raw base64 (the schema says "Path to the input image.", the samples send
  URLs; Kontext, Fill and FLUX 3 document base64).
- `genapi_async.rs` is UNVERIFIED in detail: GenAPI's docs render samples client-side, so the
  endpoints come from the official PHP SDK and the site's own client code, and whether its
  file parameters accept data URLs is unknown.
- Data-URI inputs are documented for Replicate, Runware, Polza (images), Runway and Luma; fal
  documents them for "some models" only; Polza's `mask_url` as a data URI is unverified.
- `runway_tasks.rs`: `ratio` is a fixed list of resolutions; the catalogue row's rule is that
  table (`RUNWAY_GEN4`), so only accepted sizes reach the adapter.

## Contracts and invariants
- **Documented shapes only**: every request and parser is transcribed from the provider's docs
  (URL in the file header, fetched date) and pinned by a test against that example.
- **Body markers win over status** in `classify_error`: region blocks (`OpenAI` 403
  `unsupported_country_region_territory`, Gemini 400 "User location is not supported") are
  `RegionBlocked`; moderation and exhausted-balance markers likewise; then 401/403 key, 402
  credits, 429 rate limit, other 4xx `ProviderRejected` (provider text, truncated to 500 chars),
  the rest `ProviderFailed`.
- **Result URLs get no auth** (`auth: None`): they are signed CDN links (`result_url_step`;
  an inline `data:` URL is decoded instead). Requests to the API name `auth: Some(..)`; the
  executor refuses it for any origin but the call's base URL and the provider's trusted
  suffixes (BFL). Replicate keys a result URL only when it is on the API base itself.
- **Async state lives in `JobRef`**: adapters stay stateless; a provider-issued status URL
  that cannot be derived (BFL `polling_url`, fal `status_url`, Replicate `urls.get`) is kept
  in `JobRef::poll_url` and its cancel URL in `cancel_url`. `NextStep::Poll { stage }`
  distinguishes the requests of one shape (fal: 0 = status, 1 = result fetch).
- **Moderation is typed**: HTTP moderation markers and each shape's moderation statuses
  (BFL `Request/Content Moderated`, Luma `content_moderated`, Runway `SAFETY.*`, Ideogram
  `is_image_safe: false` / 422, `DashScope` `DataInspectionFailed`, `ModelArk`
  `*SensitiveContentDetected*`, Tencent 422 / `content_filter`, Kling 1300 / 1301) are
  `Moderated`; other failed jobs go through `job_failure`.
- **Masks that need both regions** (Ideogram direct and on fal) are omitted when nothing is
  kept: an all-editable mask equals no mask there.
- **Size**: an adapter states `(call.width, call.height)` only in the style the offer's
  `SizeParamStyle` names and refuses (`RequestBuild`) a style its shape cannot express; it never
  changes the size.
- **Masks** are encoded here in the provider's polarity (`codec::MaskPolarity`), at the sent size.
- **Reference image** (`EditCall::reference_png`): appended to the shape's list field right
  after the edited image (fal / GenAPI `image_urls`, `OpenRouter` `input_references`, Together
  `reference_images`, Replicate `input_images` / `image`, Polza `images`, Kling `image_list`,
  Runway `referenceImages`, Tencent `images` / content, BFL FLUX 3 `images`, Gemini `parts`,
  `DashScope` content, Runware `referenceImages`, `OpenAI` / own server a second `image[]`).
  An endpoint with a single image field returns `ReferenceNotSupported` (`refuse_reference`).

## Editing map
- Add a shape: a new file implementing `EditProtocol`, its arm in `protocol_for`, documented
  request/response tests plus one `test_support::run_scripted` run through the real executor,
  this readme.
- Change error mapping for every shape: `classify_error` markers in `mod.rs` (+ tests).
- Change one provider's request detail inside a shared shape: that adapter, keyed on
  `call.provider` with an exhaustive match.
