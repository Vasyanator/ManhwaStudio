/*
File: src/launcher/new_project/quick_download/sites/hentai2read.rs

Purpose:
Chapter resolver for hentai2read.com (adult site).

Key functions:
- hentai2read_plan()
- parse_hentai2read_images()

Notes:
Hosts served: hentai2read.com (with or without a `www.` prefix). A chapter lives at
`/<title-slug>/<chapter>/` and embeds its page list as the `'images'` key of an inline script
object — an object key in single quotes, not a `var` assignment. The entries are bare
root-relative file paths, and the images are NOT served from the site root: they live on the
fixed host `https://hentaicdn.com/hentai`, to which each entry is appended. Array order is
the reading order. Series URLs (`/<title-slug>/`) are not resolved here — they simply carry
no `'images'` list.

The scan for the script array uses the shared `find_js_array_literal` of `html.rs`; only the
marker name and the CDN join below are site-specific.
*/

use super::super::html::find_js_array_literal;
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use serde_json::Value;

/// Image host of hentai2read, deliberately different from the site root: the `'images'`
/// entries are appended to this prefix verbatim.
const HENTAI2READ_CDN: &str = "https://hentaicdn.com/hentai";
/// Site root sent as `Referer` with the CDN requests.
const HENTAI2READ_ROOT: &str = "https://hentai2read.com";

/// Builds the download plan for a hentai2read.com chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the page fetch fails or the page carries no usable
/// `'images'` list (which is also what a non-chapter URL looks like).
pub(crate) fn hentai2read_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let html = fetch_text(url, None)?;
    let image_urls = parse_hentai2read_images(&html);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "hentai2read page '{url}' yielded no images; expected an inline \
                 `'images' : [...]` array of file paths"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(format!("{HENTAI2READ_ROOT}/")),
    })
}

/// Reads the ordered page URLs out of a hentai2read chapter page.
///
/// Appends every entry of the `'images'` array to the fixed CDN prefix, in array order.
/// Returns an empty vector when the array is missing, is not valid JSON, or holds no usable
/// entry.
fn parse_hentai2read_images(html: &str) -> Vec<String> {
    let Some(literal) = find_js_array_literal(html, "'images'") else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_str::<Value>(literal) else {
        return Vec::new();
    };
    let Some(entries) = entries.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(Value::as_str)
        .map(|path| join_cdn_path(HENTAI2READ_CDN, path))
        .collect()
}

/// Joins a CDN base to a page path with exactly one separating slash, whichever side already
/// carries one.
fn join_cdn_path(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-written stand-in for a chapter page: the reader config object with three pages in
    /// a deliberately unsorted order.
    const CHAPTER_HTML: &str = r#"<html><body>
<div data-mid="12" data-cid="34">reader</div>
<script>
    var gData = {
        'chapterId' : 34,
        'images' : ["/manga/example/1/3.jpg","/manga/example/1/1.jpg","/manga/example/1/2.jpg"],
        'pageCount' : 3
    };
</script>
</body></html>"#;

    #[test]
    fn parses_images_onto_the_fixed_cdn_in_order() {
        assert_eq!(
            parse_hentai2read_images(CHAPTER_HTML),
            vec![
                "https://hentaicdn.com/hentai/manga/example/1/3.jpg".to_string(),
                "https://hentaicdn.com/hentai/manga/example/1/1.jpg".to_string(),
                "https://hentaicdn.com/hentai/manga/example/1/2.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn cdn_join_never_doubles_or_drops_the_slash() {
        let html = r#"<script>'images' : ["a.jpg","/b.jpg"],</script>"#;
        assert_eq!(
            parse_hentai2read_images(html),
            vec![
                "https://hentaicdn.com/hentai/a.jpg".to_string(),
                "https://hentaicdn.com/hentai/b.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn missing_images_key_yields_nothing() {
        let html = "<html><body><p>Chapter not found</p></body></html>";
        assert!(parse_hentai2read_images(html).is_empty());
    }

    #[test]
    fn empty_images_array_yields_nothing() {
        assert!(parse_hentai2read_images("<script>'images' : [],</script>").is_empty());
    }

    #[test]
    fn a_distant_array_is_not_mistaken_for_the_marked_one() {
        let html = r#"<script>
            'images' : null,
            'thumbnails' : ["/ads/1.jpg"]
        </script>"#;
        assert!(parse_hentai2read_images(html).is_empty());
    }

    #[test]
    fn bracket_inside_a_quoted_path_does_not_end_the_array() {
        let html = r#"<script>'images' : ["/a]b.jpg","/c.jpg"],</script>"#;
        assert_eq!(
            parse_hentai2read_images(html),
            vec![
                "https://hentaicdn.com/hentai/a]b.jpg".to_string(),
                "https://hentaicdn.com/hentai/c.jpg".to_string(),
            ]
        );
    }
}
