/*
File: src/launcher/new_project/quick_download/sites/komikcast.rs

Purpose:
Chapter resolver for the komikcast mirror family.

Key constants:
- KOMIKCAST_READER_MARKER, KOMIKCAST_READER_END

Key functions:
- komikcast_plan()
- is_komikcast_chapter_url()
- komikcast_reader_slice()
- extract_komikcast_image_urls()

Notes:
Hosts served: `komikcast` with an optional digit suffix over `.li`, `.la`, `.lol`,
`.com`, `.cz`, `.site`, `.me` and `.moe` (`komikcast.li`, `komikcast02.lol`, ...), each
with an optional `www.`. The pasted mirror origin is used as is.
No special request header is needed for the chapter page; the page URL is sent as
`Referer` for the images because that is what a browser does on this site.
Only chapter URLs (`/chapter/<slug>/`) are resolvable; a series URL is rejected instead
of being fetched.
*/

use super::super::html::{extract_html_tags, get_html_attr, html_unescape};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url, path_segments};

/// Class prefix of the reader container that wraps the page images.
const KOMIKCAST_READER_MARKER: &str = "main-reading-area";
/// End of the reader container: its images are direct children, so the first closing
/// `div` ends the region that may contribute pages.
const KOMIKCAST_READER_END: &str = "</div";

/// Builds the download plan for a komikcast chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when `url` is not a `/chapter/` URL, when the page fetch
/// fails, or when the reader container is missing or holds no images.
pub(crate) fn komikcast_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    if !is_komikcast_chapter_url(url) {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
            log_message: format!("komikcast url '{url}' is not a chapter url of shape /chapter/<slug>/"),
        });
    }
    let html = fetch_text(url, None)?;
    let image_urls = extract_komikcast_image_urls(&html, url);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "komikcast chapter '{url}' has no <img src> inside a '{KOMIKCAST_READER_MARKER}' container"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(url.to_string()),
    })
}

/// Returns `true` for `/chapter/<slug>`, the only shape that carries page images; a
/// `/komik/<slug>` or bare `/<slug>` series URL is not one.
fn is_komikcast_chapter_url(url: &str) -> bool {
    let segments = path_segments(url);
    segments.len() >= 2 && segments[0] == "chapter"
}

/// Returns the inner HTML of the reader container: everything between the end of the
/// opening tag carrying `KOMIKCAST_READER_MARKER` and the first `KOMIKCAST_READER_END`
/// after it. Returns `None` when the marker or the end of its opening tag is missing.
fn komikcast_reader_slice(html: &str) -> Option<&str> {
    // The markers are matched on an ASCII-lowercased copy so upper-case markup matches
    // too. `to_ascii_lowercase` maps ASCII bytes onto ASCII bytes and leaves every other
    // byte untouched, so byte lengths - and therefore the indices below - are unchanged.
    let lower = html.to_ascii_lowercase();
    let marker_start = lower.find(KOMIKCAST_READER_MARKER)?;
    let content_start = lower[marker_start..].find('>')? + marker_start + 1;
    let content_end = lower[content_start..]
        .find(KOMIKCAST_READER_END)
        .map_or(html.len(), |offset| content_start + offset);
    Some(&html[content_start..content_end])
}

/// Collects the page URLs of a komikcast chapter page in document order, deduplicated and
/// resolved against `base_url`.
///
/// Only `<img src>` inside the reader container counts; site chrome outside it is
/// ignored, as are empty and `data:` sources. Returns an empty vector when the container
/// is absent.
fn extract_komikcast_image_urls(html: &str, base_url: &str) -> Vec<String> {
    let Some(reader) = komikcast_reader_slice(html) else {
        return Vec::new();
    };
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(reader) {
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

    const CHAPTER_URL: &str = "https://komikcast.li/chapter/title-chapter-12/";

    #[test]
    fn is_komikcast_chapter_url_separates_chapters_from_series() {
        assert!(is_komikcast_chapter_url(CHAPTER_URL));
        assert!(is_komikcast_chapter_url("https://komikcast02.lol/chapter/slug"));
        assert!(!is_komikcast_chapter_url("https://komikcast.li/komik/title"));
        assert!(!is_komikcast_chapter_url("https://komikcast.li/chapter/"));
        assert!(!is_komikcast_chapter_url("https://komikcast.li/"));
    }

    #[test]
    fn extract_komikcast_image_urls_takes_only_container_images_in_order() {
        let html = "<img src=\"https://komikcast.li/logo.png\">\
                    <div class=\"main-reading-area cursor-pointer\">\
                    <img src=\"https://cdn.komikcast.li/2.jpg\">\
                    <img src=\"https://cdn.komikcast.li/1.jpg\">\
                    </div>\
                    <img src=\"https://komikcast.li/footer.png\">";
        assert_eq!(
            extract_komikcast_image_urls(html, CHAPTER_URL),
            vec![
                "https://cdn.komikcast.li/2.jpg".to_string(),
                "https://cdn.komikcast.li/1.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn komikcast_reader_slice_stops_at_the_first_closing_div() {
        let html = "<div class=\"main-reading-area\">inside</div>outside";
        assert_eq!(komikcast_reader_slice(html), Some("inside"));
        // Without a closing tag the rest of the document is the container.
        let unclosed = "<div class=\"main-reading-area\">tail";
        assert_eq!(komikcast_reader_slice(unclosed), Some("tail"));
    }

    #[test]
    fn extract_komikcast_image_urls_resolves_relatives_and_unescapes() {
        let html = "<div class=\"main-reading-area\">\
                    <img src=\"/img/a.jpg?id=1&amp;v=2\">\
                    <img src=\"//cdn.komikcast.li/b.jpg\">\
                    </div>";
        assert_eq!(
            extract_komikcast_image_urls(html, CHAPTER_URL),
            vec![
                "https://komikcast.li/img/a.jpg?id=1&v=2".to_string(),
                "https://cdn.komikcast.li/b.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn extract_komikcast_image_urls_is_empty_without_container_or_sources() {
        let no_container = "<div class=\"entry-content\"><img src=\"/a.jpg\"></div>";
        assert!(extract_komikcast_image_urls(no_container, CHAPTER_URL).is_empty());
        let empty_container = "<div class=\"main-reading-area\">\
                               <img src=\"data:image/gif;base64,R0lGOD\">\
                               <img alt=\"no src\"></div>";
        assert!(extract_komikcast_image_urls(empty_container, CHAPTER_URL).is_empty());
    }
}
