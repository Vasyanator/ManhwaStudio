/*
File: crates/ms-project/src/pixel_digest.rs

Purpose:
The ONE definition of "these are the same pixels" for the image-set (single-image) mode:
`PixelDigest`, a SHA-256 over the dimensions and the straight RGBA8 bytes of a page. The open
path (`single_image::open_image_set`) records it for every page exactly as written into the
scratch chapter; the typing flatten computes it over the composed page, and Save skips a page
whose digest still equals the last one written (so an untouched JPEG is never re-encoded).

Key items:
- `PixelDigest`, `PixelDigest::of_rgba8()`.

Notes:
Compiled on every target (the typing tab, which also builds for wasm, computes it). Pure, no
I/O; the cost is one SHA-256 pass over the buffer (roughly 0.2 s per 100 MB), so call it on a
worker for full pages, never on the GUI thread.
*/

use sha2::{Digest, Sha256};

/// Digest of one RGBA8 page: SHA-256 over `width` (u32 little-endian) ‖ `height` (u32
/// little-endian) ‖ the pixel bytes. Two digests are equal exactly when (up to SHA-256
/// collisions) the dimensions and every byte are equal; the dimensions are hashed so that the
/// same bytes reshaped into another size never compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PixelDigest([u8; 32]);

impl PixelDigest {
    /// Digest of a straight (non-premultiplied) RGBA8 buffer of `width` x `height` pixels, rows
    /// top to bottom without padding. The bytes are hashed as given, whatever their length, so
    /// a buffer whose length does not match the dimensions simply yields a digest no correctly
    /// shaped page has; nothing panics.
    #[must_use]
    pub fn of_rgba8(width: u32, height: u32, rgba: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(width.to_le_bytes());
        hasher.update(height.to_le_bytes());
        hasher.update(rgba);
        Self(hasher.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::PixelDigest;

    #[test]
    fn equal_pixels_give_equal_digests_and_any_change_differs() {
        let pixels = vec![10_u8, 20, 30, 255, 40, 50, 60, 128];
        let digest = PixelDigest::of_rgba8(2, 1, &pixels);
        assert_eq!(digest, PixelDigest::of_rgba8(2, 1, &pixels.clone()));

        let mut changed = pixels.clone();
        changed[7] = 127;
        assert_ne!(digest, PixelDigest::of_rgba8(2, 1, &changed));
    }

    #[test]
    fn dimensions_are_part_of_the_digest() {
        let pixels = vec![1_u8; 16];
        assert_ne!(PixelDigest::of_rgba8(2, 2, &pixels), PixelDigest::of_rgba8(4, 1, &pixels));
        assert_ne!(PixelDigest::of_rgba8(1, 4, &pixels), PixelDigest::of_rgba8(4, 1, &pixels));
    }

    #[test]
    fn digest_is_sha256_of_the_documented_layout() {
        // Pins the byte layout (`w_le ‖ h_le ‖ bytes`): a digest stored by one component and
        // recomputed by another must never disagree because the layout drifted.
        use sha2::{Digest, Sha256};
        let pixels = [9_u8, 8, 7, 6];
        let mut expected = Vec::new();
        expected.extend_from_slice(&1_u32.to_le_bytes());
        expected.extend_from_slice(&1_u32.to_le_bytes());
        expected.extend_from_slice(&pixels);
        let want: [u8; 32] = Sha256::digest(&expected).into();
        assert_eq!(PixelDigest::of_rgba8(1, 1, &pixels), PixelDigest(want));
    }
}
