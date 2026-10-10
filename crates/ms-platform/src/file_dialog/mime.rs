/*
File: file_dialog/mime.rs

Purpose:
Pure mapping from dialog filters (extensions) to the MIME types the Android Storage Access
Framework filters by.

Key functions:
- mime_types_for(): filters -> deduplicated MIME list, or the catch-all type.

Notes:
SAF filters by MIME type only. An extension without a well-known type (`mschapter`,
`safetensors`, ...) cannot be expressed, so ONE unknown extension widens the whole request to
the catch-all type: hiding a file the user wants is worse than showing extra files.
*/

use super::FileFilter;

/// The catch-all MIME type.
const ANY: &str = "*/*";

/// The MIME types that cover `filters`, in first-seen order without duplicates. No filters, or
/// any extension without a known type, yields `["*/*"]`.
#[must_use]
pub fn mime_types_for(filters: &[FileFilter]) -> Vec<&'static str> {
    let mut types: Vec<&'static str> = Vec::new();
    for extension in filters.iter().flat_map(|filter| filter.extensions.iter()) {
        let Some(mime) = mime_for_extension(extension) else {
            return vec![ANY];
        };
        if !types.contains(&mime) {
            types.push(mime);
        }
    }
    if types.is_empty() {
        types.push(ANY);
    }
    types
}

/// The well-known MIME type of a normalized (lowercase, dotless) extension.
fn mime_for_extension(extension: &str) -> Option<&'static str> {
    let mime = match extension {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "tif" | "tiff" => "image/tiff",
        "psd" | "psb" => "image/vnd.adobe.photoshop",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "rar" => "application/vnd.rar",
        "7z" => "application/x-7z-compressed",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        "html" | "htm" => "text/html",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        _ => return None,
    };
    Some(mime)
}

#[cfg(test)]
mod tests {
    use super::super::{DialogKind, FileDialogRequest};
    use super::mime_types_for;

    fn filters_of(request: &FileDialogRequest) -> Vec<&'static str> {
        mime_types_for(request.filters())
    }

    #[test]
    fn known_extensions_map_and_deduplicate() {
        let request = FileDialogRequest::new(DialogKind::OpenFiles)
            .filter("Images", &["png", "jpg", "jpeg"])
            .filter("More", &["JPG", "psd", "psb"]);
        assert_eq!(filters_of(&request), vec!["image/png", "image/jpeg", "image/vnd.adobe.photoshop"]);
    }

    #[test]
    fn unknown_extension_widens_to_any() {
        let request = FileDialogRequest::new(DialogKind::OpenFile).filter("Images", &["png"]).filter("Chapters", &["mschapter"]);
        assert_eq!(filters_of(&request), vec!["*/*"]);
    }

    #[test]
    fn no_filter_means_any() {
        assert_eq!(filters_of(&FileDialogRequest::new(DialogKind::OpenFile)), vec!["*/*"]);
    }

    #[test]
    fn every_call_site_family_has_a_type() {
        for extension in ["webp", "bmp", "tif", "tiff", "zip", "rar", "7z", "tar", "gz", "tgz", "json", "txt", "log", "html", "htm", "pdf", "docx", "gif"] {
            let request = FileDialogRequest::new(DialogKind::OpenFile).filter("x", &[extension]);
            assert_ne!(filters_of(&request), vec!["*/*"], "{extension}");
        }
    }
}
