/*
File: crates/ms-fonts/src/manifest.rs

Purpose:
Builds and owns the process manifest of the bundled `fonts/ui` directory: which
directory to use, which files it holds, in which order they are consulted, which tier
they belong to and under which family name a shaper can address them.

Main responsibilities:
- resolve `fonts/ui` among the launch-directory and executable-directory candidates,
  taking the first one that actually yields core fonts;
- turn the files of `core/`, `bold/` and `ext/` into ordered `StackFont` records;
- expose the manifest through a process-wide `OnceLock`.

Key structures:
- `Tier`, `StackFont`, `FontStack`.

Key functions:
- `stack`: the manifest; resolved (and logged) on first use.

Notes:
This module never reads font bytes — that is `store::bytes`. It does read the `name`
table of every file it describes (`family_name`), which is a few kilobytes per file,
not the whole file.
*/

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::{env, fs};

use ms_log::runtime_log;

use crate::family_name;

/// Font container extensions the manifest accepts, lowercase.
const SUPPORTED_FONT_EXTENSIONS: [&str; 4] = ["otf", "ttf", "ttc", "otc"];

/// Fallback order given to a file whose name carries no valid `NN-` prefix.
///
/// Such files sort after every prefixed one, so an unnumbered file added to the bundle
/// can never push its way in front of the curated chain.
const UNPREFIXED_ORDER: u32 = u32::MAX;

/// The parsed manifest of the process, or `None` when no usable directory was found.
static STACK: OnceLock<Option<FontStack>> = OnceLock::new();

/// Stage of the bundled stack a font belongs to.
///
/// The tier decides how a consumer is expected to load the font, not what it contains:
/// `Core` and `Bold` are small and always resident, `Ext` is large and meant to be
/// mapped on demand (see `dev-docs/unicode_base_font_plan.md`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Tier {
    /// `fonts/ui/core`: latin, cyrillic, CJK and symbols — the chain every window needs.
    Core,
    /// `fonts/ui/bold`: bold faces, used in front of the core chain by the bold family.
    Bold,
    /// `fonts/ui/ext`: extended scripts, math, music, emoji and the rare CJK planes.
    Ext,
}

impl Tier {
    /// Name of the `fonts/ui` subdirectory this tier is read from.
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Tier::Core => "core",
            Tier::Bold => "bold",
            Tier::Ext => "ext",
        }
    }
}

/// One described font file of the bundled stack.
///
/// A `StackFont` exists only for a file whose family name could actually be read, so
/// `family_name` is always the real name from the font's `name` table — never a guess
/// derived from the file name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackFont {
    /// Fallback rank from the `NN-` file-name prefix; [`UNPREFIXED_ORDER`] when absent.
    ///
    /// Lower is consulted first. It is informational for consumers that just walk the
    /// tier slices, which are already sorted by it.
    pub order: u32,
    /// Absolute path of the font file. The bytes are read through [`crate::bytes`].
    pub path: PathBuf,
    /// Family name from the font's `name` table, extended to `'static`.
    ///
    /// `'static` is a hard requirement of the shaper side: the cosmic-text `Fallback`
    /// trait returns `&[&'static str]` and has no other way to name a family
    /// (`cosmic-text-0.14.2/src/font/fallback/mod.rs:68-77`).
    pub family_name: &'static str,
    /// Tier the file was found in.
    pub tier: Tier,
}

/// The manifest of one `fonts/ui` directory: its tiers, in fallback order.
///
/// Built once per process by [`stack`]. Every tier slice is sorted by [`font_sort_key`],
/// i.e. `NN-` prefixed files first by their number, then unprefixed ones by name.
#[derive(Debug)]
pub struct FontStack {
    /// The `fonts/ui` directory this manifest describes.
    root: PathBuf,
    /// Core fonts. Never empty: a directory yielding none is not accepted as the root.
    core: Vec<StackFont>,
    /// Bold faces; empty when the bundle ships none.
    bold: Vec<StackFont>,
    /// Extended-script fonts; empty when the bundle ships none.
    ext: Vec<StackFont>,
}

impl FontStack {
    /// The `fonts/ui` directory the manifest was built from.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Core fonts, in fallback order. Never empty.
    #[must_use]
    pub fn core(&self) -> &[StackFont] {
        &self.core
    }

    /// Bold faces, in fallback order.
    #[must_use]
    pub fn bold(&self) -> &[StackFont] {
        &self.bold
    }

    /// Extended-script fonts, in fallback order.
    #[must_use]
    pub fn ext(&self) -> &[StackFont] {
        &self.ext
    }

    /// The fonts of one tier, in fallback order.
    #[must_use]
    pub fn tier(&self, tier: Tier) -> &[StackFont] {
        match tier {
            Tier::Core => self.core(),
            Tier::Bold => self.bold(),
            Tier::Ext => self.ext(),
        }
    }
}

/// The process manifest of the bundled font stack, resolved on first use.
///
/// Returns `None` when no candidate directory yields a single usable core font; the
/// reason is logged. The result is cached for the lifetime of the process, so the
/// directory scan and the `name`-table reads happen exactly once even across several
/// `run_native` sessions of the same process (launcher, then studio).
///
/// The first call does disk work (one directory listing per tier plus a few kilobytes
/// read per font file) and must therefore not happen on the GUI thread; later calls are
/// a plain atomic load.
#[must_use]
pub fn stack() -> Option<&'static FontStack> {
    STACK.get_or_init(build_stack).as_ref()
}

/// Resolves the directory and describes its three tiers. Called once, by [`stack`].
fn build_stack() -> Option<FontStack> {
    let Some((root, core_paths)) = first_candidate_with_core(candidate_dirs()) else {
        runtime_log::log_warn(
            "[ms_fonts] no fonts/ui directory with usable core fonts was found; the \
             bundled font stack is unavailable this session",
        );
        return None;
    };

    let core = describe_tier(&core_paths, Tier::Core);
    if core.is_empty() {
        runtime_log::log_warn(format!(
            "[ms_fonts] none of the {} core font file(s) in '{}' has a readable family \
             name; the bundled font stack is unavailable this session",
            core_paths.len(),
            root.display()
        ));
        return None;
    }

    let bold = describe_tier(
        &collect_tier_paths(&root.join(Tier::Bold.dir_name())),
        Tier::Bold,
    );
    let ext = describe_tier(
        &collect_tier_paths(&root.join(Tier::Ext.dir_name())),
        Tier::Ext,
    );

    runtime_log::log_info(format!(
        "[ms_fonts] font stack resolved in '{}': {} core, {} bold, {} ext font(s)",
        root.display(),
        core.len(),
        bold.len(),
        ext.len()
    ));

    Some(FontStack {
        root,
        core,
        bold,
        ext,
    })
}

/// Describes every file of one tier, dropping the ones without a readable family name.
fn describe_tier(sorted_paths: &[PathBuf], tier: Tier) -> Vec<StackFont> {
    sorted_paths
        .iter()
        .filter_map(|path| describe_font(path, tier))
        .collect()
}

/// Describes one font file, or logs why it cannot be part of the stack.
///
/// A file whose `name` table cannot be read is skipped rather than given a name derived
/// from its file name: a fabricated family name would be silently unreachable in every
/// fallback chain. `fontdb` rejects such a file the same way
/// (`LoadError::UnnamedFont`, `fontdb-0.16.2/src/lib.rs:950`).
fn describe_font(path: &Path, tier: Tier) -> Option<StackFont> {
    let family = family_name::read_family_name(path)?;
    // The shaper side can only name a family through `&'static str` (see
    // `StackFont::family_name`), and a font registered with cosmic-text can never be
    // unregistered, so the name is leaked. The leak is bounded by the number of files in
    // `fonts/ui`: the manifest is built exactly once per process.
    let family_name: &'static str = Box::leak(family.into_boxed_str());
    Some(StackFont {
        order: font_order(path),
        path: path.to_path_buf(),
        family_name,
        tier,
    })
}

/// Builds the ordered `fonts/ui` candidate list; failing environment lookups are logged.
///
/// Probe order: the launch working directory, then the executable directory. A failing
/// `current_dir`/`current_exe` only drops that one candidate, so the stack is still found
/// through the other one.
///
/// Project/title-local overrides are deliberately NOT candidates here: they apply to the
/// UI only, because a project must not be able to change how a finished render looks
/// (`dev-docs/unicode_base_font_plan.md`, decision 2).
fn candidate_dirs() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    match env::current_dir() {
        Ok(cwd) => candidates.push(cwd.join("fonts").join("ui")),
        Err(err) => runtime_log::log_warn(format!(
            "[ms_fonts] std::env::current_dir failed: {err}; the working-directory \
             fonts/ui candidate is skipped"
        )),
    }
    match env::current_exe() {
        Ok(exe_path) => match exe_path.parent() {
            Some(exe_dir) => candidates.push(exe_dir.join("fonts").join("ui")),
            None => runtime_log::log_warn(format!(
                "[ms_fonts] executable path '{}' has no parent directory; the bundled \
                 fonts/ui candidate is skipped",
                exe_path.display()
            )),
        },
        Err(err) => runtime_log::log_warn(format!(
            "[ms_fonts] std::env::current_exe failed: {err}; the bundled fonts/ui \
             candidate is skipped"
        )),
    }

    candidates
}

/// [`first_usable_candidate`] bound to the real filesystem probe.
fn first_candidate_with_core(candidates: Vec<PathBuf>) -> Option<(PathBuf, Vec<PathBuf>)> {
    first_usable_candidate(candidates, probe_core_paths)
}

/// Returns the first deduplicated candidate `probe` accepts, logging the rejected ones.
///
/// `probe` returns the payload of an accepted candidate or `Err(reason)`; the reason is
/// logged verbatim with the path, so the log explains why a directory that exists was
/// still passed over. Duplicates are reported the same way. Kept generic and
/// filesystem-free so the selection rule itself is unit-testable.
fn first_usable_candidate<T, F>(candidates: Vec<PathBuf>, mut probe: F) -> Option<(PathBuf, T)>
where
    F: FnMut(&Path) -> Result<T, String>,
{
    let mut seen = HashSet::<PathBuf>::new();
    for candidate in candidates {
        if !seen.insert(candidate.clone()) {
            runtime_log::log_info(format!(
                "[ms_fonts] candidate '{}' skipped: already probed",
                candidate.display()
            ));
            continue;
        }
        match probe(&candidate) {
            Ok(payload) => return Some((candidate, payload)),
            Err(reason) => runtime_log::log_info(format!(
                "[ms_fonts] candidate '{}' skipped: {reason}",
                candidate.display()
            )),
        }
    }
    None
}

/// Collects the core tier of one `fonts/ui` candidate.
///
/// Accepts both layouts: the current `fonts/ui/core`, and the legacy flat one where the
/// files sit directly in `fonts/ui` and are all treated as core. Returns `Err` with a
/// human-readable reason when the candidate yields no core font at all — merely existing
/// is not enough, so an empty (or `ext`-only) directory cannot shadow a healthy one
/// later in the list.
fn probe_core_paths(fonts_dir: &Path) -> Result<Vec<PathBuf>, String> {
    // Checked up front so a candidate that simply is not there is reported as such,
    // instead of producing two "cannot list directory" lines from the two layouts.
    if !fonts_dir.is_dir() {
        return Err("directory does not exist".to_owned());
    }

    let core_paths = collect_tier_paths(&fonts_dir.join(Tier::Core.dir_name()));
    if !core_paths.is_empty() {
        return Ok(core_paths);
    }

    let flat_paths = collect_tier_paths(fonts_dir);
    if flat_paths.is_empty() {
        return Err("no font files in its core/ subdirectory and none directly in it".to_owned());
    }

    runtime_log::log_info(format!(
        "[ms_fonts] legacy flat layout in '{}': {} file(s) treated as core",
        fonts_dir.display(),
        flat_paths.len()
    ));
    Ok(flat_paths)
}

/// Lists the font files directly inside `tier_dir`, sorted by [`font_sort_key`].
///
/// Subdirectories and non-font files are ignored. A missing directory yields an empty
/// list (a tier the bundle does not ship simply has no fonts); any other listing error,
/// and an unreadable individual entry, are logged and shorten the list rather than
/// failing the whole manifest.
fn collect_tier_paths(tier_dir: &Path) -> Vec<PathBuf> {
    let entries = match fs::read_dir(tier_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            runtime_log::log_info(format!(
                "[ms_fonts] font directory '{}' does not exist; that tier stays empty",
                tier_dir.display()
            ));
            return Vec::new();
        }
        Err(err) => {
            runtime_log::log_warn(format!(
                "[ms_fonts] cannot list font directory '{}': {err}; treating it as empty",
                tier_dir.display()
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
                "[ms_fonts] cannot read a directory entry of '{}': {err}; that file is \
                 skipped",
                tier_dir.display()
            )),
        }
    }
    sort_font_paths(&mut paths);
    paths
}

/// True when the file extension is one of the supported font containers.
fn is_supported_font_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase),
        Some(ext) if SUPPORTED_FONT_EXTENSIONS.contains(&ext.as_str())
    )
}

/// Sorts font files into their intended fallback order.
fn sort_font_paths(paths: &mut [PathBuf]) {
    paths.sort_by_cached_key(|path| font_sort_key(path.as_path()));
}

/// Fallback rank of one file: its `NN-` prefix, or [`UNPREFIXED_ORDER`] when it has none.
fn font_order(path: &Path) -> u32 {
    match file_name_of(path).and_then(parse_order_prefix) {
        Some((order, _)) => order,
        None => UNPREFIXED_ORDER,
    }
}

/// Ordering key of one font file: `NN-` prefixed files first, by their number, then the
/// unprefixed ones by lowercased name.
///
/// The tuple is `(has_no_prefix, order, rest)`, so the `u32` order sorts numerically
/// (`2-` before `10-`) instead of lexicographically.
fn font_sort_key(path: &Path) -> (u8, u32, String) {
    let file_name = file_name_of(path).unwrap_or_default();

    match parse_order_prefix(file_name) {
        Some((order, rest_name)) => (0, order, rest_name.to_lowercase()),
        None => (1, UNPREFIXED_ORDER, file_name.to_lowercase()),
    }
}

/// The file name of `path` as UTF-8, or `None` when it has none or is not UTF-8.
fn file_name_of(path: &Path) -> Option<&str> {
    path.file_name().and_then(|name| name.to_str())
}

/// Splits a `NN-rest` file name into its numeric prefix and the remainder.
///
/// The separator is a HYPHEN, not a colon: a colon is not a legal character in a file
/// name on Windows, so the bundled files cannot use one. Returns `None` when the name has
/// no separator, an empty side, or a non-numeric prefix.
fn parse_order_prefix(file_name: &str) -> Option<(u32, &str)> {
    let (order_raw, rest_name) = file_name.split_once('-')?;
    if order_raw.is_empty() || rest_name.is_empty() {
        return None;
    }
    let order = order_raw.parse::<u32>().ok()?;
    Some((order, rest_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps plain file names into paths, in the order given.
    fn to_paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    /// The file names of `names` after [`sort_font_paths`].
    fn sorted_names(names: &[&str]) -> Vec<String> {
        let mut paths = to_paths(names);
        sort_font_paths(&mut paths);
        paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn order_prefix_is_parsed_from_a_hyphen_separated_name() {
        assert_eq!(
            parse_order_prefix("00-NotoSans-Regular.ttf"),
            Some((0, "NotoSans-Regular.ttf"))
        );
        assert_eq!(parse_order_prefix("91-HanaMinA.ttf"), Some((91, "HanaMinA.ttf")));
    }

    #[test]
    fn a_colon_is_not_an_order_separator() {
        // A colon cannot appear in a file name on Windows, so it never marks an order.
        assert_eq!(parse_order_prefix("0:Roboto-Regular.ttf"), None);
    }

    #[test]
    fn malformed_order_prefixes_are_rejected() {
        assert_eq!(parse_order_prefix("NotoSans-Regular.ttf"), None);
        assert_eq!(parse_order_prefix("-Regular.ttf"), None);
        assert_eq!(parse_order_prefix("12-"), None);
        assert_eq!(parse_order_prefix("1a-Regular.ttf"), None);
    }

    #[test]
    fn order_falls_back_to_the_unprefixed_rank() {
        assert_eq!(font_order(Path::new("10-NotoSansMath-Regular.ttf")), 10);
        assert_eq!(font_order(Path::new("HanaMinA.ttf")), UNPREFIXED_ORDER);
    }

    #[test]
    fn sort_key_separates_prefixed_from_unprefixed() {
        assert_eq!(
            font_sort_key(Path::new("10-NotoSansMath-Regular.ttf")),
            (0, 10, "notosansmath-regular.ttf".to_owned())
        );
        assert_eq!(
            font_sort_key(Path::new("HanaMinA.ttf")),
            (1, UNPREFIXED_ORDER, "hanamina.ttf".to_owned())
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
    fn an_empty_candidate_does_not_shadow_a_later_one_with_core_fonts() {
        let candidates = to_paths(&["/cwd/fonts/ui", "/exe/fonts/ui"]);
        let mut probed: Vec<PathBuf> = Vec::new();

        let picked = first_usable_candidate(candidates, |path| {
            probed.push(path.to_path_buf());
            if path == Path::new("/exe/fonts/ui") {
                Ok(vec![PathBuf::from("/exe/fonts/ui/core/00-NotoSans-Regular.ttf")])
            } else {
                Err("no core fonts".to_owned())
            }
        });

        assert_eq!(
            picked,
            Some((
                PathBuf::from("/exe/fonts/ui"),
                vec![PathBuf::from("/exe/fonts/ui/core/00-NotoSans-Regular.ttf")]
            ))
        );
        assert_eq!(probed, to_paths(&["/cwd/fonts/ui", "/exe/fonts/ui"]));
    }

    #[test]
    fn the_first_candidate_with_core_fonts_wins_and_duplicates_are_probed_once() {
        let candidates = to_paths(&["/cwd/fonts/ui", "/cwd/fonts/ui", "/exe/fonts/ui"]);
        let mut probes = 0usize;

        let picked = first_usable_candidate(candidates, |path| {
            probes += 1;
            Ok(vec![path.join("core").join("00-NotoSans-Regular.ttf")])
        });

        assert_eq!(
            picked,
            Some((
                PathBuf::from("/cwd/fonts/ui"),
                vec![PathBuf::from("/cwd/fonts/ui/core/00-NotoSans-Regular.ttf")]
            ))
        );
        assert_eq!(probes, 1);
    }

    #[test]
    fn resolution_on_disk_skips_an_ext_only_directory() -> Result<(), std::io::Error> {
        let root = tempfile::tempdir()?;
        // A candidate that ships `fonts/ui` but no core tier: only an (empty) `ext/`.
        let empty_ui = root.path().join("cwd").join("fonts").join("ui");
        fs::create_dir_all(empty_ui.join("ext"))?;
        // The healthy bundled directory next to the executable.
        let bundled_ui = root.path().join("exe").join("fonts").join("ui");
        fs::create_dir_all(bundled_ui.join("core"))?;
        fs::write(
            bundled_ui.join("core").join("00-NotoSans-Regular.ttf"),
            b"not a real font; the resolver never parses it",
        )?;

        let picked = first_candidate_with_core(vec![empty_ui, bundled_ui.clone()]);

        assert_eq!(
            picked,
            Some((
                bundled_ui.clone(),
                vec![bundled_ui.join("core").join("00-NotoSans-Regular.ttf")]
            ))
        );
        Ok(())
    }

    #[test]
    fn every_tier_is_addressable_through_the_tier_accessor() {
        let font = |name: &'static str, tier| StackFont {
            order: 0,
            path: PathBuf::from(name),
            family_name: name,
            tier,
        };
        let stack = FontStack {
            root: PathBuf::from("/exe/fonts/ui"),
            core: vec![font("Core Face", Tier::Core)],
            bold: vec![font("Bold Face", Tier::Bold)],
            ext: vec![font("Ext Face", Tier::Ext)],
        };

        assert_eq!(stack.root(), Path::new("/exe/fonts/ui"));
        assert_eq!(stack.tier(Tier::Core), stack.core());
        assert_eq!(stack.tier(Tier::Bold), stack.bold());
        assert_eq!(stack.tier(Tier::Ext), stack.ext());
        assert_eq!(stack.tier(Tier::Ext)[0].family_name, "Ext Face");
    }

    #[test]
    fn resolving_the_process_stack_is_total() {
        // Whether a usable `fonts/ui` sits next to the test binary is environment
        // dependent, so only the contract itself is asserted: resolution never panics,
        // and an accepted stack always has a core tier.
        if let Some(resolved) = stack() {
            assert!(!resolved.core().is_empty());
            assert!(resolved.core().iter().all(|font| font.tier == Tier::Core));
        }
    }

    #[test]
    fn a_legacy_flat_directory_is_still_accepted_as_core() -> Result<(), std::io::Error> {
        let root = tempfile::tempdir()?;
        let flat_ui = root.path().join("fonts").join("ui");
        fs::create_dir_all(&flat_ui)?;
        fs::write(flat_ui.join("00-NotoSans-Regular.ttf"), b"stub")?;
        fs::write(flat_ui.join("MODULE_README.md"), b"ignored: not a font")?;

        assert_eq!(
            probe_core_paths(&flat_ui),
            Ok(vec![flat_ui.join("00-NotoSans-Regular.ttf")])
        );
        Ok(())
    }
}
