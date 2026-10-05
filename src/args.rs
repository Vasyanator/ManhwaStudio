/*
FILE OVERVIEW: src/args.rs
CLI argument parsing for the main Rust app.

Main items:
- `Cli.project`: optional path to chapter/project directory.
- `Cli.image` / `Cli.image_positional`: one picture file to open in single-image mode
  (`--image <PATH>` or a bare positional path, the form OS "Open with" passes). Mutually
  exclusive with `--project` (clap group `open_target`); `Cli::image_arg` merges the two.
- `conflicting_image_flags` / `check_image_arg`: the image start's flag-combination rule and its
  path pre-check (missing, directory, not a regular file), run before any window exists.
- `Cli::clear_open_target`: forgets the CLI open target when the studio returns to the launcher.
- `Cli.no_ai`: disables AI-dependent functionality at startup.
- `Cli.update`: opens the Rust update window directly.
- `Cli.test_launcher`: starts the new Rust launcher test mode instead of the main app.
- `Cli.test_ver_check`: forces update checks to report an available update in launcher/update UI.
- `Cli.check_venv`: verifies the managed Python environment and exits; opens the installer in
  environment-repair mode only when something is missing (see `crates/ms-installer/src/venv_check.rs`).
- `Cli.ignore_installed`: run-from-sources mode — never touches or competes with an installed copy
  (no existing-install discovery, no Linux desktop entry, isolated backend socket, self-update off).
- `conflicting_installed_copy_flags`: the single validation of flag combinations that contradict
  `--ignore-installed`; startup must reject them before any service action runs.
- `standalone_relaunch_args`: builds the command line for relaunching this executable with
  `--ignore-installed`, used by the Windows existing-install window's "run standalone" choice.
- `Cli.continue_install`: скрытый служебный флаг продолжения установки после elevation.
- `Cli.continue_install_target`: скрытый служебный путь установки для continuation.
- `Cli.uninstall`: скрытый Windows-флаг удаления установленной копии приложения.
- `Cli.continue_uninstall`: скрытый служебный флаг продолжения удаления после elevation.
- `Cli.create_start_menu_shortcut_install_dir`: скрытый служебный путь установки для elevated-создания ярлыка меню Пуск.
- `Cli.continue_create_start_menu_shortcut`: скрытый служебный флаг продолжения elevated-создания ярлыка меню Пуск.
- `Cli.uninstall_signal_file`: скрытый служебный файл-сигнал для сценария "удалить и затем переустановить".
- `Cli.continue_update`: hidden service flag that resumes update work after executable replacement.
- `Cli.trace`: enables detailed execution tracing to `trace-last.log` (see `crates/ms-log/src/trace.rs`).

Notes:
`--version` reports the extended, git-derived `MS_APP_VERSION` (see `crates/ms-config/src/version_format.rs`
and `build.rs`), not the plain `CARGO_PKG_VERSION`.
*/

use clap::{ArgGroup, Parser};
#[cfg(any(target_os = "windows", test))]
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

// `--version` prints the EXTENDED, git-derived version (`MS_APP_VERSION`), not clap's
// default `CARGO_PKG_VERSION`: a developer typing it is exactly the reader this string
// serves. The installer also probes an installed copy with `--version`
// (`installer::utils::query_executable_version`), and that consumer compares only the
// semver core through `version_format::version_core`, so the suffix is invisible to it.
#[derive(Debug, Parser)]
#[command(
    author,
    version = env!("MS_APP_VERSION"),
    about = "Minimal Rust project viewer for MangaFucker projects"
)]
// One open target at most: a chapter (`--project`) OR one image (`--image` / positional).
// `ArgGroup::multiple` defaults to false, so clap itself rejects any two of them together.
#[command(group(ArgGroup::new("open_target")))]
pub struct Cli {
    #[arg(long, value_name = "PATH", group = "open_target")]
    pub project: Option<PathBuf>,

    /// Open one image file in single-image mode, skipping the launcher.
    #[arg(long, value_name = "PATH", group = "open_target")]
    pub image: Option<PathBuf>,

    /// Image file to open in single-image mode (same as `--image`). This positional form is
    /// what file managers pass (`.desktop` `%f`, Windows "Open with" `"%1"`); the path is kept
    /// as an `OsString`, so spaces and non-UTF-8 names survive.
    #[arg(value_name = "IMAGE", group = "open_target")]
    pub image_positional: Option<PathBuf>,

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
    INSTALLED_COPY_FLAGS
        .iter()
        .zip(installed_copy_flags_present(cli))
        .filter_map(|(flag, is_present)| is_present.then_some(*flag))
        .collect()
}

/// Presence of every [`INSTALLED_COPY_FLAGS`] entry in `cli`, index-aligned with that table.
fn installed_copy_flags_present(cli: &Cli) -> [bool; INSTALLED_COPY_FLAGS.len()] {
    // Kept in the same order as `INSTALLED_COPY_FLAGS` so the diagnostics are stable; the array
    // length is the table length, so a flag added to one but not the other fails to compile.
    [
        cli.update,
        cli.continue_update,
        cli.continue_install,
        cli.continue_install_target.is_some(),
        cli.uninstall,
        cli.continue_uninstall,
        cli.create_start_menu_shortcut_install_dir.is_some(),
        cli.continue_create_start_menu_shortcut,
        cli.uninstall_signal_file.is_some(),
    ]
}

/// Startup modes that an image start cannot be combined with: each one is its own terminal flow
/// (environment check, launcher test window) and would silently drop the image otherwise.
const IMAGE_EXCLUSIVE_MODE_FLAGS: &[&str] = &["--check-venv", "--test-launcher"];

impl Cli {
    /// The image to open in single-image mode: `--image`, else the positional path. clap's
    /// `open_target` group guarantees at most one of them (and no `--project`) is set.
    #[must_use]
    pub fn image_arg(&self) -> Option<&Path> {
        self.image.as_deref().or(self.image_positional.as_deref())
    }

    /// Forgets the open target given on the command line (`--project`, `--image`, positional
    /// image), so the next startup-loop iteration shows the launcher instead of re-opening it.
    pub fn clear_open_target(&mut self) {
        self.project = None;
        self.image = None;
        self.image_positional = None;
    }
}

/// Returns the flags present in `cli` that contradict an image start, in a stable order:
/// first [`IMAGE_EXCLUSIVE_MODE_FLAGS`], then [`INSTALLED_COPY_FLAGS`]. Empty when no image is
/// given or the combination is valid. A non-empty result must abort startup before any
/// service action runs, for the same reason as [`conflicting_installed_copy_flags`].
#[must_use]
pub fn conflicting_image_flags(cli: &Cli) -> Vec<&'static str> {
    if cli.image_arg().is_none() {
        return Vec::new();
    }
    let modes = [cli.check_venv, cli.test_launcher];
    let installed = installed_copy_flags_present(cli);
    IMAGE_EXCLUSIVE_MODE_FLAGS
        .iter()
        .zip(modes)
        .chain(INSTALLED_COPY_FLAGS.iter().zip(installed))
        .filter_map(|(flag, is_present)| is_present.then_some(*flag))
        .collect()
}

/// Why a command-line image path cannot be opened, found before any window exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageArgProblem {
    /// Nothing exists at the path (or its metadata cannot be read).
    NotFound,
    /// The path is a directory: the user most likely meant `--project`.
    Directory,
    /// The path exists but is neither a file nor a directory (socket, device, ...).
    NotAFile,
}

/// Cheap pre-check of a command-line image path (one `metadata` call, symlinks followed).
/// Decodability is NOT checked here: the studio load worker owns that error
/// (`SingleImageError::user_message` on the loading screen).
///
/// # Errors
/// The [`ImageArgProblem`] that makes the path unusable.
pub fn check_image_arg(path: &Path) -> Result<(), ImageArgProblem> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(meta) if meta.is_dir() => Err(ImageArgProblem::Directory),
        Ok(_) => Err(ImageArgProblem::NotAFile),
        Err(_) => Err(ImageArgProblem::NotFound),
    }
}

/// Spelling of the standalone-mode flag, kept next to the `Cli` field it fills so the two
/// cannot drift apart.
///
/// The `test` arm of the `cfg` exists so the relaunch logic below stays compiled and tested on
/// non-Windows hosts; its only caller is the Windows existing-install window.
#[cfg(any(target_os = "windows", test))]
pub const IGNORE_INSTALLED_FLAG: &str = "--ignore-installed";

/// Builds the argument list for relaunching this executable in standalone mode.
///
/// `original` must be the current process's arguments WITHOUT `argv[0]`. The result is the
/// same list with [`IGNORE_INSTALLED_FLAG`] in FRONT and any pre-existing occurrence of that
/// flag removed, so the relaunched process can never receive it twice — the flag is a plain
/// boolean, and a duplicate would be a usage error rather than a stronger request.
///
/// Nothing else is filtered: dropping a flag the user typed would be as surprising as
/// honoring one they did not. Flags that contradict standalone mode cannot reach here anyway,
/// because startup rejects those combinations before any window opens
/// (see [`conflicting_installed_copy_flags`]) and acts on them long before this one.
#[cfg(any(target_os = "windows", test))]
#[must_use]
pub fn standalone_relaunch_args(original: &[OsString]) -> Vec<OsString> {
    let mut args = Vec::with_capacity(original.len() + 1);
    args.push(OsString::from(IGNORE_INSTALLED_FLAG));
    args.extend(
        original
            .iter()
            .filter(|arg| arg.as_os_str() != OsStr::new(IGNORE_INSTALLED_FLAG))
            .cloned(),
    );
    args
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

    /// Parses a command line that must be REJECTED by clap and returns clap's error kind.
    fn parse_error(args: &[&str]) -> clap::error::ErrorKind {
        let mut argv = vec!["manhwastudio_rs"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv) {
            Ok(cli) => panic!("command line {args:?} must be rejected, parsed as {cli:?}"),
            Err(err) => err.kind(),
        }
    }

    #[test]
    fn image_arguments_parse_into_image_arg() {
        // (command line, expected image) -- the positional form is what file managers pass.
        let cases: &[(&[&str], Option<&str>)] = &[
            (&[], None),
            (&["--project", "/tmp/title/ch"], None),
            (&["--image", "/tmp/a.png"], Some("/tmp/a.png")),
            (&["/tmp/a.png"], Some("/tmp/a.png")),
            (&["--no-ai", "/tmp/a.png", "--trace"], Some("/tmp/a.png")),
            // Windows "Open with" passes `"%1"`: one argument with spaces and non-ASCII text.
            (&["C:\\Users\\Юзер\\My Pictures\\стр 01.jpg"], Some("C:\\Users\\Юзер\\My Pictures\\стр 01.jpg")),
            (&["/home/u/Картинки/page one.webp"], Some("/home/u/Картинки/page one.webp")),
        ];
        for (args, expected) in cases {
            let cli = parse(args);
            assert_eq!(cli.image_arg(), expected.map(Path::new), "command line {args:?}");
        }
    }

    #[test]
    fn open_targets_are_mutually_exclusive() {
        use clap::error::ErrorKind;
        assert_eq!(parse_error(&["--project", "/tmp/ch", "--image", "/tmp/a.png"]), ErrorKind::ArgumentConflict);
        assert_eq!(parse_error(&["--project", "/tmp/ch", "/tmp/a.png"]), ErrorKind::ArgumentConflict);
        assert_eq!(parse_error(&["--image", "/tmp/a.png", "/tmp/b.png"]), ErrorKind::ArgumentConflict);
        // Only one positional exists: a second bare path is a usage error, not a second image.
        assert_eq!(parse_error(&["/tmp/a.png", "/tmp/b.png"]), ErrorKind::UnknownArgument);
    }

    #[test]
    fn image_conflicts_with_terminal_modes_and_installed_copy_flags() {
        assert!(conflicting_image_flags(&parse(&["/tmp/a.png", "--no-ai", "--trace", "--ignore-installed"])).is_empty());
        assert!(
            conflicting_image_flags(&parse(&["--check-venv", "--test-launcher", "--update"])).is_empty(),
            "without an image these flags are legitimate"
        );
        assert_eq!(conflicting_image_flags(&parse(&["/tmp/a.png", "--check-venv"])), vec!["--check-venv"]);
        assert_eq!(conflicting_image_flags(&parse(&["--image", "/tmp/a.png", "--test-launcher"])), vec!["--test-launcher"]);
        assert_eq!(
            conflicting_image_flags(&parse(&["/tmp/a.png", "--uninstall", "--check-venv", "--continue-update"])),
            vec!["--check-venv", "--continue-update", "--uninstall"],
            "modes first, then installed-copy flags in table order"
        );
    }

    #[test]
    fn clear_open_target_forgets_every_open_target() {
        let mut cli = parse(&["--image", "/tmp/a.png", "--no-ai"]);
        cli.clear_open_target();
        assert!(cli.image_arg().is_none());
        assert!(cli.no_ai, "flags other than the open target are untouched");
        let mut cli = parse(&["/tmp/a.png"]);
        cli.clear_open_target();
        assert!(cli.image_arg().is_none());
        let mut cli = parse(&["--project", "/tmp/ch"]);
        cli.clear_open_target();
        assert!(cli.project.is_none());
    }

    #[test]
    fn check_image_arg_classifies_paths() {
        let dir = std::env::temp_dir().join(format!("ms-args-image-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        let file = dir.join("page one.png");
        std::fs::write(&file, b"not decoded here").expect("write test file");
        assert_eq!(check_image_arg(&file), Ok(()));
        assert_eq!(check_image_arg(&dir), Err(ImageArgProblem::Directory));
        assert_eq!(check_image_arg(&dir.join("missing.png")), Err(ImageArgProblem::NotFound));
        std::fs::remove_dir_all(&dir).expect("remove test dir");
    }

    /// Turns a borrowed command line into the owned form `standalone_relaunch_args` takes.
    fn owned(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn standalone_relaunch_prepends_the_flag_and_keeps_the_rest() {
        let relaunch = standalone_relaunch_args(&owned(&["--project", "/tmp/chapter", "--no-ai"]));
        assert_eq!(
            relaunch,
            owned(&[IGNORE_INSTALLED_FLAG, "--project", "/tmp/chapter", "--no-ai"]),
            "the flag must come first and no other argument may be dropped or reordered"
        );
    }

    #[test]
    fn standalone_relaunch_never_repeats_the_flag() {
        let relaunch = standalone_relaunch_args(&owned(&["--trace", IGNORE_INSTALLED_FLAG, "--no-ai"]));
        assert_eq!(relaunch, owned(&[IGNORE_INSTALLED_FLAG, "--trace", "--no-ai"]));
        assert_eq!(
            relaunch.iter().filter(|arg| *arg == IGNORE_INSTALLED_FLAG).count(),
            1,
            "a boolean flag passed twice is a usage error, not a stronger request"
        );
    }

    #[test]
    fn standalone_relaunch_of_an_empty_command_line_is_just_the_flag() {
        assert_eq!(standalone_relaunch_args(&[]), owned(&[IGNORE_INSTALLED_FLAG]));
    }

    #[test]
    fn standalone_relaunch_args_parse_back_into_standalone_mode() {
        let relaunch = standalone_relaunch_args(&owned(&["--no-ai"]));
        let mut argv = vec![OsString::from("manhwastudio_rs")];
        argv.extend(relaunch);
        let cli = Cli::try_parse_from(argv).expect("the relaunch command line must parse");
        assert!(cli.ignore_installed, "the relaunched process must be in standalone mode");
        assert!(cli.no_ai, "the original arguments must survive the relaunch");
        assert!(
            conflicting_installed_copy_flags(&cli).is_empty(),
            "the relaunch must not produce a command line startup rejects"
        );
    }
}
