/*
FILE OVERVIEW: src/startup/mode.rs
The ONE decision of which run mode this process takes, made once right after the command line is
parsed: `decide(&Cli, PlatformFacts) -> RunMode`. Pure (no I/O, no logging, no globals), so the
whole precedence table is unit-tested here; `run_native` (`mod.rs`) only matches on the result
and performs each mode's side effects in their documented order.

Key items:
- `RunMode` = `Service(ServiceMode)` (ends the process or returns without the main window) or
  `Main(MainRun)` (optional update-window preludes, then the test launcher or the main window).
- `ServiceMode`: usage error (`UsageConflicts`), elevated system-registration helper, the
  Stable-Diffusion.cpp worker process, Windows Start Menu shortcut, Windows uninstall,
  `--check-venv`, refused self-update.
- `PlatformFacts`: the platform inputs of the decision (`current()` reads them for this build).

Notes:
No flag is silently dropped: Windows-only service flags off Windows and `--project` with
`--test-launcher` / `--check-venv` are usage errors, and `--update` / `--continue-update` without
`InstalledCopyManagement` are refused like a launcher-requested update
(`routing::run_launcher_requested_update`). Each rule is pinned by named table rows.
*/

use crate::args::{self, Cli};
use ms_platform::PlatformFeature;
use std::path::PathBuf;

/// The flag combinations that make the command line a usage error, as the lists the
/// diagnostics name (`args::conflicting_installed_copy_flags`, `args::conflicting_image_flags`,
/// `args::conflicting_project_flags`, and `args::windows_only_flags` off Windows). At least one
/// list is non-empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UsageConflicts {
    /// Installed-copy flags given together with `--ignore-installed`.
    pub installed_copy: Vec<&'static str>,
    /// Terminal-mode or installed-copy flags given together with an image to open.
    pub image: Vec<&'static str>,
    /// Terminal-mode flags given together with `--project`.
    pub project: Vec<&'static str>,
    /// Windows-only service flags given on a build that does not act on them.
    pub windows_only: Vec<&'static str>,
}

/// A mode that ends the process (exit code) or returns without opening the main window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ServiceMode {
    /// Contradictory flags: report them and exit with the usage-error code.
    UsageError(UsageConflicts),
    /// `--system-registration-apply` (+ result file): the elevated helper; off Windows it
    /// reports "unsupported" itself (`service_flags::run_system_registration_helper`).
    RegistrationHelper,
    /// `--sdcpp-worker`: the Stable-Diffusion.cpp worker process the app starts for itself
    /// (`ms_sdcpp::worker::run_worker_main`); it serves stdin / stdout frames and exits.
    SdCppWorker,
    /// `--create-start-menu-shortcut-install-dir` (Windows only).
    WindowsStartMenuShortcut {
        /// The installed copy's directory.
        install_dir: PathBuf,
        /// `--continue-create-start-menu-shortcut`: this is the elevated continuation.
        continuation: bool,
    },
    /// `--uninstall` (Windows only).
    WindowsUninstall {
        /// `--continue-uninstall`: this is the elevated continuation.
        continuation: bool,
        /// `--uninstall-signal-file`: where the outcome is signalled to a reinstall flow.
        signal_file: Option<PathBuf>,
    },
    /// `--check-venv`: check (and possibly repair) the Python environment, then exit.
    CheckVenv,
    /// `--update` / `--continue-update` on a build without `InstalledCopyManagement`: refused
    /// (logged) and the process ends without any window, like a launcher-requested update on
    /// such a build (`routing::run_launcher_requested_update`).
    SelfUpdateRefused,
}

/// An update window run before the main mode; its `Exit` outcome ends the process, any other
/// outcome falls through to the next prelude or the main mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UpdatePrelude {
    /// `--continue-update`: resume update work after the executable was replaced.
    ContinueUpdate,
    /// `--update`: the update window.
    Update,
}

/// What the main run ends in after its preludes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MainMode {
    /// `--test-launcher`: the main window in test mode (own title / app id, any exit quits).
    TestLauncher,
    /// The main window (startup prompts, launcher or studio), then a launcher-requested update.
    Main,
}

/// The main run: `preludes` in order (both run when both flags are given), then `main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MainRun {
    pub preludes: Vec<UpdatePrelude>,
    pub main: MainMode,
}

/// The run mode of this process, decided once by [`decide`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RunMode {
    Service(ServiceMode),
    Main(MainRun),
}

/// The platform inputs of [`decide`]; tests pass every combination, `run_native` passes
/// [`PlatformFacts::current`].
#[derive(Clone, Copy, Debug)]
pub(super) struct PlatformFacts {
    /// The Windows-only service flags (Start Menu shortcut, uninstall) are acted on.
    pub windows_service_flags: bool,
    /// `PlatformFeature::InstalledCopyManagement` (installer, self-update, update check).
    pub installed_copy_management: bool,
}

impl PlatformFacts {
    /// The facts of the running build. Pure: `is_available` is a compile-time table.
    #[must_use]
    pub(super) fn current() -> Self {
        Self {
            windows_service_flags: cfg!(target_os = "windows"),
            installed_copy_management: PlatformFeature::InstalledCopyManagement.is_available(),
        }
    }
}

/// Decides the run mode of a parsed command line. First match wins:
/// 1. contradictory flags -> `UsageError`: installed-copy flags with `--ignore-installed`, an
///    image or `--project` with a terminal mode (an image also with installed-copy flags), and
///    any Windows-only service flag on a build without `windows_service_flags`;
/// 2. `--system-registration-apply` -> `RegistrationHelper` (on every OS), then
///    `--sdcpp-worker` -> `SdCppWorker` (on every OS; a build without sd.cpp reports it);
/// 3. (Windows only, step 1 rejected them elsewhere) `--create-start-menu-shortcut-install-dir`
///    -> `WindowsStartMenuShortcut`, then `--uninstall` -> `WindowsUninstall`;
/// 4. `--check-venv` -> `CheckVenv`;
/// 5. `--update` / `--continue-update` without `installed_copy_management` -> `SelfUpdateRefused`;
/// 6. otherwise `Main`: preludes `--continue-update` then `--update`, then `--test-launcher` or
///    the main window.
#[must_use]
pub(super) fn decide(cli: &Cli, platform: PlatformFacts) -> RunMode {
    let installed_copy = args::conflicting_installed_copy_flags(cli);
    let image = args::conflicting_image_flags(cli);
    let project = args::conflicting_project_flags(cli);
    let windows_only = if platform.windows_service_flags { Vec::new() } else { args::windows_only_flags(cli) };
    if !installed_copy.is_empty() || !image.is_empty() || !project.is_empty() || !windows_only.is_empty() {
        return RunMode::Service(ServiceMode::UsageError(UsageConflicts { installed_copy, image, project, windows_only }));
    }
    if cli.system_registration_apply.is_some() {
        return RunMode::Service(ServiceMode::RegistrationHelper);
    }
    if cli.sdcpp_worker {
        return RunMode::Service(ServiceMode::SdCppWorker);
    }
    if platform.windows_service_flags {
        if let Some(install_dir) = &cli.create_start_menu_shortcut_install_dir {
            return RunMode::Service(ServiceMode::WindowsStartMenuShortcut { install_dir: install_dir.clone(), continuation: cli.continue_create_start_menu_shortcut });
        }
        if cli.uninstall {
            return RunMode::Service(ServiceMode::WindowsUninstall { continuation: cli.continue_uninstall, signal_file: cli.uninstall_signal_file.clone() });
        }
    }
    if cli.check_venv {
        return RunMode::Service(ServiceMode::CheckVenv);
    }
    if (cli.update || cli.continue_update) && !platform.installed_copy_management {
        return RunMode::Service(ServiceMode::SelfUpdateRefused);
    }
    let mut preludes = Vec::with_capacity(2);
    if cli.continue_update {
        preludes.push(UpdatePrelude::ContinueUpdate);
    }
    if cli.update {
        preludes.push(UpdatePrelude::Update);
    }
    let main = if cli.test_launcher { MainMode::TestLauncher } else { MainMode::Main };
    RunMode::Main(MainRun { preludes, main })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    const WINDOWS: PlatformFacts = PlatformFacts { windows_service_flags: true, installed_copy_management: true };
    const DESKTOP_OTHER: PlatformFacts = PlatformFacts { windows_service_flags: false, installed_copy_management: true };
    const NO_INSTALLED_COPY: PlatformFacts = PlatformFacts { windows_service_flags: false, installed_copy_management: false };

    fn main_run(preludes: &[UpdatePrelude], main: MainMode) -> RunMode {
        RunMode::Main(MainRun { preludes: preludes.to_vec(), main })
    }

    fn usage(installed_copy: &[&'static str], image: &[&'static str]) -> RunMode {
        usage_all(installed_copy, image, &[], &[])
    }

    fn usage_all(installed_copy: &[&'static str], image: &[&'static str], project: &[&'static str], windows_only: &[&'static str]) -> RunMode {
        RunMode::Service(ServiceMode::UsageError(UsageConflicts { installed_copy: installed_copy.to_vec(), image: image.to_vec(), project: project.to_vec(), windows_only: windows_only.to_vec() }))
    }

    /// One row per mode and per pinned behaviour: (name, extra argv, platform, expected).
    fn rows() -> Vec<(&'static str, Vec<&'static str>, PlatformFacts, RunMode)> {
        use MainMode::{Main, TestLauncher};
        use UpdatePrelude::{ContinueUpdate, Update};
        let shortcut = RunMode::Service(ServiceMode::WindowsStartMenuShortcut { install_dir: PathBuf::from("/home/u/MS"), continuation: true });
        vec![
            ("main_no_flags", vec![], DESKTOP_OTHER, main_run(&[], Main)),
            ("main_no_flags_windows", vec![], WINDOWS, main_run(&[], Main)),
            ("main_with_project", vec!["--project", "/home/u/ch1"], DESKTOP_OTHER, main_run(&[], Main)),
            ("main_with_images", vec!["/home/u/a.png", "/home/u/b.png"], DESKTOP_OTHER, main_run(&[], Main)),
            ("usage_error_ignore_installed_with_update", vec!["--ignore-installed", "--update"], DESKTOP_OTHER, usage(&["--update"], &[])),
            ("usage_error_image_with_check_venv", vec!["--image", "/home/u/a.png", "--check-venv"], DESKTOP_OTHER, usage(&[], &["--check-venv"])),
            ("usage_error_beats_registration_helper", vec!["--ignore-installed", "--system-registration-apply", "x", "--system-registration-result", "/home/u/r.json"], WINDOWS, usage(&["--system-registration-apply", "--system-registration-result"], &[])),
            ("registration_helper_windows", vec!["--system-registration-apply", "x", "--system-registration-result", "/home/u/r.json"], WINDOWS, RunMode::Service(ServiceMode::RegistrationHelper)),
            ("registration_helper_other_os", vec!["--system-registration-apply", "x", "--system-registration-result", "/home/u/r.json"], DESKTOP_OTHER, RunMode::Service(ServiceMode::RegistrationHelper)),
            ("registration_helper_beats_windows_flags", vec!["--system-registration-apply", "x", "--system-registration-result", "/home/u/r.json", "--uninstall"], WINDOWS, RunMode::Service(ServiceMode::RegistrationHelper)),
            ("sdcpp_worker", vec!["--sdcpp-worker"], DESKTOP_OTHER, RunMode::Service(ServiceMode::SdCppWorker)),
            ("sdcpp_worker_windows", vec!["--sdcpp-worker"], WINDOWS, RunMode::Service(ServiceMode::SdCppWorker)),
            ("registration_helper_beats_sdcpp_worker", vec!["--system-registration-apply", "x", "--system-registration-result", "/home/u/r.json", "--sdcpp-worker"], DESKTOP_OTHER, RunMode::Service(ServiceMode::RegistrationHelper)),
            ("usage_error_project_with_sdcpp_worker", vec!["--sdcpp-worker", "--project", "/home/u/ch1"], DESKTOP_OTHER, usage_all(&[], &[], &["--sdcpp-worker"], &[])),
            ("windows_start_menu_shortcut", vec!["--create-start-menu-shortcut-install-dir", "/home/u/MS", "--continue-create-start-menu-shortcut"], WINDOWS, shortcut.clone()),
            ("windows_shortcut_beats_uninstall", vec!["--create-start-menu-shortcut-install-dir", "/home/u/MS", "--continue-create-start-menu-shortcut", "--uninstall"], WINDOWS, shortcut),
            ("windows_uninstall", vec!["--uninstall", "--continue-uninstall", "--uninstall-signal-file", "/home/u/sig"], WINDOWS, RunMode::Service(ServiceMode::WindowsUninstall { continuation: true, signal_file: Some(PathBuf::from("/home/u/sig")) })),
            ("windows_uninstall_beats_check_venv", vec!["--uninstall", "--check-venv"], WINDOWS, RunMode::Service(ServiceMode::WindowsUninstall { continuation: false, signal_file: None })),
            // A Windows-only flag off Windows is a usage error, never silently ignored.
            ("usage_error_windows_shortcut_off_windows", vec!["--create-start-menu-shortcut-install-dir", "/home/u/MS"], DESKTOP_OTHER, usage_all(&[], &[], &[], &["--create-start-menu-shortcut-install-dir"])),
            ("usage_error_windows_uninstall_off_windows", vec!["--uninstall", "--check-venv"], DESKTOP_OTHER, usage_all(&[], &[], &[], &["--uninstall"])),
            ("usage_error_windows_continuations_off_windows", vec!["--continue-uninstall", "--uninstall-signal-file", "/home/u/sig", "--continue-create-start-menu-shortcut"], NO_INSTALLED_COPY, usage_all(&[], &[], &[], &["--continue-uninstall", "--uninstall-signal-file", "--continue-create-start-menu-shortcut"])),
            ("windows_continuation_flag_alone_is_no_usage_error_on_windows", vec!["--continue-uninstall"], WINDOWS, main_run(&[], Main)),
            ("usage_error_lists_every_conflict", vec!["--ignore-installed", "--uninstall", "--project", "/home/u/ch1", "--test-launcher"], DESKTOP_OTHER, usage_all(&["--uninstall"], &[], &["--test-launcher"], &["--uninstall"])),
            ("check_venv", vec!["--check-venv"], DESKTOP_OTHER, RunMode::Service(ServiceMode::CheckVenv)),
            ("check_venv_beats_update_and_test_launcher", vec!["--check-venv", "--update", "--test-launcher"], DESKTOP_OTHER, RunMode::Service(ServiceMode::CheckVenv)),
            // `--project` with a terminal mode is a usage error, never silently dropped.
            ("usage_error_project_with_check_venv", vec!["--check-venv", "--project", "/home/u/ch1"], DESKTOP_OTHER, usage_all(&[], &[], &["--check-venv"], &[])),
            ("usage_error_project_with_test_launcher", vec!["--test-launcher", "--project", "/home/u/ch1"], WINDOWS, usage_all(&[], &[], &["--test-launcher"], &[])),
            ("project_with_update_runs_update_then_main", vec!["--project", "/home/u/ch1", "--update"], DESKTOP_OTHER, main_run(&[Update], Main)),
            ("test_launcher", vec!["--test-launcher"], DESKTOP_OTHER, main_run(&[], TestLauncher)),
            ("continue_update_then_main", vec!["--continue-update"], DESKTOP_OTHER, main_run(&[ContinueUpdate], Main)),
            ("update_then_main", vec!["--update"], DESKTOP_OTHER, main_run(&[Update], Main)),
            ("both_updates_in_order_then_test_launcher", vec!["--update", "--continue-update", "--test-launcher"], DESKTOP_OTHER, main_run(&[ContinueUpdate, Update], TestLauncher)),
            // Self-update without `InstalledCopyManagement` is refused (no window at all).
            ("update_refused_without_installed_copy_management", vec!["--update"], NO_INSTALLED_COPY, RunMode::Service(ServiceMode::SelfUpdateRefused)),
            ("continue_update_refused_without_installed_copy_management", vec!["--continue-update"], NO_INSTALLED_COPY, RunMode::Service(ServiceMode::SelfUpdateRefused)),
            ("update_refusal_beats_test_launcher", vec!["--update", "--test-launcher"], NO_INSTALLED_COPY, RunMode::Service(ServiceMode::SelfUpdateRefused)),
            ("check_venv_beats_update_refusal", vec!["--check-venv", "--update"], NO_INSTALLED_COPY, RunMode::Service(ServiceMode::CheckVenv)),
            ("main_without_installed_copy_management", vec!["--test-launcher"], NO_INSTALLED_COPY, main_run(&[], TestLauncher)),
        ]
    }

    #[test]
    fn decide_matches_the_mode_table() {
        for (name, argv, platform, expected) in rows() {
            let cli = Cli::try_parse_from(std::iter::once("manhwastudio_rs").chain(argv)).unwrap_or_else(|err| panic!("row {name}: command line must parse: {err}"));
            assert_eq!(decide(&cli, platform), expected, "row {name}");
        }
    }

    #[test]
    fn current_facts_follow_the_build() {
        let facts = PlatformFacts::current();
        assert_eq!(facts.windows_service_flags, cfg!(target_os = "windows"));
        assert_eq!(facts.installed_copy_management, PlatformFeature::InstalledCopyManagement.is_available());
    }
}
