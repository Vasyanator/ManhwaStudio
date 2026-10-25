/*
File: crates/ms-launcher/src/new_project/window/options.rs

Purpose:
Run-option collection for the New Project window: snapshots of the ribbon images handed to the
workers, the parsers that turn the typed option fields into stitch / waifu2x / Reline options,
and the `can_start_*` predicates.

Key items:
- `current_stitch_images`, `current_waifu2x_images`, `current_reline_images`, `current_save_images`.
- `parse_stitch_options`, `parse_waifu2x_options`, `parse_reline_options`,
  `build_reline_simple_options`, `parse_required_*` / `parse_optional_*` field parsers.
- `can_start_waifu2x`, `can_start_reline`, `can_start_image_processing`.

Notes:
A parse failure sets the window's error status and returns `None`; no fallback values.
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn parse_stitch_options(&mut self, mode: StitchSplitMode) -> Option<StitchOptions> {
        let parts = if self.stitch_parts.trim().is_empty() {
            None
        } else {
            match self.stitch_parts.trim().parse::<usize>() {
                Ok(value) if value > 0 => Some(value),
                _ => {
                    self.last_error =
                        Some(t!("launcher.new_project.stitch_k_positive_error").to_string());
                    self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                    return None;
                }
            }
        };

        let parse_positive = |value: &str, name: &str| -> Result<usize, String> {
            match value.trim().parse::<usize>() {
                Ok(parsed) if parsed > 0 => Ok(parsed),
                _ => Err(tf!("launcher.new_project.stitch_field_positive_error", name = name)),
            }
        };

        let target_height = match parse_positive(&self.stitch_target_height, "Hmax") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                return None;
            }
        };
        let band_rows = match parse_positive(&self.stitch_band_rows, "band_rows") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                return None;
            }
        };
        let search_radius = match parse_positive(&self.stitch_search_radius, "search_radius") {
            Ok(value) => value,
            Err(message) => {
                self.last_error = Some(message);
                self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                return None;
            }
        };
        let tolerance = match self.stitch_tolerance.trim().parse::<u8>() {
            Ok(value) if value > 0 => value,
            _ => {
                self.last_error = Some(t!("launcher.new_project.stitch_tol_positive_error").to_string());
                self.import_status = t!("launcher.new_project.stitch_invalid_params_error").to_string();
                return None;
            }
        };

        Some(StitchOptions {
            parts,
            target_height,
            band_rows,
            tolerance,
            search_radius,
            prefer_up_first: self.stitch_prefer_up,
            mode,
        })
    }

    pub(super) fn current_stitch_images(&self) -> Vec<StitchInputImage> {
        self.ribbon
            .pages()
            .iter()
            .map(|page| StitchInputImage {
                name: page.name.clone(),
                image: arc_image_clone(page.full_image()),
            })
            .collect()
    }

    pub(super) fn current_waifu2x_images(&self) -> Vec<Waifu2xInputImage> {
        self.ribbon
            .pages()
            .iter()
            .map(|page| Waifu2xInputImage {
                name: page.name.clone(),
                image: arc_image_clone(page.full_image()),
            })
            .collect()
    }

    pub(super) fn current_reline_images(&self) -> Vec<RelineInputImage> {
        self.ribbon
            .pages()
            .iter()
            .map(|page| RelineInputImage {
                name: page.name.clone(),
                image: arc_image_clone(page.full_image()),
            })
            .collect()
    }

    pub(super) fn current_save_images(&self) -> Vec<ProjectSaveImage> {
        self.ribbon
            .pages()
            .iter()
            .map(|page| ProjectSaveImage {
                image: arc_image_clone(page.full_image()),
            })
            .collect()
    }

    pub(super) fn source_import_options(&self) -> SourceImportOptions {
        SourceImportOptions {
            filter_same_width: self.filter_same_width,
            extra_name_patterns: self.import_extra_names.clone(),
        }
    }

    pub(super) fn parse_waifu2x_options(&mut self) -> Option<Waifu2xOptions> {
        let noise_levels = [-1, 0, 1, 2, 3];
        let scale_levels = [1, 2, 4, 8, 16, 32];
        let Some(&noise) = noise_levels.get(self.waifu_noise) else {
            self.last_error = Some(t!("launcher.new_project.waifu2x_invalid_noise_error").to_string());
            self.import_status = t!("launcher.new_project.waifu2x_invalid_params_error").to_string();
            return None;
        };
        let Some(&scale) = scale_levels.get(self.waifu_scale) else {
            self.last_error = Some(t!("launcher.new_project.waifu2x_invalid_scale_error").to_string());
            self.import_status = t!("launcher.new_project.waifu2x_invalid_params_error").to_string();
            return None;
        };
        let tile_size = match self.waifu_tile_size.trim().parse::<u32>() {
            Ok(value) if value == 0 || value >= 32 => value,
            _ => {
                self.last_error =
                    Some(t!("launcher.new_project.waifu2x_tile_size_error").to_string());
                self.import_status = t!("launcher.new_project.waifu2x_invalid_params_error").to_string();
                return None;
            }
        };
        Some(Waifu2xOptions {
            noise,
            scale,
            tile_size,
        })
    }

    pub(super) fn parse_reline_options(&mut self) -> Option<RelineOptions> {
        const READER_MODES: [&str; 3] = ["rgb", "gray", "dynamic"];
        const TILERS: [&str; 3] = ["exact", "max", "no_tiling"];
        const DTYPES: [&str; 3] = ["F32", "F16", "BF16"];
        const CANNY_TYPES: [&str; 3] = ["invert", "normal", "unsharp"];
        const DOT_TYPES: [&str; 5] = ["line", "cross", "ellipse", "invline", "circle"];
        const HALFTONE_MODES: [&str; 4] = ["gray", "rgb", "hsv", "cmyk"];
        const HALFTONE_FILTERS: [&str; 22] = [
            "nearest",
            "box",
            "sbox4",
            "sbox8",
            "linear",
            "slinear4",
            "slinear8",
            "hamming",
            "shamming4",
            "shamming8",
            "catmullrom",
            "scatmullrom4",
            "scatmullrom8",
            "mitchell",
            "smitchell4",
            "smitchell8",
            "lanczos",
            "slanczos4",
            "slanczos8",
            "gauss",
            "sgauss4",
            "sgauss8",
        ];
        const RESIZE_FILTERS: [&str; 33] = [
            "nearest",
            "box",
            "sbox4",
            "sbox8",
            "ibox",
            "linear",
            "slinear4",
            "slinear8",
            "ilinear",
            "hamming",
            "shamming4",
            "shamming8",
            "ihamming",
            "catmullrom",
            "scatmullrom4",
            "scatmullrom8",
            "icatmullrom",
            "mitchell",
            "smitchell4",
            "smitchell8",
            "imitchell",
            "lanczos",
            "slanczos4",
            "slanczos8",
            "ilanczos",
            "gauss",
            "sgauss4",
            "sgauss8",
            "igauss",
            "dpid_0.25",
            "dpid_0.5",
            "dpid_0.75",
            "dpid_1",
        ];
        const CVT_TYPES: [&str; 4] = ["RGB2Gray2020", "RGB2Gray709", "RGB2Gray", "Gray2RGB"];

        let reader_mode = selected_label(&READER_MODES, self.reline_reader_mode, t!("launcher.new_project.reline_field_read_mode"))?;
        let tiler = selected_label(&TILERS, self.reline_tiler, t!("launcher.new_project.reline_field_tiling"))?;
        let dtype = selected_label(&DTYPES, self.reline_dtype, t!("launcher.new_project.reline_field_compute_type"))?;
        let canny_type = selected_label(&CANNY_TYPES, self.reline_sharp_canny_type, t!("launcher.new_project.reline_field_canny_mode"))?;
        let dot_type = selected_label(&DOT_TYPES, self.reline_halftone_dot_type, t!("launcher.new_project.reline_field_dot_type"))?;
        let halftone_mode = selected_label(
            &HALFTONE_MODES,
            self.reline_halftone_mode,
            t!("launcher.new_project.reline_field_halftone_color_mode"),
        )?;
        let halftone_filter = selected_label(
            &HALFTONE_FILTERS,
            self.reline_halftone_ssaa_filter,
            t!("launcher.new_project.reline_field_ssaa_filter"),
        )?;
        let resize_filter = selected_label(
            &RESIZE_FILTERS,
            self.reline_resize_filter,
            t!("launcher.new_project.reline_field_resize_filter"),
        )?;
        let cvt_type = selected_label(
            &CVT_TYPES,
            self.reline_cvt_color_type,
            t!("launcher.new_project.reline_field_colorconv_type"),
        )?;

        let exact_tiler_size = self.parse_required_u32_field(
            &self.reline_exact_tiler_size.clone(),
            t!("launcher.new_project.reline_field_exact_tile"),
        )?;
        let target_scale =
            self.parse_optional_u32_field(&self.reline_target_scale.clone(), t!("launcher.new_project.reline_field_target_scale"))?;
        let sharp_low_input = self.parse_required_i32_field(
            &self.reline_sharp_low_input.clone(),
            t!("launcher.new_project.reline_field_sharpen_input_low"),
        )?;
        let sharp_high_input = self.parse_required_i32_field(
            &self.reline_sharp_high_input.clone(),
            t!("launcher.new_project.reline_field_sharpen_input_high"),
        )?;
        let sharp_gamma =
            self.parse_required_f32_field(&self.reline_sharp_gamma.clone(), t!("launcher.new_project.reline_field_sharpen_gamma"))?;
        let sharp_diapason_white = self.parse_required_i32_field(
            &self.reline_sharp_diapason_white.clone(),
            t!("launcher.new_project.reline_field_sharpen_white"),
        )?;
        let sharp_diapason_black = self.parse_required_i32_field(
            &self.reline_sharp_diapason_black.clone(),
            t!("launcher.new_project.reline_field_sharpen_black"),
        )?;
        let halftone_dot_size = self.parse_required_i32_field(
            &self.reline_halftone_dot_size.clone(),
            t!("launcher.new_project.reline_field_dot_size"),
        )?;
        let halftone_angle =
            self.parse_required_i32_field(&self.reline_halftone_angle.clone(), t!("launcher.new_project.reline_field_angle"))?;
        let halftone_ssaa_scale = self
            .parse_optional_f32_field(&self.reline_halftone_ssaa_scale.clone(), t!("launcher.new_project.reline_field_ssaa_scale"))?;
        let resize_height =
            self.parse_optional_u32_field(&self.reline_resize_height.clone(), t!("launcher.new_project.reline_height"))?;
        let resize_width =
            self.parse_optional_u32_field(&self.reline_resize_width.clone(), t!("launcher.new_project.reline_width"))?;
        let resize_percent =
            self.parse_optional_f32_field(&self.reline_resize_percent.clone(), t!("launcher.new_project.reline_percent"))?;
        if self.reline_resize_enabled
            && resize_height.is_none()
            && resize_width.is_none()
            && resize_percent.is_none()
        {
            self.last_error =
                Some(t!("launcher.new_project.reline_resize_requires_error").to_string());
            self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
            return None;
        }
        let resize_spread_size = self
            .parse_required_u32_field(&self.reline_resize_spread_size.clone(), t!("launcher.new_project.reline_spread_size"))?;
        let level_low_input = self.parse_required_i32_field(
            &self.reline_level_low_input.clone(),
            t!("launcher.new_project.reline_input_low_level"),
        )?;
        let level_high_input = self.parse_required_i32_field(
            &self.reline_level_high_input.clone(),
            t!("launcher.new_project.reline_input_high_level"),
        )?;
        let level_low_output = self.parse_required_i32_field(
            &self.reline_level_low_output.clone(),
            t!("launcher.new_project.reline_output_low_level"),
        )?;
        let level_high_output = self.parse_required_i32_field(
            &self.reline_level_high_output.clone(),
            t!("launcher.new_project.reline_output_high_level"),
        )?;
        let level_gamma =
            self.parse_required_f32_field(&self.reline_level_gamma.clone(), t!("launcher.new_project.reline_gamma"))?;

        Some(RelineOptions {
            reader_mode: reader_mode.to_string(),
            upscale: RelineUpscaleOptions {
                enabled: self.reline_upscale_enabled,
                model_name: self.reline_model_name.trim().to_string(),
                model_path: self.reline_model_path.trim().to_string(),
                model_url: self.reline_model_url.trim().to_string(),
                tiler: tiler.to_string(),
                target_scale,
                dtype: dtype.to_string(),
                exact_tiler_size,
                allow_cpu_upscale: self.reline_allow_cpu_upscale,
            },
            sharp: RelineSharpOptions {
                enabled: self.reline_sharp_enabled,
                low_input: sharp_low_input,
                high_input: sharp_high_input,
                gamma: sharp_gamma,
                diapason_white: sharp_diapason_white,
                diapason_black: sharp_diapason_black,
                canny: self.reline_sharp_canny,
                canny_type: canny_type.to_string(),
            },
            halftone: RelineHalftoneOptions {
                enabled: self.reline_halftone_enabled,
                dot_size: halftone_dot_size,
                angle: halftone_angle,
                dot_type: dot_type.to_string(),
                halftone_mode: halftone_mode.to_string(),
                ssaa_scale: halftone_ssaa_scale,
                ssaa_filter: halftone_filter.to_string(),
                disable_auto_dot: self.reline_halftone_disable_auto_dot,
            },
            resize: RelineResizeOptions {
                enabled: self.reline_resize_enabled,
                height: resize_height,
                width: resize_width,
                percent: resize_percent,
                filter: resize_filter.to_string(),
                gamma_correction: self.reline_resize_gamma_correction,
                spread: self.reline_resize_spread,
                spread_size: resize_spread_size,
            },
            level: RelineLevelOptions {
                enabled: self.reline_level_enabled,
                low_input: level_low_input,
                high_input: level_high_input,
                low_output: level_low_output,
                high_output: level_high_output,
                gamma: level_gamma,
            },
            cvt_color: RelineCvtColorOptions {
                enabled: self.reline_cvt_color_enabled,
                cvt_type: cvt_type.to_string(),
            },
        })
    }

    /// Build `RelineOptions` from the simplified UI state (preset + high-level controls).
    ///
    /// Maps the guided controls onto safe pipeline defaults: upscale always runs with the
    /// selected model; the preset decides whether sharpening and halftone nodes are added; the
    /// sharpness control governs Canny-based edge enhancement; level/cvt_color stay disabled.
    /// Returns `None` (and sets `last_error`) only when the optional resize field is invalid.
    pub(super) fn build_reline_simple_options(&mut self) -> Option<RelineOptions> {
        let preset = RelineSimplePreset::from_index(self.reline_simple_preset);

        // Target scale: 0 = model's native scale, 1 = ×2, 2 = ×4.
        let target_scale = match self.reline_simple_scale {
            1 => Some(2),
            2 => Some(4),
            _ => None,
        };

        // Sharpness strength: 0 = none, 1 = light (normal edges), 2 = strong (unsharp edges).
        let (sharp_node_enabled, canny, canny_type) = match self.reline_simple_sharp {
            1 => (true, true, "normal"),
            2 => (true, true, "unsharp"),
            _ => (false, false, "normal"),
        };

        // The preset decides which optional nodes participate; ModelOnly stays a clean pass.
        let (sharp_enabled, halftone_enabled) = match preset {
            RelineSimplePreset::ModelOnly => (false, false),
            RelineSimplePreset::RestoreLightSharp | RelineSimplePreset::Descreen => {
                (sharp_node_enabled, false)
            }
            RelineSimplePreset::AddHalftone => (sharp_node_enabled, true),
        };

        let resize_enabled = self.reline_simple_resize_enabled;
        let resize_height = if resize_enabled {
            Some(self.parse_required_u32_field(
                &self.reline_simple_resize_target.clone(),
                t!("launcher.new_project.reline_result_height"),
            )?)
        } else {
            None
        };

        Some(RelineOptions {
            reader_mode: "rgb".to_string(),
            upscale: RelineUpscaleOptions {
                enabled: true,
                model_name: self.reline_model_name.trim().to_string(),
                model_path: String::new(),
                model_url: String::new(),
                tiler: "exact".to_string(),
                target_scale,
                dtype: "F32".to_string(),
                exact_tiler_size: 800,
                allow_cpu_upscale: true,
            },
            sharp: RelineSharpOptions {
                enabled: sharp_enabled,
                low_input: 0,
                high_input: 255,
                gamma: 1.0,
                diapason_white: -1,
                diapason_black: -1,
                canny,
                canny_type: canny_type.to_string(),
            },
            halftone: RelineHalftoneOptions {
                enabled: halftone_enabled,
                dot_size: 7,
                angle: 0,
                dot_type: "circle".to_string(),
                halftone_mode: "gray".to_string(),
                ssaa_scale: None,
                ssaa_filter: "shamming4".to_string(),
                disable_auto_dot: false,
            },
            resize: RelineResizeOptions {
                enabled: resize_enabled,
                height: resize_height,
                width: None,
                percent: None,
                filter: "catmullrom".to_string(),
                gamma_correction: false,
                spread: false,
                spread_size: 2800,
            },
            level: RelineLevelOptions {
                enabled: false,
                low_input: 0,
                high_input: 255,
                low_output: 0,
                high_output: 255,
                gamma: 1.0,
            },
            cvt_color: RelineCvtColorOptions {
                enabled: false,
                cvt_type: "RGB2Gray2020".to_string(),
            },
        })
    }

    pub(super) fn can_start_waifu2x(&self) -> bool {
        !self.source_import.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.reline.is_loading()
            && !self.ribbon.pages().is_empty()
            && self.waifu2x.platform_supported()
    }

    pub(super) fn can_start_reline(&self) -> bool {
        // The simplified UI drives the model only through the gallery, so a model must be picked.
        let model_ready = match self.reline_ui_mode {
            RelineUiMode::Simple => !self.reline_model_name.trim().is_empty(),
            RelineUiMode::Full => true,
        };
        model_ready
            && !self.source_import.is_loading()
            && !self.advanced_download.is_loading()
            && !self.quick_download.is_loading()
            && !self.stitch.is_loading()
            && !self.save.is_loading()
            && !self.waifu2x.is_loading()
            && !self.reline.is_loading()
            && !self.ribbon.pages().is_empty()
    }

    pub(super) fn can_start_image_processing(&self) -> bool {
        match self.image_processor {
            ImageProcessor::Waifu2x => self.can_start_waifu2x(),
            ImageProcessor::Reline => self.can_start_reline(),
        }
    }

    pub(super) fn current_image_processing_operation(&self) -> &'static str {
        match self.image_processor {
            ImageProcessor::Waifu2x => "waifu2x",
            ImageProcessor::Reline => "reline",
        }
    }

    pub(super) fn parse_required_u32_field(&mut self, raw: &str, field_name: &str) -> Option<u32> {
        match raw.trim().parse::<u32>() {
            Ok(value) if value > 0 => Some(value),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_positive_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    pub(super) fn parse_optional_u32_field(&mut self, raw: &str, field_name: &str) -> Option<Option<u32>> {
        if raw.trim().is_empty() {
            return Some(None);
        }
        match raw.trim().parse::<u32>() {
            Ok(value) if value > 0 => Some(Some(value)),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_positive_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    pub(super) fn parse_required_i32_field(&mut self, raw: &str, field_name: &str) -> Option<i32> {
        match raw.trim().parse::<i32>() {
            Ok(value) => Some(value),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_integer_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    pub(super) fn parse_required_f32_field(&mut self, raw: &str, field_name: &str) -> Option<f32> {
        match raw.trim().parse::<f32>() {
            Ok(value) => Some(value),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_number_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    pub(super) fn parse_optional_f32_field(&mut self, raw: &str, field_name: &str) -> Option<Option<f32>> {
        if raw.trim().is_empty() {
            return Some(None);
        }
        match raw.trim().parse::<f32>() {
            Ok(value) if value > 0.0 => Some(Some(value)),
            _ => {
                self.last_error = Some(tf!("launcher.new_project.reline_field_positive_error", field_name = field_name));
                self.import_status = t!("launcher.new_project.reline_invalid_params_error").to_string();
                None
            }
        }
    }

    pub(super) fn waifu_backend_path_display(&self) -> String {
        self.waifu2x.backend_path().display().to_string()
    }
}

pub(super) fn arc_image_clone(image: Arc<RgbaImage>) -> RgbaImage {
    (*image).clone()
}
