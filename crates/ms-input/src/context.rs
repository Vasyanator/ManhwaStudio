/*
File: crates/ms-input/src/context.rs

Purpose:
Named activation contexts and conditions, the per-frame activity snapshot the matcher reads
(`ActiveSet`), and the static "can these two actions ever be active together" rule that conflict
detection uses.

Key structures:
- `ContextId`: a named context a tab / tool / window publishes while it holds
  (`detector.mask_edit`, `brush.active`, ...).
- `Condition`: one activation condition of an action.
- `ActiveSet`: what is true this frame (active tab, published contexts, text edit focus, popup,
  pointer over a view, a view taking focus-less input).

Key functions:
- `ActiveSet::meta_active`: scope + conditions of one action.
- `may_be_co_active`: the static co-activity rule.

Notes:
- `ActiveSet` is plain data; the egui runtime fills it each frame (text edit focus from
  `Context::text_edit_focused`, popups from `Popup::is_any_open || Context::any_popup_open`).
*/

use std::collections::BTreeSet;

use ms_config::app_tab::AppTab;

use crate::action::{ActionMeta, Scope};

/// A named context. Published by its owner while it holds; stable across releases only as far
/// as declarations reference it (it is never persisted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextId(pub &'static str);

/// One activation condition of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// No popup / menu / combo box list is open.
    NoPopup,
    /// The pointer is over a published view surface (not over a dock panel, popup, menu or a
    /// hole the view declared).
    PointerOnView,
    /// Some view takes focus-less input: the pointer is on a view, or a `primary` view surface is
    /// published (the zoom commands' focus rule). False on a tab that draws no viewer (PS editor,
    /// page manager, settings), so its keys are never consumed for a view nobody draws.
    ViewFocus,
    /// The context is published this frame.
    Context(ContextId),
    /// The context is NOT published this frame.
    NotContext(ContextId),
}

/// What holds this frame, as far as action activity is concerned.
#[expect(clippy::struct_excessive_bools, reason = "independent per-frame facts, each tested on its own by one `Condition` variant (or the matcher's text-edit / popup rules); not a state machine")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveSet {
    pub active_tab: Option<AppTab>,
    pub contexts: BTreeSet<ContextId>,
    /// A text edit has keyboard focus: chords holding a keyboard key never match (modifier,
    /// mouse and touch chords are unaffected).
    pub text_edit_focused: bool,
    /// A popup is open: commands never fire (popup bodies consume Enter / Esc / arrows).
    pub popup_open: bool,
    pub pointer_on_view: bool,
    /// A view takes focus-less input this frame ([`Condition::ViewFocus`]).
    pub view_focus: bool,
}

impl ActiveSet {
    /// Whether `condition` holds.
    #[must_use]
    pub fn holds(&self, condition: &Condition) -> bool {
        match condition {
            Condition::NoPopup => !self.popup_open,
            Condition::PointerOnView => self.pointer_on_view,
            Condition::ViewFocus => self.view_focus,
            Condition::Context(id) => self.contexts.contains(id),
            Condition::NotContext(id) => !self.contexts.contains(id),
        }
    }

    /// Whether an action of `scope` can be active under the current tab.
    #[must_use]
    pub fn scope_active(&self, scope: Scope) -> bool {
        scope.tab().is_none_or(|tab| self.active_tab == Some(tab))
    }

    /// Whether the action declared by `meta` is active: its scope matches the active tab and
    /// every `when` condition holds.
    #[must_use]
    pub fn meta_active(&self, meta: &ActionMeta) -> bool {
        self.scope_active(meta.scope) && meta.when.iter().all(|condition| self.holds(condition))
    }
}

/// Whether the actions `a` and `b` can ever be active in the same frame, judged statically from
/// their declarations: their tabs must be compatible, no context may be required by one and
/// excluded by the other, and they must not require two different contexts of one exclusive
/// group. Conservative: anything not provably exclusive counts as co-active.
#[must_use]
pub fn may_be_co_active(a: &ActionMeta, b: &ActionMeta, exclusive: &[Vec<ContextId>]) -> bool {
    if let (Some(tab_a), Some(tab_b)) = (a.scope.tab(), b.scope.tab())
        && tab_a != tab_b
    {
        return false;
    }
    let required = |meta: &ActionMeta| -> BTreeSet<ContextId> {
        meta.when
            .iter()
            .filter_map(|condition| match condition {
                Condition::Context(id) => Some(*id),
                Condition::NotContext(_) | Condition::NoPopup | Condition::PointerOnView | Condition::ViewFocus => None,
            })
            .collect()
    };
    let excluded = |meta: &ActionMeta| -> BTreeSet<ContextId> {
        meta.when
            .iter()
            .filter_map(|condition| match condition {
                Condition::NotContext(id) => Some(*id),
                Condition::Context(_) | Condition::NoPopup | Condition::PointerOnView | Condition::ViewFocus => None,
            })
            .collect()
    };
    let (required_a, required_b) = (required(a), required(b));
    if !required_a.is_disjoint(&excluded(b)) || !required_b.is_disjoint(&excluded(a)) {
        return false;
    }
    !exclusive.iter().any(|group| {
        let in_a: BTreeSet<&ContextId> = group.iter().filter(|id| required_a.contains(id)).collect();
        let in_b: BTreeSet<&ContextId> = group.iter().filter(|id| required_b.contains(id)).collect();
        !in_a.is_empty() && !in_b.is_empty() && in_a != in_b
    })
}
