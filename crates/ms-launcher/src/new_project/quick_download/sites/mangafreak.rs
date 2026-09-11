/*
File: src/launcher/new_project/quick_download/sites/mangafreak.rs

Purpose:
Chapter resolver for mangafreak.

Key functions:
- mangafreak_plan()
- parse_mangafreak_images()

Notes:
Hosts served: mangafreak.me with any of its rotating front-end prefixes — `ww2.`, `ww3.` (and
further `ww<digit>.` mirrors) as well as `www.`. Chapter URLs look like
`/Read1_<Manga_Slug>_<number>`. Page images are anchored on the fixed CDN prefix
`https://images.mangafreak.me/mangas/`, on a host different from the page host; filtering on
that prefix is what keeps site chrome out, and a same-host (relative) source can never be a
page image.
*/

use super::super::html::{extract_html_tags, get_html_attr};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url};

/// Host and path prefix every mangafreak page image starts with, scheme excluded so that both
/// `https://`, `http://` and protocol-relative sources are recognized.
const CDN_HOST_PATH: &str = "images.mangafreak.me/mangas/";

/// Builds the download plan for a mangafreak chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the chapter page cannot be fetched or read, or when it
/// carries no image on the mangafreak CDN prefix.
pub(crate) fn mangafreak_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let image_urls = parse_mangafreak_images(&html, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!("mangafreak page '{url}' has no '{CDN_HOST_PATH}' image sources"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        // Pages live on a different host than the chapter page, so they are requested with the
        // chapter page as `Referer`, the way a browser rendering that page would.
        referer: Some(url.to_string()),
    })
}

/// Extracts the page images of a mangafreak chapter page in document (reading) order: every
/// `<img src>` that resolves against `base` to a URL on the mangafreak CDN prefix,
/// deduplicated. Site chrome, which is served from the page host, is dropped.
fn parse_mangafreak_images(html: &str, base: &str) -> Vec<String> {
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        if src.is_empty() {
            continue;
        }
        let normalized = normalize_network_url(src, base);
        if is_cdn_page_image(&normalized) {
            image_urls.push(normalized);
        }
    }
    dedupe_preserve(image_urls)
}

/// Returns `true` when an absolute URL points at the mangafreak page CDN.
fn is_cdn_page_image(url: &str) -> bool {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .is_some_and(|rest| rest.starts_with(CDN_HOST_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTER_URL: &str = "https://ww3.mangafreak.me/Read1_Onepunch_Man_1";

    #[test]
    fn parse_mangafreak_images_keeps_cdn_sources_in_order() {
        let html = "<div id=\"gohere\">\
                    <img src=\"https://images.mangafreak.me/mangas/onepunch/001.jpg\">\
                    <img src='https://images.mangafreak.me/mangas/onepunch/002.jpg'>\
                    <img src=\"//images.mangafreak.me/mangas/onepunch/003.jpg\">\
                    </div>";
        assert_eq!(
            parse_mangafreak_images(html, CHAPTER_URL),
            vec![
                "https://images.mangafreak.me/mangas/onepunch/001.jpg".to_string(),
                "https://images.mangafreak.me/mangas/onepunch/002.jpg".to_string(),
                "https://images.mangafreak.me/mangas/onepunch/003.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn parse_mangafreak_images_drops_site_chrome() {
        let html = "<img src=\"/images/logo.png\">\
                    <img src=\"https://ww3.mangafreak.me/images/banner.jpg\">\
                    <img src=\"https://images.mangafreak.me/covers/onepunch.jpg\">\
                    <img src=\"\">\
                    <img alt=\"no source\">\
                    <img src=\"https://images.mangafreak.me/mangas/onepunch/001.jpg\">";
        assert_eq!(
            parse_mangafreak_images(html, CHAPTER_URL),
            vec!["https://images.mangafreak.me/mangas/onepunch/001.jpg".to_string()]
        );
    }

    #[test]
    fn parse_mangafreak_images_dedupes_repeated_sources() {
        let html = "<img src=\"https://images.mangafreak.me/mangas/onepunch/001.jpg\">\
                    <img src=\"https://images.mangafreak.me/mangas/onepunch/001.jpg\">";
        assert_eq!(
            parse_mangafreak_images(html, CHAPTER_URL),
            vec!["https://images.mangafreak.me/mangas/onepunch/001.jpg".to_string()]
        );
    }

    #[test]
    fn parse_mangafreak_images_returns_empty_for_a_page_without_cdn_images() {
        let html = "<html><img src=\"/a.jpg\"></html>";
        assert!(parse_mangafreak_images(html, CHAPTER_URL).is_empty());
    }
}
