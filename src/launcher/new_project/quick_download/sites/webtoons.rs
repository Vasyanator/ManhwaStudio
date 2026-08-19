/*
File: src/launcher/new_project/quick_download/sites/webtoons.rs

Purpose:
Chapter resolver for webtoons.com (including the mobile host and Canvas/Challenge series).

Key constants:
- WEBTOONS_IMAGE_LIST_MARKER, WEBTOONS_IMAGE_LIST_END
- WEBTOONS_DESKTOP_HOST, WEBTOONS_REFERER

Key functions:
- webtoons_plan()
- is_viewer_url()
- desktop_viewer_url()
- latest_episode_viewer_url()
- webtoons_image_list_slice()
- extract_webtoons_image_urls()
- strip_cdn_quality()

Notes:
The episode pages live ONLY inside the `#_imageList` container of the desktop viewer, as
`<img class="_images" data-url="...">` with a shared transparent `src` placeholder. The rest
of the viewer document is the episode-list strip (dozens of 202x142 thumbnails, top and
bottom) plus site chrome, so page collection is scoped to that container instead of scanning
the whole document. The mobile host answers a viewer URL with an episode-list shell that has
no such container, so a `m.webtoons.com` link is rewritten to the desktop host before the
fetch. A series URL is resolved to its latest episode through the mobile episode-list API.
Viewer images need the webtoons.com `Referer`; their `?type=` CDN parameter is dropped so the
un-recompressed original is downloaded rather than the site's display copy.
*/

use super::super::html::{extract_html_tags, get_html_attr, html_unescape};
use super::super::http::{fetch_json_value, fetch_text};
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::{
    dedupe_preserve, normalize_network_url, path_segments, query_param,
};
use serde_json::Value;

/// Opening tag of the container holding the episode pages; matched on a lowercased copy of
/// the document, where it occurs exactly once.
const WEBTOONS_IMAGE_LIST_MARKER: &str = "id=\"_imagelist\"";
/// End of that container: the page images are its direct children with no nested element,
/// so the first closing `div` ends the region that may contribute pages.
const WEBTOONS_IMAGE_LIST_END: &str = "</div";
/// The only host serving the full viewer document; see the file header.
const WEBTOONS_DESKTOP_HOST: &str = "www.webtoons.com";
/// `Referer` the webtoons CDN expects for both the viewer document and the page images.
const WEBTOONS_REFERER: &str = "https://www.webtoons.com/";

/// Builds the download plan for a webtoons.com viewer or series URL.
///
/// # Errors
/// Returns `QuickDownloadError` when a series URL has no `title_no`, when the episode API
/// response is malformed, when a fetch fails, or when the viewer page yields no page images.
pub(crate) fn webtoons_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let chapter_url = if is_viewer_url(url) {
        desktop_viewer_url(url)
    } else {
        latest_episode_viewer_url(url)?
    };

    let html = fetch_text(&chapter_url, Some(WEBTOONS_REFERER))?;
    let image_urls = extract_webtoons_image_urls(&html, &chapter_url);
    if image_urls.is_empty() {
        // Tell the two failures apart: a missing container means the markup changed (or the
        // episode is not an image episode), an empty one means the pages are loaded elsewhere.
        let reason = if webtoons_image_list_slice(&html).is_some() {
            "the '_imageList' container holds no <img data-url>"
        } else {
            "the html has no '_imageList' container"
        };
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.webtoons_no_episode_images_error")
                .to_string(),
            log_message: format!("webtoons chapter '{chapter_url}' has no page images: {reason}"),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: Some(WEBTOONS_REFERER.to_string()),
    })
}

/// Returns `true` for an episode URL, whose path ends in a `viewer` segment; every other
/// shape (`/list`, `/canvas/<slug>/list`, a bare series link) is a series URL.
fn is_viewer_url(url: &str) -> bool {
    path_segments(url)
        .iter()
        .any(|segment| segment == "viewer")
}

/// Rewrites a viewer URL onto the desktop host, leaving any other host untouched.
///
/// `m.webtoons.com` answers a viewer URL with an episode-list shell that carries no
/// `_imageList` container, so the mobile host can never yield pages; the scheme-less
/// `webtoons.com` is normalized the same way.
fn desktop_viewer_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    if !matches!(
        host.to_ascii_lowercase().as_str(),
        "m.webtoons.com" | "webtoons.com"
    ) {
        return url.to_string();
    }
    format!("{scheme}://{WEBTOONS_DESKTOP_HOST}/{path}")
}

/// Resolves a series URL to the viewer URL of its latest episode through the mobile
/// episode-list API.
///
/// # Errors
/// Returns `QuickDownloadError` when `url` carries no `title_no`, when the API response has
/// no `episodeList`, when its last entry has no `viewerLink`, or when the fetch fails.
fn latest_episode_viewer_url(url: &str) -> Result<String, QuickDownloadError> {
    let title_no = query_param(url, "title_no").ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.webtoons_no_titleno_error").to_string(),
        log_message: format!("webtoons url '{url}' has no title_no"),
    })?;
    // Canvas (formerly Challenge) series are a separate collection in the same API.
    let webtoon_type = if url.contains("/canvas/") || url.contains("/challenge/") {
        "canvas"
    } else {
        "webtoon"
    };
    let api_url =
        format!("https://m.webtoons.com/api/v1/{webtoon_type}/{title_no}/episodes?pageSize=2000");
    let json = fetch_json_value(&api_url, Some(WEBTOONS_REFERER))?;
    let episodes = json
        .get("result")
        .and_then(|result| result.get("episodeList"))
        .and_then(Value::as_array)
        .ok_or_else(|| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.webtoons_no_episodes_error").to_string(),
            log_message: format!("webtoons api '{api_url}' returned no episodeList"),
        })?;
    // The list is returned in ascending episode order, so the newest episode is its tail.
    let last_viewer_link = episodes
        .last()
        .and_then(|episode| episode.get("viewerLink"))
        .and_then(Value::as_str)
        .ok_or_else(|| QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.webtoons_no_episode_error").to_string(),
            log_message: format!("webtoons api '{api_url}' returned malformed viewerLink"),
        })?;
    Ok(format!("https://{WEBTOONS_DESKTOP_HOST}{last_viewer_link}"))
}

/// Returns the inner HTML of the `#_imageList` container: everything between the end of its
/// opening tag and the first `WEBTOONS_IMAGE_LIST_END` after it. Returns `None` when the
/// container or the end of its opening tag is missing.
fn webtoons_image_list_slice(html: &str) -> Option<&str> {
    // The marker is matched on an ASCII-lowercased copy so upper-case markup matches too.
    // `to_ascii_lowercase` maps ASCII bytes onto ASCII bytes and leaves every other byte
    // untouched, so byte lengths - and therefore the indices below - are unchanged.
    let lower = html.to_ascii_lowercase();
    let marker_start = lower.find(WEBTOONS_IMAGE_LIST_MARKER)?;
    let content_start = lower[marker_start..].find('>')? + marker_start + 1;
    let content_end = lower[content_start..]
        .find(WEBTOONS_IMAGE_LIST_END)
        .map_or(html.len(), |offset| content_start + offset);
    Some(&html[content_start..content_end])
}

/// Collects the episode page URLs in reading order, deduplicated, resolved against
/// `base_url` and stripped of the CDN quality parameter.
///
/// Only `<img>` inside the `#_imageList` container counts, and only its lazy-loading
/// attribute is read: every page image on this site ships a shared transparent `src`
/// placeholder, so `src` is never a page source and is deliberately not used as a fallback.
/// Returns an empty vector when the container is absent.
fn extract_webtoons_image_urls(html: &str, base_url: &str) -> Vec<String> {
    let Some(image_list) = webtoons_image_list_slice(html) else {
        return Vec::new();
    };
    let mut image_urls = Vec::new();
    for tag in extract_html_tags(image_list) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("img") {
            continue;
        }
        let Some(source) = get_html_attr(tag.attrs, "data-url")
            .or_else(|| get_html_attr(tag.attrs, "data-src"))
            .map(str::trim)
        else {
            continue;
        };
        if source.is_empty() || source.starts_with("data:") {
            continue;
        }
        let absolute = normalize_network_url(&html_unescape(source), base_url);
        image_urls.push(strip_cdn_quality(&absolute));
    }
    dedupe_preserve(image_urls)
}

/// Drops the `type` query parameter of a webtoons CDN URL, keeping every other parameter
/// and the fragment.
///
/// The markup links each page as `...jpg?type=q90`, a recompressed display copy; the same
/// path without the parameter serves the original upload at the same pixel size, which is
/// what a translation project needs.
fn strip_cdn_quality(url: &str) -> String {
    let (without_fragment, fragment) = match url.split_once('#') {
        Some((head, tail)) => (head, Some(tail)),
        None => (url, None),
    };
    let Some((path, query)) = without_fragment.split_once('?') else {
        return url.to_string();
    };
    let kept = query
        .split('&')
        .filter(|pair| {
            !pair.is_empty() && pair.split_once('=').map_or(*pair, |(name, _)| name) != "type"
        })
        .collect::<Vec<_>>()
        .join("&");
    let mut result = String::with_capacity(url.len());
    result.push_str(path);
    if !kept.is_empty() {
        result.push('?');
        result.push_str(&kept);
    }
    if let Some(fragment) = fragment {
        result.push('#');
        result.push_str(fragment);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWER_URL: &str =
        "https://www.webtoons.com/en/sports/skool-of-street/ep-35/viewer?title_no=6743&episode_no=35";

    /// Viewer document shaped like the real one: episode-list thumbnails before and after
    /// the `_imageList` container, page images inside it.
    fn viewer_html() -> String {
        concat!(
            "<div class=\"episode_area\" id=\"topEpisodeList\">",
            "<img src=\"https://webtoons-static.pstatic.net/image/bg_transparency.png\"",
            " data-url=\"https://webtoon-phinf.pstatic.net/thumb_ep_1.png\" class=\"_thumbnailImages\">",
            "</div>",
            "<div class=\"viewer_img _img_viewer_area \" id=\"_imageList\">",
            "<img src=\"https://webtoons-static.pstatic.net/image/bg_transparency.png\" width=\"800\"",
            " height=\"1280.0\" class=\"_images\"",
            " data-url=\"https://webtoon-phinf.pstatic.net/a/page_0001.jpg?type=q90\">",
            "<img src=\"https://webtoons-static.pstatic.net/image/bg_transparency.png\" width=\"800\"",
            " height=\"1280.0\" class=\"_images\"",
            " data-url=\"https://webtoon-phinf.pstatic.net/a/page_0002.jpg?type=q90\">",
            "</div>",
            "<div class=\"episode_area\" id=\"bottomEpisodeList\">",
            "<img src=\"https://webtoons-static.pstatic.net/image/bg_transparency.png\"",
            " data-url=\"https://webtoon-phinf.pstatic.net/thumb_ep_2.png\" class=\"_thumbnailImages\">",
            "</div>",
        )
        .to_string()
    }

    #[test]
    fn extract_takes_only_the_image_list_pages() {
        assert_eq!(
            extract_webtoons_image_urls(&viewer_html(), VIEWER_URL),
            vec![
                "https://webtoon-phinf.pstatic.net/a/page_0001.jpg".to_string(),
                "https://webtoon-phinf.pstatic.net/a/page_0002.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn extract_ignores_the_transparent_src_placeholder() {
        // An `<img>` without a lazy-loading attribute contributes nothing, so the shared
        // placeholder can never enter the plan.
        let html = "<div id=\"_imageList\">\
            <img src=\"https://webtoons-static.pstatic.net/image/bg_transparency.png\" class=\"_images\">\
            </div>";
        assert!(extract_webtoons_image_urls(html, VIEWER_URL).is_empty());
    }

    #[test]
    fn extract_returns_nothing_without_the_container() {
        let html = "<div class=\"episode_area\">\
            <img class=\"_thumbnailImages\" data-url=\"https://webtoon-phinf.pstatic.net/thumb.png\">\
            </div>";
        assert!(extract_webtoons_image_urls(html, VIEWER_URL).is_empty());
    }

    #[test]
    fn image_list_slice_stops_at_the_first_closing_div() {
        let html = "<div id=\"_imageList\">pages</div><div>chrome</div>";
        assert_eq!(webtoons_image_list_slice(html), Some("pages"));
        assert_eq!(webtoons_image_list_slice("<div id=\"other\">x</div>"), None);
    }

    #[test]
    fn strip_cdn_quality_drops_only_the_type_parameter() {
        assert_eq!(
            strip_cdn_quality("https://cdn.example/a.jpg?type=q90"),
            "https://cdn.example/a.jpg"
        );
        assert_eq!(
            strip_cdn_quality("https://cdn.example/a.jpg?type=q90&v=2#frag"),
            "https://cdn.example/a.jpg?v=2#frag"
        );
        assert_eq!(
            strip_cdn_quality("https://cdn.example/a.jpg"),
            "https://cdn.example/a.jpg"
        );
    }

    #[test]
    fn desktop_viewer_url_rewrites_the_mobile_host() {
        assert_eq!(
            desktop_viewer_url("https://m.webtoons.com/en/a/b/viewer?title_no=1&episode_no=2"),
            "https://www.webtoons.com/en/a/b/viewer?title_no=1&episode_no=2"
        );
        assert_eq!(
            desktop_viewer_url("https://webtoons.com/en/a/b/viewer?title_no=1"),
            "https://www.webtoons.com/en/a/b/viewer?title_no=1"
        );
        assert_eq!(desktop_viewer_url(VIEWER_URL), VIEWER_URL);
    }

    #[test]
    fn is_viewer_url_separates_episode_from_series_links() {
        assert!(is_viewer_url(VIEWER_URL));
        assert!(!is_viewer_url(
            "https://www.webtoons.com/en/sports/skool-of-street/list?title_no=6743"
        ));
    }
}
