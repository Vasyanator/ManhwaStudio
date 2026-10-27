# Module: crates/ms-sysprobe/src/ai_models/external_catalog

## Purpose
Submodules of `external_catalog.rs` (the pinned external-model catalog), one per large model
family: FLUX.2 klein 4B / 9B and FLUX.3 klein. The parent re-exports every public item, so
callers name `ms_sysprobe::ai_models::external_catalog::<item>`.

## Files and submodules
- `flux2_klein.rs`: the FLUX.2 klein statics (11 specs for 4B, 17 for 9B), the lookups from a
  typed engine selection (`ms_config::Flux2Variant` + `Flux2WeightAxis` + `Flux2EncoderSource` +
  `Flux2WeightFormat`) to specs and backend paths, the bf16 transformer layout rule, the spec
  role reverse lookup, and their tests (validation, totals, path tables, a sparse-file temp
  root for the layout rule). `FLUX2_KLEIN_SPECS` is appended by the parent's `all_specs()`.
- `flux3_klein.rs`: the FLUX.3 klein statics (12 specs), the lookups from a typed selection
  (`ms_config::Flux3DitFormat` + `Flux3EncoderFormat`) to specs and package-relative paths,
  the spec role reverse lookup, and their tests (validation, totals, gated flags, path
  agreement with `ms_config::flux3_klein_dir`, a sparse-file temp root for the adoption
  contract). `FLUX3_KLEIN_SPECS` is appended by the parent's `all_specs()`.

## Contracts and invariants
- Layout: every spec is `DirLayout::Shared` under `<side_models>/<Flux2Variant::dir_name()>`:
  the package root holds the base part and the bf16 files at exactly the paths the backend's
  former download wrote (`transformer/`, `text_encoder/`, for 9B also
  `flux-2-klein-9b.safetensors`); GGUF quants sit in `transformer_gguf/`, `text_encoder_gguf/`
  and (9B) `text_encoder_uncensored_gguf/`, one file per spec; the 9B uncensored F16 encoder
  sits in `text_encoder_uncensored/` (weights + two configs, no tokenizer: the base's official
  `tokenizer/` is used). Unmarked files there are adopted by hash.
- Offers come from `ms_config` (`Flux2WeightFormat::offered_for(variant, axis, source)`,
  `Flux2EncoderSource::offered_for(variant)`); `flux2_klein_spec` returns `Some` exactly for an
  offered combination (catalog test), and the transformer ignores `source`.
- 9B bf16 transformer: the download target is the BFL single file
  (`flux2_klein_9b_transformer_bf16`). `flux2_klein_9b_transformer_bf16_diffusers` (the legacy
  `transformer/` shards) is ADOPT-ONLY: `flux2_klein_spec` never returns it; it enters a bundle
  / path only through `Flux2TransformerLayout::LegacyDiffusers`, which only
  `flux2_klein_transformer_layout` decides (stat-only, worker thread): `Primary` if the single
  file reads `Installed`; else `LegacyDiffusers` if the legacy spec reads `Installed` or is
  present unmarked; else `Primary`. 4B has no legacy spec and is always `Primary`.
- Bundles (`flux2_klein_package_specs`): base, then — 9B with the STOCK encoder source only —
  `flux2_klein_9b_text_encoder_config` (`text_encoder/config.json` + `generation_config.json`,
  needed by the stock bf16 and GGUF encoder alike), then transformer, then encoder. The 9B base
  holds no encoder configs, so an uncensored-only 9B install (release 325 wrote no stock
  configs) adopts its base without a gated download. The 4B base keeps its encoder configs
  (an ungated repository, no uncensored source, and existing 4B markers stay valid: a changed
  file list would read as a marker of another identity, which is never adopted).
- A marker identity includes the spec's file list: changing a spec's files turns its existing
  marker into "another identity" (`Missing`, not adoptable); the next Download keeps every
  matching file by hash and transfers nothing for them.
- Gated: the BFL 9B and the uncensored-encoder repositories (`gated: true`); every other FLUX.2
  source is public. The access preflight itself is `ai_models::hf_access` + `external.rs`.
- The 4B and 9B bases pin byte-identical tokenizer, scheduler, VAE and `model_index.json`
  (test); the backend's package fallback relies on it.
- `flux2_klein_spec_role` searches the typed lookups (never a second table), so it cannot
  disagree with them; a transformer role always carries `source: Stock`.

- FLUX.3 layout: every spec is `DirLayout::Shared` under `<side_models>/FLUX.3-image-klein`
  (= `ms_config::flux3_klein_dir`), at exactly the repository paths the backend's former
  download wrote, so an existing install is adopted by hash. `flux3_klein_base` holds
  `ae.safetensors` and the `text_encoder/` licence, configs, tokenizer and preprocessor files (no
  README): exactly what the released downloader fetched for EVERY format. The root
  `config.json` is its own spec, `flux3_klein_dit_config` (read by the bf16 AND the GGUF DiT;
  one owner per file, test), because that downloader fetched it only for bf16 — in the base it
  would keep an old fp8r / nvfp4 install from adopting (regression test). DiT: `flux3_klein_dit_bf16` (root `model.safetensors` + `manifest.json`),
  `flux3_klein_dit_{fp8r,nvfp4}` (`variants/<format>/`), `flux3_klein_dit_gguf_<quant>` in
  `transformer_gguf/` (unsloth). Encoder: `flux3_klein_text_encoder_bf16` (shards + index in
  `text_encoder/`), `flux3_klein_text_encoder_gguf_{q8_0,q4_k_m}` in `text_encoder_gguf/`
  (official `Qwen/Qwen3-VL-4B-Instruct-GGUF`, text-only).
- FLUX.3 bundles (`flux3_klein_package_specs`): base, the root DiT config when the DiT reads
  it (`flux3_klein_dit_reads_root_config`: bf16 and GGUF), DiT, encoder. Paths
  (`flux3_klein_dit_relative_path`, `flux3_klein_encoder_relative_path`) are relative to the
  PACKAGE ROOT, so manual mode joins them onto the user's root; a test pins
  `flux3_klein_dir().join(rel)` to the spec's file (or, for the bf16 encoder, the folder
  holding every file of the spec).
- FLUX.3 gated: only the BFL repository (base, native DiTs, bf16 encoder); both GGUF
  repositories are public.

## Editing map
- New weight format / encoder source / variant: `ms_config::flux2_weight_format` first, then
  the statics and the exhaustive `flux2_klein_4b_weights_spec` / `flux2_klein_9b_weights_spec`
  matches, `FLUX2_KLEIN_SPECS`, the per-spec totals test and `tools/model_pins/sources.json`.
- Where a component is sent to the backend: `flux2_klein_weights_path` +
  `unquantized_component_path`.
- Which bf16 transformer layout an install uses: `flux2_klein_transformer_layout`.
- New FLUX.3 klein DiT or encoder format: `ms_config::flux3_weight_format` first, then the
  static, the exhaustive `flux3_klein_dit_spec` / `flux3_klein_encoder_spec` and
  `*_relative_path` / `flux3_klein_dit_reads_root_config` matches, `FLUX3_KLEIN_SPECS`, the totals test and
  `tools/model_pins/sources.json`.
