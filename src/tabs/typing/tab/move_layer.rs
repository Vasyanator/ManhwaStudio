/*
File: tab/move_layer.rs

Purpose:
The ONE layer-move primitive of the typing tab. A "move session"
(`TypingLayerMoveSession`, defined in `tab.rs`) represents a single translate-a-layer
gesture, whatever drives it (pointer drag or arrow-key nudge) and whatever it moves
(text/image overlay or raster layer). Every side effect of a move — page clamp,
whole-pixel snap, on-screen visibility limit, mask-clip invalidation, centering-frame
rebinding, dirty marking and the DEFERRED persist — happens here and nowhere else.

Main responsibilities:
- open / drive / end a move session (`begin_layer_move`, `drive_pointer_layer_move`,
  `add_keyboard_layer_move_step`, `settle_layer_move`);
- own the ONE arrow-key entry point for both layer kinds
  (`try_move_selected_layer_by_arrow_shortcuts`), guards included;
- run the once-per-frame settle driver (`drive_layer_move_settle`), called from
  `TypingTabState::draw` AFTER `canvas.draw`;
- map the session's target onto the two geometry stores (overlay runtime vs cached
  raster projection) through `layer_move_base` / `write_layer_move_geometry`.

Key functions:
- apply_layer_move()  — the single apply path: snap-once, write base+delta, invalidate, limit
- layer_move_settles() — the pure settle decision (unit-tested)
- snap_move_base_on_first_displacement() — the pure once-only snap rule (unit-tested)

Notes:
The delta is ALWAYS applied to the session BASE, never incrementally to the live geometry
(a per-point mesh clamp applied incrementally squashes a mesh permanently at a page edge).
The pure math lives in `mesh_geometry.rs`; this file only decides WHEN and to WHAT it applies.
`use super::*;` pulls in the parent module's types and imports; methods are `pub(super)` so
`tab.rs` and the sibling `tab/` submodules can drive them.
*/

use super::*;

impl TypingTextOverlayLayer {
    /// Opens a move session for `target` on `page_idx`, snapshotting the layer's geometry as the
    /// session base. `pointer_start_scene` is the press position for a `Pointer` session and `None`
    /// for a `Keyboard` one.
    ///
    /// A `Keyboard` session already open on the SAME target/page is left untouched (so OS key repeat
    /// keeps accumulating into one gesture); any other open session is settled first, so its write is
    /// never dropped. Returns `false` when the target does not resolve to a layer on that page, in
    /// which case no session is opened — check [`Self::can_begin_layer_move`] FIRST when the caller
    /// has already consumed input it would have to give back.
    #[must_use]
    pub(super) fn begin_layer_move(
        &mut self,
        target: TypingLayerMoveTarget,
        page_idx: usize,
        source: TypingLayerMoveSource,
        pointer_start_scene: Option<Pos2>,
        view: PageView,
    ) -> bool {
        if let Some(open) = self.move_session.as_ref() {
            if open.source == TypingLayerMoveSource::Keyboard
                && source == TypingLayerMoveSource::Keyboard
                && open.target == target
                && open.page_idx == page_idx
            {
                return true;
            }
            self.settle_layer_move();
        }
        let Some(base) = self.layer_move_base(target, page_idx, view) else {
            return false;
        };
        self.move_session = Some(TypingLayerMoveSession {
            target,
            page_idx,
            source,
            base,
            page_size_px: view.page_size_px(),
            pointer_start_scene,
            delta_page_px: [0.0, 0.0],
            snap_applied: false,
            strict_pixel_movement: false,
            has_changes: false,
        });
        true
    }

    /// Whether `begin_layer_move` would succeed for `target` on `page_idx` — i.e. the target resolves
    /// to a layer whose geometry can be snapshotted as a move base.
    ///
    /// Exists for the keyboard entry, which must decide BEFORE consuming the arrow keys: a gesture
    /// that cannot start after the keys are gone leaves the user with a swallowed key press and an
    /// unmoved layer.
    #[must_use]
    pub(super) fn can_begin_layer_move(
        &self,
        target: TypingLayerMoveTarget,
        page_idx: usize,
        view: PageView,
    ) -> bool {
        // An already-open keyboard session for the same target needs no new base.
        if self.move_session.as_ref().is_some_and(|session| {
            session.source == TypingLayerMoveSource::Keyboard
                && session.target == target
                && session.page_idx == page_idx
        }) {
            return true;
        }
        self.layer_move_base(target, page_idx, view).is_some()
    }

    /// Whether an open move session drives `target` on `page_idx`. Call sites use it to hand a drag
    /// frame to the session instead of their own drag state.
    #[must_use]
    pub(super) fn layer_move_session_targets(
        &self,
        target: TypingLayerMoveTarget,
        page_idx: usize,
    ) -> bool {
        self.move_session
            .as_ref()
            .is_some_and(|session| session.target == target && session.page_idx == page_idx)
    }

    /// Pointer frame step: recomputes the TOTAL delta from the press position and applies it.
    ///
    /// Recomputing (rather than integrating the per-frame pointer delta) is what makes the gesture
    /// idempotent: a pointer held still re-applies the same delta and changes nothing, and a drag into
    /// the page bound and back returns the layer to its exact original geometry. No-op unless a
    /// session with a press position drives `target` on `page_idx`.
    pub(super) fn drive_pointer_layer_move(
        &mut self,
        target: TypingLayerMoveTarget,
        page_idx: usize,
        pointer: Pos2,
        view: PageView,
        strict_pixel_movement: bool,
    ) {
        let Some(session) = self.move_session.as_mut() else {
            return;
        };
        if session.target != target || session.page_idx != page_idx {
            return;
        }
        let Some(start) = session.pointer_start_scene else {
            return;
        };
        let zoom = view.zoom.max(f32::EPSILON);
        let raw_delta_page_px = [
            (pointer.x - start.x) / zoom,
            (pointer.y - start.y) / zoom,
        ];
        session.delta_page_px = quantize_drag_page_delta(raw_delta_page_px, strict_pixel_movement);
        self.apply_layer_move(view, strict_pixel_movement);
    }

    /// Keyboard step: ADDS `step_page_px` to the session's total delta and applies it. No-op without
    /// an open session.
    pub(super) fn add_keyboard_layer_move_step(
        &mut self,
        step_page_px: [f32; 2],
        view: PageView,
        strict_pixel_movement: bool,
    ) {
        let Some(session) = self.move_session.as_mut() else {
            return;
        };
        session.delta_page_px[0] += step_page_px[0];
        session.delta_page_px[1] += step_page_px[1];
        self.apply_layer_move(view, strict_pixel_movement);
    }

    /// The ONE place a move is applied. Snaps the base on the FIRST real displacement, writes
    /// `base + delta` into the target, invalidates the layer's mask clip when the geometry actually
    /// changed, and (overlays only) re-runs the on-screen visibility limit.
    ///
    /// The visibility limit mutates the LIVE geometry, not the base — which is safe precisely because
    /// the next frame re-applies `base + delta` and re-runs the limit; the pair is idempotent.
    /// Requesting a repaint is left to the callers.
    fn apply_layer_move(&mut self, view: PageView, strict_pixel_movement: bool) {
        let Some(mut session) = self.move_session.take() else {
            return;
        };
        let page_size = session.page_size_px;
        // Remember the policy: a re-apply after a doc reprojection has no per-frame policy snapshot.
        session.strict_pixel_movement = strict_pixel_movement;
        if snap_move_base_on_first_displacement(
            &mut session.base,
            session.snap_applied,
            session.delta_page_px,
            strict_pixel_movement,
            page_size,
        ) {
            session.snap_applied = true;
        }
        if self.write_layer_move_geometry(
            session.target,
            session.page_idx,
            &session.base,
            session.delta_page_px,
            page_size,
            strict_pixel_movement,
        ) {
            session.has_changes = true;
            self.invalidate_layer_move_clip(session.target, session.page_idx);
        }
        // Overlays additionally obey the "keep part of the layer on screen" limit. Its result is
        // load-bearing: when it pulled the layer back, the geometry changed and must be persisted.
        if let TypingLayerMoveTarget::Overlay(overlay_idx) = session.target
            && self.enforce_overlay_visibility_limit(overlay_idx, view, strict_pixel_movement)
        {
            session.has_changes = true;
            self.mark_overlay_geometry_changed(overlay_idx, true);
        }
        self.move_session = Some(session);
    }

    /// Reads the session base out of the target's geometry store: the deform grid when the layer has
    /// one (both kinds RENDER from the mesh when present), else the affine center. `None` when the
    /// target does not resolve to a layer on `page_idx`.
    ///
    /// NORMALIZATION, deliberate: a RASTER's stored `DeformRec` is decoded through
    /// `TypingOverlayDeformMesh::from_deform_rec`, which clamps every control point into the page's
    /// overlay bounds (±0.9 of a side) — the invariant every mesh in this tab already satisfies,
    /// since `TypingOverlayDeformMesh::new` is the only constructor. Because a move now WRITES the
    /// mesh back (that is what makes a deformed raster move visibly at all), a stored mesh whose
    /// points sat outside those bounds is normalized by the first move and persisted that way. Only
    /// a mesh authored outside this tab can be in that state; keeping the layer's two meshes on one
    /// invariant is preferred over carrying an unclamped variant through the whole move path.
    fn layer_move_base(
        &self,
        target: TypingLayerMoveTarget,
        page_idx: usize,
        view: PageView,
    ) -> Option<TypingLayerMoveBase> {
        match target {
            TypingLayerMoveTarget::Overlay(overlay_idx) => {
                let overlay = self.overlays.get(overlay_idx)?;
                if overlay.page_idx != page_idx {
                    return None;
                }
                Some(match overlay.deform_mesh.as_ref() {
                    Some(mesh) => TypingLayerMoveBase::Mesh {
                        mesh: mesh.clone(),
                        center: overlay.center_page_px,
                    },
                    None => TypingLayerMoveBase::Center(overlay.center_page_px),
                })
            }
            TypingLayerMoveTarget::Raster(raster_idx) => {
                let layer = self.raster_layers_by_page.get(&page_idx)?.get(raster_idx)?;
                let center = [layer.transform.cx, layer.transform.cy];
                Some(match layer.deform.as_ref() {
                    Some(rec) => TypingLayerMoveBase::Mesh {
                        mesh: TypingOverlayDeformMesh::from_deform_rec(rec, view.page_size_px())?,
                        center,
                    },
                    None => TypingLayerMoveBase::Center(center),
                })
            }
        }
    }

    /// Writes `base + delta` into the target's geometry store. Returns whether anything actually
    /// changed (an unchanged write must not mark the project edited).
    fn write_layer_move_geometry(
        &mut self,
        target: TypingLayerMoveTarget,
        page_idx: usize,
        base: &TypingLayerMoveBase,
        delta_page_px: [f32; 2],
        page_size: [usize; 2],
        strict_pixel_movement: bool,
    ) -> bool {
        match target {
            TypingLayerMoveTarget::Overlay(overlay_idx) => {
                let Some(overlay) = self.overlays.get_mut(overlay_idx) else {
                    return false;
                };
                let previous_center = overlay.center_page_px;
                let previous_mesh = overlay.deform_mesh.clone();
                match base {
                    TypingLayerMoveBase::Mesh { mesh, .. } => {
                        let (moved, _applied) = moved_mesh_from_base(mesh, delta_page_px, page_size, strict_pixel_movement);
                        overlay.deform_mesh = Some(moved);
                        // The runtime center mirrors the mesh centroid; the paired base center is a
                        // raster concern and is not consulted here.
                        sync_overlay_center_from_deform_mesh(overlay, page_size);
                    }
                    TypingLayerMoveBase::Center(center) => {
                        overlay.center_page_px =
                            moved_center_from_base(*center, delta_page_px, page_size, strict_pixel_movement);
                    }
                }
                overlay.center_page_px != previous_center || overlay.deform_mesh != previous_mesh
            }
            TypingLayerMoveTarget::Raster(raster_idx) => {
                let Some(layer) = self
                    .raster_layers_by_page
                    .get_mut(&page_idx)
                    .and_then(|layers| layers.get_mut(raster_idx))
                else {
                    return false;
                };
                let previous_center = [layer.transform.cx, layer.transform.cy];
                // `DeformRec` is not `PartialEq`, so compare the control points (the only part a
                // move can change) rather than the whole record.
                let previous_points = layer
                    .deform
                    .as_ref()
                    .map(|rec| rec.points_px.clone())
                    .unwrap_or_default();
                match base {
                    TypingLayerMoveBase::Mesh { mesh, center } => {
                        let (moved, applied) = moved_mesh_from_base(mesh, delta_page_px, page_size, strict_pixel_movement);
                        layer.deform = Some(crate::models::layer_model::manifest::DeformRec {
                            cols: moved.cols,
                            rows: moved.rows,
                            points_px: moved.points_px.clone(),
                        });
                        // A deformed raster RENDERS from its mesh but is hit-tested and rotated about
                        // `transform.cx/cy`, so the affine center follows the mesh by the same applied
                        // delta — from the BASE center, so repeated frames cannot accumulate.
                        layer.transform.cx = center[0] + applied[0];
                        layer.transform.cy = center[1] + applied[1];
                    }
                    TypingLayerMoveBase::Center(center) => {
                        let moved = moved_center_from_base(*center, delta_page_px, page_size, strict_pixel_movement);
                        layer.transform.cx = moved[0];
                        layer.transform.cy = moved[1];
                    }
                }
                [layer.transform.cx, layer.transform.cy] != previous_center
                    || layer
                        .deform
                        .as_ref()
                        .is_some_and(|rec| rec.points_px != previous_points)
            }
        }
    }

    /// Drops the layer's cached mask clip so it is recomputed against the layer's NEW position.
    ///
    /// The clip depends on where the layer sits (mesh UVs sample the page mask), and neither store
    /// self-heals: the overlay's texture is only re-uploaded when marked stale, and the raster's
    /// `clipped_image` is otherwise only rebuilt by a full doc round-trip. Both recompute one frame
    /// later (`upload_pending_textures` / `prepare_raster_mask_clips`), matching the existing text
    /// behaviour, and both no-op when the layer has mask clipping off.
    fn invalidate_layer_move_clip(&mut self, target: TypingLayerMoveTarget, page_idx: usize) {
        match target {
            TypingLayerMoveTarget::Overlay(overlay_idx) => {
                self.mark_overlay_geometry_changed(overlay_idx, true);
            }
            TypingLayerMoveTarget::Raster(raster_idx) => {
                if let Some(layer) = self
                    .raster_layers_by_page
                    .get_mut(&page_idx)
                    .and_then(|layers| layers.get_mut(raster_idx))
                    && layer.mask_clip_enabled
                {
                    layer.clipped_image = None;
                }
            }
        }
    }

    /// Ends the gesture: exactly one texture flush and one DEFERRED persist per gesture.
    ///
    /// Does nothing at all when the session produced no geometry change, so a bare click leaves no
    /// trace and never marks the project edited. Consumes the session either way.
    pub(super) fn settle_layer_move(&mut self) {
        let Some(session) = self.move_session.take() else {
            return;
        };
        if !session.has_changes {
            return;
        }
        crate::trace_log!(
            cat::INPUT,
            "layer_move_settle target={:?} page={} source={:?} delta=({:.1},{:.1})",
            session.target,
            session.page_idx,
            session.source,
            session.delta_page_px[0],
            session.delta_page_px[1]
        );
        match session.target {
            TypingLayerMoveTarget::Overlay(overlay_idx) => {
                // An explicit layer move makes the centering frame FOLLOW the layer; re-bind it
                // before the next reconciliation can yank the layer back.
                self.sync_centering_frame_to_layer(overlay_idx, session.page_size_px);
                self.flush_overlay_texture_if_stale(overlay_idx);
                // EDIT (layer move): deferred, like every other placement edit.
                self.mark_placement_save_dirty();
            }
            TypingLayerMoveTarget::Raster(raster_idx) => {
                let Some(layer) = self
                    .raster_layers_by_page
                    .get(&session.page_idx)
                    .and_then(|layers| layers.get(raster_idx))
                else {
                    return;
                };
                let (uid, transform, deform) =
                    (layer.uid.clone(), layer.transform, layer.deform.clone());
                // ONE deferred write for the whole gesture (never a synchronous manifest rewrite on
                // the GUI thread, and never one per key press).
                let dispatch = match session.base {
                    TypingLayerMoveBase::Mesh { .. } => self.persist_raster_deform_deferred(
                        session.page_idx,
                        &uid,
                        transform,
                        deform,
                    ),
                    TypingLayerMoveBase::Center(_) => {
                        self.persist_raster_transform_deferred(session.page_idx, &uid, transform)
                    }
                };
                // The session is already consumed, so an unscheduled write has no owner left here.
                // The persist itself already logged the cause AND, for a possibly-transient write
                // failure, queued the page for a retry at the next flush point; this line ties that
                // to the gesture that produced it. A `NotWired` cause is deliberately not retried —
                // every later attempt reproduces it, which would be a per-frame loop.
                if let RasterPersistDispatch::NotEnqueued(failure) = dispatch {
                    crate::runtime_log::log_warn(format!(
                        "[typing] a raster layer MOVE was not persisted.\n\
                         Page: {}\nLayer uid: {uid}\nGesture: {:?} move of {:.1},{:.1} page px\n\
                         Class: {failure:?}\n\
                         The layer stays where the user put it on screen; see the preceding line for \
                         the cause and whether a retry was queued.",
                        session.page_idx,
                        session.source,
                        session.delta_page_px[0],
                        session.delta_page_px[1],
                    ));
                }
            }
        }
    }

    /// Re-applies an OPEN move session's `base + delta` after `sync_from_doc` rebuilt the geometry it
    /// was mutating.
    ///
    /// A gesture's geometry only reaches the shared document on settle, so a reprojection mid-gesture
    /// (a PS-tab edit bumps the doc version) overwrites the live layer with its pre-gesture state. A
    /// POINTER session self-heals only on the next frame the pointer actually moves; a KEYBOARD
    /// session never does — the delta would simply be lost, and the settle would then persist the
    /// reverted geometry. Re-applying from the base is exactly idempotent, so it costs nothing when
    /// the reprojection did not disturb the layer.
    ///
    /// The on-screen visibility limit is NOT re-run here (there is no `PageView` at a doc sync); it
    /// runs on the next interaction frame, where the per-page pass covers every layer anyway.
    pub(super) fn reapply_layer_move_after_reproject(&mut self, page_idx: usize) {
        let Some(mut session) = self.move_session.take() else {
            return;
        };
        if session.page_idx != page_idx {
            self.move_session = Some(session);
            return;
        }
        let page_size = session.page_size_px;
        if self.write_layer_move_geometry(
            session.target,
            session.page_idx,
            &session.base,
            session.delta_page_px,
            page_size,
            session.strict_pixel_movement,
        ) {
            session.has_changes = true;
            self.invalidate_layer_move_clip(session.target, session.page_idx);
        }
        self.move_session = Some(session);
    }

    /// Whether an open move gesture has changed geometry that nothing has written yet.
    ///
    /// Load-bearing for tab-leave / app-exit: `settle_layer_move` is the ONLY place a move marks the
    /// text layer dirty or hands raster geometry to the document, and it is normally driven from
    /// `TypingTabState::draw` — which stops being called the moment the user leaves the tab. Without
    /// this, an in-flight gesture is invisible to `has_pending_text_edits` and the edit is lost
    /// silently.
    #[must_use]
    pub(super) fn has_unsettled_layer_move(&self) -> bool {
        self.move_session
            .as_ref()
            .is_some_and(|session| session.has_changes)
    }

    /// DROPS an open move session without settling it — the DISCARD path only.
    ///
    /// Discard deletes the staging dir and shuts the saver down, so a settle here would re-dispatch
    /// exactly the write the user threw away (and, for a raster, re-create the deleted dir through
    /// the saver's sync fallback).
    pub(super) fn discard_layer_move(&mut self) {
        self.move_session = None;
    }

    /// Arrow-key nudge for the selected layer of `view.page_idx` — text overlay OR raster, whichever
    /// is selected (the two selections are mutually exclusive). SHIFT steps 5 px instead of 1.
    ///
    /// The nudge feeds the SAME move session the pointer drag uses, so a hold produces one gesture:
    /// the delta accumulates against a fixed base (a mesh can never be squashed by repeated
    /// application at a page edge), the mask clip is invalidated live, the visibility limit runs, and
    /// exactly ONE deferred write is dispatched when the last arrow is released — not one per key
    /// press.
    ///
    /// Every guard runs BEFORE any key is consumed, so a rejected gesture leaves the arrows to their
    /// real owner instead of silently swallowing them:
    /// - a focused text field anywhere in the UI (`panel_text_input_focused` or
    ///   `egui_wants_keyboard_input`, which is "any focused widget" — the same rule the sibling
    ///   canvas shortcut handlers use);
    /// - a text overlay in VECTOR transform mode (moving its center would invalidate the normalized
    ///   warp points, which is exactly why the pointer move is skipped there too);
    /// - a raster in perspective transform mode (there the mouse only edits corner handles).
    ///
    /// A text overlay in RASTER transform mode is deliberately still movable, because the mouse can
    /// whole-mesh-move it there as well: each kind's arrow rule matches that kind's mouse rule.
    ///
    /// Runs once per visible page, hence the per-page guards on both selections.
    pub(super) fn try_move_selected_layer_by_arrow_shortcuts(
        &mut self,
        ui: &mut egui::Ui,
        view: PageView,
        panel_text_input_focused: bool,
        strict_pixel_movement: bool,
    ) {
        let page_idx = view.page_idx;
        // Guard 1 — focus. Before `consume_key`, or a focused font-name autocomplete / combo box /
        // panel field would lose the arrow it is waiting for (risk 7 of the plan).
        if panel_text_input_focused || ui.ctx().egui_wants_keyboard_input() {
            return;
        }

        // Resolve the target. The two selections are mutually exclusive, so this cannot double-fire;
        // both are additionally pinned to the page that OWNS them.
        let target = if let Some(overlay_idx) = self.selected_overlay_idx {
            if self
                .overlays
                .get(overlay_idx)
                .is_none_or(|overlay| overlay.page_idx != page_idx)
            {
                return;
            }
            TypingLayerMoveTarget::Overlay(overlay_idx)
        } else if let Some(raster_idx) = self.selected_raster_idx {
            if self.selected_raster_page != Some(page_idx)
                || !self
                    .raster_layers_by_page
                    .get(&page_idx)
                    .is_some_and(|layers| raster_idx < layers.len())
            {
                return;
            }
            TypingLayerMoveTarget::Raster(raster_idx)
        } else {
            return;
        };

        // Guard 2 — transform modes. Also before `consume_key`, for the same reason.
        match target {
            TypingLayerMoveTarget::Overlay(overlay_idx) => {
                if self.transform_mode_overlay_idx == Some(overlay_idx)
                    && self.transform_mode_kind == TypingTransformModeKind::Vector
                {
                    return;
                }
            }
            TypingLayerMoveTarget::Raster(raster_idx) => {
                if self.transform_mode_raster_idx == Some(raster_idx) {
                    return;
                }
            }
        }

        // Guard 3 — can the gesture actually start? Checked BEFORE `consume_key` for the same reason
        // as the others: a target whose geometry cannot be snapshotted (e.g. a raster carrying a
        // malformed `DeformRec`) would otherwise eat the key and move nothing.
        if !self.can_begin_layer_move(target, page_idx, view) {
            return;
        }

        let (plain_lrud, shift_lrud) = ui.ctx().input_mut(|input| {
            (
                [
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                ],
                [
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowLeft),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowRight),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowUp),
                    input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowDown),
                ],
            )
        });
        let step_page_px = arrow_nudge_step_px(plain_lrud, shift_lrud);
        if step_page_px == [0.0, 0.0] {
            return;
        }

        // Re-opens nothing when a keyboard session for this target is already running (OS key repeat
        // keeps ONE gesture), and settles a pointer session first if one is somehow open. The guard
        // above already proved this can start, so a `false` here would mean the state changed under
        // us within the same frame — report it rather than nudging nothing silently.
        if self.begin_layer_move(
            target,
            page_idx,
            TypingLayerMoveSource::Keyboard,
            None,
            view,
        ) {
            self.add_keyboard_layer_move_step(step_page_px, view, strict_pixel_movement);
        } else {
            crate::runtime_log::log_warn(format!(
                "[typing] arrow nudge consumed a key but could not start a move.\n\
                 Page: {page_idx}\nTarget: {target:?}\n\
                 Cause: the layer's geometry could not be snapshotted as a move base."
            ));
        }
        // The session settles on the first frame with no arrow HELD, so frames must keep coming
        // (`wants_repaint` also covers the gap between key repeats).
        ui.ctx().request_repaint();
    }

    /// Once-per-frame settle driver. MUST be called from `TypingTabState::draw` AFTER `canvas.draw`
    /// (immediately before `drive_placement_save_debounce`): the move itself is applied inside
    /// `canvas.draw`'s callees, so an earlier tick would observe the release only on the NEXT frame —
    /// delaying every move write by a frame and stranding it entirely when the release frame is the
    /// last one drawn.
    pub(super) fn drive_layer_move_settle(&mut self, ctx: &egui::Context) {
        // Publish a status message parked by a settle that had no `egui::Context` (one driven from a
        // flush point). Done here because this is the one per-frame settle driver that has one.
        if let Some(message) = self.pending_status_error.take() {
            self.set_create_error(ctx, message);
        }
        let Some(source) = self.move_session.as_ref().map(|session| session.source) else {
            return;
        };
        // `keys_down` is level-triggered and survives our own `consume_key` (which only filters
        // events), and egui clears it on focus loss — so an alt-tab mid-hold settles the session
        // instead of leaving it open forever.
        let (primary_down, any_arrow_down) = ctx.input(|i| {
            (
                i.pointer.primary_down(),
                i.key_down(egui::Key::ArrowLeft)
                    || i.key_down(egui::Key::ArrowRight)
                    || i.key_down(egui::Key::ArrowUp)
                    || i.key_down(egui::Key::ArrowDown),
            )
        });
        if layer_move_settles(source, primary_down, any_arrow_down) {
            self.settle_layer_move();
        }
    }
}

/// Runs the ONE-SHOT whole-pixel snap of a move session's `base`, and reports whether it ran.
///
/// The snap is bound to the first real DISPLACEMENT, not to the press: a click that never moves the
/// pointer must leave the layer — and the project's dirty state — untouched. It therefore does
/// nothing while `delta_page_px` is zero, and nothing once `snap_applied` records that it already
/// ran, so a gesture snaps its base exactly once no matter how many frames it spans.
///
/// With `strict_pixel_movement` off the snap is the identity, but the once-only bookkeeping is the
/// same, so the caller's state machine does not branch on the setting.
#[must_use]
pub(super) fn snap_move_base_on_first_displacement(
    base: &mut TypingLayerMoveBase,
    snap_applied: bool,
    delta_page_px: [f32; 2],
    strict_pixel_movement: bool,
    page_size: [usize; 2],
) -> bool {
    if snap_applied || (delta_page_px[0] == 0.0 && delta_page_px[1] == 0.0) {
        return false;
    }
    *base = match &*base {
        TypingLayerMoveBase::Center(center) => TypingLayerMoveBase::Center(
            snapped_move_center_base(*center, strict_pixel_movement, page_size),
        ),
        TypingLayerMoveBase::Mesh { mesh, center } => {
            let centroid_before = deform_mesh_centroid_px(mesh);
            let snapped = snapped_move_mesh_base(mesh, strict_pixel_movement, page_size);
            // Shift the paired affine center by the same rigid delta the mesh took, so the two stay
            // locked together (raster hit quad / rotation center).
            let centroid_after = deform_mesh_centroid_px(&snapped);
            TypingLayerMoveBase::Mesh {
                mesh: snapped,
                center: [
                    center[0] + centroid_after[0] - centroid_before[0],
                    center[1] + centroid_after[1] - centroid_before[1],
                ],
            }
        }
    };
    true
}

/// Pure settle decision for one move session.
///
/// A `Pointer` gesture ends when the primary button is up; a `Keyboard` gesture ends on the first
/// frame with no arrow key HELD (level-triggered, so OS key-repeat gaps do not split one hold into
/// many gestures). Each source ignores the other's signal.
#[must_use]
pub(super) fn layer_move_settles(
    source: TypingLayerMoveSource,
    primary_down: bool,
    any_arrow_down: bool,
) -> bool {
    match source {
        TypingLayerMoveSource::Pointer => !primary_down,
        TypingLayerMoveSource::Keyboard => !any_arrow_down,
    }
}
