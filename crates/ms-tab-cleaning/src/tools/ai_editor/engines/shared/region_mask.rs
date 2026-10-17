/*
File: cleaning/tools/ai_editor/engines/shared/region_mask.rs

Purpose:
The working-mode rule of a region engine whose mask means "you MAY change what is under it"
(FLUX.2 klein and FLUX.3 klein): derive the L8 mask a run actually puts on the wire,
and the `whole_region` flag that must travel with it, from the host's painted layer.

Key functions:
- `mask_for_run()`

Notes:
The ONE place that decision is made for these engines. The host's `MaskStack` is never
written to: the whole-region mask is a fresh buffer.
*/

/// The L8 mask a run actually puts on the wire, region-sized either way, together with the
/// `whole_region` flag that must accompany it.
///
/// This is the ONE place the working mode is decided, and it is DERIVED, never chosen: an
/// empty `painted` layer means "edit the whole region", anything painted means "edit only
/// what is under it". A single painted pixel is already a mask — the threshold is "any byte
/// above zero" and nothing softer, so a stray dot cannot be mistaken for an empty layer.
///
/// `painted` is the host's mask layer, exactly `w * h` bytes. In the whole-region case the
/// model may change every pixel and the backend proves that by REQUIRING a solid mask
/// alongside the flag, so a fresh all-`255` buffer is built here rather than the host's
/// layer being overwritten — the host's `MaskStack` is never written to by the engine at
/// all, which is what keeps the user's painting theirs.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn mask_for_run(painted: &[u8]) -> (Vec<u8>, bool) {
    if painted.iter().any(|value| *value > 0) {
        (painted.to_vec(), false)
    } else {
        (vec![255u8; painted.len()], true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_painted_mask_travels_verbatim_and_an_empty_one_becomes_solid() {
        let mut painted = vec![0u8; 16];
        painted[5] = 1;
        assert_eq!(mask_for_run(&painted), (painted.clone(), false), "one painted byte is a mask");
        assert_eq!(mask_for_run(&[0u8; 16]), (vec![255u8; 16], true), "nothing painted: the whole region");
        assert_eq!(mask_for_run(&[]), (Vec::new(), true), "an empty layer keeps its (zero) size");
    }
}
