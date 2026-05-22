/*
FILE OVERVIEW: src/args.rs
CLI argument parsing for the main Rust app.

Main items:
- `Cli.project`: optional path to chapter/project directory.
- `Cli.no_ai`: disables AI-dependent functionality at startup.
- `Cli.update`: opens the Rust update window directly.
- `Cli.test_launcher`: starts the new Rust launcher test mode instead of the main app.
- `Cli.test_ver_check`: forces update checks to report an available update in launcher/update UI.
- `Cli.continue_install`: скрытый служебный флаг продолжения установки после elevation.
- `Cli.continue_install_target`: скрытый служебный путь установки для continuation.
- `Cli.uninstall`: скрытый Windows-флаг удаления установленной копии приложения.
- `Cli.continue_uninstall`: скрытый служебный флаг продолжения удаления после elevation.
- `Cli.create_start_menu_shortcut_install_dir`: скрытый служебный путь установки для elevated-создания ярлыка меню Пуск.
- `Cli.continue_create_start_menu_shortcut`: скрытый служебный флаг продолжения elevated-создания ярлыка меню Пуск.
- `Cli.uninstall_signal_file`: скрытый служебный файл-сигнал для сценария "удалить и затем переустановить".
- `Cli.continue_update`: hidden service flag that resumes update work after executable replacement.
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
}
