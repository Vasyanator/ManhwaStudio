/*
File: crates/ms-widgets/src/ui_fonts.rs

Purpose:
Single owner of the application UI font stack. Installs the same bundled chain
(`fonts/ui`) into every egui context the app creates — studio window, launcher,
installer, updater and the small startup prompt windows — so no window falls back
to the bare egui default fonts (tofu instead of CJK).

Main responsibilities:
- take the bundled stack from `ms_fonts::stack()` (the process manifest) and its bytes
  from `ms_fonts::bytes()`, so every font file is read once per process and epaint
  borrows it through `FontData::from_static` instead of keeping a second copy. That
  global stack is the ONLY UI font source: a title's own `fonts/` folder holds
  typesetting fonts and is never consulted for the UI;
- register every file through `egui::Context::add_font` off the GUI thread, in an order
  that makes the resulting fallback chain match the `NN-` filename ordering in the text
  families (`Monospace` gets the core fonts as a reversed trailing fallback — see
  `core_families`);
- keep the large `ext/` tier out of memory until a text actually needs a glyph the core
  chain lacks (`ensure_covers`);
- fall back to a short list of well-known system fonts when nothing is bundled, each
  validated before it reaches epaint (see `validate_font_bytes`).

Key types:
- `Tier`: how much of the stack to install (`Core` for small windows, `Full` for the studio).

Key functions:
- `install`: fire-and-forget installation; returns immediately.
- `ensure_covers`: on-demand installation of the extended tier, called from the UI while
  a frame is running.

Key constants:
- `BUBBLE_TEXT_FAMILY_NAME`: egui family used by canvas bubble text.
- `UI_BOLD_FAMILY_NAME`: egui family used wherever the UI asks for a bold face.

Notes:
- This module must never call `Context::set_fonts`: that REPLACES all font definitions
  (egui-0.36.2/src/context.rs:2106 + :548-553) and would drop the families other
  subsystems register through `add_font` (`typing-panel-combo-font-*`,
  `typing-editor-font-*`), which then panics in epaint when they are used again.
- The LOADER must never call `Context::fonts`/`Context::fonts_mut`: they panic before the
  first frame (egui-0.36.2/src/context.rs:1097-1103, :1114-1121), and the loader starts from a
  `run_native` constructor closure, i.e. before the first frame. `ensure_covers` may use
  them because it is called from UI code, from the GUI thread, during a frame.
- `add_font` is purely additive and only touches `Memory` (egui-0.36.2/src/context.rs:2129-2148), so it
  is safe both from a worker thread and before the first frame.
*/

use eframe::egui;

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
/// The stack is always the process manifest (`ms_fonts::stack`, the program's global
/// `fonts/ui`); no opened project can replace or extend it. [`Tier::Full`] additionally
/// arms the extended tier, which happens at most once per process (see `ExtGate::arm`).
///
/// Failures are never fatal: a missing `fonts/ui`, an unreadable file, or a failing
/// thread spawn is logged and degrades to the system-font fallback or, ultimately, to
/// the egui default fonts.
#[cfg(not(target_arch = "wasm32"))]
pub fn install(ctx: &egui::Context, tier: Tier) {
    // `egui::Context` is `Arc<RwLock<..>>` (egui-0.36.2/src/context.rs:723) and therefore
    // Send + Sync: the worker can call `add_font` on this clone directly, with no channel
    // and no GUI-thread polling.
    let ctx = ctx.clone();
    if let Err(err) = std::thread::Builder::new()
        .name("ui-fonts".to_owned())
        .spawn(move || desktop::install_blocking(&ctx, tier))
    {
        ms_log::runtime_log::log_warn(format!(
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
pub fn install(_ctx: &egui::Context, tier: Tier) {
    ms_log::runtime_log::log_info(format!(
        "[ui_fonts] wasm build: bundled font stack skipped (tier={tier:?})"
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
/// (epaint-0.36.2/src/text/fonts.rs:629) — a pure-ASCII string does not even get that far.
///
/// # Panics
/// Must be called from the GUI thread WHILE A FRAME IS RUNNING: it reads the installed
/// fonts through `Context::fonts_mut`, which panics before the first frame
/// (egui-0.36.2/src/context.rs:1114-1121). Never call it from a background thread or from a
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
    use ms_log::runtime_log;
    use eframe::egui;
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    use std::fs;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Mutex, MutexGuard};

    /// Last-resort system font candidates, probed in order as `(regular, bold)` pairs.
    ///
    /// Used only when no bundled `fonts/ui` chain could be loaded. The list is the
    /// historical one from `StudioApp::ensure_fonts`: CJK-capable faces on the common
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

    /// Worker-thread body of [`super::install`]: takes the process manifest, reads it, and
    /// registers everything it found. Never panics and never propagates an error.
    ///
    /// For [`Tier::Full`] the extended tier is not installed here but ARMED, so it costs
    /// nothing until [`ensure_covers`] finds a character the core chain cannot draw.
    pub(super) fn install_blocking(ctx: &egui::Context, tier: Tier) {
        let Some(stack) = ms_fonts::stack() else {
            runtime_log::log_warn(
                "[ui_fonts] no fonts/ui directory with usable core fonts found; falling back \
                 to system fonts",
            );
            install_system_fallback(ctx);
            return;
        };

        let core_loaded = install_fonts(ctx, &plan_stage(Stage::Core, stack.core()));
        if core_loaded == 0 {
            runtime_log::log_warn(format!(
                "[ui_fonts] failed to read any core font from {}; falling back to system fonts",
                stack.root().display()
            ));
            install_system_fallback(ctx);
            return;
        }

        // Bold faces are inserted after core with `Highest`, so they land in front of the
        // core fonts inside the bold family while core stays as the fallback for glyphs the
        // bold faces do not cover.
        let bold_loaded = install_fonts(ctx, &plan_stage(Stage::Bold, stack.bold()));

        runtime_log::log_info(format!(
            "[ui_fonts] installed {core_loaded} core + {bold_loaded} bold font(s) from {}",
            stack.root().display()
        ));
        ctx.request_repaint();

        match tier {
            Tier::Core => {}
            Tier::Full => arm_ext_tier(stack),
        }
    }

    /// Arms the deferred extended tier with the plan [`ensure_covers`] will execute.
    ///
    /// Logs the outcome; arming is a no-op when the stack ships no `ext/` fonts or when an
    /// earlier [`Tier::Full`] installation of this process already armed the gate.
    fn arm_ext_tier(stack: &'static ms_fonts::FontStack) {
        let planned = stack.ext().len();
        let ext_dir = stack.root().join(Stage::Ext.label());
        match EXT_GATE.arm(plan_stage(Stage::Ext, stack.ext())) {
            ArmOutcome::Armed => runtime_log::log_info(format!(
                "[ui_fonts] {planned} extended font(s) from {} armed for on-demand loading; \
                 they are installed the first time a text needs a glyph the core chain lacks",
                ext_dir.display()
            )),
            ArmOutcome::NothingToArm => runtime_log::log_info(format!(
                "[ui_fonts] no extended tier to arm in {}; rare scripts will render as tofu",
                ext_dir.display()
            )),
            ArmOutcome::AlreadyArmed => runtime_log::log_info(format!(
                "[ui_fonts] extended tier from {} was already armed by an earlier \
                 installation in this process; this request is ignored",
                ext_dir.display()
            )),
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
        /// `FontPriority::Highest` is `fam.insert(0, ..)` (egui-0.36.2/src/context.rs:567),
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

    /// Validates that `bytes` are a font that can be installed, returning its family name.
    ///
    /// This is the guard for the system-font fallback ([`install_system_fallback`]): those
    /// files sit on well-known paths but are outside this program's control, and epaint
    /// parses every registered file eagerly and turns a parse failure into a PANIC on the
    /// GUI thread — `FontFace::new(..).unwrap_or_else(|err| panic!(..))`,
    /// epaint-0.36.2/src/text/fonts.rs:981-994. The bundled path is protected indirectly
    /// (`ms_fonts` drops a file whose `name` table it cannot read,
    /// `crates/ms-fonts/src/manifest.rs`); this is the same rule for the fallback path.
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
        /// The manifest font whose bytes are registered when the plan is executed.
        ///
        /// Its bytes are read at most once per process (`ms_fonts::bytes`) and handed to
        /// epaint as `FontData::from_static`, which stores a `Cow::Borrowed`
        /// (epaint-0.36.2/src/text/fonts.rs:125-131). `from_owned` would instead keep the
        /// bytes TWICE — once in `FontDefinitions::font_data` and once as a deep clone in
        /// the `Blob` of the parsed face (fonts.rs:397-402, called from fonts.rs:988) —
        /// i.e. ~99 MB of avoidable resident memory for the current bundle.
        font: &'static ms_fonts::StackFont,
        /// egui font name; unique per stage and file name.
        name: String,
        /// Families the font joins, in the order `add_font` folds them.
        families: Vec<InsertFontFamily>,
    }

    /// Builds the ordered `add_font` plan for `stage` from one tier of the manifest.
    ///
    /// The returned order IS the ordering contract: executing the plan front to back yields,
    /// for every `Highest` family, a chain that matches the `NN-` file-name order (see
    /// [`Stage::walks_backwards`]). `sorted_fonts` must already be in fallback order, which
    /// every `ms_fonts::FontStack` tier slice is (`ms_fonts` sorts by the `NN-` prefix). The
    /// plan is empty for an empty input.
    fn plan_stage(stage: Stage, sorted_fonts: &'static [ms_fonts::StackFont]) -> Vec<PlannedFont> {
        let families = stage.families();
        let ordered: Vec<&'static ms_fonts::StackFont> = if stage.walks_backwards() {
            sorted_fonts.iter().rev().collect()
        } else {
            sorted_fonts.iter().collect()
        };

        ordered
            .into_iter()
            .map(|font| {
                let file_name = font
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("unnamed");
                PlannedFont {
                    font,
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
    /// no-op, egui-0.36.2/src/context.rs:2133-2144), and the only overlap between the core
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
    /// is logged by `ms_fonts::bytes`); the return value is the number of fonts actually
    /// registered. Blocking I/O — never call this from the GUI thread.
    fn install_fonts(ctx: &egui::Context, plan: &[PlannedFont]) -> usize {
        let mut loaded = 0usize;
        for planned in plan {
            let Some(bytes) = ms_fonts::bytes(planned.font) else {
                continue;
            };
            ctx.add_font(FontInsert::new(
                &planned.name,
                egui::FontData::from_static(bytes),
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

    /// Skipped by the coverage check: `has_glyph` reports the replacement character itself
    /// as missing whichever face owns it (epaint-0.36.2/src/text/font.rs:719-722), so a
    /// text that contains one — a mis-decoded chapter, say — would otherwise pull in the
    /// whole extended tier for nothing.
    const REPLACEMENT_CHARACTER: char = '\u{FFFD}';

    /// Not armed yet: no `Tier::Full` installation has offered a non-empty plan so far.
    const EXT_IDLE: u8 = 0;
    /// A plan is stored and waits for the first character the core chain cannot draw.
    const EXT_ARMED: u8 = 1;
    /// Exactly one caller has taken the plan and installed (or is installing) it.
    const EXT_CLAIMED: u8 = 2;
    /// The installation could not even be started; the tier is given up on for the process.
    const EXT_ABANDONED: u8 = 3;

    /// What one [`ExtGate::arm`] call did.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ArmOutcome {
        /// The plan was stored; the gate is armed.
        Armed,
        /// The plan was empty (the stack ships no `ext/` fonts); the gate is unchanged.
        NothingToArm,
        /// The gate was armed earlier in this process (it may since have been claimed or
        /// abandoned); the new plan was dropped and the gate is unchanged.
        AlreadyArmed,
    }

    /// One-shot gate guarding the deferred installation of the extended tier.
    ///
    /// The tier is ~80 MB and epaint can neither load a font lazily nor unload one
    /// (`dev-docs/unicode_base_font_plan.md`), so the studio arms this gate instead of
    /// installing it, and [`ensure_covers`] claims it at most once per process. The state
    /// is an atomic rather than the mutex alone so that the common case — a gate that is
    /// not armed because the tier is already claimed — costs a single load per call, with
    /// no locking, on a path that runs for every drawn bubble.
    ///
    /// `state` is the authority; `plan` is only meaningful while the state is [`EXT_ARMED`]
    /// and is always stored BEFORE the state is flipped, so an armed gate always has one.
    struct ExtGate {
        /// One of [`EXT_IDLE`], [`EXT_ARMED`], [`EXT_CLAIMED`], [`EXT_ABANDONED`].
        state: AtomicU8,
        /// The `add_font` plan to execute; empty except while armed.
        plan: Mutex<Vec<PlannedFont>>,
    }

    /// The gate of this process. Armed by the first `Tier::Full` installation, claimed by
    /// the first uncovered character.
    static EXT_GATE: ExtGate = ExtGate::new();

    impl ExtGate {
        /// An idle gate with no plan.
        const fn new() -> Self {
            Self {
                state: AtomicU8::new(EXT_IDLE),
                plan: Mutex::new(Vec::new()),
            }
        }

        /// Arms the gate with `plan`, at most once per process.
        ///
        /// The process has ONE shell `egui::Context`, and only it installs `Tier::Full`; the
        /// separate prompt windows use `Tier::Core` and never arm. The UI stack is the
        /// process-global manifest, so every `Tier::Full` request would offer the same plan:
        /// a second arm after the first successful one is therefore a no-op
        /// ([`ArmOutcome::AlreadyArmed`]) and never resets a claimed or abandoned gate. An
        /// empty plan arms nothing and leaves the gate as it was.
        ///
        /// The check and the store happen under the plan lock, so concurrent callers cannot
        /// both arm.
        fn arm(&self, plan: Vec<PlannedFont>) -> ArmOutcome {
            let mut stored = self.lock_plan();
            if self.state.load(Ordering::Acquire) != EXT_IDLE {
                return ArmOutcome::AlreadyArmed;
            }
            if plan.is_empty() {
                return ArmOutcome::NothingToArm;
            }
            *stored = plan;
            // Release: the plan write above must be visible to whoever observes EXT_ARMED.
            self.state.store(EXT_ARMED, Ordering::Release);
            ArmOutcome::Armed
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

        /// Gives up on the extended tier for the rest of the process.
        ///
        /// Used when the installation cannot even be started; the plan is dropped and every
        /// later [`ensure_covers`] call returns on its first atomic load. Retrying instead
        /// would re-run the failing spawn on every frame of every bubble.
        fn abandon(&self) {
            self.lock_plan().clear();
            self.state.store(EXT_ABANDONED, Ordering::Release);
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
    /// only folded into the definitions on the next pass (egui-0.36.2/src/context.rs:556-573),
    /// so the frame right after the loader finishes can still see an unbound family, and
    /// `FontsImpl::font` PANICS on one (epaint-0.36.2/src/text/fonts.rs:1025).
    fn bubble_chain_covers(ctx: &egui::Context, text: &str) -> bool {
        let family = egui::FontFamily::Name(BUBBLE_TEXT_FAMILY_NAME.into());
        ctx.fonts_mut(|fonts| {
            if !fonts.definitions().families.contains_key(&family) {
                return true;
            }
            // The size is irrelevant here: `has_glyph` only looks at `font_id.family`
            // (epaint-0.36.2/src/text/fonts.rs:852-854).
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
    /// tier for the process rather than retrying every frame.
    fn start_ext_install(ctx: &egui::Context) {
        let Some(plan) = EXT_GATE.claim() else {
            return;
        };
        let planned = plan.len();
        // `egui::Context` is `Arc<RwLock<..>>` (egui-0.36.2/src/context.rs:723), so the
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
                 scripts will render as tofu for the rest of this process"
            ));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        /// Bytes of a real, parsable font, taken from egui's own default definitions.
        ///
        /// Deliberately NOT read from `fonts/ui`: that tier is a large, separately shipped
        /// asset, and a test must not depend on it being present next to the checkout.
        /// `font_data` is a `BTreeMap`, so the first entry is stable ("Hack").
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

        /// Describes plain file names as manifest fonts of `tier`, in the order given.
        ///
        /// The slice is leaked because a plan borrows manifest fonts for `'static`, exactly
        /// as it borrows the process manifest in production; the paths are fictional and
        /// never read (the plan tests do not execute their plans).
        fn to_fonts(names: &[&str], tier: ms_fonts::Tier) -> &'static [ms_fonts::StackFont] {
            let fonts: Vec<ms_fonts::StackFont> = names
                .iter()
                .map(|name| ms_fonts::StackFont {
                    order: 0,
                    path: PathBuf::from("/exe/fonts/ui").join(tier.dir_name()).join(name),
                    family_name: "Test Family",
                    tier,
                })
                .collect();
            Box::leak(fonts.into_boxed_slice())
        }

        /// Replays `add_font` plans the way egui folds them and returns the resulting chains.
        ///
        /// Mirrors `FontPriority::Highest` = `fam.insert(0, ..)` / `Lowest` = `fam.push`
        /// (egui-0.36.2/src/context.rs:564-570). The simulated context starts empty, so the
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
        /// The extended tier is installed on demand, but when it is, it is installed with
        /// exactly this plan — so the chains pinned below are the studio's steady state
        /// after the first character the core chain could not draw.
        fn studio_chains() -> BTreeMap<egui::FontFamily, Vec<String>> {
            let core = to_fonts(
                &[
                    "00-NotoSans-Regular.ttf",
                    "01-SourceHanSansK-Regular.otf",
                    "02-NotoSansSymbols-Regular.ttf",
                    "03-NotoSansSymbols2-Regular.ttf",
                ],
                ms_fonts::Tier::Core,
            );
            let bold = to_fonts(
                &["00-NotoSans-Bold.ttf", "01-SourceHanSansK-Bold.otf"],
                ms_fonts::Tier::Bold,
            );
            let ext = to_fonts(
                &["10-NotoSansMath-Regular.ttf", "20-NotoSerifHentaigana.ttf"],
                ms_fonts::Tier::Ext,
            );

            simulate_chains(&[
                plan_stage(Stage::Core, core),
                plan_stage(Stage::Bold, bold),
                plan_stage(Stage::Ext, ext),
            ])
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
            let core = to_fonts(
                &["00-NotoSans-Regular.ttf", "01-SourceHanSansK-Regular.otf"],
                ms_fonts::Tier::Core,
            );
            let bold = to_fonts(&["00-NotoSans-Bold.ttf"], ms_fonts::Tier::Bold);
            let chains = simulate_chains(&[
                plan_stage(Stage::Core, core),
                plan_stage(Stage::Bold, bold),
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

        /// The stage labels ARE the `fonts/ui` subdirectory names; the ext-arming log names
        /// the tier directory from them.
        #[test]
        fn stage_labels_match_the_font_directory_layout() {
            assert_eq!(Stage::Core.label(), "core");
            assert_eq!(Stage::Bold.label(), "bold");
            assert_eq!(Stage::Ext.label(), "ext");
        }

        /// The validator must accept a real font, or the system fallback would reject every
        /// candidate and the check would be worthless.
        #[test]
        fn a_real_font_passes_validation_and_reports_its_family() {
            assert_eq!(validate_font_bytes(&real_font_bytes()), Ok("Hack".to_owned()));
        }

        /// The crash vector itself: bytes that are not a font must be rejected with a
        /// reason instead of reaching epaint, which panics on a parse failure
        /// (epaint-0.36.2/src/text/fonts.rs:981-994).
        #[test]
        fn junk_bytes_are_rejected_instead_of_being_installed() {
            let rejected = validate_font_bytes(JUNK_FONT_BYTES);

            assert!(rejected.is_err(), "junk must not validate: {rejected:?}");
            // Empty input is the degenerate case of the same path.
            assert!(validate_font_bytes(&[]).is_err());
        }

        /// Builds a one-font plan for the gate tests; the font is never loaded there.
        fn ext_plan() -> Vec<PlannedFont> {
            plan_stage(
                Stage::Ext,
                to_fonts(&["30-NotoSansRare-Regular.ttf"], ms_fonts::Tier::Ext),
            )
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

            assert_eq!(gate.arm(Vec::new()), ArmOutcome::NothingToArm);
            assert!(!gate.is_armed());
            // An empty offer does not use up the one arm of the process.
            assert_eq!(gate.arm(ext_plan()), ArmOutcome::Armed);
        }

        #[test]
        fn an_armed_gate_hands_its_plan_out_exactly_once() {
            let gate = ExtGate::new();
            assert_eq!(gate.arm(ext_plan()), ArmOutcome::Armed);
            assert!(gate.is_armed());

            let claimed = gate.claim().expect("the gate was armed");

            assert_eq!(claimed.len(), 1);
            assert_eq!(claimed[0].name, "ms-ui-ext-30-NotoSansRare-Regular.ttf");
            // Every later call is the cheap no-op path `ensure_covers` relies on.
            assert!(!gate.is_armed());
            assert!(gate.claim().is_none());
        }

        /// The gate arms once per process: a second arm while armed keeps the first plan.
        #[test]
        fn a_second_arm_while_armed_is_a_no_op() {
            let gate = ExtGate::new();
            assert_eq!(gate.arm(ext_plan()), ArmOutcome::Armed);

            assert_eq!(gate.arm(ext_plan()), ArmOutcome::AlreadyArmed);
            assert_eq!(gate.arm(Vec::new()), ArmOutcome::AlreadyArmed);

            assert!(gate.is_armed());
            assert_eq!(gate.claim().map(|plan| plan.len()), Some(1));
        }

        /// A claimed tier is installed in the one shell context for good, so arming again
        /// must neither re-arm nor reset the gate.
        #[test]
        fn arming_after_a_claim_does_not_re_arm() {
            let gate = ExtGate::new();
            assert_eq!(gate.arm(ext_plan()), ArmOutcome::Armed);
            assert!(gate.claim().is_some());

            assert_eq!(gate.arm(ext_plan()), ArmOutcome::AlreadyArmed);

            assert!(!gate.is_armed());
            assert!(gate.claim().is_none());
            assert!(gate.lock_plan().is_empty());
        }

        #[test]
        fn abandoning_the_gate_drops_the_plan_for_good() {
            let gate = ExtGate::new();
            assert_eq!(gate.arm(ext_plan()), ArmOutcome::Armed);
            assert!(gate.claim().is_some());

            gate.abandon();

            assert!(!gate.is_armed());
            assert!(gate.claim().is_none());
            assert!(gate.lock_plan().is_empty());
            // Given up on for the process: a later arm does not bring it back.
            assert_eq!(gate.arm(ext_plan()), ArmOutcome::AlreadyArmed);
            assert!(!gate.is_armed());
        }
    }
}
