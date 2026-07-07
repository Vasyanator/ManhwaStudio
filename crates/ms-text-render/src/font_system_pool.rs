/*
File: crates/ms-text-render/src/font_system_pool.rs

Purpose:
Process-global checkout pool of reusable `cosmic_text::FontSystem` instances so
that renders do not re-run the expensive system-font scan on every call.

Why a pool and not a thread_local:
Renders run on freshly spawned, short-lived worker threads (live-edit render,
created overlays, preview tiles). A `thread_local!` `FontSystem` would be
re-initialized (re-paying the system-font scan) on every new thread and give
almost no benefit on the hot live-edit path. A process-global pool survives
across threads, so a `FontSystem` built once is leased by whichever thread
renders next.

Main responsibilities:
- own a bounded, mutex-guarded free list of `PooledFontSystem` items;
- lease a `FontSystem` + its per-system `FontFaceCache` for the duration of one
  render and return it afterward (`with_leased_font_system`);
- bound growth by dropping systems whose face cache or the pool itself has grown
  past fixed limits (resets cosmic-text shaping/db growth on a long-lived
  `FontSystem`);
- expose `prewarm_font_system_pool` so the application can pay the first scan on
  a background thread before the first user render.

Key structures:
- `FontFaceCache`: per-`FontSystem` map of already-loaded font content (keyed by
  content id), plus the pristine default-family names captured at system creation
  and a determinism taint flag.
- `PooledFontSystem`: a `FontSystem` bundled with its `FontFaceCache`.

Determinism guards (renderer requires byte-identical output for identical params
even on a reused pooled system):
- Pristine default families: a fresh `FontSystem::new()` seeds fontdb generic
  families (sans-serif/serif/monospace/cursive/fantasy). `FontFaceCache::for_system`
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

Notes:
`FontSystem` is `Send` but not `Sync`; ownership is moved in/out of the pool
under a `Mutex`, so nothing is shared while leased. `with_leased_font_system`
uses no Drop guard (a guard would need `Option` + `unwrap`); a panic inside the
closure leaks that one system, which the pool simply recreates, and the renderer
must not panic anyway.
*/

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use cosmic_text::{FontSystem, fontdb};

use super::font_registry::RegisteredFontFace;

/// Maximum number of distinct font contents a pooled `FontSystem` may accumulate
/// before it is dropped instead of returned. Keeps a long-lived `FontSystem`
/// from growing without bound as many different fonts are rendered.
const MAX_CACHED_FILES: usize = 64;

/// Maximum number of `FontSystem` instances kept warm in the free list. Extra
/// systems returned beyond this are dropped.
const MAX_POOLED_SYSTEMS: usize = 12;

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
    /// Set when a family-name collision between two distinct files is detected.
    /// A tainted system is dropped by `return_to_pool`, never reused.
    tainted: bool,
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

    /// Restores the captured pristine default families into `font_system`'s db.
    /// No-op for the generics that were unset at capture time. Used on a render
    /// whose selected face has no family name, so matching falls back to the
    /// fresh-system defaults instead of a prior render's family.
    pub(crate) fn restore_pristine_defaults(&self, font_system: &mut FontSystem) {
        self.pristine.restore_into(font_system.db_mut());
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
    /// Builds a fresh pooled system. Constructing `FontSystem::new()` runs the
    /// system-font scan (the cost this pool exists to amortize) and keeps the
    /// default-locale behavior identical to the previous per-render creation.
    #[must_use]
    fn new() -> Self {
        // Build the system first, then capture its pristine default families so a
        // no-family render can restore fresh-system matching regardless of pool
        // history (see file header, determinism guards).
        let system = FontSystem::new();
        let cache = FontFaceCache::for_system(&system);
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

/// Leases a warm system from the pool, creating a new one if the pool is empty.
///
/// Recovers from a poisoned mutex (a panic in another lease) instead of
/// propagating it: the pooled `Vec` is never left structurally invalid, so the
/// data behind the poison is safe to reuse.
#[must_use]
fn checkout() -> PooledFontSystem {
    let mut guard = match pool().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.pop().unwrap_or_else(PooledFontSystem::new)
}

/// Returns a leased system to the pool, or drops it to bound growth or preserve
/// determinism.
///
/// Drops (does not requeue) the system when its matching has been tainted by a
/// cross-file family-name collision (so a contaminated system can never serve a
/// future render), when its face cache has grown past `MAX_CACHED_FILES`, or
/// when the pool already holds `MAX_POOLED_SYSTEMS`. Dropping also resets
/// cosmic-text shaping/db growth accumulated on a long-lived system.
fn return_to_pool(pooled: PooledFontSystem) {
    if !should_requeue(&pooled.cache) {
        // Dropped for determinism (tainted) or growth (too many cached files);
        // see `should_requeue`. Dropping also resets cosmic-text's internal
        // shaping/db growth accumulated on this long-lived system.
        return;
    }
    let mut guard = match pool().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.len() < MAX_POOLED_SYSTEMS {
        guard.push(pooled);
    }
    // Otherwise drop: the pool is already at capacity.
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

/// Runs `f` with a leased `FontSystem` and its `FontFaceCache`, returning the
/// system to the global pool afterward.
///
/// `f`'s result is returned as-is (including `Err`), and the system is returned
/// to the pool on every non-panicking path. A panic inside `f` leaks that one
/// system (the pool simply recreates it); the renderer must not panic.
pub(crate) fn with_leased_font_system<R>(
    f: impl FnOnce(&mut FontSystem, &mut FontFaceCache) -> R,
) -> R {
    let mut pooled = checkout();
    let result = f(&mut pooled.system, &mut pooled.cache);
    pooled.render_count = pooled.render_count.saturating_add(1);
    return_to_pool(pooled);
    result
}

/// Pre-builds one `FontSystem` and parks it in the pool so the first user render
/// does not pay the system-font scan on the hot path.
///
/// Intended to be called once from a background thread at startup. Cheap to call
/// again (it just leases and returns a system).
pub fn prewarm_font_system_pool() {
    let pooled = checkout();
    return_to_pool(pooled);
}

#[cfg(test)]
mod tests {
    use super::{
        FontFaceCache, checkout, return_to_pool, should_requeue, with_leased_font_system,
    };
    use crate::font_provider::{FontContent, font_content_id};
    use crate::font_registry::{load_font_content, load_selected_font_from_path};
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
            data: Arc::clone(bytes),
            face_index: 0,
            content_id,
        }
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

        let mut system = cosmic_text::FontSystem::new();
        let mut cache = FontFaceCache::new();

        let first = load_selected_font_from_path(&mut system, &mut cache, &font_path, 0)
            .expect("first font load should succeed");
        let faces_after_first = system.db().len();
        assert_eq!(cache.distinct_file_count(), 1, "one distinct file cached");

        let second = load_selected_font_from_path(&mut system, &mut cache, &font_path, 0)
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
    fn leased_system_returns_to_pool() {
        // Lease a system, mark it, and confirm a subsequent checkout can reuse a
        // pooled system (the pool is non-empty after the lease returns).
        let face_count = with_leased_font_system(|system, _cache| {
            // Touch the system so the closure genuinely uses the lease.
            system.db().len()
        });
        assert!(
            face_count >= 1,
            "a leased FontSystem should expose its system font database"
        );
        // After the lease, at least one system should be parked. Check out and
        // return it to confirm reuse works without panicking.
        let pooled = checkout();
        return_to_pool(pooled);
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

        let mut system = cosmic_text::FontSystem::new();
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
}
