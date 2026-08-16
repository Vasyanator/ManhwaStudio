/*
File: src/launcher/new_project/quick_download/sites/manganelo.rs

Purpose:
Chapter resolver for the manganelo mirror family, which serves four interchangeable domains
from one page template.

Key functions:
- manganelo_plan()
- resolve_manganelo_chapter_url()
- parse_manganelo_images()

Notes:
Hosts served: nelomanga.net, natomanga.com, manganato.gg, mangakakalot.gg (each with or
without a `www.` prefix). This module supersedes the older per-host natomanga resolver: the
chapter page carries the authoritative pair `var cdns = [...]` (CDN bases) and
`var chapterImages = [...]` (paths), so banner and chrome `<img>` elements can no longer leak
into the page list and a CDN rotation is followed automatically.

The scan for the two script arrays uses the shared `find_js_array_literal` of `html.rs`; only
the marker names and the CDN join below are site-specific.
*/

use super::super::html::{collect_anchor_hrefs_containing, find_js_array_literal};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{normalize_network_url, path_contains};
use serde_json::Value;

/// Builds the download plan for a manganelo-family chapter or series URL.
///
/// A series URL is first resolved to its last `/chapter-` anchor, i.e. the earliest chapter
/// of the newest-first list the series page renders.
///
/// # Errors
/// Returns `QuickDownloadError` when a fetch fails, when a series page exposes no chapter
/// anchors, or when the chapter page carries neither of the two expected script arrays.
pub(crate) fn manganelo_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = resolve_manganelo_chapter_url(url)?;
    let html = fetch_text(&chapter_url, None)?;
    let image_urls = parse_manganelo_images(&html);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "manganelo chapter '{chapter_url}' yielded no images; expected inline \
                 `var cdns = [...]` and `var chapterImages = [...]` script arrays"
            ),
        });
    }
    // The mirrors gate their CDN on a same-site Referer, and every mirror uses its own
    // origin, so it is derived from the chapter URL rather than hard-coded.
    let referer = normalize_network_url("/", &chapter_url);
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(referer),
    })
}

/// Returns `url` itself when it already points at a chapter, otherwise walks the series page
/// and returns its last chapter anchor.
///
/// # Errors
/// Returns `QuickDownloadError` when the series page cannot be fetched or contains no
/// `/chapter-` anchor.
fn resolve_manganelo_chapter_url(url: &str) -> Result<String, QuickDownloadError> {
    if path_contains(url, "/chapter-") {
        return Ok(url.to_string());
    }
    let html = fetch_text(url, None)?;
    let chapters = collect_anchor_hrefs_containing(&html, url, "/chapter-");
    chapters.last().cloned().ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.no_chapters_error").to_string(),
        log_message: format!("manganelo series '{url}' exposes no '/chapter-' anchors"),
    })
}

/// Reads the ordered page URLs out of a manganelo chapter page.
///
/// Joins the first entry of the `var cdns` array to every entry of the `var chapterImages`
/// array, in array order. Returns an empty vector when either array is missing, is not valid
/// JSON, or holds no usable entry — the caller turns that into the user-facing error.
fn parse_manganelo_images(html: &str) -> Vec<String> {
    let Some(cdn_literal) = find_js_array_literal(html, "var cdns") else {
        return Vec::new();
    };
    let Ok(cdns) = serde_json::from_str::<Value>(cdn_literal) else {
        return Vec::new();
    };
    let Some(cdn) = cdns
        .as_array()
        .and_then(|entries| entries.first())
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    let Some(images_literal) = find_js_array_literal(html, "var chapterImages") else {
        return Vec::new();
    };
    let Ok(images) = serde_json::from_str::<Value>(images_literal) else {
        return Vec::new();
    };
    let Some(images) = images.as_array() else {
        return Vec::new();
    };
    images
        .iter()
        .filter_map(Value::as_str)
        .map(|path| join_cdn_path(cdn, path))
        .collect()
}

/// Joins a CDN base to a page path with exactly one separating slash, whichever side already
/// carries one.
fn join_cdn_path(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-written stand-in for a chapter page: two CDN bases, three paths in a deliberately
    /// unsorted order.
    const CHAPTER_HTML: &str = r#"<html><body>
<div class="reader"><img src="https://www.natomanga.com/logo.png"></div>
<script>
    var comic_id = 42;
    var cdns = ["https://cdn1.example.net","https://cdn2.example.net"];
    var chapterImages = ["images/ch1/03.jpg","images/ch1/01.jpg","images/ch1/02.jpg"];
</script>
</body></html>"#;

    #[test]
    fn parses_images_joining_first_cdn_and_keeping_order() {
        assert_eq!(
            parse_manganelo_images(CHAPTER_HTML),
            vec![
                "https://cdn1.example.net/images/ch1/03.jpg".to_string(),
                "https://cdn1.example.net/images/ch1/01.jpg".to_string(),
                "https://cdn1.example.net/images/ch1/02.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn cdn_join_never_doubles_or_drops_the_slash() {
        let html = r#"<script>
            var cdns = ["https://cdn.example.net/"];
            var chapterImages = ["/a.jpg","b.jpg"];
        </script>"#;
        assert_eq!(
            parse_manganelo_images(html),
            vec![
                "https://cdn.example.net/a.jpg".to_string(),
                "https://cdn.example.net/b.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn missing_chapter_images_array_yields_nothing() {
        let html = r#"<script>var cdns = ["https://cdn.example.net"];</script>"#;
        assert!(parse_manganelo_images(html).is_empty());
    }

    #[test]
    fn missing_cdns_array_yields_nothing() {
        let html = r#"<script>var chapterImages = ["a.jpg"];</script>"#;
        assert!(parse_manganelo_images(html).is_empty());
    }

    #[test]
    fn empty_chapter_images_array_yields_nothing() {
        let html = r#"<script>
            var cdns = ["https://cdn.example.net"];
            var chapterImages = [];
        </script>"#;
        assert!(parse_manganelo_images(html).is_empty());
    }

    #[test]
    fn a_distant_array_is_not_mistaken_for_the_marked_one() {
        // `var cdns` exists but holds no array, so the later unrelated array must be ignored
        // instead of being adopted as the CDN list.
        let html = r#"<script>
            var cdns = null;
            var unrelated = ["https://ads.example.net"];
            var chapterImages = ["a.jpg"];
        </script>"#;
        assert!(parse_manganelo_images(html).is_empty());
    }

    #[test]
    fn bracket_inside_a_quoted_path_does_not_end_the_array() {
        let html = r#"<script>
            var cdns = ["https://cdn.example.net"];
            var chapterImages = ["a]b.jpg","c.jpg"];
        </script>"#;
        assert_eq!(
            parse_manganelo_images(html),
            vec![
                "https://cdn.example.net/a]b.jpg".to_string(),
                "https://cdn.example.net/c.jpg".to_string(),
            ]
        );
    }
}
