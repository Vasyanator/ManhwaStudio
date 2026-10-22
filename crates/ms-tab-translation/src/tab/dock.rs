/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/dock.rs
The Translation tab's panel dock: its default arrangement, the per-frame dock run and the tab
bodies this tab owns — «Последние персонажи», the «Инструменты» toolbar and the five tools.

Main items:
- `TRANSLATION_RECENT_CHARACTERS_TAB`: the stable dock tab id of «Последние персонажи» (the
  toolbar and tool ids live with the tools, `tab/tool_panels.rs`).
- `translation_default_dock_layout`: this tab's OWN default arrangement (re-exported by `tab.rs`
  for `host.rs`), the dictionary stored tab keys are resolved against: «Лента», «Последние
  персонажи», the toolbar at the bottom-right and ONE panel of the five tools above it.
- `TranslationDockCx`: the per-frame context the dock hands to one tab body at a time.
- `draw_recent_characters_tab_body` + the card helpers (`recent_character_card_caption`,
  `recent_character_card_size`, `recent_character_card_caption_pos`): the hand-painted,
  wrapping row of recent-character cards.
- `TranslationTabState::draw_panel_dock`: the dock frame, run at the end of
  `draw_canvas_overlay_top_left`; applies the tools' focus / persistence requests before
  `PanelDock::begin` and records `ribbon_panel_rect` for the next frame.
- `TranslationTabState::text_detector_edit_panel_pos`: places the detector edit boxes under the
  previous frame's «Лента» panel, with a fixed canvas-offset fallback.

Notes:
`draw_panel_dock` must run on every frame the tab is active — `PanelDock::end` is what keeps the
dock's detached sub-window viewports alive. GUI thread only; no I/O.
*/

use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use ms_canvas::{CANVAS_RIBBON_TAB, CanvasView};
use ms_config::app_tab::AppTab;
use ms_project::ProjectData;
use ms_widgets::panel_dock::{
    DockArea, DockEdge, DockLayout, HostId, PanelAnchor, PanelDock, PanelDockState, PanelId,
    PanelNode, TabId,
};

use super::TranslationTabState;
use super::tool_panels::{TOOL_TAB_INITIAL_SIZE_PX, TOOL_TAB_MIN_SIZE_PX, TRANSLATION_TOOLS_TAB, TranslationTool};

// Dock tabs of the «Перевод» program tab. The id is a stable, non-localised
// literal: it is the identity the persisted layout and every egui id of the
// owning panel derive from (`dev-docs/i18n_exclusions.md` §A9).
/// «Последние персонажи» — the row of cards naming the last
/// [`super::RECENT_CHARACTER_HISTORY_LIMIT`] characters, one per recent-character parameter mode
/// (`crate::input::RECENT_CHARS`).
const TRANSLATION_RECENT_CHARACTERS_TAB: TabId = TabId::new("translation.recent_characters");

/// Gap, in points, between two recent-character cards on both axes.
const RECENT_CHARACTER_CARD_SPACING_PX: f32 = 8.0;
/// Points between a recent-character card's border and its caption, horizontally.
const RECENT_CHARACTER_CARD_MARGIN_X_PX: f32 = 10.0;
/// Points between a recent-character card's border and its caption, vertically.
const RECENT_CHARACTER_CARD_MARGIN_Y_PX: f32 = 6.0;
/// Width, in points, of a recent-character card's border.
const RECENT_CHARACTER_CARD_STROKE_WIDTH_PX: f32 = 1.0;
/// Corner radius, in points, of a recent-character card.
const RECENT_CHARACTER_CARD_CORNER_RADIUS_PX: u8 = 8;

/// Smallest outer size, in points, the dock may shrink the «Последние персонажи»
/// panel to.
///
/// A `min_size` is a shrink FLOOR, not a request (`panel_dock/MODULE_README.md`),
/// so it only has to keep ONE card and the panel's grip reachable — the body
/// scrolls below that, and the floor is deliberately far under
/// [`TRANSLATION_RECENT_CHARACTERS_TAB_INITIAL_SIZE_PX`] so a user who narrows the
/// panel is not fought by the solver. Roughly one card (~120 pt of caption plus its
/// margins) plus the panel's own ~16 pt of chrome, and a header strip plus
/// `PANEL_MIN_BODY_HEIGHT`.
const TRANSLATION_RECENT_CHARACTERS_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(150.0, 70.0);
/// Outer size, in points, the «Последние персонажи» panel starts at.
///
/// The WIDTH is the number that matters and it is a COMPROMISE. The dock never
/// re-measures a tab's width — it reports back the width it was GIVEN
/// (`panel.rs::measured_size`) — so 560 pt stays the panel's width until the user
/// drags the resize grip, whereas the height converges to the content on the second
/// frame and only decides what the FIRST frame looks like. Before the migration the
/// row of cards ran across almost the whole canvas in a single line; a narrow panel
/// would change that look far more than the migration was meant to. 560 pt is the
/// balance: ~544 pt of content once the panel's ~16 pt of chrome is taken off,
/// against a card of ~110–160 pt plus [`RECENT_CHARACTER_CARD_SPACING_PX`], so the
/// six-card history reads in 2–3 rows while the panel still leaves most of the
/// canvas free. The height covers three such rows, the worst case at that width.
const TRANSLATION_RECENT_CHARACTERS_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(560.0, 154.0);

/// Builds the default dock arrangement of the «Перевод» program tab.
///
/// Four panels. The canvas' own «Лента» flush with the LEFT edge of the dock area —
/// the same anchor, panel id and content sizing every canvas program tab gives it,
/// so a user who already arranged the ribbon keeps it where they left it — and
/// «Последние персонажи» docked to its RIGHT, top-aligned. That is exactly where
/// the row of cards floated before it became a dock tab: it hung at a fixed
/// 340 pt offset from the canvas' left edge, a constant picked to clear the width
/// of the canvas controls panel the ribbon replaced. The «Инструменты» toolbar flush
/// with the BOTTOM edge at its right end — where the floating tools bar sat — and ONE
/// panel holding the five tool tabs docked on TOP of the toolbar, right-aligned with
/// it, so the tools open just above the buttons that toggle them and the solver
/// shrinks the two together (one chain) instead of laying the tools over the bar. The
/// five tools sharing one default panel is also what makes a user whose stored layout
/// predates them get them as ONE tabbed panel: the dock seeds missing tabs that share
/// a default panel into one panel.
///
/// The panels carry different `target` + `edge` + `align` triples, which is
/// what keeps the solver — a total function of whatever layout it is given — from
/// laying one exactly on top of the other.
///
/// This tab needs a builder of ITS own because the default layout doubles as the
/// DICTIONARY the persistence layer resolves stored tab keys against: a `TabId`
/// missing from it is dropped from the user's arrangement on every load
/// (`panel_dock/persist.rs::known_tabs`).
///
/// Used only when no layout exists yet for this program tab; a restored one always
/// wins. Handed to the app-owned dock state as a plain `fn` pointer, both when the
/// persisted layouts are restored before the first frame (`studio.rs`) and by
/// `ensure_default_layout` on every frame this tab draws — the two must name the
/// same item, since the header's «сбросить раскладку» rebuilds from the one the
/// state kept. A model refusal is logged and skipped, never panicked on: the dock
/// then creates a panel for the orphaned tab on its own, which is a degraded
/// arrangement rather than a lost tab.
#[must_use]
pub fn translation_default_dock_layout() -> DockLayout {
    let mut layout = DockLayout::new();
    let ribbon = PanelId::new(0);
    let toolbar = PanelId::new(2);
    let panels = [
        // The ribbon is inserted FIRST because the cards anchor to it, and
        // `insert_panel` rejects an anchor whose target does not exist yet.
        (
            ribbon,
            vec![CANVAS_RIBBON_TAB],
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0,
            },
        ),
        (
            PanelId::new(1),
            vec![TRANSLATION_RECENT_CHARACTERS_TAB],
            PanelAnchor::Panel {
                target: ribbon,
                edge: DockEdge::Right,
                align: 0.0,
            },
        ),
        // The toolbar is inserted before the tools panel for the same reason.
        (
            toolbar,
            vec![TRANSLATION_TOOLS_TAB],
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Bottom,
                along: 1.0,
            },
        ),
        (
            PanelId::new(3),
            TranslationTool::ALL.iter().map(|tool| tool.tab_id()).collect(),
            PanelAnchor::Panel {
                target: toolbar,
                edge: DockEdge::Top,
                align: 1.0,
            },
        ),
    ];
    for (id, tabs, anchor) in panels {
        let node = match PanelNode::new(id, HostId::MainWindow, tabs) {
            Ok(mut node) => {
                node.anchor = anchor;
                node
            }
            Err(error) => {
                ms_log::runtime_log::log_warn(format!(
                    "[translation] default dock layout: could not build panel {id} ({error}); \
                     the dock will create one per orphaned tab on its own"
                ));
                continue;
            }
        };
        if let Err(error) = layout.insert_panel(node) {
            ms_log::runtime_log::log_warn(format!(
                "[translation] default dock layout: could not insert panel {id} ({error}); \
                 the dock will create one per orphaned tab on its own"
            ));
        }
    }
    layout
}

/// Per-frame context the panel dock hands to one «Перевод» tab body at a time.
///
/// The bodies capture nothing of their own: «Лента» needs the shared `CanvasView`
/// (the hook's own parameter, borrow-independent of the tab state the hook was
/// called on), the tool bodies need the whole tab state mutably plus the canvas and
/// the project, «Последние персонажи» reads the tab's history. Closures capturing
/// those borrows directly could not coexist in the dock's queue; exclusive,
/// sequential access through this context lets every body reach exactly what it
/// needs. Built only AFTER every tab is declared, so no declaration (title closure,
/// size) may borrow the state.
///
/// None of the fields is the `PanelDockState` the dock itself borrows for the frame.
struct TranslationDockCx<'a> {
    /// The tab state: the tool bodies act on it, «Последние персонажи» reads it.
    state: &'a mut TranslationTabState,
    /// Owner of the «Лента» body; also handed to the tool bodies.
    canvas: &'a mut CanvasView,
    /// The open project, for the tool bodies.
    project: &'a ProjectData,
    /// Project page count, shown by the «Лента» tab's page counter.
    total_pages: usize,
}

impl TranslationDockCx<'_> {
    /// The accessor `canvas::declare_ribbon_tab` asks for, as a plain `fn` item the
    /// caller can hand over instead of spelling the closure out.
    fn ribbon_body(&mut self) -> (&mut CanvasView, usize) {
        (&mut *self.canvas, self.total_pages)
    }
}

/// Caption of a recent-character card: the binding text of the rank's parameter mode
/// (`crate::input::RECENT_CHARS`, "1" by default for the first card), then the name.
#[must_use]
fn recent_character_card_caption(key_label: &str, character_name: &str) -> String {
    format!("[ {key_label} ] {character_name}")
}

/// Outer size, in points, of a recent-character card whose caption laid out to
/// `caption_size`.
///
/// The caption plus the inner margin plus the border on all four sides, which is
/// exactly what `egui::Frame` allocated for the same card before: its
/// `outer_rect` is `content_rect + inner_margin + Margin::from(stroke.width) +
/// outer_margin`, and the outer margin is zero here
/// (`egui-0.35.0/src/containers/frame.rs:349-352`, `:456-462`).
#[must_use]
fn recent_character_card_size(caption_size: Vec2) -> Vec2 {
    caption_size
        + Vec2::new(
            2.0 * (RECENT_CHARACTER_CARD_MARGIN_X_PX + RECENT_CHARACTER_CARD_STROKE_WIDTH_PX),
            2.0 * (RECENT_CHARACTER_CARD_MARGIN_Y_PX + RECENT_CHARACTER_CARD_STROKE_WIDTH_PX),
        )
}

/// Top-left corner the caption is painted at inside a card laid out at `card_rect`.
///
/// The inverse of [`recent_character_card_size`]: `Frame` places the content rect
/// one inner margin and one border in from the card's own corner.
#[must_use]
fn recent_character_card_caption_pos(card_rect: Rect) -> Pos2 {
    card_rect.min
        + Vec2::new(
            RECENT_CHARACTER_CARD_MARGIN_X_PX + RECENT_CHARACTER_CARD_STROKE_WIDTH_PX,
            RECENT_CHARACTER_CARD_MARGIN_Y_PX + RECENT_CHARACTER_CARD_STROKE_WIDTH_PX,
        )
}

/// Draws the «Последние персонажи» tab body: one card per remembered character,
/// captioned with the digit that activates it, wrapping onto further rows at the
/// panel's own width.
///
/// The cards are not interactive and this is deliberate — a character is picked by
/// holding its recent-character binding together with the bubble-create command or the quick OCR
/// gesture, never by clicking the card. The body only reads the tab's
/// `recent_characters` history; the allocation senses `hover` and
/// nothing else, exactly as `Frame::allocate_space` did
/// (`egui-0.35.0/src/containers/frame.rs:466-468`), and nothing is painted from
/// the response, so there is no hover highlight and no click target.
///
/// **A card may NOT be an `egui::Frame`, and that is the whole reason this function
/// paints by hand.** `Frame::begin` takes its bounds from
/// `Ui::available_rect_before_wrap` and reserves the space only afterwards, through
/// `Prepared::allocate_space` → `Ui::allocate_rect` → `advance_cursor_after_rect`
/// (`frame.rs:378-397`, `:466-468`, `egui-0.35.0/src/ui.rs:1256-1270`). That path
/// calls `Placer::advance_after_rects` DIRECTLY and never reaches
/// `Placer::next_space`, which is where `Layout::next_frame` starts a new row
/// (`egui-0.35.0/src/placer.rs:113-125`, `src/layout.rs:506-528`). A framed card
/// therefore cannot wrap: the whole history ran off the right edge of the panel
/// and the body grew a horizontal scrollbar. `egui::Button` wraps in
/// `cleaning::tab::draw_tool_button_rows` for the opposite reason — it measures
/// itself first and allocates through `Ui::allocate_exact_size`.
///
/// So the caption is laid out FIRST (never wrapped, so a character name can never
/// break across two lines), its card size is derived from the galley, that exact
/// size goes through `allocate_exact_size` → `allocate_space` → `next_space`, and
/// only then are the background, the border and the text painted into the rect the
/// layout handed back. `Layout::next_frame` breaks a row only while
/// `max_rect.left() < cursor.left()`, so a card wider than the whole panel still
/// gets a row of its own rather than an empty one above it (`layout.rs:517`).
///
/// The look is unchanged: `StrokeKind::Inside` on the allocated rect reproduces
/// `Frame::paint`, which draws its `RectShape` with the same `StrokeKind::Inside`
/// on `widget_rect = content_rect + inner_margin + stroke.width`
/// (`frame.rs:423-445`), and the galley comes from the same `FontSelection::Default`
/// the removed `ui.label` resolved through (`egui-0.35.0/src/widgets/label.rs:173`).
fn draw_recent_characters_tab_body(ui: &mut egui::Ui, cx: &mut TranslationDockCx<'_>) {
    let fill = Color32::from_rgba_unmultiplied(110, 110, 110, 170);
    let stroke = Stroke::new(
        RECENT_CHARACTER_CARD_STROKE_WIDTH_PX,
        Color32::from_rgba_unmultiplied(255, 255, 255, 44),
    );
    let caption_color = Color32::from_rgb(235, 235, 235);
    let state = &*cx.state;
    let recent_char_keys = &state.input_labels.recent_char_keys;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(RECENT_CHARACTER_CARD_SPACING_PX);
        for (rank, entry) in state.recent_characters.iter().enumerate() {
            // `Extend` with an infinite width: the card is sized from this galley, so
            // the name must be measured as the single line it will be drawn as.
            let caption = egui::WidgetText::from(
                egui::RichText::new(recent_character_card_caption(recent_char_keys.get(rank).map_or("", String::as_str), &entry.character_name))
                    .color(caption_color),
            )
            .into_galley(
                ui,
                Some(egui::TextWrapMode::Extend),
                f32::INFINITY,
                egui::FontSelection::Default,
            );
            let (card_rect, _) = ui.allocate_exact_size(
                recent_character_card_size(caption.size()),
                egui::Sense::hover(),
            );
            let painter = ui.painter();
            painter.rect(
                card_rect,
                egui::CornerRadius::same(RECENT_CHARACTER_CARD_CORNER_RADIUS_PX),
                fill,
                stroke,
                egui::StrokeKind::Inside,
            );
            // The colour is already baked into the galley by `RichText::color`, so the
            // fallback only ever applies to a `Color32::PLACEHOLDER` section there is none of.
            painter.galley(
                recent_character_card_caption_pos(card_rect),
                caption,
                caption_color,
            );
        }
    });
}

impl TranslationTabState {
    /// Top-left corner of the text-detector edit boxes: just under the dock panel
    /// holding the «Лента» tab, as they sat under the canvas controls panel before
    /// that panel became a dock tab.
    ///
    /// Uses the PREVIOUS frame's rect ([`TranslationTabState::ribbon_panel_rect`]) —
    /// these boxes are drawn before the dock runs — and falls back to a fixed offset
    /// inside `canvas_rect` whenever the panel is not on screen in this window
    /// (hidden, or detached into a sub-window), which is the same fallback the
    /// `Memory::area_rect` read had.
    #[must_use]
    pub(super) fn text_detector_edit_panel_pos(&self, canvas_rect: Rect) -> Pos2 {
        self.ribbon_panel_rect
            .map(|rect| rect.left_bottom() + egui::vec2(0.0, 8.0))
            .unwrap_or_else(|| canvas_rect.left_top() + egui::vec2(12.0, 112.0))
    }

    /// Runs the «Перевод» tab's panel dock — the canvas' own «Лента» tab, this
    /// tab's «Последние персонажи», the «Инструменты» toolbar and the five tool
    /// tabs — and records the ribbon panel's rect for the next frame.
    ///
    /// Before `PanelDock::begin` it applies the tool switches of past frames: focus
    /// of a newly switched-on tool's tab and the on-set's persistence
    /// (`apply_tool_dock_requests`). Called at the very END of
    /// `draw_canvas_overlay_top_left`; see the call site for why that position is
    /// load-bearing.
    pub(super) fn draw_panel_dock(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &mut CanvasView,
        project: &ProjectData,
        total_pages: usize,
        panel_dock: &mut PanelDockState,
    ) {
        // The SAME `fn` item `studio.rs::restore_panel_dock` registers: the dock state
        // keeps it to rebuild the arrangement for the header's reset item, so two
        // different builders here would reset to something the user never saw.
        panel_dock.ensure_default_layout(
            AppTab::Translation.key(),
            translation_default_dock_layout,
        );
        // After the layout exists (a focus needs the tool tab's panel) and before `begin`
        // (which holds the state borrow).
        self.apply_tool_dock_requests(panel_dock);
        let recent_characters_visible = !self.recent_characters.is_empty();
        let (tools_min_size, tools_initial_size) = self.tools_tab_sizes(ctx);
        let tools = self.tools;
        let mut dock = PanelDock::begin(
            ctx,
            panel_dock,
            DockArea {
                rect: ms_canvas::dock_area_rect(canvas_rect),
                layout_key: AppTab::Translation.key(),
            },
        );
        ms_canvas::declare_ribbon_tab(&mut dock, TranslationDockCx::ribbon_body);
        // Declared on EVERY frame, visible only while the history has entries: a
        // hidden tab keeps its slot in the layout and only its panel is skipped, so
        // the first character of a fresh chapter brings the cards back to wherever
        // the user put them. Skipping the declaration instead would make the dock
        // treat the tab as another program tab's and seed it a fresh panel next time.
        //
        // Transparent until hovered: without the pointer on it the panel paints no
        // frame, border, arrow or grip, so the cards float on the canvas exactly as
        // they did as a bare `Area`. Hovering fades the chrome in and makes the panel
        // draggable and resizable like any other.
        dock.tab(TRANSLATION_RECENT_CHARACTERS_TAB)
            .title(|| t!("translation.tab.recent_characters_tab"))
            .visible(recent_characters_visible)
            .transparent_until_hover(true)
            .min_size(TRANSLATION_RECENT_CHARACTERS_TAB_MIN_SIZE_PX)
            .initial_size(TRANSLATION_RECENT_CHARACTERS_TAB_INITIAL_SIZE_PX)
            .show(draw_recent_characters_tab_body);
        // Always shown: it is the only way back to a tool that was switched off. Its
        // width is measured from the captions every frame — a dock panel never
        // re-measures its own width, so a bar started too narrow would stay wrapped.
        dock.tab(TRANSLATION_TOOLS_TAB)
            .title(|| t!("translation.tab.tools_heading"))
            .min_size(tools_min_size)
            .initial_size(tools_initial_size)
            .show(|ui, cx: &mut TranslationDockCx<'_>| cx.state.draw_tools_bar_body(ui));
        // Declared on EVERY frame, visible only while switched on, for the same reason
        // as «Последние персонажи»: a hidden tab keeps its panel, position, detached
        // window and size. Whether a tool is on is the tab STATE (`tools`), never this
        // frame's dock output.
        for tool in TranslationTool::ALL {
            dock.tab(tool.tab_id())
                .title(move || tool.title())
                .visible(tools.is_on(tool))
                .min_size(TOOL_TAB_MIN_SIZE_PX)
                .initial_size(TOOL_TAB_INITIAL_SIZE_PX)
                .show(move |ui, cx: &mut TranslationDockCx<'_>| {
                    cx.state.draw_tool_body(tool, ui, &mut *cx.canvas, cx.project);
                });
        }
        let mut cx = TranslationDockCx {
            state: self,
            canvas,
            project,
            total_pages,
        };
        // `tab_rect` answers for the MAIN window alone: it is `None` while the tab is
        // hidden AND while the user keeps it in a detached sub-window, whose rect lives
        // in that window's own coordinate frame. Both are "unknown" to the edit boxes.
        let ribbon_panel_rect = dock.end(&mut cx).tab_rect(CANVAS_RIBBON_TAB);
        self.ribbon_panel_rect = ribbon_panel_rect;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RECENT_CHARACTER_CARD_MARGIN_X_PX, RECENT_CHARACTER_CARD_MARGIN_Y_PX,
        RECENT_CHARACTER_CARD_SPACING_PX, RECENT_CHARACTER_CARD_STROKE_WIDTH_PX,
        TRANSLATION_RECENT_CHARACTERS_TAB, TRANSLATION_RECENT_CHARACTERS_TAB_INITIAL_SIZE_PX,
        TRANSLATION_RECENT_CHARACTERS_TAB_MIN_SIZE_PX, TranslationTabState, egui,
        recent_character_card_caption, recent_character_card_caption_pos,
        recent_character_card_size, translation_default_dock_layout,
    };
    use crate::tab::tool_panels::{TOOL_TAB_INITIAL_SIZE_PX, TOOL_TAB_MIN_SIZE_PX, TRANSLATION_TOOLS_TAB, TranslationTool};
    use egui::{Pos2, Rect, Vec2};
    use ms_canvas::{
        CANVAS_RIBBON_TAB, CANVAS_RIBBON_TAB_INITIAL_SIZE_PX, CANVAS_RIBBON_TAB_MIN_SIZE_PX,
    };
    use ms_widgets::panel_dock::{
        DockEdge, HostId, PanelAnchor, PanelChrome, PanelId, PanelSizes, solve,
    };

    /// A card shows the binding of its rank's parameter mode, then the name.
    #[test]
    fn a_recent_character_card_is_captioned_with_its_own_binding() {
        assert_eq!(recent_character_card_caption("1", "Аня"), "[ 1 ] Аня");
        assert_eq!(recent_character_card_caption("F6", "Bob"), "[ F6 ] Bob");
        // A missing name still leaves the binding legible — the card is the only
        // place that says which key is bound to what.
        assert_eq!(recent_character_card_caption("3", ""), "[ 3 ] ");
    }

    /// The two card helpers are each other's inverse, and nothing but their
    /// agreement keeps the caption centred: the size is what the layout allocates
    /// and the position is where the text is painted inside it, so a margin changed
    /// in one and not the other slides the text off the card.
    ///
    /// The inset reproduces `egui::Frame` exactly — one inner margin plus one border
    /// on every side (`egui-0.35.0/src/containers/frame.rs:349-352`), which is what
    /// makes the hand-painted card look identical to the framed one it replaced.
    #[test]
    fn a_recent_character_card_insets_its_caption_equally_on_every_side() {
        let caption = Vec2::new(97.0, 18.0);
        let size = recent_character_card_size(caption);
        assert_eq!(
            size,
            caption
                + Vec2::new(
                    2.0 * (RECENT_CHARACTER_CARD_MARGIN_X_PX
                        + RECENT_CHARACTER_CARD_STROKE_WIDTH_PX),
                    2.0 * (RECENT_CHARACTER_CARD_MARGIN_Y_PX
                        + RECENT_CHARACTER_CARD_STROKE_WIDTH_PX),
                )
        );

        let card = Rect::from_min_size(Pos2::new(30.0, 40.0), size);
        let drawn = Rect::from_min_size(recent_character_card_caption_pos(card), caption);
        assert!(
            card.contains_rect(drawn),
            "the caption {drawn:?} sticks out of its card {card:?}"
        );
        assert_eq!(
            drawn.left() - card.left(),
            card.right() - drawn.right(),
            "the caption is not horizontally centred in the card"
        );
        assert_eq!(
            drawn.top() - card.top(),
            card.bottom() - drawn.bottom(),
            "the caption is not vertically centred in the card"
        );
        assert_eq!(
            drawn.left() - card.left(),
            RECENT_CHARACTER_CARD_MARGIN_X_PX + RECENT_CHARACTER_CARD_STROKE_WIDTH_PX
        );
        assert_eq!(
            drawn.top() - card.top(),
            RECENT_CHARACTER_CARD_MARGIN_Y_PX + RECENT_CHARACTER_CARD_STROKE_WIDTH_PX
        );
    }

    /// A card has to fit the panel the tab STARTS at, or the very first frame the
    /// user sees is a single card with a horizontal scrollbar under it. The floor
    /// must not fight it either: `min_size` only stops the shrink, so it has to stay
    /// well under the start size, and it has to hold at least one card.
    #[test]
    fn the_start_size_fits_several_cards_and_the_floor_fits_one() {
        // A caption of ~13 characters at the body font — «[ 1 ] Персонаж».
        let card = recent_character_card_size(Vec2::new(100.0, 18.0));
        // The panel spends this much on its own frame margins and border before the
        // body gets any width (`Frame::popup`, 6 pt per side plus a 1 pt stroke).
        const PANEL_CHROME_WIDTH_PX: f32 = 16.0;

        let start_body = TRANSLATION_RECENT_CHARACTERS_TAB_INITIAL_SIZE_PX.x - PANEL_CHROME_WIDTH_PX;
        let per_row = ((start_body + RECENT_CHARACTER_CARD_SPACING_PX)
            / (card.x + RECENT_CHARACTER_CARD_SPACING_PX))
            .floor();
        // A band, not a number: the real card width is a font galley, so this only
        // pins that the start width is neither one-card-narrow (which is what the
        // pre-fix bug looked like) nor so wide the panel eats the canvas. Four fit at
        // this caption length; a name half again as long still leaves three.
        assert!(
            (2.0..=5.0).contains(&per_row),
            "the start width fits {per_row} cards per row, outside the 2-5 band the size targets"
        );

        let floor_body = TRANSLATION_RECENT_CHARACTERS_TAB_MIN_SIZE_PX.x - PANEL_CHROME_WIDTH_PX;
        assert!(
            floor_body >= card.x,
            "the shrink floor cannot hold one card: {floor_body} pt of body for a {} pt card",
            card.x
        );
        // Both operands are constants, so this is a compile-time check.
        const {
            assert!(
                TRANSLATION_RECENT_CHARACTERS_TAB_MIN_SIZE_PX.x
                    < TRANSLATION_RECENT_CHARACTERS_TAB_INITIAL_SIZE_PX.x
                    && TRANSLATION_RECENT_CHARACTERS_TAB_MIN_SIZE_PX.y
                        < TRANSLATION_RECENT_CHARACTERS_TAB_INITIAL_SIZE_PX.y,
                "the floor must stay under the start size or the solver fights a user who narrows \
                 the panel"
            );
        }
    }

    /// The default arrangement is what the dock is handed on a first run AND the
    /// dictionary `panel_dock::persist` resolves stored tab keys against, so it has
    /// to be well-formed and to name every tab this program tab can declare.
    #[test]
    fn the_default_dock_layout_places_the_translation_panels() {
        let layout = translation_default_dock_layout();
        assert_eq!(layout.validate(), Ok(()));
        assert_eq!(layout.panels().len(), 4);

        // «Лента» keeps the anchor and the id every canvas program tab gives it, so a
        // user who already arranged the ribbon finds their panel where they left it.
        let ribbon = layout
            .panel(PanelId::new(0))
            .expect("the ribbon panel exists");
        assert_eq!(ribbon.tabs, vec![CANVAS_RIBBON_TAB]);
        assert_eq!(
            ribbon.anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0,
            }
        );

        // The cards reproduce where the floating row sat: to the RIGHT of the ribbon,
        // aligned with its top edge.
        let recent = layout
            .panel(PanelId::new(1))
            .expect("the recent-characters panel exists");
        assert_eq!(recent.tabs, vec![TRANSLATION_RECENT_CHARACTERS_TAB]);
        assert_eq!(
            recent.anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Right,
                align: 0.0,
            }
        );
        assert_ne!(
            recent.anchor, ribbon.anchor,
            "two panels sharing a target+edge+align would be laid out on the same rect"
        );

        // The toolbar sits where the floating tools bar did: flush with the BOTTOM edge at
        // its right end.
        let toolbar = layout.panel(PanelId::new(2)).expect("the toolbar panel exists");
        assert_eq!(toolbar.tabs, vec![TRANSLATION_TOOLS_TAB]);
        assert_eq!(
            toolbar.anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Bottom,
                along: 1.0,
            }
        );

        // ONE panel holds all five tools, in toolbar order, docked on top of the toolbar and
        // right-aligned with it — one chain, so the solver shrinks the two together. Sharing a
        // default panel is also what seeds them as one panel into an older stored layout.
        let tools = layout.panel(PanelId::new(3)).expect("the tools panel exists");
        assert_eq!(
            tools.tabs,
            TranslationTool::ALL.iter().map(|tool| tool.tab_id()).collect::<Vec<_>>()
        );
        assert_eq!(
            tools.anchor,
            PanelAnchor::Panel {
                target: PanelId::new(2),
                edge: DockEdge::Top,
                align: 1.0,
            }
        );

        // Every panel is content-sized: its size comes from its tabs' own
        // declarations, and pinning one here would only make the first solve fight it.
        for id in (0..4).map(PanelId::new) {
            let panel = layout.panel(id).expect("a default panel exists");
            assert_eq!(panel.size_override, None, "{id} must stay content-sized");
        }

        // A `TabId` missing here is dropped from the user's stored arrangement on
        // every load (`panel_dock::persist::known_tabs`), so this list is the whole
        // set this program tab can declare.
        let declared = [CANVAS_RIBBON_TAB, TRANSLATION_RECENT_CHARACTERS_TAB, TRANSLATION_TOOLS_TAB]
            .into_iter()
            .chain(TranslationTool::ALL.iter().map(|tool| tool.tab_id()));
        for tab in declared {
            assert!(layout.panel_of_tab(tab).is_some(), "{tab} has no panel");
        }
    }

    /// Solving matters as much as being well-formed: the default arrangement is what
    /// the user SEES on a first run, and a panel laid out on top of another one is
    /// unreachable — the buried one cannot even be dragged out.
    ///
    /// Four frames are checked, because «Последние персонажи» and the tools panel are
    /// conditional: with and without history entries, with and without a tool switched
    /// on. `panel_dock::frame_layout` — built on `DockLayout::remove_panel` — drops a
    /// panel whose tabs are all hidden for the frame.
    #[test]
    fn the_default_dock_layout_solves_into_disjoint_panels() {
        // A maximised 1080p studio window: the canvas area of a 1920-wide window
        // less the page-manager column and the scrollbar reserve. Deliberately not
        // the largest possible area — the arrangement has to fit an ordinary window,
        // not only a wide one.
        let area = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1600.0, 1000.0));
        let without = |panels: &[u32]| {
            let mut layout = translation_default_dock_layout();
            for id in panels {
                layout
                    .remove_panel(PanelId::new(*id))
                    .expect("a conditional panel can be dropped for a frame");
            }
            layout
        };
        let everything = translation_default_dock_layout();
        let no_cards = without(&[1]);
        let no_tools = without(&[3]);
        let neither = without(&[1, 3]);
        for (frame, layout, expected_panels) in [
            ("history, a tool on", &everything, 4),
            ("no history, a tool on", &no_cards, 3),
            ("history, no tool on", &no_tools, 3),
            ("no history, no tool on", &neither, 2),
        ] {
            let solved = solve_default_layout(layout, area);
            assert_eq!(solved.len(), expected_panels, "{frame}: every panel is placed");
            for (id, rect) in &solved {
                assert!(
                    area.contains_rect(*rect),
                    "{frame}: panel {id} is laid out outside the dock area: {rect:?}"
                );
            }
            for (i, (id_a, rect_a)) in solved.iter().enumerate() {
                for (id_b, rect_b) in &solved[i + 1..] {
                    assert!(
                        !rect_a.intersects(*rect_b),
                        "{frame}: panels {id_a} and {id_b} overlap: {rect_a:?} vs {rect_b:?}"
                    );
                }
            }
        }
    }

    /// The toolbar's outer size as measured for five captions of ~10-20 characters at the
    /// default button font (`TranslationTabState::tools_tab_sizes` needs a live context).
    const TOOLBAR_TYPICAL_SIZE_PX: Vec2 = Vec2::new(620.0, 72.0);

    /// Solves `layout` inside `area` with the sizes the tabs declare (the toolbar at a
    /// typical measured width of its five buttons), and returns the drawn rects in panel
    /// order.
    fn solve_default_layout(
        layout: &ms_widgets::panel_dock::DockLayout,
        area: Rect,
    ) -> Vec<(PanelId, Rect)> {
        // What the first frame feeds the solver: each panel's tab's `initial_size`,
        // and its `min_size` as the floor.
        let sizes: PanelSizes = [
            (PanelId::new(0), CANVAS_RIBBON_TAB_INITIAL_SIZE_PX),
            (
                PanelId::new(1),
                TRANSLATION_RECENT_CHARACTERS_TAB_INITIAL_SIZE_PX,
            ),
            (PanelId::new(2), TOOLBAR_TYPICAL_SIZE_PX),
            (PanelId::new(3), TOOL_TAB_INITIAL_SIZE_PX),
        ]
        .into_iter()
        .collect();
        let mins: PanelSizes = [
            (PanelId::new(0), CANVAS_RIBBON_TAB_MIN_SIZE_PX),
            (
                PanelId::new(1),
                TRANSLATION_RECENT_CHARACTERS_TAB_MIN_SIZE_PX,
            ),
            (PanelId::new(2), Vec2::new(160.0, 64.0)),
            (PanelId::new(3), TOOL_TAB_MIN_SIZE_PX),
        ]
        .into_iter()
        .collect();
        let solved = solve(
            layout,
            HostId::MainWindow,
            area,
            &sizes,
            &mins,
            PanelChrome::default(),
        );
        let mut placed: Vec<(PanelId, Rect)> =
            solved.iter().map(|(id, panel)| (id, panel.rect)).collect();
        placed.sort_by_key(|(id, _)| *id);
        placed
    }

    /// The two text-detector edit boxes are drawn BEFORE the dock runs, so they position
    /// themselves from the previous frame's ribbon-panel rect and must degrade to a fixed
    /// offset inside the canvas whenever that panel was not on screen in this window.
    #[test]
    fn the_detector_edit_boxes_follow_the_ribbon_panel_and_have_a_fallback() {
        let canvas = Rect::from_min_max(Pos2::new(100.0, 50.0), Pos2::new(1100.0, 850.0));
        let mut tab = TranslationTabState::default();
        assert_eq!(
            tab.text_detector_edit_panel_pos(canvas),
            canvas.left_top() + egui::vec2(12.0, 112.0),
            "an unknown panel rect falls back inside the canvas"
        );

        let panel = Rect::from_min_size(Pos2::new(120.0, 60.0), egui::vec2(330.0, 160.0));
        tab.ribbon_panel_rect = Some(panel);
        assert_eq!(
            tab.text_detector_edit_panel_pos(canvas),
            panel.left_bottom() + egui::vec2(0.0, 8.0),
            "a known panel rect places the box one small gap under it"
        );
    }
}
