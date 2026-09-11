/*
File: src/launcher/new_project/quick_download/sites/mangataro.rs

Purpose:
Chapter resolver for mangataro.org.

Key functions:
- mangataro_plan()
- mangataro_chapter_id()
- parse_mangataro_images()

Notes:
Serves the host `mangataro.org`. A reader URL is `/read/<manga slug>/<title part>-<chapter
id>`; the numeric chapter id is the trailing digit run of that last segment and is the only
thing the image endpoint needs. `GET /auth/chapter-content?chapter_id=<id>` answers with an
`images` array of absolute CDN URLs, already in reading order. The signed chapter LIST
endpoint is deliberately not used: the quick downloader resolves one chapter, not a series.
*/

use super::super::http::fetch_json_value;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::path_segments;
use serde_json::Value;

/// Origin of the site; both the image endpoint and the `Referer` are derived from it.
const MANGATARO_ORIGIN: &str = "https://mangataro.org";

/// Builds the download plan for a mangataro.org reader URL (`/read/<slug>/<...>-<id>`).
///
/// # Errors
/// Returns `QuickDownloadError` when the URL carries no chapter id, when the content
/// request fails, or when the response holds no image URLs.
pub(crate) fn mangataro_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_id = mangataro_chapter_id(url).ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
        log_message: format!("mangataro url '{url}' is not '/read/<slug>/<name>-<chapter id>'"),
    })?;
    let api_url = format!("{MANGATARO_ORIGIN}/auth/chapter-content?chapter_id={chapter_id}");
    // The browser issues this request from the chapter page, so it is the honest `Referer`.
    let json = fetch_json_value(&api_url, Some(url))?;
    let image_urls = parse_mangataro_images(&json);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!("mangataro chapter content '{api_url}' has no images array"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(format!("{MANGATARO_ORIGIN}/")),
    })
}

/// Returns the numeric chapter id of a `/read/<slug>/<segment>` URL.
///
/// The id is the trailing digit run of the segment that follows the manga slug; `None` is
/// returned when the URL is not a reader URL or that segment does not end in digits.
fn mangataro_chapter_id(url: &str) -> Option<String> {
    let segments = path_segments(url);
    let read_index = segments
        .iter()
        .position(|segment| segment.as_str() == "read")?;
    // /read/<manga slug>/<chapter segment>
    let chapter_segment = segments.get(read_index + 2)?;
    // Walking chars (not bytes) keeps the slice boundary valid for non-ASCII slugs.
    let digits_start = chapter_segment
        .char_indices()
        .rev()
        .take_while(|(_, character)| character.is_ascii_digit())
        .last()
        .map(|(index, _)| index)?;
    Some(chapter_segment[digits_start..].to_string())
}

/// Returns the ordered image URLs of a chapter-content payload.
///
/// Returns an empty vector when the `images` array is missing or not an array; non-string
/// entries are skipped and the array order is the reading order.
fn parse_mangataro_images(payload: &Value) -> Vec<String> {
    payload
        .get("images")
        .and_then(Value::as_array)
        .map(|images| {
            images
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_mangataro_images_keeps_response_order() {
        let payload = json!({
            "images": [
                "https://cdn.example.test/b/02.jpg",
                "https://cdn.example.test/b/01.jpg",
            ],
        });
        assert_eq!(
            parse_mangataro_images(&payload),
            vec![
                "https://cdn.example.test/b/02.jpg".to_string(),
                "https://cdn.example.test/b/01.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn parse_mangataro_images_returns_empty_on_missing_or_wrong_field() {
        assert!(parse_mangataro_images(&json!({})).is_empty());
        assert!(parse_mangataro_images(&json!({"images": []})).is_empty());
        assert!(parse_mangataro_images(&json!({"images": "nope"})).is_empty());
    }

    #[test]
    fn parse_mangataro_images_skips_non_string_entries() {
        let payload = json!({"images": [17, "https://cdn.example.test/b/01.jpg", null]});
        assert_eq!(
            parse_mangataro_images(&payload),
            vec!["https://cdn.example.test/b/01.jpg".to_string()]
        );
    }

    #[test]
    fn mangataro_chapter_id_reads_trailing_digits_of_reader_urls() {
        assert_eq!(
            mangataro_chapter_id("https://mangataro.org/read/some-manga/ch12-98765"),
            Some("98765".to_string())
        );
        assert_eq!(
            mangataro_chapter_id("https://mangataro.org/read/some-manga/4242?page=2"),
            Some("4242".to_string())
        );
    }

    #[test]
    fn mangataro_chapter_id_rejects_malformed_urls() {
        // Series page: no `/read/` section at all.
        assert_eq!(
            mangataro_chapter_id("https://mangataro.org/manga/some-manga"),
            None
        );
        // Reader section without the chapter segment.
        assert_eq!(
            mangataro_chapter_id("https://mangataro.org/read/some-manga"),
            None
        );
        // Chapter segment that does not end in digits.
        assert_eq!(
            mangataro_chapter_id("https://mangataro.org/read/some-manga/latest"),
            None
        );
        assert_eq!(mangataro_chapter_id("https://mangataro.org/"), None);
    }
}
