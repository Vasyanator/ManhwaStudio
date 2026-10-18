/*
File: cleaning/tools/ai_editor/engines/flux2_klein/run_outcome.rs

Purpose:
What one FLUX.2 klein run answers with, as the run worker hands it to the engine: the RAW
regenerated region with the mask it ran under and the latent mask the model saw (the host
post-processes them), whether the backend recovered from an out-of-memory failure, the one
memory flag of `applied` this side writes back, and a degraded run's effective steps.

Key structures:
- `Flux2AppliedFlags`, `Flux2RunOutcome`, `Flux2JobResult`

Key functions:
- `parse_applied_flags()`

Notes:
The shared progress state, its generation guard and the degraded-run parser are
`../shared/stream_progress.rs`; this file holds only what is FLUX.2's own.
*/

use super::*;

/// The one memory flag of a finished run's `applied` object this side writes back.
///
/// An out-of-memory failure during the VAE decode is recovered by retrying it tiled, and
/// the value comes back here so the next run starts tiled instead of hitting the same wall.
/// `None` when the answer did not report it. Read TOLERANTLY: a v13 backend sends five
/// flags and a v14 one only this one; the others (placement-era unload flags, fp8) are not
/// written back — the steps are the user's, and the backend learns a floor instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Flux2AppliedFlags {
    pub(super) vae_tiling: Option<bool>,
}

/// One finished generation: the RAW regenerated region plus what the backend reports about
/// how it got there.
pub(super) struct Flux2RunOutcome {
    /// The backend's raw decode, exactly the region size and opaque.
    pub(super) image: egui::ColorImage,
    /// The L8 0/255 latent mask the run sent (the painted mask dilated by `mask_dilate_px`),
    /// region-sized; the host's colour-match sample mask.
    pub(super) latent_mask: Vec<u8>,
    /// The backend hit an out-of-memory failure during the VAE decode and recovered
    /// from it without re-running the denoising.
    pub(super) oom_recovered: bool,
    /// Present when the backend reported an `applied` object.
    pub(super) applied: Option<Flux2AppliedFlags>,
    /// Present when the run was degraded (`degraded: true`).
    pub(super) degraded: Option<RunDegraded>,
}

/// Message the run worker sends back: the run's facts the host post-processes with (moved
/// out of the run input once the call is over) and the call's outcome.
pub(super) struct Flux2JobResult {
    /// The mask the run used (`mask_for_run` output): nonzero = the pixel may change.
    pub(super) mask: Vec<u8>,
    /// The run regenerated the whole region (nothing painted).
    pub(super) whole_region: bool,
    pub(super) result: Result<Flux2RunOutcome, String>,
}

/// Reads the `applied` object of a generation response: `None` without one, otherwise
/// the `vae_tiling` flag it carries (any other flag is ignored, whatever the version).
pub(super) fn parse_applied_flags(header: &Value) -> Option<Flux2AppliedFlags> {
    let applied = header.get("applied")?.as_object()?;
    Some(Flux2AppliedFlags { vae_tiling: applied.get("vae_tiling").and_then(Value::as_bool) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `applied` is read tolerantly: the five-flag v13 object and the one-flag v14 object
    /// both yield `vae_tiling`, and nothing else is taken from either.
    #[test]
    fn applied_is_parsed_tolerantly_for_both_protocol_shapes() {
        let v13 = json!({
            "image_len": 4,
            "oom_recovered": true,
            "applied": {
                "unload_transformer_before_vae": true,
                "vae_tiling": true,
                "vae_slicing": false,
                "unload_text_encoder_after_encode": true,
                "text_encoder_fp8": false
            }
        });
        assert_eq!(parse_applied_flags(&v13), Some(Flux2AppliedFlags { vae_tiling: Some(true) }));
        let v14 = json!({ "applied": { "vae_tiling": false } });
        assert_eq!(parse_applied_flags(&v14), Some(Flux2AppliedFlags { vae_tiling: Some(false) }));
        assert_eq!(parse_applied_flags(&json!({ "applied": {} })), Some(Flux2AppliedFlags { vae_tiling: None }));
        assert!(parse_applied_flags(&json!({ "image_len": 4 })).is_none());
    }
}
