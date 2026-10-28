/*
File: crates/ms-config/src/sdcpp_choice.rs

Purpose:
The persisted Stable-Diffusion.cpp (sd.cpp) runtime choice under `user_config` `General`
(`sdcpp_build`, `sdcpp_device`, `sdcpp_threads`) and the image-edit runtime vocabulary
(`ImageEditRuntime`: PyTorch backend or sd.cpp) that engine settings files persist.

Key items:
- GENERAL_SDCPP_BUILD_KEY / GENERAL_SDCPP_DEVICE_KEY / GENERAL_SDCPP_THREADS_KEY: the frozen
  `General` keys.
- SdCppChoice / SdCppDeviceChoice: the parsed choice (build slug, device, thread count).
- sdcpp_choice_from_user_settings(): the pure, logging reader (malformed -> default).
- save_sdcpp_build / save_sdcpp_device / save_sdcpp_threads: one `ms_docstore::update` each.
- ImageEditRuntime: `PyTorch` / `SdCpp` with frozen wire ids `pytorch` / `sdcpp`.

Notes:
- The build slug is opaque here: the sd.cpp package catalog (`ms-sdcpp`) owns the list of
  builds and the default when none is chosen (`build: None`); this crate only stores it.
- `sdcpp_build` is deliberately NOT in `user_config_defaults()`: absent means "let the catalog
  choose", so a later catalog default reaches every install that never picked a build.
  `sdcpp_device` (`"auto"`) and `sdcpp_threads` (`0` = auto) are in the default tree.
- The Python backend never reads these keys (documented in `PROTOCOL.md`).
- The savers do blocking disk I/O: never on the GUI thread.
*/

use std::path::Path;

use ms_log::runtime_log;
use serde_json::{Map, Value, json};

use crate::{edit_section, update_user_config_root, user_config_error_message};

/// `General` key of the selected sd.cpp build slug (a string from the `ms-sdcpp` catalog).
/// Frozen spelling. NOT in the default tree: absent = the catalog's default build.
pub const GENERAL_SDCPP_BUILD_KEY: &str = "sdcpp_build";

/// `General` key of the sd.cpp compute device: the string `"auto"` or an object
/// `{"name": "Vulkan1", "description": "…"}` (see [`SdCppDeviceChoice`]). Frozen spelling.
pub const GENERAL_SDCPP_DEVICE_KEY: &str = "sdcpp_device";

/// `General` key of the sd.cpp CPU thread count (a JSON integer; `0` = auto). Frozen spelling.
pub const GENERAL_SDCPP_THREADS_KEY: &str = "sdcpp_threads";

/// The frozen `"auto"` device token.
const DEVICE_AUTO: &str = "auto";
/// Field names of a named device object.
const DEVICE_NAME_FIELD: &str = "name";
const DEVICE_DESCRIPTION_FIELD: &str = "description";

/// The sd.cpp compute device the user chose.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum SdCppDeviceChoice {
    /// Let the runtime pick its default device.
    #[default]
    Auto,
    /// A device sd.cpp reported by name (`"Vulkan1"`, `"CPU"`); `description` is the
    /// human-readable adapter text stored with it, so a renumbered device can be recognized and
    /// shown even while it is absent. `name` is never empty.
    Named {
        /// The device name sd.cpp knows it by.
        name: String,
        /// The adapter description shown to the user (may be empty).
        description: String,
    },
}

impl SdCppDeviceChoice {
    /// The JSON value stored under [`GENERAL_SDCPP_DEVICE_KEY`]: `"auto"` or
    /// `{"name": …, "description": …}`.
    #[must_use]
    pub fn to_config_value(&self) -> Value {
        match self {
            Self::Auto => Value::String(DEVICE_AUTO.to_owned()),
            Self::Named { name, description } => json!({ DEVICE_NAME_FIELD: name, DEVICE_DESCRIPTION_FIELD: description }),
        }
    }

    /// Parses a stored value; `None` for anything that is neither `"auto"` nor an object with
    /// a non-empty string `name` and a string `description` (other fields are ignored).
    #[must_use]
    pub fn from_config_value(value: &Value) -> Option<Self> {
        match value {
            Value::String(token) if token.trim() == DEVICE_AUTO => Some(Self::Auto),
            Value::Object(obj) => {
                let name = obj.get(DEVICE_NAME_FIELD).and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty())?;
                let description = obj.get(DEVICE_DESCRIPTION_FIELD).and_then(Value::as_str)?;
                Some(Self::Named { name: name.to_owned(), description: description.to_owned() })
            }
            _ => None,
        }
    }
}

/// The persisted sd.cpp runtime choice. `Default` is a fresh install: catalog build, automatic
/// device, automatic thread count.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SdCppChoice {
    /// The chosen build slug; `None` = the `ms-sdcpp` catalog's default (validated there).
    pub build: Option<String>,
    /// The chosen compute device.
    pub device: SdCppDeviceChoice,
    /// CPU thread count; `0` = automatic.
    pub threads: u32,
}

/// Reads the sd.cpp choice out of a user-config value. Absent keys resolve silently to the
/// [`SdCppChoice::default`] field; a present but malformed value (a non-string or blank build,
/// an unknown device token or shape, a thread count that is not a non-negative integer within
/// `u32`) resolves to the default with a warning naming the key — never an error, the setting
/// must not stop anything from opening.
#[must_use]
pub fn sdcpp_choice_from_user_settings(user_settings: &Value) -> SdCppChoice {
    let general = user_settings.get("General").and_then(Value::as_object);
    let field = |key: &str| general.and_then(|general| general.get(key));
    let warn = |key: &str, raw: &Value, fallback: &str| {
        runtime_log::log_warn(format!("[sdcpp] invalid General.{key} value {raw}; using {fallback}"));
    };

    let build = field(GENERAL_SDCPP_BUILD_KEY).and_then(|raw| match raw.as_str().map(str::trim) {
        Some(slug) if !slug.is_empty() => Some(slug.to_owned()),
        _ => {
            warn(GENERAL_SDCPP_BUILD_KEY, raw, "the catalog default build");
            None
        }
    });
    let device = field(GENERAL_SDCPP_DEVICE_KEY).map_or(SdCppDeviceChoice::Auto, |raw| {
        SdCppDeviceChoice::from_config_value(raw).unwrap_or_else(|| {
            warn(GENERAL_SDCPP_DEVICE_KEY, raw, "\"auto\"");
            SdCppDeviceChoice::Auto
        })
    });
    let threads = field(GENERAL_SDCPP_THREADS_KEY).map_or(0, |raw| {
        raw.as_u64().and_then(|count| u32::try_from(count).ok()).unwrap_or_else(|| {
            warn(GENERAL_SDCPP_THREADS_KEY, raw, "0 (auto)");
            0
        })
    });
    SdCppChoice { build, device, threads }
}

/// Persists the sd.cpp build slug under `General.sdcpp_build`; `None` removes the key (back to
/// the catalog default). One serialized read-modify-write of the `General` section; every other
/// key survives. Blocking disk I/O: never on the GUI thread.
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then left
/// untouched) or cannot be written.
pub fn save_sdcpp_build(user_settings_file: &Path, build_slug: Option<&str>) -> Result<(), String> {
    write_general(user_settings_file, |general| match build_slug {
        Some(slug) => {
            general.insert(GENERAL_SDCPP_BUILD_KEY.to_owned(), Value::String(slug.to_owned()));
        }
        None => {
            general.remove(GENERAL_SDCPP_BUILD_KEY);
        }
    })
}

/// Persists the sd.cpp device under `General.sdcpp_device` (`"auto"` or the named object).
/// One serialized read-modify-write of the `General` section. Blocking disk I/O.
///
/// # Errors
/// As [`save_sdcpp_build`].
pub fn save_sdcpp_device(user_settings_file: &Path, device: &SdCppDeviceChoice) -> Result<(), String> {
    write_general(user_settings_file, |general| {
        general.insert(GENERAL_SDCPP_DEVICE_KEY.to_owned(), device.to_config_value());
    })
}

/// Persists the sd.cpp thread count under `General.sdcpp_threads` (`0` = auto). One serialized
/// read-modify-write of the `General` section. Blocking disk I/O.
///
/// # Errors
/// As [`save_sdcpp_build`].
pub fn save_sdcpp_threads(user_settings_file: &Path, threads: u32) -> Result<(), String> {
    write_general(user_settings_file, |general| {
        general.insert(GENERAL_SDCPP_THREADS_KEY.to_owned(), Value::Number(threads.into()));
    })
}

/// One `General`-section transaction shared by the three savers.
fn write_general(user_settings_file: &Path, edit: impl FnOnce(&mut Map<String, Value>)) -> Result<(), String> {
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, "General", edit);
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

/// Which local runtime an image-edit engine (FLUX.2 klein, FLUX.3 klein) runs on. The engine
/// settings files persist it by [`Self::wire`]. `Default` is `PyTorch`, the only runtime that
/// existed before sd.cpp, so a settings file without the field keeps its behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ImageEditRuntime {
    /// The Python AI backend (PyTorch).
    #[default]
    PyTorch,
    /// The Stable-Diffusion.cpp worker process.
    SdCpp,
}

impl ImageEditRuntime {
    /// Every runtime, in UI order.
    pub const ALL: [Self; 2] = [Self::PyTorch, Self::SdCpp];

    /// The frozen persisted id (`"pytorch"` / `"sdcpp"`). Never localized, never renamed.
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::PyTorch => "pytorch",
            Self::SdCpp => "sdcpp",
        }
    }

    /// Parses a persisted id (surrounding whitespace ignored, otherwise exact); `None` for an
    /// unknown id, the caller decides the fallback (normally [`Self::default`]).
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        let value = value.trim();
        Self::ALL.into_iter().find(|runtime| runtime.wire() == value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_keys_read_as_the_default_choice() {
        for root in [json!({}), json!({"General": {}}), json!({"General": "not an object"})] {
            assert_eq!(sdcpp_choice_from_user_settings(&root), SdCppChoice::default(), "{root}");
        }
        assert_eq!(SdCppChoice::default(), SdCppChoice { build: None, device: SdCppDeviceChoice::Auto, threads: 0 });
    }

    #[test]
    fn well_formed_values_are_read() {
        let root = json!({"General": {
            "sdcpp_build": " vulkan ",
            "sdcpp_device": {"name": "Vulkan1", "description": "AMD Radeon RX 9070", "extra": 1},
            "sdcpp_threads": 12
        }});
        let choice = sdcpp_choice_from_user_settings(&root);
        assert_eq!(choice.build.as_deref(), Some("vulkan"));
        assert_eq!(choice.device, SdCppDeviceChoice::Named { name: "Vulkan1".to_owned(), description: "AMD Radeon RX 9070".to_owned() });
        assert_eq!(choice.threads, 12);
        let auto = sdcpp_choice_from_user_settings(&json!({"General": {"sdcpp_device": "auto", "sdcpp_threads": 0}}));
        assert_eq!(auto.device, SdCppDeviceChoice::Auto);
        assert_eq!(auto.threads, 0);
    }

    #[test]
    fn malformed_values_fall_back_per_field() {
        for (build, device, threads) in [
            (json!(""), json!("Vulkan0"), json!(-1)),
            (json!("   "), json!({"name": "", "description": "x"}), json!(1.5)),
            (json!(7), json!({"name": "Vulkan0"}), json!("4")),
            (json!(null), json!(["auto"]), json!(u64::from(u32::MAX) + 1)),
            (json!({"slug": "cpu"}), json!({"name": 3, "description": "x"}), json!(null)),
        ] {
            let root = json!({"General": {"sdcpp_build": build, "sdcpp_device": device, "sdcpp_threads": threads}});
            assert_eq!(sdcpp_choice_from_user_settings(&root), SdCppChoice::default(), "{root}");
        }
        // One malformed field does not reset its well-formed siblings.
        let root = json!({"General": {"sdcpp_build": "cpu", "sdcpp_device": 5, "sdcpp_threads": 8}});
        assert_eq!(
            sdcpp_choice_from_user_settings(&root),
            SdCppChoice { build: Some("cpu".to_owned()), device: SdCppDeviceChoice::Auto, threads: 8 }
        );
    }

    #[test]
    fn device_config_value_round_trips() {
        let named = SdCppDeviceChoice::Named { name: "CPU".to_owned(), description: String::new() };
        for device in [SdCppDeviceChoice::Auto, named] {
            assert_eq!(SdCppDeviceChoice::from_config_value(&device.to_config_value()), Some(device.clone()), "{device:?}");
        }
        assert_eq!(SdCppDeviceChoice::Auto.to_config_value(), json!("auto"));
    }

    #[test]
    fn default_tree_carries_device_and_threads_but_no_build() {
        let defaults = crate::user_config_defaults();
        let general = &defaults["General"];
        assert_eq!(general[GENERAL_SDCPP_DEVICE_KEY], json!("auto"));
        assert_eq!(general[GENERAL_SDCPP_THREADS_KEY], json!(0));
        assert!(general.get(GENERAL_SDCPP_BUILD_KEY).is_none());
        assert_eq!(sdcpp_choice_from_user_settings(&defaults), SdCppChoice::default());
    }

    /// A unique scratch path under the system temp dir (never the real `user_config`).
    fn temp_config_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ms_sdcpp_choice_{tag}_{}_{:?}.json", std::process::id(), std::thread::current().id()))
    }

    #[test]
    fn savers_round_trip_on_a_temp_document_and_keep_siblings() {
        let path = temp_config_path("round_trip");
        if path.exists() {
            std::fs::remove_file(&path).expect("remove a stale scratch config");
        }
        crate::save_onnx_build(&path, "cpu").expect("seed a sibling key");

        let device = SdCppDeviceChoice::Named { name: "Vulkan0".to_owned(), description: "GPU 0".to_owned() };
        save_sdcpp_build(&path, Some("vulkan")).expect("save build");
        save_sdcpp_device(&path, &device).expect("save device");
        save_sdcpp_threads(&path, 6).expect("save threads");
        let read = |path: &Path| -> Value {
            let raw = std::fs::read_to_string(path).expect("scratch config written");
            serde_json::from_str(&raw).expect("scratch config is valid json")
        };
        let root = read(&path);
        assert_eq!(
            sdcpp_choice_from_user_settings(&root),
            SdCppChoice { build: Some("vulkan".to_owned()), device, threads: 6 }
        );
        assert_eq!(crate::ai_onnx_build_from_user_settings(&root).as_deref(), Some("cpu"));

        save_sdcpp_build(&path, None).expect("clear build");
        assert!(read(&path)["General"].get(GENERAL_SDCPP_BUILD_KEY).is_none());

        std::fs::remove_file(&path).expect("remove the scratch config");
    }

    #[test]
    fn image_edit_runtime_wire_ids_round_trip_and_are_frozen() {
        let ids: Vec<&str> = ImageEditRuntime::ALL.iter().map(|runtime| runtime.wire()).collect();
        assert_eq!(ids, ["pytorch", "sdcpp"]);
        for runtime in ImageEditRuntime::ALL {
            assert_eq!(ImageEditRuntime::from_wire(runtime.wire()), Some(runtime));
        }
        assert_eq!(ImageEditRuntime::from_wire(" sdcpp "), Some(ImageEditRuntime::SdCpp));
        assert_eq!(ImageEditRuntime::from_wire("SdCpp"), None);
        assert_eq!(ImageEditRuntime::from_wire("torch"), None);
        assert_eq!(ImageEditRuntime::from_wire(""), None);
        assert_eq!(ImageEditRuntime::default(), ImageEditRuntime::PyTorch);
    }
}
