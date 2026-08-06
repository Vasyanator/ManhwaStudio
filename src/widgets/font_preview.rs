/*
FILE HEADER (widgets/font_preview.rs)

Purpose:
Shared egui font-registration helpers for drawing a font's own-typeface preview.
A font row that renders its name in the font itself must register that font file as
an egui font family first; this module owns the deterministic naming, the bound-check,
the OFF-THREAD read of the font file, and the GUI-thread registration used by every
such preview site.

Key functions:
- `combo_font_family_name`: deterministic egui family name for a
  `(font identity, content hash, face_index)`.
- `is_font_family_bound`: whether an egui family is already registered in a context.
- `request_font_family`: the whole preview lifecycle in one call — bound / loading /
  unavailable.

Key structures:
- `PreviewFontFamily`: the three states a preview font can be in for one frame.
- `PreviewFontLoader` (private): the process-global queue of preview font reads.

Notes:
A font is IDENTIFIED by its identity string (the typing tab's `FontEntry` identity: the
representative face's PostScript name); the file path is only the BYTE SOURCE handed to
the loader for the one-time registration. Keying the family on the path would merge two
distinct list entries that share a file — the bundled `fonts/ui` entry and a user import
of the same file — into one registration
(`dev-docs/font_identity_postscript_plan.md`).

The family name additionally carries the font's CONTENT HASH
(`FontEntry::content_hash`), because this registration hands egui BYTES and egui's
`add_font` never re-reads them: an identity alone would keep serving the bytes of a
file that has since been replaced (an updated font shipped under the same PostScript
name, or another UI catalog offering that identity from a different file), so the UI
would draw one typeface while the renderer drew another.

THE FILE IS NEVER READ ON THE GUI THREAD (`CLAUDE.md` §5). `request_font_family` hands
the read to a process-global loader with a bounded number of worker threads and returns
`Pending`; the caller draws in the default UI font for those frames and a repaint is
requested, so the preview appears by itself. Only `Context::add_font` — which needs the
`Context` and therefore the GUI thread — runs here, and egui applies it at the start of
the NEXT pass, so even a successful registration is `Pending` for one more frame. This
mirrors the on-canvas editor font (`tab/create_upload.rs`:
`request_editor_font` + `poll_editor_font_request`).

egui's `add_font` is ADD-ONLY (no eviction), so callers that scroll large font catalogs
must still bound how many distinct families they register (see the settings font-import
picker and the character table's per-frame budget).
Used by the typing create/edit panels, the character table and the settings font UI;
not domain-specific to any of them.
*/

use crate::runtime_log;
use eframe::egui;
use ms_thread as thread;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// How many preview font files may be waiting for their bytes (being read, or read and
/// not yet registered) at once.
///
/// It bounds the loader's memory: every waiting entry holds one font file's bytes.
/// Scrolling a catalog of thousands of fonts therefore cannot queue thousands of reads.
/// Reaching the cap EVICTS the oldest loaded-but-unclaimed entry rather than refusing the
/// new request (see `LoaderState::ready_order`); with every slot still being read the
/// request is answered `Pending` and re-issued next frame, because every call site
/// re-asks per frame while the row is visible.
const MAX_PENDING_PREVIEW_LOADS: usize = 32;

/// How many worker threads may read preview font files concurrently.
///
/// Two is enough to keep the queue moving without turning a fast scroll into a thread
/// storm: the work is I/O bound and the results are consumed one GUI frame at a time.
const MAX_PREVIEW_READER_THREADS: usize = 2;

/// The state of one preview font for the CURRENT frame.
///
/// `Pending` is not an error: the bytes are being read off the GUI thread, or they have
/// just been handed to egui and bind at the start of the next pass. The caller draws in
/// the default UI font meanwhile; a repaint has already been requested for it.
#[derive(Debug, Clone)]
pub enum PreviewFontFamily {
    /// Registered and usable in this frame.
    Ready(egui::FontFamily),
    /// Not usable yet — see the type comment. Try again next frame.
    Pending,
    /// The font cannot be previewed (its file could not be read, or egui refused the
    /// data). Reported once in the log and never retried for these bytes; a font whose
    /// FILE is replaced gets a new family name and is therefore tried again.
    Unavailable,
}

/// `Context::data` key of the preview families THIS context has handed to `add_font`.
///
/// Per-CONTEXT on purpose: registrations belong to one `egui::Context`, and this process
/// creates several sequentially (the launcher and the studio are separate
/// `eframe::run_native` calls). A process-global "already registered" memo would make
/// every font of the previous context look permanently unbindable in the next one.
const REGISTERED_FAMILIES_DATA_KEY: &str = "widgets.font_preview.registered_families";

/// `Context::data` key of the preview families this context handed to `add_font` and
/// which egui did not bind afterwards — i.e. data it refused. Per-context for the same
/// reason as [`REGISTERED_FAMILIES_DATA_KEY`].
const FAILED_FAMILIES_DATA_KEY: &str = "widgets.font_preview.failed_families";

/// What the loader is doing with one preview font, keyed by its egui family name.
///
/// There is no "registered" state here: once the bytes have been handed out the loader
/// forgets the entry, and whether they reached egui is recorded per `Context` (above).
#[derive(Debug)]
enum PreviewLoad {
    /// Queued or being read by a worker thread.
    InFlight,
    /// Read; waiting for a GUI-thread call to hand it to egui.
    Ready(Vec<u8>),
    /// The file could not be read. Process-global: a file that cannot be read cannot be
    /// read for any context either, and a font whose FILE is replaced gets a new family
    /// name and is therefore tried again.
    Failed,
}

/// The process-global preview-font read queue.
///
/// One instance per process, because egui font families are registered per `Context`
/// but named deterministically and globally: two panels sharing a `Context` must share
/// one registration (see [`combo_font_family_name`]).
#[derive(Debug)]
struct PreviewFontLoader {
    state: Mutex<LoaderState>,
    /// Set once a poisoned loader mutex has been reported, so the recovery is logged
    /// once per process instead of on every frame.
    poison_reported: AtomicBool,
}

/// Everything the loader mutates, behind one lock.
#[derive(Debug, Default)]
struct LoaderState {
    /// egui family name -> what is happening to that font.
    loads: HashMap<String, PreviewLoad>,
    /// Not-yet-read jobs, in request order: `(family name, font file path)`.
    queue: VecDeque<(String, PathBuf)>,
    /// Names of the `Ready` entries, oldest first.
    ///
    /// It exists so the cap can EVICT rather than refuse: a row scrolled out of view
    /// before its bytes arrived stops asking for them, and its `Ready` entry would
    /// otherwise hold a slot (and a font file's bytes) forever — a fast scroll through a
    /// large catalog would fill every slot with entries nobody will ever consume and
    /// previews would stop working for the rest of the session. An evicted entry is
    /// simply forgotten and re-read if it is ever asked for again.
    ready_order: VecDeque<String>,
    /// Entries currently in `InFlight` or `Ready`, i.e. the ones holding (or about to
    /// hold) a file's bytes. Kept as a counter so the cap check does not scan a map
    /// that also holds every already-registered font.
    pending: usize,
    /// Worker threads alive right now.
    active_readers: usize,
}

/// The process-global loader, created on first use.
fn loader() -> &'static PreviewFontLoader {
    static LOADER: OnceLock<PreviewFontLoader> = OnceLock::new();
    LOADER.get_or_init(|| PreviewFontLoader {
        state: Mutex::new(LoaderState::default()),
        poison_reported: AtomicBool::new(false),
    })
}

/// Locks the loader, recovering a poisoned mutex and reporting it ONCE per process.
///
/// The guarded sections only look up and insert into a map and a queue, so a panic
/// elsewhere cannot leave one half-updated. Abandoning the state instead would strand
/// every in-flight read and leave the previews permanently on the default font.
fn lock_loader(loader: &'static PreviewFontLoader) -> MutexGuard<'static, LoaderState> {
    match loader.state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            if !loader.poison_reported.swap(true, Ordering::Relaxed) {
                runtime_log::log_warn(
                    "widgets::font_preview: the preview-font loader mutex is poisoned (a \
                     thread panicked while holding it); the queue is recovered and font \
                     previews continue. Reported once per process.",
                );
            }
            poisoned.into_inner()
        }
    }
}

/// Test-only journal of the threads preview font reads ran on.
///
/// It is what pins the contract that a read never happens on the calling (GUI) thread:
/// a test can assert that its own thread id never appears here.
#[cfg(test)]
fn read_thread_journal() -> &'static Mutex<Vec<std::thread::ThreadId>> {
    static JOURNAL: OnceLock<Mutex<Vec<std::thread::ThreadId>>> = OnceLock::new();
    JOURNAL.get_or_init(|| Mutex::new(Vec::new()))
}

/// Records that a preview font file was read on the current thread (test builds only).
#[cfg(test)]
fn record_read_thread() {
    if let Ok(mut journal) = read_thread_journal().lock() {
        journal.push(std::thread::current().id());
    }
}

/// Deterministic egui family name for a UI font preview of
/// `(font_identity, content_hash, face_index)`.
///
/// Depends ONLY on those three values, so the same font always registers under the same
/// name (safe to share across panels that share one egui `Context`) and different fonts
/// get different names — including two entries that share a FILE but not an identity.
/// Sequential numbering would collide across independent panels (egui stores font data
/// by name), so a later registration would overwrite an earlier one and a panel would
/// draw the wrong font.
///
/// `content_hash` is `FontEntry::content_hash` (the first 8 bytes of the file's SHA-256)
/// and is what makes a binding INVALIDATE when the bytes behind one identity change: the
/// registered `egui::FontData` is a snapshot egui never refreshes. `0` is the documented
/// "content unknown" sentinel (the synthetic bundled `fonts/ui` entry, whose bytes are
/// `'static` and cannot change; a file that could not be read at load time, which cannot
/// be registered at all). Entries carrying `0` therefore share one family per
/// `(identity, face_index)`, exactly as before this discriminant existed — which is why
/// the system-font PICKER catalog resolves a real content hash for every identity two of
/// its files contest (`fonts::load_system_fonts`).
#[must_use]
pub fn combo_font_family_name(font_identity: &str, content_hash: u64, face_index: usize) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    font_identity.hash(&mut hasher);
    content_hash.hash(&mut hasher);
    face_index.hash(&mut hasher);
    format!("typing-panel-combo-font-{:016x}", hasher.finish())
}

/// Whether `family` is already registered in `ctx`'s font definitions.
///
/// Cheap and side-effect free: it is how a caller decides whether a preview costs
/// anything this frame. Must be called inside a frame — `Context::fonts` panics before
/// the first pass.
#[must_use]
pub fn is_font_family_bound(ctx: &egui::Context, family: &egui::FontFamily) -> bool {
    ctx.fonts(|fonts| fonts.definitions().families.contains_key(family))
}

/// Asks for the font identified by `font_identity` (bytes hashed to `content_hash`,
/// representative `face_index`) as an egui font family, WITHOUT reading any file on the
/// calling thread.
///
/// `font_identity` names the font and `content_hash` pins WHICH bytes that name stood
/// for; together they are what the family name is derived from. `font_path` is only
/// where the bytes are read from on first use and is NOT part of the key, so moving a
/// font file reuses the existing binding while replacing its CONTENT produces a new one.
///
/// Returns [`PreviewFontFamily::Ready`] as soon as the family is bound — a font already
/// registered elsewhere costs nothing. Otherwise the file read is queued on the loader's
/// worker threads and the result is `Pending`; a repaint is requested, so the caller
/// only has to draw its fallback and be redrawn. The first frame after the bytes arrive
/// hands them to `Context::add_font` (which needs the GUI thread) and is STILL `Pending`,
/// because egui applies new font definitions at the start of the next pass.
/// [`PreviewFontFamily::Unavailable`] means the font will not be previewed at all; it is
/// logged once and never retried for these bytes.
///
/// Call from the GUI thread inside a frame (it reads `ctx.fonts`).
#[must_use]
pub fn request_font_family(
    ctx: &egui::Context,
    font_identity: &str,
    content_hash: u64,
    font_path: &Path,
    face_index: usize,
) -> PreviewFontFamily {
    let font_name = combo_font_family_name(font_identity, content_hash, face_index);
    let family = egui::FontFamily::Name(font_name.clone().into());
    if is_font_family_bound(ctx, &family) {
        return PreviewFontFamily::Ready(family);
    }

    if family_is_in(ctx, FAILED_FAMILIES_DATA_KEY, &font_name) {
        return PreviewFontFamily::Unavailable;
    }
    if family_is_in(ctx, REGISTERED_FAMILIES_DATA_KEY, &font_name) {
        // These bytes were handed to THIS context on an earlier pass and it still does
        // not offer the family, so it refused them (a file that parses as nothing
        // usable). Report once and stop retrying: the caller falls back for good.
        add_family_to(ctx, FAILED_FAMILIES_DATA_KEY, font_name);
        runtime_log::log_warn(format!(
            "widgets::font_preview: egui did not bind the preview font family after \
             registration; the name is shown in the interface font instead. \
             Font: '{font_identity}' Path: '{}' Face index: {face_index}. \
             Likely cause: the file is not a font egui can parse.",
            font_path.display()
        ));
        return PreviewFontFamily::Unavailable;
    }

    match take_or_request_bytes(&font_name, font_path) {
        PreviewBytes::Pending => {
            // The bytes arrive on a worker thread, which schedules no frame of its own.
            ctx.request_repaint();
            PreviewFontFamily::Pending
        }
        PreviewBytes::Unavailable => PreviewFontFamily::Unavailable,
        PreviewBytes::Ready(bytes) => {
            let mut font_data = egui::FontData::from_owned(bytes);
            // A face index that does not fit `u32` cannot exist in a font file; falling
            // back to face 0 keeps the preview useful instead of refusing to draw.
            font_data.index = u32::try_from(face_index).unwrap_or(0);
            ctx.add_font(egui::epaint::text::FontInsert::new(
                font_name.as_str(),
                font_data,
                vec![egui::epaint::text::InsertFontFamily {
                    family: family.clone(),
                    priority: egui::epaint::text::FontPriority::Highest,
                }],
            ));
            add_family_to(ctx, REGISTERED_FAMILIES_DATA_KEY, font_name);
            // New definitions take effect at the START of the next pass, so `family` is
            // not bound in this one; ask for that pass explicitly.
            ctx.request_repaint();
            PreviewFontFamily::Pending
        }
    }
}

/// Whether `ctx`'s set under `key` holds `font_name`.
fn family_is_in(ctx: &egui::Context, key: &'static str, font_name: &str) -> bool {
    ctx.data(|data| data.get_temp::<HashSet<String>>(egui::Id::new(key)))
        .is_some_and(|families| families.contains(font_name))
}

/// Adds `font_name` to `ctx`'s set under `key`.
fn add_family_to(ctx: &egui::Context, key: &'static str, font_name: String) {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<HashSet<String>>(egui::Id::new(key))
            .insert(font_name);
    });
}

/// What the loader can hand back for one family name.
///
/// Deliberately not `Debug`: `Ready` carries a whole font file, which must never end up
/// in a log line (`CLAUDE.md` §8).
enum PreviewBytes {
    /// Bytes are here; the caller registers them and the loader has forgotten the entry
    /// (whether they reached egui is `Context` state, not loader state).
    Ready(Vec<u8>),
    /// Being read, or the loader is at its cap and the request must be re-issued.
    Pending,
    /// The file is known to be unreadable.
    Unavailable,
}

/// Consumes the loaded bytes of `font_name`, or queues the read of `font_path`.
///
/// Taking the bytes REMOVES the entry in the same locked step, so two call sites drawing
/// the same font in one frame cannot both get them (the second is answered `Pending` and
/// finds the family bound on the next pass).
fn take_or_request_bytes(font_name: &str, font_path: &Path) -> PreviewBytes {
    let loader = loader();
    let mut guard = lock_loader(loader);
    let state = &mut *guard;
    match state.loads.get(font_name) {
        Some(PreviewLoad::Failed) => return PreviewBytes::Unavailable,
        Some(PreviewLoad::InFlight) => return PreviewBytes::Pending,
        Some(PreviewLoad::Ready(_)) => {
            let taken = state.loads.remove(font_name);
            state.pending = state.pending.saturating_sub(1);
            state.ready_order.retain(|name| name != font_name);
            return match taken {
                Some(PreviewLoad::Ready(bytes)) => PreviewBytes::Ready(bytes),
                // Unreachable: `Ready` was observed under this very lock. Answered
                // `Pending` (ask again) rather than panicking on an impossible state.
                Some(PreviewLoad::InFlight | PreviewLoad::Failed) | None => PreviewBytes::Pending,
            };
        }
        None => {}
    }
    while state.pending >= MAX_PENDING_PREVIEW_LOADS {
        // Full. Drop the OLDEST loaded-but-unclaimed entry to make room; see
        // `ready_order` for why refusing instead would wedge the loader. With nothing
        // claimable (every slot is still being read) answer `Pending` — the caller asks
        // again next frame.
        let Some(stale) = state.ready_order.pop_front() else {
            return PreviewBytes::Pending;
        };
        state.loads.remove(&stale);
        state.pending = state.pending.saturating_sub(1);
    }
    state.loads.insert(font_name.to_string(), PreviewLoad::InFlight);
    state.pending += 1;
    state
        .queue
        .push_back((font_name.to_string(), font_path.to_path_buf()));
    spawn_readers_if_needed(state);
    PreviewBytes::Pending
}

/// Starts reader threads until the queue is covered or the thread cap is reached.
///
/// Called with the loader lock held. A worker that cannot be spawned is not silently
/// dropped: when no worker is alive at all, the whole queue is failed and logged, so a
/// preview never waits forever for a thread that does not exist.
fn spawn_readers_if_needed(state: &mut LoaderState) {
    while state.active_readers < MAX_PREVIEW_READER_THREADS
        && state.active_readers < state.queue.len()
    {
        match thread::Builder::new()
            .name("font-preview-read".to_string())
            .spawn(read_queued_fonts)
        {
            Ok(_handle) => state.active_readers += 1,
            Err(error) => {
                runtime_log::log_error(format!(
                    "widgets::font_preview: cannot spawn the preview-font reader thread; \
                     font names are shown in the interface font. Error: {error}"
                ));
                if state.active_readers == 0 {
                    // Nothing will ever drain the queue — fail the jobs instead of
                    // leaving them `InFlight` forever.
                    while let Some((font_name, _)) = state.queue.pop_front() {
                        state.loads.insert(font_name, PreviewLoad::Failed);
                        state.pending = state.pending.saturating_sub(1);
                    }
                }
                return;
            }
        }
    }
}

/// Reader-thread body: drains the loader queue, reading one font file at a time, and
/// exits when the queue is empty.
///
/// The file read happens with the lock RELEASED, so a slow or network-backed font file
/// blocks neither the GUI thread nor the other reader.
fn read_queued_fonts() {
    let loader = loader();
    loop {
        let job = {
            let mut guard = lock_loader(loader);
            match guard.queue.pop_front() {
                Some(job) => job,
                None => {
                    guard.active_readers = guard.active_readers.saturating_sub(1);
                    return;
                }
            }
        };
        let (font_name, path) = job;
        let result = std::fs::read(&path);
        #[cfg(test)]
        record_read_thread();
        let mut guard = lock_loader(loader);
        match result {
            Ok(bytes) => {
                guard
                    .loads
                    .insert(font_name.clone(), PreviewLoad::Ready(bytes));
                guard.ready_order.push_back(font_name);
            }
            Err(error) => {
                guard.loads.insert(font_name, PreviewLoad::Failed);
                guard.pending = guard.pending.saturating_sub(1);
                runtime_log::log_warn(format!(
                    "widgets::font_preview: cannot read a font file for its own-typeface \
                     preview; the name is shown in the interface font instead. \
                     Path: '{}' Error: {error}. Reported once per font.",
                    path.display()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Serializes the tests that exercise the loader.
    ///
    /// The loader is PROCESS-GLOBAL and its cap evicts across families, so a test that
    /// deliberately fills it would otherwise evict another test's bytes mid-run. Same
    /// precedent as the process-global font-settings store.
    fn loader_test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        match LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(guard) => guard,
            // A failing test panics while holding it; recovering keeps the OTHER tests
            // reporting their own failures instead of a cascade of poison panics.
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Waits until `font_name` leaves the `InFlight` state, or the deadline passes.
    ///
    /// Returns whether the loader settled; the tests assert on the settled state, so a
    /// timeout fails them with a clear message instead of hanging.
    fn wait_until_settled(font_name: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            {
                let guard = lock_loader(loader());
                if !matches!(guard.loads.get(font_name), Some(PreviewLoad::InFlight)) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// A unique font identity per test, so the PROCESS-GLOBAL loader cannot leak state
    /// between tests running in the same binary.
    fn unique_identity(tag: &str) -> String {
        format!("font-preview-test-{tag}")
    }

    /// A unique family name per test (see [`unique_identity`]).
    fn unique_family_name(tag: &str) -> String {
        combo_font_family_name(&unique_identity(tag), 0xdead_beef, 0)
    }

    /// Creates a readable fixture file for `tag` and returns `(its directory, its path)`.
    ///
    /// The content is NOT a font: nothing in these tests hands it to egui, and the
    /// loader's contract is about reading bytes, not about parsing them.
    fn fixture_file(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ms-font-preview-test-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("the test temp directory must be creatable");
        let path = dir.join("preview.bin");
        std::fs::write(&path, b"not a real font, but readable bytes")
            .expect("the fixture font file must be writable");
        (dir, path)
    }

    /// Whether a preview font file has been read on the CURRENT thread at any point.
    fn read_on_this_thread() -> bool {
        let this_thread = std::thread::current().id();
        read_thread_journal()
            .lock()
            .expect("no test thread panics while holding the journal")
            .contains(&this_thread)
    }

    /// THE CONTRACT OF THIS MODULE, at its public entry point: asking for a preview font
    /// must not read the file on the GUI thread (`CLAUDE.md` §5). The old inline
    /// `fs::read` in this function is exactly what this test refuses.
    #[test]
    fn request_font_family_does_not_read_on_the_gui_thread() {
        let _serialized = loader_test_lock();
        let (dir, path) = fixture_file("gui-thread");
        let identity = unique_identity("gui-thread");

        // `Context::fonts` (and therefore the bound-check) is only valid inside a pass.
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput::default());
        let outcome = request_font_family(&ctx, &identity, 0xdead_beef, &path, 0);
        let _output = ctx.end_pass();

        assert!(
            matches!(outcome, PreviewFontFamily::Pending),
            "an unbound preview font must be reported as pending, not read inline"
        );
        assert!(
            !read_on_this_thread(),
            "the font file must never be read on the GUI thread"
        );

        // The queued read still has to happen — off this thread.
        let family = combo_font_family_name(&identity, 0xdead_beef, 0);
        assert!(wait_until_settled(&family), "the loader must settle");
        assert!(
            matches!(
                lock_loader(loader()).loads.get(&family),
                Some(PreviewLoad::Ready(_))
            ),
            "the worker thread must have produced the bytes"
        );
        assert!(
            !read_on_this_thread(),
            "the font file must never be read on the GUI thread"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same contract at the loader level, plus the bytes actually arriving.
    #[test]
    fn requesting_a_preview_font_does_not_read_on_the_calling_thread() {
        let _serialized = loader_test_lock();
        let (dir, path) = fixture_file("off-thread");
        let family = unique_family_name("off-thread");

        // The very first request must NOT read anything here: it queues the read.
        let first = take_or_request_bytes(&family, &path);
        assert!(
            matches!(first, PreviewBytes::Pending),
            "the first request must be answered without reading the file"
        );
        assert!(
            !read_on_this_thread(),
            "the font file must never be read on the calling thread"
        );

        assert!(wait_until_settled(&family), "the loader must settle");
        match take_or_request_bytes(&family, &path) {
            PreviewBytes::Ready(bytes) => assert_eq!(
                bytes, b"not a real font, but readable bytes",
                "the worker must hand back the file's bytes"
            ),
            PreviewBytes::Pending => panic!("the queued read must have produced bytes"),
            PreviewBytes::Unavailable => panic!("the fixture file is readable"),
        }
        assert!(
            !read_on_this_thread(),
            "the font file must never be read on the calling thread"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that cannot be read fails ONCE and is never queued again, so an unreadable
    /// font does not cost a read per frame for the rest of the session.
    #[test]
    fn an_unreadable_preview_font_fails_permanently() {
        let _serialized = loader_test_lock();
        let family = unique_family_name("missing");
        let path = Path::new("/definitely/not/a/font/dir/preview-ghost.ttf");

        assert!(matches!(
            take_or_request_bytes(&family, path),
            PreviewBytes::Pending
        ));
        assert!(wait_until_settled(&family), "the loader must settle");
        assert!(
            matches!(
                take_or_request_bytes(&family, path),
                PreviewBytes::Unavailable
            ),
            "an unreadable file must be reported as unavailable, not retried"
        );
    }

    /// Nothing in the PROCESS-GLOBAL loader remembers that a family was registered: that
    /// fact belongs to one `egui::Context`, and this process creates several sequentially
    /// (the launcher and the studio are separate `eframe::run_native` calls). A global
    /// memo would make every font of the previous context look permanently unbindable in
    /// the next one — the whole settings font list would fall back to the interface font.
    #[test]
    fn taking_the_bytes_leaves_no_process_wide_registration_memo() {
        let _serialized = loader_test_lock();
        let (dir, path) = fixture_file("two-contexts");
        let family = unique_family_name("two-contexts");

        assert!(matches!(
            take_or_request_bytes(&family, &path),
            PreviewBytes::Pending
        ));
        assert!(wait_until_settled(&family), "the loader must settle");
        assert!(matches!(
            take_or_request_bytes(&family, &path),
            PreviewBytes::Ready(_)
        ));

        assert!(
            !lock_loader(loader()).loads.contains_key(&family),
            "the loader must forget a consumed entry"
        );
        assert!(
            matches!(
                take_or_request_bytes(&family, &path),
                PreviewBytes::Pending
            ),
            "a context that has not registered this family yet must get the bytes again"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A row scrolled out of view before its bytes arrived stops asking for them. The cap
    /// must therefore EVICT the oldest unclaimed entry rather than refuse new requests:
    /// refusing wedged the loader for the rest of the session once enough unclaimed
    /// entries had piled up, and every later preview fell back to the interface font.
    #[test]
    fn the_pending_cap_evicts_unclaimed_bytes_instead_of_wedging() {
        let _serialized = loader_test_lock();
        let (dir, path) = fixture_file("cap");

        // Twice the cap worth of requests, none of them ever claimed.
        for idx in 0..(MAX_PENDING_PREVIEW_LOADS * 2) {
            let family = combo_font_family_name(&format!("font-preview-test-cap-{idx}"), 1, 0);
            assert!(
                matches!(take_or_request_bytes(&family, &path), PreviewBytes::Pending),
                "a first request is always queued"
            );
            assert!(wait_until_settled(&family), "the loader must settle");
        }

        // A fresh request must still go through and still deliver its bytes.
        let family = unique_family_name("cap-after");
        assert!(matches!(
            take_or_request_bytes(&family, &path),
            PreviewBytes::Pending
        ));
        assert!(wait_until_settled(&family), "the loader must settle");
        assert!(
            matches!(
                take_or_request_bytes(&family, &path),
                PreviewBytes::Ready(_)
            ),
            "the loader must still serve requests after the cap has been reached"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two entries that differ ONLY in their content hash must never share one
    /// registration: egui stores font data by name and never re-reads it.
    #[test]
    fn the_content_hash_separates_two_files_claiming_one_identity() {
        assert_ne!(
            combo_font_family_name("Shared-Regular", 0x11, 0),
            combo_font_family_name("Shared-Regular", 0x22, 0),
            "two different files claiming one identity must get different families"
        );
    }
}
