/*
File: crates/ms-launcher/src/new_project/window/tests.rs

Purpose:
Unit tests of the New Project window: batch child-window close path, quick-download
cancellation, ribbon geometry helpers and the overwrite-confirm check.
*/

use super::{
    BatchProcessingWindowState, OperationProgress, RIBBON_MIN_WIDTH_FRACTION, TestChapterAvailabilityResult, cancel_quick_download_slot,
    clamp_ribbon_width_fraction, close_batch_window, ribbon_page_display_size, ribbon_page_left_inset,
    ribbon_scale_base_px,
};
use super::save::needs_overwrite_confirm;
use std::sync::mpsc;

fn quick_download_progress() -> Option<OperationProgress> {
    Some(OperationProgress { operation: "quick_download", stage: "connect".to_string(), current: 0, total: 1 })
}

#[test]
fn closing_cancels_an_in_flight_test_chapter_check() {
    let (tx, rx) = mpsc::channel::<TestChapterAvailabilityResult>();
    let mut check_rx = Some(rx);
    let mut progress = quick_download_progress();
    assert!(cancel_quick_download_slot(&mut check_rx, false, &mut progress));
    assert!(check_rx.is_none(), "the check's receiver is dropped");
    assert!(progress.is_none(), "its progress bar is released");
    let late = TestChapterAvailabilityResult { available: true, chapter_url: String::new(), log_message: None };
    assert!(tx.send(late).is_err(), "a late result can no longer reach the window");
}

#[test]
fn closing_releases_the_slot_of_a_cancelled_download_and_leaves_other_progress() {
    let mut check_rx = None;
    let mut progress = quick_download_progress();
    assert!(cancel_quick_download_slot(&mut check_rx, true, &mut progress));
    assert!(progress.is_none());

    let mut other = Some(OperationProgress { operation: "waifu2x", stage: "prepare".to_string(), current: 0, total: 1 });
    assert!(!cancel_quick_download_slot(&mut check_rx, false, &mut other), "nothing was running");
    assert!(other.is_some(), "another operation's progress is not touched");
}
use ms_widgets::embedded_window::covers_host;
use ms_widgets::{EmbeddedWindow, EmbeddedWindowState};

/// One headless pass of a host showing a parent window whose child is the batch window.
/// Returns whether the host was covered. The first pass's font-atlas delta is never applied
/// without a renderer, so it is cleared (epaint panics when an unapplied delta is dropped).
fn parent_with_batch_pass(ctx: &egui::Context, parent: &mut EmbeddedWindowState, batch: &mut EmbeddedWindowState) -> bool {
    let mut covered = false;
    ctx.run_ui(egui::RawInput::default(), |ui| {
        covered = covers_host(ui.ctx());
        EmbeddedWindow::new("test.new_project", "New project", &mut *parent).show(
            ui,
            |_| {},
            |ui| {
                EmbeddedWindow::new("test.new_project.batch", "Batch", &mut *batch).show(ui, |_| {}, |_| {});
            },
        );
    })
    .textures_delta
    .clear();
    covered
}

#[test]
fn closing_new_project_closes_its_batch_window() {
    // `Context::default()` embeds every window, so both stack on the root viewport.
    let ctx = egui::Context::default();
    let mut parent = EmbeddedWindowState::new();
    let mut batch_window = EmbeddedWindowState::new();
    let mut batch = BatchProcessingWindowState::new();
    parent.open(&ctx);
    batch_window.open(&ctx);
    for _ in 0..3 {
        parent_with_batch_pass(&ctx, &mut parent, &mut batch_window);
    }
    assert!(parent_with_batch_pass(&ctx, &mut parent, &mut batch_window));

    // The parent's close path (`on_window_closed`) closes the batch window too.
    close_batch_window(&mut batch_window, &mut batch);
    parent.close();
    assert!(!batch_window.is_open());
    parent_with_batch_pass(&ctx, &mut parent, &mut batch_window);
    assert!(!parent_with_batch_pass(&ctx, &mut parent, &mut batch_window), "nothing covers the host once both are closed");

    // Closing an already closed batch window is a no-op.
    close_batch_window(&mut batch_window, &mut batch);
    assert!(!batch_window.is_open());
}

/// Points of slack allowed when comparing derived layout sizes.
const EPS: f32 = 0.001;

/// Resolves a page's on-screen size the way `show_ribbon` does, for a given mode.
fn display_size(
    uniform: bool,
    content_width: f32,
    page: [usize; 2],
    widest_px: usize,
) -> (f32, egui::Vec2) {
    let base = ribbon_scale_base_px(uniform, page[0], widest_px);
    ribbon_page_display_size(content_width, page, base)
}

#[test]
fn uniform_mode_fills_the_content_width_for_every_page() {
    let content_width = 800.0;
    for page in [[1440_usize, 2048_usize], [720, 1024], [180, 180]] {
        let (_, size) = display_size(true, content_width, page, 1440);
        assert!(
            (size.x - content_width).abs() < EPS,
            "page {page:?} rendered at {} instead of {content_width}",
            size.x
        );
    }
}

#[test]
fn proportional_mode_scales_relative_to_the_widest_page() {
    let content_width = 800.0;
    let widest_px = 800_usize;

    let (_, widest) = display_size(false, content_width, [800, 1200], widest_px);
    assert!((widest.x - content_width).abs() < EPS);

    let (_, half) = display_size(false, content_width, [400, 600], widest_px);
    assert!(
        (half.x - content_width * 0.5).abs() < EPS,
        "half-width page rendered at {}",
        half.x
    );

    let (_, eighth) = display_size(false, content_width, [100, 150], widest_px);
    assert!(
        (eighth.x - content_width / 8.0).abs() < EPS,
        "eighth-width page rendered at {}",
        eighth.x
    );
}

#[test]
fn uniform_mode_never_insets_a_page() {
    let content_width = 800.0;
    let (_, size) = display_size(true, content_width, [1440, 2048], 1440);
    assert!(ribbon_page_left_inset(content_width, size.x).abs() < EPS);
}

#[test]
fn proportional_mode_centers_a_narrow_page() {
    let content_width = 800.0;
    let (_, size) = display_size(false, content_width, [400, 600], 800);
    let inset = ribbon_page_left_inset(content_width, size.x);
    // Half the leftover room on the left means the same half is left on the right.
    assert!((inset - 200.0).abs() < EPS, "inset = {inset}");
    assert!((content_width - (inset + size.x) - inset).abs() < EPS);
}

#[test]
fn left_inset_never_goes_negative_or_non_finite() {
    assert!(ribbon_page_left_inset(300.0, 900.0).abs() < EPS);
    assert!(ribbon_page_left_inset(f32::NAN, 100.0).abs() < EPS);
    assert!(ribbon_page_left_inset(f32::INFINITY, f32::INFINITY).abs() < EPS);
}

#[test]
fn both_modes_preserve_the_aspect_ratio() {
    let page = [1440_usize, 2048_usize];
    let source_ratio = page[1] as f32 / page[0] as f32;
    for uniform in [true, false] {
        let (_, size) = display_size(uniform, 640.0, page, 2000);
        assert!(
            ((size.y / size.x) - source_ratio).abs() < EPS,
            "uniform={uniform}: ratio {} != {source_ratio}",
            size.y / size.x
        );
    }
}

#[test]
fn scale_base_never_upscales_a_page_past_the_ribbon() {
    // A widest-page value smaller than the page itself cannot happen for a real ribbon, but
    // must not produce a page wider than the content width if it ever does.
    let (_, size) = display_size(false, 500.0, [900, 900], 300);
    assert!(size.x <= 500.0 + EPS, "page rendered at {}", size.x);
    assert_eq!(ribbon_scale_base_px(false, 0, 0), 1);
    assert_eq!(ribbon_scale_base_px(true, 0, 4096), 1);
}

#[test]
fn degenerate_content_width_falls_back_instead_of_dividing_by_zero() {
    let (scale, size) = ribbon_page_display_size(0.0, [100, 200], 100);
    assert!(scale.is_finite() && scale > 0.0);
    assert!((size.x - 120.0).abs() < EPS, "size.x = {}", size.x);

    let (scale, _) = ribbon_page_display_size(f32::NAN, [100, 200], 100);
    assert!(scale.is_finite());
}

#[test]
fn width_fraction_is_clamped_to_the_allowed_range() {
    assert!((clamp_ribbon_width_fraction(1.0) - 1.0).abs() < EPS);
    assert!((clamp_ribbon_width_fraction(0.5) - 0.5).abs() < EPS);
    assert!((clamp_ribbon_width_fraction(2.5) - 1.0).abs() < EPS);
    assert!(
        (clamp_ribbon_width_fraction(0.01) - RIBBON_MIN_WIDTH_FRACTION).abs() < EPS,
        "below-minimum fractions must clamp up to the minimum"
    );
    assert!(
        (clamp_ribbon_width_fraction(-3.0) - RIBBON_MIN_WIDTH_FRACTION).abs() < EPS,
        "negative fractions must clamp up to the minimum"
    );
}

#[test]
fn garbage_width_fraction_falls_back_to_full_width() {
    assert!((clamp_ribbon_width_fraction(f32::NAN) - 1.0).abs() < EPS);
    assert!((clamp_ribbon_width_fraction(f32::INFINITY) - 1.0).abs() < EPS);
    assert!((clamp_ribbon_width_fraction(f32::NEG_INFINITY) - 1.0).abs() < EPS);
}

#[test]
fn missing_or_empty_folder_needs_no_overwrite_confirm() -> std::io::Result<()> {
    let dir = tempfile::tempdir()?;
    assert!(!needs_overwrite_confirm(&dir.path().join("absent"))?);
    assert!(!needs_overwrite_confirm(dir.path())?);
    Ok(())
}

#[test]
fn folder_with_entries_needs_overwrite_confirm() -> std::io::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("001.png"), b"x")?;
    assert!(needs_overwrite_confirm(dir.path())?);
    Ok(())
}
