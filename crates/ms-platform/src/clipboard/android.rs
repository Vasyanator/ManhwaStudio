/*
File: clipboard/android.rs

Purpose:
Android clipboard text read over JNI: `Context.getSystemService("clipboard")` ->
`ClipboardManager.getPrimaryClip()` -> `getItemAt(0).coerceToText(context).toString()`.

Key functions:
- read_text(): the Android `read_text` backend; image, file list and HTML stay on
  `unavailable.rs` (Android has no reader for them, `PlatformFeature` says so).

Notes:
Runs on the CALLING thread, like the desktop arboard read: the clipboard API is synchronous
and its callers (the canvas paste flow) sit on the GUI thread. It is one short binder round
trip, the same cost class as arboard's X11/Wayland transfer; the "no JNI on the GUI thread"
rule of this crate targets the long bridge calls (class probes, SAF), which stay on workers.
`jni_ctx::with_env` attaches the thread (cheap once attached), runs the calls inside a fresh
local frame (no local-reference leak on a thread that never returns to Java) and clears any
Java exception a failed call left pending; every failure maps to `ClipboardError::Backend`.
Since Android 10 `getPrimaryClip()` answers null while the app has no input focus: that reads
as "no text" (`Ok(None)`), which is exactly what the user sees.
*/

use jni::errors::Error as JniError;
use jni::objects::{JObject, JString};
use jni::{Env, JValue, jni_sig, jni_str};
use ms_log::runtime_log;

use super::ClipboardError;
use crate::android::jni_ctx::{self, JniBridgeError};

/// Reads the primary clip as text. `Ok(None)` when there is no clip, it has no items, or the
/// system withholds it (app not focused).
///
/// # Errors
/// [`ClipboardError::Backend`] for any JNI / JVM failure (missing context, a thrown Java
/// exception, an unexpected null).
pub(super) fn read_text() -> Result<Option<String>, ClipboardError> {
    jni_ctx::with_env(|env, context| read_primary_clip_text(env, context).map_err(JniBridgeError::from)).map_err(|err| {
        runtime_log::log_warn(format!("[clipboard] Android clipboard text read failed: {err}"));
        ClipboardError::Backend { detail: err.to_string() }
    })
}

/// The JNI call chain of [`read_text`]. Every object it creates is a local reference of the
/// frame `with_env` opened.
fn read_primary_clip_text(env: &mut Env<'_>, context: &JObject<'_>) -> Result<Option<String>, JniError> {
    let service_name = JString::from_jni_str(env, jni_str!("clipboard"))?;
    let manager = env
        .call_method(context, jni_str!("getSystemService"), jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"), &[JValue::Object(&service_name)])?
        .l()?;
    if manager.is_null() {
        return Err(JniError::NullPtr("Context.getSystemService(\"clipboard\")"));
    }
    let clip = env.call_method(&manager, jni_str!("getPrimaryClip"), jni_sig!("()Landroid/content/ClipData;"), &[])?.l()?;
    if clip.is_null() {
        return Ok(None);
    }
    let item_count = env.call_method(&clip, jni_str!("getItemCount"), jni_sig!("()I"), &[])?.i()?;
    if item_count < 1 {
        return Ok(None);
    }
    let item = env.call_method(&clip, jni_str!("getItemAt"), jni_sig!("(I)Landroid/content/ClipData$Item;"), &[JValue::Int(0)])?.l()?;
    if item.is_null() {
        return Ok(None);
    }
    // `coerceToText` turns a URI or intent item into text as well, matching what a system
    // paste of the same clip inserts; it never returns null (an empty string at worst).
    let text = env
        .call_method(&item, jni_str!("coerceToText"), jni_sig!("(Landroid/content/Context;)Ljava/lang/CharSequence;"), &[JValue::Object(context)])?
        .l()?;
    if text.is_null() {
        return Ok(None);
    }
    let text = env.call_method(&text, jni_str!("toString"), jni_sig!("()Ljava/lang/String;"), &[])?.l()?;
    let text = env.cast_local::<JString>(text)?;
    if text.is_null() {
        return Ok(None);
    }
    Ok(Some(text.try_to_string(env)?))
}
