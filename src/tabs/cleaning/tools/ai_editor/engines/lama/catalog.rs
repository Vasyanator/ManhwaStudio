/*
File: cleaning/tools/ai_editor/engines/lama/catalog.rs

Purpose:
The fixed catalog of LaMa checkpoints the engine offers, and everything derived from an
entry: which backend method runs it, which parameter set it takes, whether the backend can
refine on it, which directory it lives in and how it is fetched when missing.

Main responsibilities:
- own `LAMA_MODEL_SPECS` and the default selection;
- own `LamaMethod`, the two-method dispatch shared by the run, the unload and the scan;
- resolve a persisted/selected file name back to a spec, refusing an unknown one;
- ensure the selected checkpoint is on disk before a run (download when missing).

Key structures:
- `LamaMethod`: `inpaint.lama_v2` vs `inpaint.lama_mpe`
- `LamaModelSpec`: one catalog entry

Key functions:
- `lama_model_catalog()`, `lama_v2_model_catalog()`, `default_lama_model_filename()`
- `lama_model_spec_by_name()`, `ensure_selected_lama_model_ready()`,
  `ensure_lama_model_for_external()`

Notes:
`file_name` is the persisted identity of a selection and is never localized
(`dev-docs/i18n_exclusions.md` §A5); the display name is an i18n key resolved at draw time.
`supports_refine` is a CONSTANT of the entry, not a re-derivation from the file extension:
the backend refuses its refine pass on a TorchScript `.pt` outright, and a call site that
re-guessed the rule from a suffix would be a second, drifting copy of it.

The v2 subset is public beyond this module because the SDXL 4-channel prefill runs the same
three checkpoints through `inpaint.sdxl`'s `lama_model` field. The MPE entry is deliberately
NOT in that view: its checkpoint is not in `Torch/LaMa/models` and its architecture is not
what the SDXL prefill path loads.
*/

use super::*;

/// Which backend method a catalog entry runs.
///
/// The two are separate architectures, not one model with two weight files: LaMa-MPE builds
/// a different generator with a second sub-network, takes a four-argument forward and lives
/// in its own model directory. Keeping both methods and dispatching here is what lets the
/// user see ONE run button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tabs::cleaning::tools::ai_editor::engines) enum LamaMethod {
    /// `inpaint.lama_v2`: the saicinpainting / TorchScript family in `Torch/LaMa/models`,
    /// parameterized by `refine` / `n_iters` / `max_scales` / `px_budget` / `model_name`.
    V2,
    /// `inpaint.lama_mpe`: the LaMa-MPE architecture in `Torch/LaMa_MPE`, parameterized by
    /// `inpaint_size` alone. It carries NO `model_name` — the method itself names the file.
    Mpe,
}

impl LamaMethod {
    /// The IPC method that runs one pass.
    pub(super) fn call_method(self) -> &'static str {
        match self {
            LamaMethod::V2 => backend_ipc::protocol::METHOD_INPAINT_LAMA_V2,
            LamaMethod::Mpe => backend_ipc::protocol::METHOD_INPAINT_LAMA_MPE,
        }
    }

    /// The IPC method that drops the resident model.
    pub(super) fn unload_method(self) -> &'static str {
        match self {
            LamaMethod::V2 => backend_ipc::protocol::METHOD_INPAINT_LAMA_V2_UNLOAD,
            LamaMethod::Mpe => backend_ipc::protocol::METHOD_INPAINT_LAMA_MPE_UNLOAD,
        }
    }

    /// Caption of the unload button, which names the model family it drops.
    pub(super) fn unload_button_label(self) -> &'static str {
        match self {
            LamaMethod::V2 => t!("cleaning.tools.lama.unload_button"),
            LamaMethod::Mpe => t!("cleaning.tools.lama_mpe.unload_button"),
        }
    }

    /// Status line confirming that the unload was requested.
    pub(super) fn unload_requested_status(self) -> &'static str {
        match self {
            LamaMethod::V2 => t!("cleaning.tools.lama.unload_requested_status"),
            LamaMethod::Mpe => t!("cleaning.tools.lama_mpe.unload_requested_status"),
        }
    }

    /// Directory the method's checkpoints live in, which is what the presence scan reads
    /// and what the ensure-before-run path checks.
    pub(super) fn models_dir(self) -> PathBuf {
        match self {
            LamaMethod::V2 => config::lama_models_dir(),
            LamaMethod::Mpe => config::lama_mpe_dir(),
        }
    }
}

/// One entry of the fixed model catalog.
///
/// `file_name` is the entry's IDENTITY: it is what is persisted, what the presence scan
/// matches and — for [`LamaMethod::V2`] — what travels on the wire as `model_name`. It is a
/// literal and is never localized.
#[derive(Debug, Clone, Copy)]
pub(in crate::tabs::cleaning::tools::ai_editor::engines) struct LamaModelSpec {
    /// Bare file name inside [`LamaMethod::models_dir`]. The persisted selection identity.
    pub file_name: &'static str,
    /// Stable i18n catalog key of the UI display name, resolved at draw time.
    pub display_key: &'static str,
    /// Which backend method runs this checkpoint, and therefore which parameters apply.
    pub method: LamaMethod,
    /// Whether the backend can run its refine pass on this checkpoint.
    ///
    /// `false` for every TorchScript `.pt` (the runtime raises rather than refining) and for
    /// the MPE method, which has no refine pass at all. Constant per entry on purpose — see
    /// the file header.
    pub supports_refine: bool,
}

impl LamaModelSpec {
    /// Localized UI display name. A runtime lookup rather than a `const` because `t!` is not
    /// const; falls back to the key itself on a catalog miss.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        ms_i18n::lookup(self.display_key).unwrap_or(self.display_key)
    }
}

/// File name selected when nothing is persisted yet, and the fallback for a persisted name
/// that is not in the catalog.
pub(super) const DEFAULT_LAMA_MODEL_FILENAME: &str = "anime-manga-big-lama.pt";

/// The four offered checkpoints, in picker order.
///
/// `anime-manga-big-lama.pt` is TorchScript, which is why it is the one v2 entry that
/// cannot refine; the two `.ckpt` entries load through saicinpainting and can.
const LAMA_MODEL_SPECS: [LamaModelSpec; 4] = [
    LamaModelSpec {
        file_name: "best.ckpt",
        display_key: "cleaning.tools.lama.model_base",
        method: LamaMethod::V2,
        supports_refine: true,
    },
    LamaModelSpec {
        file_name: "lama_large_512px.ckpt",
        display_key: "cleaning.tools.lama.model_anime_v1",
        method: LamaMethod::V2,
        supports_refine: true,
    },
    LamaModelSpec {
        file_name: "anime-manga-big-lama.pt",
        display_key: "cleaning.tools.lama.model_anime_v2",
        method: LamaMethod::V2,
        supports_refine: false,
    },
    LamaModelSpec {
        file_name: "inpainting_lama_mpe.ckpt",
        display_key: "cleaning.tools.lama.model_mpe",
        method: LamaMethod::Mpe,
        supports_refine: false,
    },
];

/// Every catalog entry, in picker order.
#[must_use]
pub(super) fn lama_model_catalog() -> &'static [LamaModelSpec] {
    &LAMA_MODEL_SPECS
}

/// The entries the `inpaint.lama_v2` method can run — the three checkpoints in
/// `Torch/LaMa/models`.
///
/// This is the view the SDXL 4-channel prefill picker offers, and it is the reason the
/// catalog is reachable outside `ai_editor` at all: SDXL sends a `lama_model` FILE NAME that
/// the backend resolves inside that one directory, so offering the MPE checkpoint there
/// would name a file its prefill path cannot load.
///
/// No `#[must_use]`: `impl Iterator` already carries one (`clippy::double_must_use`).
pub(in crate::tabs::cleaning::tools::ai_editor::engines) fn lama_v2_model_catalog()
-> impl Iterator<Item = &'static LamaModelSpec> {
    LAMA_MODEL_SPECS
        .iter()
        .filter(|spec| spec.method == LamaMethod::V2)
}

/// Default LaMa model file name, for callers that have no selection of their own yet.
#[must_use]
pub(in crate::tabs::cleaning::tools::ai_editor::engines) fn default_lama_model_filename() -> &'static str {
    DEFAULT_LAMA_MODEL_FILENAME
}

/// Resolves a file name to its catalog entry, `None` when it is not offered.
///
/// The name is trimmed because it may come from a hand-edited settings file.
#[must_use]
pub(super) fn lama_model_spec_by_name(model_name: &str) -> Option<&'static LamaModelSpec> {
    LAMA_MODEL_SPECS
        .iter()
        .find(|spec| spec.file_name == model_name.trim())
}

/// The catalog entry a persisted name selects, falling back to the default rather than
/// leaving the engine without a model.
#[must_use]
pub(super) fn lama_model_spec_or_default(model_name: &str) -> &'static LamaModelSpec {
    lama_model_spec_by_name(model_name).unwrap_or_else(|| {
        lama_model_spec_by_name(DEFAULT_LAMA_MODEL_FILENAME)
            .unwrap_or(&LAMA_MODEL_SPECS[0])
    })
}

/// Ensures `spec`'s checkpoint is on disk, downloading it through `ai_models` when missing.
///
/// Intended to run OFF the GUI thread: the download is a multi-hundred-megabyte transfer.
///
/// # Errors
/// Returns the localized `ai_models` message when the download fails.
pub(super) fn ensure_lama_model_ready(spec: &LamaModelSpec) -> Result<(), String> {
    let local_path = spec.method.models_dir().join(spec.file_name);
    if local_path.exists() {
        return Ok(());
    }
    // The fetched PATH is discarded: the backend resolves the checkpoint by BARE FILE NAME
    // inside its own model directory, so the local path has no reader here.
    match spec.method {
        // The v2 fetch also brings `Torch/LaMa/config.yaml`, which the backend requires
        // before ANY v2 load — including a TorchScript one that never reads it.
        LamaMethod::V2 => ai_models::ensure_lama_model(&config::models_dir(), spec.file_name)?,
        LamaMethod::Mpe => ai_models::ensure_lama_mpe(&config::models_dir())?,
    };
    Ok(())
}

/// Ensures a LaMa-v2 checkpoint named by an outside caller is present, and echoes back the
/// catalog's own spelling of its file name.
///
/// Used by the SDXL 4-channel prefill, which sends that name to the backend. Only the v2
/// subset is resolvable here: a name outside it — the MPE checkpoint included — is refused
/// rather than silently prefilled with something else.
///
/// # Errors
/// Returns a localized message when `model_name` is not a LaMa-v2 catalog entry, or when the
/// download fails.
pub(in crate::tabs::cleaning::tools::ai_editor::engines) fn ensure_lama_model_for_external(
    model_name: &str,
) -> Result<&'static str, String> {
    let spec = lama_v2_model_catalog()
        .find(|spec| spec.file_name == model_name.trim())
        .ok_or_else(|| {
            tf!("cleaning.tools.lama.unsupported_model_error", selected_name = model_name)
        })?;
    ensure_lama_model_ready(spec)?;
    Ok(spec.file_name)
}

/// Whether `key` exists in the EMBEDDED English catalog, read straight from the tracked JSON.
///
/// NOT `ms_i18n::lookup`: that answers from the process-global ACTIVE catalog, which nothing in
/// this test binary installs, so an assertion on it passes or fails depending on whether some
/// other test happened to set a locale first. This asks about the catalog FILE, which is the
/// fact the tests actually mean.
#[cfg(test)]
pub(super) fn embedded_en_catalog_has(key: &str) -> bool {
    ms_i18n::catalog::embedded_locales()
        .iter()
        .find(|(tag, _)| *tag == "en")
        .and_then(|(_, source)| serde_json::from_str::<Value>(source).ok())
        .is_some_and(|value| value.get(key).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog's shape is a contract: four entries, the anime-v2 checkpoint selected by
    /// default, and every file name distinct — the name is the persisted identity, so a
    /// duplicate would make a restored selection ambiguous.
    #[test]
    fn the_catalog_offers_four_distinct_entries_and_defaults_to_anime_v2() {
        assert_eq!(lama_model_catalog().len(), 4);
        let mut names: Vec<&str> = lama_model_catalog().iter().map(|s| s.file_name).collect();
        assert_eq!(
            names,
            [
                "best.ckpt",
                "lama_large_512px.ckpt",
                "anime-manga-big-lama.pt",
                "inpainting_lama_mpe.ckpt"
            ]
        );
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total, "two entries share a file name");
        assert_eq!(default_lama_model_filename(), "anime-manga-big-lama.pt");
        assert_eq!(
            lama_model_spec_or_default("nothing like it").file_name,
            "anime-manga-big-lama.pt",
            "an unknown persisted name must fall back to the default, not leave no model"
        );
    }

    /// Each entry names the method that runs it, and refine is offered on the two `.ckpt`
    /// v2 checkpoints only: the backend refuses it on TorchScript and LaMa-MPE has no
    /// refine pass at all.
    #[test]
    fn each_entry_pins_its_method_and_its_refine_support() {
        let by_name = |name: &str| lama_model_spec_by_name(name).expect("catalog entry");
        assert_eq!(by_name("best.ckpt").method, LamaMethod::V2);
        assert!(by_name("best.ckpt").supports_refine);
        assert_eq!(by_name("lama_large_512px.ckpt").method, LamaMethod::V2);
        assert!(by_name("lama_large_512px.ckpt").supports_refine);
        assert_eq!(by_name("anime-manga-big-lama.pt").method, LamaMethod::V2);
        assert!(
            !by_name("anime-manga-big-lama.pt").supports_refine,
            "TorchScript: the backend raises instead of refining"
        );
        assert_eq!(by_name("inpainting_lama_mpe.ckpt").method, LamaMethod::Mpe);
        assert!(!by_name("inpainting_lama_mpe.ckpt").supports_refine);
    }

    /// The methods dispatch to the two distinct IPC method pairs; nothing may collapse them
    /// into one, because the two architectures cannot be reached through one method.
    #[test]
    fn the_two_methods_dispatch_to_distinct_ipc_methods() {
        assert_eq!(LamaMethod::V2.call_method(), "inpaint.lama_v2");
        assert_eq!(LamaMethod::Mpe.call_method(), "inpaint.lama_mpe");
        assert_eq!(LamaMethod::V2.unload_method(), "inpaint.lama_v2.unload");
        assert_eq!(LamaMethod::Mpe.unload_method(), "inpaint.lama_mpe.unload");
        assert_ne!(LamaMethod::V2.models_dir(), LamaMethod::Mpe.models_dir());
    }

    /// The SDXL prefill view offers exactly today's three v2 checkpoints and never the MPE
    /// one, whose file is not in `Torch/LaMa/models` at all.
    #[test]
    fn the_sdxl_view_offers_only_the_lama_v2_checkpoints() {
        let v2: Vec<&str> = lama_v2_model_catalog().map(|s| s.file_name).collect();
        assert_eq!(
            v2,
            ["best.ckpt", "lama_large_512px.ckpt", "anime-manga-big-lama.pt"]
        );
        assert!(
            ensure_lama_model_for_external("inpainting_lama_mpe.ckpt").is_err(),
            "the MPE checkpoint must never reach the SDXL prefill path"
        );
        assert!(ensure_lama_model_for_external("no such model").is_err());
    }

    /// Every display key must resolve in the embedded catalog: the picker shows these
    /// strings, and a missing key would print the key itself at the user.
    #[test]
    fn every_display_key_resolves() {
        for spec in lama_model_catalog() {
            assert!(
                embedded_en_catalog_has(spec.display_key),
                "unknown catalog key {}",
                spec.display_key
            );
        }
    }
}
