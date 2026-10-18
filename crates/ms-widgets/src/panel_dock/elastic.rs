/*
File: crates/ms-widgets/src/panel_dock/elastic.rs

Purpose:
An ELASTIC filler for a dock tab body: a section that sits at the very bottom of
the body's viewport while the body is shorter than it, and right after the
preceding content once the body scrolls — without the filler ever growing the
panel that measures the body.

Main responsibilities:
- place one section at the bottom of the free room of the `Ui` it is drawn in;
- report the filler it inserted to the enclosing dock body, which subtracts it
  from the content height the panel asks the solver for;
- keep the section's last drawn height, and repaint when it changes.

Key functions:
- `bottom_aligned`: the public helper a tab body calls.
- `open_body` / `close_body`: the dock-body bracket `panel.rs` draws the scroll
  section inside; between the two, fillers are summed.

Notes:
- Why the dock has to know: a panel is sized from its scrolled CONTENT
  (`CollapsiblePanel::show_sections`, `measured_size`), and egui counts any
  spacer as content (`ScrollArea` returns `content_ui.min_size()`,
  `egui-0.36.2/src/containers/scroll_area.rs:1082`). A plain `add_space` would
  therefore become a permanent part of the panel's request: the panel could grow,
  and never shrink again after a section above it collapsed.
- The filler is computed from the section's height LAST frame (egui temp data),
  so a section that changes height is placed one frame late; the change requests
  a repaint. A section that GROWS (a collapsing header animating open grows a
  little every frame of its animation) would then overshoot the viewport on each
  of those frames: a scroll bar would flash for the whole animation. So while a
  filler is inserted, the section is drawn in a non-allocating child `Ui` and
  only the part of it that fits the room is allocated in the parent: the
  overshoot (at most the growth since last frame) stays clipped below the
  viewport instead of growing the scroll content, and the next frame moves the
  section up. No lag-free form exists in egui 0.36.2: a vertical child inside a
  `Layout::bottom_up` parent still lays out from the TOP of the free room
  (`Ui::allocate_ui_with_layout`, `egui-0.36.2/src/ui.rs:1331`), and measuring
  first means drawing the section twice (same reason as the pinned bottom
  section, see the module readme).
- The slot is a global temp key holding a stack, one entry per open dock body,
  so a body drawn while another one is open (it never happens today) still sums
  only its own fillers.
*/

use egui::{AsIdSalt, Context, Id, Ui, UiBuilder};

/// Room, in points, the filler leaves between the section's bottom and the
/// viewport's. egui shows a scroll bar as soon as the content is taller than
/// the ceiled viewport (`scroll_area.rs:1203-1206`), so a section that touched
/// the bottom exactly would flash a phantom bar on any sub-point rounding. It is
/// also the repaint threshold: a height change smaller than the slack cannot
/// push the section past the viewport, so it needs no extra frame.
const SLACK_PT: f32 = 1.0;

/// Global temp-data key of the filler stack (`Vec<f32>`, one sum per open body).
fn slot_id() -> Id {
    Id::new("ms_panel_dock_elastic_filler_slot")
}

/// Draws `add_contents` at the BOTTOM of the free room of a dock body when the
/// body is shorter than its viewport, else right after the preceding content.
///
/// The filler above it is reported to the enclosing dock body and excluded from
/// the panel's content measurement, so it never grows the panel; a panel sized by
/// its content still shrinks after a section above collapses. The section is
/// placed from its height LAST frame: a section seen for the first time is drawn
/// right after the content, and any height change of at least one point requests
/// a repaint, so the placement settles on the next frame. While a filler is in
/// place, a section that grew since last frame never grows the scroll content past
/// the room: its overshoot is left unallocated (clipped) for that one frame, so no
/// scroll bar flashes while a collapsing header animates open.
///
/// Contract for the caller:
/// - call it as the LAST thing of the body, directly in the dock body's `Ui` or a
///   plain child of it (`push_id`, `scope`), never inside a nested `ScrollArea`:
///   the free room is measured against the `Ui`'s own `max_rect`;
/// - `id_salt` must be stable across frames and never a localized string (it keys
///   the remembered height under `ui.make_persistent_id(id_salt)`; egui's
///   `AsIdSalt` = `Hash + Debug`).
///
/// Outside a dock body (no `open_body` bracket) it still aligns, and reports
/// nothing.
pub fn bottom_aligned<R>(ui: &mut Ui, id_salt: impl AsIdSalt, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    let height_id = ui.make_persistent_id(id_salt);
    let last_height: Option<f32> = ui.ctx().data(|data| data.get_temp(height_id));
    let filler = last_height.map_or(0.0, |height| filler_height(ui.available_height(), height));
    if filler > 0.0 {
        ui.add_space(filler);
        report_filler(ui.ctx(), filler);
    }
    // `ui.scope` without its allocation: the same child as `Ui::scope_dyn` builds
    // (`new_child` with the default builder, `egui-0.36.2/src/ui.rs:2203`), so the
    // section's widget ids and fold states are those of a plain scope. Only the parent's
    // auto-id counter advances by one more than a scope would; nothing follows this LAST
    // section to notice.
    let room_bottom = ui.max_rect().bottom();
    let mut child = ui.new_child(UiBuilder::new());
    let inner = add_contents(&mut child);
    let drawn = child.min_rect();
    // Dropping the child registers its final rect (`impl Drop for Ui`, `egui-0.36.2/src/ui.rs:2889`).
    drop(child);
    let height = drawn.height();
    ui.advance_cursor_after_rect(allocated_rect(drawn, filler > 0.0, room_bottom));
    ui.ctx().data_mut(|data| data.insert_temp(height_id, height));
    if height_changed(last_height, height) {
        ui.ctx().request_repaint();
    }
    inner
}

/// The part of the drawn section `drawn` the parent allocates: all of it without a
/// filler; with one, at most down to `SLACK_PT` above `room_bottom`, so a section
/// that grew since the filler was computed does not push the scroll content past
/// its viewport (it is placed right on the next frame). Never above its own top.
fn allocated_rect(drawn: egui::Rect, has_filler: bool, room_bottom: f32) -> egui::Rect {
    if !has_filler {
        return drawn;
    }
    let mut kept = drawn;
    kept.max.y = kept.max.y.min(room_bottom - SLACK_PT).max(kept.min.y);
    kept
}

/// The filler that puts a section of `section_height` at the bottom of
/// `available_height`, `SLACK_PT` short of it; whole points, never negative.
fn filler_height(available_height: f32, section_height: f32) -> f32 {
    (available_height - section_height - SLACK_PT).floor().max(0.0)
}

/// Whether a section that drew `height` now (after `previous` last frame) needs
/// another frame to be placed right.
fn height_changed(previous: Option<f32>, height: f32) -> bool {
    previous.is_none_or(|previous| (previous - height).abs() >= SLACK_PT)
}

/// Adds `filler` to the innermost open dock body, if any. Outside a body the
/// slot does not exist and is not created.
fn report_filler(ctx: &Context, filler: f32) {
    ctx.data_mut(|data| {
        if let Some(mut stack) = data.remove_temp::<Vec<f32>>(slot_id()) {
            if let Some(sum) = stack.last_mut() {
                *sum += filler;
            }
            data.insert_temp(slot_id(), stack);
        }
    });
}

/// Opens a dock body: fillers drawn until the matching [`close_body`] are summed
/// for it. Called by `panel.rs` right before the body's `ScrollArea::show`.
pub(super) fn open_body(ctx: &Context) {
    ctx.data_mut(|data| data.get_temp_mut_or_default::<Vec<f32>>(slot_id()).push(0.0));
}

/// Closes the innermost dock body and returns the total filler height drawn in
/// it, in points (`0.0` when nothing aligned). The slot is removed once no body
/// is open, so nothing lingers in egui memory between frames. A close without an
/// open returns `0.0`; the only caller pairs the two around one `show`.
pub(super) fn close_body(ctx: &Context) -> f32 {
    ctx.data_mut(|data| {
        let mut stack = data.remove_temp::<Vec<f32>>(slot_id()).unwrap_or_default();
        let total = stack.pop().unwrap_or(0.0);
        if !stack.is_empty() {
            data.insert_temp(slot_id(), stack);
        }
        total
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Rect, Sense, Vec2};

    /// What one probe frame of a bracketed scroll body observed.
    struct Frame {
        /// Cursor before `bottom_aligned`, i.e. where the section would start without a filler.
        cursor_before: f32,
        /// Top of the scroll content.
        content_top: f32,
        /// The section's drawn rect.
        section: Rect,
        /// Bottom of the scroll content's free room (its `max_rect`).
        room_bottom: f32,
        /// What `close_body` reported.
        filler: f32,
        content_height: f32,
        viewport_height: f32,
        repaint_requested: bool,
    }

    /// Draws `frames` passes of a `viewport`-tall scroll body: `above` points of
    /// content, then a `section`-tall block through `bottom_aligned`. `bracket`
    /// says whether the body is inside `open_body` / `close_body`.
    fn run(viewport: f32, above: f32, sections: &[f32], bracket: bool) -> Vec<Frame> {
        let ctx = egui::Context::default();
        let mut frames = Vec::new();
        for &section_height in sections {
            let mut frame = None;
            // `FullOutput` carries this test's paint output only; nothing is
            // rendered, so dropping it is the whole intent.
            ctx.run_ui(egui::RawInput::default(), |ui| {
                if bracket {
                    open_body(ui.ctx());
                }
                let mut probe = (0.0, Rect::NOTHING, 0.0, false, 0.0);
                let scroll = egui::ScrollArea::vertical()
                    .max_height(viewport)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let width = ui.available_width();
                        probe.4 = ui.cursor().top();
                        ui.allocate_exact_size(Vec2::new(width, above), Sense::hover());
                        probe.0 = ui.cursor().top();
                        probe.2 = ui.max_rect().bottom();
                        probe.1 = bottom_aligned(ui, "probe_section", |ui| {
                            ui.allocate_exact_size(Vec2::new(width, section_height), Sense::hover()).0
                        });
                        probe.3 = ui.ctx().has_requested_repaint();
                    });
                let filler = if bracket { close_body(ui.ctx()) } else { 0.0 };
                frame = Some(Frame {
                    cursor_before: probe.0,
                    content_top: probe.4,
                    section: probe.1,
                    room_bottom: probe.2,
                    filler,
                    content_height: scroll.content_size.y,
                    viewport_height: scroll.inner_rect.height(),
                    repaint_requested: probe.3,
                });
            })
            .drop_without_applying_deltas();
            frames.extend(frame);
        }
        frames
    }

    #[test]
    fn a_short_body_puts_the_section_at_the_viewport_bottom() {
        let frames = run(300.0, 50.0, &[40.0; 3], true);
        let last = frames.last().expect("three frames ran");
        let gap = last.room_bottom - last.section.bottom();
        assert!((0.0..=SLACK_PT + 1.0).contains(&gap), "gap {gap}, {:?} vs {}", last.section, last.room_bottom);
        // The reported filler is exactly the space inserted above the section…
        assert!(last.filler > 0.0);
        assert!((last.section.top() - last.cursor_before - last.filler).abs() < 0.01);
        // …so the measurement without it is the content as if nothing aligned:
        // the section starting at the cursor, `40` tall.
        let unaligned = last.cursor_before - last.content_top + 40.0;
        assert!((last.content_height - last.filler - unaligned).abs() < 0.5, "{} vs {unaligned}", last.content_height);
        // And no phantom scroll bar: the content never outgrows the viewport.
        assert!(last.content_height <= last.viewport_height.ceil(), "{} > {}", last.content_height, last.viewport_height);
    }

    #[test]
    fn a_section_seen_for_the_first_time_follows_the_content_and_repaints() {
        let frames = run(300.0, 50.0, &[40.0], true);
        let first = &frames[0];
        assert_eq!(first.filler, 0.0);
        assert!((first.section.top() - first.cursor_before).abs() < 0.01);
        assert!(first.repaint_requested);
    }

    /// A section that grows a little every frame (a collapsing header animating
    /// open) never pushes the scroll content past the viewport — no scroll bar
    /// flashes during the animation — and settles at the bottom once it stops.
    #[test]
    fn a_growing_section_never_outgrows_the_viewport() {
        let heights = [40.0, 40.0, 40.0, 55.0, 70.0, 85.0, 100.0, 100.0];
        let frames = run(300.0, 50.0, &heights, true);
        for (idx, frame) in frames.iter().enumerate() {
            assert!(frame.content_height <= frame.viewport_height.ceil(), "frame {idx}: {} > {}", frame.content_height, frame.viewport_height);
        }
        for frame in &frames[3..7] {
            assert!(frame.repaint_requested, "each growth step asks for the frame that places it");
        }
        let settled = frames.last().expect("eight frames ran");
        let gap = settled.room_bottom - settled.section.bottom();
        assert!((0.0..=SLACK_PT + 1.0).contains(&gap), "gap {gap}");
        assert!((settled.section.height() - 100.0).abs() < 0.01, "the whole section is drawn");
        let unaligned = settled.cursor_before - settled.content_top + 100.0;
        assert!((settled.content_height - settled.filler - unaligned).abs() < 0.5, "{} vs {unaligned}", settled.content_height);
    }

    /// Without a filler the whole drawn section is allocated; with one, only the
    /// part above the room's bottom (less the slack), never above its own top.
    #[test]
    fn only_the_part_that_fits_is_allocated_while_a_filler_is_in_place() {
        let drawn = Rect::from_min_max(egui::pos2(0.0, 250.0), egui::pos2(10.0, 320.0));
        assert_eq!(allocated_rect(drawn, false, 300.0), drawn);
        assert_eq!(allocated_rect(drawn, true, 300.0).bottom(), 300.0 - SLACK_PT);
        assert_eq!(allocated_rect(drawn, true, 400.0), drawn);
        let below = Rect::from_min_max(egui::pos2(0.0, 305.0), egui::pos2(10.0, 320.0));
        assert_eq!(allocated_rect(below, true, 300.0).height(), 0.0);
    }

    #[test]
    fn a_tall_body_draws_the_section_right_after_the_content() {
        let frames = run(200.0, 500.0, &[40.0; 3], true);
        let last = frames.last().expect("three frames ran");
        assert_eq!(last.filler, 0.0);
        assert!((last.section.top() - last.cursor_before).abs() < 0.01);
    }

    #[test]
    fn a_shrinking_section_moves_down_on_the_next_frame() {
        let frames = run(300.0, 50.0, &[100.0, 100.0, 100.0, 20.0, 20.0], true);
        let lag = &frames[3];
        // The lag frame still reserves the old height, and asks for another frame.
        assert!(lag.repaint_requested);
        let settled = &frames[4];
        let gap = settled.room_bottom - settled.section.bottom();
        assert!((0.0..=SLACK_PT + 1.0).contains(&gap), "gap {gap}");
        let unaligned = settled.cursor_before - settled.content_top + 20.0;
        assert!((settled.content_height - settled.filler - unaligned).abs() < 0.5, "{} vs {unaligned}", settled.content_height);
    }

    #[test]
    fn outside_a_dock_body_it_aligns_and_reports_nothing() {
        let frames = run(300.0, 50.0, &[40.0; 3], false);
        let last = frames.last().expect("three frames ran");
        let gap = last.room_bottom - last.section.bottom();
        assert!((0.0..=SLACK_PT + 1.0).contains(&gap), "gap {gap}");
        let ctx = egui::Context::default();
        assert_eq!(close_body(&ctx), 0.0);
    }

    #[test]
    fn nested_bodies_sum_only_their_own_fillers() {
        let ctx = egui::Context::default();
        open_body(&ctx);
        report_filler(&ctx, 10.0);
        open_body(&ctx);
        report_filler(&ctx, 3.0);
        assert_eq!(close_body(&ctx), 3.0);
        report_filler(&ctx, 2.0);
        assert_eq!(close_body(&ctx), 12.0);
        // Nothing is left behind once every body closed.
        assert!(ctx.data(|data| data.get_temp::<Vec<f32>>(slot_id())).is_none());
    }

    #[test]
    fn the_filler_keeps_the_slack_and_never_goes_negative() {
        assert_eq!(filler_height(100.0, 40.0), 59.0);
        assert_eq!(filler_height(100.5, 40.0), 59.0);
        assert_eq!(filler_height(30.0, 40.0), 0.0);
        assert_eq!(filler_height(-5.0, 0.0), 0.0);
    }

    #[test]
    fn only_a_change_of_at_least_the_slack_repaints() {
        assert!(height_changed(None, 10.0));
        assert!(!height_changed(Some(10.0), 10.5));
        assert!(height_changed(Some(10.0), 11.0));
        assert!(height_changed(Some(10.0), 2.0));
    }
}
