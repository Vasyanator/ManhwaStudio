/*
File: crates/ms-browser-bridge/src/test_support.rs

Purpose:
Test-only helpers: a self-removing temp folder, a short-timing `BridgeConfig`, and
`FakeExt`, a fake browser extension on the tungstenite client that speaks the real protocol
over loopback.

Notes:
- Everything stays under `std::env::temp_dir()`; nothing is written into the repository.
- `FakeExt` reads with a short socket timeout so tests can poll with deadlines.
*/

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

use crate::api::{BridgeConfig, BridgeTimings};
use crate::auth::{self, Secret};
use crate::pairing::{bridge_file_path, read_bridge_file};
use crate::protocol::WS_PATH;

/// The `Origin` the test servers accept.
pub(crate) const TEST_ORIGIN: &str = "chrome-extension://abcdefghijklmnopabcdefghijklmnop";

/// The bundle `build_id` the test servers expect.
pub(crate) const TEST_BUILD: &str = "build-a";

/// A unique folder under the system temp dir, removed on drop.
#[derive(Debug)]
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("ms-browser-bridge-{label}-{}-{}-{nanos}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.0) {
            eprintln!("could not remove test dir {}: {err}", self.0.display());
        }
    }
}

/// A config on `dir` with short timings: handshake 2 s, silence 1 min, poll 20 ms.
pub(crate) fn test_config(dir: &Path) -> BridgeConfig {
    BridgeConfig {
        install_dir: dir.to_path_buf(),
        allowed_origin: TEST_ORIGIN.to_owned(),
        expected_build_id: Some(TEST_BUILD.to_owned()),
        app_version: "4242".to_owned(),
        timings: BridgeTimings { handshake: Duration::from_secs(2), silence: Duration::from_mins(1), poll: Duration::from_millis(20), write: Duration::from_secs(2) },
    }
}

/// The secret the server in `dir` published.
pub(crate) fn published_secret(dir: &Path) -> Secret {
    let file = read_bridge_file(&bridge_file_path(dir)).expect("read bridge.json").expect("bridge.json exists");
    Secret::from_hex(&file.secret).expect("well-formed secret")
}

/// What the fake extension announces in `hello`.
#[derive(Debug, Clone)]
pub(crate) struct HelloSpec {
    pub(crate) protocol: u32,
    pub(crate) build_id: Option<String>,
    pub(crate) modules: Value,
    /// Use this secret instead of the published one (bad-HMAC tests).
    pub(crate) secret: Option<Secret>,
    pub(crate) browser: &'static str,
}

impl Default for HelloSpec {
    fn default() -> Self {
        Self {
            protocol: crate::protocol::PROTOCOL_VERSION,
            build_id: Some(TEST_BUILD.to_owned()),
            modules: json!([{"name":"translate","version":1,"sites":["google","deepl","yandex"]}]),
            secret: None,
            browser: "Chrome",
        }
    }
}

/// Opens the WebSocket upgrade to `127.0.0.1:port` with the given path / Origin / Host.
pub(crate) fn upgrade(port: u16, path: &str, origin: &str, host: Option<&str>) -> Result<WebSocket<TcpStream>, tungstenite::Error> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect to the test server");
    let mut request = format!("ws://127.0.0.1:{port}{path}").into_client_request()?;
    request.headers_mut().insert("Origin", HeaderValue::from_str(origin).expect("header value"));
    if let Some(host) = host {
        request.headers_mut().insert("Host", HeaderValue::from_str(host).expect("header value"));
    }
    match tungstenite::client(request, tcp) {
        Ok((ws, _response)) => Ok(ws),
        Err(tungstenite::HandshakeError::Failure(err)) => Err(err),
        Err(tungstenite::HandshakeError::Interrupted(_)) => panic!("blocking client handshake was interrupted"),
    }
}

/// A connected fake extension.
pub(crate) struct FakeExt {
    pub(crate) ws: WebSocket<TcpStream>,
}

impl FakeExt {
    /// Upgrades with the test origin, reads `challenge`, sends `hello` per `spec`, and returns
    /// the app's answer (`welcome` or `reject`).
    pub(crate) fn connect(port: u16, dir: &Path, spec: &HelloSpec) -> (Self, Value) {
        let ws = upgrade(port, WS_PATH, TEST_ORIGIN, None).expect("upgrade");
        ws.get_ref().set_read_timeout(Some(Duration::from_millis(20))).expect("read timeout");
        let mut ext = Self { ws };
        let challenge = ext.recv_json(Duration::from_secs(3)).expect("challenge");
        assert_eq!(challenge["type"], "challenge");
        let nonce = challenge["nonce"].as_str().expect("nonce").to_owned();
        let client_nonce = auth::generate_nonce().expect("nonce");
        let secret = spec.secret.clone().unwrap_or_else(|| published_secret(dir));
        ext.send_json(&json!({
            "type": "hello",
            "protocol": spec.protocol,
            "build_id": spec.build_id,
            "ext_version": "1.0.0",
            "browser": {"name": spec.browser, "version": "130"},
            "client_nonce": client_nonce,
            "auth": auth::client_auth(&secret, &nonce, &client_nonce),
            "modules": spec.modules,
        }));
        let answer = ext.recv_json(Duration::from_secs(3)).expect("welcome or reject");
        if answer["type"] == "welcome" {
            assert_eq!(answer["server_auth"], auth::server_auth(&secret, &nonce, &client_nonce), "mutual authentication");
        }
        (ext, answer)
    }

    pub(crate) fn send_json(&mut self, value: &Value) {
        self.ws.send(Message::text(value.to_string())).expect("fake extension send");
    }

    /// The next frame within `timeout`: `Ok(Some)` a frame, `Ok(None)` nothing yet, `Err` closed.
    pub(crate) fn recv_message(&mut self, timeout: Duration) -> Result<Option<Message>, tungstenite::Error> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.ws.read() {
                Ok(message) => return Ok(Some(message)),
                Err(tungstenite::Error::Io(err)) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// The next JSON text frame within `timeout` (WS ping / pong skipped); `None` on timeout or
    /// close.
    pub(crate) fn recv_json(&mut self, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv_message(left) {
                Ok(Some(Message::Text(text))) => return Some(serde_json::from_str(text.as_str()).expect("JSON text frame")),
                Ok(Some(Message::Close(_)) | None) | Err(_) => return None,
                Ok(Some(_)) => {}
            }
        }
    }

    /// Reads until the server closes; returns the close code it sent, if any.
    pub(crate) fn wait_close(&mut self, timeout: Duration) -> Option<u16> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.recv_message(left) {
                Ok(Some(Message::Close(frame))) => return frame.map(|f| u16::from(f.code)),
                Ok(Some(_)) => {}
                Ok(None) => panic!("server did not close within {timeout:?}"),
                Err(_) => return None,
            }
        }
    }
}
