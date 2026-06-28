/*
File: models/layer_model/migrate.rs

Purpose:
One-shot EAGER migration of a legacy text chapter (`text_info.json`) into the canonical schema-v3
inline `layers.json` form, run once in the background on chapter open. The lazy on-read migration
(`layer_doc::ensure_page_loaded`) still handles display correctness frame-to-frame; this module makes
the rewrite happen up-front and PERSISTENTLY, so a chapter is converted once and never re-read from
`text_info.json` afterwards.

Design (per user decisions):
- Pixels are PRESERVED by RENAMING (moving) the existing overlay PNG into the v3 uid-keyed name
  (`persist::text_image_file_name`) in the canonical committed `layers/` dir — never re-rendered.
- `text_info.json` becomes `text_info.json.bak` (the rollback anchor) — and only AFTER `layers.json`
  for every page has been written and PNGs renamed, so a crash mid-migration leaves the legacy file
  intact and migration simply re-runs.
- Geometry decode reuses the SHARED codec (`text_payload`), with the FULL chapter `page_sizes` map so
  the absolute-ribbon family resolves correctly.

This module owns no UI / image-render code: it only moves files and rewrites `layers.json` through
the existing `persist` writers. The typing tab spawns it on a worker thread and evicts the doc pages
on completion so both tabs re-project the migrated v3 data.
*/

use super::{persist, text_payload};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const TEXT_INFO_FILE: &str = "text_info.json";

/// Outcome of an eager chapter migration, for logging / tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// Number of text overlays migrated into inline v3 nodes.
    pub migrated_overlays: usize,
    /// Overlays whose original PNG was renamed into the v3 name (pixels preserved).
    pub renamed_pngs: usize,
    /// Overlays kept WITHOUT an image because their original PNG was missing (logged, never dropped).
    pub missing_pngs: usize,
    /// The pages that gained inline text nodes (the caller evicts these from the doc to re-project).
    pub migrated_pages: Vec<usize>,
    /// The `.bak` path the legacy `text_info.json` was moved to.
    pub backup_path: Option<PathBuf>,
}

/// Detects whether the chapter needs eager migration and, if so, returns the directory holding the
/// authoritative legacy `text_info.json` to migrate (the canonical `layers/` dir is preferred when both
/// locations have one; else the legacy `text_images/` dir). Returns `None` when there is nothing to
/// migrate.
///
/// IDEMPOTENCY IS ON THE TARGET (`layers/layers.json`), not on the presence of `text_info.json`: if the
/// committed manifest is ALREADY v3-inline (carries text nodes with `render_data`), migration has
/// already happened and this returns `None` REGARDLESS of a lingering `text_info.json` in EITHER dir.
/// This is the fix for the ВВД/13 incident — without it, after the primary `layers/text_info.json` was
/// migrated and `.bak`'d, a STALE secondary `text_images/text_info.json` re-triggered migration and
/// overwrote the good v3 data with the partial stale set. A genuinely un-migrated chapter (a
/// `text_info.json` exists and the manifest has no inline text) still migrates.
#[must_use]
pub fn chapter_needs_migration(layers_dir: &Path, legacy_text_images_dir: &Path) -> Option<PathBuf> {
    // Already migrated? If the committed manifest carries any inline (v3) TEXT node, the chapter has
    // been migrated — never re-run (a lingering stale `text_info.json` must NOT re-trigger).
    if manifest_has_inline_text(layers_dir) {
        return None;
    }

    // The legacy file may live in the committed `layers/` dir (post `text_images→layers` move) or the
    // older `text_images/` dir. Prefer the canonical `layers/` location (newer/complete).
    let source_dir = if layers_dir.join(TEXT_INFO_FILE).is_file() {
        layers_dir.to_path_buf()
    } else if legacy_text_images_dir.join(TEXT_INFO_FILE).is_file() {
        legacy_text_images_dir.to_path_buf()
    } else {
        return None;
    };

    // Require at least one TEXT overlay. (A `text_info.json` carrying only image overlays — which the
    // app treats as rasters elsewhere — is not migrated, so we don't spuriously `.bak` it.)
    let entries = text_payload::read_overlay_entries(&[source_dir.as_path()]);
    let has_text_overlay = entries.iter().filter_map(Value::as_object).any(|obj| {
        obj.get("overlay_type").and_then(Value::as_str) != Some("image")
            && obj
                .get("uid")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
    });
    has_text_overlay.then_some(source_dir)
}

/// True when `layers_dir`'s `layers.json` already carries an inline (schema-v3) TEXT node (a text node
/// with `render_data`). Used as the idempotency anchor: such a manifest is the product of a completed
/// migration / typing flush, so the chapter must never be re-migrated from a legacy `text_info.json`.
fn manifest_has_inline_text(layers_dir: &Path) -> bool {
    super::compat::read_manifest(&layers_dir.join("layers.json"))
        .ok()
        .flatten()
        .is_some_and(|m| {
            m.pages.iter().any(|p| {
                p.tree.iter().any(|r| {
                    matches!(r.kind, crate::models::layer_model::manifest::LayerKindRec::Text)
                        && r.render_data.is_some()
                })
            })
        })
}

/// Eagerly migrates the chapter's legacy text overlays to inline schema-v3 nodes in `layers_dir`'s
/// `layers.json`, preserving pixels by RENAMING each overlay PNG into the v3 name, then moves the
/// legacy `text_info.json` to `text_info.json.bak` (rollback anchor) LAST. Idempotent: if there is
/// nothing to migrate (see [`chapter_needs_migration`]) it is a clean no-op that touches no files.
///
/// `page_sizes` is the FULL chapter page-pixel-size map (the absolute-ribbon migration needs every
/// page's aspect ratio). `unsaved_layers_dir` is consulted only as an extra search location for a
/// PNG that staging already wrote.
pub fn migrate_chapter_to_v3(
    layers_dir: &Path,
    legacy_text_images_dir: &Path,
    unsaved_layers_dir: Option<&Path>,
    page_sizes: &HashMap<usize, [usize; 2]>,
) -> Result<MigrationReport, String> {
    let Some(source_dir) = chapter_needs_migration(layers_dir, legacy_text_images_dir) else {
        return Ok(MigrationReport::default()); // nothing to do
    };

    // Directories an overlay PNG may live in, in search order: the legacy source dir, the canonical
    // committed `layers/` dir, the legacy `text_images/` dir, and the unsaved staging dir.
    let mut png_dirs: Vec<PathBuf> = vec![source_dir.clone(), layers_dir.to_path_buf(), legacy_text_images_dir.to_path_buf()];
    if let Some(u) = unsaved_layers_dir {
        png_dirs.push(u.to_path_buf());
    }
    png_dirs.dedup();

    // Read + cross-entry-migrate the legacy overlays once (ribbon/top-left → modern img_u/img_v),
    // resolving each overlay PNG footprint from the candidate dirs.
    let raw_entries = text_payload::read_overlay_entries(&[source_dir.as_path()]);
    let png_dirs_for_footprint = png_dirs.clone();
    let migrated_entries = text_payload::migrate_overlay_entries(&raw_entries, page_sizes, |obj| {
        overlay_png_path(obj, &png_dirs_for_footprint)
            .and_then(|p| image::image_dimensions(&p).ok())
            .map_or((0.0, 0.0), |(w, h)| (w as f32, h as f32))
    });

    // Group TEXT overlays by page. (Image overlays are left in `text_info.json`; they are display-only
    // legacy and not part of the inline text model.)
    let mut by_page: HashMap<usize, Vec<Map<String, Value>>> = HashMap::new();
    for entry in &migrated_entries {
        let Some(obj) = entry.as_object() else { continue };
        if obj.get("overlay_type").and_then(Value::as_str) == Some("image") {
            continue;
        }
        let Some(uid) = obj.get("uid").and_then(Value::as_str) else { continue };
        if uid.is_empty() {
            continue;
        }
        let page = obj.get("img_idx").and_then(Value::as_u64).unwrap_or(0) as usize;
        by_page.entry(page).or_default().push(obj.clone());
    }

    let mut report = MigrationReport::default();

    // Build + write the inline v3 text nodes per page, renaming PNGs as we go. Do ALL pages before the
    // destructive `.bak` rename so a crash leaves `text_info.json` intact and migration re-runs.
    for (&page_idx, overlays) in &by_page {
        let page_size = page_sizes.get(&page_idx).copied().unwrap_or([1, 1]);
        let mut outs: Vec<persist::TextPayloadOut> = Vec::with_capacity(overlays.len());
        // Only TEXT overlays reach here (image overlays were filtered out), so names follow the same
        // "Текст {n}" scheme the lazy load path produces, in text_info.json order.
        let mut text_n = 0usize;
        for obj in overlays {
            let uid = obj
                .get("uid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let placement = text_payload::decode_overlay_placement(obj, page_size);
            let render_data = obj.get("render_data").cloned().unwrap_or(Value::Null);
            let mask_clip = obj.get("mask_clip_enabled").and_then(Value::as_bool);
            let layer_idx = obj.get("layer_idx").and_then(Value::as_u64).unwrap_or(0) as u32;
            text_n += 1;
            let name = format!("Текст {text_n}");

            // RENAME (move) the original PNG into the v3 name, preserving its exact bytes. Missing PNG
            // → keep the overlay WITHOUT an image (never drop), logged.
            //
            // KNOWN LIMITATION (by design): an overlay whose original PNG was ALREADY missing pre-
            // migration becomes an inline node with `rendered_file: None`. The doc build skips imageless
            // text nodes, so after `text_info.json → .bak` such an overlay is invisible/uneditable in
            // the tabs. This is NOT new data loss — its text + geometry survive in `layers.json`, and
            // the legacy entry survives in `text_info.json.bak`. Re-rendering its image from
            // `render_data` would require the typing tab's text-render engine, which the model layer
            // does not have (out of scope). Re-rendering happens naturally on the next text edit.
            let rendered_file =
                rename_overlay_png(obj, &uid, page_idx, layers_dir, &png_dirs, &mut report);

            outs.push(persist::TextPayloadOut {
                uid: uid.clone(),
                name,
                z: 0, // pin/z preserved from the existing manifest by `write_page_text_payload`
                layer_idx,
                pinned: false,
                visible: true,
                opacity: 1.0,
                group_uid: None,
                pinned_by_group: false,
                payload_uid: uid,
                render_data,
                // The codec already produced canonical geometry (center-anchored, rotation in radians).
                transform: placement.transform,
                deform: placement.deform,
                rendered_file,
                mask_clip,
            });
            report.migrated_overlays += 1;
        }
        // Write the inline payload into the canonical committed `layers/` dir; `write_page_text_payload`
        // preserves rasters / PS groups / text-group bands + PS-owned pin/z/group fields, and does NOT
        // touch the renamed PNGs.
        persist::write_page_text_payload(layers_dir, None, page_idx, &outs)?;
        report.migrated_pages.push(page_idx);

        // If the UNSAVED staging manifest ALREADY HAS THIS PAGE (the user has uncommitted edits for it,
        // e.g. PS rasters), mirror the migrated text into it ADDITIVELY. The doc reads the unsaved
        // manifest as primary, so without this a migrated page whose unsaved manifest lacks text would
        // show none (the legacy `text_info.json` is about to become `.bak`). The renamed PNGs live in
        // the committed dir; the doc finds them via its committed fallback. A page ABSENT from the
        // unsaved manifest is left untouched — the doc falls through to the committed (migrated)
        // manifest for it, so we must not create a text-only page in unsaved that would shadow
        // committed rasters.
        //
        // ADDITIVE-PER-UID (data safety): `write_page_text_payload` REPLACES all text nodes on a page,
        // so a plain mirror of the `text_info.json`-derived set would (1) DROP an overlay staged only
        // in `_unsaved` (created, never saved-to-project → no text_info.json entry) and (2) CLOBBER a
        // fresher live edit flushed to unsaved during the migration window. So we KEEP every text node
        // already inline in the unsaved page (already-v3 or a fresher edit) and only ADD migrated nodes
        // for uids NOT already inline there.
        if let Some(unsaved) = unsaved_layers_dir
            && unsaved_page_exists(Some(unsaved), page_idx)
        {
            let merged = additive_merge_for_unsaved(unsaved, page_idx, &outs)?;
            persist::write_page_text_payload(unsaved, None, page_idx, &merged)?;
        }
    }
    report.migrated_pages.sort_unstable();

    // DESTRUCTIVE step LAST: retire the legacy `text_info.json` in BOTH locations (`layers/` and
    // `text_images/`), not just the chosen source, by renaming each to `text_info.json.bak` (without
    // clobbering an existing `.bak`). This neutralizes a STALE secondary so it can never re-trigger
    // migration (the ВВД/13 incident root cause). The primary idempotency anchor is the v3-inline
    // manifest (`chapter_needs_migration`); this is belt-and-suspenders. Done last so a crash leaves
    // the legacy files intact and the migration re-runs.
    for dir in [layers_dir, legacy_text_images_dir] {
        let src = dir.join(TEXT_INFO_FILE);
        if !src.is_file() {
            continue;
        }
        let bak = next_backup_path(dir);
        std::fs::rename(&src, &bak)
            .map_err(|e| format!("rename {} -> {}: {e}", src.display(), bak.display()))?;
        if dir == source_dir.as_path() {
            report.backup_path = Some(bak);
        }
    }

    Ok(report)
}

/// True when the unsaved staging `layers.json` already contains `page_idx` (so mirroring migrated text
/// into it is needed). `None` dir → false.
fn unsaved_page_exists(unsaved_layers_dir: Option<&Path>, page_idx: usize) -> bool {
    let Some(dir) = unsaved_layers_dir else {
        return false;
    };
    super::compat::read_manifest(&dir.join("layers.json"))
        .ok()
        .flatten()
        .is_some_and(|m| m.page(page_idx).is_some())
}

/// Builds the ADDITIVE text set to write into the unsaved staging page: every text node ALREADY inline
/// in the unsaved manifest is KEPT verbatim (it is either already-v3 or a fresher live edit — never
/// overwritten or dropped), and the migrated nodes are added only for uids NOT already inline there.
/// This prevents (a) dropping an overlay staged only in `_unsaved` (no `text_info.json` entry) and
/// (b) clobbering a fresh edit flushed during the migration window. `write_page_text_payload` still
/// preserves the unsaved page's rasters / PS groups / PS-owned pin-z-group on the rewrite.
fn additive_merge_for_unsaved(
    unsaved_layers_dir: &Path,
    page_idx: usize,
    migrated: &[persist::TextPayloadOut],
) -> Result<Vec<persist::TextPayloadOut>, String> {
    let existing = persist::load_page_text_nodes(unsaved_layers_dir, None, page_idx)?;
    let mut out: Vec<persist::TextPayloadOut> = Vec::with_capacity(existing.len() + migrated.len());
    let mut kept_uids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Keep each unsaved node that already carries an inline v3 payload, rebuilt 1:1 from disk so the
    // rewrite does not lose it. (Pin/z/group are re-preserved by `write_page_text_payload`'s merge.)
    for n in existing {
        let Some(inline) = n.inline else {
            // A reference-only (non-inline) unsaved node has no self-sufficient payload; let the
            // migrated node (which carries render_data + the renamed PNG) supply it below.
            continue;
        };
        kept_uids.insert(n.uid.clone());
        out.push(persist::TextPayloadOut {
            uid: n.uid.clone(),
            name: n.name,
            z: 0, // preserved from the existing manifest by `write_page_text_payload`
            layer_idx: n.layer_idx,
            pinned: n.pinned,
            visible: n.visible,
            opacity: n.opacity,
            group_uid: n.group_uid,
            pinned_by_group: n.pinned_by_group,
            payload_uid: n.payload_uid,
            render_data: inline.render_data,
            transform: inline.transform.unwrap_or(super::manifest::TransformRec {
                cx: 0.0,
                cy: 0.0,
                rotation: 0.0,
                scale: 1.0,
            }),
            deform: inline.deform,
            rendered_file: inline.rendered_file,
            mask_clip: inline.mask_clip,
        });
    }

    // Add migrated nodes only for uids not already kept (i.e. not already inline in unsaved).
    for m in migrated {
        if !kept_uids.contains(&m.uid) {
            out.push(m.clone());
        }
    }
    Ok(out)
}

/// Resolves the on-disk path of an overlay's PNG (its `file` field) across the candidate dirs.
fn overlay_png_path(obj: &Map<String, Value>, dirs: &[PathBuf]) -> Option<PathBuf> {
    let file = obj
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let name = Path::new(file).file_name()?.to_str()?;
    dirs.iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Renames (moves) an overlay's original PNG into the v3 uid-keyed name in `layers_dir`, preserving
/// its exact bytes. Returns `Some(v3_name)` on success. If the original PNG is already at the v3 name
/// (idempotent re-run) it is reused. If the PNG is genuinely missing the overlay is kept WITHOUT an
/// image (`None`, logged) — never dropped.
fn rename_overlay_png(
    obj: &Map<String, Value>,
    uid: &str,
    page_idx: usize,
    layers_dir: &Path,
    png_dirs: &[PathBuf],
    report: &mut MigrationReport,
) -> Option<String> {
    let v3_name = persist::text_image_file_name(page_idx, uid);
    let dst = layers_dir.join(&v3_name);

    // Already at the v3 name (e.g. a partial earlier run): reuse it.
    if dst.is_file() {
        report.renamed_pngs += 1;
        return Some(v3_name);
    }

    let Some(src) = overlay_png_path(obj, png_dirs) else {
        report.missing_pngs += 1;
        crate::runtime_log::log_warn(format!(
            "[migrate] overlay '{uid}' page {page_idx}: original PNG missing; keeping overlay without an image"
        ));
        return None;
    };

    if let Err(e) = std::fs::create_dir_all(layers_dir) {
        crate::runtime_log::log_warn(format!("[migrate] create {}: {e}", layers_dir.display()));
        return None;
    }
    // Prefer a rename (atomic, preserves bytes); fall back to copy+remove across filesystems.
    match std::fs::rename(&src, &dst) {
        Ok(()) => {
            report.renamed_pngs += 1;
            Some(v3_name)
        }
        Err(_) => match std::fs::copy(&src, &dst) {
            Ok(_) => {
                let _ = std::fs::remove_file(&src);
                report.renamed_pngs += 1;
                Some(v3_name)
            }
            Err(e) => {
                report.missing_pngs += 1;
                crate::runtime_log::log_warn(format!(
                    "[migrate] overlay '{uid}' page {page_idx}: move PNG {} -> {} failed: {e}; keeping overlay without an image",
                    src.display(),
                    dst.display()
                ));
                None
            }
        },
    }
}

/// `text_info.json.bak`, or `.bak.1`, `.bak.2`, … if a backup already exists (never clobber a prior
/// rollback record).
fn next_backup_path(dir: &Path) -> PathBuf {
    let base = dir.join(format!("{TEXT_INFO_FILE}.bak"));
    if !base.exists() {
        return base;
    }
    for n in 1u32.. {
        let candidate = dir.join(format!("{TEXT_INFO_FILE}.bak.{n}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base // unreachable in practice
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::layer_model::manifest::TransformRec;
    use eframe::egui::ColorImage;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ml_migrate_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes a tiny PNG with a recognizable byte pattern (so we can prove bytes are preserved).
    fn write_png(path: &Path, w: u32, h: u32, rgba: [u8; 4]) {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(rgba));
        img.save(path).unwrap();
    }

    fn write_text_info(dir: &Path, entries: Value) {
        fs::write(
            dir.join(TEXT_INFO_FILE),
            serde_json::to_string_pretty(&entries).unwrap(),
        )
        .unwrap();
    }

    /// A modern center-anchored overlay entry.
    fn overlay(uid: &str, page: usize, file: &str, x: f64, y: f64) -> Value {
        serde_json::json!({
            "uid": uid, "img_idx": page, "overlay_type": "text", "file": file,
            "img_x_px": x, "img_y_px": y, "rotation_deg": 0.0, "scale": 1.0,
            "mask_clip_enabled": true,
            "render_data": { "text": uid }
        })
    }

    /// Counts page entries + total text nodes in a `layers.json`.
    fn manifest_pages_and_texts(dir: &Path) -> (Vec<usize>, usize) {
        let m = super::super::compat::read_manifest(&dir.join("layers.json"))
            .unwrap()
            .unwrap();
        let mut pages: Vec<usize> = m.pages.iter().map(|p| p.img_idx).collect();
        pages.sort_unstable();
        let texts = m
            .pages
            .iter()
            .flat_map(|p| p.tree.iter())
            .filter(|r| matches!(r.kind, crate::models::layer_model::manifest::LayerKindRec::Text))
            .count();
        (pages, texts)
    }

    #[test]
    fn repro_dual_location_truncation_and_regression() {
        // REPRO of the ВВД/13 incident: a chapter has text_info.json in BOTH `layers/` (COMPLETE,
        // 5 pages) and `text_images/` (STALE, 2 pages). Run migration twice (two opens). EXPECTED after
        // the fix: run 1 migrates the COMPLETE 5-page set; run 2 is a clean no-op (idempotent) and does
        // NOT regress to the stale set or drop pages 2-4.
        let root = tmp("repro_dual");
        let layers = root.join("layers");
        let text_images = root.join("text_images");
        fs::create_dir_all(&layers).unwrap();
        fs::create_dir_all(&text_images).unwrap();

        // COMPLETE set in layers/: pages 0..5, one text overlay per page, NEWER geometry (x=100*page).
        let complete: Vec<Value> = (0..5)
            .map(|p| overlay(&format!("u{p}"), p, &format!("u{p}.png"), 100.0 * (p as f64 + 1.0), 50.0))
            .collect();
        write_text_info(&layers, Value::Array(complete));
        // STALE set in text_images/: pages 0..2, SAME uids u0/u1 but OLDER geometry (x=1.0).
        let stale: Vec<Value> = (0..2)
            .map(|p| overlay(&format!("u{p}"), p, &format!("u{p}.png"), 1.0, 1.0))
            .collect();
        write_text_info(&text_images, Value::Array(stale));
        // PNGs for all overlays (in layers/, where the renamer puts them).
        for p in 0..5 {
            write_png(&layers.join(format!("u{p}.png")), 2, 2, [p as u8, 0, 0, 255]);
        }
        let page_sizes: HashMap<usize, [usize; 2]> =
            (0..5).map(|p| (p, [1000, 1000])).collect();

        // RUN 1 (first open): migrates the COMPLETE layers/ set.
        let r1 = migrate_chapter_to_v3(&layers, &text_images, None, &page_sizes).unwrap();
        let (pages1, texts1) = manifest_pages_and_texts(&layers);
        assert_eq!(pages1, vec![0, 1, 2, 3, 4], "run 1 migrated all 5 pages");
        assert_eq!(texts1, 5, "run 1 migrated all 5 text overlays");
        assert!(r1.backup_path.is_some());
        // Geometry is the COMPLETE (newer) one: u1 has cx = 200.
        let nodes1 = persist::load_page_text_nodes(&layers, None, 1).unwrap();
        let cx1 = nodes1[0].inline.as_ref().unwrap().transform.unwrap().cx;
        assert!((cx1 - 200.0).abs() < 1e-3, "run 1 used the COMPLETE geometry (cx=200)");

        // RUN 2 (second open): the STALE text_images/text_info.json still exists. Must be a NO-OP.
        let r2 = migrate_chapter_to_v3(&layers, &text_images, None, &page_sizes).unwrap();
        let (pages2, texts2) = manifest_pages_and_texts(&layers);
        assert_eq!(pages2, vec![0, 1, 2, 3, 4], "run 2 must NOT drop pages 2-4 (no truncation)");
        assert_eq!(texts2, 5, "run 2 must NOT regress the overlay set");
        assert_eq!(r2.migrated_overlays, 0, "run 2 is a clean no-op");
        let nodes2 = persist::load_page_text_nodes(&layers, None, 1).unwrap();
        let cx2 = nodes2[0].inline.as_ref().unwrap().transform.unwrap().cx;
        assert!((cx2 - 200.0).abs() < 1e-3, "run 2 kept the COMPLETE geometry (no stale regression)");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[ignore = "forensic repro against the real ВВД/13 snapshot; run explicitly"]
    fn forensic_repro_vvd13() {
        let snap = Path::new("/tmp/claude-1000/-home-vasyanator-hdd-Projects-ManhwaStudio/f696ad92-3cbf-4112-9043-69057eda940c/scratchpad/vvd13_snapshot");
        if !snap.exists() {
            eprintln!("snapshot missing, skipping");
            return;
        }
        let root = tmp("forensic");
        let layers = root.join("layers");
        let text_images = root.join("text_images");
        fs::create_dir_all(&layers).unwrap();
        fs::create_dir_all(&text_images).unwrap();
        // Recreate the PRE-migration dual-location state from the .bak files.
        fs::copy(snap.join("layers/text_info.json.bak"), layers.join(TEXT_INFO_FILE)).unwrap();
        fs::copy(snap.join("text_images/text_info.json.bak"), text_images.join(TEXT_INFO_FILE)).unwrap();
        // Synthesize a PNG for every overlay file referenced (in BOTH sources), in layers/.
        for src in [layers.join(TEXT_INFO_FILE), text_images.join(TEXT_INFO_FILE)] {
            let arr: Value = serde_json::from_str(&fs::read_to_string(&src).unwrap()).unwrap();
            for o in arr.as_array().unwrap() {
                if let Some(f) = o.get("file").and_then(Value::as_str) {
                    let _ = write_png(&layers.join(f), 2, 2, [1, 2, 3, 255]);
                }
            }
        }
        let page_sizes: HashMap<usize, [usize; 2]> = (0..23).map(|p| (p, [1000, 1600])).collect();

        // RUN 1: migrates the COMPLETE layers/ set (184 overlays across 23 pages).
        let r1 = migrate_chapter_to_v3(&layers, &text_images, None, &page_sizes).unwrap();
        let (pages1, texts1) = manifest_pages_and_texts(&layers);
        eprintln!("RUN1: pages={} texts={} migrated={}", pages1.len(), texts1, r1.migrated_overlays);
        assert_eq!(pages1.len(), 23, "run 1 migrated all 23 pages");
        assert_eq!(texts1, 184, "run 1 migrated all 184 text overlays");
        // BOTH text_info.json locations are retired so the stale secondary can't re-trigger.
        assert!(!layers.join(TEXT_INFO_FILE).is_file());
        assert!(!text_images.join(TEXT_INFO_FILE).is_file());

        // RUN 2 (the disaster trigger): the gate blocks re-migration; NO truncation, NO regression.
        let r2 = migrate_chapter_to_v3(&layers, &text_images, None, &page_sizes).unwrap();
        let (pages2, texts2) = manifest_pages_and_texts(&layers);
        eprintln!("RUN2: pages={} texts={} migrated={}", pages2.len(), texts2, r2.migrated_overlays);
        assert_eq!(r2.migrated_overlays, 0, "run 2 is a clean no-op (no stale re-migration)");
        assert_eq!(pages2.len(), 23, "no pages dropped (no truncation)");
        assert_eq!(texts2, 184, "all 184 overlays preserved (no regression to the stale 68)");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[ignore = "recovery-procedure verification against the real ВВД/13 snapshot; run explicitly"]
    fn verify_recovery_procedure_vvd13() {
        // Verifies the RECOVERY procedure on a COPY of the corrupted ВВД/13 state:
        //   1. restore layers/text_info.json from the 184-overlay .bak,
        //   2. strip the inline TEXT nodes from layers.json (preserve the 2 page-0 rasters + groups),
        //   3. run the FIXED migration → 184 text nodes across 23 pages, rasters intact, _text.png reused.
        let snap = Path::new("/tmp/claude-1000/-home-vasyanator-hdd-Projects-ManhwaStudio/f696ad92-3cbf-4112-9043-69057eda940c/scratchpad/vvd13_snapshot");
        if !snap.exists() {
            eprintln!("snapshot missing, skipping");
            return;
        }
        let root = tmp("recovery");
        let layers = root.join("layers");
        let text_images = root.join("text_images");
        fs::create_dir_all(&layers).unwrap();
        fs::create_dir_all(&text_images).unwrap();

        // Reproduce the CORRUPTED committed state: the 8-page live layers.json.
        fs::copy(snap.join("layers/layers.json"), layers.join("layers.json")).unwrap();
        // The renamed _text.png exist for ALL pages 0-22 (per the incident) — synthesize them.
        let source_184: Value =
            serde_json::from_str(&fs::read_to_string(snap.join("layers/text_info.json.bak")).unwrap())
                .unwrap();
        for o in source_184.as_array().unwrap() {
            let uid = o["uid"].as_str().unwrap();
            let page = o.get("img_idx").and_then(Value::as_u64).unwrap_or(0) as usize;
            write_png(&layers.join(persist::text_image_file_name(page, uid)), 2, 2, [1, 2, 3, 255]);
        }

        // --- RECOVERY STEP 1: restore layers/text_info.json from the 184 .bak.
        fs::copy(snap.join("layers/text_info.json.bak"), layers.join(TEXT_INFO_FILE)).unwrap();

        // --- RECOVERY STEP 2: strip inline TEXT nodes from layers.json (keep rasters + groups), so the
        // manifest is no longer v3-inline and the migration gate allows the re-run.
        strip_inline_text_nodes(&layers.join("layers.json"));
        // Sanity: the 2 page-0 rasters survived the strip.
        let stripped =
            super::super::compat::read_manifest(&layers.join("layers.json")).unwrap().unwrap();
        let p0_rasters: Vec<&str> = stripped
            .page(0)
            .unwrap()
            .tree
            .iter()
            .filter(|r| matches!(r.kind, crate::models::layer_model::manifest::LayerKindRec::Raster))
            .map(|r| r.uid.as_str())
            .collect();
        assert_eq!(p0_rasters.len(), 2, "both page-0 rasters preserved through the strip");
        assert!(chapter_needs_migration(&layers, &text_images).is_some(), "strip re-enables migration");

        // --- RECOVERY STEP 3: run the fixed migration.
        let page_sizes: HashMap<usize, [usize; 2]> = (0..23).map(|p| (p, [1000, 1600])).collect();
        let report = migrate_chapter_to_v3(&layers, &text_images, None, &page_sizes).unwrap();
        eprintln!("RECOVERY: migrated={} renamed={} missing={}", report.migrated_overlays, report.renamed_pngs, report.missing_pngs);

        let (pages, texts) = manifest_pages_and_texts(&layers);
        assert_eq!(pages.len(), 23, "recovered all 23 pages");
        assert_eq!(texts, 184, "recovered all 184 text overlays");
        // The 2 page-0 rasters are still present.
        let m = super::super::compat::read_manifest(&layers.join("layers.json")).unwrap().unwrap();
        let p0r: Vec<&str> = m.page(0).unwrap().tree.iter()
            .filter(|r| matches!(r.kind, crate::models::layer_model::manifest::LayerKindRec::Raster))
            .map(|r| r.uid.as_str()).collect();
        assert!(p0r.contains(&"f12d9a16-3448-40a7-adb8-9a4ee0714b50"), "raster f12d9a16 intact");
        assert!(p0r.contains(&"ad5994dc-b132-4f3e-ae72-23d5cf99d349"), "raster ad5994dc intact");
        // Geometry is the COMPLETE source (no stale): the migration reused the existing _text.png (no missing).
        assert_eq!(report.missing_pngs, 0, "all _text.png reused (none missing)");

        let _ = fs::remove_dir_all(&root);
    }

    /// Test helper: removes all TEXT nodes from a `layers.json`, keeping raster/group/text-group data,
    /// and drops `schema_version` to 2 so the manifest is no longer v3-inline. (Mirrors recovery step 2.)
    fn strip_inline_text_nodes(layers_json: &Path) {
        let mut m: Value = serde_json::from_str(&fs::read_to_string(layers_json).unwrap()).unwrap();
        if let Some(pages) = m.get_mut("pages").and_then(Value::as_array_mut) {
            for p in pages.iter_mut() {
                if let Some(tree) = p.get_mut("tree").and_then(Value::as_array_mut) {
                    tree.retain(|r| r.get("kind").and_then(Value::as_str) != Some("text"));
                }
                // Drop text_groups too (they are rebuilt by the migration).
                if let Some(obj) = p.as_object_mut() {
                    obj.remove("text_groups");
                }
            }
        }
        // Force a non-v3 schema so the migration gate re-enables (compat re-stamps on read anyway).
        if let Some(obj) = m.as_object_mut() {
            obj.insert("schema_version".into(), Value::from(2u64));
        }
        fs::write(layers_json, serde_json::to_string_pretty(&m).unwrap()).unwrap();
    }

    #[test]
    fn migrates_multipage_chapter_renames_pngs_and_backs_up_text_info() {
        let dir = tmp("multipage");
        // page 0: 2 overlays; page 1: 1 overlay. PNGs live next to text_info.json (the layers dir).
        write_png(&dir.join("a.png"), 4, 3, [10, 20, 30, 255]);
        write_png(&dir.join("b.png"), 5, 2, [40, 50, 60, 255]);
        write_png(&dir.join("c.png"), 6, 4, [70, 80, 90, 255]);
        write_text_info(
            &dir,
            serde_json::json!([
                overlay("a", 0, "a.png", 100.0, 50.0),
                overlay("b", 0, "b.png", 200.0, 60.0),
                overlay("c", 1, "c.png", 300.0, 70.0),
            ]),
        );
        // Capture original bytes to prove preservation.
        let a_bytes = fs::read(dir.join("a.png")).unwrap();
        let c_bytes = fs::read(dir.join("c.png")).unwrap();

        let page_sizes: HashMap<usize, [usize; 2]> =
            [(0, [1000, 1000]), (1, [1000, 1000])].into_iter().collect();

        let report = migrate_chapter_to_v3(&dir, &dir, None, &page_sizes).unwrap();
        assert_eq!(report.migrated_overlays, 3);
        assert_eq!(report.renamed_pngs, 3);
        assert_eq!(report.missing_pngs, 0);

        // (a) layers.json has inline text nodes for ALL pages.
        for page in [0usize, 1] {
            let nodes = persist::load_page_text_nodes(&dir, None, page).unwrap();
            assert!(!nodes.is_empty(), "page {page} has text nodes");
            assert!(nodes.iter().all(|n| n.inline.is_some()), "page {page} fully inline");
        }

        // (b) old PNGs renamed to the v3 name with IDENTICAL bytes (pixels preserved, not re-encoded).
        let a_v3 = dir.join(persist::text_image_file_name(0, "a"));
        assert!(a_v3.is_file(), "a.png renamed to v3");
        assert!(!dir.join("a.png").is_file(), "old a.png moved away");
        assert_eq!(fs::read(&a_v3).unwrap(), a_bytes, "a pixels byte-identical");
        let c_v3 = dir.join(persist::text_image_file_name(1, "c"));
        assert_eq!(fs::read(&c_v3).unwrap(), c_bytes, "c pixels byte-identical");

        // (c) text_info.json moved to .bak with identical content.
        assert!(!dir.join(TEXT_INFO_FILE).is_file(), "text_info.json gone");
        let bak = dir.join(format!("{TEXT_INFO_FILE}.bak"));
        assert!(bak.is_file(), ".bak created");
        assert_eq!(report.backup_path.as_deref(), Some(bak.as_path()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ribbon_page_geometry_uses_full_page_sizes() {
        // A ribbon-format overlay (absolute x/y + region) on page 1 of a chapter with NON-uniform
        // aspects. The migration must pass the FULL page-size map so the ribbon scale is correct.
        let dir = tmp("ribbon");
        write_png(&dir.join("p0.png"), 4, 3, [1, 2, 3, 255]);
        write_png(&dir.join("p1.png"), 4, 3, [4, 5, 6, 255]);
        write_text_info(
            &dir,
            serde_json::json!([
                {"uid":"r0","page":"1_1","x":10.0,"y":10.0,"region_w":20.0,"region_h":4.0,
                 "overlay_type":"text","file":"p0.png","render_data":{"text":"r0"}},
                {"uid":"r1","page":"1_2","x":10.0,"y":150.0,"region_w":20.0,"region_h":4.0,
                 "overlay_type":"text","file":"p1.png","render_data":{"text":"r1"}},
            ]),
        );
        // Non-uniform aspects: page0 100x250, page1 100x300.
        let full: HashMap<usize, [usize; 2]> = [(0, [100, 250]), (1, [100, 300])].into_iter().collect();

        // Reference geometry via the SAME shared codec + full map (what the doc would compute).
        let raw = text_payload::read_overlay_entries(&[dir.as_path()]);
        let migrated = text_payload::migrate_overlay_entries(&raw, &full, |o| {
            o.get("file")
                .and_then(Value::as_str)
                .and_then(|f| image::image_dimensions(dir.join(f)).ok())
                .map_or((0.0, 0.0), |(w, h)| (w as f32, h as f32))
        });
        let ref_obj = migrated
            .iter()
            .find_map(|e| e.as_object().filter(|o| o.get("uid").and_then(Value::as_str) == Some("r1")))
            .unwrap()
            .clone();
        let ref_cy = text_payload::decode_overlay_placement(&ref_obj, [100, 300]).transform.cy;

        migrate_chapter_to_v3(&dir, &dir, None, &full).unwrap();

        // Reload the migrated page-1 node and compare its inline transform cy to the reference.
        let nodes = persist::load_page_text_nodes(&dir, None, 1).unwrap();
        let n = nodes.iter().find(|n| n.uid == "r1").expect("r1 migrated");
        let cy = n.inline.as_ref().unwrap().transform.unwrap().cy;
        assert!((cy - ref_cy).abs() < 1e-3, "ribbon geometry uses full page sizes: {cy} vs {ref_cy}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn re_running_migration_is_a_noop() {
        let dir = tmp("idempotent");
        write_png(&dir.join("a.png"), 4, 3, [10, 20, 30, 255]);
        write_text_info(&dir, serde_json::json!([overlay("a", 0, "a.png", 100.0, 50.0)]));
        let page_sizes: HashMap<usize, [usize; 2]> = [(0, [1000, 1000])].into_iter().collect();

        let first = migrate_chapter_to_v3(&dir, &dir, None, &page_sizes).unwrap();
        assert_eq!(first.migrated_overlays, 1);
        assert!(first.backup_path.is_some());

        // Detection now returns None (text_info.json is gone, layers.json is v3-inline).
        assert!(chapter_needs_migration(&dir, &dir).is_none(), "no longer needs migration");

        // A second run touches nothing: no second .bak, an empty report.
        let second = migrate_chapter_to_v3(&dir, &dir, None, &page_sizes).unwrap();
        assert_eq!(second, MigrationReport::default(), "second run is a clean no-op");
        assert!(!dir.join(format!("{TEXT_INFO_FILE}.bak.1")).exists(), "no second .bak");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn v3_inline_manifest_blocks_re_migration_even_with_lingering_text_info() {
        // SAFETY GATE (ВВД/13 fix): once `layers.json` carries inline (v3) text, the chapter is
        // considered migrated and is NEVER re-migrated — even if a `text_info.json` still lingers in
        // either dir (a crash before the `.bak` rename, OR a STALE secondary in `text_images/`). This is
        // what stops a stale partial `text_info.json` from overwriting the good v3 data.
        let dir = tmp("v3_gate");
        write_png(&dir.join(persist::text_image_file_name(0, "a")), 4, 3, [10, 20, 30, 255]);
        // layers.json already has the inline node (the good migrated data, cx=100).
        persist::write_page_text_payload(
            &dir,
            None,
            0,
            &[persist::TextPayloadOut {
                uid: "a".into(),
                name: "Текст 1".into(),
                z: 0,
                layer_idx: 0,
                pinned: false,
                visible: true,
                opacity: 1.0,
                group_uid: None,
                pinned_by_group: false,
                payload_uid: "a".into(),
                render_data: serde_json::json!({"text": "a"}),
                transform: TransformRec { cx: 100.0, cy: 50.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                rendered_file: Some(persist::text_image_file_name(0, "a")),
                mask_clip: Some(true),
            }],
        )
        .unwrap();
        // A STALE/lingering text_info.json with DIFFERENT geometry (cx=1) is present.
        write_text_info(&dir, serde_json::json!([overlay("a", 0, "a.png", 1.0, 1.0)]));
        let page_sizes: HashMap<usize, [usize; 2]> = [(0, [1000, 1000])].into_iter().collect();

        assert!(
            chapter_needs_migration(&dir, &dir).is_none(),
            "a v3-inline manifest is already migrated — never re-run"
        );
        let report = migrate_chapter_to_v3(&dir, &dir, None, &page_sizes).unwrap();
        assert_eq!(report, MigrationReport::default(), "clean no-op (no regression)");

        // The good inline geometry (cx=100) survived — NOT regressed to the stale text_info.json (cx=1).
        let nodes = persist::load_page_text_nodes(&dir, None, 0).unwrap();
        assert_eq!(nodes.len(), 1, "no duplicate");
        let cx = nodes[0].inline.as_ref().unwrap().transform.unwrap().cx;
        assert!((cx - 100.0).abs() < 1e-3, "kept the good v3 geometry, did NOT regress to stale");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn modern_chapter_without_text_info_is_a_noop() {
        // A chapter that has no text_info.json (already v3 / no text) must not be touched.
        let dir = tmp("modern");
        // Write a v3-style layers.json with an inline text node and NO text_info.json.
        let img = ColorImage::filled([2, 2], eframe::egui::Color32::WHITE);
        persist::write_text_image(&dir, 0, "t", &img).unwrap();
        persist::write_page_text_payload(
            &dir,
            None,
            0,
            &[persist::TextPayloadOut {
                uid: "t".into(),
                name: "T".into(),
                z: 0,
                layer_idx: 0,
                pinned: false,
                visible: true,
                opacity: 1.0,
                group_uid: None,
                pinned_by_group: false,
                payload_uid: "t".into(),
                render_data: serde_json::json!({"text": "t"}),
                transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                rendered_file: Some(persist::text_image_file_name(0, "t")),
                mask_clip: None,
            }],
        )
        .unwrap();

        assert!(chapter_needs_migration(&dir, &dir).is_none(), "modern chapter: no migration");
        let report = migrate_chapter_to_v3(&dir, &dir, None, &HashMap::new()).unwrap();
        assert_eq!(report, MigrationReport::default());
        assert!(!dir.join(format!("{TEXT_INFO_FILE}.bak")).exists(), "no spurious .bak");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_png_falls_back_without_dropping_overlay() {
        let dir = tmp("missing_png");
        // Overlay "a" has a PNG; overlay "b" references a file that does not exist.
        write_png(&dir.join("a.png"), 4, 3, [10, 20, 30, 255]);
        write_text_info(
            &dir,
            serde_json::json!([
                overlay("a", 0, "a.png", 100.0, 50.0),
                overlay("b", 0, "missing.png", 200.0, 60.0),
            ]),
        );
        let page_sizes: HashMap<usize, [usize; 2]> = [(0, [1000, 1000])].into_iter().collect();

        let report = migrate_chapter_to_v3(&dir, &dir, None, &page_sizes).unwrap();
        assert_eq!(report.migrated_overlays, 2, "both overlays migrated (none dropped)");
        assert_eq!(report.renamed_pngs, 1);
        assert_eq!(report.missing_pngs, 1);

        // Both nodes exist inline; "b" has no rendered_file but keeps its render_data/geometry.
        let nodes = persist::load_page_text_nodes(&dir, None, 0).unwrap();
        assert_eq!(nodes.len(), 2, "no overlay dropped");
        let b = nodes.iter().find(|n| n.uid == "b").expect("b kept");
        let inline = b.inline.as_ref().expect("b has inline payload");
        assert!(inline.rendered_file.is_none(), "b has no rendered PNG");
        assert_eq!(inline.render_data["text"], "b", "b render_data preserved");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn does_not_clobber_an_existing_backup() {
        let dir = tmp("existing_bak");
        write_png(&dir.join("a.png"), 4, 3, [10, 20, 30, 255]);
        write_text_info(&dir, serde_json::json!([overlay("a", 0, "a.png", 100.0, 50.0)]));
        // A pre-existing .bak from an older rollback.
        fs::write(dir.join(format!("{TEXT_INFO_FILE}.bak")), "OLD BACKUP").unwrap();
        let page_sizes: HashMap<usize, [usize; 2]> = [(0, [1000, 1000])].into_iter().collect();

        let report = migrate_chapter_to_v3(&dir, &dir, None, &page_sizes).unwrap();
        // The old .bak is untouched; the new one is .bak.1.
        assert_eq!(fs::read_to_string(dir.join(format!("{TEXT_INFO_FILE}.bak"))).unwrap(), "OLD BACKUP");
        assert_eq!(report.backup_path.as_deref(), Some(dir.join(format!("{TEXT_INFO_FILE}.bak.1")).as_path()));
        assert!(dir.join(format!("{TEXT_INFO_FILE}.bak.1")).is_file());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_text_images_location_is_migrated_into_layers_dir() {
        // text_info.json + PNG live in the legacy text_images/ dir; migration writes layers.json + the
        // renamed PNG into the canonical layers/ dir.
        let root = tmp("legacy_loc");
        let layers_dir = root.join("layers");
        let text_images_dir = root.join("text_images");
        fs::create_dir_all(&layers_dir).unwrap();
        fs::create_dir_all(&text_images_dir).unwrap();
        write_png(&text_images_dir.join("a.png"), 4, 3, [10, 20, 30, 255]);
        write_text_info(&text_images_dir, serde_json::json!([overlay("a", 0, "a.png", 100.0, 50.0)]));
        let page_sizes: HashMap<usize, [usize; 2]> = [(0, [1000, 1000])].into_iter().collect();

        let report = migrate_chapter_to_v3(&layers_dir, &text_images_dir, None, &page_sizes).unwrap();
        assert_eq!(report.migrated_overlays, 1);
        // Inline node + renamed PNG land in the canonical layers/ dir.
        let nodes = persist::load_page_text_nodes(&layers_dir, None, 0).unwrap();
        assert!(nodes[0].inline.is_some());
        assert!(layers_dir.join(persist::text_image_file_name(0, "a")).is_file());
        // The legacy text_info.json (in text_images/) is the one backed up.
        assert!(text_images_dir.join(format!("{TEXT_INFO_FILE}.bak")).is_file());

        let _ = fs::remove_dir_all(&root);
    }

    /// Writes a v3 inline text node into `dir`'s page (test setup for the unsaved staging page).
    #[allow(clippy::too_many_arguments)]
    fn seed_inline_text(
        dir: &Path,
        page: usize,
        uid: &str,
        cx: f32,
        cy: f32,
        existing: &[persist::TextPayloadOut],
    ) {
        // Build the page = existing nodes + the new one, written in one call (full replace).
        let mut outs: Vec<persist::TextPayloadOut> = existing.to_vec();
        // Give the node a real rendered PNG so the loader keeps it.
        let img = ColorImage::filled([2, 2], eframe::egui::Color32::WHITE);
        persist::write_text_image(dir, page, uid, &img).unwrap();
        outs.push(persist::TextPayloadOut {
            uid: uid.into(),
            name: uid.into(),
            z: 0,
            layer_idx: 0,
            pinned: false,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            pinned_by_group: false,
            payload_uid: uid.into(),
            render_data: serde_json::json!({ "text": uid, "staged": true }),
            transform: TransformRec { cx, cy, rotation: 0.0, scale: 1.0 },
            deform: None,
            rendered_file: Some(persist::text_image_file_name(page, uid)),
            mask_clip: None,
        });
        persist::write_page_text_payload(dir, None, page, &outs).unwrap();
    }

    #[test]
    fn partial_inline_manifest_blocks_eager_re_migration_without_dropping_inline_data() {
        // A crash mid-migration leaves SOME pages inline (page 0) and some not (page 1), with
        // text_info.json still present. The SAFETY GATE (v3-inline ⇒ migrated) blocks eager re-migration
        // — page 0's good inline data is NEVER overwritten/dropped. Page 1's overlay is NOT eagerly
        // inlined here, but its data is NOT lost: it still loads via the lazy on-read path from the
        // preserved text_info.json (covered by the doc loader's per-page legacy fallback). This trades
        // "eagerly finishing a partial migration" for the absolute guarantee that no good inline data is
        // ever clobbered — the ВВД/13 priority.
        let dir = tmp("partial_inline");
        let a_v3 = dir.join(persist::text_image_file_name(0, "a"));
        write_png(&a_v3, 4, 3, [10, 20, 30, 255]);
        let a_bytes = fs::read(&a_v3).unwrap();
        // Page 0 already inline (good data, cx=100).
        persist::write_page_text_payload(
            &dir,
            None,
            0,
            &[persist::TextPayloadOut {
                uid: "a".into(),
                name: "Текст 1".into(),
                z: 0,
                layer_idx: 0,
                pinned: false,
                visible: true,
                opacity: 1.0,
                group_uid: None,
                pinned_by_group: false,
                payload_uid: "a".into(),
                render_data: serde_json::json!({ "text": "a" }),
                transform: TransformRec { cx: 100.0, cy: 50.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                rendered_file: Some(persist::text_image_file_name(0, "a")),
                mask_clip: Some(true),
            }],
        )
        .unwrap();
        write_png(&dir.join("b.png"), 5, 2, [40, 50, 60, 255]);
        write_text_info(
            &dir,
            serde_json::json!([
                overlay("a", 0, "a.png", 999.0, 999.0), // STALE legacy geometry for the already-inline a
                overlay("b", 1, "b.png", 300.0, 70.0),
            ]),
        );
        let page_sizes: HashMap<usize, [usize; 2]> =
            [(0, [1000, 1000]), (1, [1000, 1000])].into_iter().collect();

        // The gate blocks re-migration (manifest is v3-inline).
        assert!(chapter_needs_migration(&dir, &dir).is_none());
        let report = migrate_chapter_to_v3(&dir, &dir, None, &page_sizes).unwrap();
        assert_eq!(report, MigrationReport::default(), "no-op: inline data not touched");

        // Page 0's good inline data (cx=100) and renamed PNG are intact (NOT regressed to the stale 999).
        let p0 = persist::load_page_text_nodes(&dir, None, 0).unwrap();
        assert_eq!(p0.len(), 1);
        let cx = p0[0].inline.as_ref().unwrap().transform.unwrap().cx;
        assert!((cx - 100.0).abs() < 1e-3, "page 0 kept its good geometry");
        assert_eq!(fs::read(&a_v3).unwrap(), a_bytes, "page-0 pixels untouched");
        // text_info.json is PRESERVED (not `.bak`'d) so page 1 still loads via the lazy path.
        assert!(dir.join(TEXT_INFO_FILE).is_file(), "legacy file kept for the not-yet-inlined page");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsaved_mirror_is_additive_keeping_staged_only_overlay() {
        // An overlay "s" is staged ONLY in `_unsaved` (created, never saved-to-project → NOT in
        // text_info.json). Migrating the committed text_info.json must NOT drop "s" from unsaved, and
        // must ADD the text_info.json overlay "a" to unsaved too.
        let root = tmp("additive_staged");
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");
        fs::create_dir_all(&committed).unwrap();
        fs::create_dir_all(&unsaved).unwrap();

        // Committed legacy chapter: text_info.json + PNG for overlay "a".
        write_png(&committed.join("a.png"), 4, 3, [10, 20, 30, 255]);
        write_text_info(&committed, serde_json::json!([overlay("a", 0, "a.png", 100.0, 50.0)]));
        // Unsaved staging page 0 already exists with a staged-only inline overlay "s".
        seed_inline_text(&unsaved, 0, "s", 11.0, 22.0, &[]);

        let page_sizes: HashMap<usize, [usize; 2]> = [(0, [1000, 1000])].into_iter().collect();
        migrate_chapter_to_v3(&committed, &committed, Some(&unsaved), &page_sizes).unwrap();

        // Unsaved page 0 now has BOTH the staged-only "s" (survived) and the migrated "a" (added).
        let unsaved_nodes = persist::load_page_text_nodes(&unsaved, None, 0).unwrap();
        let uids: std::collections::HashSet<&str> =
            unsaved_nodes.iter().map(|n| n.uid.as_str()).collect();
        assert!(uids.contains("s"), "staged-only overlay survived the additive mirror");
        assert!(uids.contains("a"), "migrated overlay was added to unsaved");
        // "s" kept its staged geometry/payload.
        let s = unsaved_nodes.iter().find(|n| n.uid == "s").unwrap();
        let s_inline = s.inline.as_ref().unwrap();
        assert!((s_inline.transform.unwrap().cx - 11.0).abs() < 1e-4);
        assert_eq!(s_inline.render_data["staged"], true, "staged payload preserved");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unsaved_mirror_does_not_clobber_a_fresher_edit() {
        // The unsaved page has a FRESH inline edit for uid "x" (cx = 999); text_info.json has a STALE
        // "x" (cx = 100). The additive mirror must KEEP the fresh unsaved geometry, not overwrite it.
        let root = tmp("additive_fresh");
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");
        fs::create_dir_all(&committed).unwrap();
        fs::create_dir_all(&unsaved).unwrap();

        write_png(&committed.join("x.png"), 4, 3, [10, 20, 30, 255]);
        write_text_info(&committed, serde_json::json!([overlay("x", 0, "x.png", 100.0, 50.0)]));
        // Unsaved already has a fresher inline edit for "x".
        seed_inline_text(&unsaved, 0, "x", 999.0, 888.0, &[]);

        let page_sizes: HashMap<usize, [usize; 2]> = [(0, [1000, 1000])].into_iter().collect();
        migrate_chapter_to_v3(&committed, &committed, Some(&unsaved), &page_sizes).unwrap();

        let unsaved_nodes = persist::load_page_text_nodes(&unsaved, None, 0).unwrap();
        assert_eq!(unsaved_nodes.len(), 1, "no duplicate for x");
        let x = unsaved_nodes.iter().find(|n| n.uid == "x").unwrap();
        let x_inline = x.inline.as_ref().unwrap();
        assert!(
            (x_inline.transform.unwrap().cx - 999.0).abs() < 1e-4,
            "unsaved kept the FRESH geometry, not the stale text_info.json one"
        );
        // The committed dir got the (migrated) stale geometry — that's the legacy source, correct.
        let committed_nodes = persist::load_page_text_nodes(&committed, None, 0).unwrap();
        let cx_committed = committed_nodes
            .iter()
            .find(|n| n.uid == "x")
            .unwrap()
            .inline
            .as_ref()
            .unwrap()
            .transform
            .unwrap()
            .cx;
        assert!((cx_committed - 100.0).abs() < 1e-4, "committed reflects the migrated legacy geometry");

        let _ = fs::remove_dir_all(&root);
    }
}
