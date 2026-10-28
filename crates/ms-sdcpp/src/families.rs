/*
File: crates/ms-sdcpp/src/families.rs

Purpose:
The one owner of "model family -> sd.cpp context": which component roles a family needs,
which `sd_ctx_params_t` path field each role fills, how its text conditioning is built (taps
x encoder hidden size, accepted encoder architectures), and the pre-flight validation of a
`ModelFiles` set, so a wrong file is a clean typed error instead of an abort inside the
library (a mis-paired encoder makes sd.cpp hit a `GGML_ASSERT`).

Key items:
- ModelFamily::required_roles / text_conditioning() / TextConditioning
- ctx_paths() / CtxPaths
- validate_model_files() / ModelFilesError (localized `Display`, `sdcpp.files.*`)
- ComponentRole::label_key

Notes:
- Facts measured on the real files (2026-10-28): the DiT text width is the input width of
  `txt_in.weight` (GGUF ne0, safetensors shape[1]) and equals taps x encoder hidden size:
  FLUX.2 klein 4B 7680 = 3 x 2560 (Qwen3-4B), 9B 12288 = 3 x 4096 (Qwen3-8B), FLUX.3 klein
  20480 = 8 x 2560 (Qwen3-VL-4B). Only FLUX.3 has `txt_mode_blocks.*` tensors. A diffusers
  `transformer/` (tensor `context_embedder.weight`, no `txt_in.weight`) is detected by
  sd.cpp as "Flux Control" and fails, so it is `UnsupportedLayout`.
- Encoder architecture: GGUF `general.architecture` (`qwen3`, `qwen3vl`) and
  `<arch>.embedding_length`; a safetensors encoder is described by the `config.json` next to
  it (`model_type` `qwen3` / `qwen3_vl`, `text_config.hidden_size` before `hidden_size`).
- Reads headers only (bounded, `model_probe`); blocking file I/O: worker thread only.
*/

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::model_probe::{self, ModelHeader};
use crate::request::{ComponentRole, ModelFamily, ModelFiles};

/// File name of a sharded Hugging Face checkpoint's index.
pub const SAFETENSORS_INDEX: &str = "model.safetensors.index.json";
/// The diffusion model tensor whose input width is the text-conditioning width.
const TXT_IN: &str = "txt_in.weight";
/// The tensor that marks a diffusers-layout Flux transformer.
const DIFFUSERS_CONTEXT_EMBEDDER: &str = "context_embedder.weight";
/// Tensor prefix present only in FLUX.3 klein diffusion models.
const FLUX3_MARKER_PREFIX: &str = "txt_mode_blocks.";

impl ComponentRole {
    /// The i18n key of the role's display name.
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Diffusion => "sdcpp.component.diffusion_label",
            Self::Vae => "sdcpp.component.vae_label",
            Self::Llm => "sdcpp.component.llm_label",
        }
    }

    /// The localized display name.
    #[must_use]
    pub fn label(self) -> &'static str {
        ms_i18n::resolve_key(self.label_key())
    }
}

impl ModelFamily {
    /// The roles a [`ModelFiles`] of this family must provide.
    #[must_use]
    pub const fn required_roles(self) -> &'static [ComponentRole] {
        match self {
            Self::Flux2Klein | Self::Flux3Klein => &[ComponentRole::Diffusion, ComponentRole::Vae, ComponentRole::Llm],
        }
    }

    /// The i18n key of the family's display name.
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Flux2Klein => "sdcpp.family.flux2_klein_label",
            Self::Flux3Klein => "sdcpp.family.flux3_klein_label",
        }
    }

    /// The localized display name.
    #[must_use]
    pub fn label(self) -> &'static str {
        ms_i18n::resolve_key(self.label_key())
    }
}

/// How a family builds its text conditioning from an LLM encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextConditioning {
    /// How many encoder hidden states are concatenated; the diffusion model text width is
    /// `taps * encoder hidden size`.
    pub taps: u32,
    /// Accepted encoder architectures (GGUF `general.architecture` spelling).
    pub encoder_archs: &'static [&'static str],
}

/// The text conditioning of `family`, `None` for a family without an LLM encoder.
#[must_use]
pub const fn text_conditioning(family: ModelFamily) -> Option<TextConditioning> {
    match family {
        ModelFamily::Flux2Klein => Some(TextConditioning { taps: 3, encoder_archs: &["qwen3"] }),
        ModelFamily::Flux3Klein => Some(TextConditioning { taps: 8, encoder_archs: &["qwen3vl"] }),
    }
}

/// The `sd_ctx_params_t` path fields a [`ModelFiles`] fills (others stay unset).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CtxPaths {
    /// `diffusion_model_path`.
    pub diffusion_model: Option<PathBuf>,
    /// `vae_path`.
    pub vae: Option<PathBuf>,
    /// `llm_path`.
    pub llm: Option<PathBuf>,
}

/// Maps the (validated) files onto the context path fields of their family.
#[must_use]
pub fn ctx_paths(files: &ModelFiles) -> CtxPaths {
    let path = |role| files.components.get(&role).cloned();
    match files.family {
        ModelFamily::Flux2Klein | ModelFamily::Flux3Klein => CtxPaths { diffusion_model: path(ComponentRole::Diffusion), vae: path(ComponentRole::Vae), llm: path(ComponentRole::Llm) },
    }
}

/// Why a model file set cannot run under sd.cpp. `Display` is localized.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelFilesError {
    /// No file was given for `role`, or `path` does not exist.
    #[error("{}", tf!("sdcpp.files.missing_error", role = role.label(), path = path.display()))]
    Missing { role: ComponentRole, path: PathBuf },
    /// `path` is a directory or another non-file where a file is needed.
    #[error("{}", tf!("sdcpp.files.not_a_file_error", role = role.label(), path = path.display()))]
    NotAFile { role: ComponentRole, path: PathBuf },
    /// `path` has a layout sd.cpp cannot load (a diffusers transformer, an unknown diffusion model).
    #[error("{}", tf!("sdcpp.files.unsupported_layout_error", role = role.label(), path = path.display()))]
    UnsupportedLayout { role: ComponentRole, path: PathBuf },
    /// The header of `path` could not be read (`detail`: English diagnostic).
    #[error("{}", tf!("sdcpp.files.unreadable_error", role = role.label(), path = path.display(), detail = detail))]
    Unreadable { role: ComponentRole, path: PathBuf, detail: String },
    /// The diffusion model belongs to another family.
    #[error("{}", tf!("sdcpp.files.wrong_family_error", expected = expected.label(), found = found.label()))]
    WrongFamily { expected: ModelFamily, found: ModelFamily },
    /// The text encoder's architecture is not one the family accepts.
    #[error("{}", tf!("sdcpp.files.wrong_encoder_arch_error", expected = expected, found = found))]
    WrongEncoderArch { expected: String, found: String },
    /// The text encoder's width does not match the diffusion model (`taps * hidden`).
    #[error("{}", tf!("sdcpp.files.encoder_mismatch_error", expected_width = expected_width, found_width = found_width))]
    EncoderMismatch { expected_width: u64, found_width: u64 },
}

/// Validates `files` for sd.cpp and returns them with every path resolved to the file sd.cpp
/// opens (an encoder directory holding `model.safetensors.index.json` becomes that index).
///
/// Checks, in order: every required role is given and exists; each role is a file (an encoder
/// directory only with an index); the diffusion model is a BFL-layout Flux diffusion model of this family (diffusers
/// layout -> `UnsupportedLayout`); the encoder architecture is one the family accepts; the
/// diffusion model text width equals `taps * encoder hidden size`. Reads headers only.
///
/// # Errors
/// The first [`ModelFilesError`] found.
///
/// # Threading
/// Blocking file I/O: worker thread only.
pub fn validate_model_files(files: &ModelFiles) -> Result<ModelFiles, ModelFilesError> {
    let mut resolved = BTreeMap::new();
    for role in files.family.required_roles() {
        let path = files.components.get(role).ok_or_else(|| ModelFilesError::Missing { role: *role, path: PathBuf::new() })?;
        resolved.insert(*role, resolve_path(*role, path)?);
    }
    let dit_path = &resolved[&ComponentRole::Diffusion];
    let dit = read_dit_header(dit_path)?;
    let found_family = if dit.has_prefix(FLUX3_MARKER_PREFIX) { ModelFamily::Flux3Klein } else { ModelFamily::Flux2Klein };
    if found_family != files.family {
        return Err(ModelFilesError::WrongFamily { expected: files.family, found: found_family });
    }
    if let Some(conditioning) = text_conditioning(files.family) {
        let dit_width = dit.input_width(TXT_IN).ok_or_else(|| ModelFilesError::UnsupportedLayout { role: ComponentRole::Diffusion, path: dit_path.clone() })?;
        let llm_path = &resolved[&ComponentRole::Llm];
        let (arch, hidden) = encoder_facts(llm_path)?;
        if !conditioning.encoder_archs.contains(&arch.as_str()) {
            return Err(ModelFilesError::WrongEncoderArch { expected: conditioning.encoder_archs.join(", "), found: arch });
        }
        let encoder_width = hidden.saturating_mul(u64::from(conditioning.taps));
        if encoder_width != dit_width {
            return Err(ModelFilesError::EncoderMismatch { expected_width: dit_width, found_width: encoder_width });
        }
    }
    Ok(ModelFiles { family: files.family, components: resolved })
}

/// Checks that `path` exists and is a file; an LLM directory with an index maps to it.
fn resolve_path(role: ComponentRole, path: &Path) -> Result<PathBuf, ModelFilesError> {
    let metadata = std::fs::metadata(path).map_err(|_| ModelFilesError::Missing { role, path: path.to_path_buf() })?;
    if metadata.is_file() {
        return Ok(path.to_path_buf());
    }
    if metadata.is_dir() {
        let index = path.join(SAFETENSORS_INDEX);
        if index.is_file() {
            return match role {
                ComponentRole::Llm => Ok(index),
                // A sharded DiT / VAE directory is a diffusers layout sd.cpp cannot load.
                ComponentRole::Diffusion | ComponentRole::Vae => Err(ModelFilesError::UnsupportedLayout { role, path: path.to_path_buf() }),
            };
        }
        if role == ComponentRole::Diffusion && path.join("diffusion_pytorch_model.safetensors").is_file() {
            return Err(ModelFilesError::UnsupportedLayout { role, path: path.to_path_buf() });
        }
    }
    Err(ModelFilesError::NotAFile { role, path: path.to_path_buf() })
}

/// Whether `path` names a GGUF file (by its magic, not its name).
fn is_gguf(path: &Path) -> Result<bool, std::io::Error> {
    use std::io::Read;
    let mut magic = [0_u8; 4];
    let mut file = File::open(path)?;
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(&magic == b"GGUF"),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(err) => Err(err),
    }
}

/// Reads the header of a single model file (GGUF or safetensors).
fn read_file_header(role: ComponentRole, path: &Path) -> Result<ModelHeader, ModelFilesError> {
    let unreadable = |detail: String| ModelFilesError::Unreadable { role, path: path.to_path_buf(), detail };
    let gguf = is_gguf(path).map_err(|err| unreadable(err.to_string()))?;
    let file = File::open(path).map_err(|err| unreadable(err.to_string()))?;
    let mut reader = BufReader::new(file);
    let header = if gguf { model_probe::read_gguf(&mut reader) } else { model_probe::read_safetensors_header(&mut reader) };
    header.map_err(|err| unreadable(err.to_string()))
}

/// Reads the diffusion model header; a sharded index resolves to the shard holding `txt_in.weight`.
/// A diffusers layout (no `txt_in.weight`, a `context_embedder.weight`) is refused.
fn read_dit_header(path: &Path) -> Result<ModelHeader, ModelFilesError> {
    let role = ComponentRole::Diffusion;
    let header = if path.file_name().is_some_and(|name| name == SAFETENSORS_INDEX) {
        let map = read_index(role, path)?;
        if map.contains_key(DIFFUSERS_CONTEXT_EMBEDDER) || !map.contains_key(TXT_IN) {
            return Err(ModelFilesError::UnsupportedLayout { role, path: path.to_path_buf() });
        }
        let shard = path.with_file_name(&map[TXT_IN]);
        read_file_header(role, &shard)?
    } else {
        read_file_header(role, path)?
    };
    if header.tensors.contains_key(DIFFUSERS_CONTEXT_EMBEDDER) || !header.tensors.contains_key(TXT_IN) {
        return Err(ModelFilesError::UnsupportedLayout { role, path: path.to_path_buf() });
    }
    Ok(header)
}

/// Reads an index `weight_map`.
fn read_index(role: ComponentRole, path: &Path) -> Result<BTreeMap<String, String>, ModelFilesError> {
    let unreadable = |detail: String| ModelFilesError::Unreadable { role, path: path.to_path_buf(), detail };
    let file = File::open(path).map_err(|err| unreadable(err.to_string()))?;
    model_probe::read_index_weight_map(&mut BufReader::new(file)).map_err(|err| unreadable(err.to_string()))
}

/// The encoder's architecture (GGUF spelling) and hidden size: GGUF metadata, or the
/// `config.json` next to a safetensors file / index.
fn encoder_facts(path: &Path) -> Result<(String, u64), ModelFilesError> {
    let role = ComponentRole::Llm;
    let unreadable = |detail: String| ModelFilesError::Unreadable { role, path: path.to_path_buf(), detail };
    if is_gguf(path).map_err(|err| unreadable(err.to_string()))? {
        let header = read_file_header(role, path)?;
        let arch = header.meta_str("general.architecture").ok_or_else(|| unreadable("no general.architecture".to_owned()))?.to_owned();
        let hidden = header.meta_u64(&format!("{arch}.embedding_length")).ok_or_else(|| unreadable(format!("no {arch}.embedding_length")))?;
        return Ok((arch, hidden));
    }
    let config_path = path.with_file_name("config.json");
    let file = File::open(&config_path).map_err(|err| unreadable(format!("{}: {err}", config_path.display())))?;
    let config = model_probe::read_hf_text_config(&mut BufReader::new(file)).map_err(|err| unreadable(format!("{}: {err}", config_path.display())))?;
    let model_type = config.model_type.ok_or_else(|| unreadable("config.json has no model_type".to_owned()))?;
    let hidden = config.hidden_size.ok_or_else(|| unreadable("config.json has no hidden_size".to_owned()))?;
    // Hugging Face spells `qwen3_vl` where GGUF spells `qwen3vl`.
    Ok((model_type.replace('_', ""), hidden))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_probe::fixtures::{gguf, safetensors};

    /// A scratch directory with helpers to write fixture files.
    struct Scratch(tempfile::TempDir);

    impl Scratch {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("temp dir"))
        }

        fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&path, bytes).expect("write fixture");
            path
        }
    }

    /// A GGUF Flux diffusion model with text width `width` (FLUX.3 when `flux3`); dims in ggml order
    /// (ne0 = input width, ne1 = output 3072).
    fn dit_gguf(width: u64, flux3: bool) -> Vec<u8> {
        let txt_in = [width, 3072];
        let mut tensors: Vec<(&str, &[u64])> = vec![("txt_in.weight", &txt_in), ("double_blocks.0.img_attn.qkv.weight", &[3072, 9216])];
        if flux3 {
            tensors.push(("txt_mode_blocks.0.attn.weight", &[3072, 3072]));
        }
        gguf(&[("general.architecture", "flux")], &[], &tensors)
    }

    fn encoder_gguf(arch: &str, hidden: u32) -> Vec<u8> {
        let key = format!("{arch}.embedding_length");
        gguf(&[("general.architecture", arch)], &[(key.as_str(), hidden)], &[("token_embd.weight", &[u64::from(hidden), 151_936])])
    }

    fn files(family: ModelFamily, dit: &Path, vae: &Path, llm: &Path) -> ModelFiles {
        let components = BTreeMap::from([(ComponentRole::Diffusion, dit.to_path_buf()), (ComponentRole::Vae, vae.to_path_buf()), (ComponentRole::Llm, llm.to_path_buf())]);
        ModelFiles { family, components }
    }

    #[test]
    fn matching_pairs_validate_for_every_measured_model() {
        let scratch = Scratch::new();
        let vae = scratch.write("vae.safetensors", &safetensors(&[("decoder.conv_in.bias", &[512])]));
        let cases = [(ModelFamily::Flux2Klein, 7680, false, "qwen3", 2560), (ModelFamily::Flux2Klein, 12288, false, "qwen3", 4096), (ModelFamily::Flux3Klein, 20480, true, "qwen3vl", 2560)];
        for (index, (family, width, flux3, arch, hidden)) in cases.into_iter().enumerate() {
            let dit = scratch.write(&format!("dit{index}.gguf"), &dit_gguf(width, flux3));
            let llm = scratch.write(&format!("te{index}.gguf"), &encoder_gguf(arch, hidden));
            let resolved = validate_model_files(&files(family, &dit, &vae, &llm)).unwrap_or_else(|err| panic!("{family:?}: {err:?}"));
            assert_eq!(ctx_paths(&resolved), CtxPaths { diffusion_model: Some(dit), vae: Some(vae.clone()), llm: Some(llm) });
        }
    }

    #[test]
    fn mismatched_encoder_and_family_are_refused() {
        let scratch = Scratch::new();
        let vae = scratch.write("vae.safetensors", &safetensors(&[("decoder.conv_in.bias", &[512])]));
        let dit_4b = scratch.write("4b.gguf", &dit_gguf(7680, false));
        let qwen3_8b = scratch.write("8b.gguf", &encoder_gguf("qwen3", 4096));
        assert_eq!(validate_model_files(&files(ModelFamily::Flux2Klein, &dit_4b, &vae, &qwen3_8b)), Err(ModelFilesError::EncoderMismatch { expected_width: 7680, found_width: 12288 }));

        let dit_f3 = scratch.write("f3.gguf", &dit_gguf(20480, true));
        let qwen3_4b = scratch.write("4b_te.gguf", &encoder_gguf("qwen3", 2560));
        assert_eq!(
            validate_model_files(&files(ModelFamily::Flux3Klein, &dit_f3, &vae, &qwen3_4b)),
            Err(ModelFilesError::WrongEncoderArch { expected: "qwen3vl".into(), found: "qwen3".into() })
        );
        assert_eq!(
            validate_model_files(&files(ModelFamily::Flux2Klein, &dit_f3, &vae, &qwen3_4b)),
            Err(ModelFilesError::WrongFamily { expected: ModelFamily::Flux2Klein, found: ModelFamily::Flux3Klein })
        );
    }

    #[test]
    fn diffusers_transformer_is_unsupported() {
        let scratch = Scratch::new();
        let vae = scratch.write("vae.safetensors", &safetensors(&[("decoder.conv_in.bias", &[512])]));
        let llm = scratch.write("te.gguf", &encoder_gguf("qwen3", 2560));
        let diffusers = scratch.write("transformer/diffusion_pytorch_model.safetensors", &safetensors(&[("context_embedder.weight", &[3072, 7680])]));
        assert!(matches!(validate_model_files(&files(ModelFamily::Flux2Klein, &diffusers, &vae, &llm)), Err(ModelFilesError::UnsupportedLayout { role: ComponentRole::Diffusion, .. })));
        let dir = diffusers.parent().expect("transformer dir").to_path_buf();
        assert!(matches!(validate_model_files(&files(ModelFamily::Flux2Klein, &dir, &vae, &llm)), Err(ModelFilesError::UnsupportedLayout { role: ComponentRole::Diffusion, .. })));
    }

    #[test]
    fn safetensors_dit_with_sharded_encoder_directory_and_config() {
        let scratch = Scratch::new();
        let vae = scratch.write("vae.safetensors", &safetensors(&[("decoder.conv_in.bias", &[512])]));
        // BFL single file: shape [out, in].
        let dit = scratch.write("flux-2-klein-9b.safetensors", &safetensors(&[("txt_in.weight", &[4096, 12288])]));
        scratch.write("text_encoder/config.json", br#"{"model_type":"qwen3","hidden_size":4096}"#);
        scratch.write("text_encoder/model.safetensors.index.json", br#"{"weight_map":{"model.embed_tokens.weight":"model-00001-of-00002.safetensors"}}"#);
        let encoder_dir = scratch.0.path().join("text_encoder");
        let resolved = validate_model_files(&files(ModelFamily::Flux2Klein, &dit, &vae, &encoder_dir)).expect("valid");
        assert_eq!(resolved.components[&ComponentRole::Llm], encoder_dir.join(SAFETENSORS_INDEX));

        // A Qwen3-VL config nests the text hidden size.
        let dit3 = scratch.write("flux3.safetensors", &safetensors(&[("txt_in.weight", &[3072, 20480]), ("txt_mode_blocks.0.w", &[1])]));
        scratch.write("vl/config.json", br#"{"model_type":"qwen3_vl","text_config":{"hidden_size":2560},"vision_config":{"hidden_size":1024}}"#);
        let vl = scratch.write("vl/model.safetensors", &safetensors(&[("model.embed_tokens.weight", &[151_936, 2560])]));
        assert!(validate_model_files(&files(ModelFamily::Flux3Klein, &dit3, &vae, &vl)).is_ok());
    }

    #[test]
    fn missing_and_unreadable_files_are_typed() {
        let scratch = Scratch::new();
        let vae = scratch.write("vae.safetensors", &safetensors(&[]));
        let llm = scratch.write("te.gguf", &encoder_gguf("qwen3", 2560));
        let absent = scratch.0.path().join("absent.gguf");
        assert!(matches!(validate_model_files(&files(ModelFamily::Flux2Klein, &absent, &vae, &llm)), Err(ModelFilesError::Missing { role: ComponentRole::Diffusion, .. })));
        let truncated = scratch.write("trunc.gguf", &dit_gguf(7680, false)[..40]);
        assert!(matches!(validate_model_files(&files(ModelFamily::Flux2Klein, &truncated, &vae, &llm)), Err(ModelFilesError::Unreadable { role: ComponentRole::Diffusion, .. })));
        let empty_dir = scratch.0.path().join("empty");
        std::fs::create_dir_all(&empty_dir).expect("dir");
        let dit = scratch.write("dit.gguf", &dit_gguf(7680, false));
        assert!(matches!(validate_model_files(&files(ModelFamily::Flux2Klein, &dit, &empty_dir, &llm)), Err(ModelFilesError::NotAFile { role: ComponentRole::Vae, .. })));
        let mut no_vae = files(ModelFamily::Flux2Klein, &dit, &vae, &llm);
        no_vae.components.remove(&ComponentRole::Vae);
        assert!(matches!(validate_model_files(&no_vae), Err(ModelFilesError::Missing { role: ComponentRole::Vae, .. })));
    }

    #[test]
    fn every_files_error_displays_text() {
        let path = PathBuf::from("/m/x");
        let errors = [
            ModelFilesError::Missing { role: ComponentRole::Diffusion, path: path.clone() },
            ModelFilesError::NotAFile { role: ComponentRole::Vae, path: path.clone() },
            ModelFilesError::UnsupportedLayout { role: ComponentRole::Diffusion, path: path.clone() },
            ModelFilesError::Unreadable { role: ComponentRole::Llm, path, detail: "d".into() },
            ModelFilesError::WrongFamily { expected: ModelFamily::Flux2Klein, found: ModelFamily::Flux3Klein },
            ModelFilesError::WrongEncoderArch { expected: "qwen3".into(), found: "llama".into() },
            ModelFilesError::EncoderMismatch { expected_width: 7680, found_width: 12288 },
        ];
        for err in errors {
            assert_ne!(err.to_string().trim(), "", "{err:?}");
        }
    }
}
