/*
File: page_ops/plan/builder.rs

Purpose:
The plan accumulator and the plan entry point: `PlanBuilder`'s behaviour
(temp-name numbering, `from`-path dedup, renames, trash moves, JSON and owned
document rewrites, creations, warnings) and `build_plan`, which runs the
per-artifact planners in their fixed order. The `PlanBuilder` struct is
declared in `plan.rs`.

Key functions:
- build_plan(): snapshot + op -> PageOpPlan (pure; no filesystem access).
- to_pretty(): the project documents' JSON serialization.

Notes:
A child of `plan.rs`; pure like its parent (no filesystem access).
*/

use super::artifacts::{plan_bubbles, plan_clean_overlays, plan_creates, plan_layer_pngs, plan_layers_manifest, plan_src_pages, plan_text_info, plan_typing_masks};
use super::detection::plan_detection;
use super::geometry::{build_crop_geometry, build_split_geometry, build_stitch_geometry};
use super::permutation::permutation_for_op;
use super::{ChapterSnapshot, MoveDest, NewPageContent, OwnedDocFormat, OwnedDocKind, PageGeometry, PageOpPlan, PlanBuilder, PlannedCreate, PlannedJsonWrite, PlannedMove, PlannedTrashWrite, PlannedWriteFormat, TEMP_PREFIX, TEMP_SUFFIX, TRASH_DIR_NAME};
use crate::{PageOpError, PageOpKind};
use ms_config as config;
use serde_json::Value;
use std::collections::HashSet;

impl PlanBuilder {
    pub(super) fn new(trash_root: String, temp_id: u128) -> Self {
        Self {
            trash_root,
            temp_id,
            temp_counter: 0,
            planned_from: HashSet::new(),
            moves: Vec::new(),
            creates: Vec::new(),
            json_writes: Vec::new(),
            trash_writes: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Unique temp path in the same directory as `sibling_rel` (same-volume
    /// rename) that no chapter loader recognizes as an image/JSON artifact.
    pub(super) fn next_temp(&mut self, sibling_rel: &str) -> String {
        let n = self.temp_counter;
        self.temp_counter += 1;
        let dir = match sibling_rel.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        };
        let name = format!("{TEMP_PREFIX}{}_{n}{TEMP_SUFFIX}", self.temp_id);
        if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        }
    }

    /// Plans `from` -> `to`, skipping identity renames and duplicate sources.
    pub(super) fn rename(&mut self, from: String, to: String) {
        if from == to || !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Final { path: to },
        });
    }

    /// Plans moving `from` into the trash, preserving its title-relative path.
    pub(super) fn trash(&mut self, from: String) {
        if !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        let path = format!("{}/{from}", self.trash_root);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Trash { path },
        });
    }

    /// Plans discarding the file at `from` at commit time (phase A still moves
    /// it to a temp first, so the step stays reversible until commit).
    pub(super) fn discard(&mut self, from: String) {
        if !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Discard,
        });
    }

    /// Plans rewriting the plain JSON document at `target` with `content`;
    /// when the file currently exists (`had_original`) its original is
    /// discarded at commit (the remapped content supersedes it).
    pub(super) fn rewrite_json(&mut self, target: String, content: String, had_original: bool) {
        if had_original {
            self.discard(target.clone());
        }
        self.json_writes.push(PlannedJsonWrite { target, format: PlannedWriteFormat::Json, content });
    }

    /// Plans rewriting the EXISTING owned document `kind` in directory `dir`
    /// (title-relative) in the format the scan found it in: its ACTUAL file
    /// (`.json` or `.db`) is discarded at commit and replaced by `content`
    /// materialized in that same format, so the chapter keeps its format.
    pub(super) fn rewrite_owned_doc(&mut self, dir: &str, kind: OwnedDocKind, format: OwnedDocFormat, content: String) {
        let target = format!("{dir}/{}.{}", kind.file_stem(), format.extension());
        self.discard(target.clone());
        let format = match format {
            OwnedDocFormat::Json => PlannedWriteFormat::Json,
            OwnedDocFormat::Db => PlannedWriteFormat::Db { kind },
        };
        self.json_writes.push(PlannedJsonWrite { target, format, content });
    }

    /// Plans writing a NEW plain JSON document (no original to discard).
    pub(super) fn write_json(&mut self, target: String, content: String) {
        self.json_writes.push(PlannedJsonWrite { target, format: PlannedWriteFormat::Json, content });
    }

    /// Plans creating a new file at `target` from `content` (staged in phase A
    /// beside its destination, committed in phase B).
    pub(super) fn create(&mut self, target: String, content: NewPageContent) {
        let temp = self.next_temp(&target);
        self.creates.push(PlannedCreate {
            temp,
            target,
            content,
        });
    }

    pub(super) fn write_trash_extra(&mut self, rel_inside_trash: String, content: String) {
        let target = format!("{}/{rel_inside_trash}", self.trash_root);
        self.trash_writes.push(PlannedTrashWrite { target, content });
    }

    pub(super) fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }
}

/// Serializes a JSON value the way the app writes its project documents
/// (pretty, no trailing newline — matching the `ms_docstore` writes of the bubbles document
/// (`ms_models::bubbles_model::write_bubbles_snapshot_to`) and of the layer manifest).
pub(super) fn to_pretty(value: &Value) -> Result<String, PageOpError> {
    serde_json::to_string_pretty(value)
        .map_err(|err| PageOpError::Json(format!("serialize remapped document: {err}")))
}

/// Builds the full action plan for `op` from the chapter snapshot.
///
/// `trash_id` names both the trash subfolder and the temp-file namespace of
/// this transaction (the executor derives it from `SystemTime`).
///
/// # Errors
/// - [`PageOpError::InvalidOp`] for requests that do not apply to the snapshot
///   (bad indices, unsupported extensions, un-remappable legacy documents).
/// - [`PageOpError::Json`] when a page-keyed document cannot be re-serialized.
pub(crate) fn build_plan(
    snapshot: &ChapterSnapshot,
    op: &PageOpKind,
    trash_id: u128,
) -> Result<PageOpPlan, PageOpError> {
    let old_page_count = snapshot.page_file_names.len();
    let permutation = permutation_for_op(op, old_page_count)?;
    let map = &permutation.old_to_new;

    let trash_root = format!("{}/{TRASH_DIR_NAME}/{trash_id}", snapshot.chapter_rel);
    let mut b = PlanBuilder::new(trash_root.clone(), trash_id);

    // Stitch, split and crop are the operations that change a page's PIXEL
    // identity instead of merely re-keying files: they resolve their request
    // into affines first, and every planner below then treats an affected page
    // as "merged into" / "cut into" / "recropped" rather than "renamed to a new
    // key".
    let stitch;
    let split;
    let crop;
    let geometry = match op {
        PageOpKind::Stitch {
            placements,
            width,
            height,
            background,
        } => {
            stitch = build_stitch_geometry(
                snapshot,
                placements,
                [*width, *height],
                *background,
                map,
            )?;
            PageGeometry::Stitch(&stitch)
        }
        PageOpKind::Split {
            page_idx,
            axis,
            cuts,
            order,
            deleted,
        } => {
            let mut resolved =
                build_split_geometry(snapshot, *page_idx, *axis, cuts, order, deleted)?;
            for warning in resolved.take_warnings() {
                b.warn(warning);
            }
            split = resolved;
            PageGeometry::Split(&split)
        }
        PageOpKind::Crop {
            page_idx,
            quarter_turns,
            angle_deg,
            rect,
        } => {
            let mut resolved =
                build_crop_geometry(snapshot, *page_idx, *quarter_turns, *angle_deg, *rect)?;
            for warning in resolved.take_warnings() {
                b.warn(warning);
            }
            crop = resolved;
            // Degrading user data must never be silent: name every lossy
            // consequence of a fine angle up front, before the per-document
            // warnings that repeat it with their own counts.
            if crop.resamples() {
                b.warn(format!(
                    "crop of page {page_idx}: the {:.3} deg fine angle RESAMPLES every \
                     page-sized raster (a pure quarter turn would not); stored axis-aligned \
                     rectangles (bubble rect_coords and text_areas) degrade to the bounding \
                     box of the rotated rectangle, and the page's text-detection files are \
                     moved to the trash instead of being remapped wrongly",
                    crop.rotation().total_degrees()
                ));
            }
            PageGeometry::Crop(&crop)
        }
        PageOpKind::Move { .. }
        | PageOpKind::InsertFiles { .. }
        | PageOpKind::CreateBlank { .. }
        | PageOpKind::Delete { .. } => PageGeometry::None,
    };

    if snapshot.has_alt_vers {
        // `alt_vers/` pairs with pages by sorted position and has no per-file
        // page key (see this module's MODULE_README), so any change to the page
        // COUNT shifts that pairing and there is nothing to rename.
        let shift = match geometry {
            PageGeometry::Stitch(geo) => Some(format!(
                "merging {} page(s) into one shifts their alignment with the pages",
                geo.source_count()
            )),
            // The alignment shift follows the page COUNT, so only the parts
            // that actually become pages count here.
            PageGeometry::Split(geo) => Some(format!(
                "cutting one page into {} part(s) shifts their alignment with the pages",
                geo.kept_count()
            )),
            // A crop does NOT change the page count, so the positional pairing
            // itself survives — page k still pairs with the k-th alternate
            // version. What changes is the page's SIZE: the alternate version
            // is now a differently sized (and differently oriented) image of the
            // same page, which the stamp tool overlays by position, so it no
            // longer lines up with the page it belongs to.
            PageGeometry::Crop(geo) => {
                let [w, h] = geo.new_size();
                Some(format!(
                    "cropping page {} to {w}x{h} keeps their positional pairing but leaves \
                     the alternate version at the page's OLD size and orientation",
                    geo.source_old_idx()
                ))
            }
            PageGeometry::None => None,
        };
        if let Some(shift) = shift {
            b.warn(format!(
                "{}/{}: alternate versions are position-matched and are NOT remapped; \
                 {shift}",
                snapshot.chapter_rel,
                config::ALT_VERS_DIR,
            ));
        }
    }

    plan_src_pages(&mut b, snapshot, map, geometry)?;
    for tree in [&snapshot.committed, &snapshot.unsaved] {
        plan_clean_overlays(&mut b, snapshot, tree, map, geometry);
        plan_layer_pngs(&mut b, tree, map, geometry)?;
        plan_layers_manifest(&mut b, tree, map, geometry)?;
        plan_text_info(&mut b, tree, map, geometry)?;
        plan_typing_masks(&mut b, snapshot, tree, map, geometry);
        plan_bubbles(&mut b, tree, map, geometry)?;
    }
    plan_detection(&mut b, snapshot, map, geometry)?;
    plan_creates(&mut b, snapshot, &permutation);

    Ok(PageOpPlan {
        old_to_new: permutation.old_to_new,
        new_page_count: permutation.new_page_count,
        trash_root,
        moves: b.moves,
        creates: b.creates,
        json_writes: b.json_writes,
        trash_writes: b.trash_writes,
        warnings: b.warnings,
    })
}
