/*
File: credentials/android.rs

Purpose:
The JNI half of the Android credential store: static calls into the Kotlin `CredentialBridge`
(android/app/src/main/kotlin/io/github/vasyanator/manhwastudio/platform/CredentialBridge.kt).

Key functions:
- read() / write() / delete(): one blocking bridge call each, mapped to `CredentialError`.

Notes:
JNI contract (all static on the Kotlin object; Kotlin never throws into native code):
- `read(Landroid/content/Context;Ljava/lang/String;)[Ljava/lang/String;` — decoded by
  `super::decode_read_answer`;
- `write(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;` and
  `delete(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;` — null = done,
  otherwise the failure detail.
Every call is blocking (Keystore binder calls, a synchronous preferences commit): worker
threads only. Logs name the operation and the error only, never the secret (callers add their
own context: which key, which service).
*/

use std::sync::OnceLock;

use jni::objects::{JClass, JObject, JObjectArray, JString};
use jni::refs::Global;
use jni::strings::JNIStr;
use jni::{Env, JValue, jni_sig, jni_str};
use ms_log::runtime_log;

use super::{CredentialError, decode_read_answer};
use crate::android::jni_ctx::{self, JniBridgeError, read_string_array};
use crate::{PlatformFeature, Unavailable, UnavailableReason};

/// Binary name of the Kotlin bridge class, loaded through the application class loader. The
/// single owner of this name; renaming or moving the Kotlin object breaks the store.
const CREDENTIAL_CLASS: &JNIStr = jni_str!("io.github.vasyanator.manhwastudio.platform.CredentialBridge");

/// The loaded bridge class (global reference, usable from any thread).
static CLASS: OnceLock<Global<JClass<'static>>> = OnceLock::new();

/// See [`super::read_blocking`]; `name` is the [`super::entry_name`].
pub(super) fn read(name: &str) -> Result<Option<String>, CredentialError> {
    let answer = bridge_class()
        .and_then(|class| {
            jni_ctx::with_env(|env, context| {
                let name = JString::from_str(env, name)?;
                let answer = env
                    .call_static_method(class, jni_str!("read"), jni_sig!("(Landroid/content/Context;Ljava/lang/String;)[Ljava/lang/String;"), &[JValue::Object(context), JValue::Object(&name)])?
                    .l()?;
                if answer.is_null() {
                    // Never sent by Kotlin; decodes as a malformed (empty) answer.
                    return Ok(Vec::new());
                }
                let answer = JObjectArray::<JString>::cast_local(env, answer)?;
                Ok(read_string_array(env, &answer)?)
            })
        })
        .map_err(|err| bridge_error("read", &err))?;
    decode_read_answer(answer).inspect_err(|err| log_failure("read", err))
}

/// See [`super::store_blocking`].
pub(super) fn write(name: &str, secret: &str) -> Result<(), CredentialError> {
    let detail = bridge_class()
        .and_then(|class| {
            jni_ctx::with_env(|env, context| {
                let name = JString::from_str(env, name)?;
                let secret = JString::from_str(env, secret)?;
                let answer = env
                    .call_static_method(
                        class,
                        jni_str!("write"),
                        jni_sig!("(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                        &[JValue::Object(context), JValue::Object(&name), JValue::Object(&secret)],
                    )?
                    .l()?;
                optional_string(env, answer)
            })
        })
        .map_err(|err| bridge_error("write", &err))?;
    status_result("write", detail)
}

/// See [`super::delete_blocking`].
pub(super) fn delete(name: &str) -> Result<(), CredentialError> {
    let detail = bridge_class()
        .and_then(|class| {
            jni_ctx::with_env(|env, context| {
                let name = JString::from_str(env, name)?;
                let answer = env
                    .call_static_method(class, jni_str!("delete"), jni_sig!("(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;"), &[JValue::Object(context), JValue::Object(&name)])?
                    .l()?;
                optional_string(env, answer)
            })
        })
        .map_err(|err| bridge_error("delete", &err))?;
    status_result("delete", detail)
}

/// A nullable `String` answer: `None` for null.
fn optional_string<'local>(env: &mut Env<'local>, answer: JObject<'local>) -> Result<Option<String>, JniBridgeError> {
    if answer.is_null() {
        return Ok(None);
    }
    let text = env.cast_local::<JString>(answer)?;
    Ok(Some(text.try_to_string(env)?))
}

/// `write` / `delete` answer: null = done, otherwise the Kotlin failure detail.
fn status_result(operation: &str, detail: Option<String>) -> Result<(), CredentialError> {
    match detail {
        None => Ok(()),
        Some(detail) => {
            let err = CredentialError::Backend { detail };
            log_failure(operation, &err);
            Err(err)
        }
    }
}

/// The bridge class, loaded once.
fn bridge_class() -> Result<&'static Global<JClass<'static>>, JniBridgeError> {
    if let Some(class) = CLASS.get() {
        return Ok(class);
    }
    let loaded = jni_ctx::load_app_class(CREDENTIAL_CLASS)?;
    Ok(CLASS.get_or_init(|| loaded))
}

/// Maps a JVM-side failure: a missing class is the typed `Unavailable { BridgeMissing }`,
/// everything else a `Backend` error. Logged here with the operation.
fn bridge_error(operation: &str, err: &JniBridgeError) -> CredentialError {
    let mapped = match err {
        JniBridgeError::BridgeMissing { .. } => CredentialError::Unavailable(Unavailable::new(PlatformFeature::CredentialStore, UnavailableReason::BridgeMissing)),
        JniBridgeError::NoContext | JniBridgeError::Jni(_) => CredentialError::Backend { detail: err.to_string() },
    };
    log_failure(operation, &mapped);
    mapped
}

/// Warn-level log of a failed operation (typed error only; it never carries a secret).
fn log_failure(operation: &str, err: &CredentialError) {
    runtime_log::log_warn(format!("[credentials] {operation} failed: {err:?}"));
}
