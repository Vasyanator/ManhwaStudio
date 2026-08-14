/*
File: crates/ms-text-render/src/font_system_pool.rs

Purpose:
Process-global checkout pool of reusable `cosmic_text::FontSystem` instances so
that renders do not rebuild the font database on every call.

Every pooled system is built by `font_base::new_render_font_system`, i.e. over the
bundled `fonts/ui` base with the deterministic `MsFallback` chain. The OS font
database is NOT loaded (see `font_base.rs` and
`dev-docs/unicode_base_font_plan.md`, decision 1).

Why a pool and not a thread_local:
Renders run on freshly spawned, short-lived worker threads (live-edit render,
created overlays, preview tiles). A `thread_local!` `FontSystem` would be
re-initialized (re-paying the database build) on every new thread and give
almost no benefit on the hot live-edit path. A process-global pool survives
across threads, so a `FontSystem` built once is leased by whichever thread
renders next.

Main responsibilities:
- own a bounded, mutex-guarded free list of `PooledFontSystem` items, PARTITIONED
  by `EllipsisLigatureMode`;
- lease a `FontSystem` + its per-system `FontFaceCache` for the duration of one
  render and return it afterward (`with_leased_font_system`);
- bound growth by dropping systems whose face cache or the pool itself has grown
  past fixed limits (resets cosmic-text shaping/db growth on a long-lived
  `FontSystem`);
- expose `prewarm_font_system_pool` so the application can pay the first scan on
  a background thread before the first user render.

Key structures:
- `FontFaceCache`: per-`FontSystem` map of already-loaded font content (keyed by
  content id), plus the pristine default-family names captured at system creation,
  the face ids the system already held before this cache loaded anything (its
  bundled base) and a determinism taint flag.
- `PooledFontSystem`: a `FontSystem` bundled with its `FontFaceCache`.

Determinism guards (renderer requires byte-identical output for identical params
even on a reused pooled system):
- Pristine default families: a fresh render system seeds fontdb generic families
  (sans-serif/serif/monospace/cursive/fantasy) with the FIRST core face of the
  bundled base (`font_base::build_base_database`). `FontFaceCache::for_system`
  captures those names once so a render whose selected face has NO family name can
  RESTORE them instead of inheriting a prior render's family (see
  `font_registry::apply_default_families`).
- Taint-and-drop: font matching is by family name. If two DIFFERENT contents
  (different content id) declare the same `(family, weight, style, stretch)`,
  cosmic-text may resolve `Family::Name` to the wrong (earlier-loaded) face —
  history-dependent. The loader marks the cache `tainted` on such a collision and
  `return_to_pool` DROPS a tainted system so it can never serve a future render.
  Documented residual: the single render that first triggers the collision may
  still mis-match before the system is dropped (rare, self-healing).
- ELLIPSIS-PATCH PARTITION: `TextRenderParams.force_remove_ellipsis_glyph` makes
  the loader register a PATCHED copy of the caller's font (see
  `font_ligature_patch.rs`), and the flag is per render — so one process renders
  the same font both ways. `FontFaceCache` is keyed by content id ALONE, so a
  single system could serve only one of the two variants, and widening the key to
  `(content id, mode)` would instead put two faces with the same
  `(family, weight, style, stretch)` into one database — the very collision
  `collides_with_other_file` taints and drops the system for. The pool is
  therefore split by mode: a `FontSystem` is created in one mode, only ever loads
  faces of that mode, and is only ever leased to a render of that mode. The cache
  carries the mode (`FontFaceCache::ellipsis_mode`) so the loader reads it off the
  system it is loading into instead of taking it as an argument, which makes the
  wrong combination unrepresentable. Growth: the modes SHARE the
  `MAX_POOLED_SYSTEMS` ceiling (the feature must not double resident font
  memory), with `MAX_POOLED_SYSTEMS_PER_MODE` reserving room for the other mode.
- Displacement-and-drop: when the caller's font declares a family the BUNDLED base
  also declares, the loader removes the bundled faces from that system's database so
  the user's own file wins the match (`font_registry::displace_bundled_faces`). The
  system then no longer holds the base it was built on, so it is tainted for the same
  reason and dropped. `capture_preexisting_faces` is what tells the base apart from
  faces earlier renders added.

Notes:
`FontSystem` is `Send` but not `Sync`; ownership is moved in/out of the pool
under a `Mutex`, so nothing is shared while leased. `with_leased_font_system`
uses no Drop guard (a guard would need `Option` + `unwrap`); a panic inside the
closure leaks that one system, which the pool simply recreates, and the renderer
must not panic anyway.
*/

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use cosmic_text::{FontSystem, fontdb};

use super::font_base;
use super::font_ligature_patch::EllipsisLigatureMode;
use super::font_registry::RegisteredFontFace;

/// Maximum number of distinct CALLER font contents a pooled `FontSystem` may
/// accumulate before it is dropped instead of returned. Keeps a long-lived
/// `FontSystem` from growing without bound as many different fonts are rendered.
///
/// Re-verified after the move to the bundled font base (`font_base.rs`), and kept
/// at 64: the counter measures only fonts registered through `FontFaceCache`, i.e.
/// caller-supplied `Arc<Vec<u8>>` buffers that are FULLY RESIDENT for the life of
/// the system — that is the memory this bound exists to cap, and the bundled base
/// does not contribute to it. The base changed the OTHER two costs in the safe
/// direction: per-system face metadata dropped from the OS database (thousands of
/// faces) to ~50, and the bundled `ext` bytes are file MAPPINGS
/// (`fontdb::Source::File`) whose pages the kernel shares between every pooled
/// system, so they do not scale with the pool size the way resident buffers do.
const MAX_CACHED_FILES: usize = 64;

/// Maximum number of `FontSystem` instances kept warm in the free list, across
/// ALL ellipsis-patch modes. Extra systems returned beyond this are dropped.
const MAX_POOLED_SYSTEMS: usize = 12;

/// Maximum number of warm systems ONE `EllipsisLigatureMode` may occupy.
///
/// The pool is partitioned by mode (see the file header), and the total ceiling
/// above is deliberately NOT doubled: patched and unpatched systems compete for
/// the same 12 slots, so enabling the feature cannot double the renderer's
/// resident font memory. This per-mode cap only stops one mode from taking every
/// slot, which on a project that mixes both settings would leave the other mode
/// rebuilding a `FontSystem` on every render. With 8 of 12, whichever mode runs
/// first keeps at most 8 and the other still finds 4 warm systems — more than the
/// number of renders the app runs concurrently.
const MAX_POOLED_SYSTEMS_PER_MODE: usize = 8;

/// Snapshot of a `FontSystem`'s generic default-family names, captured once at
/// system creation so a later render can restore the pristine matching state.
///
/// Each field is `Some(name)` when the fresh db had a non-empty name for that
/// generic, and `None` otherwise (e.g. an empty-db throwaway system). Restoring
/// only touches the `Some` entries, so an unset generic is never clobbered.
#[derive(Debug, Default, Clone)]
struct PristineDefaultFamilies {
    sans_serif: Option<String>,
    serif: Option<String>,
    monospace: Option<String>,
    cursive: Option<String>,
    fantasy: Option<String>,
}

impl PristineDefaultFamilies {
    /// Reads the current generic default-family names from `db`. Intended to be
    /// called on a freshly created `FontSystem` before any render mutates the
    /// defaults, so the captured names are the pristine ones.
    #[must_use]
    fn capture(db: &fontdb::Database) -> Self {
        // `family_name` returns the concrete name a generic resolves to; empty
        // means unset (empty-db throwaway systems), stored as `None`.
        fn name(db: &fontdb::Database, family: fontdb::Family) -> Option<String> {
            let resolved = db.family_name(&family);
            if resolved.is_empty() {
                None
            } else {
                Some(resolved.to_string())
            }
        }
        Self {
            sans_serif: name(db, fontdb::Family::SansSerif),
            serif: name(db, fontdb::Family::Serif),
            monospace: name(db, fontdb::Family::Monospace),
            cursive: name(db, fontdb::Family::Cursive),
            fantasy: name(db, fontdb::Family::Fantasy),
        }
    }

    /// Restores each captured generic default family into `db`. Only `Some`
    /// entries are written, so generics that were unset at capture time keep
    /// whatever value they currently hold.
    fn restore_into(&self, db: &mut fontdb::Database) {
        if let Some(name) = self.sans_serif.as_ref() {
            db.set_sans_serif_family(name.clone());
        }
        if let Some(name) = self.serif.as_ref() {
            db.set_serif_family(name.clone());
        }
        if let Some(name) = self.monospace.as_ref() {
            db.set_monospace_family(name.clone());
        }
        if let Some(name) = self.cursive.as_ref() {
            db.set_cursive_family(name.clone());
        }
        if let Some(name) = self.fantasy.as_ref() {
            db.set_fantasy_family(name.clone());
        }
    }
}

/// Cache of font contents already loaded into ONE `FontSystem`'s fontdb, keyed
/// by content id. Prevents re-adding duplicate faces when the `FontSystem` is
/// reused across renders (the source of unbounded fontdb growth before pooling).
///
/// Also carries two per-system determinism guards: the pristine default-family
/// names captured at creation (`pristine`, restored on a no-family render) and a
/// `tainted` flag set when two distinct contents collide on one family name (a
/// tainted system is dropped rather than reused). See the file header.
///
/// Travels with its owning `FontSystem` inside `PooledFontSystem`, so its
/// entries always reflect exactly what that system's db has loaded.
#[derive(Debug, Default)]
pub struct FontFaceCache {
    /// Font contents already loaded, mapped to the fontdb IDs they produced.
    files: HashMap<u64, Vec<fontdb::ID>>,
    /// Resolved face metadata per `(content_id, face_index)` so a reused system
    /// does not re-read face records from the db.
    meta: HashMap<(u64, usize), RegisteredFontFace>,
    /// Generic default-family names captured at system creation. Empty for
    /// caches built with `new()` (throwaway systems); populated by `for_system`.
    pristine: PristineDefaultFamilies,
    /// Face ids the owning `FontSystem` already held before this cache loaded
    /// anything into it, i.e. the bundled base of that system. `None` until the
    /// first load captures it.
    preexisting: Option<HashSet<fontdb::ID>>,
    /// Set when a family-name collision between two distinct files is detected.
    /// A tainted system is dropped by `return_to_pool`, never reused.
    tainted: bool,
    /// Whether faces loaded through this cache have their ellipsis-producing
    /// ligatures removed. Fixed for the life of the owning `FontSystem`; see the
    /// ELLIPSIS-PATCH PARTITION contract in the file header.
    ellipsis_mode: EllipsisLigatureMode,
}

impl FontFaceCache {
    /// Creates an empty cache with NO pristine defaults captured. Used by
    /// one-shot throwaway `FontSystem`s (e.g. metric measurement) that route
    /// through the cache-aware loader but are never pooled, so leaving the
    /// pristine defaults empty (no restore) does not affect determinism.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a cache that captures `font_system`'s pristine default-family
    /// names. Call on a freshly built pooled `FontSystem` so a later no-family
    /// render can restore the matching state a fresh system would have used.
    #[must_use]
    pub fn for_system(font_system: &FontSystem) -> Self {
        Self {
            pristine: PristineDefaultFamilies::capture(font_system.db()),
            ..Self::default()
        }
    }

    /// Returns this cache with `mode` fixed as its ellipsis-patch mode.
    ///
    /// Consuming builder on purpose: the mode must be decided when the owning
    /// `FontSystem` is created and never change afterwards, because the faces
    /// already registered in that system were loaded under it.
    #[must_use]
    pub(crate) fn with_ellipsis_mode(mut self, mode: EllipsisLigatureMode) -> Self {
        self.ellipsis_mode = mode;
        self
    }

    /// The ellipsis-patch mode every load through this cache must use.
    #[must_use]
    pub(crate) fn ellipsis_mode(&self) -> EllipsisLigatureMode {
        self.ellipsis_mode
    }

    /// Restores the captured pristine default families into `font_system`'s db.
    /// No-op for the generics that were unset at capture time. Used on a render
    /// whose selected face has no family name, so matching falls back to the
    /// fresh-system defaults instead of a prior render's family.
    pub(crate) fn restore_pristine_defaults(&self, font_system: &mut FontSystem) {
        self.pristine.restore_into(font_system.db_mut());
    }

    /// Records, ONCE, which faces the owning system holds before this cache loads
    /// anything into it — the bundled base that system was built on.
    ///
    /// Must be called before the first `load_font_source`, i.e. at the top of
    /// `font_registry::load_font_content`; afterwards it is a no-op. The set is what
    /// tells a BUNDLED face (which a caller font of the same family must displace)
    /// from a face an earlier render of this same system registered (which it must
    /// not: both files may be needed together as inline fonts, so that case is
    /// handled by taint alone).
    ///
    /// Captured per system rather than read off the process-wide base, because
    /// fontdb ids are database-local: the same id means different faces in the
    /// pooled systems and in the typing panel's throwaway metric system.
    pub(crate) fn capture_preexisting_faces(&mut self, font_system: &FontSystem) {
        if self.preexisting.is_none() {
            self.preexisting = Some(font_system.db().faces().map(|face| face.id).collect());
        }
    }

    /// Whether `id` is one of the faces captured by `capture_preexisting_faces`.
    /// Always `false` before the first capture, so nothing can be removed by mistake.
    #[must_use]
    pub(crate) fn is_preexisting_face(&self, id: fontdb::ID) -> bool {
        self.preexisting
            .as_ref()
            .is_some_and(|ids| ids.contains(&id))
    }

    /// Whether a family-name collision between two distinct files has tainted
    /// this system's matching. A tainted system must not be returned to the pool.
    #[must_use]
    pub(crate) fn is_tainted(&self) -> bool {
        self.tainted
    }

    /// Marks this cache/system tainted after a family-name collision so
    /// `return_to_pool` drops it instead of reusing it.
    pub(crate) fn mark_tainted(&mut self) {
        self.tainted = true;
    }

    /// Reports whether a DIFFERENT already-loaded content declares the same
    /// `(family, weight, style, stretch)` as `new_face`, which would make
    /// `Family::Name` matching history-dependent on a reused system.
    ///
    /// Returns `false` when `new_face` has no family name: a nameless face is
    /// never selected by `Family::Name`, so it cannot collide. Compares against
    /// stored metadata only (every loaded content has at least one meta entry).
    #[must_use]
    pub(crate) fn collides_with_other_file(
        &self,
        new_key: u64,
        new_face: &RegisteredFontFace,
    ) -> bool {
        let Some(new_family) = new_face.family_name.as_deref() else {
            return false;
        };
        self.meta
            .iter()
            .any(|((existing_key, _), existing_face)| {
                *existing_key != new_key
                    && existing_face.family_name.as_deref() == Some(new_family)
                    && existing_face.weight == new_face.weight
                    && existing_face.style == new_face.style
                    && existing_face.stretch == new_face.stretch
            })
    }

    /// Number of distinct font contents loaded through this cache. Used to bound
    /// pooled-system growth and in tests to assert dedup.
    #[must_use]
    pub(crate) fn distinct_file_count(&self) -> usize {
        self.files.len()
    }

    /// Returns the fontdb IDs previously loaded for `content_id`, if any.
    #[must_use]
    pub(crate) fn loaded_ids(&self, content_id: u64) -> Option<&[fontdb::ID]> {
        self.files.get(&content_id).map(Vec::as_slice)
    }

    /// Records the fontdb IDs produced by loading `content_id`'s bytes.
    pub(crate) fn store_loaded(&mut self, content_id: u64, ids: Vec<fontdb::ID>) {
        self.files.insert(content_id, ids);
    }

    /// Returns cached face metadata for `(content_id, face_index)`, if resolved
    /// before.
    #[must_use]
    pub(crate) fn cached_meta(
        &self,
        content_id: u64,
        face_index: usize,
    ) -> Option<&RegisteredFontFace> {
        self.meta.get(&(content_id, face_index))
    }

    /// Stores resolved face metadata for `(content_id, face_index)`.
    pub(crate) fn store_meta(
        &mut self,
        content_id: u64,
        face_index: usize,
        face: RegisteredFontFace,
    ) {
        self.meta.insert((content_id, face_index), face);
    }
}

/// A `FontSystem` bundled with its dedup cache and a render counter (used only
/// for diagnostics/growth reasoning).
#[derive(Debug)]
struct PooledFontSystem {
    system: FontSystem,
    cache: FontFaceCache,
    render_count: u32,
}

impl PooledFontSystem {
    /// Builds a fresh pooled system for `mode` over the deterministic bundled font
    /// base (`font_base::new_render_font_system`), never over the OS font database.
    ///
    /// The first call in a process resolves the `fonts/ui` manifest and reads the
    /// resident tiers (blocking I/O — see `prewarm_font_system_pool`); later calls
    /// only clone the shared `fontdb::Database`, which copies `Arc`s and name
    /// strings but no font bytes (`fontdb-0.16.2/src/lib.rs:151-159`).
    ///
    /// `mode` is fixed for the life of the system: it decides how every face
    /// loaded into it is preprocessed, so it can never be changed on a system that
    /// already holds faces (see the ELLIPSIS-PATCH PARTITION contract).
    #[must_use]
    fn new(mode: EllipsisLigatureMode) -> Self {
        // Build the system first, then capture its pristine default families so a
        // no-family render can restore fresh-system matching regardless of pool
        // history (see file header, determinism guards).
        let system = font_base::new_render_font_system();
        let cache = FontFaceCache::for_system(&system).with_ellipsis_mode(mode);
        Self {
            system,
            cache,
            render_count: 0,
        }
    }
}

/// Global free list of warm `FontSystem`s. `FontSystem` is `Send`, so moving it
/// in/out under a `Mutex` is sound; nothing is shared while a system is leased.
static POOL: OnceLock<Mutex<Vec<PooledFontSystem>>> = OnceLock::new();

/// Returns the process-global pool, initializing it on first use.
fn pool() -> &'static Mutex<Vec<PooledFontSystem>> {
    POOL.get_or_init(|| Mutex::new(Vec::new()))
}

/// Leases a warm system OF `mode` from the pool, creating a new one when the pool
/// holds none.
///
/// A system of another mode is never handed out: its database already holds faces
/// preprocessed for that other mode (see the ELLIPSIS-PATCH PARTITION contract in
/// the file header). The scan is over at most `MAX_POOLED_SYSTEMS` entries.
///
/// Recovers from a poisoned mutex (a panic in another lease) instead of
/// propagating it: the pooled `Vec` is never left structurally invalid, so the
/// data behind the poison is safe to reuse.
#[must_use]
fn checkout(mode: EllipsisLigatureMode) -> PooledFontSystem {
    let mut guard = match pool().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Take the most recently parked matching system (warmest caches).
    let found = guard
        .iter()
        .rposition(|pooled| pooled.cache.ellipsis_mode() == mode)
        .map(|index| guard.remove(index));
    drop(guard);
    found.unwrap_or_else(|| PooledFontSystem::new(mode))
}

/// Returns a leased system to the pool, or drops it to bound growth or preserve
/// determinism.
///
/// Drops (does not requeue) the system when its matching has been tainted by a
/// cross-file family-name collision (so a contaminated system can never serve a
/// future render), when its face cache has grown past `MAX_CACHED_FILES`, when
/// the pool already holds `MAX_POOLED_SYSTEMS`, or when this system's ellipsis
/// mode already occupies `MAX_POOLED_SYSTEMS_PER_MODE` slots. Dropping also
/// resets cosmic-text shaping/db growth accumulated on a long-lived system.
fn return_to_pool(pooled: PooledFontSystem) {
    if !should_requeue(&pooled.cache) {
        // Dropped for determinism (tainted) or growth (too many cached files);
        // see `should_requeue`. Dropping also resets cosmic-text's internal
        // shaping/db growth accumulated on this long-lived system.
        return;
    }
    let mode = pooled.cache.ellipsis_mode();
    let mut guard = match pool().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let same_mode = guard
        .iter()
        .filter(|parked| parked.cache.ellipsis_mode() == mode)
        .count();
    if guard.len() < MAX_POOLED_SYSTEMS && same_mode < MAX_POOLED_SYSTEMS_PER_MODE {
        guard.push(pooled);
    }
    // Otherwise drop: the pool, or this mode's share of it, is already full.
}

/// Whether a returned system is healthy enough to requeue for reuse.
///
/// Returns `false` — meaning DROP the system — when its matching was tainted by a
/// cross-file family-name collision (reusing it would render identical params
/// differently, so dropping it makes the regression self-healing) or when its
/// face cache has grown past `MAX_CACHED_FILES`. The `MAX_POOLED_SYSTEMS` cap is
/// enforced separately in `return_to_pool` because it depends on live pool state.
#[must_use]
fn should_requeue(cache: &FontFaceCache) -> bool {
    !cache.is_tainted() && cache.distinct_file_count() <= MAX_CACHED_FILES
}

/// Runs `f` with a leased `FontSystem` of `mode` and its `FontFaceCache`,
/// returning the system to the global pool afterward.
///
/// `mode` selects the pool PARTITION: the leased system has only ever loaded
/// faces preprocessed for that mode, and its cache reports the mode back to
/// `font_registry::load_font_content`, which is how the per-render
/// `force_remove_ellipsis_glyph` decision reaches the loader without widening the
/// content-id cache key. See the ELLIPSIS-PATCH PARTITION contract in the file
/// header.
///
/// `f`'s result is returned as-is (including `Err`), and the system is returned
/// to the pool on every non-panicking path. A panic inside `f` leaks that one
/// system (the pool simply recreates it); the renderer must not panic.
pub(crate) fn with_leased_font_system<R>(
    mode: EllipsisLigatureMode,
    f: impl FnOnce(&mut FontSystem, &mut FontFaceCache) -> R,
) -> R {
    let mut pooled = checkout(mode);
    let result = f(&mut pooled.system, &mut pooled.cache);
    pooled.render_count = pooled.render_count.saturating_add(1);
    return_to_pool(pooled);
    result
}

/// Pre-builds one `FontSystem` and parks it in the pool so the first user render
/// does not pay the bundled-base build on the hot path.
///
/// That first build resolves the `fonts/ui` manifest and reads the resident tiers
/// (~19 MB of blocking I/O), which is exactly why this must run off the GUI thread.
/// Intended to be called once from a background thread at startup. Cheap to call
/// again (it just leases and returns a system).
///
/// Only the DEFAULT (`Keep`) partition is warmed. The expensive part — resolving
/// the manifest and reading the resident tiers into the process-wide base — is
/// shared by every partition, so a first render with
/// `force_remove_ellipsis_glyph` still only pays for cloning the already-built
/// database.
pub fn prewarm_font_system_pool() {
    let pooled = checkout(EllipsisLigatureMode::default());
    return_to_pool(pooled);
}

#[cfg(test)]
mod tests {
    use super::{
        EllipsisLigatureMode, FontFaceCache, checkout, font_base, return_to_pool, should_requeue,
        with_leased_font_system,
    };
    use crate::font_provider::{FontContent, font_content_id};
    use crate::font_registry::load_font_content;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Returns the path to a real font fixture so the test exercises actual
    /// fontdb loading, not a mock. Same fixture the pipeline tests use.
    fn test_font_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf")
    }

    /// Builds a `FontContent` over the fixture bytes with an EXPLICIT `content_id`
    /// so a test can model two distinct contents (e.g. two virtual fonts) that
    /// declare the same family. `content_id` is the cache key the loader dedups on.
    fn content_with_id(bytes: &Arc<Vec<u8>>, content_id: u64) -> FontContent {
        FontContent {
            name: "test-font".to_string(),
            original_name: "test-font".to_string(),
            data: Arc::clone(bytes) as crate::font_provider::FontBytes,
            face_index: 0,
            content_id,
        }
    }

    /// Reads the fixture and builds the `FontContent` a `FontProvider` would hand the
    /// loader for it: the real bytes plus the content id derived from them.
    fn fixture_content(font_path: &std::path::Path) -> Option<FontContent> {
        let bytes = Arc::new(std::fs::read(font_path).ok()?);
        let content_id = font_content_id(bytes.as_slice());
        Some(content_with_id(&bytes, content_id))
    }

    #[test]
    fn loading_same_file_twice_does_not_grow_faces() {
        let font_path = test_font_path();
        if !font_path.exists() {
            // The dedup contract is only meaningful against a real font file.
            // Skip rather than fabricate a fake fontdb entry.
            eprintln!(
                "skipping loading_same_file_twice_does_not_grow_faces: font not found at {}",
                font_path.display()
            );
            return;
        }

        // Built like production (bundled base, no OS font scan) so the dedup count
        // is measured against the same database a real render sees.
        let mut system = font_base::new_render_font_system();
        let mut cache = FontFaceCache::new();

        // The SAME content resolved twice, exactly as two renders of one font do.
        let content = fixture_content(&font_path).expect("the fixture must be readable");
        let first = load_font_content(&mut system, &mut cache, &content, 0)
            .expect("first font load should succeed");
        let faces_after_first = system.db().len();
        assert_eq!(cache.distinct_file_count(), 1, "one distinct file cached");

        let second = load_font_content(&mut system, &mut cache, &content, 0)
            .expect("second font load should succeed");
        let faces_after_second = system.db().len();

        assert_eq!(
            faces_after_first, faces_after_second,
            "reloading the same file must not add duplicate faces"
        );
        assert_eq!(
            cache.distinct_file_count(),
            1,
            "the cache must still hold a single distinct file"
        );
        assert_eq!(
            first.family_name, second.family_name,
            "reused metadata must match the freshly resolved face"
        );
    }

    #[test]
    fn content_id_is_stable_for_same_bytes() {
        // The load-cache key must be a pure function of the bytes: identical bytes
        // share one id (dedup into a reused FontSystem once), different bytes get
        // different ids. This is the content-id analog of the old FileKey identity.
        let a = font_content_id(b"same font bytes");
        let b = font_content_id(b"same font bytes");
        let c = font_content_id(b"other font bytes");
        assert_eq!(a, b, "identical bytes must produce an equal content id");
        assert_ne!(a, c, "different bytes must produce different content ids");
    }

    #[test]
    fn leased_system_exposes_the_bundled_base_and_returns_to_pool() {
        // Lease a system, touch it, and confirm a subsequent checkout can reuse a
        // pooled system (the pool is non-empty after the lease returns).
        let face_count = with_leased_font_system(EllipsisLigatureMode::Keep, |system, _cache| {
            // Touch the system so the closure genuinely uses the lease.
            system.db().len()
        });
        // The leased database must be EXACTLY the bundled `fonts/ui` stack — never
        // the operating system's fonts, which is what made renders machine
        // dependent (`dev-docs/unicode_base_font_plan.md`, decision 1). When no
        // stack is resolvable (a test binary's working directory is its package
        // root) the deterministic answer is an empty database, not a system scan.
        let expected = ms_fonts::stack().map_or(0, |stack| {
            stack.core().len() + stack.bold().len() + stack.ext().len()
        });
        assert_eq!(
            face_count, expected,
            "a leased FontSystem must expose the bundled base and nothing else"
        );
        // After the lease, at least one system should be parked. Check out and
        // return it to confirm reuse works without panicking.
        let pooled = checkout(EllipsisLigatureMode::Keep);
        return_to_pool(pooled);

        // The assert above is only a proof of the NEGATIVE (nothing scanned the OS),
        // because a test binary cannot resolve the manifest. Measure the POSITIVE
        // against the shipped bundle through the same constructor the pool uses,
        // with the manifest addressed from `CARGO_MANIFEST_DIR`.
        let Some(shipped) = font_base::test_bundle::stack() else {
            eprintln!(
                "skipping the bundle-backed half of \
                 leased_system_exposes_the_bundled_base_and_returns_to_pool: fonts/ui is not \
                 present next to this checkout"
            );
            return;
        };
        let system =
            font_base::test_bundle::font_system().expect("the shipped stack was just resolved");
        assert_eq!(
            system.db().len(),
            shipped.file_count(),
            "a pooled system built over the shipped bundle must hold every bundled file"
        );
        // A pooled system captures its pristine defaults at creation; on the bundled
        // base they must name a font that actually exists, not fontdb's "Arial".
        let cache = FontFaceCache::for_system(&system);
        let restored_from = cache
            .pristine
            .sans_serif
            .clone()
            .expect("the bundled base must seed a sans-serif default");
        assert!(
            system
                .db()
                .faces()
                .any(|face| face.families.iter().any(|(name, _)| *name == restored_from)),
            "the pristine default family '{restored_from}' must be a face of the bundle"
        );
    }

    #[test]
    fn tainted_cache_is_dropped_not_requeued() {
        // A clean cache is healthy and requeued; a tainted one is dropped. This
        // is the drop DECISION `return_to_pool` applies. We assert the decision
        // rather than the global pool length because the process-global pool is
        // shared across parallel tests, so exact-count assertions are racy.
        let mut cache = FontFaceCache::new();
        assert!(
            should_requeue(&cache),
            "a fresh, untainted cache must be requeued"
        );
        cache.mark_tainted();
        assert!(
            !should_requeue(&cache),
            "a tainted cache must be dropped, never reused"
        );
    }

    #[test]
    fn two_contents_same_family_taint_and_drop() {
        // Two DIFFERENT contents (different content id) declaring the SAME family
        // name must taint the system so it is dropped instead of reused. This is
        // exactly the virtual-font case: two renamed contents backed by the same
        // family. We give the same fixture bytes two distinct content ids so the
        // loader treats them as separate loads (no dedup) that then collide on the
        // shared family name.
        let font_path = test_font_path();
        if !font_path.exists() {
            eprintln!(
                "skipping two_contents_same_family_taint_and_drop: font not found at {}",
                font_path.display()
            );
            return;
        }
        let bytes = match std::fs::read(&font_path) {
            Ok(bytes) => Arc::new(bytes),
            Err(err) => {
                eprintln!(
                    "skipping two_contents_same_family_taint_and_drop: could not read fixture: {err}"
                );
                return;
            }
        };

        // Built like production (bundled base, no OS font scan).
        let mut system = font_base::new_render_font_system();
        let mut cache = FontFaceCache::for_system(&system);

        // Distinct explicit content ids model two different (e.g. virtual) fonts.
        let first = load_font_content(&mut system, &mut cache, &content_with_id(&bytes, 1), 0)
            .expect("first content load should succeed");
        assert!(
            !cache.is_tainted(),
            "loading the first distinct content must not taint the cache"
        );
        assert!(
            should_requeue(&cache),
            "an untainted single-content cache must be requeuable"
        );

        let second = load_font_content(&mut system, &mut cache, &content_with_id(&bytes, 2), 0)
            .expect("second content load should succeed");

        // Both contents declare the same family name, so the second load collides.
        assert_eq!(
            first.family_name, second.family_name,
            "the second content must declare the same family as the first"
        );
        assert!(
            cache.is_tainted(),
            "a second distinct content with the same family name must taint the cache"
        );
        assert!(
            !should_requeue(&cache),
            "a tainted cache must be dropped by return_to_pool, not requeued"
        );
    }

    /// A lease must never hand out a system of the OTHER ellipsis partition: its
    /// database already holds faces preprocessed for that other mode.
    #[test]
    fn a_lease_always_matches_the_requested_ellipsis_mode() {
        // Alternate so both a fresh build and a reuse of each partition are hit.
        for mode in [
            EllipsisLigatureMode::Keep,
            EllipsisLigatureMode::Remove,
            EllipsisLigatureMode::Remove,
            EllipsisLigatureMode::Keep,
            EllipsisLigatureMode::Keep,
            EllipsisLigatureMode::Remove,
        ] {
            with_leased_font_system(mode, |_system, cache| {
                assert_eq!(
                    cache.ellipsis_mode(),
                    mode,
                    "a lease must come from the requested pool partition"
                );
            });
        }
    }

    /// End-to-end guard for the partition: the SAME font rendered through the pool
    /// in both modes must shape `...` differently every time, in any order.
    ///
    /// Without the partition the first variant to enter a system would win for
    /// every later render on it — `FontFaceCache` is keyed by content id alone —
    /// and the feature would silently render the wrong thing.
    #[test]
    fn both_ellipsis_modes_keep_shaping_the_same_font_their_own_way() {
        use cosmic_text::{Attrs, Metrics};

        let bytes = crate::font_ligature_patch::test_fixture::bytes();
        let content = FontContent {
            name: "ellipsis-fixture".to_string(),
            original_name: "ellipsis-fixture".to_string(),
            data: Arc::new(bytes.clone()),
            face_index: 0,
            content_id: font_content_id(&bytes),
        };

        let shaped_dots = |mode| {
            with_leased_font_system(mode, |system, cache| {
                let face = load_font_content(system, cache, &content, 0)
                    .expect("the fixture must load into a leased system");
                let attrs = face.apply_to_attrs(Attrs::new().metrics(Metrics::new(32.0, 32.0)));
                let glyphs = font_base::test_bundle::shaped_glyphs(system, "...", &attrs);
                // The pool is process-global and shared with every other test in
                // this binary; taint the system so the fixture face is dropped
                // with it instead of lingering in a requeued database.
                cache.mark_tainted();
                glyphs.len()
            })
        };

        for _ in 0..2 {
            assert_eq!(
                shaped_dots(EllipsisLigatureMode::Remove),
                3,
                "with the patch the three periods must stay three glyphs"
            );
            assert_eq!(
                shaped_dots(EllipsisLigatureMode::Keep),
                1,
                "without the patch the fixture's liga rule must fuse them into one"
            );
        }
    }
}
