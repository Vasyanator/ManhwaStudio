/*
File: src/launcher/new_project/quick_download/sites/mangaread.rs

Purpose:
Chapter resolver for mangaread.org.

Key constants:
- MANGAREAD_CONTENT_MARKER, MANGAREAD_CONTENT_END

Key functions:
- mangaread_plan()
- is_mangaread_chapter_url()
- mangaread_content_slice()
- extract_mangaread_image_urls()

Notes:
Hosts served: `mangaread.org`, with or without `www.`.
No special request header is needed for the chapter page; the page URL is sent as
`Referer` for the images because that is what a browser does on this site.
Only chapter URLs (`/manga/<series>/<chapter>/`) are resolvable; a series URL is rejected
instead of being fetched. The page container nests one wrapper `div` per page, so its end
is the following `entry-header` block, not the first closing `div`.
*/

use super::super::html::{extract_html_tags, get_html_attr, html_unescape};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url, path_segments};

/// Class of the container holding the page images.
const MANGAREAD_CONTENT_MARKER: &str = "reading-content";
/// Marker of the block that follows the page container and therefore ends it.
const MANGAREAD_CONTENT_END: &str = "entry-header";

/// Builds the download plan for a mangaread.org chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when `url` is not a chapter URL, when the page fetch
/// fails, or when the page container holds no `image-<n>` images.
pub(crate) fn mangaread_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    if !is_mangaread_chapter_url(url) {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
            log_message: format!(
                "mangaread url '{url}' is not a chapter url of shape /manga/<series>/<chapter>"
            ),
        });
    }
    let html = fetch_text(url, None)?;
    let image_urls = extract_mangaread_image_urls(&html, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "mangaread chapter '{url}' has no <img id=\"image-<n>\"> inside a '{MANGAREAD_CONTENT_MARKER}' container"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(url.to_string()),
    })
}

/// Returns `true` for `/manga/<series>/<chapter>`, the only shape that carries page
/// images; a bare `/manga/<series>` series URL is not one.
fn is_mangaread_chapter_url(url: &str) -> bool {
    let segments = path_segments(url);
    // A chapter needs the `manga` section segment plus the series and chapter slugs.
    segments.len() >= 3 && segments[0] == "manga"
}

/// Returns the inner HTML of the page container: everything between the end of the
/// opening tag carrying `MANGAREAD_CONTENT_MARKER` and the following
/// `MANGAREAD_CONTENT_END` marker, or the end of the document when that marker is absent.
/// Returns `None` when the container itself is missing.
fn mangaread_content_slice(html: &str) -> Option<&str> {
    // The markers are matched on an ASCII-lowercased copy so upper-case markup matches
    // too. `to_ascii_lowercase` maps ASCII bytes onto ASCII bytes and leaves every other
    // byte untouched, so byte lengths - and therefore the indices below - are unchanged.
    let lower = html.to_ascii_lowercase();
    let marker_start = lower.find(MANGAREAD_CONTENT_MARKER)?;
    let content_start = lower[marker_start..].find('>')? + marker_start + 1;
    let content_end = lower[content_start..]
        .find(MANGAREAD_CONTENT_END)
        .map_or(html.len(), |offset| content_start + offset);
    Some(&html[content_start..content_end])
}

/// Collects the page URLs of a mangaread.org chapter page in document order,
/// deduplicated and resolved against `base_url`.
///
/// Only `<img>` elements whose `id` starts with `image-` and that sit inside the page
/// container count; empty and `data:` sources are dropped. Returns an empty vector when
/// the container is absent.
fn extract_mangaread_image_urls(html: &str, base_url: &str) -> Vec<String> {
    let Some(content) = mangaread_content_slice(html) else {
        return Vec::new();
    };
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(content) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(id) = get_html_attr(tag.attrs, "id") else {
            continue;
        };
        if !id.starts_with("image-") {
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

    const CHAPTER_URL: &str = "https://www.mangaread.org/manga/series/chapter-3/";

    #[test]
    fn is_mangaread_chapter_url_separates_chapters_from_series() {
        assert!(is_mangaread_chapter_url(CHAPTER_URL));
        assert!(!is_mangaread_chapter_url(
            "https://www.mangaread.org/manga/series"
        ));
        assert!(!is_mangaread_chapter_url("https://www.mangaread.org/"));
    }

    #[test]
    fn extract_mangaread_image_urls_reads_nested_wrappers_in_order() {
        let html = "<img id=\"image-9\" src=\"/before.jpg\">\
                    <div class=\"reading-content\">\
                    <div class=\"page-break no-gaps\">\
                    <img id=\"image-0\" src=\"\n https://www.mangaread.org/wp/a.jpg \n\">\
                    </div>\
                    <div class=\"page-break no-gaps\">\
                    <img id=\"image-1\" src=\"/wp/b.png\">\
                    </div>\
                    </div>\
                    <div class=\"entry-header\">\
                    <img id=\"image-2\" src=\"/after.jpg\">\
                    </div>";
        assert_eq!(
            extract_mangaread_image_urls(html, CHAPTER_URL),
            vec![
                "https://www.mangaread.org/wp/a.jpg".to_string(),
                "https://www.mangaread.org/wp/b.png".to_string(),
            ]
        );
    }

    #[test]
    fn extract_mangaread_image_urls_ignores_images_without_page_id() {
        let html = "<div class=\"reading-content\">\
                    <img class=\"ad\" src=\"/ad.gif\">\
                    <img id=\"image-0\" src=\"/page.jpg?v=1&amp;q=2\">\
                    </div><div class=\"entry-header\"></div>";
        assert_eq!(
            extract_mangaread_image_urls(html, CHAPTER_URL),
            vec!["https://www.mangaread.org/page.jpg?v=1&q=2".to_string()]
        );
    }

    #[test]
    fn mangaread_content_slice_falls_back_to_document_end() {
        let html = "<div class=\"reading-content\">tail";
        assert_eq!(mangaread_content_slice(html), Some("tail"));
        assert_eq!(mangaread_content_slice("<div class=\"other\">x</div>"), None);
    }

    #[test]
    fn extract_mangaread_image_urls_is_empty_without_container_or_sources() {
        let no_container = "<div class=\"other\"><img id=\"image-0\" src=\"/a.jpg\"></div>";
        assert!(extract_mangaread_image_urls(no_container, CHAPTER_URL).is_empty());
        let placeholder_only = "<div class=\"reading-content\">\
                                <img id=\"image-0\" src=\"data:image/gif;base64,R0lGOD\">\
                                </div><div class=\"entry-header\"></div>";
        assert!(extract_mangaread_image_urls(placeholder_only, CHAPTER_URL).is_empty());
    }
}
