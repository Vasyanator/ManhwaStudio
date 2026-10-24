/*
File: crates/ms-region-postprocess/src/pipeline.rs

Purpose:
The region post-processing pipeline over one raw model result: global colour match -> local
CIELAB match -> inward feather -> composite, with per-stage caches so a settings change re-runs
only the stages it affects.

Key items:
- `RegionInputs`: the owned buffers of one result; `ColourReference`: ring or edge band.
- `RegionPostprocessor`: validated inputs + stage caches; `render()`.
- `postprocess_region()`: one-shot convenience.

Notes:
Stage chaining: `global = match(generated, reference, sample_mask)` when `global_match`, else
`generated`; `local = match_local(reference, global, sample_mask)` when `local_strength > 0`,
else `global`; result = `composite(base, local, mask, feather)`. `reference` defaults to `base`,
`sample_mask` to `mask`. Under `ColourReference::EdgeBand` both colour stages sample the
region's edge band instead (`edge_band.rs`) and the local remap covers all of `mask`. The local match is cached in three parts (sampling once, fields per
global flag + scale, remap per strength), so a strength change never re-runs the cascade. CPU
work: call it off the GUI thread (a cold 4 MP render is about a hundred ms on a multi-core
machine, dominated by the local match; the feather runs concurrently with the colour chain).
*/

use crate::edge_band::EdgeBand;
use crate::feather::{feather_alpha, inside_distance};
use crate::global_match::{match_color_band, match_color_global};
use crate::local_match::{LocalFields, LocalMatchError, LocalSampling};
use crate::{EffectiveSettings, PostprocessError, check_len, checked_area, composite};

/// The buffers of one raw region result, all row-major `width * height` (`* 3` for RGB8).
#[derive(Debug, Clone)]
pub struct RegionInputs {
    /// Region width, px.
    pub width: usize,
    /// Region height, px.
    pub height: usize,
    /// RGB8 composite base: the unmarked original region.
    pub base: Vec<u8>,
    /// RGB8 colour-match reference when it differs from `base` (the region as SENT to the
    /// model, e.g. with overlay marks); `None` = `base`.
    pub reference: Option<Vec<u8>>,
    /// RGB8 raw model output.
    pub generated: Vec<u8>,
    /// Composite mask, nonzero = may change.
    pub mask: Vec<u8>,
    /// Colour-match mask when it differs from `mask` (FLUX.2: the dilated latent mask);
    /// `None` = `mask`. Ignored in [`ColourReference::EdgeBand`] mode.
    pub sample_mask: Option<Vec<u8>>,
    /// Where the colour match measures what generated and original should agree on.
    pub colour_reference: ColourReference,
}

/// Where the colour match takes its reference statistics: fixed per result (the run's mode),
/// so every stage cache stays valid across settings changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColourReference {
    /// A painted mask: the ring OUTSIDE `sample_mask` (global: grown by 6 px; local: target
    /// outside, current inside, applied inside `sample_mask`).
    #[default]
    Ring,
    /// A whole-region run (nothing painted, solid mask): the region's outer edge band
    /// (`EDGE_BAND_PX` = 12 px, at most a quarter of the shorter side). Global: the same fit on
    /// the band pixels, no margin; local: the band's Lab offset `original - generated`,
    /// extrapolated inwards by the cascade, applied over the whole `mask` at the local
    /// strength and scale. A result whose border was MEANT to change is pulled back towards
    /// the original there; the user turns the colour match off for it.
    EdgeBand,
}

/// One raw result plus cached stage outputs, so a settings change re-runs only what it affects:
/// - the global match (computed once);
/// - the local match's sampling (margin, sample sets; once), its correction fields
///   per `(global on/off, scale)` (a few recent ones, `FIELDS_CACHE_*`), and the last remapped
///   image per `(global on/off, scale, strength)` — a strength change is one per-pixel remap;
/// - the inside distance (once) and the last feather alpha (per width) — a feather change is
///   one alpha plus one composite.
#[derive(Debug)]
pub struct RegionPostprocessor {
    inputs: RegionInputs,
    colour: ColourCache,
    feather: FeatherCache,
}

/// The colour-match stage caches (see `RegionPostprocessor`).
#[derive(Debug, Default)]
struct ColourCache {
    global: Option<Vec<u8>>,
    sampling: Option<LocalSampling>,
    /// Most recently used last; `None` fields = the match is the identity for this region.
    fields: Vec<(FieldsKey, Option<LocalFields>)>,
    /// The last remapped image; `None` image = identity (use the global stage).
    local: Option<(LocalKey, Option<Vec<u8>>)>,
}

/// The feather stage caches (see `RegionPostprocessor`).
#[derive(Debug, Default)]
struct FeatherCache {
    distance: Option<Vec<f32>>,
    alpha: Option<(u32, Vec<u8>)>,
}

/// Cache key of the local correction fields: their input image (global on/off) and the scale.
/// The fields depend on nothing else: the reference and both masks are fixed per processor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldsKey {
    global_match: bool,
    scale_px: u32,
}

/// Cache key of the remapped local image: the fields plus the strength they are blended at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalKey {
    fields: FieldsKey,
    strength_bits: u32,
}

/// Most correction-field sets kept per processor (scale drags revisit nearby values).
const FIELDS_CACHE_LEN: usize = 8;
/// Byte budget of the kept field sets; the newest is always kept. A set is a coarse grid
/// (`floor(scale / 4)` px a cell), so this holds every set of a scale drag except at the
/// finest scales of a very large region.
const FIELDS_CACHE_BYTES: usize = 48 << 20;

impl RegionPostprocessor {
    /// Validates every buffer of `inputs` and takes ownership; nothing is computed yet.
    ///
    /// # Errors
    /// `EmptyRegion` for a zero width or height, `SizeOverflow` when the area overflows, and
    /// `ShapeMismatch` naming the first buffer whose length is wrong.
    pub fn new(inputs: RegionInputs) -> Result<Self, PostprocessError> {
        if inputs.width == 0 || inputs.height == 0 {
            return Err(PostprocessError::EmptyRegion);
        }
        let area = checked_area(inputs.width, inputs.height)?;
        let rgb = area.checked_mul(3).ok_or(PostprocessError::SizeOverflow { width: inputs.width, height: inputs.height })?;
        check_len("base", rgb, inputs.base.len())?;
        if let Some(reference) = &inputs.reference {
            check_len("reference", rgb, reference.len())?;
        }
        check_len("generated", rgb, inputs.generated.len())?;
        check_len("mask", area, inputs.mask.len())?;
        if let Some(sample_mask) = &inputs.sample_mask {
            check_len("sample_mask", area, sample_mask.len())?;
        }
        Ok(Self { inputs, colour: ColourCache::default(), feather: FeatherCache::default() })
    }

    /// Region width, px.
    #[must_use]
    pub fn width(&self) -> usize {
        self.inputs.width
    }

    /// Region height, px.
    #[must_use]
    pub fn height(&self) -> usize {
        self.inputs.height
    }

    /// Renders the RGB8 result for `settings`: outside `mask` byte-identical to `base`;
    /// deterministic for equal inputs and settings, whatever was rendered before.
    ///
    /// # Errors
    /// See [`Self::render_masked_into`] (its `ShapeMismatch` for `out` cannot occur here).
    pub fn render(&mut self, settings: &EffectiveSettings) -> Result<Vec<u8>, PostprocessError> {
        let mut out = self.inputs.base.clone();
        let (pixels, _) = out.as_chunks_mut::<3>();
        self.render_masked_into(settings, pixels, |px, rgb| *px = rgb)?;
        Ok(out)
    }

    /// Renders the result for `settings` into a caller's pixel buffer `out` (`width * height`
    /// pixels, row-major): `write(pixel, rgb)` receives, for every pixel INSIDE `mask`, the RGB
    /// [`Self::render`] produces there; every other pixel of `out` is left untouched. A caller
    /// that pre-fills `out` with its own picture of the base (an RGBA display image, alpha
    /// included) therefore gets the result in its own format in one pass, outside the mask
    /// verbatim. `write` runs on rayon threads.
    ///
    /// The feather stage runs concurrently with the colour match; every parallel pass is per
    /// pixel or per row, so the bytes do not depend on the thread count (no parallel float
    /// reduction anywhere).
    ///
    /// # Errors
    /// `InvalidStrength` when `local_strength` is not finite or outside `0.0..=1.0`,
    /// `InvalidScale` when the local match is on with `local_scale_px == 0`, `ShapeMismatch`
    /// (`what = "out"`) for an `out` of the wrong length, and `LocalMatch` if the local stage
    /// refuses its input (a scale above its cap). Nothing is written on an error.
    pub fn render_masked_into<P: Send>(&mut self, settings: &EffectiveSettings, out: &mut [P], write: impl Fn(&mut P, [u8; 3]) + Sync) -> Result<(), PostprocessError> {
        let strength = settings.local_strength;
        if !strength.is_finite() || !(0.0..=1.0).contains(&strength) {
            return Err(PostprocessError::InvalidStrength(strength));
        }
        let local_on = strength > 0.0;
        if local_on && settings.local_scale_px == 0 {
            return Err(PostprocessError::InvalidScale(settings.local_scale_px));
        }
        let (w, h) = (self.inputs.width, self.inputs.height);
        check_len("out", w * h, out.len())?;
        let key = LocalKey { fields: FieldsKey { global_match: settings.global_match, scale_px: settings.local_scale_px }, strength_bits: strength.to_bits() };
        let Self { inputs, colour, feather } = self;
        // The feather (distance transform + alpha) and the colour chain share only read-only
        // inputs; running them side by side hides the sequential distance transform.
        let (feathered, coloured) = rayon::join(|| feather.prepare(&inputs.mask, w, h, settings.feather_px), || colour.prepare(inputs, settings.global_match, local_on.then_some(key)));
        coloured?;
        feathered?;
        let corrected = match &colour.local {
            Some((k, Some(local))) if local_on && *k == key => local.as_slice(),
            _ => global_stage(colour.global.as_deref(), &inputs.generated, settings.global_match),
        };
        let alpha = feather.alpha.as_ref().map_or(&[][..], |(_, a)| a.as_slice());
        composite::composite_masked_into(&inputs.base, corrected, &inputs.mask, alpha, w, out, write);
        Ok(())
    }
}

impl FeatherCache {
    /// Brings the alpha up to date for `feather_px` (the distance first, once, when needed).
    fn prepare(&mut self, mask: &[u8], w: usize, h: usize, feather_px: u32) -> Result<(), PostprocessError> {
        if feather_px > 0 && self.distance.is_none() {
            self.distance = Some(inside_distance(mask, w, h)?);
        }
        if self.alpha.as_ref().is_none_or(|(f, _)| *f != feather_px) {
            // A zero feather never reads the distance; an empty slice keeps it uncomputed.
            let alpha = feather_alpha(mask, self.distance.as_deref().unwrap_or(&[]), feather_px);
            self.alpha = Some((feather_px, alpha));
        }
        Ok(())
    }
}

impl ColourCache {
    /// Brings the colour caches up to date: the global match when `global_match`, and, when
    /// `local` is `Some`, the remapped local image of that key. The global match and the local
    /// sampling are independent and built concurrently on first use.
    fn prepare(&mut self, inputs: &RegionInputs, global_match: bool, local: Option<LocalKey>) -> Result<(), PostprocessError> {
        let (w, h) = (inputs.width, inputs.height);
        let reference = inputs.reference.as_deref().unwrap_or(&inputs.base);
        let sample_mask = inputs.sample_mask.as_deref().unwrap_or(&inputs.mask);
        // The pixels the local remap rewrites: the sample mask around a ring, the whole
        // permission mask under an edge band (the band's offset extends over all of it).
        let (band, apply_mask) = match inputs.colour_reference {
            ColourReference::Ring => (None, sample_mask),
            ColourReference::EdgeBand => (Some(EdgeBand::new(w, h)), &inputs.mask[..]),
        };
        let need_global = global_match && self.global.is_none();
        let sampling_slot = &mut self.sampling;
        let (global, sampling) = rayon::join(
            || {
                need_global
                    .then(|| match &band {
                        None => match_color_global(reference, &inputs.generated, sample_mask, w, h),
                        Some(band) => Ok(match_color_band(reference, &inputs.generated, band)),
                    })
                    .transpose()
            },
            || -> Result<Option<&LocalSampling>, PostprocessError> {
                if local.is_none() {
                    return Ok(None);
                }
                if sampling_slot.is_none() {
                    let sampling = match &band {
                        None => LocalSampling::new(reference, sample_mask, w, h),
                        Some(band) => LocalSampling::new_band(band.to_mask(), w, h),
                    };
                    *sampling_slot = Some(sampling.map_err(|error| local_error(&error))?);
                }
                Ok(sampling_slot.as_ref())
            },
        );
        if let Some((global, _path)) = global? {
            self.global = Some(global);
        }
        // `None` exactly when the local match is off.
        let (Some(sampling), Some(key)) = (sampling?, local) else {
            return Ok(());
        };
        if self.local.as_ref().is_some_and(|(k, _)| *k == key) {
            return Ok(());
        }
        let current = global_stage(self.global.as_deref(), &inputs.generated, key.fields.global_match);
        let entry = match self.fields.iter().position(|(k, _)| *k == key.fields) {
            Some(index) => self.fields.remove(index),
            None => (key.fields, sampling.fields(reference, current, key.fields.scale_px).map_err(|error| local_error(&error))?),
        };
        let strength = f32::from_bits(key.strength_bits);
        let remapped = entry.1.as_ref().map(|fields| fields.apply(current, apply_mask, strength)).transpose().map_err(|error| local_error(&error))?;
        self.remember_fields(entry);
        self.local = Some((key, remapped));
        Ok(())
    }

    /// Puts `entry` last (most recent) and drops the oldest sets beyond `FIELDS_CACHE_LEN`
    /// or `FIELDS_CACHE_BYTES`, always keeping `entry`.
    fn remember_fields(&mut self, entry: (FieldsKey, Option<LocalFields>)) {
        self.fields.push(entry);
        let bytes = |fields: &[(FieldsKey, Option<LocalFields>)]| fields.iter().map(|(_, f)| f.as_ref().map_or(0, LocalFields::byte_size)).sum::<usize>();
        while self.fields.len() > 1 && (self.fields.len() > FIELDS_CACHE_LEN || bytes(&self.fields) > FIELDS_CACHE_BYTES) {
            self.fields.remove(0);
        }
    }
}

/// The crate error for a refusal of the local stage.
fn local_error(error: &LocalMatchError) -> PostprocessError {
    PostprocessError::LocalMatch { reason: error.to_string() }
}

/// Output of the global stage: the cached match when on (computed by `prepare` first), else
/// the raw output.
fn global_stage<'a>(global: Option<&'a [u8]>, generated: &'a [u8], global_match: bool) -> &'a [u8] {
    match (global, global_match) {
        (Some(global), true) => global,
        _ => generated,
    }
}

/// One-shot `RegionPostprocessor::new(inputs)?.render(settings)` (tests, simple callers).
///
/// # Errors
/// Every error of [`RegionPostprocessor::new`] and [`RegionPostprocessor::render`].
pub fn postprocess_region(inputs: RegionInputs, settings: &EffectiveSettings) -> Result<Vec<u8>, PostprocessError> {
    RegionPostprocessor::new(inputs)?.render(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(w: usize, h: usize) -> RegionInputs {
        RegionInputs { width: w, height: h, base: vec![10; w * h * 3], reference: None, generated: vec![200; w * h * 3], mask: vec![255; w * h], sample_mask: None, colour_reference: ColourReference::Ring }
    }

    const PLAIN: EffectiveSettings = EffectiveSettings { feather_px: 0, global_match: false, local_strength: 0.0, local_scale_px: 24 };

    #[test]
    fn new_refuses_bad_shapes() {
        assert_eq!(RegionPostprocessor::new(inputs(0, 4)).err(), Some(PostprocessError::EmptyRegion));
        let mut bad = inputs(2, 2);
        bad.generated.pop();
        assert_eq!(RegionPostprocessor::new(bad).err(), Some(PostprocessError::ShapeMismatch { what: "generated", expected: 12, actual: 11 }));
        let mut bad = inputs(2, 2);
        bad.sample_mask = Some(vec![0; 3]);
        assert_eq!(RegionPostprocessor::new(bad).err(), Some(PostprocessError::ShapeMismatch { what: "sample_mask", expected: 4, actual: 3 }));
        let mut bad = inputs(2, 2);
        bad.reference = Some(vec![0; 13]);
        assert_eq!(RegionPostprocessor::new(bad).err(), Some(PostprocessError::ShapeMismatch { what: "reference", expected: 12, actual: 13 }));
    }

    #[test]
    fn render_refuses_invalid_local_parameters() -> Result<(), PostprocessError> {
        let mut p = RegionPostprocessor::new(inputs(2, 2))?;
        assert_eq!(p.render(&EffectiveSettings { local_strength: f32::NAN, ..PLAIN }).map(|_| ()).err().map(|e| matches!(e, PostprocessError::InvalidStrength(_))), Some(true));
        assert_eq!(p.render(&EffectiveSettings { local_strength: 1.5, ..PLAIN }).err(), Some(PostprocessError::InvalidStrength(1.5)));
        assert_eq!(p.render(&EffectiveSettings { local_strength: 0.5, local_scale_px: 0, ..PLAIN }).err(), Some(PostprocessError::InvalidScale(0)));
        Ok(())
    }

    /// A 72x56 page-like region with an off-centre blob mask, a tone-shifted noisy answer and a
    /// wider sample mask: every stage (global, local, feather) does real work on it.
    fn textured(w: usize, h: usize) -> RegionInputs {
        let mut state = 0x2545_f491_u32;
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            u8::try_from(state % 23).unwrap_or(0)
        };
        let mut base = Vec::with_capacity(w * h * 3);
        let mut generated = Vec::with_capacity(w * h * 3);
        let mut mask = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let level = u8::try_from((x * 3 + y * 2) % 200).unwrap_or(0) + noise();
                base.extend_from_slice(&[level, level.saturating_sub(10), level / 2 + 40]);
                generated.extend_from_slice(&[level.saturating_sub(30) + noise(), level / 3 + noise(), level.saturating_add(20)]);
                let (dx, dy) = (x.abs_diff(30), y.abs_diff(26));
                if dx * dx + dy * dy <= 15 * 15 {
                    mask[y * w + x] = 255;
                }
            }
        }
        let sample_mask = crate::dilate_ellipse(&mask, w, h, 4).ok();
        RegionInputs { width: w, height: h, base, reference: None, generated, mask, sample_mask, colour_reference: ColourReference::Ring }
    }

    /// The caches never leak one setting into another: an alternating sequence that revisits
    /// strengths, scales (beyond the field-cache length) and the colour toggle renders exactly
    /// the bytes of a fresh processor every time, and the masked-into form writes exactly those
    /// bytes inside the mask and nothing outside it. Both reference modes: the ring around a
    /// blob, and the edge band of a whole-region result.
    #[test]
    fn alternating_settings_equal_fresh_renders() -> Result<(), PostprocessError> {
        assert_alternating_settings_equal_fresh_renders(|| textured(72, 56))?;
        assert_alternating_settings_equal_fresh_renders(|| RegionInputs { mask: vec![255; 72 * 56], sample_mask: None, colour_reference: ColourReference::EdgeBand, ..textured(72, 56) })
    }

    /// The body of `alternating_settings_equal_fresh_renders` for the inputs `make` builds.
    fn assert_alternating_settings_equal_fresh_renders(make: impl Fn() -> RegionInputs) -> Result<(), PostprocessError> {
        const SENTINEL: [u8; 4] = [1, 2, 3, 4];
        let base = EffectiveSettings { feather_px: 6, global_match: true, local_strength: 0.6, local_scale_px: 24 };
        let mut sequence = vec![base];
        for scale in [8, 12, 16, 20, 24, 28, 32, 40, 48, 8, 24] {
            sequence.push(EffectiveSettings { local_scale_px: scale, ..base });
        }
        sequence.extend([
            EffectiveSettings { local_strength: 0.3, ..base },
            EffectiveSettings { local_strength: 1.0, ..base },
            base,
            EffectiveSettings { global_match: false, ..base },
            EffectiveSettings { global_match: false, local_strength: 0.3, ..base },
            EffectiveSettings { local_strength: 0.0, ..base },
            EffectiveSettings { feather_px: 0, ..base },
            EffectiveSettings { feather_px: 32, local_scale_px: 8, ..base },
            base,
            EffectiveSettings { global_match: false, local_strength: 0.0, ..base },
            base,
        ]);
        let (w, h) = (make().width, make().height);
        let mut cached = RegionPostprocessor::new(make())?;
        let mask = make().mask;
        let mut previous: Option<Vec<u8>> = None;
        for settings in &sequence {
            let fresh = postprocess_region(make(), settings)?;
            assert_eq!(cached.render(settings)?, fresh, "{settings:?}");
            let mut shown = vec![SENTINEL; w * h];
            cached.render_masked_into(settings, &mut shown, |px, [r, g, b]| *px = [r, g, b, 255])?;
            for ((px, rgb), &m) in shown.iter().zip(fresh.as_chunks::<3>().0).zip(&mask) {
                let expected = if m == 0 { SENTINEL } else { [rgb[0], rgb[1], rgb[2], 255] };
                assert_eq!(*px, expected, "{settings:?}");
            }
            if let Some(previous) = &previous {
                assert_ne!(previous, &fresh, "the sequence exercises a real change at {settings:?}");
            }
            previous = Some(fresh);
        }
        Ok(())
    }

    #[test]
    fn a_wrong_out_length_is_refused_before_anything_is_written() -> Result<(), PostprocessError> {
        let mut p = RegionPostprocessor::new(inputs(2, 2))?;
        let mut out = vec![[9u8; 3]; 3];
        assert_eq!(p.render_masked_into(&PLAIN, &mut out, |px, rgb| *px = rgb), Err(PostprocessError::ShapeMismatch { what: "out", expected: 4, actual: 3 }));
        assert_eq!(out, vec![[9u8; 3]; 3]);
        Ok(())
    }

    #[test]
    fn hard_edge_without_colour_match_is_the_generated_inside_and_base_outside() -> Result<(), PostprocessError> {
        let mut i = inputs(2, 1);
        i.mask = vec![0, 255];
        assert_eq!(postprocess_region(i, &PLAIN)?, vec![10, 10, 10, 200, 200, 200]);
        Ok(())
    }
}
