/*
File: crates/ms-browser-ext/src/derive.rs

Purpose:
Pure derivations shared by the build script (`build.rs` includes this file through `#[path]`)
and the crate's tests: the Chromium extension id of a manifest `key` and the content
`build_id` of a bundle.

Key functions:
- extension_id_from_public_key() — Chromium rule: sha256(DER public key), first 32 hex digits,
  digit `0-f` mapped to letter `a-p`
- build_id_of() — truncated sha256 over the sorted (path, length, bytes) list

Notes:
Changing `build_id_of` changes every `build_id` (harmless: one automatic re-unpack + reload),
changing `extension_id_from_public_key` would break the pinned Chromium origin.
*/

use sha2::{Digest, Sha256};

/// The Chromium extension id for a manifest `key` (its base64-decoded DER public key):
/// sha256 of the DER bytes, first 16 bytes as 32 hex digits, each digit `0..=f` mapped to the
/// letter `a..=p`. Always 32 lowercase letters.
#[must_use]
pub fn extension_id_from_public_key(der: &[u8]) -> String {
    // Hex digit `n` is written as the n-th letter: the Chromium "mpdecimal" alphabet.
    const ALPHABET: &[u8; 16] = b"abcdefghijklmnop";
    let digest = Sha256::digest(der);
    digest[..16]
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        // A nibble is at most 15, so the index always lies inside the 16-letter alphabet.
        .map(|nibble| char::from(ALPHABET[usize::from(nibble)]))
        .collect()
}

/// Content identity of a bundle: sha256 over the files sorted by path, each fed as
/// `path bytes, 0x00, length as u64 little-endian, content`; the first 16 bytes as 32 hex
/// digits. Independent of the order `files` is given in.
#[must_use]
pub fn build_id_of(files: &[(&str, &[u8])]) -> String {
    let mut sorted: Vec<&(&str, &[u8])> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = Sha256::new();
    for (path, bytes) in sorted {
        hasher.update(path.as_bytes());
        hasher.update([0u8]);
        // `usize` is at most 64 bits on every supported target: a widening, never truncating, conversion.
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hex::encode(&hasher.finalize()[..16])
}
