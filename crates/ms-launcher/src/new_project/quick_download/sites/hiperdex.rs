/*
File: src/launcher/new_project/quick_download/sites/hiperdex.rs

Purpose:
Chapter resolver for the hiperdex/hipertoon mirror family.

Key functions:
- hiperdex_plan()
- is_hiperdex_chapter_url()
- extract_hiperdex_image_urls()

Notes:
Hosts served: `hiperdex`/`hipertoon`, optionally prefixed with `1st` and optionally
followed by one digit, over `.com`, `.net`, `.info` or `.top` (`hiperdex.com`,
`hipertoon2.net`, `1sthiperdex.top`, ...), each with an optional `www.`. Every mirror
serves its own pages, so the pasted origin is used as is and never rewritten to one
canonical host.
No special request header is needed for the chapter page; the page URL is sent as
`Referer` for the images because that is what a browser does on this site.
Only chapter URLs (`/manga/<series>/<chapter>/`) are resolvable: the chapter list of a
series page lives behind a POST form endpoint this module does not speak.
*/

use super::super::html::{extract_html_tags, get_html_attr, html_unescape};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url, path_segments};

/// Builds the download plan for a hiperdex/hipertoon chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when `url` is not a chapter URL (a series URL is
/// rejected here rather than fetched), when the page fetch fails, or when the page holds
/// no `image-<n>` elements.
pub(crate) fn hiperdex_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    if !is_hiperdex_chapter_url(url) {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
            log_message: format!(
                "hiperdex url '{url}' is not a chapter url of shape /manga/<series>/<chapter>"
            ),
        });
    }
    let html = fetch_text(url, None)?;
    let image_urls = extract_hiperdex_image_urls(&html, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "hiperdex chapter '{url}' has no element with id=\"image-<n>\" carrying src/data-src"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(url.to_string()),
    })
}

/// Returns `true` for `/manga/<series>/<chapter>` and its plural `/mangas/...` form, i.e.
/// for URLs that actually carry page images. A bare `/manga/<series>` series URL is not one.
fn is_hiperdex_chapter_url(url: &str) -> bool {
    let segments = path_segments(url);
    // A chapter needs the section segment plus the series and chapter slugs.
    segments.len() >= 3 && matches!(segments[0].as_str(), "manga" | "mangas")
}

/// Collects the page URLs of a hiperdex chapter page in document order, deduplicated and
/// resolved against `base_url`.
///
/// A page image is any element whose `id` starts with `image-`. Its URL is taken from
/// `data-src` when that attribute carries a value and from `src` otherwise, because the
/// lazy-loading markup parks a `data:` placeholder in `src`. `data:` values are dropped.
fn extract_hiperdex_image_urls(html: &str, base_url: &str) -> Vec<String> {
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end {
            continue;
        }
        let Some(id) = get_html_attr(tag.attrs, "id") else {
            continue;
        };
        if !id.starts_with("image-") {
            continue;
        }
        let Some(source) = hiperdex_image_source(tag.attrs) else {
            continue;
        };
        image_urls.push(normalize_network_url(&html_unescape(source), base_url));
    }
    dedupe_preserve(image_urls)
}

/// Returns the trimmed, non-empty, non-`data:` value of `data-src` or, failing that, of
/// `src` inside a raw attribute string.
fn hiperdex_image_source(attrs: &str) -> Option<&str> {
    ["data-src", "src"].into_iter().find_map(|name| {
        get_html_attr(attrs, name)
            .map(str::trim)
            .filter(|value| !value.is_empty() && !value.starts_with("data:"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_hiperdex_chapter_url_separates_chapters_from_series() {
        assert!(is_hiperdex_chapter_url(
            "https://hiperdex.com/manga/series/chapter-7/"
        ));
        assert!(is_hiperdex_chapter_url(
            "https://1sthipertoon2.top/mangas/series/chapter-7"
        ));
        assert!(!is_hiperdex_chapter_url("https://hiperdex.com/manga/series/"));
        assert!(!is_hiperdex_chapter_url(
            "https://hiperdex.com/manga-artist/name/x"
        ));
        assert!(!is_hiperdex_chapter_url("https://hiperdex.com/"));
    }

    #[test]
    fn extract_hiperdex_image_urls_keeps_order_and_resolves_relatives() {
        let html = "<div class=\"page-break\">\
                    <img id=\"image-0\" src=\"https://cdn.hiperdex.com/a.jpg\">\
                    </div><div class=\"page-break\">\
                    <img id=\"image-1\" src=\"/wp-content/b.png\">\
                    </div>";
        assert_eq!(
            extract_hiperdex_image_urls(html, "https://hiperdex.com/manga/s/chapter-1/"),
            vec![
                "https://cdn.hiperdex.com/a.jpg".to_string(),
                "https://hiperdex.com/wp-content/b.png".to_string(),
            ]
        );
    }

    #[test]
    fn extract_hiperdex_image_urls_prefers_data_src_over_placeholder() {
        let html = "<img id=\"image-0\" src=\"data:image/gif;base64,R0lGOD\" \
                    data-src=\"\n https://cdn.hiperdex.com/real.webp \n\">";
        assert_eq!(
            extract_hiperdex_image_urls(html, "https://hiperdex.net/manga/s/c/"),
            vec!["https://cdn.hiperdex.com/real.webp".to_string()]
        );
    }

    #[test]
    fn extract_hiperdex_image_urls_ignores_non_page_elements() {
        let html = "<img id=\"site-logo\" src=\"/logo.png\">\
                    <img src=\"/banner.png\">\
                    <img id=\"image-0\" src=\"/page.jpg\">";
        assert_eq!(
            extract_hiperdex_image_urls(html, "https://hiperdex.com/manga/s/c/"),
            vec!["https://hiperdex.com/page.jpg".to_string()]
        );
    }

    #[test]
    fn extract_hiperdex_image_urls_unescapes_and_dedupes() {
        let html = "<img id=\"image-0\" src=\"https://cdn.x/a.jpg?w=1&amp;h=2\">\
                    <img id=\"image-1\" src=\"https://cdn.x/a.jpg?w=1&h=2\">";
        assert_eq!(
            extract_hiperdex_image_urls(html, "https://hiperdex.com/manga/s/c/"),
            vec!["https://cdn.x/a.jpg?w=1&h=2".to_string()]
        );
    }

    #[test]
    fn extract_hiperdex_image_urls_returns_empty_without_usable_sources() {
        let html = "<img id=\"image-0\" src=\"data:image/gif;base64,R0lGOD\">\
                    <img id=\"image-1\" alt=\"missing src\">\
                    <p>no images here</p>";
        assert!(extract_hiperdex_image_urls(html, "https://hiperdex.com/manga/s/c/").is_empty());
    }
}
