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
            AiRequirement::Backend => "ИИ бэкенд недоступен.",
            AiRequirement::Torch => {
                // Torch lives in the backend: if the backend itself is unreachable,
                // that is the more specific (root) cause to report.
                if caps.backend == Some(true) {
                    "Требуется PyTorch."
                } else {
                    "ИИ бэкенд недоступен."
                }
            }
            AiRequirement::Onnx => "Требуется onnxruntime.",
            AiRequirement::TorchOrOnnx => "Требуется PyTorch или onnxruntime.",
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

/// Paints the pill-shaped marker badge straddling the TOP-RIGHT corner of
/// `button_rect`. Painter-only: it allocates no rect and never interacts, so it
/// cannot affect the button hitbox. The badge is placed so its vertical center sits
/// on the button's top edge (half above / half below the border) and the center of
/// its right rounded end sits exactly on the top-right corner. `enabled` selects a
/// muted foreground when the button is disabled so the badge reads as inactive too.
/// Colours are derived from the current visuals so the badge stays legible in both
/// light and dark themes.
fn paint_marker_badge(ui: &egui::Ui, button_rect: egui::Rect, label: &str, enabled: bool) {
    // Honor "3x smaller than the button text", clamped so the badge stays legible.
    let base = egui::TextStyle::Button.resolve(ui.style()).size;
    let font = egui::FontId::proportional((base / 3.0).max(6.0));

    let badge_bg = ui.visuals().widgets.active.bg_fill;
    let badge_fg = if enabled {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };

    let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.to_string(), font, badge_fg));

    // Small inner padding around the text; pill ends use a corner radius of half the
    // badge height so each end is a semicircle.
    let pad = egui::vec2(3.0, 1.0);
    let badge_size = galley.size() + pad * 2.0;
    let radius = badge_size.y / 2.0;

    // Anchor: the right end's semicircle center (at max.x - radius) lands on the
    // button's top-right corner, and the badge's vertical center lands on the top
    // edge (so it straddles the border).
    let badge_max_x = button_rect.right() + radius;
    let badge_min = egui::pos2(
        badge_max_x - badge_size.x,
        button_rect.top() - badge_size.y / 2.0,
    );
    let badge_rect = egui::Rect::from_min_size(badge_min, badge_size);

    // f32 -> u8 corner radius: `radius` is a small, non-negative half-height that
    // fits u8; clamp guards the conversion (no lossless integer alternative exists).
    let corner = egui::CornerRadius::same(radius.round().clamp(0.0, f32::from(u8::MAX)) as u8);
    ui.painter().rect_filled(badge_rect, corner, badge_bg);
    ui.painter().galley(badge_rect.min + pad, galley, badge_fg);
}

#[cfg(test)]
mod tests {
    use super::{AiCaps, AiRequirement};

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
