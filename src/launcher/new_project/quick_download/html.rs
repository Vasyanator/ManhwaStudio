/*
File: src/launcher/new_project/quick_download/html.rs

Purpose:
Site-agnostic HTML/JSON scraping primitives of the quick downloader: a forgiving tag
scanner, an attribute reader, entity unescaping, two collectors built on top of them, and a
scanner for the JS literals chapter pages embed in inline scripts.

Key structures:
- HtmlTag

Key functions:
- extract_html_tags(), get_html_attr(), html_unescape()
- collect_anchor_hrefs_containing(), collect_https_json_strings()
- find_js_array_literal(), find_js_string_literal(), find_array_literal_end()

Notes:
This is a deliberately tolerant scanner, not an HTML parser: chapter pages are frequently
malformed, and a strict parser would drop images a browser still shows. Behavior, including
its quirks, is pinned by the unit tests at the bottom of this file.

The JS literal scanner knows no host: it locates an assignment by variable or object-key name
and returns the balanced literal that follows. It is not a JS parser either — it only tracks
quoting, escaping and bracket depth, which is what the embedded page lists need.
*/

use super::url_util::{dedupe_preserve, normalize_network_url};

/// One `<...>` occurrence found by `extract_html_tags`: the tag name, the raw attribute
/// text after it, and whether it was a closing tag. Borrows the scanned HTML.
pub(crate) struct HtmlTag<'a> {
    pub(crate) name: &'a str,
    pub(crate) attrs: &'a str,
    pub(crate) is_end: bool,
}

/// Scans `html` for tags in document order. Comments, doctypes and processing
/// instructions are skipped; nothing is validated or nested.
pub(crate) fn extract_html_tags(html: &str) -> Vec<HtmlTag<'_>> {
    let mut tags = Vec::new();
    let mut cursor = 0usize;
    while let Some(start_offset) = html[cursor..].find('<') {
        let start = cursor + start_offset;
        let Some(end_offset) = html[start..].find('>') else {
            break;
        };
        let end = start + end_offset;
        let raw = &html[start + 1..end];
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('!') || trimmed.starts_with('?') {
            cursor = end + 1;
            continue;
        }
        let is_end = trimmed.starts_with('/');
        let content = if is_end {
            trimmed[1..].trim_start()
        } else {
            trimmed
        };
        let mut parts = content.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or_default();
        let attrs = parts.next().unwrap_or_default();
        if !name.is_empty() {
            tags.push(HtmlTag {
                name,
                attrs,
                is_end,
            });
        }
        cursor = end + 1;
    }
    tags
}

/// Returns the first value of the attribute named `attr_name` (case-insensitive) inside a
/// raw attribute string. Quoted, single-quoted and bare values are all accepted.
pub(crate) fn get_html_attr<'a>(attrs: &'a str, attr_name: &str) -> Option<&'a str> {
    let bytes = attrs.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name_start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() && bytes[index] != b'=' {
            index += 1;
        }
        let name = &attrs[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let value = if index < bytes.len() && (bytes[index] == b'"' || bytes[index] == b'\'') {
            let quote = bytes[index];
            index += 1;
            let start = index;
            while index < bytes.len() && bytes[index] != quote {
                index += 1;
            }
            let value = &attrs[start..index];
            if index < bytes.len() {
                index += 1;
            }
            value
        } else {
            let start = index;
            while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            &attrs[start..index]
        };
        if name.eq_ignore_ascii_case(attr_name) {
            return Some(value);
        }
    }
    None
}

/// Expands the handful of entities that appear in embedded JSON payloads. Applied in a
/// fixed order, so `&amp;` is expanded last.
pub(crate) fn html_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Collects every `<a href>` of `html` that resolves (against `base_url`) to a URL
/// containing `needle`, deduplicated and in document order.
pub(crate) fn collect_anchor_hrefs_containing(
    html: &str,
    base_url: &str,
    needle: &str,
) -> Vec<String> {
    let mut urls = Vec::new();
    for tag in extract_html_tags(html) {
        if tag.is_end || !tag.name.eq_ignore_ascii_case("a") {
            continue;
        }
        if let Some(href) = get_html_attr(tag.attrs, "href") {
            let normalized = normalize_network_url(href, base_url);
            if normalized.contains(needle) {
                urls.push(normalized);
            }
        }
    }
    dedupe_preserve(urls)
}

/// Scans arbitrary text (inline scripts, JSON blobs) for http(s) URLs, ending each at the
/// first quote, comma, bracket or whitespace. Order is preserved; duplicates are kept.
pub(crate) fn collect_https_json_strings(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index + 8 < bytes.len() {
        let remaining = &text[index..];
        let next_http = remaining
            .find("https://")
            .or_else(|| remaining.find("http://"));
        let Some(offset) = next_http else {
            break;
        };
        let start = index + offset;
        let mut end = start;
        while end < bytes.len() {
            let byte = bytes[end];
            if byte == b'"'
                || byte == b'\''
                || byte == b','
                || byte == b']'
                || byte.is_ascii_whitespace()
            {
                break;
            }
            end += 1;
        }
        urls.push(text[start..end].to_string());
        index = end + 1;
    }
    urls
}

/// Returns the raw `[...]` literal assigned to `marker` in an inline script, brackets and
/// quotes balanced.
///
/// `marker` is the variable or object-key text preceding the literal (`var pages`,
/// `'images'`). Only whitespace, `=` and `:` may separate it from the opening bracket, so an
/// unrelated array further down the page is never picked up; every occurrence of `marker` is
/// tried in turn. The returned slice borrows `html` and still carries the brackets, ready for
/// a JSON parse. Returns `None` when no occurrence is followed by a balanced literal.
pub(crate) fn find_js_array_literal<'a>(html: &'a str, marker: &str) -> Option<&'a str> {
    for (marker_start, _) in html.match_indices(marker) {
        let rest = &html[marker_start + marker.len()..];
        let Some(open) = rest.find('[') else {
            continue;
        };
        if !is_js_assignment_gap(&rest[..open]) {
            continue;
        }
        let Some(close) = find_array_literal_end(&rest[open..]) else {
            continue;
        };
        return Some(&rest[open..=open + close]);
    }
    None
}

/// Returns the contents of the quoted string literal assigned to `marker` in an inline
/// script, without the surrounding quotes.
///
/// `marker` is the variable or object-key text preceding the literal. Only whitespace, `=`
/// and `:` may separate it from the opening quote, so an unrelated string further down the
/// page is never picked up; every occurrence of `marker` is tried in turn. Single and double
/// quotes are both accepted; escape sequences inside the literal are returned as written.
/// Returns `None` when no occurrence is followed by a terminated string literal.
pub(crate) fn find_js_string_literal<'a>(html: &'a str, marker: &str) -> Option<&'a str> {
    for (marker_start, _) in html.match_indices(marker) {
        let rest = &html[marker_start + marker.len()..];
        let Some(open) = rest.find(['"', '\'']) else {
            continue;
        };
        if !is_js_assignment_gap(&rest[..open]) {
            continue;
        }
        let quote = rest.as_bytes()[open];
        let body = &rest[open + 1..];
        let Some(close) = find_string_literal_end(body, quote) else {
            continue;
        };
        return Some(&body[..close]);
    }
    None
}

/// `true` when `text` holds nothing but the punctuation that may sit between a JS variable or
/// object key and the value assigned to it.
fn is_js_assignment_gap(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_whitespace() || byte == b'=' || byte == b':')
}

/// Offset of the `]` closing the leading `[` of `text`, honoring nesting and skipping
/// brackets inside single- or double-quoted strings. Returns `None` when unbalanced.
///
/// Exposed for callers that locate the opening bracket themselves and therefore cannot use
/// `find_js_array_literal`; `text` must start at that bracket.
pub(crate) fn find_array_literal_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut quote = b'"';
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == quote {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => {
                in_string = true;
                quote = byte;
            }
            b'[' => depth += 1,
            b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Offset of the `quote` byte closing a JS string literal whose opening quote has already
/// been consumed, honoring backslash escapes. Returns `None` when the literal is unterminated.
fn find_string_literal_end(text: &str, quote: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\\' {
            index += 2;
            continue;
        }
        if byte == quote {
            return Some(index);
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_html_tags_reads_names_attrs_and_end_flag() {
        let html = "<!doctype html><div class=\"a\"><img src='x.png'>text</div>";
        let tags = extract_html_tags(html);
        let names: Vec<&str> = tags.iter().map(|tag| tag.name).collect();
        assert_eq!(names, vec!["div", "img", "div"]);
        assert_eq!(tags[0].attrs, "class=\"a\"");
        assert!(!tags[0].is_end);
        assert!(tags[2].is_end);
        assert_eq!(tags[2].attrs, "");
    }

    #[test]
    fn extract_html_tags_skips_comments_and_declarations() {
        let tags = extract_html_tags("<!-- <img src=a.png> --><?xml v?><br>");
        // Quirk pinned on purpose: the `!` block ends at the FIRST '>', which swallows the
        // commented-out `<img>`; the leftover `-->` holds no '<' and is skipped as text.
        let names: Vec<&str> = tags.iter().map(|tag| tag.name).collect();
        assert_eq!(names, vec!["br"]);
    }

    #[test]
    fn extract_html_tags_keeps_self_closing_slash_in_name() {
        // Quirk pinned on purpose: `<br/>` has no whitespace, so the name is `br/`.
        let tags = extract_html_tags("<br/>");
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "br/");
        assert!(!tags[0].is_end);
    }

    #[test]
    fn get_html_attr_handles_quotes_case_and_bare_values() {
        let attrs = "src='a.png' DATA-URL=b.jpg alt=\"c d\" hidden";
        assert_eq!(get_html_attr(attrs, "src"), Some("a.png"));
        assert_eq!(get_html_attr(attrs, "data-url"), Some("b.jpg"));
        assert_eq!(get_html_attr(attrs, "ALT"), Some("c d"));
        assert_eq!(get_html_attr(attrs, "hidden"), None);
        assert_eq!(get_html_attr(attrs, "missing"), None);
    }

    #[test]
    fn get_html_attr_returns_first_match() {
        assert_eq!(get_html_attr("src=a.png src=b.png", "src"), Some("a.png"));
    }

    #[test]
    fn html_unescape_expands_known_entities() {
        assert_eq!(
            html_unescape("&quot;a&#39;b&amp;c&lt;d&gt;e&#34;f&#x27;"),
            "\"a'b&c<d>e\"f'"
        );
        // Quirk pinned on purpose: `&amp;` is expanded last, so `&amp;quot;` ends up as
        // `&quot;` instead of a literal `&amp;quot;`.
        assert_eq!(html_unescape("&amp;quot;"), "&quot;");
    }

    #[test]
    fn collect_anchor_hrefs_containing_filters_and_dedupes() {
        let html = "<a href='/read/1'>a</a><a href=\"/read/1\">dup</a>\
                    <a href='/other'>b</a><a>no href</a>";
        let urls = collect_anchor_hrefs_containing(html, "https://example.com/list", "/read/");
        assert_eq!(urls, vec!["https://example.com/read/1".to_string()]);
    }

    #[test]
    fn collect_https_json_strings_stops_at_delimiters() {
        let text = "[\"https://cdn.example/a.jpg\",'http://cdn.example/b.png' ]";
        assert_eq!(
            collect_https_json_strings(text),
            vec![
                "https://cdn.example/a.jpg".to_string(),
                "http://cdn.example/b.png".to_string(),
            ]
        );
    }

    #[test]
    fn find_js_array_literal_returns_the_bracketed_literal() {
        let html = r#"<script>var pages = ["a.jpg","b.jpg"];</script>"#;
        assert_eq!(
            find_js_array_literal(html, "var pages"),
            Some(r#"["a.jpg","b.jpg"]"#)
        );
    }

    #[test]
    fn find_js_array_literal_returns_none_without_the_marker() {
        assert_eq!(
            find_js_array_literal(
                "<html><body><p>Chapter not found</p></body></html>",
                "var pages"
            ),
            None
        );
    }

    #[test]
    fn find_js_array_literal_reads_an_empty_array() {
        assert_eq!(
            find_js_array_literal("<script>var pages = [];</script>", "var pages"),
            Some("[]")
        );
    }

    #[test]
    fn find_js_array_literal_accepts_a_quoted_object_key_and_nesting() {
        let html = r#"var gData = { 'images' : [["a.jpg"],["b.jpg"]], 'pageCount' : 2 };"#;
        assert_eq!(
            find_js_array_literal(html, "'images'"),
            Some(r#"[["a.jpg"],["b.jpg"]]"#)
        );
    }

    #[test]
    fn find_js_array_literal_ignores_brackets_and_quotes_inside_strings() {
        // A `]`, a foreign quote and an escaped quote all sit inside quoted entries and must
        // not terminate the literal.
        let html = r#"var pages = ["a]b","c'd","e\"f"];"#;
        assert_eq!(
            find_js_array_literal(html, "var pages"),
            Some(r#"["a]b","c'd","e\"f"]"#)
        );
    }

    #[test]
    fn find_js_array_literal_rejects_a_non_assignment_gap() {
        // The marker holds no array, so the later unrelated array must not be adopted: only
        // whitespace, `=` and `:` may separate the marker from the literal.
        let html = r#"var pages = null; var unrelated = ["/ads/1.png"];"#;
        assert_eq!(find_js_array_literal(html, "var pages"), None);
    }

    #[test]
    fn find_js_array_literal_retries_across_marker_occurrences() {
        // The first occurrence is a mention, not an assignment; the scanner must keep going.
        let html = r#"if (pageList) { render(); } var pageList = ["a.jpg"];"#;
        assert_eq!(
            find_js_array_literal(html, "pageList"),
            Some(r#"["a.jpg"]"#)
        );
    }

    #[test]
    fn find_js_array_literal_returns_none_when_unbalanced() {
        assert_eq!(
            find_js_array_literal(r#"<script>var pages = ["a.jpg""#, "var pages"),
            None
        );
    }

    #[test]
    fn find_js_string_literal_returns_the_contents_without_quotes() {
        let html = r#"<script>var chapImages = "a.jpg,b.jpg";</script>"#;
        assert_eq!(
            find_js_string_literal(html, "var chapImages"),
            Some("a.jpg,b.jpg")
        );
    }

    #[test]
    fn find_js_string_literal_returns_none_without_the_marker() {
        assert_eq!(
            find_js_string_literal(
                "<html><body><p>Chapter not found</p></body></html>",
                "var chapImages"
            ),
            None
        );
    }

    #[test]
    fn find_js_string_literal_reads_an_empty_string() {
        assert_eq!(
            find_js_string_literal(r#"<script>var chapImages = "";</script>"#, "var chapImages"),
            Some("")
        );
    }

    #[test]
    fn find_js_string_literal_keeps_escaped_and_foreign_quotes_inside() {
        // Single-quoted literal: the escaped `'` and the plain `"` must not terminate it, and
        // the escape is returned as written.
        let html = "var chapImages = 'a\\'b\"c';";
        assert_eq!(
            find_js_string_literal(html, "var chapImages"),
            Some("a\\'b\"c")
        );
    }

    #[test]
    fn find_js_string_literal_rejects_a_non_assignment_gap() {
        let html = r#"var chapImages = null; var unrelated = "https://ads.example.net/a.jpg";"#;
        assert_eq!(find_js_string_literal(html, "var chapImages"), None);
    }

    #[test]
    fn find_js_string_literal_retries_across_marker_occurrences() {
        let html = r#"log(chapImages); var chapImages = "a.jpg";"#;
        assert_eq!(find_js_string_literal(html, "chapImages"), Some("a.jpg"));
    }

    #[test]
    fn find_js_string_literal_returns_none_when_unterminated() {
        assert_eq!(
            find_js_string_literal(r#"<script>var chapImages = "a.jpg;"#, "var chapImages"),
            None
        );
    }

    #[test]
    fn find_array_literal_end_reports_the_closing_bracket_offset() {
        assert_eq!(find_array_literal_end(r#"["a]b",["c"]]tail"#), Some(12));
        assert_eq!(find_array_literal_end(r#"["a.jpg","#), None);
    }
}
