/*
File: src/widgets/spellchecked_line.rs

Purpose:
Многострочный `egui::TextEdit` с фоновой проверкой орфографии по Hunspell-
совместимым словарям и подчёркиванием некорректных слов.

Main responsibilities:
- оборачивать стандартный `TextEdit` без блокировки GUI-потока;
- загружать все словари из папки `spell_check` и автоматически докачивать
  словарь ДЛЯ ЯЗЫКА ВЁРСТКИ (`ms_text_util::language::text_language`) при его
  отсутствии, по одному разу на язык;
- кэшировать проверки слов и отправлять новые слова в background worker;
- подчёркивать слова с ошибками через custom layouter.

Key structures:
- `SpellcheckedTextEdit`
- `SpellcheckService`
- `DictionarySpec` / `dictionary_spec` — таблица «язык вёрстки → словарь»
  (on-disk stem + verified `.aff`/`.dic` URL'ы).

Notes:
- Проверка использует pure-Rust crate `zspell`, совместимый с Hunspell
  словарями `.aff` + `.dic`.
- Словари читаются из app-local папки `spell_check`; GUI-поток не делает
  файловых и сетевых операций.
- Активный словарь следует за ЯЗЫКОМ ВЁРСТКИ (как переносы и покрытие шрифта), а
  не за языком интерфейса. Воркер сравнивает `text_language()` каждый батч
  (паттерн `panel/facade.rs`) и докачивает словарь нового языка. Скачивание
  идёт по одному разу на язык (`download_attempted: HashSet<TextLanguage>`),
  чтобы неудача одного языка не блокировала другой.
- Сопоставление по СЛОВУ: language-first, script-second. Слово в письменности
  активного языка судит ТОЛЬКО словарь этого языка (иначе оставшийся на диске
  `uk_UA` молча принял бы украинское написание в русской главе). Слово другой
  письменности судит любой словарь этой письменности — так работает смешанный
  текст (кириллическое имя в испанской главе). Если словаря активного языка нет,
  его слова остаются НЕ подчёркнутыми, а не судятся соседним словарём.
- Кэш проверок ключуется языком вёрстки (`SpellCacheKey.language`) и полностью
  очищается при смене набора загруженных словарей, чтобы вердикт, вынесенный при
  одном языке, не переживал переключение.
- Пользовательские исключения объединяют app-global `custom.dic` и project-local
  список из `settings.json`.
*/

use crate::language::{TextLanguage, text_language};
use crate::runtime_log;
use egui::epaint::text::{LayoutJob, TextFormat};
use egui::text_edit::TextEditOutput;
use egui::{Align, Color32, Id, Response, Stroke, TextBuffer, TextEdit, Ui, Widget};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hash;
// `Read::read_to_string` is only used by the native dictionary downloader below.
use ms_thread as thread;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use web_time::Duration;
use zspell::Dictionary;

const CUSTOM_DICTIONARY_STEM: &str = "custom";
const CUSTOM_AFF_CONTENT: &str = "SET UTF-8\n";
const PROJECT_CUSTOM_WORDS_KEY: &str = "project_custom_spellcheck_words";

static SPELLCHECK_SERVICE_INSTANCE: OnceLock<SpellcheckService> = OnceLock::new();
static PROJECT_SPELLCHECK_SETTINGS_FILE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static SPELLCHECK_CUSTOM_WORDS_SERVICE: OnceLock<SpellcheckCustomWordsService> = OnceLock::new();
static SPELLCHECK_WORDS_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
enum ScriptGroup {
    Latin,
    Cyrillic,
}

/// Provenance of one language's Hunspell dictionary.
///
/// `stem` is OUR on-disk file stem (`<stem>.aff` + `<stem>.dic`), chosen by this
/// module and independent of the upstream filename. It must keep
/// `infer_script_group` correct (a stem typo would silently disable checking for
/// that language); the unit tests enforce this. `aff_url`/`dic_url` are the exact
/// verified upstream sources (200 for both, declared `SET UTF-8`).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct DictionarySpec {
    stem: &'static str,
    aff_url: &'static str,
    dic_url: &'static str,
}

// Upstream dictionary bases. Two are needed:
// - `LibreOffice/dictionaries` is the primary source (UTF-8, official).
// - `wooorm/dictionaries` is used ONLY for `fr`, `pl`, and `sl`, and must not be
//   "fixed" back to LibreOffice:
//     * `fr`: the LibreOffice dictionaries repo has NO `fr_FR` directory at all
//       (French ships as a separate extension), so there is nothing to point at.
//     * `pl` / `sl`: LibreOffice's `pl_PL.aff` / `sl_SI.aff` declare
//       `SET ISO8859-2`. This module reads dictionary bodies with
//       `read_to_string`, which rejects non-UTF-8 bytes, so those files cannot be
//       loaded as-is. The wooorm copies declare `SET UTF-8`.
// `concat!` needs literal arguments, so the base is inlined per URL rather than
// referenced through a `const`.

/// Returns the dictionary provenance for `language`. Pure and total: an
/// exhaustive `match` maps every `TextLanguage` to a spec with a unique on-disk
/// stem and verified https `.aff`/`.dic` URLs. Never panics. The unit tests
/// assert totality, stem uniqueness, URL shape, and stem→script agreement.
fn dictionary_spec(language: TextLanguage) -> DictionarySpec {
    // LibreOffice-hosted entry: `path` is the repo-relative path without extension.
    macro_rules! lo {
        ($stem:literal, $path:literal) => {
            DictionarySpec {
                stem: $stem,
                aff_url: concat!(
                    "https://raw.githubusercontent.com/LibreOffice/dictionaries/master/",
                    $path,
                    ".aff"
                ),
                dic_url: concat!(
                    "https://raw.githubusercontent.com/LibreOffice/dictionaries/master/",
                    $path,
                    ".dic"
                ),
            }
        };
    }
    // wooorm-hosted entry (see the two-source rationale above).
    macro_rules! wm {
        ($stem:literal, $path:literal) => {
            DictionarySpec {
                stem: $stem,
                aff_url: concat!(
                    "https://raw.githubusercontent.com/wooorm/dictionaries/main/dictionaries/",
                    $path,
                    ".aff"
                ),
                dic_url: concat!(
                    "https://raw.githubusercontent.com/wooorm/dictionaries/main/dictionaries/",
                    $path,
                    ".dic"
                ),
            }
        };
    }
    match language {
        // `ru_RU` stem is kept exactly as-is so existing installs are not re-downloaded.
        TextLanguage::Ru => lo!("ru_RU", "ru_RU/ru_RU"),
        TextLanguage::Uk => lo!("uk_UA", "uk_UA/uk_UA"),
        TextLanguage::Be => lo!("be_BY", "be_BY/be-official"),
        TextLanguage::Sr => lo!("sr_RS", "sr/sr"),
        TextLanguage::Pl => wm!("pl_PL", "pl/index"),
        TextLanguage::Cs => lo!("cs_CZ", "cs_CZ/cs_CZ"),
        TextLanguage::Sk => lo!("sk_SK", "sk_SK/sk_SK"),
        TextLanguage::Sl => wm!("sl_SI", "sl/index"),
        TextLanguage::Hr => lo!("hr_HR", "hr_HR/hr_HR"),
        TextLanguage::Es => lo!("es_ES", "es/es_ES"),
        TextLanguage::Fr => wm!("fr_FR", "fr/index"),
        TextLanguage::Pt => lo!("pt_PT", "pt_PT/pt_PT"),
        TextLanguage::En => lo!("en_US", "en/en_US"),
    }
}

/// Per-word spellcheck cache key.
///
/// Carries the active typesetting `language` in addition to the script `group`
/// and the lowercased `word`. The language field is the cache-invalidation
/// contract: a verdict computed while one language was active is stored under a
/// distinct key from the same word under another language, so switching the
/// typesetting language can never surface a stale verdict (e.g. a word judged as
/// Russian keeping its verdict after a switch to Ukrainian). Per-word *matching*
/// is language-first, script-second (see `evaluate_word`); the language partitions the
/// cache.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct SpellCacheKey {
    language: TextLanguage,
    group: ScriptGroup,
    word: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SpellStatus {
    Pending,
    Correct,
    Incorrect,
    Unsupported,
}

#[derive(Debug, Clone)]
struct SpellRequest(SpellCacheKey);

#[derive(Clone)]
struct SpellcheckService {
    tx: Sender<Vec<SpellRequest>>,
    cache: Arc<Mutex<HashMap<SpellCacheKey, SpellStatus>>>,
}

impl SpellcheckService {
    fn global() -> &'static Self {
        SPELLCHECK_SERVICE_INSTANCE.get_or_init(Self::spawn)
    }

    fn spawn() -> Self {
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel();
        let worker_cache = Arc::clone(&cache);
        let _ = thread::Builder::new()
            .name("spellcheck-worker".to_string())
            .spawn(move || spellcheck_worker_loop(worker_cache, rx));
        Self { tx, cache }
    }
}

#[derive(Debug, Clone, Copy)]
enum CustomWordsTarget {
    Global,
    Project,
}

#[derive(Debug, Clone)]
struct SpellcheckCustomWordRequest {
    word: String,
    target: CustomWordsTarget,
}

#[derive(Clone)]
struct SpellcheckCustomWordsService {
    tx: Sender<SpellcheckCustomWordRequest>,
}

impl SpellcheckCustomWordsService {
    fn global() -> &'static Self {
        SPELLCHECK_CUSTOM_WORDS_SERVICE.get_or_init(Self::spawn)
    }

    fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("spellcheck-custom-words-worker".to_string())
            .spawn(move || spellcheck_custom_words_worker_loop(rx));
        Self { tx }
    }

    fn enqueue(&self, request: SpellcheckCustomWordRequest) -> Result<(), String> {
        self.tx
            .send(request)
            .map_err(|_| "spellcheck custom words worker is unavailable".to_string())
    }
}

#[derive(Debug)]
struct DictionaryBundle {
    /// On-disk file stem (`ru_RU`, `es_ES`, …). Identifies which language this
    /// bundle belongs to, so `evaluate_word` can prefer the active language's
    /// dictionary over another dictionary of the same script.
    stem: String,
    group: ScriptGroup,
    dictionary: Dictionary,
}

#[derive(Debug)]
struct DictionaryFiles {
    stem: String,
    aff_path: PathBuf,
    dic_path: PathBuf,
}

#[derive(Debug)]
struct SpellcheckWorkerState {
    root_dir: PathBuf,
    project_settings_file: Option<PathBuf>,
    bundles: Vec<DictionaryBundle>,
    custom_words: HashMap<ScriptGroup, HashSet<String>>,
    loaded_signature: Vec<String>,
    /// Languages whose dictionary download has already been attempted this
    /// process. Per-language (not a single boolean) so a failed download of one
    /// language never blocks a later attempt for another.
    download_attempted: HashSet<TextLanguage>,
    /// Typesetting language the worker last ensured a dictionary for. `None`
    /// until the first batch; a change drives an ensure/download of the new
    /// language's dictionary.
    active_language: Option<TextLanguage>,
}

impl SpellcheckWorkerState {
    fn new() -> Self {
        Self {
            root_dir: resolve_spellcheck_dir(),
            project_settings_file: current_project_spellcheck_settings_file(),
            bundles: Vec::new(),
            custom_words: HashMap::new(),
            loaded_signature: Vec::new(),
            download_attempted: HashSet::new(),
            active_language: None,
        }
    }
}

pub struct SpellcheckedTextEdit<'a> {
    text: &'a mut String,
    hint_text: String,
    id: Option<Id>,
    desired_width: Option<f32>,
    desired_rows: usize,
    horizontal_align: Align,
    vertical_align: Align,
    spellcheck_enabled: bool,
}

impl<'a> SpellcheckedTextEdit<'a> {
    #[must_use]
    pub fn multiline(text: &'a mut String) -> Self {
        Self {
            text,
            hint_text: String::new(),
            id: None,
            desired_width: None,
            desired_rows: 1,
            horizontal_align: Align::LEFT,
            vertical_align: Align::TOP,
            spellcheck_enabled: true,
        }
    }

    #[must_use]
    pub fn id(mut self, id: Id) -> Self {
        self.id = Some(id);
        self
    }

    #[must_use]
    pub fn id_salt(mut self, salt: impl Hash + std::fmt::Debug) -> Self {
        self.id = Some(Id::new(salt));
        self
    }

    #[must_use]
    pub fn hint_text(mut self, hint_text: impl Into<String>) -> Self {
        self.hint_text = hint_text.into();
        self
    }

    #[must_use]
    pub fn desired_width(mut self, desired_width: f32) -> Self {
        self.desired_width = Some(desired_width);
        self
    }

    #[must_use]
    pub fn desired_rows(mut self, desired_rows: usize) -> Self {
        self.desired_rows = desired_rows;
        self
    }

    #[must_use]
    pub fn horizontal_align(mut self, align: Align) -> Self {
        self.horizontal_align = align;
        self
    }

    #[must_use]
    pub fn vertical_align(mut self, align: Align) -> Self {
        self.vertical_align = align;
        self
    }

    #[must_use]
    pub fn spellcheck_enabled(mut self, enabled: bool) -> Self {
        self.spellcheck_enabled = enabled;
        self
    }

    pub fn show(self, ui: &mut Ui) -> TextEditOutput {
        let spellcheck_enabled = self.spellcheck_enabled;
        let mut layouter = move |ui: &Ui, buffer: &dyn TextBuffer, wrap_width: f32| {
            build_spellcheck_galley(ui, buffer.as_str(), wrap_width, spellcheck_enabled)
        };

        let mut edit = TextEdit::multiline(self.text)
            .hint_text(self.hint_text)
            .desired_rows(self.desired_rows)
            .horizontal_align(self.horizontal_align)
            .vertical_align(self.vertical_align);
        if let Some(id) = self.id {
            edit = edit.id(id);
        }
        if let Some(width) = self.desired_width {
            edit = edit.desired_width(width);
        }
        edit.layouter(&mut layouter).show(ui)
    }
}

impl Widget for SpellcheckedTextEdit<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        // egui 0.35: `TextEditOutput::response` is an `AtomLayoutResponse`; expose its
        // inner `Response` as the widget response.
        self.show(ui).response.response
    }
}

fn build_spellcheck_galley(
    ui: &Ui,
    text: &str,
    wrap_width: f32,
    spellcheck_enabled: bool,
) -> Arc<egui::Galley> {
    let font_id = ui
        .style()
        .override_font_id
        .clone()
        .unwrap_or_else(|| egui::FontSelection::Default.resolve(ui.style()));
    let text_color = ui.visuals().text_color();
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    job.halign = Align::LEFT;
    job.text = text.to_string();

    let default_format = TextFormat::simple(font_id.clone(), text_color);
    let misspelled_format = TextFormat {
        underline: Stroke::new(1.5, Color32::from_rgb(220, 70, 70)),
        ..default_format.clone()
    };

    if !spellcheck_enabled || text.is_empty() {
        push_section(&mut job, 0..text.len(), default_format);
        return ui.fonts_mut(|fonts| fonts.layout_job(job));
    }

    let tokens = collect_word_tokens(text);
    let statuses = statuses_for_tokens(ui, &tokens);
    let mut cursor = 0usize;
    for (token, status) in tokens.iter().zip(statuses) {
        if cursor < token.range.start {
            push_section(&mut job, cursor..token.range.start, default_format.clone());
        }
        let format = if status == SpellStatus::Incorrect {
            misspelled_format.clone()
        } else {
            default_format.clone()
        };
        push_section(&mut job, token.range.clone(), format);
        cursor = token.range.end;
    }
    if cursor < text.len() {
        push_section(&mut job, cursor..text.len(), default_format);
    }

    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

pub fn misspelled_word_at_pointer(ui: &Ui, output: &TextEditOutput, text: &str) -> Option<String> {
    let pointer_pos = output.response.interact_pointer_pos()?;
    let local_pos = pointer_pos - output.galley_pos;
    let cursor = output.galley.cursor_from_pos(local_pos);
    // epaint 0.35 `CCursor::index` is a `CharIndex`; take the inner character count.
    let byte_index = char_index_to_byte_index(text, cursor.index.0);
    let tokens = collect_word_tokens(text);
    let statuses = statuses_for_tokens(ui, &tokens);
    let token_index = token_index_at_byte(&tokens, byte_index)?;
    (statuses.get(token_index) == Some(&SpellStatus::Incorrect))
        .then(|| text[tokens[token_index].range.clone()].trim().to_string())
}

fn queue_custom_word_addition(word: &str, target: CustomWordsTarget) {
    let request = SpellcheckCustomWordRequest {
        word: word.to_string(),
        target,
    };
    if let Err(err) = SpellcheckCustomWordsService::global().enqueue(request) {
        runtime_log::log_error(format!(
            "[widgets::spellchecked_line] failed to queue custom spellcheck word '{word}'; error={err}"
        ));
    }
}

pub fn queue_word_to_global_exceptions(word: &str) {
    queue_custom_word_addition(word, CustomWordsTarget::Global);
}

pub fn queue_word_to_project_exceptions(word: &str) {
    queue_custom_word_addition(word, CustomWordsTarget::Project);
}

fn push_section(job: &mut LayoutJob, range: Range<usize>, format: TextFormat) {
    if range.is_empty() {
        return;
    }
    job.sections.push(egui::text::LayoutSection {
        leading_space: 0.0,
        // epaint 0.35 types layout ranges as `Range<ByteIndex>`; wrap the byte offsets.
        byte_range: egui::text::ByteIndex(range.start)..egui::text::ByteIndex(range.end),
        format,
    });
}

#[derive(Debug, Clone)]
struct WordToken {
    range: Range<usize>,
    key: Option<SpellCacheKey>,
}

fn collect_word_tokens(text: &str) -> Vec<WordToken> {
    let mut tokens = Vec::new();
    let mut current_start: Option<usize> = None;
    let mut current_end = 0usize;

    for (idx, ch) in text.char_indices() {
        if is_word_char(ch) {
            if current_start.is_none() {
                current_start = Some(idx);
            }
            current_end = idx + ch.len_utf8();
            continue;
        }

        if let Some(start) = current_start.take() {
            let range = start..current_end;
            tokens.push(WordToken {
                key: build_cache_key(&text[range.clone()]),
                range,
            });
        }
    }

    if let Some(start) = current_start {
        let range = start..text.len();
        tokens.push(WordToken {
            key: build_cache_key(&text[range.clone()]),
            range,
        });
    }

    tokens
}

fn char_index_to_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .map(|(byte_index, _)| byte_index)
        .nth(char_index)
        .unwrap_or(text.len())
}

fn token_index_at_byte(tokens: &[WordToken], byte_index: usize) -> Option<usize> {
    tokens.iter().position(|token| {
        token.range.contains(&byte_index)
            || (byte_index == token.range.end && byte_index > token.range.start)
    })
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphabetic() || ch == '\'' || ch == '-' || ch == '’'
}

/// Classifies a raw token into its script group and lowercased form, or `None`
/// when it should not be spellchecked (too short, contains digits, an all-caps
/// acronym, or mixed/unknown script). Pure and language-independent — the script
/// classification and normalization do not depend on the active language.
fn classify_and_normalize(raw: &str) -> Option<(ScriptGroup, String)> {
    let trimmed = raw.trim_matches(|ch: char| ch == '\'' || ch == '’' || ch == '-');
    if trimmed.chars().count() < 2 {
        return None;
    }
    if trimmed.chars().any(char::is_numeric) {
        return None;
    }

    let group = classify_word(trimmed)?;
    let lowered = trimmed.to_lowercase();
    if lowered.chars().all(|ch| !ch.is_alphabetic()) {
        return None;
    }
    if trimmed.chars().all(|ch| ch.is_uppercase()) && trimmed.chars().count() <= 5 {
        return None;
    }

    Some((group, lowered))
}

/// Builds the per-word cache key for `language`. Pure and total; the returned key
/// carries `language` so a verdict cached under one typesetting language is never
/// reused after the user switches languages. Returns `None` for tokens that are
/// not spellchecked (see `classify_and_normalize`).
fn build_cache_key_for(raw: &str, language: TextLanguage) -> Option<SpellCacheKey> {
    classify_and_normalize(raw).map(|(group, word)| SpellCacheKey {
        language,
        group,
        word,
    })
}

/// Cache key for the current process-global typesetting language. Cheap (one
/// atomic load); safe on the GUI thread.
fn build_cache_key(raw: &str) -> Option<SpellCacheKey> {
    build_cache_key_for(raw, text_language())
}

fn classify_word(word: &str) -> Option<ScriptGroup> {
    let mut saw_latin = false;
    let mut saw_cyrillic = false;
    for ch in word.chars() {
        if ch == '\'' || ch == '’' || ch == '-' {
            continue;
        }
        if ch.is_ascii_alphabetic() {
            saw_latin = true;
            continue;
        }
        if ('\u{0400}'..='\u{04FF}').contains(&ch) || ('\u{0500}'..='\u{052F}').contains(&ch) {
            saw_cyrillic = true;
            continue;
        }
        return None;
    }
    match (saw_latin, saw_cyrillic) {
        (true, false) => Some(ScriptGroup::Latin),
        (false, true) => Some(ScriptGroup::Cyrillic),
        _ => None,
    }
}

fn statuses_for_tokens(ui: &Ui, tokens: &[WordToken]) -> Vec<SpellStatus> {
    let service = SpellcheckService::global();
    let mut requests = Vec::new();
    let mut statuses = Vec::with_capacity(tokens.len());

    {
        let mut cache = match service.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for token in tokens {
            let Some(key) = token.key.as_ref() else {
                statuses.push(SpellStatus::Unsupported);
                continue;
            };
            let status = cache
                .entry(key.clone())
                .or_insert_with(|| {
                    requests.push(SpellRequest(key.clone()));
                    SpellStatus::Pending
                })
                .to_owned();
            statuses.push(status);
        }
    }

    if !requests.is_empty() {
        if service.tx.send(requests).is_err() {
            runtime_log::log_warn("[widgets::spellchecked_line] failed to queue spellcheck batch");
        }
        ui.ctx().request_repaint_after(Duration::from_millis(120));
    } else if statuses.contains(&SpellStatus::Pending) {
        ui.ctx().request_repaint_after(Duration::from_millis(120));
    }

    statuses
}

fn spellcheck_worker_loop(
    cache: Arc<Mutex<HashMap<SpellCacheKey, SpellStatus>>>,
    rx: Receiver<Vec<SpellRequest>>,
) {
    let mut state = SpellcheckWorkerState::new();
    while let Ok(mut batch) = rx.recv() {
        while let Ok(mut extra) = rx.try_recv() {
            batch.append(&mut extra);
        }
        process_spellcheck_batch(&cache, &mut state, batch);
    }
}

fn spellcheck_custom_words_worker_loop(rx: Receiver<SpellcheckCustomWordRequest>) {
    while let Ok(mut latest) = rx.recv() {
        while let Ok(next) = rx.try_recv() {
            latest = next;
        }
        if let Err(err) = persist_custom_word_request(&latest) {
            runtime_log::log_error(format!(
                "[widgets::spellchecked_line] failed to persist custom spellcheck word '{}'; error={err}",
                latest.word
            ));
        }
    }
}

fn persist_custom_word_request(request: &SpellcheckCustomWordRequest) -> Result<(), String> {
    match request.target {
        CustomWordsTarget::Global => {
            let mut words = load_custom_spellcheck_words()?;
            append_custom_word(&mut words, &request.word);
            save_custom_spellcheck_words(&words)
        }
        CustomWordsTarget::Project => {
            let Some(settings_file) = current_project_spellcheck_settings_file() else {
                return Err("project settings file is not bound".to_string());
            };
            let mut words = load_project_spellcheck_words(&settings_file)?;
            append_custom_word(&mut words, &request.word);
            save_project_spellcheck_words(&settings_file, &words)
        }
    }
}

fn process_spellcheck_batch(
    cache: &Arc<Mutex<HashMap<SpellCacheKey, SpellStatus>>>,
    state: &mut SpellcheckWorkerState,
    batch: Vec<SpellRequest>,
) {
    refresh_dictionaries(cache, state);

    // Evaluate every request outside the cache lock (dictionary checks are hashmap
    // lookups but there is no reason to hold the lock across them), then insert the
    // finished verdicts under a short critical section. Each key keeps its original
    // language, so the cache stays partitioned by typesetting language.
    let results: Vec<(SpellCacheKey, SpellStatus)> = batch
        .into_iter()
        .map(|SpellRequest(key)| {
            let status = evaluate_word(state, key.group, &key.word);
            (key, status)
        })
        .collect();

    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    for (key, status) in results {
        guard.insert(key, status);
    }
}

/// Judges a single lowercased `word` of script `group` against the currently
/// loaded state.
///
/// Dictionary choice is language-first, script-second:
/// - A word written in the ACTIVE typesetting language's own script is judged
///   ONLY by that language's dictionary. Dictionaries of other languages sharing
///   the script (left on disk from a previous selection) must not vote, or a
///   Ukrainian spelling would silently pass inside a Russian chapter.
/// - A word of the OTHER script is judged by any loaded dictionary of that script.
///   This keeps mixed-script text working (a Cyrillic name inside a Spanish
///   chapter), where being lenient is the right trade-off.
///
/// Returns `Unsupported` (word left unmarked, never flagged) when no applicable
/// dictionary is loaded — including when the active language's own dictionary is
/// missing because its download failed. Judging Ukrainian text against a Russian
/// dictionary would flag nearly every word, so silence is the honest degradation.
fn evaluate_word(state: &SpellcheckWorkerState, group: ScriptGroup, word: &str) -> SpellStatus {
    if state
        .custom_words
        .get(&group)
        .is_some_and(|custom_words| custom_words.contains(word))
    {
        return SpellStatus::Correct;
    }

    // Stem of the active language's dictionary, when the word shares its script.
    let active_stem = state
        .active_language
        .map(dictionary_spec)
        .filter(|spec| infer_script_group(spec.stem) == Some(group))
        .map(|spec| spec.stem);

    let mut saw_applicable_dictionary = false;
    for bundle in state.bundles.iter().filter(|bundle| bundle.group == group) {
        // Same-script bundles that are not the active language's dictionary do not
        // vote; other-script bundles all vote (mixed-script fallback).
        if active_stem.is_some_and(|stem| bundle.stem != stem) {
            continue;
        }
        saw_applicable_dictionary = true;
        if bundle.dictionary.check_word(word) {
            return SpellStatus::Correct;
        }
    }
    if saw_applicable_dictionary {
        SpellStatus::Incorrect
    } else {
        SpellStatus::Unsupported
    }
}

fn refresh_dictionaries(
    cache: &Arc<Mutex<HashMap<SpellCacheKey, SpellStatus>>>,
    state: &mut SpellcheckWorkerState,
) {
    state.project_settings_file = current_project_spellcheck_settings_file();
    if let Err(err) = fs::create_dir_all(&state.root_dir) {
        runtime_log::log_warn(format!(
            "[widgets::spellchecked_line] failed to create spell_check dir '{}': {err}",
            state.root_dir.display()
        ));
        return;
    }

    // The spellcheck dictionary follows the TYPESETTING language (as hyphenation and
    // font-coverage do), not the UI language. Ensure the active language's dictionary
    // is present, downloading it at most once per language. Per-word matching is
    // language-first below, so already-downloaded dictionaries of another script keep
    // working for mixed text.
    let current_language = text_language();
    state.active_language = Some(current_language);
    let spec = dictionary_spec(current_language);

    let mut files = discover_dictionary_files(&state.root_dir);
    let dictionary_missing = !contains_stem(&files, spec.stem);
    if dictionary_missing && state.download_attempted.insert(current_language) {
        if let Err(err) = download_dictionary(&state.root_dir, spec) {
            runtime_log::log_warn(format!(
                "[widgets::spellchecked_line] failed to download '{}' dictionary for language '{}': {err}",
                spec.stem,
                current_language.tag()
            ));
        }
        files = discover_dictionary_files(&state.root_dir);
    }

    let signature = dictionary_signature(&files, state.project_settings_file.as_deref());
    if signature == state.loaded_signature {
        return;
    }

    let bundles = load_dictionary_bundles(&files);
    let custom_words = load_custom_dictionary_words_from_sources(
        &state.root_dir,
        state.project_settings_file.as_deref(),
    );
    if bundles.is_empty() {
        runtime_log::log_warn(format!(
            "[widgets::spellchecked_line] no Hunspell dictionaries loaded from '{}'",
            state.root_dir.display()
        ));
    } else {
        runtime_log::log_info(format!(
            "[widgets::spellchecked_line] loaded {} spellcheck dictionaries from '{}'",
            bundles.len(),
            state.root_dir.display()
        ));
    }

    state.loaded_signature = signature;
    state.bundles = bundles;
    state.custom_words = custom_words;

    // The loaded dictionary set changed (a language was downloaded, or a file on
    // disk changed). Per-word matching is cumulative across every same-script
    // dictionary now present, so a previously-cached verdict may be wrong (e.g. a
    // word that was Incorrect before uk_UA was added may now be Correct). Clear the
    // whole cache so every visible word is re-evaluated against the new set.
    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clear();
}

fn discover_dictionary_files(root_dir: &Path) -> Vec<DictionaryFiles> {
    let Ok(entries) = fs::read_dir(root_dir) else {
        return Vec::new();
    };

    let mut aff_paths: HashMap<String, PathBuf> = HashMap::new();
    let mut dic_paths: HashMap<String, PathBuf> = HashMap::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if stem.eq_ignore_ascii_case(CUSTOM_DICTIONARY_STEM) {
            continue;
        }
        match ext.to_ascii_lowercase().as_str() {
            "aff" => {
                aff_paths.insert(stem.to_string(), path);
            }
            "dic" => {
                dic_paths.insert(stem.to_string(), path);
            }
            _ => {}
        }
    }

    let mut bundles = Vec::new();
    for (stem, aff_path) in aff_paths {
        if let Some(dic_path) = dic_paths.get(&stem) {
            bundles.push(DictionaryFiles {
                stem,
                aff_path,
                dic_path: dic_path.clone(),
            });
        }
    }
    bundles.sort_by(|lhs, rhs| lhs.stem.cmp(&rhs.stem));
    bundles
}

/// Whether a dictionary with the given on-disk `stem` is already present (either
/// bundled or previously downloaded). Case-insensitive to match filesystem
/// case-folding.
fn contains_stem(files: &[DictionaryFiles], stem: &str) -> bool {
    files
        .iter()
        .any(|files| files.stem.eq_ignore_ascii_case(stem))
}

fn dictionary_signature(
    files: &[DictionaryFiles],
    project_settings_file: Option<&Path>,
) -> Vec<String> {
    let mut signature: Vec<String> = files
        .iter()
        .map(|files| {
            let aff_meta = fs::metadata(&files.aff_path).ok();
            let dic_meta = fs::metadata(&files.dic_path).ok();
            let aff_len = aff_meta.as_ref().map_or(0, std::fs::Metadata::len);
            let dic_len = dic_meta.as_ref().map_or(0, std::fs::Metadata::len);
            let aff_modified = aff_meta
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_secs());
            let dic_modified = dic_meta
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_secs());
            format!(
                "{}|{}|{}|{aff_len}|{dic_len}|{aff_modified}|{dic_modified}",
                files.stem,
                files.aff_path.display(),
                files.dic_path.display()
            )
        })
        .collect();
    let custom_paths = custom_dictionary_paths();
    for path in [custom_paths.0, custom_paths.1] {
        if let Ok(meta) = fs::metadata(&path) {
            let modified = meta
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_secs());
            signature.push(format!(
                "custom|{}|{}|{modified}",
                path.display(),
                meta.len()
            ));
        }
    }
    if let Some(path) = project_settings_file {
        match fs::metadata(path) {
            Ok(meta) => {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_secs());
                signature.push(format!(
                    "project_custom|{}|{}|{modified}",
                    path.display(),
                    meta.len()
                ));
            }
            Err(_) => signature.push(format!("project_custom|{}|missing", path.display())),
        }
    }
    signature
}

fn load_dictionary_bundles(files: &[DictionaryFiles]) -> Vec<DictionaryBundle> {
    let mut bundles = Vec::new();
    for files in files {
        let Some(group) = infer_script_group(&files.stem) else {
            runtime_log::log_warn(format!(
                "[widgets::spellchecked_line] skipping dictionary '{}' with unsupported script inference",
                files.stem
            ));
            continue;
        };
        match build_dictionary(files) {
            Ok(dictionary) => bundles.push(DictionaryBundle {
                stem: files.stem.clone(),
                group,
                dictionary,
            }),
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[widgets::spellchecked_line] failed to load dictionary '{}': {err}",
                    files.stem
                ));
            }
        }
    }
    bundles
}

fn build_dictionary(files: &DictionaryFiles) -> Result<Dictionary, String> {
    let aff_content = fs::read_to_string(&files.aff_path)
        .map_err(|err| format!("failed to read aff '{}': {err}", files.aff_path.display()))?;
    let dic_content = fs::read_to_string(&files.dic_path)
        .map_err(|err| format!("failed to read dic '{}': {err}", files.dic_path.display()))?;
    zspell::builder()
        .config_str(&aff_content)
        .dict_str(&dic_content)
        .build()
        .map_err(|err| format!("zspell build failed: {err}"))
}

fn infer_script_group(stem: &str) -> Option<ScriptGroup> {
    let primary = stem
        .split(['_', '-'])
        .next()
        .map(|part| part.to_ascii_lowercase())?;
    if matches!(primary.as_str(), "ru" | "uk" | "be" | "bg" | "mk" | "sr") {
        return Some(ScriptGroup::Cyrillic);
    }
    if primary.chars().all(|ch| ch.is_ascii_lowercase()) {
        return Some(ScriptGroup::Latin);
    }
    None
}

/// Downloads a language's Hunspell dictionary described by `spec` into
/// `root_dir`, writing `<stem>.aff` and `<stem>.dic`.
///
/// # Errors
/// Returns an error string (no network, HTTP failure, or write failure) without
/// writing a partial/fake dictionary: both bodies are fetched before either file
/// is written. Runs only on the background spellcheck worker thread; on wasm the
/// download layer is unavailable and this returns an error.
fn download_dictionary(root_dir: &Path, spec: DictionarySpec) -> Result<(), String> {
    runtime_log::log_info(format!(
        "[widgets::spellchecked_line] downloading '{}' dictionary into '{}'",
        spec.stem,
        root_dir.display()
    ));

    // Fetch both bodies first so a mid-download failure never leaves a lone
    // `.aff`/`.dic` on disk that would look like a valid pair to discovery.
    let aff_content = download_text_file(spec.aff_url)?;
    let dic_content = download_text_file(spec.dic_url)?;
    write_text_file(&root_dir.join(format!("{}.aff", spec.stem)), &aff_content)?;
    write_text_file(&root_dir.join(format!("{}.dic", spec.stem)), &dic_content)?;
    Ok(())
}

/// Downloads a text file over HTTP into a `String`.
///
/// Native builds use `ureq`. On wasm there is no synchronous HTTP client here, so
/// dictionary download is unavailable and this returns an error instead of a fake
/// empty file.
#[cfg(not(target_arch = "wasm32"))]
fn download_text_file(url: &str) -> Result<String, String> {
    let response = ureq::get(url)
        .call()
        .map_err(|err| format!("request failed for '{url}': {err}"))?;
    let mut reader = response.into_reader();
    let mut body = String::new();
    reader
        .read_to_string(&mut body)
        .map_err(|err| format!("failed to read response body for '{url}': {err}"))?;
    Ok(body)
}

#[cfg(target_arch = "wasm32")]
fn download_text_file(_url: &str) -> Result<String, String> {
    Err(t!("widgets.spellchecked_line.dictionary_download_unavailable_web").to_string())
}

fn write_text_file(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|err| format!("failed to write '{}': {err}", path.display()))
}

fn project_spellcheck_settings_slot() -> &'static Mutex<Option<PathBuf>> {
    PROJECT_SPELLCHECK_SETTINGS_FILE.get_or_init(|| Mutex::new(None))
}

fn current_project_spellcheck_settings_file() -> Option<PathBuf> {
    match project_spellcheck_settings_slot().lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

pub fn set_project_spellcheck_settings_file(settings_file: Option<PathBuf>) {
    let changed = match project_spellcheck_settings_slot().lock() {
        Ok(mut guard) => {
            if *guard == settings_file {
                false
            } else {
                *guard = settings_file;
                true
            }
        }
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            if *guard == settings_file {
                false
            } else {
                *guard = settings_file;
                true
            }
        }
    };
    if changed {
        invalidate_spellcheck_cache();
    }
}

pub fn current_spellcheck_words_revision() -> u64 {
    SPELLCHECK_WORDS_REVISION.load(Ordering::Relaxed)
}

pub fn load_custom_spellcheck_words() -> Result<String, String> {
    let (_, dic_path) = custom_dictionary_paths();
    let Ok(content) = fs::read_to_string(&dic_path) else {
        return Ok(String::new());
    };
    Ok(parse_custom_dictionary_words(&content).join("\n"))
}

pub fn load_project_spellcheck_words(settings_file: &Path) -> Result<String, String> {
    let root = load_json_object_root(settings_file, "project spellcheck settings file")?;
    let words = root
        .get("canvas")
        .and_then(Value::as_object)
        .and_then(|canvas| canvas.get(PROJECT_CUSTOM_WORDS_KEY))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(normalize_custom_words(words).join("\n"))
}

pub fn save_custom_spellcheck_words(raw: &str) -> Result<(), String> {
    let root_dir = resolve_spellcheck_dir();
    fs::create_dir_all(&root_dir).map_err(|err| {
        format!(
            "failed to create spell_check dir '{}': {err}",
            root_dir.display()
        )
    })?;
    let (aff_path, dic_path) = custom_dictionary_paths();
    let words = normalize_custom_words(raw);
    let dic_body = if words.is_empty() {
        "0\n".to_string()
    } else {
        format!("{}\n{}\n", words.len(), words.join("\n"))
    };
    write_text_file(&aff_path, CUSTOM_AFF_CONTENT)?;
    write_text_file(&dic_path, &dic_body)?;
    SPELLCHECK_WORDS_REVISION.fetch_add(1, Ordering::Relaxed);
    invalidate_spellcheck_cache();
    Ok(())
}

pub fn save_project_spellcheck_words(settings_file: &Path, raw: &str) -> Result<(), String> {
    let mut root = load_json_object_root(settings_file, "project spellcheck settings file")?;
    let Some(root_obj) = root.as_object_mut() else {
        return Err(format!(
            "project spellcheck settings root became non-object unexpectedly: '{}'",
            settings_file.display()
        ));
    };

    let mut canvas_obj = root_obj
        .get("canvas")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    canvas_obj.insert(
        PROJECT_CUSTOM_WORDS_KEY.to_string(),
        Value::String(normalize_custom_words(raw).join("\n")),
    );
    root_obj.insert("canvas".to_string(), Value::Object(canvas_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = settings_file.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create '{}': {err}", parent.display()))?;
    }
    write_text_file(settings_file, &payload)?;
    SPELLCHECK_WORDS_REVISION.fetch_add(1, Ordering::Relaxed);
    invalidate_spellcheck_cache();
    Ok(())
}

pub fn invalidate_spellcheck_cache() {
    if let Some(service) = SPELLCHECK_SERVICE_INSTANCE.get() {
        let mut cache = match service.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        cache.clear();
    }
}

fn load_custom_dictionary_words_from_sources(
    root_dir: &Path,
    project_settings_file: Option<&Path>,
) -> HashMap<ScriptGroup, HashSet<String>> {
    let mut grouped = load_custom_dictionary_words_from_disk(root_dir);
    if let Some(path) = project_settings_file {
        let project_words = load_project_spellcheck_words(path).unwrap_or_else(|err| {
            runtime_log::log_warn(format!(
                "[widgets::spellchecked_line] failed to load project spellcheck words '{}': {err}",
                path.display()
            ));
            String::new()
        });
        merge_custom_words(&mut grouped, normalize_custom_words(&project_words));
    }
    grouped
}

fn load_custom_dictionary_words_from_disk(
    root_dir: &Path,
) -> HashMap<ScriptGroup, HashSet<String>> {
    let (_, dic_path) = custom_dictionary_paths_for_root(root_dir);
    let Ok(content) = fs::read_to_string(&dic_path) else {
        return HashMap::new();
    };
    let mut grouped: HashMap<ScriptGroup, HashSet<String>> = HashMap::new();
    merge_custom_words(&mut grouped, parse_custom_dictionary_words(&content));
    grouped
}

fn merge_custom_words(grouped: &mut HashMap<ScriptGroup, HashSet<String>>, words: Vec<String>) {
    for word in words {
        if let Some(key) = build_cache_key(&word) {
            grouped.entry(key.group).or_default().insert(key.word);
        }
    }
}

fn append_custom_word(words: &mut String, word: &str) {
    let normalized = normalize_custom_words(&format!("{words}\n{word}")).join("\n");
    words.clear();
    words.push_str(&normalized);
}

fn parse_custom_dictionary_words(content: &str) -> Vec<String> {
    content
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            if idx == 0 && trimmed.parse::<usize>().is_ok() {
                return None;
            }
            Some(trimmed.to_string())
        })
        .collect()
}

fn normalize_custom_words(raw: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut words = Vec::new();
    for word in raw.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let normalized = word.to_string();
        if seen.insert(normalized.to_lowercase()) {
            words.push(normalized);
        }
    }
    words
}

fn custom_dictionary_paths() -> (PathBuf, PathBuf) {
    custom_dictionary_paths_for_root(&resolve_spellcheck_dir())
}

fn custom_dictionary_paths_for_root(root_dir: &Path) -> (PathBuf, PathBuf) {
    (
        root_dir.join(format!("{CUSTOM_DICTIONARY_STEM}.aff")),
        root_dir.join(format!("{CUSTOM_DICTIONARY_STEM}.dic")),
    )
}

fn load_json_object_root(path: &Path, scope: &str) -> Result<Value, String> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }

    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {scope} '{}': {err}", path.display()))?;
    let root = serde_json::from_str::<Value>(&raw)
        .map_err(|err| format!("failed to parse {scope} '{}': {err}", path.display()))?;
    if root.is_object() {
        Ok(root)
    } else {
        Err(format!(
            "failed to parse {scope} '{}': root JSON value is not an object",
            path.display()
        ))
    }
}

fn resolve_spellcheck_dir() -> PathBuf {
    crate::config::data_dir().join("spell_check")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::ScriptGroup as TextScriptGroup;
    use std::collections::HashSet;

    /// The widget-level (`Latin`/`Cyrillic`) script a language's dictionary must
    /// classify to. Derived from the language's segmentation group so the two
    /// stay in sync: only the Cyrillic-Slavic languages use a Cyrillic dictionary.
    fn expected_widget_script(language: TextLanguage) -> ScriptGroup {
        match language.group() {
            TextScriptGroup::CyrillicSlavic => ScriptGroup::Cyrillic,
            TextScriptGroup::LatinSlavic
            | TextScriptGroup::Romance
            | TextScriptGroup::English => ScriptGroup::Latin,
        }
    }

    #[test]
    fn dictionary_spec_is_total_with_unique_stems_and_valid_urls() {
        let mut stems = HashSet::new();
        for language in TextLanguage::all() {
            // Total: every language yields a spec (the match is exhaustive; this
            // just exercises it and guards the invariants below).
            let spec = dictionary_spec(language);

            assert!(
                stems.insert(spec.stem),
                "duplicate on-disk stem '{}' for language '{}'",
                spec.stem,
                language.tag()
            );

            for url in [spec.aff_url, spec.dic_url] {
                assert!(
                    url.starts_with("https://"),
                    "non-https url for '{}': {url}",
                    spec.stem
                );
            }
            assert!(
                spec.aff_url.ends_with(".aff"),
                "aff_url must end in .aff for '{}': {}",
                spec.stem,
                spec.aff_url
            );
            assert!(
                spec.dic_url.ends_with(".dic"),
                "dic_url must end in .dic for '{}': {}",
                spec.stem,
                spec.dic_url
            );
        }
        assert_eq!(stems.len(), TextLanguage::all().len());
    }

    #[test]
    fn every_spec_stem_infers_the_expected_script() {
        // A stem typo would make `infer_script_group` return the wrong script (or
        // `None`), silently disabling checking for that language.
        for language in TextLanguage::all() {
            let spec = dictionary_spec(language);
            assert_eq!(
                infer_script_group(spec.stem),
                Some(expected_widget_script(language)),
                "stem '{}' (language '{}') infers the wrong script",
                spec.stem,
                language.tag()
            );
        }
    }

    #[test]
    fn cache_key_partitions_by_language() {
        // A word cached under one typesetting language must be a DIFFERENT key from
        // the same word under another language, so switching languages never reuses
        // a stale verdict. The script group and normalized word are identical (the
        // word matching stays script-based); only the language field differs.
        let ru = build_cache_key_for("привет", TextLanguage::Ru)
            .expect("cyrillic word should classify");
        let uk = build_cache_key_for("привет", TextLanguage::Uk)
            .expect("cyrillic word should classify");

        assert_ne!(ru, uk, "keys must differ across languages");
        assert_eq!(ru.language, TextLanguage::Ru);
        assert_eq!(uk.language, TextLanguage::Uk);
        assert_eq!(ru.group, uk.group);
        assert_eq!(ru.group, ScriptGroup::Cyrillic);
        assert_eq!(ru.word, uk.word);
    }

    /// Builds an in-memory dictionary bundle from a word list. No filesystem, no
    /// network: `zspell` accepts the `.aff`/`.dic` bodies as strings.
    fn test_bundle(stem: &str, group: ScriptGroup, words: &[&str]) -> DictionaryBundle {
        let dic_body = format!("{}\n{}\n", words.len(), words.join("\n"));
        let dictionary = zspell::builder()
            .config_str("SET UTF-8\n")
            .dict_str(&dic_body)
            .build()
            .expect("in-memory test dictionary should build");
        DictionaryBundle {
            stem: stem.to_string(),
            group,
            dictionary,
        }
    }

    fn test_state(active: Option<TextLanguage>, bundles: Vec<DictionaryBundle>) -> SpellcheckWorkerState {
        SpellcheckWorkerState {
            root_dir: PathBuf::from("/nonexistent-spellcheck-root"),
            project_settings_file: None,
            bundles,
            custom_words: HashMap::new(),
            loaded_signature: Vec::new(),
            download_attempted: HashSet::new(),
            active_language: active,
        }
    }

    /// The regression this guards: with `ru_RU` and `uk_UA` both left on disk, a
    /// purely script-based match would accept a Ukrainian spelling inside a Russian
    /// chapter. Only the ACTIVE language's dictionary may vote on its own script.
    #[test]
    fn same_script_dictionary_of_another_language_does_not_vote() {
        let bundles = vec![
            test_bundle("ru_RU", ScriptGroup::Cyrillic, &["привет"]),
            test_bundle("uk_UA", ScriptGroup::Cyrillic, &["привіт"]),
        ];
        let state = test_state(Some(TextLanguage::Ru), bundles);

        assert_eq!(
            evaluate_word(&state, ScriptGroup::Cyrillic, "привет"),
            SpellStatus::Correct,
            "the active language's own word must be accepted"
        );
        assert_eq!(
            evaluate_word(&state, ScriptGroup::Cyrillic, "привіт"),
            SpellStatus::Incorrect,
            "a Ukrainian spelling must not pass just because uk_UA is still on disk"
        );
    }

    /// A word of the OTHER script is still judged by any dictionary of that script,
    /// so mixed-script text (a Cyrillic name inside a Spanish chapter) keeps working.
    #[test]
    fn other_script_dictionary_still_votes_for_mixed_text() {
        let bundles = vec![
            test_bundle("es_ES", ScriptGroup::Latin, &["casa"]),
            test_bundle("ru_RU", ScriptGroup::Cyrillic, &["привет"]),
        ];
        let state = test_state(Some(TextLanguage::Es), bundles);

        assert_eq!(
            evaluate_word(&state, ScriptGroup::Cyrillic, "привет"),
            SpellStatus::Correct,
            "a Cyrillic word must still match the Cyrillic dictionary on disk"
        );
        assert_eq!(evaluate_word(&state, ScriptGroup::Latin, "casa"), SpellStatus::Correct);
        assert_eq!(evaluate_word(&state, ScriptGroup::Latin, "kasa"), SpellStatus::Incorrect);
    }

    /// When the active language's dictionary is absent (e.g. its download failed),
    /// its words are left UNMARKED rather than judged by a sibling-script dictionary
    /// that would flag nearly all of them.
    #[test]
    fn missing_active_dictionary_leaves_words_unmarked() {
        let bundles = vec![test_bundle("ru_RU", ScriptGroup::Cyrillic, &["привет"])];
        let state = test_state(Some(TextLanguage::Sr), bundles);

        assert_eq!(
            evaluate_word(&state, ScriptGroup::Cyrillic, "здраво"),
            SpellStatus::Unsupported,
            "Serbian text must not be judged by the Russian dictionary"
        );
    }

    #[test]
    fn classification_is_language_independent() {
        // The (group, normalized word) pair does not depend on the active language;
        // only the cache partition does.
        let latin = classify_and_normalize("Casa").expect("latin word should classify");
        assert_eq!(latin, (ScriptGroup::Latin, "casa".to_string()));

        for language in TextLanguage::all() {
            let key = build_cache_key_for("Casa", language).expect("should classify");
            assert_eq!(key.group, ScriptGroup::Latin);
            assert_eq!(key.word, "casa");
            assert_eq!(key.language, language);
        }
    }
}
