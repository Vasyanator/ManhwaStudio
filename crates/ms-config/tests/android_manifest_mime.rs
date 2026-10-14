/*
File: crates/ms-config/tests/android_manifest_mime.rs

Purpose:
Guards the single owner of the readable input image types: the Android "Open with" intent
filter (ACTION_VIEW of `LaunchActivity` in `android/app/src/main/AndroidManifest.xml`) must
declare exactly the MIME types of `ms_config::single_image::INPUT_FILE_TYPES`, no more, no less.

Notes:
The manifest is the tracked source file, embedded with `include_str!`, so the test needs no
Android toolchain and reads nothing at run time. The parser is deliberately narrow: it takes the
`<intent-filter>` blocks of the manifest and the `android:mimeType` attributes of the one that
names `android.intent.action.VIEW`.
*/

use std::collections::BTreeSet;

use ms_config::single_image::INPUT_FILE_TYPES;

/// The tracked Android manifest.
const MANIFEST: &str = include_str!("../../../android/app/src/main/AndroidManifest.xml");

/// The `<intent-filter>…</intent-filter>` blocks of `manifest`, in document order.
fn intent_filters(manifest: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = manifest;
    while let Some(start) = rest.find("<intent-filter>") {
        let after = &rest[start..];
        let Some(end) = after.find("</intent-filter>") else {
            panic!("unterminated <intent-filter> in AndroidManifest.xml");
        };
        blocks.push(&after[..end]);
        rest = &after[end..];
        rest = &rest["</intent-filter>".len()..];
    }
    blocks
}

/// Every `android:mimeType="…"` value inside `block`.
fn mime_types(block: &str) -> BTreeSet<&str> {
    const ATTRIBUTE: &str = "android:mimeType=\"";
    let mut types = BTreeSet::new();
    let mut rest = block;
    while let Some(start) = rest.find(ATTRIBUTE) {
        let value = &rest[start + ATTRIBUTE.len()..];
        let Some(end) = value.find('"') else {
            panic!("unterminated android:mimeType attribute in AndroidManifest.xml");
        };
        types.insert(&value[..end]);
        rest = &value[end..];
    }
    types
}

#[test]
fn view_filter_declares_exactly_the_input_file_types() {
    let view_filters: Vec<&str> = intent_filters(MANIFEST).into_iter().filter(|block| block.contains("\"android.intent.action.VIEW\"")).collect();
    assert_eq!(view_filters.len(), 1, "expected exactly one ACTION_VIEW intent filter in AndroidManifest.xml");
    let declared = mime_types(view_filters[0]);
    let expected: BTreeSet<&str> = INPUT_FILE_TYPES.iter().map(|file_type| file_type.mime).collect();
    assert_eq!(declared, expected, "the Android \"Open with\" MIME list must equal ms_config INPUT_FILE_TYPES");
}

#[test]
fn intent_filter_parser_reads_every_block_and_type() {
    let sample = "<a><intent-filter><data android:mimeType=\"image/png\" /><data android:mimeType=\"image/gif\" /></intent-filter>\
                  <intent-filter><data android:mimeType=\"image/*\" /></intent-filter></a>";
    let blocks = intent_filters(sample);
    assert_eq!(blocks.len(), 2);
    assert_eq!(mime_types(blocks[0]), BTreeSet::from(["image/gif", "image/png"]));
    assert_eq!(mime_types(blocks[1]), BTreeSet::from(["image/*"]));
}
