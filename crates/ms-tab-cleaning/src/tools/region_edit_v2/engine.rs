/*
File: region_edit_v2/engine.rs

Purpose:
The contract between the region-editing HOST (`host.rs`) and the AI engines it hosts. The host
owns the on-canvas `RegionFrame` — the rectangle, the mask stack, the pending result, the
lock and Применить/Отменить; an engine owns everything model-specific — its parameters, its
settings file, its wire protocol, its worker thread and its own progress bar. Also the ONE
owner of the user-facing sentence for a `SizeViolation`, shared by the host's panel and the
engines' run-path re-check.

Key structures:
- `AiEngine`: the trait every engine implements
- `EngineSection`: which picker section («Без промпта» / «С промптом») an engine appears in
- `MaskLayerSpec`: one mask layer an engine wants painted (re-exported from `layers`)
- `EngineRunRequest`: everything the host hands an engine to start one run
- `MarksMode`, `MarksSupport`, `RunMarks`: how the user's marks layer may reach an engine, what
  an engine accepts, and what one run carries
- `EnginePoll`: what one `poll` says about the run in flight
- `RunOptionsCtx`: the host facts an engine's pinned run options may depend on

Key functions:
- `composite_marks_over()`: the straight-alpha "over" of the marks layer onto a region (pure)
- `violation_text()`: the localized sentence for one `SizeViolation`
- `region_size_refusal()`: the run-path re-check of an engine's `FrameConstraints`

Notes:
An engine never sees `CanvasView`, `ProjectData`, the frame or an `egui::Context` outside
`poll`; its whole UI surface is a plain `&mut egui::Ui`. There is deliberately NO shared
progress vocabulary (`dev-docs/region_edit_v2_plan.md` §13.2 D13): engines disagree about
what progress even is, so each draws its own bar and the host learns only Running / Done /
Failed. The host decides only WHERE it is drawn: the panel has three sections —
`draw_progress` pinned at the top, `draw_parameters` in the scroll, and the host's actions
followed by the engine's `draw_run_options` pinned at the bottom (§13.3 amendment). A run's answer is PIXELS and only pixels (D12).
The marks layer is the user's colour annotation over the region (arrows, outlines); the HOST
decides how it travels — composited into the region, as a separate reference image or as a
transparent layer — from the engine's `marks_support()`, so an engine only ever reads
`EngineRunRequest::marks` and never sees the frame's buffers.
*/

use super::geometry::{FrameConstraints, SizeViolation, check_size};
use ms_canvas::OverlayRectPx;
use eframe::egui;

/// Which picker section an engine appears in.
///
/// The sections are drawn in the order of this enum and a section with no engines is not
/// drawn at all. An engine belongs to exactly one section; an engine whose MODES straddle
/// both (FLUX.1 Fill) picks the section of its default mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineSection {
    /// Engines that need no text prompt at all.
    WithoutPrompt,
    /// Engines whose run is driven by a prompt.
    WithPrompt,
}

/// One mask layer an engine wants the user to paint.
///
/// Re-exported from the framework rather than redeclared: the host builds the frame's
/// `MaskStack` straight from what `mask_layers()` returns, so a second, engine-side twin of
/// the type could only ever drift from the one the frame consumes. Layers are declared in
/// painting order — a later layer wins where two overlap, in the preview and in the run
/// request alike — and the layer count is the LENGTH of `mask_layers()` and nothing else.
pub use super::layers::MaskLayerSpec;

/// Everything the host hands an engine to start one run.
///
/// The host guarantees the SIZES: `region` is exactly `rect_px.w * rect_px.h` pixels, and
/// `masks` holds exactly one L8 buffer per declared mask layer, each of exactly
/// `rect_px.w * rect_px.h` bytes. An engine still validates them — a request that fails the
/// check is refused by `AiEngine::start` rather than encoded onto the wire.
#[derive(Debug, Clone)]
pub struct EngineRunRequest {
    /// Index of the page the frame sits on, for logs and for a result the host must place
    /// back on the page it came from.
    pub page_idx: usize,
    /// The frame rectangle in SOURCE PAGE PIXELS — the unit
    /// `CanvasView::replace_overlay_region_px` consumes.
    pub rect_px: OverlayRectPx,
    /// Source page crop composited with the current clean overlay, exactly `rect_px` in
    /// size. Never the bare overlay chunk: a page with no overlay would otherwise hand the
    /// model pure transparency (`dev-docs/region_edit_v2_plan.md` §13.2 D14).
    pub region: egui::ColorImage,
    /// One L8 buffer per declared mask layer, in `mask_layers()` order, each
    /// `rect_px.w * rect_px.h` bytes and holding only `0` or `255`.
    pub masks: Vec<Vec<u8>>,
    /// The user's marks, in the form the host chose from `marks_support()`. `RunMarks::None`
    /// when nothing is drawn, AND when the marks were composited into `region` already
    /// (`MarksMode::OverlayOnRegion`) — an engine never has to composite anything itself.
    pub marks: RunMarks,
}

/// How the user's marks layer reaches an engine.
///
/// Picked by the user in the compact panel among the modes the selected engine supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarksMode {
    /// The marks are composited onto the region the engine edits: the model sees them as part
    /// of the picture («На редактируемую картинку»).
    OverlayOnRegion,
    /// The region stays clean and a COPY of it with the marks composited on top travels as a
    /// separate reference image («Отдельный референс»).
    SeparateReference,
    /// The region stays clean and the marks travel alone, as a transparent RGBA layer
    /// («Прозрачный слой»).
    TransparentLayer,
}

impl MarksMode {
    /// Every mode, in the order the panel lists them.
    pub const ALL: [Self; 3] = [Self::OverlayOnRegion, Self::SeparateReference, Self::TransparentLayer];
}

/// What an engine accepts as marks: a NON-EMPTY set of modes and the preferred one, which is
/// always in the set.
///
/// Both invariants hold by construction: [`MarksSupport::new`] starts from the preferred mode
/// and [`MarksSupport::with`] can only add modes, so an empty set or a preferred mode outside
/// the set cannot be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarksSupport {
    overlay: bool,
    reference: bool,
    layer: bool,
    preferred: MarksMode,
}

impl MarksSupport {
    /// The default of every engine: the marks are composited onto the region and nothing else.
    pub const OVERLAY_ONLY: Self = Self::new(MarksMode::OverlayOnRegion);

    /// A set holding exactly `preferred`, which is also the preferred mode.
    #[must_use]
    pub const fn new(preferred: MarksMode) -> Self {
        Self { overlay: false, reference: false, layer: false, preferred }.with(preferred)
    }

    /// The same set plus `mode`; the preferred mode is unchanged.
    #[must_use]
    pub const fn with(self, mode: MarksMode) -> Self {
        match mode {
            MarksMode::OverlayOnRegion => Self { overlay: true, ..self },
            MarksMode::SeparateReference => Self { reference: true, ..self },
            MarksMode::TransparentLayer => Self { layer: true, ..self },
        }
    }

    /// Whether `mode` is in the set.
    #[must_use]
    pub const fn supports(&self, mode: MarksMode) -> bool {
        match mode {
            MarksMode::OverlayOnRegion => self.overlay,
            MarksMode::SeparateReference => self.reference,
            MarksMode::TransparentLayer => self.layer,
        }
    }

    /// The mode the host switches to when this engine is selected, or when the user's choice
    /// stops being supported.
    #[must_use]
    pub const fn preferred(&self) -> MarksMode {
        self.preferred
    }
}

/// The marks of one run, in the form the host chose.
#[derive(Debug, Clone)]
pub enum RunMarks {
    /// No marks travel separately: none were drawn, or they were composited into the region.
    None,
    /// A copy of the region with the marks composited over it, exactly `rect_px` in size and as
    /// opaque as the region is. Premultiplied like every `egui::ColorImage`.
    Reference(egui::ColorImage),
    /// The marks layer alone, `rect_px` in size, STRAIGHT (non-premultiplied) alpha; a pixel
    /// nobody marked is `[0, 0, 0, 0]`.
    Layer(image::RgbaImage),
}

/// `region` with the straight-alpha marks layer `marks` composited OVER it ("over" operator).
///
/// `marks` holds 4 straight-alpha bytes per pixel in the region's row-major order. The
/// operation is done in premultiplied space — `out = mark + region * (1 - mark.a)` — which is
/// exact for any region alpha, so an unmarked pixel comes back bit-identical and a fully
/// opaque mark replaces the pixel outright.
///
/// Returns `None`, having produced nothing, when `marks` does not hold exactly
/// `4 * width * height` bytes: a layer of another shape must never be stretched over a region.
#[must_use]
pub(super) fn composite_marks_over(region: &egui::ColorImage, marks: &[u8]) -> Option<egui::ColorImage> {
    if marks.len() != region.pixels.len().checked_mul(4)? {
        return None;
    }
    let pixels = region
        .pixels
        .iter()
        .zip(marks.chunks_exact(4))
        .map(|(dst, mark)| {
            let alpha = mark[3];
            if alpha == 0 {
                return *dst;
            }
            let src = egui::Color32::from_rgba_unmultiplied(mark[0], mark[1], mark[2], alpha).to_array();
            let dst = dst.to_array();
            let keep = u16::from(255 - alpha);
            // `src + dst * (255 - a) / 255` per premultiplied channel. Both terms are bounded by
            // the result alpha, which never exceeds 255, so the sum fits a byte; `min` keeps
            // the narrowing total against rounding.
            let blend = |s: u8, d: u8| -> u8 {
                let scaled = (u16::from(d) * keep + 127) / 255;
                u8::try_from((u16::from(s) + scaled).min(255)).unwrap_or(u8::MAX)
            };
            egui::Color32::from_rgba_premultiplied(
                blend(src[0], dst[0]),
                blend(src[1], dst[1]),
                blend(src[2], dst[2]),
                blend(src[3], dst[3]),
            )
        })
        .collect();
    Some(egui::ColorImage::new(region.size, pixels))
}

/// What one `poll` says about the run in flight.
///
/// `Done` and `Failed` are TERMINAL and are reported exactly once: the engine has dropped
/// the run by the time it returns either, and the next poll answers `Idle`. `Done` carries a
/// `ColorImage` of exactly the requested `rect_px` — the host refuses any other size rather
/// than rescaling it (D7).
#[derive(Debug, Clone)]
pub enum EnginePoll {
    /// No run has been started, or the last one has already been reported.
    Idle,
    /// A run is in flight; the engine's own panel shows how far it has got.
    Running,
    /// The run finished. The image is exactly `rect_px` in size.
    Done(egui::ColorImage),
    /// The run failed, with an already-localized message for the user.
    Failed(String),
}

/// Host facts the engine's pinned run options ([`AiEngine::draw_run_options`]) may depend on,
/// handed in per frame because the engine never sees the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunOptionsCtx {
    /// At least one mask layer holds a painted pixel (`MaskStack::is_empty` is `false`). The
    /// marks layer does not count: it is never a permission mask.
    pub mask_painted: bool,
}

/// One AI engine a region-editing host can run.
///
/// Ownership boundary: the engine owns its parameters, its persistence, its wire protocol,
/// its worker threads and its progress reporting; the host owns the rectangle, the mask
/// stack, the pending result and the apply path. An engine never touches `CanvasView`,
/// `ProjectData` or the frame, and never blocks the GUI thread — every call below must
/// return within a frame.
pub trait AiEngine {
    /// Stable identifier, used for widget ids and logs. Never shown to the user.
    fn id(&self) -> &'static str;

    /// Localized name for the engine picker, resolved at draw time.
    fn title(&self) -> String;

    /// Which picker section the engine appears in.
    fn section(&self) -> EngineSection;

    /// Whether the engine needs the PyTorch runtime, which gates its run button.
    fn requires_torch(&self) -> bool;

    /// Size requirements the engine imposes on the frame rectangle, in source page pixels.
    ///
    /// The host hands these to the frame, which snaps a DRAGGED rectangle to them and marks a
    /// rectangle that does not satisfy them as invalid — «Обработать» is refused while it does
    /// not, so a rect that reaches `start` has already satisfied every one of them. A frame
    /// that was valid for the previous engine and is not valid for this one simply turns red;
    /// it is never resized behind the user's back.
    fn constraints(&self) -> FrameConstraints;

    /// The mask layers the engine wants painted, in painting order. Must not be empty.
    fn mask_layers(&self) -> Vec<MaskLayerSpec>;

    /// Whether a run with every mask layer empty is meaningful — for FLUX.2 klein it is,
    /// because an empty mask IS its whole-region working mode. The host relaxes the frame's
    /// non-empty-mask rule exactly when this is `true`, tells the user so with its own
    /// green line under «Обработать», and re-reads the answer whenever the engine's
    /// parameters change, because an engine may make it depend on one of them.
    fn allows_empty_mask(&self) -> bool;

    /// Draws the engine's parameters and its engine-specific status — the SCROLLED body of
    /// the «Редактор области» panel. The progress belongs in [`AiEngine::draw_progress`] and
    /// must not be drawn here as well.
    ///
    /// A plain `&mut Ui`: an engine never touches `CanvasView`, `ProjectData` or the frame.
    /// The panel may not be visible on a given frame, so nothing this method does may be a
    /// precondition of `poll`.
    fn draw_parameters(&mut self, ui: &mut egui::Ui);

    /// Draws the engine's own progress — pinned at the TOP of the «Редактор области» panel,
    /// outside its scroll, so it stays visible however far the parameters are scrolled.
    /// Default: nothing.
    ///
    /// Draw NOTHING while there is nothing to report: an empty section takes no height (the
    /// dock allocates nothing for it, not even an item spacing), and
    /// every point it takes comes off the parameters' scroll viewport. Same rules as
    /// [`AiEngine::draw_parameters`]: the panel may be hidden, so nothing here may be a
    /// precondition of `poll`.
    fn draw_progress(&mut self, _ui: &mut egui::Ui) {}

    /// Draws the engine's per-run options — pinned at the BOTTOM of the panel, under the
    /// host's «Обработать» section, outside the scroll. Default: nothing.
    ///
    /// Only for the few options a user decides per run, next to the button that starts it;
    /// everything else belongs in [`AiEngine::draw_parameters`]. `ctx` carries the host facts
    /// such an option may depend on. Same rules as `draw_parameters`.
    fn draw_run_options(&mut self, _ui: &mut egui::Ui, _ctx: RunOptionsCtx) {}

    /// Why a run is refused right now, localized; `None` when it may start.
    ///
    /// Only the engine's OWN half of the gate: the rectangle is already validated against
    /// `constraints()` by the frame, and the non-empty-mask rule is `allows_empty_mask()`.
    fn run_block_reason(&self) -> Option<String>;

    /// Why switching AWAY from this engine is unsafe right now, localized; `None` when the
    /// picker may switch freely. Default: `None`.
    ///
    /// The host disables the engine picker while a reason stands and puts it on the
    /// disabled tooltip, so a refusal always says what it is waiting for.
    ///
    /// It exists because the frame lock — the picker's other gate — only covers a RUN. An
    /// engine may own work that no frame state describes: FLUX.2 klein's model download is
    /// a multi-gigabyte transfer with its own free-space budget, and nothing else would
    /// stop a user from starting a second one from the other engine and overcommitting the
    /// disk. An engine that owns no such work leaves the default.
    ///
    /// Reported, never enforced silently: this only closes a control, it does not abort
    /// anything, and it must stay cheap enough to call every frame.
    fn switch_block_reason(&self) -> Option<String> {
        None
    }

    /// How the user's marks layer may reach this engine. Default: composited onto the region
    /// only ([`MarksSupport::OVERLAY_ONLY`]).
    ///
    /// Re-read by the host EVERY frame, like `constraints()`, because an engine may derive it
    /// from one of its own parameters (the selected API model, say). When the user's chosen
    /// mode stops being supported the host falls back to `preferred()`. Must stay cheap.
    fn marks_support(&self) -> MarksSupport {
        MarksSupport::OVERLAY_ONLY
    }

    /// Starts one run. The engine takes over `request` and reports through `poll`.
    ///
    /// # Errors
    /// Returns a localized message when a run is already in flight or when `request` fails
    /// the size checks the host is supposed to guarantee.
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String>;

    /// Called every frame while the tool is active, panel visible or not — this is where an
    /// engine drains its channels, so skipping it strands a finished run.
    fn poll(&mut self, ctx: &egui::Context) -> EnginePoll;

    /// Abandons the run in flight, if any. A no-op when nothing is running.
    fn cancel(&mut self);

    /// Tells the engine whether the AI backend process is reachable.
    fn set_backend_available(&mut self, available: bool);

    /// Tells the engine whether the PyTorch runtime is present.
    fn set_torch_available(&mut self, available: bool);

    /// Publishes the frame's current rectangle, `None` while the tool has no frame.
    ///
    /// Called every frame, like the two setters above. It exists because an engine's
    /// parameter panel legitimately depends on the region BEFORE a run — FLUX.2 klein asks
    /// the backend for a RAM/VRAM forecast of exactly this rectangle and prints its size —
    /// and neither `EngineRunRequest` (which only exists once a run starts) nor
    /// `draw_parameters` (which may not be drawn at all) can supply it. An engine that does
    /// not care leaves the body empty.
    ///
    /// `geometry_settled` is `false` while a frame gesture is in flight — a move drag, a
    /// resize drag or a mask stroke. The rectangle is THIS frame's either way and must keep
    /// being DISPLAYED live, but an engine must not START WORK off an unsettled geometry: a
    /// resize drag publishes a new rectangle on every rendered frame, so an engine that
    /// queried the backend on each of them would issue one request per frame. Defer such
    /// work to the first call that reports `true`.
    fn set_region(&mut self, region: Option<OverlayRectPx>, geometry_settled: bool);
}

/// The localized sentence that says which rule a size broke.
///
/// The ONE mapping from a violation to its words: the host's main panel prints it under an
/// invalid frame, and [`region_size_refusal`] returns it from an engine's run path, so the two
/// read exactly alike and a new variant is worded in one place.
#[must_use]
pub(in crate::tools) fn violation_text(violation: SizeViolation) -> &'static str {
    match violation {
        SizeViolation::NotMultiple => t!("cleaning.tools.area_editor.violation_multiple"),
        SizeViolation::TooSmall => t!("cleaning.tools.area_editor.violation_min_side"),
        SizeViolation::TooLarge => t!("cleaning.tools.area_editor.violation_max_side"),
        SizeViolation::AreaTooSmall => t!("cleaning.tools.area_editor.violation_min_area"),
        SizeViolation::AreaTooLarge => t!("cleaning.tools.area_editor.violation_max_area"),
        SizeViolation::AspectTooSteep => t!("cleaning.tools.area_editor.violation_aspect"),
        SizeViolation::NotAllowedSize => t!("cleaning.tools.area_editor.violation_not_allowed_size"),
    }
}

/// The localized refusal for a region that violates `constraints`, or `None` when the size
/// satisfies every one of them. `width` and `height` are in source page pixels.
///
/// This is the RUN-PATH half of the size contract, and it is deliberately a second copy of a
/// check the frame already performs: the frame snaps its rectangle to the same constraints,
/// but the rectangle and the region an engine is handed can disagree — a host that hands
/// over another size must be told which rule it broke instead of having the backend refuse
/// the blob. The verdict comes from `geometry::check_size`, the single authority on what a
/// valid size is, so this side can never accept a size the frame paints red.
///
/// The wording is [`violation_text`], so an engine's refusal reads exactly like the line the
/// host draws under an invalid rectangle.
#[must_use]
pub(in crate::tools) fn region_size_refusal(
    width: usize,
    height: usize,
    constraints: &FrameConstraints,
) -> Option<String> {
    let violation = check_size(width, height, constraints)?;
    Some(violation_text(violation).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constructor cannot build an empty set nor a preferred mode outside it, and `with`
    /// only ever adds.
    #[test]
    fn marks_support_always_holds_its_preferred_mode() {
        for preferred in MarksMode::ALL {
            let support = MarksSupport::new(preferred);
            assert_eq!(support.preferred(), preferred);
            assert!(support.supports(preferred), "{preferred:?} must be in its own set");
            for other in MarksMode::ALL {
                assert_eq!(support.supports(other), other == preferred, "{preferred:?} / {other:?}");
            }
            let wider = support.with(MarksMode::TransparentLayer);
            assert_eq!(wider.preferred(), preferred, "adding a mode never changes the preference");
            assert!(wider.supports(preferred) && wider.supports(MarksMode::TransparentLayer));
        }
        assert_eq!(MarksSupport::OVERLAY_ONLY.preferred(), MarksMode::OverlayOnRegion);
        assert!(!MarksSupport::OVERLAY_ONLY.supports(MarksMode::SeparateReference));
        assert!(!MarksSupport::OVERLAY_ONLY.supports(MarksMode::TransparentLayer));
    }

    /// Unmarked pixels come back bit-identical, an opaque mark replaces the pixel, a
    /// translucent mark blends, and a layer of the wrong length is refused.
    #[test]
    fn marks_are_composited_with_the_over_operator() {
        let region = egui::ColorImage::new(
            [2, 2],
            vec![
                egui::Color32::from_rgb(10, 20, 30),
                egui::Color32::from_rgb(200, 100, 50),
                egui::Color32::from_rgba_premultiplied(40, 40, 40, 128),
                egui::Color32::from_rgb(0, 0, 0),
            ],
        );
        let mut marks = vec![0u8; 16];
        // Pixel 1: opaque red. Pixel 3: half-transparent white over black.
        marks[4..8].copy_from_slice(&[255, 0, 0, 255]);
        marks[12..16].copy_from_slice(&[255, 255, 255, 128]);
        let out = composite_marks_over(&region, &marks).expect("the layer has the region's shape");
        assert_eq!(out.size, [2, 2]);
        assert_eq!(out.pixels[0], region.pixels[0], "an unmarked pixel is untouched");
        assert_eq!(out.pixels[2], region.pixels[2], "an unmarked translucent pixel is untouched");
        assert_eq!(out.pixels[1], egui::Color32::from_rgb(255, 0, 0), "an opaque mark replaces the pixel");
        let [r, g, b, a] = out.pixels[3].to_array();
        assert_eq!(a, 255, "a mark over an opaque pixel stays opaque");
        assert!(r == g && g == b && (127..=129).contains(&r), "half white over black is mid grey: {r}");

        assert!(composite_marks_over(&region, &marks[..12]).is_none(), "a short layer is refused");
        assert!(composite_marks_over(&region, &[0u8; 20]).is_none(), "a long layer is refused");
    }

    /// The shared run-path re-check answers exactly what `check_size` decides, and names the
    /// rule that was broken.
    #[test]
    fn the_run_path_size_refusal_follows_the_authoritative_checker() {
        let grid_of_8 =
            FrameConstraints { multiple: 8, min_side: 8, ..FrameConstraints::UNCONSTRAINED };
        assert!(region_size_refusal(64, 64, &grid_of_8).is_none());
        // Off the grid, and below the shortest side that grid allows.
        assert!(region_size_refusal(7, 8, &grid_of_8).is_some());
        assert!(region_size_refusal(9, 16, &grid_of_8).is_some());
        for (w, h) in [(1024usize, 1024usize), (250, 256), (0, 8)] {
            assert_eq!(
                region_size_refusal(w, h, &grid_of_8).is_some(),
                check_size(w, h, &grid_of_8).is_some(),
                "{w}x{h}"
            );
        }
    }

    /// Every violation has a sentence of its own, and the run path speaks it word for word: a
    /// copy-pasted arm in the one mapping would tell the user to fix the wrong rule.
    #[test]
    fn every_size_violation_has_its_own_sentence() {
        // `t!` answers against the PROCESS-GLOBAL catalog and degrades to the bare key without
        // one; install the reference catalog under the shared lock, as the other UI-string
        // tests of this crate do.
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");
        let texts = [
            violation_text(SizeViolation::NotMultiple),
            violation_text(SizeViolation::TooSmall),
            violation_text(SizeViolation::TooLarge),
            violation_text(SizeViolation::AreaTooSmall),
            violation_text(SizeViolation::AreaTooLarge),
            violation_text(SizeViolation::AspectTooSteep),
            violation_text(SizeViolation::NotAllowedSize),
        ];
        for (idx, text) in texts.iter().enumerate() {
            assert!(!text.is_empty());
            assert!(texts[idx + 1..].iter().all(|other| other != text), "{text} is used twice");
        }
        let grid_of_8 =
            FrameConstraints { multiple: 8, min_side: 8, ..FrameConstraints::UNCONSTRAINED };
        assert_eq!(region_size_refusal(12, 16, &grid_of_8).as_deref(), Some(violation_text(SizeViolation::NotMultiple)));
    }
}
