/*
File: crates/ms-browser-bridge/src/protocol.rs

Purpose:
The wire protocol between the app and the ManhwaStudio browser extension: constants, the
pairing file shape and every JSON message (serde). The ONE Rust owner of the protocol; its
mirror is `browser-extension/src/core/protocol.ts`, guarded by the mirror tests in
`mirror_tests.rs` (`PROTOCOL_VERSION`, `CONFIG_FILE_NAME`, the error-code set, the golden
fixtures in `browser-extension/protocol-fixtures/`).

Key items:
- `PROTOCOL_VERSION`, `CONFIG_FILE_NAME`, `WS_PATH`, `MAX_TEXT_MESSAGE_BYTES`,
  `MAX_EVENT_MESSAGE_BYTES`, `MAX_WIRE_ID`, `AUTH_LABEL`.
- `BridgeFile` — the pairing file `{protocol, port, secret}`.
- `AppMessage` (app -> extension), `ExtMessage` (extension -> app), their payload structs.
- `ErrorCode` (closed, mirrored), `EventKind`, `PeerState`, `RejectCode`.

Versioning contract:
- `PROTOCOL_VERSION` is the ONLY compatibility gate. Any change of the envelope, of a message
  shape or of a code's meaning bumps it here AND in `protocol.ts`.
- FROZEN FOREVER (never changed, even across protocol versions), so that an app and an extension
  that disagree on `PROTOCOL_VERSION` can still authenticate each other and the app can send
  `reload`: the `type` envelope field, `challenge`, `hello`'s `protocol` / `client_nonce` /
  `auth` / `build_id` fields, `welcome`, `reject`, `reload`, `ping`, `pong`, `bridge.json` and the
  HMAC construction (`auth.rs`).
- Feature modules (`hello.modules[]`) carry their own integer `version`; the app uses a module
  only if it is announced with exactly the version it speaks. `build_id` (the bundle's content
  hash) is informational for compatibility: it only decides whether a `reload` is due.
- Ids are JSON numbers in `1..=MAX_WIRE_ID` (2^53 - 1, the JS `Number.MAX_SAFE_INTEGER`), so
  the extension's `isWireInteger` accepts every id the app sends; the app's counter wraps inside
  that range (`server/mod.rs`), it never emits a larger id.
- App-side limit (not a format change): an `event` text message longer than
  `MAX_EVENT_MESSAGE_BYTES` ends the connection (events are small status notes; the frame cap
  exists for responses).

Reserved for the future download module (NOT implemented in protocol 1; binary frames are
refused with close 1003): extension -> app chunked binary frames
`[u32 big-endian header_len][JSON header {type:"chunk", id, seq, final}][payload bytes]`,
flow-controlled by app -> extension `{"type":"ack","id","seq"}`. The app writes the bytes to
disk itself (`chrome.downloads` can only write into the user's Downloads folder).
*/

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Version of the envelope and the handshake. The only compatibility gate (see file header).
pub const PROTOCOL_VERSION: u32 = 1;

/// Name of the pairing file the app writes into the unpacked extension folder. The extension
/// reads it with `fetch(runtime.getURL(CONFIG_FILE_NAME), {cache: "no-store"})`.
pub const CONFIG_FILE_NAME: &str = "bridge.json";

/// HTTP path of the WebSocket upgrade; any other path is refused with 403.
pub const WS_PATH: &str = "/ms-bridge";

/// Largest accepted text message (and frame), in bytes: 4 MiB.
pub const MAX_TEXT_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Largest accepted `event` text message, in bytes: 64 KiB. Events carry short status data
/// (attention reason, progress step); a larger one is a protocol violation and the app closes
/// the connection (bounds what one peer can queue for a waiting requester).
pub const MAX_EVENT_MESSAGE_BYTES: usize = 64 * 1024;

/// Largest request id the app sends: 2^53 - 1 (`Number.MAX_SAFE_INTEGER`), so a JavaScript
/// peer represents every id exactly. Ids live in `1..=MAX_WIRE_ID`.
pub const MAX_WIRE_ID: u64 = (1 << 53) - 1;

/// Domain label of the handshake HMAC messages (`auth.rs`): `"<label>|client|<nonce>|<client_nonce>"`.
pub const AUTH_LABEL: &str = "ms-bridge-v1";

/// The pairing file `bridge.json`: where the app listens and the shared secret. Written only by
/// the app (atomically, never logged); the secret is 64 lowercase hex digits (32 bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeFile {
    /// `PROTOCOL_VERSION` of the app that wrote the file.
    pub protocol: u32,
    /// Loopback TCP port on `127.0.0.1`.
    pub port: u16,
    /// Hex of the 32-byte shared secret. The HMAC key is the DECODED bytes, not the hex text.
    pub secret: String,
}

/// A peer's standing after a successful (authenticated) handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    /// Same `PROTOCOL_VERSION`: requests may be routed to it.
    Ready,
    /// Different `PROTOCOL_VERSION`: only `ping` / `pong` / `reload` are exchanged; requests to
    /// it fail `BridgeError::Outdated` on the app side.
    Outdated,
}

/// Why the app refused a handshake (sent in `reject`, then the socket closes with 1008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    /// The `hello` HMAC (or its nonce / hex form) is wrong: the peer does not know the secret.
    AuthFailed,
    /// The `hello` lacks even the frozen fields needed to authenticate it.
    ProtocolMismatch,
}

/// Closed set of error codes an extension module answers with (mirrored in `protocol.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The call was aborted (an app `cancel`).
    Cancelled,
    /// The module ran out of its `timeout_ms`.
    Timeout,
    /// No module of that name is registered.
    UnknownModule,
    /// The module has no such method.
    UnknownMethod,
    /// The params do not match the method's schema.
    BadParams,
    /// The module (or the site's tab lease) is busy.
    Busy,
    /// The user did not solve a captcha / challenge in time.
    AttentionTimeout,
    /// The site refused with a rate limit; see `retry_after_ms`.
    RateLimited,
    /// The site's page no longer matches the module's selectors.
    SiteChanged,
    /// The dedicated tab was closed during the call.
    TabClosed,
    /// The site does not support the language pair.
    UnsupportedLanguage,
    /// The input exceeds what the site accepts.
    TooLong,
    /// Any other failure inside the extension.
    Internal,
}

impl ErrorCode {
    /// Every code, in declaration order (the mirror test compares it with `protocol.ts`).
    pub const ALL: [Self; 13] = [
        Self::Cancelled,
        Self::Timeout,
        Self::UnknownModule,
        Self::UnknownMethod,
        Self::BadParams,
        Self::Busy,
        Self::AttentionTimeout,
        Self::RateLimited,
        Self::SiteChanged,
        Self::TabClosed,
        Self::UnsupportedLanguage,
        Self::TooLong,
        Self::Internal,
    ];

    /// The wire token (`snake_case`), identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::UnknownModule => "unknown_module",
            Self::UnknownMethod => "unknown_method",
            Self::BadParams => "bad_params",
            Self::Busy => "busy",
            Self::AttentionTimeout => "attention_timeout",
            Self::RateLimited => "rate_limited",
            Self::SiteChanged => "site_changed",
            Self::TabClosed => "tab_closed",
            Self::UnsupportedLanguage => "unsupported_language",
            Self::TooLong => "too_long",
            Self::Internal => "internal",
        }
    }
}

/// Kind of a progress notification the extension sends while a request runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The site needs the user (captcha / challenge); the extension focused the tab. The app
    /// suspends the request deadline until `AttentionCleared` (bounded by its attention limit).
    Attention,
    /// The user resolved the attention state; the deadline runs again.
    AttentionCleared,
    /// Free-form progress (`data` is module-defined).
    Progress,
}

/// The browser the extension runs in, as it reports itself (display only).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BrowserInfo {
    /// e.g. `"Chrome"`, `"Edg"`, `"YaBrowser"`.
    #[serde(default)]
    pub name: String,
    /// Browser version string.
    #[serde(default)]
    pub version: String,
}

/// One feature module the extension announces in `hello` (its capability).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleInfo {
    /// Module name, e.g. `"translate"`.
    pub name: String,
    /// The module's own protocol version; matched exactly by the app.
    pub version: u32,
    /// Site keys the module can drive (informational).
    #[serde(default)]
    pub sites: Vec<String>,
}

/// app -> extension, first frame after the upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChallengeMsg {
    /// The app's `PROTOCOL_VERSION`.
    pub protocol: u32,
    /// 32 lowercase hex digits (16 random bytes), fresh per connection.
    pub nonce: String,
}

/// extension -> app, the answer to `challenge`. See the file header for the frozen fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloMsg {
    /// The extension's `PROTOCOL_VERSION`.
    pub protocol: u32,
    /// Content hash of the extension bundle (`build-info.json`), if it has one.
    #[serde(default)]
    pub build_id: Option<String>,
    /// `manifest.json` version.
    #[serde(default)]
    pub ext_version: String,
    /// Browser name and version.
    #[serde(default)]
    pub browser: BrowserInfo,
    /// 32 lowercase hex digits chosen by the extension.
    pub client_nonce: String,
    /// `hex(HMAC(secret, "ms-bridge-v1|client|" + nonce + "|" + client_nonce))`.
    pub auth: String,
    /// Announced feature modules.
    #[serde(default)]
    pub modules: Vec<ModuleInfo>,
}

/// app -> extension, the handshake succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WelcomeMsg {
    /// The app's `PROTOCOL_VERSION`.
    pub protocol: u32,
    /// The app's display version (`MS_APP_VERSION`).
    pub app_version: String,
    /// `build_id` of the bundle the app embeds, `null` when it embeds none (informational).
    pub expected_build_id: Option<String>,
    /// `ready` or `outdated` (protocol mismatch).
    pub state: PeerState,
    /// `hex(HMAC(secret, "ms-bridge-v1|server|" + nonce + "|" + client_nonce))`; the extension
    /// closes the socket if it is wrong (a rogue process squatting the port).
    pub server_auth: String,
}

/// app -> extension, the handshake failed; the socket closes with 1008 right after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectMsg {
    /// Why.
    pub code: RejectCode,
    /// Diagnostic text (English, for the extension's console).
    pub message: String,
}

/// app -> extension, call `module.method(params)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestMsg {
    /// Request id in `1..=MAX_WIRE_ID`, unique among the app's in-flight requests.
    pub id: u64,
    /// Module name.
    pub module: String,
    /// Method name inside the module.
    pub method: String,
    /// Module-defined params.
    pub params: Value,
    /// How long the extension may work on it, in milliseconds (attention time excluded).
    pub timeout_ms: u32,
}

/// app -> extension, abort request `id`. The app has already stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelMsg {
    /// The request to abort.
    pub id: u64,
}

/// The `error` object of a failed `response`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteErrorMsg {
    /// Closed error code.
    pub code: ErrorCode,
    /// Diagnostic text from the extension (English).
    #[serde(default)]
    pub message: String,
    /// For `rate_limited`: how long the site asked to wait, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u32>,
}

impl RemoteErrorMsg {
    /// `retry_after_ms` as a `Duration`.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after_ms.map(|ms| Duration::from_millis(u64::from(ms)))
    }
}

/// extension -> app, the final answer to request `id`: `ok: true` with `result`, or `ok: false`
/// with `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseMsg {
    /// The request id.
    pub id: u64,
    /// Success flag.
    pub ok: bool,
    /// Module-defined result (`ok: true`); absent means `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The failure (`ok: false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RemoteErrorMsg>,
}

/// extension -> app, a notification while request `id` runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventMsg {
    /// The request id the event belongs to.
    pub id: u64,
    /// What happened.
    pub kind: EventKind,
    /// Module-defined details.
    #[serde(default)]
    pub data: Value,
}

/// Every message the app sends. JSON text frames with the envelope field `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppMessage {
    /// First frame after the upgrade.
    Challenge(ChallengeMsg),
    /// Handshake accepted.
    Welcome(WelcomeMsg),
    /// Handshake refused.
    Reject(RejectMsg),
    /// A module call.
    Request(RequestMsg),
    /// Abort a module call.
    Cancel(CancelMsg),
    /// Ask the extension to `chrome.runtime.reload()` (after the app re-unpacked it).
    Reload,
    /// Liveness probe.
    Ping,
    /// Answer to `ping`.
    Pong,
}

/// Every message the extension sends. JSON text frames with the envelope field `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExtMessage {
    /// Answer to `challenge`.
    Hello(HelloMsg),
    /// Final answer to a request.
    Response(ResponseMsg),
    /// Notification while a request runs.
    Event(EventMsg),
    /// Liveness probe (the extension pings every 20 s).
    Ping,
    /// Answer to `ping`.
    Pong,
}

/// The three frozen `hello` fields needed to authenticate a peer of ANY protocol version; read
/// before (and independently of) the full `HelloMsg`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HelloCore {
    pub(crate) protocol: u32,
    pub(crate) client_nonce: String,
    pub(crate) auth: String,
}

/// The frozen, display-only `hello` fields, each defaulted so that an extension of another
/// protocol version still yields a `PeerInfo`.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct HelloDetails {
    #[serde(default)]
    pub(crate) build_id: Option<String>,
    #[serde(default)]
    pub(crate) ext_version: String,
    #[serde(default)]
    pub(crate) browser: BrowserInfo,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_code_tokens_match_serde() {
        for code in ErrorCode::ALL {
            let text = serde_json::to_value(code).unwrap();
            assert_eq!(text, Value::String(code.as_str().to_owned()));
        }
    }

    #[test]
    fn plan_message_shapes_round_trip() {
        let hello = json!({"type":"hello","protocol":1,"build_id":"abc","ext_version":"1.0.0",
            "browser":{"name":"Chrome","version":"130"},"client_nonce":"00112233445566778899aabbccddeeff",
            "auth":"ff","modules":[{"name":"translate","version":1,"sites":["google","deepl","yandex"]}]});
        let parsed: ExtMessage = serde_json::from_value(hello.clone()).unwrap();
        assert!(matches!(&parsed, ExtMessage::Hello(h) if h.modules[0].name == "translate"));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), hello);

        let ok = json!({"type":"response","id":7,"ok":true,"result":{"text":"x"}});
        assert!(matches!(serde_json::from_value::<ExtMessage>(ok).unwrap(), ExtMessage::Response(r) if r.ok && r.id == 7));
        let err = json!({"type":"response","id":8,"ok":false,"error":{"code":"rate_limited","message":"slow","retry_after_ms":1500}});
        let ExtMessage::Response(r) = serde_json::from_value::<ExtMessage>(err).unwrap() else { panic!("not a response") };
        let remote = r.error.unwrap();
        assert_eq!(remote.code, ErrorCode::RateLimited);
        assert_eq!(remote.retry_after(), Some(Duration::from_millis(1500)));

        let ev = json!({"type":"event","id":8,"kind":"attention_cleared","data":{}});
        assert!(matches!(serde_json::from_value::<ExtMessage>(ev).unwrap(), ExtMessage::Event(e) if e.kind == EventKind::AttentionCleared));

        assert_eq!(serde_json::to_value(AppMessage::Reload).unwrap(), json!({"type":"reload"}));
        assert_eq!(serde_json::to_value(AppMessage::Cancel(CancelMsg { id: 3 })).unwrap(), json!({"type":"cancel","id":3}));
        let welcome = AppMessage::Welcome(WelcomeMsg { protocol: 1, app_version: "5000".into(), expected_build_id: None, state: PeerState::Outdated, server_auth: "aa".into() });
        assert_eq!(serde_json::to_value(welcome).unwrap(), json!({"type":"welcome","protocol":1,"app_version":"5000","expected_build_id":null,"state":"outdated","server_auth":"aa"}));
    }

    #[test]
    fn hello_core_reads_a_future_protocol_hello() {
        // A protocol-2 extension may change everything except the frozen fields.
        let future = json!({"type":"hello","protocol":2,"client_nonce":"ab","auth":"cd","modules":"changed shape","browser":{"name":"Edg"}});
        let core: HelloCore = serde_json::from_value(future.clone()).unwrap();
        assert_eq!(core.protocol, 2);
        let details: HelloDetails = serde_json::from_value(future).unwrap();
        assert_eq!(details.browser.name, "Edg");
        assert_eq!(details.build_id, None);
    }
}
