/*
File: crates/ms-text-render/src/font_base.rs

Purpose:
The renderer's own, deterministic font base. It replaces the operating system's
font database with the bundled `fonts/ui` stack (`ms-fonts`) and pins the
cosmic-text fallback chain, so the same overlay renders identically on every
machine regardless of what fonts a user happens to have installed.

Main responsibilities:
- build the process-wide `fontdb::Database` of the bundled stack exactly once
  (`core`/`bold` from shared `'static` bytes, `ext` by PATH so its ~80 MB are
  mapped only when a glyph actually needs them);
- own `MsFallback`, the `cosmic_text::Fallback` implementation that turns the
  stack into an explicit script -> font chain instead of a brute-force scan;
- hand the pool a ready `FontSystem` built on that base (`new_render_font_system`);
- tell the loader which faces a resident bundled buffer ALREADY produced
  (`resident_face_ids`), so a caller that selects a bundled font as its own font
  reuses the registered face instead of adding a duplicate one.

Key structures:
- `MsFallback`: the cosmic-text fallback contract, backed by `FallbackTables`.
- `FallbackTables`: the resolved (manifest-filtered) common/forbidden/script lists.
- `RenderFontBase`: the built database plus its resident-buffer -> face-id index.

Key functions:
- `new_render_font_system`: the single constructor used by the pool.
- `base_database`: the shared, cloned-per-system `fontdb::Database`.
- `resident_face_ids`: face ids of an already-registered resident buffer.
- `build_base_database` / `build_fallback_tables`: the pure builders, unit-tested.

Notes:
Deliberate cost of dropping the system database (`dev-docs/unicode_base_font_plan.md`,
decision 1): a script that `fonts/ui` does not ship renders as tofu instead of being
covered by whatever the machine has installed. The fix is to extend `fonts/ui`, never
to reintroduce `FontSystem::new()`.
*/

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use cosmic_text::{Fallback, FontSystem, fontdb};
use ms_fonts::StackFont;
use ms_log::runtime_log;
use unicode_script::Script;

/// Locale handed to every render `FontSystem`.
///
/// Deliberately FIXED rather than read from the OS: the locale is the only other
/// machine-dependent input cosmic-text takes (it is forwarded to
/// `Fallback::script_fallback` and printed in its diagnostics), and a render must
/// not depend on the operator's regional settings. `MsFallback` ignores the
/// argument entirely, so this constant has no effect on glyph selection; it exists
/// so nothing can silently start depending on `sys_locale`.
const RENDER_LOCALE: &str = "en-US";

/// Bundled fonts that must never be reached by cosmic-text's final "try every face
/// in the database" pass (`cosmic-text-0.14.2/src/font/fallback/mod.rs:445-457`).
///
/// These four are the rare-CJK planes and weigh ~73 MB together. Because that final
/// pass calls `get_font` — and therefore mmaps — every candidate in turn, a single
/// unmapped codepoint would pull all of them into the address space. Listing them as
/// forbidden leaves them reachable ONLY through [`SCRIPT_FAMILIES`], i.e. for the
/// writing systems they actually serve — which is why every writing system whose
/// bundled coverage is exclusive to one of them needs an entry there
/// (pinned by `every_forbidden_only_codepoint_is_reachable_through_a_script_chain`).
const FORBIDDEN_FAMILIES: &[&str] = &[
    "Plangothic P1",
    "Plangothic P2",
    "HanaMinB",
    "HanaMinA",
];

/// Han-ideograph chain: the core CJK face first, then the rare-plane fonts in
/// increasing size/rarity. Everything after `Source Han Sans K` is forbidden for the
/// brute-force pass, so this table is the only way those fonts are ever mapped.
const HAN_CHAIN: &[&str] = &[
    "Source Han Sans K",
    "Plangothic P1",
    "Plangothic P2",
    "HanaMinB",
    "HanaMinA",
];

/// Hiragana chain: as [`HAN_CHAIN`], but with the purpose-built hentaigana face
/// inserted before the rare-plane fonts. Historic kana live in the Kana Supplement /
/// Kana Extended-A blocks, which `Source Han Sans K` does not cover; reaching them
/// through the 443 KB hentaigana font instead of a 19 MB rare-plane font is the
/// whole point of an explicit chain.
const HIRAGANA_CHAIN: &[&str] = &[
    "Source Han Sans K",
    "Noto Serif Hentaigana",
    "Plangothic P1",
    "Plangothic P2",
    "HanaMinB",
    "HanaMinA",
];

/// Korean chain. Hangul is fully covered by the core CJK face, so no rare-plane
/// font is consulted; hanja mixed into Korean text is caught by the Han script run.
const HANGUL_CHAIN: &[&str] = &["Source Han Sans K"];

/// Chain of the writing systems whose ONLY glyphs in the bundle sit in `HanaMinA`.
///
/// `HanaMinA` is in [`FORBIDDEN_FAMILIES`], so those glyphs are reachable through a
/// script chain and nowhere else; naming it here is what makes them render at all.
const HANAMIN_A_CHAIN: &[&str] = &["HanaMinA"];

/// [`HANAMIN_A_CHAIN`] plus the handful of codepoints of the same scripts that only
/// `Plangothic P2` adds (also forbidden, hence also unreachable without this entry).
const HANAMIN_A_THEN_PLANGOTHIC_CHAIN: &[&str] = &["HanaMinA", "Plangothic P2"];

/// Chain of the writing systems whose ONLY bundled coverage sits in `Plangothic P2`.
///
/// Same mechanism as [`HANAMIN_A_CHAIN`]: the font is forbidden, so naming it here is
/// the only thing that makes those codepoints render instead of turning into tofu.
const PLANGOTHIC_P2_CHAIN: &[&str] = &["Plangothic P2"];

/// Chain of the writing systems both Plangothic planes cover.
///
/// P1 first, matching the `NN-` order of the bundle (`80-` before `81-`), so the
/// smaller plane is mapped first when it already has the glyph.
const PLANGOTHIC_CHAIN: &[&str] = &["Plangothic P1", "Plangothic P2"];

/// Writing system -> preferred bundled families, most specific first.
///
/// This is the mechanism that makes `ext` "load on demand": the shaper names ONE
/// font for the script at hand instead of walking the database, so exactly that file
/// is mapped. Names are the family names the fonts declare in their `name` table
/// (mirrored by `ms_fonts::StackFont::family_name`), never file names — cosmic-text
/// can only address a font by family (`fallback/mod.rs:405-416`).
///
/// A script that is NOT listed here falls through to [`FallbackTables::common`] and
/// then to the final whole-database pass. That pass reaches every NON-FORBIDDEN
/// bundled font, so dropping a new `30-NotoSans<Script>` file into `fonts/ui/ext`
/// keeps working without a code change (`fonts/ui/MODULE_README.md`); listing the
/// script only makes the choice precise and cheap.
///
/// It does NOT reach a [`FORBIDDEN_FAMILIES`] font (`fallback/mod.rs:449-452`).
/// A writing system whose only glyphs in the bundle live in a forbidden font is
/// therefore UNREACHABLE — tofu, with no diagnostic — until it is named here. That is
/// why the rare-plane blocks below exist, and why any font added to
/// [`FORBIDDEN_FAMILIES`] must come with the script entries that keep its exclusive
/// coverage reachable.
///
/// FOUR SCRIPT CLASSES CAN NEVER BE ADDRESSED HERE. cosmic-text derives the scripts of
/// a shaping run from its characters and DROPS `Common`, `Inherited`, `Latin` and
/// `Unknown` while doing so (`cosmic-text-0.14.2/src/shape.rs:249-257`), so
/// `script_fallback` is never called with any of them and an entry for one would be
/// dead weight. Characters of those classes ride on the scripts of the rest of their
/// run instead: a Latin or `Inherited` codepoint inside, say, an Arabic run is looked
/// up through the Arabic chain, and in a run that has no other script it goes straight
/// to [`FallbackTables::common`] and then to the final whole-database pass — which
/// cannot reach a forbidden font. The bundle's handful of Latin-script codepoints that
/// only `Plangothic P2` ships are unreachable for that structural reason and are
/// deliberately absent below; the exhaustive guard
/// `every_forbidden_only_codepoint_is_reachable_through_a_script_chain` skips exactly
/// these four classes for the same reason.
///
/// Entries naming a family the resolved stack does not ship are dropped (and logged)
/// by [`build_fallback_tables`], so the table can safely stay ahead of the bundle.
const SCRIPT_FAMILIES: &[(Script, &[&str])] = &[
    // CJK: one shared chain, locale-independent on purpose (see `MsFallback`).
    (Script::Han, HAN_CHAIN),
    (Script::Bopomofo, HAN_CHAIN),
    (Script::Katakana, HAN_CHAIN),
    (Script::Hiragana, HIRAGANA_CHAIN),
    (Script::Hangul, HANGUL_CHAIN),
    // One bundled `30-NotoSans<Script>` face per remaining writing system.
    //
    // Where a trailing rare-plane font appears, the bundle covers a few codepoints of
    // that writing system ONLY in that (forbidden) font — recent block extensions the
    // profile Noto Sans has not caught up with. The profile face stays FIRST, so
    // ordinary text of the script is unaffected and the rare-plane font is consulted
    // only for what the profile face genuinely lacks.
    (Script::Adlam, &["Noto Sans Adlam"]),
    (Script::Arabic, &["Noto Sans Arabic", "Plangothic P2"]),
    (Script::Armenian, &["Noto Sans Armenian"]),
    (Script::Balinese, &["Noto Sans Balinese", "Plangothic P2"]),
    (Script::Bengali, &["Noto Sans Bengali"]),
    (Script::Canadian_Aboriginal, &["Noto Sans Canadian Aboriginal"]),
    (Script::Cham, &["Noto Sans Cham"]),
    (Script::Cherokee, &["Noto Sans Cherokee"]),
    // Cyrillic has no profile face of its own: the CORE `Noto Sans` covers it, and it
    // is also `common_fallback[0]`, so naming it first keeps the existing resolution
    // and only appends the rare-plane font behind it.
    (Script::Cyrillic, &["Noto Sans", "Plangothic P2"]),
    (Script::Devanagari, &["Noto Sans Devanagari", "Plangothic P2"]),
    (Script::Ethiopic, &["Noto Sans Ethiopic", "Plangothic P2"]),
    (Script::Georgian, &["Noto Sans Georgian"]),
    (Script::Gujarati, &["Noto Sans Gujarati"]),
    (Script::Gurmukhi, &["Noto Sans Gurmukhi"]),
    (Script::Hebrew, &["Noto Sans Hebrew"]),
    (Script::Javanese, &["Noto Sans Javanese"]),
    (Script::Kannada, &["Noto Sans Kannada", "Plangothic P2"]),
    (Script::Khmer, &["Noto Sans Khmer"]),
    (Script::Lao, &["Noto Sans Lao", "Plangothic P2"]),
    (Script::Malayalam, &["Noto Sans Malayalam"]),
    (Script::Mongolian, &["Noto Sans Mongolian", "Plangothic P2"]),
    (Script::Myanmar, &["Noto Sans Myanmar", "Plangothic P2"]),
    (Script::Nko, &["Noto Sans NKo"]),
    (Script::Ol_Chiki, &["Noto Sans Ol Chiki"]),
    (Script::Oriya, &["Noto Sans Oriya"]),
    (Script::Saurashtra, &["Noto Sans Saurashtra"]),
    (Script::Sinhala, &["Noto Sans Sinhala"]),
    (Script::Sundanese, &["Noto Sans Sundanese"]),
    (Script::Syriac, &["Noto Sans Syriac", "Plangothic P2"]),
    (Script::Tamil, &["Noto Sans Tamil", "Plangothic P2"]),
    (Script::Telugu, &["Noto Sans Telugu", "Plangothic P2"]),
    (Script::Thaana, &["Noto Sans Thaana"]),
    (Script::Thai, &["Noto Sans Thai"]),
    (Script::Tibetan, &["Noto Serif Tibetan"]),
    (Script::Tifinagh, &["Noto Sans Tifinagh"]),
    (Script::Vai, &["Noto Sans Vai"]),
    (Script::Yi, &["Noto Sans Yi"]),
    // Writing systems the bundle covers ONLY from a forbidden font, so the final
    // whole-database pass cannot reach them (see the note above). Counted from the
    // `cmap` tables of the shipped files: 347 codepoints exist in `91-HanaMinA.ttf`
    // and in no other bundled font, plus ten codepoints of the same scripts that only
    // `81-PlangothicP2-Regular.otf` adds.
    (Script::Runic, HANAMIN_A_THEN_PLANGOTHIC_CHAIN), // U+16A0..=U+16F0, +U+16F1..=U+16F8
    (Script::Old_Turkic, HANAMIN_A_CHAIN),            // U+10C00..=U+10C48
    (Script::Carian, HANAMIN_A_CHAIN),                // U+102A0..=U+102D0
    (Script::Lisu, HANAMIN_A_THEN_PLANGOTHIC_CHAIN),  // U+A4D0..=U+A4FF, +U+11FB0
    (Script::Old_Italic, HANAMIN_A_THEN_PLANGOTHIC_CHAIN), // U+10300..=U+10323, +U+1031F
    (Script::Lycian, HANAMIN_A_CHAIN),                // U+10280..=U+1029C
    (Script::Lydian, HANAMIN_A_CHAIN),                // U+10920..=U+1093F
    // Partial by nature: these two scripts have exactly four and two bundled glyphs
    // (U+2C80..=U+2C83 and U+10000/U+1000F). The entry makes those reachable; the rest
    // of both blocks stays tofu because `fonts/ui` ships no font for them.
    (Script::Coptic, HANAMIN_A_CHAIN),
    (Script::Linear_B, HANAMIN_A_CHAIN),
    // The same class again, but served by the Plangothic planes: writing systems the
    // bundle ships NO profile face for and that exist in the base only inside
    // `80-PlangothicP1` / `81-PlangothicP2`. Without these entries they are tofu with
    // no diagnostic, while the interface (which searches for a glyph without the
    // weight filter and without a forbidden list) shows them — so the two views of the
    // same font stack disagreed. Coverage is PARTIAL for most of them: Plangothic
    // ships what it ships, and the rest of each block stays tofu.
    (Script::Tangut, PLANGOTHIC_CHAIN),
    (Script::Khitan_Small_Script, PLANGOTHIC_CHAIN),
    (Script::Ahom, PLANGOTHIC_P2_CHAIN),
    (Script::Beria_Erfe, PLANGOTHIC_P2_CHAIN),
    (Script::Brahmi, PLANGOTHIC_P2_CHAIN),
    (Script::Caucasian_Albanian, PLANGOTHIC_P2_CHAIN),
    (Script::Chakma, PLANGOTHIC_P2_CHAIN),
    (Script::Chorasmian, PLANGOTHIC_P2_CHAIN),
    (Script::Cuneiform, PLANGOTHIC_P2_CHAIN),
    (Script::Cypro_Minoan, PLANGOTHIC_P2_CHAIN),
    (Script::Dives_Akuru, PLANGOTHIC_P2_CHAIN),
    (Script::Duployan, PLANGOTHIC_P2_CHAIN),
    (Script::Egyptian_Hieroglyphs, PLANGOTHIC_P2_CHAIN),
    (Script::Elymaic, PLANGOTHIC_P2_CHAIN),
    (Script::Garay, PLANGOTHIC_P2_CHAIN),
    (Script::Glagolitic, PLANGOTHIC_P2_CHAIN),
    (Script::Gurung_Khema, PLANGOTHIC_P2_CHAIN),
    (Script::Kaithi, PLANGOTHIC_P2_CHAIN),
    (Script::Kawi, PLANGOTHIC_P2_CHAIN),
    (Script::Kharoshthi, PLANGOTHIC_P2_CHAIN),
    (Script::Khojki, PLANGOTHIC_P2_CHAIN),
    (Script::Khudawadi, PLANGOTHIC_P2_CHAIN),
    (Script::Kirat_Rai, PLANGOTHIC_P2_CHAIN),
    (Script::Limbu, PLANGOTHIC_P2_CHAIN),
    (Script::Mahajani, PLANGOTHIC_P2_CHAIN),
    (Script::Makasar, PLANGOTHIC_P2_CHAIN),
    (Script::Mende_Kikakui, PLANGOTHIC_P2_CHAIN),
    (Script::Modi, PLANGOTHIC_P2_CHAIN),
    (Script::Nag_Mundari, PLANGOTHIC_P2_CHAIN),
    (Script::Nandinagari, PLANGOTHIC_P2_CHAIN),
    (Script::Newa, PLANGOTHIC_P2_CHAIN),
    (Script::Nushu, PLANGOTHIC_P2_CHAIN),
    (Script::Ol_Onal, PLANGOTHIC_P2_CHAIN),
    (Script::Old_Hungarian, PLANGOTHIC_P2_CHAIN),
    (Script::Old_Sogdian, PLANGOTHIC_P2_CHAIN),
    (Script::Old_Uyghur, PLANGOTHIC_P2_CHAIN),
    (Script::Psalter_Pahlavi, PLANGOTHIC_P2_CHAIN),
    (Script::Sharada, PLANGOTHIC_P2_CHAIN),
    (Script::Siddham, PLANGOTHIC_P2_CHAIN),
    (Script::Sidetic, PLANGOTHIC_P2_CHAIN),
    (Script::SignWriting, PLANGOTHIC_P2_CHAIN),
    (Script::Sogdian, PLANGOTHIC_P2_CHAIN),
    (Script::Sunuwar, PLANGOTHIC_P2_CHAIN),
    (Script::Syloti_Nagri, PLANGOTHIC_P2_CHAIN),
    (Script::Tagalog, PLANGOTHIC_P2_CHAIN),
    (Script::Tai_Yo, PLANGOTHIC_P2_CHAIN),
    (Script::Takri, PLANGOTHIC_P2_CHAIN),
    (Script::Tangsa, PLANGOTHIC_P2_CHAIN),
    (Script::Tirhuta, PLANGOTHIC_P2_CHAIN),
    (Script::Todhri, PLANGOTHIC_P2_CHAIN),
    (Script::Tolong_Siki, PLANGOTHIC_P2_CHAIN),
    (Script::Toto, PLANGOTHIC_P2_CHAIN),
    (Script::Vithkuqi, PLANGOTHIC_P2_CHAIN),
    (Script::Zanabazar_Square, PLANGOTHIC_P2_CHAIN),
];

/// The three fallback lists of [`MsFallback`], resolved against the font stack that
/// this process actually found.
///
/// Every name in here is guaranteed to be the family name of a face present in
/// [`base_database`], because the builder filters the static tables against the
/// manifest. Built once; borrowed by every `FontSystem`.
#[derive(Debug, Default)]
struct FallbackTables {
    /// The `core` chain in `NN-` order, consulted after the script chain.
    common: Vec<&'static str>,
    /// Families excluded from the final whole-database pass.
    forbidden: Vec<&'static str>,
    /// Per-script chains; a script absent from the map has no preferred font.
    script: HashMap<Script, Vec<&'static str>>,
}

/// The renderer's fallback contract: an explicit, machine-independent chain built
/// from the bundled `fonts/ui` stack.
///
/// It is deliberately LOCALE-BLIND. `PlatformFallback` picks a Han variant from the
/// system locale (`cosmic-text-0.14.2/src/font/fallback/unix.rs:56-69`); doing the
/// same here would make one project render differently for a translator and an
/// editor, which is exactly what this base exists to prevent. The bundle ships a
/// single CJK face, so there is nothing to choose between anyway.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MsFallback {
    tables: &'static FallbackTables,
}

impl Fallback for MsFallback {
    fn common_fallback(&self) -> &[&'static str] {
        &self.tables.common
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        &self.tables.forbidden
    }

    fn script_fallback(&self, script: Script, _locale: &str) -> &[&'static str] {
        self.tables
            .script
            .get(&script)
            .map_or(&[], |families| families.as_slice())
    }
}

/// Weight every FALLBACK face of the base is registered under, whatever the file
/// declares. The `bold` tier is exempt — telling it from its regular sibling by weight
/// is its entire purpose.
///
/// Why the declared weight is overridden: cosmic-text admits a candidate into the
/// script and common fallback passes only when `font_weight_diff == 0`
/// (`font/fallback/mod.rs:410-417`), i.e. only when the face's weight EQUALS the
/// weight the run is shaped at. A fallback face exists to supply glyphs the selected
/// font lacks, at whatever weight the text happens to be — but `90-HanaMinB.ttf` and
/// `91-HanaMinA.ttf` declare `usWeightClass = 500`, which put them 100 away from every
/// normal-weight run and therefore out of reach of both weight-filtered passes. The
/// final whole-database pass cannot rescue them either: they are in
/// [`FORBIDDEN_FAMILIES`]. Left alone, the two files are dead weight in the bundle and
/// the 347 codepoints only they ship render as tofu.
///
/// This changes MATCHING only. Outlines, metrics and hinting come from the file and
/// are untouched, so a glyph drawn from a re-pinned face is byte-identical to one
/// drawn from the face as declared.
const FALLBACK_FACE_WEIGHT: fontdb::Weight = fontdb::Weight::NORMAL;

/// Identity of one resident font buffer: the ADDRESS and length of the `'static`
/// slice `ms-fonts` handed out. `ms_fonts::bytes` is idempotent by address (one
/// read per process), so this pair identifies the buffer exactly and needs no
/// hashing of the ~17 MB of resident font data.
type ResidentBufferKey = (usize, usize);

/// The built render font base: the database plus the index that maps a resident
/// (`Source::Binary`) buffer back to the face ids it produced.
///
/// The index exists so a caller that selects a BUNDLED font as its own font (the
/// "built-in interface font" entry of the typing panel) can be served by the face
/// that is already in the database. Registering the same bytes a second time would
/// put a duplicate `(family, weight, style)` face into every pooled system and make
/// `Family::Name` matching depend on face-id order for no benefit.
#[derive(Debug)]
struct RenderFontBase {
    db: fontdb::Database,
    resident: HashMap<ResidentBufferKey, Vec<fontdb::ID>>,
}

/// Key of a resident buffer: its address and length.
fn resident_key(bytes: &[u8]) -> ResidentBufferKey {
    (bytes.as_ptr() as usize, bytes.len())
}

/// The bundled font base, built on first use; its database is cloned into every
/// `FontSystem`.
static BASE_DATABASE: OnceLock<RenderFontBase> = OnceLock::new();

/// The resolved fallback lists, built on first use and shared by every `MsFallback`.
static FALLBACK_TABLES: OnceLock<FallbackTables> = OnceLock::new();

/// Builds a `FontSystem` over the bundled base with the deterministic fallback
/// chain installed.
///
/// The system-font scan of `FontSystem::new()` is intentionally NOT performed: the
/// render database is the bundled stack plus whatever font the caller registers for
/// this render, and nothing else. The database clone is cheap — `fontdb::Database`
/// clones `Arc`s and name strings, never font bytes
/// (`fontdb-0.16.2/src/lib.rs:151-159`) — and the `ext` tier is stored as
/// `Source::File`, so its bytes enter the address space only when a glyph lookup
/// reaches that file.
///
/// The first call resolves the `fonts/ui` manifest and reads the `core`/`bold`
/// bytes, which is blocking I/O and must not happen on the GUI thread; the pool's
/// `prewarm_font_system_pool` exists for exactly that.
///
/// Public (re-exported from the crate root) so out-of-crate render harnesses —
/// today `src/bin/text_render_test` — build their `FontSystem` the way production
/// does instead of calling `FontSystem::new()` and silently testing against the
/// operator's installed fonts. Production render code must go through the pool
/// (`with_leased_font_system`), never through this constructor directly.
#[must_use]
pub fn new_render_font_system() -> FontSystem {
    FontSystem::new_with_locale_and_db_and_fallback(
        RENDER_LOCALE.to_string(),
        base_database().clone(),
        MsFallback {
            tables: fallback_tables(),
        },
    )
}

/// The process-wide bundled font base, built on first use.
#[must_use]
fn base() -> &'static RenderFontBase {
    BASE_DATABASE.get_or_init(|| match ms_fonts::stack() {
        Some(stack) => build_base_database(stack.core(), stack.bold(), stack.ext()),
        None => {
            runtime_log::log_warn(
                "[ms_text_render] the bundled fonts/ui stack is unavailable; renders will use \
                 ONLY the selected font and any character it lacks will be drawn as tofu",
            );
            RenderFontBase {
                db: fontdb::Database::new(),
                resident: HashMap::new(),
            }
        }
    })
}

/// The process-wide bundled font database, built on first use.
#[must_use]
fn base_database() -> &'static fontdb::Database {
    &base().db
}

/// The face ids the resident bundled buffer `bytes` produced in the base database,
/// or `None` when those bytes are not part of the bundled stack.
///
/// Matching is by BUFFER IDENTITY (address + length), not by content hash: only the
/// very slice `ms_fonts::bytes` handed out can match, so an unrelated font that
/// happens to be byte-identical to a bundled file is never silently rerouted here.
///
/// The returned ids are valid in every `FontSystem` built by
/// [`new_render_font_system`], because `fontdb::Database::clone` preserves face ids.
/// The loader still re-validates them against the system it is loading into
/// (`font_registry::resident_ids_in`), so a foreign database can never be misread.
#[must_use]
pub(crate) fn resident_face_ids(bytes: &[u8]) -> Option<&'static [fontdb::ID]> {
    base()
        .resident
        .get(&resident_key(bytes))
        .map(Vec::as_slice)
}

/// The process-wide resolved fallback lists, built on first use.
#[must_use]
fn fallback_tables() -> &'static FallbackTables {
    FALLBACK_TABLES.get_or_init(|| match ms_fonts::stack() {
        Some(stack) => build_fallback_tables(stack.core(), stack.bold(), stack.ext()),
        // The warning is already emitted by `base_database`; an empty table set means
        // "no fallback", which is the only honest answer without a stack.
        None => FallbackTables::default(),
    })
}

/// Registers the three tiers into a fresh database, in `NN-` fallback order.
///
/// Registration order is meaningful: cosmic-text's final whole-database pass walks
/// faces by `(weight distance, weight, id)` and `id`s are handed out in insertion
/// order, so `core` -> `bold` -> `ext` makes that pass follow the curated order too.
///
/// Tier storage differs on purpose:
/// - `core`/`bold` become `Source::Binary` over the `'static` bytes `ms-fonts` already
///   holds, so the renderer shares ONE copy with the egui UI instead of reading the
///   files again;
/// - `ext` becomes `Source::File`. fontdb maps such a file only to read its `name`
///   table and drops the mapping immediately (`fontdb-0.16.2/src/lib.rs:264-274`);
///   the bytes appear in the address space at the first `FontSystem::get_font`
///   (`cosmic-text-0.14.2/src/font/system.rs:252-272`). That IS the on-demand
///   loading this tier needs — do not reimplement it.
///
/// Weight handling differs by tier as well: every tier except `bold` is registered at
/// [`FALLBACK_FACE_WEIGHT`], because a fallback face has to be reachable at the weight
/// the text is shaped at. Only `bold` keeps its declared weight.
///
/// A file that cannot be read or parsed is logged and skipped; the base is always
/// built, never failed.
fn build_base_database(
    core: &[StackFont],
    bold: &[StackFont],
    ext: &[StackFont],
) -> RenderFontBase {
    let mut db = fontdb::Database::new();
    let mut resident: HashMap<ResidentBufferKey, Vec<fontdb::ID>> =
        HashMap::with_capacity(core.len() + bold.len());

    // The `bold` tier is the only one whose declared weight carries meaning; every
    // other tier is fallback material and is pinned to `FALLBACK_FACE_WEIGHT`.
    for (font, keep_declared_weight) in core
        .iter()
        .map(|font| (font, false))
        .chain(bold.iter().map(|font| (font, true)))
    {
        let Some(bytes) = ms_fonts::bytes(font) else {
            // `ms-fonts` already logged the I/O reason; state the render consequence.
            runtime_log::log_warn(format!(
                "[ms_text_render] resident font '{}' ({}) could not be read; it is missing \
                 from the render fallback chain this session",
                font.family_name,
                font.path.display()
            ));
            continue;
        };
        // `&'static [u8]` satisfies fontdb's `Arc<dyn AsRef<[u8]> + Sync + Send>`, so
        // the Arc stores a fat pointer to the shared bytes, not a second copy.
        let source: fontdb::Source =
            fontdb::Source::Binary(Arc::new(bytes) as Arc<dyn AsRef<[u8]> + Sync + Send>);
        let ids = if keep_declared_weight {
            db.load_font_source(source).to_vec()
        } else {
            load_source_at_fallback_weight(&mut db, source)
        };
        if ids.is_empty() {
            runtime_log::log_warn(format!(
                "[ms_text_render] fontdb parsed no face from resident font '{}'; it is missing \
                 from the render fallback chain this session",
                font.path.display()
            ));
            continue;
        }
        // Index the buffer so a caller selecting this very font is served by the face
        // registered here. Two stack entries pointing at the same file share one
        // buffer (`ms_fonts::bytes` is idempotent by address), so the first insert
        // wins and the ids stay the ones actually in the database.
        resident.entry(resident_key(bytes)).or_insert(ids);
    }

    for font in ext {
        if load_source_at_fallback_weight(&mut db, fontdb::Source::File(font.path.clone()))
            .is_empty()
        {
            runtime_log::log_warn(format!(
                "[ms_text_render] fontdb parsed no face from on-demand font '{}'; it is missing \
                 from the render fallback chain this session",
                font.path.display()
            ));
        }
    }

    // fontdb seeds the generic families with names of fonts nobody here ships
    // ("Arial", "Times New Roman", ... — `fontdb-0.16.2/src/lib.rs:167-188`). Point
    // them at the first core face instead, so the pool's pristine-default restore
    // (`FontFaceCache::for_system`) restores a family that actually exists.
    if let Some(first) = core.first() {
        db.set_sans_serif_family(first.family_name);
        db.set_serif_family(first.family_name);
        db.set_monospace_family(first.family_name);
        db.set_cursive_family(first.family_name);
        db.set_fantasy_family(first.family_name);
    }

    runtime_log::log_info(format!(
        "[ms_text_render] deterministic render font base built: {} face(s) from {} core + {} \
         bold (resident) and {} ext (mapped on demand); the OS font database is NOT used",
        db.len(),
        core.len(),
        bold.len(),
        ext.len()
    ));

    RenderFontBase { db, resident }
}

/// Registers `source` and returns its face ids, every face pinned to
/// [`FALLBACK_FACE_WEIGHT`].
///
/// The returned ids are in registration order and are the ids the faces have in `db`
/// afterwards — a re-pinned face gets a NEW id, so a caller must use these and not the
/// ones `load_font_source` handed out.
fn load_source_at_fallback_weight(
    db: &mut fontdb::Database,
    source: fontdb::Source,
) -> Vec<fontdb::ID> {
    let loaded: Vec<fontdb::ID> = db.load_font_source(source).to_vec();
    loaded
        .into_iter()
        .map(|id| repin_face_weight(db, id))
        .collect()
}

/// Re-registers the face `id` at [`FALLBACK_FACE_WEIGHT`] when it declares another
/// weight, and returns the id the face has afterwards.
///
/// fontdb offers no way to edit a registered face, so the face is removed and pushed
/// back as a copy with the corrected weight (`remove_face` + `push_face_info`,
/// `fontdb-0.16.2/src/lib.rs:501-517`). The freed slot is reused by the push, so the
/// face keeps its POSITION among the faces — which matters, because face-id order is
/// the order of cosmic-text's final whole-database pass and therefore the curated
/// `NN-` fallback order. `base_face_order_follows_the_registration_order` pins that.
fn repin_face_weight(db: &mut fontdb::Database, id: fontdb::ID) -> fontdb::ID {
    let Some(face) = db.face(id) else {
        return id;
    };
    if face.weight == FALLBACK_FACE_WEIGHT {
        return id;
    }
    let mut info = face.clone();
    let declared_weight = info.weight;
    let post_script_name = info.post_script_name.clone();
    info.weight = FALLBACK_FACE_WEIGHT;
    // `push_face_info` overwrites the id with the key it allocates; the dummy is the
    // documented placeholder for that.
    info.id = fontdb::ID::dummy();

    let known: HashSet<fontdb::ID> = db.faces().map(|face| face.id).collect();
    db.remove_face(id);
    db.push_face_info(info);
    let Some(new_id) = db
        .faces()
        .map(|face| face.id)
        .find(|face_id| !known.contains(face_id))
    else {
        // Unreachable with fontdb 0.16.2 (`push_face_info` always inserts); reported
        // rather than ignored so a future fontdb change cannot silently drop a font.
        runtime_log::log_warn(format!(
            "[ms_text_render] fontdb did not re-register fallback face '{post_script_name}' after \
             its weight was pinned; the font is missing from the render fallback chain this session"
        ));
        return id;
    };
    runtime_log::log_info(format!(
        "[ms_text_render] fallback face '{post_script_name}' declares weight {}; registered at {} \
         so cosmic-text's weight-filtered script/common passes can reach it",
        declared_weight.0,
        FALLBACK_FACE_WEIGHT.0
    ));
    new_id
}

/// Resolves the static fallback tables against the fonts the stack actually ships.
///
/// `common` is the `core` tier in `NN-` order (duplicates collapsed, since the bold
/// face shares its family name with its regular sibling and is selected by WEIGHT,
/// not by name). `forbidden` and the per-script chains keep only families present in
/// the stack, so a name that drifted out of the bundle turns into one logged warning
/// instead of a silent, never-matching entry.
fn build_fallback_tables(
    core: &[StackFont],
    bold: &[StackFont],
    ext: &[StackFont],
) -> FallbackTables {
    let available: HashSet<&'static str> = core
        .iter()
        .chain(bold.iter())
        .chain(ext.iter())
        .map(|font| font.family_name)
        .collect();

    let mut common: Vec<&'static str> = Vec::with_capacity(core.len());
    for font in core {
        if !common.contains(&font.family_name) {
            common.push(font.family_name);
        }
    }

    // Names the tables promise but the bundle does not provide. Collected instead of
    // logged per entry so the operator gets one actionable line, sorted and deduped.
    let mut absent: BTreeSet<&'static str> = BTreeSet::new();
    let mut keep_present = |names: &'static [&'static str]| -> Vec<&'static str> {
        let mut kept = Vec::with_capacity(names.len());
        for name in names {
            if available.contains(name) {
                kept.push(*name);
            } else {
                absent.insert(name);
            }
        }
        kept
    };

    let forbidden = keep_present(FORBIDDEN_FAMILIES);

    let mut script: HashMap<Script, Vec<&'static str>> =
        HashMap::with_capacity(SCRIPT_FAMILIES.len());
    for (writing_system, names) in SCRIPT_FAMILIES {
        let kept = keep_present(names);
        if !kept.is_empty() {
            script.insert(*writing_system, kept);
        }
    }

    if !absent.is_empty() {
        runtime_log::log_warn(format!(
            "[ms_text_render] the render fallback tables name {} font famil(y/ies) that the \
             resolved fonts/ui stack does not ship: {}; text needing them falls back to the \
             core chain and may render as tofu",
            absent.len(),
            absent.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    FallbackTables {
        common,
        forbidden,
        script,
    }
}

/// Test-only view of the `fonts/ui` bundle SHIPPED next to this checkout.
///
/// Why it exists: `ms_fonts::stack()` resolves the bundle from the process working
/// directory (or the executable directory), and a cargo test binary runs with its
/// package root as the working directory — so the production manifest never finds the
/// repository bundle and every `new_render_font_system()` in a test would be built on
/// an EMPTY database. A test asserting "the base holds exactly the bundle" would then
/// compare 0 against 0 and pass while proving nothing.
///
/// This module addresses the repository directly through `CARGO_MANIFEST_DIR` (the
/// same trick the drift guard uses) and feeds the paths through the very builders
/// production uses, so a bundle-backed test exercises the real database, the real
/// fallback tables and the real `MsFallback` — only the manifest resolution differs.
#[cfg(test)]
pub(crate) mod test_bundle {
    use super::{
        FallbackTables, MsFallback, RENDER_LOCALE, build_base_database, build_fallback_tables,
    };
    use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping, fontdb};
    use ms_fonts::{StackFont, Tier};
    use std::path::PathBuf;
    use std::sync::OnceLock;

    /// The three tiers of the shipped bundle, described once per test process.
    static SHIPPED_STACK: OnceLock<Option<ShippedStack>> = OnceLock::new();

    /// The fallback tables of the shipped bundle, built once and leaked so every
    /// test system can borrow them (`Fallback` implementors must be `'static`).
    static SHIPPED_TABLES: OnceLock<&'static FallbackTables> = OnceLock::new();

    /// The shipped bundle described as the production manifest would describe it.
    #[derive(Debug)]
    pub(crate) struct ShippedStack {
        pub(crate) core: Vec<StackFont>,
        pub(crate) bold: Vec<StackFont>,
        pub(crate) ext: Vec<StackFont>,
    }

    impl ShippedStack {
        /// Total number of shipped files, i.e. the face count the base must hold.
        pub(crate) fn file_count(&self) -> usize {
            self.core.len() + self.bold.len() + self.ext.len()
        }
    }

    /// Font files of one tier of the shipped bundle, in `NN-` (path) order.
    pub(crate) fn tier_paths(tier: Tier) -> Vec<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fonts/ui")
            .join(tier.dir_name());
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|item| item.path()))
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            matches!(
                                ext.to_ascii_lowercase().as_str(),
                                "ttf" | "otf" | "ttc" | "otc"
                            )
                        })
            })
            .collect();
        paths.sort();
        paths
    }

    /// Family name one shipped file declares, read the way fontdb reads it.
    ///
    /// Leaked because `StackFont::family_name` is `&'static str` (the cosmic-text
    /// `Fallback` trait can only name a family that way). Bounded by the number of
    /// bundled files and done once per test process.
    fn family_name_of(path: &PathBuf) -> Option<&'static str> {
        let mut db = fontdb::Database::new();
        db.load_font_file(path).ok()?;
        let name = db.faces().next()?.families.first()?.0.clone();
        Some(Box::leak(name.into_boxed_str()))
    }

    /// Describes one tier of the shipped bundle as `StackFont` records.
    fn describe(tier: Tier) -> Vec<StackFont> {
        tier_paths(tier)
            .into_iter()
            .filter_map(|path| {
                let family_name = family_name_of(&path)?;
                Some(StackFont {
                    order: 0,
                    path,
                    family_name,
                    tier,
                })
            })
            .collect()
    }

    /// The shipped bundle, or `None` when this checkout has no `fonts/ui/core`
    /// (a test that needs the bundle must then skip instead of asserting nothing).
    pub(crate) fn stack() -> Option<&'static ShippedStack> {
        SHIPPED_STACK
            .get_or_init(|| {
                let core = describe(Tier::Core);
                if core.is_empty() {
                    return None;
                }
                Some(ShippedStack {
                    core,
                    bold: describe(Tier::Bold),
                    ext: describe(Tier::Ext),
                })
            })
            .as_ref()
    }

    /// A `FontSystem` over the SHIPPED bundle, built exactly like
    /// [`super::new_render_font_system`] but with the manifest resolved from the
    /// repository instead of the working directory.
    pub(crate) fn font_system() -> Option<FontSystem> {
        let stack = stack()?;
        let tables = SHIPPED_TABLES.get_or_init(|| {
            Box::leak(Box::new(build_fallback_tables(
                &stack.core,
                &stack.bold,
                &stack.ext,
            )))
        });
        Some(FontSystem::new_with_locale_and_db_and_fallback(
            RENDER_LOCALE.to_string(),
            build_base_database(&stack.core, &stack.bold, &stack.ext).db,
            MsFallback { tables },
        ))
    }

    /// Shapes `text` with `attrs` and returns one `(glyph_id, family name)` pair per
    /// laid-out glyph.
    ///
    /// `glyph_id == 0` is `.notdef`, i.e. the tofu box a reader sees
    /// (`cosmic-text-0.14.2/src/shape.rs:506-507`), so this is how a reachability
    /// test tells "rendered by font X" from "rendered as a box".
    pub(crate) fn shaped_glyphs(
        font_system: &mut FontSystem,
        text: &str,
        attrs: &Attrs<'_>,
    ) -> Vec<(u16, String)> {
        let mut buffer = Buffer::new(font_system, Metrics::new(32.0, 32.0));
        buffer.set_size(font_system, None, None);
        buffer.set_text(font_system, text, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(font_system, false);
        let names: Vec<(u16, String)> = buffer
            .layout_runs()
            .flat_map(|run| run.glyphs.iter())
            .map(|glyph| {
                let family = font_system
                    .db()
                    .face(glyph.font_id)
                    .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
                    .unwrap_or_default();
                (glyph.glyph_id, family)
            })
            .collect();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FORBIDDEN_FAMILIES, FallbackTables, MsFallback, SCRIPT_FAMILIES, build_base_database,
        build_fallback_tables, new_render_font_system, test_bundle,
    };
    use cosmic_text::{Attrs, Fallback, fontdb};
    use ms_fonts::{StackFont, Tier};
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use unicode_script::{Script, UnicodeScript};

    /// Wraps resolved tables into an `MsFallback`.
    ///
    /// `Fallback` implementors must be `'static` (they are boxed into the
    /// `FontSystem`), so a test's tables are leaked. Bounded: a handful of small
    /// `Vec`s per test process, never in production code.
    fn fallback_over(tables: FallbackTables) -> MsFallback {
        MsFallback {
            tables: Box::leak(Box::new(tables)),
        }
    }

    /// Path of a real font fixture, so database tests exercise actual fontdb
    /// parsing instead of a mock. Same fixture the pool/pipeline tests use.
    fn fixture_font_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
    }

    /// A synthetic stack entry. `family_name` is what the fallback tables address;
    /// `path` is only read by the database builder.
    fn stack_font(order: u32, family_name: &'static str, tier: Tier, path: &Path) -> StackFont {
        StackFont {
            order,
            path: path.to_path_buf(),
            family_name,
            tier,
        }
    }

    /// The core chain must reach `common_fallback` complete and in `NN-` order:
    /// cosmic-text consults it verbatim after the script chain
    /// (`cosmic-text-0.14.2/src/font/fallback/mod.rs:429-441`), so the order here IS
    /// the documented `fonts/ui` fallback order.
    #[test]
    fn common_fallback_is_the_core_chain_in_prefix_order() {
        let path = fixture_font_path();
        let core = [
            stack_font(0, "Noto Sans", Tier::Core, &path),
            stack_font(1, "Source Han Sans K", Tier::Core, &path),
            stack_font(2, "Noto Sans Symbols", Tier::Core, &path),
            stack_font(3, "Noto Sans Symbols 2", Tier::Core, &path),
        ];
        // The bold face declares the SAME family as its regular sibling; it must not
        // appear twice in the chain (it is picked by weight, not by name).
        let bold = [stack_font(0, "Noto Sans", Tier::Bold, &path)];

        let tables = build_fallback_tables(&core, &bold, &[]);

        assert_eq!(
            tables.common,
            vec![
                "Noto Sans",
                "Source Han Sans K",
                "Noto Sans Symbols",
                "Noto Sans Symbols 2",
            ]
        );
    }

    /// The four rare-CJK fonts must be excluded from the final whole-database pass,
    /// or one missing codepoint would mmap ~73 MB.
    #[test]
    fn forbidden_fallback_holds_the_large_rare_fonts() {
        let path = fixture_font_path();
        let core = [stack_font(0, "Noto Sans", Tier::Core, &path)];
        let ext: Vec<StackFont> = FORBIDDEN_FAMILIES
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let order = u32::try_from(80 + index).unwrap_or(u32::MAX);
                stack_font(order, name, Tier::Ext, &path)
            })
            .collect();

        let tables = build_fallback_tables(&core, &[], &ext);

        assert_eq!(tables.forbidden, FORBIDDEN_FAMILIES.to_vec());
        // Forbidden is an exclusion from the BRUTE-FORCE pass only: the Han chain must
        // still be able to reach those fonts on purpose.
        let fallback = fallback_over(tables);
        let han = fallback.script_fallback(Script::Han, "en-US");
        for name in FORBIDDEN_FAMILIES {
            assert!(
                han.contains(name),
                "the Han chain must still name the forbidden font '{name}'"
            );
        }
    }

    /// A script chain naming a family the stack does not ship must be dropped, not
    /// kept as a name that can never match.
    #[test]
    fn script_chains_are_filtered_against_the_resolved_stack() {
        let path = fixture_font_path();
        let core = [stack_font(0, "Noto Sans", Tier::Core, &path)];
        let ext = [stack_font(30, "Noto Sans Arabic", Tier::Ext, &path)];

        let fallback = fallback_over(build_fallback_tables(&core, &[], &ext));

        assert_eq!(
            fallback.script_fallback(Script::Arabic, "en-US"),
            &["Noto Sans Arabic"]
        );
        // Hebrew is in the static table but absent from this stack.
        assert!(fallback.script_fallback(Script::Hebrew, "en-US").is_empty());
        // A script nobody mapped resolves to the empty chain, never to a panic.
        assert!(
            fallback
                .script_fallback(Script::Cuneiform, "en-US")
                .is_empty()
        );
        // Nothing in this stack is large enough to be forbidden.
        assert!(fallback.forbidden_fallback().is_empty());
    }

    /// The locale must not change a single decision: reproducibility between an
    /// `en-US` translator and a `ru-RU` editor is the reason this base exists.
    #[test]
    fn script_fallback_ignores_the_locale() {
        let path = fixture_font_path();
        let core = [stack_font(0, "Source Han Sans K", Tier::Core, &path)];
        let fallback = fallback_over(build_fallback_tables(&core, &[], &[]));

        for locale in ["en-US", "ja", "ko", "zh-TW", "ru-RU"] {
            assert_eq!(
                fallback.script_fallback(Script::Han, locale),
                &["Source Han Sans K"],
                "locale '{locale}' must not change the Han chain"
            );
        }
    }

    /// Tier storage is the on-demand contract: resident tiers must share the
    /// already-read `'static` bytes (`Source::Binary`), while `ext` must stay a path
    /// (`Source::File`) so its bytes are mapped only when a glyph needs them.
    #[test]
    fn resident_tiers_are_binary_and_ext_stays_a_file() {
        let path = fixture_font_path();
        if !path.exists() {
            eprintln!(
                "skipping resident_tiers_are_binary_and_ext_stays_a_file: font not found at {}",
                path.display()
            );
            return;
        }

        let core = [stack_font(0, "Core", Tier::Core, &path)];
        let bold = [stack_font(0, "Bold", Tier::Bold, &path)];
        let ext = [stack_font(30, "Ext", Tier::Ext, &path)];

        let db = build_base_database(&core, &bold, &ext).db;
        assert_eq!(db.len(), 3, "every tier entry must produce one face");

        let sources: Vec<&fontdb::Source> = db.faces().map(|face| &face.source).collect();
        assert!(
            matches!(sources[0], fontdb::Source::Binary(_)),
            "core must be registered from the shared 'static bytes"
        );
        assert!(
            matches!(sources[1], fontdb::Source::Binary(_)),
            "bold must be registered from the shared 'static bytes"
        );
        assert!(
            matches!(sources[2], fontdb::Source::File(_)),
            "ext must stay a path so its bytes are mapped only on demand"
        );
    }

    /// Every resident (`Source::Binary`) buffer must be indexed to the face ids it
    /// produced, and those ids must resolve in the database.
    ///
    /// This index is what lets a caller SELECT a bundled font (the "built-in
    /// interface font" of the typing panel) without a duplicate face being added to
    /// every pooled system; `font_registry::resident_ids_in` is its only consumer.
    #[test]
    fn resident_buffers_are_indexed_to_their_face_ids() {
        let path = fixture_font_path();
        if !path.exists() {
            eprintln!(
                "skipping resident_buffers_are_indexed_to_their_face_ids: font not found at {}",
                path.display()
            );
            return;
        }
        let core = [stack_font(0, "Core", Tier::Core, &path)];
        let ext = [stack_font(30, "Ext", Tier::Ext, &path)];
        let base = build_base_database(&core, &[], &ext);

        let bytes = ms_fonts::bytes(&core[0]).expect("the fixture must be readable");
        let ids = base
            .resident
            .get(&super::resident_key(bytes))
            .expect("a resident buffer must be indexed by its address and length");
        assert_eq!(ids.len(), 1, "the fixture ships exactly one face");
        assert!(
            base.db.face(ids[0]).is_some(),
            "an indexed face id must resolve in the database it came from"
        );
        // `ext` is stored as a PATH, so it is deliberately NOT indexed: its bytes are
        // not resident and mapping them here would defeat the on-demand contract.
        assert_eq!(
            base.resident.len(),
            1,
            "only the resident tiers may appear in the index"
        );
    }

    /// The generic families of a fresh base must point at a face that EXISTS, so the
    /// pool's pristine-default restore cannot resurrect fontdb's "Arial" placeholder.
    #[test]
    fn generic_families_point_at_the_first_core_face() {
        let path = fixture_font_path();
        if !path.exists() {
            eprintln!(
                "skipping generic_families_point_at_the_first_core_face: font not found at {}",
                path.display()
            );
            return;
        }
        let core = [stack_font(0, "Core", Tier::Core, &path)];
        let db = build_base_database(&core, &[], &[]).db;

        for family in [
            fontdb::Family::SansSerif,
            fontdb::Family::Serif,
            fontdb::Family::Monospace,
            fontdb::Family::Cursive,
            fontdb::Family::Fantasy,
        ] {
            assert_eq!(db.family_name(&family), "Core");
        }
    }

    /// Lists the font files of one tier of the SHIPPED bundle, in `NN-` order.
    ///
    /// Deliberately independent of `ms_fonts::stack()`: that manifest is resolved
    /// from the process working directory, which for a test binary is its package
    /// root, so it never finds the repository bundle (see `test_bundle`).
    fn shipped_tier_paths(tier: Tier) -> Vec<PathBuf> {
        test_bundle::tier_paths(tier)
    }

    /// Every family name the static tables promise must actually be shipped in
    /// `fonts/ui`, and each tier must be stored the way its size demands.
    ///
    /// This is the drift guard for [`SCRIPT_FAMILIES`]/[`FORBIDDEN_FAMILIES`]: those
    /// names are matched by cosmic-text against the `name` table of the bundled
    /// files, so a renamed or removed file silently disables a whole writing system.
    /// Matching semantics are cosmic-text's own — ANY entry of `FaceInfo::families`
    /// counts (`cosmic-text-0.14.2/src/font/fallback/mod.rs:267-273`).
    #[test]
    fn the_shipped_bundle_backs_every_family_the_fallback_tables_name() {
        let core = shipped_tier_paths(Tier::Core);
        let bold = shipped_tier_paths(Tier::Bold);
        let ext = shipped_tier_paths(Tier::Ext);
        if core.is_empty() {
            eprintln!(
                "skipping the_shipped_bundle_backs_every_family_the_fallback_tables_name: \
                 fonts/ui/core is not present next to this checkout"
            );
            return;
        }

        // The family names carried here are irrelevant to `build_base_database` (it
        // reads the `name` table itself through fontdb); only the paths and tiers are.
        let describe = |paths: &[PathBuf], tier: Tier| -> Vec<StackFont> {
            paths
                .iter()
                .map(|path| stack_font(0, "unused", tier, path))
                .collect()
        };
        let base = build_base_database(
            &describe(&core, Tier::Core),
            &describe(&bold, Tier::Bold),
            &describe(&ext, Tier::Ext),
        );
        let db = base.db;
        assert_eq!(
            db.len(),
            core.len() + bold.len() + ext.len(),
            "every shipped file must contribute exactly one face"
        );

        let shipped: HashSet<&str> = db
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.as_str()))
            .collect();
        let promised = FORBIDDEN_FAMILIES
            .iter()
            .copied()
            .chain(SCRIPT_FAMILIES.iter().flat_map(|(_, names)| names.iter().copied()));
        for name in promised {
            assert!(
                shipped.contains(name),
                "the fallback tables name '{name}', which fonts/ui does not ship"
            );
        }

        // Tier storage: resident tiers share the `'static` bytes, `ext` stays a path
        // so its ~80 MB are mapped only when a glyph needs them.
        let resident = core.len() + bold.len();
        for (index, face) in db.faces().enumerate() {
            if index < resident {
                assert!(
                    matches!(face.source, fontdb::Source::Binary(_)),
                    "core/bold face {index} must come from the shared 'static bytes"
                );
            } else {
                assert!(
                    matches!(face.source, fontdb::Source::File(_)),
                    "ext face {index} must stay a path"
                );
            }
        }

        // The bold file shares its family name with its regular sibling on purpose,
        // so only the WEIGHT can tell them apart.
        for family in db
            .faces()
            .filter(|face| matches!(face.source, fontdb::Source::Binary(_)))
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect::<BTreeSet<String>>()
        {
            let weights: Vec<u16> = db
                .faces()
                .filter(|face| face.families.iter().any(|(name, _)| *name == family))
                .map(|face| face.weight.0)
                .collect();
            let mut distinct = weights.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert_eq!(
                distinct.len(),
                weights.len(),
                "faces sharing family '{family}' must differ by weight"
            );
        }
    }

    /// The render `FontSystem` must expose the bundled base and nothing else — never
    /// the operating system's font database.
    ///
    /// Asserted twice, because neither half is sufficient on its own:
    /// - against the PRODUCTION constructor, whose manifest a test binary cannot
    ///   resolve (its working directory is the package root). What that half still
    ///   proves is the important negative: with no stack the database is EMPTY, so
    ///   nothing fell back to `FontSystem::new()` and scanned the operator's fonts.
    /// - against the SHIPPED bundle through `test_bundle`, so the positive half —
    ///   "exactly the bundled files, each contributing one face" — is measured
    ///   against a real 49-file database instead of comparing 0 with 0.
    #[test]
    fn the_render_font_system_never_loads_system_fonts() {
        let system = new_render_font_system();
        let expected = ms_fonts::stack().map_or(0, |stack| {
            stack.core().len() + stack.bold().len() + stack.ext().len()
        });
        assert_eq!(
            system.db().len(),
            expected,
            "the render database must hold exactly the bundled stack"
        );
        assert_eq!(system.locale(), "en-US", "the render locale must be fixed");

        let Some(shipped) = test_bundle::stack() else {
            eprintln!(
                "skipping the bundle-backed half of the_render_font_system_never_loads_system_fonts: \
                 fonts/ui/core is not present next to this checkout"
            );
            return;
        };
        let bundled = test_bundle::font_system().expect("the shipped stack was just resolved");
        assert_eq!(
            bundled.db().len(),
            shipped.file_count(),
            "a bundle-backed render database must hold exactly the shipped files"
        );
        assert_eq!(bundled.locale(), "en-US", "the render locale must be fixed");
        // Every face must come from a file of the bundle. An OS scan would show up
        // here as a face whose path is not one of the shipped ones.
        let shipped_paths: HashSet<&std::path::Path> = shipped
            .core
            .iter()
            .chain(shipped.bold.iter())
            .chain(shipped.ext.iter())
            .map(|font| font.path.as_path())
            .collect();
        for face in bundled.db().faces() {
            match &face.source {
                fontdb::Source::File(path) => assert!(
                    shipped_paths.contains(path.as_path()),
                    "face '{}' comes from '{}', which fonts/ui does not ship",
                    face.post_script_name,
                    path.display()
                ),
                // The resident tiers are registered from the shared `'static` bytes,
                // which carry no path; their count is pinned by the length assert
                // above and their storage by the drift guard.
                fontdb::Source::Binary(_) | fontdb::Source::SharedFile(_, _) => {}
            }
        }
    }

    /// Every fallback face of the shipped bundle must be registered at the weight the
    /// weight-filtered fallback passes require, and the `bold` tier must keep its own.
    ///
    /// Two shipped files declare `usWeightClass = 500` (`90-HanaMinB`, `91-HanaMinA`);
    /// left at that weight they are unreachable at any normal-weight run and the
    /// codepoints only they ship render as tofu (see [`FALLBACK_FACE_WEIGHT`]).
    #[test]
    fn fallback_faces_are_registered_at_the_fallback_weight() {
        let Some(shipped) = test_bundle::stack() else {
            eprintln!(
                "skipping fallback_faces_are_registered_at_the_fallback_weight: fonts/ui is not \
                 present next to this checkout"
            );
            return;
        };
        let system = test_bundle::font_system().expect("the shipped stack was just resolved");
        let bold_families: HashSet<&str> = shipped
            .bold
            .iter()
            .map(|font| font.family_name)
            .collect();

        for face in system.db().faces() {
            let is_bold_tier = face.weight != super::FALLBACK_FACE_WEIGHT;
            if is_bold_tier {
                assert!(
                    face.families
                        .iter()
                        .any(|(name, _)| bold_families.contains(name.as_str())),
                    "face '{}' keeps weight {} but is not a bold-tier face",
                    face.post_script_name,
                    face.weight.0
                );
            }
        }
        // The two 500-weight files must have been re-pinned, not skipped.
        for post_script_prefix in ["HanaMinA", "HanaMinB"] {
            let face = system
                .db()
                .faces()
                .find(|face| face.families.iter().any(|(name, _)| name == post_script_prefix));
            let Some(face) = face else {
                panic!("the bundle must still ship {post_script_prefix}");
            };
            assert_eq!(
                face.weight,
                super::FALLBACK_FACE_WEIGHT,
                "{post_script_prefix} must be registered at the fallback weight"
            );
        }
    }

    /// Re-pinning a face's weight must not move it: face-id order IS the order of
    /// cosmic-text's final whole-database pass, i.e. the curated `NN-` fallback order.
    #[test]
    fn base_face_order_follows_the_registration_order() {
        let path = fixture_font_path();
        if !path.exists() {
            eprintln!("skipping base_face_order_follows_the_registration_order: fixture missing");
            return;
        }
        let Some(shipped) = test_bundle::stack() else {
            eprintln!(
                "skipping base_face_order_follows_the_registration_order: fonts/ui is not present \
                 next to this checkout"
            );
            return;
        };
        let db = build_base_database(&shipped.core, &shipped.bold, &shipped.ext).db;
        // `ext` is the only tier stored by path, and it is registered last, so its
        // paths must appear in the database in exactly the order they were given —
        // including the two faces that were removed and pushed back re-weighted.
        let ext_paths: Vec<&std::path::Path> = db
            .faces()
            .filter_map(|face| match &face.source {
                fontdb::Source::File(path) => Some(path.as_path()),
                fontdb::Source::Binary(_) | fontdb::Source::SharedFile(_, _) => None,
            })
            .collect();
        let expected: Vec<&std::path::Path> =
            shipped.ext.iter().map(|font| font.path.as_path()).collect();
        assert_eq!(
            ext_paths, expected,
            "re-registering a face must keep its position in the fallback order"
        );
    }

    /// Codepoint of a rare Han ideograph (CJK Extension F) that ONLY the rare-plane
    /// fonts cover, so reaching it proves the script chain — not the core CJK face —
    /// did the work.
    const RARE_HAN_CHAR: char = '\u{2CEA1}';

    /// Codepoint of a writing system the bundle covers ONLY from the forbidden
    /// `HanaMinA` face (Lisu), i.e. one that the final whole-database pass can never
    /// reach.
    const HANAMIN_ONLY_CHAR: char = '\u{A4D0}';

    /// A rare Han ideograph must be reachable at the weight the renderer actually
    /// shapes with.
    ///
    /// This pins the reason `pipeline::synthesized_bold_params` exists. cosmic-text
    /// filters the script and common passes to `font_weight_diff == 0`
    /// (`font/fallback/mod.rs:410-417`) and every rare-plane font in the bundle is a
    /// 400-weight face, so a run shaped at `Weight::BOLD` cannot enter the Han chain
    /// at all — and the final whole-database pass excludes those fonts by design.
    /// The renderer therefore must not put a weight into the attrs that the selected
    /// family cannot serve; when it does not, the chain stays reachable.
    #[test]
    fn the_rare_han_chain_is_reachable_at_the_shaped_weight() {
        let Some(mut system) = test_bundle::font_system() else {
            eprintln!(
                "skipping the_rare_han_chain_is_reachable_at_the_shaped_weight: fonts/ui is not \
                 present next to this checkout"
            );
            return;
        };
        let text = RARE_HAN_CHAR.to_string();
        let attrs = Attrs::new().family(cosmic_text::Family::Name("Noto Sans"));

        let normal = test_bundle::shaped_glyphs(&mut system, &text, &attrs);
        assert!(
            normal.iter().all(|(glyph_id, _)| *glyph_id != 0),
            "a rare Han ideograph must not shape to .notdef at the default weight, got {normal:?}"
        );
        assert!(
            normal
                .iter()
                .any(|(_, family)| family.starts_with("Plangothic") || family.starts_with("HanaMin")),
            "the rare ideograph must come from a rare-plane font, got {normal:?}"
        );

        // The residual this pins: at a REAL bold weight the same character is
        // unreachable, because the script pass admits only `weight_diff == 0`
        // candidates and the bundle ships no bold rare-plane face. That is exactly
        // why a real bold request on a family without a Bold face is degraded to
        // faux bold instead of being sent to the shaper (see `pipeline.rs`).
        let bold = test_bundle::shaped_glyphs(
            &mut system,
            &text,
            &attrs.clone().weight(cosmic_text::Weight::BOLD),
        );
        assert!(
            bold.iter().any(|(glyph_id, _)| *glyph_id == 0),
            "the documented residual changed: a real bold weight now reaches the rare-plane \
             chain ({bold:?}); re-check `pipeline::synthesized_bold_params` and the MODULE_README"
        );
    }

    /// The hentaigana face must serve historic kana, and must do so BEFORE a
    /// rare-plane font is mapped — that is the whole reason it is in the chain.
    ///
    /// Also the guard for the file swap that replaced the variable-weight hentaigana
    /// file: the variable one registered at weight 200 and was therefore invisible to
    /// the weight-filtered script pass (see [`FALLBACK_FACE_WEIGHT`]).
    #[test]
    fn historic_kana_come_from_the_hentaigana_face() {
        let Some(mut system) = test_bundle::font_system() else {
            eprintln!(
                "skipping historic_kana_come_from_the_hentaigana_face: fonts/ui is not present \
                 next to this checkout"
            );
            return;
        };
        let attrs = Attrs::new().family(cosmic_text::Family::Name("Noto Sans"));
        // U+1B002 lives in the Kana Supplement block, which the core CJK face does
        // not cover.
        let shaped = test_bundle::shaped_glyphs(&mut system, "\u{1B002}", &attrs);
        assert!(
            shaped.iter().all(|(glyph_id, _)| *glyph_id != 0),
            "historic kana must not render as tofu, got {shaped:?}"
        );
        assert!(
            shaped
                .iter()
                .all(|(_, family)| family == "Noto Serif Hentaigana"),
            "historic kana must come from the purpose-built hentaigana face, not a rare-plane \
             font, got {shaped:?}"
        );
    }

    /// A writing system the bundle covers only from a FORBIDDEN font must be
    /// reachable through its script chain.
    ///
    /// Without an entry in `SCRIPT_FAMILIES` these 347 codepoints are tofu with no
    /// diagnostic: the final whole-database pass skips forbidden families
    /// (`font/fallback/mod.rs:449-452`), so nothing else can ever reach them.
    #[test]
    fn a_script_only_a_forbidden_font_covers_is_reachable() {
        let Some(mut system) = test_bundle::font_system() else {
            eprintln!(
                "skipping a_script_only_a_forbidden_font_covers_is_reachable: fonts/ui is not \
                 present next to this checkout"
            );
            return;
        };
        let attrs = Attrs::new().family(cosmic_text::Family::Name("Noto Sans"));
        let shaped =
            test_bundle::shaped_glyphs(&mut system, &HANAMIN_ONLY_CHAR.to_string(), &attrs);
        assert!(
            shaped.iter().all(|(glyph_id, _)| *glyph_id != 0),
            "U+A4D0 (Lisu) is shipped in HanaMinA and must not render as tofu, got {shaped:?}"
        );
        assert!(
            shaped.iter().any(|(_, family)| family == "HanaMinA"),
            "the Lisu chain must resolve to HanaMinA, got {shaped:?}"
        );
    }

    /// Every script chain the tables name must actually be able to draw the
    /// codepoints it was added for, at the weight the renderer shapes with.
    ///
    /// A sample per writing system whose bundled coverage is EXCLUSIVE to a forbidden
    /// font: those are the entries that carry their own reachability, so a chain that
    /// silently stops working shows up here as tofu. Each sample is a codepoint the
    /// shipped `cmap` tables put in a forbidden font and nowhere else, so the asserted
    /// family also proves the chain — not the final whole-database pass — did the work.
    #[test]
    fn the_rare_plane_script_chains_cover_their_own_codepoints() {
        let Some(mut system) = test_bundle::font_system() else {
            eprintln!(
                "skipping the_rare_plane_script_chains_cover_their_own_codepoints: fonts/ui is \
                 not present next to this checkout"
            );
            return;
        };
        let attrs = Attrs::new().family(cosmic_text::Family::Name("Noto Sans"));
        for (script, sample) in [
            // Served by HanaMinA (+ Plangothic P2 for a few codepoints).
            (Script::Runic, '\u{16A0}'),
            (Script::Old_Turkic, '\u{10C00}'),
            (Script::Carian, '\u{102A0}'),
            (Script::Lisu, '\u{A4D0}'),
            (Script::Old_Italic, '\u{10300}'),
            (Script::Lycian, '\u{10280}'),
            (Script::Lydian, '\u{10920}'),
            (Script::Coptic, '\u{2C80}'),
            (Script::Linear_B, '\u{10000}'),
            // Served by the Plangothic planes.
            (Script::Tangut, '\u{17000}'),
            (Script::Khitan_Small_Script, '\u{18B00}'),
            (Script::SignWriting, '\u{1D800}'),
            (Script::Nushu, '\u{1B170}'),
            (Script::Cuneiform, '\u{1236F}'),
            (Script::Mende_Kikakui, '\u{1E800}'),
            (Script::Duployan, '\u{1BC00}'),
            (Script::Old_Hungarian, '\u{10C80}'),
            (Script::Cypro_Minoan, '\u{12F90}'),
            (Script::Siddham, '\u{11580}'),
            (Script::Tangsa, '\u{16A70}'),
            (Script::Kawi, '\u{11F00}'),
            (Script::Tirhuta, '\u{11480}'),
            (Script::Modi, '\u{11600}'),
            (Script::Dives_Akuru, '\u{11900}'),
            (Script::Zanabazar_Square, '\u{11A00}'),
            (Script::Vithkuqi, '\u{10570}'),
            (Script::Khudawadi, '\u{112B0}'),
            (Script::Nandinagari, '\u{119A0}'),
            (Script::Caucasian_Albanian, '\u{10530}'),
            (Script::Sogdian, '\u{10F30}'),
            (Script::Nag_Mundari, '\u{1E4D0}'),
            (Script::Glagolitic, '\u{2C2F}'),
            (Script::Old_Sogdian, '\u{10F00}'),
            (Script::Mahajani, '\u{11150}'),
            (Script::Toto, '\u{1E290}'),
            (Script::Psalter_Pahlavi, '\u{10B80}'),
            (Script::Chorasmian, '\u{10FB0}'),
            (Script::Old_Uyghur, '\u{10F70}'),
            (Script::Makasar, '\u{11EE0}'),
            (Script::Elymaic, '\u{10FE0}'),
            (Script::Egyptian_Hieroglyphs, '\u{1342F}'),
            (Script::Ahom, '\u{11740}'),
            (Script::Beria_Erfe, '\u{16EA0}'),
            (Script::Brahmi, '\u{11070}'),
            (Script::Garay, '\u{10D59}'),
            (Script::Gurung_Khema, '\u{16100}'),
            (Script::Kirat_Rai, '\u{16D40}'),
            (Script::Ol_Onal, '\u{1E5D0}'),
            (Script::Sidetic, '\u{10940}'),
            (Script::Sunuwar, '\u{11BC0}'),
            (Script::Tai_Yo, '\u{1E6C0}'),
            (Script::Todhri, '\u{105C0}'),
            (Script::Tolong_Siki, '\u{11DB0}'),
            (Script::Newa, '\u{1145A}'),
            (Script::Tagalog, '\u{170D}'),
            (Script::Kharoshthi, '\u{10A34}'),
            (Script::Khojki, '\u{1123F}'),
            (Script::Limbu, '\u{191D}'),
            (Script::Sharada, '\u{111CE}'),
            (Script::Syloti_Nagri, '\u{A82C}'),
            (Script::Kaithi, '\u{110C2}'),
            (Script::Chakma, '\u{11147}'),
            (Script::Takri, '\u{116B9}'),
        ] {
            let shaped = test_bundle::shaped_glyphs(&mut system, &sample.to_string(), &attrs);
            assert!(
                shaped.iter().all(|(glyph_id, _)| *glyph_id != 0),
                "{script:?}: U+{:04X} must not render as tofu, got {shaped:?}",
                u32::from(sample)
            );
            assert!(
                shaped
                    .iter()
                    .any(|(_, family)| FORBIDDEN_FAMILIES.contains(&family.as_str())),
                "{script:?}: U+{:04X} is shipped only in a forbidden font, so its script chain \
                 must be what drew it, got {shaped:?}",
                u32::from(sample)
            );
        }
    }

    /// Appending a rare-plane font to an EXISTING script chain must not displace the
    /// profile face: ordinary text of the script keeps coming from its
    /// `30-NotoSans<Script>` file, and only what that file genuinely lacks reaches the
    /// rare-plane font behind it.
    ///
    /// The selected family is the core `Noto Sans`, which covers none of these scripts
    /// (except Cyrillic, where it IS the profile face), so both lookups really go
    /// through the script chain instead of being served by the selected font.
    #[test]
    fn appending_a_rare_plane_font_keeps_the_profile_face_first() {
        let Some(mut system) = test_bundle::font_system() else {
            eprintln!(
                "skipping appending_a_rare_plane_font_keeps_the_profile_face_first: fonts/ui is \
                 not present next to this checkout"
            );
            return;
        };
        let attrs = Attrs::new().family(cosmic_text::Family::Name("Noto Sans"));
        // (ordinary codepoint, its expected profile family, codepoint the profile file
        // does not ship but `Plangothic P2` does).
        //
        // Every ordinary sample is absent from the selected core `Noto Sans`, so only
        // the script chain can resolve it — except Cyrillic, whose profile face IS the
        // core font and which therefore checks that the appended rare-plane font does
        // not take a codepoint the selected font already covers. Where the bundle
        // allows it, the sample is one `Plangothic P2` ALSO ships (Arabic, Cyrillic,
        // Devanagari, Mongolian, Myanmar) — there the assertion proves the ORDER, not
        // merely that a chain exists.
        for (ordinary, profile_family, rare) in [
            ('\u{0627}', "Noto Sans Arabic", '\u{0870}'),
            ('\u{1B05}', "Noto Sans Balinese", '\u{1B4C}'),
            ('\u{1C81}', "Noto Sans", '\u{1C89}'),
            ('\u{A8FE}', "Noto Sans Devanagari", '\u{11B00}'),
            ('\u{1200}', "Noto Sans Ethiopic", '\u{1E7E0}'),
            ('\u{0C95}', "Noto Sans Kannada", '\u{0CDC}'),
            ('\u{0E81}', "Noto Sans Lao", '\u{0E86}'),
            ('\u{1878}', "Noto Sans Mongolian", '\u{180F}'),
            ('\u{A9F0}', "Noto Sans Myanmar", '\u{116D0}'),
            ('\u{0710}', "Noto Sans Syriac", '\u{0860}'),
            ('\u{0B95}', "Noto Sans Tamil", '\u{11FC0}'),
            ('\u{0C15}', "Noto Sans Telugu", '\u{0C3C}'),
        ] {
            let shaped = test_bundle::shaped_glyphs(&mut system, &ordinary.to_string(), &attrs);
            assert!(
                shaped
                    .iter()
                    .all(|(_, family)| family.as_str() == profile_family),
                "U+{:04X} must still come from '{profile_family}', got {shaped:?}",
                u32::from(ordinary)
            );

            let shaped = test_bundle::shaped_glyphs(&mut system, &rare.to_string(), &attrs);
            assert!(
                shaped.iter().all(|(glyph_id, _)| *glyph_id != 0),
                "U+{:04X} must not render as tofu, got {shaped:?}",
                u32::from(rare)
            );
            assert!(
                shaped
                    .iter()
                    .any(|(_, family)| family.as_str() == "Plangothic P2"),
                "U+{:04X} is shipped only in Plangothic P2, so the appended chain entry must be \
                 what drew it, got {shaped:?}",
                u32::from(rare)
            );
        }
    }

    /// Codepoints one shipped font maps to a real glyph, read straight from its `cmap`.
    ///
    /// Returns an empty set for a file that cannot be read or parsed; the caller's
    /// assertions then fail on the missing coverage instead of on an I/O error, which
    /// is the same outcome a missing font has in production.
    fn mapped_codepoints(path: &Path) -> HashSet<u32> {
        let Ok(data) = std::fs::read(path) else {
            return HashSet::new();
        };
        let Some(font) = swash::FontRef::from_index(data.as_slice(), 0) else {
            return HashSet::new();
        };
        let mut mapped = HashSet::new();
        font.charmap().enumerate(|codepoint, glyph_id| {
            if glyph_id != 0 {
                mapped.insert(codepoint);
            }
        });
        mapped
    }

    /// EXHAUSTIVE drift guard for the forbidden-font entries of [`SCRIPT_FAMILIES`].
    ///
    /// Every codepoint the bundle covers ONLY inside a [`FORBIDDEN_FAMILIES`] font is
    /// invisible to cosmic-text's final whole-database pass, so the ONLY thing that can
    /// draw it is a script chain naming that font. This walks the shipped `cmap` tables
    /// and asserts exactly that, which is what keeps a newly bundled font — or a
    /// Plangothic update that adds a block — from silently shipping tofu that the
    /// interface (no weight filter, no forbidden list) happily displays.
    ///
    /// `Common`, `Inherited`, `Latin` and `Unknown` are skipped because cosmic-text
    /// never asks for them (`cosmic-text-0.14.2/src/shape.rs:249-257`): such characters
    /// are looked up through the OTHER scripts of their run, so no entry here could
    /// change their fate. See the note on [`SCRIPT_FAMILIES`].
    #[test]
    fn every_forbidden_only_codepoint_is_reachable_through_a_script_chain() {
        let Some(shipped) = test_bundle::stack() else {
            eprintln!(
                "skipping every_forbidden_only_codepoint_is_reachable_through_a_script_chain: \
                 fonts/ui is not present next to this checkout"
            );
            return;
        };

        // Split the bundle in two: what the final whole-database pass can reach, and
        // the per-family coverage of the fonts it cannot.
        let mut forbidden_coverage: Vec<(&'static str, HashSet<u32>)> = Vec::new();
        let mut reachable_without_forbidden: HashSet<u32> = HashSet::new();
        for font in shipped
            .core
            .iter()
            .chain(shipped.bold.iter())
            .chain(shipped.ext.iter())
        {
            let mapped = mapped_codepoints(font.path.as_path());
            if FORBIDDEN_FAMILIES.contains(&font.family_name) {
                forbidden_coverage.push((font.family_name, mapped));
            } else {
                reachable_without_forbidden.extend(mapped);
            }
        }
        assert_eq!(
            forbidden_coverage.len(),
            FORBIDDEN_FAMILIES.len(),
            "every forbidden family must be backed by a shipped file"
        );

        // Codepoint -> the forbidden families that ship it, for codepoints nothing else
        // ships. Built per codepoint (not per font) because a chain naming ANY ONE of
        // the covering families is enough.
        let mut only_forbidden: HashMap<u32, Vec<&'static str>> = HashMap::new();
        for (family, mapped) in &forbidden_coverage {
            for codepoint in mapped {
                if reachable_without_forbidden.contains(codepoint) {
                    continue;
                }
                only_forbidden.entry(*codepoint).or_default().push(family);
            }
        }

        let chains: HashMap<Script, &[&str]> = SCRIPT_FAMILIES.iter().copied().collect();
        // Failures grouped by writing system: count plus the lowest offending
        // codepoint, so the message names the entry to add rather than one character.
        let mut unreachable: BTreeMap<String, (usize, u32)> = BTreeMap::new();
        for (codepoint, families) in &only_forbidden {
            let Some(character) = char::from_u32(*codepoint) else {
                continue;
            };
            let script = character.script();
            if matches!(
                script,
                Script::Common | Script::Inherited | Script::Latin | Script::Unknown
            ) {
                continue;
            }
            let reachable = chains.get(&script).is_some_and(|chain| {
                chain.iter().any(|family| families.contains(family))
            });
            if !reachable {
                let entry = unreachable
                    .entry(format!("{script:?}"))
                    .or_insert((0, *codepoint));
                entry.0 += 1;
                entry.1 = entry.1.min(*codepoint);
            }
        }

        assert!(
            unreachable.is_empty(),
            "the bundle covers these writing systems only inside a forbidden font, and no \
             SCRIPT_FAMILIES chain names that font, so they render as tofu: {}",
            unreachable
                .iter()
                .map(|(script, (count, sample))| format!("{script} ({count}, e.g. U+{sample:04X})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

