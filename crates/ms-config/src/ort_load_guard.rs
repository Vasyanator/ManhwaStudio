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
- `read_user_config_root()`: shared fresh read of `user_config.json` into a JSON object
  root, also used by the settings tab's other section writers.

Notes:
`write_ort_load_state` carries the ONE fsync of this codebase; the reason is spelled out
at the call site and must not be dropped as a micro-optimization.
*/

// Only the Unix parent-directory fsync logs; on Windows that branch is compiled out.
#[cfg(unix)]
use crate::runtime_log;
use serde_json::{Map, Value};
use std::fs;
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
/// Returns an error only when an existing file cannot be read or parsed, so
/// callers never silently overwrite an unreadable config.
pub fn read_user_config_root(user_settings_file: &Path) -> Result<Value, String> {
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(user_settings_file) {
            Ok(raw) => serde_json::from_str::<Value>(&raw).map_err(|err| {
                tf!("settings.config_io.parse_error", user_settings_file = user_settings_file.display(), err = err)
            })?,
            Err(err) => {
                return Err(tf!("settings.config_io.read_error", user_settings_file = user_settings_file.display(), err = err));
            }
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    Ok(root)
}

/// Read-modify-write of a single `General.ort_load_state` scope entry with an
/// fsync before returning.
///
/// `entry = Some(guard)` upserts `{attempted, succeeded}` for `scope_key`;
/// `entry = None` removes it. The whole file is rewritten fresh (other keys
/// preserved), then fsynced so the change survives a process crash.
fn write_ort_load_state(
    user_settings_file: &Path,
    scope_key: &str,
    entry: Option<crate::OrtLoadGuard>,
) -> Result<(), String> {
    let _write_guard = crate::lock_user_config_write();
    // Whether the config file already existed decides if a parent-directory fsync is
    // also needed for durability (a fresh create adds a new directory entry that an
    // in-place overwrite never touches). Captured under the write lock so it reflects
    // the state this serialized write will act on.
    let file_pre_existed = user_settings_file.exists();
    let mut root = read_user_config_root(user_settings_file)?;
    let Some(root_obj) = root.as_object_mut() else {
        return Err(t!("settings.config_io.prepare_root_error").to_string());
    };
    let mut general_obj = root_obj
        .get("General")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut state_obj = general_obj
        .get(crate::GENERAL_ORT_LOAD_STATE_KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match entry {
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
    }
    general_obj.insert(
        crate::GENERAL_ORT_LOAD_STATE_KEY.to_string(),
        Value::Object(state_obj),
    );
    root_obj.insert("General".to_string(), Value::Object(general_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, &payload).map_err(|err| err.to_string())?;
    // Durability barrier: this is intentionally the first fsync in the codebase.
    // The onnxruntime library can abort the process with an uncatchable SIGILL on
    // CPUs missing required instructions, and that fault can arrive immediately
    // after this write returns. Without flushing, the page cache may still hold
    // the marker when the process dies, so the next launch would not see the
    // aborted attempt and would re-trigger the same crash. We reopen the file we
    // just overwrote in place and `sync_all()` its data to stable storage.
    let file = fs::OpenOptions::new()
        .write(true)
        .open(user_settings_file)
        .map_err(|err| {
            tf!("settings.config_io.open_for_fsync_error", user_settings_file = user_settings_file.display(), err = err)
        })?;
    file.sync_all().map_err(|err| {
        tf!("settings.config_io.fsync_error", user_settings_file = user_settings_file.display(), err = err)
    })?;
    // For an in-place overwrite of a pre-existing file the directory entry is
    // unchanged, so flushing the file contents alone is durable. A FRESH create,
    // however, also adds a new directory entry that only a parent-directory fsync
    // makes durable; without it a crash could lose the whole file (and its marker).
    if !file_pre_existed {
        fsync_parent_dir_best_effort(user_settings_file);
    }
    Ok(())
}

/// Best-effort fsync of `path`'s parent directory so a newly created file's
/// directory entry is durable.
///
/// Only meaningful on Unix, where a directory can be opened and `sync_all()`ed. On
/// Windows the standard library cannot fsync a directory handle, so this is a no-op
/// there; in practice `user_config.json` is created at first launch (well before any
/// ORT marker write), so the fresh-create-then-crash window this closes is Unix-only
/// and rare. Failures are logged, not surfaced: the file contents were already
/// fsynced, and a missing directory-entry flush only risks losing a first-ever
/// create, not corrupting an existing config.
fn fsync_parent_dir_best_effort(path: &Path) {
    #[cfg(unix)]
    {
        let Some(parent) = path.parent() else {
            return;
        };
        // An empty parent means the current directory; skip rather than open "".
        if parent.as_os_str().is_empty() {
            return;
        }
        match fs::File::open(parent) {
            Ok(dir) => {
                if let Err(err) = dir.sync_all() {
                    runtime_log::log_warn(format!(
                        "[settings] parent directory fsync failed for {} ({err}); the config \
                         contents were still fsynced.",
                        parent.display()
                    ));
                }
            }
            Err(err) => runtime_log::log_warn(format!(
                "[settings] could not open parent directory {} for fsync ({err}); the config \
                 contents were still fsynced.",
                parent.display()
            )),
        }
    }
    #[cfg(not(unix))]
    {
        // No portable directory fsync on Windows via std; see the doc comment.
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OrtLoadDecision, OrtLoadGuard};
    use serde_json::Value;
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
