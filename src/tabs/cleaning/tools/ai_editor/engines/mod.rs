/*
File: ai_editor/engines/mod.rs

Purpose:
The catalog of AI engines the «ИИ-редактор области» tool hosts. One submodule per engine,
each implementing `super::engine::AiEngine`; this file is the only place that knows which
engines exist and in which order the picker offers them.

Key functions:
- `all_engines()`: builds one instance of every engine, in picker order

Notes:
An engine is added by writing its module and adding ONE line to `all_engines`. The picker's
two sections come from `AiEngine::section`, not from this order, so the list stays a flat
catalog. Every engine is constructed eagerly, which is why an engine's constructor must not
do I/O on the GUI thread — FLUX.2 klein reads its settings file on a worker.

One MODULE may contribute several entries when the difference between them is a parameter
rather than an implementation: FLUX.2 klein contributes one per `config::Flux2Variant` (9B
and 4B), which is what keeps the second checkpoint from being a copy of the first module.
*/

mod flux2_klein;

use super::engine::AiEngine;
use crate::config::Flux2Variant;
use flux2_klein::Flux2KleinEngine;

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
#[must_use]
pub fn all_engines() -> Vec<Box<dyn AiEngine>> {
    Flux2Variant::all()
        .into_iter()
        .map(|variant| Box::new(Flux2KleinEngine::new(variant)) as Box<dyn AiEngine>)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every FLUX.2 klein variant reaches the picker, exactly once and under its own id.
    ///
    /// The ids are widget-id stems and log tokens: two engines sharing one would make the
    /// picker's selection ambiguous, and dropping a variant would leave a downloaded model
    /// with nothing to run it.
    #[test]
    fn the_catalog_offers_one_entry_per_flux2_variant() {
        let engines = all_engines();
        assert_eq!(engines.len(), Flux2Variant::all().len());
        // Pinned literals rather than a call back into the engine module: these ids are the
        // catalog's own contract, the 9B one is frozen at its historic value, and a test
        // that asked the engine what its id is could never catch it changing.
        assert_eq!(
            engines.iter().map(|engine| engine.id()).collect::<Vec<_>>(),
            ["flux2_klein", "flux2_klein_4b"],
            "the picker offers 9B then 4B, and the 9B id never changes"
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
}
