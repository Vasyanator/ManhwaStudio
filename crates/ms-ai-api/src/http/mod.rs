/*
File: crates/ms-ai-api/src/http/mod.rs

Purpose:
The crate-private HTTP plumbing shared by the crate's hand-rolled REST clients (today the
cloud image-edit executor and the NeuroLate client): pure request / response descriptions, the `multipart/form-data`
builder, the transport error and the one native blocking transport over `ureq` with its
key-origin rule and log-safe body digest. It owns no policy of any caller: timeouts, body caps,
retries, polling and which hosts may receive a key are decided per call by the caller.

Modules:
- `request`   : `HttpMethod`, `AuthScheme`, `HttpBody`, `HttpRequestSpec`, `HttpResponse`.
- `multipart` : `MultipartForm` -> `MultipartBody` (RFC 7578, collision-free boundary).
- `error`     : `HttpError`, the transport / request-build failure (technical `Display`).
- `transport` : `HttpTransport` (the seam tests fake), `UreqTransport`, `SendOptions`,
                `auth_allowed`, `retry_allowed`, `log_body` (native only).

Notes:
Everything here is target-neutral except `transport` (native only). The module is
`pub(crate)`: its items are `pub` so a public module of the crate may re-export a type that
is part of its own contract (`image_edit::protocol` re-exports the request types,
`image_edit::multipart` the form builder). A domain error type converts from `HttpError`
(`impl From<HttpError> for ImageEditError`, `impl From<HttpError> for NeurolateError`).
*/

pub mod error;
pub mod multipart;
pub mod request;
#[cfg(not(target_arch = "wasm32"))]
pub mod transport;

pub use error::HttpError;
pub use multipart::{MultipartBody, MultipartForm};
pub use request::{AuthScheme, HttpBody, HttpMethod, HttpRequestSpec, HttpResponse};
#[cfg(not(target_arch = "wasm32"))]
pub use transport::{HttpTransport, SendOptions, UreqTransport, auth_allowed, log_body, retry_allowed};
