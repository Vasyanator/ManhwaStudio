/*
File: tabs/ps_editor/correction/model.rs

Purpose:
The data model and ALL the maths of the PS editor's VIEW-ONLY «Коррекция». Deliberately free of
egui and of glow: this file is the one place the correction's numbers are defined, and it is unit
tested. The shader in `gpu.rs` mirrors `apply_channel` line for line, and the panel in `ui.rs`
edits `Correction` through it.

Key structures:
- `CorrectionKind`: which correction the «Настройка» section is showing («Нет» / «Яркость уровня»).
- `Correction`: the parameter payload of one correction.
- `CorrectionState`: the pairing of the two, as the tab stores it.
- `ColorFilterUniforms`: the two floats the GPU filter is driven by.

Key functions:
- `Correction::uniforms`: folds the parameters into `{ gain, bias }`.
- `apply_channel`: the reference implementation of what the fragment shader computes.

Notes:
Nothing here reaches layer pixels, `layers.json`, `CleanOverlaysModel` or the saved project. The
correction is a property of the VIEW, exactly like «Сглаживание» and «Сетка пикселей».
*/


/// Lower bound of every «Коррекция» parameter. `0.0` is always the neutral value.
pub const PARAM_MIN: f32 = -100.0;
/// Upper bound of every «Коррекция» parameter.
pub const PARAM_MAX: f32 = 100.0;

/// Full-scale fraction one end of the brightness range shifts a channel by.
///
/// `±100` brightness moves a channel by `±0.25` of the 0..1 range — a visible but non-destructive
/// swing, chosen so that the control stays usable for "spot the small colour difference" work
/// instead of blowing the page to black or white halfway through its travel.
const BRIGHTNESS_FULL_SCALE_SHIFT: f32 = 0.25;

/// The two uniforms the GPU filter is driven by: `out = clamp(gain * c + bias, 0, 1)` per colour
/// channel, alpha untouched.
///
/// Gamma-encoded space, because that is the space egui's own shader composites in
/// (`egui_glow-0.35.0/src/shader/fragment.glsl:52`) and therefore the space the canvas is already
/// in when the filter reads it back out of the framebuffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorFilterUniforms {
    /// Multiplier applied to every colour channel.
    pub gain: f32,
    /// Offset added after the multiply. Negative for a contrast boost — which is precisely why the
    /// filter cannot be expressed as an egui vertex tint (see this module's `MODULE_README.md`).
    pub bias: f32,
}

impl ColorFilterUniforms {
    /// The pass-through filter: every channel survives unchanged.
    pub const IDENTITY: Self = Self {
        gain: 1.0,
        bias: 0.0,
    };

    /// Whether these uniforms leave every channel exactly as it was, so the GPU pass can be skipped
    /// entirely and the zero-cost default path stays byte-identical to having no correction at all.
    #[must_use]
    pub fn is_identity(self) -> bool {
        self.gain == Self::IDENTITY.gain && self.bias == Self::IDENTITY.bias
    }
}

/// Which correction the «Настройка» section is currently showing.
///
/// A project-owned enum: every `match` on it is exhaustive, so a new correction added later cannot
/// be forgotten at any of its sites (`AGENTS.md` §17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CorrectionKind {
    /// No correction: the canvas is shown exactly as it is composited.
    #[default]
    None,
    /// «Яркость уровня» — the user's wording for a brightness/contrast pair.
    BrightnessContrast,
}

impl CorrectionKind {
    /// Every kind, in the order the «Настройка» combo lists them.
    pub const ALL: [Self; 2] = [Self::None, Self::BrightnessContrast];

    /// The localized caption of this kind, as shown in the combo.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::None => t!("ps_editor.correction.kind_none"),
            Self::BrightnessContrast => t!("ps_editor.correction.kind_brightness_level"),
        }
    }
}

/// The parameter payload of one correction.
///
/// Split from [`CorrectionKind`] so that the parameter card (`ui.rs::correction_card_controls`) can
/// be handed a `&mut Correction` that belongs to any host — the «Настройка» section today, a preset
/// of its own later — without the card knowing which host it draws for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Correction {
    /// Brightness and contrast, both in `PARAM_MIN..=PARAM_MAX`, both neutral at `0.0`.
    BrightnessContrast {
        /// Additive lift, `-100..=100`. `±100` shifts a channel by `±BRIGHTNESS_FULL_SCALE_SHIFT`.
        brightness: f32,
        /// Contrast around mid-grey, `-100..=100`. `+100` doubles the slope, `-100` halves it.
        contrast: f32,
    },
}

impl Default for Correction {
    /// The neutral correction: both parameters at `0.0`, i.e. exactly [`ColorFilterUniforms::IDENTITY`].
    fn default() -> Self {
        Self::BrightnessContrast {
            brightness: 0.0,
            contrast: 0.0,
        }
    }
}

impl Correction {
    /// Folds this correction's parameters into the two uniforms the GPU filter runs on.
    ///
    /// The model is the LEGACY-style linear brightness/contrast, not Photoshop's modern CS3+
    /// highlight-preserving curve, and that is a deliberate choice: the linear form collapses to
    /// `out = clamp(gain * c + bias, 0, 1)`, so the fragment shader is a one-liner that cannot
    /// drift away from the Rust the unit tests below cover. A piecewise curve would move the real
    /// maths into GLSL, where nothing in this repository can test it. The correction is a VIEWING
    /// AID for spotting small colour differences, not a photo-editing operation, so the extra
    /// fidelity would buy nothing and cost testability.
    ///
    /// Operates on GAMMA-ENCODED values (the space the canvas is already composited in) and leaves
    /// alpha untouched. Neutral parameters yield exactly `gain == 1.0, bias == 0.0`.
    #[must_use]
    pub fn uniforms(self) -> ColorFilterUniforms {
        match self {
            Self::BrightnessContrast {
                brightness,
                contrast,
            } => {
                let contrast = contrast.clamp(PARAM_MIN, PARAM_MAX);
                let brightness = brightness.clamp(PARAM_MIN, PARAM_MAX);
                // Multiplicatively symmetric around 1.0: +100 doubles the slope, -100 halves it,
                // so dragging the same distance either way is the same amount of change.
                let gain = if contrast >= 0.0 {
                    1.0 + contrast / PARAM_MAX
                } else {
                    1.0 / (1.0 - contrast / PARAM_MAX)
                };
                let brightness_offset = brightness / PARAM_MAX * BRIGHTNESS_FULL_SCALE_SHIFT;
                // `0.5 - 0.5 * gain` is what pivots the contrast on mid-grey: it is the offset that
                // keeps `c == 0.5` fixed for any gain.
                ColorFilterUniforms {
                    gain,
                    bias: 0.5 - 0.5 * gain + brightness_offset,
                }
            }
        }
    }
}

/// What the «Коррекция» panel holds: the selected kind plus the parameters of the «Настройка»
/// section.
///
/// The payload survives a switch to [`CorrectionKind::None`] and back, so toggling the correction
/// off to compare with the original does not throw the user's settings away.
#[derive(Debug, Clone, Copy, Default)]
pub struct CorrectionState {
    /// The kind the «Настройка» combo shows. [`CorrectionKind::None`] means "draw nothing extra".
    pub kind: CorrectionKind,
    /// The parameters the «Настройка» card edits.
    pub correction: Correction,
}

impl CorrectionState {
    /// The uniforms the canvas pass must run this frame, or `None` when there is nothing to do.
    ///
    /// `None` covers both "«Нет» is selected" and "the parameters are neutral", so an untouched
    /// panel costs the canvas exactly nothing.
    #[must_use]
    pub fn active_uniforms(self) -> Option<ColorFilterUniforms> {
        match self.kind {
            CorrectionKind::None => None,
            CorrectionKind::BrightnessContrast => {
                let uniforms = self.correction.uniforms();
                (!uniforms.is_identity()).then_some(uniforms)
            }
        }
    }
}

/// The reference implementation of the fragment shader's per-channel maths.
///
/// `c` is one gamma-encoded colour channel in `0..=1`; the result is clamped back into that range.
/// `gpu.rs`'s GLSL mirrors this expression line for line, so the unit tests below are what pins the
/// shader's behaviour.
///
/// Test-only on purpose: the shipped binary never evaluates the correction on the CPU (the GPU
/// does), so compiling this into the product would be dead code. Its whole job is to give the
/// tests an executable statement of what the shader must compute.
#[cfg(test)]
#[must_use]
fn apply_channel(uniforms: ColorFilterUniforms, c: f32) -> f32 {
    (uniforms.gain * c + uniforms.bias).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default correction must be an EXACT identity: `active_uniforms` skips the GPU pass on
    /// that comparison, so a value merely close to neutral would make an untouched panel pay for a
    /// framebuffer copy and a quad every frame.
    #[test]
    fn neutral_parameters_are_exactly_the_identity() {
        let uniforms = Correction::default().uniforms();
        assert_eq!(uniforms.gain, 1.0);
        assert_eq!(uniforms.bias, 0.0);
        assert!(uniforms.is_identity());
        assert!(
            CorrectionState {
                kind: CorrectionKind::BrightnessContrast,
                correction: Correction::default(),
            }
            .active_uniforms()
            .is_none()
        );
    }

    /// «Нет» never runs the pass, however the parameters are set.
    #[test]
    fn the_none_kind_never_produces_uniforms() {
        let state = CorrectionState {
            kind: CorrectionKind::None,
            correction: Correction::BrightnessContrast {
                brightness: 100.0,
                contrast: -100.0,
            },
        };
        assert!(state.active_uniforms().is_none());
    }

    /// The contrast range is multiplicatively symmetric: its ends are exactly 2.0 and 0.5.
    #[test]
    fn the_contrast_extremes_are_half_and_double_gain() {
        let max = Correction::BrightnessContrast {
            brightness: 0.0,
            contrast: PARAM_MAX,
        }
        .uniforms();
        let min = Correction::BrightnessContrast {
            brightness: 0.0,
            contrast: PARAM_MIN,
        }
        .uniforms();
        assert_eq!(max.gain, 2.0);
        assert_eq!(min.gain, 0.5);
        assert!((max.gain * min.gain - 1.0).abs() < 1e-6, "reciprocal ends");
    }

    /// Mid-grey is the pivot: at zero brightness a 0.5 channel stays 0.5 at ANY contrast, so a
    /// contrast drag opens the image up around its middle instead of also shifting its exposure.
    #[test]
    fn mid_grey_is_the_contrast_pivot() {
        for contrast in [-100.0_f32, -50.0, -1.0, 0.0, 1.0, 50.0, 100.0] {
            let uniforms = Correction::BrightnessContrast {
                brightness: 0.0,
                contrast,
            }
            .uniforms();
            assert_eq!(
                apply_channel(uniforms, 0.5),
                0.5,
                "contrast {contrast} moved the pivot"
            );
        }
    }

    /// Brightness alone is a pure offset: the gain stays 1.0 and every channel shifts by the same
    /// amount, up to the full-scale swing at the ends of the range.
    #[test]
    fn brightness_shifts_without_changing_the_gain() {
        for (brightness, expected) in [
            (PARAM_MAX, BRIGHTNESS_FULL_SCALE_SHIFT),
            (50.0, BRIGHTNESS_FULL_SCALE_SHIFT / 2.0),
            (PARAM_MIN, -BRIGHTNESS_FULL_SCALE_SHIFT),
        ] {
            let uniforms = Correction::BrightnessContrast {
                brightness,
                contrast: 0.0,
            }
            .uniforms();
            assert_eq!(uniforms.gain, 1.0, "brightness must not change the gain");
            assert!((uniforms.bias - expected).abs() < 1e-6);
            assert!((apply_channel(uniforms, 0.4) - (0.4 + expected)).abs() < 1e-6);
        }
    }

    /// The shader clamps at both ends, so a channel driven past the range saturates instead of
    /// wrapping — the reference implementation must do the same or the tests would pin the wrong
    /// behaviour.
    #[test]
    fn apply_channel_clamps_at_both_ends() {
        let bright = Correction::BrightnessContrast {
            brightness: PARAM_MAX,
            contrast: PARAM_MAX,
        }
        .uniforms();
        let dark = Correction::BrightnessContrast {
            brightness: PARAM_MIN,
            contrast: PARAM_MAX,
        }
        .uniforms();
        assert_eq!(apply_channel(bright, 1.0), 1.0);
        assert_eq!(apply_channel(dark, 0.0), 0.0);
        for c in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let value = apply_channel(bright, c);
            assert!((0.0..=1.0).contains(&value), "{value} left the unit range");
        }
    }

    /// Out-of-range parameters are clamped rather than extrapolated: the panel bounds its sliders,
    /// but the model is a public contract and must not produce a wilder filter than its own range.
    #[test]
    fn parameters_outside_the_range_are_clamped() {
        let wild = Correction::BrightnessContrast {
            brightness: 1_000.0,
            contrast: 1_000.0,
        }
        .uniforms();
        let edge = Correction::BrightnessContrast {
            brightness: PARAM_MAX,
            contrast: PARAM_MAX,
        }
        .uniforms();
        assert_eq!(wild, edge);
    }

    /// Every kind the combo lists has a caption, and no two kinds share one.
    #[test]
    fn every_kind_has_its_own_caption() {
        let _guard = crate::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

        let titles: Vec<&'static str> = CorrectionKind::ALL.iter().map(|k| k.title()).collect();
        assert_eq!(titles.len(), CorrectionKind::ALL.len());
        for title in &titles {
            assert!(!title.trim().is_empty(), "a kind caption is empty");
        }
        assert_ne!(titles[0], titles[1], "two kinds share a caption");
    }
}
