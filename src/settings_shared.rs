/*
FILE OVERVIEW: src/settings_shared.rs
Menu-level shared layer for the two settings surfaces (launcher settings page and
studio settings tab).

Why this exists:
Both surfaces hand-code their own tab enum, tab bar, and dispatch match while sharing
only three widget-level "double-interface" panels (General, AiBackend, Tutorials).
This module adds a thin, shared source of truth for section metadata plus a container
that owns the three shared panel states, WITHOUT merging the two heavyweight per-surface
state containers.

Key items:
- `SettingsSurface`: which surface is rendering (`Launcher` / `Studio`).
- `SettingsSectionId`: the union of all settings sections across both surfaces.
- `SettingsSectionDescriptor` + `SECTIONS`: the static section registry (which surfaces
  list a section, and its ascending display order within a surface).
- `sections_for`: the ordered, surface-filtered section list a tab bar iterates.
- `title_key`: the EXISTING localization key each surface uses for a section (labels
  differ per surface, so this is a function of `(id, surface)`, not a table column).
- `SharedSettingsPanels`: owns the three shared panel states so each surface embeds ONE
  instance (independent scratch preserved).
- `SharedSectionOutcome`: the per-surface runtime effects a shared section produced.

Contract notes:
- The `AiBackendHandle` is passed into `draw` BY REFERENCE, not owned here, so the
  existing app-global backend sharing (launcher `ai_backend`, studio `ai_backend_handle`)
  is unchanged.
- `draw` renders ONLY the shared sections (`General`/`AiBackend`/`Tutorials`); a caller
  must not route its own local sections here (debug-asserted).
- Dynamic per-surface cases (the launcher hiding/relabelling `TorchUpgrade`) are NOT
  handled here; the registry lists the section and the surface post-filters/relabels it.
*/

use crate::ai_backend_panel::{AiBackendPanelState, draw_ai_backend_panel};
use crate::ai_backend_supervisor::AiBackendHandle;
use crate::general_settings_panel::{GeneralSettingsPanelState, draw_general_settings_panel};
use crate::memory_manager::MemoryProfile;
use std::path::PathBuf;
#[cfg(feature = "tutorial")]
use crate::tutorial::{TutorialProgressHandle, draw_tutorials_pane};

/// Which settings surface is currently rendering. Selects the localization keys and
/// the section set (see [`sections_for`] and [`title_key`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsSurface {
    /// The pre-project Rust launcher settings page (`launcher/pages/settings_page.rs`).
    Launcher,
    /// The in-editor studio settings tab (`tabs/settings/mod.rs`).
    Studio,
}

/// The union of every settings section across both surfaces.
///
/// A section is either SHARED (rendered by [`SharedSettingsPanels::draw`]) or exclusive
/// to one surface (rendered by that surface's own local renderer). The `surfaces`
/// column of each [`SECTIONS`] entry records where a section appears; a section not
/// listed for a surface is never shown there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsSectionId {
    // Shared sections (rendered by `SharedSettingsPanels`).
    /// Projects directory + global memory profile + UI/typeset language (shared widget).
    General,
    /// AI backend process controls + ONNX provider/device selection (shared widget).
    AiBackend,
    /// Tutorial progress / reset pane (shared widget). Gated behind the `tutorial`
    /// feature (off by default).
    #[cfg(feature = "tutorial")]
    Tutorials,
    // Launcher-only sections (rendered by the launcher settings page).
    /// System CPU/RAM/accelerator information (launcher only).
    SystemInfo,
    /// PyTorch / ONNX Runtime package probe (launcher only).
    AiComputations,
    /// PyTorch upgrade / alternate-wheel install flow (launcher only). The launcher
    /// hides this when no AI is installed and relabels it by install type.
    TorchUpgrade,
    /// Interactive Python-environment shell console (launcher only).
    PythonEnvironment,
    // Studio-only sections (rendered by the studio settings tab).
    /// Canvas/ribbon settings, comic type, and bubble-status rules (studio only).
    CanvasRibbon,
    /// Typesetting ("Тайп") options: hanging punctuation, rotation mode, effect/font
    /// defaults (studio only).
    Typesetting,
    /// Keyboard shortcut editor (studio only).
    Hotkeys,
}

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

/// One row of the static section registry: which surfaces list a section and where it
/// sits in a surface's tab bar.
///
/// `order` is the ascending display order WITHIN a surface. A single value serves both
/// surfaces because launcher-only and studio-only ids never collide inside one surface;
/// the shared ids (`General`/`AiBackend`/`Tutorials`) keep the same relative position in
/// both. The section's localized title is NOT stored here because it differs per surface;
/// see [`title_key`].
#[derive(Debug, Clone, Copy)]
pub struct SettingsSectionDescriptor {
    /// The section this descriptor describes.
    pub id: SettingsSectionId,
    /// The surfaces that list this section, in no particular order.
    pub surfaces: &'static [SettingsSurface],
    /// Ascending display order within a surface (lower renders first).
    pub order: u16,
}

/// The static section registry: the single source of truth for which sections each
/// surface shows and in what order. Iterate it via [`sections_for`].
const SECTIONS: &[SettingsSectionDescriptor] = &[
    SettingsSectionDescriptor {
        id: SettingsSectionId::General,
        surfaces: &[SettingsSurface::Launcher, SettingsSurface::Studio],
        order: 10,
    },
    SettingsSectionDescriptor {
        id: SettingsSectionId::SystemInfo,
        surfaces: &[SettingsSurface::Launcher],
        order: 20,
    },
    SettingsSectionDescriptor {
        id: SettingsSectionId::CanvasRibbon,
        surfaces: &[SettingsSurface::Studio],
        order: 20,
    },
    SettingsSectionDescriptor {
        id: SettingsSectionId::AiComputations,
        surfaces: &[SettingsSurface::Launcher],
        order: 30,
    },
    SettingsSectionDescriptor {
        id: SettingsSectionId::Typesetting,
        surfaces: &[SettingsSurface::Studio],
        order: 30,
    },
    SettingsSectionDescriptor {
        id: SettingsSectionId::AiBackend,
        surfaces: &[SettingsSurface::Launcher, SettingsSurface::Studio],
        order: 40,
    },
    SettingsSectionDescriptor {
        id: SettingsSectionId::TorchUpgrade,
        surfaces: &[SettingsSurface::Launcher],
        order: 50,
    },
    SettingsSectionDescriptor {
        id: SettingsSectionId::Hotkeys,
        surfaces: &[SettingsSurface::Studio],
        order: 50,
    },
    SettingsSectionDescriptor {
        id: SettingsSectionId::PythonEnvironment,
        surfaces: &[SettingsSurface::Launcher],
        order: 60,
    },
    #[cfg(feature = "tutorial")]
    SettingsSectionDescriptor {
        id: SettingsSectionId::Tutorials,
        surfaces: &[SettingsSurface::Launcher, SettingsSurface::Studio],
        order: 70,
    },
];

/// Returns the descriptors listed for `surface`, sorted ascending by `order`.
///
/// This is exactly the sequence a surface's tab bar renders left-to-right (before any
/// surface-specific post-filtering such as the launcher hiding `TorchUpgrade`). The
/// returned references borrow the static [`SECTIONS`] table.
#[must_use]
pub fn sections_for(surface: SettingsSurface) -> Vec<&'static SettingsSectionDescriptor> {
    let mut sections: Vec<&'static SettingsSectionDescriptor> = SECTIONS
        .iter()
        .filter(|descriptor| descriptor.surfaces.contains(&surface))
        .collect();
    sections.sort_by_key(|descriptor| descriptor.order);
    sections
}

/// Returns the EXISTING localization key a surface uses for a section's tab label.
///
/// Labels differ per surface (the launcher uses `launcher.settings.tab_*`, the studio
/// uses `settings.nav.*`), so the key is a function of both `id` and `surface`. For a
/// section listed on only one surface, the `surface` argument is ignored. `TorchUpgrade`
/// returns its base ("upgrade to full") label; the launcher relabels it by install type.
#[must_use]
pub fn title_key(id: SettingsSectionId, surface: SettingsSurface) -> &'static str {
    match id {
        SettingsSectionId::General => match surface {
            SettingsSurface::Launcher => "launcher.settings.tab_general",
            SettingsSurface::Studio => "settings.nav.general",
        },
        SettingsSectionId::AiBackend => match surface {
            SettingsSurface::Launcher => "launcher.settings.tab_ai_backend",
            SettingsSurface::Studio => "settings.nav.ai_backend",
        },
        #[cfg(feature = "tutorial")]
        SettingsSectionId::Tutorials => match surface {
            SettingsSurface::Launcher => "launcher.settings.tab_tutorial",
            SettingsSurface::Studio => "settings.nav.tutorials",
        },
        SettingsSectionId::SystemInfo => "launcher.settings.tab_system_info",
        SettingsSectionId::AiComputations => "launcher.settings.tab_ai_compute",
        SettingsSectionId::TorchUpgrade => "launcher.settings.upgrade_to_full_button",
        SettingsSectionId::PythonEnvironment => "launcher.settings.tab_python_env",
        SettingsSectionId::CanvasRibbon => "settings.nav.canvas_ribbon",
        SettingsSectionId::Typesetting => "settings.nav.typesetting",
        SettingsSectionId::Hotkeys => "settings.nav.hotkeys",
    }
}

/// Per-surface runtime effects produced by rendering a shared section.
///
/// Each field is `Some` only when that change happened this frame. The launcher reacts
/// to `projects_dir_saved` (emitting `ProjectsRootChanged`); the studio reacts to
/// `memory_profile_changed` (applying it to the `MemoryManager`). Each surface ignores
/// the field it does not use, exactly as before this refactor.
#[derive(Debug, Default)]
pub struct SharedSectionOutcome {
    /// Set to the normalized saved root when the user saved a new projects directory.
    pub projects_dir_saved: Option<PathBuf>,
    /// Set to the new profile when the memory-profile selection changed.
    pub memory_profile_changed: Option<MemoryProfile>,
}

/// Owns the three shared "double-interface" panel states so each surface embeds ONE
/// instance and keeps independent scratch state.
///
/// The `AiBackendHandle` is intentionally NOT owned here: it is passed into [`draw`]
/// by reference so the app-global backend handle each surface already holds stays the
/// single owner (launcher `ai_backend`, studio `ai_backend_handle`).
///
/// [`draw`]: SharedSettingsPanels::draw
#[derive(Debug)]
pub struct SharedSettingsPanels {
    /// General-settings widget state (projects dir + memory profile + languages).
    general: GeneralSettingsPanelState,
    /// AI backend panel scratch state.
    ai_backend: AiBackendPanelState,
    /// Tutorial progress handle shared with the surface's tutorial controller.
    #[cfg(feature = "tutorial")]
    tutorial_progress: TutorialProgressHandle,
}

impl SharedSettingsPanels {
    /// Builds the shared panel container.
    ///
    /// Seeds [`GeneralSettingsPanelState`] from `user_config.json` and a default
    /// [`AiBackendPanelState`]. The `AiBackendHandle` is NOT taken here because it is
    /// not owned (see the type docs); it is supplied per-frame to [`draw`]. Callers that
    /// need the projects root pre-seeded should follow with [`set_projects_root`].
    ///
    /// [`draw`]: SharedSettingsPanels::draw
    /// [`set_projects_root`]: SharedSettingsPanels::set_projects_root
    #[must_use]
    pub fn new(
        #[cfg(feature = "tutorial")] tutorial_progress: TutorialProgressHandle,
    ) -> Self {
        Self {
            general: GeneralSettingsPanelState::new(),
            ai_backend: AiBackendPanelState::default(),
            #[cfg(feature = "tutorial")]
            tutorial_progress,
        }
    }

    /// Re-syncs the general widget's projects-dir fields when the projects root changes
    /// externally (used by the launcher when another page changes the root).
    pub fn set_projects_root(&mut self, root: &str) {
        self.general.set_projects_root(root);
    }

    /// The memory profile seeded from config, so the studio can apply it to its
    /// `MemoryManager` at construction / rebinding (before any `draw`).
    #[must_use]
    pub fn memory_profile(&self) -> MemoryProfile {
        self.general.memory_profile
    }

    /// Renders one SHARED section and returns its runtime effects.
    ///
    /// Handles only `General` / `AiBackend` / `Tutorials`; `ai_backend` is the caller's
    /// app-global handle, borrowed for this frame. A non-shared `id` returns an empty
    /// outcome and debug-asserts, since a caller must render its own local sections
    /// itself rather than route them here.
    #[must_use]
    pub fn draw(
        &mut self,
        id: SettingsSectionId,
        ui: &mut egui::Ui,
        surface: SettingsSurface,
        ai_backend: &AiBackendHandle,
    ) -> SharedSectionOutcome {
        // `surface` is accepted for a stable, surface-aware signature; the shared
        // widgets currently render identically on both surfaces.
        let _ = surface;
        match id {
            SettingsSectionId::General => {
                let outcome = draw_general_settings_panel(ui, &mut self.general);
                SharedSectionOutcome {
                    projects_dir_saved: outcome.projects_dir_saved,
                    memory_profile_changed: outcome.memory_profile_changed,
                }
            }
            SettingsSectionId::AiBackend => {
                draw_ai_backend_panel(ui, ai_backend, &mut self.ai_backend);
                SharedSectionOutcome::default()
            }
            #[cfg(feature = "tutorial")]
            SettingsSectionId::Tutorials => {
                draw_tutorials_pane(ui, &self.tutorial_progress);
                SharedSectionOutcome::default()
            }
            SettingsSectionId::SystemInfo
            | SettingsSectionId::AiComputations
            | SettingsSectionId::TorchUpgrade
            | SettingsSectionId::PythonEnvironment
            | SettingsSectionId::CanvasRibbon
            | SettingsSectionId::Typesetting
            | SettingsSectionId::Hotkeys => {
                debug_assert!(
                    false,
                    "SharedSettingsPanels::draw called for non-shared section {id:?}"
                );
                SharedSectionOutcome::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn launcher_sections_are_filtered_and_ordered() {
        let ids: Vec<SettingsSectionId> = sections_for(SettingsSurface::Launcher)
            .iter()
            .map(|descriptor| descriptor.id)
            .collect();
        #[cfg(feature = "tutorial")]
        let expected = vec![
            SettingsSectionId::General,
            SettingsSectionId::SystemInfo,
            SettingsSectionId::AiComputations,
            SettingsSectionId::AiBackend,
            SettingsSectionId::TorchUpgrade,
            SettingsSectionId::PythonEnvironment,
            SettingsSectionId::Tutorials,
        ];
        #[cfg(not(feature = "tutorial"))]
        let expected = vec![
            SettingsSectionId::General,
            SettingsSectionId::SystemInfo,
            SettingsSectionId::AiComputations,
            SettingsSectionId::AiBackend,
            SettingsSectionId::TorchUpgrade,
            SettingsSectionId::PythonEnvironment,
        ];
        assert_eq!(ids, expected);
    }

    #[test]
    fn studio_sections_are_filtered_and_ordered() {
        let ids: Vec<SettingsSectionId> = sections_for(SettingsSurface::Studio)
            .iter()
            .map(|descriptor| descriptor.id)
            .collect();
        #[cfg(feature = "tutorial")]
        let expected = vec![
            SettingsSectionId::General,
            SettingsSectionId::CanvasRibbon,
            SettingsSectionId::Typesetting,
            SettingsSectionId::AiBackend,
            SettingsSectionId::Hotkeys,
            SettingsSectionId::Tutorials,
        ];
        #[cfg(not(feature = "tutorial"))]
        let expected = vec![
            SettingsSectionId::General,
            SettingsSectionId::CanvasRibbon,
            SettingsSectionId::Typesetting,
            SettingsSectionId::AiBackend,
            SettingsSectionId::Hotkeys,
        ];
        assert_eq!(ids, expected);
    }

    #[test]
    fn no_duplicate_ids_within_a_surface() {
        for surface in [SettingsSurface::Launcher, SettingsSurface::Studio] {
            let mut seen = HashSet::new();
            for descriptor in sections_for(surface) {
                assert!(
                    seen.insert(descriptor.id),
                    "duplicate id {:?} for {surface:?}",
                    descriptor.id
                );
            }
        }
    }

    #[test]
    fn title_keys_match_the_pre_refactor_localization_keys() {
        // Pin every (id, surface) -> key mapping to its exact pre-refactor value. A future
        // typo in `title_key` would otherwise compile and silently render the raw fallback key
        // in the tab bar instead of the localized label; this golden table forces any change to
        // be deliberate and mirrored here.
        use SettingsSectionId as Id;
        use SettingsSurface::{Launcher, Studio};

        let expected: &[(SettingsSectionId, SettingsSurface, &str)] = &[
            (Id::General, Launcher, "launcher.settings.tab_general"),
            (Id::General, Studio, "settings.nav.general"),
            (Id::AiBackend, Launcher, "launcher.settings.tab_ai_backend"),
            (Id::AiBackend, Studio, "settings.nav.ai_backend"),
            #[cfg(feature = "tutorial")]
            (Id::Tutorials, Launcher, "launcher.settings.tab_tutorial"),
            #[cfg(feature = "tutorial")]
            (Id::Tutorials, Studio, "settings.nav.tutorials"),
            (Id::SystemInfo, Launcher, "launcher.settings.tab_system_info"),
            (Id::AiComputations, Launcher, "launcher.settings.tab_ai_compute"),
            (
                Id::TorchUpgrade,
                Launcher,
                "launcher.settings.upgrade_to_full_button",
            ),
            (
                Id::PythonEnvironment,
                Launcher,
                "launcher.settings.tab_python_env",
            ),
            (Id::CanvasRibbon, Studio, "settings.nav.canvas_ribbon"),
            (Id::Typesetting, Studio, "settings.nav.typesetting"),
            (Id::Hotkeys, Studio, "settings.nav.hotkeys"),
        ];

        for &(id, surface, key) in expected {
            assert_eq!(
                title_key(id, surface),
                key,
                "unexpected title key for {id:?} on {surface:?}"
            );
        }

        // Every section actually listed for a surface must appear in the golden table above,
        // so a newly added section cannot slip through without a pinned key.
        for surface in [Launcher, Studio] {
            for descriptor in sections_for(surface) {
                assert!(
                    expected
                        .iter()
                        .any(|&(id, s, _)| id == descriptor.id && s == surface),
                    "section {:?} on {surface:?} is missing from the title-key golden table",
                    descriptor.id
                );
            }
        }
    }
}
