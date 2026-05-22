/*
FILE OVERVIEW: src/ai_models.rs
Application-managed AI model resolver and lazy Hugging Face downloader.

Main responsibilities:
- Define the canonical ManhwaStudio_AI_Models file layout for Rust callers.
- Resolve Hugging Face repository URLs through the official hf-hub crate and stream the selected
  files directly into ManhwaStudio_AI_Models without using HF cache blobs or symlinks.
- Return concrete local paths for Rust model initialization code.

Notes:
- Library-managed caches such as EasyOCR and Surya are intentionally absent here.
- Callers must invoke this from worker/background code, not the GUI thread.
*/

use hf_hub::api::sync::ApiBuilder;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const HF_OWNER: &str = "Vasyanator2";
pub const HF_REPO_NAME: &str = "ManhwaStudio_AI_Models";

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

pub fn ensure_paddle_ocr_full_with_reporter(
    models_root: &Path,
    model_key: &str,
    mut reporter: ModelDownloadReporter<'_>,
) -> Result<(), String> {
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
    )
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
        format!(
            "Не удалось создать корневую папку моделей {}: {err}",
            models_root.display()
        )
    })?;
    let lock = DOWNLOAD_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .map_err(|_| "Глобальная блокировка скачивания моделей повреждена.".to_string())?;

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

    let api = ApiBuilder::from_env()
        .with_progress(false)
        .build()
        .map_err(|err| format!("Не удалось создать Hugging Face client: {err}"))?;
    let repo = api.model(format!("{HF_OWNER}/{HF_REPO_NAME}"));
    for remote in still_missing {
        let url = repo.url(remote);
        download_hf_file_direct(&url, &models_root.join(remote), remote)?;
        let local = models_root.join(remote);
        if !is_nonempty_file(&local) {
            return Err(format!(
                "Hugging Face download завершился, но файл модели не найден: {}",
                local.display()
            ));
        }
    }
    Ok(())
}

fn download_hf_file_direct(url: &str, local_path: &Path, remote: &str) -> Result<(), String> {
    let parent = local_path
        .parent()
        .ok_or_else(|| format!("Некорректный путь модели: {}", local_path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|err| format!("Не удалось создать папку {}: {err}", parent.display()))?;

    let tmp_path = local_path.with_extension("part");
    let response = ureq::get(url)
        .call()
        .map_err(|err| format!("Не удалось скачать модель {remote} из HF: {err}"))?;
    let mut reader = response.into_reader();
    let mut file = fs::File::create(&tmp_path).map_err(|err| {
        format!(
            "Не удалось создать временный файл модели {}: {err}",
            tmp_path.display()
        )
    })?;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| format!("Ошибка чтения HF потока {remote}: {err}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|err| format!("Ошибка записи файла {}: {err}", tmp_path.display()))?;
    }
    file.flush()
        .map_err(|err| format!("Не удалось flush файла {}: {err}", tmp_path.display()))?;
    fs::rename(&tmp_path, local_path).map_err(|err| {
        format!(
            "Не удалось сохранить модель {}: {err}",
            local_path.display()
        )
    })?;
    Ok(())
}

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
