/*
File: src/launcher/new_project/quick_download/sites/kuaikan.rs

Purpose:
Chapter resolver for kuaikanmanhua.com.

Key functions:
- kuaikan_plan()

Notes:
The page state is an inline JSON blob rather than markup, so both the chapter link
(`/webs/comic-next/`) and the image URLs are recovered by scanning the raw HTML for
http(s) strings.
*/

use super::super::html::collect_https_json_strings;
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{dedupe_preserve, looks_like_image_url};

/// Builds the download plan for a kuaikanmanhua.com topic or chapter URL.
///
/// # Errors
/// Returns `QuickDownloadError` when a topic page exposes no chapter link, when a fetch
/// fails, or when the chapter blob contains no image URLs.
pub(crate) fn kuaikan_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = if url.contains("/web/topic/") {
        let html = fetch_text(url, None)?;
        let mut chapters = collect_https_json_strings(&html);
        chapters.retain(|item| item.contains("/webs/comic-next/"));
        chapters.last().cloned().ok_or_else(|| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.kuaikan_no_chapters_error").to_string(),
            log_message: format!("kuaikan topic '{url}' has no chapter urls"),
        })?
    } else {
        url.to_string()
    };
    let html = fetch_text(&chapter_url, None)?;
    let mut image_urls = collect_https_json_strings(&html);
    image_urls.retain(|item| looks_like_image_url(item));
    image_urls = dedupe_preserve(image_urls);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.kuaikan_no_images_error").to_string(),
            log_message: format!("kuaikan chapter '{chapter_url}' has no image urls"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}
