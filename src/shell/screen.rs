/*
File: src/shell/screen.rs

Purpose:
The pure state machine of the main window: which screen is active, which event moves it where,
what the shell must do on the way, and whether an OS close is allowed. No egui, no I/O, no
threads — `AppShell` (`mod.rs`) owns the data each screen needs and executes the returned
`Effect`; every decision about WHERE the window goes next is taken here, once.

Key structures:
- `ScreenKind` / `StudioPhase` / `AfterClose`: the active screen, the studio's phase, and where
  a Closing screen continues once the studio teardown finished.
- `ShellEvent` / `LauncherExit`: everything that can move the window.
- `Transition` / `Effect`: the next screen, the shell's action, the OS-close verdict.
- `ShellExit`: what `run_main` does after the window closed.

Key functions:
- `on_event(kind, event)`: the transition table (`None` = the event does not apply there).
- `launcher_exit_for(outcome, test_mode)`: maps a launcher outcome to its exit.
- `style_screen(kind)`, `min_inner_size(kind)`: per-screen window attributes.

Notes:
OS close policy (the `ShellEvent::OsClose` rows): PreparingLauncher, Launcher and a failed
studio load allow it (the window quits); a loading studio cancels it and closes once the load
worker delivered (it may be mid-write); a running studio cancels it and asks the studio
(`MangaApp::request_os_close`: exit dialog or a close request); a Closing screen cancels it and
turns into "exit after the teardown"; Exiting allows it, because the shell itself sent that
`Close` (eframe re-delivers a sent `Close` as `close_requested` on the next frame, so cancelling
it would loop forever).
*/

use crate::app::PendingCloseAction;
use crate::shell::studio::StartupTarget;
use crate::shell::style::StyleScreen;
use crate::tabs::AppTab;
use std::path::PathBuf;

/// What `run_main` does once the main window has closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellExit {
    /// End the process.
    Quit,
    /// Run the installer's update window (the launcher's "Update" button), then end.
    StartUpdate,
}

/// The phase of the studio screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StudioPhase {
    /// The open worker runs. `close_after_load`: the OS close arrived meanwhile; the window
    /// closes once the worker delivered (it performs non-atomic chapter writes).
    Loading { close_after_load: bool },
    /// The load failed; the error screen offers "back to the launcher" and "exit".
    Failed,
    /// `MangaApp` runs.
    Running,
}

/// Where a Closing screen continues once the studio teardown has finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AfterClose {
    /// Back to the launcher (prepared again on a worker).
    Launcher,
    /// Reload the chapter from disk after a structural page operation, on `tab`.
    Reload { project_dir: PathBuf, tab: AppTab },
    /// Close the window and end the process.
    Exit,
}

/// The active screen of the main window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScreenKind {
    /// A worker prepares the launcher entry (`ms_launcher::prepare_entry`, blocking I/O).
    PreparingLauncher,
    /// The launcher screen.
    Launcher,
    /// The studio screen in `StudioPhase`.
    Studio(StudioPhase),
    /// A worker tears the studio down (`MangaApp::shutdown_blocking`, scratch release).
    Closing { then: AfterClose },
    /// The shell sent the root `Close`; the window is going away with `exit`.
    Exiting { exit: ShellExit },
}

/// How the launcher screen was left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LauncherExit {
    /// Open a chapter or an image in the studio.
    Open(StartupTarget),
    /// Close the window and run the update window.
    StartUpdate,
    /// Close the window (a `--test-launcher` run ends on any launcher exit).
    Quit,
}

/// Everything that can move the main window from one screen to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellEvent {
    /// The launcher entry arrived from its worker.
    LauncherPrepared,
    /// The launcher screen produced its exit.
    LauncherExit(LauncherExit),
    /// The studio open worker delivered a project.
    StudioLoaded,
    /// The studio open worker reported a failure (or died).
    StudioLoadFailed,
    /// The running studio decided to close (`MangaApp::take_close_request`).
    StudioClose(PendingCloseAction),
    /// The running studio asked to reload its chapter after a structural page operation.
    StudioReload { project_dir: PathBuf },
    /// The load-error screen's "back to the launcher" button.
    FailedBack,
    /// The load-error screen's "exit" button.
    FailedExit,
    /// The studio teardown finished (and, after a failed save, the user confirmed it).
    ShutdownFinished,
    /// The OS asked to close the root window.
    OsClose,
}

/// What the shell does while entering [`Transition::next`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Nothing besides switching the screen (if it changes).
    None,
    /// Start the launcher-entry worker.
    PrepareLauncher,
    /// Build the launcher screen from the prepared entry.
    ShowLauncher,
    /// Start the studio open worker for `target`; a reload restores `tab`.
    LoadStudio { target: StartupTarget, tab: Option<AppTab> },
    /// Construct `MangaApp` from the delivered project.
    StartStudio,
    /// Show the load-error screen.
    ShowLoadError,
    /// Move the studio (app and open request) onto the teardown worker.
    Shutdown,
    /// Hand the OS close to the running studio (`MangaApp::request_os_close`).
    RequestOsClose,
    /// Send the root `ViewportCommand::Close`.
    CloseWindow,
}

/// The result of one event: the next screen, the shell's action, and — for
/// [`ShellEvent::OsClose`] — whether the OS close must be cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transition {
    pub next: ScreenKind,
    pub effect: Effect,
    pub cancel_os_close: bool,
}

impl Transition {
    /// A transition that does not touch an OS close.
    fn to(next: ScreenKind, effect: Effect) -> Self {
        Self { next, effect, cancel_os_close: false }
    }

    /// An OS-close verdict: cancel (`cancel = true`) or allow.
    fn os_close(next: ScreenKind, effect: Effect, cancel: bool) -> Self {
        Self { next, effect, cancel_os_close: cancel }
    }
}

/// The transition table. `None` means the event does not apply to `kind` (a stray event, for
/// example a second exit polled in the frame the screen already changed: the shell logs it and
/// stays where it is). See the file header for the OS-close rows. Every (screen, event) pair is
/// matched explicitly, so a new screen or event must be placed in the table.
#[must_use]
pub(crate) fn on_event(kind: &ScreenKind, event: ShellEvent) -> Option<Transition> {
    use ShellEvent as E;
    let loading = |close_after_load| ScreenKind::Studio(StudioPhase::Loading { close_after_load });
    let closing = |then| ScreenKind::Closing { then };
    let exiting = |exit| ScreenKind::Exiting { exit };
    let transition = match kind {
        ScreenKind::PreparingLauncher => match event {
            E::LauncherPrepared => Transition::to(ScreenKind::Launcher, Effect::ShowLauncher),
            E::LauncherExit(_) | E::StudioLoaded | E::StudioLoadFailed | E::StudioClose(_) | E::StudioReload { .. } | E::FailedBack | E::FailedExit | E::ShutdownFinished => return None,
            E::OsClose => on_os_close(kind),
        },
        ScreenKind::Launcher => match event {
            E::LauncherExit(LauncherExit::Open(target)) => Transition::to(loading(false), Effect::LoadStudio { target, tab: None }),
            E::LauncherExit(LauncherExit::StartUpdate) => Transition::to(exiting(ShellExit::StartUpdate), Effect::CloseWindow),
            E::LauncherExit(LauncherExit::Quit) => Transition::to(exiting(ShellExit::Quit), Effect::CloseWindow),
            E::LauncherPrepared | E::StudioLoaded | E::StudioLoadFailed | E::StudioClose(_) | E::StudioReload { .. } | E::FailedBack | E::FailedExit | E::ShutdownFinished => return None,
            E::OsClose => on_os_close(kind),
        },
        ScreenKind::Studio(StudioPhase::Loading { close_after_load }) => match event {
            // A close that arrived during the load wins over both results: tear down, then exit.
            E::StudioLoaded if *close_after_load => Transition::to(closing(AfterClose::Exit), Effect::Shutdown),
            E::StudioLoaded => Transition::to(ScreenKind::Studio(StudioPhase::Running), Effect::StartStudio),
            E::StudioLoadFailed if *close_after_load => Transition::to(closing(AfterClose::Exit), Effect::Shutdown),
            E::StudioLoadFailed => Transition::to(ScreenKind::Studio(StudioPhase::Failed), Effect::ShowLoadError),
            E::LauncherPrepared | E::LauncherExit(_) | E::StudioClose(_) | E::StudioReload { .. } | E::FailedBack | E::FailedExit | E::ShutdownFinished => return None,
            E::OsClose => on_os_close(kind),
        },
        ScreenKind::Studio(StudioPhase::Running) => match event {
            E::StudioClose(PendingCloseAction::Exit) => Transition::to(closing(AfterClose::Exit), Effect::Shutdown),
            E::StudioClose(PendingCloseAction::ReturnToLauncher) => Transition::to(closing(AfterClose::Launcher), Effect::Shutdown),
            E::StudioReload { project_dir } => Transition::to(closing(AfterClose::Reload { project_dir, tab: AppTab::PageManager }), Effect::Shutdown),
            E::LauncherPrepared | E::LauncherExit(_) | E::StudioLoaded | E::StudioLoadFailed | E::FailedBack | E::FailedExit | E::ShutdownFinished => return None,
            E::OsClose => on_os_close(kind),
        },
        ScreenKind::Studio(StudioPhase::Failed) => match event {
            E::FailedBack => Transition::to(closing(AfterClose::Launcher), Effect::Shutdown),
            E::FailedExit => Transition::to(closing(AfterClose::Exit), Effect::Shutdown),
            E::LauncherPrepared | E::LauncherExit(_) | E::StudioLoaded | E::StudioLoadFailed | E::StudioClose(_) | E::StudioReload { .. } | E::ShutdownFinished => return None,
            E::OsClose => on_os_close(kind),
        },
        ScreenKind::Closing { then } => match event {
            E::ShutdownFinished => match then {
                AfterClose::Launcher => Transition::to(ScreenKind::PreparingLauncher, Effect::PrepareLauncher),
                AfterClose::Reload { project_dir, tab } => {
                    Transition::to(loading(false), Effect::LoadStudio { target: StartupTarget::Project(project_dir.clone()), tab: Some(*tab) })
                }
                AfterClose::Exit => Transition::to(exiting(ShellExit::Quit), Effect::CloseWindow),
            },
            E::LauncherPrepared | E::LauncherExit(_) | E::StudioLoaded | E::StudioLoadFailed | E::StudioClose(_) | E::StudioReload { .. } | E::FailedBack | E::FailedExit => return None,
            E::OsClose => on_os_close(kind),
        },
        ScreenKind::Exiting { .. } => match event {
            E::LauncherPrepared | E::LauncherExit(_) | E::StudioLoaded | E::StudioLoadFailed | E::StudioClose(_) | E::StudioReload { .. } | E::FailedBack | E::FailedExit | E::ShutdownFinished => return None,
            E::OsClose => on_os_close(kind),
        },
    };
    Some(transition)
}

/// The OS-close policy (file header), one row per screen. Exhaustive on purpose: a new screen must
/// decide whether it lets the window close.
fn on_os_close(kind: &ScreenKind) -> Transition {
    match kind {
        ScreenKind::PreparingLauncher | ScreenKind::Launcher | ScreenKind::Studio(StudioPhase::Failed) => {
            Transition::os_close(ScreenKind::Exiting { exit: ShellExit::Quit }, Effect::None, false)
        }
        ScreenKind::Studio(StudioPhase::Loading { .. }) => {
            Transition::os_close(ScreenKind::Studio(StudioPhase::Loading { close_after_load: true }), Effect::None, true)
        }
        ScreenKind::Studio(StudioPhase::Running) => Transition::os_close(ScreenKind::Studio(StudioPhase::Running), Effect::RequestOsClose, true),
        ScreenKind::Closing { .. } => Transition::os_close(ScreenKind::Closing { then: AfterClose::Exit }, Effect::None, true),
        ScreenKind::Exiting { exit } => Transition::os_close(ScreenKind::Exiting { exit: *exit }, Effect::None, false),
    }
}

/// The exit a launcher `outcome` produces: a `--test-launcher` run (`test_mode`) quits on any
/// outcome; otherwise a chapter or an image opens the studio and "Update" runs the update window.
#[must_use]
pub(crate) fn launcher_exit_for(outcome: crate::launcher::state::LauncherOutcome, test_mode: bool) -> LauncherExit {
    use crate::launcher::state::LauncherOutcome;
    if test_mode {
        return LauncherExit::Quit;
    }
    match outcome {
        LauncherOutcome::OpenProject(selection) => LauncherExit::Open(StartupTarget::Project(selection.project_dir)),
        LauncherOutcome::OpenImage(path) => LauncherExit::Open(StartupTarget::Image(path)),
        LauncherOutcome::StartUpdate => LauncherExit::StartUpdate,
    }
}

/// The style a screen installs on entry, or `None` for Closing / Exiting, which keep the style of
/// the screen being left. PreparingLauncher installs the launcher style itself: it can be the
/// window's FIRST screen, with no style installed before it (stock egui visuals otherwise). Closing
/// and Exiting are never first: Closing is entered only from the studio, Exiting only from a screen
/// that installed a style.
#[must_use]
pub(crate) fn style_screen(kind: &ScreenKind) -> Option<StyleScreen> {
    match kind {
        ScreenKind::PreparingLauncher | ScreenKind::Launcher => Some(StyleScreen::Launcher),
        ScreenKind::Studio(_) => Some(StyleScreen::Studio),
        ScreenKind::Closing { .. } | ScreenKind::Exiting { .. } => None,
    }
}

/// Minimum inner size of the main window on the screen, in logical points; `None` for the
/// transitional screens (the previous minimum stays).
#[must_use]
pub(crate) fn min_inner_size(kind: &ScreenKind) -> Option<[f32; 2]> {
    match kind {
        ScreenKind::PreparingLauncher | ScreenKind::Launcher => Some(LAUNCHER_MIN_INNER_SIZE),
        ScreenKind::Studio(_) => Some(STUDIO_MIN_INNER_SIZE),
        ScreenKind::Closing { .. } | ScreenKind::Exiting { .. } => None,
    }
}

/// Minimum inner size of the launcher screen (the menu card and the subpages need it).
pub(crate) const LAUNCHER_MIN_INNER_SIZE: [f32; 2] = [980.0, 680.0];
/// Minimum inner size of the studio screen.
pub(crate) const STUDIO_MIN_INNER_SIZE: [f32; 2] = [900.0, 600.0];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launcher::state::{LauncherOutcome, OpenProjectSelection};

    fn loading(close_after_load: bool) -> ScreenKind {
        ScreenKind::Studio(StudioPhase::Loading { close_after_load })
    }

    fn running() -> ScreenKind {
        ScreenKind::Studio(StudioPhase::Running)
    }

    fn failed() -> ScreenKind {
        ScreenKind::Studio(StudioPhase::Failed)
    }

    fn closing(then: AfterClose) -> ScreenKind {
        ScreenKind::Closing { then }
    }

    fn exiting(exit: ShellExit) -> ScreenKind {
        ScreenKind::Exiting { exit }
    }

    fn reload_dir() -> PathBuf {
        PathBuf::from("/home/u/titles/Title/Chapter 1")
    }

    fn every_kind() -> Vec<ScreenKind> {
        vec![
            ScreenKind::PreparingLauncher,
            ScreenKind::Launcher,
            loading(false),
            loading(true),
            failed(),
            running(),
            closing(AfterClose::Launcher),
            closing(AfterClose::Reload { project_dir: reload_dir(), tab: AppTab::PageManager }),
            closing(AfterClose::Exit),
            exiting(ShellExit::Quit),
            exiting(ShellExit::StartUpdate),
        ]
    }

    fn assert_row(kind: &ScreenKind, event: ShellEvent, next: ScreenKind, effect: Effect, cancel: bool) {
        let transition = on_event(kind, event.clone()).unwrap_or_else(|| panic!("{event:?} must apply to {kind:?}"));
        assert_eq!(transition, Transition { next, effect, cancel_os_close: cancel }, "{kind:?} + {event:?}");
    }

    #[test]
    fn launcher_rows() {
        assert_row(&ScreenKind::PreparingLauncher, ShellEvent::LauncherPrepared, ScreenKind::Launcher, Effect::ShowLauncher, false);
        let target = StartupTarget::Project(reload_dir());
        assert_row(
            &ScreenKind::Launcher,
            ShellEvent::LauncherExit(LauncherExit::Open(target.clone())),
            loading(false),
            Effect::LoadStudio { target, tab: None },
            false,
        );
        let image = StartupTarget::Image(PathBuf::from("/home/u/a.png"));
        assert_row(
            &ScreenKind::Launcher,
            ShellEvent::LauncherExit(LauncherExit::Open(image.clone())),
            loading(false),
            Effect::LoadStudio { target: image, tab: None },
            false,
        );
        assert_row(&ScreenKind::Launcher, ShellEvent::LauncherExit(LauncherExit::StartUpdate), exiting(ShellExit::StartUpdate), Effect::CloseWindow, false);
        assert_row(&ScreenKind::Launcher, ShellEvent::LauncherExit(LauncherExit::Quit), exiting(ShellExit::Quit), Effect::CloseWindow, false);
    }

    #[test]
    fn studio_load_rows() {
        assert_row(&loading(false), ShellEvent::StudioLoaded, running(), Effect::StartStudio, false);
        assert_row(&loading(false), ShellEvent::StudioLoadFailed, failed(), Effect::ShowLoadError, false);
        // A close that arrived during the load wins over both results: tear down, then exit.
        assert_row(&loading(true), ShellEvent::StudioLoaded, closing(AfterClose::Exit), Effect::Shutdown, false);
        assert_row(&loading(true), ShellEvent::StudioLoadFailed, closing(AfterClose::Exit), Effect::Shutdown, false);
    }

    #[test]
    fn studio_close_rows() {
        assert_row(&running(), ShellEvent::StudioClose(PendingCloseAction::Exit), closing(AfterClose::Exit), Effect::Shutdown, false);
        assert_row(&running(), ShellEvent::StudioClose(PendingCloseAction::ReturnToLauncher), closing(AfterClose::Launcher), Effect::Shutdown, false);
        assert_row(
            &running(),
            ShellEvent::StudioReload { project_dir: reload_dir() },
            closing(AfterClose::Reload { project_dir: reload_dir(), tab: AppTab::PageManager }),
            Effect::Shutdown,
            false,
        );
        assert_row(&failed(), ShellEvent::FailedBack, closing(AfterClose::Launcher), Effect::Shutdown, false);
        assert_row(&failed(), ShellEvent::FailedExit, closing(AfterClose::Exit), Effect::Shutdown, false);
    }

    #[test]
    fn shutdown_finished_rows() {
        assert_row(&closing(AfterClose::Launcher), ShellEvent::ShutdownFinished, ScreenKind::PreparingLauncher, Effect::PrepareLauncher, false);
        assert_row(
            &closing(AfterClose::Reload { project_dir: reload_dir(), tab: AppTab::PageManager }),
            ShellEvent::ShutdownFinished,
            loading(false),
            Effect::LoadStudio { target: StartupTarget::Project(reload_dir()), tab: Some(AppTab::PageManager) },
            false,
        );
        assert_row(&closing(AfterClose::Exit), ShellEvent::ShutdownFinished, exiting(ShellExit::Quit), Effect::CloseWindow, false);
    }

    #[test]
    fn os_close_policy_rows() {
        for allowed in [ScreenKind::PreparingLauncher, ScreenKind::Launcher, failed()] {
            assert_row(&allowed, ShellEvent::OsClose, exiting(ShellExit::Quit), Effect::None, false);
        }
        assert_row(&loading(false), ShellEvent::OsClose, loading(true), Effect::None, true);
        assert_row(&loading(true), ShellEvent::OsClose, loading(true), Effect::None, true);
        assert_row(&running(), ShellEvent::OsClose, running(), Effect::RequestOsClose, true);
        for then in [AfterClose::Launcher, AfterClose::Reload { project_dir: reload_dir(), tab: AppTab::PageManager }, AfterClose::Exit] {
            assert_row(&closing(then), ShellEvent::OsClose, closing(AfterClose::Exit), Effect::None, true);
        }
        // The shell's own `Close` comes back as an OS close: never cancel it.
        for exit in [ShellExit::Quit, ShellExit::StartUpdate] {
            assert_row(&exiting(exit), ShellEvent::OsClose, exiting(exit), Effect::None, false);
        }
    }

    /// Every (screen, event) pair outside the table is rejected, so a stray event can never move
    /// the window: exactly the rows above apply.
    #[test]
    fn events_outside_the_table_do_not_apply() {
        let events = [
            ShellEvent::LauncherPrepared,
            ShellEvent::LauncherExit(LauncherExit::Quit),
            ShellEvent::StudioLoaded,
            ShellEvent::StudioLoadFailed,
            ShellEvent::StudioClose(PendingCloseAction::Exit),
            ShellEvent::StudioReload { project_dir: reload_dir() },
            ShellEvent::FailedBack,
            ShellEvent::FailedExit,
            ShellEvent::ShutdownFinished,
            ShellEvent::OsClose,
        ];
        let applies = |kind: &ScreenKind, event: &ShellEvent| -> bool {
            matches!(
                (kind, event),
                (ScreenKind::PreparingLauncher, ShellEvent::LauncherPrepared)
                    | (ScreenKind::Launcher, ShellEvent::LauncherExit(_))
                    | (ScreenKind::Studio(StudioPhase::Loading { .. }), ShellEvent::StudioLoaded | ShellEvent::StudioLoadFailed)
                    | (ScreenKind::Studio(StudioPhase::Running), ShellEvent::StudioClose(_) | ShellEvent::StudioReload { .. })
                    | (ScreenKind::Studio(StudioPhase::Failed), ShellEvent::FailedBack | ShellEvent::FailedExit)
                    | (ScreenKind::Closing { .. }, ShellEvent::ShutdownFinished)
                    | (_, ShellEvent::OsClose)
            )
        };
        for kind in every_kind() {
            for event in &events {
                assert_eq!(on_event(&kind, event.clone()).is_some(), applies(&kind, event), "{kind:?} + {event:?}");
            }
        }
    }

    #[test]
    fn launcher_outcomes_map_to_exits() {
        let selection = OpenProjectSelection { project_dir: reload_dir(), title: "Title".to_string(), chapter: "Chapter 1".to_string(), resume_unsaved: false };
        assert_eq!(launcher_exit_for(LauncherOutcome::OpenProject(selection.clone()), false), LauncherExit::Open(StartupTarget::Project(reload_dir())));
        assert_eq!(
            launcher_exit_for(LauncherOutcome::OpenImage(PathBuf::from("/home/u/a.png")), false),
            LauncherExit::Open(StartupTarget::Image(PathBuf::from("/home/u/a.png")))
        );
        assert_eq!(launcher_exit_for(LauncherOutcome::StartUpdate, false), LauncherExit::StartUpdate);
        for outcome in [LauncherOutcome::OpenProject(selection), LauncherOutcome::OpenImage(PathBuf::from("/home/u/a.png")), LauncherOutcome::StartUpdate] {
            assert_eq!(launcher_exit_for(outcome, true), LauncherExit::Quit);
        }
    }

    #[test]
    fn per_screen_style_and_minimum_size() {
        assert_eq!(style_screen(&ScreenKind::Launcher), Some(StyleScreen::Launcher));
        assert_eq!(style_screen(&running()), Some(StyleScreen::Studio));
        assert_eq!(style_screen(&loading(false)), Some(StyleScreen::Studio));
        // The first screen of a launcher start must install a style itself.
        assert_eq!(style_screen(&ScreenKind::PreparingLauncher), Some(StyleScreen::Launcher));
        assert_eq!(style_screen(&exiting(ShellExit::Quit)), None);
        assert_eq!(style_screen(&closing(AfterClose::Exit)), None);
        assert_eq!(min_inner_size(&ScreenKind::Launcher), Some(LAUNCHER_MIN_INNER_SIZE));
        assert_eq!(min_inner_size(&failed()), Some(STUDIO_MIN_INNER_SIZE));
        assert_eq!(min_inner_size(&exiting(ShellExit::Quit)), None);
    }
}
