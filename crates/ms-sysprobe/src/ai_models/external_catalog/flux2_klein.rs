/*
File: crates/ms-sysprobe/src/ai_models/external_catalog/flux2_klein.rs

Purpose:
The FLUX.2 klein part of the pinned external catalog: the 4B and 9B packages (BFL bf16 files,
the unsloth GGUF transformer and Qwen3 text-encoder quants, and for 9B the gated uncensored
text encoder with its GGUF quants) and the lookups that turn a typed engine selection into
specs and backend paths. The parent `external_catalog.rs` re-exports every public item, so
callers keep naming `external_catalog::<item>`.

Key items:
- `FLUX2_KLEIN_4B_*` (12 specs, one of them the BFL single-file bf16 transformer
  `FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE` that only the sd.cpp bundle uses) and
  `FLUX2_KLEIN_9B_*` (17 specs, one of them the ADOPT-ONLY legacy diffusers transformer
  `FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS`, one the stock encoder's two configs
  `FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG`): the statics.
- `flux2_klein_spec`: the download target of one component (variant, axis, encoder source,
  format); never the legacy spec.
- `flux2_klein_legacy_transformer_spec`, `Flux2TransformerLayout`,
  `flux2_klein_transformer_layout`: the one rule for which bf16 transformer layout a 9B
  install uses (the BFL single file, or a legacy diffusers `transformer/` folder).
- `flux2_klein_package_specs`: the bundle (base [+ 9B stock encoder configs] + transformer +
  encoder) a selection needs.
- `flux2_klein_weights_path`: the bf16 file / folder or GGUF file the backend is sent.
- `flux2_klein_package_specs_for` / `flux2_klein_sdcpp_paths`: the bundle per
  `ms_config::ImageEditRuntime` and the three files Stable-Diffusion.cpp loads (diffusion
  model, VAE, LLM text encoder).
- `Flux2SpecRole` + `flux2_klein_spec_role`: reverse lookup spec id -> typed role (display
  names in the UI).

Notes:
- Every spec is `DirLayout::Shared` under `<side_models>/<Flux2Variant::dir_name()>`; the bf16
  paths are exactly the ones the backend's former download wrote, so an existing install is
  adopted by hash, never refetched.
- The offered formats and encoder sources are `ms_config`'s (`Flux2WeightFormat::offered_for`,
  `Flux2EncoderSource::offered_for`); this file only maps them to pins. The exhaustive
  per-variant matches force a spec for every new format.
- Pins: the Hugging Face tree API at each commit (gated repositories read with a token); small
  files hashed after download (git blob sha1 equal to the tree's).
- sd.cpp file rule (measured against release `master-1017-be48649`): the diffusers
  `transformer/` folder does NOT load there (detected as "Flux Control"), so the sd.cpp bf16
  transformer is always the BFL single file (4B: its own spec, 9B: the primary download target).
  Everything else maps 1:1 onto the PyTorch files: the diffusers `vae/` weights, the encoder's
  `model.safetensors.index.json` (stock) or `model.safetensors` (uncensored), any GGUF.
*/

use std::path::{Path, PathBuf};

use ms_config::{Flux2EncoderSource, Flux2Variant, Flux2WeightAxis, Flux2WeightFormat, ImageEditRuntime};

use super::{SdCppComponentPaths, join_relative};
use crate::ai_models::external::{DirLayout, ExternalFile, ExternalModelSpec, ExternalModelStatus, ExternalSource, external_model_status, unmarked_files_present};

// FLUX.2 klein 4B. The three BFL specs publish into the package root `FLUX.2-klein-4B` (the
// same relative paths `ms_config::flux2_klein_*_dir` name and the backend's former download
// produced); each GGUF component has a sibling folder holding one file per quant. The BFL
// repository carries more (sample images); only the diffusers folders the PyTorch engine loads
// and the repo-root single-file checkpoint the sd.cpp runtime loads are listed.

/// The FLUX.2 klein 4B package root, relative to `side_models`
/// (`ms_config::Flux2Variant::Klein4B.dir_name()`; a catalog test checks the agreement).
const FLUX2_KLEIN_4B_DIR: &str = "FLUX.2-klein-4B";
/// The BFL repository and commit every FLUX.2 klein 4B bf16 file is pinned at.
const FLUX2_KLEIN_4B_BFL_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "black-forest-labs/FLUX.2-klein-4B", revision: "e7b7dc27f91deacad38e78976d1f2b499d76a294", gated: false };
/// The unsloth GGUF quantizations of the FLUX.2 klein 4B transformer.
const FLUX2_KLEIN_4B_TRANSFORMER_GGUF_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "unsloth/FLUX.2-klein-4B-GGUF", revision: "0084d1df98e2e2137fe776d55170bc4792ec1d66", gated: false };
/// The unsloth GGUF quantizations of Qwen3-4B, the FLUX.2 klein 4B text encoder.
const FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "unsloth/Qwen3-4B-GGUF", revision: "22c9fc8a8c7700b76a1789366280a6a5a1ad1120", gated: false };
/// Folder of the 4B transformer GGUF files, relative to `side_models`.
const FLUX2_KLEIN_4B_TRANSFORMER_GGUF_DIR: &str = "FLUX.2-klein-4B/transformer_gguf";
/// Folder of the 4B text-encoder GGUF files, relative to `side_models`.
const FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_DIR: &str = "FLUX.2-klein-4B/text_encoder_gguf";
/// The diffusers transformer folder inside a package root (also the legacy 9B layout).
const FLUX2_KLEIN_TRANSFORMER_SUBDIR: &str = "transformer";
/// The stock bf16 text-encoder folder inside a package root.
const FLUX2_KLEIN_TEXT_ENCODER_SUBDIR: &str = "text_encoder";
/// The BFL single-file 4B transformer, relative to the package root.
const FLUX2_KLEIN_4B_TRANSFORMER_SINGLE_FILE: &str = "flux-2-klein-4b.safetensors";
/// The VAE weights inside a package root (pinned by both bases; the file sd.cpp loads).
const FLUX2_KLEIN_VAE_WEIGHTS: &str = "vae/diffusion_pytorch_model.safetensors";
/// The stock bf16 encoder's shard index inside a package root: the file sd.cpp is given (it
/// reads the shards the index names).
const FLUX2_KLEIN_TEXT_ENCODER_INDEX: &str = "text_encoder/model.safetensors.index.json";
/// The uncensored unquantized encoder's single weights file inside its spec directory.
const FLUX2_KLEIN_UNCENSORED_ENCODER_WEIGHTS: &str = "model.safetensors";

// Everything the engine needs whatever the weight formats: licence, pipeline index, scheduler,
// tokenizer, VAE, and the two component configs (a GGUF transformer / encoder takes its config
// and tokenizer from here).
static FLUX2_KLEIN_4B_BASE_FILES: [ExternalFile; 15] = [
    ExternalFile::direct("LICENSE.md", 9_584, "ca02bc51900ab07789d1b70283329e7137f5af98f5161c23a1c81fc38a4af1fe"),
    ExternalFile::direct("model_index.json", 446, "51a76cb1cf3ed37423a1128c79c22faee8e6fbe7f5aaeb737f0a258930dbaac0"),
    ExternalFile::direct("scheduler/scheduler_config.json", 486, "067afb012cef64553a763447d1efd93daeffcc0123ca7e25b09f8de20b90762e"),
    ExternalFile::direct("tokenizer/added_tokens.json", 707, "c0284b582e14987fbd3d5a2cb2bd139084371ed9acbae488829a1c900833c680"),
    ExternalFile::direct("tokenizer/chat_template.jinja", 4_168, "a55ee1b1660128b7098723e0abcd92caa0788061051c62d51cbe87d9cf1974d8"),
    ExternalFile::direct("tokenizer/merges.txt", 1_671_853, "8831e4f1a044471340f7c0a83d7bd71306a5b867e95fd870f74d0c5308a904d5"),
    ExternalFile::direct("tokenizer/special_tokens_map.json", 613, "76862e765266b85aa9459767e33cbaf13970f327a0e88d1c65846c2ddd3a1ecd"),
    ExternalFile::direct("tokenizer/tokenizer.json", 11_422_654, "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4"),
    ExternalFile::direct("tokenizer/tokenizer_config.json", 5_404, "443bfa629eb16387a12edbf92a76f6a6f10b2af3b53d87ba1550adfcf45f7fa0"),
    ExternalFile::direct("tokenizer/vocab.json", 2_776_833, "ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910"),
    ExternalFile::direct("vae/config.json", 821, "0d6dfb69ae95a5e2ac9836284bbb63d8b38ce67b25ba2dff380752b2a10ab948"),
    ExternalFile::direct("vae/diffusion_pytorch_model.safetensors", 168_120_878, "ca70d2202afe6415bdbcb8793ba8cd99fd159cfe6192381504d6c4d3036e0f04"),
    ExternalFile::direct("transformer/config.json", 541, "09733c74a3da6d17dd0a0472a091a8950c7c6935889c32c16cc800ede05029de"),
    ExternalFile::direct("text_encoder/config.json", 1_536, "214b4c29a0d975e9fddf9994a5673f22cb2c4c5750352f9227c2c3251ebeab40"),
    ExternalFile::direct("text_encoder/generation_config.json", 214, "4347b1aeed2b2b78bc059920a0b7f5fec71482e1344952b76d7665d638d71f13"),
];

/// FLUX.2 klein 4B shared part (`black-forest-labs/FLUX.2-klein-4B`): licence, pipeline index,
/// scheduler, tokenizer, VAE and the transformer / text-encoder configs; no component weights.
pub static FLUX2_KLEIN_4B_BASE: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_base",
    source: FLUX2_KLEIN_4B_BFL_SOURCE,
    dir: FLUX2_KLEIN_4B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_BASE_FILES,
};

static FLUX2_KLEIN_4B_TRANSFORMER_BF16_FILES: [ExternalFile; 1] = [
    ExternalFile::direct("transformer/diffusion_pytorch_model.safetensors", 7_751_109_744, "9f29f9edcfdae452a653ffb51a534ca4decd389952c225724ff3b94042612a6e"),
];

/// FLUX.2 klein 4B bf16 transformer weights (about 7.8 GB) into `transformer/`.
pub static FLUX2_KLEIN_4B_TRANSFORMER_BF16: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_transformer_bf16",
    source: FLUX2_KLEIN_4B_BFL_SOURCE,
    dir: FLUX2_KLEIN_4B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TRANSFORMER_BF16_FILES,
};

static FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE_FILES: [ExternalFile; 1] = [
    ExternalFile::direct(FLUX2_KLEIN_4B_TRANSFORMER_SINGLE_FILE, 7_751_105_712, "ec3d4e733a771f61c052fb4856c48b336c55eaf2c65487c2a1faeb9bbda7a343"),
];

/// FLUX.2 klein 4B bf16 transformer as the BFL single file (BFL key layout, about 7.8 GB) at
/// the package root: the same weights as [`FLUX2_KLEIN_4B_TRANSFORMER_BF16`] in the layout
/// Stable-Diffusion.cpp loads (it rejects the diffusers `transformer/` file). Only the sd.cpp
/// bundle ([`flux2_klein_package_specs_for`]) contains it; [`flux2_klein_spec`] never returns it.
pub static FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_transformer_bf16_single",
    source: FLUX2_KLEIN_4B_BFL_SOURCE,
    dir: FLUX2_KLEIN_4B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE_FILES,
};

static FLUX2_KLEIN_4B_TEXT_ENCODER_BF16_FILES: [ExternalFile; 3] = [
    ExternalFile::direct("text_encoder/model-00001-of-00002.safetensors", 4_967_215_360, "8c0506e7f4936fa7e26183a4fd8da4e2bdbc5990ba64ae441f965d51228f36ea"),
    ExternalFile::direct("text_encoder/model-00002-of-00002.safetensors", 3_077_766_632, "82f2bd839378541b0557bfabaf37c7d3d637071fdcb73302dedd7cf61162ce07"),
    ExternalFile::direct("text_encoder/model.safetensors.index.json", 32_855, "06b3d5319b6d76d1a4a2433419180016cfd54ed62d086a5e6567a809f8c82634"),
];

/// FLUX.2 klein 4B bf16 Qwen3 text-encoder weights (two shards + index, about 8 GB) into
/// `text_encoder/`.
pub static FLUX2_KLEIN_4B_TEXT_ENCODER_BF16: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_text_encoder_bf16",
    source: FLUX2_KLEIN_4B_BFL_SOURCE,
    dir: FLUX2_KLEIN_4B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TEXT_ENCODER_BF16_FILES,
};

static FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q8_0_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-2-klein-4b-Q8_0.gguf", 4_300_644_928, "24a812e8f9b640e21c784164ea48571d8017ca22873696f4badeeabb006d509c")];
static FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q6_K_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-2-klein-4b-Q6_K.gguf", 3_409_273_408, "fb3b5430d9229f982616c7db084deff1db5035d0cb6558f2d4f704cfe5e609e2")];
static FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q5_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-2-klein-4b-Q5_K_M.gguf", 3_073_368_640, "58c01c75fee2272eadb127f53e26dd05a5a8e7f812e37c36fa44603301f91e54")];
static FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q4_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-2-klein-4b-Q4_K_M.gguf", 2_604_311_104, "0b25d143c8469b342bc5af3bce92b783bf6b0636d285f7b2f75e38af63af9a15")];

/// FLUX.2 klein 4B transformer, GGUF `Q8_0` (`unsloth/FLUX.2-klein-4B-GGUF`).
pub static FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q8_0: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_transformer_gguf_q8_0",
    source: FLUX2_KLEIN_4B_TRANSFORMER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_4B_TRANSFORMER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q8_0_FILES,
};

/// FLUX.2 klein 4B transformer, GGUF `Q6_K` (`unsloth/FLUX.2-klein-4B-GGUF`).
pub static FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q6_K: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_transformer_gguf_q6_k",
    source: FLUX2_KLEIN_4B_TRANSFORMER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_4B_TRANSFORMER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q6_K_FILES,
};

/// FLUX.2 klein 4B transformer, GGUF `Q5_K_M` (`unsloth/FLUX.2-klein-4B-GGUF`).
pub static FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q5_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_transformer_gguf_q5_k_m",
    source: FLUX2_KLEIN_4B_TRANSFORMER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_4B_TRANSFORMER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q5_K_M_FILES,
};

/// FLUX.2 klein 4B transformer, GGUF `Q4_K_M` (`unsloth/FLUX.2-klein-4B-GGUF`).
pub static FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q4_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_transformer_gguf_q4_k_m",
    source: FLUX2_KLEIN_4B_TRANSFORMER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_4B_TRANSFORMER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q4_K_M_FILES,
};

static FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q8_0_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3-4B-Q8_0.gguf", 4_280_405_792, "eed555233267a33c7e8ee31682762cc7751b3f6d224039086e0e846f05fffa5d")];
static FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q6_K_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3-4B-Q6_K.gguf", 3_306_261_792, "002b8b61c298976afc2c9c3ca65f9dbb28deba22c4b6381853dfaa92b5690022")];
static FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q5_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3-4B-Q5_K_M.gguf", 2_889_514_272, "37edbd37f5b7a4d4e7a766400c57def1f790e809de03513b86bf11bb06979695")];
static FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q4_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3-4B-Q4_K_M.gguf", 2_497_281_312, "f6f851777709861056efcdad3af01da38b31223a3ba26e61a4f8bf3a2195813a")];

/// FLUX.2 klein 4B text encoder, Qwen3-4B GGUF `Q8_0` (`unsloth/Qwen3-4B-GGUF`).
pub static FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q8_0: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_text_encoder_gguf_q8_0",
    source: FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q8_0_FILES,
};

/// FLUX.2 klein 4B text encoder, Qwen3-4B GGUF `Q6_K` (`unsloth/Qwen3-4B-GGUF`).
pub static FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q6_K: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_text_encoder_gguf_q6_k",
    source: FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q6_K_FILES,
};

/// FLUX.2 klein 4B text encoder, Qwen3-4B GGUF `Q5_K_M` (`unsloth/Qwen3-4B-GGUF`).
pub static FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q5_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_text_encoder_gguf_q5_k_m",
    source: FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q5_K_M_FILES,
};

/// FLUX.2 klein 4B text encoder, Qwen3-4B GGUF `Q4_K_M` (`unsloth/Qwen3-4B-GGUF`).
pub static FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q4_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_4b_text_encoder_gguf_q4_k_m",
    source: FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q4_K_M_FILES,
};

// FLUX.2 klein 9B. Same scheme as 4B, with four differences: the BFL repository is GATED; the
// bf16 transformer is the BFL single file `flux-2-klein-9b.safetensors` at the package root
// (one 18 GB file instead of the diffusers shards), while the diffusers `transformer/` shards
// the backend's former download wrote stay ADOPTABLE through a spec that is never offered as
// a download; and there is a second, gated encoder source (the uncensored Qwen3-8B fine-tune,
// F16 safetensors + three GGUF quants); and the stock encoder's `text_encoder/` configs are a
// spec of their own (`FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG`), in the bundle only for the stock
// source, so an uncensored-only install (release 325 wrote no stock configs) adopts its base.
// The uncensored bf16 spec pins only the weights and the two configs: a fresh install
// tokenizes with the official `tokenizer/` of the base.

/// The FLUX.2 klein 9B package root, relative to `side_models`
/// (`ms_config::Flux2Variant::Klein9B.dir_name()`; a catalog test checks the agreement).
const FLUX2_KLEIN_9B_DIR: &str = "FLUX.2-klein-9B";
/// The gated BFL repository and commit every FLUX.2 klein 9B bf16 file is pinned at.
const FLUX2_KLEIN_9B_BFL_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "black-forest-labs/FLUX.2-klein-9B", revision: "92196c8e11f7b6cf2b7493e037d8c5345c559216", gated: true };
/// The unsloth GGUF quantizations of the FLUX.2 klein 9B transformer.
const FLUX2_KLEIN_9B_TRANSFORMER_GGUF_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "unsloth/FLUX.2-klein-9B-GGUF", revision: "fde8634245fe6b749a221c25b34672b5b8fbd079", gated: false };
/// The unsloth GGUF quantizations of Qwen3-8B, the stock FLUX.2 klein 9B text encoder.
const FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "unsloth/Qwen3-8B-GGUF", revision: "a6adef130ffb23ddaf1a62fec9dced968c9bc482", gated: false };
/// The gated community repository of the uncensored 9B text encoder (F16 safetensors + GGUF).
const FLUX2_KLEIN_9B_UNCENSORED_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "ponpoke/flux2-klein-9b-uncensored-text-encoder", revision: "fba36e796aac081246708dd30392a401ba44922e", gated: true };
/// Folder of the 9B transformer GGUF files, relative to `side_models`.
const FLUX2_KLEIN_9B_TRANSFORMER_GGUF_DIR: &str = "FLUX.2-klein-9B/transformer_gguf";
/// Folder of the 9B stock text-encoder GGUF files, relative to `side_models`.
const FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_DIR: &str = "FLUX.2-klein-9B/text_encoder_gguf";
/// Folder of the unquantized uncensored encoder, relative to `side_models` (the folder the
/// backend's former download used); the spec's files sit directly in it.
const FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_DIR: &str = "FLUX.2-klein-9B/text_encoder_uncensored";
/// Folder of the uncensored encoder GGUF files, relative to `side_models`.
const FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_DIR: &str = "FLUX.2-klein-9B/text_encoder_uncensored_gguf";
/// The BFL single-file 9B transformer, relative to the package root.
const FLUX2_KLEIN_9B_TRANSFORMER_SINGLE_FILE: &str = "flux-2-klein-9b.safetensors";

// The 9B base part. Tokenizer, scheduler, VAE and pipeline index are byte-identical to the
// 4B ones (a test pins that; the backend's package fallback relies on it); the licence and
// the transformer config differ. The stock text-encoder configs are NOT here (see
// `FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG`).
static FLUX2_KLEIN_9B_BASE_FILES: [ExternalFile; 13] = [
    ExternalFile::direct("LICENSE.md", 18_158, "468d9f4332c0c895e9132c035982a40a12750092603a2e910913cb49aa887d3b"),
    ExternalFile::direct("model_index.json", 446, "51a76cb1cf3ed37423a1128c79c22faee8e6fbe7f5aaeb737f0a258930dbaac0"),
    ExternalFile::direct("scheduler/scheduler_config.json", 486, "067afb012cef64553a763447d1efd93daeffcc0123ca7e25b09f8de20b90762e"),
    ExternalFile::direct("tokenizer/added_tokens.json", 707, "c0284b582e14987fbd3d5a2cb2bd139084371ed9acbae488829a1c900833c680"),
    ExternalFile::direct("tokenizer/chat_template.jinja", 4_168, "a55ee1b1660128b7098723e0abcd92caa0788061051c62d51cbe87d9cf1974d8"),
    ExternalFile::direct("tokenizer/merges.txt", 1_671_853, "8831e4f1a044471340f7c0a83d7bd71306a5b867e95fd870f74d0c5308a904d5"),
    ExternalFile::direct("tokenizer/special_tokens_map.json", 613, "76862e765266b85aa9459767e33cbaf13970f327a0e88d1c65846c2ddd3a1ecd"),
    ExternalFile::direct("tokenizer/tokenizer.json", 11_422_654, "aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4"),
    ExternalFile::direct("tokenizer/tokenizer_config.json", 5_404, "443bfa629eb16387a12edbf92a76f6a6f10b2af3b53d87ba1550adfcf45f7fa0"),
    ExternalFile::direct("tokenizer/vocab.json", 2_776_833, "ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910"),
    ExternalFile::direct("vae/config.json", 821, "0d6dfb69ae95a5e2ac9836284bbb63d8b38ce67b25ba2dff380752b2a10ab948"),
    ExternalFile::direct("vae/diffusion_pytorch_model.safetensors", 168_120_878, "ca70d2202afe6415bdbcb8793ba8cd99fd159cfe6192381504d6c4d3036e0f04"),
    ExternalFile::direct("transformer/config.json", 542, "e82d0d325aff03c3b3b33a1634c47a5f88867478f53071b9c9a39c99010c5d46"),
];

/// FLUX.2 klein 9B shared part (`black-forest-labs/FLUX.2-klein-9B`, gated): licence, pipeline
/// index, scheduler, tokenizer, VAE and the transformer config; no weights and no text-encoder
/// configs (those are [`FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG`], needed by the stock source only).
pub static FLUX2_KLEIN_9B_BASE: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_base",
    source: FLUX2_KLEIN_9B_BFL_SOURCE,
    dir: FLUX2_KLEIN_9B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_BASE_FILES,
};

static FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG_FILES: [ExternalFile; 2] = [
    ExternalFile::direct("text_encoder/config.json", 1_538, "57866e90a1d6328a7ed53eca732bce106f86c76ab99d7629e01f0a319fa57998"),
    ExternalFile::direct("text_encoder/generation_config.json", 214, "4347b1aeed2b2b78bc059920a0b7f5fec71482e1344952b76d7665d638d71f13"),
];

/// FLUX.2 klein 9B stock text-encoder configs (`black-forest-labs/FLUX.2-klein-9B`, gated):
/// `text_encoder/config.json` + `generation_config.json`. Part of every 9B bundle with the
/// STOCK encoder source, bf16 or GGUF (a GGUF encoder takes its config from here); the
/// uncensored source carries its own configs and never needs it.
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_config",
    source: FLUX2_KLEIN_9B_BFL_SOURCE,
    dir: FLUX2_KLEIN_9B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG_FILES,
};

static FLUX2_KLEIN_9B_TRANSFORMER_BF16_FILES: [ExternalFile; 1] = [
    ExternalFile::direct(FLUX2_KLEIN_9B_TRANSFORMER_SINGLE_FILE, 18_157_185_168, "0975d6b77b5f510b99547d6724a208e36527df654e8f6134f59ece3f9f30da58"),
];

/// FLUX.2 klein 9B bf16 transformer: the BFL single file (BFL key layout, about 18.2 GB) at
/// the package root. The download target of the 9B bf16 transformer.
pub static FLUX2_KLEIN_9B_TRANSFORMER_BF16: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_transformer_bf16",
    source: FLUX2_KLEIN_9B_BFL_SOURCE,
    dir: FLUX2_KLEIN_9B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TRANSFORMER_BF16_FILES,
};

static FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS_FILES: [ExternalFile; 3] = [
    ExternalFile::direct("transformer/diffusion_pytorch_model-00001-of-00002.safetensors", 9_801_069_272, "cb942a7072865a1d06e47a3361f9ba8746e68ad207c8499083bcb735869f5102"),
    ExternalFile::direct("transformer/diffusion_pytorch_model-00002-of-00002.safetensors", 8_356_121_608, "ca568a31d19c03ddbcfd8b2d4ec7dbd16dcefbaa50b7aef1b8ceefd6e6eb0970"),
    ExternalFile::direct("transformer/diffusion_pytorch_model.safetensors.index.json", 24_875, "96fe598b12bdfa266f54c2a6027dedc6e87016cce7e19cb2821a8b7deaf9deb9"),
];

/// FLUX.2 klein 9B bf16 transformer in the LEGACY diffusers layout (`transformer/`, two shards
/// and their index): what the backend's former download wrote. ADOPT-ONLY: never returned by
/// [`flux2_klein_spec`], so it is never offered as a download; it joins a bundle only through
/// [`Flux2TransformerLayout::LegacyDiffusers`] ([`flux2_klein_transformer_layout`]).
pub static FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_transformer_bf16_diffusers",
    source: FLUX2_KLEIN_9B_BFL_SOURCE,
    dir: FLUX2_KLEIN_9B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS_FILES,
};

static FLUX2_KLEIN_9B_TEXT_ENCODER_BF16_FILES: [ExternalFile; 5] = [
    ExternalFile::direct("text_encoder/model-00001-of-00004.safetensors", 4_902_257_696, "c0dc64934ae0f730ddc80d99af44968d01a89e8454df07d762096ea1356446bc"),
    ExternalFile::direct("text_encoder/model-00002-of-00004.safetensors", 4_915_960_368, "d58533b468c31caa0222540a8aefddea1d74dc6e1fee928da8556d3d85729d6e"),
    ExternalFile::direct("text_encoder/model-00003-of-00004.safetensors", 4_983_068_496, "74927fec432e050365bf757bb30348f560a44394efac89f492680b7c910b64fd"),
    ExternalFile::direct("text_encoder/model-00004-of-00004.safetensors", 1_580_230_264, "cb73f466fade5716702bda38d4e3b321c9358c39889e46fb9d613fb038bfcb2f"),
    ExternalFile::direct("text_encoder/model.safetensors.index.json", 32_914, "e7e4bdc58d3302c97357f27979b270987dd620cf0cd9b1c130e3a51c9d64df95"),
];

/// FLUX.2 klein 9B stock bf16 Qwen3-8B text-encoder weights (four shards + index, about
/// 16.4 GB) into `text_encoder/`.
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_BF16: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_bf16",
    source: FLUX2_KLEIN_9B_BFL_SOURCE,
    dir: FLUX2_KLEIN_9B_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_BF16_FILES,
};

static FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q8_0_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-2-klein-9b-Q8_0.gguf", 9_978_304_800, "dfa8908dd58c5af6479b944f80558e226ee40a8fe0903729309e8d1b98fe5297")];
static FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q6_K_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-2-klein-9b-Q6_K.gguf", 7_865_424_160, "1cd667293607431e79c9e7e01ecf5c602bd00539c2c0f49d4817a62998b5fe98")];
static FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q5_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-2-klein-9b-Q5_K_M.gguf", 7_018_699_040, "ad0ace013fb401560ba4a7ff2b2bb3235795f07c19dffebf411806b424700503")];
static FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q4_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-2-klein-9b-Q4_K_M.gguf", 5_909_829_920, "5489463ed96056b0bb5472abb5d1bba7055e48d574e37877acb43b407465e26f")];

/// FLUX.2 klein 9B transformer, GGUF `Q8_0` (`unsloth/FLUX.2-klein-9B-GGUF`).
pub static FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q8_0: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_transformer_gguf_q8_0",
    source: FLUX2_KLEIN_9B_TRANSFORMER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_9B_TRANSFORMER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q8_0_FILES,
};

/// FLUX.2 klein 9B transformer, GGUF `Q6_K` (`unsloth/FLUX.2-klein-9B-GGUF`).
pub static FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q6_K: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_transformer_gguf_q6_k",
    source: FLUX2_KLEIN_9B_TRANSFORMER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_9B_TRANSFORMER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q6_K_FILES,
};

/// FLUX.2 klein 9B transformer, GGUF `Q5_K_M` (`unsloth/FLUX.2-klein-9B-GGUF`).
pub static FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q5_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_transformer_gguf_q5_k_m",
    source: FLUX2_KLEIN_9B_TRANSFORMER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_9B_TRANSFORMER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q5_K_M_FILES,
};

/// FLUX.2 klein 9B transformer, GGUF `Q4_K_M` (`unsloth/FLUX.2-klein-9B-GGUF`).
pub static FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q4_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_transformer_gguf_q4_k_m",
    source: FLUX2_KLEIN_9B_TRANSFORMER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_9B_TRANSFORMER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q4_K_M_FILES,
};

static FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q8_0_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3-8B-Q8_0.gguf", 8_709_519_168, "0cfbf745760f07a76ddeb358dd025a27f2e11d1ca9c9a4169a373d52990fe86e")];
static FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q6_K_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3-8B-Q6_K.gguf", 6_725_900_096, "0eaec718fdeab0f429dfa0ec481c090388811f5e63785ddb582292f4ef3c3827")];
static FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q5_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3-8B-Q5_K_M.gguf", 5_851_113_280, "159c694b93271e4edc1dc2a305b10cf981032c8f3035a7da00973312f0331504")];
static FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q4_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3-8B-Q4_K_M.gguf", 5_027_784_512, "120307ba529eb2439d6c430d94104dabd578497bc7bfe7e322b5d9933b449bd4")];

/// FLUX.2 klein 9B stock text encoder, Qwen3-8B GGUF `Q8_0` (`unsloth/Qwen3-8B-GGUF`).
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q8_0: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_gguf_q8_0",
    source: FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q8_0_FILES,
};

/// FLUX.2 klein 9B stock text encoder, Qwen3-8B GGUF `Q6_K` (`unsloth/Qwen3-8B-GGUF`).
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q6_K: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_gguf_q6_k",
    source: FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q6_K_FILES,
};

/// FLUX.2 klein 9B stock text encoder, Qwen3-8B GGUF `Q5_K_M` (`unsloth/Qwen3-8B-GGUF`).
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q5_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_gguf_q5_k_m",
    source: FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q5_K_M_FILES,
};

/// FLUX.2 klein 9B stock text encoder, Qwen3-8B GGUF `Q4_K_M` (`unsloth/Qwen3-8B-GGUF`).
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q4_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_gguf_q4_k_m",
    source: FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q4_K_M_FILES,
};

static FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_BF16_FILES: [ExternalFile; 3] = [
    ExternalFile::direct("model.safetensors", 16_381_516_808, "2bb97dcc052d0b9a4e2a9953229f2ea83cfa0d58c2310391279f16cad931c4ba"),
    ExternalFile::direct("config.json", 1_664, "1da5983a95aaf53157d659d79e27b4c592567d1e55f2f4a124e577d2363f429b"),
    ExternalFile::direct("generation_config.json", 226, "0f920802f4244b6cd9a343dbe1a7de2298d5770a98ab0b9e81995c45864bf1b8"),
];

/// FLUX.2 klein 9B uncensored text encoder, unquantized (`ponpoke/flux2-klein-9b-uncensored-text-encoder`,
/// gated): one F16 safetensors file (about 16.4 GB) and its two configs into
/// `text_encoder_uncensored/`. Format id `Bf16` means "unquantized" here; the real dtype is
/// `Flux2EncoderSource::Uncensored.unquantized_token()`. No tokenizer is pinned: the official
/// `tokenizer/` of [`FLUX2_KLEIN_9B_BASE`] is used.
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_BF16: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_uncensored_bf16",
    source: FLUX2_KLEIN_9B_UNCENSORED_SOURCE,
    dir: FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_BF16_FILES,
};

static FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q8_0_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux2-klein-9b-uncensored-q8_0.gguf", 8_709_518_304, "dff174fbebee8ad5eae50fb4f90a705191a8e1f0782661198c78b9b83830a1be")];
static FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q6_K_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux2-klein-9b-uncensored-q6_k.gguf", 6_725_899_232, "993eb1d28cfee234f420d1afd5217273b45e4e26d7f4d6abbf42435ace69001e")];
static FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q4_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux2-klein-9b-uncensored-q4_k_m.gguf", 5_027_783_648, "df33c8bef6f75c00dd282cd9850e2bf40afb363c6bbe464e977b225882550a14")];

/// FLUX.2 klein 9B uncensored text encoder, GGUF `Q8_0` (`ponpoke/flux2-klein-9b-uncensored-text-encoder`, gated).
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q8_0: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_uncensored_gguf_q8_0",
    source: FLUX2_KLEIN_9B_UNCENSORED_SOURCE,
    dir: FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q8_0_FILES,
};

/// FLUX.2 klein 9B uncensored text encoder, GGUF `Q6_K` (`ponpoke/flux2-klein-9b-uncensored-text-encoder`, gated).
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q6_K: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_uncensored_gguf_q6_k",
    source: FLUX2_KLEIN_9B_UNCENSORED_SOURCE,
    dir: FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q6_K_FILES,
};

/// FLUX.2 klein 9B uncensored text encoder, GGUF `Q4_K_M` (`ponpoke/flux2-klein-9b-uncensored-text-encoder`, gated).
pub static FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q4_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux2_klein_9b_text_encoder_uncensored_gguf_q4_k_m",
    source: FLUX2_KLEIN_9B_UNCENSORED_SOURCE,
    dir: FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q4_K_M_FILES,
};

/// Every FLUX.2 klein spec (both variants, the legacy one included), in catalog order; the
/// parent's `all_specs()` appends it.
pub(super) static FLUX2_KLEIN_SPECS: [&ExternalModelSpec; 29] = [
    &FLUX2_KLEIN_4B_BASE,
    &FLUX2_KLEIN_4B_TRANSFORMER_BF16,
    &FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE,
    &FLUX2_KLEIN_4B_TEXT_ENCODER_BF16,
    &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q8_0,
    &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q6_K,
    &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q5_K_M,
    &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q4_K_M,
    &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q8_0,
    &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q6_K,
    &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q5_K_M,
    &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q4_K_M,
    &FLUX2_KLEIN_9B_BASE,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG,
    &FLUX2_KLEIN_9B_TRANSFORMER_BF16,
    &FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_BF16,
    &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q8_0,
    &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q6_K,
    &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q5_K_M,
    &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q4_K_M,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q8_0,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q6_K,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q5_K_M,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q4_K_M,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_BF16,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q8_0,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q6_K,
    &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q4_K_M,
];

/// The FLUX.2 klein 4B spec holding the weights of `axis` in `format` (exhaustive: every
/// format has a pinned 4B spec on both axes; 4B has only the stock encoder).
fn flux2_klein_4b_weights_spec(axis: Flux2WeightAxis, format: Flux2WeightFormat) -> &'static ExternalModelSpec {
    match (axis, format) {
        (Flux2WeightAxis::Transformer, Flux2WeightFormat::Bf16) => &FLUX2_KLEIN_4B_TRANSFORMER_BF16,
        (Flux2WeightAxis::Transformer, Flux2WeightFormat::Q80) => &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q8_0,
        (Flux2WeightAxis::Transformer, Flux2WeightFormat::Q6K) => &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q6_K,
        (Flux2WeightAxis::Transformer, Flux2WeightFormat::Q5KM) => &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q5_K_M,
        (Flux2WeightAxis::Transformer, Flux2WeightFormat::Q4KM) => &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q4_K_M,
        (Flux2WeightAxis::TextEncoder, Flux2WeightFormat::Bf16) => &FLUX2_KLEIN_4B_TEXT_ENCODER_BF16,
        (Flux2WeightAxis::TextEncoder, Flux2WeightFormat::Q80) => &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q8_0,
        (Flux2WeightAxis::TextEncoder, Flux2WeightFormat::Q6K) => &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q6_K,
        (Flux2WeightAxis::TextEncoder, Flux2WeightFormat::Q5KM) => &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q5_K_M,
        (Flux2WeightAxis::TextEncoder, Flux2WeightFormat::Q4KM) => &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q4_K_M,
    }
}

/// The FLUX.2 klein 9B DOWNLOAD spec of `axis` / `source` / `format` (exhaustive). `None`
/// only for a combination the publisher does not ship (the uncensored `Q5_K_M`). The
/// transformer ignores `source`. Never the legacy diffusers spec.
fn flux2_klein_9b_weights_spec(axis: Flux2WeightAxis, source: Flux2EncoderSource, format: Flux2WeightFormat) -> Option<&'static ExternalModelSpec> {
    let spec = match (axis, source, format) {
        (Flux2WeightAxis::Transformer, _, Flux2WeightFormat::Bf16) => &FLUX2_KLEIN_9B_TRANSFORMER_BF16,
        (Flux2WeightAxis::Transformer, _, Flux2WeightFormat::Q80) => &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q8_0,
        (Flux2WeightAxis::Transformer, _, Flux2WeightFormat::Q6K) => &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q6_K,
        (Flux2WeightAxis::Transformer, _, Flux2WeightFormat::Q5KM) => &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q5_K_M,
        (Flux2WeightAxis::Transformer, _, Flux2WeightFormat::Q4KM) => &FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q4_K_M,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, Flux2WeightFormat::Bf16) => &FLUX2_KLEIN_9B_TEXT_ENCODER_BF16,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, Flux2WeightFormat::Q80) => &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q8_0,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, Flux2WeightFormat::Q6K) => &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q6_K,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, Flux2WeightFormat::Q5KM) => &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q5_K_M,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, Flux2WeightFormat::Q4KM) => &FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q4_K_M,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Bf16) => &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_BF16,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Q80) => &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q8_0,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Q6K) => &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q6_K,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Q5KM) => return None,
        (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Flux2WeightFormat::Q4KM) => &FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q4_K_M,
    };
    Some(spec)
}

/// The pinned DOWNLOAD spec holding the FLUX.2 klein weights of `axis` in `format` for
/// `variant` and encoder `source` (`source` only matters for the text encoder).
///
/// `None` exactly when `format` is not in `Flux2WeightFormat::offered_for(variant, axis,
/// source)` (e.g. the 4B uncensored encoder, the 9B uncensored `Q5_K_M`). Never returns the
/// adopt-only legacy spec ([`flux2_klein_legacy_transformer_spec`]).
#[must_use]
pub fn flux2_klein_spec(variant: Flux2Variant, axis: Flux2WeightAxis, source: Flux2EncoderSource, format: Flux2WeightFormat) -> Option<&'static ExternalModelSpec> {
    if !Flux2WeightFormat::offered_for(variant, axis, source).contains(&format) {
        return None;
    }
    match variant {
        Flux2Variant::Klein4B => Some(flux2_klein_4b_weights_spec(axis, format)),
        Flux2Variant::Klein9B => flux2_klein_9b_weights_spec(axis, source, format),
    }
}

/// The ADOPT-ONLY spec of `variant`'s bf16 transformer in the legacy diffusers layout
/// (`transformer/` shards + index, as the backend's former download wrote it): `Some` for
/// 9B, `None` for 4B (its bf16 transformer already IS the diffusers file).
#[must_use]
pub fn flux2_klein_legacy_transformer_spec(variant: Flux2Variant) -> Option<&'static ExternalModelSpec> {
    match variant {
        Flux2Variant::Klein9B => Some(&FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS),
        Flux2Variant::Klein4B => None,
    }
}

/// Which on-disk layout a variant's bf16 transformer is used from. Only 9B has a choice; a
/// variant without a legacy spec always uses `Primary`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Flux2TransformerLayout {
    /// The download target ([`flux2_klein_spec`]): for 9B the BFL single file at the package
    /// root, for 4B the diffusers `transformer/` file. The default before any probe ran.
    #[default]
    Primary,
    /// The legacy diffusers `transformer/` folder ([`flux2_klein_legacy_transformer_spec`]),
    /// kept so an install made by the backend's former download is used, not refetched.
    LegacyDiffusers,
}

/// The ONE rule for the bf16 transformer layout of `variant` under `side_models_root`:
/// `Primary` when the primary bf16 transformer spec reads `Installed`; else `LegacyDiffusers`
/// when the legacy spec reads `Installed` or its files are present unmarked
/// (`unmarked_files_present`, i.e. adoptable); else `Primary` (nothing there: the download
/// target). Always `Primary` for a variant without a legacy spec, without touching the disk.
///
/// Stat-only (no hashing, no network), but filesystem I/O: worker threads only.
#[must_use]
pub fn flux2_klein_transformer_layout(side_models_root: &Path, variant: Flux2Variant) -> Flux2TransformerLayout {
    let Some(legacy) = flux2_klein_legacy_transformer_spec(variant) else {
        return Flux2TransformerLayout::Primary;
    };
    let primary_installed = flux2_klein_spec(variant, Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock, Flux2WeightFormat::Bf16)
        .is_some_and(|primary| external_model_status(side_models_root, primary) == ExternalModelStatus::Installed);
    // An installed single file decides alone: the legacy folder is not even looked at.
    let legacy_usable = !primary_installed
        && (external_model_status(side_models_root, legacy) == ExternalModelStatus::Installed || unmarked_files_present(side_models_root, legacy));
    transformer_layout_rule(primary_installed, legacy_usable)
}

/// The decision of [`flux2_klein_transformer_layout`] over its two stat facts (kept apart so
/// every combination is testable without an 18 GB install).
fn transformer_layout_rule(primary_installed: bool, legacy_usable: bool) -> Flux2TransformerLayout {
    if !primary_installed && legacy_usable {
        Flux2TransformerLayout::LegacyDiffusers
    } else {
        Flux2TransformerLayout::Primary
    }
}

/// The spec holding `variant`'s weights of `axis` / `source` / `format` in the given bf16
/// transformer `layout`: the legacy spec for a bf16 transformer in `LegacyDiffusers` (when the
/// variant has one), else [`flux2_klein_spec`].
fn flux2_klein_selected_spec(variant: Flux2Variant, axis: Flux2WeightAxis, source: Flux2EncoderSource, format: Flux2WeightFormat, layout: Flux2TransformerLayout) -> Option<&'static ExternalModelSpec> {
    let primary = flux2_klein_spec(variant, axis, source, format)?;
    let legacy_bf16_transformer = axis == Flux2WeightAxis::Transformer && format == Flux2WeightFormat::Bf16 && layout == Flux2TransformerLayout::LegacyDiffusers;
    if legacy_bf16_transformer && let Some(legacy) = flux2_klein_legacy_transformer_spec(variant) {
        return Some(legacy);
    }
    Some(primary)
}

/// The bundle of specs a FLUX.2 klein selection needs installed: the variant's base part,
/// then the stock text-encoder configs when the variant keeps them apart
/// ([`flux2_klein_text_encoder_config_spec`]: 9B with the `Stock` source), then the transformer
/// spec of `transformer` (the legacy diffusers spec when `layout` is `LegacyDiffusers` and
/// `transformer` is `Bf16`), then the text-encoder spec of `text_encoder` from `source`.
///
/// A format the variant does not offer for that component contributes no spec; callers
/// normalize their selection with `Flux2WeightFormat::offered_for` / `closest_offered` first.
#[must_use]
pub fn flux2_klein_package_specs(variant: Flux2Variant, transformer: Flux2WeightFormat, layout: Flux2TransformerLayout, source: Flux2EncoderSource, text_encoder: Flux2WeightFormat) -> Vec<&'static ExternalModelSpec> {
    let base = flux2_klein_base_spec(variant);
    let mut specs = vec![base];
    specs.extend(flux2_klein_text_encoder_config_spec(variant, source));
    specs.extend(flux2_klein_selected_spec(variant, Flux2WeightAxis::Transformer, source, transformer, layout));
    specs.extend(flux2_klein_selected_spec(variant, Flux2WeightAxis::TextEncoder, source, text_encoder, layout));
    specs
}

/// The base part (licence, index, scheduler, tokenizer, VAE, component configs) of `variant`.
fn flux2_klein_base_spec(variant: Flux2Variant) -> &'static ExternalModelSpec {
    match variant {
        Flux2Variant::Klein4B => &FLUX2_KLEIN_4B_BASE,
        Flux2Variant::Klein9B => &FLUX2_KLEIN_9B_BASE,
    }
}

/// The spec holding `variant`'s stock text-encoder configs apart from its base, for encoder
/// `source`: `Some` for 9B with the `Stock` source (bf16 and GGUF alike); `None` for the
/// uncensored source (it carries its own configs) and for 4B (its base pins them).
fn flux2_klein_text_encoder_config_spec(variant: Flux2Variant, source: Flux2EncoderSource) -> Option<&'static ExternalModelSpec> {
    match (variant, source) {
        (Flux2Variant::Klein9B, Flux2EncoderSource::Stock) => Some(&FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG),
        (Flux2Variant::Klein9B, Flux2EncoderSource::Uncensored) | (Flux2Variant::Klein4B, Flux2EncoderSource::Stock | Flux2EncoderSource::Uncensored) => None,
    }
}

/// Where an UNQUANTIZED (bf16 / f16 safetensors) component lives inside its spec's directory:
/// the diffusers component folder, the single file, or the spec directory itself (`""`).
/// Every file of the spec lies at or under this path (catalog test).
fn unquantized_component_path(variant: Flux2Variant, axis: Flux2WeightAxis, source: Flux2EncoderSource, layout: Flux2TransformerLayout) -> &'static str {
    match (variant, axis, source, layout) {
        (Flux2Variant::Klein9B, Flux2WeightAxis::Transformer, _, Flux2TransformerLayout::Primary) => FLUX2_KLEIN_9B_TRANSFORMER_SINGLE_FILE,
        (Flux2Variant::Klein9B, Flux2WeightAxis::Transformer, _, Flux2TransformerLayout::LegacyDiffusers)
        | (Flux2Variant::Klein4B, Flux2WeightAxis::Transformer, _, Flux2TransformerLayout::Primary | Flux2TransformerLayout::LegacyDiffusers) => FLUX2_KLEIN_TRANSFORMER_SUBDIR,
        (Flux2Variant::Klein4B | Flux2Variant::Klein9B, Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, _) => FLUX2_KLEIN_TEXT_ENCODER_SUBDIR,
        // The uncensored spec's own directory is `text_encoder_uncensored/` (offered for 9B
        // only; 4B never reaches here because `flux2_klein_spec` refuses it first).
        (Flux2Variant::Klein4B | Flux2Variant::Klein9B, Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, _) => "",
    }
}

/// Where the FLUX.2 klein weights of `axis` / `source` / `format` live once installed under
/// `side_models_root` (pass `ms_config::side_models_dir()`), in the bf16 transformer `layout`
/// ([`flux2_klein_transformer_layout`]): what the backend is sent. Pure path joining: checks
/// nothing on disk.
///
/// | component, format | path |
/// |---|---|
/// | 9B transformer bf16, `Primary` | `<package>/flux-2-klein-9b.safetensors` |
/// | 9B transformer bf16, `LegacyDiffusers` | `<package>/transformer` |
/// | 4B transformer bf16 (any layout) | `<package>/transformer` |
/// | stock encoder bf16 | `<package>/text_encoder` |
/// | uncensored encoder bf16 (f16) | `<package>/text_encoder_uncensored` |
/// | any GGUF | the `.gguf` file |
///
/// `None` exactly when [`flux2_klein_spec`] is `None`.
#[must_use]
pub fn flux2_klein_weights_path(side_models_root: &Path, variant: Flux2Variant, axis: Flux2WeightAxis, source: Flux2EncoderSource, format: Flux2WeightFormat, layout: Flux2TransformerLayout) -> Option<PathBuf> {
    let spec = flux2_klein_selected_spec(variant, axis, source, format, layout)?;
    let dir = spec.local_dir(side_models_root);
    if format.is_gguf() {
        // A GGUF spec pins exactly one file (catalog test), which is the component.
        return spec.files.first().map(|file| join_relative(&dir, file.path));
    }
    let component = unquantized_component_path(variant, axis, source, layout);
    Some(if component.is_empty() { dir } else { join_relative(&dir, component) })
}

/// The BFL single-file bf16 transformer of `variant`: the file Stable-Diffusion.cpp loads for
/// a bf16 transformer (4B: [`FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE`], 9B: the primary download
/// target [`FLUX2_KLEIN_9B_TRANSFORMER_BF16`]).
fn flux2_klein_single_file_transformer_spec(variant: Flux2Variant) -> &'static ExternalModelSpec {
    match variant {
        Flux2Variant::Klein4B => &FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE,
        Flux2Variant::Klein9B => &FLUX2_KLEIN_9B_TRANSFORMER_BF16,
    }
}

/// The spec Stable-Diffusion.cpp loads for `variant`'s weights of `axis` / `source` / `format`:
/// the single-file transformer for a bf16 transformer (sd.cpp rejects the diffusers
/// `transformer/` layout, so the bf16 transformer layout never matters there), else
/// [`flux2_klein_spec`]. `None` exactly when [`flux2_klein_spec`] is `None`.
fn flux2_klein_sdcpp_spec(variant: Flux2Variant, axis: Flux2WeightAxis, source: Flux2EncoderSource, format: Flux2WeightFormat) -> Option<&'static ExternalModelSpec> {
    let primary = flux2_klein_spec(variant, axis, source, format)?;
    let bf16_transformer = axis == Flux2WeightAxis::Transformer && format == Flux2WeightFormat::Bf16;
    Some(if bf16_transformer { flux2_klein_single_file_transformer_spec(variant) } else { primary })
}

/// The bundle of specs a FLUX.2 klein selection needs installed on `runtime`.
///
/// - `PyTorch`: exactly [`flux2_klein_package_specs`] (`layout` applies).
/// - `SdCpp`: the variant's base part (it holds the VAE), then the transformer spec sd.cpp
///   loads (the BFL single file for bf16, whatever `layout` says; the GGUF spec otherwise),
///   then the text-encoder spec of `text_encoder` from `source`. No 9B stock encoder configs:
///   sd.cpp reads tensors only.
///
/// A format the variant does not offer for that component contributes no spec, as in
/// [`flux2_klein_package_specs`].
#[must_use]
pub fn flux2_klein_package_specs_for(runtime: ImageEditRuntime, variant: Flux2Variant, transformer: Flux2WeightFormat, layout: Flux2TransformerLayout, source: Flux2EncoderSource, text_encoder: Flux2WeightFormat) -> Vec<&'static ExternalModelSpec> {
    match runtime {
        ImageEditRuntime::PyTorch => flux2_klein_package_specs(variant, transformer, layout, source, text_encoder),
        ImageEditRuntime::SdCpp => {
            let mut specs = vec![flux2_klein_base_spec(variant)];
            specs.extend(flux2_klein_sdcpp_spec(variant, Flux2WeightAxis::Transformer, source, transformer));
            specs.extend(flux2_klein_sdcpp_spec(variant, Flux2WeightAxis::TextEncoder, source, text_encoder));
            specs
        }
    }
}

/// The three files Stable-Diffusion.cpp is given for a FLUX.2 klein selection once its sd.cpp
/// bundle ([`flux2_klein_package_specs_for`] with `SdCpp`) is installed under
/// `side_models_root` (pass `ms_config::side_models_dir()`). Pure path joining: checks nothing
/// on disk.
///
/// | component | path |
/// |---|---|
/// | diffusion, bf16 | `<package>/flux-2-klein-<4b\|9b>.safetensors` (BFL single file) |
/// | vae | `<package>/vae/diffusion_pytorch_model.safetensors` |
/// | llm, stock bf16 | `<package>/text_encoder/model.safetensors.index.json` |
/// | llm, uncensored unquantized | `<package>/text_encoder_uncensored/model.safetensors` |
/// | any GGUF | the `.gguf` file |
///
/// `None` when `transformer` or `text_encoder` is not offered for `variant` / `source`
/// (`Flux2WeightFormat::offered_for`).
#[must_use]
pub fn flux2_klein_sdcpp_paths(side_models_root: &Path, variant: Flux2Variant, transformer: Flux2WeightFormat, source: Flux2EncoderSource, text_encoder: Flux2WeightFormat) -> Option<SdCppComponentPaths> {
    // The diffusion spec is a single file for every format (the BFL single file or one GGUF;
    // catalog test), and that file is the component.
    let diffusion_spec = flux2_klein_sdcpp_spec(variant, Flux2WeightAxis::Transformer, source, transformer)?;
    let diffusion = diffusion_spec.files.first().map(|file| join_relative(&diffusion_spec.local_dir(side_models_root), file.path))?;
    let encoder_spec = flux2_klein_sdcpp_spec(variant, Flux2WeightAxis::TextEncoder, source, text_encoder)?;
    let encoder_dir = encoder_spec.local_dir(side_models_root);
    let llm = if text_encoder.is_gguf() {
        // A GGUF spec pins exactly one file (catalog test), which is the component.
        encoder_spec.files.first().map(|file| join_relative(&encoder_dir, file.path))?
    } else {
        match source {
            Flux2EncoderSource::Stock => join_relative(&encoder_dir, FLUX2_KLEIN_TEXT_ENCODER_INDEX),
            Flux2EncoderSource::Uncensored => join_relative(&encoder_dir, FLUX2_KLEIN_UNCENSORED_ENCODER_WEIGHTS),
        }
    };
    let vae = join_relative(&flux2_klein_base_spec(variant).local_dir(side_models_root), FLUX2_KLEIN_VAE_WEIGHTS);
    Some(SdCppComponentPaths { diffusion, vae, llm })
}

/// What a FLUX.2 klein catalog spec is, for display: the reverse of the lookups above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flux2SpecRole {
    /// The base part of `variant` ([`flux2_klein_package_specs`]'s first spec).
    Base {
        /// The variant.
        variant: Flux2Variant,
    },
    /// The stock text-encoder configs kept apart from `variant`'s base (9B only; in the bundle
    /// of the `Stock` source).
    TextEncoderConfig {
        /// The variant.
        variant: Flux2Variant,
    },
    /// The download spec of one component format: `flux2_klein_spec(variant, axis, source,
    /// format)` returns it. `source` is `Stock` for every transformer spec (the transformer
    /// does not depend on the encoder source).
    Weights {
        /// The variant.
        variant: Flux2Variant,
        /// The component.
        axis: Flux2WeightAxis,
        /// The encoder source (`Stock` for the transformer).
        source: Flux2EncoderSource,
        /// The weight format.
        format: Flux2WeightFormat,
    },
    /// The adopt-only legacy diffusers bf16 transformer of `variant`.
    LegacyTransformer {
        /// The variant.
        variant: Flux2Variant,
    },
    /// The BFL single-file bf16 transformer of `variant` that only the sd.cpp bundle uses
    /// ([`FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE`]). The 9B single file is the primary bf16
    /// download target and reads as `Weights`.
    SingleFileTransformer {
        /// The variant.
        variant: Flux2Variant,
    },
}

/// The role of the FLUX.2 klein spec with this `id`; `None` for any other id (another model,
/// or unknown). Searches the typed lookups, so it can never disagree with them.
#[must_use]
pub fn flux2_klein_spec_role(id: &str) -> Option<Flux2SpecRole> {
    for variant in Flux2Variant::all() {
        if flux2_klein_base_spec(variant).id == id {
            return Some(Flux2SpecRole::Base { variant });
        }
        if flux2_klein_text_encoder_config_spec(variant, Flux2EncoderSource::Stock).is_some_and(|config| config.id == id) {
            return Some(Flux2SpecRole::TextEncoderConfig { variant });
        }
        if flux2_klein_legacy_transformer_spec(variant).is_some_and(|legacy| legacy.id == id) {
            return Some(Flux2SpecRole::LegacyTransformer { variant });
        }
        for axis in Flux2WeightAxis::all() {
            // The transformer does not depend on the source: report it once, as `Stock`.
            let sources: &[Flux2EncoderSource] = match axis {
                Flux2WeightAxis::Transformer => &[Flux2EncoderSource::Stock],
                Flux2WeightAxis::TextEncoder => Flux2EncoderSource::offered_for(variant),
            };
            for &source in sources {
                for &format in Flux2WeightFormat::offered_for(variant, axis, source) {
                    if flux2_klein_spec(variant, axis, source, format).is_some_and(|spec| spec.id == id) {
                        return Some(Flux2SpecRole::Weights { variant, axis, source, format });
                    }
                }
            }
        }
        // Checked after the download targets: the 9B single file is one of them and must read
        // as `Weights`, so only the sd.cpp-only 4B single file reaches this role.
        if flux2_klein_single_file_transformer_spec(variant).id == id {
            return Some(Flux2SpecRole::SingleFileTransformer { variant });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_models::external::validate_spec;
    use crate::ai_models::external_catalog::all_specs;

    /// The (axis, source) pairs a variant offers, with the source pinned to `Stock` for the
    /// transformer (it ignores the source).
    fn component_choices(variant: Flux2Variant) -> Vec<(Flux2WeightAxis, Flux2EncoderSource)> {
        let mut choices = vec![(Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock)];
        choices.extend(Flux2EncoderSource::offered_for(variant).iter().map(|&source| (Flux2WeightAxis::TextEncoder, source)));
        choices
    }

    fn variant_token(variant: Flux2Variant) -> &'static str {
        match variant {
            Flux2Variant::Klein4B => "4b",
            Flux2Variant::Klein9B => "9b",
        }
    }

    fn ids(specs: &[&'static ExternalModelSpec]) -> Vec<&'static str> {
        specs.iter().map(|spec| spec.id).collect()
    }

    #[test]
    fn every_flux2_spec_is_in_the_catalog_list_and_all_specs() {
        assert_eq!(FLUX2_KLEIN_SPECS.len(), 29);
        let all = all_specs();
        let in_all: Vec<&str> = all.iter().map(|spec| spec.id).filter(|id| id.starts_with("flux2_klein_")).collect();
        assert_eq!(in_all, ids(&FLUX2_KLEIN_SPECS));
        assert_eq!(FLUX2_KLEIN_SPECS.iter().filter(|spec| spec.id.starts_with("flux2_klein_4b_")).count(), 12);
        assert_eq!(FLUX2_KLEIN_SPECS.iter().filter(|spec| spec.id.starts_with("flux2_klein_9b_")).count(), 17);
        for spec in FLUX2_KLEIN_SPECS {
            assert!(validate_spec(spec).is_ok(), "{}", spec.id);
        }
    }

    #[test]
    fn every_offered_flux2_format_resolves_to_its_own_spec_and_nothing_else_does() {
        let mut resolved: Vec<&str> = Vec::new();
        let sources = [Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored];
        for variant in Flux2Variant::all() {
            for axis in Flux2WeightAxis::all() {
                for source in sources {
                    for &format in Flux2WeightFormat::all() {
                        let offered = Flux2WeightFormat::offered_for(variant, axis, source).contains(&format);
                        let spec = flux2_klein_spec(variant, axis, source, format);
                        assert_eq!(spec.is_some(), offered, "{variant:?} {axis:?} {source:?} {format:?}");
                        for layout in [Flux2TransformerLayout::Primary, Flux2TransformerLayout::LegacyDiffusers] {
                            assert_eq!(flux2_klein_weights_path(Path::new("side"), variant, axis, source, format, layout).is_some(), offered);
                        }
                        let Some(spec) = spec else { continue };
                        let component = match (axis, source) {
                            (Flux2WeightAxis::Transformer, _) => "transformer",
                            (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock) => "text_encoder",
                            (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored) => "text_encoder_uncensored",
                        };
                        let kind = if format.is_gguf() { "gguf_" } else { "" };
                        assert_eq!(spec.id, format!("flux2_klein_{}_{component}_{kind}{}", variant_token(variant), format.wire()));
                        if format.is_gguf() {
                            assert_eq!(spec.files.len(), 1, "{}", spec.id);
                            let quant = format.quant_token().unwrap_or_default();
                            let name = spec.files[0].path.to_ascii_lowercase();
                            assert!(name.ends_with(&format!("-{}.gguf", quant.to_ascii_lowercase())), "{}", spec.id);
                            assert!(!spec.files[0].path.contains('/'), "{}", spec.id);
                        }
                        // The transformer does not depend on the source: one spec for both.
                        if axis == Flux2WeightAxis::Transformer && source == Flux2EncoderSource::Uncensored {
                            assert_eq!(Some(spec), flux2_klein_spec(variant, axis, Flux2EncoderSource::Stock, format));
                        } else {
                            resolved.push(spec.id);
                        }
                    }
                }
            }
        }
        // Every weights spec is reachable exactly once; only the bases, the 9B encoder configs,
        // the legacy spec and the sd.cpp-only 4B single file are not download targets.
        let mut expected: Vec<&str> = FLUX2_KLEIN_SPECS
            .iter()
            .map(|spec| spec.id)
            .filter(|id| !id.ends_with("_base") && *id != FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS.id && *id != FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG.id && *id != FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE.id)
            .collect();
        expected.sort_unstable();
        resolved.sort_unstable();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn the_legacy_spec_is_never_a_download_target() {
        for variant in Flux2Variant::all() {
            for (axis, source) in component_choices(variant) {
                for &format in Flux2WeightFormat::all() {
                    let spec = flux2_klein_spec(variant, axis, source, format);
                    assert!(spec.is_none_or(|spec| spec.id != FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS.id));
                }
            }
        }
        assert_eq!(flux2_klein_legacy_transformer_spec(Flux2Variant::Klein9B), Some(&FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS));
        assert_eq!(flux2_klein_legacy_transformer_spec(Flux2Variant::Klein4B), None);
    }

    #[test]
    fn spec_roles_round_trip_for_every_flux2_spec() {
        for spec in FLUX2_KLEIN_SPECS {
            let role = flux2_klein_spec_role(spec.id);
            let back = match role {
                Some(Flux2SpecRole::Base { variant }) => Some(flux2_klein_base_spec(variant)),
                Some(Flux2SpecRole::TextEncoderConfig { variant }) => flux2_klein_text_encoder_config_spec(variant, Flux2EncoderSource::Stock),
                Some(Flux2SpecRole::LegacyTransformer { variant }) => flux2_klein_legacy_transformer_spec(variant),
                Some(Flux2SpecRole::SingleFileTransformer { variant }) => Some(flux2_klein_single_file_transformer_spec(variant)),
                Some(Flux2SpecRole::Weights { variant, axis, source, format }) => {
                    if axis == Flux2WeightAxis::Transformer {
                        assert_eq!(source, Flux2EncoderSource::Stock, "{}", spec.id);
                    }
                    flux2_klein_spec(variant, axis, source, format)
                }
                None => None,
            };
            assert_eq!(back.map(|back| back.id), Some(spec.id), "{role:?}");
        }
        assert_eq!(flux2_klein_spec_role("flux2_klein_9b_base"), Some(Flux2SpecRole::Base { variant: Flux2Variant::Klein9B }));
        assert_eq!(flux2_klein_spec_role("flux2_klein_9b_text_encoder_config"), Some(Flux2SpecRole::TextEncoderConfig { variant: Flux2Variant::Klein9B }));
        assert_eq!(flux2_klein_spec_role("flux2_klein_4b_transformer_bf16_single"), Some(Flux2SpecRole::SingleFileTransformer { variant: Flux2Variant::Klein4B }));
        // The 9B single file is the primary bf16 download target, never the sd.cpp-only role.
        assert_eq!(
            flux2_klein_spec_role(FLUX2_KLEIN_9B_TRANSFORMER_BF16.id),
            Some(Flux2SpecRole::Weights { variant: Flux2Variant::Klein9B, axis: Flux2WeightAxis::Transformer, source: Flux2EncoderSource::Stock, format: Flux2WeightFormat::Bf16 })
        );
        assert_eq!(flux2_klein_spec_role("baberu_ocr"), None);
        assert_eq!(flux2_klein_spec_role(""), None);
    }

    #[test]
    fn flux2_specs_are_shared_and_live_under_their_variant_dir() {
        for variant in Flux2Variant::all() {
            let package = variant.dir_name();
            let prefix = format!("flux2_klein_{}_", variant_token(variant));
            let specs: Vec<_> = FLUX2_KLEIN_SPECS.iter().filter(|spec| spec.id.starts_with(&prefix)).collect();
            assert!(!specs.is_empty());
            for spec in specs {
                assert_eq!(spec.layout, DirLayout::Shared, "{}", spec.id);
                assert!(spec.dir == package || spec.dir.starts_with(&format!("{package}/")), "{}", spec.id);
            }
        }
        assert_eq!(FLUX2_KLEIN_4B_DIR, Flux2Variant::Klein4B.dir_name());
        assert_eq!(FLUX2_KLEIN_9B_DIR, Flux2Variant::Klein9B.dir_name());
        // The specs publishing into one package root must not overlap: one file, one owner.
        let roots: Vec<&ExternalModelSpec> = FLUX2_KLEIN_SPECS.iter().copied().filter(|spec| spec.dir == FLUX2_KLEIN_4B_DIR || spec.dir == FLUX2_KLEIN_9B_DIR).collect();
        assert_eq!(roots.len(), 4 + 5);
        for (index, a) in roots.iter().enumerate() {
            for b in roots.iter().skip(index + 1).filter(|b| b.dir == a.dir) {
                assert!(a.files.iter().all(|file| b.files.iter().all(|other| other.path != file.path)), "{} / {}", a.id, b.id);
            }
        }
    }

    #[test]
    fn gated_sources_are_exactly_the_bfl_9b_and_uncensored_repositories() {
        for spec in FLUX2_KLEIN_SPECS {
            let ExternalSource::HuggingFace { repo_id, revision, gated } = spec.source else {
                panic!("{} is not a Hugging Face spec", spec.id);
            };
            assert_eq!(revision.len(), 40, "{}", spec.id);
            let expect_gated = repo_id == "black-forest-labs/FLUX.2-klein-9B" || repo_id == "ponpoke/flux2-klein-9b-uncensored-text-encoder";
            assert_eq!(gated, expect_gated, "{}", spec.id);
        }
    }

    #[test]
    fn totals_match_the_pinned_tables() {
        let expected: [(&ExternalModelSpec, u64); 29] = [
            (&FLUX2_KLEIN_4B_BASE, 184_016_738),
            (&FLUX2_KLEIN_4B_TRANSFORMER_BF16, 7_751_109_744),
            (&FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE, 7_751_105_712),
            (&FLUX2_KLEIN_4B_TEXT_ENCODER_BF16, 8_045_014_847),
            (&FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q8_0, 4_300_644_928),
            (&FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q6_K, 3_409_273_408),
            (&FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q5_K_M, 3_073_368_640),
            (&FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q4_K_M, 2_604_311_104),
            (&FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q8_0, 4_280_405_792),
            (&FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q6_K, 3_306_261_792),
            (&FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q5_K_M, 2_889_514_272),
            (&FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q4_K_M, 2_497_281_312),
            (&FLUX2_KLEIN_9B_BASE, 184_023_563),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG, 1_752),
            (&FLUX2_KLEIN_9B_TRANSFORMER_BF16, 18_157_185_168),
            (&FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS, 18_157_215_755),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_BF16, 16_381_549_738),
            (&FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q8_0, 9_978_304_800),
            (&FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q6_K, 7_865_424_160),
            (&FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q5_K_M, 7_018_699_040),
            (&FLUX2_KLEIN_9B_TRANSFORMER_GGUF_Q4_K_M, 5_909_829_920),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q8_0, 8_709_519_168),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q6_K, 6_725_900_096),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q5_K_M, 5_851_113_280),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_GGUF_Q4_K_M, 5_027_784_512),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_BF16, 16_381_518_698),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q8_0, 8_709_518_304),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q6_K, 6_725_899_232),
            (&FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_GGUF_Q4_K_M, 5_027_783_648),
        ];
        for (spec, total) in expected {
            assert_eq!(spec.total_bytes(), total, "{}", spec.id);
        }
        assert!(FLUX2_KLEIN_SPECS.iter().all(|spec| expected.iter().any(|(listed, _)| listed.id == spec.id)));
    }

    #[test]
    fn the_9b_and_4b_bases_pin_identical_tokenizer_scheduler_vae_and_index() {
        // The backend's package fallback may take these from either package (D18).
        let shared = |path: &str| path == "model_index.json" || path.starts_with("tokenizer/") || path.starts_with("scheduler/") || path.starts_with("vae/");
        let four: Vec<&ExternalFile> = FLUX2_KLEIN_4B_BASE.files.iter().filter(|file| shared(file.path)).collect();
        let nine: Vec<&ExternalFile> = FLUX2_KLEIN_9B_BASE.files.iter().filter(|file| shared(file.path)).collect();
        assert_eq!(four.len(), 11);
        assert_eq!(four, nine);
        // The licence and the component configs are the variant's own (the 9B text-encoder
        // configs live in their own spec).
        let pin = |spec: &ExternalModelSpec, path: &str| spec.files.iter().find(|file| file.path == path).copied();
        for path in ["LICENSE.md", "transformer/config.json"] {
            assert!(pin(&FLUX2_KLEIN_4B_BASE, path).is_some() && pin(&FLUX2_KLEIN_9B_BASE, path).is_some());
            assert_ne!(pin(&FLUX2_KLEIN_4B_BASE, path), pin(&FLUX2_KLEIN_9B_BASE, path), "{path}");
        }
        let nine_config = pin(&FLUX2_KLEIN_9B_TEXT_ENCODER_CONFIG, "text_encoder/config.json");
        assert!(nine_config.is_some() && pin(&FLUX2_KLEIN_9B_BASE, "text_encoder/config.json").is_none());
        assert_ne!(pin(&FLUX2_KLEIN_4B_BASE, "text_encoder/config.json"), nine_config);
    }

    #[test]
    fn flux2_klein_bf16_paths_agree_with_the_config_dirs() {
        let side = Path::new("side");
        for variant in Flux2Variant::all() {
            let package = ms_config::flux2_klein_dir(variant);
            assert_eq!(package.strip_prefix(ms_config::side_models_dir()).ok(), Some(Path::new(variant.dir_name())));
            let vae = ms_config::flux2_klein_vae_dir(variant);
            let vae_relative = vae.strip_prefix(&package).ok().and_then(Path::to_str).unwrap_or_default();
            let base = flux2_klein_base_spec(variant);
            assert!(base.files.iter().any(|file| file.path == format!("{vae_relative}/config.json")));
            assert!(base.files.iter().any(|file| file.path == format!("{vae_relative}/diffusion_pytorch_model.safetensors")));
            // A GGUF component takes its config and tokenizer from the stock bundle's
            // non-weight specs (the base, plus the 9B text-encoder configs).
            let stock_bundle = flux2_klein_package_specs(variant, Flux2WeightFormat::Q80, Flux2TransformerLayout::Primary, Flux2EncoderSource::Stock, Flux2WeightFormat::Q80);
            for path in ["transformer/config.json", "text_encoder/config.json", "text_encoder/generation_config.json", "tokenizer/tokenizer.json"] {
                assert!(stock_bundle.iter().filter(|spec| !spec.dir.contains("gguf")).any(|spec| spec.files.iter().any(|file| file.path == path)), "{} {path}", base.id);
            }
        }
        let variant = Flux2Variant::Klein4B;
        let root = join_relative(side, FLUX2_KLEIN_4B_DIR);
        for (axis, folder_name) in [(Flux2WeightAxis::Transformer, "transformer"), (Flux2WeightAxis::TextEncoder, "text_encoder")] {
            let folder = flux2_klein_weights_path(side, variant, axis, Flux2EncoderSource::Stock, Flux2WeightFormat::Bf16, Flux2TransformerLayout::Primary);
            assert_eq!(folder.as_deref().and_then(|path| path.strip_prefix(&root).ok()), Some(Path::new(folder_name)));
        }
    }

    #[test]
    fn every_unquantized_spec_lies_under_the_path_the_backend_is_sent() {
        let side = Path::new("side");
        for variant in Flux2Variant::all() {
            for (axis, source) in component_choices(variant) {
                for layout in [Flux2TransformerLayout::Primary, Flux2TransformerLayout::LegacyDiffusers] {
                    let Some(spec) = flux2_klein_selected_spec(variant, axis, source, Flux2WeightFormat::Bf16, layout) else {
                        continue;
                    };
                    let Some(path) = flux2_klein_weights_path(side, variant, axis, source, Flux2WeightFormat::Bf16, layout) else {
                        panic!("{} has no path", spec.id);
                    };
                    let dir = spec.local_dir(side);
                    for file in spec.files {
                        let file_path = join_relative(&dir, file.path);
                        assert!(file_path == path || file_path.starts_with(&path), "{}: {} not under {}", spec.id, file_path.display(), path.display());
                    }
                }
            }
        }
    }

    #[test]
    fn flux2_klein_9b_paths_per_source_format_and_layout() {
        let side = Path::new("side");
        let package = side.join("FLUX.2-klein-9B");
        let path = |axis, source, format, layout| flux2_klein_weights_path(side, Flux2Variant::Klein9B, axis, source, format, layout);
        let (t, e) = (Flux2WeightAxis::Transformer, Flux2WeightAxis::TextEncoder);
        let (stock, uncensored) = (Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored);
        let (primary, legacy) = (Flux2TransformerLayout::Primary, Flux2TransformerLayout::LegacyDiffusers);
        assert_eq!(path(t, stock, Flux2WeightFormat::Bf16, primary), Some(package.join("flux-2-klein-9b.safetensors")));
        assert_eq!(path(t, uncensored, Flux2WeightFormat::Bf16, primary), Some(package.join("flux-2-klein-9b.safetensors")));
        assert_eq!(path(t, stock, Flux2WeightFormat::Bf16, legacy), Some(package.join("transformer")));
        assert_eq!(path(t, stock, Flux2WeightFormat::Q4KM, legacy), Some(package.join("transformer_gguf").join("flux-2-klein-9b-Q4_K_M.gguf")));
        assert_eq!(path(e, stock, Flux2WeightFormat::Bf16, legacy), Some(package.join("text_encoder")));
        assert_eq!(path(e, stock, Flux2WeightFormat::Q80, primary), Some(package.join("text_encoder_gguf").join("Qwen3-8B-Q8_0.gguf")));
        assert_eq!(path(e, uncensored, Flux2WeightFormat::Bf16, primary), Some(package.join("text_encoder_uncensored")));
        assert_eq!(path(e, uncensored, Flux2WeightFormat::Q6K, primary), Some(package.join("text_encoder_uncensored_gguf").join("flux2-klein-9b-uncensored-q6_k.gguf")));
        assert_eq!(path(e, uncensored, Flux2WeightFormat::Q5KM, primary), None);
        // 4B has no legacy spec: the layout changes nothing.
        let four = |layout| flux2_klein_weights_path(side, Flux2Variant::Klein4B, t, stock, Flux2WeightFormat::Bf16, layout);
        assert_eq!(four(primary), four(legacy));
        assert_eq!(four(primary), Some(side.join("FLUX.2-klein-4B").join("transformer")));
    }

    #[test]
    fn flux2_klein_bundles() {
        let (primary, legacy) = (Flux2TransformerLayout::Primary, Flux2TransformerLayout::LegacyDiffusers);
        let (stock, uncensored) = (Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored);
        let bundle = |variant, transformer, layout, source, encoder| ids(&flux2_klein_package_specs(variant, transformer, layout, source, encoder));
        assert_eq!(bundle(Flux2Variant::Klein4B, Flux2WeightFormat::Bf16, primary, stock, Flux2WeightFormat::Bf16), ["flux2_klein_4b_base", "flux2_klein_4b_transformer_bf16", "flux2_klein_4b_text_encoder_bf16"]);
        assert_eq!(bundle(Flux2Variant::Klein4B, Flux2WeightFormat::Bf16, legacy, stock, Flux2WeightFormat::Bf16), ["flux2_klein_4b_base", "flux2_klein_4b_transformer_bf16", "flux2_klein_4b_text_encoder_bf16"]);
        assert_eq!(bundle(Flux2Variant::Klein4B, Flux2WeightFormat::Q6K, primary, stock, Flux2WeightFormat::Q5KM), ["flux2_klein_4b_base", "flux2_klein_4b_transformer_gguf_q6_k", "flux2_klein_4b_text_encoder_gguf_q5_k_m"]);
        // The 4B uncensored encoder is not offered: no encoder spec at all, never a stock one.
        assert_eq!(bundle(Flux2Variant::Klein4B, Flux2WeightFormat::Q80, primary, uncensored, Flux2WeightFormat::Bf16), ["flux2_klein_4b_base", "flux2_klein_4b_transformer_gguf_q8_0"]);
        assert_eq!(bundle(Flux2Variant::Klein9B, Flux2WeightFormat::Bf16, primary, stock, Flux2WeightFormat::Bf16), ["flux2_klein_9b_base", "flux2_klein_9b_text_encoder_config", "flux2_klein_9b_transformer_bf16", "flux2_klein_9b_text_encoder_bf16"]);
        // The stock GGUF encoder needs the stock configs too; the uncensored source never does.
        assert_eq!(bundle(Flux2Variant::Klein9B, Flux2WeightFormat::Q4KM, primary, stock, Flux2WeightFormat::Q6K), ["flux2_klein_9b_base", "flux2_klein_9b_text_encoder_config", "flux2_klein_9b_transformer_gguf_q4_k_m", "flux2_klein_9b_text_encoder_gguf_q6_k"]);
        assert_eq!(bundle(Flux2Variant::Klein9B, Flux2WeightFormat::Bf16, legacy, uncensored, Flux2WeightFormat::Bf16), ["flux2_klein_9b_base", "flux2_klein_9b_transformer_bf16_diffusers", "flux2_klein_9b_text_encoder_uncensored_bf16"]);
        // The legacy layout only swaps the bf16 transformer.
        assert_eq!(bundle(Flux2Variant::Klein9B, Flux2WeightFormat::Q4KM, legacy, uncensored, Flux2WeightFormat::Q4KM), ["flux2_klein_9b_base", "flux2_klein_9b_transformer_gguf_q4_k_m", "flux2_klein_9b_text_encoder_uncensored_gguf_q4_k_m"]);
        assert_eq!(bundle(Flux2Variant::Klein9B, Flux2WeightFormat::Q80, primary, uncensored, Flux2WeightFormat::Q5KM), ["flux2_klein_9b_base", "flux2_klein_9b_transformer_gguf_q8_0"]);
        for variant in Flux2Variant::all() {
            for (_, source) in component_choices(variant).into_iter().filter(|(axis, _)| *axis == Flux2WeightAxis::TextEncoder) {
                for &transformer in Flux2WeightFormat::offered_for(variant, Flux2WeightAxis::Transformer, source) {
                    for &encoder in Flux2WeightFormat::offered_for(variant, Flux2WeightAxis::TextEncoder, source) {
                        let configs = usize::from(variant == Flux2Variant::Klein9B && source == Flux2EncoderSource::Stock);
                        for layout in [primary, legacy] {
                            assert_eq!(flux2_klein_package_specs(variant, transformer, layout, source, encoder).len(), 3 + configs);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn transformer_layout_rule_table() {
        assert_eq!(transformer_layout_rule(false, false), Flux2TransformerLayout::Primary);
        assert_eq!(transformer_layout_rule(false, true), Flux2TransformerLayout::LegacyDiffusers);
        assert_eq!(transformer_layout_rule(true, false), Flux2TransformerLayout::Primary);
        assert_eq!(transformer_layout_rule(true, true), Flux2TransformerLayout::Primary);
        assert_eq!(Flux2TransformerLayout::default(), Flux2TransformerLayout::Primary);
    }

    /// Creates every file of `spec` under `root` as a SPARSE file of its exact pinned size
    /// (stat-only probes look at sizes; no byte is written).
    fn create_sparse_files(root: &Path, spec: &ExternalModelSpec) {
        let dir = spec.local_dir(root);
        for file in spec.files {
            let path = join_relative(&dir, file.path);
            let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
            std::fs::create_dir_all(&parent).unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
            let handle = std::fs::File::create(&path).unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
            handle.set_len(file.size).unwrap_or_else(|error| panic!("set_len {}: {error}", path.display()));
        }
    }

    #[test]
    fn transformer_layout_on_disk() {
        let temp = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = temp.path();
        // Nothing installed: the download target.
        assert_eq!(flux2_klein_transformer_layout(root, Flux2Variant::Klein9B), Flux2TransformerLayout::Primary);
        // 4B has no legacy spec and never looks at the disk.
        assert_eq!(flux2_klein_transformer_layout(root, Flux2Variant::Klein4B), Flux2TransformerLayout::Primary);
        // Only the single file present (not yet adopted): still the primary layout.
        create_sparse_files(root, &FLUX2_KLEIN_9B_TRANSFORMER_BF16);
        assert_eq!(flux2_klein_transformer_layout(root, Flux2Variant::Klein9B), Flux2TransformerLayout::Primary);
        // Legacy shards present without a marker (the backend's former download): adoptable,
        // so the legacy layout is used while the single file is not installed.
        create_sparse_files(root, &FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS);
        assert_eq!(flux2_klein_transformer_layout(root, Flux2Variant::Klein9B), Flux2TransformerLayout::LegacyDiffusers);
        let single = join_relative(&FLUX2_KLEIN_9B_TRANSFORMER_BF16.local_dir(root), FLUX2_KLEIN_9B_TRANSFORMER_SINGLE_FILE);
        std::fs::remove_file(&single).unwrap_or_else(|error| panic!("remove {}: {error}", single.display()));
        assert_eq!(flux2_klein_transformer_layout(root, Flux2Variant::Klein9B), Flux2TransformerLayout::LegacyDiffusers);
        // A legacy shard of the wrong size is neither installed nor adoptable.
        let shard = join_relative(&FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS.local_dir(root), FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS_FILES[0].path);
        std::fs::File::create(&shard).unwrap_or_else(|error| panic!("truncate {}: {error}", shard.display()));
        assert_eq!(flux2_klein_transformer_layout(root, Flux2Variant::Klein9B), Flux2TransformerLayout::Primary);
    }

    #[test]
    fn the_4b_single_file_transformer_pin() {
        let spec = &FLUX2_KLEIN_4B_TRANSFORMER_BF16_SINGLE;
        assert_eq!(spec.id, "flux2_klein_4b_transformer_bf16_single");
        assert_eq!(spec.source, FLUX2_KLEIN_4B_BASE.source);
        assert_eq!((spec.dir, spec.layout), (FLUX2_KLEIN_4B_DIR, DirLayout::Shared));
        assert_eq!(spec.files, [ExternalFile::direct("flux-2-klein-4b.safetensors", 7_751_105_712, "ec3d4e733a771f61c052fb4856c48b336c55eaf2c65487c2a1faeb9bbda7a343")].as_slice());
        // Never a PyTorch download target: the diffusers file stays the 4B bf16 transformer.
        assert_eq!(flux2_klein_spec(Flux2Variant::Klein4B, Flux2WeightAxis::Transformer, Flux2EncoderSource::Stock, Flux2WeightFormat::Bf16), Some(&FLUX2_KLEIN_4B_TRANSFORMER_BF16));
    }

    /// Whether `path` is one pinned file of some spec of `bundle` under `side`.
    fn is_bundle_file(side: &Path, bundle: &[&'static ExternalModelSpec], path: &Path) -> bool {
        bundle.iter().any(|spec| spec.files.iter().any(|file| join_relative(&spec.local_dir(side), file.path) == path))
    }

    /// Every offered combination resolves on sd.cpp to a bundle of base + transformer + encoder
    /// and to three paths, each a pinned file of that bundle; nothing else resolves.
    #[test]
    fn every_offered_format_resolves_to_an_sdcpp_bundle_and_paths() {
        let side = Path::new("side");
        let sources = [Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored];
        for variant in Flux2Variant::all() {
            for source in sources {
                for &transformer in Flux2WeightFormat::all() {
                    for &encoder in Flux2WeightFormat::all() {
                        let offered = Flux2WeightFormat::offered_for(variant, Flux2WeightAxis::Transformer, source).contains(&transformer)
                            && Flux2WeightFormat::offered_for(variant, Flux2WeightAxis::TextEncoder, source).contains(&encoder);
                        let paths = flux2_klein_sdcpp_paths(side, variant, transformer, source, encoder);
                        assert_eq!(paths.is_some(), offered, "{variant:?} {source:?} {transformer:?} {encoder:?}");
                        let Some(paths) = paths else { continue };
                        let bundle = flux2_klein_package_specs_for(ImageEditRuntime::SdCpp, variant, transformer, Flux2TransformerLayout::LegacyDiffusers, source, encoder);
                        assert_eq!(bundle.len(), 3, "{variant:?} {source:?} {transformer:?} {encoder:?}");
                        assert_eq!(bundle[0].id, flux2_klein_base_spec(variant).id);
                        for path in [&paths.diffusion, &paths.vae, &paths.llm] {
                            assert!(is_bundle_file(side, &bundle, path), "{} is not a file of the bundle", path.display());
                        }
                        // The diffusion spec is exactly one file (the component itself).
                        assert_eq!(bundle[1].files.len(), 1, "{}", bundle[1].id);
                        // sd.cpp never gets the diffusers transformer, whatever the layout says.
                        assert!(bundle.iter().all(|spec| spec.id != FLUX2_KLEIN_4B_TRANSFORMER_BF16.id && spec.id != FLUX2_KLEIN_9B_TRANSFORMER_BF16_DIFFUSERS.id));
                        assert!(paths.diffusion.extension().is_some_and(|ext| ext == "safetensors" || ext == "gguf"));
                    }
                }
            }
        }
    }

    #[test]
    fn flux2_klein_sdcpp_path_table() {
        let side = Path::new("side");
        let (four, nine) = (side.join("FLUX.2-klein-4B"), side.join("FLUX.2-klein-9B"));
        let (stock, uncensored) = (Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored);
        let paths = |variant, transformer, source, encoder| flux2_klein_sdcpp_paths(side, variant, transformer, source, encoder);
        let expect = |diffusion: PathBuf, vae: PathBuf, llm: PathBuf| Some(SdCppComponentPaths { diffusion, vae, llm });
        let vae = |package: &Path| package.join("vae").join("diffusion_pytorch_model.safetensors");
        assert_eq!(
            paths(Flux2Variant::Klein4B, Flux2WeightFormat::Bf16, stock, Flux2WeightFormat::Bf16),
            expect(four.join("flux-2-klein-4b.safetensors"), vae(&four), four.join("text_encoder").join("model.safetensors.index.json"))
        );
        assert_eq!(
            paths(Flux2Variant::Klein4B, Flux2WeightFormat::Q80, stock, Flux2WeightFormat::Q4KM),
            expect(four.join("transformer_gguf").join("flux-2-klein-4b-Q8_0.gguf"), vae(&four), four.join("text_encoder_gguf").join("Qwen3-4B-Q4_K_M.gguf"))
        );
        assert_eq!(
            paths(Flux2Variant::Klein9B, Flux2WeightFormat::Bf16, stock, Flux2WeightFormat::Q80),
            expect(nine.join("flux-2-klein-9b.safetensors"), vae(&nine), nine.join("text_encoder_gguf").join("Qwen3-8B-Q8_0.gguf"))
        );
        assert_eq!(
            paths(Flux2Variant::Klein9B, Flux2WeightFormat::Q4KM, uncensored, Flux2WeightFormat::Bf16),
            expect(nine.join("transformer_gguf").join("flux-2-klein-9b-Q4_K_M.gguf"), vae(&nine), nine.join("text_encoder_uncensored").join("model.safetensors"))
        );
        assert_eq!(
            paths(Flux2Variant::Klein9B, Flux2WeightFormat::Q6K, uncensored, Flux2WeightFormat::Q6K),
            expect(nine.join("transformer_gguf").join("flux-2-klein-9b-Q6_K.gguf"), vae(&nine), nine.join("text_encoder_uncensored_gguf").join("flux2-klein-9b-uncensored-q6_k.gguf"))
        );
        // Not offered: the 4B uncensored encoder, the 9B uncensored Q5_K_M.
        assert_eq!(paths(Flux2Variant::Klein4B, Flux2WeightFormat::Q80, uncensored, Flux2WeightFormat::Bf16), None);
        assert_eq!(paths(Flux2Variant::Klein9B, Flux2WeightFormat::Q80, uncensored, Flux2WeightFormat::Q5KM), None);
    }

    #[test]
    fn package_specs_per_runtime() {
        let (primary, legacy) = (Flux2TransformerLayout::Primary, Flux2TransformerLayout::LegacyDiffusers);
        let (stock, uncensored) = (Flux2EncoderSource::Stock, Flux2EncoderSource::Uncensored);
        // PyTorch is exactly the existing bundle, for every selection and layout.
        for variant in Flux2Variant::all() {
            for source in [stock, uncensored] {
                for &transformer in Flux2WeightFormat::all() {
                    for &encoder in Flux2WeightFormat::all() {
                        for layout in [primary, legacy] {
                            assert_eq!(
                                ids(&flux2_klein_package_specs_for(ImageEditRuntime::PyTorch, variant, transformer, layout, source, encoder)),
                                ids(&flux2_klein_package_specs(variant, transformer, layout, source, encoder))
                            );
                        }
                    }
                }
            }
        }
        let sdcpp = |variant, transformer, layout, source, encoder| ids(&flux2_klein_package_specs_for(ImageEditRuntime::SdCpp, variant, transformer, layout, source, encoder));
        assert_eq!(sdcpp(Flux2Variant::Klein4B, Flux2WeightFormat::Bf16, primary, stock, Flux2WeightFormat::Bf16), ["flux2_klein_4b_base", "flux2_klein_4b_transformer_bf16_single", "flux2_klein_4b_text_encoder_bf16"]);
        assert_eq!(sdcpp(Flux2Variant::Klein9B, Flux2WeightFormat::Bf16, legacy, stock, Flux2WeightFormat::Q4KM), ["flux2_klein_9b_base", "flux2_klein_9b_transformer_bf16", "flux2_klein_9b_text_encoder_gguf_q4_k_m"]);
        assert_eq!(sdcpp(Flux2Variant::Klein9B, Flux2WeightFormat::Q80, primary, uncensored, Flux2WeightFormat::Bf16), ["flux2_klein_9b_base", "flux2_klein_9b_transformer_gguf_q8_0", "flux2_klein_9b_text_encoder_uncensored_bf16"]);
        // A format not offered contributes no spec, as on PyTorch.
        assert_eq!(sdcpp(Flux2Variant::Klein4B, Flux2WeightFormat::Q80, primary, uncensored, Flux2WeightFormat::Bf16), ["flux2_klein_4b_base", "flux2_klein_4b_transformer_gguf_q8_0"]);
        // The base holds the VAE file sd.cpp loads.
        for variant in Flux2Variant::all() {
            assert!(flux2_klein_base_spec(variant).files.iter().any(|file| file.path == FLUX2_KLEIN_VAE_WEIGHTS));
        }
        assert!(FLUX2_KLEIN_4B_TEXT_ENCODER_BF16.files.iter().any(|file| file.path == FLUX2_KLEIN_TEXT_ENCODER_INDEX));
        assert!(FLUX2_KLEIN_9B_TEXT_ENCODER_BF16.files.iter().any(|file| file.path == FLUX2_KLEIN_TEXT_ENCODER_INDEX));
        assert!(FLUX2_KLEIN_9B_TEXT_ENCODER_UNCENSORED_BF16.files.iter().any(|file| file.path == FLUX2_KLEIN_UNCENSORED_ENCODER_WEIGHTS));
    }
}
