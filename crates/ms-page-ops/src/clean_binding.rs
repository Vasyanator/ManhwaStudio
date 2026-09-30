/*
File: crates/ms-page-ops/src/clean_binding.rs

Purpose:
The single owner of the page <-> clean-overlay BINDING RULE: which file name is page N's clean
layer, and whether a clean of a given size fits a page. Pure: no filesystem access, no logging.

Key items:
- `clean_overlay_file_name` / `clean_overlay_os_file_name`: the canonical `<page stem>.png` name.
- `clean_overlay_stem`: the inverse (file name -> page stem), as page operations use it.
- `page_clean_stem` / `writer_clean_stem`: how a page path yields the stem its clean is keyed by.
- `CleanPageFit` / `classify_clean_fit`: the exact-size fit rule and its failure precedence.
- `CLEAN_NAMES_CASE_INSENSITIVE` / `clean_name_key`: how a FOUND file name is compared with a
  canonical clean name, matching the platform's filesystem.

Notes:
- The worker-side I/O resolver built on these rules (probing both clean trees, the inventory
  scan) lives in `ms_models::clean_assign`; this crate sits below `ms-project` and `ms-models`,
  so every layer can reach the rule. Never restate `<stem>.png` or the fit rule elsewhere.
- Deliberately NOT part of the binding rule: `ms_models::clean_assign::attach_fit` (a 1% aspect
  tolerance for an explicit "attach this file" operation, which rescales) and the page-op
  engine's size-blind carrying of `<stem>.png` files through structural operations.
- Non-UTF-8 page stems (a pre-existing, platform-specific edge) resolve differently per consumer
  and are preserved as they are: the clean model writer and the overlay loader use
  `writer_clean_stem` (fallback stem `overlay`), the page manager keys its per-page paths by the
  raw `OsStr` stem, and the canonical-name scan skips such pages.
- Name case. The loader opens the canonical path, so the FILESYSTEM decides which file that is:
  on Windows `004.PNG` (or `PAGE1.png` for page `Page1`) answers to `004.png`. A scan of directory
  entries must agree with that, so it compares names through `clean_name_key`, which folds ASCII
  case exactly when `CLEAN_NAMES_CASE_INSENSITIVE` (`cfg!(windows)`). Non-ASCII letters are not
  folded (NTFS folds them too; such a clean is loaded but reported as unassigned). The canonical
  name the writer produces is always the exact `<stem>.png`.
- `clean_overlay_stem` stays case-SENSITIVE on every platform: page operations carry only an
  exact `<stem>.png` through a structural op. On Windows a case-variant clean (`004.PNG`) is
  therefore loaded and reported as page 004's, but a page op leaves it at its name (see
  `dev-docs/known_gaps.md`, KG-022).
*/

use std::borrow::Cow;

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Extension (with its dot) of every clean-overlay file. The canonical name is always written with
/// it exactly; whether a found `001.PNG` answers to it is platform-dependent (`clean_name_key`).
const CLEAN_OVERLAY_SUFFIX: &str = ".png";

/// Stem the clean model writer and the overlay loader use for a page whose file stem is missing
/// or not valid UTF-8. Kept only so those two sides keep agreeing with each other.
pub const FALLBACK_CLEAN_STEM: &str = "overlay";

/// Returns the canonical clean-overlay file name of the page whose file stem is `page_stem`:
/// `<page_stem>.png`, in either clean tree (`clean_layers/` or `_unsaved/clean_layers/`).
#[must_use]
pub fn clean_overlay_file_name(page_stem: &str) -> String {
    format!("{page_stem}{CLEAN_OVERLAY_SUFFIX}")
}

/// [`clean_overlay_file_name`] for a raw, possibly non-UTF-8 page stem.
#[must_use]
pub fn clean_overlay_os_file_name(page_stem: &OsStr) -> OsString {
    let mut name = page_stem.to_os_string();
    name.push(CLEAN_OVERLAY_SUFFIX);
    name
}

/// Inverse of [`clean_overlay_file_name`]: the page stem a clean file name is keyed by.
///
/// Strips a case-sensitive `.png` suffix and rejects an empty stem (a bare `.png`), so the result
/// is `Some` exactly for names that `clean_overlay_file_name` can produce.
/// Case-sensitive on EVERY platform, unlike [`clean_name_key`]: it defines what page operations
/// carry, not what the loader can open (see the file header).
#[must_use]
pub fn clean_overlay_stem(file_name: &str) -> Option<&str> {
    file_name.strip_suffix(CLEAN_OVERLAY_SUFFIX).filter(|stem| !stem.is_empty())
}

/// The UTF-8 file stem of `page_path` that its clean overlay is keyed by, or `None` when the page
/// has no stem or it is not valid UTF-8.
#[must_use]
pub fn page_clean_stem(page_path: &Path) -> Option<&str> {
    page_path.file_stem().and_then(OsStr::to_str)
}

/// The stem the clean model WRITES a page's overlay under (and the overlay loader reads it back
/// from): [`page_clean_stem`], or [`FALLBACK_CLEAN_STEM`] when that is `None`.
#[must_use]
pub fn writer_clean_stem(page_path: &Path) -> &str {
    page_clean_stem(page_path).unwrap_or(FALLBACK_CLEAN_STEM)
}

/// Whether clean file names on this platform's filesystem are ASCII-case-insensitive, i.e. whether
/// opening the canonical `004.png` can open a file listed as `004.PNG`. `true` on Windows only.
pub const CLEAN_NAMES_CASE_INSENSITIVE: bool = cfg!(windows);

/// The key under which a clean file name found in a directory is compared with a page's canonical
/// clean name ([`clean_overlay_file_name`]): two names denote the same file exactly when their keys
/// are equal. With `case_insensitive` the key is ASCII-lower-cased (borrowed when already lower);
/// otherwise it is the name itself. Callers pass [`CLEAN_NAMES_CASE_INSENSITIVE`]; the parameter
/// exists so both behaviours are testable on any platform.
#[must_use]
pub fn clean_name_key(file_name: &str, case_insensitive: bool) -> Cow<'_, str> {
    if case_insensitive && file_name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(file_name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(file_name)
    }
}

/// Whether a clean of some size fits the page it is bound to by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanPageFit {
    /// Clean and page have exactly the same `[width, height]` (`size`): the clean binds.
    Matches { size: [u32; 2] },
    /// Both headers are readable but the sizes differ; `[width, height]` in pixels.
    SizeMismatch { clean: [u32; 2], page: [u32; 2] },
    /// The clean file's header could not be read; carries the caller's diagnostic.
    CleanUnreadable(String),
    /// The clean is readable (`clean` is its `[width, height]`) but the page's header could not
    /// be read; `error` is the page diagnostic.
    PageUnreadable { clean: [u32; 2], error: String },
}

/// Applies the binding fit rule: a clean binds only at EXACTLY the page's pixel size.
///
/// `clean` is the clean file's header size or a diagnostic. `page` yields the page's header size
/// or a diagnostic; it is called only when `clean` is `Ok`, because an unreadable clean takes
/// precedence and callers must not pay for (or log about) a page header read they do not need.
#[must_use]
pub fn classify_clean_fit(
    clean: Result<[u32; 2], String>,
    page: impl FnOnce() -> Result<[u32; 2], String>,
) -> CleanPageFit {
    let clean = match clean {
        Ok(size) => size,
        Err(error) => return CleanPageFit::CleanUnreadable(error),
    };
    match page() {
        Ok(page) if page == clean => CleanPageFit::Matches { size: clean },
        Ok(page) => CleanPageFit::SizeMismatch { clean, page },
        Err(error) => CleanPageFit::PageUnreadable { clean, error },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn file_name_and_stem_are_inverse() {
        assert_eq!(clean_overlay_file_name("001"), "001.png");
        assert_eq!(clean_overlay_stem("001.png"), Some("001"));
        assert_eq!(clean_overlay_stem(&clean_overlay_file_name("a.b")), Some("a.b"));
        assert_eq!(clean_overlay_os_file_name(OsStr::new("007")), OsString::from("007.png"));
    }

    #[test]
    fn stem_rejects_other_extensions_case_and_empty() {
        assert_eq!(clean_overlay_stem("001.PNG"), None);
        assert_eq!(clean_overlay_stem("001.webp"), None);
        assert_eq!(clean_overlay_stem(".png"), None);
        assert_eq!(clean_overlay_stem("001"), None);
    }

    #[test]
    fn name_key_folds_ascii_case_only_when_case_insensitive() {
        let canonical = clean_overlay_file_name("Page1");
        // Case-sensitive (Linux): only the exact name matches.
        assert_eq!(clean_name_key("Page1.png", false), clean_name_key(&canonical, false));
        assert_ne!(clean_name_key("PAGE1.PNG", false), clean_name_key(&canonical, false));
        assert!(matches!(clean_name_key("PAGE1.PNG", false), Cow::Borrowed(_)));
        // Case-insensitive (Windows): any ASCII case variant of the canonical name matches.
        assert_eq!(clean_name_key("PAGE1.PNG", true), clean_name_key(&canonical, true));
        assert_eq!(clean_name_key("page1.Png", true), clean_name_key(&canonical, true));
        assert!(matches!(clean_name_key("page1.png", true), Cow::Borrowed(_)));
        // Other extensions and other stems never match, and non-ASCII letters are not folded.
        assert_ne!(clean_name_key("PAGE1.WEBP", true), clean_name_key(&canonical, true));
        assert_ne!(clean_name_key("PAGE2.PNG", true), clean_name_key(&canonical, true));
        assert_ne!(clean_name_key("Ä.png", true), clean_name_key(&clean_overlay_file_name("ä"), true));
    }

    #[test]
    fn page_stems_and_writer_fallback() {
        assert_eq!(page_clean_stem(Path::new("src/001.jpg")), Some("001"));
        assert_eq!(writer_clean_stem(Path::new("src/001.jpg")), "001");
        assert_eq!(page_clean_stem(Path::new("")), None);
        assert_eq!(writer_clean_stem(&PathBuf::new()), FALLBACK_CLEAN_STEM);
    }

    #[test]
    fn fit_rule_is_exact_with_clean_error_precedence() {
        assert_eq!(classify_clean_fit(Ok([4, 5]), || Ok([4, 5])), CleanPageFit::Matches { size: [4, 5] });
        assert_eq!(
            classify_clean_fit(Ok([4, 6]), || Ok([4, 5])),
            CleanPageFit::SizeMismatch { clean: [4, 6], page: [4, 5] }
        );
        assert_eq!(
            classify_clean_fit(Ok([4, 5]), || Err("page".to_string())),
            CleanPageFit::PageUnreadable { clean: [4, 5], error: "page".to_string() }
        );
        // An unreadable clean wins and the page is never consulted.
        let fit = classify_clean_fit(Err("clean".to_string()), || -> Result<[u32; 2], String> {
            panic!("page header must not be read when the clean is unreadable")
        });
        assert_eq!(fit, CleanPageFit::CleanUnreadable("clean".to_string()));
    }
}
