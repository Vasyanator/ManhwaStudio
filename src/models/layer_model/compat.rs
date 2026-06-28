/*
File: models/layer_model/compat.rs

Purpose:
The single home for backwards-compatibility with older on-disk layer formats. Every read of
`layers.json` goes through here, so the rest of the codebase only ever sees a CANONICAL manifest at
the current schema version. Compat is **forward-only**: it migrates an old shape UP to current and
never writes an old shape back. Isolating it here guarantees that migrating a legacy field can never
silently drop or corrupt a current-format parameter — the canonical structs in `manifest.rs` stay
free of compat-only concerns.

How a read works:
  1. parse `layers.json` as untyped JSON and read its `schema_version` (absent ⇒ treated as v1);
  2. if newer than supported, log and parse best-effort (a newer binary wrote it);
  3. if older, run the forward migration chain `migrate_value` up to the current version;
  4. deserialize the (now current-shaped) JSON into `LayersManifest`.

Adding a new schema version (e.g. a future step that retires `layer_idx`/`text_groups`):
  1. bump `LAYERS_SCHEMA_VERSION` in `manifest.rs`;
  2. add a `migrate_vN_to_vN1(value)` pure transform and chain it in `migrate_value`;
  3. drop the now-retired `#[serde(default)]` from the canonical struct — the only reader of the old
     field is the migration step here, against the untyped JSON.

The v2→v3 step (the current top version) is a structural no-op: v3 only ADDED serde-default `Option`
text-payload fields, so a v2 file is already a valid v3. The cross-file fold (`text_info.json` payload
→ an inline node) is done on read in `layer_doc::ensure_page_loaded`, not here — `migrate_value` is a
pure transform over the single `layers.json` Value and cannot see `text_info.json`.
*/

use super::manifest::{LayersManifest, LAYERS_SCHEMA_VERSION};
use serde_json::Value;
use std::fs;
use std::path::Path;

/// Reads `layers.json` at `path` and returns it as a canonical, current-version `LayersManifest`,
/// migrating any older format up. `Ok(None)` when the file does not exist; `Err` only on IO / JSON
/// errors. This is the one entry point `persist::read_manifest` delegates to.
pub fn read_manifest(path: &Path) -> Result<Option<LayersManifest>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let manifest = manifest_from_value(value, &path.display().to_string())?;
    Ok(Some(manifest))
}

/// Turns raw `layers.json` JSON into a canonical manifest, applying version migrations. Factored out
/// of `read_manifest` so it is unit-testable without touching the filesystem.
fn manifest_from_value(value: Value, src: &str) -> Result<LayersManifest, String> {
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(1) as u32;

    let value = if version > LAYERS_SCHEMA_VERSION {
        // A newer binary wrote this. Read best-effort: serde keeps the fields it knows and the
        // forward-looking schema means unknown additions are simply ignored.
        crate::runtime_log::log_warn(format!(
            "[layers] {src} schema_version {version} newer than supported {LAYERS_SCHEMA_VERSION}, reading best-effort"
        ));
        value
    } else if version < LAYERS_SCHEMA_VERSION {
        migrate_value(value, version)
    } else {
        value
    };

    serde_json::from_value(value).map_err(|e| format!("parse {src}: {e}"))
}

/// Applies the chain of forward migrations from `from_version` up to the current version. Each step
/// is a pure transform on the untyped JSON; missing/renamed/retired fields are handled here, never by
/// a `#[serde(default)]` leaking a compat concern onto a canonical struct.
fn migrate_value(mut value: Value, from_version: u32) -> Value {
    // v1 → v2: `LayerRec.pinned_by_group` and `GroupRec.collapsed` were added; both default to false,
    // so a v1 file is structurally already a valid v2 — no field rewrite needed. The step exists so
    // the *reason* lives with compat (and so the v1→v2 link is explicit when v3 chains onto it).
    if from_version < 2 {
        // structurally a no-op; absent flags deserialize to false.
    }

    // v2 → v3: TEXT nodes gained inline payload fields (`render_data`, `mask_clip`, and reused
    // `transform`/`deform` geometry; the rendered PNG name in `rendered_file`). All are serde-default
    // `Option`s, so a v2 file — whose text nodes carry only a `payload_ref` into `text_info.json` —
    // is structurally already a valid v3 (the new fields deserialize to `None`). The actual cross-file
    // fold (`text_info.json` payload → an inline node) cannot happen here: `migrate_value` is a pure
    // transform over the single `layers.json` Value with no access to `text_info.json`. That migration
    // is done on read in `layer_doc::ensure_page_loaded`, which has both files: a v2 node (no inline
    // `render_data`) falls back to the legacy overlay entry; a v3 node builds from the inline payload.
    if from_version < 3 {
        // structurally a no-op; absent inline text fields deserialize to None.
    }

    // Stamp the canonical version so a later forward-only write records current, not the old number.
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "schema_version".into(),
            Value::from(u64::from(LAYERS_SCHEMA_VERSION)),
        );
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_an_identity_pass() {
        // A manifest already at the current version round-trips unchanged through the compat reader.
        let json = serde_json::json!({
            "schema_version": LAYERS_SCHEMA_VERSION,
            "pages": [{
                "img_idx": 0,
                "tree": [{
                    "uid": "r", "name": "R", "kind": "raster", "z": 0,
                    "visible": true, "opacity": 1.0,
                    "transform": { "cx": 1.0, "cy": 1.0, "rotation": 0.0, "scale": 1.0 },
                    "base_file": "ps_p0000_r.png", "image_size": [2, 2]
                }]
            }]
        });
        let m = manifest_from_value(json, "test").unwrap();
        assert_eq!(m.schema_version, LAYERS_SCHEMA_VERSION);
        assert_eq!(m.pages[0].tree[0].uid, "r");
    }

    #[test]
    fn v1_file_migrates_up_and_is_stamped_current() {
        // A v1 file lacks `pinned_by_group` / `collapsed`; it must read cleanly and be re-stamped to
        // the current version so a forward-only write never re-emits the old version number.
        let json = serde_json::json!({
            "schema_version": 1,
            "pages": [{
                "img_idx": 0,
                "groups": [{ "uid": "g", "name": "G", "visible": true, "opacity": 1.0 }],
                "tree": [{
                    "uid": "t", "name": "T", "kind": "text", "z": 3,
                    "layer_idx": 0, "pinned": true,
                    "visible": true, "opacity": 1.0,
                    "payload_ref": { "store": "text_info", "uid": "t" }
                }]
            }]
        });
        let m = manifest_from_value(json, "test").unwrap();
        assert_eq!(m.schema_version, LAYERS_SCHEMA_VERSION, "re-stamped to current");
        assert!(!m.pages[0].tree[0].pinned_by_group, "added flag defaults false");
        assert!(!m.pages[0].groups[0].collapsed, "added flag defaults false");
        assert!(m.pages[0].tree[0].pinned, "existing fields preserved");
    }

    #[test]
    fn v2_text_node_reads_as_v3_with_inline_fields_defaulting_none() {
        // A v2 text node carries only a payload_ref (payload in text_info.json); the v3 inline fields
        // must default to None and the file re-stamp to v3.
        let json = serde_json::json!({
            "schema_version": 2,
            "pages": [{
                "img_idx": 0,
                "tree": [{
                    "uid": "t", "name": "T", "kind": "text", "z": 3,
                    "layer_idx": 0, "visible": true, "opacity": 1.0,
                    "payload_ref": { "store": "text_info", "uid": "t" }
                }]
            }]
        });
        let m = manifest_from_value(json, "test").unwrap();
        assert_eq!(m.schema_version, LAYERS_SCHEMA_VERSION, "re-stamped to current (v3)");
        let node = &m.pages[0].tree[0];
        assert!(node.render_data.is_none(), "inline render_data defaults None for a v2 node");
        assert!(node.mask_clip.is_none(), "inline mask_clip defaults None for a v2 node");
        assert!(node.payload_ref.is_some(), "legacy payload_ref preserved");
    }

    #[test]
    fn missing_version_is_treated_as_v1() {
        let json = serde_json::json!({ "pages": [] });
        let m = manifest_from_value(json, "test").unwrap();
        assert_eq!(m.schema_version, LAYERS_SCHEMA_VERSION);
    }

    #[test]
    fn newer_version_reads_best_effort_without_error() {
        let json = serde_json::json!({
            "schema_version": LAYERS_SCHEMA_VERSION + 5,
            "pages": [{ "img_idx": 0, "tree": [] }]
        });
        let m = manifest_from_value(json, "test").unwrap();
        assert_eq!(m.schema_version, LAYERS_SCHEMA_VERSION + 5, "kept as-is, not downgraded");
        assert_eq!(m.pages.len(), 1);
    }
}
