/*
File: crates/ms-tab-typing/src/custom_fonts/service.rs

Purpose:
The process-global compile service of custom fonts: the ONE owner of how
`ms_custom_fonts::compile` is called, of the memory LRU of compiled full faces and of the
on-disk cache's use. Callers hand it a recipe plus resolved sources and get a
`CompiledFace`; whether the face came from memory, from `fonts/custom_cache/` or from a fresh
compile is invisible to them.

Key structures:
- `CustomFontService` (+ `service()`): memory LRU (byte budget, the newest face always kept),
  in-flight jobs (ONE job per `RecipeKey`; later callers join it), source-digest memo.
- `CompiledFace`: a full face ready for `FontContent` / egui registration.
- `CompileTicket`: a non-blocking handle (progress, cancel, result) for GUI callers.
- `CustomFontError`: why a face is unavailable (`Display` = log text; the UI maps variants).

Key functions:
- `recipe_key`, `full_face_blocking` (worker threads only), `start_full_compile` (GUI-safe),
  `peek_full` / `request_disk_load` (GUI-safe previews: never compile),
  `subset_preview_blocking` (uncached preview subset, decision D5),
  `prune_disk_cache` / `start_disk_prune_once`, `blend_stats_summary` (the log text of a
  full compile's blend-path counts).

Notes:
- Threading: the GUI thread may call only `start_full_compile`, `peek_full`,
  `request_disk_load`, `start_disk_prune_once` and the ticket's methods; they take the inner
  lock for a map lookup only. Digesting, disk I/O and compiling run on the calling worker or on
  `ms_thread` workers. No lock is held across any of them.
- Cancellation: a job's cancel flag (polled per glyph by the compiler) is raised only when
  EVERY ticket that joined it cancelled and no blocking caller waits on it; blocking callers
  never cancel. A caller that meets an already-cancelled job waits for it to end and starts
  a fresh one.
- An evicted face stays alive while a `FontContent` (or anyone) holds its `Arc`.
- `RecipeKey` (SHA-256) is the only disk key; `font_content_id` keys in-process memos only.
- The production cache directory is decided per disk access (`CacheLocation::Production`),
  never baked into the process-global service, so the test persistence latch is honoured
  even when it is armed after `service()` was first called.
*/

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use ms_custom_fonts::{
    BlendStats, CompileError, CompileScope, CompiledFont, CoverageReport, CustomFontRecipe, KerningPairPerMille, RecipeKey,
    SourceDigest, SourceFont, recipe_key, source_digest,
};
use ms_log::runtime_log;

use super::disk_cache::{self, LoadOutcome, PRUNE_GRACE, PruneReport};
use super::identity::compiled_face_post_script_name;
use super::sources::{ResolvedSource, SourceError, SourceResolver};
use crate::panel::font_settings_store::persistence_suppressed_by_tests;
use crate::render_next::{FontBytes, font_content_id};

/// Memory budget of the compiled-face LRU. A full CJK hybrid is ~50 MB, a Latin one well
/// under 1 MB; the newest face is kept even when it alone exceeds the budget.
pub(crate) const DEFAULT_MEMORY_BUDGET_BYTES: usize = 256 * 1024 * 1024;

/// How often a ticket worker waiting on a job re-checks whether its ticket was cancelled.
const TICKET_WAIT_POLL: Duration = Duration::from_millis(100);

/// The compiler entry point (`ms_custom_fonts::compile` in production; injectable for tests).
pub(crate) type CompileFn = dyn Fn(
        &CustomFontRecipe,
        &[SourceFont<'_>],
        CompileScope<'_>,
        &(dyn Fn(usize, usize) + Sync),
        &AtomicBool,
    ) -> Result<CompiledFont, CompileError>
    + Send
    + Sync;

/// A compiled FULL custom face.
pub(crate) struct CompiledFace {
    /// The TrueType file.
    pub(crate) bytes: FontBytes,
    /// `bytes` length (the LRU's accounting unit).
    pub(crate) byte_len: usize,
    /// `font_content_id(bytes)` (renderer load-cache key, in-process only).
    pub(crate) content_id: u64,
    /// PostScript name inside the face (`MSCustom-<short8>`), e.g. the PSD export font name.
    pub(crate) post_script_name: String,
    /// Family name inside the face (`MS Custom <short8>`).
    pub(crate) family_name: String,
    /// The recipe key this face was compiled for.
    pub(crate) key: RecipeKey,
    /// Coverage of the whole recipe; `None` for a face loaded by `request_disk_load`, which has
    /// no sources to compute it from.
    pub(crate) report: Option<CoverageReport>,
}

impl std::fmt::Debug for CompiledFace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledFace")
            .field("post_script_name", &self.post_script_name)
            .field("byte_len", &self.byte_len)
            .field("key", &self.key.hex())
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl CompiledFace {
    /// Wraps compiled or cached bytes.
    fn new(bytes: Vec<u8>, post_script_name: String, family_name: String, key: RecipeKey, report: Option<CoverageReport>) -> Self {
        let content_id = font_content_id(&bytes);
        let byte_len = bytes.len();
        Self { bytes: Arc::new(bytes), byte_len, content_id, post_script_name, family_name, key, report }
    }
}

/// One-line, log-ready text of a compile's blend-path counts: every path with its count
/// (zero counts included, so lines of different compiles align), the skeleton-fallback
/// reasons when there are any, and the transfer base substitutions.
#[must_use]
pub(crate) fn blend_stats_summary(stats: &BlendStats) -> String {
    let mut text = format!(
        "glyphs={} copied={} compatible={} matched={} sdf={} no_blend={} skeleton={} skeleton_fallback={} transfer={} \
         transfer_base_substituted={}",
        stats.glyphs(),
        stats.copied,
        stats.compatible,
        stats.matched,
        stats.sdf,
        stats.no_blend,
        stats.skeleton,
        stats.skeleton_fallback,
        stats.transfer,
        stats.transfer_base_substituted
    );
    if !stats.skeleton_fallback_reasons.is_empty() {
        let reasons: Vec<String> =
            stats.skeleton_fallback_reasons.iter().map(|(reason, count)| format!("{reason}={count}")).collect();
        text.push_str(&format!(" skeleton_fallback_reasons=[{}]", reasons.join(", ")));
    }
    text
}

/// Why a custom face is not available.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub(crate) enum CustomFontError {
    /// A source font could not be resolved.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// The compiler rejected the recipe or the sources.
    #[error(transparent)]
    Compile(CompileError),
    /// Every interested caller cancelled the compile.
    #[error("custom font compilation was cancelled")]
    Cancelled,
    /// The worker thread could not be started or died before publishing a result.
    #[error("custom font worker failed: {0}")]
    Worker(String),
}

impl From<CompileError> for CustomFontError {
    fn from(err: CompileError) -> Self {
        match err {
            CompileError::Cancelled => Self::Cancelled,
            other => Self::Compile(other),
        }
    }
}

/// Result of a full-face request.
pub(crate) type FaceResult = Result<Arc<CompiledFace>, CustomFontError>;

/// Memory LRU of compiled faces, accounted in bytes.
#[derive(Debug)]
struct MemoryLru {
    entries: HashMap<RecipeKey, LruEntry>,
    total_bytes: usize,
    budget_bytes: usize,
    tick: u64,
}

#[derive(Debug)]
struct LruEntry {
    face: Arc<CompiledFace>,
    last_used: u64,
}

impl MemoryLru {
    fn new(budget_bytes: usize) -> Self {
        Self { entries: HashMap::new(), total_bytes: 0, budget_bytes, tick: 0 }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        self.tick
    }

    /// The face of `key`, marked as most recently used.
    fn get(&mut self, key: &RecipeKey) -> Option<Arc<CompiledFace>> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(key)?;
        entry.last_used = tick;
        Some(Arc::clone(&entry.face))
    }

    fn contains(&self, key: &RecipeKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Inserts (or replaces) a face as the most recently used, then evicts the least recently
    /// used faces until the budget holds — never the face just inserted.
    fn insert(&mut self, face: Arc<CompiledFace>) {
        let tick = self.next_tick();
        let key = face.key;
        self.total_bytes = self.total_bytes.saturating_add(face.byte_len);
        if let Some(previous) = self.entries.insert(key, LruEntry { face, last_used: tick }) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.face.byte_len);
        }
        while self.total_bytes > self.budget_bytes {
            let oldest = self
                .entries
                .iter()
                .filter(|(candidate, _)| **candidate != key)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(candidate, _)| *candidate);
            let Some(oldest) = oldest else { break };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.face.byte_len);
            }
        }
    }

    fn keys(&self) -> impl Iterator<Item = RecipeKey> + '_ {
        self.entries.keys().copied()
    }
}

/// One in-flight full-face production for one key.
#[derive(Debug)]
struct Job {
    key: RecipeKey,
    /// Polled by the compiler; raised only when nobody is interested any more.
    cancel: AtomicBool,
    progress_done: AtomicUsize,
    progress_total: AtomicUsize,
    state: Mutex<JobState>,
    finished: Condvar,
}

#[derive(Debug, Default)]
struct JobState {
    result: Option<FaceResult>,
    /// Tickets that joined and have not cancelled.
    ticket_subscribers: usize,
    /// Blocking callers waiting for the result (they never cancel).
    blocking_waiters: usize,
}

/// Which kind of interest a caller registers on a job.
#[derive(Debug, Clone, Copy)]
enum Interest {
    Ticket,
    Blocking,
}

impl Job {
    fn new(key: RecipeKey, interest: Interest) -> Self {
        let mut state = JobState::default();
        match interest {
            Interest::Ticket => state.ticket_subscribers = 1,
            Interest::Blocking => state.blocking_waiters = 1,
        }
        Self {
            key,
            cancel: AtomicBool::new(false),
            progress_done: AtomicUsize::new(0),
            progress_total: AtomicUsize::new(0),
            state: Mutex::new(state),
            finished: Condvar::new(),
        }
    }

    /// The state; a poisoned lock is recovered because every critical section here only
    /// assigns plain counters / a result, which cannot be left half-written.
    fn lock_state(&self) -> MutexGuard<'_, JobState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers interest unless the job is already being cancelled (`false`: the caller must
    /// wait for it to end and start over).
    fn try_join(&self, interest: Interest) -> bool {
        let mut state = self.lock_state();
        if self.cancel.load(Ordering::Acquire) {
            return false;
        }
        match interest {
            Interest::Ticket => state.ticket_subscribers += 1,
            Interest::Blocking => state.blocking_waiters += 1,
        }
        true
    }

    /// A ticket lost interest; the last one out (with no blocking waiter) cancels the job.
    fn release_ticket(&self) {
        let mut state = self.lock_state();
        state.ticket_subscribers = state.ticket_subscribers.saturating_sub(1);
        if state.ticket_subscribers == 0 && state.blocking_waiters == 0 && state.result.is_none() {
            self.cancel.store(true, Ordering::Release);
        }
    }

    /// Blocks until the job has a result.
    fn wait(&self) -> FaceResult {
        let mut state = self.lock_state();
        loop {
            if let Some(result) = &state.result {
                return result.clone();
            }
            state = self.finished.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Blocks until the job has a result (used to outwait a job that is being cancelled).
    fn wait_finished(&self) {
        let mut state = self.lock_state();
        while state.result.is_none() {
            state = self.finished.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Blocks until the job has a result or `give_up()` holds (`None`). `give_up` runs with
    /// the job's state locked, so it must not take a job lock itself (it may take a ticket's).
    fn wait_unless(&self, give_up: &dyn Fn() -> bool) -> Option<FaceResult> {
        let mut state = self.lock_state();
        loop {
            if let Some(result) = &state.result {
                return Some(result.clone());
            }
            if give_up() {
                return None;
            }
            state = self.finished.wait_timeout(state, TICKET_WAIT_POLL).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Publishes the result (first call wins) and wakes every waiter.
    fn finish(&self, result: FaceResult) {
        let mut state = self.lock_state();
        if state.result.is_none() {
            state.result = Some(result);
        }
        drop(state);
        self.finished.notify_all();
    }

    fn progress(&self) -> Option<(usize, usize)> {
        let total = self.progress_total.load(Ordering::Acquire);
        (total > 0).then(|| (self.progress_done.load(Ordering::Acquire), total))
    }
}

/// In-process memo key of a source digest: `(font_content_id, face_index, byte length)`.
type DigestMemoKey = (u64, usize, usize);

#[derive(Debug)]
struct Inner {
    lru: MemoryLru,
    jobs: HashMap<RecipeKey, Arc<Job>>,
    digests: HashMap<DigestMemoKey, SourceDigest>,
    disk_loads: HashSet<RecipeKey>,
}

/// What a caller does about a key after the inner lookup.
enum Role {
    Ready(Arc<CompiledFace>),
    Lead(Arc<Job>),
    Join(Arc<Job>),
}

/// Where a service keeps its disk cache. Production code only ever builds `Production`; the
/// other locations exist for tests (`CustomFontService::new`), so they are test-only.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CacheLocation {
    /// The production directory, decided AGAIN on every disk access
    /// (`disk_cache::default_cache_dir`): the process-global service may be created before a
    /// dependent crate's test arms the persistence latch, and must not pin the real directory.
    Production,
    /// A fixed directory (a unique temp dir).
    #[cfg(test)]
    Fixed(PathBuf),
    /// No disk cache.
    #[cfg(test)]
    Disabled,
}

/// The compile service. Production code uses the process-global `service()`.
pub(crate) struct CustomFontService {
    compiler: Box<CompileFn>,
    cache: CacheLocation,
    inner: Mutex<Inner>,
    prune_started: AtomicBool,
}

impl std::fmt::Debug for CustomFontService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomFontService").field("cache", &self.cache).finish_non_exhaustive()
    }
}

/// The process-global service: the real compiler, the real disk cache (decided per access, so
/// none while the test persistence latch holds) and the default memory budget.
#[must_use]
pub(crate) fn service() -> &'static Arc<CustomFontService> {
    static SERVICE: OnceLock<Arc<CustomFontService>> = OnceLock::new();
    SERVICE.get_or_init(|| {
        Arc::new(CustomFontService::with_location(Box::new(ms_custom_fonts::compile), CacheLocation::Production, DEFAULT_MEMORY_BUDGET_BYTES))
    })
}

impl CustomFontService {
    /// A service over `compiler`, caching on disk under `cache_dir` (when `Some`) and in memory
    /// up to `memory_budget_bytes`. The test seam: tests pass a fake compiler and a unique temp
    /// directory (or `None`). Test-only: production uses `service()`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(compiler: Box<CompileFn>, cache_dir: Option<PathBuf>, memory_budget_bytes: usize) -> Self {
        let cache = cache_dir.map_or(CacheLocation::Disabled, CacheLocation::Fixed);
        Self::with_location(compiler, cache, memory_budget_bytes)
    }

    fn with_location(compiler: Box<CompileFn>, cache: CacheLocation, memory_budget_bytes: usize) -> Self {
        Self {
            compiler,
            cache,
            inner: Mutex::new(Inner {
                lru: MemoryLru::new(memory_budget_bytes),
                jobs: HashMap::new(),
                digests: HashMap::new(),
                disk_loads: HashSet::new(),
            }),
            prune_started: AtomicBool::new(false),
        }
    }

    /// The disk cache directory for THIS access, or `None` (disabled; or production while the
    /// test persistence latch holds). Callers ask once per operation and never store it.
    /// Production resolution stats the fonts directory: worker threads only (the GUI-side
    /// guards use `cache_enabled`).
    fn cache_dir(&self) -> Option<PathBuf> {
        match &self.cache {
            CacheLocation::Production => disk_cache::default_cache_dir(),
            #[cfg(test)]
            CacheLocation::Fixed(dir) => Some(dir.clone()),
            #[cfg(test)]
            CacheLocation::Disabled => None,
        }
    }

    /// Whether a disk access right now would have a directory (`cache_dir().is_some()`)
    /// without touching the file system: GUI-safe.
    fn cache_enabled(&self) -> bool {
        match &self.cache {
            CacheLocation::Production => !persistence_suppressed_by_tests(),
            #[cfg(test)]
            CacheLocation::Fixed(_) => true,
            #[cfg(test)]
            CacheLocation::Disabled => false,
        }
    }

    /// The inner maps; a poisoned lock is recovered because every critical section only
    /// inserts / removes whole map entries.
    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The key of `recipe` over exactly these sources (recipe order). Hashes every source's
    /// bytes once per process (memoized by content id), so call it on a worker.
    #[must_use]
    pub(crate) fn recipe_key(&self, recipe: &CustomFontRecipe, sources: &[ResolvedSource]) -> RecipeKey {
        let digests: Vec<SourceDigest> = sources.iter().map(|source| self.source_digest(source)).collect();
        let inputs: Vec<(SourceDigest, &[KerningPairPerMille])> =
            digests.iter().zip(sources).map(|(digest, source)| (*digest, &*source.user_kerning)).collect();
        recipe_key(recipe, &inputs)
    }

    /// The memoized SHA-256 digest of one source (computed outside the lock).
    fn source_digest(&self, source: &ResolvedSource) -> SourceDigest {
        let memo_key = (source.content_id, source.face_index, source.bytes().len());
        if let Some(digest) = self.lock_inner().digests.get(&memo_key) {
            return *digest;
        }
        // `face_index` comes from a font collection index, far below `u32::MAX`; a value that
        // does not fit is hashed as `u32::MAX`, and the compiler then rejects that source with
        // a typed error, so no wrong face can be produced under this key.
        let face_index = u32::try_from(source.face_index).unwrap_or(u32::MAX);
        let digest = source_digest(source.bytes(), face_index);
        self.lock_inner().digests.insert(memo_key, digest);
        digest
    }

    /// The full face of `recipe` over `sources`: memory, else disk, else a compile — joining an
    /// in-flight job for the same key instead of compiling twice. Blocks; worker threads only
    /// (render, export, list-build workers). Never cancels.
    ///
    /// # Errors
    /// `CustomFontError::Source` / `Compile` / `Worker` (a leader that died).
    pub(crate) fn full_face_blocking(&self, recipe: &CustomFontRecipe, sources: &[ResolvedSource]) -> FaceResult {
        let key = self.recipe_key(recipe, sources);
        loop {
            match self.role_for(key, Interest::Blocking) {
                Role::Ready(face) => return Ok(face),
                Role::Lead(job) => return self.lead(&job, recipe, sources),
                Role::Join(job) => {
                    if job.try_join(Interest::Blocking) {
                        return job.wait();
                    }
                    // A job being cancelled cannot be revived: wait for it to end, then start over.
                    job.wait_finished();
                }
            }
        }
    }

    /// Looks `key` up: a ready face, an existing job to join, or a new job this caller leads
    /// (registered with `interest`).
    fn role_for(&self, key: RecipeKey, interest: Interest) -> Role {
        let mut inner = self.lock_inner();
        if let Some(face) = inner.lru.get(&key) {
            return Role::Ready(face);
        }
        if let Some(job) = inner.jobs.get(&key) {
            return Role::Join(Arc::clone(job));
        }
        let job = Arc::new(Job::new(key, interest));
        inner.jobs.insert(key, Arc::clone(&job));
        Role::Lead(job)
    }

    /// Produces the face of a job this thread leads, publishes it (memory LRU, job result) and
    /// retires the job — also when production panics (the guard publishes a `Worker` error).
    fn lead(&self, job: &Arc<Job>, recipe: &CustomFontRecipe, sources: &[ResolvedSource]) -> FaceResult {
        let guard = LeadGuard { service: self, job: Arc::clone(job), published: false };
        let result = self.produce(job, recipe, sources);
        guard.publish(result.clone());
        result
    }

    /// Disk cache, else compile (+ disk store). Runs on the leader's thread, no lock held.
    fn produce(&self, job: &Job, recipe: &CustomFontRecipe, sources: &[ResolvedSource]) -> FaceResult {
        let key = job.key;
        let views = sources.iter().map(ResolvedSource::as_source_font).collect::<Result<Vec<_>, _>>()?;
        if let Some(face) = self.load_from_disk(&key, Some((recipe, &views))) {
            return Ok(Arc::new(face));
        }
        let identities = recipe.source_identities().join(", ");
        runtime_log::log_info(format!("typing: compiling custom font {} (sources: {identities})", key.short8()));
        let started = web_time::Instant::now();
        let progress = |done: usize, total: usize| {
            job.progress_done.store(done, Ordering::Release);
            job.progress_total.store(total, Ordering::Release);
        };
        let compiled = (self.compiler)(recipe, &views, CompileScope::Full, &progress, &job.cancel).map_err(|err| {
            if !matches!(err, CompileError::Cancelled) {
                runtime_log::log_error(format!(
                    "typing: custom font {} (sources: {identities}) failed to compile: {err}",
                    key.short8()
                ));
            }
            CustomFontError::from(err)
        })?;
        // The one log of a full face's blend paths: every full compile passes here (editor
        // commit, layer load, export), while a disk-cache hit has no compile to report.
        runtime_log::log_info(format!(
            "typing: compiled custom font {} ({} bytes, {} characters) in {:.2} s; blend paths: {}",
            compiled.postscript_name,
            compiled.bytes.len(),
            compiled.report.total_chars,
            started.elapsed().as_secs_f64(),
            blend_stats_summary(&compiled.stats)
        ));
        if compiled.key != key {
            // Both sides hash the same inputs with the same function; a mismatch means the
            // sources handed to the compiler differ from the ones the key was computed from,
            // and the disk copy would be rejected on every later load.
            runtime_log::log_error(format!(
                "typing: custom font key mismatch: service {} vs compiler {}; the disk cache will not hit for it",
                key.hex(),
                compiled.key.hex()
            ));
        }
        if let Some(dir) = self.cache_dir()
            && let Err(err) = disk_cache::store(&dir, &key, &compiled.bytes)
        {
            runtime_log::log_warn(format!("typing: {err}; the face will be recompiled after a restart"));
        }
        Ok(Arc::new(CompiledFace::new(
            compiled.bytes,
            compiled.postscript_name,
            compiled.family_name,
            key,
            Some(compiled.report),
        )))
    }

    /// A valid cached face of `key`, or `None` (no cache dir, miss, invalid file, I/O error —
    /// the last two logged). With `coverage`, the face's report is computed from the recipe
    /// (cmap lookups only).
    fn load_from_disk(&self, key: &RecipeKey, coverage: Option<(&CustomFontRecipe, &[SourceFont<'_>])>) -> Option<CompiledFace> {
        let dir = self.cache_dir()?;
        let expected = compiled_face_post_script_name(key);
        match disk_cache::load(&dir, key) {
            Ok(LoadOutcome::Hit { bytes, family_name }) => {
                let report = coverage.and_then(|(recipe, views)| match ms_custom_fonts::coverage_report(recipe, views) {
                    Ok(report) => Some(report),
                    Err(err) => {
                        runtime_log::log_warn(format!("typing: coverage of cached custom font {expected} unavailable: {err}"));
                        None
                    }
                });
                Some(CompiledFace::new(bytes, expected, family_name, *key, report))
            }
            Ok(LoadOutcome::Miss) => None,
            Ok(LoadOutcome::Invalid { reason, removal_error }) => {
                let removal = removal_error.map_or_else(|| "deleted".to_owned(), |err| format!("could not delete it: {err}"));
                runtime_log::log_warn(format!(
                    "typing: cached custom font {} rejected ({reason}); {removal}; recompiling",
                    disk_cache::face_path(&dir, key).display()
                ));
                None
            }
            Err(err) => {
                runtime_log::log_warn(format!("typing: {err}; recompiling"));
                None
            }
        }
    }

    /// Starts producing the full face on a worker (resolve sources, key, then memory / disk /
    /// compile, joining an in-flight job). GUI-safe; poll the ticket.
    #[must_use]
    pub(crate) fn start_full_compile(self: &Arc<Self>, recipe: Arc<CustomFontRecipe>, resolver: Arc<dyn SourceResolver>) -> CompileTicket {
        let shared = Arc::new(TicketShared::default());
        let service = Arc::clone(self);
        let worker_shared = Arc::clone(&shared);
        let spawned = ms_thread::Builder::new()
            .name("typing-custom-font-compile".to_owned())
            .spawn(move || service.run_ticket(&worker_shared, &recipe, &*resolver));
        if let Err(err) = spawned {
            runtime_log::log_error(format!("typing: could not start the custom font compile worker: {err}"));
            shared.set_result(Err(CustomFontError::Worker(err.to_string())));
        }
        CompileTicket { shared }
    }

    /// Body of a ticket worker.
    fn run_ticket(&self, shared: &TicketShared, recipe: &CustomFontRecipe, resolver: &dyn SourceResolver) {
        if shared.is_cancelled() {
            return;
        }
        let sources = match recipe
            .source_identities()
            .into_iter()
            .map(|identity| resolver.resolve_source(identity))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(sources) => sources,
            Err(err) => {
                runtime_log::log_warn(format!("typing: custom font sources unavailable: {err}"));
                shared.set_result(Err(err.into()));
                return;
            }
        };
        let key = self.recipe_key(recipe, &sources);
        loop {
            let (job, leads) = match self.role_for(key, Interest::Ticket) {
                Role::Ready(face) => {
                    shared.set_result(Ok(face));
                    return;
                }
                Role::Lead(job) => (job, true),
                Role::Join(job) => {
                    if !job.try_join(Interest::Ticket) {
                        job.wait_finished();
                        continue;
                    }
                    (job, false)
                }
            };
            shared.attach(&job);
            let result = if leads {
                Some(self.lead(&job, recipe, &sources))
            } else {
                job.wait_unless(&|| shared.is_cancelled())
            };
            if let Some(result) = result {
                shared.set_result(result);
            }
            return;
        }
    }

    /// The full face of `key` if it is in memory (GUI-safe; never touches disk or compiles).
    #[must_use]
    pub(crate) fn peek_full(&self, key: &RecipeKey) -> Option<Arc<CompiledFace>> {
        self.lock_inner().lru.get(key)
    }

    /// Loads the cached face of `key` from disk into memory on a worker, if present. Never
    /// compiles; a no-op when the face is in memory, being produced or already being loaded.
    /// GUI-safe (previews call it, then `peek_full` on later frames).
    pub(crate) fn request_disk_load(self: &Arc<Self>, key: RecipeKey) {
        if !self.cache_enabled() {
            return;
        }
        {
            let mut inner = self.lock_inner();
            if inner.lru.contains(&key) || inner.jobs.contains_key(&key) || !inner.disk_loads.insert(key) {
                return;
            }
        }
        let service = Arc::clone(self);
        let spawned = ms_thread::Builder::new().name("typing-custom-font-disk-load".to_owned()).spawn(move || {
            let face = service.load_from_disk(&key, None);
            let mut inner = service.lock_inner();
            if let Some(face) = face {
                inner.lru.insert(Arc::new(face));
            }
            inner.disk_loads.remove(&key);
        });
        if let Err(err) = spawned {
            runtime_log::log_warn(format!("typing: could not start the custom font disk-load worker: {err}"));
            self.lock_inner().disk_loads.remove(&key);
        }
    }

    /// Compiles only `chars` for a live preview (decision D5). Not cached; blocks; worker
    /// threads only. The ONE place a preview subset is compiled.
    ///
    /// # Errors
    /// `CustomFontError::Source` / `Compile` / `Cancelled` (when `cancel` was raised).
    pub(crate) fn subset_preview_blocking(
        &self,
        recipe: &CustomFontRecipe,
        sources: &[ResolvedSource],
        chars: &BTreeSet<char>,
        cancel: &AtomicBool,
    ) -> Result<CompiledFont, CustomFontError> {
        let views = sources.iter().map(ResolvedSource::as_source_font).collect::<Result<Vec<_>, _>>()?;
        (self.compiler)(recipe, &views, CompileScope::Subset(chars), &|_, _| {}, cancel).map_err(CustomFontError::from)
    }

    /// Deletes cached faces that are neither in `referenced` (keys saved fonts use) nor held or
    /// being produced by this process, honouring the 14-day grace window. Blocking file I/O.
    ///
    /// # Errors
    /// The `io::Error` of listing the cache directory.
    pub(crate) fn prune_disk_cache(&self, referenced: &HashSet<RecipeKey>) -> std::io::Result<PruneReport> {
        let Some(dir) = self.cache_dir() else { return Ok(PruneReport::default()) };
        let mut keep = referenced.clone();
        {
            let inner = self.lock_inner();
            keep.extend(inner.lru.keys());
            keep.extend(inner.jobs.keys().copied());
        }
        disk_cache::prune(&dir, &keep, SystemTime::now(), PRUNE_GRACE)
    }

    /// Runs `prune_disk_cache` once per process on a worker (later calls are no-ops). GUI-safe;
    /// the caller passes the keys of every saved custom font it listed.
    pub(crate) fn start_disk_prune_once(self: &Arc<Self>, referenced: HashSet<RecipeKey>) {
        if !self.cache_enabled() || self.prune_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let service = Arc::clone(self);
        let spawned = ms_thread::Builder::new().name("typing-custom-font-prune".to_owned()).spawn(move || {
            match service.prune_disk_cache(&referenced) {
                Ok(report) => {
                    for (path, err) in &report.failures {
                        runtime_log::log_warn(format!("typing: could not prune cached custom font '{}': {err}", path.display()));
                    }
                    if !report.removed.is_empty() {
                        runtime_log::log_info(format!(
                            "typing: pruned {} unused cached custom font file(s), kept {}",
                            report.removed.len(),
                            report.kept
                        ));
                    }
                }
                Err(err) => runtime_log::log_warn(format!("typing: could not list the custom font cache for pruning: {err}")),
            }
        });
        if let Err(err) = spawned {
            runtime_log::log_warn(format!("typing: could not start the custom font prune worker: {err}"));
        }
    }

    /// Test probe: `(ticket subscribers, blocking waiters)` of the in-flight job for `key`.
    #[cfg(test)]
    fn job_interest(&self, key: &RecipeKey) -> Option<(usize, usize)> {
        let job = self.lock_inner().jobs.get(key).cloned()?;
        let state = job.lock_state();
        Some((state.ticket_subscribers, state.blocking_waiters))
    }
}

/// Publishes a led job's result and retires it; on drop without `publish` (a panic during
/// production) it publishes a `Worker` error so no waiter blocks forever.
struct LeadGuard<'a> {
    service: &'a CustomFontService,
    job: Arc<Job>,
    published: bool,
}

impl LeadGuard<'_> {
    fn publish(mut self, result: FaceResult) {
        self.retire(result);
        self.published = true;
    }

    fn retire(&self, result: FaceResult) {
        {
            let mut inner = self.service.lock_inner();
            if let Ok(face) = &result {
                inner.lru.insert(Arc::clone(face));
            }
            inner.jobs.remove(&self.job.key);
        }
        self.job.finish(result);
    }
}

impl Drop for LeadGuard<'_> {
    fn drop(&mut self) {
        if !self.published {
            runtime_log::log_error(format!("typing: custom font {} worker died while compiling", self.job.key.short8()));
            self.retire(Err(CustomFontError::Worker("the compile worker panicked".to_owned())));
        }
    }
}

/// State shared by a `CompileTicket` and its worker.
#[derive(Debug, Default)]
struct TicketShared {
    state: Mutex<TicketState>,
}

#[derive(Debug, Default)]
struct TicketState {
    cancelled: bool,
    /// The job this ticket holds interest in (released exactly once, by `cancel`).
    job: Option<Arc<Job>>,
    result: Option<FaceResult>,
}

impl TicketShared {
    /// The state; a poisoned lock is recovered because every critical section assigns whole
    /// fields only.
    fn lock(&self) -> MutexGuard<'_, TicketState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Records the job this ticket joined; if the ticket was cancelled meanwhile, releases the
    /// interest at once instead.
    fn attach(&self, job: &Arc<Job>) {
        let mut state = self.lock();
        if state.cancelled {
            drop(state);
            job.release_ticket();
        } else {
            state.job = Some(Arc::clone(job));
        }
    }

    /// Stores the result unless one is already there (a cancel stores `Cancelled` first).
    fn set_result(&self, result: FaceResult) {
        let mut state = self.lock();
        if state.result.is_none() {
            state.result = Some(result);
        }
    }
}

/// Non-blocking handle of a `start_full_compile` request. Dropping it detaches WITHOUT
/// cancelling (the face still lands in the caches); call `cancel` to give up.
#[derive(Debug)]
pub(crate) struct CompileTicket {
    shared: Arc<TicketShared>,
}

impl CompileTicket {
    /// `(glyphs done, glyphs total)` of the compile this ticket waits on; `None` before the
    /// compiler reported (resolving sources, disk load, or a memory hit).
    #[must_use]
    pub(crate) fn progress(&self) -> Option<(usize, usize)> {
        self.shared.lock().job.as_ref().and_then(|job| job.progress())
    }

    /// Gives up: the ticket's result becomes `Cancelled` at once, and the compile itself stops
    /// when no other ticket or blocking caller still wants it. Idempotent.
    pub(crate) fn cancel(&self) {
        // Lock order is job -> ticket (a waiting worker's `give_up` reads the ticket under the
        // job lock), so the ticket lock is released before the job's interest is.
        let job = {
            let mut state = self.shared.lock();
            if state.cancelled {
                return;
            }
            state.cancelled = true;
            if state.result.is_none() {
                state.result = Some(Err(CustomFontError::Cancelled));
            }
            state.job.take()
        };
        if let Some(job) = job {
            job.release_ticket();
        }
    }

    /// The result once available.
    #[must_use]
    pub(crate) fn try_result(&self) -> Option<FaceResult> {
        self.shared.lock().result.clone()
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
