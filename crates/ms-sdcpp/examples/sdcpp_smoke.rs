/*
File: crates/ms-sdcpp/examples/sdcpp_smoke.rs

Purpose:
Opt-in smoke test of the REAL sd.cpp library through the real worker process and client. Not
part of any test suite: it does nothing unless `MS_SDCPP_SMOKE_PACKAGE_DIR` is set. This
example binary is also its own worker: run with `--sdcpp-worker`, it serves the worker
protocol exactly as the app binary does.

Environment:
- MS_SDCPP_SMOKE_PACKAGE_DIR : an unpacked official package (the catalog's members) — required
- MS_SDCPP_SMOKE_BUILD       : `vulkan` (default) or `cpu`
- MS_SDCPP_SMOKE_LOG_DIR     : where the session log (`last.log`) is written (optional)
- MS_SDCPP_SMOKE_DIT / _VAE / _TE : FLUX.2 klein files; when all three are set, one generation
  runs at 256x256, 4 steps, text encoder on the CPU
- MS_SDCPP_SMOKE_DEVICE      : the device name for the generation (e.g. `Vulkan1`)
- MS_SDCPP_SMOKE_OUT         : where the generated image is written as a binary PPM
- MS_SDCPP_SMOKE_WRONG_TE    : an encoder that does NOT fit the DiT; the pair is sent to a
  worker by hand (bypassing the client's validation, which would refuse it) to show how the
  library dies and how that exit is described

Run: `cargo run -p ms-sdcpp --example sdcpp_smoke` with the variables above.
*/

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use ms_sdcpp::catalog::SdCppBuild;
use ms_sdcpp::client::SdCppService;
use ms_sdcpp::client::spawn::{ProcessLauncher, WORKER_FLAG, WorkerLauncher};
use ms_sdcpp::protocol::{self, Event, GenerateMessage, Request, WORKER_PROTOCOL};
use ms_sdcpp::request::{ComponentRole, ContextOptions, DeviceSelection, FlashAttention, GenerateJob, GenerateRequest, ModelFamily, ModelFiles, RgbImage};

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn main() {
    if std::env::args().any(|arg| arg == WORKER_FLAG) {
        std::process::exit(ms_sdcpp::worker::run_worker_main());
    }
    let Some(package_dir) = env_path("MS_SDCPP_SMOKE_PACKAGE_DIR") else {
        println!("MS_SDCPP_SMOKE_PACKAGE_DIR is not set; nothing to do");
        return;
    };
    if let Some(log_dir) = env_path("MS_SDCPP_SMOKE_LOG_DIR")
        && let Err(err) = ms_log::runtime_log::init_session_logs(&log_dir)
    {
        eprintln!("log init failed: {err}");
    }
    let build = std::env::var("MS_SDCPP_SMOKE_BUILD").ok().and_then(|slug| SdCppBuild::from_wire(&slug)).unwrap_or(SdCppBuild::Vulkan);
    let launcher = match ProcessLauncher::current_exe() {
        Ok(launcher) => launcher,
        Err(err) => {
            eprintln!("cannot find this executable: {err:?}");
            std::process::exit(1);
        }
    };
    let dir = package_dir.clone();
    let service = SdCppService::new(Box::new(launcher.clone()), Box::new(move |_| Ok(dir.clone())));
    let cancel = AtomicBool::new(false);

    let started = Instant::now();
    match service.probe(build, &cancel) {
        Ok(report) => {
            println!("probe ok in {:.2?}: sd.cpp {} ({}), {} physical cores", started.elapsed(), report.sd_version, report.sd_commit, report.physical_cores);
            for device in &report.devices {
                println!("  device {:<10} {}", device.name, device.description);
            }
        }
        Err(err) => {
            println!("probe FAILED in {:.2?}: {err:?}", started.elapsed());
            std::process::exit(1);
        }
    }

    let (Some(dit), Some(vae), Some(te)) = (env_path("MS_SDCPP_SMOKE_DIT"), env_path("MS_SDCPP_SMOKE_VAE"), env_path("MS_SDCPP_SMOKE_TE")) else {
        return;
    };
    let device = std::env::var("MS_SDCPP_SMOKE_DEVICE").ok().filter(|name| !name.is_empty());
    let files = ModelFiles { family: ModelFamily::Flux2Klein, components: BTreeMap::from([(ComponentRole::Diffusion, dit.clone()), (ComponentRole::Vae, vae.clone()), (ComponentRole::Llm, te)]) };
    let context = ContextOptions {
        device: device.map_or(DeviceSelection::Auto, |name| DeviceSelection::Named { name, description: String::new() }),
        text_encoder_on_cpu: true,
        weights_in_ram: false,
        max_vram_gib: None,
        flash_attention: FlashAttention::Auto,
        threads: None,
    };
    let request = GenerateRequest {
        prompt: "a small red house on a green hill, comic style".into(),
        negative_prompt: String::new(),
        width: 256,
        height: 256,
        init_image: None,
        mask: None,
        references: Vec::new(),
        strength: 1.0,
        steps: 4,
        cfg_scale: 1.0,
        seed: 42,
        sample_method: None,
        scheduler: None,
        vae_tiling: false,
    };
    let job = GenerateJob { build, files, context: context.clone(), request: request.clone() };
    for run in 1..=2 {
        let started = Instant::now();
        let mut steps = 0_u32;
        let result = service.generate(&job, &cancel, &mut |event| {
            println!("  event {event:?} at {:.2?}", started.elapsed());
            if matches!(event, ms_sdcpp::request::RunEvent::Step { .. }) {
                steps += 1;
            }
        });
        match result {
            Ok(image) => {
                println!("generation {run} ok in {:.2?}: {}x{}, {steps} steps", started.elapsed(), image.width(), image.height());
                if let Some(out) = env_path("MS_SDCPP_SMOKE_OUT") {
                    write_ppm(&out, &image);
                }
            }
            Err(err) => println!("generation {run} FAILED in {:.2?}: {err:?}", started.elapsed()),
        }
    }
    service.unload();

    if let Some(wrong_te) = env_path("MS_SDCPP_SMOKE_WRONG_TE") {
        let files = ModelFiles { family: ModelFamily::Flux2Klein, components: BTreeMap::from([(ComponentRole::Diffusion, dit), (ComponentRole::Vae, vae), (ComponentRole::Llm, wrong_te)]) };
        wrong_pair(&launcher, &package_dir, build, files, &context, &request);
    }
}

/// Writes `image` as a binary PPM (P6).
fn write_ppm(path: &std::path::Path, image: &RgbImage) {
    let mut bytes = format!("P6\n{} {}\n255\n", image.width(), image.height()).into_bytes();
    bytes.extend_from_slice(image.pixels());
    match std::fs::write(path, bytes) {
        Ok(()) => println!("  wrote {}", path.display()),
        Err(err) => println!("  cannot write {}: {err}", path.display()),
    }
}

/// Sends a mis-paired generation (`files`) to a worker by hand and reports how it ends.
fn wrong_pair(launcher: &ProcessLauncher, package_dir: &std::path::Path, build: SdCppBuild, files: ModelFiles, context: &ContextOptions, request: &GenerateRequest) {
    let started = Instant::now();
    let mut pipes = match launcher.launch() {
        Ok(pipes) => pipes,
        Err(err) => {
            println!("wrong pair: launch failed: {err:?}");
            return;
        }
    };
    let stderr = pipes.stderr.take();
    let drain = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut stderr) = stderr {
            let mut bytes = Vec::new();
            if let Err(err) = std::io::Read::read_to_end(&mut stderr, &mut bytes) {
                text = format!("stderr read failed: {err}");
            } else {
                text = String::from_utf8_lossy(&bytes).into_owned();
            }
        }
        text
    });
    let (header, _) = protocol::split_request(request);
    let send = |pipes: &mut ms_sdcpp::client::spawn::WorkerPipes, message: &Request| protocol::write_message(&mut pipes.stdin, message, &[]);
    let sent = send(&mut pipes, &Request::Init { protocol: WORKER_PROTOCOL, package_dir: package_dir.to_path_buf(), build })
        .and_then(|()| send(&mut pipes, &Request::Generate(Box::new(GenerateMessage { files, context: context.clone(), request: header, images: Vec::new() }))));
    if let Err(err) = sent {
        println!("wrong pair: send failed: {err}");
    }
    loop {
        match protocol::read_message::<Event, _>(&mut pipes.stdout) {
            Ok((Event::Log { .. }, _)) => {}
            Ok((event, _)) => println!("  wrong pair event: {event:?}"),
            Err(err) => {
                println!("  wrong pair: worker output closed after {:.2?}: {err}", started.elapsed());
                break;
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit = loop {
        match pipes.child.try_wait() {
            Ok(Some(exit)) => break exit,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                if let Err(err) = pipes.child.kill() {
                    println!("  kill failed: {err}");
                }
                break "still running; killed".to_owned();
            }
            Err(err) => break format!("status error {err}"),
        }
    };
    let stderr = drain.join().unwrap_or_else(|_| "stderr thread panicked".to_owned());
    let tail: Vec<&str> = stderr.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    println!("wrong pair: worker exit = {exit}");
    // The shape `SdCppService` returns for a worker whose output closed (unit-tested there).
    println!("wrong pair: as a client error: {:?}", ms_sdcpp::SdCppError::WorkerCrashed { exit, stderr_tail: tail.join("\n") });
}
