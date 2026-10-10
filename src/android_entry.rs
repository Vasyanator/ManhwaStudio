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
- runtime_root_path(): `<internal data dir>/ms_root`.
- check_bundled_assets(): refuses to start until the Kotlin `AssetInstaller` has installed the
  bundled assets of THIS APK into the runtime root.
- enter_runtime_root(): creates the runtime root and makes it the working dir.

Key structures:
- AndroidStartError: why the startup could not reach `native_main`.

Notes:
Logging: android-activity already forwards stdout/stderr (so `eprintln!` and the default panic
message) to logcat, and catches a panic of `android_main` itself; `android_logger` adds the
`log` facade (eframe, winit, glow). `ms_log::runtime_log` keeps writing `last.log` under the
runtime root once `run_main` initializes it.
The runtime root (`ms_config::program_dir` / `data_dir`) falls back to the working directory,
so the working directory must be `ms_root` BEFORE the first `ms_config` / `ms_fonts` call.
Bundled assets (fonts, optionally the wiki) are copied from the APK into `ms_root` by the Kotlin
`AssetInstaller` (`android/app/src/main/kotlin/.../AssetInstaller.kt`) on a worker thread of
`LaunchActivity`, which starts `MainActivity` only afterwards. `MainActivity` can still be
re-created directly (Recents after an APK update), so the native side re-checks the stamp
itself and stops instead of running on stale or half-copied files; `MainActivity` then sends
the user to `LaunchActivity`. The stamp names below are shared with `AssetInstaller.kt`.
android-activity may call `android_main` again for a re-created Activity; every step here is
idempotent or replaces its previous state.
*/

use std::ffi::CStr;
use std::io::Read;
use std::path::{Path, PathBuf};

use jni::objects::JObject;
use jni::{JavaVM, jni_sig, jni_str};
use ms_window_geometry::android_app::{self, AndroidApp};

/// Logcat tag of every `log` record the process emits.
const LOGCAT_TAG: &str = "ManhwaStudio";
/// Directory under the app's internal data dir that becomes the runtime root.
const RUNTIME_ROOT_DIR_NAME: &str = "ms_root";
/// APK asset holding the stamp of the bundled asset set (written by `build-android.sh`; the
/// same name is `AssetInstaller.BUNDLED_STAMP_ASSET` in Kotlin).
const BUNDLED_STAMP_ASSET: &CStr = c"ms_root.stamp";
/// File inside the runtime root that `AssetInstaller` writes LAST, atomically, after every
/// bundled file is in place (`AssetInstaller.INSTALLED_STAMP_FILE` in Kotlin).
const INSTALLED_STAMP_FILE: &str = ".ms_assets_stamp";

/// Why the Android startup stopped before reaching `native_main`.
#[derive(Debug, thiserror::Error)]
enum AndroidStartError {
    /// The Activity reported no internal data directory, so there is no runtime root.
    #[error("Android reported no internal data directory for this app")]
    NoInternalDataPath,
    /// The APK carries no bundled-asset stamp: it was not built by `build-android.sh`.
    #[error("the APK has no bundled-asset stamp '{}'; build it with build-android.sh", BUNDLED_STAMP_ASSET.to_string_lossy())]
    BundledStampMissing,
    /// The bundled-asset stamp could not be read from the APK.
    #[error("could not read the bundled-asset stamp from the APK: {0}")]
    ReadBundledStamp(std::io::Error),
    /// The installed-asset stamp exists but could not be read.
    #[error("could not read the installed-asset stamp '{path}': {source}")]
    ReadInstalledStamp { path: PathBuf, source: std::io::Error },
    /// The runtime root does not hold the assets of this APK yet (first start, APK update or an
    /// interrupted copy); `MainActivity` has handed over to `LaunchActivity`, which installs them.
    #[error("bundled assets are not installed in '{root}' yet; LaunchActivity installs them")]
    AssetsNotInstalled { root: PathBuf },
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

/// Fallible body of `android_main`: registers the Activity handle, checks that the bundled
/// assets of this APK are installed, initializes TLS, enters the runtime root, then runs the
/// shared startup flow.
///
/// # Errors
/// Returns the first failed step as an `AndroidStartError`.
fn start(app: AndroidApp) -> Result<(), AndroidStartError> {
    // Before any window: every `NativeOptions` builder reads the handle from there.
    android_app::install(app.clone());
    let root = runtime_root_path(&app)?;
    check_bundled_assets(&app, &root)?;
    // A failure here is fatal on purpose: rustls-platform-verifier panics on its first
    // verification when it was never initialized, so continuing would only move the failure
    // into a later HTTPS call on some worker thread.
    init_tls_verifier(&app)?;
    enter_runtime_root(&root)?;
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

/// The runtime root `<internal data dir>/ms_root` (not created here).
///
/// # Errors
/// `NoInternalDataPath` when Android reports no internal data dir.
fn runtime_root_path(app: &AndroidApp) -> Result<PathBuf, AndroidStartError> {
    Ok(app.internal_data_path().ok_or(AndroidStartError::NoInternalDataPath)?.join(RUNTIME_ROOT_DIR_NAME))
}

/// Succeeds only when `root` holds the installed-asset stamp equal to the stamp bundled in this
/// APK, i.e. `AssetInstaller` finished copying exactly this APK's assets.
///
/// # Errors
/// `BundledStampMissing` / `ReadBundledStamp` for a broken APK, `ReadInstalledStamp` for an
/// unreadable stamp file, `AssetsNotInstalled` when the stamp is absent or different.
fn check_bundled_assets(app: &AndroidApp, root: &Path) -> Result<(), AndroidStartError> {
    let mut bundled = String::new();
    app.asset_manager()
        .open(BUNDLED_STAMP_ASSET)
        .ok_or(AndroidStartError::BundledStampMissing)?
        .read_to_string(&mut bundled)
        .map_err(AndroidStartError::ReadBundledStamp)?;
    let stamp_path = root.join(INSTALLED_STAMP_FILE);
    let installed = match std::fs::read_to_string(&stamp_path) {
        Ok(text) => text,
        // No stamp: nothing was installed yet, or an install was interrupted before its end.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(AndroidStartError::ReadInstalledStamp { path: stamp_path, source }),
    };
    if installed.trim() == bundled.trim() && !bundled.trim().is_empty() {
        Ok(())
    } else {
        Err(AndroidStartError::AssetsNotInstalled { root: root.to_path_buf() })
    }
}

/// Creates the runtime root and makes it the process working directory, which the
/// runtime-root resolver (`ms_config::runtime_root`) then picks as program and data dir.
///
/// # Errors
/// `CreateRuntimeRoot` / `EnterRuntimeRoot` with the OS error of the failed directory operation.
fn enter_runtime_root(root: &Path) -> Result<(), AndroidStartError> {
    std::fs::create_dir_all(root).map_err(|source| AndroidStartError::CreateRuntimeRoot {
        path: root.to_path_buf(),
        source,
    })?;
    std::env::set_current_dir(root).map_err(|source| AndroidStartError::EnterRuntimeRoot {
        path: root.to_path_buf(),
        source,
    })
}
