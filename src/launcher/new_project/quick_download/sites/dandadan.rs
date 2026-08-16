/*
File: src/launcher/new_project/quick_download/sites/dandadan.rs

Purpose:
Chapter resolver for dandadan.net.

Key functions:
- dandadan_plan()
- parse_dandadan_images()
- parse_dandadan_figure_images()
- parse_dandadan_aligncenter_images()

Notes:
Hosts served: dandadan.net, www.dandadan.net and its numbered mirror subdomains (the bare host
currently redirects to `w6.dandadan.net`). The site hosts a single series, and chapter URLs
look like `/manga/dandadan-chapter-<number>/`. Extraction has two tiers: the `<figure>`
container is the primary marker and carries one page image each; when a chapter page ships no
figure at all, the pages are `<img>` elements with the `aligncenter` class instead.
*/

use super::super::html::{extract_html_tags, get_html_attr};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url};

/// CSS class marking a page image on chapter pages that ship no `<figure>` container.
const FALLBACK_IMAGE_CLASS: &str = "aligncenter";

/// Builds the download plan for a dandadan.net chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the chapter page cannot be fetched or read, or when
/// neither the `<figure>` tier nor the `aligncenter` tier yields an image.
pub(crate) fn dandadan_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let image_urls = parse_dandadan_images(&html, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "dandadan page '{url}' has neither <figure> images nor <img> with class \
                 '{FALLBACK_IMAGE_CLASS}'"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        // Requested with the chapter page as `Referer`, the way a browser rendering that page
        // would; the pages may be served from a separate media host.
        referer: Some(url.to_string()),
    })
}

/// Extracts the page images of a dandadan chapter page in document (reading) order, trying the
/// `<figure>` tier first and falling back to the `aligncenter` tier when it yields nothing.
/// Sources resolve against `base`; the result is deduplicated, and an unrelated page yields an
/// empty vector rather than an error.
fn parse_dandadan_images(html: &str, base: &str) -> Vec<String> {
    let figure_images = parse_dandadan_figure_images(html, base);
    if !figure_images.is_empty() {
        return figure_images;
    }
    parse_dandadan_aligncenter_images(html, base)
}

/// Collects the first `<img src>` of every `<figure>` container, which is the one page image a
/// figure holds; any further image inside the same figure is a thumbnail or an ad and is
/// skipped. Returns an empty vector when the page has no figure carrying a usable source.
fn parse_dandadan_figure_images(html: &str, base: &str) -> Vec<String> {
    let mut image_urls = Vec::new();
    let mut inside_figure = false;
    let mut figure_image_taken = false;
    for tag in extract_html_tags(html) {
        if tag.name.eq_ignore_ascii_case("figure") {
            inside_figure = !tag.is_end;
            figure_image_taken = false;
            continue;
        }
        if !inside_figure || figure_image_taken || tag.is_end {
            continue;
        }
        if !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        if let Some(src) = usable_image_src(tag.attrs) {
            image_urls.push(normalize_network_url(src, base));
            figure_image_taken = true;
        }
    }
    dedupe_preserve(image_urls)
}

/// Collects the `<img>` elements whose class list holds `aligncenter`, the shape used by
/// chapter pages without `<figure>` containers.
fn parse_dandadan_aligncenter_images(html: &str, base: &str) -> Vec<String> {
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        if !has_class(tag.attrs, FALLBACK_IMAGE_CLASS) {
            continue;
        }
        if let Some(src) = usable_image_src(tag.attrs) {
            image_urls.push(normalize_network_url(src, base));
        }
    }
    dedupe_preserve(image_urls)
}

/// Returns the `src` of an `<img>` when it can be downloaded, i.e. it is present, non-empty
/// and not an inline `data:` placeholder.
fn usable_image_src(attrs: &str) -> Option<&str> {
    let src = get_html_attr(attrs, "src")?;
    if src.is_empty() || src.starts_with("data:") {
        return None;
    }
    Some(src)
}

/// Returns `true` when the raw attribute string carries a `class` list containing
/// `class_name` as a whole whitespace-separated token, so `aligncenter-wide` never matches
/// `aligncenter`.
fn has_class(attrs: &str, class_name: &str) -> bool {
    get_html_attr(attrs, "class")
        .is_some_and(|classes| classes.split_whitespace().any(|token| token == class_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTER_URL: &str = "https://dandadan.net/manga/dandadan-chapter-123/";

    #[test]
    fn parse_dandadan_images_prefers_the_figure_tier() {
        let html = "<img class=\"aligncenter\" src=\"/wp-content/uploads/ignored.jpg\">\
                    <figure class=\"wp-block-image\">\
                    <img src=\"https://cdn.dandadan.net/123/001.jpg\">\
                    </figure>\
                    <figure>\
                    <a href=\"/next\"><img src=\"002.png\"></a>\
                    <img src=\"https://cdn.dandadan.net/123/thumb.jpg\">\
                    </figure>";
        assert_eq!(
            parse_dandadan_images(html, CHAPTER_URL),
            vec![
                "https://cdn.dandadan.net/123/001.jpg".to_string(),
                "https://dandadan.net/manga/dandadan-chapter-123/002.png".to_string(),
            ]
        );
    }

    #[test]
    fn parse_dandadan_images_falls_back_to_aligncenter_images() {
        let html = "<img class=\"custom-logo\" src=\"/logo.png\">\
                    <img decoding=\"async\" class=\"aligncenter\" \
                    src=\"https://cdn.dandadan.net/123/001.jpg\">\
                    <img decoding=\"async\" class=\"size-full aligncenter\" \
                    src=\"/uploads/002.jpg\">\
                    <img class=\"aligncenter-wide\" src=\"/uploads/ad.jpg\">";
        assert_eq!(
            parse_dandadan_images(html, CHAPTER_URL),
            vec![
                "https://cdn.dandadan.net/123/001.jpg".to_string(),
                "https://dandadan.net/uploads/002.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn parse_dandadan_images_falls_back_when_figures_carry_no_source() {
        let html = "<figure><figcaption>Chapter 123</figcaption></figure>\
                    <img class=\"aligncenter\" src=\"https://cdn.dandadan.net/123/001.jpg\">";
        assert_eq!(
            parse_dandadan_images(html, CHAPTER_URL),
            vec!["https://cdn.dandadan.net/123/001.jpg".to_string()]
        );
    }

    #[test]
    fn parse_dandadan_images_skips_placeholders_and_dedupes() {
        let html = "<figure><img src=\"data:image/gif;base64,AAAA\"></figure>\
                    <figure><img src=\"https://cdn.dandadan.net/123/001.jpg\"></figure>\
                    <figure><img src=\"https://cdn.dandadan.net/123/001.jpg\"></figure>";
        assert_eq!(
            parse_dandadan_images(html, CHAPTER_URL),
            vec!["https://cdn.dandadan.net/123/001.jpg".to_string()]
        );
    }

    #[test]
    fn parse_dandadan_images_ignores_images_outside_both_markers() {
        let html = "<div><img src=\"/wp-content/uploads/banner.jpg\"></div>";
        assert!(parse_dandadan_images(html, CHAPTER_URL).is_empty());
        assert!(parse_dandadan_images("<html><body>empty</body></html>", CHAPTER_URL).is_empty());
    }
}
