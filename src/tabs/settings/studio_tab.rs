/*
File: src/tabs/settings/studio_tab.rs

Purpose:
The studio Settings tab's answer to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`), so `src/studio/tabs.rs` (`Tabs`) can iterate all ten program tabs.

Key items:
- `impl StudioTab for SettingsTabState`: empty.

Notes:
- Every hook keeps its no-op default on purpose: the Settings tab hosts no panel dock, declares
  no hotkeys (it EDITS the registry, lent through the draw frame), draws no source pages, owns no
  GPU cache in the root's pressure eviction, polls nothing outside its own draw, writes no
  exit-only setting (its own settings are persisted by its writers as they change) and caches no
  view of the committed chapter tree.
*/

use ms_studio_tab::StudioTab;

use super::SettingsTabState;

impl StudioTab for SettingsTabState {}
