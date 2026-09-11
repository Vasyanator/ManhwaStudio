/*
File: src/launcher/new_project/quick_download/sites/dynastyscans.rs

Purpose:
Chapter resolver for dynasty-scans.com.

Key functions:
- dynastyscans_plan()
- parse_dynastyscans_images()

Notes:
Hosts served: dynasty-scans.com (with or without a `www.` prefix). A chapter lives at
`/chapters/<name>` and embeds `var pages = [...]`, a JSON array whose entries carry an
`image` field holding a site-root-relative path; the array order is the reading order.
Series URLs (`/series/<name>`) are not resolved here — they simply carry no `var pages`.

The scan for the script array uses the shared `find_js_array_literal` of `html.rs`; only the
marker name and the site root below are site-specific.
*/

use super::super::html::find_js_array_literal;
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::normalize_network_url;
use serde_json::Value;

/// Site root the relative page paths of `var pages` are resolved against, and the `Referer`
/// the image host expects.
const DYNASTYSCANS_ROOT: &str = "https://dynasty-scans.com";

/// Builds the download plan for a dynasty-scans.com chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the page fetch fails or the page carries no usable
/// `var pages` array (which is also what a non-chapter URL looks like).
pub(crate) fn dynastyscans_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let image_urls = parse_dynastyscans_images(&html);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "dynasty-scans page '{url}' yielded no images; expected an inline \
                 `var pages = [...]` array of objects with an `image` field"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(format!("{DYNASTYSCANS_ROOT}/")),
    })
}

/// Reads the ordered page URLs out of a dynasty-scans chapter page.
///
/// Every `image` field of the `var pages` array is resolved against the site root, in array
/// order. Entries without a string `image` are skipped. Returns an empty vector when the
/// array is missing, is not valid JSON, or holds no usable entry.
fn parse_dynastyscans_images(html: &str) -> Vec<String> {
    let Some(literal) = find_js_array_literal(html, "var pages") else {
        return Vec::new();
    };
    let Ok(pages) = serde_json::from_str::<Value>(literal) else {
        return Vec::new();
    };
    let Some(pages) = pages.as_array() else {
        return Vec::new();
    };
    pages
        .iter()
        .filter_map(|page| page.get("image"))
        .filter_map(Value::as_str)
        .map(|path| normalize_network_url(path, DYNASTYSCANS_ROOT))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-written stand-in for a chapter page: three pages in a deliberately unsorted
    /// order, one of them lacking the `image` field.
    const CHAPTER_HTML: &str = r#"<html><body>
<h3 id='chapter-title'><b>Example</b></h3>
<script>
    var pages = [{"image":"/system/releases/002.png","width":800},
                 {"width":800},
                 {"image":"/system/releases/001.png","width":800}];
    $(function() { reader.init(); });
</script>
</body></html>"#;

    #[test]
    fn parses_root_relative_pages_in_array_order() {
        assert_eq!(
            parse_dynastyscans_images(CHAPTER_HTML),
            vec![
                "https://dynasty-scans.com/system/releases/002.png".to_string(),
                "https://dynasty-scans.com/system/releases/001.png".to_string(),
            ]
        );
    }

    #[test]
    fn absolute_image_paths_pass_through_unchanged() {
        let html = r#"<script>var pages = [{"image":"https://cdn.example.net/a.png"}];</script>"#;
        assert_eq!(
            parse_dynastyscans_images(html),
            vec!["https://cdn.example.net/a.png".to_string()]
        );
    }

    #[test]
    fn missing_pages_array_yields_nothing() {
        let html = "<html><body><p>Chapter not found</p></body></html>";
        assert!(parse_dynastyscans_images(html).is_empty());
    }

    #[test]
    fn empty_pages_array_yields_nothing() {
        assert!(parse_dynastyscans_images("<script>var pages = [];</script>").is_empty());
    }

    #[test]
    fn a_distant_array_is_not_mistaken_for_the_marked_one() {
        let html = r#"<script>
            var pages = null;
            var unrelated = [{"image":"/ads/1.png"}];
        </script>"#;
        assert!(parse_dynastyscans_images(html).is_empty());
    }

    #[test]
    fn bracket_inside_a_quoted_path_does_not_end_the_array() {
        let html = r#"<script>var pages = [{"image":"/a]b.png"},{"image":"/c.png"}];</script>"#;
        assert_eq!(
            parse_dynastyscans_images(html),
            vec![
                "https://dynasty-scans.com/a]b.png".to_string(),
                "https://dynasty-scans.com/c.png".to_string(),
            ]
        );
    }
}
