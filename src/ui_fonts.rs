/*
File: src/ui_fonts.rs

Purpose:
Single owner of the application UI font stack. Installs the same bundled chain
(`fonts/ui`) into every egui context the app creates — studio window, launcher,
installer, updater and the small startup prompt windows — so no window falls back
to the bare egui default fonts (tofu instead of CJK).

Main responsibilities:
- take the bundled stack from `ms_fonts::stack()` (the process manifest) and its bytes
  from `ms_fonts::bytes()`, so every font file is read once per process and epaint
  borrows it through `FontData::from_static` instead of keeping a second copy;
- resolve a title-local `fonts/ui` override on its own, because the process manifest is
  deliberately project-independent (see `ResolvedStack`), and VALIDATE its files: they come
  from an arbitrary opened project, while epaint parses everything it is given eagerly and
  panics on a parse failure (see `validate_font_bytes`);
- register every file through `egui::Context::add_font` off the GUI thread, in an order
  that makes the resulting fallback chain match the `NN-` filename ordering in the text
  families (`Monospace` gets the core fonts as a reversed trailing fallback — see
  `core_families`);
- keep the large `ext/` tier out of memory until a text actually needs a glyph the core
  chain lacks (`ensure_covers`);
- fall back to a short list of well-known system fonts when nothing is bundled.

Key types:
- `Tier`: how much of the stack to install (`Core` for small windows, `Full` for the studio).

Key functions:
- `install` / `install_with_roots`: fire-and-forget installation; both return immediately.
- `ensure_covers`: on-demand installation of the extended tier, called from the UI while
  a frame is running.

Key constants:
- `BUBBLE_TEXT_FAMILY_NAME`: egui family used by canvas bubble text.
- `UI_BOLD_FAMILY_NAME`: egui family used wherever the UI asks for a bold face.

Notes:
- This module must never call `Context::set_fonts`: that REPLACES all font definitions
  (egui-0.35.0/src/context.rs:2038 + :535-540) and would drop the families other
  subsystems register through `add_font` (`typing-panel-combo-font-*`,
  `typing-editor-font-*`), which then panics in epaint when they are used again.
- The LOADER must never call `Context::fonts`/`Context::fonts_mut`: they panic before the
  first frame (egui-0.35.0/src/context.rs:1037, :1047), and the loader starts from a
  `run_native` constructor closure, i.e. before the first frame. `ensure_covers` may use
  them because it is called from UI code, from the GUI thread, during a frame.
- `add_font` is purely additive and only touches `Memory` (context.rs:2061-2085), so it
  is safe both from a worker thread and before the first frame.
*/

use eframe::egui;
use std::path::PathBuf;

/// egui font family that carries the wide unicode chain used for canvas bubble text.
///
/// Declared here so the family name has exactly one definition; `canvas::helpers`
/// re-exports it for the canvas call sites.
pub const BUBBLE_TEXT_FAMILY_NAME: &str = "canvas-bubble-unicode";

/// egui font family used wherever the UI wants a bold face.
///
/// The chain is bold-first with the regular core fonts kept as a fallback, so glyphs
/// missing from the bold faces still render (in regular weight) instead of as tofu.
pub const UI_BOLD_FAMILY_NAME: &str = "system-ui-sans-bold";

/// How much of the bundled font stack a window needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    /// Only `core/` + `bold/` (~19 MB): latin/cyrillic/CJK plus symbols.
    ///
    /// Used by the launcher, installer, updater and the small startup prompt windows,
    /// which show short UI strings and never display chapter text.
    Core,
    /// `core/` + `bold/`, with `ext/` (~80 MB) ARMED but not installed.
    ///
    /// Studio window only: chapter text can contain any script, but most chapters need
    /// none of the extended tier, so it is only installed once [`ensure_covers`] sees a
    /// character the core chain cannot draw.
    Full,
}

/// Installs the bundled UI font stack into `ctx` without blocking the caller.
///
/// Intended to be called exactly once per egui context, from the `run_native`
/// constructor closure. All disk work happens on a spawned worker thread; the call
/// returns immediately and the fonts appear a few frames later.
///
/// Failures are never fatal: a missing `fonts/ui`, an unreadable file, or a failing
/// thread spawn is logged and degrades to the system-font fallback or, ultimately, to
/// the egui default fonts.
pub fn install(ctx: &egui::Context, tier: Tier) {
    install_with_roots(ctx, tier, &[]);
}

/// Same as [`install`], plus title-local roots that override the bundled stack.
///
/// Each entry of `extra_roots` is a directory that may contain a `fonts/ui` subtree (the
/// chapter/title folder of an opened project); the first one that actually yields core
/// fonts wins and REPLACES the bundled stack for this window. Duplicated roots are probed
/// once, and a root without core fonts is skipped rather than allowed to shadow the
/// bundled chain.
///
/// The override applies to the UI only: the process manifest (`ms_fonts::stack`) that the
/// text renderer also uses stays project-independent, so an opened project can never
/// change how a finished render looks (`dev-docs/unicode_base_font_plan.md`, decision 2).
#[cfg(not(target_arch = "wasm32"))]
pub fn install_with_roots(ctx: &egui::Context, tier: Tier, extra_roots: &[PathBuf]) {
    // `egui::Context` is `Arc<RwLock<..>>` (egui-0.35.0/src/context.rs:710) and therefore
    // Send + Sync: the worker can call `add_font` on this clone directly, with no channel
    // and no GUI-thread polling.
    let ctx = ctx.clone();
    let roots = extra_roots.to_vec();
    if let Err(err) = std::thread::Builder::new()
        .name("ui-fonts".to_owned())
        .spawn(move || desktop::install_blocking(&ctx, tier, &roots))
    {
        crate::runtime_log::log_warn(format!(
            "[ui_fonts] failed to spawn the font loader thread: {err}; \
             keeping egui default fonts"
        ));
    }
}

/// wasm no-op: there is no bundled `fonts/ui` folder next to a web build and no
/// filesystem to read it from, so the web entry point installs its own fonts.
///
/// `ctx` is intentionally unused here — the whole point of this variant is that it
/// touches no context state at all.
#[cfg(target_arch = "wasm32")]
pub fn install_with_roots(_ctx: &egui::Context, tier: Tier, extra_roots: &[PathBuf]) {
    crate::runtime_log::log_info(format!(
        "[ui_fonts] wasm build: bundled font stack skipped (tier={tier:?}, extra roots: {})",
        extra_roots.len()
    ));
}

/// Installs the extended (`ext/`) tier if `text` needs a glyph the installed fonts lack.
///
/// The tier is ~80 MB and most chapters need none of it, so [`Tier::Full`] only ARMS it
/// and this function is what actually loads it — on the first character that the bubble
/// font chain cannot draw. Reading and registering the files happens on a worker thread;
/// the call itself never blocks and the fonts appear a few frames later.
///
/// Idempotent, and cheap once the question is settled: after the tier has been claimed
/// (or when there is none to install, e.g. in a `Tier::Core` window) the call is a single
/// atomic load. While the tier is still armed the cost is one pass over the non-ASCII
/// characters of `text`, each a lookup in epaint's per-family `face_cache`
/// (epaint-0.35.0/src/text/fonts.rs:635) — a pure-ASCII string does not even get that far.
///
/// # Panics
/// Must be called from the GUI thread WHILE A FRAME IS RUNNING: it reads the installed
/// fonts through `Context::fonts_mut`, which panics before the first frame
/// (egui-0.35.0/src/context.rs:1047). Never call it from a background thread or from a
/// `run_native` constructor closure.
#[cfg(not(target_arch = "wasm32"))]
pub fn ensure_covers(ctx: &egui::Context, text: &str) {
    desktop::ensure_covers(ctx, text);
}

/// wasm no-op: there is no bundled `fonts/ui` folder next to a web build, so there is no
/// extended tier to defer — the web entry point installs its own fonts.
#[cfg(target_arch = "wasm32")]
pub fn ensure_covers(_ctx: &egui::Context, _text: &str) {}

/// Native implementation: filesystem probing plus the `add_font` ordering rules.
///
/// Kept in a target-gated private module so the wasm build does not carry (or warn
/// about) the disk-bound half of the module.
#[cfg(not(target_arch = "wasm32"))]
mod desktop {
    use super::{BUBBLE_TEXT_FAMILY_NAME, Tier, UI_BOLD_FAMILY_NAME};
    use crate::runtime_log;
    use eframe::egui;
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    use std::collections::HashSet;
    use std::fs;
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Mutex, MutexGuard};

    /// Font container extensions the loader accepts.
    const SUPPORTED_FONT_EXTENSIONS: [&str; 4] = ["otf", "ttf", "ttc", "otc"];

    /// Last-resort system font candidates, probed in order as `(regular, bold)` pairs.
    ///
    /// Used only when no bundled `fonts/ui` chain could be loaded. The list is the
    /// historical one from `MangaApp::ensure_fonts`: CJK-capable faces on the common
    /// Linux distributions, macOS and Windows.
    const SYSTEM_FONT_CANDIDATES: [(&str, Option<&str>); 6] = [
        (
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            Some("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc"),
        ),
        (
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            Some("/usr/share/fonts/truetype/noto/NotoSansCJK-Bold.ttc"),
        ),
        (
            "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
            Some("/usr/share/fonts/truetype/nanum/NanumGothicBold.ttf"),
        ),
        (
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            Some("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
        ),
        ("/System/Library/Fonts/AppleSDGothicNeo.ttc", None),
        (
            "C:\\Windows\\Fonts\\malgun.ttf",
            Some("C:\\Windows\\Fonts\\malgunbd.ttf"),
        ),
    ];

    /// Worker-thread body of [`super::install_with_roots`]: resolves the stack, reads it,
    /// and registers everything it found. Never panics and never propagates an error.
    ///
    /// For [`Tier::Full`] the extended tier is not installed here but ARMED, so it costs
    /// nothing until [`ensure_covers`] finds a character the core chain cannot draw.
    pub(super) fn install_blocking(ctx: &egui::Context, tier: Tier, extra_roots: &[PathBuf]) {
        let Some(resolved) = resolve_stack(extra_roots) else {
            runtime_log::log_warn(
                "[ui_fonts] no fonts/ui directory with usable core fonts found; falling back \
                 to system fonts",
            );
            install_system_fallback(ctx);
            return;
        };

        let core_loaded = install_fonts(ctx, &plan_stage(Stage::Core, &resolved.core));
        if core_loaded == 0 {
            runtime_log::log_warn(format!(
                "[ui_fonts] failed to read any core font from {}; falling back to system fonts",
                resolved.root.display()
            ));
            install_system_fallback(ctx);
            return;
        }

        // Bold faces are inserted after core with `Highest`, so they land in front of the
        // core fonts inside the bold family while core stays as the fallback for glyphs the
        // bold faces do not cover.
        let bold_loaded = install_fonts(ctx, &plan_stage(Stage::Bold, &resolved.bold));

        runtime_log::log_info(format!(
            "[ui_fonts] installed {core_loaded} core + {bold_loaded} bold font(s) from {}",
            resolved.root.display()
        ));
        ctx.request_repaint();

        match tier {
            Tier::Core => {}
            Tier::Full => arm_ext_tier(&resolved),
        }
    }

    /// Arms the deferred extended tier with the plan [`ensure_covers`] will execute.
    ///
    /// Logs and does nothing when the resolved stack ships no `ext/` fonts, or when the
    /// gate was already armed (only the studio window arms it, and only once).
    fn arm_ext_tier(resolved: &ResolvedStack) {
        let planned = resolved.ext.len();
        if EXT_GATE.arm(plan_stage(Stage::Ext, &resolved.ext)) {
            runtime_log::log_info(format!(
                "[ui_fonts] {planned} extended font(s) from {} armed for on-demand loading; \
                 they are installed the first time a text needs a glyph the core chain lacks",
                resolved.root.join(Stage::Ext.label()).display()
            ));
        } else {
            runtime_log::log_info(format!(
                "[ui_fonts] no extended tier to arm for {} ({planned} file(s) planned); \
                 rare scripts will render as tofu",
                resolved.root.display()
            ));
        }
    }

    /// One stage of the bundled chain; decides the family set and the walk direction.
    #[derive(Clone, Copy, Debug)]
    enum Stage {
        /// `fonts/ui/core` (or the legacy flat layout): the base chain every window gets.
        Core,
        /// `fonts/ui/bold`: bold faces for [`UI_BOLD_FAMILY_NAME`].
        Bold,
        /// `fonts/ui/ext`: extended scripts, studio window only and only on demand.
        Ext,
    }

    impl Stage {
        /// The `fonts/ui` tier this stage installs.
        ///
        /// The two enums are separate on purpose: `ms_fonts::Tier` says WHERE a font comes
        /// from, `Stage` says how this module registers it (families, walk direction).
        fn tier(self) -> ms_fonts::Tier {
            match self {
                Stage::Core => ms_fonts::Tier::Core,
                Stage::Bold => ms_fonts::Tier::Bold,
                Stage::Ext => ms_fonts::Tier::Ext,
            }
        }

        /// Name of the tier subdirectory, which doubles as the namespace of the egui font
        /// names (`ms-ui-<label>-<file name>`) and as the label used in the logs.
        fn label(self) -> &'static str {
            self.tier().dir_name()
        }

        /// Families this stage's fonts join, with their insertion position in each.
        fn families(self) -> Vec<InsertFontFamily> {
            match self {
                Stage::Core => core_families(),
                Stage::Bold => bold_families(),
                Stage::Ext => ext_families(),
            }
        }

        /// Whether the name-sorted file list is walked backwards when registering it.
        ///
        /// `FontPriority::Highest` is `fam.insert(0, ..)` (egui-0.35.0/src/context.rs:554),
        /// so a loop over ascending file names would REVERSE the chain — that was the bug
        /// where `NotoSerifHentaigana` ended up first and swallowed all latin text. Core and
        /// bold are `Highest`-driven and therefore walked backwards; `Ext` is appended with
        /// `Lowest` (`fam.push`, context.rs:555) and is walked in ascending order.
        fn walks_backwards(self) -> bool {
            match self {
                Stage::Core | Stage::Bold => true,
                Stage::Ext => false,
            }
        }
    }

    /// Where the bytes of one font come from.
    #[derive(Clone, Debug)]
    enum FontSource {
        /// A font of the process manifest (`ms_fonts::stack`), the normal case.
        ///
        /// Its bytes are read at most once per process and handed to epaint as
        /// `FontData::from_static`, which stores a `Cow::Borrowed`
        /// (epaint-0.35.0/src/text/fonts.rs:131-137). `from_owned` would instead keep the
        /// bytes TWICE — once in `FontDefinitions::font_data` and once as a deep clone in
        /// the `Blob` of the parsed face (fonts.rs:397-402, called from fonts.rs:988) —
        /// i.e. ~99 MB of avoidable resident memory for the current bundle.
        Shared(&'static ms_fonts::StackFont),
        /// A file of a title-local `fonts/ui` override, outside the process manifest.
        ///
        /// Read here and installed with `FontData::from_owned`, paying that second copy:
        /// the manifest is process-global and must stay project-independent, so an
        /// override cannot go through `ms_fonts::bytes` (see [`ResolvedStack`]). Overrides
        /// are rare, opt-in and small.
        ///
        /// Unlike a manifest font, these bytes come from an arbitrary opened project and
        /// are therefore UNTRUSTED: they are validated before they reach `add_font` (see
        /// [`validate_font_bytes`]).
        File(PathBuf),
    }

    impl FontSource {
        /// Path of the underlying file; the file name is what names the font in egui.
        fn path(&self) -> &Path {
            match self {
                FontSource::Shared(font) => font.path.as_path(),
                FontSource::File(path) => path.as_path(),
            }
        }

        /// The font data for `add_font`, or `None` when the file is unusable.
        ///
        /// The failure is logged (by `ms_fonts` for a shared font, here for an override
        /// file) and drops exactly that one font from the chain. An override file is
        /// additionally VALIDATED here, on the very bytes that would be handed to epaint:
        /// see [`validate_font_bytes`] for why nothing else is allowed through.
        fn load(&self) -> Option<egui::FontData> {
            match self {
                FontSource::Shared(font) => ms_fonts::bytes(font).map(egui::FontData::from_static),
                FontSource::File(path) => {
                    let bytes = read_override_font(path)?;
                    Some(egui::FontData::from_owned(bytes))
                }
            }
        }
    }

    /// Reads one title-override font file and returns its bytes only if it is a usable font.
    ///
    /// Returns `None` — with the reason logged next to the path — when the file cannot be
    /// read or fails [`validate_font_bytes`]. Callers drop exactly that one font.
    fn read_override_font(path: &Path) -> Option<Vec<u8>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[ui_fonts] failed to read UI font '{}': {err}; that font is skipped",
                    path.display()
                ));
                return None;
            }
        };
        match validate_font_bytes(&bytes) {
            Ok(_family) => Some(bytes),
            Err(reason) => {
                runtime_log::log_warn(format!(
                    "[ui_fonts] title-override font '{}' is not a usable font: {reason}; that \
                     file is skipped",
                    path.display()
                ));
                None
            }
        }
    }

    /// Validates that `bytes` are a font that can be installed, returning its family name.
    ///
    /// This is the guard that keeps an arbitrary opened project from killing the studio.
    /// A title-local `fonts/ui` override is selected by FILE EXTENSION alone, but epaint
    /// parses every registered file eagerly and turns a parse failure into a PANIC on the
    /// GUI thread — `FontFace::new(..).unwrap_or_else(|err| panic!(..))`,
    /// epaint-0.35.0/src/text/fonts.rs:987-1000 — so a `core/junk.ttf` in someone else's
    /// archive would otherwise crash the first frame after installation. The bundled path
    /// is protected indirectly (`ms_fonts` drops a file whose `name` table it cannot read,
    /// `crates/ms-fonts/src/manifest.rs`); this is the same rule for the override path.
    ///
    /// Both halves are required and neither is redundant: parsing is what epaint does, and
    /// a font without a readable family name is not a usable font either — `fontdb` rejects
    /// one as `LoadError::UnnamedFont`, and `ms_fonts` refuses to put it in the stack.
    ///
    /// Only face 0 is inspected, which is the face `add_font` installs (`FontData::index`
    /// defaults to 0). The family-name selection mirrors `ms_fonts::family_name`
    /// (typographic family, else family, US-English preferred) so both halves of the app
    /// judge a font file by the same rule.
    ///
    /// # Errors
    /// A short human-readable reason, meant to be logged next to the file path. The check
    /// is deliberately no LOOSER than epaint's: a file `ttf-parser` rejects but epaint
    /// would have accepted is dropped from the chain (logged), which degrades the UI font
    /// instead of aborting the process.
    fn validate_font_bytes(bytes: &[u8]) -> Result<String, String> {
        let face = ttf_parser::Face::parse(bytes, 0)
            .map_err(|err| format!("it does not parse as a font ({err})"))?;
        select_family_name(face.names())
            .ok_or_else(|| "its `name` table holds no unicode family-name record".to_owned())
    }

    /// Picks the family name out of a parsed `name` table.
    ///
    /// Mirrors `ms_fonts::family_name::select_family`: the typographic family (name ID 16)
    /// when the font has one, else the family (ID 1), preferring the US-English record so
    /// the result does not depend on record order. Non-unicode records are skipped —
    /// `ttf-parser` cannot decode them.
    fn select_family_name(names: ttf_parser::name::Names<'_>) -> Option<String> {
        let mut families = collect_family_records(ttf_parser::name_id::TYPOGRAPHIC_FAMILY, names);
        if families.is_empty() {
            families = collect_family_records(ttf_parser::name_id::FAMILY, names);
        }
        if let Some(index) = families
            .iter()
            .position(|(_, language)| *language == ttf_parser::Language::English_UnitedStates)
        {
            return Some(families.swap_remove(index).0);
        }
        families.into_iter().next().map(|(family, _)| family)
    }

    /// Collects every non-empty unicode record of `name_id` with the language it declares.
    fn collect_family_records(
        name_id: u16,
        names: ttf_parser::name::Names<'_>,
    ) -> Vec<(String, ttf_parser::Language)> {
        names
            .into_iter()
            .filter(|name| name.name_id == name_id && name.is_unicode())
            .filter_map(|name| {
                let family = name.to_string()?;
                (!family.is_empty()).then(|| (family, name.language()))
            })
            .collect()
    }

    /// One planned `add_font` call: which font, under which egui name, into which families.
    #[derive(Debug)]
    struct PlannedFont {
        /// Where the bytes come from when the plan is executed.
        source: FontSource,
        /// egui font name; unique per stage and file name.
        name: String,
        /// Families the font joins, in the order `add_font` folds them.
        families: Vec<InsertFontFamily>,
    }

    /// Builds the ordered `add_font` plan for `stage` from its name-sorted font list.
    ///
    /// The returned order IS the ordering contract: executing the plan front to back yields,
    /// for every `Highest` family, a chain that matches the `NN-` file-name order (see
    /// [`Stage::walks_backwards`]). `sorted_sources` must already be sorted by
    /// [`font_sort_key`] — both origins are: `ms_fonts` sorts its tiers by the same rule,
    /// and the override path sorts in [`collect_stage_paths`]. The plan is empty for an
    /// empty input.
    fn plan_stage(stage: Stage, sorted_sources: &[FontSource]) -> Vec<PlannedFont> {
        let families = stage.families();
        let ordered: Vec<&FontSource> = if stage.walks_backwards() {
            sorted_sources.iter().rev().collect()
        } else {
            sorted_sources.iter().collect()
        };

        ordered
            .into_iter()
            .map(|source| {
                let file_name = source
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unnamed");
                PlannedFont {
                    source: source.clone(),
                    name: format!("ms-ui-{}-{file_name}", stage.label()),
                    families: families.clone(),
                }
            })
            .collect()
    }

    /// Families a core font joins.
    ///
    /// `Proportional` and both named families take it at the FRONT (the UI moves onto the
    /// bundled Noto Sans while the egui defaults stay as a tail fallback), so their chains
    /// follow the `NN-` file-name order. `Monospace` takes it at the BACK so `Hack` keeps
    /// serving paths and code; because the whole stage is walked backwards for the
    /// `Highest` families, the core fonts land in `Monospace` in REVERSE `NN-` order. That
    /// is deliberate and not worth a second copy of the bytes to fix: re-registering a font
    /// under a second name would duplicate it (a repeated `add_font` with the same name is a
    /// no-op, egui-0.35.0/src/context.rs:2065-2076), and the only overlap between the core
    /// faces there is a handful of geometric symbols (■ ○ ● ◊).
    fn core_families() -> Vec<InsertFontFamily> {
        vec![
            InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: FontPriority::Highest,
            },
            InsertFontFamily {
                family: egui::FontFamily::Name(BUBBLE_TEXT_FAMILY_NAME.into()),
                priority: FontPriority::Highest,
            },
            InsertFontFamily {
                family: egui::FontFamily::Name(UI_BOLD_FAMILY_NAME.into()),
                priority: FontPriority::Highest,
            },
            InsertFontFamily {
                family: egui::FontFamily::Monospace,
                priority: FontPriority::Lowest,
            },
        ]
    }

    /// Families a bold font joins: only the bold family, at the front.
    fn bold_families() -> Vec<InsertFontFamily> {
        vec![InsertFontFamily {
            family: egui::FontFamily::Name(UI_BOLD_FAMILY_NAME.into()),
            priority: FontPriority::Highest,
        }]
    }

    /// Families an extended-script font joins: every text family, as a last-resort fallback.
    fn ext_families() -> Vec<InsertFontFamily> {
        vec![
            InsertFontFamily {
                family: egui::FontFamily::Proportional,
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: egui::FontFamily::Name(UI_BOLD_FAMILY_NAME.into()),
                priority: FontPriority::Lowest,
            },
            InsertFontFamily {
                family: egui::FontFamily::Name(BUBBLE_TEXT_FAMILY_NAME.into()),
                priority: FontPriority::Lowest,
            },
        ]
    }

    /// Executes a [`plan_stage`] plan: loads every font and registers it with `add_font`.
    ///
    /// The plan is executed strictly front to back, which is what makes the resulting chains
    /// match the documented order. A font whose bytes cannot be read is skipped (the reason
    /// is logged by [`FontSource::load`]); the return value is the number of fonts actually
    /// registered. Blocking I/O — never call this from the GUI thread.
    fn install_fonts(ctx: &egui::Context, plan: &[PlannedFont]) -> usize {
        let mut loaded = 0usize;
        for planned in plan {
            let Some(data) = planned.source.load() else {
                continue;
            };
            ctx.add_font(FontInsert::new(
                &planned.name,
                data,
                planned.families.clone(),
            ));
            loaded += 1;
        }
        loaded
    }

    /// Installs the first available system font pair when no bundled chain could be used.
    ///
    /// Mirrors the pre-`fonts/ui` behavior: one regular face into every text family plus an
    /// optional bold face in front of the bold family. Logs and leaves the egui defaults
    /// alone when no candidate exists.
    ///
    /// A candidate that exists but does not parse is passed over rather than installed: the
    /// paths are well-known, but the files on them are still outside this program's control
    /// and epaint panics on a font it cannot parse (see [`validate_font_bytes`]).
    fn install_system_fallback(ctx: &egui::Context) {
        for (regular_path, bold_path) in SYSTEM_FONT_CANDIDATES {
            // A missing candidate is the normal case here (these are per-OS paths), so a
            // read error is a probe miss rather than a fault worth logging.
            let Ok(regular_bytes) = fs::read(regular_path) else {
                continue;
            };
            if let Err(reason) = validate_font_bytes(&regular_bytes) {
                runtime_log::log_warn(format!(
                    "[ui_fonts] system font '{regular_path}' is not a usable font: {reason}; \
                     trying the next candidate"
                ));
                continue;
            }
            ctx.add_font(FontInsert::new(
                "ms-ui-system-regular",
                egui::FontData::from_owned(regular_bytes),
                core_families(),
            ));

            if let Some(bold_path) = bold_path {
                match fs::read(bold_path) {
                    Ok(bold_bytes) => match validate_font_bytes(&bold_bytes) {
                        Ok(_family) => {
                            ctx.add_font(FontInsert::new(
                                "ms-ui-system-bold",
                                egui::FontData::from_owned(bold_bytes),
                                bold_families(),
                            ));
                        }
                        Err(reason) => {
                            runtime_log::log_warn(format!(
                                "[ui_fonts] system bold font '{bold_path}' is not a usable font: \
                                 {reason}; bold UI text will use the regular face"
                            ));
                        }
                    },
                    Err(err) => {
                        runtime_log::log_warn(format!(
                            "[ui_fonts] system bold font '{bold_path}' is unreadable: {err}; \
                             bold UI text will use the regular face"
                        ));
                    }
                }
            }

            runtime_log::log_info(format!(
                "[ui_fonts] bundled fonts unavailable; using system font '{regular_path}'"
            ));
            ctx.request_repaint();
            return;
        }

        runtime_log::log_warn(
            "[ui_fonts] no bundled and no known system font found; keeping egui default fonts \
             (non-latin UI text may render as tofu)",
        );
    }

    /// The font stack the loader settled on, tier by tier, in fallback order.
    #[derive(Debug)]
    struct ResolvedStack {
        /// The winning `fonts/ui` directory. Only used for logging.
        root: PathBuf,
        /// Core fonts. Never empty — a directory yielding none is not accepted as a root.
        core: Vec<FontSource>,
        /// Bold faces; empty when the stack ships none.
        bold: Vec<FontSource>,
        /// Extended-script fonts; empty when the stack ships none.
        ext: Vec<FontSource>,
    }

    /// Resolves the stack to install: a title-local override if there is a usable one,
    /// otherwise the process manifest.
    ///
    /// The two halves are deliberately asymmetric, and this is the one place where
    /// `ui_fonts` still owns directory resolution:
    ///
    /// - `ms_fonts::stack()` is a process-global `OnceLock` that probes the launch
    ///   directory and the executable directory only. It must NOT know about the open
    ///   project, because the renderer shares that manifest and a project may never change
    ///   how a finished render looks (`dev-docs/unicode_base_font_plan.md`, decision 2).
    /// - The title override (`<title>/fonts/ui`, passed in as `extra_roots`) therefore
    ///   applies to the UI only, and stays a plain directory scan here.
    ///
    /// An override candidate that merely EXISTS is not enough: an empty — or `ext`-only —
    /// project-local `fonts/ui` is skipped so it cannot shadow the healthy bundled stack.
    /// Returns `None` when neither half yields a core font.
    fn resolve_stack(extra_roots: &[PathBuf]) -> Option<ResolvedStack> {
        override_stack(extra_roots).or_else(bundled_stack)
    }

    /// The stack of the first `extra_roots` entry that ships usable core fonts.
    ///
    /// Returns `None` immediately when there are no extra roots (the normal case), so the
    /// override costs no filesystem work unless a title actually opted in.
    fn override_stack(extra_roots: &[PathBuf]) -> Option<ResolvedStack> {
        if extra_roots.is_empty() {
            return None;
        }
        let candidates: Vec<PathBuf> = extra_roots
            .iter()
            .map(|root| root.join("fonts").join("ui"))
            .collect();
        let (root, core_paths) = first_candidate_with_core(candidates)?;

        runtime_log::log_info(format!(
            "[ui_fonts] title-local font override in '{}' takes precedence over the bundled \
             stack for the UI (the renderer keeps using the bundled one)",
            root.display()
        ));
        let bold = collect_stage_paths(&root.join(Stage::Bold.label()));
        let ext = collect_stage_paths(&root.join(Stage::Ext.label()));
        Some(ResolvedStack {
            root,
            core: to_file_sources(core_paths),
            bold: to_file_sources(bold),
            ext: to_file_sources(ext),
        })
    }

    /// The stack of the process manifest, whose bytes are shared with every other consumer.
    fn bundled_stack() -> Option<ResolvedStack> {
        let stack = ms_fonts::stack()?;
        Some(ResolvedStack {
            root: stack.root().to_path_buf(),
            core: to_shared_sources(stack.core()),
            bold: to_shared_sources(stack.bold()),
            ext: to_shared_sources(stack.ext()),
        })
    }

    /// Wraps manifest fonts as shared sources, preserving their fallback order.
    fn to_shared_sources(fonts: &'static [ms_fonts::StackFont]) -> Vec<FontSource> {
        fonts.iter().map(FontSource::Shared).collect()
    }

    /// Wraps override files as file sources, preserving their fallback order.
    fn to_file_sources(paths: Vec<PathBuf>) -> Vec<FontSource> {
        paths.into_iter().map(FontSource::File).collect()
    }

    /// [`first_usable_candidate`] bound to the real filesystem probe.
    fn first_candidate_with_core(candidates: Vec<PathBuf>) -> Option<(PathBuf, Vec<PathBuf>)> {
        first_usable_candidate(candidates, probe_core_paths)
    }

    /// Returns the first deduplicated candidate `probe` accepts, logging the rejected ones.
    ///
    /// `probe` returns the payload of an accepted candidate or `Err(reason)`; the reason is
    /// logged verbatim together with the path, so the log explains why a directory that
    /// exists was still passed over. Duplicates are reported the same way. Kept generic and
    /// filesystem-free so the selection rule itself is unit-testable.
    fn first_usable_candidate<T, F>(candidates: Vec<PathBuf>, mut probe: F) -> Option<(PathBuf, T)>
    where
        F: FnMut(&Path) -> Result<T, String>,
    {
        let mut seen = HashSet::<PathBuf>::new();
        for candidate in candidates {
            if !seen.insert(candidate.clone()) {
                runtime_log::log_info(format!(
                    "[ui_fonts] candidate '{}' skipped: already probed",
                    candidate.display()
                ));
                continue;
            }
            match probe(&candidate) {
                Ok(payload) => return Some((candidate, payload)),
                Err(reason) => runtime_log::log_info(format!(
                    "[ui_fonts] candidate '{}' skipped: {reason}",
                    candidate.display()
                )),
            }
        }
        None
    }

    /// Collects the core stage of one title-override `fonts/ui` candidate.
    ///
    /// Accepts both layouts: the current `fonts/ui/core`, and the legacy flat one where the
    /// files sit directly in `fonts/ui` and are all treated as core. Returns `Err` with a
    /// human-readable reason when the candidate yields no USABLE core font — the rule that
    /// keeps an empty (or junk-only) override from shadowing the bundled stack.
    ///
    /// Diverges from `ms_fonts::manifest::probe_core_paths` on purpose: the bundled
    /// candidates are described by `ms_fonts`, which already drops a file it cannot read a
    /// family name from, while the override files are untrusted and are therefore parsed
    /// here ([`usable_font_paths`]). A candidate whose core files all fail that check is
    /// rejected exactly like an absent one, so the loader falls back to the bundled stack
    /// instead of installing a chain that would abort the process.
    fn probe_core_paths(fonts_dir: &Path) -> Result<Vec<PathBuf>, String> {
        // Checked up front so a candidate that simply is not there is reported as such,
        // instead of producing two "cannot list directory" warnings from the two layouts.
        if !fonts_dir.is_dir() {
            return Err("directory does not exist".to_owned());
        }

        let core_paths = usable_font_paths(collect_stage_paths(&fonts_dir.join(Stage::Core.label())));
        if !core_paths.is_empty() {
            return Ok(core_paths);
        }

        let flat_paths = usable_font_paths(collect_stage_paths(fonts_dir));
        if flat_paths.is_empty() {
            return Err("no usable font file in its core/ subdirectory and none directly in it \
                        (a file that does not parse as a font does not count)"
                .to_owned());
        }

        runtime_log::log_info(format!(
            "[ui_fonts] legacy flat layout in {}: {} file(s) treated as core",
            fonts_dir.display(),
            flat_paths.len()
        ));
        Ok(flat_paths)
    }

    /// Lists the font files directly inside `stage_dir`, sorted by [`font_sort_key`].
    ///
    /// Only the title-override path uses this; the bundled stack is listed by `ms_fonts`.
    /// Non-font files and subdirectories are ignored. An unreadable or absent directory, and
    /// an unreadable individual entry, are logged and yield an empty or shortened list rather
    /// than an error: a missing stage simply is not installed.
    fn collect_stage_paths(stage_dir: &Path) -> Vec<PathBuf> {
        let entries = match fs::read_dir(stage_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // A title that overrides only `core/` is the normal case, so an absent
                // `bold/` or `ext/` is context, not a fault.
                runtime_log::log_info(format!(
                    "[ui_fonts] font directory '{}' does not exist; that stage stays empty",
                    stage_dir.display()
                ));
                return Vec::new();
            }
            Err(err) => {
                runtime_log::log_warn(format!(
                    "[ui_fonts] cannot list font directory '{}': {err}; treating it as empty",
                    stage_dir.display()
                ));
                return Vec::new();
            }
        };

        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(item) => {
                    let path = item.path();
                    if path.is_file() && is_supported_font_extension(&path) {
                        paths.push(path);
                    }
                }
                Err(err) => runtime_log::log_warn(format!(
                    "[ui_fonts] cannot read a directory entry of '{}': {err}; that file is \
                     skipped",
                    stage_dir.display()
                )),
            }
        }
        sort_font_paths(&mut paths);
        paths
    }

    /// Keeps only the override files that really are installable fonts, in the given order.
    ///
    /// [`collect_stage_paths`] selects by file EXTENSION, which says nothing about the
    /// contents; this reads each candidate and runs [`validate_font_bytes`] on it. A
    /// rejected file is logged with its path and reason and simply left out.
    ///
    /// Only the CORE stage is filtered here, because only core decides whether an override
    /// candidate wins at all ([`probe_core_paths`]). The `bold`/`ext` stages are validated
    /// when they are loaded ([`FontSource::load`]) — the same check, on the same bytes,
    /// without a second read of a tier that may be large.
    ///
    /// Blocking I/O; runs on the font loader thread, never on the GUI thread.
    fn usable_font_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
        paths
            .into_iter()
            .filter(|path| read_override_font(path).is_some())
            .collect()
    }

    /// True when the file extension is one of the supported font containers.
    fn is_supported_font_extension(path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase()),
            Some(ext) if SUPPORTED_FONT_EXTENSIONS.contains(&ext.as_str())
        )
    }

    /// Sorts font files into their intended fallback order.
    fn sort_font_paths(paths: &mut [PathBuf]) {
        paths.sort_by_cached_key(|path| font_sort_key(path.as_path()));
    }

    /// Ordering key of one font file: `NN-` prefixed files first, by their number, then the
    /// unprefixed ones by lowercased name.
    ///
    /// The tuple is `(has_no_prefix, prefix, rest)`, so the `u32` prefix orders numerically
    /// (`2-` before `10-`) instead of lexicographically.
    fn font_sort_key(path: &Path) -> (u8, u32, String) {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();

        if let Some((priority, rest_name)) = parse_font_priority_prefix(file_name) {
            return (0, priority, rest_name.to_lowercase());
        }

        (1, u32::MAX, file_name.to_lowercase())
    }

    /// Splits a `NN-rest` file name into its numeric prefix and the remainder.
    ///
    /// The separator is a HYPHEN, not a colon: a colon is not a legal character in a file
    /// name on Windows, so the bundled files cannot use one. Returns `None` when the name
    /// has no separator, an empty side, or a non-numeric prefix.
    fn parse_font_priority_prefix(file_name: &str) -> Option<(u32, &str)> {
        let (priority_raw, rest_name) = file_name.split_once('-')?;
        if priority_raw.is_empty() || rest_name.is_empty() {
            return None;
        }
        let priority = priority_raw.parse::<u32>().ok()?;
        Some((priority, rest_name))
    }

    /// Skipped by the coverage check: `has_glyph` reports the replacement character itself
    /// as missing whichever face owns it (epaint-0.35.0/src/text/font.rs:720-723), so a
    /// text that contains one — a mis-decoded chapter, say — would otherwise pull in the
    /// whole extended tier for nothing.
    const REPLACEMENT_CHARACTER: char = '\u{FFFD}';

    /// No extended tier to install: never armed, already claimed, or given up on.
    const EXT_IDLE: u8 = 0;
    /// A plan is stored and waits for the first character the core chain cannot draw.
    const EXT_ARMED: u8 = 1;
    /// Exactly one caller has taken the plan and is installing it.
    const EXT_CLAIMED: u8 = 2;

    /// One-shot gate guarding the deferred installation of the extended tier.
    ///
    /// The tier is ~80 MB and epaint can neither load a font lazily nor unload one
    /// (`dev-docs/unicode_base_font_plan.md`), so the studio arms this gate instead of
    /// installing it, and [`ensure_covers`] claims it at most once per process. The state
    /// is an atomic rather than the mutex alone so that the common case — a gate that is
    /// idle because the tier is already claimed — costs a single relaxed-ordering load per
    /// call, with no locking, on a path that runs for every drawn bubble.
    ///
    /// `state` is the authority; `plan` is only meaningful while the state is [`EXT_ARMED`]
    /// and is always stored BEFORE the state is flipped, so an armed gate always has one.
    struct ExtGate {
        /// One of [`EXT_IDLE`], [`EXT_ARMED`], [`EXT_CLAIMED`].
        state: AtomicU8,
        /// The `add_font` plan to execute; empty except while armed.
        plan: Mutex<Vec<PlannedFont>>,
    }

    /// The gate of this process. Armed by the studio window, claimed by the first
    /// uncovered character.
    static EXT_GATE: ExtGate = ExtGate::new();

    impl ExtGate {
        /// An idle gate with no plan.
        const fn new() -> Self {
            Self {
                state: AtomicU8::new(EXT_IDLE),
                plan: Mutex::new(Vec::new()),
            }
        }

        /// (Re)arms the gate with `plan`, replacing whatever the previous window left.
        ///
        /// Returns whether the gate is now armed. An empty plan RESETS it to idle instead:
        /// `run_main` runs the launcher and the studio as sequential `run_native` windows
        /// in ONE process (`src/main.rs`), so a second studio session gets a fresh context
        /// with no fonts in it and must be able to load the extended tier again — while a
        /// session whose stack (e.g. a title override) ships none must not inherit the plan
        /// of the previous one.
        ///
        /// Single-writer by construction: only `install_blocking` arms the gate, and the
        /// windows that trigger it never overlap.
        fn arm(&self, plan: Vec<PlannedFont>) -> bool {
            let armed = !plan.is_empty();
            *self.lock_plan() = plan;
            // Release: the plan write above must be visible to whoever observes EXT_ARMED.
            self.state.store(
                if armed { EXT_ARMED } else { EXT_IDLE },
                Ordering::Release,
            );
            armed
        }

        /// Whether a claimable plan is waiting. The hot-path check; no locking.
        fn is_armed(&self) -> bool {
            self.state.load(Ordering::Acquire) == EXT_ARMED
        }

        /// Hands the stored plan to exactly one caller, closing the gate.
        ///
        /// Every later call — and every concurrent one — gets `None`, which is what makes
        /// the installation happen once even though several bubbles may see the same
        /// uncovered character in the same frame. The returned plan is never empty.
        fn claim(&self) -> Option<Vec<PlannedFont>> {
            self.state
                .compare_exchange(EXT_ARMED, EXT_CLAIMED, Ordering::AcqRel, Ordering::Relaxed)
                .ok()?;
            Some(std::mem::take(&mut *self.lock_plan()))
        }

        /// Gives up on the extended tier for the rest of the session.
        ///
        /// Used when the installation cannot even be started; the plan is dropped and every
        /// later [`ensure_covers`] call returns on its first atomic load. Retrying instead
        /// would re-run the failing spawn on every frame of every bubble.
        fn abandon(&self) {
            self.lock_plan().clear();
            self.state.store(EXT_IDLE, Ordering::Release);
        }

        /// Locks the plan, recovering it from a poisoned mutex.
        ///
        /// The guarded sections only replace or take a `Vec`, so a panic elsewhere cannot
        /// leave it half-updated, and continuing is better than making the extended tier
        /// permanently unreachable.
        fn lock_plan(&self) -> MutexGuard<'_, Vec<PlannedFont>> {
            match self.plan.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            }
        }
    }

    /// Native body of [`super::ensure_covers`]; see there for the contract.
    pub(super) fn ensure_covers(ctx: &egui::Context, text: &str) {
        if !EXT_GATE.is_armed() {
            return;
        }
        // Cheap byte scan before touching egui at all: the core chain is what the entire UI
        // is drawn with, so a pure-ASCII string is never the one that needs the extended
        // tier. Everything else is decided by asking the real chain, not by guessing from
        // codepoint ranges.
        if text.is_ascii() {
            return;
        }
        if bubble_chain_covers(ctx, text) {
            return;
        }
        start_ext_install(ctx);
    }

    /// Whether the bubble font family can draw every non-ASCII character of `text`.
    ///
    /// The bubble family is the strictest of the families the extended tier joins: unlike
    /// `Proportional` it has no egui default fonts behind it, so a character that only an
    /// egui default covers still counts as missing — which is correct, because that family
    /// is the chain canvas text is actually drawn with.
    ///
    /// Returns `true` ("nothing to do") while the family is not bound yet: `add_font` is
    /// only folded into the definitions on the next pass (egui-0.35.0/src/context.rs:543-560),
    /// so the frame right after the loader finishes can still see an unbound family, and
    /// `FontsImpl::font` PANICS on one (epaint-0.35.0/src/text/fonts.rs:1030).
    fn bubble_chain_covers(ctx: &egui::Context, text: &str) -> bool {
        let family = egui::FontFamily::Name(BUBBLE_TEXT_FAMILY_NAME.into());
        ctx.fonts_mut(|fonts| {
            if !fonts.definitions().families.contains_key(&family) {
                return true;
            }
            // The size is irrelevant here: `has_glyph` only looks at `font_id.family`
            // (epaint-0.35.0/src/text/fonts.rs:858-860).
            let font_id = egui::FontId::new(1.0, family.clone());
            text.chars()
                .filter(|ch| !ch.is_ascii() && *ch != REPLACEMENT_CHARACTER)
                .all(|ch| fonts.has_glyph(&font_id, ch))
        })
    }

    /// Claims the extended tier and installs it on a worker thread.
    ///
    /// Only the caller that wins [`ExtGate::claim`] does anything, so concurrent callers in
    /// the same frame cannot start two installations. A failing thread spawn abandons the
    /// tier for the session rather than retrying every frame.
    fn start_ext_install(ctx: &egui::Context) {
        let Some(plan) = EXT_GATE.claim() else {
            return;
        };
        let planned = plan.len();
        // `egui::Context` is `Arc<RwLock<..>>` (egui-0.35.0/src/context.rs:710), so the
        // worker registers the fonts on this clone directly, exactly like the initial load.
        let ctx = ctx.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("ui-fonts-ext".to_owned())
            .spawn(move || {
                let loaded = install_fonts(&ctx, &plan);
                runtime_log::log_info(format!(
                    "[ui_fonts] installed {loaded} of {planned} extended font(s) on demand"
                ));
                ctx.request_repaint();
            })
        {
            EXT_GATE.abandon();
            runtime_log::log_warn(format!(
                "[ui_fonts] failed to spawn the extended-font loader thread: {err}; rare \
                 scripts will render as tofu for the rest of this session"
            ));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::BTreeMap;

        /// Wraps plain file names into paths, in the order given.
        fn to_paths(names: &[&str]) -> Vec<PathBuf> {
            names.iter().map(PathBuf::from).collect()
        }

        /// Bytes of a real, parsable font, taken from egui's own default definitions.
        ///
        /// The override resolver now PARSES the files it accepts, so a disk fixture needs a
        /// genuine font. Deliberately NOT read from `fonts/ui`: that tier is a large,
        /// separately shipped asset, and a test must not depend on it being present next to
        /// the checkout. `font_data` is a `BTreeMap`, so the first entry is stable ("Hack").
        fn real_font_bytes() -> Vec<u8> {
            let definitions = egui::FontDefinitions::default();
            let (_name, data) = definitions
                .font_data
                .first_key_value()
                .expect("egui ships default fonts (`default_fonts` is enabled)");
            data.font.to_vec()
        }

        /// Bytes that carry a font extension but are not a font — the crash vector.
        const JUNK_FONT_BYTES: &[u8] = b"not a font, just bytes with a .ttf name";

        /// Wraps plain file names into override (file) sources, in the order given.
        ///
        /// The ordering rules are source-independent, so the plan tests use the variant
        /// that needs no manifest; [`shared_and_file_sources_plan_identically`] pins that
        /// the manifest variant plans exactly the same way.
        fn to_sources(names: &[&str]) -> Vec<FontSource> {
            to_file_sources(to_paths(names))
        }

        /// Replays `add_font` plans the way egui folds them and returns the resulting chains.
        ///
        /// Mirrors `FontPriority::Highest` = `fam.insert(0, ..)` / `Lowest` = `fam.push`
        /// (egui-0.35.0/src/context.rs:551-557). The simulated context starts empty, so the
        /// chains below are relative to the egui default fonts, which stay behind the
        /// `Highest` entries and in front of the `Lowest` ones.
        fn simulate_chains(plans: &[Vec<PlannedFont>]) -> BTreeMap<egui::FontFamily, Vec<String>> {
            let mut families: BTreeMap<egui::FontFamily, Vec<String>> = BTreeMap::new();
            for planned in plans.iter().flatten() {
                for insert in &planned.families {
                    let chain = families.entry(insert.family.clone()).or_default();
                    match insert.priority {
                        FontPriority::Highest => chain.insert(0, planned.name.clone()),
                        FontPriority::Lowest => chain.push(planned.name.clone()),
                    }
                }
            }
            families
        }

        /// The chain of one family, as owned strings, for readable assertions.
        fn chain_of(
            chains: &BTreeMap<egui::FontFamily, Vec<String>>,
            family: &egui::FontFamily,
        ) -> Vec<String> {
            chains.get(family).cloned().unwrap_or_default()
        }

        /// The full studio installation: core, then bold, then ext once it is claimed.
        ///
        /// The extended tier is installed on demand now, but when it is, it is installed
        /// with exactly this plan — so the chains pinned below are the studio's steady
        /// state after the first character the core chain could not draw.
        fn studio_chains() -> BTreeMap<egui::FontFamily, Vec<String>> {
            let core = to_sources(&[
                "00-NotoSans-Regular.ttf",
                "01-SourceHanSansK-Regular.otf",
                "02-NotoSansSymbols-Regular.ttf",
                "03-NotoSansSymbols2-Regular.ttf",
            ]);
            let bold = to_sources(&["00-NotoSans-Bold.ttf", "01-SourceHanSansK-Bold.otf"]);
            let ext = to_sources(&["10-NotoSansMath-Regular.ttf", "20-NotoSerifHentaigana.ttf"]);

            simulate_chains(&[
                plan_stage(Stage::Core, &core),
                plan_stage(Stage::Bold, &bold),
                plan_stage(Stage::Ext, &ext),
            ])
        }

        fn sorted_names(names: &[&str]) -> Vec<String> {
            let mut paths: Vec<PathBuf> = names.iter().map(PathBuf::from).collect();
            sort_font_paths(&mut paths);
            paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect()
        }

        #[test]
        fn prefix_is_parsed_from_a_hyphen_separated_name() {
            assert_eq!(
                parse_font_priority_prefix("00-NotoSans-Regular.ttf"),
                Some((0, "NotoSans-Regular.ttf"))
            );
            assert_eq!(
                parse_font_priority_prefix("91-HanaMinA.ttf"),
                Some((91, "HanaMinA.ttf"))
            );
        }

        #[test]
        fn colon_is_no_longer_a_prefix_separator() {
            assert_eq!(parse_font_priority_prefix("0:Roboto-Regular.ttf"), None);
        }

        #[test]
        fn malformed_prefixes_are_rejected() {
            assert_eq!(parse_font_priority_prefix("NotoSans-Regular.ttf"), None);
            assert_eq!(parse_font_priority_prefix("-Regular.ttf"), None);
            assert_eq!(parse_font_priority_prefix("12-"), None);
            assert_eq!(parse_font_priority_prefix("1a-Regular.ttf"), None);
        }

        #[test]
        fn sort_key_separates_prefixed_from_unprefixed() {
            assert_eq!(
                font_sort_key(Path::new("10-NotoSansMath-Regular.ttf")),
                (0, 10, "notosansmath-regular.ttf".to_owned())
            );
            assert_eq!(
                font_sort_key(Path::new("HanaMinA.ttf")),
                (1, u32::MAX, "hanamina.ttf".to_owned())
            );
        }

        #[test]
        fn prefixes_sort_numerically_and_unprefixed_files_go_last() {
            assert_eq!(
                sorted_names(&[
                    "91-HanaMinA.ttf",
                    "ZZ-Custom.ttf",
                    "2-Second.ttf",
                    "10-Tenth.ttf",
                    "0:Legacy.ttf",
                    "00-NotoSans-Regular.ttf",
                ]),
                vec![
                    "00-NotoSans-Regular.ttf",
                    "2-Second.ttf",
                    "10-Tenth.ttf",
                    "91-HanaMinA.ttf",
                    "0:Legacy.ttf",
                    "ZZ-Custom.ttf",
                ]
            );
        }

        #[test]
        fn supported_extensions_are_case_insensitive_and_exclusive() {
            assert!(is_supported_font_extension(Path::new("a.ttf")));
            assert!(is_supported_font_extension(Path::new("a.OTF")));
            assert!(is_supported_font_extension(Path::new("a.ttc")));
            assert!(is_supported_font_extension(Path::new("a.otc")));
            assert!(!is_supported_font_extension(Path::new("a.woff2")));
            assert!(!is_supported_font_extension(Path::new("MODULE_README.md")));
            assert!(!is_supported_font_extension(Path::new("core")));
        }

        #[test]
        fn proportional_chain_follows_the_nn_order_with_ext_appended() {
            assert_eq!(
                chain_of(&studio_chains(), &egui::FontFamily::Proportional),
                vec![
                    "ms-ui-core-00-NotoSans-Regular.ttf",
                    "ms-ui-core-01-SourceHanSansK-Regular.otf",
                    "ms-ui-core-02-NotoSansSymbols-Regular.ttf",
                    "ms-ui-core-03-NotoSansSymbols2-Regular.ttf",
                    "ms-ui-ext-10-NotoSansMath-Regular.ttf",
                    "ms-ui-ext-20-NotoSerifHentaigana.ttf",
                ]
            );
        }

        #[test]
        fn bubble_family_matches_the_proportional_chain() {
            let chains = studio_chains();
            assert_eq!(
                chain_of(
                    &chains,
                    &egui::FontFamily::Name(BUBBLE_TEXT_FAMILY_NAME.into())
                ),
                chain_of(&chains, &egui::FontFamily::Proportional)
            );
        }

        #[test]
        fn bold_family_puts_bold_faces_in_front_of_the_core_chain() {
            assert_eq!(
                chain_of(
                    &studio_chains(),
                    &egui::FontFamily::Name(UI_BOLD_FAMILY_NAME.into())
                ),
                vec![
                    "ms-ui-bold-00-NotoSans-Bold.ttf",
                    "ms-ui-bold-01-SourceHanSansK-Bold.otf",
                    "ms-ui-core-00-NotoSans-Regular.ttf",
                    "ms-ui-core-01-SourceHanSansK-Regular.otf",
                    "ms-ui-core-02-NotoSansSymbols-Regular.ttf",
                    "ms-ui-core-03-NotoSansSymbols2-Regular.ttf",
                    "ms-ui-ext-10-NotoSansMath-Regular.ttf",
                    "ms-ui-ext-20-NotoSerifHentaigana.ttf",
                ]
            );
        }

        /// Pins the documented compromise: `Monospace` gets the core fonts in REVERSE `NN-`
        /// order, because the same backwards walk serves the `Highest` families. Only a
        /// handful of geometric symbols (■ ○ ● ◊) overlap there, so this is intentional —
        /// see `core_families` and `fonts/ui/MODULE_README.md`.
        #[test]
        fn monospace_gets_the_core_chain_reversed_and_no_ext_or_bold() {
            assert_eq!(
                chain_of(&studio_chains(), &egui::FontFamily::Monospace),
                vec![
                    "ms-ui-core-03-NotoSansSymbols2-Regular.ttf",
                    "ms-ui-core-02-NotoSansSymbols-Regular.ttf",
                    "ms-ui-core-01-SourceHanSansK-Regular.otf",
                    "ms-ui-core-00-NotoSans-Regular.ttf",
                ]
            );
        }

        #[test]
        fn core_tier_leaves_ext_out_of_every_family() {
            let core = to_sources(&["00-NotoSans-Regular.ttf", "01-SourceHanSansK-Regular.otf"]);
            let bold = to_sources(&["00-NotoSans-Bold.ttf"]);
            let chains = simulate_chains(&[
                plan_stage(Stage::Core, &core),
                plan_stage(Stage::Bold, &bold),
            ]);

            assert!(
                chains
                    .values()
                    .flatten()
                    .all(|name| !name.starts_with("ms-ui-ext-"))
            );
            assert_eq!(
                chain_of(&chains, &egui::FontFamily::Proportional),
                vec![
                    "ms-ui-core-00-NotoSans-Regular.ttf",
                    "ms-ui-core-01-SourceHanSansK-Regular.otf",
                ]
            );
        }

        #[test]
        fn an_empty_candidate_does_not_shadow_a_later_one_with_core_fonts() {
            let candidates = to_paths(&["/title/fonts/ui", "/chapter/fonts/ui"]);
            let mut probed: Vec<PathBuf> = Vec::new();

            let picked = first_usable_candidate(candidates, |path| {
                probed.push(path.to_path_buf());
                if path == Path::new("/chapter/fonts/ui") {
                    Ok(vec![PathBuf::from(
                        "/chapter/fonts/ui/core/00-NotoSans-Regular.ttf",
                    )])
                } else {
                    Err("no core fonts".to_owned())
                }
            });

            assert_eq!(
                picked,
                Some((
                    PathBuf::from("/chapter/fonts/ui"),
                    vec![PathBuf::from(
                        "/chapter/fonts/ui/core/00-NotoSans-Regular.ttf"
                    )]
                ))
            );
            assert_eq!(probed, to_paths(&["/title/fonts/ui", "/chapter/fonts/ui"]));
        }

        #[test]
        fn the_first_candidate_with_core_fonts_wins_and_duplicates_are_probed_once() {
            let candidates = to_paths(&["/title/fonts/ui", "/title/fonts/ui", "/chapter/fonts/ui"]);
            let mut probes = 0usize;

            let picked = first_usable_candidate(candidates, |path| {
                probes += 1;
                Ok(vec![path.join("core").join("00-NotoSans-Regular.ttf")])
            });

            assert_eq!(
                picked,
                Some((
                    PathBuf::from("/title/fonts/ui"),
                    vec![PathBuf::from("/title/fonts/ui/core/00-NotoSans-Regular.ttf")]
                ))
            );
            assert_eq!(probes, 1);
        }

        #[test]
        fn resolution_on_disk_skips_an_ext_only_directory() -> Result<(), std::io::Error> {
            let root = tempfile::tempdir()?;
            // A title that ships `fonts/ui` but no core stage: only an (empty) `ext/`.
            let title_ui = root.path().join("title").join("fonts").join("ui");
            fs::create_dir_all(title_ui.join("ext"))?;
            // A chapter-local override that does ship a core stage.
            let chapter_ui = root.path().join("chapter").join("fonts").join("ui");
            fs::create_dir_all(chapter_ui.join("core"))?;
            fs::write(
                chapter_ui.join("core").join("00-NotoSans-Regular.ttf"),
                real_font_bytes(),
            )?;

            let picked = first_candidate_with_core(vec![title_ui, chapter_ui.clone()]);

            assert_eq!(
                picked,
                Some((
                    chapter_ui.clone(),
                    vec![chapter_ui.join("core").join("00-NotoSans-Regular.ttf")]
                ))
            );
            Ok(())
        }

        #[test]
        fn a_legacy_flat_directory_is_still_accepted_as_core() -> Result<(), std::io::Error> {
            let root = tempfile::tempdir()?;
            let flat_ui = root.path().join("fonts").join("ui");
            fs::create_dir_all(&flat_ui)?;
            fs::write(flat_ui.join("00-NotoSans-Regular.ttf"), real_font_bytes())?;
            fs::write(flat_ui.join("MODULE_README.md"), b"ignored: not a font")?;

            assert_eq!(
                probe_core_paths(&flat_ui),
                Ok(vec![flat_ui.join("00-NotoSans-Regular.ttf")])
            );
            Ok(())
        }

        /// Both source kinds must plan identically, or the ordering tests above (which use
        /// file sources) would say nothing about the bundled stack, which uses shared ones.
        #[test]
        fn shared_and_file_sources_plan_identically() {
            let path = PathBuf::from("/exe/fonts/ui/ext/10-NotoSansMath-Regular.ttf");
            let font = Box::leak(Box::new(ms_fonts::StackFont {
                order: 10,
                path: path.clone(),
                family_name: "Noto Sans Math",
                tier: ms_fonts::Tier::Ext,
            }));

            let shared = plan_stage(Stage::Ext, &[FontSource::Shared(font)]);
            let file = plan_stage(Stage::Ext, &[FontSource::File(path)]);

            assert_eq!(shared.len(), 1);
            assert_eq!(shared[0].name, "ms-ui-ext-10-NotoSansMath-Regular.ttf");
            assert_eq!(shared[0].name, file[0].name);
            assert_eq!(shared[0].source.path(), file[0].source.path());
        }

        /// The stage labels ARE the `fonts/ui` subdirectory names; the override resolver
        /// builds its `bold/` and `ext/` paths from them.
        #[test]
        fn stage_labels_match_the_font_directory_layout() {
            assert_eq!(Stage::Core.label(), "core");
            assert_eq!(Stage::Bold.label(), "bold");
            assert_eq!(Stage::Ext.label(), "ext");
        }

        /// The title override must keep working after the directory resolution moved into
        /// `ms-fonts`: a root that ships core fonts wins, and every tier is read from it as
        /// an override (file) source, never from the process manifest.
        #[test]
        fn a_title_override_with_core_fonts_wins_over_the_bundled_stack()
        -> Result<(), std::io::Error> {
            let root = tempfile::tempdir()?;
            let title = root.path().join("title");
            let title_ui = title.join("fonts").join("ui");
            fs::create_dir_all(title_ui.join("core"))?;
            fs::create_dir_all(title_ui.join("ext"))?;
            fs::write(
                title_ui.join("core").join("00-Title-Regular.ttf"),
                real_font_bytes(),
            )?;
            // The extended tier is NOT validated at resolve time (only when it is loaded),
            // so a junk file still reaches the plan and is dropped there.
            fs::write(title_ui.join("ext").join("30-Title-Rare.ttf"), b"stub")?;

            let resolved = override_stack(&[title]).expect("the title ships core fonts");

            assert_eq!(resolved.root, title_ui);
            assert!(matches!(resolved.core.as_slice(), [FontSource::File(_)]));
            assert_eq!(
                resolved.core[0].path(),
                title_ui.join("core").join("00-Title-Regular.ttf")
            );
            // A tier the override does not ship stays empty instead of falling back to the
            // bundled one: mixing two stacks would make the chain order unpredictable.
            assert!(resolved.bold.is_empty());
            assert_eq!(
                resolved.ext[0].path(),
                title_ui.join("ext").join("30-Title-Rare.ttf")
            );
            Ok(())
        }

        /// The validator must accept the real bundled font, or every override would be
        /// rejected and the check would be worthless.
        #[test]
        fn a_real_font_passes_validation_and_reports_its_family() {
            assert_eq!(validate_font_bytes(&real_font_bytes()), Ok("Hack".to_owned()));
        }

        /// The crash vector itself: bytes that are not a font must be rejected with a
        /// reason instead of reaching epaint, which panics on a parse failure
        /// (epaint-0.35.0/src/text/fonts.rs:987-1000).
        #[test]
        fn junk_bytes_are_rejected_instead_of_being_installed() {
            let rejected = validate_font_bytes(JUNK_FONT_BYTES);

            assert!(rejected.is_err(), "junk must not validate: {rejected:?}");
            // Empty input is the degenerate case of the same path.
            assert!(validate_font_bytes(&[]).is_err());
        }

        /// A title override whose `core/` holds only a junk `.ttf` must behave exactly like
        /// an absent override: `override_stack` declines, so `resolve_stack` falls back to
        /// the bundled stack instead of installing a chain that would abort the process.
        #[test]
        fn an_override_core_with_only_junk_files_is_not_accepted() -> Result<(), std::io::Error>
        {
            let root = tempfile::tempdir()?;
            let title = root.path().join("title");
            let title_ui = title.join("fonts").join("ui");
            fs::create_dir_all(title_ui.join("core"))?;
            fs::write(title_ui.join("core").join("00-Junk.ttf"), JUNK_FONT_BYTES)?;

            assert!(override_stack(&[title]).is_none());
            Ok(())
        }

        /// A partially broken override keeps its usable files and drops the rest, rather
        /// than being rejected as a whole: the healthy half of a title's chain still wins.
        #[test]
        fn a_partly_broken_override_core_keeps_only_its_usable_files()
        -> Result<(), std::io::Error> {
            let root = tempfile::tempdir()?;
            let title = root.path().join("title");
            let core = title.join("fonts").join("ui").join("core");
            fs::create_dir_all(&core)?;
            fs::write(core.join("00-Junk.ttf"), JUNK_FONT_BYTES)?;
            fs::write(core.join("01-Real.ttf"), real_font_bytes())?;

            let resolved = override_stack(&[title]).expect("one core file is usable");

            assert_eq!(resolved.core.len(), 1);
            assert_eq!(resolved.core[0].path(), core.join("01-Real.ttf"));
            Ok(())
        }

        /// Executing a plan over a junk override file must not panic and must install
        /// nothing: `load` is the last gate before `add_font`, and it is what protects the
        /// `bold`/`ext` stages, which are not filtered at resolve time.
        #[test]
        fn loading_a_junk_override_font_yields_no_font_data() -> Result<(), std::io::Error> {
            let root = tempfile::tempdir()?;
            let junk = root.path().join("30-Junk.ttf");
            fs::write(&junk, JUNK_FONT_BYTES)?;
            let real = root.path().join("31-Real.ttf");
            fs::write(&real, real_font_bytes())?;

            let plan = plan_stage(
                Stage::Ext,
                &to_file_sources(vec![junk.clone(), real.clone()]),
            );

            assert_eq!(plan.len(), 2);
            assert!(FontSource::File(junk).load().is_none());
            assert!(FontSource::File(real).load().is_some());
            Ok(())
        }

        /// An override root without core fonts must fall through to the bundled stack, and
        /// no extra root at all must not touch the filesystem.
        #[test]
        fn an_override_without_core_fonts_falls_through() -> Result<(), std::io::Error> {
            let root = tempfile::tempdir()?;
            let title = root.path().join("title");
            fs::create_dir_all(title.join("fonts").join("ui").join("ext"))?;

            assert!(override_stack(&[title]).is_none());
            assert!(override_stack(&[]).is_none());
            Ok(())
        }

        /// Builds a one-font plan for the gate tests; the source is never loaded there.
        fn ext_plan() -> Vec<PlannedFont> {
            plan_stage(Stage::Ext, &to_sources(&["30-NotoSansRare-Regular.ttf"]))
        }

        #[test]
        fn an_idle_gate_has_nothing_to_claim() {
            let gate = ExtGate::new();

            assert!(!gate.is_armed());
            assert!(gate.claim().is_none());
        }

        #[test]
        fn an_empty_plan_does_not_arm_the_gate() {
            let gate = ExtGate::new();

            assert!(!gate.arm(Vec::new()));
            assert!(!gate.is_armed());
        }

        #[test]
        fn an_armed_gate_hands_its_plan_out_exactly_once() {
            let gate = ExtGate::new();
            assert!(gate.arm(ext_plan()));
            assert!(gate.is_armed());

            let claimed = gate.claim().expect("the gate was armed");

            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].name, "ms-ui-ext-30-NotoSansRare-Regular.ttf");
            // Every later call is the cheap no-op path `ensure_covers` relies on.
            assert!(!gate.is_armed());
            assert!(gate.claim().is_none());
        }

        /// A second studio session in the same process gets a fresh context and must be
        /// able to load the extended tier again, so arming after a claim must re-arm.
        #[test]
        fn a_second_window_can_arm_the_gate_again() {
            let gate = ExtGate::new();
            assert!(gate.arm(ext_plan()));
            assert!(gate.claim().is_some());

            assert!(gate.arm(ext_plan()));
            assert!(gate.is_armed());
            assert_eq!(gate.claim().map(|plan| plan.len()), Some(1));
        }

        /// A window whose stack ships no extended tier must not inherit the plan of the
        /// previous one — e.g. a title override without an `ext/` folder.
        #[test]
        fn arming_with_an_empty_plan_resets_a_previously_armed_gate() {
            let gate = ExtGate::new();
            assert!(gate.arm(ext_plan()));

            assert!(!gate.arm(Vec::new()));

            assert!(!gate.is_armed());
            assert!(gate.claim().is_none());
            assert!(gate.lock_plan().is_empty());
        }

        #[test]
        fn abandoning_the_gate_drops_the_plan_for_good() {
            let gate = ExtGate::new();
            assert!(gate.arm(ext_plan()));

            gate.abandon();

            assert!(!gate.is_armed());
            assert!(gate.claim().is_none());
            assert!(gate.lock_plan().is_empty());
        }
    }
}
