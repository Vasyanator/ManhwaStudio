/*
FILE OVERVIEW: crates/ms-installer/src/lib.rs
Crate root of `ms-installer`, re-exported by the binary as `crate::installer`
(`src/lib.rs`), so every `crate::installer::…` call site keeps working unchanged.

Main responsibilities:
- expose the installer UI and service flows used by application startup, including the
  missing-environment prompt window (`prompt_window`);
- keep installer backend helpers scoped to this crate.

Notes:
The installer subsystem is desktop-only: it installs the managed Python
environment and replaces the desktop binary, neither of which exists on the
web build. Every module is compiled out on `wasm32` targets (the crate then
builds as an empty lib, and its dependencies sit in the matching target table
of `Cargo.toml`); every reference to it from shared code is itself gated to
native.
*/

#![warn(clippy::all)]

// `t!` / `tf!` for every user-visible string of the installer and update windows. Gated like
// every module that uses it: `ms-i18n` sits in the native-only target table of `Cargo.toml`.
#[cfg(not(target_arch = "wasm32"))]
#[macro_use]
extern crate ms_i18n;

/// The HOST application's own version, handed to the installer by the binary.
///
/// A library cannot read the executable's version: `MS_APP_VERSION` is a `rustc-env` the
/// root `build.rs` emits for the root crate only (and `env!("CARGO_PKG_VERSION")` compiled
/// inside this crate would yield *this crate's* version). It is therefore passed in at the
/// entry points instead of being read here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostVersion {
    /// `MS_APP_VERSION` of the application: the commit count it was built from (`335`), or the
    /// full `CARGO_PKG_VERSION` for a build without git. The one version of the program —
    /// displayed, compared against release tags (`version_format::compare_release_versions`)
    /// and written as the Windows Uninstall entry's `DisplayVersion`. See
    /// `crates/ms-config/src/version_format.rs`.
    pub version: &'static str,
}

// Desktop-only subsystem: installs/updates the native Python env and binary.
// No web equivalent exists, so every module is compiled out on wasm.
#[cfg(not(target_arch = "wasm32"))]
pub mod install;
#[cfg(not(target_arch = "wasm32"))]
pub mod update;
// The release feed, the selection of the release to update to, and its `SHA256SUMS` pins;
// `pub` because the binary's startup update check selects with the same function.
#[cfg(not(target_arch = "wasm32"))]
pub mod release;
// The missing-environment startup prompt window (install / update a custom folder / launcher).
#[cfg(not(target_arch = "wasm32"))]
pub mod prompt_window;
// The one window-context preparation (theme choice + bundled UI fonts) of every window above.
#[cfg(not(target_arch = "wasm32"))]
pub mod window;
// `utils` is the installer's backend half (layout probing, archive handling, pip and
// release queries). It is `pub` rather than crate-private because the startup
// `--check-venv` path (`src/venv_check.rs`) reads the same package requirements, and a
// second copy of those rules would be free to drift from the installer's.
#[cfg(not(target_arch = "wasm32"))]
pub mod utils;
// The PyTorch seam: pins, wheel options, preflight, install stage and the "installed PyTorch
// is current" predicate. `pub` because the launcher's settings page runs the same preflight
// and upgrade worker, and `venv_check` shares the readiness predicate.
#[cfg(not(target_arch = "wasm32"))]
pub mod torch;
// GitHub release metadata and asset downloads on the shared `ms-download` engine (retry,
// resume, verification, cancel); crate-private, used by `utils` and `update`.
#[cfg(not(target_arch = "wasm32"))]
mod download;
// The non-interactive, GUI-free readiness check behind the `--check-venv` startup flag.
// It lives here rather than next to the other host probes (`ms-sysprobe`) because it reads
// the installer's OWN package requirements and its two drift-prone predicates
// (`torch::readiness`'s `installed_torch_is_current`, `utils::missing_specs_for_readiness`) —
// and `ms-sysprobe` sits BELOW this crate, so hosting it there would make the two circular.
#[cfg(not(target_arch = "wasm32"))]
pub mod venv_check;
