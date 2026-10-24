/*
File: crates/ms-tab-translation/src/tab/scrollbar_marks.rs

Purpose:
The translation-status marks the tab paints on the canvas vertical scrollbar
(`CanvasHooks::canvas_scrollbar_marks`): red while a bubble's translation is empty, blue once it
is filled, as thin stripes (`Marks`) or stripes reaching the next bubble (`UntilNext`).

Key structures:
- StatusEntry: one bubble's zoom-independent world Y and translated flag.
- ScrollbarStatusCache: the entries kept across frames under their key.

Key functions:
- translation_status_marks(): the hook body (cache lookup, then the per-frame marks).
- status_entries(): bubble set -> sorted entries (pure).

Notes:
- The hook runs on every frame the vertical bar exists. The entries need the whole chapter's
  bubble snapshot (a deep clone + sort) and a per-bubble filter/sort, so they are cached under
  `(CanvasScrollbarContext::bubbles_key, CanvasScrollbarContext::page_layout_key)`: rebuilt on
  any bubble add / delete / move / text or class change that reached the snapshot, and on any
  vertical page-layout change. Entries are in world space, so zooming does not rebuild them; only
  the cheap `ScrollMark` list is built per frame (marks are `FnOnce` and cannot be cached).
*/

use eframe::egui;
use ms_canvas::{BubbleClass, CanvasScrollbarContext, HookBubblesKey, TranslationStatusDisplay};
use ms_project::Bubble;
use ms_widgets::{MarkFill, ScrollMark, ScrollSpan};

/// One bubble's status on the scrollbar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct StatusEntry {
    /// World-space Y of the bubble anchor (`CanvasScrollbarContext::world_y`).
    world_y: f32,
    /// Whether the bubble's translation (`Bubble::text`) is non-blank.
    translated: bool,
}

/// Entries cached under the key they were built for; rebuilt when a different key is asked for.
#[derive(Debug)]
pub(super) struct KeyedStatusEntries<K> {
    key: Option<K>,
    entries: Vec<StatusEntry>,
}

impl<K> Default for KeyedStatusEntries<K> {
    fn default() -> Self {
        Self { key: None, entries: Vec::new() }
    }
}

impl<K: PartialEq> KeyedStatusEntries<K> {
    /// The entries for `key`: the cached ones when `key` equals the cached key, else `build()`'s
    /// result, which then becomes the cache.
    fn get_or_rebuild(&mut self, key: K, build: impl FnOnce() -> Vec<StatusEntry>) -> &[StatusEntry] {
        if self.key.as_ref() != Some(&key) {
            self.entries = build();
            self.key = Some(key);
        }
        &self.entries
    }
}

/// The tab's cache: keyed by the canvas bubble-set identity and the vertical page-layout key.
pub(super) type ScrollbarStatusCache = KeyedStatusEntries<(HookBubblesKey, u64)>;

/// One entry per non-hint bubble whose page is laid out (`world_y` returns `Some`), sorted by
/// world Y (stable, so equal positions keep the input order — the snapshot's id order).
///
/// A hint is not a replica: its single line lives in `text` and is never a translation, so a
/// status stripe for it would always read "translated". Same exemption as `bubble_status_style`.
pub(super) fn status_entries(bubbles: &[Bubble], world_y: impl Fn(usize, f32) -> Option<f32>) -> Vec<StatusEntry> {
    let mut entries: Vec<StatusEntry> = bubbles
        .iter()
        .filter(|bubble| bubble.bubble_class.as_deref().map(BubbleClass::from_str) != Some(BubbleClass::Hint))
        .filter_map(|bubble| {
            world_y(bubble.img_idx, bubble.img_v)
                .map(|world_y| StatusEntry { world_y, translated: !bubble.text.trim().is_empty() })
        })
        .collect();
    entries.sort_by(|a, b| a.world_y.total_cmp(&b.world_y));
    entries
}

/// Body of `CanvasHooks::canvas_scrollbar_marks` for the Translation tab. The display mode is a
/// user setting on the ribbon settings tab; `None` paints nothing and touches no cache.
pub(super) fn translation_status_marks(cache: &mut ScrollbarStatusCache, ctx: &CanvasScrollbarContext<'_>) -> Vec<ScrollMark> {
    let mode = ctx.translation_status_display();
    if mode == TranslationStatusDisplay::None {
        return Vec::new();
    }
    let key = (ctx.bubbles_key(), ctx.page_layout_key());
    let entries = cache.get_or_rebuild(key, || status_entries(&ctx.bubbles(), |img_idx, v| ctx.world_y(img_idx, v)));
    if entries.is_empty() {
        return Vec::new();
    }

    let mark_color = |translated: bool| {
        if translated {
            egui::Color32::from_rgb(40, 132, 255)
        } else {
            egui::Color32::from_rgb(220, 60, 60)
        }
    };

    if mode == TranslationStatusDisplay::Marks {
        // Thin fixed-height stripe at each bubble's own position; it does not
        // stretch down to the next bubble.
        const MARK_HALF_HEIGHT_PX: f32 = 1.0;
        return entries
            .iter()
            .map(|entry| {
                let color = mark_color(entry.translated);
                ScrollMark::custom(ScrollSpan::pixel_at(ctx.content_y_from_world(entry.world_y)), move |painter, _geom, cell| {
                    let center_y = cell.center().y;
                    let rect = egui::Rect::from_min_max(
                        egui::pos2(cell.left(), center_y - MARK_HALF_HEIGHT_PX),
                        egui::pos2(cell.right(), center_y + MARK_HALF_HEIGHT_PX),
                    );
                    painter.rect_filled(rect, 0.0, color);
                })
            })
            .collect();
    }

    // TranslationStatusDisplay::UntilNext: each bubble paints a stripe from
    // itself down to the next bubble vertically.
    let content_end = ctx.content_size_y();
    // The last bubble has no following bubble, so its mark would otherwise run
    // to the very end of the content. Instead, let it extend just a couple
    // percent of the scrollbar past the bubble, leaving the rest unpainted.
    let tail_len = content_end * 0.02;
    let mut marks = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let start = ctx.content_y_from_world(entry.world_y);
        // If another bubble follows, the mark runs down to it. The bottom-most
        // bubble's mark gets only a short tail instead of reaching the end.
        let end = entries
            .get(index + 1)
            .map_or_else(|| (start + tail_len).min(content_end), |next| ctx.content_y_from_world(next.world_y));
        if end <= start {
            continue;
        }
        marks.push(ScrollMark::fill(
            ScrollSpan::ContentPixels { start, end },
            MarkFill::Solid(mark_color(entry.translated)),
        ));
    }
    marks
}

#[cfg(test)]
mod tests {
    use super::{KeyedStatusEntries, StatusEntry, status_entries};
    use ms_project::Bubble;
    use serde_json::Map;

    fn bubble(id: i64, img_idx: usize, img_v: f32, text: &str, class: Option<&str>) -> Bubble {
        Bubble {
            id,
            img_idx,
            img_u: 0.5,
            img_v,
            side: None,
            bubble_class: class.map(str::to_string),
            bubble_type: None,
            text: text.to_string(),
            original_text: String::new(),
            extra: Map::new(),
        }
    }

    fn entry(world_y: f32, translated: bool) -> StatusEntry {
        StatusEntry { world_y, translated }
    }

    // Page 0 spans world y 0..100, page 1 spans 100..300, page 2 is not laid out.
    fn world_y(img_idx: usize, v: f32) -> Option<f32> {
        match img_idx {
            0 => Some(v * 100.0),
            1 => Some(100.0 + v * 200.0),
            _ => None,
        }
    }

    #[test]
    fn entries_skip_hints_and_unlaid_pages_and_sort_by_position() {
        let bubbles = vec![
            bubble(1, 1, 0.5, "done", None),
            bubble(2, 0, 0.5, "  ", Some("text")),
            bubble(3, 0, 0.1, "note", Some("hint")),
            bubble(4, 2, 0.1, "x", None),
            bubble(5, 0, 0.9, "", Some("image")),
        ];
        assert_eq!(
            status_entries(&bubbles, world_y),
            vec![entry(50.0, false), entry(90.0, false), entry(200.0, true)]
        );
    }

    #[test]
    fn equal_positions_keep_snapshot_order() {
        let bubbles = vec![bubble(1, 0, 0.5, "a", None), bubble(2, 0, 0.5, "", None)];
        assert_eq!(status_entries(&bubbles, world_y), vec![entry(50.0, true), entry(50.0, false)]);
    }

    #[test]
    fn cache_rebuilds_only_when_the_key_changes() {
        let mut cache = KeyedStatusEntries::<(u64, u64)>::default();
        let mut builds = 0;
        let mut build = |value: f32| {
            builds += 1;
            vec![entry(value, true)]
        };
        assert_eq!(cache.get_or_rebuild((1, 7), || build(1.0)), &[entry(1.0, true)]);
        assert_eq!(cache.get_or_rebuild((1, 7), || build(2.0)), &[entry(1.0, true)], "same key reuses the entries");
        assert_eq!(cache.get_or_rebuild((2, 7), || build(3.0)), &[entry(3.0, true)], "a bubble change rebuilds");
        assert_eq!(cache.get_or_rebuild((2, 8), || build(4.0)), &[entry(4.0, true)], "a layout change rebuilds");
        assert_eq!(builds, 3);
    }
}
