/*
File: crates/ms-actions/tests/history.rs

Purpose:
Contract tests for the generic `ms-actions` engine. Because Phase 0 defines no
domain types, these tests declare a tiny in-test `SetOp` action over a
`Vec<i32>` context that captures the overwritten cell into `prev`, and exercise
the self-inverting contract plus the `ActionHistory` undo/redo/limit semantics.
*/

use ms_actions::{ActionHistory, ReversibleAction};

/// In-test action: overwrite `ctx[index]` with `new`, capturing the previous
/// value into `prev` on `apply` so `inverse` is pure. Exercises the
/// self-inverting `apply`-captures-`prev` contract.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SetOp {
    index: usize,
    new: i32,
    prev: Option<i32>,
    name: String,
}

/// Error for the in-test action: the target index was out of bounds.
#[derive(Debug, PartialEq, Eq)]
struct OutOfBounds(usize);

impl SetOp {
    fn new(index: usize, new: i32) -> Self {
        Self {
            index,
            new,
            prev: None,
            name: format!("set[{index}]={new}"),
        }
    }
}

impl ReversibleAction for SetOp {
    type Ctx = Vec<i32>;
    type Err = OutOfBounds;

    fn apply(&mut self, ctx: &mut Self::Ctx) -> Result<(), Self::Err> {
        let cell = ctx.get_mut(self.index).ok_or(OutOfBounds(self.index))?;
        // Capture the value we are about to overwrite so `inverse` is pure.
        self.prev = Some(*cell);
        *cell = self.new;
        Ok(())
    }

    fn inverse(&self) -> Self {
        // Pure: build the reverse purely from captured `prev`. `prev` is present
        // because the engine only calls `inverse` after `apply` on this instance.
        let prev = self.prev.expect("inverse requires apply to have captured prev");
        Self {
            index: self.index,
            new: prev,
            prev: None,
            name: format!("undo {}", self.name),
        }
    }

    fn label(&self) -> &str {
        &self.name
    }
}

#[test]
fn apply_undo_restores_previous_then_redo_reapplies() {
    let mut ctx = vec![10, 20, 30];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(16);

    hist.apply(SetOp::new(1, 99), &mut ctx).expect("apply");
    assert_eq!(ctx, vec![10, 99, 30]);
    assert!(hist.can_undo());
    assert!(!hist.can_redo());

    assert!(hist.undo(&mut ctx).expect("undo"));
    assert_eq!(ctx, vec![10, 20, 30]);
    assert!(!hist.can_undo());
    assert!(hist.can_redo());

    assert!(hist.redo(&mut ctx).expect("redo"));
    assert_eq!(ctx, vec![10, 99, 30]);
    assert!(hist.can_undo());
    assert!(!hist.can_redo());
}

#[test]
fn apply_then_inverse_round_trips() {
    let mut ctx = vec![5, 6, 7];
    let mut op = SetOp::new(2, 42);
    op.apply(&mut ctx).expect("apply");
    assert_eq!(ctx, vec![5, 6, 42]);

    // Applying the inverse must return ctx to its original state.
    let mut inv = op.inverse();
    inv.apply(&mut ctx).expect("apply inverse");
    assert_eq!(ctx, vec![5, 6, 7]);
}

#[test]
fn fresh_apply_after_undo_clears_redo() {
    let mut ctx = vec![0, 0];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(16);

    hist.apply(SetOp::new(0, 1), &mut ctx).expect("apply a");
    assert!(hist.undo(&mut ctx).expect("undo a"));
    assert!(hist.can_redo());

    // A brand-new edit after an undo must truncate the redo branch.
    hist.apply(SetOp::new(1, 2), &mut ctx).expect("apply b");
    assert!(!hist.can_redo());
    assert_eq!(hist.redo_len(), 0);
    assert_eq!(ctx, vec![0, 2]);
}

#[test]
fn limit_evicts_oldest_undo_from_front() {
    let mut ctx = vec![0];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(2);

    hist.apply(SetOp::new(0, 1), &mut ctx).expect("apply 1"); // evicted later
    hist.apply(SetOp::new(0, 2), &mut ctx).expect("apply 2");
    hist.apply(SetOp::new(0, 3), &mut ctx).expect("apply 3");
    assert_eq!(ctx, vec![3]);
    // Only the two most recent actions are retained.
    assert_eq!(hist.undo_len(), 2);

    // Undo the two retained actions: 3->2, then 2->1.
    assert!(hist.undo(&mut ctx).expect("undo 3"));
    assert_eq!(ctx, vec![2]);
    assert!(hist.undo(&mut ctx).expect("undo 2"));
    assert_eq!(ctx, vec![1]);

    // The first action (0->1) was evicted; undo beyond the limit is unavailable
    // and the value never returns to its original 0.
    assert!(!hist.can_undo());
    assert!(!hist.undo(&mut ctx).expect("undo empty"));
    assert_eq!(ctx, vec![1]);
}

#[test]
fn can_undo_can_redo_transitions() {
    let mut ctx = vec![0];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(16);

    assert!(!hist.can_undo());
    assert!(!hist.can_redo());

    hist.apply(SetOp::new(0, 1), &mut ctx).expect("apply");
    assert!(hist.can_undo());
    assert!(!hist.can_redo());

    hist.undo(&mut ctx).expect("undo");
    assert!(!hist.can_undo());
    assert!(hist.can_redo());

    hist.redo(&mut ctx).expect("redo");
    assert!(hist.can_undo());
    assert!(!hist.can_redo());
}

#[test]
fn multi_step_sequence_leaves_expected_state() {
    let mut ctx = vec![0, 0, 0];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(16);

    // apply, apply, undo, undo, redo
    hist.apply(SetOp::new(0, 1), &mut ctx).expect("apply 0");
    hist.apply(SetOp::new(1, 2), &mut ctx).expect("apply 1");
    assert_eq!(ctx, vec![1, 2, 0]);

    assert!(hist.undo(&mut ctx).expect("undo 1")); // reverts index 1
    assert_eq!(ctx, vec![1, 0, 0]);
    assert!(hist.undo(&mut ctx).expect("undo 0")); // reverts index 0
    assert_eq!(ctx, vec![0, 0, 0]);

    assert!(hist.redo(&mut ctx).expect("redo 0")); // re-applies index 0
    assert_eq!(ctx, vec![1, 0, 0]);

    // One redo remains available (the index-1 edit); undo now available too.
    assert!(hist.can_undo());
    assert!(hist.can_redo());
    assert_eq!(hist.redo_len(), 1);
}

#[test]
fn peek_labels_report_next_action() {
    let mut ctx = vec![0];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(16);
    assert_eq!(hist.peek_undo_label(), None);
    assert_eq!(hist.peek_redo_label(), None);

    hist.apply(SetOp::new(0, 7), &mut ctx).expect("apply");
    assert_eq!(hist.peek_undo_label(), Some("set[0]=7"));

    hist.undo(&mut ctx).expect("undo");
    assert_eq!(hist.peek_redo_label(), Some("set[0]=7"));
}

#[test]
fn apply_error_leaves_history_untouched() {
    let mut ctx = vec![0, 0];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(16);

    hist.apply(SetOp::new(0, 5), &mut ctx).expect("apply ok");
    assert!(hist.undo(&mut ctx).expect("undo"));
    assert!(hist.can_redo());

    // Out-of-bounds apply must fail and leave the redo branch intact.
    let err = hist.apply(SetOp::new(9, 1), &mut ctx).expect_err("must fail");
    assert_eq!(err, OutOfBounds(9));
    assert!(hist.can_redo());
    assert_eq!(hist.undo_len(), 0);
}

#[test]
fn record_then_undo_reverses_and_clears_redo() {
    // `record` is the observer-style entry point: the caller already applied the
    // mutation to the domain, so `record` only captures the reversible op WITHOUT
    // re-running `apply`. Build an op whose `apply` already ran (its `prev` is
    // populated) and hand it to `record`; undo must then reverse it.
    let mut ctx = vec![10, 20, 30];

    // Simulate the caller mutating the domain directly, capturing before/after.
    let before = ctx[1];
    ctx[1] = 99; // the caller's own mutation, not via the engine
    let recorded = SetOp {
        index: 1,
        new: 99,
        prev: Some(before), // populated as if `apply` had already run
        name: "set[1]=99".to_string(),
    };

    let mut hist: ActionHistory<SetOp> = ActionHistory::new(16);
    // A stale redoable future that `record` must truncate.
    hist.apply(SetOp::new(0, 1), &mut ctx).expect("seed apply");
    assert!(hist.undo(&mut ctx).expect("seed undo"));
    assert!(hist.can_redo());

    hist.record(recorded);
    // record does not touch the context: the caller's mutation stands unchanged.
    assert_eq!(ctx, vec![10, 99, 30]);
    // A fresh record truncates the redo branch just like `apply`.
    assert!(!hist.can_redo());
    assert!(hist.can_undo());

    // Undo reverses the recorded op using its captured `prev`.
    assert!(hist.undo(&mut ctx).expect("undo recorded"));
    assert_eq!(ctx, vec![10, 20, 30]);
    assert!(hist.can_redo());

    // Redo re-applies it.
    assert!(hist.redo(&mut ctx).expect("redo recorded"));
    assert_eq!(ctx, vec![10, 99, 30]);
}

#[test]
fn record_respects_limit_eviction() {
    // `record` shares `apply`'s tail bookkeeping, so the oldest entries beyond the
    // limit are evicted from the front and become non-undoable.
    let mut ctx = vec![0];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(2);

    for (new, prev) in [(1, 0), (2, 1), (3, 2)] {
        ctx[0] = new;
        hist.record(SetOp {
            index: 0,
            new,
            prev: Some(prev),
            name: format!("set[0]={new}"),
        });
    }
    assert_eq!(hist.undo_len(), 2); // only the two most recent are retained

    assert!(hist.undo(&mut ctx).expect("undo 3"));
    assert_eq!(ctx, vec![2]);
    assert!(hist.undo(&mut ctx).expect("undo 2"));
    assert_eq!(ctx, vec![1]);
    assert!(!hist.can_undo()); // the first record was evicted
}

#[test]
fn zero_limit_runs_but_does_not_retain() {
    let mut ctx = vec![0];
    let mut hist: ActionHistory<SetOp> = ActionHistory::new(0);

    hist.apply(SetOp::new(0, 3), &mut ctx).expect("apply");
    assert_eq!(ctx, vec![3]); // action still ran
    assert!(!hist.can_undo()); // but nothing retained
}

/// In-test action carrying an explicit `weight` so the weight-budget eviction of
/// `ActionHistory` can be exercised without a real raster payload. Behaves like
/// `SetOp` (overwrite `ctx[index]`, capture `prev`) plus a `weight()` override.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WeightedOp {
    index: usize,
    new: i32,
    prev: Option<i32>,
    weight: usize,
    name: String,
}

impl WeightedOp {
    fn new(index: usize, new: i32, weight: usize) -> Self {
        Self {
            index,
            new,
            prev: None,
            weight,
            name: format!("wset[{index}]={new}(w{weight})"),
        }
    }
}

impl ReversibleAction for WeightedOp {
    type Ctx = Vec<i32>;
    type Err = OutOfBounds;

    fn apply(&mut self, ctx: &mut Self::Ctx) -> Result<(), Self::Err> {
        let cell = ctx.get_mut(self.index).ok_or(OutOfBounds(self.index))?;
        self.prev = Some(*cell);
        *cell = self.new;
        Ok(())
    }

    fn inverse(&self) -> Self {
        let prev = self.prev.expect("inverse requires apply to have captured prev");
        Self {
            index: self.index,
            new: prev,
            prev: None,
            weight: self.weight,
            name: format!("undo {}", self.name),
        }
    }

    fn label(&self) -> &str {
        &self.name
    }

    fn weight(&self) -> usize {
        self.weight
    }
}

#[test]
fn weight_budget_evicts_oldest_until_under_budget() {
    // Budget 100 bytes, generous count cap. Each op weighs 40, so at most two fit
    // (80 <= 100); a third pushes the total to 120 and evicts the oldest.
    let mut ctx = vec![0];
    let mut hist: ActionHistory<WeightedOp> = ActionHistory::with_weight_budget(1000, 100);
    assert_eq!(hist.weight_budget(), Some(100));

    hist.apply(WeightedOp::new(0, 1, 40), &mut ctx).expect("a");
    assert_eq!(hist.undo_weight(), 40);
    hist.apply(WeightedOp::new(0, 2, 40), &mut ctx).expect("b");
    assert_eq!(hist.undo_weight(), 80);
    assert_eq!(hist.undo_len(), 2);

    // Third push -> 120 > 100 -> evict oldest (the first 40) -> back to 80.
    hist.apply(WeightedOp::new(0, 3, 40), &mut ctx).expect("c");
    assert_eq!(hist.undo_len(), 2);
    assert_eq!(hist.undo_weight(), 80);

    // Undo the two retained ops (3->2, 2->1); the first (0->1) was evicted.
    assert!(hist.undo(&mut ctx).expect("undo c"));
    assert_eq!(ctx, vec![2]);
    assert_eq!(hist.undo_weight(), 40);
    assert!(hist.undo(&mut ctx).expect("undo b"));
    assert_eq!(ctx, vec![1]);
    assert_eq!(hist.undo_weight(), 0);
    assert!(!hist.can_undo());
}

#[test]
fn single_entry_larger_than_budget_is_retained() {
    // A lone entry heavier than the whole budget must stay: you cannot drop the
    // only undo step. The `undo_len > 1` guard prevents evicting it.
    let mut ctx = vec![0];
    let mut hist: ActionHistory<WeightedOp> = ActionHistory::with_weight_budget(1000, 100);

    hist.apply(WeightedOp::new(0, 9, 500), &mut ctx).expect("big");
    assert_eq!(hist.undo_len(), 1, "the sole entry is retained despite over-budget");
    assert_eq!(hist.undo_weight(), 500);
    assert!(hist.can_undo());

    // It remains fully undoable.
    assert!(hist.undo(&mut ctx).expect("undo big"));
    assert_eq!(ctx, vec![0]);
    assert_eq!(hist.undo_weight(), 0);
}

#[test]
fn undo_and_redo_update_undo_weight() {
    let mut ctx = vec![0];
    // No weight budget here: just verify the running sum tracks push/undo/redo.
    let mut hist: ActionHistory<WeightedOp> = ActionHistory::new(16);
    assert_eq!(hist.weight_budget(), None);

    hist.apply(WeightedOp::new(0, 1, 30), &mut ctx).expect("a");
    hist.apply(WeightedOp::new(0, 2, 70), &mut ctx).expect("b");
    assert_eq!(hist.undo_weight(), 100);

    // Undo moves the newest (70) off the undo stack -> weight drops by 70.
    assert!(hist.undo(&mut ctx).expect("undo"));
    assert_eq!(hist.undo_weight(), 30);

    // Redo re-pushes it -> weight returns to 100.
    assert!(hist.redo(&mut ctx).expect("redo"));
    assert_eq!(hist.undo_weight(), 100);

    // Clear zeroes the running sum.
    hist.clear();
    assert_eq!(hist.undo_weight(), 0);
}

#[test]
fn count_limit_and_weight_budget_bind_independently() {
    // Count cap 3, weight budget 100. With small weights the COUNT cap binds
    // first; with large weights the WEIGHT budget binds first.
    let mut ctx = vec![0];
    let mut hist: ActionHistory<WeightedOp> = ActionHistory::with_weight_budget(3, 100);

    // Four weight-10 ops: total 40 stays under budget, but count cap 3 evicts the
    // oldest -> 3 entries, weight 30.
    for v in 1..=4 {
        hist.apply(WeightedOp::new(0, v, 10), &mut ctx).expect("small");
    }
    assert_eq!(hist.undo_len(), 3, "count cap binds first for light ops");
    assert_eq!(hist.undo_weight(), 30);

    // Now push a weight-90 op: total 30+90=120 > 100 (weight binds before count).
    // Evict oldest weight-10 entries until under budget: 120->110->100 (two
    // evictions leaves one weight-10 + the weight-90 = 100 <= 100).
    hist.apply(WeightedOp::new(0, 5, 90), &mut ctx).expect("heavy");
    assert_eq!(hist.undo_weight(), 100);
    assert!(hist.undo_len() <= 3, "still within count cap");
    assert_eq!(hist.undo_weight(), 100, "weight budget respected");
}

#[test]
fn set_weight_budget_enforces_against_existing_stack() {
    // Build up an over-budget stack with no budget, then set one and confirm the
    // setter evicts down to fit.
    let mut ctx = vec![0];
    let mut hist: ActionHistory<WeightedOp> = ActionHistory::new(16);
    for v in 1..=4 {
        hist.apply(WeightedOp::new(0, v, 40), &mut ctx).expect("push");
    }
    assert_eq!(hist.undo_weight(), 160);

    // Budget 100 -> evict oldest weight-40 entries until <=100: 160->120->80.
    hist.set_weight_budget(Some(100));
    assert_eq!(hist.undo_weight(), 80);
    assert_eq!(hist.undo_len(), 2);
}

#[test]
fn retain_drops_matching_entries_from_both_stacks_and_recomputes_weight() {
    // Two independent cells; retain removes every entry touching cell 0 from
    // BOTH stacks and rebuilds the undo weight from the survivors.
    let mut ctx = vec![0, 0];
    let mut hist: ActionHistory<WeightedOp> = ActionHistory::new(16);
    hist.apply(WeightedOp::new(0, 1, 10), &mut ctx).expect("a");
    hist.apply(WeightedOp::new(1, 2, 20), &mut ctx).expect("b");
    hist.apply(WeightedOp::new(0, 3, 40), &mut ctx).expect("c");
    // Move the newest cell-0 entry onto the redo stack too.
    assert!(hist.undo(&mut ctx).expect("undo c"));
    assert_eq!(hist.undo_weight(), 30);
    assert!(hist.can_redo());

    hist.retain(|op| op.index != 0);

    // Only the cell-1 entry survives; the redo branch for cell 0 is gone.
    assert_eq!(hist.undo_len(), 1);
    assert!(!hist.can_redo());
    assert_eq!(hist.undo_weight(), 20);
    assert!(hist.undo(&mut ctx).expect("undo b"));
    assert_eq!(ctx, vec![1, 0]);
    assert!(!hist.can_undo());
}
