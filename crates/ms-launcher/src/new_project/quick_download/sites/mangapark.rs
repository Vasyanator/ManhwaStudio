/*
File: src/launcher/new_project/quick_download/sites/mangapark.rs

Purpose:
Chapter resolver for mangapark and its mirror domains.

Key functions:
- mangapark_plan()
- mangapark_chapter_id()
- parse_mangapark_images()

Notes:
Serves the mirror family `mangapark`, `comicpark` and `readpark` on `.com`, `.net`, `.org`,
`.me`, `.io` and `.to`, plus `parkmanga.com/.net/.org` and `mpark.to`, each with an optional
`www.` prefix. It does NOT serve bato.to, which has its own resolver. Every mirror answers
on its own origin, so the request origin is taken from the incoming URL. Pages come from one
GraphQL POST to `{origin}/apo/` (operation `Get_chapterNode`), whose payload carries the
ordered `imageFile.urlList`. Both reader URL shapes are accepted: `/title/<slug>/<id>-...`
and `/comic/<comic id>/<slug>/<part>-i<id>`.
*/

use super::super::http::post_json_value;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{normalize_network_url, path_segments};
use serde_json::{Value, json};

/// GraphQL operation name and root field serving a single chapter node.
const CHAPTER_OPERATION: &str = "Get_chapterNode";
/// Root field of the response payload, i.e. the key holding the chapter node.
const CHAPTER_FIELD: &str = "get_chapterNode";
/// Query document requesting only what a download needs: the chapter id and its page list.
const CHAPTER_QUERY: &str = "query Get_chapterNode($id: ID!) { \
     get_chapterNode(id: $id) { data { id imageFile { urlList } } } }";

/// Builds the download plan for a mangapark-family chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the URL carries no chapter id, when the GraphQL
/// request fails, or when the response holds no page URLs.
pub(crate) fn mangapark_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_id = mangapark_chapter_id(url).ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
        log_message: format!("mangapark url '{url}' carries no chapter id"),
    })?;
    // Mirrors are independent origins; keep every request on the one the user opened.
    let api_url = normalize_network_url("/apo/", url);
    let referer = normalize_network_url("/", url);
    let origin = referer.trim_end_matches('/').to_string();
    let body = json!({
        "query": CHAPTER_QUERY,
        "variables": {"id": chapter_id},
        "operationName": CHAPTER_OPERATION,
    });
    let json = post_json_value(
        &api_url,
        &body,
        &[
            ("Referer", referer.as_str()),
            ("Origin", origin.as_str()),
            ("Content-Type", "application/json"),
        ],
    )?;
    let image_urls = parse_mangapark_images(&json, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "mangapark '{api_url}' returned no imageFile.urlList for chapter '{chapter_id}'"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(referer),
    })
}

/// Extracts the numeric chapter id from either reader URL shape.
///
/// `/title/<slug>/<id>-<lang>-ch.<n>` uses the leading digits of the third segment;
/// `/comic/<comic id>/<slug>/<part>-i<id>` uses the digits after the last `-i`. Returns
/// `None` for a series URL or any other shape.
fn mangapark_chapter_id(url: &str) -> Option<String> {
    let segments = path_segments(url);
    match segments.first().map(String::as_str)? {
        "title" => leading_digits(segments.get(2)?),
        "comic" => {
            let (_, tail) = segments.get(3)?.rsplit_once("-i")?;
            leading_digits(tail)
        }
        _ => None,
    }
}

/// Returns the leading ASCII digit run of `value`, or `None` when it does not start with a
/// digit. Char-indexed so the slice boundary stays valid for non-ASCII slugs.
fn leading_digits(value: &str) -> Option<String> {
    let end = value
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(value.len(), |(index, _)| index);
    if end == 0 {
        return None;
    }
    Some(value[..end].to_string())
}

/// Returns the ordered page URLs of a `Get_chapterNode` response.
///
/// Reads `data.get_chapterNode.data.imageFile.urlList`, falling back to the single member
/// of `data` when the server aliased the root field. Entries are resolved against
/// `page_url` so a relative entry still yields an absolute URL; the list order is the
/// reading order. Returns an empty vector when any level is missing.
fn parse_mangapark_images(payload: &Value, page_url: &str) -> Vec<String> {
    let Some(root) = payload.get("data") else {
        return Vec::new();
    };
    let Some(node) = root.get(CHAPTER_FIELD).or_else(|| single_member(root)) else {
        return Vec::new();
    };
    let Some(urls) = node
        .get("data")
        .and_then(|data| data.get("imageFile"))
        .and_then(|file| file.get("urlList"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    urls.iter()
        .filter_map(Value::as_str)
        .map(|entry| normalize_network_url(entry, page_url))
        .collect()
}

/// Returns the only member of a JSON object, or `None` when it holds zero or several.
fn single_member(value: &Value) -> Option<&Value> {
    let object = value.as_object()?;
    if object.len() == 1 {
        object.values().next()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Response envelope of one chapter node carrying `urls` as its page list.
    fn chapter_response(field: &str, urls: Value) -> Value {
        json!({"data": {field: {"data": {"id": "67890", "imageFile": {"urlList": urls}}}}})
    }

    #[test]
    fn parse_mangapark_images_keeps_response_order() {
        let payload = chapter_response(
            CHAPTER_FIELD,
            json!([
                "https://cdn.example.test/p/02.jpeg",
                "https://cdn.example.test/p/01.jpeg",
            ]),
        );
        assert_eq!(
            parse_mangapark_images(&payload, "https://mangapark.net/title/demo/67890"),
            vec![
                "https://cdn.example.test/p/02.jpeg".to_string(),
                "https://cdn.example.test/p/01.jpeg".to_string(),
            ]
        );
    }

    #[test]
    fn parse_mangapark_images_resolves_relative_entries_and_aliases() {
        let payload = chapter_response("chapter", json!(["/media/01.jpeg"]));
        assert_eq!(
            parse_mangapark_images(&payload, "https://comicpark.org/title/demo/67890"),
            vec!["https://comicpark.org/media/01.jpeg".to_string()]
        );
    }

    #[test]
    fn parse_mangapark_images_returns_empty_on_missing_pieces() {
        assert!(parse_mangapark_images(&json!({}), "https://mangapark.net/").is_empty());
        assert!(
            parse_mangapark_images(&chapter_response(CHAPTER_FIELD, json!([])), "https://x.test/")
                .is_empty()
        );
        let no_image_file = json!({"data": {CHAPTER_FIELD: {"data": {"id": "1"}}}});
        assert!(parse_mangapark_images(&no_image_file, "https://x.test/").is_empty());
        // Two root fields and no expected key: the alias fallback must stay off.
        let ambiguous = json!({"data": {"a": {"data": {"imageFile": {"urlList": ["/1.jpg"]}}},
                                        "b": {}}});
        assert!(parse_mangapark_images(&ambiguous, "https://x.test/").is_empty());
    }

    #[test]
    fn mangapark_chapter_id_reads_both_reader_shapes() {
        assert_eq!(
            mangapark_chapter_id("https://mangapark.net/title/12345-en-demo/67890-en-ch.01"),
            Some("67890".to_string())
        );
        assert_eq!(
            mangapark_chapter_id("https://mpark.to/comic/12345/demo/vol1-ch1-i67890"),
            Some("67890".to_string())
        );
    }

    #[test]
    fn mangapark_chapter_id_rejects_malformed_urls() {
        // Series URL: no chapter segment.
        assert_eq!(
            mangapark_chapter_id("https://mangapark.net/title/12345-demo"),
            None
        );
        // Chapter segment not starting with digits.
        assert_eq!(
            mangapark_chapter_id("https://mangapark.net/title/12345-demo/latest"),
            None
        );
        // `comic` shape without the `-i<id>` marker.
        assert_eq!(
            mangapark_chapter_id("https://parkmanga.com/comic/12345/demo/vol1"),
            None
        );
        assert_eq!(mangapark_chapter_id("https://mangapark.net/"), None);
    }
}
