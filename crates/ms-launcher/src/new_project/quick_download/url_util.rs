/*
File: src/launcher/new_project/quick_download/url_util.rs

Purpose:
Site-agnostic URL primitives shared by the quick downloader: scheme normalization, host and
query extraction, relative-link resolution, path segment access, and cheap URL predicates.

Key functions:
- normalize_http_url(), extract_host(), query_param()
- normalize_network_url(), path_contains(), path_segments(), path_segment_after(),
  path_segment_count()
- dedupe_preserve(), looks_like_image_url()

Notes:
Deliberately dependency-free string handling (no `url` crate) and deliberately permissive:
these helpers accept the sloppy markup real chapter pages contain. Behavior is pinned by the
unit tests at the bottom of this file, including its known quirks.
*/

use std::collections::HashSet;

/// Normalizes user input into an absolute http(s) URL, adding `https://` to a bare host.
///
/// # Errors
/// Returns a short technical reason when the input is empty or does not look like a URL.
pub(crate) fn normalize_http_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("empty url".to_string());
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(trimmed.to_string());
    }
    if looks_like_host(trimmed) {
        return Ok(format!("https://{trimmed}"));
    }
    Err("missing http/https scheme or host".to_string())
}

/// Heuristic for a scheme-less host: a `www.` prefix or any dot at all.
fn looks_like_host(value: &str) -> bool {
    value.starts_with("www.") || value.contains('.')
}

/// Returns the lowercased host of an absolute URL, without userinfo and port.
/// Returns `None` when the URL has no `://` separator.
pub(crate) fn extract_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.split('@').next_back().unwrap_or(host);
    Some(host.split(':').next().unwrap_or(host).to_ascii_lowercase())
}

/// Returns the percent-decoded value of query parameter `key`, or `None` if absent.
pub(crate) fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next().unwrap_or_default();
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let name = parts.next().unwrap_or_default();
        let value = parts.next().unwrap_or_default();
        if name == key {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Decodes `%XX` escapes and `+` as space; invalid escapes are kept verbatim and invalid
/// UTF-8 is replaced lossily.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push((hi << 4) | lo);
            index += 3;
            continue;
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

/// Maps one ASCII hex digit to its value, or `None` for any other byte.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Resolves a link found in a page against the page URL: absolute URLs pass through,
/// `//host` inherits the base scheme, `/path` uses the base origin, anything else is
/// resolved relative to the base directory.
pub(crate) fn normalize_network_url(url: &str, base: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    if url.starts_with("//") {
        let scheme = if base.starts_with("http://") {
            "http:"
        } else {
            "https:"
        };
        return format!("{scheme}{url}");
    }
    let origin = origin_from_url(base);
    if url.starts_with('/') {
        return format!("{origin}{url}");
    }
    let base_dir = base.rsplit_once('/').map(|(left, _)| left).unwrap_or(base);
    format!("{base_dir}/{url}")
}

/// Returns `scheme://host` of an absolute URL, or an empty string when there is no scheme.
fn origin_from_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let host = rest.split('/').next().unwrap_or_default();
        format!("{scheme}://{host}")
    } else {
        String::new()
    }
}

/// Returns `true` if the path part (query included, host excluded) contains `needle`.
pub(crate) fn path_contains(url: &str, needle: &str) -> bool {
    let path = url
        .split_once("://")
        .map(|(_, rest)| {
            rest.split_once('/')
                .map(|(_, path)| path)
                .unwrap_or_default()
        })
        .unwrap_or_default();
    path.contains(needle)
}

/// Splits the path into non-empty segments, dropping the query and fragment-free tail.
pub(crate) fn path_segments(url: &str) -> Vec<String> {
    url.split_once("://")
        .map(|(_, rest)| {
            rest.split_once('/')
                .map(|(_, path)| path)
                .unwrap_or_default()
        })
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

/// Returns the path segment following the first segment equal to `key`, if any.
pub(crate) fn path_segment_after(url: &str, key: &str) -> Option<String> {
    let segments = path_segments(url);
    let index = segments.iter().position(|segment| segment == key)?;
    segments.get(index + 1).cloned()
}

/// Number of non-empty path segments; used to tell a series URL from a chapter URL.
pub(crate) fn path_segment_count(url: &str) -> usize {
    path_segments(url).len()
}

/// Removes duplicates while keeping the first occurrence order.
pub(crate) fn dedupe_preserve(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}

/// Cheap "is this an image link" filter: a known extension appearing anywhere in the
/// lowercased URL (not necessarily at its end).
pub(crate) fn looks_like_image_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif"]
        .iter()
        .any(|ext| lower.contains(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_http_url_keeps_absolute_and_trims() {
        assert_eq!(
            normalize_http_url("  https://example.com/a  "),
            Ok("https://example.com/a".to_string())
        );
        assert_eq!(
            normalize_http_url("http://example.com"),
            Ok("http://example.com".to_string())
        );
    }

    #[test]
    fn normalize_http_url_adds_https_to_bare_host() {
        assert_eq!(
            normalize_http_url("example.com/a"),
            Ok("https://example.com/a".to_string())
        );
        // A `www.` prefix is accepted even without a second dot.
        assert_eq!(
            normalize_http_url("www.host"),
            Ok("https://www.host".to_string())
        );
    }

    #[test]
    fn normalize_http_url_rejects_empty_and_non_urls() {
        assert_eq!(normalize_http_url("   "), Err("empty url".to_string()));
        assert_eq!(
            normalize_http_url("not-a-url"),
            Err("missing http/https scheme or host".to_string())
        );
    }

    #[test]
    fn extract_host_strips_userinfo_port_and_case() {
        assert_eq!(
            extract_host("https://User@Example.COM:8443/path?q=1"),
            Some("example.com".to_string())
        );
        assert_eq!(extract_host("no-scheme.example/path"), None);
    }

    #[test]
    fn query_param_reads_and_decodes() {
        let url = "https://example.com/v?titleId=123&no=45#frag";
        assert_eq!(query_param(url, "titleId"), Some("123".to_string()));
        assert_eq!(query_param(url, "no"), Some("45".to_string()));
        assert_eq!(query_param(url, "missing"), None);
        assert_eq!(
            query_param("https://example.com/v?url=a%2Fb+c", "url"),
            Some("a/b c".to_string())
        );
        // A key without `=` yields an empty value rather than `None`.
        assert_eq!(
            query_param("https://example.com/v?flag", "flag"),
            Some(String::new())
        );
    }

    #[test]
    fn percent_decode_handles_escapes_and_leftovers() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("%41%2f"), "A/");
        // Truncated or invalid escapes are kept verbatim.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn normalize_network_url_resolves_all_link_forms() {
        assert_eq!(
            normalize_network_url("https://cdn.example/a.jpg", "https://example.com/p/q"),
            "https://cdn.example/a.jpg"
        );
        assert_eq!(
            normalize_network_url("//cdn.example/a.jpg", "http://example.com/p/q"),
            "http://cdn.example/a.jpg"
        );
        assert_eq!(
            normalize_network_url("//cdn.example/a.jpg", "https://example.com/p/q"),
            "https://cdn.example/a.jpg"
        );
        assert_eq!(
            normalize_network_url("/a.jpg", "https://example.com/p/q"),
            "https://example.com/a.jpg"
        );
        assert_eq!(
            normalize_network_url("a.jpg", "https://example.com/p/q"),
            "https://example.com/p/a.jpg"
        );
    }

    #[test]
    fn path_helpers_split_on_segments() {
        let url = "https://example.com/title/abc/chapter/42?x=1";
        assert_eq!(path_segment_after(url, "chapter"), Some("42".to_string()));
        assert_eq!(path_segment_after(url, "title"), Some("abc".to_string()));
        assert_eq!(path_segment_after(url, "missing"), None);
        assert_eq!(path_segment_count(url), 4);
        assert!(path_contains(url, "/chapter"));
        assert!(!path_contains("https://example.com/", "/chapter"));
        // The host is never part of the inspected path.
        assert!(!path_contains("https://chapter.example.com/x", "chapter"));
    }

    #[test]
    fn dedupe_preserve_keeps_first_occurrence_order() {
        let input = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(dedupe_preserve(input), vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn looks_like_image_url_matches_extension_anywhere() {
        assert!(looks_like_image_url("https://example.com/a.JPG"));
        assert!(looks_like_image_url("https://example.com/a.png?w=100"));
        // Quirk pinned on purpose: the extension may appear mid-path.
        assert!(looks_like_image_url("https://example.com/a.webp/full"));
        assert!(!looks_like_image_url("https://example.com/page"));
    }
}
