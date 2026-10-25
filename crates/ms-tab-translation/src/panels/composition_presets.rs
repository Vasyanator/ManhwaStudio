/*
File: crates/ms-tab-translation/src/panels/composition_presets.rs

Purpose:
Named presets of the composition panel's options («Компоновка» › «Параметры»): the built-in
presets («Экспорт перевода», «Перевод через ИИ») with their factory values, the app-wide preset
store (built-in overrides + user presets), the per-title selection codec, the
`ms_widgets::PresetPicker` block drawn at the top of the Params sub-tab, and the ordered
off-thread writer (`ms_config::preset_store::OrderedWriter`) that persists every preset
operation into `user_config`.

Key structures:
- BuiltinCompositionPreset: `TranslationExport` / `AiTranslation`, with a stable persistence
  `id()`, a localized `display_name()` and `factory()` = THE table of their values.
- CompositionPresetRef: one preset's identity (a built-in by id, a user preset by name): the
  per-title selection and the failed-write bookkeeping key.
- CompositionPresets: built-in overrides + user presets, the selection, the picker state, the
  validation and write-error lines, the entries whose last write failed and the lazily started
  writer.
- PresetWrite: one persisted operation, applied the same way in memory and on disk.
- PresetStoreError: why an operation was refused (a store written by a newer build).
- WriteOutcome: the writer's answer for one operation (its entries, success or the error).

Key functions:
- CompositionPresets::from_user_settings(): seeds the store from the pre-read `user_config`.
- CompositionPresets::draw(): the picker block plus its status line.
- CompositionPresets::restore_selection() / selection(): the per-title selection.
- read_selected_preset() / write_selected_preset(): the title's `composition.preset` codec.
- apply_preset_write(): the pure `user_config` mutation of one operation.

Notes:
- A preset value is a whole `CompositionPanelOptions` in `stored_form` (sorting included);
  its JSON is the ONE codec of `composition.rs`, so a preset entry has exactly the keys of
  the title `composition` section.
- Storage: `user_config` › `TranslarionTab` (misspelled, frozen) › `composition_presets` =
  `{"version": 1, "builtin": {"<id>": {…}}, "user": {"<name>": {…}}}`. Rust-only: the Python
  backend never reads it (outside `PROTOCOL_VERSION`). A store WITHOUT a numeric `version` is
  the earlier flat shape `{"<name>": {…}}`: every entry is a user preset (a built-in's display
  name included — such an entry is an ordinary user preset, never an override), and the first
  write moves the entries verbatim under `user`. An unknown numeric `version` (a newer build)
  is read as empty and never written: every operation is refused with a status line.
  Each operation is its own read-modify-write, run IN ORDER on one writer thread, so another
  app instance's presets survive and a quick save-then-delete can never land reversed.
- Built-ins are identified by `id()` everywhere (overrides, selection, bookkeeping); the shown
  name is a translation, so switching the UI language renames the row, never the preset. An
  override exists only while it differs from the factory values. «Delete» on a modified
  built-in removes the override (factory values come back, the selection stays); an unmodified
  built-in has no «Delete»; a built-in cannot be renamed (`PresetPicker::rename(false)`) and
  «Save» commits only its values. A user preset may not be created or renamed to a built-in's
  display name in the CURRENT language; one that already carries such a name (another language,
  or the flat store) is listed next to the built-in, which keeps its own row.
- Failed writes: the in-memory store is ahead of the disk after a failed write. An entry whose
  LAST write failed stays "write failed" until a later write of that same entry succeeds (each
  write stores the entry's current in-memory state, so a later success makes the disk match):
  while it is selected the picker's unsaved warning stays raised («Save» retries the write), and
  the write-error line stays under the picker while ANY entry is failed (a failed delete has no
  row left to mark). A selection change clears only the validation line, never the write error.
- The selection is remembered per title (`composition.preset` in `settings.json`, written by
  the tab's settings saver) as `{"builtin": "<id>"}` / `{"user": "<name>"}`; a plain string (the
  earlier form) is a user preset name; a user preset that no longer exists reads as «Нет».
*/

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use eframe::egui;
use ms_config::mt_transport::TRANSLATION_TAB_SECTION;
use ms_config::preset_store::{OrderedWriter, SubmitError, free_default_name};
use ms_log::runtime_log;
use ms_widgets::{PresetPicker, PresetPickerAction, PresetPickerState};
use serde_json::{Map, Value};

use crate::panels::composition::{
    CharacterNamePosition, CompositionPanelActions, CompositionPanelOptions, CompositionSortMethod, CompositionSourceMode, read_composition_options_json,
    write_composition_options_json,
};

/// Key of the preset store inside the `TranslarionTab` section of `user_config` (persisted).
pub const COMPOSITION_PRESETS_KEY: &str = "composition_presets";

/// Key of the selected preset inside the title `settings.json` `composition` section
/// (persisted; absent = «Нет»).
pub const SELECTED_PRESET_KEY: &str = "preset";

/// The store format version this build reads and writes.
const STORE_VERSION: u64 = 1;

/// Store keys of the versioned shape (persisted).
const STORE_VERSION_KEY: &str = "version";
const STORE_BUILTIN_KEY: &str = "builtin";
const STORE_USER_KEY: &str = "user";

/// i18n key of the combo label, also the picker's stable id salt.
const PRESET_LABEL_KEY: &str = "translation.composition.preset_label";

/// A factory preset. Its [`Self::id`] is the persistence key; its name is a translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BuiltinCompositionPreset {
    /// «Экспорт перевода»: the translation as a readable script (names before the line,
    /// image bubbles included, no limit, no wrapping).
    TranslationExport,
    /// «Перевод через ИИ»: the original text prepared for an AI translator prompt.
    AiTranslation,
}

impl BuiltinCompositionPreset {
    /// Every built-in in combo order.
    pub(crate) const ALL: [Self; 2] = [Self::TranslationExport, Self::AiTranslation];

    /// The persistence id (`"translation_export"` / `"ai_translation"`); never rename one.
    #[must_use]
    pub(crate) const fn id(self) -> &'static str {
        match self {
            Self::TranslationExport => "translation_export",
            Self::AiTranslation => "ai_translation",
        }
    }

    /// The built-in with persistence id `raw` (exact match), or `None`.
    #[must_use]
    pub(crate) fn from_id(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == raw)
    }

    /// The localized name shown in the combo (follows the UI language).
    #[must_use]
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::TranslationExport => t!("translation.composition.preset_builtin_translation_export_label"),
            Self::AiTranslation => t!("translation.composition.preset_builtin_ai_translation_label"),
        }
    }

    /// The factory values, in `stored_form`. Spelled out field by field (no `..Default`), so a
    /// change of the panel defaults never silently changes a built-in and a new option field is
    /// a compile error here.
    #[must_use]
    pub(crate) fn factory(self) -> CompositionPanelOptions {
        let options = match self {
            Self::TranslationExport => CompositionPanelOptions {
                sort_method: CompositionSortMethod::Height,
                source_mode: CompositionSourceMode::Translation,
                ignore_translated_lines: true,
                merge_same_character: true,
                sep_same_character: "\\n".to_string(),
                sep_between: "\\n\\n".to_string(),
                replica_prefix: "- ".to_string(),
                nl_replace: " ".to_string(),
                nl_replace_enabled: false,
                wrap_with: "``".to_string(),
                wrap_with_enabled: false,
                limit: 650,
                limit_enabled: false,
                use_character_names: true,
                character_name_position: CharacterNamePosition::Before,
                character_name_prefix: String::new(),
                character_name_suffix: ": ".to_string(),
                include_image_bubbles: true,
                include_hint_bubbles: true,
                hint_wrap: "()".to_string(),
                hint_wrap_enabled: true,
                hint_extra_sep: String::new(),
                jinja2_enabled: false,
                jinja2_template: String::new(),
            },
            Self::AiTranslation => CompositionPanelOptions {
                sort_method: CompositionSortMethod::Height,
                source_mode: CompositionSourceMode::Original,
                ignore_translated_lines: true,
                merge_same_character: true,
                sep_same_character: "\\n".to_string(),
                sep_between: "\\n\\n".to_string(),
                replica_prefix: String::new(),
                nl_replace: " ".to_string(),
                nl_replace_enabled: true,
                wrap_with: "``".to_string(),
                wrap_with_enabled: true,
                limit: 650,
                limit_enabled: true,
                use_character_names: true,
                character_name_position: CharacterNamePosition::After,
                character_name_prefix: " - ".to_string(),
                character_name_suffix: String::new(),
                include_image_bubbles: false,
                include_hint_bubbles: true,
                hint_wrap: "()".to_string(),
                hint_wrap_enabled: true,
                hint_extra_sep: String::new(),
                jinja2_enabled: false,
                jinja2_template: String::new(),
            },
        };
        options.stored_form()
    }
}

/// One preset's identity: a built-in by id or a user preset by its verbatim name. It is the
/// per-title selection (`composition.preset`) and the failed-write bookkeeping key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CompositionPresetRef {
    /// Stored as `{"builtin": "<id>"}`.
    Builtin(BuiltinCompositionPreset),
    /// Stored as `{"user": "<name>"}` (a plain string is read as this too).
    User(String),
}

impl CompositionPresetRef {
    /// The persisted JSON form (see the variants).
    #[must_use]
    fn to_json(&self) -> Value {
        match self {
            Self::Builtin(preset) => serde_json::json!({ STORE_BUILTIN_KEY: preset.id() }),
            Self::User(name) => serde_json::json!({ STORE_USER_KEY: name }),
        }
    }

    /// Decodes [`Self::to_json`]'s shapes and the earlier plain-name string; `None` for
    /// anything else (an unknown built-in id too).
    #[must_use]
    fn from_json(raw: &Value) -> Option<Self> {
        if let Some(name) = raw.as_str() {
            return Some(Self::User(name.to_string()));
        }
        let obj = raw.as_object().filter(|obj| obj.len() == 1)?;
        if let Some(id) = obj.get(STORE_BUILTIN_KEY) {
            return id.as_str().and_then(BuiltinCompositionPreset::from_id).map(Self::Builtin);
        }
        obj.get(STORE_USER_KEY).and_then(Value::as_str).map(|name| Self::User(name.to_string()))
    }

    /// The combo row's visible name.
    #[must_use]
    fn label(&self) -> String {
        match self {
            Self::Builtin(preset) => preset.display_name().to_string(),
            Self::User(name) => name.clone(),
        }
    }
}

/// Reads the title's selected preset out of its `composition` section; `None` when absent or
/// malformed (the latter is logged).
#[must_use]
pub(crate) fn read_selected_preset(composition: &Map<String, Value>) -> Option<CompositionPresetRef> {
    let raw = composition.get(SELECTED_PRESET_KEY)?;
    let selection = CompositionPresetRef::from_json(raw);
    if selection.is_none() {
        runtime_log::log_warn(format!("[translation::composition_presets] malformed composition.{SELECTED_PRESET_KEY} in the title settings; reading it as no preset; value={raw}"));
    }
    selection
}

/// Writes (or, for `None`, removes) the title's selected preset in its `composition` section.
pub(crate) fn write_selected_preset(composition: &mut Map<String, Value>, selected: Option<&CompositionPresetRef>) {
    match selected {
        Some(selection) => {
            composition.insert(SELECTED_PRESET_KEY.to_string(), selection.to_json());
        }
        None => {
            composition.remove(SELECTED_PRESET_KEY);
        }
    }
}

/// Why a store operation was refused (nothing changed, in memory or on disk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PresetStoreError {
    /// The store carries a numeric `version` this build does not know (a newer build wrote it):
    /// it is read as empty and never written, so the newer presets survive. `version` is the raw
    /// JSON value.
    UnsupportedVersion { version: String },
}

impl std::fmt::Display for PresetStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { version } => write!(
                f,
                "user_config {TRANSLATION_TAB_SECTION}.{COMPOSITION_PRESETS_KEY} has format version {version}, this build writes only version {STORE_VERSION}"
            ),
        }
    }
}

impl std::error::Error for PresetStoreError {}

/// The shape of a stored preset store object.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StoreShape {
    /// No numeric `version`: the earlier flat `{name: {…}}` map, every entry a user preset.
    Flat,
    /// `version` == [`STORE_VERSION`].
    Current,
    /// Any other numeric `version` (raw JSON value).
    Unsupported(String),
}

/// Classifies a store object. A non-numeric `version` value is a flat-store user preset named
/// "version", not a format marker.
#[must_use]
fn store_shape(store: &Map<String, Value>) -> StoreShape {
    match store.get(STORE_VERSION_KEY) {
        Some(version) if version.is_number() => {
            if version.as_u64() == Some(STORE_VERSION) {
                StoreShape::Current
            } else {
                StoreShape::Unsupported(version.to_string())
            }
        }
        Some(_) | None => StoreShape::Flat,
    }
}

/// One persisted preset operation, applied the same way in memory
/// ([`CompositionPresets::apply`]) and on disk ([`apply_preset_write`]). Values are in
/// `stored_form`. Name validation (empty, taken) is the caller's: every operation is total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PresetWrite {
    /// Inserts or replaces the user preset `name` (taken verbatim).
    UpsertUser { name: String, options: CompositionPanelOptions },
    /// Removes the user preset `from` and stores `options` under `to`, in one step.
    RenameUser { from: String, to: String, options: CompositionPanelOptions },
    /// Removes the user preset `name`; an absent one is not an error.
    RemoveUser { name: String },
    /// Stores `options` as the override of `preset`; the factory values clear the override
    /// instead (no copy of the factory values is ever stored).
    SaveBuiltin { preset: BuiltinCompositionPreset, options: CompositionPanelOptions },
    /// Clears the override of `preset`: its factory values come back.
    ResetBuiltin { preset: BuiltinCompositionPreset },
}

impl PresetWrite {
    /// The store entries this operation writes (both names of a rename).
    #[must_use]
    fn entries(&self) -> Vec<CompositionPresetRef> {
        match self {
            Self::UpsertUser { name, .. } | Self::RemoveUser { name } => vec![CompositionPresetRef::User(name.clone())],
            Self::RenameUser { from, to, .. } => vec![CompositionPresetRef::User(from.clone()), CompositionPresetRef::User(to.clone())],
            Self::SaveBuiltin { preset, .. } | Self::ResetBuiltin { preset } => vec![CompositionPresetRef::Builtin(*preset)],
        }
    }

    /// A one-line log description (kind and names, never the payload).
    #[must_use]
    fn describe(&self) -> String {
        match self {
            Self::UpsertUser { name, .. } => format!("upsert user preset {name:?}"),
            Self::RenameUser { from, to, .. } => format!("rename user preset {from:?} -> {to:?}"),
            Self::RemoveUser { name } => format!("remove user preset {name:?}"),
            Self::SaveBuiltin { preset, .. } => format!("save built-in preset {:?}", preset.id()),
            Self::ResetBuiltin { preset } => format!("reset built-in preset {:?}", preset.id()),
        }
    }
}

/// Applies `op` to a `user_config` root, editing ONLY the touched entry of
/// `TranslarionTab.composition_presets` (and setting its `version`): every other key, every
/// other preset and every entry the reader skipped as malformed stays as it is. A flat store is
/// first moved verbatim under `user`. A non-object value along the path is replaced by an object
/// (the same rule as the section writers of `ms-config`).
///
/// # Errors
/// [`PresetStoreError::UnsupportedVersion`] when the stored store has a format this build does
/// not know; `root` is then unchanged.
pub(crate) fn apply_preset_write(root: &mut Value, op: &PresetWrite) -> Result<(), PresetStoreError> {
    if let Some(store) = root.get(TRANSLATION_TAB_SECTION).and_then(|tab| tab.get(COMPOSITION_PRESETS_KEY)).and_then(Value::as_object)
        && let StoreShape::Unsupported(version) = store_shape(store)
    {
        return Err(PresetStoreError::UnsupportedVersion { version });
    }
    let mut root_obj = take_object(std::mem::take(root));
    edit_object(&mut root_obj, TRANSLATION_TAB_SECTION, |tab| {
        edit_object(tab, COMPOSITION_PRESETS_KEY, |store| {
            if store_shape(store) == StoreShape::Flat {
                // Every flat entry is a user preset; it moves under `user` byte for byte.
                let flat = std::mem::take(store);
                store.insert(STORE_USER_KEY.to_string(), Value::Object(flat));
            }
            store.insert(STORE_VERSION_KEY.to_string(), Value::from(STORE_VERSION));
            match op {
                PresetWrite::UpsertUser { name, options } => edit_object(store, STORE_USER_KEY, |user| {
                    user.insert(name.clone(), preset_json(options));
                }),
                PresetWrite::RenameUser { from, to, options } => edit_object(store, STORE_USER_KEY, |user| {
                    user.remove(from);
                    user.insert(to.clone(), preset_json(options));
                }),
                PresetWrite::RemoveUser { name } => edit_object(store, STORE_USER_KEY, |user| {
                    user.remove(name);
                }),
                PresetWrite::SaveBuiltin { preset, options } => edit_object(store, STORE_BUILTIN_KEY, |builtin| {
                    if options.stored_form() == preset.factory() {
                        builtin.remove(preset.id());
                    } else {
                        builtin.insert(preset.id().to_string(), preset_json(options));
                    }
                }),
                PresetWrite::ResetBuiltin { preset } => edit_object(store, STORE_BUILTIN_KEY, |builtin| {
                    builtin.remove(preset.id());
                }),
            }
        });
    });
    *root = Value::Object(root_obj);
    Ok(())
}

/// The object inside `value`, or an empty one for anything else.
fn take_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Edits the object under `key` of `parent` through `edit` (an absent or non-object value
/// becomes an empty object first). `serde_json` maps are key-sorted here (`preserve_order` is
/// never enabled), so taking an entry out and putting it back keeps the document's bytes stable.
fn edit_object(parent: &mut Map<String, Value>, key: &str, edit: impl FnOnce(&mut Map<String, Value>)) {
    let mut child = take_object(parent.remove(key).unwrap_or(Value::Null));
    edit(&mut child);
    parent.insert(key.to_string(), Value::Object(child));
}

/// The JSON object a preset is stored as.
#[must_use]
fn preset_json(options: &CompositionPanelOptions) -> Value {
    let mut obj = Map::new();
    write_composition_options_json(&mut obj, options);
    Value::Object(obj)
}

/// Decodes one stored preset entry over `base` (an absent key reads as `base`'s value).
/// `None` for a non-object entry.
#[must_use]
fn decode_preset(value: &Value, base: CompositionPanelOptions) -> Option<CompositionPanelOptions> {
    let obj = value.as_object()?;
    let mut options = base;
    read_composition_options_json(obj, &mut options);
    Some(options.stored_form())
}

/// Decodes the user presets of a `{name: {…}}` map into `out`. Malformed entries (an empty
/// name, a non-object value) are skipped with a warning and stay on disk untouched.
fn decode_user_presets(entries: &Map<String, Value>, out: &mut BTreeMap<String, CompositionPanelOptions>) {
    for (name, value) in entries {
        if name.is_empty() {
            runtime_log::log_warn(format!("[translation::composition_presets] skipped a user preset with an empty name in {TRANSLATION_TAB_SECTION}.{COMPOSITION_PRESETS_KEY}"));
            continue;
        }
        match decode_preset(value, CompositionPanelOptions::default()) {
            Some(options) => {
                out.insert(name.clone(), options);
            }
            None => runtime_log::log_warn(format!("[translation::composition_presets] skipped user preset {name:?}: its {TRANSLATION_TAB_SECTION}.{COMPOSITION_PRESETS_KEY} entry is not a JSON object")),
        }
    }
}

/// The writer's answer for one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteOutcome {
    /// The entries the operation wrote ([`PresetWrite::entries`]).
    entries: Vec<CompositionPresetRef>,
    /// `None` on success, else the technical error.
    error: Option<String>,
}

/// One job of the ordered writer: an operation and the `user_config` file it goes to.
type WriteJob = (PathBuf, PresetWrite);

/// Runs one operation against the document at `path` (blocking; the writer thread only).
///
/// # Errors
/// A technical message when the document cannot be read, does not parse (it is then left
/// untouched), carries an unknown store version (left untouched) or cannot be written.
fn persist_preset_write(path: &std::path::Path, op: &PresetWrite) -> Result<(), String> {
    ms_config::update_user_config_file(path, |root| apply_preset_write(root, op).map_err(Into::into)).map_err(|err| format!("{err:#}"))
}

/// The app-wide composition preset store and its picker. See the file header for the
/// storage, built-in and selection contracts.
#[derive(Debug)]
pub struct CompositionPresets {
    /// Edited built-ins (only while they differ from the factory values), in `stored_form`.
    overrides: BTreeMap<BuiltinCompositionPreset, CompositionPanelOptions>,
    /// User presets by verbatim name (byte-sorted), in `stored_form`.
    user: BTreeMap<String, CompositionPanelOptions>,
    /// `Some(raw version)` when the stored store has a format this build does not know: nothing
    /// was read and every operation is refused.
    unsupported_version: Option<String>,
    /// The selected preset; always an existing one or `None` («Нет»).
    selected: Option<CompositionPresetRef>,
    /// Rename buffer + armed delete of the picker; reset at every selection change.
    picker: PresetPickerState,
    /// The last refused operation (validation, read-only store), shown under the picker until
    /// the next selection change.
    error: Option<String>,
    /// The last write failure, shown while `failed` is not empty.
    write_error: Option<String>,
    /// Entries whose last write failed (see the file header).
    failed: BTreeSet<CompositionPresetRef>,
    /// Where writes go; `None` keeps every operation in memory (tests, the default state).
    store_path: Option<PathBuf>,
    /// The ordered writer (`ms_config::preset_store`); its thread starts on the first write and
    /// is joined when the store drops — on the studio's teardown worker, never the GUI thread.
    writer: OrderedWriter<WriteJob, WriteOutcome>,
}

impl Default for CompositionPresets {
    /// The built-ins with their factory values, no user presets, no persistence.
    fn default() -> Self {
        Self::from_parts(BTreeMap::new(), BTreeMap::new(), None, None)
    }
}

impl CompositionPresets {
    /// A store over already-decoded parts, persisting to `store_path` when given.
    fn from_parts(
        overrides: BTreeMap<BuiltinCompositionPreset, CompositionPanelOptions>,
        user: BTreeMap<String, CompositionPanelOptions>,
        unsupported_version: Option<String>,
        store_path: Option<PathBuf>,
    ) -> Self {
        Self {
            overrides,
            user,
            unsupported_version,
            selected: None,
            picker: PresetPickerState::default(),
            error: None,
            write_error: None,
            failed: BTreeSet::new(),
            store_path,
            writer: OrderedWriter::new("composition-presets-writer", run_write_job),
        }
    }

    /// Seeds the store from the pre-read `user_config` root (no file I/O here) and persists
    /// later operations to `store_path` (production: `ms_config::user_config_path()`; `None` keeps
    /// them in memory, which every test construction uses). Lenient: an absent store is empty;
    /// a non-object store, a non-object entry, an unknown built-in id and an empty user name are
    /// skipped with a warning (and left on disk as they are); a missing or wrongly typed field
    /// takes the base value (the built-in's factory value for an override, the panel default for
    /// a user preset). A flat store reads as user presets; an unknown `version` yields an empty
    /// store that refuses every operation.
    #[must_use]
    pub fn from_user_settings(user_settings: &Value, store_path: Option<PathBuf>) -> Self {
        let mut overrides = BTreeMap::new();
        let mut user = BTreeMap::new();
        let mut unsupported_version = None;
        match user_settings.get(TRANSLATION_TAB_SECTION).and_then(|tab| tab.get(COMPOSITION_PRESETS_KEY)) {
            None => {}
            Some(Value::Object(store)) => match store_shape(store) {
                StoreShape::Flat => decode_user_presets(store, &mut user),
                StoreShape::Current => {
                    for (id, entry) in store.get(STORE_BUILTIN_KEY).and_then(Value::as_object).into_iter().flatten() {
                        let Some(preset) = BuiltinCompositionPreset::from_id(id) else {
                            runtime_log::log_warn(format!("[translation::composition_presets] skipped an override of an unknown built-in composition preset {id:?}"));
                            continue;
                        };
                        let factory = preset.factory();
                        match decode_preset(entry, factory.clone()) {
                            Some(options) if options != factory => {
                                overrides.insert(preset, options);
                            }
                            Some(_) => {}
                            None => runtime_log::log_warn(format!("[translation::composition_presets] skipped the override of built-in composition preset {id:?}: not a JSON object")),
                        }
                    }
                    if let Some(entries) = store.get(STORE_USER_KEY).and_then(Value::as_object) {
                        decode_user_presets(entries, &mut user);
                    }
                }
                StoreShape::Unsupported(version) => {
                    runtime_log::log_warn(format!(
                        "[translation::composition_presets] user_config {TRANSLATION_TAB_SECTION}.{COMPOSITION_PRESETS_KEY} has format version {version} (this build knows {STORE_VERSION}); presets are not read and will not be written"
                    ));
                    unsupported_version = Some(version);
                }
            },
            Some(_) => runtime_log::log_warn(format!("[translation::composition_presets] user_config {TRANSLATION_TAB_SECTION}.{COMPOSITION_PRESETS_KEY} is not a JSON object; no composition presets read")),
        }
        Self::from_parts(overrides, user, unsupported_version, store_path)
    }

    /// Every combo row in order: the built-ins in [`BuiltinCompositionPreset::ALL`] order, then
    /// the user presets byte-sorted by name.
    #[must_use]
    fn rows(&self) -> Vec<CompositionPresetRef> {
        BuiltinCompositionPreset::ALL.into_iter().map(CompositionPresetRef::Builtin).chain(self.user.keys().cloned().map(CompositionPresetRef::User)).collect()
    }

    /// What `preset` means now: its override when the user edited it, else its factory values.
    #[must_use]
    fn effective_builtin(&self, preset: BuiltinCompositionPreset) -> CompositionPanelOptions {
        self.overrides.get(&preset).cloned().unwrap_or_else(|| preset.factory())
    }

    /// The values `preset` currently stands for; `None` for a user preset that does not exist.
    #[must_use]
    fn effective(&self, preset: &CompositionPresetRef) -> Option<CompositionPanelOptions> {
        match preset {
            CompositionPresetRef::Builtin(builtin) => Some(self.effective_builtin(*builtin)),
            CompositionPresetRef::User(name) => self.user.get(name).cloned(),
        }
    }

    /// Whether `preset` exists (a built-in always does).
    #[must_use]
    fn exists(&self, preset: &CompositionPresetRef) -> bool {
        match preset {
            CompositionPresetRef::Builtin(_) => true,
            CompositionPresetRef::User(name) => self.user.contains_key(name),
        }
    }

    /// Whether a NEW user preset name `name` is taken: by a user preset, or by a built-in's
    /// display name in the current UI language.
    #[must_use]
    fn name_taken(&self, name: &str) -> bool {
        self.user.contains_key(name) || BuiltinCompositionPreset::ALL.iter().any(|preset| preset.display_name() == name)
    }

    /// The selected preset, for the title settings.
    #[must_use]
    pub(crate) fn selection(&self) -> Option<&CompositionPresetRef> {
        self.selected.as_ref()
    }

    /// Re-selects the title's remembered preset WITHOUT applying its values (the title's own
    /// options are already loaded). A user preset that no longer exists selects «Нет».
    pub(crate) fn restore_selection(&mut self, selection: Option<CompositionPresetRef>) {
        let selection = selection.filter(|preset| self.exists(preset));
        self.set_selection(selection);
    }

    /// Whether the selected preset's values differ from `options` (both in `stored_form`).
    /// `false` with no selection. The pending rename is the picker's own half of "unsaved".
    #[must_use]
    fn selected_differs(&self, options: &CompositionPanelOptions) -> bool {
        self.selected.as_ref().and_then(|preset| self.effective(preset)).is_some_and(|preset| preset != options.stored_form())
    }

    /// Whether the selected preset's last write failed (it then shows as unsaved).
    #[must_use]
    fn selected_write_failed(&self) -> bool {
        self.selected.as_ref().is_some_and(|preset| self.failed.contains(preset))
    }

    /// Whether «Delete» applies to the selection: a user preset, or a built-in with an
    /// override to remove. An unmodified built-in cannot be deleted.
    #[must_use]
    fn can_delete_selected(&self) -> bool {
        match &self.selected {
            None => false,
            Some(CompositionPresetRef::User(_)) => true,
            Some(CompositionPresetRef::Builtin(preset)) => self.overrides.contains_key(preset),
        }
    }

    /// Draws the picker block and its error line, applies the action asked for, and reports
    /// what the title must do (save the section, rebuild the text).
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui, options: &mut CompositionPanelOptions) -> CompositionPanelActions {
        self.poll_write_outcomes();
        let rows = self.rows();
        let names: Vec<String> = rows.iter().map(CompositionPresetRef::label).collect();
        let selected_pos = self.selected.as_ref().and_then(|selected| rows.iter().position(|row| row == selected));
        // With no selection the delete button stays (disabled) so the row does not jump; it is
        // left out only for an unmodified built-in, which has nothing to delete.
        let show_delete = self.selected.is_none() || self.can_delete_selected();
        // A built-in's name is a translation, not a key: its field is shown disabled.
        let renamable = !matches!(self.selected, Some(CompositionPresetRef::Builtin(_)));
        let response = PresetPicker::new(PRESET_LABEL_KEY, t!("translation.composition.preset_label"), &names, selected_pos)
            .unsaved(self.selected_differs(options) || self.selected_write_failed())
            .delete(show_delete)
            .rename(renamable)
            .show(ui, &mut self.picker);
        let actions = match response.action {
            Some(PresetPickerAction::Deselect) => self.deselect(),
            Some(PresetPickerAction::Create) => self.create(options),
            Some(PresetPickerAction::Select(idx)) => match rows.get(idx) {
                Some(row) => self.select(row, options),
                None => CompositionPanelActions::default(),
            },
            Some(PresetPickerAction::Save) => self.save(options),
            // The widget reports the selected index; deleting it is deleting the selection.
            Some(PresetPickerAction::Delete(_)) => self.delete_selected(options),
            None => CompositionPanelActions::default(),
        };
        if let Some(error) = &self.error {
            ui.colored_label(ms_theme::status::ERROR, error);
        }
        if !self.failed.is_empty()
            && let Some(write_error) = &self.write_error
        {
            ui.colored_label(ms_theme::status::ERROR, write_error);
        }
        actions
    }

    /// Selects `preset` and applies ALL its values to `options`.
    fn select(&mut self, preset: &CompositionPresetRef, options: &mut CompositionPanelOptions) -> CompositionPanelActions {
        let Some(values) = self.effective(preset) else {
            return CompositionPanelActions::default();
        };
        *options = values;
        self.set_selection(Some(preset.clone()));
        CompositionPanelActions { options_changed: true, request_rebuild: true }
    }

    /// «Нет»: drops the selection; the options stay as they are.
    fn deselect(&mut self) -> CompositionPanelActions {
        self.set_selection(None);
        CompositionPanelActions { options_changed: true, request_rebuild: false }
    }

    /// Creates a user preset from `options` under a free default name and selects it.
    fn create(&mut self, options: &CompositionPanelOptions) -> CompositionPanelActions {
        let name = self.free_default_name();
        if !self.commit(PresetWrite::UpsertUser { name: name.clone(), options: options.stored_form() }) {
            return CompositionPanelActions::default();
        }
        self.set_selection(Some(CompositionPresetRef::User(name)));
        CompositionPanelActions { options_changed: true, request_rebuild: false }
    }

    /// Commits the selected preset: its values from `options` and, for a user preset, the
    /// name in the rename buffer (taken verbatim). Refused, with the reason in the error line:
    /// an empty name, a name another preset holds (a built-in's display name included), a
    /// read-only store. A built-in keeps its identity whatever the buffer holds (the picker shows
    /// it disabled); saving it with its factory values removes its override.
    fn save(&mut self, options: &CompositionPanelOptions) -> CompositionPanelActions {
        let value = options.stored_form();
        match self.selected.clone() {
            None => CompositionPanelActions::default(),
            Some(CompositionPresetRef::Builtin(preset)) => {
                if self.commit(PresetWrite::SaveBuiltin { preset, options: value }) {
                    self.set_selection(Some(CompositionPresetRef::Builtin(preset)));
                }
                CompositionPanelActions::default()
            }
            Some(CompositionPresetRef::User(current)) => {
                let new_name = self.picker.name().to_owned();
                if new_name.is_empty() {
                    self.error = Some(t!("translation.composition.preset_empty_name_status").to_string());
                    return CompositionPanelActions::default();
                }
                if new_name != current && self.name_taken(&new_name) {
                    self.error = Some(tf!("translation.composition.preset_name_taken_status", name = new_name));
                    return CompositionPanelActions::default();
                }
                let renamed = new_name != current;
                let op = if renamed {
                    PresetWrite::RenameUser { from: current, to: new_name.clone(), options: value }
                } else {
                    PresetWrite::UpsertUser { name: current, options: value }
                };
                if !self.commit(op) {
                    return CompositionPanelActions::default();
                }
                self.set_selection(Some(CompositionPresetRef::User(new_name)));
                // A rename changes the title's remembered selection.
                CompositionPanelActions { options_changed: renamed, request_rebuild: false }
            }
        }
    }

    /// Deletes the selection: a user preset is removed and the panel goes to «Нет»; a
    /// modified built-in loses its override, stays selected, and its factory values are
    /// applied to `options`. An unmodified built-in is left alone.
    fn delete_selected(&mut self, options: &mut CompositionPanelOptions) -> CompositionPanelActions {
        match self.selected.clone() {
            None => CompositionPanelActions::default(),
            Some(CompositionPresetRef::Builtin(preset)) => {
                if !self.overrides.contains_key(&preset) || !self.commit(PresetWrite::ResetBuiltin { preset }) {
                    return CompositionPanelActions::default();
                }
                *options = preset.factory();
                self.set_selection(Some(CompositionPresetRef::Builtin(preset)));
                CompositionPanelActions { options_changed: true, request_rebuild: true }
            }
            Some(CompositionPresetRef::User(name)) => {
                if !self.commit(PresetWrite::RemoveUser { name }) {
                    return CompositionPanelActions::default();
                }
                self.set_selection(None);
                CompositionPanelActions { options_changed: true, request_rebuild: false }
            }
        }
    }

    /// Applies `op` in memory, with the same rules as the disk writer.
    ///
    /// # Errors
    /// [`PresetStoreError::UnsupportedVersion`] for a read-only store; nothing changes then.
    fn apply(&mut self, op: &PresetWrite) -> Result<(), PresetStoreError> {
        if let Some(version) = &self.unsupported_version {
            return Err(PresetStoreError::UnsupportedVersion { version: version.clone() });
        }
        match op {
            PresetWrite::UpsertUser { name, options } => {
                self.user.insert(name.clone(), options.stored_form());
            }
            PresetWrite::RenameUser { from, to, options } => {
                self.user.remove(from);
                self.user.insert(to.clone(), options.stored_form());
            }
            PresetWrite::RemoveUser { name } => {
                self.user.remove(name);
            }
            PresetWrite::SaveBuiltin { preset, options } => {
                let options = options.stored_form();
                if options == preset.factory() {
                    self.overrides.remove(preset);
                } else {
                    self.overrides.insert(*preset, options);
                }
            }
            PresetWrite::ResetBuiltin { preset } => {
                self.overrides.remove(preset);
            }
        }
        Ok(())
    }

    /// Applies `op` in memory and hands it to the writer. A read-only store (a newer build's
    /// format) refuses it with a status line; returns whether the operation was applied.
    fn commit(&mut self, op: PresetWrite) -> bool {
        if let Err(err) = self.apply(&op) {
            runtime_log::log_warn(format!("[translation::composition_presets] refused a preset operation ({}); error={err}", op.describe()));
            self.error = Some(t!("translation.composition.preset_store_read_only_status").to_string());
            return false;
        }
        self.queue_write(op);
        true
    }

    /// Writes the selection and re-syncs everything bound to it (picker, validation line; a write
    /// error stays).
    fn set_selection(&mut self, selected: Option<CompositionPresetRef>) {
        self.picker.reset_to(selected.as_ref().map(CompositionPresetRef::label).as_deref());
        self.selected = selected;
        self.error = None;
    }

    /// `translation.composition.preset_default_name` with the lowest 1-based index no preset
    /// carries, by the shared bounded allocator (`ms_config::preset_store::free_default_name`).
    #[must_use]
    fn free_default_name(&self) -> String {
        free_default_name(self.rows().len(), |index| tf!("translation.composition.preset_default_name", index = index), |name| self.name_taken(name))
    }

    /// Hands `op` to the writer thread (started on first use). Without a store path the
    /// operation stays in memory only. A writer that cannot start or has died marks the
    /// operation's entries failed (write-error line) and is logged.
    fn queue_write(&mut self, op: PresetWrite) {
        let Some(path) = self.store_path.clone() else {
            return;
        };
        let (path, op, error) = match self.writer.submit((path, op)) {
            Ok(()) => return,
            // Both are logged by the writer; the op's entries are marked failed here.
            Err(SubmitError::Spawn { job: (path, op), error }) => (path, op, error.to_string()),
            Err(SubmitError::WorkerGone { job: (path, op) }) => (path, op, t!("translation.composition.preset_writer_lost_error").to_string()),
        };
        runtime_log::log_error(format!("[translation::composition_presets] a preset operation was not saved; op={} path={} error={error}", op.describe(), path.display()));
        self.apply_write_outcome(WriteOutcome { entries: op.entries(), error: Some(error) });
    }

    /// Applies every answer the writer sent since the last frame.
    fn poll_write_outcomes(&mut self) {
        while let Some(outcome) = self.writer.try_recv_outcome() {
            self.apply_write_outcome(outcome);
        }
    }

    /// The failed-write rule (file header): a failure marks the operation's entries failed and
    /// becomes the write-error line; a success clears its entries, and the line once none is left.
    fn apply_write_outcome(&mut self, outcome: WriteOutcome) {
        let WriteOutcome { entries, error } = outcome;
        match error {
            Some(err) => {
                self.failed.extend(entries);
                self.write_error = Some(tf!("translation.composition.preset_save_error_status", err = err));
            }
            None => {
                for entry in &entries {
                    self.failed.remove(entry);
                }
                if self.failed.is_empty() {
                    self.write_error = None;
                }
            }
        }
    }
}

/// The writer thread's step: persists one operation, logs the result and answers with the
/// operation's entries and the error, if any.
fn run_write_job((path, op): WriteJob) -> WriteOutcome {
    let error = match persist_preset_write(&path, &op) {
        Ok(()) => {
            runtime_log::log_info(format!("[translation::composition_presets] saved preset operation ({}) to {}", op.describe(), path.display()));
            None
        }
        Err(err) => {
            runtime_log::log_error(format!(
                "[translation::composition_presets] failed to save a preset operation; op={} path={} error={err} possible_cause=malformed or unwritable user_config, or a preset store of a newer build (both are left untouched)",
                op.describe(),
                path.display()
            ));
            Some(err)
        }
    };
    WriteOutcome { entries: op.entries(), error }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EXPORT: BuiltinCompositionPreset = BuiltinCompositionPreset::TranslationExport;
    const AI: BuiltinCompositionPreset = BuiltinCompositionPreset::AiTranslation;

    /// Installs the embedded catalog of `tag` for tests whose result depends on the UI language,
    /// serialized on the process-global locale lock (the active catalog is one `ArcSwap`).
    fn locale(tag: &str) -> std::sync::MutexGuard<'static, ()> {
        let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        install_locale(tag);
        guard
    }

    /// Switches the active catalog; only while holding the guard of [`locale`].
    fn install_locale(tag: &str) {
        let tag = ms_i18n::LocaleTag::parse(tag).expect("a shipped locale tag parses");
        ms_i18n::set_locale(&tag).expect("a shipped catalog installs");
    }

    fn user(name: &str) -> CompositionPresetRef {
        CompositionPresetRef::User(name.to_string())
    }

    fn builtin(preset: BuiltinCompositionPreset) -> CompositionPresetRef {
        CompositionPresetRef::Builtin(preset)
    }

    /// A store with the given user presets, no overrides, no persistence.
    fn store(stored: &[(&str, CompositionPanelOptions)]) -> CompositionPresets {
        CompositionPresets::from_parts(BTreeMap::new(), stored.iter().map(|(name, options)| ((*name).to_string(), options.stored_form())).collect(), None, None)
    }

    fn user_preset() -> CompositionPanelOptions {
        CompositionPanelOptions { limit: 1234, include_image_bubbles: true, ..CompositionPanelOptions::default() }
    }

    /// The user's own «Экспорт перевода», exactly as the codec wrote it into `user_config`.
    fn translation_export_json() -> Value {
        json!({
            "character_name_position": "before", "character_name_prefix": "", "character_name_suffix": ": ",
            "hint_extra_sep": "", "hint_wrap": "()", "hint_wrap_enabled": true, "ignore_translated_lines": true,
            "include_hint_bubbles": true, "include_image_bubbles": true, "jinja2_enabled": false, "jinja2_template": "",
            "limit": 650, "limit_enabled": false, "merge_same_character": true, "method": "height", "nl_replace": " ",
            "nl_replace_enabled": false, "replica_prefix": "- ", "sep_between": "\\n\\n", "sep_same_character": "\\n",
            "source_mode": "translation", "use_character_names": true, "wrap_with": "``", "wrap_with_enabled": false
        })
    }

    /// The user's own «Перевод через ИИ», exactly as the codec wrote it into `user_config`.
    fn ai_translation_json() -> Value {
        json!({
            "character_name_position": "after", "character_name_prefix": " - ", "character_name_suffix": "",
            "hint_extra_sep": "", "hint_wrap": "()", "hint_wrap_enabled": true, "ignore_translated_lines": true,
            "include_hint_bubbles": true, "include_image_bubbles": false, "jinja2_enabled": false, "jinja2_template": "",
            "limit": 650, "limit_enabled": true, "merge_same_character": true, "method": "height", "nl_replace": " ",
            "nl_replace_enabled": true, "replica_prefix": "", "sep_between": "\\n\\n", "sep_same_character": "\\n",
            "source_mode": "original", "use_character_names": true, "wrap_with": "``", "wrap_with_enabled": true
        })
    }

    /// The factory values are what the codec reads from the user's presets they were made from,
    /// and writing them back yields those exact JSON objects.
    #[test]
    fn builtin_factory_values_equal_the_codec_reading_the_source_presets() -> Result<(), String> {
        for (preset, source) in [(EXPORT, translation_export_json()), (AI, ai_translation_json())] {
            let obj = source.as_object().ok_or("the source preset is an object")?;
            let mut read = CompositionPanelOptions::default();
            read_composition_options_json(obj, &mut read);
            assert_eq!(preset.factory(), read.stored_form(), "{preset:?}");
            assert_eq!(preset_json(&preset.factory()), source, "{preset:?}");
        }
        Ok(())
    }

    #[test]
    fn builtin_ids_are_stable_unique_and_round_trip() {
        assert_eq!(EXPORT.id(), "translation_export");
        assert_eq!(AI.id(), "ai_translation");
        for preset in BuiltinCompositionPreset::ALL {
            assert_eq!(BuiltinCompositionPreset::from_id(preset.id()), Some(preset));
        }
        assert_eq!(BuiltinCompositionPreset::from_id("Экспорт перевода"), None, "a display name is not an id");
    }

    /// The shown name follows the UI language; the identity (selection, values) does not.
    #[test]
    fn builtin_display_name_follows_the_locale_but_the_selection_does_not() {
        let _guard = locale("ru");
        assert_eq!(EXPORT.display_name(), "Экспорт перевода");
        assert_eq!(AI.display_name(), "Перевод через ИИ");
        let mut presets = store(&[]);
        let mut options = CompositionPanelOptions::default();
        presets.select(&builtin(AI), &mut options);
        assert_eq!(presets.picker.name(), "Перевод через ИИ");
        install_locale("en");
        assert_eq!(EXPORT.display_name(), "Translation export");
        assert_eq!(AI.display_name(), "AI translation");
        assert_eq!(presets.selection(), Some(&builtin(AI)));
        assert!(!presets.selected_differs(&options));
        let rows: Vec<String> = presets.rows().iter().map(CompositionPresetRef::label).collect();
        assert_eq!(rows, vec!["Translation export", "AI translation"]);
    }

    #[test]
    fn rows_list_builtins_first_then_sorted_user_presets() {
        let presets = store(&[("z", user_preset()), ("m", user_preset())]);
        assert_eq!(presets.rows(), vec![builtin(EXPORT), builtin(AI), user("m"), user("z")]);
        assert_eq!(presets.effective(&builtin(AI)), Some(AI.factory()));
    }

    #[test]
    fn select_applies_every_value_including_sorting() {
        let mut presets = store(&[]);
        let mut options = CompositionPanelOptions::default();
        let actions = presets.select(&builtin(EXPORT), &mut options);
        assert!(actions.options_changed && actions.request_rebuild);
        assert_eq!(options, EXPORT.factory());
        assert_eq!(presets.selection(), Some(&builtin(EXPORT)));
        assert!(!presets.selected_differs(&options));
        options.limit_enabled = true;
        assert!(presets.selected_differs(&options), "an edit is unsaved");
    }

    #[test]
    fn unmodified_builtin_cannot_be_deleted_and_modified_one_reverts() {
        let mut presets = store(&[]);
        let mut options = CompositionPanelOptions::default();
        presets.select(&builtin(EXPORT), &mut options);
        assert!(!presets.can_delete_selected());
        assert!(!presets.delete_selected(&mut options).options_changed);

        // Edit + save stores an override under the built-in's id.
        options.replica_prefix = "edited".to_string();
        presets.save(&options);
        assert_eq!(presets.error, None);
        assert_eq!(presets.overrides.get(&EXPORT).map(|value| value.replica_prefix.as_str()), Some("edited"));
        assert!(presets.user.is_empty(), "an override is not a user preset");
        assert!(presets.can_delete_selected());
        assert!(!presets.selected_differs(&options));

        // Delete removes the override, keeps the selection and restores the factory values.
        let actions = presets.delete_selected(&mut options);
        assert!(actions.options_changed && actions.request_rebuild);
        assert!(presets.overrides.is_empty());
        assert_eq!(presets.selection(), Some(&builtin(EXPORT)));
        assert_eq!(options, EXPORT.factory());
    }

    #[test]
    fn saving_factory_values_on_a_builtin_drops_the_override() {
        let mut presets = store(&[]);
        let mut options = AI.factory();
        options.limit = 999;
        presets.restore_selection(Some(builtin(AI)));
        presets.save(&options);
        assert!(presets.overrides.contains_key(&AI));
        presets.save(&AI.factory());
        assert!(presets.overrides.is_empty());
        assert!(!presets.can_delete_selected());
    }

    #[test]
    fn builtin_save_ignores_the_name_buffer() {
        let _guard = locale("en");
        let mut presets = store(&[]);
        let mut options = CompositionPanelOptions::default();
        presets.select(&builtin(AI), &mut options);
        presets.picker.set_name("B2");
        options.limit = 2000;
        let actions = presets.save(&options);
        assert!(!actions.options_changed, "the selection keeps its identity");
        assert_eq!(presets.error, None);
        assert_eq!(presets.rows(), vec![builtin(EXPORT), builtin(AI)]);
        assert_eq!(presets.selection(), Some(&builtin(AI)));
        assert_eq!(presets.overrides.get(&AI).map(|value| value.limit), Some(2000));
        assert_eq!(presets.picker.name(), AI.display_name(), "the buffer is re-synced with the selection");
    }

    #[test]
    fn user_preset_rename_rules_are_verbatim_and_unique() {
        let _guard = locale("en");
        let mut presets = store(&[("mine", user_preset()), ("other", user_preset())]);
        let mut options = CompositionPanelOptions::default();
        presets.select(&user("mine"), &mut options);

        presets.picker.set_name("");
        presets.save(&options);
        assert_eq!(presets.error.as_deref(), Some(t!("translation.composition.preset_empty_name_status")));

        for taken in ["other", "Translation export"] {
            presets.picker.set_name(taken);
            presets.save(&options);
            assert_eq!(presets.error, Some(tf!("translation.composition.preset_name_taken_status", name = taken)));
        }

        presets.picker.set_name(" mine ");
        let actions = presets.save(&options);
        assert!(actions.options_changed, "a rename changes the remembered selection");
        assert_eq!(presets.error, None);
        assert_eq!(presets.selection(), Some(&user(" mine ")));
        assert!(presets.user.contains_key(" mine ") && !presets.user.contains_key("mine"));
    }

    /// The user's own copies named like the built-ins (made before the built-ins shipped) stay
    /// ordinary user presets next to the built-ins; each keeps its own identity and values.
    #[test]
    fn user_presets_named_like_builtins_coexist_with_them() {
        let _guard = locale("ru");
        let mut copy = EXPORT.factory();
        copy.limit = 5000;
        let mut presets = store(&[("Экспорт перевода", copy.clone()), ("Перевод через ИИ", AI.factory())]);
        let rows = presets.rows();
        assert_eq!(rows, vec![builtin(EXPORT), builtin(AI), user("Перевод через ИИ"), user("Экспорт перевода")]);
        let labels: Vec<String> = rows.iter().map(CompositionPresetRef::label).collect();
        assert_eq!(labels.iter().filter(|label| *label == "Экспорт перевода").count(), 2, "both rows are listed");

        let mut options = CompositionPanelOptions::default();
        presets.select(&user("Экспорт перевода"), &mut options);
        assert_eq!(options, copy.stored_form());
        // Saving the copy under its own (colliding) name is not a rename and stays allowed.
        options.limit = 6000;
        presets.save(&options);
        assert_eq!(presets.error, None);
        assert_eq!(presets.user.get("Экспорт перевода").map(|value| value.limit), Some(6000));
        assert!(presets.overrides.is_empty(), "the copy never becomes the built-in's override");

        presets.select(&builtin(EXPORT), &mut options);
        assert_eq!(options, EXPORT.factory());
        presets.delete_selected(&mut options);
        assert!(presets.user.contains_key("Экспорт перевода"), "an unmodified built-in deletes nothing");
        install_locale("en");
    }

    #[test]
    fn create_picks_a_free_default_name_and_selects_it() {
        let _guard = locale("en");
        let first = tf!("translation.composition.preset_default_name", index = 1);
        let mut presets = store(&[(first.as_str(), user_preset())]);
        let options = CompositionPanelOptions { nl_replace: String::new(), ..CompositionPanelOptions::default() };
        presets.create(&options);
        let created = presets.selection().cloned();
        assert!(matches!(&created, Some(CompositionPresetRef::User(name)) if *name != first), "{created:?}");
        assert!(!presets.selected_differs(&options), "an empty nl_replace is not a false difference");
        presets.create(&options);
        assert_eq!(presets.rows().len(), 5, "two built-ins, the seeded one and two created ones");
    }

    #[test]
    fn deleting_a_user_preset_goes_to_none() {
        let mut presets = store(&[("mine", user_preset())]);
        let mut options = CompositionPanelOptions::default();
        presets.select(&user("mine"), &mut options);
        assert!(presets.can_delete_selected());
        presets.delete_selected(&mut options);
        assert_eq!(presets.selection(), None);
        assert_eq!(presets.rows(), vec![builtin(EXPORT), builtin(AI)]);
        assert_eq!(options, user_preset().stored_form(), "deleting does not touch the options");
    }

    #[test]
    fn restore_selection_drops_unknown_user_presets() {
        let mut presets = store(&[("mine", user_preset())]);
        presets.restore_selection(Some(user("gone")));
        assert_eq!(presets.selection(), None);
        presets.restore_selection(Some(builtin(AI)));
        assert_eq!(presets.selection(), Some(&builtin(AI)));
        presets.restore_selection(Some(user("mine")));
        assert_eq!(presets.picker.name(), "mine");
    }

    /// `composition.preset`: the id-keyed shapes round-trip; the earlier plain name is a user
    /// preset; anything else (an unknown id too) is no selection.
    #[test]
    fn selection_codec_reads_current_and_earlier_forms() {
        let mut obj = Map::new();
        for selection in [builtin(EXPORT), builtin(AI), user("Экспорт перевода")] {
            write_selected_preset(&mut obj, Some(&selection));
            assert_eq!(read_selected_preset(&obj), Some(selection));
        }
        write_selected_preset(&mut obj, Some(&builtin(AI)));
        assert_eq!(obj.get(SELECTED_PRESET_KEY), Some(&json!({"builtin": "ai_translation"})));
        write_selected_preset(&mut obj, None);
        assert!(!obj.contains_key(SELECTED_PRESET_KEY));

        let read = |value: Value| read_selected_preset(&Map::from_iter([(SELECTED_PRESET_KEY.to_string(), value)]));
        assert_eq!(read(json!("Экспорт перевода")), Some(user("Экспорт перевода")), "an earlier plain name");
        assert_eq!(read(json!({"builtin": "nope"})), None);
        assert_eq!(read(json!({"builtin": "ai_translation", "user": "x"})), None);
        assert_eq!(read(json!(7)), None);
    }

    /// The flat store (the earlier shape) reads as user presets, a built-in's name included;
    /// an entry named "version" with an object value is a preset, not a format marker.
    #[test]
    fn flat_store_reads_as_user_presets() {
        let settings = json!({"TranslarionTab": {"composition_presets": {
            "Экспорт перевода": translation_export_json(),
            "partial": {"limit": 5000, "method": "order"},
            "version": {"limit": 300},
            "broken": 7,
            "": {"limit": 300},
        }}});
        let presets = CompositionPresets::from_user_settings(&settings, None);
        assert_eq!(presets.user.keys().collect::<Vec<_>>(), vec!["partial", "version", "Экспорт перевода"]);
        assert!(presets.overrides.is_empty() && presets.unsupported_version.is_none());
        let expected = CompositionPanelOptions { limit: 5000, sort_method: CompositionSortMethod::Order, ..CompositionPanelOptions::default() };
        assert_eq!(presets.user.get("partial"), Some(&expected.stored_form()));
        assert_eq!(presets.user.get("Экспорт перевода"), Some(&EXPORT.factory()));
    }

    #[test]
    fn current_store_reads_overrides_by_id_and_user_presets() {
        let settings = json!({"TranslarionTab": {"composition_presets": {
            "version": 1,
            "builtin": {"ai_translation": {"limit": 900}, "translation_export": translation_export_json(), "unknown": {}, "Перевод через ИИ": {"limit": 1}},
            "user": {"mine": {"limit": 5000}, "": {}},
        }}});
        let presets = CompositionPresets::from_user_settings(&settings, None);
        let ai_edited = CompositionPanelOptions { limit: 900, ..AI.factory() };
        assert_eq!(presets.overrides.keys().copied().collect::<Vec<_>>(), vec![AI], "a factory-equal override is dropped; ids only");
        assert_eq!(presets.effective(&builtin(AI)), Some(ai_edited.stored_form()));
        assert_eq!(presets.user.keys().collect::<Vec<_>>(), vec!["mine"]);
    }

    #[test]
    fn unknown_store_version_is_read_only() {
        let _guard = locale("en");
        let settings = json!({"TranslarionTab": {"composition_presets": {"version": 2, "user": {"mine": {}}}}});
        let mut presets = CompositionPresets::from_user_settings(&settings, None);
        assert!(presets.user.is_empty());
        let mut options = CompositionPanelOptions::default();
        presets.select(&builtin(EXPORT), &mut options);
        options.limit = 777;
        presets.save(&options);
        assert_eq!(presets.error.as_deref(), Some(t!("translation.composition.preset_store_read_only_status")));
        assert!(presets.overrides.is_empty());

        let mut root = settings.clone();
        let refused = apply_preset_write(&mut root, &PresetWrite::RemoveUser { name: "mine".into() });
        assert!(matches!(refused, Err(PresetStoreError::UnsupportedVersion { .. })), "{refused:?}");
        assert_eq!(root, settings, "a refused write changes nothing");
    }

    /// The first write into a flat store moves its entries verbatim under `user`; later writes
    /// edit only the touched entry.
    #[test]
    fn writes_migrate_a_flat_store_and_keep_other_keys() -> Result<(), PresetStoreError> {
        let mut root = json!({"General": {"x": 1}, "TranslarionTab": {"MachineTranslation": {"service": "deepl"}, "composition_presets": {"old": {"limit": 1}, "keep": {}, "broken": 7}}});
        apply_preset_write(&mut root, &PresetWrite::RenameUser { from: "old".into(), to: "new".into(), options: user_preset() })?;
        apply_preset_write(&mut root, &PresetWrite::RemoveUser { name: "keep".into() })?;
        apply_preset_write(&mut root, &PresetWrite::SaveBuiltin { preset: AI, options: CompositionPanelOptions { limit: 900, ..AI.factory() } })?;
        apply_preset_write(&mut root, &PresetWrite::SaveBuiltin { preset: EXPORT, options: EXPORT.factory() })?;
        assert_eq!(root["General"]["x"], 1);
        assert_eq!(root["TranslarionTab"]["MachineTranslation"]["service"], "deepl");
        let store = &root["TranslarionTab"][COMPOSITION_PRESETS_KEY];
        assert_eq!(store["version"], 1);
        assert_eq!(store["user"], json!({"new": preset_json(&user_preset()), "broken": 7}));
        assert_eq!(store["builtin"], json!({"ai_translation": preset_json(&CompositionPanelOptions { limit: 900, ..AI.factory() })}));
        apply_preset_write(&mut root, &PresetWrite::ResetBuiltin { preset: AI })?;
        assert_eq!(root["TranslarionTab"][COMPOSITION_PRESETS_KEY]["builtin"], json!({}));

        let mut broken = json!({"TranslarionTab": 5});
        apply_preset_write(&mut broken, &PresetWrite::UpsertUser { name: "p".into(), options: CompositionPanelOptions::default() })?;
        assert_eq!(broken, json!({"TranslarionTab": {"composition_presets": {"version": 1, "user": {"p": preset_json(&CompositionPanelOptions::default())}}}}));
        Ok(())
    }

    #[test]
    fn the_writer_persists_operations_in_order_to_its_own_file() -> Result<(), String> {
        let _guard = locale("en");
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("user_config.json");
        std::fs::write(&path, r#"{"General":{"ui_language":"ru"},"TranslarionTab":{"composition_presets":{"legacy":{"limit":300}}}}"#).map_err(|err| err.to_string())?;
        {
            let seed: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|err| err.to_string())?).map_err(|err| err.to_string())?;
            let mut presets = CompositionPresets::from_user_settings(&seed, Some(path.clone()));
            let mut options = user_preset();
            presets.create(&options);
            options.limit = 4321;
            presets.save(&options);
            presets.create(&options);
            presets.delete_selected(&mut options);
            presets.select(&builtin(AI), &mut options);
            options.limit = 900;
            presets.save(&options);
            // Dropping joins the writer, so every queued operation is on disk afterwards.
        }
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|err| err.to_string())?).map_err(|err| err.to_string())?;
        assert_eq!(root["General"]["ui_language"], "ru");
        let store = &root["TranslarionTab"][COMPOSITION_PRESETS_KEY];
        let first = tf!("translation.composition.preset_default_name", index = 1);
        let user_names: Vec<&String> = store["user"].as_object().ok_or("user map written")?.keys().collect();
        assert_eq!(user_names, vec![&first, &"legacy".to_string()], "the second preset was deleted after it was created; the flat entry moved");
        let reloaded = CompositionPresets::from_user_settings(&root, None);
        assert_eq!(reloaded.user.get(&first), Some(&CompositionPanelOptions { limit: 4321, ..user_preset() }.stored_form()));
        assert_eq!(reloaded.effective(&builtin(AI)).map(|value| value.limit), Some(900));
        Ok(())
    }

    /// A failed write keeps the entry unsaved and the error line up across selection changes,
    /// until a later write of the same entry succeeds.
    #[test]
    fn a_failed_write_stays_unsaved_until_a_later_success() {
        let mut presets = store(&[("mine", user_preset()), ("other", user_preset())]);
        let mut options = CompositionPanelOptions::default();
        presets.select(&user("mine"), &mut options);
        presets.apply_write_outcome(WriteOutcome { entries: vec![user("mine")], error: Some("disk full".to_string()) });
        assert!(presets.selected_write_failed());
        assert!(!presets.selected_differs(&options), "the values match; only the write failed");
        presets.select(&user("other"), &mut options);
        assert!(!presets.selected_write_failed());
        assert!(presets.write_error.is_some(), "the error survives a selection change");
        presets.apply_write_outcome(WriteOutcome { entries: vec![builtin(AI)], error: None });
        assert!(presets.write_error.is_some(), "a success of ANOTHER entry clears nothing");
        presets.apply_write_outcome(WriteOutcome { entries: vec![user("mine"), user("renamed")], error: None });
        assert!(presets.failed.is_empty() && presets.write_error.is_none());
    }

    /// End to end: a write into a malformed `user_config` fails and marks the created preset.
    #[test]
    fn a_real_failed_write_is_reported_by_the_writer() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("user_config.json");
        std::fs::write(&path, "{ not json").map_err(|err| err.to_string())?;
        let mut presets = CompositionPresets::from_parts(BTreeMap::new(), BTreeMap::new(), None, Some(path.clone()));
        presets.create(&user_preset());
        let outcome = presets.writer.recv_outcome_timeout(std::time::Duration::from_secs(30)).map_err(|err| err.to_string())?;
        assert!(outcome.error.is_some(), "{outcome:?}");
        presets.apply_write_outcome(outcome);
        assert!(presets.selected_write_failed());
        assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, "{ not json", "the malformed file is untouched");
        Ok(())
    }
}
