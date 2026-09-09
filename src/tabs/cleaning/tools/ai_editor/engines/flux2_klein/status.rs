/*
File: cleaning/tools/ai_editor/engines/flux2_klein/status.rs

Purpose:
The `.status` answer of the FLUX.2 klein backend and every verdict derived from it: the
component presence catalog, the per-component residency block with the actions the
backend offered, and the three-state "is the model installed on this machine" answer the
panel and the run path both consult.

Main responsibilities:
- own `Flux2Status` and the component vocabulary it is spelled in;
- decide whether a catalog still describes the paths currently configured
  (`flux2_status_for_paths`, `flux2_status_paths_stale`) and turn it into
  `Flux2ModelReadiness`;
- merge the presence half and the residency half of the answer into the panel's rows
  (`flux2_component_rows`);
- report whether the pipeline is busy and whether the install fold should be seeded open.

Key structures:
- `Flux2Status`, `Flux2Component`, `Flux2ComponentSnapshot`, `Flux2ComponentResidency`
- `Flux2ComponentId`, `Flux2ComponentAction`, `Flux2Residency`, `Flux2ComponentBlock`
- `Flux2MissingComponents`, `Flux2ModelReadiness`, `Flux2InstallSeed`
- `Flux2CatalogComponent`, `Flux2ComponentRow`

Key functions:
- `flux2_model_readiness()`, `flux2_status_for_paths()`, `flux2_status_paths_stale()`
- `flux2_component_rows()`, `flux2_component_block()`, `flux2_pipeline_busy()`
- `flux2_install_seed()`, `component_action_tooltip()`, `flux2_component_row_tooltip()`

Notes:
The parsers that BUILD a `Flux2Status` from a backend header live in `wire.rs`; this
file owns the shape and the decisions taken on it. `Flux2ModelReadiness` is three-state
on purpose: `Unknown` never blocks a run.
*/

use super::*;

// ---------------------------------------------------------------------------------------
// Backend answers
// ---------------------------------------------------------------------------------------

/// One entry of the `.status` component catalog.
#[derive(Debug, Clone, Default)]
pub(super) struct Flux2Component {
    pub(super) path: String,
    /// `exists` for a path component, `found` for the tokenizer/scheduler.
    pub(super) present: bool,
    pub(super) size_bytes: u64,
}

impl Flux2Component {
    /// Reads a component entry, accepting either the `exists` or the `found` spelling
    /// of "is it there" so the tokenizer/scheduler entries parse with the same code.
    pub(super) fn parse(value: Option<&Value>) -> Self {
        let Some(value) = value else {
            return Self::default();
        };
        let present = value
            .get("exists")
            .or_else(|| value.get("found"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Self {
            path: value
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            present,
            size_bytes: value
                .get("size_bytes")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        }
    }
}

/// Where one component's weights are RIGHT NOW, as `.status` reports it.
///
/// The wire literals are pinned by `dev-docs/flux2_component_residency.md` §2 and are
/// stable. Three labels are not enough and the two extra ones are not decoration:
/// under `sequential_cpu_offload` accelerate moves the parameters to the `meta` device
/// and keeps the bytes in a host weights map, so the component is neither in RAM nor on
/// the GPU (`Offloaded`), and a load can leave part of a component behind (`Mixed`).
/// Neither is ever rounded to one of the other three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2Residency {
    /// The component does not exist in the service right now.
    NotLoaded,
    /// Every parameter and buffer is on the host.
    Ram,
    /// Every parameter and buffer is on the compute device.
    Gpu,
    /// An accelerate hook owns it: the parameters sit on `meta`, the bytes in a host
    /// weights map.
    Offloaded,
    /// Genuinely split across devices.
    Mixed,
}

impl Flux2Residency {
    /// The wire literal of this state.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::NotLoaded => "not_loaded",
            Self::Ram => "ram",
            Self::Gpu => "gpu",
            Self::Offloaded => "offloaded",
            Self::Mixed => "mixed",
        }
    }

    /// Reads a residency literal.
    ///
    /// `None` for anything this build does not know — a NEWER backend that reports a
    /// state added after this release. The honest answer there is "not known": coercing
    /// an unknown literal into `NotLoaded` would tell the user their weights are gone,
    /// and into any of the others would name a device nobody reported.
    pub(super) fn from_wire(value: &str) -> Option<Self> {
        [
            Self::NotLoaded,
            Self::Ram,
            Self::Gpu,
            Self::Offloaded,
            Self::Mixed,
        ]
        .into_iter()
        .find(|candidate| candidate.wire() == value)
    }

    /// The localized name of the state.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::NotLoaded => t!("cleaning.tools.flux2_klein.component_residency_not_loaded"),
            Self::Ram => t!("cleaning.tools.flux2_klein.component_residency_ram"),
            Self::Gpu => t!("cleaning.tools.flux2_klein.component_residency_gpu"),
            Self::Offloaded => t!("cleaning.tools.flux2_klein.component_residency_offloaded"),
            Self::Mixed => t!("cleaning.tools.flux2_klein.component_residency_mixed"),
        }
    }

    /// The colour the state is drawn in, `None` for a neutral line.
    ///
    /// GREEN is "loaded and wholly on one device" — and that includes `Ram`, because the
    /// text encoder can NEVER be on the GPU (prompt encoding pins `torch.device("cpu")`),
    /// so amber there would call the ideal state a problem. AMBER is "loaded, but in a
    /// state that needs the explanation its hover carries". NEUTRAL is "nothing is
    /// loaded", which is not a fault either.
    pub(super) fn color(self) -> Option<Color32> {
        match self {
            Self::NotLoaded => None,
            Self::Ram | Self::Gpu => Some(FLUX2_STATUS_OK_COLOR),
            Self::Offloaded | Self::Mixed => Some(FLUX2_STATUS_WARN_COLOR),
        }
    }

    /// The hover that explains the state, for the two that are not self-explanatory.
    pub(super) fn hint(self) -> Option<&'static str> {
        match self {
            Self::NotLoaded | Self::Ram | Self::Gpu => None,
            Self::Offloaded => Some(t!(
                "cleaning.tools.flux2_klein.component_residency_offloaded_hint"
            )),
            Self::Mixed => Some(t!("cleaning.tools.flux2_klein.component_residency_mixed_hint")),
        }
    }
}

/// The three components the residency block describes, in display order.
///
/// The tokenizer and the scheduler are deliberately absent: they are configuration files
/// rather than weights, so there is no residency to report and no action to offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2ComponentId {
    TextEncoder,
    Transformer,
    Vae,
}

impl Flux2ComponentId {
    /// The wire name, which is also the key of this component in the `components` object.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::TextEncoder => "text_encoder",
            Self::Transformer => "transformer",
            Self::Vae => "vae",
        }
    }

    /// The three components in the order the block draws them.
    pub(super) fn all() -> [Self; 3] {
        [Self::TextEncoder, Self::Transformer, Self::Vae]
    }

    /// The localized component name, the SAME one the presence catalog above uses: one
    /// component must not carry two names in two adjacent blocks.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::TextEncoder => t!("cleaning.tools.flux2_klein.component_text_encoder"),
            Self::Transformer => t!("cleaning.tools.flux2_klein.component_transformer"),
            Self::Vae => t!("cleaning.tools.flux2_klein.component_vae"),
        }
    }

}

/// One action the BACKEND offered for a component.
///
/// Which of them are possible is decided by the service alone and travels in the
/// component's `actions` list; this side renders what it is given and never re-derives
/// the matrix — it depends on the accelerate hooks, the model cache key and the memory
/// guard, none of which this side can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2ComponentAction {
    Load,
    Unload,
    ToRam,
    ToGpu,
    Warmup,
}

impl Flux2ComponentAction {
    /// The wire literal of the action.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Unload => "unload",
            Self::ToRam => "to_ram",
            Self::ToGpu => "to_gpu",
            Self::Warmup => "warmup",
        }
    }

    /// Reads an action literal, `None` for one this build does not know.
    ///
    /// Such an entry is DROPPED from the list rather than shown: a button whose caption
    /// is untranslatable and whose effect is unknown cannot honestly be offered, and
    /// guessing an effect from a wire name is exactly what this design exists to avoid.
    /// The rest of the list still renders, so a newer backend loses one button instead of
    /// the whole block.
    pub(super) fn from_wire(value: &str) -> Option<Self> {
        [
            Self::Load,
            Self::Unload,
            Self::ToRam,
            Self::ToGpu,
            Self::Warmup,
        ]
        .into_iter()
        .find(|candidate| candidate.wire() == value)
    }

    /// The button caption.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Load => t!("cleaning.tools.flux2_klein.component_action_load_button"),
            Self::Unload => t!("cleaning.tools.flux2_klein.component_action_unload_button"),
            Self::ToRam => t!("cleaning.tools.flux2_klein.component_action_to_ram_button"),
            Self::ToGpu => t!("cleaning.tools.flux2_klein.component_action_to_gpu_button"),
            Self::Warmup => t!("cleaning.tools.flux2_klein.component_action_warmup_button"),
        }
    }
}

/// The hover of one action button.
///
/// `load` and `unload` on the TRANSFORMER and the VAE get their own text: the model cache
/// key describes a whole pipeline, so dropping one of the two alone would make the next
/// request hit a broken cache entry — they load and unload TOGETHER, and the user has to
/// be told that before pressing rather than after.
pub(super) fn component_action_tooltip(id: Flux2ComponentId, action: Flux2ComponentAction) -> &'static str {
    let pipeline_pair = matches!(id, Flux2ComponentId::Transformer | Flux2ComponentId::Vae);
    match (action, pipeline_pair) {
        (Flux2ComponentAction::Load, true) => {
            t!("cleaning.tools.flux2_klein.component_action_load_pair_tooltip")
        }
        (Flux2ComponentAction::Unload, true) => {
            t!("cleaning.tools.flux2_klein.component_action_unload_pair_tooltip")
        }
        (Flux2ComponentAction::Load, false) => {
            t!("cleaning.tools.flux2_klein.component_action_load_tooltip")
        }
        (Flux2ComponentAction::Unload, false) => {
            t!("cleaning.tools.flux2_klein.component_action_unload_tooltip")
        }
        (Flux2ComponentAction::ToRam, _) => {
            t!("cleaning.tools.flux2_klein.component_action_to_ram_tooltip")
        }
        (Flux2ComponentAction::ToGpu, _) => {
            t!("cleaning.tools.flux2_klein.component_action_to_gpu_tooltip")
        }
        (Flux2ComponentAction::Warmup, _) => {
            t!("cleaning.tools.flux2_klein.component_action_warmup_tooltip")
        }
    }
}

/// One row of the residency block: where a component is, and what may be done to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Flux2ComponentResidency {
    pub(super) id: Flux2ComponentId,
    /// The parsed state; `None` when the backend reported none, or reported a literal
    /// this build does not know ([`Flux2Residency::from_wire`]).
    pub(super) residency: Option<Flux2Residency>,
    /// The literal the backend actually sent, kept verbatim so an unrecognised one can
    /// still be shown on hover instead of vanishing without trace.
    pub(super) residency_wire: String,
    /// The actions the BACKEND offered, in the order it listed them, with the ones this
    /// build does not know dropped. This is the whole button set — nothing is added here.
    pub(super) actions: Vec<Flux2ComponentAction>,
}

/// The `components` / `components_busy` pair of a `.status` or `.component_action` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Flux2ComponentSnapshot {
    /// One row per component the backend described, in [`Flux2ComponentId::all`] order.
    ///
    /// `None` means NOT KNOWN — the field was absent (the service could not take its lock
    /// without waiting) or was not an object. It must never read as "not loaded": that is
    /// the same three-state rule `prompt_cached` and `text_encoder_available` already
    /// carry, and here it decides whether the user is told their 16 GB encoder is gone.
    pub(super) components: Option<Vec<Flux2ComponentResidency>>,
    /// The service could not answer without waiting for the lock a generation holds for
    /// its whole run. `false` when the field is absent, which is also what a backend that
    /// predates the block reports — it then has no `components` either, so the block
    /// reports "not known" rather than "busy".
    pub(super) components_busy: bool,
}

/// The `.status` answer: whether a run can start at all, plus the host's memory.
#[derive(Debug, Clone, Default)]
pub(super) struct Flux2Status {
    pub(super) available: bool,
    pub(super) reason: String,
    pub(super) text_encoder: Flux2Component,
    pub(super) transformer: Flux2Component,
    pub(super) vae: Flux2Component,
    pub(super) tokenizer: Flux2Component,
    pub(super) scheduler: Flux2Component,
    /// Total device VRAM in bytes, `0` when the backend did not report one. Shown
    /// beside the forecast's "free" figure, which alone says nothing about headroom.
    pub(super) vram_total: u64,
    /// Total host RAM in bytes, `0` when unknown. Same role as `vram_total`.
    pub(super) ram_total: u64,
    pub(super) loaded: bool,
    pub(super) device: String,
    /// Whether the backend already holds the embeddings of the prompt this answer was
    /// asked about, so a run can skip the ~16 GB text encoder entirely.
    ///
    /// `None` when the backend did not report the field at all — an older build, or one
    /// whose prompt-cache methods do not exist yet. That is NOT the same as `Some(false)`
    /// and must not be shown as "not cached": the honest answer there is "not known".
    pub(super) prompt_cached: Option<bool>,
    /// Whether a text encoder is present ON THIS MACHINE for the paths this answer was
    /// asked about. Reported separately from `available`, because a run whose prompt is
    /// already cached needs no encoder at all: `available` stays `true` while this is
    /// `false`, and only the operations that must ENCODE (a new prompt, `.build`, a
    /// `.save` that has to name the encoder) are refused.
    ///
    /// An empty path and a path that does not exist are the same `false` — the second is
    /// what a settings file carried over from another machine looks like.
    ///
    /// `None` is "not known": a backend that predates the field, or no answer yet. It must
    /// not read as `Some(false)`, which is what the warning line and the encode gates act on.
    pub(super) text_encoder_available: Option<bool>,
    /// Whether `guidance_scale` can change anything on the checkpoint the backend loaded.
    ///
    /// A DISTILLED checkpoint declares `"is_distilled": true` in its `model_index.json`,
    /// and diffusers then computes `do_classifier_free_guidance = guidance_scale > 1 and
    /// not is_distilled`. On such a checkpoint a guidance above 1.0 doubles the per-step
    /// compute and leaves the result identical, so the parameter is not a trade-off the
    /// user can make — it is inert, and the panel closes the control rather than let it
    /// move a number with no effect.
    ///
    /// `None` is "not known" and MUST read as SUPPORTED — today's behaviour. The field is
    /// new, so an older backend and any answer produced before it existed report nothing;
    /// treating that silence as `Some(false)` would take a working control away from a
    /// user whose backend is merely older. Only a positive `false` gates anything, which is
    /// the single place that rule is applied ([`flux2_guidance_supported`]).
    pub(super) guidance_supported: Option<bool>,
    /// Where each component's weights are, and what may be done to them. Three-state:
    /// see [`Flux2ComponentSnapshot::components`].
    pub(super) components: Flux2ComponentSnapshot,
}

/// What the per-component residency block shows this frame.
///
/// A separate decision from the drawing so the three-state rule can be asserted without a
/// live `Ui`: the difference between "busy", "the backend said nothing" and "everything is
/// unloaded" is exactly what this block must never blur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2ComponentBlock<'a> {
    /// No `.status` answer at all. The catalog above already says so; a second "not known"
    /// line under it would be noise.
    Hidden,
    /// The service could not read the residencies without waiting for the lock a
    /// generation holds. The block says THAT and draws neither stale states nor buttons —
    /// every one of them would be about a moment that has passed.
    Busy,
    /// There is an answer, but it carries no `components`: a backend that predates the
    /// block. Reported as "not known", never as "not loaded".
    Unknown,
    /// One row per component the backend described.
    Rows(&'a [Flux2ComponentResidency]),
}

/// Decides what the residency block shows from the last `.status` answer.
pub(super) fn flux2_component_block(status: Option<&Flux2Status>) -> Flux2ComponentBlock<'_> {
    let Some(status) = status else {
        return Flux2ComponentBlock::Hidden;
    };
    // Busy wins over a `components` the backend may still have sent: it says the
    // residencies could not be read, so whatever came with it is not current.
    if status.components.components_busy {
        return Flux2ComponentBlock::Busy;
    }
    match status.components.components.as_deref() {
        Some(rows) => Flux2ComponentBlock::Rows(rows),
        None => Flux2ComponentBlock::Unknown,
    }
}

/// Whether the backend's ONE pipeline — and with it the single shared progress bar — is
/// held right now by one of the FOUR long operations that claim it.
///
/// A generation, a `.prompt_cache.build`, a `.component_action` and a `.download.start`
/// all take the service for minutes (the download for hours) and all call
/// [`begin_progress_generation`], so starting a second one would both queue behind the
/// first and steal its bar. Every gate that means "wait for the current operation" reads
/// this, and nothing re-derives it field by field.
pub(super) fn flux2_pipeline_busy(
    run_in_flight: bool,
    prompt_cache_in_flight: bool,
    component_action_in_flight: bool,
    download_in_flight: bool,
) -> bool {
    run_in_flight || prompt_cache_in_flight || component_action_in_flight || download_in_flight
}

/// Which of the model components a run needs the machine does not have.
///
/// A SET and not a list so the verdict stays `Copy` and a refusal can name every missing
/// part at once: a user who is told only about the transformer downloads it, presses the
/// button again and is told about the VAE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct Flux2MissingComponents {
    pub(super) text_encoder: bool,
    pub(super) transformer: bool,
    pub(super) vae: bool,
    pub(super) tokenizer: bool,
    pub(super) scheduler: bool,
}

impl Flux2MissingComponents {
    /// Whether anything at all is missing.
    #[must_use]
    pub(super) fn any(self) -> bool {
        self.text_encoder || self.transformer || self.vae || self.tokenizer || self.scheduler
    }

    /// The localized component names, in the order of the presence catalog, joined with
    /// «, ». Empty when nothing is missing.
    ///
    /// The names are the SAME keys the catalog above prints, so the refusal and the ✗ rows
    /// the user is looking at cannot call one component two different things. `t!` takes a
    /// literal, which is why the pairs are spelled out instead of looked up in a table.
    #[must_use]
    pub(super) fn names(self) -> String {
        [
            (self.text_encoder, t!("cleaning.tools.flux2_klein.component_text_encoder")),
            (self.transformer, t!("cleaning.tools.flux2_klein.component_transformer")),
            (self.vae, t!("cleaning.tools.flux2_klein.component_vae")),
            (self.tokenizer, t!("cleaning.tools.flux2_klein.component_tokenizer")),
            (self.scheduler, t!("cleaning.tools.flux2_klein.component_scheduler")),
        ]
        .into_iter()
        .filter_map(|(missing, name)| missing.then_some(name))
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// Whether the model a run would use is actually installed on THIS machine.
///
/// The ONE answer to that question: the run gate refuses on it, and the panel's status
/// line reads the same verdict rather than deriving a second one that could disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2ModelReadiness {
    /// Nothing has answered yet — no `.status` round trip has landed for the current
    /// paths. This is NOT evidence of a missing model and **must never block a run**: a
    /// slow or briefly unreachable backend would otherwise leave the button permanently
    /// dead with no way for the user to find out why.
    Unknown,
    /// The backend answered, or a required path is empty, and at least one component a
    /// run needs is not there. This is the only verdict that blocks.
    Missing(Flux2MissingComponents),
    /// Every component a run needs is present.
    Ready,
}

/// The `.status` catalog, kept only while it still describes the paths in the settings.
///
/// `asked_about` is the effective-paths triple the answer was fetched for; `current` is
/// what [`Flux2KleinSettings::effective_paths`] yields now. The presence half of the
/// catalog is a statement about THOSE files, so once the two disagree the answer describes
/// files nobody is asking about any more and "not known" is the only honest report —
/// exactly the rule [`prompt_cache_state_for`] applies to the prompt half of the same
/// answer. Without it a corrected model path left the previous `Missing` verdict standing
/// and «Обработать» kept naming components that are now there.
///
/// A free function so the rule can be tested without a live engine.
#[must_use]
pub(super) fn flux2_status_for_paths<'a>(
    status: Option<&'a Flux2Status>,
    asked_about: Option<&Flux2EffectivePaths>,
    current: &Flux2EffectivePaths,
) -> Option<&'a Flux2Status> {
    let status = status?;
    (asked_about? == current).then_some(status)
}

/// Whether a fresh `.status` query is owed because the settings now name other paths than
/// anything already answered or in flight.
///
/// `answered` is what the cached catalog describes, `in_flight` what the query on the wire
/// was asked about. A query already carrying `current` is enough: arming again would only
/// send a duplicate the moment the first one lands.
#[must_use]
pub(super) fn flux2_status_paths_stale(
    current: &Flux2EffectivePaths,
    answered: Option<&Flux2EffectivePaths>,
    in_flight: Option<&Flux2EffectivePaths>,
) -> bool {
    answered != Some(current) && in_flight != Some(current)
}

/// Derives [`Flux2ModelReadiness`] from the last `.status` answer and the EFFECTIVE paths.
///
/// `status` must be an answer that describes the CURRENT paths — pass
/// [`Flux2KleinEngine::status_for_current_paths`], never the raw field, or a catalog
/// fetched for paths the user has since corrected will keep reading as `Missing`.
///
/// `status` is the cached component catalog (`None` until the first answer lands);
/// `prompt_cached` is the same three-state answer [`flux2_run_block_reason`] reads, and
/// `Some(true)` waives the text encoder exactly as it does there — a cached prompt is
/// generated without ever reading the 16 GB Qwen3.
///
/// An EMPTY required path is decided locally and needs no backend: nothing can be found at
/// a path that was never given. Everything else waits for the catalog, which is why the
/// download mode needs this function at all — [`Flux2KleinSettings::effective_paths`]
/// DERIVES its three paths under `config::flux2_klein_dir()`, so they are never empty and
/// the emptiness check alone can never notice that nothing has been downloaded yet.
///
/// The tokenizer and the scheduler have no path of their own: the backend resolves them
/// beside the three that do, so they can only ever be judged from the catalog.
#[must_use]
pub(super) fn flux2_model_readiness(
    settings: &Flux2KleinSettings,
    status: Option<&Flux2Status>,
    prompt_cached: Option<bool>,
) -> Flux2ModelReadiness {
    let paths = settings.effective_paths();
    let encoder_needed = prompt_cached != Some(true);
    let unset = Flux2MissingComponents {
        text_encoder: encoder_needed && paths.text_encoder.is_empty(),
        transformer: paths.transformer.is_empty(),
        vae: paths.vae.is_empty(),
        ..Flux2MissingComponents::default()
    };
    if unset.any() {
        return Flux2ModelReadiness::Missing(unset);
    }
    let Some(status) = status else {
        return Flux2ModelReadiness::Unknown;
    };
    let missing = Flux2MissingComponents {
        text_encoder: encoder_needed && !status.text_encoder.present,
        transformer: !status.transformer.present,
        vae: !status.vae.present,
        tokenizer: !status.tokenizer.present,
        scheduler: !status.scheduler.present,
    };
    if missing.any() {
        Flux2ModelReadiness::Missing(missing)
    } else {
        Flux2ModelReadiness::Ready
    }
}

/// What the one-shot seeding does to «Установка модели» on this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2InstallSeed {
    /// Touch neither the fold nor the flag: the seeding is spent, or no definite verdict
    /// has landed yet and guessing is exactly what the flag exists to avoid.
    Keep,
    /// Open the section and spend the seeding — the first `Missing`.
    Open,
    /// Spend the seeding without touching the fold: everything is installed, or the user
    /// has already moved the section himself.
    Seal,
}

/// Decides the one-shot opening of «Установка модели».
///
/// `seeded` is whether the seeding is already spent; `user_moved` says the fold changed
/// under a hand this file did not move it with; `readiness` is the current verdict.
///
/// The rule is "open it once, when we first learn the model is missing" — and a section the
/// user has closed by hand is his, so a `Missing` landing afterwards must not reopen it.
/// That window is real rather than theoretical: `.status` can take seconds, and the whole
/// time the verdict is `Unknown` the user is free to open the section, look and close it.
/// «Установить» is unaffected — it opens the section explicitly, on any frame.
///
/// A free function so the rule is pinned by tests instead of living inside the drawing.
#[must_use]
pub(super) fn flux2_install_seed(
    seeded: bool,
    user_moved: bool,
    readiness: Flux2ModelReadiness,
) -> Flux2InstallSeed {
    if seeded {
        return Flux2InstallSeed::Keep;
    }
    if user_moved {
        return Flux2InstallSeed::Seal;
    }
    match readiness {
        Flux2ModelReadiness::Unknown => Flux2InstallSeed::Keep,
        Flux2ModelReadiness::Missing(_) => Flux2InstallSeed::Open,
        Flux2ModelReadiness::Ready => Flux2InstallSeed::Seal,
    }
}

/// The five entries of the `.status` presence catalog, in display order.
///
/// Wider than [`Flux2ComponentId`], which names only the three components that carry
/// WEIGHTS and therefore a residency: the tokenizer and the scheduler are configuration
/// files, and the merged list still has to show whether they are on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2CatalogComponent {
    TextEncoder,
    Transformer,
    Vae,
    Tokenizer,
    Scheduler,
}

impl Flux2CatalogComponent {
    /// The five components in the order the list draws them.
    pub(super) fn all() -> [Self; 5] {
        [
            Self::TextEncoder,
            Self::Transformer,
            Self::Vae,
            Self::Tokenizer,
            Self::Scheduler,
        ]
    }

    /// The localized component name — the SAME keys the run-gate refusal names, so a
    /// refusal and the ✗ row the user is looking at cannot call one component two things.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::TextEncoder => t!("cleaning.tools.flux2_klein.component_text_encoder"),
            Self::Transformer => t!("cleaning.tools.flux2_klein.component_transformer"),
            Self::Vae => t!("cleaning.tools.flux2_klein.component_vae"),
            Self::Tokenizer => t!("cleaning.tools.flux2_klein.component_tokenizer"),
            Self::Scheduler => t!("cleaning.tools.flux2_klein.component_scheduler"),
        }
    }

    /// The hover that says what the component IS. A row reports presence, size and
    /// residency, none of which means anything to a user who does not know what each
    /// part does.
    pub(super) fn hint(self) -> &'static str {
        match self {
            Self::TextEncoder => t!("cleaning.tools.flux2_klein.component_text_encoder_hint"),
            Self::Transformer => t!("cleaning.tools.flux2_klein.component_transformer_hint"),
            Self::Vae => t!("cleaning.tools.flux2_klein.component_vae_hint"),
            Self::Tokenizer => t!("cleaning.tools.flux2_klein.component_tokenizer_hint"),
            Self::Scheduler => t!("cleaning.tools.flux2_klein.component_scheduler_hint"),
        }
    }

    /// The residency identity of this component, `None` for the two that have no weights
    /// and therefore never appear in the backend's `components` block.
    pub(super) fn residency_id(self) -> Option<Flux2ComponentId> {
        match self {
            Self::TextEncoder => Some(Flux2ComponentId::TextEncoder),
            Self::Transformer => Some(Flux2ComponentId::Transformer),
            Self::Vae => Some(Flux2ComponentId::Vae),
            Self::Tokenizer | Self::Scheduler => None,
        }
    }

    /// The presence entry of this component in a `.status` answer.
    pub(super) fn presence(self, status: &Flux2Status) -> &Flux2Component {
        match self {
            Self::TextEncoder => &status.text_encoder,
            Self::Transformer => &status.transformer,
            Self::Vae => &status.vae,
            Self::Tokenizer => &status.tokenizer,
            Self::Scheduler => &status.scheduler,
        }
    }
}

/// One row of the merged component list: everything one `.status` answer says about a
/// single component.
///
/// The two blocks this replaces sat next to each other and described the same five things
/// twice — one by presence and size, the other by residency and the buttons that change
/// it. A row carries both halves so a component is named once and read once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Flux2ComponentRow<'a> {
    pub(super) component: Flux2CatalogComponent,
    /// Whether the files are on disk (`exists` / `found` in the answer).
    pub(super) present: bool,
    /// Size on disk in bytes, `0` when the backend reported none.
    pub(super) size_bytes: u64,
    /// The path the BACKEND resolved, empty when it reported none. It is the only way to
    /// tell a typo in the field above from a genuinely missing file, so it goes on hover.
    pub(super) path: &'a str,
    /// The residency half. `None` for a component the backend's `components` block does
    /// not cover — the tokenizer and the scheduler always, and every row while the block
    /// is busy or the backend predates it.
    pub(super) residency: Option<&'a Flux2ComponentResidency>,
}

/// Builds the merged component list from a `.status` answer.
///
/// `residency` is the block's rows when there are any — `None` covers "busy", "not
/// reported" and "no answer", which the caller reports once as a line rather than five
/// times as empty cells. A component the block simply omits keeps its presence half and
/// shows no state, which is the honest rendering of a partial answer.
#[must_use]
pub(super) fn flux2_component_rows<'a>(
    status: &'a Flux2Status,
    residency: Option<&'a [Flux2ComponentResidency]>,
) -> Vec<Flux2ComponentRow<'a>> {
    Flux2CatalogComponent::all()
        .into_iter()
        .map(|component| {
            let presence = component.presence(status);
            Flux2ComponentRow {
                component,
                present: presence.present,
                size_bytes: presence.size_bytes,
                path: presence.path.as_str(),
                residency: component.residency_id().and_then(|id| {
                    residency.and_then(|rows| rows.iter().find(|row| row.id == id))
                }),
            }
        })
        .collect()
}

/// The hover of one merged row: what the component IS, then the path the backend
/// resolved for it.
///
/// Merging the two lists merged their hovers too — the presence catalog showed the path
/// and the residency block explained the component — and a single label has a single
/// hover, so neither half may be dropped.
#[must_use]
pub(super) fn flux2_component_row_tooltip(component: Flux2CatalogComponent, path: &str) -> String {
    let hint = component.hint();
    if path.is_empty() {
        return hint.to_string();
    }
    format!("{hint}\n{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// «Установка модели» opens itself once, and never against the user's hand.
    ///
    /// The window the last rule closes is real: `.status` can take seconds, and while the
    /// verdict is `Unknown` the user is free to open the section, look at it and close it
    /// again — the first `Missing` landing afterwards used to reopen it.
    #[test]
    fn the_install_section_seeds_once_and_yields_to_the_user() {
        let missing = Flux2ModelReadiness::Missing(Flux2MissingComponents {
            transformer: true,
            ..Flux2MissingComponents::default()
        });

        // Nothing has answered: the section keeps waiting rather than guess.
        assert_eq!(
            flux2_install_seed(false, false, Flux2ModelReadiness::Unknown),
            Flux2InstallSeed::Keep
        );
        // The first `Missing` opens it, once.
        assert_eq!(flux2_install_seed(false, false, missing), Flux2InstallSeed::Open);
        assert_eq!(flux2_install_seed(true, false, missing), Flux2InstallSeed::Keep);
        // Installed: the seeding is spent without unfolding anything.
        assert_eq!(
            flux2_install_seed(false, false, Flux2ModelReadiness::Ready),
            Flux2InstallSeed::Seal
        );
        // The user moved the fold while the verdict was still outstanding. From then on the
        // section is his, whatever lands next.
        for readiness in [Flux2ModelReadiness::Unknown, missing, Flux2ModelReadiness::Ready] {
            assert_eq!(
                flux2_install_seed(false, true, readiness),
                Flux2InstallSeed::Seal,
                "a section the user has moved by hand must never be forced open"
            );
        }
    }

    /// Merging the two lists merged their hovers as well: the presence catalog showed the
    /// resolved path, the residency block explained what the component IS, and one label
    /// has one hover. Neither half may be dropped, and an unresolved path may not leave a
    /// dangling separator behind.
    #[test]
    fn a_merged_row_keeps_both_hovers_the_two_lists_had() {
        let both = flux2_component_row_tooltip(Flux2CatalogComponent::Vae, "/models/vae");
        assert_eq!(
            both,
            format!(
                "{}\n/models/vae",
                t!("cleaning.tools.flux2_klein.component_vae_hint")
            )
        );
        assert_eq!(
            flux2_component_row_tooltip(Flux2CatalogComponent::Scheduler, ""),
            t!("cleaning.tools.flux2_klein.component_scheduler_hint")
        );
    }

    #[test]
    fn every_residency_literal_survives_the_round_trip() {
        for residency in [
            Flux2Residency::NotLoaded,
            Flux2Residency::Ram,
            Flux2Residency::Gpu,
            Flux2Residency::Offloaded,
            Flux2Residency::Mixed,
        ] {
            assert_eq!(Flux2Residency::from_wire(residency.wire()), Some(residency));
        }
        // A state added by a newer backend is "not known" and is never coerced into one
        // of the five — `NotLoaded` in particular would claim the weights are gone.
        for unknown in ["", "vram", "not-loaded", "NotLoaded", "disk"] {
            assert_eq!(
                Flux2Residency::from_wire(unknown),
                None,
                "`{unknown}` must not be read as a known state"
            );
        }
    }

    #[test]
    fn only_the_two_states_that_need_explaining_are_amber_and_carry_a_hover() {
        // `ram` must not be amber: the text encoder can never be on the GPU, so RAM is its
        // ideal state rather than a compromise.
        assert_eq!(Flux2Residency::Ram.color(), Some(FLUX2_STATUS_OK_COLOR));
        assert_eq!(Flux2Residency::Gpu.color(), Some(FLUX2_STATUS_OK_COLOR));
        assert_eq!(Flux2Residency::NotLoaded.color(), None);
        for state in [Flux2Residency::Offloaded, Flux2Residency::Mixed] {
            assert_eq!(state.color(), Some(FLUX2_STATUS_WARN_COLOR));
            assert!(
                state.hint().is_some(),
                "{state:?} is not self-explanatory and must carry a hover"
            );
        }
        for state in [
            Flux2Residency::NotLoaded,
            Flux2Residency::Ram,
            Flux2Residency::Gpu,
        ] {
            assert!(state.hint().is_none(), "{state:?} explains itself");
        }
    }

    #[test]
    fn the_pipeline_pair_says_so_on_the_two_components_it_is_true_of() {
        // The transformer and the VAE load and unload TOGETHER — the model cache key
        // describes a whole pipeline — and the hover is where the user learns it.
        let pair = component_action_tooltip(
            Flux2ComponentId::Transformer,
            Flux2ComponentAction::Unload,
        );
        assert_eq!(
            pair,
            component_action_tooltip(Flux2ComponentId::Vae, Flux2ComponentAction::Unload)
        );
        assert_ne!(
            pair,
            component_action_tooltip(
                Flux2ComponentId::TextEncoder,
                Flux2ComponentAction::Unload
            ),
            "the encoder unloads alone and must not claim otherwise"
        );
        let pair_load =
            component_action_tooltip(Flux2ComponentId::Vae, Flux2ComponentAction::Load);
        assert_eq!(
            pair_load,
            component_action_tooltip(Flux2ComponentId::Transformer, Flux2ComponentAction::Load)
        );
        assert_ne!(
            pair_load,
            component_action_tooltip(Flux2ComponentId::TextEncoder, Flux2ComponentAction::Load)
        );
        // Moving and warming up act on one component, so the pair wording must not leak
        // into them even on the two components it is true of for load/unload.
        assert_eq!(
            component_action_tooltip(Flux2ComponentId::Vae, Flux2ComponentAction::ToRam),
            component_action_tooltip(Flux2ComponentId::TextEncoder, Flux2ComponentAction::ToRam)
        );
    }

    /// The readiness helper is the ONE answer to "is the model installed", and the whole
    /// point of it is the difference between "not known" and "not there": blocking on the
    /// first would leave «Обработать» dead on a slow backend, and never blocking on the
    /// second is what used to leak the backend's untranslated «Путь ... не найден» to the
    /// user in download mode, where the derived paths are never empty.
    #[test]
    fn model_readiness_separates_not_known_from_not_installed() {
        let manual = runnable_settings();

        // No answer yet: paths are filled in and nothing has contradicted them.
        assert_eq!(
            flux2_model_readiness(&manual, None, None),
            Flux2ModelReadiness::Unknown,
            "an outstanding `.status` is not evidence of a missing model"
        );

        // Everything present.
        assert_eq!(
            flux2_model_readiness(&manual, Some(&status_with_present(&FLUX2_ALL_COMPONENTS)), None),
            Flux2ModelReadiness::Ready
        );

        // One component gone: the verdict names it and nothing else, so the refusal can
        // send the user after the one file that is actually missing.
        let without_transformer: Vec<&str> = FLUX2_ALL_COMPONENTS
            .into_iter()
            .filter(|name| *name != "transformer")
            .collect();
        assert_eq!(
            flux2_model_readiness(&manual, Some(&status_with_present(&without_transformer)), None),
            Flux2ModelReadiness::Missing(Flux2MissingComponents {
                transformer: true,
                ..Flux2MissingComponents::default()
            })
        );

        // An empty required path is decided WITHOUT the backend: nothing can be found at a
        // path that was never given, so this must not wait for an answer that would only
        // repeat it.
        let manual_blank = Flux2KleinSettings {
            text_encoder_path: String::new(),
            transformer_path: String::new(),
            vae_path: String::new(),
            ..runnable_settings()
        };
        assert_eq!(
            flux2_model_readiness(&manual_blank, None, None),
            Flux2ModelReadiness::Missing(Flux2MissingComponents {
                text_encoder: true,
                transformer: true,
                vae: true,
                ..Flux2MissingComponents::default()
            })
        );
        // With the prompt cached the encoder is waived here exactly as it is in the gate.
        assert_eq!(
            flux2_model_readiness(&manual_blank, None, Some(true)),
            Flux2ModelReadiness::Missing(Flux2MissingComponents {
                transformer: true,
                vae: true,
                ..Flux2MissingComponents::default()
            })
        );

        // Download mode on a machine where nothing has been downloaded. This is the case
        // the emptiness check can NEVER see: `effective_paths` derives all three under the
        // models directory, so they are non-empty from the first frame.
        let download = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            ..runnable_settings()
        };
        assert!(
            !download.effective_paths().transformer.is_empty(),
            "the derived paths are never empty — that is why the catalog is consulted"
        );
        assert_eq!(
            flux2_model_readiness(&download, Some(&status_with_present(&[])), None),
            Flux2ModelReadiness::Missing(Flux2MissingComponents {
                text_encoder: true,
                transformer: true,
                vae: true,
                tokenizer: true,
                scheduler: true,
            })
        );
        assert_eq!(
            flux2_model_readiness(&download, None, None),
            Flux2ModelReadiness::Unknown,
            "before the first answer the download mode must not block the button either"
        );
    }

    /// The two component blocks the panel used to draw described the same five parts twice.
    /// Merging them may not lose either half, and it may not invent the half the backend
    /// did not send.
    #[test]
    fn the_merged_list_joins_presence_and_residency_without_inventing_either() {
        let status = parse_flux2_status(&json!({
            "available": true,
            "components": {
                "text_encoder": {
                    "exists": true,
                    "size_bytes": 1024,
                    "path": "/models/te",
                    "residency": "ram",
                    "actions": ["unload"],
                },
                // On disk, and genuinely SPLIT across devices — one of the two states that
                // exist precisely because they must not be rounded to another.
                "vae": { "exists": true, "residency": "mixed", "actions": ["unload", "warmup"] },
                // Not on disk, and the answer says nothing about where it is.
                "transformer": { "exists": false, "path": "/models/tr" },
                "tokenizer": { "found": true, "path": "/models/tok" },
                "scheduler": { "found": false },
            },
        }));
        let rows = flux2_component_rows(&status, status.components.components.as_deref());
        assert_eq!(rows.len(), 5, "every catalog component gets a row of its own");
        assert_eq!(
            rows.iter().map(|row| row.component).collect::<Vec<_>>(),
            Flux2CatalogComponent::all().to_vec(),
            "the display order is the catalog order and does not follow the answer"
        );

        // Present: presence, size and path from one half, residency and the backend's own
        // button list from the other — in ONE row.
        let encoder = &rows[0];
        assert!(encoder.present);
        assert_eq!(encoder.size_bytes, 1024);
        assert_eq!(encoder.path, "/models/te");
        let residency = encoder.residency.expect("the answer covered the encoder");
        assert_eq!(residency.residency, Some(Flux2Residency::Ram));
        assert_eq!(residency.actions, vec![Flux2ComponentAction::Unload]);

        // Split across devices, and the two offered actions survive in the backend's order.
        let vae = &rows[2];
        assert_eq!(
            vae.residency.and_then(|row| row.residency),
            Some(Flux2Residency::Mixed)
        );
        assert_eq!(
            vae.residency.map(|row| row.actions.as_slice()),
            Some([Flux2ComponentAction::Unload, Flux2ComponentAction::Warmup].as_slice())
        );

        // Absent on disk: the row keeps its presence half, and the residency half claims
        // no state and offers no button rather than guessing at one.
        let transformer = &rows[1];
        assert!(!transformer.present);
        assert_eq!(transformer.path, "/models/tr");
        let transformer_residency = transformer
            .residency
            .expect("the answer listed the transformer, if without a state");
        assert_eq!(transformer_residency.residency, None);
        assert!(transformer_residency.actions.is_empty());

        // The two components the residency answer never covers: they carry no weights, so
        // "no state" is the truth about them and not a gap.
        for row in &rows[3..] {
            assert!(
                row.residency.is_none(),
                "{:?} has no weights and must never claim a residency",
                row.component
            );
        }
        assert!(rows[3].present, "the tokenizer's presence still comes through");
        assert!(!rows[4].present);

        // Busy, or a backend that predates the block: the caller reports that once above
        // the list, and every row falls back to its presence half.
        let presence_only = flux2_component_rows(&status, None);
        assert!(presence_only.iter().all(|row| row.residency.is_none()));
        assert_eq!(
            presence_only
                .iter()
                .map(|row| row.present)
                .collect::<Vec<_>>(),
            vec![true, false, true, true, false],
            "dropping the residency half must not disturb the presence half"
        );
    }

    #[test]
    fn an_unrecognised_residency_claims_no_state_and_keeps_the_literal() {
        let status = parse_flux2_status(&json!({
            "components": { "vae": { "residency": "nvme", "actions": ["unload"] } },
        }));
        let rows = status
            .components
            .components
            .as_deref()
            .expect("the block was reported");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].residency, None, "an unknown literal is not a state");
        assert_eq!(
            rows[0].residency_wire, "nvme",
            "the literal is kept so the hover can still show what the backend said"
        );
        // The actions the backend listed are unaffected: it is the service that decides
        // them, and it did not stop being the authority because of one unknown state.
        assert_eq!(rows[0].actions, vec![Flux2ComponentAction::Unload]);
    }

    #[test]
    fn every_action_literal_survives_the_round_trip_and_an_unknown_one_is_dropped() {
        for action in [
            Flux2ComponentAction::Load,
            Flux2ComponentAction::Unload,
            Flux2ComponentAction::ToRam,
            Flux2ComponentAction::ToGpu,
            Flux2ComponentAction::Warmup,
        ] {
            assert_eq!(
                Flux2ComponentAction::from_wire(action.wire()),
                Some(action)
            );
        }
        assert_eq!(Flux2ComponentAction::from_wire("quantize"), None);

        // One unknown entry costs its own button and nothing else: the rest of the list
        // still renders, so a newer backend does not blank the row.
        let status = parse_flux2_status(&json!({
            "components": {
                "transformer": { "residency": "gpu", "actions": ["unload", "quantize", "to_ram"] },
            },
        }));
        let rows = status
            .components
            .components
            .as_deref()
            .expect("the block was reported");
        assert_eq!(
            rows[0].actions,
            vec![Flux2ComponentAction::Unload, Flux2ComponentAction::ToRam]
        );
    }

    #[test]
    fn an_absent_components_block_is_not_known_and_never_not_loaded() {
        // A backend that predates the block, or one that could not take its lock.
        let silent = parse_flux2_status(&json!({ "available": true }));
        assert_eq!(
            silent.components.components, None,
            "an absent field is not an empty catalog and not a set of unloaded components"
        );
        assert!(!silent.components.components_busy);
        assert_eq!(
            flux2_component_block(Some(&silent)),
            Flux2ComponentBlock::Unknown
        );
        // No answer at all draws nothing: the catalog above already says the state is
        // not known, and a second line saying it would be noise.
        assert_eq!(flux2_component_block(None), Flux2ComponentBlock::Hidden);
        // A `components` that is not an object is refused rather than half-read.
        let broken = parse_flux2_status(&json!({ "components": "later" }));
        assert_eq!(broken.components.components, None);
    }

    #[test]
    fn a_busy_answer_reports_that_instead_of_stale_rows_and_offers_no_buttons() {
        let busy = parse_flux2_status(&json!({
            "components_busy": true,
            // Even if rows travelled with it, they describe a moment that has passed.
            "components": { "vae": { "residency": "gpu", "actions": ["unload"] } },
        }));
        assert!(busy.components.components_busy);
        assert_eq!(
            flux2_component_block(Some(&busy)),
            Flux2ComponentBlock::Busy,
            "busy wins over rows: the residencies could not be read"
        );
    }

    #[test]
    fn the_button_set_is_exactly_what_the_backend_listed() {
        let status = status_with_components();
        let rows = status
            .components
            .components
            .as_deref()
            .expect("the block was reported");
        let listed: Vec<(Flux2ComponentId, Vec<Flux2ComponentAction>)> = rows
            .iter()
            .map(|row| (row.id, row.actions.clone()))
            .collect();
        // Exactly the payload of the pinned contract, in `Flux2ComponentId::all` order and
        // with no action added by this side: the service is the authority on the matrix.
        assert_eq!(
            listed,
            vec![
                (
                    Flux2ComponentId::TextEncoder,
                    vec![Flux2ComponentAction::Unload]
                ),
                (
                    Flux2ComponentId::Transformer,
                    vec![Flux2ComponentAction::Unload, Flux2ComponentAction::ToRam]
                ),
                (
                    Flux2ComponentId::Vae,
                    vec![Flux2ComponentAction::Unload, Flux2ComponentAction::Warmup]
                ),
            ]
        );
        assert_eq!(
            rows.iter().map(|row| row.residency).collect::<Vec<_>>(),
            vec![
                Some(Flux2Residency::Ram),
                Some(Flux2Residency::Gpu),
                Some(Flux2Residency::Offloaded)
            ]
        );
        // A component the backend did not mention gets no row rather than an invented one.
        let partial = parse_flux2_status(&json!({
            "components": { "vae": { "residency": "not_loaded", "actions": [] } },
        }));
        let partial_rows = partial
            .components
            .components
            .as_deref()
            .expect("the block was reported");
        assert_eq!(partial_rows.len(), 1);
        assert_eq!(partial_rows[0].id, Flux2ComponentId::Vae);
        assert!(partial_rows[0].actions.is_empty());
    }
}
