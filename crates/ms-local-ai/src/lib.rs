/*
FILE OVERVIEW: crates/ms-local-ai/src/lib.rs
Crate root of `ms-local-ai`: the process's local-AI facts shared by the Translation and Cleaning
tabs, the settings panes and backend supervisor (`ms-settings-ui`), the launcher's reline flow
and the root.

Submodules:
- `backend_health`: push-driven AI-backend health (`TOPIC_HEALTH` v2 events) + device-control helpers.
- `large_models`: the one Rust client of the backend's large-model pool (`large_models.*`):
  wire types and strict parsers, the step vocabulary with its labels, the calls (worker-only),
  and the latest-wins `ProfilesQuery` / debounced `LimitsWriter`.
- `local_ai`: which local AI runtimes (Python backend, native ONNX) the process may use
  (`--no-ai` + platform), the one route-availability rule (`AiRouteKind::availability`) and
  the effective-runtime helper (`effective_ai_runtime`).

Notes:
- GUI-free and below every tab: a crate that needs only these facts must not depend on
  `ms-tab-translation` for them.
*/

#![warn(clippy::all)]

// The `ms-i18n` UI-string macros (`t!` / `tf!`), mounted crate-wide: the localized status and
// error texts use the bare macro names the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

pub mod backend_health;
pub mod large_models;
pub mod local_ai;
