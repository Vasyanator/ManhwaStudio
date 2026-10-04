/*
File: ai_api_editor/decisions.rs

Purpose:
The cloud engine's half of the run gate as a pure function over a snapshot of its state, so the
order of the reasons is pinned by tests without a window, a key store or a network.

Key structures:
- `RunGate`: the snapshot the gate reads

Key functions:
- `run_block_reason()`: the first reason a run may not start, localized

Notes:
The sentences are the `ImageEditError` `Display`s of `ms-ai-api` (and the host's own size
refusal), so a gated button and a failed run say the same thing in the same words.
*/

use crate::tools::region_edit_v2::engine::region_size_refusal;
use crate::tools::region_edit_v2::geometry::FrameConstraints;
use ms_ai_api::image_edit::{EndpointChoice, ImageEditError, ModelOffer};

/// What the run gate reads, snapshotted from the engine.
#[derive(Debug)]
pub(super) struct RunGate<'a> {
    /// A run of this engine is in flight.
    pub run_in_flight: bool,
    /// The settings file has been read (or its read failed): the selection is the user's.
    pub settings_loaded: bool,
    /// This is the web build, which has no HTTP executor.
    pub web_build: bool,
    /// The selected model's catalogue offer.
    pub offer: Result<&'static ModelOffer, ImageEditError>,
    /// The selected endpoint (region or server address).
    pub endpoint: Result<EndpointChoice, ImageEditError>,
    /// The selected provider requires an API key.
    pub key_required: bool,
    /// Whether a key is stored for the selection; `None` until a check answered.
    pub key_configured: Option<bool>,
    /// The prompt as typed.
    pub prompt: &'a str,
    /// The frame rectangle's size in page pixels, `None` while there is no frame.
    pub region: Option<(usize, usize)>,
    /// The constraints the frame validates against (the selected offer's rule).
    pub constraints: FrameConstraints,
}

/// The first reason a run may not start, in the order: run in flight, settings still loading,
/// web build, no usable model, invalid endpoint, a key known to be missing, a blank prompt, a
/// region the selected model cannot take at any allowed upscale. `None` when it may start.
///
/// An UNKNOWN key state (`None`) does not block: the check may still be running, and a run
/// without a key fails with the same `KeyMissing` sentence on the worker.
#[must_use]
pub(super) fn run_block_reason(gate: &RunGate<'_>) -> Option<String> {
    if gate.run_in_flight {
        return Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
    }
    if !gate.settings_loaded {
        return Some(t!("cleaning.tools.ai_api_editor.settings_loading_status").to_string());
    }
    if gate.web_build {
        return Some(ImageEditError::WebUnavailable.to_string());
    }
    if let Err(error) = &gate.offer {
        return Some(error.to_string());
    }
    if let Err(error) = &gate.endpoint {
        return Some(error.to_string());
    }
    if gate.key_required && gate.key_configured == Some(false) {
        return Some(ImageEditError::KeyMissing.to_string());
    }
    if gate.prompt.trim().is_empty() {
        return Some(ImageEditError::EmptyPrompt.to_string());
    }
    let (width, height) = gate.region?;
    region_size_refusal(width, height, &gate.constraints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ai_api_editor::constraints::frame_constraints;
    use ms_ai_api::image_edit::{ImageEditProvider, lookup};

    fn offer() -> &'static ModelOffer {
        lookup(ImageEditProvider::OpenAi, "gpt-image-2").expect("gpt-image-2 is in the catalogue")
    }

    /// A gate that lets a run start: every later test breaks exactly one input.
    fn open_gate() -> RunGate<'static> {
        RunGate {
            run_in_flight: false,
            settings_loaded: true,
            web_build: false,
            offer: Ok(offer()),
            endpoint: Ok(EndpointChoice::Default),
            key_required: true,
            key_configured: Some(true),
            prompt: "remove the text",
            region: Some((1024, 1024)),
            constraints: frame_constraints(offer().rule),
        }
    }

    #[test]
    fn an_open_gate_lets_the_run_start() {
        assert_eq!(run_block_reason(&open_gate()), None);
        assert_eq!(run_block_reason(&RunGate { key_configured: None, ..open_gate() }), None, "an unknown key state never blocks");
        assert_eq!(run_block_reason(&RunGate { region: None, ..open_gate() }), None, "no frame: the frame's own gate decides");
        assert_eq!(run_block_reason(&RunGate { key_required: false, key_configured: Some(false), ..open_gate() }), None, "a keyless server");
    }

    /// Each reason closes the gate on its own, and the earlier reason wins when several hold.
    #[test]
    fn the_reasons_are_checked_in_their_documented_order() {
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");

        let every_reason = RunGate {
            run_in_flight: true,
            settings_loaded: false,
            web_build: true,
            offer: Err(ImageEditError::UnknownModel { model_id: String::new() }),
            endpoint: Err(ImageEditError::InvalidEndpoint { detail: String::new() }),
            key_required: true,
            key_configured: Some(false),
            prompt: "  ",
            region: Some((1, 1)),
            constraints: frame_constraints(offer().rule),
        };
        let expected = [
            t!("cleaning.mask_editor.processing_already_running_status").to_string(),
            t!("cleaning.tools.ai_api_editor.settings_loading_status").to_string(),
            ImageEditError::WebUnavailable.to_string(),
            ImageEditError::UnknownModel { model_id: String::new() }.to_string(),
            ImageEditError::InvalidEndpoint { detail: String::new() }.to_string(),
            ImageEditError::KeyMissing.to_string(),
            ImageEditError::EmptyPrompt.to_string(),
        ];
        // Clear the reasons one by one, front to back: each step must expose the next.
        let mut gate = every_reason;
        for (step, sentence) in expected.iter().enumerate() {
            assert_eq!(run_block_reason(&gate).as_deref(), Some(sentence.as_str()), "step {step}");
            match step {
                0 => gate.run_in_flight = false,
                1 => gate.settings_loaded = true,
                2 => gate.web_build = false,
                3 => gate.offer = Ok(offer()),
                4 => gate.endpoint = Ok(EndpointChoice::Default),
                5 => gate.key_configured = Some(true),
                6 => gate.prompt = "remove the text",
                _ => unreachable!("only seven reasons precede the size check"),
            }
        }
        // Last: a 1×1 region no allowed upscale makes legal for the OpenAI rule.
        let size = run_block_reason(&gate).expect("1x1 is refused by the size rule");
        assert_eq!(Some(size), region_size_refusal(1, 1, &gate.constraints));
    }
}
