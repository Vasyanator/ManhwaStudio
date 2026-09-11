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
            ModelCategory::MangaBwUpscale => t!("launcher.new_project.reline_models.category_manga_bw_upscale"),
            ModelCategory::MangaRestore => t!("launcher.new_project.reline_models.category_manga_bw_restore"),
            ModelCategory::Descreen => t!("launcher.new_project.reline_models.category_descreen"),
            ModelCategory::Halftone => t!("launcher.new_project.reline_models.category_halftone"),
            ModelCategory::IllustrationColor => t!("launcher.new_project.reline_models.category_color_illustration"),
            ModelCategory::AnimeColor => t!("launcher.new_project.reline_models.category_anime_color"),
            ModelCategory::Photo => t!("launcher.new_project.reline_models.category_photo"),
            ModelCategory::Generic => t!("launcher.new_project.reline_models.category_light_upscalers"),
            ModelCategory::Other => t!("launcher.new_project.reline_models.category_other"),
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
            Some(1) => tf!("launcher.new_project.reline_models.suffix_no_upscale", arg = self.title),
            Some(factor) => tf!("launcher.new_project.reline_models.suffix_upscale", arg = self.title, factor = factor),
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
///
/// `title`, `description`, and `recommendation` hold i18n catalog KEYS (not the text),
/// because `t!` is not `const` and this table is a `static`. They are resolved to the
/// active-locale label in `classify` via [`resolve_key`]. This mirrors the
/// `(wire_code, display_key)` pattern in `tabs/translation/panels/ocr_langs.rs`.
struct CuratedEntry {
    kind: MatchKind,
    /// Lowercased lookup key.
    key: &'static str,
    category: ModelCategory,
    title: &'static str,
    description: &'static str,
    recommendation: Option<&'static str>,
}

/// Resolves an i18n catalog key to its localized label, falling back to the key on a
/// catalog miss. Runtime (not `const`) because `t!` is not `const`.
fn resolve_key(key: &'static str) -> String {
    ms_i18n::lookup(key).unwrap_or(key).to_string()
}

// Order matters: the first matching entry wins, so more specific keys come before broader ones.
static CURATED: &[CuratedEntry] = &[
    // --- Descreen / dehalftone (very specific, must precede generic manga rules) ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "descreen",
        category: ModelCategory::Descreen,
        title: "launcher.new_project.reline_models.descreen_title",
        description: "launcher.new_project.reline_models.descreen_desc",
        recommendation: Some("launcher.new_project.reline_models.descreen_hint"),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "dehalfton",
        category: ModelCategory::Descreen,
        title: "launcher.new_project.reline_models.dehalftone_title",
        description: "launcher.new_project.reline_models.dehalftone_desc",
        recommendation: Some("launcher.new_project.reline_models.dehalftone_hint"),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "halftone",
        category: ModelCategory::Halftone,
        title: "launcher.new_project.reline_models.halftone_title",
        description: "launcher.new_project.reline_models.halftone_desc",
        recommendation: None,
    },
    // --- Manga B/W restoration (1x de-JPEG / decompress) ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mangajpeg",
        category: ModelCategory::MangaRestore,
        title: "launcher.new_project.reline_models.dejpeg_manga_title",
        description: "launcher.new_project.reline_models.dejpeg_manga_desc",
        recommendation: Some("launcher.new_project.reline_models.dejpeg_manga_hint"),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "dejpeg",
        category: ModelCategory::MangaRestore,
        title: "launcher.new_project.reline_models.dejpeg_title",
        description: "launcher.new_project.reline_models.dejpeg_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "decompress",
        category: ModelCategory::MangaRestore,
        title: "launcher.new_project.reline_models.decompress_title",
        description: "launcher.new_project.reline_models.decompress_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "wtp_mr",
        category: ModelCategory::MangaRestore,
        title: "launcher.new_project.reline_models.wtp_restore_title",
        description: "launcher.new_project.reline_models.wtp_restore_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "unimr",
        category: ModelCategory::MangaRestore,
        title: "launcher.new_project.reline_models.wtp_universal_title",
        description: "launcher.new_project.reline_models.wtp_universal_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "dwtp",
        category: ModelCategory::MangaRestore,
        title: "launcher.new_project.reline_models.dwtp_title",
        description: "launcher.new_project.reline_models.dwtp_desc",
        recommendation: Some("launcher.new_project.reline_models.dwtp_hint"),
    },
    // --- Manga B/W upscaling ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mangajanai",
        category: ModelCategory::MangaBwUpscale,
        title: "launcher.new_project.reline_models.mangajanai_title",
        description: "launcher.new_project.reline_models.mangajanai_desc",
        recommendation: Some("launcher.new_project.reline_models.mangajanai_hint"),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mangascale",
        category: ModelCategory::MangaBwUpscale,
        title: "launcher.new_project.reline_models.mangascale_title",
        description: "launcher.new_project.reline_models.mangascale_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mangasoup",
        category: ModelCategory::MangaBwUpscale,
        title: "launcher.new_project.reline_models.mangasoup_title",
        description: "launcher.new_project.reline_models.mangasoup_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "wtp_ms",
        category: ModelCategory::MangaBwUpscale,
        title: "launcher.new_project.reline_models.wtp_scale_title",
        description: "launcher.new_project.reline_models.wtp_scale_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mcover",
        category: ModelCategory::MangaBwUpscale,
        title: "launcher.new_project.reline_models.wtp_covers_title",
        description: "launcher.new_project.reline_models.wtp_covers_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "digimanga",
        category: ModelCategory::MangaBwUpscale,
        title: "launcher.new_project.reline_models.digital_bw_title",
        description: "launcher.new_project.reline_models.digital_bw_desc",
        recommendation: None,
    },
    // --- Color illustration / digital art ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "enhancr",
        category: ModelCategory::IllustrationColor,
        title: "launcher.new_project.reline_models.enhancr_title",
        description: "launcher.new_project.reline_models.enhancr_desc",
        recommendation: Some("launcher.new_project.reline_models.enhancr_hint"),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "illustrationjanai",
        category: ModelCategory::IllustrationColor,
        title: "launcher.new_project.reline_models.illustrationjanai_title",
        description: "launcher.new_project.reline_models.illustrationjanai_desc",
        recommendation: Some("launcher.new_project.reline_models.illustrationjanai_hint"),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "digital_art",
        category: ModelCategory::IllustrationColor,
        title: "launcher.new_project.reline_models.digital_art_title",
        description: "launcher.new_project.reline_models.digital_art_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "digitalart",
        category: ModelCategory::IllustrationColor,
        title: "launcher.new_project.reline_models.digital_art_title",
        description: "launcher.new_project.reline_models.digital_art_desc2",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "illust",
        category: ModelCategory::IllustrationColor,
        title: "launcher.new_project.reline_models.illustrations_title",
        description: "launcher.new_project.reline_models.illustrations_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "comics",
        category: ModelCategory::IllustrationColor,
        title: "launcher.new_project.reline_models.comics_title",
        description: "launcher.new_project.reline_models.comics_desc",
        recommendation: None,
    },
    // --- Anime (color) ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "anisd",
        category: ModelCategory::AnimeColor,
        title: "launcher.new_project.reline_models.anisd_title",
        description: "launcher.new_project.reline_models.anisd_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "animesharp",
        category: ModelCategory::AnimeColor,
        title: "AnimeSharp",
        description: "launcher.new_project.reline_models.anime_sharp_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "mahou",
        category: ModelCategory::AnimeColor,
        title: "launcher.new_project.reline_models.mahou_title",
        description: "launcher.new_project.reline_models.mahou_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "anime",
        category: ModelCategory::AnimeColor,
        title: "launcher.new_project.reline_models.anime_title",
        description: "launcher.new_project.reline_models.anime_desc",
        recommendation: None,
    },
    // --- Photo / general ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "purephoto",
        category: ModelCategory::Photo,
        title: "launcher.new_project.reline_models.purephoto_title",
        description: "launcher.new_project.reline_models.purephoto_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "realwebphoto",
        category: ModelCategory::Photo,
        title: "launcher.new_project.reline_models.realwebphoto_title",
        description: "launcher.new_project.reline_models.realwebphoto_desc",
        recommendation: Some("launcher.new_project.reline_models.realwebphoto_hint"),
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "ultramix",
        category: ModelCategory::Photo,
        title: "launcher.new_project.reline_models.ultramix_title",
        description: "launcher.new_project.reline_models.ultramix_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "realesrgan",
        category: ModelCategory::Photo,
        title: "launcher.new_project.reline_models.realesrgan_title",
        description: "launcher.new_project.reline_models.realesrgan_desc",
        recommendation: None,
    },
    // --- Generic lightweight ---
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "spanplus",
        category: ModelCategory::Generic,
        title: "launcher.new_project.reline_models.spanplus_title",
        description: "launcher.new_project.reline_models.spanplus_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "span_franken",
        category: ModelCategory::Generic,
        title: "launcher.new_project.reline_models.span_title",
        description: "launcher.new_project.reline_models.span_desc",
        recommendation: None,
    },
    CuratedEntry {
        kind: MatchKind::Contains,
        key: "span_v2",
        category: ModelCategory::Generic,
        title: "SPAN v2 (beta)",
        description: "launcher.new_project.reline_models.span_v2_desc",
        recommendation: None,
    },
    // --- Raw / unnamed checkpoints ---
    CuratedEntry {
        kind: MatchKind::Prefix,
        key: "net_g",
        category: ModelCategory::Other,
        title: "launcher.new_project.reline_models.unnamed_title",
        description: "launcher.new_project.reline_models.unnamed_desc",
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
                title: resolve_key(entry.title),
                description: resolve_key(entry.description),
                recommendation: entry.recommendation.map(resolve_key),
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
            t!("launcher.new_project.reline_models.restore_1x_desc").to_string(),
        ),
        Some(factor) => (ModelCategory::Generic, tf!("launcher.new_project.reline_models.upscale_factor_desc", factor = factor)),
        None => (
            ModelCategory::Other,
            t!("launcher.new_project.reline_models.unknown_purpose_desc").to_string(),
        ),
    };

    let description = match arch {
        Some(note) => format!("{base} {note}"),
        None => base,
    };

    ModelMeta {
        category,
        title: t!("launcher.new_project.reline_model_from_catalog").to_string(),
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
            t!("launcher.new_project.reline_models.arch_transformer_desc"),
        );
    }
    if lowered.contains("esrgan") {
        return Some(t!("launcher.new_project.reline_models.arch_esrgan_desc"));
    }
    if lowered.contains("cugan") {
        return Some(t!("launcher.new_project.reline_models.arch_cugan_desc"));
    }
    if MODERN.iter().any(|token| lowered.contains(token)) {
        return Some(t!("launcher.new_project.reline_models.arch_modern_desc"));
    }
    if FAST.iter().any(|token| lowered.contains(token)) {
        return Some(t!("launcher.new_project.reline_models.arch_light_desc"));
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
        // Pin the exact catalog keys the description is composed from (upscale-factor
        // note + heavy-architecture note), so a key remap fails here.
        assert_eq!(
            meta.description,
            format!(
                "{} {}",
                tf!("launcher.new_project.reline_models.upscale_factor_desc", factor = 4u32),
                t!("launcher.new_project.reline_models.arch_transformer_desc"),
            ),
        );
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
        assert_eq!(
            meta.display_title(),
            tf!("launcher.new_project.reline_models.suffix_upscale", arg = meta.title, factor = 2u32),
        );
        assert!(meta.recommendation.is_some());
    }

    #[test]
    fn scale_suffix_in_display_title() {
        // `display_title` pins the suffix catalog keys, so a key remap fails here.
        let m1 = classify("1x-MangaJPEGLQ");
        assert_eq!(m1.scale, Some(1));
        assert_eq!(
            m1.display_title(),
            tf!("launcher.new_project.reline_models.suffix_no_upscale", arg = m1.title),
        );
        let m4 = classify("4x_MangaJaNai_V1RC34_ESRGAN_760k");
        assert_eq!(m4.scale, Some(4));
        assert_eq!(
            m4.display_title(),
            tf!("launcher.new_project.reline_models.suffix_upscale", arg = m4.title, factor = 4u32),
        );
        // Embedded scale token without a leading prefix.
        assert_eq!(classify("RealESRGAN_x4plus").scale, Some(4));
        assert_eq!(classify("2x_spanplus").scale, Some(2));
        // No recognizable scale -> no suffix (title returned verbatim).
        let mn = classify("net_g_20000");
        assert_eq!(mn.scale, None);
        assert_eq!(mn.display_title(), mn.title);
    }

    #[test]
    fn every_category_has_title() {
        for category in ModelCategory::ALL {
            assert!(!category.title().is_empty());
        }
    }
}
