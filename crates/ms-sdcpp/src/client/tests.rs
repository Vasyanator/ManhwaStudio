/*
File: crates/ms-sdcpp/src/client/tests.rs

Purpose:
Tests of `SdCppService` over an in-process fake worker: a thread running `worker::serve` with
the fake backend over OS pipes (or a scripted crashing worker), launched through the
`WorkerLauncher` seam. No library, no GPU, no child process.

Notes:
Model files are tiny synthetic GGUF / safetensors headers in a temp directory, so the
service's real validation runs.
*/

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;

use super::spawn::{WorkerChild, WorkerLauncher, WorkerPipes};
use super::*;
use crate::model_probe::fixtures::{gguf, safetensors};
use crate::protocol::{self, Event, Request};
use crate::request::{ComponentRole, ContextOptions, DeviceSelection, FlashAttention, GenerateRequest, ModelFamily, ModelFiles};
use crate::worker::fake::{FakeBackend, FakeMode};

/// The fake process: a thread whose return value is the exit code.
struct FakeChild {
    handle: Option<JoinHandle<i32>>,
    exit: Option<String>,
    release: Arc<AtomicBool>,
}

impl WorkerChild for FakeChild {
    fn try_wait(&mut self) -> io::Result<Option<String>> {
        if self.exit.is_none() && self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            let code = self.handle.take().map(JoinHandle::join);
            self.exit = Some(match code {
                Some(Ok(code)) => format!("code {code}"),
                Some(Err(_)) | None => "panicked".to_owned(),
            });
        }
        Ok(self.exit.clone())
    }

    fn kill(&mut self) -> io::Result<()> {
        self.release.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Launches fake workers; `crash` makes them abort on `Generate` like a ggml assert.
struct FakeLauncher {
    mode: FakeMode,
    crash: bool,
    contexts: Arc<AtomicU32>,
    release: Arc<AtomicBool>,
    launches: Arc<AtomicU32>,
}

impl FakeLauncher {
    fn new(mode: FakeMode) -> Self {
        Self { mode, crash: false, contexts: Arc::new(AtomicU32::new(0)), release: Arc::new(AtomicBool::new(false)), launches: Arc::new(AtomicU32::new(0)) }
    }
}

/// A worker that answers `Init`, then on `Generate` writes an assert to stderr and dies.
fn crashing_worker(mut requests: io::PipeReader, mut events: io::PipeWriter, mut stderr: io::PipeWriter) -> i32 {
    while let Ok((request, _)) = protocol::read_message::<Request, _>(&mut requests) {
        match request {
            Request::Init { .. } => protocol::write_message(&mut events, &Event::Ready { protocol: WORKER_PROTOCOL, sd_version: "fake".into(), sd_commit: "be48649".into() }, &[]).expect("ready"),
            Request::Generate(_) => {
                writeln!(stderr, "ggml.c:3390: GGML_ASSERT(ggml_can_mul_mat(a, b)) failed").expect("stderr");
                return 134;
            }
            _ => {}
        }
    }
    0
}

impl WorkerLauncher for FakeLauncher {
    fn launch(&self) -> Result<WorkerPipes, SdCppError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        let pipe = || io::pipe().map_err(|err| SdCppError::WorkerSpawn { detail: err.to_string() });
        let (req_r, req_w) = pipe()?;
        let (ev_r, ev_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        let (mode, crash, contexts, release) = (self.mode, self.crash, self.contexts.clone(), self.release.clone());
        let handle = std::thread::spawn(move || {
            if crash {
                return crashing_worker(req_r, ev_w, err_w);
            }
            let mut backend = FakeBackend::new(mode);
            backend.contexts_created = contexts;
            backend.release = release;
            let code = crate::worker::serve(req_r, ev_w, &mut backend);
            drop(err_w);
            code
        });
        Ok(WorkerPipes {
            stdin: Box::new(req_w),
            stdout: Box::new(ev_r) as Box<dyn Read + Send>,
            stderr: Some(Box::new(err_r)),
            child: Box::new(FakeChild { handle: Some(handle), exit: None, release: self.release.clone() }),
        })
    }
}

/// A service over `launcher` whose package directory is the temp dir.
fn service_with(launcher: FakeLauncher) -> SdCppService {
    let mut service = SdCppService::new(Box::new(launcher), Box::new(|_| Ok(std::env::temp_dir())));
    service.cancel_grace = Duration::from_millis(200);
    service
}

/// Valid FLUX.2 klein 4B fixture files in `dir`.
fn fixture_files(dir: &Path) -> ModelFiles {
    let write = |name: &str, bytes: Vec<u8>| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        path
    };
    let dit = write("dit.gguf", gguf(&[("general.architecture", "flux")], &[], &[("txt_in.weight", &[7680, 3072])]));
    let te = write("te.gguf", gguf(&[("general.architecture", "qwen3")], &[("qwen3.embedding_length", 2560)], &[("token_embd.weight", &[2560, 10])]));
    let vae = write("vae.safetensors", safetensors(&[("decoder.conv_in.bias", &[512])]));
    ModelFiles { family: ModelFamily::Flux2Klein, components: BTreeMap::from([(ComponentRole::Diffusion, dit), (ComponentRole::Vae, vae), (ComponentRole::Llm, te)]) }
}

fn job(files: ModelFiles, device: &str, threads: Option<u32>) -> GenerateJob {
    GenerateJob {
        build: SdCppBuild::Cpu,
        files,
        context: ContextOptions { device: DeviceSelection::Named { name: device.into(), description: String::new() }, text_encoder_on_cpu: true, weights_in_ram: false, max_vram_gib: None, flash_attention: FlashAttention::Auto, threads },
        request: GenerateRequest {
            prompt: "p".into(),
            negative_prompt: String::new(),
            width: 8,
            height: 4,
            init_image: Some(RgbImage::new(8, 4, vec![1; 96]).expect("init")),
            mask: None,
            references: Vec::new(),
            strength: 1.0,
            steps: 2,
            cfg_scale: 1.0,
            seed: 0,
            sample_method: None,
            scheduler: None,
            vae_tiling: false,
        },
    }
}

#[test]
fn generate_forwards_events_and_reuses_the_loaded_model() {
    let dir = tempfile::tempdir().expect("temp dir");
    let launcher = FakeLauncher::new(FakeMode::Normal);
    let (context_count, launch_count) = (launcher.contexts.clone(), launcher.launches.clone());
    let service = service_with(launcher);
    let files = fixture_files(dir.path());
    let mut seen = Vec::new();
    let image = service.generate(&job(files.clone(), "Vulkan1", None), &AtomicBool::new(false), &mut |event| seen.push(event)).expect("generate");
    assert_eq!((image.width(), image.height()), (8, 4));
    assert_eq!(
        seen,
        vec![RunEvent::Stage(RunStage::StartingWorker), RunEvent::Stage(RunStage::LoadingModel), RunEvent::Stage(RunStage::Generating), RunEvent::Step { step: 1, steps: 2 }, RunEvent::Step { step: 2, steps: 2 }]
    );
    assert!(service.is_worker_running());

    // Same files and options: the warm worker and model are reused.
    service.generate(&job(files.clone(), "Vulkan1", None), &AtomicBool::new(false), &mut |_| {}).expect("second");
    assert_eq!((launch_count.load(Ordering::SeqCst), context_count.load(Ordering::SeqCst)), (1, 1));
    // Another context option recreates the model in the same worker.
    service.generate(&job(files, "Vulkan1", Some(3)), &AtomicBool::new(false), &mut |_| {}).expect("third");
    assert_eq!((launch_count.load(Ordering::SeqCst), context_count.load(Ordering::SeqCst)), (1, 2));

    service.unload();
    assert!(!service.is_worker_running());
}

#[test]
fn cancel_is_cooperative_then_forced() {
    let dir = tempfile::tempdir().expect("temp dir");
    let files = fixture_files(dir.path());
    // The cancel is raised once sampling started (a cancel before that kills the starting
    // worker instead).
    let cancel = AtomicBool::new(false);
    let mut cancel_when_generating = |event: RunEvent| {
        if event == RunEvent::Stage(RunStage::Generating) {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let cooperative = service_with(FakeLauncher::new(FakeMode::WaitForCancel));
    assert_eq!(cooperative.generate(&job(files.clone(), "Vulkan1", None), &cancel, &mut cancel_when_generating), Err(SdCppError::GenerationCancelled));
    assert!(cooperative.is_worker_running(), "a cooperative cancel keeps the worker");

    cancel.store(false, Ordering::SeqCst);
    let stubborn = service_with(FakeLauncher::new(FakeMode::IgnoreCancel));
    let started = Instant::now();
    assert_eq!(stubborn.generate(&job(files, "Vulkan1", None), &cancel, &mut cancel_when_generating), Err(SdCppError::GenerationCancelled));
    assert!(started.elapsed() >= Duration::from_millis(200), "killed only after the grace period");
    assert!(!stubborn.is_worker_running(), "a killed worker is discarded");
}

#[test]
fn a_dying_worker_is_reported_with_its_stderr() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut launcher = FakeLauncher::new(FakeMode::Normal);
    launcher.crash = true;
    let service = service_with(launcher);
    let err = service.generate(&job(fixture_files(dir.path()), "Vulkan1", None), &AtomicBool::new(false), &mut |_| {}).expect_err("crash");
    match err {
        SdCppError::WorkerCrashed { exit, stderr_tail } => {
            assert_eq!(exit, "code 134");
            assert!(stderr_tail.contains("GGML_ASSERT"), "{stderr_tail}");
        }
        other => panic!("expected WorkerCrashed, got {other:?}"),
    }
    assert!(!service.is_worker_running());
}

#[test]
fn worker_errors_are_typed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let files = fixture_files(dir.path());
    let normal = service_with(FakeLauncher::new(FakeMode::Normal));
    assert_eq!(normal.generate(&job(files.clone(), "Vulkan9", None), &AtomicBool::new(false), &mut |_| {}), Err(SdCppError::DeviceNotFound { name: "Vulkan9".into() }));
    let wrong = service_with(FakeLauncher::new(FakeMode::WrongSize));
    assert_eq!(wrong.generate(&job(files.clone(), "Vulkan1", None), &AtomicBool::new(false), &mut |_| {}), Err(SdCppError::OutputSize { expected: (8, 4), found: (4, 2) }));
    let abi = service_with(FakeLauncher::new(FakeMode::AbiMismatch));
    assert!(matches!(abi.generate(&job(files.clone(), "Vulkan1", None), &AtomicBool::new(false), &mut |_| {}), Err(SdCppError::AbiMismatch { .. })));

    // Invalid files never reach a worker.
    let launcher = FakeLauncher::new(FakeMode::Normal);
    let launch_count = launcher.launches.clone();
    let checked = service_with(launcher);
    let mut bad = files;
    bad.components.insert(ComponentRole::Llm, PathBuf::from("/absent/te.gguf"));
    assert!(matches!(checked.generate(&job(bad, "Vulkan1", None), &AtomicBool::new(false), &mut |_| {}), Err(SdCppError::Files(_))));
    assert_eq!(launch_count.load(Ordering::SeqCst), 0);
}

#[test]
fn a_second_concurrent_generation_is_busy() {
    let dir = tempfile::tempdir().expect("temp dir");
    let files = fixture_files(dir.path());
    let service = Arc::new(service_with(FakeLauncher::new(FakeMode::WaitForCancel)));
    let cancel = Arc::new(AtomicBool::new(false));
    let first = {
        let (service, cancel, job) = (service.clone(), cancel.clone(), job(files.clone(), "Vulkan1", None));
        std::thread::spawn(move || service.generate(&job, &cancel, &mut |_| {}))
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !service.is_worker_running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(service.generate(&job(files, "Vulkan1", None), &AtomicBool::new(false), &mut |_| {}), Err(SdCppError::GenerationBusy));
    cancel.store(true, Ordering::SeqCst);
    assert_eq!(first.join().expect("first thread"), Err(SdCppError::GenerationCancelled));
}

/// Starts `generate` on another thread and waits until it is sampling.
fn generate_in_background(service: &Arc<SdCppService>, files: ModelFiles, build: SdCppBuild) -> JoinHandle<Result<RgbImage, SdCppError>> {
    let (service_thread, job) = (service.clone(), GenerateJob { build, ..job(files, "Vulkan1", None) });
    let sampling = Arc::new(AtomicBool::new(false));
    let seen = sampling.clone();
    let handle = std::thread::spawn(move || {
        service_thread.generate(&job, &AtomicBool::new(false), &mut |event| {
            if event == RunEvent::Stage(RunStage::Generating) {
                seen.store(true, Ordering::SeqCst);
            }
        })
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sampling.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(sampling.load(Ordering::SeqCst), "the generation started sampling");
    handle
}

#[test]
fn removing_a_build_stops_and_reaps_its_running_generation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let files = fixture_files(dir.path());
    let service = Arc::new(service_with(FakeLauncher::new(FakeMode::WaitForCancel)));

    // Another build's removal leaves the running generation alone.
    let running = generate_in_background(&service, files.clone(), SdCppBuild::Cpu);
    service.unload_build(SdCppBuild::Vulkan).expect("other build");
    assert!(!running.is_finished(), "a generation of another build keeps running");

    // The matching build: cancelled, and the hook returns only after the worker is gone.
    service.unload_build(SdCppBuild::Cpu).expect("stop the running build");
    assert!(!service.is_worker_running(), "no worker of the build is left when the hook returns");
    assert_eq!(running.join().expect("generation thread"), Err(SdCppError::GenerationCancelled));
}

#[test]
fn a_generation_that_ignores_cancel_is_killed_before_removal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let service = Arc::new(service_with(FakeLauncher::new(FakeMode::IgnoreCancel)));
    let running = generate_in_background(&service, fixture_files(dir.path()), SdCppBuild::Cpu);
    let started = Instant::now();
    service.unload_build(SdCppBuild::Cpu).expect("killed after the grace period");
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert!(!service.is_worker_running());
    assert_eq!(running.join().expect("generation thread"), Err(SdCppError::GenerationCancelled));
}

#[test]
fn unload_stops_a_running_generation_too() {
    let dir = tempfile::tempdir().expect("temp dir");
    let service = Arc::new(service_with(FakeLauncher::new(FakeMode::WaitForCancel)));
    let running = generate_in_background(&service, fixture_files(dir.path()), SdCppBuild::Cpu);
    service.unload();
    assert!(!service.is_worker_running());
    assert_eq!(running.join().expect("generation thread"), Err(SdCppError::GenerationCancelled));
}

#[test]
fn probe_uses_a_short_lived_worker() {
    let launcher = FakeLauncher::new(FakeMode::Normal);
    let launch_count = launcher.launches.clone();
    let service = service_with(launcher);
    let report = service.probe(SdCppBuild::Cpu, &AtomicBool::new(false)).expect("probe");
    assert_eq!(report.devices[0].name, "Vulkan1");
    assert_eq!(launch_count.load(Ordering::SeqCst), 1);
    assert!(!service.is_worker_running());
    let missing = SdCppService::new(Box::new(FakeLauncher::new(FakeMode::Normal)), Box::new(|build| Err(SdCppError::NotInstalled { build })));
    assert_eq!(missing.probe(SdCppBuild::Vulkan, &AtomicBool::new(false)), Err(SdCppError::NotInstalled { build: SdCppBuild::Vulkan }));
}

#[test]
fn exit_descriptions_name_signals_and_codes() {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(spawn::describe_exit(std::process::ExitStatus::from_raw(6)), "signal 6 (SIGABRT)");
        assert_eq!(spawn::describe_exit(std::process::ExitStatus::from_raw(134 << 8)), "code 134");
    }
}
