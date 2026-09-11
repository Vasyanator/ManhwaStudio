/*
File: cleaning/tools/ai_editor/engines/flux2_klein/decisions.rs

Purpose:
The PURE decisions the FLUX.2 klein panel renders. Every function here answers one
question about the state the panel was handed — which single line to show, whether a run
may start, whether a region is legal — and none of them touches a `Ui`, a channel or the
disk. They live apart from the drawing precisely so the answers can be asserted without a
frame, which is what the tests at the end of this file do.

Main responsibilities:
- decide the at-most-one line under the prompt field (`flux2_prompt_cache_line`);
- decide whether the `guidance_scale` control is live at all (`flux2_guidance_supported`);
- decide what the always-visible readiness line reports (`flux2_readiness_line`), with the
  memory forecast folded in as `Flux2MemorySummary`;
- name the first reason a run cannot start (`flux2_run_block_reason`);
- validate a region SIZE against the model's hard limits (`region_block_reason`), the
  check the run gate and the worker share so the two cannot disagree.

Key structures:
- `Flux2PromptCacheLine`, `Flux2ReadinessLine`, `Flux2LineTone`, `Flux2MemorySummary`

Key functions:
- `flux2_prompt_cache_line()`, `flux2_guidance_supported()`, `flux2_readiness_line()`
- `flux2_run_block_reason()`, `region_block_reason()`

Notes:
Three-state on purpose throughout: an `Option<bool>` of `None` means "no answer yet" and
must never be read as `false`, because the difference between "not installed" and "not
known" is exactly what these lines exist to keep apart. The limits themselves
(`FLUX2_MIN_SELECTION_PX` and friends) and the tone colours stay in the module root.
*/

use super::*;

/// The at-most-one compact line drawn under the prompt field.
///
/// A separate decision from the drawing because it is a PRIORITY rule, not a formatting
/// one: three facts compete for one line and only the most consequential may take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2PromptCacheLine {
    /// Nothing is claimed. The answer is outstanding, which is not information — the old
    /// «Состояние кэша промпта пока неизвестно.» line said only that the panel had asked.
    Silent,
    /// The embeddings of this prompt are held: the next run skips the encoder read entirely.
    Cached,
    /// They are not: the next run pays for them.
    NotCached,
    /// This machine has NO text encoder, so nothing can be encoded here and only ready
    /// caches work. It outranks "not cached", which would read as a delay the user could
    /// wait out rather than as a run that cannot start.
    NoEncoder,
}

/// Decides which single line the prompt block shows about the cache.
///
/// Both inputs are the project's three-state `Option<bool>`, where `None` is "not known"
/// and never `false`. The priority is what the user can DO next:
/// - a cached prompt wins outright, because with the embeddings held the run works whether
///   an encoder exists or not — that is the whole point of the library;
/// - otherwise a missing encoder wins, because it changes what is POSSIBLE rather than
///   what something costs;
/// - "not cached" is left, and an outstanding answer says nothing at all.
#[must_use]
pub(super) fn flux2_prompt_cache_line(
    prompt_cached: Option<bool>,
    text_encoder_available: Option<bool>,
) -> Flux2PromptCacheLine {
    if prompt_cached == Some(true) {
        return Flux2PromptCacheLine::Cached;
    }
    if text_encoder_available == Some(false) {
        return Flux2PromptCacheLine::NoEncoder;
    }
    match prompt_cached {
        // Handled above; spelled out so the match stays exhaustive over the three states.
        Some(true) | None => Flux2PromptCacheLine::Silent,
        Some(false) => Flux2PromptCacheLine::NotCached,
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

/// The tone a one-line verdict is drawn in.
///
/// The three colours are this file's own ([`FLUX2_STATUS_OK_COLOR`] and friends); naming
/// the tone rather than the colour keeps the decision testable without a live `Ui`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2LineTone {
    Ok,
    Warn,
    Neutral,
}

/// The memory forecast as the readiness line carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Flux2MemorySummary {
    /// The one-line forecast, already formatted by [`estimate_status_line`].
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
            Self::Missing { .. } => Flux2LineTone::Warn,
            Self::Unknown => Flux2LineTone::Neutral,
        }
    }

    /// Whether «Установить» is offered beside the line.
    pub(super) fn offers_install(&self) -> bool {
        matches!(self, Self::Missing { .. })
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
#[must_use]
pub(super) fn flux2_readiness_line(
    readiness: Flux2ModelReadiness,
    memory: Option<Flux2MemorySummary>,
) -> Flux2ReadinessLine {
    match readiness {
        Flux2ModelReadiness::Ready => Flux2ReadinessLine::Ready { memory },
        Flux2ModelReadiness::Missing(missing) => Flux2ReadinessLine::Missing {
            components: missing.names(),
        },
        Flux2ModelReadiness::Unknown => Flux2ReadinessLine::Unknown,
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
    /// it, so the priority is a rule and not a formatting accident. A cached prompt wins
    /// outright — the run works with no encoder at all, which is what the library is for —
    /// and only after that does a missing encoder outrank «не кэширован», because it says
    /// the run cannot start rather than that it will be slow.
    #[test]
    fn the_prompt_line_reports_the_most_consequential_state_and_stays_silent_when_unsure() {
        // Nothing has answered: an outstanding query is not information. The old
        // «Состояние кэша промпта пока неизвестно.» line said only that the panel had asked.
        assert_eq!(
            flux2_prompt_cache_line(None, None),
            Flux2PromptCacheLine::Silent
        );
        assert_eq!(
            flux2_prompt_cache_line(Some(true), None),
            Flux2PromptCacheLine::Cached
        );
        assert_eq!(
            flux2_prompt_cache_line(Some(false), None),
            Flux2PromptCacheLine::NotCached
        );
        // A cached prompt survives a machine with no encoder — that is the whole point of
        // a carried `.msprompt`, and the run gate waives the encoder on exactly this.
        assert_eq!(
            flux2_prompt_cache_line(Some(true), Some(false)),
            Flux2PromptCacheLine::Cached
        );
        // Without the embeddings AND without an encoder nothing can be produced here, so
        // that outranks the cache state — including an unanswered one.
        assert_eq!(
            flux2_prompt_cache_line(Some(false), Some(false)),
            Flux2PromptCacheLine::NoEncoder
        );
        assert_eq!(
            flux2_prompt_cache_line(None, Some(false)),
            Flux2PromptCacheLine::NoEncoder
        );
        // "Not known" is never `false`: an encoder that has not been reported on must not
        // raise the warning that closes the two encode-only library buttons.
        assert_eq!(
            flux2_prompt_cache_line(Some(false), None),
            Flux2PromptCacheLine::NotCached
        );
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

        let unknown = flux2_readiness_line(Flux2ModelReadiness::Unknown, Some(forecast.clone()));
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
        let bare = flux2_readiness_line(Flux2ModelReadiness::Ready, None);
        assert_eq!(bare.tone(), Flux2LineTone::Ok);
        assert!(!bare.offers_install());
        assert_eq!(
            bare.text(),
            t!("cleaning.tools.flux2_klein.model_ready_status")
        );

        let ready = flux2_readiness_line(Flux2ModelReadiness::Ready, Some(forecast));
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
            Some(Flux2MemorySummary {
                line: "VRAM 30.0".to_string(),
                fits: false,
            }),
        );
        assert_eq!(tight.tone(), Flux2LineTone::Warn);
        assert!(!tight.offers_install(), "the model is installed; it is the memory that is short");
    }
}
