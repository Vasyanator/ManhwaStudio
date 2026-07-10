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
            t!("launcher.tutorial.open_chapter_title"),
            t!("launcher.tutorial.open_chapter_message"),
        ),
        TutorialStep::new(
            [TARGET_NEW],
            t!("launcher.tutorial.new_chapter_title"),
            t!("launcher.tutorial.new_chapter_message"),
        ),
        TutorialStep::new(
            [TARGET_IMPORT],
            t!("launcher.tutorial.import_chapter_title"),
            t!("launcher.tutorial.import_chapter_message"),
        ),
        TutorialStep::new(
            [TARGET_EXPORT],
            t!("launcher.tutorial.export_chapter_title"),
            t!("launcher.tutorial.export_chapter_message"),
        ),
        TutorialStep::new(
            [TARGET_SETTINGS],
            t!("launcher.tutorial.settings_title"),
            t!("launcher.tutorial.settings_message"),
        ),
    ]
}
