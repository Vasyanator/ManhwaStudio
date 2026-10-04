/*
File: crates/ms-ai-api/src/image_edit/multipart.rs

Purpose:
A pure `multipart/form-data` body builder (RFC 7578) for the image-edit adapters whose API
takes form uploads (`OpenAI`-shaped `/images/edits`, Ideogram). The HTTP client in use
(`ureq` 2) has none.

Key structures:
- MultipartForm (builder), MultipartBody (finished bytes + content type)

Notes:
The boundary is chosen at `finish()` as the first `ManhwaStudioFormBoundary{n}x` that occurs
in no part's bytes, so it can never collide with an image or prompt and the output is
deterministic (testable). Field names and file names are adapter constants; a name with a
quote, backslash or line break is refused instead of being escaped.
*/

use super::error::ImageEditError;

/// One form part.
#[derive(Debug, Clone)]
struct Part {
    name: &'static str,
    file: Option<(&'static str, &'static str)>,
    data: Vec<u8>,
}

/// A form being built. Parts are emitted in insertion order.
#[derive(Debug, Clone, Default)]
pub struct MultipartForm {
    parts: Vec<Part>,
}

/// A finished form body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartBody {
    /// The `Content-Type` header value, `multipart/form-data; boundary=...`.
    pub content_type: String,
    /// The encoded body.
    pub bytes: Vec<u8>,
}

impl MultipartForm {
    /// An empty form.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a text field (UTF-8, sent as is; line breaks are allowed in the value).
    #[must_use]
    pub fn text(mut self, name: &'static str, value: &str) -> Self {
        self.parts.push(Part { name, file: None, data: value.as_bytes().to_vec() });
        self
    }

    /// Adds a file field with its file name and media type (e.g. `image/png`).
    #[must_use]
    pub fn file(mut self, name: &'static str, file_name: &'static str, content_type: &'static str, data: Vec<u8>) -> Self {
        self.parts.push(Part { name, file: Some((file_name, content_type)), data });
        self
    }

    /// Encodes the form.
    ///
    /// # Errors
    /// `ImageEditError::RequestBuild` when a field name, file name or media type contains a
    /// `"`, `\`, CR or LF (they cannot be stated in a header line without escaping).
    pub fn finish(self) -> Result<MultipartBody, ImageEditError> {
        for part in &self.parts {
            let mut header_values = vec![part.name];
            if let Some((file_name, content_type)) = part.file {
                header_values.push(file_name);
                header_values.push(content_type);
            }
            if let Some(bad) = header_values.into_iter().find(|value| value.is_empty() || value.contains(['"', '\\', '\r', '\n'])) {
                return Err(ImageEditError::RequestBuild { detail: format!("invalid multipart header value {bad:?}") });
            }
        }
        let boundary = self.pick_boundary()?;
        let mut bytes = Vec::new();
        for part in &self.parts {
            bytes.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match part.file {
                Some((file_name, content_type)) => {
                    bytes.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"; filename=\"{file_name}\"\r\nContent-Type: {content_type}\r\n\r\n", part.name).as_bytes());
                }
                None => bytes.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", part.name).as_bytes()),
            }
            bytes.extend_from_slice(&part.data);
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Ok(MultipartBody { content_type: format!("multipart/form-data; boundary={boundary}"), bytes })
    }

    /// The first numbered boundary `ManhwaStudioFormBoundary{n}x` that occurs in no part's
    /// data.
    ///
    /// # Errors
    /// `ImageEditError::RequestBuild` if none of the `total_len + 1` candidates is free, which
    /// cannot happen: the closing `x` makes the candidates prefix-free, so at most one of them
    /// starts at each byte offset of the data.
    fn pick_boundary(&self) -> Result<String, ImageEditError> {
        let total_len: usize = self.parts.iter().map(|part| part.data.len()).sum();
        (0..=total_len)
            .map(|counter| format!("ManhwaStudioFormBoundary{counter}x"))
            .find(|candidate| !self.parts.iter().any(|part| contains(&part.data, candidate.as_bytes())))
            .ok_or_else(|| ImageEditError::RequestBuild { detail: "no free multipart boundary".to_string() })
    }
}

/// Whether `haystack` contains `needle` as a contiguous byte run.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::MultipartForm;
    use crate::image_edit::error::ImageEditError;

    #[test]
    fn frames_text_and_file_parts() {
        let body = MultipartForm::new().text("prompt", "remove\r\nthe text").file("image", "image.png", "image/png", vec![1, 2, 3]).finish().ok();
        let expected = b"--ManhwaStudioFormBoundary0x\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nremove\r\nthe text\r\n--ManhwaStudioFormBoundary0x\r\nContent-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\n\x01\x02\x03\r\n--ManhwaStudioFormBoundary0x--\r\n";
        assert_eq!(body.as_ref().map(|body| body.bytes.as_slice()), Some(&expected[..]));
        assert_eq!(body.map(|body| body.content_type).as_deref(), Some("multipart/form-data; boundary=ManhwaStudioFormBoundary0x"));
    }

    #[test]
    fn boundary_never_occurs_in_the_data() {
        let body = MultipartForm::new().text("prompt", "ManhwaStudioFormBoundary0x and ManhwaStudioFormBoundary1x").finish().ok();
        assert_eq!(body.map(|body| body.content_type).as_deref(), Some("multipart/form-data; boundary=ManhwaStudioFormBoundary2x"));
    }

    #[test]
    fn empty_form_is_just_the_closing_delimiter() {
        let body = MultipartForm::new().finish().ok().map(|body| body.bytes);
        assert_eq!(body.as_deref(), Some(&b"--ManhwaStudioFormBoundary0x--\r\n"[..]));
    }

    #[test]
    fn header_injection_is_refused() {
        assert!(matches!(MultipartForm::new().text("a\"b", "x").finish(), Err(ImageEditError::RequestBuild { .. })));
        assert!(matches!(MultipartForm::new().file("image", "a\r\nb.png", "image/png", Vec::new()).finish(), Err(ImageEditError::RequestBuild { .. })));
        assert!(matches!(MultipartForm::new().text("", "x").finish(), Err(ImageEditError::RequestBuild { .. })));
    }
}
