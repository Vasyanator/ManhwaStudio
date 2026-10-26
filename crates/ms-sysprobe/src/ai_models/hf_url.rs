/*
File: crates/ms-sysprobe/src/ai_models/hf_url.rs

Purpose:
Hugging Face resolve URLs for both `ai_models` downloaders (pinned external models and the
app-managed main repository). Native-only (the parent mounts it under `cfg(not(wasm32))`).

Key items:
- `hf_resolve_url()`: `{HF_ENDPOINT or https://huggingface.co}/{repo}/resolve/{rev}/{path}`
  with every component percent-encoded. Replaces `hf_hub::ApiBuilder::from_env`, which
  panics without a home directory (Android app processes have none).
- `hf_auth_check_url()`: `{endpoint}/api/models/{repo}/auth-check`, the Hub's gated-access
  probe (200 granted, 401 no / rejected token, 403 terms not accepted, 404 no such repo);
  used by `hf_access`.
- `endpoint_from_env_value()` / `hf_resolve_url_at()` / `hf_auth_check_url_at()`: the pure
  halves, tested without touching the process environment.
- `url_source_file_url()`: the same segment encoding for files of plain-HTTPS (`Url` source)
  external models (GitHub releases, the Surya model host).
*/

/// Environment variable that redirects Hugging Face requests (mirrors, proxies, tests);
/// the same name `huggingface_hub` and `hf-hub` honour.
pub(crate) const HF_ENDPOINT_ENV: &str = "HF_ENDPOINT";
/// Endpoint used when `HF_ENDPOINT` is unset or empty.
pub(crate) const DEFAULT_HF_ENDPOINT: &str = "https://huggingface.co";

/// Resolve URL of `path` in the model repository `repo_id` at `revision`, on the endpoint
/// named by `HF_ENDPOINT` (default `https://huggingface.co`). Reads the environment on every
/// call and never touches the home directory.
#[must_use]
pub(crate) fn hf_resolve_url(repo_id: &str, revision: &str, path: &str) -> String {
    let configured = std::env::var(HF_ENDPOINT_ENV).ok();
    hf_resolve_url_at(endpoint_from_env_value(configured.as_deref()), repo_id, revision, path)
}

/// The endpoint a raw `HF_ENDPOINT` value selects: the default for an absent or blank value,
/// otherwise the trimmed value without trailing slashes.
#[must_use]
pub(crate) fn endpoint_from_env_value(value: Option<&str>) -> &str {
    match value.map(str::trim) {
        Some(endpoint) if !endpoint.is_empty() => endpoint.trim_end_matches('/'),
        Some(_) | None => DEFAULT_HF_ENDPOINT,
    }
}

/// [`hf_resolve_url`] with an explicit endpoint (no trailing slash). `repo_id` (`owner/name`)
/// and `path` are `/`-separated: each segment is percent-encoded and the separators kept;
/// `revision` is encoded as ONE segment (a `/` in a branch name becomes `%2F`, as the Hub
/// expects).
#[must_use]
pub(crate) fn hf_resolve_url_at(endpoint: &str, repo_id: &str, revision: &str, path: &str) -> String {
    format!("{endpoint}/{}/resolve/{}/{}", encode_segments(repo_id), percent_encode(revision), encode_segments(path))
}

/// URL of the Hugging Face access probe of model repository `repo_id` (`owner/name`) on the
/// endpoint named by `HF_ENDPOINT` (default `https://huggingface.co`). The only host a
/// Hugging Face token may be sent to, together with the resolve URLs above.
#[must_use]
pub(crate) fn hf_auth_check_url(repo_id: &str) -> String {
    let configured = std::env::var(HF_ENDPOINT_ENV).ok();
    hf_auth_check_url_at(endpoint_from_env_value(configured.as_deref()), repo_id)
}

/// [`hf_auth_check_url`] with an explicit endpoint (no trailing slash); every segment of
/// `repo_id` is percent-encoded like a resolve URL.
#[must_use]
pub(crate) fn hf_auth_check_url_at(endpoint: &str, repo_id: &str) -> String {
    format!("{endpoint}/api/models/{}/auth-check", encode_segments(repo_id))
}

/// URL of the `/`-separated relative `path` under a plain-HTTPS `base_url` (no trailing
/// slash) of an `ExternalSource::Url` model, every path segment percent-encoded like a
/// resolve URL. `base_url` itself is used verbatim (the catalog validates it).
#[must_use]
pub(crate) fn url_source_file_url(base_url: &str, path: &str) -> String {
    format!("{base_url}/{}", encode_segments(path))
}

/// Percent-encodes every `/`-separated segment of `value`, keeping the separators.
fn encode_segments(value: &str) -> String {
    value.split('/').map(percent_encode).collect::<Vec<_>>().join("/")
}

/// RFC 3986 percent-encoding of one path segment: unreserved characters stay, every other
/// UTF-8 byte becomes `%XX` (uppercase hex).
fn percent_encode(segment: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = "Vasyanator2/ManhwaStudio_AI_Models";

    #[test]
    fn resolve_url_encodes_every_segment_and_keeps_separators() {
        assert_eq!(
            hf_resolve_url_at(DEFAULT_HF_ENDPOINT, REPO, "main", "ONNX/PaddleOCR/detection/v5/det.onnx"),
            "https://huggingface.co/Vasyanator2/ManhwaStudio_AI_Models/resolve/main/ONNX/PaddleOCR/detection/v5/det.onnx"
        );
        assert_eq!(
            hf_resolve_url_at("https://hf.example", "owner/repo", "main", "dir name/a+b#c?.pt"),
            "https://hf.example/owner/repo/resolve/main/dir%20name/a%2Bb%23c%3F.pt"
        );
        // Non-ASCII is encoded byte by byte; unreserved punctuation stays.
        assert_eq!(hf_resolve_url_at("https://hf.example", "o/r", "main", "ä/x-y_z.~1"), "https://hf.example/o/r/resolve/main/%C3%A4/x-y_z.~1");
        // The revision is one segment: a branch-name slash is encoded as the Hub expects.
        assert_eq!(hf_resolve_url_at("https://hf.example", "o/r", "refs/pr/1", "f.bin"), "https://hf.example/o/r/resolve/refs%2Fpr%2F1/f.bin");
    }

    #[test]
    fn auth_check_url_targets_the_hub_api_of_the_repository() {
        assert_eq!(
            hf_auth_check_url_at(DEFAULT_HF_ENDPOINT, "black-forest-labs/FLUX.2-klein-9B"),
            "https://huggingface.co/api/models/black-forest-labs/FLUX.2-klein-9B/auth-check"
        );
        assert_eq!(hf_auth_check_url_at(endpoint_from_env_value(Some("https://hf-mirror.example/")), "o/r x"), "https://hf-mirror.example/api/models/o/r%20x/auth-check");
    }

    #[test]
    fn url_source_paths_are_encoded_under_the_base() {
        assert_eq!(
            url_source_file_url("https://github.com/JaidedAI/EasyOCR/releases/download", "pre-v1.1.6/craft_mlt_25k.zip"),
            "https://github.com/JaidedAI/EasyOCR/releases/download/pre-v1.1.6/craft_mlt_25k.zip"
        );
        assert_eq!(url_source_file_url("https://h.example/m", "a b/.gitattributes"), "https://h.example/m/a%20b/.gitattributes");
    }

    #[test]
    fn hf_endpoint_value_overrides_the_default() {
        assert_eq!(endpoint_from_env_value(None), DEFAULT_HF_ENDPOINT);
        assert_eq!(endpoint_from_env_value(Some("")), DEFAULT_HF_ENDPOINT);
        assert_eq!(endpoint_from_env_value(Some("   ")), DEFAULT_HF_ENDPOINT);
        assert_eq!(endpoint_from_env_value(Some(" https://hf-mirror.example/ ")), "https://hf-mirror.example");
        assert_eq!(endpoint_from_env_value(Some("http://127.0.0.1:8080//")), "http://127.0.0.1:8080");
        let url = hf_resolve_url_at(endpoint_from_env_value(Some("https://hf-mirror.example/")), REPO, "main", "a.json");
        assert_eq!(url, "https://hf-mirror.example/Vasyanator2/ManhwaStudio_AI_Models/resolve/main/a.json");
    }
}
