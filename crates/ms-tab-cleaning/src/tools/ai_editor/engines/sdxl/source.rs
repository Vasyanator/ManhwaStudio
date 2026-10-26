/*
File: cleaning/tools/ai_editor/engines/sdxl/source.rs

Purpose:
What the weights field may name and which app-managed downloads a run of it needs. The one
owner of the SDXL model-source rule on the Rust side: the run gate (`engine.rs`), the
download block, the field's one-click choice and the worker's re-check (`wire.rs`) all ask
here, so they can never disagree about whether a value is accepted.

Main responsibilities:
- classify the weights field TEXTUALLY (`classify_model_path`): the curated repository id,
  a local single-file checkpoint, a local diffusers folder — or a refusal;
- name the pinned download each source needs (`required_download`): the channel mode's
  diffusers config spec for a single file, the curated fp16 model for the curated id,
  nothing for a folder;
- decide the GUI-thread half of the run gate from the download controller's state
  (`download_block_reason`).

Key items:
- `SdxlModelSource`, `SdxlSourceRefusal`, `curated_repo_id()`, `config_spec_for()`

Notes:
The classification never touches the filesystem, because the run gate is asked on the GUI
thread every frame. A path's existence is the backend's to check (it names the missing path
in its own error). Only ABSOLUTE paths (or `~`-prefixed ones, which the backend expands) are
local: a relative path would resolve against the backend's working directory, which is not
anything the user chose, and is textually indistinguishable from a Hugging Face repository id
— which the backend refuses (protocol v12) except the one curated id, so the refusal is made
here, before anything travels.

The curated repository id is not spelled here: it is read from the pinned catalog spec
`SDXL_INPAINT_FP16` (`ms_sysprobe::ai_models::external_catalog`, the one owner of that
value), so this side and the download agree by construction. The backend keeps its own
mirror (`CURATED_INPAINT_REPO_ID` in `modules/ai_backend/inpaint/sdxl.py`) under
`PROTOCOL_VERSION`.
*/

use super::*;

/// Single-file checkpoint extensions the backend loads with `from_single_file` (its
/// `MODEL_SUFFIXES`), compared case-insensitively.
const SINGLE_FILE_EXTENSIONS: [&str; 2] = ["safetensors", "ckpt"];

/// The download bundle of the 9-channel config spec (`SDXL_CONFIG_INPAINT`).
const CONFIG_INPAINT_BUNDLE: [&ExternalModelSpec; 1] = [&SDXL_CONFIG_INPAINT];
/// The download bundle of the 4-channel config spec (`SDXL_CONFIG_BASE`).
const CONFIG_BASE_BUNDLE: [&ExternalModelSpec; 1] = [&SDXL_CONFIG_BASE];
/// The download bundle of the curated inpainting model (`SDXL_INPAINT_FP16`).
const CURATED_MODEL_BUNDLE: [&ExternalModelSpec; 1] = [&SDXL_INPAINT_FP16];

/// What a valid weights field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SdxlModelSource {
    /// A local `.safetensors` / `.ckpt` file: loaded against the mode's pinned config spec.
    SingleFile,
    /// A local diffusers folder (with `model_index.json`): loaded as it is.
    Folder,
    /// The one curated repository id: loaded from its pinned fp16 download.
    Curated,
}

/// Why a weights field cannot run, before any download state is consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SdxlSourceRefusal {
    /// Nothing entered.
    Empty,
    /// Neither an absolute local path nor the curated id (a repository id, a relative path).
    NotLocal(String),
    /// The curated id in the 4-channel mode: it is a 9-channel inpainting model, which the
    /// backend's 4-channel pipeline refuses only after loading all of it.
    CuratedNeedsNineChannel,
}

impl SdxlSourceRefusal {
    /// The localized message the run gate and the worker show.
    #[must_use]
    pub(super) fn message(&self) -> String {
        match self {
            SdxlSourceRefusal::Empty => t!("cleaning.tools.sdxl.weights_path_required_error").to_string(),
            SdxlSourceRefusal::NotLocal(value) => tf!(
                "cleaning.tools.sdxl.weights_path_not_local_error",
                value = value,
                model = external_model_display_name(&SDXL_INPAINT_FP16)
            ),
            SdxlSourceRefusal::CuratedNeedsNineChannel => tf!(
                "cleaning.tools.sdxl.curated_model_needs_9ch_error",
                model = external_model_display_name(&SDXL_INPAINT_FP16),
                mode = SdxlMode::NineChannel.display_name()
            ),
        }
    }
}

/// The curated repository id, read from the pinned catalog spec; `None` only if the catalog
/// stopped sourcing that spec from Hugging Face (then no field value is curated).
#[must_use]
pub(super) fn curated_repo_id() -> Option<&'static str> {
    match SDXL_INPAINT_FP16.source {
        ExternalSource::HuggingFace { repo_id, .. } => Some(repo_id),
        ExternalSource::Url { .. } => None,
    }
}

/// Classifies the weights field of a run in `mode`. Pure text, never a filesystem probe (see
/// the file header for why relative paths are refused).
///
/// # Errors
/// [`SdxlSourceRefusal`] for an empty field, for anything that is neither an absolute local
/// path nor the curated id, and for the curated id outside the 9-channel mode.
pub(super) fn classify_model_path(mode: SdxlMode, model_path: &str) -> Result<SdxlModelSource, SdxlSourceRefusal> {
    let value = model_path.trim();
    if value.is_empty() {
        return Err(SdxlSourceRefusal::Empty);
    }
    if curated_repo_id() == Some(value) {
        return match mode {
            SdxlMode::NineChannel => Ok(SdxlModelSource::Curated),
            SdxlMode::FourChannel => Err(SdxlSourceRefusal::CuratedNeedsNineChannel),
        };
    }
    let path = Path::new(value);
    // `~` is expanded by the backend (`Path.expanduser`), so it names a local path too.
    if !(path.is_absolute() || value.starts_with('~')) {
        return Err(SdxlSourceRefusal::NotLocal(value.to_string()));
    }
    let single_file = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|ext| SINGLE_FILE_EXTENSIONS.iter().any(|known| ext.eq_ignore_ascii_case(known)));
    Ok(if single_file { SdxlModelSource::SingleFile } else { SdxlModelSource::Folder })
}

/// The pinned diffusers config spec of `mode`, the `sdxl_config_dir` a single-file checkpoint
/// is loaded against: inpainting 0.1 for the 9-channel UNet, base 1.0 for the 4-channel one.
#[must_use]
pub(super) fn config_spec_for(mode: SdxlMode) -> &'static ExternalModelSpec {
    match mode {
        SdxlMode::NineChannel => &SDXL_CONFIG_INPAINT,
        SdxlMode::FourChannel => &SDXL_CONFIG_BASE,
    }
}

/// The download bundle a run of `source` in `mode` needs installed; empty for a folder.
#[must_use]
pub(super) fn required_download(mode: SdxlMode, source: SdxlModelSource) -> &'static [&'static ExternalModelSpec] {
    match (source, mode) {
        (SdxlModelSource::SingleFile, SdxlMode::NineChannel) => &CONFIG_INPAINT_BUNDLE,
        (SdxlModelSource::SingleFile, SdxlMode::FourChannel) => &CONFIG_BASE_BUNDLE,
        (SdxlModelSource::Curated, _) => &CURATED_MODEL_BUNDLE,
        (SdxlModelSource::Folder, _) => &[],
    }
}

/// The download bundle the current field needs, or empty when it needs none or is refused
/// (a refused field gets no download block: there is nothing it could download).
#[must_use]
pub(super) fn required_download_for(mode: SdxlMode, model_path: &str) -> &'static [&'static ExternalModelSpec] {
    classify_model_path(mode, model_path).map_or(&[], |source| required_download(mode, source))
}

/// The localized "not downloaded, press {button}" refusal for `spec`, naming the exact caption
/// the download block draws (`button`) and the panel that block sits in (by its own label).
#[must_use]
pub(super) fn not_downloaded_message(spec: &ExternalModelSpec, button: &str) -> String {
    tf!(
        "cleaning.tools.sdxl.model_not_downloaded_error",
        model = external_model_display_name(spec),
        button = button,
        panel = t!("cleaning.tab.area_editor_tab")
    )
}

/// The GUI-thread download half of the run gate for the bundle `specs`, from its controller
/// `snapshot`: `None` when installed (or nothing is needed). Not installed names the button the
/// block offers; checking / downloading / verifying / an unavailable probe reuse the shared wording.
#[must_use]
pub(super) fn download_block_reason(specs: &[&'static ExternalModelSpec], snapshot: &OcrModelPanelSnapshot) -> Option<String> {
    let first = specs.first()?;
    match &snapshot.state {
        OcrModelDownloadState::Missing | OcrModelDownloadState::Partial { .. } => {
            let button = offered_download_label(snapshot).unwrap_or_default();
            Some(not_downloaded_message(first, &button))
        }
        OcrModelDownloadState::Installed
        | OcrModelDownloadState::Unknown
        | OcrModelDownloadState::Checking
        | OcrModelDownloadState::Downloading(_)
        | OcrModelDownloadState::Verifying(_)
        | OcrModelDownloadState::Unavailable(_) => load_blocked_reason(&bundle_key(specs), Some(snapshot)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute path for the host platform: `is_absolute` is platform-defined.
    fn absolute(tail: &str) -> String {
        if cfg!(windows) { format!("C:\\models\\{tail}") } else { format!("/models/{tail}") }
    }

    fn snapshot(specs: &[&'static ExternalModelSpec], state: OcrModelDownloadState) -> OcrModelPanelSnapshot {
        OcrModelPanelSnapshot {
            bundle_key: bundle_key(specs),
            state,
            missing_bytes: specs.iter().map(|spec| spec.total_bytes()).sum(),
            notice: None,
            other_download: None,
            access: Vec::new(),
        }
    }

    /// The curated id comes from the pinned catalog, and it is the one the backend accepts.
    #[test]
    fn the_curated_id_is_the_catalog_repository() {
        assert_eq!(curated_repo_id(), Some("diffusers/stable-diffusion-xl-1.0-inpainting-0.1"));
    }

    /// Every accepted field kind, both modes, and the trimmed spelling.
    #[test]
    fn the_field_is_classified_by_kind() {
        let curated = curated_repo_id().expect("a Hugging Face spec");
        assert_eq!(classify_model_path(SdxlMode::NineChannel, &format!("  {curated} ")), Ok(SdxlModelSource::Curated));
        for mode in [SdxlMode::NineChannel, SdxlMode::FourChannel] {
            assert_eq!(classify_model_path(mode, &absolute("anime.safetensors")), Ok(SdxlModelSource::SingleFile));
            assert_eq!(classify_model_path(mode, &absolute("anime.CKPT")), Ok(SdxlModelSource::SingleFile));
            assert_eq!(classify_model_path(mode, &absolute("sdxl-diffusers")), Ok(SdxlModelSource::Folder));
            assert_eq!(classify_model_path(mode, "~/models/sdxl.safetensors"), Ok(SdxlModelSource::SingleFile));
        }
    }

    /// Empty, any other repository id, a relative path, and the curated id in the 4-channel mode
    /// are refused before anything travels, each with its own message.
    #[test]
    fn the_field_refusals_are_typed_and_localized() {
        assert_eq!(classify_model_path(SdxlMode::NineChannel, "   "), Err(SdxlSourceRefusal::Empty));
        let repo = "stabilityai/stable-diffusion-xl-base-1.0";
        assert_eq!(classify_model_path(SdxlMode::FourChannel, repo), Err(SdxlSourceRefusal::NotLocal(repo.to_string())));
        assert_eq!(
            classify_model_path(SdxlMode::NineChannel, "models/sdxl.safetensors"),
            Err(SdxlSourceRefusal::NotLocal("models/sdxl.safetensors".to_string()))
        );
        let curated = curated_repo_id().expect("a Hugging Face spec");
        assert_eq!(classify_model_path(SdxlMode::FourChannel, curated), Err(SdxlSourceRefusal::CuratedNeedsNineChannel));
        for refusal in [
            SdxlSourceRefusal::Empty,
            SdxlSourceRefusal::NotLocal(repo.to_string()),
            SdxlSourceRefusal::CuratedNeedsNineChannel,
        ] {
            assert!(!refusal.message().is_empty(), "{refusal:?}");
        }
    }

    /// A single file needs the MODE's config spec, the curated id its fp16 download, a folder
    /// nothing; a refused field needs no download either.
    #[test]
    fn each_source_names_its_required_download() {
        let ids = |specs: &[&'static ExternalModelSpec]| specs.iter().map(|spec| spec.id).collect::<Vec<_>>();
        assert_eq!(ids(required_download(SdxlMode::NineChannel, SdxlModelSource::SingleFile)), vec![SDXL_CONFIG_INPAINT.id]);
        assert_eq!(ids(required_download(SdxlMode::FourChannel, SdxlModelSource::SingleFile)), vec![SDXL_CONFIG_BASE.id]);
        assert_eq!(ids(required_download(SdxlMode::NineChannel, SdxlModelSource::Curated)), vec![SDXL_INPAINT_FP16.id]);
        assert!(required_download(SdxlMode::NineChannel, SdxlModelSource::Folder).is_empty());
        assert!(required_download_for(SdxlMode::FourChannel, "owner/repo").is_empty());
        assert_eq!(config_spec_for(SdxlMode::NineChannel).id, SDXL_CONFIG_INPAINT.id);
        assert_eq!(config_spec_for(SdxlMode::FourChannel).id, SDXL_CONFIG_BASE.id);
    }

    /// The download half of the gate: installed passes, not installed names the offered button,
    /// and every in-between state still refuses.
    #[test]
    fn the_download_gate_follows_the_bundle_state() {
        let specs = required_download(SdxlMode::NineChannel, SdxlModelSource::SingleFile);
        assert_eq!(download_block_reason(specs, &snapshot(specs, OcrModelDownloadState::Installed)), None);

        let missing = snapshot(specs, OcrModelDownloadState::Missing);
        let button = offered_download_label(&missing).expect("a missing bundle offers a download");
        assert_eq!(download_block_reason(specs, &missing), Some(not_downloaded_message(specs[0], &button)));

        for state in [OcrModelDownloadState::Unknown, OcrModelDownloadState::Checking, OcrModelDownloadState::Unavailable("x".to_string())] {
            assert!(download_block_reason(specs, &snapshot(specs, state.clone())).is_some(), "{state:?}");
        }
        assert_eq!(download_block_reason(&[], &missing), None, "nothing required, nothing to wait for");
    }
}
