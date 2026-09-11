/*
File: cleaning/tools/ai_editor/engines/flux2_klein/test_support.rs

Purpose:
The fixtures the unit tests of this module are built from. It exists only under
`cfg(test)`: the tests of the engine, the panel, the pure decisions and the wire files
all need the same handful of settled settings and canned `.status` catalogs, and a
fixture duplicated per file is a fixture that drifts per file.

Main responsibilities:
- hand out settings that pass the run gate (`runnable_settings`) and settings whose
  encoder path lets the prompt-cache calls run (`cacheable_settings`);
- build a `.status` catalog that declares exactly the components a test names
  (`status_with_present`, `status_with_components`, `FLUX2_ALL_COMPONENTS`);
- build the frame geometry an engine test starts from (`region_rect`,
  `engine_with_settled_region`).

Key functions:
- `runnable_settings()`, `cacheable_settings()`
- `status_with_present()`, `status_with_components()`
- `region_rect()`, `engine_with_settled_region()`

Notes:
Not a test module itself — it declares no `#[test]`. Every item is `pub(super)` so the
`mod tests` of any sibling can reach it through the module root's re-export.
*/

use super::*;

/// A frame rectangle for the `set_region` rules below.
pub(super) fn region_rect(x: usize, y: usize, w: usize, h: usize) -> Option<OverlayRectPx> {
    Some(OverlayRectPx { x, y, w, h })
}

/// An engine whose forecast is disarmed and which already knows a 128x128 frame at the
/// origin, so each rule below starts from a settled, quiet state.
pub(super) fn engine_with_settled_region() -> Flux2KleinEngine {
    let mut engine = Flux2KleinEngine::default();
    engine.set_region(region_rect(0, 0, 128, 128), true);
    engine.estimate_wanted = false;
    engine.region_resize_pending = false;
    engine
}

/// Settings that pass every gate except the one under test, so a block reason a
/// test observes can only be the one it is asking about.
pub(super) fn runnable_settings() -> Flux2KleinSettings {
    Flux2KleinSettings {
        text_encoder_path: "/models/qwen3".to_string(),
        transformer_path: "/models/flux2.safetensors".to_string(),
        vae_path: "/models/vae".to_string(),
        prompt: "a clean background".to_string(),
        ..Flux2KleinSettings::default()
    }
}

/// A `.status` answer whose presence catalog can be dictated component by component.
///
/// `present` lists the components the backend reports as being on disk; everything
/// else parses as absent, which is exactly what a fresh machine answers.
pub(super) fn status_with_present(present: &[&str]) -> Flux2Status {
    let mut components = serde_json::Map::new();
    for name in ["text_encoder", "transformer", "vae", "tokenizer", "scheduler"] {
        components.insert(
            name.to_string(),
            json!({ "exists": present.contains(&name), "found": present.contains(&name) }),
        );
    }
    parse_flux2_status(&json!({ "available": true, "components": components }))
}

/// Every component the catalog knows, so `Ready` is reachable.
pub(super) const FLUX2_ALL_COMPONENTS: [&str; 5] = [
    "text_encoder",
    "transformer",
    "vae",
    "tokenizer",
    "scheduler",
];

/// Settings that pass every prompt-cache gate except the one under test.
pub(super) fn cacheable_settings() -> Flux2KleinSettings {
    Flux2KleinSettings {
        text_encoder_path: "/models/qwen3".to_string(),
        prompt: "remove the sfx".to_string(),
        ..Flux2KleinSettings::default()
    }
}

/// A `.status` answer carrying the full residency block of the pinned contract
/// (`dev-docs/flux2_component_residency.md` §3).
pub(super) fn status_with_components() -> Flux2Status {
    parse_flux2_status(&json!({
        "available": true,
        "components": {
            "text_encoder": { "residency": "ram", "actions": ["unload"] },
            "transformer": { "residency": "gpu", "actions": ["unload", "to_ram"] },
            "vae": { "residency": "offloaded", "actions": ["unload", "warmup"] },
        },
    }))
}
