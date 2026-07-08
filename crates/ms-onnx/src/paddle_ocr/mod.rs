/*
File: crates/ms-onnx/src/paddle_ocr/mod.rs

Purpose:
Native PaddleOCR (PP-OCRv5) text detection + recognition over ONNX Runtime, in
pure Rust (no OpenCV/Clipper). Faithful port of the pipelines in
`modules/ai_backend/paddle_onnx_runtime.py` (DBPostProcess, CTC decode,
pre/post-processing) and `paddle_text_detector_service.py` (glyph mask).

Key structures:
- PaddleDetection : detector output (quads, axis-aligned blocks, glyph mask).
- PaddleLine      : one recognized line (text + mean-token confidence).
- PaddleDetector  : owns the detection session (`textdetector.paddle`).
- PaddleRecognizer: owns the recognition session + character table.
- PaddleOcrEngine : composes both (`ocr.paddle`).

Submodules:
- preprocess     : detection/recognition input preprocessing.
- db_postprocess : DB probability-map -> text quads.
- crop           : perspective crop + reading-order sort.
- ctc            : CTC greedy decode.
- dict           : character-table construction.
- glyph_mask     : glyph-shaped binary mask for the detector op.

Notes:
Sessions are built through [`crate::OrtRuntime::build_session`], so the committed
execution provider is applied uniformly. Model input/output names are discovered
positionally (input[0]/output[0]) rather than hard-coded. The crate never
downloads or resolves models: the caller supplies model + dict paths.
*/

pub mod crop;
pub mod ctc;
pub mod db_postprocess;
pub mod dict;
pub mod glyph_mask;
pub mod preprocess;

use std::collections::BTreeMap;
use std::path::Path;

use image::{GrayImage, RgbaImage};
use ms_log::trace::cat;
use ort::session::Session;
use ort::value::{Shape, Tensor};

use crate::{OrtError, OrtRuntime};
use dict::CharacterTable;

/// A detected text region: four corner points `[TL, TR, BR, BL]` in image pixels.
pub type Quad = [[f32; 2]; 4];

// --- Numeric conversion helpers (centralize the few unavoidable float<->int casts) ---

/// Lossless-in-practice `u32` -> `f32` for image dimensions and pixel counts.
///
/// All call sites pass image sizes / counts far below f32's 2^24 exact-integer
/// limit, so no precision is lost in practice.
#[must_use]
pub(crate) fn u32_to_f32(value: u32) -> f32 {
    // f32 cannot represent every u32 exactly, but our values stay < 2^24.
    #[allow(clippy::cast_precision_loss)]
    let out = value as f32;
    out
}

/// Truncates a non-negative, finite `f32` to `u32`, saturating out-of-range input.
///
/// Matches Python's `int(...)` for non-negative values (truncation toward zero).
/// NaN/negative map to 0; values above `u32::MAX` saturate.
#[must_use]
pub(crate) fn nonneg_f32_to_u32(value: f32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    if value >= u32_to_f32(u32::MAX) {
        return u32::MAX;
    }
    // Safe: finite and in [0, u32::MAX); truncation drops only the fraction.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = value as u32;
    out
}

/// Truncates a finite `f32` to `i32`, saturating out-of-range input.
///
/// Matches NumPy's `astype(np.int32)` / Python `int(...)` truncation toward zero.
#[must_use]
pub(crate) fn f32_to_i32_trunc(value: f32) -> i32 {
    if value.is_nan() {
        return 0;
    }
    let capped = value.clamp(u32_to_f32_i32_floor(), u32_to_f32_i32_ceil());
    // Safe: `capped` is finite and within i32 range; truncation drops the fraction.
    #[allow(clippy::cast_possible_truncation)]
    let out = capped as i32;
    out
}

/// `i32::MIN` as `f32` (lower clamp bound for [`f32_to_i32_trunc`]).
fn u32_to_f32_i32_floor() -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let out = i32::MIN as f32;
    out
}

/// `i32::MAX` as `f32` (upper clamp bound for [`f32_to_i32_trunc`]).
fn u32_to_f32_i32_ceil() -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let out = i32::MAX as f32;
    out
}

/// Result of `PaddleDetector::detect`: quads, axis-aligned blocks, and glyph mask.
#[derive(Debug, Clone)]
pub struct PaddleDetection {
    /// Original image size `(width, height)`.
    pub source_size: (u32, u32),
    /// Detected text quads `[TL, TR, BR, BL]` in original-image pixels.
    pub quads: Vec<Quad>,
    /// Axis-aligned bounding boxes of the quads as `[x1, y1, x2, y2]` (xyxy),
    /// clamped to the image. One per quad in the same order.
    pub blocks: Vec<[f32; 4]>,
    /// Glyph-shaped binary mask (0/255) at the original image size.
    pub glyph_mask: GrayImage,
}

/// One recognized text line: the decoded string and its mean-token confidence.
#[derive(Debug, Clone)]
pub struct PaddleLine {
    /// Decoded (already whitespace-trimmed) text; may be empty.
    pub text: String,
    /// Mean probability of the kept CTC tokens in `[0, 1]` (0.0 when empty).
    pub confidence: f32,
}

/// Native PaddleOCR text detector (`textdetector.paddle` op).
///
/// Owns the DB detection session. Construct with [`PaddleDetector::load`].
#[derive(Debug)]
pub struct PaddleDetector {
    /// Detection session: NCHW image -> `[1, 1, H, W]` DB probability map.
    session: Session,
    /// Discovered input name (index 0).
    input_name: String,
    /// Discovered output name (index 0).
    output_name: String,
}

impl PaddleDetector {
    /// Loads the detection model and builds its session for `runtime`'s provider.
    ///
    /// # Errors
    /// [`OrtError::SessionBuild`] if the model is missing/unreadable or the session
    /// (incl. execution-provider registration) cannot be built;
    /// [`OrtError::TensorShape`] if the model exposes no input/output.
    pub fn load(runtime: &OrtRuntime, det_model_path: &Path) -> Result<Self, OrtError> {
        ms_log::trace_log!(
            cat::STARTUP,
            "PaddleDetector load provider={} model={}",
            runtime.provider().id(),
            det_model_path.display()
        );
        let session = runtime.build_session(det_model_path)?;
        let input_name = nth_input_name(&session, 0, "paddle_det")?;
        let output_name = nth_output_name(&session, 0, "paddle_det")?;
        Ok(Self {
            session,
            input_name,
            output_name,
        })
    }

    /// Detects text regions in `image`, returning quads, blocks, and a glyph mask.
    ///
    /// Takes `&mut self` because `ort::session::Session::run` requires it.
    ///
    /// # Errors
    /// [`OrtError::ImagePreprocess`] on an empty image; [`OrtError::Inference`] if
    /// the detector run fails; [`OrtError::TensorShape`] on an unexpected output.
    pub fn detect(&mut self, image: &RgbaImage) -> Result<PaddleDetection, OrtError> {
        let input = preprocess::preprocess_det(image)?;
        let shape = vec![
            1_i64,
            3_i64,
            i64::from(input.model_h),
            i64::from(input.model_w),
        ];
        let tensor = Tensor::<f32>::from_array((shape, input.data.clone())).map_err(|e| {
            OrtError::TensorShape {
                detail: format!("не удалось создать тензор входа детектора: {e}"),
            }
        })?;

        let input_name = self.input_name.clone();
        let output_name = self.output_name.clone();
        let outputs = self
            .session
            .run(ort::inputs![input_name.as_str() => tensor])
            .map_err(|e| OrtError::Inference {
                stage: "paddle_det",
                reason: e.to_string(),
            })?;

        let value = outputs.get(output_name.as_str()).ok_or_else(|| OrtError::TensorShape {
            detail: format!("выход детектора «{output_name}» отсутствует"),
        })?;
        let (out_shape, data): (&Shape, &[f32]) =
            value.try_extract_tensor::<f32>().map_err(|e| OrtError::TensorShape {
                detail: format!("не удалось извлечь карту вероятностей детектора: {e}"),
            })?;
        let dims: &[i64] = out_shape;
        if dims.len() != 4 {
            return Err(OrtError::TensorShape {
                detail: format!("детектор: ожидалась форма [1, 1, H, W], получено {dims:?}"),
            });
        }
        let map_h = usize::try_from(dims[2]).map_err(|_| OrtError::TensorShape {
            detail: format!("детектор: некорректная высота карты {}", dims[2]),
        })?;
        let map_w = usize::try_from(dims[3]).map_err(|_| OrtError::TensorShape {
            detail: format!("детектор: некорректная ширина карты {}", dims[3]),
        })?;
        // Take the single prob plane (output[0, 0]); dims[0]=dims[1]=1 for DB.
        let plane = map_h.checked_mul(map_w).ok_or_else(|| OrtError::TensorShape {
            detail: "детектор: переполнение размера карты".to_owned(),
        })?;
        let prob = data.get(..plane).ok_or_else(|| OrtError::TensorShape {
            detail: "детектор: карта вероятностей короче ожидаемой".to_owned(),
        })?;

        let quads = db_postprocess::boxes_from_bitmap(prob, map_w, map_h, input.src_w, input.src_h);
        let blocks = quads.iter().map(|q| block_from_quad(q, input.src_w, input.src_h)).collect();
        let glyph_mask = glyph_mask::build_glyph_mask(image, &quads);

        ms_log::trace_log!(
            cat::RENDER,
            "PaddleDetector detect done src={}x{} map={}x{} quads={}",
            input.src_w,
            input.src_h,
            map_w,
            map_h,
            quads.len()
        );

        Ok(PaddleDetection {
            source_size: (input.src_w, input.src_h),
            quads,
            blocks,
            glyph_mask,
        })
    }
}

/// Native PaddleOCR text recognizer (`ocr.paddle` recognition stage).
///
/// Owns the recognition session and the character table. Construct with
/// [`PaddleRecognizer::load`].
#[derive(Debug)]
pub struct PaddleRecognizer {
    /// Recognition session: NCHW crop batch -> `[N, T, num_classes]` logits/probs.
    session: Session,
    /// Class-index -> character map (index 0 = CTC blank).
    table: CharacterTable,
    /// Discovered input name (index 0).
    input_name: String,
    /// Discovered output name (index 0).
    output_name: String,
}

impl PaddleRecognizer {
    /// Loads the recognition model + character dictionary and builds the session.
    ///
    /// # Errors
    /// [`OrtError::SessionBuild`] on a session/EP failure; [`OrtError::PaddleDictLoad`]
    /// if the dictionary cannot be read; [`OrtError::TensorShape`] if the model has
    /// no input/output.
    pub fn load(
        runtime: &OrtRuntime,
        rec_model_path: &Path,
        dict_path: &Path,
    ) -> Result<Self, OrtError> {
        ms_log::trace_log!(
            cat::STARTUP,
            "PaddleRecognizer load provider={} model={} dict={}",
            runtime.provider().id(),
            rec_model_path.display(),
            dict_path.display()
        );
        let session = runtime.build_session(rec_model_path)?;
        let table = CharacterTable::load(dict_path)?;
        let input_name = nth_input_name(&session, 0, "paddle_rec")?;
        let output_name = nth_output_name(&session, 0, "paddle_rec")?;
        Ok(Self {
            session,
            table,
            input_name,
            output_name,
        })
    }

    /// Number of recognizer classes (== the character-table length).
    #[must_use]
    pub fn num_classes(&self) -> usize {
        self.table.len()
    }

    /// Recognizes a batch of pre-cropped text-line images, preserving input order.
    ///
    /// Crops are grouped by their planned dynamic width, batched, run, and decoded;
    /// results are returned one-per-crop in the original order. Text is trimmed of
    /// surrounding whitespace (may be empty).
    ///
    /// # Errors
    /// [`OrtError::ImagePreprocess`] if a crop cannot be preprocessed;
    /// [`OrtError::Inference`] on a run failure; [`OrtError::TensorShape`] on an
    /// unexpected output shape or a class-count mismatch with the dictionary.
    pub fn recognize_crops(&mut self, crops: &[RgbaImage]) -> Result<Vec<PaddleLine>, OrtError> {
        let mut lines = vec![
            PaddleLine {
                text: String::new(),
                confidence: 0.0,
            };
            crops.len()
        ];
        if crops.is_empty() {
            return Ok(lines);
        }

        // Group crop indices by their planned batch width (no MIGraphX bucketing:
        // the width is used directly, matching the non-MIGraphX Python path).
        let mut groups: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
        for (idx, crop) in crops.iter().enumerate() {
            let (cw, ch) = crop.dimensions();
            let width = preprocess::plan_rec_width(cw, ch);
            groups.entry(width).or_default().push(idx);
        }

        for (width, indices) in groups {
            self.run_group(crops, &indices, width, &mut lines)?;
        }
        Ok(lines)
    }

    /// Runs one same-width crop group as a single batch and fills `lines` by index.
    fn run_group(
        &mut self,
        crops: &[RgbaImage],
        indices: &[usize],
        width: u32,
        lines: &mut [PaddleLine],
    ) -> Result<(), OrtError> {
        let batch = indices.len();
        let width_usize = usize::try_from(width).map_err(|_| OrtError::TensorShape {
            detail: "распознаватель: ширина не помещается в usize".to_owned(),
        })?;
        let height_usize = usize::try_from(preprocess::REC_HEIGHT).unwrap_or(48);
        let per_image = 3 * height_usize * width_usize;

        let mut data = Vec::with_capacity(batch * per_image);
        for &idx in indices {
            let plane = preprocess::preprocess_rec(&crops[idx], width)?;
            data.extend_from_slice(&plane);
        }

        let shape = vec![
            i64::try_from(batch).map_err(|_| OrtError::TensorShape {
                detail: "распознаватель: размер батча не помещается в i64".to_owned(),
            })?,
            3_i64,
            i64::from(preprocess::REC_HEIGHT),
            i64::from(width),
        ];
        let tensor = Tensor::<f32>::from_array((shape, data)).map_err(|e| OrtError::TensorShape {
            detail: format!("не удалось создать тензор входа распознавателя: {e}"),
        })?;

        let input_name = self.input_name.clone();
        let output_name = self.output_name.clone();
        let outputs = self
            .session
            .run(ort::inputs![input_name.as_str() => tensor])
            .map_err(|e| OrtError::Inference {
                stage: "paddle_rec",
                reason: e.to_string(),
            })?;

        let value = outputs.get(output_name.as_str()).ok_or_else(|| OrtError::TensorShape {
            detail: format!("выход распознавателя «{output_name}» отсутствует"),
        })?;
        let (out_shape, out_data): (&Shape, &[f32]) =
            value.try_extract_tensor::<f32>().map_err(|e| OrtError::TensorShape {
                detail: format!("не удалось извлечь логиты распознавателя: {e}"),
            })?;
        let dims: &[i64] = out_shape;
        if dims.len() != 3 {
            return Err(OrtError::TensorShape {
                detail: format!("распознаватель: ожидалась форма [N, T, C], получено {dims:?}"),
            });
        }
        let time_steps = usize::try_from(dims[1]).map_err(|_| OrtError::TensorShape {
            detail: format!("распознаватель: некорректное число шагов {}", dims[1]),
        })?;
        let num_classes = usize::try_from(dims[2]).map_err(|_| OrtError::TensorShape {
            detail: format!("распознаватель: некорректное число классов {}", dims[2]),
        })?;
        if num_classes != self.table.len() {
            return Err(OrtError::TensorShape {
                detail: format!(
                    "распознаватель: число классов {num_classes} не совпадает со словарём {}",
                    self.table.len()
                ),
            });
        }

        // Copy the output so softmax can normalize in place if the export emitted
        // raw logits (Python softmaxes only when values fall outside [0, 1]).
        let mut buffer = out_data.to_vec();
        if ctc::needs_softmax(&buffer) {
            ctc::softmax_rows(&mut buffer, num_classes);
        }

        let per_sample = time_steps.checked_mul(num_classes).ok_or_else(|| OrtError::TensorShape {
            detail: "распознаватель: переполнение размера образца".to_owned(),
        })?;
        for (local, &idx) in indices.iter().enumerate() {
            let start = local * per_sample;
            let sample = buffer.get(start..start + per_sample).ok_or_else(|| OrtError::TensorShape {
                detail: "распознаватель: срез образца вне диапазона".to_owned(),
            })?;
            let (text, confidence) = ctc::decode_greedy(sample, time_steps, num_classes, &self.table);
            lines[idx] = PaddleLine {
                text: text.trim().to_owned(),
                confidence,
            };
        }
        Ok(())
    }
}

/// Full native PaddleOCR engine: detection + recognition (`ocr.paddle` op).
///
/// Construct with [`PaddleOcrEngine::load`]; call [`PaddleOcrEngine::recognize`]
/// for the end-to-end pipeline. The [`PaddleOcrEngine::detector`] /
/// [`PaddleOcrEngine::recognizer`] accessors expose each stage standalone.
#[derive(Debug)]
pub struct PaddleOcrEngine {
    /// Detection stage.
    detector: PaddleDetector,
    /// Recognition stage.
    recognizer: PaddleRecognizer,
}

impl PaddleOcrEngine {
    /// Loads both detection and recognition models plus the character dictionary.
    ///
    /// # Errors
    /// Propagates [`PaddleDetector::load`] / [`PaddleRecognizer::load`] errors.
    pub fn load(
        runtime: &OrtRuntime,
        det_model_path: &Path,
        rec_model_path: &Path,
        dict_path: &Path,
    ) -> Result<Self, OrtError> {
        let detector = PaddleDetector::load(runtime, det_model_path)?;
        let recognizer = PaddleRecognizer::load(runtime, rec_model_path, dict_path)?;
        Ok(Self {
            detector,
            recognizer,
        })
    }

    /// Mutable access to the detection stage (for the `textdetector.paddle` op).
    pub fn detector(&mut self) -> &mut PaddleDetector {
        &mut self.detector
    }

    /// Mutable access to the recognition stage.
    pub fn recognizer(&mut self) -> &mut PaddleRecognizer {
        &mut self.recognizer
    }

    /// Runs the full detect -> crop -> recognize pipeline, returning ordered text.
    ///
    /// Detects text quads, crops them in reading order, recognizes each, and drops
    /// empty results. Returns the non-empty recognized lines top-to-bottom.
    ///
    /// Delegates to the free [`paddle_recognize`] function so the pipeline has a
    /// single source of truth shared with callers that own detector/recognizer
    /// sessions separately.
    ///
    /// # Errors
    /// Propagates detection ([`PaddleDetector::detect`]) and recognition
    /// ([`PaddleRecognizer::recognize_crops`]) errors.
    pub fn recognize(&mut self, image: &RgbaImage) -> Result<Vec<String>, OrtError> {
        paddle_recognize(&mut self.detector, &mut self.recognizer, image)
    }
}

/// Runs the full detect -> crop -> recognize pipeline over borrowed sessions.
///
/// Detects text quads with `detector`, crops them in reading order
/// (top-to-bottom, left-to-right), recognizes each crop with `recognizer`, and
/// drops empty results. Returns the non-empty recognized lines in reading order.
///
/// This is the single source of truth for the PaddleOCR end-to-end pipeline.
/// Taking the detector and recognizer by `&mut` (rather than owning them, as
/// [`PaddleOcrEngine`] does) lets a caller share ONE [`PaddleDetector`] across
/// many [`PaddleRecognizer`]s — e.g. one detector session reused for every
/// PaddleOCR language and the standalone text-detector op. Both are `&mut`
/// because [`ort::session::Session::run`] requires it.
///
/// # Errors
/// Propagates detection ([`PaddleDetector::detect`]) and recognition
/// ([`PaddleRecognizer::recognize_crops`]) errors.
pub fn paddle_recognize(
    detector: &mut PaddleDetector,
    recognizer: &mut PaddleRecognizer,
    image: &RgbaImage,
) -> Result<Vec<String>, OrtError> {
    let detection = detector.detect(image)?;
    // Reading order: top-to-bottom, left-to-right.
    let order = crop::sort_quad_indices(&detection.quads);

    let mut crops = Vec::with_capacity(order.len());
    for &idx in &order {
        let Some(crop) = crop::rotate_crop(image, &detection.quads[idx]) else {
            continue;
        };
        // Skip degenerate crops (Python drops crops smaller than 2x2).
        let (cw, ch) = crop.dimensions();
        if cw < 2 || ch < 2 {
            continue;
        }
        crops.push(crop);
    }

    let lines = recognizer.recognize_crops(&crops)?;
    Ok(lines.into_iter().map(|line| line.text).filter(|t| !t.is_empty()).collect())
}

/// Axis-aligned `[x1, y1, x2, y2]` bounding box of a quad, clamped to the image.
fn block_from_quad(quad: &Quad, img_w: u32, img_h: u32) -> [f32; 4] {
    let xs = [quad[0][0], quad[1][0], quad[2][0], quad[3][0]];
    let ys = [quad[0][1], quad[1][1], quad[2][1], quad[3][1]];
    let w = u32_to_f32(img_w);
    let h = u32_to_f32(img_h);
    [
        xs.iter().copied().fold(f32::INFINITY, f32::min).clamp(0.0, w),
        ys.iter().copied().fold(f32::INFINITY, f32::min).clamp(0.0, h),
        xs.iter().copied().fold(f32::NEG_INFINITY, f32::max).clamp(0.0, w),
        ys.iter().copied().fold(f32::NEG_INFINITY, f32::max).clamp(0.0, h),
    ]
}

/// Returns the name of the session's `index`-th input, or a typed shape error.
fn nth_input_name(session: &Session, index: usize, stage: &str) -> Result<String, OrtError> {
    session
        .inputs()
        .get(index)
        .map(|outlet| outlet.name().to_owned())
        .ok_or_else(|| OrtError::TensorShape {
            detail: format!("{stage}: отсутствует вход #{index}"),
        })
}

/// Returns the name of the session's `index`-th output, or a typed shape error.
fn nth_output_name(session: &Session, index: usize, stage: &str) -> Result<String, OrtError> {
    session
        .outputs()
        .get(index)
        .map(|outlet| outlet.name().to_owned())
        .ok_or_else(|| OrtError::TensorShape {
            detail: format!("{stage}: отсутствует выход #{index}"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_from_quad_is_clamped_bbox() {
        // Quad partly outside the image: bbox clamps to [0, dims].
        let quad: Quad = [[-5.0, 10.0], [30.0, 8.0], [32.0, 40.0], [-2.0, 42.0]];
        let block = block_from_quad(&quad, 25, 35);
        let expected = [0.0_f32, 8.0, 25.0, 35.0];
        for (got, want) in block.iter().zip(expected.iter()) {
            assert!((got - want).abs() < 1e-6, "block {block:?} != {expected:?}");
        }
    }

    #[test]
    fn conversions_saturate_and_truncate() {
        assert_eq!(nonneg_f32_to_u32(3.9), 3);
        assert_eq!(nonneg_f32_to_u32(-1.0), 0);
        assert_eq!(nonneg_f32_to_u32(f32::NAN), 0);
        assert_eq!(f32_to_i32_trunc(-2.7), -2);
        assert_eq!(f32_to_i32_trunc(2.7), 2);
    }
}
