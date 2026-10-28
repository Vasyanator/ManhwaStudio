/*
File: cleaning/tools/ai_editor/engines/flux3_klein/decisions.rs

Purpose:
The PURE decisions the FLUX.3 klein panel renders: which readiness line to show, whether a run
may start, what the seed field means, how a backend stage is named, what a finished run says,
which catalog bundle the selection needs, how a weight-format row is captioned, and what the
shared prompt-cache gate knows about the typed text and the current encoder.
None of them touches a `Ui`, a channel or the disk, so each is asserted without a frame.

Key structures:
- `Flux3ReadinessLine`, `Flux3LineTone`, `Flux3MemorySummary`, `Flux3SeedInput`

Key functions:
- `flux3_readiness_line()`, `flux3_run_block_reason()`, `parse_seed_input()`,
  `flux3_stage_label()`, `flux3_run_done_status()`
- the Stable-Diffusion.cpp runtime: `flux3_sdcpp_readiness_line()`, `flux3_torch_block_reason()`,
  `flux3_sdcpp_run_block_reason()`, `flux3_sdcpp_format_fallback_line()`, `FLUX3_SDCPP_SEED_MAX`
- `flux3_catalog_bundle()`; the gated-repository / bundle-state / job rules over it are the
  shared `../shared/catalog_download.rs`
- `flux3_dit_format_name()`, `flux3_encoder_format_name()`, `flux3_dit_format_hint()`,
  `flux3_encoder_format_hint()`, `flux3_dit_format_row()`, `flux3_encoder_format_row()`
- `flux3_cache_context()`, `flux3_encoder_caption()` (the prompt library's FLUX.3 facts; the
  gates, the line and the jobs are `../shared/prompt_cache.rs`)

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
    /// The one-line forecast of the whole run (`large_model_resources::forecast_line`).
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
    /// `.status` names nothing missing (or has not answered), but the catalog bundle of the
    /// selection is known to be incomplete (download mode): «Установить» opens the block that
    /// fetches it.
    BundleMissing,
    /// Manual mode without a folder; «Установить» is offered (it opens the section).
    NoRoot,
    /// Manual mode with a folder that is not an absolute path; «Установить» is offered (it
    /// opens the section, where the folder is fixed).
    RootNotAbsolute,
    /// No answer yet: neutral, no button.
    Unknown,
    /// Stable-Diffusion.cpp: the files of the selection are being checked (neutral, no button).
    SdcppFilesChecking,
    /// Stable-Diffusion.cpp: the files are there and fit together.
    SdcppFilesReady,
    /// Stable-Diffusion.cpp: the files are missing or unusable (the localized reason);
    /// «Установить» is offered.
    SdcppFilesRefused(String),
}

impl Flux3ReadinessLine {
    /// The colour of the line.
    pub(super) fn tone(&self) -> Flux3LineTone {
        match self {
            Self::Ready { memory: Some(summary) } if !summary.fits => Flux3LineTone::Warn,
            Self::Ready { .. } | Self::SdcppFilesReady => Flux3LineTone::Ok,
            Self::Missing { .. } | Self::BundleMissing | Self::NoRoot | Self::RootNotAbsolute | Self::SdcppFilesRefused(_) => Flux3LineTone::Warn,
            Self::Unknown | Self::SdcppFilesChecking => Flux3LineTone::Neutral,
        }
    }

    /// Whether «Установить» is offered beside the line.
    pub(super) fn offers_install(&self) -> bool {
        matches!(self, Self::Missing { .. } | Self::BundleMissing | Self::NoRoot | Self::RootNotAbsolute | Self::SdcppFilesRefused(_))
    }

    /// The localized text.
    pub(super) fn text(&self) -> String {
        match self {
            Self::Ready { memory: Some(summary) } => tf!("cleaning.tools.flux3_klein.model_ready_memory_status", memory = summary.line),
            Self::Ready { memory: None } => t!("cleaning.tools.flux3_klein.model_ready_status").to_string(),
            Self::Missing { components } => tf!("cleaning.tools.flux3_klein.model_missing_status", components = components),
            Self::BundleMissing => t!("cleaning.tools.flux3_klein.bundle_missing_status").to_string(),
            Self::NoRoot => t!("cleaning.tools.flux3_klein.model_root_missing_status").to_string(),
            Self::RootNotAbsolute => t!("cleaning.tools.flux3_klein.model_root_not_absolute_status").to_string(),
            Self::Unknown => t!("cleaning.tools.flux3_klein.model_readiness_unknown_status").to_string(),
            Self::SdcppFilesChecking => t!("cleaning.tools.flux3_klein.sdcpp_files_checking_status").to_string(),
            Self::SdcppFilesReady => t!("cleaning.tools.flux3_klein.sdcpp_files_ready_status").to_string(),
            Self::SdcppFilesRefused(reason) => tf!("cleaning.tools.flux3_klein.sdcpp_files_error", err = reason),
        }
    }
}

/// Turns the model verdict into the readiness line; `memory` counts only for `Ready`.
///
/// `bundle_missing` is whether the catalog bundle of the selection is known to be not fully
/// downloaded (`catalog_bundle_missing`; always `false` in manual mode). It turns a `Ready` or
/// `Unknown` verdict into [`Flux3ReadinessLine::BundleMissing`]; a `Missing` verdict keeps its
/// own line (it names the parts). It never gates a run: that stays the `.status` verdict.
#[must_use]
pub(super) fn flux3_readiness_line(readiness: &Flux3ModelReadiness, bundle_missing: bool, memory: Option<Flux3MemorySummary>) -> Flux3ReadinessLine {
    match readiness {
        Flux3ModelReadiness::Ready | Flux3ModelReadiness::Unknown if bundle_missing => Flux3ReadinessLine::BundleMissing,
        Flux3ModelReadiness::Ready => Flux3ReadinessLine::Ready { memory },
        Flux3ModelReadiness::Missing(missing) => Flux3ReadinessLine::Missing { components: missing.names() },
        Flux3ModelReadiness::NoRoot => Flux3ReadinessLine::NoRoot,
        Flux3ModelReadiness::RootNotAbsolute => Flux3ReadinessLine::RootNotAbsolute,
        Flux3ModelReadiness::Unknown => Flux3ReadinessLine::Unknown,
    }
}

/// The files line of the Stable-Diffusion.cpp runtime (the runtime's own readiness is the shared
/// line above it). `bundle_missing` (download mode, the catalog bundle known incomplete) takes a
/// checking, ready or file-refused state, so the line offers the block that fetches the files; a
/// root problem keeps its own line.
#[must_use]
pub(super) fn flux3_sdcpp_readiness_line(files: &Flux3SdcppFilesState, bundle_missing: bool) -> Flux3ReadinessLine {
    match files {
        Flux3SdcppFilesState::Refused(Flux3SdcppFilesError::NoRoot) => Flux3ReadinessLine::NoRoot,
        Flux3SdcppFilesState::Refused(Flux3SdcppFilesError::RootNotAbsolute) => Flux3ReadinessLine::RootNotAbsolute,
        Flux3SdcppFilesState::Checking | Flux3SdcppFilesState::Ready | Flux3SdcppFilesState::Refused(Flux3SdcppFilesError::Files(_)) if bundle_missing => Flux3ReadinessLine::BundleMissing,
        Flux3SdcppFilesState::Checking => Flux3ReadinessLine::SdcppFilesChecking,
        Flux3SdcppFilesState::Ready => Flux3ReadinessLine::SdcppFilesReady,
        Flux3SdcppFilesState::Refused(Flux3SdcppFilesError::Files(err)) => Flux3ReadinessLine::SdcppFilesRefused(err.to_string()),
    }
}

/// The largest seed a Stable-Diffusion.cpp run takes: its seed is a signed 64-bit integer whose
/// negative values mean "random".
pub(super) const FLUX3_SDCPP_SEED_MAX: u64 = i64::MAX.unsigned_abs();

/// The engine's half of the PyTorch requirement: PyTorch selected while Torch is missing names
/// the way out (switch to Stable-Diffusion.cpp) — but only where the sd.cpp route is available
/// (`sdcpp_available`: the shared `sdcpp_route_available`, platform and `--no-ai` included);
/// elsewhere `None`, and the host's generic «needs PyTorch» refusal applies.
#[must_use]
pub(super) fn flux3_torch_block_reason(runtime: ImageEditRuntime, torch_available: bool, sdcpp_available: bool) -> Option<String> {
    match runtime {
        ImageEditRuntime::PyTorch if !torch_available && sdcpp_available => Some(t!("cleaning.tools.sdcpp.torch_missing_hint").to_string()),
        ImageEditRuntime::PyTorch | ImageEditRuntime::SdCpp => None,
    }
}

/// The first reason a run on Stable-Diffusion.cpp cannot start, or `None`.
///
/// In order: the runtime itself (`runtime_block`, the shared readiness's
/// `SdCppReadiness::run_block_reason`), the files (still being checked — a run needs the
/// validated paths —, or refused with their reason), an empty prompt, an invalid seed field and a
/// pinned seed above [`FLUX3_SDCPP_SEED_MAX`].
#[must_use]
pub(super) fn flux3_sdcpp_run_block_reason(settings: &Flux3KleinSettings, runtime_block: Option<String>, files: &Flux3SdcppFilesState, seed: Flux3SeedInput) -> Option<String> {
    if runtime_block.is_some() {
        return runtime_block;
    }
    match files {
        Flux3SdcppFilesState::Checking => return Some(t!("cleaning.tools.flux3_klein.sdcpp_files_checking_status").to_string()),
        Flux3SdcppFilesState::Refused(err) => return Some(err.message()),
        Flux3SdcppFilesState::Ready => {}
    }
    if settings.prompt.trim().is_empty() {
        return Some(t!("cleaning.tools.flux3_klein.prompt_required_error").to_string());
    }
    match seed {
        Flux3SeedInput::Invalid => Some(t!("cleaning.tools.flux3_klein.seed_invalid_error").to_string()),
        Flux3SeedInput::Pinned(value) if value > FLUX3_SDCPP_SEED_MAX => Some(tf!("cleaning.tools.flux3_klein.sdcpp_seed_range_error", max = FLUX3_SDCPP_SEED_MAX)),
        Flux3SeedInput::Pinned(_) | Flux3SeedInput::Random => None,
    }
}

/// The one line under the DiT picker on Stable-Diffusion.cpp when the stored format is one it
/// cannot run (fp8r, nvfp4): which format runs instead. `None` on PyTorch or for an offered
/// format.
#[must_use]
pub(super) fn flux3_sdcpp_format_fallback_line(settings: &Flux3KleinSettings) -> Option<String> {
    let stored = settings.dit_format();
    let used = settings.run_dit_format();
    (stored != used).then(|| tf!("cleaning.tools.flux3_klein.sdcpp_format_fallback_status", stored = flux3_dit_format_name(stored), used = flux3_dit_format_name(used)))
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
        Flux3ModelReadiness::RootNotAbsolute => return Some(t!("cleaning.tools.flux3_klein.model_root_not_absolute_error").to_string()),
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

/// The catalog bundle the CURRENT selection needs in download mode, for the selected runtime
/// (`flux3_klein_package_specs_for`): on PyTorch base, root DiT config for bf16 / GGUF, DiT,
/// encoder; on Stable-Diffusion.cpp base, the DiT format it runs (`run_dit_format`), encoder.
/// Empty in manual mode, which never draws or probes a catalog block. Pure: no disk access.
#[must_use]
pub(super) fn flux3_catalog_bundle(settings: &Flux3KleinSettings) -> Vec<&'static ExternalModelSpec> {
    match settings.source_mode() {
        Flux3SourceMode::Download => flux3_klein_package_specs_for(settings.runtime(), settings.run_dit_format(), settings.encoder_format()),
        Flux3SourceMode::Manual => Vec::new(),
    }
}

/// The localized name of a DiT format: a native one by its own caption, a GGUF one as `GGUF`
/// with its quant token (a technical literal, `dev-docs/i18n_exclusions.md`).
#[must_use]
pub(super) fn flux3_dit_format_name(format: Flux3DitFormat) -> String {
    match format {
        Flux3DitFormat::Bf16 => t!("cleaning.tools.flux3_klein.format_bf16_label").to_string(),
        Flux3DitFormat::Fp8r => t!("cleaning.tools.flux3_klein.format_fp8r_label").to_string(),
        Flux3DitFormat::Nvfp4 => t!("cleaning.tools.flux3_klein.format_nvfp4_label").to_string(),
        Flux3DitFormat::Q80 | Flux3DitFormat::Q6K | Flux3DitFormat::Q5KM | Flux3DitFormat::Q4KM => {
            tf!("cleaning.tools.flux3_klein.format_gguf_label", quant = format.quant_token().unwrap_or_default())
        }
    }
}

/// The hover of one DiT format row: what choosing it costs.
#[must_use]
pub(super) fn flux3_dit_format_hint(format: Flux3DitFormat) -> &'static str {
    match format {
        Flux3DitFormat::Bf16 => t!("cleaning.tools.flux3_klein.format_bf16_hint"),
        Flux3DitFormat::Fp8r => t!("cleaning.tools.flux3_klein.format_fp8r_hint"),
        Flux3DitFormat::Nvfp4 => t!("cleaning.tools.flux3_klein.format_nvfp4_hint"),
        Flux3DitFormat::Q80 | Flux3DitFormat::Q6K | Flux3DitFormat::Q5KM | Flux3DitFormat::Q4KM => t!("cleaning.tools.flux3_klein.format_gguf_hint"),
    }
}

/// The localized name of a text-encoder format: the bf16 shards by their caption, a GGUF file
/// as `GGUF` with its quant token.
#[must_use]
pub(super) fn flux3_encoder_format_name(format: Flux3EncoderFormat) -> String {
    match format.quant_token() {
        None => t!("cleaning.tools.flux3_klein.text_encoder_format_bf16_label").to_string(),
        Some(quant) => tf!("cleaning.tools.flux3_klein.format_gguf_label", quant = quant),
    }
}

/// The hover of one text-encoder format row (a GGUF encoder also closes the references in the
/// encoder, D6).
#[must_use]
pub(super) fn flux3_encoder_format_hint(format: Flux3EncoderFormat) -> &'static str {
    if format.is_gguf() { t!("cleaning.tools.flux3_klein.text_encoder_format_gguf_hint") } else { t!("cleaning.tools.flux3_klein.text_encoder_format_bf16_hint") }
}

/// Appends the installed mark: a literal glyph, as in FLUX.2 klein's format rows.
fn with_installed_mark(label: String, installed: bool) -> String {
    if installed { format!("{label} ✓") } else { label }
}

/// The row (and closed-combo) caption of a DiT format: its name with the download size of its
/// catalog spec, and a trailing ✓ when `status` (already guarded to the current root) says its
/// file is there. No answer claims nothing.
#[must_use]
pub(super) fn flux3_dit_format_row(format: Flux3DitFormat, status: Option<&Flux3Status>) -> String {
    let label = tf!("cleaning.tools.flux3_klein.format_sized_label", format = flux3_dit_format_name(format), size = format_gib(flux3_klein_dit_spec(format).total_bytes()));
    with_installed_mark(label, status.is_some_and(|status| status.dit_available(format)))
}

/// The row caption of a text-encoder format: name, catalog download size, ✓ when present.
#[must_use]
pub(super) fn flux3_encoder_format_row(format: Flux3EncoderFormat, status: Option<&Flux3Status>) -> String {
    let label = tf!(
        "cleaning.tools.flux3_klein.format_sized_label",
        format = flux3_encoder_format_name(format),
        size = format_gib(flux3_klein_encoder_spec(format).total_bytes())
    );
    with_installed_mark(label, status.is_some_and(|status| status.encoder_available(format)))
}

/// The encoder source id of every FLUX.3 klein library entry: FLUX.3 has one encoder family
/// (the stock Qwen3-VL in bf16 shards or a GGUF quant), so the source is always `stock` (D12).
const FLUX3_PROMPT_ENCODER_SOURCE: &str = "stock";

/// The precision id of a weight-only fp8 encode in the shared gate. FLUX.3 has no fp8 encoder,
/// so no FLUX.3 entry carries it; an entry that did would rank after every known format
/// (`flux3_prompt_cache_precision_rank`), exactly as the backend ranks an unknown id.
const FLUX3_PROMPT_PRECISION_FP8: &str = "fp8";

/// What the shared «Кэшировать» gate knows about the typed text and the encoder `settings`
/// select:
/// - an encoder is always configured (download mode derives it; manual mode reads it under the
///   package folder, whose absence is the readiness line's business);
/// - `current` is `(stock, <encoder format id>)` in DOWNLOAD mode, where the catalog names the
///   encoder files, and `None` in manual mode (no "more precise" check then);
/// - the ranking is `ms_config::flux3_prompt_cache_precision_rank`;
/// - in `vl_refs` (`text_mode_in_effect` false) caching is closed with the VL reason: the
///   encoder also reads the images there, so every run encodes and nothing is cached.
#[must_use]
pub(super) fn flux3_cache_context(settings: &Flux3KleinSettings) -> CacheContext<'_> {
    let current = match settings.source_mode() {
        Flux3SourceMode::Download => Some((FLUX3_PROMPT_ENCODER_SOURCE.to_string(), settings.encoder_format().wire().to_string())),
        Flux3SourceMode::Manual => None,
    };
    CacheContext {
        prompt: &settings.prompt,
        encoder_configured: true,
        current,
        rank: flux3_prompt_cache_precision_rank,
        fp8_id: FLUX3_PROMPT_PRECISION_FP8,
        engine_block: (!settings.text_mode_in_effect()).then(|| t!("cleaning.tools.flux3_klein.prompt_cache_vl_refs_tooltip").to_string()),
    }
}

/// The localized name of the encoder that made a library entry: the encoder format's own name
/// (`BF16` shards / `GGUF <quant>`), an unknown format id verbatim, else the backend's
/// `encoder_label`. Empty only when the entry says nothing at all (the shared line then names
/// no encoder).
#[must_use]
pub(super) fn flux3_encoder_caption(entry: &PromptLibraryEntry) -> String {
    let format = entry.encoder_format.trim();
    if let Some(known) = Flux3EncoderFormat::from_wire(format) {
        return flux3_encoder_format_name(known);
    }
    if !format.is_empty() {
        return format.to_string();
    }
    entry.encoder_label.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(file: &str, format: &str, current: Option<bool>) -> PromptLibraryEntry {
        PromptLibraryEntry {
            file: file.to_string(),
            encoder_source: FLUX3_PROMPT_ENCODER_SOURCE.to_string(),
            encoder_format: format.to_string(),
            current_encoder: current,
            ..PromptLibraryEntry::default()
        }
    }

    fn preset(name: &str, entries: Vec<PromptLibraryEntry>) -> PromptLibraryText {
        PromptLibraryText { prompt: FLUX3_DEFAULT_PROMPT.to_string(), name: name.to_string(), selected: entries[0].clone(), entries }
    }

    /// The shared gate over the FLUX.3 context of `settings`, backend ready.
    fn flux3_cache_button(settings: &Flux3KleinSettings, hit: Option<&PromptLibraryText>, name: &str, prompt_cached: Option<bool>) -> CacheButton {
        cache_button(&flux3_cache_context(settings), hit, name, prompt_cached, Some(true), true)
    }

    /// In `vl_refs` «Кэшировать» is closed with the VL reason, right after readiness; a GGUF
    /// encoder (sent as `text`) keeps it open.
    #[test]
    fn caching_is_closed_in_vl_refs_mode_with_its_reason() {
        let _locale = english();
        let vl = Flux3KleinSettings { vl_refs: true, ..Flux3KleinSettings::default() };
        assert_eq!(flux3_cache_button(&vl, None, "", None), CacheButton::Closed(CacheBlock::Engine(t!("cleaning.tools.flux3_klein.prompt_cache_vl_refs_tooltip").to_string())));
        assert_eq!(cache_button(&flux3_cache_context(&vl), None, "", None, None, false), CacheButton::Closed(CacheBlock::NotReady));
        let gguf = Flux3KleinSettings { vl_refs: true, text_encoder_format: "q8_0".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(flux3_cache_button(&gguf, None, "", None), CacheButton::Cache);
        assert_eq!(flux3_cache_button(&Flux3KleinSettings::default(), None, "", None), CacheButton::Cache);
        let blank = Flux3KleinSettings { prompt: " ".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(flux3_cache_button(&blank, None, "", None), CacheButton::Closed(CacheBlock::NothingToEncode));
    }

    /// A strictly more precise entry closes a download-mode build; manual mode knows no current
    /// encoder and never closes for that reason.
    #[test]
    fn a_more_precise_entry_closes_caching_in_download_mode() {
        let q4 = Flux3KleinSettings { text_encoder_format: "q4_k_m".to_string(), ..Flux3KleinSettings::default() };
        let q8 = entry("q8.msprompt", "q8_0", Some(false));
        let hit = preset("sfx", vec![q8.clone()]);
        assert_eq!(flux3_cache_button(&q4, Some(&hit), "sfx", Some(true)), CacheButton::Closed(CacheBlock::MorePrecise(q8)));
        let bf16 = Flux3KleinSettings::default();
        assert_eq!(flux3_cache_button(&bf16, Some(&hit), "sfx", Some(true)), CacheButton::Cache, "a less precise entry keeps it open");
        let manual = Flux3KleinSettings { source_mode: "manual".to_string(), model_root: "/m/flux3".to_string(), ..q4 };
        assert_eq!(flux3_cache_button(&manual, Some(&hit), "sfx", Some(true)), CacheButton::Cache);
        // `.status` says the prompt is not served: the build stays open whatever the library holds.
        assert_eq!(flux3_cache_button(&Flux3KleinSettings { text_encoder_format: "q4_k_m".to_string(), ..Flux3KleinSettings::default() }, Some(&hit), "sfx", Some(false)), CacheButton::Cache);
    }

    /// The current encoder's own entry closes caching; a different name renames it.
    #[test]
    fn an_entry_of_the_current_encoder_offers_only_a_rename() {
        let settings = Flux3KleinSettings::default();
        let hit = preset("sfx", vec![entry("a.msprompt", "bf16", Some(true))]);
        assert_eq!(flux3_cache_button(&settings, Some(&hit), "sfx", Some(true)), CacheButton::Closed(CacheBlock::AlreadyCached));
        assert_eq!(flux3_cache_button(&settings, Some(&hit), "sound effects", Some(true)), CacheButton::Rename);
    }

    /// The caption names the encoder format; an unknown id is shown verbatim, an empty one
    /// falls back to the backend's label.
    #[test]
    fn the_encoder_caption_names_the_format() {
        let _locale = english();
        assert_eq!(flux3_encoder_caption(&entry("a", "bf16", None)), flux3_encoder_format_name(Flux3EncoderFormat::Bf16));
        assert!(flux3_encoder_caption(&entry("a", "q4_k_m", None)).contains("Q4_K_M"));
        assert_eq!(flux3_encoder_caption(&entry("a", "q2_k", None)), "q2_k");
        let labelled = PromptLibraryEntry { encoder_label: "text_encoder".to_string(), ..PromptLibraryEntry::default() };
        assert_eq!(flux3_encoder_caption(&labelled), "text_encoder");
        assert_eq!(flux3_encoder_caption(&PromptLibraryEntry::default()), "");
    }

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
        assert!(flux3_run_block_reason(&settings, &Flux3ModelReadiness::RootNotAbsolute, Flux3SeedInput::Random).is_some());
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
        assert_eq!(flux3_readiness_line(&Flux3ModelReadiness::Ready, false, Some(summary(false))).tone(), Flux3LineTone::Warn);
        assert_eq!(flux3_readiness_line(&Flux3ModelReadiness::Ready, false, Some(summary(true))).tone(), Flux3LineTone::Ok);
        assert!(!flux3_readiness_line(&Flux3ModelReadiness::Ready, false, None).offers_install());
        assert!(flux3_readiness_line(&Flux3ModelReadiness::NoRoot, false, None).offers_install());
        let relative = flux3_readiness_line(&Flux3ModelReadiness::RootNotAbsolute, false, None);
        assert!(relative.offers_install());
        assert_eq!(relative.tone(), Flux3LineTone::Warn);
        let missing = Flux3ModelReadiness::Missing(Flux3MissingParts(vec![Flux3Part::TextEncoder(Flux3EncoderFormat::Bf16)]));
        assert!(flux3_readiness_line(&missing, true, Some(summary(true))).offers_install());
        assert_eq!(flux3_readiness_line(&Flux3ModelReadiness::Unknown, false, None).tone(), Flux3LineTone::Neutral);
        // An incomplete catalog bundle takes a ready or unanswered line (and offers the block),
        // but a `Missing` verdict keeps naming its parts.
        for readiness in [Flux3ModelReadiness::Ready, Flux3ModelReadiness::Unknown] {
            let line = flux3_readiness_line(&readiness, true, Some(summary(true)));
            assert_eq!(line, Flux3ReadinessLine::BundleMissing);
            assert!(line.offers_install());
            assert_eq!(line.tone(), Flux3LineTone::Warn);
        }
        assert!(matches!(flux3_readiness_line(&missing, true, None), Flux3ReadinessLine::Missing { .. }));
    }

    /// The bundle is the catalog's bundle of the selected DiT + encoder (base, root DiT config
    /// when the DiT reads it, DiT, encoder) in download mode, nothing in manual mode, for every
    /// pair of formats.
    #[test]
    fn the_catalog_bundle_follows_the_selection_and_only_in_download_mode() {
        for &dit in Flux3DitFormat::all() {
            for &encoder in Flux3EncoderFormat::all() {
                let settings = Flux3KleinSettings { format: dit.wire().to_string(), text_encoder_format: encoder.wire().to_string(), ..Flux3KleinSettings::default() };
                let ids: Vec<&str> = flux3_catalog_bundle(&settings).iter().map(|spec| spec.id).collect();
                let mut expected = vec!["flux3_klein_base"];
                if dit == Flux3DitFormat::Bf16 || dit.is_gguf() {
                    expected.push("flux3_klein_dit_config");
                }
                expected.extend([flux3_klein_dit_spec(dit).id, flux3_klein_encoder_spec(encoder).id]);
                assert_eq!(ids, expected, "{dit:?} {encoder:?}");
                // sd.cpp: no DiT config, and the DiT it runs (fp8r / nvfp4 fall back).
                let sdcpp = Flux3KleinSettings { runtime: "sdcpp".to_string(), ..settings.clone() };
                let ids: Vec<&str> = flux3_catalog_bundle(&sdcpp).iter().map(|spec| spec.id).collect();
                let run = flux3_closest_offered(dit, ImageEditRuntime::SdCpp);
                assert_eq!(ids, vec!["flux3_klein_base", flux3_klein_dit_spec(run).id, flux3_klein_encoder_spec(encoder).id], "sd.cpp {dit:?} {encoder:?}");
                let manual = Flux3KleinSettings { source_mode: "manual".to_string(), model_root: "/m".to_string(), ..settings };
                assert!(flux3_catalog_bundle(&manual).is_empty(), "manual mode has no catalog block");
            }
        }
    }

    /// The token row is drawn for the gated BFL repository while a spec from it is not seen
    /// installed: any bundle needs the base (gated), a GGUF DiT also the root DiT config
    /// (gated); the open GGUF repositories never ask.
    #[test]
    fn the_token_row_follows_the_gated_specs_not_installed() {
        let gguf = Flux3KleinSettings { format: "q8_0".to_string(), text_encoder_format: "q4_k_m".to_string(), ..Flux3KleinSettings::default() };
        let bundle = flux3_catalog_bundle(&gguf);
        assert_eq!(catalog_gated_repos_missing(&bundle, |_| false), [FLUX3_REPO]);
        let base_installed = |spec: &ExternalModelSpec| spec.id == "flux3_klein_base";
        assert_eq!(catalog_gated_repos_missing(&bundle, base_installed), [FLUX3_REPO], "the root DiT config is gated");
        let bfl_installed = |spec: &ExternalModelSpec| spec.id == "flux3_klein_base" || spec.id == "flux3_klein_dit_config";
        assert!(catalog_gated_repos_missing(&bundle, bfl_installed).is_empty(), "the GGUF repositories are open");
        let native = flux3_catalog_bundle(&Flux3KleinSettings::default());
        assert_eq!(catalog_gated_repos_missing(&native, base_installed), [FLUX3_REPO], "the fp8r DiT and the bf16 encoder are gated");
        assert!(catalog_gated_repos_missing(&native, |_| true).is_empty());
    }

    /// A format row names the format and its catalog size, and carries ✓ exactly when the
    /// guarded answer says its file is there.
    #[test]
    fn a_format_row_names_the_size_and_the_installed_mark() {
        let _locale = english();
        let status = status_with(&[Flux3DitFormat::Q80], true, true);
        for &format in Flux3DitFormat::all() {
            let row = flux3_dit_format_row(format, Some(&status));
            assert!(row.contains(&format_gib(flux3_klein_dit_spec(format).total_bytes())), "{row}");
            assert_eq!(row.ends_with('✓'), format == Flux3DitFormat::Q80, "{row}");
            assert!(!flux3_dit_format_row(format, None).ends_with('✓'), "no answer claims nothing");
            if let Some(quant) = format.quant_token() {
                assert!(row.contains(quant), "{row}");
            }
        }
        for &format in Flux3EncoderFormat::all() {
            let row = flux3_encoder_format_row(format, Some(&status));
            assert!(row.contains(&format_gib(flux3_klein_encoder_spec(format).total_bytes())), "{row}");
            // `status_with` reports only the aggregate flag, which covers the selection.
            assert!(row.ends_with('✓'), "{row}");
        }
    }

    /// The PyTorch requirement: only PyTorch without Torch on an sd.cpp platform names the way
    /// out; elsewhere the engine says nothing (the host's own refusal applies).
    #[test]
    fn the_torch_gate_table() {
        let _locale = english();
        for runtime in ImageEditRuntime::ALL {
            for torch in [false, true] {
                for platform in [false, true] {
                    let reason = flux3_torch_block_reason(runtime, torch, platform);
                    let expected = runtime == ImageEditRuntime::PyTorch && !torch && platform;
                    assert_eq!(reason.is_some(), expected, "{runtime:?} torch {torch} platform {platform}");
                    if expected {
                        assert_eq!(reason.as_deref(), Some(t!("cleaning.tools.sdcpp.torch_missing_hint")));
                    }
                }
            }
        }
    }

    /// The sd.cpp gate, in order: the runtime's own block, the files (checking or refused), the
    /// prompt, the seed field and the seed range.
    #[test]
    fn the_sdcpp_run_gate_table() {
        let _locale = english();
        let settings = Flux3KleinSettings { runtime: "sdcpp".to_string(), ..Flux3KleinSettings::default() };
        let ready = Flux3SdcppFilesState::Ready;
        assert_eq!(flux3_sdcpp_run_block_reason(&settings, None, &ready, Flux3SeedInput::Random), None);
        assert_eq!(flux3_sdcpp_run_block_reason(&settings, Some("runtime".to_string()), &ready, Flux3SeedInput::Random).as_deref(), Some("runtime"), "the runtime first");
        assert_eq!(flux3_sdcpp_run_block_reason(&settings, None, &Flux3SdcppFilesState::Checking, Flux3SeedInput::Random).as_deref(), Some(t!("cleaning.tools.flux3_klein.sdcpp_files_checking_status")));
        let refused = Flux3SdcppFilesState::Refused(Flux3SdcppFilesError::NoRoot);
        assert_eq!(flux3_sdcpp_run_block_reason(&settings, None, &refused, Flux3SeedInput::Random).as_deref(), Some(t!("cleaning.tools.flux3_klein.model_root_required_error")));
        let blank = Flux3KleinSettings { prompt: " ".to_string(), ..settings.clone() };
        assert!(flux3_sdcpp_run_block_reason(&blank, None, &ready, Flux3SeedInput::Random).is_some());
        assert!(flux3_sdcpp_run_block_reason(&settings, None, &ready, Flux3SeedInput::Invalid).is_some());
        assert_eq!(flux3_sdcpp_run_block_reason(&settings, None, &ready, Flux3SeedInput::Pinned(FLUX3_SDCPP_SEED_MAX)), None, "the largest int64 seed runs");
        assert!(flux3_sdcpp_run_block_reason(&settings, None, &ready, Flux3SeedInput::Pinned(FLUX3_SDCPP_SEED_MAX + 1)).is_some(), "above int64 is refused");
    }

    /// The sd.cpp files line: root problems keep their lines, an incomplete bundle offers the
    /// download, refused files name the reason and offer «Установить».
    #[test]
    fn the_sdcpp_files_line_follows_the_state() {
        let _locale = english();
        assert_eq!(flux3_sdcpp_readiness_line(&Flux3SdcppFilesState::Checking, false), Flux3ReadinessLine::SdcppFilesChecking);
        assert_eq!(flux3_sdcpp_readiness_line(&Flux3SdcppFilesState::Ready, false).tone(), Flux3LineTone::Ok);
        assert_eq!(flux3_sdcpp_readiness_line(&Flux3SdcppFilesState::Ready, true), Flux3ReadinessLine::BundleMissing);
        let root = Flux3SdcppFilesState::Refused(Flux3SdcppFilesError::RootNotAbsolute);
        assert_eq!(flux3_sdcpp_readiness_line(&root, true), Flux3ReadinessLine::RootNotAbsolute);
        let missing = Flux3SdcppFilesState::Refused(Flux3SdcppFilesError::Files(ms_sdcpp::ModelFilesError::Missing { role: ms_sdcpp::ComponentRole::Vae, path: PathBuf::from("/m/ae.safetensors") }));
        let line = flux3_sdcpp_readiness_line(&missing, false);
        assert!(matches!(line, Flux3ReadinessLine::SdcppFilesRefused(_)));
        assert!(line.offers_install() && line.tone() == Flux3LineTone::Warn);
        assert_eq!(flux3_sdcpp_readiness_line(&missing, true), Flux3ReadinessLine::BundleMissing);
    }

    /// The fallback line appears only on sd.cpp with a stored format it cannot run.
    #[test]
    fn the_format_fallback_line_names_both_formats() {
        let _locale = english();
        assert!(flux3_sdcpp_format_fallback_line(&Flux3KleinSettings::default()).is_none(), "PyTorch runs fp8r");
        let sdcpp = Flux3KleinSettings { runtime: "sdcpp".to_string(), ..Flux3KleinSettings::default() };
        let line = flux3_sdcpp_format_fallback_line(&sdcpp).expect("fp8r falls back");
        assert!(line.contains(&flux3_dit_format_name(Flux3DitFormat::Fp8r)) && line.contains("Q8_0"), "{line}");
        let offered = Flux3KleinSettings { format: "q4_k_m".to_string(), ..sdcpp };
        assert!(flux3_sdcpp_format_fallback_line(&offered).is_none());
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
