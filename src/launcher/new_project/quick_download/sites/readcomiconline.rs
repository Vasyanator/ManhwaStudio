/*
File: src/launcher/new_project/quick_download/sites/readcomiconline.rs

Purpose:
Chapter resolver for readcomiconline.li, including its obfuscated image-URL decoder.

Key functions:
- readcomiconline_plan()
- readcomiconline_decode()
- find_quoted_end()

Notes:
Page URLs are pushed by an inline script as `lstImages.push("<encoded>")`. The encoding is
the site's own scramble (marker substitution, fixed-offset slicing, base64, size suffix);
it is intentionally reproduced verbatim and must not be "cleaned up" without a real sample
to test against.
*/

use super::super::base64::base64_decode;
use super::super::html::collect_anchor_hrefs_containing;
use super::super::http::fetch_text;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{path_segment_count, path_segments};

/// Builds the download plan for a readcomiconline.li issue or comic URL.
///
/// # Errors
/// Returns `QuickDownloadError` when a comic URL has too few path segments, when the comic
/// page lists no issues, when a fetch fails, or when the issue page has no `lstImages`.
pub(crate) fn readcomiconline_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = if path_segment_count(url) <= 2 {
        let comic_id = path_segments(url)
            .get(1)
            .cloned()
            .ok_or_else(|| QuickDownloadError {
                user_message: t!("launcher.new_project.quick_dl.readcomic_incomplete_url_error").to_string(),
                log_message: format!("readcomiconline url '{url}' has not enough segments"),
            })?;
        let list_url = format!("https://readcomiconline.li/Comic/{comic_id}");
        let html = fetch_text(&list_url, None)?;
        let chapters = collect_anchor_hrefs_containing(&html, &list_url, "/Comic/");
        chapters.last().cloned().ok_or_else(|| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.comic_no_chapters_error").to_string(),
            log_message: format!("readcomiconline list '{list_url}' has no chapters"),
        })?
    } else {
        url.to_string()
    };

    let html = fetch_text(&chapter_url, None)?;
    let mut image_urls = Vec::new();
    let mut start = 0usize;
    while let Some(index) = html[start..].find("lstImages.push(") {
        let absolute_index = start + index + "lstImages.push(".len();
        let Some(quote) = html.as_bytes().get(absolute_index).copied() else {
            break;
        };
        if quote != b'"' && quote != b'\'' {
            start = absolute_index;
            continue;
        }
        let value_start = absolute_index + 1;
        let Some(end_offset) = find_quoted_end(&html[value_start..], quote) else {
            break;
        };
        let encoded = &html[value_start..value_start + end_offset];
        image_urls.push(readcomiconline_decode(encoded));
        start = value_start + end_offset + 1;
    }

    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.readcomic_no_pages_error").to_string(),
            log_message: format!("readcomiconline chapter '{chapter_url}' has no lstImages"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

/// Decodes one scrambled `lstImages` entry into a CDN URL. Already-absolute `https` values
/// are returned unchanged; malformed input degrades to a short/empty URL rather than
/// panicking (every slice is checked).
fn readcomiconline_decode(url: &str) -> String {
    let url = url.replace("_x236", "d").replace("_x945", "g");
    if url.starts_with("https") {
        return url;
    }

    let (main, suffix) = url
        .split_once('?')
        .map_or((url.as_str(), ""), |(a, b)| (a, b));
    let contains_s0 = main.contains("=s0");
    let trimmed = if contains_s0 {
        main.get(..main.len().saturating_sub(3)).unwrap_or_default()
    } else {
        main.get(..main.len().saturating_sub(6)).unwrap_or_default()
    };
    let stage1 = format!(
        "{}{}",
        trimmed.get(4..22).unwrap_or_default(),
        trimmed.get(25..).unwrap_or_default()
    );
    let stage2 = format!(
        "{}{}",
        stage1
            .get(..stage1.len().saturating_sub(6))
            .unwrap_or_default(),
        stage1
            .get(stage1.len().saturating_sub(2)..)
            .unwrap_or_default()
    );
    let decoded = base64_decode(&stage2)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let stage3 = format!(
        "{}{}",
        decoded.get(..13).unwrap_or_default(),
        decoded.get(17..).unwrap_or_default()
    );
    let suffix_param = if contains_s0 { "=s0" } else { "=s1600" };
    let final_url = format!(
        "{}{}",
        stage3
            .get(..stage3.len().saturating_sub(2))
            .unwrap_or_default(),
        suffix_param
    );
    if suffix.is_empty() {
        format!("https://2.bp.blogspot.com/{final_url}")
    } else {
        format!("https://2.bp.blogspot.com/{final_url}?{suffix}")
    }
}

/// Offset of the closing `quote` in a JavaScript string body, skipping backslash escapes.
/// Returns `None` when the string is unterminated.
fn find_quoted_end(text: &str, quote: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == quote {
            return Some(index);
        }
        index += 1;
    }
    None
}
