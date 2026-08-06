/*
File: panel/font_provider.rs

Purpose:
App-side implementation of the renderer's `FontProvider` contract. The renderer
resolves the main font (`TextRenderParams.font_name`) and inline `<font=...>` tags
by WORKING NAME through a provider; this module builds that provider from the
typing panel's font list and loads bytes lazily.

Main responsibilities:
- map a normalized working name to a resolvable font entry, keyed PRIMARILY by each
  font's identity (`FontEntry.identity_name`: the representative face's PostScript
  name, `%hash`-suffixed on a collision), with the bare contested name, the family
  name, the file stem and the display label kept as READ-ONLY legacy aliases;
- read font bytes lazily OUTSIDE the lock and cache the shared buffer + content id so
  a repeated resolve does not re-read the file, re-reading only when the file behind
  the cached bytes has actually changed;
- serve the synthetic BUNDLED `fonts/ui` entry from the `'static` bytes `ms-fonts`
  already holds, so the built-in font is never read a second time;
- carry each font's ORIGINAL name (real family/name) through to the renderer for
  callers that need the real identity (e.g. PSD export, future virtual fonts).

Key structures:
- `ProviderEntry`: how to obtain one font's bytes (a file path, or a bundled stack
  font whose bytes are already resident).
- `FontByteSource`: what a cache slot belongs to — a file, or one bundled stack font.
- `FileStamp` / `CachedFontBytes`: the cached buffer and what proves it is still current.
- `TabFontProvider`: the panel-owned `FontProvider`.

Notes:
Normalization mirrors the renderer's `normalize_inline_font_label`
(`trim().to_ascii_lowercase()`) so a name resolves identically on both sides. The
identity primary key is unique in the common case (`assign_font_identity_names`
suffixes files that claim one PostScript name with different bytes); any residual
key collision is deterministic FIRST-wins over the font list and logged (see
`from_fonts`).

ALIASES ARE A READ PATH ONLY. Everything except `identity_name` is registered so
that data written by an older build — a family name, a file stem, a display label,
the previous bundled-UI spelling, or a bare PostScript name that has since become
contested — still resolves. Nothing in the app may WRITE those forms any more:
stored documents are converted to the identity on load (`tab/codec.rs`,
`panel/text_params_schema.rs`).

THE BYTE CACHE EXPIRES. A font file replaced IN PLACE — with no font-list reload,
which would rebuild this provider — used to render from the bytes of the first
resolve for the rest of the session. The cache therefore re-checks a file's size and
modification time at most once per `CACHE_REVALIDATION_INTERVAL` per font (one
`fs::metadata`, taken outside the lock) and re-reads on a difference. It is never a
check per resolve: `resolve` runs on the render threads for every text image and
every inline `<font=…>` span.

A failing resolve is never silent: an unreadable file is logged with its path and
the OS reason (once per path and operation, since the resolve is retried on every
render), and a poisoned cache mutex is recovered and logged once instead of dropping
the cache and re-reading the file on every resolve. Only an UNKNOWN name resolves to
`None` without a log — it is not an error at this layer.
*/

use super::*;
use crate::tabs::typing::render_next::{FontBytes, FontContent, FontProvider, font_content_id};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

/// How long cached font bytes are served before the file behind them is checked again.
///
/// The check is ONE `fs::metadata` per font per interval — never per glyph and never per
/// resolve: `resolve` is called for every text image (and every inline `<font=…>` span) on
/// the render threads, so a `stat` per call would put thousands of syscalls into a page
/// render. Two seconds is short enough that a font a user just replaced on disk is picked
/// up while they are still looking at the same page, and long enough that the check is
/// invisible next to the render itself.
const CACHE_REVALIDATION_INTERVAL: Duration = Duration::from_secs(2);

/// What a cached buffer belongs to.
///
/// The BUNDLED entry and a user import of the same file are two different sources even
/// though they share a path: the bundled one serves the `'static` `ms_fonts` buffer that
/// the renderer's font base recognizes as already resident, while the import serves the
/// file's current bytes. One cache slot for both would let whichever resolved first
/// decide what the other gets.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FontByteSource {
    /// Bytes read from this file on disk.
    File(PathBuf),
    /// Bytes of a bundled stack font, already resident for the process. The path only
    /// says WHICH file of the bundle it is; the bytes never come from it, so this source
    /// is never revalidated.
    Bundled(PathBuf),
}

/// What proves that cached bytes are still the file's current content.
///
/// Size plus modification time, i.e. exactly what a `stat` yields. It cannot prove
/// equality (a replacement of identical size within one timestamp tick slips through),
/// but it costs one syscall instead of re-reading and re-hashing the file, which is the
/// trade this cache exists for. A font replaced by the app's own font administration is
/// picked up regardless: that path rebuilds the whole provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    /// File size in bytes.
    len: u64,
    /// Modification time, `None` on a platform or filesystem that does not report one.
    modified: Option<SystemTime>,
}

/// Cached bytes of one font source plus what tells them apart from newer content.
///
/// Deliberately not `Debug`: `data` is a whole font file, which must never reach a log
/// line (`CLAUDE.md` §8), and the erased `dyn AsRef<[u8]>` fontdb takes cannot derive it
/// anyway.
#[derive(Clone)]
struct CachedFontBytes {
    data: FontBytes,
    /// Stable id of `data` (`font_content_id`), handed to the renderer's load cache.
    content_id: u64,
    /// Stamp of the file taken BEFORE the read. `None` when the metadata could not be
    /// read, which disables revalidation for this entry (there is nothing to compare).
    ///
    /// Taken before rather than after the read on purpose: a file replaced DURING the
    /// read then leaves a stamp that no longer matches, so the next check re-reads. The
    /// other order would pair old bytes with the new stamp and never notice.
    stamp: Option<FileStamp>,
    /// When `stamp` was last confirmed against the file.
    checked_at: Instant,
}

/// One resolvable font entry: how to obtain its bytes plus the face to use and the
/// font's original name.
///
/// `bundled` is `Some` only for the synthetic built-in entry; its bytes come from
/// `ms_fonts::bytes` (read once per process, shared with the egui UI and with the
/// renderer's own font base) instead of from a second read of the same file.
#[derive(Debug, Clone)]
struct ProviderEntry {
    path: PathBuf,
    face_index: usize,
    original_name: String,
    bundled: Option<&'static ms_fonts::StackFont>,
}

/// App-side font provider: maps a working name (font label, normalized) to a font.
/// Obtains bytes lazily (outside the lock) and caches the shared buffer + content id
/// so a name resolves without re-reading. Built once per font-list revision and
/// shared (Arc) with background render threads. Future virtual fonts add synthesized
/// entries whose bytes are composed rather than read.
pub(in crate::tabs::typing) struct TabFontProvider {
    /// key = normalized name (`trim().to_ascii_lowercase()`): primarily each font's
    /// `identity_name` (its PostScript name), plus READ-ONLY legacy aliases — the bare
    /// contested name, the family name, the file stem, the label and the previous
    /// bundled-UI spelling (see `from_fonts`).
    by_name: HashMap<String, ProviderEntry>,
    /// byte source -> cached bytes; populated lazily on first resolve of a font and
    /// re-read when the file behind it changed (see [`CACHE_REVALIDATION_INTERVAL`]).
    cache: Mutex<HashMap<FontByteSource, CachedFontBytes>>,
    /// `(path, operation)` pairs already reported. A failing resolve is retried on every
    /// later resolve (the file may reappear), so without this set the same warning would
    /// be written on every render.
    reported_failures: Mutex<HashSet<(PathBuf, &'static str)>>,
    /// Set once a poisoned provider mutex has been reported, so the recovery is logged
    /// once per provider instead of on every resolve.
    poison_reported: AtomicBool,
    /// How long cached bytes are served before the file is checked again. Always
    /// [`CACHE_REVALIDATION_INTERVAL`] outside tests, which pass `Duration::ZERO` to make
    /// the check deterministic.
    revalidate_after: Duration,
    /// Test-only counter of `fs::metadata` calls, per provider, so a test can pin that
    /// the cache does NOT stat on every resolve.
    #[cfg(test)]
    stat_calls: std::sync::atomic::AtomicUsize,
}

/// Hand-written because the cached buffers are erased (`dyn AsRef<[u8]>`, the type
/// fontdb takes) and cannot derive `Debug`. Reports sizes only — never font bytes.
impl std::fmt::Debug for TabFontProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cached = self.cache.lock().map(|cache| cache.len()).ok();
        formatter
            .debug_struct("TabFontProvider")
            .field("names", &self.by_name.len())
            .field("cached_files", &cached)
            .finish()
    }
}

/// Normalizes a working font name for lookup: the ONE identity normalization
/// (`fonts::normalize_font_identity`), which mirrors the renderer's
/// `normalize_inline_font_label` so both sides key on the same string.
fn normalize_name(name: &str) -> String {
    fonts::normalize_font_identity(name)
}

impl TabFontProvider {
    /// Builds a provider from the panel's font list. The PRIMARY key is each font's
    /// normalized identity (`FontEntry.identity_name`, the representative face's
    /// PostScript name) — the canonical render identity persisted in
    /// `render_data`/`TextRenderParams.font_name` and emitted in `<font=...>` tags. The
    /// representative face is used.
    ///
    /// Every OTHER key is a READ-ONLY LEGACY ALIAS, kept solely so data written by an
    /// older build still resolves; the app must never write any of these forms again
    /// (stored documents are converted to the identity on load, see `tab/codec.rs`).
    /// They are, in insertion order:
    ///
    /// 1. `fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY` for the bundled entry — projects
    ///    saved before the reserved name was renamed persisted that exact spelling.
    /// 2. `"{base}%{own content hash}"` for EVERY font, including uncontested ones: a
    ///    document written while the name WAS contested keeps resolving after the other
    ///    claimant is removed and this font goes back to owning the bare name.
    /// 3. The bare (unsuffixed) base name, inserted in ascending content-hash order, so
    ///    a contested name deterministically resolves to its LOWEST-hash claimant.
    ///    Reserved bundled-UI spellings are excluded: they must never fall back to a
    ///    user font.
    /// 4. The original family name, then the display LABEL, then the file stem — the
    ///    forms older projects and older inline tags persisted (a blob written with a
    ///    family name, an old project that saved only `font_path` and derives the name
    ///    from the file stem, an inline tag naming `"{stem} [system]"`).
    ///
    /// The user display-name OVERRIDE (`display_label`) is never a key — it is a
    /// presentation-only rename and must not affect resolution.
    ///
    /// Precedence and collisions: identities are inserted FIRST with deterministic
    /// FIRST-wins over the given font order, and every alias pass uses `or_insert`, so
    /// a weaker alias can never displace an identity or an earlier alias.
    /// `assign_font_identity_names` suffixes files that claim one PostScript name with
    /// different bytes, so a residual identity collision is rare; when it happens the
    /// first font in the list claims the key and it is logged (naming both paths).
    /// Renderer resolution is name-only BY DESIGN — exact-file selection is a panel
    /// concern (path-first), not the provider's.
    #[must_use]
    pub(in crate::tabs::typing) fn from_fonts(fonts: &[FontEntry]) -> Self {
        Self::with_revalidation(fonts, CACHE_REVALIDATION_INTERVAL)
    }

    /// [`Self::from_fonts`] with an explicit cache revalidation interval.
    ///
    /// The interval is a parameter for ONE reason: tests pass `Duration::ZERO` so a
    /// replaced font file is detected on the very next resolve instead of after a wall
    /// clock wait. Production always goes through [`Self::from_fonts`].
    #[must_use]
    fn with_revalidation(fonts: &[FontEntry], revalidate_after: Duration) -> Self {
        let mut by_name = HashMap::with_capacity(fonts.len());
        let entry_for = |font: &FontEntry| ProviderEntry {
            path: font.path.clone(),
            face_index: font.faces.first().map(|face| face.face_index).unwrap_or(0),
            original_name: font.original_name.clone(),
            bundled: font.bundled_stack_font(),
        };
        // Primary keys: normalized collision-aware identities, FIRST-wins on collision.
        for font in fonts {
            let identity = font.identity_name.trim();
            if identity.is_empty() {
                continue;
            }
            let key = normalize_name(identity);
            match by_name.entry(key) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(entry_for(font));
                }
                std::collections::hash_map::Entry::Occupied(existing) => {
                    // Two entries resolved to the same identity (should be rare after
                    // `assign_font_identity_names`): keep the first and warn.
                    if existing.get().path != font.path {
                        crate::runtime_log::log_warn(format!(
                            "TabFontProvider: render identity '{}' is shared by two files; \
                             resolving to '{}' and shadowing '{}'. The shadowed font \
                             stays reachable only by its file-stem/label alias.",
                            identity,
                            existing.get().path.display(),
                            font.path.display(),
                        ));
                    }
                }
            }
        }
        // Legacy alias of the RESERVED bundled-UI name: projects saved before the
        // rename named the built-in font by its previous spelling. Inserted right after
        // the identities so no user font's family/label/stem alias can take it.
        for font in fonts {
            if font.bundled_stack_font().is_none() {
                continue;
            }
            by_name
                .entry(normalize_name(fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY))
                .or_insert_with(|| entry_for(font));
        }
        // Stability alias `{base}%{own hash}`: an entry that is NOT contested today may
        // have been contested when a document was written (and vice versa). Registering
        // its own suffixed form unconditionally means neither the bare nor the suffixed
        // spelling stops resolving when the other claimant comes or goes.
        for font in fonts {
            if font.bundled_stack_font().is_some() {
                continue;
            }
            let base = font.base_identity_name();
            if base.trim().is_empty() {
                continue;
            }
            by_name
                .entry(normalize_name(&fonts::suffixed_font_identity_name(
                    &base,
                    font.content_hash,
                )))
                .or_insert_with(|| entry_for(font));
        }
        // Bare (unsuffixed) base name for a CONTESTED name: `assign_font_identity_names`
        // suffixed every claimant, so nothing owns the bare form as an identity, yet
        // documents written before the contest persisted exactly that. Ascending content
        // hash makes the winner deterministic (the lowest-hash claimant) and independent
        // of list order.
        let mut by_content_hash: Vec<&FontEntry> = fonts
            .iter()
            .filter(|font| font.bundled_stack_font().is_none())
            .collect();
        // Stable sort: equal hashes keep list order, so the pass stays FIRST-wins there.
        by_content_hash.sort_by_key(|font| font.content_hash);
        for font in by_content_hash {
            let base = font.base_identity_name();
            // A reserved spelling must never resolve to a user font, not even when the
            // bundled stack is unavailable and nothing shadows it.
            if base.trim().is_empty() || fonts::is_reserved_bundled_identity(&base) {
                continue;
            }
            by_name
                .entry(normalize_name(&base))
                .or_insert_with(|| entry_for(font));
        }
        // Family-name alias next (FIRST-wins), so a blob persisted with a family name
        // that later became a collision still resolves to the first declaring font —
        // matching the panel's whole-list first-match resolution.
        for font in fonts {
            let original = font.original_name.trim();
            if !original.is_empty() {
                by_name
                    .entry(normalize_name(original))
                    .or_insert_with(|| entry_for(font));
            }
        }
        // Label/stem aliases, only for keys not already claimed (so an identity or
        // family key never loses to an alias). Labels before stems so a label beats
        // another font's identical stem, matching the historical alias precedence.
        for font in fonts {
            by_name
                .entry(normalize_name(&font.label))
                .or_insert_with(|| entry_for(font));
        }
        for font in fonts {
            // The bundled entry deliberately claims NO stem alias: its path is a file
            // of the shipped `fonts/ui` bundle, and letting it own that stem would
            // shadow a user's own copy of the same file for stem-only lookups. It is
            // reachable by its reserved identity, which is the only name ever
            // persisted for it.
            if font.bundled_stack_font().is_some() {
                continue;
            }
            if let Some(stem) = font.path.file_stem().and_then(|s| s.to_str()) {
                by_name
                    .entry(normalize_name(stem))
                    .or_insert_with(|| entry_for(font));
            }
        }
        Self {
            by_name,
            cache: Mutex::new(HashMap::new()),
            reported_failures: Mutex::new(HashSet::new()),
            poison_reported: AtomicBool::new(false),
            revalidate_after,
            #[cfg(test)]
            stat_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Locks one of the provider's maps, recovering it from a poisoned mutex and
    /// reporting the poisoning ONCE per provider.
    ///
    /// The guarded sections only look up and insert into a map, so a panic elsewhere
    /// cannot leave one half-updated. Recovery is what keeps the byte cache usable:
    /// abandoning it would make every later resolve re-read the font file from disk on
    /// a background render thread, silently and forever.
    fn lock_map<'a, T>(&self, mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
        match mutex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                if !self.poison_reported.swap(true, Ordering::Relaxed) {
                    crate::runtime_log::log_warn(format!(
                        "TabFontProvider: the {what} mutex is poisoned (a thread panicked \
                         while holding it); the map is recovered and font resolution \
                         continues with it. Reported once per provider."
                    ));
                }
                poisoned.into_inner()
            }
        }
    }

    /// Reports a font-file failure of `operation`, at most once per `(path, operation)`
    /// for this provider.
    ///
    /// The resolve itself still behaves the same on every attempt (see `resolve`); only
    /// the log line is de-duplicated, so a font file that reappears is still picked up.
    /// `message` is built lazily, so a repeated failure costs a set lookup and nothing
    /// else — this runs on render threads, per text image.
    fn report_once(
        &self,
        path: &Path,
        operation: &'static str,
        message: impl FnOnce() -> String,
    ) {
        let mut reported = self.lock_map(&self.reported_failures, "failure-report");
        if !reported.insert((path.to_path_buf(), operation)) {
            return;
        }
        crate::runtime_log::log_warn(message());
    }

    /// Whether the cached bytes stamped `cached_stamp` are still what `path` holds.
    ///
    /// Costs ONE `fs::metadata`. On `true` the entry's check timestamp is refreshed, so
    /// the next `revalidate_after` worth of resolves pay nothing at all. Answers `true`
    /// (keep serving) in the two cases where the question cannot be decided:
    /// - the entry carries no stamp (its metadata was unreadable when it was cached);
    /// - the file's metadata cannot be read now — it was deleted or became unreachable.
    ///   Serving the last known good bytes is what keeps an open page rendering; the
    ///   situation is reported once per path.
    fn cached_bytes_still_current(
        &self,
        path: &Path,
        cached_stamp: Option<FileStamp>,
        source: &FontByteSource,
    ) -> bool {
        let Some(cached_stamp) = cached_stamp else {
            self.mark_checked(source);
            return true;
        };
        let current = self.file_stamp(path);
        let unchanged = match current {
            Some(current) => current == cached_stamp,
            None => {
                self.report_once(path, "check font file freshness", || {
                    format!(
                        "TabFontProvider: cannot read the metadata of a cached font file; \
                         operation: check whether the cached bytes are still current; path: \
                         '{}'. The font keeps rendering from the bytes read earlier. \
                         Possible cause: the file was deleted, renamed or its volume is \
                         unavailable. Reported once per path.",
                        path.display()
                    )
                });
                true
            }
        };
        if unchanged {
            self.mark_checked(source);
        }
        unchanged
    }

    /// Restarts the revalidation interval of `source`'s cache entry.
    ///
    /// A no-op when the entry has meanwhile been dropped or replaced by another thread —
    /// that thread stamped its own fresh entry.
    fn mark_checked(&self, source: &FontByteSource) {
        let mut cache = self.lock_map(&self.cache, "byte-cache");
        if let Some(cached) = cache.get_mut(source) {
            cached.checked_at = Instant::now();
        }
    }

    /// Size + modification time of `path`, or `None` when they cannot be read.
    ///
    /// A `None` is not an error at this layer: it only means the entry cannot be
    /// revalidated, and `resolve` keeps serving the bytes it already has.
    fn file_stamp(&self, path: &Path) -> Option<FileStamp> {
        #[cfg(test)]
        self.stat_calls.fetch_add(1, Ordering::Relaxed);
        let metadata = std::fs::metadata(path).ok()?;
        Some(FileStamp {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

impl FontProvider for TabFontProvider {
    /// Resolves a working `name` to its content. Obtains the bytes lazily on a cache
    /// miss (never holding the lock across the read) and caches them with their
    /// content id. Returns `None` for an unknown name or an unreadable file (a
    /// missing font surfaces as a render error upstream) — the two cases are told
    /// apart in the log: an unreadable file is reported with its path and reason
    /// (once per path), an unknown name is not an error here.
    ///
    /// CACHED BYTES EXPIRE. A file REPLACED IN PLACE — without the font list being
    /// reloaded, which would rebuild this provider — used to be rendered from the bytes
    /// of the first resolve for the rest of the session. At most once per
    /// [`CACHE_REVALIDATION_INTERVAL`] per font the file's size and modification time are
    /// checked (one `fs::metadata`, outside the lock) and the bytes are re-read when they
    /// differ. Never once per resolve: this runs on the render threads for every text
    /// image and every inline `<font=…>` span. A file whose metadata cannot be read at
    /// all (it was deleted) keeps being served from the cache — the last known good bytes
    /// beat a page that suddenly loses its font — and is reported once.
    ///
    /// The synthetic BUNDLED entry takes its bytes from `ms_fonts::bytes` — the very
    /// `'static` buffer the egui UI and the renderer's font base already share — so
    /// the file is not read again AND the renderer can recognize the buffer as
    /// already registered instead of adding a duplicate face
    /// (`ms_text_render::font_base::resident_face_ids`). Those bytes cannot change, so a
    /// bundled entry is never revalidated, and it holds its OWN cache slot: a user import
    /// of the same file is a different byte source (see [`FontByteSource`]).
    fn resolve(&self, name: &str) -> Option<FontContent> {
        let entry = self.by_name.get(&normalize_name(name))?.clone();
        let source = match entry.bundled {
            Some(_) => FontByteSource::Bundled(entry.path.clone()),
            None => FontByteSource::File(entry.path.clone()),
        };

        // Fast path: bytes already cached. Clone the Arc + id and release the lock; the
        // freshness check below never runs with the lock held.
        let cached = {
            let cache = self.lock_map(&self.cache, "byte-cache");
            cache.get(&source).cloned()
        };
        if let Some(cached) = cached {
            let fresh = entry.bundled.is_some()
                || cached.checked_at.elapsed() < self.revalidate_after
                || self.cached_bytes_still_current(&entry.path, cached.stamp, &source);
            if fresh {
                return Some(FontContent {
                    name: name.to_string(),
                    original_name: entry.original_name,
                    data: Arc::clone(&cached.data),
                    face_index: entry.face_index,
                    content_id: cached.content_id,
                });
            }
            // The file changed: fall through and re-read it, replacing the entry.
        }

        // Slow path: obtain the bytes OUTSIDE the lock, then insert.
        // The stamp is taken BEFORE the read (see `CachedFontBytes::stamp`).
        let stamp = entry
            .bundled
            .is_none()
            .then(|| self.file_stamp(&entry.path))
            .flatten();
        let (data, content_id): (FontBytes, u64) = match entry.bundled {
            // `ms_fonts::bytes` logs its own failure with the path and the reason.
            Some(font) => {
                let bytes = ms_fonts::bytes(font)?;
                (Arc::new(bytes), font_content_id(bytes))
            }
            None => match std::fs::read(&entry.path) {
                Ok(bytes) => {
                    let content_id = font_content_id(&bytes);
                    (Arc::new(bytes), content_id)
                }
                Err(error) => {
                    self.report_once(&entry.path, "read font bytes", || {
                        format!(
                            "TabFontProvider: cannot read font file; operation: resolve font \
                             bytes; path: '{}'; error: {error}. The name resolves to nothing, \
                             which the renderer reports as a missing font. Reported once per \
                             path.",
                            entry.path.display()
                        )
                    });
                    return None;
                }
            },
        };
        {
            let mut cache = self.lock_map(&self.cache, "byte-cache");
            // `insert`, not `or_insert_with`: this may be a REPLACEMENT of bytes that have
            // just been found stale. A concurrent resolve of the same font read the same
            // file, so whichever write lands last is equally correct.
            cache.insert(
                source,
                CachedFontBytes {
                    data: Arc::clone(&data),
                    content_id,
                    stamp,
                    checked_at: Instant::now(),
                },
            );
        }
        Some(FontContent {
            name: name.to_string(),
            original_name: entry.original_name,
            data,
            face_index: entry.face_index,
            content_id,
        })
    }
}

/// Test-only introspection: the source path a normalized `name` currently resolves to,
/// WITHOUT reading the backing file (so fixtures with non-existent paths are testable).
/// Lets sibling panel tests assert that a name resolves to the SAME font the panel picks.
#[cfg(test)]
impl TabFontProvider {
    pub(in crate::tabs::typing::panel) fn resolved_path_for(&self, name: &str) -> Option<&Path> {
        self.by_name
            .get(&normalize_name(name))
            .map(|entry| entry.path.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal `FontEntry` fixture for provider-key tests with an explicit
    /// PostScript name and content hash, and the per-entry base identity (what
    /// `assign_font_identity_names` leaves for an uncontested name).
    fn font_entry_with_post_script(
        label: &str,
        path: &str,
        original_name: &str,
        post_script_name: &str,
        content_hash: u64,
    ) -> FontEntry {
        FontEntry {
            kind: FontEntryKind::File,
            label: label.to_string(),
            path: PathBuf::from(path),
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces: vec![FontFaceEntry {
                label: "Face 0".to_string(),
                face_index: 0,
                post_script_name: post_script_name.to_string(),
            }],
            coverage: FontLanguageCoverage::default(),
            original_name: original_name.to_string(),
            post_script_name: post_script_name.to_string(),
            content_hash,
            display_name: None,
            identity_name: super::super::fonts::base_font_identity_name(
                post_script_name,
                original_name,
                label,
            ),
            virtual_group_aliases: std::collections::BTreeMap::new(),
        }
    }

    /// Builds a fixture WITHOUT a PostScript name (an unparsable file), so its identity
    /// exercises the family-or-label fallback. The content hash is derived from the path
    /// so distinct fixture files stay distinct fonts, as real distinct files are.
    fn font_entry(label: &str, path: &str, original_name: &str) -> FontEntry {
        font_entry_with_post_script(
            label,
            path,
            original_name,
            "",
            super::super::fonts::font_content_hash(path.as_bytes()),
        )
    }

    /// Builds a font list and runs the collision-aware identity assignment on it, so
    /// provider tests key on the SAME identities the panel does.
    fn fonts_with_identities(fonts: Vec<FontEntry>) -> Vec<FontEntry> {
        let mut fonts = fonts;
        super::super::fonts::assign_font_identity_names(&mut fonts);
        fonts
    }

    #[test]
    fn resolves_by_post_script_name_family_label_and_stem() {
        // A system font: label is "arial [system]", stem is "arial", family is "Arial",
        // PostScript name is "ArialMT".
        let fonts = fonts_with_identities(vec![font_entry_with_post_script(
            "arial [system]",
            "/usr/share/fonts/arial.ttf",
            "Arial",
            "ArialMT",
            7,
        )]);
        let provider = TabFontProvider::from_fonts(&fonts);
        // Primary key: the identity (the PostScript name). Back-compat aliases: the
        // family name, display label and file stem all still resolve.
        for name in ["ArialMT", "arialmt", "Arial", "arial", "arial [system]", "ARIAL [System]"] {
            assert!(
                provider.by_name.contains_key(&normalize_name(name)),
                "provider must resolve font by '{name}'"
            );
        }
    }

    #[test]
    fn post_script_name_is_primary_key() {
        // The identity IS the PostScript name; it maps to THIS font.
        let fonts = fonts_with_identities(vec![font_entry_with_post_script(
            "основной",
            "/fonts/основной.ttf",
            "Anime Ace v05",
            "AnimeAcev05",
            11,
        )]);
        let provider = TabFontProvider::from_fonts(&fonts);
        let entry = provider
            .by_name
            .get(&normalize_name("AnimeAcev05"))
            .expect("the PostScript name must be a key");
        assert_eq!(entry.path, PathBuf::from("/fonts/основной.ttf"));
        // The legacy family and stem/label aliases still resolve to the same font.
        for legacy in ["Anime Ace v05", "основной"] {
            assert_eq!(
                provider.by_name.get(&normalize_name(legacy)).map(|e| &e.path),
                Some(&PathBuf::from("/fonts/основной.ttf")),
                "the legacy form '{legacy}' must still resolve to the same font"
            );
        }
    }

    #[test]
    fn shared_family_pair_each_file_resolves_to_itself() {
        // Regular + Bold shipped as separate files share one family name but NOT their
        // PostScript names, so each identity is distinct by construction and each file
        // renders itself (no silent swap); the family-name alias falls to the first.
        let fonts = fonts_with_identities(vec![
            font_entry_with_post_script(
                "myfont-regular",
                "/fonts/regular.ttf",
                "MyFont",
                "MyFont-Regular",
                1,
            ),
            font_entry_with_post_script("myfont-bold", "/fonts/bold.ttf", "MyFont", "MyFont-Bold", 2),
        ]);
        let provider = TabFontProvider::from_fonts(&fonts);
        // Each file resolves to ITSELF by its own identity (its PostScript name).
        assert_eq!(
            provider.by_name.get(&normalize_name("MyFont-Regular")).map(|e| &e.path),
            Some(&PathBuf::from("/fonts/regular.ttf")),
            "the Regular file resolves to itself"
        );
        assert_eq!(
            provider.by_name.get(&normalize_name("MyFont-Bold")).map(|e| &e.path),
            Some(&PathBuf::from("/fonts/bold.ttf")),
            "the Bold file resolves to itself"
        );
        // Legacy stem/label tags keep resolving to their own file, too.
        assert_eq!(
            provider.by_name.get(&normalize_name("myfont-bold")).map(|e| &e.path),
            Some(&PathBuf::from("/fonts/bold.ttf")),
            "a legacy stem tag still resolves to its own file"
        );
        // The shared family alias falls, FIRST-wins, to the first font in the list.
        assert_eq!(
            provider.by_name.get(&normalize_name("MyFont")).map(|e| &e.path),
            Some(&PathBuf::from("/fonts/regular.ttf")),
            "a blob still naming the family resolves to the first declaring font"
        );
    }

    /// Two DIFFERENT files claiming one PostScript name: each keeps a distinct
    /// `%hash`-suffixed identity, and the bare name — the form already persisted in old
    /// documents — resolves to the LOWEST-hash claimant, deterministically and
    /// independently of list order.
    #[test]
    fn contested_post_script_name_suffixes_both_and_bare_name_picks_the_lowest_hash() {
        let fonts = fonts_with_identities(vec![
            font_entry_with_post_script("dup-b", "/fonts/b.ttf", "Dup", "DupFont", 0x9000_0000_0000_0000),
            font_entry_with_post_script("dup-a", "/fonts/a.ttf", "Dup", "DupFont", 0x1000_0000_0000_0000),
        ]);
        let provider = TabFontProvider::from_fonts(&fonts);

        assert_eq!(
            provider.resolved_path_for("DupFont%9000000000000000"),
            Some(Path::new("/fonts/b.ttf")),
            "each claimant resolves by its own content-hash-suffixed identity"
        );
        assert_eq!(
            provider.resolved_path_for("DupFont%1000000000000000"),
            Some(Path::new("/fonts/a.ttf"))
        );
        assert_eq!(
            provider.resolved_path_for("DupFont"),
            Some(Path::new("/fonts/a.ttf")),
            "the bare contested name resolves to the lowest-hash claimant, not the first listed"
        );
    }

    /// A suffixed identity must keep resolving after the OTHER claimant is removed, when
    /// the surviving font goes back to owning the bare name: documents written during the
    /// contest must not become unresolvable.
    #[test]
    fn a_suffixed_identity_still_resolves_after_the_other_claimant_disappears() {
        let survivor = font_entry_with_post_script("dup-a", "/fonts/a.ttf", "Dup", "DupFont", 0x1000_0000_0000_0000);
        let alone = fonts_with_identities(vec![survivor]);
        assert_eq!(
            alone[0].identity_name, "DupFont",
            "with no contest the identity is the bare PostScript name"
        );
        let provider = TabFontProvider::from_fonts(&alone);
        assert_eq!(
            provider.resolved_path_for("DupFont%1000000000000000"),
            Some(Path::new("/fonts/a.ttf")),
            "the suffixed form stays a resolution alias so old documents keep rendering"
        );
    }

    #[test]
    fn display_name_override_is_never_a_key() {
        let mut entry = font_entry("basic", "/fonts/basic.ttf", "Basic Family");
        entry.display_name = Some("My Pretty Name".to_string());
        let fonts = fonts_with_identities(vec![entry]);
        let provider = TabFontProvider::from_fonts(&fonts);
        assert!(
            !provider.by_name.contains_key(&normalize_name("My Pretty Name")),
            "a display-name override is presentation-only and must not be a resolution key"
        );
        // The real identity + aliases still resolve.
        for name in ["Basic Family", "basic"] {
            assert!(provider.by_name.contains_key(&normalize_name(name)));
        }
    }

    /// Builds the panel list the way a real reload does — user fonts finalized, then
    /// the bundled entry prepended — or `None` when this process has no `fonts/ui`
    /// stack (then there is nothing to assert).
    fn fonts_with_bundled(fonts: Vec<FontEntry>) -> Option<Vec<FontEntry>> {
        let mut fonts = fonts_with_identities(fonts);
        super::super::fonts::prepend_bundled_ui_font(&mut fonts);
        fonts
            .first()
            .and_then(FontEntry::bundled_stack_font)
            .is_some()
            .then_some(fonts)
    }

    #[test]
    fn built_in_font_resolves_by_its_reserved_identity() {
        let Some(fonts) = fonts_with_bundled(vec![font_entry(
            "myfont",
            "/fonts/myfont.ttf",
            "My Font",
        )]) else {
            eprintln!("skipping built_in_font_resolves_by_its_reserved_identity: no fonts/ui stack");
            return;
        };
        let bundled_path = fonts[0].path.clone();
        let provider = TabFontProvider::from_fonts(&fonts);

        assert_eq!(
            provider.resolved_path_for(fonts::BUNDLED_UI_FONT_IDENTITY),
            Some(bundled_path.as_path()),
            "the reserved identity must resolve to the bundled core font file"
        );
        // Projects saved before the reserved name was renamed persisted the previous
        // spelling; it must keep resolving to the same built-in entry.
        assert_eq!(
            provider.resolved_path_for(fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY),
            Some(bundled_path.as_path()),
            "the legacy spelling of the reserved identity must still resolve"
        );
        // The bundle's file STEM is deliberately not an alias of the built-in entry.
        let stem = bundled_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        assert!(
            !stem.is_empty() && provider.resolved_path_for(stem).is_none(),
            "the built-in entry must not claim the bundled file's stem as an alias"
        );
    }

    #[test]
    fn a_user_font_claiming_the_reserved_name_is_shadowed_by_the_built_in_entry() {
        // A user font claiming a reserved spelling: it is given a content-hash-suffixed
        // identity, so the reserved name is not even contested, and the built-in entry
        // (first in the list) owns both spellings — by the same FIRST-wins rule over the
        // same list, in the panel's `find_font_idx_by_name_forms` as well.
        //
        // The two spellings are claimed through DIFFERENT doors, and that is the point:
        // `"ManhwaStudio-UI"` is a spec-valid PostScript name, so a font can declare it as
        // its own; `"ManhwaStudio UI"` contains a SPACE, which the spec forbids, so no
        // valid PostScript name can ever be it — a font can only reach that spelling
        // through the family-name fallback of a file with no valid PostScript name.
        for reserved in [
            fonts::BUNDLED_UI_FONT_IDENTITY,
            fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY,
        ] {
            let claims_by_post_script = fonts::is_valid_post_script_name(reserved);
            let impostor = if claims_by_post_script {
                font_entry_with_post_script(
                    "impostor",
                    "/fonts/impostor.ttf",
                    "Impostor Family",
                    reserved,
                    0x2a00_0000_0000_0000,
                )
            } else {
                // No PostScript name at all, and the reserved spelling as the FAMILY name:
                // the only way this spelling can be claimed.
                font_entry_with_post_script(
                    "impostor",
                    "/fonts/impostor.ttf",
                    reserved,
                    "",
                    0x2a00_0000_0000_0000,
                )
            };
            let Some(fonts) = fonts_with_bundled(vec![impostor]) else {
                eprintln!(
                    "skipping a_user_font_claiming_the_reserved_name_is_shadowed_by_the_built_in_entry: \
                     no fonts/ui stack"
                );
                return;
            };
            let bundled_path = fonts[0].path.clone();
            assert_eq!(
                fonts[1].identity_name,
                format!("{reserved}%2a00000000000000"),
                "a user font claiming '{reserved}' must be suffixed off the reserved name"
            );
            let provider = TabFontProvider::from_fonts(&fonts);

            for spelling in [
                fonts::BUNDLED_UI_FONT_IDENTITY,
                fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY,
            ] {
                assert_eq!(
                    provider.resolved_path_for(spelling),
                    Some(bundled_path.as_path()),
                    "the built-in entry must keep '{spelling}' against a user font claiming \
                     '{reserved}'"
                );
            }
            // The shadowed font stays reachable by its own suffixed identity and by its
            // own label/stem.
            for own in [format!("{reserved}%2a00000000000000"), "impostor".to_string()] {
                assert_eq!(
                    provider.resolved_path_for(&own),
                    Some(Path::new("/fonts/impostor.ttf")),
                    "the shadowed font must stay reachable by '{own}'"
                );
            }
        }
    }

    /// A build WITHOUT the built-in entry (an older app version opening a project that
    /// uses it) must report the font as unknown, so the panel shows the normal
    /// "font not found" state instead of silently rendering with someone else's font.
    /// This holds for BOTH reserved spellings, and even when a user font claims one.
    #[test]
    fn without_the_built_in_entry_the_reserved_name_resolves_to_nothing() {
        let fonts = fonts_with_identities(vec![
            font_entry("myfont", "/fonts/myfont.ttf", "My Font"),
            font_entry_with_post_script(
                "impostor",
                "/fonts/impostor.ttf",
                "Other Family",
                fonts::BUNDLED_UI_FONT_IDENTITY,
                5,
            ),
        ]);
        let provider = TabFontProvider::from_fonts(&fonts);
        for spelling in [
            fonts::BUNDLED_UI_FONT_IDENTITY,
            fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY,
        ] {
            assert!(
                provider.resolved_path_for(spelling).is_none(),
                "the reserved name '{spelling}' must not fall back to an unrelated font"
            );
        }
    }

    /// Selecting the built-in font must NOT add a face to the render database: its
    /// bytes are the very `'static` buffer `font_base` already registered, and a
    /// second registration would put a duplicate `(family, weight, style)` face into
    /// every pooled `FontSystem`.
    #[test]
    fn the_built_in_font_registers_no_duplicate_face_in_the_renderer() {
        use crate::tabs::typing::render_next::{
            FontFaceCache, load_font_content, new_render_font_system,
        };

        let Some(fonts) = fonts_with_bundled(Vec::new()) else {
            eprintln!(
                "skipping the_built_in_font_registers_no_duplicate_face_in_the_renderer: \
                 no fonts/ui stack"
            );
            return;
        };
        let provider = TabFontProvider::from_fonts(&fonts);
        let content = provider
            .resolve(fonts::BUNDLED_UI_FONT_IDENTITY)
            .expect("the built-in font must resolve to content");

        let mut system = new_render_font_system();
        let mut cache = FontFaceCache::for_system(&system);
        let faces_before = system.db().len();
        let face = load_font_content(&mut system, &mut cache, &content, content.face_index)
            .expect("the bundled core font must load");

        assert_eq!(
            system.db().len(),
            faces_before,
            "loading the built-in font must reuse the resident face, not add one"
        );
        // It resolves to the REAL family of the core file, which is what the renderer
        // shapes with; the reserved identity is only the provider-side key.
        let core_family = fonts[0]
            .bundled_stack_font()
            .map(|font| font.family_name)
            .unwrap_or_default();
        assert_eq!(
            face.family_name.as_deref(),
            Some(core_family),
            "the selected face must be the bundled core face"
        );
    }

    /// An unreadable (here: missing) file must resolve to `None` on every attempt, cache
    /// nothing, and be reported exactly ONCE — the renderer retries the resolve for every
    /// text image, so an undeduplicated warning would flood the session log.
    #[test]
    fn an_unreadable_file_resolves_to_none_and_is_reported_once() {
        let missing = "/definitely/not/a/font/dir/ghost.ttf";
        let fonts = fonts_with_identities(vec![font_entry("ghost", missing, "Ghost Family")]);
        let provider = TabFontProvider::from_fonts(&fonts);

        for _ in 0..3 {
            assert!(
                provider.resolve("Ghost Family").is_none(),
                "an unreadable file must resolve to nothing"
            );
        }

        let reported = provider
            .reported_failures
            .lock()
            .expect("no test thread panics while holding this lock");
        assert_eq!(
            reported.len(),
            1,
            "the read failure must be logged once, not once per resolve"
        );
        assert!(reported.contains(&(PathBuf::from(missing), "read font bytes")));
        let cache = provider
            .cache
            .lock()
            .expect("no test thread panics while holding this lock");
        assert!(
            cache.is_empty(),
            "a failed read must not cache anything (the file may reappear)"
        );
    }

    /// A poisoned cache mutex must not disable caching: the map is recovered, so a font
    /// resolved once is not re-read from disk on every later resolve.
    #[test]
    fn a_poisoned_cache_is_recovered_instead_of_disabling_the_cache() {
        let Some(fonts) = fonts_with_bundled(Vec::new()) else {
            eprintln!(
                "skipping a_poisoned_cache_is_recovered_instead_of_disabling_the_cache: \
                 no fonts/ui stack"
            );
            return;
        };
        let provider = Arc::new(TabFontProvider::from_fonts(&fonts));

        // Poison the cache mutex the only way a mutex gets poisoned: panic while holding it.
        let poisoner = Arc::clone(&provider);
        let panicked = std::thread::spawn(move || {
            let _guard = poisoner
                .cache
                .lock()
                .expect("the lock is not poisoned yet in this test");
            panic!("deliberate panic to poison the cache mutex");
        })
        .join();
        assert!(panicked.is_err(), "the helper thread must have panicked");
        assert!(provider.cache.is_poisoned());

        let first = provider
            .resolve(fonts::BUNDLED_UI_FONT_IDENTITY)
            .expect("a poisoned cache must not break resolution");
        assert!(
            provider
                .lock_map(&provider.cache, "byte-cache")
                .contains_key(&FontByteSource::Bundled(fonts[0].path.clone())),
            "the recovered cache must still be populated"
        );
        let second = provider
            .resolve(fonts::BUNDLED_UI_FONT_IDENTITY)
            .expect("the second resolve must hit the recovered cache");
        assert_eq!(first.content_id, second.content_id);
    }

    #[test]
    fn label_key_wins_over_alias_on_collision() {
        // Font A's LABEL equals font B's file stem ("beta"). The label key must point
        // to A (inserted in the label pass), not be overwritten by B's stem alias.
        // A's identity is its unique family "Alpha Family", so "beta" is only A's label.
        let fonts = fonts_with_identities(vec![
            font_entry("beta", "/fonts/a.ttf", "Alpha Family"),
            font_entry("gamma", "/fonts/beta.ttf", "Gamma Family"),
        ]);
        let provider = TabFontProvider::from_fonts(&fonts);
        let entry = provider
            .by_name
            .get(&normalize_name("beta"))
            .expect("'beta' must resolve");
        assert_eq!(
            entry.path,
            PathBuf::from("/fonts/a.ttf"),
            "a label must win over another font's stem alias"
        );
    }

    /// Creates a temp directory holding one fixture font file and returns
    /// `(directory, file path)`. The content is not a real font: this provider only
    /// hands bytes on, it never parses them.
    fn fixture_font_file(tag: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ms_provider_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the test temp directory must be creatable");
        let path = dir.join("font.ttf");
        std::fs::write(&path, bytes).expect("the fixture font file must be writable");
        (dir, path)
    }

    /// A font file REPLACED IN PLACE (no font-list reload, so this provider is not
    /// rebuilt) must stop being served from the cache: the renderer would otherwise draw
    /// the old typeface for the rest of the session.
    #[test]
    fn a_replaced_font_file_is_re_read_instead_of_served_from_the_cache() {
        let (dir, path) = fixture_font_file("revalidate", b"first font bytes");
        let fonts = fonts_with_identities(vec![font_entry(
            "repl",
            &path.to_string_lossy(),
            "Repl Family",
        )]);
        // Zero interval: the check happens on the next resolve instead of after a wall
        // clock wait. It is the only thing this parameter exists for.
        let provider = TabFontProvider::with_revalidation(&fonts, Duration::ZERO);

        let first = provider
            .resolve("Repl Family")
            .expect("the fixture file is readable");
        assert_eq!(first.data.as_ref().as_ref(), b"first font bytes");

        // A different LENGTH, so the check cannot depend on the filesystem's modification
        // time resolution.
        std::fs::write(&path, b"second font bytes, deliberately longer")
            .expect("the fixture font file must be re-writable");

        let second = provider
            .resolve("Repl Family")
            .expect("the replaced file is readable");
        assert_eq!(
            second.data.as_ref().as_ref(),
            b"second font bytes, deliberately longer",
            "the replaced file's bytes must be served, not the cached ones"
        );
        assert_ne!(
            first.content_id, second.content_id,
            "new bytes must carry a new content id, or the renderer reuses its loaded face"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The freshness check must NOT cost a syscall per resolve: `resolve` runs on the
    /// render threads for every text image and every inline `<font=…>` span.
    #[test]
    fn the_byte_cache_does_not_check_the_file_on_every_resolve() {
        let (dir, path) = fixture_font_file("nostat", b"font bytes that never change");
        let fonts = fonts_with_identities(vec![font_entry(
            "nostat",
            &path.to_string_lossy(),
            "Nostat Family",
        )]);
        let provider = TabFontProvider::from_fonts(&fonts);

        for _ in 0..8 {
            assert!(
                provider.resolve("Nostat Family").is_some(),
                "the fixture file is readable"
            );
        }
        assert_eq!(
            provider.stat_calls.load(Ordering::Relaxed),
            1,
            "only the stamp taken before the single read may touch the filesystem within \
             one revalidation interval"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
