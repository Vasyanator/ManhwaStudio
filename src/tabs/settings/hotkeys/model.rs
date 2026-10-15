/*
File: src/tabs/settings/hotkeys/model.rs

Purpose:
Pure, GUI-free decisions of the hotkeys pane: how actions are grouped and ordered, which declared
modes the "use as modifier" chips offer, how a capture's chip choice rewrites the captured
alternative, which alternative a conflict or shadow line belongs under, and the search match.

Key structures:
- `Group`: the pane's top-level blocks (session, view, canvas, shared tool contexts, one per tab).
- `RowKey`: the grouping inputs of one action (id, scope, section key, declaration index).
- `GroupBlock` / `SectionBlock`: the ordered layout the pane draws.

Key functions:
- `group_of`, `layout_rows`: grouping and ordering.
- `offered_modes`, `template_modes`, `apply_modifier_choice`: the "use as modifier" chips.
- `template_axis_source`, `template_gesture_kind`: what an added axis / touch alternative
  starts from.
- `conflicts_by_alternative`, `shadows_by_alternative`: conflict-row mapping.
- `matches_query`: case-insensitive search over display texts.

Notes:
- Nothing here draws or reads an egui `Context`; `tests.rs` pins every rule.
*/

use std::collections::{BTreeMap, BTreeSet};

use ms_config::app_tab::AppTab;
use ms_input::{ActionId, ActionKind, AxisSource, Binding, Button, CaptureSlot, Chord, Conflict, ConflictReport, GestureKind, InputRegistry, Scope, ScopeLevel, Shadow};

/// A top-level block of the pane, outermost first. Sub-contexts and windows owned by a tab are
/// listed inside that tab's block; shared ones (no owning tab) get their own block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Group {
    Session,
    View,
    Canvas,
    SharedContexts,
    Tab(AppTab),
}

impl Group {
    /// Sort key: fixed blocks first, then tabs in program tab order.
    fn order(self) -> (u8, usize) {
        match self {
            Self::Session => (0, 0),
            Self::View => (1, 0),
            Self::Canvas => (2, 0),
            Self::SharedContexts => (3, 0),
            Self::Tab(tab) => (4, AppTab::ALL.iter().position(|candidate| *candidate == tab).unwrap_or(AppTab::ALL.len())),
        }
    }
}

/// The block an action of `scope` is listed in.
pub(super) fn group_of(scope: Scope) -> Group {
    match scope {
        Scope::Session => Group::Session,
        Scope::View => Group::View,
        Scope::Canvas => Group::Canvas,
        Scope::Tab(tab) => Group::Tab(tab),
        Scope::Sub(owner) | Scope::Window(owner) => owner.map_or(Group::SharedContexts, Group::Tab),
    }
}

/// The grouping inputs of one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowKey {
    pub(super) id: ActionId,
    pub(super) scope: Scope,
    pub(super) section_key: &'static str,
    /// Position in the registry's declaration order (fixed actions come last there).
    pub(super) declaration_index: usize,
}

/// Consecutive actions of one section on one scope level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SectionBlock {
    pub(super) section_key: &'static str,
    pub(super) level: ScopeLevel,
    pub(super) actions: Vec<ActionId>,
}

/// One top-level block with its sections in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GroupBlock {
    pub(super) group: Group,
    pub(super) sections: Vec<SectionBlock>,
}

/// Orders `rows` for display: blocks by [`Group`] order; inside a block, outer scope levels
/// first (a tab's own actions before its sub-contexts and windows); on one level, sections in
/// the order their first action was declared; inside a section, declaration order.
pub(super) fn layout_rows(rows: &[RowKey]) -> Vec<GroupBlock> {
    // First declaration index of every (group, level, section): the section's position.
    let mut section_first: BTreeMap<((u8, usize), ScopeLevel, &'static str), usize> = BTreeMap::new();
    for row in rows {
        let key = (group_of(row.scope).order(), row.scope.level(), row.section_key);
        section_first.entry(key).and_modify(|first| *first = (*first).min(row.declaration_index)).or_insert(row.declaration_index);
    }
    let mut sorted: Vec<&RowKey> = rows.iter().collect();
    sorted.sort_by_key(|row| {
        let group = group_of(row.scope).order();
        let level = row.scope.level();
        let section = section_first.get(&(group, level, row.section_key)).copied().unwrap_or(row.declaration_index);
        (group, level, section, row.declaration_index)
    });
    let mut blocks: Vec<GroupBlock> = Vec::new();
    for row in sorted {
        let group = group_of(row.scope);
        let level = row.scope.level();
        if blocks.last().is_none_or(|block| block.group != group) {
            blocks.push(GroupBlock { group, sections: Vec::new() });
        }
        // Invariant: the block was pushed just above when the list was empty.
        let Some(block) = blocks.last_mut() else { continue };
        match block.sections.last_mut() {
            Some(section) if section.level == level && section.section_key == row.section_key => section.actions.push(row.id),
            Some(_) | None => block.sections.push(SectionBlock { section_key: row.section_key, level, actions: vec![row.id] }),
        }
    }
    blocks
}

/// The grouping inputs of every registered action, in declaration order.
pub(super) fn row_keys(registry: &InputRegistry) -> Vec<RowKey> {
    registry
        .actions()
        .enumerate()
        .map(|(declaration_index, view)| RowKey { id: view.meta.id, scope: view.meta.scope, section_key: view.meta.section_key, declaration_index })
        .collect()
}

/// The declared modes a capture of `action` may attach as modifier references ("use as
/// modifier" chips): rebindable modes of the same scope, or of any scope owned by the same tab,
/// plus every mode the action's current alternatives already reference. Empty for modes
/// (a mode alternative never holds references) and for fixed or unknown actions. Declaration
/// order.
pub(super) fn offered_modes(registry: &InputRegistry, action: ActionId) -> Vec<ActionId> {
    let Some(view) = registry.action(action.0) else { return Vec::new() };
    if view.fixed || view.kind == ActionKind::Mode {
        return Vec::new();
    }
    let referenced: BTreeSet<ActionId> = view.bindings.iter().flat_map(|binding| binding.chords().into_iter().flat_map(|chord| chord.modes.iter().copied())).collect();
    let scope = view.meta.scope;
    registry
        .actions()
        .filter(|candidate| candidate.kind == ActionKind::Mode && !candidate.fixed && candidate.meta.id != action)
        .filter(|candidate| {
            let same_scope = candidate.meta.scope == scope;
            let same_tab = candidate.meta.scope.tab().is_some() && candidate.meta.scope.tab() == scope.tab();
            same_scope || same_tab || referenced.contains(&candidate.meta.id)
        })
        .map(|candidate| candidate.meta.id)
        .collect()
}

/// The mode references the capture of `action` into `slot` starts from: those of the chord the
/// capture writes (the last chord of a command, the chord of an axis or gesture) in the
/// template alternative — the replaced one, else the first current, else the first default.
/// This mirrors the template rule of `ms_input::captured_alternatives`.
pub(super) fn template_modes(registry: &InputRegistry, action: ActionId, slot: CaptureSlot) -> BTreeSet<ActionId> {
    let Some(view) = registry.action(action.0) else { return BTreeSet::new() };
    let template = match slot {
        CaptureSlot::Replace(index) => view.bindings.get(index),
        CaptureSlot::Append => view.bindings.first().or_else(|| view.meta.defaults.first()),
    };
    template.and_then(target_chord).map(|chord| chord.modes.clone()).unwrap_or_default()
}

/// The alternative new alternatives of `action` are modelled on: its first current one, else
/// its first default (the same template `ms_input::captured_alternatives` appends with).
fn append_template(registry: &InputRegistry, action: ActionId) -> Option<&Binding> {
    let view = registry.action(action.0)?;
    view.bindings.first().or_else(|| view.meta.defaults.first())
}

/// The axis source a new alternative of `action` starts with (`None` when `action` is not an
/// axis or has no template).
pub(super) fn template_axis_source(registry: &InputRegistry, action: ActionId) -> Option<AxisSource> {
    match append_template(registry, action)? {
        Binding::Axis(axis) => Some(axis.source),
        Binding::Command(_) | Binding::Mode(_) | Binding::Gesture(_) => None,
    }
}

/// The gesture kind a new alternative of `action` starts with (`None` when `action` is not a
/// gesture or has no template).
pub(super) fn template_gesture_kind(registry: &InputRegistry, action: ActionId) -> Option<GestureKind> {
    match append_template(registry, action)? {
        Binding::Gesture(gesture) => Some(gesture.kind),
        Binding::Command(_) | Binding::Mode(_) | Binding::Axis(_) => None,
    }
}

/// The chord a capture writes into: the last chord of a command sequence, the chord of an axis
/// or gesture; `None` for a mode alternative.
fn target_chord(binding: &Binding) -> Option<&Chord> {
    match binding {
        Binding::Command(command) => command.sequence.last(),
        Binding::Axis(axis) => Some(&axis.chord),
        Binding::Gesture(gesture) => Some(&gesture.chord),
        Binding::Mode(_) => None,
    }
}

fn target_chord_mut(binding: &mut Binding) -> Option<&mut Chord> {
    match binding {
        Binding::Command(command) => command.sequence.last_mut(),
        Binding::Axis(axis) => Some(&mut axis.chord),
        Binding::Gesture(gesture) => Some(&mut gesture.chord),
        Binding::Mode(_) => None,
    }
}

/// Applies the user's "use as modifier" choice to a freshly captured alternative: the target
/// chord's mode references become exactly `selected`, and every alternative of a selected mode
/// that is contained in the captured buttons is removed from them (pressing Ctrl+= with the
/// Ctrl-bound zoom modifier selected yields `[zoom modifier] + =`). `mode_sets` returns a
/// mode's CURRENT button sets. A mode alternative is left unchanged.
pub(super) fn apply_modifier_choice(binding: &mut Binding, selected: &BTreeSet<ActionId>, mode_sets: &dyn Fn(ActionId) -> Vec<BTreeSet<Button>>) {
    let Some(chord) = target_chord_mut(binding) else { return };
    for mode in selected {
        for set in mode_sets(*mode) {
            if !set.is_empty() && set.is_subset(&chord.buttons) {
                chord.buttons.retain(|button| !set.contains(button));
            }
        }
    }
    chord.modes.clone_from(selected);
}

/// The alternative index of `id` a conflict line belongs under.
fn conflict_alternative(conflict: &Conflict, id: ActionId) -> Option<usize> {
    if conflict.a == id {
        Some(conflict.a_alt)
    } else if conflict.b == Some(id) {
        conflict.b_alt
    } else {
        None
    }
}

/// The conflicts of `id`, keyed by the index of the alternative of `id` they concern (the `a`
/// or `b` side, whichever is `id`). Report order is kept inside each list.
pub(super) fn conflicts_by_alternative(report: &ConflictReport, id: ActionId) -> BTreeMap<usize, Vec<Conflict>> {
    let mut rows: BTreeMap<usize, Vec<Conflict>> = BTreeMap::new();
    for conflict in &report.conflicts {
        if let Some(alternative) = conflict_alternative(conflict, id) {
            rows.entry(alternative).or_default().push(*conflict);
        }
    }
    rows
}

/// The shadows `id` takes part in, keyed by the index of its own alternative (the inner or the
/// outer side, whichever is `id`).
pub(super) fn shadows_by_alternative(report: &ConflictReport, id: ActionId) -> BTreeMap<usize, Vec<Shadow>> {
    let mut rows: BTreeMap<usize, Vec<Shadow>> = BTreeMap::new();
    for shadow in &report.shadows {
        let alternative = if shadow.inner == id {
            Some(shadow.inner_alt)
        } else if shadow.outer == id {
            Some(shadow.outer_alt)
        } else {
            None
        };
        if let Some(alternative) = alternative {
            rows.entry(alternative).or_default().push(*shadow);
        }
    }
    rows
}

/// Whether any of `texts` contains `query` (already trimmed and lowercased by the caller),
/// case-insensitively. An empty query matches everything.
pub(super) fn matches_query(query: &str, texts: &[&str]) -> bool {
    query.is_empty() || texts.iter().any(|text| text.to_lowercase().contains(query))
}
