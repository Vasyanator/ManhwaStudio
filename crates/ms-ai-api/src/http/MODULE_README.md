# Module: crates/ms-ai-api/src/http

## Purpose
Crate-private HTTP plumbing shared by the hand-rolled REST clients of `ms-ai-api` (today the
cloud image-edit executor in `image_edit/` and the NeuroLate client in `neurolate/`). It describes requests, encodes multipart forms and
executes one request over blocking `ureq`; it decides no caller policy.

## Architecture
```text
pure builder (adapter / protocol module) -> HttpRequestSpec (+ AuthScheme, never the key)
caller (executor / client)                -> vets the key's host with auth_allowed, picks
                                             timeout + cap + stage (SendOptions)
HttpTransport::send                       -> HttpResponse (any status) | HttpError
caller                                    -> maps HttpError onto its own domain error,
                                             retries only when retry_allowed(method)
```
`mod.rs` is `pub(crate)` in `lib.rs`; its items are `pub` so a public module may re-export a
type that is part of its own contract (`image_edit::protocol`, `image_edit::multipart`).

## Files and submodules
- `mod.rs`: module wiring and re-exports.
- `request.rs`: `HttpMethod`, `AuthScheme`, `HttpBody`, `HttpRequestSpec`, `HttpResponse`
  (target-neutral, pure).
- `multipart.rs`: `MultipartForm` -> `MultipartBody` (RFC 7578; boundary chosen so it occurs in
  no part; header-unsafe names refused) and its tests.
- `error.rs`: `HttpError` (`RequestBuild`, `Network`, `Timeout { stage }`,
  `BodyTooLarge { limit_bytes }`); technical `Display`, never a user message.
- `transport.rs` (native only): `HttpTransport` (the test seam), `UreqTransport::new(connect)`,
  `SendOptions`, `retry_allowed`, `auth_allowed`, `log_body`, with the origin / digest tests.

## Contracts and invariants
- A non-2xx status is an `Ok(HttpResponse)`; only transport failures are `HttpError`.
- The body is read up to `cap + 1` bytes; more than `cap` is `BodyTooLarge`. A timed-out socket
  (connect or read) is `Timeout { stage }`, every other transport / IO failure `Network`.
- Keyed requests never follow redirects (a key header would travel to the target);
  unauthenticated requests do. The key arrives only through `send`'s `auth` argument, already
  vetted by the caller with `auth_allowed` (exact origin of the base URL, or https:443 on a
  dot-boundary trusted suffix).
- `retry_allowed`: only GET may be retried; a POST is a paid request and is never re-sent.
- Nothing here logs, holds a key, sleeps or retries; `log_body` only builds a digest (base64
  runs elided, 2048-character cap) for the caller's log line.
- Each domain error converts from `HttpError` 1:1 (`From<HttpError>`); keep those maps total
  when a variant is added.

## Editing map
- Change how a request goes on the wire (agents, header forms, body encodings, caps, timeout
  mapping): `transport.rs`; then every `From<HttpError>` if an error variant changes.
- Change which hosts may receive a key: `auth_allowed` in `transport.rs` (callers pass their own
  base URL and trusted suffixes).
- Add a request field or body kind: `request.rs` + `transport.rs` `send`.
- Change multipart encoding: `multipart.rs`.
