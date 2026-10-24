/*
File: crates/ms-browser-bridge/src/api.rs

Purpose:
Plain data types of the bridge API, shared by the server, the process-wide service and
consumers: configuration, status snapshot, peer description, request and its options, events.
No I/O and no threads here.

Key items:
- `BridgeConfig` + `BridgeTimings` — what `ensure_started` / `BridgeServer::start` take.
- `BridgeStatus` (`ListenState` + `PeerInfo`s) — the GUI-safe snapshot.
- `BridgeRequest`, `RequestOptions`, `BridgeEvent`, `ReloadTarget`.
- `select_peer` — the routing rule (oldest ready peer announcing the exact module version,
  preferring one that runs the app's bundle).
*/

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::Value;

use crate::errors::BridgeError;
use crate::protocol::{BrowserInfo, EventKind, ModuleInfo, PeerState};

/// Timings of the server; `Default` holds the production values. Tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeTimings {
    /// Upgrade + `challenge` / `hello` must complete within this (production 5 s).
    pub handshake: Duration,
    /// A peer that sent nothing for this long is dropped (production 60 s; the extension
    /// pings every 20 s).
    pub silence: Duration,
    /// Socket read timeout of a connection thread and the listener's accept poll: bounds how
    /// late an outbound message or a shutdown is noticed (production 100 ms).
    pub poll: Duration,
    /// A peer that does not take a frame within this long is dropped (production 5 s).
    pub write: Duration,
}

impl Default for BridgeTimings {
    fn default() -> Self {
        Self { handshake: Duration::from_secs(5), silence: Duration::from_mins(1), poll: Duration::from_millis(100), write: Duration::from_secs(5) }
    }
}

/// What the server needs to start. Built in ONE place for the app (`ms_browser_bridge::app`),
/// from `ms-browser-ext` facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeConfig {
    /// The unpacked extension folder; `bridge.json` is written into it (created if missing).
    pub install_dir: PathBuf,
    /// The only accepted `Origin` of the upgrade, e.g. `chrome-extension://<id>`.
    pub allowed_origin: String,
    /// `build_id` of the extension bundle the app embeds; `None` when the build has none.
    /// Informational: a differing peer stays `ready` and is only reported as needing a reload.
    pub expected_build_id: Option<String>,
    /// The app's display version, sent in `welcome`.
    pub app_version: String,
    /// Server timings.
    pub timings: BridgeTimings,
}

/// One authenticated extension connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// Connection sequence number; smaller = connected earlier.
    pub id: u64,
    /// The browser it runs in.
    pub browser: BrowserInfo,
    /// The extension's `manifest.json` version.
    pub ext_version: String,
    /// The extension's bundle `build_id`, if it reported one.
    pub build_id: Option<String>,
    /// The extension's `PROTOCOL_VERSION`.
    pub protocol: u32,
    /// Announced modules (empty for an outdated peer: its shapes are unknown).
    pub modules: Vec<ModuleInfo>,
    /// `Ready` (same protocol) or `Outdated`.
    pub state: PeerState,
    /// `build_id` equals the app's embedded one (always true when the app embeds none).
    pub build_current: bool,
}

impl PeerInfo {
    /// True when the peer is ready and announces `module` at exactly `version`.
    #[must_use]
    pub fn supports(&self, module: &str, version: u32) -> bool {
        self.state == PeerState::Ready && self.modules.iter().any(|m| m.name == module && m.version == version)
    }

    /// True when the peer should reload: another protocol, or a bundle other than the app's.
    #[must_use]
    pub fn needs_reload(&self) -> bool {
        self.state == PeerState::Outdated || !self.build_current
    }
}

/// Whether the loopback listener runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenState {
    /// `ensure_started` was never called.
    NotStarted,
    /// The start worker is binding / writing `bridge.json`.
    Starting,
    /// Accepting extension connections on `127.0.0.1:port`.
    Listening {
        /// The bound port (also in `bridge.json`).
        port: u16,
    },
    /// The start failed; the next `ensure_started` retries.
    Failed {
        /// Why.
        error: BridgeError,
    },
}

/// A snapshot of the bridge: listener state and connected peers (oldest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeStatus {
    /// Listener state.
    pub listen: ListenState,
    /// Authenticated peers, in connection order.
    pub peers: Vec<PeerInfo>,
}

impl BridgeStatus {
    /// The peer a request for `module` / `version` would go to.
    #[must_use]
    pub fn serving_peer(&self, module: &str, version: u32) -> Option<&PeerInfo> {
        self.peers.iter().find(|peer| peer.supports(module, version))
    }

    /// True when at least one `Ready` peer is connected.
    #[must_use]
    pub fn has_ready_peer(&self) -> bool {
        self.peers.iter().any(|peer| peer.state == PeerState::Ready)
    }
}

/// A module call. `params` is module-defined JSON.
#[derive(Debug, Clone, Copy)]
pub struct BridgeRequest<'a> {
    /// Module name, e.g. `"translate"`.
    pub module: &'a str,
    /// The module version the caller speaks; only a peer announcing exactly it is used.
    pub module_version: u32,
    /// Method name.
    pub method: &'a str,
    /// Params.
    pub params: &'a Value,
}

/// A notification the extension sent while the caller's request ran.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeEvent {
    /// The request it belongs to.
    pub request_id: u64,
    /// What happened.
    pub kind: EventKind,
    /// Module-defined details.
    pub data: Value,
}

/// How a blocking request waits.
pub struct RequestOptions<'a> {
    /// Overall deadline, excluding time spent in an attention state. Also sent to the
    /// extension as `timeout_ms`.
    pub timeout: Duration,
    /// Longest single attention state (captcha shown to the user) before the request fails
    /// `AttentionTimeout`.
    pub attention_timeout: Duration,
    /// Set by the caller to abort; noticed within 250 ms.
    pub cancel: Option<&'a AtomicBool>,
    /// Called on the requesting thread, with no bridge lock held, for every event.
    pub on_event: Option<&'a dyn Fn(&BridgeEvent)>,
}

impl fmt::Debug for RequestOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestOptions")
            .field("timeout", &self.timeout)
            .field("attention_timeout", &self.attention_timeout)
            .field("cancel", &self.cancel)
            .field("on_event", &self.on_event.is_some())
            .finish()
    }
}

/// Which peers `request_reload` addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadTarget {
    /// Every connected peer.
    All,
    /// Peers for which `PeerInfo::needs_reload` holds.
    NeedsUpdate,
}

impl ReloadTarget {
    /// True when `peer` is addressed.
    #[must_use]
    pub fn includes(self, peer: &PeerInfo) -> bool {
        match self {
            Self::All => true,
            Self::NeedsUpdate => peer.needs_reload(),
        }
    }
}

/// The routing rule: index of the OLDEST peer that is ready and announces `module` at exactly
/// `version` (`peers` is in connection order), preferring a peer whose `build_current` holds.
///
/// Why the preference: a stale-build peer is what the app's reload watcher reloads (dropping its
/// in-flight request), so a matching current peer is used whenever one exists. When only stale
/// peers match, the oldest of them still serves the request (it may be reloaded under it and then
/// fails `Disconnected`; a stale peer loaded from another folder is never reloaded for long).
///
/// # Errors
/// `NotConnected` (no peer), `Outdated` (only protocol-mismatched peers), `CapabilityMissing`
/// (ready peers, none with that module version).
pub fn select_peer(peers: &[PeerInfo], module: &str, version: u32) -> Result<usize, BridgeError> {
    if peers.is_empty() {
        return Err(BridgeError::NotConnected);
    }
    if !peers.iter().any(|peer| peer.state == PeerState::Ready) {
        return Err(BridgeError::Outdated);
    }
    peers
        .iter()
        .position(|peer| peer.supports(module, version) && peer.build_current)
        .or_else(|| peers.iter().position(|peer| peer.supports(module, version)))
        .ok_or_else(|| BridgeError::CapabilityMissing { module: module.to_owned(), version })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: u64, state: PeerState, modules: &[(&str, u32)]) -> PeerInfo {
        PeerInfo {
            id,
            browser: BrowserInfo::default(),
            ext_version: "1.0.0".into(),
            build_id: None,
            protocol: 1,
            modules: modules.iter().map(|(name, version)| ModuleInfo { name: (*name).to_owned(), version: *version, sites: Vec::new() }).collect(),
            state,
            build_current: true,
        }
    }

    #[test]
    fn routing_picks_oldest_ready_peer_with_exact_module_version() {
        let peers = vec![
            peer(1, PeerState::Outdated, &[]),
            peer(2, PeerState::Ready, &[("translate", 2)]),
            peer(3, PeerState::Ready, &[("translate", 1)]),
            peer(4, PeerState::Ready, &[("translate", 1)]),
        ];
        assert_eq!(select_peer(&peers, "translate", 1), Ok(2));
        assert_eq!(select_peer(&peers, "translate", 2), Ok(1));
        assert_eq!(select_peer(&peers, "download", 1), Err(BridgeError::CapabilityMissing { module: "download".into(), version: 1 }));
    }

    #[test]
    fn routing_prefers_a_current_build_and_falls_back_to_a_stale_one() {
        let mut stale = peer(1, PeerState::Ready, &[("translate", 1)]);
        stale.build_current = false;
        let current = peer(2, PeerState::Ready, &[("translate", 1)]);
        assert_eq!(select_peer(&[stale.clone(), current], "translate", 1), Ok(1), "the stale peer is about to be reloaded");
        assert_eq!(select_peer(std::slice::from_ref(&stale), "translate", 1), Ok(0), "a lone stale peer still serves");
        let current_without_module = peer(3, PeerState::Ready, &[]);
        assert_eq!(select_peer(&[stale, current_without_module], "translate", 1), Ok(0));
    }

    #[test]
    fn routing_errors_distinguish_no_peer_and_outdated_only() {
        assert_eq!(select_peer(&[], "translate", 1), Err(BridgeError::NotConnected));
        assert_eq!(select_peer(&[peer(1, PeerState::Outdated, &[])], "translate", 1), Err(BridgeError::Outdated));
    }

    #[test]
    fn reload_target_selects_outdated_and_foreign_builds() {
        let current = peer(1, PeerState::Ready, &[]);
        let mut foreign = peer(2, PeerState::Ready, &[]);
        foreign.build_current = false;
        let outdated = peer(3, PeerState::Outdated, &[]);
        assert!(!ReloadTarget::NeedsUpdate.includes(&current));
        assert!(ReloadTarget::NeedsUpdate.includes(&foreign));
        assert!(ReloadTarget::NeedsUpdate.includes(&outdated));
        assert!(ReloadTarget::All.includes(&current));
    }
}
