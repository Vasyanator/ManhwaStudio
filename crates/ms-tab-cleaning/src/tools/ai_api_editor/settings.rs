/*
File: ai_api_editor/settings.rs

Purpose:
Everything the cloud engine persists to `ms_config::ai_api_edit_settings_path()` — the selected
provider, each visited provider's model id and endpoint (region id or server address), the
prompt and the mask blend — plus the file IO. The save gate is the shared
`region_edit_v2::engine_settings::settings_save_due`.

Key structures:
- `ApiEditSettings`, `StoredChoice`
- `SettingsSaveError`

Key functions:
- `load_api_edit_settings()`, `save_api_edit_settings()`
- `suppress_settings_persistence_for_tests()`: the test-only door that arms the sticky latch
- `ApiEditSettings::{capture, selection, blend}`: the bridge to the engine's live state

Notes:
NEVER an API key: keys live only in the OS credential store (`ms_ai_api::image_edit::keys`).
Providers are stored by their frozen `ImageEditProvider::key()`; an unknown key in a hand-edited
or newer file is skipped, never mapped onto another provider. The blend is clamped on load.
The file IO runs on worker threads only, and a test process never writes the file (the file is
the user's): `save_api_edit_settings` refuses under `cfg!(test)` OR once the sticky test latch
is armed (`persistence_suppressed_by_tests`, the pattern of
`ms-tab-typing/src/panel/font_settings_store.rs`).
*/

use ms_ai_api::image_edit::{ImageEditProvider, ImageEditSelection, MaskBlend, ProviderChoice};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};

/// Current format version written to the file. Read back only for diagnostics: every field is
/// `#[serde(default)]`, so an older or partial document loads with defaults for what it lacks.
const SETTINGS_VERSION: u32 = 1;

/// Largest dilate / feather radius the panel offers and the loader accepts, in region pixels.
pub(super) const BLEND_RADIUS_MAX_PX: u32 = 64;

/// One provider's persisted choice (mirrors `ms_ai_api::image_edit::ProviderChoice`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct StoredChoice {
    /// The model id as the provider's API expects it.
    pub model_id: String,
    /// Region id, server address, or empty for a fixed endpoint.
    pub endpoint: String,
}

/// The engine's persisted state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct ApiEditSettings {
    /// Format version of the document.
    pub version: u32,
    /// `ImageEditProvider::key()` of the selected provider.
    pub provider: String,
    /// Each visited provider's choice, by `ImageEditProvider::key()`.
    pub choices: BTreeMap<String, StoredChoice>,
    /// The edit instruction.
    pub prompt: String,
    /// `MaskBlend::dilate_px`.
    pub dilate_px: u32,
    /// `MaskBlend::feather_px`.
    pub feather_px: u32,
}

impl Default for ApiEditSettings {
    fn default() -> Self {
        let blend = MaskBlend::default();
        Self {
            version: SETTINGS_VERSION,
            provider: default_provider().key().to_string(),
            choices: BTreeMap::new(),
            prompt: String::new(),
            dilate_px: blend.dilate_px,
            feather_px: blend.feather_px,
        }
    }
}

/// The provider a fresh install starts with: the head of the catalogue's provider order.
fn default_provider() -> ImageEditProvider {
    ImageEditProvider::ALL[0]
}

impl ApiEditSettings {
    /// The document for the engine's live state.
    #[must_use]
    pub(super) fn capture(selection: &ImageEditSelection, prompt: &str, blend: MaskBlend) -> Self {
        let choices = selection
            .choices
            .iter()
            .map(|(provider, choice)| (provider.key().to_string(), StoredChoice { model_id: choice.model_id.clone(), endpoint: choice.endpoint.clone() }))
            .collect();
        Self {
            version: SETTINGS_VERSION,
            provider: selection.provider.key().to_string(),
            choices,
            prompt: prompt.to_string(),
            dilate_px: blend.dilate_px.min(BLEND_RADIUS_MAX_PX),
            feather_px: blend.feather_px.min(BLEND_RADIUS_MAX_PX),
        }
    }

    /// The picker selection this document describes, with the selected provider's empty fields
    /// filled with defaults. Unknown provider keys are skipped; an unknown selected provider
    /// falls back to the default provider (the other providers' choices are kept).
    #[must_use]
    pub(super) fn selection(&self) -> ImageEditSelection {
        let provider = ImageEditProvider::from_key(&self.provider).unwrap_or_else(default_provider);
        let mut selection = ImageEditSelection::new(provider);
        for (key, stored) in &self.choices {
            if let Some(provider) = ImageEditProvider::from_key(key) {
                selection.choices.insert(provider, ProviderChoice { model_id: stored.model_id.clone(), endpoint: stored.endpoint.clone() });
            }
        }
        // The stored choice may have empty fields (or none at all for the selected provider).
        selection.fill_defaults();
        selection
    }

    /// The mask blend, clamped to the offered range.
    #[must_use]
    pub(super) fn blend(&self) -> MaskBlend {
        MaskBlend { dilate_px: self.dilate_px.min(BLEND_RADIUS_MAX_PX), feather_px: self.feather_px.min(BLEND_RADIUS_MAX_PX) }
    }
}

/// Why the settings file could not be written. Logged by the saver; never shown as a dialog.
#[derive(Debug, thiserror::Error)]
pub(super) enum SettingsSaveError {
    #[error("could not create the settings directory {path}: {source}")]
    CreateDir { path: String, source: std::io::Error },
    #[error("could not serialize the settings: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("could not write {path}: {source}")]
    Write { path: String, source: std::io::Error },
}

/// Reads the settings file. A missing file is the first-run case and yields the defaults
/// silently; an unreadable or malformed one yields the defaults and a warning in the log (the
/// next user edit then replaces it). Runs on a worker thread.
#[must_use]
pub(super) fn load_api_edit_settings() -> ApiEditSettings {
    let path = ms_config::ai_api_edit_settings_path();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return ApiEditSettings::default(),
        Err(error) => {
            ms_log::runtime_log::log_warn(format!("[cleaning/ai_api_editor] could not read the settings file {}: {error}; using defaults", path.display()));
            return ApiEditSettings::default();
        }
    };
    match serde_json::from_str::<ApiEditSettings>(&raw) {
        Ok(settings) => settings,
        Err(error) => {
            ms_log::runtime_log::log_warn(format!("[cleaning/ai_api_editor] the settings file {} is malformed: {error}; using defaults", path.display()));
            ApiEditSettings::default()
        }
    }
}

/// `true` once this process has been identified as a TEST process; never cleared.
///
/// Why a runtime latch and not `cfg!(test)` alone: `cfg!(test)` holds only while THIS crate is
/// its own test target. Another crate's test binary (the app's, which builds the cleaning tab
/// and with it this tool) links this crate as a plain dependency, where `cfg!(test)` is false
/// and a save would write the developer's real settings file. A `test-support` feature gate on
/// the save itself would not do either: `cargo build --all-targets` unifies dev-dependency
/// features into the runnable binary, which would then silently stop saving. So the feature
/// only exposes the door (`suppress_settings_persistence_for_tests`), and the door arms this
/// latch at runtime.
static TEST_PROCESS_LATCH: AtomicBool = AtomicBool::new(false);

/// Marks this process as a test process: from now on the cloud-edit tool never writes its
/// settings file. Sticky. For tests only: this crate's own (`cfg(test)`) and, through the
/// `test-support` feature enabled from a `[dev-dependencies]` entry, any other crate's test
/// that builds the cleaning tab.
#[cfg(any(test, feature = "test-support"))]
pub fn suppress_settings_persistence_for_tests() {
    TEST_PROCESS_LATCH.store(true, Ordering::Release);
}

/// Whether the settings file must not be written because this process runs tests:
/// `cfg!(test)` for this crate's own tests, the latch for every other crate's.
#[must_use]
fn persistence_suppressed_by_tests() -> bool {
    cfg!(test) || TEST_PROCESS_LATCH.load(Ordering::Acquire)
}

/// Writes `settings` to the settings file. Runs on a worker thread. A test process writes
/// nothing (`persistence_suppressed_by_tests`): the path is the user's real data directory.
///
/// # Errors
/// `CreateDir` / `Write` on IO failure, `Serialize` when the document cannot be encoded.
pub(super) fn save_api_edit_settings(settings: &ApiEditSettings) -> Result<(), SettingsSaveError> {
    if persistence_suppressed_by_tests() {
        return Ok(());
    }
    let path = ms_config::ai_api_edit_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SettingsSaveError::CreateDir { path: parent.display().to_string(), source })?;
    }
    let raw = serde_json::to_string_pretty(settings)?;
    fs::write(&path, raw).map_err(|source| SettingsSaveError::Write { path: path.display().to_string(), source })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live state -> document -> live state keeps every provider's choice, the prompt and the
    /// blend; the JSON round trip is lossless.
    #[test]
    fn the_document_round_trips_the_live_state() {
        let mut selection = ImageEditSelection::new(ImageEditProvider::OpenAi);
        selection.select_provider(ImageEditProvider::DashScope);
        let blend = MaskBlend { dilate_px: 7, feather_px: 3 };
        let document = ApiEditSettings::capture(&selection, "remove the text", blend);
        let parsed: ApiEditSettings = serde_json::from_str(&serde_json::to_string(&document).expect("serialize")).expect("deserialize");
        assert_eq!(parsed, document);

        let restored = parsed.selection();
        assert_eq!(restored.provider, ImageEditProvider::DashScope);
        assert_eq!(restored.choices, selection.choices, "every visited provider's choice survives");
        assert_eq!(parsed.blend(), blend);
        assert_eq!(parsed.prompt, "remove the text");
        assert!(!serde_json::to_string(&document).expect("serialize").contains("key\""), "no key field is ever written");
    }

    /// A partial, hand-edited or newer document loads: missing fields default, an unknown
    /// provider is skipped (never mapped onto another), out-of-range blend values are clamped.
    #[test]
    fn a_partial_or_foreign_document_loads_with_defaults() {
        let parsed: ApiEditSettings = serde_json::from_str(r#"{"provider":"martian_ai","choices":{"martian_ai":{"model_id":"x"},"openai":{"model_id":"gpt-image-2"}},"dilate_px":1000}"#).expect("parses");
        let selection = parsed.selection();
        assert_eq!(selection.provider, default_provider());
        assert!(selection.choices.keys().all(|provider| ImageEditProvider::ALL.contains(provider)));
        assert_eq!(selection.choices.get(&ImageEditProvider::OpenAi).map(|choice| choice.model_id.as_str()), Some("gpt-image-2"));
        assert_eq!(parsed.blend().dilate_px, BLEND_RADIUS_MAX_PX);
        assert_eq!(parsed.blend().feather_px, MaskBlend::default().feather_px);
        assert!(parsed.prompt.is_empty());
    }

    /// The test build never touches the user's file.
    #[test]
    fn a_test_build_never_writes_the_settings_file() {
        let path = ms_config::ai_api_edit_settings_path();
        let before = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        assert!(save_api_edit_settings(&ApiEditSettings::default()).is_ok());
        let after = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        assert_eq!(before, after);
    }

    /// The test-only door arms the sticky latch that suppresses saving in a test binary where
    /// `cfg!(test)` is false (another crate's).
    #[test]
    fn the_test_door_arms_the_sticky_latch() {
        suppress_settings_persistence_for_tests();
        assert!(TEST_PROCESS_LATCH.load(Ordering::Acquire));
        assert!(persistence_suppressed_by_tests());
    }
}
