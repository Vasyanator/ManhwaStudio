/*
File: crates/ms-canvas/src/bubble_action.rs

Purpose:
Concrete bubble undo/redo action (family A of the unified action system, see
`docs/unified_action_system.md`, "Фаза 1 — пузыри на Op"). Bubbles are cheap
field data, so this is a behavior-preserving FULL snapshot op, not a field-level
typed patch: it carries the whole bubble list before and after one logical
mutation and reverses it by resetting `BubblesModel` to the stored snapshot.

Key structures:
- BubbleSnapshotOp: `ms_actions::ReversibleAction` over `BubblesModel`.
- BubbleSnapshotOpError: reset failure surfaced by the model.

Notes:
- Both `before` and `after` are known at construction (the caller mutates the
  model directly and records afterward), so `apply` does not need to capture a
  prev and `inverse` is a pure swap. The trait permits this.
- Snapshots are shared `Arc<Vec<Bubble>>`, so construction is an O(1) refcount
  bump, not a deep clone of the bubble list.
*/

use std::fmt;
use std::sync::Arc;

use ms_actions::ReversibleAction;

use ms_models::bubbles_model::BubblesModel;
use ms_project::Bubble;

/// Error returned when a `BubbleSnapshotOp` cannot reset `BubblesModel` to its
/// stored snapshot.
///
/// `BubblesModel::reset` is effectively infallible today (it only republishes a
/// snapshot to the coalescing saver channel), but its signature is fallible, so
/// the op surfaces the failure as a typed error instead of unwrapping.
#[derive(Debug)]
pub(crate) enum BubbleSnapshotOpError {
    /// Resetting the model to the target snapshot failed; carries the model error.
    Reset(anyhow::Error),
}

impl fmt::Display for BubbleSnapshotOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reset(err) => write!(f, "failed to reset bubbles model: {err:#}"),
        }
    }
}

impl std::error::Error for BubbleSnapshotOpError {}

/// One reversible bubble mutation, stored as full before/after snapshots.
///
/// `before` is the whole bubble list as it was before the logical mutation;
/// `after` is the list after it. Applying the op resets the model to `after`;
/// its inverse resets the model to `before`. Both snapshots are shared, so the
/// op is cheap to build and clone.
#[derive(Clone)]
pub(crate) struct BubbleSnapshotOp {
    /// Human-readable label for UI history / logging.
    label: String,
    /// Bubble list before the mutation. Applying the inverse restores this.
    before: Arc<Vec<Bubble>>,
    /// Bubble list after the mutation. Applying the op restores this.
    after: Arc<Vec<Bubble>>,
}

impl fmt::Debug for BubbleSnapshotOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BubbleSnapshotOp")
            .field("label", &self.label)
            .field("before_len", &self.before.len())
            .field("after_len", &self.after.len())
            .finish()
    }
}

impl BubbleSnapshotOp {
    /// Builds an op capturing one logical bubble mutation.
    ///
    /// `before`/`after` are shared snapshots of the whole bubble list around the
    /// mutation; `label` names the action for history/logging.
    pub(crate) fn new(label: String, before: Arc<Vec<Bubble>>, after: Arc<Vec<Bubble>>) -> Self {
        Self {
            label,
            before,
            after,
        }
    }
}

impl ReversibleAction for BubbleSnapshotOp {
    type Ctx = BubblesModel;
    type Err = BubbleSnapshotOpError;

    /// Resets the model to the `after` snapshot.
    ///
    /// No prev-capture is needed because `before` is already stored, so the
    /// inverse is pure. Called by the engine on redo; the observer-style record
    /// path does NOT call it (the caller already performed the mutation).
    fn apply(&mut self, ctx: &mut Self::Ctx) -> Result<(), Self::Err> {
        ctx.reset(self.after.as_ref().clone())
            .map_err(BubbleSnapshotOpError::Reset)
    }

    /// Produces the reverse op by swapping `before`/`after`.
    ///
    /// Pure and local: applying the returned op resets the model to `before`,
    /// undoing this op.
    fn inverse(&self) -> Self {
        Self {
            label: format!("Undo: {}", self.label),
            before: Arc::clone(&self.after),
            after: Arc::clone(&self.before),
        }
    }

    fn label(&self) -> &str {
        &self.label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_models::bubbles_model::{
        SharedCanvasSettings, runtime_bubble_to_record,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_model(bubbles: Vec<Bubble>) -> BubblesModel {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let unsaved: PathBuf =
            std::env::temp_dir().join(format!("manhwastudio_bubble_action_test_{n}.json"));
        BubblesModel::new(
            bubbles,
            unsaved.with_extension("saved.json"),
            unsaved,
            SharedCanvasSettings::default(),
            None,
        )
    }

    fn bubble(id: i64, u: f32) -> Bubble {
        runtime_bubble_to_record(
            id,
            0,
            u,
            0.5,
            Some("left".to_string()),
            Some("text".to_string()),
            Some("aside".to_string()),
            String::new(),
            String::new(),
            None,
        )
    }

    #[test]
    fn apply_and_inverse_round_trip_through_the_engine() {
        use ms_actions::ActionHistory;

        // A model with one bubble; the caller "mutates" it to two bubbles and
        // records the before/after snapshots, then undo must restore the single
        // bubble and redo must restore the pair.
        let before = Arc::new(vec![bubble(1, 0.3)]);
        let after = Arc::new(vec![bubble(1, 0.3), bubble(2, 0.7)]);
        let mut model = test_model(before.as_ref().clone());

        // Simulate the caller's own direct mutation of the model.
        model.reset(after.as_ref().clone()).expect("seed mutation");
        assert_eq!(model.snapshot().len(), 2);

        let mut hist: ActionHistory<BubbleSnapshotOp> = ActionHistory::new(128);
        hist.record(BubbleSnapshotOp::new(
            "add bubble".to_string(),
            Arc::clone(&before),
            Arc::clone(&after),
        ));

        // Undo restores the pre-mutation single-bubble list.
        assert!(hist.undo(&mut model).expect("undo"));
        let after_undo = model.snapshot();
        assert_eq!(after_undo.len(), 1);
        assert_eq!(after_undo[0].id, 1);

        // Redo restores the post-mutation pair.
        assert!(hist.redo(&mut model).expect("redo"));
        let after_redo = model.snapshot();
        assert_eq!(after_redo.len(), 2);
        assert_eq!(after_redo[1].id, 2);
    }
}
