/*
File: src/launcher/new_project/quick_download/sites/mangadex.rs

Purpose:
Chapter resolver for mangadex.org.

Key functions:
- mangadex_plan()
- pick_latest_mangadex_chapter()

Notes:
Fully API driven: the chapter id comes from the URL (`/chapter/<id>`) or from the title
feed, and page URLs are assembled from the `at-home` server response
(`{baseUrl}/data/{hash}/{file}`).
*/

use super::super::http::fetch_json_value;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::path_segment_after;
use serde_json::Value;

/// Builds the download plan for a mangadex.org chapter or title URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the URL is neither a title nor a chapter, when the
/// title has no chapters, or when the `at-home` response lacks `baseUrl`/`chapter` data.
pub(crate) fn mangadex_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_id = if let Some(id) = path_segment_after(url, "chapter") {
        id
    } else {
        let manga_id = path_segment_after(url, "title").ok_or_else(|| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.mangadex_bad_url_error").to_string(),
            log_message: format!("mangadex url '{url}' is neither title nor chapter"),
        })?;
        pick_latest_mangadex_chapter(&manga_id)?
    };

    let api_url = format!("https://api.mangadex.org/at-home/server/{chapter_id}");
    let json = fetch_json_value(&api_url, None)?;
    let base_url =
        json.get("baseUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| QuickDownloadError {
                user_message: t!("launcher.new_project.quick_dl.mangadex_no_server_error").to_string(),
                log_message: format!("mangadex at-home '{api_url}' has no baseUrl"),
            })?;
    let chapter = json.get("chapter").ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.mangadex_no_chapter_data_error").to_string(),
        log_message: format!("mangadex at-home '{api_url}' has no chapter field"),
    })?;
    let hash = chapter
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.mangadex_no_hash_error").to_string(),
            log_message: format!("mangadex at-home '{api_url}' has no hash"),
        })?;
    let data = chapter
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.mangadex_no_pages_error").to_string(),
            log_message: format!("mangadex at-home '{api_url}' has no chapter.data"),
        })?;
    let image_urls = data
        .iter()
        .filter_map(Value::as_str)
        .map(|name| format!("{base_url}/data/{hash}/{name}"))
        .collect::<Vec<_>>();
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

/// Returns the id of the latest chapter of `manga_id`, preferring the English feed and
/// falling back to the unfiltered feed.
///
/// # Errors
/// Returns `QuickDownloadError` when a feed request fails or both feeds are empty.
fn pick_latest_mangadex_chapter(manga_id: &str) -> Result<String, QuickDownloadError> {
    for language_filtered in [true, false] {
        let lang_param = if language_filtered {
            "&translatedLanguage[]=en"
        } else {
            ""
        };
        let api_url = format!(
            "https://api.mangadex.org/manga/{manga_id}/feed?limit=1{lang_param}\
             &order[volume]=desc&order[chapter]=desc"
        );
        let json = fetch_json_value(&api_url, None)?;
        if let Some(id) = json
            .get("data")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|entry| entry.get("id"))
            .and_then(Value::as_str)
        {
            return Ok(id.to_string());
        }
    }
    Err(QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.mangadex_no_title_chapters_error").to_string(),
        log_message: format!("mangadex manga '{manga_id}' has no chapters"),
    })
}
