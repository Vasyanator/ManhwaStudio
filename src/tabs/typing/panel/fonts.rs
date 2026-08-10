/*
File: panel/fonts.rs

Purpose:
Free-function helpers extracted verbatim from panel.rs for font discovery and
loading.

Main responsibilities:
- discover and load fonts from the project fonts directory PLUS a user-curated list
  of imported system-font FILE paths (`load_fonts` / `load_imported_system_fonts`);
- read every font FILE exactly ONCE (`read_font_file` -> `FontFileData`): one
  `fs::read` plus one `fontdb` parse yield the face list (with each face's
  PostScript name), the representative face's family name, the language coverage
  and the content hash together, with the bytes shared by `Arc` and never copied;
- merge duplicate font files (key: valid PostScript name + content hash, so
  byte-identical copies merge across differing FILE NAMES) and assign disambiguating
  group labels; the same key folds a folder font together with a byte-identical
  IMPORTED copy of it on the combined list (`merge_duplicate_font_entries`);
- validate the PostScript name a face claims (`is_valid_post_script_name`): a name the
  spec forbids is treated as ABSENT, which is what keeps the identity namespace clean;
- assign the canonical render IDENTITY of every panel font
  (`assign_font_identity_names`): the representative face's PostScript name, suffixed
  with `%{16 hex of the content hash}` when another file claims the same name with
  different bytes, when a user font claims the reserved bundled-UI name, or when the
  base identity itself contains the (spec-forbidden) separator;
- list font groups, compute font-file content hashes, and recurse font-file
  directories.
- run the DEFERRED schema-1 -> schema-2 `fonts_data.json` migration
  (`run_pending_fonts_data_migration`) at the end of the COMBINED list build — the first
  moment a `path -> identity` map exists AND the identities are final. The folder-only
  subset (`folder_font_entries`) deliberately does NOT run it, because its identities are
  pre-collision; `legacy_font_settings_key` is the read-only remains of the v1 keying rule
  and exists only to feed it.
- inject the user-defined VIRTUAL font groups into a finalized panel list
  (`apply_virtual_groups`): append each membership into a font's `groups` and its
  optional per-group alias into `virtual_group_aliases`, returning the merged
  (real + virtual) combobox group list. MUST run after merge/disambiguation/identity.
- `load_system_fonts` enumerates ALL OS-installed fonts; it is the catalog source
  for the settings font-import picker (`panel/font_settings.rs`), run off-thread,
  and it publishes what it enumerated as the process-wide system-font NAME INDEX.
- locate an imported system font BY NAME when its recorded path hint fails
  (`SystemFontNameIndex` / `system_font_name_index` / `locate_system_font_by_identity`):
  a lazily built, process-cached `PostScript name -> file(s)` map over the installed
  faces, so a system font that moved or was repackaged re-links itself. HEAVY to
  build (whole OS font database) — every caller is off the GUI thread.
  `locate_system_font_file_by_identity` is the typing-wide (`font_admin`) wrapper over it,
  returning just the confirmed identity plus the file path.
- offer the bundled `fonts/ui` stack as ONE selectable font of the PANEL list
  (`BUNDLED_UI_FONT_IDENTITY`, `bundled_ui_font_entry`, `prepend_bundled_ui_font`);
  see "Built-in interface font" in `MODULE_README.md` for the full contract.

Notes:
Extracted verbatim from panel.rs. Free fns are pub(super) so panel.rs can use
them. use super::*; pulls in the parent module's types and imports.
*/

use super::*;
use std::sync::{Mutex, OnceLock, RwLock};

/// Applies the user display-name overrides to a FINALIZED font list, keyed by each entry's
/// render IDENTITY. DISPLAY ONLY: the result feeds `FontEntry.display_name`, never the
/// render/inline-tag identity.
///
/// MUST run AFTER `assign_font_identity_names`, because the identity is the key: running it
/// earlier would look up the per-entry base identity and miss a `%hash`-suffixed one. A
/// merged cluster of byte-identical copies has exactly ONE override slot (its identity), so
/// no per-path scan is needed — that was the path-keyed era's workaround.
fn apply_display_name_overrides(entries: &mut [FontEntry]) {
    for entry in entries.iter_mut() {
        entry.display_name =
            font_settings_store::font_display_name_override(&entry.render_identity_name());
    }
}

/// Normalizes a font identity for COMPARISON and MAP KEYS: `trim` + ASCII lowercase.
///
/// The single definition of identity normalization, mirrored by
/// `font_provider::normalize_name` (and therefore by the renderer's
/// `normalize_inline_font_label`), so a name keys identically wherever it is looked up.
/// Identities are always STORED with their original casing; only comparisons fold it.
///
/// Visible to the whole typing module so `font_admin` can re-export it: the settings font
/// UI must decide "is this stored member identity loaded?" by exactly this rule, or it
/// flags as missing a member the panel resolves fine.
#[must_use]
pub(in crate::tabs::typing) fn normalize_font_identity(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// Separator between a base font identity and its content-hash collision suffix.
///
/// It is deliberately a character the PostScript-name spec FORBIDS. A suffixed identity
/// therefore lives in a namespace no spec-valid PostScript name can reach, so a font
/// whose real name happens to look like `"Acme%1122334455667788"` can never claim the
/// suffixed identity of a different font called `Acme` — the ambiguity is impossible BY
/// CONSTRUCTION rather than caught by a check. `/` would serve equally well;
/// `%` is chosen because identities are shown next to file paths (settings font
/// properties, log lines), where a `/` would read as a path separator.
///
/// The guarantee is completed by two rules: a face's PostScript name is only accepted
/// when [`is_valid_post_script_name`] holds (an invalid one counts as ABSENT), and a base
/// identity that still contains this separator — only reachable through the
/// family-or-label fallback of a file with no valid PostScript name — is suffixed
/// unconditionally by [`assign_font_identity_names`], so its suffixed form cannot equal
/// another font's suffixed form either.
pub(super) const IDENTITY_HASH_SEPARATOR: char = '%';

/// Characters the PostScript language reserves as token delimiters, and which a
/// PostScript font name (`name` table id 6) therefore may not contain.
const POST_SCRIPT_NAME_FORBIDDEN_CHARS: [char; 10] =
    ['[', ']', '(', ')', '{', '}', '<', '>', '/', '%'];

/// Maximum length of a PostScript name; the OpenType `name` specification pins name id 6
/// at no more than 63 characters.
const POST_SCRIPT_NAME_MAX_LEN: usize = 63;

/// Whether `name` is a spec-valid PostScript font name and may therefore be used as a
/// font IDENTITY.
///
/// Valid means: after trimming surrounding whitespace, 1..=63 characters, every one of
/// them printable ASCII (`0x21..=0x7E`) and none of them a PostScript delimiter
/// (`[](){}<>/%`). Everything else — an empty name, an over-long one, an interior space,
/// a control character, any non-ASCII character — is INVALID and is treated as "the face
/// carries no PostScript name" by [`validated_post_script_name`].
///
/// Surrounding whitespace is trimmed before validating because every identity comparison
/// in the app trims (`normalize_font_identity`), so a name that only offends by a
/// trailing space is not thrown away; an INTERIOR space still invalidates it.
#[must_use]
pub(in crate::tabs::typing) fn is_valid_post_script_name(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || name.len() > POST_SCRIPT_NAME_MAX_LEN {
        return false;
    }
    name.chars()
        .all(|ch| ch.is_ascii_graphic() && !POST_SCRIPT_NAME_FORBIDDEN_CHARS.contains(&ch))
}

/// The trimmed PostScript name when it is spec-valid ([`is_valid_post_script_name`]),
/// otherwise the EMPTY string — i.e. an invalid name is reported as no name at all, and
/// the caller falls back to the documented family-or-label rule.
///
/// This is what keeps the identity namespace clean: a name that cannot be a PostScript
/// name never becomes an identity, so no identity can contain
/// [`IDENTITY_HASH_SEPARATOR`] except a deliberately suffixed one.
#[must_use]
pub(super) fn validated_post_script_name(raw: &str) -> &str {
    if is_valid_post_script_name(raw) {
        raw.trim()
    } else {
        ""
    }
}

/// UNSUFFIXED identity claimed by one font file: its representative face's PostScript
/// name (`name` table id 6), with the documented family-or-label FALLBACK for a file that
/// has no parsed face to read one from (see [`base_font_identity_str`]).
#[must_use]
pub(super) fn base_font_identity_name(
    post_script_name: &str,
    original_name: &str,
    label: &str,
) -> String {
    base_font_identity_str(post_script_name, original_name, label).to_string()
}

/// Borrowing form of [`base_font_identity_name`] — the SAME rule, without allocating.
///
/// The rule: the trimmed PostScript name when it is SPEC-VALID
/// ([`is_valid_post_script_name`]); when the file has none, the trimmed original FAMILY
/// name; when it has neither, the file-stem `label`. `fontdb` refuses a face without a
/// PostScript name (`LoadError::UnnamedFont`), so the fallback runs for entries with no
/// parsed face at all — a font file that could not be read or could not be parsed, which
/// is still LISTED so the user sees what they put in the fonts folder — and for the rare
/// file whose name table holds a name the PostScript spec forbids. Such an entry must
/// still get a meaningful, stable identity instead of an empty one, and this
/// family-or-label rule is that identity.
///
/// The validation is repeated here (loaders already store a validated name on the face)
/// deliberately, as defence in depth: this is the single funnel every identity passes
/// through, so no construction path can inject a name carrying
/// [`IDENTITY_HASH_SEPARATOR`] into the identity namespace.
///
/// The borrowing form exists because the panel's ordered name resolver evaluates the base
/// identity of every font on every lookup (and the font combo runs a lookup per frame),
/// where an allocation per entry is pure waste. The returned slice is not trimmed in the
/// label case; callers compare with `trim()`, exactly as `normalize_font_identity` would.
#[must_use]
pub(super) fn base_font_identity_str<'a>(
    post_script_name: &'a str,
    original_name: &'a str,
    label: &'a str,
) -> &'a str {
    let post_script = validated_post_script_name(post_script_name);
    if !post_script.is_empty() {
        return post_script;
    }
    let original = original_name.trim();
    if original.is_empty() { label } else { original }
}

/// Builds the COLLISION-SUFFIXED identity
/// `"{base}{IDENTITY_HASH_SEPARATOR}{16 hex digits of the content hash}"` for a font
/// whose base identity is claimed by another file with different bytes.
///
/// The suffix is a pure function of the entry's OWN bytes, never of the other
/// claimant's presence or of a list position: an ordinal suffix would renumber the
/// remaining entries whenever a claimant appears or disappears, invalidating everything
/// already persisted under the old number.
///
/// The WHOLE 64-bit `font_content_hash` is spelled out (16 hex digits), not a truncation
/// of it: contest detection compares full hashes, so a shorter suffix would let two files
/// that ARE recognized as different receive one identity — the collision the suffix
/// exists to prevent.
#[must_use]
pub(super) fn suffixed_font_identity_name(base: &str, content_hash: u64) -> String {
    format!(
        "{}{IDENTITY_HASH_SEPARATOR}{content_hash:016x}",
        base.trim()
    )
}

/// Reserved render/inline-tag IDENTITY of the synthetic bundled `fonts/ui` entry.
///
/// It is persisted into projects (`font_label` / `font_original_name` /
/// `TextRenderParams.font_name`) and emitted in `<font=...>` tags, so it MUST stay a
/// fixed, non-localized ASCII string: a project saved under a Russian interface has
/// to open identically under an English one. The human-readable name lives in the
/// catalog instead (`typing.fonts.bundled_ui_font_label`, resolved by
/// `FontEntry::display_label`) — see `dev-docs/i18n_exclusions.md`.
///
/// Collision policy: no user font may claim this name in EITHER spelling. A user font
/// whose PostScript name matches is given a content-hash-suffixed identity by
/// `assign_font_identity_names` (so it is still reachable, just not under the reserved
/// name), and the synthetic entry additionally stays the FIRST element of the panel
/// font list and the FIRST key inserted into `TabFontProvider`, so the shadowing is
/// consistent between the panel and the renderer.
pub(in crate::tabs::typing) const BUNDLED_UI_FONT_IDENTITY: &str = "ManhwaStudio-UI";

/// Previous spelling of `BUNDLED_UI_FONT_IDENTITY`, kept as a READ-ONLY resolution
/// alias.
///
/// Projects saved before the identity became the PostScript name persisted this exact
/// string in `font_label` / `font_original_name` / `<font=...>` tags, so it must keep
/// resolving to the bundled entry (`TabFontProvider::from_fonts`, and the panel through
/// the bundled entry's `original_name`). It is NEVER written any more: a stored document
/// still carrying it is rewritten to the current spelling by
/// `tab/codec::upgrade_text_params_to_v2`.
pub(in crate::tabs::typing) const BUNDLED_UI_FONT_LEGACY_IDENTITY: &str = "ManhwaStudio UI";

/// Whether `name` claims the reserved bundled-UI identity in EITHER spelling
/// (case-insensitively). Used to keep a user font from taking the reserved name.
///
/// Visible to the whole typing module so `font_admin` can re-export it: the synthetic
/// bundled entry is NOT part of the settings font categories, yet the panel list carries
/// it (`prepend_bundled_ui_font`), so a group member holding this identity is present for
/// the panel and must not be reported as missing by the settings UI.
#[must_use]
pub(in crate::tabs::typing) fn is_reserved_bundled_identity(name: &str) -> bool {
    let normalized = normalize_font_identity(name);
    normalized == normalize_font_identity(BUNDLED_UI_FONT_IDENTITY)
        || normalized == normalize_font_identity(BUNDLED_UI_FONT_LEGACY_IDENTITY)
}

/// Builds the synthetic panel entry for the bundled `fonts/ui` stack, or `None` when
/// this process could not resolve the stack at all (then the option is simply not
/// offered — nothing is faked).
///
/// The entry points at the FIRST `core` file of the stack, which is a real, readable
/// font file. That is deliberate and is what keeps every per-entry consumer working
/// without a special case: the own-typeface combo preview, the advanced-form width
/// metric and the PSD export all read `FontEntry::path`. Selecting it makes the
/// renderer use that file as the selected face, and the REST of the stack follows
/// automatically through `MsFallback::common_fallback`
/// (`dev-docs/unicode_base_font_plan.md`, layer 3) — there is no "font chain" type,
/// because `FontContent` carries the bytes of exactly one file.
///
/// `original_name` is a reserved spelling rather than the core font's real family
/// name ("Noto Sans"). If it were the family name, a build WITHOUT this feature would
/// silently resolve the overlay to whatever "Noto Sans" it finds in the user's own
/// font folder; with the reserved name it degrades to the normal "font not found"
/// state instead. It holds the LEGACY spelling specifically: `original_name` is the
/// family-name resolution alias on both sides (panel `font_matches_original_name` and
/// `TabFontProvider`), so parking the previous identity there is what keeps projects
/// saved before the rename selecting the built-in font in the PANEL too, not only in
/// the renderer.
///
/// Blocking I/O on first use: `ms_fonts::stack()` reads the `name` table of each
/// bundled file (kilobytes each). It is normally already resolved by `ui_fonts` at
/// startup, and the surrounding panel font loading reads whole font files anyway.
#[must_use]
pub(super) fn bundled_ui_font_entry() -> Option<FontEntry> {
    let core = ms_fonts::stack()?.core().first()?;
    Some(FontEntry {
        kind: FontEntryKind::BundledUiStack(core),
        label: BUNDLED_UI_FONT_IDENTITY.to_string(),
        path: core.path.clone(),
        alt_paths: Vec::new(),
        groups: vec![None],
        disambig: None,
        faces: default_single_face(),
        // Reported as FULL on purpose: the classifier can only measure the ONE file
        // an entry points at, but this entry stands for the whole bundled chain —
        // core + bold + the ~44 `ext` fonts the renderer reaches through
        // `MsFallback`, which together cover the overwhelming majority of assigned
        // Unicode. Classifying the single core file would understate that and paint
        // the option as `Partial` for languages the chain does serve.
        coverage: FontLanguageCoverage::default(),
        original_name: BUNDLED_UI_FONT_LEGACY_IDENTITY.to_string(),
        // Deliberately EMPTY: this entry stands for the whole bundled chain, not for
        // one face, and its identity is the reserved `BUNDLED_UI_FONT_IDENTITY`.
        // Reporting the core file's own PostScript name here would name a font
        // ("NotoSans-Regular") that the entry is not, and that a machine without the
        // bundle would resolve to something else entirely.
        post_script_name: String::new(),
        // No single file stands behind this entry, so there is nothing to hash; the
        // reserved identity never needs a collision suffix anyway.
        content_hash: 0,
        display_name: None,
        identity_name: BUNDLED_UI_FONT_IDENTITY.to_string(),
        virtual_group_aliases: BTreeMap::new(),
    })
}

/// Prepends the synthetic bundled-stack entry to a FINALIZED panel font list.
///
/// Must run AFTER `assign_font_identity_names` (the reserved identity is fixed and
/// must not be recomputed) and AFTER any sorting: position 0 is a contract, not
/// cosmetics. Both the panel's ordered name lookup
/// (`create_state::find_font_idx_by_name_forms`) and `TabFontProvider::from_fonts`
/// are FIRST-wins over the list order, so being first is what guarantees that a user
/// font claiming the reserved name loses in BOTH places rather than in only one.
///
/// A process without a resolvable `fonts/ui` stack simply does not get the entry
/// (logged once per reload); the panel keeps working with the user's own fonts.
///
/// BOTH reserved spellings are checked: a user font matching the previous
/// `BUNDLED_UI_FONT_LEGACY_IDENTITY` must not be able to take over documents that
/// still name the built-in font that way.
pub(super) fn prepend_bundled_ui_font(entries: &mut Vec<FontEntry>) {
    let Some(entry) = bundled_ui_font_entry() else {
        crate::runtime_log::log_warn(
            "typing fonts: the bundled fonts/ui stack is unavailable, so the built-in interface \
             font is not offered in the font list",
        );
        return;
    };
    for reserved_name in [BUNDLED_UI_FONT_IDENTITY, BUNDLED_UI_FONT_LEGACY_IDENTITY] {
        let reserved = normalize_font_identity(reserved_name);
        for font in entries.iter() {
            if font_claims_name_in_any_form(font, &reserved) {
                crate::runtime_log::log_warn(format!(
                    "typing fonts: '{}' ({}) claims the reserved name '{reserved_name}' of the \
                     built-in interface font; the built-in entry keeps the name in both the \
                     panel and the renderer, and that font stays reachable by its other forms \
                     only.",
                    font.label,
                    font.path.display()
                ));
            }
        }
    }
    entries.insert(0, entry);
}

/// Whether `font` claims `name_norm` (an already-normalized name) in ANY resolution form:
/// its base identity, its family name, its file-stem label, or its path stem.
///
/// DIAGNOSTIC ONLY — it answers "could this font have been meant by that name?", which is
/// exactly what the reserved-name warning asks. Resolution itself never uses a
/// form-agnostic union: it runs the ordered per-form passes of
/// `create_state::find_font_idx_by_name_forms`, mirroring `TabFontProvider`.
#[must_use]
fn font_claims_name_in_any_form(font: &FontEntry, name_norm: &str) -> bool {
    if normalize_font_identity(&font.base_identity_name()) == name_norm
        || normalize_font_identity(&font.original_name) == name_norm
        || normalize_font_identity(&font.label) == name_norm
    {
        return true;
    }
    font.path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| normalize_font_identity(stem) == name_norm)
}

/// Computes the render/inline-tag identity of every font in a FINALIZED panel list and
/// writes it into `FontEntry.identity_name`.
///
/// Rule: the identity is the representative face's POSTSCRIPT NAME, kept with its
/// original casing (`base_font_identity_name`; a file with no valid, parsed name falls
/// back to family-or-label). Three cases append a
/// `{IDENTITY_HASH_SEPARATOR}{16 hex of the content hash}` suffix:
///
/// - **Contested name.** The same base name is claimed by files with DIFFERENT content
///   hashes. Byte-identical claimants are the same font and keep the bare name (the
///   folder loader already folds those into one entry, and `load_fonts` folds an
///   imported copy of a folder font into it as well). Every contested claimant is
///   suffixed from its OWN bytes, so its identity does not shift when another claimant is
///   added or removed, and the bare name stays a resolution alias for the lowest-hash
///   claimant (`TabFontProvider::from_fonts`). Logged once per contested name, listing
///   the files.
/// - **Reserved name.** A user font claiming `BUNDLED_UI_FONT_IDENTITY` or
///   `BUNDLED_UI_FONT_LEGACY_IDENTITY` is suffixed unconditionally, so the reserved name
///   can never resolve to a user font — not even in a build where the bundled stack is
///   unavailable and nothing would shadow it.
/// - **Base carrying the separator.** Only reachable through the family-or-label
///   FALLBACK (a valid PostScript name cannot contain `IDENTITY_HASH_SEPARATOR`), e.g. a
///   broken file whose family name literally reads `"Acme%1122334455667788"`. Suffixing
///   it unconditionally keeps the suffixed namespace injective: such a base can no longer
///   pose as another font's suffixed identity, because its own identity carries one more
///   separator than that form does.
///
/// Idempotent: recomputes purely from `post_script_name`/`original_name`/`label` and the
/// content hashes, so calling it again on an already-assigned list is a no-op.
///
/// The synthetic bundled-stack entry is skipped ENTIRELY — it neither receives an
/// identity here (its reserved one is fixed, `BUNDLED_UI_FONT_IDENTITY`) nor counts as a
/// claimant of any name.
pub(super) fn assign_font_identity_names(fonts: &mut [FontEntry]) {
    // Base (unsuffixed) identity per entry, in list order. `None` marks the synthetic
    // bundled entry, whose reserved identity is fixed at construction.
    let bases: Vec<Option<String>> = fonts
        .iter()
        .map(|font| {
            if font.bundled_stack_font().is_some() {
                None
            } else {
                Some(font.base_identity_name())
            }
        })
        .collect();

    // Who claims each normalized base name, and with which bytes.
    let mut claims: HashMap<String, Vec<(u64, PathBuf)>> = HashMap::new();
    for (font, base) in fonts.iter().zip(bases.iter()) {
        let Some(base) = base else { continue };
        let key = normalize_font_identity(base);
        // An empty base (an unreadable file with neither family name nor stem) cannot
        // be contested and is not a usable key anywhere.
        if key.is_empty() {
            continue;
        }
        claims
            .entry(key)
            .or_default()
            .push((font.content_hash, font.path.clone()));
    }

    // A name is contested only when its claimants disagree about the BYTES: identical
    // bytes under one name are the same font, whichever path they were found at.
    let contested: HashSet<String> = claims
        .iter()
        .filter(|(_, claimants)| {
            let mut hashes: Vec<u64> = claimants.iter().map(|(hash, _)| *hash).collect();
            hashes.sort_unstable();
            hashes.dedup();
            hashes.len() > 1
        })
        .map(|(name, _)| name.clone())
        .collect();

    // One warning per contested NAME (not per file), listing every claimant, so the
    // ambiguity is diagnosable without repeating the same line for each of them.
    for name in &contested {
        let Some(claimants) = claims.get(name) else {
            continue;
        };
        let mut files: Vec<String> = claimants
            .iter()
            .map(|(hash, path)| format!("{} (hash {hash:016x})", path.display()))
            .collect();
        files.sort();
        crate::runtime_log::log_warn(format!(
            "typing fonts: PostScript name '{name}' is claimed by {} files with DIFFERENT \
             content; each keeps a content-hash-suffixed render identity \
             ('{name}{IDENTITY_HASH_SEPARATOR}<16 hex>') and the bare name resolves to the \
             lowest-hash file. Files: {}",
            claimants.len(),
            files.join(", ")
        ));
    }

    for (font, base) in fonts.iter_mut().zip(bases.iter()) {
        let Some(base) = base else {
            // Reserved identity: fixed at construction, never recomputed.
            continue;
        };
        let reserved = is_reserved_bundled_identity(base);
        if reserved {
            crate::runtime_log::log_warn(format!(
                "typing fonts: '{}' ({}) claims the reserved built-in-interface-font name \
                 '{base}'; it is given a content-hash-suffixed identity instead, so the \
                 reserved name keeps resolving to the built-in entry only.",
                font.label,
                font.path.display()
            ));
        }
        // A base that already carries the separator can only come from the
        // family-or-label fallback; suffixing it keeps it out of the suffixed namespace
        // of the fonts whose identity it otherwise imitates.
        let imitates_suffixed_form = base.contains(IDENTITY_HASH_SEPARATOR);
        if imitates_suffixed_form {
            crate::runtime_log::log_warn(format!(
                "typing fonts: '{}' ({}) has no valid PostScript name and falls back to \
                 '{base}', which contains the identity separator \
                 '{IDENTITY_HASH_SEPARATOR}' reserved for collision suffixes; it is given a \
                 content-hash-suffixed identity so it cannot imitate another font's \
                 suffixed identity.",
                font.label,
                font.path.display()
            ));
        }
        let key = normalize_font_identity(base);
        font.identity_name = if reserved || imitates_suffixed_form || contested.contains(&key) {
            suffixed_font_identity_name(base, font.content_hash)
        } else {
            base.clone()
        };
    }
}

pub(in crate::tabs::typing) fn resolve_fonts_dir() -> PathBuf {
    if let Ok(cwd) = env::current_dir() {
        let candidate = cwd.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }

    if let Ok(exe_path) = env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }

    PathBuf::from("fonts")
}

/// Everything the font list needs out of ONE font FILE, produced by a SINGLE
/// `fs::read` and a SINGLE `fontdb` parse of the resulting bytes.
///
/// Previously each file was read once but parsed two or three times (face list,
/// original name, import probe), each time into a throwaway `fontdb::Database` fed a
/// fresh `bytes.to_vec()` copy. `read_font_file` replaces all of those; the bytes are
/// handed to `fontdb` through the same `Arc` the hash and the coverage classifier
/// read, so nothing is copied.
pub(super) struct FontFileData {
    /// Hash of the raw file bytes — one half of the duplicate-merge key.
    pub(super) content_hash: u64,
    /// Selectable faces in file order (`face_index` == position in the file). NEVER
    /// empty: an unparsable file yields the single placeholder face and `parsed == false`.
    pub(super) faces: Vec<FontFaceEntry>,
    /// Family name (`name` id 1) of the representative face, falling back to its
    /// PostScript name. Empty when the file could not be parsed or carried neither —
    /// callers substitute the file stem, exactly as before.
    pub(super) original_name: String,
    /// Coverage of the representative face against the current TYPESETTING language,
    /// classified from the same bytes.
    pub(super) coverage: FontLanguageCoverage,
    /// Whether `fontdb` accepted the file at all. `false` means `faces` is the
    /// placeholder and both names are empty; the folder loader still lists such a
    /// file, while the imported-fonts loader skips it.
    pub(super) parsed: bool,
}

impl FontFileData {
    /// PostScript name (`name` id 6) of the REPRESENTATIVE face — the first face of the
    /// file, the same one `original_name` and `coverage` describe. Empty for a file
    /// `fontdb` could not parse, and for a face whose declared name is not spec-valid
    /// (`is_valid_post_script_name`), which counts as no name at all.
    pub(super) fn post_script_name(&self) -> &str {
        self.faces
            .first()
            .map_or("", |face| face.post_script_name.as_str())
    }
}

/// Test-only journal of every font FILE that went through `read_font_file`, in call
/// order, each entry paired with the number of `fontdb` PARSES that read performed.
///
/// The Phase-0 contract is "one read AND one parse per font file". The read count is the
/// number of journal entries for a path; the parse count is recorded separately because
/// counting calls to `read_font_file` alone would NOT notice a regression that builds a
/// second throwaway `fontdb::Database` inside the very same read — exactly the shape of
/// the code phase 0 removed. Tests filter the journal by their OWN unique temp paths, so
/// unit tests running in parallel cannot disturb each other's counts.
#[cfg(test)]
pub(super) static FONT_FILE_PARSE_JOURNAL: std::sync::Mutex<Vec<(PathBuf, usize)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
thread_local! {
    /// Test-only running count of `fontdb::Database` builds performed by
    /// `font_file_data_from_bytes` ON THIS THREAD. Thread-local rather than global so
    /// that fonts parsed by other tests running in parallel cannot be attributed to the
    /// file this thread is reading.
    static FONT_DB_PARSES_ON_THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Records that one `fontdb::Database` was built to parse font bytes on this thread.
#[cfg(test)]
fn note_font_db_parse() {
    FONT_DB_PARSES_ON_THREAD.with(|count| count.set(count.get().saturating_add(1)));
}

/// Number of `fontdb` parses performed on this thread so far (test-only).
#[cfg(test)]
fn font_db_parses_on_thread() -> usize {
    FONT_DB_PARSES_ON_THREAD.with(std::cell::Cell::get)
}

/// Records one font-file read in `FONT_FILE_PARSE_JOURNAL` together with how many
/// `fontdb` parses it performed, recovering the lock when a panicking test poisoned it (a
/// poisoned journal must not cascade into unrelated tests).
#[cfg(test)]
fn note_font_file_parse(path: &Path, parses: usize) {
    let mut journal = match FONT_FILE_PARSE_JOURNAL.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    journal.push((path.to_path_buf(), parses));
}

/// Reads `path` once and derives everything the font list needs from its bytes.
///
/// # Errors
/// Returns the underlying `std::io::Error` when the file cannot be read (missing,
/// unreadable, permissions). A file that reads but does NOT parse is not an error
/// here: the result carries `parsed == false`, and each loader decides whether to
/// list it (folder fonts) or skip it (imported system fonts).
pub(super) fn read_font_file(path: &Path) -> std::io::Result<FontFileData> {
    let bytes = Arc::new(fs::read(path)?);
    // The journal entry is written AFTER the parse and carries how many `fontdb`
    // databases that parse built, so the one-parse-per-file contract is pinned by what
    // actually happened rather than by the number of calls to this function.
    #[cfg(test)]
    let parses_before = font_db_parses_on_thread();
    let data = font_file_data_from_bytes(&bytes);
    #[cfg(test)]
    note_font_file_parse(
        path,
        font_db_parses_on_thread().saturating_sub(parses_before),
    );
    Ok(data)
}

/// Single-parse core of `read_font_file`, split out so it can be exercised on bytes
/// that have no file behind them.
///
/// `bytes` is shared with the temporary `fontdb::Database` by `Arc::clone`, so the file
/// content exists exactly once in memory for the duration of the call.
pub(super) fn font_file_data_from_bytes(bytes: &Arc<Vec<u8>>) -> FontFileData {
    let content_hash = font_content_hash(bytes.as_slice());
    let mut db = fontdb::Database::new();
    #[cfg(test)]
    note_font_db_parse();
    // The database borrows the SAME allocation (one more `Arc` handle, no copy);
    // the explicit type is only the unsizing coercion `fontdb::Source::Binary` wants.
    let shared: Arc<dyn AsRef<[u8]> + Send + Sync> = bytes.clone();
    let ids = db.load_font_source(fontdb::Source::Binary(shared));

    let mut faces = Vec::with_capacity(ids.len());
    for (idx, id) in ids.iter().enumerate() {
        let (label, post_script_name) = match db.face(*id) {
            Some(face) => {
                let family = face
                    .families
                    .first()
                    .map(|(name, _)| name.as_str())
                    .unwrap_or("Unknown");
                let style = match face.style {
                    fontdb::Style::Normal => "Normal",
                    fontdb::Style::Italic => "Italic",
                    fontdb::Style::Oblique => "Oblique",
                };
                // The stored name is VALIDATED (an invalid one counts as absent, so it
                // can never become an identity), while the DISPLAY label keeps the raw
                // string: when a font does carry a malformed name, seeing it in the face
                // combo is what makes the situation diagnosable.
                let validated = validated_post_script_name(&face.post_script_name);
                if validated.is_empty() && !face.post_script_name.trim().is_empty() {
                    crate::runtime_log::log_warn(format!(
                        "typing fonts: face #{idx} of a font file declares the PostScript \
                         name '{}', which the PostScript specification forbids (printable \
                         ASCII without '[](){{}}<>/%' or spaces, 1..=63 chars); the name is \
                         ignored and the font's identity falls back to its family name.",
                        face.post_script_name
                    ));
                }
                (
                    format!(
                        "#{idx} {family} | {style} | w{} | {}",
                        face.weight.0, face.post_script_name
                    ),
                    validated.to_string(),
                )
            }
            // Defensive: `load_font_source` just handed us this id, so the face must
            // exist; keep a usable entry instead of dropping a selectable face.
            None => (format!("#{idx} Face"), String::new()),
        };
        faces.push(FontFaceEntry {
            label,
            face_index: idx,
            post_script_name,
        });
    }

    // Representative face = the first one, matching `FontEntry::representative_face_index`.
    let representative = ids.first().and_then(|id| db.face(*id));
    let original_name = representative
        .and_then(|face| {
            face.families
                .first()
                .map(|(name, _)| name.clone())
                // Fallback to the PostScript name only when it is spec-valid: an invalid
                // one is treated as absent everywhere, and letting it in here would
                // re-introduce it as an identity through the family branch.
                .filter(|name| !name.is_empty())
                .or_else(|| {
                    Some(validated_post_script_name(&face.post_script_name).to_string())
                        .filter(|name| !name.is_empty())
                })
        })
        .unwrap_or_default();
    let representative_face_index = faces.first().map_or(0, |face| face.face_index);
    // Classified from the SAME bytes; an unparsable file classifies as the default
    // (`FontRef::from_index` rejects it), which is what the previous code produced too.
    let coverage =
        super::font_coverage::classify_font_bytes(bytes.as_slice(), representative_face_index);

    let parsed = !faces.is_empty();
    if !parsed {
        faces = default_single_face();
    }

    FontFileData {
        content_hash,
        faces,
        original_name,
        coverage,
        parsed,
    }
}

/// Builds the panel font list: all fonts from `fonts_dir` PLUS the user-curated
/// imported system-font FILE paths in `imported_system_paths`.
///
/// The folder fonts (with their duplicate-merge and group disambiguation) come first;
/// each imported path is appended as a single entry unless its file path already
/// belongs to a folder entry (matched against that entry's `path` or `alt_paths`), so an
/// imported copy of an already-present font is not listed twice. The merged list is
/// sorted case-insensitively by label. An empty `imported_system_paths` yields the sorted
/// folder fonts only.
///
/// The synthetic bundled-stack entry is prepended LAST (see
/// `prepend_bundled_ui_font`), so it stays at index 0 regardless of the sort. This is
/// a PANEL list; the settings font-administration list (`font_admin`) deliberately
/// does not get the entry — there is nothing to administer about it.
pub(super) fn load_fonts(fonts_dir: &Path, imported_system_paths: &[PathBuf]) -> Vec<FontEntry> {
    // The panel knows the imported fonts only as path hints; recover the name recorded beside
    // each so the combined builder can apply the same "the file must still claim that name"
    // rule the settings list gets.
    let mut refs: Vec<fonts_data::SystemFontRef> = imported_system_paths
        .iter()
        .map(|path| fonts_data::SystemFontRef {
            font: font_settings_store::system_font_identity_for_path(path).unwrap_or_default(),
            last_path: Some(path.clone()),
        })
        .collect();
    // A stored entry with NO path hint contributes no path, so the caller's snapshot cannot
    // carry it — yet such an entry can still be located BY NAME. Add those directly from the
    // store, or the panel would be missing a font the settings list resolves and shows.
    refs.extend(
        font_settings_store::imported_system_font_refs()
            .into_iter()
            .filter(|reference| reference.last_path.is_none()),
    );
    let mut entries = build_combined_font_list(fonts_dir, &refs).entries;
    prepend_bundled_ui_font(&mut entries);
    entries
}

/// The COMBINED font list — folder fonts and imported system fonts merged, sorted and
/// identity-assigned — plus one row per stored imported system font.
///
/// It exists because the panel and the settings font administration must speak the SAME
/// identities. Building the two categories independently (as the settings pane used to) hides
/// every cross-source name collision from one of them: the panel would suffix a contested
/// name with `%hash` while the settings pane still showed and wrote the bare name, so a group
/// membership or a display-name override created there matched no panel entry and silently
/// did nothing.
pub(in crate::tabs::typing) struct CombinedFontList {
    /// Merged, sorted, identity-assigned entries with display-name overrides applied. Does
    /// NOT include the synthetic bundled-UI entry — that belongs to the PANEL list only and
    /// is prepended by `load_fonts`.
    pub entries: Vec<FontEntry>,
    /// One row per entry of `fonts_data.json`'s `system_fonts`, in stored order, INCLUDING
    /// the ones that could not be loaded this run.
    pub imported_rows: Vec<ImportedSystemFontRow>,
}

/// Why an imported system font recorded in `fonts_data.json` could not be loaded this run.
///
/// The store entry survives all of these — it is the user's record that they imported that
/// font — so the settings UI shows the row as unavailable and offers to remove it. Silently
/// skipping it (the previous behavior) made such an entry impossible to get rid of: no row
/// existed to click, and nothing ever pruned the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tabs::typing) enum ImportedFontUnavailable {
    /// The entry records no `last_path` at all, so step 1 (read the hinted file) had nothing
    /// to read. Like every other variant here, it is only REPORTED once the by-name lookup
    /// has failed too — it describes what happened at the recorded path, not the whole search.
    NoPathHint,
    /// The recorded file is missing or unreadable; carries the OS error.
    Unreadable(String),
    /// The file exists and is readable, but no font parser accepts it.
    Unparsable,
    /// The file now holds a DIFFERENT font than the one that was imported (it was replaced).
    /// Carries the name the file claims now.
    NameMismatch {
        /// The identity the file at `last_path` claims today.
        found: String,
    },
}

/// One imported system font as the settings list needs it: what the DOCUMENT records, plus
/// the loaded entry when the file could be used.
///
/// `FontEntry` is deliberately not `Debug` (it carries whole face lists), so this type is not
/// either; its diagnosable half is `stored_identity` / `last_path` / `unavailable`.
pub(in crate::tabs::typing) struct ImportedSystemFontRow {
    /// The identity stored in `fonts_data.json` — the key `remove_imported_system_font`
    /// matches. It is the UNSUFFIXED PostScript name; it is NOT necessarily the loaded
    /// entry's render identity, which can carry a `%hash` collision suffix.
    pub stored_identity: String,
    /// The recorded path hint, shown to the user and used to read the bytes. Never a key.
    pub last_path: Option<PathBuf>,
    /// The loaded entry (with its final, collision-aware identity), or `None`.
    pub entry: Option<FontEntry>,
    /// Why the font could not be loaded; `None` exactly when `entry` is `Some`.
    pub unavailable: Option<ImportedFontUnavailable>,
}

/// Builds the combined font list plus the imported-font rows.
///
/// Order of operations is a contract (mirrored from the panel loader): folder fonts first,
/// then the loadable imported fonts appended, then the cross-source byte-identical fold, the
/// label renumbering, the sort, and only then the collision-aware identity assignment — a
/// folder font's name may be contested by an imported one, so identity cannot be resolved on
/// either subset alone. The deferred v1 migration and the display-name overrides run last,
/// against those final identities.
pub(in crate::tabs::typing) fn build_combined_font_list(
    fonts_dir: &Path,
    imported_refs: &[fonts_data::SystemFontRef],
) -> CombinedFontList {
    let mut entries = folder_font_entries(fonts_dir);

    // Paths already covered by a folder entry (its own path plus merged-duplicate
    // `alt_paths`); an imported path matching any of these is skipped as a duplicate.
    // This catches only the SAME file imported twice; a byte-identical copy under another
    // path is folded by `merge_duplicate_font_entries` below.
    let mut known_paths: HashSet<PathBuf> = entries
        .iter()
        .flat_map(|font| std::iter::once(font.path.clone()).chain(font.alt_paths.iter().cloned()))
        .collect();
    let mut imported_rows = load_imported_system_font_rows(imported_refs);
    let mut imported_paths: Vec<PathBuf> = Vec::new();
    for row in &mut imported_rows {
        let Some(imported) = row.entry.take() else {
            continue;
        };
        if known_paths.insert(imported.path.clone()) {
            imported_paths.push(imported.path.clone());
            entries.push(imported);
        }
        // Either way the row is re-linked to its surviving list entry below; an imported copy
        // of a font already in the folder is represented by that folder entry.
    }
    // Fold byte-identical duplicates ACROSS the two sources; without this a folder font
    // and an imported copy of the same bytes stay two entries carrying ONE identity, and
    // the provider silently shadows one of them.
    merge_duplicate_font_entries(&mut entries);
    // The `(N)` suffixes of the imported labels were handed out BEFORE that fold, so a
    // folded copy can leave a gap (`… [system] (2)` with no unsuffixed sibling). Renumber
    // over the survivors.
    renumber_imported_system_font_labels(&mut entries, &imported_paths);
    entries.sort_by_key(|font| font.label.to_lowercase());
    // Assign the collision-aware render identity on the COMBINED list: a folder font's
    // family name may collide with an imported system font's, so identity must be
    // resolved after the merge, not on the folder-only subset.
    assign_font_identity_names(&mut entries);
    // This is the AUTHORITATIVE list — folder fonts and imported system fonts together.
    run_pending_fonts_data_migration(&entries, fonts_dir);
    // Overrides are keyed by identity, so they are resolved only now: the combined identity
    // assignment can have suffixed an entry, and the migration above can have re-keyed the
    // store.
    apply_display_name_overrides(&mut entries);
    relink_imported_rows(&mut imported_rows, &entries);
    CombinedFontList {
        entries,
        imported_rows,
    }
}

/// Re-attaches each loadable imported row to the entry that ended up REPRESENTING its file in
/// the finalized list.
///
/// The entry a row started with may have been folded into a byte-identical folder font, and
/// every surviving entry's identity was (re)assigned afterwards, so the row's own copy is
/// stale by construction. Matching is by FILE PATH — the one thing that survives the fold
/// (the representative absorbs the folded copy's path into `alt_paths`) — and the result is
/// the entry whose `render_identity_name()` the settings UI must use for per-font settings.
fn relink_imported_rows(rows: &mut [ImportedSystemFontRow], entries: &[FontEntry]) {
    for row in rows {
        let Some(path) = row.last_path.clone() else {
            continue;
        };
        if row.unavailable.is_some() {
            continue;
        }
        row.entry = entries
            .iter()
            .find(|entry| entry.path == path || entry.alt_paths.contains(&path))
            .cloned();
        if row.entry.is_none() {
            // Defensive: every loadable row's path was either pushed as an entry or folded
            // into one that absorbed it, so this cannot happen. Report rather than present a
            // row that silently claims to be loaded.
            row.unavailable = Some(ImportedFontUnavailable::Unreadable(
                "the loaded font did not survive the duplicate merge".to_string(),
            ));
            crate::runtime_log::log_warn(format!(
                "typing: imported system font '{}' loaded but matched no entry of the combined \
                 font list. Path: {}",
                row.stored_identity,
                path.display()
            ));
        }
    }
}

/// Base display label of an imported system font: `"{stem} [system]"`, before any ` (N)`
/// duplicate suffix. A pure function of the file path, so the numbering can be redone
/// later over a list from which duplicates have been folded out.
#[must_use]
fn imported_system_font_base_label(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("system font");
    format!("{stem} [system]")
}

/// Re-derives the `"{stem} [system]"` labels — including their ` (N)` duplicate suffixes —
/// of the entries whose REPRESENTATIVE path is one of `imported_paths`.
///
/// `load_imported_system_font_rows` numbers duplicate labels while it builds its own list,
/// i.e. BEFORE `merge_duplicate_font_entries` folds byte-identical copies into folder entries.
/// A folded copy takes its (unsuffixed) label with it, which used to leave the survivor
/// showing `"X [system] (2)"` with no `"X [system]"` anywhere in the list. Renumbering the
/// SURVIVORS closes that gap. Purely cosmetic: the label is a display string, never a
/// resolution key, and folder entries (whose path is not in `imported_paths`) are untouched.
fn renumber_imported_system_font_labels(entries: &mut [FontEntry], imported_paths: &[PathBuf]) {
    if imported_paths.is_empty() {
        return;
    }
    let imported: HashSet<&Path> = imported_paths.iter().map(PathBuf::as_path).collect();
    let mut used_labels: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter_mut() {
        if !imported.contains(entry.path.as_path()) {
            continue;
        }
        let base_label = imported_system_font_base_label(&entry.path);
        let count = used_labels.entry(base_label.clone()).or_insert(0);
        *count += 1;
        entry.label = if *count > 1 {
            format!("{base_label} ({count})")
        } else {
            base_label
        };
    }
}

/// Builds one [`ImportedSystemFontRow`] per stored imported system font, in stored order.
///
/// A loadable entry is labeled `"{stem} [system]"` (duplicate labels get a ` (N)` suffix,
/// mirroring `load_system_fonts`) and carries the faces/coverage/original name read from the
/// file bytes, an empty `alt_paths`, `groups = [None]`, and no `disambig`.
///
/// Every entry of the document produces a row — that is the difference from
/// [`load_imported_system_fonts`], and it is what finally makes an unavailable import
/// visible and removable in the settings UI. A row carries a loaded `FontEntry` when the
/// recorded file could be read, parsed, and still claims the recorded name; otherwise it
/// carries the typed reason it could not.
///
/// THE PATH IS A HINT, THE NAME IS THE KEY, and the font is located BY NAME when the hint
/// fails. Resolution of one entry, in order:
/// 1. the recorded `last_path` still exists AND its PostScript name still matches → use it,
///    without ever touching the system font index (a file that was replaced by a DIFFERENT
///    font under the same path does NOT match, and is never silently substituted);
/// 2. otherwise the font is looked up by NAME in the process-global system-font index
///    ([`locate_system_font_by_identity`]); on a hit the recorded hint is rewritten, so a
///    system font that was moved, repackaged or updated re-links itself automatically;
/// 3. otherwise the entry stays in the document and the row carries the typed reason the
///    HINT failed — the row is what makes it visible and removable.
///
/// A hint whose name is not recorded yet (an unmigrated v1 document) is accepted at step 1
/// and the name is LEARNED from it; such an entry cannot reach step 2, because there is no
/// name to look up.
pub(in crate::tabs::typing) fn load_imported_system_font_rows(
    refs: &[fonts_data::SystemFontRef],
) -> Vec<ImportedSystemFontRow> {
    let mut used_labels: HashMap<String, usize> = HashMap::new();
    let mut rows = Vec::with_capacity(refs.len());
    for reference in refs {
        let stored_identity = reference.font.trim().to_string();
        // STEP 1 — the recorded hint. A hit costs exactly one file read.
        let from_hint = match reference.last_path.as_deref() {
            None => Err(ImportedFontUnavailable::NoPathHint),
            Some(path) => load_system_font_file_as(path, &stored_identity),
        };
        let loaded = match from_hint {
            Ok(loaded) => {
                if stored_identity.is_empty() {
                    // Name not recorded yet (a legacy v1 entry): learn it from the file we
                    // just read, so the next launch can locate it by name.
                    font_settings_store::learn_system_font_identity(
                        &loaded.path,
                        &loaded.identity_name,
                    );
                }
                loaded
            }
            // STEP 2 — locate by NAME. Reached only when the hint failed, which is what
            // keeps a document whose hints all resolve from ever scanning the system.
            Err(reason) => match locate_system_font_by_identity(&stored_identity) {
                Some(found) => {
                    crate::runtime_log::log_info(format!(
                        "typing: the imported system font '{stored_identity}' was not usable at \
                         its recorded path ({reason:?}); it was located by name at {} and the \
                         recorded path was updated.",
                        found.path.display()
                    ));
                    // The hint follows the font, so the next launch resolves at step 1 again.
                    font_settings_store::set_system_font_path(
                        &stored_identity,
                        found.path.clone(),
                    );
                    found
                }
                // STEP 3 — nothing found; keep the entry and report why the hint failed.
                None => {
                    log_unavailable_imported_font(
                        &stored_identity,
                        reference.last_path.as_deref(),
                        &reason,
                    );
                    rows.push(ImportedSystemFontRow {
                        stored_identity,
                        last_path: reference.last_path.clone(),
                        entry: None,
                        unavailable: Some(reason),
                    });
                    continue;
                }
            },
        };

        let base_label = imported_system_font_base_label(&loaded.path);
        let count = used_labels.entry(base_label.clone()).or_insert(0);
        *count += 1;
        let label = if *count > 1 {
            format!("{base_label} ({count})")
        } else {
            base_label
        };

        let LoadedSystemFontFile {
            path,
            data,
            identity_name,
            original_name,
        } = loaded;
        // Read before `data.faces` is moved out below.
        let post_script_name = data.post_script_name().to_string();
        let display_name = font_settings_store::font_display_name_override(&identity_name);
        let entry = FontEntry {
            kind: FontEntryKind::File,
            label,
            path: path.clone(),
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces: data.faces,
            coverage: data.coverage,
            original_name,
            post_script_name,
            content_hash: data.content_hash,
            display_name,
            identity_name: identity_name.clone(),
            virtual_group_aliases: BTreeMap::new(),
        };
        rows.push(ImportedSystemFontRow {
            // A legacy entry whose name was just learned records that name from now on.
            stored_identity: if stored_identity.is_empty() {
                identity_name
            } else {
                stored_identity
            },
            last_path: Some(path),
            entry: Some(entry),
            unavailable: None,
        });
    }
    rows
}

/// Logs, with the context needed to diagnose it, why a stored imported system font could
/// neither be loaded from its recorded path nor located by name.
fn log_unavailable_imported_font(
    stored_identity: &str,
    last_path: Option<&Path>,
    reason: &ImportedFontUnavailable,
) {
    let where_ = last_path.map_or_else(
        || "no recorded path".to_string(),
        |path| format!("Path: {}", path.display()),
    );
    let detail = match reason {
        ImportedFontUnavailable::NoPathHint => {
            "no file path was ever recorded for it".to_string()
        }
        ImportedFontUnavailable::Unreadable(error) => {
            format!("the file cannot be read (Error: {error})")
        }
        ImportedFontUnavailable::Unparsable => "the file cannot be parsed as a font".to_string(),
        ImportedFontUnavailable::NameMismatch { found } => {
            format!("the file now holds a different font ('{found}')")
        }
    };
    crate::runtime_log::log_warn(format!(
        "typing: the imported system font '{stored_identity}' is unavailable: {detail}, and no \
         installed font declares that PostScript name. The entry is kept and listed as \
         unavailable so it can be removed. {where_}"
    ));
}

/// Scans `fonts_dir`, merges duplicate files and assigns labels/disambiguators and the
/// folder-list identity — everything that depends ONLY on the folder contents.
///
/// NOT AUTHORITATIVE, and deliberately store-FREE: it neither runs the deferred
/// `fonts_data.json` migration nor applies display-name overrides. Both belong to
/// [`build_combined_font_list`], the ONE pass that can see imported system fonts and can
/// therefore resolve a folder-vs-imported name contest. Letting the folder-only subset
/// finalize the migration wrote a PRE-collision identity into the store: a folder font
/// contested by an imported one looks uncontested here, so its legacy key was re-keyed to
/// the BARE name, the combined pass then suffixed both claimants, and the re-key — being
/// one-way, and with the bare name already counted as "already migrated" — was never
/// redone. The per-font setting or virtual-group membership then hung on an identity no
/// final entry carries.
pub(super) fn folder_font_entries(fonts_dir: &Path) -> Vec<FontEntry> {
    let mut files = Vec::<PathBuf>::new();
    collect_font_files_recursive(fonts_dir, fonts_dir, &mut files);
    files.sort_by_key(|path| path.to_string_lossy().to_lowercase());

    // Читаем и разбираем каждый файл РОВНО один раз: faces (с их PostScript-именами),
    // имя семейства представительного face, покрытие и хэш содержимого приходят вместе.
    let raws: Vec<RawFontFile> = files
        .into_iter()
        .map(|path| {
            let stem = path
                .file_stem()
                .and_then(|v| v.to_str())
                .unwrap_or("font")
                .to_string();
            let group = font_group_name_for_path(fonts_dir, &path);
            // An unreadable file still gets a placeholder entry (so the user sees the
            // font they put in the folder), but the reason is logged rather than lost.
            let data = match read_font_file(&path) {
                Ok(data) => Some(data),
                Err(err) => {
                    crate::runtime_log::log_warn(format!(
                        "typing fonts: cannot read font file, listing it without faces or \
                         coverage. Path: {} Error: {err}",
                        path.display()
                    ));
                    None
                }
            };
            let Some(data) = data else {
                return RawFontFile {
                    path,
                    // Original family/name is unknown for an unreadable file: fall back
                    // to the file stem, exactly as for an unparsable one.
                    original_name: stem.clone(),
                    stem,
                    group,
                    content_hash: 0,
                    faces: default_single_face(),
                    coverage: FontLanguageCoverage::default(),
                };
            };
            let original_name = if data.original_name.is_empty() {
                stem.clone()
            } else {
                data.original_name
            };
            RawFontFile {
                path,
                stem,
                group,
                content_hash: data.content_hash,
                faces: data.faces,
                coverage: data.coverage,
                original_name,
            }
        })
        .collect();

    let mut entries = merge_duplicate_fonts(raws);
    assign_font_disambiguators(&mut entries);
    // Resolve the collision-aware render identity for this folder-only list; the
    // combined builder re-runs it once the imported fonts are merged in.
    assign_font_identity_names(&mut entries);
    entries
}

/// Legacy `fonts_data.json` per-font KEY of `path` — the schema-1 keying rule, kept ONLY so
/// a v1 document can still be translated into identities.
///
/// When `path` lives under `fonts_dir` the key is the RELATIVE path with forward-slash
/// separators (so the same font keyed identically on Linux and Windows); otherwise — e.g. an
/// imported system font living elsewhere on disk — the key is the absolute path string.
/// Nothing WRITES this form any more: it exists purely as the left-hand side of the
/// migration map (`run_pending_fonts_data_migration`).
#[must_use]
fn legacy_font_settings_key(fonts_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(fonts_dir) {
        // Normalize separators so a folder font keyed the same across platforms.
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        // Outside the fonts dir: the absolute path was the key verbatim.
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Runs the deferred schema-1 → schema-2 `fonts_data.json` migration against a freshly built
/// font list, if one is still pending.
///
/// The migration cannot happen at startup: re-keying a path-keyed document needs a
/// `path → identity` map, and that map only exists once fonts have been parsed. This is
/// therefore called at the END of a font-list build, where every entry already carries its
/// final identity. It has EXACTLY ONE caller, [`build_combined_font_list`] — the only list
/// that can see imported system fonts too, and therefore the only one whose identities are
/// final. The folder-only subset ([`folder_font_entries`]) must never call it: see that
/// function's contract for the pre-collision identity it used to persist.
///
/// Cheap no-op in the (normal) v2 case: the pending flag is checked before any map is built.
/// The write itself goes through the store's existing off-thread atomic save.
///
/// PLACEHOLDER ENTRIES ARE EXCLUDED. A font file that could not be read or parsed this run is
/// still LISTED (so the user sees what they put in the folder), but its "identity" is the
/// family-or-file-stem fallback — a guess, not the font's PostScript name. Letting such a
/// guess resolve a legacy key would rewrite the key to a name the font does not have and,
/// because the re-key is one-way, strand the setting for good the moment the file becomes
/// readable again. They therefore contribute NOTHING to the resolution, which leaves the keys
/// that pointed at them unresolved — and an unresolved key keeps the migration pending, so it
/// is retried once the file reads.
fn run_pending_fonts_data_migration(entries: &[FontEntry], fonts_dir: &Path) {
    if !font_settings_store::migration_pending() {
        return;
    }
    let mut resolution = font_settings_store::LegacyKeyResolution::default();
    for entry in entries {
        // The synthetic bundled-stack entry stands for a chain of files, not a file a v1
        // document could have referenced.
        if entry.bundled_stack_font().is_some() {
            continue;
        }
        // No valid parsed PostScript name => a placeholder for an unreadable/unparsable file
        // (or a file whose declared name the spec forbids). Its identity is a fallback guess
        // and must not be allowed to claim a legacy key.
        if validated_post_script_name(entry.post_script_name()).is_empty() {
            crate::runtime_log::log_warn(format!(
                "typing: fonts_data migration: '{}' has no readable PostScript name this run, so \
                 it cannot resolve any legacy reference; anything that pointed at it stays \
                 unresolved and the migration is retried later.",
                entry.path.display()
            ));
            continue;
        }
        let identity = entry.render_identity_name();
        // The UNSUFFIXED name is what `system_fonts` records: it names a FILE's face, while
        // the `%hash` contest suffix is a property of one panel list.
        let base_identity = entry.base_identity_name();
        // Both forms are "already migrated" keys, so a second pass does not report the keys
        // an earlier pass converted as lost.
        resolution
            .identities
            .insert(normalize_font_identity(&identity));
        resolution
            .identities
            .insert(normalize_font_identity(&base_identity));
        for path in std::iter::once(&entry.path).chain(entry.alt_paths.iter()) {
            resolution
                .by_key
                .insert(legacy_font_settings_key(fonts_dir, path), identity.clone());
            resolution
                .by_path
                .insert(path.clone(), base_identity.clone());
        }
    }
    font_settings_store::migrate_legacy_font_keys(&resolution);
}

/// Duplicate-merge key: WHICH font a raw file claims to be, and whether it claims
/// anything at all.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum FontMergeKey {
    /// Normalized VALID PostScript name of the representative face plus the file's
    /// content hash — the font's identity plus its bytes, so two files carrying both are
    /// one font under two file names.
    Content(String, u64),
    /// A file that claims NO identity we can trust: no parsed face, a PostScript name the
    /// spec forbids, or no computed content hash (the `0` sentinel — an unreadable file).
    /// Keyed by its POSITION in the input, so such a file never merges with anything.
    ///
    /// The previous key for these was the lowercased file stem, which silently folded two
    /// genuinely different unreadable files that happened to share a stem
    /// (`groups/a/Broken.ttf` and `groups/b/broken.ttf`) into one entry, hiding one of
    /// them from the user entirely. Two files nothing can identify are two files.
    ///
    /// Consequence, accepted deliberately: two BYTE-IDENTICAL unidentifiable files whose
    /// fallback identity also coincides (same family name or same stem) stay two entries
    /// claiming one identity. They are not contested — their bytes agree — so neither is
    /// suffixed, and `TabFontProvider` resolves the name to the first of them (logged).
    /// That is harmless precisely because the bytes are identical: whichever one the
    /// renderer picks produces the same pixels, and the user still sees BOTH files in the
    /// list, which is what the merge was hiding.
    Unmergeable(usize),
}

/// Объединяет копии одного шрифта (совпадает PostScript-имя и содержимое — «тот же
/// хэш») в один пункт со списком групп; разные по содержимому остаются раздельно.
///
/// The merge key is `(valid PostScript name, content hash)`: byte-identical copies of one
/// font merge even when their FILE NAMES differ (six such pairs ship under `fonts/`),
/// with the path-sorted first copy as the representative, the rest in `alt_paths`, and
/// the union of their folder groups. Files that share a name but not their bytes stay
/// separate entries and are given distinct suffixed identities later, by
/// `assign_font_identity_names`. A file with no valid PostScript name or no computed
/// content hash claims no identity and NEVER merges (see `FontMergeKey::Unmergeable`).
pub(super) fn merge_duplicate_fonts(raws: Vec<RawFontFile>) -> Vec<FontEntry> {
    // Кластеризация по (валидное PostScript-имя, хэш содержимого), с сохранением порядка
    // первого появления; файлы без опознаваемой идентичности не кластеризуются вовсе.
    let mut order: Vec<FontMergeKey> = Vec::new();
    let mut clusters: HashMap<FontMergeKey, Vec<RawFontFile>> = HashMap::new();
    for (index, raw) in raws.into_iter().enumerate() {
        let post_script = raw
            .faces
            .first()
            .map(|face| validated_post_script_name(&face.post_script_name))
            .unwrap_or_default();
        let key = if post_script.is_empty() || raw.content_hash == 0 {
            FontMergeKey::Unmergeable(index)
        } else {
            FontMergeKey::Content(normalize_font_identity(post_script), raw.content_hash)
        };
        if !clusters.contains_key(&key) {
            order.push(key.clone());
        }
        clusters.entry(key).or_default().push(raw);
    }

    let mut entries = Vec::with_capacity(order.len());
    for key in order {
        let mut cluster = clusters.remove(&key).unwrap_or_default();
        // Представитель — первый по пути (детерминированно).
        cluster.sort_by(|a, b| a.path.cmp(&b.path));
        // `order` only ever records keys that were just inserted into `clusters`, so a
        // cluster is never empty here — but this loop must not be able to PANIC on an
        // index, so the emptiness is handled instead of assumed.
        let Some((rep, folded)) = cluster.split_first() else {
            continue;
        };
        let label = rep.stem.clone();
        let faces = rep.faces.clone();
        let path = rep.path.clone();
        let coverage = rep.coverage.clone();
        let original_name = rep.original_name.clone();
        // Representative face = the first one, so the entry's PostScript name is that
        // face's — empty for a file that could not be parsed, and empty as well for a
        // name the spec forbids, which counts as absent everywhere (the loaders already
        // validate; re-validating here keeps the entry-level contract true whatever built
        // the raw file).
        let post_script_name = rep.faces.first().map_or_else(String::new, |face| {
            validated_post_script_name(&face.post_script_name).to_string()
        });
        let content_hash = rep.content_hash;
        let identity_name = base_font_identity_name(&post_script_name, &original_name, &label);
        let alt_paths: Vec<PathBuf> = folded.iter().map(|raw| raw.path.clone()).collect();
        // Объединение групп копий (без повторов, в стабильном порядке).
        let mut groups: Vec<Option<String>> = Vec::new();
        for raw in std::iter::once(rep).chain(folded.iter()) {
            if !groups.contains(&raw.group) {
                groups.push(raw.group.clone());
            }
        }
        entries.push(FontEntry {
            kind: FontEntryKind::File,
            label,
            path,
            alt_paths,
            groups,
            disambig: None,
            faces,
            coverage,
            original_name,
            post_script_name,
            content_hash,
            // Filled by `apply_display_name_overrides` at the end of
            // `build_combined_font_list`, once every entry carries its final identity
            // (the override key).
            display_name: None,
            identity_name,
            // Filled by `apply_virtual_groups` after the finalized list is built.
            virtual_group_aliases: BTreeMap::new(),
        });
    }
    entries
}

/// Folds byte-identical copies of ONE font that reached the COMBINED panel list from
/// different sources — the fonts folder and the user's imported system fonts — into a
/// single entry, in place.
///
/// The key is the one `merge_duplicate_fonts` uses on raw folder files (valid PostScript
/// name + content hash), applied one level up because the folder pass cannot see the
/// imported paths. Without this pass `fonts/Acme.ttf` and an imported
/// `/usr/share/fonts/Acme-copy.ttf` with the same bytes and the same PostScript name
/// remain two entries claiming ONE identity: they are not contested (their hashes agree,
/// so neither is suffixed), and `TabFontProvider` — first-wins on the identity key —
/// resolves to one and hides the other.
///
/// Merge rules:
/// - The FIRST entry of a cluster in list order stays as the representative. `load_fonts`
///   appends imported fonts after the folder fonts, so a folder font always wins and
///   keeps its label, its folder groups and its disambiguator; the `"{stem} [system]"`
///   label of the folded imported copy disappears, which is correct — the font IS in the
///   fonts folder, and showing it twice under two names is the defect being fixed.
/// - The folded entries contribute their file paths (representative path + `alt_paths`)
///   to the representative's `alt_paths`, so a legacy reference to any copy still
///   resolves, and their display-name override when the representative has none.
/// - `groups` are deliberately NOT unioned. The folder list is already merged by the same
///   key, so a cluster holds at most one folder entry and there is nothing to combine;
///   an imported file lives outside the fonts dir and its `groups = [None]` is a
///   placeholder, not membership of the fonts-dir root — unioning it would list a
///   grouped folder font in the root group as well and invalidate its disambiguator.
/// - An entry with no valid PostScript name or no computed content hash (`0`) claims no
///   identity and never merges, mirroring `FontMergeKey::Unmergeable`. The synthetic
///   bundled entry is skipped for the same reason (it stands for a chain of files).
///
/// Must run BEFORE `assign_font_identity_names`, so identity assignment sees one entry
/// per font and cannot mistake a duplicate for a claimant.
pub(super) fn merge_duplicate_font_entries(entries: &mut Vec<FontEntry>) {
    // (identity, content hash) -> index of the entry that represents that font.
    let mut representative_of: HashMap<(String, u64), usize> = HashMap::new();
    // Parallel to `entries`: whether the entry was folded into a representative.
    let mut folded = vec![false; entries.len()];

    for index in 0..entries.len() {
        let (key, paths, display_name) = {
            let entry = &entries[index];
            if entry.bundled_stack_font().is_some() {
                continue;
            }
            let post_script = validated_post_script_name(entry.post_script_name());
            if post_script.is_empty() || entry.content_hash == 0 {
                continue;
            }
            let key = (
                normalize_font_identity(post_script),
                entry.content_hash,
            );
            let paths: Vec<PathBuf> = std::iter::once(entry.path.clone())
                .chain(entry.alt_paths.iter().cloned())
                .collect();
            (key, paths, entry.display_name.clone())
        };
        let Some(&rep_index) = representative_of.get(&key) else {
            representative_of.insert(key, index);
            continue;
        };
        let duplicate_path = entries[index].path.clone();
        let representative = &mut entries[rep_index];
        crate::runtime_log::log_info(format!(
            "typing fonts: '{}' and '{}' are byte-identical copies of the font '{}'; they \
             are listed as ONE entry so a single identity cannot name two list items.",
            representative.path.display(),
            duplicate_path.display(),
            representative.post_script_name(),
        ));
        for path in paths {
            if representative.path != path && !representative.alt_paths.contains(&path) {
                representative.alt_paths.push(path);
            }
        }
        // Overrides are keyed by IDENTITY and the two entries share one, so the folded copy
        // can only carry the same value. Inheriting it is still done rather than dropped:
        // this function is also reachable before `apply_display_name_overrides` has run on
        // the combined list, and an entry with a display name must not lose it there.
        if representative.display_name.is_none() {
            representative.display_name = display_name.filter(|name| !name.trim().is_empty());
        }
        folded[index] = true;
    }

    // `Vec::retain` visits elements in order, so the counter walks `folded` in step.
    let mut index = 0;
    entries.retain(|_| {
        let keep = !folded[index];
        index += 1;
        keep
    });
}

/// Проставляет скобочное уточнение (по группам) тем пунктам, у которых базовое
/// имя совпадает с другим пунктом.
pub(super) fn assign_font_disambiguators(entries: &mut [FontEntry]) {
    let mut label_counts: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter() {
        *label_counts.entry(entry.label.to_lowercase()).or_insert(0) += 1;
    }
    // Уникальное имя — уточнение не нужно.
    let mut used: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter_mut() {
        if label_counts.get(&entry.label.to_lowercase()).copied().unwrap_or(0) <= 1 {
            entry.disambig = None;
            continue;
        }
        let mut suffix = font_groups_label(&entry.groups);
        // Если уточнения совпали (например, два корневых) — добавим индекс.
        let key = format!("{}\u{0}{}", entry.label.to_lowercase(), suffix.to_lowercase());
        let n = used.entry(key).or_insert(0);
        *n += 1;
        if *n > 1 {
            suffix = format!("{suffix} {n}");
        }
        entry.disambig = Some(suffix);
    }
}

/// Отображаемое имя группы для уточнения: имя группы или «корень».
pub(super) fn font_groups_label(groups: &[Option<String>]) -> String {
    let parts: Vec<&str> = groups
        .iter()
        .map(|group| group.as_deref().unwrap_or(t!("typing.fonts.root_group_label")))
        .collect();
    if parts.is_empty() {
        t!("typing.fonts.root_group_label").to_string()
    } else {
        parts.join(", ")
    }
}

/// Stable 64-bit content hash of a font FILE's raw bytes: the first 8 bytes of their
/// SHA-256 digest, read big-endian.
///
/// The algorithm is part of a PERSISTED contract and must never change: the hash is what
/// a collision suffix (`suffixed_font_identity_name`) is spelled from, and that suffix is
/// written into projects, presets and `fonts_data.json`. That rules out
/// `std::collections::hash_map::DefaultHasher`, whose algorithm the standard library
/// explicitly does NOT guarantee across releases — a toolchain upgrade would re-suffix
/// every contested font and orphan everything stored under the old identity.
///
/// `0` is reserved as the "not computed" sentinel (`FontEntry.content_hash`); a real file
/// whose digest starts with eight zero bytes is cryptographically infeasible, and the
/// only consequence would be that this one file never merges with a copy of itself.
#[must_use]
pub(super) fn font_content_hash(bytes: &[u8]) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    // SHA-256 is 32 bytes, so the first 8 always exist; `copy_from_slice` cannot panic
    // on a fixed 8-byte prefix of it.
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

/// Placeholder face list for an entry with no parsed faces (an unreadable/unparsable
/// file, or the synthetic bundled-stack entry). Its PostScript name is EMPTY on
/// purpose: there is no face to read one from, and inventing one would fabricate an
/// identity.
#[must_use]
pub(super) fn default_single_face() -> Vec<FontFaceEntry> {
    vec![FontFaceEntry {
        label: "Face 0".to_string(),
        face_index: 0,
        post_script_name: String::new(),
    }]
}

/// Lists the real FOLDER-group names: the immediate subdirectory names of
/// `fonts_dir/groups/`, sorted case-insensitively. Performs one `read_dir`; returns an empty
/// list when the `groups/` directory is absent or unreadable. Widened to
/// `pub(in crate::tabs::typing)` so the `font_admin` facade can expose folder-group names
/// alongside virtual groups.
pub(in crate::tabs::typing) fn load_font_groups(fonts_dir: &Path) -> Vec<String> {
    let groups_dir = fonts_dir.join("groups");
    let Ok(read_dir) = fs::read_dir(groups_dir) else {
        return Vec::new();
    };

    let mut groups = read_dir
        .filter_map(|entry_result| {
            let entry = entry_result.ok()?;
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            path.file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    groups.sort_by_key(|group_name| group_name.to_lowercase());
    groups
}

/// Injects the user-defined VIRTUAL font groups into an already-finalized panel font
/// list and returns the merged combobox group list (real FOLDER groups + surviving
/// virtual group names, case-insensitively sorted, stable on ties so the folder groups
/// keep priority within a tie).
///
/// For each virtual group whose name does NOT collide case-insensitively with a real
/// folder-group name, every member is matched against a loaded font by IDENTITY
/// (`FontEntry::render_identity_name`, compared case-insensitively through
/// `normalize_font_identity`). A match records the virtual group in the font's `groups` (so
/// the existing `font_in_group` / `filtered_font_indices` membership machinery treats it
/// exactly like a folder group) and, when the member carries an alias, stores it in the
/// font's `virtual_group_aliases` for group-aware display. A member identity with no loaded
/// font is silently skipped — the config is preserved elsewhere; the font simply does not
/// appear (a virtual group may legitimately have zero loaded members).
///
/// Matching by identity is what makes membership survive MOVING OR RENAMING a font file:
/// the identity comes from the face's PostScript name, not from where the bytes live. A
/// cluster of byte-identical copies merged into one entry has exactly one identity, so the
/// per-`alt_path` scan the path-keyed era needed is gone.
///
/// ALIAS MERGE RULE: two distinct member entries in the SAME virtual group can still resolve
/// to one font (a stale legacy key kept beside the identity it was migrated to). Aliases are
/// inserted in member order and only when `Some`, so the LAST `Some` alias wins and a later
/// `None` member does NOT clear an alias an earlier member already set.
///
/// A virtual group whose name collides case-insensitively with a real folder group is
/// SKIPPED ENTIRELY with a warning (defensive; the settings UI also validates this), so
/// the folder group's real membership is never diluted by virtual references.
///
/// ORDERING CONTRACT: this MUST run AFTER `merge_duplicate_fonts`,
/// `assign_font_disambiguators`, and `assign_font_identity_names`, because (a) the
/// disambiguators «(корень)/(group)» must keep reflecting only the REAL folder locations
/// (virtual membership must not change the "All groups" disambiguation view) and (b) the
/// collision-aware render identity must already be assigned — it is the MATCH KEY, and
/// virtual membership never touches it. Adding a virtual membership only APPENDS to
/// `groups`, so the folder-derived disambiguator (computed earlier from the folder-only
/// `groups`) is left intact.
#[must_use]
pub(in crate::tabs::typing) fn apply_virtual_groups(
    fonts: &mut [FontEntry],
    real_groups: &[String],
    virtual_groups: &[fonts_data::VirtualFontGroup],
) -> Vec<String> {
    // Real folder-group names lowercased, for case-insensitive collision detection.
    // Unicode-lowercased (not ASCII) so Cyrillic group names fold correctly.
    let real_lower: HashSet<String> = real_groups.iter().map(|name| name.to_lowercase()).collect();

    // Precompute each entry's normalized identity once, so the membership test below is a
    // string compare instead of re-normalizing per (member × entry) pair.
    let entry_identities: Vec<String> = fonts
        .iter()
        .map(|entry| normalize_font_identity(&entry.render_identity_name()))
        .collect();

    let mut surviving: Vec<String> = Vec::new();
    for group in virtual_groups {
        // Defensive: the settings UI forbids a virtual name colliding with a real folder
        // group, but validate here too so a stale/edited config can never dilute a real
        // group's membership.
        if real_lower.contains(&group.name.to_lowercase()) {
            crate::runtime_log::log_warn(format!(
                "typing fonts: virtual font group '{}' collides with a real folder group of the \
                 same name (case-insensitive); skipping the virtual group.",
                group.name
            ));
            continue;
        }
        surviving.push(group.name.clone());

        for member in &group.members {
            let wanted = normalize_font_identity(&member.font);
            for (entry, identity) in fonts.iter_mut().zip(entry_identities.iter()) {
                // An identity names exactly one list entry (that is the whole point of the
                // collision suffix), so at most one entry matches; scanning all is harmless
                // and keeps the match total.
                if identity != &wanted {
                    continue;
                }
                // Dedup: a font may already carry this virtual group from an earlier member
                // pass (e.g. two member keys resolving to the same merged entry).
                let group_key = Some(group.name.clone());
                if !entry.groups.contains(&group_key) {
                    entry.groups.push(group_key);
                }
                if let Some(alias) = &member.alias {
                    entry
                        .virtual_group_aliases
                        .insert(group.name.clone(), alias.clone());
                }
            }
        }
    }

    // Merged list: real groups + surviving virtual names, case-insensitively sorted.
    // `sort_by` is STABLE, so a case-insensitive tie keeps insertion order (real groups
    // were pushed first), matching the "ties: stable" contract.
    let mut merged: Vec<String> = real_groups.to_vec();
    merged.extend(surviving);
    merged.sort_by_key(|name| name.to_lowercase());
    merged
}

// ===========================================================================
// Locating an imported system font BY NAME
// (`dev-docs/font_identity_postscript_plan.md`, phase 6)
// ===========================================================================

/// One installed system face as the name index needs it: the spec-valid PostScript name it
/// declares and the FILE it lives in.
///
/// A face whose declared name is NOT spec-valid ([`is_valid_post_script_name`]) never becomes
/// a record. An invalid name counts as absent everywhere else in the identity contract, so it
/// must not be locatable either — otherwise a font could be found under a name it can never
/// be stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SystemFaceRecord {
    /// Validated PostScript name (`name` id 6) of the face.
    pub(super) post_script_name: String,
    /// File the face was enumerated from.
    pub(super) path: PathBuf,
}

/// Process-global `PostScript name → installed FILE(s)` index.
///
/// `fontdb` exposes no PostScript-name query (only `Family::Name`), so this is our own linear
/// pass over every installed face — the same pass the settings import picker already runs.
/// ONE NAME CAN MAP TO SEVERAL FILES (a family commonly ships a variable and a static cut
/// under one PostScript name, and a font can be installed twice by two packages), so the
/// value is a candidate LIST, never a single path; which candidate wins is decided by
/// [`locate_system_font_by_identity`], which reads them.
pub(super) struct SystemFontNameIndex {
    /// Normalized identity → candidate files, deduplicated and sorted BY PATH.
    ///
    /// Sorted by path rather than kept in enumeration order: `fontdb`'s order follows
    /// directory iteration, which is filesystem-dependent and not reproducible across runs,
    /// while a path sort is.
    by_identity: HashMap<String, Vec<PathBuf>>,
}

impl SystemFontNameIndex {
    /// Builds the index from an enumeration of installed faces. Records with an empty
    /// (i.e. invalid or absent) name are dropped; candidates are deduplicated and sorted.
    fn from_faces(faces: Vec<SystemFaceRecord>) -> Self {
        let mut by_identity: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for face in faces {
            let key = normalize_font_identity(&face.post_script_name);
            if key.is_empty() {
                continue;
            }
            by_identity.entry(key).or_default().push(face.path);
        }
        // A `.ttc` contributes one record per face, so the same file appears repeatedly under
        // one name; dedup before the candidates are ever read.
        for paths in by_identity.values_mut() {
            paths.sort();
            paths.dedup();
        }
        Self { by_identity }
    }

    /// Files that declare `identity` (compared case-insensitively), in the index's
    /// deterministic path order. Empty when no installed face claims that name.
    fn candidates(&self, identity: &str) -> &[PathBuf] {
        self.by_identity
            .get(&normalize_font_identity(identity))
            .map_or(&[][..], Vec::as_slice)
    }

    /// Number of distinct PostScript names in the index. Test-only observer.
    #[cfg(test)]
    pub(super) fn name_count(&self) -> usize {
        self.by_identity.len()
    }
}

/// The process-wide cache of the system-font name index. `None` means "not built yet"; the
/// next lookup builds it.
fn system_font_index_cache() -> &'static RwLock<Option<Arc<SystemFontNameIndex>>> {
    static CACHE: OnceLock<RwLock<Option<Arc<SystemFontNameIndex>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(None))
}

/// Serializes index BUILDS, so several worker threads reaching a cold cache at once scan the
/// system font database ONCE instead of once per thread.
fn system_font_index_build_lock() -> &'static Mutex<()> {
    static BUILD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    BUILD_LOCK.get_or_init(|| Mutex::new(()))
}

/// How many times the index has been built in this process. Test-only: it is what proves
/// that a document whose path hints all resolve never scans the system.
#[cfg(test)]
static SYSTEM_FONT_INDEX_BUILDS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The cached index, or `None` when it has not been built yet.
fn cached_system_font_name_index() -> Option<Arc<SystemFontNameIndex>> {
    let guard = match system_font_index_cache().read() {
        Ok(guard) => guard,
        // A poisoned lock still holds a valid index; recover it rather than panicking.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clone()
}

/// Publishes `index` as the process-wide one, replacing whatever was cached.
fn publish_system_font_name_index(index: Arc<SystemFontNameIndex>) {
    let mut guard = match system_font_index_cache().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(index);
}

/// Returns the process-global system-font name index, building it on first use.
///
/// HEAVY ON A MISS: the build enumerates every font installed on the machine
/// (`fontdb::Database::load_system_fonts` — thousands of files on a desktop), so callers MUST
/// be off the GUI thread. Both callers are: the panel's font-reload worker and the settings
/// pane's off-thread list load.
///
/// The result is cached for the whole process. It is REBUILT only when a fresh enumeration is
/// published — which is what [`load_system_fonts`] does at the end of the import picker's
/// catalog load, so opening the picker doubles as the explicit refresh after the user has
/// installed or removed a font.
///
/// Concurrent first callers do not each scan: the build is serialized and the cache is
/// re-checked inside the lock.
pub(super) fn system_font_name_index() -> Arc<SystemFontNameIndex> {
    if let Some(index) = cached_system_font_name_index() {
        return index;
    }
    // RAII: held for the whole build so a second thread waits and then finds the cache warm
    // instead of running a duplicate scan.
    let _build_guard = match system_font_index_build_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(index) = cached_system_font_name_index() {
        return index;
    }
    let started = std::time::Instant::now();
    let index = Arc::new(SystemFontNameIndex::from_faces(enumerate_system_font_faces()));
    #[cfg(test)]
    SYSTEM_FONT_INDEX_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::runtime_log::log_info(format!(
        "typing fonts: built the system-font name index ({} distinct PostScript names) in {} ms.",
        index.by_identity.len(),
        started.elapsed().as_millis()
    ));
    publish_system_font_name_index(Arc::clone(&index));
    index
}

/// Enumerates the PostScript name and file of every face installed on the machine.
#[cfg(not(test))]
fn enumerate_system_font_faces() -> Vec<SystemFaceRecord> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    db.faces()
        .filter_map(|face| {
            // Only file-backed faces can be re-read later; a binary source has no path to
            // record as a hint.
            let fontdb::Source::File(path) = &face.source else {
                return None;
            };
            let name = validated_post_script_name(&face.post_script_name);
            if name.is_empty() {
                return None;
            }
            Some(SystemFaceRecord {
                post_script_name: name.to_string(),
                path: path.clone(),
            })
        })
        .collect()
}

/// Test build of the enumerator: the machine's real font set is NEVER touched.
///
/// A unit test must be reproducible on any machine, and scanning thousands of installed files
/// would also make every test that exercises an unresolvable import slow. Tests install their
/// own face list through [`test_install_system_faces`]; with none installed, the machine has
/// no fonts.
#[cfg(test)]
fn enumerate_system_font_faces() -> Vec<SystemFaceRecord> {
    let guard = match test_system_faces_cell().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clone()
}

/// The face list a test has installed as the enumeration source. Test-only.
#[cfg(test)]
fn test_system_faces_cell() -> &'static RwLock<Vec<SystemFaceRecord>> {
    static FACES: OnceLock<RwLock<Vec<SystemFaceRecord>>> = OnceLock::new();
    FACES.get_or_init(|| RwLock::new(Vec::new()))
}

/// Installs the face list the index builder will see and DROPS the cached index, so the next
/// lookup rebuilds from it. Callers must hold `font_settings_store::test_lock()` — the index
/// is process-global. Test-only.
#[cfg(test)]
pub(super) fn test_install_system_faces(faces: Vec<SystemFaceRecord>) {
    {
        let mut guard = match test_system_faces_cell().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = faces;
    }
    let mut cache = match system_font_index_cache().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *cache = None;
}

/// Clears the installed face list, the cached index and the build counter, so a test starts
/// from a cold, empty system. Callers must hold `font_settings_store::test_lock()`.
/// Test-only.
#[cfg(test)]
pub(super) fn test_reset_system_font_index() {
    test_install_system_faces(Vec::new());
    SYSTEM_FONT_INDEX_BUILDS.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// How many times the index was built since the last [`test_reset_system_font_index`].
/// Test-only.
#[cfg(test)]
pub(super) fn test_system_font_index_builds() -> u64 {
    SYSTEM_FONT_INDEX_BUILDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// A font file that was read, parsed and — when a name was expected — confirmed to still
/// claim it.
struct LoadedSystemFontFile {
    /// File the bytes came from.
    path: PathBuf,
    /// Everything the single read + single parse produced.
    data: FontFileData,
    /// The UNSUFFIXED identity the file claims (its PostScript name, with the documented
    /// family-or-label fallback).
    identity_name: String,
    /// Representative family name, with the file stem substituted when the file carries none
    /// — exactly what `FontEntry.original_name` needs.
    original_name: String,
}

/// Reads `path` and confirms it still holds the font named `expected_identity`.
///
/// `expected_identity` may be EMPTY, meaning "no name recorded yet" (an unmigrated v1 entry):
/// then any parsable file is accepted and the name it claims is what the caller learns.
///
/// # Errors
/// [`ImportedFontUnavailable::Unreadable`] when the file cannot be read,
/// [`ImportedFontUnavailable::Unparsable`] when no parser accepts it, and
/// [`ImportedFontUnavailable::NameMismatch`] when the file holds a DIFFERENT font — it was
/// replaced, and silently substituting it is exactly what a name-keyed identity forbids.
fn load_system_font_file_as(
    path: &Path,
    expected_identity: &str,
) -> Result<LoadedSystemFontFile, ImportedFontUnavailable> {
    // ONE read + ONE parse per file; the same result carries the faces, the family name and
    // the coverage that used to cost two more throwaway databases.
    let data = read_font_file(path)
        .map_err(|err| ImportedFontUnavailable::Unreadable(err.to_string()))?;
    // Reject a corrupt/unsupported file up front so it never becomes a fake single-face
    // entry. fontdb yields no ids for a file it cannot parse.
    if !data.parsed {
        return Err(ImportedFontUnavailable::Unparsable);
    }
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("system font");
    let original_name = if data.original_name.is_empty() {
        stem.to_string()
    } else {
        data.original_name.clone()
    };
    // The UNNUMBERED base label is the identity's last-resort fallback: a duplicate-label
    // ` (N)` suffix is a property of one list, and an identity must not depend on a position
    // in it. The finalized list recomputes identities anyway (`assign_font_identity_names`).
    let identity_name = base_font_identity_name(
        data.post_script_name(),
        &original_name,
        &imported_system_font_base_label(path),
    );
    if !expected_identity.trim().is_empty()
        && normalize_font_identity(expected_identity) != normalize_font_identity(&identity_name)
    {
        return Err(ImportedFontUnavailable::NameMismatch {
            found: identity_name,
        });
    }
    Ok(LoadedSystemFontFile {
        path: path.to_path_buf(),
        data,
        identity_name,
        original_name,
    })
}

/// Locates the installed font named `identity` (its PostScript name) among the system fonts.
///
/// This is STEP 2 of resolving a stored imported system font: it runs only when the recorded
/// `last_path` hint failed (missing file, unparsable file, or a file that now holds a
/// different font), which is what keeps a document whose hints are all valid from ever
/// scanning the system. Returns `None` for a blank identity, when nothing installed claims
/// the name, and when every candidate turned out not to claim it after all.
///
/// COLLISION RULE. Several installed files can declare one PostScript name (the variable and
/// the static cut of a family; a font packaged twice). Every candidate is READ and confirmed
/// to still claim the name, and the winner is the one with the LOWEST content hash, ties
/// broken by the lexicographically first path. Two reasons:
/// - it is the SAME rule the identity contract already uses for a contested name (the bare
///   name resolves to the lowest-hash claimant, `assign_font_identity_names`), so the file
///   this returns is the file that bare name means everywhere else in the app;
/// - it is a function of the candidates' BYTES, not of enumeration order or of which package
///   was installed first, so the same file is picked on every run and on every machine
///   holding those files.
fn locate_system_font_by_identity(identity: &str) -> Option<LoadedSystemFontFile> {
    let identity = identity.trim();
    if identity.is_empty() {
        return None;
    }
    let index = system_font_name_index();
    let candidates = index.candidates(identity);
    if candidates.is_empty() {
        return None;
    }
    let mut best: Option<LoadedSystemFontFile> = None;
    for path in candidates {
        match load_system_font_file_as(path, identity) {
            Ok(loaded) => {
                let better = best.as_ref().is_none_or(|current| {
                    (loaded.data.content_hash, loaded.path.as_path())
                        < (current.data.content_hash, current.path.as_path())
                });
                if better {
                    best = Some(loaded);
                }
            }
            Err(reason) => {
                // The index is a snapshot: a file can be gone or replaced since it was built.
                crate::runtime_log::log_warn(format!(
                    "typing fonts: while locating the system font '{identity}' by name, the \
                     candidate file {} could not be used ({reason:?}); trying the remaining \
                     candidates.",
                    path.display()
                ));
            }
        }
    }
    let best = best?;
    if candidates.len() > 1 {
        crate::runtime_log::log_info(format!(
            "typing fonts: {} installed files declare the PostScript name '{identity}'; the \
             lowest-content-hash one was chosen. Chosen: {} (hash {:016x})",
            candidates.len(),
            best.path.display(),
            best.data.content_hash
        ));
    }
    Some(best)
}

/// Locates the INSTALLED font whose PostScript name is `post_script_name` and reports the
/// identity the winning file actually claims together with that file's path.
///
/// The typing-wide entry point to the by-name lookup (the facade `font_admin` wraps this one).
/// It returns only what a caller outside this module can key on — the CONFIRMED identity, in
/// the casing the file itself declares, and the byte-source path — because the parsed font
/// data [`locate_system_font_by_identity`] carries is private to `panel`.
///
/// BLOCKING and potentially VERY HEAVY: a cold lookup builds the process-wide system-font name
/// index (a scan of the whole OS font database) and every candidate file is read and parsed.
/// Callers MUST be off the GUI thread.
///
/// Returns `None` for a blank name, when nothing installed declares it, and when no candidate
/// turned out to claim it after all. The selection rule among several claimants (lowest content
/// hash, ties broken by the lexicographically first path) is documented on
/// [`locate_system_font_by_identity`].
#[must_use]
pub(in crate::tabs::typing) fn locate_system_font_file_by_identity(
    post_script_name: &str,
) -> Option<(String, PathBuf)> {
    locate_system_font_by_identity(post_script_name)
        .map(|located| (located.identity_name, located.path))
}

/// Enumerates ALL OS-installed fonts (one `FontEntry` per file) for the settings
/// font-import picker, which lets the user pick individual system fonts to import.
/// HEAVY (hundreds of faces via `fontdb::load_system_fonts`): callers must run it off
/// the GUI thread. Regular font loading is config-driven (folder + imported paths); this
/// bulk enumerator is only the picker's catalog source.
///
/// SIDE EFFECT: the catalog it produced is published as the process-wide system-font name
/// index (`system_font_name_index`), so the picker's load doubles as the explicit refresh of
/// that index and locating a moved imported font afterwards costs no extra scan.
pub(in crate::tabs::typing) fn load_system_fonts() -> Vec<FontEntry> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut by_path = HashMap::<PathBuf, Vec<FontFaceEntry>>::new();
    // Track the fontdb id per (path, face_index) so the representative face's
    // coverage can be read back via `db.with_face_data` (memory-mapped) below.
    let mut ids_by_path: HashMap<PathBuf, Vec<(usize, fontdb::ID)>> = HashMap::new();
    // Original family name per (path, face_index), used to pick the representative
    // face's real name for `FontEntry.original_name`.
    let mut families_by_path: HashMap<PathBuf, Vec<(usize, String)>> = HashMap::new();
    for face in db.faces() {
        let path = match &face.source {
            fontdb::Source::File(path) => path.clone(),
            _ => continue,
        };
        let family = face
            .families
            .first()
            .map(|(name, _)| name.as_str())
            .unwrap_or("Unknown");
        let style = match face.style {
            fontdb::Style::Normal => "Normal",
            fontdb::Style::Italic => "Italic",
            fontdb::Style::Oblique => "Oblique",
        };
        let face_index = face.index as usize;
        ids_by_path
            .entry(path.clone())
            .or_default()
            .push((face_index, face.id));
        families_by_path
            .entry(path.clone())
            .or_default()
            .push((face_index, family.to_string()));
        by_path.entry(path).or_default().push(FontFaceEntry {
            label: format!(
                "#{face_index} {family} | {style} | w{} | {}",
                face.weight.0, face.post_script_name
            ),
            face_index,
            // Same rule as `font_file_data_from_bytes`: a name the PostScript spec
            // forbids counts as absent, so it can never become an identity. Rejections
            // are NOT logged here — this pass walks every face installed on the machine
            // (thousands), and one warning per malformed system font would drown the log;
            // importing such a font goes through `read_font_file`, which does warn.
            post_script_name: validated_post_script_name(&face.post_script_name).to_string(),
        });
    }

    let mut files: Vec<PathBuf> = by_path.keys().cloned().collect();
    files.sort_by_key(|path| path.to_string_lossy().to_lowercase());

    let mut used_labels = HashMap::<String, usize>::new();
    let mut entries = Vec::<FontEntry>::with_capacity(files.len());
    for path in files {
        let mut faces = by_path.remove(&path).unwrap_or_default();
        faces.sort_by_key(|face| face.face_index);
        if faces.is_empty() {
            faces.extend(default_single_face());
        }

        let stem = path
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("system font");
        let base_label = format!("{stem} [system]");
        let count = used_labels.entry(base_label.clone()).or_insert(0);
        *count += 1;
        let label = if *count > 1 {
            format!("{base_label} ({count})")
        } else {
            base_label
        };
        let rep_face_index = faces.first().map(|face| face.face_index).unwrap_or(0);
        let coverage = ids_by_path
            .get(&path)
            .and_then(|ids| {
                ids.iter()
                    .find(|(idx, _)| *idx == rep_face_index)
                    .map(|(_, id)| *id)
            })
            .and_then(|id| {
                db.with_face_data(id, |data, index| {
                    super::font_coverage::classify_font_bytes(data, index as usize)
                })
            })
            .unwrap_or_default();
        // Representative face's real family name; fall back to the file stem.
        let original_name = families_by_path
            .get(&path)
            .and_then(|fams| {
                fams.iter()
                    .find(|(idx, _)| *idx == rep_face_index)
                    .map(|(_, name)| name.clone())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| stem.to_string());
        // Representative face's PostScript name, read straight from the system
        // database's face record (the faces are sorted by index, so `first` is it).
        let post_script_name = faces
            .first()
            .map_or_else(String::new, |face| face.post_script_name.clone());
        let identity_name = base_font_identity_name(&post_script_name, &original_name, &label);
        // Overrides are keyed by identity; this catalog never runs the collision pass, so
        // an entry's identity here is always its unsuffixed base form.
        let display_name = font_settings_store::font_display_name_override(&identity_name);
        entries.push(FontEntry {
            kind: FontEntryKind::File,
            label,
            path,
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces,
            coverage,
            original_name,
            post_script_name,
            // This catalog enumerates system faces through `fontdb` WITHOUT reading whole
            // files, so no content hash exists here yet. It is a picker catalog, never a
            // panel list, so it never reaches `assign_font_identity_names` and needs a hash
            // only where two files claim ONE identity — filled in right below.
            content_hash: 0,
            display_name,
            identity_name,
            virtual_group_aliases: BTreeMap::new(),
        });
    }

    resolve_contested_catalog_content_hashes(&mut entries);

    // This pass has just enumerated every installed face; publishing it as the process-wide
    // name index is the "explicit refresh" of that index (the picker is opened precisely when
    // the user has installed or removed fonts) and makes locating a moved imported font free
    // afterwards.
    publish_system_font_name_index(Arc::new(SystemFontNameIndex::from_faces(
        system_face_records_from_entries(&entries),
    )));
    entries
}

/// Fills in a REAL content hash for every catalog entry whose identity is claimed by more
/// than one FILE, leaving every uncontested entry on the `0` "content unknown" sentinel.
///
/// WHY IT IS NEEDED. The picker catalog is enumerated through `fontdb` without reading
/// whole files, so every entry starts at `content_hash == 0`. The own-typeface preview
/// registers an egui family named after `(identity, content hash, face index)`
/// (`widgets::font_preview`), so two DIFFERENT installed files declaring one PostScript
/// name — variable vs. static cuts of the same family are the usual case — collapsed onto
/// ONE registration, and the second row was drawn in the first row's typeface.
///
/// WHY ONLY THE CONTESTED ONES. Hashing every row means reading every installed font file
/// (2153 files, hundreds of MB, on the maintainer's machine); hashing only the contested
/// ones costs one read per file of the handful of names two files claim (6 names / 12
/// files there). An uncontested identity needs no discriminant: it already names exactly
/// one row of this catalog.
///
/// A file that cannot be read keeps `0` and is logged; it then shares the sentinel family
/// with any other unreadable claimant of the same name, which is the pre-existing
/// behavior for content that is unknown.
///
/// Runs as part of [`load_system_fonts`], i.e. OFF the GUI thread.
pub(super) fn resolve_contested_catalog_content_hashes(entries: &mut [FontEntry]) {
    let mut claimants_by_identity: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        let identity = normalize_font_identity(&entry.identity_name);
        if identity.is_empty() {
            continue;
        }
        claimants_by_identity.entry(identity).or_default().push(idx);
    }
    for (identity, claimants) in claimants_by_identity {
        if claimants.len() < 2 {
            continue;
        }
        for idx in claimants {
            let Some(entry) = entries.get_mut(idx) else {
                continue;
            };
            match std::fs::read(&entry.path) {
                Ok(bytes) => entry.content_hash = font_content_hash(&bytes),
                Err(error) => crate::runtime_log::log_warn(format!(
                    "typing fonts: cannot read a system font file to tell two files claiming \
                     one name apart; the import picker may preview both rows in the same \
                     typeface. Identity: '{identity}' Path: '{}' Error: {error}",
                    entry.path.display()
                )),
            }
        }
    }
}

/// Flattens an enumerated system-font catalog into name-index records: every FACE of every
/// entry that declares a spec-valid PostScript name.
///
/// Every face contributes, not just the representative one: a `.ttc` holds several fonts with
/// distinct PostScript names in ONE file, and each of them must be locatable.
fn system_face_records_from_entries(entries: &[FontEntry]) -> Vec<SystemFaceRecord> {
    entries
        .iter()
        .flat_map(|entry| {
            entry
                .faces
                .iter()
                .filter(|face| !face.post_script_name.trim().is_empty())
                .map(|face| SystemFaceRecord {
                    post_script_name: face.post_script_name.clone(),
                    path: entry.path.clone(),
                })
        })
        .collect()
}

pub(super) fn font_group_name_for_path(fonts_dir: &Path, path: &Path) -> Option<String> {
    let mut components = path.strip_prefix(fonts_dir).ok()?.components();
    let first = components.next()?.as_os_str().to_str()?;
    if !first.eq_ignore_ascii_case("groups") {
        return None;
    }
    components
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .map(ToOwned::to_owned)
}

pub(super) fn collect_font_files_recursive(root_dir: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };

    for entry_result in read_dir {
        let Ok(entry) = entry_result else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            if should_skip_font_dir(root_dir, &path) {
                continue;
            }
            collect_font_files_recursive(root_dir, &path, out);
            continue;
        }

        let ext = path
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "ttf" | "otf" | "ttc") {
            out.push(path);
        }
    }
}

pub(super) fn should_skip_font_dir(root_dir: &Path, dir: &Path) -> bool {
    dir.strip_prefix(root_dir)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|component| component.eq_ignore_ascii_case("ui"))
}
