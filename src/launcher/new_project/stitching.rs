/*
File: src/launcher/new_project/stitching.rs

Purpose:
Background stitch/split pipeline for the New Project launcher window.

Main responsibilities:
- port the legacy Python stitch/split flow from `modules/new_project/stitching.py`;
- merge vertically contiguous pages, compute recommended cut positions, and support manual cut apply;
- selectively merge adjacent pages whose bottom edge does not look like a safe cut band;
- keep long-running image processing off the GUI thread.

Key structures:
- StitchController
- StitchRequest
- StitchOptions
- StitchEvent

Notes:
The implementation intentionally follows the old Python heuristics, but uses lightweight
Rust image processing primitives instead of OpenCV/Numpy.
*/

use crate::launcher::new_project::ribbon::{ImportedImage, RibbonPage, build_ribbon_pages};
use image::imageops::{FilterType, overlay, resize};
use image::{GrayImage, ImageBuffer, Luma, Rgba, RgbaImage};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

const SAFE_ROWS_STRIDE: usize = 32;
const SEARCH_DELTA: usize = 3000;
const MATCH_WIDTH: u32 = 256;
const STITCH_SEGMENT_NAME_WIDTH: usize = 3;

#[derive(Clone)]
pub struct StitchInputImage {
    pub name: String,
    pub image: RgbaImage,
}

#[derive(Clone, Copy)]
pub struct StitchOptions {
    pub parts: Option<usize>,
    pub target_height: usize,
    pub band_rows: usize,
    pub tolerance: u8,
    pub search_radius: usize,
    pub prefer_up_first: bool,
    pub mode: StitchSplitMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StitchSplitMode {
    StitchOnly,
    ManualCutPreview,
    AutoCut,
    HeterogeneousBottoms,
}

pub enum StitchRequest {
    StitchSplit {
        images: Vec<StitchInputImage>,
        options: StitchOptions,
    },
    CutLikeReference {
        images: Vec<StitchInputImage>,
        reference_dir: PathBuf,
    },
    ApplyManualCutsToPages {
        images: Vec<StitchInputImage>,
        cut_guides: Vec<ManualCutGuide>,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct ManualCutGuide {
    pub page_index: usize,
    pub y: usize,
}

#[derive(Debug)]
struct PendingStitch {
    rx: Receiver<StitchWorkerEvent>,
}

pub struct StitchController {
    pending: Option<PendingStitch>,
}

pub enum StitchSuccessKind {
    AutoCut,
    ReferenceCut,
    ManualPreview,
    ManualApply,
    StitchOnly,
    HeterogeneousBottoms,
}

pub struct StitchSuccess {
    pub pages: Vec<RibbonPage>,
    pub cut_guides: Vec<ManualCutGuide>,
    pub kind: StitchSuccessKind,
}

pub enum StitchEvent {
    Progress {
        stage: String,
        current: usize,
        total: usize,
    },
    Completed(StitchSuccess),
    Failed {
        user_message: String,
        log_message: String,
    },
    WorkerDisconnected,
}

#[derive(Debug)]
struct StitchError {
    user_message: String,
    log_message: String,
}

enum StitchWorkerEvent {
    Progress {
        stage: &'static str,
        current: usize,
        total: usize,
    },
    Finished(Result<StitchSuccess, StitchError>),
}

impl StitchController {
    pub fn new() -> Self {
        Self { pending: None }
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn begin(&mut self, request: StitchRequest) {
        self.pending = Some(PendingStitch {
            rx: spawn_stitch_worker(request),
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) -> Option<StitchEvent> {
        let pending = self.pending.take()?;
        let mut last_progress = None;
        loop {
            match pending.rx.try_recv() {
                Ok(StitchWorkerEvent::Progress {
                    stage,
                    current,
                    total,
                }) => {
                    ctx.request_repaint();
                    last_progress = Some(StitchEvent::Progress {
                        stage: stage.to_string(),
                        current,
                        total,
                    });
                }
                Ok(StitchWorkerEvent::Finished(result)) => match result {
                    Ok(success) => {
                        ctx.request_repaint();
                        return Some(StitchEvent::Completed(success));
                    }
                    Err(err) => {
                        return Some(StitchEvent::Failed {
                            user_message: err.user_message,
                            log_message: err.log_message,
                        });
                    }
                },
                Err(mpsc::TryRecvError::Empty) => {
                    self.pending = Some(pending);
                    return last_progress;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Some(StitchEvent::WorkerDisconnected);
                }
            }
        }
    }
}

fn spawn_stitch_worker(request: StitchRequest) -> Receiver<StitchWorkerEvent> {
    let (tx, rx) = mpsc::channel();
    let tx_worker = tx.clone();
    match thread::Builder::new()
        .name("new-project-stitch".to_string())
        .spawn(move || {
            let result = run_stitch_request(request, &tx_worker);
            if tx_worker.send(StitchWorkerEvent::Finished(result)).is_err() {
                crate::runtime_log::log_warn("[new-project] failed to send stitch result to UI");
            }
        }) {
        Ok(_) => {}
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "[new-project] failed to spawn stitch worker: {err}"
            ));
            if tx
                .send(StitchWorkerEvent::Finished(Err(StitchError {
                    user_message: "Не удалось запустить склейку/нарезку.".to_string(),
                    log_message: format!("failed to spawn stitch worker: {err}"),
                })))
                .is_err()
            {
                crate::runtime_log::log_warn("[new-project] failed to deliver stitch spawn error");
            }
        }
    }
    rx
}

fn run_stitch_request(
    request: StitchRequest,
    progress_tx: &Sender<StitchWorkerEvent>,
) -> Result<StitchSuccess, StitchError> {
    match request {
        StitchRequest::StitchSplit { images, options } => {
            stitch_split(images, options, progress_tx)
        }
        StitchRequest::CutLikeReference {
            images,
            reference_dir,
        } => cut_like_reference(images, &reference_dir, progress_tx),
        StitchRequest::ApplyManualCutsToPages { images, cut_guides } => {
            apply_manual_cuts_to_pages(images, cut_guides, progress_tx)
        }
    }
}

fn stitch_split(
    images: Vec<StitchInputImage>,
    options: StitchOptions,
    progress_tx: &Sender<StitchWorkerEvent>,
) -> Result<StitchSuccess, StitchError> {
    if images.is_empty() {
        return Err(stitch_error(
            "Сначала откройте папку или скачайте главу.",
            "stitch requested with zero images",
        ));
    }

    let names = images
        .iter()
        .map(|image| image.name.clone())
        .collect::<Vec<_>>();
    let rgba_images = images
        .into_iter()
        .map(|image| image.image)
        .collect::<Vec<_>>();
    if options.mode == StitchSplitMode::HeterogeneousBottoms {
        return stitch_heterogeneous_bottoms(names, rgba_images, options, progress_tx);
    }

    let (tape, cuts) =
        build_stitched_tape_and_cuts(&rgba_images, options, progress_tx).map_err(|err| {
            stitch_error(
                "Не удалось выполнить склейку и нарезку.",
                format!("stitch_split failed: {err}"),
            )
        })?;

    match options.mode {
        StitchSplitMode::AutoCut => {
            let _ = send_stitch_progress(progress_tx, "split", 0, 1);
            let segments = split_tape_by_cuts(&tape, &cuts).map_err(|err| {
                stitch_error(
                    "Не удалось нарезать склеенную ленту.",
                    format!("split_tape_by_cuts failed: {err}"),
                )
            })?;
            let imported = segments
                .into_iter()
                .enumerate()
                .map(|(index, image)| ImportedImage {
                    name: names
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| format_stitch_segment_name(index)),
                    image: image::DynamicImage::ImageRgba8(image),
                })
                .collect::<Vec<_>>();
            let _ = send_stitch_progress(progress_tx, "preview", 0, imported.len().max(1));
            Ok(StitchSuccess {
                pages: build_ribbon_pages(imported),
                cut_guides: Vec::new(),
                kind: StitchSuccessKind::AutoCut,
            })
        }
        StitchSplitMode::ManualCutPreview | StitchSplitMode::StitchOnly => {
            let tape_name = names
                .first()
                .map(|name| format!("stitched-{name}"))
                .unwrap_or_else(|| "stitched-tape.png".to_string());
            let _ = send_stitch_progress(progress_tx, "preview", 0, 1);
            let cut_guides = if options.mode == StitchSplitMode::ManualCutPreview {
                cuts.into_iter()
                    .skip(1)
                    .take_while(|cut| *cut > 0)
                    .map(|y| ManualCutGuide { page_index: 0, y })
                    .collect()
            } else {
                Vec::new()
            };
            Ok(StitchSuccess {
                pages: build_ribbon_pages(vec![ImportedImage {
                    name: tape_name,
                    image: image::DynamicImage::ImageRgba8(tape),
                }]),
                cut_guides,
                kind: if options.mode == StitchSplitMode::ManualCutPreview {
                    StitchSuccessKind::ManualPreview
                } else {
                    StitchSuccessKind::StitchOnly
                },
            })
        }
        StitchSplitMode::HeterogeneousBottoms => unreachable!("handled before full stitch"),
    }
}

fn stitch_heterogeneous_bottoms(
    names: Vec<String>,
    images: Vec<RgbaImage>,
    options: StitchOptions,
    progress_tx: &Sender<StitchWorkerEvent>,
) -> Result<StitchSuccess, StitchError> {
    if images.is_empty() {
        return Err(stitch_error(
            "Сначала откройте папку или скачайте главу.",
            "heterogeneous-bottom stitch requested with zero images",
        ));
    }

    let _ = send_stitch_progress(progress_tx, "normalize", 0, images.len().max(1));
    let (images, _) = unify_widths(&images);
    let _ = send_stitch_progress(progress_tx, "normalize", images.len(), images.len().max(1));

    let _ = send_stitch_progress(progress_tx, "stitch", 0, images.len().max(1));
    let mut pages = Vec::new();
    let mut index = 0usize;
    while index < images.len() {
        let start_index = index;
        let mut current = images[index].clone();
        index += 1;

        while index < images.len()
            && bottom_is_heterogeneous_cut_area(&current, options.band_rows, options.tolerance)
        {
            current = stitch_two_pages_without_width_normalization(&current, &images[index]);
            index += 1;
        }

        pages.push(ImportedImage {
            name: names
                .get(start_index)
                .map(|name| format!("stitched-{name}"))
                .unwrap_or_else(|| format_stitch_segment_name(start_index)),
            image: image::DynamicImage::ImageRgba8(current),
        });
        let _ = send_stitch_progress(progress_tx, "stitch", index, images.len().max(1));
    }

    let _ = send_stitch_progress(progress_tx, "preview", 0, pages.len().max(1));
    Ok(StitchSuccess {
        pages: build_ribbon_pages(pages),
        cut_guides: Vec::new(),
        kind: StitchSuccessKind::HeterogeneousBottoms,
    })
}

fn apply_manual_cuts_to_pages(
    images: Vec<StitchInputImage>,
    cut_guides: Vec<ManualCutGuide>,
    progress_tx: &Sender<StitchWorkerEvent>,
) -> Result<StitchSuccess, StitchError> {
    let _ = send_stitch_progress(progress_tx, "split", 0, images.len().max(1));
    let mut guides_by_page: HashMap<usize, Vec<usize>> = HashMap::new();
    for guide in cut_guides {
        guides_by_page
            .entry(guide.page_index)
            .or_default()
            .push(guide.y);
    }

    let mut imported = Vec::new();
    for (page_index, image) in images.into_iter().enumerate() {
        let tape_height = image.image.height() as usize;
        let mut cuts = vec![0usize];
        if let Some(page_guides) = guides_by_page.remove(&page_index) {
            cuts.extend(
                page_guides
                    .into_iter()
                    .filter(|cut| *cut > 0 && *cut < tape_height),
            );
        }
        cuts.sort_unstable();
        cuts.dedup();
        cuts.push(tape_height);
        let segments = split_tape_by_cuts(&image.image, &cuts).map_err(|err| {
            stitch_error(
                "Не удалось нарезать ленту по выбранным линиям.",
                format!(
                    "apply_manual_cuts_to_pages failed on page '{}' at index {page_index}: {err}",
                    image.name
                ),
            )
        })?;
        let segment_count = segments.len();
        imported.extend(
            segments
                .into_iter()
                .enumerate()
                .map(|(segment_index, segment)| ImportedImage {
                    name: format!("{:03}_{:03}.png", page_index + 1, segment_index + 1),
                    image: image::DynamicImage::ImageRgba8(segment),
                }),
        );
        let _ = send_stitch_progress(progress_tx, "split", page_index + 1, segment_count.max(1));
    }

    if imported.is_empty() {
        return Err(stitch_error(
            "По текущим линиям не удалось получить сегменты.",
            "manual cuts across pages produced zero segments",
        ));
    }
    let _ = send_stitch_progress(progress_tx, "preview", 0, imported.len().max(1));
    Ok(StitchSuccess {
        pages: build_ribbon_pages(imported),
        cut_guides: Vec::new(),
        kind: StitchSuccessKind::ManualApply,
    })
}

fn cut_like_reference(
    images: Vec<StitchInputImage>,
    reference_dir: &Path,
    progress_tx: &Sender<StitchWorkerEvent>,
) -> Result<StitchSuccess, StitchError> {
    if images.is_empty() {
        return Err(stitch_error(
            "Сначала откройте папку или скачайте главу.",
            "cut_like_reference requested with zero source images",
        ));
    }

    let reference_images = load_reference_images(reference_dir, progress_tx).map_err(|err| {
        stitch_error(
            "Не удалось загрузить пример главы для нарезки.",
            format!(
                "failed to load reference images from '{}': {err}",
                reference_dir.display()
            ),
        )
    })?;
    if reference_images.is_empty() {
        return Err(stitch_error(
            "В выбранной папке нет изображений для примера главы.",
            format!(
                "reference dir '{}' does not contain supported images",
                reference_dir.display()
            ),
        ));
    }

    let reference_widths = reference_images
        .iter()
        .map(|image| image.image.width())
        .collect::<Vec<_>>();
    if reference_widths
        .first()
        .copied()
        .is_some_and(|first| reference_widths.iter().any(|width| *width != first))
    {
        return Err(stitch_error(
            "Картинки примера главы должны иметь одинаковую ширину.",
            format!(
                "reference dir '{}' contains mixed widths: {:?}",
                reference_dir.display(),
                reference_widths,
            ),
        ));
    }

    let current_images = images
        .into_iter()
        .map(|image| image.image)
        .collect::<Vec<_>>();
    let reference_heights = reference_images
        .iter()
        .map(|image| image.image.height() as usize)
        .collect::<Vec<_>>();
    let reference_total = reference_heights.iter().sum::<usize>();
    if reference_total == 0 {
        return Err(stitch_error(
            "Пример главы оказался пустым.",
            format!(
                "reference dir '{}' produced zero total height",
                reference_dir.display()
            ),
        ));
    }

    let _ = send_stitch_progress(progress_tx, "normalize", 0, current_images.len().max(1));
    let (normalized, _) = unify_widths(&current_images);
    let _ = send_stitch_progress(
        progress_tx,
        "normalize",
        current_images.len(),
        current_images.len().max(1),
    );

    let _ = send_stitch_progress(progress_tx, "stitch", 0, normalized.len().max(1));
    let superframes = stitch_sequence(&normalized);
    if superframes.is_empty() {
        return Err(stitch_error(
            "Не удалось сшить текущую ленту перед нарезкой.",
            "cut_like_reference produced zero superframes",
        ));
    }
    let _ = send_stitch_progress(
        progress_tx,
        "stitch",
        normalized.len(),
        normalized.len().max(1),
    );

    let tape_height = superframes
        .iter()
        .map(|image| image.height() as usize)
        .sum::<usize>();
    if tape_height == 0 {
        return Err(stitch_error(
            "Склеенная лента оказалась пустой.",
            "cut_like_reference stitched tape height is zero",
        ));
    }

    let _ = send_stitch_progress(progress_tx, "compose", 0, superframes.len().max(1));
    let mut tape = compose_superframes(&superframes, progress_tx).map_err(|err| {
        stitch_error(
            "Не удалось собрать склеенную ленту перед нарезкой.",
            format!("failed to compose stitched tape for reference cut: {err}"),
        )
    })?;
    let crop_offset = detect_reference_crop_offset(&tape, &reference_images[0].image);
    if crop_offset > 0 && crop_offset < tape.height() as usize {
        crate::runtime_log::log_info(format!(
            "[new-project] auto-aligned reference cut by cropping {} px from top for '{}'",
            crop_offset,
            reference_dir.display(),
        ));
        tape = image::imageops::crop_imm(
            &tape,
            0,
            crop_offset as u32,
            tape.width(),
            tape.height().saturating_sub(crop_offset as u32),
        )
        .to_image();
    }

    let mut segments = Vec::with_capacity(reference_heights.len());
    let mut offset = 0usize;
    let mut cut_incomplete = false;
    for &height in &reference_heights {
        let next = offset.saturating_add(height);
        if next > tape.height() as usize {
            cut_incomplete = true;
            break;
        }
        segments.push(
            image::imageops::crop_imm(&tape, 0, offset as u32, tape.width(), height as u32)
                .to_image(),
        );
        offset = next;
    }

    if segments.is_empty() {
        return Err(stitch_error(
            "Не удалось разрезать ленту по примеру главы.",
            format!(
                "reference cut produced zero segments for '{}'",
                reference_dir.display()
            ),
        ));
    }

    if cut_incomplete {
        let avg_ref_height = reference_total / reference_heights.len().max(1);
        let short_by = reference_total.saturating_sub(tape.height() as usize);
        if short_by > 0 && short_by >= avg_ref_height {
            crate::runtime_log::log_warn(format!(
                "[new-project] reference cut for '{}' is missing trailing pages: short_by={} avg_ref_height={}",
                reference_dir.display(),
                short_by,
                avg_ref_height,
            ));
        }
    }

    let imported = segments
        .into_iter()
        .enumerate()
        .map(|(index, segment)| ImportedImage {
            name: reference_images
                .get(index)
                .map(|image| image.name.clone())
                .unwrap_or_else(|| format_stitch_segment_name(index)),
            image: image::DynamicImage::ImageRgba8(segment),
        })
        .collect::<Vec<_>>();
    let _ = send_stitch_progress(progress_tx, "split", imported.len(), imported.len().max(1));
    let _ = send_stitch_progress(
        progress_tx,
        "preview",
        imported.len(),
        imported.len().max(1),
    );
    Ok(StitchSuccess {
        pages: build_ribbon_pages(imported),
        cut_guides: Vec::new(),
        kind: StitchSuccessKind::ReferenceCut,
    })
}

fn build_stitched_tape_and_cuts(
    images: &[RgbaImage],
    options: StitchOptions,
    progress_tx: &Sender<StitchWorkerEvent>,
) -> anyhow::Result<(RgbaImage, Vec<usize>)> {
    let _ = send_stitch_progress(progress_tx, "normalize", 0, images.len().max(1));
    let (normalized, _width) = unify_widths(images);
    let _ = send_stitch_progress(progress_tx, "normalize", images.len(), images.len().max(1));
    let _ = send_stitch_progress(progress_tx, "stitch", 0, normalized.len().max(1));
    let superframes = stitch_sequence(&normalized);
    if superframes.is_empty() {
        anyhow::bail!("no stitched superframes produced");
    }
    let _ = send_stitch_progress(
        progress_tx,
        "stitch",
        normalized.len(),
        normalized.len().max(1),
    );

    let heights = superframes
        .iter()
        .map(|image| image.height() as usize)
        .collect::<Vec<_>>();
    let total_height = heights.iter().sum::<usize>();
    if total_height == 0 {
        anyhow::bail!("stitched tape height is zero");
    }

    let mut parts = options.parts.unwrap_or_else(|| {
        ((total_height as f32) / options.target_height.max(1) as f32).ceil() as usize
    });
    parts = parts.max(1);
    let min_required =
        ((total_height as f32) / options.target_height.max(1) as f32).ceil() as usize;
    if parts < min_required {
        parts = min_required;
    }

    let _ = send_stitch_progress(progress_tx, "cuts", 0, superframes.len().max(1));
    let candidate_analyses = analyze_superframe_candidates(&superframes);
    let (positions, cost_map) = build_candidates(&superframes, &candidate_analyses);
    let mut cuts = greedy_cut_positions(
        &positions,
        &cost_map,
        total_height,
        parts,
        options.target_height,
    );

    let mut fixed = vec![cuts[0]];
    for index in 1..cuts.len() {
        let prev = *fixed.last().unwrap_or(&0);
        let mut current = cuts[index];
        let is_last = index + 1 == cuts.len();
        if !is_last && current.saturating_sub(prev) > options.target_height {
            let hi = prev + options.target_height;
            let best = positions_in_range(&positions, prev.saturating_add(1), hi)
                .iter()
                .copied()
                .min_by(|left, right| {
                    let left_cost = cost_map.get(left).copied().unwrap_or(0.0);
                    let right_cost = cost_map.get(right).copied().unwrap_or(0.0);
                    left_cost
                        .partial_cmp(&right_cost)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            current = best.unwrap_or(hi);
        }
        fixed.push(current);
    }

    cuts = refine_cuts_to_uniform_bands(
        &superframes,
        &fixed,
        options.target_height,
        options.band_rows,
        options.tolerance,
        options.search_radius,
        options.prefer_up_first,
    );
    let _ = send_stitch_progress(
        progress_tx,
        "cuts",
        superframes.len(),
        superframes.len().max(1),
    );

    let _ = send_stitch_progress(progress_tx, "compose", 0, superframes.len().max(1));
    let tape = compose_superframes(&superframes, progress_tx)?;
    Ok((tape, cuts))
}

fn compose_superframes(
    superframes: &[RgbaImage],
    progress_tx: &Sender<StitchWorkerEvent>,
) -> anyhow::Result<RgbaImage> {
    let width = superframes.first().map(RgbaImage::width).unwrap_or(0);
    let total_height = superframes
        .iter()
        .map(|image| image.height() as usize)
        .sum::<usize>();
    if width == 0 || total_height == 0 {
        anyhow::bail!("cannot compose empty superframes");
    }

    let mut tape = RgbaImage::new(width, total_height as u32);
    let mut offset = 0i64;
    for (index, frame) in superframes.iter().enumerate() {
        let _ = send_stitch_progress(progress_tx, "compose", index, superframes.len().max(1));
        overlay(&mut tape, frame, 0, offset);
        offset += i64::from(frame.height());
    }
    let _ = send_stitch_progress(
        progress_tx,
        "compose",
        superframes.len(),
        superframes.len().max(1),
    );
    Ok(tape)
}

fn send_stitch_progress(
    progress_tx: &Sender<StitchWorkerEvent>,
    stage: &'static str,
    current: usize,
    total: usize,
) -> Result<(), mpsc::SendError<StitchWorkerEvent>> {
    progress_tx.send(StitchWorkerEvent::Progress {
        stage,
        current,
        total,
    })
}

fn split_tape_by_cuts(tape: &RgbaImage, cuts: &[usize]) -> anyhow::Result<Vec<RgbaImage>> {
    if tape.width() == 0 || tape.height() == 0 {
        anyhow::bail!("tape image is empty");
    }
    let tape_height = tape.height() as usize;
    let mut normalized = cuts
        .iter()
        .copied()
        .filter(|cut| *cut <= tape_height)
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    if normalized.first().copied().unwrap_or_default() != 0 {
        normalized.insert(0, 0);
    }
    if normalized.last().copied().unwrap_or_default() != tape_height {
        normalized.push(tape_height);
    }

    let mut segments = Vec::new();
    for pair in normalized.windows(2) {
        let start = pair[0];
        let end = pair[1];
        if end <= start {
            continue;
        }
        let segment =
            image::imageops::crop_imm(tape, 0, start as u32, tape.width(), (end - start) as u32)
                .to_image();
        segments.push(segment);
    }
    Ok(segments)
}

fn unify_widths(images: &[RgbaImage]) -> (Vec<RgbaImage>, u32) {
    let mut width_counts = HashMap::<u32, usize>::new();
    for image in images {
        *width_counts.entry(image.width()).or_default() += 1;
    }
    let target_width = width_counts
        .into_iter()
        .max_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
        .map(|(width, _)| width)
        .unwrap_or(1);

    let normalized = images
        .par_iter()
        .map(|image| {
            if image.width() == target_width {
                image.clone()
            } else {
                let scale = target_width as f32 / image.width().max(1) as f32;
                let new_height = ((image.height() as f32) * scale).round().max(1.0) as u32;
                resize(image, target_width, new_height, resize_filter(scale))
            }
        })
        .collect::<Vec<_>>();
    (normalized, target_width)
}

struct LoadedReferenceImage {
    name: String,
    image: RgbaImage,
}

fn load_reference_images(
    dir: &Path,
    progress_tx: &Sender<StitchWorkerEvent>,
) -> anyhow::Result<Vec<LoadedReferenceImage>> {
    let mut paths = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && has_reference_image_extension(path))
        .collect::<Vec<_>>();
    paths.sort();

    let total = paths.len().max(1);
    let mut images = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let _ = send_stitch_progress(progress_tx, "decode", index, total);
        match image::open(path) {
            Ok(image) => images.push(LoadedReferenceImage {
                name: path
                    .file_name()
                    .and_then(|file_name| file_name.to_str())
                    .unwrap_or("page.png")
                    .to_string(),
                image: image.to_rgba8(),
            }),
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[new-project] failed to decode reference image '{}': {err}",
                    path.display()
                ));
            }
        }
    }
    let _ = send_stitch_progress(progress_tx, "decode", images.len().max(paths.len()), total);
    Ok(images)
}

fn has_reference_image_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase()),
        Some(extension)
            if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp")
    )
}

fn detect_reference_crop_offset(tape: &RgbaImage, reference: &RgbaImage) -> usize {
    if tape.width() == 0
        || tape.height() == 0
        || reference.width() == 0
        || reference.height() == 0
        || tape.height() <= reference.height()
    {
        return 0;
    }

    let search_width = tape.width().min(reference.width()).min(96);
    if search_width < 8 {
        return 0;
    }

    let tape_scale = search_width as f32 / tape.width() as f32;
    let reference_scale = search_width as f32 / reference.width() as f32;
    let tape_small = resize(
        tape,
        search_width,
        ((tape.height() as f32) * tape_scale).round().max(1.0) as u32,
        FilterType::Triangle,
    );
    let reference_small = resize(
        reference,
        search_width,
        ((reference.height() as f32) * reference_scale)
            .round()
            .max(1.0) as u32,
        FilterType::Triangle,
    );
    if tape_small.height() <= reference_small.height() {
        return 0;
    }

    let tape_gray = prepare_match_gray(&tape_small);
    let reference_gray = prepare_match_gray(&reference_small);
    if template_gradient_energy(&reference_gray) < 1.0 {
        return 0;
    }

    let max_offset = tape_gray.height().saturating_sub(reference_gray.height());
    if max_offset == 0 {
        return 0;
    }

    let coarse_step = (max_offset / 1024).max(1);
    let mut best_offset = 0u32;
    let mut best_score = f32::NEG_INFINITY;
    let mut offset = 0u32;
    while offset <= max_offset {
        let score = ncc_region(
            &tape_gray,
            offset,
            &reference_gray,
            0,
            reference_gray.height(),
        );
        if score > best_score {
            best_score = score;
            best_offset = offset;
        }
        match offset.checked_add(coarse_step) {
            Some(next) if next > offset => offset = next,
            _ => break,
        }
    }

    let refine_start = best_offset.saturating_sub(coarse_step);
    let refine_end = (best_offset + coarse_step).min(max_offset);
    for candidate in refine_start..=refine_end {
        let score = ncc_region(
            &tape_gray,
            candidate,
            &reference_gray,
            0,
            reference_gray.height(),
        );
        if score > best_score {
            best_score = score;
            best_offset = candidate;
        }
    }

    if best_score < 0.55 {
        return 0;
    }
    ((best_offset as f32) / tape_scale).round().max(0.0) as usize
}

fn resize_filter(scale: f32) -> FilterType {
    if scale < 1.0 {
        FilterType::Triangle
    } else {
        FilterType::CatmullRom
    }
}

fn stitch_sequence(images: &[RgbaImage]) -> Vec<RgbaImage> {
    let mut stitched = Vec::new();
    let mut index = 0usize;
    while index < images.len() {
        let mut current = images[index].clone();
        index += 1;
        while index < images.len() {
            let next = &images[index];
            let current_gray = prepare_match_gray(&current);
            let next_gray = prepare_match_gray(next);
            let Some(alignment) = find_vertical_alignment(&current_gray, &next_gray) else {
                break;
            };

            let band_a = crop_gray(&current_gray, alignment.y_a, alignment.overlap_h);
            let band_b = crop_gray(&next_gray, 0, alignment.overlap_h);
            let color_mad = mad_gray(&band_a, &band_b);

            current = if color_mad <= 7.0 {
                blend_vertical(&current, next, alignment.y_a, alignment.overlap_h)
            } else {
                stack_without_blend(&current, next, alignment.y_a)
            };
            index += 1;
        }
        stitched.push(current);
    }
    stitched
}

fn stitch_two_pages_without_width_normalization(a: &RgbaImage, b: &RgbaImage) -> RgbaImage {
    let a_gray = prepare_match_gray(a);
    let b_gray = prepare_match_gray(b);
    let Some(alignment) = find_vertical_alignment(&a_gray, &b_gray) else {
        return stack_without_blend(a, b, a.height());
    };

    let band_a = crop_gray(&a_gray, alignment.y_a, alignment.overlap_h);
    let band_b = crop_gray(&b_gray, 0, alignment.overlap_h);
    let color_mad = mad_gray(&band_a, &band_b);
    if color_mad <= 7.0 {
        blend_vertical(a, b, alignment.y_a, alignment.overlap_h)
    } else {
        stack_without_blend(a, b, alignment.y_a)
    }
}

fn bottom_is_heterogeneous_cut_area(image: &RgbaImage, band_rows: usize, tolerance: u8) -> bool {
    let height = image.height() as usize;
    if height == 0 {
        return false;
    }
    let band_rows = band_rows.max(1).min(height);
    let start = height.saturating_sub(band_rows);
    !(start..start + band_rows).all(|row| row_is_uniform(image, row, tolerance))
}

#[derive(Clone, Copy)]
struct AlignmentMatch {
    y_a: u32,
    overlap_h: u32,
}

struct SuperframeCandidateAnalysis {
    rows: Vec<usize>,
    costs: Vec<f32>,
}

struct UniformBandAnalysis {
    offsets: Vec<usize>,
    row_prefix_sums: Vec<Vec<usize>>,
}

fn find_vertical_alignment(a_gray: &GrayImage, b_gray: &GrayImage) -> Option<AlignmentMatch> {
    let ha = a_gray.height();
    let hb = b_gray.height();
    let wa = a_gray.width();
    if wa == 0 || wa != b_gray.width() {
        return None;
    }

    let min_h = ha.min(hb);
    let omax = ((min_h as f32) * 0.18).round() as u32;
    let tpl_h = ((min_h as f32) * 0.12).round().clamp(64.0, 260.0) as u32;
    if tpl_h == 0 || tpl_h >= hb || tpl_h > ha {
        return None;
    }

    let template = crop_gray(b_gray, 0, tpl_h);
    if template_gradient_energy(&template) < 2.0 {
        return None;
    }

    let search_h = ha.min(tpl_h + omax);
    if search_h <= tpl_h {
        return None;
    }
    let y0 = ha - search_h;
    let mut scores = Vec::new();
    for offset in 0..=(search_h - tpl_h) {
        let score = ncc_region(a_gray, y0 + offset, &template, 0, tpl_h);
        scores.push(score);
    }
    let (best_idx, best_score) = scores.iter().copied().enumerate().max_by(|left, right| {
        left.1
            .partial_cmp(&right.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    let second = second_best_value(&scores, best_idx, 12);
    let peak_ratio = best_score / second.max(1e-6);
    let y_a = y0 + best_idx as u32;
    let overlap_h = ha.saturating_sub(y_a);

    let overlap_limit = (((ha as f32) * 0.5) + 40.0)
        .min(((hb as f32) * 0.5) + 40.0)
        .min((tpl_h + omax) as f32) as u32;
    if best_score < 0.72 || peak_ratio < 1.08 || overlap_h < 16 || overlap_h > overlap_limit {
        return None;
    }

    let band_a = crop_gray(a_gray, ha.saturating_sub(overlap_h), overlap_h);
    let search_h_b = hb.min(overlap_h + omax);
    let mut reverse_scores = Vec::new();
    for offset in 0..=(search_h_b.saturating_sub(overlap_h)) {
        reverse_scores.push(ncc_region(b_gray, offset, &band_a, 0, overlap_h));
    }
    let (reverse_idx, reverse_best) =
        reverse_scores
            .iter()
            .copied()
            .enumerate()
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;
    if reverse_idx > 6 || reverse_best < 0.70 {
        return None;
    }

    let band_b = crop_gray(b_gray, 0, overlap_h);
    let mad = mad_gray(&band_a, &band_b);
    let ssim = ssim_gray(&band_a, &band_b);
    if mad > 6.5 || ssim < 0.60 {
        return None;
    }

    Some(AlignmentMatch { y_a, overlap_h })
}

fn analyze_superframe_candidates(superframes: &[RgbaImage]) -> Vec<SuperframeCandidateAnalysis> {
    superframes
        .par_iter()
        .map(|image| {
            let gray = prepare_cost_gray(image);
            let (rows, costs) = safe_rows(&gray, SAFE_ROWS_STRIDE);
            SuperframeCandidateAnalysis { rows, costs }
        })
        .collect()
}

fn build_candidates(
    superframes: &[RgbaImage],
    analyses: &[SuperframeCandidateAnalysis],
) -> (Vec<usize>, HashMap<usize, f32>) {
    let mut positions = vec![0usize];
    let mut cost_map = HashMap::from([(0usize, 0.0f32)]);
    let mut offset = 0usize;
    for (image, analysis) in superframes.iter().zip(analyses.iter()) {
        for (&row, &cost) in analysis.rows.iter().zip(analysis.costs.iter()) {
            positions.push(offset + row);
            cost_map.insert(offset + row, cost);
        }
        offset += image.height() as usize;
        positions.push(offset);
        cost_map.insert(offset, 0.0);
    }
    positions.sort_unstable();
    positions.dedup();
    (positions, cost_map)
}

fn greedy_cut_positions(
    positions: &[usize],
    cost_map: &HashMap<usize, f32>,
    total_height: usize,
    parts: usize,
    target_height: usize,
) -> Vec<usize> {
    let mut cuts = vec![0usize];
    let part_target = total_height as f32 / parts.max(1) as f32;
    let alpha = 1.0f32;
    let beta = 0.6f32;

    for part in 1..parts {
        let prev = *cuts.last().unwrap_or(&0);
        let ideal = (part as f32 * part_target).round().max(0.0) as usize;
        let lo = prev + ((part_target * 0.5).round().max(1.0) as usize);
        let hi = (prev + target_height).min(total_height);
        let win_lo = lo.max(ideal.saturating_sub(SEARCH_DELTA));
        let win_hi = hi.min(ideal + SEARCH_DELTA);
        let window = positions_in_range(positions, win_lo, win_hi);
        let fallback = positions_in_range(positions, lo, hi);
        let candidates = if window.is_empty() { fallback } else { window };
        if candidates.is_empty() {
            cuts.push((prev + target_height).min(total_height));
            continue;
        }

        let mut best_cut = None;
        let mut best_score = f32::INFINITY;
        for &position in candidates {
            if position <= prev || position > hi {
                continue;
            }
            let cost = cost_map.get(&position).copied().unwrap_or(0.0);
            let score =
                beta * cost + alpha * ((position.abs_diff(ideal) as f32) / part_target.max(1.0));
            if score < best_score {
                best_score = score;
                best_cut = Some(position);
            }
        }
        cuts.push(best_cut.unwrap_or_else(|| (prev + target_height).min(total_height)));
    }

    if cuts.last().copied().unwrap_or_default() != total_height {
        cuts.push(total_height);
    }
    cuts
}

fn refine_cuts_to_uniform_bands(
    superframes: &[RgbaImage],
    cuts: &[usize],
    target_height: usize,
    band_rows: usize,
    tolerance: u8,
    search_radius: usize,
    prefer_up_first: bool,
) -> Vec<usize> {
    let analysis = analyze_uniform_bands(superframes, tolerance);
    let total_height = *analysis.offsets.last().unwrap_or(&0);
    let mut refined = vec![cuts.first().copied().unwrap_or(0)];

    for (index, target) in cuts
        .iter()
        .copied()
        .enumerate()
        .skip(1)
        .take(cuts.len().saturating_sub(2))
    {
        let prev = *refined.last().unwrap_or(&0);
        let max_down = (prev + target_height).min(total_height.saturating_sub(band_rows));
        let mut best = target;
        let mut found = false;

        for delta in 0..=search_radius {
            let up = target.saturating_sub(delta);
            let down = target.saturating_add(delta);
            let candidates = if prefer_up_first {
                [(up, true), (down, false)]
            } else {
                [(down, false), (up, true)]
            };
            for (candidate, _) in candidates {
                if candidate <= prev || candidate > max_down {
                    continue;
                }
                if has_uniform_band(&analysis, candidate, band_rows, total_height) {
                    best = candidate;
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }

        if best.saturating_sub(prev) > target_height {
            best = prev + target_height;
        }
        crate::runtime_log::log_info(format!(
            "[new-project] refine cut {}: {} -> {}{}",
            index,
            target,
            best,
            if found { " [uniform]" } else { "" }
        ));
        refined.push(best);
    }

    refined.push(cuts.last().copied().unwrap_or(total_height));
    refined
}

fn analyze_uniform_bands(images: &[RgbaImage], tolerance: u8) -> UniformBandAnalysis {
    let offsets = cumulative_offsets(images);
    let row_prefix_sums = images
        .par_iter()
        .map(|image| build_uniform_row_prefix(image, tolerance))
        .collect();
    UniformBandAnalysis {
        offsets,
        row_prefix_sums,
    }
}

fn build_uniform_row_prefix(image: &RgbaImage, tolerance: u8) -> Vec<usize> {
    let height = image.height() as usize;
    let mut prefix = Vec::with_capacity(height + 1);
    prefix.push(0);
    let mut total = 0usize;
    for row_index in 0..height {
        if row_is_uniform(image, row_index, tolerance) {
            total += 1;
        }
        prefix.push(total);
    }
    prefix
}

fn positions_in_range(positions: &[usize], start: usize, end: usize) -> &[usize] {
    if start > end {
        return &[];
    }
    let from = positions.partition_point(|position| *position < start);
    let to = positions.partition_point(|position| *position <= end);
    &positions[from..to]
}

fn format_stitch_segment_name(index: usize) -> String {
    format!(
        "{:0width$}.png",
        index + 1,
        width = STITCH_SEGMENT_NAME_WIDTH
    )
}

fn safe_rows(gray: &GrayImage, stride: usize) -> (Vec<usize>, Vec<f32>) {
    let (bright_norm, row_std, cost) = row_cost_map(gray);
    let tau = quantile(&cost, 0.30);
    let mut rows = Vec::new();
    let mut costs = Vec::new();
    for y in 0..gray.height() as usize {
        let is_white_pad = bright_norm[y] >= 0.95 && row_std[y] <= 3.0;
        let is_safe = cost[y] <= tau || is_white_pad;
        if is_safe && ((y % stride) == 0 || is_white_pad) {
            rows.push(y);
            costs.push(cost[y]);
        }
    }
    if rows.is_empty() {
        let mut y = 0usize;
        while y < gray.height() as usize {
            rows.push(y);
            costs.push(cost.get(y).copied().unwrap_or(0.0));
            y += stride.max(1);
        }
    }
    (rows, costs)
}

fn row_cost_map(gray: &GrayImage) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let width = gray.width() as usize;
    let height = gray.height() as usize;
    let raw = gray.as_raw();
    let mut grad = vec![0.0f32; height];
    let mut bright = vec![0.0f32; height];
    let mut row_std = vec![0.0f32; height];

    for y in 0..height {
        let row_start = y * width;
        let row = &raw[row_start..row_start + width];
        let next_row = (y + 1 < height).then(|| {
            let next_start = (y + 1) * width;
            &raw[next_start..next_start + width]
        });
        let mut sum = 0.0f32;
        let mut sum_sq = 0.0f32;
        let mut grad_sum = 0.0f32;
        for x in 0..width {
            let current = row[x] as f32;
            sum += current;
            sum_sq += current * current;
            if x + 1 < width {
                let next = row[x + 1] as f32;
                grad_sum += (next - current).abs();
            }
            if let Some(next_row) = next_row {
                grad_sum += (next_row[x] as f32 - current).abs();
            }
        }
        let mean = sum / width.max(1) as f32;
        let variance = (sum_sq / width.max(1) as f32) - mean * mean;
        bright[y] = (mean / 255.0).clamp(0.0, 1.0);
        row_std[y] = variance.max(0.0).sqrt();
        grad[y] = grad_sum / width.max(1) as f32;
    }

    let grad_norm = robust_norm(&grad);
    let cost = grad_norm
        .iter()
        .zip(bright.iter())
        .map(|(grad, bright)| (0.75 * grad + 0.25 * (1.0 - bright)).clamp(0.0, 1.0))
        .collect::<Vec<_>>();
    (bright, row_std, cost)
}

fn robust_norm(values: &[f32]) -> Vec<f32> {
    let hi = quantile(values, 0.95).max(1.0);
    values
        .iter()
        .map(|value| (value / hi).clamp(0.0, 1.0))
        .collect()
}

fn quantile(values: &[f32], q: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((sorted.len().saturating_sub(1)) as f32 * q.clamp(0.0, 1.0)).round() as usize;
    sorted.get(index).copied().unwrap_or(0.0)
}

fn prepare_match_gray(image: &RgbaImage) -> GrayImage {
    let gray = rgba_to_gray(image);
    if gray.width() <= MATCH_WIDTH {
        gray
    } else {
        let scale = MATCH_WIDTH as f32 / gray.width() as f32;
        let new_height = ((gray.height() as f32) * scale).round().max(1.0) as u32;
        resize(&gray, MATCH_WIDTH, new_height, FilterType::Triangle)
    }
}

fn prepare_cost_gray(image: &RgbaImage) -> GrayImage {
    rgba_to_gray(image)
}

fn rgba_to_gray(image: &RgbaImage) -> GrayImage {
    let gray = image
        .as_raw()
        .chunks_exact(4)
        .map(|px| {
            (0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32)
                .round()
                .clamp(0.0, 255.0) as u8
        })
        .collect::<Vec<_>>();
    ImageBuffer::<Luma<u8>, Vec<u8>>::from_raw(image.width(), image.height(), gray)
        .unwrap_or_else(|| GrayImage::new(image.width(), image.height()))
}

fn crop_gray(image: &GrayImage, y: u32, height: u32) -> GrayImage {
    image::imageops::crop_imm(
        image,
        0,
        y.min(image.height()),
        image.width(),
        height.min(image.height().saturating_sub(y)),
    )
    .to_image()
}

fn template_gradient_energy(gray: &GrayImage) -> f32 {
    if gray.width() < 2 || gray.height() < 2 {
        return 0.0;
    }
    let width = gray.width() as usize;
    let height = gray.height() as usize;
    let raw = gray.as_raw();
    let mut sum = 0.0f32;
    let mut count = 0usize;
    for y in 0..(height - 1) {
        let row_start = y * width;
        let next_start = (y + 1) * width;
        for x in 0..(width - 1) {
            let current = raw[row_start + x] as f32;
            let right = raw[row_start + x + 1] as f32;
            let down = raw[next_start + x] as f32;
            sum += ((right - current).abs() + (down - current).abs()) * 0.5;
            count += 1;
        }
    }
    if count == 0 { 0.0 } else { sum / count as f32 }
}

fn ncc_region(a: &GrayImage, ay: u32, b: &GrayImage, by: u32, height: u32) -> f32 {
    let width = a.width().min(b.width()) as usize;
    let height = height
        .min(a.height().saturating_sub(ay))
        .min(b.height().saturating_sub(by)) as usize;
    if width == 0 || height == 0 {
        return -1.0;
    }
    let a_stride = a.width() as usize;
    let b_stride = b.width() as usize;
    let a_raw = a.as_raw();
    let b_raw = b.as_raw();
    let ay = ay as usize;
    let by = by as usize;
    let mut sum_a = 0.0f32;
    let mut sum_b = 0.0f32;
    let mut count = 0usize;
    for y in 0..height {
        let a_row = &a_raw[(ay + y) * a_stride..(ay + y) * a_stride + width];
        let b_row = &b_raw[(by + y) * b_stride..(by + y) * b_stride + width];
        for x in 0..width {
            sum_a += a_row[x] as f32;
            sum_b += b_row[x] as f32;
            count += 1;
        }
    }
    if count == 0 {
        return -1.0;
    }
    let mean_a = sum_a / count as f32;
    let mean_b = sum_b / count as f32;
    let mut num = 0.0f32;
    let mut den_a = 0.0f32;
    let mut den_b = 0.0f32;
    for y in 0..height {
        let a_row = &a_raw[(ay + y) * a_stride..(ay + y) * a_stride + width];
        let b_row = &b_raw[(by + y) * b_stride..(by + y) * b_stride + width];
        for x in 0..width {
            let va = a_row[x] as f32 - mean_a;
            let vb = b_row[x] as f32 - mean_b;
            num += va * vb;
            den_a += va * va;
            den_b += vb * vb;
        }
    }
    if den_a <= 1e-6 || den_b <= 1e-6 {
        -1.0
    } else {
        num / (den_a.sqrt() * den_b.sqrt())
    }
}

fn second_best_value(scores: &[f32], best_index: usize, window: usize) -> f32 {
    scores
        .iter()
        .enumerate()
        .filter(|(index, _)| index.abs_diff(best_index) > window)
        .map(|(_, value)| *value)
        .fold(-1.0f32, f32::max)
}

fn mad_gray(a: &GrayImage, b: &GrayImage) -> f32 {
    let width = a.width().min(b.width()) as usize;
    let height = a.height().min(b.height()) as usize;
    if width == 0 || height == 0 {
        return 255.0;
    }
    let a_stride = a.width() as usize;
    let b_stride = b.width() as usize;
    let a_raw = a.as_raw();
    let b_raw = b.as_raw();
    let mut diffs = Vec::with_capacity(width * height);
    for y in 0..height {
        let a_row = &a_raw[y * a_stride..y * a_stride + width];
        let b_row = &b_raw[y * b_stride..y * b_stride + width];
        for x in 0..width {
            let left = a_row[x] as i16;
            let right = b_row[x] as i16;
            diffs.push((left - right).unsigned_abs() as u8);
        }
    }
    diffs.sort_unstable();
    diffs.get(diffs.len() / 2).copied().unwrap_or(255) as f32
}

fn ssim_gray(a: &GrayImage, b: &GrayImage) -> f32 {
    let width = a.width().min(b.width()) as usize;
    let height = a.height().min(b.height()) as usize;
    if width == 0 || height == 0 {
        return 0.0;
    }
    let a_stride = a.width() as usize;
    let b_stride = b.width() as usize;
    let a_raw = a.as_raw();
    let b_raw = b.as_raw();
    let mut sum_a = 0.0f32;
    let mut sum_b = 0.0f32;
    let mut count = 0usize;
    for y in 0..height {
        let a_row = &a_raw[y * a_stride..y * a_stride + width];
        let b_row = &b_raw[y * b_stride..y * b_stride + width];
        for x in 0..width {
            sum_a += a_row[x] as f32;
            sum_b += b_row[x] as f32;
            count += 1;
        }
    }
    let mean_a = sum_a / count.max(1) as f32;
    let mean_b = sum_b / count.max(1) as f32;
    let mut var_a = 0.0f32;
    let mut var_b = 0.0f32;
    let mut cov = 0.0f32;
    for y in 0..height {
        let a_row = &a_raw[y * a_stride..y * a_stride + width];
        let b_row = &b_raw[y * b_stride..y * b_stride + width];
        for x in 0..width {
            let va = a_row[x] as f32 - mean_a;
            let vb = b_row[x] as f32 - mean_b;
            var_a += va * va;
            var_b += vb * vb;
            cov += va * vb;
        }
    }
    let denom = (count.saturating_sub(1)).max(1) as f32;
    var_a /= denom;
    var_b /= denom;
    cov /= denom;
    let c1 = 6.5025f32;
    let c2 = 58.5225f32;
    let numerator = (2.0 * mean_a * mean_b + c1) * (2.0 * cov + c2);
    let denominator = (mean_a * mean_a + mean_b * mean_b + c1) * (var_a + var_b + c2);
    if denominator <= 1e-6 {
        0.0
    } else {
        numerator / denominator
    }
}

fn blend_vertical(a: &RgbaImage, b: &RgbaImage, y_a: u32, overlap_h: u32) -> RgbaImage {
    let out_h = a.height().max(y_a + b.height());
    let mut out = RgbaImage::from_pixel(a.width(), out_h, Rgba([0, 0, 0, 0]));
    overlay(&mut out, a, 0, 0);
    if overlap_h > 0 {
        for row in 0..overlap_h {
            let weight = row as f32 / overlap_h.max(1) as f32;
            for x in 0..a.width() {
                let left = a.get_pixel(x, y_a + row).0;
                let right = b.get_pixel(x, row).0;
                let blended = [
                    ((left[0] as f32) * (1.0 - weight) + (right[0] as f32) * weight)
                        .round()
                        .clamp(0.0, 255.0) as u8,
                    ((left[1] as f32) * (1.0 - weight) + (right[1] as f32) * weight)
                        .round()
                        .clamp(0.0, 255.0) as u8,
                    ((left[2] as f32) * (1.0 - weight) + (right[2] as f32) * weight)
                        .round()
                        .clamp(0.0, 255.0) as u8,
                    255,
                ];
                out.put_pixel(x, y_a + row, Rgba(blended));
            }
        }
    }
    if b.height() > overlap_h {
        let tail = image::imageops::crop_imm(b, 0, overlap_h, b.width(), b.height() - overlap_h)
            .to_image();
        overlay(&mut out, &tail, 0, i64::from(y_a + overlap_h));
    }
    out
}

fn stack_without_blend(a: &RgbaImage, b: &RgbaImage, y_a: u32) -> RgbaImage {
    let out_h = a.height().max(y_a + b.height());
    let mut out = RgbaImage::from_pixel(a.width(), out_h, Rgba([0, 0, 0, 0]));
    overlay(&mut out, a, 0, 0);
    overlay(&mut out, b, 0, i64::from(y_a));
    out
}

fn cumulative_offsets(images: &[RgbaImage]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(images.len() + 1);
    offsets.push(0);
    for image in images {
        let next = offsets.last().copied().unwrap_or(0) + image.height() as usize;
        offsets.push(next);
    }
    offsets
}

fn has_uniform_band(
    analysis: &UniformBandAnalysis,
    y0: usize,
    band_rows: usize,
    total_height: usize,
) -> bool {
    if y0 + band_rows > total_height {
        return false;
    }
    let start_index = analysis
        .offsets
        .partition_point(|offset| *offset <= y0)
        .saturating_sub(1);
    let end_y = y0 + band_rows;
    let end_index = analysis
        .offsets
        .partition_point(|offset| *offset < end_y)
        .saturating_sub(1);
    if start_index != end_index {
        return false;
    }
    let start = analysis.offsets.get(start_index).copied().unwrap_or(0);
    let local_start = y0.saturating_sub(start);
    let local_end = end_y.saturating_sub(start);
    let Some(prefix) = analysis.row_prefix_sums.get(start_index) else {
        return false;
    };
    if local_end > prefix.len().saturating_sub(1) {
        return false;
    }
    prefix[local_end].saturating_sub(prefix[local_start]) == band_rows
}

fn row_is_uniform(image: &RgbaImage, row_index: usize, tolerance: u8) -> bool {
    let width = image.width() as usize;
    let row_start = row_index * width * 4;
    let row = &image.as_raw()[row_start..row_start + width * 4];
    let mut min_rgb = [u8::MAX; 3];
    let mut max_rgb = [u8::MIN; 3];
    for pixel in row.chunks_exact(4) {
        for channel in 0..3 {
            min_rgb[channel] = min_rgb[channel].min(pixel[channel]);
            max_rgb[channel] = max_rgb[channel].max(pixel[channel]);
        }
    }
    (0..3).all(|channel| max_rgb[channel].saturating_sub(min_rgb[channel]) <= tolerance)
}

fn stitch_error(user_message: impl Into<String>, log_message: impl Into<String>) -> StitchError {
    StitchError {
        user_message: user_message.into(),
        log_message: log_message.into(),
    }
}

/// Synchronous stitch/split entry point for use by the batch executor (already off GUI thread).
/// Returns the resulting `RgbaImage`s or an error string.
pub fn run_stitch_split_sync(
    images: Vec<StitchInputImage>,
    options: StitchOptions,
) -> Result<Vec<RgbaImage>, String> {
    let (dummy_tx, _rx) = mpsc::channel::<StitchWorkerEvent>();
    stitch_split(images, options, &dummy_tx)
        .map(|success| {
            success
                .pages
                .into_iter()
                .map(|page| (*page.full_image()).clone())
                .collect()
        })
        .map_err(|err| err.log_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_image(width: u32, height: u32, value: u8) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba([value, value, value, 255]))
    }

    fn image_with_heterogeneous_bottom(width: u32, height: u32, band_rows: u32) -> RgbaImage {
        let mut image = solid_image(width, height, 255);
        let start = height.saturating_sub(band_rows);
        for y in start..height {
            for x in 0..width {
                let value = if (x + y) % 2 == 0 { 0 } else { 255 };
                image.put_pixel(x, y, Rgba([value, value, value, 255]));
            }
        }
        image
    }

    fn heterogeneous_options() -> StitchOptions {
        StitchOptions {
            parts: None,
            target_height: 19000,
            band_rows: 4,
            tolerance: 15,
            search_radius: 5500,
            prefer_up_first: true,
            mode: StitchSplitMode::HeterogeneousBottoms,
        }
    }

    #[test]
    fn bottom_heterogeneous_detection_uses_uniform_band_settings() {
        let uniform = solid_image(8, 12, 255);
        let heterogeneous = image_with_heterogeneous_bottom(8, 12, 4);

        assert!(!bottom_is_heterogeneous_cut_area(&uniform, 4, 15));
        assert!(bottom_is_heterogeneous_cut_area(&heterogeneous, 4, 15));
    }

    #[test]
    fn heterogeneous_mode_merges_until_bottom_becomes_uniform() {
        let images = vec![
            StitchInputImage {
                name: "001.png".to_string(),
                image: image_with_heterogeneous_bottom(8, 12, 4),
            },
            StitchInputImage {
                name: "002.png".to_string(),
                image: solid_image(8, 12, 255),
            },
            StitchInputImage {
                name: "003.png".to_string(),
                image: solid_image(8, 12, 255),
            },
        ];

        let result = match run_stitch_split_sync(images, heterogeneous_options()) {
            Ok(result) => result,
            Err(err) => panic!("heterogeneous stitching should succeed: {err}"),
        };

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].height(), 24);
        assert_eq!(result[1].height(), 12);
    }
}
