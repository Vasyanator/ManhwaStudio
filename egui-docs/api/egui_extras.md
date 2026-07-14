# API index: `egui_extras` 0.35.0

GENERATED FILE — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`. Extracted from rustdoc JSON of the exact crate source in the local cargo registry, so every signature and line number below is real.

**If a name is not in this file, it does not exist in our version of the crate.** Grep here before writing egui code from memory.

Items are listed under the path callers actually write (the public re-export, e.g. `egui::Panel`, `egui::Color32`), not where they happen to be defined. Citations point into the crate that owns the item, so a type `egui` re-exports from `epaint` cites `epaint-0.35.0/src/…`.

## `egui_extras`

### `Size` (enum) — `egui_extras-0.35.0/src/sizing.rs:5`

Size hint for table column/strip cell.

Variants:

- `Size::Absolute` — Absolute size in points, with a given range of allowed sizes to resize within.
- `Size::Relative` — Relative size relative to all available space.
- `Size::Remainder` — Multiple remainders each get the same space.

Methods:

- `fn at_least(self, minimum: f32) -> Self` — `egui_extras-0.35.0/src/sizing.rs:54`
  Won't shrink below this size (in points).
- `fn at_most(self, maximum: f32) -> Self` — `egui_extras-0.35.0/src/sizing.rs:61`
  Won't grow above this size (in points).
- `fn exact(points: f32) -> Self` — `egui_extras-0.35.0/src/sizing.rs:18`
  Exactly this big, with no room for resize.
- `fn initial(points: f32) -> Self` — `egui_extras-0.35.0/src/sizing.rs:26`
  Initially this big, but can resize.
- `fn is_absolute(&self) -> bool` — `egui_extras-0.35.0/src/sizing.rs:90`
- `fn is_relative(&self) -> bool` — `egui_extras-0.35.0/src/sizing.rs:95`
- `fn is_remainder(&self) -> bool` — `egui_extras-0.35.0/src/sizing.rs:100`
- `fn range(self) -> Rangef` — `egui_extras-0.35.0/src/sizing.rs:73`
  Allowed range of movement (in points), if in a resizable [`Table`](crate::table::Table).
- `fn range_mut(&mut self) -> &mut Rangef` — `egui_extras-0.35.0/src/sizing.rs:81`
- `fn relative(fraction: f32) -> Self` — `egui_extras-0.35.0/src/sizing.rs:34`
  Relative size relative to all available space. Values must be in range `0.0..=1.0`.
- `fn remainder() -> Self` — `egui_extras-0.35.0/src/sizing.rs:46`
  Multiple remainders each get the same space.
- `fn with_range(self, range: Rangef) -> Self` — `egui_extras-0.35.0/src/sizing.rs:67`

Implements: `Clone`, `Copy`, `Debug`

### `install_image_loaders` — `egui_extras-0.35.0/src/loaders.rs:58`

```rust
fn install_image_loaders(ctx: &Context)
```

Installs a set of image loaders.

### `Column` (struct) — `egui_extras-0.35.0/src/table.rs:32`

Specifies the properties of a column, like its width range.

Methods:

- `fn at_least(self, minimum: f32) -> Self` — `egui_extras-0.35.0/src/table.rs:125`
  Won't shrink below this width (in points).
- `fn at_most(self, maximum: f32) -> Self` — `egui_extras-0.35.0/src/table.rs:134`
  Won't grow above this width (in points).
- `fn auto() -> Self` — `egui_extras-0.35.0/src/table.rs:53`
  Automatically sized based on content.
- `fn auto_size_this_frame(self, auto_size_this_frame: bool) -> Self` — `egui_extras-0.35.0/src/table.rs:150`
  If set, the column will be automatically sized based on the content this frame.
- `fn auto_with_initial_suggestion(suggested_width: f32) -> Self` — `egui_extras-0.35.0/src/table.rs:62`
  Automatically sized.
- `fn clip(self, clip: bool) -> Self` — `egui_extras-0.35.0/src/table.rs:116`
  If `true`: Allow the column to shrink enough to clip the contents. If `false`: The column will always be wide…
- `fn exact(width: f32) -> Self` — `egui_extras-0.35.0/src/table.rs:72`
  Always this exact width, never shrink or grow.
- `fn initial(width: f32) -> Self` — `egui_extras-0.35.0/src/table.rs:67`
  With this initial width.
- `fn range(self, range: impl Into<Rangef>) -> Self` — `egui_extras-0.35.0/src/table.rs:141`
  Allowed range of movement (in points), if in a resizable [`Table`].
- `fn remainder() -> Self` — `egui_extras-0.35.0/src/table.rs:83`
  Take all the space remaining after the other columns have been sized.
- `fn resizable(self, resizable: bool) -> Self` — `egui_extras-0.35.0/src/table.rs:102`
  Can this column be resized by dragging the column separator?

Implements: `Clone`, `Copy`, `Debug`, `PartialEq`, `StructuralPartialEq`

### `Strip` (struct) — `egui_extras-0.35.0/src/strip.rs:161`

A Strip of cells which go in one direction. Each cell has a fixed size. In contrast to normal egui behavior, strip cells do *not* grow with its children!

Methods:

- `fn cell(&mut self, add_contents: impl FnOnce(&mut Ui))` — `egui_extras-0.35.0/src/strip.rs:191`
  Add cell contents.
- `fn empty(&mut self)` — `egui_extras-0.35.0/src/strip.rs:208`
  Add an empty cell.
- `fn strip(&mut self, strip_builder: impl FnOnce(StripBuilder<'_>))` — `egui_extras-0.35.0/src/strip.rs:214`
  Add a strip as cell.

Implements: `Drop`

### `StripBuilder` (struct) — `egui_extras-0.35.0/src/strip.rs:44`

Builder for creating a new [`Strip`].

Methods:

- `fn cell_layout(self, cell_layout: Layout) -> Self` — `egui_extras-0.35.0/src/strip.rs:74`
  What layout should we use for the individual cells?
- `fn clip(self, clip: bool) -> Self` — `egui_extras-0.35.0/src/strip.rs:67`
  Should we clip the contents of each cell? Default: `false`.
- `fn horizontal<F>(self, strip: F) -> Response` — `egui_extras-0.35.0/src/strip.rs:106`
  Build horizontal strip: Cells are positions from left to right. Takes the available horizontal width, so ther…
- `fn new(ui: &'a mut Ui) -> Self` — `egui_extras-0.35.0/src/strip.rs:54`
  Create new strip builder.
- `fn sense(self, sense: Sense) -> Self` — `egui_extras-0.35.0/src/strip.rs:81`
  What should strip cells sense for? Default: [`egui::Sense::hover()`].
- `fn size(self, size: Size) -> Self` — `egui_extras-0.35.0/src/strip.rs:88`
  Allocate space for one column/row.
- `fn sizes(self, size: Size, count: usize) -> Self` — `egui_extras-0.35.0/src/strip.rs:95`
  Allocate space for several columns/rows at once.
- `fn vertical<F>(self, strip: F) -> Response` — `egui_extras-0.35.0/src/strip.rs:134`
  Build vertical strip: Cells are positions from top to bottom. Takes the full available vertical height, so th…

### `Table` (struct) — `egui_extras-0.35.0/src/table.rs:673`

Table struct which can construct a [`TableBody`].

Methods:

- `fn body<F>(self, add_body_contents: F) -> ScrollAreaOutput<()>` — `egui_extras-0.35.0/src/table.rs:704`
  Create table body after adding a header row
- `fn ui_mut(&mut self) -> &mut Ui` — `egui_extras-0.35.0/src/table.rs:699`
  Access the contained [`egui::Ui`].

### `TableBody` (struct) — `egui_extras-0.35.0/src/table.rs:916`

The body of a table.

Methods:

- `fn heterogeneous_rows(self, heights: impl Iterator<Item = f32>, add_row_content: impl FnMut(TableRow<'_, '_>))` — `egui_extras-0.35.0/src/table.rs:1112`
  Add rows with varying heights.
- `fn max_rect(&self) -> Rect` — `egui_extras-0.35.0/src/table.rs:953`
  Where in screen-space is the table body?
- `fn row(&mut self, height: f32, add_row_content: impl FnOnce(TableRow<'a, '_>))` — `egui_extras-0.35.0/src/table.rs:976`
  Add a single row with the given height.
- `fn rows(self, row_height_sans_spacing: f32, total_rows: usize, add_row_content: impl FnMut(TableRow<'_, '_>))` — `egui_extras-0.35.0/src/table.rs:1027`
  Add many rows with same height.
- `fn ui_mut(&mut self) -> &mut Ui` — `egui_extras-0.35.0/src/table.rs:948`
  Access the contained [`egui::Ui`].
- `fn widths(&self) -> &[f32]` — `egui_extras-0.35.0/src/table.rs:968`
  Return a vector containing all column widths for this table body.

Implements: `Drop`

### `TableBuilder` (struct) — `egui_extras-0.35.0/src/table.rs:247`

Builder for a [`Table`] with (optional) fixed header and scrolling body.

Methods:

- `fn animate_scrolling(self, animated: bool) -> Self` — `egui_extras-0.35.0/src/table.rs:411`
  Should the scroll area animate `scroll_to_*` functions?
- `fn auto_shrink(self, auto_shrink: impl Into<Vec2b>) -> Self` — `egui_extras-0.35.0/src/table.rs:393`
  For each axis (x,y): * If true, add blank space outside the table, keeping the table small. * If false, add b…
- `fn body<F>(self, add_body_contents: F) -> ScrollAreaOutput<()>` — `egui_extras-0.35.0/src/table.rs:527`
  Create table body without a header row
- `fn cell_layout(self, cell_layout: Layout) -> Self` — `egui_extras-0.35.0/src/table.rs:418`
  What layout should we use for the individual cells?
- `fn column(self, column: Column) -> Self` — `egui_extras-0.35.0/src/table.rs:425`
  Allocate space for one column.
- `fn columns(self, column: Column, count: usize) -> Self` — `egui_extras-0.35.0/src/table.rs:432`
  Allocate space for several columns at once.
- `fn drag_to_scroll(self, drag_to_scroll: DragScroll) -> Self` — `egui_extras-0.35.0/src/table.rs:327`
  Controls scrolling the table's contents by dragging with the pointer.
- `fn header(self, height: f32, add_header_row: impl FnOnce(TableRow<'_, '_>)) -> Table<'a>` — `egui_extras-0.35.0/src/table.rs:452`
  Create a header row which always stays visible and at the top
- `fn id_salt(self, id_salt: impl AsIdSalt) -> Self` — `egui_extras-0.35.0/src/table.rs:277`
  Give this table a unique salt within the parent [`Ui`].
- `fn max_scroll_height(self, max_scroll_height: f32) -> Self` — `egui_extras-0.35.0/src/table.rs:380`
  Don't make the scroll area higher than this (add scroll-bars instead!).
- `fn min_scrolled_height(self, min_scrolled_height: f32) -> Self` — `egui_extras-0.35.0/src/table.rs:370`
  The minimum height of a vertical scroll area which requires scroll bars.
- `fn new(ui: &'a mut Ui) -> Self` — `egui_extras-0.35.0/src/table.rs:259`
- `fn reset(&self)` — `egui_extras-0.35.0/src/table.rs:446`
  Reset all column widths.
- `fn resizable(self, resizable: bool) -> Self` — `egui_extras-0.35.0/src/table.rs:309`
  Make the columns resizable by dragging.
- `fn scroll_bar_visibility(self, scroll_bar_visibility: ScrollBarVisibility) -> Self` — `egui_extras-0.35.0/src/table.rs:402`
  Set the visibility of both horizontal and vertical scroll bars.
- `fn scroll_to_row(self, row: usize, align: Option<Align>) -> Self` — `egui_extras-0.35.0/src/table.rs:349`
  Set a row to scroll to.
- `fn sense(self, sense: Sense) -> Self` — `egui_extras-0.35.0/src/table.rs:293`
  What should table cells sense for? (default: [`egui::Sense::hover()`]).
- `fn stick_to_bottom(self, stick: bool) -> Self` — `egui_extras-0.35.0/src/table.rs:336`
  Should the scroll handle stick to the bottom position even as the content size changes dynamically? The scrol…
- `fn striped(self, striped: bool) -> Self` — `egui_extras-0.35.0/src/table.rs:286`
  Enable striped row background for improved readability.
- `fn vertical_scroll_offset(self, offset: f32) -> Self` — `egui_extras-0.35.0/src/table.rs:358`
  Set the vertical scroll offset position, in points.
- `fn vscroll(self, vscroll: bool) -> Self` — `egui_extras-0.35.0/src/table.rs:316`
  Enable vertical scrolling in body (default: `true`)

### `TableRow` (struct) — `egui_extras-0.35.0/src/table.rs:1249`

The row of a table. Is created by [`TableRow`] for each created [`TableBody::row`] or each visible row in rows created by calling [`TableBody::rows`].

Methods:

- `fn col(&mut self, add_cell_contents: impl FnOnce(&mut Ui)) -> (Rect, Response)` — `egui_extras-0.35.0/src/table.rs:1274`
  Add the contents of a column on this row (i.e. a cell).
- `fn col_index(&self) -> usize` — `egui_extras-0.35.0/src/table.rs:1363`
  Returns the index of the column. Incremented after a column is added.
- `fn index(&self) -> usize` — `egui_extras-0.35.0/src/table.rs:1357`
  Returns the index of the row.
- `fn response(&self) -> Response` — `egui_extras-0.35.0/src/table.rs:1349`
  Returns a union of the [`Response`]s of the cells added to the row up to this point.
- `fn set_hovered(&mut self, hovered: bool)` — `egui_extras-0.35.0/src/table.rs:1335`
  Set the hovered highlight state for cells added after a call to this function.
- `fn set_overline(&mut self, overline: bool)` — `egui_extras-0.35.0/src/table.rs:1342`
  Set the overline state for this row. The overline is a line above the row, usable for e.g. visually grouping…
- `fn set_selected(&mut self, selected: bool)` — `egui_extras-0.35.0/src/table.rs:1329`
  Set the selection highlight state for cells added after a call to this function.

Implements: `Drop`


## `egui_extras::syntax_highlighting`

### `code_view_ui` — `egui_extras-0.35.0/src/syntax_highlighting.rs:10`

```rust
fn code_view_ui(ui: &mut Ui, theme: &CodeTheme, code: &str, language: &str) -> Response
```

View some code with syntax highlighting and selection.

### `highlight` — `egui_extras-0.35.0/src/syntax_highlighting.rs:23`

```rust
fn highlight(ctx: &Context, style: &Style, theme: &CodeTheme, code: &str, language: &str) -> LayoutJob
```

Add syntax highlighting to a code string.

### `CodeTheme` (struct) — `egui_extras-0.35.0/src/syntax_highlighting.rs:213`

A selected color theme.

Methods:

- `fn dark(font_size: f32) -> Self` — `egui_extras-0.35.0/src/syntax_highlighting.rs:258`
  ### Example
- `fn from_memory(ctx: &Context, style: &Style) -> Self` — `egui_extras-0.35.0/src/syntax_highlighting.rs:277`
  Load code theme from egui memory.
- `fn from_style(style: &Style) -> Self` — `egui_extras-0.35.0/src/syntax_highlighting.rs:237`
  Selects either dark or light theme based on the given style.
- `fn is_dark(&self) -> bool` — `egui_extras-0.35.0/src/syntax_highlighting.rs:232`
- `fn light(font_size: f32) -> Self` — `egui_extras-0.35.0/src/syntax_highlighting.rs:270`
  ### Example
- `fn store_in_memory(self, ctx: &Context)` — `egui_extras-0.35.0/src/syntax_highlighting.rs:306`
  Store theme to egui memory.
- `fn ui(&mut self, ui: &mut Ui)` — `egui_extras-0.35.0/src/syntax_highlighting.rs:414`
  Show UI for changing the color theme.

Implements: `Clone`, `Default`, `Deserialize<'de>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`


## `egui_extras::loaders::file_loader`

### `FileLoader` (struct) — `egui_extras-0.35.0/src/loaders/file_loader.rs:17`

Implements: `BytesLoader`, `Default`


