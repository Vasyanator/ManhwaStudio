/*
File: cleaning/tools/ai_editor/engines/flux3_klein/engine/actions.rs

Purpose:
The long operations of `Flux3KleinEngine` and the fold of the panel's intents into them. One
`start_*` hands work to a worker thread, one `poll_*` drains its channel; nothing in between
runs on the GUI thread.

Main responsibilities:
- fold one frame's `Flux3PanelIntents` (`apply_intents`): settings changes, the forecast
  widget's actions, the pickers, the reference list, the token, the download, the unload;
- the Hugging Face token (`poll_hf_token`), the access check and the download with its cancel
  (`start_download_check` .. `poll_download`);
- the file dialogs (`start_picker`, `poll_picker`) and the worker that reads picked
  reference files (`start_reference_read`, `poll_references`);
- `.unload` (`start_unload`, `poll_unload`).

Notes:
A second inherent `impl Flux3KleinEngine` block; the state lives in `mod.rs`. A download claims
the shared progress bar by GENERATION, like a run, so the two are mutually exclusive
(`pipeline_busy`).
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
            want_status,
            want_forecast,
            resources_actions,
            unload,
            picker,
            remove_reference,
            clear_references,
            hf_token_action,
            download_action,
        } = intents;
        // The widget wrote a picked step pair (and the encoder flag) into the settings already;
        // a suggestion and a format row are written here, the caps and the reset go to the
        // controller.
        let mut choice = self.settings.step_choice();
        let before = choice;
        let mut format = self.settings.format().wire().to_string();
        settings_changed |= self.resources.fold(resources_actions, &mut choice, Some(&mut format));
        if choice != before {
            settings_changed |= self.settings.set_step_choice(choice);
        }
        if let Some(picked) = Flux3Format::from_wire(&format)
            && picked != self.settings.format()
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
        }
        if want_status {
            self.status_wanted = true;
        }
        if want_forecast {
            // The refresh button: the staleness it exists for is caused outside the app.
            self.resources.invalidate();
        }
        if let Some(purpose) = picker {
            self.start_picker(purpose);
        }
        if let Some(action) = hf_token_action {
            self.hf_token.apply(action);
        }
        match download_action {
            Some(HfDownloadAction::Check) => self.start_download_check(),
            Some(HfDownloadAction::Start) => self.start_download(),
            Some(HfDownloadAction::Cancel) => self.cancel_download(),
            None => {}
        }
        if unload {
            self.start_unload();
        }
    }

    /// Drains the token field's secret-store worker; a changed token drops the held check (it
    /// was computed for the previous one).
    pub(super) fn poll_hf_token(&mut self) {
        if self.hf_token.poll(FLUX3_LOG_LABEL) {
            self.download_check = None;
            self.download_check_error = None;
        }
    }

    /// Starts the one-shot `.download.check` for the selected format. The token is the STORED
    /// one (the process-wide cache), never the field's unsaved text.
    pub(super) fn start_download_check(&mut self) {
        if self.download_check_rx.is_some() || self.pipeline_busy() {
            return;
        }
        let header = flux3_download_header(&ms_sysprobe::hf_token::hf_token(), self.settings.format());
        let (tx, rx) = mpsc::channel();
        self.download_check_rx = Some(rx);
        self.download_check_error = None;
        thread::spawn(move || {
            let _ = tx.send(check_flux3_download(header));
        });
    }

    /// Drains the access check; a refused repository is logged with the backend's message (the
    /// row shows the localized verdict), a missing plan with its reason.
    pub(super) fn poll_download_check(&mut self) {
        let Some(rx) = self.download_check_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(check)) => {
                self.download_check_rx = None;
                for repo in check.repos.iter().filter(|repo| repo.state != Some(HfDownloadState::Ok)) {
                    ms_log::runtime_log::log_warn(format!(
                        "[cleaning] {FLUX3_LOG_LABEL} download access check: repo {}, state {}, message: {}",
                        repo.repo, repo.state_wire, repo.message
                    ));
                }
                if check.plan.is_none() {
                    ms_log::runtime_log::log_warn(format!(
                        "[cleaning] {FLUX3_LOG_LABEL} download access check for format {} returned no plan: {}",
                        check.question, check.plan_error
                    ));
                }
                self.download_check = Some(check);
            }
            Ok(Err(err)) => {
                self.download_check_rx = None;
                self.download_check_error = Some(err);
            }
            Err(TryRecvError::Disconnected) => {
                self.download_check_rx = None;
                self.download_check_error = Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Starts the streaming `.download.start` of the selected format, claiming the progress bar
    /// on the GUI thread so a cancel can retire it.
    pub(super) fn start_download(&mut self) {
        if self.pipeline_busy() {
            return;
        }
        let header = flux3_download_header(&ms_sysprobe::hf_token::hf_token(), self.settings.format());
        let claim = begin_progress_generation(&self.progress);
        let progress = Arc::clone(&self.progress);
        let (tx, rx) = mpsc::channel();
        self.download_rx = Some(rx);
        self.download_status = Some(t!("cleaning.tools.flux3_klein.download_running_status").to_string());
        thread::spawn(move || {
            let _ = tx.send(run_flux3_download(header, &progress, &claim));
        });
    }

    /// Abandons the download: the answer is dropped, the bar retired, the backend told to stop
    /// (sticky: also when the request id is not known yet). The worker is kept in `abandoned`
    /// until it ends, so nothing new starts meanwhile. Staged bytes stay on disk and the next
    /// download resumes them.
    pub(super) fn cancel_download(&mut self) {
        let Some(rx) = self.download_rx.take() else {
            return;
        };
        self.abandoned.push(rx);
        retire_progress_generation(&self.progress, FLUX3_LOG_LABEL);
        self.download_status = Some(t!("cleaning.tools.flux3_klein.download_cancelled_status").to_string());
    }

    /// Drains the download. A finished one writes NOTHING into the settings (the download-mode
    /// root is derived); an answered root that differs from the derived one is logged as drift
    /// between the two halves. `.status` and the forecast are re-asked: the files changed.
    pub(super) fn poll_download(&mut self) {
        let Some(rx) = self.download_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.download_rx = None;
                let derived = config::flux3_klein_dir().to_string_lossy().to_string();
                if !outcome.root.is_empty() && outcome.root != derived {
                    ms_log::runtime_log::log_warn(format!(
                        "[cleaning] {FLUX3_LOG_LABEL} download placed the package at {}, but this build derives it from {derived}; the two halves disagree about the layout",
                        outcome.root
                    ));
                }
                self.status_wanted = true;
                self.resources.invalidate();
                self.download_check = None;
                self.download_status = Some(tf!(
                    "cleaning.tools.flux3_klein.download_done_status",
                    size = format_gib(outcome.downloaded_bytes),
                    skipped = outcome.skipped_files
                ));
            }
            Ok(Err(err)) => {
                self.download_rx = None;
                ms_log::runtime_log::log_warn(format!("[cleaning] {FLUX3_LOG_LABEL} model download failed: {err}"));
                self.download_status = Some(tf!("cleaning.tools.flux3_klein.download_error", err = err));
                // A partial download changed the files the status describes.
                self.status_wanted = true;
            }
            Err(TryRecvError::Disconnected) => {
                self.download_rx = None;
                self.download_status = Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
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
        };
        self.picker = Some((purpose, file_dialog::request(request, None)));
    }

    /// Folds a finished pick: the package folder into the settings, picked images into a read
    /// on a worker. A cancelled dialog is a no-op; a failure is shown where the pick belongs.
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
        assert_eq!(engine.settings.format(), Flux3Format::Bf16);
        assert!(engine.dirty);
        engine.resources.wanted = false;
        engine.apply_intents(Flux3PanelIntents { remove_reference: Some(0), ..Flux3PanelIntents::default() });
        assert_eq!(engine.references.items.len(), 1);
        assert_eq!(engine.references.items[0].name, "b");
        assert!(engine.resources.wanted);
        engine.apply_intents(Flux3PanelIntents { clear_references: true, ..Flux3PanelIntents::default() });
        assert!(engine.references.items.is_empty());
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
