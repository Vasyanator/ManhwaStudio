/*
File: crates/ms-sysprobe/src/ai_models/external_catalog.rs

Purpose:
The pinned catalog of EXTERNAL (third-party Hugging Face) models the application downloads
itself: Baberu OCR and the three PaddleOCR-VL variants. This file is the single owner of each
model's repository id, pinned commit, file allowlist (exact size + sha256) and target
directory under the caller's `side_models` root. Nothing else in the workspace may spell
these values; the Python backend receives resolved paths and never downloads these models.

Key items:
- `BABERU_OCR`, `PADDLE_VL_OFFICIAL_1_6`, `PADDLE_VL_OFFICIAL_1_5`, `PADDLE_VL_MANGA_JA`:
  the `ExternalModelSpec` statics consumed by `external::download_external_model` /
  `external::installed_dir`.
- `BaberuFiles` + `baberu_files()`: the one owner of the Baberu on-disk layout names; uses
  the same `const` strings as the `BABERU_OCR` file table.
- `PaddleVlVariant`: typed variant selector with its persistence key (`key` / `from_key`).

Notes:
- Changing a pin (revision or any file entry) changes the spec identity recorded in the
  completion marker, so an existing install reads as not installed until re-downloaded.
- Values were taken from the Hugging Face tree API at the pinned revisions; tests assert
  hex lengths, unique directories and per-spec byte totals.
*/

use std::path::{Path, PathBuf};

use super::external::{ExternalFile, ExternalModelSpec};

/// Baberu vision encoder (fp16 ONNX), relative to the model directory.
const BABERU_VISION: &str = "onnx/vision_fp16.onnx";
/// Baberu decoder prefill graph (int8 ONNX), relative to the model directory.
const BABERU_PREFILL: &str = "onnx/decoder_prefill_int8.onnx";
/// Baberu decoder single-step graph (int8 ONNX), relative to the model directory.
const BABERU_STEP: &str = "onnx/decoder_step_int8.onnx";
/// Baberu tokenizer vocabulary, relative to the model directory.
const BABERU_VOCAB: &str = "tokenizer/vocab.json";

static BABERU_FILES: [ExternalFile; 5] = [
    ExternalFile { path: BABERU_VISION, size: 172_917_304, sha256: "4ce333804846b23f1983376efe363c164d4624ecc6bd16f99a15a859d8423117" },
    ExternalFile { path: BABERU_PREFILL, size: 35_133_596, sha256: "91de16d4e70adaadc0c76ea536f0195b79f54b52e3279aeaac8fc06b46824c60" },
    ExternalFile { path: BABERU_STEP, size: 34_715_466, sha256: "f6164e9daee6622891d2a52394f143faa7f0ccfdfb90e8d6443193aff36d4c2d" },
    ExternalFile { path: BABERU_VOCAB, size: 130_761, sha256: "681de4eb83154a397af49e2ddae4705041b12a22e16fe00860b829e889e5f47c" },
    ExternalFile { path: "LICENSE", size: 11_348, sha256: "186332043e82fcfdbeb0b8e0e8d77b63d97ffa3dd9d91ccf7cef1377d8ab345b" },
];

/// Baberu OCR (`genshiai-daichi/baberu-ocr`), native ONNX vision encoder + int8 decoder.
pub static BABERU_OCR: ExternalModelSpec = ExternalModelSpec {
    id: "baberu_ocr",
    repo_id: "genshiai-daichi/baberu-ocr",
    revision: "cc74c99765dc5f089d12a8f2e11692bb029a4391",
    dir: "BaberuOCR",
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
const PADDLE_VL_OFFICIAL_CONFIG: ExternalFile = ExternalFile { path: "config.json", size: 2_059, sha256: "ce7f4565f8b1db78532ad5d1b9ebe55c2139d49bd4cb04778b580a08a598f171" };
const PADDLE_VL_OFFICIAL_GENERATION: ExternalFile = ExternalFile { path: "generation_config.json", size: 133, sha256: "a6701d78ab3b4d972307cdec3b69d4c13f46e0d5140514f50ab7d84259324b94" };
const PADDLE_VL_OFFICIAL_PREPROCESSOR: ExternalFile = ExternalFile { path: "preprocessor_config.json", size: 641, sha256: "111872ab1e8bb7fd040ac5087bfced7ab8f011f02139b088cba294964c3b1d0e" };
const PADDLE_VL_PROCESSOR: ExternalFile = ExternalFile { path: "processor_config.json", size: 137, sha256: "1568858960a9760c54431dae693a6152e601ff55cdf6d2eab97a4a99958faea0" };
const PADDLE_VL_OFFICIAL_TOKENIZER_JSON: ExternalFile = ExternalFile { path: "tokenizer.json", size: 11_189_060, sha256: "c8a215a59183d0d0781adc33bacd3ce6162716f7fd568fb30234a74d69803a7d" };
const PADDLE_VL_TOKENIZER_MODEL: ExternalFile = ExternalFile { path: "tokenizer.model", size: 1_614_363, sha256: "34ef7db83df785924fb83d7b887b6e822a031c56e15cff40aaf9b982988180df" };
const PADDLE_VL_OFFICIAL_TOKENIZER_CONFIG: ExternalFile = ExternalFile { path: "tokenizer_config.json", size: 186_947, sha256: "1f979337347cc0cb72a6282d8a23ed183539aa81a87a906f022aee2bab83c7c5" };
const PADDLE_VL_OFFICIAL_SPECIAL_TOKENS: ExternalFile = ExternalFile { path: "special_tokens_map.json", size: 1_151, sha256: "d3a125c03103deb2acaf7730791bdbbf196f620e5a2213b664511ff9b4b25bab" };
const PADDLE_VL_ADDED_TOKENS: ExternalFile = ExternalFile { path: "added_tokens.json", size: 25_381, sha256: "f59f889088e0fe21c523e7cf121bb6dca3b0bb148cb7159fbb4572c74dfc5644" };
const PADDLE_VL_OFFICIAL_CHAT_TEMPLATE: ExternalFile = ExternalFile { path: "chat_template.jinja", size: 1_474, sha256: "2f27812dab7f333e471884e0c803d807f11953d5453140dfb1aaba234f872bc8" };
const PADDLE_VL_OFFICIAL_LICENSE: ExternalFile = ExternalFile { path: "LICENSE", size: 11_376, sha256: "b8c4d7deccd236af023af1c88c4d4e8f0fc2f41914e0fb23a3ec9678fb5a8456" };

static PADDLE_VL_OFFICIAL_1_6_FILES: [ExternalFile; 12] = [
    PADDLE_VL_OFFICIAL_CONFIG,
    PADDLE_VL_OFFICIAL_GENERATION,
    ExternalFile { path: "model.safetensors", size: 1_917_255_968, sha256: "85a479d506a11e724e7285d395c551be69f41dbc16b6342d3cacfb189aed71db" },
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
    repo_id: "PaddlePaddle/PaddleOCR-VL-1.6",
    revision: "c5630abae1d940eafe0697512a0325494b02ab42",
    dir: "PaddleOCR-VL/official_1_6",
    files: &PADDLE_VL_OFFICIAL_1_6_FILES,
};

static PADDLE_VL_OFFICIAL_1_5_FILES: [ExternalFile; 12] = [
    PADDLE_VL_OFFICIAL_CONFIG,
    PADDLE_VL_OFFICIAL_GENERATION,
    ExternalFile { path: "model.safetensors", size: 1_917_255_968, sha256: "d557c9d8997ae57ed3b1b33bdf347be878cc335687f32ca105341c16973f8958" },
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
    repo_id: "PaddlePaddle/PaddleOCR-VL-1.5",
    revision: "2a4195faa5e7914c12f2fc601d72c81caf8d2da5",
    dir: "PaddleOCR-VL/official_1_5",
    files: &PADDLE_VL_OFFICIAL_1_5_FILES,
};

// The manga fine-tune ships no LICENSE file (its model card states Apache-2.0).
static PADDLE_VL_MANGA_JA_FILES: [ExternalFile; 11] = [
    ExternalFile { path: "config.json", size: 1_987, sha256: "928aaf78567a273cb73ede3671253ec4e38eb60c27a30e945bcd13b4131a0147" },
    ExternalFile { path: "generation_config.json", size: 133, sha256: "cf202f984e003e92dceaa27e749b60b4e6e1b566a1df8486b5b41adf1d016cea" },
    ExternalFile { path: "model.safetensors", size: 1_917_255_968, sha256: "71fcee0e3618582d4c8acc705242aa79b471b6134e7023bf3820642ba638b602" },
    ExternalFile { path: "preprocessor_config.json", size: 687, sha256: "f417a7f977820dfe6828f3ec2e461c027fdb0662f25cae4e841ec1028e0b988a" },
    PADDLE_VL_PROCESSOR,
    ExternalFile { path: "tokenizer.json", size: 11_187_679, sha256: "f90f04fd8e5eb6dfa380f37d10c87392de8438dccb6768a2486b5a96ee76dba6" },
    PADDLE_VL_TOKENIZER_MODEL,
    ExternalFile { path: "tokenizer_config.json", size: 185_545, sha256: "67c651ba09c22151a1fff31e8773a24f7607aef1541aa2f200b48552ed30e894" },
    ExternalFile { path: "special_tokens_map.json", size: 1_152, sha256: "215bf3a1b155fafe3497f8790bedf280af92d29c2f0286c2f87a5c78baff8f7c" },
    PADDLE_VL_ADDED_TOKENS,
    ExternalFile { path: "chat_template.jinja", size: 1_566, sha256: "344fea8b69546526a00996468f86f583fd65441582a36f2fa4abc794aa94094c" },
];

/// PaddleOCR-VL fine-tuned for Japanese manga (`jzhang533/PaddleOCR-VL-For-Manga`).
pub static PADDLE_VL_MANGA_JA: ExternalModelSpec = ExternalModelSpec {
    id: "paddle_vl_manga_ja",
    repo_id: "jzhang533/PaddleOCR-VL-For-Manga",
    revision: "1e8aa5f1dd90cc86fe9137c9c0b26ebde613cfe8",
    dir: "PaddleOCR-VL/manga_ja",
    files: &PADDLE_VL_MANGA_JA_FILES,
};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_models::external::validate_spec;

    fn all_specs() -> [&'static ExternalModelSpec; 4] {
        [&BABERU_OCR, &PADDLE_VL_OFFICIAL_1_6, &PADDLE_VL_OFFICIAL_1_5, &PADDLE_VL_MANGA_JA]
    }

    #[test]
    fn every_spec_passes_validation() {
        for spec in all_specs() {
            let result = validate_spec(spec);
            assert!(result.is_ok(), "spec {}: {result:?}", spec.id);
        }
    }

    #[test]
    fn spec_ids_and_dirs_are_unique() {
        let specs = all_specs();
        for (index, spec) in specs.iter().enumerate() {
            for other in &specs[index + 1..] {
                assert_ne!(spec.id, other.id);
                assert_ne!(spec.dir, other.dir);
            }
        }
    }

    #[test]
    fn totals_match_the_pinned_tables() {
        assert_eq!(BABERU_OCR.total_bytes(), 242_908_475);
        assert_eq!(PADDLE_VL_OFFICIAL_1_6.total_bytes(), 1_930_288_690);
        assert_eq!(PADDLE_VL_OFFICIAL_1_5.total_bytes(), 1_930_288_690);
        assert_eq!(PADDLE_VL_MANGA_JA.total_bytes(), 1_930_274_598);
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
