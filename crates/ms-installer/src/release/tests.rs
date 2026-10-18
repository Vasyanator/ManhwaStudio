/*
File: crates/ms-installer/src/release/tests.rs

Purpose:
Unit tests of the release owner: release selection with and without `SHA256SUMS` (first
complete release wins, incomplete newer ones are skipped, empty tags skipped, malformed JSON),
selection of the program's own release by tag (a newer release is ignored, a missing or
incomplete own release is an error),
the strict `SHA256SUMS` parser (valid, every malformed shape, duplicates) and the per-asset pin
(entry present, entry missing, GitHub digest agreeing / disagreeing).

Notes:
JSON fixtures are inline; nothing touches the network or the file system.
*/

use super::*;

const BINARY: &str = "manhwastudio_rs";
const HASH_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const HASH_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// One release-list asset object.
fn asset_json(name: &str) -> String {
    format!(r#"{{"name":"{name}","browser_download_url":"https://example.invalid/{name}","size":10,"digest":null}}"#)
}

/// One release object with `tag` and the named assets.
fn release_json(tag: &str, assets: &[&str]) -> String {
    let assets: Vec<String> = assets.iter().map(|name| asset_json(name)).collect();
    format!(r#"{{"tag_name":"{tag}","name":"Release {tag}","assets":[{}]}}"#, assets.join(","))
}

fn list_json(releases: &[String]) -> String {
    format!("[{}]", releases.join(","))
}

fn github_asset(name: &str, digest: Option<&str>) -> GithubAsset {
    GithubAsset { name: name.to_owned(), browser_download_url: format!("https://example.invalid/{name}"), size: 10, digest: digest.map(str::to_owned) }
}

fn select(body: &str) -> Result<AppRelease, ReleaseSelectError> {
    select_app_release_for(body, BINARY)
}

#[test]
fn a_release_with_archive_binary_and_checksums_is_selected() {
    let body = list_json(&[release_json("330", &[APP_ZIP_ASSET_NAME, BINARY, APP_CHECKSUMS_ASSET_NAME, "manhwastudio_rs.exe"])]);
    let release = select(&body).unwrap_or_else(|err| panic!("selection failed: {err}"));
    assert_eq!(release.tag, "330");
    assert_eq!(release.archive.name, APP_ZIP_ASSET_NAME);
    assert_eq!(release.binary.name, BINARY);
    assert_eq!(release.checksums.asset.name, APP_CHECKSUMS_ASSET_NAME);
}

#[test]
fn a_release_without_checksums_is_never_selected() {
    let body = list_json(&[release_json("3.6.0", &[APP_ZIP_ASSET_NAME, BINARY])]);
    assert_eq!(select(&body), Err(ReleaseSelectError::NoUpdatableRelease { binary: BINARY.to_owned() }));
}

#[test]
fn the_first_complete_release_wins_and_incomplete_ones_are_skipped() {
    let body = list_json(&[
        // Newest, but published without the manifest: skipped, not taken unverified.
        release_json("340", &[APP_ZIP_ASSET_NAME, BINARY]),
        // No binary for this platform.
        release_json("335", &[APP_ZIP_ASSET_NAME, "manhwastudio_rs.exe", APP_CHECKSUMS_ASSET_NAME]),
        release_json("330", &[APP_ZIP_ASSET_NAME, BINARY, APP_CHECKSUMS_ASSET_NAME]),
        release_json("320", &[APP_ZIP_ASSET_NAME, BINARY, APP_CHECKSUMS_ASSET_NAME]),
    ]);
    assert_eq!(select(&body).map(|release| release.tag), Ok("330".to_owned()));
}

#[test]
fn an_empty_tag_falls_back_to_the_name_and_a_nameless_release_is_skipped() {
    let complete = format!("[{},{},{}]", asset_json(APP_ZIP_ASSET_NAME), asset_json(BINARY), asset_json(APP_CHECKSUMS_ASSET_NAME));
    let body = format!(r#"[{{"tag_name":"  ","name":null,"assets":{complete}}},{{"tag_name":null,"name":" 331 ","assets":{complete}}}]"#);
    assert_eq!(select(&body).map(|release| release.tag), Ok("331".to_owned()));
}

#[test]
fn a_malformed_release_list_is_reported() {
    assert!(matches!(select("{\"message\":\"rate limited\"}"), Err(ReleaseSelectError::Malformed(_))));
    assert!(matches!(select("not json"), Err(ReleaseSelectError::Malformed(_))));
    assert_eq!(select("[]"), Err(ReleaseSelectError::NoUpdatableRelease { binary: BINARY.to_owned() }));
}

#[test]
fn a_valid_manifest_parses_every_line() {
    let text = format!("{HASH_A}  ManhwaStudio.zip\n{HASH_B}  manhwastudio_rs\n");
    let entries = parse_sha256sums(&text).unwrap_or_else(|err| panic!("parse failed: {err}"));
    assert_eq!(entries.len(), 2);
    assert_eq!(entries.get("ManhwaStudio.zip").map(String::as_str), Some(HASH_A));
    assert_eq!(entries.get("manhwastudio_rs").map(String::as_str), Some(HASH_B));
}

#[test]
fn malformed_manifests_are_rejected_with_the_offending_line() {
    use ChecksumsParseErrorKind as Kind;
    let upper = HASH_A.to_ascii_uppercase();
    let short = &HASH_A[..63];
    let cases: Vec<(String, usize, Kind)> = vec![
        (String::new(), 1, Kind::Empty),
        ("\n".to_owned(), 1, Kind::BadHash),
        (format!("{HASH_A}  a.zip"), 1, Kind::MissingFinalNewline),
        (format!("{HASH_A}  a.zip\n{HASH_B}  b.zip"), 2, Kind::MissingFinalNewline),
        (format!("{upper}  a.zip\n"), 1, Kind::BadHash),
        (format!("{short}  a.zip\n"), 1, Kind::BadHash),
        (format!("{HASH_A} a.zip\n"), 1, Kind::BadSeparator),
        (format!("{HASH_A} *a.zip\n"), 1, Kind::BadSeparator),
        (format!("{HASH_A}\ta.zip\n"), 1, Kind::BadSeparator),
        (format!("{HASH_A}  *a.zip\n"), 1, Kind::BadName),
        (format!("{HASH_A}  dir/a.zip\n"), 1, Kind::BadName),
        (format!("{HASH_A}  \n"), 1, Kind::BadName),
        (format!("{HASH_A}   a.zip\n"), 1, Kind::BadName),
        (format!("{HASH_A}  a.zip\r\n"), 1, Kind::BadName),
        (format!("{HASH_A}  a.zip\n\n"), 2, Kind::BadHash),
        (format!("{HASH_A}  a.zip\n{HASH_B}  a.zip\n"), 2, Kind::DuplicateName),
        (format!("{}é{}  a.zip\n", &HASH_A[..30], &HASH_A[32..]), 1, Kind::BadHash),
    ];
    for (text, line, kind) in cases {
        assert_eq!(parse_sha256sums(&text), Err(ChecksumsParseError { line, kind }), "input {text:?}");
    }
}

fn checksums_330() -> ChecksumsAsset {
    ChecksumsAsset { tag: "330".to_owned(), asset: github_asset(APP_CHECKSUMS_ASSET_NAME, None) }
}

#[test]
fn a_malformed_downloaded_manifest_is_a_user_visible_error() {
    let checksums = checksums_330();
    let expected = tf!("installer.release.checksums_malformed_error", checksums = APP_CHECKSUMS_ASSET_NAME, tag = "330", line = 1);
    assert_eq!(checksums.parse(b"garbage\n"), Err(expected.clone()));
    assert_eq!(checksums.parse(&[0xff, 0xfe, b'\n']), Err(expected));
}

#[test]
fn the_pin_is_the_manifest_entry_of_the_asset() {
    let checksums = checksums_330().parse(format!("{HASH_A}  ManhwaStudio.zip\n{HASH_B}  manhwastudio_rs\n").as_bytes()).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(checksums.pin_for(&github_asset(APP_ZIP_ASSET_NAME, None)), Ok(HASH_A));
    assert_eq!(checksums.pin_for(&github_asset(BINARY, None)), Ok(HASH_B));
    // A GitHub digest that agrees, or one that is not a usable sha256, changes nothing.
    assert_eq!(checksums.pin_for(&github_asset(BINARY, Some(&format!("sha256:{HASH_B}")))), Ok(HASH_B));
    assert_eq!(checksums.pin_for(&github_asset(BINARY, Some("sha512:00"))), Ok(HASH_B));
}

#[test]
fn a_missing_entry_is_a_hard_error() {
    let checksums = checksums_330().parse(format!("{HASH_A}  ManhwaStudio.zip\n").as_bytes()).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(
        checksums.pin_for(&github_asset(BINARY, None)),
        Err(tf!("installer.release.checksum_missing_error", checksums = APP_CHECKSUMS_ASSET_NAME, tag = "330", asset = BINARY))
    );
}

#[test]
fn a_github_digest_that_disagrees_with_the_manifest_is_a_hard_error() {
    let checksums = checksums_330().parse(format!("{HASH_A}  manhwastudio_rs\n").as_bytes()).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(
        checksums.pin_for(&github_asset(BINARY, Some(&format!("sha256:{HASH_B}")))),
        Err(tf!("installer.release.checksum_conflict_error", checksums = APP_CHECKSUMS_ASSET_NAME, tag = "330", asset = BINARY))
    );
}

#[test]
fn the_continuation_takes_its_own_release_even_when_a_newer_one_exists() {
    let body = list_json(&[
        release_json("340", &[APP_ZIP_ASSET_NAME, BINARY, APP_CHECKSUMS_ASSET_NAME]),
        release_json("335", &[APP_ZIP_ASSET_NAME, BINARY, APP_CHECKSUMS_ASSET_NAME]),
        // The own release needs no platform binary: the program already is it.
        release_json("330", &[APP_ZIP_ASSET_NAME, APP_CHECKSUMS_ASSET_NAME]),
        release_json("320", &[APP_ZIP_ASSET_NAME, BINARY, APP_CHECKSUMS_ASSET_NAME]),
    ]);
    let release = select_app_release_by_tag(&body, "330").unwrap_or_else(|err| panic!("selection failed: {err}"));
    assert_eq!(release.tag, "330");
    assert_eq!(release.archive.name, APP_ZIP_ASSET_NAME);
    assert_eq!(release.archive.browser_download_url, format!("https://example.invalid/{APP_ZIP_ASSET_NAME}"));
    assert_eq!(release.checksums.asset.name, APP_CHECKSUMS_ASSET_NAME);
    // A `v` prefix on the tag is still the same version.
    let prefixed = list_json(&[release_json("v330", &[APP_ZIP_ASSET_NAME, APP_CHECKSUMS_ASSET_NAME])]);
    assert_eq!(select_app_release_by_tag(&prefixed, "330").map(|release| release.tag), Ok("v330".to_owned()));
}

#[test]
fn a_missing_or_incomplete_own_release_is_an_error() {
    let missing = ReleaseSelectError::OwnReleaseMissing { version: "330".to_owned() };
    let without_own = list_json(&[release_json("340", &[APP_ZIP_ASSET_NAME, BINARY, APP_CHECKSUMS_ASSET_NAME])]);
    assert_eq!(select_app_release_by_tag(&without_own, "330"), Err(missing.clone()));
    let own_without_manifest = list_json(&[release_json("330", &[APP_ZIP_ASSET_NAME, BINARY])]);
    assert_eq!(select_app_release_by_tag(&own_without_manifest, "330"), Err(missing.clone()));
    // A source build reports the Cargo fallback `330.0.0`, which is not the same version as tag `330`.
    let own = list_json(&[release_json("330", &[APP_ZIP_ASSET_NAME, APP_CHECKSUMS_ASSET_NAME])]);
    assert_eq!(select_app_release_by_tag(&own, "330.0.0"), Err(ReleaseSelectError::OwnReleaseMissing { version: "330.0.0".to_owned() }));
    assert!(matches!(select_app_release_by_tag("{}", "330"), Err(ReleaseSelectError::Malformed(_))));
    assert_eq!(
        missing.user_message(),
        tf!("installer.release.own_release_missing_error", version = "330", archive = APP_ZIP_ASSET_NAME, checksums = APP_CHECKSUMS_ASSET_NAME)
    );
}
