/*
File: crates/ms-onnx/src/paddle_ocr/mod.rs

Purpose:
Native PaddleOCR (PP-OCRv5) text detection + recognition over ONNX Runtime, in
pure Rust (no OpenCV/Clipper). Faithful port of the pipelines in
`modules/ai_backend/engines/paddle_onnx.py` (CTC decode, pre/post-processing).
The DB postprocess and the glyph mask are the engine-neutral detection domain and
live in `ms_text_detect::{db, glyph_mask}`; `detect` calls them.
`PaddleDetector::forward_prob_maps` is the forward-only entry of the tiled
text-detector pipeline: prepared tiles in, quantized `ms_text_detect::ProbMap`s
out; `ms-text-detect` plans, stitches and postprocesses around it.

Key structures:
- PaddleDetection : detector output (quads, axis-aligned blocks, glyph mask).
- PaddleLine      : one recognized line (text + mean-token confidence).
- PaddleDetector  : owns the detection session; `detect` (single 960 pass, used
                    by OCR) and `forward_prob_maps` (tiles -> u8 maps).
- PaddleRecognizer: owns the recognition session + character table.
- PaddleOcrEngine : composes both (`ocr.paddle`).

Submodules:
- preprocess     : detection/recognition input preprocessing.
- crop           : perspective crop + reading-order sort.
- ctc            : CTC greedy decode.
- dict           : character-table construction.

Notes:
Sessions are built through [`crate::OrtRuntime::build_session`], so the committed
execution provider is applied uniformly. Model input/output names are discovered
positionally (input[0]/output[0]) rather than hard-coded. The crate never
downloads or resolves models: the caller supplies model + dict paths.
*/

pub mod crop;
pub mod ctc;
pub mod dict;
pub mod preprocess;

use std::collections::BTreeMap;
use std::path::Path;

use image::{GrayImage, RgbImage, RgbaImage};
use ms_log::trace::cat;
use ort::session::Session;
use ort::value::{Shape, Tensor};

use crate::{OrtError, OrtRuntime};
use dict::CharacterTable;
use ms_text_detect::db::{block_from_quad, boxes_from_bitmap};
use ms_text_detect::glyph_mask::build_glyph_mask;
use ms_text_detect::ProbMap;

/// A detected text region: four corner points `[TL, TR, BR, BL]` in image pixels.
/// Owned by `ms-text-detect`; re-exported here so `paddle_ocr::Quad` keeps its path.
pub use ms_text_detect::Quad;

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

/// Native PaddleOCR text detector: the whole-page [`PaddleDetector::detect`] and the
/// forward-only [`PaddleDetector::forward_prob_maps`] of the tiled pipeline (the in-process
/// counterpart of the backend's `textdetector.paddle.forward` method).
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

        let quads = boxes_from_bitmap(prob, map_w, map_h, input.src_w, input.src_h);
        let blocks = quads.iter().map(|q| block_from_quad(q, input.src_w, input.src_h)).collect();
        let glyph_mask = build_glyph_mask(image, &quads);

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

    /// Runs the detection model on prepared tiles and returns one quantized DB
    /// probability map per tile, in order — the forward pass ONLY.
    ///
    /// No resize, no postprocess: the `ms-text-detect` plan has already scaled and
    /// padded the tiles, and it stitches the maps and runs the DB postprocess once on
    /// the whole page. Tiles are ImageNet-normalized exactly as [`Self::detect`] does
    /// (`preprocess::preprocess_det_tiles`) and run as ONE batch (the model's batch
    /// axis is dynamic). Each map has the tile's size and holds
    /// `floor(clamp(p, 0, 1) * 255 + 0.5)`, the same rule as the Python backend's
    /// `forward_maps.quantize_probability_maps`. An empty `tiles` slice returns an
    /// empty vector without running the model.
    ///
    /// Takes `&mut self` because `ort::session::Session::run` requires it.
    ///
    /// # Errors
    /// [`OrtError::TensorShape`] when the tiles are not all the same positive size
    /// with both sides a multiple of [`preprocess::DET_TILE_ALIGN`], when the output
    /// is not `[N, 1, H, W]` matching the batch, or when it holds a non-finite value;
    /// [`OrtError::Inference`] if the run fails.
    pub fn forward_prob_maps(&mut self, tiles: &[RgbImage]) -> Result<Vec<ProbMap>, OrtError> {
        if tiles.is_empty() {
            return Ok(Vec::new());
        }
        let batch = preprocess::preprocess_det_tiles(tiles)?;
        let batch_dim = i64::try_from(batch.count).map_err(|_| OrtError::TensorShape {
            detail: format!("детектор: размер пакета {} не помещается в i64", batch.count),
        })?;
        let shape = vec![batch_dim, 3_i64, i64::from(batch.height), i64::from(batch.width)];
        let tensor = Tensor::<f32>::from_array((shape, batch.data)).map_err(|e| {
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
        let maps = quantize_prob_maps(out_shape, data, batch.count, batch.width, batch.height)?;

        ms_log::trace_log!(
            cat::RENDER,
            "PaddleDetector forward done tiles={} tile={}x{}",
            batch.count,
            batch.width,
            batch.height
        );
        Ok(maps)
    }
}

/// Validates a DB detector output and quantizes it into one [`ProbMap`] per tile.
///
/// `dims` must be exactly `[count, 1, height, width]` and `data` exactly that long:
/// the DB head returns one probability plane the size of its 32-aligned input.
/// Each value becomes `floor(clamp(p, 0, 1) * 255 + 0.5)` computed in `f32` — the
/// rule of `modules/ai_backend/detection/forward_maps.py::quantize_probability_maps`,
/// mirrored operation for operation so the native and backend routes hand the
/// stitcher identical bytes for identical probabilities.
///
/// # Errors
/// [`OrtError::TensorShape`] on a shape or length mismatch or any NaN / infinite
/// value (a broken forward pass must not read as an empty map).
fn quantize_prob_maps(
    dims: &[i64],
    data: &[f32],
    count: usize,
    width: u32,
    height: u32,
) -> Result<Vec<ProbMap>, OrtError> {
    let expected = [
        i64::try_from(count).ok(),
        Some(1),
        Some(i64::from(height)),
        Some(i64::from(width)),
    ];
    if dims.len() != 4 || dims.iter().zip(expected).any(|(&dim, want)| Some(dim) != want) {
        return Err(OrtError::TensorShape {
            detail: format!(
                "детектор: ожидалась форма [{count}, 1, {height}, {width}], получено {dims:?}"
            ),
        });
    }
    let plane = usize::try_from(width)
        .ok()
        .zip(usize::try_from(height).ok())
        .and_then(|(w, h)| w.checked_mul(h))
        .ok_or_else(|| OrtError::TensorShape {
            detail: "детектор: переполнение размера карты".to_owned(),
        })?;
    if plane.checked_mul(count) != Some(data.len()) {
        return Err(OrtError::TensorShape {
            detail: format!(
                "детектор: карта вероятностей содержит {} значений, ожидалось {count}x{plane}",
                data.len()
            ),
        });
    }
    if let Some(bad) = data.iter().position(|p| !p.is_finite()) {
        return Err(OrtError::TensorShape {
            detail: format!("детектор: нечисловое значение вероятности в позиции {bad}"),
        });
    }

    data.chunks_exact(plane)
        .map(|chunk| {
            let bytes = chunk.iter().map(|&p| quantize_prob(p)).collect();
            ProbMap::new(width, height, bytes).map_err(|e| OrtError::TensorShape {
                detail: format!("детектор: некорректная карта вероятностей: {e}"),
            })
        })
        .collect()
}

/// Quantizes one FINITE probability: `floor(clamp(p, 0, 1) * 255 + 0.5)` in `f32`.
fn quantize_prob(p: f32) -> u8 {
    let level = (p.clamp(0.0, 1.0) * 255.0 + 0.5).floor();
    // `level` is an integer in [0, 255] for finite `p` (the clamp bounds it and the
    // caller rejected NaN/inf), so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = level as u8;
    out
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

    /// Mutable access to the detection stage (the native Paddle detection forward).
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
    fn conversions_saturate_and_truncate() {
        assert_eq!(nonneg_f32_to_u32(3.9), 3);
        assert_eq!(nonneg_f32_to_u32(-1.0), 0);
        assert_eq!(nonneg_f32_to_u32(f32::NAN), 0);
    }

    #[test]
    fn quantize_prob_clamps_and_rounds_half_up() {
        assert_eq!(quantize_prob(-0.5), 0);
        assert_eq!(quantize_prob(0.0), 0);
        assert_eq!(quantize_prob(1.0), 255);
        assert_eq!(quantize_prob(7.0), 255);
        // 0.5 * 255 = 127.5 -> half up -> 128.
        assert_eq!(quantize_prob(0.5), 128);
        // Just under one level's midpoint stays below it.
        assert_eq!(quantize_prob(0.3 / 255.0), 0);
        assert_eq!(quantize_prob(0.7 / 255.0), 1);
    }

    #[test]
    fn quantize_maps_splits_tiles_in_order() {
        // Two 2x1 tiles (width 2, height 1).
        let data = [0.0, 1.0, 0.5, 0.25];
        let maps = quantize_prob_maps(&[2, 1, 1, 2], &data, 2, 2, 1).expect("valid output");
        assert_eq!(maps.len(), 2);
        assert_eq!(maps[0].size(), [2, 1]);
        assert_eq!(maps[0].data(), &[0, 255]);
        assert_eq!(maps[1].data(), &[128, 64]);
    }

    #[test]
    fn quantize_maps_rejects_bad_shape_length_and_non_finite() {
        let is_shape = |r: Result<Vec<ProbMap>, OrtError>| matches!(r, Err(OrtError::TensorShape { .. }));
        let data = [0.1_f32; 4];
        // Wrong rank, batch, channel count and map size.
        assert!(is_shape(quantize_prob_maps(&[2, 1, 2], &data, 2, 2, 1)));
        assert!(is_shape(quantize_prob_maps(&[1, 1, 1, 2], &data, 2, 2, 1)));
        assert!(is_shape(quantize_prob_maps(&[2, 2, 1, 1], &data, 2, 1, 1)));
        assert!(is_shape(quantize_prob_maps(&[2, 1, 2, 1], &data, 2, 2, 1)));
        // Data shorter than the declared shape.
        assert!(is_shape(quantize_prob_maps(&[2, 1, 1, 2], &data[..3], 2, 2, 1)));
        // NaN and infinity are errors, never zeros.
        assert!(is_shape(quantize_prob_maps(&[1, 1, 1, 2], &[0.1, f32::NAN], 1, 2, 1)));
        assert!(is_shape(quantize_prob_maps(&[1, 1, 1, 2], &[f32::INFINITY, 0.1], 1, 2, 1)));
    }
}
