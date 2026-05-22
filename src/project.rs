/*
FILE OVERVIEW: src/project.rs
Project-level data loading and filesystem helpers.

Main items:
- Data models: `Bubble`, `Page`, `CanvasSettings`, `ProjectPaths`, `ProjectData`.
  `Bubble` stores per-bubble placement plus optional `bubble_type`
  (`default`/`aside`/`on_top`) for mixed layouts.
- `CanvasSettings` stores editable/readonly default bubble display types.
- `ProjectData::load`: discovers project/title paths, loads pages, bubbles and settings.
- `ProjectData::load`: also reconciles minor clean-overlay filename mismatches against `src/`
  page names (for example `1.png` -> `001.png`) before the overlay loader runs.
- `ProjectData::load_resume_unsaved`: like `load`, but reads bubbles/text-info from the
  `{chapter}_unsaved/` folder when present (crash-recovery mode).
- `ProjectPaths::unsaved_dir` and related fields: paths to the parallel `_unsaved` folder
  where all in-session mutations are staged before an explicit "save to project".
- Canvas settings parsing keeps project-scoped keys in `settings.json`, while selected
  canvas preferences can be sourced from global `user_config.json`.
- Utility helpers for image collection, directory bootstrap and recursive copies.
*/

use crate::bubble_status::{
    BubbleStatusRule, bubble_status_rules_from_value, default_bubble_status_rules,
};
use crate::config;
use crate::config::JsonConfig;
use crate::runtime_log;
use anyhow::{Context, Result};
use image::ImageFormat;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
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
            Self::Pages => "Страничный",
            Self::Ribbon => "Вебтун",
            Self::Custom => "Свой",
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
    pub text_images_dir: PathBuf,
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
    pub unsaved_text_images_dir: PathBuf,
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
        let project_dir = project_dir
            .canonicalize()
            .with_context(|| format!("project dir not found: {}", project_dir.display()))?;

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
        let clean_layers_dir = project_dir.join(config::CLEAN_LAYERS_DIR);
        let cleaned_dir = project_dir.join(config::CLEANED_DIR);
        let alt_vers_dir = project_dir.join(config::ALT_VERS_DIR);
        let saved_dir = project_dir.join(config::SAVED_DIR);
        let text_images_dir = project_dir.join(config::TEXT_IMAGES_DIR);
        let text_detection_dir = project_dir.join(config::TEXT_DETECTION_DIR);
        let characters_dir = title_dir.join(config::CHARACTERS_DIR);
        let terms_file = title_dir.join(config::TERMS_FILE);
        let settings_file = title_dir.join(config::PROJECT_SETTINGS_FILE);

        // Unsaved staging folder lives next to the chapter folder.
        let unsaved_dir = title_dir.join(format!("{chapter_name}_unsaved"));
        let unsaved_bubbles_file = unsaved_dir.join(config::BUBBLES_FILE);
        let unsaved_clean_layers_dir = unsaved_dir.join(config::CLEAN_LAYERS_DIR);
        let unsaved_text_images_dir = unsaved_dir.join(config::TEXT_IMAGES_DIR);

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
            text_images_dir,
            text_detection_dir,
            characters_dir,
            terms_file,
            settings_file: settings_file.clone(),
            unsaved_dir: unsaved_dir.clone(),
            unsaved_bubbles_file: unsaved_bubbles_file.clone(),
            unsaved_clean_layers_dir,
            unsaved_text_images_dir,
        };

        let pages = collect_images(&src_dir)?;
        reconcile_clean_overlay_names(&pages, &paths.clean_layers_dir)?;
        reconcile_clean_overlay_names(&pages, &paths.unsaved_clean_layers_dir)?;

        // In resume mode, prefer the unsaved bubbles file if it exists.
        let effective_bubbles_file = if resume_unsaved && unsaved_bubbles_file.exists() {
            &unsaved_bubbles_file
        } else {
            &bubbles_file
        };
        let bubbles = load_bubbles(effective_bubbles_file)?;

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
        self.project_dir.is_dir()
    }

    pub fn autosave_bubbles(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(self.bubbles.as_ref())
            .context("failed to serialize bubbles")?;
        fs::write(&self.paths.bubbles_file, raw).with_context(|| {
            format!(
                "failed to write bubbles file {}",
                self.paths.bubbles_file.display()
            )
        })?;
        Ok(())
    }

    pub fn ensure_saved(&self) -> Result<()> {
        fs::create_dir_all(&self.paths.cleaned_dir).with_context(|| {
            format!(
                "failed to create cleaned dir {}",
                self.paths.cleaned_dir.display()
            )
        })?;
        if has_any_entries(&self.paths.cleaned_dir)? {
            return Ok(());
        }

        for entry in fs::read_dir(&self.paths.src_dir)
            .with_context(|| format!("failed to read {}", self.paths.src_dir.display()))?
        {
            let entry = entry?;
            let src_path = entry.path();
            if !src_path.is_file() {
                continue;
            }

            let stem = src_path
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or("image");
            let ext = src_path
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase();

            let dst_path = unique_png_path(&self.paths.cleaned_dir, stem);
            if ext == "png" {
                fs::copy(&src_path, &dst_path).with_context(|| {
                    format!(
                        "failed to copy {} -> {}",
                        src_path.display(),
                        dst_path.display()
                    )
                })?;
                continue;
            }

            match image::open(&src_path) {
                Ok(img) => {
                    img.save_with_format(&dst_path, ImageFormat::Png)
                        .with_context(|| {
                            format!(
                                "failed to convert {} -> {}",
                                src_path.display(),
                                dst_path.display()
                            )
                        })?;
                }
                Err(_) => {
                    // Non-image files are skipped, mirroring Python behavior.
                    continue;
                }
            }
        }

        Ok(())
    }

    pub fn ensure_clean_layers_dir(&self) -> Result<()> {
        if has_any_entries(&self.paths.clean_layers_dir)? {
            return Ok(());
        }
        if !has_any_entries(&self.paths.cleaned_dir)? {
            return Ok(());
        }

        fs::create_dir_all(&self.paths.clean_layers_dir).with_context(|| {
            format!(
                "failed to create clean layers dir {}",
                self.paths.clean_layers_dir.display()
            )
        })?;

        copy_dir_recursive(&self.paths.cleaned_dir, &self.paths.clean_layers_dir)
    }

    pub fn ensure_translation_notes(&self) -> Result<()> {
        fs::create_dir_all(&self.paths.title_dir).with_context(|| {
            format!(
                "failed to create title dir {}",
                self.paths.title_dir.display()
            )
        })?;
        if self.paths.notes_file.exists() {
            return Ok(());
        }
        fs::write(&self.paths.notes_file, b"").with_context(|| {
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
    if src.is_dir() {
        return Ok(src);
    }

    let scr = project_dir.join("scr");
    if scr.is_dir() {
        fs::rename(&scr, &src).with_context(|| {
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

fn collect_images(dir: &Path) -> Result<Vec<Page>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("failed to read {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
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

fn reconcile_clean_overlay_names(pages: &[Page], overlay_dir: &Path) -> Result<()> {
    if !overlay_dir.is_dir() {
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
        if !desired_path.is_file() {
            pages_by_key
                .entry(match_key)
                .or_default()
                .push((page_idx, desired_stem, desired_path));
        }
    }

    let mut overlays_by_key: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for entry in fs::read_dir(overlay_dir)
        .with_context(|| format!("failed to read overlay directory {}", overlay_dir.display()))?
    {
        let path = entry?.path();
        if !path.is_file() || !is_png_path(&path) || exact_target_paths.contains(&path) {
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
        if desired_path.exists() {
            continue;
        }

        fs::rename(source_path, desired_path).with_context(|| {
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

fn load_bubbles(path: &Path) -> Result<Vec<Bubble>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read bubbles json: {}", path.display()))?;
    let bubbles: Vec<Bubble> = serde_json::from_str(&data)
        .with_context(|| format!("invalid bubbles json: {}", path.display()))?;
    Ok(bubbles)
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
    let mut root = if settings_file.exists() {
        match fs::read_to_string(settings_file) {
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
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(settings_file, payload).map_err(|err| err.to_string())?;
    Ok(())
}

#[allow(dead_code)]
fn has_any_entries(dir: &Path) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let mut iter =
        fs::read_dir(dir).with_context(|| format!("failed to read directory {}", dir.display()))?;
    Ok(iter.next().transpose()?.is_some())
}

#[allow(dead_code)]
fn unique_png_path(dst_dir: &Path, stem: &str) -> PathBuf {
    let base = dst_dir.join(format!("{stem}.png"));
    if !base.exists() {
        return base;
    }
    let mut i = 1usize;
    loop {
        let candidate = dst_dir.join(format!("{stem}-{i}.png"));
        if !candidate.exists() {
            return candidate;
        }
        i += 1;
    }
}

#[allow(dead_code)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("failed to create directory {}", dst.display()))?;

    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            match fs::copy(&src_path, &dst_path) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if let Some(parent) = dst_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&src_path, &dst_path)?;
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
    use super::overlay_name_match_key;

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
}
