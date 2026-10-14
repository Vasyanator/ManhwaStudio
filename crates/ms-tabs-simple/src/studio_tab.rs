/*
File: crates/ms-tabs-simple/src/studio_tab.rs

Purpose:
The four simple tabs' answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`): «Персонажи», «Термины», «Заметки» and «Вики».

Key items:
- `impl StudioTab` for `CharactersTabState`, `TermsTabState`, `NotesTabState` and
  `WikiTabState`, all empty.

Notes:
- Every hook keeps its no-op default on purpose: none of these tabs hosts the panel dock,
  declares hotkeys, draws source pages, owns a GPU cache in the root's pressure eviction, polls
  background work outside its own draw, writes exit-only settings, holds a debounced store or
  caches a view of the committed chapter tree. The notes refresh after a characters / terms
  change is an ordered app request (`AppRequest`), not a hook.
- `WikiTabState` is also mounted by the launcher, which simply does not use this impl.
*/

use ms_studio_tab::StudioTab;

use crate::characters::CharactersTabState;
use crate::notes::NotesTabState;
use crate::terms::TermsTabState;
use crate::wiki::WikiTabState;

impl StudioTab for CharactersTabState {}

impl StudioTab for TermsTabState {}

impl StudioTab for NotesTabState {}

impl StudioTab for WikiTabState {}

#[cfg(test)]
mod tests {
    use ms_studio_tab::StudioTab;

    /// Compile-time guard: the studio root holds each of the four tab states as `dyn StudioTab`.
    fn is_studio_tab<T: StudioTab + 'static>() {}

    #[test]
    fn the_four_simple_tabs_are_studio_tabs() {
        is_studio_tab::<crate::characters::CharactersTabState>();
        is_studio_tab::<crate::terms::TermsTabState>();
        is_studio_tab::<crate::notes::NotesTabState>();
        is_studio_tab::<crate::wiki::WikiTabState>();
    }
}
