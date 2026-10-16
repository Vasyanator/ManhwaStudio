/*
File: crates/ms-tab-typing/src/custom_fonts/service_tests.rs

Purpose:
Tests of the custom-font compile service (`service.rs`): memory LRU accounting, one job per
key, cancellation rules, ticket results, disk-cache use, and one end-to-end compile of real
bundled fonts that proves the cached face's name convention matches the compiler's.

Notes:
Fake compilers are gated by channels / flags, never by timing, so the tests are deterministic.
Disk tests use unique directories under the system temp dir; the real `fonts/custom_cache/`
is never touched (`default_cache_dir()` is `None` in a test process, and the global service
re-evaluates it on every access).
*/

use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::mpsc;
use std::time::Instant;

use ms_custom_fonts::{CustomFontKind, HybridParams, HybridSource, MissingGlyphPolicy};

use super::*;

/// Upper bound for every wait in these tests (a hang fails instead of blocking the suite).
const DEADLINE: Duration = Duration::from_secs(60);

fn recipe(identities: [&str; 2]) -> Arc<CustomFontRecipe> {
    Arc::new(CustomFontRecipe {
        kind: CustomFontKind::Hybrid(HybridParams {
            sources: identities
                .iter()
                .map(|identity| HybridSource { identity: (*identity).to_owned(), weight: 1.0, missing_override: None })
                .collect(),
            missing_default: MissingGlyphPolicy::NoInfluence,
            algorithm: ms_custom_fonts::BlendAlgorithm::Auto,
            property_weights: None,
        }),
    })
}

fn source(identity: &str, bytes: Vec<u8>) -> ResolvedSource {
    let content_id = font_content_id(&bytes);
    ResolvedSource { identity: identity.to_owned(), data: Arc::new(bytes), face_index: 0, content_id, user_kerning: Arc::from(Vec::new()) }
}

fn fake_sources() -> Vec<ResolvedSource> {
    vec![source("Fake-A", b"font a".to_vec()), source("Fake-B", b"font b".to_vec())]
}

/// A resolver over a fixed identity -> source table.
struct TableResolver(HashMap<String, ResolvedSource>);

impl SourceResolver for TableResolver {
    fn resolve_source(&self, identity: &str) -> Result<ResolvedSource, SourceError> {
        self.0.get(identity).cloned().ok_or_else(|| SourceError::Unknown(identity.to_owned()))
    }
}

fn fake_resolver() -> Arc<dyn SourceResolver> {
    Arc::new(TableResolver(fake_sources().into_iter().map(|source| (source.identity.clone(), source)).collect()))
}

/// Boxes a closure as a `CompileFn` (the bound lets the closure's higher-ranked signature be
/// inferred, which a bare `Box::new` cannot do).
fn boxed(
    compiler: impl Fn(&CustomFontRecipe, &[SourceFont<'_>], CompileScope<'_>, &(dyn Fn(usize, usize) + Sync), &AtomicBool) -> Result<CompiledFont, CompileError>
        + Send
        + Sync
        + 'static,
) -> Box<CompileFn> {
    Box::new(compiler)
}

/// A `CompiledFont` as the real compiler would key it, with `len` filler bytes.
fn fake_compiled(recipe: &CustomFontRecipe, sources: &[SourceFont<'_>], len: usize) -> CompiledFont {
    let digests: Vec<(SourceDigest, &[KerningPairPerMille])> =
        sources.iter().map(|source| (source_digest(source.data, source.face_index), source.user_kerning)).collect();
    let key = recipe_key(recipe, &digests);
    CompiledFont {
        bytes: vec![0_u8; len],
        family_name: format!("MS Custom {}", key.short8()),
        postscript_name: compiled_face_post_script_name(&key),
        key,
        report: CoverageReport::default(),
        stats: ms_custom_fonts::BlendStats::default(),
    }
}

/// A compiler that returns at once and counts its calls.
fn counting_compiler(calls: Arc<AtomicUsize>, len: usize) -> Box<CompileFn> {
    boxed(move |recipe, sources, _scope, progress, _cancel| {
        calls.fetch_add(1, Ordering::SeqCst);
        progress(1, 1);
        Ok(fake_compiled(recipe, sources, len))
    })
}

/// A compiler that signals `entered`, then waits until `release` is set or its cancel flag is
/// raised (reporting `Cancelled` and signalling `saw_cancel`).
struct GatedCompiler {
    calls: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    entered: mpsc::Receiver<()>,
    saw_cancel: mpsc::Receiver<()>,
}

fn gated_compiler() -> (Box<CompileFn>, GatedCompiler) {
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered) = mpsc::channel();
    let (cancel_tx, saw_cancel) = mpsc::channel();
    let entered_tx = Mutex::new(entered_tx);
    let cancel_tx = Mutex::new(cancel_tx);
    let (fn_calls, fn_release) = (Arc::clone(&calls), Arc::clone(&release));
    let compiler: Box<CompileFn> = boxed(move |recipe, sources, _scope, progress, cancel| {
        fn_calls.fetch_add(1, Ordering::SeqCst);
        progress(0, 10);
        entered_tx.lock().expect("entered sender").send(()).expect("test still listening");
        let started = Instant::now();
        while !fn_release.load(Ordering::SeqCst) {
            if cancel.load(Ordering::SeqCst) {
                cancel_tx.lock().expect("cancel sender").send(()).expect("test still listening");
                return Err(CompileError::Cancelled);
            }
            assert!(started.elapsed() < DEADLINE, "gated compiler was never released");
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(fake_compiled(recipe, sources, 16))
    });
    (compiler, GatedCompiler { calls, release, entered, saw_cancel })
}

/// Polls `condition` until it holds (fails the test after `DEADLINE`).
fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < DEADLINE, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn ticket_result(ticket: &CompileTicket) -> FaceResult {
    let started = Instant::now();
    loop {
        if let Some(result) = ticket.try_result() {
            return result;
        }
        assert!(started.elapsed() < DEADLINE, "ticket never produced a result");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A ticket result reduced to comparable data (`CompiledFace` is not `PartialEq`).
fn result_key(result: Option<FaceResult>) -> Option<Result<RecipeKey, CustomFontError>> {
    result.map(|result| result.map(|face| face.key))
}

fn face(key_byte: u8, len: usize) -> Arc<CompiledFace> {
    Arc::new(CompiledFace::new(vec![0; len], String::new(), String::new(), RecipeKey([key_byte; 32]), None))
}

#[test]
fn lru_evicts_least_recently_used_by_bytes_and_keeps_the_newest() {
    let mut lru = MemoryLru::new(250);
    lru.insert(face(1, 100));
    lru.insert(face(2, 100));
    // Touch 1 so 2 becomes the least recently used.
    assert!(lru.get(&RecipeKey([1; 32])).is_some());
    lru.insert(face(3, 100));
    assert!(lru.contains(&RecipeKey([1; 32])) && lru.contains(&RecipeKey([3; 32])));
    assert!(!lru.contains(&RecipeKey([2; 32])));
    assert_eq!(lru.total_bytes, 200);

    // A face larger than the whole budget evicts everything else but stays itself.
    lru.insert(face(4, 1000));
    assert_eq!(lru.keys().collect::<Vec<_>>(), vec![RecipeKey([4; 32])]);
    assert_eq!(lru.total_bytes, 1000);

    // Replacing a face re-accounts its bytes.
    lru.insert(face(4, 10));
    assert_eq!(lru.total_bytes, 10);
}

#[test]
fn an_evicted_face_stays_alive_while_held() {
    let service = CustomFontService::new(counting_compiler(Arc::new(AtomicUsize::new(0)), 100), None, 150);
    let first = service.full_face_blocking(&recipe(["Fake-A", "Fake-B"]), &fake_sources()).expect("compiles");
    let other = vec![source("Fake-A", b"font a".to_vec()), source("Fake-B", b"font c".to_vec())];
    let second = service.full_face_blocking(&recipe(["Fake-A", "Fake-B"]), &other).expect("compiles");
    assert_ne!(first.key, second.key);
    assert!(service.peek_full(&first.key).is_none(), "first face was evicted");
    assert_eq!(first.byte_len, 100);
    assert_eq!(service.peek_full(&second.key).map(|face| face.key), Some(second.key));
}

#[test]
fn memory_hit_does_not_recompile() {
    let calls = Arc::new(AtomicUsize::new(0));
    let service = CustomFontService::new(counting_compiler(Arc::clone(&calls), 32), None, DEFAULT_MEMORY_BUDGET_BYTES);
    let recipe = recipe(["Fake-A", "Fake-B"]);
    let first = service.full_face_blocking(&recipe, &fake_sources()).expect("compiles");
    let second = service.full_face_blocking(&recipe, &fake_sources()).expect("hits memory");
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.key, service.recipe_key(&recipe, &fake_sources()));
    assert_eq!(first.post_script_name, compiled_face_post_script_name(&first.key));
}

#[test]
fn concurrent_callers_share_one_job() {
    let (compiler, gate) = gated_compiler();
    let service = Arc::new(CustomFontService::new(compiler, None, DEFAULT_MEMORY_BUDGET_BYTES));
    let recipe = recipe(["Fake-A", "Fake-B"]);
    let key = service.recipe_key(&recipe, &fake_sources());

    let spawn_caller = || {
        let (service, recipe) = (Arc::clone(&service), Arc::clone(&recipe));
        std::thread::spawn(move || service.full_face_blocking(&recipe, &fake_sources()))
    };
    let leader = spawn_caller();
    gate.entered.recv_timeout(DEADLINE).expect("leader started compiling");
    let follower = spawn_caller();
    wait_until("the follower to join", || service.job_interest(&key) == Some((0, 2)));
    gate.release.store(true, Ordering::SeqCst);

    let a = leader.join().expect("leader thread").expect("leader result");
    let b = follower.join().expect("follower thread").expect("follower result");
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
    assert_eq!(service.job_interest(&key), None, "the job was retired");
}

#[test]
fn cancelling_the_only_ticket_cancels_the_compile() {
    let (compiler, gate) = gated_compiler();
    let service = Arc::new(CustomFontService::new(compiler, None, DEFAULT_MEMORY_BUDGET_BYTES));
    let recipe = recipe(["Fake-A", "Fake-B"]);
    let ticket = service.start_full_compile(Arc::clone(&recipe), fake_resolver());
    gate.entered.recv_timeout(DEADLINE).expect("compile started");
    wait_until("progress", || ticket.progress() == Some((0, 10)));

    ticket.cancel();
    assert_eq!(result_key(ticket.try_result()), Some(Err(CustomFontError::Cancelled)));
    gate.saw_cancel.recv_timeout(DEADLINE).expect("the compiler observed the cancel flag");
    let key = service.recipe_key(&recipe, &fake_sources());
    wait_until("the cancelled job to retire", || service.job_interest(&key).is_none());
    assert!(service.peek_full(&key).is_none());

    // A later request starts a fresh compile.
    gate.release.store(true, Ordering::SeqCst);
    let face = service.full_face_blocking(&recipe, &fake_sources()).expect("fresh compile");
    assert_eq!(face.key, key);
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_remaining_subscriber_keeps_the_compile_alive() {
    let (compiler, gate) = gated_compiler();
    let service = Arc::new(CustomFontService::new(compiler, None, DEFAULT_MEMORY_BUDGET_BYTES));
    let recipe = recipe(["Fake-A", "Fake-B"]);
    let key = service.recipe_key(&recipe, &fake_sources());
    let first = service.start_full_compile(Arc::clone(&recipe), fake_resolver());
    gate.entered.recv_timeout(DEADLINE).expect("compile started");
    let second = service.start_full_compile(Arc::clone(&recipe), fake_resolver());
    wait_until("the second ticket to join", || service.job_interest(&key) == Some((2, 0)));

    first.cancel();
    assert_eq!(result_key(first.try_result()), Some(Err(CustomFontError::Cancelled)));
    assert_eq!(service.job_interest(&key), Some((1, 0)));
    assert!(gate.saw_cancel.try_recv().is_err(), "the compile must not be cancelled");

    gate.release.store(true, Ordering::SeqCst);
    let face = ticket_result(&second).expect("second ticket completes");
    assert_eq!(face.key, key);
    assert_eq!(result_key(first.try_result()), Some(Err(CustomFontError::Cancelled)), "a cancelled ticket stays cancelled");
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_blocking_waiter_keeps_the_compile_alive() {
    let (compiler, gate) = gated_compiler();
    let service = Arc::new(CustomFontService::new(compiler, None, DEFAULT_MEMORY_BUDGET_BYTES));
    let recipe = recipe(["Fake-A", "Fake-B"]);
    let key = service.recipe_key(&recipe, &fake_sources());
    let ticket = service.start_full_compile(Arc::clone(&recipe), fake_resolver());
    gate.entered.recv_timeout(DEADLINE).expect("compile started");
    let blocking = {
        let (service, recipe) = (Arc::clone(&service), Arc::clone(&recipe));
        std::thread::spawn(move || service.full_face_blocking(&recipe, &fake_sources()))
    };
    wait_until("the blocking caller to join", || service.job_interest(&key) == Some((1, 1)));
    ticket.cancel();
    gate.release.store(true, Ordering::SeqCst);
    let face = blocking.join().expect("blocking thread").expect("blocking caller gets the face");
    assert_eq!(face.key, key);
    assert!(gate.saw_cancel.try_recv().is_err(), "the compile must not be cancelled");
}

#[test]
fn ticket_reports_source_errors_and_memory_hits() {
    let calls = Arc::new(AtomicUsize::new(0));
    let service = Arc::new(CustomFontService::new(counting_compiler(Arc::clone(&calls), 8), None, DEFAULT_MEMORY_BUDGET_BYTES));
    let missing = service.start_full_compile(recipe(["Fake-A", "Nope"]), fake_resolver());
    assert_eq!(ticket_result(&missing).map(|face| face.key), Err(CustomFontError::Source(SourceError::Unknown("Nope".to_owned()))));

    let recipe = recipe(["Fake-A", "Fake-B"]);
    let compiled = ticket_result(&service.start_full_compile(Arc::clone(&recipe), fake_resolver())).expect("compiles");
    let hit = ticket_result(&service.start_full_compile(recipe, fake_resolver())).expect("memory hit");
    assert!(Arc::ptr_eq(&compiled, &hit));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn compile_errors_reach_every_caller_and_are_not_cached() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let failing: Box<CompileFn> = boxed(move |_recipe, _sources, _scope, _progress, _cancel| {
        counter.fetch_add(1, Ordering::SeqCst);
        Err(CompileError::TooManyGlyphs { count: 70_000 })
    });
    let service = CustomFontService::new(failing, None, DEFAULT_MEMORY_BUDGET_BYTES);
    let recipe = recipe(["Fake-A", "Fake-B"]);
    for _ in 0..2 {
        assert_eq!(
            service.full_face_blocking(&recipe, &fake_sources()).map(|face| face.key),
            Err(CustomFontError::Compile(CompileError::TooManyGlyphs { count: 70_000 }))
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn subset_preview_is_compiled_every_time_and_never_cached() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let compiler: Box<CompileFn> = boxed(move |recipe, sources, scope, _progress, _cancel| {
        counter.fetch_add(1, Ordering::SeqCst);
        assert!(matches!(scope, CompileScope::Subset(chars) if chars.len() == 2));
        Ok(fake_compiled(recipe, sources, 4))
    });
    let service = CustomFontService::new(compiler, None, DEFAULT_MEMORY_BUDGET_BYTES);
    let recipe = recipe(["Fake-A", "Fake-B"]);
    let chars: BTreeSet<char> = ['a', 'b'].into_iter().collect();
    let cancel = AtomicBool::new(false);
    for _ in 0..2 {
        let preview = service.subset_preview_blocking(&recipe, &fake_sources(), &chars, &cancel).expect("preview");
        assert!(service.peek_full(&preview.key).is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// A unique scratch directory under the system temp dir, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ms-custom-service-{tag}-{}-{unique}", std::process::id()));
        if path.exists() {
            std::fs::remove_dir_all(&path).expect("clear stale scratch dir");
        }
        Self(path)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if self.0.exists()
            && let Err(err) = std::fs::remove_dir_all(&self.0)
        {
            eprintln!("could not remove scratch dir {}: {err}", self.0.display());
        }
    }
}

#[test]
fn production_service_never_uses_the_real_cache_in_tests() {
    assert_eq!(service().cache_dir(), None);
    // The production location is never resolved once and stored: every disk access asks
    // `default_cache_dir` again, so a latch armed after the service was created still holds.
    assert_eq!(service().cache, CacheLocation::Production);
}

#[test]
fn an_invalid_cached_file_is_replaced_by_a_recompile() {
    let dir = ScratchDir::new("invalid");
    let calls = Arc::new(AtomicUsize::new(0));
    let recipe = recipe(["Fake-A", "Fake-B"]);
    // The fake compiler's bytes are not a font, so the stored file can never validate.
    let first = CustomFontService::new(counting_compiler(Arc::clone(&calls), 64), Some(dir.0.clone()), DEFAULT_MEMORY_BUDGET_BYTES);
    let face = first.full_face_blocking(&recipe, &fake_sources()).expect("compiles");
    let cached = disk_cache::face_path(&dir.0, &face.key);
    assert!(cached.exists(), "the compiled face was written to the disk cache");

    let second = CustomFontService::new(counting_compiler(Arc::clone(&calls), 64), Some(dir.0.clone()), DEFAULT_MEMORY_BUDGET_BYTES);
    second.full_face_blocking(&recipe, &fake_sources()).expect("recompiles");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "the invalid file was not trusted");
    assert!(cached.exists(), "the recompiled face replaced the rejected file");
}

#[test]
fn prune_keeps_faces_held_in_memory() {
    let dir = ScratchDir::new("prune");
    let service = CustomFontService::new(counting_compiler(Arc::new(AtomicUsize::new(0)), 64), Some(dir.0.clone()), DEFAULT_MEMORY_BUDGET_BYTES);
    let face = service.full_face_blocking(&recipe(["Fake-A", "Fake-B"]), &fake_sources()).expect("compiles");
    let stale = disk_cache::face_path(&dir.0, &RecipeKey([0xee; 32]));
    std::fs::write(&stale, b"x").expect("write stale face");
    let old = SystemTime::now() - PRUNE_GRACE - Duration::from_secs(60);
    for path in [&stale, &disk_cache::face_path(&dir.0, &face.key)] {
        std::fs::File::options().write(true).open(path).and_then(|file| file.set_modified(old)).expect("age file");
    }
    let report = service.prune_disk_cache(&HashSet::new()).expect("prune");
    assert_eq!(report.removed, vec![stale]);
    assert_eq!(report.kept, 1);
}

#[test]
fn background_prune_runs_once_per_service() {
    let dir = ScratchDir::new("prune-once");
    std::fs::create_dir_all(&dir.0).expect("create cache dir");
    let stale = disk_cache::face_path(&dir.0, &RecipeKey([0xdd; 32]));
    std::fs::write(&stale, b"x").expect("write stale face");
    let old = SystemTime::now() - PRUNE_GRACE - Duration::from_secs(60);
    std::fs::File::options().write(true).open(&stale).and_then(|file| file.set_modified(old)).expect("age file");

    let service = Arc::new(CustomFontService::new(counting_compiler(Arc::new(AtomicUsize::new(0)), 8), Some(dir.0.clone()), DEFAULT_MEMORY_BUDGET_BYTES));
    service.start_disk_prune_once(HashSet::new());
    wait_until("the background prune", || !stale.exists());
    assert!(service.prune_started.load(Ordering::SeqCst));

    // Without a cache directory nothing is started (and the once-flag stays unset).
    let no_cache = Arc::new(CustomFontService::new(counting_compiler(Arc::new(AtomicUsize::new(0)), 8), None, DEFAULT_MEMORY_BUDGET_BYTES));
    no_cache.start_disk_prune_once(HashSet::new());
    assert!(!no_cache.prune_started.load(Ordering::SeqCst));
}

/// A bundled `fonts/ui` font, or `None` (logged) when the checkout lacks it.
fn bundled_font(relative: &str) -> Option<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts/ui").join(relative);
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            eprintln!("skipping: fixture font {} unavailable: {err}", path.display());
            None
        }
    }
}

#[test]
fn real_compile_round_trips_through_the_disk_cache() {
    let (Some(regular), Some(bold)) =
        (bundled_font("core/00-NotoSans-Regular.ttf"), bundled_font("bold/00-NotoSans-Bold.ttf"))
    else {
        return;
    };
    let dir = ScratchDir::new("real");
    let sources = vec![source("NotoSans-Regular", regular), source("NotoSans-Bold", bold)];
    let recipe = recipe(["NotoSans-Regular", "NotoSans-Bold"]);

    let compiling = CustomFontService::new(Box::new(ms_custom_fonts::compile), Some(dir.0.clone()), DEFAULT_MEMORY_BUDGET_BYTES);
    let compiled = compiling.full_face_blocking(&recipe, &sources).expect("real compile");
    assert_eq!(compiled.post_script_name, compiled_face_post_script_name(&compiled.key));
    assert!(compiled.report.as_ref().is_some_and(|report| report.total_chars > 0));

    // A fresh process (new service, same directory) must find the face on disk: the file's
    // PostScript name matches the service's expectation, so nothing is recompiled.
    let never: Box<CompileFn> = boxed(|_recipe, _sources, _scope, _progress, _cancel| panic!("must load from disk"));
    let reloading = Arc::new(CustomFontService::new(never, Some(dir.0.clone()), DEFAULT_MEMORY_BUDGET_BYTES));
    reloading.request_disk_load(compiled.key);
    wait_until("the background disk load", || reloading.peek_full(&compiled.key).is_some());
    let background = reloading.peek_full(&compiled.key).expect("loaded");
    assert_eq!(background.report, None, "a key-only load has no sources for a report");
    assert_eq!(background.content_id, compiled.content_id);

    let fresh = Arc::new(CustomFontService::new(
        boxed(|_recipe, _sources, _scope, _progress, _cancel| panic!("must load from disk")),
        Some(dir.0.clone()),
        DEFAULT_MEMORY_BUDGET_BYTES,
    ));
    let loaded = fresh.full_face_blocking(&recipe, &sources).expect("disk hit");
    assert_eq!(loaded.content_id, compiled.content_id);
    assert_eq!((*loaded.bytes).as_ref().len(), loaded.byte_len);
    assert_eq!(loaded.family_name, compiled.family_name);
    assert_eq!(loaded.report, compiled.report);
}

#[test]
fn the_blend_stats_log_line_names_every_path_and_the_fallback_reasons() {
    let mut stats = ms_custom_fonts::BlendStats { copied: 4, skeleton: 2, skeleton_fallback: 3, ..ms_custom_fonts::BlendStats::default() };
    assert_eq!(
        blend_stats_summary(&stats),
        "glyphs=9 copied=4 compatible=0 matched=0 sdf=0 no_blend=0 skeleton=2 skeleton_fallback=3 transfer=0 \
         transfer_base_substituted=0"
    );
    stats.skeleton_fallback_reasons.insert("no skeleton", 1);
    stats.skeleton_fallback_reasons.insert("too many strokes", 2);
    assert!(
        blend_stats_summary(&stats).ends_with(" skeleton_fallback_reasons=[no skeleton=1, too many strokes=2]"),
        "{}",
        blend_stats_summary(&stats)
    );
}
