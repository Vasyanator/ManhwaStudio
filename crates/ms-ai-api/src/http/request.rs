/*
File: crates/ms-ai-api/src/http/request.rs

Purpose:
Pure descriptions of one HTTP request and its response, the contract between a pure request
builder (an image-edit adapter, a REST client's protocol module) and the one transport that
executes it (`http::transport`). A description never carries the API key: it only names the
`AuthScheme`, and the caller hands the vetted key to the transport separately.

Key structures:
- HttpMethod, AuthScheme, HttpBody, HttpRequestSpec, HttpResponse

Notes:
Target-neutral and network-free, so request builders are unit-tested against documented
request and response examples.
*/

use super::multipart::MultipartBody;

/// HTTP method of a request description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    /// Only for best-effort cancel requests (Runway `DELETE /v1/tasks/{id}`).
    Delete,
}

/// How the transport attaches the API key to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    /// `Authorization: Bearer {key}`.
    Bearer,
    /// `{header}: {key}` (e.g. `x-key`, `x-goog-api-key`, `Api-Key`).
    Header(&'static str),
    /// `Authorization: {prefix} {key}` (e.g. fal's `Key`).
    KeyPrefix(&'static str),
}

/// A request body.
#[derive(Debug, Clone, PartialEq)]
pub enum HttpBody {
    /// No body.
    Empty,
    /// `application/json`.
    Json(serde_json::Value),
    /// `multipart/form-data` (its content type carries the boundary).
    Multipart(MultipartBody),
}

/// One HTTP request, described by a pure builder and executed by the transport.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequestSpec {
    pub method: HttpMethod,
    /// Absolute https URL (http only for the user's own server).
    pub url: String,
    /// Extra headers (never the key; never `Authorization`).
    pub headers: Vec<(&'static str, String)>,
    pub body: HttpBody,
    /// How to attach the key; `None` for unauthenticated requests (signed download URLs).
    pub auth: Option<AuthScheme>,
}

/// An HTTP response as the transport hands it back (body already size-capped). Any status,
/// error statuses included, is a response; only transport failures are errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// The `Content-Type` header, if any.
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}
