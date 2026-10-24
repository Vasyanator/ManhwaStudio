/*
File: crates/ms-browser-ext/src/bundle.rs

Purpose:
The extension bundle embedded at build time. `build.rs` generates `OUT_DIR/bundle.rs`, a
single `EMBEDDED` value: either the full file list (every file `include_bytes!`-ed from the
build's staging directory) or the reason the build has no extension (node/npm missing,
`MS_SKIP_BROWSER_EXTENSION=1`, a failed npm build).

Key items:
- Bundle — `build_id` + `(relative path, bytes)` list, sorted by path
- NotIncluded — why this build carries no extension
- bundle() — the embedded value

Notes:
A bundle always contains `manifest.json` and `build-info.json` and never `bridge.json` (the
bridge's pairing file) or the installer stamp; build.rs refuses to embed anything else.
*/

use std::fmt;

/// One built extension: its content identity and its files.
///
/// `files` holds `/`-separated paths relative to the extension root, sorted, unique; `build_id`
/// is `derive::build_id_of` over every file except `build-info.json` (which carries it).
pub struct Bundle {
    /// 32 lowercase hex digits; changes whenever any bundled byte changes.
    pub build_id: &'static str,
    /// `(relative path, content)` of every file of the unpacked extension.
    pub files: &'static [(&'static str, &'static [u8])],
}

impl fmt::Debug for Bundle {
    // The file contents are hundreds of kilobytes; Debug shows the identity and the paths only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bundle")
            .field("build_id", &self.build_id)
            .field("files", &self.files.iter().map(|(path, bytes)| (path, bytes.len())).collect::<Vec<_>>())
            .finish()
    }
}

/// This build carries no extension bundle; `reason` says why (for logs and the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the browser extension is not included in this build: {reason}")]
pub struct NotIncluded {
    /// Short English reason, e.g. "node was not found" or `MS_SKIP_BROWSER_EXTENSION=1`.
    pub reason: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/bundle.rs"));

/// The bundle embedded in this build.
///
/// # Errors
/// `NotIncluded` when the build had no node/npm, was told to skip the extension, or the npm
/// build failed (the build printed a `cargo:warning` with details).
pub fn bundle() -> Result<&'static Bundle, NotIncluded> {
    EMBEDDED.as_ref().map_err(|not_included| *not_included)
}

#[cfg(test)]
mod tests {
    use super::bundle;
    use crate::derive::build_id_of;
    use crate::names::{BUILD_INFO_FILE_NAME, STAMP_FILE_NAME};

    #[test]
    fn embedded_bundle_is_consistent_or_explains_its_absence() {
        let bundle = match bundle() {
            Ok(bundle) => bundle,
            Err(not_included) => {
                // A build without node/npm (or with MS_SKIP_BROWSER_EXTENSION=1) is valid; it
                // must still say why.
                assert_ne!(not_included.reason, "");
                return;
            }
        };
        let paths: Vec<&str> = bundle.files.iter().map(|(path, _)| *path).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(paths, sorted, "files are sorted and unique");
        for required in ["manifest.json", BUILD_INFO_FILE_NAME, "background.js", "icons/icon-128.png"] {
            assert!(paths.contains(&required), "bundle lacks {required}");
        }
        for forbidden in ["bridge.json", STAMP_FILE_NAME] {
            assert!(!paths.contains(&forbidden), "bundle must not contain {forbidden}");
        }
        assert!(paths.iter().all(|p| !p.starts_with('.') && !p.contains('\\')), "{paths:?}");

        let content: Vec<(&str, &[u8])> = bundle.files.iter().filter(|(path, _)| *path != BUILD_INFO_FILE_NAME).map(|(path, bytes)| (*path, *bytes)).collect();
        assert_eq!(bundle.build_id, build_id_of(&content));

        // Test-only: a missing build-info.json is exactly the failure this test reports.
        let Some((_, info_bytes)) = bundle.files.iter().find(|(path, _)| *path == BUILD_INFO_FILE_NAME) else {
            panic!("bundle lacks {BUILD_INFO_FILE_NAME}");
        };
        let info: serde_json::Value = serde_json::from_slice(info_bytes).unwrap_or_else(|err| panic!("{err:?}"));
        assert_eq!(info["build_id"], bundle.build_id);
        assert!(info["protocol"].as_u64().is_some_and(|p| p >= 1), "{info}");
    }
}
