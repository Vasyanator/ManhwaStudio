/*
File: crates/ms-tab-translation/src/machine_translators/http.rs

Purpose:
The one owner of the machine-translation providers' HTTP transport facts: the timeouts every
request has, whether redirects are followed, and how a `Retry-After` header is read. A provider
builds its `ureq` agent here instead of choosing its own numbers (STATE S-082: an agent without
timeouts can hang a run forever).

Key items:
- `CONNECT_TIMEOUT` / `READ_TIMEOUT` / `REQUEST_TIMEOUT`.
- `agent_settings` (pure, tested) and `build_agent` (applies them to a `ureq::Agent`).
- `retry_after` / `parse_retry_after`: delta-seconds `Retry-After` -> `Duration`.

Notes:
Native only (`ureq` is not compiled for wasm). GUI-free, no logging, no i18n.
*/

use std::time::Duration;

/// TCP + TLS connect budget per request. The MT hosts answer in well under a second; a longer
/// wait means the host is unreachable.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Budget for each socket read of the response. A bubble-sized translation is answered in about
/// a second; 30 s tolerates a slow link without letting a stalled socket hang the run forever.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Overall deadline of one request (connect + send + whole response). `READ_TIMEOUT` alone is
/// per read, so a drip-fed body would never time out; this caps every request.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(40);
/// Redirects followed when a provider asks for them (`ureq`'s own default).
const FOLLOWED_REDIRECTS: u32 = 5;

/// The transport configuration of one provider agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentSettings {
    pub(crate) user_agent: String,
    pub(crate) connect_timeout: Duration,
    pub(crate) read_timeout: Duration,
    pub(crate) request_timeout: Duration,
    /// How many redirects the agent follows; 0 = a 3xx answer is returned as is (so a provider
    /// can classify a redirect to a CAPTCHA or consent page instead of following it).
    pub(crate) redirects: u32,
}

/// The settings `build_agent` applies: the shared timeouts, `user_agent`, and either
/// `FOLLOWED_REDIRECTS` or none.
#[must_use]
pub(crate) fn agent_settings(user_agent: &str, follow_redirects: bool) -> AgentSettings {
    AgentSettings {
        user_agent: user_agent.to_string(),
        connect_timeout: CONNECT_TIMEOUT,
        read_timeout: READ_TIMEOUT,
        request_timeout: REQUEST_TIMEOUT,
        redirects: if follow_redirects { FOLLOWED_REDIRECTS } else { 0 },
    }
}

/// A blocking `ureq` agent for one provider with the shared timeouts. Build it once per run, or
/// once per process (DeepL's `shared_agent`): it pools connections.
#[must_use]
pub(crate) fn build_agent(user_agent: &str, follow_redirects: bool) -> ureq::Agent {
    let settings = agent_settings(user_agent, follow_redirects);
    ureq::AgentBuilder::new()
        .user_agent(&settings.user_agent)
        .timeout_connect(settings.connect_timeout)
        .timeout_read(settings.read_timeout)
        .timeout(settings.request_timeout)
        .redirects(settings.redirects)
        .build()
}

/// The server's `Retry-After` of `response`, when it sent a usable one (see
/// `parse_retry_after`).
#[must_use]
pub(crate) fn retry_after(response: &ureq::Response) -> Option<Duration> {
    response.header("retry-after").and_then(parse_retry_after)
}

/// Parses a `Retry-After` value given as delta-seconds (`"120"`). The HTTP-date form, an empty
/// or negative value and anything else unparsable give `None`: the limiter then uses its own
/// cooldown step, which is always a safe wait (the MT services observed so far send seconds or
/// nothing).
#[must_use]
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    #[test]
    fn retry_after_reads_delta_seconds_only() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after(" 120 "), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("-5"), None);
        assert_eq!(parse_retry_after("1.5"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn retry_after_is_read_from_the_response_header() {
        let parse = |raw: &str| raw.parse::<ureq::Response>().unwrap_or_else(|err| panic!("test response must parse: {err}"));
        let limited = parse("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(retry_after(&limited), Some(Duration::from_secs(7)));
        let dated = parse("HTTP/1.1 503 Unavailable\r\nretry-after: Wed, 21 Oct 2026 07:28:00 GMT\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(retry_after(&dated), None);
        let plain = parse("HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(retry_after(&plain), None);
    }

    #[test]
    fn agent_settings_carry_the_shared_timeouts() {
        let strict = agent_settings("UA/1", false);
        assert_eq!(strict.user_agent, "UA/1");
        assert_eq!(strict.connect_timeout, Duration::from_secs(10));
        assert_eq!(strict.read_timeout, Duration::from_secs(30));
        assert_eq!(strict.request_timeout, Duration::from_secs(40));
        assert_eq!(strict.redirects, 0);
        assert_eq!(agent_settings("UA/1", true).redirects, FOLLOWED_REDIRECTS);
    }

    /// Answers every connection with a redirect to `/target`, and `/target` with `200`.
    fn serve_redirects(listener: &TcpListener, connections: usize) -> Vec<String> {
        let mut seen = Vec::new();
        for stream in listener.incoming().take(connections) {
            let mut stream = stream.unwrap_or_else(|err| panic!("accept: {err}"));
            let mut reader = BufReader::new(stream.try_clone().unwrap_or_else(|err| panic!("clone stream: {err}")));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap_or_else(|err| panic!("request line: {err}"));
            let mut headers = Vec::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap_or_else(|err| panic!("header: {err}"));
                if header.trim().is_empty() {
                    break;
                }
                headers.push(header.trim().to_ascii_lowercase());
            }
            let response = if request_line.contains(" /target") {
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
            } else {
                "HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            stream.write_all(response.as_bytes()).unwrap_or_else(|err| panic!("write: {err}"));
            seen.push(format!("{} | {}", request_line.trim(), headers.iter().find(|h| h.starts_with("user-agent:")).map_or("", String::as_str)));
        }
        seen
    }

    #[test]
    fn built_agent_applies_the_user_agent_and_the_redirect_choice() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|err| panic!("bind: {err}"));
        let base = format!("http://{}", listener.local_addr().unwrap_or_else(|err| panic!("addr: {err}")));
        let seen = std::thread::scope(|scope| {
            let server = scope.spawn(|| serve_redirects(&listener, 3));
            let strict = build_agent("MtTest/1", false).get(&format!("{base}/start")).call().unwrap_or_else(|err| panic!("strict: {err}"));
            assert_eq!(strict.status(), 302, "redirects must not be followed");
            let follow = build_agent("MtTest/1", true).get(&format!("{base}/start")).call().unwrap_or_else(|err| panic!("follow: {err}"));
            assert_eq!(follow.status(), 200, "redirects must be followed");
            server.join().unwrap_or_else(|_| panic!("server thread panicked"))
        });
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|line| line.ends_with("user-agent: mttest/1")), "{seen:?}");
    }
}
