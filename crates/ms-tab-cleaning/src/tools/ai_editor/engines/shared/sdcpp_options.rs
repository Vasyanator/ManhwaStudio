/*
File: cleaning/tools/ai_editor/engines/shared/sdcpp_options.rs

Purpose:
The per-engine sd.cpp run options a large-model engine (FLUX.2 klein, FLUX.3 klein) persists in
its settings file and shows under «Stable-Diffusion.cpp»: where the text encoder runs, whether
weights stay in RAM, the device memory budget, VAE tiling and flash attention. Model-agnostic.

Key items:
- `SdCppRunOptions`: the persisted options (`#[serde(default)]`, unknown fields ignored) with
  `normalized()` and, natively, `to_context()` (-> `ms_sdcpp::ContextOptions`).
- `FlashAttentionChoice`: auto / on / off with a frozen, tolerant wire spelling.
- `VRAM_BUDGET_MIN_GIB` / `VRAM_BUDGET_MAX_GIB`: the budget range.

Notes:
The option types compile on every target (an engine's settings struct holds them everywhere);
only `to_context` names `ms_sdcpp` and is compiled out on wasm, where that crate is empty. The
device and the thread count are NOT here: they are app-wide (`ms_config::SdCppChoice`, set in
Settings › AI backend › Stable-Diffusion.cpp) and joined in by `to_context`.
*/

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Smallest device memory budget the dial offers, in GiB.
pub(in crate::tools::ai_editor::engines) const VRAM_BUDGET_MIN_GIB: f32 = 1.0;
/// Largest device memory budget the dial offers, in GiB.
pub(in crate::tools::ai_editor::engines) const VRAM_BUDGET_MAX_GIB: f32 = 128.0;

/// Flash-attention mode the user picked for sd.cpp runs. Persisted by [`Self::wire`]; an unknown
/// or malformed stored value reads as [`Self::Auto`] instead of failing the whole settings file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(in crate::tools::ai_editor::engines) enum FlashAttentionChoice {
    /// sd.cpp decides.
    #[default]
    Auto,
    /// Forced on.
    On,
    /// Forced off.
    Off,
}

impl FlashAttentionChoice {
    /// Every choice, in UI order.
    pub(in crate::tools::ai_editor::engines) const ALL: [Self; 3] = [Self::Auto, Self::On, Self::Off];

    /// The frozen persisted id (`"auto"` / `"on"` / `"off"`). Never localized, never renamed.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) const fn wire(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// Parses a persisted id (surrounding whitespace ignored); `None` for an unknown one.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn from_wire(value: &str) -> Option<Self> {
        let value = value.trim();
        Self::ALL.into_iter().find(|choice| choice.wire() == value)
    }

    /// The localized name shown in the picker.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn label(self) -> &'static str {
        match self {
            Self::Auto => t!("cleaning.tools.sdcpp.flash_attention_auto"),
            Self::On => t!("cleaning.tools.sdcpp.flash_attention_on"),
            Self::Off => t!("cleaning.tools.sdcpp.flash_attention_off"),
        }
    }
}

impl Serialize for FlashAttentionChoice {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.wire())
    }
}

impl<'de> Deserialize<'de> for FlashAttentionChoice {
    /// Tolerant: any value that is not a known id (another string, a number, null) reads as
    /// [`FlashAttentionChoice::Auto`] with a log line, so one bad field never discards the rest
    /// of an engine's settings.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        Ok(raw.as_str().and_then(Self::from_wire).unwrap_or_else(|| {
            ms_log::runtime_log::log_warn(format!("[cleaning] unknown sd.cpp flash_attention value {raw}; using \"auto\""));
            Self::Auto
        }))
    }
}

/// The sd.cpp options one engine persists. `Default` is a fresh settings file; a missing field
/// reads as its default and an unknown field is ignored (`#[serde(default)]`, no
/// `deny_unknown_fields`), so older and newer builds read each other's files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::tools::ai_editor::engines) struct SdCppRunOptions {
    /// Run the text encoder on the CPU (`te=cpu`); default on: it frees device memory for the
    /// diffusion model at the cost of a slower prompt encode.
    pub(in crate::tools::ai_editor::engines) text_encoder_on_cpu: bool,
    /// Keep the weights in RAM and stream them to the device per use.
    pub(in crate::tools::ai_editor::engines) weights_in_ram: bool,
    /// Device memory budget in GiB; `None` = automatic (no limit). Valid values lie in
    /// [`VRAM_BUDGET_MIN_GIB`]`..=`[`VRAM_BUDGET_MAX_GIB`] ([`Self::normalized`]).
    pub(in crate::tools::ai_editor::engines) vram_budget_gib: Option<f32>,
    /// Decode the VAE in tiles (less memory, slightly slower).
    pub(in crate::tools::ai_editor::engines) vae_tiling: bool,
    /// Flash-attention mode.
    pub(in crate::tools::ai_editor::engines) flash_attention: FlashAttentionChoice,
}

impl Default for SdCppRunOptions {
    fn default() -> Self {
        Self { text_encoder_on_cpu: true, weights_in_ram: false, vram_budget_gib: None, vae_tiling: false, flash_attention: FlashAttentionChoice::Auto }
    }
}

impl SdCppRunOptions {
    /// A copy with every value inside its valid range: the budget clamped to
    /// [`VRAM_BUDGET_MIN_GIB`]`..=`[`VRAM_BUDGET_MAX_GIB`], a non-finite budget (a hand-edited
    /// file) treated as automatic.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn normalized(&self) -> Self {
        let vram_budget_gib = self.vram_budget_gib.filter(|gib| gib.is_finite()).map(|gib| gib.clamp(VRAM_BUDGET_MIN_GIB, VRAM_BUDGET_MAX_GIB));
        Self { vram_budget_gib, ..self.clone() }
    }

    /// The context options of a run: these options (normalized) plus the app-wide device and
    /// thread count of `choice` (`0` threads = sd.cpp's default). VAE tiling is a REQUEST field,
    /// not a context one: the run job applies it to the request.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn to_context(&self, choice: &ms_config::SdCppChoice) -> ms_sdcpp::ContextOptions {
        let options = self.normalized();
        let device = match &choice.device {
            ms_config::SdCppDeviceChoice::Auto => ms_sdcpp::DeviceSelection::Auto,
            ms_config::SdCppDeviceChoice::Named { name, description } => ms_sdcpp::DeviceSelection::Named { name: name.clone(), description: description.clone() },
        };
        let flash_attention = match options.flash_attention {
            FlashAttentionChoice::Auto => ms_sdcpp::FlashAttention::Auto,
            FlashAttentionChoice::On => ms_sdcpp::FlashAttention::On,
            FlashAttentionChoice::Off => ms_sdcpp::FlashAttention::Off,
        };
        ms_sdcpp::ContextOptions {
            device,
            text_encoder_on_cpu: options.text_encoder_on_cpu,
            weights_in_ram: options.weights_in_ram,
            max_vram_gib: options.vram_budget_gib,
            flash_attention,
            threads: (choice.threads > 0).then_some(choice.threads),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty object is the default (encoder on the CPU), a missing field keeps its default,
    /// an unknown field is ignored and an unknown flash-attention value reads as auto.
    #[test]
    fn serde_defaults_and_tolerance() {
        let empty: SdCppRunOptions = serde_json::from_str("{}").expect("empty object");
        assert_eq!(empty, SdCppRunOptions::default());
        assert!(empty.text_encoder_on_cpu);
        let partial: SdCppRunOptions = serde_json::from_str(r#"{"vae_tiling": true, "from_the_future": 7}"#).expect("unknown field is ignored");
        assert_eq!(partial, SdCppRunOptions { vae_tiling: true, ..SdCppRunOptions::default() });
        let odd: SdCppRunOptions = serde_json::from_str(r#"{"flash_attention": "sometimes", "weights_in_ram": true}"#).expect("unknown flash value is tolerated");
        assert_eq!(odd.flash_attention, FlashAttentionChoice::Auto);
        assert!(odd.weights_in_ram, "the other fields survive a bad flash value");
        let number: SdCppRunOptions = serde_json::from_str(r#"{"flash_attention": 3}"#).expect("a non-string flash value is tolerated");
        assert_eq!(number.flash_attention, FlashAttentionChoice::Auto);
    }

    /// Every field round-trips, and the flash-attention ids are the frozen spellings.
    #[test]
    fn serde_round_trip_uses_the_frozen_ids() {
        let options = SdCppRunOptions { text_encoder_on_cpu: false, weights_in_ram: true, vram_budget_gib: Some(6.5), vae_tiling: true, flash_attention: FlashAttentionChoice::Off };
        let text = serde_json::to_string(&options).expect("serialize");
        assert!(text.contains(r#""flash_attention":"off""#), "{text}");
        let back: SdCppRunOptions = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, options);
        for choice in FlashAttentionChoice::ALL {
            assert_eq!(FlashAttentionChoice::from_wire(choice.wire()), Some(choice));
        }
        assert_eq!(FlashAttentionChoice::from_wire(" on "), Some(FlashAttentionChoice::On));
        assert_eq!(FlashAttentionChoice::from_wire("ON"), None);
    }

    /// The budget is clamped into 1..=128 GiB; a non-finite one is automatic.
    #[test]
    fn normalized_clamps_the_budget() {
        let with = |gib: Option<f32>| SdCppRunOptions { vram_budget_gib: gib, ..SdCppRunOptions::default() }.normalized().vram_budget_gib;
        assert_eq!(with(None), None);
        assert_eq!(with(Some(0.2)), Some(VRAM_BUDGET_MIN_GIB));
        assert_eq!(with(Some(500.0)), Some(VRAM_BUDGET_MAX_GIB));
        assert_eq!(with(Some(12.0)), Some(12.0));
        assert_eq!(with(Some(f32::NAN)), None);
        assert_eq!(with(Some(f32::INFINITY)), None);
    }

    /// `to_context` joins the options with the app-wide device and threads.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn to_context_maps_every_field() {
        let options = SdCppRunOptions { text_encoder_on_cpu: false, weights_in_ram: true, vram_budget_gib: Some(300.0), vae_tiling: true, flash_attention: FlashAttentionChoice::On };
        let named = ms_config::SdCppChoice {
            build: None,
            device: ms_config::SdCppDeviceChoice::Named { name: "Vulkan1".to_string(), description: "GPU".to_string() },
            threads: 6,
        };
        let context = options.to_context(&named);
        assert_eq!(
            context,
            ms_sdcpp::ContextOptions {
                device: ms_sdcpp::DeviceSelection::Named { name: "Vulkan1".to_string(), description: "GPU".to_string() },
                text_encoder_on_cpu: false,
                weights_in_ram: true,
                max_vram_gib: Some(VRAM_BUDGET_MAX_GIB),
                flash_attention: ms_sdcpp::FlashAttention::On,
                threads: Some(6),
            }
        );
        let auto = SdCppRunOptions::default().to_context(&ms_config::SdCppChoice::default());
        assert_eq!(auto.device, ms_sdcpp::DeviceSelection::Auto);
        assert_eq!(auto.threads, None, "0 threads = sd.cpp's default");
        assert_eq!(auto.max_vram_gib, None);
        assert_eq!(auto.flash_attention, ms_sdcpp::FlashAttention::Auto);
        assert!(auto.text_encoder_on_cpu);
        for (choice, expected) in [(FlashAttentionChoice::Auto, ms_sdcpp::FlashAttention::Auto), (FlashAttentionChoice::Off, ms_sdcpp::FlashAttention::Off)] {
            let options = SdCppRunOptions { flash_attention: choice, ..SdCppRunOptions::default() };
            assert_eq!(options.to_context(&ms_config::SdCppChoice::default()).flash_attention, expected);
        }
    }
}
