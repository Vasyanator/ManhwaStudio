/*
File: tab/create_upload.rs

Purpose:
Text/image overlay creation UI and GPU texture upload plumbing for the typing
tab. Covers shift-drag selection capture, the inline text editor, dispatching the
create-overlay/create-raster render workers, transient status hints, runtime
overlay insertion into the doc, raster mask-clip preparation, and the
per-overlay texture upload/dirty-tracking helpers on `TypingTextOverlayLayer`.

Notes:
Extracted verbatim from `tab.rs`. Methods are `pub(super)` so `tab.rs` and sibling
submodules of `tab` can use them. `use super::*;` pulls in the parent module's
types and imports. Struct/enum definitions and the rest of the big
`impl TypingTextOverlayLayer` block remain in `tab.rs`; these methods reach the
private items that stay there as descendants of module `tab`.

The on-canvas editor's own-typeface font is obtained in TWO steps on purpose:
`request_editor_font` spawns the `FontProvider::resolve` (a provider cache miss is an
`fs::read`, which must never run on the GUI thread) and `poll_editor_font_request` —
driven once per frame from `draw_text_editor`, before its "no editor open" early
return — registers the arrived bytes with egui, which does need the GUI thread. The
field draws in the default UI font meanwhile.

The egui family those bytes are registered under is DERIVED from the resolved
content by `editor_font_family_name`; nothing here mints a name from a counter (see
that function for why a counter is a defect in this position).

The SIZE the field draws at is not the panel's size either: the panel names a size in
SOURCE pixels, the field lives in screen pixels, so the field scales it by its own
screen-per-source factor (`TypingCreateTextEditor::display_font_size_px`). Display only —
the render keeps taking the panel's unscaled size.
*/

use super::*;

/// Layer id of the full-canvas Shift+drag selection-capture overlay.
///
/// Kept on [`egui::Order::Middle`] so it sits BELOW the Foreground typing panels (their
/// Wheel widgets keep winning the z-order hit-test over their own rects and keep
/// receiving hover/scroll) but ABOVE the Background canvas content (bare-canvas
/// drag-selection is unaffected). Shared by the spawn site here and the tab's canvas
/// Shift+wheel font handler so both agree on one layer identity.
pub(super) fn shift_drag_capture_layer_id() -> egui::LayerId {
    egui::LayerId::new(
        egui::Order::Middle,
        egui::Id::new("typing_text_create_shift_capture"),
    )
}

/// Deterministic egui family name for the on-canvas create editor's own-typeface font of
/// `(font_identity, content_id, face_index)`.
///
/// Depends ONLY on those three values — the very key the registration is memoized by — so the
/// same font always names the same family, and fonts differing in any of them get different
/// names up to a 64-bit hash collision (the bound `combo_font_family_name` accepts as well).
///
/// # Invariant
/// The name may NOT depend on any counter or handle that lives shorter than the `egui::Context`.
/// `Context::add_font` resolves a name collision in favour of the FIRST registrant and never
/// compares bytes (egui-0.35.0/src/context.rs:2066-2074), while a project reload (a structural
/// page-manager operation, «Перезагрузить проект») rebuilds the whole `MangaApp` — and with it
/// this layer — INSIDE THE SAME context (`src/studio_bootstrap.rs`). A per-instance sequence
/// number therefore re-issues a name the context already holds foreign bytes for, and the editor
/// silently draws the previous session's typeface.
///
/// # Why this is not `widgets::font_preview::combo_font_family_name`
/// The two namespaces must stay disjoint (hence the distinct prefix) because their CONTENT
/// discriminants are different quantities for the same font: here it is `FontContent::content_id`
/// (a `DefaultHasher` over the served bytes — `font_provider::font_content_id` in the
/// `ms-text-render` crate), while the combo uses `FontEntry::content_hash` (the first 8 bytes
/// of the file's SHA-256). Folding
/// both into one name would either give one font two names or — worse — let two different fonts
/// meet on one name.
#[must_use]
fn editor_font_family_name(font_identity: &str, content_id: u64, face_index: usize) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    font_identity.hash(&mut hasher);
    content_id.hash(&mut hasher);
    face_index.hash(&mut hasher);
    format!("typing-editor-font-{:016x}", hasher.finish())
}

impl TypingCreateTextEditor {
    /// Font size the FIELD is drawn at: the panel's size scaled to this editor's view of the
    /// page, so the glyphs being typed are already the size the render will put in this box.
    ///
    /// `font_size_px` is in SOURCE pixels — the unit the renderer works in — while the field is
    /// laid out in screen pixels, so drawing it unscaled makes the text change size the moment
    /// the render replaces it. `scene_rect.width() / width_px` is this editor's
    /// screen-pixels-per-source-pixel: the canvas zoom, but taken from the very two numbers the
    /// box and the render width were built from, so it cannot disagree with either — it stays
    /// right even for a page whose clean overlay is not the size the page is drawn at, where the
    /// raw canvas zoom would be off.
    ///
    /// The factor is FROZEN with the box. `scene_rect` is captured once when the editor opens and
    /// does not follow a later pan or zoom, so a live factor would grow the text inside a box that
    /// stayed put.
    ///
    /// Display only: nothing here reaches the renderer, which keeps taking the panel's unscaled
    /// `font_size_px`.
    fn display_font_size_px(&self) -> f32 {
        // `width_px` is an image width and at least 1 (`selection_width_in_source_px`), so it is
        // far inside f32's exactly-representable integer range and the division stays finite.
        let screen_px_per_source_px = self.scene_rect.width() / self.width_px as f32;
        // `f32::max` also returns the bound for a NaN input, so degenerate geometry can never
        // hand egui a zero or NaN font size to lay a row out with.
        (self.font_size_px * screen_px_per_source_px).max(MIN_EDITOR_DISPLAY_FONT_SIZE_PX)
    }
}

impl TypingTextOverlayLayer {
    pub(super) fn wants_canvas_shift_drag_selection(&self, ctx: &egui::Context) -> bool {
        self.create_selection.is_some()
            || self.create_editor.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || ctx.input(|i| i.modifiers.shift)
    }

    pub(super) fn draw_create_overlay_ui(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        let now_s = ctx.input(|i| i.time);
        if self
            .create_status_error
            .as_ref()
            .is_some_and(|(_, hide_at)| now_s >= *hide_at)
        {
            self.create_status_error = None;
        }
        if self
            .create_status_warning
            .as_ref()
            .is_some_and(|(_, hide_at)| now_s >= *hide_at)
        {
            self.create_status_warning = None;
        }

        self.capture_shift_drag_selection(ctx, canvas_rect, canvas, project, top_panel);
        self.draw_active_shift_selection(ctx);
        self.draw_text_editor(ctx, project, top_panel);
        self.draw_render_inflight_hint(ctx);
        self.draw_status_error(ctx, canvas_rect);
        self.draw_status_warning(ctx, canvas_rect);
    }

    pub(super) fn capture_shift_drag_selection(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        if self.loading_rx.is_some()
            || self.create_editor.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
        {
            return;
        }
        let shift_down = ctx.input(|i| i.modifiers.shift);
        let selection_active = self.create_selection.is_some();
        if !shift_down && !selection_active {
            return;
        }

        // Middle order (not Foreground): the capture overlay must sit below the Foreground
        // panels so the panels win the z-order hit-test over their own rects and their Wheel
        // widgets keep receiving hover/scroll; Middle still sits above the Background canvas,
        // so drag-selection over bare canvas is unaffected.
        let capture_layer = shift_drag_capture_layer_id();
        egui::Area::new(capture_layer.id)
            .order(capture_layer.order)
            .fixed_pos(canvas_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(canvas_rect.size());
                let local_rect = Rect::from_min_size(Pos2::ZERO, canvas_rect.size());
                let sense = if shift_down {
                    egui::Sense::click_and_drag()
                } else {
                    egui::Sense::hover()
                };
                let response =
                    ui.interact(local_rect, ui.id().with("typing_text_shift_drag"), sense);

                if shift_down
                    && response.drag_started()
                    && let Some(pos) = response.interact_pointer_pos()
                    && contains_any_page(canvas, project, pos)
                {
                    self.create_selection = Some(TypingCreateSelection {
                        start: pos,
                        current: pos,
                    });
                }

                if let Some(selection) = self.create_selection.as_mut()
                    && let Some(pos) = ctx.input(|i| i.pointer.latest_pos())
                {
                    selection.current = pos;
                }

                let should_finish =
                    self.create_selection.is_some() && (response.drag_stopped() || !shift_down);
                if should_finish && let Some(selection) = self.create_selection.take() {
                    let rect = selection.rect();
                    if rect.width() >= TEXT_CREATE_SELECTION_MIN_SIDE_PX
                        && rect.height() >= TEXT_CREATE_SELECTION_MIN_SIDE_PX
                    {
                        self.open_text_editor_for_selection(ctx, canvas, project, top_panel, rect);
                    }
                }
            });
    }

    pub(super) fn draw_active_shift_selection(&self, ctx: &egui::Context) {
        let Some(selection) = self.create_selection else {
            return;
        };
        let rect = selection.rect();
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("typing_text_shift_selection_painter"),
        ));
        painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(245, 210, 60, 52));
        painter.rect_stroke(
            rect,
            0.0,
            Stroke::new(2.0, Color32::from_rgb(245, 210, 60)),
            egui::StrokeKind::Outside,
        );
    }

    pub(super) fn open_text_editor_for_selection(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
        scene_selection_rect: Rect,
    ) {
        let Some((page_idx, page_rect, scene_rect)) =
            resolve_selection_to_page(canvas, project, scene_selection_rect)
        else {
            self.set_create_error(ctx, t!("typing.create.selection_must_cross_page_error"));
            return;
        };

        let width_px = selection_width_in_source_px(canvas, page_idx, page_rect, scene_rect);
        if width_px == 0 {
            self.set_create_error(ctx, t!("typing.create.selection_width_error"));
            return;
        }

        let center_page_px = selection_center_page_px(page_rect, scene_rect, canvas.zoom());
        let seed_text =
            pick_bubble_text_for_selection(&project.bubbles, page_idx, scene_rect, page_rect)
                .unwrap_or_default();

        let mut font_size_px = 24.0;
        if let Some(spec) = top_panel.create_editor_font_spec() {
            font_size_px = spec.ui_font_size_px.clamp(8.0, 128.0);
            self.request_editor_font(&spec, top_panel.font_provider());
        }

        self.create_editor = Some(TypingCreateTextEditor {
            page_idx,
            scene_rect,
            center_page_px,
            width_px,
            text: seed_text,
            // Filled in by `poll_editor_font_request` as soon as the background resolve
            // lands; until then the field draws in the default UI font.
            font_family: None,
            font_size_px,
            needs_focus: true,
            window_focused_last_frame: ctx.input(|input| input.viewport().focused.unwrap_or(true)),
        });
        self.create_status_error = None;
    }

    /// Starts a BACKGROUND resolve of the font the on-canvas create editor should draw in.
    ///
    /// The bytes come from `fonts` — the panel's `FontProvider`, i.e. the SAME resolution
    /// the renderer performs for that identity — so the editor cannot preview a different
    /// file than the one the render will use, and nothing about it is keyed by a path.
    /// A provider cache MISS reads the font file, which is why this never happens inline:
    /// the GUI thread must not do file I/O (`CLAUDE.md` §5). The editor draws in the
    /// default UI font until [`Self::poll_editor_font_request`] picks the result up; a
    /// previous in-flight request is dropped, so the newest selection wins.
    pub(super) fn request_editor_font(
        &mut self,
        spec: &TypingEditorFontSpec,
        fonts: Arc<dyn FontProvider>,
    ) {
        let font_identity = spec.font_identity.clone();
        let face_index = spec.face_index;
        let (tx, rx) = mpsc::channel::<Option<FontContent>>();
        let worker_identity = font_identity.clone();
        let spawn_result = thread::Builder::new()
            .name("typing-editor-font-resolve".to_string())
            .spawn(move || {
                // A failed send just means the editor was closed or superseded before the
                // bytes arrived; the provider keeps them cached either way.
                let _ = tx.send(fonts.resolve(&worker_identity));
            });
        match spawn_result {
            Ok(_handle) => {
                self.editor_font_request = Some(TypingEditorFontRequest {
                    font_identity,
                    face_index,
                    rx,
                });
            }
            Err(err) => {
                // The editor still works — it simply shows the default UI font — but the
                // reason must be diagnosable rather than silent.
                self.editor_font_request = None;
                crate::runtime_log::log_error(format!(
                    "typing: failed to spawn the on-canvas editor font resolver; the text \
                     field falls back to the interface font. Font: {} Error: {err}",
                    spec.font_identity
                ));
            }
        }
    }

    /// Picks up a finished [`Self::request_editor_font`] without blocking and registers the
    /// bytes with egui (which must happen on the GUI thread, where the `Context` lives).
    ///
    /// The family the bytes go under is `editor_font_family_name(identity, content id, face
    /// index)` — the SAME key the registration is memoized by, and a pure function of it, so a
    /// layer rebuilt by a project reload inside the same `egui::Context` re-derives the name
    /// instead of re-issuing a stale one (see that function's invariant). The content id is
    /// what the provider reported for the bytes it actually served, so a font file replaced
    /// under the same PostScript name gets its own family instead of being drawn forever from
    /// the snapshot egui already holds.
    ///
    /// The bytes are handed to egui at most once per key and per app instance; a repeat under an
    /// existing name is a no-op in egui anyway. An identity that did not resolve leaves the
    /// editor on the default UI font. Call once per frame, including on frames with no open
    /// editor, so a late result is not stranded in the channel.
    pub(super) fn poll_editor_font_request(&mut self, ctx: &egui::Context) {
        let Some(request) = self.editor_font_request.as_ref() else {
            return;
        };
        let content = match request.rx.try_recv() {
            Ok(content) => content,
            Err(TryRecvError::Empty) => {
                // The result arrives on a worker thread, which schedules no frame of its
                // own: without this the editor would keep the fallback font until the user
                // happened to move the mouse or type.
                ctx.request_repaint();
                return;
            }
            Err(TryRecvError::Disconnected) => {
                self.editor_font_request = None;
                return;
            }
        };
        let Some(request) = self.editor_font_request.take() else {
            return;
        };
        let Some(content) = content else {
            return;
        };

        let cache_key = (
            request.font_identity,
            content.content_id,
            request.face_index,
        );
        // Derived from the key, never minted: the name has to survive this layer's lifetime,
        // which the egui `Context` outlives (`editor_font_family_name`).
        let font_name = editor_font_family_name(&cache_key.0, cache_key.1, cache_key.2);
        if self.editor_font_cache.insert(cache_key) {
            // First time THIS instance hands these bytes over. egui owns the bytes it renders
            // from, so the shared buffer is copied once per registration (never per frame).
            // A repeat after a reload would be harmless — `add_font` keeps the existing
            // registration and one key always stands for one set of bytes — the memo only
            // spares the copy.
            let font_bytes = content.data.as_ref().as_ref().to_vec();
            let mut font_data = egui::FontData::from_owned(font_bytes);
            font_data.index = u32::try_from(request.face_index).unwrap_or(0);
            ctx.add_font(egui::epaint::text::FontInsert::new(
                font_name.as_str(),
                font_data,
                vec![egui::epaint::text::InsertFontFamily {
                    family: egui::FontFamily::Name(font_name.clone().into()),
                    priority: egui::epaint::text::FontPriority::Highest,
                }],
            ));
        }
        if let Some(editor) = self.create_editor.as_mut() {
            editor.font_family = Some(egui::FontFamily::Name(font_name.into()));
        }
    }

    pub(super) fn draw_text_editor(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
    ) {
        // Runs before the early return: the resolve must be able to finish (and its family
        // to be cached) even on a frame where the editor has already been closed.
        self.poll_editor_font_request(ctx);
        if self.create_editor.is_none() {
            return;
        }

        let editor_rect = {
            let editor = self.create_editor.as_mut().expect("checked above");
            let desired_rect = Rect::from_min_size(
                editor.scene_rect.min,
                egui::vec2(
                    editor.scene_rect.width().max(TEXT_EDITOR_MIN_WIDTH_PX),
                    editor.scene_rect.height().max(TEXT_EDITOR_MIN_HEIGHT_PX),
                ),
            );
            let text_edit_id = Id::new((
                "typing_text_editor_input",
                editor.page_idx,
                editor.scene_rect.min.x.to_bits(),
                editor.scene_rect.min.y.to_bits(),
            ));
            let area_response = egui::Area::new(Id::new((
                "typing_text_editor_area",
                editor.page_idx,
                editor.scene_rect.min.x.to_bits(),
                editor.scene_rect.min.y.to_bits(),
            )))
            .order(egui::Order::Foreground)
            .fixed_pos(desired_rect.min)
            .show(ctx, |ui| {
                ui.set_min_size(desired_rect.size());
                ui.set_max_size(desired_rect.size());
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(235, 200, 85)))
                    .show(ui, |ui| {
                        ui.set_min_size(desired_rect.size());
                        let family = editor
                            .font_family
                            .clone()
                            .filter(|family| is_font_family_bound(ctx, family))
                            .unwrap_or(egui::FontFamily::Proportional);
                        // Read before the field takes `editor.text` mutably.
                        let font_size_px = editor.display_font_size_px();
                        // The overlay-creation field is where a rare script normally
                        // enters the app, and when the selected font is not (yet) bound
                        // it is drawn with `Proportional`, which carries only the `core`
                        // chain. Arm the extended tier from the assembled buffer so the
                        // field does not show tofu for text the page renders correctly.
                        // Cheap and idempotent — see `ui_fonts::ensure_covers`.
                        crate::ui_fonts::ensure_covers(ctx, &editor.text);
                        let edit = egui::TextEdit::multiline(&mut editor.text)
                            .id(text_edit_id)
                            .font(egui::FontId::new(font_size_px, family))
                            .desired_width(f32::INFINITY)
                            .desired_rows(1)
                            .lock_focus(true)
                            .frame(egui::Frame::NONE);
                        let output = edit.show(ui);
                        let viewport_focused =
                            ctx.input(|input| input.viewport().focused.unwrap_or(true));
                        let clicked_inside_editor = ctx.input(|input| {
                            input.pointer.primary_clicked()
                                && input
                                    .pointer
                                    .interact_pos()
                                    .is_some_and(|pos| desired_rect.contains(pos))
                        });
                        if editor.needs_focus
                            || (viewport_focused && !editor.window_focused_last_frame)
                            || (clicked_inside_editor && !output.response.has_focus())
                        {
                            output.response.request_focus();
                            editor.needs_focus = false;
                        }
                        editor.window_focused_last_frame = viewport_focused;
                    });
            });
            area_response.response.rect
        };

        let clicked_outside = ctx.input(|i| {
            i.pointer.primary_clicked()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|pos| !editor_rect.contains(pos))
        });
        if clicked_outside && let Some(finished_editor) = self.create_editor.take() {
            self.start_create_overlay_render(ctx, project, top_panel, finished_editor);
        }
    }

    pub(super) fn start_create_overlay_render(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        top_panel: &TypingTopPanelState,
        editor: TypingCreateTextEditor,
    ) {
        if editor.text.trim().is_empty() {
            self.create_status_error = None;
            return;
        }

        let (render_params, render_data_json) =
            match top_panel.build_create_text_render_bundle(editor.text.clone(), editor.width_px) {
                Ok(bundle) => bundle,
                Err(err) => {
                    self.set_create_error(ctx, err);
                    return;
                }
            };

        let request = TypingCreateOverlayRequest {
            text_images_dir: project.paths.unsaved_layers_dir.clone(),
            page_idx: editor.page_idx,
            center_page_px: editor.center_page_px,
            render_params,
            render_data_json,
            font_provider: Arc::clone(&self.font_provider),
        };
        crate::trace_log!(
            cat::SYNC,
            "create_overlay_render dispatch page={} center=({:.1},{:.1}) width_px={}",
            editor.page_idx,
            editor.center_page_px[0],
            editor.center_page_px[1],
            editor.width_px
        );
        let (tx, rx) = mpsc::channel::<Result<TypingOverlayDecoded, String>>();
        thread::spawn(move || {
            let result = render_and_store_created_overlay(request);
            let _ = tx.send(result);
        });
        self.create_render_state = Some(TypingCreateRenderState {
            rx,
            scene_rect: Some(editor.scene_rect),
        });
        self.create_status_error = None;
    }

    pub(super) fn request_create_image_overlay(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        page_idx: usize,
        center_page_px: [f32; 2],
        request: TypingCreateImageRequest,
    ) {
        if self.loading_rx.is_some()
            || self.create_render_state.is_some()
            || self.create_raster_state.is_some()
            || self.raster_effects_state.is_some()
            || self.create_raster_state.is_some()
        {
            self.set_create_error(ctx, t!("typing.create.wait_current_operation_error"));
            return;
        }
        if project.pages.is_empty() {
            self.set_create_error(ctx, t!("typing.create.no_pages_error"));
            return;
        }
        let target_page_idx = page_idx.min(project.pages.len().saturating_sub(1));
        let source = match request {
            TypingCreateImageRequest::FromClipboard => TypingCreateImageSource::Clipboard,
            TypingCreateImageRequest::FromFile(path) => TypingCreateImageSource::File(path),
        };
        // DATA-SAFETY (anti-resurrection): the worker's `add_page_raster` seeds an unstaged page from the
        // COMMITTED manifest (so a typeset page keeps its text — the drop fix). But committed is STALE
        // w.r.t. an in-session deletion: when the user deleted the page's LAST text, the placement-save
        // skipped the now-empty page (`pages_with_text` no longer lists it), so the deletion lived only
        // in the doc. Seeding committed would RESURRECT it. Fix: flush the target page's CURRENT doc text
        // to staging NOW (main thread, has the doc) — for a deleted-last-text page this writes it
        // PRESENT-but-EMPTY, so `ensure_page_staged` sees the page present and does NOT seed stale text;
        // for a typeset page it writes the current text, which the new raster is then added on top of.
        self.flush_target_page_text_to_staging(target_page_idx);

        // External images now become RASTER layers (in layers.json), not text/image overlays, so
        // they are first-class in both the typing and PS editor tabs.
        let create_request = TypingCreateRasterRequest {
            layers_dir: project.paths.unsaved_layers_dir.clone(),
            fallback_dir: Some(project.paths.layers_dir.clone()),
            page_idx: target_page_idx,
            center_page_px,
            source,
        };
        let (tx, rx) = mpsc::channel::<Result<TypingCreatedRaster, String>>();
        thread::spawn(move || {
            let _ = tx.send(render_and_store_created_raster(create_request));
        });
        self.create_raster_state = Some(TypingCreateRasterState { rx });
        self.create_status_error = None;
    }

    pub(super) fn draw_render_inflight_hint(&self, ctx: &egui::Context) {
        let Some(state) = self.create_render_state.as_ref() else {
            return;
        };
        let Some(scene_rect) = state.scene_rect else {
            return;
        };
        let hint_pos = scene_rect.center() - egui::vec2(76.0, 18.0);
        egui::Area::new("typing_text_editor_render_hint".into())
            .order(egui::Order::Foreground)
            .fixed_pos(hint_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("typing.create.rendering_text_hint"));
                    });
                });
            });
    }

    pub(super) fn draw_status_error(&self, ctx: &egui::Context, canvas_rect: Rect) {
        let Some((message, _)) = self.create_status_error.as_ref() else {
            return;
        };
        egui::Area::new("typing_text_editor_error".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-220.0, 16.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(240, 110, 110)))
                    .show(ui, |ui| {
                        ui.colored_label(Color32::from_rgb(240, 110, 110), message);
                    });
            });
    }

    pub(super) fn draw_status_warning(&self, ctx: &egui::Context, canvas_rect: Rect) {
        let Some((message, _)) = self.create_status_warning.as_ref() else {
            return;
        };
        egui::Area::new("typing_text_editor_warning".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-220.0, 52.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, Color32::from_rgb(232, 188, 66)))
                    .show(ui, |ui| {
                        ui.colored_label(Color32::from_rgb(232, 188, 66), message);
                    });
            });
    }

    pub(super) fn set_create_error(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        let now_s = ctx.input(|i| i.time);
        self.create_status_error = Some((message.into(), now_s + TEXT_EDITOR_STATUS_ERROR_SECONDS));
    }

    pub(super) fn set_create_warning(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        let now_s = ctx.input(|i| i.time);
        self.create_status_warning =
            Some((message.into(), now_s + TEXT_EDITOR_STATUS_ERROR_SECONDS));
    }

    pub(super) fn insert_runtime_overlay(&mut self, decoded: TypingOverlayDecoded) {
        let idx = self.overlays.len();
        // Build the doc Text node for a TEXT overlay (the doc is the source of truth, so it joins the
        // unified Z stack and re-projects like the rest). Image overlays remain local-only → no node.
        //
        // CRITICAL ordering: build the node here, but ADD it to the doc only AFTER the runtime is pushed
        // into `self.overlays` (below). `route_to_doc` reprojects via `sync_from_doc`, whose CREATE/None
        // branch MATERIALIZES a runtime for any doc Text node that has no matching local runtime yet. If
        // we added the node before pushing the runtime, that branch would create a SECOND runtime for the
        // same uid — a duplicate text layer (one doc-backed, one orphaned). The duplicate is invisible at
        // create time (both render the same image, perfectly overlapping) but becomes visible on the
        // first advanced-form apply: `sync_from_doc` reconciles only the FIRST uid match, leaving the
        // other stuck on the pre-form render.
        let pending_text_node = if decoded.kind == TypingOverlayKind::Text
            && decoded.size_px[0] > 0
            && decoded.size_px[1] > 0
            && decoded.rgba.len() == decoded.size_px[0] * decoded.size_px[1] * 4
        {
            use crate::models::layer_model::layer_doc::{LayerNode, NodeBody, NodeKind};
            let page_idx = decoded.page_idx;
            let uid = decoded.uid.clone();
            let name = decoded
                .render_data_json
                .as_ref()
                .and_then(|v| v.get("text"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(40).collect::<String>())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| t!("typing.layers.text_row_label").to_string());
            let transform = crate::models::layer_model::manifest::TransformRec {
                cx: decoded.center_page_px[0],
                cy: decoded.center_page_px[1],
                rotation: decoded.angle_deg.to_radians(),
                scale: decoded.user_scale,
            };
            let deform = decoded.deform_mesh.as_ref().map(|m| {
                crate::models::layer_model::manifest::DeformRec {
                    cols: m.cols,
                    rows: m.rows,
                    points_px: m.points_px.clone(),
                }
            });
            let image =
                ColorImage::from_rgba_unmultiplied(decoded.size_px, decoded.rgba.as_slice());
            let render_data = decoded.render_data_json.clone().unwrap_or(Value::Null);
            let node = LayerNode {
                uid: uid.clone(),
                name,
                kind: NodeKind::Text,
                z: 0, // set on top by add_node
                visible: true,
                opacity: 1.0,
                group_uid: None,
                // The typing tab's «Группа текста N» axis — carried so the doc flush persists it.
                text_layer_idx: u32::try_from(decoded.layer_idx).ok(),
                transform,
                deform,
                generation: 0,
                // A freshly rendered overlay: mark dirty so the doc flush writes its rendered PNG.
                pixels_dirty: true,
                body: NodeBody::Text {
                    render_data,
                    image,
                    is_image: matches!(decoded.kind, TypingOverlayKind::Image),
                    payload_uid: uid,
                    // Carry the overlay's mask-clip flag so the v3 inline payload persists it.
                    mask_clip: Some(decoded.mask_clip_enabled),
                    // The centers measured for THESE pixels. The `route_to_doc` below re-projects
                    // this node onto the runtime pushed after it, so the doc must already carry them
                    // or the projection would hand the runtime empty centers.
                    extra_centers: decoded.extra.clone(),
                    // A freshly created overlay has no guide frame yet; the assist creates one lazily
                    // on the canvas and the placement sync pushes it here.
                    centering_frame: None,
                },
            };
            Some((page_idx, node))
        } else {
            None
        };
        self.overlays.push(TypingOverlayRuntime {
            uid: decoded.uid,
            kind: decoded.kind,
            page_idx: decoded.page_idx,
            center_page_px: decoded.center_page_px,
            mask_clip_enabled: decoded.mask_clip_enabled,
            layer_idx: decoded.layer_idx,
            user_scale: decoded.user_scale,
            angle_deg: decoded.angle_deg,
            deform_mesh: decoded.deform_mesh,
            file_name: decoded.file_name,
            original_file_name: decoded.original_file_name,
            render_data_json: decoded.render_data_json,
            size_px: decoded.size_px,
            source_rgba: decoded.rgba,
            // Carry the freshly-created overlay's mean/median centers for the centering-assist
            // marker/binding (all-`None` unless assist requested them).
            extra: decoded.extra,
            centering_frame: None,
            texture: None,
            display_texture_stale: true,
            last_texture_used_frame: 0,
        });
        // Now that the runtime is in `self.overlays`, add the doc node. `route_to_doc`'s reproject finds
        // the runtime by uid and RECONCILES it (no duplicate materialized). See the ordering note above.
        if let Some((page_idx, node)) = pending_text_node {
            self.route_to_doc(page_idx, move |doc| {
                doc.add_node(page_idx, node);
            });
        }
        self.queue_overlay_texture_upload(idx);
        self.selected_overlay_idx = Some(idx);
        self.transform_mode_overlay_idx = None;
        self.drag_state = None;
    }

    /// Computes the mask-clipped DISPLAY image for every mask-clip-enabled raster whose clipped image
    /// is not yet cached, and drops its GPU texture so `draw_one_raster_layer` re-uploads the clipped
    /// version. Runs before the overlay upload (which already has the mask layer). Mirrors the overlay
    /// clip path (`clip_overlay_rgba_if_needed` with the layer's deform mesh as page-relative UV; an
    /// affine raster uses an identity quad mesh derived from its transform).
    pub(super) fn prepare_raster_mask_clips(&mut self, mask_layer: &TypingMaskLayer) {
        let pages: Vec<usize> = self.raster_layers_by_page.keys().copied().collect();
        for page_idx in pages {
            let Some(page_size) = mask_layer.page_mask_size(page_idx) else {
                continue;
            };
            let Some(layers) = self.raster_layers_by_page.get_mut(&page_idx) else {
                continue;
            };
            for layer in layers.iter_mut() {
                if !layer.mask_clip_enabled {
                    layer.clipped_image = None;
                    continue;
                }
                if layer.clipped_image.is_some() {
                    continue; // already computed for this generation
                }
                let [w, h] = layer.image.size;
                if w == 0 || h == 0 {
                    continue;
                }
                // Deform mesh in page-relative UV (the raster's mesh, or an identity quad for affine).
                let mesh = match &layer.deform {
                    Some(rec) => TypingOverlayDeformMesh::from_deform_rec(rec, page_size),
                    None => Some(default_deform_mesh_for_page(
                        [layer.transform.cx, layer.transform.cy],
                        layer.image.size,
                        layer.transform.scale,
                        layer.transform.rotation.to_degrees(),
                        page_size,
                    )),
                };
                let Some(mesh) = mesh else { continue };
                let points_uv: Vec<[f32; 2]> = mesh
                    .points_px
                    .iter()
                    .map(|&p| page_px_to_uv(p, page_size))
                    .collect();
                // Clip straight into a `ColorImage`, reusing the previous buffer when its size still
                // matches. This path runs EVERY frame a mask-clipped raster is being moved, so the
                // old `ColorImage -> Vec<u8> -> clip's own to_vec -> ColorImage` chain cost three
                // full W×H×4 buffers plus two per-pixel alpha conversions per frame — tens of MB on
                // a full-page raster, on the GUI thread. One reused buffer and one copy remain.
                let mut clipped = match layer.clipped_image.take() {
                    Some(previous) if previous.size == layer.image.size => previous,
                    Some(_) | None => layer.image.clone(),
                };
                clipped.pixels.copy_from_slice(&layer.image.pixels);
                if mask_layer.clip_overlay_color_image_in_place(
                    page_idx,
                    &mut clipped,
                    mesh.cols,
                    mesh.rows,
                    &points_uv,
                ) {
                    layer.clipped_image = Some(clipped);
                    // Force re-upload with the clipped pixels.
                    layer.texture = None;
                }
            }
        }
    }

    pub(super) fn upload_pending_textures(
        &mut self,
        ctx: &egui::Context,
        mask_layer: &TypingMaskLayer,
    ) -> bool {
        self.prepare_raster_mask_clips(mask_layer);
        let mut uploaded_any = false;
        let mut uploaded_textures = 0usize;
        let mut uploaded_bytes = 0usize;

        while uploaded_textures < TEXT_OVERLAY_UPLOAD_TEXTURE_BUDGET_PER_FRAME
            && uploaded_bytes < TEXT_OVERLAY_UPLOAD_BYTES_BUDGET_PER_FRAME
        {
            let Some(idx) = self.pending_upload_indices.pop_front() else {
                break;
            };
            self.pending_upload_set.remove(&idx);
            let Some(overlay) = self.overlays.get_mut(idx) else {
                continue;
            };
            if overlay.texture.is_some() && !overlay.display_texture_stale {
                continue;
            }
            if overlay.source_rgba.is_empty() {
                continue;
            };
            if overlay.size_px[0] == 0 || overlay.size_px[1] == 0 {
                continue;
            }
            if overlay.source_rgba.len() != overlay.size_px[0] * overlay.size_px[1] * 4 {
                continue;
            }

            let display_rgba = if overlay.mask_clip_enabled {
                if let Some(page_size) = mask_layer.page_mask_size(overlay.page_idx) {
                    let deform_mesh = overlay_deform_mesh_for_page(overlay, page_size);
                    let deform_mesh_points_uv = deform_mesh
                        .points_px
                        .iter()
                        .map(|&point| page_px_to_uv(point, page_size))
                        .collect::<Vec<_>>();
                    mask_layer
                        .clip_overlay_rgba_if_needed(
                            overlay.page_idx,
                            overlay.size_px,
                            &overlay.source_rgba,
                            deform_mesh.cols,
                            deform_mesh.rows,
                            deform_mesh_points_uv.as_slice(),
                        )
                        .unwrap_or_else(|| overlay.source_rgba.clone())
                } else {
                    overlay.source_rgba.clone()
                }
            } else {
                overlay.source_rgba.clone()
            };

            let image = egui::ColorImage::from_rgba_unmultiplied(
                [overlay.size_px[0], overlay.size_px[1]],
                &display_rgba,
            );
            if let Some(texture) = overlay.texture.as_mut() {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                let texture = ctx.load_texture(
                    format!(
                        "typing-text-overlay-{}-{}-{}",
                        overlay.page_idx, idx, overlay.file_name
                    ),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                overlay.texture = Some(texture);
            }
            overlay.display_texture_stale = false;

            uploaded_any = true;
            uploaded_textures += 1;
            uploaded_bytes += display_rgba.len();
        }

        if uploaded_any {
            crate::trace_log!(
                cat::RENDER,
                "upload_overlay_textures count={} bytes={} pending_remaining={}",
                uploaded_textures,
                uploaded_bytes,
                self.pending_upload_indices.len()
            );
        }
        uploaded_any
    }

    pub(super) fn ensure_overlay_deform_mesh(&mut self, overlay_idx: usize, view: PageView) -> bool {
        let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
            return false;
        };
        let page_size = view.page_size_px();
        if overlay.deform_mesh.is_none() {
            overlay.deform_mesh = Some(default_overlay_deform_mesh(overlay, view));
        } else if let Some(mesh) = overlay.deform_mesh.as_ref() {
            let normalized = normalize_deform_mesh_resolution(mesh, page_size);
            if &normalized != mesh {
                overlay.deform_mesh = Some(normalized);
            }
        }
        sync_overlay_center_from_deform_mesh(overlay, page_size);
        true
    }

    pub(super) fn queue_overlay_texture_upload(&mut self, idx: usize) {
        if idx >= self.overlays.len() {
            return;
        }
        if self.pending_upload_set.insert(idx) {
            self.pending_upload_indices.push_back(idx);
        }
    }

    pub(super) fn mark_overlay_pixels_dirty(&mut self, idx: usize) {
        if let Some(overlay) = self.overlays.get_mut(idx) {
            overlay.display_texture_stale = true;
        } else {
            return;
        }
        self.queue_overlay_texture_upload(idx);
    }

    pub(super) fn mark_overlay_geometry_changed(&mut self, idx: usize, defer_mask_refresh: bool) {
        let should_refresh = if let Some(overlay) = self.overlays.get_mut(idx) {
            if !overlay.mask_clip_enabled {
                false
            } else {
                overlay.display_texture_stale = true;
                true
            }
        } else {
            return;
        };
        if should_refresh && !defer_mask_refresh {
            self.queue_overlay_texture_upload(idx);
        }
    }

    pub(super) fn flush_overlay_texture_if_stale(&mut self, idx: usize) {
        if self
            .overlays
            .get(idx)
            .is_some_and(|overlay| overlay.display_texture_stale)
        {
            self.queue_overlay_texture_upload(idx);
        }
    }

    pub(super) fn mark_page_texture_dirty(&mut self, page_idx: usize) {
        for idx in 0..self.overlays.len() {
            if self.overlays[idx].page_idx == page_idx && self.overlays[idx].mask_clip_enabled {
                self.mark_overlay_pixels_dirty(idx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Test `FontProvider` that can be made to BLOCK inside `resolve`, so a test can prove
    /// the call does not happen on the calling (GUI) thread.
    ///
    /// `finished` flips only after `resolve` returns, and `gate` is what holds it: while
    /// nothing has been sent on the gate's sender, `resolve` cannot complete.
    struct GatedFontProvider {
        gate: Mutex<Receiver<()>>,
        finished: Arc<AtomicBool>,
        content_id: u64,
    }

    impl FontProvider for GatedFontProvider {
        fn resolve(&self, name: &str) -> Option<FontContent> {
            // A poisoned lock cannot happen here (the test never panics while holding it),
            // and recovering keeps the test failure readable instead of a second panic.
            let gate = match self.gate.lock() {
                Ok(gate) => gate,
                Err(poisoned) => poisoned.into_inner(),
            };
            let _ = gate.recv();
            self.finished.store(true, Ordering::SeqCst);
            Some(FontContent {
                name: name.to_string(),
                original_name: name.to_string(),
                data: Arc::new(Vec::new()),
                face_index: 0,
                content_id: self.content_id,
            })
        }
    }

    /// Provider that answers immediately with a fixed content id, for the cache-key tests.
    struct FixedFontProvider {
        content_id: u64,
    }

    impl FontProvider for FixedFontProvider {
        fn resolve(&self, name: &str) -> Option<FontContent> {
            Some(FontContent {
                name: name.to_string(),
                original_name: name.to_string(),
                data: Arc::new(Vec::new()),
                face_index: 0,
                content_id: self.content_id,
            })
        }
    }

    fn spec(identity: &str) -> TypingEditorFontSpec {
        TypingEditorFontSpec {
            font_identity: identity.to_string(),
            face_index: 0,
            ui_font_size_px: 24.0,
        }
    }

    /// Minimal OPEN create-editor, so a test can observe the family the poll installs on the
    /// text field (the poll only writes it when an editor is open).
    fn open_editor_stub() -> TypingCreateTextEditor {
        TypingCreateTextEditor {
            page_idx: 0,
            scene_rect: Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 50.0)),
            center_page_px: [0.0, 0.0],
            width_px: 100,
            text: String::new(),
            font_family: None,
            font_size_px: 24.0,
            needs_focus: false,
            window_focused_last_frame: true,
        }
    }

    /// A layer whose create editor is OPEN — the state in which `poll_editor_font_request`
    /// installs the resolved family on the text field.
    fn layer_with_open_editor() -> TypingTextOverlayLayer {
        TypingTextOverlayLayer {
            create_editor: Some(open_editor_stub()),
            ..TypingTextOverlayLayer::default()
        }
    }

    /// The family the OPEN create editor's text field is currently set to draw in.
    fn editor_family_of(layer: &TypingTextOverlayLayer) -> egui::FontFamily {
        layer
            .create_editor
            .as_ref()
            .and_then(|editor| editor.font_family.clone())
            .expect("the resolved font must reach the open editor's text field")
    }

    /// Polls until the request has been consumed, without blocking forever if it never is.
    fn poll_until_settled(layer: &mut TypingTextOverlayLayer, ctx: &egui::Context) -> bool {
        for _ in 0..2000 {
            layer.poll_editor_font_request(ctx);
            if layer.editor_font_request.is_none() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    /// Obtaining the editor font's BYTES is `fs::read` on a provider cache miss, so it must
    /// never run on the GUI thread (`CLAUDE.md` §5). `request_editor_font` therefore only
    /// dispatches: with the provider held inside `resolve`, the call still returns and the
    /// poll reports nothing yet — proof the read is not inline.
    #[test]
    fn requesting_the_editor_font_does_not_resolve_on_the_calling_thread() {
        let ctx = egui::Context::default();
        let (release, gate) = mpsc::channel::<()>();
        let finished = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(GatedFontProvider {
            gate: Mutex::new(gate),
            finished: Arc::clone(&finished),
            content_id: 7,
        });

        let mut layer = TypingTextOverlayLayer::default();
        layer.request_editor_font(&spec("Alpha-Regular"), provider);
        assert!(
            !finished.load(Ordering::SeqCst),
            "the resolve must not have run on the calling thread"
        );
        layer.poll_editor_font_request(&ctx);
        assert!(
            layer.editor_font_request.is_some(),
            "an unfinished resolve must leave the request in flight, not block the caller"
        );
        assert!(
            layer.editor_font_cache.is_empty(),
            "nothing may be registered before the bytes arrive"
        );

        release.send(()).expect("the worker holds the receiver");
        assert!(poll_until_settled(&mut layer, &ctx), "the result must land");
        assert_eq!(
            layer.editor_font_cache.len(),
            1,
            "the resolved font's bytes are handed to egui exactly once, on the GUI thread"
        );
    }

    /// The editor's egui registration is keyed by the CONTENT the provider served, not by
    /// `(identity, face)` alone: a font file replaced under the same PostScript name used to
    /// keep the first registration forever, so the editor drew the old typeface while the
    /// renderer drew the new one.
    #[test]
    fn the_editor_font_cache_is_keyed_by_the_resolved_content() {
        let ctx = egui::Context::default();
        let mut layer = layer_with_open_editor();

        layer.request_editor_font(&spec("Alpha-Regular"), Arc::new(FixedFontProvider { content_id: 1 }));
        assert!(poll_until_settled(&mut layer, &ctx));
        let first_key = ("Alpha-Regular".to_string(), 1_u64, 0_usize);
        assert_eq!(layer.editor_font_cache.len(), 1);
        assert!(layer.editor_font_cache.contains(&first_key));
        let after_first = editor_family_of(&layer);

        // Same identity, same bytes: the existing family is reused (egui `add_font` never
        // evicts, so a re-registration per open would leak atlases).
        layer.request_editor_font(&spec("Alpha-Regular"), Arc::new(FixedFontProvider { content_id: 1 }));
        assert!(poll_until_settled(&mut layer, &ctx));
        assert_eq!(
            layer.editor_font_cache.len(),
            1,
            "unchanged bytes must not hand the bytes over a second time"
        );
        assert_eq!(
            editor_family_of(&layer),
            after_first,
            "unchanged bytes must keep the editor on the same family"
        );

        // Same identity, DIFFERENT bytes: a fresh family, so the editor shows the new file.
        layer.request_editor_font(&spec("Alpha-Regular"), Arc::new(FixedFontProvider { content_id: 2 }));
        assert!(poll_until_settled(&mut layer, &ctx));
        assert_eq!(
            layer.editor_font_cache.len(),
            2,
            "replaced bytes behind one identity must register a new family"
        );
        assert!(
            layer.editor_font_cache.contains(&first_key),
            "the previous registration is still live in this egui context"
        );
        assert_ne!(
            editor_family_of(&layer),
            after_first,
            "replaced bytes must move the editor onto the new family"
        );
    }

    /// An identity the provider cannot resolve leaves the editor on the default UI font and
    /// registers nothing — the request is still consumed, so it cannot leak.
    #[test]
    fn an_unresolvable_editor_font_registers_nothing() {
        struct EmptyProvider;
        impl FontProvider for EmptyProvider {
            fn resolve(&self, _name: &str) -> Option<FontContent> {
                None
            }
        }

        let ctx = egui::Context::default();
        let mut layer = layer_with_open_editor();
        layer.request_editor_font(&spec("Ghost-Regular"), Arc::new(EmptyProvider));
        assert!(poll_until_settled(&mut layer, &ctx));
        assert!(layer.editor_font_cache.is_empty());
        assert!(
            layer
                .create_editor
                .as_ref()
                .is_some_and(|editor| editor.font_family.is_none()),
            "an unresolvable identity must leave the field on the default UI font"
        );
    }

    /// REGRESSION: a project reload (a structural page-manager operation, «Перезагрузить
    /// проект») rebuilds the whole `MangaApp` — and with it this layer — inside the SAME
    /// `egui::Context`. While the family name was a per-instance sequence number, the rebuilt
    /// layer re-issued `typing-editor-font-1`, `Context::add_font` kept the bytes registered
    /// under that name in the previous life of the app (it never compares bytes), and the editor
    /// drew a FOREIGN typeface. Two layers over one context must therefore never name two
    /// different fonts alike.
    #[test]
    fn two_layers_over_one_context_do_not_share_a_family_name() {
        let ctx = egui::Context::default();

        let mut before_reload = layer_with_open_editor();
        before_reload.request_editor_font(
            &spec("Alpha-Regular"),
            Arc::new(FixedFontProvider { content_id: 1 }),
        );
        assert!(poll_until_settled(&mut before_reload, &ctx));
        let first_family = editor_family_of(&before_reload);

        // The reload: a brand-new layer (empty memo, no counter) over the SAME context.
        let mut after_reload = layer_with_open_editor();
        after_reload.request_editor_font(
            &spec("Beta-Regular"),
            Arc::new(FixedFontProvider { content_id: 2 }),
        );
        assert!(poll_until_settled(&mut after_reload, &ctx));
        let second_family = editor_family_of(&after_reload);

        assert_ne!(
            first_family, second_family,
            "a rebuilt layer must not name a different font the way the previous one named its own"
        );

        // …and the same font asked for again after the reload keeps its family, so the bytes
        // already in the context are the right ones to reuse.
        let mut same_font_again = layer_with_open_editor();
        same_font_again.request_editor_font(
            &spec("Alpha-Regular"),
            Arc::new(FixedFontProvider { content_id: 1 }),
        );
        assert!(poll_until_settled(&mut same_font_again, &ctx));
        assert_eq!(
            editor_family_of(&same_font_again),
            first_family,
            "one font must keep one family name across instances of the layer"
        );
    }

    /// The family name is a PURE function of `(identity, content id, face index)`: equal inputs
    /// name one family, and any changed input names another. Its namespace is also disjoint from
    /// the panel combo's, whose content discriminant is a different quantity for the same font.
    #[test]
    fn the_editor_font_family_name_is_a_pure_function_of_its_key() {
        let base = editor_font_family_name("Alpha-Regular", 1, 0);
        assert_eq!(base, editor_font_family_name("Alpha-Regular", 1, 0));
        assert_ne!(base, editor_font_family_name("Beta-Regular", 1, 0));
        assert_ne!(base, editor_font_family_name("Alpha-Regular", 2, 0));
        assert_ne!(base, editor_font_family_name("Alpha-Regular", 1, 1));
        assert!(
            base.starts_with("typing-editor-font-"),
            "the prefix is what keeps the editor's namespace apart: {base}"
        );
        assert_ne!(
            base,
            crate::widgets::combo_font_family_name("Alpha-Regular", 1, 0),
            "the panel combo's names must not meet the editor's"
        );
    }

    /// An editor whose box is `scene_width_px` wide on screen for `width_px` rendered source
    /// pixels, typed in a `font_size_px` (source) font.
    fn editor_with_scale(
        scene_width_px: f32,
        width_px: u32,
        font_size_px: f32,
    ) -> TypingCreateTextEditor {
        TypingCreateTextEditor {
            scene_rect: Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(scene_width_px, 50.0),
            ),
            width_px,
            font_size_px,
            ..open_editor_stub()
        }
    }

    /// The field must show the size the RENDER will produce in that same box: the panel names a
    /// size in source pixels, the box is screen pixels, so the drawn size carries the editor's
    /// screen-per-source factor. Without it the glyphs jump the moment the render lands.
    #[test]
    fn the_field_font_size_carries_the_editor_screen_to_source_scale() {
        // 200 screen px showing 100 source px — the page is drawn at 2x.
        let zoomed_in = editor_with_scale(200.0, 100, 24.0);
        assert!(
            (zoomed_in.display_font_size_px() - 48.0).abs() < 1e-4,
            "at 2x the field must draw the panel's 24 source px as 48 screen px, got {}",
            zoomed_in.display_font_size_px()
        );

        let zoomed_out = editor_with_scale(50.0, 100, 24.0);
        assert!(
            (zoomed_out.display_font_size_px() - 12.0).abs() < 1e-4,
            "at 0.5x the same font must draw at 12 screen px, got {}",
            zoomed_out.display_font_size_px()
        );

        // 1:1 is the case the field used to assume unconditionally.
        let unzoomed = editor_with_scale(100.0, 100, 24.0);
        assert!(
            (unzoomed.display_font_size_px() - 24.0).abs() < 1e-4,
            "at 1x the panel's size must reach the field untouched, got {}",
            unzoomed.display_font_size_px()
        );

        // The scale is the BOX's, not a constant: a wider render behind the same box shrinks it.
        let wide_render = editor_with_scale(100.0, 400, 24.0);
        assert!(
            (wide_render.display_font_size_px() - 6.0).abs() < 1e-4,
            "the factor is scene width over rendered width, got {}",
            wide_render.display_font_size_px()
        );
    }

    /// The scaling may never hand egui a size it cannot lay a row out with, however degenerate the
    /// selection geometry is. The floor is a guard, not a readability policy — see
    /// `MIN_EDITOR_DISPLAY_FONT_SIZE_PX`.
    #[test]
    fn a_degenerate_editor_box_cannot_produce_a_zero_font_size() {
        let degenerate = editor_with_scale(0.001, 100_000, 8.0);
        let size = degenerate.display_font_size_px();
        assert!(
            size >= MIN_EDITOR_DISPLAY_FONT_SIZE_PX,
            "a vanishing box must still yield a layout-able size, got {size}"
        );
        assert!(size.is_finite(), "the drawn size must never be NaN or infinite");
    }
}
