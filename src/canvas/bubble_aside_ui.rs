/*
File: src/canvas/bubble_aside_ui.rs

Purpose:
Aside bubble UI subsystem for `CanvasView`: column layout, repack, link rendering,
hit-testing, and drag interactions for side-mounted bubbles.

Main responsibilities:
- layout aside items (whole bubbles, or one per image text area) into left/right columns;
- optionally split a side into two side-by-side columns (near/far) when room allows;
- render bubble cards and per-area anchor links without pulling persistence into UI code;
- render the editable multi-area image bubble card (preview, framed row blocks, add/remove area);
- manage aside drag lifecycle for bubble-body, rect-area, and image red-rect/area/anchor moves;
- keep runtime bubble geometry in sync with scene interactions.

Key structures:
- AsidePageContext: per-page shared bundle (ui, project, hooks, page geometry, link sink) threaded
  as `&mut` through the whole layout chain; `CanvasView` stays a SEPARATE `&mut` param.
- AsideColumnLayout: per-column sizing/style bundle shared by the measure and draw passes.
- AsideBodyMeasure: grouped inputs for the card body-height estimator.

Key functions:
- draw_aside_for_page()
- draw_aside_side()  -> single- vs two-column dispatch per side
- draw_aside_column() / draw_aside_two_columns()
- build_aside_desired_slots() -> pack_aside_slots() -> draw_aside_slots()
- draw_editable_image_areas()
- draw_image_bubble_page_overlay()
- aside_hit_test()
- drag_aside_by_pointer()

Notes:
- Layout is split into three reusable stages: build desired slots (measure), pack them vertically
  (pure geometry in `pack_aside_slots`, unit tested), then draw the packed slots.
- The per-cluster crossing minimizer inside `pack_aside_slots` is the audited low-zoom hot spot
  (one ~48-card cluster, O(n^3) swap loop per frame). It is now bounded (`ASIDE_PACK_MAX_PASSES`)
  and memoized across frames: the converged item ordering is cached in egui temp data keyed by an
  exact-bit cluster fingerprint (`cluster_crossing_fingerprint` / `hash_crossing_coord`), so a steady
  camera replays the stored permutation instead of recomputing. The fingerprint hashes the exact
  IEEE-754 bits of every crossing-relevant coordinate (not a coarse quantization bucket): the crossing
  decision in `lines_cross` is a pure function of those coordinates and amplifies them by an unbounded
  slope ratio near its `0.0001` denominator guard, so any quantization larger than the `±0.001`
  comparison epsilon could let a cache hit replay a stale, link-crossing layout. The cache mirrors the
  `helpers.rs` text-measure cache (bounded `Arc<Mutex<HashMap>>`, clear-on-overflow); unit tests pass
  `None` for the pure path.
- Two-column mode (`CanvasState::aside_second_column`) activates per side only while both columns
  plus the three gaps fit inside the viewport (so the far column never spills past the edge before
  its bubbles appear). Columns are equal width (>= min width, capped at max only when stretching is
  on; both stay at min width and hug the ribbon when stretching is off). Distribution is
  near-priority: isolated bubbles stay near, and only overlapping clusters are split alternately
  near/far. Far links stay roughly horizontal, while the near column packs invisible spacers at each
  far anchor height so near cards spread apart and far links thread between them.
- Persistence and shared-model sync remain in `bubble_runtime.rs`; image text-area parse/serialize
  helpers live in `helpers.rs`.
- A read-only image bubble splits into one `AsideItem` per text area; an editable one stays a single
  card whose blocks each own a colored rect, anchor point, and link.
- This module only drives `CanvasView` through existing runtime/edit helpers.
*/

use super::helpers::{
    draw_anchor_link, measure_text_widget_compact_width, measure_text_widget_content_height,
    normalize_image_text_areas, with_bubble_text_font,
};
use super::types::{
    AsideBubbleCompactMode, AsideBubbleSideMode, AsideDragTarget, AsideItem, BubbleClass,
    BubbleLink, BubbleMenuCommand, BubbleMenuContext, BubbleTextField, BubbleType,
    CanvasScenePageFrame, ImageTextArea, RectCoords, image_area_palette,
    image_bubble_side_from_areas,
};
use super::{CanvasHooks, CanvasView};
use crate::bubble_status::paint_bubble_status_border;
use crate::project::{Bubble, ProjectData, Side};
use crate::widgets::{SpellcheckedTextEdit, misspelled_word_at_pointer};
use eframe::egui;
use egui::{Align, Color32, CornerRadius, Id, Pos2, Rect, Sense, Stroke};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

/// Builds a per-frame `id -> &Bubble` index over `project.bubbles` so aside-draw passes can resolve
/// the persisted bubble for a given id in O(1) instead of a linear scan per visible item.
///
/// The returned references borrow `project` immutably for the lifetime of the map; callers must not
/// take a mutable borrow of `project` while the map is alive (`project` is a shared reference in the
/// aside-draw path, so this holds naturally).
fn build_project_bubble_index(project: &ProjectData) -> HashMap<i64, &Bubble> {
    index_bubbles_by_id(&project.bubbles)
}

/// Maps a slice of bubbles into an `id -> &Bubble` lookup. Pure (no `ProjectData`/egui), so it is
/// unit testable. Bubble ids are unique in a project; on a duplicate id the last entry wins, which
/// matches the former `iter().find()` returning the first match only when ids are distinct.
fn index_bubbles_by_id(bubbles: &[Bubble]) -> HashMap<i64, &Bubble> {
    bubbles.iter().map(|b| (b.id, b)).collect()
}

/// egui temp-data id for the cross-frame aside crossing-minimization permutation cache.
const ASIDE_PACK_PERM_CACHE_ID: &str = "canvas_aside_pack_perm_cache";

/// Sentinel hash seed for a non-finite crossing-relevant coordinate.
///
/// `f32::to_bits` already gives every finite value a distinct, stable key, but `NaN` has many bit
/// patterns (and `+0.0`/`-0.0` differ in bits while being geometrically equal). Folding non-finite
/// inputs to one sentinel keeps a degenerate geometry hashable and stable instead of leaking raw
/// `NaN` bit noise into the key; finite zeros are normalized in `hash_crossing_coord`.
const ASIDE_CROSSING_NONFINITE_SENTINEL: u32 = 0xFFC0_0000;

/// Upper bound on full swap-improvement passes per cluster on a cache miss.
///
/// Each pass is O(n) adjacent-swap probes, and every probe recomputes `count_crossings` in O(n^2),
/// so one pass is O(n^3) and the historic unbounded "while improving" loop was the audited per-frame
/// hot spot at low zoom (one ~48-card cluster). A bubble-sort-style crossing reducer needs at most
/// O(n) passes to converge, so 64 passes comfortably covers the worst supported cluster (<=48) while
/// turning the previously unbounded loop into a hard ceiling even on a cold cache.
const ASIDE_PACK_MAX_PASSES: usize = 64;

/// Maximum number of cached cluster permutations.
///
/// The key is a fingerprint of the cluster's quantized geometry, which changes as the camera moves,
/// so entries accumulate across a pan/zoom session. Clear-on-overflow (as in the text-measure cache)
/// keeps the store O(1): a permutation is cheap to recompute on the rare overflow frame.
const ASIDE_PACK_CACHE_MAX_ENTRIES: usize = 4096;

/// Cheap-to-clone shared handle to the bounded crossing-minimization permutation cache stored in
/// egui temp data. The value is the final cluster ordering as indices into the post-sort cluster
/// order. The `Mutex` keeps it `Send + Sync` as required by `egui::Memory::insert_temp`.
type AsidePackPermCache = Arc<Mutex<HashMap<u64, Vec<usize>>>>;

/// Returns the shared permutation-cache handle from egui temp data, creating an empty one on first
/// use. Only the `Arc` is cloned (O(1)); the underlying map is never cloned.
fn aside_pack_perm_cache_handle(ui: &egui::Ui) -> AsidePackPermCache {
    let cache_id = egui::Id::new(ASIDE_PACK_PERM_CACHE_ID);
    ui.ctx().data_mut(|data| {
        if let Some(handle) = data.get_temp::<AsidePackPermCache>(cache_id) {
            return handle;
        }
        let handle: AsidePackPermCache = Arc::new(Mutex::new(HashMap::new()));
        data.insert_temp(cache_id, handle.clone());
        handle
    })
}

/// Hashes a crossing-relevant coordinate into `hasher` by its exact IEEE-754 bit pattern.
///
/// CORRECTNESS: the crossing decision in `lines_cross` is a pure function of these coordinates, and
/// it amplifies them through a slope ratio `(x - sx) / (oriented_target_x - sx)` whose denominator is
/// only guarded down to `0.0001`. Near that guard the ratio can magnify a sub-point coordinate error
/// by ~`column_span / 0.0001` (tens of thousands), so NO fixed quantization bucket can provably keep
/// the compared differences from crossing the `lines_cross` `±0.001` boundary. Exact-bit hashing is
/// therefore the only provably-correct fingerprint: identical bits guarantee identical crossing
/// decisions (cache hit is always safe), and any differing bit busts the cache so the minimizer
/// recomputes. A genuinely static cluster recomputes these values bit-for-bit from the same
/// camera/anchor/measurement inputs, so the idle case the audit targets still hits.
///
/// `-0.0` is normalized to `+0.0` (geometrically equal) and every non-finite value folds to one
/// sentinel so a degenerate geometry stays stable and hashable instead of leaking `NaN` bit noise.
fn hash_crossing_coord<H: Hasher>(value: f32, hasher: &mut H) {
    let bits = if value.is_finite() {
        // Collapse the two zero bit patterns; +0.0/-0.0 never flip a crossing.
        (value + 0.0).to_bits()
    } else {
        ASIDE_CROSSING_NONFINITE_SENTINEL
    };
    bits.hash(hasher);
}

/// Computes the fingerprint that fully determines a cluster's crossing-minimization result.
///
/// Inputs that affect the optimal ordering: the column side, the oriented column edge X
/// (`oriented_target_x`), the inter-card `gap`, the cluster top, and, in post-sort order, each item's
/// `(bid, area_idx, is_spacer)` identity plus its `source_scene_x` / `source_scene_y` / `h` (the only
/// per-item values that feed `count_crossings`/`lines_cross`). `desired_cy` is captured indirectly
/// through `source_scene_y` and the cluster top. `items` must already be sorted into the canonical
/// pre-pack order so the same logical cluster yields the same fingerprint frame to frame.
///
/// CORRECTNESS: every geometric coordinate is hashed by its exact IEEE-754 bits via
/// `hash_crossing_coord`, not by a coarse quantization bucket. The previous half-point bucket was
/// ~500x larger than the `lines_cross` `±0.001` comparison epsilon, so two configurations sharing a
/// fingerprint could still produce different crossing decisions and a cache hit could replay a stale
/// (visibly crossing) layout. Because the crossing decision is a pure function of exactly these
/// coordinates, identical bits imply an identical decision, so a cache hit can never replay a wrong
/// permutation; any bit change recomputes. See `hash_crossing_coord` for why no fixed bucket is
/// provably safe given the unbounded slope amplification near the `0.0001` denominator guard.
fn cluster_crossing_fingerprint(
    items: &[AsideDesiredSlot],
    side: Side,
    oriented_target_x: f32,
    gap: f32,
    top: f32,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match side {
        Side::Left => 0u8.hash(&mut hasher),
        Side::Right => 1u8.hash(&mut hasher),
    }
    hash_crossing_coord(oriented_target_x, &mut hasher);
    hash_crossing_coord(gap, &mut hasher);
    hash_crossing_coord(top, &mut hasher);
    items.len().hash(&mut hasher);
    for item in items {
        item.item.bid.hash(&mut hasher);
        item.item.area_idx.hash(&mut hasher);
        item.is_spacer.hash(&mut hasher);
        hash_crossing_coord(item.source_scene_x, &mut hasher);
        hash_crossing_coord(item.source_scene_y, &mut hasher);
        hash_crossing_coord(item.h, &mut hasher);
    }
    hasher.finish()
}

/// Reorders `items` in place to match the cached permutation `perm`, returning `false` (leaving
/// `items` untouched) when `perm` is not a valid permutation of `0..items.len()`.
///
/// A validity check is mandatory: a 64-bit fingerprint collision could in principle map to a
/// permutation built for a different cluster length or index set, and applying it blindly would
/// reorder cards incorrectly. On any mismatch the caller falls back to recomputing, so a collision
/// degrades to a recompute, never to a broken layout.
fn apply_cached_permutation(items: &mut Vec<AsideDesiredSlot>, perm: &[usize]) -> bool {
    if perm.len() != items.len() {
        return false;
    }
    let mut seen = vec![false; items.len()];
    for &idx in perm {
        match seen.get_mut(idx) {
            Some(slot) if !*slot => *slot = true,
            _ => return false,
        }
    }
    // SAFETY (index): every `idx` in `perm` passed the `seen.get_mut(idx)` validation above, which
    // returns `None` (and short-circuits with `false`) for any `idx >= items.len()`. So at this point
    // `perm` is a verified permutation of `0..items.len()` and `items[idx]` is always in range.
    let reordered: Vec<AsideDesiredSlot> = perm.iter().map(|&idx| items[idx].clone()).collect();
    *items = reordered;
    true
}

/// Inserts `key`/`perm` into the bounded permutation cache, clearing on overflow (as in the
/// text-measure cache) so the map stays O(1) without unbounded growth across a camera session.
fn aside_pack_cache_store(handle: &AsidePackPermCache, key: u64, perm: Vec<usize>) {
    if let Ok(mut guard) = handle.lock() {
        if guard.len() >= ASIDE_PACK_CACHE_MAX_ENTRIES && !guard.contains_key(&key) {
            guard.clear();
        }
        guard.insert(key, perm);
    }
}

#[derive(Clone, Copy)]
enum AsideBubbleBodyMode {
    Full,
    CompactDual,
    CompactSingle(BubbleTextField),
}

#[derive(Clone, Copy)]
struct AsideBubbleVisibleGroups {
    show_header: bool,
    show_original: bool,
    show_translation: bool,
    show_actions: bool,
    show_footer: bool,
    show_readonly_text: bool,
}

/// Vertical span `[top, bottom]` an aside column may use to place its bubble clusters.
///
/// `page_top`/`page_bottom` are the on-screen page rect bounds (they shrink with zoom).
/// `viewport_top`/`viewport_bottom` are the visible canvas viewport bounds (zoom-independent
/// available height). The returned span is the union of the page span with the viewport span so
/// the column can spread clustered cards into the room above/below the page instead of cramming
/// them into the zoom-shrunk page height. The result is always at least as tall as the page span;
/// degenerate viewport bounds fall back to the page span.
///
/// This only changes how far a cluster may relax outward; it does not change which cards cluster
/// together (that decision is purely card-vs-card overlap and stays anchored to the page).
fn aside_column_vertical_bounds(
    page_top: f32,
    page_bottom: f32,
    viewport_top: f32,
    viewport_bottom: f32,
) -> [f32; 2] {
    let (page_lo, page_hi) = if page_top <= page_bottom {
        (page_top, page_bottom)
    } else {
        (page_bottom, page_top)
    };
    // A non-finite or inverted viewport contributes nothing; keep the page span in that case.
    if !viewport_top.is_finite() || !viewport_bottom.is_finite() || viewport_bottom <= viewport_top
    {
        return [page_lo, page_hi];
    }
    [page_lo.min(viewport_top), page_hi.max(viewport_bottom)]
}

/// Whether a slot whose top edge is `slot_top` overlaps the running cluster bottom `cluster_bottom`.
///
/// Both are screen-space Y values; `gap` is the required clear spacing between cards (currently 0,
/// so the test fires only when cards truly touch/collide). Two slots merge into one cluster exactly
/// when this returns `true`, so this is the sole anti-overlap clustering criterion.
#[inline]
fn aside_slots_overlap(slot_top: f32, cluster_bottom: f32, gap: f32) -> bool {
    slot_top <= cluster_bottom + gap
}

fn displayed_aside_side(canvas: &CanvasView, bubble_side: Side) -> Side {
    match canvas.state.aside_side_mode {
        AsideBubbleSideMode::Auto => bubble_side,
        AsideBubbleSideMode::Left => Side::Left,
        AsideBubbleSideMode::Right => Side::Right,
    }
}

/// Lays out and draws both aside columns for one page, then paints the collected anchor links.
///
/// Takes the shared per-page geometry as a `CanvasScenePageFrame` (already built by the scene pass)
/// and threads a single `AsidePageContext` through the layout chain; `canvas` stays a separate
/// `&mut` so the snapshot-then-mutate pattern inside keeps its split borrows.
pub(super) fn draw_aside_for_page(
    canvas: &mut CanvasView,
    ui: &mut egui::Ui,
    project: &ProjectData,
    page_frame: CanvasScenePageFrame,
    left_items: Vec<AsideItem>,
    right_items: Vec<AsideItem>,
    hooks: &mut dyn CanvasHooks,
) {
    let page_idx = page_frame.page_idx;
    let row_rect = page_frame.row_rect;
    let image_rect = page_frame.image_rect;

    let [left_w, right_w] = canvas
        .scene
        .page_aside_widths
        .get(&page_idx)
        .copied()
        .unwrap_or_else(|| {
            canvas.aside_available_widths_for_page_viewport(
                image_rect,
                canvas.scene.visible_scene_rect.unwrap_or(row_rect),
            )
        });

    let (left_items, right_items) = match canvas.state.aside_side_mode {
        AsideBubbleSideMode::Auto => (left_items, right_items),
        AsideBubbleSideMode::Left => {
            let mut merged = left_items;
            merged.extend(right_items);
            (merged, Vec::new())
        }
        AsideBubbleSideMode::Right => {
            let mut merged = left_items;
            merged.extend(right_items);
            (Vec::new(), merged)
        }
    };

    // The column may spread clusters into the available viewport height, not just the zoom-shrunk
    // page height; fall back to the page row rect when the viewport span is unknown.
    let viewport_rect = canvas.scene.visible_scene_rect.unwrap_or(row_rect);

    // Each side decides single- vs two-column layout independently from its own free horizontal
    // span. `left_w`/`right_w` size the single-column fallback (clamped aside width for the side).
    let mut links = Vec::new();
    let mut pctx = AsidePageContext {
        // Reborrow `ui`: it is reused below to paint the collected anchor links after layout.
        ui: &mut *ui,
        project,
        // `hooks` is not used again in this function, so move it into the context.
        hooks,
        row_rect,
        image_rect,
        viewport_rect,
        links: &mut links,
    };
    draw_aside_side(canvas, &mut pctx, Side::Left, left_items, left_w);
    draw_aside_side(canvas, &mut pctx, Side::Right, right_items, right_w);

    for link in links {
        draw_anchor_link(
            ui.painter(),
            image_rect,
            link.img_u,
            link.img_v,
            link.target_x,
            link.target_y,
            link.color,
        );
    }
}

/// Default link color for an ordinary text bubble on `side` (image areas carry their own palette).
fn aside_side_link_color(side: Side) -> Color32 {
    match side {
        Side::Left => Color32::from_rgb(80, 190, 255),
        Side::Right => Color32::from_rgb(255, 160, 80),
    }
}

/// Returns the normalized anchor `(u, v)` that drives an aside item's vertical position, link
/// origin, and side. For a whole-bubble item it is the bubble anchor; for an image area item it is
/// that area's own anchor.
fn aside_item_anchor_uv(
    bubble: &super::types::RuntimeBubble,
    area_idx: Option<usize>,
) -> (f32, f32) {
    match area_idx {
        Some(idx) => bubble
            .text_areas
            .get(idx)
            .map(|area| (area.anchor.x, area.anchor.y))
            .unwrap_or((bubble.img_u, bubble.img_v)),
        None => (bubble.img_u, bubble.img_v),
    }
}

/// Link/card color for an aside item: an image area carries its palette color; an ordinary bubble
/// uses its side color.
fn aside_item_color(area_idx: Option<usize>, side: Side) -> Color32 {
    match area_idx {
        Some(idx) => image_area_palette(idx),
        None => aside_side_link_color(side),
    }
}

fn aside_body_mode(canvas: &CanvasView, bid: i64, has_translation: bool) -> AsideBubbleBodyMode {
    if canvas
        .bubble_runtime
        .runtime_bubbles
        .get(&bid)
        .is_some_and(|bubble| bubble.bubble_class == BubbleClass::Image)
    {
        return AsideBubbleBodyMode::Full;
    }
    if !canvas.editable || canvas.bubble_runtime.selected_bubble == Some(bid) {
        return AsideBubbleBodyMode::Full;
    }
    match canvas.state.aside_compact_mode {
        AsideBubbleCompactMode::None => AsideBubbleBodyMode::Full,
        AsideBubbleCompactMode::Moderate => AsideBubbleBodyMode::CompactDual,
        AsideBubbleCompactMode::Strong => AsideBubbleBodyMode::CompactSingle(if has_translation {
            BubbleTextField::Translation
        } else {
            BubbleTextField::Original
        }),
    }
}

/// Read-only primary text for an image bubble taken from runtime state: area 0's text if present,
/// otherwise the legacy fields. Used when a read-only image bubble has no split areas.
fn image_bubble_readonly_text_runtime(bubble: &super::types::RuntimeBubble) -> String {
    if let Some(first) = bubble.text_areas.first() {
        let text = first.readonly_text();
        if !text.is_empty() {
            return text.to_string();
        }
    }
    bubble.display_text().to_string()
}

fn aside_visible_groups(
    editable: bool,
    body_mode: AsideBubbleBodyMode,
    has_header: bool,
) -> AsideBubbleVisibleGroups {
    if !editable {
        return AsideBubbleVisibleGroups {
            show_header: has_header,
            show_original: false,
            show_translation: false,
            show_actions: false,
            show_footer: false,
            show_readonly_text: true,
        };
    }

    match body_mode {
        AsideBubbleBodyMode::Full => AsideBubbleVisibleGroups {
            show_header: true,
            show_original: true,
            show_translation: true,
            show_actions: true,
            show_footer: true,
            show_readonly_text: false,
        },
        AsideBubbleBodyMode::CompactDual => AsideBubbleVisibleGroups {
            show_header: false,
            show_original: true,
            show_translation: true,
            show_actions: false,
            show_footer: false,
            show_readonly_text: false,
        },
        AsideBubbleBodyMode::CompactSingle(BubbleTextField::Original) => AsideBubbleVisibleGroups {
            show_header: false,
            show_original: true,
            show_translation: false,
            show_actions: false,
            show_footer: false,
            show_readonly_text: false,
        },
        AsideBubbleBodyMode::CompactSingle(BubbleTextField::Translation) => {
            AsideBubbleVisibleGroups {
                show_header: false,
                show_original: false,
                show_translation: true,
                show_actions: false,
                show_footer: false,
                show_readonly_text: false,
            }
        }
    }
}

/// Estimates one aside card's total body height for pre-pack layout. Pure geometry over the
/// measured text metrics in `m`; see `AsideBodyMeasure` for the inputs.
fn estimate_aside_body_height(m: &AsideBodyMeasure<'_>) -> f32 {
    let ui = m.ui;
    let original_text = m.original_text;
    let translation_text = m.translation_text;
    let display_text = m.display_text;
    let base_width_px = m.base_width_px;
    let body_mode = m.body_mode;
    let editable = m.editable;
    let scale_factor = m.scale_factor;
    let frame_inner_margin_px = m.frame_inner_margin_px;
    let has_header = m.has_header;
    let margin_unscaled = frame_inner_margin_px / scale_factor.max(f32::EPSILON);
    let content_width_px = (base_width_px - margin_unscaled * 2.0).max(40.0);
    let original_height =
        measure_text_widget_content_height(ui, original_text, content_width_px) * scale_factor;
    let translation_height =
        measure_text_widget_content_height(ui, translation_text, content_width_px) * scale_factor;
    let display_height =
        measure_text_widget_content_height(ui, display_text, content_width_px) * scale_factor;
    let vertical_padding = frame_inner_margin_px * 2.0 + 2.0;
    let item_spacing = ui.style().spacing.item_spacing.y * scale_factor;
    let chrome_row_height = ui.style().spacing.interact_size.y * scale_factor;
    if !editable {
        let header_height = if has_header {
            chrome_row_height + 4.0
        } else {
            0.0
        };
        return display_height + vertical_padding + header_height;
    }
    match body_mode {
        AsideBubbleBodyMode::Full => {
            let action_spacing = (6.0 + 4.0) * scale_factor;
            let inter_row_spacing = item_spacing * 2.0;
            let safety_padding = 6.0 * scale_factor;
            let editable_extra_height =
                chrome_row_height * 2.0 + action_spacing + inter_row_spacing + safety_padding;
            original_height
                + translation_height
                + vertical_padding
                + item_spacing
                + editable_extra_height
        }
        AsideBubbleBodyMode::CompactDual => {
            original_height + translation_height + vertical_padding + item_spacing
        }
        AsideBubbleBodyMode::CompactSingle(BubbleTextField::Original) => {
            original_height + vertical_padding
        }
        AsideBubbleBodyMode::CompactSingle(BubbleTextField::Translation) => {
            translation_height + vertical_padding
        }
    }
}

/// One slot to place in an aside column before vertical packing.
///
/// `is_spacer` marks an invisible placeholder used in two-column mode: it reserves vertical space in
/// the near column at a far bubble's anchor height so near cards spread apart and the far bubble's
/// link can pass between them. Spacers participate in packing but are never drawn.
#[derive(Clone)]
struct AsideDesiredSlot {
    item: AsideItem,
    width: f32,
    desired_cy: f32,
    h: f32,
    source_scene_x: f32,
    source_scene_y: f32,
    angle_key: f32,
    is_spacer: bool,
}

/// Per-page shared context threaded through the whole aside layout/draw call chain.
///
/// Bundles the egui `Ui`, project, hook object, page geometry, and the shared anchor-link sink so
/// the aside functions take one `&mut AsidePageContext` plus a separate `&mut CanvasView`, instead
/// of a 9-12 argument parameter list per level. `CanvasView` is deliberately NOT a field: the draw
/// pass clones runtime snapshots before mutating canvas state, and embedding the canvas here would
/// collide with those split borrows. Borrowed, never consumed.
struct AsidePageContext<'a> {
    /// Scene-space egui `Ui` the aside cards are drawn into.
    ui: &'a mut egui::Ui,
    /// Project used for hook-bubble resolution and spellcheck lookups; never mutated here.
    project: &'a ProjectData,
    /// Tab hook object for per-bubble header/footer/status customization.
    hooks: &'a mut dyn CanvasHooks,
    /// Full page row rect (unscaled page-height band) in scene space.
    row_rect: Rect,
    /// Page image rect in scene space; anchor origins and side edges derive from it.
    image_rect: Rect,
    /// Viewport rect used to spread columns into available height and gate two-column mode.
    viewport_rect: Rect,
    /// Shared sink for anchor links, drained and painted once after both sides are laid out.
    links: &'a mut Vec<BubbleLink>,
}

/// Column-layout parameters for one aside column (single, near, or far).
///
/// Bundles the sizing/style values computed once per column so the measure pass
/// (`build_aside_desired_slots`) and the draw pass (`draw_aside_slots`) read the same numbers.
struct AsideColumnLayout {
    /// Page side this column belongs to.
    side: Side,
    /// Column rect in scene space.
    column_rect: Rect,
    /// Unscaled column width (px), used for read-only compact-width measuring.
    base_width: f32,
    /// Scaled column width (px), used for editable card width and clamping.
    scaled_width: f32,
    /// Card scale factor (>= 1.0) applied to fonts/spacing/heights.
    scale_factor: f32,
    /// Inner frame margin (px) for cards at this scale.
    frame_inner_margin: i8,
    /// Per-scale egui style for cards, or `None` at scale 1.0 (keep ambient style).
    scaled_style: Option<egui::Style>,
}

/// Inputs for estimating one aside card's body height before packing.
///
/// Groups the text buffers and per-card scalars `estimate_aside_body_height` needs so that measure
/// helper takes a single reference instead of ten positional arguments.
struct AsideBodyMeasure<'a> {
    /// Ui used only for font/spacing metrics (read-only).
    ui: &'a egui::Ui,
    /// Original-text buffer measured for the original row.
    original_text: &'a str,
    /// Translation-text buffer measured for the translation row.
    translation_text: &'a str,
    /// Read-only display text measured in read-only / compact-single modes.
    display_text: &'a str,
    /// Card content width (px) the text is wrapped to.
    base_width_px: f32,
    /// Which body groups are visible, driving which rows contribute height.
    body_mode: AsideBubbleBodyMode,
    /// Whether the canvas is editable (adds editor chrome height).
    editable: bool,
    /// Card scale factor applied to measured heights and chrome.
    scale_factor: f32,
    /// Inner frame margin (px) at this scale.
    frame_inner_margin_px: f32,
    /// Whether a header row is present (read-only path only).
    has_header: bool,
}

/// A slot after vertical packing: final center Y and height within one column.
struct PackedAsideSlot {
    item: AsideItem,
    width: f32,
    cy: f32,
    h: f32,
    is_spacer: bool,
}

/// Builds the per-scale egui style for aside cards (fonts/spacing scaled by `scale_factor`).
/// Returns `None` at scale 1.0 so callers keep the ambient style.
fn aside_scaled_style(ui: &egui::Ui, scale_factor: f32) -> Option<egui::Style> {
    if (scale_factor - 1.0).abs() <= f32::EPSILON {
        return None;
    }
    let mut style = ui.style().as_ref().clone();
    for font in style.text_styles.values_mut() {
        font.size = (font.size * scale_factor).max(1.0);
    }
    style.spacing.item_spacing *= scale_factor;
    style.spacing.button_padding *= scale_factor;
    style.spacing.interact_size *= scale_factor;
    Some(style)
}

/// Inner frame margin (px) for aside cards at the given scale, clamped to a sane range.
fn aside_frame_inner_margin(scale_factor: f32) -> i8 {
    // SAFETY (cast): the `.clamp(2.0, 48.0)` bounds the value to [2, 48] before the cast, which is
    // fully inside `i8`'s [-128, 127] range, so the `as i8` truncation is lossless.
    (8.0 * scale_factor).round().clamp(2.0, 48.0) as i8
}

/// Whether a side has enough horizontal room to split aside bubbles into two columns while keeping
/// the far column fully inside the viewport.
///
/// `side_span` is the distance from that ribbon edge to the matching viewport edge. Two columns
/// (each at least `min_width`) plus the three small gaps (ribbon->near, near->far, far->edge) must
/// fit within the span; otherwise the far column would spill past the viewport edge before its
/// bubbles became visible, so the mode stays off for that side. This is why the deactivation point
/// is the full two-column width, not a narrow activation threshold.
fn aside_two_column_active(side_span: f32, min_width: f32, spacing: f32) -> bool {
    side_span >= 2.0 * min_width.max(1.0) + 3.0 * spacing.max(0.0)
}

/// Equal width of each of the two aside columns for a side with `side_span` room.
///
/// Accounts for three small gaps (ribbon->near, near->far, far->edge). The width is never below
/// `min_width`; it is capped at `max_width` only when bubble scaling is on (otherwise both columns
/// stay exactly at `min_width`).
fn aside_two_column_card_width(
    side_span: f32,
    spacing: f32,
    min_width: f32,
    max_width: f32,
    scale_bubbles: bool,
) -> f32 {
    let min_width = min_width.max(1.0);
    if !scale_bubbles {
        return min_width;
    }
    let usable = (side_span - 3.0 * spacing.max(0.0)) * 0.5;
    usable.clamp(min_width, max_width.max(min_width))
}

/// Near/far column rects for the two-column aside layout.
///
/// The near column hugs the ribbon (one `spacing` gap from the page edge); the far column is offset
/// outward by one column width plus another `spacing`. Both columns are `col_w` wide and as tall as
/// the page rect.
fn aside_two_column_rects(
    side: Side,
    image_rect: Rect,
    row_rect: Rect,
    col_w: f32,
    spacing: f32,
) -> (Rect, Rect) {
    let col_w = col_w.max(1.0);
    let h = image_rect.height();
    let top = row_rect.top();
    match side {
        Side::Left => {
            let near_left = image_rect.left() - spacing - col_w;
            let far_left = near_left - spacing - col_w;
            (
                Rect::from_min_size(egui::pos2(near_left, top), egui::vec2(col_w, h)),
                Rect::from_min_size(egui::pos2(far_left, top), egui::vec2(col_w, h)),
            )
        }
        Side::Right => {
            let near_left = image_rect.right() + spacing;
            let far_left = near_left + col_w + spacing;
            (
                Rect::from_min_size(egui::pos2(near_left, top), egui::vec2(col_w, h)),
                Rect::from_min_size(egui::pos2(far_left, top), egui::vec2(col_w, h)),
            )
        }
    }
}

/// Lays out one side's aside bubbles, choosing single- or two-column layout.
///
/// Two columns are used only when the setting is on and the side has enough free horizontal room for
/// both columns plus gaps to stay inside the viewport (evaluated independently per side, so
/// left/right can differ under horizontal ribbon scroll). Otherwise the classic single column is
/// drawn, sized to `single_col_width`.
fn draw_aside_side(
    canvas: &mut CanvasView,
    pctx: &mut AsidePageContext<'_>,
    side: Side,
    items: Vec<AsideItem>,
    single_col_width: f32,
) {
    if items.is_empty() {
        return;
    }
    let image_rect = pctx.image_rect;
    let row_rect = pctx.row_rect;
    let viewport_rect = pctx.viewport_rect;
    let side_margin = canvas.state.side_margin;
    let min_width = canvas.state.bubble_min_width.max(1.0);
    let span = match side {
        Side::Left => image_rect.left() - viewport_rect.left(),
        Side::Right => viewport_rect.right() - image_rect.right(),
    }
    .max(0.0);
    let spacing = side_margin.max(0.0);
    if canvas.state.aside_second_column && aside_two_column_active(span, min_width, spacing) {
        let col_w = aside_two_column_card_width(
            span,
            spacing,
            min_width,
            canvas.state.bubble_max_width,
            canvas.state.scale_bubbles,
        );
        let (near_rect, far_rect) =
            aside_two_column_rects(side, image_rect, row_rect, col_w, spacing);
        draw_aside_two_columns(canvas, pctx, side, items, near_rect, far_rect);
    } else {
        let col_rect = match side {
            Side::Left => Rect::from_min_size(
                egui::pos2(
                    image_rect.left() - side_margin - single_col_width,
                    row_rect.top(),
                ),
                egui::vec2(single_col_width.max(1.0), image_rect.height()),
            ),
            Side::Right => Rect::from_min_size(
                egui::pos2(image_rect.right() + side_margin, row_rect.top()),
                egui::vec2(single_col_width.max(1.0), image_rect.height()),
            ),
        };
        draw_aside_column(canvas, pctx, side, items, col_rect);
    }
}

fn draw_aside_column(
    canvas: &mut CanvasView,
    pctx: &mut AsidePageContext<'_>,
    side: Side,
    items: Vec<AsideItem>,
    column_rect: Rect,
) {
    if items.is_empty() || !column_rect.is_positive() {
        return;
    }
    let scale_factor = canvas.aside_scale_factor();
    let base_column_width = column_rect.width().max(1.0);
    let scaled_column_width = (base_column_width * scale_factor).max(1.0);
    let frame_inner_margin = aside_frame_inner_margin(scale_factor);
    let scaled_bubble_style = aside_scaled_style(pctx.ui, scale_factor);
    let layout = AsideColumnLayout {
        side,
        column_rect,
        base_width: base_column_width,
        scaled_width: scaled_column_width,
        scale_factor,
        frame_inner_margin,
        scaled_style: scaled_bubble_style,
    };
    let desired = build_aside_desired_slots(canvas, pctx, &layout, &items);
    let perm_cache = aside_pack_perm_cache_handle(pctx.ui);
    let packed = pack_aside_slots(
        desired,
        side,
        column_rect,
        pctx.viewport_rect,
        0.0,
        Some(&perm_cache),
    );
    draw_aside_slots(canvas, pctx, &layout, packed);
}

/// Lays out one side's aside bubbles in two side-by-side columns (near = hugging the ribbon,
/// far = offset outward).
///
/// Near cards are preferred: a bubble is offloaded to the far column only where the near column
/// gets dense. All items are first packed conceptually into one near column and clustered by
/// overlap; an isolated (non-overlapping) bubble stays near, while each overlapping cluster is split
/// alternately between near and far to relieve the crowding. The far column is then packed
/// independently so its links stay close to their anchor heights (roughly horizontal). The near
/// column is packed together with one invisible spacer per far bubble (at the far anchor height) so
/// near cards spread apart and each far link threads the gap between near cards instead of crossing
/// over/under one.
fn draw_aside_two_columns(
    canvas: &mut CanvasView,
    pctx: &mut AsidePageContext<'_>,
    side: Side,
    items: Vec<AsideItem>,
    near_rect: Rect,
    far_rect: Rect,
) {
    if items.is_empty() || !near_rect.is_positive() {
        return;
    }
    let viewport_rect = pctx.viewport_rect;
    let scale_factor = canvas.aside_scale_factor();
    let frame_inner_margin = aside_frame_inner_margin(scale_factor);
    let scaled_bubble_style = aside_scaled_style(pctx.ui, scale_factor);
    // Small mandatory gap so near cards keep a little distance and far links have room between them.
    let gap = (10.0 * scale_factor).max(2.0);
    // Vertical clearance reserved in the near column for one far link to pass between cards.
    let clearance = (18.0 * scale_factor).max(6.0);

    // Both columns share the same width; build every desired slot once at that width.
    let base_w = near_rect.width().max(1.0);
    let scaled_w = (base_w * scale_factor).max(1.0);
    // Near-column layout also drives the shared measure pass (its width matches the far column).
    let near_layout = AsideColumnLayout {
        side,
        column_rect: near_rect,
        base_width: base_w,
        scaled_width: scaled_w,
        scale_factor,
        frame_inner_margin,
        scaled_style: scaled_bubble_style.clone(),
    };
    let mut all = build_aside_desired_slots(canvas, pctx, &near_layout, &items);
    if all.is_empty() {
        return;
    }
    all.sort_by(|a, b| {
        a.desired_cy
            .total_cmp(&b.desired_cy)
            .then_with(|| a.item.bid.cmp(&b.item.bid))
    });

    // Near-priority distribution: cluster by overlap in the near column, keep isolated bubbles near,
    // and split each overlapping cluster alternately between near and far.
    let (near_desired, far_desired) = split_near_priority(all, gap);

    // Near column: real near cards plus invisible spacers at each far anchor height so near cards
    // open a gap where the far link crosses.
    let mut near_input = near_desired;
    for far in &far_desired {
        near_input.push(spacer_from_slot(far, clearance));
    }
    let perm_cache = aside_pack_perm_cache_handle(pctx.ui);
    let near_packed = pack_aside_slots(
        near_input,
        side,
        near_rect,
        viewport_rect,
        gap,
        Some(&perm_cache),
    );
    let near_real: Vec<PackedAsideSlot> =
        near_packed.into_iter().filter(|s| !s.is_spacer).collect();
    draw_aside_slots(canvas, pctx, &near_layout, near_real);

    // Far column: packed independently so links stay close to their anchors (straight).
    if !far_desired.is_empty() && far_rect.is_positive() {
        let far_packed = pack_aside_slots(
            far_desired,
            side,
            far_rect,
            viewport_rect,
            gap,
            Some(&perm_cache),
        );
        let far_layout = AsideColumnLayout {
            side,
            column_rect: far_rect,
            base_width: base_w,
            scaled_width: scaled_w,
            scale_factor,
            frame_inner_margin,
            scaled_style: scaled_bubble_style,
        };
        draw_aside_slots(canvas, pctx, &far_layout, far_packed);
    }
}

/// Splits desired slots (already sorted top->bottom) into near and far columns, preferring near.
///
/// Slots are grouped into clusters of vertically overlapping cards (`gap` clearance). A singleton
/// cluster is sparse and stays entirely near; a cluster of two or more is crowded and is split
/// alternately (first->near, second->far, ...) to halve the per-column density.
fn split_near_priority(
    sorted: Vec<AsideDesiredSlot>,
    gap: f32,
) -> (Vec<AsideDesiredSlot>, Vec<AsideDesiredSlot>) {
    let mut near = Vec::new();
    let mut far = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut j = i + 1;
        let mut cluster_bottom = sorted[i].desired_cy + sorted[i].h * 0.5;
        while j < sorted.len() {
            let top = sorted[j].desired_cy - sorted[j].h * 0.5;
            if aside_slots_overlap(top, cluster_bottom, gap) {
                cluster_bottom = cluster_bottom.max(sorted[j].desired_cy + sorted[j].h * 0.5);
                j += 1;
            } else {
                break;
            }
        }
        let cluster_len = j - i;
        for (offset, slot) in sorted[i..j].iter().enumerate() {
            // Isolated cards stay near; crowded clusters alternate near/far starting near.
            if cluster_len == 1 || offset % 2 == 0 {
                near.push(slot.clone());
            } else {
                far.push(slot.clone());
            }
        }
        i = j;
    }
    (near, far)
}

/// Builds an invisible spacer matching a far slot's anchor height, so the near column opens a gap
/// exactly where that far link will cross it.
fn spacer_from_slot(far: &AsideDesiredSlot, clearance: f32) -> AsideDesiredSlot {
    AsideDesiredSlot {
        item: far.item,
        width: 0.0,
        desired_cy: far.desired_cy,
        h: clearance.max(1.0),
        source_scene_x: far.source_scene_x,
        source_scene_y: far.source_scene_y,
        angle_key: far.angle_key,
        is_spacer: true,
    }
}

/// Builds the unsorted desired slots (height, width, anchor positions) for one column's items.
fn build_aside_desired_slots(
    canvas: &mut CanvasView,
    pctx: &mut AsidePageContext<'_>,
    layout: &AsideColumnLayout,
    items: &[AsideItem],
) -> Vec<AsideDesiredSlot> {
    // Rebind context/layout fields to locals so the measure logic below reads them as before.
    let project = pctx.project;
    let image_rect = pctx.image_rect;
    let ui = &mut *pctx.ui;
    let hooks = &mut *pctx.hooks;
    let side = layout.side;
    let base_column_width = layout.base_width;
    let scaled_column_width = layout.scaled_width;
    let frame_inner_margin = layout.frame_inner_margin;
    let scale_factor = layout.scale_factor;
    let mut desired_slots: Vec<AsideDesiredSlot> = Vec::new();

    // O(1) id -> persisted bubble lookup for this measure pass, replacing the former per-item
    // linear `project.bubbles.iter().find()`. Built once per call so every item resolves its hook
    // bubble in constant time. Borrows `project` immutably; `project` is never mutated here.
    let project_bubbles_by_id = build_project_bubble_index(project);

    // Resolve image-bubble preview textures before packing so layout reads the same `size_px` that
    // the draw pass will use this frame. Without this, the first frame (or the frame after a crop
    // key change) would pack against a stale/absent size and the card would visibly jump a frame
    // later. `ensure_image_bubble_preview_loaded` is a cheap no-op when the entry already matches
    // and is skipped during rect drag, so this adds no per-frame heavy work.
    if canvas.editable {
        let ctx = ui.ctx().clone();
        for item in items {
            let bid = item.bid;
            let Some(b) = canvas.bubble_runtime.runtime_bubbles.get(&bid) else {
                continue;
            };
            if b.bubble_class != BubbleClass::Image {
                continue;
            }
            let hook_bubble_fallback;
            let hook_bubble = match project_bubbles_by_id.get(&bid) {
                Some(project_bubble) => *project_bubble,
                None => {
                    hook_bubble_fallback = canvas.hook_bubble_for_runtime(project, b);
                    &hook_bubble_fallback
                }
            };
            // Clone so the immutable runtime borrow ends before the mutable ensure-load call.
            let hook_bubble = hook_bubble.clone();
            canvas.ensure_image_bubble_preview_loaded(&ctx, project, &hook_bubble);
        }
    }

    let image_center_y = image_rect.center().y;
    let side_edge_x = match side {
        Side::Left => image_rect.left(),
        Side::Right => image_rect.right(),
    };

    for &item in items {
        let bid = item.bid;
        let Some(b) = canvas.bubble_runtime.runtime_bubbles.get(&bid) else {
            continue;
        };
        let is_image = b.bubble_class == BubbleClass::Image;
        let (anchor_u, anchor_v) = aside_item_anchor_uv(b, item.area_idx);
        // Read-only text shown for this item (used for width and height estimation).
        let item_readonly_text: String = match item.area_idx {
            Some(idx) => b
                .text_areas
                .get(idx)
                .map(|area| area.readonly_text().to_string())
                .unwrap_or_default(),
            None if is_image => image_bubble_readonly_text_runtime(b),
            None => b.display_text().to_string(),
        };
        let body_mode = aside_body_mode(canvas, bid, !b.text.trim().is_empty());
        let hook_bubble_fallback;
        let hook_bubble = match project_bubbles_by_id.get(&bid) {
            Some(project_bubble) => *project_bubble,
            None => {
                hook_bubble_fallback = canvas.hook_bubble_for_runtime(project, b);
                &hook_bubble_fallback
            }
        };
        let has_header = hooks.has_bubble_header(hook_bubble, canvas.editable);
        let bubble_width = if canvas.editable {
            scaled_column_width
        } else {
            let frame_inner_margin_px = f32::from(frame_inner_margin);
            let text_width =
                measure_text_widget_compact_width(ui, &item_readonly_text, base_column_width);
            let header_width = if has_header {
                hooks
                    .readonly_aside_header_width_hint(ui, hook_bubble, canvas.editable)
                    .unwrap_or(0.0)
            } else {
                0.0
            };
            (text_width.max(header_width) + frame_inner_margin_px * 2.0)
                .clamp(1.0, scaled_column_width)
        };
        let mut estimated_h = estimate_aside_body_height(&AsideBodyMeasure {
            ui,
            original_text: &b.original_text,
            translation_text: &b.text,
            display_text: &item_readonly_text,
            base_width_px: bubble_width,
            body_mode,
            editable: canvas.editable,
            scale_factor,
            frame_inner_margin_px: f32::from(frame_inner_margin),
            has_header,
        });
        if canvas.editable && is_image {
            // Editable image bubbles render a preview plus one row block per text area.
            let preview_content_width =
                (bubble_width - f32::from(frame_inner_margin) * 2.0).max(1.0);
            let preview_height = canvas
                .image_bubble_preview_height(bid, preview_content_width)
                .unwrap_or_else(|| {
                    canvas
                        .state
                        .bubble_max_width
                        .max(canvas.state.bubble_min_width)
                });
            let area_count = b.text_areas.len().max(1);
            let row_height = ui.style().spacing.interact_size.y * scale_factor;
            let spacing = ui.style().spacing.item_spacing.y * scale_factor;
            // Three text rows + frame chrome per block, plus the add-area button row.
            let per_block = row_height * 3.0 + spacing * 4.0 + f32::from(frame_inner_margin) * 2.0;
            estimated_h += preview_height + per_block * area_count as f32 + row_height + spacing;
        }
        let measured_h = if b.mounted && b.height_px.is_finite() {
            b.height_px.max(1.0)
        } else {
            0.0
        };
        // Editable image bubbles render a preview and multiple blocks whose combined height the
        // estimate cannot predict exactly. Once the bubble has been laid out at least once, trust
        // the measured height instead of `max(estimate, measured)`: an over-tall slot would push
        // the body to the top of the slot (so the anchor line enters near the bottom edge instead
        // of the center) and force lower bubbles further down than the free space requires.
        let h = if canvas.editable && is_image && measured_h > 0.5 {
            measured_h
        } else {
            estimated_h.max(measured_h).max(1.0)
        };
        let source_scene_x = image_rect.left() + anchor_u.clamp(0.0, 1.0) * image_rect.width();
        let source_scene_y = image_rect.top() + anchor_v.clamp(0.0, 1.0) * image_rect.height();
        let desired_cy = source_scene_y;
        let edge_dx = match side {
            Side::Left => source_scene_x - side_edge_x,
            Side::Right => side_edge_x - source_scene_x,
        }
        .max(1.0);
        let angle_key = (source_scene_y - image_center_y).atan2(edge_dx);
        desired_slots.push(AsideDesiredSlot {
            item,
            width: bubble_width,
            desired_cy,
            h,
            source_scene_x,
            source_scene_y,
            angle_key,
            is_spacer: false,
        });
    }
    desired_slots
}

/// Packs desired slots into one column.
///
/// Clusters overlapping cards, relaxes each cluster into the available vertical span, minimizes
/// link crossings within a cluster, and returns the final center Y / height per slot. `gap` is the
/// required clear spacing between cards. Pure geometry: no UI or shared-model access, so it is unit
/// testable.
///
/// `perm_cache`, when `Some`, memoizes the per-cluster crossing-minimization result across frames
/// keyed by `cluster_crossing_fingerprint`. On a hit the bounded swap-improvement loop is skipped and
/// the cached ordering is reused, which removes the audited per-frame O(n^3) hot spot when the camera
/// is steady; on a miss the loop runs (capped at `ASIDE_PACK_MAX_PASSES`) and its result is stored.
/// Passing `None` always recomputes and is used by unit tests for the pure-geometry contract.
fn pack_aside_slots(
    desired: Vec<AsideDesiredSlot>,
    side: Side,
    column_rect: Rect,
    viewport_rect: Rect,
    gap: f32,
    perm_cache: Option<&AsidePackPermCache>,
) -> Vec<PackedAsideSlot> {
    if desired.is_empty() {
        return Vec::new();
    }
    let mut desired_slots = desired;
    desired_slots.sort_by(|a, b| {
        a.desired_cy
            .total_cmp(&b.desired_cy)
            .then_with(|| a.item.bid.cmp(&b.item.bid))
            .then_with(|| {
                a.item
                    .area_idx
                    .unwrap_or(usize::MAX)
                    .cmp(&b.item.area_idx.unwrap_or(usize::MAX))
            })
    });

    #[derive(Default)]
    struct Cluster {
        items: Vec<AsideDesiredSlot>,
        block_h: f32,
        top: f32,
    }

    let mut clusters: Vec<Cluster> = Vec::new();
    let mut current: Vec<AsideDesiredSlot> = Vec::new();
    let mut current_bottom = 0.0f32;

    let flush_cluster = |clusters: &mut Vec<Cluster>, items: &mut Vec<AsideDesiredSlot>| {
        if items.is_empty() {
            return;
        }
        let count = items.len() as f32;
        let desired_center = items.iter().map(|item| item.desired_cy).sum::<f32>() / count.max(1.0);
        let body_h = items.iter().map(|item| item.h).sum::<f32>();
        let block_h = body_h + gap * (items.len().saturating_sub(1) as f32);
        let top = desired_center - block_h * 0.5;
        clusters.push(Cluster {
            items: std::mem::take(items),
            block_h,
            top,
        });
    };

    for slot in desired_slots {
        let top = slot.desired_cy - slot.h * 0.5;
        let bottom = slot.desired_cy + slot.h * 0.5;

        if current.is_empty() {
            current_bottom = bottom;
            current.push(slot);
            continue;
        }

        if aside_slots_overlap(top, current_bottom, gap) {
            current_bottom = current_bottom.max(bottom);
            current.push(slot);
        } else {
            flush_cluster(&mut clusters, &mut current);
            current_bottom = bottom;
            current.push(slot);
        }
    }
    flush_cluster(&mut clusters, &mut current);
    if clusters.is_empty() {
        return Vec::new();
    }

    // Clamp clusters to the available viewport span, not only the zoom-shrunk page height: at low
    // zoom the page is short on screen while bubble cards keep a fixed screen height, so cards
    // genuinely overlap and must cluster, but the cluster should relax into the room above/below
    // the page instead of being crammed and pushed far from its anchors. The clustering decision
    // itself is unchanged, so at normal/high zoom (page span >= viewport span) the bounds equal the
    // page span and behavior is identical.
    let [top_bound, bottom_bound] = aside_column_vertical_bounds(
        column_rect.top(),
        column_rect.bottom(),
        viewport_rect.top(),
        viewport_rect.bottom(),
    );
    for i in 0..clusters.len() {
        let min_top = if i == 0 {
            top_bound
        } else {
            clusters[i - 1].top + clusters[i - 1].block_h + gap
        };
        clusters[i].top = clusters[i].top.max(min_top);
    }
    for i in (0..clusters.len()).rev() {
        let max_top = if i + 1 >= clusters.len() {
            bottom_bound - clusters[i].block_h
        } else {
            clusters[i + 1].top - gap - clusters[i].block_h
        };
        clusters[i].top = clusters[i].top.min(max_top);
    }
    for i in 0..clusters.len() {
        let min_top = if i == 0 {
            top_bound
        } else {
            clusters[i - 1].top + clusters[i - 1].block_h + gap
        };
        clusters[i].top = clusters[i].top.max(min_top);
    }

    let oriented_target_x = match side {
        Side::Left => -column_rect.right(),
        Side::Right => column_rect.left(),
    };
    let oriented_source_x = |x: f32| -> f32 {
        match side {
            Side::Left => -x,
            Side::Right => x,
        }
    };
    let y_at_x = |sx: f32, sy: f32, ty: f32, x: f32| -> f32 {
        let denom = oriented_target_x - sx;
        if denom.abs() <= 0.0001 {
            sy
        } else {
            sy + (ty - sy) * ((x - sx) / denom)
        }
    };
    let lines_cross = |a: (f32, f32, f32), b: (f32, f32, f32)| -> bool {
        let overlap_start_x = a.0.max(b.0);
        if overlap_start_x >= oriented_target_x - 0.0001 {
            return false;
        }
        let a_overlap_y = y_at_x(a.0, a.1, a.2, overlap_start_x);
        let b_overlap_y = y_at_x(b.0, b.1, b.2, overlap_start_x);
        let diff_at_overlap = a_overlap_y - b_overlap_y;
        let diff_at_target = a.2 - b.2;
        (diff_at_overlap > 0.001 && diff_at_target < -0.001)
            || (diff_at_overlap < -0.001 && diff_at_target > 0.001)
    };
    let count_crossings = |items: &[AsideDesiredSlot], top: f32| -> usize {
        if items.len() < 2 {
            return 0;
        }
        let mut lines: Vec<(f32, f32, f32)> = Vec::with_capacity(items.len());
        let mut cursor = top;
        for item in items {
            let target_y = cursor + item.h * 0.5;
            cursor += item.h + gap;
            lines.push((
                oriented_source_x(item.source_scene_x),
                item.source_scene_y,
                target_y,
            ));
        }
        let mut crossings = 0usize;
        for i in 0..lines.len() {
            for j in (i + 1)..lines.len() {
                if lines_cross(lines[i], lines[j]) {
                    crossings = crossings.saturating_add(1);
                }
            }
        }
        crossings
    };

    let mut slots: Vec<PackedAsideSlot> = Vec::new();
    for cluster in &mut clusters {
        if cluster.items.len() > 1 {
            cluster.items.sort_by(|a, b| {
                a.angle_key
                    .total_cmp(&b.angle_key)
                    .then_with(|| a.desired_cy.total_cmp(&b.desired_cy))
                    .then_with(|| a.item.bid.cmp(&b.item.bid))
            });

            if cluster.items.len() <= 48 {
                // Fingerprint the cluster in its canonical post-sort order so a steady camera reuses
                // the same minimized ordering instead of re-running the O(n^3) swap loop every frame.
                let fingerprint = perm_cache.map(|_| {
                    cluster_crossing_fingerprint(
                        &cluster.items,
                        side,
                        oriented_target_x,
                        gap,
                        cluster.top,
                    )
                });

                let mut applied_from_cache = false;
                if let (Some(handle), Some(key)) = (perm_cache, fingerprint) {
                    // Brief lock only for the lookup; clone the small permutation out so the swap loop
                    // (or apply) runs without holding the lock.
                    let cached = handle
                        .lock()
                        .ok()
                        .and_then(|guard| guard.get(&key).cloned());
                    if let Some(perm) = cached {
                        applied_from_cache = apply_cached_permutation(&mut cluster.items, &perm);
                    }
                }

                if !applied_from_cache {
                    // `perm[i]` is the post-sort index currently sitting at final position `i`; it is
                    // swapped in lock-step with `cluster.items` so the converged ordering can be cached
                    // and replayed next frame.
                    let mut perm: Vec<usize> = (0..cluster.items.len()).collect();
                    let mut best_crossings = count_crossings(&cluster.items, cluster.top);
                    if best_crossings > 0 {
                        let mut passes = 0usize;
                        let mut improved = true;
                        // Bounded swap-improvement: cap the passes so even a cold-cache 48-card single
                        // cluster at low zoom is a hard ceiling, not an unbounded "while improving".
                        while improved && best_crossings > 0 && passes < ASIDE_PACK_MAX_PASSES {
                            improved = false;
                            passes += 1;
                            for idx in 0..(cluster.items.len() - 1) {
                                cluster.items.swap(idx, idx + 1);
                                let candidate_crossings =
                                    count_crossings(&cluster.items, cluster.top);
                                if candidate_crossings < best_crossings {
                                    best_crossings = candidate_crossings;
                                    perm.swap(idx, idx + 1);
                                    improved = true;
                                } else {
                                    cluster.items.swap(idx, idx + 1);
                                }
                            }
                        }
                    }
                    if let (Some(handle), Some(key)) = (perm_cache, fingerprint) {
                        aside_pack_cache_store(handle, key, perm);
                    }
                }
            }
        }

        let mut top = cluster.top;
        for slot in &cluster.items {
            let cy = top + slot.h * 0.5;
            slots.push(PackedAsideSlot {
                item: slot.item,
                width: slot.width,
                cy,
                h: slot.h,
                is_spacer: slot.is_spacer,
            });
            top += slot.h + gap;
        }
    }

    slots
}

/// Draws already-packed aside cards in one column and appends their anchor links to the shared sink.
fn draw_aside_slots(
    canvas: &mut CanvasView,
    pctx: &mut AsidePageContext<'_>,
    layout: &AsideColumnLayout,
    slots: Vec<PackedAsideSlot>,
) {
    // Rebind context/layout fields to locals so the draw logic below reads them as before.
    let project = pctx.project;
    let image_rect = pctx.image_rect;
    let ui = &mut *pctx.ui;
    let hooks = &mut *pctx.hooks;
    let out_links = &mut *pctx.links;
    let side = layout.side;
    let column_rect = layout.column_rect;
    let frame_inner_margin = layout.frame_inner_margin;
    let scaled_bubble_style = &layout.scaled_style;
    // O(1) id -> persisted bubble lookup for this draw pass, replacing the former per-slot linear
    // `project.bubbles.iter().find()`. Built once per call; borrows `project` immutably for the loop
    // below, which never mutates `project`. (Same small map is rebuilt independently in
    // `build_aside_desired_slots`, since the two passes are separate functions reached from
    // different call sites in this file.)
    let project_bubbles_by_id = build_project_bubble_index(project);

    for slot in slots {
        let item = slot.item;
        let bubble_width = slot.width;
        let cy = slot.cy;
        let h = slot.h;
        let bid = item.bid;
        let area_idx = item.area_idx;
        let Some(snapshot) = canvas.bubble_runtime.runtime_bubbles.get(&bid).cloned() else {
            continue;
        };
        let hook_bubble_fallback;
        let hook_bubble = match project_bubbles_by_id.get(&bid) {
            Some(project_bubble) => *project_bubble,
            None => {
                hook_bubble_fallback = canvas.hook_bubble_for_runtime(project, &snapshot);
                &hook_bubble_fallback
            }
        };

        let bubble_top = cy - h * 0.5;
        let bubble_left = match side {
            Side::Left => column_rect.right() - bubble_width,
            Side::Right => column_rect.left(),
        };
        let bubble_slot_rect = Rect::from_min_size(
            egui::pos2(bubble_left, bubble_top),
            egui::vec2(bubble_width, h),
        );
        let bubble_available_rect = Rect::from_min_max(
            bubble_slot_rect.min,
            egui::pos2(
                bubble_slot_rect.right(),
                column_rect.bottom().max(bubble_slot_rect.bottom()),
            ),
        );
        let selected = canvas.bubble_runtime.selected_bubble == Some(bid);
        let frame_color = if selected {
            Color32::from_rgb(42, 54, 71)
        } else {
            Color32::from_rgb(35, 35, 42)
        };
        let frame = egui::Frame::new()
            .fill(frame_color.gamma_multiply(canvas.state.bubble_opacity))
            .stroke(Stroke::new(1.0, Color32::from_gray(90)))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::same(frame_inner_margin));
        let status_stroke = if canvas.editable && canvas.state.show_bubble_status {
            hooks.bubble_status_style(hook_bubble, canvas.editable, canvas)
        } else {
            None
        };

        let mut new_original = snapshot.original_text.clone();
        let mut new_text = snapshot.text.clone();
        let is_image_bubble = snapshot.bubble_class == BubbleClass::Image;
        // Read-only text for this specific item: an image area shows its own text, a whole-bubble
        // item shows the bubble primary text.
        let txt_owned = match area_idx {
            Some(idx) => snapshot
                .text_areas
                .get(idx)
                .map(|area| area.readonly_text().to_string())
                .unwrap_or_default(),
            None if is_image_bubble => image_bubble_readonly_text_runtime(&snapshot),
            None => snapshot.display_text().to_owned(),
        };
        // Editable multi-area image bubble drafts: one (original, description, translation) per
        // text area. Empty for text bubbles.
        let mut area_drafts: Vec<(String, String, String)> = snapshot
            .text_areas
            .iter()
            .map(|area| {
                (
                    area.original.clone(),
                    area.description.clone(),
                    area.translation.clone(),
                )
            })
            .collect();
        let mut want_add_area = false;
        let mut remove_area: Option<usize> = None;
        let mut area_text_changed = false;
        let mut block_rects: Vec<Rect> = Vec::new();
        // Unique egui id discriminator: read-only image areas share a bubble id across cards.
        let item_key = (bid, area_idx.unwrap_or(usize::MAX));
        // Single deferred command accumulated across all three context-menu closures below.
        // Only one menu is ever open at a time and each item closes the menu, so at most one
        // command is produced per frame; applied after the closures release their borrows.
        let mut menu_command: Option<BubbleMenuCommand> = None;
        let mut want_translate = false;
        let mut want_delete = false;
        let mut text_changed = false;
        let mut interacted_with_bubble = false;
        let mut bubble_has_focus = selected;
        let rtl_align_frame = !canvas.editable && side == Side::Left;
        let body_mode = aside_body_mode(canvas, bid, !snapshot.text.trim().is_empty());
        let has_header = hooks.has_bubble_header(hook_bubble, canvas.editable);
        let visible_groups = aside_visible_groups(canvas.editable, body_mode, has_header);

        let scene_clip_rect = ui.clip_rect();
        let zoom_drag_active = canvas.scene.zoom_drag_active;

        let mut bubble_body = |ui: &mut egui::Ui| {
            if !canvas.editable && rtl_align_frame {
                ui.vertical(|ui| {
                    if visible_groups.show_header {
                        ui.horizontal(|ui| {
                            hooks.build_bubble_header(ui, hook_bubble, canvas.editable);
                        });
                        ui.add_space(4.0);
                    }
                    if visible_groups.show_readonly_text {
                        with_bubble_text_font(ui, |ui| {
                            ui.add(egui::Label::new(txt_owned.as_str()).wrap());
                        });
                    }
                });
            } else {
                if visible_groups.show_header {
                    ui.horizontal(|ui| {
                        hooks.build_bubble_header(ui, hook_bubble, canvas.editable);
                    });
                    ui.add_space(4.0);
                }

                if canvas.editable && is_image_bubble {
                    let text_width = ui.available_width().max(40.0);
                    // Use the exact same content width the layout pass fed to
                    // `image_bubble_preview_height`, so the drawn preview size equals the reserved
                    // slot height this frame (live `ui.available_width()` can differ slightly).
                    let preview_content_width =
                        (bubble_width - f32::from(frame_inner_margin) * 2.0).max(1.0);
                    canvas.draw_image_bubble_preview(
                        ui,
                        project,
                        hook_bubble,
                        preview_content_width,
                        side,
                    );
                    ui.add_space(4.0);
                    draw_editable_image_areas(
                        canvas,
                        ui,
                        bid,
                        text_width,
                        &mut area_drafts,
                        &mut want_add_area,
                        &mut remove_area,
                        &mut area_text_changed,
                        &mut block_rects,
                        &mut bubble_has_focus,
                        &mut interacted_with_bubble,
                    );
                } else if canvas.editable {
                    let text_width = ui.available_width().max(40.0);
                    if visible_groups.show_original {
                        let original_spellcheck_enabled = !canvas.bubble_spellcheck_disabled(
                            project,
                            bid,
                            BubbleTextField::Original,
                        );
                        let orig_resp = with_bubble_text_font(ui, |ui| {
                            SpellcheckedTextEdit::multiline(&mut new_original)
                                .id_salt(("aside_original", bid))
                                .hint_text(t!("canvas.bubble.original_label"))
                                .desired_width(text_width)
                                .desired_rows(1)
                                .spellcheck_enabled(
                                    canvas.state.spellcheck_original && original_spellcheck_enabled,
                                )
                                .show(ui)
                        });
                        let orig_misspelled_word =
                            misspelled_word_at_pointer(ui, &orig_resp, &new_original);
                        canvas.note_focused_bubble_text_input(
                            ui.ctx(),
                            bid,
                            BubbleTextField::Original,
                            &orig_resp.response,
                        );
                        if orig_resp.response.clicked() || orig_resp.response.changed() {
                            interacted_with_bubble = true;
                        }
                        if orig_resp.response.has_focus() {
                            canvas.bubble_runtime.selected_bubble = Some(bid);
                            interacted_with_bubble = true;
                        }
                        bubble_has_focus = bubble_has_focus || orig_resp.response.has_focus();
                        if orig_resp.response.changed() {
                            text_changed = true;
                            canvas.schedule_text_upsert(bid, ui.ctx().input(|i| i.time));
                        }
                        if orig_resp.response.lost_focus() {
                            canvas.commit_text_upsert_now(bid);
                        }
                        if orig_resp.response.secondary_clicked() {
                            canvas.bubble_runtime.bubble_context_menu_misspelled_word =
                                orig_misspelled_word.clone();
                        }
                        orig_resp.response.context_menu(|ui| {
                            let outcome = canvas.show_bubble_context_menu(
                                ui,
                                BubbleMenuContext {
                                    project,
                                    bubble_id: bid,
                                    bubble_type: snapshot.bubble_type,
                                    original_text: &new_original,
                                    translated_text: &new_text,
                                },
                            );
                            interacted_with_bubble |= outcome.interacted;
                            if outcome.command.is_some() {
                                menu_command = outcome.command;
                            }
                        });
                    }
                    if visible_groups.show_translation {
                        let translation_spellcheck_enabled = !canvas.bubble_spellcheck_disabled(
                            project,
                            bid,
                            BubbleTextField::Translation,
                        );
                        let tr_resp = with_bubble_text_font(ui, |ui| {
                            SpellcheckedTextEdit::multiline(&mut new_text)
                                .id_salt(("aside_translation", bid))
                                .hint_text(t!("canvas.bubble.translation_label"))
                                .desired_width(text_width)
                                .desired_rows(1)
                                .spellcheck_enabled(
                                    canvas.state.spellcheck_translation
                                        && translation_spellcheck_enabled,
                                )
                                .show(ui)
                        });
                        let tr_misspelled_word =
                            misspelled_word_at_pointer(ui, &tr_resp, &new_text);
                        canvas.note_focused_bubble_text_input(
                            ui.ctx(),
                            bid,
                            BubbleTextField::Translation,
                            &tr_resp.response,
                        );
                        if tr_resp.response.clicked() || tr_resp.response.changed() {
                            interacted_with_bubble = true;
                        }
                        if tr_resp.response.has_focus() {
                            canvas.bubble_runtime.selected_bubble = Some(bid);
                            interacted_with_bubble = true;
                        }
                        bubble_has_focus = bubble_has_focus || tr_resp.response.has_focus();
                        if tr_resp.response.changed() {
                            text_changed = true;
                            canvas.schedule_text_upsert(bid, ui.ctx().input(|i| i.time));
                        }
                        if tr_resp.response.lost_focus() {
                            canvas.commit_text_upsert_now(bid);
                        }
                        if tr_resp.response.secondary_clicked() {
                            canvas.bubble_runtime.bubble_context_menu_misspelled_word =
                                tr_misspelled_word.clone();
                        }
                        tr_resp.response.context_menu(|ui| {
                            let outcome = canvas.show_bubble_context_menu(
                                ui,
                                BubbleMenuContext {
                                    project,
                                    bubble_id: bid,
                                    bubble_type: snapshot.bubble_type,
                                    original_text: &new_original,
                                    translated_text: &new_text,
                                },
                            );
                            interacted_with_bubble |= outcome.interacted;
                            if outcome.command.is_some() {
                                menu_command = outcome.command;
                            }
                        });
                    }
                } else if visible_groups.show_readonly_text {
                    with_bubble_text_font(ui, |ui| {
                        ui.add(egui::Label::new(txt_owned.as_str()).wrap());
                    });
                }
            }

            if visible_groups.show_actions {
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button(t!("canvas.bubble.translate_button")).clicked() {
                        want_translate = true;
                    }
                    if ui.small_button(t!("canvas.bubble.delete_button")).clicked() {
                        want_delete = true;
                    }
                });
            }

            if visible_groups.show_footer {
                hooks.build_bubble_footer(ui, project, hook_bubble, canvas.editable);
            }
        };
        let bubble_hit_response = ui.interact(
            bubble_slot_rect,
            Id::new(("aside_bubble_hit", item_key)),
            if zoom_drag_active {
                Sense::hover()
            } else {
                Sense::click_and_drag()
            },
        );
        let mut bubble_ui = CanvasView::new_scene_overlay_child(
            ui,
            bubble_available_rect,
            scene_clip_rect,
            egui::Layout::top_down(Align::LEFT),
        );
        bubble_ui.set_max_width(bubble_slot_rect.width());
        if let Some(style) = scaled_bubble_style.as_ref() {
            bubble_ui.set_style(style.clone());
        }
        let response = if rtl_align_frame {
            bubble_ui
                .with_layout(egui::Layout::right_to_left(Align::TOP), |ui| {
                    frame.show(ui, |ui| bubble_body(ui)).response
                })
                .inner
        } else {
            frame.show(&mut bubble_ui, |ui| bubble_body(ui)).response
        };
        if let Some(style) = status_stroke {
            paint_bubble_status_border(ui.painter(), response.rect, CornerRadius::same(6), style);
        }
        bubble_hit_response.context_menu(|ui| {
            let outcome = canvas.show_bubble_context_menu(
                ui,
                BubbleMenuContext {
                    project,
                    bubble_id: bid,
                    bubble_type: snapshot.bubble_type,
                    original_text: &new_original,
                    translated_text: &new_text,
                },
            );
            interacted_with_bubble |= outcome.interacted;
            if outcome.command.is_some() {
                menu_command = outcome.command;
            }
        });
        if bubble_hit_response.secondary_clicked() {
            canvas.bubble_runtime.bubble_context_menu_misspelled_word = None;
        }

        let pressed_primary_on_bubble = bubble_hit_response.is_pointer_button_down_on()
            && ui.ctx().input(|i| i.pointer.primary_down());
        if pressed_primary_on_bubble {
            canvas.bubble_runtime.selected_bubble = Some(bid);
            interacted_with_bubble = true;
        }
        if response.clicked() || bubble_hit_response.clicked() {
            canvas.bubble_runtime.selected_bubble = Some(bid);
            interacted_with_bubble = true;
        }
        if menu_command.is_some() || want_translate || want_delete || interacted_with_bubble {
            canvas.bubble_runtime.selected_bubble = Some(bid);
        }
        let selected_now = canvas.bubble_runtime.selected_bubble == Some(bid);
        if selected_now {
            bubble_has_focus = true;
        }
        if bubble_has_focus {
            canvas.bubble_runtime.focused_bubbles.insert(bid);
        }

        canvas.apply_bubble_menu_command(
            project,
            ui.ctx(),
            bid,
            menu_command,
            "canvas::bubble_aside_ui",
        );
        if want_translate {
            canvas.bubble_runtime.pending_translate.insert(bid);
        }
        if want_delete {
            canvas.bubble_runtime.pending_delete.insert(bid);
        }
        if is_image_bubble && canvas.editable && area_idx.is_none() {
            apply_image_area_edits(
                canvas,
                bid,
                &area_drafts,
                area_text_changed,
                want_add_area,
                remove_area,
                &block_rects,
            );
        }

        let can_drag_aside = canvas.editable
            && selected_now
            && !canvas.scene.zoom_drag_active
            && canvas.bubble_runtime.move_active_bid.is_none()
            && !ui.ctx().egui_wants_keyboard_input()
            && canvas
                .bubble_runtime
                .active_rect_handle
                .is_none_or(|(active_bid, _)| active_bid != bid)
            && canvas
                .bubble_runtime
                .active_area_handle
                .is_none_or(|(active_bid, _, _)| active_bid != bid);
        let mut drag_stopped = false;
        // Card-body drag: a text bubble moves its anchor; an image bubble moves either the grabbed
        // row block's area, or (grabbed outside any block) the whole red rect with all areas.
        if can_drag_aside
            && bubble_hit_response.drag_started()
            && let Some(pos) = bubble_hit_response.interact_pointer_pos()
        {
            let target = if is_image_bubble {
                // Grabbing a row block moves that area; grabbing the card body outside all blocks
                // moves the red image rect (with every area following it).
                match block_rects.iter().position(|block| block.contains(pos)) {
                    Some(idx) => AsideDragTarget::ImageAreaRect(idx),
                    None => AsideDragTarget::ImageRedRect,
                }
            } else {
                AsideDragTarget::BubbleBody
            };
            start_aside_drag(canvas, bid, target, pos);
        }
        if can_drag_aside
            && bubble_hit_response.dragged()
            && let Some(pos) = bubble_hit_response.interact_pointer_pos()
        {
            drag_aside_by_pointer(canvas, bid, image_rect, pos);
        }
        if canvas
            .bubble_runtime
            .aside_drag_state
            .is_some_and(|state| state.bid == bid)
            && bubble_hit_response.drag_stopped()
        {
            drag_stopped = true;
        }

        let show_rect = canvas.editable
            && selected_now
            && (bubble_has_focus
                || canvas
                    .bubble_runtime
                    .active_rect_handle
                    .is_some_and(|(active_bid, _)| active_bid == bid)
                || canvas
                    .bubble_runtime
                    .active_area_handle
                    .is_some_and(|(active_bid, _, _)| active_bid == bid));
        if show_rect {
            if is_image_bubble {
                if draw_image_bubble_page_overlay(canvas, ui, bid, image_rect, can_drag_aside) {
                    drag_stopped = true;
                }
            } else {
                let coords = canvas
                    .bubble_runtime
                    .runtime_bubbles
                    .get(&bid)
                    .map(|bubble| bubble.rect_coords)
                    .unwrap_or(snapshot.rect_coords);
                let rect = CanvasView::rect_from_coords(image_rect, coords).intersect(image_rect);
                if rect.is_positive() {
                    ui.painter().rect_stroke(
                        rect,
                        0.0,
                        Stroke::new(3.0, Color32::from_rgb(0, 120, 215)),
                        egui::StrokeKind::Inside,
                    );
                    let rect_drag_response = ui.interact(
                        rect,
                        Id::new(("aside_rect_drag", bid)),
                        if canvas.scene.zoom_drag_active {
                            Sense::hover()
                        } else {
                            Sense::click_and_drag()
                        },
                    );
                    let pointer_on_handle =
                        rect_drag_response
                            .interact_pointer_pos()
                            .is_some_and(|pos| {
                                super::bubble_on_top_ui::is_scene_pos_on_rect_handle(rect, pos)
                            });
                    let can_drag_rect = can_drag_aside && !pointer_on_handle;
                    if can_drag_rect
                        && rect_drag_response.drag_started()
                        && let Some(pos) = rect_drag_response.interact_pointer_pos()
                    {
                        start_aside_drag(canvas, bid, AsideDragTarget::RectArea, pos);
                    }
                    if can_drag_rect
                        && rect_drag_response.dragged()
                        && let Some(pos) = rect_drag_response.interact_pointer_pos()
                    {
                        drag_aside_by_pointer(canvas, bid, image_rect, pos);
                    }
                    if canvas
                        .bubble_runtime
                        .aside_drag_state
                        .is_some_and(|state| state.bid == bid)
                        && rect_drag_response.drag_stopped()
                    {
                        drag_stopped = true;
                    }
                }
                super::bubble_on_top_ui::draw_rect_handles(canvas, ui, bid, image_rect, coords);
            }
        }
        if drag_stopped {
            finish_aside_drag(canvas, bid);
        }
        let measured_height = response.rect.height().max(1.0);
        let measured_width = response.rect.width().max(1.0);
        let measured_anchor_y = response.rect.center().y;
        let layout_changed =
            canvas
                .bubble_runtime
                .runtime_bubbles
                .get(&bid)
                .is_some_and(|bubble| {
                    (bubble.height_px - measured_height).abs() > 0.5
                        || (bubble.max_width_px - measured_width).abs() > 0.5
                        || (bubble.anchor_y - measured_anchor_y).abs() > 0.5
                });
        if let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) {
            if text_changed {
                bubble.original_text = new_original;
                bubble.text = new_text;
                bubble.mounted = true;
            }
            bubble.anchor_y = measured_anchor_y;
            bubble.height_px = measured_height;
            bubble.max_width_px = measured_width;
            bubble.line_x = match side {
                Side::Left => response.rect.right(),
                Side::Right => response.rect.left(),
            };
            bubble.mounted = true;
        }
        if layout_changed {
            ui.ctx().request_repaint();
        }

        let bubble_edge_x = match side {
            Side::Left => response.rect.right(),
            Side::Right => response.rect.left(),
        };
        let min_target_y = response.rect.top() + 8.0;
        let max_target_y = (response.rect.bottom() - 8.0).max(min_target_y);
        if is_image_bubble && area_idx.is_none() {
            // Editable image bubble: one colored link per area, aimed at that area's row block.
            let areas = canvas
                .bubble_runtime
                .runtime_bubbles
                .get(&bid)
                .map(|bubble| bubble.text_areas.clone())
                .unwrap_or_default();
            for (idx, area) in areas.iter().enumerate() {
                let color = image_area_palette(idx);
                let block_center_y = block_rects
                    .get(idx)
                    .map_or(response.rect.center().y, |block| block.center().y);
                let target_y = block_center_y.clamp(min_target_y, max_target_y);
                let line_start = egui::pos2(
                    image_rect.left() + area.anchor.x.clamp(0.0, 1.0) * image_rect.width(),
                    image_rect.top() + area.anchor.y.clamp(0.0, 1.0) * image_rect.height(),
                );
                let line_end = egui::pos2(bubble_edge_x, target_y);
                if !hooks.should_hide_aside_bubble_line(
                    snapshot.img_idx,
                    hook_bubble,
                    line_start,
                    line_end,
                ) {
                    out_links.push(BubbleLink {
                        img_u: area.anchor.x,
                        img_v: area.anchor.y,
                        target_x: bubble_edge_x,
                        target_y,
                        color,
                    });
                }
            }
        } else {
            // Text bubble, or a read-only image area card: a single link from the item anchor.
            let (link_img_u, link_img_v) = match area_idx {
                Some(idx) => canvas
                    .bubble_runtime
                    .runtime_bubbles
                    .get(&bid)
                    .and_then(|bubble| bubble.text_areas.get(idx))
                    .map(|area| (area.anchor.x, area.anchor.y))
                    .unwrap_or((snapshot.img_u, snapshot.img_v)),
                None => canvas
                    .bubble_runtime
                    .runtime_bubbles
                    .get(&bid)
                    .map(|bubble| (bubble.img_u, bubble.img_v))
                    .unwrap_or((snapshot.img_u, snapshot.img_v)),
            };
            let line_start = egui::pos2(
                image_rect.left() + link_img_u.clamp(0.0, 1.0) * image_rect.width(),
                image_rect.top() + link_img_v.clamp(0.0, 1.0) * image_rect.height(),
            );
            let target_y = line_start.y.clamp(min_target_y, max_target_y);
            let line_end = egui::pos2(bubble_edge_x, target_y);
            if !hooks.should_hide_aside_bubble_line(
                snapshot.img_idx,
                hook_bubble,
                line_start,
                line_end,
            ) {
                out_links.push(BubbleLink {
                    img_u: link_img_u,
                    img_v: link_img_v,
                    target_x: bubble_edge_x,
                    target_y,
                    color: aside_item_color(area_idx, side),
                });
            }
        }
    }
}

pub(super) fn aside_hit_test(canvas: &CanvasView, page_idx: usize, scene_pos: Pos2) -> bool {
    for bubble in canvas.bubble_runtime.runtime_bubbles.values() {
        if bubble.img_idx != page_idx
            || canvas.displayed_bubble_type_for_runtime(bubble) != BubbleType::Aside
            || !bubble.mounted
        {
            continue;
        }
        let rect = match displayed_aside_side(canvas, bubble.side) {
            Side::Left => Rect::from_center_size(
                egui::pos2(bubble.line_x - bubble.max_width_px * 0.5, bubble.anchor_y),
                egui::vec2(bubble.max_width_px.max(1.0), bubble.height_px.max(1.0)),
            ),
            Side::Right => Rect::from_center_size(
                egui::pos2(bubble.line_x + bubble.max_width_px * 0.5, bubble.anchor_y),
                egui::vec2(bubble.max_width_px.max(1.0), bubble.height_px.max(1.0)),
            ),
        };
        if rect.contains(scene_pos) {
            return true;
        }
    }
    false
}

pub(super) fn start_aside_drag(
    canvas: &mut CanvasView,
    bid: i64,
    target: AsideDragTarget,
    pointer_pos: Pos2,
) {
    canvas.bubble_runtime.aside_drag_state = Some(super::types::AsideDragState {
        bid,
        target,
        last_pointer_pos: pointer_pos,
        moved: false,
    });
}

pub(super) fn drag_aside_by_pointer(
    canvas: &mut CanvasView,
    bid: i64,
    image_rect: Rect,
    pointer_pos: Pos2,
) {
    let Some(mut state) = canvas.bubble_runtime.aside_drag_state else {
        return;
    };
    if state.bid != bid {
        return;
    }
    let dx = pointer_pos.x - state.last_pointer_pos.x;
    let dy = pointer_pos.y - state.last_pointer_pos.y;
    state.last_pointer_pos = pointer_pos;
    canvas.bubble_runtime.aside_drag_state = Some(state);

    if dx.abs() <= f32::EPSILON && dy.abs() <= f32::EPSILON {
        return;
    }
    let du = dx / image_rect.width().max(1.0);
    let dv = dy / image_rect.height().max(1.0);
    match state.target {
        AsideDragTarget::BubbleBody => {
            let Some(current) = canvas.bubble_runtime.runtime_bubbles.get(&bid).cloned() else {
                return;
            };
            canvas.move_bubble_anchor_impl(
                bid,
                current.img_u + du,
                current.img_v + dv,
                true,
                false,
            );
        }
        AsideDragTarget::RectArea => {
            move_bubble_rect_by_delta(canvas, bid, du, dv);
        }
        AsideDragTarget::ImageRedRect => {
            move_image_red_rect_by_delta(canvas, bid, du, dv);
        }
        AsideDragTarget::ImageAreaRect(idx) => {
            move_image_area_rect_by_delta(canvas, bid, idx, du, dv);
        }
        AsideDragTarget::ImageAreaAnchor(idx) => {
            move_image_area_anchor_by_delta(canvas, bid, idx, du, dv);
        }
    }
    if let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) {
        if bubble.bubble_class == BubbleClass::Image {
            // Keep the primary anchor (used for layout/persistence) in sync with area 0.
            if let Some(first) = bubble.text_areas.first() {
                bubble.img_u = first.anchor.x;
                bubble.img_v = first.anchor.y;
            }
            bubble.side = image_bubble_side_from_areas(&bubble.text_areas);
        } else {
            bubble.side = if bubble.img_u < 0.5 {
                Side::Left
            } else {
                Side::Right
            };
        }
    }
    if let Some(state) = canvas.bubble_runtime.aside_drag_state.as_mut()
        && state.bid == bid
    {
        state.moved = true;
        state.last_pointer_pos = pointer_pos;
    }
}

pub(super) fn finish_aside_drag(canvas: &mut CanvasView, bid: i64) {
    let Some(state) = canvas.bubble_runtime.aside_drag_state else {
        return;
    };
    if state.bid != bid {
        return;
    }
    canvas.bubble_runtime.aside_drag_state = None;
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };
    let next_side = if bubble.bubble_class == BubbleClass::Image {
        image_bubble_side_from_areas(&bubble.text_areas)
    } else if bubble.img_u < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    let mut changed = state.moved;
    if bubble.side != next_side {
        bubble.side = next_side;
        changed = true;
    }
    if changed {
        canvas.bubble_runtime.pending_upsert.insert(bid);
    }
}

fn move_bubble_rect_by_delta(canvas: &mut CanvasView, bid: i64, du: f32, dv: f32) {
    let Some(page_idx) = canvas
        .bubble_runtime
        .runtime_bubbles
        .get(&bid)
        .map(|bubble| bubble.img_idx)
    else {
        return;
    };
    let (min_margin_u, min_margin_v) = canvas.bubble_min_uv_margin_for_page(page_idx);
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };

    let mut rect = bubble.rect_coords.normalized();
    rect.p1.x = rect.p1.x.clamp(0.0, 1.0);
    rect.p1.y = rect.p1.y.clamp(0.0, 1.0);
    rect.p2.x = rect.p2.x.clamp(0.0, 1.0);
    rect.p2.y = rect.p2.y.clamp(0.0, 1.0);
    rect = rect.normalized();

    let shift_x = CanvasView::clamp_rect_shift_axis(rect.p1.x, rect.p2.x, du);
    let shift_y = CanvasView::clamp_rect_shift_axis(rect.p1.y, rect.p2.y, dv);
    rect.p1.x += shift_x;
    rect.p2.x += shift_x;
    rect.p1.y += shift_y;
    rect.p2.y += shift_y;
    rect = rect.normalized();

    let anchor = CanvasView::clamp_anchor_to_rect(
        bubble.img_u,
        bubble.img_v,
        rect,
        min_margin_u,
        min_margin_v,
    );
    bubble.rect_coords = rect;
    bubble.img_u = anchor.x;
    bubble.img_v = anchor.y;
}

/// Clamps a 1D translation so that `[lo + shift, hi + shift]` stays inside `[bound_lo, bound_hi]`.
fn clamp_shift_within(lo: f32, hi: f32, bound_lo: f32, bound_hi: f32, desired: f32) -> f32 {
    let min_shift = bound_lo - lo;
    let max_shift = bound_hi - hi;
    if min_shift <= max_shift {
        desired.clamp(min_shift, max_shift)
    } else {
        0.0
    }
}

/// Builds a default text area for a newly added block: a sub-rect of the red rect, offset by
/// `index` so consecutive new areas do not stack exactly on top of each other.
fn default_image_text_area(red: RectCoords, index: usize) -> ImageTextArea {
    let red = red.normalized();
    let w = (red.p2.x - red.p1.x).max(0.001);
    let h = (red.p2.y - red.p1.y).max(0.001);
    let aw = w * 0.5;
    let ah = h * 0.4;
    let off = (index as f32 * 0.06).min(0.4);
    let x1 = (red.p1.x + w * 0.1 + off * w).clamp(red.p1.x, red.p2.x - aw);
    let y1 = (red.p1.y + h * 0.1 + off * h).clamp(red.p1.y, red.p2.y - ah);
    let area_rect = RectCoords {
        p1: egui::pos2(x1, y1),
        p2: egui::pos2(x1 + aw, y1 + ah),
    }
    .normalized();
    ImageTextArea {
        anchor: area_rect.center_uv(),
        area_rect,
        original: String::new(),
        description: String::new(),
        translation: String::new(),
    }
}

/// Moves an image bubble's red rect by `(du, dv)` (clamped into [0,1]) and shifts every text area
/// rect and anchor by the same amount so the whole group travels together.
fn move_image_red_rect_by_delta(canvas: &mut CanvasView, bid: i64, du: f32, dv: f32) {
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };
    let rect = bubble.rect_coords.normalized();
    let shift_x = CanvasView::clamp_rect_shift_axis(rect.p1.x, rect.p2.x, du);
    let shift_y = CanvasView::clamp_rect_shift_axis(rect.p1.y, rect.p2.y, dv);
    if shift_x.abs() <= f32::EPSILON && shift_y.abs() <= f32::EPSILON {
        return;
    }
    bubble.rect_coords = RectCoords {
        p1: egui::pos2(rect.p1.x + shift_x, rect.p1.y + shift_y),
        p2: egui::pos2(rect.p2.x + shift_x, rect.p2.y + shift_y),
    };
    bubble.img_u = (bubble.img_u + shift_x).clamp(0.0, 1.0);
    bubble.img_v = (bubble.img_v + shift_y).clamp(0.0, 1.0);
    for area in &mut bubble.text_areas {
        area.area_rect.p1.x += shift_x;
        area.area_rect.p2.x += shift_x;
        area.area_rect.p1.y += shift_y;
        area.area_rect.p2.y += shift_y;
        area.anchor.x += shift_x;
        area.anchor.y += shift_y;
    }
}

/// Moves text area `area_idx`'s rect by `(du, dv)`, clamped to stay inside the red rect, and shifts
/// its anchor by the same amount so the point keeps its position within the area.
fn move_image_area_rect_by_delta(
    canvas: &mut CanvasView,
    bid: i64,
    area_idx: usize,
    du: f32,
    dv: f32,
) {
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };
    let red = bubble.rect_coords.normalized();
    let Some(area) = bubble.text_areas.get_mut(area_idx) else {
        return;
    };
    let rect = area.area_rect.normalized();
    let shift_x = clamp_shift_within(rect.p1.x, rect.p2.x, red.p1.x, red.p2.x, du);
    let shift_y = clamp_shift_within(rect.p1.y, rect.p2.y, red.p1.y, red.p2.y, dv);
    area.area_rect = RectCoords {
        p1: egui::pos2(rect.p1.x + shift_x, rect.p1.y + shift_y),
        p2: egui::pos2(rect.p2.x + shift_x, rect.p2.y + shift_y),
    };
    area.anchor.x += shift_x;
    area.anchor.y += shift_y;
}

/// Moves text area `area_idx`'s anchor by `(du, dv)`, clamped to stay inside its own rect.
fn move_image_area_anchor_by_delta(
    canvas: &mut CanvasView,
    bid: i64,
    area_idx: usize,
    du: f32,
    dv: f32,
) {
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };
    let Some(area) = bubble.text_areas.get_mut(area_idx) else {
        return;
    };
    let rect = area.area_rect.normalized();
    area.anchor.x = (area.anchor.x + du).clamp(rect.p1.x, rect.p2.x);
    area.anchor.y = (area.anchor.y + dv).clamp(rect.p1.y, rect.p2.y);
}

/// Applies the per-frame editable image-bubble edits: text drafts, add/remove area, and the
/// recorded row-block rects (used next frame to route card-body drags). Re-normalizes areas inside
/// the red rect, mirrors area 0's text to the legacy fields, recomputes the side, and marks an
/// upsert when anything changed.
#[allow(clippy::too_many_arguments)]
fn apply_image_area_edits(
    canvas: &mut CanvasView,
    bid: i64,
    drafts: &[(String, String, String)],
    text_changed: bool,
    want_add: bool,
    remove: Option<usize>,
    block_rects: &[Rect],
) {
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };
    bubble.image_block_rects = block_rects.to_vec();

    let mut text_dirty = false;
    if text_changed {
        for (idx, (orig, desc, tr)) in drafts.iter().enumerate() {
            if let Some(area) = bubble.text_areas.get_mut(idx)
                && (&area.original != orig || &area.description != desc || &area.translation != tr)
            {
                area.original = orig.clone();
                area.description = desc.clone();
                area.translation = tr.clone();
                text_dirty = true;
            }
        }
    }

    let mut structure_dirty = false;
    if let Some(idx) = remove
        && idx > 0
        && idx < bubble.text_areas.len()
    {
        bubble.text_areas.remove(idx);
        structure_dirty = true;
    }
    if want_add {
        let new_area = default_image_text_area(bubble.rect_coords, bubble.text_areas.len());
        bubble.text_areas.push(new_area);
        structure_dirty = true;
    }

    if text_dirty || structure_dirty {
        let red = bubble.rect_coords;
        normalize_image_text_areas(&mut bubble.text_areas, red);
        if let Some(first) = bubble.text_areas.first() {
            bubble.original_text = first.original.clone();
            bubble.text = first.translation.clone();
            bubble.img_u = first.anchor.x;
            bubble.img_v = first.anchor.y;
        }
        bubble.side = image_bubble_side_from_areas(&bubble.text_areas);
        if structure_dirty {
            // Area count changed, so the card height changes; re-measure next layout.
            bubble.mounted = false;
        }
        canvas.bubble_runtime.pending_upsert.insert(bid);
    }
}

/// Renders the editable image bubble's per-area row blocks and the "add area" button.
///
/// Each block frames `Оригинал` / `Описание` / `Перевод` for one text area in that area's palette
/// color; blocks after the first carry a remove (`✕`) button. Records each block's scene rect into
/// `block_rects` so the caller can target links at block centers and route card-body drags.
#[allow(clippy::too_many_arguments)]
fn draw_editable_image_areas(
    canvas: &mut CanvasView,
    ui: &mut egui::Ui,
    bid: i64,
    text_width: f32,
    drafts: &mut [(String, String, String)],
    want_add: &mut bool,
    remove: &mut Option<usize>,
    text_changed: &mut bool,
    block_rects: &mut Vec<Rect>,
    bubble_has_focus: &mut bool,
    interacted: &mut bool,
) {
    let field_width = (text_width - 20.0).max(24.0);
    for (idx, draft) in drafts.iter_mut().enumerate() {
        let color = image_area_palette(idx);
        let block = egui::Frame::new()
            .stroke(Stroke::new(2.0, color))
            .corner_radius(CornerRadius::same(4))
            .inner_margin(egui::Margin::same(6))
            .show(ui, |ui| {
                if idx > 0 {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(tf!("canvas.image_area.area_index", idx = idx + 1))
                                .color(color)
                                .small(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("✕").clicked() {
                                *remove = Some(idx);
                                *interacted = true;
                            }
                        });
                    });
                }
                let orig = with_bubble_text_font(ui, |ui| {
                    SpellcheckedTextEdit::multiline(&mut draft.0)
                        .id_salt(("aside_img_original", bid, idx))
                        .hint_text(t!("canvas.bubble.original_label"))
                        .desired_width(field_width)
                        .desired_rows(1)
                        .spellcheck_enabled(canvas.state.spellcheck_original)
                        .show(ui)
                });
                let desc = with_bubble_text_font(ui, |ui| {
                    egui::TextEdit::multiline(&mut draft.1)
                        .id_salt(("aside_img_description", bid, idx))
                        .hint_text(t!("canvas.image_area.description_label"))
                        .desired_width(field_width)
                        .desired_rows(1)
                        .show(ui)
                });
                let tr = with_bubble_text_font(ui, |ui| {
                    SpellcheckedTextEdit::multiline(&mut draft.2)
                        .id_salt(("aside_img_translation", bid, idx))
                        .hint_text(t!("canvas.bubble.translation_label"))
                        .desired_width(field_width)
                        .desired_rows(1)
                        .spellcheck_enabled(canvas.state.spellcheck_translation)
                        .show(ui)
                });
                for resp in [&orig.response, &desc.response, &tr.response] {
                    if resp.changed() {
                        *text_changed = true;
                        *interacted = true;
                        canvas.schedule_text_upsert(bid, ui.ctx().input(|i| i.time));
                    }
                    if resp.has_focus() {
                        canvas.bubble_runtime.selected_bubble = Some(bid);
                        *bubble_has_focus = true;
                        *interacted = true;
                    }
                    if resp.lost_focus() {
                        canvas.commit_text_upsert_now(bid);
                    }
                }
            });
        block_rects.push(block.response.rect);
        ui.add_space(4.0);
    }
    if ui.button(t!("canvas.image_area.add_text_area_button")).clicked() {
        *want_add = true;
        *interacted = true;
    }
}

/// The 8 resize handle positions (4 corners + 4 edge midpoints) of a scene rect.
fn image_area_handle_points(rect: Rect) -> [Pos2; 8] {
    let center = rect.center();
    [
        egui::pos2(rect.left(), rect.top()),
        egui::pos2(center.x, rect.top()),
        egui::pos2(rect.right(), rect.top()),
        egui::pos2(rect.right(), center.y),
        egui::pos2(rect.right(), rect.bottom()),
        egui::pos2(center.x, rect.bottom()),
        egui::pos2(rect.left(), rect.bottom()),
        egui::pos2(rect.left(), center.y),
    ]
}

/// Draws and interacts the image bubble's page overlay.
///
/// The red rect is the image area and also area 0's region: it is drawn red, is movable, and has
/// 8 resize handles. Area 0 has no separate box — only its anchor point. Areas >= 1 each draw their
/// own colored sub-rect (inside the red rect), a draggable anchor point, and resize handles.
/// Returns `true` if a drag finished this frame so the caller can flush the move.
fn draw_image_bubble_page_overlay(
    canvas: &mut CanvasView,
    ui: &mut egui::Ui,
    bid: i64,
    image_rect: Rect,
    can_drag: bool,
) -> bool {
    let Some((red_coords, areas)) = canvas
        .bubble_runtime
        .runtime_bubbles
        .get(&bid)
        .map(|bubble| (bubble.rect_coords, bubble.text_areas.clone()))
    else {
        return false;
    };
    let sense = if canvas.scene.zoom_drag_active {
        Sense::hover()
    } else {
        Sense::click_and_drag()
    };
    let mut drag_stopped = false;
    let red = CanvasView::rect_from_coords(image_rect, red_coords).intersect(image_rect);
    let anchor_scene = |area: &ImageTextArea| {
        egui::pos2(
            image_rect.left() + area.anchor.x.clamp(0.0, 1.0) * image_rect.width(),
            image_rect.top() + area.anchor.y.clamp(0.0, 1.0) * image_rect.height(),
        )
    };
    // Scene rects of all text-area sub-boxes, used to guard the red-rect move (grabbing inside any
    // area moves that area, not the red image rect).
    let sub_rects: Vec<Rect> = areas
        .iter()
        .map(|area| CanvasView::rect_from_coords(image_rect, area.area_rect).intersect(image_rect))
        .collect();

    if red.is_positive() {
        ui.painter().rect_stroke(
            red,
            0.0,
            Stroke::new(3.0, Color32::from_rgb(220, 60, 60)),
            egui::StrokeKind::Inside,
        );
        let red_resp = ui.interact(red, Id::new(("aside_img_red", bid)), sense);
        // Move the red rect only when not over a resize handle, a sub-area, or any anchor point.
        let on_red_handle = red_resp.interact_pointer_pos().is_some_and(|pos| {
            image_area_handle_points(red)
                .iter()
                .any(|point| Rect::from_center_size(*point, egui::vec2(10.0, 10.0)).contains(pos))
        });
        let on_sub_area = red_resp
            .interact_pointer_pos()
            .is_some_and(|pos| sub_rects.iter().any(|rect| rect.contains(pos)));
        let on_any_anchor = red_resp.interact_pointer_pos().is_some_and(|pos| {
            areas
                .iter()
                .any(|area| anchor_scene(area).distance(pos) <= 8.0)
        });
        let can_move_red = can_drag && !on_red_handle && !on_sub_area && !on_any_anchor;
        if can_move_red
            && red_resp.drag_started()
            && let Some(pos) = red_resp.interact_pointer_pos()
        {
            start_aside_drag(canvas, bid, AsideDragTarget::ImageRedRect, pos);
        }
        if can_move_red
            && red_resp.dragged()
            && let Some(pos) = red_resp.interact_pointer_pos()
        {
            drag_aside_by_pointer(canvas, bid, image_rect, pos);
        }
        if canvas
            .bubble_runtime
            .aside_drag_state
            .is_some_and(|state| state.bid == bid)
            && red_resp.drag_stopped()
        {
            drag_stopped = true;
        }
    }

    for (idx, area) in areas.iter().enumerate() {
        let color = image_area_palette(idx);
        let anchor_pos = anchor_scene(area);

        // Every area draws its own movable, resizable sub-rect inside the red image area.
        let area_rect =
            CanvasView::rect_from_coords(image_rect, area.area_rect).intersect(image_rect);
        if area_rect.is_positive() {
            ui.painter().rect_stroke(
                area_rect,
                0.0,
                Stroke::new(2.0, color),
                egui::StrokeKind::Inside,
            );
            let area_resp = ui.interact(area_rect, Id::new(("aside_img_area", bid, idx)), sense);
            let on_handle = area_resp.interact_pointer_pos().is_some_and(|pos| {
                image_area_handle_points(area_rect).iter().any(|point| {
                    Rect::from_center_size(*point, egui::vec2(10.0, 10.0)).contains(pos)
                })
            });
            let on_anchor = area_resp
                .interact_pointer_pos()
                .is_some_and(|pos| anchor_pos.distance(pos) <= 8.0);
            let can_move_area = can_drag && !on_handle && !on_anchor;
            if can_move_area
                && area_resp.drag_started()
                && let Some(pos) = area_resp.interact_pointer_pos()
            {
                start_aside_drag(canvas, bid, AsideDragTarget::ImageAreaRect(idx), pos);
            }
            if can_move_area
                && area_resp.dragged()
                && let Some(pos) = area_resp.interact_pointer_pos()
            {
                drag_aside_by_pointer(canvas, bid, image_rect, pos);
            }
            if canvas
                .bubble_runtime
                .aside_drag_state
                .is_some_and(|state| state.bid == bid)
                && area_resp.drag_stopped()
            {
                drag_stopped = true;
            }
            draw_image_area_handles(canvas, ui, bid, idx, image_rect, area.area_rect, color);
        }

        // Anchor point: draggable only within its own area.
        let anchor_hit = Rect::from_center_size(anchor_pos, egui::vec2(16.0, 16.0));
        let anchor_resp = ui.interact(anchor_hit, Id::new(("aside_img_anchor", bid, idx)), sense);
        if can_drag
            && anchor_resp.drag_started()
            && let Some(pos) = anchor_resp.interact_pointer_pos()
        {
            start_aside_drag(canvas, bid, AsideDragTarget::ImageAreaAnchor(idx), pos);
        }
        if can_drag
            && anchor_resp.dragged()
            && let Some(pos) = anchor_resp.interact_pointer_pos()
        {
            drag_aside_by_pointer(canvas, bid, image_rect, pos);
        }
        if canvas
            .bubble_runtime
            .aside_drag_state
            .is_some_and(|state| state.bid == bid)
            && anchor_resp.drag_stopped()
        {
            drag_stopped = true;
        }
        ui.painter().circle_filled(anchor_pos, 5.0, color);
        ui.painter()
            .circle_stroke(anchor_pos, 5.0, Stroke::new(1.5, Color32::WHITE));
    }

    // Red-rect resize handles (drawn last so they take interaction priority at the corners). After
    // a resize, re-pin area 0 to the red rect and keep sub-areas inside it.
    super::bubble_on_top_ui::draw_rect_handles(canvas, ui, bid, image_rect, red_coords);
    if let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) {
        let red_now = bubble.rect_coords;
        normalize_image_text_areas(&mut bubble.text_areas, red_now);
    }
    drag_stopped
}

/// Draws the 8 resize handles of one text area rect and resizes the area (clamped inside the red
/// rect) when a handle is dragged.
fn draw_image_area_handles(
    canvas: &mut CanvasView,
    ui: &mut egui::Ui,
    bid: i64,
    area_idx: usize,
    image_rect: Rect,
    area_coords: RectCoords,
    color: Color32,
) {
    let rect = CanvasView::rect_from_coords(image_rect, area_coords).intersect(image_rect);
    if !rect.is_positive() {
        return;
    }
    for (handle_idx, point) in image_area_handle_points(rect).iter().enumerate() {
        let handle_rect = Rect::from_center_size(*point, egui::vec2(10.0, 10.0));
        let response = ui.interact(
            handle_rect,
            Id::new(("aside_img_handle", bid, area_idx, handle_idx)),
            Sense::click_and_drag(),
        );
        ui.painter().circle_filled(*point, 4.0, Color32::WHITE);
        ui.painter()
            .circle_stroke(*point, 4.0, Stroke::new(1.0, color));
        if response.dragged() {
            canvas.bubble_runtime.active_area_handle = Some((bid, area_idx, handle_idx));
            if let Some(pos) = response.interact_pointer_pos() {
                resize_image_area_by_handle(canvas, bid, area_idx, handle_idx, image_rect, pos);
            }
        }
        if response.drag_stopped() {
            canvas.bubble_runtime.active_area_handle = None;
            canvas.bubble_runtime.pending_upsert.insert(bid);
        }
    }
}

/// Resizes text area `area_idx` from handle `handle_idx` toward `scene_pos`, then re-normalizes the
/// area inside the red rect and keeps the anchor inside it.
fn resize_image_area_by_handle(
    canvas: &mut CanvasView,
    bid: i64,
    area_idx: usize,
    handle_idx: usize,
    image_rect: Rect,
    scene_pos: Pos2,
) {
    let min_sc = (8.0 / canvas.state.zoom.max(0.2)).max(4.0);
    let Some(bubble) = canvas.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
        return;
    };
    let red = bubble.rect_coords;
    {
        let Some(area) = bubble.text_areas.get_mut(area_idx) else {
            return;
        };
        let mut rect =
            CanvasView::rect_from_coords(image_rect, area.area_rect).intersect(image_rect);
        if !rect.is_positive() {
            return;
        }
        if matches!(handle_idx, 0 | 6 | 7) {
            rect.set_left(scene_pos.x.min(rect.right() - min_sc));
        }
        if matches!(handle_idx, 2..=4) {
            rect.set_right(scene_pos.x.max(rect.left() + min_sc));
        }
        if matches!(handle_idx, 0..=2) {
            rect.set_top(scene_pos.y.min(rect.bottom() - min_sc));
        }
        if matches!(handle_idx, 4..=6) {
            rect.set_bottom(scene_pos.y.max(rect.top() + min_sc));
        }
        let uv1 = CanvasView::uv_from_scene(image_rect, rect.left_top());
        let uv2 = CanvasView::uv_from_scene(image_rect, rect.right_bottom());
        area.area_rect = RectCoords {
            p1: egui::pos2(uv1.x.min(uv2.x), uv1.y.min(uv2.y)),
            p2: egui::pos2(uv1.x.max(uv2.x), uv1.y.max(uv2.y)),
        }
        .normalized();
    }
    normalize_image_text_areas(&mut bubble.text_areas, red);
}

#[cfg(test)]
mod tests {
    use super::{
        AsideDesiredSlot, AsidePackPermCache, PackedAsideSlot, apply_cached_permutation,
        aside_column_vertical_bounds, aside_slots_overlap, aside_two_column_active,
        aside_two_column_card_width, aside_two_column_rects, cluster_crossing_fingerprint,
        index_bubbles_by_id, pack_aside_slots, split_near_priority,
    };
    use crate::canvas::types::AsideItem;
    use crate::project::{Bubble, Side};
    use eframe::egui::{self, Rect};
    use serde_json::Map;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn test_bubble(id: i64) -> Bubble {
        Bubble {
            id,
            img_idx: 0,
            img_u: 0.0,
            img_v: 0.0,
            side: None,
            bubble_class: None,
            bubble_type: None,
            text: String::new(),
            original_text: String::new(),
            extra: Map::new(),
        }
    }

    // The per-frame index must resolve present ids to the matching bubble and report `None` for
    // absent ids, so the aside-draw fallback path behaves exactly as the former linear `find`.
    #[test]
    fn bubble_index_resolves_present_and_missing_ids() {
        let bubbles = vec![test_bubble(7), test_bubble(3), test_bubble(42)];
        let index = index_bubbles_by_id(&bubbles);
        assert_eq!(index.len(), 3);
        assert_eq!(index.get(&7).map(|b| b.id), Some(7));
        assert_eq!(index.get(&3).map(|b| b.id), Some(3));
        assert_eq!(index.get(&42).map(|b| b.id), Some(42));
        assert!(!index.contains_key(&100));
        // Resolved reference points at the actual slice element, not a copy.
        assert_eq!(
            index.get(&3).map(|b| std::ptr::eq(*b, &bubbles[1])),
            Some(true)
        );
    }

    // Two cards with a clear vertical gap must NOT merge into one cluster: their on-screen rows do
    // not touch, so the column can place them at their own anchors.
    #[test]
    fn separated_cards_do_not_merge() {
        let card_h = 40.0_f32;
        let first_bottom = 100.0 + card_h * 0.5; // first card centered at y=100
        let second_top = 200.0 - card_h * 0.5; // second card centered at y=200, well below
        assert!(!aside_slots_overlap(second_top, first_bottom, 0.0));
    }

    // Two cards whose rows truly collide (overlapping y-extent) MUST merge: this is the
    // anti-overlap guarantee that low-zoom clustering relies on.
    #[test]
    fn colliding_cards_merge() {
        let card_h = 40.0_f32;
        let first_bottom = 100.0 + card_h * 0.5; // first card bottom at y=120
        let second_top = 110.0 - card_h * 0.5; // second card top at y=90, overlaps first
        assert!(aside_slots_overlap(second_top, first_bottom, 0.0));
    }

    // Exactly touching edges count as overlap (closed interval), so cards never visually abut.
    #[test]
    fn touching_cards_merge() {
        assert!(aside_slots_overlap(120.0, 120.0, 0.0));
    }

    // At low zoom the page span is short while the viewport is tall: the usable column span must be
    // taken from the viewport, not the zoom-shrunk page height, so clusters can relax outward.
    #[test]
    fn low_zoom_uses_viewport_span_not_page_span() {
        // Page occupies only [300, 360] on screen (60px tall); viewport is [0, 800].
        let [top, bottom] = aside_column_vertical_bounds(300.0, 360.0, 0.0, 800.0);
        assert!((top - 0.0).abs() < f32::EPSILON);
        assert!((bottom - 800.0).abs() < f32::EPSILON);
        assert!(bottom - top > 360.0 - 300.0);
    }

    // At high zoom the page is taller than the viewport: bounds must include the full page span so
    // off-screen anchors stay reachable and behavior is unchanged from the page-only bound.
    #[test]
    fn high_zoom_keeps_full_page_span() {
        let [top, bottom] = aside_column_vertical_bounds(-500.0, 1500.0, 0.0, 800.0);
        assert!((top - (-500.0)).abs() < f32::EPSILON);
        assert!((bottom - 1500.0).abs() < f32::EPSILON);
    }

    // A degenerate/unknown viewport must fall back to the page span without panicking.
    #[test]
    fn degenerate_viewport_falls_back_to_page_span() {
        let [top, bottom] = aside_column_vertical_bounds(100.0, 200.0, f32::NAN, f32::NAN);
        assert!((top - 100.0).abs() < f32::EPSILON);
        assert!((bottom - 200.0).abs() < f32::EPSILON);
        let [t2, b2] = aside_column_vertical_bounds(100.0, 200.0, 500.0, 400.0);
        assert!((t2 - 100.0).abs() < f32::EPSILON);
        assert!((b2 - 200.0).abs() < f32::EPSILON);
    }

    // The mode activates only when both columns plus gaps fit inside the viewport, so the far column
    // never spills past the viewport edge before its bubbles appear.
    #[test]
    fn two_column_activation_threshold() {
        let (min_width, spacing) = (200.0_f32, 20.0_f32);
        let threshold = 2.0 * min_width + 3.0 * spacing; // 460
        assert!(!aside_two_column_active(
            threshold - 1.0,
            min_width,
            spacing
        ));
        assert!(!aside_two_column_active(
            min_width * 1.1,
            min_width,
            spacing
        )); // old 110% -> too tight now
        assert!(aside_two_column_active(threshold, min_width, spacing));
        assert!(aside_two_column_active(
            threshold + 200.0,
            min_width,
            spacing
        ));
    }

    fn split_slot(bid: i64, cy: f32, h: f32) -> AsideDesiredSlot {
        AsideDesiredSlot {
            item: AsideItem {
                bid,
                area_idx: None,
            },
            width: 100.0,
            desired_cy: cy,
            h,
            source_scene_x: 0.0,
            source_scene_y: cy,
            angle_key: cy,
            is_spacer: false,
        }
    }

    // Near-priority: well-separated bubbles all stay near (far column empty); only an overlapping
    // cluster is split alternately between near and far.
    #[test]
    fn near_priority_splits_only_dense_clusters() {
        // Sparse: three cards far apart -> all near.
        let sparse = vec![
            split_slot(1, 100.0, 40.0),
            split_slot(2, 300.0, 40.0),
            split_slot(3, 500.0, 40.0),
        ];
        let (near, far) = split_near_priority(sparse, 8.0);
        assert_eq!(near.len(), 3);
        assert!(far.is_empty());

        // Dense: four overlapping cards -> alternate, two near and two far.
        let dense = vec![
            split_slot(1, 100.0, 120.0),
            split_slot(2, 140.0, 120.0),
            split_slot(3, 180.0, 120.0),
            split_slot(4, 220.0, 120.0),
        ];
        let (near, far) = split_near_priority(dense, 8.0);
        assert_eq!(near.len(), 2);
        assert_eq!(far.len(), 2);
        // The first (topmost) card stays near.
        assert_eq!(near[0].item.bid, 1);
    }

    // Column width: floored at min, capped at max only when scaling, fixed at min when not scaling.
    #[test]
    fn two_column_card_width_rules() {
        let (min_w, max_w, sp) = (100.0_f32, 300.0_f32, 20.0_f32);
        // Plenty of room -> capped at max width.
        assert!((aside_two_column_card_width(900.0, sp, min_w, max_w, true) - max_w).abs() < 0.01);
        // Moderate room -> equal split of the usable span: (300-60)/2 = 120.
        assert!((aside_two_column_card_width(300.0, sp, min_w, max_w, true) - 120.0).abs() < 0.01);
        // Tight room -> floored at the minimum width (columns may overlap/overflow).
        assert!((aside_two_column_card_width(150.0, sp, min_w, max_w, true) - min_w).abs() < 0.01);
        // Scaling off -> always the minimum width, regardless of room.
        assert!((aside_two_column_card_width(900.0, sp, min_w, max_w, false) - min_w).abs() < 0.01);
    }

    // The near column hugs the ribbon edge; the far column is offset one more column width outward,
    // and both columns are equal width. Mirrored for the right side.
    #[test]
    fn two_column_rects_geometry() {
        let image = Rect::from_min_max(egui::pos2(400.0, 0.0), egui::pos2(800.0, 1000.0));
        let row = image;
        let (col_w, spacing) = (120.0_f32, 20.0_f32);

        let (near, far) = aside_two_column_rects(Side::Right, image, row, col_w, spacing);
        assert!((near.width() - col_w).abs() < 0.01 && (far.width() - col_w).abs() < 0.01);
        assert!((near.left() - (image.right() + spacing)).abs() < 0.01);
        assert!(far.left() > near.left()); // far is further from the ribbon
        assert!((far.left() - (near.right() + spacing)).abs() < 0.01);

        let (near_l, far_l) = aside_two_column_rects(Side::Left, image, row, col_w, spacing);
        assert!((near_l.right() - (image.left() - spacing)).abs() < 0.01);
        assert!(far_l.left() < near_l.left()); // far is further left (from the ribbon)
        assert!((near_l.left() - (far_l.right() + spacing)).abs() < 0.01);
    }

    fn pack_slot(bid: i64, cy: f32, h: f32, spacer: bool, col_left: f32) -> AsideDesiredSlot {
        AsideDesiredSlot {
            item: AsideItem {
                bid,
                area_idx: None,
            },
            width: if spacer { 0.0 } else { 100.0 },
            desired_cy: cy,
            h,
            source_scene_x: col_left - 100.0, // ribbon-side origin, left of the right column
            source_scene_y: cy,
            angle_key: cy, // monotonic in vertical order -> stable cluster ordering
            is_spacer: spacer,
        }
    }

    // A far-line spacer placed between two near cards must open a real gap: after packing, no drawn
    // (non-spacer) card overlaps the spacer's vertical band, so the far link can pass between them.
    #[test]
    fn spacer_opens_gap_between_near_cards() {
        let column = Rect::from_min_max(egui::pos2(1000.0, 0.0), egui::pos2(1100.0, 2000.0));
        let viewport = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1200.0, 2000.0));
        let gap = 4.0;
        // Two near cards whose rows would otherwise abut, with a spacer at the far anchor between.
        let desired = vec![
            pack_slot(1, 500.0, 120.0, false, column.left()),
            pack_slot(-1, 560.0, 40.0, true, column.left()),
            pack_slot(2, 620.0, 120.0, false, column.left()),
        ];
        let packed = pack_aside_slots(desired, Side::Right, column, viewport, gap, None);

        let band = |s: &PackedAsideSlot| (s.cy - s.h * 0.5, s.cy + s.h * 0.5);
        let spacer = packed
            .iter()
            .find(|s| s.is_spacer)
            .expect("spacer must survive packing");
        let (sp_top, sp_bottom) = band(spacer);
        let reals: Vec<&PackedAsideSlot> = packed.iter().filter(|s| !s.is_spacer).collect();
        assert_eq!(reals.len(), 2);
        for real in &reals {
            let (top, bottom) = band(real);
            // No real card overlaps the spacer band (a clear gap exists for the far link).
            assert!(
                bottom <= sp_top + f32::EPSILON || top >= sp_bottom - f32::EPSILON,
                "real card [{top},{bottom}] overlaps spacer band [{sp_top},{sp_bottom}]"
            );
        }
        // The two near cards are separated by at least the spacer height plus gaps.
        let mut centers: Vec<f32> = reals.iter().map(|s| s.cy).collect();
        centers.sort_by(f32::total_cmp);
        assert!(centers[1] - centers[0] >= 120.0 * 0.5 + 40.0 + 120.0 * 0.5);
    }

    // A genuinely static cluster (recomputed bit-for-bit from identical inputs) must still yield the
    // SAME fingerprint, so the idle case the audit targets keeps hitting the cache instead of
    // re-running the crossing minimizer every frame. Exact-bit hashing also treats `-0.0` like `+0.0`
    // (geometrically equal, never flips a crossing) so a signed-zero coordinate does not bust it.
    #[test]
    fn fingerprint_stable_for_identical_cluster() {
        let a = vec![
            pack_slot(1, 100.0, 40.0, false, 1000.0),
            pack_slot(2, 130.0, 40.0, false, 1000.0),
        ];
        let b = a.clone();
        let key_a = cluster_crossing_fingerprint(&a, Side::Right, 1000.0, 4.0, 50.0);
        let key_b = cluster_crossing_fingerprint(&b, Side::Right, 1000.0, 4.0, 50.0);
        assert_eq!(
            key_a, key_b,
            "an unchanged cluster must keep hitting the cache"
        );

        // Replacing a +0.0 coordinate with -0.0 must not change the key (no crossing can flip).
        let mut signed_zero = a.clone();
        signed_zero[0].source_scene_x = -0.0;
        let plus_zero = {
            let mut c = a.clone();
            c[0].source_scene_x = 0.0;
            c
        };
        assert_eq!(
            cluster_crossing_fingerprint(&plus_zero, Side::Right, 1000.0, 4.0, 50.0),
            cluster_crossing_fingerprint(&signed_zero, Side::Right, 1000.0, 4.0, 50.0),
            "signed zero must not bust the cache",
        );
    }

    // 🔴 REGRESSION: the heart of the audited bug. A geometry change SMALL enough to stay inside the
    // old 0.5pt quantization bucket (so the old fingerprint stayed identical) but LARGE enough to
    // FLIP a crossing decision must now change the fingerprint, so the cache correctly recomputes the
    // layout instead of replaying a stale permutation that crosses links.
    #[test]
    fn fingerprint_changes_on_subbucket_geometry_that_flips_a_crossing() {
        let old_bucket = 0.5_f32; // the former ASIDE_PACK_QUANT
        // Two cards anchored on the right ribbon. Card 2 sits a hair below card 1; both target the
        // same packed rows. A tiny vertical nudge of card 2's anchor (well under half a point, so the
        // old half-point bucket would NOT change) is enough to move `diff_at_overlap` across the
        // `lines_cross` ±0.001 boundary and flip whether the two links cross.
        let side = Side::Right;
        let oriented_target_x = 1000.0_f32;
        let gap = 4.0_f32;
        let top = 50.0_f32;
        let base = vec![
            pack_slot(1, 100.0, 40.0, false, oriented_target_x),
            pack_slot(2, 100.0, 40.0, false, oriented_target_x),
        ];
        // Place the two source anchors so the crossing decision sits right on the epsilon edge, then
        // nudge by a sub-bucket amount that crosses it. The exact-bit fingerprint must react.
        let nudge = old_bucket * 0.3; // 0.15pt: inside the old bucket, far above the 0.001 epsilon.
        let mut nudged = base.clone();
        nudged[1].source_scene_y += nudge;

        let key_base = cluster_crossing_fingerprint(&base, side, oriented_target_x, gap, top);
        let key_nudged = cluster_crossing_fingerprint(&nudged, side, oriented_target_x, gap, top);
        assert!(
            nudge < old_bucket * 0.5,
            "the nudge must be inside the old half-point bucket to model the bug",
        );
        assert!(
            nudge > 0.001,
            "the nudge must exceed the lines_cross epsilon so it can flip a crossing",
        );
        assert_ne!(
            key_base, key_nudged,
            "a sub-bucket geometry change that can flip a crossing must bust the cache",
        );
    }

    // Moving a bubble's anchor MUST change the fingerprint so the layout is recomputed (positions
    // actually moved, the cached ordering may no longer be optimal). Membership and side changes too.
    #[test]
    fn fingerprint_changes_when_bubble_moves() {
        let a = vec![
            pack_slot(1, 100.0, 40.0, false, 1000.0),
            pack_slot(2, 130.0, 40.0, false, 1000.0),
        ];
        let mut moved = a.clone();
        moved[1].source_scene_y += 3.0;
        let key_a = cluster_crossing_fingerprint(&a, Side::Right, 1000.0, 4.0, 50.0);
        let key_moved = cluster_crossing_fingerprint(&moved, Side::Right, 1000.0, 4.0, 50.0);
        assert_ne!(key_a, key_moved);

        // Removing a bubble (different membership/length) also changes the key.
        let key_fewer = cluster_crossing_fingerprint(&a[..1], Side::Right, 1000.0, 4.0, 50.0);
        assert_ne!(key_a, key_fewer);

        // A different column side changes the optimal ordering, so it must change the key.
        let key_left = cluster_crossing_fingerprint(&a, Side::Left, 1000.0, 4.0, 50.0);
        assert_ne!(key_a, key_left);
    }

    // The cache must be transparent: packing the same dense cluster with a fresh cache (cold, then
    // warm) yields byte-identical placements, and the warm pass replays the stored permutation
    // instead of recomputing. This guards visual equivalence of the caching path.
    #[test]
    fn cached_pack_matches_uncached_pack() {
        let column = Rect::from_min_max(egui::pos2(1000.0, 0.0), egui::pos2(1100.0, 3000.0));
        let viewport = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1200.0, 3000.0));
        let gap = 4.0;
        // A dense colliding cluster whose anchors are deliberately out of vertical order so the
        // crossing minimizer actually reorders cards (otherwise the test would not exercise it).
        let make = || {
            vec![
                pack_slot(1, 500.0, 120.0, false, column.left()),
                pack_slot(2, 540.0, 120.0, false, column.left()),
                pack_slot(3, 520.0, 120.0, false, column.left()),
                pack_slot(4, 560.0, 120.0, false, column.left()),
            ]
        };

        let baseline = pack_aside_slots(make(), Side::Right, column, viewport, gap, None);

        let cache: AsidePackPermCache = Arc::new(Mutex::new(HashMap::new()));
        let cold = pack_aside_slots(make(), Side::Right, column, viewport, gap, Some(&cache));
        // A permutation was stored on the cold (cache-miss) pass.
        assert_eq!(cache.lock().expect("cache lock").len(), 1);
        let warm = pack_aside_slots(make(), Side::Right, column, viewport, gap, Some(&cache));

        let same = |a: &[PackedAsideSlot], b: &[PackedAsideSlot]| {
            a.len() == b.len()
                && a.iter().zip(b).all(|(x, y)| {
                    x.item.bid == y.item.bid
                        && x.item.area_idx == y.item.area_idx
                        && (x.cy - y.cy).abs() < f32::EPSILON
                        && (x.h - y.h).abs() < f32::EPSILON
                })
        };
        assert!(same(&baseline, &cold), "cold cache must match uncached");
        assert!(same(&baseline, &warm), "warm cache must match uncached");
    }

    // The replayed permutation must be a valid permutation of the cluster; an invalid one (wrong
    // length or duplicate index) is rejected so a fingerprint collision degrades to a recompute,
    // never to a corrupted ordering.
    #[test]
    fn apply_cached_permutation_validates() {
        let mut items = vec![
            pack_slot(1, 100.0, 40.0, false, 1000.0),
            pack_slot(2, 130.0, 40.0, false, 1000.0),
            pack_slot(3, 160.0, 40.0, false, 1000.0),
        ];
        // Valid reversal permutation applies and reorders.
        assert!(apply_cached_permutation(&mut items, &[2, 1, 0]));
        assert_eq!(
            items.iter().map(|s| s.item.bid).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        // Wrong length is rejected, items untouched.
        let before: Vec<i64> = items.iter().map(|s| s.item.bid).collect();
        assert!(!apply_cached_permutation(&mut items, &[0, 1]));
        assert_eq!(items.iter().map(|s| s.item.bid).collect::<Vec<_>>(), before);
        // Duplicate index is rejected.
        assert!(!apply_cached_permutation(&mut items, &[0, 0, 1]));
        assert_eq!(items.iter().map(|s| s.item.bid).collect::<Vec<_>>(), before);
    }
}
