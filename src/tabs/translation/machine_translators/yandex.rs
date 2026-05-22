/*
FILE OVERVIEW: src/tabs/translation/machine_translators/yandex.rs
Yandex Translate backend implementation adapted from translatepy behavior.

Main items:
- `YandexMtBackend`: backend object implementing shared translator contract.
- `YandexApiClient`: low-level HTTP calls (`detect` + `translate` endpoints).
- `cached_ucid`: Yandex UCID generator with 360s TTL (translatepy-compatible).

Behavior notes:
- If source language is `auto`, language detection is executed for each text.
- Translate endpoint is called with Android-like params and User-Agent.
- Error codes are mapped to readable identifiers close to translatepy.
*/

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use ureq::Agent;

use super::MachineTranslatorBackend;

const API_BASE: &str = "https://translate.yandex.net/api/v1/tr.json";
const DETECT_HINT: &str = "en";
const UCID_TTL: Duration = Duration::from_secs(360);

#[derive(Debug, Default, Clone, Copy)]
pub struct YandexMtBackend;

impl MachineTranslatorBackend for YandexMtBackend {
    fn translate_texts(
        &self,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, String>>, String> {
        let mut client = YandexApiClient::new();
        let mut out = Vec::with_capacity(texts.len());
        let normalized_source = normalize_yandex_lang(source_lang, true);
        let normalized_target = normalize_yandex_lang(target_lang, false);

        for text in texts {
            let source_text = text.trim();
            if source_text.is_empty() {
                out.push(Ok(String::new()));
                continue;
            }

            let effective_source = if normalized_source == "auto" {
                match client.detect_language(source_text) {
                    Ok(lang) => lang,
                    Err(err) => {
                        out.push(Err(err));
                        continue;
                    }
                }
            } else {
                normalized_source.clone()
            };

            let lang_pair = format!("{effective_source}-{normalized_target}");
            match client.translate(source_text, &lang_pair) {
                Ok(translated) => out.push(Ok(translated)),
                Err(err) => out.push(Err(err)),
            }
        }

        Ok(out)
    }
}

struct YandexApiClient {
    agent: Agent,
}

impl YandexApiClient {
    fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .user_agent("ru.yandex.translate/3.20.2024")
            .build();
        Self { agent }
    }

    fn detect_language(&mut self, text: &str) -> Result<String, String> {
        let ucid = cached_ucid();
        let url = format!("{API_BASE}/detect");
        let response = self
            .agent
            .get(&url)
            .query("ucid", &ucid)
            .query("srv", "android")
            .query("text", text)
            .query("hint", DETECT_HINT)
            .call();

        let (status, body) = read_response(response)?;
        let parsed: YandexDetectResponse = serde_json::from_str(&body)
            .map_err(|err| format!("Yandex detect: некорректный JSON: {err}"))?;
        if status < 400 && parsed.code == 200 {
            let lang = parsed
                .lang
                .as_deref()
                .unwrap_or("auto")
                .trim()
                .to_ascii_lowercase();
            if lang.is_empty() {
                Ok("auto".to_string())
            } else {
                Ok(normalize_detected_language(&lang))
            }
        } else {
            Err(format_yandex_api_error(parsed.code, status, &body))
        }
    }

    fn translate(&mut self, text: &str, lang_pair: &str) -> Result<String, String> {
        let ucid = cached_ucid();
        let url = format!("{API_BASE}/translate");
        let response = self
            .agent
            .post(&url)
            .query("ucid", &ucid)
            .query("srv", "android")
            .query("format", "text")
            .send_form(&[("text", text), ("lang", lang_pair)]);

        let (status, body) = read_response(response)?;
        let parsed: YandexTranslateResponse = serde_json::from_str(&body)
            .map_err(|err| format!("Yandex translate: некорректный JSON: {err}"))?;
        if status < 400 && parsed.code == 200 {
            parsed
                .text
                .and_then(|mut values| {
                    if values.is_empty() {
                        None
                    } else {
                        Some(values.remove(0))
                    }
                })
                .ok_or_else(|| "Yandex translate: пустой ответ.".to_string())
        } else {
            Err(format_yandex_api_error(parsed.code, status, &body))
        }
    }
}

fn read_response(response: Result<ureq::Response, ureq::Error>) -> Result<(u16, String), String> {
    match response {
        Ok(resp) => {
            let status = resp.status();
            let body = resp
                .into_string()
                .map_err(|err| format!("Yandex: ошибка чтения тела ответа: {err}"))?;
            Ok((status, body))
        }
        Err(ureq::Error::Status(status, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Ok((status, body))
        }
        Err(ureq::Error::Transport(err)) => Err(format!("Yandex: ошибка сети: {err}")),
    }
}

fn normalize_yandex_lang(raw: &str, source: bool) -> String {
    let fallback = if source { "auto" } else { "ru" };
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return fallback.to_string();
    }

    match trimmed.as_str() {
        "zh-cn" | "zh-tw" => "zh".to_string(),
        _ => trimmed,
    }
}

fn normalize_detected_language(lang: &str) -> String {
    let first = lang
        .split('-')
        .next()
        .unwrap_or(lang)
        .trim()
        .to_ascii_lowercase();
    if first.is_empty() {
        "auto".to_string()
    } else {
        first
    }
}

fn format_yandex_api_error(code: i32, status: u16, body: &str) -> String {
    let code_text = yandex_error_code_text(code);
    if body.trim().is_empty() {
        format!("Yandex API error {code} ({code_text}), HTTP {status}.")
    } else {
        let compact = body.trim().replace('\n', " ");
        format!("Yandex API error {code} ({code_text}), HTTP {status}: {compact}")
    }
}

fn yandex_error_code_text(code: i32) -> &'static str {
    match code {
        401 => "ERR_KEY_INVALID",
        402 => "ERR_KEY_BLOCKED",
        403 => "ERR_DAILY_REQ_LIMIT_EXCEEDED",
        404 => "ERR_DAILY_CHAR_LIMIT_EXCEEDED",
        408 => "ERR_MONTHLY_CHAR_LIMIT_EXCEEDED",
        413 => "ERR_TEXT_TOO_LONG",
        422 => "ERR_UNPROCESSABLE_TEXT",
        501 => "ERR_LANG_NOT_SUPPORTED",
        503 => "ERR_SERVICE_NOT_AVAILABLE",
        _ => "ERR_UNKNOWN",
    }
}

#[derive(Debug)]
struct UcidCache {
    value: String,
    generated_at: Instant,
}

static UCID_COUNTER: AtomicU64 = AtomicU64::new(1);
static UCID_CACHE: OnceLock<Mutex<Option<UcidCache>>> = OnceLock::new();

fn cached_ucid() -> String {
    let cache = UCID_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(_) => return generate_ucid(),
    };
    if let Some(existing) = guard.as_ref()
        && existing.generated_at.elapsed() < UCID_TTL
    {
        return existing.value.clone();
    }
    let value = generate_ucid();
    *guard = Some(UcidCache {
        value: value.clone(),
        generated_at: Instant::now(),
    });
    value
}

fn generate_ucid() -> String {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let ctr = UCID_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    format!("{ns:016x}{pid:08x}{ctr:08x}")
}

#[derive(Debug, Deserialize)]
struct YandexDetectResponse {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Debug, Deserialize)]
struct YandexTranslateResponse {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    text: Option<Vec<String>>,
}
