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
- drive the shared `PromptCacheController` (`prompt_cache`) with FLUX.2's header and methods:
  the build (`start_prompt_cache_build`), the export / import started from the dialogs, the
  drains (`poll_prompt_cache`, `poll_and_maybe_query_prompt_cache_list`) and folding what an
  answer invalidated back into `.status` and the forecast (`apply_prompt_cache_effects`).

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
use ms_platform::file_dialog;

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
                    Flux2PickerPurpose::PromptCacheExport | Flux2PickerPurpose::PromptCacheImport => self.prompt_cache.status = message,
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
        // committed (`PromptCacheController::start_export`).
        if purpose == Flux2PickerPurpose::PromptCacheExport {
            match picked.into_save_target() {
                Some(target) => {
                    let settings = self.settings.normalized();
                    self.prompt_cache.start_export(|extra| flux2_prompt_cache_header(&settings, extra), target, &FLUX2_PROMPT_CACHE_METHODS);
                }
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
                let settings = self.settings.normalized();
                self.prompt_cache.start_import(|extra| flux2_prompt_cache_header(&settings, extra), path, &FLUX2_PROMPT_CACHE_METHODS);
                return;
            }
        }
        self.dirty = true;
        self.status_wanted = true;
        self.resources.arm();
        // Every listed entry's `current_encoder` is an answer about the encoder path, so a new
        // path invalidates the listing just as it does the catalog.
        self.prompt_cache.arm_list();
    }

    /// Starts the prompt-cache build (`renamed` = «Переименовать») through the shared
    /// controller: the preset name is the name field, the header the normalized settings, and
    /// it claims the shared progress bar exactly as a generation does.
    pub(super) fn start_prompt_cache_build(&mut self, renamed: bool) {
        let settings = self.settings.normalized();
        self.prompt_cache.start_build(|extra| flux2_prompt_cache_header(&settings, extra), renamed, &FLUX2_PROMPT_CACHE_METHODS, &self.progress);
    }

    /// Folds what a finished prompt-cache answer invalidated back into the engine: `.status`
    /// (whether the prompt is served) and the memory forecast (whether the encoder counts).
    pub(super) fn apply_prompt_cache_effects(&mut self, effects: PromptCacheEffects) {
        if effects.status_wanted {
            self.status_wanted = true;
        }
        if effects.forecast_invalidated {
            self.invalidate_forecast();
        }
    }

    /// Polls the library listing and asks a new `.list` when one is wanted, the backend is up
    /// and nothing holds the pipeline (same one-shot arming as `.status`: a failing query must
    /// not spawn a thread per frame).
    pub(super) fn poll_and_maybe_query_prompt_cache_list(&mut self) {
        let gate_open = self.ai_backend_available && !self.pipeline_busy();
        let settings = &self.settings;
        let effects = self.prompt_cache.poll_list(gate_open, |extra| flux2_prompt_cache_header(&settings.normalized(), extra), &FLUX2_PROMPT_CACHE_METHODS);
        self.apply_prompt_cache_effects(effects);
    }

    /// Drains the prompt-cache operation into the outcome line of the library fold.
    pub(super) fn poll_prompt_cache(&mut self) {
        let effects = self.prompt_cache.poll(FLUX2_LOG_LABEL);
        self.apply_prompt_cache_effects(effects);
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

    /// A finished «Кэшировать» changes what the run will encode, so the held forecast and
    /// `.status` are re-asked and the listing too; a listing that differs from the one held
    /// (the first one included: it is what migrates the old caches) re-asks `.status` and the
    /// forecast, an identical one does not.
    #[test]
    fn a_built_prompt_cache_and_a_changed_library_re_ask_status_and_the_forecast() {
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut engine = Flux2KleinEngine { status_wanted: false, ..Flux2KleinEngine::default() };
        engine.prompt_cache.list_wanted = false;
        engine.resources.wanted = false;
        let (_line, effects) = engine.prompt_cache.apply_outcome(PromptCacheOutcome::Built { written: Vec::new(), renamed: true });
        engine.apply_prompt_cache_effects(effects);
        assert!(engine.resources.wanted && engine.status_wanted && engine.prompt_cache.list_wanted, "a build re-asks everything it changed");

        let listing = |texts: &[&str]| {
            parse_prompt_library_listing(&json!({
                "texts": texts.iter().map(|text| json!({ "prompt": text, "entries": [{ "file": format!("{text}.msprompt") }] })).collect::<Vec<_>>()
            }))
        };
        let deliver = |engine: &mut Flux2KleinEngine, library: PromptLibraryListing| {
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(library)).expect("the receiver is alive");
            engine.prompt_cache.list_rx = Some(rx);
            engine.prompt_cache.list_wanted = false;
            engine.resources.wanted = false;
            engine.status_wanted = false;
            engine.poll_and_maybe_query_prompt_cache_list();
            (engine.resources.wanted, engine.status_wanted)
        };
        let mut fresh = Flux2KleinEngine::default();
        assert_eq!(deliver(&mut fresh, listing(&["sfx"])), (true, true), "the first listing may have migrated old caches");
        assert_eq!(deliver(&mut fresh, listing(&["sfx"])), (false, false), "an identical listing is not a change");
        assert_eq!(deliver(&mut fresh, listing(&["sfx", "title"])), (true, true), "a changed listing re-arms both");
    }
}
