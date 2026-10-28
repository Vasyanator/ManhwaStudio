/*
File: cleaning/tools/ai_editor/engines/flux3_klein/sdcpp_files.rs

Purpose:
Which files Stable-Diffusion.cpp loads for the FLUX.3 klein selection: the diffusion model, the
autoencoder and the Qwen3-VL text encoder under the package root, for the DiT format sd.cpp runs
(`Flux3KleinSettings::run_dit_format`) and the selected encoder format; and the worker-side check
that those files exist and fit together before a run is offered.

Main responsibilities:
- map the selection onto `ms_sdcpp::ModelFiles` (`flux3_sdcpp_model_files`, pure): the package
  root is `effective_root()` in BOTH source modes (download = the catalog's directory, manual =
  the user's package folder with the same layout), the paths are the catalog's
  (`flux3_klein_sdcpp_paths`); no root / a relative root are refused locally;
- validate the mapped files on a WORKER (`resolve_flux3_sdcpp_files`: `validate_model_files`
  reads the headers, so a missing file, a diffusers layout or a text encoder of the wrong size —
  which would abort sd.cpp mid-run — is refused before launch);
- the answer the files worker hands the engine (`Flux3SdcppFilesAnswer`, keyed by the selection
  it describes) and the state the panel and the run gate read (`Flux3SdcppFilesState`).

Key structures:
- `Flux3SdcppFilesError`, `Flux3SdcppFilesKey`, `Flux3SdcppFilesAnswer`, `Flux3SdcppFilesState`

Key functions:
- `flux3_sdcpp_model_files()`, `resolve_flux3_sdcpp_files()`, `flux3_sdcpp_files_key()`

Notes:
Never called per frame for the disk half: the engine runs `resolve_flux3_sdcpp_files` on its
files worker when the key changes (or on «Обновить» / a catalog job's end). The texts are
`ms_sdcpp::ModelFilesError`'s localized `Display` and the engine's own root lines.
*/

use super::*;
use ms_sdcpp::{ComponentRole, ModelFamily, ModelFiles, ModelFilesError};

/// Why the selection has no usable sd.cpp model files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux3SdcppFilesError {
    /// Manual mode without a package folder.
    NoRoot,
    /// The package folder is not an absolute path.
    RootNotAbsolute,
    /// The files under the root are missing or unusable (`validate_model_files`'s answer, or a
    /// format the catalog maps to no sd.cpp file).
    Files(ModelFilesError),
}

impl Flux3SdcppFilesError {
    /// The localized refusal a run gate shows.
    pub(super) fn message(&self) -> String {
        match self {
            Self::NoRoot => t!("cleaning.tools.flux3_klein.model_root_required_error").to_string(),
            Self::RootNotAbsolute => t!("cleaning.tools.flux3_klein.model_root_not_absolute_error").to_string(),
            Self::Files(err) => tf!("cleaning.tools.flux3_klein.sdcpp_files_error", err = err),
        }
    }
}

/// What a files answer describes: the package root, the DiT format sd.cpp runs and the encoder
/// format. An answer counts only while it equals the current selection's key.
pub(super) type Flux3SdcppFilesKey = (String, Flux3DitFormat, Flux3EncoderFormat);

/// The key of `settings`' sd.cpp selection (cheap: no disk access).
#[must_use]
pub(super) fn flux3_sdcpp_files_key(settings: &Flux3KleinSettings) -> Flux3SdcppFilesKey {
    (settings.effective_root(), flux3_closest_offered(settings.dit_format(), ImageEditRuntime::SdCpp), settings.encoder_format())
}

/// One answer of the files worker.
#[derive(Debug, Clone)]
pub(super) struct Flux3SdcppFilesAnswer {
    /// The selection the answer describes.
    pub(super) key: Flux3SdcppFilesKey,
    /// The validated files (paths as sd.cpp opens them), or why there are none.
    pub(super) files: Result<ModelFiles, Flux3SdcppFilesError>,
    /// The device saved in Settings › AI backend › Stable-Diffusion.cpp (the readiness line
    /// reports a saved device the last probe did not find).
    pub(super) device: ms_config::SdCppDeviceChoice,
}

/// What the panel and the run gate know about the sd.cpp files of the CURRENT selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux3SdcppFilesState {
    /// No answer about this selection yet (asked, or about to be).
    Checking,
    /// The files are there and fit together.
    Ready,
    /// They are not usable; the reason.
    Refused(Flux3SdcppFilesError),
}

impl Flux3SdcppFilesState {
    /// The state `answer` gives the selection `current`: an answer about another selection is
    /// no answer.
    #[must_use]
    pub(super) fn of(answer: Option<&Flux3SdcppFilesAnswer>, current: &Flux3SdcppFilesKey) -> Self {
        match answer.filter(|answer| answer.key == *current).map(|answer| &answer.files) {
            None => Self::Checking,
            Some(Ok(_)) => Self::Ready,
            Some(Err(err)) => Self::Refused(err.clone()),
        }
    }
}

/// Maps `settings` onto the files sd.cpp loads (pure path joining, no disk access): the catalog
/// paths under `effective_root()` for the DiT format sd.cpp runs (a stored fp8r / nvfp4 maps to
/// its closest offered format) and the selected encoder (bf16 = the shard index JSON, GGUF = the
/// file).
///
/// # Errors
/// [`Flux3SdcppFilesError::NoRoot`] / [`Flux3SdcppFilesError::RootNotAbsolute`] for a missing or
/// relative package folder; `Files(UnsupportedLayout)` if the catalog maps the format to no sd.cpp
/// file (cannot happen for a format `Flux3DitFormat::offered_on(SdCpp)` lists; logged).
pub(super) fn flux3_sdcpp_model_files(settings: &Flux3KleinSettings) -> Result<ModelFiles, Flux3SdcppFilesError> {
    let (root, dit, encoder) = flux3_sdcpp_files_key(settings);
    if root.is_empty() {
        return Err(Flux3SdcppFilesError::NoRoot);
    }
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        return Err(Flux3SdcppFilesError::RootNotAbsolute);
    }
    let Some(paths) = flux3_klein_sdcpp_paths(&root, dit, encoder) else {
        ms_log::runtime_log::log_error(format!("[cleaning] {FLUX3_LOG_LABEL} the catalog maps the DiT format {} to no sd.cpp file", dit.wire()));
        return Err(Flux3SdcppFilesError::Files(ModelFilesError::UnsupportedLayout { role: ComponentRole::Diffusion, path: root }));
    };
    let components = [(ComponentRole::Diffusion, paths.diffusion), (ComponentRole::Vae, paths.vae), (ComponentRole::Llm, paths.llm)].into_iter().collect();
    Ok(ModelFiles { family: ModelFamily::Flux3Klein, components })
}

/// Maps `settings` ([`flux3_sdcpp_model_files`]) and validates the files for sd.cpp
/// (`ms_sdcpp::validate_model_files`: presence, layout, family, encoder architecture and width).
///
/// # Errors
/// The mapping's refusal, or the first [`ModelFilesError`] (logged with the selection).
///
/// # Threading
/// Blocking file I/O (header reads): worker threads only.
pub(super) fn resolve_flux3_sdcpp_files(settings: &Flux3KleinSettings) -> Result<ModelFiles, Flux3SdcppFilesError> {
    let files = flux3_sdcpp_model_files(settings)?;
    ms_sdcpp::validate_model_files(&files).map_err(|err| {
        ms_log::runtime_log::log_warn(format!(
            "[cleaning] {FLUX3_LOG_LABEL} sd.cpp files refused: {err:?} (DiT {}, encoder {}, source {})",
            settings.run_dit_format().wire(),
            settings.encoder_format().wire(),
            settings.source_mode().wire()
        ));
        Flux3SdcppFilesError::Files(err)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fictional absolute root under the temp dir (never created).
    fn fictional_root(tag: &str) -> String {
        std::env::temp_dir().join(format!("ms_flux3_sdcpp_files_{tag}_{}", std::process::id())).to_string_lossy().to_string()
    }

    fn path_of(files: &ModelFiles, role: ComponentRole) -> PathBuf {
        files.components.get(&role).cloned().expect("every role is mapped")
    }

    /// Download and manual mode map the same catalog layout under their root: the DiT format
    /// sd.cpp runs (fp8r falls back to the Q8_0 GGUF), the AE, and the encoder (shard index for
    /// bf16, the GGUF file otherwise).
    #[test]
    fn the_selection_maps_onto_the_catalog_files_under_the_root() {
        let manual_root = fictional_root("manual");
        let download = Flux3KleinSettings { runtime: "sdcpp".to_string(), ..Flux3KleinSettings::default() };
        let manual = Flux3KleinSettings { source_mode: "manual".to_string(), model_root: manual_root.clone(), ..download.clone() };
        for (settings, root) in [(download, config::flux3_klein_dir().to_string_lossy().to_string()), (manual, manual_root)] {
            let files = flux3_sdcpp_model_files(&settings).expect("an absolute root maps");
            assert_eq!(files.family, ModelFamily::Flux3Klein);
            assert_eq!(path_of(&files, ComponentRole::Diffusion), PathBuf::from(package_path(&root, flux3_klein_dit_relative_path(Flux3DitFormat::Q80))), "fp8r runs as Q8_0");
            assert_eq!(path_of(&files, ComponentRole::Vae), PathBuf::from(package_path(&root, "ae.safetensors")));
            assert_eq!(path_of(&files, ComponentRole::Llm), PathBuf::from(package_path(&root, "text_encoder/model.safetensors.index.json")));
            for path in files.components.values() {
                assert!(path.starts_with(&root), "{}", path.display());
            }
        }
        let gguf = Flux3KleinSettings {
            runtime: "sdcpp".to_string(),
            format: "bf16".to_string(),
            text_encoder_format: "q4_k_m".to_string(),
            source_mode: "manual".to_string(),
            model_root: fictional_root("gguf"),
            ..Flux3KleinSettings::default()
        };
        let files = flux3_sdcpp_model_files(&gguf).expect("maps");
        assert_eq!(path_of(&files, ComponentRole::Diffusion), PathBuf::from(package_path(&gguf.model_root, "model.safetensors")), "bf16 is the root file");
        assert_eq!(path_of(&files, ComponentRole::Llm), PathBuf::from(package_path(&gguf.model_root, flux3_klein_encoder_relative_path(Flux3EncoderFormat::Q4KM))));
    }

    /// No folder and a relative folder are refused locally, with the root lines.
    #[test]
    fn a_missing_or_relative_root_is_refused_without_the_disk() {
        let none = Flux3KleinSettings { source_mode: "manual".to_string(), ..Flux3KleinSettings::default() };
        assert_eq!(flux3_sdcpp_model_files(&none), Err(Flux3SdcppFilesError::NoRoot));
        let relative = Flux3KleinSettings { model_root: "models/flux3".to_string(), ..none };
        assert_eq!(flux3_sdcpp_model_files(&relative), Err(Flux3SdcppFilesError::RootNotAbsolute));
        assert_eq!(resolve_flux3_sdcpp_files(&relative), Err(Flux3SdcppFilesError::RootNotAbsolute));
    }

    /// Absent files are refused by the validation (nothing is created on disk), naming the role.
    #[test]
    fn absent_files_are_refused_before_a_run() {
        let settings = Flux3KleinSettings { source_mode: "manual".to_string(), model_root: fictional_root("absent"), ..Flux3KleinSettings::default() };
        match resolve_flux3_sdcpp_files(&settings) {
            Err(Flux3SdcppFilesError::Files(ModelFilesError::Missing { role, .. })) => assert_eq!(role, ComponentRole::Diffusion),
            other => panic!("expected a missing DiT, got {other:?}"),
        }
    }

    /// An answer counts only for the selection it describes.
    #[test]
    fn a_files_answer_counts_only_for_its_selection() {
        let settings = Flux3KleinSettings { source_mode: "manual".to_string(), model_root: fictional_root("state"), ..Flux3KleinSettings::default() };
        let key = flux3_sdcpp_files_key(&settings);
        let files = flux3_sdcpp_model_files(&settings).expect("maps");
        let answer = Flux3SdcppFilesAnswer { key: key.clone(), files: Ok(files), device: ms_config::SdCppDeviceChoice::Auto };
        assert_eq!(Flux3SdcppFilesState::of(Some(&answer), &key), Flux3SdcppFilesState::Ready);
        assert_eq!(Flux3SdcppFilesState::of(None, &key), Flux3SdcppFilesState::Checking);
        let other = flux3_sdcpp_files_key(&Flux3KleinSettings { text_encoder_format: "q8_0".to_string(), ..settings });
        assert_eq!(Flux3SdcppFilesState::of(Some(&answer), &other), Flux3SdcppFilesState::Checking, "another encoder");
        let refused = Flux3SdcppFilesAnswer { files: Err(Flux3SdcppFilesError::NoRoot), ..answer };
        assert_eq!(Flux3SdcppFilesState::of(Some(&refused), &key), Flux3SdcppFilesState::Refused(Flux3SdcppFilesError::NoRoot));
    }
}
