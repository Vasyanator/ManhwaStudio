/*
File: crates/ms-input/src/action.rs

Purpose:
Code-side declarations of input actions: identity, kind, scope, activation conditions, default
bindings and the kind-specific behaviour (accepted parameter modes, repeat, mode effect and
precedence, gesture target). The user can rebind an action, never change any of this.

Key structures:
- `ActionId`: stable persistence key of an action (never renamed once shipped).
- `ActionKind`, `Scope` / `ScopeLevel`, `ActionMeta`.
- `CommandSpec`, `ModeSpec`, `AxisSpec`, `GestureSpec`, `FixedAction`, `ActionSpec`.
- `TargetId` / `TargetSpec`: gesture targets with priorities.
- `Declarations`: what one owner (the view set, a tab, the session) contributes to a registry.

Notes:
- The registry (`registry.rs`) validates declarations; nothing here checks cross references.
*/

use ms_config::app_tab::AppTab;

use crate::binding::Binding;
use crate::context::{Condition, ContextId};

/// Stable identifier of an action. It is the persistence key of the user's override, so it is
/// never renamed once shipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionId(pub &'static str);

impl ActionId {
    /// The id text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// The behaviour class of an action, fixed by code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// Fires once on the press that completes its chord (edge-triggered).
    Command,
    /// Active while its chord is held (level-triggered).
    Mode,
    /// A continuous value (wheel, pinch, drag delta) gated by a held chord.
    Axis,
    /// A pointer gesture (click, double click, drag, long press) on a declared target.
    Gesture,
}

/// Where an action lives. Resolution is innermost-first: `Window` > `Sub` > `Tab` > `Canvas` >
/// `View` > `Session`. Overlapping bindings on ONE level are conflicts; across levels they are
/// shadows (the inner one wins while active).
///
/// `Sub` and `Window` name their owning tab when they belong to one (`None` = shared by every tab,
/// e.g. the brush sub-context of `ms-tools`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Session,
    View,
    Canvas,
    Tab(AppTab),
    Sub(Option<AppTab>),
    Window(Option<AppTab>),
}

/// The nesting level of a [`Scope`]; `Ord` runs from outermost (`Session`) to innermost
/// (`Window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScopeLevel {
    Session,
    View,
    Canvas,
    Tab,
    Sub,
    Window,
}

impl Scope {
    /// The nesting level used for conflict / shadow classification and resolution order.
    #[must_use]
    pub fn level(self) -> ScopeLevel {
        match self {
            Self::Session => ScopeLevel::Session,
            Self::View => ScopeLevel::View,
            Self::Canvas => ScopeLevel::Canvas,
            Self::Tab(_) => ScopeLevel::Tab,
            Self::Sub(_) => ScopeLevel::Sub,
            Self::Window(_) => ScopeLevel::Window,
        }
    }

    /// The tab this scope is restricted to, if any.
    #[must_use]
    pub fn tab(self) -> Option<AppTab> {
        match self {
            Self::Tab(tab) => Some(tab),
            Self::Sub(tab) | Self::Window(tab) => tab,
            Self::Session | Self::View | Self::Canvas => None,
        }
    }
}

/// What every action declares, whatever its kind.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionMeta {
    pub id: ActionId,
    /// i18n key of the action's display title (resolved with `ms_i18n::resolve_key`).
    pub title_key: &'static str,
    /// i18n key of the settings section the action is listed under.
    pub section_key: &'static str,
    pub scope: Scope,
    /// Every condition must hold for the action to be active.
    pub when: Vec<Condition>,
    /// Default alternatives; an empty list means "unbound by default".
    pub defaults: Vec<Binding>,
}

/// How an accepted parameter mode is sampled for a gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sample {
    /// The mode's state at the press that started the action.
    AtPress,
    /// The mode's latest state while the gesture runs.
    LatestDuringGesture,
}

/// A mode whose state an action receives as a parameter (e.g. `recent_char.3` held while `T`
/// creates a bubble). Its modifiers are tolerated by the action's exact-modifier match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accept {
    pub mode: ActionId,
    pub sample: Sample,
}

/// A command: fires on the press completing the last chord of one of its alternatives.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandSpec {
    pub meta: ActionMeta,
    pub accepts: Vec<Accept>,
    /// Re-fire on key auto-repeat while the completing button stays held.
    pub repeat: bool,
}

/// When a mode's effect starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeEffect {
    /// As soon as the chord is held.
    Immediate,
    /// Only when a drag starts while the chord is held (e.g. Space-to-pan).
    OnDragStart,
}

/// What a running gesture does when this mode is released mid-gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasePolicy {
    Commit,
    Cancel,
}

/// A mode: active while one of its alternatives is held (subset match).
#[derive(Debug, Clone, PartialEq)]
pub struct ModeSpec {
    pub meta: ActionMeta,
    pub effect: ModeEffect,
    pub on_release_mid_gesture: ReleasePolicy,
    /// Modes this one deactivates while both are held (declared precedence where the chords do
    /// not order them).
    pub beats: Vec<ActionId>,
    /// The value of a parameter mode (`recent_char.N` carries `Some(N)`).
    pub param: Option<u8>,
}

/// A continuous axis (wheel, pinch, drag delta) under a held chord.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisSpec {
    pub meta: ActionMeta,
}

/// Identifier of a gesture target (a hit-testable part of a view, e.g. `canvas.bubble.handle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetId(pub &'static str);

/// A gesture target. Higher `priority` wins between gestures of equal chord specificity;
/// `canvas_wide` targets are claimed by the router before widgets draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetSpec {
    pub id: TargetId,
    pub priority: u16,
    pub canvas_wide: bool,
}

/// A pointer gesture bound on one target.
#[derive(Debug, Clone, PartialEq)]
pub struct GestureSpec {
    pub meta: ActionMeta,
    pub target: TargetId,
    pub accepts: Vec<Accept>,
}

/// A read-only binding the program does not let the user change (clipboard events produced by
/// the platform layer, egui's own scrolling). Listed in the settings pane and part of conflict
/// detection; `meta.defaults` are its fixed bindings.
#[derive(Debug, Clone, PartialEq)]
pub struct FixedAction {
    pub meta: ActionMeta,
}

/// One declared, rebindable action.
#[derive(Debug, Clone, PartialEq)]
pub enum ActionSpec {
    Command(CommandSpec),
    Mode(ModeSpec),
    Axis(AxisSpec),
    Gesture(GestureSpec),
}

impl ActionSpec {
    /// The common declaration part.
    #[must_use]
    pub fn meta(&self) -> &ActionMeta {
        match self {
            Self::Command(spec) => &spec.meta,
            Self::Mode(spec) => &spec.meta,
            Self::Axis(spec) => &spec.meta,
            Self::Gesture(spec) => &spec.meta,
        }
    }

    /// The action's kind.
    #[must_use]
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Command(_) => ActionKind::Command,
            Self::Mode(_) => ActionKind::Mode,
            Self::Axis(_) => ActionKind::Axis,
            Self::Gesture(_) => ActionKind::Gesture,
        }
    }

    /// The accepted parameter modes (none for modes and axes).
    #[must_use]
    pub fn accepts(&self) -> &[Accept] {
        match self {
            Self::Command(spec) => &spec.accepts,
            Self::Gesture(spec) => &spec.accepts,
            Self::Mode(_) | Self::Axis(_) => &[],
        }
    }
}

/// What one owner contributes to a registry. Several are merged with [`Declarations::extend`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Declarations {
    pub actions: Vec<ActionSpec>,
    pub targets: Vec<TargetSpec>,
    pub fixed: Vec<FixedAction>,
    /// Groups of contexts of which at most one is active at a time (e.g. the tools of one tab).
    /// Actions requiring two different contexts of one group are never co-active, so their
    /// overlapping bindings are neither conflicts nor shadows.
    pub exclusive_contexts: Vec<Vec<ContextId>>,
}

impl Declarations {
    /// Appends everything `other` declares.
    pub fn extend(&mut self, other: Declarations) {
        self.actions.extend(other.actions);
        self.targets.extend(other.targets);
        self.fixed.extend(other.fixed);
        self.exclusive_contexts.extend(other.exclusive_contexts);
    }
}
