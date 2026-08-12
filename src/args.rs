/*
FILE OVERVIEW: src/args.rs
CLI argument parsing for the main Rust app.

Main items:
- `Cli.project`: optional path to chapter/project directory.
- `Cli.no_ai`: disables AI-dependent functionality at startup.
- `Cli.update`: opens the Rust update window directly.
- `Cli.test_launcher`: starts the new Rust launcher test mode instead of the main app.
- `Cli.test_ver_check`: forces update checks to report an available update in launcher/update UI.
- `Cli.check_venv`: verifies the managed Python environment and exits; opens the installer in
  environment-repair mode only when something is missing (see `src/venv_check.rs`).
- `Cli.ignore_installed`: run-from-sources mode — never touches or competes with an installed copy
  (no existing-install discovery, no Linux desktop entry, isolated backend socket, self-update off).
- `conflicting_installed_copy_flags`: the single validation of flag combinations that contradict
  `--ignore-installed`; startup must reject them before any service action runs.
- `Cli.continue_install`: скрытый служебный флаг продолжения установки после elevation.
- `Cli.continue_install_target`: скрытый служебный путь установки для continuation.
- `Cli.uninstall`: скрытый Windows-флаг удаления установленной копии приложения.
- `Cli.continue_uninstall`: скрытый служебный флаг продолжения удаления после elevation.
- `Cli.create_start_menu_shortcut_install_dir`: скрытый служебный путь установки для elevated-создания ярлыка меню Пуск.
- `Cli.continue_create_start_menu_shortcut`: скрытый служебный флаг продолжения elevated-создания ярлыка меню Пуск.
- `Cli.uninstall_signal_file`: скрытый служебный файл-сигнал для сценария "удалить и затем переустановить".
- `Cli.continue_update`: hidden service flag that resumes update work after executable replacement.
- `Cli.trace`: enables detailed execution tracing to `trace-last.log` (see `src/trace.rs`).
*/

use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Minimal Rust project viewer for MangaFucker projects"
)]
pub struct Cli {
    #[arg(long, value_name = "PATH")]
    pub project: Option<PathBuf>,

    #[arg(long, default_value_t = false)]
    pub no_ai: bool,

    #[arg(long, default_value_t = false)]
    pub update: bool,

    #[arg(long, default_value_t = false)]
    pub test_launcher: bool,

    #[arg(long, default_value_t = false)]
    pub test_ver_check: bool,

    /// Check the managed Python environment and exit without opening any window when it is
    /// complete; otherwise open the installer in environment-repair mode. Exit code 0 means
    /// "environment ready", 1 means "still not ready" (see `crate::venv_check`).
    #[arg(long, default_value_t = false)]
    pub check_venv: bool,

    /// Running from a source checkout: do not discover, modify, launch or compete with an
    /// installed copy of the program (no Linux desktop entry, no existing-install prompts,
    /// per-root backend socket, self-update refused).
    #[arg(long, default_value_t = false)]
    pub ignore_installed: bool,

    #[arg(long, default_value_t = false, hide = true)]
    pub continue_install: bool,

    #[arg(long, value_name = "PATH", hide = true)]
    pub continue_install_target: Option<PathBuf>,

    #[arg(long, default_value_t = false, hide = true)]
    pub uninstall: bool,

    #[arg(long, default_value_t = false, hide = true)]
    pub continue_uninstall: bool,

    #[arg(long, value_name = "PATH", hide = true)]
    pub create_start_menu_shortcut_install_dir: Option<PathBuf>,

    #[arg(long, default_value_t = false, hide = true)]
    pub continue_create_start_menu_shortcut: bool,

    #[arg(long, value_name = "PATH", hide = true)]
    pub uninstall_signal_file: Option<PathBuf>,

    #[arg(long, default_value_t = false, hide = true)]
    pub continue_update: bool,

    #[arg(long, default_value_t = false)]
    pub trace: bool,
}

/// Startup flags that manage an INSTALLED copy of the program: they install into it,
/// update it, uninstall it, or rewrite its desktop/Start Menu integration.
///
/// Every one of them is incompatible with `--ignore-installed`, whose whole contract is
/// "this process never touches an installed copy". They are listed here — rather than as
/// clap `conflicts_with` attributes — because most of them are HIDDEN service flags:
/// keeping one runtime check means one diagnostic and one place to extend, instead of
/// clap's English usage error for the visible flag and a separate check for the rest.
const INSTALLED_COPY_FLAGS: &[&str] = &[
    "--update",
    "--continue-update",
    "--continue-install",
    "--continue-install-target",
    "--uninstall",
    "--continue-uninstall",
    "--create-start-menu-shortcut-install-dir",
    "--continue-create-start-menu-shortcut",
    "--uninstall-signal-file",
];

/// Returns the [`INSTALLED_COPY_FLAGS`] present in `cli` that contradict
/// `--ignore-installed`, in declaration order.
///
/// The result is empty when the combination is valid — including every case where
/// `--ignore-installed` is absent, since those flags are legitimate on their own. A
/// non-empty result must abort startup BEFORE any service action runs: several of these
/// flags act immediately (uninstall, shortcut creation, update continuation) and would
/// otherwise modify or delete an installed copy the caller asked to leave alone.
#[must_use]
pub fn conflicting_installed_copy_flags(cli: &Cli) -> Vec<&'static str> {
    if !cli.ignore_installed {
        return Vec::new();
    }
    // Kept in the same order as `INSTALLED_COPY_FLAGS` so the diagnostic is stable.
    let present = [
        cli.update,
        cli.continue_update,
        cli.continue_install,
        cli.continue_install_target.is_some(),
        cli.uninstall,
        cli.continue_uninstall,
        cli.create_start_menu_shortcut_install_dir.is_some(),
        cli.continue_create_start_menu_shortcut,
        cli.uninstall_signal_file.is_some(),
    ];
    INSTALLED_COPY_FLAGS
        .iter()
        .zip(present)
        .filter_map(|(flag, is_present)| is_present.then_some(*flag))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a command line the same way startup does, so the test also pins the
    /// actual flag spelling clap accepts.
    fn parse(args: &[&str]) -> Cli {
        let mut argv = vec!["manhwastudio_rs"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("test command line must parse")
    }

    #[test]
    fn installed_copy_flags_are_allowed_without_ignore_installed() {
        assert!(conflicting_installed_copy_flags(&parse(&["--update"])).is_empty());
        assert!(conflicting_installed_copy_flags(&parse(&["--uninstall"])).is_empty());
        assert!(conflicting_installed_copy_flags(&parse(&[])).is_empty());
    }

    #[test]
    fn a_plain_source_run_has_no_conflict() {
        let cli = parse(&["--ignore-installed", "--check-venv", "--no-ai", "--trace"]);
        assert!(conflicting_installed_copy_flags(&cli).is_empty());
    }

    #[test]
    fn every_installed_copy_flag_conflicts_with_ignore_installed() {
        // Each flag is checked on its own so a missed field in the mapping fails here
        // instead of being masked by another flag in the same command line.
        let cases: &[(&[&str], &str)] = &[
            (&["--update"], "--update"),
            (&["--continue-update"], "--continue-update"),
            (&["--continue-install"], "--continue-install"),
            (
                &["--continue-install-target", "/tmp/x"],
                "--continue-install-target",
            ),
            (&["--uninstall"], "--uninstall"),
            (&["--continue-uninstall"], "--continue-uninstall"),
            (
                &["--create-start-menu-shortcut-install-dir", "/tmp/x"],
                "--create-start-menu-shortcut-install-dir",
            ),
            (
                &["--continue-create-start-menu-shortcut"],
                "--continue-create-start-menu-shortcut",
            ),
            (&["--uninstall-signal-file", "/tmp/x"], "--uninstall-signal-file"),
        ];
        for (args, expected) in cases {
            let mut argv = vec!["--ignore-installed"];
            argv.extend_from_slice(args);
            assert_eq!(
                conflicting_installed_copy_flags(&parse(&argv)),
                vec![*expected],
                "flag {expected} must be rejected together with --ignore-installed"
            );
        }
        assert_eq!(
            INSTALLED_COPY_FLAGS.len(),
            cases.len(),
            "every flag in INSTALLED_COPY_FLAGS needs a case here"
        );
    }

    #[test]
    fn several_conflicts_are_all_reported() {
        let cli = parse(&["--ignore-installed", "--update", "--continue-update"]);
        assert_eq!(
            conflicting_installed_copy_flags(&cli),
            vec!["--update", "--continue-update"]
        );
    }
}
