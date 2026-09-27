/*
File: crates/ms-config/src/ort_load_guard.rs

Purpose:
The crash-safe `General.ort_load_state` guard: the durable markers that record whether
loading the onnxruntime dynamic library for a given `(build, provider, device, version)`
scope was ATTEMPTED and whether it SUCCEEDED, so a launch after an uncatchable SIGILL
can refuse to repeat the same load.

Why it lives HERE and not in the settings tab, where it was written:
its WRITER is the native ONNX Runtime load path (crate `ms-native-runtime`) and its
READER is `crate::read_ort_load_guard` / `crate::ort_load_decision` in this crate; the
settings UI only offers a "retry" button. It is a `user_config.json` section writer like
every other one in this crate, and the loader must not depend on the settings tab.
`src/tabs/settings/mod.rs` re-exports the three public markers, so
`crate::tabs::settings::{mark_ort_load_attempted, mark_ort_load_succeeded,
reset_ort_load_guard}` still resolve inside the binary.

Key functions:
- `mark_ort_load_attempted()` / `mark_ort_load_succeeded()` / `reset_ort_load_guard()`
- `read_user_config_root()`: shared fresh (unlocked) read of `user_config.json` into a
  JSON object root, used by display/seeding readers of the settings surfaces.

Notes:
Every marker write is ONE serialized `ms_docstore` read-modify-write with
`Durability::ContentsAndDirectory` (contents fsynced before the atomic rename, directory
fsynced after). The reason is spelled out on `write_ort_load_state` and the durability
must not be dropped as a micro-optimization.
*/

use crate::runtime_log;
use ms_docstore::{AtomicWriteError, DocStoreError, Durability};
use serde_json::{Map, Value};
use std::path::Path;

/// Marks the ONNX Runtime load for `scope_key` as attempted-but-not-succeeded and
/// flushes the change to durable storage before returning.
///
/// Writes `{attempted:true, succeeded:false}` under
/// `General.ort_load_state[scope_key]` and fsyncs the file. Call this
/// immediately BEFORE touching the onnxruntime library so a subsequent SIGILL
/// leaves the aborted-attempt marker on disk for the next launch to read.
/// Synchronous disk I/O: do not call from the GUI thread.
// Called by the native ONNX Runtime load path (crate `ms-native-runtime`).
pub fn mark_ort_load_attempted(
    user_settings_file: &Path,
    scope_key: &str,
) -> Result<(), String> {
    write_ort_load_state(
        user_settings_file,
        scope_key,
        Some(crate::OrtLoadGuard {
            attempted: true,
            succeeded: false,
        }),
    )
}

/// Marks the ONNX Runtime load for `scope_key` as succeeded and flushes to disk.
///
/// Writes `{attempted:true, succeeded:true}` under
/// `General.ort_load_state[scope_key]` and fsyncs the file (durable for
/// symmetry with [`mark_ort_load_attempted`]). Call this only after the load
/// returns normally. Synchronous disk I/O: do not call from the GUI thread.
// Called by the native ONNX Runtime load path (crate `ms-native-runtime`).
pub fn mark_ort_load_succeeded(
    user_settings_file: &Path,
    scope_key: &str,
) -> Result<(), String> {
    write_ort_load_state(
        user_settings_file,
        scope_key,
        Some(crate::OrtLoadGuard {
            attempted: true,
            succeeded: true,
        }),
    )
}

/// Clears the ONNX Runtime load guard for `scope_key` (used by a future "retry
/// ORT" control) and flushes to disk.
///
/// Removes the `General.ort_load_state[scope_key]` entry so the scope reads as
/// "no attempt recorded" again, then fsyncs. Synchronous disk I/O: do not call
/// from the GUI thread.
// Called by the "Повторить попытку ORT" control + the native graceful-failure reset.
pub fn reset_ort_load_guard(
    user_settings_file: &Path,
    scope_key: &str,
) -> Result<(), String> {
    write_ort_load_state(user_settings_file, scope_key, None)
}

/// Reads `user_config.json` fresh into a JSON object root, tolerating a missing
/// or non-object file by returning an empty object.
///
/// Unlocked read through `ms_docstore` (the atomic writes guarantee an untorn file). Use
/// it for display/seeding only: a value read here and written back later is a stale
/// snapshot — writers go through [`crate::update_user_config_file`] or the section
/// writers instead.
///
/// # Errors
/// A localized message when an existing file cannot be read or parsed, so callers never
/// silently overwrite an unreadable config.
pub fn read_user_config_root(user_settings_file: &Path) -> Result<Value, String> {
    let doc = ms_docstore::DocRef::new(user_settings_file, ms_docstore::DocKind::UserConfig);
    match ms_docstore::read_value(&doc) {
        Ok(Some(root)) if root.is_object() => Ok(root),
        Ok(Some(_) | None) => Ok(Value::Object(Map::new())),
        Err(err) => Err(crate::user_config_error_message(user_settings_file, &err)),
    }
}

/// Read-modify-write of a single `General.ort_load_state` scope entry that is durable
/// before returning.
///
/// `entry = Some(guard)` upserts `{attempted, succeeded}` for `scope_key`;
/// `entry = None` removes it. One serialized `ms_docstore::update` (other keys preserved)
/// with [`Durability::ContentsAndDirectory`]: the new contents are fsynced in the temp
/// file BEFORE the atomic rename, and the containing directory is fsynced after it.
///
/// Durability is the whole point: the onnxruntime library can abort the process with an
/// uncatchable SIGILL on CPUs missing required instructions, and that fault can arrive
/// immediately after this returns. The marker must already be on stable storage then, or
/// the next launch would not see the aborted attempt and would re-trigger the same crash.
///
/// A failed DIRECTORY fsync is logged and tolerated, as it always was: the new file is
/// already renamed into place with fsynced contents, so a process crash (the SIGILL case)
/// cannot lose it; only a simultaneous OS crash could. Every other failure — including a
/// malformed document, which is never overwritten — is an error.
fn write_ort_load_state(
    user_settings_file: &Path,
    scope_key: &str,
    entry: Option<crate::OrtLoadGuard>,
) -> Result<(), String> {
    let outcome = crate::update_user_config_root(user_settings_file, Durability::ContentsAndDirectory, |root_obj| {
        crate::edit_section(root_obj, "General", |general_obj| {
            crate::edit_section(general_obj, crate::GENERAL_ORT_LOAD_STATE_KEY, |state_obj| match entry {
                Some(guard) => {
                    state_obj.insert(
                        scope_key.to_string(),
                        serde_json::json!({
                            "attempted": guard.attempted,
                            "succeeded": guard.succeeded,
                        }),
                    );
                }
                None => {
                    state_obj.remove(scope_key);
                }
            });
        });
    });
    match outcome {
        Ok(()) => Ok(()),
        Err(DocStoreError::Write(AtomicWriteError::DirSync { dir, reason })) => {
            runtime_log::log_warn(format!(
                "[settings] ORT load-guard marker written to {} but its directory {} could not be fsynced ({reason}); \
                 the marker contents were fsynced and the file is in place.",
                user_settings_file.display(),
                dir.display()
            ));
            Ok(())
        }
        Err(err) => Err(crate::user_config_error_message(user_settings_file, &err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OrtLoadDecision, OrtLoadGuard};
    use serde_json::Value;
    use std::fs;
    use std::path::PathBuf;

    // Unique temp file per test to avoid cross-test/process collisions,
    // following the project's existing `temp_dir + process id` test pattern.
    fn temp_config_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ms_ort_guard_{}_{}_{:?}.json",
            tag,
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn read_root(path: &Path) -> Value {
        let raw = fs::read_to_string(path).expect("config file written");
        serde_json::from_str::<Value>(&raw).expect("config file is valid json")
    }

    #[test]
    fn ort_load_state_round_trip_attempted_succeeded_reset() {
        let path = temp_config_path("round_trip");
        let _ = fs::remove_file(&path);
        let scope = crate::ort_load_scope_key("cpu", None, "1.20.1");

        // Before any write, the scope reads as "no attempt".
        mark_ort_load_attempted(&path, &scope).expect("mark attempted");
        let root = read_root(&path);
        assert_eq!(
            crate::read_ort_load_guard(&root, &scope),
            OrtLoadGuard {
                attempted: true,
                succeeded: false
            }
        );
        assert_eq!(
            crate::ort_load_decision(crate::read_ort_load_guard(&root, &scope)),
            OrtLoadDecision::Suspect
        );

        mark_ort_load_succeeded(&path, &scope).expect("mark succeeded");
        let root = read_root(&path);
        assert_eq!(
            crate::read_ort_load_guard(&root, &scope),
            OrtLoadGuard {
                attempted: true,
                succeeded: true
            }
        );
        assert_eq!(
            crate::ort_load_decision(crate::read_ort_load_guard(&root, &scope)),
            OrtLoadDecision::Safe
        );

        reset_ort_load_guard(&path, &scope).expect("reset guard");
        let root = read_root(&path);
        assert_eq!(
            crate::read_ort_load_guard(&root, &scope),
            OrtLoadGuard {
                attempted: false,
                succeeded: false
            }
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ort_marker_write_makes_its_directory_entry_durable() {
        let path = temp_config_path("durable");
        let _ = fs::remove_file(&path);
        let scope = crate::ort_load_scope_key("cpu", None, "1.20.1");

        mark_ort_load_attempted(&path, &scope).expect("mark attempted");

        // The rename is followed by the directory fsync before the call returns (a no-op
        // step on Windows, recorded all the same), so a SIGILL right after cannot lose it.
        let steps = ms_docstore::recorded_steps(&path);
        let renamed = steps.iter().position(|step| *step == ms_docstore::WriteStep::Renamed);
        let durable = steps.iter().position(|step| *step == ms_docstore::WriteStep::DirectoryDurable);
        assert!(matches!((renamed, durable), (Some(r), Some(d)) if r < d), "steps: {steps:?}");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ort_marker_write_never_overwrites_a_malformed_config() {
        let path = temp_config_path("malformed");
        fs::write(&path, "{broken").expect("seed malformed config");
        let scope = crate::ort_load_scope_key("cpu", None, "1.20.1");

        assert!(mark_ort_load_attempted(&path, &scope).is_err());
        assert_eq!(fs::read_to_string(&path).expect("read back"), "{broken");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ort_load_state_is_scoped_per_provider() {
        let path = temp_config_path("scoped");
        let _ = fs::remove_file(&path);
        let cpu = crate::ort_load_scope_key("cpu", None, "1.20.1");
        let cuda = crate::ort_load_scope_key("cuda", Some(0), "1.20.1");

        mark_ort_load_attempted(&path, &cuda).expect("mark cuda attempted");
        let root = read_root(&path);
        // A failed CUDA attempt must not mark the CPU scope as suspect.
        assert_eq!(
            crate::read_ort_load_guard(&root, &cpu),
            OrtLoadGuard {
                attempted: false,
                succeeded: false
            }
        );
        assert_eq!(
            crate::read_ort_load_guard(&root, &cuda),
            OrtLoadGuard {
                attempted: true,
                succeeded: false
            }
        );

        let _ = fs::remove_file(&path);
    }

}
