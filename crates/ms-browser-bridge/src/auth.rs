/*
File: crates/ms-browser-bridge/src/auth.rs

Purpose:
The shared secret and the mutual HMAC-SHA256 handshake authentication (FROZEN across protocol
versions, see `protocol.rs`). Mirrored by `browser-extension/src/core/auth.ts` (WebCrypto) and
pinned by the golden `browser-extension/protocol-fixtures/auth_vector.json`.

Key items:
- `Secret` — 32 random bytes; Debug redacted; hex form in `bridge.json`.
- `client_auth` / `server_auth` — lowercase hex tags.
- `verify_client_auth` — constant-time check of the extension's tag.

Construction:
key = the 32 DECODED secret bytes (not the hex text);
client tag = HMAC(key, "ms-bridge-v1|client|" + nonce + "|" + client_nonce);
server tag = HMAC(key, "ms-bridge-v1|server|" + nonce + "|" + client_nonce).
The distinct `client` / `server` labels stop a reflected tag from authenticating the other side.
*/

use std::fmt;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::protocol::AUTH_LABEL;

type HmacSha256 = Hmac<Sha256>;

/// Length of the shared secret in bytes.
pub const SECRET_LEN: usize = 32;

/// Length of a handshake nonce in hex digits (16 random bytes).
pub const NONCE_HEX_LEN: usize = 32;

/// The pairing secret shared through `bridge.json`. Never logged: `Debug` prints a placeholder.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret([u8; SECRET_LEN]);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Secret {
    /// Wraps raw secret bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; SECRET_LEN]) -> Self {
        Self(bytes)
    }

    /// Parses the `bridge.json` form: exactly 64 hex digits (either case). `None` otherwise.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let mut bytes = [0_u8; SECRET_LEN];
        hex::decode_to_slice(text, &mut bytes).ok()?;
        Some(Self(bytes))
    }

    /// The `bridge.json` form: 64 lowercase hex digits.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// A fresh secret from the OS random source.
    ///
    /// # Errors
    /// The OS random source failed (`getrandom`).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; SECRET_LEN];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    fn mac(&self, side: &str, nonce: &str, client_nonce: &str) -> HmacSha256 {
        // `new_from_slice` fails only for key lengths HMAC cannot take; HMAC accepts keys of any
        // length (long ones are hashed), so the 32-byte key always succeeds.
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.0).unwrap_or_else(|_| unreachable!("HMAC accepts a key of any length"));
        mac.update(AUTH_LABEL.as_bytes());
        mac.update(b"|");
        mac.update(side.as_bytes());
        mac.update(b"|");
        mac.update(nonce.as_bytes());
        mac.update(b"|");
        mac.update(client_nonce.as_bytes());
        mac
    }
}

/// A fresh handshake nonce: `NONCE_HEX_LEN` lowercase hex digits from the OS random source.
///
/// # Errors
/// The OS random source failed (`getrandom`).
#[cfg(not(target_arch = "wasm32"))]
pub fn generate_nonce() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; NONCE_HEX_LEN / 2];
    getrandom::fill(&mut bytes)?;
    Ok(hex::encode(bytes))
}

/// True when `text` is a well-formed nonce: exactly `NONCE_HEX_LEN` hex digits.
#[must_use]
pub fn is_valid_nonce(text: &str) -> bool {
    text.len() == NONCE_HEX_LEN && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The extension's tag for `(nonce, client_nonce)`, lowercase hex.
#[must_use]
pub fn client_auth(secret: &Secret, nonce: &str, client_nonce: &str) -> String {
    hex::encode(secret.mac("client", nonce, client_nonce).finalize().into_bytes())
}

/// The app's tag for `(nonce, client_nonce)`, lowercase hex.
#[must_use]
pub fn server_auth(secret: &Secret, nonce: &str, client_nonce: &str) -> String {
    hex::encode(secret.mac("server", nonce, client_nonce).finalize().into_bytes())
}

/// Checks the extension's `auth` tag in constant time (`Mac::verify_slice`). A tag that is not
/// valid hex is simply wrong.
#[must_use]
pub fn verify_client_auth(secret: &Secret, nonce: &str, client_nonce: &str, auth_hex: &str) -> bool {
    let Ok(tag) = hex::decode(auth_hex) else { return false };
    secret.mac("client", nonce, client_nonce).verify_slice(&tag).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "00112233445566778899aabbccddeeff";
    const CLIENT_NONCE: &str = "ffeeddccbbaa99887766554433221100";

    fn fixed_secret() -> Secret {
        let mut bytes = [0_u8; SECRET_LEN];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap();
        }
        Secret::from_bytes(bytes)
    }

    #[test]
    fn known_answer_vector() {
        // Computed independently with:
        // printf 'ms-bridge-v1|client|<NONCE>|<CLIENT_NONCE>' | openssl dgst -sha256 -mac HMAC -macopt hexkey:000102..1f
        assert_eq!(client_auth(&fixed_secret(), NONCE, CLIENT_NONCE), "aad29bfa2ef57cfa5b12df39d7b57e223fd9ef63625bb38d44a6b4063398d18a");
        assert_eq!(server_auth(&fixed_secret(), NONCE, CLIENT_NONCE), "b93cf443a74b87c39f42ca1605b7ccceef94d517eb907d6feeb973ab4188439c");
    }

    #[test]
    fn verify_accepts_own_tag_and_rejects_others() {
        let secret = fixed_secret();
        let tag = client_auth(&secret, NONCE, CLIENT_NONCE);
        assert!(verify_client_auth(&secret, NONCE, CLIENT_NONCE, &tag));
        assert!(verify_client_auth(&secret, NONCE, CLIENT_NONCE, &tag.to_uppercase()));
        // A reflected server tag, a swapped nonce, a wrong secret and garbage all fail.
        assert!(!verify_client_auth(&secret, NONCE, CLIENT_NONCE, &server_auth(&secret, NONCE, CLIENT_NONCE)));
        assert!(!verify_client_auth(&secret, CLIENT_NONCE, NONCE, &tag));
        assert!(!verify_client_auth(&Secret::from_bytes([7; SECRET_LEN]), NONCE, CLIENT_NONCE, &tag));
        assert!(!verify_client_auth(&secret, NONCE, CLIENT_NONCE, "zz"));
        assert!(!verify_client_auth(&secret, NONCE, CLIENT_NONCE, ""));
    }

    #[test]
    fn secret_hex_round_trip_and_redacted_debug() {
        let secret = fixed_secret();
        let text = secret.to_hex();
        assert_eq!(text.len(), 64);
        assert_eq!(Secret::from_hex(&text), Some(secret.clone()));
        assert_eq!(Secret::from_hex(&text[..62]), None);
        assert_eq!(Secret::from_hex(&"g".repeat(64)), None);
        assert!(!format!("{secret:?}").contains(&text[..8]));
    }

    #[test]
    fn nonces_are_fresh_and_well_formed() {
        let a = generate_nonce().unwrap();
        let b = generate_nonce().unwrap();
        assert!(is_valid_nonce(&a) && is_valid_nonce(&b));
        assert_ne!(a, b);
        assert!(!is_valid_nonce("abc"));
        assert!(!is_valid_nonce(&"x".repeat(NONCE_HEX_LEN)));
    }
}
