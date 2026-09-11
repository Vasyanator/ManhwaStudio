/*
File: src/launcher/new_project/quick_download/sites/weebdex.rs

Purpose:
Chapter resolver for weebdex.org.

Key functions:
- weebdex_plan()
- weebdex_chapter_id()
- parse_weebdex_images()

Notes:
Serves the host `weebdex.org` (the reader pages); the JSON API it talks to lives on the
separate host `api.weebdex.org`, which is why every API call carries the site `Referer` and
`Origin`. Fully API driven: `GET /chapter/{id}` answers with the CDN origin (`node`), the
chapter id and the page file names, and one page URL is `{node}/data/{id}/{name}`. Title
URLs are rejected: resolving a chapter list is out of scope for the quick downloader.
*/

use super::super::http::fetch_json_with_headers;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::path_segment_after;
use serde_json::Value;

/// Origin of the reader site, used for the `Referer`/`Origin` the cross-origin API expects.
const WEEBDEX_SITE_ORIGIN: &str = "https://weebdex.org";
/// Origin of the JSON API.
const WEEBDEX_API_ORIGIN: &str = "https://api.weebdex.org";

/// Builds the download plan for a weebdex.org chapter URL (`/chapter/<id>`).
///
/// # Errors
/// Returns `QuickDownloadError` when the URL carries no chapter id, when the API request
/// fails, or when the chapter payload yields no page URLs.
pub(crate) fn weebdex_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_id = weebdex_chapter_id(url).ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
        log_message: format!("weebdex url '{url}' has no '/chapter/<id>' segment"),
    })?;
    let api_url = format!("{WEEBDEX_API_ORIGIN}/chapter/{chapter_id}");
    let referer = format!("{WEEBDEX_SITE_ORIGIN}/");
    let json = fetch_json_with_headers(
        &api_url,
        &[
            ("Referer", referer.as_str()),
            ("Origin", WEEBDEX_SITE_ORIGIN),
        ],
    )?;
    let image_urls = parse_weebdex_images(&json);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "weebdex chapter response '{api_url}' has no node/id/data page list"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(referer),
    })
}

/// Returns the chapter id of a `/chapter/<id>[/<page>]` URL, or `None` for any other shape
/// (a title URL in particular).
fn weebdex_chapter_id(url: &str) -> Option<String> {
    path_segment_after(url, "chapter")
}

/// Assembles the ordered page URLs of a chapter payload as `{node}/data/{id}/{name}`.
///
/// Returns an empty vector when `node`, `id` or the `data` page array is missing; pages
/// without a `name` are skipped, and the order of `data` is the reading order.
fn parse_weebdex_images(payload: &Value) -> Vec<String> {
    let Some(node) = payload.get("node").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(chapter_id) = json_id_to_string(payload.get("id")) else {
        return Vec::new();
    };
    let Some(pages) = payload.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    // `node` is an origin that may or may not carry a trailing slash.
    let base = node.trim_end_matches('/');
    pages
        .iter()
        .filter_map(|page| page.get("name"))
        .filter_map(Value::as_str)
        .map(|name| format!("{base}/data/{chapter_id}/{name}"))
        .collect()
}

/// Reads an id the API may encode either as a JSON string or as a JSON number.
/// Returns `None` for a missing id or any other JSON type.
fn json_id_to_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_weebdex_images_builds_cdn_urls_in_order() {
        let payload = json!({
            "node": "https://cdn.example.test",
            "id": "chap-1",
            "data": [
                {"name": "02.webp", "dimensions": [800, 1200]},
                {"name": "01.webp", "dimensions": [800, 1200]},
            ],
        });
        assert_eq!(
            parse_weebdex_images(&payload),
            vec![
                "https://cdn.example.test/data/chap-1/02.webp".to_string(),
                "https://cdn.example.test/data/chap-1/01.webp".to_string(),
            ]
        );
    }

    #[test]
    fn parse_weebdex_images_accepts_numeric_id_and_trailing_slash_node() {
        let payload = json!({
            "node": "https://cdn.example.test/",
            "id": 4242,
            "data": [{"name": "01.png"}],
        });
        assert_eq!(
            parse_weebdex_images(&payload),
            vec!["https://cdn.example.test/data/4242/01.png".to_string()]
        );
    }

    #[test]
    fn parse_weebdex_images_returns_empty_on_missing_fields() {
        let no_node = json!({"id": "chap-1", "data": [{"name": "01.png"}]});
        assert!(parse_weebdex_images(&no_node).is_empty());
        let no_id = json!({"node": "https://cdn.example.test", "data": [{"name": "01.png"}]});
        assert!(parse_weebdex_images(&no_id).is_empty());
        let no_pages = json!({"node": "https://cdn.example.test", "id": "chap-1"});
        assert!(parse_weebdex_images(&no_pages).is_empty());
        let empty_pages =
            json!({"node": "https://cdn.example.test", "id": "chap-1", "data": []});
        assert!(parse_weebdex_images(&empty_pages).is_empty());
    }

    #[test]
    fn parse_weebdex_images_skips_pages_without_name() {
        let payload = json!({
            "node": "https://cdn.example.test",
            "id": "chap-1",
            "data": [{"dimensions": [1, 2]}, {"name": "01.png"}],
        });
        assert_eq!(
            parse_weebdex_images(&payload),
            vec!["https://cdn.example.test/data/chap-1/01.png".to_string()]
        );
    }

    #[test]
    fn weebdex_chapter_id_reads_chapter_urls_only() {
        assert_eq!(
            weebdex_chapter_id("https://weebdex.org/chapter/abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            weebdex_chapter_id("https://weebdex.org/chapter/abc123/5"),
            Some("abc123".to_string())
        );
        assert_eq!(
            weebdex_chapter_id("https://weebdex.org/title/xyz/some-slug"),
            None
        );
        assert_eq!(weebdex_chapter_id("https://weebdex.org/chapter/"), None);
        assert_eq!(weebdex_chapter_id("https://weebdex.org/"), None);
    }
}
