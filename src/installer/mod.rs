/*
File: mod.rs

Purpose:
Defines the installer module boundary.

Main responsibilities:
- expose the installer UI and service flows used by application startup;
- keep installer backend helpers private to the installer module.

Notes:
The installer subsystem is desktop-only: it installs the managed Python
environment and replaces the desktop binary, neither of which exists on the
web build. The whole module is compiled out on `wasm32` targets; every
reference to it from shared code is itself gated to native.
*/

// Desktop-only subsystem: installs/updates the native Python env and binary.
// No web equivalent exists, so the whole module is compiled out on wasm.
#[cfg(not(target_arch = "wasm32"))]
pub mod install;
#[cfg(not(target_arch = "wasm32"))]
pub mod update;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod utils;
