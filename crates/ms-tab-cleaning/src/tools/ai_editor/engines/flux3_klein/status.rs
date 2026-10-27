/*
File: cleaning/tools/ai_editor/engines/flux3_klein/status.rs

Purpose:
The `inpaint.flux3_klein.status` answer as this side reads it, and the ONE verdict derived from
it: is the model package usable for the selected DiT and text-encoder formats right now.

Main responsibilities:
- parse the answer tolerantly (`parse_flux3_status`): every field optional, an absent one
  "not present" / "not known", never an error; the per-format rows are keyed by the wire id
  the backend echoes (protocol 24: every id `params.dit_formats` / `text_encoder_formats`
  listed), so an id this build does not know is kept and simply never asked about;
- guard it against staleness: it is an answer ABOUT ONE PACKAGE ROOT and counts only while it
  still describes the root the settings name (`flux3_status_for_root`); its cache half
  (`prompt_cached`, `prompt_cache_entry`) is also an answer about ONE PROMPT
  (`prompt_cache_asked_about`, compared by the shared `prompt_cache_state_for`);
- decide `Flux3ModelReadiness` (`flux3_model_readiness`): `Unknown` never blocks a run,
  `NoRoot`, `RootNotAbsolute` and `Missing` do and say what to fix; a prompt served from the
  cache in TEXT mode waives the text encoder.

Key structures:
- `Flux3Status`, `Flux3FormatStatus`, `Flux3Part`, `Flux3MissingParts`, `Flux3ModelReadiness`

Key functions:
- `parse_flux3_status()`, `flux3_status_for_root()`, `flux3_model_readiness()`

Notes:
`.status` checks files only and never loads or probes (`service.status`), so asking it is
cheap; it answers every offered format at once (the request carries all candidates), which is
why the staleness key is the root alone and a format switch needs no new question.
*/

use super::*;
use std::collections::BTreeMap;

/// What `.status` says about one weight format (a DiT row of `formats`, or an encoder row of
/// `text_encoder_formats`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux3FormatStatus {
    pub(super) available: bool,
    /// The backend's reason when not available (its own words, shown on hover).
    pub(super) reason: String,
    /// The weight bytes on disk (`dit_bytes` of a DiT row, `bytes` of an encoder row); 0 when
    /// absent or not reported.
    pub(super) bytes: u64,
}

/// One `.status` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux3Status {
    /// The selected formats (the request's) are runnable.
    pub(super) available: bool,
    pub(super) reason: String,
    /// The DiT rows by wire id (`formats`); an id the answer omits reads as not available.
    pub(super) formats: BTreeMap<String, Flux3FormatStatus>,
    /// The encoder rows by wire id (`text_encoder_formats`): `bf16` (the package's
    /// `text_encoder/` shards) plus every offered GGUF encoder.
    pub(super) text_encoder_formats: BTreeMap<String, Flux3FormatStatus>,
    pub(super) ae_present: bool,
    /// The SELECTED encoder format is present (the bf16 shards, or the selected GGUF file).
    pub(super) text_encoder_present: bool,
    /// A pipeline is resident in the backend.
    pub(super) loaded: bool,
    /// Where the DiT sits (`cuda:0`, `cpu`), `None` when not resident or not reported.
    pub(super) dit_device: Option<String>,
    /// Where the text encoder sits, `None` when not resident or not reported.
    pub(super) encoder_resident: Option<String>,
    /// The backend's selected discrete device, empty when not reported.
    pub(super) device: String,
    /// Whether the prompt the question carried is served from the prompt cache (memory or
    /// library) in TEXT mode. Three-state, deliberately NOT defaulted to `false`: an answer
    /// without the field is "not known", never "not cached".
    pub(super) prompt_cached: Option<bool>,
    /// The entry a run would use for that prompt (`EntryInfo`), `None` without one.
    pub(super) prompt_cache_entry: Option<PromptLibraryEntry>,
    /// The trimmed prompt the question carried (`params.prompt`), recorded by the worker that
    /// asked (`fetch_flux3_status`): the cache half counts only while the field still holds it.
    pub(super) prompt_cache_asked_about: Option<String>,
}

impl Flux3Status {
    /// The answer about `format`'s DiT, `None` when the answer has no row for it.
    pub(super) fn dit(&self, format: Flux3DitFormat) -> Option<&Flux3FormatStatus> {
        self.formats.get(format.wire())
    }

    /// Whether `format`'s DiT is present.
    pub(super) fn dit_available(&self, format: Flux3DitFormat) -> bool {
        self.dit(format).is_some_and(|row| row.available)
    }

    /// Whether the text encoder of `format` is present: its own row (protocol 24 answers one
    /// for `bf16` and for every offered GGUF id). Only without a row does it fall back to
    /// `text_encoder_present`, which describes the request's SELECTED encoder format — never
    /// read it for another format, or a present GGUF selection would report absent bf16
    /// shards as present.
    pub(super) fn encoder_available(&self, format: Flux3EncoderFormat) -> bool {
        self.text_encoder_formats.get(format.wire()).map_or(self.text_encoder_present, |row| row.available)
    }

    /// The backend's reason about the encoder of `format` (its row), empty without a row.
    pub(super) fn encoder_reason(&self, format: Flux3EncoderFormat) -> &str {
        self.text_encoder_formats.get(format.wire()).map_or("", |row| row.reason.as_str())
    }
}

/// Parses one per-format map (`{id: {available, reason, <bytes_field>}}`); a non-object
/// entry or a missing map reads as no rows.
fn parse_format_rows(header: &Value, field: &str, bytes_field: &str) -> BTreeMap<String, Flux3FormatStatus> {
    header
        .get(field)
        .and_then(Value::as_object)
        .map(|rows| {
            rows.iter()
                .filter(|(_, row)| row.is_object())
                .map(|(id, row)| {
                    let status = Flux3FormatStatus {
                        available: row.get("available").and_then(Value::as_bool).unwrap_or(false),
                        reason: row.get("reason").and_then(Value::as_str).unwrap_or_default().to_string(),
                        bytes: row.get(bytes_field).and_then(Value::as_u64).unwrap_or(0),
                    };
                    (id.clone(), status)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parses a `.status` answer (PROTOCOL.md §5.4 «FLUX.3 klein region editing»). The cache half
/// is the shared reader (`parse_prompt_cache_status`); `prompt_cache_asked_about` is not part of
/// the answer and stays `None` here.
pub(super) fn parse_flux3_status(header: &Value) -> Flux3Status {
    let flag = |name: &str| header.get(name).and_then(Value::as_bool).unwrap_or(false);
    let text = |name: &str| header.get(name).and_then(Value::as_str).unwrap_or_default().to_string();
    let optional_text = |name: &str| header.get(name).and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_string);
    let (prompt_cached, prompt_cache_entry) = parse_prompt_cache_status(header);
    Flux3Status {
        available: flag("available"),
        reason: text("reason"),
        formats: parse_format_rows(header, "formats", "dit_bytes"),
        text_encoder_formats: parse_format_rows(header, "text_encoder_formats", "bytes"),
        ae_present: flag("ae_present"),
        text_encoder_present: flag("text_encoder_present"),
        loaded: flag("loaded"),
        dit_device: optional_text("dit_device"),
        encoder_resident: optional_text("encoder_resident"),
        device: text("device"),
        prompt_cached,
        prompt_cache_entry,
        prompt_cache_asked_about: None,
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
    Dit(Flux3DitFormat),
    /// `ae.safetensors`.
    Autoencoder,
    /// The text encoder of a weight format (`text_encoder/` or its GGUF file).
    TextEncoder(Flux3EncoderFormat),
}

impl Flux3Part {
    /// The localized name of the part.
    pub(super) fn label(self) -> String {
        match self {
            Self::Dit(format) => tf!("cleaning.tools.flux3_klein.part_dit", format = flux3_dit_format_name(format)),
            Self::Autoencoder => t!("cleaning.tools.flux3_klein.part_autoencoder").to_string(),
            Self::TextEncoder(format) => match format.quant_token() {
                None => t!("cleaning.tools.flux3_klein.part_text_encoder").to_string(),
                Some(_) => tf!("cleaning.tools.flux3_klein.part_text_encoder_format", format = flux3_encoder_format_name(format)),
            },
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
    /// A package folder that is not an absolute path (typed relative, `~/…`, half-typed):
    /// decided locally, the backend is never asked about it.
    RootNotAbsolute,
    /// The answer names parts that are not there.
    Missing(Flux3MissingParts),
    /// Every part is there.
    Ready,
}

/// THE verdict. `status` must already be guarded by [`flux3_status_for_root`].
///
/// `prompt_cached` is the three-state `.status` answer for the prompt in the field (already
/// guarded by `prompt_cache_state_for`). **A cached prompt waives the text encoder in TEXT
/// mode** (FLUX.2 klein's rule): the run then never acquires the encoder, so its weights may be
/// absent on disk. The waiver needs `Some(true)` and nothing weaker (`None` is an outstanding
/// answer, or a backend without the field, which cannot run without an encoder either), and the
/// mode the wire carries (`text_mode_in_effect`): in `vl_refs` every run encodes. The DiT and the
/// AE are never waived; the backend keeps the final word.
#[must_use]
pub(super) fn flux3_model_readiness(settings: &Flux3KleinSettings, status: Option<&Flux3Status>, prompt_cached: Option<bool>) -> Flux3ModelReadiness {
    if settings.effective_root().is_empty() {
        return Flux3ModelReadiness::NoRoot;
    }
    if !settings.has_usable_root() {
        return Flux3ModelReadiness::RootNotAbsolute;
    }
    let Some(status) = status else {
        return Flux3ModelReadiness::Unknown;
    };
    let format = settings.dit_format();
    let encoder = settings.encoder_format();
    let mut missing = Vec::new();
    if !status.dit_available(format) {
        missing.push(Flux3Part::Dit(format));
    }
    if !status.ae_present {
        missing.push(Flux3Part::Autoencoder);
    }
    let encoder_waived = prompt_cached == Some(true) && settings.text_mode_in_effect();
    if !encoder_waived && !status.encoder_available(encoder) {
        missing.push(Flux3Part::TextEncoder(encoder));
    }
    if missing.is_empty() { Flux3ModelReadiness::Ready } else { Flux3ModelReadiness::Missing(Flux3MissingParts(missing)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer shape the backend serves (`service.status`, protocol 24): a row per listed
    /// DiT id (natives and GGUF), a row for `bf16` and per GGUF encoder id, the two shared
    /// parts, and the health fields. An id this build does not know is kept, never an error.
    #[test]
    fn a_status_answer_is_parsed_with_every_format_row() {
        let status = parse_flux3_status(&json!({
            "available": false,
            "reason": "нет файла",
            "formats": {
                "bf16": { "available": true, "reason": null, "dit_bytes": 11_100_000_000u64 },
                "fp8r": { "available": false, "reason": "нет variants/fp8r", "dit_bytes": 0 },
                "nvfp4": { "available": true, "reason": null, "dit_bytes": 5_280_000_000u64 },
                "q8_0": { "available": true, "reason": null, "dit_bytes": 6_092_755_872u64 },
                "q4_k_m": { "available": false, "reason": "no such file", "dit_bytes": 0 },
                "some_future_quant": { "available": true },
                "broken": 7
            },
            "text_encoder_formats": {
                "bf16": { "available": true, "reason": null, "bytes": 8_875_000_000u64 },
                "q8_0": { "available": true, "reason": null, "bytes": 4_280_406_144u64 },
                "q4_k_m": { "available": false, "reason": "no such file", "bytes": 0 }
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
        assert!(status.dit_available(Flux3DitFormat::Bf16));
        assert_eq!(status.dit(Flux3DitFormat::Bf16).map(|row| row.bytes), Some(11_100_000_000));
        assert_eq!(status.dit(Flux3DitFormat::Fp8r).map(|row| row.reason.as_str()), Some("нет variants/fp8r"));
        assert!(status.dit_available(Flux3DitFormat::Nvfp4));
        assert!(status.dit_available(Flux3DitFormat::Q80));
        assert!(!status.dit_available(Flux3DitFormat::Q4KM));
        assert!(!status.dit_available(Flux3DitFormat::Q6K), "an omitted id is not available");
        assert!(status.formats.contains_key("some_future_quant"), "an unknown id is kept");
        assert!(!status.formats.contains_key("broken"), "a malformed row is dropped");
        assert!(status.encoder_available(Flux3EncoderFormat::Q80));
        assert!(!status.encoder_available(Flux3EncoderFormat::Q4KM));
        assert_eq!(status.encoder_reason(Flux3EncoderFormat::Q4KM), "no such file");
        assert!(status.encoder_available(Flux3EncoderFormat::Bf16), "bf16 reads its own row");
        assert_eq!(status.encoder_resident.as_deref(), Some("cpu"));
        assert_eq!(status.dit_device.as_deref(), Some("cuda:0"));
        let silent = parse_flux3_status(&json!({}));
        assert_eq!(silent, Flux3Status::default(), "an empty answer reads as nothing present, never as an error");
        // Without a row the selected GGUF encoder falls back to the aggregate flag.
        let aggregate = parse_flux3_status(&json!({ "text_encoder_present": true }));
        assert!(aggregate.encoder_available(Flux3EncoderFormat::Q4KM));
    }

    /// A `.status` answer RECORDED from the Python side: `Flux3KleinService.status`
    /// (`modules/ai_backend/inpaint/flux3_klein/service.py`, the answer dict at the end of
    /// `status`; DiT rows from `large_model.formats`, encoder rows from
    /// `config.text_encoder_availability`) over the service test rig's package
    /// (`_service_test_fixtures.write_package` with fp8r + bf16 exports), a placeholder q8_0
    /// GGUF DiT (11 bytes) and q8_0 GGUF encoder (7 bytes) at their catalog paths, and the
    /// params this engine sends (`to_params`: every DiT id of `Flux3DitFormat::all`, both GGUF
    /// encoders, `format` = `text_encoder_format` = `q8_0`). The temp root is replaced by a
    /// fictional one. Field names (`dit_bytes` vs `bytes`) and the bf16 encoder row are what
    /// this pins.
    const RECORDED_STATUS: &str = r#"{
 "available": true, "reason": null,
 "formats": {
  "bf16": {"available": true, "reason": null, "dit_bytes": 432},
  "fp8r": {"available": true, "reason": null, "dit_bytes": 432},
  "q8_0": {"available": true, "reason": null, "dit_bytes": 11},
  "q6_k": {"available": false, "reason": "FLUX.3 klein (q6_k): не найдены файлы модели: /home/u/flux3/transformer_gguf/flux-3-image-klein-Q6_K.gguf", "dit_bytes": 0},
  "q5_k_m": {"available": false, "reason": "FLUX.3 klein (q5_k_m): не найдены файлы модели: /home/u/flux3/transformer_gguf/flux-3-image-klein-Q5_K_M.gguf", "dit_bytes": 0},
  "nvfp4": {"available": false, "reason": "FLUX.3 klein (nvfp4): не найдены файлы модели: /home/u/flux3/variants/nvfp4/model.safetensors, /home/u/flux3/variants/nvfp4/config.json", "dit_bytes": 0},
  "q4_k_m": {"available": false, "reason": "FLUX.3 klein (q4_k_m): не найдены файлы модели: /home/u/flux3/transformer_gguf/flux-3-image-klein-Q4_K_M.gguf", "dit_bytes": 0}
 },
 "text_encoder_formats": {
  "bf16": {"available": true, "reason": null, "bytes": 624},
  "q8_0": {"available": true, "reason": null, "bytes": 7},
  "q4_k_m": {"available": false, "reason": "FLUX.3 klein (текстовый энкодер): не найден файл формата «q4_k_m»: /home/u/flux3/text_encoder_gguf/Qwen3VL-4B-Instruct-Q4_K_M.gguf", "bytes": 0}
 },
 "ae_present": true, "text_encoder_present": true,
 "device": null, "model_key": null, "variant": null, "fp8_compute": null, "compute_path": null,
 "vram_step": null, "ram_step": null, "degraded": false, "last_error": null,
 "loaded": false, "ready": false, "dit_device": null, "encoder_resident": null
}"#;

    /// The recorded Python answer parses into every row with its bytes and reasons, and the
    /// verdict for the selection it was asked about is `Ready`.
    #[test]
    fn a_status_answer_recorded_from_the_backend_is_read() {
        let header: Value = serde_json::from_str(RECORDED_STATUS).expect("the recorded answer is JSON");
        let status = parse_flux3_status(&header);
        let ids: Vec<&str> = status.formats.keys().map(String::as_str).collect();
        let mut expected: Vec<&str> = Flux3DitFormat::all().iter().map(|format| format.wire()).collect();
        expected.sort_unstable();
        assert_eq!(ids, expected, "one row per offered DiT id");
        assert_eq!(status.dit(Flux3DitFormat::Q80).map(|row| row.bytes), Some(11), "`dit_bytes` of a GGUF row");
        assert_eq!(status.dit(Flux3DitFormat::Fp8r).map(|row| row.bytes), Some(432));
        assert!(!status.dit_available(Flux3DitFormat::Nvfp4));
        assert!(status.dit(Flux3DitFormat::Q4KM).is_some_and(|row| row.reason.contains("Q4_K_M.gguf")));
        assert_eq!(status.text_encoder_formats.get("bf16").map(|row| row.bytes), Some(624), "`bytes` of an encoder row");
        assert_eq!(status.text_encoder_formats.get("q8_0").map(|row| row.bytes), Some(7));
        assert!(status.encoder_available(Flux3EncoderFormat::Q80));
        assert!(status.encoder_available(Flux3EncoderFormat::Bf16));
        assert!(!status.encoder_available(Flux3EncoderFormat::Q4KM));
        assert!(status.encoder_reason(Flux3EncoderFormat::Q4KM).contains("q4_k_m"));
        assert!(status.ae_present && status.available && !status.loaded);
        assert!(status.dit_device.is_none() && status.device.is_empty(), "null health fields read as absent");
        let selected = Flux3KleinSettings { format: "q8_0".to_string(), text_encoder_format: "q8_0".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(flux3_model_readiness(&selected, Some(&status), None), Flux3ModelReadiness::Ready);
        // `text_encoder_present` describes the SELECTED (q8_0) encoder: bf16 must read its own
        // row, so absent shards stay absent while the GGUF selection is present.
        let mut no_shards = header;
        no_shards["text_encoder_formats"]["bf16"] = serde_json::json!({ "available": false, "reason": "нет text_encoder", "bytes": 0 });
        let status = parse_flux3_status(&no_shards);
        assert!(status.text_encoder_present);
        assert!(!status.encoder_available(Flux3EncoderFormat::Bf16));
        let bf16 = Flux3KleinSettings { format: "q8_0".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(
            flux3_model_readiness(&bf16, Some(&status), None),
            Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::TextEncoder(Flux3EncoderFormat::Bf16)]))
        );
    }

    /// The verdict: no folder decides locally, no answer never blocks, a missing part is named.
    #[test]
    fn the_readiness_verdict_names_what_is_missing() {
        let manual_empty = Flux3KleinSettings { source_mode: "manual".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(flux3_model_readiness(&manual_empty, None, None), Flux3ModelReadiness::NoRoot);
        // A relative folder is decided locally too, even with a (stale) answer at hand.
        for relative in ["models/flux3", "~/models/flux3", "D"] {
            let manual_relative = Flux3KleinSettings { source_mode: "manual".to_string(), model_root: relative.to_string(), ..Flux3KleinSettings::default() };
            assert!(!manual_relative.has_usable_root(), "{relative}");
            assert_eq!(flux3_model_readiness(&manual_relative, None, None), Flux3ModelReadiness::RootNotAbsolute, "{relative}");
            assert_eq!(flux3_model_readiness(&manual_relative, Some(&status_with(&[Flux3DitFormat::Fp8r], true, true)), None), Flux3ModelReadiness::RootNotAbsolute, "{relative}");
        }
        let settings = Flux3KleinSettings::default();
        assert_eq!(flux3_model_readiness(&settings, None, None), Flux3ModelReadiness::Unknown);
        let all = status_with(&[Flux3DitFormat::Fp8r], true, true);
        assert_eq!(flux3_model_readiness(&settings, Some(&all), None), Flux3ModelReadiness::Ready);
        let only_bf16 = status_with(&[Flux3DitFormat::Bf16], false, true);
        assert_eq!(
            flux3_model_readiness(&settings, Some(&only_bf16), None),
            Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::Dit(Flux3DitFormat::Fp8r), Flux3Part::Autoencoder]))
        );
        let bf16 = Flux3KleinSettings { format: "bf16".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(
            flux3_model_readiness(&bf16, Some(&only_bf16), None),
            Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::Autoencoder])),
            "a format switch is answered by the same status"
        );
        // A GGUF encoder is judged by its own row.
        let mut gguf_encoder = status_with(&[Flux3DitFormat::Q80], true, true);
        gguf_encoder.text_encoder_formats.insert("q4_k_m".to_string(), Flux3FormatStatus { available: false, reason: String::new(), bytes: 0 });
        let gguf = Flux3KleinSettings { format: "q8_0".to_string(), text_encoder_format: "q4_k_m".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(
            flux3_model_readiness(&gguf, Some(&gguf_encoder), None),
            Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::TextEncoder(Flux3EncoderFormat::Q4KM)]))
        );
    }

    /// A prompt served from the cache waives the text encoder in TEXT mode only: `Some(true)` and
    /// nothing weaker, never in `vl_refs`, and a GGUF encoder (normalized to `text`) waives. The
    /// DiT and the AE are never waived.
    #[test]
    fn a_cached_prompt_waives_the_text_encoder_in_text_mode_only() {
        let no_encoder = status_with(&[Flux3DitFormat::Fp8r], true, false);
        let text = Flux3KleinSettings::default();
        let missing_encoder = Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::TextEncoder(Flux3EncoderFormat::Bf16)]));
        assert_eq!(flux3_model_readiness(&text, Some(&no_encoder), Some(true)), Flux3ModelReadiness::Ready, "the encoder may be absent");
        assert_eq!(flux3_model_readiness(&text, Some(&no_encoder), Some(false)), missing_encoder);
        assert_eq!(flux3_model_readiness(&text, Some(&no_encoder), None), missing_encoder, "an outstanding answer waives nothing");
        let vl = Flux3KleinSettings { vl_refs: true, ..Flux3KleinSettings::default() };
        assert_eq!(flux3_model_readiness(&vl, Some(&no_encoder), Some(true)), missing_encoder, "VL_REFS always encodes");
        let gguf = Flux3KleinSettings { vl_refs: true, text_encoder_format: "q8_0".to_string(), ..Flux3KleinSettings::default() };
        let mut gguf_absent = no_encoder.clone();
        gguf_absent.text_encoder_formats.insert("q8_0".to_string(), Flux3FormatStatus::default());
        assert_eq!(flux3_model_readiness(&gguf, Some(&gguf_absent), Some(true)), Flux3ModelReadiness::Ready, "a GGUF encoder is sent as text: waived");
        let nothing = status_with(&[], false, false);
        assert_eq!(
            flux3_model_readiness(&text, Some(&nothing), Some(true)),
            Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::Dit(Flux3DitFormat::Fp8r), Flux3Part::Autoencoder])),
            "the DiT and the AE are never waived"
        );
    }

    /// The cache half of the answer is the shared reader: three-state, the entry tolerant, and
    /// the asked-about prompt is the worker's to fill.
    #[test]
    fn the_status_cache_half_is_read() {
        let status = parse_flux3_status(&json!({ "prompt_cached": true, "prompt_cache_entry": { "file": "a.msprompt", "encoder_source": "stock", "encoder_format": "q8_0", "origin": "built" } }));
        assert_eq!(status.prompt_cached, Some(true));
        assert_eq!(status.prompt_cache_entry.as_ref().map(|entry| entry.encoder_format.as_str()), Some("q8_0"));
        assert_eq!(status.prompt_cache_asked_about, None);
        let silent = parse_flux3_status(&json!({ "text_encoder_present": true }));
        assert_eq!((silent.prompt_cached, silent.prompt_cache_entry), (None, None), "an older answer is not known, never not cached");
        assert_eq!(parse_flux3_status(&json!({ "prompt_cached": false })).prompt_cached, Some(false));
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
