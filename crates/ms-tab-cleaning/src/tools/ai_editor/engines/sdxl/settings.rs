/*
File: cleaning/tools/ai_editor/engines/sdxl/settings.rs

Purpose:
The channel mode, the per-mode generation parameters, the document persisted to
`config::sdxl_inpaint_settings_path()`. The save gate that decides when to write it is the
shared `region_edit_v2::engine_settings::settings_save_due`.

Main responsibilities:
- own `SdxlMode` and its wire spelling;
- own `SdxlSettings` — one full parameter set per mode — and the per-mode defaults;
- own `SdxlPersisted`, the on-disk document, and read/write it on worker threads;

Key structures:
- `SdxlMode`, `SdxlSettings`, `SdxlPersisted`, `SdxlRunConfig`

Key functions:
- `load_sdxl_settings()`, `save_sdxl_settings()`

Notes:
The FILE and every FIELD NAME are FIXED by on-disk compatibility: an existing
`sdxl_inpaint_settings.json` must keep loading, with no silent reset of anything a user had
set. Renaming a field here silently resets it for everyone who already has that file.

BOTH modes are persisted, not only the selected one — switching mode and back must not reset
the other mode's prompts, denoise or weights path. The mode itself is a persisted field, so
one file (rather than a file per mode) is what can record which mode to restore.

There is deliberately NO `normalized()` clamp here, unlike the LaMa engine's settings: the
persisted values go on the wire verbatim, two of the fields are free-form text (a weights
path and two prompts), and clamping on load would silently rewrite a value the user had put
in the file by hand. The panel's own ranges are what bound anything
entered through the UI.
*/

use super::*;

/// Channel mode of the SDXL checkpoint the run uses.
///
/// It decides three things at once: which pipeline the backend builds, which defaults a
/// fresh parameter set gets, and whether `lama_model` travels in the request at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SdxlMode {
    /// Dedicated 9-channel inpainting UNet: the clean masked image is its own conditioning
    /// channel, so a full denoise is the meaningful default and there is no LaMa prefill.
    NineChannel,
    /// Ordinary 4-channel SDXL checkpoint: the hole is prefilled by LaMa before generation,
    /// which is what makes a denoise below 1.0 work.
    FourChannel,
}

impl SdxlMode {
    /// The wire token sent as `mode` and stored in the settings file.
    ///
    /// A literal, never localized: it is the backend's own dispatch key
    /// (`dev-docs/i18n_exclusions.md` §A5).
    #[must_use]
    pub(super) fn wire(self) -> &'static str {
        match self {
            SdxlMode::NineChannel => "nine_channel",
            SdxlMode::FourChannel => "four_channel",
        }
    }

    /// Parses a persisted wire token. An unknown or misspelled value falls back to the
    /// 9-channel mode rather than failing the whole document.
    #[must_use]
    pub(super) fn from_wire(value: &str) -> Self {
        match value.trim() {
            "four_channel" => SdxlMode::FourChannel,
            "nine_channel" => SdxlMode::NineChannel,
            _ => SdxlMode::NineChannel,
        }
    }

    /// Localized caption of this mode in the picker.
    #[must_use]
    pub(super) fn display_name(self) -> &'static str {
        match self {
            SdxlMode::NineChannel => t!("cleaning.tools.sdxl.mode_9ch"),
            SdxlMode::FourChannel => t!("cleaning.tools.sdxl.mode_4ch"),
        }
    }
}

/// The full generation parameter set of ONE mode.
///
/// Persisted per mode, so the modes never overwrite each other's prompts or weights path.
/// `model_path` is a ckpt / safetensors path, a diffusers folder, or a Hugging Face repo id —
/// the backend decides which; `lama_model` is a LaMa-v2 catalog FILE NAME and is read only in
/// [`SdxlMode::FourChannel`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SdxlSettings {
    /// ckpt / safetensors path, diffusers folder, or a Hugging Face repo id.
    pub(super) model_path: String,
    pub(super) positive_prompt: String,
    pub(super) negative_prompt: String,
    pub(super) steps: u32,
    pub(super) cfg_scale: f32,
    /// Denoising strength. 9ch runs 1.0; 4ch runs below 1.0 over the LaMa prefill.
    pub(super) denoise_strength: f32,
    /// Random seed; `-1` means a fresh random seed on every run. The sentinel is the
    /// BACKEND's contract, which is why the field stays signed.
    pub(super) seed: i64,
    /// One of [`SDXL_SAMPLERS`]; a literal scheduler token, never localized.
    pub(super) sampler: String,
    /// Mask gaussian blur radius in pixels — the soft seam around the regenerated area.
    pub(super) mask_blur: u32,
    /// Mask dilation in pixels, which covers the anti-aliasing halo around text.
    pub(super) mask_dilation: u32,
    /// LaMa checkpoint file name used for the 4-channel prefill. Ignored by the 9-channel
    /// mode and never put on the wire there.
    pub(super) lama_model: String,
}

impl Default for SdxlSettings {
    fn default() -> Self {
        Self::for_mode(SdxlMode::NineChannel)
    }
}

impl SdxlSettings {
    /// The shipped defaults of `mode`.
    ///
    /// Only the denoise differs between the two: the 9-channel pipeline regenerates the hole
    /// outright, while the 4-channel one paints over a LaMa prefill that already removed the
    /// text, so a moderate denoise keeps the surrounding texture.
    #[must_use]
    pub(super) fn for_mode(mode: SdxlMode) -> Self {
        let denoise = match mode {
            SdxlMode::NineChannel => 1.0,
            SdxlMode::FourChannel => 0.75,
        };
        Self {
            model_path: String::new(),
            positive_prompt: "clean background".to_string(),
            negative_prompt: "text, letters, watermark, signature, speech bubble".to_string(),
            steps: 30,
            cfg_scale: 7.0,
            denoise_strength: denoise,
            seed: SDXL_RANDOM_SEED,
            sampler: "DPM++ 2M Karras".to_string(),
            mask_blur: 4,
            mask_dilation: 6,
            lama_model: default_lama_model_filename().to_string(),
        }
    }
}

/// The persisted document: the parameters of BOTH modes plus the mode last selected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SdxlPersisted {
    /// [`SdxlMode::wire`] of the selected mode.
    pub(super) mode: String,
    pub(super) nine_channel: SdxlSettings,
    pub(super) four_channel: SdxlSettings,
}

impl Default for SdxlPersisted {
    fn default() -> Self {
        Self {
            mode: SdxlMode::NineChannel.wire().to_string(),
            nine_channel: SdxlSettings::for_mode(SdxlMode::NineChannel),
            four_channel: SdxlSettings::for_mode(SdxlMode::FourChannel),
        }
    }
}

/// The immutable snapshot one run is executed from.
///
/// Taken on the GUI thread when the run starts and moved into the worker, so a parameter the
/// user edits while the pass is in flight cannot change what that pass is doing.
#[derive(Debug, Clone)]
pub(super) struct SdxlRunConfig {
    pub(super) mode: SdxlMode,
    pub(super) settings: SdxlSettings,
}

/// Reads the settings file, falling back to defaults on any error.
///
/// A missing file is the normal first-run case and a corrupt one must not block the tool, so
/// neither is reported: the engine simply starts from its defaults. Runs on a worker thread.
#[must_use]
pub(super) fn load_sdxl_settings() -> SdxlPersisted {
    let path = config::sdxl_inpaint_settings_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return SdxlPersisted::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Writes `persisted` to the engine's settings file. Runs on a worker thread.
///
/// # Errors
/// Returns a user-facing message when the data directory cannot be created, when the
/// document cannot be serialized, or when the write fails.
pub(super) fn save_sdxl_settings(persisted: &SdxlPersisted) -> Result<(), String> {
    let path = config::sdxl_inpaint_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    }
    let raw = serde_json::to_string_pretty(persisted)
        .map_err(|err| tf!("cleaning.tools.sdxl.serialize_settings_error", err = err))?;
    fs::write(&path, raw).map_err(|err| tf!("cleaning.tools.sdxl.write_settings_error", err = err))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire tokens are the backend's dispatch keys; an unknown one falls back to the
    /// 9-channel mode rather than leaving the engine with no mode at all.
    #[test]
    fn the_mode_survives_a_wire_round_trip_and_an_unknown_token_falls_back() {
        assert_eq!(SdxlMode::NineChannel.wire(), "nine_channel");
        assert_eq!(SdxlMode::FourChannel.wire(), "four_channel");
        for mode in [SdxlMode::NineChannel, SdxlMode::FourChannel] {
            assert_eq!(SdxlMode::from_wire(mode.wire()), mode);
        }
        assert_eq!(SdxlMode::from_wire("bogus"), SdxlMode::NineChannel);
        assert_eq!(SdxlMode::from_wire("  four_channel  "), SdxlMode::FourChannel);
    }

    /// The shipped defaults, pinned: a change here silently changes what every user gets on
    /// a fresh install. Only the denoise differs between the modes.
    #[test]
    fn the_defaults_match_the_shipped_parameters() {
        let nine = SdxlSettings::for_mode(SdxlMode::NineChannel);
        let four = SdxlSettings::for_mode(SdxlMode::FourChannel);
        assert!((nine.denoise_strength - 1.0).abs() < f32::EPSILON);
        assert!((four.denoise_strength - 0.75).abs() < f32::EPSILON);
        assert_eq!(nine.steps, 30);
        assert!((nine.cfg_scale - 7.0).abs() < f32::EPSILON);
        assert_eq!(nine.seed, SDXL_RANDOM_SEED, "the -1 sentinel is the backend's own contract");
        assert_eq!(SDXL_RANDOM_SEED, -1, "the sentinel value itself is the wire contract");
        assert_eq!(nine.mask_blur, 4);
        assert_eq!(nine.mask_dilation, 6);
        assert!(nine.model_path.is_empty(), "there is no default checkpoint to guess");
        assert!(
            SDXL_SAMPLERS.contains(&nine.sampler.as_str()),
            "the default sampler must be one the panel offers"
        );
        assert_eq!(
            nine.lama_model,
            default_lama_model_filename(),
            "the prefill default comes from the LaMa catalog, not from a second copy of it"
        );
        // Everything except the denoise is shared, which is what makes switching mode cheap.
        assert_eq!(SdxlSettings { denoise_strength: nine.denoise_strength, ..four.clone() }, nine);
    }

    /// The on-disk document keeps BOTH modes: switching mode and back must not reset the
    /// other mode's prompts, weights path or denoise.
    #[test]
    fn both_modes_survive_a_document_round_trip() {
        let mut doc = SdxlPersisted {
            mode: SdxlMode::FourChannel.wire().to_string(),
            ..SdxlPersisted::default()
        };
        doc.four_channel.model_path = "/models/anime.safetensors".to_string();
        doc.four_channel.steps = 42;
        doc.nine_channel.positive_prompt = "nine channel prompt".to_string();
        let raw = serde_json::to_string(&doc).expect("serialize");
        let back: SdxlPersisted = serde_json::from_str(&raw).expect("deserialize");
        assert_eq!(back, doc, "the unselected mode's parameters must survive");
    }

    /// A partial or hand-edited document loads with defaults for the missing keys instead of
    /// failing outright — the field names are fixed by on-disk compatibility, so an existing
    /// file keeps loading.
    #[test]
    fn a_partial_document_loads_with_defaults_for_the_missing_fields() {
        let doc: SdxlPersisted = serde_json::from_str("{}").expect("deserialize empty");
        assert_eq!(doc, SdxlPersisted::default());

        let legacy = r#"{"mode":"four_channel","four_channel":{"model_path":"/w.safetensors","steps":24}}"#;
        let doc: SdxlPersisted = serde_json::from_str(legacy).expect("deserialize legacy");
        assert_eq!(doc.mode, "four_channel");
        assert_eq!(doc.four_channel.model_path, "/w.safetensors");
        assert_eq!(doc.four_channel.steps, 24);
        assert_eq!(
            doc.four_channel.sampler,
            SdxlSettings::default().sampler,
            "a missing key is its default, not a parse failure"
        );
        assert_eq!(doc.nine_channel, SdxlSettings::for_mode(SdxlMode::NineChannel));
    }
}
