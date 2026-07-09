/*
File: crates/ms-i18n/tests/key_validation.rs

Purpose:
Guard against typo'd translation keys. This test genuinely scans the repository
source tree for `t!` / `tf!` / `tp!` invocations, extracts each string-literal
key, and asserts it exists in the reference catalog `en.json`. It also asserts
`ru.json` introduces no key that `en.json` lacks (en is the reference locale).

Notes:
- Almost no call sites exist yet, so the extracted set is expected to be (near)
  empty; the test must still pass on an empty result while performing a real scan.
- The `ms-i18n` crate's own directory is skipped: its unit tests deliberately
  call the macros with unknown keys (negative cases) that must NOT be validated
  here.
- The scanner skips comments and string/char literals so a macro name appearing
  inside a doc comment or a string does not produce a false positive.
*/

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

/// Loads the top-level keys of a locale JSON file (excluding the reserved
/// `_meta`) as a set.
fn load_keys(path: &Path) -> BTreeSet<String> {
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let value: serde_json::Value = serde_json::from_str(&source)
        .unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()));
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{} root is not a JSON object", path.display()));
    object
        .keys()
        .filter(|k| k.as_str() != "_meta")
        .cloned()
        .collect()
}

/// Recursively collects every `.rs` file under `dir`, skipping build/output and
/// vendored directories and the `ms-i18n` crate itself (`skip`).
fn collect_rs_files(dir: &Path, skip: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip build output, VCS metadata, virtualenvs, and any hidden dir.
            let skip_dir = matches!(
                name.as_ref(),
                "target" | "node_modules" | "venv" | ".git"
            ) || name.starts_with('.');
            if skip_dir {
                continue;
            }
            collect_rs_files(&path, skip, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Extracts the string-literal first argument of every `t!` / `tf!` / `tp!`
/// invocation in `src`, skipping comments and string/char literals so that macro
/// names inside them do not register.
fn extract_keys(src: &str, out: &mut Vec<String>) {
    let bytes = src.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        let c = bytes[i];

        // Line comment.
        if c == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
            i += 2;
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment (Rust allows nesting).
        if c == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            i += 2;
            let mut depth = 1;
            while i < n && depth > 0 {
                if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // Raw string literal: r"..." / r#"..."# (not preceded by an identifier char).
        if c == b'r' && i + 1 < n && (bytes[i + 1] == b'"' || bytes[i + 1] == b'#') {
            let prev_is_ident = i > 0
                && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if !prev_is_ident {
                let mut j = i + 1;
                let mut hashes = 0;
                while j < n && bytes[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                if j < n && bytes[j] == b'"' {
                    j += 1;
                    // Scan to the matching closing quote followed by `hashes` #.
                    while j < n {
                        if bytes[j] == b'"' {
                            let mut k = j + 1;
                            let mut seen = 0;
                            while k < n && seen < hashes && bytes[k] == b'#' {
                                seen += 1;
                                k += 1;
                            }
                            if seen == hashes {
                                j = k;
                                break;
                            }
                            j += 1;
                        } else {
                            j += 1;
                        }
                    }
                    i = j;
                    continue;
                }
            }
        }
        // Normal string literal.
        if c == b'"' {
            i += 1;
            while i < n {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Char literal or lifetime.
        if c == b'\'' {
            if i + 1 < n && bytes[i + 1] == b'\\' {
                // Escaped char literal: skip to the closing quote.
                let mut j = i + 2;
                while j < n && bytes[j] != b'\'' {
                    j += 1;
                }
                i = if j < n { j + 1 } else { j };
                continue;
            }
            if i + 2 < n && bytes[i + 2] == b'\'' {
                // Simple char literal 'a'.
                i += 3;
                continue;
            }
            // Lifetime / label: consume only the quote.
            i += 1;
            continue;
        }
        // Identifier: check for a `t!` / `tf!` / `tp!` macro invocation.
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &src[start..i];
            if matches!(ident, "t" | "tf" | "tp") && i < n && bytes[i] == b'!' {
                let mut k = i + 1;
                while k < n && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < n && matches!(bytes[k], b'(' | b'[' | b'{') {
                    k += 1;
                    while k < n && bytes[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    if k < n
                        && bytes[k] == b'"'
                        && let Some(key) = read_string_literal(src, k)
                    {
                        out.push(key);
                    }
                }
            }
            continue;
        }
        i += 1;
    }
}

/// Reads a normal double-quoted string literal starting at `open` (which must be
/// the opening `"`) and returns its unescaped contents.
fn read_string_literal(src: &str, open: usize) -> Option<String> {
    let bytes = src.as_bytes();
    let start = open + 1;
    let mut k = start;
    while k < bytes.len() {
        match bytes[k] {
            b'\\' => k += 2,
            b'"' => return Some(unescape(&src[start..k])),
            _ => k += 1,
        }
    }
    None
}

/// Minimal unescape for the escape sequences a translation key could contain.
/// Keys are dotted ASCII identifiers in practice, so this only needs the common
/// cases; unknown escapes are preserved literally.
fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[test]
fn every_macro_key_exists_in_en_and_ru_is_a_subset() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = crate_dir
        .parent()
        .and_then(Path::parent)
        .expect("ms-i18n crate must live two levels under the repo root")
        .to_path_buf();

    let en_keys = load_keys(&crate_dir.join("locales/en.json"));
    let ru_keys = load_keys(&crate_dir.join("locales/ru.json"));

    // ru.json must not introduce keys absent from the en.json reference.
    for key in &ru_keys {
        assert!(
            en_keys.contains(key),
            "ru.json key {key:?} is absent from the en.json reference catalog"
        );
    }

    // Scan the whole repo except this crate (its tests use unknown keys on purpose).
    let mut files = Vec::new();
    collect_rs_files(&repo_root, &crate_dir, &mut files);
    assert!(
        !files.is_empty(),
        "scan found no .rs files under {}; the walk is broken",
        repo_root.display()
    );

    let mut used_keys = Vec::new();
    for file in &files {
        if let Ok(src) = std::fs::read_to_string(file) {
            let before = used_keys.len();
            extract_keys(&src, &mut used_keys);
            // Attribute keys to their file for a useful failure message.
            for key in &used_keys[before..] {
                assert!(
                    en_keys.contains(key),
                    "translation key {key:?} used in {} is missing from en.json",
                    file.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod scanner_tests {
    use super::extract_keys;

    #[test]
    fn extracts_keys_from_all_three_macros() {
        let src = r#"
            let a = t!("k.one");
            let b = tf!("k.two", err = e);
            let c = tp!("k.three", n);
        "#;
        let mut keys = Vec::new();
        extract_keys(src, &mut keys);
        assert_eq!(keys, vec!["k.one", "k.two", "k.three"]);
    }

    #[test]
    fn ignores_macros_in_comments_and_strings() {
        let src = concat!(
            "// t!(\"in.line.comment\")\n",
            "/* tf!(\"in.block.comment\") */\n",
            "let s = \"t!(\\\"in.string\\\")\";\n",
            "let real = t!(\"real.key\");\n"
        );
        let mut keys = Vec::new();
        extract_keys(src, &mut keys);
        assert_eq!(keys, vec!["real.key"]);
    }

    #[test]
    fn does_not_match_format_or_assert() {
        let src = "format!(\"x\"); assert!(true); print!(\"y\");";
        let mut keys = Vec::new();
        extract_keys(src, &mut keys);
        assert!(keys.is_empty());
    }
}
