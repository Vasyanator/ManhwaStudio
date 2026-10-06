/*
File: crates/ms-ai-api/src/loopback_test_server.rs

Purpose:
Test-only loopback HTTP server speaking the OpenAI-compatible chat protocol (server-sent-events
streams and plain JSON replies), shared by the executor tests of `generation.rs` and
`validated.rs`. No external network, no files.

Key items:
- serve()       : starts a server answering one connection per scripted response, in order.
- TestServer    : its base URL, the received request bodies, the "client closed" signal.
- sse_response() / json_response() / delta_event() / finish_events() / finish_events_with():
                  response builders.

Notes:
Compiled only for native tests (`cfg(all(test, not(target_arch = "wasm32")))` in `lib.rs`).
*/

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// Upper bound for every wait of a test on the server.
pub(crate) const WAIT: Duration = Duration::from_secs(10);

/// One streamed delta event carrying `text` in the delta field `field` (`content` /
/// `reasoning_content`).
pub(crate) fn delta_event(field: &str, text: &str) -> String {
    let delta = serde_json::json!({ "choices": [{ "index": 0, "delta": { field: text }, "finish_reason": null }] });
    format!("data: {delta}\n\n")
}

/// The terminal events of a stream that finished normally (`finish_reason: "stop"`).
pub(crate) fn finish_events() -> Vec<String> {
    finish_events_with("stop")
}

/// The terminal events of a stream whose `finish_reason` is `reason` (e.g. `"length"`).
pub(crate) fn finish_events_with(reason: &str) -> Vec<String> {
    let finish = serde_json::json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": reason }] });
    vec![format!("data: {finish}\n\n"), "data: [DONE]\n\n".to_string()]
}

/// Reads one HTTP request (headers + `Content-Length` body) and returns its body, so the
/// client is never answered before it finished sending.
fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(String::new());
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
        let content_length = headers.lines().find_map(|line| line.strip_prefix("content-length:")).and_then(|value| value.trim().parse::<usize>().ok()).unwrap_or(0);
        if buffer.len() >= header_end + 4 + content_length {
            return Ok(String::from_utf8_lossy(&buffer[header_end + 4..]).into_owned());
        }
    }
}

/// A 200 server-sent-events response whose body is `events`, closed after them.
pub(crate) fn sse_response(events: &[String]) -> String {
    let mut response = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n");
    for event in events {
        response.push_str(event);
    }
    response
}

/// A JSON response with `status_line` (e.g. `"400 Bad Request"`).
pub(crate) fn json_response(status_line: &str, body: &str) -> String {
    format!("HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

/// A loopback server answering one connection per entry of `responses`, in order.
pub(crate) struct TestServer {
    /// `http://127.0.0.1:<port>`.
    pub(crate) base_url: String,
    /// Signalled when the client closed the stalled last connection.
    pub(crate) closed: mpsc::Receiver<()>,
    /// The request bodies, in order.
    pub(crate) requests: mpsc::Receiver<String>,
}

/// Starts a `TestServer`. With `stall`, the last connection is held open after its
/// response until the client closes it.
pub(crate) fn serve(responses: Vec<String>, stall: bool) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port for the test server");
    let port = listener.local_addr().expect("loopback listener has an address").port();
    let (closed_tx, closed) = mpsc::channel();
    let (requests_tx, requests) = mpsc::channel();
    std::thread::spawn(move || {
        let last = responses.len().saturating_sub(1);
        for (index, response) in responses.iter().enumerate() {
            let Ok((mut stream, _)) = listener.accept() else { return };
            let Ok(body) = read_request(&mut stream) else { return };
            // An `Err` only means the test does not inspect the requests.
            requests_tx.send(body).ok();
            if stream.write_all(response.as_bytes()).and_then(|()| stream.flush()).is_err() {
                return;
            }
            if stall && index == last {
                // Blocks until the client drops the connection (read returns 0), bounded
                // by `WAIT` so a broken test cannot leave the thread hanging.
                if stream.set_read_timeout(Some(WAIT)).is_err() {
                    return;
                }
                let mut probe = [0_u8; 64];
                if matches!(stream.read(&mut probe), Ok(0)) {
                    // An `Err` only means the test stopped waiting for the close.
                    closed_tx.send(()).ok();
                }
            }
        }
    });
    TestServer { base_url: format!("http://127.0.0.1:{port}"), closed, requests }
}
