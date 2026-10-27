/*
File: cleaning/tools/ai_editor/engines/flux2_klein/decisions.rs

Purpose:
The PURE decisions the FLUX.2 klein panel renders. Every function here answers one
question about the state the panel was handed — which single line to show, whether a run
may start, whether a region is legal — and none of them touches a `Ui`, a channel or the
disk. They live apart from the drawing precisely so the answers can be asserted without a
frame, which is what the tests at the end of this file do.

Main responsibilities:
- name the encoder a cache entry was made by (`flux2_encoder_caption`), the caption the
  shared prompt-cache line and picker show (the line itself is `../shared/prompt_cache.rs`);
- decide whether the `guidance_scale` control is live at all (`flux2_guidance_supported`);
- decide whether the «Внимание только к маске» run option is live
  (`flux2_text_attention_block_reason`);
- decide what the always-visible readiness line reports (`flux2_readiness_line`), with the
  memory forecast folded in as `Flux2MemorySummary` and a catalog bundle known to be
  incomplete as its own line;
- name the catalog package the current selection needs (`flux2_catalog_bundle`: both
  variants, download mode only); the model-agnostic questions about it (gated repositories
  still missing, bundle state, running job, engine-picker block) are `../shared/catalog_download.rs`;
- caption each weight-format row with its size and installed mark, and say what it costs
  (`flux2_weight_format_label`, `flux2_weight_format_hint`);
- name the first reason a run cannot start (`flux2_run_block_reason`);
- validate a region SIZE against the model's hard limits (`region_block_reason`), the
  check the run gate and the worker share so the two cannot disagree.

Key structures:
- `Flux2ReadinessLine`, `Flux2LineTone`, `Flux2MemorySummary`

Key functions:
- `flux2_encoder_caption()`, `flux2_guidance_supported()`,
  `flux2_readiness_line()`
- `flux2_text_attention_block_reason()`
- `flux2_catalog_bundle()`
- `flux2_weight_format_label()`, `flux2_weight_format_hint()`, `flux2_encoder_format_token()`
- `flux2_run_block_reason()`, `region_block_reason()`

Notes:
Three-state on purpose throughout: an `Option<bool>` of `None` means "no answer yet" and
must never be read as `false`, because the difference between "not installed" and "not
known" is exactly what these lines exist to keep apart. The limits themselves
(`FLUX2_MIN_SELECTION_PX` and friends) and the tone colours stay in the module root.
*/

use super::*;

/// The localized name of the encoder that made a library entry: its source («стандартный» /
/// «расцензуренный») and precision (the format's technical token — `bf16` / `f16` / a GGUF
/// quant — or fp8). An entry of unknown source (a manual-mode encoder, a migration that could
/// not prove one) is named by the backend's `encoder_label`; an unknown format id is shown
/// verbatim. Empty only when the entry says nothing at all.
#[must_use]
pub(super) fn flux2_encoder_caption(entry: &PromptLibraryEntry) -> String {
    let source = Flux2EncoderSource::from_wire(&entry.encoder_source);
    let source_caption = source.map(|source| match source {
        Flux2EncoderSource::Stock => t!("cleaning.tools.flux2_klein.encoder_source_stock").to_string(),
        Flux2EncoderSource::Uncensored => t!("cleaning.tools.flux2_klein.encoder_source_uncensored").to_string(),
    });
    let format = entry.encoder_format.as_str();
    let precision = if entry.encoder_fp8 {
        Some(t!("cleaning.tools.flux2_klein.encoder_precision_fp8").to_string())
    } else if let Some(known) = Flux2WeightFormat::from_wire(format) {
        // The unquantized token depends on the source (`bf16` stock, `f16` uncensored); an
        // unknown source reads as the stock spelling.
        Some(flux2_encoder_format_token(source.unwrap_or_default(), known).to_string())
    } else if format == ms_config::flux2_weight_format::FLUX2_PROMPT_PRECISION_GGUF {
        Some(t!("cleaning.tools.flux2_klein.encoder_precision_gguf").to_string())
    } else {
        (!format.is_empty()).then(|| format.to_string())
    };
    let name = source_caption.or_else(|| (!entry.encoder_label.is_empty()).then(|| entry.encoder_label.clone()));
    match (name, precision) {
        (Some(name), Some(precision)) => tf!("cleaning.tools.flux2_klein.encoder_caption", source = name, format = precision),
        (Some(name), None) => name,
        (None, Some(precision)) => precision,
        (None, None) => String::new(),
    }
}

/// Whether the `guidance_scale` control may be touched, from the last `.status` answer.
///
/// `false` ONLY when the backend positively answered `guidance_supported: false`, i.e. the
/// loaded checkpoint declares itself distilled and diffusers switches classifier-free
/// guidance off regardless of the value. Everything else — no answer yet, a backend that
/// predates the field, a field of the wrong type — is `true`, which is what the control did
/// before the field existed. Reading silence as "unsupported" would take a working
/// parameter away from a user whose backend is simply older, so the asymmetry is the whole
/// point of this function and is why it is not spelled inline at the call site.
#[must_use]
pub(super) fn flux2_guidance_supported(status: Option<&Flux2Status>) -> bool {
    status.and_then(|status| status.guidance_supported) != Some(false)
}

/// Why the «Внимание только к маске» checkbox is closed right now, localized; `None` when
/// the user may toggle it.
///
/// It is closed exactly while no mask pixel is painted: the run is then a whole-region
/// edit, there is no mask to confine the prompt to, and the flag would not travel
/// ([`Flux2KleinSettings::to_params`]). It is NOT closed during a run — like every other
/// parameter it is read when the next run starts — and closing it never rewrites the
/// user's stored choice.
#[must_use]
pub(super) fn flux2_text_attention_block_reason(mask_painted: bool) -> Option<&'static str> {
    (!mask_painted).then(|| t!("cleaning.tools.flux2_klein.text_attention_in_mask_disabled_hint"))
}

/// The tone a one-line verdict is drawn in.
///
/// The tones render in the studio's shared status colours (`ms_theme::status::SUCCESS` /
/// `WARNING`, or plain small text); naming the tone rather than the colour keeps the decision
/// testable without a live `Ui`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2LineTone {
    Ok,
    Warn,
    Neutral,
}

/// The memory forecast as the readiness line carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Flux2MemorySummary {
    /// The one-line forecast of the whole run (text encoder included), already formatted by
    /// `large_model_resources::forecast_line`.
    pub(super) line: String,
    /// The backend's own verdict. `false` turns the readiness line amber, because the
    /// spelled-out «не помещается» warning lives inside «Память и скорость» and that
    /// section may well be folded away.
    pub(super) fits: bool,
}

/// What the always-visible readiness line reports.
///
/// A decision separate from the drawing, for the reason every three-state answer in this
/// file is: the difference between "not installed" and "no answer yet" is exactly what the
/// line must never blur, and it has to be assertable without a live `Ui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux2ReadinessLine {
    /// Every component a run needs is there. `memory` is the forecast when one has landed.
    Ready { memory: Option<Flux2MemorySummary> },
    /// Components are missing; the string is the localized list, and «Установить» is
    /// offered beside the line because the fix is one click away.
    Missing { components: String },
    /// `.status` names nothing missing (or has not answered), but the catalog package the
    /// selected weight formats need is not fully downloaded (download mode): the
    /// download block in «Установка модели» is the fix, so «Установить» is offered too.
    BundleMissing,
    /// No `.status` answer yet. Neutral and buttonless: nothing is wrong, nothing is known.
    Unknown,
}

impl Flux2ReadinessLine {
    /// The colour the line is drawn in.
    pub(super) fn tone(&self) -> Flux2LineTone {
        match self {
            // A forecast that does not fit is the one way a READY model still needs
            // attention, and the tone is the only room this one line has to say so.
            Self::Ready { memory } => match memory {
                Some(summary) if !summary.fits => Flux2LineTone::Warn,
                Some(_) | None => Flux2LineTone::Ok,
            },
            Self::Missing { .. } | Self::BundleMissing => Flux2LineTone::Warn,
            Self::Unknown => Flux2LineTone::Neutral,
        }
    }

    /// Whether «Установить» is offered beside the line.
    pub(super) fn offers_install(&self) -> bool {
        matches!(self, Self::Missing { .. } | Self::BundleMissing)
    }

    /// The localized text of the line.
    pub(super) fn text(&self) -> String {
        match self {
            Self::Ready { memory: Some(summary) } => tf!(
                "cleaning.tools.flux2_klein.model_ready_memory_status",
                memory = summary.line
            ),
            Self::Ready { memory: None } => {
                t!("cleaning.tools.flux2_klein.model_ready_status").to_string()
            }
            Self::Missing { components } => tf!(
                "cleaning.tools.flux2_klein.model_missing_status",
                components = components
            ),
            Self::BundleMissing => t!("cleaning.tools.flux2_klein.bundle_missing_status").to_string(),
            Self::Unknown => {
                t!("cleaning.tools.flux2_klein.model_readiness_unknown_status").to_string()
            }
        }
    }
}

/// Turns the model verdict into the line the panel shows.
///
/// `memory` is the forecast summary and is meaningful only for [`Flux2ModelReadiness::Ready`];
/// it is IGNORED for the other two, because figures about a run that cannot start compete
/// with the sentence that says why.
///
/// `bundle_missing` is whether the catalog package the selected weight formats need is known
/// to be not fully downloaded ([`catalog_bundle_missing`]; always `false` outside
/// download mode). It turns a `Ready` or `Unknown` verdict into
/// [`Flux2ReadinessLine::BundleMissing`]; a `Missing` verdict keeps its own line, because it
/// names the components. It never gates a run: that stays the `.status` readiness.
#[must_use]
pub(super) fn flux2_readiness_line(
    readiness: Flux2ModelReadiness,
    bundle_missing: bool,
    memory: Option<Flux2MemorySummary>,
) -> Flux2ReadinessLine {
    match readiness {
        Flux2ModelReadiness::Missing(missing) => Flux2ReadinessLine::Missing {
            components: missing.names(),
        },
        Flux2ModelReadiness::Ready | Flux2ModelReadiness::Unknown if bundle_missing => Flux2ReadinessLine::BundleMissing,
        Flux2ModelReadiness::Ready => Flux2ReadinessLine::Ready { memory },
        Flux2ModelReadiness::Unknown => Flux2ReadinessLine::Unknown,
    }
}

/// The catalog package (`flux2_klein_package_specs`) the CURRENT selection needs: the base
/// files, then the transformer spec (the adopt-only legacy one when the probe found that bf16
/// layout), then the text-encoder spec of the selected source and formats.
///
/// Empty unless the weight formats are in effect
/// ([`Flux2KleinSettings::weight_formats_in_effect`]: download mode, either variant), so
/// manual mode never draws or probes a catalog block. Pure: no disk access.
#[must_use]
pub(super) fn flux2_catalog_bundle(settings: &Flux2KleinSettings) -> Vec<&'static ExternalModelSpec> {
    if !settings.weight_formats_in_effect() {
        return Vec::new();
    }
    flux2_klein_package_specs(
        settings.variant(),
        settings.weight_format(Flux2WeightAxis::Transformer),
        settings.installed_formats.transformer_layout,
        settings.encoder_source(),
        settings.weight_format(Flux2WeightAxis::TextEncoder),
    )
}

/// The row (and closed-combo) caption of one weight format in «Веса трансформера» /
/// «Веса текстового энкодера»: the format with the download size of its catalog spec, and a
/// trailing ✓ when the last installed-formats probe found it installed. The unquantized row
/// names the real dtype of its files (`bf16`, or `f16` for the uncensored encoder:
/// `Flux2EncoderSource::unquantized_token`).
///
/// The quant and dtype tokens are technical identifiers and stay literal
/// (`dev-docs/i18n_exclusions.md`); so is the ✓ glyph, as in the FLUX Fill quant combo. A
/// format without a catalog spec shows no size (never reached for an offered format: the
/// catalog's tests pin one per offered format).
#[must_use]
pub(super) fn flux2_weight_format_label(
    variant: Flux2Variant,
    axis: Flux2WeightAxis,
    source: Flux2EncoderSource,
    format: Flux2WeightFormat,
    installed: &Flux2InstalledFormats,
) -> String {
    let size = flux2_klein_spec(variant, axis, source, format).map(|spec| format_gib(spec.total_bytes())).unwrap_or_default();
    let label = match format.quant_token() {
        None => {
            let dtype = match axis {
                Flux2WeightAxis::Transformer => Flux2WeightFormat::Bf16.wire(),
                Flux2WeightAxis::TextEncoder => source.unquantized_token(),
            };
            tf!("cleaning.tools.flux2_klein.format_unquantized_label", dtype = dtype, size = size)
        }
        Some(quant) => tf!("cleaning.tools.flux2_klein.format_gguf_label", quant = quant, size = size),
    };
    if installed.contains(axis, source, format) {
        format!("{label} ✓")
    } else {
        label
    }
}

/// The technical name of an encoder `format` of `source`: its GGUF quant (`Q5_K_M`, …) or the
/// real dtype of its unquantized files (`bf16` / `f16`). A literal token, never localized
/// (`dev-docs/i18n_exclusions.md`); it names both formats in the line a source switch leaves.
#[must_use]
pub(super) fn flux2_encoder_format_token(source: Flux2EncoderSource, format: Flux2WeightFormat) -> &'static str {
    format.quant_token().unwrap_or_else(|| source.unquantized_token())
}

/// The hover of one weight-format row: what choosing it costs on THIS axis — a GGUF
/// transformer makes every step slower, a GGUF encoder makes encoding slower and may shift
/// quality at the smallest quants (and excludes fp8, D8).
#[must_use]
pub(super) fn flux2_weight_format_hint(axis: Flux2WeightAxis, format: Flux2WeightFormat) -> &'static str {
    match (format.is_gguf(), axis) {
        (false, Flux2WeightAxis::Transformer | Flux2WeightAxis::TextEncoder) => t!("cleaning.tools.flux2_klein.format_bf16_hint"),
        (true, Flux2WeightAxis::Transformer) => t!("cleaning.tools.flux2_klein.format_gguf_transformer_hint"),
        (true, Flux2WeightAxis::TextEncoder) => t!("cleaning.tools.flux2_klein.format_gguf_encoder_hint"),
    }
}

/// The first reason a run cannot start that this ENGINE can see, or `None` when it can.
///
/// It covers the model paths and the prompt, and nothing else. The two checks that used to
/// live here belong to the host now and must not be duplicated: the region size is
/// validated by the frame against [`Flux2KleinEngine::constraints`] (and again on the worker
/// by [`region_block_reason`], which is what protects the wire), and "is anything painted?"
/// is [`Flux2KleinEngine::allows_empty_mask`], which the frame's own process button honours.
///
/// `prompt_cached` is the three-state `.status` answer for the prompt in the field. **A
/// cached prompt waives the text encoder**: the denoise and the VAE decode never look at
/// it, so a `.msprompt` carried to a machine that never downloaded the 16 GB Qwen3 is
/// enough to run, and blocking there would hide a run that works. The waiver needs
/// `Some(true)` and nothing weaker: `None` means the answer is outstanding — or that the
/// backend does not report the field at all, and such a backend cannot generate without an
/// encoder either, so an enabled button would only offer a run that always fails.
/// The transformer, the VAE, the tokenizer and the scheduler are never waived; the backend
/// makes the final decision either way (`_first_unavailable_reason`) and this gate exists
/// to explain it before the click, not to duplicate it.
///
/// `status` is the cached `.status` catalog — guarded by
/// [`Flux2KleinEngine::status_for_current_paths`], so it is never an answer about paths
/// that have since changed — and it is what makes the refusal LOCAL and
/// actionable: with the paths filled in, the backend used to be the first thing to notice
/// that the files behind them do not exist, and answered with its own untranslated
/// `Путь transformer_path не найден`. In DOWNLOAD mode that was the normal state of a
/// fresh installation, because the derived paths are never empty. The verdict comes from
/// [`flux2_model_readiness`], and only `Missing` blocks: `Unknown` means no answer has
/// landed yet and must leave the button alive.
///
/// A free function rather than a method for the same reason [`region_block_reason`] is
/// one — the gate is the contract worth testing, and it must be testable without an engine
/// instance, which owns channels and worker state.
pub(super) fn flux2_run_block_reason(
    settings: &Flux2KleinSettings,
    status: Option<&Flux2Status>,
    prompt_cached: Option<bool>,
) -> Option<String> {
    // The EFFECTIVE paths, so the gate answers about the mode the user is actually in: in
    // download mode the manual fields may be empty while the derived paths are not, and
    // reading the fields would refuse a run the backend would have accepted. Built rather
    // than taken from `normalized()`, which would rebuild the whole struct on every frame
    // of an open panel.
    let paths = settings.effective_paths();
    let encoder_waived = prompt_cached == Some(true);
    if paths.transformer.is_empty() || paths.vae.is_empty() {
        // Two messages, because naming a path the run does not need would send the user
        // looking for a 16 GB download they have already worked around.
        return Some(if encoder_waived {
            t!("cleaning.tools.flux2_klein.model_paths_required_error").to_string()
        } else {
            t!("cleaning.tools.flux2_klein.paths_required_error").to_string()
        });
    }
    if !encoder_waived && paths.text_encoder.is_empty() {
        return Some(t!("cleaning.tools.flux2_klein.paths_required_error").to_string());
    }
    // The paths are filled in; whether anything is BEHIND them is the next question, and
    // answering it here is what replaces the backend's raw «Путь ... не найден». The
    // message differs by mode because the fix does: a download is a button, a wrong manual
    // path is a field. `Unknown` and `Ready` both fall through — see `flux2_model_readiness`.
    if let Flux2ModelReadiness::Missing(missing) = flux2_model_readiness(settings, status, prompt_cached) {
        let components = missing.names();
        return Some(match settings.source_mode() {
            Flux2SourceMode::Download => tf!("cleaning.tools.flux2_klein.model_not_downloaded_error", components = components).to_string(),
            Flux2SourceMode::Manual => tf!("cleaning.tools.flux2_klein.model_components_missing_error", components = components).to_string(),
        });
    }
    if settings.prompt.trim().is_empty() {
        return Some(t!("cleaning.tools.flux2_klein.prompt_required_error").to_string());
    }
    None
}

/// Validates a REGION SIZE against the model's hard constraints, naming the first one
/// it breaks. Shared by the run gate and the worker, so the two cannot disagree.
pub(super) fn region_block_reason(region: [usize; 2]) -> Option<String> {
    let [w, h] = region;
    if w == 0 || h == 0 {
        return Some(t!("cleaning.region.invalid_selection_size_error").to_string());
    }
    if w % FLUX2_SELECTION_MULTIPLE != 0 || h % FLUX2_SELECTION_MULTIPLE != 0 {
        return Some(tf!(
            "cleaning.tools.flux2_klein.region_multiple_error",
            mult = FLUX2_SELECTION_MULTIPLE,
            w = w,
            h = h
        ));
    }
    if w.min(h) < FLUX2_MIN_SELECTION_PX {
        return Some(tf!(
            "cleaning.region.min_selection_error",
            min = FLUX2_MIN_SELECTION_PX,
            w = w,
            h = h
        ));
    }
    if w.saturating_mul(h) > FLUX2_MAX_SELECTION_AREA_PX2 {
        return Some(tf!(
            "cleaning.region.max_selection_area_error",
            max = FLUX2_MAX_SELECTION_AREA_PX2,
            w = w,
            h = h
        ));
    }
    let long = w.max(h);
    let short = w.min(h).max(1);
    // Same rearrangement as `RegionEditToolBase::check_selection_limits`, so the two
    // gates accept exactly the same set of regions.
    if (long as f32) > (FLUX2_MAX_SELECTION_ASPECT * short as f32).floor() {
        return Some(tf!(
            "cleaning.region.max_aspect_error",
            ratio = format!("{FLUX2_MAX_SELECTION_ASPECT:.0}"),
            w = w,
            h = h
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::region_edit_v2::geometry::check_size;

    /// The run option is open exactly while a mask is painted, and a closed one always
    /// says why.
    #[test]
    fn text_attention_is_closed_only_without_a_painted_mask() {
        assert_eq!(flux2_text_attention_block_reason(true), None);
        assert!(flux2_text_attention_block_reason(false).is_some_and(|reason| !reason.is_empty()));
    }

    #[test]
    fn region_block_reason_enforces_every_limit() {
        assert!(region_block_reason([512, 512]).is_none());
        assert!(region_block_reason([0, 512]).is_some(), "empty region");
        assert!(region_block_reason([510, 512]).is_some(), "not a multiple of 16");
        assert!(region_block_reason([112, 512]).is_some(), "below the min side");
        assert!(region_block_reason([1024, 1040]).is_some(), "over 1 MP");
        assert!(region_block_reason([128, 1040]).is_some(), "steeper than 8:1");
        assert!(region_block_reason([128, 1024]).is_none(), "exactly 8:1 is allowed");
        // The frame validates a rectangle against `constraints()`, the worker against
        // `region_block_reason`. The two must accept exactly the same regions, or the
        // frame would offer a size the run then refuses.
        let constraints = Flux2KleinEngine::frame_constraints();
        for size in [
            [512usize, 512],
            [510, 512],
            [112, 512],
            [1024, 1040],
            [128, 1040],
            [128, 1024],
        ] {
            assert_eq!(
                check_size(size[0], size[1], &constraints).is_some(),
                region_block_reason(size).is_some(),
                "{size:?}"
            );
        }
    }

    #[test]
    fn guidance_stays_live_until_the_backend_says_otherwise() {
        // No `.status` answer at all — the first frames of every session. The panel is not
        // allowed to close a control on a question nobody has answered yet.
        assert!(flux2_guidance_supported(None));
        // An answer that predates the field: the same "not known", and it must read the
        // same way. This is the case a future edit is most likely to invert.
        assert!(flux2_guidance_supported(Some(&status_with_present(
            &FLUX2_ALL_COMPONENTS
        ))));
        let mut distilled = status_with_present(&FLUX2_ALL_COMPONENTS);
        distilled.guidance_supported = Some(false);
        assert!(!flux2_guidance_supported(Some(&distilled)));
        distilled.guidance_supported = Some(true);
        assert!(flux2_guidance_supported(Some(&distilled)));
    }

    #[test]
    fn a_cached_prompt_lets_a_run_start_without_a_text_encoder() {
        let no_encoder = Flux2KleinSettings {
            text_encoder_path: String::new(),
            ..runnable_settings()
        };
        // The whole point of the prompt-cache library: the denoise and the VAE decode
        // never look at the encoder, so a ready embedding is enough to run.
        assert!(
            flux2_run_block_reason(&no_encoder, None, Some(true)).is_none(),
            "a cached prompt must waive the encoder path"
        );
        // Without a cache the encoder is required again, and the message names all three
        // paths because all three are genuinely needed then.
        for state in [None, Some(false)] {
            let reason = flux2_run_block_reason(&no_encoder, None, state)
                .expect("no encoder and no cache must be refused");
            assert_eq!(
                reason,
                t!("cleaning.tools.flux2_klein.paths_required_error"),
                "{state:?} must be refused with the three-path message"
            );
        }
        // `Some(true)` and nothing weaker: a backend that never reports `prompt_cached`
        // cannot generate without an encoder either, so an enabled button there would only
        // offer a run that always fails.

        // The waiver covers the encoder ALONE. The transformer and the VAE are still
        // required, and their message must not send the user after the 16 GB encoder they
        // have just worked around.
        for broken in [
            Flux2KleinSettings {
                transformer_path: "  ".to_string(),
                ..no_encoder.clone()
            },
            Flux2KleinSettings {
                vae_path: String::new(),
                ..no_encoder.clone()
            },
        ] {
            let reason = flux2_run_block_reason(&broken, None, Some(true))
                .expect("a missing transformer or VAE still blocks the run");
            assert_eq!(
                reason,
                t!("cleaning.tools.flux2_klein.model_paths_required_error")
            );
        }
        // And every other gate keeps working with the encoder waived.
        let no_prompt = Flux2KleinSettings {
            prompt: "   ".to_string(),
            ..no_encoder.clone()
        };
        assert!(flux2_run_block_reason(&no_prompt, None, Some(true)).is_some());
    }

    /// A catalog fetched for OTHER paths says nothing about the ones in the settings now.
    ///
    /// The regression this pins: the user pastes a corrected model path, the cached answer
    /// still reports the old file as absent, and «Обработать» keeps refusing with a list of
    /// components that are in fact there. The guard is the same one the prompt half of the
    /// answer already had, and it degrades to `Unknown` — which never blocks — rather than
    /// to a verdict of its own.
    #[test]
    fn a_catalog_about_other_paths_is_not_a_verdict_about_these() {
        let wrong = Flux2KleinSettings {
            transformer_path: "/models/typo.safetensors".to_string(),
            ..runnable_settings()
        };
        let present: Vec<&str> = FLUX2_ALL_COMPONENTS
            .into_iter()
            .filter(|name| *name != "transformer")
            .collect();
        let answer = status_with_present(&present);
        let asked_about = wrong.effective_paths();

        // While the answer still describes the paths in the fields it is a real verdict.
        assert_eq!(
            flux2_model_readiness(
                &wrong,
                flux2_status_for_paths(Some(&answer), Some(&asked_about), &wrong.effective_paths()),
                None
            ),
            Flux2ModelReadiness::Missing(Flux2MissingComponents {
                transformer: true,
                ..Flux2MissingComponents::default()
            })
        );

        // The path is corrected by TYPING — no picker, no «Обновить». The catalog in hand
        // is now about a file nobody asked about, and must stop being evidence.
        let fixed = runnable_settings();
        assert_ne!(asked_about, fixed.effective_paths());
        let guarded =
            flux2_status_for_paths(Some(&answer), Some(&asked_about), &fixed.effective_paths());
        assert!(guarded.is_none(), "a stale catalog must not survive the guard");
        assert_eq!(
            flux2_model_readiness(&fixed, guarded, None),
            Flux2ModelReadiness::Unknown,
            "a corrected path may not keep reading as a missing component"
        );
        assert!(
            flux2_run_block_reason(&fixed, guarded, None).is_none(),
            "and the run gate must let the corrected configuration through"
        );

        // No answer at all behaves as before; the guard adds nothing there.
        assert!(flux2_status_for_paths(None, Some(&asked_about), &fixed.effective_paths()).is_none());
        // An answer whose paths were never recorded (an older session state) is treated the
        // same way: unattributed evidence is no evidence.
        assert!(flux2_status_for_paths(Some(&answer), None, &fixed.effective_paths()).is_none());
    }

    /// The refusal must be LOCAL and actionable: it names the missing parts and the fix,
    /// and the fix differs by mode — a download is a button, a wrong manual path is a field.
    #[test]
    fn a_missing_model_is_refused_locally_and_says_what_to_do() {
        let nothing = status_with_present(&[]);

        let download = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            ..runnable_settings()
        };
        let reason = flux2_run_block_reason(&download, Some(&nothing), None)
            .expect("an empty models directory must be refused before the request goes out");
        assert_eq!(
            reason,
            tf!(
                "cleaning.tools.flux2_klein.model_not_downloaded_error",
                components = Flux2MissingComponents {
                    text_encoder: true,
                    transformer: true,
                    vae: true,
                    tokenizer: true,
                    scheduler: true,
                }
                .names()
            )
        );
        // That the message really NAMES the components is a property of the templates and
        // is pinned per locale by `every_catalog_carries_the_new_panel_keys` — a unit test
        // runs with no active catalog, where `tf!` answers the key itself.

        // Manual mode: same verdict, a different instruction.
        let manual = runnable_settings();
        let manual_reason = flux2_run_block_reason(&manual, Some(&nothing), None)
            .expect("paths that point at nothing must be refused here, not by the backend");
        assert_ne!(
            manual_reason, reason,
            "the two modes are fixed in different places and must not share one message"
        );

        // Nothing outstanding may block: an unanswered `.status` leaves the run alive.
        assert!(
            flux2_run_block_reason(&manual, None, None).is_none(),
            "`Unknown` must never block — a slow backend would kill the button"
        );
        // And a complete installation is not blocked either.
        assert!(
            flux2_run_block_reason(
                &manual,
                Some(&status_with_present(&FLUX2_ALL_COMPONENTS)),
                None
            )
            .is_none()
        );
    }

    /// The line under the prompt field has room for exactly ONE fact and three compete for
    /// it, so the priority is a rule and not a formatting accident. A served prompt wins
    /// outright — the run works with no encoder at all, which is what the library is for —
    /// and only after that does a missing encoder outrank «не кэширован», because it says
    /// the run cannot start rather than that it will be slow.
    #[test]
    fn the_prompt_line_reports_the_most_consequential_state_and_stays_silent_when_unsure() {
        let line = |prompt_cached, encoder| prompt_cache_line(None, None, prompt_cached, encoder, &flux2_encoder_caption);
        let plain_cached = PromptCacheLine::Cached { encoder: None, memory: false };
        assert_eq!(line(None, None), PromptCacheLine::Silent);
        assert_eq!(line(Some(true), None), plain_cached);
        assert_eq!(line(Some(false), None), PromptCacheLine::NotCached);
        // A served prompt survives a machine with no encoder — the run gate waives the encoder
        // on exactly this.
        assert_eq!(line(Some(true), Some(false)), plain_cached);
        // Without the embeddings AND without an encoder nothing can be produced here.
        assert_eq!(line(Some(false), Some(false)), PromptCacheLine::NoEncoder);
        assert_eq!(line(None, Some(false)), PromptCacheLine::NoEncoder);
    }

    /// The line names the encoder of the entry in use: the fresh `.status` entry first, the
    /// listing's selection while `.status` has not answered about this text, and a memory-only
    /// encode says it is not saved. A `.status` that says NOT served outranks the listing.
    #[test]
    fn the_prompt_line_names_the_encoder_of_the_entry_in_use() {
        let entry = |source: &str, format: &str, origin: &str| PromptLibraryEntry {
            file: "a.msprompt".to_string(),
            encoder_source: source.to_string(),
            encoder_format: format.to_string(),
            origin: origin.to_string(),
            ..PromptLibraryEntry::default()
        };
        let listed = entry("stock", "bf16", "built");
        let fresh = entry("uncensored", "q8_0", "memory");
        let caption = |entry: &PromptLibraryEntry| Some(flux2_encoder_caption(entry));
        assert_eq!(
            prompt_cache_line(Some(&listed), Some(&fresh), Some(true), None, &flux2_encoder_caption),
            PromptCacheLine::Cached { encoder: caption(&fresh), memory: true }
        );
        assert_eq!(
            prompt_cache_line(Some(&listed), None, Some(true), None, &flux2_encoder_caption),
            PromptCacheLine::Cached { encoder: caption(&listed), memory: false }
        );
        assert_eq!(
            prompt_cache_line(Some(&listed), None, None, Some(false), &flux2_encoder_caption),
            PromptCacheLine::Cached { encoder: caption(&listed), memory: false },
            "a preset of the typed text is a fact before `.status` answers"
        );
        assert_eq!(prompt_cache_line(Some(&listed), None, Some(false), None, &flux2_encoder_caption), PromptCacheLine::NotCached);
    }

    /// Every caption names the source and the precision it can; the unquantized token follows
    /// the source, fp8 and the unnamed GGUF have their own, and an unknown source falls back to
    /// the backend's label. The rendered text depends on the active catalog, so the BRANCHES
    /// are asserted through the same macros.
    #[test]
    fn an_encoder_caption_names_the_source_and_the_precision() {
        let entry = |source: &str, format: &str, fp8: bool, label: &str| PromptLibraryEntry {
            encoder_source: source.to_string(),
            encoder_format: format.to_string(),
            encoder_fp8: fp8,
            encoder_label: label.to_string(),
            ..PromptLibraryEntry::default()
        };
        let stock = t!("cleaning.tools.flux2_klein.encoder_source_stock");
        let uncensored = t!("cleaning.tools.flux2_klein.encoder_source_uncensored");
        let caption = |source: &str, format: &str| tf!("cleaning.tools.flux2_klein.encoder_caption", source = source, format = format);
        assert_eq!(flux2_encoder_caption(&entry("stock", "bf16", false, "")), caption(stock, "bf16"));
        assert_eq!(flux2_encoder_caption(&entry("uncensored", "bf16", false, "")), caption(uncensored, "f16"));
        assert_eq!(flux2_encoder_caption(&entry("uncensored", "q8_0", false, "")), caption(uncensored, "Q8_0"));
        assert_eq!(flux2_encoder_caption(&entry("stock", "bf16", true, "")), caption(stock, t!("cleaning.tools.flux2_klein.encoder_precision_fp8")));
        assert_eq!(flux2_encoder_caption(&entry("", "gguf", false, "my-encoder.gguf")), caption("my-encoder.gguf", t!("cleaning.tools.flux2_klein.encoder_precision_gguf")));
        assert_eq!(flux2_encoder_caption(&entry("", "", false, "qwen3")), "qwen3");
        assert_eq!(flux2_encoder_caption(&entry("", "f32", false, "")), "f32");
        assert_eq!(flux2_encoder_caption(&entry("", "", false, "")), "");
    }

    /// The always-visible readiness line is the panel's ONE report of the model verdict.
    /// Each of the three states has to read differently — and `Unknown` must never borrow
    /// the wording or the tone of `Missing`, which is the confusion this whole three-state
    /// answer exists to prevent.
    ///
    /// Every expected text is built from the SAME macro the code uses, because whether a
    /// catalog is active depends on which other test installed one first; that the
    /// templates really carry their placeholders is pinned per locale by
    /// `every_catalog_carries_the_new_panel_keys`.
    #[test]
    fn the_readiness_line_says_one_thing_per_verdict() {
        let forecast = Flux2MemorySummary {
            line: "VRAM 9.0".to_string(),
            fits: true,
        };

        let unknown = flux2_readiness_line(Flux2ModelReadiness::Unknown, false, Some(forecast.clone()));
        assert_eq!(unknown, Flux2ReadinessLine::Unknown);
        assert_eq!(unknown.tone(), Flux2LineTone::Neutral);
        assert!(
            !unknown.offers_install(),
            "nothing is known to be missing, so there is nothing to install"
        );
        assert_eq!(
            unknown.text(),
            t!("cleaning.tools.flux2_klein.model_readiness_unknown_status")
        );

        let missing = flux2_readiness_line(
            Flux2ModelReadiness::Missing(Flux2MissingComponents {
                transformer: true,
                vae: true,
                ..Flux2MissingComponents::default()
            }),
            false,
            Some(forecast.clone()),
        );
        assert_eq!(missing.tone(), Flux2LineTone::Warn);
        assert!(missing.offers_install());
        let Flux2ReadinessLine::Missing { components } = &missing else {
            panic!("a missing model must keep the list of what is missing");
        };
        assert_eq!(
            components,
            &format!(
                "{}, {}",
                t!("cleaning.tools.flux2_klein.component_transformer"),
                t!("cleaning.tools.flux2_klein.component_vae")
            ),
            "one refusal names every missing part, in catalog order"
        );
        assert_eq!(
            missing.text(),
            tf!(
                "cleaning.tools.flux2_klein.model_missing_status",
                components = components
            )
        );

        // Ready with no answered forecast yet: the short line, and never a template with an
        // empty `{memory}` in it.
        let bare = flux2_readiness_line(Flux2ModelReadiness::Ready, false, None);
        assert_eq!(bare.tone(), Flux2LineTone::Ok);
        assert!(!bare.offers_install());
        assert_eq!(
            bare.text(),
            t!("cleaning.tools.flux2_klein.model_ready_status")
        );

        let ready = flux2_readiness_line(Flux2ModelReadiness::Ready, false, Some(forecast.clone()));
        assert_eq!(ready.tone(), Flux2LineTone::Ok);
        assert_eq!(
            ready.text(),
            tf!(
                "cleaning.tools.flux2_klein.model_ready_memory_status",
                memory = "VRAM 9.0"
            ),
            "a landed forecast takes the other template, so the figures cannot be dropped"
        );

        // The one way a READY model still needs attention: the spelled-out «не помещается»
        // warning lives inside «Память и скорость», which may well be folded away, so the
        // tone of this line has to carry it.
        let tight = flux2_readiness_line(
            Flux2ModelReadiness::Ready,
            false,
            Some(Flux2MemorySummary {
                line: "VRAM 30.0".to_string(),
                fits: false,
            }),
        );
        assert_eq!(tight.tone(), Flux2LineTone::Warn);
        assert!(!tight.offers_install(), "the model is installed; it is the memory that is short");
    }

    /// The catalog half of the readiness line: a bundle known to be incomplete turns a
    /// `Ready` or `Unknown` verdict into its own amber line with «Установить»; a `Missing`
    /// verdict keeps naming its components, and a complete bundle changes nothing.
    #[test]
    fn a_missing_catalog_bundle_takes_the_readiness_line_but_not_a_named_refusal() {
        for readiness in [Flux2ModelReadiness::Ready, Flux2ModelReadiness::Unknown] {
            let line = flux2_readiness_line(readiness, true, None);
            assert_eq!(line, Flux2ReadinessLine::BundleMissing, "{readiness:?}");
            assert_eq!(line.tone(), Flux2LineTone::Warn);
            assert!(line.offers_install());
            assert_eq!(line.text(), t!("cleaning.tools.flux2_klein.bundle_missing_status"));
        }
        let missing = Flux2ModelReadiness::Missing(Flux2MissingComponents { transformer: true, ..Flux2MissingComponents::default() });
        assert!(matches!(flux2_readiness_line(missing, true, None), Flux2ReadinessLine::Missing { .. }));
        assert_eq!(flux2_readiness_line(Flux2ModelReadiness::Ready, false, None), Flux2ReadinessLine::Ready { memory: None });
    }

    fn catalog_settings(variant: Flux2Variant, mode: Flux2SourceMode, source: Flux2EncoderSource, transformer: Flux2WeightFormat, text_encoder: Flux2WeightFormat) -> Flux2KleinSettings {
        Flux2KleinSettings {
            variant: variant.wire().to_string(),
            source_mode: mode.wire().to_string(),
            uncensored_text_encoder: source == Flux2EncoderSource::Uncensored,
            transformer_format: transformer.wire().to_string(),
            text_encoder_format: text_encoder.wire().to_string(),
            ..Flux2KleinSettings::default()
        }
    }

    /// The bundle follows the selected formats AND the encoder source (base, the 9B stock
    /// text-encoder configs, transformer, encoder, in that order) for BOTH variants, uses the
    /// legacy transformer spec in the legacy layout, and exists only in download mode.
    #[test]
    fn the_catalog_bundle_follows_the_selected_formats_and_only_in_download_mode() {
        for variant in Flux2Variant::all() {
            for &source in Flux2EncoderSource::offered_for(variant) {
                for &transformer in Flux2WeightFormat::offered_for(variant, Flux2WeightAxis::Transformer, source) {
                    for &text_encoder in Flux2WeightFormat::offered_for(variant, Flux2WeightAxis::TextEncoder, source) {
                        let settings = catalog_settings(variant, Flux2SourceMode::Download, source, transformer, text_encoder);
                        let bundle: Vec<&str> = flux2_catalog_bundle(&settings).iter().map(|spec| spec.id).collect();
                        let case = format!("{variant:?} {source:?} {transformer:?} / {text_encoder:?}");
                        // 9B keeps the stock encoder's configs in their own spec, after the base.
                        let configs = usize::from(variant == Flux2Variant::Klein9B && source == Flux2EncoderSource::Stock);
                        assert_eq!(bundle.len(), 3 + configs, "{case}: base (+ encoder configs) + transformer + encoder");
                        if configs == 1 {
                            assert_eq!(bundle[1], "flux2_klein_9b_text_encoder_config", "{case}");
                        }
                        let transformer_spec = flux2_klein_spec(variant, Flux2WeightAxis::Transformer, source, transformer).expect("offered");
                        let encoder_spec = flux2_klein_spec(variant, Flux2WeightAxis::TextEncoder, source, text_encoder).expect("offered");
                        assert_eq!(bundle[1 + configs], transformer_spec.id, "{case}");
                        assert_eq!(bundle[2 + configs], encoder_spec.id, "{case}");
                    }
                }
            }
            let manual = catalog_settings(variant, Flux2SourceMode::Manual, Flux2EncoderSource::Stock, Flux2WeightFormat::Q80, Flux2WeightFormat::Q80);
            assert!(flux2_catalog_bundle(&manual).is_empty(), "manual mode has no catalog block");
        }
        // The legacy bf16 layout of 9B swaps in the adopt-only diffusers spec.
        let mut legacy = catalog_settings(Flux2Variant::Klein9B, Flux2SourceMode::Download, Flux2EncoderSource::Stock, Flux2WeightFormat::Bf16, Flux2WeightFormat::Bf16);
        legacy.installed_formats.transformer_layout = Flux2TransformerLayout::LegacyDiffusers;
        let legacy_id = ms_sysprobe::ai_models::external_catalog::flux2_klein_legacy_transformer_spec(Flux2Variant::Klein9B).map(|spec| spec.id);
        assert_eq!(flux2_catalog_bundle(&legacy).get(2).map(|spec| spec.id), legacy_id, "after the base and the encoder configs");
    }

    /// The token row is drawn for the gated repositories of the bundle's specs that are not
    /// installed: none for 4B (open repositories), the BFL base for a fresh 9B, ponpoke too
    /// with the uncensored encoder, each once, and nothing once the gated specs are installed.
    #[test]
    fn the_token_row_names_the_gated_repositories_still_missing() {
        let bundle = |variant, source| flux2_catalog_bundle(&catalog_settings(variant, Flux2SourceMode::Download, source, Flux2WeightFormat::Bf16, Flux2WeightFormat::Bf16));
        let nothing_installed = |_: &ExternalModelSpec| false;
        assert!(catalog_gated_repos_missing(&bundle(Flux2Variant::Klein4B, Flux2EncoderSource::Stock), nothing_installed).is_empty());
        let stock = catalog_gated_repos_missing(&bundle(Flux2Variant::Klein9B, Flux2EncoderSource::Stock), nothing_installed);
        assert_eq!(stock, ["black-forest-labs/FLUX.2-klein-9B"], "base, transformer and encoder share one repository");
        let uncensored = catalog_gated_repos_missing(&bundle(Flux2Variant::Klein9B, Flux2EncoderSource::Uncensored), nothing_installed);
        assert_eq!(uncensored, ["black-forest-labs/FLUX.2-klein-9B", "ponpoke/flux2-klein-9b-uncensored-text-encoder"]);
        assert!(catalog_gated_repos_missing(&bundle(Flux2Variant::Klein9B, Flux2EncoderSource::Uncensored), |_| true).is_empty());
        // Only the uncensored encoder missing: only its repository.
        let only_encoder = |spec: &ExternalModelSpec| !spec.dir.ends_with("text_encoder_uncensored");
        assert_eq!(
            catalog_gated_repos_missing(&bundle(Flux2Variant::Klein9B, Flux2EncoderSource::Uncensored), only_encoder),
            ["ponpoke/flux2-klein-9b-uncensored-text-encoder"]
        );
    }

    /// A format row names the format, its real dtype (bf16 / the uncensored encoder's f16) and
    /// its catalog download size, and carries ✓ exactly when the installed-formats probe found
    /// it for THAT source; every offered row of either variant and component has a size.
    #[test]
    fn a_format_row_names_its_size_and_whether_it_is_installed() {
        let mut installed = Flux2InstalledFormats::default();
        installed.insert(Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock, Flux2WeightFormat::Q80);
        installed.insert(Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Bf16);
        for variant in Flux2Variant::all() {
            let mut choices = vec![(Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock)];
            choices.extend(Flux2EncoderSource::offered_for(variant).iter().map(|&source| (Flux2WeightAxis::TextEncoder, source)));
            for (axis, source) in choices {
                for &format in Flux2WeightFormat::offered_for(variant, axis, source) {
                    let spec = flux2_klein_spec(variant, axis, source, format).expect("every offered format has a spec");
                    let size = format_gib(spec.total_bytes());
                    let dtype = if axis == Flux2WeightAxis::TextEncoder && source == Flux2EncoderSource::Uncensored { "f16" } else { "bf16" };
                    let bare = match format.quant_token() {
                        None => tf!("cleaning.tools.flux2_klein.format_unquantized_label", dtype = dtype, size = size),
                        Some(quant) => tf!("cleaning.tools.flux2_klein.format_gguf_label", quant = quant, size = size),
                    };
                    let marked = (axis == Flux2WeightAxis::Transformer && format == Flux2WeightFormat::Q80)
                        || (axis == Flux2WeightAxis::TextEncoder && source == Flux2EncoderSource::Uncensored && format == Flux2WeightFormat::Bf16);
                    let expected = if marked { format!("{bare} ✓") } else { bare };
                    assert_eq!(flux2_weight_format_label(variant, axis, source, format, &installed), expected, "{variant:?} {axis:?} {source:?} {format:?}");
                }
            }
        }
        // The two GGUF hovers differ per axis: the costs are not the same.
        assert_ne!(
            flux2_weight_format_hint(Flux2WeightAxis::Transformer, Flux2WeightFormat::Q4KM),
            flux2_weight_format_hint(Flux2WeightAxis::TextEncoder, Flux2WeightFormat::Q4KM)
        );
    }
}
