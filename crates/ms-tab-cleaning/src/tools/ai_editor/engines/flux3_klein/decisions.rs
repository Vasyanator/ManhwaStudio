/*
File: cleaning/tools/ai_editor/engines/flux3_klein/decisions.rs

Purpose:
The PURE decisions the FLUX.3 klein panel renders: which readiness line to show, whether a run
may start, what the seed field means, how a backend stage is named, what a finished run says.
None of them touches a `Ui`, a channel or the disk, so each is asserted without a frame.

Key structures:
- `Flux3ReadinessLine`, `Flux3LineTone`, `Flux3MemorySummary`, `Flux3SeedInput`

Key functions:
- `flux3_readiness_line()`, `flux3_run_block_reason()`, `parse_seed_input()`,
  `flux3_stage_label()`, `flux3_run_done_status()`

Notes:
The size rule is NOT here: validity is `region_edit_v2::geometry::check_size` over
`Flux3KleinEngine::frame_constraints`, and the run path asks the shared `region_size_refusal`.
*/

use super::*;

/// The tone a one-line verdict is drawn in (`ms_theme::status` colours or plain small text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux3LineTone {
    Ok,
    Warn,
    Neutral,
}

/// The memory forecast as the readiness line carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Flux3MemorySummary {
    /// The one-line forecast (`large_model_resources::forecast_line`).
    pub(super) line: String,
    /// The backend's verdict; `false` turns the line amber.
    pub(super) fits: bool,
}

/// What the always-visible readiness line reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux3ReadinessLine {
    /// Everything is there; `memory` is the forecast when one has landed.
    Ready { memory: Option<Flux3MemorySummary> },
    /// Parts are missing (the localized list); «Установить» is offered.
    Missing { components: String },
    /// Manual mode without a folder; «Установить» is offered (it opens the section).
    NoRoot,
    /// No answer yet: neutral, no button.
    Unknown,
}

impl Flux3ReadinessLine {
    /// The colour of the line.
    pub(super) fn tone(&self) -> Flux3LineTone {
        match self {
            Self::Ready { memory: Some(summary) } if !summary.fits => Flux3LineTone::Warn,
            Self::Ready { .. } => Flux3LineTone::Ok,
            Self::Missing { .. } | Self::NoRoot => Flux3LineTone::Warn,
            Self::Unknown => Flux3LineTone::Neutral,
        }
    }

    /// Whether «Установить» is offered beside the line.
    pub(super) fn offers_install(&self) -> bool {
        matches!(self, Self::Missing { .. } | Self::NoRoot)
    }

    /// The localized text.
    pub(super) fn text(&self) -> String {
        match self {
            Self::Ready { memory: Some(summary) } => tf!("cleaning.tools.flux3_klein.model_ready_memory_status", memory = summary.line),
            Self::Ready { memory: None } => t!("cleaning.tools.flux3_klein.model_ready_status").to_string(),
            Self::Missing { components } => tf!("cleaning.tools.flux3_klein.model_missing_status", components = components),
            Self::NoRoot => t!("cleaning.tools.flux3_klein.model_root_missing_status").to_string(),
            Self::Unknown => t!("cleaning.tools.flux3_klein.model_readiness_unknown_status").to_string(),
        }
    }
}

/// Turns the model verdict into the readiness line; `memory` counts only for `Ready`.
#[must_use]
pub(super) fn flux3_readiness_line(readiness: &Flux3ModelReadiness, memory: Option<Flux3MemorySummary>) -> Flux3ReadinessLine {
    match readiness {
        Flux3ModelReadiness::Ready => Flux3ReadinessLine::Ready { memory },
        Flux3ModelReadiness::Missing(missing) => Flux3ReadinessLine::Missing { components: missing.names() },
        Flux3ModelReadiness::NoRoot => Flux3ReadinessLine::NoRoot,
        Flux3ModelReadiness::Unknown => Flux3ReadinessLine::Unknown,
    }
}

/// What the seed field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux3SeedInput {
    /// Empty: every run draws a fresh random seed (the backend's, echoed back).
    Random,
    /// A whole number in `0..=u64::MAX`: every run uses it.
    Pinned(u64),
    /// Anything else: the run is refused until it is fixed, never silently randomized.
    Invalid,
}

impl Flux3SeedInput {
    /// The value the settings persist: `Some` only for a pinned seed. `Invalid` has none and
    /// keeps the previous stored value (the field is what is wrong, not the setting).
    pub(super) fn seed(self) -> Option<Option<u64>> {
        match self {
            Self::Random => Some(None),
            Self::Pinned(seed) => Some(Some(seed)),
            Self::Invalid => None,
        }
    }
}

/// Reads the seed field: blank = random, a whole number = pinned, anything else = invalid.
#[must_use]
pub(super) fn parse_seed_input(text: &str) -> Flux3SeedInput {
    let text = text.trim();
    if text.is_empty() {
        return Flux3SeedInput::Random;
    }
    text.parse::<u64>().map_or(Flux3SeedInput::Invalid, Flux3SeedInput::Pinned)
}

/// The first reason a run cannot start that this engine sees, or `None`.
///
/// The region size and the empty mask belong to the host (`constraints()`, `allows_empty_mask()`).
/// `Unknown` readiness never blocks — the backend keeps the final word and refuses with its own
/// message.
#[must_use]
pub(super) fn flux3_run_block_reason(settings: &Flux3KleinSettings, readiness: &Flux3ModelReadiness, seed: Flux3SeedInput) -> Option<String> {
    match readiness {
        Flux3ModelReadiness::NoRoot => return Some(t!("cleaning.tools.flux3_klein.model_root_required_error").to_string()),
        Flux3ModelReadiness::Missing(missing) => {
            let components = missing.names();
            return Some(match settings.source_mode() {
                Flux3SourceMode::Download => tf!("cleaning.tools.flux3_klein.model_not_downloaded_error", components = components),
                Flux3SourceMode::Manual => tf!("cleaning.tools.flux3_klein.model_components_missing_error", components = components),
            });
        }
        Flux3ModelReadiness::Unknown | Flux3ModelReadiness::Ready => {}
    }
    if settings.prompt.trim().is_empty() {
        return Some(t!("cleaning.tools.flux3_klein.prompt_required_error").to_string());
    }
    if seed == Flux3SeedInput::Invalid {
        return Some(t!("cleaning.tools.flux3_klein.seed_invalid_error").to_string());
    }
    None
}

/// The localized name of a backend progress `stage` (`region.progress_stages()`), `None` for
/// an id this build does not know (the bar then shows the backend's own label).
#[must_use]
pub(super) fn flux3_stage_label(stage: &str) -> Option<&'static str> {
    Some(match stage {
        "prepare" => t!("cleaning.tools.flux3_klein.stage_prepare"),
        "dit_load" => t!("cleaning.tools.flux3_klein.stage_dit_load"),
        "ae_load" => t!("cleaning.tools.flux3_klein.stage_ae_load"),
        "dit_offload" => t!("cleaning.tools.flux3_klein.stage_dit_offload"),
        "encoder_load" => t!("cleaning.tools.flux3_klein.stage_encoder_load"),
        "encode" => t!("cleaning.tools.flux3_klein.stage_encode"),
        "encoder_release" => t!("cleaning.tools.flux3_klein.stage_encoder_release"),
        "ref_encode" => t!("cleaning.tools.flux3_klein.stage_ref_encode"),
        "dit_restore" => t!("cleaning.tools.flux3_klein.stage_dit_restore"),
        "denoise" => t!("cleaning.tools.flux3_klein.stage_denoise"),
        "dit_offload_decode" => t!("cleaning.tools.flux3_klein.stage_dit_offload_decode"),
        "decode" => t!("cleaning.tools.flux3_klein.stage_decode"),
        _ => return None,
    })
}

/// Every stage id the backend sends, in its order (`region.progress_stages()`); the catalog
/// test walks it.
#[cfg(test)]
pub(super) const FLUX3_STAGES: [&str; 12] = [
    "prepare", "dit_load", "ae_load", "dit_offload", "encoder_load", "encode", "encoder_release", "ref_encode", "dit_restore", "denoise",
    "dit_offload_decode", "decode",
];

/// The engine's line about a finished run: the seed it used (so a good result can be
/// repeated), and for a degraded run the step pair it actually ran as.
#[must_use]
pub(super) fn flux3_run_done_status(seed: u64, degraded: Option<&RunDegraded>) -> String {
    match degraded {
        Some(degraded) => {
            let unknown = || t!("large_models.step.unknown_label").to_string();
            tf!(
                "cleaning.tools.flux3_klein.run_degraded_status",
                seed = seed,
                vram = degraded.vram.map_or_else(unknown, |step| step.label().to_string()),
                ram = degraded.ram.map_or_else(unknown, |step| step.label().to_string())
            )
        }
        None => tf!("cleaning.tools.flux3_klein.run_done_status", seed = seed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blank is random, a number is pinned (the full u64 range), anything else is refused.
    #[test]
    fn the_seed_field_reads_blank_as_random() {
        assert_eq!(parse_seed_input(""), Flux3SeedInput::Random);
        assert_eq!(parse_seed_input("   "), Flux3SeedInput::Random);
        assert_eq!(parse_seed_input(" 42 "), Flux3SeedInput::Pinned(42));
        assert_eq!(parse_seed_input("18446744073709551615"), Flux3SeedInput::Pinned(u64::MAX));
        assert_eq!(parse_seed_input("18446744073709551616"), Flux3SeedInput::Invalid);
        assert_eq!(parse_seed_input("-1"), Flux3SeedInput::Invalid);
        assert_eq!(parse_seed_input("abc"), Flux3SeedInput::Invalid);
        assert_eq!(Flux3SeedInput::Random.seed(), Some(None));
        assert_eq!(Flux3SeedInput::Pinned(7).seed(), Some(Some(7)));
        assert_eq!(Flux3SeedInput::Invalid.seed(), None);
    }

    /// The gate: no folder and missing parts block with their own sentence, `Unknown` never
    /// blocks, an empty prompt and an invalid seed do.
    #[test]
    fn the_run_gate_blocks_only_on_what_the_user_can_fix() {
        let _locale = english();
        let settings = Flux3KleinSettings::default();
        assert!(flux3_run_block_reason(&settings, &Flux3ModelReadiness::Unknown, Flux3SeedInput::Random).is_none());
        assert!(flux3_run_block_reason(&settings, &Flux3ModelReadiness::Ready, Flux3SeedInput::Pinned(1)).is_none());
        assert!(flux3_run_block_reason(&settings, &Flux3ModelReadiness::NoRoot, Flux3SeedInput::Random).is_some());
        let missing = Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::Autoencoder]));
        let download = flux3_run_block_reason(&settings, &missing, Flux3SeedInput::Random).expect("missing blocks");
        let manual = Flux3KleinSettings { source_mode: "manual".to_string(), model_root: "/m".to_string(), ..Flux3KleinSettings::default() };
        let manual_reason = flux3_run_block_reason(&manual, &missing, Flux3SeedInput::Random).expect("missing blocks");
        assert_ne!(download, manual_reason, "the fix differs by mode, so does the sentence");
        let blank = Flux3KleinSettings { prompt: " ".to_string(), ..Flux3KleinSettings::default() };
        assert!(flux3_run_block_reason(&blank, &Flux3ModelReadiness::Ready, Flux3SeedInput::Random).is_some());
        assert!(flux3_run_block_reason(&settings, &Flux3ModelReadiness::Ready, Flux3SeedInput::Invalid).is_some());
    }

    /// The readiness line: only the missing / no-folder lines offer «Установить», a forecast that
    /// does not fit turns the ready line amber.
    #[test]
    fn the_readiness_line_follows_the_verdict() {
        let summary = |fits| Flux3MemorySummary { line: "≈ 8 GiB".to_string(), fits };
        assert_eq!(flux3_readiness_line(&Flux3ModelReadiness::Ready, Some(summary(false))).tone(), Flux3LineTone::Warn);
        assert_eq!(flux3_readiness_line(&Flux3ModelReadiness::Ready, Some(summary(true))).tone(), Flux3LineTone::Ok);
        assert!(!flux3_readiness_line(&Flux3ModelReadiness::Ready, None).offers_install());
        assert!(flux3_readiness_line(&Flux3ModelReadiness::NoRoot, None).offers_install());
        let missing = Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::TextEncoder]));
        assert!(flux3_readiness_line(&missing, Some(summary(true))).offers_install());
        assert_eq!(flux3_readiness_line(&Flux3ModelReadiness::Unknown, None).tone(), Flux3LineTone::Neutral);
    }

    /// Every stage the backend sends has a name; an unknown one falls back to the label.
    #[test]
    fn every_backend_stage_has_a_name() {
        for stage in FLUX3_STAGES {
            assert!(flux3_stage_label(stage).is_some(), "{stage}");
        }
        assert!(flux3_stage_label("some_future_stage").is_none());
    }

    /// The finished line names the seed; a degraded run also names the steps it ran as.
    #[test]
    fn the_done_line_names_the_seed() {
        let _locale = english();
        assert!(flux3_run_done_status(1234, None).contains("1234"));
        let degraded = RunDegraded { vram: Some(VramStep::Half), ram: None, reason: None };
        let line = flux3_run_done_status(99, Some(&degraded));
        assert!(line.contains("99") && line.contains(VramStep::Half.label()), "{line}");
    }
}
