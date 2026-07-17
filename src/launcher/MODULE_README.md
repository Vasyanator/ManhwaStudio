# Module: src/launcher

## Purpose
Rust launcher runtime shown before a chapter is opened.

## Architecture
`mod.rs` owns the native launcher entry points and returns a typed outcome to `main.rs`.
`app.rs` owns the root `eframe::App`, background image workers, detached child windows, and
page navigation. Page modules render focused launcher workflows and report actions through
`PageNavAction`.

The launcher does not perform blocking I/O on the GUI thread. Startup update checks are started by
`main.rs` and delivered to the launcher through a channel; the launcher only polls and renders the
notification.

## Files and submodules
- `mod.rs`: launcher window setup, app metadata, and public run functions.
- `app.rs`: root app state, worker polling, page routing, detached viewport handling.
- `main_page.rs`: central menu, update notification overlay, and AI install-type notices.
- `state.rs`: page enum, shared UI state, and typed launcher outcomes.
- `background.rs`: background image plan and decode workers.
- `first_run_language.rs`: first-run interface/typesetting language-selection modal
  (radio toggles, system-locale preselect). Reuses `general_settings_panel`'s
  `pub(crate)` scan/install/persist helpers; blocks input with the tutorial-engine
  overlay pattern. Edit here to change the modal's detection, preselection, or layout.
- `pages/`: fullscreen launcher pages for open/import/export/settings flows.
- `new_project/`: detached new-project workflow.
- `psd_import_window.rs`: detached PSD import workflow.
- `theme.rs`: launcher visual style helpers.
- `tutorial.rs`: step script for the main-menu tour (`TutorialId::LauncherMain`); its target keys
  match the `mark` calls in `main_page.rs`.

## Tutorial wiring
`app.rs` owns a `TutorialController<LauncherState>` (lighter dim so the wallpaper stays visible) and
shares its `TutorialProgressHandle` with `settings_page` (the "Обучение" tab). Per-frame in `fn ui`:
edge-triggered `maybe_autoplay(LauncherMain)` on entering the main page → `sync` + `begin_frame`
before the panel → `main_page.rs` records button rects via `app.tutorial.mark(...)` → `render` after
the child windows. See `src/tutorial/MODULE_README.md` for the engine contract.

## Contracts and invariants
- Launcher outcomes are returned to startup flow; the launcher must not spawn a second main app.
- Long scans, image decoding, probes, downloads, and shell work run on worker threads.
- Settings changes to the projects root must be propagated to every page/window that caches it.
- Update notifications are advisory UI state; starting an update closes the launcher and returns
  `LauncherOutcome::StartUpdate` to `main.rs`.
- The first-run language modal (`first_run_language.rs`) and the main-menu tutorial are mutually
  exclusive: while the modal is `Some`, `app.rs` suppresses `maybe_autoplay(LauncherMain)` and hands
  the tour off exactly once on confirm. The modal is gated on the persisted tri-state marker
  `General.first_run_languages_confirmed` (present + `false` = show), NOT on the language keys being
  absent — the startup flow persists the defaults tree (materializing both language keys) before the
  launcher constructs, so `config::mark_first_run_languages_if_needed` records the marker earlier, before
  any defaults-persisting call. The modal persists nothing until confirmed (then writes both languages
  plus the marker `= true`); a config read error skips it (never blocks the user).

## Editing map
- To change launcher startup or return values, edit `mod.rs` and `state.rs`.
- To change root polling, page routing, or window lifecycle, edit `app.rs`.
- To change the main menu or update notice, edit `main_page.rs`.
- To change a specific page workflow, edit `pages/`.
