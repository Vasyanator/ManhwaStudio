/*
File: composition.rs

Purpose:
Composed-translation panel of the Translation tab: builds the prompt text from the project
bubbles, hosts the plain/MiniJinja formatting options, and exports the result to TXT/DOCX.

Main responsibilities:
- own `CompositionPanelOptions` (the settings boundary read and written by the tab (`tab/settings_io.rs`) through the
  codec below)
- filter, sort and format bubbles into the composed text (`compose_translation_text`)
- own the ONE JSON mapping of `CompositionPanelOptions` (`write_composition_options_json` /
  `read_composition_options_json`, `CompositionPanelOptions::stored_form`), shared by the
  title settings and the composition presets
- draw the «Текст» / «Параметры» sub-tabs (the preset picker of `composition_presets.rs` on
  top of «Параметры») and the export actions
- run a TXT/DOCX export: a non-blocking save dialog (`ms_platform::file_dialog`), then the write
  and the target's `commit()` on a worker (`write_export`)

Key structures:
- CompositionPanelOptions: every composition setting, persisted in `settings.json`
- ComposedItem: one formatted entry handed to the emission pass, including the non-emitting
  `DroppedReplica` barrier that stops hint lookahead
- HintBinding: forward / dropped / trailing, the resolved attachment of one hint
- EmitParams: emission-time settings (limit, merging, separators, name format)
- CharacterNamePosition / CharacterNameFormat: where and how a character name is attached to a
  replica or merged group (one `attach` for every emission site)
- ExportFormat / ExportStage: the export in flight (dialog open, or the worker writing)

Key functions:
- compose_translation_text(): entry point, dispatches to `compose_plain` / `compose_minijinja`
- compose_plain(): filter + sort + format, then `emit_composition_items`
- classify_hint_bindings(): one backward scan resolving every hint's attachment
- emit_composition_items(): pure emission pass — limit, character merging, hint attachment
- write_export(): worker half of an export (write the whole file, then commit the target)

Notes:
Formatting is separated from emission so the ordering, limit and hint-attachment rules can be
unit-tested without a `ProjectData`. Composition of an input containing no hints must stay
byte-identical to the pre-hint composer.
*/

use ms_project::{Bubble, ProjectData};
use crate::panels::composition_presets::CompositionPresets;
use crate::panels::bubbles::{
    bubble_extra_bool, bubble_extra_i32, bubble_extra_string,
};
use ms_widgets::WheelSpinBox;
use eframe::egui;
use minijinja::{AutoEscape, Environment, context};
use ms_platform::file_dialog::{self, DialogKind, FileDialogRequest, PendingDialog, SaveTarget};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

const LIMIT_MIN: usize = 100;
const LIMIT_MAX: usize = 100_000;
const LIMIT_DEFAULT: usize = 700;
/// Default `character_name_prefix`: with the default `After` position it reproduces the
/// historical `"{line} - {name}"` byte for byte. Mirrored in `ms_config`'s composition defaults.
const DEFAULT_CHARACTER_NAME_PREFIX: &str = " - ";
/// Runtime (not `const`) because `t!` is not const; resolves the active catalog value.
#[must_use]
fn no_items_text() -> &'static str {
    t!("translation.composition.no_replicas")
}
/// Runtime (not `const`) because `t!` is not const; resolves the active catalog value.
#[must_use]
fn empty_template_text() -> &'static str {
    t!("translation.composition.template_empty")
}
/// Runtime (not `const`) because `t!` is not const; resolves the active catalog value.
#[must_use]
fn unknown_character_text() -> &'static str {
    t!("translation.composition.unknown_character")
}
/// Runtime (not `const`) because `t!` is not const; resolves the active catalog value.
#[must_use]
fn narrator_character_text() -> &'static str {
    t!("translation.composition.narrator")
}
/// File stem the export dialog suggests (an on-disk name, not UI text).
const DEFAULT_EXPORT_NAME: &str = "composition_export";
const COMPOSED_TEXT_ROWS: usize = 12;
const TEMPLATE_ROWS: usize = 6;
const VARS_ROWS: usize = 6;
/// Runtime (not `const`) because `t!` is not const; resolves the active catalog value.
#[must_use]
fn vars_info_text() -> &'static str {
    t!("translation.composition.variables_help")
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum CompositionSortMethod {
    #[default]
    Height,
    Order,
}

impl CompositionSortMethod {
    pub fn key(self) -> &'static str {
        match self {
            CompositionSortMethod::Height => "height",
            CompositionSortMethod::Order => "order",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            CompositionSortMethod::Height => t!("translation.composition.sort_by_height"),
            CompositionSortMethod::Order => t!("translation.composition.sort_by_replica_number"),
        }
    }

    pub fn from_key(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("order") {
            Self::Order
        } else {
            Self::Height
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum CompositionSourceMode {
    #[default]
    Original,
    Translation,
}

impl CompositionSourceMode {
    pub fn key(self) -> &'static str {
        match self {
            CompositionSourceMode::Original => "original",
            CompositionSourceMode::Translation => "translation",
        }
    }

    pub fn from_key(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("translation") {
            Self::Translation
        } else {
            Self::Original
        }
    }
}

/// Where a replica's character name is attached in the plain composer (see
/// [`CharacterNameFormat::attach`]). `After` is the historical layout (`"{line} - {name}"`).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum CharacterNamePosition {
    /// `{prefix}{name}{suffix}{line}`.
    Before,
    /// `{line}{prefix}{name}{suffix}`.
    #[default]
    After,
}

impl CharacterNamePosition {
    /// Persistence key (settings / preset JSON); never localize or rename.
    pub fn key(self) -> &'static str {
        match self {
            CharacterNamePosition::Before => "before",
            CharacterNamePosition::After => "after",
        }
    }

    /// Parses [`Self::key`]; anything unrecognized reads as the default `After`.
    pub fn from_key(raw: &str) -> Self {
        if raw.trim().eq_ignore_ascii_case("before") {
            Self::Before
        } else {
            Self::After
        }
    }
}

/// Every composition setting. One value is both the per-title `settings.json` `composition`
/// section and the payload of a composition preset (`composition_presets`): its ONE JSON
/// mapping is [`write_composition_options_json`] / [`read_composition_options_json`].
/// `PartialEq` compares [`Self::stored_form`] values to tell "the preset differs from the
/// screen".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositionPanelOptions {
    pub sort_method: CompositionSortMethod,
    pub source_mode: CompositionSourceMode,
    pub ignore_translated_lines: bool,
    pub merge_same_character: bool,
    pub sep_same_character: String,
    pub sep_between: String,
    pub replica_prefix: String,
    pub nl_replace: String,
    pub nl_replace_enabled: bool,
    pub wrap_with: String,
    pub wrap_with_enabled: bool,
    pub limit: usize,
    pub limit_enabled: bool,
    pub use_character_names: bool,
    /// Side of the replica (or merged group) the character name goes to. Plain path only;
    /// ignored while `use_character_names` is off.
    pub character_name_position: CharacterNamePosition,
    /// Text placed immediately before the character name. No separator is added between the
    /// replica and the name, so this field carries it: the default `" - "` with `After`
    /// reproduces the historical `"{line} - {name}"`. Escapes (`\n`, `\t`, …) are decoded at
    /// compose time.
    pub character_name_prefix: String,
    /// Text placed immediately after the character name (e.g. `": "` with `Before`). Escapes are
    /// decoded at compose time.
    pub character_name_suffix: String,
    /// Include image-bubble translations in the composition. When enabled, each text area of an
    /// image bubble contributes one line `{translation}` (plus ` - {description}` when
    /// `use_character_names` is on and the description is non-empty).
    pub include_image_bubbles: bool,
    /// Include hint bubbles in the composition. When disabled, hints are dropped in both the
    /// plain and the MiniJinja path.
    pub include_hint_bubbles: bool,
    /// Bracket pair a hint line is wrapped in, normalized to exactly two characters by
    /// [`normalize_wrap_with`]. Ignored when `hint_wrap_enabled` is off.
    pub hint_wrap: String,
    pub hint_wrap_enabled: bool,
    /// Optional extra padding placed inside the hint entry, before and after the wrapped line,
    /// on top of the global `sep_between`. Escapes (`\n`, `\t`, …) are decoded at compose time.
    pub hint_extra_sep: String,
    pub jinja2_enabled: bool,
    pub jinja2_template: String,
}

impl Default for CompositionPanelOptions {
    fn default() -> Self {
        Self {
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
            limit: LIMIT_DEFAULT,
            limit_enabled: true,
            use_character_names: true,
            character_name_position: CharacterNamePosition::After,
            character_name_prefix: DEFAULT_CHARACTER_NAME_PREFIX.to_string(),
            character_name_suffix: String::new(),
            include_image_bubbles: false,
            include_hint_bubbles: true,
            hint_wrap: "()".to_string(),
            hint_wrap_enabled: true,
            hint_extra_sep: String::new(),
            jinja2_enabled: false,
            jinja2_template: String::new(),
        }
    }
}

impl CompositionPanelOptions {
    /// Clamps the character limit and forces both wrap fields to a two-character pair.
    pub fn normalize(&mut self) {
        self.wrap_with = normalize_wrap_with(&self.wrap_with);
        self.hint_wrap = normalize_wrap_with(&self.hint_wrap);
        self.limit = self.limit.clamp(LIMIT_MIN, LIMIT_MAX);
    }

    /// These options as they read back after a write + read through the JSON codec
    /// ([`write_composition_options_json`] then [`read_composition_options_json`]): normalized,
    /// and an empty `nl_replace` stored as one space ([`stored_nl_replace`]). Compare THIS
    /// form, never the raw options, against a persisted value, or a reloaded title would show
    /// a difference that is only the codec's own rewriting. Guarded by the round-trip test.
    #[must_use]
    pub fn stored_form(&self) -> Self {
        let mut out = self.clone();
        out.nl_replace = stored_nl_replace(&out.nl_replace);
        out.normalize();
        out
    }
}

/// The value `nl_replace` is persisted as: an empty field means "replace with a space" (the
/// field's hint), so it is stored as `" "`.
#[must_use]
fn stored_nl_replace(value: &str) -> String {
    if value.is_empty() { " ".to_string() } else { value.to_string() }
}

/// Writes every field of `options` into a composition JSON object (the title `settings.json`
/// `composition` section, or one entry of `composition_presets`). Keys the codec does not own
/// (e.g. the title's selected-preset key) are left untouched. The ONE writer of this mapping.
///
/// The exhaustive destructuring is deliberate: a new option field must not compile until it
/// is given a key here and in [`read_composition_options_json`].
pub fn write_composition_options_json(obj: &mut Map<String, Value>, options: &CompositionPanelOptions) {
    let CompositionPanelOptions {
        sort_method,
        source_mode,
        ignore_translated_lines,
        merge_same_character,
        sep_same_character,
        sep_between,
        replica_prefix,
        nl_replace,
        nl_replace_enabled,
        wrap_with,
        wrap_with_enabled,
        limit,
        limit_enabled,
        use_character_names,
        character_name_position,
        character_name_prefix,
        character_name_suffix,
        include_image_bubbles,
        include_hint_bubbles,
        hint_wrap,
        hint_wrap_enabled,
        hint_extra_sep,
        jinja2_enabled,
        jinja2_template,
    } = options;
    let mut put = |key: &str, value: Value| {
        obj.insert(key.to_string(), value);
    };
    put("method", Value::String(sort_method.key().to_string()));
    put("source_mode", Value::String(source_mode.key().to_string()));
    put("ignore_translated_lines", Value::Bool(*ignore_translated_lines));
    put("merge_same_character", Value::Bool(*merge_same_character));
    put("sep_same_character", Value::String(sep_same_character.clone()));
    put("sep_between", Value::String(sep_between.clone()));
    put("replica_prefix", Value::String(replica_prefix.clone()));
    put("nl_replace", Value::String(stored_nl_replace(nl_replace)));
    put("nl_replace_enabled", Value::Bool(*nl_replace_enabled));
    put("wrap_with", Value::String(normalize_wrap_with(wrap_with)));
    put("wrap_with_enabled", Value::Bool(*wrap_with_enabled));
    // `usize` -> `u64` is lossless on every supported target; the fallback only documents
    // the impossible case instead of truncating.
    let limit = u64::try_from((*limit).clamp(LIMIT_MIN, LIMIT_MAX)).unwrap_or(u64::MAX);
    put("limit", Value::Number(limit.into()));
    put("limit_enabled", Value::Bool(*limit_enabled));
    put("use_character_names", Value::Bool(*use_character_names));
    put("character_name_position", Value::String(character_name_position.key().to_string()));
    put("character_name_prefix", Value::String(character_name_prefix.clone()));
    put("character_name_suffix", Value::String(character_name_suffix.clone()));
    put("include_image_bubbles", Value::Bool(*include_image_bubbles));
    put("include_hint_bubbles", Value::Bool(*include_hint_bubbles));
    put("hint_wrap", Value::String(normalize_wrap_with(hint_wrap)));
    put("hint_wrap_enabled", Value::Bool(*hint_wrap_enabled));
    put("hint_extra_sep", Value::String(hint_extra_sep.clone()));
    put("jinja2_enabled", Value::Bool(*jinja2_enabled));
    put("jinja2_template", Value::String(jinja2_template.clone()));
}

/// Overlays the keys present in a composition JSON object onto `options`, then normalizes
/// them. An absent key, or one of the wrong JSON type, leaves its field as it was; callers
/// that want "absent = default" start from `CompositionPanelOptions::default()`. The ONE
/// reader of the mapping written by [`write_composition_options_json`].
pub fn read_composition_options_json(obj: &Map<String, Value>, options: &mut CompositionPanelOptions) {
    let CompositionPanelOptions {
        sort_method,
        source_mode,
        ignore_translated_lines,
        merge_same_character,
        sep_same_character,
        sep_between,
        replica_prefix,
        nl_replace,
        nl_replace_enabled,
        wrap_with,
        wrap_with_enabled,
        limit,
        limit_enabled,
        use_character_names,
        character_name_position,
        character_name_prefix,
        character_name_suffix,
        include_image_bubbles,
        include_hint_bubbles,
        hint_wrap,
        hint_wrap_enabled,
        hint_extra_sep,
        jinja2_enabled,
        jinja2_template,
    } = options;
    let read_bool = |key: &str, field: &mut bool| {
        if let Some(value) = obj.get(key).and_then(Value::as_bool) {
            *field = value;
        }
    };
    read_bool("ignore_translated_lines", ignore_translated_lines);
    read_bool("merge_same_character", merge_same_character);
    read_bool("nl_replace_enabled", nl_replace_enabled);
    read_bool("wrap_with_enabled", wrap_with_enabled);
    read_bool("limit_enabled", limit_enabled);
    read_bool("use_character_names", use_character_names);
    read_bool("include_image_bubbles", include_image_bubbles);
    read_bool("include_hint_bubbles", include_hint_bubbles);
    read_bool("hint_wrap_enabled", hint_wrap_enabled);
    read_bool("jinja2_enabled", jinja2_enabled);
    let read_string = |key: &str, field: &mut String| {
        if let Some(value) = obj.get(key).and_then(Value::as_str) {
            *field = value.to_string();
        }
    };
    read_string("sep_same_character", sep_same_character);
    read_string("sep_between", sep_between);
    read_string("replica_prefix", replica_prefix);
    read_string("nl_replace", nl_replace);
    read_string("wrap_with", wrap_with);
    read_string("hint_wrap", hint_wrap);
    read_string("hint_extra_sep", hint_extra_sep);
    read_string("character_name_prefix", character_name_prefix);
    read_string("character_name_suffix", character_name_suffix);
    read_string("jinja2_template", jinja2_template);
    if let Some(raw) = obj.get("method").and_then(Value::as_str) {
        *sort_method = CompositionSortMethod::from_key(raw);
    }
    if let Some(raw) = obj.get("source_mode").and_then(Value::as_str) {
        *source_mode = CompositionSourceMode::from_key(raw);
    }
    if let Some(raw) = obj.get("character_name_position").and_then(Value::as_str) {
        *character_name_position = CharacterNamePosition::from_key(raw);
    }
    if let Some(value) = obj.get("limit").and_then(Value::as_u64) {
        // Out of `usize` range can only mean "huge"; `normalize` clamps it to the maximum.
        *limit = usize::try_from(value).unwrap_or(LIMIT_MAX);
    }
    options.normalize();
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum CompositionTab {
    #[default]
    Text,
    Params,
}

#[derive(Debug, Clone)]
struct CompositionNotice {
    message: String,
    is_error: bool,
}

/// A composed-text export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Txt,
    Docx,
}

impl ExportFormat {
    /// File extension (lowercase, no dot).
    fn extension(self) -> &'static str {
        match self {
            ExportFormat::Txt => "txt",
            ExportFormat::Docx => "docx",
        }
    }

    /// Label of the save dialog's filter.
    fn filter_label(self) -> &'static str {
        match self {
            ExportFormat::Txt => t!("translation.composition.txt_files_filter"),
            ExportFormat::Docx => t!("translation.composition.docx_files_filter"),
        }
    }

    /// Success notice naming the written file.
    fn saved_status(self, name: &str) -> String {
        match self {
            ExportFormat::Txt => tf!("translation.composition.txt_saved_status", path = name),
            ExportFormat::Docx => tf!("translation.composition.docx_saved_status", path = name),
        }
    }

    /// Failure notice wrapping an already-localized or OS error text.
    fn save_error(self, err: &str) -> String {
        match self {
            ExportFormat::Txt => tf!("translation.composition.txt_save_error", err = err),
            ExportFormat::Docx => tf!("translation.composition.docx_save_error", err = err),
        }
    }
}

/// The export in flight; at most one, and both export buttons are disabled meanwhile.
#[derive(Debug)]
enum ExportStage {
    /// The save dialog is open. `text` is the composition as it was when the button was
    /// clicked, so what is written is what the user saw.
    Picking { format: ExportFormat, text: String, dialog: PendingDialog },
    /// A worker writes the file and commits the target; `Ok` carries the file's display name.
    Writing { format: ExportFormat, rx: Receiver<Result<String, String>> },
}

#[derive(Debug)]
pub struct CompositionPanelState {
    tab: CompositionTab,
    pub composed_text: String,
    notice: Option<CompositionNotice>,
    export: Option<ExportStage>,
}

impl Default for CompositionPanelState {
    fn default() -> Self {
        Self {
            tab: CompositionTab::Text,
            composed_text: no_items_text().to_string(),
            notice: None,
            export: None,
        }
    }
}

impl CompositionPanelState {
    fn set_info(&mut self, message: impl Into<String>) {
        self.notice = Some(CompositionNotice {
            message: message.into(),
            is_error: false,
        });
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.notice = Some(CompositionNotice {
            message: message.into(),
            is_error: true,
        });
    }
}

/// What one frame of the composition panel asks of its host.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompositionPanelActions {
    /// The title's `composition` section changed — an option, or the selected preset — and
    /// must be saved.
    pub options_changed: bool,
    /// The composed text must be rebuilt from the (possibly changed) options.
    pub request_rebuild: bool,
}

/// Draws the panel: the «Текст» / «Параметры» sub-tabs over `options`. `presets` is the
/// app-wide composition preset store, drawn at the top of «Параметры».
pub fn draw_composition_panel(
    ui: &mut egui::Ui,
    project: &ProjectData,
    state: &mut CompositionPanelState,
    options: &mut CompositionPanelOptions,
    presets: &mut CompositionPresets,
) -> CompositionPanelActions {
    let mut actions = CompositionPanelActions::default();
    options.normalize();

    ui.horizontal(|ui| {
        if ui
            .selectable_label(state.tab == CompositionTab::Text, t!("translation.composition.text_tab"))
            .clicked()
        {
            state.tab = CompositionTab::Text;
        }
        if ui
            .selectable_label(state.tab == CompositionTab::Params, t!("translation.composition.params_tab"))
            .clicked()
        {
            state.tab = CompositionTab::Params;
        }
    });
    ui.separator();

    match state.tab {
        CompositionTab::Text => draw_text_tab(ui, project, state, options, &mut actions),
        CompositionTab::Params => draw_params_tab(ui, state, options, presets, &mut actions),
    }

    actions
}

pub fn compose_translation_text(
    project: &ProjectData,
    options: &CompositionPanelOptions,
) -> String {
    if options.jinja2_enabled {
        return compose_minijinja(project, options);
    }
    compose_plain(project, options)
}

pub fn normalize_wrap_with(value: &str) -> String {
    let mut chars = value.chars();
    match (chars.next(), chars.next()) {
        (Some(left), Some(right)) => format!("{left}{right}"),
        (Some(one), None) => format!("{one}{one}"),
        _ => "``".to_string(),
    }
}

fn draw_text_tab(
    ui: &mut egui::Ui,
    project: &ProjectData,
    state: &mut CompositionPanelState,
    options: &mut CompositionPanelOptions,
    actions: &mut CompositionPanelActions,
) {
    ui.horizontal(|ui| {
        ui.label(t!("translation.composition.sort_label"));
        if ui
            .button("⇆")
            .on_hover_text(t!("translation.composition.toggle_sort_tooltip"))
            .clicked()
        {
            options.sort_method = if options.sort_method == CompositionSortMethod::Height {
                CompositionSortMethod::Order
            } else {
                CompositionSortMethod::Height
            };
            actions.options_changed = true;
            actions.request_rebuild = true;
        }
        ui.label(options.sort_method.title());
    });

    draw_readonly_big_text(
        ui,
        "composition_text_output",
        &state.composed_text,
        COMPOSED_TEXT_ROWS,
        t!("translation.composition.output_placeholder"),
    );

    poll_export(ui.ctx(), state);
    ui.horizontal_wrapped(|ui| {
        if ui.button(t!("translation.composition.copy_output_button")).clicked() {
            ui.ctx().copy_text(state.composed_text.clone());
            state.set_info(t!("translation.composition.text_copied_status"));
        }
        if ui.button(t!("translation.common.refresh_button")).clicked() {
            actions.request_rebuild = true;
        }
        let export_idle = state.export.is_none();
        if ui.add_enabled(export_idle, egui::Button::new(t!("translation.composition.export_txt_button"))).clicked() {
            start_export(ui.ctx(), project, state, ExportFormat::Txt);
        }
        if ui.add_enabled(export_idle, egui::Button::new(t!("translation.composition.export_docx_button"))).clicked() {
            start_export(ui.ctx(), project, state, ExportFormat::Docx);
        }
        if !export_idle {
            ui.spinner();
        }
    });

    if let Some(notice) = &state.notice {
        let color = if notice.is_error {
            ms_theme::status::ERROR
        } else {
            ms_theme::status::SUCCESS
        };
        ui.colored_label(color, &notice.message);
    }
}

/// The «Параметры» sub-tab: the preset picker first — OUTSIDE the MiniJinja-disabled scope,
/// because a preset also switches `jinja2_enabled` — then every option.
fn draw_params_tab(
    ui: &mut egui::Ui,
    state: &mut CompositionPanelState,
    options: &mut CompositionPanelOptions,
    presets: &mut CompositionPresets,
    actions: &mut CompositionPanelActions,
) {
    let preset_actions = presets.draw(ui, options);
    actions.options_changed |= preset_actions.options_changed;
    actions.request_rebuild |= preset_actions.request_rebuild;
    ui.separator();

    let mut changed = false;
    let use_jinja = options.jinja2_enabled;

    ui.add_enabled_ui(!use_jinja, |ui| {
        ui.horizontal(|ui| {
            ui.label(t!("translation.composition.replicas_label"));
            changed |= ui
                .selectable_value(
                    &mut options.source_mode,
                    CompositionSourceMode::Original,
                    t!("translation.common.original_label"),
                )
                .changed();
            changed |= ui
                .selectable_value(
                    &mut options.source_mode,
                    CompositionSourceMode::Translation,
                    t!("translation.common.translation_label"),
                )
                .changed();
        });

        ui.add_enabled_ui(
            options.source_mode == CompositionSourceMode::Original,
            |ui| {
                changed |= ui
                    .checkbox(
                        &mut options.ignore_translated_lines,
                        t!("translation.composition.ignore_translated_label"),
                    )
                    .changed();
            },
        );

        ui.horizontal(|ui| {
            ui.label(t!("translation.composition.newline_replace_label"));
            changed |= ui.checkbox(&mut options.nl_replace_enabled, "").changed();
            ui.add_enabled_ui(options.nl_replace_enabled, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut options.nl_replace)
                        .desired_width(f32::INFINITY)
                        .hint_text(t!("translation.composition.space_value")),
                );
                if resp.changed() {
                    truncate_chars(&mut options.nl_replace, 8);
                    changed = true;
                }
            });
        });

        ui.horizontal(|ui| {
            ui.label(t!("translation.composition.wrap_replicas_label"));
            changed |= ui.checkbox(&mut options.wrap_with_enabled, "").changed();
            ui.add_enabled_ui(options.wrap_with_enabled, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut options.wrap_with)
                        .desired_width(f32::INFINITY)
                        .hint_text("``"),
                );
                if resp.changed() {
                    truncate_chars(&mut options.wrap_with, 2);
                    options.wrap_with = normalize_wrap_with(&options.wrap_with);
                    changed = true;
                }
            });
        });

        ui.horizontal(|ui| {
            ui.label(t!("translation.composition.replica_prefix_label"));
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut options.replica_prefix)
                        .desired_width(f32::INFINITY),
                )
                .changed();
        });

        ui.horizontal(|ui| {
            ui.label(t!("translation.composition.char_limit_label"));
            changed |= ui.checkbox(&mut options.limit_enabled, "").changed();
            ui.add_enabled_ui(options.limit_enabled, |ui| {
                let mut limit = options.limit as i64;
                let resp = ui.add(
                    WheelSpinBox::new(&mut limit)
                        .range(LIMIT_MIN as i64..=LIMIT_MAX as i64)
                        .speed(1.0),
                );
                if resp.changed() {
                    options.limit = (limit as usize).clamp(LIMIT_MIN, LIMIT_MAX);
                    changed = true;
                }
            });
        });

        changed |= ui
            .checkbox(
                &mut options.use_character_names,
                t!("translation.common.use_character_names_label"),
            )
            .changed();
        // The name layout belongs to the checkbox above: shown only while names are used, and
        // indented so it reads as its sub-options.
        if options.use_character_names {
            ui.indent("translation.composition.character_name_options", |ui| {
                changed |= draw_character_name_options(ui, options);
            });
        }
        changed |= ui
            .checkbox(
                &mut options.merge_same_character,
                t!("translation.composition.merge_same_character_label"),
            )
            .changed();
        changed |= ui
            .checkbox(
                &mut options.include_image_bubbles,
                t!("translation.composition.include_imagebubble_label"),
            )
            .on_hover_text(
                t!("translation.composition.imagebubble_area_hint"),
            )
            .changed();
        changed |= ui
            .checkbox(
                &mut options.include_hint_bubbles,
                t!("translation.composition.include_hint_bubbles_label"),
            )
            .on_hover_text(t!("translation.composition.include_hint_bubbles_hint"))
            .changed();

        // Both hint formatting fields are meaningless while hints are excluded entirely.
        ui.add_enabled_ui(options.include_hint_bubbles, |ui| {
            ui.horizontal(|ui| {
                ui.label(t!("translation.composition.hint_wrap_label"));
                changed |= ui.checkbox(&mut options.hint_wrap_enabled, "").changed();
                ui.add_enabled_ui(options.hint_wrap_enabled, |ui| {
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut options.hint_wrap)
                            .desired_width(f32::INFINITY)
                            .hint_text("()"),
                    );
                    if resp.changed() {
                        truncate_chars(&mut options.hint_wrap, 2);
                        options.hint_wrap = normalize_wrap_with(&options.hint_wrap);
                        changed = true;
                    }
                });
            });

            ui.horizontal(|ui| {
                ui.label(t!("translation.composition.hint_separator_label"));
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut options.hint_extra_sep)
                            .desired_width(f32::INFINITY)
                            .hint_text("\\n"),
                    )
                    .changed();
            });
        });

        ui.horizontal(|ui| {
            ui.label(t!("translation.composition.between_same_character_label"));
            ui.add_enabled_ui(options.merge_same_character, |ui| {
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut options.sep_same_character)
                            .desired_width(f32::INFINITY),
                    )
                    .changed();
            });
        });

        ui.horizontal(|ui| {
            ui.label(t!("translation.composition.between_replicas_label"));
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut options.sep_between)
                        .desired_width(f32::INFINITY),
                )
                .changed();
        });
    });

    ui.separator();
    ui.heading("MiniJinja");
    changed |= ui
        .checkbox(&mut options.jinja2_enabled, t!("translation.composition.use_minijinja_label"))
        .changed();

    ui.horizontal(|ui| {
        ui.label(t!("translation.composition.available_variables_label"));
        if ui.button(t!("translation.common.copy_button")).clicked() {
            ui.ctx().copy_text(vars_info_text().to_string());
            state.set_info(t!("translation.composition.variables_copied_status"));
        }
    });
    draw_readonly_big_text(ui, "composition_vars_info", vars_info_text(), VARS_ROWS, "");
    ui.label(t!("translation.composition.minijinja_template_label"));
    changed |= draw_editable_big_text(
        ui,
        "composition_template_editor",
        &mut options.jinja2_template,
        TEMPLATE_ROWS,
        "{% for bubble in bubbles %}{{ bubble.id }}: {{ bubble.original_text }}\n{% endfor %}",
    );

    if changed {
        actions.options_changed = true;
        actions.request_rebuild = true;
    }
}

/// Sub-options of «Использовать имена персонажей»: the before / after switch and the name's
/// prefix and suffix. Returns whether any of them changed this frame.
fn draw_character_name_options(ui: &mut egui::Ui, options: &mut CompositionPanelOptions) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        changed |= ui
            .selectable_value(
                &mut options.character_name_position,
                CharacterNamePosition::Before,
                t!("translation.composition.name_before_replica_label"),
            )
            .changed();
        changed |= ui
            .selectable_value(
                &mut options.character_name_position,
                CharacterNamePosition::After,
                t!("translation.composition.name_after_replica_label"),
            )
            .changed();
    });
    ui.horizontal(|ui| {
        ui.label(t!("translation.composition.name_prefix_label"));
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut options.character_name_prefix)
                    .id_salt("translation.composition.name_prefix_label")
                    .desired_width(f32::INFINITY),
            )
            .on_hover_text(t!("translation.composition.name_affix_hint"))
            .changed();
    });
    ui.horizontal(|ui| {
        ui.label(t!("translation.composition.name_suffix_label"));
        changed |= ui
            .add(
                egui::TextEdit::singleline(&mut options.character_name_suffix)
                    .id_salt("translation.composition.name_suffix_label")
                    .desired_width(f32::INFINITY),
            )
            .on_hover_text(t!("translation.composition.name_affix_hint"))
            .changed();
    });
    changed
}

fn compose_plain(project: &ProjectData, options: &CompositionPanelOptions) -> String {
    let use_original = options.source_mode == CompositionSourceMode::Original;
    // Every entry is `(bubble, emits)`. An ordinary text bubble that fails the filter is KEPT here
    // with `emits = false`: it must stay in reading order as a barrier, otherwise a hint standing
    // in front of it would silently re-target the next surviving replica further down.
    let mut filtered: Vec<(&Bubble, bool)> = Vec::new();
    for bubble in project.bubbles.iter() {
        // A hint carries its single line in `text` whatever the source mode is, and it is never
        // subject to `ignore_translated_lines`: it is an author note, not a replica. A hint that
        // its own option excludes leaves no barrier — a hint may legitimately skip over hints.
        if is_hint_bubble(bubble) {
            if options.include_hint_bubbles && !bubble.text.trim().is_empty() {
                filtered.push((bubble, true));
            }
            continue;
        }
        // Image bubbles are gated by their own option and always contribute their area translations
        // (independent of the original/translation source mode used for text bubbles). They are
        // never barriers either: a hint binds across an image bubble (the forward-binding rule in
        // `panels/MODULE_README.md`, "Hint attachment in `composition.rs`").
        if is_image_bubble(bubble) {
            if options.include_image_bubbles && !image_bubble_area_translations(bubble).is_empty() {
                filtered.push((bubble, true));
            }
            continue;
        }
        let translation_text = bubble.text.trim();
        let original_text = bubble.original_text.trim();
        let emits = if use_original {
            // Dropped when there is no source at all, or when `ignore_translated_lines` skips an
            // already-translated replica.
            !original_text.is_empty()
                && (translation_text.is_empty() || !options.ignore_translated_lines)
        } else {
            !translation_text.is_empty()
        };
        filtered.push((bubble, emits));
    }

    // Keyed on genuinely emitting entries: a stream that holds nothing but barriers composes to
    // nothing, exactly as it did before barriers were retained.
    if !filtered.iter().any(|(_, emits)| *emits) {
        return no_items_text().to_string();
    }

    filtered.sort_by(|(a, _), (b, _)| match options.sort_method {
        CompositionSortMethod::Height => a
            .img_idx
            .cmp(&b.img_idx)
            .then_with(|| a.img_v.total_cmp(&b.img_v)),
        CompositionSortMethod::Order => {
            let a_order = bubble_extra_i32(&a.extra, "bubble_order", 0);
            let b_order = bubble_extra_i32(&b.extra, "bubble_order", 0);
            a.img_idx
                .cmp(&b.img_idx)
                .then(a_order.cmp(&b_order))
                .then_with(|| a.img_v.total_cmp(&b.img_v))
        }
    });

    let newline_replacement = if options.nl_replace.is_empty() {
        " ".to_string()
    } else {
        options.nl_replace.clone()
    };
    let sep_same_character = decode_separator_text(&options.sep_same_character);
    let sep_between = decode_separator_text(&options.sep_between);
    let (wrap_left, wrap_right) = if options.wrap_with_enabled {
        let normalized = normalize_wrap_with(&options.wrap_with);
        let mut chars = normalized.chars();
        (
            chars.next().unwrap_or('`').to_string(),
            chars.next().unwrap_or('`').to_string(),
        )
    } else {
        (String::new(), String::new())
    };
    let hint_extra_sep = decode_separator_text(&options.hint_extra_sep);
    let (hint_wrap_left, hint_wrap_right) = if options.hint_wrap_enabled {
        let normalized = normalize_wrap_with(&options.hint_wrap);
        let mut chars = normalized.chars();
        (
            chars.next().unwrap_or('(').to_string(),
            chars.next().unwrap_or(')').to_string(),
        )
    } else {
        (String::new(), String::new())
    };
    let nl_replace = if options.nl_replace_enabled {
        Some(newline_replacement.as_str())
    } else {
        None
    };

    // Formatting pass: turn every surviving bubble into a ready-to-emit entry. An ordinary text
    // bubble that emits nothing — filtered above, or normalizing to an empty line here — still
    // produces a `DroppedReplica` barrier so hint lookahead sees it.
    let mut items = Vec::<ComposedItem>::with_capacity(filtered.len());
    for (bubble, emits) in filtered {
        // Only ordinary text bubbles are ever marked non-emitting; hints and image bubbles that
        // fail their own gate were left out of `filtered` entirely.
        if !emits {
            items.push(ComposedItem::DroppedReplica);
            continue;
        }

        if is_hint_bubble(bubble) {
            let Some(normalized_text) = normalize_composition_text(bubble.text.trim(), nl_replace)
            else {
                continue;
            };
            // No character name and no replica prefix are ever attached to a hint.
            items.push(ComposedItem::Hint {
                text: format!(
                    "{hint_extra_sep}{hint_wrap_left}{normalized_text}{hint_wrap_right}{hint_extra_sep}"
                ),
            });
            continue;
        }

        if is_image_bubble(bubble) {
            let mut lines = Vec::new();
            for (translation, description) in image_bubble_area_translations(bubble) {
                let Some(normalized_text) = normalize_composition_text(&translation, nl_replace)
                else {
                    continue;
                };
                let mut line_text = format!(
                    "{}{}{}{}",
                    options.replica_prefix, wrap_left, normalized_text, wrap_right
                );
                // "{translation} - {description}" only when character names are enabled and the
                // description is non-empty; otherwise just the translation (no trailing dash).
                if options.use_character_names {
                    let description = description.trim();
                    if !description.is_empty() {
                        line_text.push_str(" - ");
                        line_text.push_str(description);
                    }
                }
                lines.push(line_text);
            }
            items.push(ComposedItem::ImageAreas { lines });
            continue;
        }

        let source_text = if use_original {
            bubble.original_text.trim()
        } else {
            bubble.text.trim()
        };
        let Some(normalized_text) = normalize_composition_text(source_text, nl_replace) else {
            // The bubble is an ordinary replica that produces no line after normalization; it must
            // still act as a barrier for a hint standing in front of it.
            items.push(ComposedItem::DroppedReplica);
            continue;
        };
        items.push(ComposedItem::Replica {
            line: format!(
                "{}{}{}{}",
                options.replica_prefix, wrap_left, normalized_text, wrap_right
            ),
            character: if options.use_character_names {
                bubble_character_text(bubble)
            } else {
                String::new()
            },
        });
    }

    let name_prefix = decode_separator_text(&options.character_name_prefix);
    let name_suffix = decode_separator_text(&options.character_name_suffix);
    let result_lines = emit_composition_items(
        &items,
        EmitParams {
            sep_between: &sep_between,
            sep_same_character: &sep_same_character,
            use_character_names: options.use_character_names,
            name_format: CharacterNameFormat {
                position: options.character_name_position,
                prefix: &name_prefix,
                suffix: &name_suffix,
            },
            merge_same_character: options.merge_same_character,
            limit_enabled: options.limit_enabled,
            limit: options.limit,
        },
    );

    if result_lines.is_empty() {
        no_items_text().to_string()
    } else {
        result_lines.join(&sep_between)
    }
}

/// One filtered and formatted composition entry, ready for the emission pass.
///
/// Splitting formatting (which needs a `Bubble`) from emission (ordering, the character limit,
/// same-character merging and hint attachment) keeps the emission rules unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ComposedItem {
    /// Ordinary replica: its formatted line and the character it is attributed to.
    /// `character` is empty and unused when character names are disabled.
    Replica { line: String, character: String },
    /// Image bubble: one formatted line per non-empty text area (possibly none). An image entry
    /// with no lines still breaks a merged-character group, exactly like one with lines.
    ImageAreas { lines: Vec<String> },
    /// Hint bubble: the fully formatted entry text, wrap and extra separators already applied.
    Hint { text: String },
    /// An ordinary text bubble that exists in reading order but emits nothing (filtered as
    /// already-translated or source-less, or normalizing to an empty line).
    ///
    /// It emits no text and never disturbs a merge group; its only role is to be a **barrier**
    /// that stops a preceding hint's lookahead, so a hint cannot re-target the next surviving
    /// replica further down.
    DroppedReplica,
}

/// How a hint binds to the replica stream, decided by a lookahead over the whole item list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HintBinding {
    /// The next replica-or-barrier is an emitting `Replica`: the hint queues up and is emitted
    /// immediately in front of that replica, atomically with it.
    Forward,
    /// The next replica-or-barrier is a `DroppedReplica` barrier: the bubble the hint comments on
    /// will not be inserted, so the hint is not inserted either.
    Dropped,
    /// Neither a replica nor a barrier follows: the hint binds backward to the previous entry and
    /// is emitted at the very end, past the character limit.
    Trailing,
}

/// Resolves the binding of every [`ComposedItem::Hint`] in `items` with one backward scan.
///
/// Returns a vector parallel to `items`, holding `Some(binding)` at each hint position and `None`
/// everywhere else. `ImageAreas` entries and other hints are transparent to the lookahead — a hint
/// legitimately skips over them; only an emitting `Replica` and a `DroppedReplica` barrier stop it.
fn classify_hint_bindings(items: &[ComposedItem]) -> Vec<Option<HintBinding>> {
    let mut bindings = Vec::<Option<HintBinding>>::with_capacity(items.len());
    // The nearest replica-or-barrier seen so far while scanning backwards, already expressed as
    // the binding a hint at the current position would get. `None` means neither has been seen.
    let mut next_gate: Option<HintBinding> = None;
    for item in items.iter().rev() {
        let binding = match item {
            ComposedItem::Replica { .. } => {
                next_gate = Some(HintBinding::Forward);
                None
            }
            ComposedItem::DroppedReplica => {
                next_gate = Some(HintBinding::Dropped);
                None
            }
            ComposedItem::ImageAreas { .. } => None,
            ComposedItem::Hint { .. } => Some(next_gate.unwrap_or(HintBinding::Trailing)),
        };
        bindings.push(binding);
    }
    // Built back-to-front by the lookahead; restore item order.
    bindings.reverse();
    bindings
}

/// Emission-time settings of the plain composer: everything that is decided after per-bubble
/// formatting. Borrowed separators keep the struct allocation-free.
#[derive(Debug, Clone, Copy)]
struct EmitParams<'a> {
    sep_between: &'a str,
    sep_same_character: &'a str,
    use_character_names: bool,
    /// How a character name is attached to a replica or a merged group; used only while
    /// `use_character_names` is on.
    name_format: CharacterNameFormat<'a>,
    merge_same_character: bool,
    limit_enabled: bool,
    limit: usize,
}

/// Compose-time form of the character-name options: escapes already decoded, borrowed so
/// [`EmitParams`] stays `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CharacterNameFormat<'a> {
    position: CharacterNamePosition,
    prefix: &'a str,
    suffix: &'a str,
}

impl CharacterNameFormat<'_> {
    /// Joins `text` (one replica line or a merged group) with `name`, adding nothing but the
    /// prefix and suffix: `Before` -> `{prefix}{name}{suffix}{text}`, `After` ->
    /// `{text}{prefix}{name}{suffix}`. Any separator between the text and the name is part of
    /// the prefix (`After`) or the suffix (`Before`).
    #[must_use]
    fn attach(&self, text: &str, name: &str) -> String {
        let Self { position, prefix, suffix } = *self;
        match position {
            CharacterNamePosition::Before => format!("{prefix}{name}{suffix}{text}"),
            CharacterNamePosition::After => format!("{text}{prefix}{name}{suffix}"),
        }
    }
}

/// Emits `items` into the final list of composition entries, applying the character limit,
/// same-character merging and the hint attachment rule. Entries are joined with `sep_between`
/// by the caller.
///
/// Hint attachment: a hint is never emitted where it stands. [`classify_hint_bindings`] decides,
/// per hint, what the next replica-or-barrier in the stream is:
///
/// - an emitting `Replica` — the hint queues up and is flushed immediately in front of that
///   replica, atomically with it, so a hint can never outlive the replica it comments on. When the
///   pair does not fit the limit, composition stops and the queued hints are dropped.
/// - a `DroppedReplica` barrier — the bubble the hint comments on is not inserted, so the hint is
///   dropped and never queued. It must not re-target the next surviving replica further down.
/// - neither — the hint is trailing: it binds backward to the previous entry and is emitted at the
///   very end past the character limit, whether or not the loop stopped on the limit elsewhere.
///
/// Image entries and further hints are transparent to that lookahead.
fn emit_composition_items(items: &[ComposedItem], params: EmitParams<'_>) -> Vec<String> {
    let bindings = classify_hint_bindings(items);
    let mut result_lines = Vec::<String>::new();
    let mut current_length = 0_usize;
    let mut prev_character: Option<String> = None;
    let mut current_group = Vec::<String>::new();
    let mut pending_hints = Vec::<String>::new();

    for (item, binding) in items.iter().zip(bindings.iter()) {
        match item {
            ComposedItem::DroppedReplica => {
                // A barrier emits nothing and never disturbs an open merge group; it exists only
                // to stop the hint lookahead performed above.
            }
            ComposedItem::Hint { text } => {
                // Only a forward-bound hint queues. A dropped one comments on a replica that is
                // never inserted; a trailing one is emitted by the tail flush below, and queuing
                // it here would make it die with the loop when the limit stops composition.
                if *binding == Some(HintBinding::Forward) {
                    pending_hints.push(text.clone());
                }
            }
            ComposedItem::ImageAreas { lines } => {
                // Flush any pending merged-character group first so image lines keep reading
                // order. Pending hints deliberately survive an image bubble: a hint binds to the
                // next ordinary replica further down.
                if params.use_character_names
                    && params.merge_same_character
                    && !current_group.is_empty()
                    && let Some(prev) = prev_character.take()
                {
                    let group_text =
                        params.name_format.attach(&current_group.join(params.sep_same_character), &prev);
                    current_group.clear();
                    if !append_result_item(
                        &mut result_lines,
                        &mut current_length,
                        params.sep_between,
                        &group_text,
                        params.limit_enabled,
                        params.limit,
                    ) {
                        break;
                    }
                }
                prev_character = None;
                let mut hit_limit = false;
                for line_text in lines {
                    if !append_result_item(
                        &mut result_lines,
                        &mut current_length,
                        params.sep_between,
                        line_text,
                        params.limit_enabled,
                        params.limit,
                    ) {
                        hit_limit = true;
                        break;
                    }
                }
                if hit_limit {
                    break;
                }
            }
            ComposedItem::Replica { line, character } => {
                if !params.use_character_names {
                    if !admit_replica_with_hints(
                        &mut result_lines,
                        &mut current_length,
                        &mut pending_hints,
                        params,
                        line,
                    ) {
                        break;
                    }
                    continue;
                }

                if !params.merge_same_character {
                    let single_line = params.name_format.attach(line, character);
                    if !admit_replica_with_hints(
                        &mut result_lines,
                        &mut current_length,
                        &mut pending_hints,
                        params,
                        &single_line,
                    ) {
                        break;
                    }
                    continue;
                }

                // A hint must sit immediately in front of its target replica, so with merging on
                // it acts as a group boundary: flush the open group, drop character continuity,
                // emit the hints, and let the target replica open a fresh group. Accepted
                // approximation: the atomic limit check covers the hints plus this replica's own
                // line, not the whole group the line will eventually join.
                if !pending_hints.is_empty() {
                    let group_text = if current_group.is_empty() {
                        None
                    } else {
                        prev_character.as_ref().map(|prev| {
                            params.name_format.attach(&current_group.join(params.sep_same_character), prev)
                        })
                    };
                    let fits = {
                        let mut planned = Vec::<&str>::with_capacity(pending_hints.len() + 2);
                        if let Some(text) = group_text.as_deref() {
                            planned.push(text);
                        }
                        planned.extend(pending_hints.iter().map(String::as_str));
                        planned.push(line);
                        items_fit(current_length, result_lines.len(), params, &planned)
                    };
                    if !fits {
                        break;
                    }
                    // `items_fit` has already taken the decision for the whole bundle, so these
                    // appends are unconditional: re-checking the limit element by element could
                    // emit the hints and then reject the replica they annotate.
                    if let Some(text) = group_text.as_deref() {
                        force_append_result_item(
                            &mut result_lines,
                            &mut current_length,
                            params.sep_between,
                            text,
                        );
                        current_group.clear();
                    }
                    prev_character = None;
                    for hint in pending_hints.drain(..) {
                        force_append_result_item(
                            &mut result_lines,
                            &mut current_length,
                            params.sep_between,
                            &hint,
                        );
                    }
                }

                match prev_character.as_ref() {
                    None => {
                        current_group.clear();
                        current_group.push(line.clone());
                        prev_character = Some(character.clone());
                    }
                    Some(prev) if prev == character => {
                        current_group.push(line.clone());
                    }
                    Some(prev) => {
                        let group_text =
                            params.name_format.attach(&current_group.join(params.sep_same_character), prev);
                        if !append_result_item(
                            &mut result_lines,
                            &mut current_length,
                            params.sep_between,
                            &group_text,
                            params.limit_enabled,
                            params.limit,
                        ) {
                            break;
                        }
                        current_group.clear();
                        current_group.push(line.clone());
                        prev_character = Some(character.clone());
                    }
                }
            }
        }
    }

    if params.merge_same_character
        && !current_group.is_empty()
        && let Some(prev) = prev_character
    {
        let group_text = params.name_format.attach(&current_group.join(params.sep_same_character), &prev);
        force_append_result_item(
            &mut result_lines,
            &mut current_length,
            params.sep_between,
            &group_text,
        );
    }

    // Trailing hints have no following ordinary replica anywhere in the stream, so they bind
    // backward to the previous entry and are emitted past the character limit. They were never
    // queued, so this holds whether or not the loop above stopped on the limit.
    for (item, binding) in items.iter().zip(bindings.iter()) {
        if *binding != Some(HintBinding::Trailing) {
            continue;
        }
        if let ComposedItem::Hint { text } = item {
            force_append_result_item(
                &mut result_lines,
                &mut current_length,
                params.sep_between,
                text,
            );
        }
    }

    result_lines
}

/// Emits the queued hints and then `item_text` as one atomic unit: either all of them are
/// appended and the queue is cleared, or nothing is appended and composition must stop
/// (returns `false`).
///
/// The atomicity is what binds a hint to its replica: appending the hints first and only then
/// failing on the replica would leave a note about a line that never made it into the text.
/// Conversely, the "the very first entry is always admitted" privilege applies to the whole
/// bundle — otherwise a leading hint would take that privilege for itself and starve the replica
/// it annotates, deleting a line that would have been emitted without the hint.
fn admit_replica_with_hints(
    result_lines: &mut Vec<String>,
    current_length: &mut usize,
    pending_hints: &mut Vec<String>,
    params: EmitParams<'_>,
    item_text: &str,
) -> bool {
    let fits = {
        let mut planned = Vec::<&str>::with_capacity(pending_hints.len() + 1);
        planned.extend(pending_hints.iter().map(String::as_str));
        planned.push(item_text);
        items_fit(*current_length, result_lines.len(), params, &planned)
    };
    if !fits {
        return false;
    }
    // The decision was taken for the bundle as a whole, so the appends themselves are
    // unconditional; re-checking the limit per element would break the atomicity.
    for hint in pending_hints.drain(..) {
        force_append_result_item(result_lines, current_length, params.sep_between, &hint);
    }
    force_append_result_item(result_lines, current_length, params.sep_between, item_text);
    true
}

/// Simulates appending `texts` in order through [`append_result_item`] and reports whether every
/// one of them would be admitted under the character limit.
///
/// `result_len` is the number of entries already emitted. When nothing has been emitted yet the
/// whole bundle is admitted unconditionally: this is the historical "the very first entry always
/// gets in, however long it is" rule, widened from a single entry to the atomic unit so a leading
/// hint cannot consume the privilege and then reject its own target replica.
fn items_fit(
    current_length: usize,
    result_len: usize,
    params: EmitParams<'_>,
    texts: &[&str],
) -> bool {
    if result_len == 0 {
        return true;
    }
    let mut length = current_length;
    for text in texts {
        let new_length = length.saturating_add(char_len(params.sep_between) + char_len(text));
        if params.limit_enabled && new_length > params.limit {
            return false;
        }
        length = new_length;
    }
    true
}

/// Normalizes one composition line: folds CRLF/CR to `\n`, optionally replaces every newline
/// with `nl_replace`, collapses runs of inline whitespace and trims the result.
///
/// Returns `None` when nothing is left, in which case the bubble contributes no entry.
fn normalize_composition_text(raw: &str, nl_replace: Option<&str>) -> Option<String> {
    let mut normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    if let Some(replacement) = nl_replace {
        normalized = normalized.replace('\n', replacement);
    }
    let normalized = collapse_inline_whitespace(&normalized).trim().to_string();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn compose_minijinja(project: &ProjectData, options: &CompositionPanelOptions) -> String {
    if options.jinja2_template.trim().is_empty() {
        return empty_template_text().to_string();
    }

    let mut env = Environment::new();
    env.set_auto_escape_callback(|_| AutoEscape::None);
    if let Err(err) = env.add_template("composition", &options.jinja2_template) {
        return tf!("translation.composition.minijinja_error", err = err);
    }
    let template = match env.get_template("composition") {
        Ok(template) => template,
        Err(err) => return tf!("translation.composition.minijinja_error", err = err),
    };

    let bubbles = project
        .bubbles
        .iter()
        // The template path has no ordering, limit or merging to integrate with: a class is
        // simply included or excluded by its own option.
        .filter(|bubble| {
            if is_image_bubble(bubble) {
                return options.include_image_bubbles;
            }
            if is_hint_bubble(bubble) {
                return options.include_hint_bubbles;
            }
            true
        })
        .map(|bubble| serde_json::to_value(bubble).unwrap_or(Value::Null))
        .collect::<Vec<_>>();

    template
        .render(context! { bubbles => bubbles })
        .unwrap_or_else(|err| tf!("translation.composition.minijinja_error", err = err))
}

/// True when the bubble is an `ImageBubble` (`bubble_class == "image"`).
fn is_image_bubble(bubble: &Bubble) -> bool {
    bubble
        .bubble_class
        .as_deref()
        .is_some_and(|class| class.eq_ignore_ascii_case("image"))
}

/// True when the bubble is a hint bubble (`bubble_class == "hint"`), whose single line lives in
/// `Bubble.text` and which is never treated as a replica.
fn is_hint_bubble(bubble: &Bubble) -> bool {
    bubble
        .bubble_class
        .as_deref()
        .is_some_and(|class| class.eq_ignore_ascii_case("hint"))
}

/// Returns `(translation, description)` for each image-bubble text area that has a non-empty
/// translation, in area order. Area 0 reads the legacy `text` / `extra.description` fields; later
/// areas read their entries in `extra["text_areas"]`.
fn image_bubble_area_translations(bubble: &Bubble) -> Vec<(String, String)> {
    let legacy_description = bubble_extra_string(&bubble.extra, "description");
    let mut out = Vec::new();
    match bubble.extra.get("text_areas").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => {
            for (idx, entry) in arr.iter().enumerate() {
                let (translation, description) = if idx == 0 {
                    (bubble.text.clone(), legacy_description.clone())
                } else {
                    (
                        entry
                            .get("translation")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        entry
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                };
                if !translation.trim().is_empty() {
                    out.push((translation, description));
                }
            }
        }
        _ => {
            if !bubble.text.trim().is_empty() {
                out.push((bubble.text.clone(), legacy_description));
            }
        }
    }
    out
}

fn bubble_character_text(bubble: &Bubble) -> String {
    let is_known = bubble_extra_bool(&bubble.extra, "is_known_character", true);
    let character_name = bubble_extra_string(&bubble.extra, "character_name");
    let clarification = bubble_extra_string(&bubble.extra, "clarification");

    let mut character = if !character_name.is_empty() {
        character_name
    } else if is_known {
        unknown_character_text().to_string()
    } else {
        narrator_character_text().to_string()
    };

    if is_known && !clarification.is_empty() {
        character.push_str(" (");
        character.push_str(&clarification);
        character.push(')');
    }
    character
}

/// Appends `item_text` unless the character limit rejects it, returning `false` in that case
/// (composition must then stop). The very first entry is always admitted, however long it is.
///
/// `current_length` tracks the char length of the joined result, separators included.
fn append_result_item(
    result_lines: &mut Vec<String>,
    current_length: &mut usize,
    sep_between: &str,
    item_text: &str,
    use_limit: bool,
    limit: usize,
) -> bool {
    let sep_len = if result_lines.is_empty() {
        0
    } else {
        char_len(sep_between)
    };
    let new_length = current_length.saturating_add(sep_len + char_len(item_text));
    if use_limit && new_length > limit && !result_lines.is_empty() {
        return false;
    }
    result_lines.push(item_text.to_string());
    *current_length = new_length;
    true
}

/// Appends `item_text` unconditionally, bypassing the character limit. It cannot fail, hence no
/// result to check.
///
/// Only for the three places where the decision to emit has already been taken elsewhere: the
/// tail flush of an open merge group, a backward-bound trailing hint, and every element of a
/// bundle already admitted as a whole by [`items_fit`].
fn force_append_result_item(
    result_lines: &mut Vec<String>,
    current_length: &mut usize,
    sep_between: &str,
    item_text: &str,
) {
    let sep_len = if result_lines.is_empty() {
        0
    } else {
        char_len(sep_between)
    };
    *current_length = current_length.saturating_add(sep_len + char_len(item_text));
    result_lines.push(item_text.to_string());
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn collapse_inline_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if matches!(ch, ' ' | '\t') {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn decode_separator_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('0') => out.push('\0'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn truncate_chars(text: &mut String, max_chars: usize) {
    if text.chars().count() <= max_chars {
        return;
    }
    *text = text.chars().take(max_chars).collect::<String>();
}

fn draw_readonly_big_text(
    ui: &mut egui::Ui,
    id_salt: &str,
    source: &str,
    rows: usize,
    hint_text: &str,
) {
    let width = ui.available_width();
    let height = textedit_height(ui, rows);
    let mut text = source.to_string();
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::ScrollArea::both().id_salt(id_salt).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .code_editor()
                        .desired_rows(rows)
                        .desired_width(width)
                        .hint_text(hint_text),
                );
            });
        },
    );
}

fn draw_editable_big_text(
    ui: &mut egui::Ui,
    id_salt: &str,
    text: &mut String,
    rows: usize,
    hint_text: &str,
) -> bool {
    let width = ui.available_width();
    let height = textedit_height(ui, rows);
    ui.allocate_ui_with_layout(
        egui::vec2(width, height),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::ScrollArea::both()
                .id_salt(id_salt)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .code_editor()
                            .desired_rows(rows)
                            .desired_width(width)
                            .hint_text(hint_text),
                    )
                    .changed()
                })
                .inner
        },
    )
    .inner
}

fn textedit_height(ui: &egui::Ui, rows: usize) -> f32 {
    let line_h = ui.text_style_height(&egui::TextStyle::Monospace);
    line_h * rows as f32 + 12.0
}

/// Opens the save dialog of a `format` export without blocking.
///
/// The dialog starts in `ProjectData::user_facing_dir`: the chapter directory of a project,
/// the opened image's folder in a single-image session — never that session's scratch
/// directory, which is deleted on exit.
fn start_export(ctx: &egui::Context, project: &ProjectData, state: &mut CompositionPanelState, format: ExportFormat) {
    let extension = format.extension();
    let request = FileDialogRequest::new(DialogKind::SaveFile)
        .start_dir(&project.user_facing_dir())
        .suggested_name(format!("{DEFAULT_EXPORT_NAME}.{extension}"))
        .filter(format.filter_label(), &[extension]);
    let repaint_ctx = ctx.clone();
    let dialog = file_dialog::request(request, Some(Arc::new(move || repaint_ctx.request_repaint())));
    state.export = Some(ExportStage::Picking { format, text: state.composed_text.clone(), dialog });
}

/// Advances the export in flight: a closed dialog hands its target to the write worker, a
/// finished worker becomes the notice. A cancelled dialog ends the export silently.
fn poll_export(ctx: &egui::Context, state: &mut CompositionPanelState) {
    match state.export.take() {
        None => {}
        Some(ExportStage::Picking { format, text, mut dialog }) => match dialog.poll() {
            None => state.export = Some(ExportStage::Picking { format, text, dialog }),
            Some(Ok(None)) => {}
            Some(Ok(Some(picked))) => match picked.into_save_target() {
                Some(target) => state.export = Some(spawn_export_writer(ctx, format, target, text)),
                // `SaveFile` always answers with a target (`Picked` contract); anything else is a
                // platform-layer bug, logged and treated as a cancel.
                None => ms_log::runtime_log::log_error("[translation] composition export: save dialog returned no target for a SaveFile request"),
            },
            Some(Err(err)) => {
                ms_log::runtime_log::log_warn(format!("[translation] composition {format:?} export: save dialog failed: {err:?}"));
                state.set_error(format.save_error(&err.user_message()));
            }
        },
        Some(ExportStage::Writing { format, rx }) => match rx.try_recv() {
            Ok(Ok(name)) => state.set_info(format.saved_status(&name)),
            Ok(Err(err)) => state.set_error(format.save_error(&err)),
            Err(TryRecvError::Empty) => state.export = Some(ExportStage::Writing { format, rx }),
            Err(TryRecvError::Disconnected) => {
                ms_log::runtime_log::log_error(format!("[translation] composition {format:?} export: writer thread ended without a result"));
                state.set_error(format.save_error(t!("translation.composition.export_worker_lost_error")));
            }
        },
    }
}

/// Starts the worker that writes `text` to `target` and commits it; the GUI is repainted when
/// it finishes.
fn spawn_export_writer(ctx: &egui::Context, format: ExportFormat, target: SaveTarget, text: String) -> ExportStage {
    let (tx, rx) = mpsc::channel();
    let repaint_ctx = ctx.clone();
    ms_thread::spawn(move || {
        let result = write_export(format, target, &text);
        if let Err(err) = &result {
            ms_log::runtime_log::log_warn(format!("[translation] composition {format:?} export failed: {err}"));
        }
        // The receiver is gone only when the panel state was dropped (project closed); there is
        // nobody left to report to.
        if tx.send(result).is_err() {
            ms_log::runtime_log::log_info("[translation] composition export finished after its panel was dropped");
        }
        repaint_ctx.request_repaint();
    });
    ExportStage::Writing { format, rx }
}

/// Writes the COMPLETE export to the target's write path, then commits the target (a blocking
/// stream on Android, so this runs on a worker). Returns the file's display name.
///
/// # Errors
/// The OS error of the directory creation or the write, or the localized commit failure.
fn write_export(format: ExportFormat, target: SaveTarget, text: &str) -> Result<String, String> {
    let path = target.write_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    match format {
        ExportFormat::Txt => fs::write(path, text).map_err(|err| err.to_string())?,
        ExportFormat::Docx => save_simple_docx(path, text)?,
    }
    let name = target.display_name().to_owned();
    target.commit().map_err(|err| err.user_message())?;
    Ok(name)
}

fn save_simple_docx(path: &Path, text: &str) -> Result<(), String> {
    let mut paragraph_xml = String::new();
    if text.is_empty() {
        paragraph_xml.push_str("<w:p/>");
    } else {
        for line in text.split('\n') {
            if line.is_empty() {
                paragraph_xml.push_str("<w:p/>");
                continue;
            }
            paragraph_xml.push_str("<w:p><w:r><w:t xml:space=\"preserve\">");
            paragraph_xml.push_str(&xml_escape(line));
            paragraph_xml.push_str("</w:t></w:r></w:p>");
        }
    }

    let document_xml = format!(
        concat!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
            "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">",
            "<w:body>{}<w:sectPr/></w:body></w:document>"
        ),
        paragraph_xml
    );
    let content_types_xml = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">",
        "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>",
        "<Default Extension=\"xml\" ContentType=\"application/xml\"/>",
        "<Override PartName=\"/word/document.xml\" ",
        "ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>",
        "</Types>"
    );
    let rels_xml = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>",
        "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
        "<Relationship Id=\"rId1\" ",
        "Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" ",
        "Target=\"word/document.xml\"/>",
        "</Relationships>"
    );

    let entries = [
        ("[Content_Types].xml", content_types_xml.as_bytes().to_vec()),
        ("_rels/.rels", rels_xml.as_bytes().to_vec()),
        ("word/document.xml", document_xml.as_bytes().to_vec()),
    ];
    write_zip_store(path, &entries)
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn write_zip_store(path: &Path, entries: &[(&str, Vec<u8>)]) -> Result<(), String> {
    let mut file = fs::File::create(path).map_err(|err| err.to_string())?;
    let mut central = Vec::<u8>::new();
    let mut local_offset = 0_u32;

    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        let crc = crc32(data);
        let size_u32 = u32::try_from(data.len()).map_err(|_| t!("translation.composition.zip_file_too_large_error").to_string())?;
        let name_len =
            u16::try_from(name_bytes.len()).map_err(|_| t!("translation.composition.zip_name_too_long_error").to_string())?;

        write_u32(&mut file, 0x0403_4b50)?;
        write_u16(&mut file, 20)?;
        write_u16(&mut file, 0)?;
        write_u16(&mut file, 0)?;
        write_u16(&mut file, 0)?;
        write_u16(&mut file, 0)?;
        write_u32(&mut file, crc)?;
        write_u32(&mut file, size_u32)?;
        write_u32(&mut file, size_u32)?;
        write_u16(&mut file, name_len)?;
        write_u16(&mut file, 0)?;
        file.write_all(name_bytes).map_err(|err| err.to_string())?;
        file.write_all(data).map_err(|err| err.to_string())?;

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, crc);
        push_u32(&mut central, size_u32);
        push_u32(&mut central, size_u32);
        push_u16(&mut central, name_len);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, local_offset);
        central.extend_from_slice(name_bytes);

        let local_size = 30_u32
            .saturating_add(u32::from(name_len))
            .saturating_add(size_u32);
        local_offset = local_offset.saturating_add(local_size);
    }

    let central_offset = local_offset;
    file.write_all(&central).map_err(|err| err.to_string())?;
    let central_size =
        u32::try_from(central.len()).map_err(|_| t!("translation.composition.zip_archive_too_large_error").to_string())?;
    let entries_count =
        u16::try_from(entries.len()).map_err(|_| t!("translation.composition.zip_too_many_files_error").to_string())?;

    write_u32(&mut file, 0x0605_4b50)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, entries_count)?;
    write_u16(&mut file, entries_count)?;
    write_u32(&mut file, central_size)?;
    write_u32(&mut file, central_offset)?;
    write_u16(&mut file, 0)?;
    Ok(())
}

fn write_u16<W: Write>(w: &mut W, value: u16) -> Result<(), String> {
    w.write_all(&value.to_le_bytes())
        .map_err(|err| err.to_string())
}

fn write_u32<W: Write>(w: &mut W, value: u32) -> Result<(), String> {
    w.write_all(&value.to_le_bytes())
        .map_err(|err| err.to_string())
}

fn push_u16(dst: &mut Vec<u8>, value: u16) {
    dst.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(dst: &mut Vec<u8>, value: u32) {
    dst.extend_from_slice(&value.to_le_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = if (crc & 1) == 1 { 0xedb8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{
        CharacterNameFormat, CharacterNamePosition, ComposedItem, CompositionPanelOptions, CompositionSortMethod, CompositionSourceMode, EmitParams, compose_plain,
        ExportFormat, emit_composition_items, image_bubble_area_translations, is_hint_bubble,
        is_image_bubble, read_composition_options_json, write_composition_options_json, write_export,
    };
    use ms_platform::file_dialog::SaveTarget;
    use ms_project::{Bubble, CanvasSettings, ProjectData, ProjectPaths};
    use serde_json::{Map, Value, json};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// The name format the default options produce: the historical `"{line} - {name}"`.
    const HISTORICAL_NAME_FORMAT: CharacterNameFormat<'static> =
        CharacterNameFormat { position: CharacterNamePosition::After, prefix: " - ", suffix: "" };

    /// Emission settings for the tests: the default separators, an optional character limit.
    fn params(limit: Option<usize>, use_character_names: bool, merge: bool) -> EmitParams<'static> {
        params_with_sep("\n\n", limit, use_character_names, merge)
    }

    /// Same as [`params`] but with an explicit entry separator, for separator-accounting tests.
    fn params_with_sep(
        sep_between: &'static str,
        limit: Option<usize>,
        use_character_names: bool,
        merge: bool,
    ) -> EmitParams<'static> {
        EmitParams {
            sep_between,
            sep_same_character: "\n",
            use_character_names,
            name_format: HISTORICAL_NAME_FORMAT,
            merge_same_character: merge,
            limit_enabled: limit.is_some(),
            limit: limit.unwrap_or(0),
        }
    }

    fn replica(line: &str, character: &str) -> ComposedItem {
        ComposedItem::Replica {
            line: line.to_string(),
            character: character.to_string(),
        }
    }

    fn hint(text: &str) -> ComposedItem {
        ComposedItem::Hint {
            text: text.to_string(),
        }
    }

    fn image(lines: &[&str]) -> ComposedItem {
        ComposedItem::ImageAreas {
            lines: lines.iter().map(|line| (*line).to_string()).collect(),
        }
    }

    /// An ordinary text bubble that exists in reading order but emits nothing.
    fn barrier() -> ComposedItem {
        ComposedItem::DroppedReplica
    }

    /// Write + read over the defaults reproduces `stored_form` exactly, for edge values the
    /// codec rewrites (empty `nl_replace`, short wrap pairs, an out-of-range limit) and for
    /// every non-default field — so a reloaded title or preset never shows a false difference.
    #[test]
    fn codec_round_trip_equals_stored_form() {
        let edge = CompositionPanelOptions {
            sort_method: CompositionSortMethod::Order,
            source_mode: CompositionSourceMode::Translation,
            ignore_translated_lines: false,
            merge_same_character: false,
            sep_same_character: "|".to_string(),
            sep_between: "\\t".to_string(),
            replica_prefix: "- ".to_string(),
            nl_replace: String::new(),
            nl_replace_enabled: false,
            wrap_with: "[".to_string(),
            wrap_with_enabled: false,
            limit: 5,
            limit_enabled: false,
            use_character_names: false,
            character_name_position: CharacterNamePosition::Before,
            character_name_prefix: String::new(),
            character_name_suffix: "\\n".to_string(),
            include_image_bubbles: true,
            include_hint_bubbles: false,
            hint_wrap: String::new(),
            hint_wrap_enabled: false,
            hint_extra_sep: "\\n".to_string(),
            jinja2_enabled: true,
            jinja2_template: "{{ bubbles }}".to_string(),
        };
        for options in [CompositionPanelOptions::default(), edge] {
            let mut obj = Map::new();
            write_composition_options_json(&mut obj, &options);
            let mut read_back = CompositionPanelOptions::default();
            read_composition_options_json(&obj, &mut read_back);
            assert_eq!(read_back, options.stored_form());
            assert_eq!(read_back.stored_form(), read_back, "stored_form is idempotent");
        }
    }

    /// Keys the codec does not own (the title's selected preset) survive a write.
    #[test]
    fn codec_write_keeps_foreign_keys() {
        let mut obj = Map::new();
        obj.insert("preset".to_string(), Value::String("mine".to_string()));
        write_composition_options_json(&mut obj, &CompositionPanelOptions::default());
        assert_eq!(obj.get("preset"), Some(&Value::String("mine".to_string())));
        assert_eq!(obj.get("include_image_bubbles"), Some(&Value::Bool(false)));
    }

    #[test]
    fn hint_binds_to_the_next_replica_across_an_image_bubble() {
        let items = [hint("(note)"), image(&["img"]), replica("a", "A")];
        // The image bubble is emitted where it stands; the hint skips over it and lands
        // immediately in front of the next ordinary replica.
        assert_eq!(
            emit_composition_items(&items, params(None, false, false)),
            vec!["img", "(note)", "a"]
        );
    }

    #[test]
    fn hints_queue_up_and_are_emitted_in_order_before_the_target() {
        let items = [hint("(h1)"), hint("(h2)"), replica("a", "A")];
        assert_eq!(
            emit_composition_items(&items, params(None, false, false)),
            vec!["(h1)", "(h2)", "a"]
        );
    }

    #[test]
    fn hint_and_target_replica_are_dropped_together_on_the_limit() {
        // "aaaa" = 4 chars; with the "\n\n" separator the hint alone would already reach 12 and
        // the target replica 18, while "bbbb" on its own would have fit exactly (10).
        let items = [replica("aaaa", "A"), hint("(hint)"), replica("bbbb", "A")];
        assert_eq!(
            emit_composition_items(&items, params(Some(10), false, false)),
            vec!["aaaa"]
        );
        // Without the hint the second replica does fit, proving the hint's own characters count.
        let without_hint = [replica("aaaa", "A"), replica("bbbb", "A")];
        assert_eq!(
            emit_composition_items(&without_hint, params(Some(10), false, false)),
            vec!["aaaa", "bbbb"]
        );
    }

    #[test]
    fn trailing_hint_is_force_emitted_past_the_limit() {
        // No ordinary replica follows, so the hint binds backward and ignores the limit.
        let items = [replica("aaaa", "A"), hint("(hint)")];
        assert_eq!(
            emit_composition_items(&items, params(Some(10), false, false)),
            vec!["aaaa", "(hint)"]
        );
    }

    #[test]
    fn hint_queued_when_the_limit_stops_composition_is_dropped() {
        // The limit stops the loop on "bbbb", so the hint queued after it never binds forward and
        // must not be force-emitted at the tail either.
        let items = [
            replica("aaaa", "A"),
            replica("bbbb", "A"),
            hint("(hint)"),
            replica("cccc", "A"),
        ];
        assert_eq!(
            emit_composition_items(&items, params(Some(11), false, false)),
            vec!["aaaa", "bbbb"]
        );
    }

    #[test]
    fn hint_breaks_a_merged_character_group() {
        let items = [
            replica("a1", "A"),
            replica("a2", "A"),
            hint("(h)"),
            replica("b1", "A"),
        ];
        // With merging on the hint acts as a group boundary: the open group is flushed, the hint
        // is emitted, and the target replica starts a fresh group even though the character is
        // unchanged.
        assert_eq!(
            emit_composition_items(&items, params(None, true, true)),
            vec!["a1\na2 - A", "(h)", "b1 - A"]
        );
    }

    #[test]
    fn hint_with_merging_off_sits_directly_before_the_named_replica() {
        let items = [replica("a1", "A"), hint("(h)"), replica("b1", "B")];
        assert_eq!(
            emit_composition_items(&items, params(None, true, false)),
            vec!["a1 - A", "(h)", "b1 - B"]
        );
    }

    #[test]
    fn merged_groups_and_image_bubbles_are_unchanged_without_hints() {
        // Regression guard: with no hints in the input the emission must behave exactly as it did
        // before hints existed — a character change flushes the group, an image bubble flushes it
        // and resets the character, and the tail group is force-emitted.
        let items = [
            replica("l1", "A"),
            replica("l2", "A"),
            replica("l3", "B"),
            image(&["img1", "img2"]),
            replica("l4", "B"),
        ];
        assert_eq!(
            emit_composition_items(&items, params(None, true, true)),
            vec!["l1\nl2 - A", "l3 - B", "img1", "img2", "l4 - B"]
        );
        // And the limit still stops composition at the same place.
        let limited = [replica("aaaa", "A"), replica("bbbb", "A")];
        assert_eq!(
            emit_composition_items(&limited, params(Some(9), false, false)),
            vec!["aaaa"]
        );
    }

    #[test]
    fn hint_before_a_barrier_is_dropped_instead_of_re_targeting() {
        // The bubble the hint comments on emits nothing, so the hint must not silently rebind to
        // the next surviving replica further down.
        let items = [hint("(note)"), barrier(), replica("b", "B")];
        assert_eq!(
            emit_composition_items(&items, params(None, false, false)),
            vec!["b"]
        );
        // Control: without the barrier the very same hint binds forward and is emitted.
        let without_barrier = [hint("(note)"), replica("b", "B")];
        assert_eq!(
            emit_composition_items(&without_barrier, params(None, false, false)),
            vec!["(note)", "b"]
        );
    }

    #[test]
    fn barrier_is_transparent_to_everything_except_hint_lookahead() {
        // A barrier emits nothing and must not break an open merged-character group.
        let items = [
            replica("a1", "A"),
            barrier(),
            replica("a2", "A"),
            barrier(),
        ];
        assert_eq!(
            emit_composition_items(&items, params(None, true, true)),
            vec!["a1\na2 - A"]
        );
    }

    #[test]
    fn a_leading_hint_never_starves_the_replica_it_annotates() {
        // The "first entry is always admitted" privilege belongs to the whole atomic bundle, not
        // to its first element: taking it for the hint alone would delete the replica.
        let items = [hint("H"), replica("R", "A")];
        assert_eq!(
            emit_composition_items(&items, params(Some(1), false, false)),
            vec!["H", "R"]
        );

        let long_hint = "h".repeat(60);
        let long_replica = "r".repeat(50);
        let items = [hint(&long_hint), replica(&long_replica, "A")];
        assert_eq!(
            emit_composition_items(&items, params(Some(100), false, false)),
            vec![long_hint.clone(), long_replica.clone()]
        );

        // Control: with no hint at all the replica was always admitted, so adding the hint must
        // not be able to turn a non-empty composition into an empty one.
        let control = [replica(&long_replica, "A")];
        assert_eq!(
            emit_composition_items(&control, params(Some(100), false, false)),
            vec![long_replica]
        );
    }

    #[test]
    fn trailing_hint_survives_a_limit_break_at_a_non_replica_item() {
        // No ordinary replica follows the hint anywhere, so it binds backward. The limit stops the
        // loop on the image entry, which must not take the trailing hint down with it.
        let items = [replica("aaaa", "A"), hint("(note)"), image(&["iiiiiiiiii"])];
        assert_eq!(
            emit_composition_items(&items, params(Some(10), false, false)),
            vec!["aaaa", "(note)"]
        );
    }

    #[test]
    fn forward_bound_hints_die_with_the_loop_when_an_image_hits_the_limit() {
        // Here a replica DOES follow, so the hint is forward-bound and queued; the limit stops the
        // loop on the image entry before the target is reached, and the queue is dropped.
        let items = [
            replica("aaaa", "A"),
            hint("(h)"),
            image(&["iiiiiiiiii"]),
            replica("bbbb", "B"),
        ];
        assert_eq!(
            emit_composition_items(&items, params(Some(10), false, false)),
            vec!["aaaa"]
        );
    }

    #[test]
    fn merged_group_hints_and_target_are_admitted_at_the_exact_boundary() {
        // "x - X" = 5, "a1 - A" = 6, "(h)" = 3, "b1" = 2, separator "\n\n" = 2.
        // The bundle simulation walks 5 -> 13 -> 18 -> 22, so 22 is the exact admission boundary.
        let items = [
            replica("x", "X"),
            replica("a1", "A"),
            hint("(h)"),
            replica("b1", "B"),
        ];
        assert_eq!(
            emit_composition_items(&items, params(Some(22), true, true)),
            vec!["x - X", "a1 - A", "(h)", "b1 - B"]
        );
        // One character less rejects the bundle: the hint and its target are both dropped, while
        // the group that was already open is still force-flushed at the tail.
        assert_eq!(
            emit_composition_items(&items, params(Some(21), true, true)),
            vec!["x - X", "a1 - A"]
        );
    }

    #[test]
    fn separator_accounting_covers_every_queued_hint() {
        // Custom 3-char separator: 4 -> 11 -> 18 -> 25 over ["(h1)", "(h2)", "bbbb"].
        let items = [
            replica("aaaa", "A"),
            hint("(h1)"),
            hint("(h2)"),
            replica("bbbb", "B"),
        ];
        assert_eq!(
            emit_composition_items(&items, params_with_sep(" | ", Some(25), false, false)),
            vec!["aaaa", "(h1)", "(h2)", "bbbb"]
        );
        assert_eq!(
            emit_composition_items(&items, params_with_sep(" | ", Some(24), false, false)),
            vec!["aaaa"]
        );
    }

    /// Minimal `ProjectData` for the `compose_plain` end-to-end tests. Every path points at the
    /// same non-existent directory: composition never touches the filesystem.
    fn test_project(bubbles: Vec<Bubble>) -> ProjectData {
        let dir = PathBuf::from("composition-test-project");
        ProjectData {
            project_dir: dir.clone(),
            image_dir: dir.clone(),
            pages: Vec::new(),
            bubbles: Arc::new(bubbles),
            paths: ProjectPaths {
                project_dir: dir.clone(),
                title_dir: dir.clone(),
                notes_file: dir.clone(),
                char_favorites_file: dir.clone(),
                color_presets_file: dir.clone(),
                bubbles_file: dir.clone(),
                src_dir: dir.clone(),
                clean_layers_dir: dir.clone(),
                cleaned_dir: dir.clone(),
                alt_vers_dir: dir.clone(),
                saved_dir: dir.clone(),
                image_bubbles_dir: dir.clone(),
                text_images_dir: dir.clone(),
                layers_dir: dir.clone(),
                text_detection_dir: dir.clone(),
                characters_dir: dir.clone(),
                terms_file: dir.clone(),
                settings_file: dir.clone(),
                unsaved_dir: dir.clone(),
                unsaved_bubbles_file: dir.clone(),
                unsaved_clean_layers_dir: dir.clone(),
                unsaved_image_bubbles_dir: dir.clone(),
                unsaved_text_images_dir: dir.clone(),
                unsaved_layers_dir: dir,
            },
            comic_type: None,
            canvas_settings: CanvasSettings::default(),
            settings_data: Value::Null,
            session: Default::default(),
        }
    }

    fn text_bubble(id: i64, img_v: f32, original: &str, translation: &str) -> Bubble {
        Bubble {
            id,
            img_idx: 0,
            img_u: 0.5,
            img_v,
            side: Some("right".to_string()),
            bubble_class: Some("text".to_string()),
            bubble_type: Some("aside".to_string()),
            text: translation.to_string(),
            original_text: original.to_string(),
            extra: Map::new(),
        }
    }

    fn hint_bubble(id: i64, img_v: f32, line: &str) -> Bubble {
        Bubble {
            id,
            img_idx: 0,
            img_u: 0.5,
            img_v,
            side: Some("right".to_string()),
            bubble_class: Some("hint".to_string()),
            bubble_type: Some("aside".to_string()),
            text: line.to_string(),
            original_text: String::new(),
            extra: Map::new(),
        }
    }

    /// Plain-path options with every flag that would obscure the hint attachment turned off.
    fn plain_options() -> CompositionPanelOptions {
        CompositionPanelOptions {
            source_mode: CompositionSourceMode::Original,
            ignore_translated_lines: true,
            merge_same_character: false,
            use_character_names: false,
            wrap_with_enabled: false,
            limit_enabled: false,
            include_hint_bubbles: true,
            ..CompositionPanelOptions::default()
        }
    }

    #[test]
    fn compose_plain_drops_a_hint_whose_replica_is_already_translated() {
        // Reading order: hint, an already-translated replica (dropped by `ignore_translated_lines`)
        // and an untranslated one. The filtered replica must stay in the stream as a barrier so
        // the hint does not re-target the replica below it.
        let project = test_project(vec![
            hint_bubble(1, 0.1, "note"),
            text_bubble(2, 0.2, "source A", "translated A"),
            text_bubble(3, 0.3, "source B", ""),
        ]);
        assert_eq!(compose_plain(&project, &plain_options()), "source B");

        // Control: with the filter off, replica A survives and the hint binds to it.
        let mut options = plain_options();
        options.ignore_translated_lines = false;
        assert_eq!(
            compose_plain(&project, &options),
            "(note)\n\nsource A\n\nsource B"
        );
    }

    #[test]
    fn compose_plain_treats_a_source_less_replica_as_a_barrier() {
        // Same rule for the other pass-1 rejection: no source text at all.
        let project = test_project(vec![
            hint_bubble(1, 0.1, "note"),
            text_bubble(2, 0.2, "", ""),
            text_bubble(3, 0.3, "source B", ""),
        ]);
        assert_eq!(compose_plain(&project, &plain_options()), "source B");
    }

    #[test]
    fn compose_plain_keeps_a_hint_across_an_excluded_image_bubble() {
        // An image bubble excluded by its own option leaves no barrier: a hint binds across image
        // bubbles whether or not they are included (`panels/MODULE_README.md`, "Hint attachment in
        // `composition.rs`").
        let mut image = image_bubble("area", Map::new());
        image.id = 2;
        image.img_v = 0.2;
        let project = test_project(vec![
            hint_bubble(1, 0.1, "note"),
            image,
            text_bubble(3, 0.3, "source B", ""),
        ]);
        let options = plain_options();
        assert!(!options.include_image_bubbles);
        assert_eq!(compose_plain(&project, &options), "(note)\n\nsource B");
    }

    #[test]
    fn compose_plain_reports_no_replicas_when_only_barriers_survive() {
        // A stream of nothing but barriers must still take the "no replicas" early return.
        let project = test_project(vec![
            text_bubble(1, 0.1, "source A", "translated A"),
            text_bubble(2, 0.2, "source B", "translated B"),
        ]);
        assert_eq!(compose_plain(&project, &plain_options()), super::no_items_text());
    }

    fn image_bubble(text: &str, extra: Map<String, Value>) -> Bubble {
        Bubble {
            id: 1,
            img_idx: 0,
            img_u: 0.5,
            img_v: 0.5,
            side: Some("right".to_string()),
            bubble_class: Some("image".to_string()),
            bubble_type: Some("aside".to_string()),
            text: text.to_string(),
            original_text: String::new(),
            extra,
        }
    }

    #[test]
    fn is_image_bubble_detects_class() {
        let mut text_bubble = image_bubble("t", Map::new());
        assert!(is_image_bubble(&text_bubble));
        text_bubble.bubble_class = Some("text".to_string());
        assert!(!is_image_bubble(&text_bubble));
        text_bubble.bubble_class = None;
        assert!(!is_image_bubble(&text_bubble));
    }

    #[test]
    fn is_hint_bubble_detects_class() {
        let mut bubble = image_bubble("t", Map::new());
        assert!(!is_hint_bubble(&bubble));
        bubble.bubble_class = Some("hint".to_string());
        assert!(is_hint_bubble(&bubble));
        assert!(!is_image_bubble(&bubble));
        bubble.bubble_class = None;
        assert!(!is_hint_bubble(&bubble));
    }

    #[test]
    fn area_translations_use_legacy_for_area0_and_array_for_rest() {
        let mut extra = Map::new();
        extra.insert(
            "description".to_string(),
            Value::String("desc0".to_string()),
        );
        extra.insert(
            "text_areas".to_string(),
            json!([
                {"rect": [0.0, 0.0, 0.4, 0.4], "anchor": [0.2, 0.2]},
                {"rect": [0.5, 0.5, 0.9, 0.9], "anchor": [0.7, 0.7],
                 "translation": "tr1", "description": "desc1"},
                {"rect": [0.5, 0.1, 0.9, 0.3], "anchor": [0.7, 0.2],
                 "translation": "   ", "description": "empty-skipped"}
            ]),
        );
        let areas = image_bubble_area_translations(&image_bubble("tr0", extra));
        // Area 0 from legacy text + extra.description; area 1 from the array; the blank area skipped.
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[0], ("tr0".to_string(), "desc0".to_string()));
        assert_eq!(areas[1], ("tr1".to_string(), "desc1".to_string()));
    }

    #[test]
    fn area_translations_fall_back_to_legacy_single_area() {
        let mut extra = Map::new();
        extra.insert("description".to_string(), Value::String("only".to_string()));
        let areas = image_bubble_area_translations(&image_bubble("solo", extra));
        assert_eq!(areas, vec![("solo".to_string(), "only".to_string())]);
        // No translation at all -> no lines.
        let empty = image_bubble_area_translations(&image_bubble("  ", Map::new()));
        assert!(empty.is_empty());
    }

    /// The export writer writes the whole file to the target, creating missing parents, and
    /// reports the target's display name; DOCX output is a zip container.
    #[test]
    fn write_export_writes_txt_and_docx_into_the_target() {
        let dir = tempfile::tempdir().expect("temp dir");
        let txt_path = dir.path().join("nested").join("out.txt");
        let name = write_export(ExportFormat::Txt, SaveTarget::for_test(txt_path.clone()), "line 1\nline 2").expect("txt export");
        assert_eq!(name, "out.txt");
        assert_eq!(std::fs::read_to_string(&txt_path).expect("read txt"), "line 1\nline 2");

        let docx_path = dir.path().join("out.docx");
        let name = write_export(ExportFormat::Docx, SaveTarget::for_test(docx_path.clone()), "текст").expect("docx export");
        assert_eq!(name, "out.docx");
        let bytes = std::fs::read(&docx_path).expect("read docx");
        assert!(bytes.starts_with(b"PK\x03\x04"), "a docx is a zip archive");
    }

    /// A write that cannot happen is an error, never a silent success.
    #[test]
    fn write_export_reports_an_unwritable_target() {
        let dir = tempfile::tempdir().expect("temp dir");
        // The target's parent is a FILE, so neither the directory nor the file can be created.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("blocker file");
        let result = write_export(ExportFormat::Txt, SaveTarget::for_test(blocker.join("out.txt")), "x");
        assert!(result.is_err());
    }

    fn named_bubble(id: i64, img_v: f32, original: &str, character: &str) -> Bubble {
        let mut bubble = text_bubble(id, img_v, original, "");
        bubble.extra.insert("character_name".to_string(), Value::String(character.to_string()));
        bubble
    }

    /// Two replicas of Ann, then one of Bob, all untranslated.
    fn named_project() -> ProjectData {
        test_project(vec![named_bubble(1, 0.1, "a1", "Ann"), named_bubble(2, 0.2, "a2", "Ann"), named_bubble(3, 0.3, "b1", "Bob")])
    }

    /// The default options keep the historical name layout byte for byte (golden strings of
    /// the composer before the name options existed), merged and unmerged.
    #[test]
    fn default_name_options_reproduce_the_historical_output() {
        let project = named_project();
        let options = CompositionPanelOptions::default();
        assert_eq!(compose_plain(&project, &options), "`a1`\n`a2` - Ann\n\n`b1` - Bob");
        let unmerged = CompositionPanelOptions { merge_same_character: false, ..CompositionPanelOptions::default() };
        assert_eq!(compose_plain(&project, &unmerged), "`a1` - Ann\n\n`a2` - Ann\n\n`b1` - Bob");
    }

    /// `Before` puts `{prefix}{name}{suffix}` in front of the line (or the merged group) with no
    /// separator of its own; `After` appends it; escapes in both fields are decoded.
    #[test]
    fn name_position_prefix_and_suffix_shape_each_entry() {
        let project = named_project();
        let before = CompositionPanelOptions {
            character_name_position: CharacterNamePosition::Before,
            character_name_prefix: "[".to_string(),
            character_name_suffix: "]: ".to_string(),
            wrap_with_enabled: false,
            ..CompositionPanelOptions::default()
        };
        assert_eq!(compose_plain(&project, &before), "[Ann]: a1\na2\n\n[Bob]: b1");
        let before_unmerged = CompositionPanelOptions { merge_same_character: false, ..before.clone() };
        assert_eq!(compose_plain(&project, &before_unmerged), "[Ann]: a1\n\n[Ann]: a2\n\n[Bob]: b1");

        let after = CompositionPanelOptions {
            character_name_position: CharacterNamePosition::After,
            character_name_prefix: "\\n— ".to_string(),
            character_name_suffix: ".".to_string(),
            wrap_with_enabled: false,
            merge_same_character: false,
            ..CompositionPanelOptions::default()
        };
        assert_eq!(compose_plain(&project, &after), "a1\n— Ann.\n\na2\n— Ann.\n\nb1\n— Bob.");
    }

    /// With names off the name options change nothing.
    #[test]
    fn name_options_are_ignored_without_character_names() {
        let project = named_project();
        let options = CompositionPanelOptions {
            use_character_names: false,
            character_name_position: CharacterNamePosition::Before,
            character_name_prefix: "X".to_string(),
            character_name_suffix: "Y".to_string(),
            wrap_with_enabled: false,
            ..CompositionPanelOptions::default()
        };
        assert_eq!(compose_plain(&project, &options), "a1\n\na2\n\nb1");
    }

    /// A settings object or preset written before the name options existed reads as the
    /// defaults; an unknown position reads as `After`; whitespace-only affixes survive verbatim,
    /// so a reloaded preset never shows a false «unsaved».
    #[test]
    fn codec_name_options_missing_unknown_and_whitespace() {
        let mut obj = Map::new();
        write_composition_options_json(&mut obj, &CompositionPanelOptions::default());
        for key in ["character_name_position", "character_name_prefix", "character_name_suffix"] {
            assert!(obj.remove(key).is_some(), "{key} is written");
        }
        let mut read_back = CompositionPanelOptions::default();
        read_composition_options_json(&obj, &mut read_back);
        assert_eq!(read_back, CompositionPanelOptions::default());

        obj.insert("character_name_position".to_string(), json!("sideways"));
        let mut read_back = CompositionPanelOptions { character_name_position: CharacterNamePosition::Before, ..CompositionPanelOptions::default() };
        read_composition_options_json(&obj, &mut read_back);
        assert_eq!(read_back.character_name_position, CharacterNamePosition::After);

        let spaced = CompositionPanelOptions {
            character_name_position: CharacterNamePosition::Before,
            character_name_prefix: " ".to_string(),
            character_name_suffix: String::new(),
            ..CompositionPanelOptions::default()
        };
        let mut obj = Map::new();
        write_composition_options_json(&mut obj, &spaced);
        let mut read_back = CompositionPanelOptions::default();
        read_composition_options_json(&obj, &mut read_back);
        assert_eq!(read_back, spaced.stored_form());
        assert_eq!(read_back.stored_form(), spaced.stored_form(), "preset comparison form is unchanged by a reload");
    }

    /// `ms_config`'s project defaults tree reads back as `CompositionPanelOptions::default()`,
    /// so a title without a `composition` section and a fresh panel agree.
    #[test]
    fn config_defaults_tree_matches_the_option_defaults() {
        let defaults = ms_config::project_config_defaults();
        let obj = defaults.get("composition").and_then(Value::as_object).expect("composition defaults section");
        let mut read_back = CompositionPanelOptions { character_name_prefix: String::new(), ..CompositionPanelOptions::default() };
        read_composition_options_json(obj, &mut read_back);
        assert_eq!(read_back, CompositionPanelOptions::default().stored_form());
    }
}
