/*
File: crates/ms-config/src/settings_deep_link.rs

Purpose:
The in-app "deep link" into the settings surface: the enumeration of reveal targets one
part of the app can ask the settings surface to open, expand, scroll to and highlight.

Why it lives HERE and not next to the settings surface itself:
the REQUESTERS (the typing tab) and the CONSUMER (`SettingsTabState::navigate_to`, in the
binary's settings tab) sit in different crates, and the requester must not depend on the
settings UI. This enum is the only thing they share, it is a pure data type, and this crate
is already below both — so it is declared here and re-exported by `src/settings_shared.rs`,
which keeps `crate::settings_shared::SettingsDeepLink` valid inside the binary.

Key items:
- `SettingsDeepLink`
*/

/// An in-app "deep link" request: a target inside the settings surface that another
/// part of the app asks to reveal (open the right section, expand the relevant block,
/// scroll to it, highlight it). Consumed by `SettingsTabState::navigate_to`.
///
/// Each variant names a concrete reveal target, NOT just a section, so a link can point
/// at a nested collapsed block. Extend it with a new variant per future target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsDeepLink {
    /// Settings → "Тайп" (Typesetting) → "Настройки шрифтов" → nested "Группы" block.
    TypesettingFontGroups,
}
