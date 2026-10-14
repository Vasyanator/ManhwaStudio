/*
File: native_libs.rs

Purpose:
Where the APK's own native libraries live on the device: the directory Android extracted
the `jniLibs` of the installed package into (`ApplicationInfo.nativeLibraryDir`). The
native ONNX Runtime loads the onnxruntime library bundled from the official
`onnxruntime-android` AAR from there.

Key functions:
- native_library_dir(): the directory, asked once over JNI and cached for the process.

Key structures:
- NativeLibraryDirError: the typed JNI failure.

Notes:
Android only (the module does not exist elsewhere: desktop has no package library folder,
so there is no fake answer to give). The directory path is randomized per install
(`/data/app/~~<hash>/<package>-<hash>/lib/arm64`), so it is always queried, never built.
It holds real files only because the app is packaged with `useLegacyPackaging = true`
(`android/app/build.gradle.kts`); with compressed-in-APK libraries the directory would be
empty and `dlopen` by path would fail.
*/

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jni::errors::Error as JniError;
use jni::objects::{JObject, JString};
use jni::{Env, jni_sig, jni_str};
use ms_log::runtime_log;

use crate::android::jni_ctx::{self, JniBridgeError};

/// The native library directory could not be asked from the JVM. `Display` is a
/// technical English detail (it is embedded in the caller's localized message and logged).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("could not read ApplicationInfo.nativeLibraryDir: {detail}")]
pub struct NativeLibraryDirError {
    /// The JNI failure text.
    pub detail: String,
}

/// Process cache: the directory never changes while the process lives (an app update
/// kills the process first).
static NATIVE_LIBRARY_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The installed package's native library directory (`ApplicationInfo.nativeLibraryDir`),
/// asked once over JNI and cached for the process; a failure is not cached, so a later
/// call asks again.
///
/// # Threading
/// The first call is a JVM round trip: call it from a worker thread, never the GUI thread
/// (the crate rule for JNI). Its consumer, the onnxruntime resolver, is worker-only.
///
/// # Errors
/// [`NativeLibraryDirError`] when the JVM context is missing, a JNI call throws, or the
/// field is null or empty.
pub fn native_library_dir() -> Result<&'static Path, NativeLibraryDirError> {
    if let Some(dir) = NATIVE_LIBRARY_DIR.get() {
        return Ok(dir);
    }
    let dir = jni_ctx::with_env(|env, context| read_native_library_dir(env, context).map_err(JniBridgeError::from)).map_err(|err| {
        runtime_log::log_warn(format!("[native-libs] could not read the native library directory: {err}"));
        NativeLibraryDirError { detail: err.to_string() }
    })?;
    if dir.as_os_str().is_empty() {
        runtime_log::log_warn("[native-libs] ApplicationInfo.nativeLibraryDir is empty");
        return Err(NativeLibraryDirError { detail: "the field is empty".to_string() });
    }
    runtime_log::log_info(format!("[native-libs] native library directory: {}", dir.display()));
    // A racing worker may have stored the same value first; both read one immutable path.
    Ok(NATIVE_LIBRARY_DIR.get_or_init(|| dir))
}

/// `context.getApplicationInfo().nativeLibraryDir` as a path. Every object is a local
/// reference of the frame `with_env` opened.
fn read_native_library_dir(env: &mut Env<'_>, context: &JObject<'_>) -> Result<PathBuf, JniError> {
    let info = env.call_method(context, jni_str!("getApplicationInfo"), jni_sig!("()Landroid/content/pm/ApplicationInfo;"), &[])?.l()?;
    if info.is_null() {
        return Err(JniError::NullPtr("Context.getApplicationInfo()"));
    }
    let dir = env.get_field(&info, jni_str!("nativeLibraryDir"), jni_sig!("Ljava/lang/String;"))?.l()?;
    let dir = env.cast_local::<JString>(dir)?;
    if dir.is_null() {
        return Err(JniError::NullPtr("ApplicationInfo.nativeLibraryDir"));
    }
    Ok(PathBuf::from(dir.try_to_string(env)?))
}
