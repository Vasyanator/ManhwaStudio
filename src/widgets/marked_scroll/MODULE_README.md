# Module: src/widgets/marked_scroll

## Purpose
Vertical scroll area whose scrollbar can carry marks (typed fills, hatching, or
free drawing) painted under the handle, plus elements placed in a gutter to the
left of the bar. Generalizes the New Project "cut arrow" scroll markers into a
reusable widget.

## Architecture
The host `egui::ScrollArea` is the scroll engine only: wheel, drag-to-scroll,
momentum, clipping, and offset/content size. Its native bars are hidden
(`ScrollBarVisibility::AlwaysHidden`). The widget reserves a right-hand strip,
runs the engine in the remaining content rect, then paints a ported bar with
marks and gutter items on top. A handle drag is written back into the engine
`State` (offset only) so kinetic and wheel scrolling stay consistent.

Data flow per frame:
1. `mod.rs::show` reserves the strip and runs the engine → `ScrollAreaOutput`.
2. `bar.rs::run_vertical_bar` rebuilds handle geometry from the output, handles
   the handle drag, and paints `track background -> marks -> handle -> gutter`.
3. `mod.rs::show` writes a dragged offset back into the engine `State`.

## Files and submodules
- `mod.rs`: public `MarkedScrollArea` builder, `MarkedScrollOutput`, strip
  layout, engine invocation, offset write-back. Also exposes `paint_marks_on_bar`
  for decorating a foreign scrollbar (e.g. an `egui::ScrollArea::both` vertical
  bar) that `MarkedScrollArea` cannot own; the caller supplies the `BarGeometry`
  and re-draws the handle on top via `BarGeometry::handle_rect`.
- `bar.rs`: PORT of egui 0.33.3 `ScrollArea` bar block (vertical only) — handle
  geometry, drag, floating opacity/width, layered painting. Upgrade boundary.
- `marks.rs`: typed mark model (`ScrollSpan`, `ScrollSector`, `MarkFill`,
  `ScrollMark`), `BarGeometry` projection, and mark painting; unit-tested math.
- `gutter.rs`: `GutterItem`/`GutterSlot` and the built-in `arrow` helper.

## Contracts and invariants
- Vertical axis only.
- Marks always paint below the handle; `layer` orders marks among themselves
  (stable for equal layers).
- `ScrollSpan::ContentPixels` is in the engine content space (same as
  `content_size`/`offset`); marks map content positions to the full track
  independently of the current offset.
- Painting must not panic on empty/degenerate content (zero content size) or
  out-of-range spans/sectors; bounds are clamped.
- `bar.rs` is a vendored port: keep its `PORT SOURCE` header in sync when the
  egui dependency is bumped. The engine still owns wheel/momentum/clipping; this
  module never duplicates those.

## Editing map
- To change mark kinds, projection, or hatching, edit `marks.rs` (and its tests).
- To change gutter elements or the arrow, edit `gutter.rs`.
- To change bar geometry, drag, opacity, or paint order, edit `bar.rs`.
- To change strip layout or the public builder/output, edit `mod.rs`.

## Testing
`marks.rs` unit-tests the pure projection math (content↔track mapping, span
ordering/clamping, sector thirds, empty content). Drag and painting are GUI
interactions not covered by unit tests; they are exercised through the app.
