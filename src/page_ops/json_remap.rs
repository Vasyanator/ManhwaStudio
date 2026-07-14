/*
File: page_ops/json_remap.rs

Purpose:
Pure JSON rewrites for the page-keyed documents a structural page operation
must keep consistent: `translation_bubbles.json` (bubble `img_idx` +
ImageBubble `crop_page_idx`/`crop_rect`), `layers/layers.json` (page `img_idx`
plus the `ps_p{page:04}_...` file references inside layer records),
`text_info.json` (legacy typing overlay `img_idx`) and text-detection
`{idx:05}_blocks.json` (`mask_file` reference).

Key functions:
- remap_bubbles(): bubbles kept/remapped/deleted per the permutation.
- remap_text_info(): typing overlay entries kept/remapped/deleted.
- remap_layers_manifest(): layer-manifest pages remapped, deleted pages split off.
- remap_detection_blocks(): `mask_file` default-name rewrite for one page.
- remap_layers_png_name(): `ps_p{old:04}_` -> `ps_p{new:04}_` file-name rewrite.

Notes:
Everything operates on `serde_json::Value` so unknown/extra fields survive the
rewrite byte-for-byte at the value level (object key ORDER may change, matching
how the app itself re-serializes these documents). No filesystem access.
*/

use super::PageOpError;
use super::plan::layers_png_prefix;
use serde_json::{Map, Value};

/// Result of remapping the bubbles array.
#[derive(Debug)]
pub(crate) struct BubblesRemap {
    /// Surviving bubbles with `img_idx` (and crop fields) remapped.
    pub kept: Vec<Value>,
    /// Bubbles of deleted pages, verbatim (archived in the trash).
    pub deleted: Vec<Value>,
    /// True when `kept` differs from the input or anything was deleted.
    pub changed: bool,
    pub warnings: Vec<String>,
}

/// Remaps every bubble's page association per `old_to_new`.
///
/// Rules:
/// - `img_idx` is rewritten to the page's new index; bubbles whose page was
///   deleted move to `deleted` (the caller archives them as
///   `deleted_bubbles.json` in the trash).
/// - a `crop_page_idx` (page-crop ImageBubble) is rewritten the same way; when
///   the crop TARGET page was deleted, `crop_page_idx` and `crop_rect` are
///   removed so the bubble degrades to a plain external-image bubble instead
///   of cropping a wrong page.
/// - an `img_idx`/`crop_page_idx` beyond the current page count is left
///   untouched with a warning (already-dangling data is not made worse).
///
/// # Errors
/// [`PageOpError::InvalidOp`] when an entry is not an object, still uses the
/// legacy absolute-coordinate format (no `img_u`, numeric `x`/`y` — those are
/// keyed by ribbon position, not page, and must be migrated by a normal
/// project load first), or has no `img_idx` at all.
pub(crate) fn remap_bubbles(
    entries: &[Value],
    old_to_new: &[Option<usize>],
) -> Result<BubblesRemap, PageOpError> {
    let mut kept = Vec::with_capacity(entries.len());
    let mut deleted = Vec::new();
    let mut changed = false;
    let mut warnings = Vec::new();

    for (pos, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            return Err(PageOpError::InvalidOp(format!(
                "bubble entry #{pos} is not a JSON object"
            )));
        };
        if obj.get("img_u").is_none()
            && obj.get("x").and_then(Value::as_f64).is_some()
            && obj.get("y").and_then(Value::as_f64).is_some()
        {
            return Err(PageOpError::InvalidOp(format!(
                "bubble entry #{pos} uses the legacy absolute-coordinate format; \
                 open the chapter once so the load migration rewrites it before \
                 running page operations"
            )));
        }
        let Some(old_idx) = obj.get("img_idx").and_then(Value::as_u64) else {
            return Err(PageOpError::InvalidOp(format!(
                "bubble entry #{pos} has no numeric img_idx"
            )));
        };
        let Ok(old_idx) = usize::try_from(old_idx) else {
            return Err(PageOpError::InvalidOp(format!(
                "bubble entry #{pos} img_idx {old_idx} does not fit usize"
            )));
        };
        if old_idx >= old_to_new.len() {
            warnings.push(format!(
                "bubble entry #{pos} references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            ));
            kept.push(entry.clone());
            continue;
        }
        match old_to_new[old_idx] {
            None => {
                deleted.push(entry.clone());
                changed = true;
            }
            Some(new_idx) => {
                let mut new_obj = obj.clone();
                if new_idx != old_idx {
                    new_obj.insert("img_idx".to_string(), Value::from(new_idx));
                    changed = true;
                }
                let (crop_changed, crop_warning) =
                    remap_crop_fields(&mut new_obj, old_to_new, pos);
                changed |= crop_changed;
                if let Some(warning) = crop_warning {
                    warnings.push(warning);
                }
                kept.push(Value::Object(new_obj));
            }
        }
    }

    Ok(BubblesRemap {
        kept,
        deleted,
        changed,
        warnings,
    })
}

/// Rewrites `crop_page_idx` in one bubble object; removes `crop_page_idx` +
/// `crop_rect` when the crop target page was deleted. Returns
/// `(changed, warning)`.
fn remap_crop_fields(
    obj: &mut Map<String, Value>,
    old_to_new: &[Option<usize>],
    entry_pos: usize,
) -> (bool, Option<String>) {
    let Some(crop_idx) = obj.get("crop_page_idx").and_then(Value::as_u64) else {
        return (false, None);
    };
    let Ok(crop_idx) = usize::try_from(crop_idx) else {
        return (
            false,
            Some(format!(
                "bubble entry #{entry_pos} crop_page_idx {crop_idx} does not fit usize; \
                 left untouched"
            )),
        );
    };
    if crop_idx >= old_to_new.len() {
        return (
            false,
            Some(format!(
                "bubble entry #{entry_pos} crop_page_idx {crop_idx} is beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            )),
        );
    }
    match old_to_new[crop_idx] {
        Some(new_idx) => {
            if new_idx != crop_idx {
                obj.insert("crop_page_idx".to_string(), Value::from(new_idx));
                return (true, None);
            }
            (false, None)
        }
        None => {
            // The page this bubble cropped from is gone: drop the crop link so
            // the bubble cannot show a crop of an unrelated page.
            obj.remove("crop_page_idx");
            obj.remove("crop_rect");
            (true, None)
        }
    }
}

/// Result of remapping one `text_info.json` array.
#[derive(Debug)]
pub(crate) struct TextInfoRemap {
    pub kept: Vec<Value>,
    /// Entries of deleted pages, verbatim (archived in the trash).
    pub deleted: Vec<Value>,
    /// `file` names referenced by the deleted entries (overlay PNGs the caller
    /// moves to the trash).
    pub deleted_files: Vec<String>,
    pub changed: bool,
    pub warnings: Vec<String>,
}

/// Remaps legacy typing-overlay entries (`text_info.json`) per `old_to_new`.
///
/// Only `img_idx` is rewritten: the overlay PNG names (`file`) are NOT
/// page-keyed for loading (their `p{page:04}` token is a creation-time
/// uniqueness hint and the stable overlay uid is derived from the file name),
/// so renaming them would break `layers.json` references.
///
/// An entry with no `img_idx` and no legacy `x`/`y` is treated as page 0,
/// mirroring the typing loader (`helpers.rs` defaults a missing `img_idx`
/// to 0), and gets an explicit remapped `img_idx`.
///
/// # Errors
/// [`PageOpError::InvalidOp`] for the legacy absolute-coordinate placement
/// family (numeric `x`/`y`, no `img_idx`/`u`/`v`): those entries are keyed by
/// continuous-ribbon position — which any page operation changes — and must be
/// migrated by opening the typing tab first.
pub(crate) fn remap_text_info(
    entries: &[Value],
    old_to_new: &[Option<usize>],
) -> Result<TextInfoRemap, PageOpError> {
    let mut kept = Vec::with_capacity(entries.len());
    let mut deleted = Vec::new();
    let mut deleted_files = Vec::new();
    let mut changed = false;
    let mut warnings = Vec::new();

    for (pos, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            return Err(PageOpError::InvalidOp(format!(
                "text_info entry #{pos} is not a JSON object"
            )));
        };
        let has_img_idx = obj.get("img_idx").and_then(Value::as_u64).is_some();
        if !has_img_idx
            && obj.get("img_u").is_none()
            && obj.get("u").is_none()
            && obj.get("x").and_then(Value::as_f64).is_some()
            && obj.get("y").and_then(Value::as_f64).is_some()
        {
            return Err(PageOpError::InvalidOp(format!(
                "text_info entry #{pos} uses the legacy absolute-coordinate placement; \
                 open the chapter (typing tab) once so the load migration rewrites it \
                 before running page operations"
            )));
        }
        // Mirror of the typing loader: a missing img_idx reads as page 0.
        let old_idx = obj
            .get("img_idx")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        if !has_img_idx {
            warnings.push(format!(
                "text_info entry #{pos} has no img_idx; treated as page 0 \
                 (matching the typing loader)"
            ));
        }
        if old_idx >= old_to_new.len() {
            warnings.push(format!(
                "text_info entry #{pos} references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            ));
            kept.push(entry.clone());
            continue;
        }
        match old_to_new[old_idx] {
            None => {
                if let Some(file) = obj.get("file").and_then(Value::as_str) {
                    let trimmed = file.trim();
                    if !trimmed.is_empty() {
                        deleted_files.push(trimmed.to_string());
                    }
                }
                deleted.push(entry.clone());
                changed = true;
            }
            Some(new_idx) => {
                if new_idx != old_idx || !has_img_idx {
                    let mut new_obj = obj.clone();
                    new_obj.insert("img_idx".to_string(), Value::from(new_idx));
                    kept.push(Value::Object(new_obj));
                    changed = true;
                } else {
                    kept.push(entry.clone());
                }
            }
        }
    }

    Ok(TextInfoRemap {
        kept,
        deleted,
        deleted_files,
        changed,
        warnings,
    })
}

/// Result of remapping `layers.json`.
#[derive(Debug)]
pub(crate) struct LayersRemap {
    /// The full manifest with surviving pages remapped and re-sorted.
    pub manifest: Value,
    /// Page entries of deleted pages, verbatim (archived in the trash).
    pub deleted_pages: Vec<Value>,
    pub changed: bool,
    pub warnings: Vec<String>,
}

/// Remaps the layer manifest (`layers/layers.json`) per `old_to_new`.
///
/// For every surviving page entry: `img_idx` is rewritten and every
/// `base_file` / `rendered_file` in its `tree` gets its `ps_p{page:04}_`
/// prefix rewritten to the file's new page index (the prefix is load-bearing:
/// `persist.rs::prune_orphan_pngs` prunes by it, so a stale prefix would let a
/// later save of the page now holding the OLD index delete the moved page's
/// PNGs). Page entries kept sorted by `img_idx` (a `LayersManifest` invariant,
/// see `manifest.rs::upsert_page`). Entries of deleted pages are split off.
///
/// # Errors
/// [`PageOpError::Json`] when the manifest root is not an object.
pub(crate) fn remap_layers_manifest(
    manifest: &Value,
    old_to_new: &[Option<usize>],
) -> Result<LayersRemap, PageOpError> {
    let Some(root) = manifest.as_object() else {
        return Err(PageOpError::Json(
            "layers.json root is not a JSON object".to_string(),
        ));
    };
    let mut warnings = Vec::new();
    let mut deleted_pages = Vec::new();
    let mut changed = false;

    let Some(pages) = root.get("pages").and_then(Value::as_array) else {
        // No pages array: nothing page-keyed to rewrite.
        return Ok(LayersRemap {
            manifest: manifest.clone(),
            deleted_pages,
            changed: false,
            warnings,
        });
    };

    let mut kept: Vec<Value> = Vec::with_capacity(pages.len());
    for (pos, page) in pages.iter().enumerate() {
        let Some(page_obj) = page.as_object() else {
            return Err(PageOpError::Json(format!(
                "layers.json pages[{pos}] is not a JSON object"
            )));
        };
        let Some(old_idx) = page_obj
            .get("img_idx")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
        else {
            return Err(PageOpError::Json(format!(
                "layers.json pages[{pos}] has no numeric img_idx"
            )));
        };
        if old_idx >= old_to_new.len() {
            warnings.push(format!(
                "layers.json pages[{pos}] references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            ));
            kept.push(page.clone());
            continue;
        }
        match old_to_new[old_idx] {
            None => {
                deleted_pages.push(page.clone());
                changed = true;
            }
            Some(new_idx) => {
                let mut new_page = page_obj.clone();
                new_page.insert("img_idx".to_string(), Value::from(new_idx));
                // File references are remapped even on a page that keeps its
                // index: each name's EMBEDDED index is remapped independently,
                // so a cross-page PNG reference stays aligned with the
                // file-rename pass.
                if let Some(tree) = new_page.get_mut("tree").and_then(Value::as_array_mut) {
                    for rec in tree.iter_mut() {
                        if let Some(rec_obj) = rec.as_object_mut() {
                            remap_layer_rec_files(rec_obj, old_to_new);
                        }
                    }
                }
                if new_page == *page_obj {
                    kept.push(page.clone());
                } else {
                    kept.push(Value::Object(new_page));
                    changed = true;
                }
            }
        }
    }

    // Preserve the manifest's sorted-by-img_idx invariant after remapping.
    kept.sort_by_key(|page| {
        page.as_object()
            .and_then(|o| o.get("img_idx"))
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX)
    });

    let mut new_root = root.clone();
    new_root.insert("pages".to_string(), Value::Array(kept));
    Ok(LayersRemap {
        manifest: Value::Object(new_root),
        deleted_pages,
        changed,
        warnings,
    })
}

/// Rewrites the `ps_p{page:04}_` prefix of `base_file` / `rendered_file` in
/// one layer record. The embedded index of EACH FILE NAME is remapped
/// independently (rather than assuming the page's own index) so a record
/// referencing a PNG with a different page prefix stays consistent with the
/// file-rename pass, which is also keyed by the name's embedded index.
fn remap_layer_rec_files(rec: &mut Map<String, Value>, old_to_new: &[Option<usize>]) {
    for key in ["base_file", "rendered_file"] {
        let Some(name) = rec.get(key).and_then(Value::as_str) else {
            continue;
        };
        let Some(file_idx) = super::plan::parse_layers_png_page_idx(name) else {
            continue;
        };
        if file_idx >= old_to_new.len() {
            continue;
        }
        if let Some(new_idx) = old_to_new[file_idx]
            && let Some(new_name) = remap_layers_png_name(name, file_idx, new_idx)
        {
            rec.insert(key.to_string(), Value::String(new_name));
        }
    }
}

/// Rewrites a layer PNG name from the `old_idx` prefix to the `new_idx`
/// prefix; returns `None` when the name does not carry the `old_idx` prefix
/// or already has the target name.
#[must_use]
pub(crate) fn remap_layers_png_name(name: &str, old_idx: usize, new_idx: usize) -> Option<String> {
    if old_idx == new_idx {
        return None;
    }
    let rest = name.strip_prefix(&layers_png_prefix(old_idx))?;
    Some(format!("{}{rest}", layers_png_prefix(new_idx)))
}

/// Rewrites the `mask_file` reference inside a text-detection blocks document
/// when it points at the page's DEFAULT mask name (`{old:05}_mask.png`), which
/// the transaction renames. A custom `mask_file` value is left untouched (the
/// engine does not rename such files). Returns the (possibly new) document and
/// whether it changed.
#[must_use]
pub(crate) fn remap_detection_blocks(
    blocks: &Value,
    old_idx: usize,
    new_idx: usize,
) -> (Value, bool) {
    let Some(obj) = blocks.as_object() else {
        return (blocks.clone(), false);
    };
    let old_default = super::plan::detection_mask_file_name(old_idx);
    let matches_default = obj
        .get("mask_file")
        .and_then(Value::as_str)
        .is_some_and(|name| name.trim() == old_default);
    if !matches_default || old_idx == new_idx {
        return (blocks.clone(), false);
    }
    let mut new_obj = obj.clone();
    new_obj.insert(
        "mask_file".to_string(),
        Value::String(super::plan::detection_mask_file_name(new_idx)),
    );
    (Value::Object(new_obj), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Move page 0 to the end of 4 pages: 0->3, 1->0, 2->1, 3->2.
    fn move_map() -> Vec<Option<usize>> {
        vec![Some(3), Some(0), Some(1), Some(2)]
    }

    /// Delete page 1 of 3: 0->0, 1->gone, 2->1.
    fn delete_map() -> Vec<Option<usize>> {
        vec![Some(0), None, Some(1)]
    }

    #[test]
    fn bubbles_img_idx_and_crop_are_remapped() {
        let entries = vec![
            json!({"id": 1, "img_idx": 0, "img_u": 0.5, "img_v": 0.5, "text": "a",
                   "custom_field": "kept"}),
            json!({"id": 2, "img_idx": 3, "img_u": 0.1, "img_v": 0.2,
                   "crop_page_idx": 1, "crop_rect": [0, 0, 10, 10],
                   "image_source_type": "page_crop"}),
        ];
        let out = remap_bubbles(&entries, &move_map()).expect("remaps");
        assert!(out.changed);
        assert!(out.deleted.is_empty());
        assert_eq!(out.kept[0]["img_idx"], json!(3));
        // Unknown fields survive.
        assert_eq!(out.kept[0]["custom_field"], json!("kept"));
        assert_eq!(out.kept[1]["img_idx"], json!(2));
        // crop target page 1 moved to index 0.
        assert_eq!(out.kept[1]["crop_page_idx"], json!(0));
        assert_eq!(out.kept[1]["crop_rect"], json!([0, 0, 10, 10]));
    }

    #[test]
    fn bubbles_of_deleted_pages_are_split_off_and_crop_links_dropped() {
        let entries = vec![
            json!({"id": 1, "img_idx": 1, "img_u": 0.5, "img_v": 0.5}),
            json!({"id": 2, "img_idx": 2, "img_u": 0.5, "img_v": 0.5,
                   "crop_page_idx": 1, "crop_rect": [1, 2, 3, 4]}),
        ];
        let out = remap_bubbles(&entries, &delete_map()).expect("remaps");
        assert!(out.changed);
        // Bubble on the deleted page is archived verbatim.
        assert_eq!(out.deleted.len(), 1);
        assert_eq!(out.deleted[0]["id"], json!(1));
        // Survivor: img_idx 2 -> 1, crop link to the deleted page removed.
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0]["img_idx"], json!(1));
        assert!(out.kept[0].get("crop_page_idx").is_none());
        assert!(out.kept[0].get("crop_rect").is_none());
    }

    #[test]
    fn bubbles_reject_legacy_absolute_coordinates() {
        let entries = vec![json!({"id": 1, "x": 10.0, "y": 20.0, "text": "legacy"})];
        assert!(matches!(
            remap_bubbles(&entries, &move_map()),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn bubbles_out_of_range_img_idx_is_left_with_warning() {
        let entries = vec![json!({"id": 1, "img_idx": 99, "img_u": 0.5, "img_v": 0.5})];
        let out = remap_bubbles(&entries, &move_map()).expect("remaps");
        assert!(!out.changed);
        assert_eq!(out.kept[0]["img_idx"], json!(99));
        assert_eq!(out.warnings.len(), 1);
    }

    #[test]
    fn text_info_entries_are_remapped_and_deleted_files_collected() {
        let entries = vec![
            json!({"img_idx": 1, "file": "typing_overlay_p0001_1.png", "u": 0.5}),
            json!({"img_idx": 0, "file": "typing_overlay_p0000_2.png"}),
        ];
        let out = remap_text_info(&entries, &delete_map()).expect("remaps");
        assert!(out.changed);
        assert_eq!(out.deleted.len(), 1);
        assert_eq!(out.deleted_files, vec!["typing_overlay_p0001_1.png"]);
        assert_eq!(out.kept.len(), 1);
        // Page 0 keeps index 0 and its file name is NOT rewritten.
        assert_eq!(out.kept[0]["img_idx"], json!(0));
        assert_eq!(out.kept[0]["file"], json!("typing_overlay_p0000_2.png"));
    }

    #[test]
    fn text_info_rejects_legacy_ribbon_placement() {
        let entries = vec![json!({"x": 100.0, "y": 2000.0, "file": "t.png"})];
        assert!(matches!(
            remap_text_info(&entries, &move_map()),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn layers_manifest_remaps_img_idx_and_png_references() {
        let manifest = json!({
            "schema_version": 3,
            "pages": [
                {"img_idx": 0, "tree": [
                    {"uid": "u1", "base_file": "ps_p0000_u1.png",
                     "rendered_file": "ps_p0000_u1_fx.png", "z": 0}
                ]},
                {"img_idx": 2, "tree": [
                    {"uid": "u2", "rendered_file": "ps_p0002_u2_text.png", "z": 0}
                ]}
            ]
        });
        let out = remap_layers_manifest(&manifest, &move_map()).expect("remaps");
        assert!(out.changed);
        assert!(out.deleted_pages.is_empty());
        let pages = out.manifest["pages"].as_array().expect("pages array");
        // Sorted by the NEW img_idx: page 2 -> 1 first, page 0 -> 3 second.
        assert_eq!(pages[0]["img_idx"], json!(1));
        assert_eq!(
            pages[0]["tree"][0]["rendered_file"],
            json!("ps_p0001_u2_text.png")
        );
        assert_eq!(pages[1]["img_idx"], json!(3));
        assert_eq!(pages[1]["tree"][0]["base_file"], json!("ps_p0003_u1.png"));
        assert_eq!(
            pages[1]["tree"][0]["rendered_file"],
            json!("ps_p0003_u1_fx.png")
        );
        // schema_version survives untouched.
        assert_eq!(out.manifest["schema_version"], json!(3));
    }

    #[test]
    fn layers_manifest_splits_off_deleted_pages() {
        let manifest = json!({
            "schema_version": 3,
            "pages": [
                {"img_idx": 1, "tree": [{"uid": "gone", "base_file": "ps_p0001_g.png", "z": 0}]},
                {"img_idx": 2, "tree": []}
            ]
        });
        let out = remap_layers_manifest(&manifest, &delete_map()).expect("remaps");
        assert!(out.changed);
        assert_eq!(out.deleted_pages.len(), 1);
        assert_eq!(out.deleted_pages[0]["img_idx"], json!(1));
        let pages = out.manifest["pages"].as_array().expect("pages array");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["img_idx"], json!(1));
    }

    #[test]
    fn layers_manifest_rejects_page_without_numeric_img_idx() {
        for invalid in [json!({"tree": []}), json!({"img_idx": "1", "tree": []}), json!(7)] {
            let manifest = json!({"schema_version": 3, "pages": [invalid]});
            assert!(matches!(
                remap_layers_manifest(&manifest, &move_map()),
                Err(PageOpError::Json(_))
            ));
        }
    }

    #[test]
    fn detection_blocks_mask_file_default_is_rewritten() {
        let blocks = json!({
            "source_size": [10, 20],
            "blocks": [],
            "mask_file": "00001_mask.png"
        });
        let (out, changed) = remap_detection_blocks(&blocks, 1, 0);
        assert!(changed);
        assert_eq!(out["mask_file"], json!("00000_mask.png"));

        // A custom mask_file name is left untouched.
        let custom = json!({"mask_file": "custom.png"});
        let (out, changed) = remap_detection_blocks(&custom, 1, 0);
        assert!(!changed);
        assert_eq!(out["mask_file"], json!("custom.png"));
    }

    #[test]
    fn layers_png_name_rewrite() {
        assert_eq!(
            remap_layers_png_name("ps_p0002_u2_text.png", 2, 5),
            Some("ps_p0005_u2_text.png".to_string())
        );
        assert_eq!(remap_layers_png_name("ps_p0002_u2.png", 2, 2), None);
        assert_eq!(remap_layers_png_name("other.png", 2, 5), None);
    }
}
