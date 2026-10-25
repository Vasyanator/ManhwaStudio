/*
File: tools/watermark_removal/chapter_jobs.rs

Purpose:
Chapter-mode worker jobs: sampling, scanning, applying, using a library match and loading the
library, plus kind rebuild / library adopt-release-match and the library save request builder.

Key items:
- `run_chapter_sample()`, `run_chapter_scan()`, `run_chapter_apply()`, `run_chapter_use_match()`,
  `run_chapter_library_load()`
- `rebuild_chapter_kind()`, `adopt_library_entry()`, `release_library_entry()`,
  `match_catalog_against_library()`
- `build_library_request()`, `chapter_source_ref()`

Notes:
Every job runs off the GUI thread and reports through `ChapterEvent`; request types live in `mod.rs`.
*/
use crate::tools::watermark_entry::{
    StoredEntryIdentity, alpha_assumption_from_stored, candidate_improves, engine_background,
    luma_of_level, rank_library_candidates, rebuild_stored_kind, save_request_from_kind,
    stored_flat_background,
};
use crate::tools::watermark_library::{
    EntrySummary, LibrarySample, LoadedEntry, SaveEntryRequest, StoredAlphaAssumption,
    StoredSampleOrigin, StoredSourceRef, list_entries, load_entry,
};
use ms_project::ProjectData;
use crate::watermark_chapter::{
    AcceptanceEvidence, CalibrationSample, MarkSignature, MarkTemplate, Occurrence, PixelRect,
    RemovalResidual, SampleVerdict, WatermarkKind, alpha_blend_operator,
    calibration_sample_from_page, discover_anchors, find_matching_kind, find_occurrences,
    remove_occurrences_on_page, scan_page, validate_calibration_sample,
};
use eframe::egui;
use image::RgbaImage;
use std::sync::mpsc::Sender;
use super::{
    CHAPTER_MAX_AUTO_SAMPLES, CHAPTER_MAX_TEMPLATE_SIDE, ChapterApplyBatch, ChapterApplyReport,
    ChapterApplyRequest, ChapterCatalog, ChapterEvent, ChapterHitRecord, ChapterLoadOutcome,
    ChapterMark, ChapterMatchRequest, ChapterPageHits, ChapterPatch, ChapterSampleOutcome,
    ChapterSampleRequest, ChapterScanOutcome, ChapterScanReport, ChapterScanRequest,
    ParkedCalibration,
};
use super::chapter_state::{
    build_chapter_preview, chapter_engine_error, chapter_histogram_bin, crop_page,
    decode_chapter_page, overlay_rect_to_page, refit_chapter_kind, refresh_chapter_preview,
};
use super::describe::describe_sample_verdict;


/// Adds one calibration sample, creating a mark when the caller asked for a new one.
///
/// Identity is the engine's `MarkSignature`: a selection offered as a new mark is matched
/// against the catalog with `find_matching_kind` first, because a colour mark and its
/// greyscale twin can be pixel-identical in shape and must NOT be merged, while a second
/// sample of the SAME mark must not become a second entry.
///
/// # Errors
/// A user-facing message for a decode failure, a selection that is degenerate, too large,
/// or does not match the target mark's footprint, and for an engine failure.
pub(super) fn run_chapter_sample(request: ChapterSampleRequest) -> Result<ChapterSampleOutcome, String> {
    let ChapterSampleRequest {
        mut catalog,
        target,
        page: page_task,
        rect,
        settings,
        next_number,
    } = request;
    let page = decode_chapter_page(&page_task.path)?;
    let overlay_size = page_task
        .overlay_size
        .unwrap_or([page.width() as usize, page.height() as usize]);
    let rect = overlay_rect_to_page(rect, overlay_size, page.width(), page.height())
        .ok_or_else(|| t!("cleaning.region.invalid_selection_size_error").to_string())?;
    if rect.width > CHAPTER_MAX_TEMPLATE_SIDE || rect.height > CHAPTER_MAX_TEMPLATE_SIDE {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.selection_too_large_error",
            width = rect.width,
            height = rect.height,
            limit = CHAPTER_MAX_TEMPLATE_SIDE
        ));
    }

    let sample_params = settings.chapter_sample_params();
    let (verdict, sample) =
        calibration_sample_from_page(&page, page_task.page_idx, rect, &sample_params)
            .map_err(chapter_engine_error)?;

    // A selection that cannot even serve as a detection template must not enter the catalog
    // at all; the verdict says why in the user's own terms.
    if !verdict.usable_as_template() {
        return Err(describe_sample_verdict(&verdict));
    }

    // Resolve which mark this belongs to. A sample with a measured signature can be
    // recognised; one without (no flat ring) has a shape but no identity and therefore
    // always starts a new mark.
    let signature = sample.as_ref().and_then(MarkSignature::from_flat_sample);
    let matched = match target {
        Some(index) if index < catalog.len() => Some(index),
        // The catalog changed under the request (the target mark was removed while the
        // button was down). Refuse rather than quietly creating a mark nobody asked for.
        Some(_) => return Err(t!("cleaning.tools.watermark.chapter.no_marks_error").to_string()),
        None => signature
            .as_ref()
            .and_then(|signature| find_matching_kind(&catalog.kinds, signature)),
    };

    let mut status = Vec::new();
    let index = match matched {
        Some(index) => {
            let template = catalog.kinds[index].template();
            if template.width() != rect.width || template.height() != rect.height {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.geometry_mismatch_error",
                    width = rect.width,
                    height = rect.height,
                    expected_width = template.width(),
                    expected_height = template.height()
                ));
            }
            if target.is_none() {
                status.push(tf!(
                    "cleaning.tools.watermark.chapter.sample_matched_existing",
                    name = catalog.marks[index].name.clone()
                ));
            }
            index
        }
        None => {
            let template =
                MarkTemplate::from_page(&page, rect).map_err(chapter_engine_error)?;
            let id = format!("mark-{next_number}");
            let name = tf!(
                "cleaning.tools.watermark.chapter.mark_default_name",
                number = next_number
            );
            let mark = ChapterMark::new(name.clone(), crop_page(&page, rect), page.width());
            catalog.push(WatermarkKind::new(id, template, alpha_blend_operator()), mark);
            status.push(tf!(
                "cleaning.tools.watermark.chapter.sample_created",
                name = name
            ));
            catalog.len() - 1
        }
    };

    if let Some(sample) = sample {
        let SampleVerdict::Calibration {
            level,
            ring_std,
            ring,
        } = verdict
        else {
            return Err(chapter_engine_error("calibration sample without a flat ring"));
        };
        catalog.kinds[index]
            .add_sample(sample)
            .map_err(chapter_engine_error)?;
        catalog.marks[index].crops.push(LibrarySample {
            image: crop_page(&page, rect),
            origin: StoredSampleOrigin::Page {
                page_index: page_task.page_idx,
                x: rect.x,
                y: rect.y,
            },
            background: stored_flat_background(level, ring_std, ring),
        });
        refit_chapter_kind(&mut catalog.kinds[index])?;
    } else if verdict.usable_as_template() {
        catalog.marks[index].template_only += 1;
    }
    status.push(describe_sample_verdict(&verdict));
    catalog.marks[index].last_verdict = Some(verdict);
    catalog.marks[index].page_width = page.width();
    refresh_chapter_preview(&mut catalog, index);

    Ok(ChapterSampleOutcome {
        catalog,
        selected: index,
        status: status.join(" "),
    })
}

/// Rebuilds one kind from a template crop and a calibration crop list.
///
/// The engine deliberately offers no way to REMOVE a sample from a `WatermarkKind` — a fit
/// is only ever grown — so swapping a mark's calibration means building the kind again.
/// The DETECTION reference stays the chapter's own template crop: only the calibration
/// changes when a library entry is adopted.
///
/// # Errors
/// A user-facing message for a template with no contrast, an empty anchor set, a crop whose
/// footprint does not match the template, or an invalid fit input.
fn rebuild_chapter_kind(
    id: &str,
    template_crop: &RgbaImage,
    anchors: &[u32],
    assumption: StoredAlphaAssumption,
    crops: &[LibrarySample],
) -> Result<WatermarkKind, String> {
    let rect = PixelRect::new(0, 0, template_crop.width(), template_crop.height());
    let template = MarkTemplate::from_page(template_crop, rect).map_err(chapter_engine_error)?;
    let mut kind = WatermarkKind::new(id.to_string(), template, alpha_blend_operator());
    if !anchors.is_empty() {
        kind.template_mut()
            .set_anchors(anchors)
            .map_err(chapter_engine_error)?;
    }
    kind.set_alpha_assumption(alpha_assumption_from_stored(assumption));
    for crop in crops {
        let crop_rect = PixelRect::new(0, 0, crop.image.width(), crop.image.height());
        let sample =
            CalibrationSample::from_page(&crop.image, 0, crop_rect, engine_background(crop.background))
                .map_err(chapter_engine_error)?;
        kind.add_sample(sample).map_err(chapter_engine_error)?;
    }
    refit_chapter_kind(&mut kind)?;
    Ok(kind)
}

/// Makes a library entry supply one mark's calibration.
///
/// The chapter's own crops are parked, not dropped, so the choice can be reversed. The
/// entry's footprint must equal the mark's, because its `c`/`s` planes are per pixel: a
/// model of a different footprint is not a model of this mark whatever its signature says.
///
/// The entry is handed in already loaded: reading it is I/O, and keeping it out of here is
/// what lets the decision half of auto-match be tested without touching the installation's
/// own library.
///
/// # Errors
/// A user-facing message for a footprint mismatch or an engine failure while refitting.
fn adopt_library_entry(
    kind: &mut WatermarkKind,
    mark: &mut ChapterMark,
    entry: LoadedEntry,
) -> Result<(), String> {
    let entry_id = entry.meta.id.clone();
    let (width, height) = (kind.template().width(), kind.template().height());
    if entry.meta.width != width || entry.meta.height != height {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.geometry_mismatch_error",
            width = entry.meta.width,
            height = entry.meta.height,
            expected_width = width,
            expected_height = height
        ));
    }
    if mark.parked.is_none() {
        mark.parked = Some(ParkedCalibration {
            crops: std::mem::take(&mut mark.crops),
            alpha_assumption: mark.alpha_assumption,
        });
    }
    let rebuilt = rebuild_chapter_kind(
        kind.id(),
        &mark.template_crop,
        kind.template().anchors(),
        entry.meta.alpha_assumption,
        &entry.samples,
    )?;
    *kind = rebuilt;
    mark.crops = entry.samples;
    mark.alpha_assumption = entry.meta.alpha_assumption;
    mark.adopted_entry = Some(entry_id.clone());
    mark.library_entry = Some(entry_id);
    Ok(())
}

/// Gives one mark its own calibration back after an adopted entry is dropped.
///
/// A no-op when nothing was adopted: without the guard this would replace a mark's own
/// samples with an empty parked set and silently destroy its calibration.
///
/// # Errors
/// A user-facing message for an engine failure while refitting.
fn release_library_entry(kind: &mut WatermarkKind, mark: &mut ChapterMark) -> Result<(), String> {
    if mark.adopted_entry.is_none() {
        return Ok(());
    }
    let parked = mark.parked.take().unwrap_or(ParkedCalibration {
        crops: Vec::new(),
        alpha_assumption: StoredAlphaAssumption::FromDeposit,
    });
    let rebuilt = rebuild_chapter_kind(
        kind.id(),
        &mark.template_crop,
        kind.template().anchors(),
        parked.alpha_assumption,
        &parked.crops,
    )?;
    *kind = rebuilt;
    mark.crops = parked.crops;
    mark.alpha_assumption = parked.alpha_assumption;
    mark.adopted_entry = None;
    Ok(())
}

/// Matches every fitted mark of `catalog` against the library and adopts the entries that
/// strengthen it, returning how many marks a library entry now calibrates.
///
/// This is the step that makes a second chapter of the same source instant: a mark whose
/// signature is already in the library needs no calibration of its own at all. Adoption is
/// deliberately conservative — see `watermark_entry::candidate_improves` — and never
/// overrides a choice the user already made (`ChapterMark::adopted_entry`).
///
/// `load` reads one entry by id. It is a parameter rather than a direct `load_entry` call
/// so the decision can be exercised against an in-memory library in tests.
pub(super) fn match_catalog_against_library<F>(
    catalog: &mut ChapterCatalog,
    library: &[EntrySummary],
    load: F,
) -> usize
where
    F: Fn(&str) -> Result<LoadedEntry, String>,
{
    let mut matched = 0usize;
    for index in 0..catalog.len() {
        let ChapterCatalog { kinds, marks } = &mut *catalog;
        let (Some(kind), Some(mark)) = (kinds.get_mut(index), marks.get_mut(index)) else {
            continue;
        };
        let Some(signature) = kind.signature() else {
            // No model and no flat sample: the mark has a shape but no measured identity,
            // and matching on shape is exactly what this feature must not do.
            mark.matches.clear();
            continue;
        };
        let footprint = (kind.template().width(), kind.template().height());
        mark.matches = rank_library_candidates(library, &signature, footprint);
        // The user's explicit choice wins outright; otherwise the best candidate is adopted
        // only when it is genuinely stronger than what this chapter measured itself.
        let chosen = match mark.pinned_entry.clone() {
            Some(entry_id) => Some(entry_id),
            None => mark
                .matches
                .first()
                .filter(|candidate| candidate_improves(candidate, kind.conditioning()))
                .map(|candidate| candidate.entry_id.clone()),
        };
        let Some(entry_id) = chosen else {
            continue;
        };
        match load(&entry_id).and_then(|entry| adopt_library_entry(kind, mark, entry)) {
            Ok(()) => matched += 1,
            Err(err) => ms_log::runtime_log::log_warn(format!(
                "[cleaning] watermark library entry {entry_id} does not fit mark {}: {err}",
                kind.id()
            )),
        }
    }
    matched
}

/// Scans the whole chapter: anchor discovery and flat-ring sample collection first, then
/// the fit, then the gain-verified rescan.
///
/// Detection is inherently two-pass — the gain test needs a model, so it cannot be the
/// first thing that runs. Pages are decoded ONE AT A TIME (a chapter is several strips of
/// ~700x18000 and holding them all would cost hundreds of megabytes); `rayon` inside the
/// engine parallelizes within a page.
///
/// # Errors
/// A user-facing message for an empty catalog, a page that cannot be decoded, or an engine
/// failure.
pub(super) fn run_chapter_scan(
    request: ChapterScanRequest,
    tx: &Sender<ChapterEvent>,
) -> Result<ChapterScanOutcome, String> {
    let ChapterScanRequest {
        mut catalog,
        pages,
        settings,
    } = request;
    if catalog.is_empty() {
        return Err(t!("cleaning.tools.watermark.chapter.no_marks_error").to_string());
    }
    if pages.is_empty() {
        return Err(t!("cleaning.tab.no_pages_error").to_string());
    }
    let params = settings.chapter_detection_params();
    let sample_params = settings.chapter_sample_params();
    let total = pages.len();
    let kinds = catalog.len();
    let mut anchor_hits: Vec<Vec<u32>> = vec![Vec::new(); kinds];
    for mark in &mut catalog.marks {
        mark.reset_scan_stats();
    }
    // A scan re-measures the chapter, so every mark goes back to its OWN calibration before
    // pass 1 collects into it. The adopted entry is re-applied in pass 1.5 (pinned choices
    // verbatim, automatic ones only when they still improve), which keeps the parked set a
    // clean record of what this chapter measured rather than a mixture of the two.
    for index in 0..kinds {
        let ChapterCatalog { kinds, marks } = &mut catalog;
        let (Some(kind), Some(mark)) = (kinds.get_mut(index), marks.get_mut(index)) else {
            continue;
        };
        release_library_entry(kind, mark)?;
    }

    // Pass 1: correlation-only bootstrap. Discover the anchor SET the source stamps at and
    // collect every occurrence whose ring is flat as a calibration sample.
    for (done, task) in pages.iter().enumerate() {
        let page = decode_chapter_page(&task.path)?;
        for (index, hits) in anchor_hits.iter_mut().enumerate() {
            let found = discover_anchors(&[&page], catalog.kinds[index].template(), &params)
                .map_err(chapter_engine_error)?;
            if !found.is_empty() {
                hits.extend(found);
                catalog.kinds[index]
                    .template_mut()
                    .set_anchors(hits)
                    .map_err(chapter_engine_error)?;
            }
            let occurrences = find_occurrences(&page, &catalog.kinds[index], &params)
                .map_err(chapter_engine_error)?;
            for occurrence in &occurrences {
                if catalog.kinds[index].samples().len() >= CHAPTER_MAX_AUTO_SAMPLES {
                    break;
                }
                let (verdict, sample) = calibration_sample_from_page(
                    &page,
                    task.page_idx,
                    occurrence.rect,
                    &sample_params,
                )
                .map_err(chapter_engine_error)?;
                let (
                    Some(sample),
                    SampleVerdict::Calibration {
                        level,
                        ring_std,
                        ring,
                    },
                ) = (sample, verdict)
                else {
                    continue;
                };
                catalog.kinds[index]
                    .add_sample(sample)
                    .map_err(chapter_engine_error)?;
                catalog.marks[index].crops.push(LibrarySample {
                    image: crop_page(&page, occurrence.rect),
                    origin: StoredSampleOrigin::Page {
                        page_index: task.page_idx,
                        x: occurrence.rect.x,
                        y: occurrence.rect.y,
                    },
                    background: stored_flat_background(level, ring_std, ring),
                });
            }
            catalog.marks[index].page_width = page.width();
        }
        let _ = tx.send(ChapterEvent::Progress {
            done: done + 1,
            total,
            label: t!("cleaning.tools.watermark.chapter.scan_stage_detect").to_string(),
        });
    }

    for index in 0..kinds {
        refit_chapter_kind(&mut catalog.kinds[index])?;
    }

    // Pass 1.5: AUTO-MATCH. A mark whose signature is already in the library needs no
    // calibration of its own, so the library is consulted before the verified rescan and
    // its model — when it is the stronger one — is what pass 2 then verifies against.
    let matched = match_catalog_against_library(&mut catalog, &list_entries(), load_entry);
    for index in 0..kinds {
        refresh_chapter_preview(&mut catalog, index);
    }

    // Pass 2: the gain-verified rescan, which is the only evidence removal is licensed on.
    let mut hits: Vec<ChapterPageHits> = Vec::with_capacity(pages.len());
    let mut report = ChapterScanReport {
        pages: pages.len(),
        matched,
        ..ChapterScanReport::default()
    };
    let mut gain_sums = vec![(0.0f32, 0.0f32, 0usize); kinds];
    for (done, task) in pages.iter().enumerate() {
        let page = decode_chapter_page(&task.path)?;
        let page_hits = scan_page(&page, task.page_idx, &catalog.kinds, &params)
            .map_err(chapter_engine_error)?;
        let mut records = Vec::with_capacity(page_hits.len());
        for hit in page_hits {
            let Some(mark) = catalog.marks.get_mut(hit.kind_index) else {
                continue;
            };
            mark.occurrences += 1;
            report.found += 1;
            match hit.occurrence.evidence {
                AcceptanceEvidence::Correlation => {
                    mark.unverified += 1;
                    report.unverified += 1;
                }
                AcceptanceEvidence::Gain { gain, snr } => {
                    let slot = &mut gain_sums[hit.kind_index];
                    slot.0 += gain;
                    slot.1 += snr;
                    slot.2 += 1;
                }
            }
            // The ring around an accepted occurrence is what the background histogram
            // counts: it is the level the removal there is exact at.
            if let Some(level) =
                validate_calibration_sample(&page, hit.occurrence.rect, &sample_params).level()
            {
                mark.histogram[chapter_histogram_bin(luma_of_level(level))] += 1;
            }
            records.push(ChapterHitRecord {
                mark_id: catalog.kinds[hit.kind_index].id().to_string(),
                occurrence: hit.occurrence,
            });
        }
        hits.push(ChapterPageHits {
            page_idx: task.page_idx,
            hits: records,
        });
        let _ = tx.send(ChapterEvent::Progress {
            done: done + 1,
            total,
            label: t!("cleaning.tools.watermark.chapter.scan_stage_verify").to_string(),
        });
    }
    for (index, (gain, snr, count)) in gain_sums.into_iter().enumerate() {
        if count == 0 {
            continue;
        }
        // Cast justification: a count of occurrences on one chapter, far below 2^24.
        let n = count as f32;
        catalog.marks[index].gain_mean = gain / n;
        catalog.marks[index].snr_mean = snr / n;
    }

    Ok(ChapterScanOutcome {
        catalog,
        hits,
        report,
    })
}

/// Removes every gain-verified occurrence, page by page, and streams the prepared patches
/// to the GUI thread.
///
/// A correlation-only accept, and every occurrence of a mark that has no model, is COUNTED
/// as refused rather than removed: subtracting a mark that is not there injects an inverse
/// one, and a silent skip would hide that from the user.
///
/// # Errors
/// A user-facing message for a page that cannot be decoded or an engine failure.
pub(super) fn run_chapter_apply(
    request: ChapterApplyRequest,
    tx: &Sender<ChapterEvent>,
) -> Result<ChapterApplyReport, String> {
    let ChapterApplyRequest {
        catalog,
        pages,
        hits,
    } = request;
    let mut report = ChapterApplyReport::default();
    let total = hits.iter().filter(|page| !page.hits.is_empty()).count();
    let mut done = 0usize;
    for page_hits in &hits {
        if page_hits.hits.is_empty() {
            continue;
        }
        let Some(task) = pages
            .iter()
            .find(|task| task.page_idx == page_hits.page_idx)
        else {
            continue;
        };
        let page = decode_chapter_page(&task.path)?;
        let page_size = [page.width() as usize, page.height() as usize];
        let mut patches: Vec<ChapterPatch> = Vec::new();
        let mut residual = RemovalResidual::default();
        let mut removed = 0usize;
        let mut refused = 0usize;
        for index in 0..catalog.len() {
            let mark_id = catalog.kinds[index].id();
            let occurrences: Vec<Occurrence> = page_hits
                .hits
                .iter()
                .filter(|record| record.mark_id == mark_id)
                .map(|record| record.occurrence.clone())
                .collect();
            if occurrences.is_empty() {
                continue;
            }
            let Some(model) = catalog.kinds[index].model() else {
                refused += occurrences.len();
                continue;
            };
            let safe: Vec<Occurrence> = occurrences
                .iter()
                .filter(|occurrence| occurrence.is_removal_safe())
                .cloned()
                .collect();
            refused += occurrences.len() - safe.len();
            if safe.is_empty() {
                continue;
            }
            let page_patches =
                remove_occurrences_on_page(&page, model, &safe).map_err(chapter_engine_error)?;
            for patch in page_patches {
                residual.merge(&patch.residual);
                removed += 1;
                patches.push(ChapterPatch {
                    rect: patch.rect,
                    image: egui::ColorImage::from_rgba_unmultiplied(
                        [patch.rect.width as usize, patch.rect.height as usize],
                        &patch.pixels,
                    ),
                });
            }
        }
        report.pages += 1;
        report.removed += removed;
        report.refused += refused;
        report.residual.merge(&residual);
        let _ = tx.send(ChapterEvent::ApplyBatch(Box::new(ChapterApplyBatch {
            page_idx: page_hits.page_idx,
            page_size,
            overlay_size: task.overlay_size,
            patches,
            removed,
            refused,
            residual,
        })));
        done += 1;
        let _ = tx.send(ChapterEvent::Progress {
            done,
            total,
            label: t!("cleaning.tools.watermark.chapter.apply_stage").to_string(),
        });
    }
    Ok(report)
}

/// Applies the user's override of an automatic library match.
///
/// The scan that produced the match is invalidated by the caller: the model changed, and
/// the gain test — the only evidence removal is licensed on — was run against the old one.
///
/// # Errors
/// A user-facing message for an unreadable entry, a footprint mismatch, or an engine
/// failure while refitting.
pub(super) fn run_chapter_use_match(request: ChapterMatchRequest) -> Result<ChapterSampleOutcome, String> {
    let ChapterMatchRequest {
        mut catalog,
        index,
        entry,
    } = request;
    if index >= catalog.len() {
        return Err(t!("cleaning.tools.watermark.chapter.no_marks_error").to_string());
    }
    let status = {
        let ChapterCatalog { kinds, marks } = &mut catalog;
        let (kind, mark) = (&mut kinds[index], &mut marks[index]);
        // The choice is PINNED before it is applied, so a later rescan rebuilds the same
        // decision instead of falling back to the automatic one.
        mark.pinned_entry.clone_from(&entry);
        match entry.as_deref() {
            Some(entry_id) => {
                adopt_library_entry(kind, mark, load_entry(entry_id)?)?;
                tf!(
                    "cleaning.tools.watermark.chapter.library_match_adopted_status",
                    name = mark
                        .matches
                        .iter()
                        .find(|candidate| candidate.entry_id == entry_id)
                        .map_or_else(|| entry_id.to_string(), |candidate| candidate.name.clone())
                )
            }
            None => {
                release_library_entry(kind, mark)?;
                t!("cleaning.tools.watermark.chapter.library_match_released_status").to_string()
            }
        }
    };
    refresh_chapter_preview(&mut catalog, index);
    Ok(ChapterSampleOutcome {
        catalog,
        selected: index,
        status,
    })
}

/// Rebuilds a catalog mark from a library entry.
///
/// The calibration crops are the reconstruction source: the engine only produces a model
/// through `WatermarkKind::refit`, so a loaded entry is refitted from its own measurements
/// and cannot disagree with them.
///
/// # Errors
/// A user-facing message for an unreadable entry or an engine failure.
pub(super) fn run_chapter_library_load(entry_id: &str) -> Result<ChapterLoadOutcome, String> {
    let entry = load_entry(entry_id)?;
    // The rebuild is SHARED with the reference intake and the sample editor
    // (`watermark_entry::rebuild_stored_kind`): a stored entry becomes a model in exactly one
    // place, so the three paths cannot disagree about how a kind is assembled.
    // An EMPTY entry has no mark yet: nothing to correlate against, no footprint to scan for
    // and no model to remove with. Refusing by name is the only honest answer — the alternative
    // is a chapter mark made of a template that does not exist.
    let Some(template) = entry.template.clone() else {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_entry_empty_error",
            name = entry.meta.name.clone()
        ));
    };
    let kind = rebuild_stored_kind(
        entry.meta.id.clone(),
        &template,
        (entry.meta.width, entry.meta.height),
        &entry.meta.anchors,
        entry.meta.alpha_assumption,
        &entry.samples,
    )?;
    let mut mark = ChapterMark::new(
        entry.meta.name.clone(),
        template,
        entry
            .meta
            .sources
            .first()
            .map_or(entry.meta.width, |source| source.page_width),
    );
    mark.library_entry = Some(entry.meta.id.clone());
    mark.alpha_assumption = entry.meta.alpha_assumption;
    // The crops are kept in LOCKSTEP with the kind's calibration samples: the engine never
    // hands sample pixels back, and the library needs them to write the entry again.
    mark.crops = entry.samples;
    mark.preview = kind.model().map(build_chapter_preview);
    mark.preview_revision = 1;
    let status = tf!(
        "cleaning.tools.watermark.chapter.library_loaded_status",
        name = mark.name.clone()
    );
    Ok(ChapterLoadOutcome { kind, mark, status })
}

/// Builds the whole save request for one mark.
///
/// Only the mark's flat calibration crops are handed over — an estimated per-pixel
/// background is derived from a model rather than measured, and persisting one would let a
/// later fold-in overwrite a measurement with an estimate. The request itself is assembled by
/// the SHARED `watermark_entry::save_request_from_kind`, so a chapter write and a library-side
/// rewrite cannot describe the same kind differently.
///
/// A mark with NO flat crops is written DELIBERATELY, not by omission. Such a mark was
/// discovered by correlation and never measured against a background the engine would call
/// flat, so the entry it produces carries its template and its footprint but no samples and no
/// model — a legitimate state the library expresses (`NotEnoughSamples`) and the one the user
/// then adds crops to from the panel. It is logged as such, because "the entry I saved has no
/// samples" is otherwise indistinguishable from a crop list that was lost on the way here.
pub(super) fn build_library_request(
    mark: &ChapterMark,
    kind: &WatermarkKind,
    source: Option<StoredSourceRef>,
) -> SaveEntryRequest {
    if mark.crops.is_empty() {
        ms_log::runtime_log::log_info(format!(
            "[cleaning] watermark library: saving mark {:?} with NO calibration crops. \
             The entry keeps its template and footprint and carries no model; samples can be \
             added to it from the library panel.",
            mark.name
        ));
    }
    save_request_from_kind(
        StoredEntryIdentity {
            entry_id: mark.library_entry.clone(),
            name: mark.name.clone(),
            alpha_assumption: mark.alpha_assumption,
            source,
        },
        kind,
        // A chapter mark ALWAYS has its template crop — it is what detection ran on — so this
        // path never writes a template-less entry. That state belongs to «+ новый» alone.
        Some(mark.template_crop.clone()),
        mark.crops.clone(),
    )
}

/// Search metadata of the open project: where this measurement came from.
///
/// The source key is the SERIES folder (the chapter's parent), because a mark belongs to a
/// publisher rather than to one chapter; the chapter folder is recorded next to it for the
/// user's own bookkeeping. Both are literal, never localized.
pub(super) fn chapter_source_ref(
    project: &ProjectData,
    page_width: u32,
    variant_id: &str,
    anchor_key: String,
) -> StoredSourceRef {
    let source_key = project
        .project_dir
        .parent()
        .and_then(|parent| parent.file_name())
        .or_else(|| project.project_dir.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    StoredSourceRef {
        source_key,
        page_width,
        anchor_key,
        variant_id: variant_id.to_string(),
        chapter: project
            .project_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string()),
    }
}
