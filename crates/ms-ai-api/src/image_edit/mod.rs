/*
File: crates/ms-ai-api/src/image_edit/mod.rs

Purpose:
Public surface of the cloud image-edit layer: edit a region of a page with a hosted model
(or the user's own OpenAI-compatible server) and get back EXACTLY the region's size, changed
only inside the user's mask.

Modules:
- `provider`  : `ImageEditProvider` catalogue (frozen ids), API shape, endpoint, key slot,
                availability from Russia.
- `size_rule` : `ImageSizeRule` DATA (mapped 1:1 onto the cleaning frame's constraints),
                `SizeEvidence`, the named rules and Gemini size tables.
- `catalog`   : `ModelOffer` per provider (`offers`, `lookup`), mask support, size parameter
                style, retirement dates, deliberate exclusions.
- `request`   : `RgbaRegion`, `ImageEditRequest`, `MaskBlend`, outcome, stages, `CancelFlag`.
- `codec`     : PNG encode of the sent image / native mask (polarity), bounded decode.
- `composite` : the feathered composite inside the mask.
- `pipeline`  : pure `prepare` / `finish` around the HTTP exchange (the owner of size) and
                `run_image_edit`, the whole run (native; wasm stub).
- `protocol`  : `EditProtocol` (pure adapters) and the HTTP request / response descriptions.
- `multipart` : the `multipart/form-data` body builder.
- `error`     : `ImageEditError`, localized `Display`.
- `adapters`  : one pure `EditProtocol` per API shape, `protocol_for`.
- `executor`  : the native HTTP executor (ureq): steps, polls, cancel, deadlines, caps, key
                injection on the provider's own origin only (crate-private, native only).
- `keys`      : `ImageEditKeySlot`, `key_slot` and read / store / clear onto `crate::keys`.
- `key_state` : `ImageEditKeyState` (single-flight key check / store / delete) and
                `ImageEditKeyRunner` (runs it on a worker); `ImageEditKeyStore` seam.
- `view`      : `draw_image_edit_picker` (provider + Russia badge, endpoint, key block, model,
                offer notes) over `ImageEditSelection`.

Notes:
Everything here is target-neutral except `executor` (native only; `run_image_edit` is a
`WebUnavailable` stub on wasm). Only the executor does network I/O; only `keys` and
`key_state` touch the credential store (blocking: worker threads only).
*/

pub mod catalog;
pub mod codec;
pub mod composite;
pub mod error;
pub mod multipart;
pub mod pipeline;
pub mod protocol;
pub mod provider;
pub mod request;
pub mod size_rule;

pub use catalog::{MaskSupport, ModelOffer, SizeParamStyle, all_offers, lookup, offers};
pub use error::ImageEditError;
pub use pipeline::{PreparedCall, finish, prepare};
pub use provider::{ApiShape, EndpointKind, EndpointRegion, ImageEditProvider, ProviderInfo, ProviderKeySlot, RussiaAccess, RussiaNote, RussiaStatus};
pub use request::{CancelFlag, EndpointChoice, ImageEditOutcome, ImageEditRequest, ImageEditStage, MaskBlend, RgbaRegion};
pub use size_rule::{AspectTierEntry, ImageAspectLimit, ImageSizeRule, SizeEvidence};

// The key block's state and the picker widget (provider, endpoint, key, model, notes).
pub mod key_state;
pub mod view;

pub use key_state::{ImageEditKeyError, ImageEditKeyEvent, ImageEditKeyOp, ImageEditKeyOpKind, ImageEditKeyRequest, ImageEditKeyRunner, ImageEditKeyState, ImageEditKeyStore};
pub use view::{ImageEditPickerActions, ImageEditSelection, ProviderChoice, RussiaBadge, active_russia_badge, draw_image_edit_picker, draw_russia_badge, russia_badge};

// The native HTTP executor, the per-shape adapters and the key-slot glue.
pub mod adapters;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod executor;
pub mod keys;

pub use adapters::protocol_for;
pub use keys::{ImageEditKeySlot, clear_key, key_slot, read_key, store_key};
pub use pipeline::run_image_edit;
