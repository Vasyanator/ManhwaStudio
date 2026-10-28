/*
File: cleaning/tools/ai_editor/engines/flux2_klein/sdcpp_files.rs

Purpose:
Which three files Stable-Diffusion.cpp loads for this FLUX.2 klein engine: the diffusion
transformer, the VAE and the Qwen3 text encoder, from the catalog bundle (download mode) or from
the user's three path rows (manual mode), checked to exist.

Key items:
- `Flux2SdcppFilesKey`: the settings facts the answer depends on (re-resolve when it changes).
- `Flux2SdcppComponent`, `Flux2SdcppFilesError` (+ `message`).
- `flux2_sdcpp_map_files()`: the map (exists?), `flux2_sdcpp_model_files()`: the map plus
  `ms_sdcpp::validate_model_files` (family, encoder pairing). WORKER ONLY (disk, headers).

Notes:
Download mode is `external_catalog::flux2_klein_sdcpp_paths` (the sd.cpp bundle: the BFL single
file or a GGUF transformer, the base package's VAE file, the encoder's shard index or GGUF).
Manual mode maps a folder onto the file sd.cpp needs: encoder folder -> its
`model.safetensors.index.json` (or a single `model.safetensors`), VAE folder ->
`diffusion_pytorch_model.safetensors`; a transformer FOLDER is a diffusers layout sd.cpp cannot
load and is refused with the hint to pick a single file or a GGUF. Whether the three files
belong together (DiT <-> encoder pairing) is `ms_sdcpp::validate_model_files`, run here on the
status worker (so the readiness line names a wrong encoder before any run) and again by the
service before any worker starts.
*/

use super::*;
use ms_sdcpp::{ComponentRole, ModelFamily, ModelFiles};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The encoder shard index of a diffusers-style Qwen3 folder.
const ENCODER_INDEX_FILE: &str = "model.safetensors.index.json";
/// The single-file encoder of a folder without an index.
const ENCODER_SINGLE_FILE: &str = "model.safetensors";
/// The weights file of a diffusers VAE folder.
const VAE_WEIGHTS_FILE: &str = "diffusion_pytorch_model.safetensors";

/// The settings facts [`flux2_sdcpp_model_files`] depends on; a change means the held answer
/// describes other files. Built from the settings every frame (strings only, no disk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Flux2SdcppFilesKey {
    variant: Flux2Variant,
    source_mode: Flux2SourceMode,
    selection: Flux2WeightSelection,
    text_encoder_path: String,
    transformer_path: String,
    vae_path: String,
}

impl Flux2SdcppFilesKey {
    /// The key of `settings` (the manual fields trimmed, as `effective_paths` reads them).
    #[must_use]
    pub(super) fn of(settings: &Flux2KleinSettings) -> Self {
        Self {
            variant: settings.variant(),
            source_mode: settings.source_mode(),
            selection: settings.weight_selection(),
            text_encoder_path: settings.text_encoder_path.trim().to_string(),
            transformer_path: settings.transformer_path.trim().to_string(),
            vae_path: settings.vae_path.trim().to_string(),
        }
    }
}

/// One of the three files sd.cpp loads, for messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2SdcppComponent {
    TextEncoder,
    Transformer,
    Vae,
}

impl Flux2SdcppComponent {
    /// The localized component name (the progress bar's component names).
    fn label(self) -> &'static str {
        match self {
            Self::TextEncoder => t!("cleaning.tools.engine_progress.component_text_encoder_label"),
            Self::Transformer => t!("cleaning.tools.engine_progress.component_transformer_label"),
            Self::Vae => t!("cleaning.tools.engine_progress.component_vae_label"),
        }
    }
}

/// Why the sd.cpp files of the current selection cannot be named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Flux2SdcppFilesError {
    /// Manual mode with an empty path row.
    PathsRequired,
    /// Manual mode with a transformer FOLDER (diffusers layout): sd.cpp loads one file.
    DiffusersTransformer,
    /// The catalog has no sd.cpp file for the selected formats (never for an offered format;
    /// the catalog's tests pin one).
    SelectionUnsupported,
    /// These components are not on disk; `download` tells which fix applies.
    Missing { download: bool, components: Vec<Flux2SdcppComponent> },
    /// The files exist but do not form a FLUX.2 klein model sd.cpp can load (wrong family, a
    /// mis-paired encoder, an unreadable header): `ms_sdcpp::validate_model_files`' localized text.
    Invalid(String),
}

impl Flux2SdcppFilesError {
    /// The localized refusal.
    #[must_use]
    pub(super) fn message(&self) -> String {
        match self {
            Self::PathsRequired => t!("cleaning.tools.flux2_klein.paths_required_error").to_string(),
            Self::DiffusersTransformer => t!("cleaning.tools.flux2_klein.sdcpp_diffusers_transformer_error").to_string(),
            Self::SelectionUnsupported => t!("cleaning.tools.flux2_klein.sdcpp_selection_unsupported_error").to_string(),
            Self::Invalid(text) => text.clone(),
            Self::Missing { download, components } => {
                let components = components.iter().map(|component| component.label()).collect::<Vec<_>>().join(", ");
                if *download {
                    tf!("cleaning.tools.flux2_klein.model_not_downloaded_error", components = components)
                } else {
                    tf!("cleaning.tools.flux2_klein.model_components_missing_error", components = components)
                }
            }
        }
    }
}

/// The three files sd.cpp loads for `settings`, existing and checked to form a loadable FLUX.2
/// klein model (`ms_sdcpp::validate_model_files`: the family and the DiT <-> encoder pairing, so
/// a wrong encoder is refused on the readiness line, before any run). `side_models_root` is
/// `ms_config::side_models_dir()`.
///
/// # Errors
/// Those of [`flux2_sdcpp_map_files`], and [`Flux2SdcppFilesError::Invalid`].
///
/// # Threading
/// Stats the disk and reads bounded file headers: worker threads only (the engine's sd.cpp
/// status worker).
pub(super) fn flux2_sdcpp_model_files(settings: &Flux2KleinSettings, side_models_root: &Path) -> Result<ModelFiles, Flux2SdcppFilesError> {
    let files = flux2_sdcpp_map_files(settings, side_models_root)?;
    ms_sdcpp::validate_model_files(&files).map_err(|err| {
        ms_log::runtime_log::log_warn(format!("[cleaning] {FLUX2_LOG_LABEL}: the sd.cpp model files do not form a loadable model: {err:?}"));
        Flux2SdcppFilesError::Invalid(err.to_string())
    })
}

/// The three files sd.cpp loads for `settings` (normalized or not: only the key facts are
/// read), each checked to be an existing file. `side_models_root` is
/// `ms_config::side_models_dir()` (the catalog's root; a test passes a scratch directory).
///
/// # Errors
/// [`Flux2SdcppFilesError`]: an empty manual row, a diffusers transformer folder, a selection
/// without sd.cpp files, or the components not found on disk (all of them named).
///
/// # Threading
/// Stats the disk: worker threads only.
pub(super) fn flux2_sdcpp_map_files(settings: &Flux2KleinSettings, side_models_root: &Path) -> Result<ModelFiles, Flux2SdcppFilesError> {
    let (diffusion, vae, llm, download) = match settings.source_mode() {
        Flux2SourceMode::Download => {
            let selection = settings.weight_selection();
            let paths = flux2_klein_sdcpp_paths(side_models_root, settings.variant(), selection.transformer, selection.source, selection.text_encoder).ok_or(Flux2SdcppFilesError::SelectionUnsupported)?;
            (Some(paths.diffusion), Some(paths.vae), Some(paths.llm), true)
        }
        Flux2SourceMode::Manual => {
            let row = |value: &str| (!value.trim().is_empty()).then(|| PathBuf::from(value.trim()));
            let (Some(encoder), Some(transformer), Some(vae)) = (row(&settings.text_encoder_path), row(&settings.transformer_path), row(&settings.vae_path)) else {
                return Err(Flux2SdcppFilesError::PathsRequired);
            };
            // A diffusers transformer folder is refused before anything else: no file of it
            // is what sd.cpp loads, so "missing" would send the user looking for the wrong fix.
            if transformer.is_dir() {
                return Err(Flux2SdcppFilesError::DiffusersTransformer);
            }
            let encoder = if encoder.is_dir() { [ENCODER_INDEX_FILE, ENCODER_SINGLE_FILE].into_iter().map(|name| encoder.join(name)).find(|path| path.is_file()) } else { Some(encoder) };
            let vae = if vae.is_dir() { Some(vae.join(VAE_WEIGHTS_FILE)) } else { Some(vae) };
            (Some(transformer), vae, encoder, false)
        }
    };
    let mut components = BTreeMap::new();
    let mut missing = Vec::new();
    for (component, role, path) in [(Flux2SdcppComponent::TextEncoder, ComponentRole::Llm, llm), (Flux2SdcppComponent::Transformer, ComponentRole::Diffusion, diffusion), (Flux2SdcppComponent::Vae, ComponentRole::Vae, vae)] {
        match path.filter(|path| path.is_file()) {
            Some(path) => {
                components.insert(role, path);
            }
            None => missing.push(component),
        }
    }
    if !missing.is_empty() {
        return Err(Flux2SdcppFilesError::Missing { download, components: missing });
    }
    Ok(ModelFiles { family: ModelFamily::Flux2Klein, components })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A unique scratch directory under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or_default();
            let dir = std::env::temp_dir().join(format!("ms-flux2-sdcpp-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().expect("a file has a parent")).expect("parent dir");
            fs::write(&path, b"x").expect("scratch file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Err(err) = fs::remove_dir_all(&self.0) {
                eprintln!("could not remove {}: {err}", self.0.display());
            }
        }
    }

    fn manual(encoder: &Path, transformer: &Path, vae: &Path) -> Flux2KleinSettings {
        Flux2KleinSettings {
            source_mode: Flux2SourceMode::Manual.wire().to_string(),
            text_encoder_path: encoder.to_string_lossy().to_string(),
            transformer_path: transformer.to_string_lossy().to_string(),
            vae_path: vae.to_string_lossy().to_string(),
            ..Flux2KleinSettings::default()
        }
    }

    /// Manual folders map onto the files sd.cpp needs: the encoder's index, the VAE weights.
    #[test]
    fn manual_folders_map_onto_their_files() {
        let scratch = Scratch::new("manual");
        let index = scratch.file("te/model.safetensors.index.json");
        let dit = scratch.file("flux-2-klein-4b.safetensors");
        let vae = scratch.file("vae/diffusion_pytorch_model.safetensors");
        let files = flux2_sdcpp_map_files(&manual(&scratch.0.join("te"), &dit, &scratch.0.join("vae")), &scratch.0).expect("all three found");
        assert_eq!(files.family, ModelFamily::Flux2Klein);
        assert_eq!(files.components.get(&ComponentRole::Llm), Some(&index));
        assert_eq!(files.components.get(&ComponentRole::Diffusion), Some(&dit));
        assert_eq!(files.components.get(&ComponentRole::Vae), Some(&vae));
    }

    /// An encoder folder without an index falls back to its single `model.safetensors`; files
    /// picked directly are used as they are.
    #[test]
    fn manual_files_are_used_as_picked() {
        let scratch = Scratch::new("files");
        let single = scratch.file("te/model.safetensors");
        let dit = scratch.file("dit.gguf");
        let vae = scratch.file("ae.safetensors");
        let files = flux2_sdcpp_map_files(&manual(&scratch.0.join("te"), &dit, &vae), &scratch.0).expect("found");
        assert_eq!(files.components.get(&ComponentRole::Llm), Some(&single));
        let gguf = scratch.file("qwen3.gguf");
        let files = flux2_sdcpp_map_files(&manual(&gguf, &dit, &vae), &scratch.0).expect("found");
        assert_eq!(files.components.get(&ComponentRole::Llm), Some(&gguf));
    }

    /// A transformer folder (diffusers) is refused with its own hint, before any missing file.
    #[test]
    fn a_diffusers_transformer_folder_is_refused() {
        let scratch = Scratch::new("diffusers");
        fs::create_dir_all(scratch.0.join("transformer")).expect("dir");
        let settings = manual(&scratch.0.join("absent_te"), &scratch.0.join("transformer"), &scratch.0.join("absent_vae"));
        assert_eq!(flux2_sdcpp_map_files(&settings, &scratch.0), Err(Flux2SdcppFilesError::DiffusersTransformer));
    }

    /// Empty rows and missing files are named; manual mode says so.
    #[test]
    fn manual_rows_must_be_filled_and_exist() {
        let scratch = Scratch::new("missing");
        let empty = Flux2KleinSettings { source_mode: Flux2SourceMode::Manual.wire().to_string(), ..Flux2KleinSettings::default() };
        assert_eq!(flux2_sdcpp_map_files(&empty, &scratch.0), Err(Flux2SdcppFilesError::PathsRequired));
        let dit = scratch.file("dit.safetensors");
        let settings = manual(&scratch.0.join("te"), &dit, &scratch.0.join("vae"));
        assert_eq!(
            flux2_sdcpp_map_files(&settings, &scratch.0),
            Err(Flux2SdcppFilesError::Missing { download: false, components: vec![Flux2SdcppComponent::TextEncoder, Flux2SdcppComponent::Vae] })
        );
    }

    /// Download mode uses the catalog's sd.cpp map under the given root; files not yet
    /// downloaded are reported as a download problem.
    #[test]
    fn download_mode_uses_the_catalog_map() {
        let scratch = Scratch::new("download");
        let settings = Flux2KleinSettings {
            variant: Flux2Variant::Klein4B.wire().to_string(),
            source_mode: Flux2SourceMode::Download.wire().to_string(),
            transformer_format: Flux2WeightFormat::Q4KM.wire().to_string(),
            text_encoder_format: Flux2WeightFormat::Q4KM.wire().to_string(),
            ..Flux2KleinSettings::default()
        };
        let selection = settings.weight_selection();
        let expected = flux2_klein_sdcpp_paths(&scratch.0, Flux2Variant::Klein4B, selection.transformer, selection.source, selection.text_encoder).expect("offered formats have sd.cpp files");
        assert_eq!(
            flux2_sdcpp_map_files(&settings, &scratch.0),
            Err(Flux2SdcppFilesError::Missing { download: true, components: vec![Flux2SdcppComponent::TextEncoder, Flux2SdcppComponent::Transformer, Flux2SdcppComponent::Vae] })
        );
        for path in [&expected.diffusion, &expected.vae, &expected.llm] {
            let relative = path.strip_prefix(&scratch.0).expect("under the root");
            scratch.file(&relative.to_string_lossy());
        }
        let files = flux2_sdcpp_map_files(&settings, &scratch.0).expect("downloaded");
        assert_eq!(files.components.get(&ComponentRole::Diffusion), Some(&expected.diffusion));
        assert_eq!(files.components.get(&ComponentRole::Vae), Some(&expected.vae));
        assert_eq!(files.components.get(&ComponentRole::Llm), Some(&expected.llm));
    }

    /// Existing files that are not a FLUX.2 klein model are refused by the validation, with
    /// its own text.
    #[test]
    fn files_that_are_no_model_are_invalid() {
        let scratch = Scratch::new("invalid");
        let te = scratch.file("te.gguf");
        let dit = scratch.file("dit.gguf");
        let vae = scratch.file("vae.safetensors");
        assert!(matches!(flux2_sdcpp_model_files(&manual(&te, &dit, &vae), &scratch.0), Err(Flux2SdcppFilesError::Invalid(_))));
    }

    /// The key follows the facts the map reads and nothing else.
    #[test]
    fn the_key_follows_the_paths_and_the_selection() {
        let base = Flux2KleinSettings::default();
        let mut moved = base.clone();
        moved.transformer_path = "/m/dit.gguf".to_string();
        assert_ne!(Flux2SdcppFilesKey::of(&base), Flux2SdcppFilesKey::of(&moved));
        let mut prompt_only = base.clone();
        prompt_only.prompt = "another".to_string();
        assert_eq!(Flux2SdcppFilesKey::of(&base), Flux2SdcppFilesKey::of(&prompt_only));
    }
}
