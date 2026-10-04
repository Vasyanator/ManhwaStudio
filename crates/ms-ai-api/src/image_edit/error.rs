/*
File: crates/ms-ai-api/src/image_edit/error.rs

Purpose:
`ImageEditError`, the one typed error of the cloud image-edit layer. Its `Display` is the
localized user message (rendered at display time through `t!` / `tf!`), so the cleaning tool
shows `to_string()` as is.

Notes:
Target-neutral, `Clone` and `Send`: source errors (`image`, `ureq`, `serde_json`, raster) are
kept as `detail` text. No variant ever carries an API key or the prompt; `detail` strings are
technical, non-localized text from the failing library or provider (a provider error message
is passed through so the user sees the provider's own reason).
*/

use std::fmt;

use crate::error::AiApiError;

/// Failure of an image-edit run, from request validation through the HTTP exchange to the
/// size-exact finish of the result.
#[derive(Debug, Clone)]
pub enum ImageEditError {
    /// Image editing was requested on the web build, which has no HTTP executor.
    WebUnavailable,
    /// The model id is not in the provider's catalogue (and the provider takes no free id).
    UnknownModel { model_id: String },
    /// The prompt is empty or whitespace-only.
    EmptyPrompt,
    /// A raster buffer does not match its declared shape (zero size, wrong length, overflow).
    ShapeMismatch { detail: String },
    /// The caller's upscale factor `k` is outside `1..=max` of the selected offer.
    UpscaleNotAllowed { k: u8, max: u8 },
    /// The size to send is not an entry of the offer's allowed-size table (a caller bug: the
    /// cleaning frame only produces sizes that pass the offer's rule).
    SizeNotOffered { width: u32, height: u32 },
    /// The endpoint choice does not fit the provider (unknown region, missing or malformed
    /// base URL of the user's own server).
    InvalidEndpoint { detail: String },
    /// A request description could not be built (a multipart field name, a JSON body).
    RequestBuild { detail: String },
    /// The image or mask could not be encoded as PNG.
    Encode { detail: String },
    /// No API key is stored for a provider that requires one.
    KeyMissing,
    /// The provider rejected the API key (401 / 403 without a region marker).
    KeyRejected,
    /// The provider refuses the user's country or region.
    RegionBlocked,
    /// The provider's rate limit was hit (429).
    RateLimited,
    /// The account has no credits left (402 or an equivalent provider code).
    OutOfCredits,
    /// The provider's content filter refused the request or the result.
    Moderated,
    /// The provider rejected the request (4xx) with its own message.
    ProviderRejected { status: u16, message: String },
    /// The provider accepted the request but the generation failed.
    ProviderFailed { detail: String },
    /// The provider answered without an image; `reason` is its finish reason or text.
    NoImageReturned { reason: String },
    /// The HTTP exchange failed (connection, TLS, IO).
    Network { detail: String },
    /// A step exceeded its time limit; `stage` names the step (technical text).
    Timeout { stage: String },
    /// The result download exceeded the size cap.
    DownloadTooLarge { limit_bytes: u64 },
    /// The returned image could not be decoded (format, corrupt data, decoder limits).
    Decode { detail: String },
    /// The provider returned an image whose size is not exactly the size that was sent. Never
    /// repaired by resampling: the model does not honour the size this offer's rule promises.
    SizeMismatch { expected: (u32, u32), got: (u32, u32) },
    /// The finished image does not have the source size (a bug guard of the pipeline);
    /// `detail` describes the buffer that was produced.
    SizeContractViolated { expected: (u32, u32), detail: String },
    /// The run was cancelled by the user.
    Cancelled,
    /// A key-store operation failed.
    Key(AiApiError),
}

impl ImageEditError {
    /// The localized, human-readable message for the active UI locale. This is the text
    /// `Display` writes.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::WebUnavailable => t!("ai_api.image_edit.error.web_unavailable_error").to_string(),
            Self::UnknownModel { model_id } => tf!("ai_api.image_edit.error.unknown_model_error", model = model_id),
            Self::EmptyPrompt => t!("ai_api.image_edit.error.empty_prompt_error").to_string(),
            Self::ShapeMismatch { detail } => tf!("ai_api.image_edit.error.shape_mismatch_error", err = detail),
            Self::UpscaleNotAllowed { k, max } => tf!("ai_api.image_edit.error.upscale_not_allowed_error", k = k, max = max),
            Self::SizeNotOffered { width, height } => tf!("ai_api.image_edit.error.size_not_offered_error", width = width, height = height),
            Self::InvalidEndpoint { detail } => tf!("ai_api.image_edit.error.invalid_endpoint_error", err = detail),
            Self::RequestBuild { detail } => tf!("ai_api.image_edit.error.request_build_error", err = detail),
            Self::Encode { detail } => tf!("ai_api.image_edit.error.encode_error", err = detail),
            Self::KeyMissing => t!("ai_api.image_edit.error.key_missing_error").to_string(),
            Self::KeyRejected => t!("ai_api.image_edit.error.key_rejected_error").to_string(),
            Self::RegionBlocked => t!("ai_api.image_edit.error.region_blocked_error").to_string(),
            Self::RateLimited => t!("ai_api.image_edit.error.rate_limited_error").to_string(),
            Self::OutOfCredits => t!("ai_api.image_edit.error.out_of_credits_error").to_string(),
            Self::Moderated => t!("ai_api.image_edit.error.moderated_error").to_string(),
            Self::ProviderRejected { status, message } => tf!("ai_api.image_edit.error.provider_rejected_error", status = status, err = message),
            Self::ProviderFailed { detail } => tf!("ai_api.image_edit.error.provider_failed_error", err = detail),
            Self::NoImageReturned { reason } => tf!("ai_api.image_edit.error.no_image_returned_error", reason = reason),
            Self::Network { detail } => tf!("ai_api.image_edit.error.network_error", err = detail),
            Self::Timeout { stage } => tf!("ai_api.image_edit.error.timeout_error", stage = stage),
            Self::DownloadTooLarge { limit_bytes } => tf!("ai_api.image_edit.error.download_too_large_error", limit = limit_bytes / (1024 * 1024)),
            Self::Decode { detail } => tf!("ai_api.image_edit.error.decode_error", err = detail),
            Self::SizeMismatch { expected, got } => tf!("ai_api.image_edit.error.size_mismatch_error", expected_w = expected.0, expected_h = expected.1, got_w = got.0, got_h = got.1),
            Self::SizeContractViolated { expected, detail } => tf!("ai_api.image_edit.error.size_contract_violated_error", expected_w = expected.0, expected_h = expected.1, err = detail),
            Self::Cancelled => t!("ai_api.image_edit.error.cancelled_error").to_string(),
            Self::Key(error) => error.user_message(),
        }
    }
}

impl fmt::Display for ImageEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for ImageEditError {}

impl From<AiApiError> for ImageEditError {
    fn from(error: AiApiError) -> Self {
        Self::Key(error)
    }
}

#[cfg(test)]
mod tests {
    use super::ImageEditError;
    use crate::error::AiApiError;

    // No UI locale is installed in this test binary, so only the Display == user_message
    // contract is asserted, never the localized text itself.
    #[test]
    fn display_is_user_message() {
        let errors = [
            ImageEditError::SizeMismatch { expected: (1024, 1024), got: (1024, 1040) },
            ImageEditError::ProviderRejected { status: 400, message: "bad size".to_string() },
            ImageEditError::DownloadTooLarge { limit_bytes: 64 * 1024 * 1024 },
            ImageEditError::Cancelled,
        ];
        for error in errors {
            assert_eq!(error.to_string(), error.user_message());
        }
    }

    #[test]
    fn key_errors_keep_the_key_store_message() {
        let source = AiApiError::EmptyKey;
        let wrapped = ImageEditError::from(source.clone());
        assert_eq!(wrapped.to_string(), source.to_string());
    }
}
