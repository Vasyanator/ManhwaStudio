/*
File: src/launcher/new_project/quick_download/sites/comicfury.rs

Purpose:
Chapter resolver for comicfury.com and its `*.thecomicseries.com` mirrors.

Key functions:
- comicfury_plan()
- comicfury_id()

Notes:
The comic id comes either from the `/read/<id>/` path (or `?url=` parameter) on the main
host, or from the subdomain on a mirror. Page images are recognized by a `/comic/` or
`/comics/` path fragment.
*/

use super::super::html::{collect_anchor_hrefs_containing, extract_html_tags, get_html_attr};
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{
    dedupe_preserve, extract_host, normalize_network_url, path_segments, query_param,
};

/// Builds the download plan for a ComicFury page, archive, or mirror URL.
///
/// # Errors
/// Returns `QuickDownloadError` when no comic id can be derived, when the archive lists no
/// chapters, when a fetch fails, or when the chapter page has no matching images.
pub(crate) fn comicfury_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let comic_id = comicfury_id(url).ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.comicfury_no_comic_error").to_string(),
        log_message: format!("comicfury url '{url}' has no comic id"),
    })?;

    let archive_url = if url.contains("/read/") && url.contains("/comics/") {
        url.to_string()
    } else {
        format!("https://comicfury.com/read/{comic_id}/archive")
    };
    let html = fetch_text(&archive_url, None)?;
    let chapter_url = if archive_url == url {
        archive_url
    } else {
        collect_anchor_hrefs_containing(&html, &archive_url, &format!("/read/{comic_id}/comics/"))
            .last()
            .cloned()
            .ok_or_else(|| QuickDownloadError {
                user_message: t!("launcher.new_project.quick_dl.comicfury_no_chapters_error").to_string(),
                log_message: format!("comicfury archive '{archive_url}' has no chapter urls"),
            })?
    };

    let chapter_html = fetch_text(&chapter_url, None)?;
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(&chapter_html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(src) = get_html_attr(tag.attrs, "src") else {
            continue;
        };
        let normalized = normalize_network_url(src, &chapter_url);
        if normalized.contains("/comic/") || normalized.contains("/comics/") {
            image_urls.push(normalized);
        }
    }
    image_urls = dedupe_preserve(image_urls);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.comicfury_no_images_error").to_string(),
            log_message: format!("comicfury chapter '{chapter_url}' has no matching images"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

/// Extracts the ComicFury comic id from a main-host URL (`?url=` or `/read/<id>`) or from
/// a `<id>.thecomicseries.com` mirror host. Returns `None` for anything else.
fn comicfury_id(url: &str) -> Option<String> {
    let host = extract_host(url)?;
    if host == "comicfury.com" {
        if let Some(value) = query_param(url, "url") {
            return Some(value);
        }
        let segments = path_segments(url);
        if segments.first().map(String::as_str) == Some("read") {
            return segments.get(1).cloned();
        }
    }
    if host.ends_with(".thecomicseries.com") {
        return host.split('.').next().map(str::to_string);
    }
    None
}
