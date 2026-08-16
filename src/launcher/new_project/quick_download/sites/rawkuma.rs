/*
File: src/launcher/new_project/quick_download/sites/rawkuma.rs

Purpose:
Chapter resolver for rawkuma.

Key functions:
- rawkuma_plan()
- parse_rawkuma_images()

Notes:
Hosts served: rawkuma.net and rawkuma.com. Chapter URLs look like
`/manga/<slug>/chapter-<number>.<chapter-id>`. The chapter page is plain WordPress markup and
its pages are ordinary `<img>` elements — the site template happens to write them with
single-quoted `src='...'`, which is not a contract, so the attribute reader must accept both
quote styles. Site chrome is separated from pages by the WordPress asset prefixes
(`/wp-content/themes/`, `/wp-content/plugins/`, `/wp-includes/`), which never hold uploaded
content, plus the generic "does this look like an image" filter.
*/

use super::super::html::{extract_html_tags, get_html_attr};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, looks_like_image_url, normalize_network_url};

/// WordPress asset prefixes that only ever carry theme/plugin chrome (logos, sprites, icons),
/// never chapter pages.
const CHROME_PATH_MARKERS: [&str; 3] = [
    "/wp-content/themes/",
    "/wp-content/plugins/",
    "/wp-includes/",
];

/// Builds the download plan for a rawkuma chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the chapter page cannot be fetched or read, or when the
/// page carries no image `<img>` source outside the WordPress chrome paths.
pub(crate) fn rawkuma_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let image_urls = parse_rawkuma_images(&html, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!("rawkuma page '{url}' has no content <img> sources"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        // Requested with the chapter page as `Referer`, the way a browser rendering that page
        // would; the pages may be served from a separate media host.
        referer: Some(url.to_string()),
    })
}

/// Extracts the page images of a rawkuma chapter page in document (reading) order: every
/// `<img src>` (either quote style) that resolves against `base` to an image URL outside the
/// WordPress chrome paths, deduplicated. A page without such images yields an empty vector.
fn parse_rawkuma_images(html: &str, base: &str) -> Vec<String> {
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        if src.is_empty() || src.starts_with("data:") {
            continue;
        }
        let normalized = normalize_network_url(src, base);
        if !looks_like_image_url(&normalized) {
            continue;
        }
        if CHROME_PATH_MARKERS
            .iter()
            .any(|marker| normalized.contains(marker))
        {
            continue;
        }
        image_urls.push(normalized);
    }
    dedupe_preserve(image_urls)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTER_URL: &str = "https://rawkuma.net/manga/title/chapter-12.345";

    #[test]
    fn parse_rawkuma_images_reads_both_quote_styles_in_order() {
        let html = "<div id='readerarea'>\
                    <img src='https://cdn.rawkuma.net/title/12/001.jpg'>\
                    <img src=\"https://cdn.rawkuma.net/title/12/002.jpg\">\
                    <img src='/wp-content/uploads/title/003.png'>\
                    </div>";
        assert_eq!(
            parse_rawkuma_images(html, CHAPTER_URL),
            vec![
                "https://cdn.rawkuma.net/title/12/001.jpg".to_string(),
                "https://cdn.rawkuma.net/title/12/002.jpg".to_string(),
                "https://rawkuma.net/wp-content/uploads/title/003.png".to_string(),
            ]
        );
    }

    #[test]
    fn parse_rawkuma_images_drops_theme_chrome_and_non_images() {
        let html = "<img src='/wp-content/themes/rawkuma/img/logo.png'>\
                    <img src=\"/wp-content/plugins/slider/banner.jpg\">\
                    <img src='/wp-includes/images/spinner.gif'>\
                    <img src='/counter.php?id=7'>\
                    <img src=''>\
                    <img src='data:image/gif;base64,AAAA'>\
                    <img alt='no source'>\
                    <img src='https://cdn.rawkuma.net/title/12/001.jpg'>";
        assert_eq!(
            parse_rawkuma_images(html, CHAPTER_URL),
            vec!["https://cdn.rawkuma.net/title/12/001.jpg".to_string()]
        );
    }

    #[test]
    fn parse_rawkuma_images_resolves_relative_sources_and_dedupes() {
        let html = "<img src='001.jpg'><img src='001.jpg'>";
        assert_eq!(
            parse_rawkuma_images(html, CHAPTER_URL),
            vec!["https://rawkuma.net/manga/title/001.jpg".to_string()]
        );
    }

    #[test]
    fn parse_rawkuma_images_returns_empty_for_a_page_without_images() {
        let html = "<html><body>nothing here</body></html>";
        assert!(parse_rawkuma_images(html, CHAPTER_URL).is_empty());
    }
}
