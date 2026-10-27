/*
File: cleaning/tools/ai_editor/engines/shared/hf_download.rs

Purpose:
The secret-store controls of the process-wide Hugging Face token for the large-model engines:
the token entry field's buffer, its store / delete worker and the line about the last
operation. The token itself lives in `ms_sysprobe::hf_token`; the gated downloads that need it
are the app's model catalog (`ms_tab_translation::ocr_model_download`), which reads the stored
token on its own.

Key structures:
- `HfTokenAction`, `HfTokenControl`

Notes:
The drawing half (the token row) is `hf_download_ui.rs`. The token is never persisted, logged
or formatted here. Every text is a `cleaning.tools.hf_download.*` key.
*/

use ms_thread as thread;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// The one secret-store control the user pressed this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum HfTokenAction {
    /// Write the field's contents into the OS secret store.
    Save,
    /// Delete the stored token.
    Delete,
}

/// The Hugging Face token entry field and its secret-store worker.
///
/// The token itself lives in the process-wide `ms_sysprobe::hf_token` store; this holds
/// only what the user is TYPING (`input`, never persisted, never sent anywhere on its own),
/// the store/delete operation in flight, and the line about the last one.
#[derive(Debug, Default)]
pub(in crate::tools::ai_editor::engines) struct HfTokenControl {
    /// Buffer of the masked token field. Cleared as soon as «Сохранить» hands it to the
    /// worker, so the value does not linger on screen.
    pub(in crate::tools::ai_editor::engines) input: String,
    /// A secret-store save or delete in flight. `Ok` carries the localized outcome line;
    /// neither branch ever carries the token.
    pub(in crate::tools::ai_editor::engines) rx: Option<Receiver<Result<String, String>>>,
    /// User-facing line about the last token operation, shown beside the badge.
    pub(in crate::tools::ai_editor::engines) status: Option<String>,
}

impl HfTokenControl {
    /// Whether a store or delete is in flight.
    pub(in crate::tools::ai_editor::engines) fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// Applies the control the user pressed.
    pub(in crate::tools::ai_editor::engines) fn apply(&mut self, action: HfTokenAction) {
        match action {
            HfTokenAction::Save => self.start_store(),
            HfTokenAction::Delete => self.start_clear(),
        }
    }

    /// Writes the field into the OS secret store, on a worker thread.
    ///
    /// The keyring is an OS round trip, so it never happens on the GUI thread
    /// (`machine_translation.rs` spawns for the AI API keys for the same reason). The
    /// buffer is cleared IMMEDIATELY, before the worker even starts: the value is already
    /// on its way to the store, and leaving it in a widget only keeps a secret on screen.
    pub(in crate::tools::ai_editor::engines) fn start_store(&mut self) {
        if self.rx.is_some() {
            return;
        }
        let token = std::mem::take(&mut self.input);
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let result = ms_sysprobe::hf_token::store_hf_token(&token)
                .map(|()| t!("cleaning.tools.hf_download.token_saved_status").to_string());
            // A failed send means the engine was dropped meanwhile: nobody waits for it.
            let _ = tx.send(result);
        });
    }

    /// Deletes the stored token, on a worker thread. See [`Self::start_store`].
    pub(in crate::tools::ai_editor::engines) fn start_clear(&mut self) {
        if self.rx.is_some() {
            return;
        }
        self.input.clear();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let result = ms_sysprobe::hf_token::clear_hf_token()
                .map(|()| t!("cleaning.tools.hf_download.token_deleted_status").to_string());
            // A failed send means the engine was dropped meanwhile: nobody waits for it.
            let _ = tx.send(result);
        });
    }

    /// Drains the secret-store channel into the status line. Returns `true` when an
    /// operation SUCCEEDED: the stored token changed, so the caller must drop any access
    /// check computed for the previous one. `log_label` names the engine in the log.
    pub(in crate::tools::ai_editor::engines) fn poll(&mut self, log_label: &str) -> bool {
        let Some(rx) = self.rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(status)) => {
                self.rx = None;
                self.status = Some(status);
                true
            }
            Ok(Err(err)) => {
                self.rx = None;
                // The error carries the keyring's message, which never contains the token.
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] {log_label} Hugging Face token operation failed: {err}"
                ));
                self.status = Some(err);
                false
            }
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                self.status = Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                false
            }
            Err(TryRecvError::Empty) => false,
        }
    }
}
