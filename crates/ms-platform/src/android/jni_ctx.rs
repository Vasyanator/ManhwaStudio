/*
File: android/jni_ctx.rs

Purpose:
Access to the JVM for the Android backends: attach the current thread and hand out the
application context, and load the app's own classes.

Key functions:
- with_env(): attach + application context; clears a pending Java exception on failure.
- load_app_class(): `Context.getClassLoader().loadClass(name)` as a global reference.
- read_string_array(): a Java `String[]` answer as `Vec<String>` (shared by the bridges).

Notes:
`FindClass` on a native thread only sees system classes, so app classes (the Kotlin bridges)
must come from the application class loader. Do not call these on the GUI thread: every call
is a JVM round trip; the backends run them on a `platform-bridge` worker. Sole sanctioned
exception: `clipboard::android::read_text` (one short `ClipboardManager` binder call, the same
cost as the desktop clipboard read it replaces; see `clipboard/android.rs`).
*/

use jni::errors::Error as JniError;
use jni::objects::{JClass, JClassLoader, JObject, JObjectArray, JString};
use jni::refs::{Global, LoaderContext};
use jni::strings::JNIStr;
use jni::{Env, JavaVM, jni_sig, jni_str};

/// A failed JVM interaction.
#[derive(Debug, thiserror::Error)]
pub(crate) enum JniBridgeError {
    /// `ndk-context` holds a null `JavaVM` or application context.
    #[error("the Android JavaVM or application context is not available")]
    NoContext,
    /// The requested app class is not part of this build.
    #[error("Android bridge class {class} is not part of this build")]
    BridgeMissing { class: String },
    /// Any other JNI failure.
    #[error("JNI call failed: {0}")]
    Jni(#[from] JniError),
}

/// Attaches the current thread to the JVM and runs `f` with the env and the APPLICATION
/// context. A Java exception left pending by a failed `f` is logged to logcat and cleared, so
/// the thread never returns to native code with one pending.
///
/// # Errors
/// [`JniBridgeError::NoContext`] for a null VM/context, or whatever `f` returns.
pub(crate) fn with_env<R>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> Result<R, JniBridgeError>) -> Result<R, JniBridgeError> {
    // Invariant: the library's only entry point is `android_main`, and android-activity calls
    // `ndk_context::initialize_android_context` before it (android-activity `init.rs`), so the
    // context is set for the whole process lifetime and `android_context()` cannot panic here.
    let android = ndk_context::android_context();
    if android.vm().is_null() || android.context().is_null() {
        return Err(JniBridgeError::NoContext);
    }
    // SAFETY: the pointer is the process's `JavaVM` published by android-activity and checked
    // non-null above.
    let vm = unsafe { JavaVM::from_raw(android.vm().cast()) };
    let raw_context: jni::sys::jobject = android.context().cast();
    vm.attach_current_thread(|env| -> Result<R, JniBridgeError> {
        // SAFETY: `raw_context` is the Application global reference owned by android-activity
        // and alive for the whole process; the `Cast` wrapper never deletes it.
        let context = unsafe { env.as_cast_raw::<JObject>(&raw_context)? };
        let result = f(env, &context);
        if result.is_err() && env.exception_check() {
            env.exception_describe();
            env.exception_clear();
        }
        result
    })
}

/// Loads the app class `name` (binary name, dots) through the application class loader and
/// returns a global reference usable from any thread.
///
/// # Errors
/// [`JniBridgeError::BridgeMissing`] when the class is not in the APK (`ClassNotFoundException`);
/// [`JniBridgeError::Jni`] / [`JniBridgeError::NoContext`] for other failures.
pub(crate) fn load_app_class(name: &'static JNIStr) -> Result<Global<JClass<'static>>, JniBridgeError> {
    with_env(|env, context| {
        let loader = env.call_method(context, jni_str!("getClassLoader"), jni_sig!("()Ljava/lang/ClassLoader;"), &[])?.l()?;
        let loader = env.cast_local::<JClassLoader>(loader)?;
        let class = match LoaderContext::Loader(&loader).load_class(env, name, true) {
            Ok(class) => class,
            Err(JniError::NoClassDefFound { .. }) => return Err(JniBridgeError::BridgeMissing { class: name.to_string() }),
            Err(other) => return Err(other.into()),
        };
        Ok(env.new_global_ref(&class)?)
    })
}

/// Every element of a `String[]` (null array = empty, null element = empty string). Each
/// element's local reference is deleted at once, so long listings never fill the local frame.
///
/// # Errors
/// The JNI failure of a length / element read or of a string conversion.
pub(crate) fn read_string_array(env: &mut Env<'_>, array: &JObjectArray<'_, JString<'_>>) -> Result<Vec<String>, JniError> {
    if array.is_null() {
        return Ok(Vec::new());
    }
    let len = array.len(env)?;
    let mut items = Vec::with_capacity(len);
    for index in 0..len {
        let element = array.get_element(env, index)?;
        let text = if element.is_null() { String::new() } else { element.try_to_string(env)? };
        env.delete_local_ref(element);
        items.push(text);
    }
    Ok(items)
}
