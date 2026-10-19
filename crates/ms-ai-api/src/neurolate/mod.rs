/*
File: crates/ms-ai-api/src/neurolate/mod.rs

Purpose:
The NeuroLate cloud OCR / text-detection client (`https://api.neurolate.com`): where its API
key lives, the typed request / response model, the native client (`recognize`, the paid OCR
call; `fetch_key_info`, the free key / limit call) and the key + limit panel (GUI-free state
and its egui view). Consumers (the translation tab's OCR engine and detector mode) build an
`OcrRequest`, call `recognize` on a worker and convert the typed answer themselves.

Modules:
- `protocol`: the typed model, the multipart form, the parsers, status -> error (pure).
- `error`   : `NeurolateError`, `Display` = localized user message.
- `client`  : `recognize`, `fetch_key_info` (native; wasm stubs answer `WebUnavailable`).
- `account` : `NeurolateKeyPanel`, `NeurolateKeyActions` (GUI-free, single-flight workers).
- `view`    : `draw_neurolate_key_panel`.

Key functions:
- key_slot(): the credential-store slot of the key (`"neurolate:api"`, frozen).
- read_key(): the stored key, trimmed; `KeyMissing` when none.

Notes:
The key lives in the crate's credential store under the named slot `NamedKeyUser::neurolate()`
and is driven through the image-edit key machinery (`ImageEditKeySlot`, `ImageEditKeyState`,
`ImageEditKeyRunner`) unchanged. A paid request is never retried; see `client.rs` for the
abandon contract. Blocking calls (`read_key`, `recognize`, `fetch_key_info`) never run on the
GUI thread.
*/

pub mod account;
mod client;
pub mod error;
pub mod protocol;
pub mod view;

pub use account::{NeurolateKeyActions, NeurolateKeyPanel};
pub use client::{fetch_key_info, recognize};
pub use error::NeurolateError;
pub use protocol::{BASE_URL, JobError, KeyInfo, KeyStatus, LimitPeriod, NeurolateComicType, NeurolateLanguage, OcrBlock, OcrBox, OcrImageResult, OcrKind, OcrMode, OcrParams, OcrPolygon, OcrRequest, OcrResponse, TextStatus};
pub use view::draw_neurolate_key_panel;

use crate::image_edit::ImageEditKeySlot;
use crate::keys::{NamedKeyUser, read_named_key};

/// Display name of the service (a brand: not localized), used in key notices and logs.
pub const LABEL: &str = "NeuroLate";

/// The credential-store slot of the `NeuroLate` key, for the image-edit key machinery.
#[must_use]
pub fn key_slot() -> ImageEditKeySlot {
    ImageEditKeySlot::Named { user: NamedKeyUser::neurolate(), label: LABEL }
}

/// Reads the stored `NeuroLate` key, trimmed. Blocking credential-store I/O (worker threads
/// only); the secret must never be logged or persisted.
///
/// # Errors
/// `KeyMissing` when no key (or a blank one) is stored; `Key` when the credential store fails.
pub fn read_key() -> Result<String, NeurolateError> {
    let key = read_named_key(&NamedKeyUser::neurolate(), LABEL)?;
    let key = key.trim();
    if key.is_empty() { Err(NeurolateError::KeyMissing) } else { Ok(key.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::key_slot;
    use crate::image_edit::ImageEditKeySlot;

    // Persistence contract: the slot users' NeuroLate keys are saved under.
    #[test]
    fn the_key_slot_is_frozen() {
        match key_slot() {
            ImageEditKeySlot::Named { user, label } => assert_eq!((user.as_str(), label), ("neurolate:api", "NeuroLate")),
            ImageEditKeySlot::Chat(target) => panic!("not a named slot: {target:?}"),
        }
    }
}
