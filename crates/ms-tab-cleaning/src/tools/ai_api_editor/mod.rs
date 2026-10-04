/*
File: ai_api_editor/mod.rs

Purpose:
The «ИИ редактирование (API)» cleaning tool: a `HostSpec` for the generic region-editing host
(`../region_edit_v2/host.rs`) whose catalog is ONE engine, «Облачные модели» — hosted image-edit
models reached over their HTTP APIs through `ms_ai_api::image_edit`. The host owns the frame,
the run and mask-generation pipelines, the apply path and both panel bodies; this module says
which tool it is, and its engine owns the provider/model choice, the prompt, the mask blend,
the key block and the cloud run.

Key structures:
- `AI_API_EDITOR_SPEC`: the tool's id, title, egui id salts, log tag and catalog

Key functions:
- `tool()`: builds the tool, as the cleaning tab registers it

Submodules:
- `engine`: `CloudEditEngine`, the one hosted engine (state, `AiEngine`, panel, polling)
- `constraints`: the selected offer's `ImageSizeRule` mapped onto `FrameConstraints`
- `decisions`: the pure run gate
- `settings`: the persisted selection, prompt and blend, and their file IO
- `worker`: one run on a worker thread (key read, `run_image_edit`, result conversion)

Notes:
Torch-free and backend-free: the run needs only the network and the provider's key, so the tool
is offered without the AI backend. The spec values are frozen once shipped: the two id salts key
egui's stored widget state and must differ from every other hosted tool's.
*/

mod constraints;
mod decisions;
mod engine;
mod settings;
mod worker;

#[cfg(any(test, feature = "test-support"))]
pub use settings::suppress_settings_persistence_for_tests;

use super::region_edit_v2::engine::AiEngine;
use super::region_edit_v2::host::{HostSpec, RegionEditHost};

/// The «ИИ редактирование (API)» tool as the generic host sees it.
static AI_API_EDITOR_SPEC: HostSpec = HostSpec {
    tool_id: "ai_api_editor",
    title: ai_api_editor_title,
    log_tag: "[cleaning/ai_api_editor]",
    mask_generation_section_salt: "cleaning_ai_api_editor_mask_generation_section",
    mask_source_picker_salt: "cleaning_ai_api_editor_mask_source_picker",
    catalog: cloud_catalog,
};

/// The tool's localized name; a function because `t!` takes only a literal key.
fn ai_api_editor_title() -> &'static str {
    t!("cleaning.tools.ai_api_editor.title")
}

/// The tool's catalog: the single «Облачные модели» engine. One engine on purpose — the
/// provider and model are parameters of it, so a model switch never re-creates the mask stack,
/// and the host hides its engine picker for a one-engine catalog.
fn cloud_catalog() -> Vec<Box<dyn AiEngine>> {
    vec![Box::new(engine::CloudEditEngine::new())]
}

/// Builds the «ИИ редактирование (API)» tool: a region-editing host over the cloud engine.
#[must_use]
pub fn tool() -> RegionEditHost {
    RegionEditHost::new(&AI_API_EDITOR_SPEC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::CleaningTool;

    /// The catalog is exactly one engine, Torch-free, with one mask layer whose name the catalog
    /// resolves; the tool is offered without Torch and never blocks canvas zoom (D5).
    #[test]
    fn the_tool_hosts_one_torch_free_cloud_engine() {
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");
        let engines = (AI_API_EDITOR_SPEC.catalog)();
        assert_eq!(engines.len(), 1, "the provider and model are parameters of ONE engine");
        let engine = &engines[0];
        assert_eq!(engine.id(), "cloud_edit");
        assert!(!engine.requires_torch());
        assert!(engine.allows_empty_mask(), "an empty mask means the whole region");
        assert!(engine.switch_block_reason().is_none());
        let layers = engine.mask_layers();
        assert_eq!(layers.len(), 1);
        assert!(ms_i18n::lookup(layers[0].label_key).is_some(), "unknown key {}", layers[0].label_key);

        let tool = tool();
        assert_eq!(tool.tool_id(), "ai_api_editor");
        assert!(!tool.title().is_empty());
        assert!(!tool.pytorch_required(), "the tool button must not be gated on Torch");
        assert!(!tool.block_canvas_zoom(), "D5: blocking is precise, never the whole canvas");
        assert!(!tool.block_canvas_zoom_on_ctrl_primary());
    }

    /// Two hosted tools sharing a salt would share egui's stored widget state.
    #[test]
    fn the_spec_values_are_distinct_from_the_ai_editor() {
        assert_ne!(AI_API_EDITOR_SPEC.tool_id, "ai_editor");
        assert_ne!(AI_API_EDITOR_SPEC.mask_generation_section_salt, "cleaning_ai_editor_mask_generation_section");
        assert_ne!(AI_API_EDITOR_SPEC.mask_source_picker_salt, "cleaning_ai_editor_mask_source_picker");
    }
}
