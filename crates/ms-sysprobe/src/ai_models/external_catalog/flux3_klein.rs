/*
File: crates/ms-sysprobe/src/ai_models/external_catalog/flux3_klein.rs

Purpose:
The FLUX.3 klein part of the pinned external catalog: the gated BFL package (the base files,
the root DiT config, the three native DiT formats bf16 / fp8r / nvfp4 and the bf16 Qwen3-VL
text encoder), the
unsloth GGUF DiT quants and the official Qwen GGUF text-encoder quants, plus the lookups that
turn a typed engine selection into specs and package-relative paths. The parent
`external_catalog.rs` re-exports every public item, so callers keep naming
`external_catalog::<item>`.

Key items:
- `FLUX3_KLEIN_*` (12 specs): the statics; `FLUX3_KLEIN_BFL_REPO`: the gated repository id.
- `flux3_klein_dit_spec` / `flux3_klein_encoder_spec`: the download target of one DiT /
  encoder format.
- `flux3_klein_package_specs`: the bundle (base + [root DiT config] + DiT + encoder) a
  selection needs.
- `flux3_klein_dit_relative_path` / `flux3_klein_encoder_relative_path`: where a component
  lives relative to the PACKAGE ROOT (so manual mode joins them onto its own root).
- `Flux3SpecRole` + `flux3_klein_spec_role`: reverse lookup spec id -> typed role (display
  names in the UI).

Notes:
- Every spec is `DirLayout::Shared` under `<side_models>/FLUX.3-image-klein`
  (`ms_config::flux3_klein_dir`; a test checks the agreement). The BFL files sit at exactly
  the paths the backend's former download wrote (repository layout), so an existing install
  is adopted by hash, never refetched; GGUF quants sit in `transformer_gguf/` and
  `text_encoder_gguf/`, one file per spec.
- The root `config.json` is its OWN spec (`FLUX3_KLEIN_DIT_CONFIG`), bundled only with the
  DiT formats that read it (bf16 and GGUF); fp8r / nvfp4 carry `variants/<format>/config.json`.
  The released backend downloader fetched the root config only for bf16, so keeping it out of
  the base lets an old fp8r / nvfp4 install adopt every spec of its bundle. The root
  `LICENSE.md` / `NOTICE.md`, the README files and the diffusers `transformer/` / `vae/`
  folders the repository also ships are not pinned (never used).
- The formats are `ms_config`'s (`Flux3DitFormat`, `Flux3EncoderFormat`); this file only maps
  them to pins. The exhaustive matches force a spec for every new format.
- Pins: the Hugging Face tree API at each commit (the gated BFL repository read with a token);
  small files hashed after download (git blob sha1 equal to the tree's).
*/

use ms_config::{Flux3DitFormat, Flux3EncoderFormat};

use crate::ai_models::external::{DirLayout, ExternalFile, ExternalModelSpec, ExternalSource};

/// The FLUX.3 klein package root, relative to `side_models` (`ms_config::flux3_klein_dir`; a
/// catalog test checks the agreement).
const FLUX3_KLEIN_DIR: &str = "FLUX.3-image-klein";
/// Folder of the GGUF DiT files, relative to `side_models`.
const FLUX3_KLEIN_DIT_GGUF_DIR: &str = "FLUX.3-image-klein/transformer_gguf";
/// Folder of the GGUF text-encoder files, relative to `side_models`.
const FLUX3_KLEIN_TEXT_ENCODER_GGUF_DIR: &str = "FLUX.3-image-klein/text_encoder_gguf";
/// The gated BFL repository (`owner/name`) every native FLUX.3 klein file comes from: the one
/// spelling of the id (the panel names it with its licence page).
pub const FLUX3_KLEIN_BFL_REPO: &str = "black-forest-labs/flux-3-image-klein";
/// The gated BFL repository and commit every native FLUX.3 klein file is pinned at.
const FLUX3_KLEIN_BFL_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: FLUX3_KLEIN_BFL_REPO, revision: "337337621b8526e120f2275d250242f4a6bedd75", gated: true };
/// The unsloth GGUF quantizations of the FLUX.3 klein DiT (tensor names equal the BFL
/// `model.safetensors` keys).
const FLUX3_KLEIN_DIT_GGUF_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "unsloth/flux-3-image-klein-GGUF", revision: "12ef13ce920d53f45fe805093910c899668b9e76", gated: false };
/// The official text-only GGUF quantizations of Qwen3-VL-4B-Instruct, whose bf16 weights are
/// byte-identical to the BFL `text_encoder/` shards.
const FLUX3_KLEIN_TEXT_ENCODER_GGUF_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "Qwen/Qwen3-VL-4B-Instruct-GGUF", revision: "1cd86afb9a95c410a6038ab3b40d8b578c892266", gated: false };

// Everything the engine needs whatever the formats: the autoencoder and the text encoder's
// licence, configs, tokenizer and preprocessor files (a GGUF encoder takes its config and
// tokenizer from here). `text_encoder/README.md` is deliberately not pinned. Exactly the files
// the released backend downloader fetched for EVERY format, so any old install adopts it.
static FLUX3_KLEIN_BASE_FILES: [ExternalFile; 11] = [
    ExternalFile::direct("ae.safetensors", 336_211_292, "868fe7b343cc8f3a19dbcfcafbc3d5f888802be3f89bd81b65b3621a066ce8f3"),
    ExternalFile::direct("text_encoder/LICENSE", 11_357, "c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4"),
    ExternalFile::direct("text_encoder/chat_template.json", 5_502, "6f8a6a55027e3da5160105556cda5dd69f6423f1c32645f6730d32de7773d0c4"),
    ExternalFile::direct("text_encoder/config.json", 1_505, "edac7703329133edfc53e46ac0081835144c99d7eebf28b71c732694d435224d"),
    ExternalFile::direct("text_encoder/generation_config.json", 269, "8469742d1fce0de951c8909b26a2c0c0d8490837ce476efb114da9e0cefc4d44"),
    ExternalFile::direct("text_encoder/merges.txt", 1_671_839, "599bab54075088774b1733fde865d5bd747cbcc7a547c5bc12610e874e26f5e3"),
    ExternalFile::direct("text_encoder/preprocessor_config.json", 390, "27225450ac9c6529872ee1924fcb0962ff5634834f817040f444118116f4e516"),
    ExternalFile::direct("text_encoder/tokenizer.json", 7_032_403, "a5d85b6dcc535e6b93115a9ef287e6132fdbf30270da6218194ba742261173c7"),
    ExternalFile::direct("text_encoder/tokenizer_config.json", 10_868, "c2da771801886ad9ae98181793ffd3dfb7f1af30f6f7c6a4e15d7dbba52e2399"),
    ExternalFile::direct("text_encoder/video_preprocessor_config.json", 385, "7768af27c1fafa9cc9011c1dc20067e03f8915e03b63504550e11d5066986d13"),
    ExternalFile::direct("text_encoder/vocab.json", 2_776_833, "ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910"),
];

/// FLUX.3 klein shared part (`black-forest-labs/flux-3-image-klein`, gated): the autoencoder
/// and the text encoder's non-weight files; no DiT config, DiT or encoder weights.
pub static FLUX3_KLEIN_BASE: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_base",
    source: FLUX3_KLEIN_BFL_SOURCE,
    dir: FLUX3_KLEIN_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_BASE_FILES,
};

static FLUX3_KLEIN_DIT_CONFIG_FILES: [ExternalFile; 1] = [ExternalFile::direct("config.json", 828, "c4fc638a9618e8ddc25f013c009be4d2aa9bd631d89dcf45149a377dd472c67d")];

/// FLUX.3 klein root DiT config (`config.json`, gated BFL repository), read by the bf16 and the
/// GGUF DiTs ([`flux3_klein_dit_reads_root_config`]); fp8r / nvfp4 carry their own.
pub static FLUX3_KLEIN_DIT_CONFIG: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_dit_config",
    source: FLUX3_KLEIN_BFL_SOURCE,
    dir: FLUX3_KLEIN_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_DIT_CONFIG_FILES,
};

static FLUX3_KLEIN_DIT_BF16_FILES: [ExternalFile; 2] = [
    ExternalFile::direct("model.safetensors", 11_097_404_512, "c3e9813cdd049c831c836b2e9db61150e49a93fc1a4c63b256494642fee0bd7c"),
    ExternalFile::direct("manifest.json", 582, "e6e1d78fe913eebe33bf2be8da985113a877aaa42e3af2525a0685d34435e11d"),
];

/// FLUX.3 klein bf16 DiT (about 11.1 GB): the repository-root `model.safetensors` and its
/// `manifest.json` (gated BFL repository).
pub static FLUX3_KLEIN_DIT_BF16: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_dit_bf16",
    source: FLUX3_KLEIN_BFL_SOURCE,
    dir: FLUX3_KLEIN_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_DIT_BF16_FILES,
};

static FLUX3_KLEIN_DIT_FP8R_FILES: [ExternalFile; 4] = [
    ExternalFile::direct("variants/fp8r/config.json", 830, "3f8fdaf707b6f4de023f35d253db58077bfb36df746331fcec93fc570a744811"),
    ExternalFile::direct("variants/fp8r/hf_quant_config.json", 1_438, "6179a2d069688ca3331966910cf1968f56bca81c8266ff3a3c3537a108260631"),
    ExternalFile::direct("variants/fp8r/manifest.json", 1_872, "cc8ca26b9f9c7b59167c0ca3f77d5a13a95d5c8008af8e5f9d6f98efb2d6e65b"),
    ExternalFile::direct("variants/fp8r/model.safetensors", 6_270_949_144, "30054053f630725b488aead9a81b422793cee18027856a16bd17b902ad612018"),
];

/// FLUX.3 klein row-wise fp8 DiT (about 6.3 GB) in `variants/fp8r/` (gated BFL repository).
pub static FLUX3_KLEIN_DIT_FP8R: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_dit_fp8r",
    source: FLUX3_KLEIN_BFL_SOURCE,
    dir: FLUX3_KLEIN_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_DIT_FP8R_FILES,
};

static FLUX3_KLEIN_DIT_NVFP4_FILES: [ExternalFile; 4] = [
    ExternalFile::direct("variants/nvfp4/config.json", 831, "4ec70789b865b7168fe800d45c8a57a527b6707a2c3169af061bdd3fd4f3d0c2"),
    ExternalFile::direct("variants/nvfp4/hf_quant_config.json", 2_784, "1a7c401b5f194d62763e42d9848d6c44c109b7e6abc6371611c71538836937d1"),
    ExternalFile::direct("variants/nvfp4/manifest.json", 3_974, "a1001d62704bcc234fd07b131487b1452997d744e57603bca78f40a7d1725542"),
    ExternalFile::direct("variants/nvfp4/model.safetensors", 5_277_674_144, "30b110b0675249cf0461247b4613240312c8064ccc4ef486f4d025c9ff65fb83"),
];

/// FLUX.3 klein 4-bit NVFP4 DiT (about 5.3 GB) in `variants/nvfp4/` (gated BFL repository).
pub static FLUX3_KLEIN_DIT_NVFP4: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_dit_nvfp4",
    source: FLUX3_KLEIN_BFL_SOURCE,
    dir: FLUX3_KLEIN_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_DIT_NVFP4_FILES,
};

static FLUX3_KLEIN_DIT_GGUF_Q8_0_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-3-image-klein-Q8_0.gguf", 6_092_755_872, "af562de40a975439a0f55340221bf48c951e2fc5ad0e11df0bdd73c8091eefcf")];
static FLUX3_KLEIN_DIT_GGUF_Q6_K_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-3-image-klein-Q6_K.gguf", 4_799_886_240, "a33abd53ee3484cb9aec0a81c4bc5227e0a0fc40a67e645834f3253195cf5ed8")];
static FLUX3_KLEIN_DIT_GGUF_Q5_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-3-image-klein-Q5_K_M.gguf", 4_216_230_816, "0a796f9a549e44dd19bdc60f9a350ec692eb0b8679db3675c410a543958ee334")];
static FLUX3_KLEIN_DIT_GGUF_Q4_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("flux-3-image-klein-Q4_K_M.gguf", 3_666_908_064, "a0fe17e27da7660832a3ae3fe35b9c5cd0d81566a4ea5ec7bb4a26421d4f8542")];

/// FLUX.3 klein DiT, GGUF `Q8_0` (`unsloth/flux-3-image-klein-GGUF`).
pub static FLUX3_KLEIN_DIT_GGUF_Q8_0: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_dit_gguf_q8_0",
    source: FLUX3_KLEIN_DIT_GGUF_SOURCE,
    dir: FLUX3_KLEIN_DIT_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_DIT_GGUF_Q8_0_FILES,
};

/// FLUX.3 klein DiT, GGUF `Q6_K` (`unsloth/flux-3-image-klein-GGUF`).
pub static FLUX3_KLEIN_DIT_GGUF_Q6_K: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_dit_gguf_q6_k",
    source: FLUX3_KLEIN_DIT_GGUF_SOURCE,
    dir: FLUX3_KLEIN_DIT_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_DIT_GGUF_Q6_K_FILES,
};

/// FLUX.3 klein DiT, GGUF `Q5_K_M` (`unsloth/flux-3-image-klein-GGUF`).
pub static FLUX3_KLEIN_DIT_GGUF_Q5_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_dit_gguf_q5_k_m",
    source: FLUX3_KLEIN_DIT_GGUF_SOURCE,
    dir: FLUX3_KLEIN_DIT_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_DIT_GGUF_Q5_K_M_FILES,
};

/// FLUX.3 klein DiT, GGUF `Q4_K_M` (`unsloth/flux-3-image-klein-GGUF`).
pub static FLUX3_KLEIN_DIT_GGUF_Q4_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_dit_gguf_q4_k_m",
    source: FLUX3_KLEIN_DIT_GGUF_SOURCE,
    dir: FLUX3_KLEIN_DIT_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_DIT_GGUF_Q4_K_M_FILES,
};

static FLUX3_KLEIN_TEXT_ENCODER_BF16_FILES: [ExternalFile; 3] = [
    ExternalFile::direct("text_encoder/model-00001-of-00002.safetensors", 4_967_229_296, "30a01a0556622645a3cce87b655bbbbbc1f170c196099f1b666c93202c3339a9"),
    ExternalFile::direct("text_encoder/model-00002-of-00002.safetensors", 3_908_490_048, "046296a2a387efb43b0c997d5833c789604d168834f6e0d3064bf7bb13d002a6"),
    ExternalFile::direct("text_encoder/model.safetensors.index.json", 64_742, "58a7841d7bff2548dd91577d216274a83cf1b500bc6a534b809d6c1b1707cf2b"),
];

/// FLUX.3 klein bf16 Qwen3-VL text-encoder weights (two shards + index, about 8.9 GB) into
/// `text_encoder/` (gated BFL repository; the configs and tokenizer are the base's).
pub static FLUX3_KLEIN_TEXT_ENCODER_BF16: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_text_encoder_bf16",
    source: FLUX3_KLEIN_BFL_SOURCE,
    dir: FLUX3_KLEIN_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_TEXT_ENCODER_BF16_FILES,
};

static FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q8_0_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3VL-4B-Instruct-Q8_0.gguf", 4_280_406_144, "054721f478bc5fa6beffb7f38eae575d45298f88cbb8d2f83ef675a727863eb1")];
static FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q4_K_M_FILES: [ExternalFile; 1] = [ExternalFile::direct("Qwen3VL-4B-Instruct-Q4_K_M.gguf", 2_497_281_664, "66358cb18bb6b3b1b6675aa412c7a88ef01d228f481184d13668e5201c730a0a")];

/// FLUX.3 klein text encoder, text-only Qwen3-VL-4B GGUF `Q8_0` (`Qwen/Qwen3-VL-4B-Instruct-GGUF`).
pub static FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q8_0: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_text_encoder_gguf_q8_0",
    source: FLUX3_KLEIN_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX3_KLEIN_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q8_0_FILES,
};

/// FLUX.3 klein text encoder, text-only Qwen3-VL-4B GGUF `Q4_K_M` (`Qwen/Qwen3-VL-4B-Instruct-GGUF`).
pub static FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q4_K_M: ExternalModelSpec = ExternalModelSpec {
    id: "flux3_klein_text_encoder_gguf_q4_k_m",
    source: FLUX3_KLEIN_TEXT_ENCODER_GGUF_SOURCE,
    dir: FLUX3_KLEIN_TEXT_ENCODER_GGUF_DIR,
    layout: DirLayout::Shared,
    files: &FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q4_K_M_FILES,
};

/// Every FLUX.3 klein spec, in catalog order; the parent's `all_specs()` appends it.
pub(super) static FLUX3_KLEIN_SPECS: [&ExternalModelSpec; 12] = [
    &FLUX3_KLEIN_BASE,
    &FLUX3_KLEIN_DIT_CONFIG,
    &FLUX3_KLEIN_DIT_BF16,
    &FLUX3_KLEIN_DIT_FP8R,
    &FLUX3_KLEIN_DIT_NVFP4,
    &FLUX3_KLEIN_DIT_GGUF_Q8_0,
    &FLUX3_KLEIN_DIT_GGUF_Q6_K,
    &FLUX3_KLEIN_DIT_GGUF_Q5_K_M,
    &FLUX3_KLEIN_DIT_GGUF_Q4_K_M,
    &FLUX3_KLEIN_TEXT_ENCODER_BF16,
    &FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q8_0,
    &FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q4_K_M,
];

/// The pinned DOWNLOAD spec holding the FLUX.3 klein DiT in `format` (every format has one).
#[must_use]
pub fn flux3_klein_dit_spec(format: Flux3DitFormat) -> &'static ExternalModelSpec {
    match format {
        Flux3DitFormat::Bf16 => &FLUX3_KLEIN_DIT_BF16,
        Flux3DitFormat::Fp8r => &FLUX3_KLEIN_DIT_FP8R,
        Flux3DitFormat::Nvfp4 => &FLUX3_KLEIN_DIT_NVFP4,
        Flux3DitFormat::Q80 => &FLUX3_KLEIN_DIT_GGUF_Q8_0,
        Flux3DitFormat::Q6K => &FLUX3_KLEIN_DIT_GGUF_Q6_K,
        Flux3DitFormat::Q5KM => &FLUX3_KLEIN_DIT_GGUF_Q5_K_M,
        Flux3DitFormat::Q4KM => &FLUX3_KLEIN_DIT_GGUF_Q4_K_M,
    }
}

/// The pinned DOWNLOAD spec holding the FLUX.3 klein text-encoder weights in `format` (every
/// format has one; the bf16 encoder's configs and tokenizer are in [`FLUX3_KLEIN_BASE`]).
#[must_use]
pub fn flux3_klein_encoder_spec(format: Flux3EncoderFormat) -> &'static ExternalModelSpec {
    match format {
        Flux3EncoderFormat::Bf16 => &FLUX3_KLEIN_TEXT_ENCODER_BF16,
        Flux3EncoderFormat::Q80 => &FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q8_0,
        Flux3EncoderFormat::Q4KM => &FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q4_K_M,
    }
}

/// Whether the DiT in `format` reads the package-root `config.json` ([`FLUX3_KLEIN_DIT_CONFIG`]):
/// bf16 and every GGUF quant do; fp8r / nvfp4 read `variants/<format>/config.json` instead.
#[must_use]
pub fn flux3_klein_dit_reads_root_config(format: Flux3DitFormat) -> bool {
    match format {
        Flux3DitFormat::Bf16 | Flux3DitFormat::Q80 | Flux3DitFormat::Q6K | Flux3DitFormat::Q5KM | Flux3DitFormat::Q4KM => true,
        Flux3DitFormat::Fp8r | Flux3DitFormat::Nvfp4 => false,
    }
}

/// The bundle of specs a FLUX.3 klein selection needs installed, in this order: the base part,
/// the root DiT config when `dit` reads it ([`flux3_klein_dit_reads_root_config`]), the DiT
/// spec of `dit`, the encoder spec of `encoder`. Three or four specs.
#[must_use]
pub fn flux3_klein_package_specs(dit: Flux3DitFormat, encoder: Flux3EncoderFormat) -> Vec<&'static ExternalModelSpec> {
    let mut specs = vec![&FLUX3_KLEIN_BASE];
    if flux3_klein_dit_reads_root_config(dit) {
        specs.push(&FLUX3_KLEIN_DIT_CONFIG);
    }
    specs.push(flux3_klein_dit_spec(dit));
    specs.push(flux3_klein_encoder_spec(encoder));
    specs
}

/// Where the DiT weights of `format` live relative to the PACKAGE ROOT
/// (`ms_config::flux3_klein_dir()` in download mode, the user's root in manual mode), as a
/// `/`-separated path: the weights FILE for every format.
///
/// | format | path |
/// |---|---|
/// | bf16 | `model.safetensors` |
/// | fp8r / nvfp4 | `variants/<format>/model.safetensors` |
/// | GGUF | `transformer_gguf/flux-3-image-klein-<QUANT>.gguf` |
///
/// A catalog test pins each path to the file of [`flux3_klein_dit_spec`].
#[must_use]
pub fn flux3_klein_dit_relative_path(format: Flux3DitFormat) -> &'static str {
    match format {
        Flux3DitFormat::Bf16 => "model.safetensors",
        Flux3DitFormat::Fp8r => "variants/fp8r/model.safetensors",
        Flux3DitFormat::Nvfp4 => "variants/nvfp4/model.safetensors",
        Flux3DitFormat::Q80 => "transformer_gguf/flux-3-image-klein-Q8_0.gguf",
        Flux3DitFormat::Q6K => "transformer_gguf/flux-3-image-klein-Q6_K.gguf",
        Flux3DitFormat::Q5KM => "transformer_gguf/flux-3-image-klein-Q5_K_M.gguf",
        Flux3DitFormat::Q4KM => "transformer_gguf/flux-3-image-klein-Q4_K_M.gguf",
    }
}

/// Where the text encoder of `format` lives relative to the PACKAGE ROOT, as a `/`-separated
/// path: the `text_encoder` FOLDER for bf16 (shards + index; every file of
/// [`flux3_klein_encoder_spec`] lies under it), the `.gguf` FILE for a GGUF format.
#[must_use]
pub fn flux3_klein_encoder_relative_path(format: Flux3EncoderFormat) -> &'static str {
    match format {
        Flux3EncoderFormat::Bf16 => "text_encoder",
        Flux3EncoderFormat::Q80 => "text_encoder_gguf/Qwen3VL-4B-Instruct-Q8_0.gguf",
        Flux3EncoderFormat::Q4KM => "text_encoder_gguf/Qwen3VL-4B-Instruct-Q4_K_M.gguf",
    }
}

/// What a FLUX.3 klein catalog spec is, for display: the reverse of the lookups above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flux3SpecRole {
    /// The base part ([`flux3_klein_package_specs`]'s first spec).
    Base,
    /// The package-root DiT config ([`FLUX3_KLEIN_DIT_CONFIG`]).
    DitConfig,
    /// The DiT weights of one format ([`flux3_klein_dit_spec`]).
    Dit(Flux3DitFormat),
    /// The text-encoder weights of one format ([`flux3_klein_encoder_spec`]).
    TextEncoder(Flux3EncoderFormat),
}

/// The role of the FLUX.3 klein spec with this `id`; `None` for any other id (another model,
/// or unknown). Searches the typed lookups, so it can never disagree with them.
#[must_use]
pub fn flux3_klein_spec_role(id: &str) -> Option<Flux3SpecRole> {
    if FLUX3_KLEIN_BASE.id == id {
        return Some(Flux3SpecRole::Base);
    }
    if FLUX3_KLEIN_DIT_CONFIG.id == id {
        return Some(Flux3SpecRole::DitConfig);
    }
    if let Some(&format) = Flux3DitFormat::all().iter().find(|format| flux3_klein_dit_spec(**format).id == id) {
        return Some(Flux3SpecRole::Dit(format));
    }
    Flux3EncoderFormat::all()
        .iter()
        .find(|format| flux3_klein_encoder_spec(**format).id == id)
        .map(|&format| Flux3SpecRole::TextEncoder(format))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::ai_models::external::{unmarked_files_present, validate_spec};
    use crate::ai_models::external_catalog::{all_specs, join_relative};

    fn ids(specs: &[&'static ExternalModelSpec]) -> Vec<&'static str> {
        specs.iter().map(|spec| spec.id).collect()
    }

    #[test]
    fn every_flux3_spec_is_in_the_catalog_list_and_all_specs() {
        let all = all_specs();
        let in_all: Vec<&str> = all.iter().map(|spec| spec.id).filter(|id| id.starts_with("flux3_klein_")).collect();
        assert_eq!(in_all, ids(&FLUX3_KLEIN_SPECS));
        for spec in FLUX3_KLEIN_SPECS {
            assert!(validate_spec(spec).is_ok(), "{}", spec.id);
            assert_eq!(spec.layout, DirLayout::Shared, "{}", spec.id);
            assert!(spec.dir == FLUX3_KLEIN_DIR || spec.dir.starts_with(&format!("{FLUX3_KLEIN_DIR}/")), "{}", spec.id);
        }
    }

    /// Every format resolves to its own spec, named after it; every spec but the base and the
    /// root DiT config is reachable exactly once.
    #[test]
    fn every_format_resolves_to_its_own_spec() {
        let mut resolved: Vec<&str> = Vec::new();
        for &format in Flux3DitFormat::all() {
            let spec = flux3_klein_dit_spec(format);
            let kind = if format.is_gguf() { "gguf_" } else { "" };
            assert_eq!(spec.id, format!("flux3_klein_dit_{kind}{}", format.wire()));
            if let Some(quant) = format.quant_token() {
                assert_eq!(spec.files.len(), 1, "{}", spec.id);
                assert!(spec.files[0].path.ends_with(&format!("-{quant}.gguf")), "{}", spec.id);
                assert_eq!(spec.dir, FLUX3_KLEIN_DIT_GGUF_DIR);
            } else {
                assert_eq!(spec.dir, FLUX3_KLEIN_DIR);
            }
            resolved.push(spec.id);
        }
        for &format in Flux3EncoderFormat::all() {
            let spec = flux3_klein_encoder_spec(format);
            let kind = if format.is_gguf() { "gguf_" } else { "" };
            assert_eq!(spec.id, format!("flux3_klein_text_encoder_{kind}{}", format.wire()));
            if let Some(quant) = format.quant_token() {
                assert_eq!(spec.files.len(), 1, "{}", spec.id);
                assert!(spec.files[0].path.ends_with(&format!("-{quant}.gguf")), "{}", spec.id);
                assert_eq!(spec.dir, FLUX3_KLEIN_TEXT_ENCODER_GGUF_DIR);
            }
            resolved.push(spec.id);
        }
        let mut expected: Vec<&str> = FLUX3_KLEIN_SPECS.iter().map(|spec| spec.id).filter(|id| *id != FLUX3_KLEIN_BASE.id && *id != FLUX3_KLEIN_DIT_CONFIG.id).collect();
        expected.sort_unstable();
        resolved.sort_unstable();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn spec_roles_round_trip_for_every_flux3_spec() {
        for spec in FLUX3_KLEIN_SPECS {
            let back = match flux3_klein_spec_role(spec.id) {
                Some(Flux3SpecRole::Base) => Some(&FLUX3_KLEIN_BASE),
                Some(Flux3SpecRole::DitConfig) => Some(&FLUX3_KLEIN_DIT_CONFIG),
                Some(Flux3SpecRole::Dit(format)) => Some(flux3_klein_dit_spec(format)),
                Some(Flux3SpecRole::TextEncoder(format)) => Some(flux3_klein_encoder_spec(format)),
                None => None,
            };
            assert_eq!(back.map(|back| back.id), Some(spec.id));
        }
        assert_eq!(flux3_klein_spec_role("flux3_klein_dit_fp8r"), Some(Flux3SpecRole::Dit(Flux3DitFormat::Fp8r)));
        assert_eq!(flux3_klein_spec_role("flux3_klein_text_encoder_gguf_q4_k_m"), Some(Flux3SpecRole::TextEncoder(Flux3EncoderFormat::Q4KM)));
        assert_eq!(flux3_klein_spec_role("flux2_klein_9b_base"), None);
        assert_eq!(flux3_klein_spec_role(""), None);
    }

    #[test]
    fn gated_sources_are_exactly_the_bfl_repository() {
        for spec in FLUX3_KLEIN_SPECS {
            let ExternalSource::HuggingFace { repo_id, revision, gated } = spec.source else {
                panic!("{} is not a Hugging Face spec", spec.id);
            };
            assert_eq!(revision.len(), 40, "{}", spec.id);
            assert_eq!(gated, repo_id == FLUX3_KLEIN_BFL_REPO, "{}", spec.id);
            assert_eq!(spec.gated_repo().is_some(), gated, "{}", spec.id);
        }
        // The native files all come from the gated BFL repository, the quants from open ones.
        for &format in Flux3DitFormat::all() {
            assert_eq!(flux3_klein_dit_spec(format).gated_repo().is_some(), format.is_native(), "{format:?}");
        }
        for &format in Flux3EncoderFormat::all() {
            assert_eq!(flux3_klein_encoder_spec(format).gated_repo().is_some(), !format.is_gguf(), "{format:?}");
        }
        assert!(FLUX3_KLEIN_BASE.gated_repo().is_some());
        assert!(FLUX3_KLEIN_DIT_CONFIG.gated_repo().is_some());
    }

    #[test]
    fn totals_match_the_pinned_tables() {
        let expected: [(&ExternalModelSpec, u64); 12] = [
            (&FLUX3_KLEIN_BASE, 347_722_643),
            (&FLUX3_KLEIN_DIT_CONFIG, 828),
            (&FLUX3_KLEIN_DIT_BF16, 11_097_405_094),
            (&FLUX3_KLEIN_DIT_FP8R, 6_270_953_284),
            (&FLUX3_KLEIN_DIT_NVFP4, 5_277_681_733),
            (&FLUX3_KLEIN_DIT_GGUF_Q8_0, 6_092_755_872),
            (&FLUX3_KLEIN_DIT_GGUF_Q6_K, 4_799_886_240),
            (&FLUX3_KLEIN_DIT_GGUF_Q5_K_M, 4_216_230_816),
            (&FLUX3_KLEIN_DIT_GGUF_Q4_K_M, 3_666_908_064),
            (&FLUX3_KLEIN_TEXT_ENCODER_BF16, 8_875_784_086),
            (&FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q8_0, 4_280_406_144),
            (&FLUX3_KLEIN_TEXT_ENCODER_GGUF_Q4_K_M, 2_497_281_664),
        ];
        for (spec, total) in expected {
            assert_eq!(spec.total_bytes(), total, "{}", spec.id);
        }
        assert!(FLUX3_KLEIN_SPECS.iter().all(|spec| expected.iter().any(|(listed, _)| listed.id == spec.id)));
    }

    /// The specs publishing into one directory never overlap (one file, one owner); the root
    /// `config.json` belongs to the root DiT config spec alone (the bf16 and GGUF DiTs read it).
    #[test]
    fn one_owner_per_file_and_root_config_json_only_in_its_own_spec() {
        for (index, a) in FLUX3_KLEIN_SPECS.iter().enumerate() {
            for b in FLUX3_KLEIN_SPECS.iter().skip(index + 1).filter(|b| b.dir == a.dir) {
                assert!(a.files.iter().all(|file| b.files.iter().all(|other| other.path != file.path)), "{} / {}", a.id, b.id);
            }
        }
        let owners: Vec<&str> = FLUX3_KLEIN_SPECS
            .iter()
            .filter(|spec| spec.dir == FLUX3_KLEIN_DIR && spec.files.iter().any(|file| file.path == "config.json"))
            .map(|spec| spec.id)
            .collect();
        assert_eq!(owners, [FLUX3_KLEIN_DIT_CONFIG.id]);
        // No README, licence of the repository root or diffusers folder is pinned.
        for spec in FLUX3_KLEIN_SPECS {
            for file in spec.files {
                assert!(!file.path.ends_with("README.md") && !file.path.starts_with("transformer/") && !file.path.starts_with("vae/"), "{}: {}", spec.id, file.path);
            }
        }
    }

    #[test]
    fn bundles_are_base_root_config_dit_encoder() {
        assert_eq!(
            ids(&flux3_klein_package_specs(Flux3DitFormat::default(), Flux3EncoderFormat::default())),
            ["flux3_klein_base", "flux3_klein_dit_fp8r", "flux3_klein_text_encoder_bf16"]
        );
        assert_eq!(
            ids(&flux3_klein_package_specs(Flux3DitFormat::Q4KM, Flux3EncoderFormat::Q80)),
            ["flux3_klein_base", "flux3_klein_dit_config", "flux3_klein_dit_gguf_q4_k_m", "flux3_klein_text_encoder_gguf_q8_0"]
        );
        assert_eq!(
            ids(&flux3_klein_package_specs(Flux3DitFormat::Bf16, Flux3EncoderFormat::Bf16)),
            ["flux3_klein_base", "flux3_klein_dit_config", "flux3_klein_dit_bf16", "flux3_klein_text_encoder_bf16"]
        );
        for &dit in Flux3DitFormat::all() {
            // The root config goes with bf16 and the GGUF quants, never with fp8r / nvfp4.
            assert_eq!(flux3_klein_dit_reads_root_config(dit), dit == Flux3DitFormat::Bf16 || dit.is_gguf(), "{dit:?}");
            for &encoder in Flux3EncoderFormat::all() {
                let bundle = flux3_klein_package_specs(dit, encoder);
                let mut expected = vec![FLUX3_KLEIN_BASE.id];
                if flux3_klein_dit_reads_root_config(dit) {
                    expected.push(FLUX3_KLEIN_DIT_CONFIG.id);
                }
                expected.extend([flux3_klein_dit_spec(dit).id, flux3_klein_encoder_spec(encoder).id]);
                assert_eq!(ids(&bundle), expected);
            }
        }
    }

    /// The package-relative paths agree with the catalog files: joined onto
    /// `ms_config::flux3_klein_dir()` they name the spec's file (DiT, GGUF encoder) or the
    /// folder holding every file of the spec (bf16 encoder).
    #[test]
    fn relative_paths_agree_with_the_config_dir_and_the_spec_files() {
        let side = ms_config::side_models_dir();
        let package = ms_config::flux3_klein_dir();
        assert_eq!(package.strip_prefix(&side).ok(), Some(Path::new(FLUX3_KLEIN_DIR)));
        for spec in FLUX3_KLEIN_SPECS {
            assert!(spec.local_dir(&side).starts_with(&package), "{}", spec.id);
        }
        let file_of = |spec: &ExternalModelSpec, wanted: &Path| spec.files.iter().any(|file| join_relative(&spec.local_dir(&side), file.path) == wanted);
        for &format in Flux3DitFormat::all() {
            let path = join_relative(&package, flux3_klein_dit_relative_path(format));
            let spec = flux3_klein_dit_spec(format);
            assert!(file_of(spec, &path), "{}: {}", spec.id, path.display());
            assert!(path.extension().is_some_and(|ext| ext == if format.is_gguf() { "gguf" } else { "safetensors" }), "{}", path.display());
        }
        for &format in Flux3EncoderFormat::all() {
            let path = join_relative(&package, flux3_klein_encoder_relative_path(format));
            let spec = flux3_klein_encoder_spec(format);
            if format.is_gguf() {
                assert!(file_of(spec, &path), "{}: {}", spec.id, path.display());
            } else {
                assert_eq!(path, package.join("text_encoder"));
                for file in spec.files {
                    assert!(join_relative(&spec.local_dir(&side), file.path).starts_with(&path), "{}: {}", spec.id, file.path);
                }
            }
        }
        // The base's encoder configs and tokenizer sit in the bf16 encoder folder too (a GGUF
        // encoder reads them from there).
        let encoder_dir = join_relative(&package, flux3_klein_encoder_relative_path(Flux3EncoderFormat::Bf16));
        for name in ["config.json", "tokenizer.json", "chat_template.json"] {
            assert!(file_of(&FLUX3_KLEIN_BASE, &encoder_dir.join(name)), "{name}");
        }
    }

    /// Creates every file of `spec` under `root` as a SPARSE file of its exact pinned size
    /// (stat-only probes look at sizes; no byte is written).
    fn create_sparse_files(root: &Path, spec: &ExternalModelSpec) {
        let dir = spec.local_dir(root);
        for file in spec.files {
            create_sparse(&join_relative(&dir, file.path), file.size);
        }
    }

    /// Creates (or truncates) `path` as a sparse file of `size` bytes, with its parents.
    fn create_sparse(path: &Path, size: u64) {
        let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
        std::fs::create_dir_all(&parent).unwrap_or_else(|error| panic!("create {}: {error}", parent.display()));
        let handle = std::fs::File::create(path).unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
        handle.set_len(size).unwrap_or_else(|error| panic!("set_len {}: {error}", path.display()));
    }

    /// Adoption contract: a temp root shaped like an existing install (the backend's former
    /// download plus hand-placed GGUF files, no completion marker, an extra unpinned
    /// `text_encoder/README.md`) reads as present-unmarked for EVERY spec, so the catalog
    /// adopts it by hash instead of downloading.
    #[test]
    fn an_unmarked_install_is_adoptable_for_every_spec() {
        let temp = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
        let root = temp.path();
        for spec in FLUX3_KLEIN_SPECS {
            assert!(!unmarked_files_present(root, spec), "{} on an empty root", spec.id);
        }
        for spec in FLUX3_KLEIN_SPECS {
            create_sparse_files(root, spec);
        }
        create_sparse(&join_relative(&FLUX3_KLEIN_BASE.local_dir(root), "text_encoder/README.md"), 1_234);
        for spec in FLUX3_KLEIN_SPECS {
            assert!(unmarked_files_present(root, spec), "{}", spec.id);
        }
        // A file of the wrong size makes only its own spec non-adoptable.
        let shard = join_relative(&FLUX3_KLEIN_TEXT_ENCODER_BF16.local_dir(root), FLUX3_KLEIN_TEXT_ENCODER_BF16_FILES[0].path);
        create_sparse(&shard, 1);
        assert!(!unmarked_files_present(root, &FLUX3_KLEIN_TEXT_ENCODER_BF16));
        assert!(unmarked_files_present(root, &FLUX3_KLEIN_BASE));
    }

    /// Regression: the released (325) backend downloader fetched, for fp8r / nvfp4, only
    /// `ae.safetensors`, the whole `text_encoder/` folder and `variants/<format>/` (no root
    /// `config.json` / `manifest.json`). A sparse tree shaped exactly like that adopts EVERY spec
    /// of its format's bundle, and nothing reads the root config as present.
    #[test]
    fn an_old_quantized_install_without_root_config_adopts_its_whole_bundle() {
        for format in [Flux3DitFormat::Fp8r, Flux3DitFormat::Nvfp4] {
            let temp = tempfile::tempdir().unwrap_or_else(|error| panic!("temp dir: {error}"));
            let root = temp.path();
            let package = FLUX3_KLEIN_BASE.local_dir(root);
            // The old manifest: `ae.safetensors`, `text_encoder/` (README included), `variants/<format>/`.
            for spec in [&FLUX3_KLEIN_BASE, &FLUX3_KLEIN_TEXT_ENCODER_BF16, flux3_klein_dit_spec(format)] {
                create_sparse_files(root, spec);
            }
            create_sparse(&join_relative(&package, "text_encoder/README.md"), 1_234);
            assert!(!package.join("config.json").exists() && !package.join("manifest.json").exists());
            let bundle = flux3_klein_package_specs(format, Flux3EncoderFormat::Bf16);
            assert_eq!(bundle.len(), 3, "{format:?}");
            for spec in bundle {
                assert!(unmarked_files_present(root, spec), "{format:?}: {}", spec.id);
            }
            assert!(!unmarked_files_present(root, &FLUX3_KLEIN_DIT_CONFIG), "{format:?}");
        }
    }
}
