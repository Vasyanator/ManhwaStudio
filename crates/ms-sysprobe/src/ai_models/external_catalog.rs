/*
File: crates/ms-sysprobe/src/ai_models/external_catalog.rs

Purpose:
The pinned catalog of EXTERNAL (third-party) models the application downloads itself:
Baberu OCR, the three PaddleOCR-VL variants, the Surya detection and recognition
checkpoints, the SDXL diffusers configs, the curated SDXL inpainting model and the FLUX.2
klein 4B package (BFL bf16 files plus the unsloth GGUF transformer and Qwen3 text-encoder
quants) (the EasyOCR specs live with their language rule in `easyocr.rs`). This file is the single owner of each
model's source (Hugging Face repository + pinned commit, or HTTPS base URL), file allowlist
(exact size + sha256) and target directory under the caller's `side_models` root. Nothing
else in the workspace may spell these values; the Python backend receives resolved paths and
never downloads these models.

Key items:
- `BABERU_OCR`, `PADDLE_VL_OFFICIAL_1_6`, `PADDLE_VL_OFFICIAL_1_5`, `PADDLE_VL_MANGA_JA`,
  `SURYA_DETECTION_2025_05_07`, `SURYA_RECOGNITION_2025_09_23`, `SDXL_CONFIG_BASE`,
  `SDXL_CONFIG_INPAINT`, `SDXL_INPAINT_FP16`, `FLUX2_KLEIN_4B_BASE`,
  `FLUX2_KLEIN_4B_TRANSFORMER_BF16`, `FLUX2_KLEIN_4B_TEXT_ENCODER_BF16`,
  `FLUX2_KLEIN_4B_TRANSFORMER_GGUF_*`, `FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_*`: the
  `ExternalModelSpec` statics consumed by `external::download_external_model` /
  `external::installed_dir`.
- `flux2_klein_spec` / `flux2_klein_package_specs` / `flux2_klein_weights_path`: the FLUX.2
  klein lookups by `ms_config::Flux2Variant` + `Flux2WeightAxis` + `Flux2WeightFormat` (spec
  of one component format, the bundle a selection needs, the path the backend is sent).
- `all_specs()`: every spec of the application (catalog + EasyOCR), for whole-catalog
  checks.
- `BaberuFiles` + `baberu_files()`: the one owner of the Baberu on-disk layout names; uses
  the same `const` strings as the `BABERU_OCR` file table.
- `PaddleVlVariant`: typed variant selector with its persistence key (`key` / `from_key`).

Notes:
- Changing a pin (revision or any file entry) changes the spec identity recorded in the
  completion marker, so an existing install reads as not installed until re-downloaded.
- Values were taken from the Hugging Face tree API at the pinned revisions, or computed from
  the pinned URLs by `tools/model_pins/compute_pins.py`; tests assert validation, unique ids,
  unique directories (except `Shared` ones) and per-spec byte totals.
*/

use std::path::{Path, PathBuf};

use ms_config::{Flux2Variant, Flux2WeightAxis, Flux2WeightFormat};

use super::external::{DirLayout, ExternalFile, ExternalModelSpec, ExternalSource};

/// Baberu vision encoder (fp16 ONNX), relative to the model directory.
const BABERU_VISION: &str = "onnx/vision_fp16.onnx";
/// Baberu decoder prefill graph (int8 ONNX), relative to the model directory.
const BABERU_PREFILL: &str = "onnx/decoder_prefill_int8.onnx";
/// Baberu decoder single-step graph (int8 ONNX), relative to the model directory.
const BABERU_STEP: &str = "onnx/decoder_step_int8.onnx";
/// Baberu tokenizer vocabulary, relative to the model directory.
const BABERU_VOCAB: &str = "tokenizer/vocab.json";

static BABERU_FILES: [ExternalFile; 5] = [
    ExternalFile::direct(BABERU_VISION, 172_917_304, "4ce333804846b23f1983376efe363c164d4624ecc6bd16f99a15a859d8423117"),
    ExternalFile::direct(BABERU_PREFILL, 35_133_596, "91de16d4e70adaadc0c76ea536f0195b79f54b52e3279aeaac8fc06b46824c60"),
    ExternalFile::direct(BABERU_STEP, 34_715_466, "f6164e9daee6622891d2a52394f143faa7f0ccfdfb90e8d6443193aff36d4c2d"),
    ExternalFile::direct(BABERU_VOCAB, 130_761, "681de4eb83154a397af49e2ddae4705041b12a22e16fe00860b829e889e5f47c"),
    ExternalFile::direct("LICENSE", 11_348, "186332043e82fcfdbeb0b8e0e8d77b63d97ffa3dd9d91ccf7cef1377d8ab345b"),
];

/// Baberu OCR (`genshiai-daichi/baberu-ocr`), native ONNX vision encoder + int8 decoder.
pub static BABERU_OCR: ExternalModelSpec = ExternalModelSpec {
    id: "baberu_ocr",
    source: ExternalSource::HuggingFace { repo_id: "genshiai-daichi/baberu-ocr", revision: "cc74c99765dc5f089d12a8f2e11692bb029a4391" },
    dir: "BaberuOCR",
    layout: DirLayout::Exclusive,
    files: &BABERU_FILES,
};

/// Resolved absolute paths of the Baberu model files inside an installed model directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaberuFiles {
    /// Vision encoder graph.
    pub vision: PathBuf,
    /// Decoder prefill graph.
    pub prefill: PathBuf,
    /// Decoder single-step graph.
    pub step: PathBuf,
    /// Tokenizer vocabulary (`vocab.json`).
    pub vocab: PathBuf,
}

/// Maps a Baberu model directory (normally the `Ok` of `external::installed_dir` for
/// [`BABERU_OCR`]) to its file paths. Pure path joining: checks nothing on disk.
#[must_use]
pub fn baberu_files(model_dir: &Path) -> BaberuFiles {
    BaberuFiles {
        vision: join_relative(model_dir, BABERU_VISION),
        prefill: join_relative(model_dir, BABERU_PREFILL),
        step: join_relative(model_dir, BABERU_STEP),
        vocab: join_relative(model_dir, BABERU_VOCAB),
    }
}

/// Joins a `/`-separated catalog path onto `base` component by component, so the result
/// uses the platform separator.
fn join_relative(base: &Path, relative: &str) -> PathBuf {
    relative.split('/').fold(base.to_path_buf(), |path, part| path.join(part))
}

// PaddleOCR-VL official 1.5 and 1.6 share every file except `model.safetensors`.
const PADDLE_VL_OFFICIAL_CONFIG: ExternalFile = ExternalFile::direct("config.json", 2_059, "ce7f4565f8b1db78532ad5d1b9ebe55c2139d49bd4cb04778b580a08a598f171");
const PADDLE_VL_OFFICIAL_GENERATION: ExternalFile = ExternalFile::direct("generation_config.json", 133, "a6701d78ab3b4d972307cdec3b69d4c13f46e0d5140514f50ab7d84259324b94");
const PADDLE_VL_OFFICIAL_PREPROCESSOR: ExternalFile = ExternalFile::direct("preprocessor_config.json", 641, "111872ab1e8bb7fd040ac5087bfced7ab8f011f02139b088cba294964c3b1d0e");
const PADDLE_VL_PROCESSOR: ExternalFile = ExternalFile::direct("processor_config.json", 137, "1568858960a9760c54431dae693a6152e601ff55cdf6d2eab97a4a99958faea0");
const PADDLE_VL_OFFICIAL_TOKENIZER_JSON: ExternalFile = ExternalFile::direct("tokenizer.json", 11_189_060, "c8a215a59183d0d0781adc33bacd3ce6162716f7fd568fb30234a74d69803a7d");
const PADDLE_VL_TOKENIZER_MODEL: ExternalFile = ExternalFile::direct("tokenizer.model", 1_614_363, "34ef7db83df785924fb83d7b887b6e822a031c56e15cff40aaf9b982988180df");
const PADDLE_VL_OFFICIAL_TOKENIZER_CONFIG: ExternalFile = ExternalFile::direct("tokenizer_config.json", 186_947, "1f979337347cc0cb72a6282d8a23ed183539aa81a87a906f022aee2bab83c7c5");
const PADDLE_VL_OFFICIAL_SPECIAL_TOKENS: ExternalFile = ExternalFile::direct("special_tokens_map.json", 1_151, "d3a125c03103deb2acaf7730791bdbbf196f620e5a2213b664511ff9b4b25bab");
const PADDLE_VL_ADDED_TOKENS: ExternalFile = ExternalFile::direct("added_tokens.json", 25_381, "f59f889088e0fe21c523e7cf121bb6dca3b0bb148cb7159fbb4572c74dfc5644");
const PADDLE_VL_OFFICIAL_CHAT_TEMPLATE: ExternalFile = ExternalFile::direct("chat_template.jinja", 1_474, "2f27812dab7f333e471884e0c803d807f11953d5453140dfb1aaba234f872bc8");
const PADDLE_VL_OFFICIAL_LICENSE: ExternalFile = ExternalFile::direct("LICENSE", 11_376, "b8c4d7deccd236af023af1c88c4d4e8f0fc2f41914e0fb23a3ec9678fb5a8456");

static PADDLE_VL_OFFICIAL_1_6_FILES: [ExternalFile; 12] = [
    PADDLE_VL_OFFICIAL_CONFIG,
    PADDLE_VL_OFFICIAL_GENERATION,
    ExternalFile::direct("model.safetensors", 1_917_255_968, "85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db"),
    PADDLE_VL_OFFICIAL_PREPROCESSOR,
    PADDLE_VL_PROCESSOR,
    PADDLE_VL_OFFICIAL_TOKENIZER_JSON,
    PADDLE_VL_TOKENIZER_MODEL,
    PADDLE_VL_OFFICIAL_TOKENIZER_CONFIG,
    PADDLE_VL_OFFICIAL_SPECIAL_TOKENS,
    PADDLE_VL_ADDED_TOKENS,
    PADDLE_VL_OFFICIAL_CHAT_TEMPLATE,
    PADDLE_VL_OFFICIAL_LICENSE,
];

/// PaddleOCR-VL 1.6 (`PaddlePaddle/PaddleOCR-VL-1.6`), the default variant.
pub static PADDLE_VL_OFFICIAL_1_6: ExternalModelSpec = ExternalModelSpec {
    id: "paddle_vl_official_1_6",
    source: ExternalSource::HuggingFace { repo_id: "PaddlePaddle/PaddleOCR-VL-1.6", revision: "c5630abae1d940eafe0697512a0325494b02ab42" },
    dir: "PaddleOCR-VL/official_1_6",
    layout: DirLayout::Exclusive,
    files: &PADDLE_VL_OFFICIAL_1_6_FILES,
};

static PADDLE_VL_OFFICIAL_1_5_FILES: [ExternalFile; 12] = [
    PADDLE_VL_OFFICIAL_CONFIG,
    PADDLE_VL_OFFICIAL_GENERATION,
    ExternalFile::direct("model.safetensors", 1_917_255_968, "d557c9d8997ae57ed3b1b33bdf347be878cc335687f32ca105341c16973f8958"),
    PADDLE_VL_OFFICIAL_PREPROCESSOR,
    PADDLE_VL_PROCESSOR,
    PADDLE_VL_OFFICIAL_TOKENIZER_JSON,
    PADDLE_VL_TOKENIZER_MODEL,
    PADDLE_VL_OFFICIAL_TOKENIZER_CONFIG,
    PADDLE_VL_OFFICIAL_SPECIAL_TOKENS,
    PADDLE_VL_ADDED_TOKENS,
    PADDLE_VL_OFFICIAL_CHAT_TEMPLATE,
    PADDLE_VL_OFFICIAL_LICENSE,
];

/// PaddleOCR-VL 1.5 (`PaddlePaddle/PaddleOCR-VL-1.5`).
pub static PADDLE_VL_OFFICIAL_1_5: ExternalModelSpec = ExternalModelSpec {
    id: "paddle_vl_official_1_5",
    source: ExternalSource::HuggingFace { repo_id: "PaddlePaddle/PaddleOCR-VL-1.5", revision: "2a4195faa5e7914c12f2fc601d72c81caf8d2da5" },
    dir: "PaddleOCR-VL/official_1_5",
    layout: DirLayout::Exclusive,
    files: &PADDLE_VL_OFFICIAL_1_5_FILES,
};

// The manga fine-tune ships no LICENSE file (its model card states Apache-2.0).
static PADDLE_VL_MANGA_JA_FILES: [ExternalFile; 11] = [
    ExternalFile::direct("config.json", 1_987, "928aaf78567a273cb73ede3671253ec4e38eb60c27a30e945bcd13b4131a0147"),
    ExternalFile::direct("generation_config.json", 133, "cf202f984e003e92dceaa27e749b60b4e6e1b566a1df8486b5b41adf1d016cea"),
    ExternalFile::direct("model.safetensors", 1_917_255_968, "71fcee0e3618582d4c8acc705242aa79b471b6134e7023bf3820642ba638b602"),
    ExternalFile::direct("preprocessor_config.json", 687, "f417a7f977820dfe6828f3ec2e461c027fdb0662f25cae4e841ec1028e0b988a"),
    PADDLE_VL_PROCESSOR,
    ExternalFile::direct("tokenizer.json", 11_187_679, "f90f04fd8e5eb6dfa380f37d10c87392de8438dccb6768a2486b5a96ee76dba6"),
    PADDLE_VL_TOKENIZER_MODEL,
    ExternalFile::direct("tokenizer_config.json", 185_545, "67c651ba09c22151a1fff31e8773a24f7607aef1541aa2f200b48552ed30e894"),
    ExternalFile::direct("special_tokens_map.json", 1_152, "215bf3a1b155fafe3497f8790bedf280af92d29c2f0286c2f87a5c78baff8f7c"),
    PADDLE_VL_ADDED_TOKENS,
    ExternalFile::direct("chat_template.jinja", 1_566, "344fea8b69546526a00996468f86f583fd65441582a36f2fa4abc794aa94094c"),
];

/// PaddleOCR-VL fine-tuned for Japanese manga (`jzhang533/PaddleOCR-VL-For-Manga`).
pub static PADDLE_VL_MANGA_JA: ExternalModelSpec = ExternalModelSpec {
    id: "paddle_vl_manga_ja",
    source: ExternalSource::HuggingFace { repo_id: "jzhang533/PaddleOCR-VL-For-Manga", revision: "1e8aa5f1dd90cc86fe9137c9c0b26ebde613cfe8" },
    dir: "PaddleOCR-VL/manga_ja",
    layout: DirLayout::Exclusive,
    files: &PADDLE_VL_MANGA_JA_FILES,
};

// Surya (surya-ocr 0.17.1) checkpoints and SDXL diffusers configs / the curated SDXL
// inpainting model. Pins computed by `tools/model_pins/compute_pins.py` (Surya: from each
// checkpoint's `manifest.json`; SDXL: the Hugging Face tree at the pinned commit).

static SURYA_DETECTION_2025_05_07_FILES: [ExternalFile; 7] = [
    ExternalFile::direct("manifest.json", 127, "f57a95d3da7d182c4ef556171115a6557455c98f847e81e59a30f37898d994c0"),
    ExternalFile::direct("model.safetensors", 76_930_732, "38c3749eeb5f06fc93ed71eeee5cbd86b1945d08f8f74746bda035d41324bd3e"),
    ExternalFile::direct("preprocessor_config.json", 373, "594344843fcd54db03240194c7677b5c459e78d349e68d4a89d7d91e4fa90587"),
    ExternalFile::direct(".gitattributes", 1_519, "11ad7efa24975ee4b0c3c3a38ed18737f0658a5f75a0a96787b576a78a023361"),
    ExternalFile::direct("README.md", 393, "aab5b4ba5afb1d27b08f4dd6ab4900b588050b6d41a3b6ed0448105b08d87f24"),
    ExternalFile::direct("training_args.bin", 5_624, "7e605d76a38a3739d8372218370b9d5a008b8ff4299cfe61fb2a46e1fa91b525"),
    ExternalFile::direct("config.json", 858, "0f99d8176e3fc194966659cf6f1c52ab9ca1d573f47ba57aca55036ff5267d69"),
];

/// Surya text detection checkpoint `text_detection/2025_05_07` (surya-ocr 0.17.1 `DETECTOR_MODEL_CHECKPOINT`), from the
/// Surya model host; `manifest.json` lists the other files (`surya/common/s3.py`).
pub static SURYA_DETECTION_2025_05_07: ExternalModelSpec = ExternalModelSpec {
    id: "surya_detection_2025_05_07",
    source: ExternalSource::Url { base_url: "https://models.datalab.to/text_detection/2025_05_07" },
    dir: "Surya/text_detection/2025_05_07",
    layout: DirLayout::Exclusive,
    files: &SURYA_DETECTION_2025_05_07_FILES,
};

static SURYA_RECOGNITION_2025_09_23_FILES: [ExternalFile; 13] = [
    ExternalFile::direct("manifest.json", 262, "7f683384c9d18cd4b12ad52bfd05fea1c9e7814caa35a2f181a7d1c0ce793721"),
    ExternalFile::direct(".gitattributes", 1_519, "11ad7efa24975ee4b0c3c3a38ed18737f0658a5f75a0a96787b576a78a023361"),
    ExternalFile::direct("README.md", 5_174, "0336368f8274a5ed09f54cee8d56a851ecee96026d9f391a6735762c16608a60"),
    ExternalFile::direct("specials_dict.json", 44_510, "e9ef2acbf88b3998a2e4bebc1672e7f10f4ef4dfca74863ce02fac994b3e0936"),
    ExternalFile::direct("training_args.bin", 7_633, "e72b841abbff68eaf26ecde03b48678ef52085b5d75542d3818391f8cac43be0"),
    ExternalFile::direct("special_tokens_map.json", 278, "d3399c9cc75ffa4513f4c055673a35bbbc0317218e3c4cd25a3578944d6dbc75"),
    ExternalFile::direct("vocab_math.json", 20_574, "963a928f2385cea48e29461af980dee3d319d30365088282635a6846aba9861b"),
    ExternalFile::direct("specials.json", 20_116, "ca52649007ff824a61a99da32d5a237f8883807a398c870bf33dc4bf96e51b43"),
    ExternalFile::direct("tokenizer_config.json", 694, "96af51d686a557f221d9553ecdfd7c89572a923b433fff90a1604adebf46cbaf"),
    ExternalFile::direct("preprocessor_config.json", 419, "d164ad496629e84733225af0d7af0a71e3f0ceecfcceb7e0fd4da960c45caef9"),
    ExternalFile::direct("config.json", 51_403, "71f612967b4c51fe9d1ec74c2f51639070ce3db7a73f9c1274360f1400021db6"),
    ExternalFile::direct("processor_config.json", 411, "fba86e949c27a2c02ad8c65ccd321f5d2bc2f1c451e98582debd1c2ca14c497c"),
    ExternalFile::direct("model.safetensors", 1_438_882_494, "dd07d624d42dc1b2f6bd871777c088052635294e1637805304300bc4ec8fb3ed"),
];

/// Surya foundation / recognition checkpoint `text_recognition/2025_09_23` (surya-ocr 0.17.1
/// `FOUNDATION_MODEL_CHECKPOINT` = `RECOGNITION_MODEL_CHECKPOINT`; also its tokenizer).
pub static SURYA_RECOGNITION_2025_09_23: ExternalModelSpec = ExternalModelSpec {
    id: "surya_recognition_2025_09_23",
    source: ExternalSource::Url { base_url: "https://models.datalab.to/text_recognition/2025_09_23" },
    dir: "Surya/text_recognition/2025_09_23",
    layout: DirLayout::Exclusive,
    files: &SURYA_RECOGNITION_2025_09_23_FILES,
};

static SDXL_CONFIG_BASE_FILES: [ExternalFile; 17] = [
    ExternalFile::direct("model_index.json", 609, "6d7b93508390ab91ac5bfbe4aeb4dc2d83f7bb1b05fb069d714b5b0c75f70d44"),
    ExternalFile::direct("scheduler/scheduler_config.json", 479, "af3e45a949aff8b8341ab8b811429ec03fee857a700a1d9477363e4fff9666e2"),
    ExternalFile::direct("text_encoder/config.json", 565, "39b8b2e4b1949e36969caa425b6c81c68bace99198dd9078ce05d16ad401fe7f"),
    ExternalFile::direct("text_encoder_2/config.json", 575, "a892d1c3a69a7e9247a24de2bc1d5891e3109a54696e53be20093af671072c34"),
    ExternalFile::direct("tokenizer/merges.txt", 524_619, "9fd691f7c8039210e0fced15865466c65820d09b63988b0174bfe25de299051a"),
    ExternalFile::direct("tokenizer/special_tokens_map.json", 472, "c4864a9376a8401918425bed71fc14fc0e81f9b59ec45c1cf96cccb2df508eac"),
    ExternalFile::direct("tokenizer/tokenizer_config.json", 737, "19d7b034cb0cc3ce9766c2231373ab8aa8991fc72e2c8f76558bfaae3de0d563"),
    ExternalFile::direct("tokenizer/vocab.json", 1_059_962, "e089ad92ba36837a0d31433e555c8f45fe601ab5c221d4f607ded32d9f7a4349"),
    ExternalFile::direct("tokenizer_2/merges.txt", 524_619, "9fd691f7c8039210e0fced15865466c65820d09b63988b0174bfe25de299051a"),
    ExternalFile::direct("tokenizer_2/special_tokens_map.json", 460, "f118ab3a983206e4f32583448de6bd6aae4ee21869135cef1f5848a753cdaab6"),
    ExternalFile::direct("tokenizer_2/tokenizer_config.json", 725, "c9d23941f76a41cbd50eda9290f57be7828f0a7a677939e9ef181f7e12bd1bdf"),
    ExternalFile::direct("tokenizer_2/vocab.json", 1_059_962, "e089ad92ba36837a0d31433e555c8f45fe601ab5c221d4f607ded32d9f7a4349"),
    ExternalFile::direct("unet/config.json", 1_680, "30ebc70750223e59006f7f2b4e1e6c102570aa19a9c4ae3e1fbe7591332dbae6"),
    ExternalFile::direct("vae/config.json", 642, "0b331c8ac22ded5f9997a144a575c1d113d6169aff262c353f39015bd24a6264"),
    ExternalFile::direct("vae_1_0/config.json", 607, "89a914a415bc3211fe15306c5b1d89774d928d6cc6c91dc8e86208710829355c"),
    ExternalFile::direct("vae_decoder/config.json", 607, "89a914a415bc3211fe15306c5b1d89774d928d6cc6c91dc8e86208710829355c"),
    ExternalFile::direct("vae_encoder/config.json", 607, "89a914a415bc3211fe15306c5b1d89774d928d6cc6c91dc8e86208710829355c"),
];

/// Diffusers pipeline configs (no weights) of SDXL base 1.0: the `config=` directory a 4-channel single-file
/// SDXL checkpoint is loaded with (`from_single_file(..., config=dir, local_files_only=True)`).
pub static SDXL_CONFIG_BASE: ExternalModelSpec = ExternalModelSpec {
    id: "sdxl_config_base_1_0",
    source: ExternalSource::HuggingFace { repo_id: "stabilityai/stable-diffusion-xl-base-1.0", revision: "462165984030d82259a11f4367a4eed129e94a7b" },
    dir: "SDXL/configs/stable-diffusion-xl-base-1.0",
    layout: DirLayout::Exclusive,
    files: &SDXL_CONFIG_BASE_FILES,
};

static SDXL_CONFIG_INPAINT_FILES: [ExternalFile; 14] = [
    ExternalFile::direct("model_index.json", 690, "308a792b15086bf89807b88729ef80d33d5b8264b14a96b57d2c92b5ab7f5a53"),
    ExternalFile::direct("scheduler/scheduler_config.json", 479, "a8e9a8517e8739938ea85819012bad195d41ccb3e11e2501cf196da64099b1f6"),
    ExternalFile::direct("text_encoder/config.json", 746, "46ae51fa8bde7817168d97faa0556cc0ac42ec0a7a53248bab4a52f8f5866f13"),
    ExternalFile::direct("text_encoder_2/config.json", 758, "fbcd3aad07a99e28d4c51550bd798903922083527ec69ad026f25699518af82d"),
    ExternalFile::direct("tokenizer/merges.txt", 524_619, "9fd691f7c8039210e0fced15865466c65820d09b63988b0174bfe25de299051a"),
    ExternalFile::direct("tokenizer/special_tokens_map.json", 472, "c4864a9376a8401918425bed71fc14fc0e81f9b59ec45c1cf96cccb2df508eac"),
    ExternalFile::direct("tokenizer/tokenizer_config.json", 737, "19d7b034cb0cc3ce9766c2231373ab8aa8991fc72e2c8f76558bfaae3de0d563"),
    ExternalFile::direct("tokenizer/vocab.json", 1_059_962, "e089ad92ba36837a0d31433e555c8f45fe601ab5c221d4f607ded32d9f7a4349"),
    ExternalFile::direct("tokenizer_2/merges.txt", 524_619, "9fd691f7c8039210e0fced15865466c65820d09b63988b0174bfe25de299051a"),
    ExternalFile::direct("tokenizer_2/special_tokens_map.json", 460, "f118ab3a983206e4f32583448de6bd6aae4ee21869135cef1f5848a753cdaab6"),
    ExternalFile::direct("tokenizer_2/tokenizer_config.json", 725, "c9d23941f76a41cbd50eda9290f57be7828f0a7a677939e9ef181f7e12bd1bdf"),
    ExternalFile::direct("tokenizer_2/vocab.json", 1_059_962, "e089ad92ba36837a0d31433e555c8f45fe601ab5c221d4f607ded32d9f7a4349"),
    ExternalFile::direct("unet/config.json", 1_932, "aaedf41fbcb29d2d8725241413b8525bd97005dea3f86d8056199469032537e9"),
    ExternalFile::direct("vae/config.json", 659, "b822341e6125efedebffb8e3f35e7288aab06bb4c49853d50a16964bf104a727"),
];

/// Diffusers pipeline configs (no weights) of SDXL inpainting 0.1: the `config=` directory a 9-channel
/// (inpainting) single-file SDXL checkpoint is loaded with.
pub static SDXL_CONFIG_INPAINT: ExternalModelSpec = ExternalModelSpec {
    id: "sdxl_config_inpaint_0_1",
    source: ExternalSource::HuggingFace { repo_id: "diffusers/stable-diffusion-xl-1.0-inpainting-0.1", revision: "115134f363124c53c7d878647567d04daf26e41e" },
    dir: "SDXL/configs/stable-diffusion-xl-1.0-inpainting-0.1",
    layout: DirLayout::Exclusive,
    files: &SDXL_CONFIG_INPAINT_FILES,
};

static SDXL_INPAINT_FP16_FILES: [ExternalFile; 18] = [
    ExternalFile::direct("model_index.json", 690, "308a792b15086bf89807b88729ef80d33d5b8264b14a96b57d2c92b5ab7f5a53"),
    ExternalFile::direct("scheduler/scheduler_config.json", 479, "a8e9a8517e8739938ea85819012bad195d41ccb3e11e2501cf196da64099b1f6"),
    ExternalFile::direct("text_encoder/config.json", 746, "46ae51fa8bde7817168d97faa0556cc0ac42ec0a7a53248bab4a52f8f5866f13"),
    ExternalFile::direct("text_encoder/model.fp16.safetensors", 246_144_867, "fc83cf401d930147807e7c44021c164bcc5508c9d4cc0ff35f4e354685ca9cd0"),
    ExternalFile::direct("text_encoder_2/config.json", 758, "fbcd3aad07a99e28d4c51550bd798903922083527ec69ad026f25699518af82d"),
    ExternalFile::direct("text_encoder_2/model.fp16.safetensors", 1_389_382_884, "a8622bd41f8d359df484fdf9de091edb9337a2fd747f0a5c9c320a62e24d1fd3"),
    ExternalFile::direct("tokenizer/merges.txt", 524_619, "9fd691f7c8039210e0fced15865466c65820d09b63988b0174bfe25de299051a"),
    ExternalFile::direct("tokenizer/special_tokens_map.json", 472, "c4864a9376a8401918425bed71fc14fc0e81f9b59ec45c1cf96cccb2df508eac"),
    ExternalFile::direct("tokenizer/tokenizer_config.json", 737, "19d7b034cb0cc3ce9766c2231373ab8aa8991fc72e2c8f76558bfaae3de0d563"),
    ExternalFile::direct("tokenizer/vocab.json", 1_059_962, "e089ad92ba36837a0d31433e555c8f45fe601ab5c221d4f607ded32d9f7a4349"),
    ExternalFile::direct("tokenizer_2/merges.txt", 524_619, "9fd691f7c8039210e0fced15865466c65820d09b63988b0174bfe25de299051a"),
    ExternalFile::direct("tokenizer_2/special_tokens_map.json", 460, "f118ab3a983206e4f32583448de6bd6aae4ee21869135cef1f5848a753cdaab6"),
    ExternalFile::direct("tokenizer_2/tokenizer_config.json", 725, "c9d23941f76a41cbd50eda9290f57be7828f0a7a677939e9ef181f7e12bd1bdf"),
    ExternalFile::direct("tokenizer_2/vocab.json", 1_059_962, "e089ad92ba36837a0d31433e555c8f45fe601ab5c221d4f607ded32d9f7a4349"),
    ExternalFile::direct("unet/config.json", 1_932, "aaedf41fbcb29d2d8725241413b8525bd97005dea3f86d8056199469032537e9"),
    ExternalFile::direct("unet/diffusion_pytorch_model.fp16.safetensors", 5_135_178_560, "6470840731e98cc16713ddf3ac7ee458c9fdbcb881a98c6727cd4a938f227d3f"),
    ExternalFile::direct("vae/config.json", 659, "b822341e6125efedebffb8e3f35e7288aab06bb4c49853d50a16964bf104a727"),
    ExternalFile::direct("vae/diffusion_pytorch_model.fp16.safetensors", 167_335_338, "4ad62825e5c8b31eefb77355ba6693785619bf7d669d9b1a6fd9f19dec6d65b3"),
];

/// The curated SDXL inpainting model (`diffusers/stable-diffusion-xl-1.0-inpainting-0.1`, fp16 variant, about
/// 6.9 GB): a complete diffusers folder loaded with `from_pretrained(dir, variant="fp16", local_files_only=True)`.
pub static SDXL_INPAINT_FP16: ExternalModelSpec = ExternalModelSpec {
    id: "sdxl_inpaint_0_1_fp16",
    source: ExternalSource::HuggingFace { repo_id: "diffusers/stable-diffusion-xl-1.0-inpainting-0.1", revision: "115134f363124c53c7d878647567d04daf26e41e" },
    dir: "SDXL/stable-diffusion-xl-1.0-inpainting-0.1-fp16",
    layout: DirLayout::Exclusive,
    files: &SDXL_INPAINT_FP16_FILES,
};

// FLUX.2 klein 4B. Every spec is `Shared`: the three BFL specs publish into the package root
// `FLUX.2-klein-4B` (the same relative paths `ms_config::flux2_klein_*_dir` name and the
// backend's former download produced, so an existing download is adopted, never refetched);
// each GGUF component has a sibling folder holding one file per quant. The BFL repository
// carries more (the repo-root single-file checkpoint, sample images); only the diffusers
// folders the engine loads are listed. Pins: the Hugging Face tree API at each commit, small
// BFL files cross-checked against an existing download (git blob sha1 equal to the tree's).

/// The FLUX.2 klein 4B package root, relative to `side_models`
/// (`ms_config::Flux2Variant::Klein4B.dir_name()`; a catalog test checks the agreement).
const FLUX2_KLEIN_4B_DIR: &str = "FLUX.2-klein-4B";
/// The BFL repository and commit every FLUX.2 klein 4B bf16 file is pinned at.
const FLUX2_KLEIN_4B_BFL_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "black-forest-labs/FLUX.2-klein-4B", revision: "e7b7dc27f91deacad38e78976d1f2b499d76a294" };
/// The unsloth GGUF quantizations of the FLUX.2 klein 4B transformer.
const FLUX2_KLEIN_4B_TRANSFORMER_GGUF_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "unsloth/FLUX.2-klein-4B-GGUF", revision: "0084d1df98e2e2137fe776d55170bc4792ec1d66" };
/// The unsloth GGUF quantizations of Qwen3-4B, the FLUX.2 klein 4B text encoder.
const FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_SOURCE: ExternalSource = ExternalSource::HuggingFace { repo_id: "unsloth/Qwen3-4B-GGUF", revision: "22c9fc8a8c7700b76a1789366280a6a5a1ad1120" };
/// Folder of the transformer GGUF files, relative to `side_models`.
const FLUX2_KLEIN_4B_TRANSFORMER_GGUF_DIR: &str = "FLUX.2-klein-4B/transformer_gguf";
/// Folder of the text-encoder GGUF files, relative to `side_models`.
const FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_DIR: &str = "FLUX.2-klein-4B/text_encoder_gguf";
/// The bf16 transformer folder inside the package root (`ms_config::flux2_klein_transformer_dir`).
const FLUX2_KLEIN_TRANSFORMER_SUBDIR: &str = "transformer";
/// The bf16 text-encoder folder inside the package root (`ms_config::flux2_klein_text_encoder_dir`).
const FLUX2_KLEIN_TEXT_ENCODER_SUBDIR: &str = "text_encoder";

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

/// The FLUX.2 klein 4B spec holding the weights of `axis` in `format` (exhaustive: every
/// format has a pinned 4B spec on both axes).
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

/// The pinned spec holding the FLUX.2 klein weights of `axis` in `format` for `variant`.
///
/// `None` when the variant is not downloaded from this catalog
/// (`Flux2Variant::downloads_from_catalog`, today 9B) or does not offer `format` on `axis`
/// (`Flux2WeightFormat::offered_for`).
#[must_use]
pub fn flux2_klein_spec(variant: Flux2Variant, axis: Flux2WeightAxis, format: Flux2WeightFormat) -> Option<&'static ExternalModelSpec> {
    if !variant.downloads_from_catalog() || !Flux2WeightFormat::offered_for(variant, axis).contains(&format) {
        return None;
    }
    match variant {
        Flux2Variant::Klein4B => Some(flux2_klein_4b_weights_spec(axis, format)),
        Flux2Variant::Klein9B => None,
    }
}

/// The bundle of specs a FLUX.2 klein selection needs installed: the shared base part, then
/// the transformer spec of `transformer`, then the text-encoder spec of `text_encoder`.
///
/// Empty for a variant this catalog does not download (9B). A format the variant does not
/// offer contributes no spec; callers normalize their selection with
/// `Flux2WeightFormat::offered_for` first (every format is offered for 4B).
#[must_use]
pub fn flux2_klein_package_specs(variant: Flux2Variant, transformer: Flux2WeightFormat, text_encoder: Flux2WeightFormat) -> Vec<&'static ExternalModelSpec> {
    let base = match variant {
        Flux2Variant::Klein4B => &FLUX2_KLEIN_4B_BASE,
        Flux2Variant::Klein9B => return Vec::new(),
    };
    let mut specs = vec![base];
    specs.extend(flux2_klein_spec(variant, Flux2WeightAxis::Transformer, transformer));
    specs.extend(flux2_klein_spec(variant, Flux2WeightAxis::TextEncoder, text_encoder));
    specs
}

/// Where the FLUX.2 klein weights of `axis` in `format` live once installed under
/// `side_models_root` (pass `ms_config::side_models_dir()`): the bf16 component FOLDER
/// (`<package>/transformer` or `<package>/text_encoder`, the same paths as
/// `ms_config::flux2_klein_transformer_dir` / `flux2_klein_text_encoder_dir`) or the GGUF
/// FILE. Pure path joining: checks nothing on disk.
///
/// `None` exactly when [`flux2_klein_spec`] is `None`.
#[must_use]
pub fn flux2_klein_weights_path(side_models_root: &Path, variant: Flux2Variant, axis: Flux2WeightAxis, format: Flux2WeightFormat) -> Option<PathBuf> {
    let spec = flux2_klein_spec(variant, axis, format)?;
    let dir = spec.local_dir(side_models_root);
    if format.is_gguf() {
        // A GGUF spec pins exactly one file (catalog test), which is the component.
        return spec.files.first().map(|file| join_relative(&dir, file.path));
    }
    Some(dir.join(match axis {
        Flux2WeightAxis::Transformer => FLUX2_KLEIN_TRANSFORMER_SUBDIR,
        Flux2WeightAxis::TextEncoder => FLUX2_KLEIN_TEXT_ENCODER_SUBDIR,
    }))
}


/// The PaddleOCR-VL weights a user can select. All three run through the same vendored
/// 1.6 model code in the backend; only the downloaded directory differs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaddleVlVariant {
    /// Official PaddleOCR-VL 1.6 (default).
    #[default]
    Official16,
    /// Official PaddleOCR-VL 1.5.
    Official15,
    /// Community fine-tune for Japanese manga.
    MangaJa,
}

impl PaddleVlVariant {
    /// Every variant, in UI order.
    pub const ALL: [Self; 3] = [Self::Official16, Self::Official15, Self::MangaJa];

    /// Stable persistence / protocol key (`OCR.params.paddle_vl.model`, backend
    /// `paddle_vl_model`). Never localized, never renamed.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Official16 => "official_1_6",
            Self::Official15 => "official_1_5",
            Self::MangaJa => "manga_ja",
        }
    }

    /// Parses a persisted key (trimmed, ASCII case-insensitive). `None` for an unknown key;
    /// the caller decides the fallback (normally `PaddleVlVariant::default()`).
    #[must_use]
    pub fn from_key(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        Self::ALL.into_iter().find(|variant| variant.key() == normalized)
    }

    /// The pinned download spec of this variant.
    #[must_use]
    pub fn spec(self) -> &'static ExternalModelSpec {
        match self {
            Self::Official16 => &PADDLE_VL_OFFICIAL_1_6,
            Self::Official15 => &PADDLE_VL_OFFICIAL_1_5,
            Self::MangaJa => &PADDLE_VL_MANGA_JA,
        }
    }
}

/// Every external spec the application downloads: this catalog plus the EasyOCR scripts.
#[must_use]
pub fn all_specs() -> Vec<&'static ExternalModelSpec> {
    let mut specs: Vec<&'static ExternalModelSpec> = vec![
        &BABERU_OCR,
        &PADDLE_VL_OFFICIAL_1_6,
        &PADDLE_VL_OFFICIAL_1_5,
        &PADDLE_VL_MANGA_JA,
        &SURYA_DETECTION_2025_05_07,
        &SURYA_RECOGNITION_2025_09_23,
        &SDXL_CONFIG_BASE,
        &SDXL_CONFIG_INPAINT,
        &SDXL_INPAINT_FP16,
        &FLUX2_KLEIN_4B_BASE,
        &FLUX2_KLEIN_4B_TRANSFORMER_BF16,
        &FLUX2_KLEIN_4B_TEXT_ENCODER_BF16,
        &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q8_0,
        &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q6_K,
        &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q5_K_M,
        &FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q4_K_M,
        &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q8_0,
        &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q6_K,
        &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q5_K_M,
        &FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q4_K_M,
    ];
    specs.extend(super::easyocr::EasyOcrScript::ALL.into_iter().map(super::easyocr::easyocr_spec));
    specs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_models::external::{FileOrigin, validate_spec};

    #[test]
    fn every_spec_passes_validation() {
        for spec in all_specs() {
            let result = validate_spec(spec);
            assert!(result.is_ok(), "spec {}: {result:?}", spec.id);
        }
    }

    #[test]
    fn spec_ids_are_unique_and_only_shared_dirs_coincide() {
        let specs = all_specs();
        for (index, spec) in specs.iter().enumerate() {
            for other in &specs[index + 1..] {
                assert_ne!(spec.id, other.id);
                if spec.dir == other.dir {
                    assert_eq!((spec.layout, other.layout), (DirLayout::Shared, DirLayout::Shared), "{} / {}", spec.id, other.id);
                }
            }
        }
    }

    #[test]
    fn a_file_shared_between_specs_has_one_pin() {
        let specs = all_specs();
        for spec in &specs {
            for other in &specs {
                if spec.dir != other.dir {
                    continue;
                }
                for file in spec.files {
                    if let Some(twin) = other.files.iter().find(|candidate| candidate.path == file.path) {
                        assert_eq!(file, twin, "{} vs {}: {}", spec.id, other.id, file.path);
                    }
                }
            }
        }
    }

    #[test]
    fn totals_match_the_pinned_tables() {
        assert_eq!(BABERU_OCR.total_bytes(), 242_908_475);
        assert_eq!(PADDLE_VL_OFFICIAL_1_6.total_bytes(), 1_930_288_690);
        assert_eq!(PADDLE_VL_OFFICIAL_1_5.total_bytes(), 1_930_288_690);
        assert_eq!(PADDLE_VL_MANGA_JA.total_bytes(), 1_930_274_598);
        assert_eq!(SURYA_DETECTION_2025_05_07.total_bytes(), 76_939_626);
        assert_eq!(SURYA_RECOGNITION_2025_09_23.total_bytes(), 1_439_035_487);
        assert_eq!(SDXL_CONFIG_BASE.total_bytes(), 3_177_927);
        assert_eq!(SDXL_CONFIG_INPAINT.total_bytes(), 3_176_820);
        assert_eq!(SDXL_INPAINT_FP16.total_bytes(), 6_941_218_469);
        assert_eq!(FLUX2_KLEIN_4B_BASE.total_bytes(), 184_016_738);
        assert_eq!(FLUX2_KLEIN_4B_TRANSFORMER_BF16.total_bytes(), 7_751_109_744);
        assert_eq!(FLUX2_KLEIN_4B_TEXT_ENCODER_BF16.total_bytes(), 8_045_014_847);
        assert_eq!(FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q8_0.total_bytes(), 4_300_644_928);
        assert_eq!(FLUX2_KLEIN_4B_TRANSFORMER_GGUF_Q4_K_M.total_bytes(), 2_604_311_104);
        assert_eq!(FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q8_0.total_bytes(), 4_280_405_792);
        assert_eq!(FLUX2_KLEIN_4B_TEXT_ENCODER_GGUF_Q4_K_M.total_bytes(), 2_497_281_312);
    }

    #[test]
    fn surya_checkpoints_ship_their_manifest_and_sdxl_configs_carry_no_weights() {
        for spec in [&SURYA_DETECTION_2025_05_07, &SURYA_RECOGNITION_2025_09_23] {
            assert!(spec.files.iter().any(|file| file.path == "manifest.json"), "{}", spec.id);
            assert!(matches!(spec.source, ExternalSource::Url { base_url } if base_url.starts_with("https://models.datalab.to/")));
        }
        for spec in [&SDXL_CONFIG_BASE, &SDXL_CONFIG_INPAINT] {
            assert!(spec.files.iter().all(|file| !file.path.ends_with(".safetensors") && file.size < 2_000_000), "{}", spec.id);
            assert!(spec.files.iter().any(|file| file.path == "model_index.json"));
        }
        // The curated model is the inpainting config repo at the same commit, plus fp16 weights.
        assert_eq!(SDXL_INPAINT_FP16.source, SDXL_CONFIG_INPAINT.source);
        assert_eq!(SDXL_INPAINT_FP16.files.iter().filter(|file| file.path.ends_with(".fp16.safetensors")).count(), 4);
        assert!(all_specs().iter().flat_map(|spec| spec.files).filter(|file| file.origin != FileOrigin::Direct).all(|file| file.path.ends_with(".pth")));
    }

    /// Every 4B spec in a stable order: base, bf16 transformer, bf16 encoder, then the GGUF
    /// specs of both axes.
    fn flux2_klein_4b_specs() -> Vec<&'static ExternalModelSpec> {
        all_specs().into_iter().filter(|spec| spec.id.starts_with("flux2_klein_4b_")).collect()
    }

    #[test]
    fn every_offered_flux2_format_resolves_to_its_own_spec_and_nothing_else_does() {
        let mut resolved: Vec<&str> = Vec::new();
        for variant in Flux2Variant::all() {
            for axis in Flux2WeightAxis::all() {
                for &format in Flux2WeightFormat::all() {
                    let offered = variant.downloads_from_catalog() && Flux2WeightFormat::offered_for(variant, axis).contains(&format);
                    let spec = flux2_klein_spec(variant, axis, format);
                    assert_eq!(spec.is_some(), offered, "{variant:?} {axis:?} {format:?}");
                    assert_eq!(flux2_klein_weights_path(Path::new("side"), variant, axis, format).is_some(), offered);
                    if let Some(spec) = spec {
                        let axis_token = match axis {
                            Flux2WeightAxis::Transformer => "transformer",
                            Flux2WeightAxis::TextEncoder => "text_encoder",
                        };
                        let kind = if format.is_gguf() { "gguf_" } else { "" };
                        assert_eq!(spec.id, format!("flux2_klein_4b_{axis_token}_{kind}{}", format.wire()));
                        if format.is_gguf() {
                            assert_eq!(spec.files.len(), 1, "{}", spec.id);
                            assert!(spec.files[0].path.ends_with(&format!("-{}.gguf", format.quant_token().unwrap_or_default())), "{}", spec.id);
                        }
                        resolved.push(spec.id);
                    }
                }
            }
        }
        // Every 4B weights spec is reachable exactly once; only the base is not a weights spec.
        let mut expected: Vec<&str> = flux2_klein_4b_specs().iter().map(|spec| spec.id).filter(|id| *id != FLUX2_KLEIN_4B_BASE.id).collect();
        expected.sort_unstable();
        resolved.sort_unstable();
        assert_eq!(resolved, expected);
    }

    #[test]
    fn flux2_klein_4b_specs_are_shared_and_live_under_the_variant_dir() {
        let specs = flux2_klein_4b_specs();
        assert_eq!(specs.len(), 11);
        let package = Flux2Variant::Klein4B.dir_name();
        for spec in specs {
            assert_eq!(spec.layout, DirLayout::Shared, "{}", spec.id);
            assert!(spec.dir == package || spec.dir.starts_with(&format!("{package}/")), "{}", spec.id);
        }
        assert_eq!(FLUX2_KLEIN_4B_DIR, package);
        // The bf16 parts must not overlap: one file has one owning spec in the package root.
        for (a, b) in [(&FLUX2_KLEIN_4B_BASE, &FLUX2_KLEIN_4B_TRANSFORMER_BF16), (&FLUX2_KLEIN_4B_BASE, &FLUX2_KLEIN_4B_TEXT_ENCODER_BF16), (&FLUX2_KLEIN_4B_TRANSFORMER_BF16, &FLUX2_KLEIN_4B_TEXT_ENCODER_BF16)] {
            assert!(a.files.iter().all(|file| b.files.iter().all(|other| other.path != file.path)), "{} / {}", a.id, b.id);
        }
    }

    #[test]
    fn flux2_klein_4b_bf16_paths_agree_with_the_config_dirs() {
        let variant = Flux2Variant::Klein4B;
        let package = ms_config::flux2_klein_dir(variant);
        assert_eq!(package.strip_prefix(ms_config::side_models_dir()).ok(), Some(Path::new(FLUX2_KLEIN_4B_DIR)));
        let side = Path::new("side");
        let root = join_relative(side, FLUX2_KLEIN_4B_DIR);
        for (axis, config_dir, spec) in [
            (Flux2WeightAxis::Transformer, ms_config::flux2_klein_transformer_dir(variant), &FLUX2_KLEIN_4B_TRANSFORMER_BF16),
            (Flux2WeightAxis::TextEncoder, ms_config::flux2_klein_text_encoder_dir(variant, false), &FLUX2_KLEIN_4B_TEXT_ENCODER_BF16),
        ] {
            let relative = config_dir.strip_prefix(&package).ok().map(Path::to_path_buf);
            let folder = flux2_klein_weights_path(side, variant, axis, Flux2WeightFormat::Bf16);
            assert_eq!(folder.as_deref().and_then(|path| path.strip_prefix(&root).ok()), relative.as_deref());
            let subdir = relative.as_deref().and_then(Path::to_str).unwrap_or_default();
            assert!(spec.files.iter().all(|file| file.path.starts_with(&format!("{subdir}/"))), "{}", spec.id);
        }
        let vae = ms_config::flux2_klein_vae_dir(variant);
        let vae_relative = vae.strip_prefix(&package).ok().and_then(Path::to_str).unwrap_or_default();
        assert!(FLUX2_KLEIN_4B_BASE.files.iter().any(|file| file.path == format!("{vae_relative}/config.json")));
        assert!(FLUX2_KLEIN_4B_BASE.files.iter().any(|file| file.path == format!("{vae_relative}/diffusion_pytorch_model.safetensors")));
        // The GGUF encoder takes its config and tokenizer from the base part.
        assert!(FLUX2_KLEIN_4B_BASE.files.iter().any(|file| file.path == "transformer/config.json"));
        assert!(FLUX2_KLEIN_4B_BASE.files.iter().any(|file| file.path == "text_encoder/config.json"));
        assert!(FLUX2_KLEIN_4B_BASE.files.iter().any(|file| file.path == "tokenizer/tokenizer.json"));
    }

    #[test]
    fn flux2_klein_gguf_paths_and_bundles() {
        let side = Path::new("side");
        let path = flux2_klein_weights_path(side, Flux2Variant::Klein4B, Flux2WeightAxis::Transformer, Flux2WeightFormat::Q80);
        assert_eq!(path, Some(side.join("FLUX.2-klein-4B").join("transformer_gguf").join("flux-2-klein-4b-Q8_0.gguf")));
        let path = flux2_klein_weights_path(side, Flux2Variant::Klein4B, Flux2WeightAxis::TextEncoder, Flux2WeightFormat::Q4KM);
        assert_eq!(path, Some(side.join("FLUX.2-klein-4B").join("text_encoder_gguf").join("Qwen3-4B-Q4_K_M.gguf")));

        let ids = |specs: Vec<&'static ExternalModelSpec>| specs.into_iter().map(|spec| spec.id).collect::<Vec<_>>();
        assert_eq!(ids(flux2_klein_package_specs(Flux2Variant::Klein4B, Flux2WeightFormat::Bf16, Flux2WeightFormat::Bf16)), ["flux2_klein_4b_base", "flux2_klein_4b_transformer_bf16", "flux2_klein_4b_text_encoder_bf16"]);
        assert_eq!(ids(flux2_klein_package_specs(Flux2Variant::Klein4B, Flux2WeightFormat::Q6K, Flux2WeightFormat::Q5KM)), ["flux2_klein_4b_base", "flux2_klein_4b_transformer_gguf_q6_k", "flux2_klein_4b_text_encoder_gguf_q5_k_m"]);
        for &transformer in Flux2WeightFormat::all() {
            for &text_encoder in Flux2WeightFormat::all() {
                assert_eq!(flux2_klein_package_specs(Flux2Variant::Klein4B, transformer, text_encoder).len(), 3);
                assert!(flux2_klein_package_specs(Flux2Variant::Klein9B, transformer, text_encoder).is_empty());
            }
        }
    }

    #[test]
    fn official_variants_differ_only_in_weights() {
        let differing: Vec<&str> = PADDLE_VL_OFFICIAL_1_6
            .files
            .iter()
            .zip(PADDLE_VL_OFFICIAL_1_5.files)
            .filter(|(a, b)| a != b)
            .map(|(a, _)| a.path)
            .collect();
        assert_eq!(differing, vec!["model.safetensors"]);
    }

    #[test]
    fn variant_keys_round_trip_and_default_is_1_6() {
        assert_eq!(PaddleVlVariant::default(), PaddleVlVariant::Official16);
        for variant in PaddleVlVariant::ALL {
            assert_eq!(PaddleVlVariant::from_key(variant.key()), Some(variant));
        }
        assert_eq!(PaddleVlVariant::from_key("  MANGA_JA "), Some(PaddleVlVariant::MangaJa));
        assert_eq!(PaddleVlVariant::from_key("official"), None);
        assert_eq!(PaddleVlVariant::Official15.spec().id, "paddle_vl_official_1_5");
    }

    #[test]
    fn baberu_layout_names_are_catalog_files() {
        let root = Path::new("models").join("BaberuOCR");
        let files = baberu_files(&root);
        for (path, relative) in [
            (&files.vision, BABERU_VISION),
            (&files.prefill, BABERU_PREFILL),
            (&files.step, BABERU_STEP),
            (&files.vocab, BABERU_VOCAB),
        ] {
            assert!(BABERU_OCR.files.iter().any(|file| file.path == relative));
            assert_eq!(*path, join_relative(&root, relative));
        }
        assert_eq!(files.vision, root.join("onnx").join("vision_fp16.onnx"));
    }
}
