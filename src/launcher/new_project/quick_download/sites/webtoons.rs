/*
File: src/launcher/new_project/quick_download/sites/webtoons.rs

Purpose:
Chapter resolver for webtoons.com (including the mobile host and Canvas/Challenge series).

Key functions:
- webtoons_plan()

Notes:
A series URL is resolved to its latest episode through the mobile episode-list API; a
`/viewer` URL is used as is. Viewer images need the webtoons.com `Referer`.
*/

use super::super::html::{extract_html_tags, get_html_attr};
use super::super::http::{fetch_json_value, fetch_text};
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{
    dedupe_preserve, looks_like_image_url, normalize_network_url, path_contains, query_param,
};
use serde_json::Value;

/// Builds the download plan for a webtoons.com viewer or series URL.
///
/// # Errors
/// Returns `QuickDownloadError` when a series URL has no `title_no`, when the episode API
/// response is malformed, when a fetch fails, or when the viewer page yields no images.
pub(crate) fn webtoons_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = if path_contains(url, "/viewer") {
        url.to_string()
    } else {
        let title_no = query_param(url, "title_no").ok_or_else(|| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.webtoons_no_titleno_error").to_string(),
            log_message: format!("webtoons url '{url}' has no title_no"),
        })?;
        let webtoon_type = if url.contains("/canvas/") || url.contains("/challenge/") {
            "canvas"
        } else {
            "webtoon"
        };
        let api_url = format!(
            "https://m.webtoons.com/api/v1/{webtoon_type}/{title_no}/episodes?pageSize=2000"
        );
        let json = fetch_json_value(&api_url, Some("https://webtoons.com/"))?;
        let episodes = json
            .get("result")
            .and_then(|result| result.get("episodeList"))
            .and_then(Value::as_array)
            .ok_or_else(|| QuickDownloadError {
                user_message: t!("launcher.new_project.quick_dl.webtoons_no_episodes_error").to_string(),
                log_message: format!("webtoons api '{api_url}' returned no episodeList"),
            })?;
        let last_viewer_link = episodes
            .last()
            .and_then(|episode| episode.get("viewerLink"))
            .and_then(Value::as_str)
            .ok_or_else(|| QuickDownloadError {
                user_message: t!("launcher.new_project.quick_dl.webtoons_no_episode_error").to_string(),
                log_message: format!("webtoons api '{api_url}' returned malformed viewerLink"),
            })?;
        format!("https://www.webtoons.com{last_viewer_link}")
    };

    let html = fetch_text(&chapter_url, Some("https://webtoons.com/"))?;
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(&html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let candidate = get_html_attr(tag.attrs, "data-url")
            .or_else(|| get_html_attr(tag.attrs, "data-src"))
            .or_else(|| get_html_attr(tag.attrs, "src"));
        let Some(src) = candidate else {
            continue;
        };
        let normalized = normalize_network_url(src, &chapter_url);
        if normalized.contains("/viewer/") || !looks_like_image_url(&normalized) {
            continue;
        }
        image_urls.push(normalized);
    }
    image_urls = dedupe_preserve(image_urls);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.webtoons_no_episode_images_error").to_string(),
            log_message: format!("webtoons chapter '{chapter_url}' has no image urls"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some("https://www.webtoons.com/".to_string()),
    })
}
