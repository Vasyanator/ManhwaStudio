/*
File: torch/constraints.rs

Purpose:
Writes the pip constraints file that pins the INSTALLED torch / torchvision for every PyPI
install that follows the Torch stage, for every flavour.

Key items:
- `TORCH_CONSTRAINTS_FILE`, `torch_constraints_content()`, `write_torch_constraints()`

Notes:
Without it a later `uv pip install --upgrade …` (or a dependency that wants a newer torch)
silently swaps a ROCm / CUDA torch for the PyPI build. With `-c` the resolver keeps the
installed one or fails loudly (verified with uv against a stub `+rocm` torch). Blocking file
I/O: worker threads only.
*/

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// File name of the constraints file inside `installer_files/`.
const TORCH_CONSTRAINTS_FILE: &str = "torch-constraints.txt";

/// The constraints text for `installed` (a `pip freeze` snapshot keyed by normalized name):
/// `torch==<v>` and, when present, `torchvision==<v>`, one per line. `None` without torch.
#[must_use]
pub(crate) fn torch_constraints_content(installed: &HashMap<String, String>) -> Option<String> {
    let torch = installed.get("torch")?;
    let mut content = format!("torch=={torch}\n");
    if let Some(torchvision) = installed.get("torchvision") {
        content.push_str(&format!("torchvision=={torchvision}\n"));
    }
    Some(content)
}

/// Writes `root_dir/installer_files/torch-constraints.txt` for `installed` and returns its
/// path, or `None` (nothing written) when `installed` has no torch.
///
/// # Errors
/// A localized message when `installer_files/` cannot be created or the file not written.
pub(crate) fn write_torch_constraints(root_dir: &Path, installed: &HashMap<String, String>) -> Result<Option<PathBuf>, String> {
    let Some(content) = torch_constraints_content(installed) else {
        return Ok(None);
    };
    let dir = root_dir.join("installer_files");
    let path = dir.join(TORCH_CONSTRAINTS_FILE);
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&path, content))
        .map_err(|err| {
            ms_log::runtime_log::log_error(format!("[torch] failed to write the constraints file '{}': {err}", path.display()));
            tf!("installer.torch.constraints_write_error", path = path.display(), err = err)
        })?;
    Ok(Some(path))
}
