/*
File: ai_editor/engines/mod.rs

Purpose:
The catalog of AI engines the «ИИ-редактор области» tool hosts. One submodule per engine,
each implementing `region_edit_v2::engine::AiEngine`; this file is the only place that knows
which engines exist and in which order the picker offers them.

Key functions:
- `all_engines()`: builds one instance of every engine, in picker order (the
  `HostSpec::catalog` of `super::AI_EDITOR_SPEC`)

Notes:
An engine is added by writing its module and adding ONE line to `all_engines`. The picker's
two sections come from `AiEngine::section`, not from this order, so the list stays a flat
catalog — «Lama» is next to last here and still draws FIRST on screen, because its section
(«Без промпта») is drawn before «С промптом». The order does decide one thing: the engine at
index 0 is the one selected when the tool is created, which is why FLUX.2 klein stays at the
head of the list.

Every engine is constructed eagerly, which is why an engine's constructor must not do I/O on
the GUI thread — every engine here reads its settings file on a worker.

One MODULE may contribute several entries when the difference between them is a parameter
rather than an implementation: FLUX.2 klein contributes one per `config::Flux2Variant` (9B
and 4B), which is what keeps the second checkpoint from being a copy of the first module.
The `lama` and `sdxl` modules take the opposite shape — ONE entry each, offering four
checkpoints behind two backend methods and two channel modes behind one backend method
respectively — because the choice there is a parameter of a single engine, not a separate
picker entry per model.

`lama` also exports the LaMa-v2 model catalog to its sibling `sdxl`, whose 4-channel mode
prefills the hole with LaMa before generating. That is the only cross-engine dependency in
this subtree, and it is one-directional.
*/

mod flux2_klein;
// Private, like every other engine: the LaMa model catalog it exports reaches only its
// sibling `sdxl`, whose 4-channel prefill sends a checkpoint file name to the backend.
mod lama;
mod sdxl;

use crate::tools::region_edit_v2::engine::AiEngine;
use ms_config::Flux2Variant;
use flux2_klein::Flux2KleinEngine;
use lama::LamaEngine;
use sdxl::SdxlEngine;

/// Builds one instance of every hosted engine, in the order the picker offers them.
///
/// Called once when the tool is created: the engines hold live channels and worker state, so
/// they are kept for the session rather than rebuilt per frame.
///
/// FLUX.2 klein appears TWICE — once per `Flux2Variant`. The two entries are the same
/// implementation parameterized by the checkpoint, not two modules: the variant keys the
/// engine id, the picker caption, the settings file and the model directory. Only one of
/// them can be resident in the backend at a time (one pipeline, keyed by the component
/// paths), which the panel says and this side does not orchestrate.
///
/// «Lama» appears ONCE and offers its four checkpoints inside its own panel; the two backend
/// methods behind them are its business, not the picker's. «SDXL Inpaint» likewise appears
/// ONCE and offers its two channel modes inside its own panel — the mode is a parameter of
/// one `inpaint.sdxl` method, not a second implementation.
#[must_use]
pub(super) fn all_engines() -> Vec<Box<dyn AiEngine>> {
    let mut engines: Vec<Box<dyn AiEngine>> = Flux2Variant::all()
        .into_iter()
        .map(|variant| Box::new(Flux2KleinEngine::new(variant)) as Box<dyn AiEngine>)
        .collect();
    engines.push(Box::new(LamaEngine::new()));
    engines.push(Box::new(SdxlEngine::new()));
    engines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every engine reaches the picker, exactly once and under its own id.
    ///
    /// The ids are widget-id stems and log tokens: two engines sharing one would make the
    /// picker's selection ambiguous, and dropping an entry would leave a downloaded model
    /// with nothing to run it.
    #[test]
    fn the_catalog_offers_every_engine_exactly_once() {
        let engines = all_engines();
        assert_eq!(engines.len(), Flux2Variant::all().len() + 2);
        // Pinned literals rather than a call back into the engine modules: these ids are
        // the catalog's own contract, the 9B one is frozen at its historic value, and a
        // test that asked the engine what its id is could never catch it changing.
        assert_eq!(
            engines.iter().map(|engine| engine.id()).collect::<Vec<_>>(),
            ["flux2_klein", "flux2_klein_4b", "lama", "sdxl"],
            "FLUX.2 klein 9B stays at index 0 — it is the engine selected on creation"
        );
        for engine in &engines {
            assert!(!engine.title().is_empty());
        }
        let mut ids: Vec<&str> = engines.iter().map(|engine| engine.id()).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), total, "two engines share an id");
    }

    /// Every hosted engine's frame behaves exactly as it did before `FrameConstraints` grew
    /// the max-side, min-area, aspect-pair, table and upscale rules: validity, snapping and
    /// page fit are swept against the frozen legacy copy. `legacy_of` also fails the test if an
    /// engine starts declaring one of the new rules, which is a behaviour change this guard
    /// must then be told about.
    #[test]
    fn every_engine_frame_answers_exactly_like_the_legacy_size_rules() {
        for engine in all_engines() {
            crate::tools::region_edit_v2::size_oracle::assert_equivalent_to_legacy(&engine.constraints(), engine.id());
        }
    }
}
