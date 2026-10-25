/*
File: crates/ms-tab-ps-editor/src/raster_effects.rs

Purpose:
Non-destructive raster-layer effects: the floating effects editor, the off-thread render job and the
GUI-side apply.

Key functions:
- `draw_effects_editor()`, `apply_effects_to_raster()`: start a render (latest-wins via
  `pending_raster_effects`).
- `PsRasterEffectsJob::run()`: the worker-thread render.
- `poll_ps_raster_effects_jobs()`, `apply_ps_raster_effects_result()`: recenter, doc routing,
  reversible persist.
- `effects_recenter_offset()`, `bake_raster()`, `apply_text_layer_op()`.

Notes:
Mirrors the typing tab's `apply_raster_effects_edit` pipeline; the GUI thread never runs
`apply_effects_to_color_image`.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

impl PsEditorTabState {
    /// Floating editor for a raster layer's effects chain (non-destructive: applying renders the
    /// chain off the GUI thread via `apply_effects_to_raster`, leaving the base pixels reversible).
    pub(crate) fn draw_effects_editor(&mut self, ctx: &egui::Context) {
        if self.effects_editor.is_none() {
            return;
        }
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new(t!("ps_editor.effects_editor.window_title"))
            .id(egui::Id::new("ps_editor.effects_editor.window_title"))
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(t!("ps_editor.effects_editor.chain_label"));
                if let Some((_, text)) = self.effects_editor.as_mut() {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .desired_rows(6)
                            .desired_width(360.0)
                            .code_editor(),
                    );
                }
                ui.horizontal(|ui| {
                    if ui
                        .button(t!("ps_editor.effects_editor.apply_button"))
                        .clicked()
                    {
                        apply = true;
                    }
                    if ui
                        .button(t!("ps_editor.effects_editor.cancel_button"))
                        .clicked()
                    {
                        cancel = true;
                    }
                });
                ui.small(t!("ps_editor.effects_editor.example_hint"));
            });

        if apply {
            if let Some((id, text)) = self.effects_editor.take() {
                self.apply_effects_to_raster(id, &text);
            }
        } else if cancel || !open {
            self.effects_editor = None;
        }
    }

    /// Applies an effects chain to raster layer `id` **non-destructively** (reversibly), matching the
    /// typing tab: effects render from the layer's pre-effects base pixels, the rendered result
    /// becomes the display `image`, and the chain is stored on the layer + persisted via
    /// `update_raster_effects` (base PNG untouched). An empty/blank chain clears effects and restores
    /// the base pixels.
    ///
    /// The expensive `apply_effects_to_color_image` call (tens of ms on a large page) runs on a
    /// worker thread, never the GUI thread: this method only parses the JSON, clones the base
    /// ColorImage (dropping every lock before spawning), and spawns the render. The result is applied
    /// by `poll_ps_raster_effects_jobs`, which does the cheap recenter / doc-routing / persist on the
    /// GUI thread. If a render is already in flight, the latest request is stashed (latest-wins) so a
    /// second raster's effects are not lost.
    pub(crate) fn apply_effects_to_raster(&mut self, id: LayerId, json: &str) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // Parse the editor text into the on-disk effects chain shape (a JSON array of objects, the
        // typing-tab contract). A blank string is "no effects".
        let effects: Vec<serde_json::Value> = if json.trim().is_empty() {
            Vec::new()
        } else {
            match serde_json::from_str::<Vec<serde_json::Value>>(json) {
                Ok(chain) => chain,
                Err(err) => {
                    ms_log::runtime_log::log_warn(format!("[ps_editor] effects parse: {err}"));
                    return;
                }
            }
        };

        if self.raster_effects_state.is_some() {
            // A render is already in flight: stash the latest request (superseding any older pending
            // one) so `poll_ps_raster_effects_jobs` re-dispatches it once the current render finishes.
            // Otherwise this edit would be silently lost — e.g. effecting a second raster right after
            // a first, leaving the second without its effects on save.
            self.pending_raster_effects = Some((id, json.to_string()));
            return;
        }

        // Resolve the pre-effects render source on the GUI thread (a cheap clone), then drop the
        // stack borrow BEFORE spawning so no lock is held across the worker. For a RAW raster the
        // source is the current display pixels (what's on screen now); for an effected raster it is
        // the preserved base, so re-applying replaces (not stacks) the chain.
        let prepared: Option<(ColorImage, [usize; 2], LayerTransform)> = {
            let Some(stack) = self.stack.as_ref() else {
                return;
            };
            let Some(layer) = stack.layer(id) else {
                return;
            };
            if layer.kind.is_base() {
                return;
            }
            let is_raw = layer.effects.is_empty();
            let base = if is_raw {
                layer.image.clone()
            } else {
                layer.base_image.clone()
            };
            let base_size = base.size;
            Some((base, base_size, layer.transform))
        };
        let Some((base_image, base_size, base_t)) = prepared else {
            return;
        };
        let Some(uid) = self.raster_uid(id) else {
            return;
        };

        // Spawn the expensive render off the GUI thread; `poll_ps_raster_effects_jobs` applies it.
        let job = PsRasterEffectsJob {
            page_idx,
            uid,
            id,
            base_image,
            base_size,
            base_t,
            json: json.to_string(),
            effects,
        };
        let (tx, rx) = mpsc::channel::<Result<PsRasterEffectsResult, String>>();
        let (log_page, log_uid) = (job.page_idx, job.uid.clone());
        thread::spawn(move || {
            // The receiver is gone only when the tab was torn down (or the state replaced) while
            // rendering; the result has no consumer then, so it is dropped with a note.
            if tx.send(job.run()).is_err() {
                ms_log::runtime_log::log_info(format!(
                    "[ps_editor] raster effects result discarded: receiver gone (page={log_page} uid={log_uid})"
                ));
            }
        });
        self.raster_effects_state = Some(rx);
    }

    /// Polls the non-destructive raster-effects worker once per frame. When a result arrives it does
    /// the GUI-side cheap work the typing tab keeps on the main thread: the recenter anchoring math,
    /// the `edit_doc_node` routing (swap base/display/effects + bump generation), the reversible
    /// `update_raster_effects` persist, and the `render_cache` drop. Then it re-dispatches any
    /// request stashed while the render was in flight (latest-wins). Returns `true` when a result was
    /// consumed (the frame should repaint).
    pub(crate) fn poll_ps_raster_effects_jobs(&mut self, project: &ProjectData) -> bool {
        let recv = {
            let Some(rx) = self.raster_effects_state.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err(t!("ps_editor.effects.raster_interrupted").to_string()))
                }
            }
        };
        let Some(recv) = recv else {
            return false;
        };
        self.raster_effects_state = None;
        let result = match recv {
            Ok(r) => r,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[ps_editor] effects: {err}"));
                // Still re-dispatch a stashed request so a queued edit is not stranded.
                self.dispatch_pending_raster_effects();
                return true;
            }
        };
        self.apply_ps_raster_effects_result(result, project);
        // Re-dispatch an edit that arrived while this render was in flight, so the last requested
        // effects (e.g. on a second raster) are not lost. `raster_effects_state` is now `None`, so
        // this spawns a fresh render instead of re-stashing.
        self.dispatch_pending_raster_effects();
        true
    }

    /// Re-dispatches the stashed raster-effects request, if any. Called from
    /// `poll_ps_raster_effects_jobs` after the in-flight render is consumed.
    pub(crate) fn dispatch_pending_raster_effects(&mut self) {
        if let Some((id, json)) = self.pending_raster_effects.take() {
            self.apply_effects_to_raster(id, &json);
        }
    }

    /// GUI-side apply step for a completed raster-effects render (mirrors the typing tab's
    /// `poll_raster_effects_jobs` body). Performs the recenter anchoring, routes the swap to the
    /// shared doc, persists the chain reversibly (base PNG untouched), and drops the layer's GPU
    /// cache so the new display re-uploads. No long work, no decode, no held lock across a worker.
    pub(crate) fn apply_ps_raster_effects_result(
        &mut self,
        result: PsRasterEffectsResult,
        project: &ProjectData,
    ) {
        let PsRasterEffectsResult {
            page_idx,
            uid,
            id,
            new_image,
            origin,
            base_size,
            base_t,
            effects,
        } = result;

        let new_size = new_image.size;
        // World-space center shift that keeps the original content anchored after effects grow the
        // image (shadow/glow). Pure math, unit-tested in `effects_recenter_offset`.
        let rotated = effects_recenter_offset(new_size, origin, base_size, base_t);

        // Compute the post-effects display, base snapshot, and recentered transform, then write them
        // to the shared doc. The base snapshot only happens going RAW→effects — when effects already
        // exist, the doc node's base is left untouched. Routed via the doc so the projection re-derives
        // the stack layer from one model.
        let (display, new_transform): (Option<ColorImage>, TransformRec) = if effects.is_empty() {
            // No effects: display is the (current) base, placed at the base transform.
            (None, transform_to_rec(base_t))
        } else {
            let mut t = base_t;
            t.center = base_t.center + rotated;
            (Some(new_image), transform_to_rec(t))
        };
        let effects_for_doc = effects.clone();
        // The RAW→effects base snapshot is keyed on the doc node's own effects state (`e.is_empty()`)
        // below, the authoritative trigger — the same rule the original synchronous path used.
        self.edit_doc_node(page_idx, |doc| {
            if let Some(node) = doc.node_mut(page_idx, &uid)
                && let ms_models::layer_model::layer_doc::NodeBody::Raster {
                    base_image,
                    display_image,
                    effects: e,
                    ..
                } = &mut node.body
            {
                if effects_for_doc.is_empty() {
                    // Clear: display becomes the base; chain emptied; transform restored.
                    *display_image = base_image.clone();
                    *e = Vec::new();
                } else {
                    // RAW→effects: snapshot the current display as the new base first.
                    if e.is_empty() {
                        *base_image = display_image.clone();
                    }
                    if let Some(d) = display.clone() {
                        *display_image = d;
                    }
                    *e = effects_for_doc.clone();
                }
                node.transform = new_transform;
                node.bump_generation();
            }
        });

        // Persist reversibly: writes the effects chain + the `_fx` rendered PNG (or clears them both),
        // leaving the base PNG intact. (`flush_page` only re-writes non-empty chains, so the CLEAR case
        // is handled here.) ASYNC: route through the doc's effects-only saver path (PNG encode
        // off-thread; targeted single-raster RMW, never a whole-page rewrite) — falls back to the sync
        // `update_raster_effects` when no saver is enabled. The save-to-project / app-close barriers
        // guarantee the enqueued effects land. Then the cross-tab bump already happened via the doc edit.
        let rendered_for_persist = if effects.is_empty() {
            None
        } else {
            display.as_ref()
        };
        let effects_persist = self
            .layer_doc
            .as_ref()
            .and_then(|doc| {
                doc.lock().ok().map(|mut guard| {
                    guard.enqueue_raster_effects(
                        page_idx,
                        &project.paths.unsaved_layers_dir,
                        Some(&project.paths.layers_dir),
                        &uid,
                        &effects,
                        rendered_for_persist,
                    )
                })
            })
            // No doc wired (defensive): fall back to a direct synchronous effects write so the disk
            // state is still correct, identical to the pre-async behavior.
            .unwrap_or_else(|| {
                persist::update_raster_effects(
                    &project.paths.unsaved_layers_dir,
                    page_idx,
                    &uid,
                    &effects,
                    rendered_for_persist,
                    Some(&project.paths.layers_dir),
                )
            });
        if let Err(err) = effects_persist {
            ms_log::runtime_log::log_warn(format!("[ps_editor] persist effects: {err}"));
        }
        self.render_cache.remove(&id);
        // No cross-tab signal needed: the MODEL change routed through `edit_doc_node` above, which
        // bumped the doc version (so the typing tab re-projects).
    }

    /// Bakes (запекает) raster `id`: flattens its non-destructive effects render into the base
    /// pixels and clears the chain, turning it into an ordinary directly-editable raster. The dirty
    /// save rewrites the base PNG and drops the chain — a permanent flatten. A raw raster (empty
    /// effects) is a no-op.
    pub(crate) fn bake_raster(&mut self, id: LayerId, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // Read the rendered display + effects state from the stack (the projection mirror of the doc).
        let Some((uid, display, has_effects)) = self
            .stack
            .as_ref()
            .and_then(|s| s.layer(id))
            .filter(|l| l.kind == LayerKind::Raster)
            .map(|l| (l.uid.to_string(), l.image.clone(), !l.effects.is_empty()))
        else {
            return;
        };
        if !has_effects {
            return; // raw raster: nothing to bake
        }
        // Route to the doc: the rendered display becomes the new base, the chain is dropped, and the
        // node is marked pixels_dirty so the flush rewrites the base PNG. `route_to_doc` flushes +
        // bumps + re-projects (the projection drops the stale cache via the generation change).
        let base = display.clone();
        self.route_to_doc(page_idx, project, |doc| {
            doc.set_raster_pixels(page_idx, &uid, base, display, Vec::new(), true);
        });
        self.render_cache.remove(&id);
    }

    /// Applies a deferred panel action (rasterize) on the text layer at `index`.
    pub(crate) fn apply_text_layer_op(&mut self, index: usize, op: TextLayerOp, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        match op {
            TextLayerOp::Rasterize => {
                let Some(layer) = self.text_layers.get(index) else {
                    return;
                };
                let (uid, name, image, transform) = (
                    layer.uid.clone(),
                    layer.name.clone(),
                    layer.image().clone(),
                    layer.transform(),
                );
                // Add the baked raster on the stack first (it becomes pixels_dirty so a re-projection
                // keeps it), then mirror the op onto the shared doc: add the new Raster node and remove
                // the Text node. The new raster carries the text overlay's placement.
                let new_id = self
                    .stack
                    .as_mut()
                    // Persisted raster-layer name (round-trips to `layers.json`); stable
                    // literal, not localized. See dev-docs/i18n_exclusions.md §A.
                    .map(|s| {
                        s.add_raster_layer_image(format!("Запечён: {name}"), image, transform)
                    });
                // The doc is the sole text writer: removing the Text node and flushing drops it from
                // `layers.json` (a migrated page ignores the stale `text_info.json` entry, so the
                // rasterized overlay does not resurrect). No `text_info.json` write here.
                let new_node = new_id
                    .and_then(|id| self.stack.as_ref().and_then(|s| s.layer(id)))
                    .map(layer_to_raster_node);
                if let Some(node) = new_node {
                    let text_uid = uid.clone();
                    self.route_to_doc(page_idx, project, |doc| {
                        doc.remove_node(page_idx, &text_uid);
                        doc.add_node(page_idx, node);
                    });
                }
            }
        }
    }
}

impl PsRasterEffectsJob {
    /// Worker: renders the effects chain from the pre-effects base image. Runs the expensive
    /// `apply_effects_to_color_image` off the GUI thread and returns the data the GUI-side apply
    /// step needs (the new display image, the base content `origin` inside it, and the recenter
    /// references).
    ///
    /// An empty `effects` chain means "clear effects": the result carries the base image unchanged
    /// with a zero `origin`, so the GUI step restores the base placement.
    ///
    /// # Errors
    /// Returns a human-readable message string when the effects render fails (the caller logs it
    /// and keeps the raster unchanged).
    pub(crate) fn run(self) -> Result<PsRasterEffectsResult, String> {
        let Self {
            page_idx,
            uid,
            id,
            base_image,
            base_size,
            base_t,
            json,
            effects,
        } = self;
        if effects.is_empty() {
            // No render needed: clearing effects restores the base. `origin` is the base top-left.
            return Ok(PsRasterEffectsResult {
                page_idx,
                uid,
                id,
                new_image: base_image,
                origin: [0, 0],
                base_size,
                base_t,
                effects,
            });
        }
        match effects::apply_effects_to_color_image(&base_image, &json) {
            Ok((new_image, origin)) => Ok(PsRasterEffectsResult {
                page_idx,
                uid,
                id,
                new_image,
                origin,
                base_size,
                base_t,
                effects,
            }),
            Err(err) => Err(err),
        }
    }
}

/// World-space center offset that keeps a raster's original content anchored after a
/// non-destructive effects render resized the image (effects like shadow/glow grow the canvas and
/// shift the base content to `origin` inside the new image).
///
/// `new_size`/`base_size` are `[w, h]` in pixels; `origin` is the base content's top-left inside the
/// new image (the `[i32; 2]` content origin from `apply_effects_to_color_image`). The raw center
/// delta is taken in base-local pixels, then mapped through the base transform's scale + rotation so
/// it is added directly to `LayerTransform::center` in world space. Repeated re-applies stay stable
/// because the delta is always measured against the same pre-effects base reference.
#[must_use]
pub(crate) fn effects_recenter_offset(
    new_size: [usize; 2],
    origin: [i32; 2],
    base_size: [usize; 2],
    base_t: LayerTransform,
) -> Vec2 {
    let dx = new_size[0] as f32 * 0.5 - origin[0] as f32 - base_size[0] as f32 * 0.5;
    let dy = new_size[1] as f32 * 0.5 - origin[1] as f32 - base_size[1] as f32 * 0.5;
    let (sin, cos) = base_t.rotation.sin_cos();
    let scaled = Vec2::new(dx, dy) * base_t.scale;
    Vec2::new(
        scaled.x * cos - scaled.y * sin,
        scaled.x * sin + scaled.y * cos,
    )
}
