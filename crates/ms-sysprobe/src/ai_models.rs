/*
FILE OVERVIEW: crates/ms-sysprobe/src/ai_models.rs
Application-managed AI model resolver and lazy Hugging Face downloader.

Main responsibilities:
- Define the canonical ManhwaStudio_AI_Models file layout for Rust callers.
- Download missing files of the `main` revision directly into ManhwaStudio_AI_Models (no HF
  cache blobs or symlinks) through the shared resumable fetch (`ai_models/resumable.rs`):
  `<file>.part` staging next to the target, HTTP `Range` resume, retry with capped backoff,
  connect/read timeouts, size verified against the server's announcement, rename on success.
- Return concrete local paths for Rust model initialization code.

- Mount the `external` (pinned third-party model downloader), `external_catalog` and
  `resumable` (shared HTTP mechanics) submodules; see `ai_models/MODULE_README.md`.

Notes:
- Library-managed caches such as EasyOCR and Surya are intentionally absent here.
- Callers must invoke this from worker/background code, not the GUI thread.
- The main repository is not pinned: there is no hash to check, so a file is published once
  exactly the announced number of bytes is staged. Requests carry no Hugging Face token (the
  repository is public). The `ensure_*` API has no cancel parameter, so a download runs until
  it completes or its retries are exhausted.
*/

// The HTTP download stack is native-only (`ureq` is compiled out on wasm); the resolver
// keeps its public signatures on both targets and stubs the actual download on the web
// build.
#[cfg(not(target_arch = "wasm32"))]
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::AtomicBool;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Mutex, OnceLock};

#[cfg(not(target_arch = "wasm32"))]
use resumable::{PartRequest, RangeFetcher, ResumableFetch, RetryPolicy, StreamError, UreqFetcher, hf_resolve_url};

/// Pinned external (third-party) model downloader and status probe; see `ai_models/MODULE_README.md`.
pub mod external;
/// The pinned external-model catalog (Baberu OCR, PaddleOCR-VL variants).
pub mod external_catalog;
/// Shared HTTP mechanics of both downloaders: resolve URLs and the resumable per-file fetch.
#[cfg(not(target_arch = "wasm32"))]
mod resumable;

pub const HF_OWNER: &str = "Vasyanator2";
pub const HF_REPO_NAME: &str = "ManhwaStudio_AI_Models";
/// Revision of the app-managed repository the downloader resolves (unpinned branch).
#[cfg(not(target_arch = "wasm32"))]
const HF_REVISION: &str = "main";
/// Log prefix of the main-repository downloader.
#[cfg(not(target_arch = "wasm32"))]
const LOG_TAG: &str = "[ai-models]";

const PADDLE_DET_ONNX: &str = "ONNX/PaddleOCR/detection/v5/det.onnx";
const PADDLE_DET_CONFIG: &str = "ONNX/PaddleOCR/detection/v5/config.json";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MangaOcrOnnxModel {
    Base,
    Model2025,
}

pub type ModelDownloadReporter<'a> = Option<&'a mut dyn FnMut()>;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct PaddleLanguageSpec {
    key: &'static str,
    dir_name: &'static str,
}

const PADDLE_LANGUAGES: &[PaddleLanguageSpec] = &[
    PaddleLanguageSpec {
        key: "english_v5",
        dir_name: "english",
    },
    PaddleLanguageSpec {
        key: "latin_v5",
        dir_name: "latin",
    },
    PaddleLanguageSpec {
        key: "eslav_v5",
        dir_name: "eslav",
    },
    PaddleLanguageSpec {
        key: "korean_v5",
        dir_name: "korean",
    },
    PaddleLanguageSpec {
        key: "chinese_v5",
        dir_name: "chinese",
    },
    PaddleLanguageSpec {
        key: "thai_v5",
        dir_name: "thai",
    },
    PaddleLanguageSpec {
        key: "greek_v5",
        dir_name: "greek",
    },
    PaddleLanguageSpec {
        key: "arabic_v3",
        dir_name: "arabic",
    },
    PaddleLanguageSpec {
        key: "hindi_v3",
        dir_name: "hindi",
    },
    PaddleLanguageSpec {
        key: "telugu_v3",
        dir_name: "telugu",
    },
    PaddleLanguageSpec {
        key: "tamil_v3",
        dir_name: "tamil",
    },
];

const MANGA_OCR_BASE_FILES: &[&str] = &[
    "README.md",
    "config.json",
    "decoder_model.onnx",
    "encoder_model.onnx",
    "generation_config.json",
    "preprocessor_config.json",
];

const MANGA_OCR_2025_FILES: &[&str] = &[
    "README.md",
    "config.json",
    "decoder_model.onnx",
    "encoder_model.onnx",
    "generation_config.json",
    "preprocessor_config.json",
    "special_tokens_map.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "vocab.txt",
];

#[cfg(not(target_arch = "wasm32"))]
static DOWNLOAD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn ensure_paddle_ocr_detector(models_root: &Path) -> Result<PathBuf, String> {
    let mut reporter = None;
    ensure_remote_files(
        models_root,
        &[PADDLE_DET_CONFIG, PADDLE_DET_ONNX],
        &mut reporter,
    )?;
    Ok(models_root.join(PADDLE_DET_ONNX))
}

pub fn ensure_paddle_ocr_detector_with_reporter(
    models_root: &Path,
    mut reporter: ModelDownloadReporter<'_>,
) -> Result<PathBuf, String> {
    ensure_remote_files(
        models_root,
        &[PADDLE_DET_CONFIG, PADDLE_DET_ONNX],
        &mut reporter,
    )?;
    Ok(models_root.join(PADDLE_DET_ONNX))
}

/// Local paths of the PaddleOCR model files for one language: the shared detector
/// model, the per-language recognizer, and its character dictionary.
///
/// Returned by [`ensure_paddle_ocr_full_paths_with_reporter`] so native callers can
/// build a `PaddleOcrEngine` without re-deriving the on-disk layout.
#[derive(Debug, Clone)]
pub struct PaddleOcrModelPaths {
    /// Shared DB text-detection model (`det.onnx`).
    pub det: PathBuf,
    /// Per-language recognizer model (`rec.onnx`).
    pub rec: PathBuf,
    /// Per-language character dictionary (`dict.txt`).
    pub dict: PathBuf,
}

pub fn ensure_paddle_ocr_full_with_reporter(
    models_root: &Path,
    model_key: &str,
    reporter: ModelDownloadReporter<'_>,
) -> Result<(), String> {
    ensure_paddle_ocr_full_paths_with_reporter(models_root, model_key, reporter).map(|_| ())
}

/// Ensures every PaddleOCR file for `model_key` (shared detector + per-language
/// recognizer + dictionary) is present locally, downloading missing ones, and
/// returns their resolved on-disk paths.
///
/// `model_key` is a PaddleOCR language key (see [`PADDLE_LANGUAGES`] / the alias
/// map in `paddle_language_spec`); unknown keys resolve to the default language
/// rather than failing. Must be called from a worker/background thread.
pub fn ensure_paddle_ocr_full_paths_with_reporter(
    models_root: &Path,
    model_key: &str,
    mut reporter: ModelDownloadReporter<'_>,
) -> Result<PaddleOcrModelPaths, String> {
    ensure_remote_files(
        models_root,
        &[PADDLE_DET_CONFIG, PADDLE_DET_ONNX],
        &mut reporter,
    )?;
    let spec = paddle_language_spec(model_key);
    let lang_dir = format!("ONNX/PaddleOCR/languages/{}", spec.dir_name);
    let rec_model = format!("{lang_dir}/rec.onnx");
    let rec_dict = format!("{lang_dir}/dict.txt");
    let rec_config = format!("{lang_dir}/config.json");
    ensure_remote_files(
        models_root,
        &[&rec_config, &rec_dict, &rec_model],
        &mut reporter,
    )?;
    Ok(PaddleOcrModelPaths {
        det: models_root.join(PADDLE_DET_ONNX),
        rec: models_root.join(&rec_model),
        dict: models_root.join(&rec_dict),
    })
}

pub fn ensure_manga_ocr_onnx_with_reporter(
    models_root: &Path,
    model: MangaOcrOnnxModel,
    mut reporter: ModelDownloadReporter<'_>,
) -> Result<PathBuf, String> {
    let (dir_name, files) = match model {
        MangaOcrOnnxModel::Base => ("base", MANGA_OCR_BASE_FILES),
        MangaOcrOnnxModel::Model2025 => ("2025", MANGA_OCR_2025_FILES),
    };
    let remote_files = files
        .iter()
        .map(|file| format!("ONNX/MangaOCR/{dir_name}/{file}"))
        .collect::<Vec<_>>();
    let remote_refs = remote_files.iter().map(String::as_str).collect::<Vec<_>>();
    ensure_remote_files(models_root, &remote_refs, &mut reporter)?;
    Ok(models_root.join("ONNX").join("MangaOCR").join(dir_name))
}

pub fn manga_ocr_model_from_key(model_key: &str) -> Option<MangaOcrOnnxModel> {
    match model_key.trim().to_ascii_lowercase().as_str() {
        "base" | "base_onnx" | "basic" | "default" | "mangaocr_base" | "manga_ocr_base" => {
            Some(MangaOcrOnnxModel::Base)
        }
        "2025" | "2025_onnx" | "mangaocr_2025" | "manga_ocr_2025" => {
            Some(MangaOcrOnnxModel::Model2025)
        }
        _ => None,
    }
}

pub fn ensure_comic_text_detector_torch(models_root: &Path) -> Result<PathBuf, String> {
    ensure_comic_text_detector_torch_with_reporter(models_root, None)
}

pub fn ensure_comic_text_detector_torch_with_reporter(
    models_root: &Path,
    mut reporter: ModelDownloadReporter<'_>,
) -> Result<PathBuf, String> {
    let remote = "Torch/ComicTextDetector/comictextdetector.pt";
    ensure_remote_files(models_root, &[remote], &mut reporter)?;
    Ok(models_root.join(remote))
}

pub fn ensure_aot(models_root: &Path) -> Result<PathBuf, String> {
    let remote = "Torch/AOT/inpainting.ckpt";
    let mut reporter = None;
    ensure_remote_files(models_root, &[remote], &mut reporter)?;
    Ok(models_root.join(remote))
}

pub fn ensure_lama_mpe(models_root: &Path) -> Result<PathBuf, String> {
    let remote = "Torch/LaMa_MPE/inpainting_lama_mpe.ckpt";
    let mut reporter = None;
    ensure_remote_files(models_root, &[remote], &mut reporter)?;
    Ok(models_root.join(remote))
}

pub fn ensure_lama_model(models_root: &Path, model_file_name: &str) -> Result<PathBuf, String> {
    let model_file_name = canonical_lama_model_file(model_file_name);
    let config = "Torch/LaMa/config.yaml";
    let model = format!("Torch/LaMa/models/{model_file_name}");
    let mut reporter = None;
    ensure_remote_files(models_root, &[config, &model], &mut reporter)?;
    Ok(models_root.join(model))
}

/// Web stub: the Hugging Face download stack is compiled out on wasm, so model
/// files cannot be fetched or verified. Returns a clear typed error instead of a
/// fake success so callers surface "unavailable on web".
#[cfg(target_arch = "wasm32")]
fn ensure_remote_files(
    _models_root: &Path,
    _remote_files: &[&str],
    _reporter: &mut ModelDownloadReporter<'_>,
) -> Result<(), String> {
    Err(t!("ai_models.web_unavailable").to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure_remote_files(
    models_root: &Path,
    remote_files: &[&str],
    reporter: &mut ModelDownloadReporter<'_>,
) -> Result<(), String> {
    let missing = remote_files
        .iter()
        .copied()
        .filter(|remote| !is_nonempty_file(&models_root.join(remote)))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(models_root).map_err(|err| {
        tf!("ai_models.create_root_error", models_root = models_root.display(), err = err)
    })?;
    let lock = DOWNLOAD_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .map_err(|_| t!("ai_models.download_lock_poisoned").to_string())?;

    let still_missing = remote_files
        .iter()
        .copied()
        .filter(|remote| !is_nonempty_file(&models_root.join(remote)))
        .collect::<Vec<_>>();
    if still_missing.is_empty() {
        return Ok(());
    }
    if let Some(report) = reporter.as_mut() {
        report();
    }

    let repo_id = format!("{HF_OWNER}/{HF_REPO_NAME}");
    // The main repository is public: no token, as before (only pinned external models use
    // the user's Hugging Face token).
    let fetcher = UreqFetcher::new(None);
    let retry = RetryPolicy::standard();
    for remote in still_missing {
        let url = hf_resolve_url(&repo_id, HF_REVISION, remote);
        let local = models_root.join(remote);
        download_hf_file_with(&fetcher, &retry, &url, &local, remote)?;
        if !is_nonempty_file(&local) {
            return Err(tf!("ai_models.file_missing_after_download", local = local.display()));
        }
    }
    Ok(())
}

/// Downloads `url` into `local_path` through `<file name>.part` next to it: resumes a part
/// left by an earlier run with `Range`, retries transient failures per `retry`, verifies the
/// staged length against the size the server announced (`Content-Length` /
/// `Content-Range`), fsyncs and renames it into place. `remote` is the repository path, for
/// messages and logs only. A failed download keeps the `.part` for the next call unless its
/// bytes were proven wrong.
///
/// # Errors
/// A localized message (`ai_models.*` keys) for a folder, download, write or rename failure.
#[cfg(not(target_arch = "wasm32"))]
fn download_hf_file_with(
    fetcher: &dyn RangeFetcher,
    retry: &RetryPolicy<'_>,
    url: &str,
    local_path: &Path,
    remote: &str,
) -> Result<(), String> {
    let parent = local_path
        .parent()
        .ok_or_else(|| tf!("ai_models.invalid_model_path", local_path = local_path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| tf!("ai_models.create_folder_error", parent = parent.display(), err = err))?;
    let part = part_path_for(local_path)
        .ok_or_else(|| tf!("ai_models.invalid_model_path", local_path = local_path.display()))?;
    let offset = part.metadata().map(|metadata| if metadata.is_file() { metadata.len() } else { 0 }).unwrap_or(0);
    ms_log::runtime_log::log_info(format!("{LOG_TAG} download start: file={remote} resume_from={offset} url={url}"));

    // The `ensure_*` API has no cancel parameter (see the file header): this flag is never set.
    let never_cancelled = AtomicBool::new(false);
    let log_context = format!("file={remote}");
    let engine = ResumableFetch { fetcher, retry, cancel: &never_cancelled, log_tag: LOG_TAG, log_context: &log_context };
    let request = PartRequest { url, part: &part, offset, expected_size: None };
    let size = engine.fetch_part(&request, None, &mut |_| {}).map_err(|error| {
        ms_log::runtime_log::log_error(format!("{LOG_TAG} download failed: file={remote} part={} error={error:?}", part.display()));
        match error {
            StreamError::Io { path, source } => tf!("ai_models.write_error", tmp_path = path.display(), err = source),
            StreamError::Http { detail } => tf!("ai_models.download_error", remote = remote, err = detail),
            StreamError::SizeMismatch { expected, actual } => tf!(
                "ai_models.download_error",
                remote = remote,
                err = format!("size mismatch: expected {expected} bytes, got {actual}")
            ),
            // Unreachable while the flag above is never set; mapped rather than assumed.
            StreamError::Cancelled => tf!("ai_models.download_error", remote = remote, err = "cancelled"),
        }
    })?;
    fs::rename(&part, local_path).map_err(|err| {
        ms_log::runtime_log::log_error(format!("{LOG_TAG} could not publish the download: file={remote} error={err}"));
        tf!("ai_models.save_error", local_path = local_path.display(), err = err)
    })?;
    ms_log::runtime_log::log_info(format!("{LOG_TAG} download complete: file={remote} bytes={size}"));
    Ok(())
}

/// `<file name>.part` next to `local_path`; `None` when the path has no file name.
#[cfg(not(target_arch = "wasm32"))]
fn part_path_for(local_path: &Path) -> Option<PathBuf> {
    let mut name = local_path.file_name()?.to_os_string();
    name.push(".part");
    Some(local_path.with_file_name(name))
}

#[cfg(not(target_arch = "wasm32"))]
fn is_nonempty_file(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.len() > 0)
        .unwrap_or(false)
}

fn paddle_language_spec(model_key: &str) -> PaddleLanguageSpec {
    let canonical = match model_key.trim().to_ascii_lowercase().as_str() {
        "japan_v5" | "chinese_cht_v5" => "chinese_v5",
        "cyrillic_v3" => "eslav_v5",
        "devanagari_v3" => "hindi_v3",
        "english_v5" => "english_v5",
        "latin_v5" => "latin_v5",
        "eslav_v5" => "eslav_v5",
        "korean_v5" => "korean_v5",
        "chinese_v5" => "chinese_v5",
        "thai_v5" => "thai_v5",
        "greek_v5" => "greek_v5",
        "arabic_v3" => "arabic_v3",
        "hindi_v3" => "hindi_v3",
        "telugu_v3" => "telugu_v3",
        "tamil_v3" => "tamil_v3",
        _ => "korean_v5",
    };
    PADDLE_LANGUAGES
        .iter()
        .copied()
        .find(|spec| spec.key == canonical)
        .unwrap_or(PADDLE_LANGUAGES[0])
}

fn canonical_lama_model_file(model_file_name: &str) -> &'static str {
    match model_file_name.trim() {
        "best.ckpt" => "best.ckpt",
        "lama_large_512px.ckpt" => "lama_large_512px.ckpt",
        "1anime-manga-big-lama.pt" => "1anime-manga-big-lama.pt",
        "anime-manga-big-lama.pt" => "anime-manga-big-lama.pt",
        _ => "anime-manga-big-lama.pt",
    }
}

#[cfg(test)]
mod tests {
    use super::{MangaOcrOnnxModel, manga_ocr_model_from_key, paddle_language_spec};

    #[test]
    fn paddle_aliases_map_to_local_dirs() {
        assert_eq!(paddle_language_spec("cyrillic_v3").key, "eslav_v5");
        assert_eq!(paddle_language_spec("devanagari_v3").dir_name, "hindi");
        assert_eq!(paddle_language_spec("unknown").dir_name, "korean");
    }

    #[test]
    fn manga_ocr_keys_only_match_onnx_models() {
        assert_eq!(
            manga_ocr_model_from_key("base_onnx"),
            Some(MangaOcrOnnxModel::Base)
        );
        assert_eq!(
            manga_ocr_model_from_key("2025_onnx"),
            Some(MangaOcrOnnxModel::Model2025)
        );
        assert_eq!(manga_ocr_model_from_key("base_torch"), None);
    }
}
