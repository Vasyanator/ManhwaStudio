/*
FILE OVERVIEW: src/tabs/translation/machine_translators/deepl.rs
DeepL backend implementation adapted from translatepy JSON-RPC flow.

Main items:
- `DeeplMtBackend`: backend object implementing shared translator contract.
- `DeeplJsonRpcClient`: throttled JSON-RPC client for DeepL web endpoints.
- `translate_single_text`: translatepy-like flow
  (`LMT_split_into_sentences` -> `LMT_handle_jobs`) per bubble.

Behavior notes:
- Uses DeepL web JSON-RPC endpoints (`www2.deepl.com/jsonrpc`).
- Initializes JSON-RPC session via `getClientState` (`w.deepl.com/web`) to
  obtain cookies + stable request-id baseline (translatepy-compatible).
- Throttles requests to 1 call per 3 seconds to reduce block risk.
- Uses split+handle flow close to `translatepy`, including
  `source_lang_computed` for auto-detect mode.
- Retries 429/rate-limit responses with backoff + state refresh fallback.
*/

use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use ureq::Agent;

use super::MachineTranslatorBackend;

const DEEPL_JSONRPC_URL: &str = "https://www2.deepl.com/jsonrpc";
const DEEPL_CLIENT_STATE_URL: &str = "https://w.deepl.com/web";
const DEEPL_RATE_LIMIT_DELAY: Duration = Duration::from_secs(3);
const DEEPL_RETRY_COUNT: usize = 3;
const DEEPL_CLIENT_STATE_V: &str = "20180814";
const DEEPL_PREFERRED_LANGS: [&str; 2] = ["EN", "RU"];
static DEEPL_GLOBAL_LAST_ACCESS: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

#[derive(Debug, Default, Clone, Copy)]
pub struct DeeplMtBackend;

impl MachineTranslatorBackend for DeeplMtBackend {
    fn translate_texts(
        &self,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, String>>, String> {
        let mut client = DeeplJsonRpcClient::new();
        let source = normalize_deepl_lang(source_lang, true);
        let target = normalize_deepl_lang(target_lang, false);
        let mut out = Vec::with_capacity(texts.len());

        for text in texts {
            let source_text = text.trim().to_string();
            if source_text.is_empty() {
                out.push(Ok(String::new()));
                continue;
            }
            out.push(translate_single_text(
                &mut client,
                &source_text,
                &source,
                &target,
            ));
        }

        Ok(out)
    }
}

fn translate_single_text(
    client: &mut DeeplJsonRpcClient,
    text: &str,
    source_lang: &str,
    target_lang: &str,
) -> Result<String, String> {
    let (sentences, computed_lang) = split_into_sentences(client, text, source_lang, target_lang)?;
    if sentences.is_empty() {
        return Ok(String::new());
    }

    let jobs = build_jobs(&sentences);
    let timestamp = build_timestamp(&sentences);

    let mut user_preferred_langs = vec![target_lang.to_string()];
    let mut lang = json!({
        "target_lang": target_lang,
        "user_preferred_langs": user_preferred_langs.clone(),
    });

    if source_lang == "AUTO" {
        if let Some(computed) = computed_lang {
            let normalized = normalize_deepl_lang(&computed, false);
            if !user_preferred_langs.iter().any(|it| it == &normalized) {
                user_preferred_langs.push(normalized.clone());
                lang["user_preferred_langs"] = json!(user_preferred_langs);
            }
            lang["source_lang_computed"] = Value::String(normalized);
        } else {
            lang["source_lang_user_selected"] = Value::String("AUTO".to_string());
        }
    } else {
        lang["source_lang_user_selected"] = Value::String(source_lang.to_string());
    }

    let params = json!({
        "jobs": jobs,
        "lang": lang,
        "priority": 1,
        "timestamp": timestamp,
    });
    let result = client.send_jsonrpc("LMT_handle_jobs", params)?;
    let translations = result
        .get("translations")
        .and_then(Value::as_array)
        .ok_or_else(|| "DeepL: ответ не содержит translations.".to_string())?;
    if translations.len() != sentences.len() {
        return Err(format!(
            "DeepL: некорректный размер поля translations: ожидалось {}, получено {}.",
            sentences.len(),
            translations.len()
        ));
    }

    let merged = translations
        .iter()
        .filter_map(|item| {
            item.get("beams")
                .and_then(Value::as_array)
                .and_then(|beams| beams.first())
                .and_then(|beam| beam.get("postprocessed_sentence"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok(merged)
}

fn split_into_sentences(
    client: &mut DeeplJsonRpcClient,
    text: &str,
    source_lang: &str,
    target_lang: &str,
) -> Result<(Vec<String>, Option<String>), String> {
    let split_source = if source_lang == "AUTO" {
        "auto"
    } else {
        source_lang
    };
    let mut preferred = DEEPL_PREFERRED_LANGS
        .into_iter()
        .map(|it| it.to_string())
        .collect::<Vec<_>>();
    if !preferred.iter().any(|it| it == target_lang) {
        preferred.push(target_lang.to_string());
    }
    let params = json!({
        "texts": [text.trim()],
        "lang": {
            "lang_user_selected": split_source,
            "user_preferred_langs": preferred,
        }
    });
    let result = client.send_jsonrpc("LMT_split_into_sentences", params)?;
    let sentences = parse_split_sentences(&result);
    if sentences.is_empty() {
        return Ok((vec![text.trim().to_string()], None));
    }
    let computed_lang = result
        .get("lang")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    Ok((sentences, computed_lang))
}

fn parse_split_sentences(result: &Value) -> Vec<String> {
    let Some(splitted) = result.get("splitted_texts").and_then(Value::as_array) else {
        return Vec::new();
    };
    if let Some(first) = splitted.first()
        && let Some(items) = first.as_array()
    {
        return items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
    }
    splitted
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
}

fn build_jobs(sentences: &[String]) -> Vec<Value> {
    let mut jobs = Vec::with_capacity(sentences.len());
    let mut before: Vec<String> = Vec::new();

    for (index, sentence) in sentences.iter().enumerate() {
        let after = if index + 1 < sentences.len() {
            vec![sentences[index + 1].clone()]
        } else {
            Vec::new()
        };
        if index > 0 {
            if before.len() >= 5 {
                before.remove(0);
            }
            before.push(sentences[index - 1].clone());
        }
        jobs.push(json!({
            "kind": "default",
            "raw_en_context_after": after,
            "raw_en_context_before": before.clone(),
            "raw_en_sentence": sentence,
        }));
    }
    jobs
}

fn build_timestamp(sentences: &[String]) -> i64 {
    let mut i_count = 1_i64;
    for sentence in sentences {
        i_count += sentence.matches('i').count() as i64;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let ts = (now_ms / 100) * 100 + 1000;
    ts + (i_count - ts.rem_euclid(i_count))
}

fn normalize_deepl_lang(raw: &str, source: bool) -> String {
    let fallback = if source { "AUTO" } else { "RU" };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    let upper = trimmed.to_ascii_uppercase();
    let code = upper.split('-').next().unwrap_or(&upper).to_string();
    match code.as_str() {
        "AUTO" => "AUTO".to_string(),
        "ZH" | "ZH_CN" | "ZH_TW" => "ZH".to_string(),
        _ => code,
    }
}

struct DeeplJsonRpcClient {
    agent: Agent,
    id_number: i64,
}

impl DeeplJsonRpcClient {
    fn new() -> Self {
        let agent = ureq::AgentBuilder::new().build();
        let mut client = Self {
            agent,
            id_number: initial_request_id(),
        };
        let _ = client.refresh_client_state();
        client
    }

    fn send_jsonrpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        for attempt in 0..=DEEPL_RETRY_COUNT {
            wait_deepl_global_rate_slot();
            self.id_number += 1;
            let payload = json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
                "id": self.id_number,
            });
            let payload_text = serialize_json_like_requests(&payload)
                .map_err(|err| format!("DeepL {method}: ошибка сериализации JSON: {err}"))?;

            let response = self
                .agent
                .post(DEEPL_JSONRPC_URL)
                .set(
                    "User-Agent",
                    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36",
                )
                .set("Accept", "*/*")
                .set("Accept-Language", "en-US,en-GB;q=0.5")
                .set("Accept-Encoding", "gzip, deflate")
                .set("Connection", "keep-alive")
                .set("Referer", "https://www.deepl.com/")
                .set("Origin", "https://www.deepl.com")
                .set("Content-Type", "application/json")
                .send_string(&payload_text);
            let (status, body) = read_response(response)?;
            let value: Value = serde_json::from_str(&body)
                .map_err(|err| format!("DeepL {method}: некорректный JSON: {err}"))?;

            if status == 200 {
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| format!("DeepL {method}: отсутствует поле result."));
            }

            let code = value
                .get("error")
                .and_then(|err| err.get("code"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if attempt < DEEPL_RETRY_COUNT && is_deepl_rate_limited(status, code) {
                let _ = self.refresh_client_state();
                thread::sleep(deepl_retry_backoff(attempt));
                continue;
            }
            let code_text = deepl_error_code_text(code);
            let compact = body.trim().replace('\n', " ");
            return Err(format!(
                "DeepL API error {code} ({code_text}), HTTP {status}: {compact}"
            ));
        }
        Err(format!(
            "DeepL {method}: превышено число повторных попыток."
        ))
    }

    fn refresh_client_state(&mut self) -> Result<(), String> {
        wait_deepl_global_rate_slot();
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "getClientState",
            "params": {
                "v": DEEPL_CLIENT_STATE_V,
                "clientVars": {},
            },
            "id": initial_request_id(),
        });
        let payload_text = serialize_json_like_requests(&payload)
            .map_err(|err| format!("DeepL getClientState: ошибка сериализации JSON: {err}"))?;
        let response = self
            .agent
            .post(DEEPL_CLIENT_STATE_URL)
            .query("request_type", "jsonrpc")
            .query("il", "E")
            .query("method", "getClientState")
            .set(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36",
            )
            .set("Accept", "*/*")
            .set("Accept-Language", "en-US,en-GB;q=0.5")
            .set("Accept-Encoding", "gzip, deflate")
            .set("Connection", "keep-alive")
            .set("Referer", "https://www.deepl.com/")
            .set("Origin", "https://www.deepl.com")
            .set("Content-Type", "application/json")
            .send_string(&payload_text);
        let (status, body) = read_response(response)?;
        if status >= 400 {
            let compact = body.trim().replace('\n', " ");
            return Err(format!("DeepL getClientState: HTTP {status}: {compact}"));
        }
        let value: Value = serde_json::from_str(&body)
            .map_err(|err| format!("DeepL getClientState: некорректный JSON: {err}"))?;
        if let Some(id) = value.get("id").and_then(Value::as_i64) {
            self.id_number = id;
            return Ok(());
        }
        Err("DeepL getClientState: отсутствует поле id.".to_string())
    }
}

fn wait_deepl_global_rate_slot() {
    let lock = DEEPL_GLOBAL_LAST_ACCESS.get_or_init(|| Mutex::new(None));
    if let Ok(mut last_guard) = lock.lock() {
        if let Some(last) = *last_guard {
            let elapsed = last.elapsed();
            if elapsed < DEEPL_RATE_LIMIT_DELAY {
                thread::sleep(DEEPL_RATE_LIMIT_DELAY - elapsed);
            }
        }
        *last_guard = Some(Instant::now());
    } else {
        thread::sleep(DEEPL_RATE_LIMIT_DELAY);
    }
}

fn read_response(response: Result<ureq::Response, ureq::Error>) -> Result<(u16, String), String> {
    match response {
        Ok(resp) => {
            let status = resp.status();
            let body = resp
                .into_string()
                .map_err(|err| format!("DeepL: ошибка чтения тела ответа: {err}"))?;
            Ok((status, body))
        }
        Err(ureq::Error::Status(status, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Ok((status, body))
        }
        Err(ureq::Error::Transport(err)) => Err(format!("DeepL: ошибка сети: {err}")),
    }
}

fn initial_request_id() -> i64 {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros() as i64)
        .unwrap_or(1);
    let suffix = (seed.abs() % 9000) + 1000;
    suffix * 10000 + 1
}

fn serialize_json_like_requests(value: &Value) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value)
}

fn deepl_retry_backoff(attempt: usize) -> Duration {
    let attempt_num = (attempt as u64) + 1;
    let base_secs = DEEPL_RATE_LIMIT_DELAY.as_secs().saturating_mul(attempt_num);
    let jitter_ms = ((SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_millis() as u64)
        .unwrap_or(0))
        % 500)
        + 250;
    Duration::from_secs(base_secs) + Duration::from_millis(jitter_ms)
}

fn deepl_error_code_text(code: i64) -> &'static str {
    match code {
        1042911 => "Too many requests.",
        1042912 => "Too many requests.",
        _ => "ERR_UNKNOWN",
    }
}

fn is_deepl_rate_limited(status: u16, code: i64) -> bool {
    status == 429 || matches!(code, 1042911 | 1042912)
}
