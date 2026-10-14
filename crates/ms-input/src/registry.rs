/*
File: crates/ms-input/src/registry.rs

Purpose:
The validated set of declared actions with their CURRENT bindings (defaults or the user's
override), the conflict report over them and a revision counter consumers cache labels by.

Key structures:
- `InputRegistry`: declarations + current bindings + conflicts + revision.
- `ActionView`: a read view of one action for the settings pane and the runtime.
- `OverrideEntry`: what one change means for the persisted section (write or delete the id).
- `DeclarationError`, `OverrideWarning`.

Key functions:
- `InputRegistry::new`, `apply_overrides`, `set_bindings`, `reset`, `resolve`.

Notes:
- Every mutation recomputes the conflict report and bumps the revision exactly once.
- Labels live in `label.rs`, the matcher in `matching.rs`; both read this registry.
*/

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

use crate::action::{ActionId, ActionKind, ActionMeta, ActionSpec, Declarations, FixedAction, TargetId, TargetSpec};
use crate::binding::{Binding, BindingError, Chord, MAX_ALTERNATIVES, MAX_COMBINATIONS_PER_BINDING, MAX_MODE_REFS_PER_CHORD, combination_count, resolve_chord};
use crate::button::Button;
use crate::conflicts::{self, Conflict, ConflictReport, Shadow};
use crate::context::ContextId;
use crate::persist::{self, DecodeError, HotkeyOverrides};

/// Why a set of declarations cannot form a registry. Always a programming error in the
/// declaring crate (golden tests pin every declaration set).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeclarationError {
    #[error("input action `{0}` is declared twice")]
    DuplicateAction(&'static str),
    #[error("gesture target `{0}` is declared twice")]
    DuplicateTarget(&'static str),
    #[error("gesture `{action}` names undeclared target `{target}`")]
    UnknownTarget { action: &'static str, target: &'static str },
    #[error("input action `{action}` accepts `{mode}`, which is not a declared mode")]
    UnknownAccept { action: &'static str, mode: &'static str },
    #[error("mode `{action}` beats `{mode}`, which is not a declared mode")]
    UnknownBeats { action: &'static str, mode: &'static str },
    #[error("mode `{0}` is on a `beats` cycle (or beats itself)")]
    BeatsCycle(&'static str),
    #[error("fixed input action `{0}` has no binding")]
    FixedWithoutBinding(&'static str),
    #[error("default binding of `{action}` is invalid: {source}")]
    InvalidDefault {
        action: &'static str,
        #[source]
        source: BindingError,
    },
}

/// A persisted override entry that was skipped (the action keeps its defaults).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OverrideWarning {
    #[error("override for unknown input action `{0}` skipped")]
    UnknownAction(String),
    #[error("override for fixed input action `{0}` skipped")]
    FixedAction(String),
    #[error("override for `{id}` skipped: alternative {index} does not decode: {source}")]
    Undecodable {
        id: String,
        index: usize,
        #[source]
        source: DecodeError,
    },
    #[error("override for `{id}` skipped: {source}")]
    Invalid {
        id: String,
        #[source]
        source: BindingError,
    },
}

/// The persisted consequence of one binding change: `alternatives: Some(..)` writes the id's
/// full encoded alternative list (`[]` = explicitly unbound), `None` deletes the id (defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct OverrideEntry {
    pub id: ActionId,
    pub alternatives: Option<Vec<Value>>,
}

/// A read view of one registered action.
#[derive(Debug, Clone, Copy)]
pub struct ActionView<'a> {
    pub meta: &'a ActionMeta,
    pub kind: ActionKind,
    /// `None` for a fixed action.
    pub spec: Option<&'a ActionSpec>,
    pub fixed: bool,
    /// The current bindings differ from the defaults.
    pub overridden: bool,
    pub bindings: &'a [Binding],
}

#[derive(Debug, Clone)]
enum Declared {
    Action(ActionSpec),
    Fixed(FixedAction, ActionKind),
}

#[derive(Debug, Clone)]
struct Entry {
    declared: Declared,
    bindings: Vec<Binding>,
}

impl Entry {
    fn meta(&self) -> &ActionMeta {
        match &self.declared {
            Declared::Action(spec) => spec.meta(),
            Declared::Fixed(fixed, _) => &fixed.meta,
        }
    }

    fn kind(&self) -> ActionKind {
        match &self.declared {
            Declared::Action(spec) => spec.kind(),
            Declared::Fixed(_, kind) => *kind,
        }
    }

    fn view(&self) -> ActionView<'_> {
        let (spec, fixed) = match &self.declared {
            Declared::Action(spec) => (Some(spec), false),
            Declared::Fixed(..) => (None, true),
        };
        ActionView {
            meta: self.meta(),
            kind: self.kind(),
            spec,
            fixed,
            overridden: self.bindings != self.meta().defaults,
            bindings: &self.bindings,
        }
    }
}

/// The validated declarations of one studio session with their current bindings.
#[derive(Debug, Clone)]
pub struct InputRegistry {
    entries: Vec<Entry>,
    index: HashMap<&'static str, usize>,
    targets: Vec<TargetSpec>,
    exclusive_contexts: Vec<Vec<ContextId>>,
    revision: u64,
    conflicts: ConflictReport,
}

impl InputRegistry {
    /// Validates `decls` and builds the registry with every action on its defaults.
    ///
    /// # Errors
    /// [`DeclarationError`] for a duplicate action or target id, a gesture naming an undeclared
    /// target, an `accepts` / `beats` entry that is not a declared mode, a `beats` self-edge or
    /// cycle, a fixed action without bindings, or a default alternative list that fails
    /// [`BindingError`] validation (limits included).
    pub fn new(decls: Declarations) -> Result<Self, DeclarationError> {
        let Declarations { actions, targets, fixed, exclusive_contexts } = decls;
        let mut entries = Vec::with_capacity(actions.len().saturating_add(fixed.len()));
        for spec in actions {
            let bindings = spec.meta().defaults.clone();
            entries.push(Entry { declared: Declared::Action(spec), bindings });
        }
        for action in fixed {
            let kind = action
                .meta
                .defaults
                .first()
                .map(Binding::kind)
                .ok_or(DeclarationError::FixedWithoutBinding(action.meta.id.0))?;
            let bindings = action.meta.defaults.clone();
            entries.push(Entry { declared: Declared::Fixed(action, kind), bindings });
        }
        let mut index = HashMap::with_capacity(entries.len());
        for (position, entry) in entries.iter().enumerate() {
            let id = entry.meta().id.0;
            if index.insert(id, position).is_some() {
                return Err(DeclarationError::DuplicateAction(id));
            }
        }
        let mut target_ids = BTreeSet::new();
        for target in &targets {
            if !target_ids.insert(target.id) {
                return Err(DeclarationError::DuplicateTarget(target.id.0));
            }
        }
        let registry = Self { entries, index, targets, exclusive_contexts, revision: 0, conflicts: ConflictReport::default() };
        registry.validate_declarations(&target_ids)?;
        let conflicts = conflicts::compute(&registry);
        Ok(Self { conflicts, ..registry })
    }

    fn validate_declarations(&self, targets: &BTreeSet<TargetId>) -> Result<(), DeclarationError> {
        let counts = |mode: ActionId| self.bindings(mode).len();
        for entry in &self.entries {
            let meta = entry.meta();
            let action = meta.id.0;
            self.validate_list(meta.id, entry.kind(), &meta.defaults, &counts)
                .map_err(|source| DeclarationError::InvalidDefault { action, source })?;
            let Declared::Action(spec) = &entry.declared else { continue };
            for accept in spec.accepts() {
                if self.kind(accept.mode.0) != Some(ActionKind::Mode) {
                    return Err(DeclarationError::UnknownAccept { action, mode: accept.mode.0 });
                }
            }
            match spec {
                ActionSpec::Mode(mode) => {
                    if let Some(beaten) = mode.beats.iter().find(|beaten| self.kind(beaten.0) != Some(ActionKind::Mode)) {
                        return Err(DeclarationError::UnknownBeats { action, mode: beaten.0 });
                    }
                }
                ActionSpec::Gesture(gesture) => {
                    if !targets.contains(&gesture.target) {
                        return Err(DeclarationError::UnknownTarget { action, target: gesture.target.0 });
                    }
                }
                ActionSpec::Command(_) | ActionSpec::Axis(_) => {}
            }
        }
        self.reject_beats_cycles()
    }

    /// `beats` must be a precedence order: a self-edge or a cycle would deactivate every mode on it
    /// whenever they are held together, turning a declaration mistake into silently dead input.
    fn reject_beats_cycles(&self) -> Result<(), DeclarationError> {
        let beats_of = |id: ActionId| -> &[ActionId] {
            match self.spec(id) {
                Some(ActionSpec::Mode(mode)) => &mode.beats,
                Some(ActionSpec::Command(_) | ActionSpec::Axis(_) | ActionSpec::Gesture(_)) | None => &[],
            }
        };
        // Iterative DFS with three colours: absent = unvisited, false = on the stack, true = done.
        let mut state: HashMap<ActionId, bool> = HashMap::new();
        for entry in &self.entries {
            let root = entry.meta().id;
            if state.contains_key(&root) || entry.kind() != ActionKind::Mode {
                continue;
            }
            let mut stack: Vec<(ActionId, usize)> = vec![(root, 0)];
            state.insert(root, false);
            while let Some((node, next)) = stack.last().copied() {
                let edges = beats_of(node);
                if let Some(&child) = edges.get(next) {
                    if let Some(top) = stack.last_mut() {
                        top.1 = next.saturating_add(1);
                    }
                    match state.get(&child) {
                        Some(false) => return Err(DeclarationError::BeatsCycle(child.0)),
                        Some(true) => {}
                        None => {
                            state.insert(child, false);
                            stack.push((child, 0));
                        }
                    }
                } else {
                    state.insert(node, true);
                    stack.pop();
                }
            }
        }
        Ok(())
    }

    /// Checks a whole alternative list for `action`: at most [`MAX_ALTERNATIVES`] entries, each
    /// valid under [`Self::validate_alternative`]. `mode_count` gives each mode's alternative count
    /// as it will be once the change applies.
    fn validate_list(&self, action: ActionId, kind: ActionKind, alternatives: &[Binding], mode_count: &dyn Fn(ActionId) -> usize) -> Result<(), BindingError> {
        if alternatives.len() > MAX_ALTERNATIVES {
            return Err(BindingError::TooManyAlternatives { action: action.0, count: alternatives.len() });
        }
        alternatives.iter().try_for_each(|binding| self.validate_alternative(action, kind, binding, mode_count))
    }

    /// Checks one alternative for action `action` of kind `expected`: matching variant, no
    /// empty sequence or command / mode chord, every mode reference names a declared Mode, at most
    /// [`MAX_MODE_REFS_PER_CHORD`] references per chord and [`MAX_COMBINATIONS_PER_BINDING`]
    /// resolved combinations (under `mode_count`), a gesture uses a pointer button, an axis has a
    /// finite positive sensitivity.
    fn validate_alternative(&self, action: ActionId, expected: ActionKind, binding: &Binding, mode_count: &dyn Fn(ActionId) -> usize) -> Result<(), BindingError> {
        let found = binding.kind();
        if found != expected {
            return Err(BindingError::KindMismatch { action: action.0, expected, found });
        }
        match binding {
            Binding::Command(command) => {
                if command.sequence.is_empty() {
                    return Err(BindingError::EmptySequence { action: action.0 });
                }
                if command.sequence.iter().any(Chord::is_empty) {
                    return Err(BindingError::EmptyChord { action: action.0 });
                }
            }
            Binding::Mode(chord) => {
                if chord.buttons.is_empty() {
                    return Err(BindingError::EmptyChord { action: action.0 });
                }
            }
            Binding::Axis(axis) => {
                if !axis.sensitivity.is_finite() || axis.sensitivity <= 0.0 {
                    return Err(BindingError::InvalidSensitivity { action: action.0 });
                }
            }
            Binding::Gesture(gesture) => {
                if !gesture.button.is_pointer_button() {
                    return Err(BindingError::InvalidGestureButton { action: action.0 });
                }
            }
        }
        for chord in binding.chords() {
            if chord.modes.len() > MAX_MODE_REFS_PER_CHORD {
                return Err(BindingError::TooManyModeRefs { action: action.0 });
            }
            for reference in &chord.modes {
                match self.kind(reference.0) {
                    Some(ActionKind::Mode) => {}
                    Some(ActionKind::Command | ActionKind::Axis | ActionKind::Gesture) => {
                        return Err(BindingError::RefNotAMode { action: action.0, reference: reference.0 });
                    }
                    None => return Err(BindingError::UnknownModeRef { action: action.0, reference: reference.0 }),
                }
            }
        }
        if combination_count(binding, mode_count) > MAX_COMBINATIONS_PER_BINDING {
            return Err(BindingError::TooManyCombinations { action: action.0 });
        }
        Ok(())
    }

    /// Checks that giving the modes in `mode_count` their new alternative counts keeps every
    /// current binding that references one of them within [`MAX_COMBINATIONS_PER_BINDING`].
    fn validate_dependents(&self, mode_count: &dyn Fn(ActionId) -> usize) -> Result<(), BindingError> {
        for entry in &self.entries {
            if entry.bindings.iter().any(|binding| combination_count(binding, mode_count) > MAX_COMBINATIONS_PER_BINDING) {
                return Err(BindingError::TooManyCombinations { action: entry.meta().id.0 });
            }
        }
        Ok(())
    }

    /// Applies persisted overrides: every named action gets the decoded alternative list. An
    /// entry for an unknown or fixed action, or one that does not decode, validate or stay within
    /// the limits (`binding.rs`), is skipped whole (the action keeps its defaults), logged and
    /// returned. Mode entries apply first (a mode entry that would push a current binding over the
    /// combination limit is skipped), then every other entry is validated against the new modes.
    /// Bumps the revision once.
    pub fn apply_overrides(&mut self, overrides: &HotkeyOverrides) -> Vec<OverrideWarning> {
        let mut warnings = Vec::new();
        let mut accepted: Vec<(usize, Vec<Binding>)> = Vec::new();
        let mut new_counts: HashMap<&'static str, usize> = HashMap::new();
        let is_mode = |registry: &Self, id: &str| registry.kind(id) == Some(ActionKind::Mode);
        for mode_pass in [true, false] {
            for (id, raw_alternatives) in overrides.actions.iter().filter(|(id, _)| is_mode(self, id) == mode_pass) {
                let counts = |mode: ActionId| new_counts.get(mode.0).copied().unwrap_or_else(|| self.bindings(mode).len());
                let outcome = self.decode_override(id, raw_alternatives, &counts).and_then(|(position, bindings)| {
                    if !mode_pass {
                        return Ok((position, bindings));
                    }
                    let mode = self.entries.get(position).map(|entry| entry.meta().id);
                    let with_this = |other: ActionId| if Some(other) == mode { bindings.len() } else { counts(other) };
                    self.validate_dependents(&with_this)
                        .map(|()| (position, bindings))
                        .map_err(|source| OverrideWarning::Invalid { id: id.clone(), source })
                });
                match outcome {
                    Ok((position, bindings)) => {
                        if let Some(entry) = self.entries.get(position) {
                            new_counts.insert(entry.meta().id.0, bindings.len());
                        }
                        accepted.push((position, bindings));
                    }
                    Err(warning) => {
                        ms_log::runtime_log::log_warn(format!("[ms-input::registry] {warning}"));
                        warnings.push(warning);
                    }
                }
            }
        }
        for (position, bindings) in accepted {
            if let Some(entry) = self.entries.get_mut(position) {
                entry.bindings = bindings;
            }
        }
        self.after_change();
        warnings
    }

    fn decode_override(&self, id: &str, raw_alternatives: &[Value], mode_count: &dyn Fn(ActionId) -> usize) -> Result<(usize, Vec<Binding>), OverrideWarning> {
        let Some(&position) = self.index.get(id) else {
            return Err(OverrideWarning::UnknownAction(id.to_owned()));
        };
        let Some(entry) = self.entries.get(position) else {
            return Err(OverrideWarning::UnknownAction(id.to_owned()));
        };
        if matches!(entry.declared, Declared::Fixed(..)) {
            return Err(OverrideWarning::FixedAction(id.to_owned()));
        }
        let action = entry.meta().id;
        // Checked before decoding, so an oversized list is never even materialized.
        if raw_alternatives.len() > MAX_ALTERNATIVES {
            return Err(OverrideWarning::Invalid { id: id.to_owned(), source: BindingError::TooManyAlternatives { action: action.0, count: raw_alternatives.len() } });
        }
        let kind = entry.kind();
        let resolve = |name: &str| self.resolve_id(name);
        let bindings = raw_alternatives
            .iter()
            .enumerate()
            .map(|(index, raw)| persist::decode_binding(raw, kind, &resolve).map_err(|source| OverrideWarning::Undecodable { id: id.to_owned(), index, source }))
            .collect::<Result<Vec<_>, _>>()?;
        self.validate_list(action, kind, &bindings, mode_count).map_err(|source| OverrideWarning::Invalid { id: id.to_owned(), source })?;
        Ok((position, bindings))
    }

    /// Replaces the alternatives of `id` (an empty list = explicitly unbound).
    ///
    /// Returns the persisted consequence: the encoded list, or a delete when `alternatives`
    /// equals the defaults (an override never freezes the defaults).
    ///
    /// # Errors
    /// [`BindingError::UnknownAction`], [`BindingError::FixedAction`], the first alternative's
    /// validation error, or — for a mode — [`BindingError::TooManyCombinations`] naming a current
    /// binding the new alternative count would push over the limit (nothing changes then).
    pub fn set_bindings(&mut self, id: ActionId, alternatives: Vec<Binding>) -> Result<OverrideEntry, BindingError> {
        let position = self.position_of_rebindable(id)?;
        let kind = self.entries.get(position).map(Entry::kind).ok_or_else(|| BindingError::UnknownAction(id.0.to_owned()))?;
        let current = |mode: ActionId| self.bindings(mode).len();
        self.validate_list(id, kind, &alternatives, &current)?;
        if kind == ActionKind::Mode {
            self.validate_dependents(&|mode: ActionId| if mode == id { alternatives.len() } else { current(mode) })?;
        }
        let entry = self.entries.get_mut(position).ok_or_else(|| BindingError::UnknownAction(id.0.to_owned()))?;
        let is_default = alternatives == entry.meta().defaults;
        let encoded = (!is_default).then(|| alternatives.iter().map(persist::encode_binding).collect());
        entry.bindings = alternatives;
        self.after_change();
        Ok(OverrideEntry { id, alternatives: encoded })
    }

    /// Restores the defaults of `id`; the persisted consequence is always a delete. Never
    /// refused: should restored mode defaults push a dependent override past the combination
    /// limit, `resolve_chord`'s hard bound still caps every expansion.
    ///
    /// # Errors
    /// [`BindingError::UnknownAction`] or [`BindingError::FixedAction`].
    pub fn reset(&mut self, id: ActionId) -> Result<OverrideEntry, BindingError> {
        let position = self.position_of_rebindable(id)?;
        let entry = self.entries.get_mut(position).ok_or_else(|| BindingError::UnknownAction(id.0.to_owned()))?;
        entry.bindings = entry.meta().defaults.clone();
        self.after_change();
        Ok(OverrideEntry { id, alternatives: None })
    }

    fn position_of_rebindable(&self, id: ActionId) -> Result<usize, BindingError> {
        let position = *self.index.get(id.0).ok_or_else(|| BindingError::UnknownAction(id.0.to_owned()))?;
        match self.entries.get(position).map(|entry| &entry.declared) {
            Some(Declared::Action(_)) => Ok(position),
            Some(Declared::Fixed(..)) => Err(BindingError::FixedAction(id.0)),
            None => Err(BindingError::UnknownAction(id.0.to_owned())),
        }
    }

    fn after_change(&mut self) {
        self.conflicts = conflicts::compute(self);
        self.revision = self.revision.saturating_add(1);
    }

    /// The registered id with this text, if any (turns a persisted string into an [`ActionId`]).
    #[must_use]
    pub fn resolve_id(&self, id: &str) -> Option<ActionId> {
        self.action(id).map(|view| view.meta.id)
    }

    /// The read view of `id`.
    #[must_use]
    pub fn action(&self, id: &str) -> Option<ActionView<'_>> {
        self.index.get(id).and_then(|&position| self.entries.get(position)).map(Entry::view)
    }

    /// Every action (declared first, fixed last), in declaration order.
    pub fn actions(&self) -> impl Iterator<Item = ActionView<'_>> {
        self.entries.iter().map(Entry::view)
    }

    /// The kind of `id`, if registered.
    #[must_use]
    pub fn kind(&self, id: &str) -> Option<ActionKind> {
        self.index.get(id).and_then(|&position| self.entries.get(position)).map(Entry::kind)
    }

    /// The current alternatives of `id` (empty for an unknown id).
    #[must_use]
    pub fn bindings(&self, id: ActionId) -> &[Binding] {
        self.action(id.0).map_or(&[], |view| view.bindings)
    }

    /// The declared default alternatives of `id` (empty for an unknown id).
    #[must_use]
    pub fn defaults(&self, id: ActionId) -> &[Binding] {
        self.action(id.0).map_or(&[], |view| view.meta.defaults.as_slice())
    }

    /// The declared spec of `id` (`None` for unknown and fixed actions).
    #[must_use]
    pub fn spec(&self, id: ActionId) -> Option<&ActionSpec> {
        self.action(id.0).and_then(|view| view.spec)
    }

    /// The declared gesture target `id`.
    #[must_use]
    pub fn target(&self, id: TargetId) -> Option<&TargetSpec> {
        self.targets.iter().find(|target| target.id == id)
    }

    /// Every declared gesture target.
    #[must_use]
    pub fn targets(&self) -> &[TargetSpec] {
        &self.targets
    }

    /// The declared exclusive context groups.
    #[must_use]
    pub fn exclusive_contexts(&self) -> &[Vec<ContextId>] {
        &self.exclusive_contexts
    }

    /// Incremented by every binding change; label caches compare it.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The full conflict and shadow report over the current bindings.
    #[must_use]
    pub fn conflicts(&self) -> &ConflictReport {
        &self.conflicts
    }

    /// The conflicts `id` takes part in (either side).
    #[must_use]
    pub fn conflicts_for(&self, id: ActionId) -> Vec<&Conflict> {
        self.conflicts.conflicts.iter().filter(|conflict| conflict.a == id || conflict.b == Some(id)).collect()
    }

    /// The shadows `id` takes part in (as the inner or the outer action).
    #[must_use]
    pub fn shadows_for(&self, id: ActionId) -> Vec<&Shadow> {
        self.conflicts.shadows.iter().filter(|shadow| shadow.inner == id || shadow.outer == id).collect()
    }

    /// The current button sets of mode `id` (one per alternative; empty if `id` is not a mode).
    #[must_use]
    pub fn mode_button_sets(&self, id: ActionId) -> Vec<BTreeSet<Button>> {
        self.bindings(id)
            .iter()
            .filter_map(|binding| match binding {
                Binding::Mode(chord) => Some(chord.buttons.clone()),
                Binding::Command(_) | Binding::Axis(_) | Binding::Gesture(_) => None,
            })
            .collect()
    }

    /// The concrete button sets `chord` stands for under the CURRENT mode bindings
    /// (see [`resolve_chord`]).
    #[must_use]
    pub fn resolve(&self, chord: &Chord) -> Vec<BTreeSet<Button>> {
        resolve_chord(chord, &|mode| self.mode_button_sets(mode))
    }
}
