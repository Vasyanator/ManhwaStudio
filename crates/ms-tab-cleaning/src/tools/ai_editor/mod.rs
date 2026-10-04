/*
File: ai_editor/mod.rs

Purpose:
The «ИИ-редактор области» cleaning tool: a `HostSpec` for the generic region-editing host
(`../region_edit_v2/host.rs`) plus the catalog of local AI engines it offers (`engines/`). The
host owns the frame, the run and mask-generation pipelines, the apply path and both panel
bodies; this module only says which tool it is and which engines it hosts.

Key structures:
- `AI_EDITOR_SPEC`: the tool's id, title, egui id salts, log tag and catalog

Key functions:
- `tool()`: builds the tool, as the cleaning tab registers it

Submodules:
- `engines`: the engine catalog, one module per engine

Notes:
The spec values are frozen: the two id salts key egui's stored widget state (the folded
mask-generation section and its source popup), so changing one silently resets that state for
every user. Design: `dev-docs/region_edit_v2_plan.md` (§13).
*/

mod engines;

use super::region_edit_v2::host::{HostSpec, RegionEditHost};

/// The «ИИ-редактор области» tool as the generic host sees it.
static AI_EDITOR_SPEC: HostSpec = HostSpec {
    tool_id: "ai_editor",
    title: ai_editor_title,
    log_tag: "[cleaning/ai_editor]",
    mask_generation_section_salt: "cleaning_ai_editor_mask_generation_section",
    mask_source_picker_salt: "cleaning_ai_editor_mask_source_picker",
    catalog: engines::all_engines,
};

/// The tool's localized name; a function because `t!` takes only a literal key.
fn ai_editor_title() -> &'static str {
    t!("cleaning.tools.area_editor.title")
}

/// Builds the «ИИ-редактор области» tool: a region-editing host over the local engine catalog.
#[must_use]
pub fn tool() -> RegionEditHost {
    RegionEditHost::new(&AI_EDITOR_SPEC)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog must not be empty, every engine must name itself uniquely, and every mask
    /// layer it declares must have a name the catalog can resolve — the picker and the frame's
    /// layer chips both show those strings.
    #[test]
    fn every_hosted_engine_is_usable_by_the_picker() {
        // The assertion below reads a UI string, and `t!` / `tf!` / `ms_i18n::lookup`
        // answer against the PROCESS-GLOBAL active catalog: with none installed they
        // degrade to the bare key. Install the reference catalog under the shared lock
        // (the catalog slot is one `ArcSwap`, so tests must serialize on it).
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");
        // The spec's own catalog, which is exactly what the host builds its engines from.
        let engines = (AI_EDITOR_SPEC.catalog)();
        assert!(!engines.is_empty(), "the picker would have nothing to offer");
        let mut ids: Vec<&str> = engines.iter().map(|engine| engine.id()).collect();
        ids.sort_unstable();
        let unique = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), unique, "engine ids are used as widget id stems and must be unique");
        for engine in &engines {
            assert!(!engine.title().is_empty(), "engine {} has no picker caption", engine.id());
            let layers = engine.mask_layers();
            assert!(!layers.is_empty(), "engine {} declares no mask layer", engine.id());
            for layer in &layers {
                assert!(
                    ms_i18n::lookup(layer.label_key).is_some(),
                    "engine {} names a mask layer with an unknown key {}",
                    engine.id(),
                    layer.label_key
                );
            }
        }
    }

    /// The default trait body is `None`: an engine that owns no such work must not have to
    /// implement the hook, and every real engine in the catalog is switchable at rest.
    #[test]
    fn a_resting_catalog_never_blocks_the_picker() {
        for engine in &(AI_EDITOR_SPEC.catalog)() {
            assert!(
                engine.switch_block_reason().is_none(),
                "engine {} refuses a switch while it is doing nothing",
                engine.id()
            );
        }
    }
}
