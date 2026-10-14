/*
FILE OVERVIEW: crates/ms-input/src/lib.rs
Crate root of `ms-input`: the unified input model of the studio.

Submodules:
- `button`: bindable inputs (keys, modifier roles, mouse, touch sources, unknown devices).
- `action`: code-side action declarations (kinds, scopes, specs, targets, `Declarations`).
- `binding`: binding alternatives per kind and chord resolution through mode references.
- `context`: named contexts, conditions, the per-frame `ActiveSet`, co-activity.
- `registry`: `InputRegistry` (validated declarations + current bindings + conflicts).
- `matching`: the pure matcher over abstract press / release events.
- `conflicts`: conflict and shadow detection.
- `label`: localized binding labels and conflict texts.
- `persist`: the `Hotkeys` section codec, v1 migration and writers.
- `view`: the shared `view.*` action set.
- `router`: the egui runtime (`InputRouter::begin_frame`) over the matcher.
- `frame`: the per-frame result (`InputFrame`) and the view-surface publication.
- `capture`: binding capture for the settings pane.
- `touch`: the canvas touch-gesture router (raw-input stage), re-exported by `ms-widgets` as
  `touch_nav` until its importers are re-pointed.

Notes:
- The model (`action` .. `view`) is pure: it never reads an egui `Context`. The runtime
  (`router`, `frame`, `capture`, `touch`) builds on it and never re-implements matching.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

// The `ms-i18n` UI-string macros (`t!` / `tf!`), mounted crate-wide: the label code uses the bare
// macro names the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

pub mod action;
pub mod binding;
pub mod button;
pub mod capture;
pub mod conflicts;
pub mod context;
pub mod frame;
pub mod label;
pub mod matching;
pub mod persist;
pub mod registry;
pub mod router;
pub mod touch;
pub mod view;

pub use action::{
    Accept, ActionId, ActionKind, ActionMeta, ActionSpec, AxisSpec, CommandSpec, Declarations, FixedAction, GestureSpec, ModeEffect, ModeSpec,
    ReleasePolicy, Sample, Scope, ScopeLevel, TargetId, TargetSpec,
};
pub use binding::{AxisBinding, AxisSource, Binding, BindingError, ButtonChord, Chord, CommandBinding, GestureBinding, GestureKind};
pub use button::{Button, ModifierKey, MouseButton, TouchSource};
pub use capture::{CaptureError, CaptureSlot, CaptureStatus, captured_alternatives};
pub use conflicts::{Conflict, ConflictKind, ConflictReport, Shadow};
pub use context::{ActiveSet, Condition, ContextId};
pub use frame::{AxisSample, ClaimedGesture, Fired, FrameEnv, GestureHit, GesturePhase, HitPhase, InputFrame, ViewFocus, ViewId, ViewSurface, publish_view_surface};
pub use label::button_label;
pub use matching::{AxisMatch, FiredCommand, GestureMatch, MatchEvent, MatchOutcome, MatchState};
pub use persist::{
    DecodeError, HOTKEYS_FORMAT, HOTKEYS_SECTION, HotkeyOverrides, LoadedOverrides, PersistError, load_overrides, write_migrated_section, write_override,
};
pub use registry::{ActionView, DeclarationError, InputRegistry, OverrideEntry, OverrideWarning};
pub use router::{InputRouter, WHEEL_NOTCH_PT};

#[cfg(test)]
mod tests;
