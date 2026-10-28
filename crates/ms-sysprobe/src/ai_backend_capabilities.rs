/*
FILE OVERVIEW: crates/ms-sysprobe/src/ai_backend_capabilities.rs
Process-wide AI runtime capability slots, mirrored from honest sources so the UI
can gate tool buttons without touching the shared health snapshot lock.

Four independent capabilities and one process fact are exposed:
- Backend: whether the Python AI backend is reachable overall (from the health
  snapshot `connected` flag).
- Torch: whether PyTorch is available in the backend (from the health snapshot
  `is_torch_available` flag).
- onnxruntime (ORT): whether onnxruntime is reachable via EITHER the native
  in-process dylib OR the Python backend. The two writers use two independent
  slots (`ORT_NATIVE_STATUS`, `ORT_BACKEND_STATUS`) so they never clobber each
  other; `ort_available()` combines them (available if either is known-available).
- sd.cpp route: whether this process may use the Stable-Diffusion.cpp runtime at all
  (`--no-ai` + platform, published once by the AI backend supervisor from
  `AiRouteKind::SdCpp.availability`). It is ONLY the route answer: whether sd.cpp is
  installed and ready is `ms_sdcpp::status::ready()`, which consumers combine with it
  (`ms_widgets::AiCaps`). This crate cannot read that status itself (same layer).
- Backend starting (a plain flag, not a capability): whether the supervisor has a backend start
  underway — an armed autostart (gate pending included) or a launched process that has not
  connected yet. Written only by the supervisor's process worker, so "the backend is down"
  can be told apart from "the backend is still booting".

Each capability is a tri-state (`Unknown` / `Available` / `Unavailable`) stored in
an `AtomicU8` with `Ordering::Relaxed`, encoded/decoded via the shared
[`encode_state`] / [`decode_state`] helpers.

Main functions:
- `set_backend_available` / `backend_available`
- `set_torch_available` / `torch_available`
- `set_native_ort_available` / `set_backend_ort_available` / `ort_available`
- `set_sdcpp_route_allowed` / `sdcpp_route_allowed`
- `set_backend_starting` / `backend_starting`
- `combine_ort`: pure combine rule for the two ORT slots (unit-tested).
*/

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const STATUS_UNKNOWN: u8 = 0;
const STATUS_AVAILABLE: u8 = 1;
const STATUS_UNAVAILABLE: u8 = 2;

/// Encodes an optional availability into the tri-state atomic representation
/// (`None` => Unknown, `Some(true)` => Available, `Some(false)` => Unavailable).
#[must_use]
fn encode_state(value: Option<bool>) -> u8 {
    match value {
        Some(true) => STATUS_AVAILABLE,
        Some(false) => STATUS_UNAVAILABLE,
        None => STATUS_UNKNOWN,
    }
}

/// Decodes a tri-state atomic value back into an optional availability. Any
/// unrecognized encoding (should not occur) decodes to `None` (Unknown).
#[must_use]
fn decode_state(raw: u8) -> Option<bool> {
    match raw {
        STATUS_AVAILABLE => Some(true),
        STATUS_UNAVAILABLE => Some(false),
        _ => None,
    }
}

static BACKEND_STATUS: AtomicU8 = AtomicU8::new(STATUS_UNKNOWN);
static TORCH_STATUS: AtomicU8 = AtomicU8::new(STATUS_UNKNOWN);
static ORT_NATIVE_STATUS: AtomicU8 = AtomicU8::new(STATUS_UNKNOWN);
static ORT_BACKEND_STATUS: AtomicU8 = AtomicU8::new(STATUS_UNKNOWN);
static SDCPP_ROUTE_STATUS: AtomicU8 = AtomicU8::new(STATUS_UNKNOWN);
static BACKEND_STARTING: AtomicBool = AtomicBool::new(false);

/// Records whether the Python AI backend is reachable overall. `None` clears the
/// slot back to Unknown (e.g. before the first health snapshot).
pub fn set_backend_available(value: Option<bool>) {
    BACKEND_STATUS.store(encode_state(value), Ordering::Relaxed);
}

/// Returns the last known Python AI backend reachability, or `None` if unknown.
#[must_use]
pub fn backend_available() -> Option<bool> {
    decode_state(BACKEND_STATUS.load(Ordering::Relaxed))
}

/// Records whether PyTorch is available in the backend. `None` clears the slot
/// back to Unknown (e.g. before the first health snapshot or on a lost backend).
pub fn set_torch_available(value: Option<bool>) {
    TORCH_STATUS.store(encode_state(value), Ordering::Relaxed);
}

/// Returns the last known PyTorch availability, or `None` if unknown.
#[must_use]
pub fn torch_available() -> Option<bool> {
    decode_state(TORCH_STATUS.load(Ordering::Relaxed))
}

/// Records the NATIVE onnxruntime capability (the in-process dylib). Native ORT is
/// "armed" — available even before its dylib loads — when the effective runtime is
/// Native and the SIGILL load guard is Safe; the translation tab publishes this
/// from its route-inputs refresh. Independent of the backend slot.
pub fn set_native_ort_available(value: Option<bool>) {
    ORT_NATIVE_STATUS.store(encode_state(value), Ordering::Relaxed);
}

/// Records the BACKEND onnxruntime capability (onnxruntime inside the Python
/// backend). Written from backend health/device signals; independent of the
/// native slot.
pub fn set_backend_ort_available(value: Option<bool>) {
    ORT_BACKEND_STATUS.store(encode_state(value), Ordering::Relaxed);
}

/// Returns the combined onnxruntime availability across the native and backend
/// runtimes, or `None` while both are unknown. See [`combine_ort`].
#[must_use]
pub fn ort_available() -> Option<bool> {
    let native = decode_state(ORT_NATIVE_STATUS.load(Ordering::Relaxed));
    let backend = decode_state(ORT_BACKEND_STATUS.load(Ordering::Relaxed));
    combine_ort(native, backend)
}

/// Records whether this process may use the sd.cpp runtime (`--no-ai` and the platform,
/// decided by `ms_local_ai::local_ai::AiRouteKind::SdCpp.availability`). `None` clears the
/// slot back to Unknown. Says nothing about installation or readiness.
pub fn set_sdcpp_route_allowed(value: Option<bool>) {
    SDCPP_ROUTE_STATUS.store(encode_state(value), Ordering::Relaxed);
}

/// Returns whether this process may use the sd.cpp runtime, or `None` before the supervisor
/// published it.
#[must_use]
pub fn sdcpp_route_allowed() -> Option<bool> {
    decode_state(SDCPP_ROUTE_STATUS.load(Ordering::Relaxed))
}

/// Records whether a backend start is underway (the AI backend supervisor's process worker
/// is the only writer): `true` from an armed autostart (its gate still pending included) or
/// a launch until the first connect after it, the launch failing, the process exiting, or
/// the start being cancelled / stopped.
pub fn set_backend_starting(value: bool) {
    BACKEND_STARTING.store(value, Ordering::Relaxed);
}

/// Whether a backend start is underway (see [`set_backend_starting`]); `false` before the
/// supervisor published anything.
#[must_use]
pub fn backend_starting() -> bool {
    BACKEND_STARTING.load(Ordering::Relaxed)
}

/// Combines the two independent onnxruntime capability slots into one answer.
///
/// onnxruntime is considered available if EITHER runtime is known-available.
/// The result is `None` (Unknown) only while BOTH slots are unknown; once either
/// slot carries a known result, a definite `Some(_)` is produced.
#[must_use]
fn combine_ort(native: Option<bool>, backend: Option<bool>) -> Option<bool> {
    // Available if EITHER runtime is known-available.
    // Unknown only while BOTH are unknown; otherwise a known result exists.
    match (native, backend) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (None, None) => None,
        _ => Some(false),
    }
}

#[cfg(test)]
mod tests {
    use super::combine_ort;

    /// Truth table for [`combine_ort`] across all 9 (native × backend)
    /// combinations of {None, Some(true), Some(false)}.
    #[test]
    fn combine_ort_truth_table() {
        // Either known-available => Some(true).
        assert_eq!(combine_ort(Some(true), None), Some(true));
        assert_eq!(combine_ort(Some(true), Some(true)), Some(true));
        assert_eq!(combine_ort(Some(true), Some(false)), Some(true));
        assert_eq!(combine_ort(None, Some(true)), Some(true));
        assert_eq!(combine_ort(Some(false), Some(true)), Some(true));

        // Both unknown => None.
        assert_eq!(combine_ort(None, None), None);

        // One known-unavailable, the other unknown or unavailable => Some(false).
        assert_eq!(combine_ort(Some(false), None), Some(false));
        assert_eq!(combine_ort(None, Some(false)), Some(false));
        assert_eq!(combine_ort(Some(false), Some(false)), Some(false));
    }
}
