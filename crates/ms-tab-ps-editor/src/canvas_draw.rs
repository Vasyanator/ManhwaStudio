/*
File: crates/ms-tab-ps-editor/src/canvas_draw.rs

Purpose:
The canvas: input occlusion gate, the full-area canvas draw, tool routing and commits, the layer
render cache and composite, and the tab's keyboard shortcuts.

Key functions:
- `pointer_in_any_panel()`, `canvas_pointer_occluded()`: the input gate against this frame's panel
  rects.
- `draw_canvas()`: camera, touch, tool routing, overlays; `draw_correction_pass()` (view-only GPU
  correction).
- `commit_brush_stroke()`, `commit_pixel_region()`, `apply_tool_actions()`, `apply_tool_outcome()`:
  tool results into layers + undo.
- `sync_render_cache()`, `upload_layers()`, `draw_composite()`: tiled GPU cache and composite.
- `handle_hotkeys()` (pub), `brush_shortcuts()`, `ps_editor_common_hotkey_rows()`: shortcuts and
  their displayed list, kept together.

Notes:
A finished brush stroke is committed OUTSIDE the routing gate (see `commit_brush_stroke`).
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

impl PsEditorTabState {
    /// Whether `pointer_pos` is inside one of THIS frame's drawn dock panels.
    ///
    /// The rects come from `PanelDockOutput::drawn_panels` and are main-window rects by
    /// construction: a panel the user detached into a sub-window is never reported, so it can never
    /// blank out a region of this window.
    pub(crate) fn pointer_in_any_panel(&self, pointer_pos: Pos2) -> bool {
        self.panel_rects
            .iter()
            .any(|panel_rect| panel_rect.contains(pointer_pos))
    }

    /// Whether canvas pointer input at `pointer_pos` is claimed by UI floating above the canvas.
    ///
    /// Three terms, mirroring the cleaning tab's gate (`tabs/cleaning/tab.rs`):
    /// * an open popup or menu anywhere — its own layer may not cover this point yet on the frame
    ///   it opens;
    /// * this frame's dock-panel rects — the tab's uniform statement of "a floating surface of mine
    ///   is here", independent of the panel widget staying an interactable `Area`;
    /// * z-order occlusion: anything above `Order::Background` at that point, which covers the
    ///   effects-editor window, tooltips and context menus — the z-order rule the migrated
    ///   canvases get from `ms_input::Condition::PointerOnView`, asked here at the HOVER position.
    pub(crate) fn canvas_pointer_occluded(&self, ctx: &egui::Context, pointer_pos: Pos2) -> bool {
        ctx.any_popup_open()
            || self.pointer_in_any_panel(pointer_pos)
            || ctx.layer_id_at(pointer_pos).is_some_and(|layer| {
                matches!(
                    layer.order,
                    egui::Order::Middle
                        | egui::Order::Foreground
                        | egui::Order::Tooltip
                        | egui::Order::Debug
                )
            })
    }

    /// Runs the view-only «Коррекция» pass over the composited page, if one is active.
    ///
    /// Paints an `egui-shader-layers` `brightness_contrast` layer that re-reads the already-drawn
    /// canvas and redraws it through `clamp((c - 0.5) * gain + 0.5 + offset, 0, 1)`; the reason
    /// egui alone cannot express it is in `correction/MODULE_README.md`.
    ///
    /// Clipped to `page_rect ∩ canvas_rect`, so the viewport ground outside the page keeps its own
    /// colour. Paints nothing when the correction is neutral or «Нет» is selected — so the default
    /// path costs one comparison and no GPU work at all — and the library itself paints nothing
    /// when its backend is missing or failed (the panel reports that state).
    ///
    /// VIEW-ONLY: it changes what is on screen this frame and nothing else. No layer buffer, no
    /// document node and no on-disk file is touched.
    pub(crate) fn draw_correction_pass(&mut self, ui: &egui::Ui, canvas_rect: Rect, page_rect: Rect) {
        let Some(params) = self.correction.active_params() else {
            return;
        };
        let target = page_rect.intersect(canvas_rect);
        if !target.is_positive() {
            return;
        }
        // A painter clipped to the target: the layer captures only what lies inside its clip.
        correction::paint_correction_layer(&ui.painter_at(target), target, params);
        // Also observed here, not only in the panel: a GPU failure surfacing while the panel is
        // collapsed must still reach `runtime_log` (once; the latch dedups with the panel's check).
        self.correction_availability.observe(ui.ctx());
    }

    /// Central pan/zoom canvas: input handling, layer composite, overlays.
    ///
    /// `rect` is the SAME rect the dock was given this frame, so the canvas fills the whole
    /// program-tab area and the floating panels sit over it rather than beside it.
    pub(crate) fn draw_canvas(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        rect: Rect,
        project: &ProjectData,
    ) {
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        ui.painter()
            .rect_filled(rect, CornerRadius::ZERO, Color32::from_gray(40));

        let Some(page_size) = self.stack.as_ref().map(|stack| stack.size()) else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                t!("ps_editor.canvas.no_page"),
                egui::FontId::proportional(16.0),
                ui.visuals().weak_text_color(),
            );
            return;
        };

        self.viewport.fit_page_if_needed(rect, page_size);
        if self.pending_actual_size {
            self.viewport.reset_to_actual_size(page_size);
            self.pending_actual_size = false;
        }

        // Keyboard gestures routed to tools (Esc / Backspace / Delete) must not be stolen from a
        // focused text field — the same discipline `handle_hotkeys` applies to its letter keys.
        // Resolved BEFORE the `ui.input` closure so the memory and input locks are never nested.
        let keyboard_free = !ctx.memory(|m| m.focused().is_some());

        // Gather pointer / button / wheel input for this frame.
        let input = ui.input(|i| CanvasInput {
            hover_pos: i.pointer.hover_pos(),
            primary_down: i.pointer.primary_down(),
            primary_pressed: i.pointer.primary_pressed(),
            primary_released: i.pointer.primary_released(),
            secondary_down: i.pointer.secondary_down(),
            middle_down: i.pointer.middle_down(),
            space_down: i.key_down(egui::Key::Space),
            pointer_delta: i.pointer.delta(),
            scroll_y: i.smooth_scroll_delta.y,
            modifiers: i.modifiers,
            // `key_pressed` only observes the event; it does not consume it, so no other handler
            // (widgets use `consume_key` while focused) loses its key.
            cancel_pressed: keyboard_free && i.key_pressed(egui::Key::Escape),
            remove_point_pressed: keyboard_free
                && (i.key_pressed(egui::Key::Backspace) || i.key_pressed(egui::Key::Delete)),
        });
        // The canvas now fills the WHOLE program-tab area and the dock panels float over it, so
        // "the pointer is inside the canvas rect" is no longer the same question as "the pointer is
        // on bare canvas". Everything derived from the pointer — the wheel, the zoom anchor and the
        // routing to the active tool — is gated on this instead of on `response.hovered()` alone;
        // without it a click on a panel would paint underneath it.
        // The active tool's own on-canvas surface is the fourth term of the gate, asked of the TOOL
        // because only it knows where its surface is this frame (`PsTool::captures_canvas_pointer`;
        // a region tool re-projects the answer from PAGE pixels, so it does not drift on a pan).
        // It is deliberately narrow: it withholds the wheel, the zoom anchor, the routing to
        // `interact` and the tool's own cursor preview, and NOTHING else — the undo/redo shortcuts
        // live in `handle_hotkeys`, which never consults this gate, so an on-canvas surface can
        // never disable them (the mistake the cleaning tab's whole-canvas `block_canvas_zoom` flag
        // makes, `tabs/cleaning/tools/MODULE_README.md`).
        let pointer_occluded = input.hover_pos.is_some_and(|pos| {
            self.canvas_pointer_occluded(ctx, pos)
                || self.tools[self.active_tool_idx].captures_canvas_pointer(pos)
        });
        let hovered = response.hovered() && !pointer_occluded;
        let pointer_in_viewport = hovered && input.hover_pos.is_some_and(|p| rect.contains(p));
        // Panning is deliberately NOT gated on `pointer_occluded`: the gate answers where the
        // pointer is RIGHT NOW, and a pan that started on bare canvas must survive the pointer
        // crossing a panel. What it costs is a middle-drag begun on a panel also panning — the same
        // behaviour this tab had before the panels floated, and harmless (a pan destroys nothing).
        let pan_active = input.middle_down || (input.space_down && input.primary_down);

        // Wheel: Shift+wheel adjusts the brush; plain wheel zooms toward the cursor.
        let mut wheel_for_zoom = 0.0;
        if hovered && input.scroll_y.abs() > f32::EPSILON {
            if input.modifiers.shift && self.active_tool_id() == PsToolId::Brush {
                if let Some(brush) = self.brush_tool_mut() {
                    brush.handle_wheel(input.scroll_y);
                }
            } else {
                wheel_for_zoom = input.scroll_y;
            }
        }
        let pan_delta = if pan_active {
            input.pointer_delta
        } else {
            Vec2::ZERO
        };
        let anchor = input.hover_pos.filter(|p| rect.contains(*p));
        self.viewport
            .handle_input(rect, anchor, wheel_for_zoom, pan_delta);
        // Finger input (`ms_input::touch`): a claimed finger never reaches the tools; it
        // pans / pinches here, BEFORE this frame's transform is taken, opens the selection menu
        // on a long-press and drives the active selection tool on a long-press drag.
        let touch = canvas_touch::publish_and_take(
            ctx,
            rect,
            self.tools[self.active_tool_idx].accepts_touch_selection(),
        );
        if let Some(navigation) = touch.navigation {
            self.viewport.apply_touch_navigation(rect, navigation);
        }

        if self.active_tool_id() == PsToolId::Brush {
            self.brush_shortcuts(ctx);
        }

        let view = self.viewport.transform(rect);

        // With the Transform tool, dragging a typing text layer translates it (and writes the new
        // position back to text_info.json on release). This takes priority over raster transforms
        // when the press lands on a text layer, since text sits on top.
        let text_drag_active = !pan_active
            && self.active_tool_id() == PsToolId::Transform
            && self.handle_text_layer_drag(&input, &view, pointer_in_viewport, project);

        // Hint once on a fresh brush press when the active raster still shows an effects chain: it
        // is not directly editable until baked (`active_editable_mut` will refuse the paint).
        if !pan_active
            && input.primary_pressed
            && pointer_in_viewport
            && self.active_tool_id() == PsToolId::Brush
            && self
                .stack
                .as_ref()
                .and_then(|s| s.layer(s.active_id()))
                .is_some_and(|l| l.kind == LayerKind::Raster && !l.can_edit_pixels())
        {
            ms_log::runtime_log::log_warn("[ps_editor] Сначала запеките слой");
        }

        // Route input to the active tool unless the user is panning or dragging a text layer.
        if !pan_active && !text_drag_active {
            // Snapshot the active raster's transform/deform at gesture START (before the tool mutates
            // the stack this frame), so the release commit records ONE undo step per gesture — not one
            // per drag frame. Cleared/consumed in the matching release blocks below.
            if input.primary_pressed && self.active_tool_id() == PsToolId::Transform {
                self.transform_gesture_before = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.kind == LayerKind::Raster)
                    .map(|l| (l.uid.to_string(), l.transform));
            }
            if input.primary_pressed && self.active_tool_id() == PsToolId::Deform {
                self.deform_gesture_before = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.kind == LayerKind::Raster)
                    .map(|l| (l.uid.to_string(), l.deform.clone()));
            }
            // Alt + left click/drag with the brush is the EYEDROPPER, and it lives here rather than
            // in the tool: sampling the visible composite needs the unified band Z order
            // (`self.bands`), which `PsToolContext` does not carry. `BrushTool` stands aside for
            // the whole button hold while Alt was down at the press, so this never paints.
            //
            // The in-flight test is the other half of that agreement, and it is not redundant: the
            // tool only latches `suppress_stroke` when Alt was down at the PRESS, so pressing Alt
            // in the middle of a live stroke would otherwise resample the brush colour every frame
            // while the stroke keeps re-compositing with it — one soft stroke coming out
            // multi-coloured. The eyedropper and a stroke must never both act on one gesture.
            if self.active_tool_id() == PsToolId::Brush
                && input.modifiers.alt
                && input.primary_down
                && pointer_in_viewport
                && !self.tools[self.active_tool_idx].gesture_in_flight()
            {
                let sampled = input
                    .hover_pos
                    .map(|p| view.screen_to_world(p))
                    .and_then(|w| Some((page_pixel_index(w.x)?, page_pixel_index(w.y)?)))
                    .and_then(|(wx, wy)| self.sample_visible_composite(wx, wy));
                if let Some(color) = sampled
                    && let Some(brush) = self.brush_tool_mut()
                {
                    brush.set_color(color);
                }
            }

            let outcome = if let Some(stack) = self.stack.as_mut() {
                let pointer_image = input.hover_pos.map(|p| view.screen_to_world(p));
                // Taken from the STACK, not from `active_page_idx`: the stack is the buffer the
                // tool is about to edit, so its own page index is the one a queued action or a
                // worker job must be stamped with.
                let tool_page_idx = stack.page_idx();
                let mut tool_ctx = PsToolContext {
                    page_idx: tool_page_idx,
                    page_size,
                    pointer_image,
                    pointer_in_viewport,
                    primary_pressed: input.primary_pressed && pointer_in_viewport,
                    primary_down: input.primary_down,
                    primary_released: input.primary_released,
                    // The right button is NOT masked with `pointer_in_viewport`: the brush's HUD
                    // gesture is a drag whose whole point is that the pointer leaves where it
                    // started, and it paints nothing, so it cannot damage anything under a panel.
                    secondary_down: input.secondary_down,
                    pointer_delta: input.pointer_delta,
                    view,
                    stack,
                    selection: &mut self.selection,
                    modifiers: input.modifiers,
                    cancel_pressed: input.cancel_pressed,
                    remove_point_pressed: input.remove_point_pressed,
                };
                let tool = &mut self.tools[self.active_tool_idx];
                // A finger selection drag replaces the mouse frame; on the frame it is cancelled
                // the pointer frame runs too, so a cancelling mouse/pen press starts its drag
                // (`canvas_touch::tool_frame_route`).
                let route = canvas_touch::tool_frame_route(touch.selection, tool.accepts_touch_selection());
                let mut outcome = ToolOutcome::default();
                if let Some(selection) = route.touch {
                    outcome = tool.touch_selection(&mut tool_ctx, TouchSelectionInput::from_touch(&selection, &view));
                }
                if route.interact {
                    outcome = outcome.merged(tool.interact(&mut tool_ctx));
                }
                outcome
            } else {
                ToolOutcome::default()
            };
            // Log tool routing only on genuine activity (press / release / a selection change),
            // never on every idle or mid-drag frame — per-stroke detail lives in the tools.
            if ms_log::trace::trace_enabled()
                && (input.primary_pressed || input.primary_released || outcome.selection_changed)
            {
                let pi = input.hover_pos.map(|p| view.screen_to_world(p));
                ms_log::trace_log!(
                    cat::INPUT,
                    "tool_interact tool={:?} pressed={} released={} ptr={:?} dirty={:?} sel_changed={}",
                    self.active_tool_id(),
                    input.primary_pressed,
                    input.primary_released,
                    pi.map(|p| (p.x.round() as i32, p.y.round() as i32)),
                    outcome.dirty.map(|d| (d.min_x, d.min_y, d.max_x, d.max_y)),
                    outcome.selection_changed
                );
            }
            self.apply_tool_outcome(outcome);

            // Accumulate the active brush stroke's per-segment dirty rects into a union so the release
            // commit can build a region-bounded undo diff. Reset on the press frame (which also paints
            // the first stamp, so reset must run before accumulating this frame's dirty rect).
            if self.active_tool_id() == PsToolId::Brush {
                if input.primary_pressed {
                    self.brush_stroke_dirty = None;
                }
                if let Some(d) = outcome.dirty {
                    self.brush_stroke_dirty = Some(match self.brush_stroke_dirty {
                        Some(u) => tools::DirtyRect {
                            min_x: u.min_x.min(d.min_x),
                            min_y: u.min_y.min(d.min_y),
                            max_x: u.max_x.max(d.max_x),
                            max_y: u.max_y.max(d.max_y),
                        },
                        None => d,
                    });
                }
            }

            // Transform tool release on a raster: commit the live (stack-mutated) transform to the
            // shared doc so the move/rotate/scale is the model truth (and cross-tab visible).
            if input.primary_released
                && self.active_tool_id() == PsToolId::Transform
                && let Some(page_idx) = self.active_page_idx
                && let Some((uid, after_lt)) = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.kind == LayerKind::Raster)
                    .map(|l| (l.uid.to_string(), l.transform))
            {
                ms_log::trace_log!(cat::SYNC, "commit transform page={} uid={}", page_idx, uid);
                self.route_to_doc(page_idx, project, |doc| {
                    doc.set_transform(page_idx, &uid, transform_to_rec(after_lt));
                });
                // Record ONE `FieldPatch::Transform` for the completed gesture, if it actually moved
                // the same layer we snapshotted at press.
                if let Some((before_uid, before_lt)) = self.transform_gesture_before.take()
                    && before_uid == uid
                    && before_lt != after_lt
                {
                    self.history.record(PsEditOp::FieldPatch {
                        page_idx,
                        layer_uid: uid,
                        field: LayerFieldPatch::Transform {
                            before: before_lt,
                            after: after_lt,
                        },
                    });
                }
            }

            // Deform tool release on a raster: commit the live mesh grid (stack-mutated) to the
            // shared doc as the model truth. The tool seeds an identity grid on entry, so even a
            // bare click persists the (no-op) mesh; subsequent grid-point drags persist the warp.
            if input.primary_released
                && self.active_tool_id() == PsToolId::Deform
                && let Some(page_idx) = self.active_page_idx
                && let Some((uid, deform)) = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.kind == LayerKind::Raster)
                    .map(|l| (l.uid.to_string(), l.deform.clone()))
            {
                ms_log::trace_log!(cat::SYNC, "commit deform page={} uid={}", page_idx, uid);
                self.route_to_doc(page_idx, project, |doc| {
                    doc.set_deform(page_idx, &uid, deform.clone());
                });
                // Record ONE `FieldPatch::Deform` for the completed gesture, if the mesh actually
                // changed on the same layer we snapshotted at press (entering deform mode seeds an
                // identity grid: None → Some(identity) is a real, undoable state change).
                if let Some((before_uid, before_deform)) = self.deform_gesture_before.take()
                    && before_uid == uid
                    && !deform_eq(&before_deform, &deform)
                {
                    self.history.record(PsEditOp::FieldPatch {
                        page_idx,
                        layer_uid: uid,
                        field: LayerFieldPatch::Deform {
                            before: before_deform,
                            after: deform,
                        },
                    });
                }
            }

        } else if input.cancel_pressed {
            // Esc means "abandon the outline" even while the canvas is panning: the tool never sees
            // this frame's input, so the tab performs the abandonment on its behalf. A brush stroke
            // that has painted is committed by `reset_active_tool` before it is abandoned — its
            // pixels are already on the layer and Esc must not leave them un-undoable.
            self.reset_active_tool(project);
        } else {
            // Input went to the pan / text drag instead of the tool. Freeze the active tool so a
            // multi-frame gesture survives: without this the release performed during the pan is
            // never delivered, and the first routed frame afterwards reads "button up, no release"
            // as an interrupted drag and discards the traced outline.
            if let Some(tool) = self.tools.get_mut(self.active_tool_idx) {
                tool.freeze();
            }
        }

        // The brush stroke commit sits OUTSIDE the routing branch above on purpose, and is driven
        // by the stroke's own end rather than by `input.primary_released`. A release delivered on a
        // frame that went to a pan (middle button, or Space+drag) never reaches that branch, and a
        // commit trapped inside it would then never run: the stroke would get no undo entry, a
        // `Клин` stroke would never reach `CleanOverlaysModel` and would vanish on the next page
        // switch, and a raster stroke's pixels would ride into the NEXT stroke's commit. The brush
        // ends the stroke on exactly those suppressed frames (`BrushTool::freeze`) and latches it
        // here.
        if self
            .brush_tool_mut()
            .is_some_and(BrushTool::take_stroke_finished)
        {
            self.commit_brush_stroke(project);
        }

        // Upload + composite layers bottom-to-top in unified band order (rasters + typing overlays).
        self.sync_render_cache();
        self.upload_layers(ctx);
        self.draw_composite(ctx, ui, &view);
        // The page rect in SCREEN space: the correction pass and the pixel grid both need it.
        let page_screen_rect = view.world_rect_to_screen(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(page_size[0] as f32, page_size[1] as f32),
        ));
        // «Коррекция» runs HERE: after the composite (so it corrects the page and the checkerboard
        // under it) and before every legibility overlay below (so the grid, the marquee, the menu
        // and the tool cursor stay exactly as drawn).
        self.draw_correction_pass(ui, rect, page_screen_rect);
        // The pixel grid is ONE late overlay pass over the whole page, never a per-layer one —
        // the same contract the canvas keeps (`canvas/scene.rs::draw_visible_pixel_grid_overlay`).
        // The manual checkbox states the user's intent; the magnification threshold stays as a cost
        // and legibility guard (see `pixel_grid_enabled`).
        if self.pixel_grid_enabled
            && ms_canvas::pixel_inspection_recommended_for(view.zoom, ctx.pixels_per_point())
        {
            // A PAGE grid: page px == source px for the two identity-transformed base layers, so
            // the lines are axis-aligned at `view.zoom` spacing. A rotated/scaled/deformed raster
            // layer has its own texel grid and will NOT line up with this one — an accepted
            // limitation (see `MODULE_README.md`), not something to compensate for here.
            ms_canvas::pixel_grid::draw_pixel_grid(
                &ui.painter_at(rect),
                page_screen_rect,
                view.zoom,
                rect,
                ctx.pixels_per_point(),
            );
        }
        self.draw_selection_marquee(ui, &view);

        // Right-click menu on the selection: copy/cut from chosen layers. Suppressed while Alt is
        // held AND the brush is active, because Alt + right-drag is the brush's size/hardness HUD
        // and a short one still registers as a secondary CLICK, which would pop the menu open
        // mid-gesture. The suppression is scoped to the brush because only the brush owns that
        // gesture: under any other tool Alt+right-click is an ordinary right-click and must still
        // open the menu.
        let hud_gesture_possible = input.modifiers.alt && self.active_tool_id() == PsToolId::Brush;
        self.draw_selection_menu(&response, project, hud_gesture_possible, touch.long_press_at);

        // Tool cursor / preview overlay. It obeys the SAME occlusion gate as the input path
        // (`pointer_occluded`, resolved once above): a brush circle that keeps tracking the cursor
        // under a floating panel advertises a stroke the tools already refuse to start there.
        // The exception is a gesture the tool has actually accepted — see `overlay_pointer` for why
        // that question goes to the tool and not to `input.primary_down`.
        let pointer_image = overlay_pointer(
            input.hover_pos,
            rect,
            pointer_occluded,
            self.tools[self.active_tool_idx].gesture_in_flight(),
        )
        .map(|p| view.screen_to_world(p));
        let painter = ui.painter_at(rect);
        self.tools[self.active_tool_idx].draw_overlay(&painter, &view, pointer_image);
        if let Some(armed_at) = touch.long_press_armed_at {
            canvas_touch::draw_long_press_marker(&painter, armed_at);
        }

        // Page border for orientation.
        let page_rect = view.world_rect_to_screen(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(page_size[0] as f32, page_size[1] as f32),
        ));
        painter.rect_stroke(
            page_rect,
            CornerRadius::ZERO,
            egui::Stroke::new(1.0, ms_theme::canvas::NEUTRAL_BORDER),
            egui::StrokeKind::Outside,
        );

        // Resolved in its own statement so the immutable stack borrow ends before `self.tools` is
        // borrowed mutably below. The stack is present here — `page_size` above returns otherwise.
        let overlay_page_idx = self.stack.as_ref().map_or(0, layers::LayerStack::page_idx);
        // The active tool's on-canvas pass, LAST in the canvas frame and for the ACTIVE tool only.
        // Unlike `draw_overlay` above it owns the `Context`, so it may open its own `Area` and
        // sense input there; being last costs nothing for those (an `Area` is its own layer and is
        // composited by layer order, not by call order — `egui-docs/06-overlays.md`) and keeps the
        // painter-based decorations above from being reordered.
        //
        // It runs only while a page is resident, because `PsToolOverlayCx` is page geometry — which
        // is exactly why a tool's WORKER results are consumed in `poll_tools` instead, and why the
        // pixel writes it asks for are queued rather than performed here.
        self.tools[self.active_tool_idx].draw_overlay_ui(
            ctx,
            tools::PsToolOverlayCx {
                viewport: rect,
                view,
                page_size,
                page_idx: overlay_page_idx,
            },
        );

        if response.hovered()
            || pan_active
            || self.pending_job_id.is_some()
            || self.raster_effects_state.is_some()
        {
            // Keep polling so a finished off-thread effects render is consumed promptly even with no
            // pointer activity.
            ctx.request_repaint();
        }
    }

    /// Commits a FINISHED brush stroke: records the reversible undo diff, then pushes the painted
    /// pixels onward and clears the stroke's dirty union.
    ///
    /// Called once per stroke, from the `BrushTool::take_stroke_finished` latch in `draw_canvas` —
    /// never from the pointer-release edge, which a pan frame can swallow (see the call site).
    /// During the stroke the local `image` is mutated live (responsive) while `base_image` still
    /// holds the pre-stroke pixels, so `record_pixel_region` reads the "before" from `base_image`
    /// for free — no stroke-start snapshot. A paintable raster has no effects, so
    /// base == display == painted pixels.
    ///
    /// The destination depends on the painted layer's KIND; see
    /// [`PsEditorTabState::commit_pixel_region`], which this shares with every other tool-driven
    /// pixel commit. A stroke that painted nothing (no union, or a layer that refuses pixel edits)
    /// is a no-op.
    pub(crate) fn commit_brush_stroke(&mut self, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // Read the stroke union BEFORE the commit, which does not consume it but is followed by the
        // reset below.
        let stroke_union = self.brush_stroke_dirty;
        self.commit_pixel_region(
            page_idx,
            stroke_union,
            t!("ps_editor.edit_op.brush_stroke"),
            project,
        );
        self.brush_stroke_dirty = None;
    }

    /// Commits an ALREADY-APPLIED pixel edit of the active layer: one reversible undo entry, then
    /// the push onward that makes it durable and cross-tab visible.
    ///
    /// The single commit path for every tool-driven pixel edit — the brush stroke and the
    /// [`tools::PsToolAction::WriteRegion`] a region tool queues both end here, so the rules below
    /// hold exactly once.
    ///
    /// `region` is the edit's bounding rect in the ACTIVE LAYER's own pixel grid, or `None` when
    /// the edit had no measured region. `label` is the already-localized undo caption.
    ///
    /// Order is load-bearing: the undo entry is recorded FIRST, because the push and the next
    /// reprojection sync `base_image` to the edited pixels and would erase the "before" the diff is
    /// built from.
    ///
    /// The destination depends on the edited layer's KIND, the same split
    /// [`pixel_region_edit_op`] makes for the undo variant: a user raster commits to the shared
    /// `LayerDoc` (the model truth, cross-tab visible); the `Клин` base layer is not a doc node at
    /// all, so it goes to the shared `CleanOverlaysModel` through the one
    /// `write_clean_region_to_model` helper — bounded to `region`, because a ribbon page can be
    /// ~800x19000 px. `Исходник` is never editable and is a no-op here.
    pub(crate) fn commit_pixel_region(
        &mut self,
        page_idx: usize,
        region: Option<tools::DirtyRect>,
        label: &str,
        project: &ProjectData,
    ) {
        // Record BEFORE pushing the pixels onward (the push + next reprojection sync base_image to
        // the edited pixels, which would erase the "before").
        if let Some(region) = region {
            self.record_pixel_region(page_idx, region, label);
        }
        let painted = self
            .stack
            .as_ref()
            .and_then(|s| s.layer(s.active_id()))
            .filter(|l| l.pixels_dirty && l.can_edit_pixels())
            .map(|l| (l.kind, l.uid.to_string(), l.image.clone()));
        match painted {
            Some((LayerKind::Raster, uid, painted)) => {
                let base = painted.clone();
                ms_log::trace_log!(
                    cat::SYNC,
                    "commit pixels page={} uid={} label={}",
                    page_idx,
                    uid,
                    label
                );
                self.route_to_doc(page_idx, project, |doc| {
                    doc.set_raster_pixels(page_idx, &uid, base, painted, Vec::new(), true);
                });
            }
            Some((LayerKind::Clean, _, _)) => {
                if let Some(union) = region {
                    self.write_clean_region_to_model(
                        page_idx,
                        union.min_x,
                        union.min_y,
                        union.max_x.saturating_sub(union.min_x) + 1,
                        union.max_y.saturating_sub(union.min_y) + 1,
                    );
                }
            }
            // `Source` is never paintable (`can_edit_pixels`), and `None` means the edit changed
            // nothing.
            Some((LayerKind::Source, _, _)) | None => {}
        }
    }

    /// Drains every tool's deferred action queue and performs the requests.
    ///
    /// The tab-side half of [`tools::PsTool::take_actions`]: a tool may not record history, may not
    /// write the shared `LayerDoc` and may not reach `CleanOverlaysModel`, so it describes the
    /// commit and this performs it. Drained from EVERY tool, not only the active one — a worker
    /// result that landed after a tool switch still represents work the user did.
    ///
    /// Called once per frame from [`PsEditorTabState::draw`], after `draw_canvas`; that is the only
    /// site reached on every frame, since `draw_canvas` returns early while no page is resident.
    ///
    /// A request for a page that is NOT resident is dropped and logged: a worker result that
    /// outlived a page switch would otherwise land on the new page's pixels.
    ///
    /// Returns `true` when at least one request changed pixels, so the caller can request the
    /// repaint that shows them.
    pub(crate) fn apply_tool_actions(&mut self, project: &ProjectData) -> bool {
        let mut actions: Vec<tools::PsToolAction> = Vec::new();
        for tool in &mut self.tools {
            actions.extend(tool.take_actions());
        }
        let mut changed = false;
        for action in actions {
            match action {
                tools::PsToolAction::WriteRegion(write) => {
                    changed |= self.apply_tool_region_write(write, project);
                }
            }
        }
        changed
    }

    /// Performs one [`tools::ToolRegionWrite`]: blend into the active editable layer, invalidate the
    /// affected tiles, then commit it as ONE undo step through
    /// [`PsEditorTabState::commit_pixel_region`].
    ///
    /// Refuses — logging why, never silently degrading — when the page is not resident, when the
    /// active layer accepts no pixel edit (`Исходник`, or a raster still showing an effects chain),
    /// when the rect falls entirely outside the layer, or when the coverage buffer's length does
    /// not match the source rect.
    ///
    /// Returns `true` when pixels actually changed.
    pub(crate) fn apply_tool_region_write(
        &mut self,
        write: tools::ToolRegionWrite,
        project: &ProjectData,
    ) -> bool {
        let tools::ToolRegionWrite {
            page_idx,
            origin,
            pixels,
            coverage,
            label,
        } = write;
        if self.active_page_idx != Some(page_idx)
            || self
                .stack
                .as_ref()
                .is_none_or(|stack| stack.page_idx() != page_idx)
        {
            ms_log::runtime_log::log_warn(format!(
                "[ps_editor] a tool asked to write {}x{} px into page {page_idx}, which is no \
                 longer the resident page; the request was dropped",
                pixels.size[0], pixels.size[1]
            ));
            return false;
        }
        // `active_editable_mut` is the same gate the brush paints through: it refuses `Исходник`
        // and a raster whose effects chain is not baked, so a tool cannot write where the brush
        // cannot.
        let Some(layer) = self
            .stack
            .as_mut()
            .and_then(layers::LayerStack::active_editable_mut)
        else {
            ms_log::runtime_log::log_warn(format!(
                "[ps_editor] a tool asked to write into page {page_idx}, but the active layer \
                 accepts no pixel edit; the request was dropped"
            ));
            return false;
        };
        let layer_id = layer.id;
        let Some(dirty) =
            blend_premul_region(&mut layer.image, origin, &pixels, coverage.as_deref())
        else {
            ms_log::runtime_log::log_warn(format!(
                "[ps_editor] a tool's {}x{} px write at ({},{}) on page {page_idx} was refused: it \
                 falls outside the active layer, or its coverage buffer is not {} bytes long",
                pixels.size[0],
                pixels.size[1],
                origin[0],
                origin[1],
                pixels.size[0].saturating_mul(pixels.size[1])
            ));
            return false;
        };
        layer.pixels_dirty = true;
        if let Some(cache) = self.render_cache.get_mut(&layer_id) {
            cache.mark_dirty_rect(dirty);
        }
        self.commit_pixel_region(page_idx, Some(dirty), &label, project);
        true
    }

    /// Commits an in-flight brush stroke that has already painted, before its gesture is
    /// ABANDONED (Esc under input suppression, a tool switch, a page switch).
    ///
    /// `PsTool::reset` means "abandon", and for every other tool that is exactly right: the lasso's
    /// Esc drops its pending polygon and must commit nothing. The brush is different — its pixels
    /// are already in `layer.image` and `reset` cannot take them back — so abandoning without this
    /// leaves pixels the user can see with no undo entry, and a `Клин` stroke with no write-back to
    /// `CleanOverlaysModel`. Every pixel the user can see must stay undoable.
    ///
    /// Two guards keep it strictly scoped. The stroke is ended through
    /// `BrushTool::end_stroke_for_commit`, so an idle brush (or any other active tool) latches
    /// nothing and nothing is committed; and the commit itself runs only when the stroke actually
    /// painted (`brush_stroke_dirty`), so a stroke that never landed a stamp cannot push a previous
    /// stroke's pixels a second time. The latch is the SAME one the ordinary ending uses and is
    /// consumed here, so a stroke can never commit twice.
    pub(crate) fn commit_brush_stroke_before_abandon(&mut self, project: &ProjectData) {
        if let Some(brush) = self.brush_tool_mut() {
            brush.end_stroke_for_commit();
        }
        if self
            .brush_tool_mut()
            .is_some_and(BrushTool::take_stroke_finished)
            && self.brush_stroke_dirty.is_some()
        {
            self.commit_brush_stroke(project);
        }
    }

    /// Applies a tool outcome: tile invalidation + selection-overlay refresh.
    pub(crate) fn apply_tool_outcome(&mut self, outcome: ToolOutcome) {
        if let Some(dirty) = outcome.dirty {
            let Some(active) = self.stack.as_ref().map(|s| s.active_id()) else {
                return;
            };
            if let Some(cache) = self.render_cache.get_mut(&active) {
                cache.mark_dirty_rect(dirty);
            }
            // A pixel-dirty outcome (e.g. a brush stroke) edited the active layer's base pixels, so
            // a save must rewrite its base PNG and bake in any non-destructive effects.
            if let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(active)) {
                layer.pixels_dirty = true;
            }
        }
        // The marquee is drawn from `Selection::outline_loops`, set by the tools, so a changed
        // selection needs no extra rebuild here.
    }

    /// Sampling mode every PS-editor texture is uploaded with, from the «Сглаживание» checkbox.
    ///
    /// One notion for the whole tab: layer tiles and typing text overlays must not disagree, or a
    /// magnified page would show smoothed text over un-smoothed art.
    #[must_use]
    pub(crate) fn layer_texture_options(&self) -> egui::TextureOptions {
        if self.smoothing_enabled {
            egui::TextureOptions::LINEAR
        } else {
            egui::TextureOptions::NEAREST
        }
    }

    /// Ensures a render cache entry exists for every current layer and drops stale ones.
    pub(crate) fn sync_render_cache(&mut self) {
        let Some(stack) = &self.stack else {
            return;
        };
        let ids: Vec<LayerId> = stack.layers().iter().map(|l| l.id).collect();
        self.render_cache.retain(|id, _| ids.contains(id));
        for layer in stack.layers() {
            let size = layer.image.size;
            // Recreate the cache when a layer's image was resized (e.g. a freshly cropped clip).
            let needs_new = self
                .render_cache
                .get(&layer.id)
                .is_none_or(|cache| !cache.matches_size(size));
            if needs_new {
                self.render_cache.insert(
                    layer.id,
                    TiledTexture::new(size, format!("ps_layer_{}", layer.id)),
                );
            }
        }
    }

    /// Uploads dirty layer tiles within the per-frame budget.
    ///
    /// Also reconciles every cache with the current «Сглаживание» mode BEFORE the budgeted sweep,
    /// and does so over the whole cache rather than inside the budgeted loop: the loop stops once
    /// the budget is spent, so a mode change announced there would silently skip the layers that
    /// did not fit this frame. `TiledTexture::set_options` is a no-op unless the mode really
    /// changed, so this costs one comparison per layer per frame.
    ///
    /// A fully spent budget means tiles are still waiting, so this REQUESTS the next frame. Without
    /// that, a «Сглаживание» flip — which re-dirties every tile of every layer at once — would
    /// convert only `TILE_UPLOAD_BUDGET_PER_FRAME` tiles and stop: the checkbox lives in a dock
    /// panel away from the canvas, `draw_canvas` repaints only on canvas hover / pan / a pending
    /// job, and egui's `Checkbox` requests no repaint of its own, so an idle app would keep a page
    /// split between the two sampling modes until some unrelated repaint arrived. A layer whose
    /// image no longer matches its cache uploads nothing and therefore spends no budget, so this
    /// cannot become a self-feeding repaint loop.
    pub(crate) fn upload_layers(&mut self, ctx: &egui::Context) {
        let options = self.layer_texture_options();
        for cache in self.render_cache.values_mut() {
            cache.set_options(options);
        }
        let Some(stack) = &self.stack else {
            return;
        };
        let mut budget = TILE_UPLOAD_BUDGET_PER_FRAME;
        for layer in stack.layers() {
            if budget == 0 {
                break;
            }
            if let Some(cache) = self.render_cache.get_mut(&layer.id) {
                let uploaded = cache.upload_budgeted(ctx, &layer.image, budget);
                budget = budget.saturating_sub(uploaded);
            }
        }
        if budget == 0 {
            ctx.request_repaint();
        }
    }

    /// Composites everything bottom-to-top in unified band order: the locked base layers first,
    /// then raster layers and typing overlays interleaved by their band Z (`self.bands`). Unsaved
    /// rasters / overlays without a band sit on top; equal-rank items keep stack / `text_layers`
    /// order. The order, visibility and group-folded opacities come from the pure
    /// [`composite_steps`] (the shared `ordering::composite_plan` owner); this method only draws.
    pub(crate) fn draw_composite(
        &mut self,
        ctx: &egui::Context,
        ui: &egui::Ui,
        view: &viewport::ViewTransform,
    ) {
        // Resolved before any field of `self` is borrowed for the draw plan: the text-overlay loop
        // below holds `&mut self.text_layers`, which would forbid reading the flag there.
        let text_options = self.layer_texture_options();

        let painter = ui.painter_at(view.viewport_rect);

        // Base layers (source/clean) are always the bottom and are not bands, so they are drawn
        // here directly and never enter the plan.
        let plan: Vec<(u32, PsStep)> = {
            let Some(stack) = self.stack.as_ref() else {
                return;
            };
            // Transparency checkerboard under the page: a transparent hole must read as a hole,
            // not as the viewport void. Drawn before every layer so it stays the bottom-most mark.
            layer_render::draw_page_checkerboard(&painter, view, stack.size());
            for layer in stack.layers() {
                if !layer.kind.is_base() || !stack.layer_visible(layer) {
                    continue;
                }
                let opacity = stack.layer_opacity(layer);
                if opacity <= 0.0 {
                    continue;
                }
                if let Some(cache) = self.render_cache.get(&layer.id) {
                    cache.draw(&painter, view, opacity, layer);
                }
            }
            // Unified-group visibility/opacity, folded by the owner for rasters and texts alike.
            let group_uids = stack_group_uids(stack);
            let groups = stack_group_folds(stack, &group_uids);
            let rasters = PsRasterView::collect(stack);
            let texts: Vec<PsTextView<'_>> = self
                .text_layers
                .iter()
                .enumerate()
                .map(|(index, layer)| PsTextView::of(index, layer))
                .collect();
            composite_steps(&rasters, &texts, &self.bands, &groups)
        };

        // The composite is rebuilt every frame (no cache), so only emit when the plan size changes
        // to avoid a 60/s flood. `usize::MAX` sentinel forces the first frame to log.
        if ms_log::trace::trace_enabled() && plan.len() != self.trace_last_composite_steps {
            ms_log::trace_log!(
                cat::RENDER,
                "draw_composite page={:?} steps={}",
                self.active_page_idx,
                plan.len()
            );
            self.trace_last_composite_steps = plan.len();
        }

        for (_, step) in plan {
            match step {
                PsStep::Raster { id, opacity } => {
                    if let (Some(stack), Some(cache)) =
                        (self.stack.as_ref(), self.render_cache.get(&id))
                        && let Some(layer) = stack.layer(id)
                    {
                        if layer.deform.is_some() {
                            cache.draw_deform(&painter, view, opacity, layer);
                        } else {
                            cache.draw(&painter, view, opacity, layer);
                        }
                    }
                }
                PsStep::Text { index, opacity } => {
                    if let Some(layer) = self.text_layers.get_mut(index) {
                        layer.draw(ctx, &painter, view, opacity, text_options);
                    }
                }
            }
        }
    }

    /// Mutable access to the brush tool instance, if present.
    pub(crate) fn brush_tool_mut(&mut self) -> Option<&mut BrushTool> {
        self.tools.iter_mut().find_map(|tool| tool.as_brush_mut())
    }

    /// Reads every brush key the tab owns this frame and forwards it to the brush tool.
    ///
    /// Called once per frame from `draw_canvas` while the brush is active. It is suppressed while
    /// a widget holds keyboard focus, exactly like [`PsEditorTabState::handle_hotkeys`]: without
    /// that guard, typing into the brush panel's own `WheelSlider` still resized the brush.
    ///
    /// The tool itself never sees an `egui::Context`, so the tab owns the dispatch; the tool's
    /// `hotkey_rows` publishes the same list to the shortcut panel and the two must be edited
    /// together.
    pub(crate) fn brush_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let actions = ctx.input(|i| collect_brush_key_actions(&i.events));
        if actions.is_empty() {
            return;
        }
        let Some(brush) = self.brush_tool_mut() else {
            return;
        };
        for action in actions {
            match action {
                BrushKeyAction::DiameterStep { up } => {
                    brush.step_diameter(up);
                }
                BrushKeyAction::DiameterScale { down } => {
                    brush.scale_diameter(down);
                }
                BrushKeyAction::HardnessStep { up } => brush.step_hardness(up),
                BrushKeyAction::Opacity(fraction) => brush.set_opacity(fraction),
                BrushKeyAction::Flow(fraction) => brush.set_flow(fraction),
            }
        }
    }

    /// Nearest-neighbour sample of the VISIBLE raster composite at page pixel `(wx, wy)`.
    ///
    /// Layers are composited bottom-to-top in the same order [`PsEditorTabState::draw_composite`]
    /// uses — the two base layers first (they are not bands), then the user rasters in the order,
    /// visibility and group-folded opacity of [`composite_steps`] (the shared ordering owner).
    ///
    /// TEXT overlays are deliberately NOT sampled: they are drawn by the typing renderer and have
    /// no `Layer` buffer to read. That is an accepted limitation of the brush's Alt+click
    /// eyedropper, recorded in `tools/MODULE_README.md`.
    ///
    /// Returns the STRAIGHT (un-premultiplied) colour, or `None` when the composite is fully
    /// transparent there — sampling a hole would otherwise set the brush colour to black.
    pub(crate) fn sample_visible_composite(&self, wx: usize, wy: usize) -> Option<Color32> {
        let stack = self.stack.as_ref()?;
        let [pw, ph] = stack.size();
        if wx >= pw || wy >= ph {
            return None;
        }
        // The user rasters' order, visibility and group-folded opacity come from the SAME plan the
        // canvas draws (`composite_steps` over the shared ordering owner), so the eyedropper samples
        // exactly what `draw_composite` shows. Text overlays are not sampled (see above).
        let group_uids = stack_group_uids(stack);
        let groups = stack_group_folds(stack, &group_uids);
        let raster_views = PsRasterView::collect(stack);
        let raster_plan = composite_steps(&raster_views, &[], &self.bands, &groups);

        // `over(src, dst)` is src-over, so walking bottom-to-top and putting each layer over the
        // accumulator reproduces the composite the user sees.
        let mut px = Color32::TRANSPARENT;
        for layer in stack.layers().iter().filter(|l| l.kind.is_base()) {
            if !stack.layer_visible(layer) {
                continue;
            }
            let opacity = stack.layer_opacity(layer);
            if opacity <= 0.0 {
                continue;
            }
            px = over(scale_premultiplied(sample_layer_world(layer, wx, wy), opacity), px);
        }
        for (_, step) in raster_plan {
            match step {
                PsStep::Raster { id, opacity } => {
                    let Some(layer) = stack.layer(id) else {
                        continue;
                    };
                    px = over(scale_premultiplied(sample_layer_world(layer, wx, wy), opacity), px);
                }
                // No text views were passed in, so the plan holds rasters only.
                PsStep::Text { .. } => {}
            }
        }
        if px.a() == 0 {
            return None;
        }
        let [r, g, b, _] = px.to_srgba_unmultiplied();
        Some(Color32::from_rgb(r, g, b))
    }

    /// Handles tab-local tool/selection hotkeys. Called from the root hotkey dispatch.
    ///
    /// Letter shortcuts are suppressed while a widget holds keyboard focus so they do not fire
    /// while the user is interacting with a focused control.
    pub fn handle_hotkeys(&mut self, ctx: &egui::Context, project: &ProjectData) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let (b, m, l, v, deselect, undo, redo) = ctx.input(|i| {
            let cmd = i.modifiers.command;
            (
                i.key_pressed(egui::Key::B),
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::L),
                i.key_pressed(egui::Key::V),
                cmd && i.key_pressed(egui::Key::D),
                // Ctrl/Cmd+Z (without Shift) = undo.
                cmd && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                // Ctrl/Cmd+Shift+Z or Ctrl/Cmd+Y = redo.
                cmd && ((i.modifiers.shift && i.key_pressed(egui::Key::Z))
                    || i.key_pressed(egui::Key::Y)),
            )
        });
        if b && let Some(idx) = self.tool_index(PsToolId::Brush) {
            self.set_active_tool(idx, project);
        }
        if m && let Some(idx) = self.tool_index(PsToolId::SelectRect) {
            self.set_active_tool(idx, project);
        }
        if l && let Some(idx) = self.tool_index(PsToolId::SelectLasso) {
            self.set_active_tool(idx, project);
        }
        if v && let Some(idx) = self.tool_index(PsToolId::Transform) {
            self.set_active_tool(idx, project);
        }
        if deselect {
            self.clear_selection();
        }
        // Undo takes priority; the two are mutually exclusive for a given key event anyway.
        if undo {
            self.undo(project);
        } else if redo {
            self.redo(project);
        }
    }
}

/// The TAB-level shortcuts, already localized, for the «Горячие клавиши» panel.
///
/// Deliberately placed immediately next to [`PsEditorTabState::handle_hotkeys`] — which dispatches
/// the letter keys, Ctrl+D and undo/redo — and next to `draw_canvas`, which reads the pan buttons
/// and the wheel: the displayed list and the dispatched keys have no other coupling, so keeping
/// them apart is exactly how the two drift.
///
/// A TOOL's own shortcuts are not here: they come from `PsTool::hotkey_rows`, so a tool stays the
/// single owner of the keys it interprets. `PsToolId::Deform` has no tool letter today, which is
/// why the list names four tools and not five.
#[must_use]
pub(crate) fn ps_editor_common_hotkey_rows() -> Vec<tools::PsHotkeyRow> {
    use tools::PsHotkeyRow;
    vec![
        // Tool letters — `handle_hotkeys`, suppressed while a widget holds keyboard focus.
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.tool_brush_label"),
            t!("ps_editor.hotkeys.tool_brush_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.tool_rect_select_label"),
            t!("ps_editor.hotkeys.tool_rect_select_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.tool_lasso_label"),
            t!("ps_editor.hotkeys.tool_lasso_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.tool_transform_label"),
            t!("ps_editor.hotkeys.tool_transform_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.clear_selection_label"),
            t!("ps_editor.hotkeys.clear_selection_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.undo_label"),
            t!("ps_editor.hotkeys.undo_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.redo_label"),
            t!("ps_editor.hotkeys.redo_keys"),
        ),
        // Canvas gestures — `draw_canvas`: middle button or Space+LMB pans, the wheel zooms.
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.pan_label"),
            t!("ps_editor.hotkeys.pan_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.zoom_label"),
            t!("ps_editor.hotkeys.zoom_keys"),
        ),
    ]
}
