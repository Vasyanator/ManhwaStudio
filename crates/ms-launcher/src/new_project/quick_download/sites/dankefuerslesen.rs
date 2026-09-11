/*
File: src/launcher/new_project/quick_download/sites/dankefuerslesen.rs

Purpose:
Chapter resolver for danke.moe (Danke fürs Lesen).

Key functions:
- dankefuerslesen_plan()
- danke_chapter_ref()
- parse_danke_images()

Notes:
Serves the hosts `danke.moe` and `www.danke.moe`. Reader URLs are `/read/manga/<slug>/
<chapter>/<page>`, where `read` may also be `reader` and `manga` may also be `series`. One
request to `/api/series/<slug>/` returns the whole series document, chapters included; the
chapter key inside it uses `.` where the URL segment uses `-` (URL `12-5` -> key `12.5`).
Pages are static media: `/media/manga/<slug>/chapters/<folder>/<group id>/<file name>`.
A chapter may be released by several groups; each list is complete on its own, so the
lowest group id is picked to keep the result deterministic.
*/

use super::super::http::fetch_json_value;
use super::super::plan::{QuickDownloadError, SiteDownloadPlan};
use super::super::url_util::path_segments;
use serde_json::{Map, Value};

/// Origin serving both the series API and the chapter media.
const DANKE_ORIGIN: &str = "https://danke.moe";

/// Builds the download plan for a danke.moe reader URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the URL is not a reader URL, when the series request
/// fails, or when the series document holds no pages for that chapter.
pub(crate) fn dankefuerslesen_plan(url: &str) -> Result<SiteDownloadPlan, QuickDownloadError> {
    let (slug, chapter_key) = danke_chapter_ref(url).ok_or_else(|| QuickDownloadError {
        user_message: t!("launcher.new_project.quick_dl.invalid_url_error").to_string(),
        log_message: format!("danke.moe url '{url}' is not '/read/manga/<slug>/<chapter>'"),
    })?;
    let api_url = format!("{DANKE_ORIGIN}/api/series/{slug}/");
    let json = fetch_json_value(&api_url, None)?;
    let image_urls = parse_danke_images(&json, &slug, &chapter_key);
    if image_urls.is_empty() {
        return Err(QuickDownloadError {
            user_message: t!("launcher.new_project.quick_dl.no_chapter_images_error").to_string(),
            log_message: format!(
                "danke.moe series '{api_url}' has no page list for chapter key '{chapter_key}'"
            ),
        });
    }
    Ok(SiteDownloadPlan {
        image_urls,
        referer: None,
    })
}

/// Splits a reader URL into the series slug and the chapter key used by the series API.
///
/// Accepts `/read/...` and `/reader/...` with either `manga` or `series` as the second
/// segment; the chapter segment is translated from the URL form (`12-5`) to the JSON key
/// form (`12.5`). Returns `None` for any other URL shape.
fn danke_chapter_ref(url: &str) -> Option<(String, String)> {
    let segments = path_segments(url);
    let section = segments.first().map(String::as_str)?;
    if section != "read" && section != "reader" {
        return None;
    }
    let kind = segments.get(1).map(String::as_str)?;
    if kind != "manga" && kind != "series" {
        return None;
    }
    let slug = segments.get(2)?;
    let chapter_segment = segments.get(3).map(String::as_str)?;
    Some((slug.clone(), chapter_segment.replace('-', ".")))
}

/// Assembles the ordered page URLs of one chapter of a series document.
///
/// Returns an empty vector when the chapter key, its `folder`, or a non-empty group file
/// list is missing. The file-name order inside the selected group is the reading order.
fn parse_danke_images(payload: &Value, slug: &str, chapter_key: &str) -> Vec<String> {
    let Some(chapter) = payload
        .get("chapters")
        .and_then(|chapters| chapters.get(chapter_key))
    else {
        return Vec::new();
    };
    let Some(folder) = chapter.get("folder").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(groups) = chapter.get("groups").and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(group_id) = pick_danke_group(groups) else {
        return Vec::new();
    };
    let Some(files) = groups.get(group_id).and_then(Value::as_array) else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(Value::as_str)
        .map(|file| {
            format!("{DANKE_ORIGIN}/media/manga/{slug}/chapters/{folder}/{group_id}/{file}")
        })
        .collect()
}

/// Picks the release group to download: the lowest group id that actually carries files.
///
/// Ids are compared numerically when they parse as numbers, so the choice does not depend
/// on how the JSON object happened to be ordered. Returns `None` when no group has files.
fn pick_danke_group(groups: &Map<String, Value>) -> Option<&str> {
    let mut ids = groups
        .iter()
        .filter(|(_, files)| {
            files
                .as_array()
                .is_some_and(|files| files.iter().any(Value::is_string))
        })
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    // Non-numeric ids sort last and then alphabetically, keeping the order total.
    ids.sort_by(|left, right| {
        let left_key = (left.parse::<u64>().unwrap_or(u64::MAX), *left);
        let right_key = (right.parse::<u64>().unwrap_or(u64::MAX), *right);
        left_key.cmp(&right_key)
    });
    ids.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Minimal series document with one chapter released by one group.
    fn single_group_series() -> Value {
        json!({
            "slug": "demo",
            "title": "Demo",
            "groups": {"7": "Some Group"},
            "chapters": {
                "12.5": {
                    "folder": "0012_5",
                    "groups": {"7": ["02.png", "01.png"]},
                },
            },
        })
    }

    #[test]
    fn parse_danke_images_builds_media_urls_in_order() {
        assert_eq!(
            parse_danke_images(&single_group_series(), "demo", "12.5"),
            vec![
                "https://danke.moe/media/manga/demo/chapters/0012_5/7/02.png".to_string(),
                "https://danke.moe/media/manga/demo/chapters/0012_5/7/01.png".to_string(),
            ]
        );
    }

    #[test]
    fn parse_danke_images_returns_empty_on_missing_pieces() {
        let series = single_group_series();
        // Unknown chapter key (e.g. the URL form was not translated).
        assert!(parse_danke_images(&series, "demo", "12-5").is_empty());
        assert!(parse_danke_images(&json!({}), "demo", "12.5").is_empty());
        let no_folder = json!({"chapters": {"1": {"groups": {"7": ["01.png"]}}}});
        assert!(parse_danke_images(&no_folder, "demo", "1").is_empty());
        let no_groups = json!({"chapters": {"1": {"folder": "0001"}}});
        assert!(parse_danke_images(&no_groups, "demo", "1").is_empty());
        let empty_group = json!({"chapters": {"1": {"folder": "0001", "groups": {"7": []}}}});
        assert!(parse_danke_images(&empty_group, "demo", "1").is_empty());
    }

    #[test]
    fn parse_danke_images_prefers_lowest_numeric_group_with_files() {
        let series = json!({
            "chapters": {
                "1": {
                    "folder": "0001",
                    "groups": {"2": [], "10": ["b.png"], "7": ["a.png"]},
                },
            },
        });
        // Group 2 is empty, so group 7 wins over the lexicographically smaller "10".
        assert_eq!(
            parse_danke_images(&series, "demo", "1"),
            vec!["https://danke.moe/media/manga/demo/chapters/0001/7/a.png".to_string()]
        );
    }

    #[test]
    fn danke_chapter_ref_maps_url_segment_to_json_key() {
        assert_eq!(
            danke_chapter_ref("https://danke.moe/read/manga/demo/12/1/"),
            Some(("demo".to_string(), "12".to_string()))
        );
        assert_eq!(
            danke_chapter_ref("https://www.danke.moe/reader/series/demo/12-5"),
            Some(("demo".to_string(), "12.5".to_string()))
        );
    }

    #[test]
    fn danke_chapter_ref_rejects_malformed_urls() {
        // Series page without a chapter segment.
        assert_eq!(danke_chapter_ref("https://danke.moe/read/manga/demo"), None);
        // Unknown section names.
        assert_eq!(
            danke_chapter_ref("https://danke.moe/browse/manga/demo/12"),
            None
        );
        assert_eq!(
            danke_chapter_ref("https://danke.moe/read/chapters/demo/12"),
            None
        );
        assert_eq!(danke_chapter_ref("https://danke.moe/"), None);
    }
}
