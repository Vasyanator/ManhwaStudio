/*
File: src/launcher/new_project/quick_download/sites/senmanga.rs

Purpose:
Chapter resolver for raw.senmanga.com.

Key constants:
- SENMANGA_VIEWER_COOKIE, SENMANGA_PAGE_IMAGE_CLASS

Key functions:
- senmanga_plan()
- senmanga_request_headers()
- extract_senmanga_image_urls()

Notes:
Hosts served: `raw.senmanga.com`.
Required request headers, both mandatory and both listed in `senmanga_request_headers`:
`Cookie: viewer=1` (a fixed value, not a session token - it selects the all-pages viewer;
without it the page shows a single image) and `Referer: <chapter url>`. The site expects
that same `Referer` on the image requests, so it is also returned in the plan.
Chapter URLs have the shape `/<series>/<chapter>`; anything shorter is rejected.
*/

use super::super::html::{extract_html_tags, get_html_attr, html_unescape};
use super::super::http::fetch_text_with_headers;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url, path_segment_count};

/// Fixed cookie selecting the all-pages viewer. Not user-specific and not a session.
const SENMANGA_VIEWER_COOKIE: &str = "viewer=1";
/// Class marking the `<img>` elements that are chapter pages.
const SENMANGA_PAGE_IMAGE_CLASS: &str = "picture";

/// Builds the download plan for a raw.senmanga.com chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when `url` is not a `/<series>/<chapter>` URL, when the
/// page fetch fails, or when the page holds no `picture` images.
pub(crate) fn senmanga_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    // Both the series and the chapter segment are required; the viewer only exists there.
    if path_segment_count(url) < 2 {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
            log_message: format!(
                "senmanga url '{url}' is not a chapter url of shape /<series>/<chapter>"
            ),
        });
    }
    let html = fetch_text_with_headers(url, &senmanga_request_headers(url))?;
    let image_urls = extract_senmanga_image_urls(&html, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "senmanga chapter '{url}' has no <img class=\"{SENMANGA_PAGE_IMAGE_CLASS}\"> \
                 (sent with cookie '{SENMANGA_VIEWER_COOKIE}')"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(url.to_string()),
    })
}

/// The complete header list the chapter page request must carry: the all-pages viewer
/// cookie and the chapter URL as `Referer`.
fn senmanga_request_headers(chapter_url: &str) -> [(&str, &str); 2] {
    [
        ("Cookie", SENMANGA_VIEWER_COOKIE),
        ("Referer", chapter_url),
    ]
}

/// Collects the page URLs of a senmanga chapter page in document order, deduplicated and
/// resolved against `base_url`.
///
/// A page is an `<img>` whose `class` list contains `picture`; sources may be
/// protocol-relative and are resolved to the base scheme.
fn extract_senmanga_image_urls(html: &str, base_url: &str) -> Vec<String> {
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let is_page = get_html_attr(tag.attrs, "class").is_some_and(|classes| {
            classes
                .split_whitespace()
                .any(|class| class == SENMANGA_PAGE_IMAGE_CLASS)
        });
        if !is_page {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src").map(str::trim) else {
            continue;
        };
        if src.is_empty() || src.starts_with("data:") {
            continue;
        }
        image_urls.push(normalize_network_url(&html_unescape(src), base_url));
    }
    dedupe_preserve(image_urls)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTER_URL: &str = "https://raw.senmanga.com/series/12";

    #[test]
    fn senmanga_request_headers_carry_viewer_cookie_and_referer() {
        assert_eq!(
            senmanga_request_headers(CHAPTER_URL),
            [("Cookie", "viewer=1"), ("Referer", CHAPTER_URL)]
        );
    }

    #[test]
    fn extract_senmanga_image_urls_keeps_order_and_adds_scheme() {
        let html = "<img class=\"picture\" src=\"https://raw.senmanga.com/viewer/s/12/1\">\
                    <img class=\"img-fluid picture\" src=\"//raw.senmanga.com/viewer/s/12/2\">\
                    <img class=\"picture\" src=\"/viewer/s/12/3\">";
        assert_eq!(
            extract_senmanga_image_urls(html, CHAPTER_URL),
            vec![
                "https://raw.senmanga.com/viewer/s/12/1".to_string(),
                "https://raw.senmanga.com/viewer/s/12/2".to_string(),
                "https://raw.senmanga.com/viewer/s/12/3".to_string(),
            ]
        );
    }

    #[test]
    fn extract_senmanga_image_urls_ignores_other_images() {
        let html = "<img class=\"logo\" src=\"/logo.png\">\
                    <img src=\"/banner.png\">\
                    <img class=\"picture-frame\" src=\"/frame.png\">\
                    <img class=\"picture\" src=\"/viewer/s/12/1\">";
        assert_eq!(
            extract_senmanga_image_urls(html, CHAPTER_URL),
            vec!["https://raw.senmanga.com/viewer/s/12/1".to_string()]
        );
    }

    #[test]
    fn extract_senmanga_image_urls_unescapes_and_dedupes() {
        let html = "<img class=\"picture\" src=\"/viewer?p=1&amp;s=2\">\
                    <img class=\"picture\" src=\"/viewer?p=1&s=2\">";
        assert_eq!(
            extract_senmanga_image_urls(html, CHAPTER_URL),
            vec!["https://raw.senmanga.com/viewer?p=1&s=2".to_string()]
        );
    }

    #[test]
    fn extract_senmanga_image_urls_returns_empty_without_pages() {
        let html = "<div class=\"picture\"><img src=\"/x.png\"></div>\
                    <img class=\"picture\" src=\"\">";
        assert!(extract_senmanga_image_urls(html, CHAPTER_URL).is_empty());
    }
}
