/*
File: cleaning/tools/ai_editor/engines/flux3_klein/test_support.rs

Purpose:
`cfg(test)` only: the fixtures the `mod tests` of this directory share — the English catalog
under the crate's locale lock, a `.status` answer with dictated presence, a reference with a
recognisable byte, and the list of every `cleaning.tools.flux3_klein.*` key the code spells plus
the shared «Постобработка» keys the panel draws.

Key items:
- `english()`, `status_with()`, `test_reference()`
- `FLUX3_PLAIN_KEYS`, `FLUX3_PLACEHOLDER_KEYS` (the stage keys come from `FLUX3_STAGES`),
  `POSTPROCESS_KEYS` (the shared section's full keys), `FLUX3_RETIRED_KEYS` (keys of the
  retired backend download, absent from every catalog)

Notes:
Not a test module itself; it declares no `#[test]`.
*/

use super::*;

/// Installs the English reference catalog under the crate's shared locale lock (`t!` reads the
/// PROCESS-GLOBAL catalog, so tests that read a text must serialize on it).
pub(super) fn english() -> std::sync::MutexGuard<'static, ()> {
    let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
    ms_i18n::set_locale(&en).expect("en catalog installs");
    guard
}

/// A `.status` answer whose present DiT formats and shared parts are dictated (a row for every
/// DiT id, no encoder rows: `text_encoder_present` covers the selection).
pub(super) fn status_with(formats: &[Flux3DitFormat], ae_present: bool, text_encoder_present: bool) -> Flux3Status {
    let mut entries = serde_json::Map::new();
    for format in Flux3DitFormat::all() {
        entries.insert(format.wire().to_string(), json!({ "available": formats.contains(format), "reason": null, "dit_bytes": 1 }));
    }
    parse_flux3_status(&json!({ "formats": entries, "ae_present": ae_present, "text_encoder_present": text_encoder_present }))
}

/// A 64x32 reference whose wire bytes are the single byte `marker` (tests read it back).
pub(super) fn test_reference(name: &str, marker: u8) -> Flux3Reference {
    Flux3Reference { name: name.to_string(), size: [64, 32], png: Arc::new(vec![marker]) }
}

/// Every `cleaning.tools.flux3_klein.<key>` without placeholders (the catalog test walks it).
pub(super) const FLUX3_PLAIN_KEYS: [&str; 94] = [
    "advanced_heading", "ae_bf16_hint", "ae_bf16_label", "browse_folder_tooltip",
    "bundle_missing_status", "cancel_pending_error", "download_destination_label", "catalog_download_hint",
    "empty_mask_error", "format_bf16_hint", "format_bf16_label", "format_fp8r_hint",
    "format_fp8r_label", "format_gguf_hint", "dit_format_picker_hint", "format_label", "format_nvfp4_hint",
    "format_nvfp4_label", "fp8_compute_auto", "fp8_compute_dequant", "fp8_compute_hint",
    "fp8_compute_label", "fp8_compute_scaled_mm", "install_button", "install_heading",
    "install_hint", "licence_link", "mask_heading", "mask_hint", "memory_heading",
    "model_readiness_unknown_status", "model_ready_status", "model_root_dialog_title",
    "package_folder_hint", "model_root_label", "model_root_missing_status", "model_root_placeholder",
    "model_root_required_error", "part_autoencoder", "part_text_encoder",
    "prompt_cache_busy_error", "prompt_cache_vl_refs_status", "prompt_cache_vl_refs_tooltip",
    "prompt_hint", "prompt_label", "prompt_required_error", "prompt_tooltip", "reference_cap_hint",
    "reference_cap_label", "reference_cap_off", "references_add_button",
    "references_add_disabled_tooltip", "references_add_tooltip", "references_clear_button",
    "references_clear_tooltip", "references_dialog_title", "references_files_filter",
    "references_loading_error", "references_remove_tooltip", "references_tooltip",
    "refresh_state_button", "refresh_state_hint", "residency_none", "residency_unloaded_status",
    "reuse_seed_button", "reuse_seed_tooltip", "seed_hint", "seed_invalid_error", "seed_label",
    "seed_random_hint", "source_mode_download", "source_mode_download_hint", "source_mode_label",
    "source_mode_manual", "source_mode_manual_hint", "steps_hint", "text_encoder_format_bf16_hint",
    "text_encoder_format_bf16_label", "text_encoder_format_gguf_hint", "text_encoder_format_hint",
    "text_encoder_format_label", "title", "unload_button", "unload_disabled_tooltip",
    "unload_done_status", "unload_hint", "unload_nothing_status", "unload_requested_status",
    "vl_refs_gguf_disabled_hint", "vl_refs_hint", "vl_refs_label",
    "switch_busy_reason", "sdcpp_files_checking_status", "sdcpp_files_ready_status",
];

/// The FULL keys of the shared «Постобработка» section this panel draws (owned by
/// `ms_region_postprocess`, outside the `cleaning.tools.flux3_klein.*` namespace).
pub(super) const POSTPROCESS_KEYS: [&str; 10] = [
    "cleaning.region_postprocess.section_title", "cleaning.region_postprocess.feather_label",
    "cleaning.region_postprocess.feather_hint", "cleaning.region_postprocess.color_match_label",
    "cleaning.region_postprocess.color_match_hint", "cleaning.region_postprocess.color_match_local_label",
    "cleaning.region_postprocess.color_match_local_hint", "cleaning.region_postprocess.color_match_scale_label",
    "cleaning.region_postprocess.color_match_scale_hint", "cleaning.tab.pixels_suffix",
];

/// Every `cleaning.tools.flux3_klein.<key>` with the placeholders the code fills.
pub(super) const FLUX3_PLACEHOLDER_KEYS: [(&str, &[&str]); 30] = [
    ("blob_length_error", &["{actual}", "{declared}"]),
    ("download_repo_status", &["{repo}"]),
    ("format_gguf_label", &["{quant}"]),
    ("format_sized_label", &["{format}", "{size}"]),
    ("last_seed_status", &["{seed}"]),
    ("malformed_answer_error", &["{field}"]),
    ("model_components_missing_error", &["{components}"]),
    ("model_missing_status", &["{components}"]),
    ("model_not_downloaded_error", &["{components}"]),
    ("model_ready_memory_status", &["{memory}"]),
    ("part_dit", &["{format}"]),
    ("part_text_encoder_format", &["{format}"]),
    ("reference_cap_side", &["{side}"]),
    ("reference_unreadable_error", &["{err}", "{name}"]),
    ("references_full_error", &["{count}", "{max}"]),
    ("references_heading", &["{count}", "{max}"]),
    ("residency_status", &["{dit}", "{encoder}"]),
    ("run_degraded_status", &["{ram}", "{seed}", "{vram}"]),
    ("run_done_status", &["{seed}"]),
    ("serialize_settings_error", &["{err}"]),
    ("settings_malformed_status", &["{path}"]),
    ("settings_save_blocked_error", &["{err}"]),
    ("settings_save_error", &["{err}"]),
    ("status_error", &["{err}"]),
    ("too_many_references_error", &["{max}"]),
    ("write_settings_error", &["{err}"]),
    ("sdcpp_format_fallback_status", &["{stored}", "{used}"]),
    ("sdcpp_files_error", &["{err}"]),
    ("sdcpp_seed_range_error", &["{max}"]),
    ("sdcpp_reference_error", &["{err}"]),
];

/// The retired `cleaning.tools.flux3_klein.<key>`s: the backend download's (its progress lines,
/// its engine-picker blocks — now the shared catalog block's — and the run-gate line a download
/// used to hold) and the ids whose text was reworded under a new id: absent from every catalog.
pub(super) const FLUX3_RETIRED_KEYS: [&str; 11] = [
    "download_cancelled_status", "download_done_status", "download_error", "download_running_status", "download_tooltip",
    "pipeline_busy_error", "switch_blocked_check", "switch_blocked_download",
    // Reworded under a new id (an install's on-disk catalog never overwrites an existing key):
    // `catalog_download_hint`, `dit_format_picker_hint`, `package_folder_hint`.
    "download_hint", "format_hint", "model_root_hint",
];
