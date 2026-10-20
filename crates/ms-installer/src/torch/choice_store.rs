/*
File: torch/choice_store.rs

Purpose:
Reads and persists the recorded PyTorch choice (`General.torch_flavour` /
`General.torch_rocm_gfx`, vocabulary in `ms_config::torch_choice`) in an install root's
`user_config` document.

Key functions:
- `read_torch_choice_record()`: read-only probe; any failure reads as `Unrecorded`.
- `persist_torch_choice_record()`: one docstore update of `General`.

Notes:
The write goes through `install::update_install_target_general`, the same transaction the
installer uses for `General.ai_install_type` (defaults backfill, malformed document left
untouched). Blocking file I/O: worker threads only.
*/

use std::path::Path;

use ms_config as config;
use ms_config::torch_choice::TorchChoiceRecord;

use crate::install::update_install_target_general;

/// Reads the recorded torch choice of the install root `root_dir` without creating or
/// rewriting its `user_config`.
///
/// An absent document or key reads as `Unrecorded`; an unreadable or malformed document is
/// logged and also reads as `Unrecorded` — the legacy rule, which never forces a reinstall.
#[must_use]
pub fn read_torch_choice_record(root_dir: &Path) -> TorchChoiceRecord {
    let doc = ms_docstore::DocRef::new(root_dir.join(config::USER_CONFIG_FILE), ms_docstore::DocKind::UserConfig);
    match ms_docstore::read_value(&doc) {
        Ok(Some(settings)) => TorchChoiceRecord::from_user_settings(&settings),
        Ok(None) => TorchChoiceRecord::Unrecorded,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[torch] failed to read the recorded PyTorch choice of '{}': {err}; assuming unrecorded",
                root_dir.display()
            ));
            TorchChoiceRecord::Unrecorded
        }
    }
}

/// Writes `record` into `General` of the install root `root_dir`'s `user_config`.
///
/// # Errors
/// A localized message when the document cannot be read, is malformed (it is then left
/// untouched) or cannot be written. The failure is also logged.
pub(crate) fn persist_torch_choice_record(root_dir: &Path, record: &TorchChoiceRecord) -> Result<(), String> {
    update_install_target_general(root_dir, |general| record.write_into_general(general)).map_err(|err| {
        let path = root_dir.join(config::USER_CONFIG_FILE);
        ms_log::runtime_log::log_error(format!(
            "[torch] failed to record the PyTorch choice {record:?} in '{}': {err}",
            path.display()
        ));
        tf!("installer.torch.record_save_error", path = path.display(), err = err.to_string())
    })
}
