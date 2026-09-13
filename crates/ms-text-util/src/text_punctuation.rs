/*
File: crates/ms-text-util/src/text_punctuation.rs

Purpose:
Общий редактируемый список «висящей» пунктуации — символов, которые при включённой
висящей пунктуации выносятся за края строки и не идут в счёт её ширины. Один набор
на всё приложение.

Here also lives `clamp_hanging_weight`, the single normalizer of the hanging
strength the renderer and the wrap share (`0.0..=1.0`). It is deliberately in this
module and not in the renderer: both crates read the same contract, and the weight
is meaningless without the character set defined here. It does NOT touch the set.

Contract (после выноса в крейт `ms-text-util`):
Крейт config-free — сам он НЕ читает `user_config.json`. При первом обращении набор
инициализируется из `DEFAULT_HANGING_PUNCTUATION`. Приложение на старте засевает
пользовательское значение через `set_hanging_punctuation` (см. main.rs
`seed_hanging_punctuation_from_config`), а настройки редактируют его тем же вызовом.

Used by:
- `ms_text_render` (перенос/раскладка и перебор форм) через `is_hanging_punctuation`;
- вкладка настроек — чтение/запись через `hanging_punctuation_string` /
  `set_hanging_punctuation`;
- `config::user_config_defaults` берёт отсюда дефолт (`DEFAULT_HANGING_PUNCTUATION`).

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

/// Инициализирует хранилище дефолтным набором. Крейт config-free: пользовательское
/// значение засевается приложением через `set_hanging_punctuation` на старте.
fn store() -> &'static RwLock<PunctState> {
    STORE.get_or_init(|| RwLock::new(PunctState::from_text(DEFAULT_HANGING_PUNCTUATION)))
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

/// Normalizes a hanging-punctuation STRENGTH into its contract range `0.0..=1.0`.
///
/// `0.0` = punctuation counts in full toward the line (historical "off"), `1.0` =
/// it hangs completely and contributes nothing (historical "on"), an intermediate
/// `v` = the hanging run contributes `1 - v` of its real width. Out-of-range
/// values are clamped and `NaN` is treated as `0.0`, so a caller that computed the
/// weight from user input or a config file can never poison the layout.
///
/// The zero it returns is always `+0.0`. `-0.0` (reachable from a hand-edited
/// document) compares equal to `0.0` yet carries a different bit pattern, and
/// consumers key caches by `f32::to_bits`, where the two zeros split one key in two.
/// This being THE single normalizer of the value, a non-canonical zero must not
/// survive it.
#[must_use]
pub fn clamp_hanging_weight(weight: f32) -> f32 {
    // `f32::clamp` propagates NaN instead of rejecting it, so NaN is caught first.
    if weight.is_nan() {
        return 0.0;
    }
    let clamped = weight.clamp(0.0, 1.0);
    // `f32::clamp` compares numerically and `-0.0 >= 0.0` holds, so a negative zero
    // passes straight through it. Hand back the positive-zero literal instead.
    if clamped == 0.0 { 0.0 } else { clamped }
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

    #[test]
    fn hanging_weight_is_clamped_and_nan_means_off() {
        assert_eq!(clamp_hanging_weight(0.0), 0.0);
        assert_eq!(clamp_hanging_weight(1.0), 1.0);
        assert_eq!(clamp_hanging_weight(0.25), 0.25);
        assert_eq!(clamp_hanging_weight(-3.0), 0.0);
        assert_eq!(clamp_hanging_weight(17.5), 1.0);
        assert_eq!(clamp_hanging_weight(f32::INFINITY), 1.0);
        assert_eq!(clamp_hanging_weight(f32::NEG_INFINITY), 0.0);
        // NaN must behave exactly like "off", never propagate into the layout math.
        assert_eq!(clamp_hanging_weight(f32::NAN), 0.0);

        // Every zero the normalizer hands back is the POSITIVE one. `assert_eq!` cannot
        // see this (`-0.0 == 0.0`), so the bit pattern is checked directly: consumers key
        // caches by `to_bits`, where the two zeros would be two different keys.
        for input in [-0.0f32, 0.0, -1.0, f32::NEG_INFINITY, f32::NAN] {
            let clamped = clamp_hanging_weight(input);
            assert_eq!(clamped, 0.0, "input {input} must clamp to zero");
            assert!(
                clamped.is_sign_positive(),
                "input {input} must yield +0.0, got a negative zero"
            );
            assert_eq!(clamped.to_bits(), 0.0f32.to_bits(), "input {input}");
        }
        // A non-zero result passes through untouched, bit pattern included.
        assert_eq!(clamp_hanging_weight(0.25).to_bits(), 0.25f32.to_bits());
        assert_eq!(clamp_hanging_weight(1.0).to_bits(), 1.0f32.to_bits());
    }
}
