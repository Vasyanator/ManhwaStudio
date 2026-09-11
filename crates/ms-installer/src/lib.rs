/*
FILE OVERVIEW: crates/ms-installer/src/lib.rs
Crate root of `ms-installer`, re-exported by the binary as `crate::installer`
(`src/main.rs`), so every `crate::installer::…` call site keeps working unchanged.

Main responsibilities:
- expose the installer UI and service flows used by application startup;
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

// `t!` / `tf!` for every user-visible string of the installer and update windows.
#[macro_use]
extern crate ms_i18n;

/// The HOST application's own version, handed to the installer by the binary.
///
/// A library cannot read the executable's version: `env!("CARGO_PKG_VERSION")` compiled
/// inside this crate would yield *this crate's* version, and `MS_APP_VERSION` is a
/// `rustc-env` the root `build.rs` emits for the root crate only. Both halves are
/// therefore passed in at the entry points instead of being read here.
///
/// The two fields are NOT interchangeable — see
/// `crates/ms-config/src/version_format.rs` for the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostVersion {
    /// Plain `CARGO_PKG_VERSION` of the application. The machine-facing value: every
    /// release comparison reduces this and the remote tag with `version_core` first, so a
    /// git-suffixed development build never outranks the tag it was built from.
    pub core: &'static str,
    /// Extended, git-derived `MS_APP_VERSION` of the application. Display only — never
    /// compared, never parsed.
    pub display: &'static str,
}

// Desktop-only subsystem: installs/updates the native Python env and binary.
// No web equivalent exists, so every module is compiled out on wasm.
#[cfg(not(target_arch = "wasm32"))]
pub mod install;
#[cfg(not(target_arch = "wasm32"))]
pub mod update;
// `utils` is the installer's backend half (layout probing, archive handling, pip and
// release queries). It is `pub` rather than crate-private because the startup
// `--check-venv` path (`src/venv_check.rs`) and the launcher's settings page read the
// same package requirements and version helpers, and a second copy of those rules
// would be free to drift from the installer's.
#[cfg(not(target_arch = "wasm32"))]
pub mod utils;
// The non-interactive, GUI-free readiness check behind the `--check-venv` startup flag.
// It lives here rather than next to the other host probes (`ms-sysprobe`) because it reads
// the installer's OWN package requirements and its two drift-prone predicates
// (`installed_torch_is_current`, `missing_specs_for_readiness`) — and `ms-sysprobe` sits
// BELOW this crate, so hosting it there would make the two circular.
#[cfg(not(target_arch = "wasm32"))]
pub mod venv_check;
