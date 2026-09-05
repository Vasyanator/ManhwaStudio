/*
File: cleaning/tools/ai_editor/engines/flux2_klein/engine/actions.rs

Purpose:
The long operations of `Flux2KleinEngine` — everything the panel ARMS and the engine then
has to see through. One `start_*` hands the work to a worker thread, one `poll_*` drains
the channel it answers on, and nothing in between runs on the GUI thread.

Main responsibilities:
- the per-component load/unload actions (`start_component_action`, `poll_component_action`);
- the Hugging Face token, which travels in the request and never into the settings file
  (`start_store_hf_token`, `start_clear_hf_token`, `poll_hf_token`);
- the model download: the pre-flight check, the transfer, its cancellation, and the
  derivation it applies when it finishes (`start_download_check` .. `apply_download_outcome`);
- the prompt translation (`start_translate`, `poll_translate`) and the native file pickers
  (`start_picker`, `poll_picker`);
- the six `.prompt_cache.*` jobs, the header they share and the outcome they apply
  (`start_prompt_cache_*`, `poll_prompt_cache`, `apply_prompt_cache_outcome`).

Key functions:
- one `start_*` / `poll_*` pair per operation, all on `impl Flux2KleinEngine`

Notes:
A second inherent `impl Flux2KleinEngine` block; the state these methods move is declared
in `mod.rs` beside the struct. All four operations here share ONE progress bar and claim
it by GENERATION, so a write from a retired job is dropped — see `../progress.rs`. The
gate that means "wait for the current operation" is `pipeline_busy`, in `mod.rs`, and it
reads all four channels at once.
*/

use super::*;

impl Flux2KleinEngine {
    /// Starts one per-component action on a worker thread.
    ///
    /// Streaming and claiming the shared progress bar exactly as
    /// [`Self::start_prompt_cache_build`] does — loading the ~16 GB text encoder takes
    /// ~100 s — which is also what makes it mutually exclusive with a generation.
    ///
    /// Whether the action is POSSIBLE is not decided here: the service listed it, and
    /// re-deriving that rule on this side would duplicate a matrix that depends on the
    /// accelerate hooks and the memory guard. This side only refuses to start a second
    /// operation while one is in flight; a refusal for any other reason comes back from
    /// the backend as an error with an actionable message.
    pub(super) fn start_component_action(&mut self, id: Flux2ComponentId, action: Flux2ComponentAction) {
        if self.pipeline_busy() {
            return;
        }
        let header = flux2_component_action_header(&self.settings.normalized(), id, action);
        let generation = begin_progress_generation(&self.progress);
        let progress = Arc::clone(&self.progress);
        let (tx, rx) = mpsc::channel();
        self.component_action_rx = Some(rx);
        self.component_action_pending = Some((id, action));
        self.component_action_status = Some(tf!(
            "cleaning.tools.flux2_klein.component_action_running_status",
            name = id.label(),
            action = action.label()
        ));
        thread::spawn(move || {
            let _ = tx.send(run_flux2_component_action(header, &progress, generation));
        });
    }

    /// Drains the component-action channel into the residency block.
    ///
    /// A successful action answers with the snapshot AFTER it, which is written straight
    /// into the catalog so the rows stop describing the state the user has just changed.
    /// `.status` is re-armed as well: the action moved weights, so `device` and `loaded`
    /// are stale too, and only a full answer can refresh them.
    pub(super) fn poll_component_action(&mut self) {
        let Some(rx) = self.component_action_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(snapshot)) => {
                self.component_action_rx = None;
                self.component_action_pending = None;
                // Written into the catalog only when one exists; without an answer to
                // attach it to there is nothing to show it beside, and the re-armed
                // `.status` below brings back the whole thing anyway.
                if let Some(status) = self.status.as_mut() {
                    status.components = snapshot;
                }
                self.status_wanted = true;
                self.component_action_status =
                    Some(t!("cleaning.tools.flux2_klein.component_action_done_status").to_string());
            }
            Ok(Err(err)) => {
                self.component_action_rx = None;
                let pending = self.component_action_pending.take();
                // The user gets the localized sentence; the log gets the wire pair, which
                // is what makes a refusal ("that action is not offered", "the memory guard
                // says no") attributable to a component and an action.
                let (component, action) = pending.map_or(("?", "?"), |(id, action)| {
                    (id.wire(), action.wire())
                });
                crate::runtime_log::log_warn(format!(
                    "[cleaning] FLUX.2 klein component action failed. Component: {component}. Action: {action}. Error: {err}"
                ));
                self.component_action_status = Some(tf!(
                    "cleaning.tools.flux2_klein.component_action_error",
                    err = err
                ));
                // The action may have moved weights before it failed, so the rows on
                // screen are no longer trustworthy.
                self.status_wanted = true;
            }
            Err(TryRecvError::Disconnected) => {
                self.component_action_rx = None;
                self.component_action_pending = None;
                self.component_action_status =
                    Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                self.status_wanted = true;
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Writes the token field into the OS secret store, on a worker thread.
    ///
    /// The keyring is an OS round trip, so it never happens on the GUI thread
    /// (`machine_translation.rs` spawns for the AI API keys for the same reason). The
    /// buffer is cleared IMMEDIATELY, before the worker even starts: the value is already
    /// on its way to the store, and leaving it in a widget only keeps a secret on screen.
    pub(super) fn start_store_hf_token(&mut self) {
        if self.hf_token_rx.is_some() {
            return;
        }
        let token = std::mem::take(&mut self.hf_token_input);
        let (tx, rx) = mpsc::channel();
        self.hf_token_rx = Some(rx);
        thread::spawn(move || {
            let result = crate::hf_token::store_hf_token(&token).map(|()| {
                t!("cleaning.tools.flux2_klein.download.token_saved_status").to_string()
            });
            let _ = tx.send(result);
        });
    }

    /// Deletes the stored token, on a worker thread. See [`Self::start_store_hf_token`].
    pub(super) fn start_clear_hf_token(&mut self) {
        if self.hf_token_rx.is_some() {
            return;
        }
        self.hf_token_input.clear();
        let (tx, rx) = mpsc::channel();
        self.hf_token_rx = Some(rx);
        thread::spawn(move || {
            let result = crate::hf_token::clear_hf_token().map(|()| {
                t!("cleaning.tools.flux2_klein.download.token_deleted_status").to_string()
            });
            let _ = tx.send(result);
        });
    }

    /// Drains the secret-store channel into the token status line.
    ///
    /// A finished operation also drops the last access check: it was computed for the
    /// PREVIOUS token and would otherwise keep claiming access the new one may not have.
    pub(super) fn poll_hf_token(&mut self) {
        let Some(rx) = self.hf_token_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(status)) => {
                self.hf_token_rx = None;
                self.hf_token_status = Some(status);
                self.download_check = None;
                self.download_check_error = None;
            }
            Ok(Err(err)) => {
                self.hf_token_rx = None;
                // The error carries the keyring's message, which never contains the token.
                crate::runtime_log::log_warn(format!(
                    "[cleaning] FLUX.2 klein Hugging Face token operation failed: {err}"
                ));
                self.hf_token_status = Some(err);
            }
            Err(TryRecvError::Disconnected) => {
                self.hf_token_rx = None;
                self.hf_token_status =
                    Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Starts the one-shot `.download.check` on a worker thread.
    ///
    /// The token is read from the process-wide cache rather than from the field: the field
    /// holds what the user is TYPING, while the check must ask about what is actually
    /// stored — otherwise a typo that was never saved would report access the download
    /// then cannot use.
    pub(super) fn start_download_check(&mut self) {
        if self.download_check_rx.is_some() || self.pipeline_busy() {
            return;
        }
        let uncensored = self.settings.uncensored_encoder_active();
        let header = flux2_download_header(&crate::hf_token::hf_token(), uncensored, self.variant);
        let (tx, rx) = mpsc::channel();
        self.download_check_rx = Some(rx);
        self.download_check_error = None;
        thread::spawn(move || {
            // Stamped on the worker, from the value that actually travelled: the answer
            // describes THAT toggle, whatever the user flips while it is in flight.
            let answer = check_flux2_download(header).map(|mut check| {
                check.uncensored = uncensored;
                check
            });
            let _ = tx.send(answer);
        });
    }


    /// Drains the access-check channel.
    ///
    /// A `variant` ECHO that disagrees with this engine's checkpoint is REPORTED, not
    /// dropped: the answer prices a different download, and silently discarding it would
    /// leave «Скачать» dead with nothing on screen to explain it. This is never the other
    /// engine's in-flight answer — the channel is created per engine instance by
    /// [`Self::start_download_check`] and only that instance's worker ever writes to it —
    /// so the only thing a mismatch can mean is a backend that answered about a checkpoint
    /// nobody asked for, typically one too old to know the field at all.
    pub(super) fn poll_download_check(&mut self) {
        let Some(rx) = self.download_check_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(check)) => {
                self.download_check_rx = None;
                self.download_check_error = None;
                if let Some(message) = download_check_variant_mismatch(&check, self.variant) {
                    crate::runtime_log::log_warn(format!(
                        "[cleaning] FLUX.2 klein download access check answered about variant {}, but {} was requested; the answer is discarded",
                        check.variant.wire(),
                        self.variant.wire()
                    ));
                    // The previous answer, if any, described THIS variant and is still
                    // true, so it is left in place; only the failed refresh is reported.
                    self.download_check_error = Some(message);
                    return;
                }
                // The technical half goes to the log with the repository it belongs to:
                // the row on screen carries the localized state, and `network_error` has
                // no localizable content at all. The request's token is not in scope here
                // and must never enter this line.
                for repo in &check.repos {
                    if repo.state != Some(Flux2DownloadState::Ok) {
                        crate::runtime_log::log_warn(format!(
                            "[cleaning] FLUX.2 klein download access check: repo {}, state {}, message: {}",
                            repo.repo, repo.state_wire, repo.message
                        ));
                    }
                }
                // A missing plan is its own diagnosis and a different failure from a
                // refused repository: access succeeded and the LISTING did not.
                if check.plan.is_none() {
                    crate::runtime_log::log_warn(format!(
                        "[cleaning] FLUX.2 klein download access check returned no plan; the download size is unknown. Reason: {}",
                        check.plan_error
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
                self.download_check_error =
                    Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Starts the streaming `.download.start` on a worker thread.
    ///
    /// Claims the shared progress bar here, on the GUI thread, exactly as a generation and
    /// a `.prompt_cache.build` do: that is what lets a cancel retire it and what makes the
    /// four operations mutually exclusive.
    pub(super) fn start_download(&mut self) {
        if self.pipeline_busy() {
            return;
        }
        let header = flux2_download_header(
            &crate::hf_token::hf_token(),
            self.settings.uncensored_encoder_active(),
            self.variant,
        );
        let generation = begin_progress_generation(&self.progress);
        let progress = Arc::clone(&self.progress);
        let (tx, rx) = mpsc::channel();
        self.download_rx = Some(rx);
        self.download_status =
            Some(t!("cleaning.tools.flux2_klein.download.running_status").to_string());
        thread::spawn(move || {
            let _ = tx.send(run_flux2_download(header, &progress, generation));
        });
    }

    /// Abandons the download in flight: its answer is discarded, its progress generation is
    /// retired so it can neither move nor stop the bar of whatever runs next, and the
    /// backend is told to stop instead of pulling gigabytes nobody will keep.
    ///
    /// Files already published on disk stay — they are complete and were renamed
    /// atomically, so a re-run resumes at file granularity.
    pub(super) fn cancel_download(&mut self) {
        if self.download_rx.is_none() {
            return;
        }
        self.download_rx = None;
        if let Some(id) = retire_progress_generation(&self.progress) {
            spawn_flux2_cancel(id);
        }
        self.download_status =
            Some(t!("cleaning.tools.flux2_klein.download.cancelled_status").to_string());
    }

    /// Drains the download channel and CONFIGURES THE ENGINE from a finished download.
    ///
    /// The three answered paths are written into the settings and the file is marked dirty,
    /// so the user does not then hand-pick paths for files the tool has just placed. Only
    /// non-empty paths are written: a partial answer must not blank a path the user set by
    /// hand. `.status`, the forecast and the prompt-cache listing are all re-armed, because
    /// new model paths change every one of their answers.
    pub(super) fn poll_download(&mut self) {
        let Some(rx) = self.download_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.download_rx = None;
                self.apply_download_outcome(&outcome);
            }
            Ok(Err(err)) => {
                self.download_rx = None;
                crate::runtime_log::log_warn(format!(
                    "[cleaning] FLUX.2 klein model download failed: {err}"
                ));
                self.download_status =
                    Some(tf!("cleaning.tools.flux2_klein.download.error", err = err));
            }
            Err(TryRecvError::Disconnected) => {
                self.download_rx = None;
                self.download_status =
                    Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Writes a finished download's paths into the settings and re-arms every query whose
    /// answer the new files invalidate.
    pub(super) fn apply_download_outcome(&mut self, outcome: &Flux2DownloadOutcome) {
        // The answer's paths are NOT written into the settings. In download mode the three
        // effective paths are derived from the models directory and the encoder toggle
        // (`Flux2KleinSettings::effective_paths`), so the engine is already configured and
        // stays configured; the manual fields belong to the OTHER mode and hold a
        // configuration the user can switch back to. Writing here would silently destroy a
        // hand-built model tree, which is exactly what a download must not cost.
        //
        // They are still worth comparing: the backend put the files somewhere, and if that
        // is not where this side derives them from, the two halves have drifted and the
        // engine would look for a model that is not there. That is a bug report, not a
        // reason to adopt the backend's answer.
        let derived = self.settings.effective_paths();
        for (component, derived, answered) in [
            ("transformer", &derived.transformer, &outcome.transformer_path),
            ("text_encoder", &derived.text_encoder, &outcome.text_encoder_path),
            ("vae", &derived.vae, &outcome.vae_path),
        ] {
            if !answered.is_empty() && answered != derived {
                crate::runtime_log::log_warn(format!(
                    "[cleaning] FLUX.2 klein download placed {component} at {answered}, but this build derives it from {derived}. The two halves disagree about the model layout."
                ));
            }
        }
        // The files on disk changed, so what the backend reports about them did too.
        self.status_wanted = true;
        self.estimate_wanted = true;
        self.prompt_cache_list_wanted = true;
        // The plan is spent: the missing bytes it named are on disk now.
        self.download_check = None;
        self.download_status = Some(tf!(
            "cleaning.tools.flux2_klein.download.done_status",
            size = format_gib(outcome.downloaded_bytes),
            skipped = outcome.skipped_files
        ));
    }

    /// Drains the prompt-translation channel into the English prompt field.
    pub(super) fn poll_translate(&mut self) {
        let Some(rx) = self.translate_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(text)) => {
                self.translate_rx = None;
                self.settings.prompt = text;
                self.dirty = true;
                // A different prompt: whatever the catalog last said about the cache is
                // now about the previous one.
                self.status_wanted = true;
                self.translate_status =
                    Some(t!("cleaning.tools.flux2_klein.translate_done_status").to_string());
            }
            Ok(Err(err)) => {
                self.translate_rx = None;
                self.translate_status =
                    Some(tf!("cleaning.tools.flux2_klein.translate_error", err = err));
            }
            Err(TryRecvError::Disconnected) => self.translate_rx = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Starts one machine-translation request for the user-language prompt.
    ///
    /// The call BLOCKS on the network (DeepL additionally self-throttles), so it runs
    /// on its own worker and the GUI only polls the channel.
    pub(super) fn start_translate(&mut self) {
        if self.translate_rx.is_some() {
            return;
        }
        let source = self.settings.source_prompt.trim().to_string();
        if source.is_empty() {
            self.translate_status =
                Some(t!("cleaning.tools.flux2_klein.translate_empty_error").to_string());
            return;
        }
        let service = MtService::from_key(&self.settings.mt_service).unwrap_or(MtService::Google);
        let source_lang = normalize_source_lang(&self.settings.source_lang);
        let (tx, rx) = mpsc::channel();
        self.translate_rx = Some(rx);
        self.translate_status =
            Some(t!("cleaning.tools.flux2_klein.translate_running_status").to_string());
        thread::spawn(move || {
            let _ = tx.send(translate_prompt_to_english(service, &source_lang, source));
        });
    }

    /// Starts a native file dialog for `purpose`; at most one dialog at a time.
    pub(super) fn start_picker(&mut self, purpose: Flux2PickerPurpose) {
        if self.picker_rx.is_some() {
            return;
        }
        self.picker_rx = Some(spawn_flux2_picker(purpose));
        self.picker = Some(purpose);
    }

    /// Folds a finished file pick into the matching model-path field, or — for the two
    /// prompt-cache purposes — starts the IPC call the dialog was opened for.
    ///
    /// A cancelled dialog is a no-op in both cases: nothing is written and no call goes
    /// out.
    pub(super) fn poll_picker(&mut self) {
        let Some(rx) = self.picker_rx.as_ref() else {
            return;
        };
        let picked = match rx.try_recv() {
            Ok(picked) => picked,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        self.picker_rx = None;
        let Some(purpose) = self.picker.take() else {
            return;
        };
        let Some(path) = picked else {
            return;
        };
        let value = path.to_string_lossy().to_string();
        match purpose {
            Flux2PickerPurpose::TextEncoderDir => self.settings.text_encoder_path = value,
            Flux2PickerPurpose::TransformerFile | Flux2PickerPurpose::TransformerDir => {
                self.settings.transformer_path = value;
            }
            Flux2PickerPurpose::VaeFile | Flux2PickerPurpose::VaeDir => {
                self.settings.vae_path = value;
            }
            // These two carry a library entry in or out through a FILE: nothing is
            // persisted and the settings are left exactly as they were.
            Flux2PickerPurpose::PromptCacheExport => {
                self.start_prompt_cache_export(path);
                return;
            }
            Flux2PickerPurpose::PromptCacheImport => {
                self.start_prompt_cache_import(path);
                return;
            }
        }
        self.dirty = true;
        self.status_wanted = true;
        self.estimate_wanted = true;
        // The encoder family — and therefore the whole library listing — is decided by
        // the model paths, so a new path invalidates the list just as it does the catalog.
        self.prompt_cache_list_wanted = true;
    }

    /// Starts the prompt-cache build on a worker thread.
    ///
    /// Streaming, and it claims the shared progress bar exactly as a generation does —
    /// reading the ~16 GB Qwen3 encoder takes ~106 s and the user needs to see it move.
    /// Claiming the generation here, on the GUI thread, is what makes a cancel and a moved
    /// frame able to retire and stop it, just like a run.
    pub(super) fn start_prompt_cache_build(&mut self) {
        if self.prompt_cache_rx.is_some() {
            return;
        }
        let header = self.prompt_cache_header(&[]);
        let generation = begin_progress_generation(&self.progress);
        let progress = Arc::clone(&self.progress);
        let (tx, rx) = mpsc::channel();
        self.prompt_cache_rx = Some(rx);
        self.prompt_cache_status =
            Some(t!("cleaning.tools.flux2_klein.prompt_cache_building_status").to_string());
        // A warning belongs to the operation that produced it, not to the next one.
        self.prompt_cache_warning = None;
        thread::spawn(move || {
            let result = build_flux2_prompt_cache(header, &progress, generation);
            let _ = tx.send(result);
        });
    }

    /// Stores the current prompt's cache in the library under the name in the field.
    pub(super) fn start_prompt_cache_save(&mut self) {
        let name = self.prompt_cache_name_input.trim().to_string();
        if name.is_empty() {
            return;
        }
        let header = self.prompt_cache_header(&[("name", json!(name.clone()))]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_saving_status"),
            move || save_flux2_prompt_cache(header).map(|()| Flux2PromptCacheOutcome::Saved(name)),
        );
    }

    /// Loads the selected library entry into the backend's live cache.
    pub(super) fn start_prompt_cache_load(&mut self) {
        let Some(name) = self.prompt_cache_selected.clone() else {
            return;
        };
        let header = self.prompt_cache_header(&[("name", json!(name))]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_loading_status"),
            move || load_flux2_prompt_cache(header).map(Flux2PromptCacheOutcome::Loaded),
        );
    }

    /// Writes the entry the export dialog was opened for to `path`.
    pub(super) fn start_prompt_cache_export(&mut self, path: PathBuf) {
        let Some(name) = self.prompt_cache_export_name.take() else {
            return;
        };
        let header = self.prompt_cache_header(&[
            ("name", json!(name)),
            ("path", json!(path.to_string_lossy())),
        ]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_exporting_status"),
            move || {
                export_flux2_prompt_cache(header).map(|()| Flux2PromptCacheOutcome::Exported(path))
            },
        );
    }

    /// Takes the file at `path` into the library.
    ///
    /// No `name` is sent: the backend then names the entry after the file's own stem,
    /// which is what the user just picked and therefore recognises.
    pub(super) fn start_prompt_cache_import(&mut self, path: PathBuf) {
        let family = self.prompt_cache_family().to_string();
        let header = self.prompt_cache_header(&[("path", json!(path.to_string_lossy()))]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_importing_status"),
            move || import_flux2_prompt_cache(header, &family),
        );
    }

    /// Builds a prompt-cache request header: the normalized settings under `params`
    /// (which is what identifies the encoder family) plus the operation's own fields.
    pub(super) fn prompt_cache_header(&self, extra: &[(&str, Value)]) -> Value {
        flux2_prompt_cache_header(&self.settings.normalized(), extra)
    }

    /// The encoder family the library listing describes, empty when it is not known yet.
    pub(super) fn prompt_cache_family(&self) -> &str {
        self.prompt_cache_library
            .as_ref()
            .map_or("", |library| library.family.as_str())
    }

    /// Runs one prompt-cache `job` on a worker thread, showing `running` meanwhile.
    ///
    /// Refuses to start a second operation while one is in flight — the backend holds one
    /// pipeline and one library, and the buttons are gated on the same condition.
    pub(super) fn start_prompt_cache_job<F>(&mut self, running: &str, job: F)
    where
        F: FnOnce() -> Result<Flux2PromptCacheOutcome, String> + Send + 'static,
    {
        if self.prompt_cache_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.prompt_cache_rx = Some(rx);
        self.prompt_cache_status = Some(running.to_string());
        // A warning belongs to the operation that produced it, not to the next one.
        self.prompt_cache_warning = None;
        thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    /// Polls the library listing and arms a new `.list` query when one is wanted.
    ///
    /// Same one-shot arming as `.status`, and for the same reason: a failing query must
    /// not spawn a thread per frame.
    pub(super) fn poll_and_maybe_query_prompt_cache_list(&mut self) {
        if let Some(rx) = self.prompt_cache_list_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(library)) => {
                    self.prompt_cache_list_rx = None;
                    self.prompt_cache_list_error = None;
                    // A selection that the refreshed listing no longer contains is
                    // dropped rather than left pointing at a deleted entry.
                    if let Some(selected) = self.prompt_cache_selected.as_ref()
                        && !library.entries.iter().any(|entry| &entry.name == selected)
                    {
                        self.prompt_cache_selected = None;
                    }
                    self.prompt_cache_library = Some(library);
                }
                Ok(Err(err)) => {
                    self.prompt_cache_list_rx = None;
                    self.prompt_cache_list_error = Some(err);
                }
                Err(TryRecvError::Disconnected) => self.prompt_cache_list_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.prompt_cache_list_wanted
            && self.ai_backend_available
            && self.prompt_cache_list_rx.is_none()
            && !self.pipeline_busy()
        {
            self.prompt_cache_list_wanted = false;
            let header = self.prompt_cache_header(&[]);
            let (tx, rx) = mpsc::channel();
            self.prompt_cache_list_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(list_flux2_prompt_caches(header));
            });
        }
    }

    /// Drains the prompt-cache channel into the status line under the prompt field.
    ///
    /// A finished BUILD re-arms `.status`, which is what turns the line above the buttons
    /// green. A finished LOAD additionally writes the prompt the file was built from into
    /// the field, so the user sees exactly what they loaded, and persists it.
    pub(super) fn poll_prompt_cache(&mut self) {
        let Some(rx) = self.prompt_cache_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(outcome)) => {
                self.prompt_cache_rx = None;
                self.prompt_cache_status = Some(self.apply_prompt_cache_outcome(outcome));
            }
            Ok(Err(err)) => {
                self.prompt_cache_rx = None;
                self.prompt_cache_status =
                    Some(tf!("cleaning.tools.flux2_klein.prompt_cache_error", err = err));
            }
            Err(TryRecvError::Disconnected) => {
                self.prompt_cache_rx = None;
                self.prompt_cache_status =
                    Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
            }
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Applies one finished prompt-cache operation and returns the line to show for it.
    ///
    /// Save and import both change the library, so both re-arm `.list`: the combo must
    /// show the new entry without the user reopening the panel. A load whose
    /// `encoder_verified` came back `false` additionally raises the one-off notice that the
    /// entry's encoder identity was taken on trust.
    pub(super) fn apply_prompt_cache_outcome(&mut self, outcome: Flux2PromptCacheOutcome) -> String {
        match outcome {
            Flux2PromptCacheOutcome::Built => {
                // The live cache changed, so the catalog's `prompt_cached` is stale.
                self.status_wanted = true;
                t!("cleaning.tools.flux2_klein.prompt_cache_built_status").to_string()
            }
            Flux2PromptCacheOutcome::Saved(name) => {
                self.prompt_cache_list_wanted = true;
                // The entry the user has just created is the one they will act on next.
                self.prompt_cache_selected = Some(name.clone());
                tf!(
                    "cleaning.tools.flux2_klein.prompt_cache_saved_status",
                    name = name
                )
            }
            // An entry that carries no prompt is refused rather than half-applied: an
            // empty prompt would block the run gate, and silently clearing the field would
            // look like the tool had lost the user's text.
            Flux2PromptCacheOutcome::Loaded(loaded) if loaded.prompt.is_empty() => {
                t!("cleaning.tools.flux2_klein.prompt_cache_load_empty_error").to_string()
            }
            Flux2PromptCacheOutcome::Loaded(loaded) => {
                self.settings.prompt = loaded.prompt;
                self.dirty = true;
                self.status_wanted = true;
                self.estimate_wanted = true;
                // The load SUCCEEDED and everything checkable was checked; what could not
                // be checked is the encoder's own fingerprint, because there is no local
                // encoder to compare it against. That is a one-off remark about this
                // operation — it rides the same warning slot as the foreign-family notice
                // and is cleared by the next operation — not a standing alarm.
                if loaded.encoder_verified == Some(false) {
                    self.prompt_cache_warning = Some(
                        t!("cleaning.tools.flux2_klein.prompt_cache_unverified_encoder_warning")
                            .to_string(),
                    );
                }
                t!("cleaning.tools.flux2_klein.prompt_cache_loaded_status").to_string()
            }
            Flux2PromptCacheOutcome::Exported(path) => tf!(
                "cleaning.tools.flux2_klein.prompt_cache_exported_status",
                path = path.display()
            ),
            Flux2PromptCacheOutcome::Imported {
                name,
                family_matches,
            } => {
                self.prompt_cache_list_wanted = true;
                // A foreign entry was still imported — into ITS family's folder, so it is
                // not lost — but it will not appear in this family's list and the backend
                // will refuse to load it. Saying so is the whole point: the alternative is
                // a successful import the user then cannot find anywhere.
                if family_matches == Some(false) {
                    self.prompt_cache_warning = Some(
                        t!("cleaning.tools.flux2_klein.prompt_cache_import_foreign_warning")
                            .to_string(),
                    );
                } else if !name.is_empty() {
                    self.prompt_cache_selected = Some(name.clone());
                }
                tf!(
                    "cleaning.tools.flux2_klein.prompt_cache_imported_status",
                    name = name
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A check answer about ANOTHER checkpoint is reported, never dropped in silence.
    ///
    /// The staleness filter alone would hide it and leave «Скачать» dead with nothing on
    /// screen — the exact failure the panel cannot recover from. This channel is created per
    /// engine instance, so a mismatch is never the other engine's in-flight answer; it is a
    /// backend that answered about a checkpoint nobody asked for.
    #[test]
    fn a_check_answer_about_another_checkpoint_is_reported_and_leaves_the_old_one_alone() {
        let previous = Flux2DownloadCheck {
            variant: Flux2Variant::Klein9B,
            plan_error: "the previous, correct answer".to_string(),
            ..Flux2DownloadCheck::default()
        };
        let (tx, rx) = mpsc::channel();
        let mut engine = Flux2KleinEngine {
            download_check: Some(previous.clone()),
            download_check_rx: Some(rx),
            ..Flux2KleinEngine::default()
        };
        tx.send(Ok(Flux2DownloadCheck {
            variant: Flux2Variant::Klein4B,
            ..Flux2DownloadCheck::default()
        }))
        .expect("the receiver is alive");
        engine.poll_download_check();

        assert!(engine.download_check_rx.is_none(), "the one-shot query is over either way");
        assert_eq!(
            engine.download_check.as_ref(),
            Some(&previous),
            "the previous answer described THIS checkpoint and is still true"
        );
        let error = engine
            .download_check_error
            .as_deref()
            .expect("the disagreement must reach the user");
        assert!(error.contains("4b") && error.contains("9b"), "{error}");
    }

    /// The agreeing case, so the guard above cannot silently reject everything.
    #[test]
    fn a_check_answer_about_this_checkpoint_is_stored() {
        let answer = Flux2DownloadCheck {
            variant: Flux2Variant::Klein9B,
            ..Flux2DownloadCheck::default()
        };
        let (tx, rx) = mpsc::channel();
        let mut engine = Flux2KleinEngine {
            download_check_rx: Some(rx),
            ..Flux2KleinEngine::default()
        };
        tx.send(Ok(answer.clone())).expect("the receiver is alive");
        engine.poll_download_check();
        assert_eq!(engine.download_check.as_ref(), Some(&answer));
        assert!(engine.download_check_error.is_none());
    }

    /// The token is a CREDENTIAL: it travels as a per-call request field and reaches no
    /// settings file, no `params` and no persisted document at all.
    #[test]
    fn the_token_travels_in_the_request_and_never_into_the_settings() {
        const SECRET: &str = "hf_a_token_that_must_never_be_persisted";

        let header = flux2_download_header(SECRET, true, Flux2Variant::Klein9B);
        assert_eq!(header["hf_token"], json!(SECRET));
        assert_eq!(header["uncensored"], json!(true));
        assert!(
            header.get("params").is_none(),
            "the token must not travel inside `params`"
        );

        // A fully configured engine, serialized exactly as `save_flux2_settings` writes it.
        let engine = Flux2KleinEngine {
            settings: Flux2KleinSettings {
                uncensored_text_encoder: true,
                ..runnable_settings()
            },
            hf_token_input: SECRET.to_string(),
            ..Flux2KleinEngine::default()
        };
        let document =
            serde_json::to_string(&engine.settings).expect("the settings document serializes");
        assert!(
            !document.contains(SECRET),
            "the settings document must not contain the token"
        );
        assert!(
            !engine
                .settings
                .normalized()
                .to_params(false)
                .to_string()
                .contains(SECRET),
            "no request `params` may contain the token"
        );
    }

    /// What a finished download does NOW: it configures the engine by DERIVATION, not by
    /// writing paths into the settings.
    ///
    /// This replaces the original contract's "a finished download writes its three paths
    /// into the settings". That rule was safe while the paths block and the download block
    /// were both always visible; with the two modes it became destructive, because the
    /// manual fields hold a configuration the user switches back to. Derivation configures
    /// the engine continuously instead of once, so the write-back is gone — and the
    /// answered paths are kept only to detect DRIFT between the two halves.
    #[test]
    fn a_finished_download_configures_the_engine_by_derivation() {
        let outcome = parse_flux2_download_outcome(&json!({
            "paths": {
                "transformer": " /models/FLUX.2-klein-9B/transformer ",
                "text_encoder": "/models/FLUX.2-klein-9B/text_encoder_uncensored",
                "vae": "/models/FLUX.2-klein-9B/vae"
            },
            "downloaded_bytes": 40_000_000_000u64,
            "skipped_files": 4
        }));
        assert_eq!(outcome.transformer_path, "/models/FLUX.2-klein-9B/transformer");
        assert_eq!(outcome.downloaded_bytes, 40_000_000_000);
        assert_eq!(outcome.skipped_files, 4);

        let mut engine = Flux2KleinEngine {
            settings: Flux2KleinSettings {
                source_mode: Flux2SourceMode::Download.wire().to_string(),
                uncensored_text_encoder: true,
                ..Flux2KleinSettings::default()
            },
            settings_loaded: true,
            dirty: false,
            ..Flux2KleinEngine::default()
        };
        let before = engine.settings.clone();
        engine.apply_download_outcome(&outcome);

        // No field of the settings was touched at all — not even a path that happened to
        // be empty, which the old write-back would have filled in.
        assert_eq!(
            serde_json::to_value(&engine.settings).expect("serialize"),
            serde_json::to_value(&before).expect("serialize"),
            "a finished download must not write into the settings"
        );
        assert!(!engine.dirty);

        // The engine is nevertheless configured: the derived paths point at the files the
        // download just placed, and they follow the encoder toggle.
        let paths = engine.settings.effective_paths();
        assert_eq!(
            paths.text_encoder,
            config::flux2_klein_text_encoder_dir(Flux2Variant::Klein9B, true).to_string_lossy()
        );
        assert_eq!(
            paths.transformer,
            config::flux2_klein_transformer_dir(Flux2Variant::Klein9B).to_string_lossy()
        );
        assert_eq!(paths.vae, config::flux2_klein_vae_dir(Flux2Variant::Klein9B).to_string_lossy());

        // Everything the new files invalidate is re-armed, and the spent plan is dropped.
        assert!(engine.status_wanted && engine.estimate_wanted);
        assert!(engine.prompt_cache_list_wanted);
        assert!(engine.download_check.is_none());

        // A partial answer is not a reason to write anything either.
        let partial = parse_flux2_download_outcome(&json!({
            "paths": { "transformer": "/models/new-transformer" }
        }));
        engine.apply_download_outcome(&partial);
        assert_eq!(
            serde_json::to_value(&engine.settings).expect("serialize"),
            serde_json::to_value(&before).expect("serialize")
        );
    }

    /// A finished download must not touch the manual fields. This is the destructive case
    /// the derivation exists to prevent: a hand-built tree the user switches back to.
    #[test]
    fn a_finished_download_leaves_the_manual_paths_untouched() {
        let outcome = parse_flux2_download_outcome(&json!({
            "paths": {
                "transformer": "/models/FLUX.2-klein-9B/transformer",
                "text_encoder": "/models/FLUX.2-klein-9B/text_encoder_uncensored",
                "vae": "/models/FLUX.2-klein-9B/vae"
            },
            "downloaded_bytes": 40_000_000_000u64,
            "skipped_files": 4
        }));
        let hand_made = Flux2KleinSettings {
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            text_encoder_path: "/home/u/sd_models/qwen3-uncensored".to_string(),
            transformer_path: "/home/u/sd_models/flux2.safetensors".to_string(),
            vae_path: "/home/u/sd_models/vae".to_string(),
            ..runnable_settings()
        };
        let mut engine = Flux2KleinEngine {
            settings: hand_made.clone(),
            settings_loaded: true,
            dirty: false,
            ..Flux2KleinEngine::default()
        };
        engine.apply_download_outcome(&outcome);

        assert_eq!(
            engine.settings.text_encoder_path, hand_made.text_encoder_path,
            "a download must never overwrite a hand-built encoder path"
        );
        assert_eq!(engine.settings.transformer_path, hand_made.transformer_path);
        assert_eq!(engine.settings.vae_path, hand_made.vae_path);
        assert!(!engine.dirty, "nothing was written, so nothing needs saving");

        // Switching back to the manual mode restores exactly the user's configuration.
        engine.settings.source_mode = Flux2SourceMode::Manual.wire().to_string();
        assert_eq!(
            engine.settings.effective_paths(),
            Flux2EffectivePaths {
                text_encoder: "/home/u/sd_models/qwen3-uncensored".to_string(),
                transformer: "/home/u/sd_models/flux2.safetensors".to_string(),
                vae: "/home/u/sd_models/vae".to_string(),
            }
        );
    }
}
