/*
File: crates/ms-browser-bridge/src/errors.rs

Purpose:
`BridgeError`, the one typed failure of the bridge API, with a localized `Display`
(`browser_bridge.*` keys via `ms-i18n`). Remote failures keep the extension's closed
`ErrorCode` so a consumer can map them to its own semantics (e.g. MT: `rate_limited` ->
cooldown) without parsing text.

Notes:
- `Clone` because the listener's start failure is kept in the status snapshot the GUI reads.
- Texts never contain the pairing secret; `detail` fields carry OS / protocol diagnostics.
*/

use std::path::PathBuf;
use std::time::Duration;

use crate::protocol::ErrorCode;

/// Every way a bridge operation can fail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BridgeError {
    /// The platform cannot host the browser extension (decided by the caller's capability gate).
    #[error("{}", t!("browser_bridge.error.unavailable"))]
    Unavailable,
    /// `ensure_started` was never called (or the server shut down).
    #[error("{}", t!("browser_bridge.error.not_started"))]
    NotStarted,
    /// The server runs (or is starting) but no extension is connected.
    #[error("{}", t!("browser_bridge.error.not_connected"))]
    NotConnected,
    /// Only extensions with another `PROTOCOL_VERSION` are connected.
    #[error("{}", t!("browser_bridge.error.outdated"))]
    Outdated,
    /// Connected extensions are current but none announces `module` at exactly `version`.
    #[error("{}", capability_missing_text(.module, *.version))]
    CapabilityMissing {
        /// Requested module name.
        module: String,
        /// Requested module version.
        version: u32,
    },
    /// The request deadline passed without a response (a `cancel` was sent).
    #[error("{}", t!("browser_bridge.error.timeout"))]
    Timeout,
    /// The extension's attention state (captcha / challenge) lasted longer than the caller's
    /// attention limit (a `cancel` was sent).
    #[error("{}", t!("browser_bridge.error.attention_timeout"))]
    AttentionTimeout,
    /// The caller's cancel flag was set (a `cancel` was sent).
    #[error("{}", t!("browser_bridge.error.cancelled"))]
    Cancelled,
    /// The serving extension disconnected before answering.
    #[error("{}", t!("browser_bridge.error.disconnected"))]
    Disconnected,
    /// The extension sent a message that violates the protocol.
    #[error("{}", protocol_text(.detail))]
    Protocol {
        /// What was wrong.
        detail: String,
    },
    /// The pairing file could not be read or written.
    #[error("{}", pairing_io_text(.path, .detail))]
    PairingIo {
        /// The `bridge.json` path (or its folder).
        path: PathBuf,
        /// OS error text.
        detail: String,
    },
    /// Another running app process owns the bridge of this extension folder (it holds the
    /// folder's inter-process owner lock), so the extension pairs with that process, not this
    /// one. Not retried by itself: the next `ensure_started` tries again (and takes over once the
    /// other process exited).
    #[error("{}", t!("browser_bridge.error.owned_by_other_process"))]
    OwnedByOtherProcess,
    /// No loopback port could be bound.
    #[error("{}", bind_text(.detail))]
    Bind {
        /// OS error text.
        detail: String,
    },
    /// A bridge thread could not be spawned, or the OS random source failed.
    #[error("{}", system_text(.detail))]
    System {
        /// OS error text.
        detail: String,
    },
    /// The extension module answered with an error.
    #[error("{}", remote_text(*.code, .message))]
    Remote {
        /// Closed error code.
        code: ErrorCode,
        /// The extension's diagnostic text.
        message: String,
        /// For `RateLimited`: how long the site asked to wait.
        retry_after: Option<Duration>,
    },
}

fn capability_missing_text(module: &str, version: u32) -> String {
    tf!("browser_bridge.error.capability_missing", module = module, version = version)
}

fn protocol_text(detail: &str) -> String {
    tf!("browser_bridge.error.protocol", detail = detail)
}

fn pairing_io_text(path: &std::path::Path, detail: &str) -> String {
    tf!("browser_bridge.error.pairing_io", path = path.display(), detail = detail)
}

fn bind_text(detail: &str) -> String {
    tf!("browser_bridge.error.bind", detail = detail)
}

fn system_text(detail: &str) -> String {
    tf!("browser_bridge.error.system", detail = detail)
}

fn remote_text(code: ErrorCode, message: &str) -> String {
    let what = remote_code_text(code);
    if message.is_empty() { what.to_owned() } else { tf!("browser_bridge.error.remote", what = what, detail = message) }
}

/// The localized one-line meaning of a remote `ErrorCode`.
#[must_use]
pub fn remote_code_text(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Cancelled => t!("browser_bridge.remote.cancelled"),
        ErrorCode::Timeout => t!("browser_bridge.remote.timeout"),
        ErrorCode::UnknownModule => t!("browser_bridge.remote.unknown_module"),
        ErrorCode::UnknownMethod => t!("browser_bridge.remote.unknown_method"),
        ErrorCode::BadParams => t!("browser_bridge.remote.bad_params"),
        ErrorCode::Busy => t!("browser_bridge.remote.busy"),
        ErrorCode::AttentionTimeout => t!("browser_bridge.remote.attention_timeout"),
        ErrorCode::RateLimited => t!("browser_bridge.remote.rate_limited"),
        ErrorCode::SiteChanged => t!("browser_bridge.remote.site_changed"),
        ErrorCode::TabClosed => t!("browser_bridge.remote.tab_closed"),
        ErrorCode::UnsupportedLanguage => t!("browser_bridge.remote.unsupported_language"),
        ErrorCode::TooLong => t!("browser_bridge.remote.too_long"),
        ErrorCode::Internal => t!("browser_bridge.remote.internal"),
    }
}
