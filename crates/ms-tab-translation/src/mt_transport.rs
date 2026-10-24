/*
File: crates/ms-tab-translation/src/mt_transport.rs

Purpose:
The live, process-wide per-service machine-translation transport (`MtTransport`: the program's
own HTTP requests or the user's browser extension). Seeded once at startup from `user_config`
(`ms_config::mt_transport` owns the persisted value and its writer); the MT panel changes it live,
and every MT entry (a Translation run, the Cleaning tab's prompt translator) reads it.

Key functions:
- current(service) / set(service, transport) / seed_from_user_settings(&Value)
- start_browser_bridge_if_used(): the startup's eager bridge start (native only).

Notes:
- One relaxed `AtomicU8` per service (`MtService` order), a documented exception to global §5 in
  the same way as `ms_config::touch_gestures`: an independent flag per service, read once per run
  or call, never part of a multi-value invariant. Nothing here persists (the panel calls
  `ms_config::mt_transport::save_mt_transport` on a worker).
- GUI-safe: no I/O except the warning log of the reader; `start_browser_bridge_if_used` only
  spawns the bridge-start worker (`machine_translators::browser::start_app_bridge_in_background`).
*/

use std::sync::atomic::{AtomicU8, Ordering};

use ms_config::mt_transport::{MtTransport, transport_from_user_settings};
use serde_json::Value;

use crate::machine_translation::MtService;

/// Stored form of `MtTransport::Builtin` (also the start value: every build before the setting
/// existed used the built-in requests).
const BUILTIN: u8 = 0;
/// Stored form of `MtTransport::Browser`.
const BROWSER: u8 = 1;

/// The live transport per service, indexed by [`slot`].
static CURRENT: [AtomicU8; 3] = [AtomicU8::new(BUILTIN), AtomicU8::new(BUILTIN), AtomicU8::new(BUILTIN)];

/// Index of `service` in [`CURRENT`].
const fn slot(service: MtService) -> usize {
    match service {
        MtService::Google => 0,
        MtService::Yandex => 1,
        MtService::Deepl => 2,
    }
}

const fn encode(transport: MtTransport) -> u8 {
    match transport {
        MtTransport::Builtin => BUILTIN,
        MtTransport::Browser => BROWSER,
    }
}

/// The live transport of `service` (`Builtin` until seeded or set).
#[must_use]
pub fn current(service: MtService) -> MtTransport {
    // Only `encode` writes the slots, so any other value is impossible; reading it as the
    // default keeps the reader total.
    if CURRENT[slot(service)].load(Ordering::Relaxed) == BROWSER { MtTransport::Browser } else { MtTransport::Builtin }
}

/// Replaces the live transport of `service`. Not persisted here.
pub fn set(service: MtService, transport: MtTransport) {
    CURRENT[slot(service)].store(encode(transport), Ordering::Relaxed);
}

/// Starts the browser bridge on a worker when any service's live transport is `Browser`, so
/// the extension has connected by the time the first request goes out (a request that has to
/// start the bridge itself waits for the extension, see `machine_translators::browser`).
/// Called once at startup after [`seed_from_user_settings`] and the app-version registration.
/// GUI-safe: only spawns the worker.
#[cfg(not(target_arch = "wasm32"))]
pub fn start_browser_bridge_if_used() {
    if MtService::all().iter().any(|&service| current(service) == MtTransport::Browser) {
        crate::machine_translators::browser::start_app_bridge_in_background("startup: a service uses the browser transport");
    }
}

/// Seeds every service's live transport from a loaded `user_config` root
/// (`ms_config::mt_transport::transport_from_user_settings`: absent or unknown = `Builtin`).
pub fn seed_from_user_settings(user_settings: &Value) {
    for &service in MtService::all() {
        set(service, transport_from_user_settings(user_settings, service.key()));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn seeding_reads_every_service_and_set_overrides_one() {
        // The statics are process-wide: this is the only test that writes them, and it restores
        // the start state at the end.
        let root = json!({"TranslarionTab": {"MachineTranslation": {"transport": {"deepl": "browser", "google": "builtin", "yandex": "bogus"}}}});
        seed_from_user_settings(&root);
        assert_eq!(current(MtService::Deepl), MtTransport::Browser);
        assert_eq!(current(MtService::Google), MtTransport::Builtin);
        assert_eq!(current(MtService::Yandex), MtTransport::Builtin);
        set(MtService::Yandex, MtTransport::Browser);
        assert_eq!(current(MtService::Yandex), MtTransport::Browser);
        assert_eq!(current(MtService::Deepl), MtTransport::Browser);
        seed_from_user_settings(&json!({}));
        for &service in MtService::all() {
            assert_eq!(current(service), MtTransport::Builtin, "{service:?}");
        }
    }
}
