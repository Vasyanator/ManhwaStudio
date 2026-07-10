/*
FILE OVERVIEW: src/project.rs
Project-level data loading and filesystem helpers.

Main items:
- Data models: `Bubble`, `Page`, `CanvasSettings`, `ProjectPaths`, `ProjectData`.
  `Bubble` stores per-bubble placement plus optional `bubble_class`
  (`text`/`image`) and optional `bubble_type` (`default`/`aside`/`on_top`) for text display.
- `CanvasSettings` stores editable/readonly default bubble display types.
- `ProjectData::load`: discovers project/title paths, loads pages, bubbles and settings.
- `load_bubbles` + `LegacyRibbonGeometry`: detect and migrate the very old bubble format
  (absolute Tkinter ribbon `x`/`y`, no `img_u`/`img_v`) into page-normalized coordinates,
  recovering the shared continuous-ribbon scale/offset from all bubbles and the page sizes;
  `persist_migrated_bubbles` rewrites the file once, backing the original up to `*_legacy_xy.json`.
- `reconcile_clean_layers_dir`: renames a legacy `cleaned` folder to `clean_layers` when the
  chapter has the former but not the latter, before any overlay loading happens.
- `convert_jpegs_to_png` / `convert_one_jpeg_to_png`: re-encodes JPEG-content images (detected by
  magic bytes, not by extension) in `src`/`cleaned`/`clean_layers` to real PNG files before
  pages/overlays load. Independent files are converted in parallel over the global rayon pool
  (called from the background load thread). `write_png_fast` writes the internal service-format
  PNGs with fast, low-compression settings (`CompressionType::Fast` + `FilterType::NoFilter`).
- `reconcile_legacy_cleaned_names`: renames legacy `cleaned` overlays using the old
  `<group>_<page>` numbering (for example `1_1.png` -> page `001.png`) onto the current page
  stems, before the overlay loader runs.
- `ProjectData::load`: also reconciles minor clean-overlay filename mismatches against `src/`
  page names (for example `1.png` -> `001.png`) before the overlay loader runs.
- `normalize_page_filenames`: after the reconcile passes, renames every page in `src/` (and its
  matched `clean_layers`/unsaved overlays) to the canonical zero-based three-digit form
  (`000.png`, `001.png`, …) in reading order, so non-numeric pages sort last; runs two-phase via
  temp names so reorderings never collide mid-rename.
- `overlays_already_canonical`: guards the overlay reconcile passes — when a clean folder already
  holds the complete canonical `000..` sequence it is left untouched (it pairs with the pages by
  position), so dropping a fresh 1-based source next to a 0-based clean folder no longer shifts the
  overlays back a page.
- `ProjectData::load_resume_unsaved`: like `load`, but reads bubbles/text-info from the
  `{chapter}_unsaved/` folder when present (crash-recovery mode).
- `ProjectPaths::unsaved_dir` and related fields: paths to the parallel `_unsaved` folder
  where all in-session mutations are staged before an explicit "save to project".
- `ProjectPaths::image_bubbles_dir` and `unsaved_image_bubbles_dir`: saved and staged external
  ImageBubble media directories.
- Canvas settings parsing keeps project-scoped keys in `settings.json`, while selected
  canvas preferences can be sourced from global `user_config.json`.
- Utility helpers for image collection, directory bootstrap and recursive copies.

Storage:
All filesystem access routes through the `crate::storage::storage()` seam (read/write/
read_dir/rename/exists/is_dir/create_dir_all/remove_file) instead of `std::fs`, so the same
loading pipeline works on native (real FS) and web (in-memory store). Images are decoded from
in-memory bytes (`image::load_from_memory` / `ImageReader`) and encoded to a buffer before being
handed to the seam; there is no `copy`, so copies are emulated as read + write. The only native-
specific call left is `canonicalize()` on the incoming real project dir, which has no seam analog
and runs before the virtual layer applies.
*/

use crate::bubble_status::{
    BubbleStatusRule, bubble_status_rules_from_value, default_bubble_status_rules,
};
use crate::config;
use crate::config::JsonConfig;
use crate::runtime_log;
use crate::storage::storage;
use anyhow::{Context, Result};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bubble {
    pub id: i64,
    pub img_idx: usize,
    pub img_u: f32,
    pub img_v: f32,
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bubble_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bubble_type: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub original_text: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub idx: usize,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CanvasSettings {
    pub bubble_type: String,
    pub editable_bubble_type: String,
    pub readonly_bubble_type: String,
    pub show_bubbles: bool,
    pub show_bubble_status: bool,
    pub bubble_status_rules: Vec<BubbleStatusRule>,
    pub bubble_opacity: f32,
    pub aside_min_width_px: i32,
    pub aside_max_width_px: i32,
    pub aside_compact_mode: String,
    pub aside_side_mode: String,
    pub aside_second_column: bool,
    pub on_top_focus_mode: String,
    pub scale_bubbles: bool,
    pub page_spacing_px: i32,
    pub separate_pages: bool,
    pub vertical_edge_margin_px: i32,
    pub side_margin_px: i32,
    pub aside_scale_pct: i32,
    pub auto_insert_last_character: bool,
    pub spellcheck_original: bool,
    pub spellcheck_translation: bool,
    pub tabs_autosync_enabled: bool,
    pub cache_pages: bool,
    pub translation_status_display: String,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ComicType {
    Pages,
    Ribbon,
    Custom,
}

impl ComicType {
    pub fn from_config_value(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pages" => Some(Self::Pages),
            "ribbon" => Some(Self::Ribbon),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Pages => "pages",
            Self::Ribbon => "ribbon",
            Self::Custom => "custom",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Pages => t!("project.comic_type.pages"),
            Self::Ribbon => t!("project.comic_type.ribbon"),
            Self::Custom => t!("project.comic_type.custom"),
        }
    }

    pub fn canvas_preset(self) -> Option<(&'static str, bool)> {
        match self {
            Self::Pages => Some(("strong", true)),
            Self::Ribbon => Some(("none", false)),
            Self::Custom => None,
        }
    }

    pub fn from_canvas_preset_fields(aside_compact_mode: &str, separate_pages: bool) -> Self {
        match (
            aside_compact_mode.trim().to_ascii_lowercase().as_str(),
            separate_pages,
        ) {
            ("moderate", true) => Self::Pages,
            ("none", false) => Self::Ribbon,
            _ => Self::Custom,
        }
    }
}

impl Default for CanvasSettings {
    fn default() -> Self {
        Self {
            bubble_type: "hybrid".to_string(),
            editable_bubble_type: "aside".to_string(),
            readonly_bubble_type: "aside".to_string(),
            show_bubbles: true,
            show_bubble_status: false,
            bubble_status_rules: default_bubble_status_rules(),
            bubble_opacity: 1.0,
            aside_min_width_px: 450,
            aside_max_width_px: 550,
            aside_compact_mode: "none".to_string(),
            aside_side_mode: "auto".to_string(),
            aside_second_column: false,
            on_top_focus_mode: "around".to_string(),
            scale_bubbles: true,
            page_spacing_px: 200,
            separate_pages: true,
            vertical_edge_margin_px: 200,
            side_margin_px: 20,
            aside_scale_pct: 100,
            auto_insert_last_character: true,
            spellcheck_original: false,
            spellcheck_translation: true,
            tabs_autosync_enabled: true,
            cache_pages: true,
            translation_status_display: "until_next".to_string(),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ProjectPaths {
    pub project_dir: PathBuf,
    pub title_dir: PathBuf,
    pub notes_file: PathBuf,
    pub bubbles_file: PathBuf,
    pub src_dir: PathBuf,
    pub clean_layers_dir: PathBuf,
    pub cleaned_dir: PathBuf,
    pub alt_vers_dir: PathBuf,
    pub saved_dir: PathBuf,
    pub image_bubbles_dir: PathBuf,
    pub text_images_dir: PathBuf,
    pub layers_dir: PathBuf,
    pub text_detection_dir: PathBuf,
    pub characters_dir: PathBuf,
    pub terms_file: PathBuf,
    pub settings_file: PathBuf,
    // Unsaved staging folder: {title_dir}/{chapter_name}_unsaved/
    // All in-session mutations are written here; the main folder is only
    // updated on an explicit "save to project" action.
    pub unsaved_dir: PathBuf,
    pub unsaved_bubbles_file: PathBuf,
    pub unsaved_clean_layers_dir: PathBuf,
    pub unsaved_image_bubbles_dir: PathBuf,
    pub unsaved_text_images_dir: PathBuf,
    pub unsaved_layers_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ProjectData {
    pub project_dir: PathBuf,
    pub image_dir: PathBuf,
    pub pages: Vec<Page>,
    pub bubbles: Arc<Vec<Bubble>>,
    #[allow(dead_code)]
    pub paths: ProjectPaths,
    pub comic_type: Option<ComicType>,
    pub canvas_settings: CanvasSettings,
    #[allow(dead_code)]
    pub settings_data: Value,
}

#[allow(dead_code)]
impl ProjectData {
    /// Normal load: bubbles are read from the main chapter folder.
    pub fn load(project_dir: &Path, user_settings: &Value) -> Result<Self> {
        Self::load_internal(project_dir, user_settings, false)
    }

    /// Crash-recovery load: if `{chapter}_unsaved/translation_bubbles.json` exists
    /// it is used instead of the main one; otherwise falls back to the main file.
    pub fn load_resume_unsaved(project_dir: &Path, user_settings: &Value) -> Result<Self> {
        Self::load_internal(project_dir, user_settings, true)
    }

    fn load_internal(
        project_dir: &Path,
        user_settings: &Value,
        resume_unsaved: bool,
    ) -> Result<Self> {
        // `canonicalize` resolves symlinks/`..` against the real filesystem. On
        // wasm the store is virtual (paths are already canonical, no symlinks),
        // so it is skipped and the path is used as-is.
        #[cfg(not(target_arch = "wasm32"))]
        let project_dir = project_dir
            .canonicalize()
            .with_context(|| format!("project dir not found: {}", project_dir.display()))?;
        #[cfg(target_arch = "wasm32")]
        let project_dir = project_dir.to_path_buf();

        let title_dir = project_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project_dir.clone());

        let chapter_name = project_dir
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("chapter")
            .to_string();

        let notes_file = title_dir.join(config::NOTES_FILE);
        let bubbles_file = project_dir.join(config::BUBBLES_FILE);
        let src_dir = ensure_src_dir(&project_dir)?;
        // Migrate the legacy `cleaned` overlay folder to `clean_layers` before any overlay
        // loading happens, so the overlay loader always reads from the current location.
        reconcile_clean_layers_dir(&project_dir)?;
        let clean_layers_dir = project_dir.join(config::CLEAN_LAYERS_DIR);
        let cleaned_dir = project_dir.join(config::CLEANED_DIR);
        // Re-encode any JPEG-content images (detected by magic bytes, not extension) to real PNG
        // before pages and clean overlays are read, so the rest of the pipeline sees only PNG.
        for dir in [&src_dir, &cleaned_dir, &clean_layers_dir] {
            if let Err(err) = convert_jpegs_to_png(dir) {
                runtime_log::log_warn(format!(
                    "JPEG->PNG conversion failed for {}: {err:#}",
                    dir.display()
                ));
            }
        }
        let alt_vers_dir = project_dir.join(config::ALT_VERS_DIR);
        let saved_dir = project_dir.join(config::SAVED_DIR);
        let image_bubbles_dir = project_dir.join("image_bubbles");
        let text_images_dir = project_dir.join(config::TEXT_IMAGES_DIR);
        let layers_dir = project_dir.join(config::LAYERS_DIR);
        let text_detection_dir = project_dir.join(config::TEXT_DETECTION_DIR);
        let characters_dir = title_dir.join(config::CHARACTERS_DIR);
        let terms_file = title_dir.join(config::TERMS_FILE);
        let settings_file = title_dir.join(config::PROJECT_SETTINGS_FILE);

        // Unsaved staging folder lives next to the chapter folder.
        let unsaved_dir = title_dir.join(format!("{chapter_name}_unsaved"));
        let unsaved_bubbles_file = unsaved_dir.join(config::BUBBLES_FILE);
        let unsaved_clean_layers_dir = unsaved_dir.join(config::CLEAN_LAYERS_DIR);
        let unsaved_image_bubbles_dir = unsaved_dir.join("image_bubbles");
        let unsaved_text_images_dir = unsaved_dir.join(config::TEXT_IMAGES_DIR);
        let unsaved_layers_dir = unsaved_dir.join(config::LAYERS_DIR);

        let paths = ProjectPaths {
            project_dir: project_dir.clone(),
            title_dir: title_dir.clone(),
            notes_file,
            bubbles_file: bubbles_file.clone(),
            src_dir: src_dir.clone(),
            clean_layers_dir,
            cleaned_dir,
            alt_vers_dir,
            saved_dir,
            image_bubbles_dir,
            text_images_dir,
            layers_dir,
            text_detection_dir,
            characters_dir,
            terms_file,
            settings_file: settings_file.clone(),
            unsaved_dir: unsaved_dir.clone(),
            unsaved_bubbles_file: unsaved_bubbles_file.clone(),
            unsaved_clean_layers_dir,
            unsaved_image_bubbles_dir,
            unsaved_text_images_dir,
            unsaved_layers_dir,
        };

        let mut pages = collect_images(&src_dir)?;
        // Map legacy `cleaned` numbering (for example `1_1.png` -> page `001.png`) onto the
        // current page stems first, then fix any remaining zero-padding mismatches.
        reconcile_legacy_cleaned_names(&pages, &paths.clean_layers_dir)?;
        reconcile_legacy_cleaned_names(&pages, &paths.unsaved_clean_layers_dir)?;
        reconcile_clean_overlay_names(&pages, &paths.clean_layers_dir)?;
        reconcile_clean_overlay_names(&pages, &paths.unsaved_clean_layers_dir)?;
        // With the overlays now sharing each page's current stem, rename every page (and its
        // matched overlays) to the canonical zero-based three-digit form (`000.png`, `001.png`, …)
        // so the on-disk naming is standardized regardless of the original src / clean_layers names.
        normalize_page_filenames(
            &mut pages,
            &paths.src_dir,
            &[&paths.clean_layers_dir, &paths.unsaved_clean_layers_dir],
        )?;

        // In resume mode, prefer the unsaved bubbles file if it exists.
        let effective_bubbles_file = if resume_unsaved
            && storage().exists(unsaved_bubbles_file.to_string_lossy().as_ref())
        {
            &unsaved_bubbles_file
        } else {
            &bubbles_file
        };
        let (bubbles, migrated) = load_bubbles(effective_bubbles_file, &pages)?;
        // Legacy chapters stored absolute Tkinter ribbon coordinates; once converted to the
        // page-normalized format, persist them so the migration happens only once.
        if migrated && let Err(err) = persist_migrated_bubbles(effective_bubbles_file, &bubbles) {
            runtime_log::log_warn(format!(
                "failed to persist migrated legacy bubbles ({}): {err:#}",
                effective_bubbles_file.display()
            ));
        }

        let settings_cfg = JsonConfig::new(settings_file, config::project_config_defaults())?;
        let comic_type = comic_type_from_config(&settings_cfg.data);
        let canvas_settings = canvas_settings_from_config(&settings_cfg.data, user_settings);

        Ok(Self {
            project_dir: project_dir.clone(),
            image_dir: src_dir,
            pages,
            bubbles: Arc::new(bubbles),
            paths,
            comic_type,
            canvas_settings,
            settings_data: settings_cfg.data,
        })
    }

    pub fn exists(&self) -> bool {
        storage().is_dir(self.project_dir.to_string_lossy().as_ref())
    }

    pub fn autosave_bubbles(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(self.bubbles.as_ref())
            .context("failed to serialize bubbles")?;
        storage()
            .write(
                self.paths.bubbles_file.to_string_lossy().as_ref(),
                raw.as_bytes(),
            )
            .with_context(|| {
                format!(
                    "failed to write bubbles file {}",
                    self.paths.bubbles_file.display()
                )
            })?;
        Ok(())
    }

    pub fn ensure_saved(&self) -> Result<()> {
        convert_src_to_cleaned(&self.paths.src_dir, &self.paths.cleaned_dir)
    }

    pub fn ensure_clean_layers_dir(&self) -> Result<()> {
        if has_any_entries(&self.paths.clean_layers_dir)? {
            return Ok(());
        }
        if !has_any_entries(&self.paths.cleaned_dir)? {
            return Ok(());
        }

        storage()
            .create_dir_all(self.paths.clean_layers_dir.to_string_lossy().as_ref())
            .with_context(|| {
                format!(
                    "failed to create clean layers dir {}",
                    self.paths.clean_layers_dir.display()
                )
            })?;

        copy_dir_recursive(&self.paths.cleaned_dir, &self.paths.clean_layers_dir)
    }

    pub fn ensure_translation_notes(&self) -> Result<()> {
        storage()
            .create_dir_all(self.paths.title_dir.to_string_lossy().as_ref())
            .with_context(|| {
                format!(
                    "failed to create title dir {}",
                    self.paths.title_dir.display()
                )
            })?;
        if storage().exists(self.paths.notes_file.to_string_lossy().as_ref()) {
            return Ok(());
        }
        storage()
            .write(self.paths.notes_file.to_string_lossy().as_ref(), b"")
            .with_context(|| {
                format!(
                    "failed to create translation notes {}",
                    self.paths.notes_file.display()
                )
            })?;
        Ok(())
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Side {
    Left,
    Right,
}

fn ensure_src_dir(project_dir: &Path) -> Result<PathBuf> {
    let src = project_dir.join(config::SRC_DIR);
    if storage().is_dir(src.to_string_lossy().as_ref()) {
        return Ok(src);
    }

    let scr = project_dir.join("scr");
    if storage().is_dir(scr.to_string_lossy().as_ref()) {
        storage()
            .rename(
                scr.to_string_lossy().as_ref(),
                src.to_string_lossy().as_ref(),
            )
            .with_context(|| {
                format!(
                    "failed to rename legacy {} -> {}",
                    scr.display(),
                    src.display()
                )
            })?;
        return Ok(src);
    }

    anyhow::bail!("src/scr not found")
}

/// Migrates a legacy `cleaned` clean-overlay folder to `clean_layers`.
///
/// Renames `{project_dir}/cleaned` to `{project_dir}/clean_layers` only when the target
/// `clean_layers` folder does not yet exist and the legacy `cleaned` folder does. When
/// `clean_layers` already exists (or `cleaned` is absent) this is a no-op, so a chapter that
/// has both folders keeps them untouched.
///
/// # Errors
/// Returns an error if the rename fails.
fn reconcile_clean_layers_dir(project_dir: &Path) -> Result<()> {
    let clean_layers = project_dir.join(config::CLEAN_LAYERS_DIR);
    let cleaned = project_dir.join(config::CLEANED_DIR);
    if storage().is_dir(clean_layers.to_string_lossy().as_ref())
        || !storage().is_dir(cleaned.to_string_lossy().as_ref())
    {
        return Ok(());
    }
    storage()
        .rename(
            cleaned.to_string_lossy().as_ref(),
            clean_layers.to_string_lossy().as_ref(),
        )
        .with_context(|| {
            format!(
                "failed to rename legacy {} -> {}",
                cleaned.display(),
                clean_layers.display()
            )
        })?;
    runtime_log::log_info(format!(
        "migrated legacy clean-overlay folder: {} -> {}",
        cleaned.display(),
        clean_layers.display()
    ));
    Ok(())
}

/// Encodes `image` to `target` as a PNG using fast, low-compression settings.
///
/// These PNGs are an internal service format (decoded again immediately by the page/overlay
/// pipeline), not an archival output, so encode speed matters far more than file size. The
/// `image` crate's default `save_with_format` uses adaptive filtering with the slow default
/// compression, which dominates project-open time. Here `CompressionType::Fast` plus
/// `FilterType::NoFilter` cut encode time substantially while still producing valid PNGs.
///
/// Note: the source is always converted to RGBA8 before encoding (`to_rgba8`), so grayscale and
/// RGB inputs gain an alpha channel and the on-disk PNG is ~33% larger than a channel-preserving
/// encode. This is intentional: downstream load decodes via `to_rgba8()` anyway, so RGBA8-on-disk
/// is semantically harmless and keeps the fast encode path uniform.
///
/// # Errors
/// Returns an error if the file cannot be created or the encoder fails to write `target`.
fn write_png_fast(image: &image::DynamicImage, target: &Path) -> Result<()> {
    let rgba = image.to_rgba8();
    // Encode into an in-memory buffer, then hand the finished PNG bytes to the storage seam.
    // The seam owns the actual byte sink (real file on native, virtual store on web), so no
    // direct file handle is created here.
    let mut buf = Vec::new();
    let encoder = PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::NoFilter);
    rgba.write_with_encoder(encoder)
        .with_context(|| format!("failed to encode PNG {}", target.display()))?;
    storage()
        .write(target.to_string_lossy().as_ref(), &buf)
        .with_context(|| format!("failed to write PNG {}", target.display()))?;
    Ok(())
}

/// Re-encodes JPEG-content images in `dir` to real PNG files.
///
/// JPEG content is detected by its magic bytes (`FF D8 FF`), not by the file extension, so a
/// file named `001.png` that actually holds JPEG bytes is also handled. Each detected file is
/// decoded and written to `<stem>.png`; when the original had a different name (for example
/// `001.jpg`), it is removed after the PNG is written. A `.png`-named JPEG is re-encoded in
/// place. If a different file already occupies the target `<stem>.png`, the source is left
/// untouched and a warning is logged to avoid clobbering unrelated data.
///
/// The per-file decode + PNG re-encode is CPU-bound and the files are independent, so conversion
/// runs in parallel over the shared global rayon pool (this is called from the background project
/// load thread, not the GUI thread). The first per-file error is propagated with full context
/// (path, operation); no detected JPEG is silently dropped.
///
/// A missing directory is a no-op. Returns the number of converted files.
///
/// # Errors
/// Returns an error if the directory cannot be read or a detected JPEG fails to decode or encode.
fn convert_jpegs_to_png(dir: &Path) -> Result<usize> {
    if !storage().is_dir(dir.to_string_lossy().as_ref()) {
        return Ok(0);
    }
    // Collect candidate files sequentially first: directory iteration and JPEG magic-byte
    // detection are cheap I/O, while the dominant decode+encode cost is parallelized below.
    let mut candidates: Vec<PathBuf> = Vec::new();
    let entries = storage()
        .read_dir(dir.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read {}", dir.display()))?;
    for entry in entries {
        // Storage entries are either files or directories; a non-directory is a regular file,
        // mirroring the previous `path.is_file()` filter.
        if entry.is_dir {
            continue;
        }
        let path = dir.join(&entry.name);
        if !file_is_jpeg(&path)? {
            continue;
        }
        candidates.push(path);
    }

    // Convert each independent file in parallel. Every element yields a typed outcome so a single
    // failed conversion surfaces as the first real error instead of being dropped.
    let converted = candidates
        .par_iter()
        .map(|path| convert_one_jpeg_to_png(path))
        .collect::<Result<Vec<bool>>>()?
        .into_iter()
        .filter(|&did_convert| did_convert)
        .count();

    if converted > 0 {
        runtime_log::log_info(format!(
            "converted {converted} JPEG image(s) to PNG in {}",
            dir.display()
        ));
    }
    Ok(converted)
}

/// Converts a single JPEG-content file at `path` to `<stem>.png` using fast PNG settings.
///
/// Returns `Ok(true)` when a PNG was written, or `Ok(false)` when the conversion was skipped
/// because a distinct PNG already occupies the target name (a warning is logged in that case).
///
/// # Errors
/// Returns an error if the source cannot be read, decoded, encoded, or (for a renamed source)
/// removed, with the failing path and operation in the context.
fn convert_one_jpeg_to_png(path: &Path) -> Result<bool> {
    let stem = path.file_stem().and_then(OsStr::to_str).unwrap_or("image");
    let target = path.with_file_name(format!("{stem}.png"));
    // Never overwrite a distinct existing PNG with converted bytes.
    if target != *path && storage().exists(target.to_string_lossy().as_ref()) {
        runtime_log::log_warn(format!(
            "skipping JPEG->PNG conversion for {}: target {} already exists",
            path.display(),
            target.display()
        ));
        return Ok(false);
    }
    let bytes = storage()
        .read(path.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read image {}", path.display()))?;
    let image = image::load_from_memory(&bytes)
        .with_context(|| format!("failed to decode JPEG image {}", path.display()))?;
    write_png_fast(&image, &target)?;
    // Drop the original only when it was a differently named file (for example `.jpg`).
    if target != *path {
        storage()
            .remove_file(path.to_string_lossy().as_ref())
            .with_context(|| format!("failed to remove converted source {}", path.display()))?;
    }
    Ok(true)
}

/// Materializes `cleaned_dir` from `src_dir`: every source image becomes a PNG in `cleaned_dir`.
///
/// `cleaned_dir` is created if missing. If it already contains any entry the call is a no-op (the
/// caller's bootstrap guard), so a leftover placeholder here would permanently disable future
/// conversion — see the cleanup contract below.
///
/// Destination names are resolved sequentially via `unique_png_path` and each is reserved by
/// creating an empty placeholder file, so colliding source stems (e.g. `001.png`/`001.jpg`) get
/// distinct outputs even though the heavy per-file copy/decode/encode runs in parallel afterwards.
/// PNG sources are copied verbatim; other formats are decoded and re-encoded with `write_png_fast`.
///
/// Cleanup contract: every path that does not produce a real output removes its reserved
/// placeholder before returning, so no 0-byte PNG is ever left behind. This covers both the
/// skip path (a non-image source that fails to decode) and every error path (a failed copy,
/// decode, or encode). If placeholder removal itself fails on an error path, the original
/// conversion error is still propagated (the cleanup failure is only logged) so the root cause is
/// not masked.
///
/// Partial-failure limitation: tasks that already finished before the first error keep their real
/// PNGs, and because the caller's guard skips a non-empty `cleaned_dir`, conversion is not retried.
/// This is not a full transactional rollback; the guarantee here is only that the *failed* task
/// leaves no 0-byte file that would corrupt the directory and break later 0-byte PNG decodes.
///
/// # Errors
/// Returns the first real copy/decode/encode error (with path/operation context), or an error if
/// `src_dir`/`cleaned_dir` cannot be read/created or a unique name cannot be resolved.
fn convert_src_to_cleaned(src_dir: &Path, cleaned_dir: &Path) -> Result<()> {
    storage()
        .create_dir_all(cleaned_dir.to_string_lossy().as_ref())
        .with_context(|| format!("failed to create cleaned dir {}", cleaned_dir.display()))?;
    if has_any_entries(cleaned_dir)? {
        return Ok(());
    }

    // Resolve destination names sequentially so `unique_png_path` keeps deterministic,
    // collision-free output (two source files may share a stem, e.g. `001.png`/`001.jpg`).
    // Each reserved name is materialized as an empty file so the next lookup sees it as taken.
    // The heavy per-file copy/decode/encode then runs in parallel below.
    let mut plans: Vec<(PathBuf, PathBuf, bool)> = Vec::new();
    let entries = storage()
        .read_dir(src_dir.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read {}", src_dir.display()))?;
    for entry in entries {
        // A non-directory storage entry is a regular file, mirroring `src_path.is_file()`.
        if entry.is_dir {
            continue;
        }
        let src_path = src_dir.join(&entry.name);

        let stem = src_path
            .file_stem()
            .and_then(OsStr::to_str)
            .unwrap_or("image");
        let ext = src_path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();

        let dst_path = unique_png_path(cleaned_dir, stem)?;
        // Reserve the name on disk so a later `unique_png_path` for a colliding stem skips it.
        // An empty write materializes the 0-byte placeholder the old `File::create` produced.
        storage()
            .write(dst_path.to_string_lossy().as_ref(), &[])
            .with_context(|| format!("failed to reserve cleaned file {}", dst_path.display()))?;
        plans.push((src_path, dst_path, ext == "png"));
    }

    // Copy PNG sources verbatim and decode+re-encode other formats in parallel; non-image
    // files that fail to decode are skipped, mirroring the previous behavior. The first real
    // copy/encode error is propagated with full context. On every non-producing path the reserved
    // 0-byte placeholder is removed so the bootstrap guard never sees a corrupt cleaned dir.
    plans
        .par_iter()
        .map(|(src_path, dst_path, is_png)| -> Result<()> {
            let src_str = src_path.to_string_lossy();
            let dst_str = dst_path.to_string_lossy();
            if *is_png {
                // Storage has no `copy`; emulate as read + write of the raw PNG bytes verbatim.
                return storage()
                    .read(src_str.as_ref())
                    .and_then(|bytes| storage().write(dst_str.as_ref(), &bytes))
                    .map_err(|err| {
                        anyhow::Error::new(err).context(format!(
                            "failed to copy {} -> {}",
                            src_path.display(),
                            dst_path.display()
                        ))
                    })
                    // Remove the reserved placeholder before propagating so a failed copy never
                    // leaves a 0-byte file. The original error is preserved; a cleanup failure is
                    // only logged so it cannot mask the root cause.
                    .map_err(|err| remove_placeholder_on_error(dst_path, err));
            }
            // Decode from storage bytes; a read or decode failure collapses to `None`, matching
            // the old `image::open(..)` `Err(_)` skip arm (non-image or unreadable source).
            let decoded = storage()
                .read(src_str.as_ref())
                .ok()
                .and_then(|bytes| image::load_from_memory(&bytes).ok());
            match decoded {
                Some(img) => write_png_fast(&img, dst_path)
                    .map_err(|err| {
                        err.context(format!(
                            "failed to convert {} -> {}",
                            src_path.display(),
                            dst_path.display()
                        ))
                    })
                    // A decode succeeded but the encode failed: drop the placeholder so retry is
                    // not blocked, preserving the encode error as the propagated cause.
                    .map_err(|err| remove_placeholder_on_error(dst_path, err)),
                None => {
                    // Non-image files are skipped; remove the reserved empty placeholder.
                    storage().remove_file(dst_str.as_ref()).with_context(|| {
                        format!(
                            "failed to remove placeholder for non-image {}",
                            src_path.display()
                        )
                    })
                }
            }
        })
        .collect::<Result<Vec<()>>>()?;

    Ok(())
}

/// Removes the reserved placeholder at `dst_path` while propagating `original` as the cause.
///
/// Used on `convert_src_to_cleaned` error paths so a failed task never leaves a 0-byte PNG. The
/// returned error is always `original` (the real copy/decode/encode failure); if the cleanup
/// removal itself fails it is only logged, never substituted, so the root cause is not masked.
fn remove_placeholder_on_error(dst_path: &Path, original: anyhow::Error) -> anyhow::Error {
    if let Err(cleanup_err) = storage().remove_file(dst_path.to_string_lossy().as_ref()) {
        runtime_log::log_warn(format!(
            "failed to remove reserved placeholder {} after a conversion error: {cleanup_err}",
            dst_path.display()
        ));
    }
    original
}

/// Returns `true` when the file at `path` starts with the JPEG magic bytes `FF D8 FF`.
///
/// # Errors
/// Returns an error if the file cannot be opened or read; a file shorter than three bytes
/// returns `Ok(false)`.
fn file_is_jpeg(path: &Path) -> Result<bool> {
    // The storage seam exposes only whole-file reads, so the full file is loaded to inspect its
    // three magic bytes. A file shorter than three bytes cannot be a JPEG and yields `Ok(false)`,
    // matching the previous `UnexpectedEof` handling of the header read.
    let bytes = storage()
        .read(path.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read header of {}", path.display()))?;
    Ok(bytes.starts_with(&[0xFF, 0xD8, 0xFF]))
}

fn collect_images(dir: &Path) -> Result<Vec<Page>> {
    let entries = storage()
        .read_dir(dir.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read {}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .into_iter()
        // A non-directory storage entry is a regular file, mirroring the old `is_file` filter.
        .filter(|entry| !entry.is_dir)
        .map(|entry| dir.join(entry.name))
        .filter(|p| {
            let ext = p
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            matches!(ext.as_str(), "png" | "jpg" | "jpeg")
        })
        .collect();

    files.sort_by(|a, b| image_sort_key(a, b));

    Ok(files
        .into_iter()
        .enumerate()
        .map(|(idx, path)| Page { idx, path })
        .collect())
}

/// Renames legacy `cleaned` overlay files that use the old `<group>_<page>` numbering
/// (for example `1_1.png` for page 1, `1_19.png` for page 19) onto the current page stems.
///
/// The stem must consist solely of underscore-separated numeric groups; the trailing group is
/// the 1-based page number, mapped to the page with that position. The target name is taken
/// from the actual page stem, so the result inherits the current zero-padded form (for example
/// `001.png`). Files whose stem already equals a page stem are skipped. When several files map
/// to the same page, or the target name is already present, the file is left untouched and the
/// conflict is logged.
///
/// A missing directory is a no-op.
///
/// # Errors
/// Returns an error if the directory cannot be read or a rename fails.
fn reconcile_legacy_cleaned_names(pages: &[Page], overlay_dir: &Path) -> Result<()> {
    if !storage().is_dir(overlay_dir.to_string_lossy().as_ref()) {
        return Ok(());
    }

    // Current page stems by 1-based page position, plus the set of stems already in use.
    let page_stems: Vec<&str> = pages
        .iter()
        .filter_map(|page| page.path.file_stem().and_then(OsStr::to_str))
        .collect();
    if page_stems.len() != pages.len() {
        // A page path without a usable stem would make position mapping unreliable.
        return Ok(());
    }
    let existing_stems: HashSet<&str> = page_stems.iter().copied().collect();

    // Collect candidate renames keyed by target page position to detect collisions.
    let mut by_page: HashMap<usize, Vec<PathBuf>> = HashMap::new();
    let entries = storage()
        .read_dir(overlay_dir.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read overlay directory {}", overlay_dir.display()))?;
    for entry in entries {
        if entry.is_dir {
            continue;
        }
        let path = overlay_dir.join(&entry.name);
        if !is_png_path(&path) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        // Leave files that already match a current page name untouched.
        if existing_stems.contains(stem) {
            continue;
        }
        let Some(page_number) = legacy_cleaned_page_number(stem) else {
            continue;
        };
        if page_number == 0 || page_number > page_stems.len() {
            continue;
        }
        by_page.entry(page_number).or_default().push(path);
    }

    for (page_number, sources) in by_page {
        if sources.len() != 1 {
            runtime_log::log_warn(format!(
                "[cleaned-reconcile] ambiguous legacy name for page #{page_number} in '{}': {} files",
                overlay_dir.display(),
                sources.len()
            ));
            continue;
        }
        let source_path = &sources[0];
        // `page_number` is 1-based; pages are stored in reading order.
        let desired_stem = page_stems[page_number - 1];
        let desired_path = overlay_dir.join(format!("{desired_stem}.png"));
        if storage().exists(desired_path.to_string_lossy().as_ref()) {
            runtime_log::log_warn(format!(
                "[cleaned-reconcile] target '{}' already exists; leaving '{}'",
                desired_path.display(),
                source_path.display()
            ));
            continue;
        }
        storage()
            .rename(
                source_path.to_string_lossy().as_ref(),
                desired_path.to_string_lossy().as_ref(),
            )
            .with_context(|| {
                format!(
                    "failed to rename legacy clean overlay '{}' -> '{}'",
                    source_path.display(),
                    desired_path.display()
                )
            })?;
        runtime_log::log_info(format!(
            "[cleaned-reconcile] page #{page_number} '{}' -> '{}'",
            source_path.display(),
            desired_path.display()
        ));
    }

    Ok(())
}

/// Parses the 1-based page number from a legacy `cleaned` file stem.
///
/// Returns `Some(n)` only when the stem is two or more underscore-separated numeric groups
/// (for example `1_1` -> 1, `1_19` -> 19); the trailing group is the page number. Any
/// non-numeric component or a single group yields `None`, so modern page stems like `001` are
/// ignored.
fn legacy_cleaned_page_number(stem: &str) -> Option<usize> {
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() < 2 {
        return None;
    }
    if !parts
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    parts.last().and_then(|last| last.parse::<usize>().ok())
}

/// Returns `true` when `overlay_dir`'s files are exactly the canonical page sequence
/// `000.png, 001.png, …, {N-1:03}.png` for `N == pages.len()` — every index present, none
/// repeated, no extra or non-PNG file.
///
/// In that state the overlays are already normalized and pair with the pages purely by position,
/// so the reconcile passes must leave them untouched. This is the signal that distinguishes "the
/// clean folder already holds the finished 0-based sequence" (for example the previous source was
/// moved here verbatim, while a fresh 1-based source was dropped into `src/`) from a folder that
/// merely shares per-file numbers with the source and still needs aligning. Without this guard the
/// fuzzy number match in [`reconcile_clean_overlay_names`] pairs 0-based overlays with the 1-based
/// source off by one, shifting every overlay back a page and dropping the first one.
fn overlays_already_canonical(pages: &[Page], overlay_dir: &Path) -> bool {
    if pages.is_empty() || !storage().is_dir(overlay_dir.to_string_lossy().as_ref()) {
        return false;
    }
    let Ok(entries) = storage().read_dir(overlay_dir.to_string_lossy().as_ref()) else {
        return false;
    };
    let mut seen = vec![false; pages.len()];
    let mut count = 0usize;
    for entry in entries {
        if entry.is_dir {
            continue;
        }
        let path = overlay_dir.join(&entry.name);
        // Any non-PNG file or a stem that is not a canonical `{:03}` index means the folder is not
        // a clean canonical overlay set, so the reconcile passes should run as usual.
        if !is_png_path(&path) {
            return false;
        }
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            return false;
        };
        let Ok(idx) = stem.parse::<usize>() else {
            return false;
        };
        // Reject non-canonical padding (`1`, `01`, `0001`) and out-of-range / duplicate indices.
        if format!("{idx:03}") != stem || idx >= pages.len() || seen[idx] {
            return false;
        }
        seen[idx] = true;
        count += 1;
    }
    count == pages.len()
}

fn reconcile_clean_overlay_names(pages: &[Page], overlay_dir: &Path) -> Result<()> {
    if !storage().is_dir(overlay_dir.to_string_lossy().as_ref()) {
        return Ok(());
    }
    // When the overlays already form the complete canonical `000..` sequence they are finished and
    // pair with the pages by position; aligning them to the pages' current (pre-normalize) stems
    // here would wrongly shift them (for example a 0-based clean folder against a 1-based source).
    // Leave them untouched — `normalize_page_filenames` then only renames the source files.
    if overlays_already_canonical(pages, overlay_dir) {
        return Ok(());
    }

    let rename_targets: Vec<(usize, String, PathBuf, String)> = pages
        .iter()
        .filter_map(|page| {
            let stem = page
                .path
                .file_stem()
                .and_then(OsStr::to_str)
                .map(str::trim)
                .filter(|stem| !stem.is_empty())?;
            let match_key = overlay_name_match_key(stem)?;
            Some((
                page.idx,
                stem.to_string(),
                overlay_dir.join(format!("{stem}.png")),
                match_key,
            ))
        })
        .collect();

    let exact_target_paths: HashSet<PathBuf> = rename_targets
        .iter()
        .map(|(_, _, desired_path, _)| desired_path.clone())
        .collect();

    let mut pages_by_key: HashMap<String, Vec<(usize, String, PathBuf)>> = HashMap::new();
    for (page_idx, desired_stem, desired_path, match_key) in rename_targets {
        // These are concrete `<stem>.png` names, so `exists` stands in for the old `is_file`
        // check (a directory of that exact name never occurs in a clean-overlay folder).
        if !storage().exists(desired_path.to_string_lossy().as_ref()) {
            pages_by_key
                .entry(match_key)
                .or_default()
                .push((page_idx, desired_stem, desired_path));
        }
    }

    let mut overlays_by_key: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let entries = storage()
        .read_dir(overlay_dir.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read overlay directory {}", overlay_dir.display()))?;
    for entry in entries {
        if entry.is_dir {
            continue;
        }
        let path = overlay_dir.join(&entry.name);
        if !is_png_path(&path) || exact_target_paths.contains(&path) {
            continue;
        }

        let Some(stem) = path
            .file_stem()
            .and_then(OsStr::to_str)
            .map(str::trim)
            .filter(|stem| !stem.is_empty())
        else {
            continue;
        };
        let Some(match_key) = overlay_name_match_key(stem) else {
            continue;
        };
        overlays_by_key.entry(match_key).or_default().push(path);
    }

    for (match_key, page_candidates) in pages_by_key {
        let Some(overlay_candidates) = overlays_by_key.get(&match_key) else {
            continue;
        };
        if page_candidates.len() != 1 || overlay_candidates.len() != 1 {
            runtime_log::log_warn(format!(
                "[overlay-reconcile] ambiguous match in '{}': key='{}', pages={}, overlays={}",
                overlay_dir.display(),
                match_key,
                page_candidates.len(),
                overlay_candidates.len()
            ));
            continue;
        }

        let (page_idx, desired_stem, desired_path) = &page_candidates[0];
        let source_path = &overlay_candidates[0];
        if storage().exists(desired_path.to_string_lossy().as_ref()) {
            continue;
        }

        storage()
            .rename(
                source_path.to_string_lossy().as_ref(),
                desired_path.to_string_lossy().as_ref(),
            )
            .with_context(|| {
                format!(
                    "failed to rename clean overlay '{}' -> '{}'",
                    source_path.display(),
                    desired_path.display()
                )
            })?;
        runtime_log::log_info(format!(
            "[overlay-reconcile] page #{page_idx} stem='{}' '{}' -> '{}'",
            desired_stem,
            source_path.display(),
            desired_path.display()
        ));
    }

    Ok(())
}

/// Renames every page image (and its matched clean-overlay files) to the canonical zero-based,
/// three-digit `NNN` form: `000.png`, `001.png`, `002.png`, …
///
/// Pages are processed in reading order — the order established by [`collect_images`], which sorts
/// numeric stems ascending and places non-numeric stems last — so `1.png` becomes `000.png`,
/// `2.png` -> `001.png`, `10.png` -> the position its number sorts to, and a page named `cover.png`
/// ends up last. Each page's source file keeps its extension (`1.jpg` -> `000.jpg`); overlay files
/// are always `.png`. For every directory in `overlay_dirs`, a file whose stem currently equals the
/// page's stem is renamed in lockstep, so the overlay keeps matching its page. This runs after the
/// overlay-reconcile passes, which have already brought each overlay onto its page's current stem.
///
/// Renames go through unique temporary names first (two-phase) so a reordering such as
/// `1.png`/`2.png` -> `000.png`/`001.png` never collides mid-rename. `pages` is updated in place to
/// the new source paths (order is preserved; only names change). Pages already in canonical form,
/// and overlays without a matching source file, are left untouched.
///
/// # Errors
/// Returns an error if any rename fails.
fn normalize_page_filenames(
    pages: &mut [Page],
    src_dir: &Path,
    overlay_dirs: &[&Path],
) -> Result<()> {
    struct Plan {
        idx: usize,
        current_stem: String,
        src_ext: String,
        target_stem: String,
    }

    let mut plans: Vec<Plan> = Vec::new();
    for page in pages.iter() {
        let Some(current_stem) = page.path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        let target_stem = format!("{:03}", page.idx);
        // Already canonical: nothing to rename for this page or its overlays.
        if current_stem == target_stem {
            continue;
        }
        let src_ext = page
            .path
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("png")
            .to_string();
        plans.push(Plan {
            idx: page.idx,
            current_stem: current_stem.to_string(),
            src_ext,
            target_stem,
        });
    }

    if plans.is_empty() {
        return Ok(());
    }

    // Phase 1: move every file that will be renamed to a unique temporary name keyed by page
    // index, freeing any target name that a different page currently occupies before phase 2
    // writes the final names.
    for plan in &plans {
        let cur_src = src_dir.join(format!("{}.{}", plan.current_stem, plan.src_ext));
        let tmp_src = src_dir.join(format!("__ms_normalize_{}.{}", plan.idx, plan.src_ext));
        rename_if_exists(&cur_src, &tmp_src)?;
        for dir in overlay_dirs {
            let cur_overlay = dir.join(format!("{}.png", plan.current_stem));
            let tmp_overlay = dir.join(format!("__ms_normalize_{}.png", plan.idx));
            rename_if_exists(&cur_overlay, &tmp_overlay)?;
        }
    }

    // Phase 2: move each temporary file to its final canonical name and update the page path.
    for plan in &plans {
        let tmp_src = src_dir.join(format!("__ms_normalize_{}.{}", plan.idx, plan.src_ext));
        let final_src = src_dir.join(format!("{}.{}", plan.target_stem, plan.src_ext));
        rename_if_exists(&tmp_src, &final_src)?;
        for dir in overlay_dirs {
            let tmp_overlay = dir.join(format!("__ms_normalize_{}.png", plan.idx));
            let final_overlay = dir.join(format!("{}.png", plan.target_stem));
            rename_if_exists(&tmp_overlay, &final_overlay)?;
        }
        if let Some(page) = pages.iter_mut().find(|p| p.idx == plan.idx) {
            page.path = final_src;
        }
        runtime_log::log_info(format!(
            "[name-normalize] page #{} '{}.{}' -> '{}.{}'",
            plan.idx, plan.current_stem, plan.src_ext, plan.target_stem, plan.src_ext
        ));
    }

    Ok(())
}

/// Renames `from` to `to`, treating a missing `from` as a silent no-op.
///
/// # Errors
/// Returns an error if `from` exists but the rename fails.
fn rename_if_exists(from: &Path, to: &Path) -> Result<()> {
    if !storage().exists(from.to_string_lossy().as_ref()) {
        return Ok(());
    }
    storage()
        .rename(from.to_string_lossy().as_ref(), to.to_string_lossy().as_ref())
        .with_context(|| format!("failed to rename '{}' -> '{}'", from.display(), to.display()))?;
    Ok(())
}

fn overlay_name_match_key(stem: &str) -> Option<String> {
    let trimmed = stem.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut key = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() {
            let mut digits = String::new();
            digits.push(ch);
            while let Some(next) = chars.peek().copied() {
                if !next.is_ascii_digit() {
                    break;
                }
                digits.push(next);
                if chars.next().is_none() {
                    break;
                }
            }
            key.push('#');
            let normalized_digits = digits.trim_start_matches('0');
            if normalized_digits.is_empty() {
                key.push('0');
            } else {
                key.push_str(normalized_digits);
            }
            continue;
        }

        for normalized in ch.to_lowercase() {
            key.push(normalized);
        }
    }

    Some(key)
}

fn is_png_path(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|ext| ext.eq_ignore_ascii_case("png"))
        .unwrap_or(false)
}

fn image_sort_key(a: &Path, b: &Path) -> Ordering {
    let an = a.file_name().and_then(OsStr::to_str).unwrap_or_default();
    let bn = b.file_name().and_then(OsStr::to_str).unwrap_or_default();

    let (a_num, a_ext_weight, a_base) = parse_sort_parts(an);
    let (b_num, b_ext_weight, b_base) = parse_sort_parts(bn);

    match (a_num, b_num) {
        (Some(x), Some(y)) => x
            .cmp(&y)
            .then_with(|| a_ext_weight.cmp(&b_ext_weight))
            .then_with(|| a_base.cmp(&b_base)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a_base
            .cmp(&b_base)
            .then_with(|| a_ext_weight.cmp(&b_ext_weight)),
    }
}

fn parse_sort_parts(name: &str) -> (Option<u64>, u8, String) {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    let num = stem.parse::<u64>().ok();
    let ext_weight = match ext.as_str() {
        "png" => 0,
        "jpg" | "jpeg" => 1,
        _ => 2,
    };

    (num, ext_weight, name.to_ascii_lowercase())
}

/// Loads bubbles from `path`, transparently migrating the legacy absolute-coordinate
/// format into the current page-normalized one.
///
/// Very old chapters stored each bubble as a raw `x`/`y` point on a single continuous
/// Tkinter canvas (all pages scaled to one common drawn width and stacked vertically
/// with no gaps) plus a correct `img_idx`, but without `img_u`/`img_v`. Such files are
/// detected per bubble and converted through [`LegacyRibbonGeometry`]; `pages` supplies
/// the page image dimensions needed to recover the ribbon scale.
///
/// Returns the parsed bubbles and `true` when at least one legacy bubble was converted,
/// so the caller can persist the result in the new format. Returns an empty list when
/// the file does not exist.
///
/// # Errors
/// Returns an error if the file cannot be read, is not valid bubble JSON, or legacy
/// conversion fails (for example when page dimensions cannot be read).
fn load_bubbles(path: &Path, pages: &[Page]) -> Result<(Vec<Bubble>, bool)> {
    if !storage().exists(path.to_string_lossy().as_ref()) {
        return Ok((Vec::new(), false));
    }
    let data = storage()
        .read_to_string(path.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read bubbles json: {}", path.display()))?;
    let raw: Vec<Value> = serde_json::from_str(&data)
        .with_context(|| format!("invalid bubbles json: {}", path.display()))?;

    // Fast path: no legacy entries, deserialize straight into the current model.
    if !raw.iter().any(value_is_legacy_xy) {
        let bubbles = raw
            .into_iter()
            .map(serde_json::from_value::<Bubble>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("invalid bubbles json: {}", path.display()))?;
        return Ok((bubbles, false));
    }

    let geometry = LegacyRibbonGeometry::solve(&raw, pages)
        .context("failed to recover legacy ribbon geometry for bubble migration")?;
    let mut bubbles = Vec::with_capacity(raw.len());
    for value in raw {
        if value_is_legacy_xy(&value) {
            bubbles.push(geometry.convert(&value));
        } else {
            bubbles.push(
                serde_json::from_value::<Bubble>(value)
                    .with_context(|| format!("invalid bubbles json: {}", path.display()))?,
            );
        }
    }
    Ok((bubbles, true))
}

/// Returns `true` when `value` is a legacy bubble: numeric `x`/`y` present and no `img_u`.
fn value_is_legacy_xy(value: &Value) -> bool {
    value.get("img_u").is_none()
        && value.get("x").and_then(Value::as_f64).is_some()
        && value.get("y").and_then(Value::as_f64).is_some()
}

/// Clamps a value into the `[0.0, 1.0]` normalized range.
fn clamp01(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

/// Reads `img_idx` from a legacy bubble, clamping it into the valid page range.
fn legacy_img_idx(value: &Value, page_count: usize) -> usize {
    let raw = value.get("img_idx").and_then(Value::as_u64).unwrap_or(0);
    usize::try_from(raw)
        .unwrap_or(0)
        .min(page_count.saturating_sub(1))
}

/// A single legacy bubble reduced to the fields needed to recover ribbon geometry.
#[derive(Debug, Clone, Copy)]
struct LegacyEntry {
    /// Page index, already clamped into the valid page range.
    idx: usize,
    /// Absolute canvas x of the bubble anchor.
    x: f64,
    /// Absolute canvas y of the bubble anchor (cumulative over the ribbon).
    y: f64,
    /// `Some(true)` for the left side, `Some(false)` for the right side.
    is_left: Option<bool>,
}

/// Recovered geometry of the legacy continuous vertical ribbon, used to map absolute
/// Tkinter canvas coordinates back to per-page normalized `img_u`/`img_v`.
///
/// In the legacy layout every page was scaled to one shared drawn width `page_width`
/// and stacked top-to-bottom with no spacing, horizontally centered so each page left
/// edge sat at `page_left`. The page top offsets therefore equal `page_width *
/// cum_aspect[i]`, where `cum_aspect[i]` is the sum of `height/width` aspect ratios of
/// all earlier pages. The scale and horizontal offset are unknown per bubble but shared
/// across the whole chapter, so they are solved once from all bubbles together.
#[derive(Debug, Clone)]
pub(crate) struct LegacyRibbonGeometry {
    /// Shared drawn page width `T` in canvas pixels.
    page_width: f64,
    /// Canvas x of every page's left edge.
    page_left: f64,
    /// Per-page aspect ratio (`height / width`).
    page_aspect: Vec<f64>,
    /// Cumulative aspect ratio per page boundary; length is `page_aspect.len() + 1`.
    cum_aspect: Vec<f64>,
}

impl LegacyRibbonGeometry {
    /// Builds ribbon geometry from precomputed page aspect ratios and absolute ribbon points.
    ///
    /// Shared entry point for reusing the ribbon scale recovery outside bubble loading (for
    /// example the legacy text-overlay migration). `page_aspect[i]` is `height / width` of page
    /// `i`; `points` are `(page_idx, x, y)` absolute ribbon-canvas coordinates. Page indices are
    /// clamped into the valid range. Horizontal offset is estimated without side information.
    pub(crate) fn from_legacy_points(page_aspect: Vec<f64>, points: &[(usize, f64, f64)]) -> Self {
        let max_idx = page_aspect.len().saturating_sub(1);
        let entries: Vec<LegacyEntry> = points
            .iter()
            .map(|&(idx, x, y)| LegacyEntry {
                idx: idx.min(max_idx),
                x,
                y,
                is_left: None,
            })
            .collect();
        Self::from_geometry(page_aspect, &entries)
    }

    /// Solves ribbon geometry from the raw bubble values and the chapter pages.
    ///
    /// Reads page image dimensions (header only) to obtain aspect ratios, then recovers
    /// the shared scale and horizontal offset from every legacy bubble.
    ///
    /// # Errors
    /// Returns an error when the chapter has no pages or a page image header cannot be read.
    fn solve(raw: &[Value], pages: &[Page]) -> Result<Self> {
        if pages.is_empty() {
            anyhow::bail!("cannot convert legacy bubbles: chapter has no source pages");
        }
        let mut page_aspect = Vec::with_capacity(pages.len());
        for page in pages {
            // Storage has no header-only read; load the bytes and let `ImageReader` parse just the
            // dimensions from the in-memory stream (no full decode), preserving the previous
            // `image::image_dimensions` behavior and its single error context.
            let bytes = storage()
                .read(page.path.to_string_lossy().as_ref())
                .with_context(|| {
                    format!("failed to read page dimensions: {}", page.path.display())
                })?;
            let (w, h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .with_context(|| {
                    format!("failed to read page dimensions: {}", page.path.display())
                })?
                .into_dimensions()
                .with_context(|| {
                    format!("failed to read page dimensions: {}", page.path.display())
                })?;
            let w = f64::from(w.max(1));
            let h = f64::from(h.max(1));
            page_aspect.push(h / w);
        }
        let entries: Vec<LegacyEntry> = raw
            .iter()
            .filter(|v| value_is_legacy_xy(v))
            .map(|v| LegacyEntry {
                idx: legacy_img_idx(v, page_aspect.len()),
                x: v.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                y: v.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                is_left: v.get("side").and_then(Value::as_str).and_then(|s| {
                    match s.trim().to_ascii_lowercase().as_str() {
                        "left" => Some(true),
                        "right" => Some(false),
                        _ => None,
                    }
                }),
            })
            .collect();
        Ok(Self::from_geometry(page_aspect, &entries))
    }

    /// Builds geometry from precomputed page aspect ratios and legacy entries.
    ///
    /// Pure function (no I/O) so the recovery math can be unit-tested directly.
    fn from_geometry(page_aspect: Vec<f64>, entries: &[LegacyEntry]) -> Self {
        let mut cum_aspect = Vec::with_capacity(page_aspect.len() + 1);
        cum_aspect.push(0.0);
        for r in &page_aspect {
            let last = cum_aspect.last().copied().unwrap_or(0.0);
            cum_aspect.push(last + r);
        }

        // The page index pins each bubble to a vertical band [cum[i], cum[i+1]] measured in
        // units of page_width, so y/page_width must fall inside that band. Each bubble gives a
        // lower bound (assuming it sits at the band bottom) and, unless it is on the very first
        // page, an upper bound (assuming it sits at the band top). Intersecting all bounds
        // pins the shared page_width.
        let mut t_lo: Option<f64> = None;
        let mut t_hi: Option<f64> = None;
        let mut x_min = f64::INFINITY;
        let mut x_max = f64::NEG_INFINITY;
        let mut left_max: Option<f64> = None;
        let mut right_min: Option<f64> = None;
        for e in entries {
            let top = cum_aspect[e.idx];
            let bottom = cum_aspect[e.idx + 1];
            if bottom > 0.0 {
                let lo = e.y / bottom;
                t_lo = Some(t_lo.map_or(lo, |m| m.max(lo)));
            }
            if top > 0.0 {
                let hi = e.y / top;
                t_hi = Some(t_hi.map_or(hi, |m| m.min(hi)));
            }
            x_min = x_min.min(e.x);
            x_max = x_max.max(e.x);
            match e.is_left {
                Some(true) => left_max = Some(left_max.map_or(e.x, |m| m.max(e.x))),
                Some(false) => right_min = Some(right_min.map_or(e.x, |m| m.min(e.x))),
                None => {}
            }
        }

        let page_width = match (t_lo, t_hi) {
            (Some(lo), Some(hi)) => (lo + hi) * 0.5,
            // Only the first page carries bubbles: no upper bound, so assume the lowest bubble
            // sits at the page bottom (tightest consistent scale).
            (Some(lo), None) => lo,
            (None, Some(hi)) => hi,
            (None, None) => 1.0,
        };
        let page_width = if page_width.is_finite() && page_width > 0.0 {
            page_width
        } else {
            1.0
        };

        let page_left = solve_page_left(page_width, x_min, x_max, left_max, right_min);
        Self {
            page_width,
            page_left,
            page_aspect,
            cum_aspect,
        }
    }

    /// Converts one legacy bubble value into the current normalized [`Bubble`].
    ///
    /// `value` must be a legacy bubble (see [`value_is_legacy_xy`]); other fields
    /// (`id`, `side`, `text`) are carried over unchanged and `img_u`/`img_v` are derived.
    fn convert(&self, value: &Value) -> Bubble {
        let idx = legacy_img_idx(value, self.page_aspect.len());
        let x = value.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let y = value.get("y").and_then(Value::as_f64).unwrap_or(0.0);
        let (u, v) = self.to_uv(idx, x, y);
        Bubble {
            id: value.get("id").and_then(Value::as_i64).unwrap_or(0),
            img_idx: idx,
            // Normalized UV: f64 -> f32 narrowing is safe, values are clamped to [0,1].
            img_u: u as f32,
            img_v: v as f32,
            side: value
                .get("side")
                .and_then(Value::as_str)
                .map(str::to_string),
            bubble_class: None,
            bubble_type: None,
            text: value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            original_text: String::new(),
            extra: Map::new(),
        }
    }

    /// Maps an absolute canvas point on page `idx` to normalized `(u, v)` in `[0, 1]`.
    pub(crate) fn to_uv(&self, idx: usize, x: f64, y: f64) -> (f64, f64) {
        let idx = idx.min(self.page_aspect.len().saturating_sub(1));
        let aspect = self.page_aspect.get(idx).copied().unwrap_or(1.0).max(1e-9);
        let top = self.cum_aspect.get(idx).copied().unwrap_or(0.0);
        let v = ((y / self.page_width) - top) / aspect;
        let u = (x - self.page_left) / self.page_width;
        (clamp01(u), clamp01(v))
    }
}

/// Recovers the canvas x of the (shared) page left edge for the legacy ribbon.
///
/// Pages were horizontally centered, so the left/right side split sits at the page
/// center. The center is estimated from the boundary between left- and right-side
/// bubbles, then nudged so every bubble x stays inside `[left, left + page_width]`
/// whenever the bubble x-span fits within one page width.
fn solve_page_left(
    page_width: f64,
    x_min: f64,
    x_max: f64,
    left_max: Option<f64>,
    right_min: Option<f64>,
) -> f64 {
    let span_mid = if x_min.is_finite() && x_max.is_finite() {
        (x_min + x_max) * 0.5
    } else {
        0.0
    };
    let center = match (left_max, right_min) {
        (Some(l), Some(r)) => (l + r) * 0.5,
        // Only one side is present: the page extends a quarter width past the clicks.
        (Some(l), None) => l + page_width * 0.25,
        (None, Some(r)) => r - page_width * 0.25,
        (None, None) => span_mid,
    };
    let mut left = center - page_width * 0.5;
    // Keep every bubble inside the page horizontally when the span allows it.
    if x_min.is_finite() && x_max.is_finite() && (x_max - x_min) <= page_width {
        let lo = x_max - page_width; // left >= lo keeps u <= 1
        let hi = x_min; // left <= hi keeps u >= 0
        if lo <= hi {
            left = left.clamp(lo, hi);
        }
    }
    left
}

/// Persists migrated legacy bubbles back to `path` in the current format.
///
/// The original legacy file is backed up once to a sibling `*_legacy_xy.json` file before
/// being overwritten, so the pre-migration data is never lost.
///
/// # Errors
/// Returns an error if the backup copy or the rewrite fails.
fn persist_migrated_bubbles(path: &Path, bubbles: &[Bubble]) -> Result<()> {
    let backup = legacy_backup_path(path);
    if !storage().exists(backup.to_string_lossy().as_ref()) {
        // Storage has no `copy`; emulate the one-time backup as read + write of the raw bytes.
        let original = storage()
            .read(path.to_string_lossy().as_ref())
            .with_context(|| format!("failed to back up legacy bubbles to {}", backup.display()))?;
        storage()
            .write(backup.to_string_lossy().as_ref(), &original)
            .with_context(|| format!("failed to back up legacy bubbles to {}", backup.display()))?;
    }
    let json =
        serde_json::to_string_pretty(bubbles).context("failed to serialize migrated bubbles")?;
    storage()
        .write(path.to_string_lossy().as_ref(), json.as_bytes())
        .with_context(|| format!("failed to write migrated bubbles: {}", path.display()))?;
    runtime_log::log_info(format!(
        "migrated {} legacy bubble(s) to page-normalized format: {}",
        bubbles.len(),
        path.display()
    ));
    Ok(())
}

/// Builds the one-time backup path for a legacy bubbles file (`*_legacy_xy.json`).
fn legacy_backup_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("translation_bubbles");
    path.with_file_name(format!("{stem}_legacy_xy.json"))
}

fn canvas_settings_from_config(settings: &Value, user_settings: &Value) -> CanvasSettings {
    let mut out = CanvasSettings::default();
    let canvas = settings.get("canvas");
    let user_canvas = user_settings.get("Canvas");

    if let Some(v) = canvas
        .and_then(|c| c.get("bubble_type"))
        .or_else(|| settings.get("bubble_type"))
        .and_then(Value::as_str)
    {
        out.bubble_type = v.to_string();
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("editable_bubble_type"))
        .or_else(|| settings.get("editable_bubble_type"))
        .and_then(Value::as_str)
    {
        out.editable_bubble_type = v.to_string();
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("readonly_bubble_type"))
        .or_else(|| settings.get("readonly_bubble_type"))
        .and_then(Value::as_str)
    {
        out.readonly_bubble_type = v.to_string();
    }
    if out.bubble_type.eq_ignore_ascii_case("aside")
        || out.bubble_type.eq_ignore_ascii_case("on_top")
    {
        out.editable_bubble_type = out.bubble_type.clone();
        out.readonly_bubble_type = out.bubble_type.clone();
        out.bubble_type = "hybrid".to_string();
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("show_bubbles"))
        .and_then(Value::as_bool)
    {
        out.show_bubbles = v;
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("show_bubble_status"))
        .or_else(|| settings.get("show_bubble_status"))
        .and_then(Value::as_bool)
    {
        out.show_bubble_status = v;
    }
    if let Some(rules) = user_canvas
        .and_then(|c| c.get("bubble_status_rules"))
        .or_else(|| canvas.and_then(|c| c.get("bubble_status_rules")))
        .and_then(bubble_status_rules_from_value)
    {
        out.bubble_status_rules = rules;
    }
    if let Some(v) = user_canvas
        .and_then(|c| c.get("aside_min_width_px"))
        .or_else(|| canvas.and_then(|c| c.get("aside_min_width_px")))
        .and_then(Value::as_i64)
    {
        out.aside_min_width_px = (v as i32).max(40);
    }
    if let Some(v) = user_canvas
        .and_then(|c| c.get("aside_max_width_px"))
        .or_else(|| canvas.and_then(|c| c.get("aside_max_width_px")))
        .and_then(Value::as_i64)
    {
        out.aside_max_width_px = (v as i32).max(40);
    }
    if let Some(v) = user_canvas
        .and_then(|c| c.get("aside_compact_mode"))
        .or_else(|| canvas.and_then(|c| c.get("aside_compact_mode")))
        .and_then(Value::as_str)
    {
        out.aside_compact_mode = v.to_string();
    }
    if let Some(v) = user_canvas
        .and_then(|c| c.get("aside_side_mode"))
        .or_else(|| canvas.and_then(|c| c.get("aside_side_mode")))
        .and_then(Value::as_str)
    {
        out.aside_side_mode = v.to_string();
    }
    if let Some(v) = user_canvas
        .and_then(|c| c.get("aside_second_column"))
        .or_else(|| canvas.and_then(|c| c.get("aside_second_column")))
        .and_then(Value::as_bool)
    {
        out.aside_second_column = v;
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("bubble_opacity"))
        .and_then(Value::as_f64)
    {
        out.bubble_opacity = (v as f32).clamp(0.0, 1.0);
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("on_top_focus_mode"))
        .or_else(|| settings.get("on_top_focus_mode"))
        .and_then(Value::as_str)
    {
        out.on_top_focus_mode = v.to_string();
    }
    if let Some(v) = user_canvas
        .and_then(|c| c.get("scale_bubbles"))
        .or_else(|| canvas.and_then(|c| c.get("scale_bubbles")))
        .and_then(Value::as_bool)
    {
        out.scale_bubbles = v;
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("page_spacing_px"))
        .and_then(Value::as_i64)
    {
        out.page_spacing_px = (v as i32).max(0);
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("separate_pages"))
        .and_then(Value::as_bool)
    {
        out.separate_pages = v;
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("vertical_edge_margin_px"))
        .and_then(Value::as_i64)
    {
        out.vertical_edge_margin_px = (v as i32).max(0);
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("side_margin_px"))
        .and_then(Value::as_i64)
    {
        out.side_margin_px = (v as i32).max(0);
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("aside_scale_pct"))
        .and_then(Value::as_i64)
    {
        out.aside_scale_pct = (v as i32).clamp(25, 300);
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("auto_insert_last_character"))
        .or_else(|| settings.get("auto_insert_last_character"))
        .and_then(Value::as_bool)
    {
        out.auto_insert_last_character = v;
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("spellcheck_original"))
        .and_then(Value::as_bool)
    {
        out.spellcheck_original = v;
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("spellcheck_translation"))
        .and_then(Value::as_bool)
    {
        out.spellcheck_translation = v;
    }
    if let Some(v) = canvas
        .and_then(|c| c.get("tabs_autosync_enabled"))
        .and_then(Value::as_bool)
    {
        out.tabs_autosync_enabled = v;
    }
    if let Some(v) = user_canvas
        .and_then(|c| c.get("cache_pages"))
        .or_else(|| canvas.and_then(|c| c.get("cache_pages")))
        .and_then(Value::as_bool)
    {
        out.cache_pages = v;
    }
    if let Some(v) = user_canvas
        .and_then(|c| c.get("translation_status_display"))
        .or_else(|| canvas.and_then(|c| c.get("translation_status_display")))
        .and_then(Value::as_str)
    {
        out.translation_status_display = v.to_string();
    }
    if out.aside_max_width_px < out.aside_min_width_px {
        out.aside_max_width_px = out.aside_min_width_px;
    }
    out
}

fn comic_type_from_config(settings: &Value) -> Option<ComicType> {
    settings
        .get("comic_type")
        .and_then(Value::as_str)
        .and_then(ComicType::from_config_value)
}

pub fn save_comic_type_to_project_file(
    settings_file: &Path,
    comic_type: ComicType,
) -> Result<(), String> {
    let mut root = if storage().exists(settings_file.to_string_lossy().as_ref()) {
        match storage().read_to_string(settings_file.to_string_lossy().as_ref()) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(err) => {
                return Err(format!(
                    "failed to read project settings '{}': {err}",
                    settings_file.display()
                ));
            }
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let Some(root_obj) = root.as_object_mut() else {
        return Err(format!(
            "project settings root is not an object: '{}'",
            settings_file.display()
        ));
    };
    root_obj.insert(
        "comic_type".to_string(),
        Value::String(comic_type.as_config_str().to_string()),
    );

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = settings_file.parent() {
        storage()
            .create_dir_all(parent.to_string_lossy().as_ref())
            .map_err(|err| err.to_string())?;
    }
    storage()
        .write(settings_file.to_string_lossy().as_ref(), payload.as_bytes())
        .map_err(|err| err.to_string())?;
    Ok(())
}

#[allow(dead_code)]
fn has_any_entries(dir: &Path) -> Result<bool> {
    let dir_str = dir.to_string_lossy();
    if !storage().is_dir(dir_str.as_ref()) {
        return Ok(false);
    }
    let entries = storage()
        .read_dir(dir_str.as_ref())
        .with_context(|| format!("failed to read directory {}", dir.display()))?;
    Ok(!entries.is_empty())
}

/// Maximum `<stem>-<i>` suffixes tried before giving up on resolving a unique name.
/// A chapter never legitimately has this many colliding stems; hitting the bound means a
/// pathological directory (or a filesystem race recreating names), so we stop instead of
/// looping unboundedly or overflowing the counter.
const UNIQUE_PNG_PATH_MAX_ATTEMPTS: u32 = 65_536;

/// Resolves a non-colliding `<stem>.png` path inside `dst_dir`.
///
/// Returns `<stem>.png` when free, otherwise the first free `<stem>-<i>.png` for `i >= 1`.
///
/// # Errors
/// Returns an error if no free name is found within `UNIQUE_PNG_PATH_MAX_ATTEMPTS` suffixes,
/// rather than looping forever or overflowing the suffix counter.
fn unique_png_path(dst_dir: &Path, stem: &str) -> Result<PathBuf> {
    let base = dst_dir.join(format!("{stem}.png"));
    if !storage().exists(base.to_string_lossy().as_ref()) {
        return Ok(base);
    }
    for i in 1..=UNIQUE_PNG_PATH_MAX_ATTEMPTS {
        let candidate = dst_dir.join(format!("{stem}-{i}.png"));
        if !storage().exists(candidate.to_string_lossy().as_ref()) {
            return Ok(candidate);
        }
    }
    Err(anyhow::anyhow!(
        "could not resolve a unique PNG name for stem '{stem}' in {} after {} attempts",
        dst_dir.display(),
        UNIQUE_PNG_PATH_MAX_ATTEMPTS
    ))
}

#[allow(dead_code)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    storage()
        .create_dir_all(dst.to_string_lossy().as_ref())
        .with_context(|| format!("failed to create directory {}", dst.display()))?;

    let entries = storage()
        .read_dir(src.to_string_lossy().as_ref())
        .with_context(|| format!("failed to read {}", src.display()))?;
    for entry in entries {
        let src_path = src.join(&entry.name);
        let dst_path = dst.join(&entry.name);
        if entry.is_dir {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            let src_str = src_path.to_string_lossy();
            let dst_str = dst_path.to_string_lossy();
            // Storage has no `copy`; emulate as read + write. Preserve the previous NotFound
            // retry: when the destination's parent directory is missing, create it and copy again.
            match storage()
                .read(src_str.as_ref())
                .and_then(|bytes| storage().write(dst_str.as_ref(), &bytes))
            {
                Ok(()) => {}
                Err(ms_storage::StorageError::NotFound(_)) => {
                    if let Some(parent) = dst_path.parent() {
                        storage().create_dir_all(parent.to_string_lossy().as_ref())?;
                    }
                    let bytes = storage().read(src_str.as_ref())?;
                    storage().write(dst_str.as_ref(), &bytes)?;
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "failed to copy {} -> {}",
                            src_path.display(),
                            dst_path.display()
                        )
                    });
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LegacyEntry, LegacyRibbonGeometry, overlay_name_match_key, value_is_legacy_xy};
    use serde_json::json;

    /// Forward map used only by tests: place a normalized (u, v) point on page `idx`
    /// back into absolute legacy canvas coordinates for a known scale/offset.
    fn forward_xy(
        page_aspect: &[f64],
        page_width: f64,
        page_left: f64,
        idx: usize,
        u: f64,
        v: f64,
    ) -> (f64, f64) {
        let mut top = 0.0;
        for r in &page_aspect[..idx] {
            top += r;
        }
        let x = page_left + u * page_width;
        let y = page_width * (top + v * page_aspect[idx]);
        (x, y)
    }

    #[test]
    fn legacy_detection_matches_only_xy_without_uv() {
        assert!(value_is_legacy_xy(
            &json!({"id": 1, "img_idx": 0, "x": 10.0, "y": 20.0, "side": "left"})
        ));
        assert!(!value_is_legacy_xy(
            &json!({"id": 1, "img_idx": 0, "img_u": 0.5, "img_v": 0.5})
        ));
        assert!(!value_is_legacy_xy(&json!({"id": 1, "img_idx": 0})));
    }

    #[test]
    fn ribbon_geometry_round_trips_multi_page() {
        // Three stacked pages with distinct aspect ratios.
        let page_aspect = vec![18.55, 11.59, 2.32];
        let page_width = 632.0;
        let page_left = 635.0;

        // Sample points across pages. As on a real ribbon, every page carries bubbles near its
        // top and bottom edges, which tightens the recovered scale; points straddling the
        // left/right split make the horizontal offset recoverable.
        let samples = [
            (0usize, 0.49, 0.02),
            (0, 0.51, 0.50),
            (0, 0.20, 0.98),
            (1, 0.80, 0.02),
            (1, 0.49, 0.55),
            (1, 0.55, 0.98),
            (2, 0.51, 0.02),
            (2, 0.30, 0.50),
            (2, 0.95, 0.98),
        ];
        let entries: Vec<LegacyEntry> = samples
            .iter()
            .map(|&(idx, u, v)| {
                let (x, y) = forward_xy(&page_aspect, page_width, page_left, idx, u, v);
                LegacyEntry {
                    idx,
                    x,
                    y,
                    is_left: Some(u < 0.5),
                }
            })
            .collect();

        let geom = LegacyRibbonGeometry::from_geometry(page_aspect.clone(), &entries);
        for (&(idx, u, v), e) in samples.iter().zip(entries.iter()) {
            let (ru, rv) = geom.to_uv(idx, e.x, e.y);
            assert!((rv - v).abs() < 1e-3, "v mismatch page {idx}: {rv} vs {v}");
            assert!((ru - u).abs() < 5e-3, "u mismatch page {idx}: {ru} vs {u}");
        }
    }

    #[test]
    fn ribbon_geometry_single_page_keeps_values_in_range() {
        // All bubbles on page 0 (no upper scale bound): the lowest must map near v = 1.
        let page_aspect = vec![18.55, 11.59];
        let page_width = 632.0;
        let page_left = 635.0;
        let vs = [0.1, 0.5, 1.0];
        let entries: Vec<LegacyEntry> = vs
            .iter()
            .map(|&v| {
                let (x, y) = forward_xy(&page_aspect, page_width, page_left, 0, 0.5, v);
                LegacyEntry {
                    idx: 0,
                    x,
                    y,
                    is_left: Some(false),
                }
            })
            .collect();
        let geom = LegacyRibbonGeometry::from_geometry(page_aspect, &entries);
        for (&v, e) in vs.iter().zip(entries.iter()) {
            let (_, rv) = geom.to_uv(0, e.x, e.y);
            assert!((0.0..=1.0).contains(&rv));
            // The deepest sample (v = 1.0) maps to the page bottom under the fallback scale.
            if (v - 1.0).abs() < f64::EPSILON {
                assert!((rv - 1.0).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn overlay_match_key_normalizes_numeric_padding() {
        assert_eq!(overlay_name_match_key("1"), overlay_name_match_key("001"));
        assert_eq!(
            overlay_name_match_key("page1"),
            overlay_name_match_key("page001")
        );
        assert_eq!(
            overlay_name_match_key("Page_0007"),
            overlay_name_match_key("page_7")
        );
    }

    #[test]
    fn overlay_match_key_keeps_distinct_names_separate() {
        assert_ne!(overlay_name_match_key("1a"), overlay_name_match_key("1b"));
        assert_ne!(
            overlay_name_match_key("chapter10"),
            overlay_name_match_key("chapter11")
        );
    }

    /// Builds a process-unique temporary directory under the system temp root.
    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        use web_time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "manhwastudio_test_{tag}_{}_{nanos}",
            std::process::id()
        ))
    }

    /// Encodes a tiny solid image to JPEG bytes.
    fn jpeg_bytes() -> image::ImageResult<Vec<u8>> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            8,
            8,
            image::Rgb([120, 30, 200]),
        ));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg)?;
        Ok(buf.into_inner())
    }

    /// Encodes a tiny solid image to PNG bytes.
    fn png_bytes() -> image::ImageResult<Vec<u8>> {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            8,
            image::Rgba([10, 20, 30, 255]),
        ));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png)?;
        Ok(buf.into_inner())
    }

    fn is_png(bytes: &[u8]) -> bool {
        bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47])
    }

    #[test]
    fn jpeg_magic_detection_ignores_extension() -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_temp_dir("jpeg_magic");
        std::fs::create_dir_all(&dir)?;
        let jpg = dir.join("a.bin");
        std::fs::write(&jpg, jpeg_bytes()?)?;
        let png = dir.join("b.bin");
        std::fs::write(&png, png_bytes()?)?;
        assert!(super::file_is_jpeg(&jpg)?);
        assert!(!super::file_is_jpeg(&png)?);
        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn legacy_cleaned_page_number_parses_only_numeric_groups() {
        use super::legacy_cleaned_page_number;
        assert_eq!(legacy_cleaned_page_number("1_1"), Some(1));
        assert_eq!(legacy_cleaned_page_number("1_19"), Some(19));
        assert_eq!(legacy_cleaned_page_number("2_5"), Some(5));
        assert_eq!(legacy_cleaned_page_number("001"), None);
        assert_eq!(legacy_cleaned_page_number("page_1"), None);
        assert_eq!(legacy_cleaned_page_number("1_"), None);
    }

    #[test]
    fn reconcile_legacy_cleaned_names_maps_to_page_stems() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = unique_temp_dir("cleaned_names");
        std::fs::create_dir_all(&dir)?;
        // Modern three-digit zero-padded page stems.
        let pages: Vec<super::Page> = (1..=3)
            .map(|n| super::Page {
                idx: n - 1,
                path: std::path::PathBuf::from(format!("/src/{n:03}.png")),
            })
            .collect();
        // Legacy cleaned files use the `<group>_<page>` numbering; content is irrelevant here.
        for legacy in ["1_1.png", "1_2.png", "1_3.png"] {
            std::fs::write(dir.join(legacy), b"x")?;
        }
        super::reconcile_legacy_cleaned_names(&pages, &dir)?;
        assert!(dir.join("001.png").exists());
        assert!(dir.join("002.png").exists());
        assert!(dir.join("003.png").exists());
        assert!(!dir.join("1_1.png").exists());
        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    #[test]
    fn normalize_page_filenames_canonicalizes_src_and_overlays()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("name_normalize");
        let src = root.join("src");
        let overlays = root.join("clean_layers");
        std::fs::create_dir_all(&src)?;
        std::fs::create_dir_all(&overlays)?;

        // Source images in reading order: `1.png`, `2.png`, `10.png`, then a non-numeric page.
        // `collect_images` would hand them to us in exactly this order/idx, with `cover` last.
        let src_names = ["1.png", "2.png", "10.png", "cover.png"];
        for name in src_names {
            std::fs::write(src.join(name), b"img")?;
        }
        // Overlays already share each page's current stem (the reconcile passes ran before us).
        // Page `2.png` deliberately has no overlay to confirm a missing overlay is a no-op.
        for name in ["1.png", "10.png", "cover.png"] {
            std::fs::write(overlays.join(name), name.as_bytes())?;
        }

        let mut pages: Vec<super::Page> = src_names
            .iter()
            .enumerate()
            .map(|(idx, name)| super::Page {
                idx,
                path: src.join(name),
            })
            .collect();

        super::normalize_page_filenames(&mut pages, &src, &[&overlays])?;

        // Every source file is now canonical and the originals are gone.
        for (idx, original) in src_names.iter().enumerate() {
            let canonical = format!("{idx:03}.png");
            assert!(src.join(&canonical).exists(), "missing {canonical}");
            assert!(!src.join(original).exists(), "stale {original}");
            assert_eq!(pages[idx].path, src.join(&canonical));
        }

        // Overlays moved in lockstep with their pages; the non-numeric page sorts last (003).
        assert_eq!(std::fs::read(overlays.join("000.png"))?, b"1.png");
        assert_eq!(std::fs::read(overlays.join("002.png"))?, b"10.png");
        assert_eq!(std::fs::read(overlays.join("003.png"))?, b"cover.png");
        assert!(!overlays.join("001.png").exists());
        assert!(!overlays.join("1.png").exists());

        std::fs::remove_dir_all(&root).ok();
        Ok(())
    }

    #[test]
    fn canonical_overlays_are_untouched_when_new_source_is_one_based()
    -> Result<(), Box<dyn std::error::Error>> {
        // Reproduces the reported workflow: the previous source (`000.png`..) was moved into the
        // clean folder verbatim, and a fresh 1-based source (`1.png`..) was dropped into `src/`.
        // The source must be renamed to canonical form while the clean overlays stay exactly as
        // they are — no shift, no dropped first page.
        let root = unique_temp_dir("canonical_overlays");
        let src = root.join("src");
        let clean = root.join("clean_layers");
        std::fs::create_dir_all(&src)?;
        std::fs::create_dir_all(&clean)?;

        const N: usize = 5;
        for n in 1..=N {
            std::fs::write(src.join(format!("{n}.png")), format!("src-{n}").as_bytes())?;
        }
        for i in 0..N {
            std::fs::write(clean.join(format!("{i:03}.png")), format!("clean-{i}").as_bytes())?;
        }

        // Run the same pass sequence as `ProjectData::load_internal`.
        let mut pages = super::collect_images(&src)?;
        super::reconcile_legacy_cleaned_names(&pages, &clean)?;
        super::reconcile_clean_overlay_names(&pages, &clean)?;
        super::normalize_page_filenames(&mut pages, &src, &[&clean])?;

        // Source is now canonical and the originals are gone.
        for (i, page) in pages.iter().enumerate() {
            assert!(src.join(format!("{i:03}.png")).exists(), "missing src {i:03}");
            assert!(!src.join(format!("{}.png", i + 1)).exists(), "stale src {}", i + 1);
            assert_eq!(page.path, src.join(format!("{i:03}.png")));
        }
        // Every clean overlay is byte-for-byte intact at its original canonical name.
        for i in 0..N {
            let content = std::fs::read(clean.join(format!("{i:03}.png")))?;
            assert_eq!(content, format!("clean-{i}").as_bytes(), "overlay {i:03} changed");
        }
        // No stray temp files were left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&clean)?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("__ms_normalize"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left in clean dir");

        std::fs::remove_dir_all(&root).ok();
        Ok(())
    }

    #[test]
    fn overlays_already_canonical_detects_complete_sequence_only()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("canonical_detect");
        let dir = root.join("clean");
        std::fs::create_dir_all(&dir)?;
        let pages: Vec<super::Page> = (0..3)
            .map(|idx| super::Page {
                idx,
                path: std::path::PathBuf::from(format!("/src/{idx:03}.png")),
            })
            .collect();

        // Complete `000..002` set → canonical.
        for i in 0..3 {
            std::fs::write(dir.join(format!("{i:03}.png")), b"x")?;
        }
        assert!(super::overlays_already_canonical(&pages, &dir));

        // A gap (remove `001`) → not canonical.
        std::fs::remove_file(dir.join("001.png"))?;
        assert!(!super::overlays_already_canonical(&pages, &dir));
        std::fs::write(dir.join("001.png"), b"x")?;

        // Non-canonical padding (`1.png` instead of `001.png`) → not canonical.
        let dir2 = root.join("clean2");
        std::fs::create_dir_all(&dir2)?;
        for i in 0..3 {
            std::fs::write(dir2.join(format!("{i}.png")), b"x")?;
        }
        assert!(!super::overlays_already_canonical(&pages, &dir2));

        std::fs::remove_dir_all(&root).ok();
        Ok(())
    }

    #[test]
    fn normalize_page_filenames_is_noop_when_already_canonical()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("name_normalize_noop");
        let src = root.join("src");
        std::fs::create_dir_all(&src)?;
        for name in ["000.png", "001.jpg"] {
            std::fs::write(src.join(name), b"img")?;
        }
        let mut pages: Vec<super::Page> = ["000.png", "001.jpg"]
            .iter()
            .enumerate()
            .map(|(idx, name)| super::Page {
                idx,
                path: src.join(name),
            })
            .collect();

        super::normalize_page_filenames(&mut pages, &src, &[])?;

        // The canonical `.png` stays; the canonical `.jpg` keeps its extension untouched.
        assert!(src.join("000.png").exists());
        assert!(src.join("001.jpg").exists());
        assert_eq!(pages[1].path, src.join("001.jpg"));
        std::fs::remove_dir_all(&root).ok();
        Ok(())
    }

    #[test]
    fn converts_jpeg_content_regardless_of_extension() -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_temp_dir("jpeg_convert");
        std::fs::create_dir_all(&dir)?;
        let jpeg = jpeg_bytes()?;
        // JPEG content named `.jpg` and JPEG content masquerading as `.png`.
        std::fs::write(dir.join("001.jpg"), &jpeg)?;
        std::fs::write(dir.join("002.png"), &jpeg)?;
        // A genuine PNG must be left untouched.
        let real_png = png_bytes()?;
        std::fs::write(dir.join("003.png"), &real_png)?;

        let converted = super::convert_jpegs_to_png(&dir)?;
        assert_eq!(converted, 2);

        assert!(dir.join("001.png").exists());
        assert!(!dir.join("001.jpg").exists());
        assert!(is_png(&std::fs::read(dir.join("001.png"))?));
        assert!(is_png(&std::fs::read(dir.join("002.png"))?));
        assert_eq!(std::fs::read(dir.join("003.png"))?, real_png);

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    /// A multi-file JPEG set must convert correctly under the parallel `par_iter` path: the
    /// output set is the same regardless of file order, every output is a valid PNG, and the
    /// fast PNG encoder preserves the original pixels of each distinct source.
    #[test]
    fn parallel_conversion_is_order_independent_and_lossless()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_temp_dir("jpeg_parallel");
        std::fs::create_dir_all(&dir)?;

        // Several JPEGs with distinct solid colors, written in a deliberately shuffled order so a
        // correct parallel conversion cannot depend on enumeration order.
        let colors = [
            (5u8, [10u8, 200, 30]),
            (1u8, [255, 0, 0]),
            (4u8, [0, 0, 255]),
            (2u8, [0, 255, 0]),
            (3u8, [123, 45, 67]),
        ];
        for (n, rgb) in colors {
            let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                16,
                16,
                image::Rgb(rgb),
            ));
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Jpeg)?;
            std::fs::write(dir.join(format!("{n:03}.jpg")), buf.into_inner())?;
        }

        let converted = super::convert_jpegs_to_png(&dir)?;
        assert_eq!(converted, colors.len());

        // Every source produced exactly one valid PNG and the original `.jpg` is gone.
        for (n, rgb) in colors {
            let png_path = dir.join(format!("{n:03}.png"));
            assert!(!dir.join(format!("{n:03}.jpg")).exists());
            let png_bytes = std::fs::read(&png_path)?;
            assert!(is_png(&png_bytes), "output {n:03}.png is not a valid PNG");
            // JPEG is lossy, but a 16x16 solid block decodes back to its (near-)constant color;
            // require the center pixel to match the source within a small JPEG tolerance.
            let decoded = image::load_from_memory(&png_bytes)?.to_rgb8();
            let center = decoded.get_pixel(8, 8).0;
            for ch in 0..3 {
                let diff = i32::from(center[ch]) - i32::from(rgb[ch]);
                assert!(
                    diff.abs() <= 6,
                    "channel {ch} of {n:03}.png drifted too far: {center:?} vs {rgb:?}"
                );
            }
        }

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    /// Happy path for the parallel `ensure_saved` core: a mix of PNG and JPEG sources must each
    /// yield exactly one valid, non-empty PNG in `cleaned/`, with the correct total count.
    #[test]
    fn convert_src_to_cleaned_writes_valid_pngs_for_mixed_sources()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("ensure_saved_ok");
        let src = root.join("src");
        let cleaned = root.join("cleaned");
        std::fs::create_dir_all(&src)?;

        // Two genuine PNG sources and two JPEG sources.
        std::fs::write(src.join("001.png"), png_bytes()?)?;
        std::fs::write(src.join("002.png"), png_bytes()?)?;
        std::fs::write(src.join("003.jpg"), jpeg_bytes()?)?;
        std::fs::write(src.join("004.jpg"), jpeg_bytes()?)?;

        super::convert_src_to_cleaned(&src, &cleaned)?;

        // Exactly four outputs, all valid decodable PNGs, none left as a 0-byte placeholder.
        let mut outputs: Vec<std::path::PathBuf> = std::fs::read_dir(&cleaned)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        outputs.sort();
        assert_eq!(
            outputs.len(),
            4,
            "unexpected cleaned output set: {outputs:?}"
        );
        for path in &outputs {
            let bytes = std::fs::read(path)?;
            assert!(!bytes.is_empty(), "{} is a 0-byte file", path.display());
            assert!(is_png(&bytes), "{} is not a valid PNG", path.display());
            // Must be fully decodable, not just PNG-magic.
            image::load_from_memory(&bytes)?;
        }

        std::fs::remove_dir_all(&root).ok();
        Ok(())
    }

    /// Exercises bug A1's fix: a source with a real image extension but corrupt content makes
    /// `image::open` fail, which must remove the reserved placeholder instead of leaving a 0-byte
    /// PNG that would permanently disable the bootstrap guard. The whole call propagates the decode
    /// error, and no 0-byte file is left behind for the failed source.
    #[test]
    fn convert_src_to_cleaned_removes_placeholder_on_decode_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = unique_temp_dir("ensure_saved_fail");
        let src = root.join("src");
        let cleaned = root.join("cleaned");
        std::fs::create_dir_all(&src)?;

        // A `.jpg`-named file whose bytes are not a decodable image: `image::open` returns Err,
        // hitting the skip arm, which must delete the reserved placeholder.
        std::fs::write(src.join("bad.jpg"), b"not an image at all")?;
        // A valid neighbor to confirm good outputs still appear.
        std::fs::write(src.join("good.png"), png_bytes()?)?;

        super::convert_src_to_cleaned(&src, &cleaned)?;

        // The corrupt source leaves no output at all (placeholder removed); no 0-byte file exists.
        assert!(
            !cleaned.join("bad.png").exists(),
            "placeholder for the failed source was left behind"
        );
        for entry in std::fs::read_dir(&cleaned)? {
            let path = entry?.path();
            let len = std::fs::metadata(&path)?.len();
            assert!(
                len > 0,
                "0-byte placeholder left behind: {}",
                path.display()
            );
        }
        // The valid neighbor still produced a real PNG.
        let good = std::fs::read(cleaned.join("good.png"))?;
        assert!(is_png(&good) && !good.is_empty());

        std::fs::remove_dir_all(&root).ok();
        Ok(())
    }

    /// `unique_png_path` returns the bare `<stem>.png` when free and a suffixed name when taken.
    #[test]
    fn unique_png_path_resolves_collisions() -> Result<(), Box<dyn std::error::Error>> {
        let dir = unique_temp_dir("unique_png");
        std::fs::create_dir_all(&dir)?;

        assert_eq!(super::unique_png_path(&dir, "001")?, dir.join("001.png"));
        std::fs::write(dir.join("001.png"), b"x")?;
        assert_eq!(super::unique_png_path(&dir, "001")?, dir.join("001-1.png"));
        std::fs::write(dir.join("001-1.png"), b"x")?;
        assert_eq!(super::unique_png_path(&dir, "001")?, dir.join("001-2.png"));

        std::fs::remove_dir_all(&dir).ok();
        Ok(())
    }
}
