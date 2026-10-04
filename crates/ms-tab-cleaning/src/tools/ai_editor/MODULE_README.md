# Module: crates/ms-tab-cleaning/src/tools/ai_editor

## Purpose
The «ИИ-редактор области» cleaning tool, as a DESCRIPTOR: a `HostSpec` for the generic
region-editing host (`../region_edit_v2/host.rs`) plus the catalog of local AI engines it offers
(`engines/` — FLUX.2 klein 9B and 4B, Lama, SDXL Inpaint). Everything the tool DOES — the
on-canvas frame, the run and mask-generation pipelines, the apply path, both panel bodies — is
the host's and is documented in `../region_edit_v2/MODULE_README.md` ("The generic host").

## Architecture
```
tab.rs  -> tools::ai_editor_tool() -> ai_editor::tool() -> RegionEditHost::new(&AI_EDITOR_SPEC)
AI_EDITOR_SPEC.catalog = engines::all_engines   (index 0 is selected on creation)
```

## Files and submodules
- `mod.rs`: `AI_EDITOR_SPEC` (tool id `"ai_editor"`, title key
  `cleaning.tools.area_editor.title`, id salts `cleaning_ai_editor_mask_generation_section` /
  `cleaning_ai_editor_mask_source_picker`, log tag `[cleaning/ai_editor]`, catalog
  `engines::all_engines`), `tool()`, and the two tests that need THIS catalog (every engine is
  usable by the picker; a resting catalog never blocks a switch).
- `engines/`: the engine catalog — one module per engine plus `all_engines()`. Own
  `MODULE_README.md`.

## Contracts and invariants
- **The spec values are frozen.** The two id salts key egui's stored widget state (the folded
  mask-generation section, the source popup), so changing one resets that state; the tool id and
  log tag are what logs and the tool list are read by.
- **Nothing generic lives here.** A behaviour every hosted tool should share goes into
  `../region_edit_v2/host.rs` / `host_panels.rs`; an engine-specific one into `engines/`.
- **What is pushed when** (the host's rule, restated because engines here rely on it): an engine's
  `mask_layers()` reach the frame when the tool is built and when the engine is selected, NOT
  every frame; `constraints()` and `allows_empty_mask()` are re-read every frame. Every engine of
  this catalog answers all three with constants, and they DISAGREE on purpose: FLUX.2 klein allows
  an empty mask (its whole-region mode), Lama and SDXL Inpaint do not (their mask is the hole).
- **Size rules: only the original four.** Every engine here declares grid, minimum side and
  (FLUX.2 klein) maximum area and a symmetric aspect, as `..FrameConstraints::UNCONSTRAINED`
  updates; none uses the maximum side, area floor, size table or upscale allowance.
  `engines/mod.rs` sweeps every engine's declaration against the frozen legacy size rules
  (`../region_edit_v2/size_oracle.rs`), and that test fails if one starts using a new rule.

## Editing map
- To change an engine, or to add one: `engines/`, never here.
- To change the tool's id, title, salts or log tag: `AI_EDITOR_SPEC` in `mod.rs` (read the frozen
  rule above first).
- To change what the tool does with the frame, the picker, the panels or the run path:
  `../region_edit_v2/` (`host.rs`, `host_panels.rs`); to change what the host may ask of an
  engine: `../region_edit_v2/engine.rs`.
