/*
File: src/launcher/new_project/quick_download/sites/naver.rs

Purpose:
Chapter resolver for comic.naver.com.

Key functions:
- comic_naver_plan()
- naver_image_order()

Notes:
Naver serves the episode images inline in the viewer HTML; they are recognized by the
`/webtoon/{titleId}/{no}/` path marker and reordered by their `IMAG<a>_<b>` file name,
because the DOM order is not the reading order.
*/

use super::super::html::{extract_html_tags, get_html_attr};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, normalize_network_url, query_param};

/// Builds the download plan for a comic.naver.com viewer URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the URL lacks `titleId`/`no`, or when the page fetch
/// fails.
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
    let marker = format!("/webtoon/{title_id}/{episode_no}/");
    let mut items = Vec::new();
    for tag in extract_html_tags(&html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        if !src.contains(&marker) {
            continue;
        }
        let normalized = normalize_network_url(src, url);
        let order = naver_image_order(&normalized);
        items.push((order, normalized));
    }
    items.sort_by_key(|(order, _)| *order);
    Ok(SiteDownloadPlan {
        image_urls: dedupe_preserve(items.into_iter().map(|(_, url)| url).collect()),
        referer: None,
    })
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
