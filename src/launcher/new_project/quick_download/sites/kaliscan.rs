/*
File: src/launcher/new_project/quick_download/sites/kaliscan.rs

Purpose:
Chapter resolver for kaliscan.me.

Key functions:
- kaliscan_plan()
- parse_kaliscan_images()

Notes:
Hosts served: kaliscan.me. A chapter lives at `/manga/<slug>/chapter-<number>` and embeds the
whole page list as ONE quoted JS string, `var chapImages = "url,url,..."`, not as an array;
the order inside that string is the reading order. Series URLs (`/manga/<slug>`) are not
resolved here — they simply carry no `var chapImages`.

The scan for the script string uses the shared `find_js_string_literal` of `html.rs`; only the
marker name and the comma splitting below are site-specific.
*/

use super::super::html::find_js_string_literal;
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::normalize_network_url;

/// Site root a (normally absolute) page URL is resolved against, and the `Referer` the image
/// host expects.
const KALISCAN_ROOT: &str = "https://kaliscan.me";

/// Builds the download plan for a kaliscan.me chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the page fetch fails or the page carries no usable
/// `var chapImages` string (which is also what a non-chapter URL looks like).
pub(crate) fn kaliscan_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let image_urls = parse_kaliscan_images(&html);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "kaliscan page '{url}' yielded no images; expected an inline \
                 `var chapImages = \"url,url,...\"` string"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(format!("{KALISCAN_ROOT}/")),
    })
}

/// Reads the ordered page URLs out of a kaliscan chapter page.
///
/// Splits the single `var chapImages` string on commas, trims each item and drops empty ones
/// (the list is normally comma-terminated). Items are already absolute URLs; a relative one
/// is resolved against the site root. Returns an empty vector when the string is missing or
/// holds no usable item.
fn parse_kaliscan_images(html: &str) -> Vec<String> {
    let Some(list) = find_js_string_literal(html, "var chapImages") else {
        return Vec::new();
    };
    list.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(|item| normalize_network_url(item, KALISCAN_ROOT))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-written stand-in for a chapter page: three pages in a deliberately unsorted order,
    /// padded with spaces and closed by the trailing comma the site emits.
    const CHAPTER_HTML: &str = r#"<html><body>
<div id="chapter-images">loading</div>
<script>
    var bookId = 7;
    var chapImages = "https://cdn.example.net/a/03.jpg, https://cdn.example.net/a/01.jpg,https://cdn.example.net/a/02.jpg,";
</script>
</body></html>"#;

    #[test]
    fn parses_comma_separated_list_in_order() {
        assert_eq!(
            parse_kaliscan_images(CHAPTER_HTML),
            vec![
                "https://cdn.example.net/a/03.jpg".to_string(),
                "https://cdn.example.net/a/01.jpg".to_string(),
                "https://cdn.example.net/a/02.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn relative_items_are_resolved_against_the_site_root() {
        let html = r#"<script>var chapImages = "/uploads/a.jpg";</script>"#;
        assert_eq!(
            parse_kaliscan_images(html),
            vec!["https://kaliscan.me/uploads/a.jpg".to_string()]
        );
    }

    #[test]
    fn missing_variable_yields_nothing() {
        let html = "<html><body><p>Chapter not found</p></body></html>";
        assert!(parse_kaliscan_images(html).is_empty());
    }

    #[test]
    fn empty_and_comma_only_strings_yield_nothing() {
        assert!(parse_kaliscan_images(r#"<script>var chapImages = "";</script>"#).is_empty());
        assert!(parse_kaliscan_images(r#"<script>var chapImages = " , ";</script>"#).is_empty());
    }

    #[test]
    fn a_distant_string_is_not_mistaken_for_the_marked_one() {
        let html = r#"<script>
            var chapImages = null;
            var unrelated = "https://ads.example.net/a.jpg";
        </script>"#;
        assert!(parse_kaliscan_images(html).is_empty());
    }
}
