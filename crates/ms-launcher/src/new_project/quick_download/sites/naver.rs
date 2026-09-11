/*
File: src/launcher/new_project/quick_download/sites/naver.rs

Purpose:
Chapter resolver for comic.naver.com.

Key constants:
- NAVER_CONTENT_IMAGE_ID_PREFIX

Key functions:
- comic_naver_plan()
- parse_naver_images()
- naver_image_order()

Notes:
Naver serves the episode images inline in the viewer HTML, tagged `id="content_image_<n>"`.
The episode path `/webtoon/{titleId}/{no}/` alone does NOT identify them: the episode's own
202x120 list thumbnail lives under the same path and would otherwise be downloaded as a page.
Pages are reordered by their `IMAG<a>_<b>` file name, because the DOM order is not the reading
order. The CDN ignores a `?type=` parameter here, so the linked URL already is the original.
*/

use super::super::html::{extract_html_tags, get_html_attr};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url, query_param};

/// `id` prefix the viewer puts on every episode page image (`content_image_0`, ...); site
/// chrome and the episode thumbnail carry no such id.
const NAVER_CONTENT_IMAGE_ID_PREFIX: &str = "content_image";

/// Builds the download plan for a comic.naver.com viewer URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the URL lacks `titleId`/`no`, when the page fetch
/// fails, or when the page carries no episode images.
pub(crate) fn comic_naver_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let title_id = query_param(url, "titleId").ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.naver_no_titleid_error").to_string(),
        log_message: format!("naver url '{url}' has no titleId"),
    })?;
    let episode_no = query_param(url, "no").ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.naver_no_chapter_error").to_string(),
        log_message: format!("naver url '{url}' has no no parameter"),
    })?;
    let html = fetch_text(url, None)?;
    let image_urls = parse_naver_images(&html, url, &title_id, &episode_no);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "naver chapter '{url}' has no <img id=\"{NAVER_CONTENT_IMAGE_ID_PREFIX}*\"> \
                 under '/webtoon/{title_id}/{episode_no}/'"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

/// Collects the episode pages of a Naver viewer document in reading order, deduplicated and
/// resolved against `base_url`.
///
/// An image counts only when it carries the viewer's content id AND sits under the episode's
/// own `/webtoon/{title_id}/{episode_no}/` path: the id keeps the episode list thumbnail out,
/// the path keeps images of neighbouring episodes out. Returns an empty vector when the
/// document holds no such image.
fn parse_naver_images(
    html: &str,
    base_url: &str,
    title_id: &str,
    episode_no: &str,
) -> Vec<String> {
    let episode_path = format!("/webtoon/{title_id}/{episode_no}/");
    let mut items = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let is_content_image = get_html_attr(tag.attrs, "id")
            .is_some_and(|id| id.starts_with(NAVER_CONTENT_IMAGE_ID_PREFIX));
        if !is_content_image {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        if !src.contains(&episode_path) {
            continue;
        }
        let normalized = normalize_network_url(src, base_url);
        let order = naver_image_order(&normalized);
        items.push((order, normalized));
    }
    items.sort_by_key(|(order, _)| *order);
    dedupe_preserve(items.into_iter().map(|(_, url)| url).collect())
}

/// Reading-order key parsed from a Naver image file name (`...IMAG<first>_<second>.jpg`).
/// Missing or unparsable parts sort as `0`.
fn naver_image_order(url: &str) -> (u32, u32) {
    let file_name = url.rsplit('/').next().unwrap_or_default();
    let name = file_name.split('.').next().unwrap_or_default();
    let digits = name
        .rsplit_once("IMAG")
        .map(|(_, right)| right)
        .unwrap_or_default();
    let mut parts = digits.split('_');
    let first = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let second = parts
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    (first, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWER_URL: &str = "https://comic.naver.com/webtoon/detail?titleId=837504&no=72&week=thu";
    const CDN: &str = "https://image-comic.pstatic.net/webtoon/837504/72";

    #[test]
    fn parse_naver_images_skips_the_episode_thumbnail() {
        // The 202x120 list thumbnail of the very same episode shares its CDN path and used to
        // enter the plan as the first page; only the content id tells them apart.
        let html = format!(
            "<img src=\"{CDN}/thumbnail_202x120_ec5fc23e.jpg\" alt=\"72\" />\
             <img src=\"{CDN}/2026_IMAG01_1.jpg\" alt=\"comic content\" id=\"content_image_0\">\
             <img src=\"{CDN}/2026_IMAG01_2.jpg\" alt=\"comic content\" id=\"content_image_1\">"
        );
        assert_eq!(
            parse_naver_images(&html, VIEWER_URL, "837504", "72"),
            vec![
                format!("{CDN}/2026_IMAG01_1.jpg"),
                format!("{CDN}/2026_IMAG01_2.jpg"),
            ]
        );
    }

    #[test]
    fn parse_naver_images_ignores_other_episodes() {
        let html = format!(
            "<img src=\"https://image-comic.pstatic.net/webtoon/837504/71/2026_IMAG01_1.jpg\" \
             id=\"content_image_0\">\
             <img src=\"{CDN}/2026_IMAG01_1.jpg\" id=\"content_image_0\">"
        );
        assert_eq!(
            parse_naver_images(&html, VIEWER_URL, "837504", "72"),
            vec![format!("{CDN}/2026_IMAG01_1.jpg")]
        );
    }

    #[test]
    fn parse_naver_images_sorts_numerically_not_lexically() {
        let html = format!(
            "<img src=\"{CDN}/2026_IMAG01_10.jpg\" id=\"content_image_9\">\
             <img src=\"{CDN}/2026_IMAG01_2.jpg\" id=\"content_image_1\">\
             <img src=\"{CDN}/2026_IMAG02_1.jpg\" id=\"content_image_23\">"
        );
        assert_eq!(
            parse_naver_images(&html, VIEWER_URL, "837504", "72"),
            vec![
                format!("{CDN}/2026_IMAG01_2.jpg"),
                format!("{CDN}/2026_IMAG01_10.jpg"),
                format!("{CDN}/2026_IMAG02_1.jpg"),
            ]
        );
    }

    #[test]
    fn parse_naver_images_returns_nothing_without_content_images() {
        let html = format!("<img src=\"{CDN}/thumbnail_202x120_ec5fc23e.jpg\" alt=\"72\" />");
        assert!(parse_naver_images(&html, VIEWER_URL, "837504", "72").is_empty());
    }

    #[test]
    fn naver_image_order_reads_both_indices() {
        assert_eq!(naver_image_order("https://x/y_IMAG03_7.jpg"), (3, 7));
        assert_eq!(naver_image_order("https://x/thumbnail_202x120_a.jpg"), (0, 0));
    }
}
