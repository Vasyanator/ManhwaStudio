/*
File: src/widgets/marked_scroll/mod.rs

Purpose:
Public API of the "marked scrollbar" widget: a vertical scroll area whose bar can
carry typed or freely drawn marks (under the handle) and gutter elements to the
left of the bar.

Architecture:
The host `egui::ScrollArea` is used purely as the scroll engine (wheel,
drag-to-scroll, momentum, clipping, offset/content size). Its native bars are
hidden; this widget reserves a right-hand strip, runs the engine in the
remaining content rect, and then paints a ported bar (`bar.rs`) with marks
(`marks.rs`) and gutter items (`gutter.rs`) on top. Handle drags are written back
into the engine `State` so kinetic/wheel scrolling keep working.

Key structures:
- MarkedScrollArea: builder.
- MarkedScrollOutput: closure result plus geometry (offset, content size, inner
  rect, bar geometry) for callers that position external overlays.

Notes:
Vertical only. See `bar.rs` for the egui port boundary and upgrade contract.
*/

mod bar;
mod gutter;
mod marks;

pub use gutter::{ArrowStyle, GutterItem, GutterSlot, arrow};
pub use marks::{BarGeometry, MarkFill, MarkKind, ScrollMark, ScrollSector, ScrollSpan};

use egui::scroll_area::ScrollBarVisibility;
use egui::{Id, Painter, Rect, ScrollArea, UiBuilder, Vec2, pos2};

/// Paints `marks` onto an externally-owned bar `geometry`, in ascending layer
/// order. `opacity` multiplies typed-fill alpha.
///
/// Use this to decorate a scrollbar this module does not own — for example the
/// vertical bar of an `egui::ScrollArea::both`, where `MarkedScrollArea` cannot
/// be used. The caller is responsible for the bar geometry and for drawing the
/// handle on top of the returned marks (see [`BarGeometry::handle_rect`]).
pub fn paint_marks_on_bar(
    painter: &Painter,
    geometry: &BarGeometry,
    marks: impl IntoIterator<Item = ScrollMark>,
    opacity: f32,
) {
    let mut marks: Vec<ScrollMark> = marks.into_iter().collect();
    marks.sort_by_key(|mark| mark.layer);
    for mark in marks {
        marks::paint_mark(painter, mark, geometry, opacity);
    }
}

/// Vertical scroll area with a markable scrollbar and a left gutter.
///
/// Build it like an `egui::ScrollArea`, attach marks and gutter items, then call
/// [`MarkedScrollArea::show`]. Marks are painted under the handle; gutter items
/// are painted in a reserved column left of the bar.
pub struct MarkedScrollArea {
    id_salt: Id,
    floating: bool,
    marks_follow_bar_opacity: bool,
    gutter_width: f32,
    marks: Vec<ScrollMark>,
    gutter_items: Vec<GutterItem>,
}

/// Result of [`MarkedScrollArea::show`].
///
/// Mirrors the useful parts of `egui::ScrollAreaOutput` and adds the resolved
/// [`BarGeometry`] so callers can place additional overlays using the same
/// content-to-bar projection.
pub struct MarkedScrollOutput<R> {
    pub inner: R,
    pub id: Id,
    pub offset: Vec2,
    pub content_size: Vec2,
    pub inner_rect: Rect,
    pub bar_geometry: BarGeometry,
    /// Reserved gutter column left of the bar, if `gutter_width > 0`.
    pub gutter_rect: Option<Rect>,
}

impl<R> MarkedScrollOutput<R> {
    /// Paints gutter items after the fact, using the resolved bar geometry.
    ///
    /// Use this when the anchor positions are only known after the content has
    /// been laid out (e.g. derived from rendered item positions). Items are
    /// projected onto the same bar geometry as build-time gutter items. No-op
    /// when no gutter was reserved (`gutter_width == 0`).
    pub fn paint_gutter(
        &self,
        painter: &egui::Painter,
        items: impl IntoIterator<Item = GutterItem>,
    ) {
        if let Some(gutter_rect) = self.gutter_rect {
            gutter::paint_gutter_items(
                painter,
                gutter_rect,
                &self.bar_geometry,
                items.into_iter().collect(),
            );
        }
    }
}

impl MarkedScrollArea {
    /// Creates a vertical markable scroll area. `id_salt` namespaces the host
    /// scroll area state and the bar interaction ids.
    #[must_use]
    pub fn vertical(id_salt: impl std::hash::Hash) -> Self {
        Self {
            id_salt: Id::new(id_salt),
            floating: true,
            marks_follow_bar_opacity: false,
            gutter_width: 0.0,
            marks: Vec::new(),
            gutter_items: Vec::new(),
        }
    }

    /// Enables/disables the floating bar (thin, expands on hover). Default: true.
    #[must_use]
    pub fn floating(mut self, floating: bool) -> Self {
        self.floating = floating;
        self
    }

    /// When true, typed-fill marks fade with the floating track background.
    /// Default: false (marks stay fully opaque).
    #[must_use]
    pub fn marks_follow_bar_opacity(mut self, follow: bool) -> Self {
        self.marks_follow_bar_opacity = follow;
        self
    }

    /// Reserves a gutter of `width` points left of the bar for gutter items.
    /// `0.0` (default) disables the gutter.
    #[must_use]
    pub fn gutter_width(mut self, width: f32) -> Self {
        self.gutter_width = width.max(0.0);
        self
    }

    /// Adds a single mark.
    #[must_use]
    pub fn mark(mut self, mark: ScrollMark) -> Self {
        self.marks.push(mark);
        self
    }

    /// Adds many marks.
    #[must_use]
    pub fn marks(mut self, marks: impl IntoIterator<Item = ScrollMark>) -> Self {
        self.marks.extend(marks);
        self
    }

    /// Adds a single gutter item.
    #[must_use]
    pub fn gutter(mut self, item: GutterItem) -> Self {
        self.gutter_items.push(item);
        self
    }

    /// Adds many gutter items.
    #[must_use]
    pub fn gutters(mut self, items: impl IntoIterator<Item = GutterItem>) -> Self {
        self.gutter_items.extend(items);
        self
    }

    /// Runs the scroll engine, then paints the markable bar and gutter.
    ///
    /// The content closure runs inside the reserved content rect with native
    /// bars hidden. Handle drags are written back into the engine state and a
    /// repaint is requested so the visible offset updates without a frame delay.
    #[must_use]
    pub fn show<R>(
        self,
        ui: &mut egui::Ui,
        add_contents: impl FnOnce(&mut egui::Ui) -> R,
    ) -> MarkedScrollOutput<R> {
        let scroll_style = ui.spacing().scroll;
        let bar_width = scroll_style.bar_width;
        let bar_outer = scroll_style.bar_outer_margin;
        // Floating bars reserve only a thin sliver of content space and overlap
        // the content when expanded, matching egui's `floating_allocated_width`.
        let reserved = if self.floating {
            scroll_style.floating_allocated_width
        } else {
            bar_width
        };

        let total = ui.available_rect_before_wrap();
        let strip = self.gutter_width + reserved + bar_outer;
        let content_right = (total.right() - strip).max(total.left());
        let content_rect = Rect::from_min_max(total.min, pos2(content_right, total.bottom()));

        // Run the engine in the content rect with native bars hidden.
        let layout = *ui.layout();
        let output = ui
            .scope_builder(
                UiBuilder::new().max_rect(content_rect).layout(layout),
                |ui| {
                    ScrollArea::vertical()
                        .id_salt(self.id_salt)
                        .scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
                        .auto_shrink([false, false])
                        .show(ui, add_contents)
                },
            )
            .inner;

        // Bar/gutter rects align to the visible viewport from the engine output.
        let bar_left = total.right() - bar_width - bar_outer;
        let bar_right = total.right() - bar_outer;
        let track_rect = Rect::from_min_max(
            pos2(bar_left, output.inner_rect.top()),
            pos2(bar_right, output.inner_rect.bottom()),
        );
        let gutter_rect = (self.gutter_width > 0.0).then(|| {
            Rect::from_min_max(
                pos2(bar_left - self.gutter_width, output.inner_rect.top()),
                pos2(bar_left, output.inner_rect.bottom()),
            )
        });

        let result = bar::run_vertical_bar(
            ui,
            &bar::BarConfig {
                floating: self.floating,
                marks_follow_bar_opacity: self.marks_follow_bar_opacity,
            },
            &bar::BarInputs {
                base_id: output.id,
                track_rect,
                gutter_rect,
                content_size_y: output.content_size.y,
                viewport_y: output.inner_rect.height(),
                offset_y: output.state.offset.y,
            },
            self.marks,
            self.gutter_items,
        );

        // Reserve the whole widget area (engine only reserved the content rect).
        ui.advance_cursor_after_rect(total);

        // Write a handle drag back into the engine so wheel/momentum stay in sync.
        let mut offset = output.state.offset;
        if let Some(new_offset_y) = result.new_offset_y {
            let mut state = output.state;
            state.offset.y = new_offset_y;
            state.store(ui.ctx(), output.id);
            ui.ctx().request_repaint();
            offset.y = new_offset_y;
        }

        MarkedScrollOutput {
            inner: output.inner,
            id: output.id,
            offset,
            content_size: output.content_size,
            inner_rect: output.inner_rect,
            bar_geometry: result.geometry,
            gutter_rect,
        }
    }
}
