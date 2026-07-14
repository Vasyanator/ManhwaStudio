/*
File: crates/ms-actions/src/history.rs

Purpose:
Generic in-memory undo/redo engine `ActionHistory<A>` over any `ReversibleAction`.
Mirrors the semantics of Koharu's `history.rs` undo/redo stacks (apply pushes to
undo and truncates the redo branch; undo applies the inverse and keeps the
original for redo) but is fully generic and has NO durable log — the append-only
history log is a later, optional phase.

Key items:
- struct `ActionHistory<A>`

Notes:
GUI-free; holds no locks and does no I/O. All heavy work lives inside the
action's `apply`/`inverse`, which the caller schedules off the GUI thread as
needed.
*/

use std::collections::VecDeque;

use crate::action::ReversibleAction;

/// In-memory undo/redo engine for a single action type `A`.
///
/// Holds two stacks: `undo` (oldest at the front, newest at the back) and
/// `redo`. `apply` runs an action forward and records it for undo, truncating
/// the redo branch. `undo` applies the action's inverse and preserves the
/// original on the redo stack; `redo` re-applies it.
///
/// The undo stack is bounded two ways, evicting oldest-first (front): a count
/// cap (`limit`) and an optional total-bytes cap (`weight_budget`) summed over
/// [`ReversibleAction::weight`]. The running total is tracked incrementally in
/// `undo_weight` (never recomputed by iteration). The weight budget applies to
/// the UNDO stack only — redo weight is intentionally not budgeted, keeping the
/// bookkeeping simple; the redo branch is bounded in practice because it is
/// truncated on every fresh edit. A single undo entry larger than the whole
/// budget is still retained (you cannot drop the only undo step).
#[derive(Debug)]
pub struct ActionHistory<A: ReversibleAction> {
    /// Applied actions available for undo. Front = oldest, back = most recent.
    undo: VecDeque<A>,
    /// Undone actions available for redo. Back = most recently undone.
    redo: Vec<A>,
    /// Maximum number of retained undo entries. A `limit` of 0 disables undo.
    limit: usize,
    /// Optional total-bytes cap over the undo stack, summed via `weight()`.
    /// `None` disables weight budgeting.
    weight_budget: Option<usize>,
    /// Running sum of `weight()` over the current undo stack. Maintained
    /// incrementally on push/eviction/undo/redo/clear; never recomputed.
    undo_weight: usize,
}

impl<A: ReversibleAction> ActionHistory<A> {
    /// Create an empty history retaining at most `limit` undo entries.
    ///
    /// A `limit` of 0 means no action is retained for undo (each `apply` still
    /// runs the action, it just cannot be undone).
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit,
            weight_budget: None,
            undo_weight: 0,
        }
    }

    /// Create an empty history with both a count cap and a total-bytes cap.
    ///
    /// `count_limit` behaves exactly like [`Self::new`]'s `limit`.
    /// `weight_budget` bounds the sum of [`ReversibleAction::weight`] over the
    /// undo stack: pushing evicts oldest entries until the total is within
    /// budget, except that the single most-recent entry is always retained even
    /// if it alone exceeds the budget (you cannot drop the only undo step).
    #[must_use]
    pub fn with_weight_budget(count_limit: usize, weight_budget: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: Vec::new(),
            limit: count_limit,
            weight_budget: Some(weight_budget),
            undo_weight: 0,
        }
    }

    /// Set (or clear with `None`) the undo-stack total-bytes budget and enforce
    /// it immediately against the current stack, evicting oldest-first.
    pub fn set_weight_budget(&mut self, budget: Option<usize>) {
        self.weight_budget = budget;
        self.evict_over_budget();
    }

    /// Apply `action` to `ctx`, then record it for undo and truncate the redo
    /// branch.
    ///
    /// On success the action (now with its captured previous state populated) is
    /// pushed onto the undo stack, the redo stack is cleared (a fresh edit
    /// abandons any redoable future), and the oldest entries beyond `limit` are
    /// evicted from the front.
    ///
    /// # Errors
    /// Propagates `A::Err` from `action.apply`. On error nothing is recorded and
    /// the redo stack is left intact.
    pub fn apply(&mut self, mut action: A, ctx: &mut A::Ctx) -> Result<(), A::Err> {
        action.apply(ctx)?;
        // A new edit invalidates the redo branch: the future it represented no
        // longer follows from the current state.
        self.redo.clear();
        self.push_undo(action);
        Ok(())
    }

    /// Record an action that has ALREADY been applied to the domain by the
    /// caller, without calling `apply`.
    ///
    /// Pushes `action` onto the undo stack, clears the redo branch (a fresh edit
    /// abandons any redoable future), and evicts the oldest entries beyond
    /// `limit` — the same tail bookkeeping as [`Self::apply`], but skipping the
    /// forward `apply` call. Use this for observer-style call sites where the
    /// domain mutation already happened directly on the model and only the
    /// reversible record needs to be captured.
    ///
    /// Contract: the caller must pass an `action` whose captured state (e.g. its
    /// before/after snapshots) is already fully populated so that a later
    /// `undo`/`inverse` is valid without a prior `apply` on this instance.
    pub fn record(&mut self, action: A) {
        // A new edit invalidates the redo branch, exactly as `apply` does.
        self.redo.clear();
        self.push_undo(action);
    }

    /// Undo the most recent action, applying its inverse to `ctx`.
    ///
    /// Returns `Ok(false)` when there is nothing to undo. Otherwise the original
    /// action's inverse is applied and the original is moved onto the redo stack
    /// (mirroring Koharu: undo applies the inverse, keeps the original for redo).
    ///
    /// # Errors
    /// Propagates `A::Err` from applying the inverse. On error the original
    /// action has already been removed from the undo stack and is NOT restored;
    /// callers treat an apply failure as a corrupt-context condition.
    pub fn undo(&mut self, ctx: &mut A::Ctx) -> Result<bool, A::Err> {
        let Some(original) = self.undo.pop_back() else {
            return Ok(false);
        };
        // The entry leaves the undo stack: drop its weight from the running sum.
        self.undo_weight = self.undo_weight.saturating_sub(original.weight());
        // `inverse()` is pure/local: valid because `original` was already
        // applied (its captured prev is populated) when it entered the stack.
        let mut inverse = original.inverse();
        inverse.apply(ctx)?;
        self.redo.push(original);
        Ok(true)
    }

    /// Redo the most recently undone action, re-applying it to `ctx`.
    ///
    /// Returns `Ok(false)` when there is nothing to redo. Otherwise the original
    /// action is re-applied and pushed back onto the undo stack (respecting
    /// `limit`).
    ///
    /// # Errors
    /// Propagates `A::Err` from re-applying the action. On error the action has
    /// already been removed from the redo stack and is NOT restored.
    pub fn redo(&mut self, ctx: &mut A::Ctx) -> Result<bool, A::Err> {
        let Some(mut action) = self.redo.pop() else {
            return Ok(false);
        };
        // Re-applying re-captures `prev` from the current (post-undo) context
        // state. That is exactly Koharu's behavior and is correct: after an undo
        // the context matches the pre-apply state, so the freshly captured prev
        // equals the one this action originally held.
        action.apply(ctx)?;
        self.push_undo(action);
        Ok(true)
    }

    /// Whether there is at least one action available to undo.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether there is at least one action available to redo.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Number of actions currently available to undo.
    #[must_use]
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Number of actions currently available to redo.
    #[must_use]
    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }

    /// Retained undo-entry cap this history was constructed with.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Current total `weight()` of the undo stack, in bytes.
    #[must_use]
    pub fn undo_weight(&self) -> usize {
        self.undo_weight
    }

    /// The undo-stack total-bytes budget, or `None` if weight is unbudgeted.
    #[must_use]
    pub fn weight_budget(&self) -> Option<usize> {
        self.weight_budget
    }

    /// Label of the action that a call to `undo` would reverse, if any.
    #[must_use]
    pub fn peek_undo_label(&self) -> Option<&str> {
        self.undo.back().map(ReversibleAction::label)
    }

    /// Label of the action that a call to `redo` would re-apply, if any.
    #[must_use]
    pub fn peek_redo_label(&self) -> Option<&str> {
        self.redo.last().map(ReversibleAction::label)
    }

    /// Drop all undo and redo history. Does not touch any context.
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.undo_weight = 0;
    }

    /// Remove every undo AND redo entry for which `keep` returns `false`,
    /// preserving the order of the retained entries, and recompute the undo
    /// weight (the one place the running sum is rebuilt by iteration, because
    /// arbitrary mid-stack entries may leave).
    ///
    /// Contract: the caller must only remove entries whose absence keeps the
    /// remaining stacks valid — the removed actions must be independent of every
    /// retained action (for example, per-page raster diffs when ALL of one
    /// page's entries are removed at once). The engine cannot verify this.
    pub fn retain(&mut self, mut keep: impl FnMut(&A) -> bool) {
        self.undo.retain(|action| keep(action));
        self.redo.retain(|action| keep(action));
        self.undo_weight = self
            .undo
            .iter()
            .map(ReversibleAction::weight)
            .fold(0usize, usize::saturating_add);
    }

    /// Push an applied action onto the undo stack, add its weight to the running
    /// sum, and evict the oldest entries (front) beyond the count/weight caps.
    fn push_undo(&mut self, action: A) {
        if self.limit == 0 {
            // Undo disabled: run-but-forget. Keep the stack empty (and its
            // weight at zero — nothing is retained).
            return;
        }
        self.undo_weight = self.undo_weight.saturating_add(action.weight());
        self.undo.push_back(action);
        self.evict_over_budget();
    }

    /// Evict oldest undo entries (front) while the stack exceeds the count cap
    /// OR (a weight budget is set AND the running weight exceeds it AND more than
    /// one entry remains). The `len > 1` guard keeps the single most-recent entry
    /// even when it alone exceeds the whole budget, so there is always at least
    /// one undoable step. Each eviction subtracts the evicted entry's `weight()`
    /// from the running sum.
    fn evict_over_budget(&mut self) {
        loop {
            let over_count = self.undo.len() > self.limit;
            let over_weight = self.weight_budget.is_some_and(|b| self.undo_weight > b)
                && self.undo.len() > 1;
            if !over_count && !over_weight {
                break;
            }
            let Some(front) = self.undo.pop_front() else {
                break;
            };
            self.undo_weight = self.undo_weight.saturating_sub(front.weight());
        }
    }
}
