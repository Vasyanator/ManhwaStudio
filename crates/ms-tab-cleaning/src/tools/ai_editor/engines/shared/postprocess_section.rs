/*
File: cleaning/tools/ai_editor/engines/shared/postprocess_section.rs

Purpose:
The «Постобработка» section of a region engine whose runs answer RAW pixels (FLUX.2 klein and
FLUX.3 klein): the region-editor section chrome around the shared settings body of
`ms_region_postprocess`, docked at the bottom of the engine's scrolled panel body.

Key functions:
- `draw_postprocess_section()`

Notes:
The body (dials, checkbox, ranges, texts) is `ms_region_postprocess::draw_postprocess_body`, the
one owner; this file adds only the chrome every region-editor section uses
(`RegionEditToolBase::draw_region_editor_collapsible_section`, collapsed by default) and the
bottom placement (`ms_widgets::panel_dock::elastic::bottom_aligned`). The settings themselves
are the engine's: it persists them and answers them through `AiEngine::postprocess_settings`,
which the host re-reads every frame to re-derive a pending result.
*/

use crate::tools::base::RegionEditToolBase;
use ms_region_postprocess::PostprocessSettings;
use ms_widgets::panel_dock::elastic;
use eframe::egui;

/// Draws the «Постобработка» section for `settings` and returns whether a value changed.
///
/// Call it as the LAST thing of the engine's `draw_parameters` body, directly in the `Ui` the
/// host hands it (or a `push_id` child of it), never inside a nested `ScrollArea`: it sits at the
/// bottom of the dock body's free room while the body is shorter than the panel, and right
/// after the content once the body scrolls (`elastic::bottom_aligned` contract). `id` is a
/// stable, NON-localized key unique per engine (e.g. `"cleaning_flux2_klein"`); it keys the
/// remembered section height, the fold state and the body's widget ids. The section starts
/// collapsed.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn draw_postprocess_section(ui: &mut egui::Ui, id: &'static str, settings: &mut PostprocessSettings) -> bool {
    let mut changed = false;
    elastic::bottom_aligned(ui, (id, "postprocess"), |ui| {
        RegionEditToolBase::draw_region_editor_collapsible_section(ui, (id, "postprocess"), ms_region_postprocess::section_title(), false, |ui| {
            changed = ms_region_postprocess::draw_postprocess_body(ui, settings, (id, "postprocess_body"));
        });
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without input nothing changes, and a collapsed section does not touch the settings.
    #[test]
    fn an_untouched_section_reports_no_change() {
        let ctx = egui::Context::default();
        let mut settings = PostprocessSettings::default();
        let mut changed = true;
        for _ in 0..2 {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                changed = draw_postprocess_section(ui, "cleaning_postprocess_section_test", &mut settings);
            })
            .drop_without_applying_deltas();
        }
        assert!(!changed);
        assert_eq!(settings, PostprocessSettings::default());
    }
}
