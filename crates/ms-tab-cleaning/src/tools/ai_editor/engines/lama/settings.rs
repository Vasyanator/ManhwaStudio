/*
File: cleaning/tools/ai_editor/engines/lama/settings.rs

Purpose:
Everything the LaMa engine persists to `config::lama_engine_settings_path()` — the selected
model and the parameters of BOTH backend methods — plus the file IO and the save gate.

Main responsibilities:
- own `LamaSettings`, its defaults and its clamping (`normalized`);
- read and write the settings file on worker threads;
- decide when a save is due (`settings_save_due`).

Key structures:
- `LamaSettings`

Key functions:
- `load_lama_settings()`, `save_lama_settings()`, `settings_save_due()`

Notes:
ONE file for the whole engine, unlike FLUX.2 klein's file-per-variant: the selected model is
itself a persisted field here, so a per-model file could not record which model to restore.
The parameters of the method that is NOT selected are persisted too — switching model and
back must not silently reset the other method's `inpaint_size` or refine settings.

`normalized()` is the ONLY value ever put on the wire or written to disk: a hand-edited file
is clamped rather than rejected, and an unknown model name falls back to the default instead
of leaving the engine with no model at all.
*/

use super::*;

/// The engine's persisted state: the selected checkpoint and every parameter of both
/// methods.
///
/// `model` is a catalog FILE NAME (`LamaModelSpec::file_name`) and is the selection's
/// identity; it is a literal, never a localized string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct LamaSettings {
    /// File name of the selected catalog entry.
    pub(super) model: String,
    /// `inpaint.lama_v2`: run the refine pass. Ignored — and never put on the wire as
    /// `true` — for an entry whose `supports_refine` is `false`.
    pub(super) refine: bool,
    /// `inpaint.lama_v2` refine iterations.
    pub(super) n_iters: u8,
    /// `inpaint.lama_v2` refine scale count.
    pub(super) max_scales: u8,
    /// `inpaint.lama_v2` refine pixel budget.
    pub(super) px_budget: u32,
    /// `inpaint.lama_mpe` working resolution, in pixels of the longest side.
    pub(super) inpaint_size: u32,
}

impl Default for LamaSettings {
    fn default() -> Self {
        Self {
            model: DEFAULT_LAMA_MODEL_FILENAME.to_string(),
            // The four v2 defaults are the SHIPPED values users already have on disk:
            // refine off, and the three refine parameters at their shipped settings. A
            // change here silently changes what every fresh install gets, so a test pins
            // them.
            refine: false,
            n_iters: 15,
            max_scales: 3,
            px_budget: 1_000_000,
            inpaint_size: 2048,
        }
    }
}

impl LamaSettings {
    /// The catalog entry the selection names, falling back to the default entry.
    #[must_use]
    pub(super) fn spec(&self) -> &'static LamaModelSpec {
        lama_model_spec_or_default(&self.model)
    }

    /// A copy with every field inside its offered range and the model name spelled the way
    /// the catalog spells it.
    ///
    /// This is what goes on the wire and what is written to disk, so a hand-edited file can
    /// neither push an out-of-range parameter onto the backend nor persist itself further.
    #[must_use]
    pub(super) fn normalized(&self) -> Self {
        Self {
            model: self.spec().file_name.to_string(),
            refine: self.refine,
            n_iters: self.n_iters.clamp(LAMA_N_ITERS_MIN, LAMA_N_ITERS_MAX),
            max_scales: self.max_scales.clamp(LAMA_MAX_SCALES_MIN, LAMA_MAX_SCALES_MAX),
            px_budget: self.px_budget.clamp(LAMA_PX_BUDGET_MIN, LAMA_PX_BUDGET_MAX),
            inpaint_size: self
                .inpaint_size
                .clamp(LAMA_INPAINT_SIZE_MIN, LAMA_INPAINT_SIZE_MAX),
        }
    }
}

/// Reads the settings file, falling back to defaults on any error.
///
/// A missing file is the normal first-run case and a corrupt one must not block the tool, so
/// neither is reported: the engine simply starts from its defaults. Runs on a worker thread.
#[must_use]
pub(super) fn load_lama_settings() -> LamaSettings {
    let path = config::lama_engine_settings_path();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return LamaSettings::default(),
    };
    serde_json::from_str::<LamaSettings>(&raw)
        .map_or_else(|_| LamaSettings::default(), |settings| settings.normalized())
}

/// Writes `settings` to the engine's settings file.
///
/// Runs on a worker thread. The caller passes an already-`normalized` document.
///
/// # Errors
/// Returns a user-facing message when the data directory cannot be created, when the
/// settings cannot be serialized, or when the write fails.
pub(super) fn save_lama_settings(settings: &LamaSettings) -> Result<(), String> {
    let path = config::lama_engine_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    }
    let raw = serde_json::to_string_pretty(settings)
        .map_err(|err| tf!("cleaning.tools.lama.serialize_settings_error", err = err))?;
    fs::write(&path, raw).map_err(|err| tf!("cleaning.tools.lama.write_settings_error", err = err))
}

/// Whether a settings save must be started right now.
///
/// `dirty` is raised by every parameter change. `settings_loaded` gates it because the
/// initial load runs on its own worker: saving the in-memory DEFAULTS before that load lands
/// would overwrite the user's file with defaults — a silent data loss rather than a visible
/// failure. `save_in_flight` keeps at most one writer on the file at a time.
#[must_use]
pub(super) fn settings_save_due(dirty: bool, settings_loaded: bool, save_in_flight: bool) -> bool {
    dirty && settings_loaded && !save_in_flight
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These are the SHIPPED defaults; a change here silently changes what every existing
    /// user gets on a fresh install.
    #[test]
    fn the_defaults_match_the_shipped_parameters() {
        let settings = LamaSettings::default();
        assert_eq!(settings.model, "anime-manga-big-lama.pt");
        assert!(!settings.refine);
        assert_eq!(settings.n_iters, 15);
        assert_eq!(settings.max_scales, 3);
        assert_eq!(settings.px_budget, 1_000_000);
        assert_eq!(settings.inpaint_size, 2048);
    }

    /// A hand-edited file is clamped, not rejected: out-of-range parameters come back inside
    /// their offered ranges and an unknown model name falls back to the default entry.
    #[test]
    fn normalization_clamps_every_field_and_repairs_the_model_name() {
        let wild = LamaSettings {
            model: "  best.ckpt  ".to_string(),
            refine: true,
            n_iters: 200,
            max_scales: 0,
            px_budget: 9_000_000,
            inpaint_size: 1,
        }
        .normalized();
        assert_eq!(wild.model, "best.ckpt", "the catalog's own spelling is stored");
        assert!(wild.refine, "refine itself is a bool and is not clamped here");
        assert_eq!(wild.n_iters, LAMA_N_ITERS_MAX);
        assert_eq!(wild.max_scales, LAMA_MAX_SCALES_MIN);
        assert_eq!(wild.px_budget, LAMA_PX_BUDGET_MAX);
        assert_eq!(wild.inpaint_size, LAMA_INPAINT_SIZE_MIN);

        let unknown = LamaSettings {
            model: "somebody-elses.ckpt".to_string(),
            ..LamaSettings::default()
        }
        .normalized();
        assert_eq!(unknown.model, DEFAULT_LAMA_MODEL_FILENAME);
    }

    /// An absent key loads as its default rather than failing the whole document, and the
    /// parameters of the method that is not selected survive a round trip.
    #[test]
    fn a_partial_document_loads_with_defaults_for_the_missing_fields() {
        let parsed: LamaSettings = serde_json::from_str(r#"{"model":"best.ckpt"}"#)
            .expect("a partial settings document must still parse");
        assert_eq!(parsed.model, "best.ckpt");
        assert_eq!(parsed.inpaint_size, 2048, "the MPE parameter keeps its default");
        assert_eq!(parsed.n_iters, 15);

        let both = LamaSettings {
            model: "inpainting_lama_mpe.ckpt".to_string(),
            n_iters: 20,
            inpaint_size: 1024,
            ..LamaSettings::default()
        };
        let round_trip: LamaSettings =
            serde_json::from_str(&serde_json::to_string(&both).expect("serialize"))
                .expect("deserialize");
        assert_eq!(
            round_trip, both,
            "the unselected method's parameters must survive, or switching model and back would reset them"
        );
    }

    /// The save gate: only a real change writes, never before the initial load has landed,
    /// and never a second writer while one is in flight.
    #[test]
    fn a_save_is_due_only_after_the_load_and_never_twice_at_once() {
        assert!(settings_save_due(true, true, false));
        assert!(!settings_save_due(false, true, false), "nothing changed: no write");
        assert!(
            !settings_save_due(true, false, false),
            "saving before the load lands would overwrite the file with defaults"
        );
        assert!(!settings_save_due(true, true, true), "at most one writer at a time");
    }
}
