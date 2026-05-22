/*
File: src/widgets/spellchecked_line.rs

Purpose:
Многострочный `egui::TextEdit` с фоновой проверкой орфографии по Hunspell-
совместимым словарям и подчёркиванием некорректных слов.

Main responsibilities:
- оборачивать стандартный `TextEdit` без блокировки GUI-потока;
- загружать все словари из папки `spell_check` и автоматически докачивать
  `ru_RU` из LibreOffice при отсутствии русского словаря;
- кэшировать проверки слов и отправлять новые слова в background worker;
- подчёркивать слова с ошибками через custom layouter.

Key structures:
- `SpellcheckedTextEdit`
- `SpellcheckService`

Notes:
- Проверка использует pure-Rust crate `zspell`, совместимый с Hunspell
  словарями `.aff` + `.dic`.
- Словари читаются из app-local папки `spell_check`; GUI-поток не делает
  файловых и сетевых операций.
- Пользовательские исключения объединяют app-global `custom.dic` и project-local
  список из `settings.json`.
*/

use crate::runtime_log;
use egui::epaint::text::{LayoutJob, TextFormat};
use egui::text_edit::TextEditOutput;
use egui::{Align, Color32, Id, Response, Stroke, TextBuffer, TextEdit, Ui, Widget};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::Hash;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
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

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct SpellCacheKey {
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
    russian_download_attempted: bool,
}

impl SpellcheckWorkerState {
    fn new() -> Self {
        Self {
            root_dir: resolve_spellcheck_dir(),
            project_settings_file: current_project_spellcheck_settings_file(),
            bundles: Vec::new(),
            custom_words: HashMap::new(),
            loaded_signature: Vec::new(),
            russian_download_attempted: false,
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
    pub fn id_salt(mut self, salt: impl Hash) -> Self {
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
        self.show(ui).response
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
    for (token, status) in tokens.iter().zip(statuses.into_iter()) {
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
    let byte_index = char_index_to_byte_index(text, cursor.index);
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
        byte_range: range,
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

fn build_cache_key(raw: &str) -> Option<SpellCacheKey> {
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

    Some(SpellCacheKey {
        group,
        word: lowered,
    })
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

    let mut grouped: HashMap<ScriptGroup, Vec<String>> = HashMap::new();
    for SpellRequest(key) in batch {
        grouped.entry(key.group).or_default().push(key.word);
    }

    for (group, words) in grouped {
        let bundles: Vec<&DictionaryBundle> = state
            .bundles
            .iter()
            .filter(|bundle| bundle.group == group)
            .collect();
        let mut guard = match cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for word in words {
            let status = if state
                .custom_words
                .get(&group)
                .is_some_and(|custom_words| custom_words.contains(&word))
            {
                SpellStatus::Correct
            } else if bundles.is_empty() {
                SpellStatus::Unsupported
            } else if bundles
                .iter()
                .any(|bundle| bundle.dictionary.check_word(&word))
            {
                SpellStatus::Correct
            } else {
                SpellStatus::Incorrect
            };
            guard.insert(SpellCacheKey { group, word }, status);
        }
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

    let mut files = discover_dictionary_files(&state.root_dir);
    if !contains_russian_bundle(&files) && !state.russian_download_attempted {
        state.russian_download_attempted = true;
        if let Err(err) = download_russian_dictionary(&state.root_dir) {
            runtime_log::log_warn(format!(
                "[widgets::spellchecked_line] failed to download LibreOffice ru_RU dictionary: {err}"
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

    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.retain(|_, status| *status != SpellStatus::Unsupported);
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

fn contains_russian_bundle(files: &[DictionaryFiles]) -> bool {
    files
        .iter()
        .any(|files| files.stem.eq_ignore_ascii_case("ru_RU"))
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
            Ok(dictionary) => bundles.push(DictionaryBundle { group, dictionary }),
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

fn download_russian_dictionary(root_dir: &Path) -> Result<(), String> {
    const RU_AFF_URL: &str =
        "https://raw.githubusercontent.com/LibreOffice/dictionaries/master/ru_RU/ru_RU.aff";
    const RU_DIC_URL: &str =
        "https://raw.githubusercontent.com/LibreOffice/dictionaries/master/ru_RU/ru_RU.dic";

    runtime_log::log_info(format!(
        "[widgets::spellchecked_line] downloading LibreOffice ru_RU dictionary into '{}'",
        root_dir.display()
    ));

    let aff_content = download_text_file(RU_AFF_URL)?;
    let dic_content = download_text_file(RU_DIC_URL)?;
    write_text_file(&root_dir.join("ru_RU.aff"), &aff_content)?;
    write_text_file(&root_dir.join("ru_RU.dic"), &dic_content)?;
    Ok(())
}

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
