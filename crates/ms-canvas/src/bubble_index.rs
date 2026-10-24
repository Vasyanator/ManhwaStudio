/*
File: crates/ms-canvas/src/bubble_index.rs

Purpose:
Per-frame `bubble id -> persisted bubble` lookup over `ProjectData::bubbles`, shared by the aside
and on-top bubble passes of one `CanvasView::draw`.

Key structures:
- FrameBubbleIndex: the lazily built, frame-scoped `id -> position` map held by `CanvasView`.

Key functions:
- FrameBubbleIndex::reset() / get_or_build(): frame lifecycle (reset at the start and end of `draw`).
- BubblePositions::resolve(): O(1) lookup that can never return a bubble with another id.

Notes:
- The map stores POSITIONS, not references, so it can live on `CanvasView` between the passes of
  one frame without borrowing the frame's `ProjectData`. It is rebuilt every frame rather than
  cached across frames: `ProjectData::bubbles` is an `Arc<Vec<_>>` that the bubbles model mutates in
  place (`Arc::make_mut`) and that may be reallocated at a reused address, so neither pointer nor
  length identifies its content. One build per frame replaces the former build per non-empty aside
  side per visible page per pass, plus the on-top pass's linear scan per visible bubble.
*/

use ms_project::Bubble;
use std::collections::HashMap;
use std::sync::Arc;

/// An `id -> position in ProjectData::bubbles` map for one frame's `ProjectData`.
#[derive(Debug, Default)]
pub(super) struct BubblePositions {
    positions: HashMap<i64, usize>,
}

impl BubblePositions {
    /// Indexes `bubbles` by id. Ids are unique in a project; on a duplicate the FIRST entry wins,
    /// matching the `iter().find()` lookup this index replaces.
    #[must_use]
    pub(super) fn build(bubbles: &[Bubble]) -> Self {
        let mut positions = HashMap::with_capacity(bubbles.len());
        for (position, bubble) in bubbles.iter().enumerate() {
            positions.entry(bubble.id).or_insert(position);
        }
        Self { positions }
    }

    /// The bubble with id `bid` in `bubbles`, which must be the slice this index was built from.
    /// Returns `None` when the id is absent; a position that no longer holds `bid` (a slice other
    /// than the indexed one) also yields `None` instead of a wrong bubble.
    #[must_use]
    pub(super) fn resolve<'a>(&self, bubbles: &'a [Bubble], bid: i64) -> Option<&'a Bubble> {
        self.positions
            .get(&bid)
            .and_then(|&position| bubbles.get(position))
            .filter(|bubble| bubble.id == bid)
    }
}

/// The frame-scoped, lazily built [`BubblePositions`] of a `CanvasView`.
#[derive(Debug, Default)]
pub(super) struct FrameBubbleIndex {
    current: Option<Arc<BubblePositions>>,
}

impl FrameBubbleIndex {
    /// Forgets the index; the next [`Self::get_or_build`] rebuilds it. Called at the start and end
    /// of every `CanvasView::draw`, so an index never outlives the `ProjectData` of its frame.
    pub(super) fn reset(&mut self) {
        self.current = None;
    }

    /// This frame's index over `bubbles`, built on first use. The `Arc` lets a pass keep it while
    /// it mutably borrows the canvas.
    #[must_use]
    pub(super) fn get_or_build(&mut self, bubbles: &[Bubble]) -> Arc<BubblePositions> {
        Arc::clone(
            self.current
                .get_or_insert_with(|| Arc::new(BubblePositions::build(bubbles))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{BubblePositions, FrameBubbleIndex};
    use ms_project::Bubble;
    use serde_json::Map;
    use std::sync::Arc;

    fn test_bubble(id: i64, text: &str) -> Bubble {
        Bubble {
            id,
            img_idx: 0,
            img_u: 0.0,
            img_v: 0.0,
            side: None,
            bubble_class: None,
            bubble_type: None,
            text: text.to_string(),
            original_text: String::new(),
            extra: Map::new(),
        }
    }

    // Present ids resolve to the actual slice element; absent ids report `None`, so the callers'
    // runtime fallback path behaves exactly as with the former linear `find`.
    #[test]
    fn resolves_present_and_missing_ids() {
        let bubbles = vec![test_bubble(7, ""), test_bubble(3, ""), test_bubble(42, "")];
        let index = BubblePositions::build(&bubbles);
        for id in [7, 3, 42] {
            assert_eq!(index.resolve(&bubbles, id).map(|b| b.id), Some(id));
        }
        assert!(index.resolve(&bubbles, 100).is_none());
        assert!(index.resolve(&bubbles, 3).is_some_and(|b| std::ptr::eq(b, &bubbles[1])));
    }

    #[test]
    fn duplicate_ids_resolve_to_the_first_entry_like_find() {
        let bubbles = vec![test_bubble(5, "first"), test_bubble(5, "second")];
        let index = BubblePositions::build(&bubbles);
        assert_eq!(index.resolve(&bubbles, 5).map(|b| b.text.as_str()), Some("first"));
    }

    #[test]
    fn a_foreign_slice_never_yields_a_bubble_with_another_id() {
        let indexed = vec![test_bubble(1, ""), test_bubble(2, "")];
        let other = vec![test_bubble(2, ""), test_bubble(1, "")];
        let index = BubblePositions::build(&indexed);
        assert!(index.resolve(&other, 1).is_none());
        assert!(index.resolve(&other[..1], 2).is_none());
    }

    #[test]
    fn frame_index_is_built_once_until_reset() {
        let first = vec![test_bubble(1, "")];
        let second = vec![test_bubble(2, "")];
        let mut frame = FrameBubbleIndex::default();
        let built = frame.get_or_build(&first);
        assert!(Arc::ptr_eq(&built, &frame.get_or_build(&second)), "one build per frame");
        frame.reset();
        let rebuilt = frame.get_or_build(&second);
        assert_eq!(rebuilt.resolve(&second, 2).map(|b| b.id), Some(2));
        assert!(rebuilt.resolve(&first, 1).is_none());
    }
}
