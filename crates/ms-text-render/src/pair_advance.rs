/*
File: crates/ms-text-render/src/pair_advance.rs

Purpose:
The ONE owner of "the advance between two adjacent glyphs": whether a pair is overridden by
a user-authored kerning pair (guard), the overridden magnitude, the `KerningMode::Fixed`
own-advance step, how an unsigned magnitude is oriented onto the run axis, and the tracking
basis each layout path measures manual tracking against.

Contract:
The pair DECISION lives here; accumulation loops live with their consumers (the horizontal
pen in `pipeline.rs`, the formula seed advances in `formula/render.rs`, the vertical column
stacking in `layout/vertical.rs`). Optical normalization and faux-bold growth are additive
per-path terms on top of a `PairStep`, never part of it. Every expression is the verbatim
arithmetic the consumers used before this module existed (bit-exact: pinned by the recorded
characterization matrices, `src/characterization.rs`); do not "simplify" operand order,
`abs` placement or `max` chains.

Key items:
- `KerningSettings` (+ `effective_spacing_percent`): a glyph's resolved kerning mode and
  manual tracking, and the byte-identical fast-path predicate `uses_default_metric_layout`.
- `single_char_cluster`: the single `char` of a glyph's source cluster (the cluster guard).
- `PairGlyph`: one glyph as the owner sees it (glyph + its single source char, if any).
- `PairAdvanceModel`: a `Copy` view over the render's face-keyed `CustomKerningMap`;
  `override_delta_px` / `is_overridden` (the guard) and `resolve` (the decided `PairStep`).
- `StepDirection`: signed run-axis step (`FollowRun`) vs unsigned path distance (`Magnitude`).
- `RunOrientation`: which way a run-axis step walks, which glyph's advance spans it (the
  VISUALLY LEFT one), and how an unsigned magnitude is signed onto the axis.
- `TrackingBasis`: the four historical tracking bases, gathered in one place.

Notes:
- RIGHT-TO-LEFT: on a run axis every step (override, `Fixed`, and the optical step the pen
  builds through `RunOrientation`) is anchored on the advance of the glyph that physically
  spans it and walks in the run's direction (`RunOrientation`).
- PRESERVED DIVERGENCE (pinned by a test in `pipeline.rs`): the four tracking bases differ by
  path (`TrackingBasis`).
- GUI-free, no I/O, no logging; never panics (a glyph whose font is gone falls back to its
  shaped step).
*/

use crate::font_provider::CustomKerningTable;
use crate::font_registry::CustomKerningMap;
use crate::glyph_blit::nominal_glyph_advance_px;
use crate::optical::optical_base_advance;
use crate::types::{KerningMode, TextRenderParams};
use cosmic_text::{FontSystem, LayoutGlyph};

/// A glyph's resolved kerning mode and manual tracking, as every layout path consumes it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KerningSettings {
    pub(crate) mode: KerningMode,
    pub(crate) spacing_px: f32,
    pub(crate) spacing_percent: f32,
    /// Whether the FONT this glyph was drawn from carries user-authored kerning
    /// pair overrides (`font_provider::CustomKerningTable`). Set per glyph by
    /// [`Self::with_custom_pairs`] from the render's `CustomKerningMap`; `false`
    /// on every path that does not apply pair overrides at all (the vertical
    /// layout, see the MODULE_README).
    ///
    /// It exists only to take the run OFF the byte-identical shaped-position fast
    /// path — see [`Self::uses_default_metric_layout`]. Whether an individual PAIR
    /// is actually overridden is decided by [`PairAdvanceModel`], not here.
    pub(crate) custom_pairs: bool,
}

impl KerningSettings {
    /// The block-level settings of `params`: mode, tracking px clamped to `[-300, 300]`, and
    /// tracking percent folded with the glyph width scale (`effective_spacing_percent`).
    #[must_use]
    pub(crate) fn from_params(params: &TextRenderParams) -> Self {
        Self {
            mode: params.kerning_mode,
            spacing_px: params.kerning_px.clamp(-300.0, 300.0),
            spacing_percent: effective_spacing_percent(
                params.kerning_percent,
                params.glyph_width_percent,
            ),
            custom_pairs: false,
        }
    }

    /// Same settings with the custom-override flag set to `active`.
    ///
    /// Applied by the horizontal and formula paths right after
    /// `pipeline::inline_kerning_for_glyph`, from `CustomKerningMap::has_table` for the
    /// glyph's own face. Kept a separate step rather than a parameter so the
    /// vertical path — which applies no pair kerning of any kind — keeps calling
    /// the resolver unchanged.
    #[must_use]
    pub(crate) fn with_custom_pairs(mut self, active: bool) -> Self {
        self.custom_pairs = active;
        self
    }

    /// Whether neither tracking term moves anything.
    #[must_use]
    fn has_zero_adjustment(self) -> bool {
        self.spacing_px.abs() <= f32::EPSILON && self.spacing_percent.abs() <= f32::EPSILON
    }

    /// Manual tracking in px added to one step: the px term plus the percent term of
    /// `basis_px` (negative bases count as 0). Which basis a path passes is
    /// [`TrackingBasis`]'s decision; prefer [`Self::tracking_px`].
    #[must_use]
    pub(crate) fn extra_spacing_px(self, basis_px: f32) -> f32 {
        self.spacing_px + basis_px.max(0.0) * (self.spacing_percent / 100.0)
    }

    /// Manual tracking in px for one step measured against `basis` — exactly
    /// `self.extra_spacing_px(basis.basis_px())`.
    #[must_use]
    pub(crate) fn tracking_px(self, basis: TrackingBasis) -> f32 {
        self.extra_spacing_px(basis.basis_px())
    }

    /// Whether this glyph may take the fast byte-identical shaped-position path
    /// (cosmic-text `Shaping::Advanced` positions with no manual tracking). Only
    /// `Auto` qualifies: `Fixed` needs own-advance repositioning and `Optical`
    /// needs ink-gap normalization, so both must go through the custom path.
    ///
    /// A font carrying user-authored kerning overrides (`custom_pairs`) is
    /// disqualified for the same reason: the shaped positions already contain the
    /// font's OWN pair kerning, and an override REPLACES it — taking the shortcut
    /// would silently render the overrides as if they did not exist.
    #[must_use]
    pub(crate) fn uses_default_metric_layout(self) -> bool {
        self.mode == KerningMode::Auto && self.has_zero_adjustment() && !self.custom_pairs
    }
}

/// `base_percent` folded with a glyph scale given in percent (`glyph_percent`, 100 = no
/// scale), clamped to `[-300, 300]`. Shared by tracking (glyph WIDTH scale) and line spacing
/// (glyph HEIGHT scale).
#[must_use]
pub(crate) fn effective_spacing_percent(base_percent: f32, glyph_percent: f32) -> f32 {
    (base_percent + (glyph_percent - 100.0)).clamp(-300.0, 300.0)
}

/// The single `char` of a glyph's source cluster, or `None` when the cluster is
/// not exactly one character.
///
/// `glyph.start`/`glyph.end` are byte offsets into the run's own text, so the
/// slice is the cluster the shaper folded into this glyph. A multi-character
/// cluster (a ligature, a base + combining mark, a surrogate-free emoji sequence)
/// deliberately yields `None`: user-authored kerning pairs are entered as exactly
/// TWO characters in the settings UI, so a cluster pair has no authored meaning and
/// guessing one would kern text the user never described.
///
/// `get` rather than indexing: the offsets come from the shaper, and an
/// out-of-range or non-boundary pair must degrade to "no override", never panic.
#[must_use]
pub(crate) fn single_char_cluster(run_text: &str, glyph: &LayoutGlyph) -> Option<char> {
    let start = glyph.start.min(glyph.end);
    let end = glyph.start.max(glyph.end);
    let mut chars = run_text.get(start..end)?.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// One glyph as the owner sees it: the shaped glyph plus the single `char` of its source
/// cluster (`None` for a multi-char cluster or a synthesized glyph).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PairGlyph<'a> {
    pub(crate) glyph: &'a LayoutGlyph,
    pub(crate) cluster_char: Option<char>,
}

impl<'a> PairGlyph<'a> {
    /// A glyph still attached to its `LayoutRun`: the cluster char is read from `run_text`
    /// through [`single_char_cluster`].
    #[must_use]
    pub(crate) fn from_run(run_text: &str, glyph: &'a LayoutGlyph) -> Self {
        Self {
            glyph,
            cluster_char: single_char_cluster(run_text, glyph),
        }
    }

    /// A glyph detached from its run (a formula seed), whose cluster char was captured at
    /// seed time (`FormulaGlyphSeed::cluster_char`).
    #[must_use]
    pub(crate) fn detached(glyph: &'a LayoutGlyph, cluster_char: Option<char>) -> Self {
        Self {
            glyph,
            cluster_char,
        }
    }
}

/// Orientation of a pair step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepDirection {
    /// A signed x step along a shaped run (the horizontal pen). Override and `Fixed`
    /// magnitudes are anchored on the advance of the glyph that spans the step and signed
    /// onto the run axis, both through [`RunOrientation`].
    FollowRun,
    /// An unsigned distance along a drawn path (formula / drawn lines, LTR by construction):
    /// nothing is mirrored and the shaped step is used verbatim, never through `abs`.
    Magnitude,
}

/// The decided base step of one pair, BEFORE tracking, faux growth and optical normalization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PairStep {
    /// A user-authored pair REPLACES the font's own value: `step_px` is the left glyph's raw
    /// `hmtx` advance (falling back to the shaped step) plus `delta_px`, oriented per
    /// [`StepDirection`]. Same under every `KerningMode`.
    Override { step_px: f32, delta_px: f32 },
    /// `KerningMode::Fixed`, no override: the left glyph's own (un-kerned) advance, falling
    /// back to the shaped step; never mirrored.
    OwnAdvance { step_px: f32 },
    /// `Auto`, or `Optical` BEFORE its per-path normalization: the shaped step verbatim.
    Shaped { step_px: f32 },
}

impl PairStep {
    /// The step in px, whichever branch decided it.
    #[must_use]
    pub(crate) fn step_px(self) -> f32 {
        match self {
            Self::Override { step_px, .. } | Self::OwnAdvance { step_px } | Self::Shaped { step_px } => {
                step_px
            }
        }
    }

    /// Whether a user-authored pair decided this step.
    #[must_use]
    pub(crate) fn is_override(self) -> bool {
        match self {
            Self::Override { .. } => true,
            Self::OwnAdvance { .. } | Self::Shaped { .. } => false,
        }
    }
}

/// The owner of the pair decision: a `Copy` view over the render's face-keyed override map.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PairAdvanceModel<'a> {
    custom_kerning: &'a CustomKerningMap,
}

impl<'a> PairAdvanceModel<'a> {
    /// A model over this render's overrides (selected font plus every inline `<font=…>` font).
    #[must_use]
    pub(crate) fn new(custom_kerning: &'a CustomKerningMap) -> Self {
        Self { custom_kerning }
    }

    /// `settings` stamped with whether `glyph`'s own face carries an override table — the
    /// per-glyph flag that takes a run off the byte-identical fast path.
    #[must_use]
    pub(crate) fn kerning_for_glyph(self, settings: KerningSettings, glyph: &LayoutGlyph) -> KerningSettings {
        settings.with_custom_pairs(self.custom_kerning.has_table(glyph.font_id))
    }

    /// The user-authored advance delta in px for the pair (`prev`, `cur`), or `None` when no
    /// override applies.
    ///
    /// Conservative by contract — ALL of the following must hold, and each guard has a
    /// reason that is easy to "simplify" away:
    /// - both glyphs come from the SAME face (`font_id`). A pair that straddles a
    ///   font-fallback boundary is not a kerning pair in ANY font: the two characters
    ///   were never adjacent in one typeface's design, and the override was authored
    ///   against one specific font entry.
    /// - that face carries an override table at all.
    /// - each glyph's source cluster is exactly one `char` ([`single_char_cluster`]).
    ///
    /// The em basis is the LEFT glyph's own `font_size`, not the block font size, so an
    /// inline `<size=…>` span kerns proportionally to the text it is part of.
    /// `Some(0.0)` is NOT `None`: it cancels the font's own pair value.
    #[must_use]
    pub(crate) fn override_delta_px(self, prev: PairGlyph<'_>, cur: PairGlyph<'_>) -> Option<f32> {
        let (table, left, right) = self.guarded_pair(prev, cur)?;
        table.delta_px(left, right, prev.glyph.font_size)
    }

    /// The same override as [`Self::override_delta_px`], in THOUSANDTHS OF AN EM (the
    /// table's own unit), for a width metric that measures at 1000 units per em
    /// (`wrap::forms::GlyphWidths`). Same guard, so the metric and the pen agree on WHICH
    /// pairs are overridden.
    #[must_use]
    pub(crate) fn override_per_mille(self, prev: PairGlyph<'_>, cur: PairGlyph<'_>) -> Option<f32> {
        let (table, left, right) = self.guarded_pair(prev, cur)?;
        table.delta_per_mille(left, right)
    }

    /// The override guard: the table and the two characters when both glyphs come from the
    /// same face, that face has a table and both clusters are single chars; else `None`.
    fn guarded_pair(self, prev: PairGlyph<'_>, cur: PairGlyph<'_>) -> Option<(&'a CustomKerningTable, char, char)> {
        if prev.glyph.font_id != cur.glyph.font_id {
            return None;
        }
        let table = self.custom_kerning.table_for(prev.glyph.font_id)?;
        Some((table, prev.cluster_char?, cur.cluster_char?))
    }


    /// Whether a user-authored pair applies to (`prev`, `cur`) — [`Self::override_delta_px`]
    /// as a boolean, for callers that only exempt an overridden pair from a normalization.
    #[must_use]
    pub(crate) fn is_overridden(self, prev: PairGlyph<'_>, cur: PairGlyph<'_>) -> bool {
        self.override_delta_px(prev, cur).is_some()
    }

    /// Decides the base step of the pair (`prev`, `cur`).
    ///
    /// `shaped_step_px` is the caller's starting step: `cur.x - prev.x` (signed) on the pen,
    /// the floored `seed_metric_advance_px(..)` on formula seeds. An override wins under
    /// every mode; otherwise `Fixed` steps by the spanning glyph's own advance and `Auto` /
    /// `Optical` keep the shaped step (the optical callers add their own normalization).
    /// Under `FollowRun` the override / `Fixed` magnitude is anchored on the glyph that spans
    /// the step and signed onto the run axis ([`RunOrientation`]); under `Magnitude` it is
    /// anchored on `prev` and never signed.
    /// `font_system` is read only for an override or `Fixed` (the `hmtx` advance); a glyph
    /// whose font is gone falls back to the shaped step. Never panics.
    #[must_use]
    pub(crate) fn resolve(
        self,
        font_system: &mut FontSystem,
        mode: KerningMode,
        direction: StepDirection,
        prev: PairGlyph<'_>,
        cur: PairGlyph<'_>,
        shaped_step_px: f32,
    ) -> PairStep {
        let delta_px = self.override_delta_px(prev, cur);
        let unsigned_step = match (delta_px, mode) {
            (Some(_), _) | (None, KerningMode::Fixed) => true,
            (None, KerningMode::Auto | KerningMode::Optical) => false,
        };
        if !unsigned_step {
            return PairStep::Shaped {
                step_px: shaped_step_px,
            };
        }
        // An override, or `Fixed` (no override): both step by a glyph's OWN nominal
        // (un-kerned) `hmtx` advance — cosmic-text bakes pair kerning into the shaped
        // advance, so only the raw metrics table drops it — plus the authored delta for an
        // override. The `hmtx` advance is UNSIGNED, so on a run axis the shaped step is
        // folded in only as an unsigned FALLBACK magnitude and the orientation is applied
        // once at the end; a path distance is unsigned already and used verbatim.
        let (anchor, fallback_px, orientation) = match direction {
            StepDirection::FollowRun => {
                let orientation = RunOrientation::of_step(shaped_step_px, prev.glyph);
                (orientation.spanning_glyph(prev.glyph, cur.glyph), shaped_step_px.abs(), Some(orientation))
            }
            StepDirection::Magnitude => (prev.glyph, shaped_step_px, None),
        };
        let own_px = nominal_glyph_advance_px(font_system, anchor).unwrap_or(fallback_px);
        let base_px = optical_base_advance(own_px, fallback_px);
        let signed = |magnitude_px: f32| orientation.map_or(magnitude_px, |orientation| orientation.sign(magnitude_px));
        match delta_px {
            Some(delta_px) => PairStep::Override {
                step_px: signed(base_px + delta_px),
                delta_px,
            },
            None => PairStep::OwnAdvance {
                step_px: signed(base_px),
            },
        }
    }
}

/// Which way a step along a shaped run walks.
///
/// cosmic-text lays a right-to-left run out by SUBTRACTING each (positive) shaped advance,
/// so `glyph.x` decreases with the glyph index and the next glyph lands at
/// `cur.x = prev.x - cur.w` (`cosmic-text/src/shape.rs`, `ShapeLine::layout_to_buffer`).
/// Two facts follow, and every run-axis step must honour both: the step is NEGATIVE, and the
/// glyph whose advance spans it is the visually LEFT one, i.e. `cur` — not `prev` as in a
/// left-to-right run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunOrientation {
    LeftToRight,
    RightToLeft,
}

impl RunOrientation {
    /// The orientation of the step `prev -> cur` whose shaped length is `shaped_step_px`.
    ///
    /// The shaped step is the primary evidence (it IS the direction the shaper walked); a
    /// degenerate one (zero or non-finite, e.g. a zero-width glyph) carries no sign, so the
    /// bidi level of `prev` decides instead.
    #[must_use]
    pub(crate) fn of_step(shaped_step_px: f32, prev: &LayoutGlyph) -> Self {
        let rtl = if shaped_step_px.is_finite() && shaped_step_px != 0.0 {
            shaped_step_px < 0.0
        } else {
            prev.level.is_rtl()
        };
        if rtl { Self::RightToLeft } else { Self::LeftToRight }
    }

    /// The glyph whose advance spans the step: `prev` left-to-right, `cur` right-to-left.
    #[must_use]
    pub(crate) fn spanning_glyph<'g>(self, prev: &'g LayoutGlyph, cur: &'g LayoutGlyph) -> &'g LayoutGlyph {
        match self {
            Self::LeftToRight => prev,
            Self::RightToLeft => cur,
        }
    }

    /// `magnitude_px` signed onto the run axis (negated right-to-left), so a positive
    /// magnitude always moves the pen in the run's direction and a negative authored delta
    /// TIGHTENS a pair either way.
    #[must_use]
    pub(crate) fn sign(self, magnitude_px: f32) -> f32 {
        match self {
            Self::LeftToRight => magnitude_px,
            Self::RightToLeft => -magnitude_px,
        }
    }
}

/// The basis a path measures PERCENT tracking against, one variant per historical rule.
///
/// KNOWN DIVERGENCE, preserved on purpose (behaviour change B3 was declined): the four
/// layout paths track against four different bases, and gathering them here makes the
/// difference visible in one place instead of hiding it in four loops.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TrackingBasis {
    /// The horizontal pen (metric and optical branches): `|shaped step|`, floored at the left
    /// glyph's shaped advance and at `default_advance_px` (half the em).
    HorizontalPen { shaped_step_px: f32, prev_w_px: f32, default_advance_px: f32 },
    /// A formula seed whose step was a metric one (override, `Auto`, `Fixed`): the floored
    /// seed metric advance itself.
    SeedMetric { metric_advance_px: f32 },
    /// A formula seed under `Optical` without override: the mean ink width of the pair,
    /// floored at `default_advance_px`.
    SeedOpticalInk { mean_ink_width_px: f32, default_advance_px: f32 },
    /// The vertical column stack: the block font size.
    FontSize { font_size_px: f32 },
}

impl TrackingBasis {
    /// The basis in px (verbatim per variant; see the variant docs).
    #[must_use]
    pub(crate) fn basis_px(self) -> f32 {
        match self {
            Self::HorizontalPen {
                shaped_step_px,
                prev_w_px,
                default_advance_px,
            } => shaped_step_px.abs().max(prev_w_px.max(default_advance_px)),
            Self::SeedMetric { metric_advance_px } => metric_advance_px,
            Self::SeedOpticalInk {
                mean_ink_width_px,
                default_advance_px,
            } => mean_ink_width_px.max(default_advance_px),
            Self::FontSize { font_size_px } => font_size_px,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KerningSettings, PairAdvanceModel, PairGlyph, PairStep, StepDirection, TrackingBasis,
        single_char_cluster,
    };
    use crate::characterization::TrackedFont;
    use crate::font_provider::CustomKerningTable;
    use crate::font_registry::CustomKerningMap;
    use crate::glyph_blit::nominal_glyph_advance_px;
    use crate::types::KerningMode;
    use cosmic_text::{
        Attrs, Buffer, Family, FontSystem, LayoutGlyph, Metrics, Shaping, fontdb,
    };
    use std::sync::Arc;

    /// One `FontSystem` holding BOTH tracked fixture faces (Latin, Hebrew), so a test can
    /// build a pair across two faces of one database.
    struct TwoFaces {
        font_system: FontSystem,
        latin: (fontdb::ID, String),
        hebrew: (fontdb::ID, String),
    }

    fn two_faces() -> TwoFaces {
        let mut db = fontdb::Database::new();
        db.load_font_data(TrackedFont::Latin.bytes());
        db.load_font_data(TrackedFont::Hebrew.bytes());
        let faces = db
            .faces()
            .map(|face| {
                let family = face.families.first().map(|(name, _)| name.clone()).expect("family");
                (face.id, family)
            })
            .collect::<Vec<_>>();
        assert_eq!(faces.len(), 2, "both tracked fixture files register one face each");
        let font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);
        TwoFaces {
            font_system,
            latin: faces[0].clone(),
            hebrew: faces[1].clone(),
        }
    }

    /// Shapes `text` in `family` at `em` as one line; returns the run's glyphs and text.
    fn shape(font_system: &mut FontSystem, family: &str, text: &str, em: f32) -> (Vec<LayoutGlyph>, String) {
        let mut buffer = Buffer::new(font_system, Metrics::new(em, em * 1.2));
        buffer.set_size(font_system, None, None);
        let attrs = Attrs::new().family(Family::Name(family)).metrics(Metrics::new(em, em));
        buffer.set_text(font_system, text, &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(font_system, false);
        let run = buffer.layout_runs().next().expect("one layout run");
        (run.glyphs.to_vec(), run.text.to_string())
    }

    fn map_with(face: fontdb::ID, pairs: &[(char, char, f32)]) -> CustomKerningMap {
        let mut map = CustomKerningMap::default();
        map.insert(face, Arc::new(CustomKerningTable::from_pairs(pairs.iter().copied())), "test");
        map
    }

    /// A `FontSystem` with no faces at all: every `hmtx` read returns `None`, which is how a
    /// test proves a branch does (or does not) consult the font.
    fn empty_font_system() -> FontSystem {
        FontSystem::new_with_locale_and_db("en-US".to_string(), fontdb::Database::new())
    }

    /// The guard matrix: an override needs the same face on both sides, a table on that face,
    /// and a single-char cluster on both sides; `Some(0.0)` is an override, not `None`.
    #[test]
    fn the_override_guard_requires_one_face_a_table_and_single_char_clusters() {
        let mut faces = two_faces();
        let (latin_id, latin_family) = faces.latin.clone();
        let (glyphs, text) = shape(&mut faces.font_system, &latin_family, "AV", 100.0);
        assert_eq!(glyphs.len(), 2);
        let prev = PairGlyph::from_run(&text, &glyphs[0]);
        let cur = PairGlyph::from_run(&text, &glyphs[1]);
        assert_eq!((prev.cluster_char, cur.cluster_char), (Some('A'), Some('V')));

        let tightened = map_with(latin_id, &[('A', 'V', -100.0)]);
        let model = PairAdvanceModel::new(&tightened);
        let delta = model.override_delta_px(prev, cur).expect("a listed pair is overridden");
        assert!((delta + 10.0).abs() < 1e-4, "-100 per mille of a 100 px em is -10 px: {delta}");
        assert!(model.is_overridden(prev, cur));

        // No table at all.
        let empty = CustomKerningMap::default();
        assert_eq!(PairAdvanceModel::new(&empty).override_delta_px(prev, cur), None);
        // A table on ANOTHER face.
        let elsewhere = map_with(faces.hebrew.0, &[('A', 'V', -100.0)]);
        assert_eq!(PairAdvanceModel::new(&elsewhere).override_delta_px(prev, cur), None);
        // The pair straddles two faces (a fallback boundary).
        let mut foreign = glyphs[1].clone();
        foreign.font_id = faces.hebrew.0;
        assert_eq!(model.override_delta_px(prev, PairGlyph::detached(&foreign, Some('V'))), None);
        // A multi-char (or synthesized) cluster on either side.
        assert_eq!(model.override_delta_px(PairGlyph::detached(&glyphs[0], None), cur), None);
        assert_eq!(model.override_delta_px(prev, PairGlyph::detached(&glyphs[1], None)), None);
        // `Some(0.0)` cancels the font's own value; it is NOT "no override".
        let cancelled = map_with(latin_id, &[('A', 'V', 0.0)]);
        assert_eq!(PairAdvanceModel::new(&cancelled).override_delta_px(prev, cur), Some(0.0));
        assert!(PairAdvanceModel::new(&cancelled).is_overridden(prev, cur));
    }

    /// An override REPLACES the font's pair value: the step is anchored on the left glyph's
    /// raw `hmtx` advance, so -100 / 0 / +100 per mille sit exactly 10 px apart at em 100
    /// and the `0.0` entry lands on the nominal advance itself — under every mode.
    #[test]
    fn an_override_is_the_nominal_advance_plus_the_authored_delta_in_every_mode() {
        let mut faces = two_faces();
        let (latin_id, latin_family) = faces.latin.clone();
        let (glyphs, text) = shape(&mut faces.font_system, &latin_family, "AV", 100.0);
        let prev = PairGlyph::from_run(&text, &glyphs[0]);
        let cur = PairGlyph::from_run(&text, &glyphs[1]);
        let shaped = glyphs[1].x - glyphs[0].x;
        let nominal = nominal_glyph_advance_px(&mut faces.font_system, &glyphs[0]).expect("hmtx");
        assert!((nominal - shaped).abs() > 1e-2, "the fixture font must kern 'AV' itself");

        for mode in [KerningMode::Auto, KerningMode::Fixed, KerningMode::Optical] {
            let steps = [-100.0, 0.0, 100.0].map(|offset| {
                let map = map_with(latin_id, &[('A', 'V', offset)]);
                PairAdvanceModel::new(&map).resolve(
                    &mut faces.font_system,
                    mode,
                    StepDirection::FollowRun,
                    prev,
                    cur,
                    shaped,
                )
            });
            assert!(steps.iter().all(|step| step.is_override()), "{mode:?}: {steps:?}");
            assert_eq!(steps[1], PairStep::Override { step_px: nominal, delta_px: 0.0 });
            assert!(
                (steps[1].step_px() - steps[0].step_px() - 10.0).abs() < 1e-3
                    && (steps[2].step_px() - steps[1].step_px() - 10.0).abs() < 1e-3,
                "{mode:?}: offsets must sit 10 px apart: {steps:?}"
            );
        }
    }

    /// `FollowRun` signs an override onto the run axis — by the sign of the shaped step, or,
    /// when that step is zero or not finite, by `prev`'s bidi level — and anchors it on the
    /// glyph that SPANS the step: `cur` right-to-left (cosmic-text puts it at
    /// `prev.x - cur.w`), `prev` left-to-right. `Magnitude` never signs and always anchors on
    /// `prev`.
    #[test]
    fn an_override_follows_the_run_axis_only_under_follow_run() {
        let mut faces = two_faces();
        let (hebrew_id, hebrew_family) = faces.hebrew.clone();
        let (glyphs, text) = shape(&mut faces.font_system, &hebrew_family, "\u{05D0}\u{05D1}", 100.0);
        assert_eq!(glyphs.len(), 2);
        assert!(glyphs[0].level.is_rtl(), "precondition: a Hebrew glyph sits on an RTL level");
        let prev = PairGlyph::from_run(&text, &glyphs[0]);
        let cur = PairGlyph::from_run(&text, &glyphs[1]);
        let shaped = glyphs[1].x - glyphs[0].x;
        assert!(shaped < 0.0, "precondition: the RTL run steps leftwards: {shaped}");
        let map = map_with(hebrew_id, &[('\u{05D0}', '\u{05D1}', -100.0)]);
        let model = PairAdvanceModel::new(&map);
        let nominal_prev = nominal_glyph_advance_px(&mut faces.font_system, &glyphs[0]).expect("hmtx");
        let nominal_cur = nominal_glyph_advance_px(&mut faces.font_system, &glyphs[1]).expect("hmtx");
        assert!(
            (nominal_prev - nominal_cur).abs() > 1.0,
            "precondition: alef and bet differ in width, or the anchor is not observable"
        );

        let mut resolve = |direction, step| {
            model
                .resolve(&mut faces.font_system, KerningMode::Auto, direction, prev, cur, step)
                .step_px()
        };
        let rtl = -(nominal_cur - 10.0);
        assert_eq!(resolve(StepDirection::FollowRun, shaped), rtl, "sign of the shaped step");
        assert_eq!(resolve(StepDirection::FollowRun, 0.0), rtl, "bidi level on a zero step");
        assert_eq!(resolve(StepDirection::FollowRun, f32::NAN), rtl, "bidi level on NaN");
        assert_eq!(
            resolve(StepDirection::FollowRun, 7.0),
            nominal_prev - 10.0,
            "a positive step is LTR and anchors on prev"
        );
        assert_eq!(
            resolve(StepDirection::Magnitude, shaped),
            nominal_prev - 10.0,
            "Magnitude never signs and anchors on prev"
        );
    }

    /// `Fixed` without an override follows a right-to-left run exactly like an override:
    /// leftwards, by the own advance of the glyph that spans the step (`cur`). The fixture
    /// font has no alef -> bet kerning, so that is precisely the shaped step.
    #[test]
    fn fixed_without_an_override_follows_an_rtl_run_on_the_spanning_glyph() {
        let mut faces = two_faces();
        let (_, hebrew_family) = faces.hebrew.clone();
        let (glyphs, text) = shape(&mut faces.font_system, &hebrew_family, "\u{05D0}\u{05D1}", 100.0);
        let shaped = glyphs[1].x - glyphs[0].x;
        assert!(shaped < 0.0);
        let empty = CustomKerningMap::default();
        let step = PairAdvanceModel::new(&empty).resolve(
            &mut faces.font_system,
            KerningMode::Fixed,
            StepDirection::FollowRun,
            PairGlyph::from_run(&text, &glyphs[0]),
            PairGlyph::from_run(&text, &glyphs[1]),
            shaped,
        );
        let nominal_cur = nominal_glyph_advance_px(&mut faces.font_system, &glyphs[1]).expect("hmtx");
        assert_eq!(step, PairStep::OwnAdvance { step_px: -nominal_cur });
        assert!(
            (step.step_px() - shaped).abs() < 1e-3,
            "no font kerning: Fixed equals the shaped step: {step:?} vs {shaped}"
        );
        assert!(!step.is_override());
    }

    /// `Auto` and `Optical` (before its normalization) keep the shaped step verbatim and never
    /// read the font; `Fixed` and an override fall back to the shaped step when the font is
    /// gone — the override through `|shaped|` on a run axis, verbatim on a path.
    #[test]
    fn a_missing_font_falls_back_to_the_shaped_step() {
        let mut faces = two_faces();
        let (latin_id, latin_family) = faces.latin.clone();
        let (glyphs, text) = shape(&mut faces.font_system, &latin_family, "AV", 100.0);
        let prev = PairGlyph::from_run(&text, &glyphs[0]);
        let cur = PairGlyph::from_run(&text, &glyphs[1]);
        let mut gone = empty_font_system();

        let empty = CustomKerningMap::default();
        let plain = PairAdvanceModel::new(&empty);
        for mode in [KerningMode::Auto, KerningMode::Optical] {
            assert_eq!(
                plain.resolve(&mut gone, mode, StepDirection::FollowRun, prev, cur, -12.5),
                PairStep::Shaped { step_px: -12.5 }
            );
        }
        assert_eq!(
            plain.resolve(&mut gone, KerningMode::Fixed, StepDirection::FollowRun, prev, cur, 40.0),
            PairStep::OwnAdvance { step_px: 40.0 }
        );
        assert_eq!(
            plain.resolve(&mut gone, KerningMode::Fixed, StepDirection::FollowRun, prev, cur, -40.0),
            PairStep::OwnAdvance { step_px: -40.0 },
            "right-to-left: |shaped| as the magnitude, signed back onto the run axis"
        );

        let map = map_with(latin_id, &[('A', 'V', -100.0)]);
        let model = PairAdvanceModel::new(&map);
        assert_eq!(
            model.resolve(&mut gone, KerningMode::Auto, StepDirection::FollowRun, prev, cur, -40.0),
            PairStep::Override { step_px: -30.0, delta_px: -10.0 },
            "run axis: magnitude |shaped| + delta, oriented by the shaped step"
        );
        assert_eq!(
            model.resolve(&mut gone, KerningMode::Auto, StepDirection::Magnitude, prev, cur, 40.0),
            PairStep::Override { step_px: 30.0, delta_px: -10.0 },
            "path: the shaped distance verbatim + delta"
        );
    }

    /// The four historical tracking bases, verbatim, and `tracking_px` as
    /// `extra_spacing_px(basis_px)`.
    #[test]
    fn each_tracking_basis_keeps_its_historical_rule() {
        let pen = |shaped_step_px, prev_w_px| TrackingBasis::HorizontalPen {
            shaped_step_px,
            prev_w_px,
            default_advance_px: 50.0,
        };
        assert_eq!(pen(-70.0, 60.0).basis_px(), 70.0, "|shaped step| wins when largest");
        assert_eq!(pen(20.0, 60.0).basis_px(), 60.0, "then the left glyph's advance");
        assert_eq!(pen(20.0, 30.0).basis_px(), 50.0, "floored at half the em");
        assert_eq!(TrackingBasis::SeedMetric { metric_advance_px: 33.0 }.basis_px(), 33.0);
        let ink = |mean_ink_width_px| TrackingBasis::SeedOpticalInk {
            mean_ink_width_px,
            default_advance_px: 22.0,
        };
        assert_eq!(ink(40.0).basis_px(), 40.0);
        assert_eq!(ink(10.0).basis_px(), 22.0, "floored at the default advance");
        assert_eq!(TrackingBasis::FontSize { font_size_px: 64.0 }.basis_px(), 64.0);

        let tracking = KerningSettings {
            mode: KerningMode::Auto,
            spacing_px: 2.0,
            spacing_percent: 10.0,
            custom_pairs: false,
        };
        let basis = pen(20.0, 30.0);
        assert_eq!(tracking.tracking_px(basis), tracking.extra_spacing_px(basis.basis_px()));
        assert_eq!(tracking.tracking_px(basis), 7.0);
    }

    /// The per-glyph stamp: a glyph whose face carries a table leaves the fast path.
    #[test]
    fn kerning_for_glyph_stamps_the_faces_own_table() {
        let mut faces = two_faces();
        let (latin_id, latin_family) = faces.latin.clone();
        let (glyphs, _) = shape(&mut faces.font_system, &latin_family, "AV", 40.0);
        let settings = KerningSettings {
            mode: KerningMode::Auto,
            spacing_px: 0.0,
            spacing_percent: 0.0,
            custom_pairs: false,
        };
        let with_table = map_with(latin_id, &[('Q', 'z', -200.0)]);
        assert!(PairAdvanceModel::new(&with_table).kerning_for_glyph(settings, &glyphs[0]).custom_pairs);
        let elsewhere = map_with(faces.hebrew.0, &[('Q', 'z', -200.0)]);
        assert!(!PairAdvanceModel::new(&elsewhere).kerning_for_glyph(settings, &glyphs[0]).custom_pairs);
    }

    /// The fast path must be unreachable for a font with overrides, otherwise the
    /// run would keep the shaped positions and the overrides would silently do
    /// nothing. `Auto` with zero tracking is the ONLY configuration that qualifies
    /// without them, so it is the only one that can regress.
    #[test]
    fn a_custom_table_takes_the_run_off_the_default_metric_fast_path() {
        let settings = KerningSettings {
            mode: KerningMode::Auto,
            spacing_px: 0.0,
            spacing_percent: 0.0,
            custom_pairs: false,
        };
        assert!(
            settings.uses_default_metric_layout(),
            "Auto with zero tracking must still qualify without overrides"
        );
        assert!(
            !settings.with_custom_pairs(true).uses_default_metric_layout(),
            "a font with overrides must never take the shaped-position shortcut"
        );
        assert!(
            settings.with_custom_pairs(false).uses_default_metric_layout(),
            "clearing the flag must restore the fast path exactly"
        );
    }

    /// The cluster guard: a pair is only authored for two single characters, so a
    /// glyph whose source cluster is not exactly one `char` can never match one.
    #[test]
    fn only_single_char_clusters_can_carry_an_authored_pair() {
        let mut latin = TrackedFont::Latin.load();
        let family = latin.family.clone();
        let (glyphs, text) = shape(&mut latin.font_system, &family, "AV", 40.0);
        let first = glyphs.first().expect("a shaped glyph");
        assert_eq!(
            single_char_cluster(&text, first),
            Some('A'),
            "a one-character cluster must resolve to that character"
        );
        // Out-of-range byte offsets (which a caller must never produce, but which
        // must not panic either) degrade to "no override".
        let mut broken = first.clone();
        broken.start = 0;
        broken.end = text.len() + 8;
        assert_eq!(single_char_cluster(&text, &broken), None);
        // A two-character span is a multi-char cluster and is skipped.
        let mut wide = first.clone();
        wide.start = 0;
        wide.end = 2;
        assert_eq!(single_char_cluster(&text, &wide), None);
    }
}
