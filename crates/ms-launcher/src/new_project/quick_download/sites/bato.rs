/*
File: src/launcher/new_project/quick_download/sites/bato.rs

Purpose:
Chapter resolver for bato.to.

Key functions:
- bato_plan()
- extract_bato_astro_image_urls()
- extract_bato_script_image_urls()

Notes:
Two page shapes are supported: the current Astro island (`<astro-island component-url=
"/_astro/ImageList..." props="...">`, whose props hold doubly-encoded JSON) and the older
inline `imgHttps` array. Images require the bato.to `Referer`.

The legacy scan deliberately does NOT use the shared `find_js_array_literal`: it accepts the
first `[` after the first `imgHttps` occurrence whatever separates the two, while the shared
scanner requires an assignment gap. Only the bracket matching is shared
(`html::find_array_literal_end`).
*/

use super::super::html::{extract_html_tags, find_array_literal_end, get_html_attr, html_unescape};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use serde_json::Value;

/// Builds the download plan for a bato.to chapter URL, trying the Astro payload first and
/// falling back to the legacy inline script array.
///
/// # Errors
/// Returns `QuickDownloadError` when the page fetch fails or neither payload yields images.
pub(crate) fn bato_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let mut image_urls = extract_bato_astro_image_urls(&html);
    if image_urls.is_empty() {
        image_urls = extract_bato_script_image_urls(&html);
    }
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.batoto_no_images_error").to_string(),
            log_message: format!("bato page '{url}' has no image urls"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some("https://bato.to/".to_string()),
    })
}

/// Reads page URLs from the `ImageList` Astro island props (HTML-unescaped JSON whose
/// `imageFiles[1]` is itself a JSON array of `[id, url]` pairs). Returns an empty vector
/// when the island is absent or shaped differently.
fn extract_bato_astro_image_urls(html: &str) -> Vec<String> {
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("astro-island") {
            continue;
        }
        let Some(component_url) = get_html_attr(tag.attrs, "component-url") else {
            continue;
        };
        if !component_url.starts_with("/_astro/ImageList") {
            continue;
        }
        let Some(props_raw) = get_html_attr(tag.attrs, "props") else {
            continue;
        };
        let props = html_unescape(props_raw);
        let Ok(json) = serde_json::from_str::<Value>(&props) else {
            continue;
        };
        let Some(image_files) = json.get("imageFiles").and_then(Value::as_array) else {
            continue;
        };
        let Some(second) = image_files.get(1).and_then(Value::as_str) else {
            continue;
        };
        let Ok(entries) = serde_json::from_str::<Value>(second) else {
            continue;
        };
        let Some(entries) = entries.as_array() else {
            continue;
        };
        let urls = entries
            .iter()
            .filter_map(Value::as_array)
            .filter_map(|entry| entry.get(1))
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !urls.is_empty() {
            return urls;
        }
    }
    Vec::new()
}

/// Reads page URLs from the legacy inline `imgHttps` JSON array. Returns an empty vector
/// when the marker, the array, or valid JSON is missing.
///
/// The array is taken from the first `[` after the first `imgHttps` occurrence, regardless of
/// what separates them; that tolerance is what the legacy pages need, so this does not go
/// through `html::find_js_array_literal`, which insists on an assignment gap.
fn extract_bato_script_image_urls(html: &str) -> Vec<String> {
    let marker = "imgHttps";
    let Some(marker_index) = html.find(marker) else {
        return Vec::new();
    };
    let remainder = &html[marker_index..];
    let Some(open) = remainder.find('[') else {
        return Vec::new();
    };
    let Some(close) = find_array_literal_end(&remainder[open..]) else {
        return Vec::new();
    };
    let payload = &remainder[open..open + close + 1];
    serde_json::from_str::<Vec<String>>(payload).unwrap_or_default()
}
