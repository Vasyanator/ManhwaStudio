/*
File: models/layer_model/mod.rs

Purpose:
Home of the unified layer model shared between the PS editor and the typing tab. The goal is one
notion of "layer" for both tabs: a normal raster layer and a text layer are the same node type,
differing only in metadata and in which operations they permit.

Two layer kinds (plus groups), per the agreed design:
- Normal (raster): pixels from any source — pasted, cut out of another layer, or rasterized text.
  Can be painted, deformed, cut, merged, transformed, and run through the effects render.
- Text: re-renderable from its text params (render type 1) and editable as text. Can be deformed,
  transformed, and run through the effects render. Cannot be painted / cut / merged without first
  rasterizing, because the next text render would discard those edits.
- Group: a folder of layers (future).

Two render types:
1. Text render — regenerates a text layer's base image from its parameters (Text only).
2. Effects render — applies a post-effects chain over a *preserved* base image; available to any
   non-group layer. This is why every effected layer stores its pre-effects base separately.

Rasterizing a text layer freezes its current render into a normal raster base, drops the text
params (no more render type 1), and keeps the effects chain and deform (render type 2 still works).

Phasing:
- Phase 1 (current): on-disk persistence of the PS editor's raster layers via `manifest` +
  `persist`. The capability rules already live on `manifest::LayerKindRec`. Text geometry stays in
  `text_images/text_info.json`; this model only references it.
- Phase 2+: a concrete in-memory `LayerNode` tree (groups, effects, text payloads), migrating the
  PS editor and typing tabs onto it. Until then `manifest` is the single source of truth for shape.
*/

pub mod compat;
pub mod effects;
pub mod layer_doc;
pub mod manifest;
pub mod migrate;
pub mod ordering;
pub mod persist;
pub mod text_payload;
