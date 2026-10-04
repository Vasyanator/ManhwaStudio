/*
File: crates/ms-ai-api/src/encoding.rs

Purpose:
The one standard base64 codec (RFC 4648 alphabet, `=` padding) of the crate: the encoder for
binary chat parts (page crops, ImageBubble images) handed to `genai` and for image-edit
request bodies, and the strict decoder for base64 images in image-edit responses.

Key functions:
- base64_encode()
- base64_decode()  : strict (padding required, no whitespace, canonical trailing bits)

Key structures:
- Base64Error

Notes:
Hand-written on purpose: one owner of both directions, no extra dependency edge.
*/

use std::fmt;

/// RFC 4648 standard alphabet.
const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `data` as standard, padded base64 (RFC 4648 section 4). Empty input gives an
/// empty string.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        // A short final chunk is zero-padded; its missing sextets become `=` below.
        let mut bytes = [0u8; 3];
        bytes[..chunk.len()].copy_from_slice(chunk);
        let [b0, b1, b2] = bytes;
        let sextets = [
            b0 >> 2,
            ((b0 & 0x03) << 4) | (b1 >> 4),
            ((b1 & 0x0f) << 2) | (b2 >> 6),
            b2 & 0x3f,
        ];
        // `n` input bytes carry `n + 1` significant sextets.
        let significant = chunk.len() + 1;
        for (index, sextet) in sextets.into_iter().enumerate() {
            if index < significant {
                out.push(char::from(TABLE[usize::from(sextet)]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Why `base64_decode` rejected its input. `Display` is a technical (non-localized) detail
/// text; callers wrap it in their own localized error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64Error {
    /// The length is not a multiple of 4 (padding is required).
    InvalidLength { len: usize },
    /// A byte outside the standard alphabet (including whitespace and `-` / `_` of the URL-safe
    /// alphabet) at byte offset `index`.
    InvalidByte { index: usize },
    /// `=` padding in a position other than the last one or two bytes of the input, or
    /// non-zero bits under the padding (a non-canonical encoding).
    InvalidPadding,
}

impl fmt::Display for Base64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { len } => write!(f, "base64 length {len} is not a multiple of 4"),
            Self::InvalidByte { index } => write!(f, "invalid base64 byte at offset {index}"),
            Self::InvalidPadding => f.write_str("invalid base64 padding"),
        }
    }
}

impl std::error::Error for Base64Error {}

/// Value of one standard-alphabet byte, `None` for anything else (`=` included).
fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decodes standard, padded base64 (RFC 4648 section 4), the exact inverse of
/// [`base64_encode`]. Strict: the length must be a multiple of 4, padding (at most two `=`)
/// may only end the input, whitespace and the URL-safe alphabet are refused, and the bits
/// under the padding must be zero, so every accepted input is the canonical encoding of its
/// output. Empty input gives an empty `Vec`.
///
/// # Errors
/// `Base64Error::InvalidLength`, `InvalidByte` (with the offending offset) or
/// `InvalidPadding`, as documented on the variants.
pub fn base64_decode(text: &str) -> Result<Vec<u8>, Base64Error> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(Base64Error::InvalidLength { len: bytes.len() });
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let quads = bytes.len() / 4;
    for (quad_index, quad) in bytes.chunks_exact(4).enumerate() {
        let last = quad_index + 1 == quads;
        // Padding is legal only in the final quad, as `xx==` or `xxx=`.
        let pad = quad.iter().rev().take_while(|&&byte| byte == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return Err(Base64Error::InvalidPadding);
        }
        let significant = 4 - pad;
        let mut values = [0u8; 4];
        for (offset, (&byte, value)) in quad.iter().zip(values.iter_mut()).take(significant).enumerate() {
            *value = sextet(byte).ok_or(Base64Error::InvalidByte { index: quad_index * 4 + offset })?;
        }
        let [v0, v1, v2, v3] = values;
        let decoded = [(v0 << 2) | (v1 >> 4), (v1 << 4) | (v2 >> 2), (v2 << 6) | v3];
        // `n` significant sextets carry `n - 1` whole bytes; the leftover low bits of the last
        // significant sextet must be zero in a canonical encoding.
        let whole = significant - 1;
        let leftover_bits_set = match pad {
            1 => v2 & 0x03 != 0,
            2 => v1 & 0x0f != 0,
            _ => false,
        };
        if leftover_bits_set {
            return Err(Base64Error::InvalidPadding);
        }
        out.extend_from_slice(&decoded[..whole]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{Base64Error, base64_decode, base64_encode};

    #[test]
    fn characterize_base64_encode_rfc4648() {
        let vectors = [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")];
        for (input, expected) in vectors {
            assert_eq!(base64_encode(input.as_bytes()), expected);
        }
    }

    #[test]
    fn base64_encode_covers_every_byte_value() {
        let data: Vec<u8> = (0..=255u8).collect();
        let encoded = base64_encode(&data);
        // Reference value: Python's `base64.b64encode(bytes(range(256)))`.
        assert_eq!(encoded, "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq+wsbKztLW2t7i5uru8vb6/wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj5OXm5+jp6uvs7e7v8PHy8/T19vf4+fr7/P3+/w==");
    }

    #[test]
    fn base64_decode_inverts_the_rfc4648_vectors() {
        let vectors = [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")];
        for (plain, encoded) in vectors {
            assert_eq!(base64_decode(encoded), Ok(plain.as_bytes().to_vec()), "{encoded}");
        }
    }

    #[test]
    fn base64_decode_round_trips_every_byte_value_and_length() {
        let data: Vec<u8> = (0..=255u8).collect();
        for len in 0..data.len() {
            let slice = &data[..len];
            assert_eq!(base64_decode(&base64_encode(slice)), Ok(slice.to_vec()), "len {len}");
        }
    }

    #[test]
    fn base64_decode_is_strict() {
        assert_eq!(base64_decode("Zg="), Err(Base64Error::InvalidLength { len: 3 }));
        assert_eq!(base64_decode("Zm9v\nYmFy"), Err(Base64Error::InvalidLength { len: 9 }));
        assert_eq!(base64_decode("Zm 9"), Err(Base64Error::InvalidByte { index: 2 }));
        assert_eq!(base64_decode("Zm9-"), Err(Base64Error::InvalidByte { index: 3 }));
        assert_eq!(base64_decode("Zm_v"), Err(Base64Error::InvalidByte { index: 2 }));
        assert_eq!(base64_decode("Z==="), Err(Base64Error::InvalidPadding));
        assert_eq!(base64_decode("Zg==Zm9v"), Err(Base64Error::InvalidPadding));
        assert_eq!(base64_decode("Z=g="), Err(Base64Error::InvalidByte { index: 1 }));
        // Non-zero bits under the padding: "Zh==" would also decode to "f" non-canonically.
        assert_eq!(base64_decode("Zh=="), Err(Base64Error::InvalidPadding));
        assert_eq!(base64_decode("Zm9="), Err(Base64Error::InvalidPadding));
    }
}
