/*
File: crates/ms-launcher/src/new_project/window/capture.rs

Purpose:
Clipboard paste and the screen-capture overlay of the New Project window: request/poll of the
background paste and capture jobs, the full-desktop selection overlay, and the mapping between
desktop bounds, overlay rects and the global capture rectangle.

Key items:
- `start_clipboard_paste` / `poll_clipboard_paste`.
- `start_screen_capture_mode`, `queue_screen_capture`, `poll_screen_capture`,
  `show_screen_capture_overlay`.
- `resolve_capture_desktop_bounds` (with the macOS-only `macos_monitor_size_to_bounds`),
  `default_screen_capture_selection`, `screen_capture_selection_to_global_rect`.

Notes:
The macOS-only helper and its test module live together here: macOS is not a mandatory
check target, so keep every cfg twin in this one file.
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn start_clipboard_paste(&mut self) {
        if self.clipboard_paste_in_flight {
            return;
        }
        let (tx, rx) = mpsc::channel::<ClipboardPasteResult>();
        self.clipboard_paste_rx = Some(rx);
        self.clipboard_paste_in_flight = true;
        self.active_progress = None;
        self.last_error = None;
        self.import_status = t!("launcher.new_project.reading_clipboard_status").to_string();
        let page_name = self.next_clipboard_page_name();
        thread::spawn(move || {
            let result = match paste_image::read_image_from_clipboard() {
                Ok(image) => {
                    let rgba = image::RgbaImage::from_raw(
                        u32::try_from(image.width).unwrap_or(u32::MAX),
                        u32::try_from(image.height).unwrap_or(u32::MAX),
                        image.rgba,
                    );
                    match rgba {
                        Some(rgba) => ClipboardPasteResult {
                            image: Some(ImportedImage {
                                name: page_name,
                                image: DynamicImage::ImageRgba8(rgba),
                            }),
                            error: None,
                        },
                        None => ClipboardPasteResult {
                            image: None,
                            error: Some(
                                t!("launcher.new_project.clipboard_unsupported_size_error")
                                    .to_string(),
                            ),
                        },
                    }
                }
                Err(error) => ClipboardPasteResult {
                    image: None,
                    error: Some(error),
                },
            };
            let _ = tx.send(result);
        });
    }

    pub(super) fn poll_clipboard_paste(&mut self) {
        let Some(rx) = self.clipboard_paste_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(ClipboardPasteResult {
                image: None,
                error: Some(t!("launcher.new_project.clipboard_read_interrupted_error").to_string()),
            }),
        };
        let Some(result) = result else {
            return;
        };
        self.clipboard_paste_in_flight = false;
        self.clipboard_paste_rx = None;

        if let Some(image) = result.image {
            self.apply_source_import_result(
                PathBuf::from("[clipboard]"),
                build_ribbon_pages(vec![image]),
            );
            self.crop_editor = None;
            self.manual_cut_guides.clear();
            self.import_status = t!("launcher.new_project.clipboard_image_pasted_status").to_string();
            self.last_error = None;
            self.advance_simple_import_step_after_success();
            ms_log::runtime_log::log_info("[new-project] ribbon image pasted from clipboard");
        } else if let Some(error) = result.error {
            self.import_status = t!("launcher.new_project.clipboard_paste_error").to_string();
            self.last_error = Some(error.clone());
            ms_log::runtime_log::log_error(format!("[new-project] clipboard paste failed: {error}"));
        }
    }

    pub(super) fn next_clipboard_page_name(&self) -> String {
        let next_index = self.ribbon.pages().len().saturating_add(1);
        format!("clipboard_{next_index:03}.png")
    }

    pub(super) fn is_screen_capture_mode_enabled(&self) -> bool {
        self.screen_capture_overlay.is_some()
            || self.screen_capture_pending.is_some()
            || self.screen_capture_in_flight
    }

    /// Enables the screen-capture overlay for a fresh selection.
    ///
    /// `ctx` is the launcher viewport context; on macOS it supplies the monitor
    /// size in logical points used to size and position the overlay. See
    /// `resolve_capture_desktop_bounds` for why the bounds source is
    /// platform-dependent.
    pub(super) fn start_screen_capture_mode(&mut self, ctx: &egui::Context) {
        match resolve_capture_desktop_bounds(ctx) {
            Ok(desktop_bounds) => {
                self.screen_capture_overlay = Some(ScreenCaptureOverlayState {
                    selection: default_screen_capture_selection(desktop_bounds),
                    desktop_bounds,
                    drag_state: None,
                });
                self.screen_capture_pending = None;
                self.screen_capture_rx = None;
                self.screen_capture_in_flight = false;
                self.import_status =
                    t!("launcher.new_project.capture_active_hint").to_string();
                self.last_error = None;
                ms_log::runtime_log::log_info(format!(
                    "[new-project] screen capture mode enabled for desktop {}x{} at {},{}",
                    desktop_bounds.width, desktop_bounds.height, desktop_bounds.x, desktop_bounds.y
                ));
            }
            Err(error) => {
                self.import_status = t!("launcher.new_project.capture_enable_error").to_string();
                self.last_error = Some(error.clone());
                ms_log::runtime_log::log_error(format!(
                    "[new-project] failed to enable screen capture mode: {error}"
                ));
            }
        }
    }

    pub(super) fn stop_screen_capture_mode(&mut self) {
        self.screen_capture_overlay = None;
        self.screen_capture_pending = None;
        self.screen_capture_rx = None;
        self.screen_capture_in_flight = false;
        self.import_status = t!("launcher.new_project.capture_disabled_status").to_string();
        self.last_error = None;
        ms_log::runtime_log::log_info("[new-project] screen capture mode disabled");
    }

    pub(super) fn queue_screen_capture(&mut self) {
        let Some(overlay) = self.screen_capture_overlay.as_ref() else {
            return;
        };
        if self.screen_capture_pending.is_some() || self.screen_capture_in_flight {
            return;
        }
        self.screen_capture_pending = Some(PendingScreenCapture {
            requested_at: Instant::now(),
            region: screen_capture_selection_to_global_rect(
                overlay.desktop_bounds,
                overlay.selection,
            ),
        });
        self.import_status =
            t!("launcher.new_project.capture_preparing_status").to_string();
        self.last_error = None;
    }

    pub(super) fn maybe_start_pending_screen_capture(&mut self) {
        let Some(pending) = self.screen_capture_pending.as_ref() else {
            return;
        };
        if pending.requested_at.elapsed() < Duration::from_millis(SCREEN_CAPTURE_CONFIRM_DELAY_MS) {
            return;
        }
        if self.screen_capture_in_flight {
            return;
        }

        let region = pending.region;
        let page_name = self.next_screen_capture_page_name();
        let (tx, rx) = mpsc::channel::<ScreenCaptureResult>();
        self.screen_capture_rx = Some(rx);
        self.screen_capture_in_flight = true;
        self.screen_capture_pending = None;
        self.import_status = t!("launcher.new_project.capture_snapping_status").to_string();

        thread::spawn(move || {
            let result = match screen_capture::capture_screen_rect(region) {
                Ok(image) => ScreenCaptureResult {
                    image: Some(ImportedImage {
                        name: page_name,
                        image: DynamicImage::ImageRgba8(image),
                    }),
                    error: None,
                },
                Err(error) => ScreenCaptureResult {
                    image: None,
                    error: Some(error),
                },
            };
            let _ = tx.send(result);
        });
    }

    pub(super) fn poll_screen_capture(&mut self) {
        let Some(rx) = self.screen_capture_rx.as_ref() else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(ScreenCaptureResult {
                image: None,
                error: Some(t!("launcher.new_project.capture_interrupted_error").to_string()),
            }),
        };
        let Some(result) = result else {
            return;
        };
        self.screen_capture_rx = None;
        self.screen_capture_in_flight = false;

        if let Some(image) = result.image {
            self.apply_source_import_result(
                PathBuf::from("[screen-capture]"),
                build_ribbon_pages(vec![image]),
            );
            self.crop_editor = None;
            self.manual_cut_guides.clear();
            self.import_status = t!("launcher.new_project.capture_added_status").to_string();
            self.last_error = None;
            self.advance_simple_import_step_after_success();
            ms_log::runtime_log::log_info("[new-project] screen capture inserted into ribbon");
        } else if let Some(error) = result.error {
            self.import_status = t!("launcher.new_project.capture_failed_error").to_string();
            self.last_error = Some(error.clone());
            ms_log::runtime_log::log_error(format!("[new-project] screen capture failed: {error}"));
        }
    }

    pub(super) fn next_screen_capture_page_name(&self) -> String {
        let next_index = self.ribbon.pages().len().saturating_add(1);
        format!("capture_{next_index:03}.png")
    }

    pub(super) fn show_screen_capture_overlay(&mut self, ctx: &egui::Context) {
        if self.screen_capture_pending.is_some() || self.screen_capture_in_flight {
            return;
        }
        let Some(overlay) = self.screen_capture_overlay.as_mut() else {
            return;
        };

        let viewport_id = egui::ViewportId::from_hash_of(SCREEN_CAPTURE_VIEWPORT_ID_SALT);
        let mut keep_open = true;
        let mut capture_requested = false;
        let builder = crate::app::with_window_identity(
            egui::ViewportBuilder::default()
                .with_title(t!("launcher.new_project.capture_mode_button"))
                .with_position(egui::pos2(
                    overlay.desktop_bounds.x as f32,
                    overlay.desktop_bounds.y as f32,
                ))
                .with_inner_size(egui::vec2(
                    overlay.desktop_bounds.width as f32,
                    overlay.desktop_bounds.height as f32,
                ))
                .with_resizable(false)
                .with_transparent(true)
                .with_decorations(false)
                .with_clamp_size_to_monitor_size(false)
                .with_window_level(WindowLevel::AlwaysOnTop)
                .with_mouse_passthrough(false)
                .with_close_button(false)
                .with_minimize_button(false)
                .with_maximize_button(false)
                .with_active(true),
            // The new-project window does not know the run mode; the capture overlay has
            // always carried the normal app id.
            false,
        );

        ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
            let ctx = ui.ctx().clone();
            keep_open = !ctx.input(|input| input.viewport().close_requested());
            if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
                keep_open = false;
            }
            if ctx.input(|input| input.key_pressed(egui::Key::S)) {
                capture_requested = true;
            }
            ctx.request_repaint_after(Duration::from_millis(16));

            CentralPanel::default()
                .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
                .show(ui, |ui| {
                    render_screen_capture_overlay(ui, overlay);
                    if show_screen_capture_overlay_controls(ui, overlay.selection) {
                        capture_requested = true;
                    }
                });
        });

        if capture_requested {
            self.queue_screen_capture();
        }
        if !keep_open {
            self.stop_screen_capture_mode();
        }
    }
}

pub(super) fn screen_capture_rect_to_screen(selection: RibbonCrop, desktop_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(
            desktop_rect.left() + selection.left as f32,
            desktop_rect.top() + selection.top as f32,
        ),
        egui::pos2(
            desktop_rect.left() + (selection.left + selection.width) as f32,
            desktop_rect.top() + (selection.top + selection.height) as f32,
        ),
    )
}

pub(super) fn paint_screen_capture_overlay(ui: &Ui, desktop_rect: egui::Rect, selection_rect: egui::Rect) {
    let painter = ui.painter();
    let shade = egui::Color32::from_rgba_premultiplied(4, 8, 14, 156);
    let top = egui::Rect::from_min_max(
        desktop_rect.min,
        egui::pos2(desktop_rect.right(), selection_rect.top()),
    );
    let bottom = egui::Rect::from_min_max(
        egui::pos2(desktop_rect.left(), selection_rect.bottom()),
        desktop_rect.max,
    );
    let left = egui::Rect::from_min_max(
        egui::pos2(desktop_rect.left(), selection_rect.top()),
        egui::pos2(selection_rect.left(), selection_rect.bottom()),
    );
    let right = egui::Rect::from_min_max(
        egui::pos2(selection_rect.right(), selection_rect.top()),
        egui::pos2(desktop_rect.right(), selection_rect.bottom()),
    );
    for rect in [top, bottom, left, right] {
        if rect.is_positive() {
            painter.rect_filled(rect, 0.0, shade);
        }
    }
    painter.rect_stroke(
        selection_rect,
        10.0,
        Stroke::new(2.0, egui::Color32::from_rgb(255, 196, 74)),
        egui::StrokeKind::Inside,
    );
    painter.rect_stroke(
        selection_rect.expand(1.0),
        10.0,
        Stroke::new(1.0, egui::Color32::from_rgba_premultiplied(18, 18, 18, 220)),
        egui::StrokeKind::Outside,
    );
}

pub(super) fn paint_screen_capture_handles(ui: &Ui, selection_rect: egui::Rect) {
    let painter = ui.painter();
    for (_, handle_rect) in crop_handle_rects(selection_rect) {
        painter.rect_filled(handle_rect, 4.0, egui::Color32::from_rgb(255, 196, 74));
        painter.rect_stroke(
            handle_rect,
            4.0,
            Stroke::new(1.0, egui::Color32::from_rgb(74, 43, 6)),
            egui::StrokeKind::Inside,
        );
    }
}

pub(super) fn render_screen_capture_overlay(ui: &mut Ui, overlay: &mut ScreenCaptureOverlayState) {
    let available = ui.max_rect();
    let desktop_rect = egui::Rect::from_min_size(available.min, available.size());
    let selection_rect = screen_capture_rect_to_screen(overlay.selection, desktop_rect);
    paint_screen_capture_overlay(ui, desktop_rect, selection_rect);
    paint_screen_capture_handles(ui, selection_rect);
    handle_screen_capture_drag(ui, desktop_rect, overlay, selection_rect);
}

pub(super) fn show_screen_capture_overlay_controls(ui: &mut Ui, selection: RibbonCrop) -> bool {
    let rect = screen_capture_rect_to_screen(selection, ui.max_rect());
    let button_size = egui::vec2(132.0, 32.0);
    let controls_width = 312.0;
    let x =
        (rect.right() - controls_width).clamp(16.0, ui.max_rect().right() - controls_width - 16.0);
    let y = (rect.top() - button_size.y - 12.0).max(16.0);
    let controls_rect =
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(controls_width, button_size.y));

    let frame = Frame::new()
        .fill(egui::Color32::from_rgba_premultiplied(14, 18, 24, 228))
        .corner_radius(10.0)
        .stroke(Stroke::new(1.0, egui::Color32::from_rgb(255, 196, 74)))
        .inner_margin(egui::Margin::symmetric(8, 6));

    let mut capture_clicked = false;
    ui.scope_builder(egui::UiBuilder::new().max_rect(controls_rect), |ui| {
        frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                if button_sized(ui, t!("launcher.new_project.capture_snap_button"), button_size, true).clicked() {
                    capture_clicked = true;
                }
                ui.label(
                    RichText::new(format!("{} × {}", selection.width, selection.height))
                        .small()
                        .weak(),
                );
                ui.add_space(4.0);
                ui.label(RichText::new(t!("launcher.new_project.capture_esc_hint")).small().weak());
            });
        });
    });
    capture_clicked
}

pub(super) fn handle_screen_capture_drag(
    ui: &mut Ui,
    desktop_rect: egui::Rect,
    overlay: &mut ScreenCaptureOverlayState,
    selection_rect: egui::Rect,
) {
    let handle_rects = crop_handle_rects(selection_rect);
    for (handle, rect) in handle_rects {
        process_screen_capture_drag_response(ui, desktop_rect, overlay, rect, handle);
    }
    let move_rect = selection_rect.shrink(CROP_HANDLE_SIZE * 0.75);
    if move_rect.is_positive() {
        process_screen_capture_drag_response(
            ui,
            desktop_rect,
            overlay,
            move_rect,
            CropHandle::Move,
        );
    }
}

pub(super) fn process_screen_capture_drag_response(
    ui: &mut Ui,
    desktop_rect: egui::Rect,
    overlay: &mut ScreenCaptureOverlayState,
    drag_rect: egui::Rect,
    handle: CropHandle,
) {
    let response = ui.interact(
        drag_rect,
        ui.id()
            .with(("launcher_new_project_screen_capture_drag", handle)),
        egui::Sense::drag(),
    );
    if response.drag_started()
        && let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
    {
        overlay.drag_state = Some(CropDragState {
            handle,
            start_rect: overlay.selection,
            start_pointer_px: egui::pos2(
                pointer_pos.x - desktop_rect.left(),
                pointer_pos.y - desktop_rect.top(),
            ),
        });
    }
    if response.dragged()
        && let Some(drag_state) = overlay.drag_state
        && drag_state.handle == handle
        && let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
    {
        let current_pointer_px = egui::pos2(
            pointer_pos.x - desktop_rect.left(),
            pointer_pos.y - desktop_rect.top(),
        );
        overlay.selection = apply_crop_drag(
            drag_state.start_rect,
            drag_state.handle,
            drag_state.start_pointer_px,
            current_pointer_px,
            [
                usize::try_from(overlay.desktop_bounds.width).unwrap_or(usize::MAX),
                usize::try_from(overlay.desktop_bounds.height).unwrap_or(usize::MAX),
            ],
            SCREEN_CAPTURE_MIN_SIDE,
        );
    }
    if response.drag_stopped()
        && overlay
            .drag_state
            .is_some_and(|state| state.handle == handle)
    {
        overlay.drag_state = None;
    }
}

/// Resolves the virtual-desktop bounds used to place the capture overlay.
///
/// The whole capture-overlay chain (the `ViewportBuilder` position/size, the
/// default selection, `screen_capture_selection_to_global_rect`, and
/// `screencapture -R`) works in logical points. On macOS the pixel-based
/// `query_virtual_desktop_bounds` reads a full-screen PNG header and therefore
/// reports *device pixels*, which on a Retina (2x) display are twice the point
/// magnitude — that would make the overlay 2x oversized and the captured region
/// 2x wrong. To keep every stage consistent in points, macOS instead sources the
/// bounds from egui's monitor size (already in logical points).
///
/// Limitation: egui reports the monitor the launcher window currently sits on,
/// so on a multi-display desktop only that monitor is covered. This is correct
/// for the common single-display case and strictly better than the previously
/// 2x-broken pixel-based bounds. If egui has no monitor size yet (`None`), macOS
/// falls back to the pixel-based query rather than failing.
///
/// Non-macOS platforms use `query_virtual_desktop_bounds` exactly as before.
///
/// # Errors
/// Propagates the platform-specific error from `query_virtual_desktop_bounds`
/// when the desktop bounds cannot be determined.
pub(super) fn resolve_capture_desktop_bounds(ctx: &egui::Context) -> Result<ScreenRect, String> {
    #[cfg(target_os = "macos")]
    {
        // Prefer egui's point-based monitor size so overlay sizing, the default
        // selection, and `screencapture -R` all share the point coordinate space.
        if let Some(bounds) = ctx
            .input(|input| input.viewport().monitor_size)
            .and_then(macos_monitor_size_to_bounds)
        {
            return Ok(bounds);
        }
        // Monitor size unavailable (e.g. not yet populated): degrade to the
        // existing pixel-based behavior instead of crashing.
        screen_capture::query_virtual_desktop_bounds()
    }
    #[cfg(not(target_os = "macos"))]
    {
        // `ctx` is only needed for the macOS monitor-size path; other platforms
        // keep using the pixel-based desktop query unchanged.
        let _ = ctx;
        screen_capture::query_virtual_desktop_bounds()
    }
}

/// Converts an egui monitor size (logical points) into capture-overlay bounds.
///
/// The overlay is anchored at the top-left of the current monitor, so the result
/// uses origin `(0, 0)` and the rounded point dimensions. Returns `None` for a
/// non-finite or sub-pixel size so the caller can fall back to the pixel-based
/// desktop query.
#[cfg(target_os = "macos")]
pub(super) fn macos_monitor_size_to_bounds(monitor_size: egui::Vec2) -> Option<ScreenRect> {
    let width = monitor_size.x;
    let height = monitor_size.y;
    // Reject unusable sizes; a valid monitor is at least one point on each side.
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 {
        return None;
    }
    // Round to whole points and clamp to a sane monitor range. Bounding to
    // `u16::MAX` makes the following f32 -> u32 conversion provably exact
    // (integral value within `1..=65535`), which is the safety justification the
    // project's no-blind-`as` rule requires.
    let width = width.round().clamp(1.0, f32::from(u16::MAX));
    let height = height.round().clamp(1.0, f32::from(u16::MAX));
    Some(ScreenRect {
        x: 0,
        y: 0,
        width: width as u32,
        height: height as u32,
    })
}

pub(super) fn default_screen_capture_selection(desktop_bounds: ScreenRect) -> RibbonCrop {
    let desktop_width = usize::try_from(desktop_bounds.width)
        .unwrap_or(usize::MAX)
        .max(1);
    let desktop_height = usize::try_from(desktop_bounds.height)
        .unwrap_or(usize::MAX)
        .max(1);
    let width = (desktop_width.saturating_mul(45) / 100)
        .max(SCREEN_CAPTURE_MIN_SIDE)
        .min(desktop_width);
    let height = (desktop_height.saturating_mul(32) / 100)
        .max(SCREEN_CAPTURE_MIN_SIDE)
        .min(desktop_height);
    RibbonCrop {
        left: desktop_width.saturating_sub(width) / 2,
        top: desktop_height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(super) fn screen_capture_selection_to_global_rect(
    desktop_bounds: ScreenRect,
    selection: RibbonCrop,
) -> ScreenRect {
    ScreenRect {
        x: desktop_bounds
            .x
            .saturating_add(i32::try_from(selection.left).unwrap_or(i32::MAX)),
        y: desktop_bounds
            .y
            .saturating_add(i32::try_from(selection.top).unwrap_or(i32::MAX)),
        width: u32::try_from(selection.width).unwrap_or(u32::MAX),
        height: u32::try_from(selection.height).unwrap_or(u32::MAX),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_capture_bounds_tests {
    use super::{ScreenRect, macos_monitor_size_to_bounds};

    #[test]
    fn retina_point_size_maps_to_point_bounds_at_origin() {
        // egui reports the monitor in logical points, so a 2x Retina 2880x1800
        // panel arrives here already as 1440x900 points.
        let bounds = macos_monitor_size_to_bounds(egui::vec2(1440.0, 900.0));
        assert_eq!(
            bounds,
            Some(ScreenRect {
                x: 0,
                y: 0,
                width: 1440,
                height: 900,
            })
        );
    }

    #[test]
    fn fractional_points_round_to_nearest_whole_point() {
        let bounds = macos_monitor_size_to_bounds(egui::vec2(1512.4, 981.6));
        assert_eq!(
            bounds,
            Some(ScreenRect {
                x: 0,
                y: 0,
                width: 1512,
                height: 982,
            })
        );
    }

    #[test]
    fn unusable_sizes_return_none_for_fallback() {
        assert_eq!(macos_monitor_size_to_bounds(egui::vec2(0.0, 900.0)), None);
        assert_eq!(macos_monitor_size_to_bounds(egui::vec2(1440.0, 0.5)), None);
        assert_eq!(
            macos_monitor_size_to_bounds(egui::vec2(f32::NAN, 900.0)),
            None
        );
        assert_eq!(
            macos_monitor_size_to_bounds(egui::vec2(f32::INFINITY, 900.0)),
            None
        );
    }
}
