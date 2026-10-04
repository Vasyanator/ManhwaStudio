/*
File: ai_api_editor/constraints.rs

Purpose:
Maps a catalogue offer's `ms_ai_api::image_edit::ImageSizeRule` onto the frame's
`FrameConstraints`, field for field. The rule is DATA owned by `ms-ai-api`; what it means
(validity, the upscale factor `k`, snapping) is decided only by `region_edit_v2::geometry`, so
this file copies values and decides nothing.

Key functions:
- `frame_constraints()`: the 1:1 mapping

Notes:
The only conversion is `u32` -> `usize` for the side fields, which is lossless on every
supported target (`usize` is at least 32 bits); it saturates rather than failing so a
hypothetical 16-bit target would read "unlimited", never panic.
*/

use crate::tools::region_edit_v2::geometry::{AspectLimit, FrameConstraints};
use ms_ai_api::image_edit::ImageSizeRule;

/// The frame constraints of `rule`: every field copied 1:1 (sides widened to `usize`).
#[must_use]
pub(super) fn frame_constraints(rule: &ImageSizeRule) -> FrameConstraints {
    FrameConstraints {
        multiple: side(rule.multiple),
        min_side: side(rule.min_side),
        max_side: rule.max_side.map(side),
        min_area: rule.min_area,
        max_area: rule.max_area,
        aspect: rule.aspect.map(|aspect| AspectLimit { max_w_over_h: aspect.max_w_over_h, max_h_over_w: aspect.max_h_over_w }),
        sizes: rule.sizes,
        max_upscale: rule.max_upscale,
    }
}

/// A rule side in frame units; saturating, see the file header.
fn side(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::region_edit_v2::geometry::{check_size, snap_size, upscale_factor_for};
    use ms_ai_api::image_edit::all_offers;

    /// Every catalogue offer maps, and a 512×512 drag on a 4000×4000 page snaps to a size the
    /// mapped rule accepts: an empty or contradictory rule would leave the frame red forever.
    #[test]
    fn every_offer_maps_onto_a_satisfiable_frame_rule() {
        for offer in all_offers() {
            let c = frame_constraints(offer.rule);
            let (w, h) = snap_size(512, 512, 4000, 4000, &c);
            assert!(
                check_size(w, h, &c).is_none(),
                "{:?} {}: snapped {w}x{h} is still invalid",
                offer.provider,
                offer.model_id
            );
            let k = upscale_factor_for(w, h, &c).expect("a valid size has an upscale factor");
            assert!(k >= 1 && k <= offer.rule.effective_max_upscale());
        }
    }

    /// The copy is exact: the fields the geometry reads arrive unchanged.
    #[test]
    fn the_mapping_copies_every_field() {
        static SIZES: [(u32, u32); 1] = [(1024, 1024)];
        let rule = ImageSizeRule {
            multiple: 16,
            min_side: 64,
            max_side: Some(3840),
            min_area: Some(655_360),
            max_area: Some(8_294_400),
            aspect: Some(ms_ai_api::image_edit::ImageAspectLimit { max_w_over_h: 3.0, max_h_over_w: 2.0 }),
            sizes: &SIZES,
            max_upscale: 4,
        };
        let c = frame_constraints(&rule);
        assert_eq!((c.multiple, c.min_side, c.max_side), (16, 64, Some(3840)));
        assert_eq!((c.min_area, c.max_area), (Some(655_360), Some(8_294_400)));
        let aspect = c.aspect.expect("aspect is copied");
        assert_eq!((aspect.max_w_over_h.to_bits(), aspect.max_h_over_w.to_bits()), (3.0f32.to_bits(), 2.0f32.to_bits()));
        assert_eq!(c.sizes, &SIZES[..]);
        assert_eq!(c.max_upscale, 4);
    }

    /// An OpenAI-like rule needs an upscale for a small region: 300×900 is sent at ×2.
    #[test]
    fn a_small_region_is_sent_upscaled() {
        let openai = all_offers()
            .iter()
            .find(|offer| offer.model_id == "gpt-image-2" && offer.rule.min_area == Some(655_360))
            .expect("the catalogue lists gpt-image-2 with the OpenAI arbitrary-size rule");
        let c = frame_constraints(openai.rule);
        assert_eq!(upscale_factor_for(304, 912, &c), Some(2));
    }
}
