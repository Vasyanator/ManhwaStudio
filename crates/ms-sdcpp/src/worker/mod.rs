/*
File: crates/ms-sdcpp/src/worker/mod.rs

Purpose:
The sd.cpp worker process: `<app exe> --sdcpp-worker` runs `run_worker_main`, which loads the
library of an installed package and serves requests from the app over stdin / stdout frames
(`protocol`). Running the library in its own process means a ggml abort (a `GGML_ASSERT`, an
OOM `abort()`) kills only this process; the app sees a crashed worker, never a crashed editor.

Key items:
- run_worker_main()        : the process entry (stdout redirection, parent-death signal, serve)
- serve()                  : the request loop over any reader / writer and any `SdBackend`
- SdBackend / Canceller    : the library seam (real: `session::RealBackend`; fakes in tests)
- EventSink                : the shared frame writer (main loop + library callbacks)

Notes:
- Threads: the main thread runs the backend; a reader thread decodes requests, answers
  `Cancel` at once through the `Canceller` (the main thread is busy generating), and turns
  `Shutdown` / EOF of stdin into a cancel + exit request. The worker never opens the session
  log (`runtime_log` lines are dropped); diagnostics go to stderr, which the app drains and logs.
- A `Generate` is followed by exactly its announced `Image` frames; anything else is a
  protocol error. The output size is checked against the request before the result is sent.
*/

pub(crate) mod session;
mod stdio;

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};

use crate::catalog::SdCppBuild;
use crate::error::SdCppError;
use crate::protocol::{self, Event, GenerateMessage, Request, WORKER_PROTOCOL, WireError};
use crate::request::{ContextOptions, GenerateRequest, ModelFiles, ProbeReport, RgbImage};

/// Exit code of a worker whose frame channel broke (the app is gone).
const EXIT_PIPE_BROKEN: i32 = 3;
/// Exit code of a worker that could not set up its stdio.
const EXIT_SETUP_FAILED: i32 = 4;
/// Exit code of a worker whose app died before it started serving.
const EXIT_PARENT_GONE: i32 = 5;

/// The shared, serialized frame writer of the worker.
#[derive(Clone)]
pub(crate) struct EventSink(Arc<Mutex<Box<dyn Write + Send>>>);

impl EventSink {
    pub(crate) fn new(writer: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }

    /// Writes `event` with `blob` as one frame.
    ///
    /// # Errors
    /// The frame codec's message (the app closed the pipe, a blob over its limit).
    pub(crate) fn send(&self, event: &Event, blob: &[u8]) -> Result<(), String> {
        // A poisoned lock still holds a usable writer: a frame write either completed or
        // failed as a whole call.
        let mut writer = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        protocol::write_message(&mut *writer, event, blob)
    }
}

/// Cancels the running generation from another thread.
pub(crate) trait Canceller: Send + Sync {
    /// Requests cancellation (idempotent).
    fn cancel(&self);
    /// Clears a previous request before a new generation.
    fn reset(&self);
    /// Whether cancellation was requested since the last `reset`.
    fn is_cancelled(&self) -> bool;
}

/// The library seam of the worker.
pub(crate) trait SdBackend {
    /// Loads the package in `package_dir`; returns `(sd_version, sd_commit)`.
    fn init(&mut self, package_dir: &Path, build: SdCppBuild) -> Result<(String, String), SdCppError>;
    /// Version, devices and cores of the loaded library.
    fn probe(&mut self) -> Result<ProbeReport, SdCppError>;
    /// Generates one image, reporting stages / steps / logs through `sink`. A cancelled run
    /// returns `SdCppError::GenerationCancelled`.
    fn generate(&mut self, files: &ModelFiles, context: &ContextOptions, request: &GenerateRequest, sink: &EventSink) -> Result<RgbImage, SdCppError>;
    /// Frees the loaded model (no-op when none).
    fn unload(&mut self);
    /// The canceller the reader thread uses while a generation runs.
    fn canceller(&self) -> Arc<dyn Canceller>;
}

/// What the reader thread hands the main loop.
enum Incoming {
    /// A request (with its frame blob).
    Request(Request, Vec<u8>),
    /// `Shutdown`, or stdin closed / unreadable: finish and exit.
    Exit,
}

/// The process entry of `--sdcpp-worker`: redirect stdout, arm the parent-death signal, drop
/// `GGML_BACKEND_PATH`, then serve the real library over stdin / stdout. Returns the exit code.
#[must_use]
pub fn run_worker_main() -> i32 {
    let frames = match stdio::take_frame_stdout() {
        Ok(frames) => frames,
        Err(err) => {
            eprintln!("[sdcpp-worker] cannot set up the frame channel: {err}");
            return EXIT_SETUP_FAILED;
        }
    };
    let expected_parent = std::env::var(crate::client::spawn::PARENT_PID_ENV).ok().and_then(|pid| pid.parse::<u32>().ok());
    if !stdio::die_with_parent(expected_parent) {
        return EXIT_PARENT_GONE;
    }
    // SAFETY (edition-2024 `remove_var`): still single-threaded here — no thread has been
    // spawned yet and no library is loaded, so nothing reads the environment concurrently.
    // ggml reads this variable to load backends from elsewhere; the package dir must win.
    unsafe { std::env::remove_var("GGML_BACKEND_PATH") };
    let mut backend = session::RealBackend::new();
    serve(std::io::stdin(), frames, &mut backend)
}

/// Serves requests from `reader` until `Shutdown` or EOF, answering on `writer`. Returns the
/// process exit code (0 = orderly exit).
pub(crate) fn serve<R, W>(reader: R, writer: W, backend: &mut dyn SdBackend) -> i32
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let sink = EventSink::new(Box::new(writer));
    let (tx, rx) = mpsc::channel::<Incoming>();
    let canceller = backend.canceller();
    let reader_thread = ms_thread::Builder::new().name("sdcpp-worker-reader".to_owned()).spawn(move || read_requests(reader, &tx, &*canceller));
    if let Err(err) = reader_thread {
        eprintln!("[sdcpp-worker] cannot start the reader thread: {err}");
        return EXIT_SETUP_FAILED;
    }
    let mut initialized = false;
    while let Ok(incoming) = rx.recv() {
        let Incoming::Request(request, _blob) = incoming else {
            break;
        };
        let reply = match request {
            Request::Init { protocol, package_dir, build } => handle_init(backend, protocol, &package_dir, build, &mut initialized),
            Request::Probe if initialized => backend.probe().map(|report| (Event::ProbeResult { report }, Vec::new())),
            Request::Generate(message) if initialized => handle_generate(backend, *message, &rx, &sink),
            Request::Unload => {
                backend.unload();
                Ok((Event::Unloaded, Vec::new()))
            }
            Request::Probe | Request::Generate(_) => Err(SdCppError::Protocol { detail: "the worker was not initialized".to_owned() }),
            Request::Image { .. } => Err(SdCppError::Protocol { detail: "an image frame without a generate request".to_owned() }),
            // Answered by the reader thread; one arriving here (no generation running) is moot.
            Request::Cancel => continue,
            Request::Shutdown => break,
        };
        let (event, blob) = match reply {
            Ok(reply) => reply,
            Err(SdCppError::GenerationCancelled | SdCppError::Cancelled) => (Event::Cancelled, Vec::new()),
            Err(err) => (Event::Error { error: WireError::from_error(&err) }, Vec::new()),
        };
        if let Err(err) = sink.send(&event, &blob) {
            eprintln!("[sdcpp-worker] the app closed the frame channel: {err}");
            backend.unload();
            return EXIT_PIPE_BROKEN;
        }
    }
    backend.unload();
    0
}

/// The reader thread: forwards requests (resetting the canceller as it forwards a
/// `Generate`), answers `Cancel` immediately, and on `Shutdown` / EOF / a broken frame cancels
/// the running work and tells the main loop to exit.
fn read_requests<R: Read>(mut reader: R, tx: &mpsc::Sender<Incoming>, canceller: &dyn Canceller) {
    loop {
        match protocol::read_message::<Request, _>(&mut reader) {
            Ok((Request::Cancel, _)) => canceller.cancel(),
            Ok((Request::Shutdown, _)) => break,
            Ok((request, blob)) => {
                // Reset in stream order: a `Cancel` that follows this `Generate` on the wire
                // must stay set however late the main loop picks the request up.
                if matches!(request, Request::Generate(_)) {
                    canceller.reset();
                }
                if tx.send(Incoming::Request(request, blob)).is_err() {
                    return;
                }
            }
            Err(err) => {
                eprintln!("[sdcpp-worker] request channel closed: {err}");
                break;
            }
        }
    }
    canceller.cancel();
    // The main loop may already be gone (it returned on a broken frame channel).
    if tx.send(Incoming::Exit).is_err() {
        eprintln!("[sdcpp-worker] the main loop ended before the exit request");
    }
}

/// `Init`: checks the protocol and loads the library.
fn handle_init(backend: &mut dyn SdBackend, protocol: u32, package_dir: &Path, build: SdCppBuild, initialized: &mut bool) -> Result<(Event, Vec<u8>), SdCppError> {
    if protocol != WORKER_PROTOCOL {
        return Err(SdCppError::Protocol { detail: format!("worker protocol {WORKER_PROTOCOL}, app sent {protocol}") });
    }
    let (sd_version, sd_commit) = backend.init(package_dir, build)?;
    *initialized = true;
    Ok((Event::Ready { protocol: WORKER_PROTOCOL, sd_version, sd_commit }, Vec::new()))
}

/// `Generate`: collects the announced image frames, runs the backend and checks the size.
fn handle_generate(backend: &mut dyn SdBackend, message: GenerateMessage, rx: &mpsc::Receiver<Incoming>, sink: &EventSink) -> Result<(Event, Vec<u8>), SdCppError> {
    let mut images = Vec::with_capacity(message.images.len());
    for expected in &message.images {
        match rx.recv() {
            Ok(Incoming::Request(Request::Image { slot }, blob)) if slot == *expected => images.push((slot, blob)),
            Ok(Incoming::Request(other, _)) => return Err(SdCppError::Protocol { detail: format!("expected image {expected:?}, got {other:?}") }),
            Ok(Incoming::Exit) | Err(_) => return Err(SdCppError::GenerationCancelled),
        }
    }
    let request = protocol::assemble_request(message.request, images).map_err(|detail| SdCppError::Protocol { detail })?;
    let image = backend.generate(&message.files, &message.context, &request, sink)?;
    if (image.width(), image.height()) != (request.width, request.height) {
        return Err(SdCppError::OutputSize { expected: (request.width, request.height), found: (image.width(), image.height()) });
    }
    let (width, height) = (image.width(), image.height());
    Ok((Event::Result { width, height }, image.into_pixels()))
}

/// Test doubles of the backend, shared by the worker and client tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::request::RunStage;

    /// How the fake generation behaves.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum FakeMode {
        /// Emits stages and `steps` steps, returns a gray image of the requested size.
        Normal,
        /// Returns an image of half the requested size.
        WrongSize,
        /// Waits until cancelled, then returns `Cancelled`.
        WaitForCancel,
        /// Ignores cancel and blocks until `release` is set.
        IgnoreCancel,
        /// Fails `init` with an ABI mismatch.
        AbiMismatch,
    }

    /// A flag-based canceller.
    #[derive(Default)]
    pub(crate) struct FlagCanceller(pub(crate) AtomicBool);

    impl Canceller for FlagCanceller {
        fn cancel(&self) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn reset(&self) {
            self.0.store(false, Ordering::SeqCst);
        }
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// The fake backend; counts context (re)creations.
    pub(crate) struct FakeBackend {
        pub(crate) mode: FakeMode,
        pub(crate) cancel: Arc<FlagCanceller>,
        pub(crate) release: Arc<AtomicBool>,
        pub(crate) contexts_created: Arc<AtomicU32>,
        key: Option<(ModelFiles, ContextOptions)>,
    }

    impl FakeBackend {
        pub(crate) fn new(mode: FakeMode) -> Self {
            Self { mode, cancel: Arc::new(FlagCanceller::default()), release: Arc::new(AtomicBool::new(false)), contexts_created: Arc::new(AtomicU32::new(0)), key: None }
        }
    }

    impl SdBackend for FakeBackend {
        fn init(&mut self, _package_dir: &Path, _build: SdCppBuild) -> Result<(String, String), SdCppError> {
            if self.mode == FakeMode::AbiMismatch {
                return Err(SdCppError::AbiMismatch { expected: crate::catalog::PINNED.short_commit.to_owned(), found: "0000000".to_owned() });
            }
            Ok(("fake".to_owned(), crate::catalog::PINNED.short_commit.to_owned()))
        }

        fn probe(&mut self) -> Result<ProbeReport, SdCppError> {
            Ok(ProbeReport { sd_version: "fake".into(), sd_commit: crate::catalog::PINNED.short_commit.into(), devices: vec![crate::request::DeviceInfo { name: "Vulkan1".into(), description: "Fake GPU".into() }], physical_cores: 4 })
        }

        fn generate(&mut self, files: &ModelFiles, context: &ContextOptions, request: &GenerateRequest, sink: &EventSink) -> Result<RgbImage, SdCppError> {
            if let crate::request::DeviceSelection::Named { name, .. } = &context.device
                && name != "Vulkan1"
            {
                return Err(SdCppError::DeviceNotFound { name: name.clone() });
            }
            let key = (files.clone(), context.clone());
            if self.key.as_ref() != Some(&key) {
                sink.send(&Event::Stage { stage: RunStage::LoadingModel }, &[]).map_err(|detail| SdCppError::Protocol { detail })?;
                self.contexts_created.fetch_add(1, Ordering::SeqCst);
                self.key = Some(key);
            }
            sink.send(&Event::Stage { stage: RunStage::Generating }, &[]).map_err(|detail| SdCppError::Protocol { detail })?;
            match self.mode {
                FakeMode::WaitForCancel => {
                    while !self.cancel.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    return Err(SdCppError::GenerationCancelled);
                }
                FakeMode::IgnoreCancel => {
                    while !self.release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    return Err(SdCppError::GenerationCancelled);
                }
                FakeMode::Normal | FakeMode::WrongSize | FakeMode::AbiMismatch => {}
            }
            for step in 1..=request.steps {
                sink.send(&Event::Step { step, steps: request.steps }, &[]).map_err(|detail| SdCppError::Protocol { detail })?;
            }
            let (width, height) = if self.mode == FakeMode::WrongSize { (request.width / 2, request.height / 2) } else { (request.width, request.height) };
            let len = usize::try_from(u64::from(width) * u64::from(height) * 3).map_err(|_| SdCppError::Generation { detail: "size".into() })?;
            RgbImage::new(width, height, vec![128; len]).map_err(|err| SdCppError::Generation { detail: err.to_string() })
        }

        fn unload(&mut self) {
            self.key = None;
        }

        fn canceller(&self) -> Arc<dyn Canceller> {
            self.cancel.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::fake::{FakeBackend, FakeMode};
    use super::*;
    use crate::protocol::{ImageSlot, SlotKind, split_request};
    use crate::request::{ComponentRole, DeviceSelection, FlashAttention, ModelFamily, RunStage};

    pub(crate) fn job_parts(width: u32, height: u32) -> (ModelFiles, ContextOptions, GenerateRequest) {
        let files = ModelFiles { family: ModelFamily::Flux2Klein, components: BTreeMap::from([(ComponentRole::Diffusion, PathBuf::from("/m/d.gguf"))]) };
        let context = ContextOptions { device: DeviceSelection::Named { name: "Vulkan1".into(), description: String::new() }, text_encoder_on_cpu: true, weights_in_ram: false, max_vram_gib: None, flash_attention: FlashAttention::Auto, threads: None };
        let request = GenerateRequest {
            prompt: "p".into(),
            negative_prompt: String::new(),
            width,
            height,
            init_image: Some(RgbImage::new(width, height, vec![9; usize::try_from(width * height * 3).expect("small")]).expect("init")),
            mask: None,
            references: Vec::new(),
            strength: 1.0,
            steps: 3,
            cfg_scale: 1.0,
            seed: 1,
            sample_method: None,
            scheduler: None,
            vae_tiling: false,
        };
        (files, context, request)
    }

    /// A writer appending to a shared buffer.
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("output lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs `serve` over in-memory requests and returns every event written.
    fn run(mode: FakeMode, requests: &[(Request, Vec<u8>)]) -> (i32, Vec<(Event, Vec<u8>)>) {
        let mut input = Vec::new();
        for (request, blob) in requests {
            protocol::write_message(&mut input, request, blob).expect("encode request");
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let mut backend = FakeBackend::new(mode);
        let code = serve(std::io::Cursor::new(input), Shared(output.clone()), &mut backend);
        let bytes = output.lock().expect("output lock").clone();
        let mut cursor = std::io::Cursor::new(bytes);
        let mut events = Vec::new();
        while usize::try_from(cursor.position()).expect("position") < cursor.get_ref().len() {
            events.push(protocol::read_message::<Event, _>(&mut cursor).expect("decode event"));
        }
        (code, events)
    }

    fn init() -> (Request, Vec<u8>) {
        (Request::Init { protocol: WORKER_PROTOCOL, package_dir: std::env::temp_dir(), build: SdCppBuild::Cpu }, Vec::new())
    }

    fn generate_frames(width: u32, height: u32) -> Vec<(Request, Vec<u8>)> {
        let (files, context, request) = job_parts(width, height);
        let (header, images) = split_request(&request);
        let mut frames = vec![(Request::Generate(Box::new(GenerateMessage { files, context, request: header, images: images.iter().map(|(slot, _)| *slot).collect() })), Vec::new())];
        frames.extend(images.into_iter().map(|(slot, bytes)| (Request::Image { slot }, bytes.to_vec())));
        frames
    }

    #[test]
    fn init_probe_generate_and_unload() {
        let mut requests = vec![init(), (Request::Probe, Vec::new())];
        requests.extend(generate_frames(4, 2));
        requests.push((Request::Unload, Vec::new()));
        let (code, events) = run(FakeMode::Normal, &requests);
        assert_eq!(code, 0);
        let kinds: Vec<&Event> = events.iter().map(|(event, _)| event).collect();
        assert!(matches!(kinds[0], Event::Ready { protocol: WORKER_PROTOCOL, .. }));
        assert!(matches!(kinds[1], Event::ProbeResult { .. }));
        assert_eq!(kinds[2], &Event::Stage { stage: RunStage::LoadingModel });
        assert_eq!(kinds[3], &Event::Stage { stage: RunStage::Generating });
        assert_eq!(kinds[4..7], [&Event::Step { step: 1, steps: 3 }, &Event::Step { step: 2, steps: 3 }, &Event::Step { step: 3, steps: 3 }]);
        assert_eq!(kinds[7], &Event::Result { width: 4, height: 2 });
        assert_eq!(events[7].1.len(), 4 * 2 * 3);
        assert_eq!(kinds[8], &Event::Unloaded);
    }

    #[test]
    fn protocol_violations_and_wrong_size_are_errors() {
        let (_, events) = run(FakeMode::Normal, &[(Request::Probe, Vec::new())]);
        assert!(matches!(&events[0].0, Event::Error { error: WireError::Protocol { .. } }));

        let mut stray = vec![init()];
        stray.push((Request::Image { slot: ImageSlot { kind: SlotKind::Init, width: 1, height: 1 } }, vec![0; 3]));
        let (_, events) = run(FakeMode::Normal, &stray);
        assert!(matches!(&events[1].0, Event::Error { error: WireError::Protocol { .. } }));

        let mut wrong = vec![init()];
        wrong.extend(generate_frames(4, 2));
        let (_, events) = run(FakeMode::WrongSize, &wrong);
        assert_eq!(events.last().map(|(event, _)| event.clone()), Some(Event::Error { error: WireError::OutputSize { expected: (4, 2), found: (2, 1) } }));

        let old = (Request::Init { protocol: WORKER_PROTOCOL + 1, package_dir: std::env::temp_dir(), build: SdCppBuild::Cpu }, Vec::new());
        let (_, events) = run(FakeMode::Normal, &[old]);
        assert!(matches!(&events[0].0, Event::Error { error: WireError::Protocol { .. } }));

        let (_, events) = run(FakeMode::AbiMismatch, &[init()]);
        assert!(matches!(&events[0].0, Event::Error { error: WireError::AbiMismatch { .. } }));
    }

    #[test]
    fn eof_during_a_generation_cancels_it() {
        // The request stream ends right after the generate frames: the reader thread cancels,
        // the waiting fake returns `Cancelled`, and the worker exits cleanly.
        let mut requests = vec![init()];
        requests.extend(generate_frames(2, 2));
        let (code, events) = run(FakeMode::WaitForCancel, &requests);
        assert_eq!(code, 0);
        assert_eq!(events.last().map(|(event, _)| event.clone()), Some(Event::Cancelled));
    }
}
