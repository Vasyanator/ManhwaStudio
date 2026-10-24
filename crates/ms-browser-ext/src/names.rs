/*
File: crates/ms-browser-ext/src/names.rs

Purpose:
File names this crate reserves inside the unpacked extension folder. Shared verbatim with the
build script (`build.rs` includes this file through `#[path]`), so the bundle builder and the
installer can never disagree about them.

Key items:
- BUILD_INFO_FILE_NAME — `{build_id, protocol}`, added to the bundle by build.rs, read by the
  extension (`browser-extension/src/core/protocol.ts` `BUILD_INFO_FILE_NAME`)
- STAMP_FILE_NAME — the installer's record of the unpacked bundle (`{build_id, files}`)
*/

/// Bundle file carrying `{"build_id": .., "protocol": ..}`; the extension announces its
/// `build_id` from it. Mirrors `BUILD_INFO_FILE_NAME` in `browser-extension/src/core/protocol.ts`.
pub const BUILD_INFO_FILE_NAME: &str = "build-info.json";

/// Installer stamp `{"build_id": .., "files": [..]}`, written LAST by `install`; the folder is
/// complete for a bundle exactly when this stamp names its `build_id`. Never part of a bundle.
pub const STAMP_FILE_NAME: &str = "ms-extension-stamp.json";
