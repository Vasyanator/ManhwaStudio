/*
File: crates/ms-browser-bridge/src/mirror_tests.rs

Purpose:
Guards the Rust <-> TypeScript protocol mirror: `browser-extension/src/core/protocol.ts`
(`PROTOCOL_VERSION`, `CONFIG_FILE_NAME`, the error-code set), every golden message (each JSON file in
`browser-extension/protocol-fixtures/`), and the shared HMAC vector `auth_vector.json`.

Notes:
- The inputs are tracked files of the extension tree. While a file is absent (a checkout
  without the extension tree) the test logs one line and returns; once present it asserts.
- Files are read at run time (not `include_str!`) so their absence cannot break the build.
*/

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use crate::auth::{self, Secret};
use crate::protocol::{AppMessage, BridgeFile, CONFIG_FILE_NAME, ErrorCode, ExtMessage, PROTOCOL_VERSION};

fn extension_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../browser-extension")
}

/// Reads `path` relative to the extension root, or logs the skip and returns `None`.
fn read_or_skip(relative: &str) -> Option<String> {
    let path = extension_root().join(relative);
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("mirror test skipped: {} does not exist", path.display());
            None
        }
        Err(err) => panic!("could not read {}: {err}", path.display()),
    }
}

/// The text after `anchor` (e.g. `export const NAME`) up to the end of its statement (`;`).
/// Anchoring on the declaration keyword skips mentions of the name in comments.
fn declaration<'a>(source: &'a str, anchor: &str) -> &'a str {
    let start = source.find(anchor).unwrap_or_else(|| panic!("protocol.ts has no `{anchor}`"));
    let rest = &source[start + anchor.len()..];
    let end = rest.find(';').unwrap_or(rest.len());
    &rest[..end]
}

/// Every single- or double-quoted string literal in `text`.
fn quoted_strings(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' {
            let literal: String = chars.by_ref().take_while(|&d| d != c).collect();
            found.push(literal);
        }
    }
    found
}

#[test]
fn protocol_ts_mirrors_constants_and_error_codes() {
    let Some(source) = read_or_skip("src/core/protocol.ts") else { return };

    let version_decl = declaration(&source, "export const PROTOCOL_VERSION");
    let digits: String = version_decl.chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
    assert_eq!(digits.parse::<u32>().ok(), Some(PROTOCOL_VERSION), "PROTOCOL_VERSION differs: `{version_decl}`");

    let file_decl = declaration(&source, "export const CONFIG_FILE_NAME");
    assert_eq!(quoted_strings(file_decl).first().map(String::as_str), Some(CONFIG_FILE_NAME), "CONFIG_FILE_NAME differs: `{file_decl}`");

    // The error-code list: `export const ERROR_CODES = [...] as const;`.
    let ts_codes: BTreeSet<String> = quoted_strings(declaration(&source, "export const ERROR_CODES")).into_iter().collect();
    let rust_codes: BTreeSet<String> = ErrorCode::ALL.iter().map(|code| code.as_str().to_owned()).collect();
    assert_eq!(ts_codes, rust_codes, "error-code sets differ between protocol.ts and protocol.rs");
}

/// Every non-null field of `fixture` survives a parse + serialize round trip with the same
/// value: catches a misspelled field (`retryAfterMs`) that serde would silently ignore.
fn assert_known_fields(name: &str, at: &str, fixture: &Value, reserialized: &Value) {
    match (fixture, reserialized) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected.iter().filter(|(_, value)| !value.is_null()) {
                let path = format!("{at}.{key}");
                let other = actual.get(key).unwrap_or_else(|| panic!("{name}: field {path} is unknown to protocol.rs"));
                assert_known_fields(name, &path, value, other);
            }
        }
        _ => assert_eq!(fixture, reserialized, "{name}: field {at} changes on a round trip"),
    }
}

/// Checks one fixture value: a message of either direction, or the pairing file.
fn check_fixture(name: &str, value: &Value) {
    if let Value::Array(items) = value {
        for item in items {
            check_fixture(name, item);
        }
        return;
    }
    match value.get("type").and_then(Value::as_str) {
        Some("hello" | "response" | "event") => {
            let parsed: ExtMessage = serde_json::from_value(value.clone()).unwrap_or_else(|err| panic!("{name}: not an extension message: {err}"));
            assert_known_fields(name, "", value, &serde_json::to_value(&parsed).expect("serialize"));
        }
        Some("challenge" | "welcome" | "reject" | "request" | "cancel" | "reload") => {
            let parsed: AppMessage = serde_json::from_value(value.clone()).unwrap_or_else(|err| panic!("{name}: not an app message: {err}"));
            assert_known_fields(name, "", value, &serde_json::to_value(&parsed).expect("serialize"));
        }
        // `ping` / `pong` exist in both directions with the same shape.
        Some("ping" | "pong") => {
            serde_json::from_value::<ExtMessage>(value.clone()).unwrap_or_else(|err| panic!("{name}: {err}"));
            serde_json::from_value::<AppMessage>(value.clone()).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
        Some(other) => panic!("{name}: unknown message type {other:?}"),
        None => {
            let file: BridgeFile = serde_json::from_value(value.clone()).unwrap_or_else(|err| panic!("{name}: neither a message nor bridge.json: {err}"));
            assert_eq!(file.protocol, PROTOCOL_VERSION, "{name}");
            assert!(Secret::from_hex(&file.secret).is_some(), "{name}: secret is not 64 hex digits");
        }
    }
}

#[test]
fn every_protocol_fixture_deserializes() {
    let dir = extension_root().join("protocol-fixtures");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("mirror test skipped: {} does not exist", dir.display());
            return;
        }
        Err(err) => panic!("could not list {}: {err}", dir.display()),
    };
    let mut checked = 0_usize;
    for entry in entries {
        let path = entry.expect("fixture dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        if path.extension().and_then(|e| e.to_str()) != Some("json") || name == "auth_vector.json" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{name}: {err}"));
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|err| panic!("{name}: invalid JSON: {err}"));
        check_fixture(&name, &value);
        checked += 1;
    }
    if checked == 0 {
        eprintln!("mirror test skipped: no message fixtures in {} yet", dir.display());
    }
}

#[test]
fn auth_vector_matches_the_typescript_side() {
    let Some(text) = read_or_skip("protocol-fixtures/auth_vector.json") else { return };
    let vector: Value = serde_json::from_str(&text).expect("auth_vector.json is JSON");
    let field = |key: &str| vector[key].as_str().unwrap_or_else(|| panic!("auth_vector.json lacks {key}")).to_owned();
    let secret = Secret::from_hex(&field("secret")).expect("auth_vector secret is 64 hex digits");
    let (nonce, client_nonce) = (field("nonce"), field("client_nonce"));
    assert_eq!(auth::client_auth(&secret, &nonce, &client_nonce), field("client_auth").to_ascii_lowercase());
    assert_eq!(auth::server_auth(&secret, &nonce, &client_nonce), field("server_auth").to_ascii_lowercase());
    assert!(auth::verify_client_auth(&secret, &nonce, &client_nonce, &field("client_auth")));
}
