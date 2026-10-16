/*
File: hybrid_editor/state.rs

Purpose:
The hybrid editor's state: the DRAFT recipe the user is editing (source rows, default
missing-glyph policy, blend algorithm and transfer base, separate property weights), the
preview text, the live preview engine and the full-face commit.
Everything the editor decides that does not need a `Ui` lives here, so it is testable
without a frame.

Key structures:
- `HybridEditorState`: one editor instance (one per host: the typing dock panel, the
  settings create dialog, the settings properties window).
- `DraftSource`: one source row (identity or an empty slot, weight, the three separate
  property weights, own policy override).
- `BlendAlgorithmChoice`: the algorithm combo's options (the transfer base is a separate field).
- `DraftError`: why the draft is not a valid recipe yet.
- `CommittedRecipe`: the result of a finished commit (recipe + its compiled full face).

Key functions:
- `HybridEditorState::new_draft` / `from_recipe` / `set_preview_text`.
- `HybridEditorState::draft_recipe` (validated through `ms_custom_fonts` with the typing
  model's identity normalization), `is_dirty`.
- `HybridEditorState::start_commit` / `cancel_commit` / `is_committing`, `poll_commit`.
- `HybridEditorState::set_split_weights` (seeds the property weights), `remove_row` (keeps
  the transfer base pointing at the same font), `blend_algorithm`.
- `weight_shares`, `override_index` / `override_from_index`, `algorithm_index` /
  `algorithm_from_index`, `cycle_index`, `skeleton_fallback_counts` (pure helpers).

Notes:
- Nothing is persisted here: a commit only produces the recipe and its face; the host
  decides what Save/Apply means (layer recipe, saved font).
- Dropping an editor cancels its running commit (`Drop`), whichever host drops it.
- Every draft or preview-text edit bumps `revision`; the preview engine re-renders from it
  (latest-wins, decision D5). A commit compiles the FULL face through
  `custom_fonts::service().start_full_compile` (progress + cancel, never on the GUI thread).
*/

use std::sync::Arc;

use eframe::egui;
use ms_custom_fonts::{
    BlendAlgorithm, BlendStats, CustomFontKind, CustomFontRecipe, HybridParams, HybridSource, MAX_HYBRID_SOURCES,
    MIN_HYBRID_SOURCES, MissingGlyphPolicy, PropertyWeights, RecipeError,
};
use ms_log::runtime_log;

use super::catalog::SourceCatalog;
use super::preview::PreviewEngine;
use crate::custom_fonts::service::{CompileTicket, CompiledFace, CustomFontError, service};
use crate::panel::fonts::normalize_font_identity;

/// Weight of a newly added source row. With [`WEIGHT_WHEEL_STEP`] = 1 one wheel notch moves
/// a two-source draft by about one percent.
pub(crate) const DEFAULT_SOURCE_WEIGHT: f32 = 50.0;
/// Smallest weight the weight field accepts (the recipe needs `> 0`).
pub(crate) const MIN_SOURCE_WEIGHT: f32 = 0.1;
/// Largest weight the weight field accepts.
pub(crate) const MAX_SOURCE_WEIGHT: f32 = 100.0;
/// Weight change of one wheel notch on the weight field.
pub(crate) const WEIGHT_WHEEL_STEP: f32 = 1.0;
/// Longest preview text, in characters (longer input is cut): keeps one subset compile and
/// one render short.
pub(crate) const PREVIEW_TEXT_MAX_CHARS: usize = 200;
/// The hybrid default policy of a new draft: a character one source lacks is built from the
/// others, so a fresh hybrid has no holes until the user asks for them.
pub(crate) const DEFAULT_MISSING_POLICY: MissingGlyphPolicy = MissingGlyphPolicy::NoInfluence;

/// The per-source override choices, in the order the override combo lists them
/// («как у шрифта / отключать символ / без влияния»).
pub(crate) const OVERRIDE_CHOICES: [Option<MissingGlyphPolicy>; 3] =
    [None, Some(MissingGlyphPolicy::Disable), Some(MissingGlyphPolicy::NoInfluence)];
/// The hybrid default policy choices, in the order the default combo lists them.
pub(crate) const DEFAULT_POLICY_CHOICES: [MissingGlyphPolicy; 2] = [MissingGlyphPolicy::Disable, MissingGlyphPolicy::NoInfluence];

/// The blend-algorithm choices, in the order the algorithm combo lists them.
pub(crate) const ALGORITHM_CHOICES: [BlendAlgorithmChoice; 5] = [
    BlendAlgorithmChoice::Auto,
    BlendAlgorithmChoice::Sdf,
    BlendAlgorithmChoice::NoBlend,
    BlendAlgorithmChoice::Skeleton,
    BlendAlgorithmChoice::Transfer,
];

/// One option of the algorithm combo: `ms_custom_fonts::BlendAlgorithm` without the transfer
/// base index, which the editor keeps in its own field (`HybridEditorState::base_source`) so
/// the user's base survives switching the algorithm away and back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum BlendAlgorithmChoice {
    /// `BlendAlgorithm::Auto` (the baseline algorithm).
    #[default]
    Auto,
    /// `BlendAlgorithm::Sdf`.
    Sdf,
    /// `BlendAlgorithm::NoBlend`.
    NoBlend,
    /// `BlendAlgorithm::Skeleton`.
    Skeleton,
    /// `BlendAlgorithm::Transfer` (base from `HybridEditorState::base_source`).
    Transfer,
}

impl BlendAlgorithmChoice {
    /// The choice of a recipe's algorithm (the transfer base is read separately).
    #[must_use]
    pub(crate) fn of(algorithm: BlendAlgorithm) -> Self {
        match algorithm {
            BlendAlgorithm::Auto => Self::Auto,
            BlendAlgorithm::Sdf => Self::Sdf,
            BlendAlgorithm::NoBlend => Self::NoBlend,
            BlendAlgorithm::Skeleton => Self::Skeleton,
            BlendAlgorithm::Transfer { .. } => Self::Transfer,
        }
    }

    /// The recipe algorithm for this choice; `base_source` is used only by `Transfer`.
    #[must_use]
    pub(crate) fn to_algorithm(self, base_source: usize) -> BlendAlgorithm {
        match self {
            Self::Auto => BlendAlgorithm::Auto,
            Self::Sdf => BlendAlgorithm::Sdf,
            Self::NoBlend => BlendAlgorithm::NoBlend,
            Self::Skeleton => BlendAlgorithm::Skeleton,
            Self::Transfer => BlendAlgorithm::Transfer { base_source },
        }
    }
}

/// One source row of the draft.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DraftSource {
    /// The chosen font's identity, or `None` for a slot the user has not filled yet.
    pub(crate) identity: Option<String>,
    /// Relative weight (normalized by the sum over the rows); the only weight while
    /// separate weights are off.
    pub(crate) weight: f32,
    /// Glyph-shape weight, used only while separate weights are on (seeded from `weight`
    /// the first time they turn on for this row); also written as the recipe's `weight`
    /// while they are on.
    pub(crate) shape: f32,
    /// Spacing (advances, vertical metrics) weight, used only while separate weights are on.
    pub(crate) spacing: f32,
    /// Kerning weight, used only while separate weights are on.
    pub(crate) kerning: f32,
    /// Whether `shape` / `spacing` / `kerning` hold split values of this editor session
    /// (seeded once, or read from a split recipe). Turning the toggle on seeds only rows
    /// without them, so off-then-on restores the user's split values.
    pub(crate) has_split_values: bool,
    /// This source's own missing-glyph policy; `None` follows the hybrid default.
    pub(crate) missing_override: Option<MissingGlyphPolicy>,
}

impl DraftSource {
    /// An unfilled slot with the default weight (all four weights equal).
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self {
            identity: None,
            weight: DEFAULT_SOURCE_WEIGHT,
            shape: DEFAULT_SOURCE_WEIGHT,
            spacing: DEFAULT_SOURCE_WEIGHT,
            kerning: DEFAULT_SOURCE_WEIGHT,
            has_split_values: false,
            missing_override: None,
        }
    }

    /// Copies `weight` into the three property weights unless the row already holds split
    /// values (the split toggle turning on, or a row added while it is on).
    fn seed_property_weights(&mut self) {
        if self.has_split_values {
            return;
        }
        self.shape = self.weight;
        self.spacing = self.weight;
        self.kerning = self.weight;
        self.has_split_values = true;
    }
}

/// Why the draft is not a usable recipe yet.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DraftError {
    /// Fewer source rows than a hybrid needs.
    #[error("a hybrid font needs at least {min} sources, the draft has {count}")]
    TooFewSources {
        /// Rows in the draft.
        count: usize,
        /// Required minimum.
        min: usize,
    },
    /// The row at this zero-based position has no font chosen.
    #[error("source slot #{0} has no font chosen")]
    EmptySlot(usize),
    /// This font is chosen in more than one row (normalized identity comparison).
    #[error("font '{0}' is chosen more than once")]
    Duplicate(String),
    /// Any other recipe invariant (a weight that is not finite and positive, too many rows).
    #[error(transparent)]
    Invalid(RecipeError),
}

/// A finished commit: the recipe and the compiled full face it produced (kept resident
/// while the host holds this value). The host persists the recipe as it sees fit.
#[derive(Clone)]
pub struct CommittedRecipe {
    /// The committed recipe.
    pub recipe: Arc<CustomFontRecipe>,
    /// Its compiled full face (already in the compile service's caches).
    face: Arc<CompiledFace>,
}

impl std::fmt::Debug for CommittedRecipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommittedRecipe").field("face", &self.face).finish_non_exhaustive()
    }
}

impl CommittedRecipe {
    /// PostScript name of the compiled full face (`MSCustom-<8 hex>`).
    #[must_use]
    pub fn post_script_name(&self) -> &str {
        &self.face.post_script_name
    }
}

/// Where the full-face commit stands.
#[derive(Debug, Default)]
enum CommitStatus {
    /// No commit running and nothing to report.
    #[default]
    Idle,
    /// The full face of `recipe` is being produced.
    Running {
        /// The service ticket (progress, cancel, result).
        ticket: CompileTicket,
        /// The recipe being built.
        recipe: Arc<CustomFontRecipe>,
    },
    /// The user cancelled the last commit.
    Cancelled,
    /// The last commit failed (logged with context).
    Failed,
}

/// What one frame of the commit reported.
#[derive(Debug)]
pub(crate) enum CommitPoll {
    /// Nothing new (idle, or still running).
    Nothing,
    /// The full face is ready.
    Committed(CommittedRecipe),
    /// The commit failed this frame (already logged).
    Failed,
}

/// One hybrid editor: the draft, the preview text, the preview engine and the commit.
///
/// Created by the host (`new_draft` / `from_recipe`) and drawn every frame with
/// `draw_hybrid_editor`. Dropping it stops its preview worker and CANCELS a running commit:
/// a commit's result reaches the host only through this editor, so a closed editor (a dock
/// closed or rebound to another layer, a settings window closed) must not keep a full-face
/// compile of tens of seconds running for nobody.
#[derive(Debug)]
pub struct HybridEditorState {
    /// Base id of every widget of this editor (one editor per id in a frame).
    pub(crate) id: egui::Id,
    /// The source rows, `MIN_HYBRID_SOURCES..=MAX_HYBRID_SOURCES` of them.
    pub(crate) rows: Vec<DraftSource>,
    /// The hybrid's default missing-glyph policy.
    pub(crate) missing_default: MissingGlyphPolicy,
    /// The chosen blend algorithm.
    pub(crate) algorithm: BlendAlgorithmChoice,
    /// Row index of the transfer base font (used only by `Transfer`; clamped to the rows
    /// when the recipe is built).
    pub(crate) base_source: usize,
    /// Whether every row carries separate shape / spacing / kerning weights.
    pub(crate) split_weights: bool,
    /// The preview sample text (at most `PREVIEW_TEXT_MAX_CHARS` characters).
    pub(crate) preview_text: String,
    /// Bumped on every draft or preview-text edit; the preview re-renders from it.
    pub(crate) revision: u64,
    /// The recipe of the last successful commit (or the one the editor was opened with).
    last_committed: Option<Arc<CustomFontRecipe>>,
    /// The full-face commit.
    commit: CommitStatus,
    /// Live preview + coverage worker.
    pub(crate) preview: PreviewEngine,
}

impl HybridEditorState {
    /// A new, empty draft: two unfilled source rows, the default policy, the localized
    /// default sample text. `id` must be unique among editors drawn in one frame.
    #[must_use]
    pub fn new_draft(id: egui::Id) -> Self {
        Self {
            id,
            rows: vec![DraftSource::empty(); MIN_HYBRID_SOURCES],
            missing_default: DEFAULT_MISSING_POLICY,
            algorithm: BlendAlgorithmChoice::Auto,
            base_source: 0,
            split_weights: false,
            preview_text: t!("typing.hybrid_font.preview_default_text").to_owned(),
            revision: 0,
            last_committed: None,
            commit: CommitStatus::Idle,
            preview: PreviewEngine::default(),
        }
    }

    /// An editor opened on an existing recipe (a saved font, a layer's ad-hoc recipe); the
    /// recipe counts as committed, so the editor starts clean (`is_dirty` is `false`).
    #[must_use]
    pub fn from_recipe(recipe: &CustomFontRecipe, id: egui::Id) -> Self {
        let mut state = Self::new_draft(id);
        match &recipe.kind {
            CustomFontKind::Hybrid(params) => {
                let split = params.property_weights.as_deref();
                state.rows = params
                    .sources
                    .iter()
                    .enumerate()
                    .map(|(index, source)| {
                        let weights = split.and_then(|weights| weights.get(index)).copied().unwrap_or(PropertyWeights {
                            shape: source.weight,
                            spacing: source.weight,
                            kerning: source.weight,
                        });
                        DraftSource {
                            identity: Some(source.identity.clone()),
                            weight: source.weight,
                            shape: weights.shape,
                            spacing: weights.spacing,
                            kerning: weights.kerning,
                            has_split_values: split.is_some(),
                            missing_override: source.missing_override,
                        }
                    })
                    .collect();
                state.missing_default = params.missing_default;
                state.split_weights = split.is_some();
                state.algorithm = BlendAlgorithmChoice::of(params.algorithm);
                state.base_source = match params.algorithm {
                    BlendAlgorithm::Transfer { base_source } => base_source,
                    BlendAlgorithm::Auto | BlendAlgorithm::Sdf | BlendAlgorithm::NoBlend | BlendAlgorithm::Skeleton => 0,
                };
            }
        }
        state.last_committed = Some(Arc::new(recipe.clone()));
        state
    }

    /// Replaces the preview sample text (the typing dock prefills it from the layer text);
    /// cut to `PREVIEW_TEXT_MAX_CHARS` characters. Re-renders the preview when it changed.
    pub fn set_preview_text(&mut self, text: &str) {
        let text = clamp_preview_text(text);
        if text != self.preview_text {
            self.preview_text = text;
            self.touch();
        }
    }

    /// Marks the draft or the preview text as edited (the preview re-renders).
    pub(crate) fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Turns separate property weights on or off. Turning them on seeds the shape, spacing
    /// and kerning weights from `weight` only for rows that never held split values in this
    /// editor (a fresh row, a baseline recipe), so the first split draft blends exactly like
    /// the unsplit one; rows that had them get their values back (off-then-on restores).
    /// Turning them off keeps the values; the recipe ignores them. Bumps the revision on a
    /// change.
    pub(crate) fn set_split_weights(&mut self, on: bool) {
        if on == self.split_weights {
            return;
        }
        if on {
            self.rows.iter_mut().for_each(DraftSource::seed_property_weights);
        }
        self.split_weights = on;
        self.touch();
    }

    /// Appends an unfilled source row (the caller checks the row limits); while separate
    /// weights are on, its property weights are seeded from its weight at once. Bumps the
    /// revision.
    pub(crate) fn add_row(&mut self) {
        let mut row = DraftSource::empty();
        if self.split_weights {
            row.seed_property_weights();
        }
        self.rows.push(row);
        self.touch();
    }

    /// Removes the source row at `index` (no-op when out of range; the caller checks the
    /// row limits). The transfer base keeps pointing at the same font: a removed row before
    /// it shifts the index down, removing the base row itself resets the base to the first
    /// row. Bumps the revision on a change.
    pub(crate) fn remove_row(&mut self, index: usize) {
        if index >= self.rows.len() {
            return;
        }
        self.rows.remove(index);
        self.base_source = match self.base_source.cmp(&index) {
            std::cmp::Ordering::Less => self.base_source,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => self.base_source - 1,
        };
        self.touch();
    }

    /// The transfer base row clamped to the rows (an index past the end is the last row).
    #[must_use]
    pub(crate) fn clamped_base_source(&self) -> usize {
        self.base_source.min(self.rows.len().saturating_sub(1))
    }

    /// The recipe algorithm of the current choice (transfer base clamped to the rows).
    #[must_use]
    pub(crate) fn blend_algorithm(&self) -> BlendAlgorithm {
        self.algorithm.to_algorithm(self.clamped_base_source())
    }

    /// The draft as a recipe, validated with the typing model's identity normalization.
    ///
    /// # Errors
    /// `TooFewSources`, `EmptySlot` (first unfilled row), `Duplicate` (a font chosen twice,
    /// compared normalized), `Invalid` (any other recipe invariant).
    pub fn draft_recipe(&self) -> Result<CustomFontRecipe, DraftError> {
        if self.rows.len() < MIN_HYBRID_SOURCES {
            return Err(DraftError::TooFewSources { count: self.rows.len(), min: MIN_HYBRID_SOURCES });
        }
        let sources = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                row.identity
                    .as_ref()
                    .filter(|identity| !identity.trim().is_empty())
                    .map(|identity| HybridSource {
                        identity: identity.clone(),
                        // While split, the main weight is hidden: write the shape weight so
                        // an invisible value neither changes the key nor forces a recompile.
                        weight: if self.split_weights { row.shape } else { row.weight },
                        missing_override: row.missing_override,
                    })
                    .ok_or(DraftError::EmptySlot(index))
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Property weights are a recipe fact only while the split toggle is on; off, the
        // recipe is baseline-shaped again (`hybrid` kind when the algorithm is Auto).
        let property_weights = self.split_weights.then(|| {
            self.rows.iter().map(|row| PropertyWeights { shape: row.shape, spacing: row.spacing, kerning: row.kerning }).collect()
        });
        let recipe = CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources,
                missing_default: self.missing_default,
                algorithm: self.blend_algorithm(),
                property_weights,
            }),
        };
        recipe.validate(&normalize_font_identity).map_err(|err| match err {
            RecipeError::DuplicateSource { identity } => DraftError::Duplicate(identity),
            other => DraftError::Invalid(other),
        })?;
        Ok(recipe)
    }

    /// Whether the draft differs from the last committed recipe (or nothing was committed
    /// and the draft is valid).
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.draft_recipe().ok().as_ref() != self.last_committed.as_deref()
    }

    /// Starts building the draft's full face (sources resolved through `catalog`) on a
    /// worker; `draw_hybrid_editor` then shows progress + cancel and reports
    /// `HybridEditorOutcome::Committed` when it is ready. A commit already running is
    /// cancelled first. GUI-safe.
    ///
    /// # Errors
    /// The draft is not a valid recipe (nothing is started).
    pub fn start_commit(&mut self, catalog: &SourceCatalog) -> Result<(), DraftError> {
        let recipe = Arc::new(self.draft_recipe()?);
        self.cancel_commit();
        let ticket = service().start_full_compile(Arc::clone(&recipe), Arc::clone(catalog.resolver()));
        self.commit = CommitStatus::Running { ticket, recipe };
        Ok(())
    }

    /// Whether a commit is running.
    #[must_use]
    pub fn is_committing(&self) -> bool {
        matches!(self.commit, CommitStatus::Running { .. })
    }

    /// Cancels a running commit (no-op otherwise); the editor then shows "cancelled".
    pub fn cancel_commit(&mut self) {
        if let CommitStatus::Running { ticket, .. } = &self.commit {
            ticket.cancel();
            self.commit = CommitStatus::Cancelled;
        }
    }

    /// `(glyphs done, glyphs total)` of the running commit, once the compiler reported.
    #[must_use]
    pub(crate) fn commit_progress(&self) -> Option<(usize, usize)> {
        match &self.commit {
            CommitStatus::Running { ticket, .. } => ticket.progress(),
            CommitStatus::Idle | CommitStatus::Cancelled | CommitStatus::Failed => None,
        }
    }

    /// Whether the last commit was cancelled / failed (for the status line).
    #[must_use]
    pub(crate) fn commit_cancelled(&self) -> bool {
        matches!(self.commit, CommitStatus::Cancelled)
    }

    /// Whether the last commit failed.
    #[must_use]
    pub(crate) fn commit_failed(&self) -> bool {
        matches!(self.commit, CommitStatus::Failed)
    }

    /// Takes the commit's result when it arrived (GUI thread, once per frame). A failure is
    /// logged here with the recipe and the error (global §7).
    pub(crate) fn poll_commit(&mut self) -> CommitPoll {
        let CommitStatus::Running { ticket, recipe } = &self.commit else {
            return CommitPoll::Nothing;
        };
        let Some(result) = ticket.try_result() else {
            return CommitPoll::Nothing;
        };
        match result {
            Ok(face) => {
                let recipe = Arc::clone(recipe);
                runtime_log::log_info(format!(
                    "typing: hybrid font built: {} ({} bytes, recipe {})",
                    face.post_script_name,
                    face.byte_len,
                    recipe.to_json()
                ));
                self.last_committed = Some(Arc::clone(&recipe));
                self.commit = CommitStatus::Idle;
                CommitPoll::Committed(CommittedRecipe { recipe, face })
            }
            Err(CustomFontError::Cancelled) => {
                self.commit = CommitStatus::Cancelled;
                CommitPoll::Nothing
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "typing: the hybrid font could not be built: {err}. Recipe: {}. Possible cause: a source \
                     font is missing or unreadable, or a glyph could not be converted.",
                    recipe.to_json()
                ));
                self.commit = CommitStatus::Failed;
                CommitPoll::Failed
            }
        }
    }
}

impl Drop for HybridEditorState {
    /// Cancels a running commit (no-op otherwise): its result could only reach the host
    /// through this editor. Other tickets or blocking callers on the same recipe keep the
    /// compile alive (`CompileTicket::cancel`).
    fn drop(&mut self) {
        self.cancel_commit();
    }
}

/// `text` cut to [`PREVIEW_TEXT_MAX_CHARS`] characters.
#[must_use]
pub(crate) fn clamp_preview_text(text: &str) -> String {
    text.chars().take(PREVIEW_TEXT_MAX_CHARS).collect()
}

/// Each weight's share of the total in percent; `None` for a weight that is not finite and
/// positive (it takes no share) and for every row when no weight is valid. Takes `f64` so the
/// recipe's `HybridParams::effective_weights` feeds it directly (the editor's `f32` rows
/// widen losslessly).
#[must_use]
pub(crate) fn weight_shares(weights: &[f64]) -> Vec<Option<f64>> {
    let valid = |weight: f64| weight.is_finite() && weight > 0.0;
    let total: f64 = weights.iter().copied().filter(|&weight| valid(weight)).sum();
    weights
        .iter()
        .map(|&weight| (valid(weight) && total > 0.0).then(|| weight / total * 100.0))
        .collect()
}

/// Position of `choice` in [`OVERRIDE_CHOICES`].
#[must_use]
pub(crate) fn override_index(choice: Option<MissingGlyphPolicy>) -> usize {
    match choice {
        None => 0,
        Some(MissingGlyphPolicy::Disable) => 1,
        Some(MissingGlyphPolicy::NoInfluence) => 2,
    }
}

/// The override at `index` of [`OVERRIDE_CHOICES`]; an out-of-range index follows the
/// default (`None`).
#[must_use]
pub(crate) fn override_from_index(index: usize) -> Option<MissingGlyphPolicy> {
    OVERRIDE_CHOICES.get(index).copied().flatten()
}

/// Position of `choice` in [`ALGORITHM_CHOICES`].
#[must_use]
pub(crate) fn algorithm_index(choice: BlendAlgorithmChoice) -> usize {
    match choice {
        BlendAlgorithmChoice::Auto => 0,
        BlendAlgorithmChoice::Sdf => 1,
        BlendAlgorithmChoice::NoBlend => 2,
        BlendAlgorithmChoice::Skeleton => 3,
        BlendAlgorithmChoice::Transfer => 4,
    }
}

/// The choice at `index` of [`ALGORITHM_CHOICES`]; an out-of-range index is `Auto`.
#[must_use]
pub(crate) fn algorithm_from_index(index: usize) -> BlendAlgorithmChoice {
    ALGORITHM_CHOICES.get(index).copied().unwrap_or_default()
}

/// `(skeleton fallbacks, glyphs the skeleton path attempted)` of a preview compile, or
/// `None` when nothing fell back. Attempted = exact interpolations + skeleton blends +
/// fallbacks: glyphs with ink in at most one source never reach the blend.
#[must_use]
pub(crate) fn skeleton_fallback_counts(stats: &BlendStats) -> Option<(usize, usize)> {
    (stats.skeleton_fallback > 0)
        .then(|| (stats.skeleton_fallback, stats.compatible + stats.skeleton + stats.skeleton_fallback))
}

/// Position of `policy` in [`DEFAULT_POLICY_CHOICES`].
#[must_use]
pub(crate) fn default_policy_index(policy: MissingGlyphPolicy) -> usize {
    match policy {
        MissingGlyphPolicy::Disable => 0,
        MissingGlyphPolicy::NoInfluence => 1,
    }
}

/// `index` moved by `steps` wheel notches over `len` choices, wrapping around. `len == 0`
/// gives 0.
#[must_use]
pub(crate) fn cycle_index(index: usize, len: usize, steps: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let Ok(len_i64) = i64::try_from(len) else { return 0 };
    let current = i64::try_from(index.min(len - 1)).unwrap_or(0);
    let moved = (current + i64::from(steps)).rem_euclid(len_i64);
    usize::try_from(moved).unwrap_or(0)
}

/// Whether one more source row may be added.
#[must_use]
pub(crate) fn can_add_row(rows: usize) -> bool {
    rows < MAX_HYBRID_SOURCES
}

/// Whether a source row may be removed.
#[must_use]
pub(crate) fn can_remove_row(rows: usize) -> bool {
    rows > MIN_HYBRID_SOURCES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(rows: &[(Option<&str>, f32)]) -> HybridEditorState {
        let mut state = HybridEditorState::new_draft(egui::Id::new("test-editor"));
        state.rows = rows
            .iter()
            .map(|&(identity, weight)| DraftSource { identity: identity.map(str::to_owned), weight, ..DraftSource::empty() })
            .collect();
        state
    }

    fn params(recipe: &CustomFontRecipe) -> &HybridParams {
        match &recipe.kind {
            CustomFontKind::Hybrid(params) => params,
        }
    }

    #[test]
    fn a_new_draft_has_two_empty_rows_and_is_not_a_recipe_yet() {
        let state = HybridEditorState::new_draft(egui::Id::new("draft"));
        assert_eq!(state.rows.len(), MIN_HYBRID_SOURCES);
        assert!(state.rows.iter().all(|row| row.identity.is_none() && row.missing_override.is_none()));
        assert_eq!(state.draft_recipe(), Err(DraftError::EmptySlot(0)));
        assert!(!state.is_dirty(), "an invalid draft with nothing committed has nothing to apply");
        assert!(!state.preview_text.is_empty());
    }

    #[test]
    fn draft_validation_counts_slots_duplicates_and_weights() {
        assert_eq!(editor(&[(Some("A"), 1.0)]).draft_recipe(), Err(DraftError::TooFewSources { count: 1, min: 2 }));
        assert_eq!(editor(&[(Some("A"), 1.0), (None, 1.0)]).draft_recipe(), Err(DraftError::EmptySlot(1)));
        assert_eq!(editor(&[(Some("A"), 1.0), (Some("  "), 1.0)]).draft_recipe(), Err(DraftError::EmptySlot(1)));
        // Duplicates are compared under the model's normalization (trim + ASCII lowercase).
        assert_eq!(
            editor(&[(Some("Font-A"), 1.0), (Some(" font-a "), 1.0)]).draft_recipe(),
            Err(DraftError::Duplicate(" font-a ".to_owned()))
        );
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(
                matches!(editor(&[(Some("A"), 1.0), (Some("B"), bad)]).draft_recipe(), Err(DraftError::Invalid(RecipeError::InvalidWeight { .. }))),
                "weight {bad}"
            );
        }
        let five: Vec<(Option<&str>, f32)> = ["A", "B", "C", "D", "E"].iter().map(|id| (Some(*id), 1.0)).collect();
        assert!(matches!(editor(&five).draft_recipe(), Err(DraftError::Invalid(RecipeError::TooManySources { .. }))));
        let four: Vec<(Option<&str>, f32)> = ["A", "B", "C", "D"].iter().map(|id| (Some(*id), 1.0)).collect();
        assert!(editor(&four).draft_recipe().is_ok());
    }

    #[test]
    fn a_recipe_round_trips_through_the_editor_and_starts_clean() {
        let recipe = CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: vec![
                    HybridSource { identity: "A".to_owned(), weight: 3.0, missing_override: Some(MissingGlyphPolicy::Disable) },
                    HybridSource { identity: "B".to_owned(), weight: 1.0, missing_override: None },
                ],
                missing_default: MissingGlyphPolicy::Disable,
                algorithm: ms_custom_fonts::BlendAlgorithm::Auto,
                property_weights: None,
            }),
        };
        let mut state = HybridEditorState::from_recipe(&recipe, egui::Id::new("edit"));
        assert_eq!(state.draft_recipe().as_ref(), Ok(&recipe));
        assert!(!state.is_dirty());
        state.rows[1].weight = 2.0;
        assert!(state.is_dirty());
        state.rows[1].weight = 1.0;
        assert!(!state.is_dirty());
        state.missing_default = MissingGlyphPolicy::NoInfluence;
        assert!(state.is_dirty());
    }

    #[test]
    fn algorithm_choices_map_both_ways() {
        for (index, choice) in ALGORITHM_CHOICES.iter().enumerate() {
            assert_eq!(algorithm_index(*choice), index);
            assert_eq!(algorithm_from_index(index), *choice);
            assert_eq!(BlendAlgorithmChoice::of(choice.to_algorithm(1)), *choice);
        }
        assert_eq!(algorithm_from_index(ALGORITHM_CHOICES.len()), BlendAlgorithmChoice::Auto);
        assert_eq!(BlendAlgorithmChoice::Transfer.to_algorithm(2), BlendAlgorithm::Transfer { base_source: 2 });
    }

    #[test]
    fn the_draft_kind_follows_the_algorithm_and_the_split_toggle() {
        let mut state = editor(&[(Some("A"), 3.0), (Some("B"), 1.0)]);
        let baseline = state.draft_recipe().expect("valid draft");
        assert_eq!(baseline.kind.tag(), ms_custom_fonts::KIND_HYBRID);
        assert_eq!(params(&baseline).algorithm, BlendAlgorithm::Auto);
        assert_eq!(params(&baseline).property_weights, None);

        state.algorithm = BlendAlgorithmChoice::Skeleton;
        let skeleton = state.draft_recipe().expect("valid draft");
        assert_eq!(skeleton.kind.tag(), ms_custom_fonts::KIND_HYBRID_V2);
        assert_eq!(params(&skeleton).algorithm, BlendAlgorithm::Skeleton);

        state.algorithm = BlendAlgorithmChoice::Auto;
        state.set_split_weights(true);
        let split = state.draft_recipe().expect("valid draft");
        assert_eq!(split.kind.tag(), ms_custom_fonts::KIND_HYBRID_V2);
        assert_eq!(
            params(&split).property_weights,
            Some(vec![
                PropertyWeights { shape: 3.0, spacing: 3.0, kerning: 3.0 },
                PropertyWeights { shape: 1.0, spacing: 1.0, kerning: 1.0 },
            ]),
            "turning the toggle on seeds every property from the row's weight"
        );

        state.rows[1].kerning = 7.0;
        state.rows[1].shape = 2.0;
        let edited = state.draft_recipe().expect("valid draft");
        let sources = &params(&edited).sources;
        assert!(
            (sources[0].weight - 3.0).abs() < f32::EPSILON && (sources[1].weight - 2.0).abs() < f32::EPSILON,
            "while split, the hidden main weight is written as the shape weight: {sources:?}"
        );
        state.set_split_weights(false);
        assert_eq!(state.draft_recipe().as_ref(), Ok(&baseline), "off again: the baseline recipe returns");
        state.rows[1].weight = 5.0;
        state.set_split_weights(true);
        assert_eq!(state.draft_recipe().as_ref(), Ok(&edited), "off-then-on restores the split values, ignoring the weight edit");

        // A row added while split is on is seeded from its own weight at once; a row added
        // while it is off is seeded when it turns on.
        state.add_row();
        assert!(state.rows[2].has_split_values);
        state.rows.pop();
        state.set_split_weights(false);
        state.add_row();
        state.rows[2].weight = 9.0;
        state.set_split_weights(true);
        assert!((state.rows[2].kerning - 9.0).abs() < f32::EPSILON, "a never-split row seeds from its weight");
        assert!((state.rows[1].kerning - 7.0).abs() < f32::EPSILON, "a split row keeps its values");
        state.rows.pop();

        // (A zero SHAPE weight is also the written main weight and fails as `InvalidWeight`.)
        state.rows[0].spacing = 0.0;
        assert!(matches!(state.draft_recipe(), Err(DraftError::Invalid(RecipeError::InvalidPropertyWeight { .. }))));
    }

    #[test]
    fn the_transfer_base_is_clamped_and_follows_row_removal() {
        let mut state = editor(&[(Some("A"), 1.0), (Some("B"), 1.0), (Some("C"), 1.0), (Some("D"), 1.0)]);
        state.algorithm = BlendAlgorithmChoice::Transfer;
        state.base_source = 2;
        assert_eq!(state.blend_algorithm(), BlendAlgorithm::Transfer { base_source: 2 });

        let revision = state.revision;
        state.remove_row(0);
        assert_ne!(state.revision, revision);
        assert_eq!(state.base_source, 1, "a row removed before the base keeps the same font as base");
        assert_eq!(state.rows[state.base_source].identity.as_deref(), Some("C"));
        state.remove_row(2);
        assert_eq!(state.base_source, 1, "a row removed after the base changes nothing");
        state.remove_row(1);
        assert_eq!(state.base_source, 0, "removing the base row resets it to the first row");
        state.remove_row(9);
        assert_eq!(state.rows.len(), 1, "an out-of-range removal is a no-op");

        let mut state = editor(&[(Some("A"), 1.0), (Some("B"), 1.0)]);
        state.algorithm = BlendAlgorithmChoice::Transfer;
        state.base_source = 5;
        assert_eq!(state.blend_algorithm(), BlendAlgorithm::Transfer { base_source: 1 }, "an index past the rows is the last row");
        assert!(state.draft_recipe().is_ok());
    }

    #[test]
    fn every_algorithm_round_trips_through_the_editor() {
        for algorithm in [
            BlendAlgorithm::Auto,
            BlendAlgorithm::Sdf,
            BlendAlgorithm::NoBlend,
            BlendAlgorithm::Skeleton,
            BlendAlgorithm::Transfer { base_source: 1 },
        ] {
            for property_weights in [
                None,
                Some(vec![
                    PropertyWeights { shape: 3.0, spacing: 0.5, kerning: 1.0 },
                    PropertyWeights { shape: 1.0, spacing: 4.0, kerning: 3.0 },
                ]),
            ] {
                // The editor writes `weight = shape` for split recipes, so the fixture's main
                // weights (3, 1) equal the shape weights for an exact round trip.
                let recipe = CustomFontRecipe {
                    kind: CustomFontKind::Hybrid(HybridParams {
                        sources: vec![
                            HybridSource { identity: "A".to_owned(), weight: 3.0, missing_override: None },
                            HybridSource { identity: "B".to_owned(), weight: 1.0, missing_override: Some(MissingGlyphPolicy::Disable) },
                        ],
                        missing_default: MissingGlyphPolicy::NoInfluence,
                        algorithm,
                        property_weights: property_weights.clone(),
                    }),
                };
                let state = HybridEditorState::from_recipe(&recipe, egui::Id::new("round-trip"));
                assert_eq!(state.split_weights, property_weights.is_some());
                assert_eq!(state.draft_recipe().as_ref(), Ok(&recipe), "{algorithm:?} / {property_weights:?}");
                assert!(!state.is_dirty());
            }
        }
    }

    #[test]
    fn skeleton_fallback_counts_only_report_attempted_glyphs() {
        let mut stats = BlendStats { copied: 10, compatible: 3, skeleton: 5, ..BlendStats::default() };
        assert_eq!(skeleton_fallback_counts(&stats), None, "nothing fell back");
        stats.skeleton_fallback = 2;
        assert_eq!(skeleton_fallback_counts(&stats), Some((2, 10)), "copied glyphs never reach the blend");
    }

    #[test]
    fn weight_shares_are_normalized_percentages() {
        let shares = weight_shares(&[60.0, 20.0, 20.0]);
        let expected = [60.0_f64, 20.0, 20.0];
        for (share, expected) in shares.iter().zip(expected) {
            assert!(share.is_some_and(|value| (value - expected).abs() < 1e-4), "{shares:?}");
        }
        assert_eq!(weight_shares(&[1.0, 0.0]), vec![Some(100.0), None]);
        assert_eq!(weight_shares(&[f64::NAN, -1.0]), vec![None, None]);
        assert!(weight_shares(&[]).is_empty());
    }

    #[test]
    fn override_choices_map_both_ways() {
        for (index, choice) in OVERRIDE_CHOICES.iter().enumerate() {
            assert_eq!(override_index(*choice), index);
            assert_eq!(override_from_index(index), *choice);
        }
        assert_eq!(override_from_index(OVERRIDE_CHOICES.len()), None);
        for (index, policy) in DEFAULT_POLICY_CHOICES.iter().enumerate() {
            assert_eq!(default_policy_index(*policy), index);
        }
    }

    #[test]
    fn wheel_cycling_wraps_both_ways() {
        assert_eq!(cycle_index(0, 3, 1), 1);
        assert_eq!(cycle_index(2, 3, 1), 0);
        assert_eq!(cycle_index(0, 3, -1), 2);
        assert_eq!(cycle_index(1, 3, -7), 0);
        assert_eq!(cycle_index(9, 3, 0), 2, "an out-of-range index is clamped first");
        assert_eq!(cycle_index(0, 0, 5), 0);
    }

    #[test]
    fn row_limits_follow_d3() {
        assert!(!can_remove_row(2));
        assert!(can_remove_row(3));
        assert!(can_add_row(3));
        assert!(!can_add_row(4));
    }

    #[test]
    fn preview_text_is_capped_by_characters_and_bumps_the_revision() {
        let mut state = HybridEditorState::new_draft(egui::Id::new("text"));
        let before = state.revision;
        state.set_preview_text(&"ж".repeat(PREVIEW_TEXT_MAX_CHARS + 10));
        assert_eq!(state.preview_text.chars().count(), PREVIEW_TEXT_MAX_CHARS);
        assert_ne!(state.revision, before);
        let after = state.revision;
        state.set_preview_text(&"ж".repeat(PREVIEW_TEXT_MAX_CHARS));
        assert_eq!(state.revision, after, "an unchanged text does not re-render");
    }

    #[test]
    fn the_editor_state_can_move_to_a_worker() {
        fn assert_send<T: Send>() {}
        assert_send::<HybridEditorState>();
    }
}
