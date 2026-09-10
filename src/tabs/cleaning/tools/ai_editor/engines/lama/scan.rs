/*
File: cleaning/tools/ai_editor/engines/lama/scan.rs

Purpose:
The background presence scan of the LaMa model directories and the one status line it
feeds — «Файл найден» / «Будет скачана перед запуском» / «Ожидается от установщика».

Main responsibilities:
- list the checkpoints actually on disk in BOTH model directories, off the GUI thread;
- hold the scan's lifecycle (`LamaModelListState`) and drain its channel;
- turn "this entry, this scan result" into the status string the panel prints.

Key structures:
- `LamaModelScan`: what was found, per backend method
- `LamaModelListState`: idle / in flight / ready / failed

Key functions:
- `scan_lama_models()`, `lama_model_status_text()`

Notes:
The scan is INFORMATIONAL: the picker always offers the full fixed catalog, whatever the
scan found, because a missing file is downloaded before the run rather than hidden from the
list. Both directories are read because the four entries do not share one — `Torch/LaMa/models`
for the `inpaint.lama_v2` checkpoints and `Torch/LaMa_MPE` for the MPE one — and the results
are kept apart per method so a same-named file in the other directory could never be read as
a hit.
*/

use super::*;

/// What one scan found, per backend method. Both lists hold bare file names, sorted.
#[derive(Debug, Default, Clone)]
pub(super) struct LamaModelScan {
    /// Checkpoints present in `Torch/LaMa/models`.
    pub(super) v2: Vec<String>,
    /// Checkpoints present in `Torch/LaMa_MPE`.
    pub(super) mpe: Vec<String>,
}

impl LamaModelScan {
    /// Whether `spec`'s file was found in the directory that BELONGS to its method.
    #[must_use]
    pub(super) fn contains(&self, spec: &LamaModelSpec) -> bool {
        let present = match spec.method {
            LamaMethod::V2 => &self.v2,
            LamaMethod::Mpe => &self.mpe,
        };
        present.iter().any(|name| name == spec.file_name)
    }
}

/// Lifecycle of the presence scan.
///
/// `Idle` is both the initial state and what a «Проверить модели» press restores, which is
/// what makes the refresh button a one-liner: the poll re-arms a scan whenever it sees it.
#[derive(Debug)]
pub(super) enum LamaModelListState {
    Idle,
    Loading(Receiver<Result<LamaModelScan, String>>),
    Ready(LamaModelScan),
    Error(String),
}

/// Lists the checkpoints present in both model directories.
///
/// Runs on a worker thread: it touches the filesystem, which the GUI thread may not.
///
/// # Errors
/// Returns a localized message naming the directory when it cannot be read. A directory that
/// does not exist yet is NOT an error — it is the normal state before the first download.
pub(super) fn scan_lama_models() -> Result<LamaModelScan, String> {
    Ok(LamaModelScan {
        v2: scan_model_dir(&LamaMethod::V2.models_dir())?,
        mpe: scan_model_dir(&LamaMethod::Mpe.models_dir())?,
    })
}

/// Lists the `.ckpt` / `.pt` files directly inside `models_dir`, sorted.
///
/// # Errors
/// Returns a localized message when the directory or one of its entries cannot be read.
fn scan_model_dir(models_dir: &Path) -> Result<Vec<String>, String> {
    if !models_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(models_dir).map_err(|err| {
        tf!("cleaning.tools.lama.read_models_dir_error", models_dir = models_dir.display(), err = err)
    })?;
    let mut models = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            tf!("cleaning.tools.lama.read_models_entry_error", models_dir = models_dir.display(), err = err)
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if is_supported_lama_model_path(&path) {
            models.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    models.sort();
    Ok(models)
}

/// Whether a file could be a LaMa checkpoint at all, by extension.
///
/// This is the SCAN's filter, not a model-capability test: what a checkpoint supports is
/// declared by its catalog entry (`LamaModelSpec::supports_refine`), never re-derived here.
fn is_supported_lama_model_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ckpt") || ext.eq_ignore_ascii_case("pt"))
}

/// The one-line presence status shown under the model picker for `spec`.
///
/// Every entry of the catalog is downloadable, so "not found" reads as «будет скачана»;
/// the installer wording is kept for a name the catalog does not know at all, which can
/// only reach here through a hand-edited settings file.
#[must_use]
pub(super) fn lama_model_status_text(
    spec_name: &str,
    model_list_state: &LamaModelListState,
) -> &'static str {
    match model_list_state {
        LamaModelListState::Idle | LamaModelListState::Loading(_) => {
            t!("cleaning.tools.lama.checking_files_status")
        }
        LamaModelListState::Ready(scan) => match lama_model_spec_by_name(spec_name) {
            Some(spec) if scan.contains(spec) => t!("cleaning.tools.lama.file_found_status"),
            Some(_) => t!("cleaning.tools.lama.will_download_status"),
            None => t!("cleaning.tools.lama.expected_from_installer_status"),
        },
        LamaModelListState::Error(_) => t!("cleaning.tools.lama.status_unavailable_status"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scan result answers PER METHOD: the MPE checkpoint found in `Torch/LaMa_MPE` must
    /// not be reported as present just because a file of that name sits in the v2 folder,
    /// and vice versa.
    #[test]
    fn presence_is_answered_in_the_directory_that_owns_the_entry() {
        let scan = LamaModelScan {
            v2: vec!["best.ckpt".to_string()],
            mpe: vec!["inpainting_lama_mpe.ckpt".to_string()],
        };
        let spec = |name: &str| lama_model_spec_by_name(name).expect("catalog entry");
        assert!(scan.contains(spec("best.ckpt")));
        assert!(scan.contains(spec("inpainting_lama_mpe.ckpt")));
        assert!(!scan.contains(spec("anime-manga-big-lama.pt")));

        let crossed = LamaModelScan {
            v2: vec!["inpainting_lama_mpe.ckpt".to_string()],
            mpe: vec!["best.ckpt".to_string()],
        };
        assert!(!crossed.contains(spec("best.ckpt")));
        assert!(!crossed.contains(spec("inpainting_lama_mpe.ckpt")));
    }

    /// The status line reports the three states the user can act on, and says nothing
    /// definite while a scan is still in flight.
    #[test]
    fn the_status_line_distinguishes_found_downloadable_and_unknown() {
        let ready = LamaModelListState::Ready(LamaModelScan {
            v2: vec!["best.ckpt".to_string()],
            mpe: Vec::new(),
        });
        assert_eq!(
            lama_model_status_text("best.ckpt", &ready),
            t!("cleaning.tools.lama.file_found_status")
        );
        assert_eq!(
            lama_model_status_text("lama_large_512px.ckpt", &ready),
            t!("cleaning.tools.lama.will_download_status")
        );
        assert_eq!(
            lama_model_status_text("hand-edited.ckpt", &ready),
            t!("cleaning.tools.lama.expected_from_installer_status")
        );
        assert_eq!(
            lama_model_status_text("best.ckpt", &LamaModelListState::Idle),
            t!("cleaning.tools.lama.checking_files_status")
        );
        assert_eq!(
            lama_model_status_text("best.ckpt", &LamaModelListState::Error("boom".to_string())),
            t!("cleaning.tools.lama.status_unavailable_status")
        );
    }

    /// Only `.ckpt` / `.pt` files are checkpoints, and the test is case-insensitive because
    /// the Windows target reports whatever case the filesystem stored.
    #[test]
    fn only_checkpoint_extensions_are_scanned() {
        assert!(is_supported_lama_model_path(Path::new("/m/best.ckpt")));
        assert!(is_supported_lama_model_path(Path::new("/m/big.PT")));
        assert!(!is_supported_lama_model_path(Path::new("/m/config.yaml")));
        assert!(!is_supported_lama_model_path(Path::new("/m/README")));
    }

    /// A model directory that does not exist yet is the normal pre-download state, not a
    /// failure: reporting it as an error would put a red line under an untouched install.
    #[test]
    fn a_missing_directory_scans_as_empty() {
        let missing = Path::new("/definitely/not/a/models/dir/for/manhwastudio");
        assert_eq!(scan_model_dir(missing), Ok(Vec::new()));
    }
}
