/*
File: src/text_punctuation.rs

Purpose:
Общий редактируемый список «висящей» пунктуации — символов, которые при включённой
висящей пунктуации выносятся за края строки и не идут в счёт её ширины. Раньше
список был зашит прямо в рендере текста (`render_next::wrap::is_hanging_punctuation`);
теперь он один на всё приложение, грузится из `user_config.json`
(`TextTab.hanging_punctuation`) и редактируется в настройках.

Used by:
- `render_next::wrap` (перенос/раскладка) и `render_next::wrap::forms` (перебор форм)
  через `is_hanging_punctuation`;
- вкладка настроек — чтение/запись через `hanging_punctuation_string` /
  `set_hanging_punctuation`;
- `config::user_config_defaults` берёт отсюда дефолт.

Concurrency:
Набор хранится в глобальном `RwLock` плюс счётчик поколений. Горячий путь
(`is_hanging_punctuation`, вызывается по символу в тугих циклах рендера и перебора
форм) читает потоково-локальный снимок и обновляет его только при смене поколения,
так что блокировка берётся лишь при фактическом изменении списка.
*/

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

/// Дефолтный список висящей пунктуации (латиница, кириллица, типографские кавычки,
/// CJK и полуширинные знаки). Пробелы игнорируются при разборе.
pub const DEFAULT_HANGING_PUNCTUATION: &str =
    ".,!?:;-–—~…·•。、，．！？：；・･()[]{}\"'«»\u{201C}\u{201D}\u{2018}\u{2019}\u{2039}\u{203A}\u{201E}\u{201F}\u{201A}";

struct PunctState {
    /// Исходный текст набора (для отображения/редактирования без потери порядка).
    text: String,
    /// Множество символов для быстрых проверок.
    set: HashSet<char>,
}

impl PunctState {
    fn from_text(text: &str) -> Self {
        let set = text.chars().filter(|ch| !ch.is_whitespace()).collect();
        Self {
            text: text.to_string(),
            set,
        }
    }
}

static STORE: OnceLock<RwLock<PunctState>> = OnceLock::new();
/// Стартует с 1, чтобы потоко-локальный кеш (поколение 0) обновился при первом вызове.
static GENERATION: AtomicU64 = AtomicU64::new(1);

fn store() -> &'static RwLock<PunctState> {
    STORE.get_or_init(|| RwLock::new(PunctState::from_text(&load_initial_text())))
}

/// Читает сохранённый набор из пользовательского конфига; при любой ошибке —
/// дефолт. Вызывается один раз при первом обращении к набору.
fn load_initial_text() -> String {
    crate::config::load_user_config()
        .ok()
        .and_then(|cfg| {
            cfg.get_path(&["TextTab", "hanging_punctuation"])
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .filter(|text| text.chars().any(|ch| !ch.is_whitespace()))
        .unwrap_or_else(|| DEFAULT_HANGING_PUNCTUATION.to_string())
}

thread_local! {
    /// `(поколение, снимок множества)` для текущего потока.
    static CACHE: RefCell<(u64, HashSet<char>)> = RefCell::new((0, HashSet::new()));
}

/// Является ли символ висящей пунктуацией согласно текущему набору.
#[must_use]
pub fn is_hanging_punctuation(ch: char) -> bool {
    let generation = GENERATION.load(Ordering::Acquire);
    CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.0 != generation {
            cache.1 = store().read().unwrap_or_else(|err| err.into_inner()).set.clone();
            cache.0 = generation;
        }
        cache.1.contains(&ch)
    })
}

/// Заменяет набор и помечает кеши всех потоков устаревшими. Пробелы отбрасываются.
pub fn set_hanging_punctuation(text: &str) {
    {
        let mut guard = store().write().unwrap_or_else(|err| err.into_inner());
        *guard = PunctState::from_text(text);
    }
    GENERATION.fetch_add(1, Ordering::Release);
}

/// Текущий набор как строка (в исходном порядке, для отображения в настройках).
#[must_use]
pub fn hanging_punctuation_string() -> String {
    store()
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .text
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Разбор текста в множество проверяем на приватном `PunctState`, чтобы не
    // трогать глобальный набор: он общий на весь тест-процесс, и `forms`-тесты
    // тоже читают его через `is_hanging_punctuation`.
    #[test]
    fn parses_text_into_set_ignoring_whitespace() {
        let state = PunctState::from_text(".,  !-");
        assert!(state.set.contains(&'.'));
        assert!(state.set.contains(&','));
        assert!(state.set.contains(&'!'));
        assert!(state.set.contains(&'-'));
        assert!(!state.set.contains(&' '));
        // Текст сохраняется как есть (для отображения в настройках).
        assert_eq!(state.text, ".,  !-");
    }

    // Глобально только подтверждаем дефолт (это и есть ожидаемое состояние для
    // прочих тестов), не сужая набор.
    #[test]
    fn default_set_marks_hanging_chars() {
        set_hanging_punctuation(DEFAULT_HANGING_PUNCTUATION);
        assert!(is_hanging_punctuation('-'));
        assert!(is_hanging_punctuation('—'));
        assert!(is_hanging_punctuation('«'));
        assert!(!is_hanging_punctuation('а'));
        assert!(!is_hanging_punctuation('1'));
    }
}
