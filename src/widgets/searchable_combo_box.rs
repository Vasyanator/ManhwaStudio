/*
File: src/widgets/searchable_combo_box.rs

Purpose:
A combo box whose drop-down rows carry TWO texts — a main line that may be drawn in the
row's own typeface and a smaller grey second line in the interface font — and whose popup can
be put into a SEARCH MODE that filters the list, colouring every matched character. The second
line goes either UNDER the main one (`RowLayout::Tall`, two lines per row) or AFTER it on the
same line (`RowLayout::Wide`, one line per row, so far more rows fit the popup).

Main responsibilities:
- draw a combo-box button whose caption is the selected row's main line, in that row's own
  font family (the same in both row layouts: the button never shows the second line), plus the
  SQUARE search button that follows it — the two share the width the caller asks for;
- open a popup that is JUST THE LIST, and reveal a search field above it on either of two
  triggers: the user typed into the open popup, or pressed the search button (which opens the
  popup too, straight into search mode). The row takes its own space, so the list is pushed
  down and never covered;
- filter the item list by a case-insensitive SUBSTRING of either line and highlight every
  occurrence in both;
- keep the drop-down keyboard-drivable (Escape / ArrowUp / ArrowDown / Enter — `Escape` first
  leaves search mode, then closes) and wheel-correct (the wheel over either closed button
  cycles the selection; an open popup publishes the shared wheel guard so widgets underneath
  it do not react);
- let a caller mark INDIVIDUAL rows: an optional colour for the main line's unmatched
  characters and an optional already-localized hover tooltip, both off by default;
- stay virtualization-friendly: within one layout every row is exactly as tall as every
  other row, and the caller's font resolver is invoked only for the rows actually drawn this
  frame.

Key structures:
- `SearchableComboItem`: one row — a main line, an optional second line, and the two optional
  per-row marks (main-line colour, hover tooltip).
- `RowLayout`: where that second line goes, and therefore how tall a row is.
- `SearchableComboBox`: the per-frame builder.
- `SearchableComboResponse`: the button response, "the selection changed", and the row the
  popup committed to this frame (which a re-affirming click reports and `changed` cannot).
- `PopupState` (private): the popup's own temporary state (search mode, query, highlight,
  scrolling).
- `EscapeAction` / `SearchButtonAction` (private, pure, unit-tested): what `Escape` and the
  square button do, decided away from the drawing code.
- `RowGeometry` (private): every length one frame's rows are laid out from.
- `InterfaceMetrics` (private): the interface font's ascent and line box at one size.

Key functions:
- `SearchableComboBox::show`: the whole frame.
- `SearchableComboBox::search_button_overhang`: what a caller must add to its width budget for
  the square button, asked instead of re-derived.
- `take_typed_text` (private, pure, unit-tested): moves the characters that SUMMONED the
  search row out of the event queue and into the query, which is what keeps the first
  keystroke from being lost to a field that did not exist when it was typed.
- `draw_search_button` / `paint_search_icon` (private): the square magnifier button. The
  drop-down arrow is NOT its business — it stays inside the combo button, drawn by
  `paint_dropdown_icon`.
- `layout_line` (private): one line as a single elided text row with coloured matches.
- `RowBaselines::measure` / `paint_line_on_baseline` (private): every galley of every row is
  positioned by a BASELINE this widget computes, never by its top edge.
- `matching::match_ranges` / `matching::item_matches` (private, GUI-free, unit-tested):
  the search predicate and the byte ranges it highlights.
- `reveal_offset` (private, pure, unit-tested): the scroll offset that brings a row into
  view with the least movement.

A row's lines are separate galleys the widget places by hand, in BOTH layouts. Every baseline
comes from the INTERFACE font's metrics at the line's nominal size and never from the face a
row happens to draw its main line in: epaint puts a galley's baseline at its own face's ascent
(15 pt for the interface font against 24 pt for a display face, both at a nominal 16 pt), so
painting galleys at their bands' top edges made the ink of a font catalog float up and down
from row to row — which reads as uneven row heights — and dropped a `Wide` row's second line
below its first. `Tall` gives each line a band of its own and a baseline inside it; `Wide` has
one text row, one baseline, and starts the second line after the main line's advance width
plus the gap.

The per-row marks are the caller's diagnostics and stay out of the widget's own decisions.
The colour reaches the main line's UNMATCHED characters only — the search highlight still wins
on the matched ones, so a coloured row keeps showing where the query hit — and it never touches
the fill drawn behind the keyboard cursor's row or behind the current selection, which is what
keeps a coloured row recognisable as the selected one. The tooltip is shown verbatim: the
widget neither translates it nor owns it.

Notes:
The widget knows nothing about fonts, font catalogs or font entries: a caller that wants
own-typeface rows lends it a resolver `usize -> Option<egui::FontFamily>` and stays
responsible for registering those families (`widgets::font_preview`). The resolver is called
ONLY for the rows drawn this frame plus the selected row on the button — egui's `add_font`
never evicts, so resolving every filtered row every frame would grow the font atlas without
bound while the user scrolls a large catalog.

The popup's list asks for its height explicitly every frame (`Ui::set_max_height` in
`show_popup`), because an `Area` hands its body LAST frame's content size as this frame's
`max_rect`: without that the list could shrink with a query and never grow back within one
opening. The full mechanism and the rejected alternatives are documented at that call site.

The popup's open flag is a plain `bool` this widget owns, never `egui::ComboBox::is_open`:
that helper needs the id egui derived internally, and passing an already-salted id to a
combo box makes it answer `false` forever (`egui-0.35.0/src/containers/combo_box.rs:232`
re-salts what it is given).
*/
use super::autocomplete_line::ignore_case_prefix_len;
use super::wheel_input_guard::{
    cycle_wrapped_index, publish_combo_popup_open, publish_combo_popup_rect, wheel_steps_if_hovered,
};
use eframe::egui;
use egui::text::{ByteIndex, LayoutJob, LayoutSection, TextFormat, TextWrapping};
use egui::{
    Align2, Color32, FontFamily, FontId, Galley, Id, Key, Modifiers, PopupCloseBehavior, Rect,
    Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType,
};
use std::ops::Range;
use std::sync::Arc;

/// Size of a row's second line, as a fraction of the main line's size.
///
/// The second line is a subordinate identifier (the font call site shows a PostScript name
/// under a display name), so it is drawn at EXACTLY half the main line's size at every
/// `primary_size`, with no lower bound: a floor would make the two lines nearly the same size
/// as soon as the main line got small, which is the one thing the second line must never look
/// like. A caller that cannot read the result should raise `primary_size`.
const SECONDARY_SIZE_FACTOR: f32 = 0.5;

/// Vertical headroom factor for the main line.
///
/// The main line is drawn in the ROW's own face, whose intrinsic line height can exceed its
/// nominal size by a wide margin. The same factor is used by the settings font picker
/// (`tabs::settings::typesetting::font_settings::PREVIEW_ROW_HEIGHT_FACTOR`) for the same
/// reason, and it is what keeps the row's reserved height independent of which face lands
/// in it — the property `ScrollArea::show_rows` virtualization depends on.
const PRIMARY_LINE_HEIGHT_FACTOR: f32 = 1.6;

/// Vertical headroom factor for the second line. Smaller than the main line's, because the
/// second line is always drawn in the interface font, whose metrics are known.
const SECONDARY_LINE_HEIGHT_FACTOR: f32 = 1.25;

/// Gap between a `Wide` row's main line and the second line that trails it, as a fraction of
/// the main line's size.
///
/// Proportional to the main line rather than a fixed number of points, because the two texts
/// it separates both scale with `primary_size`: a fixed gap would read as a wide margin at
/// small sizes and as a missing separator at large ones.
const WIDE_SECONDARY_GAP_FACTOR: f32 = 0.5;

/// Padding inside a popup row, in points: horizontal on both sides, vertical top and bottom.
const ROW_HORIZONTAL_PADDING: f32 = 4.0;
const ROW_VERTICAL_PADDING: f32 = 2.0;

/// Default size of a row's main line, in points.
const DEFAULT_PRIMARY_SIZE: f32 = 14.0;

/// Default cap on the drop-down list's height, in points.
const DEFAULT_MAX_POPUP_HEIGHT: f32 = 320.0;

/// Colour of the matched characters on a DARK row background.
///
/// Deliberately not `Visuals::hyperlink_color`: in the light theme that colour
/// (`rgb(0, 155, 255)`) sits on the light-theme selection fill (`rgb(144, 209, 255)`) at a
/// contrast ratio of about 1.8:1, i.e. it would read WORSE than the plain row text (about
/// 6.7:1) — the highlight would disappear on exactly the row the user is aiming at. The two
/// constants below are picked per background instead (see [`match_highlight_color`]) and
/// keep the highlight at roughly 4:1 or better in every combination this widget paints.
const HIGHLIGHT_ON_DARK: Color32 = Color32::from_rgb(120, 190, 255);

/// Colour of the matched characters on a LIGHT row background. See [`HIGHLIGHT_ON_DARK`].
const HIGHLIGHT_ON_LIGHT: Color32 = Color32::from_rgb(0, 90, 190);

/// Where a row's second line goes — and therefore how tall a row is.
///
/// The two variants are the user-facing «высокий» (`Tall`) and «широкий» (`Wide`) modes.
/// Both draw exactly the same texts and highlight the same matches; they differ only in
/// where the second line is put and, as a consequence, in how many rows fit the popup.
///
/// Row height stays UNIFORM inside one layout — see [`SearchableComboBox`]'s contract — but
/// it differs BETWEEN layouts, which is the whole point of the switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowLayout {
    /// «Высокий»: the second line sits UNDER the main one, in a band of its own, so a row is
    /// two lines high and reserves the second line's height for every row as soon as ANY row
    /// carries one. Each line is placed on its band's own baseline, so a decorative main face
    /// cannot shift the text up or down relative to its neighbours.
    ///
    /// The default, so a caller that does not choose keeps the taller, roomier rows.
    #[default]
    Tall,
    /// «Широкий»: the second line follows the main one ON THE SAME LINE — after the main
    /// line's advance width plus a gap of [`WIDE_SECONDARY_GAP_FACTOR`] of the main line's
    /// size, and on the SAME baseline, so the two texts sit level along the bottom despite
    /// their different sizes and faces. A row is exactly one main line high whatever the
    /// items carry, so a popup of a given height shows far more rows.
    ///
    /// Long content is elided at the row's wrap width and clipped by the row's clip rect;
    /// nothing reflows onto a second line, and a face whose ink overshoots the row is
    /// clipped rather than allowed to grow it.
    Wide,
}

/// One row of a [`SearchableComboBox`].
///
/// `primary` is the row's main line and the text the closed button shows for the selected
/// row; `secondary`, when present, is drawn at [`SECONDARY_SIZE_FACTOR`] of the main line's
/// size in the interface font — underneath the main line in [`RowLayout::Tall`], after it on
/// the same line in [`RowLayout::Wide`]. Both lines are searched in both layouts.
///
/// Whether a row HAS a second line is the caller's decision. In [`RowLayout::Tall`] it should
/// be uniform across the list: rows reserve space for a second line as soon as ANY row has
/// one, so a mixed list simply leaves that space empty on the rows without one (which is what
/// keeps every row the same height). [`RowLayout::Wide`] reserves nothing and is indifferent
/// to a mixed list.
///
/// Two per-row decorations are optional and OFF by default, so a row built by [`Self::new`]
/// or [`Self::with_secondary`] looks exactly as it did before they existed: a colour for the
/// main line ([`Self::primary_color`]) and a hover tooltip ([`Self::tooltip`]). Both are
/// meant for per-row diagnostics — the typing tab's font list colours a face by how well it
/// covers the typesetting language and explains the colour on hover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchableComboItem<'a> {
    /// The main line, drawn in the row's own font family when the caller resolves one.
    pub primary: &'a str,
    /// The optional smaller second line, always drawn in the interface font.
    pub secondary: Option<&'a str>,
    /// Colour of the main line's UNMATCHED characters, replacing the colour this row's state
    /// (plain / current selection / keyboard cursor) would give them. `None` — the default —
    /// keeps that state colour and changes nothing at all.
    ///
    /// Three limits, all deliberate. The SEARCH HIGHLIGHT still wins on the characters the
    /// query matched, so a coloured row keeps showing WHERE it matched. The second line is
    /// never affected: it stays `Visuals::weak_text_color` whatever this field says. And the
    /// colour replaces TEXT only — the fill painted behind the keyboard cursor's row and
    /// behind the current selection is untouched, so a coloured row still reads as the
    /// selected one.
    pub primary_color: Option<Color32>,
    /// Tooltip shown while the pointer is over this row; `None` — the default — and an empty
    /// string show none.
    ///
    /// The caller passes ALREADY-LOCALIZED text: this widget never translates it and never
    /// owns it. Like both of the row's lines it is borrowed for the frame, so a caller that
    /// builds the text per frame must keep it alive until [`SearchableComboBox::show`]
    /// returns.
    pub tooltip: Option<&'a str>,
}

impl<'a> SearchableComboItem<'a> {
    /// A row with only a main line, no colour of its own and no tooltip.
    #[must_use]
    pub fn new(primary: &'a str) -> Self {
        Self {
            primary,
            secondary: None,
            primary_color: None,
            tooltip: None,
        }
    }

    /// A row with a main line and a smaller second line under it, no colour of its own and no
    /// tooltip.
    #[must_use]
    pub fn with_secondary(primary: &'a str, secondary: &'a str) -> Self {
        Self {
            primary,
            secondary: Some(secondary),
            primary_color: None,
            tooltip: None,
        }
    }

    /// Draws this row's main line in `color` wherever the query did NOT match.
    ///
    /// The matched characters keep the search highlight and the second line keeps its grey;
    /// see [`Self::primary_color`] for the full contract.
    #[must_use]
    #[inline]
    pub fn primary_color(mut self, color: Color32) -> Self {
        self.primary_color = Some(color);
        self
    }

    /// Shows `tooltip` while the pointer is over this row.
    ///
    /// Pass an ALREADY-LOCALIZED string; an empty one shows nothing. See [`Self::tooltip`].
    #[must_use]
    #[inline]
    pub fn tooltip(mut self, tooltip: &'a str) -> Self {
        self.tooltip = Some(tooltip);
        self
    }
}

/// What one frame of a [`SearchableComboBox`] reports back.
#[derive(Debug)]
pub struct SearchableComboResponse {
    /// The closed button's response. It is marked changed whenever `changed` is `true`.
    pub response: Response,
    /// Whether the selected index was written this frame (by a click, `Enter`, or the wheel).
    pub changed: bool,
    /// The row the user COMMITTED to in the popup this frame — a click on a row, or `Enter`
    /// on the keyboard cursor — as an index into `items`. `None` for a wheel step (which
    /// `changed` reports) and for a frame with no input.
    ///
    /// Reported EVEN WHEN it equals the current selection, which is the one thing `changed`
    /// cannot express: clicking the row that is already selected writes nothing, yet it is a
    /// deliberate user act. The typing tab's font combo depends on exactly that distinction —
    /// it is the only way to pin a `<font>` tag on a span whose effective font already equals
    /// the shown row (`tabs::typing::panel::create_main_text::font_combo_user_pick`).
    pub picked: Option<usize>,
}

/// Resolver handed to [`SearchableComboBox::item_font`].
///
/// Answers "which egui font family should row `index`'s main line be drawn in?"; `None`
/// means the interface font. Registering the family is the caller's job
/// (`widgets::font_preview::request_font_family`), and so is deciding how many distinct
/// families it is willing to register — egui never evicts one.
type ItemFontResolver<'a> = &'a mut dyn FnMut(usize) -> Option<FontFamily>;

/// A short-lived reborrow of the caller's [`ItemFontResolver`] for one drawing pass.
///
/// The two lifetimes must stay separate: `'r` is how long this pass holds the resolver and
/// `'a` is how long the closure itself lives. Collapsing them into one (by reusing
/// `Option<ItemFontResolver<'_>>` in a helper's signature) makes every reborrow last as long
/// as the closure, so the button and the popup could not both use it in one frame.
type ItemFontRef<'r, 'a> = Option<&'r mut (dyn FnMut(usize) -> Option<FontFamily> + 'a)>;

/// Reborrows a resolver so several drawing passes of one frame can each use it.
///
/// `Option::as_deref_mut` expresses exactly this, but clippy reads it as a no-op deref
/// (`needless_option_as_deref`) because the result differs from the input only in lifetime.
/// The reborrow is not a no-op: `Option<&mut _>` is not `Copy`, so without it the button
/// would MOVE the resolver and the popup would have none left.
fn reborrow_resolver<'r, 'a>(item_font: &'r mut ItemFontRef<'_, 'a>) -> ItemFontRef<'r, 'a> {
    item_font.as_mut().map(|resolve| &mut **resolve)
}

/// A combo box with two-line rows, per-row typefaces, and an on-demand search field.
///
/// Build it, then call [`Self::show`]. All of the widget's own state (whether the search row
/// is up, the query, the highlighted row, the scroll position) is temporary `egui::Context`
/// state keyed off the widget's id and dropped when the popup closes — the caller owns
/// nothing but the selected index.
///
/// The widget draws TWO controls side by side: the combo button (caption plus the drop-down
/// arrow inside it) and a square search button after it. [`Self::width`] is the width of both
/// together, and the popup is exactly that wide.
///
/// # Contracts
/// - The search field is a MODE, off when the popup opens. It appears when the user types
///   into the open popup or presses the square button, takes its own space above the list,
///   and is dropped — together with the query — by `Escape` or by pressing the button again.
/// - The font resolver is invoked ONLY for the rows drawn this frame plus the selected row
///   on the closed button. Do not rely on it being called for every item.
/// - Every popup row is exactly as tall as every other one, including rows without a second
///   line. This is what makes `ScrollArea::show_rows` virtualization valid here. The height
///   itself depends on the chosen [`RowLayout`] — two lines in `Tall`, one in `Wide` — but
///   never on the individual row, and never on the face a row is drawn in: a face whose ink
///   overshoots the row is clipped, not accommodated.
/// - Every line is painted on a baseline derived from the INTERFACE font at that line's
///   nominal size, identical for every row of the list. The item's own face decides what the
///   main line looks like and how wide it is — never where it sits. In [`RowLayout::Wide`]
///   both of a row's texts share that one baseline.
/// - While the popup is open the widget publishes the shared wheel guard
///   (`widgets::wheel_input_guard`) every frame and does NOT react to the wheel itself.
pub struct SearchableComboBox<'a> {
    id_salt: Id,
    width: Option<f32>,
    max_popup_height: f32,
    primary_size: f32,
    row_layout: RowLayout,
    selected_text: Option<String>,
    item_font: Option<ItemFontResolver<'a>>,
}

impl std::fmt::Debug for SearchableComboBox<'_> {
    /// Prints the builder's settings; the font resolver is a closure and is elided.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchableComboBox")
            .field("id_salt", &self.id_salt)
            .field("width", &self.width)
            .field("max_popup_height", &self.max_popup_height)
            .field("primary_size", &self.primary_size)
            .field("row_layout", &self.row_layout)
            .field("selected_text", &self.selected_text)
            .field("item_font", &self.item_font.is_some())
            .finish()
    }
}

impl<'a> SearchableComboBox<'a> {
    /// A combo box identified by `id_salt`.
    ///
    /// `id_salt` must be a STABLE literal, never a localized caption: it seeds the widget id
    /// under which the popup state lives, so a translated salt would drop that state on a
    /// language switch (`egui-docs/05-ids-and-i18n.md` §2).
    #[must_use]
    pub fn new(id_salt: impl std::hash::Hash + std::fmt::Debug) -> Self {
        Self {
            id_salt: Id::new(id_salt),
            width: None,
            max_popup_height: DEFAULT_MAX_POPUP_HEIGHT,
            primary_size: DEFAULT_PRIMARY_SIZE,
            row_layout: RowLayout::default(),
            selected_text: None,
            item_font: None,
        }
    }

    /// Total width of the widget, in points: the combo button, the gap after it, and the
    /// square search button. Defaults to `Spacing::combo_width`.
    ///
    /// NOTE that this is the width of BOTH controls together — the combo button gets what is
    /// left after the search button and the gap. Ask
    /// [`Self::search_button_overhang`] for that remainder instead of re-deriving it, the way
    /// `widgets::ai_button::marker_badge_overhang` is asked. The popup is exactly this wide.
    #[must_use]
    #[inline]
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    /// Width, in points, that [`Self::width`] carries ON TOP of the combo button itself: the
    /// gap plus the square search button.
    ///
    /// `primary_size` must be the value the caller passes to [`Self::primary_size`], because
    /// the button row's height — and therefore the square's side — follows the main line.
    /// A caller sizing a row asks this instead of re-deriving the button's geometry.
    #[must_use]
    pub fn search_button_overhang(ui: &Ui, primary_size: f32) -> f32 {
        ui.spacing().item_spacing.x + button_row_height(ui, primary_line_height(primary_size))
    }

    /// Upper bound on the drop-down list's height, in points (the search field sits above
    /// it and is not counted).
    #[must_use]
    #[inline]
    pub fn max_popup_height(mut self, max_popup_height: f32) -> Self {
        self.max_popup_height = max_popup_height.max(1.0);
        self
    }

    /// Point size of a row's main line. The second line is always exactly half of it
    /// ([`SECONDARY_SIZE_FACTOR`]), with no lower bound, so a small `primary_size` makes the
    /// second line small too.
    #[must_use]
    #[inline]
    pub fn primary_size(mut self, primary_size: f32) -> Self {
        self.primary_size = primary_size.max(1.0);
        self
    }

    /// Where a row's second line goes: under the main line ([`RowLayout::Tall`], the
    /// default) or after it on the same line ([`RowLayout::Wide`]).
    ///
    /// The choice changes the popup's row height and nothing else: the closed button, the
    /// filtering, and the highlighting are identical in both.
    #[must_use]
    #[inline]
    pub fn row_layout(mut self, row_layout: RowLayout) -> Self {
        self.row_layout = row_layout;
        self
    }

    /// Overrides the closed button's caption with plain interface-font text.
    ///
    /// Without it the button shows the selected row's main line in that row's own family.
    /// Use it when the button has to say something that is not a row — "font not found",
    /// for instance.
    #[must_use]
    #[inline]
    pub fn selected_text(mut self, selected_text: impl Into<String>) -> Self {
        self.selected_text = Some(selected_text.into());
        self
    }

    /// Lends the widget a per-row font-family resolver; see [`ItemFontResolver`].
    ///
    /// It is called only for the rows drawn this frame plus the selected row on the closed
    /// button, so a caller may do real work in it (registering a preview font, for example)
    /// without paying for the whole list.
    #[must_use]
    #[inline]
    pub fn item_font(mut self, item_font: ItemFontResolver<'a>) -> Self {
        self.item_font = Some(item_font);
        self
    }

    /// Draws the widget for this frame — combo button, square search button, and the popup
    /// while it is open — and writes the picked row into `selected`.
    ///
    /// `selected` is an index into `items`. An out-of-range index is CLAMPED for drawing but
    /// never written back, so the caller's value survives a transient list rebuild; an empty
    /// `items` draws an empty button and a "nothing found" popup. Neither panics.
    ///
    /// Returns the button's response, whether `selected` was written this frame, and which
    /// row the popup committed to (see [`SearchableComboResponse::picked`]).
    #[must_use]
    pub fn show(
        self,
        ui: &mut Ui,
        selected: &mut usize,
        items: &[SearchableComboItem<'_>],
    ) -> SearchableComboResponse {
        let Self {
            id_salt,
            width,
            max_popup_height,
            primary_size,
            row_layout,
            selected_text,
            mut item_font,
        } = self;

        let ctx = ui.ctx().clone();
        let widget_id = ui.make_persistent_id(id_salt);
        let open_id = widget_id.with("open");
        let popup_id = widget_id.with("popup");
        let search_id = widget_id.with("search");
        let search_button_id = widget_id.with("search_button");
        let state_id = widget_id.with("state");

        let mut open = ctx
            .data(|data| data.get_temp::<bool>(open_id))
            .unwrap_or(false);
        let mut state = if open {
            ctx.data(|data| data.get_temp::<PopupState>(state_id))
                .unwrap_or_default()
        } else {
            PopupState::default()
        };

        let geometry = RowGeometry::new(primary_size, row_layout, items);
        // Clamp only for DRAWING: writing a clamped index back would report a change the
        // user never made, and a caller that rebuilds its list every frame would see its
        // selection silently rewritten mid-rebuild.
        let shown_index = (!items.is_empty()).then(|| (*selected).min(items.len() - 1));

        // `.width(..)` is the width of the WHOLE widget — combo button, gap, square search
        // button — so the layout is decided here and handed to both. The search button is
        // square by construction: its side IS the row height, which is also the combo
        // button's.
        let total_width = width.unwrap_or_else(|| ui.spacing().combo_width);
        let button_height = button_row_height(ui, geometry.primary_line_height);
        let gap = ui.spacing().item_spacing.x;
        let combo_width = (total_width - gap - button_height).max(0.0);

        let (button_response, search_response) = ui
            .horizontal(|ui| {
                let button_response = draw_button(
                    ui,
                    widget_id,
                    &ButtonSpec {
                        width: combo_width,
                        height: button_height,
                        primary_size,
                        line_height: geometry.primary_line_height,
                        open,
                        caption: selected_text.as_deref(),
                        shown_index,
                    },
                    items,
                    reborrow_resolver(&mut item_font),
                );
                // Drawn from the state as it stands BEFORE this frame's click is read: a
                // toggle that lights up one frame later is what every egui button does, and
                // any click repaints immediately anyway.
                let search_response = draw_search_button(
                    ui,
                    search_button_id,
                    button_height,
                    open && state.search_active,
                );
                (button_response, search_response)
            })
            .inner;

        let mut changed = false;
        // The popup's commit for this frame, reported separately from `changed` because a
        // click on the already-selected row writes nothing and still counts as a pick.
        let mut picked = None;
        let was_open = open;
        if button_response.clicked() {
            open = !open;
            if open {
                // A fresh popup: no search row, no query, the cursor on the selected row, and
                // a scroll that brings that row into view on the first frame. With an empty
                // query the filtered list IS the item list, so the row's position equals its
                // index.
                state = fresh_popup_state(shown_index, false);
            }
        }
        // The square button is a TOGGLE that also opens: pressed on a closed list it opens it
        // with the search row already up and focused.
        let mut keep_open_after_search_click = false;
        if search_response.clicked() {
            match search_button_action(open, state.search_active) {
                SearchButtonAction::Open => {
                    open = true;
                    state = fresh_popup_state(shown_index, true);
                }
                SearchButtonAction::Enable => {
                    state.search_active = true;
                    state.focus_search = true;
                    keep_open_after_search_click = true;
                }
                SearchButtonAction::Disable => {
                    // Leaving search mode drops the query, so the list is unfiltered again —
                    // the same reset `Escape` performs.
                    state.search_active = false;
                    state.focus_search = false;
                    state.query.clear();
                    state.highlighted = None;
                    state.reveal = Some(0);
                    keep_open_after_search_click = true;
                }
            }
        }
        if open && !was_open {
            // The drop-down owns the keyboard from the moment it appears. Whatever held focus
            // until now — the typing tab's own text editor, most often — would otherwise keep
            // swallowing `Event::Text`, and the keystroke meant to summon this popup's search
            // row would land in that editor instead (`egui-0.35.0/src/memory/mod.rs:985`).
            ctx.memory_mut(|memory| memory.stop_text_input());
        }

        if open {
            // Tell the wheel-aware widgets underneath the list to sit still, every frame the
            // list is up (`widgets::wheel_input_guard`).
            publish_combo_popup_open(&ctx);
            let body = PopupBody {
                popup_id,
                search_id,
                // The popup spans BOTH buttons, not just the combo one.
                popup_width: total_width,
                max_popup_height,
                primary_size,
                geometry,
            };
            let outcome = show_popup(
                &button_response,
                &body,
                &mut open,
                &mut state,
                items,
                // The CLAMPED index, the same one the closed button draws: passing the raw
                // one would leave an out-of-range selection marked on no row at all while
                // the button showed the last one.
                shown_index,
                reborrow_resolver(&mut item_font),
            );
            if keep_open_after_search_click {
                // `PopupCloseBehavior::CloseOnClickOutside` counts the search button as
                // "outside": it is this widget's, but it is not inside the popup's `Area`, so
                // `Popup::show` has just written `false` into `open`
                // (`egui-0.35.0/src/containers/popup.rs:598-599` then `:621-624`). The click
                // that toggles the search row is never a dismissal, so take that back — a
                // pick or an `Escape` below still closes.
                open = true;
            }
            if let Some(index) = outcome.picked {
                picked = Some(index);
                if *selected != index {
                    *selected = index;
                    changed = true;
                }
                open = false;
            }
            if outcome.close_requested {
                open = false;
            }
        } else {
            // The wheel cycles the selection over EITHER button: the square one sits inside
            // the width the caller budgeted for this widget, so a notch over it must move a
            // row rather than scroll whatever is behind the widget.
            let wheel_target = button_response.clone().union(search_response);
            if let Some(steps) = wheel_steps_if_hovered(&ctx, &wheel_target) {
                let previous = *selected;
                *selected = cycle_wrapped_index(previous, items.len(), steps);
                changed |= *selected != previous;
            }
        }

        ctx.data_mut(|data| {
            data.insert_temp(open_id, open);
            if open {
                data.insert_temp(state_id, state);
            } else {
                // The popup's state is scoped to one opening: dropping it is what makes the
                // next opening start with an empty query and the cursor on the selection.
                data.remove::<PopupState>(state_id);
            }
        });

        let mut response = button_response;
        if changed {
            response.mark_changed();
        }
        SearchableComboResponse {
            response,
            changed,
            picked,
        }
    }
}

/// The popup's own state for one opening, stored as temporary `Context` data.
#[derive(Clone, Debug, Default)]
struct PopupState {
    /// Whether the SEARCH ROW is showing. `false` — the default — is a popup that is just a
    /// list: the row appears only when the user types into the open popup or presses the
    /// search button, and it takes its own space ABOVE the list rather than over it.
    search_active: bool,
    /// The search query. Empty matches every row. Always empty while `search_active` is
    /// `false`: leaving search mode drops the query, so the list is unfiltered again.
    query: String,
    /// Index INTO THE ITEM LIST of the row the keyboard cursor is on, if any. Stored as an
    /// item index rather than a position in the filtered list so it survives edits to the
    /// query without pointing at a different row.
    highlighted: Option<usize>,
    /// Set on the frame the search row APPEARS, so the field is focused before it has ever
    /// been drawn. Focus is re-requested on any later frame nothing holds it (a click anywhere
    /// else in the popup surrenders it), so this flag only covers the first.
    focus_search: bool,
    /// A position in the FILTERED list that should be scrolled into view this frame.
    reveal: Option<usize>,
    /// Last frame's vertical scroll offset of the list, in points.
    scroll_offset: f32,
    /// Last frame's visible height of the list, in points. `0.0` before the first frame.
    viewport_height: f32,
}

/// Everything the row layout of one frame is derived from, computed once per frame.
///
/// Row height is UNIFORM across the whole list, and in [`RowLayout::Tall`] the space for a
/// second line is reserved for EVERY row as soon as any row has one. Both properties are
/// load-bearing: `ScrollArea::show_rows` positions rows by multiplying one height by an
/// index, so a per-row height would misplace every row after the first odd one, and
/// reserving the second line conditionally would make the list jump as the filter changes
/// which rows are shown.
///
/// [`RowLayout::Wide`] reserves NO second-line height at all — the second line shares the
/// main line's text row — so a `Wide` row is one main line plus the vertical padding
/// whatever the items carry.
#[derive(Clone, Copy, Debug)]
struct RowGeometry {
    /// Which of the two row layouts every length below was derived for.
    layout: RowLayout,
    primary_line_height: f32,
    secondary_size: f32,
    /// Height reserved UNDER the main line for the second one; always `0.0` in
    /// [`RowLayout::Wide`].
    secondary_line_height: f32,
    /// Horizontal gap before the trailing second line; always `0.0` in [`RowLayout::Tall`],
    /// where the second line starts a row of its own.
    secondary_gap: f32,
    row_height: f32,
}

impl RowGeometry {
    /// Derives the frame's row geometry from the main line's size, the row layout, and — in
    /// [`RowLayout::Tall`] only — whether any row has a second line.
    ///
    /// `items` is scanned solely for that `Tall` reservation; `Wide` ignores it, which is
    /// exactly why a `Wide` row's height cannot depend on the list's content.
    fn new(primary_size: f32, layout: RowLayout, items: &[SearchableComboItem<'_>]) -> Self {
        let primary_line_height = primary_line_height(primary_size);
        let secondary_size = primary_size * SECONDARY_SIZE_FACTOR;
        let (secondary_line_height, secondary_gap) = match layout {
            RowLayout::Tall => {
                let has_secondary = items.iter().any(|item| item.secondary.is_some());
                let line_height = if has_secondary {
                    secondary_galley_line_height(secondary_size)
                } else {
                    0.0
                };
                (line_height, 0.0)
            }
            // The second line lives inside the main line's text row here, so it adds height
            // to nothing and needs a horizontal gap instead.
            RowLayout::Wide => (0.0, primary_size * WIDE_SECONDARY_GAP_FACTOR),
        };
        Self {
            layout,
            primary_line_height,
            secondary_size,
            secondary_line_height,
            secondary_gap,
            row_height: primary_line_height + secondary_line_height + 2.0 * ROW_VERTICAL_PADDING,
        }
    }
}

/// The closed button's drawing parameters for one frame.
///
/// `width` and `height` are decided by [`SearchableComboBox::show`], not here: the button
/// shares its row with the square search button, and the two must add up to the width the
/// caller asked for and stand exactly as tall as each other.
#[derive(Clone, Copy, Debug)]
struct ButtonSpec<'a> {
    width: f32,
    height: f32,
    primary_size: f32,
    line_height: f32,
    open: bool,
    /// Interface-font caption that replaces the selected row's own-typeface main line.
    caption: Option<&'a str>,
    /// Index of the row whose main line the button shows, already clamped into range.
    shown_index: Option<usize>,
}

/// The popup's per-frame drawing parameters.
#[derive(Clone, Copy, Debug)]
struct PopupBody {
    popup_id: Id,
    search_id: Id,
    /// Width of the whole widget — combo button + gap + square search button — which is what
    /// the popup spans, not the combo button's own width.
    popup_width: f32,
    max_popup_height: f32,
    primary_size: f32,
    geometry: RowGeometry,
}

/// What one frame of the popup body decided.
#[derive(Clone, Copy, Debug, Default)]
struct PopupOutcome {
    /// The row the user committed to, if any.
    picked: Option<usize>,
    /// Whether the popup asked to be closed (Escape on an empty query, or a pick).
    close_requested: bool,
}

/// The state a freshly opened popup starts from.
///
/// `search` decides whether it opens as a PLAIN LIST (the combo button's own click) or
/// straight into search mode (the square button's). Either way the keyboard cursor starts on
/// the selected row and a reveal is queued so that row is on screen on the very first frame;
/// with an empty query the filtered list IS the item list, so the row's position equals its
/// index.
fn fresh_popup_state(shown_index: Option<usize>, search: bool) -> PopupState {
    PopupState {
        search_active: search,
        focus_search: search,
        highlighted: shown_index,
        reveal: Some(shown_index.unwrap_or(0)),
        ..PopupState::default()
    }
}

/// The main line's reserved height, in points, at a given `primary_size`.
///
/// Extracted so the row geometry and the width budget a caller asks for
/// ([`SearchableComboBox::search_button_overhang`]) cannot drift apart.
fn primary_line_height(primary_size: f32) -> f32 {
    (primary_size * PRIMARY_LINE_HEIGHT_FACTOR).max(1.0)
}

/// Height of the widget's button row in points — and therefore the SIDE of its square search
/// button, which is what makes the two buttons stand exactly as tall as each other.
///
/// The same three-way maximum egui's own combo box takes for its button
/// (`egui-0.35.0/src/containers/combo_box.rs:361-362`): the caption's line box, the drop-down
/// icon, and the style's minimum interactive height.
fn button_row_height(ui: &Ui, line_height: f32) -> f32 {
    line_height
        .max(ui.spacing().icon_width)
        .max(ui.spacing().interact_size.y)
}

/// Draws the square search button that follows the combo button, and returns its response.
///
/// `size` is both its width and its height. `active` says whether the search row is currently
/// showing, which the button reflects with the same "open" visuals the combo button wears
/// while its popup is up. The hover tooltip is localized here: the button belongs to the
/// widget, not to the caller.
fn draw_search_button(ui: &mut Ui, id: Id, size: f32, active: bool) -> Response {
    let (_, rect) = ui.allocate_space(Vec2::splat(size));
    let response = ui.interact(rect, id, Sense::click());
    let enabled = ui.is_enabled();
    let tooltip = t!("widgets.searchable_combo_box.search_button_tooltip");
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, tooltip));
    let response = response.on_hover_text(tooltip);
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let visuals = if active {
        ui.visuals().widgets.open
    } else {
        *ui.style().interact(&response)
    };
    ui.painter().rect(
        rect,
        visuals.corner_radius,
        visuals.weak_bg_fill,
        visuals.bg_stroke,
        StrokeKind::Inside,
    );
    paint_search_icon(ui.painter(), rect, visuals.fg_stroke.color);
    response
}

/// Paints a magnifier inside `rect`: a circle for the lens plus a short diagonal handle.
///
/// Painted rather than typeset as a glyph on purpose — the interface font stack is not
/// guaranteed to carry a magnifier code point, and a missing glyph would leave the button
/// blank with nothing in the type system to warn about it.
fn paint_search_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let side = rect.width().min(rect.height());
    let stroke = Stroke::new((side * 0.09).max(1.0), color);
    let radius = side * 0.24;
    // The lens sits slightly up and left of centre, which is what leaves room for its handle
    // inside the same square.
    let center = rect.center() - Vec2::splat(side * 0.07);
    painter.circle_stroke(center, radius, stroke);
    // The handle leaves the lens at 45 degrees, starting on the rim so the two meet.
    let start = center + Vec2::splat(radius * std::f32::consts::FRAC_1_SQRT_2);
    painter.line_segment([start, start + Vec2::splat(side * 0.18)], stroke);
}

/// Draws the closed combo-box button and returns its response.
///
/// Mirrors egui's own combo-box button (`egui-0.35.0/src/containers/combo_box.rs:320-390`):
/// a framed rect with a downward triangle on the right and the caption on the left, elided
/// rather than wrapped. The caption is drawn from an explicit galley instead of a
/// `WidgetText` so the selected row's OWN family can be used for it while the triangle stays
/// in the interface font's colour.
fn draw_button(
    ui: &mut Ui,
    widget_id: Id,
    spec: &ButtonSpec<'_>,
    items: &[SearchableComboItem<'_>],
    mut item_font: ItemFontRef<'_, '_>,
) -> Response {
    let icon_size = Vec2::splat(ui.spacing().icon_width);
    let icon_spacing = ui.spacing().icon_spacing;
    let margin = ui.spacing().button_padding;
    let desired_width = spec.width;
    let desired_height = spec.height;

    // Reserve the space, then interact separately, so the response carries the widget's own
    // stable id rather than an auto-generated one — the popup id is derived from it. This is
    // egui's own combo-box shape (`egui-0.35.0/src/containers/combo_box.rs:364-367`);
    // `allocate_exact_size` would register a second widget rect over the same area.
    let (_, rect) = ui.allocate_space(Vec2::new(desired_width, desired_height));
    let response = ui.interact(rect, widget_id, Sense::click());

    // Reported before any early return, so the control is never anonymous in the AccessKit
    // tree — that tree is what the project's `egui-mcp` inspection reads. Mirrors egui's own
    // combo box (`egui-0.35.0/src/containers/combo_box.rs:246-252`).
    let enabled = ui.is_enabled();
    response.widget_info(|| {
        let mut info = WidgetInfo::new(WidgetType::ComboBox);
        info.enabled = enabled;
        // The same caption the button paints below: the override, else the selected row's
        // main line, else nothing.
        let caption = spec.caption.or_else(|| {
            spec.shown_index
                .and_then(|index| items.get(index))
                .map(|item| item.primary)
        });
        info.current_text_value = Some(caption.unwrap_or_default().to_owned());
        info
    });

    if !ui.is_rect_visible(rect) {
        return response;
    }

    let visuals = if spec.open {
        ui.visuals().widgets.open
    } else {
        *ui.style().interact(&response)
    };
    ui.painter().rect(
        rect,
        visuals.corner_radius,
        visuals.weak_bg_fill,
        visuals.bg_stroke,
        StrokeKind::Inside,
    );

    let inner = rect.shrink2(margin);
    let icon_rect = Align2::RIGHT_CENTER.align_size_within_rect(icon_size, inner);
    paint_dropdown_icon(ui.painter(), icon_rect, visuals.fg_stroke.color);

    let text_width = (icon_rect.left() - icon_spacing - inner.left()).max(0.0);
    if text_width <= 0.0 {
        return response;
    }
    let text_color = visuals.text_color();
    let galley = match (spec.caption, spec.shown_index) {
        (Some(caption), _) => layout_line(
            ui,
            caption,
            FontId::new(spec.primary_size, FontFamily::Proportional),
            Some(spec.line_height),
            LineColors::plain(text_color),
            &[],
            text_width,
        ),
        (None, Some(index)) => {
            let Some(item) = items.get(index) else {
                return response;
            };
            // The ONLY resolver call the closed button makes: exactly the selected row.
            let family = reborrow_resolver(&mut item_font)
                .and_then(|resolve| resolve(index))
                .unwrap_or(FontFamily::Proportional);
            layout_line(
                ui,
                item.primary,
                FontId::new(spec.primary_size, family),
                Some(spec.line_height),
                LineColors::plain(text_color),
                &[],
                text_width,
            )
        }
        (None, None) => return response,
    };

    let text_rect = Rect::from_min_max(
        inner.min,
        egui::pos2(icon_rect.left() - icon_spacing, inner.max.y),
    );
    let pos = Align2::LEFT_CENTER
        .align_size_within_rect(galley.size(), text_rect)
        .min;
    ui.painter()
        .with_clip_rect(text_rect.intersect(ui.clip_rect()))
        .galley(pos, galley, text_color);
    response
}

/// Paints the downward triangle that marks a drop-down, filling `rect`.
///
/// Reproduces the geometry of egui's private `paint_default_icon`
/// (`egui-0.35.0/src/containers/combo_box.rs:472-487`) so this widget's button is
/// indistinguishable from a stock combo box's.
fn paint_dropdown_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let triangle = Rect::from_center_size(
        rect.center(),
        Vec2::new(rect.width() * 0.7, rect.height() * 0.45),
    );
    painter.add(Shape::convex_polygon(
        vec![
            triangle.left_top(),
            triangle.right_top(),
            triangle.center_bottom(),
        ],
        color,
        Stroke::NONE,
    ));
}

/// Draws the whole popup for one frame: search field, keyboard handling, and the row list.
///
/// `open` is handed to `egui::Popup` so that clicking outside closes the list; every other
/// close reason travels back through the returned [`PopupOutcome`], because the popup's own
/// body may not touch the flag egui borrowed.
///
/// The shared wheel guard's RECT is published after the body has run, since that is when the
/// popup's rect exists; the rect-less `publish_combo_popup_open` of the caller is what covers
/// the wheel for the whole frame.
///
/// Keyboard handling deliberately lives INSIDE the popup body. `Popup::show` runs the body
/// before its own unconditional `key_pressed(Escape)` close check
/// (`egui-0.35.0/src/containers/popup.rs:585` then `:605`), so consuming `Escape` here is
/// what lets the first `Escape` clear the query instead of closing the list. It is also the
/// only place where the filtered list is known, which the arrow keys need.
fn show_popup(
    button_response: &Response,
    body: &PopupBody,
    open: &mut bool,
    state: &mut PopupState,
    items: &[SearchableComboItem<'_>],
    selected: Option<usize>,
    mut item_font: ItemFontRef<'_, '_>,
) -> PopupOutcome {
    let popup_width = body.popup_width;
    let inner = egui::Popup::from_response(button_response)
        .id(body.popup_id)
        .open_bool(open)
        // `CloseOnClick` would close the list on the very first click on a row, before this
        // body ever sees it.
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .width(popup_width)
        .show(|ui| {
            ui.set_min_width(popup_width);

            let mut outcome = PopupOutcome::default();

            // Escape first: it can drop the query and the whole search row, and everything
            // below reads both.
            if ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape)) {
                match escape_action(state.search_active, state.query.is_empty()) {
                    EscapeAction::LeaveSearch => {
                        state.search_active = false;
                        state.focus_search = false;
                        state.query.clear();
                        state.highlighted = None;
                        state.reveal = Some(0);
                    }
                    EscapeAction::ClosePopup => outcome.close_requested = true,
                }
            }

            // Typing into a popup that shows no search row is what SUMMONS the row — and the
            // characters that summoned it must not be lost. They are taken out of the event
            // queue and seeded into the query HERE, before the field that would otherwise
            // have eaten them exists (see [`take_typed_text`]).
            let summoned_by_typing = !state.search_active
                && ui.input_mut(|input| take_typed_text(&mut input.events, &mut state.query));
            if summoned_by_typing {
                state.search_active = true;
                state.focus_search = true;
                state.reveal = Some(0);
            }

            let mut filtered = filter_items(items, &state.query);
            if summoned_by_typing {
                // A query appeared out of nothing: the cursor belongs on the first row that
                // survived it, exactly as when the field's own text changes.
                state.highlighted = filtered.first().copied();
            }
            let mut highlight_pos = resolve_highlight(&filtered, state.highlighted);

            // Arrow keys use the COUNT so an auto-repeating key still moves one row per
            // repeat instead of one row per frame.
            let steps = ui.input_mut(|input| {
                let down = input.count_and_consume_key(Modifiers::NONE, Key::ArrowDown);
                let up = input.count_and_consume_key(Modifiers::NONE, Key::ArrowUp);
                isize::try_from(down).unwrap_or(isize::MAX)
                    - isize::try_from(up).unwrap_or(isize::MAX)
            });
            if steps != 0 && !filtered.is_empty() {
                let moved = step_highlight(highlight_pos, filtered.len(), steps);
                highlight_pos = Some(moved);
                state.highlighted = filtered.get(moved).copied();
                state.reveal = Some(moved);
            }

            if ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter))
                && let Some(pos) = highlight_pos
                && let Some(&index) = filtered.get(pos)
            {
                outcome.picked = Some(index);
            }

            // The search row is a MODE, not furniture: it is drawn only while it is on, and
            // it takes its own space at the top of the popup, so the list below it is pushed
            // down rather than covered.
            if state.search_active {
                let search_response = ui.add(
                    egui::TextEdit::singleline(&mut state.query)
                        .id(body.search_id)
                        .desired_width(f32::INFINITY)
                        .hint_text(t!("widgets.searchable_combo_box.search_placeholder")),
                );
                // Re-requested on EVERY frame no widget holds focus, not just on the frame the
                // row appeared. egui's default `SurrenderFocusOn::Clicks` drops the field's
                // focus on any click that is not on it
                // (`egui-0.35.0/src/context.rs:1484-1493`) — the scrollbar trough, the
                // separator, the frame padding — and the popup, which stays open, has no other
                // focusable widget to take it, so a single stray click would otherwise leave
                // the search box dead for the rest of the opening. The request is conditioned
                // on focus being FREE rather than on the field simply lacking it, so that a
                // click on a widget outside the popup (which closes it) keeps the focus it
                // just took.
                if state.focus_search || ui.memory(|memory| memory.focused().is_none()) {
                    ui.memory_mut(|memory| memory.request_focus(body.search_id));
                    state.focus_search = false;
                }
                if search_response.changed() {
                    // The query moved, so the filtered list and every position into it did too.
                    filtered = filter_items(items, &state.query);
                    state.highlighted = filtered.first().copied();
                    highlight_pos = resolve_highlight(&filtered, state.highlighted);
                    state.reveal = Some(0);
                }

                ui.separator();
            }

            if filtered.is_empty() {
                ui.weak(t!("widgets.searchable_combo_box.nothing_found_status"));
                return outcome;
            }

            // The list must be allowed to GROW BACK after a query narrowed it, and its height
            // must not depend on whether the search row above it is showing. The popup's
            // `Area` hands its body LAST frame's content size as this frame's `max_rect`
            // (`egui-0.35.0/src/containers/area.rs:610` reads `AreaState::rect`, which is
            // written from `content_ui.min_size()` at `:666`), and the `ScrollArea` inside can
            // never exceed the space that leaves it (`scroll_area.rs:763-765`:
            // `outer_size = available_outer.size().at_most(max_size)`). Shrinking would
            // therefore be sticky and growing impossible: type a query that leaves two rows,
            // clear it, and the list would stay two rows tall for the rest of the opening.
            //
            // `set_min_height` is the ONLY correct call here, for a reason that cost a defect:
            // it goes through `Region::expand_to_include_y` (`ui.rs:748` -> `placer.rs:274-281`
            // -> `layout.rs:67-71`), which only extends `min_rect`/`max_rect`/`cursor` DOWNWARD
            // and leaves the cursor's top where it is. `set_max_height` looks like the same
            // thing and is not: it unions `max_rect` with `min_rect` and then assigns
            // `cursor.min.y = max_rect.min.y` (`placer.rs:248-258`), which — after the search
            // row and the separator have already been emitted — drags the cursor back to the
            // TOP of the popup, so the list is laid out over the widgets above it. That is
            // exactly the "search field painted over the first row" defect.
            //
            // Salting the popup id instead (what the old `WheelComboBox` call site did with the
            // combo's `id_salt`) is not usable either: the row count changes WITHIN one opening
            // now, and a churning id would both drop this widget's popup state and hand egui a
            // brand-new `Area` — drawn INVISIBLE for one frame while it sizes itself
            // (`area.rs:444` sets `sizing_pass = state.is_none()`, `:623-624` makes that pass
            // invisible), i.e. a blink per keystroke. Reserving the height is a FLOOR, not a
            // fixed height: `auto_shrink([false, true])` still shrinks a short list to its
            // content (`scroll_area.rs:1182-1183`).
            let desired_list_height = (row_index_as_f32(filtered.len())
                * body.geometry.row_height)
                .min(body.max_popup_height);
            ui.set_min_height(desired_list_height);

            let rows = ui
                .scope(|ui| {
                    // `show_rows` derives its row pitch as `row_height + item_spacing.y`
                    // (`egui-0.35.0/src/containers/scroll_area.rs:993`). Zeroing the spacing
                    // makes the pitch exactly `row_height`, which is what the reveal
                    // arithmetic below assumes; the padding rows need is already inside
                    // `row_height`.
                    ui.spacing_mut().item_spacing.y = 0.0;
                    draw_rows(
                        ui,
                        body,
                        state,
                        &filtered,
                        items,
                        RowSelection {
                            highlight_pos,
                            selected,
                        },
                        reborrow_resolver(&mut item_font),
                    )
                })
                .inner;
            if let Some(index) = rows.hovered {
                state.highlighted = Some(index);
            }
            if rows.picked.is_some() {
                outcome.picked = rows.picked;
            }

            outcome
        });

    inner.map_or_else(PopupOutcome::default, |popup| {
        // Published only now, because the popup's rect is not known until its body has run.
        // It must be the POPUP's rect and never `ui.clip_rect()` from inside the body: there
        // the clip rect is the Area's, which egui resolves to `Context::content_rect()`
        // (`egui-0.35.0/src/containers/area.rs:439` and `:628`) — the whole window. Publishing
        // that would make every `WheelSlider`/`WheelSpinBox` in the window consider itself
        // covered by the list and render visually dead while the drop-down is up.
        publish_combo_popup_rect(&button_response.ctx, popup.response.rect);
        popup.inner
    })
}

/// Which row is under the keyboard cursor and which one is the caller's current selection.
#[derive(Clone, Copy, Debug)]
struct RowSelection {
    /// Position in the FILTERED list of the keyboard cursor.
    highlight_pos: Option<usize>,
    /// The caller's selected index CLAMPED into range, as an index into the FULL item list;
    /// `None` when the list is empty. Clamped rather than raw so the row marked as the
    /// current value is always the row the closed button shows.
    selected: Option<usize>,
}

/// What one frame of the row list observed.
#[derive(Clone, Copy, Debug, Default)]
struct RowsOutcome {
    /// Item index the pointer is over, if any.
    hovered: Option<usize>,
    /// Item index the user clicked, if any.
    picked: Option<usize>,
}

/// Draws the virtualized row list and reports what the pointer did to it.
///
/// Only the rows `ScrollArea::show_rows` asks for are drawn, and therefore only those rows'
/// fonts are resolved — see [`SearchableComboBox`]'s contract.
fn draw_rows(
    ui: &mut Ui,
    body: &PopupBody,
    state: &mut PopupState,
    filtered: &[usize],
    items: &[SearchableComboItem<'_>],
    selection: RowSelection,
    mut item_font: ItemFontRef<'_, '_>,
) -> RowsOutcome {
    let geometry = body.geometry;
    let mut scroll_area = egui::ScrollArea::vertical()
        .id_salt("searchable_combo_rows")
        .max_height(body.max_popup_height)
        .auto_shrink([false, true]);
    if let Some(pos) = state.reveal.take() {
        scroll_area = scroll_area.vertical_scroll_offset(reveal_offset(
            RevealGeometry {
                current_offset: state.scroll_offset,
                viewport_height: state.viewport_height,
                max_viewport_height: body.max_popup_height,
                row_pitch: geometry.row_height,
                total_rows: filtered.len(),
            },
            pos,
        ));
    }

    // Measured ONCE per frame, never per row: every row of the list puts its text on the same
    // pair of baselines, and neither of them may depend on the face a given row draws in.
    let baselines = RowBaselines::measure(ui, body.primary_size, geometry);

    let mut rows = RowsOutcome::default();
    let output = scroll_area.show_rows(ui, geometry.row_height, filtered.len(), |ui, range| {
        for row in range {
            let Some(&index) = filtered.get(row) else {
                continue;
            };
            let Some(item) = items.get(index) else {
                continue;
            };
            let (rect, mut response) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), geometry.row_height),
                Sense::click(),
            );
            // The caller's own, already-localized explanation of this row. An empty string is
            // treated as absent: `on_hover_text` would otherwise pop an empty bubble over the
            // list. Attached before the hover checks below because it only reads the response
            // and returns it unchanged (`egui-0.35.0/src/response.rs:707`).
            if let Some(tooltip) = row_tooltip(item.tooltip) {
                response = response.on_hover_text(tooltip);
            }
            if response.hovered() {
                rows.hovered = Some(index);
            }
            if response.clicked() {
                rows.picked = Some(index);
            }
            if !ui.is_rect_visible(rect) {
                continue;
            }

            let is_highlighted = selection.highlight_pos == Some(row) || response.hovered();
            let is_selected = selection.selected == Some(index);
            let visuals = ui.visuals();
            let background = if is_highlighted {
                Some(visuals.selection.bg_fill)
            } else if is_selected {
                Some(visuals.faint_bg_color)
            } else {
                None
            };
            // The row STATE's own text colour. A row carrying its own colour
            // ([`SearchableComboItem::primary_color`]) replaces this on its unmatched
            // characters — but never the FILL above, which is what keeps the cursor's row and
            // the current selection recognisable whatever colour a row asks for.
            let primary_color = if is_highlighted {
                visuals.selection.stroke.color
            } else if is_selected {
                visuals.strong_text_color()
            } else {
                visuals.text_color()
            };
            let secondary_color = visuals.weak_text_color();
            let effective_background = background.unwrap_or_else(|| visuals.window_fill());
            let highlight = match_highlight_color(effective_background);
            let corner_radius = visuals.widgets.hovered.corner_radius;

            // Clipped to the row: a face whose ink overshoots the reserved line height must
            // not bleed into its neighbours, which uniform row height alone cannot prevent.
            let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
            if let Some(fill) = background {
                painter.rect_filled(rect, corner_radius, fill);
            }

            let text_rect =
                rect.shrink2(Vec2::new(ROW_HORIZONTAL_PADDING, ROW_VERTICAL_PADDING));
            let family = reborrow_resolver(&mut item_font)
                .and_then(|resolve| resolve(index))
                .unwrap_or(FontFamily::Proportional);
            let primary_ranges = matching::match_ranges(item.primary, &state.query);
            let primary_colors =
                LineColors::primary_row(primary_color, item.primary_color, highlight);
            let secondary_colors = LineColors {
                base: secondary_color,
                highlight,
            };
            // Both layouts place every galley by its BASELINE. Painting at a band's top edge
            // instead would put each baseline at the ITEM FACE's own ascent, which spans about
            // 9 pt at 16 pt across a font catalog — the main line would float up and down from
            // row to row and the second line would fall away from it.
            let primary_baseline = text_rect.top() + baselines.primary;
            let secondary_baseline = text_rect.top() + baselines.secondary;
            match geometry.layout {
                RowLayout::Tall => {
                    let primary_galley = layout_line(
                        ui,
                        item.primary,
                        FontId::new(body.primary_size, family),
                        Some(geometry.primary_line_height),
                        primary_colors,
                        &primary_ranges,
                        text_rect.width(),
                    );
                    paint_line_on_baseline(
                        &painter,
                        text_rect.left(),
                        primary_baseline,
                        primary_galley,
                        primary_colors.base,
                    );

                    if let Some(secondary) = item.secondary {
                        let secondary_ranges = matching::match_ranges(secondary, &state.query);
                        let secondary_galley = layout_line(
                            ui,
                            secondary,
                            // Always the interface font: the second line is an identifier, and
                            // rendering it in the row's own decorative face would make it
                            // unreadable.
                            FontId::new(geometry.secondary_size, FontFamily::Proportional),
                            Some(geometry.secondary_line_height),
                            secondary_colors,
                            &secondary_ranges,
                            text_rect.width(),
                        );
                        paint_line_on_baseline(
                            &painter,
                            text_rect.left(),
                            secondary_baseline,
                            secondary_galley,
                            secondary_color,
                        );
                    }
                }
                RowLayout::Wide => {
                    let primary_galley = layout_line(
                        ui,
                        item.primary,
                        FontId::new(body.primary_size, family),
                        Some(geometry.primary_line_height),
                        primary_colors,
                        &primary_ranges,
                        text_rect.width(),
                    );
                    // The only thing the second line takes from the main one: where it ends.
                    let secondary_left =
                        text_rect.left() + primary_galley.size().x + geometry.secondary_gap;
                    paint_line_on_baseline(
                        &painter,
                        text_rect.left(),
                        primary_baseline,
                        primary_galley,
                        primary_colors.base,
                    );

                    let secondary_width = text_rect.right() - secondary_left;
                    // A second line with no room left is skipped rather than laid out and
                    // painted into the clip rect's shadow.
                    if let Some(secondary) = item.secondary
                        && secondary_width > 0.0
                    {
                        let secondary_ranges = matching::match_ranges(secondary, &state.query);
                        let secondary_galley = layout_line(
                            ui,
                            secondary,
                            // Always the interface font: the second line is an identifier, and
                            // rendering it in the row's own decorative face would make it
                            // unreadable.
                            FontId::new(geometry.secondary_size, FontFamily::Proportional),
                            Some(secondary_galley_line_height(geometry.secondary_size)),
                            secondary_colors,
                            &secondary_ranges,
                            secondary_width,
                        );
                        paint_line_on_baseline(
                            &painter,
                            secondary_left,
                            // In `Wide` this IS the main line's baseline: one text row, one
                            // baseline, which is what makes the two texts sit level.
                            secondary_baseline,
                            secondary_galley,
                            secondary_color,
                        );
                    }
                }
            }
        }
    });

    // Remembered so the next frame's reveal can scroll by the smallest amount that brings
    // the target row into view instead of slamming it to the top of the list.
    state.scroll_offset = output.state.offset.y;
    state.viewport_height = output.inner_rect.height();
    rows
}

/// The tooltip a row actually shows: the caller's text, unless it is absent or EMPTY.
///
/// An empty string is treated as absent because `Response::on_hover_text("")` still opens a
/// bubble — an empty one — over the list (`egui-0.35.0/src/response.rs:707` adds a `Label`
/// unconditionally).
fn row_tooltip(tooltip: Option<&str>) -> Option<&str> {
    tooltip.filter(|text| !text.is_empty())
}

/// The two colours one line of a row is painted with.
#[derive(Clone, Copy, Debug)]
struct LineColors {
    /// Colour of the characters the query did not match.
    base: Color32,
    /// Colour of the characters the query matched.
    highlight: Color32,
}

impl LineColors {
    /// A line with nothing highlighted — the closed button's caption.
    fn plain(color: Color32) -> Self {
        Self {
            base: color,
            highlight: color,
        }
    }

    /// Colours of a popup row's MAIN line.
    ///
    /// `state_color` is what the row's state (plain / current selection / keyboard cursor)
    /// asks for, `item_color` is the caller's per-row override
    /// ([`SearchableComboItem::primary_color`]) and `highlight` is the colour the search
    /// already picked for this row's background.
    ///
    /// The override reaches the UNMATCHED characters only: `highlight` is passed through
    /// untouched, so a coloured row still shows where the query matched. `None` yields
    /// exactly the colours this widget used before per-row colouring existed.
    fn primary_row(state_color: Color32, item_color: Option<Color32>, highlight: Color32) -> Self {
        Self {
            base: item_color.unwrap_or(state_color),
            highlight,
        }
    }
}

/// Lays one line out as a single elided row, colouring the byte ranges in `highlights`.
///
/// `line_height` pins the galley's height so that it does not depend on `font`'s intrinsic
/// metrics — the property the uniform row height rests on. `None` asks for the face's OWN row
/// height instead, which is how [`interface_metrics`] measures a font. `highlights` must be
/// sorted, non-overlapping and on char boundaries, which is what [`matching::match_ranges`]
/// returns.
///
/// Pinning the height does NOT pin where the baseline sits inside the galley: epaint puts it
/// at the face's own ascent (`epaint-0.35.0/src/text/text_layout.rs:978`), so two galleys of
/// different faces painted at a common top edge do not share a line. Use
/// [`paint_line_on_baseline`] whenever that matters.
fn layout_line(
    ui: &Ui,
    text: &str,
    font: FontId,
    line_height: Option<f32>,
    colors: LineColors,
    highlights: &[Range<usize>],
    max_width: f32,
) -> Arc<Galley> {
    let LineColors {
        base: base_color,
        highlight: highlight_color,
    } = colors;
    let format = |color: Color32| TextFormat {
        font_id: font.clone(),
        color,
        line_height,
        ..TextFormat::default()
    };
    let section = |range: Range<usize>, color: Color32| LayoutSection {
        leading_space: 0.0,
        byte_range: ByteIndex::from(range.start)..ByteIndex::from(range.end),
        format: format(color),
    };

    let mut job = LayoutJob {
        text: text.to_owned(),
        wrap: TextWrapping {
            max_width: max_width.max(0.0),
            max_rows: 1,
            break_anywhere: true,
            overflow_character: Some('…'),
        },
        // The lines of a row are laid out as separate galleys, so a stray newline inside one
        // of them must not silently add a row and break the uniform height.
        break_on_newline: false,
        ..LayoutJob::default()
    };

    let mut cursor = 0usize;
    for range in highlights {
        if range.start > cursor {
            job.sections.push(section(cursor..range.start, base_color));
        }
        job.sections
            .push(section(range.start..range.end, highlight_color));
        cursor = range.end;
    }
    if cursor < text.len() || job.sections.is_empty() {
        // `LayoutJob` requires its sections to cover the whole text with no gaps, which for
        // empty text means one empty section rather than none.
        job.sections.push(section(cursor..text.len(), base_color));
    }

    ui.painter().layout_job(job)
}

/// Text the interface font's vertical metrics are measured with.
///
/// What it spells is irrelevant — the numbers come from the FACE — but a capital with neither
/// descender nor accent keeps the measurement readable while debugging.
const METRICS_PROBE_TEXT: &str = "H";

/// Vertical metrics of the interface font at one size, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
struct InterfaceMetrics {
    /// Distance from the top of the font's own line box down to its baseline.
    ascent: f32,
    /// Height of that line box.
    line_height: f32,
}

/// Measures the interface font at `size`, so that a row's baseline can be placed without
/// asking the row's OWN face where it would like to sit.
///
/// Costs one galley: epaint keys its layout cache on the job
/// (`epaint-0.35.0/src/text/fonts.rs:896`) and this job is identical for every row of a frame,
/// so it must still be called ONCE per frame rather than once per row. Falls back to the line
/// box itself when the probe produced no glyph, which only a font without `H` could cause.
fn interface_metrics(ui: &Ui, size: f32) -> InterfaceMetrics {
    let galley = layout_line(
        ui,
        METRICS_PROBE_TEXT,
        FontId::new(size, FontFamily::Proportional),
        // The face's own row height is exactly what is being measured, so nothing is pinned.
        None,
        // Never painted; the colour only takes part in the cache key.
        LineColors::plain(Color32::WHITE),
        &[],
        f32::INFINITY,
    );
    let line_height = galley.size().y;
    InterfaceMetrics {
        ascent: galley_baseline(&galley).unwrap_or(line_height),
        line_height,
    }
}

/// Distance from the top of `galley` to the baseline of its first row, or `None` when it has
/// no glyph to measure (empty text).
fn galley_baseline(galley: &Galley) -> Option<f32> {
    let row = galley.rows.first()?;
    // `Glyph::pos` is relative to its row and the row to the galley. The second term is zero
    // for the single-row galleys this widget lays out, but adding it costs nothing and keeps
    // the helper correct if one ever wraps.
    Some(row.pos.y + row.glyphs.first()?.pos.y)
}

/// Y of a row's shared baseline, measured from the top of the row's text area.
///
/// The interface font's line box is centred inside the row's box and its baseline is taken
/// from there. Two consequences, both wanted: the result depends ONLY on the interface font
/// at `primary_size`, so every row of a list gets the same baseline whatever face it draws in;
/// and the slack the row box has over that line box is split evenly, which leaves a decorative
/// face with an unusually tall ascent room above the baseline instead of clipping it.
fn row_baseline(metrics: InterfaceMetrics, box_height: f32) -> f32 {
    (box_height - metrics.line_height) * 0.5 + metrics.ascent
}

/// The baseline of each of a row's two lines, measured from the top of the row's text area.
///
/// Both are derived from the INTERFACE font, never from the face a row happens to draw its
/// main line in — see [`RowBaselines::measure`].
#[derive(Clone, Copy, Debug, PartialEq)]
struct RowBaselines {
    /// Baseline of the main line.
    primary: f32,
    /// Baseline of the second line. Equal to [`Self::primary`] in [`RowLayout::Wide`], where
    /// the two lines share one text row.
    secondary: f32,
}

impl RowBaselines {
    /// Measures one frame's baselines from the interface font at the row's nominal sizes.
    ///
    /// Call ONCE PER FRAME, not once per row: the two measurements are galley layouts, cached
    /// by epaint but not free, and their whole point is that they are the same for every row.
    ///
    /// In [`RowLayout::Tall`] the second line has a band of its own under the main line, so it
    /// gets its own baseline inside that band; the band's height is
    /// [`secondary_galley_line_height`], which is exactly what [`RowGeometry`] reserved for it.
    /// In [`RowLayout::Wide`] there is one text row and therefore one baseline.
    fn measure(ui: &Ui, primary_size: f32, geometry: RowGeometry) -> Self {
        let primary = row_baseline(
            interface_metrics(ui, primary_size),
            geometry.primary_line_height,
        );
        let secondary = match geometry.layout {
            RowLayout::Tall => {
                let band = secondary_galley_line_height(geometry.secondary_size);
                geometry.primary_line_height
                    + row_baseline(interface_metrics(ui, geometry.secondary_size), band)
            }
            RowLayout::Wide => primary,
        };
        Self { primary, secondary }
    }
}

/// Y of `galley`'s top edge that puts the baseline of its first row exactly on `baseline`.
fn galley_top_for_baseline(galley: &Galley, baseline: f32) -> f32 {
    // A galley with no glyph paints nothing, so any top edge is as good as another.
    baseline - galley_baseline(galley).unwrap_or(0.0)
}

/// Paints `galley` with its left edge at `left` and its first baseline exactly on `baseline`.
///
/// This is what makes two galleys of DIFFERENT faces and sizes share one line. Painting them
/// at a common TOP edge lines up their line boxes instead, and since epaint puts each
/// baseline at its own face's ascent inside that box
/// (`epaint-0.35.0/src/text/text_layout.rs:978`), the smaller or the taller-ascent one then
/// floats visibly off the line.
fn paint_line_on_baseline(
    painter: &egui::Painter,
    left: f32,
    baseline: f32,
    galley: Arc<Galley>,
    color: Color32,
) {
    let top = galley_top_for_baseline(&galley, baseline);
    painter.galley(egui::pos2(left, top), galley, color);
}

/// Line height of a second line's galley, from its own size.
///
/// A pure function of `secondary_size`, and therefore of `primary_size`: it never consults a
/// face, which is what lets [`RowLayout::Tall`] reserve exactly this much room under the main
/// line and lets [`RowLayout::Wide`] leave it out of the row's height entirely.
fn secondary_galley_line_height(secondary_size: f32) -> f32 {
    (secondary_size * SECONDARY_LINE_HEIGHT_FACTOR).max(1.0)
}

/// Colour for matched characters that stays legible on `background`.
///
/// The decision is made against the ACTUAL background the text is painted on (selection
/// fill, faint fill, or the popup's own fill), not against the theme, because the selected
/// row's fill and the popup's fill are on opposite ends of the lightness range in both
/// themes. `Rgba::intensity` is a perceptual weighting of the LINEAR components, so the
/// 0.5 threshold sits near mid-grey rather than near the sRGB midpoint.
fn match_highlight_color(background: Color32) -> Color32 {
    if egui::Rgba::from(background).intensity() < 0.5 {
        HIGHLIGHT_ON_DARK
    } else {
        HIGHLIGHT_ON_LIGHT
    }
}

/// What `Escape` does inside an open popup.
///
/// Two presses take a searching user back to a closed list at most: the first leaves search
/// mode, the second closes the popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscapeAction {
    /// Drop the query and hide the search row; the plain list stays open.
    LeaveSearch,
    /// Close the popup.
    ClosePopup,
}

/// Decides what `Escape` does, from the search row's state alone.
///
/// A search row with something typed in it is the only thing `Escape` un-does before closing:
/// an empty query (or no search row at all) means there is nothing to clear, so the key goes
/// straight to closing the list.
fn escape_action(search_active: bool, query_is_empty: bool) -> EscapeAction {
    if search_active && !query_is_empty {
        EscapeAction::LeaveSearch
    } else {
        EscapeAction::ClosePopup
    }
}

/// What a click on the square search button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchButtonAction {
    /// The popup was closed: open it with the search row already up and focused.
    Open,
    /// The popup was open without a search row: show it and focus it.
    Enable,
    /// The search row was up: hide it and drop the query, leaving the list open.
    Disable,
}

/// Decides what the search button does this frame — it is a TOGGLE, and it also opens.
fn search_button_action(open: bool, search_active: bool) -> SearchButtonAction {
    match (open, search_active) {
        (false, _) => SearchButtonAction::Open,
        (true, false) => SearchButtonAction::Enable,
        (true, true) => SearchButtonAction::Disable,
    }
}

/// Moves the text typed this frame into `query`, CONSUMING those events, and reports whether
/// anything was typed.
///
/// This is how the first character survives the search row's appearance. While the row is
/// hidden nothing in the popup holds focus, so the `egui::TextEdit` that is about to be shown
/// would never see the keystroke that summoned it: typing `san` would leave `an` in the field.
/// The events are removed from the queue precisely so that the field — focused later in the
/// SAME frame — cannot append them a second time.
///
/// Only `Event::Text` is taken (`egui-0.35.0/src/data/input/event.rs:30`), which egui emits
/// for printable input only: `Enter`, the arrows and `Escape` arrive as `Event::Key` and stay
/// in the queue for the popup's own handling.
fn take_typed_text(events: &mut Vec<egui::Event>, query: &mut String) -> bool {
    let mut typed = String::new();
    events.retain(|event| {
        if let egui::Event::Text(text) = event {
            typed.push_str(text);
            false
        } else {
            true
        }
    });
    if typed.is_empty() {
        return false;
    }
    query.push_str(&typed);
    true
}

/// Indices of the items matching `query`, in list order. An empty query keeps everything.
fn filter_items(items: &[SearchableComboItem<'_>], query: &str) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| matching::item_matches(item.primary, item.secondary, query))
        .map(|(index, _)| index)
        .collect()
}

/// Position of `highlighted` inside `filtered`, falling back to the first row.
///
/// Returns `None` only for an empty list. A highlighted item that the current query filters
/// out lands on the first row, so the keyboard always has somewhere to start.
fn resolve_highlight(filtered: &[usize], highlighted: Option<usize>) -> Option<usize> {
    if filtered.is_empty() {
        return None;
    }
    let position = highlighted.and_then(|index| filtered.iter().position(|&item| item == index));
    Some(position.unwrap_or(0))
}

/// Moves the keyboard cursor by `steps` rows inside a list of `len` rows, clamping at both
/// ends. `len` must be non-zero.
fn step_highlight(position: Option<usize>, len: usize, steps: isize) -> usize {
    let last = len.saturating_sub(1);
    let current = isize::try_from(position.unwrap_or(0)).unwrap_or(0);
    let target = current.saturating_add(steps).max(0);
    usize::try_from(target).unwrap_or(0).min(last)
}

/// Everything [`reveal_offset`] derives one frame's scroll offset from.
#[derive(Clone, Copy, Debug)]
struct RevealGeometry {
    /// Last frame's vertical scroll offset of the list, in points.
    current_offset: f32,
    /// Last frame's visible height of the list, in points. `0.0` before the first frame.
    viewport_height: f32,
    /// Height the list is capped at, in points. Stands in for `viewport_height` on the frame
    /// the popup opens, where the real one has not been measured yet.
    max_viewport_height: f32,
    /// Distance between two consecutive row tops, in points. Equals the row height here
    /// because the list zeroes its vertical item spacing.
    row_pitch: f32,
    /// Number of rows in the FILTERED list — the list the offset is measured against.
    total_rows: usize,
}

/// Scroll offset that brings row `position` into view with the least movement.
///
/// A `viewport_height` of zero means the list has not been laid out yet (the frame the popup
/// opens), and the row is put at the top — there is nothing better to align it against.
///
/// The result is always inside the list's own scroll range, `0 ..= content height - viewport`.
/// `ScrollArea` applies an offset in `begin` but clamps it only in `end`
/// (`egui-0.35.0/src/containers/scroll_area.rs:743` and `:1437`), so an offset past the end of
/// the content paints one frame of over-scroll before snapping back — precisely on the frame a
/// popup opens near the end of a long list, where the viewport is still unmeasured and
/// `max_viewport_height` is the only bound available.
fn reveal_offset(geometry: RevealGeometry, position: usize) -> f32 {
    let RevealGeometry {
        current_offset,
        viewport_height,
        max_viewport_height,
        row_pitch,
        total_rows,
    } = geometry;
    let content_height = row_pitch * row_index_as_f32(total_rows);
    let effective_viewport = if viewport_height > 0.0 {
        viewport_height
    } else {
        max_viewport_height
    };
    // Never negative, so it is always a valid upper bound for the clamp below.
    let max_offset = (content_height - effective_viewport).max(0.0);

    let row_top = row_pitch * row_index_as_f32(position);
    let offset = if viewport_height <= 0.0 {
        row_top
    } else {
        let row_bottom = row_top + row_pitch;
        if row_top < current_offset {
            row_top
        } else if row_bottom > current_offset + viewport_height {
            row_bottom - viewport_height
        } else {
            current_offset
        }
    };
    offset.clamp(0.0, max_offset)
}

/// Largest row index this widget converts to `f32` exactly: `f32` represents every integer
/// below 2^24 without loss.
const MAX_EXACT_ROW_INDEX: usize = 1 << 24;

/// Converts a row index to `f32` for scroll arithmetic, saturating instead of rounding.
///
/// Rust offers no lossless `usize -> f32` conversion, so the value is bounded first: below
/// [`MAX_EXACT_ROW_INDEX`] the `as` cast is provably exact (`CLAUDE.md` §17's proven-safe
/// exception), and at or above it the offset saturates — a list of 16.7 million rows cannot
/// be scrolled meaningfully anyway, and a saturated offset is already past its end.
fn row_index_as_f32(position: usize) -> f32 {
    if position >= MAX_EXACT_ROW_INDEX {
        // 2^24 is itself exactly representable.
        return 16_777_216.0;
    }
    // Proven exact by the guard above.
    position as f32
}

/// The search predicate and the ranges it highlights. GUI-free on purpose, so the matching
/// rules are unit-tested without an `egui::Context`.
mod matching {
    use super::ignore_case_prefix_len;
    use std::ops::Range;

    /// Byte ranges of every case-insensitive occurrence of `query` in `haystack`.
    ///
    /// The ranges are sorted, non-overlapping, and always land on char boundaries — the
    /// contract [`super::layout_line`] relies on when it slices the text into `LayoutJob`
    /// sections. An empty `query` matches nothing to highlight and returns an empty vector.
    ///
    /// OCCURRENCES MAY OVERLAP, and all of them are covered: `"ana"` occurs twice in
    /// `"banana"`, so the whole `"anana"` is highlighted. Overlapping and touching hits are
    /// merged into one range, which is how the output stays non-overlapping without dropping
    /// any matched character.
    ///
    /// Matching streams `char::to_lowercase` through
    /// [`ignore_case_prefix_len`](super::ignore_case_prefix_len) rather than lowercasing the
    /// candidate: an offset found in a lowercased copy cannot be mapped back onto the
    /// original, because folding changes both char count and byte length.
    #[must_use]
    pub(super) fn match_ranges(haystack: &str, query: &str) -> Vec<Range<usize>> {
        let mut ranges: Vec<Range<usize>> = Vec::new();
        if query.is_empty() {
            return ranges;
        }
        let mut start = 0usize;
        while start < haystack.len() {
            let tail = &haystack[start..];
            let step = first_char_len(tail);
            if step == 0 {
                // Unreachable while `start < haystack.len()`, but bailing out is cheaper than
                // an infinite loop if that ever stops holding.
                break;
            }
            if let Some(chars) = ignore_case_prefix_len(tail, query) {
                let length = byte_len_of_chars(tail, chars);
                if length == 0 {
                    // A non-empty query cannot consume zero chars; bail out rather than
                    // loop forever if that invariant is ever broken upstream.
                    break;
                }
                push_merged(&mut ranges, start..start + length);
            }
            // Advance ONE char rather than past the match: the next occurrence may start
            // inside this one, and skipping to its end would leave that one unhighlighted.
            start += step;
        }
        ranges
    }

    /// Appends `range` to `ranges`, merging it into the last one when the two overlap or
    /// touch.
    ///
    /// `ranges` is built left to right, so only the last one can ever be adjacent to a new
    /// range. Merging is what keeps the output sorted and non-overlapping, which is the
    /// contract [`super::layout_line`] slices the text on.
    fn push_merged(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
        match ranges.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => ranges.push(range),
        }
    }

    /// Whether a row matches `query`: an empty query matches everything, otherwise either
    /// line must contain the query case-insensitively.
    #[must_use]
    pub(super) fn item_matches(primary: &str, secondary: Option<&str>, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        contains_ignore_case(primary, query)
            || secondary.is_some_and(|text| contains_ignore_case(text, query))
    }

    /// Case-insensitive `str::contains` with the streaming folding of
    /// [`ignore_case_prefix_len`](super::ignore_case_prefix_len). Short-circuits on the first
    /// hit, which is what makes filtering a large catalog per frame affordable.
    #[must_use]
    fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let mut start = 0usize;
        while start < haystack.len() {
            let tail = &haystack[start..];
            if ignore_case_prefix_len(tail, needle).is_some() {
                return true;
            }
            start += first_char_len(tail);
        }
        false
    }

    /// Byte length of the first `count` chars of `text`, or the whole of it when it is
    /// shorter. Keeps every offset this module produces on a char boundary.
    fn byte_len_of_chars(text: &str, count: usize) -> usize {
        text.char_indices()
            .nth(count)
            .map_or(text.len(), |(offset, _)| offset)
    }

    /// Byte length of `text`'s first char; `text.len()` when it is empty (never reached by
    /// the callers above, which test `start < haystack.len()` first).
    fn first_char_len(text: &str) -> usize {
        text.chars().next().map_or(text.len(), char::len_utf8)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn searchable_combo_empty_query_highlights_nothing_and_matches_everything() {
            assert!(match_ranges("Arial", "").is_empty());
            assert!(item_matches("Arial", Some("ArialMT"), ""));
            assert!(item_matches("", None, ""));
        }

        #[test]
        fn searchable_combo_reports_no_match() {
            assert!(match_ranges("Arial", "zzz").is_empty());
            assert!(!item_matches("Arial", Some("ArialMT"), "zzz"));
        }

        #[test]
        fn searchable_combo_finds_every_occurrence() {
            // Touching occurrences are merged: bytes 1..5 of "banana" are all matched.
            assert_eq!(match_ranges("banana", "an"), vec![1..5]);
            assert_eq!(match_ranges("aaaa", "aa"), vec![0..4]);
            // Separated occurrences stay separate.
            assert_eq!(match_ranges("an-an", "an"), vec![0..2, 3..5]);
        }

        #[test]
        fn searchable_combo_covers_overlapping_occurrences() {
            // The regression this guards: the scan used to resume past the END of a match,
            // so the second "ana" of "banana" was left unhighlighted.
            assert_eq!(match_ranges("banana", "ana"), vec![1..6]);
            assert_eq!(match_ranges("aaa", "aa"), vec![0..3]);
            // An overlap that does not reach the end of the text.
            assert_eq!(match_ranges("ababa!", "aba"), vec![0..5]);
        }

        #[test]
        fn searchable_combo_merged_ranges_stay_ordered_and_disjoint() {
            let ranges = match_ranges("banana banana", "ana");
            assert_eq!(ranges, vec![1..6, 8..13]);
            for pair in ranges.windows(2) {
                assert!(pair[0].end < pair[1].start);
            }
        }

        #[test]
        fn searchable_combo_matches_case_insensitively_anywhere() {
            assert_eq!(match_ranges("CCWildWords", "wild"), vec![2..6]);
            assert_eq!(match_ranges("CCWildWords", "WORDS"), vec![6..11]);
        }

        #[test]
        fn searchable_combo_matches_secondary_line_only() {
            assert!(item_matches("Основной", Some("MainRoman"), "roman"));
            assert!(!item_matches("Основной", None, "roman"));
        }

        #[test]
        fn searchable_combo_folds_cyrillic_case() {
            // Both letters are two bytes in UTF-8, so a byte-oriented match would land
            // inside a char here.
            assert_eq!(match_ranges("Ёлка ёлка", "ёл"), vec![0..4, 9..13]);
            assert!(item_matches("ТЕХНО", None, "техно"));
        }

        #[test]
        fn searchable_combo_multibyte_ranges_stay_on_char_boundaries() {
            let text = "日本語フォント";
            let ranges = match_ranges(text, "フォント");
            assert_eq!(ranges, vec![9..21]);
            for range in &ranges {
                assert!(text.is_char_boundary(range.start));
                assert!(text.is_char_boundary(range.end));
                assert!(text.get(range.clone()).is_some());
            }
        }

        #[test]
        fn searchable_combo_query_longer_than_candidate_never_matches() {
            assert!(match_ranges("ab", "abcdef").is_empty());
            assert!(!item_matches("ab", Some("cd"), "abcdef"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The keystroke that SUMMONS the search row must survive it. Typing `san` into a popup
    // whose field does not exist yet has to end up as `san`, not `an`: the first character is
    // taken out of the event queue here, and the rest reach the field once it is focused.
    #[test]
    fn searchable_combo_typed_text_seeds_the_query_and_is_consumed() {
        let mut events = vec![
            egui::Event::Text("s".to_owned()),
            egui::Event::Text("a".to_owned()),
            egui::Event::Text("n".to_owned()),
        ];
        let mut query = String::new();

        assert!(take_typed_text(&mut events, &mut query));
        assert_eq!(query, "san");
        // Consumed, so the field focused later in the SAME frame cannot append them again.
        assert!(events.is_empty());
    }

    // Only `Event::Text` is taken: the keys the popup drives itself with must stay in the
    // queue for `consume_key` further down the same frame.
    #[test]
    fn searchable_combo_typed_text_leaves_other_events_alone() {
        let key_event = egui::Event::Key {
            key: Key::ArrowDown,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let mut events = vec![key_event.clone(), egui::Event::Text("q".to_owned())];
        let mut query = String::from("existing");

        assert!(take_typed_text(&mut events, &mut query));
        assert_eq!(query, "existingq");
        assert_eq!(events, vec![key_event]);
    }

    // A frame with no typing must not summon the search row.
    #[test]
    fn searchable_combo_no_text_events_report_nothing_typed() {
        let mut events = vec![egui::Event::PointerMoved(egui::pos2(0.0, 0.0))];
        let mut query = String::new();

        assert!(!take_typed_text(&mut events, &mut query));
        assert!(query.is_empty());
        assert_eq!(events.len(), 1);
    }

    // Two presses at most from "searching" to "closed", and never more than one step per
    // press: the first `Escape` drops the query and the search row, the second closes.
    #[test]
    fn searchable_combo_escape_leaves_search_before_closing() {
        assert_eq!(escape_action(true, false), EscapeAction::LeaveSearch);
        assert_eq!(escape_action(true, true), EscapeAction::ClosePopup);
        assert_eq!(escape_action(false, true), EscapeAction::ClosePopup);
        // A stale query with the row already gone must not swallow an `Escape`.
        assert_eq!(escape_action(false, false), EscapeAction::ClosePopup);
    }

    // The square button opens a closed list straight into search mode and toggles the row on
    // an open one.
    #[test]
    fn searchable_combo_search_button_opens_then_toggles() {
        assert_eq!(search_button_action(false, false), SearchButtonAction::Open);
        assert_eq!(search_button_action(false, true), SearchButtonAction::Open);
        assert_eq!(search_button_action(true, false), SearchButtonAction::Enable);
        assert_eq!(search_button_action(true, true), SearchButtonAction::Disable);
    }

    // A popup opened by the combo button is JUST the list; only the square button opens it
    // with the search row already up and focused. Both start with the cursor on the selection.
    #[test]
    fn searchable_combo_fresh_popup_starts_without_the_search_row() {
        let plain = fresh_popup_state(Some(4), false);
        assert!(!plain.search_active);
        assert!(!plain.focus_search);
        assert!(plain.query.is_empty());
        assert_eq!(plain.highlighted, Some(4));
        assert_eq!(plain.reveal, Some(4));

        let searching = fresh_popup_state(Some(4), true);
        assert!(searching.search_active);
        assert!(searching.focus_search);
        assert!(searching.query.is_empty());
        assert_eq!(searching.highlighted, Some(4));

        // An empty list has no row to reveal, and 0 is where the list would start.
        assert_eq!(fresh_popup_state(None, false).reveal, Some(0));
    }

    #[test]
    fn searchable_combo_highlight_falls_back_to_first_row() {
        assert_eq!(resolve_highlight(&[], Some(3)), None);
        assert_eq!(resolve_highlight(&[4, 7, 9], Some(7)), Some(1));
        // Filtered out by the current query: start over at the top.
        assert_eq!(resolve_highlight(&[4, 7, 9], Some(5)), Some(0));
        assert_eq!(resolve_highlight(&[4, 7, 9], None), Some(0));
    }

    #[test]
    fn searchable_combo_arrow_steps_clamp_at_both_ends() {
        assert_eq!(step_highlight(Some(0), 3, 1), 1);
        assert_eq!(step_highlight(Some(2), 3, 1), 2);
        assert_eq!(step_highlight(Some(0), 3, -1), 0);
        assert_eq!(step_highlight(None, 3, 2), 2);
        assert_eq!(step_highlight(Some(1), 3, isize::MIN), 0);
        assert_eq!(step_highlight(Some(1), 3, isize::MAX), 2);
    }

    #[test]
    fn searchable_combo_reveal_moves_by_the_smallest_amount() {
        // A list long enough (100 rows of 20 pt) that the clamp never binds here.
        let geometry = |current_offset: f32, viewport_height: f32| RevealGeometry {
            current_offset,
            viewport_height,
            max_viewport_height: 100.0,
            row_pitch: 20.0,
            total_rows: 100,
        };
        // No viewport yet (the frame the popup opens): put the row at the top.
        assert!((reveal_offset(geometry(0.0, 0.0), 4) - 80.0).abs() < f32::EPSILON);
        // Already visible: do not move.
        assert!((reveal_offset(geometry(40.0, 100.0), 4) - 40.0).abs() < f32::EPSILON);
        // Above the viewport: align to its top.
        assert!((reveal_offset(geometry(100.0, 100.0), 1) - 20.0).abs() < f32::EPSILON);
        // Below the viewport: align to its bottom.
        assert!((reveal_offset(geometry(0.0, 100.0), 6) - 40.0).abs() < f32::EPSILON);
        assert!(reveal_offset(geometry(0.0, 100.0), 0) >= 0.0);
    }

    #[test]
    fn searchable_combo_reveal_never_scrolls_past_the_end_of_the_list() {
        // The opening frame: the viewport is not measured yet, so the cap the list is about
        // to be given bounds the offset. 10 rows x 20 pt = 200 pt of content in a 100 pt
        // viewport leaves exactly 100 pt of scroll range, and the last row's top (180) is
        // past it.
        let opening = RevealGeometry {
            current_offset: 0.0,
            viewport_height: 0.0,
            max_viewport_height: 100.0,
            row_pitch: 20.0,
            total_rows: 10,
        };
        assert!((reveal_offset(opening, 9) - 100.0).abs() < f32::EPSILON);
        // A list shorter than the viewport cannot be scrolled at all.
        let short = RevealGeometry {
            total_rows: 3,
            ..opening
        };
        assert!(reveal_offset(short, 2).abs() < f32::EPSILON);
        // The clamp also holds once the viewport is known.
        let measured = RevealGeometry {
            viewport_height: 100.0,
            ..opening
        };
        assert!((reveal_offset(measured, 9) - 100.0).abs() < f32::EPSILON);
    }

    #[test]
    fn searchable_combo_row_index_conversion_is_exact_then_saturates() {
        assert!((row_index_as_f32(0) - 0.0).abs() < f32::EPSILON);
        assert!((row_index_as_f32(123_456) - 123_456.0).abs() < f32::EPSILON);
        assert!((row_index_as_f32(MAX_EXACT_ROW_INDEX - 1) - 16_777_215.0).abs() < f32::EPSILON);
        assert!((row_index_as_f32(usize::MAX) - 16_777_216.0).abs() < f32::EPSILON);
    }

    /// Two items, the first with a second line and the second without — the mixed list the
    /// `Tall` reservation exists for.
    const MIXED_ITEMS: [SearchableComboItem<'static>; 2] = [
        SearchableComboItem {
            primary: "A",
            secondary: Some("a"),
            primary_color: None,
            tooltip: None,
        },
        SearchableComboItem {
            primary: "B",
            secondary: None,
            primary_color: None,
            tooltip: None,
        },
    ];

    /// The same two items with no second line anywhere.
    const PLAIN_ITEMS: [SearchableComboItem<'static>; 2] = [
        SearchableComboItem {
            primary: "A",
            secondary: None,
            primary_color: None,
            tooltip: None,
        },
        SearchableComboItem {
            primary: "B",
            secondary: None,
            primary_color: None,
            tooltip: None,
        },
    ];

    #[test]
    fn searchable_combo_row_geometry_is_uniform_and_reserves_the_second_line() {
        let mixed = RowGeometry::new(14.0, RowLayout::Tall, &MIXED_ITEMS);
        let plain = RowGeometry::new(14.0, RowLayout::Tall, &PLAIN_ITEMS);
        assert!(mixed.secondary_line_height > 0.0);
        assert!((plain.secondary_line_height - 0.0).abs() < f32::EPSILON);
        assert!(mixed.row_height > plain.row_height);
        // The second line is EXACTLY half the main line's size, at every size: there is no
        // floor, because one would make the two lines look alike on a small main line.
        assert!((mixed.secondary_size - 7.0).abs() < f32::EPSILON);
        assert!(
            (RowGeometry::new(8.0, RowLayout::Tall, &MIXED_ITEMS).secondary_size - 4.0).abs()
                < f32::EPSILON
        );
        assert!(
            (RowGeometry::new(4.0, RowLayout::Tall, &MIXED_ITEMS).secondary_size - 2.0).abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn searchable_combo_tall_is_the_default_layout() {
        // A caller that never mentions a layout must keep the two-line rows it had before the
        // layout existed.
        assert_eq!(RowLayout::default(), RowLayout::Tall);
        assert_eq!(
            SearchableComboBox::new("searchable_combo_tests.default").row_layout,
            RowLayout::Tall
        );
    }

    #[test]
    fn searchable_combo_tall_geometry_is_unchanged_by_the_layout_switch() {
        // The exact numbers `Tall` produced before `RowLayout` existed: 14 x 1.6 main line,
        // a second line of 14 x 0.5 x 1.25, and 2 x 2 pt of vertical padding.
        let tall = RowGeometry::new(14.0, RowLayout::Tall, &MIXED_ITEMS);
        assert!((tall.primary_line_height - 22.4).abs() < 1e-4);
        assert!((tall.secondary_size - 7.0).abs() < 1e-4);
        assert!((tall.secondary_line_height - 8.75).abs() < 1e-4);
        assert!((tall.row_height - 35.15).abs() < 1e-4);
        // `Tall` puts the second line on a row of its own, so it needs no horizontal gap.
        assert!((tall.secondary_gap - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn searchable_combo_wide_reserves_no_second_line() {
        let wide = RowGeometry::new(14.0, RowLayout::Wide, &MIXED_ITEMS);
        assert!((wide.secondary_line_height - 0.0).abs() < f32::EPSILON);
        // Exactly one main line plus the vertical padding, nothing else.
        assert!((wide.row_height - (22.4 + 2.0 * ROW_VERTICAL_PADDING)).abs() < 1e-4);
        assert!(wide.row_height < RowGeometry::new(14.0, RowLayout::Tall, &MIXED_ITEMS).row_height);
        // The trailing second line still needs its size and the gap before it.
        assert!((wide.secondary_size - 7.0).abs() < 1e-4);
        assert!((wide.secondary_gap - 14.0 * WIDE_SECONDARY_GAP_FACTOR).abs() < 1e-4);
    }

    #[test]
    fn searchable_combo_wide_row_height_ignores_second_lines() {
        // What makes `show_rows` virtualization valid in `Wide`: the height cannot depend on
        // the list's content, so filtering can never change the pitch under the scroll area.
        let mixed = RowGeometry::new(14.0, RowLayout::Wide, &MIXED_ITEMS);
        let plain = RowGeometry::new(14.0, RowLayout::Wide, &PLAIN_ITEMS);
        let empty = RowGeometry::new(14.0, RowLayout::Wide, &[]);
        assert!((mixed.row_height - plain.row_height).abs() < f32::EPSILON);
        assert!((mixed.row_height - empty.row_height).abs() < f32::EPSILON);
        // In `Tall` the same two lists differ, which is exactly the reservation `Wide` drops.
        let tall_mixed = RowGeometry::new(14.0, RowLayout::Tall, &MIXED_ITEMS);
        let tall_plain = RowGeometry::new(14.0, RowLayout::Tall, &PLAIN_ITEMS);
        assert!(tall_mixed.row_height > tall_plain.row_height);
    }

    #[test]
    fn searchable_combo_both_layouts_keep_the_same_main_line() {
        // The main line is what the closed button draws, and its height feeds the button's
        // own size: it must not move when the layout does.
        for size in [8.0_f32, 14.0, 28.0] {
            let tall = RowGeometry::new(size, RowLayout::Tall, &MIXED_ITEMS);
            let wide = RowGeometry::new(size, RowLayout::Wide, &MIXED_ITEMS);
            assert!((tall.primary_line_height - wide.primary_line_height).abs() < f32::EPSILON);
            assert!((tall.secondary_size - wide.secondary_size).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn searchable_combo_filters_by_either_line() {
        let items = [
            SearchableComboItem::with_secondary("Основной", "MainRoman"),
            SearchableComboItem::with_secondary("Техно", "TechnoB"),
            SearchableComboItem::new("Narezka"),
        ];
        assert_eq!(filter_items(&items, ""), vec![0, 1, 2]);
        assert_eq!(filter_items(&items, "roman"), vec![0]);
        assert_eq!(filter_items(&items, "техно"), vec![1]);
        assert_eq!(filter_items(&items, "n"), vec![0, 1, 2]);
        assert!(filter_items(&items, "zzz").is_empty());
        assert!(filter_items(&[], "a").is_empty());
    }

    /// A context whose `FontFamily::Name("decorative")` is the interface face re-registered
    /// with a `scale` tweak — a stand-in for a display face whose ascent is nothing like the
    /// interface font's, without shipping a font file into the test.
    fn context_with_decorative_face(scale: f32) -> egui::Context {
        let ctx = egui::Context::default();
        let mut definitions = egui::FontDefinitions::default();
        let base = definitions
            .families
            .get(&FontFamily::Proportional)
            .and_then(|names| names.first())
            .and_then(|name| definitions.font_data.get(name))
            .map(|data| (**data).clone());
        assert!(base.is_some(), "the default definitions must carry a proportional font");
        if let Some(base) = base {
            let tweaked = base.tweak(egui::FontTweak {
                scale,
                ..egui::FontTweak::default()
            });
            definitions
                .font_data
                .insert("decorative".to_owned(), Arc::new(tweaked));
            definitions.families.insert(
                FontFamily::Name("decorative".into()),
                vec!["decorative".to_owned()],
            );
        }
        ctx.set_fonts(definitions);
        ctx
    }

    /// Lays a row's MAIN line out the way `draw_rows` does, at `size` and in `family`.
    fn main_line_galley(
        ui: &Ui,
        geometry: RowGeometry,
        size: f32,
        family: FontFamily,
    ) -> Arc<Galley> {
        layout_line(
            ui,
            "Main",
            FontId::new(size, family),
            Some(geometry.primary_line_height),
            LineColors::plain(Color32::WHITE),
            &[],
            1000.0,
        )
    }

    #[test]
    fn searchable_combo_row_baseline_centres_the_interface_line_box() {
        // egui's default proportional font at 16 pt, measured: ascent 15, line box 18.
        let metrics = InterfaceMetrics {
            ascent: 15.0,
            line_height: 18.0,
        };
        // (25.6 - 18) / 2 + 15. The row box is `PRIMARY_LINE_HEIGHT_FACTOR` x 16.
        assert!((row_baseline(metrics, 25.6) - 18.8).abs() < 1e-4);
        // A taller row moves the baseline down by half the extra height, keeping the interface
        // font's ink centred rather than pinned to either edge.
        assert!((row_baseline(metrics, 29.6) - 20.8).abs() < 1e-4);
        // The headroom above the baseline exceeds the interface font's own ascent, which is
        // what a decorative face with a taller one draws into instead of being cut off.
        assert!(row_baseline(metrics, 25.6) > metrics.ascent);
        // A row box no larger than the line box still yields a usable baseline.
        assert!((row_baseline(metrics, 18.0) - 15.0).abs() < 1e-4);
    }

    #[test]
    fn searchable_combo_wide_baseline_does_not_follow_the_item_face() {
        // The defect this pins: a row's main line used to be painted at the row's TOP edge,
        // which puts its baseline at the FACE's own ascent — 15 pt for the interface font and
        // 24 pt for the decorative face below, at the same nominal 16 pt. Rows stayed exactly
        // as tall as each other, but their ink floated up and down by that difference, which
        // is what read as "some rows are taller".
        let ctx = context_with_decorative_face(1.6);
        let geometry = RowGeometry::new(16.0, RowLayout::Wide, &MIXED_ITEMS);
        let mut measured: Vec<(f32, f32, f32)> = Vec::new();
        // A pass is what makes a font atlas exist at all: `Context::fonts_mut` panics before
        // the first one (`egui-0.35.0/src/context.rs:1055`). The frame's paint output has no
        // window to go to, hence the drop.
        drop(ctx.run_ui(egui::RawInput::default(), |ui| {
            let baseline = RowBaselines::measure(ui, 16.0, geometry).primary;
            for family in [FontFamily::Proportional, FontFamily::Name("decorative".into())] {
                let galley = main_line_galley(ui, geometry, 16.0, family);
                if let Some(in_galley) = galley_baseline(&galley) {
                    measured.push((
                        galley.size().y,
                        in_galley,
                        galley_top_for_baseline(&galley, baseline) + in_galley,
                    ));
                }
            }
            measured.push((baseline, baseline, baseline));
        }));
        assert_eq!(measured.len(), 3, "both faces must produce a measurable row");
        let (interface_height, interface_in_galley, interface_landed) = measured[0];
        let (decorative_height, decorative_in_galley, decorative_landed) = measured[1];
        let (baseline, _, _) = measured[2];

        // The galley never inflates: its height is the PINNED line height (25.6, rounded to
        // the pixel grid), whatever the face — so the item's face cannot change the row.
        assert!((interface_height - decorative_height).abs() < f32::EPSILON);
        assert!((interface_height - geometry.primary_line_height).abs() <= 1.0);
        // The two faces really do disagree about where their baseline sits, which is what
        // painting at a common top edge would have exposed.
        assert!(
            (interface_in_galley - decorative_in_galley).abs() > 1.0,
            "the decorative face must have a different ascent for this test to mean anything"
        );
        // And both land on the row's ONE baseline once placed by it.
        assert!((interface_landed - baseline).abs() < 1e-3);
        assert!((decorative_landed - baseline).abs() < 1e-3);
    }

    #[test]
    fn searchable_combo_wide_second_line_sits_on_the_main_baseline() {
        // "Level along the bottom" in the user's words: the half-size trailing text shares the
        // main line's baseline, so glyphs without descenders end on the same pixel row.
        let ctx = egui::Context::default();
        let geometry = RowGeometry::new(16.0, RowLayout::Wide, &MIXED_ITEMS);
        let mut measured: Vec<(f32, f32, f32, f32)> = Vec::new();
        drop(ctx.run_ui(egui::RawInput::default(), |ui| {
            let baselines = RowBaselines::measure(ui, 16.0, geometry);
            // One text row in `Wide`, so both galleys go on the very same baseline.
            assert!((baselines.primary - baselines.secondary).abs() < f32::EPSILON);
            let baseline = baselines.primary;
            let main = main_line_galley(ui, geometry, 16.0, FontFamily::Proportional);
            let second = layout_line(
                ui,
                "Second",
                FontId::new(geometry.secondary_size, FontFamily::Proportional),
                Some(secondary_galley_line_height(geometry.secondary_size)),
                LineColors::plain(Color32::GRAY),
                &[],
                1000.0,
            );
            if let (Some(main_in_galley), Some(second_in_galley)) =
                (galley_baseline(&main), galley_baseline(&second))
            {
                measured.push((
                    galley_top_for_baseline(&main, baseline) + main_in_galley,
                    galley_top_for_baseline(&second, baseline) + second_in_galley,
                    galley_top_for_baseline(&main, baseline),
                    galley_top_for_baseline(&second, baseline),
                ));
            }
        }));
        assert_eq!(measured.len(), 1, "both lines must produce a measurable row");
        let (main_landed, second_landed, main_top, second_top) = measured[0];
        // One baseline for both.
        assert!((main_landed - second_landed).abs() < 1e-3);
        // The smaller line is therefore pushed DOWN relative to the main one; a shared top
        // edge — the bug the user reported as "ниже основной" the other way round — would put
        // it above instead.
        assert!(second_top > main_top);
    }

    #[test]
    fn searchable_combo_tall_baselines_do_not_follow_the_item_face() {
        // The same defect `Wide` had, in the layout that is the DEFAULT: the main line used to
        // be painted at the top of its band, so its baseline was the item face's own ascent —
        // 15 pt for the interface font against 24 pt for the decorative face below, both at a
        // nominal 16 pt. Rows kept their height and their ink floated.
        let ctx = context_with_decorative_face(1.6);
        let geometry = RowGeometry::new(16.0, RowLayout::Tall, &MIXED_ITEMS);
        let mut measured: Vec<(f32, f32, f32)> = Vec::new();
        drop(ctx.run_ui(egui::RawInput::default(), |ui| {
            let baseline = RowBaselines::measure(ui, 16.0, geometry).primary;
            for family in [FontFamily::Proportional, FontFamily::Name("decorative".into())] {
                let galley = main_line_galley(ui, geometry, 16.0, family);
                if let Some(in_galley) = galley_baseline(&galley) {
                    measured.push((
                        galley.size().y,
                        in_galley,
                        galley_top_for_baseline(&galley, baseline) + in_galley,
                    ));
                }
            }
            measured.push((baseline, baseline, baseline));
        }));
        assert_eq!(measured.len(), 3, "both faces must produce a measurable row");
        let (interface_height, interface_in_galley, interface_landed) = measured[0];
        let (decorative_height, decorative_in_galley, decorative_landed) = measured[1];
        let (baseline, _, _) = measured[2];

        // The main line's band is the same height for both faces, as it always was.
        assert!((interface_height - decorative_height).abs() < f32::EPSILON);
        assert!((interface_height - geometry.primary_line_height).abs() <= 1.0);
        // The faces disagree about their own baseline...
        assert!(
            (interface_in_galley - decorative_in_galley).abs() > 1.0,
            "the decorative face must have a different ascent for this test to mean anything"
        );
        // ...and are placed on the row's single one regardless.
        assert!((interface_landed - baseline).abs() < 1e-3);
        assert!((decorative_landed - baseline).abs() < 1e-3);
    }

    #[test]
    fn searchable_combo_tall_second_line_baseline_sits_in_its_own_band() {
        // `Tall` gives each line a band of its own, and each baseline is measured from the
        // INTERFACE font at that band's nominal size — 16 pt over 8 pt here.
        let ctx = egui::Context::default();
        let mut measured: Vec<(f32, RowBaselines)> = Vec::new();
        drop(ctx.run_ui(egui::RawInput::default(), |ui| {
            for size in [14.0_f32, 16.0] {
                let geometry = RowGeometry::new(size, RowLayout::Tall, &MIXED_ITEMS);
                measured.push((size, RowBaselines::measure(ui, size, geometry)));
            }
        }));
        assert_eq!(measured.len(), 2, "both sizes must be measured");

        // At the widget's default size 14: bands of 22.4 and 8.75; the interface font measures
        // {ascent 13, line 16} at 14 pt and {ascent 7, line 8} at 7 pt.
        let (_, at_14) = measured[0];
        assert!((at_14.primary - 16.2).abs() < 1e-3);
        assert!((at_14.secondary - 29.775).abs() < 1e-3);
        // At 16 pt: bands of 25.6 and 10; {15, 18} at 16 pt and {7, 9} at 8 pt.
        let (_, at_16) = measured[1];
        assert!((at_16.primary - 18.8).abs() < 1e-3);
        assert!((at_16.secondary - 33.1).abs() < 1e-3);

        // The second line's baseline lands inside its OWN band, under the main line's, and the
        // main line's ink still has room for a descender before that band starts.
        let geometry = RowGeometry::new(16.0, RowLayout::Tall, &MIXED_ITEMS);
        let band_top = geometry.primary_line_height;
        let band_bottom = band_top + geometry.secondary_line_height;
        assert!(at_16.secondary > band_top && at_16.secondary < band_bottom);
        assert!(at_16.primary < band_top);
    }

    #[test]
    fn searchable_combo_wide_keeps_the_highlight_in_both_lines() {
        // Both lines are separate galleys now, so highlighting is proved on each of them:
        // "Nar[ez]ka" and "Nar[ez]kaRegular" must each come out as three sections with the
        // middle one in the highlight colour.
        let ctx = egui::Context::default();
        let geometry = RowGeometry::new(16.0, RowLayout::Wide, &MIXED_ITEMS);
        let highlight = HIGHLIGHT_ON_DARK;
        let colors = LineColors {
            base: Color32::WHITE,
            highlight,
        };
        let mut sections: Vec<Vec<Color32>> = Vec::new();
        drop(ctx.run_ui(egui::RawInput::default(), |ui| {
            for (text, size) in [("Narezka", 16.0), ("NarezkaRegular", geometry.secondary_size)] {
                let ranges = matching::match_ranges(text, "ez");
                let galley = layout_line(
                    ui,
                    text,
                    FontId::new(size, FontFamily::Proportional),
                    Some(geometry.primary_line_height),
                    colors,
                    &ranges,
                    1000.0,
                );
                sections.push(
                    galley
                        .job
                        .sections
                        .iter()
                        .map(|section| section.format.color)
                        .collect(),
                );
            }
        }));
        assert_eq!(sections.len(), 2, "both lines must be laid out");
        for colors in &sections {
            assert_eq!(
                colors,
                &vec![Color32::WHITE, highlight, Color32::WHITE],
                "the matched run must be the only highlighted section"
            );
        }
    }

    /// A caller's per-row diagnostic colour — the typing tab's "this font only partly covers
    /// the language" yellow (`tabs::typing::panel::create_presets::FONT_DIAGNOSTIC_WARNING_COLOR`).
    const ITEM_COLOR: Color32 = Color32::from_rgb(240, 200, 60);

    /// Lays "Narezka" out the way a popup row's main line is laid out, with `ez` matched, and
    /// returns the colour of every `LayoutJob` section it produced.
    fn main_line_section_colors(colors: LineColors) -> Vec<Color32> {
        let ctx = egui::Context::default();
        let geometry = RowGeometry::new(16.0, RowLayout::Tall, &MIXED_ITEMS);
        let mut sections: Vec<Color32> = Vec::new();
        drop(ctx.run_ui(egui::RawInput::default(), |ui| {
            let ranges = matching::match_ranges("Narezka", "ez");
            let galley = layout_line(
                ui,
                "Narezka",
                FontId::new(16.0, FontFamily::Proportional),
                Some(geometry.primary_line_height),
                colors,
                &ranges,
                1000.0,
            );
            sections = galley
                .job
                .sections
                .iter()
                .map(|section| section.format.color)
                .collect();
        }));
        sections
    }

    #[test]
    fn searchable_combo_item_color_leaves_the_match_highlighted() {
        // The whole point of the per-row colour: it takes over the UNMATCHED characters and
        // stops at the match, so a coloured row still shows where the query hit.
        let colors = LineColors::primary_row(Color32::WHITE, Some(ITEM_COLOR), HIGHLIGHT_ON_DARK);
        assert_eq!(colors.base, ITEM_COLOR);
        assert_eq!(colors.highlight, HIGHLIGHT_ON_DARK);
        assert_eq!(
            main_line_section_colors(colors),
            vec![ITEM_COLOR, HIGHLIGHT_ON_DARK, ITEM_COLOR],
            "the matched run must keep the search highlight on a coloured row"
        );
        // The colour is the ROW's, not the state's: it wins over every state colour, while the
        // fill behind the row — which `draw_rows` picks separately — still marks the selection.
        for state_color in [
            Color32::WHITE,
            Color32::from_gray(160),
            Color32::from_rgb(0, 0, 0),
        ] {
            let colors =
                LineColors::primary_row(state_color, Some(ITEM_COLOR), HIGHLIGHT_ON_LIGHT);
            assert_eq!(colors.base, ITEM_COLOR);
            assert_eq!(colors.highlight, HIGHLIGHT_ON_LIGHT);
        }
    }

    #[test]
    fn searchable_combo_row_without_item_color_is_unchanged() {
        // `None` must reproduce exactly what the widget painted before per-row colours
        // existed: the row state's own colour on the unmatched characters.
        let state_color = Color32::from_gray(200);
        let colors = LineColors::primary_row(state_color, None, HIGHLIGHT_ON_DARK);
        assert_eq!(colors.base, state_color);
        assert_eq!(colors.highlight, HIGHLIGHT_ON_DARK);
        assert_eq!(
            main_line_section_colors(colors),
            vec![state_color, HIGHLIGHT_ON_DARK, state_color]
        );
        // Byte for byte the pre-existing construction.
        let before = LineColors {
            base: state_color,
            highlight: HIGHLIGHT_ON_DARK,
        };
        assert_eq!(
            main_line_section_colors(colors),
            main_line_section_colors(before)
        );
    }

    #[test]
    fn searchable_combo_items_start_with_neither_mark() {
        // A caller that says nothing gets exactly the row it got before the two options
        // existed — the contract the existing call sites and tests rest on.
        for item in [
            SearchableComboItem::new("Narezka"),
            SearchableComboItem::with_secondary("Narezka", "NarezkaRegular"),
        ] {
            assert_eq!(item.primary_color, None);
            assert_eq!(item.tooltip, None);
        }
        let marked = SearchableComboItem::with_secondary("Narezka", "NarezkaRegular")
            .primary_color(ITEM_COLOR)
            .tooltip("The font does not cover the typesetting language.");
        assert_eq!(marked.primary, "Narezka");
        assert_eq!(marked.secondary, Some("NarezkaRegular"));
        assert_eq!(marked.primary_color, Some(ITEM_COLOR));
        assert_eq!(
            marked.tooltip,
            Some("The font does not cover the typesetting language.")
        );
    }

    #[test]
    fn searchable_combo_empty_tooltip_shows_nothing() {
        assert_eq!(row_tooltip(None), None);
        // An empty tooltip is a bubble with nothing in it, so it counts as no tooltip.
        assert_eq!(row_tooltip(Some("")), None);
        assert_eq!(row_tooltip(Some("why")), Some("why"));
    }

    #[test]
    fn searchable_combo_highlight_color_follows_the_row_background() {
        assert_eq!(
            match_highlight_color(Color32::from_rgb(0, 92, 128)),
            HIGHLIGHT_ON_DARK
        );
        assert_eq!(match_highlight_color(Color32::from_gray(27)), HIGHLIGHT_ON_DARK);
        assert_eq!(
            match_highlight_color(Color32::from_rgb(144, 209, 255)),
            HIGHLIGHT_ON_LIGHT
        );
        assert_eq!(
            match_highlight_color(Color32::from_gray(248)),
            HIGHLIGHT_ON_LIGHT
        );
    }
}
