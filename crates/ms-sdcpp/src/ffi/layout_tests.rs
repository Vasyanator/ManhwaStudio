/*
File: crates/ms-sdcpp/src/ffi/layout_tests.rs

Purpose:
Pins the `#[repr(C)]` mirrors of `types.rs` to the vendored header: the size, alignment and
every field offset of each mirrored struct, and the sha256 of `vendor/stable-diffusion.h`.

Notes:
- The constants were measured ONCE (2026-10-28) by compiling a C program that prints
  `sizeof` / `_Alignof` / `offsetof` for every field against `vendor/` with `gcc -m64 -std=c11`
  on x86_64 Linux, and confirmed identical for Windows by compiling `_Static_assert`s of the
  same values with `x86_64-w64-mingw32-gcc` (no value differed; C `bool` is 1 byte and C enums
  4 bytes on both). Never edit a constant to make a test pass: re-measure against a new header.
- Enum constant values (`SAMPLE_METHOD_COUNT` 21, `SCHEDULER_COUNT` 17, `SD_CANCEL_ALL` 0,
  `SD_LOG_INFO` 2, `SD_LOG_ERROR` 4) were checked the same way.
*/

use std::mem::{align_of, offset_of, size_of};

use sha2::Digest;

use super::types::*;

/// Asserts the size, alignment and field offsets of `T` against the C measurements.
fn check<T>(c_name: &str, size: usize, align: usize, fields: &[(&str, usize, usize)]) {
    assert_eq!(size_of::<T>(), size, "size of {c_name}");
    assert_eq!(align_of::<T>(), align, "alignment of {c_name}");
    for (field, rust, c) in fields {
        assert_eq!(rust, c, "offset of {c_name}.{field}");
    }
}

#[test]
fn mirrors_match_the_measured_c_layout() {
    check::<SdTilingParams>("sd_tiling_params_t", 32, 8, &[("enabled", offset_of!(SdTilingParams, enabled), 0), ("temporal_tiling", offset_of!(SdTilingParams, temporal_tiling), 1), ("tile_size_w", offset_of!(SdTilingParams, tile_size_w), 4), ("tile_size_h", offset_of!(SdTilingParams, tile_size_h), 8), ("target_overlap", offset_of!(SdTilingParams, target_overlap), 12), ("rel_size_w", offset_of!(SdTilingParams, rel_size_w), 16), ("rel_size_h", offset_of!(SdTilingParams, rel_size_h), 20), ("extra_tiling_args", offset_of!(SdTilingParams, extra_tiling_args), 24)]);
    check::<SdEmbedding>("sd_embedding_t", 16, 8, &[("name", offset_of!(SdEmbedding, name), 0), ("path", offset_of!(SdEmbedding, path), 8)]);
    check::<SdCtxParams>("sd_ctx_params_t", 336, 8, &[("model_path", offset_of!(SdCtxParams, model_path), 0), ("clip_l_path", offset_of!(SdCtxParams, clip_l_path), 8), ("clip_g_path", offset_of!(SdCtxParams, clip_g_path), 16), ("clip_vision_path", offset_of!(SdCtxParams, clip_vision_path), 24), ("t5xxl_path", offset_of!(SdCtxParams, t5xxl_path), 32), ("llm_path", offset_of!(SdCtxParams, llm_path), 40), ("llm_vision_path", offset_of!(SdCtxParams, llm_vision_path), 48), ("diffusion_model_path", offset_of!(SdCtxParams, diffusion_model_path), 56), ("high_noise_diffusion_model_path", offset_of!(SdCtxParams, high_noise_diffusion_model_path), 64), ("uncond_diffusion_model_path", offset_of!(SdCtxParams, uncond_diffusion_model_path), 72), ("embeddings_connectors_path", offset_of!(SdCtxParams, embeddings_connectors_path), 80), ("vae_path", offset_of!(SdCtxParams, vae_path), 88), ("audio_vae_path", offset_of!(SdCtxParams, audio_vae_path), 96), ("audio_encoder_path", offset_of!(SdCtxParams, audio_encoder_path), 104), ("taesd_path", offset_of!(SdCtxParams, taesd_path), 112), ("control_net_path", offset_of!(SdCtxParams, control_net_path), 120), ("ip_adapter_path", offset_of!(SdCtxParams, ip_adapter_path), 128), ("motion_module_path", offset_of!(SdCtxParams, motion_module_path), 136), ("embeddings", offset_of!(SdCtxParams, embeddings), 144), ("embedding_count", offset_of!(SdCtxParams, embedding_count), 152), ("photo_maker_path", offset_of!(SdCtxParams, photo_maker_path), 160), ("pulid_weights_path", offset_of!(SdCtxParams, pulid_weights_path), 168), ("tensor_type_rules", offset_of!(SdCtxParams, tensor_type_rules), 176), ("n_threads", offset_of!(SdCtxParams, n_threads), 184), ("wtype", offset_of!(SdCtxParams, wtype), 188), ("rng_type", offset_of!(SdCtxParams, rng_type), 192), ("sampler_rng_type", offset_of!(SdCtxParams, sampler_rng_type), 196), ("prediction", offset_of!(SdCtxParams, prediction), 200), ("lora_apply_mode", offset_of!(SdCtxParams, lora_apply_mode), 204), ("enable_mmap", offset_of!(SdCtxParams, enable_mmap), 208), ("flash_attn", offset_of!(SdCtxParams, flash_attn), 209), ("diffusion_flash_attn", offset_of!(SdCtxParams, diffusion_flash_attn), 210), ("tae_preview_only", offset_of!(SdCtxParams, tae_preview_only), 211), ("diffusion_conv_direct", offset_of!(SdCtxParams, diffusion_conv_direct), 212), ("vae_conv_direct", offset_of!(SdCtxParams, vae_conv_direct), 213), ("force_sdxl_vae_conv_scale", offset_of!(SdCtxParams, force_sdxl_vae_conv_scale), 214), ("vae_format", offset_of!(SdCtxParams, vae_format), 216), ("max_vram", offset_of!(SdCtxParams, max_vram), 224), ("disable_prefetch", offset_of!(SdCtxParams, disable_prefetch), 232), ("eager_load", offset_of!(SdCtxParams, eager_load), 233), ("backend", offset_of!(SdCtxParams, backend), 240), ("params_backend", offset_of!(SdCtxParams, params_backend), 248), ("split_mode", offset_of!(SdCtxParams, split_mode), 256), ("auto_fit", offset_of!(SdCtxParams, auto_fit), 264), ("rpc_servers", offset_of!(SdCtxParams, rpc_servers), 272), ("model_args", offset_of!(SdCtxParams, model_args), 280), ("disable_segmented_compute", offset_of!(SdCtxParams, disable_segmented_compute), 288), ("batched_cfg", offset_of!(SdCtxParams, batched_cfg), 289), ("linear_scale", offset_of!(SdCtxParams, linear_scale), 292), ("attn_scale", offset_of!(SdCtxParams, attn_scale), 296), ("tokenizer", offset_of!(SdCtxParams, tokenizer), 304), ("sage_attn", offset_of!(SdCtxParams, sage_attn), 312), ("conditioning_cache_size", offset_of!(SdCtxParams, conditioning_cache_size), 316), ("flash_attn_mode", offset_of!(SdCtxParams, flash_attn_mode), 320), ("conv_direct_mode", offset_of!(SdCtxParams, conv_direct_mode), 328)]);
    check::<SdImage>("sd_image_t", 24, 8, &[("width", offset_of!(SdImage, width), 0), ("height", offset_of!(SdImage, height), 4), ("channel", offset_of!(SdImage, channel), 8), ("data", offset_of!(SdImage, data), 16)]);
    check::<SdImageResult>("sd_image_result_t", 32, 8, &[("image", offset_of!(SdImageResult, image), 0), ("generation_info", offset_of!(SdImageResult, generation_info), 24)]);
    check::<SdImageGenerationResult>("sd_image_generation_result_t", 16, 8, &[("items", offset_of!(SdImageGenerationResult, items), 0), ("count", offset_of!(SdImageGenerationResult, count), 8)]);
    check::<SdImagePreprocessParams>("sd_image_preprocess_params_t", 8, 8, &[("rules", offset_of!(SdImagePreprocessParams, rules), 0)]);
    check::<SdSlgParams>("sd_slg_params_t", 32, 8, &[("layers", offset_of!(SdSlgParams, layers), 0), ("layer_count", offset_of!(SdSlgParams, layer_count), 8), ("layer_start", offset_of!(SdSlgParams, layer_start), 16), ("layer_end", offset_of!(SdSlgParams, layer_end), 20), ("scale", offset_of!(SdSlgParams, scale), 24)]);
    check::<SdGuidanceParams>("sd_guidance_params_t", 48, 8, &[("txt_cfg", offset_of!(SdGuidanceParams, txt_cfg), 0), ("img_cfg", offset_of!(SdGuidanceParams, img_cfg), 4), ("distilled_guidance", offset_of!(SdGuidanceParams, distilled_guidance), 8), ("slg", offset_of!(SdGuidanceParams, slg), 16)]);
    check::<SdSampleParams>("sd_sample_params_t", 96, 8, &[("guidance", offset_of!(SdSampleParams, guidance), 0), ("scheduler", offset_of!(SdSampleParams, scheduler), 48), ("sample_method", offset_of!(SdSampleParams, sample_method), 52), ("sample_steps", offset_of!(SdSampleParams, sample_steps), 56), ("eta", offset_of!(SdSampleParams, eta), 60), ("shifted_timestep", offset_of!(SdSampleParams, shifted_timestep), 64), ("custom_sigmas", offset_of!(SdSampleParams, custom_sigmas), 72), ("custom_sigmas_count", offset_of!(SdSampleParams, custom_sigmas_count), 80), ("flow_shift", offset_of!(SdSampleParams, flow_shift), 84), ("extra_sample_args", offset_of!(SdSampleParams, extra_sample_args), 88)]);
    check::<SdPmParams>("sd_pm_params_t", 32, 8, &[("id_images", offset_of!(SdPmParams, id_images), 0), ("id_images_count", offset_of!(SdPmParams, id_images_count), 8), ("id_embed_path", offset_of!(SdPmParams, id_embed_path), 16), ("style_strength", offset_of!(SdPmParams, style_strength), 24)]);
    check::<SdPulidParams>("sd_pulid_params_t", 16, 8, &[("id_embedding_path", offset_of!(SdPulidParams, id_embedding_path), 0), ("id_weight", offset_of!(SdPulidParams, id_weight), 8)]);
    check::<SdCacheParams>("sd_cache_params_t", 96, 8, &[("mode", offset_of!(SdCacheParams, mode), 0), ("reuse_threshold", offset_of!(SdCacheParams, reuse_threshold), 4), ("start_percent", offset_of!(SdCacheParams, start_percent), 8), ("end_percent", offset_of!(SdCacheParams, end_percent), 12), ("error_decay_rate", offset_of!(SdCacheParams, error_decay_rate), 16), ("use_relative_threshold", offset_of!(SdCacheParams, use_relative_threshold), 20), ("reset_error_on_compute", offset_of!(SdCacheParams, reset_error_on_compute), 21), ("fn_compute_blocks", offset_of!(SdCacheParams, fn_compute_blocks), 24), ("bn_compute_blocks", offset_of!(SdCacheParams, bn_compute_blocks), 28), ("residual_diff_threshold", offset_of!(SdCacheParams, residual_diff_threshold), 32), ("max_warmup_steps", offset_of!(SdCacheParams, max_warmup_steps), 36), ("max_cached_steps", offset_of!(SdCacheParams, max_cached_steps), 40), ("max_continuous_cached_steps", offset_of!(SdCacheParams, max_continuous_cached_steps), 44), ("taylorseer_n_derivatives", offset_of!(SdCacheParams, taylorseer_n_derivatives), 48), ("taylorseer_skip_interval", offset_of!(SdCacheParams, taylorseer_skip_interval), 52), ("scm_mask", offset_of!(SdCacheParams, scm_mask), 56), ("scm_policy_dynamic", offset_of!(SdCacheParams, scm_policy_dynamic), 64), ("spectrum_w", offset_of!(SdCacheParams, spectrum_w), 68), ("spectrum_m", offset_of!(SdCacheParams, spectrum_m), 72), ("spectrum_lam", offset_of!(SdCacheParams, spectrum_lam), 76), ("spectrum_window_size", offset_of!(SdCacheParams, spectrum_window_size), 80), ("spectrum_flex_window", offset_of!(SdCacheParams, spectrum_flex_window), 84), ("spectrum_warmup_steps", offset_of!(SdCacheParams, spectrum_warmup_steps), 88), ("spectrum_stop_percent", offset_of!(SdCacheParams, spectrum_stop_percent), 92)]);
    check::<SdLora>("sd_lora_t", 16, 8, &[("is_high_noise", offset_of!(SdLora, is_high_noise), 0), ("multiplier", offset_of!(SdLora, multiplier), 4), ("path", offset_of!(SdLora, path), 8)]);
    check::<SdHiresParams>("sd_hires_params_t", 56, 8, &[("enabled", offset_of!(SdHiresParams, enabled), 0), ("upscaler", offset_of!(SdHiresParams, upscaler), 4), ("model_path", offset_of!(SdHiresParams, model_path), 8), ("scale", offset_of!(SdHiresParams, scale), 16), ("target_width", offset_of!(SdHiresParams, target_width), 20), ("target_height", offset_of!(SdHiresParams, target_height), 24), ("steps", offset_of!(SdHiresParams, steps), 28), ("denoising_strength", offset_of!(SdHiresParams, denoising_strength), 32), ("upscale_tile_size", offset_of!(SdHiresParams, upscale_tile_size), 36), ("custom_sigmas", offset_of!(SdHiresParams, custom_sigmas), 40), ("custom_sigmas_count", offset_of!(SdHiresParams, custom_sigmas_count), 48)]);
    check::<SdImgGenParams>("sd_img_gen_params_t", 552, 8, &[("loras", offset_of!(SdImgGenParams, loras), 0), ("lora_count", offset_of!(SdImgGenParams, lora_count), 8), ("prompt", offset_of!(SdImgGenParams, prompt), 16), ("negative_prompt", offset_of!(SdImgGenParams, negative_prompt), 24), ("clip_skip", offset_of!(SdImgGenParams, clip_skip), 32), ("init_image", offset_of!(SdImgGenParams, init_image), 40), ("ref_images", offset_of!(SdImgGenParams, ref_images), 64), ("ref_images_count", offset_of!(SdImgGenParams, ref_images_count), 72), ("ref_image_args", offset_of!(SdImgGenParams, ref_image_args), 80), ("mask_image", offset_of!(SdImgGenParams, mask_image), 88), ("width", offset_of!(SdImgGenParams, width), 112), ("height", offset_of!(SdImgGenParams, height), 116), ("sample_params", offset_of!(SdImgGenParams, sample_params), 120), ("strength", offset_of!(SdImgGenParams, strength), 216), ("seed", offset_of!(SdImgGenParams, seed), 224), ("batch_count", offset_of!(SdImgGenParams, batch_count), 232), ("control_image", offset_of!(SdImgGenParams, control_image), 240), ("control_strength", offset_of!(SdImgGenParams, control_strength), 264), ("ip_adapter_image", offset_of!(SdImgGenParams, ip_adapter_image), 272), ("ip_adapter_strength", offset_of!(SdImgGenParams, ip_adapter_strength), 296), ("pm_params", offset_of!(SdImgGenParams, pm_params), 304), ("pulid_params", offset_of!(SdImgGenParams, pulid_params), 336), ("vae_tiling_params", offset_of!(SdImgGenParams, vae_tiling_params), 352), ("cache", offset_of!(SdImgGenParams, cache), 384), ("hires", offset_of!(SdImgGenParams, hires), 480), ("qwen_image_layers", offset_of!(SdImgGenParams, qwen_image_layers), 536), ("circular_x", offset_of!(SdImgGenParams, circular_x), 540), ("circular_y", offset_of!(SdImgGenParams, circular_y), 541), ("image_preprocess", offset_of!(SdImgGenParams, image_preprocess), 544)]);
}

#[test]
fn enum_constants_match_the_header() {
    assert_eq!(SampleMethod::COUNT.0, 21);
    assert_eq!(Scheduler::COUNT.0, 17);
    assert_eq!(SdCancelMode::ALL.0, 0);
    assert_eq!(SdLogLevel::INFO.0, 2);
    assert_eq!(SdLogLevel::ERROR.0, 4);
    assert_eq!(size_of::<SdType>(), 4);
}

#[test]
fn vendored_header_hash_is_pinned() {
    let header = include_bytes!("../../vendor/stable-diffusion.h");
    let digest: String = sha2::Sha256::digest(header).iter().flat_map(|byte| {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        [DIGITS[usize::from(byte >> 4)], DIGITS[usize::from(byte & 0x0f)]]
    }).map(char::from).collect();
    assert_eq!(digest, crate::catalog::PINNED.header_sha256);
}
