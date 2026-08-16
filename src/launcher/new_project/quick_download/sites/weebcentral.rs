/*
File: src/launcher/new_project/quick_download/sites/weebcentral.rs

Purpose:
Chapter resolver for weebcentral.com.

Key constants:
- WEEBCENTRAL_IMAGES_PATH, WEEBCENTRAL_IMAGES_QUERY

Key functions:
- weebcentral_plan()
- weebcentral_chapter_url(), weebcentral_images_url()
- weebcentral_fragment_headers()
- extract_weebcentral_image_urls()

Notes:
Hosts served: `weebcentral.com`, with or without `www.`.
The chapter page carries no page images: they come from a second request to the same
host, `GET <chapter url>/images?is_prev=False&current_page=1&reading_style=long_strip`,
which answers with an HTML fragment. That request is only served with the full header set
in `weebcentral_fragment_headers`: `HX-Request: true`, `HX-Current-URL: <chapter url>`,
`Referer: <chapter url>` and a wildcard `Accept`. The chapter URL is also returned as the
plan `Referer` for the image downloads.
Only chapter URLs (`/chapters/<id>`) are resolvable; a series URL is rejected instead of
being fetched.
*/

use super::super::html::{extract_html_tags, get_html_attr, html_unescape};
use super::super::http::fetch_text_with_headers;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url, path_segments};

/// Path appended to the chapter URL to reach the page-list fragment.
const WEEBCENTRAL_IMAGES_PATH: &str = "/images";
/// Query of the page-list fragment: first page, forward direction, long-strip reader.
const WEEBCENTRAL_IMAGES_QUERY: &str = "is_prev=False&current_page=1&reading_style=long_strip";

/// Builds the download plan for a weebcentral.com chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when `url` is not a `/chapters/<id>` URL, when the
/// fragment request fails, or when the fragment holds no images.
pub(crate) fn weebcentral_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    if !is_weebcentral_chapter_url(url) {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
            log_message: format!(
                "weebcentral url '{url}' is not a chapter url of shape /chapters/<id>"
            ),
        });
    }
    let chapter_url = weebcentral_chapter_url(url);
    let images_url = weebcentral_images_url(&chapter_url);
    let fragment =
        fetch_text_with_headers(&images_url, &weebcentral_fragment_headers(&chapter_url))?;
    let image_urls = extract_weebcentral_image_urls(&fragment, &chapter_url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "weebcentral fragment '{images_url}' has no <img src> (requested with HX-Request \
                 and HX-Current-URL '{chapter_url}')"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(chapter_url),
    })
}

/// Returns `true` for URLs pointing at a single chapter, the only shape whose page-list
/// fragment exists; a `/series/<id>` URL is not one.
fn is_weebcentral_chapter_url(url: &str) -> bool {
    let segments = path_segments(url);
    segments.len() >= 2 && segments[0] == "chapters"
}

/// Normalizes a pasted chapter link into the exact URL the site expects to see in
/// `Referer` and `HX-Current-URL`: no fragment, no query, no trailing slash.
fn weebcentral_chapter_url(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment.split('?').next().unwrap_or(without_fragment);
    without_query.trim_end_matches('/').to_string()
}

/// Builds the page-list fragment endpoint of a normalized chapter URL.
fn weebcentral_images_url(chapter_url: &str) -> String {
    format!("{chapter_url}{WEEBCENTRAL_IMAGES_PATH}?{WEEBCENTRAL_IMAGES_QUERY}")
}

/// The complete header list the fragment request must carry. The site answers the
/// endpoint only as a partial-page request, which the `HX-*` pair announces.
fn weebcentral_fragment_headers(chapter_url: &str) -> [(&str, &str); 4] {
    [
        ("HX-Request", "true"),
        ("HX-Current-URL", chapter_url),
        ("Referer", chapter_url),
        ("Accept", "*/*"),
    ]
}

/// Collects the page URLs of a weebcentral page-list fragment in document order,
/// deduplicated and resolved against `base_url`.
///
/// Every `<img src>` of the fragment is a page; empty and `data:` sources are dropped.
fn extract_weebcentral_image_urls(fragment: &str, base_url: &str) -> Vec<String> {
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(fragment) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
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

    const CHAPTER_URL: &str = "https://weebcentral.com/chapters/01JHABCDEF";

    #[test]
    fn is_weebcentral_chapter_url_rejects_series_urls() {
        assert!(is_weebcentral_chapter_url(CHAPTER_URL));
        assert!(is_weebcentral_chapter_url(
            "https://www.weebcentral.com/chapters/01JHABCDEF/"
        ));
        assert!(!is_weebcentral_chapter_url(
            "https://weebcentral.com/series/01J7ABCDEF/Title"
        ));
        assert!(!is_weebcentral_chapter_url("https://weebcentral.com/chapters"));
    }

    #[test]
    fn weebcentral_chapter_url_drops_query_fragment_and_trailing_slash() {
        assert_eq!(
            weebcentral_chapter_url("https://weebcentral.com/chapters/01JHABCDEF/?x=1#top"),
            CHAPTER_URL
        );
        assert_eq!(weebcentral_chapter_url(CHAPTER_URL), CHAPTER_URL);
    }

    #[test]
    fn weebcentral_images_url_appends_endpoint_and_query() {
        assert_eq!(
            weebcentral_images_url(CHAPTER_URL),
            "https://weebcentral.com/chapters/01JHABCDEF/images\
             ?is_prev=False&current_page=1&reading_style=long_strip"
        );
    }

    #[test]
    fn weebcentral_fragment_headers_are_complete() {
        assert_eq!(
            weebcentral_fragment_headers(CHAPTER_URL),
            [
                ("HX-Request", "true"),
                ("HX-Current-URL", CHAPTER_URL),
                ("Referer", CHAPTER_URL),
                ("Accept", "*/*"),
            ]
        );
    }

    #[test]
    fn extract_weebcentral_image_urls_keeps_order_and_resolves_relatives() {
        let fragment = "<section>\
                        <img src=\"https://cdn.weebcentral.com/manga/1.png\" width=\"800\" height=\"1200\">\
                        <img src=\"/manga/2.png\" width=\"800\" height=\"1200\">\
                        </section>";
        assert_eq!(
            extract_weebcentral_image_urls(fragment, CHAPTER_URL),
            vec![
                "https://cdn.weebcentral.com/manga/1.png".to_string(),
                "https://weebcentral.com/manga/2.png".to_string(),
            ]
        );
    }

    #[test]
    fn extract_weebcentral_image_urls_unescapes_and_dedupes() {
        let fragment = "<img src=\"https://cdn.weebcentral.com/1.png?a=1&amp;b=2\">\
                        <img src=\"https://cdn.weebcentral.com/1.png?a=1&b=2\">";
        assert_eq!(
            extract_weebcentral_image_urls(fragment, CHAPTER_URL),
            vec!["https://cdn.weebcentral.com/1.png?a=1&b=2".to_string()]
        );
    }

    #[test]
    fn extract_weebcentral_image_urls_returns_empty_for_a_pageless_fragment() {
        let fragment = "<section><p>Chapter not available</p>\
                        <img src=\"data:image/gif;base64,R0lGOD\"><img alt=\"no src\"></section>";
        assert!(extract_weebcentral_image_urls(fragment, CHAPTER_URL).is_empty());
    }
}
