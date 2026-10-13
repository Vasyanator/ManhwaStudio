/*
File: crates/ms-widgets/src/embedded_window/stack.rs

Purpose:
The pure stack model behind `EmbeddedWindow`: which embedded windows are open in which OS window
(viewport), in what order, and which one covers the viewport in a given egui pass. No drawing, no
`Context`; the only egui types are the `Id` / `ViewportId` keys, so every rule is unit-tested here.

Key structures:
- WindowStacks : every viewport's stack plus its per-pass covering decision; stored ONCE per egui
                 `Context` in temp data by `embedded_window.rs` (the one owner of the stack)
- StackEntry   : one open window (id, parent window, collapsed flag, last pass it was shown)
- Decision     : the covering window fixed for one pass of one viewport

Key functions:
- WindowStacks::decide()  : the per-pass covering decision (prunes stale entries first)
- WindowStacks::touch() / remove() / raise() / set_collapsed() : the stack mutations
- escape_closes()         : the pure Escape rule of a window

Notes:
- "Fresh" means shown in this pass or the previous one. A window that stops calling `show` drops
  out after one pass; a pass number from the future (egui recreated the viewport and restarted
  its counter) is stale too.
- Removing or raising a window acts on its whole subtree (entries whose `parent` chain reaches
  it), so a closed parent never leaves a child covering a viewport nobody repaints.
*/

use std::collections::HashMap;

use egui::{Id, ViewportId};

/// One open embedded window on a viewport's stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StackEntry {
    /// The window's stack identity (`Id::new(window id)`).
    pub(crate) id: Id,
    /// The window whose `children` closure showed this one, if any (same viewport only).
    pub(crate) parent: Option<Id>,
    /// Collapsed windows keep their place but never cover anything.
    pub(crate) collapsed: bool,
    /// `Context::cumulative_pass_nr` of the last pass that showed the window.
    pub(crate) last_pass: u64,
}

/// The covering window of one viewport, fixed at the first evaluation of a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Decision {
    /// The pass this decision belongs to.
    pub(crate) pass: u64,
    /// The topmost fresh, non-collapsed window, or `None` when the host is uncovered.
    pub(crate) covering: Option<Id>,
}

/// Result of [`WindowStacks::decide`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DecideOutcome {
    /// The covering window of this pass.
    pub(crate) covering: Option<Id>,
    /// True exactly once per pass in which the covering window differs from the previous
    /// decision of the same viewport (the caller then drops keyboard focus).
    pub(crate) changed: bool,
}

/// One viewport's partition: the stack (bottom first) and the cached decision.
#[derive(Clone, Debug, Default)]
struct ViewportStack {
    entries: Vec<StackEntry>,
    decision: Option<Decision>,
    /// Windows whose `children` closure is running right now, innermost last.
    parent_scope: Vec<Id>,
}

/// Every viewport's stack of embedded windows. `Default` + `Clone` + `Send` + `Sync` because it
/// lives in egui's temp data (`IdTypeMap::get_temp_mut_or_default`).
#[derive(Clone, Debug, Default)]
pub(crate) struct WindowStacks {
    by_viewport: HashMap<ViewportId, ViewportStack>,
}

/// Whether an entry last shown in `last_pass` still counts in `pass`: shown in this pass or the
/// previous one. A `last_pass` greater than `pass` is stale (the viewport's counter restarted).
fn is_fresh(last_pass: u64, pass: u64) -> bool {
    last_pass == pass || last_pass.checked_add(1) == Some(pass)
}

impl WindowStacks {
    /// The covering window of `viewport` for `pass`, decided at the first call of the pass and
    /// returned unchanged by every later call of the same pass, whatever the stack does in
    /// between. A new decision first prunes entries that are no longer fresh and resets a
    /// leftover children scope (a pass that unwound mid-`children`).
    pub(crate) fn decide(&mut self, viewport: ViewportId, pass: u64) -> DecideOutcome {
        let stack = self.by_viewport.entry(viewport).or_default();
        if let Some(decision) = stack.decision
            && decision.pass == pass
        {
            return DecideOutcome { covering: decision.covering, changed: false };
        }
        stack.entries.retain(|entry| is_fresh(entry.last_pass, pass));
        stack.parent_scope.clear();
        let covering = stack.entries.iter().rev().find(|entry| !entry.collapsed).map(|entry| entry.id);
        let previous = stack.decision.and_then(|decision| decision.covering);
        stack.decision = Some(Decision { pass, covering });
        DecideOutcome { covering, changed: previous != covering }
    }

    /// The topmost fresh, non-collapsed window of `viewport` in `pass`, computed now (no cache).
    #[cfg(test)]
    pub(crate) fn covering(&self, viewport: ViewportId, pass: u64) -> Option<Id> {
        self.by_viewport.get(&viewport)?.entries.iter().rev().find(|entry| !entry.collapsed && is_fresh(entry.last_pass, pass)).map(|entry| entry.id)
    }

    /// Records that `id` is shown on `viewport` in `pass`. An absent window is pushed on TOP
    /// with the current children scope as its parent; a present one keeps its position and only
    /// refreshes `collapsed` and `last_pass`. Returns true when the window was pushed.
    pub(crate) fn touch(&mut self, viewport: ViewportId, id: Id, collapsed: bool, pass: u64) -> bool {
        let stack = self.by_viewport.entry(viewport).or_default();
        if let Some(entry) = stack.entries.iter_mut().find(|entry| entry.id == id) {
            entry.collapsed = collapsed;
            entry.last_pass = pass;
            return false;
        }
        let parent = stack.parent_scope.last().copied();
        stack.entries.push(StackEntry { id, parent, collapsed, last_pass: pass });
        true
    }

    /// Removes `id` and every window below it in the parent chain from `viewport`. Returns
    /// whether anything was removed.
    pub(crate) fn remove(&mut self, viewport: ViewportId, id: Id) -> bool {
        let Some(stack) = self.by_viewport.get_mut(&viewport) else {
            return false;
        };
        let subtree = subtree_of(&stack.entries, id);
        let before = stack.entries.len();
        stack.entries.retain(|entry| !subtree.contains(&entry.id));
        stack.entries.len() != before
    }

    /// Moves `id` and its subtree to the top of `viewport`'s stack, keeping their relative order
    /// (a restored window rises above everything, and its own open children stay above it).
    /// Returns whether `id` was on the stack.
    pub(crate) fn raise(&mut self, viewport: ViewportId, id: Id) -> bool {
        let Some(stack) = self.by_viewport.get_mut(&viewport) else {
            return false;
        };
        if !stack.entries.iter().any(|entry| entry.id == id) {
            return false;
        }
        let subtree = subtree_of(&stack.entries, id);
        let (moved, kept): (Vec<StackEntry>, Vec<StackEntry>) = stack.entries.drain(..).partition(|entry| subtree.contains(&entry.id));
        stack.entries = kept;
        stack.entries.extend(moved);
        true
    }

    /// Sets the collapsed flag of `id` on `viewport` in place. Returns whether `id` was found.
    pub(crate) fn set_collapsed(&mut self, viewport: ViewportId, id: Id, collapsed: bool) -> bool {
        let Some(entry) = self.by_viewport.get_mut(&viewport).and_then(|stack| stack.entries.iter_mut().find(|entry| entry.id == id)) else {
            return false;
        };
        entry.collapsed = collapsed;
        true
    }

    /// Forgets `viewport` entirely (stack, decision, scope): used when the separate OS window
    /// that owned it closes or turns embedded, because egui restarts a recreated viewport's pass
    /// counter.
    pub(crate) fn drop_viewport(&mut self, viewport: ViewportId) {
        self.by_viewport.remove(&viewport);
    }

    /// Enters `id`'s `children` closure on `viewport`: windows pushed until the matching
    /// [`Self::pop_parent`] get `id` as their parent.
    pub(crate) fn push_parent(&mut self, viewport: ViewportId, id: Id) {
        self.by_viewport.entry(viewport).or_default().parent_scope.push(id);
    }

    /// Leaves the innermost `children` closure of `viewport`.
    pub(crate) fn pop_parent(&mut self, viewport: ViewportId) {
        if let Some(stack) = self.by_viewport.get_mut(&viewport) {
            stack.parent_scope.pop();
        }
    }

    /// The stack of `viewport`, bottom first (tests and diagnostics).
    #[cfg(test)]
    pub(crate) fn entries(&self, viewport: ViewportId) -> &[StackEntry] {
        self.by_viewport.get(&viewport).map_or(&[], |stack| stack.entries.as_slice())
    }
}

/// `root` plus every entry whose parent chain reaches it. The chain is followed through the
/// stack itself, so a parent that already left the stack ends the chain.
fn subtree_of(entries: &[StackEntry], root: Id) -> Vec<Id> {
    let mut subtree = vec![root];
    // Each sweep adds the direct children of what is already collected; a sweep that adds
    // nothing ends the walk, so the loop runs at most `entries.len() + 1` times.
    loop {
        let before = subtree.len();
        for entry in entries {
            if !subtree.contains(&entry.id) && entry.parent.is_some_and(|parent| subtree.contains(&parent)) {
                subtree.push(entry.id);
            }
        }
        if subtree.len() == before {
            return subtree;
        }
    }
}

/// Whether Escape closes a window: only the covering window, only when the window enabled it,
/// and only when no widget held keyboard focus (now or at the end of the window's previous
/// pass — egui drops focus on Escape before the pass runs, so "nothing focused now" alone would
/// let the press that un-focuses a text field also close the window).
#[must_use]
pub(crate) fn escape_closes(covering: bool, enabled: bool, nothing_focused: bool) -> bool {
    covering && enabled && nothing_focused
}

#[cfg(test)]
mod tests {
    use super::{WindowStacks, escape_closes};
    use egui::{Id, ViewportId};

    fn vp() -> ViewportId {
        ViewportId::ROOT
    }

    fn other_vp() -> ViewportId {
        ViewportId::from_hash_of("ms_embedded_window_test_other")
    }

    fn ids(stacks: &WindowStacks, viewport: ViewportId) -> Vec<Id> {
        stacks.entries(viewport).iter().map(|entry| entry.id).collect()
    }

    #[test]
    fn touch_pushes_new_windows_on_top_and_keeps_existing_positions() {
        let (a, b) = (Id::new("a"), Id::new("b"));
        let mut stacks = WindowStacks::default();
        assert!(stacks.touch(vp(), a, false, 1));
        assert!(stacks.touch(vp(), b, false, 1));
        assert!(!stacks.touch(vp(), a, false, 2), "a present window is refreshed, not pushed");
        assert_eq!(ids(&stacks, vp()), vec![a, b]);
        assert_eq!(stacks.covering(vp(), 2), Some(b));
    }

    #[test]
    fn collapsed_entry_is_skipped_by_covering() {
        let (a, b) = (Id::new("a"), Id::new("b"));
        let mut stacks = WindowStacks::default();
        stacks.touch(vp(), a, false, 1);
        stacks.touch(vp(), b, true, 1);
        assert_eq!(stacks.covering(vp(), 1), Some(a));
        assert!(stacks.set_collapsed(vp(), a, true));
        assert_eq!(stacks.covering(vp(), 1), None);
    }

    #[test]
    fn raise_moves_the_window_and_its_subtree_on_top_in_order() {
        let (parent, child, other) = (Id::new("parent"), Id::new("child"), Id::new("other"));
        let mut stacks = WindowStacks::default();
        stacks.touch(vp(), parent, true, 1);
        stacks.push_parent(vp(), parent);
        stacks.touch(vp(), child, false, 1);
        stacks.pop_parent(vp());
        stacks.touch(vp(), other, false, 1);
        assert_eq!(ids(&stacks, vp()), vec![parent, child, other]);
        assert!(stacks.raise(vp(), parent));
        assert_eq!(ids(&stacks, vp()), vec![other, parent, child]);
        assert!(!stacks.raise(vp(), Id::new("absent")));
    }

    #[test]
    fn entry_not_shown_last_pass_is_pruned_by_decide() {
        let (a, b) = (Id::new("a"), Id::new("b"));
        let mut stacks = WindowStacks::default();
        stacks.touch(vp(), a, false, 1);
        stacks.touch(vp(), b, false, 2);
        assert_eq!(stacks.decide(vp(), 3).covering, Some(b));
        assert_eq!(ids(&stacks, vp()), vec![b], "a was last shown in pass 1, so it is gone in pass 3");
        assert_eq!(stacks.decide(vp(), 4).covering, None);
        assert!(ids(&stacks, vp()).is_empty());
    }

    #[test]
    fn entry_from_a_restarted_pass_counter_is_stale() {
        let a = Id::new("a");
        let mut stacks = WindowStacks::default();
        stacks.touch(vp(), a, false, 50);
        assert_eq!(stacks.decide(vp(), 0).covering, None);
    }

    #[test]
    fn decide_is_fixed_for_the_whole_pass() {
        let (a, b) = (Id::new("a"), Id::new("b"));
        let mut stacks = WindowStacks::default();
        stacks.touch(vp(), a, false, 1);
        let first = stacks.decide(vp(), 2);
        assert_eq!(first.covering, Some(a));
        assert!(first.changed);
        // A window opened later in the same pass does not change this pass's decision.
        stacks.touch(vp(), b, false, 2);
        stacks.remove(vp(), a);
        let again = stacks.decide(vp(), 2);
        assert_eq!(again.covering, Some(a));
        assert!(!again.changed);
        let next = stacks.decide(vp(), 3);
        assert_eq!(next.covering, Some(b));
        assert!(next.changed);
        stacks.touch(vp(), b, false, 3);
        assert!(!stacks.decide(vp(), 4).changed, "same covering window in consecutive passes");
    }

    #[test]
    fn remove_closes_the_window_and_its_descendants_one_level_at_a_time() {
        let (root, child, grandchild) = (Id::new("root"), Id::new("child"), Id::new("grandchild"));
        let mut stacks = WindowStacks::default();
        stacks.touch(vp(), root, false, 1);
        stacks.push_parent(vp(), root);
        stacks.touch(vp(), child, false, 1);
        stacks.push_parent(vp(), child);
        stacks.touch(vp(), grandchild, false, 1);
        stacks.pop_parent(vp());
        stacks.pop_parent(vp());
        assert_eq!(stacks.covering(vp(), 1), Some(grandchild));
        // Closing the top window uncovers exactly one level.
        assert!(stacks.remove(vp(), grandchild));
        assert_eq!(stacks.covering(vp(), 1), Some(child));
        // Closing a parent takes its remaining descendants with it.
        stacks.push_parent(vp(), child);
        stacks.touch(vp(), grandchild, false, 1);
        stacks.pop_parent(vp());
        assert_eq!(stacks.covering(vp(), 1), Some(grandchild));
        assert!(stacks.remove(vp(), root));
        assert!(ids(&stacks, vp()).is_empty());
        assert!(!stacks.remove(vp(), root));
    }

    #[test]
    fn viewports_are_isolated() {
        let (a, b) = (Id::new("a"), Id::new("b"));
        let mut stacks = WindowStacks::default();
        stacks.touch(vp(), a, false, 1);
        stacks.touch(other_vp(), b, false, 7);
        assert_eq!(stacks.covering(vp(), 1), Some(a));
        assert_eq!(stacks.covering(other_vp(), 7), Some(b));
        stacks.drop_viewport(other_vp());
        assert_eq!(stacks.covering(other_vp(), 7), None);
        assert_eq!(stacks.covering(vp(), 1), Some(a));
        // A parent scope on one viewport does not parent windows of another.
        stacks.push_parent(vp(), a);
        stacks.touch(other_vp(), b, false, 7);
        assert_eq!(stacks.entries(other_vp())[0].parent, None);
    }

    #[test]
    fn escape_closes_only_the_unfocused_covering_window_that_enabled_it() {
        assert!(escape_closes(true, true, true));
        assert!(!escape_closes(false, true, true));
        assert!(!escape_closes(true, false, true));
        assert!(!escape_closes(true, true, false));
    }
}
