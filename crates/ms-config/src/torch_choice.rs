/*
File: crates/ms-config/src/torch_choice.rs

Purpose:
The shared vocabulary of "which PyTorch build is (or should be) installed": the torch
flavour, the AMD GPU architecture name (`gfxNNNN`) and the persisted record of the user's
installer choice under `General.torch_flavour` / `General.torch_rocm_gfx` in
`user_config`. GUI-free, I/O-free: every item is a pure value or a pure conversion.

Key structures:
- TorchFlavour: CPU / CUDA / ROCm, with its frozen config token and the mapping from a torch
  local version tag (`+cpu`, `+cu128`, `+rocm10.1.0`).
- GfxArch: a validated LLVM AMDGPU processor name (`gfx1201`, `gfx90a`), parsed from tool
  output or decoded from the KFD `gfx_target_version` integer; names the AMD wheel extra.
- TorchChoiceRecord: the persisted installer choice; `Unrecorded` is the legacy state.

Key functions:
- GfxArch::parse / GfxArch::from_kfd_target_version / GfxArch::extra_name
- TorchFlavour::from_local_version
- TorchChoiceRecord::from_user_settings / TorchChoiceRecord::write_into_general

Notes:
The two keys are deliberately NOT part of `user_config_defaults()`: an absent key means
`Unrecorded` — an install made before the choice was recorded — and readers must keep the
legacy behaviour for it. Writing the record is the installer's job (one docstore update);
this module only reads and edits a `General` map handed to it. The Python backend never reads
these keys.
*/

use serde_json::{Map, Value};
use std::fmt;

/// `General` key holding the recorded torch flavour token (`"cpu"` / `"cuda"` / `"rocm"`,
/// see [`TorchFlavour::as_token`]). Written by the installer after a successful torch
/// install. NOT in the defaults tree: absent means [`TorchChoiceRecord::Unrecorded`], the
/// legacy-compatible path for installs that predate the record.
pub const GENERAL_TORCH_FLAVOUR_KEY: &str = "torch_flavour";

/// `General` key holding the AMD architecture (`"gfx1201"`) the ROCm torch was installed
/// for. Present only together with `torch_flavour = "rocm"`; NOT in the defaults tree.
pub const GENERAL_TORCH_ROCM_GFX_KEY: &str = "torch_rocm_gfx";

/// Which compute backend a PyTorch build targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TorchFlavour {
    /// CPU-only build (PyPI default on Windows/macOS, `+cpu` wheels).
    Cpu,
    /// NVIDIA CUDA build (`+cuNNN` wheels).
    Cuda,
    /// AMD ROCm / HIP build (`+rocmX.Y[.Z]` wheels).
    Rocm,
}

impl TorchFlavour {
    /// Frozen config token stored under [`GENERAL_TORCH_FLAVOUR_KEY`]:
    /// `Cpu` -> `"cpu"`, `Cuda` -> `"cuda"`, `Rocm` -> `"rocm"`. Never localized.
    #[must_use]
    pub fn as_token(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
            Self::Rocm => "rocm",
        }
    }

    /// Inverse of [`as_token`](Self::as_token) (surrounding whitespace ignored, case
    /// sensitive like every frozen token). Unknown tokens return `None`.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token.trim() {
            "cpu" => Some(Self::Cpu),
            "cuda" => Some(Self::Cuda),
            "rocm" => Some(Self::Rocm),
            _ => None,
        }
    }

    /// Flavour named by the local version tag of a torch version string.
    ///
    /// `version` is a full version (`"2.13.0+rocm10.1.0"`) or just the tag (`"+cu128"`).
    /// `+cpu` -> `Cpu`, `+cu<digit>…` -> `Cuda`, `+rocm<digit>…` -> `Rocm`. An untagged
    /// version (`"2.9.1"`: PyPI, whose flavour depends on the platform) or any other tag
    /// returns `None` — the caller decides what an untagged build means.
    #[must_use]
    pub fn from_local_version(version: &str) -> Option<Self> {
        let (_, tag) = version.trim().split_once('+')?;
        let tag = tag.trim().to_ascii_lowercase();
        let starts_with_digit_after = |prefix: &str| {
            tag.strip_prefix(prefix)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|ch| ch.is_ascii_digit())
        };
        if tag == "cpu" {
            Some(Self::Cpu)
        } else if starts_with_digit_after("rocm") {
            Some(Self::Rocm)
        } else if starts_with_digit_after("cu") {
            Some(Self::Cuda)
        } else {
            None
        }
    }
}

/// A validated LLVM AMDGPU processor name such as `gfx1201`, `gfx1036` or `gfx90a`.
///
/// Invariant: the inner string matches `^gfx[0-9][0-9a-f]{2,3}$`, is lowercase, and is not
/// `gfx000` (the CPU agent `rocm_agent_enumerator` prints). Generic and family ISA names
/// (`gfx11-generic`, `gfx12-0`, `gfx110x`) are NOT processors and never parse.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GfxArch(String);

impl GfxArch {
    /// Parses a processor name from tool output, a config value or a wheel name.
    ///
    /// Trims, lowercases and drops a target-feature suffix (`"gfx90a:sramecc+:xnack-"` ->
    /// `gfx90a`), then requires the invariant above. Returns `None` for anything else.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let lower = text.trim().to_ascii_lowercase();
        let base = lower.split(':').next().unwrap_or_default();
        let tail = base.strip_prefix("gfx")?;
        let mut chars = tail.chars();
        let first_is_digit = chars.next().is_some_and(|ch| ch.is_ascii_digit());
        let rest_is_hex = chars.all(|ch| ch.is_ascii_digit() || ('a'..='f').contains(&ch));
        let valid_len = (3..=4).contains(&tail.len());
        if !(first_is_digit && rest_is_hex && valid_len) || base == "gfx000" {
            return None;
        }
        Some(Self(base.to_string()))
    }

    /// Decodes the KFD topology `gfx_target_version` integer
    /// (`/sys/class/kfd/kfd/topology/nodes/<n>/properties`).
    ///
    /// Encoding: `major * 10000 + minor * 100 + stepping`, printed as
    /// `gfx{major}{minor:x}{stepping:x}` (`120001` -> `gfx1201`, `90010` -> `gfx90a`,
    /// `100306` -> `gfx1036`). `0` (a CPU node), a minor or stepping of 16 or more, and any
    /// result that does not satisfy the invariant return `None`.
    #[must_use]
    pub fn from_kfd_target_version(value: u32) -> Option<Self> {
        if value == 0 {
            return None;
        }
        let major = value / 10_000;
        let minor = (value / 100) % 100;
        let stepping = value % 100;
        if minor >= 16 || stepping >= 16 {
            return None;
        }
        Self::parse(&format!("gfx{major}{minor:x}{stepping:x}"))
    }

    /// The processor name, e.g. `"gfx1201"`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Name of the AMD torch wheel extra that selects this architecture's device
    /// packages: `"device-gfx1201"` (used as `torch[device-gfx1201]`).
    #[must_use]
    pub fn extra_name(&self) -> String {
        format!("device-{}", self.0)
    }
}

impl fmt::Display for GfxArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The installer's recorded torch choice, persisted in `user_config` `General`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorchChoiceRecord {
    /// No valid record: an install made before the choice was recorded, a hand-edited
    /// value, or `"rocm"` without a valid architecture. Readers keep the legacy behaviour.
    Unrecorded,
    /// The CPU build was chosen.
    Cpu,
    /// A CUDA build was chosen.
    Cuda,
    /// A ROCm build was chosen for the given architecture.
    Rocm {
        /// The architecture whose device packages were installed.
        gfx: GfxArch,
    },
}

impl TorchChoiceRecord {
    /// Reads the record from a raw `user_config` tree. Pure; never fails.
    ///
    /// A missing `General` section, a missing or unknown [`GENERAL_TORCH_FLAVOUR_KEY`], or
    /// `"rocm"` with a missing or invalid [`GENERAL_TORCH_ROCM_GFX_KEY`] all yield
    /// [`Unrecorded`](Self::Unrecorded). A gfx key next to `"cpu"`/`"cuda"` is ignored.
    #[must_use]
    pub fn from_user_settings(user_settings: &Value) -> Self {
        let general = user_settings.get("General").and_then(Value::as_object);
        let flavour = general
            .and_then(|general| general.get(GENERAL_TORCH_FLAVOUR_KEY))
            .and_then(Value::as_str)
            .and_then(TorchFlavour::from_token);
        match flavour {
            None => Self::Unrecorded,
            Some(TorchFlavour::Cpu) => Self::Cpu,
            Some(TorchFlavour::Cuda) => Self::Cuda,
            Some(TorchFlavour::Rocm) => general
                .and_then(|general| general.get(GENERAL_TORCH_ROCM_GFX_KEY))
                .and_then(Value::as_str)
                .and_then(GfxArch::parse)
                .map_or(Self::Unrecorded, |gfx| Self::Rocm { gfx }),
        }
    }

    /// Writes the record into a `General` object map (the caller owns the docstore
    /// transaction around it).
    ///
    /// `Cpu` / `Cuda` set the flavour key and REMOVE the gfx key; `Rocm` sets both;
    /// `Unrecorded` removes both, returning the document to the legacy state. Every other
    /// key of the map is left untouched.
    pub fn write_into_general(&self, general: &mut Map<String, Value>) {
        match self {
            Self::Unrecorded => {
                general.remove(GENERAL_TORCH_FLAVOUR_KEY);
                general.remove(GENERAL_TORCH_ROCM_GFX_KEY);
            }
            Self::Cpu => {
                general.insert(GENERAL_TORCH_FLAVOUR_KEY.to_string(), Value::from(TorchFlavour::Cpu.as_token()));
                general.remove(GENERAL_TORCH_ROCM_GFX_KEY);
            }
            Self::Cuda => {
                general.insert(GENERAL_TORCH_FLAVOUR_KEY.to_string(), Value::from(TorchFlavour::Cuda.as_token()));
                general.remove(GENERAL_TORCH_ROCM_GFX_KEY);
            }
            Self::Rocm { gfx } => {
                general.insert(GENERAL_TORCH_FLAVOUR_KEY.to_string(), Value::from(TorchFlavour::Rocm.as_token()));
                general.insert(GENERAL_TORCH_ROCM_GFX_KEY.to_string(), Value::from(gfx.as_str()));
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gfx(text: &str) -> GfxArch {
        GfxArch::parse(text).unwrap_or_else(|| panic!("{text} must parse"))
    }

    #[test]
    fn flavour_tokens_round_trip() {
        for flavour in [TorchFlavour::Cpu, TorchFlavour::Cuda, TorchFlavour::Rocm] {
            assert_eq!(TorchFlavour::from_token(flavour.as_token()), Some(flavour));
        }
        assert_eq!(TorchFlavour::from_token(" rocm "), Some(TorchFlavour::Rocm));
        assert_eq!(TorchFlavour::from_token("ROCm"), None);
        assert_eq!(TorchFlavour::from_token("hip"), None);
    }

    #[test]
    fn flavour_from_local_version_table() {
        let cases = [
            ("2.9.1+cpu", Some(TorchFlavour::Cpu)),
            ("+cpu", Some(TorchFlavour::Cpu)),
            ("2.9.1+cu128", Some(TorchFlavour::Cuda)),
            ("2.9.1+cu126", Some(TorchFlavour::Cuda)),
            ("2.13.0+rocm10.1.0", Some(TorchFlavour::Rocm)),
            ("2.9.1+rocm6.4", Some(TorchFlavour::Rocm)),
            ("2.9.1", None),
            ("2.9.1+", None),
            ("2.9.1+custom", None),
            ("2.9.1+cuda", None),
            ("2.9.1+rocm", None),
            ("", None),
        ];
        for (version, expected) in cases {
            assert_eq!(TorchFlavour::from_local_version(version), expected, "{version}");
        }
    }

    #[test]
    fn gfx_parse_accepts_processor_names() {
        assert_eq!(gfx("gfx1201").as_str(), "gfx1201");
        assert_eq!(gfx(" GFX1100 \n").as_str(), "gfx1100");
        assert_eq!(gfx("gfx90a").as_str(), "gfx90a");
        assert_eq!(gfx("gfx803").as_str(), "gfx803");
        assert_eq!(gfx("gfx90a:sramecc+:xnack-").as_str(), "gfx90a");
        assert_eq!(gfx("gfx1036").to_string(), "gfx1036");
    }

    #[test]
    fn gfx_parse_rejects_non_processors() {
        for text in [
            "gfx000", "gfx11-generic", "gfx12-0", "gfx110x", "gfx11", "gfx", "gfx12010", "gfxa01",
            "gfx1g01", "amdgcn-amd-amdhsa--gfx1201", "1201", "", "gfx 1201",
        ] {
            assert_eq!(GfxArch::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn gfx_from_kfd_target_version_table() {
        let cases = [
            (120_001, Some("gfx1201")),
            (120_000, Some("gfx1200")),
            (110_000, Some("gfx1100")),
            (110_501, Some("gfx1151")),
            // Not a valid encoding (minor 51); gfx1151 is 110501, not 115100.
            (115_100, None),
            (100_306, Some("gfx1036")),
            (90_010, Some("gfx90a")),
            (90_402, Some("gfx942")),
            (80_003, Some("gfx803")),
            (0, None),
            (91_600, None),
            (90_016, None),
            (1_000_000, None),
        ];
        for (value, expected) in cases {
            assert_eq!(GfxArch::from_kfd_target_version(value).as_ref().map(GfxArch::as_str), expected, "{value}");
        }
    }

    #[test]
    fn gfx_extra_name() {
        assert_eq!(gfx("gfx1201").extra_name(), "device-gfx1201");
    }

    #[test]
    fn record_round_trips_through_general() {
        for record in [
            TorchChoiceRecord::Cpu,
            TorchChoiceRecord::Cuda,
            TorchChoiceRecord::Rocm { gfx: gfx("gfx1201") },
            TorchChoiceRecord::Unrecorded,
        ] {
            let mut general = Map::new();
            general.insert("ai_install_type".to_string(), json!("Full"));
            record.write_into_general(&mut general);
            assert_eq!(general.get("ai_install_type"), Some(&json!("Full")), "{record:?}");
            let root = json!({ "General": Value::Object(general) });
            assert_eq!(TorchChoiceRecord::from_user_settings(&root), record);
        }
    }

    #[test]
    fn record_write_clears_stale_keys() {
        let mut general = Map::new();
        TorchChoiceRecord::Rocm { gfx: gfx("gfx1100") }.write_into_general(&mut general);
        TorchChoiceRecord::Cuda.write_into_general(&mut general);
        assert_eq!(general.get(GENERAL_TORCH_FLAVOUR_KEY), Some(&json!("cuda")));
        assert!(!general.contains_key(GENERAL_TORCH_ROCM_GFX_KEY));
        TorchChoiceRecord::Unrecorded.write_into_general(&mut general);
        assert!(general.is_empty());
    }

    #[test]
    fn record_unrecorded_cases() {
        let cases = [
            json!({}),
            json!({ "General": {} }),
            json!({ "General": "not an object" }),
            json!({ "General": { "torch_flavour": "metal" } }),
            json!({ "General": { "torch_flavour": 3 } }),
            json!({ "General": { "torch_flavour": "rocm" } }),
            json!({ "General": { "torch_flavour": "rocm", "torch_rocm_gfx": "gfx11-generic" } }),
            json!({ "General": { "torch_flavour": "rocm", "torch_rocm_gfx": "gfx000" } }),
            json!({ "General": { "torch_rocm_gfx": "gfx1201" } }),
        ];
        for root in cases {
            assert_eq!(TorchChoiceRecord::from_user_settings(&root), TorchChoiceRecord::Unrecorded, "{root}");
        }
        assert_eq!(
            TorchChoiceRecord::from_user_settings(&json!({ "General": { "torch_flavour": "cpu", "torch_rocm_gfx": "gfx1201" } })),
            TorchChoiceRecord::Cpu
        );
    }

    #[test]
    fn record_keys_are_not_in_defaults() {
        let defaults = crate::user_config_defaults();
        let general = defaults.get("General").and_then(Value::as_object);
        assert!(general.is_some_and(|general| !general.contains_key(GENERAL_TORCH_FLAVOUR_KEY)
            && !general.contains_key(GENERAL_TORCH_ROCM_GFX_KEY)));
    }
}
