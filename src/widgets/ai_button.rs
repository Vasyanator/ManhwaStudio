/*
FILE HEADER (widgets/ai_button.rs)
- Purpose: a button for launching an AI tool that gates its own availability on
  three process-global capability signals (`ai_backend_capabilities`):
  backend / torch / onnxruntime. It disables itself automatically when the
  required runtime is unavailable and shows the reason on hover.
- Key items:
  - `AiRequirement`: which runtime a tool needs (Backend/Torch/Onnx/TorchOrOnnx).
    `satisfied` is the strict check; `is_met(caps, unknown_ok)` adds an opt-in
    optimistic mode where an unknown capability counts as available.
  - `AiCaps`: a pure snapshot of the three global signals (keeps `satisfied`/
    `is_met` unit-testable without touching globals).
  - `AiButton`: builder widget (text + requirement + optional selected/marker/
    min_size/extra enable condition/`enabled_on_unknown` optimistic gating).
  - `AiButtonResponse`: per-frame result (`response`, `enabled`).
  - `marker_badge_overhang`: how far the marker badge sticks out past the
    button's right edge, for callers that budget a button's width.
- Drawing invariant: the marker badge is painted with the painter ONLY, over the
  button; it NEVER allocates a second interactive rect (which would carve a hole
  in the button hitbox). Clicks/hover come solely from the single `response`.
*/

use eframe::egui;
use egui::Vec2;

/// Which runtime capability a tool needs to be usable.
///
/// [`AiRequirement::satisfied`] is the strict check (only `Some(true)` counts as
/// available). [`AiRequirement::is_met`] generalizes it with an opt-in optimistic
/// mode where an unknown (`None`) capability may count as available; a known
/// `Some(false)` always gates off in either mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRequirement {
    Backend,
    Torch,
    Onnx,
    TorchOrOnnx,
}

impl AiRequirement {
    /// Strict check: only `Some(true)` counts as available (both `None` and
    /// `Some(false)` gate off). Alias for [`AiRequirement::is_met`] with
    /// `unknown_ok = false`, kept as the default gating used by most callers.
    #[must_use]
    pub fn satisfied(self, caps: &AiCaps) -> bool {
        self.is_met(caps, false)
    }

    /// Requirement check. `unknown_ok` decides whether an unknown (`None`)
    /// capability counts as available (optimistic) or not (strict). A known
    /// `Some(false)` is ALWAYS unavailable regardless of `unknown_ok`. Pure (no
    /// globals) so the gating logic stays unit-testable.
    #[must_use]
    pub fn is_met(self, caps: &AiCaps, unknown_ok: bool) -> bool {
        let ok = |v: Option<bool>| matches!(v, Some(true)) || (unknown_ok && v.is_none());
        match self {
            AiRequirement::Backend => ok(caps.backend),
            AiRequirement::Torch => ok(caps.torch),
            AiRequirement::Onnx => ok(caps.ort),
            AiRequirement::TorchOrOnnx => ok(caps.torch) || ok(caps.ort),
        }
    }

    /// Returns a short Russian user-facing reason the requirement is unmet, for a
    /// disabled-button tooltip. Picks the most specific applicable message. Public
    /// so plain (non-`AiButton`) controls can reuse the same disabled reason.
    #[must_use]
    pub fn disabled_reason(self, caps: &AiCaps) -> &'static str {
        match self {
            AiRequirement::Backend => t!("widgets.ai_button.backend_unavailable"),
            AiRequirement::Torch => {
                // Torch lives in the backend: if the backend itself is unreachable,
                // that is the more specific (root) cause to report.
                if caps.backend == Some(true) {
                    t!("widgets.ai_button.requires_pytorch")
                } else {
                    t!("widgets.ai_button.backend_unavailable")
                }
            }
            AiRequirement::Onnx => t!("widgets.ai_button.requires_onnxruntime"),
            AiRequirement::TorchOrOnnx => t!("widgets.ai_button.requires_pytorch_or_onnxruntime"),
        }
    }
}

/// Immutable snapshot of the three process-global AI capability slots. Kept pure
/// (no globals in `satisfied`) so the requirement logic is unit-testable.
#[derive(Debug, Clone, Copy)]
pub struct AiCaps {
    pub backend: Option<bool>,
    pub torch: Option<bool>,
    pub ort: Option<bool>,
}

impl AiCaps {
    /// Reads the three process-global capability slots into a snapshot.
    #[must_use]
    pub fn current() -> Self {
        Self {
            backend: crate::ai_backend_capabilities::backend_available(),
            torch: crate::ai_backend_capabilities::torch_available(),
            ort: crate::ai_backend_capabilities::ort_available(),
        }
    }
}

/// Result of drawing an [`AiButton`] for one frame: the underlying egui
/// `Response` (for clicks/hover) and whether the button was enabled this frame.
#[derive(Debug)]
pub struct AiButtonResponse {
    pub response: egui::Response,
    pub enabled: bool,
}

/// Builder for an AI-tool button that gates itself on runtime capabilities.
///
/// The button is enabled only when `custom_enabled` (an optional caller-supplied
/// extra condition, default `true`) AND the [`AiRequirement`] are both satisfied.
/// When disabled by an unmet requirement it shows the requirement's reason on
/// hover; when disabled purely by `custom_enabled`, no AI reason is shown.
///
/// `enabled_on_unknown` (default `false`) opts into optimistic gating: an unknown
/// (`None`) capability then counts as available, so a runtime whose capability is
/// not yet probed (e.g. the native ONNX runtime before its first load) does not
/// lock the button out. A known `Some(false)` still disables regardless.
///
/// `frame` (default `true`) selects the visual: `true` renders a normal framed
/// button; `false` renders a frameless selectable (transparent at rest, highlighted
/// only on hover/selection — like `ui.selectable_value`), for toggle rows that
/// should not show a resting background box.
pub struct AiButton {
    text: egui::WidgetText,
    requirement: AiRequirement,
    marker: Option<String>,
    selected: bool,
    min_size: Option<Vec2>,
    custom_enabled: bool,
    enabled_on_unknown: bool,
    frame: bool,
}

impl AiButton {
    /// Creates a button labelled `text` gated on `requirement`. Defaults:
    /// `custom_enabled = true`, `selected = false`, `enabled_on_unknown = false`,
    /// `frame = true`, no marker, no explicit size.
    pub fn new(text: impl Into<egui::WidgetText>, requirement: AiRequirement) -> Self {
        Self {
            text: text.into(),
            requirement,
            marker: None,
            selected: false,
            min_size: None,
            custom_enabled: true,
            enabled_on_unknown: false,
            frame: true,
        }
    }

    /// Adds an optional free-form corner badge (e.g. a backend/runtime tag) painted
    /// in the top-right corner of the button.
    #[must_use]
    pub fn marker(mut self, text: impl Into<String>) -> Self {
        self.marker = Some(text.into());
        self
    }

    /// Sets the toggled ("selected") look of the button.
    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Sets a minimum size for the button.
    #[must_use]
    pub fn min_size(mut self, size: Vec2) -> Self {
        self.min_size = Some(size);
        self
    }

    /// ANDs an extra caller-supplied condition into the enable state (chainable).
    /// The button stays enabled only if every `and_enabled` condition is `true`
    /// AND the runtime requirement is satisfied.
    #[must_use]
    pub fn and_enabled(mut self, condition: bool) -> Self {
        self.custom_enabled = self.custom_enabled && condition;
        self
    }

    /// Opts into optimistic gating: when `value` is `true`, an unknown (`None`)
    /// capability counts as available so a not-yet-probed runtime does not disable
    /// the button. A known `Some(false)` still disables. Default `false` (strict).
    #[must_use]
    pub fn enabled_on_unknown(mut self, value: bool) -> Self {
        self.enabled_on_unknown = value;
        self
    }

    /// Selects the visual: `true` (default) draws a normal framed button; `false`
    /// draws a frameless selectable (transparent at rest, highlighted only on
    /// hover/selection, like `ui.selectable_value`). Frameless ignores `min_size`.
    #[must_use]
    pub fn frame(mut self, frame: bool) -> Self {
        self.frame = frame;
        self
    }

    /// Draws the button for this frame, applying capability gating, the disabled
    /// hover reason, and the optional corner badge. Returns the response and the
    /// resolved enabled state.
    pub fn draw(self, ui: &mut egui::Ui) -> AiButtonResponse {
        let caps = AiCaps::current();
        let requirement_satisfied = self.requirement.is_met(&caps, self.enabled_on_unknown);
        let enabled = self.custom_enabled && requirement_satisfied;

        // Single allocation/interaction: this is the ONLY hitbox for the button.
        // `frame` picks a framed button vs a frameless selectable (transparent at
        // rest); the frameless variant ignores `min_size` (SelectableLabel has none).
        let response = if self.frame {
            let mut button = egui::Button::new(self.text).selected(self.selected);
            if let Some(size) = self.min_size {
                button = button.min_size(size);
            }
            ui.add_enabled(enabled, button)
        } else {
            // `Button::selectable` sets `frame_when_inactive(selected)`, so an
            // unselected button has no resting frame (transparent), matching
            // `ui.selectable_value`; the selection/hover highlight still shows.
            ui.add_enabled(enabled, egui::Button::selectable(self.selected, self.text))
        };

        // Only surface an AI reason when the block is due to the unmet requirement;
        // a block caused purely by `custom_enabled` carries no AI-capability reason.
        let response = if !enabled && !requirement_satisfied {
            response.on_disabled_hover_text(self.requirement.disabled_reason(&caps))
        } else {
            response
        };

        // Painter-only badge: never allocate/interact a second rect (that would
        // carve a hole in the button hitbox); reuse `response.rect` for placement.
        if let Some(label) = self.marker.as_ref()
            && ui.is_rect_visible(response.rect)
        {
            paint_marker_badge(ui, response.rect, label, enabled);
        }

        AiButtonResponse { response, enabled }
    }
}

/// Inner padding, in points, between the marker badge's caption and its pill
/// outline.
const MARKER_BADGE_PAD: Vec2 = Vec2::new(3.0, 1.0);

/// The font the marker badge is laid out with under `style`: a third of the
/// button text size ("3x smaller"), clamped so the badge stays legible.
#[must_use]
fn marker_badge_font(style: &egui::Style) -> egui::FontId {
    let base = egui::TextStyle::Button.resolve(style).size;
    egui::FontId::proportional((base / 3.0).max(6.0))
}

/// Outer size of the badge pill whose caption laid out to `caption_size`.
///
/// Pure, and the ONE definition of the pill's size: the painter and the width
/// measurement below both go through it, so "how big is the badge" cannot be
/// answered two ways.
#[must_use]
fn marker_badge_size(caption_size: Vec2) -> Vec2 {
    caption_size + MARKER_BADGE_PAD * 2.0
}

/// Corner radius of a badge of `badge_size` — half its height, which is what makes
/// each pill end a semicircle.
///
/// It is ALSO the badge's overhang past the button's right edge, because
/// [`marker_badge_rect`] centres the right semicircle on the button's top-right
/// corner. The two are one number by construction, which is the invariant
/// `marker_badge_overhang_is_the_pills_right_radius` pins.
#[must_use]
fn marker_badge_radius(badge_size: Vec2) -> f32 {
    badge_size.y / 2.0
}

/// Where a badge whose caption laid out to `caption_size` lands on `button_rect`.
///
/// Pure geometry: the right end's semicircle centre (at `max.x - radius`) sits on
/// the button's top-right corner and the badge's vertical centre sits on the top
/// edge, so the pill straddles the border and overhangs the right edge by exactly
/// [`marker_badge_radius`].
#[must_use]
fn marker_badge_rect(button_rect: egui::Rect, caption_size: Vec2) -> egui::Rect {
    let badge_size = marker_badge_size(caption_size);
    let badge_max_x = button_rect.right() + marker_badge_radius(badge_size);
    let badge_min = egui::pos2(
        badge_max_x - badge_size.x,
        button_rect.top() - badge_size.y / 2.0,
    );
    egui::Rect::from_min_size(badge_min, badge_size)
}

/// Lays `label` out in the badge font under `style` and returns the caption size.
///
/// Text LAYOUT only (no painting, no I/O), so it is safe on the GUI thread, and
/// egui caches galleys, so re-measuring a fixed marker every frame is a lookup.
/// The colour is irrelevant to the size, so the measurement path uses an arbitrary
/// one and the painter lays the caption out again in the colour it needs.
#[must_use]
fn marker_badge_caption_size(ctx: &egui::Context, style: &egui::Style, label: &str) -> Vec2 {
    let font = marker_badge_font(style);
    ctx.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(label.to_string(), font, egui::Color32::WHITE)
            .size()
    })
}

/// Points the marker badge painted for `label` overhangs the button's RIGHT edge
/// by, under `style`.
///
/// A caller that budgets a button's width — a panel that must not clip the badge at
/// its border — has to add this on top of the button's own width; the badge is
/// painted outside the button's allocated rect and is not part of it.
///
/// Same layout-only cost and safety as [`marker_badge_caption_size`].
#[must_use]
pub fn marker_badge_overhang(ctx: &egui::Context, style: &egui::Style, label: &str) -> f32 {
    let caption_size = marker_badge_caption_size(ctx, style, label);
    marker_badge_radius(marker_badge_size(caption_size))
}

/// Paints the pill-shaped marker badge straddling the TOP-RIGHT corner of
/// `button_rect`. Painter-only: it allocates no rect and never interacts, so it
/// cannot affect the button hitbox. Placement and size come from
/// [`marker_badge_rect`] / [`marker_badge_size`], the same pure geometry
/// [`marker_badge_overhang`] answers from, so a caller's width budget and what is
/// actually drawn cannot drift. `enabled` selects a muted foreground when the
/// button is disabled so the badge reads as inactive too. Colours are derived from
/// the current visuals so the badge stays legible in both light and dark themes.
fn paint_marker_badge(ui: &egui::Ui, button_rect: egui::Rect, label: &str, enabled: bool) {
    let style = ui.style();
    let badge_bg = ui.visuals().widgets.active.bg_fill;
    let badge_fg = if enabled {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };

    // The measured caption size and the painted galley are laid out from the SAME
    // style and font; only the colour differs, which does not affect the size.
    let caption_size = marker_badge_caption_size(ui.ctx(), style, label);
    let font = marker_badge_font(style);
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.to_string(), font, badge_fg));

    let badge_rect = marker_badge_rect(button_rect, caption_size);
    let radius = marker_badge_radius(badge_rect.size());

    // f32 -> u8 corner radius: `radius` is a small, non-negative half-height that
    // fits u8; clamp guards the conversion (no lossless integer alternative exists).
    let corner = egui::CornerRadius::same(radius.round().clamp(0.0, f32::from(u8::MAX)) as u8);
    ui.painter().rect_filled(badge_rect, corner, badge_bg);
    ui.painter()
        .galley(badge_rect.min + MARKER_BADGE_PAD, galley, badge_fg);
}

#[cfg(test)]
mod tests {
    use super::{
        AiCaps, AiRequirement, marker_badge_radius, marker_badge_rect, marker_badge_size,
    };
    use eframe::egui;
    use egui::Vec2;

    /// The number a caller budgets a button's width with (`marker_badge_overhang`)
    /// and the number the painter actually draws with must be ONE number. Both are
    /// `marker_badge_radius(marker_badge_size(caption))`, and the invariant that
    /// makes that correct is geometric: the pill's right end is centred on the
    /// button's top-right corner, so the drawn rect sticks out past that edge by
    /// exactly the radius. Pure, so it needs no fonts and no window — only the
    /// caption measurement does, and that one is colour-independent by
    /// construction.
    #[test]
    fn marker_badge_overhang_is_the_pills_right_radius() {
        let button = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(120.0, 24.0));
        for caption in [
            Vec2::new(18.0, 7.0),
            Vec2::new(31.5, 9.0),
            Vec2::new(0.0, 0.0),
        ] {
            let size = marker_badge_size(caption);
            let radius = marker_badge_radius(size);
            let rect = marker_badge_rect(button, caption);
            assert_eq!(rect.size(), size, "the drawn pill is the measured pill");
            assert!(
                (rect.right() - button.right() - radius).abs() <= f32::EPSILON * 100.0,
                "the drawn overhang {} must equal the budgeted radius {radius}",
                rect.right() - button.right()
            );
            // Straddles the top border: half the pill above it, half below.
            assert!((rect.center().y - button.top()).abs() <= f32::EPSILON * 100.0);
        }
    }

    /// Convenience constructor for a capability snapshot in tests.
    fn caps(backend: Option<bool>, torch: Option<bool>, ort: Option<bool>) -> AiCaps {
        AiCaps {
            backend,
            torch,
            ort,
        }
    }

    #[test]
    fn backend_requirement_needs_backend_up() {
        assert!(AiRequirement::Backend.satisfied(&caps(Some(true), None, None)));
        assert!(!AiRequirement::Backend.satisfied(&caps(Some(false), Some(true), Some(true))));
        assert!(!AiRequirement::Backend.satisfied(&caps(None, Some(true), Some(true))));
    }

    #[test]
    fn torch_requirement_needs_torch_present() {
        assert!(AiRequirement::Torch.satisfied(&caps(Some(true), Some(true), None)));
        assert!(!AiRequirement::Torch.satisfied(&caps(Some(true), Some(false), Some(true))));
        assert!(!AiRequirement::Torch.satisfied(&caps(Some(true), None, Some(true))));
    }

    #[test]
    fn onnx_requirement_needs_ort_present() {
        assert!(AiRequirement::Onnx.satisfied(&caps(None, None, Some(true))));
        assert!(!AiRequirement::Onnx.satisfied(&caps(Some(true), Some(true), Some(false))));
        assert!(!AiRequirement::Onnx.satisfied(&caps(Some(true), Some(true), None)));
    }

    #[test]
    fn torch_or_onnx_requirement_needs_either() {
        // Either present satisfies it.
        assert!(AiRequirement::TorchOrOnnx.satisfied(&caps(Some(true), Some(true), None)));
        assert!(AiRequirement::TorchOrOnnx.satisfied(&caps(Some(true), None, Some(true))));
        assert!(AiRequirement::TorchOrOnnx.satisfied(&caps(Some(true), Some(false), Some(true))));
        // Neither present (absent/unknown) gates off.
        assert!(!AiRequirement::TorchOrOnnx.satisfied(&caps(Some(true), Some(false), Some(false))));
        assert!(!AiRequirement::TorchOrOnnx.satisfied(&caps(Some(true), None, None)));
    }

    #[test]
    fn optimistic_mode_treats_unknown_as_available() {
        // `unknown_ok = true`: an unknown capability counts as available.
        assert!(AiRequirement::Onnx.is_met(&caps(None, None, None), true));
        assert!(AiRequirement::Torch.is_met(&caps(None, None, None), true));
        assert!(AiRequirement::TorchOrOnnx.is_met(&caps(None, None, None), true));
        // But a KNOWN-unavailable capability still gates off even when optimistic.
        assert!(!AiRequirement::Onnx.is_met(&caps(None, None, Some(false)), true));
        assert!(!AiRequirement::Torch.is_met(&caps(None, Some(false), None), true));
        // TorchOrOnnx: unknown one side is still optimistically available.
        assert!(AiRequirement::TorchOrOnnx.is_met(&caps(None, Some(false), None), true));
        // Both known-unavailable gates off.
        assert!(!AiRequirement::TorchOrOnnx.is_met(&caps(None, Some(false), Some(false)), true));
    }

    #[test]
    fn strict_mode_rejects_unknown() {
        // `satisfied` (strict) and `is_met(.., false)` reject unknown capabilities.
        assert!(!AiRequirement::Onnx.satisfied(&caps(None, None, None)));
        assert!(!AiRequirement::Onnx.is_met(&caps(None, None, None), false));
        assert!(!AiRequirement::Torch.is_met(&caps(None, None, None), false));
        // A known-available capability satisfies both modes.
        assert!(AiRequirement::Onnx.is_met(&caps(None, None, Some(true)), false));
        assert!(AiRequirement::Onnx.is_met(&caps(None, None, Some(true)), true));
    }
}
