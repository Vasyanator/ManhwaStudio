/*
File: crates/ms-ai-api/src/encoding.rs

Purpose:
The one standard base64 encoder (RFC 4648 alphabet, `=` padding) used for binary chat parts
(page crops, ImageBubble images) handed to `genai`.

Key functions:
- base64_encode()
*/

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

#[cfg(test)]
mod tests {
    use super::base64_encode;

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
}
