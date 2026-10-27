/*
File: cleaning/tools/ai_editor/engines/shared/prompt_cache_ui.rs

Purpose:
The drawing half of the shared prompt-cache controller: the preset NAME field with
«Кэшировать» / «Переименовать» (`draw_cache_row`), the one cache line under the prompt
(`draw_prompt_cache_line`), the «Библиотека промптов» toggle (`draw_prompt_library_toggle`)
and the library fold — the preset picker, «Экспорт» / «Импорт» and the outcome, error and
engine note lines (`draw_prompt_cache_library`).

Key structures:
- `PromptCacheBlockView`, `PromptCacheNote`, `PromptCacheUiOutput`

Key functions:
- `draw_cache_row()`, `draw_prompt_cache_line()`, `draw_prompt_library_toggle()`,
  `draw_prompt_cache_library()`

Notes:
Decides nothing: the button state comes from `prompt_cache_gates`, the line from
`prompt_cache_line` (both `prompt_cache.rs`), and a press is RETURNED as a
`PromptCacheAction` for the engine to start — never work started inside a widget. Every text
is a `cleaning.tools.prompt_library.*` key, the engine's already-localized encoder caption or
one of its note lines; the egui id salts are the engine's literals.
*/

use super::hf_download_ui::gated_button;
use super::prompt_cache::{CacheBlock, CacheButton, PromptCacheAction, PromptCacheController, PromptCacheGates, PromptCacheLine};
use super::prompt_library::{MAX_PRESET_NAME_CHARS, PromptLibraryEntry, apply_pick};
use super::prompt_library_ui::draw_prompt_library_picker;
use eframe::egui;

/// Width kept free for «Кэшировать» / «Переименовать» beside the preset-name field, in
/// points: the field takes the rest of the row (never less than 80).
const NAME_BUTTON_RESERVE: f32 = 140.0;

/// One engine-specific line drawn in the library fold after the outcome and warning lines
/// and before the skipped-files line (FLUX.2: its legacy-migration report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum PromptCacheNote {
    /// A small informational line.
    Small(String),
    /// A line in the warning colour.
    Warning(String),
}

/// Everything the cache row and the library fold read or mutate, borrowed for one frame.
pub(in crate::tools::ai_editor::engines) struct PromptCacheBlockView<'a> {
    /// The engine's library state (name field, listing, outcome lines).
    pub(in crate::tools::ai_editor::engines) ctl: &'a mut PromptCacheController,
    /// The prompt field; a pick writes the preset's text into it.
    pub(in crate::tools::ai_editor::engines) prompt: &'a mut String,
    /// The «Нет» stash (the last prompt no preset holds).
    pub(in crate::tools::ai_editor::engines) uncached: &'a str,
    /// This frame's gates (`prompt_cache_gates`).
    pub(in crate::tools::ai_editor::engines) gates: &'a PromptCacheGates,
    /// The engine's localized name of the encoder that made an entry.
    pub(in crate::tools::ai_editor::engines) caption: &'a dyn Fn(&PromptLibraryEntry) -> String,
    /// Stable id salt of the preset-name field (a literal, never localized).
    pub(in crate::tools::ai_editor::engines) name_id_salt: &'a str,
    /// Stable id salt of the preset picker (a literal, never localized).
    pub(in crate::tools::ai_editor::engines) library_id_salt: &'a str,
    /// The engine's own lines of the fold.
    pub(in crate::tools::ai_editor::engines) notes: &'a [PromptCacheNote],
}

/// What the library fold did this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) struct PromptCacheUiOutput {
    /// A pick wrote a different text into the prompt field: the engine treats it as a
    /// prompt edit (a save and a fresh `.status`).
    pub(in crate::tools::ai_editor::engines) prompt_changed: bool,
    /// «Экспорт» or «Импорт» was pressed.
    pub(in crate::tools::ai_editor::engines) action: Option<PromptCacheAction>,
}

/// Draws the preset NAME field and «Кэшировать» / «Переименовать» (`view.gates.build`), with
/// the reason on hover when it is closed — and, when it is closed because the library already
/// holds a better or the current encoder's entry, that reason as a line too, since it is not a
/// fault the user could fix but a fact about their library. Returns the press.
pub(in crate::tools::ai_editor::engines) fn draw_cache_row(ui: &mut egui::Ui, view: &mut PromptCacheBlockView<'_>) -> Option<PromptCacheAction> {
    let button = &view.gates.build;
    let (enabled, caption, tooltip) = match button {
        CacheButton::Cache => (true, t!("cleaning.tools.prompt_library.prompt_cache_build_button"), t!("cleaning.tools.prompt_library.prompt_library_save_tooltip")),
        CacheButton::Rename => (true, t!("cleaning.tools.prompt_library.prompt_cache_rename_button"), t!("cleaning.tools.prompt_library.prompt_cache_rename_tooltip")),
        CacheButton::Closed(_) => (false, t!("cleaning.tools.prompt_library.prompt_cache_build_button"), t!("cleaning.tools.prompt_library.prompt_library_save_tooltip")),
    };
    let (disabled_tooltip, library_note) = match button {
        CacheButton::Cache | CacheButton::Rename => (String::new(), None),
        CacheButton::Closed(CacheBlock::NotReady | CacheBlock::NothingToEncode) => {
            (t!("cleaning.tools.prompt_library.prompt_library_save_disabled_tooltip").to_string(), None)
        }
        CacheButton::Closed(CacheBlock::NoEncoder) => (t!("cleaning.tools.prompt_library.prompt_library_no_encoder_disabled_tooltip").to_string(), None),
        CacheButton::Closed(CacheBlock::MorePrecise(entry)) => {
            let note = tf!("cleaning.tools.prompt_library.prompt_cache_build_more_precise_status", format = (view.caption)(entry));
            (note.clone(), Some(note))
        }
        CacheButton::Closed(CacheBlock::AlreadyCached) => {
            let note = t!("cleaning.tools.prompt_library.prompt_cache_build_current_exists_status").to_string();
            (note.clone(), Some(note))
        }
        // The engine's own reason is already localized; it explains the grey button only.
        CacheButton::Closed(CacheBlock::Engine(reason)) => (reason.clone(), None),
    };
    let busy = view.ctl.busy();
    let mut pressed = false;
    ui.horizontal(|ui| {
        let field_width = (ui.available_width() - NAME_BUTTON_RESERVE).max(80.0);
        ui.add(
            egui::TextEdit::singleline(&mut view.ctl.name_input)
                .id_salt(view.name_id_salt)
                .hint_text(t!("cleaning.tools.prompt_library.prompt_library_preset_name_hint"))
                .char_limit(MAX_PRESET_NAME_CHARS)
                .desired_width(field_width),
        )
        .on_hover_text(t!("cleaning.tools.prompt_library.prompt_cache_name_tooltip"));
        pressed = gated_button(ui, enabled, caption, tooltip, &disabled_tooltip);
        if busy {
            ui.spinner();
            ui.ctx().request_repaint();
        }
    });
    if let Some(note) = library_note {
        ui.small(note);
    }
    let action = match button {
        CacheButton::Rename => PromptCacheAction::Rename,
        CacheButton::Cache | CacheButton::Closed(_) => PromptCacheAction::Build,
    };
    pressed.then_some(action)
}

/// Draws the ONE compact line under the cache row that says what the next run will cost:
/// whether the prompt is served from the cache — naming the encoder that made the entry in
/// use — or whether this machine has no encoder to make it with (`prompt_cache_line`
/// decides which). It stays OUTSIDE the collapsible library on purpose: a cached prompt is
/// what skips the encoder read, so this is everyday information. `Silent` draws nothing.
pub(in crate::tools::ai_editor::engines) fn draw_prompt_cache_line(ui: &mut egui::Ui, line: &PromptCacheLine) {
    match line {
        PromptCacheLine::Silent => {}
        PromptCacheLine::Cached { encoder, memory } => {
            let mut text = match encoder {
                Some(encoder) => tf!("cleaning.tools.prompt_library.prompt_cached_with_encoder_status", encoder = encoder),
                None => t!("cleaning.tools.prompt_library.prompt_cached_status").to_string(),
            };
            if *memory {
                text.push(' ');
                text.push_str(t!("cleaning.tools.prompt_library.prompt_cache_memory_suffix"));
            }
            ui.colored_label(ms_theme::status::SUCCESS, text);
        }
        PromptCacheLine::NotCached => {
            ui.colored_label(ms_theme::status::WARNING, t!("cleaning.tools.prompt_library.prompt_not_cached_status"));
        }
        // A WARNING, not an error: without an encoder the tool still generates from ready
        // caches, and only encoding a new prompt is closed.
        PromptCacheLine::NoEncoder => {
            ui.colored_label(ms_theme::status::WARNING, t!("cleaning.tools.prompt_library.text_encoder_missing_warning"));
        }
    }
}

/// The «Библиотека промптов» toggle (a stock `toggle_value`, so a caller may place it in a
/// wrapping row beside its own toggles). It writes the session flag directly.
pub(in crate::tools::ai_editor::engines) fn draw_prompt_library_toggle(ui: &mut egui::Ui, open: &mut bool) {
    ui.toggle_value(open, t!("cleaning.tools.prompt_library.prompt_library_label"))
        .on_hover_text(t!("cleaning.tools.prompt_library.prompt_library_toggle_hint"));
}

/// Draws the prompt LIBRARY fold: the preset picker (row «Нет» plus the model's named
/// presets; choosing one puts its text into the prompt field at once — the embeddings are read
/// only by the run), «Экспорт» / «Импорт», and the listing error, outcome, warning, the
/// engine's notes and the skipped-file line under them.
pub(in crate::tools::ai_editor::engines) fn draw_prompt_cache_library(ui: &mut egui::Ui, view: &mut PromptCacheBlockView<'_>) -> PromptCacheUiOutput {
    let mut output = PromptCacheUiOutput::default();
    let listing = view.ctl.library.as_ref();
    let pick = draw_prompt_library_picker(ui, view.library_id_salt, listing, view.prompt, view.uncached, view.caption);
    if let Some(pick) = pick
        && apply_pick(pick, listing, view.uncached, view.prompt)
    {
        output.prompt_changed = true;
    }

    ui.horizontal_wrapped(|ui| {
        for (enabled, caption, tooltip, disabled_tooltip, requested) in [
            (
                view.gates.export,
                t!("cleaning.tools.prompt_library.prompt_cache_export_button"),
                t!("cleaning.tools.prompt_library.prompt_library_export_tooltip"),
                t!("cleaning.tools.prompt_library.prompt_library_export_disabled_tooltip"),
                PromptCacheAction::Export,
            ),
            (
                view.gates.import,
                t!("cleaning.tools.prompt_library.prompt_cache_import_button"),
                t!("cleaning.tools.prompt_library.prompt_library_import_tooltip"),
                t!("cleaning.tools.prompt_library.prompt_cache_import_disabled_tooltip"),
                PromptCacheAction::Import,
            ),
        ] {
            if gated_button(ui, enabled, caption, tooltip, disabled_tooltip) {
                output.action = Some(requested);
            }
        }
        if view.ctl.list_busy() {
            ui.spinner();
            ui.ctx().request_repaint();
        }
    });
    if let Some(error) = view.ctl.list_error.as_deref() {
        ui.colored_label(ms_theme::status::ERROR, tf!("cleaning.tools.prompt_library.prompt_cache_list_error", err = error));
    }
    if let Some(status) = view.ctl.status.as_deref() {
        ui.small(status);
    }
    if let Some(warning) = view.ctl.warning.as_deref() {
        ui.colored_label(ms_theme::status::WARNING, warning);
    }
    for note in view.notes {
        match note {
            PromptCacheNote::Small(text) => {
                ui.small(text);
            }
            PromptCacheNote::Warning(text) => {
                ui.colored_label(ms_theme::status::WARNING, text);
            }
        }
    }
    if let Some(skipped) = listing.map(|listing| listing.skipped).filter(|skipped| *skipped > 0) {
        ui.small(tf!("cleaning.tools.prompt_library.prompt_cache_skipped_status", count = skipped));
    }
    output
}
