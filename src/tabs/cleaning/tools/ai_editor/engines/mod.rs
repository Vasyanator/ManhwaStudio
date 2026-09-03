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
*/

mod flux2_klein;

use super::engine::AiEngine;
use flux2_klein::Flux2KleinEngine;

/// Builds one instance of every hosted engine, in the order the picker offers them.
///
/// Called once when the tool is created: the engines hold live channels and worker state, so
/// they are kept for the session rather than rebuilt per frame.
#[must_use]
pub fn all_engines() -> Vec<Box<dyn AiEngine>> {
    vec![Box::<Flux2KleinEngine>::default()]
}
