/*
File: cleaning/tools/ai_editor/engines/flux3_klein/status.rs

Purpose:
The `inpaint.flux3_klein.status` answer as this side reads it, and the ONE verdict derived from
it: is the model package usable for the selected weight format right now.

Main responsibilities:
- parse the answer tolerantly (`parse_flux3_status`): every field optional, an absent one
  "not present" / "not known", never an error;
- guard it against staleness: it is an answer ABOUT ONE PACKAGE ROOT and counts only while it
  still describes the root the settings name (`flux3_status_for_root`);
- decide `Flux3ModelReadiness` (`flux3_model_readiness`): `Unknown` never blocks a run,
  `NoRoot` and `Missing` do and say what to fix.

Key structures:
- `Flux3Status`, `Flux3FormatStatus`, `Flux3Part`, `Flux3MissingParts`, `Flux3ModelReadiness`

Key functions:
- `parse_flux3_status()`, `flux3_status_for_root()`, `flux3_model_readiness()`

Notes:
`.status` checks files only and never loads or probes (`service.status`), so asking it is
cheap; it answers all three formats at once, which is why the staleness key is the root
alone and a format switch needs no new question.
*/

use super::*;

/// What `.status` says about one weight format's DiT.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux3FormatStatus {
    pub(super) available: bool,
    /// The backend's reason when not available (its own words, shown on hover).
    pub(super) reason: String,
    pub(super) dit_bytes: u64,
}

/// One `.status` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux3Status {
    /// The selected format (the request's) is runnable.
    pub(super) available: bool,
    pub(super) reason: String,
    /// The three formats, in `Flux3Format::all()` order; a format the answer omits reads as
    /// not available with no reason.
    pub(super) formats: [Flux3FormatStatus; 3],
    pub(super) ae_present: bool,
    pub(super) text_encoder_present: bool,
    /// A pipeline is resident in the backend.
    pub(super) loaded: bool,
    /// Where the DiT sits (`cuda:0`, `cpu`), `None` when not resident or not reported.
    pub(super) dit_device: Option<String>,
    /// Where the text encoder sits, `None` when not resident or not reported.
    pub(super) encoder_resident: Option<String>,
    /// The backend's selected discrete device, empty when not reported.
    pub(super) device: String,
}

impl Flux3Status {
    /// The answer about `format`'s DiT.
    pub(super) fn format(&self, format: Flux3Format) -> &Flux3FormatStatus {
        match format {
            Flux3Format::Bf16 => &self.formats[0],
            Flux3Format::Fp8r => &self.formats[1],
            Flux3Format::Nvfp4 => &self.formats[2],
        }
    }
}

/// Parses a `.status` answer (PROTOCOL.md §5.4 «FLUX.3 klein region editing»).
pub(super) fn parse_flux3_status(header: &Value) -> Flux3Status {
    let flag = |name: &str| header.get(name).and_then(Value::as_bool).unwrap_or(false);
    let text = |name: &str| header.get(name).and_then(Value::as_str).unwrap_or_default().to_string();
    let optional_text = |name: &str| header.get(name).and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_string);
    let format = |format: Flux3Format| {
        let entry = header.get("formats").and_then(|formats| formats.get(format.wire()));
        Flux3FormatStatus {
            available: entry.and_then(|entry| entry.get("available")).and_then(Value::as_bool).unwrap_or(false),
            reason: entry.and_then(|entry| entry.get("reason")).and_then(Value::as_str).unwrap_or_default().to_string(),
            dit_bytes: entry.and_then(|entry| entry.get("dit_bytes")).and_then(Value::as_u64).unwrap_or(0),
        }
    };
    Flux3Status {
        available: flag("available"),
        reason: text("reason"),
        formats: Flux3Format::all().map(format),
        ae_present: flag("ae_present"),
        text_encoder_present: flag("text_encoder_present"),
        loaded: flag("loaded"),
        dit_device: optional_text("dit_device"),
        encoder_resident: optional_text("encoder_resident"),
        device: text("device"),
    }
}

/// The answer, but only while it still describes `current_root` — the root the settings name
/// now. `answered_root` is the root the answer was asked about.
pub(super) fn flux3_status_for_root<'a>(status: Option<&'a Flux3Status>, answered_root: Option<&str>, current_root: &str) -> Option<&'a Flux3Status> {
    status.filter(|_| answered_root == Some(current_root))
}

/// One part of the package a run needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux3Part {
    /// The DiT of a weight format.
    Dit(Flux3Format),
    /// `ae.safetensors`.
    Autoencoder,
    /// `text_encoder/`.
    TextEncoder,
}

impl Flux3Part {
    /// The localized name of the part.
    pub(super) fn label(self) -> String {
        match self {
            Self::Dit(format) => tf!("cleaning.tools.flux3_klein.part_dit", format = format.label()),
            Self::Autoencoder => t!("cleaning.tools.flux3_klein.part_autoencoder").to_string(),
            Self::TextEncoder => t!("cleaning.tools.flux3_klein.part_text_encoder").to_string(),
        }
    }
}

/// The parts a verdict found missing, in a fixed order, never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Flux3MissingParts(pub(super) Vec<Flux3Part>);

impl Flux3MissingParts {
    /// The localized comma-separated list.
    pub(super) fn names(&self) -> String {
        self.0.iter().map(|part| part.label()).collect::<Vec<_>>().join(", ")
    }
}

/// Whether the model package is usable for the selected format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux3ModelReadiness {
    /// No answer about the current root yet. NEVER blocks a run: an unanswered question is no
    /// evidence of a missing model, and blocking on it leaves «Обработать» dead on a slow backend.
    Unknown,
    /// Manual mode without a package folder: decided locally, no backend needed.
    NoRoot,
    /// The answer names parts that are not there.
    Missing(Flux3MissingParts),
    /// Every part is there.
    Ready,
}

/// THE verdict. `status` must already be guarded by [`flux3_status_for_root`].
#[must_use]
pub(super) fn flux3_model_readiness(settings: &Flux3KleinSettings, status: Option<&Flux3Status>) -> Flux3ModelReadiness {
    if settings.effective_root().is_empty() {
        return Flux3ModelReadiness::NoRoot;
    }
    let Some(status) = status else {
        return Flux3ModelReadiness::Unknown;
    };
    let format = settings.format();
    let mut missing = Vec::new();
    if !status.format(format).available {
        missing.push(Flux3Part::Dit(format));
    }
    if !status.ae_present {
        missing.push(Flux3Part::Autoencoder);
    }
    if !status.text_encoder_present {
        missing.push(Flux3Part::TextEncoder);
    }
    if missing.is_empty() { Flux3ModelReadiness::Ready } else { Flux3ModelReadiness::Missing(Flux3MissingParts(missing)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer shape the backend serves (`service.status`): three formats, the two shared
    /// parts, and the health fields.
    #[test]
    fn a_status_answer_is_parsed_with_every_format() {
        let status = parse_flux3_status(&json!({
            "available": false,
            "reason": "нет файла",
            "formats": {
                "bf16": { "available": true, "reason": null, "dit_bytes": 11_100_000_000u64 },
                "fp8r": { "available": false, "reason": "нет variants/fp8r", "dit_bytes": 0 },
                "nvfp4": { "available": true, "reason": null, "dit_bytes": 5_280_000_000u64 }
            },
            "ae_present": true,
            "text_encoder_present": true,
            "loaded": true,
            "device": "cuda:0",
            "dit_device": "cuda:0",
            "encoder_resident": "cpu",
            "fp8_compute": null
        }));
        assert!(!status.available);
        assert!(status.format(Flux3Format::Bf16).available);
        assert_eq!(status.format(Flux3Format::Bf16).dit_bytes, 11_100_000_000);
        assert_eq!(status.format(Flux3Format::Fp8r).reason, "нет variants/fp8r");
        assert!(status.format(Flux3Format::Nvfp4).available);
        assert_eq!(status.encoder_resident.as_deref(), Some("cpu"));
        assert_eq!(status.dit_device.as_deref(), Some("cuda:0"));
        let silent = parse_flux3_status(&json!({}));
        assert_eq!(silent, Flux3Status::default(), "an empty answer reads as nothing present, never as an error");
    }

    /// The verdict: no folder decides locally, no answer never blocks, a missing part is named.
    #[test]
    fn the_readiness_verdict_names_what_is_missing() {
        let manual_empty = Flux3KleinSettings { source_mode: "manual".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(flux3_model_readiness(&manual_empty, None), Flux3ModelReadiness::NoRoot);
        let settings = Flux3KleinSettings::default();
        assert_eq!(flux3_model_readiness(&settings, None), Flux3ModelReadiness::Unknown);
        let all = status_with(&[Flux3Format::Fp8r], true, true);
        assert_eq!(flux3_model_readiness(&settings, Some(&all)), Flux3ModelReadiness::Ready);
        let only_bf16 = status_with(&[Flux3Format::Bf16], false, true);
        assert_eq!(
            flux3_model_readiness(&settings, Some(&only_bf16)),
            Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::Dit(Flux3Format::Fp8r), Flux3Part::Autoencoder]))
        );
        let bf16 = Flux3KleinSettings { format: "bf16".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(
            flux3_model_readiness(&bf16, Some(&only_bf16)),
            Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::Autoencoder])),
            "a format switch is answered by the same status"
        );
    }

    /// An answer about another root is not about this package.
    #[test]
    fn an_answer_counts_only_for_the_root_it_was_asked_about() {
        let status = Flux3Status::default();
        assert!(flux3_status_for_root(Some(&status), Some("/a"), "/a").is_some());
        assert!(flux3_status_for_root(Some(&status), Some("/a"), "/b").is_none());
        assert!(flux3_status_for_root(Some(&status), None, "/a").is_none());
        assert!(flux3_status_for_root(None, Some("/a"), "/a").is_none());
    }
}
