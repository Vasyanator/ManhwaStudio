/*
File: tabs/ps_editor/tools/patch.rs

Purpose:
The PS editor's host for the «Заплатка» (patch) tool. The tool itself — the selection, the
gesture, the ROI geometry, the membrane solve and the outline painting — lives in
`ms_tools::patch`; this file is the adapter that drives it against this editor's viewport and
turns its answer into a `PsToolAction::WriteRegion` for the tab to commit.

Main responsibilities:
- Implement `PsTool` by forwarding the pointer gesture, the on-canvas pass and the options pane to
  `PatchToolCore`.
- Implement `PatchHost` in TWO halves: the geometry answers, which only need this frame's
  `ViewTransform`, and the pixel work, which needs the `LayerStack` and is therefore PARKED and
  serviced on the next `interact`.
- Decide whether the active layer may receive a patch at all, and say why when it may not.
- Build the ROI composite (the membrane's destination) and the backdrop (the store step's `base`)
  in the two planes that must agree: the patch is solved in the plane the ACTIVE layer is part of
  and stored over the plane strictly BELOW it.

Key structures:
- `PatchTool`: the `PsTool` implementation; a core plus this host's state.
- `HostState`: everything the two host halves hand each other across a frame boundary.
- `PsPatchHost`: the short-lived `PatchHost` view over `HostState`.
- `PatchTarget` / `PatchRefusal`: which layer a patch may be written into, or why none may.
- `ParkedCommit`: a solved patch waiting for an `interact` that can read the layer stack.

Key functions:
- `patch_target_for()`: THE refusal predicate — pure, unit-tested, and the only place the four
  eligibility rules live.
- `PatchTarget::local_origin_for()`: the fifth rule — the ROI must lie inside the layer's own
  footprint, because `ToolRegionWrite` geometry is LAYER-local while the patch works in PAGE px.
- `PatchTool::service_pending_region()`: the SOLVE-PLANE composite, run on the GUI thread by design.
- `PatchTool::service_pending_commit()`: the STORE step — solved colours into layer pixels.
- `backdrop_rect()`: what the written pixels will be composited OVER.

Notes:
Why the host is split in two: `PsTool::draw_overlay_ui` — the hook the core's whole frame pass runs
from — is handed no `PsToolContext` and therefore no `LayerStack`, deliberately, so an on-canvas
pass cannot mutate pixels behind the tab's back. Everything the core asks for there that needs
pixels (`start_region_load`, `commit_patch`) is parked in `HostState` and serviced by the next
`interact`, which does hold `&mut LayerStack`. That costs ONE frame on an operation that is already
asynchronous, and it is the same shape the tab itself uses for `apply_tool_actions` and the brush
for its per-frame cursor cache.

Why no worker thread for the region load: the cleaning host needs one because it decodes a PNG from
disk. Here the pixels are already in memory in the `LayerStack`, so the composite is a bounded
ROI-sized buffer walk on the GUI thread. The ROI is the user's selection box plus the drag offset,
padded — small relative to the membrane solve, which IS off-thread (the core owns that worker), so
`AGENTS.md` §5 is honoured. A selection large enough to make this composite visible would already
have made the solve unusable.
*/

use super::super::layers::{Layer, LayerId, LayerKind, LayerStack};
use super::super::viewport::ViewTransform;
use super::super::{CompositeBound, CompositeLayer, composite_rect, visible_layers_bottom_to_top};
use super::{
    PsHotkeyRow, PsTool, PsToolAction, PsToolContext, PsToolId, PsToolOverlayCx, ToolOutcome,
    ToolRegionWrite,
};
use ms_canvas::OverlayRectPx;
use ms_tools::overlay_pixel_for_final_color;
use ms_tools::patch::{
    PagePixelProjection, PatchCommit, PatchHost, PatchHostError, PatchRegionPoll,
    PatchRegionRequest, PatchToolCore,
};
use eframe::egui;
use egui::{Color32, ColorImage, Pos2};

/// Log prefix of this host. Also the reason a log line says which surface a patch came from.
const LOG_TAG: &str = "ps_editor/patch";

/// `egui::Id` source of the bare layer the selection outline is painted on.
///
/// Distinct from the cleaning host's, as `PatchToolCore::new` requires: two hosts sharing one layer
/// id would paint into the same layer.
const OUTLINE_LAYER_ID: &str = "ps_editor_patch_outline";

/// Largest whole-pixel layer placement this host will convert to an integer, in page pixels.
///
/// Well beyond any real page (a ribbon is ~800x19000) and far below the point where an `f32` stops
/// representing integers exactly, so the conversion in [`axis_aligned_origin`] cannot lose a pixel.
const MAX_PLACEMENT_PX: f32 = 1.0e9;

/// Why the active layer may not receive a patch.
///
/// Every variant is a REFUSAL, never a fallback: the patch works in PAGE pixels while
/// `ToolRegionWrite` geometry is LAYER-local, so the two spaces must coincide up to a whole-pixel
/// translation. Clamping or reprojecting instead would silently drop part of the patch or land it
/// on the wrong pixels (`AGENTS.md` §6).
///
/// `Eq` is deliberately absent: `Transformed` carries the raw `f32` transform values for the log,
/// so only `PartialEq` is meaningful. The tests compare variants, which is all `PartialEq` needs.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
enum PatchRefusal {
    /// The stack has no active layer at all (no page resident, or a stale active id).
    #[error("the layer stack has no active layer")]
    NoActiveLayer,
    /// `Исходник` is structurally read-only.
    #[error("the active layer is the immutable source layer")]
    SourceLayer,
    /// The active layer contributes nothing to the composite — hidden, inside a hidden group, or
    /// at zero effective opacity — so the solve plane would not contain the layer being written.
    #[error("the active layer is hidden or fully transparent")]
    InvisibleLayer,
    /// A raster still showing a non-destructive effects chain: bake it first.
    #[error("the active raster still shows a non-destructive effects chain")]
    EffectsChain,
    /// A deformed raster is rendered through its mesh, so its affine transform does not describe
    /// where its pixels land. The same reason `TransformTool` refuses one.
    #[error("the active layer carries a deform mesh")]
    Deformed,
    /// Rotated, scaled, or placed off the whole-pixel grid.
    #[error(
        "the active layer is rotated ({rotation} rad), scaled ({scale}x) or placed off the \
         whole-pixel grid"
    )]
    Transformed { rotation: f32, scale: f32 },
    /// The ROI is not fully inside the layer's own footprint.
    #[error(
        "the ROI {roi_x};{roi_y} {roi_w}x{roi_h} is not inside the {layer_w}x{layer_h} layer \
         placed at {origin_x};{origin_y}"
    )]
    RoiOutsideLayer {
        roi_x: usize,
        roi_y: usize,
        roi_w: usize,
        roi_h: usize,
        layer_w: usize,
        layer_h: usize,
        origin_x: i64,
        origin_y: i64,
    },
}

impl PatchRefusal {
    /// The already-localized sentence this refusal shows the user.
    ///
    /// The technical numbers stay in the `Display` text, which only reaches the log.
    fn user_message(self) -> String {
        match self {
            Self::NoActiveLayer => t!("ps_editor.tools.patch_refuse_no_layer").to_string(),
            Self::SourceLayer => t!("ps_editor.tools.patch_refuse_source_layer").to_string(),
            Self::InvisibleLayer => {
                t!("ps_editor.tools.patch_refuse_invisible_layer").to_string()
            }
            Self::EffectsChain => t!("ps_editor.tools.patch_refuse_effects").to_string(),
            Self::Deformed => t!("ps_editor.tools.patch_refuse_deformed").to_string(),
            Self::Transformed { .. } => t!("ps_editor.tools.patch_refuse_transformed").to_string(),
            Self::RoiOutsideLayer { .. } => {
                t!("ps_editor.tools.patch_refuse_roi_outside").to_string()
            }
        }
    }
}

/// The layer one patch will be written into, and where its pixel grid sits on the page.
///
/// Produced only by [`patch_target_for`], so a `PatchTarget` in hand is proof that all four
/// layer-level rules passed. The ROI-level rule is [`PatchTarget::local_origin_for`], which needs
/// an ROI and therefore cannot be answered at the same moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PatchTarget {
    /// The layer itself, so a parked commit can check it is still the active one.
    layer_id: LayerId,
    /// Position of the layer in the stack, i.e. how many layers sit strictly below it.
    stack_idx: usize,
    /// The kind, which decides what the store step's backdrop is.
    kind: LayerKind,
    /// PAGE-pixel position of the layer image's top-left corner. Whole pixels by construction.
    origin: [i64; 2],
    /// The layer image's size in its own pixels.
    size: [usize; 2],
}

impl PatchTarget {
    /// Where `roi` starts in this layer's OWN pixel grid.
    ///
    /// The fifth refusal rule, and the reason it is separate from [`patch_target_for`]: it needs an
    /// ROI, which does not exist while the options pane renders the eligibility of the active
    /// layer.
    ///
    /// # Errors
    /// [`PatchRefusal::RoiOutsideLayer`] when any part of `roi` falls outside the layer. It is
    /// never clipped here: `ToolRegionWrite` clipping is the TAB's job for a rect that reaches the
    /// page edge, but a patch whose ROI leaves the layer would be committed with part of its
    /// content silently dropped, and the user would see a half-applied patch with no explanation.
    fn local_origin_for(&self, roi: OverlayRectPx) -> Result<[usize; 2], PatchRefusal> {
        let outside = || PatchRefusal::RoiOutsideLayer {
            roi_x: roi.x,
            roi_y: roi.y,
            roi_w: roi.w,
            roi_h: roi.h,
            layer_w: self.size[0],
            layer_h: self.size[1],
            origin_x: self.origin[0],
            origin_y: self.origin[1],
        };
        let to_i64 = |value: usize| i64::try_from(value).map_err(|_| outside());
        let local_x = to_i64(roi.x)? - self.origin[0];
        let local_y = to_i64(roi.y)? - self.origin[1];
        let end_x = local_x + to_i64(roi.w)?;
        let end_y = local_y + to_i64(roi.h)?;
        if local_x < 0 || local_y < 0 || end_x > to_i64(self.size[0])? || end_y > to_i64(self.size[1])?
        {
            return Err(outside());
        }
        let x = usize::try_from(local_x).map_err(|_| outside())?;
        let y = usize::try_from(local_y).map_err(|_| outside())?;
        Ok([x, y])
    }
}

/// The PAGE-pixel position of `layer`'s top-left corner, when its placement is a whole-pixel
/// translation and nothing else.
///
/// `None` for a rotated or scaled layer, and for one whose translation is fractional or absurd:
/// each of those makes the layer's pixel grid disagree with the page's, which is exactly what the
/// patch's page-pixel geometry may not survive.
fn axis_aligned_origin(layer: &Layer) -> Option<[i64; 2]> {
    let transform = layer.transform;
    if transform.rotation != 0.0 || (transform.scale - 1.0).abs() > f32::EPSILON {
        return None;
    }
    let corner = [
        transform.center.x - layer.image.size[0] as f32 * 0.5,
        transform.center.y - layer.image.size[1] as f32 * 0.5,
    ];
    let mut origin = [0_i64; 2];
    for (out, value) in origin.iter_mut().zip(corner) {
        if !value.is_finite() || value.abs() > MAX_PLACEMENT_PX {
            return None;
        }
        let rounded = value.round();
        if (value - rounded).abs() > f32::EPSILON {
            return None;
        }
        // `rounded` is an integral `f32` whose magnitude is at most `MAX_PLACEMENT_PX` (1e9), far
        // inside `i64`'s range and inside the range where `f32` represents integers exactly, so the
        // conversion is lossless (`AGENTS.md` §17's proven-safe exception).
        *out = rounded as i64;
    }
    Some(origin)
}

/// Whether the stack's ACTIVE layer may receive a patch, and where its pixel grid sits.
///
/// THE refusal predicate: the options pane, the region-load start and the commit all read this one
/// answer, so what the user is told and what the tool does can never disagree. Pure — a
/// `LayerStack` in, a verdict out — and unit-tested one case per variant.
///
/// The default active layer of a freshly loaded page is `Клин`, which is page-sized and carries the
/// identity transform, so the ordinary case passes every rule.
///
/// # Errors
/// One [`PatchRefusal`] per rule: no active layer, `Исходник`, a layer that is not visible, an
/// unbaked effects chain, a deform mesh, or a rotated / scaled / fractionally-placed layer.
fn patch_target_for(stack: &LayerStack) -> Result<PatchTarget, PatchRefusal> {
    let active_id = stack.active_id();
    let (stack_idx, layer) = stack
        .layers()
        .iter()
        .enumerate()
        .find(|(_, layer)| layer.id == active_id)
        .ok_or(PatchRefusal::NoActiveLayer)?;
    if layer.kind == LayerKind::Source {
        return Err(PatchRefusal::SourceLayer);
    }
    // The SAME predicate `visible_layers_bottom_to_top` filters on, so "the active layer is in the
    // solve plane" is checked rather than assumed: the patch samples the plane the target layer is
    // part of, and a target the composite drops would be solved against pixels that do not contain
    // it and then written where the user cannot see the result (`AGENTS.md` §6).
    if !stack.layer_visible(layer) || stack.layer_opacity(layer) <= 0.0 {
        return Err(PatchRefusal::InvisibleLayer);
    }
    // Everything but `Source` reaches here, so a `false` can only mean an unbaked effects chain —
    // the state the tab already hints «Сначала запеките слой» about on a brush press.
    if !layer.can_edit_pixels() {
        return Err(PatchRefusal::EffectsChain);
    }
    if layer.deform.is_some() {
        return Err(PatchRefusal::Deformed);
    }
    let origin = axis_aligned_origin(layer).ok_or(PatchRefusal::Transformed {
        rotation: layer.transform.rotation,
        scale: layer.transform.scale,
    })?;
    Ok(PatchTarget {
        layer_id: layer.id,
        stack_idx,
        kind: layer.kind,
        origin,
        size: layer.image.size,
    })
}

/// The pixels the patch's own pixels will be composited OVER, for `roi` of the page.
///
/// This is the `base` argument [`overlay_pixel_for_final_color`] is defined against, and it is a
/// property of WHERE the patch is stored, not of what it looks like:
/// * `Клин` — the shared clean overlay — is always composited over the page source, so its backdrop
///   is `Исходник` and nothing else. It is read straight out of `stack.layers()[0]`, which
///   `LayerStack::new` places there page-sized with the identity transform, and its VISIBILITY is
///   deliberately ignored: hiding `Исходник` in this editor is a view toggle, while the clean
///   overlay's storage contract ("these pixels sit over the page source") is not.
/// * a user raster is composited over whatever is visible below it, so its backdrop is the ordered
///   composite of exactly those layers.
///
/// The returned image is `roi`-sized and premultiplied. Where the backdrop is fully transparent —
/// possible only for a raster with nothing opaque beneath it — the solved pixel is solved against
/// black, which is the documented limit of `overlay_pixel_for_final_color`'s "known, opaque
/// backdrop" contract.
fn backdrop_rect(stack: &LayerStack, target: &PatchTarget, roi: OverlayRectPx) -> ColorImage {
    if target.kind == LayerKind::Clean
        && let Some(source) = stack.layers().first()
        && source.kind == LayerKind::Source
    {
        // A page-sized identity layer: the ROI maps 1:1, so this is a direct memory read rather
        // than a composite. The general path below would answer the same thing whenever `Исходник`
        // is visible; this branch is also what keeps the answer right when it is hidden.
        let layers: [CompositeLayer<'_>; 1] = [(source, 1.0)];
        return composite_rect(&layers, roi, None);
    }
    let layers = visible_layers_bottom_to_top(stack, CompositeBound::Below(target.stack_idx));
    composite_rect(&layers, roi, None)
}

/// Page geometry as of the last frame that had any, captured so the geometry half of [`PatchHost`]
/// can answer from either `interact` or `draw_overlay_ui`.
///
/// `viewport` is the canvas rect in screen points — the outer bound the outline is clipped to. The
/// tab builds `ViewTransform::viewport_rect` from the very same rect it puts in
/// [`PsToolOverlayCx::viewport`], so `interact`, which is handed only the transform, can fill this
/// from there and the two hooks always agree.
#[derive(Debug, Clone, Copy)]
struct PageGeometry {
    view: ViewTransform,
    viewport: egui::Rect,
    page_size: [usize; 2],
    page_idx: usize,
}

/// One solved patch waiting for an `interact` that can read the layer stack.
///
/// The buffers are OWNED copies: `PatchCommit` borrows the worker's result, which the core drops as
/// soon as `commit_patch` returns.
#[derive(Debug, Clone)]
struct ParkedCommit {
    page_idx: usize,
    roi: OverlayRectPx,
    /// The colour each ROI pixel must end up showing.
    rgb: Vec<[u8; 3]>,
    /// How strongly the patch claims each ROI pixel, `0..=1`.
    coverage: Vec<f32>,
}

/// Everything the two halves of this host hand each other across a frame boundary.
///
/// `draw_overlay_ui` writes the two `pending_*` slots and reads `region` / `region_error`;
/// `interact` does the opposite. Nothing here outlives a cancelled job: `discard_region_load`
/// clears all of it, and the core calls that after every finished solve and every cancellation.
#[derive(Debug, Default)]
struct HostState {
    /// This frame's page geometry, or `None` before the first frame with a resident page.
    geometry: Option<PageGeometry>,
    /// Eligibility of the active layer as of the last `interact`; `None` before the first one.
    target: Option<Result<PatchTarget, PatchRefusal>>,
    /// A composite the core asked for, parked until the next `interact`.
    pending_region: Option<PatchRegionRequest>,
    /// The composited ROI, waiting to be handed to the core.
    region: Option<ColorImage>,
    /// Why the parked composite could not be produced. English; the core supplies the sentence.
    region_error: Option<String>,
    /// A finished patch, parked until the next `interact`.
    pending_commit: Option<ParkedCommit>,
    /// What the tool asks the tab to commit, drained by `take_actions`.
    queued: Vec<PsToolAction>,
    /// A host-side sentence shown under the core's own controls.
    ///
    /// The core owns its own status line; this one carries what only the host can know — that the
    /// active layer refused the write after the solve had already finished.
    status: Option<String>,
}

impl HostState {
    /// Drops what is retained for one REGION LOAD. Idempotent, as `discard_region_load` must be.
    ///
    /// A parked COMMIT is deliberately not dropped here: the core calls `discard_region_load`
    /// immediately after a `commit_patch` it accepted, and a patch the host has accepted is the
    /// user's finished work — it must reach `take_actions`, not be swept up with the job's scratch
    /// buffers.
    fn clear_region_load(&mut self) {
        self.pending_region = None;
        self.region = None;
        self.region_error = None;
    }

    /// Drops a parked commit that will never be serviced, saying so.
    ///
    /// Reachable only through [`PsTool::reset`] — a tool, page or gesture abandonment in the ONE
    /// frame between `commit_patch` and the `interact` that would have built the write. Silence
    /// there would lose a finished patch with no trace (`AGENTS.md` §6), so it is reported even
    /// though the window is a single frame.
    fn drop_parked_commit(&mut self, reason: &dyn std::fmt::Display) {
        if let Some(commit) = self.pending_commit.take() {
            let text = t!("ps_editor.tools.patch_error_apply").to_string();
            self.fail(
                text,
                &format!(
                    "a solved patch for page {} at {:?} was abandoned before it could be                      committed: {reason}",
                    commit.page_idx, commit.roi
                ),
            );
        }
    }

    /// Records a host-side failure in the log and in the panel's status line.
    fn fail(&mut self, message: String, detail: &dyn std::fmt::Display) {
        ms_log::runtime_log::log_warn(format!("[{LOG_TAG}] {message} | {detail}"));
        self.status = Some(message);
    }
}

/// The [`PatchHost`] view the core is driven through for one hook.
///
/// Short-lived on purpose, and deliberately holding NO layer stack: the geometry hooks answer from
/// [`PageGeometry`], and the two hooks that need pixels park their request instead.
struct PsPatchHost<'a> {
    state: &'a mut HostState,
}

impl PsPatchHost<'_> {
    /// This frame's geometry, but only for the page the caller asked about.
    ///
    /// Every geometry hook is page-indexed and the editor is single-page, so a mismatch means the
    /// core is asking about a page that is no longer resident — which must answer `None`, not the
    /// current page's numbers.
    fn geometry_for(&self, page_idx: usize) -> Option<PageGeometry> {
        self.state
            .geometry
            .filter(|geometry| geometry.page_idx == page_idx)
    }
}

impl PatchHost for PsPatchHost<'_> {
    fn page_source_size(&self, page_idx: usize) -> Option<[usize; 2]> {
        let geometry = self.geometry_for(page_idx)?;
        (geometry.page_size[0] > 0 && geometry.page_size[1] > 0).then_some(geometry.page_size)
    }

    fn page_projection(&self, page_idx: usize) -> Option<PagePixelProjection> {
        let geometry = self.geometry_for(page_idx)?;
        let [w, h] = geometry.page_size;
        if w == 0 || h == 0 {
            return None;
        }
        // The selection's page-pixel space IS this editor's world space (`viewport.rs`: world
        // coordinates are image pixels), so the page rect is the world rect of the whole page.
        let scene_rect = geometry.view.world_rect_to_screen(egui::Rect::from_min_size(
            Pos2::ZERO,
            egui::Vec2::new(w as f32, h as f32),
        ));
        scene_rect.is_positive().then_some(PagePixelProjection {
            scene_rect,
            pixel_size: geometry.page_size,
        })
    }

    /// The whole canvas rect, with NO panel cut-out.
    ///
    /// The dock panels float on `Order::Foreground` (`widgets/panel_dock/panel.rs`) while the core
    /// paints its outline on `Order::Middle`, so a panel already covers the outline by z-order and
    /// cutting it out would only duplicate that. The clip still matters: this editor is one program
    /// tab among several, and a layer painter is not otherwise bounded by the tab's area.
    fn usable_viewport(&self) -> Option<egui::Rect> {
        let viewport = self.state.geometry?.viewport;
        viewport.is_positive().then_some(viewport)
    }

    fn scene_pos_to_page_pos(&self, page_idx: usize, scene_pos: Pos2) -> Option<(f32, f32)> {
        let geometry = self.geometry_for(page_idx)?;
        let [w, h] = geometry.page_size;
        if w == 0 || h == 0 {
            return None;
        }
        let world = geometry.view.screen_to_world(scene_pos);
        // CLAMPED, as the trait requires: a press dragged off the page maps to its border rather
        // than to a page position that does not exist.
        Some((
            world.x.clamp(0.0, w as f32),
            world.y.clamp(0.0, h as f32),
        ))
    }

    /// No-op: this editor's page-pixel space is the resident `LayerStack`, which exists before any
    /// tool is routed input at all. There is nothing to allocate on demand.
    fn ensure_page_pixels(&mut self, _page_idx: usize, _scene_pos: Pos2) {}

    /// PARKS the composite request for the next `interact`.
    ///
    /// The stack is unreachable from `draw_overlay_ui`, which is where the core starts a load. The
    /// eligibility of the active layer IS checked here, from the answer the last `interact` cached:
    /// refusing before the solve costs the user nothing, while refusing after it wastes the whole
    /// membrane pass.
    ///
    /// # Errors
    /// A [`PatchHostError`] when the page is no longer resident or the active layer may not receive
    /// a patch.
    fn start_region_load(&mut self, request: &PatchRegionRequest) -> Result<(), PatchHostError> {
        let Some(geometry) = self.geometry_for(request.page_idx) else {
            return Err(PatchHostError {
                user_message: tf!("ps_editor.tools.patch_error_page_missing", page = request.page_idx + 1),
                detail: format!("page {} is not the resident page", request.page_idx),
            });
        };
        if geometry.page_size != request.source_size {
            return Err(PatchHostError {
                user_message: t!("ps_editor.tools.patch_error_page_changed").to_string(),
                detail: format!(
                    "the ROI was measured against a {:?} page, the resident page is {:?}",
                    request.source_size, geometry.page_size
                ),
            });
        }
        match self.state.target {
            Some(Ok(target)) => {
                if let Err(refusal) = target.local_origin_for(request.roi) {
                    return Err(PatchHostError {
                        user_message: refusal.user_message(),
                        detail: refusal.to_string(),
                    });
                }
            }
            Some(Err(refusal)) => {
                return Err(PatchHostError {
                    user_message: refusal.user_message(),
                    detail: refusal.to_string(),
                });
            }
            None => {
                return Err(PatchHostError {
                    user_message: PatchRefusal::NoActiveLayer.user_message(),
                    detail: "no frame has inspected the layer stack yet".to_string(),
                });
            }
        }
        // A fresh job never inherits the previous one's buffers, as the trait requires of a host
        // whose cancellation may have been parked rather than delivered.
        self.state.clear_region_load();
        self.state.drop_parked_commit(&"a new patch was started");
        self.state.status = None;
        self.state.pending_region = Some(*request);
        Ok(())
    }

    fn poll_region_load(&mut self) -> PatchRegionPoll {
        if let Some(error) = self.state.region_error.take() {
            return PatchRegionPoll::Failed(error);
        }
        match self.state.region.take() {
            Some(region) => PatchRegionPoll::Ready(region),
            None => PatchRegionPoll::Pending,
        }
    }

    fn discard_region_load(&mut self) {
        self.state.clear_region_load();
    }

    /// PARKS the solved patch for the next `interact`, which turns it into a
    /// [`PsToolAction::WriteRegion`].
    ///
    /// The store step needs the backdrop under the ROI, which only the layer stack can supply, so
    /// nothing is built here. What IS decided here is whether the write can happen at all, from the
    /// eligibility the last `interact` cached — so a patch that cannot land says so immediately
    /// rather than disappearing silently.
    ///
    /// # Errors
    /// The localized sentence of the refusal, when the page is gone or the active layer may not
    /// receive the write.
    fn commit_patch(&mut self, patch: &PatchCommit<'_>) -> Result<(), String> {
        if self.geometry_for(patch.page_idx).is_none() {
            let text = tf!(
                "ps_editor.tools.patch_error_page_missing",
                page = patch.page_idx + 1
            );
            self.state.fail(
                text.clone(),
                &format!("page {} is not the resident page", patch.page_idx),
            );
            return Err(text);
        }
        let refusal = match self.state.target {
            Some(Ok(target)) => target.local_origin_for(patch.roi).err(),
            Some(Err(refusal)) => Some(refusal),
            None => Some(PatchRefusal::NoActiveLayer),
        };
        if let Some(refusal) = refusal {
            let text = refusal.user_message();
            self.state.fail(text.clone(), &refusal);
            return Err(text);
        }
        self.state.pending_commit = Some(ParkedCommit {
            page_idx: patch.page_idx,
            roi: patch.roi,
            rgb: patch.rgb.to_vec(),
            coverage: patch.coverage.to_vec(),
        });
        Ok(())
    }
}

/// Photoshop-style patch tool for the PS editor: select, drag onto a clean source, get a
/// colour-adapted copy written into the active layer as one undo step.
///
/// A thin adapter. Every decision about the gesture, the geometry and the solve lives in
/// `PatchToolCore`; what is here is this editor's viewport, its layer stack and its commit queue.
pub struct PatchTool {
    core: PatchToolCore,
    host: HostState,
}

impl Default for PatchTool {
    fn default() -> Self {
        Self {
            core: PatchToolCore::new(LOG_TAG, OUTLINE_LAYER_ID),
            host: HostState::default(),
        }
    }
}

impl std::fmt::Debug for PatchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatchTool")
            .field("core", &self.core)
            .field("target", &self.host.target)
            .finish_non_exhaustive()
    }
}

impl PatchTool {
    /// Produces the ROI composite the core is waiting for, if one was parked.
    ///
    /// THE SOLVE PLANE: the visible layers from the bottom up to and INCLUDING the active one
    /// ([`CompositeBound::UpTo`]). That plane is what the patch both samples (destination and
    /// source alike) and writes into, and it is the plane [`backdrop_rect`] is the complement of —
    /// the store step solves each pixel so that "the active layer over everything BELOW it" shows
    /// the colour the membrane asked for, and the tab then composites everything ABOVE it on top,
    /// exactly as any other pixel of that layer. Compositing the whole stack here instead would
    /// copy the layers above the target INTO the target, and the real render would then paint them
    /// a second time over their own copy. It is also what Photoshop's Patch Tool does: it works on
    /// the active layer, not on a flattened view.
    ///
    /// Runs on the GUI thread by design — see this file's header for why that is bounded work and
    /// why `AGENTS.md` §5 is still honoured.
    fn service_pending_region(&mut self, stack: &LayerStack) {
        let Some(request) = self.host.pending_region.take() else {
            return;
        };
        if stack.page_idx() != request.page_idx || stack.size() != request.source_size {
            self.host.region_error = Some(format!(
                "the composite was asked for page {} at {:?}, the resident stack is page {} at {:?}",
                request.page_idx,
                request.source_size,
                stack.page_idx(),
                stack.size()
            ));
            return;
        }
        // Re-resolved from the resident stack rather than read from the cached answer, for the
        // reason `service_pending_commit` gives: a frame has passed since the core asked, and the
        // plane must describe the layer the write will actually land in.
        let target = match patch_target_for(stack) {
            Ok(target) => target,
            Err(refusal) => {
                self.host.region_error = Some(refusal.to_string());
                return;
            }
        };
        let layers = visible_layers_bottom_to_top(stack, CompositeBound::UpTo(target.stack_idx));
        self.host.region = Some(composite_rect(&layers, request.roi, None));
    }

    /// Turns a parked solved patch into the [`PsToolAction::WriteRegion`] the tab commits.
    ///
    /// THE store step, and the only part of this tool that is PS-editor-specific: the core stops at
    /// `(rgb, coverage)` because how those pixels are stored — over which backdrop, into which
    /// layer — is a property of this editor's layer model, not of the patch maths.
    ///
    /// The eligibility of the active layer is re-checked HERE, not trusted from the parked answer:
    /// a frame passed since `commit_patch`, and the user may have picked another layer in the
    /// layers panel in the meantime. That is also what makes the answer correct rather than merely
    /// safe — the tab's `apply_tool_region_write` writes into whatever is active when it runs, and
    /// the panel dock draws BEFORE `interact`, so re-resolving the target here is exactly what
    /// keeps `origin` and the backdrop describing the layer the write will land in.
    fn service_pending_commit(&mut self, stack: &LayerStack) {
        let Some(commit) = self.host.pending_commit.take() else {
            return;
        };
        if stack.page_idx() != commit.page_idx {
            let text = tf!(
                "ps_editor.tools.patch_error_page_missing",
                page = commit.page_idx + 1
            );
            self.host.fail(
                text,
                &format!(
                    "the patch was solved for page {}, the resident stack is page {}",
                    commit.page_idx,
                    stack.page_idx()
                ),
            );
            return;
        }
        let target = match patch_target_for(stack) {
            Ok(target) => target,
            Err(refusal) => {
                let text = refusal.user_message();
                self.host.fail(text, &refusal);
                return;
            }
        };
        let origin = match target.local_origin_for(commit.roi) {
            Ok(origin) => origin,
            Err(refusal) => {
                let text = refusal.user_message();
                self.host.fail(text, &refusal);
                return;
            }
        };
        let count = commit.roi.w.saturating_mul(commit.roi.h);
        if commit.rgb.len() != count || commit.coverage.len() != count {
            let text = t!("ps_editor.tools.patch_error_apply").to_string();
            self.host.fail(
                text,
                &format!(
                    "the solver answered {} colours and {} coverage values for a {}x{} ROI",
                    commit.rgb.len(),
                    commit.coverage.len(),
                    commit.roi.w,
                    commit.roi.h
                ),
            );
            return;
        }
        let backdrop = backdrop_rect(stack, &target, commit.roi);
        if backdrop.pixels.len() != count {
            let text = t!("ps_editor.tools.patch_error_apply").to_string();
            self.host.fail(
                text,
                &format!(
                    "the backdrop composite is {:?} with {} pixels for a {}x{} ROI",
                    backdrop.size,
                    backdrop.pixels.len(),
                    commit.roi.w,
                    commit.roi.h
                ),
            );
            return;
        }
        let (pixels, coverage) = build_region_write(&backdrop, &commit.rgb, &commit.coverage);
        self.host
            .queued
            .push(PsToolAction::WriteRegion(ToolRegionWrite {
                page_idx: commit.page_idx,
                origin,
                pixels: ColorImage::new([commit.roi.w, commit.roi.h], pixels),
                coverage: Some(coverage),
                label: t!("ps_editor.edit_op.patch").to_string(),
            }));
        self.host.status = None;
    }

    /// Drives the core's gesture from this frame's pointer state.
    ///
    /// The end is detected as a LEVEL (`!primary_down`) rather than as the release EDGE, for the
    /// reason `BrushTool` gives for its own anchor: a release delivered on a frame the tab routed
    /// to a canvas pan never reaches `interact` at all, and an edge-driven end would leave the
    /// gesture live until the next press. The core parks the end either way, and this frame's
    /// `draw_overlay_ui` judges it against the real pointer state.
    fn drive_gesture(&mut self, ctx: &PsToolContext<'_>) {
        let page_idx = ctx.page_idx;
        let scene_pos = ctx
            .pointer_image
            .map(|image| ctx.view.world_to_screen(image));
        if let Some(scene_pos) = scene_pos {
            let mut host = PsPatchHost { state: &mut self.host };
            if ctx.primary_pressed {
                self.core.stroke_begin(&mut host, page_idx, scene_pos);
            } else if ctx.primary_down {
                self.core.stroke_update(&host, page_idx, scene_pos);
            }
        }
        if !ctx.primary_down && self.core.gesture_active() {
            self.core.stroke_end();
        }
    }
}

/// Turns one solved patch into the premultiplied pixels and the write mask the tab blends in.
///
/// `backdrop`, `rgb` and `coverage` must all cover the same ROI; the caller checks that.
///
/// Each covered pixel is solved with the shared [`overlay_pixel_for_final_color`] — the same
/// function the cleaning host's store step uses — so the written layer, composited over
/// `backdrop`, shows exactly the colour the membrane asked for.
///
/// The returned coverage is BINARY: 255 where the patch claims the pixel, 0 where it does not. The
/// feather is already inside `rgb` (the core blended the solution into the destination by coverage)
/// and inside the solved pixel's alpha, so blending a second time would attenuate it twice. The
/// zero bytes are what leave the surrounding work untouched, as `PatchCommit` requires.
fn build_region_write(
    backdrop: &ColorImage,
    rgb: &[[u8; 3]],
    coverage: &[f32],
) -> (Vec<Color32>, Vec<u8>) {
    let mut pixels = vec![Color32::TRANSPARENT; rgb.len()];
    let mut mask = vec![0_u8; rgb.len()];
    let per_pixel = pixels
        .iter_mut()
        .zip(mask.iter_mut())
        .zip(backdrop.pixels.iter())
        .zip(rgb.iter())
        .zip(coverage.iter());
    for ((((pixel, mask), base), final_rgb), coverage) in per_pixel {
        if *coverage <= 0.0 {
            continue;
        }
        *pixel = overlay_pixel_for_final_color(
            *base,
            Color32::from_rgb(final_rgb[0], final_rgb[1], final_rgb[2]),
            *coverage,
        );
        *mask = u8::MAX;
    }
    (pixels, mask)
}

impl PsTool for PatchTool {
    fn id(&self) -> PsToolId {
        PsToolId::Patch
    }

    fn title(&self) -> &'static str {
        t!("ps_editor.tools.patch_title")
    }

    /// The pixel half of the host, plus the gesture.
    ///
    /// The order matters: the geometry and the eligibility are refreshed FIRST, so the two parked
    /// requests are serviced against this frame's stack, and the gesture runs LAST, so a press that
    /// starts a new job sees a host that is no longer holding the previous one's buffers.
    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        self.host.geometry = Some(PageGeometry {
            view: ctx.view,
            viewport: ctx.view.viewport_rect,
            page_size: ctx.page_size,
            page_idx: ctx.page_idx,
        });
        self.host.target = Some(patch_target_for(ctx.stack));
        self.service_pending_region(ctx.stack);
        self.service_pending_commit(ctx.stack);
        self.drive_gesture(ctx);
        // Pixels are never written here: this tool's whole output is the queued `WriteRegion`, and
        // the tab reports its dirty rect itself when it applies it.
        ToolOutcome::default()
    }

    /// Nothing: the selection outline is painted by the core from [`PsTool::draw_overlay_ui`].
    ///
    /// It has to be. This hook's painter is pointer-gated by the tab — it stops following an
    /// occluded pointer — and a selection that lives on the canvas for a whole session would blink
    /// out whenever the pointer crossed a panel.
    fn draw_overlay(
        &self,
        _painter: &egui::Painter,
        _view: &ViewTransform,
        _pointer_image: Option<Pos2>,
    ) {
    }

    fn has_options(&self) -> bool {
        true
    }

    /// The core's own controls, plus the one thing only the host knows: why the active layer
    /// refuses the write.
    ///
    /// The refusal line is a STATUS, not a hint — it changes with the layer selection and is the
    /// only place the user can find out why a drag did nothing — so it belongs here rather than in
    /// `hotkey_rows`.
    fn options_ui(&mut self, ui: &mut egui::Ui) {
        self.core.draw_ui(ui);
        if let Some(Err(refusal)) = self.host.target {
            ui.colored_label(ui.visuals().error_fg_color, refusal.user_message());
        }
        if let Some(status) = self.host.status.as_ref() {
            ui.small(status.clone());
        }
    }

    fn hotkey_rows(&self) -> Vec<PsHotkeyRow> {
        vec![
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.patch.draw_label"),
                t!("ps_editor.tools.hotkey.patch.draw_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.patch.source_label"),
                t!("ps_editor.tools.hotkey.patch.source_keys"),
            ),
            PsHotkeyRow::new(
                t!("ps_editor.tools.hotkey.patch.clear_label"),
                t!("ps_editor.tools.hotkey.patch.clear_keys"),
            ),
        ]
    }

    /// Only a LIVE gesture counts, never a running solve.
    ///
    /// A press the core refused (it was busy, or the page was not laid out) leaves no gesture, so
    /// this cannot report one — which is the rule this method exists to enforce. A parked gesture
    /// end is still in flight: the frame that judges it has not run yet.
    fn gesture_in_flight(&self) -> bool {
        self.core.gesture_in_flight()
    }

    /// Abandons the job in flight and everything parked for it; the selection survives.
    ///
    /// Called on a tool switch, a page switch and a suppressed-frame Esc. The core's `deactivate`
    /// is the matching hook: a job left running would finish unpolled — `draw_overlay_ui` runs for
    /// the ACTIVE tool only — and would land on a page the user has meanwhile changed.
    fn reset(&mut self) {
        let mut host = PsPatchHost { state: &mut self.host };
        self.core.deactivate(&mut host);
        self.host.clear_region_load();
        self.host.status = None;
        self.host
            .drop_parked_commit(&"the tool, the page or the gesture was abandoned");
    }

    /// Asks for the next frame while anything is outstanding.
    ///
    /// This tool owns no channel of its own — the solve worker is the core's, polled from
    /// `draw_overlay_ui` — but the two parked halves of the host DO need a frame each to be
    /// serviced, and a job whose gesture has ended produces no pointer events to trigger one.
    fn poll_workers(&mut self) -> bool {
        self.core.busy()
            || self.host.pending_region.is_some()
            || self.host.region.is_some()
            || self.host.pending_commit.is_some()
            || !self.host.queued.is_empty()
    }

    fn take_actions(&mut self) -> Vec<PsToolAction> {
        std::mem::take(&mut self.host.queued)
    }

    /// The core's whole frame pass: resolve the parked gesture end, drive the job, paint.
    ///
    /// Escape is handled here rather than from `PsToolContext::cancel_pressed` because the core's
    /// `on_escape` reads the key from the `egui::Context`, which only this hook is handed.
    fn draw_overlay_ui(&mut self, ctx: &egui::Context, cx: PsToolOverlayCx) {
        self.host.geometry = Some(PageGeometry {
            view: cx.view,
            viewport: cx.viewport,
            page_size: cx.page_size,
            page_idx: cx.page_idx,
        });
        if self.core.on_escape(ctx) {
            self.host.status = None;
        }
        // Checked as `!primary_down || primary_released`, so a release followed by a fresh press
        // inside one frame still counts as the release it was.
        let released =
            ctx.input(|input| !input.pointer.primary_down() || input.pointer.primary_released());
        let mut host = PsPatchHost { state: &mut self.host };
        self.core.draw_overlay_ui(ctx, &mut host, released);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::super::layers::LayerTransform;
    use egui::Vec2;

    /// A stack with a page-sized `Исходник` / `Клин` pair, both filled with the given colours.
    fn stack_with(size: [usize; 2], source: Color32, clean: Color32) -> LayerStack {
        LayerStack::new(
            3,
            size,
            ColorImage::filled(size, source),
            ColorImage::filled(size, clean),
        )
    }

    fn roi(x: usize, y: usize, w: usize, h: usize) -> OverlayRectPx {
        OverlayRectPx { x, y, w, h }
    }

    /// Every visible layer of `stack`: the bound that includes its topmost layer.
    ///
    /// Production code never composites a page rectangle that way — a patch is solved up to its
    /// TARGET layer and stored below it — so this lives here rather than as a variant of
    /// `CompositeBound`.
    fn whole_stack(stack: &LayerStack) -> CompositeBound {
        CompositeBound::UpTo(stack.layers().len().saturating_sub(1))
    }

    // ----------------------------------------------------------------------------------------
    // `composite_rect` — the shared ROI composite.
    // ----------------------------------------------------------------------------------------

    /// The composite walks BOTTOM to TOP: an opaque upper layer hides the lower one, and the
    /// result is cropped to the ROI, not to the page.
    #[test]
    fn the_roi_composite_is_ordered_and_cropped() {
        let stack = stack_with([8, 8], Color32::RED, Color32::TRANSPARENT);
        let layers = visible_layers_bottom_to_top(&stack, whole_stack(&stack));
        let image = composite_rect(&layers, roi(2, 3, 4, 2), None);
        assert_eq!(image.size, [4, 2], "the composite is cropped to the ROI");
        assert!(
            image.pixels.iter().all(|px| *px == Color32::RED),
            "a transparent Клин leaves Исходник showing"
        );

        // Now make `Клин` opaque: it sits ABOVE `Исходник`, so it must win everywhere.
        let stack = stack_with([8, 8], Color32::RED, Color32::BLUE);
        let layers = visible_layers_bottom_to_top(&stack, whole_stack(&stack));
        let image = composite_rect(&layers, roi(0, 0, 3, 3), None);
        assert!(
            image.pixels.iter().all(|px| *px == Color32::BLUE),
            "the upper layer must be composited over the lower one"
        );
    }

    /// Layer opacity is applied BEFORE the src-over, exactly as it is at composite time.
    #[test]
    fn the_roi_composite_applies_layer_opacity() {
        let mut stack = stack_with([4, 4], Color32::BLACK, Color32::WHITE);
        let clean_id = stack.layers()[1].id;
        stack
            .layer_mut(clean_id)
            .expect("the Клин layer exists")
            .opacity = 0.5;
        let layers = visible_layers_bottom_to_top(&stack, whole_stack(&stack));
        let image = composite_rect(&layers, roi(0, 0, 2, 2), None);
        let px = image.pixels[0];
        assert_eq!(px.a(), 255, "the opaque Исходник keeps the composite opaque");
        assert!(
            (100..=160).contains(&px.r()),
            "a half-opacity white over black must land near mid-grey, got {px:?}"
        );
    }

    /// A rectangle that leaves the page samples transparent there instead of panicking or
    /// clamping onto the page's border pixels.
    #[test]
    fn the_roi_composite_tolerates_a_rect_outside_the_page() {
        let stack = stack_with([4, 4], Color32::RED, Color32::TRANSPARENT);
        let layers = visible_layers_bottom_to_top(&stack, whole_stack(&stack));
        let image = composite_rect(&layers, roi(3, 3, 3, 3), None);
        assert_eq!(image.size, [3, 3]);
        assert_eq!(image.pixels[0], Color32::RED, "the one in-page pixel");
        assert_eq!(
            image.pixels[8],
            Color32::TRANSPARENT,
            "a pixel past the page edge must stay transparent"
        );
    }

    /// A hidden layer contributes nothing, and the two bounded forms are exactly one layer apart:
    /// `Below` stops before the index, `UpTo` includes it. Confusing them is the defect the named
    /// bound exists to prevent, so both are pinned here against the same stack.
    #[test]
    fn the_ordered_walk_honours_visibility_and_both_bounds() {
        let mut stack = stack_with([4, 4], Color32::RED, Color32::BLUE);
        assert_eq!(
            visible_layers_bottom_to_top(&stack, whole_stack(&stack)).len(),
            2
        );
        assert_eq!(
            visible_layers_bottom_to_top(&stack, CompositeBound::Below(1)).len(),
            1,
            "below the Клин layer there is only Исходник"
        );
        assert_eq!(
            visible_layers_bottom_to_top(&stack, CompositeBound::UpTo(1)).len(),
            2,
            "up to the Клин layer means Исходник AND Клин"
        );
        assert!(
            visible_layers_bottom_to_top(&stack, CompositeBound::Below(0)).is_empty(),
            "nothing sits below the bottom layer"
        );
        assert_eq!(
            visible_layers_bottom_to_top(&stack, CompositeBound::UpTo(0)).len(),
            1,
            "up to the bottom layer is the bottom layer itself"
        );
        let source_id = stack.layers()[0].id;
        stack
            .layer_mut(source_id)
            .expect("the Исходник layer exists")
            .visible = false;
        assert_eq!(
            visible_layers_bottom_to_top(&stack, whole_stack(&stack)).len(),
            1,
            "a hidden layer is not composited"
        );
        assert!(
            visible_layers_bottom_to_top(&stack, CompositeBound::UpTo(0)).is_empty(),
            "an inclusive bound does not resurrect a hidden layer"
        );
    }

    /// `UpTo` and `Below` differ by exactly the layer at the index, on a stack deep enough that an
    /// off-by-one in either direction changes the answer.
    #[test]
    fn the_inclusive_and_exclusive_bounds_differ_by_exactly_one_layer() {
        let mut stack = stack_with([4, 4], Color32::RED, Color32::BLUE);
        for _ in 0..2 {
            stack.add_raster_layer_image(
                "raster".to_string(),
                ColorImage::filled([4, 4], Color32::GREEN),
                LayerTransform::identity_for([4, 4]),
            );
        }
        assert_eq!(stack.layers().len(), 4, "Исходник, Клин and two rasters");
        for idx in 0..4 {
            let below = visible_layers_bottom_to_top(&stack, CompositeBound::Below(idx));
            let up_to = visible_layers_bottom_to_top(&stack, CompositeBound::UpTo(idx));
            assert_eq!(below.len(), idx, "Below(idx) is the idx layers under it");
            assert_eq!(up_to.len(), idx + 1, "UpTo(idx) adds the layer itself");
            assert_eq!(
                up_to[idx].0.id,
                stack.layers()[idx].id,
                "the last layer of the inclusive walk IS the one at the index"
            );
        }
        assert_eq!(
            visible_layers_bottom_to_top(&stack, CompositeBound::UpTo(3)).len(),
            4,
            "including the topmost layer is the whole visible stack"
        );
    }

    // ----------------------------------------------------------------------------------------
    // The refusal predicate.
    // ----------------------------------------------------------------------------------------

    /// The DEFAULT active layer of a freshly loaded page is `Клин`, which must pass every rule —
    /// this is the case the whole tool exists for.
    #[test]
    fn the_clean_base_layer_is_a_valid_patch_target() {
        let stack = stack_with([16, 16], Color32::WHITE, Color32::TRANSPARENT);
        let target = patch_target_for(&stack).expect("Клин is a valid patch target");
        assert_eq!(target.kind, LayerKind::Clean);
        assert_eq!(target.stack_idx, 1);
        assert_eq!(target.origin, [0, 0]);
        assert_eq!(target.size, [16, 16]);
        assert_eq!(
            target.local_origin_for(roi(4, 5, 2, 2)),
            Ok([4, 5]),
            "a page-aligned layer maps page pixels to its own one for one"
        );
    }

    /// `Исходник` is structurally read-only, and is refused with its own reason.
    #[test]
    fn the_source_layer_is_refused() {
        let mut stack = stack_with([8, 8], Color32::WHITE, Color32::TRANSPARENT);
        let source_id = stack.layers()[0].id;
        stack.set_active(source_id);
        assert_eq!(patch_target_for(&stack), Err(PatchRefusal::SourceLayer));
    }

    /// A raster still showing a non-destructive effects chain must be baked first.
    #[test]
    fn a_raster_with_an_unbaked_effects_chain_is_refused() {
        let mut stack = stack_with([8, 8], Color32::WHITE, Color32::TRANSPARENT);
        let raster = stack.add_raster_layer();
        stack.set_active(raster);
        assert!(patch_target_for(&stack).is_ok(), "a plain raster is accepted");
        stack
            .layer_mut(raster)
            .expect("the raster exists")
            .effects
            .push(serde_json::json!({ "kind": "stroke" }));
        assert_eq!(patch_target_for(&stack), Err(PatchRefusal::EffectsChain));
    }

    /// A deformed raster is rendered through its mesh, so its affine transform no longer says
    /// where its pixels land — the same reason `TransformTool` refuses one.
    #[test]
    fn a_deformed_raster_is_refused() {
        let mut stack = stack_with([8, 8], Color32::WHITE, Color32::TRANSPARENT);
        let raster = stack.add_raster_layer();
        stack.set_active(raster);
        let grid = stack
            .layer(raster)
            .expect("the raster exists")
            .identity_deform_grid(2, 2);
        stack.layer_mut(raster).expect("the raster exists").deform = Some(grid);
        assert_eq!(patch_target_for(&stack), Err(PatchRefusal::Deformed));
    }

    /// A rotated or scaled raster is refused; a whole-pixel TRANSLATION is not, because the two
    /// pixel grids still coincide.
    #[test]
    fn rotation_and_scale_are_refused_but_a_whole_pixel_offset_is_not() {
        let mut stack = stack_with([32, 32], Color32::WHITE, Color32::TRANSPARENT);
        let raster = stack.add_raster_layer_image(
            "clip".to_string(),
            ColorImage::filled([8, 8], Color32::TRANSPARENT),
            LayerTransform {
                center: Vec2::new(12.0, 16.0),
                rotation: 0.0,
                scale: 1.0,
            },
        );
        stack.set_active(raster);
        let target = patch_target_for(&stack).expect("a translated clip layer is patchable");
        assert_eq!(target.origin, [8, 12], "the clip layer's top-left page pixel");
        assert_eq!(target.local_origin_for(roi(10, 14, 4, 4)), Ok([2, 2]));

        stack
            .layer_mut(raster)
            .expect("the raster exists")
            .transform
            .rotation = 0.3;
        assert!(matches!(
            patch_target_for(&stack),
            Err(PatchRefusal::Transformed { .. })
        ));
        let layer = stack.layer_mut(raster).expect("the raster exists");
        layer.transform.rotation = 0.0;
        layer.transform.scale = 2.0;
        assert!(matches!(
            patch_target_for(&stack),
            Err(PatchRefusal::Transformed { .. })
        ));
        // A HALF-pixel placement puts the two grids out of phase, which is the same defect.
        let layer = stack.layer_mut(raster).expect("the raster exists");
        layer.transform.scale = 1.0;
        layer.transform.center = Vec2::new(12.5, 16.0);
        assert!(matches!(
            patch_target_for(&stack),
            Err(PatchRefusal::Transformed { .. })
        ));
    }

    /// An ROI that leaves an "incomplete" layer is REFUSED, never clipped: a clipped commit would
    /// drop part of the patch with nothing said (`AGENTS.md` §6).
    #[test]
    fn an_roi_outside_the_layer_footprint_is_refused() {
        let mut stack = stack_with([32, 32], Color32::WHITE, Color32::TRANSPARENT);
        let raster = stack.add_raster_layer_image(
            "clip".to_string(),
            ColorImage::filled([8, 8], Color32::TRANSPARENT),
            LayerTransform {
                center: Vec2::new(12.0, 16.0),
                rotation: 0.0,
                scale: 1.0,
            },
        );
        stack.set_active(raster);
        let target = patch_target_for(&stack).expect("the clip layer is patchable");
        assert!(target.local_origin_for(roi(8, 12, 8, 8)).is_ok(), "exactly the footprint");
        assert!(matches!(
            target.local_origin_for(roi(7, 12, 8, 8)),
            Err(PatchRefusal::RoiOutsideLayer { .. })
        ));
        assert!(matches!(
            target.local_origin_for(roi(8, 12, 9, 8)),
            Err(PatchRefusal::RoiOutsideLayer { .. })
        ));
    }

    /// A hidden active layer is REFUSED: the solve plane is the plane the target layer is part of,
    /// and a target the composite drops would be solved against pixels that do not contain it and
    /// written where the user cannot see it. A zero effective opacity is the same case, because it
    /// is the same predicate the composite filters on.
    #[test]
    fn an_invisible_active_layer_is_refused() {
        let mut stack = stack_with([8, 8], Color32::WHITE, Color32::TRANSPARENT);
        let clean_id = stack.layers()[1].id;
        assert!(patch_target_for(&stack).is_ok(), "a visible Клин is a valid target");

        stack
            .layer_mut(clean_id)
            .expect("the Клин layer exists")
            .visible = false;
        assert_eq!(patch_target_for(&stack), Err(PatchRefusal::InvisibleLayer));

        let layer = stack.layer_mut(clean_id).expect("the Клин layer exists");
        layer.visible = true;
        layer.opacity = 0.0;
        assert_eq!(
            patch_target_for(&stack),
            Err(PatchRefusal::InvisibleLayer),
            "a layer at zero opacity is absent from the composite exactly as a hidden one is"
        );
    }

    // ----------------------------------------------------------------------------------------
    // The solve plane.
    // ----------------------------------------------------------------------------------------

    /// THE defect this plane exists to prevent: a visible layer ABOVE the active one must NOT be
    /// composited into the region handed to the solver. It is painted over the result by the real
    /// render, so including it here would apply it twice on screen.
    #[test]
    fn the_solve_plane_excludes_everything_above_the_active_layer() {
        let mut stack = stack_with([8, 8], Color32::RED, Color32::BLUE);
        let clean_id = stack.layers()[1].id;
        stack.add_raster_layer_image(
            "upper".to_string(),
            ColorImage::filled([8, 8], Color32::WHITE),
            LayerTransform::identity_for([8, 8]),
        );
        // `add_raster_layer_image` activates what it adds; the patch targets Клин here.
        stack.set_active(clean_id);

        let mut tool = PatchTool::default();
        tool.host.pending_region = Some(PatchRegionRequest {
            page_idx: stack.page_idx(),
            roi: roi(0, 0, 2, 2),
            source_size: [8, 8],
        });
        tool.service_pending_region(&stack);
        let mut host = PsPatchHost { state: &mut tool.host };
        match host.poll_region_load() {
            PatchRegionPoll::Ready(image) => assert!(
                image.pixels.iter().all(|px| *px == Color32::BLUE),
                "the plane must stop at the active Клин layer, not include the opaque layer above it"
            ),
            other => panic!("the composite must be ready after one service pass, got {other:?}"),
        }
    }

    /// The solve plane and the storage backdrop are complementary: the plane is everything up to
    /// and including the target, the backdrop everything strictly below it. Together they cover the
    /// stack under the target exactly once, which is what makes the store step's colour solve
    /// (`overlay_pixel_for_final_color`) describe the pixel the user will see.
    #[test]
    fn the_solve_plane_and_the_backdrop_meet_at_the_active_layer() {
        let mut stack = stack_with([8, 8], Color32::RED, Color32::TRANSPARENT);
        let middle = stack.add_raster_layer_image(
            "middle".to_string(),
            ColorImage::filled([8, 8], Color32::GREEN),
            LayerTransform::identity_for([8, 8]),
        );
        stack.add_raster_layer_image(
            "upper".to_string(),
            ColorImage::filled([8, 8], Color32::WHITE),
            LayerTransform::identity_for([8, 8]),
        );
        stack.set_active(middle);
        let target = patch_target_for(&stack).expect("a plain raster is patchable");

        let plane = visible_layers_bottom_to_top(&stack, CompositeBound::UpTo(target.stack_idx));
        let backdrop = visible_layers_bottom_to_top(&stack, CompositeBound::Below(target.stack_idx));
        assert_eq!(
            plane.len(),
            backdrop.len() + 1,
            "the plane is the backdrop plus the target layer itself"
        );
        assert_eq!(
            plane[plane.len() - 1].0.id,
            target.layer_id,
            "the topmost layer of the solve plane IS the layer the patch is written into"
        );
        assert!(
            backdrop.iter().all(|(layer, _)| layer.id != target.layer_id),
            "the backdrop never contains the target itself"
        );
    }

    /// The default case — `Клин` active, no user rasters — must be UNCHANGED by the plane rule:
    /// the whole visible stack is already "up to and including the active layer" there.
    #[test]
    fn the_default_page_solve_plane_is_the_whole_visible_stack() {
        let stack = stack_with([8, 8], Color32::RED, Color32::TRANSPARENT);
        let target = patch_target_for(&stack).expect("Клин is the default active layer");
        let patch_roi = roi(1, 1, 3, 3);
        let expected = composite_rect(
            &visible_layers_bottom_to_top(&stack, whole_stack(&stack)),
            patch_roi,
            None,
        );

        let mut tool = PatchTool::default();
        tool.host.pending_region = Some(PatchRegionRequest {
            page_idx: stack.page_idx(),
            roi: patch_roi,
            source_size: [8, 8],
        });
        tool.service_pending_region(&stack);
        assert_eq!(target.stack_idx, 1, "Клин is the topmost layer of a fresh page");
        let region = tool.host.region.as_ref().expect("the composite was produced");
        assert_eq!(region.size, expected.size);
        assert_eq!(
            region.pixels, expected.pixels,
            "with nothing above Клин the plane is the whole visible stack, as it always was"
        );
    }

    // ----------------------------------------------------------------------------------------
    // The backdrop.
    // ----------------------------------------------------------------------------------------

    /// For `Клин` the backdrop is `Исходник` and nothing else — not the visible composite, which
    /// would include `Клин` itself, and not an empty image when `Исходник` is hidden, because the
    /// clean overlay's storage contract does not depend on a view toggle.
    #[test]
    fn the_clean_layer_backdrop_is_the_source_layer() {
        let mut stack = stack_with([8, 8], Color32::RED, Color32::BLUE);
        let target = patch_target_for(&stack).expect("Клин is the default active layer");
        let backdrop = backdrop_rect(&stack, &target, roi(1, 1, 3, 3));
        assert_eq!(backdrop.size, [3, 3]);
        assert!(
            backdrop.pixels.iter().all(|px| *px == Color32::RED),
            "the Клин backdrop must be Исходник, never the composite that includes Клин"
        );

        let source_id = stack.layers()[0].id;
        stack
            .layer_mut(source_id)
            .expect("the Исходник layer exists")
            .visible = false;
        let backdrop = backdrop_rect(&stack, &target, roi(1, 1, 3, 3));
        assert!(
            backdrop.pixels.iter().all(|px| *px == Color32::RED),
            "hiding Исходник is a view toggle; the storage backdrop is unchanged"
        );
    }

    /// For a raster the backdrop is everything visible strictly BELOW it — never the layer itself,
    /// and never a layer above it.
    #[test]
    fn a_raster_backdrop_excludes_itself_and_everything_above() {
        let mut stack = stack_with([8, 8], Color32::RED, Color32::TRANSPARENT);
        let lower = stack.add_raster_layer_image(
            "lower".to_string(),
            ColorImage::filled([8, 8], Color32::GREEN),
            LayerTransform::identity_for([8, 8]),
        );
        let middle = stack.add_raster_layer_image(
            "middle".to_string(),
            ColorImage::filled([8, 8], Color32::BLUE),
            LayerTransform::identity_for([8, 8]),
        );
        let upper = stack.add_raster_layer_image(
            "upper".to_string(),
            ColorImage::filled([8, 8], Color32::WHITE),
            LayerTransform::identity_for([8, 8]),
        );
        assert!(lower < middle && middle < upper, "the fixture stacks bottom to top");
        stack.set_active(middle);
        let target = patch_target_for(&stack).expect("a plain raster is patchable");
        let backdrop = backdrop_rect(&stack, &target, roi(0, 0, 2, 2));
        assert!(
            backdrop.pixels.iter().all(|px| *px == Color32::GREEN),
            "the backdrop is the layer directly below, not the target and not the layer above"
        );
    }

    // ----------------------------------------------------------------------------------------
    // The store step and the tab hand-off.
    // ----------------------------------------------------------------------------------------

    /// The store step writes only what the patch covers, solved against the backdrop under it.
    #[test]
    fn the_store_step_writes_only_the_covered_pixels() {
        let backdrop = ColorImage::filled([4, 4], Color32::WHITE);
        let rgb = vec![[10, 20, 30]; 16];
        let mut coverage = vec![0.0_f32; 16];
        coverage[5] = 1.0;
        let (pixels, mask) = build_region_write(&backdrop, &rgb, &coverage);
        assert_eq!(pixels.len(), 16);
        assert_eq!(mask.len(), 16);
        assert_eq!(mask[0], 0, "an uncovered pixel must not be written at all");
        assert_eq!(pixels[0], Color32::TRANSPARENT);
        assert_eq!(mask[5], 255, "a covered pixel replaces the destination");
        assert_eq!(
            pixels[5],
            overlay_pixel_for_final_color(Color32::WHITE, Color32::from_rgb(10, 20, 30), 1.0),
            "a covered pixel is solved against the backdrop under it"
        );
    }

    /// One finished patch raises exactly ONE `WriteRegion`, in LAYER-local geometry, stamped with
    /// the page it was solved for — and `take_actions` DRAINS.
    #[test]
    fn a_finished_patch_raises_one_layer_local_write_region() {
        let _guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

        let mut stack = stack_with([16, 16], Color32::WHITE, Color32::TRANSPARENT);
        let raster = stack.add_raster_layer_image(
            "clip".to_string(),
            ColorImage::filled([8, 8], Color32::TRANSPARENT),
            LayerTransform {
                center: Vec2::new(8.0, 8.0),
                rotation: 0.0,
                scale: 1.0,
            },
        );
        stack.set_active(raster);

        let mut tool = PatchTool::default();
        let patch_roi = roi(6, 6, 4, 4);
        tool.host.pending_commit = Some(ParkedCommit {
            page_idx: stack.page_idx(),
            roi: patch_roi,
            rgb: vec![[0, 0, 0]; 16],
            coverage: vec![1.0; 16],
        });
        tool.service_pending_commit(&stack);

        let actions = tool.take_actions();
        assert_eq!(actions.len(), 1, "one patch is one commit");
        let PsToolAction::WriteRegion(write) = &actions[0];
        assert_eq!(write.page_idx, stack.page_idx());
        assert_eq!(
            write.origin,
            [2, 2],
            "the origin is LAYER-local: the ROI at page 6;6 inside a clip layer placed at 4;4"
        );
        assert_eq!(write.pixels.size, [4, 4]);
        assert_eq!(write.coverage.as_ref().map(Vec::len), Some(16));
        assert!(!write.label.trim().is_empty(), "the undo label is localized text");
        assert!(
            tool.take_actions().is_empty(),
            "take_actions must DRAIN: a commit handed over twice would be applied twice"
        );
    }

    /// A patch solved for a page that is no longer resident is DROPPED, not landed on the new
    /// page's pixels, and the user is told why.
    #[test]
    fn a_commit_for_a_departed_page_is_dropped() {
        let _guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

        let stack = stack_with([8, 8], Color32::WHITE, Color32::TRANSPARENT);
        let mut tool = PatchTool::default();
        tool.host.pending_commit = Some(ParkedCommit {
            page_idx: stack.page_idx() + 1,
            roi: roi(0, 0, 2, 2),
            rgb: vec![[0, 0, 0]; 4],
            coverage: vec![1.0; 4],
        });
        tool.service_pending_commit(&stack);
        assert!(tool.take_actions().is_empty(), "the commit must not land");
        assert!(
            tool.host.status.is_some(),
            "a dropped commit must be reported, never silent"
        );
    }

    /// The parked region request is serviced against the resident stack, and a request that names
    /// another page fails the load instead of compositing the wrong pixels.
    #[test]
    fn the_parked_region_request_is_serviced_against_the_resident_stack() {
        let stack = stack_with([8, 8], Color32::RED, Color32::TRANSPARENT);
        let mut tool = PatchTool::default();
        tool.host.pending_region = Some(PatchRegionRequest {
            page_idx: stack.page_idx(),
            roi: roi(1, 1, 2, 2),
            source_size: [8, 8],
        });
        tool.service_pending_region(&stack);
        let mut host = PsPatchHost { state: &mut tool.host };
        match host.poll_region_load() {
            PatchRegionPoll::Ready(image) => {
                assert_eq!(image.size, [2, 2]);
                assert!(image.pixels.iter().all(|px| *px == Color32::RED));
            }
            other => panic!("the composite must be ready after one service pass, got {other:?}"),
        }

        tool.host.pending_region = Some(PatchRegionRequest {
            page_idx: stack.page_idx() + 1,
            roi: roi(1, 1, 2, 2),
            source_size: [8, 8],
        });
        tool.service_pending_region(&stack);
        let mut host = PsPatchHost { state: &mut tool.host };
        assert!(
            matches!(host.poll_region_load(), PatchRegionPoll::Failed(_)),
            "a request for another page must fail the load, not composite this page"
        );
    }

    /// A commit the host has ACCEPTED survives `discard_region_load`.
    ///
    /// The core calls that hook immediately after every `commit_patch` it accepted (`poll_solve`),
    /// so a host that swept its parked commit up with the job's scratch buffers would drop every
    /// patch it ever accepted — the write would simply never be queued.
    #[test]
    fn an_accepted_commit_survives_the_discard_that_ends_the_job() {
        let mut tool = PatchTool::default();
        tool.host.pending_region = Some(PatchRegionRequest {
            page_idx: 3,
            roi: roi(0, 0, 2, 2),
            source_size: [8, 8],
        });
        tool.host.pending_commit = Some(ParkedCommit {
            page_idx: 3,
            roi: roi(0, 0, 2, 2),
            rgb: vec![[1, 2, 3]; 4],
            coverage: vec![1.0; 4],
        });
        let mut host = PsPatchHost { state: &mut tool.host };
        host.discard_region_load();
        assert!(
            tool.host.pending_region.is_none(),
            "the region load's own state is dropped"
        );
        assert!(
            tool.host.pending_commit.is_some(),
            "an accepted commit is the user's finished work and must reach take_actions"
        );
    }

    /// Abandoning the tool in the ONE frame between `commit_patch` and the servicing `interact`
    /// drops the patch — but says so, in the log and in the panel.
    #[test]
    fn an_abandoned_parked_commit_is_reported() {
        let _guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

        let mut tool = PatchTool::default();
        tool.host.pending_commit = Some(ParkedCommit {
            page_idx: 3,
            roi: roi(0, 0, 2, 2),
            rgb: vec![[1, 2, 3]; 4],
            coverage: vec![1.0; 4],
        });
        tool.reset();
        assert!(tool.host.pending_commit.is_none());
        assert!(tool.take_actions().is_empty());
        assert!(
            tool.host.status.is_some(),
            "a lost patch must be reported, never silent"
        );
    }

    /// A fresh tool claims none of the region hooks and asks for no frames: the patch tool is only
    /// busy while a job is actually in flight.
    #[test]
    fn an_idle_patch_tool_is_inert() {
        let mut tool = PatchTool::default();
        assert!(!tool.poll_workers());
        assert!(tool.take_actions().is_empty());
        assert!(!tool.wants_main_panel());
        assert!(!tool.captures_canvas_pointer(Pos2::new(10.0, 10.0)));
        assert!(!tool.gesture_in_flight());
        assert!(tool.has_options(), "the tool owns the core's shape/blend/feather controls");
    }
}
