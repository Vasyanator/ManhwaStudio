/*
File: crates/ms-ai-api/src/image_edit/protocol.rs

Purpose:
The contract between the pure per-shape adapters and the one native HTTP executor. An adapter
turns a prepared `EditCall` into an `HttpRequestSpec` and each HTTP response into the
`NextStep`; the executor owns the network, timeouts, polling delays, cancellation, downloads
and the API key (an adapter only names the `AuthScheme`, it never sees the key).

Key structures:
- HttpMethod, AuthScheme, HttpBody, HttpRequestSpec, HttpResponse
- EditCall (what an adapter receives), StepPhase, JobRef, StepCtx, NextStep

Key traits:
- EditProtocol

Notes:
Target-neutral and network-free, so adapters are unit-tested against documented request and
response examples. The executor never retries a POST (a paid request); it attaches the key
only to the origin of the call's base URL or a host the provider's `auth_host_suffixes`
trusts (see `image_edit/MODULE_README.md`).
*/

use std::time::Duration;

use super::catalog::SizeParamStyle;
use super::error::ImageEditError;
use super::multipart::MultipartBody;
use super::provider::ImageEditProvider;
use super::size_rule::AspectTierEntry;

/// HTTP method of a request description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    /// Only for best-effort cancel requests (Runway `DELETE /v1/tasks/{id}`).
    Delete,
}

/// How the executor attaches the API key to a request.
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

/// One HTTP request, described by an adapter and executed by the executor.
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

/// An HTTP response as the executor hands it to an adapter (body already size-capped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// The `Content-Type` header, if any.
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Everything an adapter needs to build the submit request; produced by `pipeline::prepare`.
#[derive(Debug, Clone)]
pub struct EditCall {
    pub provider: ImageEditProvider,
    /// The model id the provider expects.
    pub model_id: String,
    /// The resolved API base URL (region or user URL applied; no trailing `/`).
    pub base_url: String,
    /// The frozen id of the selected region, for providers with a region list.
    pub region_id: Option<&'static str>,
    /// The edit instruction (non-blank).
    pub prompt: String,
    /// RGB PNG of the image to send, `width x height`.
    pub image_png: Vec<u8>,
    /// PNG of the one reference image sent AFTER the edited image (`width x height`, RGB, or
    /// RGBA when it carries transparency); `None` for no reference. Only an offer with
    /// `max_extra_references > 0` gets one (`pipeline::prepare` guards it). An adapter with a
    /// list field appends it after `image_png`; an endpoint without one refuses the call with
    /// `ImageEditError::ReferenceNotSupported`, never dropping it.
    pub reference_png: Option<Vec<u8>>,
    /// The native mask to send, `width * height` bytes, 255 = editable, 0 = keep; `None` when
    /// the offer takes no mask or nothing was painted (unless the offer requires one). The
    /// adapter encodes it in its provider's polarity (`codec::encode_mask_png`).
    pub mask: Option<Vec<u8>>,
    /// Sent width `k * W`.
    pub width: u32,
    /// Sent height `k * H`.
    pub height: u32,
    /// How to state `(width, height)` in the request.
    pub size_param: SizeParamStyle,
    /// For `SizeParamStyle::AspectTier`: the table entry equal to `(width, height)`.
    pub size_entry: Option<AspectTierEntry>,
}

/// Which request a response answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepPhase {
    /// The submit request.
    Submit,
    /// A follow-up request; `stage` is the adapter's own label from `NextStep::Poll`.
    Poll { stage: u8 },
}

/// Provider job handle kept by the executor between steps (for status polls and cancel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRef {
    /// The provider's job / request id.
    pub id: String,
    /// The provider's status URL, when it hands one out instead of a derivable path (BFL
    /// `polling_url`, fal `status_url`, Replicate `urls.get`); the adapter polls it again.
    pub poll_url: Option<String>,
    /// The provider's cancel URL, when it hands one out (fal `cancel_url`, Replicate
    /// `urls.cancel`).
    pub cancel_url: Option<String>,
}

/// Context of one adapter step.
#[derive(Debug, Clone, Copy)]
pub struct StepCtx<'a> {
    pub call: &'a EditCall,
    pub phase: StepPhase,
    /// Follow-up requests made so far.
    pub polls: u32,
    /// The latest job handle an adapter returned, if any.
    pub job: Option<&'a JobRef>,
}

/// What the executor does after a response.
#[derive(Debug, Clone, PartialEq)]
pub enum NextStep {
    /// Wait `after`, then send `request`; its response comes back with phase
    /// `Poll { stage }`. `job` replaces the kept handle when `Some`.
    Poll { request: HttpRequestSpec, after: Duration, stage: u8, job: Option<JobRef> },
    /// GET `request` and take its body as the final image bytes.
    Download(HttpRequestSpec),
    /// The final image bytes are already here (inline base64 decoded by the adapter).
    Image(Vec<u8>),
}

/// One provider API shape, as a pure state machine over HTTP request descriptions.
pub trait EditProtocol {
    /// The paid submit request for `call`.
    ///
    /// # Errors
    /// `ImageEditError::RequestBuild` (or a more specific variant) when the call cannot be
    /// expressed for this shape.
    fn submit(&self, call: &EditCall) -> Result<HttpRequestSpec, ImageEditError>;

    /// Interprets `response` to the request of `step` and decides the next step.
    ///
    /// # Errors
    /// The typed provider failure (`KeyRejected`, `RegionBlocked`, `RateLimited`,
    /// `OutOfCredits`, `Moderated`, `ProviderRejected`, `ProviderFailed`, `NoImageReturned`,
    /// `Decode`).
    fn next(&self, step: &StepCtx<'_>, response: &HttpResponse) -> Result<NextStep, ImageEditError>;

    /// A best-effort cancel request for the job of `step`, if the provider has one.
    fn cancel_request(&self, step: &StepCtx<'_>) -> Option<HttpRequestSpec>;
}
