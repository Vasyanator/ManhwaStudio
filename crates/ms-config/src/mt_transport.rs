/*
File: crates/ms-config/src/mt_transport.rs

Purpose:
The persisted per-service choice of "how a machine-translation service is reached": through
the program's own HTTP requests (`Builtin`) or through the user's browser extension
(`Browser`). The vocabulary, a pure reader and one blocking writer.

Key structures:
- MtTransport: `Builtin` / `Browser`, with the frozen tokens `"builtin"` / `"browser"`.

Key functions:
- MtTransport::token / MtTransport::from_token
- transport_from_user_settings: pure reader over a loaded `user_config` root.
- save_mt_transport: one `ms_docstore::update` of `user_config` (blocking; worker threads only).

Notes:
- Stored at `TranslarionTab.MachineTranslation.transport.<service key>` in `user_config`
  (the section name's misspelling is the real persisted key; never "fix" it), e.g.
  `{"TranslarionTab": {"MachineTranslation": {"transport": {"deepl": "browser"}}}}`.
  `<service key>` is `MtService::key()` of `ms-tab-translation`, passed in as a string: this
  crate does not know the services.
- `transport` is deliberately NOT in `user_config_defaults()`: absent (and an unknown token)
  reads as `Builtin`, the behaviour of every build before the setting existed.
- Rust-only key: the Python backend never reads it, so it is outside `PROTOCOL_VERSION`.
- The same enum selects the machine-translation limiter's lane in `ms-tab-translation`.
- The tokens are persistence values: never rename one.
*/

use std::path::Path;

use ms_log::runtime_log;
use serde_json::Value;

use crate::{edit_section, update_user_config_root, user_config_error_message};

/// The `user_config` section holding the machine-translation settings (misspelled on disk).
pub const TRANSLATION_TAB_SECTION: &str = "TranslarionTab";
/// The machine-translation object inside [`TRANSLATION_TAB_SECTION`].
pub const MACHINE_TRANSLATION_KEY: &str = "MachineTranslation";
/// The per-service transport object inside [`MACHINE_TRANSLATION_KEY`].
pub const TRANSPORT_KEY: &str = "transport";

/// How one machine-translation service is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MtTransport {
    /// The program's own HTTP requests (the default, and the reading of an absent or unknown
    /// stored value).
    #[default]
    Builtin,
    /// The user's own browser, driven through the ManhwaStudio browser extension.
    Browser,
}

impl MtTransport {
    /// Every transport, in display order.
    pub const ALL: [Self; 2] = [Self::Builtin, Self::Browser];

    /// The frozen config token (persisted; never rename).
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Browser => "browser",
        }
    }

    /// Parses a frozen config token (exact, case-sensitive); `None` for anything else.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|transport| transport.token() == token)
    }
}

/// Reads the transport of the service `service_key` (`MtService::key()`) out of a loaded
/// `user_config` root. Absent anywhere along the path resolves silently to `Builtin`; a present
/// value that is not a known token resolves to `Builtin` with a warning (never an error: a bad
/// value must not stop translation).
#[must_use]
pub fn transport_from_user_settings(user_settings: &Value, service_key: &str) -> MtTransport {
    let Some(raw) = user_settings.get(TRANSLATION_TAB_SECTION).and_then(|tab| tab.get(MACHINE_TRANSLATION_KEY)).and_then(|mt| mt.get(TRANSPORT_KEY)).and_then(|transport| transport.get(service_key))
    else {
        return MtTransport::default();
    };
    raw.as_str().and_then(MtTransport::from_token).unwrap_or_else(|| {
        runtime_log::log_warn(format!(
            "[mt-transport] invalid {TRANSLATION_TAB_SECTION}.{MACHINE_TRANSLATION_KEY}.{TRANSPORT_KEY}.{service_key} value {raw} (expected \"builtin\" / \"browser\"); using builtin"
        ));
        MtTransport::default()
    })
}

/// Persists `transport` for the service `service_key` into the `user_config` document at
/// `user_settings_file` (production: `crate::user_config_path()`), as ONE serialized
/// read-modify-write. Every other key survives; a non-object value along the path is replaced
/// by an object. Blocking disk I/O: call from a worker thread, never the GUI thread.
///
/// # Errors
/// A user-facing message when the document cannot be read, does not parse (it is then left
/// untouched) or cannot be written.
pub fn save_mt_transport(user_settings_file: &Path, service_key: &str, transport: MtTransport) -> Result<(), String> {
    update_user_config_root(user_settings_file, ms_docstore::Durability::Contents, |root_obj| {
        edit_section(root_obj, TRANSLATION_TAB_SECTION, |tab_obj| {
            edit_section(tab_obj, MACHINE_TRANSLATION_KEY, |mt_obj| {
                edit_section(mt_obj, TRANSPORT_KEY, |transport_obj| {
                    transport_obj.insert(service_key.to_owned(), Value::String(transport.token().to_owned()));
                });
            });
        });
    })
    .map_err(|err| user_config_error_message(user_settings_file, &err))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    #[test]
    fn tokens_are_frozen_and_round_trip() {
        assert_eq!(MtTransport::Builtin.token(), "builtin");
        assert_eq!(MtTransport::Browser.token(), "browser");
        for transport in MtTransport::ALL {
            assert_eq!(MtTransport::from_token(transport.token()), Some(transport));
        }
        assert_eq!(MtTransport::from_token("Browser"), None);
        assert_eq!(MtTransport::from_token(""), None);
        assert_eq!(MtTransport::default(), MtTransport::Builtin);
    }

    #[test]
    fn reader_defaults_to_builtin_when_absent() {
        for root in [
            json!({}),
            json!({"TranslarionTab": {}}),
            json!({"TranslarionTab": {"MachineTranslation": {"service": "deepl"}}}),
            json!({"TranslarionTab": {"MachineTranslation": {"transport": {}}}}),
            json!({"TranslarionTab": {"MachineTranslation": {"transport": {"google": "browser"}}}}),
            json!({"TranslarionTab": "corrupt"}),
        ] {
            assert_eq!(transport_from_user_settings(&root, "deepl"), MtTransport::Builtin, "{root}");
        }
    }

    #[test]
    fn reader_maps_unknown_tokens_to_builtin_and_honours_known_ones() {
        let read = |value: Value| transport_from_user_settings(&json!({"TranslarionTab": {"MachineTranslation": {"transport": {"deepl": value}}}}), "deepl");
        assert_eq!(read(json!("browser")), MtTransport::Browser);
        assert_eq!(read(json!("builtin")), MtTransport::Builtin);
        assert_eq!(read(json!("BROWSER")), MtTransport::Builtin);
        assert_eq!(read(json!("extension")), MtTransport::Builtin);
        assert_eq!(read(json!(true)), MtTransport::Builtin);
        assert_eq!(read(Value::Null), MtTransport::Builtin);
    }

    /// A unique scratch `user_config` under the system temp dir (never the real one).
    fn temp_config_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ms_mt_transport_{tag}_{}_{:?}.json", std::process::id(), std::thread::current().id()))
    }

    fn read_root(path: &Path) -> Value {
        let raw = fs::read_to_string(path).unwrap_or_else(|err| panic!("config file written: {err}"));
        serde_json::from_str(&raw).unwrap_or_else(|err| panic!("config file is valid json: {err}"))
    }

    #[test]
    fn writer_round_trips_per_service_and_keeps_sibling_keys() {
        let path = temp_config_path("round_trip");
        // A stale file of an earlier crashed run would make the test order-dependent.
        if path.exists() {
            fs::remove_file(&path).unwrap_or_else(|err| panic!("remove stale {}: {err}", path.display()));
        }
        let seed = json!({"General": {"ui_language": "ru"}, "TranslarionTab": {"MachineTranslation": {"service": "deepl", "target_lang": "ru"}}});
        fs::write(&path, seed.to_string()).unwrap_or_else(|err| panic!("seed config: {err}"));

        save_mt_transport(&path, "deepl", MtTransport::Browser).unwrap_or_else(|err| panic!("save deepl: {err}"));
        save_mt_transport(&path, "google", MtTransport::Browser).unwrap_or_else(|err| panic!("save google: {err}"));
        save_mt_transport(&path, "google", MtTransport::Builtin).unwrap_or_else(|err| panic!("save google back: {err}"));

        let root = read_root(&path);
        assert_eq!(transport_from_user_settings(&root, "deepl"), MtTransport::Browser);
        assert_eq!(transport_from_user_settings(&root, "google"), MtTransport::Builtin);
        assert_eq!(transport_from_user_settings(&root, "yandex"), MtTransport::Builtin);
        assert_eq!(root["TranslarionTab"]["MachineTranslation"]["transport"], json!({"deepl": "browser", "google": "builtin"}));
        assert_eq!(root["TranslarionTab"]["MachineTranslation"]["service"], json!("deepl"));
        assert_eq!(root["TranslarionTab"]["MachineTranslation"]["target_lang"], json!("ru"));
        assert_eq!(root["General"]["ui_language"], json!("ru"));

        fs::remove_file(&path).unwrap_or_else(|err| panic!("remove {}: {err}", path.display()));
    }
}
