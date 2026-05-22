/*
FILE OVERVIEW: src/ai_backend_capabilities.rs
Global runtime capabilities mirrored from Python AI backend health snapshot.

Main responsibilities:
- keep the latest known PyTorch availability in a process-wide atomic slot;
- expose a cheap read path for tabs/controllers that need Torch capability gates
  outside of the shared health snapshot lock.
*/

use std::sync::atomic::{AtomicU8, Ordering};

const TORCH_STATUS_UNKNOWN: u8 = 0;
const TORCH_STATUS_AVAILABLE: u8 = 1;
const TORCH_STATUS_UNAVAILABLE: u8 = 2;

static TORCH_STATUS: AtomicU8 = AtomicU8::new(TORCH_STATUS_UNKNOWN);

pub fn set_torch_available(value: Option<bool>) {
    let encoded = match value {
        Some(true) => TORCH_STATUS_AVAILABLE,
        Some(false) => TORCH_STATUS_UNAVAILABLE,
        None => TORCH_STATUS_UNKNOWN,
    };
    TORCH_STATUS.store(encoded, Ordering::Relaxed);
}

#[must_use]
pub fn torch_available() -> Option<bool> {
    match TORCH_STATUS.load(Ordering::Relaxed) {
        TORCH_STATUS_AVAILABLE => Some(true),
        TORCH_STATUS_UNAVAILABLE => Some(false),
        _ => None,
    }
}
