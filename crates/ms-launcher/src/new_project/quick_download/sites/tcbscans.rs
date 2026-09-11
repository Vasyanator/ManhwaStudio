/*
File: src/launcher/new_project/quick_download/sites/tcbscans.rs

Purpose:
Chapter resolver for the tcbscans mirror family.

Key functions:
- tcbscans_plan()
- parse_tcbscans_images()

Notes:
Hosts served: tcbscans.com, tcbscans.me, onepiecechapters.com, onepiecechapters.me and the
backup mirror tcb-backup.bihar-mirchi.com / tcb-backup.bihar-mirchi.me. Chapter URLs look like
`/chapters/<id>/<slug>`. All mirrors ship identical markup, and page images are the `<img>`
elements carrying the `fixed-ratio-content` class; every other image on the page is site
chrome. No host is hardcoded here: sources resolve against the URL the user pasted, which is
what lets one module serve every mirror.
*/

use super::super::html::{extract_html_tags, get_html_attr};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url};

/// CSS class that marks a page image on a tcbscans chapter page.
const PAGE_IMAGE_CLASS: &str = "fixed-ratio-content";

/// Builds the download plan for a chapter URL on any tcbscans mirror.
///
/// # Errors
/// Returns `QuickDownloadError` when the chapter page cannot be fetched or read, or when it
/// carries no `fixed-ratio-content` image.
pub(crate) fn tcbscans_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let image_urls = parse_tcbscans_images(&html, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "tcbscans page '{url}' has no <img> with class '{PAGE_IMAGE_CLASS}'"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        // Pages are served from a CDN host, so the images are requested with the chapter page
        // as `Referer`, the way a browser rendering that page would.
        referer: Some(url.to_string()),
    })
}

/// Extracts the page images of a tcbscans chapter page in document (reading) order: every
/// `<img>` whose class list holds `fixed-ratio-content`, resolved against `base` and
/// deduplicated. Empty and `data:` sources are dropped; an unrelated page yields an empty
/// vector rather than an error.
fn parse_tcbscans_images(html: &str, base: &str) -> Vec<String> {
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        if !has_class(tag.attrs, PAGE_IMAGE_CLASS) {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        if src.is_empty() || src.starts_with("data:") {
            continue;
        }
        image_urls.push(normalize_network_url(src, base));
    }
    dedupe_preserve(image_urls)
}

/// Returns `true` when the raw attribute string carries a `class` list containing
/// `class_name` as a whole whitespace-separated token, so `fixed-ratio-content-teaser`
/// never matches `fixed-ratio-content`.
fn has_class(attrs: &str, class_name: &str) -> bool {
    get_html_attr(attrs, "class")
        .is_some_and(|classes| classes.split_whitespace().any(|token| token == class_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTER_URL: &str = "https://tcbscans.me/chapters/1234/manga-chapter-5";

    #[test]
    fn parse_tcbscans_images_keeps_marked_images_in_order() {
        let html = "<div class=\"flex\">\
                    <img class=\"fixed-ratio-content\" src=\"https://cdn.example/a.jpg\">\
                    <img class=\"lazyload fixed-ratio-content\" src=\"/files/b.png\">\
                    <img class='fixed-ratio-content' src='c.webp'>\
                    </div>";
        assert_eq!(
            parse_tcbscans_images(html, CHAPTER_URL),
            vec![
                "https://cdn.example/a.jpg".to_string(),
                "https://tcbscans.me/files/b.png".to_string(),
                "https://tcbscans.me/chapters/1234/c.webp".to_string(),
            ]
        );
    }

    #[test]
    fn parse_tcbscans_images_drops_chrome_and_lookalike_classes() {
        let html = "<img class=\"site-logo\" src=\"/static/logo.png\">\
                    <img src=\"/static/banner.jpg\">\
                    <img class=\"fixed-ratio-content-teaser\" src=\"/static/teaser.jpg\">\
                    <img class=\"fixed-ratio-content\" src=\"\">\
                    <img class=\"fixed-ratio-content\" src=\"data:image/gif;base64,AAAA\">\
                    <img class=\"fixed-ratio-content\" src=\"https://cdn.example/page-1.jpg\">";
        assert_eq!(
            parse_tcbscans_images(html, CHAPTER_URL),
            vec!["https://cdn.example/page-1.jpg".to_string()]
        );
    }

    #[test]
    fn parse_tcbscans_images_dedupes_repeated_sources() {
        let html = "<img class=\"fixed-ratio-content\" src=\"https://cdn.example/a.jpg\">\
                    <img class=\"fixed-ratio-content\" src=\"https://cdn.example/a.jpg\">";
        assert_eq!(
            parse_tcbscans_images(html, CHAPTER_URL),
            vec!["https://cdn.example/a.jpg".to_string()]
        );
    }

    #[test]
    fn parse_tcbscans_images_resolves_against_the_pasted_mirror() {
        // The mirror is taken from the input URL, not hardcoded.
        let html = "<img class=\"fixed-ratio-content\" src=\"/files/a.jpg\">";
        assert_eq!(
            parse_tcbscans_images(html, "https://onepiecechapters.com/chapters/9/x"),
            vec!["https://onepiecechapters.com/files/a.jpg".to_string()]
        );
    }

    #[test]
    fn parse_tcbscans_images_returns_empty_for_a_page_without_markers() {
        let html = "<html><body>no images</body></html>";
        assert!(parse_tcbscans_images(html, CHAPTER_URL).is_empty());
    }
}
