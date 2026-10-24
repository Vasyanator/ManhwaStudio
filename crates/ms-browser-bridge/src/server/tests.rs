//! Loopback tests of the bridge server against `FakeExt` (the real protocol on the tungstenite
//! client). Every server lives on its own temp folder and port; `BridgeService::shared()` is
//! never used.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::Message;

use super::connection::{HelloVerdict, evaluate_hello, upgrade_refusal};
use super::*;
use crate::api::{BridgeEvent, BridgeRequest, ListenState, ReloadTarget, RequestOptions};
use crate::auth::{self, Secret};
use crate::protocol::{AppMessage, BrowserInfo, EventKind, MAX_EVENT_MESSAGE_BYTES, MAX_WIRE_ID, ModuleInfo, PROTOCOL_VERSION, PeerState, RejectCode, WS_PATH};
use crate::test_support::{FakeExt, HelloSpec, TEST_ORIGIN, TempDir, published_secret, test_config, upgrade};

const LONG: Duration = Duration::from_secs(5);

fn start(label: &str) -> (TempDir, BridgeServer) {
    let dir = TempDir::new(label);
    let server = BridgeServer::start(test_config(dir.path())).expect("server start");
    (dir, server)
}

fn start_with(label: &str, adjust: impl FnOnce(&mut BridgeConfig)) -> (TempDir, BridgeServer) {
    let dir = TempDir::new(label);
    let mut cfg = test_config(dir.path());
    adjust(&mut cfg);
    let server = BridgeServer::start(cfg).expect("server start");
    (dir, server)
}

fn translate_request(params: &Value) -> BridgeRequest<'_> {
    BridgeRequest { module: "translate", module_version: 1, method: "translate", params }
}

fn options(timeout: Duration) -> RequestOptions<'static> {
    RequestOptions { timeout, attention_timeout: LONG, cancel: None, on_event: None }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + LONG;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reads frames until a `request` arrives; returns it.
fn expect_request(ext: &mut FakeExt) -> Value {
    loop {
        let message = ext.recv_json(LONG).expect("a request frame");
        if message["type"] == "request" {
            return message;
        }
    }
}

fn http_status(err: &tungstenite::Error) -> Option<u16> {
    match err {
        tungstenite::Error::Http(response) => Some(response.status().as_u16()),
        _ => None,
    }
}

// ---- upgrade guards -------------------------------------------------------------------------

#[test]
fn upgrade_rule_checks_path_host_and_origin() {
    let ok = upgrade_refusal(WS_PATH, None, Some("127.0.0.1:4000"), Some(TEST_ORIGIN), 4000, TEST_ORIGIN);
    assert_eq!(ok, None);
    assert_eq!(upgrade_refusal("/other", None, Some("127.0.0.1:4000"), Some(TEST_ORIGIN), 4000, TEST_ORIGIN), Some("wrong path"));
    assert_eq!(upgrade_refusal(WS_PATH, Some("x=1"), Some("127.0.0.1:4000"), Some(TEST_ORIGIN), 4000, TEST_ORIGIN), Some("wrong path"));
    assert_eq!(upgrade_refusal(WS_PATH, None, Some("localhost:4000"), Some(TEST_ORIGIN), 4000, TEST_ORIGIN), Some("wrong host"));
    assert_eq!(upgrade_refusal(WS_PATH, None, Some("127.0.0.1:4001"), Some(TEST_ORIGIN), 4000, TEST_ORIGIN), Some("wrong host"));
    assert_eq!(upgrade_refusal(WS_PATH, None, None, Some(TEST_ORIGIN), 4000, TEST_ORIGIN), Some("wrong host"));
    assert_eq!(upgrade_refusal(WS_PATH, None, Some("127.0.0.1:4000"), Some("https://evil.example"), 4000, TEST_ORIGIN), Some("wrong origin"));
    assert_eq!(upgrade_refusal(WS_PATH, None, Some("127.0.0.1:4000"), None, 4000, TEST_ORIGIN), Some("wrong origin"));
}

#[test]
fn wrong_origin_gets_403() {
    let (_dir, server) = start("origin");
    let err = upgrade(server.port(), WS_PATH, "https://evil.example", None).expect_err("must be refused");
    assert_eq!(http_status(&err), Some(403));
    assert_eq!(server.status().peers, [] as [crate::api::PeerInfo; 0]);
}

#[test]
fn wrong_host_gets_403() {
    let (_dir, server) = start("host");
    let host = format!("rebind.example:{}", server.port());
    let err = upgrade(server.port(), WS_PATH, TEST_ORIGIN, Some(&host)).expect_err("must be refused");
    assert_eq!(http_status(&err), Some(403));
}

#[test]
fn wrong_path_gets_403() {
    let (_dir, server) = start("path");
    let err = upgrade(server.port(), "/elsewhere", TEST_ORIGIN, None).expect_err("must be refused");
    assert_eq!(http_status(&err), Some(403));
}

// ---- handshake ------------------------------------------------------------------------------

#[test]
fn good_hello_is_welcomed_ready() {
    let (dir, server) = start("welcome");
    let (_ext, welcome) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["state"], "ready");
    assert_eq!(welcome["app_version"], "4242");
    assert_eq!(welcome["expected_build_id"], crate::test_support::TEST_BUILD);
    let status = server.status();
    assert_eq!(status.listen, ListenState::Listening { port: server.port() });
    assert_eq!(status.peers.len(), 1);
    assert_eq!(status.peers[0].state, PeerState::Ready);
    assert!(status.peers[0].build_current);
    assert_eq!(status.peers[0].browser.name, "Chrome");
    assert!(status.peers[0].supports("translate", 1));
}

#[test]
fn bad_hmac_is_rejected_and_closed() {
    let (dir, server) = start("bad-hmac");
    let spec = HelloSpec { secret: Some(Secret::from_bytes([9; 32])), ..HelloSpec::default() };
    let (mut ext, answer) = FakeExt::connect(server.port(), dir.path(), &spec);
    assert_eq!(answer["type"], "reject");
    assert_eq!(answer["code"], "auth_failed");
    assert_eq!(ext.wait_close(LONG), Some(1008));
    assert_eq!(server.status().peers, [] as [crate::api::PeerInfo; 0]);
}

#[test]
fn hello_decision_table() {
    let (dir, server) = start("hello-table");
    let secret = published_secret(dir.path());
    let nonce = auth::generate_nonce().unwrap();
    let client_nonce = auth::generate_nonce().unwrap();
    let tag = auth::client_auth(&secret, &nonce, &client_nonce);
    let shared = &server.shared;

    let verdict = |text: String| evaluate_hello(shared, &nonce, &text);
    assert!(matches!(verdict("not json".into()), HelloVerdict::Reject { code: RejectCode::ProtocolMismatch, .. }));
    assert!(matches!(verdict(json!({"type":"ping"}).to_string()), HelloVerdict::Reject { code: RejectCode::ProtocolMismatch, .. }));
    assert!(matches!(verdict(json!({"type":"hello","protocol":1}).to_string()), HelloVerdict::Reject { code: RejectCode::ProtocolMismatch, .. }));
    // A short client nonce is refused even with a matching tag.
    let short_tag = auth::client_auth(&secret, &nonce, "ab");
    assert!(matches!(verdict(json!({"type":"hello","protocol":1,"client_nonce":"ab","auth":short_tag}).to_string()), HelloVerdict::Reject { code: RejectCode::AuthFailed, .. }));
    // Valid tag, same protocol, but a malformed module list: refused as malformed.
    let broken = json!({"type":"hello","protocol":1,"client_nonce":client_nonce,"auth":tag,"modules":"nope"});
    assert!(matches!(verdict(broken.to_string()), HelloVerdict::Reject { code: RejectCode::ProtocolMismatch, .. }));
    // Valid tag, another protocol with an unknown shape: accepted as outdated.
    let future = json!({"type":"hello","protocol":99,"client_nonce":client_nonce,"auth":tag,"modules":"new shape","build_id":"zzz"});
    match verdict(future.to_string()) {
        HelloVerdict::Accept { info, .. } => {
            assert_eq!(info.state, PeerState::Outdated);
            assert_eq!(info.protocol, 99);
            assert_eq!(info.modules, [] as [ModuleInfo; 0]);
            assert!(!info.build_current);
        }
        other @ HelloVerdict::Reject { .. } => panic!("expected outdated accept, got {other:?}"),
    }
}

#[test]
fn protocol_mismatch_is_welcomed_outdated_requests_fail_and_reload_is_delivered() {
    let (dir, server) = start("outdated");
    let spec = HelloSpec { protocol: crate::protocol::PROTOCOL_VERSION + 1, ..HelloSpec::default() };
    let (mut ext, welcome) = FakeExt::connect(server.port(), dir.path(), &spec);
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["state"], "outdated");
    assert_eq!(server.status().peers[0].state, PeerState::Outdated);

    let params = json!({});
    assert_eq!(server.request(&translate_request(&params), &options(LONG)), Err(BridgeError::Outdated));

    // An outdated peer still gets pong for its ping.
    ext.send_json(&json!({"type":"ping"}));
    assert_eq!(ext.recv_json(LONG).expect("pong")["type"], "pong");

    assert_eq!(server.request_reload(ReloadTarget::NeedsUpdate), 1);
    assert_eq!(ext.recv_json(LONG).expect("reload"), json!({"type":"reload"}));
}

#[test]
fn foreign_build_stays_ready_but_needs_reload() {
    let (dir, server) = start("foreign-build");
    let spec = HelloSpec { build_id: Some("build-b".into()), ..HelloSpec::default() };
    let (mut ext, welcome) = FakeExt::connect(server.port(), dir.path(), &spec);
    assert_eq!(welcome["state"], "ready");
    let peer = server.status().peers[0].clone();
    assert_eq!(peer.state, PeerState::Ready);
    assert!(!peer.build_current);
    assert!(peer.needs_reload());

    // Requests still work.
    let responder = thread::spawn(move || {
        let request = expect_request(&mut ext);
        ext.send_json(&json!({"type":"response","id":request["id"],"ok":true,"result":"fine"}));
        ext
    });
    let params = json!({});
    assert_eq!(server.request(&translate_request(&params), &options(LONG)), Ok(json!("fine")));
    let mut ext = responder.join().unwrap();

    assert_eq!(server.request_reload(ReloadTarget::NeedsUpdate), 1);
    assert_eq!(ext.recv_json(LONG).expect("reload")["type"], "reload");
}

#[test]
fn silent_upgrade_is_dropped_at_the_handshake_deadline() {
    let (_dir, server) = start_with("handshake-deadline", |cfg| cfg.timings.handshake = Duration::from_millis(300));
    let ws = upgrade(server.port(), WS_PATH, TEST_ORIGIN, None).expect("upgrade");
    ws.get_ref().set_read_timeout(Some(Duration::from_millis(20))).unwrap();
    let mut ext = FakeExt { ws };
    assert_eq!(ext.recv_json(LONG).expect("challenge")["type"], "challenge");
    let started = Instant::now();
    assert_eq!(ext.wait_close(LONG), Some(1008));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(server.status().peers, [] as [crate::api::PeerInfo; 0]);
}

// ---- requests -------------------------------------------------------------------------------

#[test]
fn request_round_trip() {
    let (dir, server) = start("round-trip");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let responder = thread::spawn(move || {
        let request = expect_request(&mut ext);
        ext.send_json(&json!({"type":"response","id":request["id"],"ok":true,"result":{"text":"hola"}}));
        (request, ext)
    });
    let params = json!({"text":"hello","src":"en","tgt":"es"});
    let result = server.request(&translate_request(&params), &options(Duration::from_secs(7)));
    assert_eq!(result, Ok(json!({"text":"hola"})));
    let (request, _ext) = responder.join().unwrap();
    assert_eq!(request["module"], "translate");
    assert_eq!(request["method"], "translate");
    assert_eq!(request["params"], params);
    assert_eq!(request["timeout_ms"], 7000);
    assert!(request["id"].as_u64().is_some_and(|id| id >= 1));
    assert!(server.shared.pending.lock().unwrap().is_empty(), "pending entry removed");
}

#[test]
fn ping_gets_pong() {
    let (dir, server) = start("ping");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    ext.send_json(&json!({"type":"ping"}));
    assert_eq!(ext.recv_json(LONG).expect("pong"), json!({"type":"pong"}));
}

#[test]
fn remote_errors_keep_their_code_and_retry_after() {
    let (dir, server) = start("remote-error");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let responder = thread::spawn(move || {
        let request = expect_request(&mut ext);
        ext.send_json(&json!({"type":"response","id":request["id"],"ok":false,"error":{"code":"rate_limited","message":"429","retry_after_ms":2500}}));
        let request = expect_request(&mut ext);
        ext.send_json(&json!({"type":"response","id":request["id"],"ok":false}));
        let request = expect_request(&mut ext);
        ext.send_json(&json!({"type":"response","id":request["id"],"ok":false,"error":{"code":"not_a_code"}}));
        ext
    });
    let params = json!({});
    assert_eq!(
        server.request(&translate_request(&params), &options(LONG)),
        Err(BridgeError::Remote { code: crate::protocol::ErrorCode::RateLimited, message: "429".into(), retry_after: Some(Duration::from_millis(2500)) })
    );
    assert!(matches!(server.request(&translate_request(&params), &options(LONG)), Err(BridgeError::Protocol { .. })));
    // A response that does not parse fails its request at once instead of waiting out the deadline.
    let started = Instant::now();
    assert!(matches!(server.request(&translate_request(&params), &options(LONG)), Err(BridgeError::Protocol { .. })));
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(responder.join().unwrap());
}

#[test]
fn capability_and_connection_errors() {
    let (dir, server) = start("capability");
    let params = json!({});
    assert_eq!(server.request(&translate_request(&params), &options(LONG)), Err(BridgeError::NotConnected));
    let (_ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let wanted = BridgeRequest { module: "translate", module_version: 2, method: "translate", params: &params };
    assert_eq!(server.request(&wanted, &options(LONG)), Err(BridgeError::CapabilityMissing { module: "translate".into(), version: 2 }));
}

#[test]
fn events_reach_the_caller_and_attention_suspends_the_deadline() {
    let (dir, server) = start("attention");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let responder = thread::spawn(move || {
        let request = expect_request(&mut ext);
        let id = request["id"].clone();
        ext.send_json(&json!({"type":"event","id":id,"kind":"progress","data":{"step":1}}));
        ext.send_json(&json!({"type":"event","id":id,"kind":"attention","data":{"reason":"captcha"}}));
        // Far longer than the 300 ms request timeout: only the suspension keeps it alive.
        thread::sleep(Duration::from_millis(900));
        ext.send_json(&json!({"type":"event","id":id,"kind":"attention_cleared","data":{}}));
        thread::sleep(Duration::from_millis(100));
        ext.send_json(&json!({"type":"response","id":id,"ok":true,"result":"done"}));
        ext
    });
    let seen: Mutex<Vec<EventKind>> = Mutex::new(Vec::new());
    let on_event = |event: &BridgeEvent| seen.lock().unwrap().push(event.kind);
    let opts = RequestOptions { timeout: Duration::from_millis(300), attention_timeout: LONG, cancel: None, on_event: Some(&on_event) };
    let params = json!({});
    assert_eq!(server.request(&translate_request(&params), &opts), Ok(json!("done")));
    assert_eq!(*seen.lock().unwrap(), vec![EventKind::Progress, EventKind::Attention, EventKind::AttentionCleared]);
    drop(responder.join().unwrap());
}

#[test]
fn attention_longer_than_its_limit_fails_and_cancels() {
    let (dir, server) = start("attention-limit");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let responder = thread::spawn(move || {
        let request = expect_request(&mut ext);
        ext.send_json(&json!({"type":"event","id":request["id"],"kind":"attention","data":{}}));
        let cancel = ext.recv_json(LONG).expect("cancel");
        (request, cancel)
    });
    let opts = RequestOptions { timeout: LONG, attention_timeout: Duration::from_millis(300), cancel: None, on_event: None };
    let params = json!({});
    assert_eq!(server.request(&translate_request(&params), &opts), Err(BridgeError::AttentionTimeout));
    let (request, cancel) = responder.join().unwrap();
    assert_eq!(cancel, json!({"type":"cancel","id":request["id"]}));
}

#[test]
fn cancel_returns_within_a_slice_and_sends_cancel() {
    let (dir, server) = start("cancel");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let observer = thread::spawn(move || {
        let request = expect_request(&mut ext);
        let cancel = ext.recv_json(LONG).expect("cancel");
        (request, cancel)
    });
    let flag = Arc::new(AtomicBool::new(false));
    let setter = {
        let flag = Arc::clone(&flag);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            flag.store(true, Ordering::Relaxed);
            Instant::now()
        })
    };
    let opts = RequestOptions { timeout: Duration::from_secs(30), attention_timeout: LONG, cancel: Some(flag.as_ref()), on_event: None };
    let params = json!({});
    assert_eq!(server.request(&translate_request(&params), &opts), Err(BridgeError::Cancelled));
    let returned = Instant::now();
    let set_at = setter.join().unwrap();
    assert!(returned.duration_since(set_at) <= CANCEL_SLICE + Duration::from_millis(100), "noticed the cancel late: {:?}", returned.duration_since(set_at));
    let (request, cancel) = observer.join().unwrap();
    assert_eq!(cancel, json!({"type":"cancel","id":request["id"]}));
}

#[test]
fn deadline_times_out_and_sends_cancel() {
    let (dir, server) = start("timeout");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let observer = thread::spawn(move || {
        let request = expect_request(&mut ext);
        (request, ext.recv_json(LONG).expect("cancel"))
    });
    let params = json!({});
    let started = Instant::now();
    assert_eq!(server.request(&translate_request(&params), &options(Duration::from_millis(200))), Err(BridgeError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(1));
    let (request, cancel) = observer.join().unwrap();
    assert_eq!(cancel["id"], request["id"]);
    // A late response for the finished request is ignored without harm.
    assert!(server.shared.pending.lock().unwrap().is_empty());
}

#[test]
fn peer_disconnect_fails_the_pending_request() {
    let (dir, server) = start("disconnect");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let dropper = thread::spawn(move || {
        expect_request(&mut ext);
        drop(ext);
    });
    let params = json!({});
    let started = Instant::now();
    assert_eq!(server.request(&translate_request(&params), &options(Duration::from_secs(30))), Err(BridgeError::Disconnected));
    assert!(started.elapsed() < Duration::from_secs(3));
    dropper.join().unwrap();
    wait_until("peer unregistered", || server.status().peers.is_empty());
}

#[test]
fn silent_peer_is_dropped_but_a_pinging_one_stays() {
    let (dir, server) = start_with("silence", |cfg| cfg.timings.silence = Duration::from_millis(400));
    let (mut pinging, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let (mut silent, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec { browser: "Silent", ..HelloSpec::default() });
    let keeper = thread::spawn(move || {
        let until = Instant::now() + Duration::from_millis(1200);
        while Instant::now() < until {
            pinging.send_json(&json!({"type":"ping"}));
            assert_eq!(pinging.recv_json(LONG).expect("pong")["type"], "pong");
            thread::sleep(Duration::from_millis(100));
        }
        pinging
    });
    assert_eq!(silent.wait_close(LONG), Some(1008));
    let _pinging = keeper.join().unwrap();
    let peers = server.status().peers;
    assert_eq!(peers.len(), 1, "{peers:?}");
    assert_eq!(peers[0].browser.name, "Chrome");
}

#[test]
fn requests_go_to_the_oldest_ready_peer() {
    let (dir, server) = start("routing");
    let (outdated, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec { protocol: 99, browser: "Old", ..HelloSpec::default() });
    let (first, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec { browser: "First", ..HelloSpec::default() });
    let (second, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec { browser: "Second", ..HelloSpec::default() });
    let answer_as = |mut ext: FakeExt, name: &'static str| {
        thread::spawn(move || {
            if let Some(request) = ext.recv_json(Duration::from_millis(1500)).filter(|m| m["type"] == "request") {
                ext.send_json(&json!({"type":"response","id":request["id"],"ok":true,"result":name}));
            }
            ext
        })
    };
    let first_thread = answer_as(first, "first");
    let second_thread = answer_as(second, "second");
    let params = json!({});
    assert_eq!(server.request(&translate_request(&params), &options(LONG)), Ok(json!("first")));
    drop(first_thread.join().unwrap());
    let second = second_thread.join().unwrap();
    wait_until("first peer gone", || server.status().peers.len() == 2);

    let second_thread = answer_as(second, "second");
    assert_eq!(server.request(&translate_request(&params), &options(LONG)), Ok(json!("second")));
    drop(second_thread.join().unwrap());
    drop(outdated);
}

// ---- frame limits ---------------------------------------------------------------------------

#[test]
fn binary_frame_closes_with_1003() {
    let (dir, server) = start("binary");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    ext.ws.send(Message::binary(vec![1_u8, 2, 3])).unwrap();
    assert_eq!(ext.wait_close(LONG), Some(1003));
    wait_until("peer unregistered", || server.status().peers.is_empty());
}

#[test]
fn oversized_text_ends_the_connection() {
    let (dir, server) = start("oversized");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let huge = "x".repeat(crate::protocol::MAX_TEXT_MESSAGE_BYTES + 1);
    // The server may close while the frame is still being written; either way it must go.
    if ext.ws.send(Message::text(huge)).is_ok() {
        ext.wait_close(LONG);
    }
    wait_until("peer unregistered", || server.status().peers.is_empty());
}

#[test]
fn shutdown_closes_peers() {
    let (dir, server) = start("shutdown");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    server.shutdown();
    assert_eq!(ext.wait_close(LONG), Some(1001));
    assert_eq!(server.status().listen, ListenState::NotStarted);
}

// ---- bounded queues, ids, targeted reload ---------------------------------------------------

/// Runs one translate request whose `on_event` blocks for 600 ms on the first event, while
/// `flood` (on the fake extension's thread) sends whatever it wants. Returns the outcome and the
/// number of events the caller saw.
fn request_with_slow_listener(label: &str, flood: impl FnOnce(&mut FakeExt, &Value) + Send + 'static) -> (Result<Value, BridgeError>, usize, FakeExt) {
    let (dir, server) = start(label);
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let flooder = thread::spawn(move || {
        let request = expect_request(&mut ext);
        flood(&mut ext, &request["id"]);
        ext
    });
    let seen = AtomicUsize::new(0);
    let on_event = |_event: &BridgeEvent| {
        if seen.fetch_add(1, Ordering::Relaxed) == 0 {
            thread::sleep(Duration::from_millis(600));
        }
    };
    let opts = RequestOptions { timeout: LONG, attention_timeout: LONG, cancel: None, on_event: Some(&on_event) };
    let params = json!({});
    let outcome = server.request(&translate_request(&params), &opts);
    let ext = flooder.join().unwrap();
    (outcome, seen.load(Ordering::Relaxed), ext)
}

/// Sends `count` events of `kind`; stops quietly once the server closed the socket.
fn send_events(ext: &mut FakeExt, id: &Value, kind: &str, count: usize) {
    for step in 0..count {
        let event = json!({"type":"event","id":id,"kind":kind,"data":{"step":step}});
        if ext.ws.send(Message::text(event.to_string())).is_err() {
            return;
        }
    }
}

#[test]
fn a_progress_flood_is_capped_and_the_response_still_arrives() {
    let (outcome, seen, _ext) = request_with_slow_listener("progress-flood", |ext, id| {
        send_events(ext, id, "progress", 200);
        ext.send_json(&json!({"type":"response","id":id,"ok":true,"result":"done"}));
    });
    assert_eq!(outcome, Ok(json!("done")), "the terminal reply is never dropped");
    assert!((1..=EVENT_QUEUE_CAPACITY + 1).contains(&seen), "saw {seen} events");
}

#[test]
fn an_attention_flood_beyond_the_budget_drops_the_peer() {
    let (outcome, seen, mut ext) = request_with_slow_listener("attention-flood", |ext, id| send_events(ext, id, "attention", EVENT_QUEUE_CAPACITY + 10));
    assert_eq!(outcome, Err(BridgeError::Disconnected));
    assert!(seen <= EVENT_QUEUE_CAPACITY + 1, "saw {seen} events");
    assert_eq!(ext.wait_close(LONG), Some(1008));
}

#[test]
fn an_oversized_event_drops_the_peer() {
    let (dir, server) = start("event-size");
    let (mut ext, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec::default());
    let sender = thread::spawn(move || {
        let request = expect_request(&mut ext);
        let data = "x".repeat(MAX_EVENT_MESSAGE_BYTES);
        ext.send_json(&json!({"type":"event","id":request["id"],"kind":"progress","data":data}));
        ext.wait_close(LONG)
    });
    let params = json!({});
    assert_eq!(server.request(&translate_request(&params), &options(LONG)), Err(BridgeError::Disconnected));
    assert_eq!(sender.join().unwrap(), Some(1009));
}

/// A registered peer whose connection thread takes nothing: its outbound queue is full.
fn wedged_peer(server: &BridgeServer, id: u64) -> mpsc::Receiver<AppMessage> {
    let (outbound, receiver) = mpsc::sync_channel(OUTBOUND_CAPACITY);
    for _ in 0..OUTBOUND_CAPACITY {
        outbound.try_send(AppMessage::Pong).unwrap();
    }
    let info = PeerInfo {
        id,
        browser: BrowserInfo::default(),
        ext_version: "1.0.0".into(),
        build_id: Some(crate::test_support::TEST_BUILD.into()),
        protocol: PROTOCOL_VERSION,
        modules: vec![ModuleInfo { name: "translate".into(), version: 1, sites: Vec::new() }],
        state: PeerState::Ready,
        build_current: false,
    };
    server.shared.peers.lock().unwrap().push(PeerSlot { info, outbound });
    receiver
}

#[test]
fn a_peer_that_takes_no_messages_fails_requests_at_once() {
    let (_dir, server) = start("outbound-full");
    let _receiver = wedged_peer(&server, 77);
    let params = json!({});
    let started = Instant::now();
    assert_eq!(server.request(&translate_request(&params), &options(LONG)), Err(BridgeError::Disconnected));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(server.shared.pending.lock().unwrap().is_empty());
    assert!(server.request_reload_peers(&[77]).is_empty(), "a full queue is reported as not queued");
    assert_eq!(server.request_reload(ReloadTarget::All), 0);
}

#[test]
fn request_ids_stay_js_safe_and_skip_pending_ones() {
    let counter = AtomicU64::new(MAX_WIRE_ID);
    let mut pending = HashMap::new();
    assert_eq!(allocate_request_id(&counter, &pending), MAX_WIRE_ID);
    assert_eq!(allocate_request_id(&counter, &pending), 1, "wraps inside the JS-safe range");
    let (reply, _replies) = mpsc::sync_channel(1);
    pending.insert(2, Pending { peer: 1, reply, budget: Arc::default() });
    assert_eq!(allocate_request_id(&counter, &pending), 3, "an id still pending is skipped");
    let wild = AtomicU64::new(u64::MAX);
    assert_eq!(allocate_request_id(&wild, &HashMap::new()), 1);
}

#[test]
fn reload_can_target_one_peer() {
    let (dir, server) = start("reload-one");
    let stale = HelloSpec { build_id: Some("build-b".into()), ..HelloSpec::default() };
    let (mut first, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec { browser: "First", ..stale.clone() });
    let (mut second, _) = FakeExt::connect(server.port(), dir.path(), &HelloSpec { browser: "Second", ..stale });
    let second_id = server.status().peers.iter().find(|peer| peer.browser.name == "Second").map(|peer| peer.id).unwrap();
    assert_eq!(server.request_reload_peers(&[second_id]), vec![second_id]);
    assert_eq!(second.recv_json(LONG), Some(json!({"type":"reload"})));
    assert_eq!(first.recv_json(Duration::from_millis(300)), None, "the other stale peer is not addressed");
}

#[test]
fn a_second_server_on_the_folder_is_refused_until_the_first_is_gone() {
    let (dir, first) = start("two-owners");
    assert_eq!(BridgeServer::start(test_config(dir.path())).map(|server| server.port()), Err(BridgeError::OwnedByOtherProcess));
    first.shutdown();
    // The listener thread releases the lock within one poll after its socket closed.
    let deadline = Instant::now() + LONG;
    let second = loop {
        match BridgeServer::start(test_config(dir.path())) {
            Ok(server) => break server,
            Err(err) => assert_eq!(err, BridgeError::OwnedByOtherProcess),
        }
        assert!(Instant::now() < deadline, "the first server never released the folder");
        thread::sleep(Duration::from_millis(20));
    };
    let (_ext, welcome) = FakeExt::connect(second.port(), dir.path(), &HelloSpec::default());
    assert_eq!(welcome["type"], "welcome");
}
