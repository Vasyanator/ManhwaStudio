/*
File: src/launcher/new_project/quick_download/base64.rs

Purpose:
Minimal standard-alphabet base64 decoder used by the quick downloader.

Key functions:
- base64_decode()

Notes:
Generic codec, not site knowledge: it lives above `sites/` even though only one site parser
currently needs it. Deliberately permissive, see the declaration comment.
*/

/// Decodes standard-alphabet base64, ignoring any character outside the alphabet and
/// stopping at the first `=` padding byte. Trailing bits that do not complete a byte are
/// dropped.
///
/// Returns `Some` for every input (including garbage, which decodes to an empty vector);
/// the `Option` exists for call-site symmetry with fallible decoders.
pub(crate) fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut output = Vec::new();
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_known_vectors() {
        assert_eq!(base64_decode("TWFu").as_deref(), Some(&b"Man"[..]));
        assert_eq!(base64_decode("TWE=").as_deref(), Some(&b"Ma"[..]));
        assert_eq!(base64_decode("TQ==").as_deref(), Some(&b"M"[..]));
        assert_eq!(
            base64_decode("aHR0cHM6Ly8=").as_deref(),
            Some(&b"https://"[..])
        );
        assert_eq!(base64_decode("Pz8/").as_deref(), Some(&b"???"[..]));
    }

    #[test]
    fn ignores_unknown_characters() {
        // Whitespace, newlines and URL-safe characters are skipped, not rejected.
        assert_eq!(base64_decode("TW\nF u").as_deref(), Some(&b"Man"[..]));
        assert_eq!(base64_decode("!!!").as_deref(), Some(&b""[..]));
        assert_eq!(base64_decode("").as_deref(), Some(&b""[..]));
    }

    #[test]
    fn stops_at_padding_and_drops_incomplete_tail() {
        // Everything after the first '=' is ignored, even valid base64.
        assert_eq!(base64_decode("TWFu=TWFu").as_deref(), Some(&b"Man"[..]));
        // A lone leftover sextet cannot form a byte and is dropped.
        assert_eq!(base64_decode("T").as_deref(), Some(&b""[..]));
    }
}
