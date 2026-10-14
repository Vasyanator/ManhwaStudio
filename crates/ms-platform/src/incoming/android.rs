/*
File: incoming/android.rs

Purpose:
The JNI half of the incoming-images inbox: binds the Kotlin wake-up `IncomingImages.nativeOnIncoming`
to this library and calls the static helpers of `IncomingImages` (Kotlin file
`android/app/src/main/kotlin/io/github/vasyanator/manhwastudio/platform/IncomingImages.kt`).

Key functions:
- init(): load the class, `RegisterNatives`, `markNativeReady`, `sweepClaimed`, `takePending`.
- take_pending(): `takePending` + decoding; releases the folders the decode drops.
- release_batch(): `releaseBatch`.
- Java_io_github_vasyanator_manhwastudio_platform_IncomingImages_nativeOnIncoming(): the wake-up.

Notes:
JNI contract (all static on the Kotlin object; Kotlin never throws into native code):
- `markNativeReady()V`; `sweepClaimed(Landroid/content/Context;)V`;
- `takePending(Landroid/content/Context;)[Ljava/lang/String;`: one "<dir>\n<batch.tsv>" entry
  per claimed batch (decoded by `listing.rs`);
- `releaseBatch(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;`: null = ok;
- native `nativeOnIncoming()V`: may run on any Kotlin thread; only raises the flag and wakes.
Every call here is blocking JNI: `init` runs pre-window on the android_main thread, the others
on workers.
*/

use std::path::Path;
use std::sync::OnceLock;

use jni::errors::Error as JniError;
use jni::objects::{JClass, JObjectArray, JString};
use jni::refs::Global;
use jni::strings::JNIStr;
use jni::{EnvUnowned, JValue, NativeMethod, Outcome, jni_sig, jni_str};
use ms_log::runtime_log;

use super::listing::decode_entries;
use super::{IncomingBatch, IncomingError};
use crate::android::jni_ctx::{self, JniBridgeError, read_string_array};

/// Binary name of the Kotlin inbox class, loaded through the application class loader. The
/// single owner of this name; renaming or moving the Kotlin object breaks the hand-off.
const INCOMING_CLASS: &JNIStr = jni_str!("io.github.vasyanator.manhwastudio.platform.IncomingImages");

/// The loaded class with `nativeOnIncoming` bound (only a fully bound class is cached).
static CLASS: OnceLock<Global<JClass<'static>>> = OnceLock::new();

/// See [`super::init`].
pub(super) fn init() -> Result<Vec<IncomingBatch>, IncomingError> {
    let class = incoming_class().map_err(|err| bridge_error(&err))?;
    jni_ctx::with_env(|env, context| {
        env.call_static_method(class, jni_str!("markNativeReady"), jni_sig!("()V"), &[])?.v()?;
        env.call_static_method(class, jni_str!("sweepClaimed"), jni_sig!("(Landroid/content/Context;)V"), &[JValue::Object(context)])?.v()?;
        Ok(())
    })
    .map_err(|err| bridge_error(&err))?;
    let batches = take_with(class)?;
    runtime_log::log_info(format!("[incoming] inbox bound; {} batch(es) waiting at start", batches.len()));
    Ok(batches)
}

/// See [`super::take_pending_blocking`].
pub(super) fn take_pending() -> Result<Vec<IncomingBatch>, IncomingError> {
    let class = incoming_class().map_err(|err| bridge_error(&err))?;
    take_with(class)
}

/// See [`super::release_batch_blocking`].
pub(super) fn release_batch(dir: &Path) {
    let released = incoming_class().and_then(|class| release_with(class, dir));
    match released {
        Ok(None) => runtime_log::log_info(format!("[incoming] released batch '{}'", folder_name(dir))),
        Ok(Some(detail)) => runtime_log::log_warn(format!("[incoming] could not release batch '{}': {detail}", folder_name(dir))),
        Err(err) => runtime_log::log_warn(format!("[incoming] could not release batch '{}': {err}", folder_name(dir))),
    }
}

/// `takePending`, decoded; the folders the decode drops are released here.
fn take_with(class: &Global<JClass<'static>>) -> Result<Vec<IncomingBatch>, IncomingError> {
    let entries = jni_ctx::with_env(|env, context| {
        let answer = env
            .call_static_method(class, jni_str!("takePending"), jni_sig!("(Landroid/content/Context;)[Ljava/lang/String;"), &[JValue::Object(context)])?
            .l()?;
        if answer.is_null() {
            return Err(JniError::NullPtr("IncomingImages.takePending").into());
        }
        let answer = JObjectArray::<JString>::cast_local(env, answer)?;
        Ok(read_string_array(env, &answer)?)
    })
    .map_err(|err| bridge_error(&err))?;
    let take = decode_entries(&entries);
    for dir in &take.to_release {
        match release_with(class, dir) {
            Ok(None) => {}
            Ok(Some(detail)) => runtime_log::log_warn(format!("[incoming] could not release dropped batch '{}': {detail}", folder_name(dir))),
            Err(err) => runtime_log::log_warn(format!("[incoming] could not release dropped batch '{}': {err}", folder_name(dir))),
        }
    }
    if let Some(detail) = take.error {
        runtime_log::log_error(format!("[incoming] malformed inbox listing; {} claimed batch(es) dropped: {detail}", take.to_release.len()));
        return Err(IncomingError::Listing(detail));
    }
    let files: usize = take.batches.iter().map(|batch| batch.files.len()).sum();
    let skipped: usize = take.batches.iter().map(|batch| batch.skipped.len()).sum();
    runtime_log::log_info(format!("[incoming] took {} batch(es): {files} file(s), {skipped} skipped", take.batches.len()));
    Ok(take.batches)
}

/// `releaseBatch(dir)`: `Ok(None)` = deleted, `Ok(Some(text))` = Kotlin refused or failed.
fn release_with(class: &Global<JClass<'static>>, dir: &Path) -> Result<Option<String>, JniBridgeError> {
    jni_ctx::with_env(|env, context| {
        let dir = JString::from_str(env, dir.to_string_lossy())?;
        let answer = env
            .call_static_method(class, jni_str!("releaseBatch"), jni_sig!("(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;"), &[JValue::Object(context), JValue::Object(&dir)])?
            .l()?;
        if answer.is_null() {
            return Ok(None);
        }
        let text = env.cast_local::<JString>(answer)?;
        Ok(Some(text.try_to_string(env)?))
    })
}

/// The `IncomingImages` class, loaded once, with `nativeOnIncoming` bound to this library.
fn incoming_class() -> Result<&'static Global<JClass<'static>>, JniBridgeError> {
    if let Some(class) = CLASS.get() {
        return Ok(class);
    }
    let loaded = jni_ctx::load_app_class(INCOMING_CLASS)?;
    register_natives(&loaded)?;
    Ok(CLASS.get_or_init(|| loaded))
}

/// Binds `IncomingImages.nativeOnIncoming` explicitly (`RegisterNatives`), so the wake-up never
/// depends on the dynamic-symbol export of the final `cdylib`.
fn register_natives(class: &Global<JClass<'static>>) -> Result<(), JniBridgeError> {
    jni_ctx::with_env(|env, _context| {
        // SAFETY: the pointer is the `extern "system"` function below; its parameters (the env and
        // the class of a STATIC method) and its void return match the signature "()V" exactly.
        let method = unsafe {
            NativeMethod::from_raw_parts(
                jni_str!("nativeOnIncoming"),
                jni_str!("()V"),
                Java_io_github_vasyanator_manhwastudio_platform_IncomingImages_nativeOnIncoming as *mut std::ffi::c_void,
            )
        };
        // SAFETY: `nativeOnIncoming` is declared `@JvmStatic external` on the Kotlin object, i.e. a
        // static native method, matching the `JClass` second parameter.
        unsafe { env.register_native_methods(class, &[method])? };
        Ok(())
    })
}

/// The log form of a bridge failure, as the typed error.
fn bridge_error(err: &JniBridgeError) -> IncomingError {
    runtime_log::log_warn(format!("[incoming] bridge call failed: {err}"));
    IncomingError::Bridge(err.to_string())
}

/// Only the batch folder's own name goes to the log (no app-private absolute paths).
fn folder_name(dir: &Path) -> String {
    dir.file_name().map_or_else(|| "?".to_owned(), |name| name.to_string_lossy().into_owned())
}

/// `IncomingImages.nativeOnIncoming` (JNI name fixed by `INCOMING_CLASS`; also bound explicitly
/// by `register_natives`). Called by Kotlin after a batch was published; never blocks, never
/// throws or unwinds into the JVM.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_vasyanator_manhwastudio_platform_IncomingImages_nativeOnIncoming<'local>(mut unowned_env: EnvUnowned<'local>, _class: JClass<'local>) {
    let outcome = unowned_env
        .with_env(|_env| -> Result<(), JniError> {
            super::signal_pending();
            Ok(())
        })
        .into_outcome();
    match outcome {
        Outcome::Ok(()) => {}
        Outcome::Err(err) => runtime_log::log_warn(format!("[incoming] wake-up failed: {err}")),
        Outcome::Panic(_) => runtime_log::log_error("[incoming] panic in the incoming-images wake-up"),
    }
}
