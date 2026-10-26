/*
File: cleaning/tools/ai_editor/engines/flux2_klein/engine/actions.rs

Purpose:
The long operations of `Flux2KleinEngine` — everything the panel ARMS and the engine then
has to see through. One `start_*` hands the work to a worker thread, one `poll_*` drains
the channel it answers on, and nothing in between runs on the GUI thread.

Main responsibilities:
- the per-component load/unload actions (`start_component_action`, `poll_component_action`);
- the Hugging Face token field, whose value goes only into the OS secret store and never
  into the settings file (the shared `HfTokenControl`, drained by `poll_hf_token`); the
  catalog download that uses it is the engine's `OcrModelDownloadController`, driven from
  `mod.rs`;
- the prompt translation (`start_translate`, `poll_translate`) and the non-blocking file
  dialogs (`start_picker`, `poll_picker`);
- the six `.prompt_cache.*` jobs, the header they share and the outcome they apply
  (`start_prompt_cache_*`, `poll_prompt_cache`, `apply_prompt_cache_outcome`).

Key functions:
- one `start_*` / `poll_*` pair per operation, all on `impl Flux2KleinEngine`

Notes:
A second inherent `impl Flux2KleinEngine` block; the state these methods move is declared
in `mod.rs` beside the struct. The component actions and the prompt-cache build share ONE
progress bar with the run and claim it by GENERATION, so a write from a retired job is
dropped — see `../../shared/stream_progress.rs`. The gate that means "wait for the current
operation" is `pipeline_busy`, in `mod.rs`, and it reads all three channels at once.
*/

use super::*;
use ms_platform::file_dialog::{self, SaveTarget};

impl Flux2KleinEngine {
    /// Starts one per-component action on a worker thread.
    ///
    /// Streaming and claiming the shared progress bar exactly as
    /// [`Self::start_prompt_cache_build`] does — loading the ~16 GB text encoder takes
    /// tens of seconds — which is also what makes it mutually exclusive with a generation.
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
        let claim = begin_progress_generation(&self.progress);
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
            let _ = tx.send(run_flux2_component_action(header, &progress, &claim));
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
                // Weights moved: the forecast's "what is resident" is stale.
                self.invalidate_forecast();
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
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] FLUX.2 klein component action failed. Component: {component}. Action: {action}. Error: {err}"
                ));
                self.component_action_status = Some(tf!(
                    "cleaning.tools.flux2_klein.component_action_error",
                    err = err
                ));
                // The action may have moved weights before it failed, so the rows on
                // screen are no longer trustworthy.
                self.status_wanted = true;
                self.invalidate_forecast();
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

    /// Drains the secret-store operation of the token field (the shared `HfTokenControl`).
    ///
    /// Nothing else is reset here: the catalog controller notices a changed token by itself
    /// and re-checks the gated repositories' access.
    pub(super) fn poll_hf_token(&mut self) {
        self.hf_token.poll(FLUX2_LOG_LABEL);
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
    /// The call BLOCKS on the network and may wait for the service's request limiter
    /// (process-wide, shared with the Translation tab), so it runs on its own worker and
    /// the GUI only polls the channel.
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

    /// Starts a non-blocking file dialog for `purpose`; at most one dialog at a time.
    pub(super) fn start_picker(&mut self, purpose: Flux2PickerPurpose) {
        if self.picker.is_some() {
            return;
        }
        // No waker, as before: the result is picked up by the next frame's `poll_picker`.
        let dialog = file_dialog::request(flux2_picker_request(purpose), None);
        self.picker = Some((purpose, dialog));
    }

    /// Folds a finished file pick into the matching model-path field, or — for the two
    /// prompt-cache purposes — starts the IPC call the dialog was opened for.
    ///
    /// A cancelled dialog is a no-op in both cases: nothing is written and no call goes
    /// out. A dialog failure is shown in the prompt-cache status line for the two cache
    /// purposes and in the run status line for the model paths.
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
                ms_log::runtime_log::log_warn(format!("[cleaning] FLUX.2 klein {purpose:?} file dialog failed: {err:?}"));
                let message = Some(err.user_message());
                match purpose {
                    Flux2PickerPurpose::PromptCacheExport | Flux2PickerPurpose::PromptCacheImport => self.prompt_cache_status = message,
                    Flux2PickerPurpose::TextEncoderDir
                    | Flux2PickerPurpose::TextEncoderFile
                    | Flux2PickerPurpose::TransformerFile
                    | Flux2PickerPurpose::TransformerDir
                    | Flux2PickerPurpose::VaeFile
                    | Flux2PickerPurpose::VaeDir => self.run_status = message,
                }
                return;
            }
        };
        // The `Picked` variant always matches the request kind (`flux2_picker_request`); a
        // mismatch is a platform-layer bug, logged and treated as a cancel.
        let mismatch = || ms_log::runtime_log::log_error(format!("[cleaning] FLUX.2 klein {purpose:?}: file dialog answered with the wrong result kind"));
        // The export keeps its save TARGET: the backend writes the file, then the target is
        // committed (`start_prompt_cache_export`).
        if purpose == Flux2PickerPurpose::PromptCacheExport {
            match picked.into_save_target() {
                Some(target) => self.start_prompt_cache_export(target),
                None => mismatch(),
            }
            return;
        }
        let path = match purpose {
            Flux2PickerPurpose::TextEncoderDir | Flux2PickerPurpose::TransformerDir | Flux2PickerPurpose::VaeDir => picked.into_directory_path(),
            Flux2PickerPurpose::TextEncoderFile | Flux2PickerPurpose::TransformerFile | Flux2PickerPurpose::VaeFile | Flux2PickerPurpose::PromptCacheImport | Flux2PickerPurpose::PromptCacheExport => {
                picked.into_file().map(|file| file.path)
            }
        };
        let Some(path) = path else {
            mismatch();
            return;
        };
        let value = path.to_string_lossy().to_string();
        match purpose {
            Flux2PickerPurpose::TextEncoderDir | Flux2PickerPurpose::TextEncoderFile => self.settings.text_encoder_path = value,
            Flux2PickerPurpose::TransformerFile | Flux2PickerPurpose::TransformerDir => {
                self.settings.transformer_path = value;
            }
            Flux2PickerPurpose::VaeFile | Flux2PickerPurpose::VaeDir => {
                self.settings.vae_path = value;
            }
            // These two carry a library entry in or out through a FILE: nothing is
            // persisted and the settings are left exactly as they were. The export was
            // dispatched above, with its save target.
            Flux2PickerPurpose::PromptCacheExport => return,
            Flux2PickerPurpose::PromptCacheImport => {
                self.start_prompt_cache_import(path);
                return;
            }
        }
        self.dirty = true;
        self.status_wanted = true;
        self.resources.arm();
        // The encoder family — and therefore the whole library listing — is decided by
        // the model paths, so a new path invalidates the list just as it does the catalog.
        self.prompt_cache_list_wanted = true;
    }

    /// Starts the prompt-cache build on a worker thread.
    ///
    /// Streaming, and it claims the shared progress bar exactly as a generation does —
    /// reading the Qwen3 encoder takes long enough that the user needs to see it move.
    /// Claiming the generation here, on the GUI thread, is what makes a cancel and a moved
    /// frame able to retire and stop it, just like a run.
    pub(super) fn start_prompt_cache_build(&mut self) {
        if self.prompt_cache_rx.is_some() {
            return;
        }
        let header = self.prompt_cache_header(&[]);
        let claim = begin_progress_generation(&self.progress);
        let progress = Arc::clone(&self.progress);
        let (tx, rx) = mpsc::channel();
        self.prompt_cache_rx = Some(rx);
        self.prompt_cache_status =
            Some(t!("cleaning.tools.flux2_klein.prompt_cache_building_status").to_string());
        // A warning belongs to the operation that produced it, not to the next one.
        self.prompt_cache_warning = None;
        thread::spawn(move || {
            let result = build_flux2_prompt_cache(header, &progress, &claim);
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

    /// Writes the entry the export dialog was opened for to the dialog's `target`.
    pub(super) fn start_prompt_cache_export(&mut self, target: SaveTarget) {
        let Some(name) = self.prompt_cache_export_name.take() else {
            return;
        };
        let path = target.write_path().to_path_buf();
        let header = self.prompt_cache_header(&[
            ("name", json!(name)),
            ("path", json!(path.to_string_lossy())),
        ]);
        self.start_prompt_cache_job(
            t!("cleaning.tools.flux2_klein.prompt_cache_exporting_status"),
            move || {
                // The backend writes the whole file to the target's write path; only then is the
                // target published (a no-op on desktop, a blocking stream on Android).
                export_flux2_prompt_cache(header)?;
                target.commit().map_err(|err| err.user_message())?;
                Ok(Flux2PromptCacheOutcome::Exported(path))
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
                    // A CHANGED library (not the first listing) re-asks the forecast: what the
                    // backend counts for the encoder may follow the prompt caches it holds.
                    if self.prompt_cache_library.as_ref().is_some_and(|previous| *previous != library) {
                        self.invalidate_forecast();
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
    /// green, and invalidates the forecast (the encoder may no longer be part of the run). A
    /// finished LOAD additionally writes the prompt the file was built from into the field,
    /// so the user sees exactly what they loaded, and persists it.
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
                // The live cache changed, so the catalog's `prompt_cached` is stale — and so is
                // the forecast: whether the run will ENCODE (and so whether the encoder counts
                // into the total) is the backend's answer about that same cache.
                self.status_wanted = true;
                self.invalidate_forecast();
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
                self.resources.arm();
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

    /// The token is a CREDENTIAL: the field's buffer reaches no settings file, no `params`
    /// and no persisted document at all (the catalog download reads it from the OS secret
    /// store).
    #[test]
    fn the_token_never_reaches_the_settings_or_the_params() {
        const SECRET: &str = "hf_a_token_that_must_never_be_persisted";

        // A fully configured engine, serialized exactly as `save_flux2_settings` writes it.
        let engine = Flux2KleinEngine {
            settings: Flux2KleinSettings {
                uncensored_text_encoder: true,
                ..runnable_settings()
            },
            hf_token: HfTokenControl { input: SECRET.to_string(), ..HfTokenControl::default() },
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

    /// A finished «Кэшировать» changes what the run will encode, so the held forecast is
    /// marked stale and asked again; a CHANGED library listing does the same, while the
    /// first listing and an identical one do not re-ask.
    #[test]
    fn a_built_prompt_cache_and_a_changed_library_re_ask_the_forecast() {
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut engine = Flux2KleinEngine::default();
        engine.resources.wanted = false;
        engine.apply_prompt_cache_outcome(Flux2PromptCacheOutcome::Built);
        assert!(engine.resources.wanted, "a built cache re-arms the forecast");

        let listing = |names: &[&str]| Flux2PromptCacheList {
            entries: names.iter().map(|name| Flux2PromptCacheEntry { name: (*name).to_string(), ..Flux2PromptCacheEntry::default() }).collect(),
            ..Flux2PromptCacheList::default()
        };
        let deliver = |engine: &mut Flux2KleinEngine, library: Flux2PromptCacheList| {
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(library)).expect("the receiver is alive");
            engine.prompt_cache_list_rx = Some(rx);
            engine.resources.wanted = false;
            engine.poll_and_maybe_query_prompt_cache_list();
            engine.resources.wanted
        };
        assert!(!deliver(&mut engine, listing(&["sfx"])), "the first listing is not a change");
        assert!(!deliver(&mut engine, listing(&["sfx"])), "an identical listing is not a change");
        assert!(deliver(&mut engine, listing(&["sfx", "title"])), "a changed listing re-arms the forecast");
    }
}
