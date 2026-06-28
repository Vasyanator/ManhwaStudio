/*
File: src/launcher/new_project/reline_models.rs

Purpose:
Curated, offline classification of Reline super-resolution / restoration models for the
simplified Reline UI in the New Project window.

Main responsibilities:
- map a remote-catalog model name to a user-facing category, friendly title, capability
  description, and optional recommendation;
- group models into a small, ordered set of categories for the model gallery;
- fall back to a name-derived heuristic so brand-new catalog models still get a sensible card.

Key structures:
- ModelCategory: fixed gallery groups with stable ordering and titles.
- ModelMeta: the per-model presentation payload shown by the gallery.

Key functions:
- classify(): resolve a model name into ModelMeta (curated table first, heuristic fallback).

Notes:
This module is pure (no UI, no network). The model catalog itself is dynamic and fetched by
`reline.rs`; this module only annotates names. Descriptions are intentionally family-level so
they stay maintainable as the remote catalog grows.
*/

/// Gallery group a Reline model belongs to.
///
/// The set is fixed and ordered: `order()` controls the top-to-bottom group order in the
/// simplified model gallery, `title()` is the group header. No catch-all arm is used so adding
/// a variant forces every match site to be revisited (project clippy rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCategory {
    /// Black-and-white manga, upscaling (2x/4x line + screentone reconstruction).
    MangaBwUpscale,
    /// Black-and-white manga restoration without upscaling (de-JPEG / decompress).
    MangaRestore,
    /// Remove printed halftone screen (descreen / dehalftone).
    Descreen,
    /// Models built around halftone / screentone synthesis.
    Halftone,
    /// Color illustrations / digital art.
    IllustrationColor,
    /// Anime-style color sources.
    AnimeColor,
    /// Photos and general "real world" sources.
    Photo,
    /// Generic lightweight upscalers without a strong domain specialization.
    Generic,
    /// Anything that could not be classified (raw checkpoints, unknown names).
    Other,
}

impl ModelCategory {
    /// All categories in gallery order. Keep in sync with `order()`.
    pub const ALL: [ModelCategory; 9] = [
        ModelCategory::MangaBwUpscale,
        ModelCategory::MangaRestore,
        ModelCategory::Descreen,
        ModelCategory::Halftone,
        ModelCategory::IllustrationColor,
        ModelCategory::AnimeColor,
        ModelCategory::Photo,
        ModelCategory::Generic,
        ModelCategory::Other,
    ];

    /// Stable sort key for gallery group order (lower shows first).
    #[must_use]
    pub fn order(self) -> u8 {
        match self {
            ModelCategory::MangaBwUpscale => 0,
            ModelCategory::MangaRestore => 1,
            ModelCategory::Descreen => 2,
            ModelCategory::Halftone => 3,
            ModelCategory::IllustrationColor => 4,
            ModelCategory::AnimeColor => 5,
            ModelCategory::Photo => 6,
            ModelCategory::Generic => 7,
            ModelCategory::Other => 8,
        }
    }

    /// Human-readable group header for the gallery.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            ModelCategory::MangaBwUpscale => "Манга Ч/Б · увеличение",
            ModelCategory::MangaRestore => "Манга Ч/Б · реставрация без увеличения",
            ModelCategory::Descreen => "Убрать печатный растр (дескрин)",
            ModelCategory::Halftone => "Полутон / скринтон",
            ModelCategory::IllustrationColor => "Цветные иллюстрации / диджитал-арт",
            ModelCategory::AnimeColor => "Аниме (цвет)",
            ModelCategory::Photo => "Фото / универсальные",
            ModelCategory::Generic => "Лёгкие универсальные апскейлеры",
            ModelCategory::Other => "Прочие / без классификации",
        }
    }
}

/// Presentation payload for a single model in the gallery.
///
/// `title` is a short friendly label, `description` explains capabilities, `recommendation`
/// (when present) is a one-line "when to use this" hint shown in an accent color.
#[derive(Debug, Clone)]
pub struct ModelMeta {
    pub category: ModelCategory,
    pub title: String,
    pub description: String,
    pub recommendation: Option<String>,
    /// Upscale factor derived from the model name (`1` = restoration only). `None` when the name
    /// has no recognizable scale token.
    pub scale: Option<u32>,
}

impl ModelMeta {
    /// Friendly title with an explicit scale suffix, e.g. `… (без апскейла)` / `… (апскейл 2x)`.
    ///
    /// Returns the bare title when the scale could not be determined.
    #[must_use]
    pub fn display_title(&self) -> String {
        match self.scale {
            Some(1) => format!("{} (без апскейла)", self.title),
            Some(factor) => format!("{} (апскейл {factor}x)", self.title),
            None => self.title.clone(),
        }
    }
}

/// How a curated key is matched against the (lowercased) model name.
#[derive(Debug, Clone, Copy)]
enum MatchKind {
    Prefix,
    Contains,
}

/// One curated family rule. Family-level by design: one entry annotates many catalog models.
struct CuratedEntry {
    kind: MatchKind,
    /// Lowercased lookup key.
    key: &'static str,
    category: ModelCategory,
    title: &'static str,
    description: &'static str,
    recommendation: Option<&'static str>,
}

// Order matters: the first matching entry wins, so more specific keys come before broader ones.
static CURATED: &[CuratedEntry] = &[
    // --- Descreen / dehalftone (very specific, must precede generic manga rules) ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "descreen",
        category: ModelCategory::Descreen,
        title: "Дескрин печатного скана",
        description: "Убирает растровую сетку (муар) с отсканированных печатных страниц и одновременно увеличивает изображение.",
        recommendation: Some("Для сканов бумажных журналов с видимыми точками растра."),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "dehalfton",
        category: ModelCategory::Descreen,
        title: "Удаление полутонового растра",
        description: "Сглаживает полутоновые точки (dehalftone) и восстанавливает чистые тона, с увеличением.",
        recommendation: Some("Когда нужно превратить точечный растр в гладкую заливку."),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "halftone",
        category: ModelCategory::Halftone,
        title: "Полутон / скринтон",
        description: "Модель, работающая с полутоновым растром (скринтоном) — синтез или коррекция точечной сетки.",
        recommendation: None,
    },
    // --- Manga B/W restoration (1x de-JPEG / decompress) ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mangajpeg",
        category: ModelCategory::MangaRestore,
        title: "Деджейпег манги",
        description: "Убирает JPEG-артефакты и звон на Ч/Б манге без изменения размера. Варианты LQ/MQ/HQ рассчитаны на разную степень сжатия исходника (LQ — сильные артефакты, HQ — лёгкие).",
        recommendation: Some("Подбирайте LQ/MQ/HQ под качество исходника."),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "dejpeg",
        category: ModelCategory::MangaRestore,
        title: "Снятие JPEG-сжатия",
        description: "Реставрирует изображение, пострадавшее от JPEG-компрессии.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "decompress",
        category: ModelCategory::MangaRestore,
        title: "Декомпрессия артефактов",
        description: "Восстанавливает детали после агрессивного сжатия (decompress).",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "wtp_mr",
        category: ModelCategory::MangaRestore,
        title: "WTP · реставрация манги",
        description: "Серия WTP manga-restore: чистка и восстановление Ч/Б манги (линия, тон) без увеличения.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "unimr",
        category: ModelCategory::MangaRestore,
        title: "WTP · универсальная реставрация",
        description: "Универсальный WTP-реставратор Ч/Б манги.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "dwtp",
        category: ModelCategory::MangaRestore,
        title: "DWTP · реставрация манги",
        description: "Мощная серия DWTP: чистка датасета манги (de-screen/downsample), снятие артефактов и увеличение. Тяжёлые варианты (ATD/DAT2) дают лучшее качество, но медленнее.",
        recommendation: Some("Сильная реставрация; тяжёлые архитектуры любят GPU."),
    },
    // --- Manga B/W upscaling ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mangajanai",
        category: ModelCategory::MangaBwUpscale,
        title: "MangaJaNai · апскейл Ч/Б манги",
        description: "Флагман для Ч/Б манги: восстанавливает линию и скринтон, давит JPEG-артефакты, увеличивает ×4. Варианты 1200p…2048p подобраны под исходную высоту страницы.",
        recommendation: Some("Лучший выбор для Ч/Б сканов; берите вариант под высоту страницы."),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mangascale",
        category: ModelCategory::MangaBwUpscale,
        title: "MangaScale · апскейл манги",
        description: "Увеличение Ч/Б манги с сохранением чёткой линии.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mangasoup",
        category: ModelCategory::MangaBwUpscale,
        title: "MangaSoup · апскейл манги",
        description: "Апскейл манги на основе CUGAN.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "wtp_ms",
        category: ModelCategory::MangaBwUpscale,
        title: "WTP · апскейл манги",
        description: "Серия WTP manga-scale: увеличение Ч/Б манги.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mcover",
        category: ModelCategory::MangaBwUpscale,
        title: "WTP · обложки",
        description: "Апскейл обложек манги (CUGAN).",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "digimanga",
        category: ModelCategory::MangaBwUpscale,
        title: "Цифровая Ч/Б манга",
        description: "Реставрация и увеличение цифровой Ч/Б манги.",
        recommendation: None,
    },
    // --- Color illustration / digital art ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "enhancr",
        category: ModelCategory::IllustrationColor,
        title: "enhancr · быстрая реставрация арта",
        description: "Реставрация цветной манхвы и диджитал-арта на архитектуре SMoSR (автор Umzi). Очень быстрая: ≈0.5 c против ~1.5 мин у тяжёлых DAT2 на RTX 3060. Чуть шумнее и артефактнее, чем FIGSR, но в разы быстрее. Требует resselt ≥ 1.4.0.",
        recommendation: Some("Когда нужна быстрая реставрация цветного арта/манхвы."),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "illustrationjanai",
        category: ModelCategory::IllustrationColor,
        title: "IllustrationJaNai · цветной арт",
        description: "Флагман для цветных иллюстраций и диджитал-арта: максимум деталей (DAT2) или быстрее (ESRGAN).",
        recommendation: Some("Лучший выбор для цветных иллюстраций и вебтунов."),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "digital_art",
        category: ModelCategory::IllustrationColor,
        title: "Диджитал-арт",
        description: "Увеличение цветного диджитал-арта. Суффиксы _t/_l — tiny/large (скорость против качества).",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "digitalart",
        category: ModelCategory::IllustrationColor,
        title: "Диджитал-арт",
        description: "Увеличение цветного диджитал-арта.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "illust",
        category: ModelCategory::IllustrationColor,
        title: "Иллюстрации",
        description: "Реставрация/увеличение цветных иллюстраций.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "comics",
        category: ModelCategory::IllustrationColor,
        title: "Комиксы",
        description: "Деджейпег и восстановление цветных комиксов.",
        recommendation: None,
    },
    // --- Anime (color) ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "anisd",
        category: ModelCategory::AnimeColor,
        title: "AniSD · аниме",
        description: "Реставрация/увеличение аниме-кадров (на быстрой архитектуре SPAN).",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "animesharp",
        category: ModelCategory::AnimeColor,
        title: "AnimeSharp",
        description: "Резкий апскейл аниме ×4.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mahou",
        category: ModelCategory::AnimeColor,
        title: "Mahou · аниме",
        description: "Апскейл аниме ×2 на основе CUGAN.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "anime",
        category: ModelCategory::AnimeColor,
        title: "Аниме",
        description: "Апскейл аниме-стиля.",
        recommendation: None,
    },
    // --- Photo / general ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "purephoto",
        category: ModelCategory::Photo,
        title: "PurePhoto · фото",
        description: "Увеличение фотографий (span — быстрее, compact — очень быстро).",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "realwebphoto",
        category: ModelCategory::Photo,
        title: "RealWebPhoto · веб-фото",
        description: "Восстановление «грязных» веб-фото с артефактами (ATD/DRCT-L — тяжёлые, лучшее восстановление).",
        recommendation: Some("Для сжатых фото из интернета."),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "ultramix",
        category: ModelCategory::Photo,
        title: "UltraMix · универсал",
        description: "Универсальные миксы: Balanced (баланс), Restore (упор на реставрацию), Smooth (сглаживание).",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "realesrgan",
        category: ModelCategory::Photo,
        title: "RealESRGAN · классика",
        description: "Классический универсальный апскейлер ×4. Вариант anime — для аниме/рисунка.",
        recommendation: None,
    },
    // --- Generic lightweight ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "spanplus",
        category: ModelCategory::Generic,
        title: "SPAN+ · лёгкий апскейлер",
        description: "Быстрый лёгкий апскейлер общего назначения. Суффиксы _s/_st — small/small-tiny.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "span_franken",
        category: ModelCategory::Generic,
        title: "SPAN · сборный",
        description: "Экспериментальный сборный SPAN-апскейлер.",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "span_v2",
        category: ModelCategory::Generic,
        title: "SPAN v2 (beta)",
        description: "Бета лёгкого апскейлера SPAN v2.",
        recommendation: None,
    },
    // --- Raw / unnamed checkpoints ---
    CuratedEntry {
        kind: MatchKind::Prefix,
        key: "net_g",
        category: ModelCategory::Other,
        title: "Безымянный чекпоинт",
        description: "Сырые тренировочные веса (имя = только номер итерации). Назначение по имени определить нельзя — используйте на свой риск.",
        recommendation: None,
    },
];

/// Resolve a catalog model name into presentation metadata.
///
/// Tries the curated family table first (first match wins, specific keys are ordered before
/// broad ones), then falls back to `derive_from_name` so unknown/new models still get a card.
#[must_use]
pub fn classify(name: &str) -> ModelMeta {
    let lowered = name.to_lowercase();
    let scale = detect_scale(&lowered);
    for entry in CURATED {
        let matches = match entry.kind {
            MatchKind::Prefix => lowered.starts_with(entry.key),
            MatchKind::Contains => lowered.contains(entry.key),
        };
        if matches {
            return ModelMeta {
                category: entry.category,
                title: entry.title.to_string(),
                description: entry.description.to_string(),
                recommendation: entry.recommendation.map(str::to_string),
                scale,
            };
        }
    }
    derive_from_name(&lowered, scale)
}

/// Heuristic fallback: build a card from the scale prefix and architecture token in the name.
///
/// Restoration (`1x`) vs upscaling (`2x`/`4x`/…) sets the category between `Generic` and
/// `Other`; the architecture token drives the speed/quality note in the description.
fn derive_from_name(lowered: &str, scale: Option<u32>) -> ModelMeta {
    let arch = arch_note(lowered);

    let (category, base) = match scale {
        Some(1) => (
            ModelCategory::Other,
            "Реставрация без увеличения (1x): чистка/снятие артефактов.".to_string(),
        ),
        Some(factor) => (ModelCategory::Generic, format!("Апскейлер ×{factor}.")),
        None => (
            ModelCategory::Other,
            "Назначение по имени определить не удалось.".to_string(),
        ),
    };

    let description = match arch {
        Some(note) => format!("{base} {note}"),
        None => base,
    };

    ModelMeta {
        category,
        title: "Модель из каталога".to_string(),
        description,
        recommendation: None,
        scale,
    }
}

/// Determine the upscale factor of a model from its (lowercased) name.
///
/// Prefers a leading scale prefix (`4x_…`, `1x-…`); otherwise looks for an embedded `x4`/`4x`
/// token (e.g. `realesrgan_x4plus`). Returns `None` when no scale token is recognizable.
fn detect_scale(lowered: &str) -> Option<u32> {
    if let Some(factor) = leading_scale(lowered) {
        return Some(factor);
    }
    // Common embedded forms in community model names; check both `Nx` and `xN`.
    for factor in [1u32, 2, 4, 8] {
        if lowered.contains(&format!("{factor}x")) || lowered.contains(&format!("x{factor}")) {
            return Some(factor);
        }
    }
    None
}

/// Parse a leading scale factor like `1x`, `2x`, `4x` (case-insensitive, name already lowered).
///
/// Accepts both `4x_name` and `4x-name`. Returns `None` when there is no numeric scale prefix.
fn leading_scale(lowered: &str) -> Option<u32> {
    let digits: String = lowered.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &lowered[digits.len()..];
    if !rest.starts_with('x') {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Map a known architecture token in the name to a short speed/quality note.
fn arch_note(lowered: &str) -> Option<&'static str> {
    const HEAVY: [&str; 6] = ["dat2", "atd", "drct", "hat", "rgt", "swinir"];
    const FAST: [&str; 3] = ["compact", "span", "mosr"];
    const MODERN: [&str; 6] = ["rplksr", "plksr", "moesr", "flexnet", "gfisr", "gater"];

    if HEAVY.iter().any(|token| lowered.contains(token)) {
        return Some(
            "Тяжёлая трансформерная архитектура — максимум качества, медленно, лучше на GPU.",
        );
    }
    if lowered.contains("esrgan") {
        return Some("Классическая архитектура ESRGAN — резкий результат, средняя скорость.");
    }
    if lowered.contains("cugan") {
        return Some("Архитектура CUGAN — заточена под аниме/рисунок.");
    }
    if MODERN.iter().any(|token| lowered.contains(token)) {
        return Some("Современная архитектура — хороший баланс качества и скорости.");
    }
    if FAST.iter().any(|token| lowered.contains(token)) {
        return Some("Лёгкая быстрая архитектура — подходит для слабых ПК/CPU.");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_manga_bw_upscale() {
        let meta = classify("4x_MangaJaNai_1200p_V1RC71_ESRGAN_70k");
        assert_eq!(meta.category, ModelCategory::MangaBwUpscale);
        assert!(meta.recommendation.is_some());
    }

    #[test]
    fn curated_manga_restore() {
        assert_eq!(
            classify("1x-MangaJPEGLQ").category,
            ModelCategory::MangaRestore
        );
        assert_eq!(
            classify("1x_umzi_digital_decompress_gaterv3_1").category,
            ModelCategory::MangaRestore
        );
    }

    #[test]
    fn curated_descreen_precedes_manga() {
        // descreen / dehalfton must win over broader manga rules
        assert_eq!(
            classify("4x_DWTP_descreenon_dat2").category,
            ModelCategory::Descreen
        );
        assert_eq!(
            classify("4x_umzi_dehalfton_realplksr_v1").category,
            ModelCategory::Descreen
        );
    }

    #[test]
    fn curated_illustration_and_anime() {
        assert_eq!(
            classify("4x_IllustrationJaNai_V1_DAT2_190k").category,
            ModelCategory::IllustrationColor
        );
        assert_eq!(
            classify("4x-AnimeSharp").category,
            ModelCategory::AnimeColor
        );
        assert_eq!(
            classify("RealESRGAN_x4plus_anime_6B").category,
            ModelCategory::AnimeColor
        );
    }

    #[test]
    fn fallback_upscale_with_heavy_arch() {
        let meta = classify("4x_unknownfamily_dat2");
        assert_eq!(meta.category, ModelCategory::Generic);
        assert!(meta.description.contains("×4"));
        assert!(meta.description.to_lowercase().contains("gpu"));
    }

    #[test]
    fn fallback_restore_1x() {
        let meta = classify("1x_unknownfamily_v1");
        assert_eq!(meta.category, ModelCategory::Other);
        assert!(!meta.description.is_empty());
    }

    #[test]
    fn fallback_unknown_no_scale() {
        let meta = classify("net_g_20000");
        // net_g is curated as Other; verify the curated raw-checkpoint rule applies.
        assert_eq!(meta.category, ModelCategory::Other);
    }

    #[test]
    fn leading_scale_parsing() {
        assert_eq!(leading_scale("4x_name"), Some(4));
        assert_eq!(leading_scale("1x-name"), Some(1));
        assert_eq!(leading_scale("realesrgan_x4plus"), None);
        assert_eq!(leading_scale("span_v2_beta"), None);
    }

    #[test]
    fn curated_enhancr_smosr() {
        let meta = classify("2x_enhancr_da_smosr");
        assert_eq!(meta.category, ModelCategory::IllustrationColor);
        assert_eq!(meta.scale, Some(2));
        assert!(meta.display_title().contains("(апскейл 2x)"));
        assert!(meta.recommendation.is_some());
    }

    #[test]
    fn scale_suffix_in_display_title() {
        assert_eq!(classify("1x-MangaJPEGLQ").scale, Some(1));
        assert!(
            classify("1x-MangaJPEGLQ")
                .display_title()
                .contains("(без апскейла)")
        );
        assert_eq!(classify("4x_MangaJaNai_V1RC34_ESRGAN_760k").scale, Some(4));
        assert!(
            classify("4x_MangaJaNai_V1RC34_ESRGAN_760k")
                .display_title()
                .contains("(апскейл 4x)")
        );
        // Embedded scale token without a leading prefix.
        assert_eq!(classify("RealESRGAN_x4plus").scale, Some(4));
        assert_eq!(classify("2x_spanplus").scale, Some(2));
        // No recognizable scale -> no suffix.
        assert_eq!(classify("net_g_20000").scale, None);
        assert!(!classify("net_g_20000").display_title().contains("апскейл"));
    }

    #[test]
    fn every_category_has_title() {
        for category in ModelCategory::ALL {
            assert!(!category.title().is_empty());
        }
    }
}
