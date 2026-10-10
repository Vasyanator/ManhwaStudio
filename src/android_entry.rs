/*
File: src/android_entry.rs

Purpose:
Android process entry of the `manhwastudio_rs` library (built as a `cdylib` for the APK). It
prepares the three things the desktop startup takes for granted on Android and then runs the
same startup flow as the desktop binary (`crate::native_main`).

Key functions:
- android_main(): the unmangled entry android-activity's GameActivity glue calls on its
  dedicated main-loop thread.
- start(): the fallible body of `android_main`.
- init_tls_verifier(): hands the application context to `rustls-platform-verifier`.
- enter_runtime_root(): creates `<internal data dir>/ms_root` and makes it the working dir.

Key structures:
- AndroidStartError: why the startup could not reach `native_main`.

Notes:
Logging: android-activity already forwards stdout/stderr (so `eprintln!` and the default panic
message) to logcat, and catches a panic of `android_main` itself; `android_logger` adds the
`log` facade (eframe, winit, glow). `ms_log::runtime_log` keeps writing `last.log` under the
runtime root once `run_main` initializes it.
The runtime root (`ms_config::program_dir` / `data_dir`) falls back to the working directory,
so the working directory must be `ms_root` BEFORE the first `ms_config` / `ms_fonts` call.
android-activity may call `android_main` again for a re-created Activity; every step here is
idempotent or replaces its previous state.
*/

use std::path::PathBuf;

use jni::objects::JObject;
use jni::{JavaVM, jni_sig, jni_str};
use ms_window_geometry::android_app::{self, AndroidApp};

/// Logcat tag of every `log` record the process emits.
const LOGCAT_TAG: &str = "ManhwaStudio";
/// Directory under the app's internal data dir that becomes the runtime root.
const RUNTIME_ROOT_DIR_NAME: &str = "ms_root";

/// Why the Android startup stopped before reaching `native_main`.
#[derive(Debug, thiserror::Error)]
enum AndroidStartError {
    /// The Activity reported no internal data directory, so there is no runtime root.
    #[error("Android reported no internal data directory for this app")]
    NoInternalDataPath,
    /// The runtime root directory could not be created.
    #[error("could not create the runtime root '{path}': {source}")]
    CreateRuntimeRoot { path: PathBuf, source: std::io::Error },
    /// The runtime root could not become the working directory.
    #[error("could not enter the runtime root '{path}': {source}")]
    EnterRuntimeRoot { path: PathBuf, source: std::io::Error },
    /// A JNI call needed to initialize the TLS verifier failed.
    #[error("could not initialize the platform TLS verifier: {0}")]
    TlsVerifier(#[from] jni::errors::Error),
    /// The shared startup flow returned a fatal error (already logged by `native_main`).
    #[error("startup failed: {0:#}")]
    Startup(anyhow::Error),
}

/// Android entry point, called by android-activity on its main-loop thread with the handle of
/// the current Activity. Never panics on a startup error: it logs the error to logcat and
/// returns, after which android-activity finishes the Activity.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    // `init_once` ignores repeated calls, so a re-created Activity keeps the first logger.
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag(LOGCAT_TAG),
    );
    log::info!("android_main: starting");
    match start(app) {
        Ok(()) => log::info!("android_main: exited normally"),
        Err(err) => log::error!("android_main: {err}"),
    }
}

/// Fallible body of `android_main`: registers the Activity handle, initializes TLS, enters
/// the runtime root, then runs the shared startup flow.
///
/// # Errors
/// Returns the first failed step as an `AndroidStartError`.
fn start(app: AndroidApp) -> Result<(), AndroidStartError> {
    // Before any window: every `NativeOptions` builder reads the handle from there.
    android_app::install(app.clone());
    // A failure here is fatal on purpose: rustls-platform-verifier panics on its first
    // verification when it was never initialized, so continuing would only move the failure
    // into a later HTTPS call on some worker thread.
    init_tls_verifier(&app)?;
    let root = enter_runtime_root(&app)?;
    log::info!("android_main: runtime root '{}'", root.display());
    crate::native_main().map_err(AndroidStartError::Startup)
}

/// Initializes `rustls-platform-verifier` with the APPLICATION context.
///
/// The application context lives as long as the process, unlike the Activity, which may be
/// re-created while the verifier keeps its first context forever. The verifier initializes
/// only once; a repeated call is a no-op.
///
/// # Errors
/// Returns the JNI error of a failed `getApplicationContext` call or verifier setup.
fn init_tls_verifier(app: &AndroidApp) -> Result<(), jni::errors::Error> {
    // SAFETY: `vm_as_ptr` is the process's valid, non-null JavaVM pointer (android-activity
    // created the `AndroidApp` from it).
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let raw_activity: jni::sys::jobject = app.activity_as_ptr().cast();
    vm.attach_current_thread(|env| {
        // SAFETY: `raw_activity` is a valid global reference owned by android-activity and
        // alive as long as `app`, which outlives this call; the `Cast` wrapper does not take
        // ownership, so nothing here deletes it.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw_activity)? };
        let context = env
            .call_method(&*activity, jni_str!("getApplicationContext"), jni_sig!("()Landroid/content/Context;"), &[])?
            .l()?;
        rustls_platform_verifier::android::init_with_env(env, context)
    })
}

/// Creates `<internal data dir>/ms_root` and makes it the process working directory, which
/// the runtime-root resolver (`ms_config::runtime_root`) then picks as program and data dir.
///
/// # Errors
/// `NoInternalDataPath` when Android reports no internal data dir; `CreateRuntimeRoot` /
/// `EnterRuntimeRoot` with the OS error of the failed directory operation.
fn enter_runtime_root(app: &AndroidApp) -> Result<PathBuf, AndroidStartError> {
    let root = app
        .internal_data_path()
        .ok_or(AndroidStartError::NoInternalDataPath)?
        .join(RUNTIME_ROOT_DIR_NAME);
    std::fs::create_dir_all(&root).map_err(|source| AndroidStartError::CreateRuntimeRoot {
        path: root.clone(),
        source,
    })?;
    std::env::set_current_dir(&root).map_err(|source| AndroidStartError::EnterRuntimeRoot {
        path: root.clone(),
        source,
    })?;
    Ok(root)
}
