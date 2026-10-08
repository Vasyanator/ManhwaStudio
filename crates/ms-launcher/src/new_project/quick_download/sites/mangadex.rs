/*
File: crates/ms-launcher/src/new_project/quick_download/sites/mangadex.rs

Purpose:
Chapter resolver for mangadex.org.

Key functions:
- mangadex_plan() - the I/O half: chapter id, `at-home` request
- mangadex_plan_from_at_home() - pure: `at-home` JSON -> plan with fallbacks and digests
- mangadex_page_sha256() - the SHA-256 MangaDex embeds in a page file name
- pick_latest_mangadex_chapter()

Notes:
Fully API driven: the chapter id comes from the URL (`/chapter/<id>`) or from the title
feed, and page URLs are assembled from the `at-home` server response
(`{baseUrl}/data/{hash}/{file}`). `baseUrl` is a MangaDex@Home volunteer node that can be
broken (404s, TLS failures) while the API keeps handing it out, so every page also carries
the same path on the main upload origin as a fallback and its file-name digest — the
official reader does the same. Node success/failure reporting to MangaDex is not done.
*/

use super::super::http::fetch_json_value;
use super::super::plan::{PlannedImage, QuickDownloadError, SiteDownloadPlan, sha256_from_hex};
use super::super::url_util::path_segment_after;
use serde_json::Value;

/// The main MangaDex image origin, serving the same `/data/{hash}/{file}` paths as every
/// MangaDex@Home node; the official reader's fallback (`cdnOrigin`).
const MANGADEX_UPLOADS_ORIGIN: &str = "https://uploads.mangadex.org";

/// Builds the download plan for a mangadex.org chapter or title URL.
///
/// # Errors
/// Returns `QuickDownloadError` when the URL is neither a title nor a chapter, when the
/// title has no chapters, when a request fails, or when the `at-home` response lacks
/// `baseUrl`/`chapter` data (see `mangadex_plan_from_at_home`).
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
    mangadex_plan_from_at_home(&api_url, &json)
}

/// Turns an `at-home/server` response into the page plan, in `chapter.data` order.
///
/// Each page's primary URL is `{baseUrl}/data/{hash}/{file}`; its fallback is the same path on
/// `MANGADEX_UPLOADS_ORIGIN`, omitted when `baseUrl` already is that origin. The expected
/// digest comes from the file name (`mangadex_page_sha256`); a name without one is still
/// downloaded, just unverified. `api_url` only names the source in log messages.
///
/// # Errors
/// Returns `QuickDownloadError` when `baseUrl`, `chapter`, `chapter.hash` or `chapter.data`
/// is missing or has the wrong type.
fn mangadex_plan_from_at_home(api_url: &str, json: &Value) -> Result<SiteDownloadPlan, QuickDownloadError> {
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
    let base_url = base_url.trim_end_matches('/');
    let has_fallback_origin = !is_uploads_origin(base_url);
    let images = data
        .iter()
        .filter_map(Value::as_str)
        .map(|name| PlannedImage {
            url: format!("{base_url}/data/{hash}/{name}"),
            fallbacks: if has_fallback_origin {
                vec![format!("{MANGADEX_UPLOADS_ORIGIN}/data/{hash}/{name}")]
            } else {
                Vec::new()
            },
            sha256: mangadex_page_sha256(name),
        })
        .collect::<Vec<_>>();
    Ok(SiteDownloadPlan {
        images,
        referer: None,
    })
}

/// Whether `base_url` (already without a trailing slash) names `MANGADEX_UPLOADS_ORIGIN`,
/// tolerating an explicit default port and letter case, so the fallback never duplicates the
/// primary origin.
fn is_uploads_origin(base_url: &str) -> bool {
    let origin = base_url.strip_suffix(":443").unwrap_or(base_url);
    origin.eq_ignore_ascii_case(MANGADEX_UPLOADS_ORIGIN)
}

/// Extracts the SHA-256 MangaDex embeds in a page file name `<n>-<64 hex>.<ext>` (the
/// official reader's `/-([0-9a-f]{64})\.[a-z]{3,4}$/`). Returns `None` for any other shape.
fn mangadex_page_sha256(file_name: &str) -> Option<[u8; 32]> {
    let (stem, extension) = file_name.rsplit_once('.')?;
    let extension_ok = (3..=4).contains(&extension.len())
        && extension.bytes().all(|byte| byte.is_ascii_lowercase());
    if !extension_ok {
        return None;
    }
    let (_, digest) = stem.rsplit_once('-')?;
    // The reader's pattern is lower-case only; `sha256_from_hex` alone would also take
    // upper-case digits.
    if digest.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    sha256_from_hex(digest)
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

#[cfg(test)]
mod tests {
    use super::{MANGADEX_UPLOADS_ORIGIN, mangadex_page_sha256, mangadex_plan_from_at_home};
    use serde_json::json;

    const API_URL: &str = "https://api.mangadex.org/at-home/server/cd72ff48";
    const DIGEST_A: &str = "4b0c3a5bd0d8a1b6b2f1e9f2c5e8d7a6b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8";
    const DIGEST_B: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn at_home_json_yields_node_primary_upload_fallback_and_digest_in_order() {
        let json = json!({
            "result": "ok",
            "baseUrl": "https://cmdxd98sb0x3yprd.mangadex.network",
            "chapter": {
                "hash": "abc123",
                "data": [format!("1-{DIGEST_A}.png"), "2-plain.jpg", format!("3-{DIGEST_B}.jpg")],
                "dataSaver": ["ignored.jpg"],
            }
        });
        let plan = mangadex_plan_from_at_home(API_URL, &json).expect("valid at-home response");
        assert_eq!(plan.referer, None);
        assert_eq!(plan.images.len(), 3);
        let first = &plan.images[0];
        assert_eq!(
            first.url,
            format!("https://cmdxd98sb0x3yprd.mangadex.network/data/abc123/1-{DIGEST_A}.png")
        );
        assert_eq!(
            first.fallbacks,
            [format!("{MANGADEX_UPLOADS_ORIGIN}/data/abc123/1-{DIGEST_A}.png")]
        );
        assert_eq!(first.sha256, mangadex_page_sha256(&format!("1-{DIGEST_A}.png")));
        assert!(first.sha256.is_some());
        assert_eq!(plan.images[1].url, "https://cmdxd98sb0x3yprd.mangadex.network/data/abc123/2-plain.jpg");
        assert_eq!(plan.images[1].sha256, None);
        assert!(plan.images[2].url.ends_with(&format!("/3-{DIGEST_B}.jpg")));
    }

    #[test]
    fn upload_origin_as_base_url_gets_no_duplicate_fallback() {
        for base in [
            MANGADEX_UPLOADS_ORIGIN.to_string(),
            format!("{MANGADEX_UPLOADS_ORIGIN}/"),
            format!("{MANGADEX_UPLOADS_ORIGIN}:443"),
            format!("{MANGADEX_UPLOADS_ORIGIN}:443/"),
            "https://Uploads.MangaDex.org".to_string(),
        ] {
            let json = json!({
                "baseUrl": base,
                "chapter": {"hash": "h", "data": ["1-x.png"]},
            });
            let plan = mangadex_plan_from_at_home(API_URL, &json).expect("valid at-home response");
            assert!(plan.images[0].url.ends_with("/data/h/1-x.png"), "{}", plan.images[0].url);
            assert!(plan.images[0].fallbacks.is_empty(), "{base}");
        }
    }

    #[test]
    fn at_home_json_missing_fields_is_an_error_naming_the_field() {
        let cases = [
            (json!({"chapter": {"hash": "h", "data": []}}), "baseUrl"),
            (json!({"baseUrl": "https://n"}), "chapter field"),
            (json!({"baseUrl": "https://n", "chapter": {"data": []}}), "hash"),
            (json!({"baseUrl": "https://n", "chapter": {"hash": "h"}}), "chapter.data"),
        ];
        for (json, expected) in cases {
            let Err(err) = mangadex_plan_from_at_home(API_URL, &json) else {
                panic!("expected an error for {json}");
            };
            assert!(err.log_message.contains(expected), "{}", err.log_message);
            assert!(err.log_message.contains(API_URL), "{}", err.log_message);
        }
    }

    #[test]
    fn page_sha256_follows_the_official_reader_pattern() {
        let digest = mangadex_page_sha256(&format!("12-{DIGEST_B}.png")).expect("digest");
        assert_eq!(digest[0], 0x00);
        assert_eq!(digest[1], 0x11);
        assert_eq!(digest[31], 0xff);
        assert!(mangadex_page_sha256(&format!("1-{DIGEST_B}.webp")).is_some());
        // Not the reader's shape: no dash, short digest, upper-case, bad extension.
        assert_eq!(mangadex_page_sha256(&format!("{DIGEST_B}.png")), None);
        assert_eq!(mangadex_page_sha256(&format!("1-{}.png", &DIGEST_B[..63])), None);
        assert_eq!(mangadex_page_sha256(&format!("1-{}.png", DIGEST_B.to_ascii_uppercase())), None);
        assert_eq!(mangadex_page_sha256(&format!("1-{DIGEST_B}.PNG")), None);
        assert_eq!(mangadex_page_sha256(&format!("1-{DIGEST_B}.jp")), None);
        assert_eq!(mangadex_page_sha256(&format!("1-{DIGEST_B}")), None);
    }
}
