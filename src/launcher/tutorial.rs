/*
File: src/launcher/tutorial.rs

Purpose:
The step script for the launcher main-menu tutorial (`TutorialId::LauncherMain`).
Each step points at a main-menu button by the key `main_page.rs` records via
`app.tutorial.mark(...)`. The context is `LauncherState` so a future step could
navigate pages through `on_enter`; the current basic tour needs no navigation
because every target lives on the main page.

Notes:
Keys here MUST match the `mark` keys in `src/launcher/main_page.rs`. If a button
is renamed/removed, update both sites.
*/

use crate::launcher::state::LauncherState;
use crate::tutorial::TutorialStep;

/// Target keys shared with `main_page.rs`. Kept as constants so the two sites
/// (mark + script) reference one source of truth.
pub const TARGET_OPEN: &str = "launcher_open";
pub const TARGET_NEW: &str = "launcher_new";
pub const TARGET_IMPORT: &str = "launcher_import";
pub const TARGET_EXPORT: &str = "launcher_export";
pub const TARGET_SETTINGS: &str = "launcher_settings";

/// Build the launcher main-menu tour: one short callout per menu action.
#[must_use]
pub fn steps() -> Vec<TutorialStep<LauncherState>> {
    vec![
        TutorialStep::new(
            [TARGET_OPEN],
            "Открыть главу",
            "Открывает уже созданную главу из вашей папки проектов, чтобы продолжить перевод.",
        ),
        TutorialStep::new(
            [TARGET_NEW],
            "Новая глава",
            "Полноценный комбайн для выкачки и предобработки глав:\n\
             - Просто открытие уже скачанной главы\n\
             - Быстрая выкачка с определенных сайтов\n\
             - Продвинутая выкачка разными способами из подконтрольного браузера\n\
             - Склейка и нарезка ленты вебтуна\n\
             - Удаление шума и апскейл через Reline и Waifu2x\n\
             - Можно использовать просто для выкачки и обработки глав, не обязательно сохраняя их как проект",
        ),
        TutorialStep::new(
            [TARGET_IMPORT],
            "Импорт главы",
            "Импортирует готовую главу из файла .mschapter или .psd.\n\n\
            Можно импортировать сразу заклиненную главу в zip или rar.",
        ),
        TutorialStep::new(
            [TARGET_EXPORT],
            "Экспорт главы",
            "Упаковывает главу в .mschapter, чтобы передать её другому переводчику.",
        ),
        TutorialStep::new(
            [TARGET_SETTINGS],
            "Настройки",
            "Папка проектов, ИИ-бэкенд, вычислительные устройства и это обучение — всё здесь. \
             Заново запустить любую подсказку можно во вкладке «Обучение».",
        ),
    ]
}
