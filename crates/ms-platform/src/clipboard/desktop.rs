/*
File: clipboard/desktop.rs

Purpose:
Desktop clipboard reads over `arboard`.

Notes:
A fresh `arboard::Clipboard` per read, as the previous call sites did: arboard opens the native
clipboard only for the transfer. `ContentNotAvailable` (empty clipboard or another format) maps
to `Ok(None)` for text/HTML and to `NoContent` for image/file list; every other arboard error
is `Backend` with arboard's own description.
*/

use std::path::PathBuf;

use super::{ClipboardError, ClipboardRgba};

pub(super) fn read_text() -> Result<Option<String>, ClipboardError> {
    optional(open()?.get_text())
}

pub(super) fn read_image() -> Result<ClipboardRgba, ClipboardError> {
    let image = open()?.get_image().map_err(map_required)?;
    Ok(ClipboardRgba { width: image.width, height: image.height, rgba: image.bytes.into_owned() })
}

pub(super) fn read_file_list() -> Result<Vec<PathBuf>, ClipboardError> {
    open()?.get().file_list().map_err(map_required)
}

pub(super) fn read_html() -> Result<Option<String>, ClipboardError> {
    optional(open()?.get().html())
}

fn open() -> Result<arboard::Clipboard, ClipboardError> {
    arboard::Clipboard::new().map_err(|err| backend_error(&err))
}

/// `ContentNotAvailable` means "none of this kind" for the optional reads.
fn optional(result: Result<String, arboard::Error>) -> Result<Option<String>, ClipboardError> {
    match result {
        Ok(text) => Ok(Some(text)),
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(err) => Err(backend_error(&err)),
    }
}

/// `ContentNotAvailable` means `NoContent` for the reads that must produce a value.
fn map_required(err: arboard::Error) -> ClipboardError {
    match err {
        arboard::Error::ContentNotAvailable => ClipboardError::NoContent,
        other => backend_error(&other),
    }
}

fn backend_error(err: &arboard::Error) -> ClipboardError {
    ClipboardError::Backend { detail: err.to_string() }
}
