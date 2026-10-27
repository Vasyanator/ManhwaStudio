/*
File: cleaning/tools/ai_editor/engines/flux3_klein/engine/actions.rs

Purpose:
The long operations of `Flux3KleinEngine` and the fold of the panel's intents into them. One
`start_*` hands work to a worker thread, one `poll_*` drains its channel; nothing in between
runs on the GUI thread.

Main responsibilities:
- fold one frame's `Flux3PanelIntents` (`apply_intents`): settings changes, the «Нет» stash,
  the forecast widget's actions, the pickers, the reference list, the token, the catalog
  block's click, the unload, the prompt-library press;
- the Hugging Face token (`poll_hf_token`; the catalog download reads the stored token);
- the file dialogs (`start_picker`, `poll_picker`) and the worker that reads picked
  reference files (`start_reference_read`, `poll_references`);
- `.unload` (`start_unload`, `poll_unload`);
- drive the shared `PromptCacheController` (`prompt_cache`) with FLUX.3's header and methods:
  the build (`start_prompt_cache_build`), the export / import started from the dialogs, the
  drains (`poll_prompt_cache`, `poll_and_maybe_query_prompt_cache_list`) and folding what an
  answer invalidated back into `.status` and the forecast (`apply_prompt_cache_effects`).

Notes:
A second inherent `impl Flux3KleinEngine` block; the state lives in `mod.rs`. The catalog
download is the controller's (`mod.rs`, `poll_catalog_bundle` / `apply_catalog_action`); it
runs in the app and never claims the run's progress bar.
*/

use super::*;
use ms_platform::file_dialog::{self, DialogKind, FileDialogRequest};

/// The image formats a reference may be picked from (decoded by the `image` crate).
const FLUX3_REFERENCE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff"];

impl Flux3KleinEngine {
    /// Folds the intents the panel raised this frame, after it was drawn.
    pub(super) fn apply_intents(&mut self, intents: Flux3PanelIntents) {
        let Flux3PanelIntents {
            mut settings_changed,
            postprocess_changed,
            uncached_changed,
            want_status,
            want_forecast,
            resources_actions,
            unload,
            picker,
            remove_reference,
            clear_references,
            hf_token_action,
            catalog_action,
            prompt_cache_action,
        } = intents;
        // The widget wrote a picked step pair (and the encoder flag) into the settings already;
        // a suggestion and a format row are written here, the caps and the reset go to the
        // controller.
        let mut choice = self.settings.step_choice();
        let before = choice;
        let mut format = self.settings.dit_format().wire().to_string();
        settings_changed |= self.resources.fold(resources_actions, &mut choice, Some(&mut format));
        if choice != before {
            settings_changed |= self.settings.set_step_choice(choice);
        }
        if let Some(picked) = Flux3DitFormat::from_wire(&format)
            && picked != self.settings.dit_format()
        {
            self.settings.format = picked.wire().to_string();
            settings_changed = true;
        }
        let references_changed = remove_reference.is_some() || clear_references;
        if let Some(index) = remove_reference {
            self.references.remove(index);
        }
        if clear_references {
            self.references.clear();
        }
        if references_changed {
            self.references_errors.clear();
            // The forecast counts the references.
            self.resources.arm();
        }
        if settings_changed {
            self.note_settings_changed();
        } else if postprocess_changed {
            self.note_postprocess_changed();
        } else if uncached_changed {
            // Only the «Нет» stash moved (it follows the typed text): a save, nothing else.
            self.dirty = true;
        }
        if want_status {
            self.status_wanted = true;
        }
        if want_forecast {
            // The refresh button: the staleness it exists for is caused outside the app — and
            // so are files that appeared on disk, so the catalog bundle is re-probed too.
            self.resources.invalidate();
            self.request_catalog_status();
        }
        if let Some(purpose) = picker {
            self.start_picker(purpose);
        }
        if let Some(action) = hf_token_action {
            self.hf_token.apply(action);
        }
        self.apply_catalog_action(catalog_action);
        if unload {
            self.start_unload();
        }
        match prompt_cache_action {
            Some(PromptCacheAction::Build) => self.start_prompt_cache_build(false),
            Some(PromptCacheAction::Rename) => self.start_prompt_cache_build(true),
            // The dialog is opened here and the entry's file is captured with it, so a prompt
            // changed while it is open cannot redirect the export to another entry.
            Some(PromptCacheAction::Export) => {
                if self.prompt_cache.arm_export(&self.settings.prompt) {
                    self.start_picker(Flux3PickerPurpose::PromptCacheExport);
                }
            }
            Some(PromptCacheAction::Import) => self.start_picker(Flux3PickerPurpose::PromptCacheImport),
            None => {}
        }
    }

    /// Drains the token field's secret-store worker (the shared `HfTokenControl`).
    ///
    /// Nothing else is reset here: the catalog controller notices a changed token by itself
    /// and re-checks the gated repository's access.
    pub(super) fn poll_hf_token(&mut self) {
        self.hf_token.poll(FLUX3_LOG_LABEL);
    }

    /// Opens a non-blocking file dialog for `purpose`; at most one at a time.
    pub(super) fn start_picker(&mut self, purpose: Flux3PickerPurpose) {
        if self.picker.is_some() {
            return;
        }
        let request = match purpose {
            Flux3PickerPurpose::ModelRoot => FileDialogRequest::new(DialogKind::PickDirectoryPath).title(t!("cleaning.tools.flux3_klein.model_root_dialog_title")),
            Flux3PickerPurpose::References => FileDialogRequest::new(DialogKind::OpenFiles)
                .title(t!("cleaning.tools.flux3_klein.references_dialog_title"))
                .filter(t!("cleaning.tools.flux3_klein.references_files_filter"), &FLUX3_REFERENCE_EXTENSIONS),
            Flux3PickerPurpose::PromptCacheExport => prompt_cache_export_dialog(),
            Flux3PickerPurpose::PromptCacheImport => prompt_cache_import_dialog(),
        };
        self.picker = Some((purpose, file_dialog::request(request, None)));
    }

    /// Folds a finished pick: the package folder into the settings, picked images into a read
    /// on a worker, a prompt-cache file into the export (with its save target) or the import. A
    /// cancelled dialog is a no-op; a failure is shown where the pick belongs.
    pub(super) fn poll_picker(&mut self) {
        let Some(result) = self.picker.as_mut().and_then(|(_, dialog)| dialog.poll()) else {
            return;
        };
        let Some((purpose, _)) = self.picker.take() else {
            return;
        };
        let picked = match result {
            Ok(Some(picked)) => picked,
            Ok(None) => return,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[cleaning] {FLUX3_LOG_LABEL} {purpose:?} file dialog failed: {err:?}"));
                match purpose {
                    Flux3PickerPurpose::ModelRoot => self.status_error = Some(err.user_message()),
                    Flux3PickerPurpose::References => self.references_errors = vec![err.user_message()],
                    Flux3PickerPurpose::PromptCacheExport | Flux3PickerPurpose::PromptCacheImport => self.prompt_cache.status = Some(err.user_message()),
                }
                return;
            }
        };
        let mismatch = || ms_log::runtime_log::log_error(format!("[cleaning] {FLUX3_LOG_LABEL} {purpose:?}: file dialog answered with the wrong result kind"));
        match purpose {
            Flux3PickerPurpose::ModelRoot => {
                let Some(path) = picked.into_directory_path() else {
                    mismatch();
                    return;
                };
                self.settings.model_root = path.to_string_lossy().to_string();
                self.note_settings_changed();
            }
            Flux3PickerPurpose::References => {
                let Some(files) = picked.into_files() else {
                    mismatch();
                    return;
                };
                self.start_reference_read(files.into_iter().map(|file| (file.path, file.display_name)).collect());
            }
            // These two carry a library entry in or out through a FILE: nothing is persisted.
            // The export keeps its save TARGET: the backend writes the file, then the target is
            // committed (`PromptCacheController::start_export`).
            Flux3PickerPurpose::PromptCacheExport => {
                let Some(target) = picked.into_save_target() else {
                    mismatch();
                    return;
                };
                let settings = self.settings.normalized();
                self.prompt_cache.start_export(|extra| flux3_prompt_cache_header(&settings, extra), target, &FLUX3_PROMPT_CACHE_METHODS);
            }
            Flux3PickerPurpose::PromptCacheImport => {
                let Some(file) = picked.into_file() else {
                    mismatch();
                    return;
                };
                let settings = self.settings.normalized();
                self.prompt_cache.start_import(|extra| flux3_prompt_cache_header(&settings, extra), file.path, &FLUX3_PROMPT_CACHE_METHODS);
            }
        }
    }

    /// Reads picked reference files on a worker. Only as many as there are free slots are read
    /// at all; the rest are counted as left out and reported.
    pub(super) fn start_reference_read(&mut self, files: Vec<(PathBuf, String)>) {
        if self.references_rx.is_some() || files.is_empty() {
            return;
        }
        self.references_errors.clear();
        let free = self.references.free_slots();
        let left_out = files.len().saturating_sub(free);
        let (tx, rx) = mpsc::channel();
        self.references_rx = Some(rx);
        thread::spawn(move || {
            let loads: Vec<Flux3ReferenceLoad> = files.iter().take(free).map(|(path, name)| read_reference_file(path, name)).collect();
            let _ = tx.send((loads, left_out));
        });
    }

    /// Drains the reference read into the list (capped) and the refusals into the panel.
    pub(super) fn poll_references(&mut self) {
        let Some(rx) = self.references_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((loads, left_out)) => {
                self.references_rx = None;
                self.references_errors = self.references.push_loaded(loads, left_out);
                self.resources.arm();
            }
            Err(TryRecvError::Disconnected) => {
                self.references_rx = None;
                self.references_errors = vec![t!("cleaning.mask_editor.processing_thread_crashed_error").to_string()];
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Starts the prompt-cache build (`renamed` = «Переименовать») through the shared
    /// controller: the preset name is the name field, the header the normalized settings, and
    /// it claims the shared progress bar exactly as a run does (its `stage` frames are named by
    /// `flux3_stage_label`).
    pub(super) fn start_prompt_cache_build(&mut self, renamed: bool) {
        let settings = self.settings.normalized();
        self.prompt_cache.start_build(|extra| flux3_prompt_cache_header(&settings, extra), renamed, &FLUX3_PROMPT_CACHE_METHODS, &self.progress);
    }

    /// Folds what a finished prompt-cache answer invalidated back into the engine: `.status`
    /// (whether the prompt is served) and the memory forecast (whether the encoder counts).
    pub(super) fn apply_prompt_cache_effects(&mut self, effects: PromptCacheEffects) {
        if effects.status_wanted {
            self.status_wanted = true;
        }
        if effects.forecast_invalidated {
            self.resources.invalidate();
        }
    }

    /// Polls the library listing and asks a new `.list` when one is wanted, the backend is up,
    /// nothing holds the pipeline and an absolute package root exists (the backend refuses
    /// relative candidate paths for the whole call, as for `.status`).
    pub(super) fn poll_and_maybe_query_prompt_cache_list(&mut self) {
        let gate_open = self.ai_backend_available && !self.pipeline_busy() && self.settings.has_usable_root();
        let settings = &self.settings;
        let effects = self.prompt_cache.poll_list(gate_open, |extra| flux3_prompt_cache_header(&settings.normalized(), extra), &FLUX3_PROMPT_CACHE_METHODS);
        self.apply_prompt_cache_effects(effects);
    }

    /// Drains the prompt-cache operation into the outcome line of the library fold.
    pub(super) fn poll_prompt_cache(&mut self) {
        let effects = self.prompt_cache.poll(FLUX3_LOG_LABEL);
        self.apply_prompt_cache_effects(effects);
    }

    /// Asks the backend to release the FLUX.3 weights, on a worker.
    pub(super) fn start_unload(&mut self) {
        if self.unload_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.unload_rx = Some(rx);
        self.unload_status = Some(t!("cleaning.tools.flux3_klein.unload_requested_status").to_string());
        thread::spawn(move || {
            let _ = tx.send(unload_flux3_klein());
        });
    }

    /// Drains `.unload`; the status and forecast are re-asked (residency changed).
    pub(super) fn poll_unload(&mut self) {
        let Some(rx) = self.unload_rx.as_ref() else {
            return;
        };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string()),
        };
        self.unload_rx = None;
        self.unload_status = Some(match outcome {
            Ok(true) => t!("cleaning.tools.flux3_klein.unload_done_status").to_string(),
            Ok(false) => t!("cleaning.tools.flux3_klein.unload_nothing_status").to_string(),
            Err(err) => tf!("cleaning.inpaint.unload_error", err = err),
        });
        self.status_wanted = true;
        self.resources.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A format row picked in the memory widget becomes the persisted format (a save is owed),
    /// and a removed reference re-arms the forecast.
    #[test]
    fn the_widget_format_choice_and_the_reference_list_are_folded() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.references.push_loaded(vec![Ok(test_reference("a", 1)), Ok(test_reference("b", 2))], 0);
        engine.resources.wanted = false;
        engine.apply_intents(Flux3PanelIntents { resources_actions: vec![ResourcesAction::ApplyFormat("bf16".to_string())], ..Flux3PanelIntents::default() });
        // A GGUF row of the widget is a format like any other.
        engine.apply_intents(Flux3PanelIntents { resources_actions: vec![ResourcesAction::ApplyFormat("q5_k_m".to_string())], ..Flux3PanelIntents::default() });
        assert_eq!(engine.settings.dit_format(), Flux3DitFormat::Q5KM);
        engine.apply_intents(Flux3PanelIntents { resources_actions: vec![ResourcesAction::ApplyFormat("bf16".to_string())], ..Flux3PanelIntents::default() });
        assert_eq!(engine.settings.dit_format(), Flux3DitFormat::Bf16);
        assert!(engine.dirty);
        engine.resources.wanted = false;
        engine.apply_intents(Flux3PanelIntents { remove_reference: Some(0), ..Flux3PanelIntents::default() });
        assert_eq!(engine.references.items.len(), 1);
        assert_eq!(engine.references.items[0].name, "b");
        assert!(engine.resources.wanted);
        engine.apply_intents(Flux3PanelIntents { clear_references: true, ..Flux3PanelIntents::default() });
        assert!(engine.references.items.is_empty());
    }

    /// A «Постобработка» edit owes a save but re-arms neither the forecast nor `.status`; a
    /// forecast parameter in the same frame still re-arms it.
    #[test]
    fn a_postprocess_only_change_does_not_re_arm_the_forecast() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.status_root = Some(engine.settings.effective_root());
        engine.status_wanted = false;
        engine.resources.wanted = false;
        engine.settings.postprocess.mask_feather_px += 1;
        engine.apply_intents(Flux3PanelIntents { postprocess_changed: true, ..Flux3PanelIntents::default() });
        assert!(engine.dirty, "a save is owed");
        assert!(!engine.resources.wanted, "the forecast is not re-asked");
        assert!(!engine.status_wanted, "nor the status");

        engine.dirty = false;
        engine.apply_intents(Flux3PanelIntents { settings_changed: true, postprocess_changed: true, ..Flux3PanelIntents::default() });
        assert!(engine.dirty && engine.resources.wanted, "any other change still re-arms it");
    }

    /// The encoder flag toggled in the widget is a settings change (a save is owed).
    #[test]
    fn the_encoder_flag_change_is_a_settings_change() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.settings.encoder_on_cpu = false;
        engine.apply_intents(Flux3PanelIntents { resources_actions: vec![ResourcesAction::EncoderOnCpuChanged], ..Flux3PanelIntents::default() });
        assert!(engine.dirty);
        assert!(!engine.settings.encoder_on_cpu);
    }
}
