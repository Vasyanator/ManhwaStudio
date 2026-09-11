/*
FILE OVERVIEW: src/tabs/translation/machine_translators/google.rs
Google Translate backend implementation for Translation tab MT worker.

Main items:
- `GoogleMtBackend`: backend object implementing shared translator contract.
- `normalize_google_lang`: source/target language normalization.

Behavior:
- Uses Rust crate `translators` (`GoogleTranslator`) synchronously on native.
- Returns per-item result list (`Ok` translated text or `Err` per bubble).
- The `translators` crate (native HTTP) is unavailable on the web build, so the
  wasm target implements the same contract with a body that returns a clear
  "unavailable on web" error for every item instead of a fake translation.
*/

use super::MachineTranslatorBackend;
#[cfg(not(target_arch = "wasm32"))]
use translators::{GoogleTranslator, Translator};

#[derive(Debug, Default, Clone, Copy)]
pub struct GoogleMtBackend;

#[cfg(not(target_arch = "wasm32"))]
impl MachineTranslatorBackend for GoogleMtBackend {
    fn translate_texts(
        &self,
        source_lang: &str,
        target_lang: &str,
        texts: Vec<String>,
    ) -> Result<Vec<Result<String, String>>, String> {
        let source = normalize_google_lang(source_lang, true);
        let target = normalize_google_lang(target_lang, false);
        let translator = GoogleTranslator::default();

        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let source_text = text.trim();
            if source_text.is_empty() {
                out.push(Ok(String::new()));
                continue;
            }
            match translator.translate_sync(source_text, &source, &target) {
                Ok(translated) => out.push(Ok(translated)),
                Err(err) => out.push(Err(err.to_string())),
            }
        }
        Ok(out)
    }
}

/// Web stub: Google Translate uses the native `translators` crate, which is not
/// available in the browser build. The whole batch fails with a clear message so
/// no fake translation is ever produced.
#[cfg(target_arch = "wasm32")]
impl MachineTranslatorBackend for GoogleMtBackend {
    fn translate_texts(
        &self,
        _source_lang: &str,
        _target_lang: &str,
        _texts: Vec<String>,
    ) -> Result<Vec<Result<String, String>>, String> {
        Err(t!("translation.mt.google.web_unavailable_error").to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn normalize_google_lang(raw: &str, source: bool) -> String {
    let fallback = if source { "auto" } else { "ru" };
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    }
}
