/*
FILE OVERVIEW: crates/ms-sysprobe/src/ai_models.rs
Application-managed AI model resolver and lazy Hugging Face downloader.

Main responsibilities:
- Define the canonical ManhwaStudio_AI_Models file layout for Rust callers.
- Download missing files of the `main` revision directly into ManhwaStudio_AI_Models (no HF
  cache blobs or symlinks) through `ms_download::fetch_verified` (`Expected::Announced`):
  `<file>.part` staging next to the target, HTTP `Range` resume, retry with capped backoff,
  connect/read timeouts, cancel, size verified against the server's announcement,
  fsync + rename on success.
- `EnsureCtx` (cancel flag + progress sink), `ModelDownloadEvent` / `ModelDownloadPhase`
  (per-file progress, waits and retries) and `ModelEnsureError` (`Cancelled` / localized
  `Failed`): the contract every `ensure_*` shares.
- Return concrete local paths for Rust model initialization code.

- Mount the `external` (pinned third-party model downloader), `external_catalog`,
  `easyocr` and `hf_url` (Hugging Face resolve URLs) submodules; see
  `ai_models/MODULE_README.md`.

Notes:
- Third-party models (EasyOCR, Surya, SDXL configs, Baberu, PaddleOCR-VL) are the pinned
  `external` catalog, not this main-repository resolver.
- Callers must invoke this from worker/background code, not the GUI thread.
- The main repository is not pinned (`main`): a file is published once exactly the
  announced number of bytes is staged, its `.part` resumes only while the server names the
  same object (`ms_download` identity binding: the LFS sha256 / git-blob ETag of the first
  response), and an LFS sha256 the server announces is checked before publishing. The one
  pinned file is LaMa-MPE (`LAMA_MPE_SIZE` / `LAMA_MPE_SHA256`, `ensure_lama_mpe`), trusted
  without hashing only through its `<file>.verified.json` marker (unix: dev + inode + mtime +
  ctime, ctime older than the marker; elsewhere hashed on every call). Requests carry no Hugging
  Face token (the repository is public).
- One download per file per process: each missing file is fetched under the busy id
  `ai-models:<absolute local path>`; a second caller waits (cancellable), then finds the
  file present. Presence is "non-empty regular file".
*/

// The HTTP download stack is native-only (`ureq` is compiled out on wasm); the resolver
// keeps its public signatures on both targets and stubs the actual download on the web
// build.
#[cfg(not(target_arch = "wasm32"))]
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[cfg(not(target_arch = "wasm32"))]
use hf_url::hf_resolve_url;
#[cfg(not(target_arch = "wasm32"))]
use ms_download::busy::BusyGuard;
#[cfg(not(target_arch = "wasm32"))]
use ms_download::resumable::{RangeFetcher, ResumableFetch, RetryPolicy, StreamEvent, UreqFetcher};
#[cfg(not(target_arch = "wasm32"))]
use ms_download::{DownloadError, Expected, FileEvent, FileRequest, fetch_verified};

/// Pinned external (third-party) model downloader and status probe; see `ai_models/MODULE_README.md`.
pub mod external;
/// The pinned external-model catalog (Baberu OCR, PaddleOCR-VL variants, Surya, SDXL).
pub mod external_catalog;
/// EasyOCR: language normalization, recognizer choice (easyocr 1.7.2 mirror) and the pinned
/// per-script specs in one shared directory.
pub mod easyocr;
/// Hugging Face resolve URLs of both downloaders (`HF_ENDPOINT` honoured, no home directory).
#[cfg(not(target_arch = "wasm32"))]
mod hf_url;
/// Tests of the main-repository downloader `download_hf_file_with`.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod main_repo_tests;

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

/// Cancel switch and progress sink of one `ensure_*` call.
///
/// `cancel` is polled before every file, during the wait for another download of the same
/// file, before every request, after every read and every 100 ms of a retry backoff; setting
/// it ends the call with [`ModelEnsureError::Cancelled`] and keeps the staged `.part` for a
/// later resume. `progress` is called on the calling (worker) thread.
pub struct EnsureCtx<'a> {
    /// Set by the caller's Stop / supersede / drop path.
    pub cancel: &'a AtomicBool,
    /// Receives the download events of the call (nothing when every file is present),
    /// thinned per [`progress_due`]: every phase change, then every
    /// [`PROGRESS_STEP_BYTES`] of a transfer and its last byte.
    pub progress: &'a mut dyn FnMut(&ModelDownloadEvent),
}

/// One progress report of a model-file download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDownloadEvent {
    /// Repository path of the file (e.g. `ONNX/PaddleOCR/detection/v5/det.onnx`).
    pub file: String,
    /// 0-based index of `file` among the files this call downloads.
    pub file_index: usize,
    /// Number of files this call downloads (present files are not counted).
    pub file_count: usize,
    /// What the download is doing.
    pub phase: ModelDownloadPhase,
    /// Bytes of `file` staged so far.
    pub staged: u64,
    /// Size of `file` as announced by the server, `None` until known.
    pub total: Option<u64>,
}

/// Phase of a [`ModelDownloadEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelDownloadPhase {
    /// Another download in this process is fetching the same file; waiting for it.
    WaitingForOtherDownload,
    /// Bytes are arriving.
    Downloading,
    /// A transient failure: waiting `delay_secs` before retry `attempt` of `max_attempts`
    /// (progress, not a failure).
    Retrying { attempt: u32, max_attempts: u32, delay_secs: u64 },
}

/// Transfer progress granularity of [`EnsureCtx::progress`]: one `Downloading` event per this
/// many staged bytes (a read is at most 128 KiB, so an unthinned stream floods a UI channel).
pub const PROGRESS_STEP_BYTES: u64 = 1024 * 1024;

/// Whether an event of `phase` at `staged` (of `total`) is reported, given the last reported
/// `(phase, staged)` of the same file: always on a phase change (including a new retry delay),
/// a restart (fewer bytes) or the last byte; otherwise once per [`PROGRESS_STEP_BYTES`].
#[must_use]
pub fn progress_due(last: Option<(ModelDownloadPhase, u64)>, phase: ModelDownloadPhase, staged: u64, total: Option<u64>) -> bool {
    match last {
        None => true,
        Some((last_phase, last_staged)) => {
            last_phase != phase
                || staged < last_staged
                || total == Some(staged)
                || staged.saturating_sub(last_staged) >= PROGRESS_STEP_BYTES
        }
    }
}

/// Failure of an `ensure_*` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEnsureError {
    /// The caller's cancel flag was set; nothing was published, staged bytes are kept.
    Cancelled,
    /// The files could not be made present; the payload is the localized user-facing text.
    Failed(String),
}

/// Ensures the shared PaddleOCR detector (`det.onnx` + its config) and returns `det.onnx`.
///
/// # Errors
/// [`ModelEnsureError`]; worker thread only (may download).
pub fn ensure_paddle_ocr_detector(models_root: &Path, ctx: &mut EnsureCtx<'_>) -> Result<PathBuf, ModelEnsureError> {
    ensure_remote_files(models_root, &[PADDLE_DET_CONFIG, PADDLE_DET_ONNX], ctx)?;
    Ok(models_root.join(PADDLE_DET_ONNX))
}

/// Local paths of the PaddleOCR model files for one language: the shared detector
/// model, the per-language recognizer, and its character dictionary.
///
/// Returned by [`ensure_paddle_ocr_full`] so native callers can build a
/// `PaddleOcrEngine` without re-deriving the on-disk layout.
#[derive(Debug, Clone)]
pub struct PaddleOcrModelPaths {
    /// Shared DB text-detection model (`det.onnx`).
    pub det: PathBuf,
    /// Per-language recognizer model (`rec.onnx`).
    pub rec: PathBuf,
    /// Per-language character dictionary (`dict.txt`).
    pub dict: PathBuf,
}

/// Ensures every PaddleOCR file for `model_key` (shared detector + per-language
/// recognizer + dictionary) is present locally, downloading missing ones, and
/// returns their resolved on-disk paths.
///
/// `model_key` is a PaddleOCR language key (see [`PADDLE_LANGUAGES`] / the alias
/// map in `paddle_language_spec`); unknown keys resolve to the default language
/// rather than failing.
///
/// # Errors
/// [`ModelEnsureError`]; worker thread only (may download).
pub fn ensure_paddle_ocr_full(models_root: &Path, model_key: &str, ctx: &mut EnsureCtx<'_>) -> Result<PaddleOcrModelPaths, ModelEnsureError> {
    let spec = paddle_language_spec(model_key);
    let lang_dir = format!("ONNX/PaddleOCR/languages/{}", spec.dir_name);
    let rec_model = format!("{lang_dir}/rec.onnx");
    let rec_dict = format!("{lang_dir}/dict.txt");
    let rec_config = format!("{lang_dir}/config.json");
    ensure_remote_files(models_root, &[PADDLE_DET_CONFIG, PADDLE_DET_ONNX, &rec_config, &rec_dict, &rec_model], ctx)?;
    Ok(PaddleOcrModelPaths {
        det: models_root.join(PADDLE_DET_ONNX),
        rec: models_root.join(&rec_model),
        dict: models_root.join(&rec_dict),
    })
}

/// Ensures the MangaOCR ONNX export `model` and returns its directory.
///
/// # Errors
/// [`ModelEnsureError`]; worker thread only (may download).
pub fn ensure_manga_ocr_onnx(models_root: &Path, model: MangaOcrOnnxModel, ctx: &mut EnsureCtx<'_>) -> Result<PathBuf, ModelEnsureError> {
    let (dir_name, files) = match model {
        MangaOcrOnnxModel::Base => ("base", MANGA_OCR_BASE_FILES),
        MangaOcrOnnxModel::Model2025 => ("2025", MANGA_OCR_2025_FILES),
    };
    let remote_files = files
        .iter()
        .map(|file| format!("ONNX/MangaOCR/{dir_name}/{file}"))
        .collect::<Vec<_>>();
    let remote_refs = remote_files.iter().map(String::as_str).collect::<Vec<_>>();
    ensure_remote_files(models_root, &remote_refs, ctx)?;
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

/// Ensures the ComicTextDetector Torch checkpoint and returns its path.
///
/// # Errors
/// [`ModelEnsureError`]; worker thread only (may download).
pub fn ensure_comic_text_detector_torch(models_root: &Path, ctx: &mut EnsureCtx<'_>) -> Result<PathBuf, ModelEnsureError> {
    let remote = "Torch/ComicTextDetector/comictextdetector.pt";
    ensure_remote_files(models_root, &[remote], ctx)?;
    Ok(models_root.join(remote))
}

/// Ensures the AOT inpainting checkpoint and returns its path.
///
/// # Errors
/// [`ModelEnsureError`]; worker thread only (may download).
pub fn ensure_aot(models_root: &Path, ctx: &mut EnsureCtx<'_>) -> Result<PathBuf, ModelEnsureError> {
    let remote = "Torch/AOT/inpainting.ckpt";
    ensure_remote_files(models_root, &[remote], ctx)?;
    Ok(models_root.join(remote))
}

/// Repository path of the LaMa-MPE checkpoint.
const LAMA_MPE_REMOTE: &str = "Torch/LaMa_MPE/inpainting_lama_mpe.ckpt";
/// Exact size of the LaMa-MPE checkpoint in bytes. The main-repository copy is byte-identical
/// to the upstream manga-image-translator `beta-0.3` release asset (verified when pinned).
pub const LAMA_MPE_SIZE: u64 = 108_580_583;
/// Lowercase sha256 of the LaMa-MPE checkpoint: the one Rust owner of this value (the backend
/// keeps its own load-time check in `modules/ai_backend/inpaint/lama_mpe.py`).
pub const LAMA_MPE_SHA256: &str = "d625aa1b3e0d0408acfd6928aa84f005867aa8dbb9162480346a4e20660786cc";

/// Ensures the LaMa-MPE checkpoint is present AND is the pinned file ([`LAMA_MPE_SIZE`] +
/// [`LAMA_MPE_SHA256`]), re-downloading a wrong or truncated one, and returns its path.
///
/// Fast path (stat only, no hashing, unix only): the file has the pinned size and its
/// verified-marker sidecar (`<file>.verified.json`, see [`VerifiedMarker`] for the trust
/// boundary) records the pin together with the file's current device, inode, mtime and
/// ctime. Otherwise the file is hashed (worker thread) and kept if it matches, or downloaded
/// again; the marker is then rewritten.
///
/// # Errors
/// [`ModelEnsureError`]; worker thread only (may hash 100 MB and download).
pub fn ensure_lama_mpe(models_root: &Path, ctx: &mut EnsureCtx<'_>) -> Result<PathBuf, ModelEnsureError> {
    ensure_pinned_remote_file(models_root, LAMA_MPE_REMOTE, LAMA_MPE_SIZE, LAMA_MPE_SHA256, ctx)?;
    Ok(models_root.join(LAMA_MPE_REMOTE))
}

/// Ensures the LaMa-v2 config plus the checkpoint `model_file_name` (unknown names map to the
/// default checkpoint) and returns the checkpoint path.
///
/// # Errors
/// [`ModelEnsureError`]; worker thread only (may download).
pub fn ensure_lama_model(models_root: &Path, model_file_name: &str, ctx: &mut EnsureCtx<'_>) -> Result<PathBuf, ModelEnsureError> {
    let model_file_name = canonical_lama_model_file(model_file_name);
    let config = "Torch/LaMa/config.yaml";
    let model = format!("Torch/LaMa/models/{model_file_name}");
    ensure_remote_files(models_root, &[config, &model], ctx)?;
    Ok(models_root.join(model))
}

/// Web stub: the Hugging Face download stack is compiled out on wasm, so model
/// files cannot be fetched or verified. Returns a clear typed error instead of a
/// fake success so callers surface "unavailable on web".
#[cfg(target_arch = "wasm32")]
fn ensure_remote_files(_models_root: &Path, _remote_files: &[&str], _ctx: &mut EnsureCtx<'_>) -> Result<(), ModelEnsureError> {
    Err(ModelEnsureError::Failed(t!("ai_models.web_unavailable").to_string()))
}

/// Web stub of [`ensure_pinned_remote_file`]: nothing can be fetched or verified on wasm.
#[cfg(target_arch = "wasm32")]
fn ensure_pinned_remote_file(_models_root: &Path, _remote: &str, _size: u64, _sha256: &str, _ctx: &mut EnsureCtx<'_>) -> Result<(), ModelEnsureError> {
    Err(ModelEnsureError::Failed(t!("ai_models.web_unavailable").to_string()))
}

/// Makes `remote` present under `models_root` as exactly the pinned file (`size` +
/// lowercase `sha256`): a verified marker that still describes the file is enough, otherwise
/// the file is hashed and kept, or downloaded again from the main repository.
#[cfg(not(target_arch = "wasm32"))]
fn ensure_pinned_remote_file(models_root: &Path, remote: &str, size: u64, sha256: &str, ctx: &mut EnsureCtx<'_>) -> Result<(), ModelEnsureError> {
    if VerifiedMarker::confirms(&models_root.join(remote), size, sha256) {
        return Ok(());
    }
    fs::create_dir_all(models_root).map_err(|err| {
        ModelEnsureError::Failed(tf!("ai_models.create_root_error", models_root = models_root.display(), err = err))
    })?;
    let fetcher = UreqFetcher::new(None);
    let retry = RetryPolicy::standard();
    ensure_pinned_file_with(&fetcher, &retry, models_root, remote, size, sha256, ctx)
}

/// Core of [`ensure_pinned_remote_file`] over an injected fetcher and retry schedule (tests).
///
/// Under the same busy id as [`ensure_files_with`] ([`busy_key`]): re-check the marker, then
/// `fetch_verified` with `Expected::SizeSha256` — which keeps an existing file that hashes to
/// the pin without any request, and otherwise resumes or restarts `<file>.part` and publishes
/// only verified bytes — and finally record the marker. A marker that cannot be written is a
/// logged warning (the next call hashes again), not a failure.
#[cfg(not(target_arch = "wasm32"))]
fn ensure_pinned_file_with(
    fetcher: &dyn RangeFetcher,
    retry: &RetryPolicy<'_>,
    models_root: &Path,
    remote: &str,
    size: u64,
    sha256: &str,
    ctx: &mut EnsureCtx<'_>,
) -> Result<(), ModelEnsureError> {
    let local = models_root.join(remote);
    let mut last_reported: Option<(ModelDownloadPhase, u64)> = None;
    let mut report = |phase: ModelDownloadPhase, staged: u64, total: Option<u64>| {
        if progress_due(last_reported, phase, staged, total) {
            last_reported = Some((phase, staged));
            (ctx.progress)(&ModelDownloadEvent { file: remote.to_string(), file_index: 0, file_count: 1, phase, staged, total });
        }
    };
    let key = busy_key(&local);
    let _guard = BusyGuard::acquire_waiting(&key, ctx.cancel, retry, &mut || report(ModelDownloadPhase::WaitingForOtherDownload, 0, None))
        .map_err(|error| map_download_error(remote, error))?;
    if VerifiedMarker::confirms(&local, size, sha256) {
        return Ok(());
    }
    let url = hf_resolve_url(&format!("{HF_OWNER}/{HF_REPO_NAME}"), HF_REVISION, remote);
    download_hf_file_with(fetcher, retry, ctx.cancel, &HfFile { url: &url, local_path: &local, remote, expected: Expected::SizeSha256 { size, sha256 } }, &mut report)?;
    if let Err(error) = VerifiedMarker::record(&local, size, sha256) {
        ms_log::runtime_log::log_warn(format!("{LOG_TAG} verified marker not written (the file is hashed again next time): file={remote} error={error}"));
    }
    Ok(())
}

/// Sidecar `<file>.verified.json` saying "this exact file version was verified against
/// this pin". It is a CACHE HINT that lets a presence check skip hashing, never a proof that
/// outlives a change of the file.
///
/// Trust boundary: the marker records the pin, the size and, on unix, the file's device,
/// inode, mtime and ctime. ctime cannot be set by user tools (`touch`, `cp -p`, archive
/// extraction and backup restores all leave a NEW ctime), and every content write, rename
/// onto the path or metadata restore changes it, so a recorded ctime pins one version of one
/// inode. A change in the same clock tick as the verification ("racy" stamp) is excluded by
/// trusting the marker only when the file's ctime is strictly older than the marker's own
/// mtime. Not covered: a deliberate attacker with root rights resetting the system clock.
/// On platforms without a stable cheap identity (non-unix: std exposes no file index or
/// change time on stable) nothing is ever trusted unhashed: the file is hashed on every call.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct VerifiedMarker {
    /// Pinned (and verified) size in bytes.
    size: u64,
    /// Pinned (and verified) lowercase sha256.
    sha256: String,
    /// Device id of the verified file.
    dev: u64,
    /// Inode number of the verified file.
    ino: u64,
    /// Modification time of the verified file (seconds, nanoseconds since the epoch).
    mtime: (i64, i64),
    /// Status-change time of the verified file (seconds, nanoseconds since the epoch).
    ctime: (i64, i64),
}

#[cfg(not(target_arch = "wasm32"))]
impl VerifiedMarker {
    /// Path of the marker of `file`.
    fn path_for(file: &Path) -> PathBuf {
        let mut name = file.as_os_str().to_os_string();
        name.push(".verified.json");
        PathBuf::from(name)
    }

    /// The marker the file at `file` would need now; `None` when it is not a regular file of
    /// the pinned size, or the platform gives no cheap identity (then nothing is trusted
    /// unhashed).
    #[cfg(unix)]
    fn describe(file: &Path, size: u64, sha256: &str) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = fs::metadata(file).ok().filter(fs::Metadata::is_file)?;
        if metadata.len() != size {
            return None;
        }
        Some(Self {
            size,
            sha256: sha256.to_owned(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    /// Non-unix: no stable cheap file identity, so never trusted unhashed.
    #[cfg(not(unix))]
    fn describe(_file: &Path, _size: u64, _sha256: &str) -> Option<Self> {
        None
    }

    /// Modification time of the marker file itself, `(seconds, nanoseconds)`.
    #[cfg(unix)]
    fn marker_written_at(marker: &Path) -> Option<(i64, i64)> {
        use std::os::unix::fs::MetadataExt;

        fs::metadata(marker).ok().map(|metadata| (metadata.mtime(), metadata.mtime_nsec()))
    }

    #[cfg(not(unix))]
    fn marker_written_at(_marker: &Path) -> Option<(i64, i64)> {
        None
    }

    /// Whether `file` has the pinned size and a marker that matches the pin and the file's
    /// current identity, with the file's ctime strictly older than the marker (see the type
    /// comment). Stat + one small read; never hashes.
    fn confirms(file: &Path, size: u64, sha256: &str) -> bool {
        let Some(expected) = Self::describe(file, size, sha256) else { return false };
        let marker = Self::path_for(file);
        let Some(written_at) = Self::marker_written_at(&marker) else { return false };
        let recorded = fs::read(&marker).ok().and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok());
        expected.ctime < written_at && recorded == Some(expected)
    }

    /// Writes the marker for the just-verified `file` through the owner recipe
    /// (`ms_atomic_file`: hidden sibling temp, contents fsynced, rename; the temp is removed on
    /// failure). On a platform without a cheap identity it writes nothing (`Ok`): the file is
    /// hashed on every call. The identical-bytes skip is OFF: [`Self::confirms`] trusts a
    /// marker only when its OWN mtime is newer than the file's ctime, so an unchanged marker
    /// left from a racy stamp must still be rewritten to get a fresh mtime. Worker thread only.
    fn record(file: &Path, size: u64, sha256: &str) -> Result<(), String> {
        let Some(marker) = Self::describe(file, size, sha256) else { return Ok(()) };
        let bytes = serde_json::to_vec(&marker).map_err(|err| err.to_string())?;
        let options = ms_atomic_file::AtomicWriteOptions { skip_if_identical: false, ..ms_atomic_file::AtomicWriteOptions::new(ms_atomic_file::Durability::Contents) };
        ms_atomic_file::write_atomic_with(&Self::path_for(file), &bytes, &options).map_err(|err| err.to_string())
    }
}

/// Makes every `remote_files` entry present under `models_root` (a present file is a
/// non-empty regular file), downloading the missing ones from the main repository.
#[cfg(not(target_arch = "wasm32"))]
fn ensure_remote_files(models_root: &Path, remote_files: &[&str], ctx: &mut EnsureCtx<'_>) -> Result<(), ModelEnsureError> {
    if remote_files.iter().all(|remote| is_nonempty_file(&models_root.join(remote))) {
        return Ok(());
    }
    fs::create_dir_all(models_root).map_err(|err| {
        ModelEnsureError::Failed(tf!("ai_models.create_root_error", models_root = models_root.display(), err = err))
    })?;
    // The main repository is public: no token (only pinned external models use the user's
    // Hugging Face token).
    let fetcher = UreqFetcher::new(None);
    let retry = RetryPolicy::standard();
    ensure_files_with(&fetcher, &retry, models_root, remote_files, ctx)
}

/// Core of [`ensure_remote_files`] over an injected fetcher and retry schedule (tests).
///
/// Per missing file: take the process-wide busy id [`busy_key`] (waiting, cancellable, sending
/// `WaitingForOtherDownload`), re-check presence (the holder may just have published it), then
/// `fetch_verified` with the server-announced size into `<file>.part` next to the target and
/// rename it into place.
#[cfg(not(target_arch = "wasm32"))]
fn ensure_files_with(
    fetcher: &dyn RangeFetcher,
    retry: &RetryPolicy<'_>,
    models_root: &Path,
    remote_files: &[&str],
    ctx: &mut EnsureCtx<'_>,
) -> Result<(), ModelEnsureError> {
    let missing = remote_files
        .iter()
        .copied()
        .filter(|remote| !is_nonempty_file(&models_root.join(remote)))
        .collect::<Vec<_>>();
    let repo_id = format!("{HF_OWNER}/{HF_REPO_NAME}");
    let file_count = missing.len();
    for (file_index, remote) in missing.into_iter().enumerate() {
        let local = models_root.join(remote);
        let mut last_reported: Option<(ModelDownloadPhase, u64)> = None;
        let mut report = |phase: ModelDownloadPhase, staged: u64, total: Option<u64>| {
            if progress_due(last_reported, phase, staged, total) {
                last_reported = Some((phase, staged));
                (ctx.progress)(&ModelDownloadEvent { file: remote.to_string(), file_index, file_count, phase, staged, total });
            }
        };
        let key = busy_key(&local);
        let _guard = BusyGuard::acquire_waiting(&key, ctx.cancel, retry, &mut || report(ModelDownloadPhase::WaitingForOtherDownload, 0, None))
            .map_err(|error| map_download_error(remote, error))?;
        if is_nonempty_file(&local) {
            continue;
        }
        let url = hf_resolve_url(&repo_id, HF_REVISION, remote);
        download_hf_file_with(fetcher, retry, ctx.cancel, &HfFile { url: &url, local_path: &local, remote, expected: Expected::Announced }, &mut report)?;
        if !is_nonempty_file(&local) {
            return Err(ModelEnsureError::Failed(tf!("ai_models.file_missing_after_download", local = local.display())));
        }
    }
    Ok(())
}

/// The busy id of one main-repository file: `ai-models:` + its absolute local path (symlinks
/// not resolved), so two callers of the same file serialize and different model roots do not.
#[cfg(not(target_arch = "wasm32"))]
fn busy_key(local: &Path) -> String {
    // `absolute` fails only for an empty path or an unreadable current directory; the path as
    // given is then still a stable key for every caller that spells it the same way.
    let absolute = std::path::absolute(local).unwrap_or_else(|_| local.to_path_buf());
    format!("ai-models:{}", absolute.display())
}

/// One main-repository file to download: resolve `url`, published path `local_path`, repository
/// path `remote` (messages and logs) and what the content is checked against.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy)]
struct HfFile<'a> {
    url: &'a str,
    local_path: &'a Path,
    remote: &'a str,
    expected: Expected<'a>,
}

/// Downloads `url` into `local_path` through `<file name>.part` next to it with
/// `ms_download::fetch_verified` against `expected` (`Announced` for the unpinned files, a
/// pin for LaMa-MPE; resume, retry, cancel and the resume binding to the server's object
/// identity per the engine) and reports progress through `report(phase, staged, total)`.
/// `remote` is the repository path, for messages and logs only. A failed download keeps the
/// `.part` unless its bytes were proven wrong.
///
/// # Errors
/// [`ModelEnsureError::Cancelled`], or `Failed` with a localized `ai_models.*` message.
#[cfg(not(target_arch = "wasm32"))]
fn download_hf_file_with(
    fetcher: &dyn RangeFetcher,
    retry: &RetryPolicy<'_>,
    cancel: &AtomicBool,
    file: &HfFile<'_>,
    report: &mut dyn FnMut(ModelDownloadPhase, u64, Option<u64>),
) -> Result<(), ModelEnsureError> {
    let HfFile { url, local_path, remote, expected } = *file;
    let part = part_path_for(local_path)
        .ok_or_else(|| ModelEnsureError::Failed(tf!("ai_models.invalid_model_path", local_path = local_path.display())))?;
    let offset = part.metadata().map(|metadata| if metadata.is_file() { metadata.len() } else { 0 }).unwrap_or(0);
    ms_log::runtime_log::log_info(format!("{LOG_TAG} download start: file={remote} resume_from={offset} url={url}"));

    let log_context = format!("file={remote}");
    let engine = ResumableFetch { fetcher, retry, cancel, log_tag: LOG_TAG, log_context: &log_context };
    let request = FileRequest { url, part: &part, dest: local_path, expected };
    let result = fetch_verified(&engine, &request, &mut |event| match event {
        FileEvent::Waiting => report(ModelDownloadPhase::WaitingForOtherDownload, 0, None),
        // Hashing a pinned file (LaMa-MPE) or an LFS file whose sha256 the server announced;
        // shown as progress over the known length.
        FileEvent::Verifying { done, total } => report(ModelDownloadPhase::Downloading, done, Some(total)),
        FileEvent::Stream(StreamEvent::Downloading { staged, total }) => report(ModelDownloadPhase::Downloading, staged, total),
        FileEvent::Stream(StreamEvent::Retrying { attempt, max_attempts, delay_secs, staged }) => {
            report(ModelDownloadPhase::Retrying { attempt, max_attempts, delay_secs }, staged, None);
        }
    });
    match result {
        Ok(size) => {
            ms_log::runtime_log::log_info(format!("{LOG_TAG} download complete: file={remote} bytes={size}"));
            Ok(())
        }
        Err(DownloadError::Cancelled) => {
            ms_log::runtime_log::log_info(format!("{LOG_TAG} download cancelled: file={remote} part={}", part.display()));
            Err(ModelEnsureError::Cancelled)
        }
        Err(error) => {
            ms_log::runtime_log::log_error(format!("{LOG_TAG} download failed: file={remote} part={} error={error:?}", part.display()));
            Err(map_download_error(remote, error))
        }
    }
}

/// Maps an engine failure of `remote` to the typed ensure error (localized text for `Failed`).
#[cfg(not(target_arch = "wasm32"))]
fn map_download_error(remote: &str, error: DownloadError) -> ModelEnsureError {
    match error {
        DownloadError::Cancelled => ModelEnsureError::Cancelled,
        DownloadError::Io { path, source } => ModelEnsureError::Failed(tf!("ai_models.write_error", tmp_path = path.display(), err = source)),
        DownloadError::Http { detail } => ModelEnsureError::Failed(tf!("ai_models.download_error", remote = remote, err = detail)),
        other @ (DownloadError::Busy { .. }
        | DownloadError::InvalidRequest { .. }
        | DownloadError::SizeMismatch { .. }
        | DownloadError::HashMismatch { .. }
        | DownloadError::Archive { .. }) => ModelEnsureError::Failed(tf!("ai_models.download_error", remote = remote, err = other)),
    }
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
