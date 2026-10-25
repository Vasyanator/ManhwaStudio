/*
File: models/layer_model/layer_doc.rs

Purpose:
The foundational shared, in-memory, per-page layer document that both view tabs (the PS editor and
the typing tab) will eventually read and edit instead of their own separate caches. Both tabs always
display the SAME page (a shared page index), so this document is current-page-centric: pages are
loaded on demand and can be evicted.

A `LayerNode` is the unified notion of a layer — a raster node (pixels from any source) or a text
node (re-renderable from text params) — mirroring the on-disk `manifest::LayerKindRec` capability
model. This step (step 1) implements the types plus the RASTER node load / flush / in-memory ops,
backed by the existing `persist` functions. Text-node DISK loading is a follow-up (step 1b); the
`Text` node body type is defined now and exercised by a unit test so it is real, not dead code.

Persistence is via the existing `persist` API: `load_page_rasters` / `load_page_bands` on the way
in, `save_page_rasters` + `update_raster_effects` on the way out. The doc is authoritative over the
rasters it holds, so a flush writes the doc's effects chain (not merely whatever was on disk).

The doc exposes BOTH a synchronous flush (`flush_page` / `flush_page_text`, byte-stable, used by
tests and read-back paths) and an ADDITIVE async path: with an optional `LayerSaver` enabled
(`enable_background_saver`), `enqueue_page_save` / `enqueue_page_text_save` build an OWNED page job
(via `build_page_save_job`, cloning the same data the sync flush gathers) and hand it to the
coalescing background saver in `saver.rs`; without a saver they fall back to the synchronous flush.
The saver runs the identical `persist::*` write sequence off-thread, so on-disk bytes match the sync
flush (the async job carries a raster's pixels only when it is dirty; a failed raster ack re-dirties
the page's rasters so the retry carries them). `Drop` shuts the saver down (flush queue + join).
With an `AutosaveGate` passed to `enable_background_saver`, the saver HOLDS jobs until the gate is
due; every async funnel call reports ONE action to the gate (`note_autosave_action`), never a
per-frame dirty mark.

Structural edits (PS band order / grouping) and raster deletions are saver jobs too
(`enqueue_page_band_order` / `enqueue_page_grouping` / `enqueue_page_save_dropping_raster`), so every
`layers.json` write of a doc-wired tab is FIFO-ordered behind the page's earlier content saves. The PS
text pin meta (`LayerNode.text_pinned` / `text_pinned_by_group`) lives on the node
(`set_text_pin_meta`), so the PS editor never re-reads it from disk after the page load. The PS
unified group composite meta (visible / opacity) is doc-owned as well (`set_group_meta`): every tab's
group fold reads `DocPage::groups`, and the page's next doc save writes it. The panel-only
`collapsed` flag has the doc as its single owner too (`set_group_collapsed`).
*/

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use eframe::egui::ColorImage;
use ms_text_render::types::RenderedTextExtraInfo;
use serde_json::Value;

use super::manifest::{CenteringFrameRec, DeformRec, TextCentersRec, TransformRec};
use super::ordering::Band;
use super::persist::{self, BandRef, GroupMeta, GroupingEdit, RasterLayerOut};
use super::saver::{
    EffectsSaveItem, LayerSaver, LayerSaverHandle, OwnedRasterLayer, OwnedTextNode, PageSaveJob,
    RasterSavePart, SaveAckMap, SaveKind, StructuralEdit, TextSavePart,
};
use super::text_payload;
use crate::autosave_gate::AutosaveGate;
use ms_log::trace::cat;

/// Decodes a PNG at `path` into an unmultiplied `ColorImage`, mirroring the raster load path's
/// decode. Returns `None` if the file is absent or undecodable.
fn read_png(path: &Path) -> Option<ColorImage> {
    if !path.is_file() {
        return None;
    }
    let rgba = image::open(path).ok()?.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Converts the renderer's in-memory centering-assist centers into the on-disk record.
///
/// Returns `None` when neither center was measured, so the manifest field stays absent and the JSON
/// key is omitted (the `skip_serializing_if` contract of `LayerRec.text_centers`). This is the ONE
/// direction of the conversion boundary that keeps `ms-text-render` free of a serde dependency.
///
/// A center with any NON-FINITE component (NaN / ±∞) is treated as NOT measured and dropped here,
/// exactly as an unmeasured one is. This is not cosmetic: `serde_json` serializes a non-finite float
/// as a bare `null` WITHOUT reporting an error, and the next `read_manifest` then fails to decode
/// that `null` into an `f32` — which makes the chapter's whole `layers.json` both unreadable and
/// unwritable (rasters included) until a human edits the JSON by hand. Losing one measurement that
/// was garbage anyway is the strictly cheaper outcome.
#[must_use]
fn text_centers_rec_from(extra: &RenderedTextExtraInfo) -> Option<TextCentersRec> {
    /// A center is usable only when BOTH components are finite (see the non-finite rationale above).
    fn finite(c: Option<[f32; 2]>) -> Option<[f32; 2]> {
        c.filter(|p| p.iter().all(|v| v.is_finite()))
    }
    let rec = TextCentersRec {
        mean: finite(extra.mean_center),
        median: finite(extra.median_center),
    };
    (!rec.is_empty()).then_some(rec)
}

/// Converts an on-disk centers record back into the renderer's in-memory shape. `None` ⇒ all-`None`,
/// i.e. "never measured", which is what every pre-v4 node yields.
#[must_use]
fn extra_centers_from_rec(rec: Option<TextCentersRec>) -> RenderedTextExtraInfo {
    RenderedTextExtraInfo {
        mean_center: rec.and_then(|c| c.mean),
        median_center: rec.and_then(|c| c.median),
    }
}

/// The kind of a unified layer node. Mirrors `manifest::LayerKindRec` (sans `Group`, which is a
/// container, not a node body here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Raster,
    Text,
}

/// The body of a layer node, carrying the kind-specific pixels and payload.
pub enum NodeBody {
    /// A raster node: pre-effects `base_image`, post-effects `display_image` (equal to the base when
    /// there are no effects), the opaque effects chain, the base PNG filename on disk, and the
    /// persisted mask-clip flag.
    Raster {
        base_image: ColorImage,
        display_image: ColorImage,
        effects: Vec<Value>,
        base_file: String,
        /// Persisted mask-clip flag (typing tab): whether the raster is clipped to the page mask.
        /// `None`/`Some(false)` ⇒ no clip (rasters default OFF). Round-trips through `LayerRec.mask_clip`.
        mask_clip: Option<bool>,
    },
    /// A text node: its render params (`render_data`, opaque here), the rendered `image`, the uid of
    /// the payload it references in the text store, and the persisted mask-clip flag. `is_image == true`
    /// marks a placed PNG image overlay: `render_data` is `Value::Null` and it is not re-renderable. The text flush
    /// (`LayerDoc::flush_page`) writes these inline into `layers.json` (schema v3); the load builds
    /// them from the inline node when present, else from the legacy `text_info.json` overlay entry.
    Text {
        render_data: Value,
        image: ColorImage,
        is_image: bool,
        /// The uid of the overlay payload this text node renders from (its `payload_ref` uid, or the
        /// node uid for a v3 inline node). Written back as the node's `payload_uid` on flush.
        payload_uid: String,
        /// Persisted `mask_clip_enabled` (typing tab): whether the text is clipped to its mask. `None`
        /// ⇒ default. Round-trips through the v3 inline payload.
        mask_clip: Option<bool>,
        /// Renderer-measured centering-assist centers for `image`, in final-image pixels.
        ///
        /// PERSISTED since schema v4 (`LayerRec.text_centers`), so a chapter re-open restores the
        /// bound center instead of collapsing onto the plain image center. Held HERE rather than only
        /// on the typing tab's runtime because `sync_from_doc` rebuilds that runtime from this node —
        /// a runtime-only copy is wiped by the projection that follows every `set_text_render`.
        /// INVARIANT (unchanged by persistence): always written together with `image` (see
        /// [`LayerDoc::set_text_render`]), so the centers can never describe pixels other than the
        /// ones stored here.
        extra_centers: RenderedTextExtraInfo,
        /// The typing tab's centering-assist guide rectangle bound to this node, persisted since
        /// schema v4 (`LayerRec.centering_frame`).
        ///
        /// The typing runtime is the LIVE owner (a drag in progress must win); this is the DURABLE
        /// copy, pushed here by the typing tab's placement sync and read back when a runtime is
        /// created. `None` ⇒ no frame yet. Deliberately NOT tied to `image`: the frame is an anchor
        /// the user places, not a measurement of the pixels, and survives every re-render.
        ///
        /// LIMITATION — SET-ONLY. The placement sync only ever writes a `Some` here
        /// (`tabs/typing/tab/persist.rs`, `sync_overlay_state_into_doc`), because a runtime's `None`
        /// means "this runtime has not picked the frame up yet", not "the user removed it". There is
        /// therefore NO path that clears a stored frame. That is correct for every path that exists
        /// today, but a future "remove the frame" action cannot be built by resetting the runtime to
        /// `None`: the frame would resurrect from disk on the next reload. Such an action must add an
        /// explicit clear that reaches this field.
        centering_frame: Option<CenteringFrameRec>,
    },
}

/// One unified layer node, resident in memory for a loaded page.
pub struct LayerNode {
    pub uid: String,
    pub name: String,
    pub kind: NodeKind,
    /// Unified Z, bottom-to-top.
    pub z: u32,
    pub visible: bool,
    pub opacity: f32,
    pub group_uid: Option<String>,
    /// TEXT node only: the typing tab's «Группа текста N» grouping axis (`layer_idx`). Carried on the
    /// node so a doc text flush persists it for NEW overlays too (the persist layer separately preserves
    /// it for existing nodes). `None` for rasters and for text nodes whose grouping is unknown (flush
    /// then defaults to the persisted value, else 0). Orthogonal to the PS `group_uid`.
    pub text_layer_idx: Option<u32>,
    /// TEXT node only (always `false` on a raster): the PS editor's explicit-pin flag, decoded from
    /// the node's `layers.json` record and updated by [`LayerDoc::set_text_pin_meta`], so the PS
    /// editor reads its pin state from the doc instead of re-reading disk. Not written by the text
    /// flush — the writer always emits text pinned-with-explicit-Z.
    pub text_pinned: bool,
    /// TEXT node only (always `false` on a raster): the pin is owned by PS grouping (vs. an explicit
    /// user pin). Doc-owned and written verbatim by the text flush (`pinned_by_group`).
    pub text_pinned_by_group: bool,
    pub transform: TransformRec,
    pub deform: Option<DeformRec>,
    /// Bumped whenever pixels change, so the GPU texture cache can invalidate.
    pub generation: u64,
    /// The raster base pixels were edited this session (paint / cut / merge / bake), so a flush must
    /// rewrite the base PNG and drop any non-destructive effects chain.
    pub pixels_dirty: bool,
    pub body: NodeBody,
}

impl LayerNode {
    #[must_use]
    pub fn is_raster(&self) -> bool {
        matches!(self.kind, NodeKind::Raster)
    }

    // Test-only accessors: both tabs read `NodeBody` by pattern match rather than via these helpers,
    // so they exist only in test builds (avoiding a dead-code allow in the production build). The
    // `test-support` feature is what carries them into a DEPENDENT crate's tests — the typing tab's
    // tests live in the binary, and a `#[cfg(test)]` item is invisible outside its own crate.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn is_text(&self) -> bool {
        matches!(self.kind, NodeKind::Text)
    }

    /// The image a tab should display: the raster's post-effects render, or the text's rendered image.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn display_image(&self) -> &ColorImage {
        match &self.body {
            NodeBody::Raster { display_image, .. } => display_image,
            NodeBody::Text { image, .. } => image,
        }
    }

    /// Bumps the GPU-cache invalidation counter (call after changing pixels / display image).
    pub fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        ms_log::trace_log!(
            cat::SYNC,
            "bump_generation uid={} generation={}",
            self.uid,
            self.generation
        );
    }
}

/// One resident page: its nodes (sorted bottom-to-top by `z`) and its raster-layer groups.
pub struct DocPage {
    pub nodes: Vec<LayerNode>,
    pub groups: Vec<GroupMeta>,
}

/// An OWNED, fully-decoded page payload built off any doc lock by [`LayerDoc::decode_page_payload`].
///
/// It is the exact set of values `ensure_page_loaded` produces BEFORE inserting them into
/// `LayerDoc::pages`: the page's raster + text `LayerNode`s (already sorted bottom-to-top and
/// re-ranked to unique contiguous `z`) and its raster-layer groups. All disk I/O and PNG decode that
/// produced it happened WITHOUT holding the doc lock; moving it into the doc is a cheap, lock-brief
/// insert (`insert_decoded_page`). `LayerNode`/`ColorImage`/`Value` are `Send`, so a payload can be
/// produced on a worker thread and inserted on the GUI thread.
#[must_use]
pub struct DecodedPagePayload {
    pub nodes: Vec<LayerNode>,
    pub groups: Vec<GroupMeta>,
}

// Compact, pixel-free Debug so a `LoadedPage`/`PageLoadResult` carrying a payload stays `Debug`
// (the page-loader worker structs derive it) without dumping multi-MB image buffers into logs.
impl std::fmt::Debug for DecodedPagePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedPagePayload")
            .field("nodes", &self.nodes.len())
            .field("groups", &self.groups.len())
            .finish()
    }
}

/// The shared in-memory layer document: pages loaded on demand, keyed by page index.
pub struct LayerDoc {
    pages: HashMap<usize, DocPage>,
    /// Monotonic document-wide version, bumped by EVERY mutating operation (page load/evict, node/group
    /// adds and removes, transform/effects/pixels/text/visibility/opacity/group/reorder edits) on ANY
    /// page. Means "something in the doc changed"; whole-doc readers (the page manager's node count)
    /// gate on it. A reader that projects ONE page gates on [`Self::page_revision`] instead.
    version: u64,
    /// Per-page revision stamps: `page_idx -> version` right after the last mutation of that page
    /// (the `CleanOverlaysModel::page_change_revision` pattern). Written only by [`Self::bump_page`].
    /// Stamps are NEVER removed (an evict is itself a bump of the page), so a stamp only grows and is
    /// never reused across evict + re-insert; the map is bounded by the chapter's page count.
    page_revisions: HashMap<usize, u64>,
    /// Optional background saver (additive). When present, `enqueue_page_save` /
    /// `enqueue_page_text_save` route page persistence off-thread (coalescing); when absent (the
    /// default — e.g. unit tests that construct a bare `LayerDoc`), those entry points fall back to the
    /// synchronous `flush_page` / `flush_page_text`. Created lazily via `enable_background_saver` so a
    /// constructed-and-dropped doc spawns no thread.
    saver: Option<LayerSaver>,
    save_epoch: u64,
    latest_epoch: HashMap<(usize, SaveKind), u64>,
    save_acks: Option<Arc<Mutex<SaveAckMap>>>,
}

impl Default for LayerDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl LayerDoc {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            version: 0,
            page_revisions: HashMap::new(),
            saver: None,
            save_epoch: 0,
            latest_epoch: HashMap::new(),
            save_acks: None,
        }
    }

    /// The current document-wide version: advances on every mutation of any page. For whole-doc
    /// readers only; a tab projecting one page compares [`Self::page_revision`] instead, so an edit
    /// on another page does not make its projection look stale.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The revision of `page_idx`: the document [`Self::version`] right after that page's last
    /// mutation (load, evict, or any content edit), or 0 for a page never touched this session.
    ///
    /// Contract: a content change of the page always changes this value; a mutation of another page
    /// never does; the value never decreases and is never reused (it survives evict + re-insert), and
    /// it is always `<= version()`. A spurious advance (a call that changed nothing visible) is
    /// allowed and costs the reader one re-projection. Cheap: one map lookup under the doc lock.
    #[must_use]
    pub fn page_revision(&self, page_idx: usize) -> u64 {
        self.page_revisions.get(&page_idx).copied().unwrap_or(0)
    }

    /// Records a mutation of `page_idx`: advances the document version and stamps the page with it.
    /// The ONLY bump of the doc — there is deliberately no page-less variant, so a new mutator cannot
    /// bump without naming the page it changed. A new mutator must also get a step in the
    /// `page_revision_tracks_every_content_change` property test.
    fn bump_page(&mut self, page_idx: usize) {
        self.version = self.version.wrapping_add(1);
        self.page_revisions.insert(page_idx, self.version);
        ms_log::trace_log!(cat::SYNC, "bump_page page={} version={}", page_idx, self.version);
    }

    /// Explicitly records a change of `page_idx` (advances its [`Self::page_revision`] and the
    /// document version). For callers that mutate node fields directly via [`Self::node_mut`]
    /// (bypassing the bumping mutators) and still need cross-tab listeners to re-project that page.
    pub fn mark_page_changed(&mut self, page_idx: usize) {
        ms_log::trace_log!(cat::SYNC, "mark_page_changed page={} version_before={}", page_idx, self.version);
        self.bump_page(page_idx);
    }

    /// Loads `page_idx` into memory if not already resident.
    ///
    /// Convenience composition of [`Self::decode_page_payload`] (disk I/O + PNG decode + legacy
    /// migration, lock-free) and [`Self::insert_decoded_page`] (the cheap memoized move).
    ///
    /// TEST-ONLY reference implementation: every production caller now runs the two steps explicitly so
    /// the multi-MB PNG decode happens off the shared doc lock (a worker in the PS page loader, the GUI
    /// thread before locking in the typing tab). This monolith is retained only as the differential
    /// oracle the split path is unit-tested against (see
    /// `decode_then_insert_matches_ensure_page_loaded_incl_legacy_ribbon`); keeping it out of production
    /// prevents a future caller from reintroducing a decode under the lock.
    ///
    /// See [`Self::decode_page_payload`] for the full decode contract (raster + text node build,
    /// unified-Z re-rank, the FULL-`page_sizes`-map ribbon-migration requirement).
    ///
    /// # Errors
    /// Propagates [`Self::decode_page_payload`]'s error string.
    #[cfg(any(test, feature = "test-support"))]
    pub fn ensure_page_loaded(
        &mut self,
        page_idx: usize,
        primary_dir: &Path,
        fallback_dir: Option<&Path>,
        legacy_text_dir: Option<&Path>,
        page_sizes: &HashMap<usize, [usize; 2]>,
    ) -> Result<(), String> {
        if self.pages.contains_key(&page_idx) {
            return Ok(());
        }
        // Decode off the (here non-existent) lock, then move the payload in. Identical behavior to the
        // pre-split monolith; the split lets a worker run `decode_page_payload` lock-free and the GUI
        // thread run only the cheap `insert_decoded_page`.
        let payload =
            Self::decode_page_payload(page_idx, primary_dir, fallback_dir, legacy_text_dir, page_sizes)?;
        self.insert_decoded_page(page_idx, payload);
        Ok(())
    }

    /// Inserts an already-decoded page payload into the doc, taking effect only if `page_idx` is not
    /// already resident (memoization: a live page's in-memory edits are NEVER clobbered by a stale
    /// background decode). On a real insert the page's revision (and the document `version`) is bumped
    /// so both tabs re-project it.
    ///
    /// This is the ONLY lock-held step of a page load: it moves an OWNED [`DecodedPagePayload`] (built
    /// lock-free by [`Self::decode_page_payload`]) into `self.pages`. No disk I/O or PNG decode happens
    /// here, so a shared `Arc<Mutex<LayerDoc>>` is held only for this cheap move.
    pub fn insert_decoded_page(&mut self, page_idx: usize, payload: DecodedPagePayload) {
        if self.pages.contains_key(&page_idx) {
            // Already loaded (e.g. a concurrent sync load, or the page was edited between the
            // background decode request and this insert): discard the freshly-decoded payload so live
            // in-memory edits survive. Mirrors `ensure_page_loaded`'s memoization no-op.
            ms_log::trace_log!(
                cat::PERSIST,
                "insert_decoded_page page={} already resident; discarding decoded payload",
                page_idx
            );
            return;
        }
        ms_log::trace_log!(
            cat::PERSIST,
            "insert_decoded_page page={} nodes={} groups={}",
            page_idx,
            payload.nodes.len(),
            payload.groups.len()
        );
        self.pages.insert(
            page_idx,
            DocPage {
                nodes: payload.nodes,
                groups: payload.groups,
            },
        );
        self.bump_page(page_idx);
    }

    /// Decodes a page's full layer payload from disk WITHOUT touching `LayerDoc` state, so it can run
    /// off the GUI thread (the page loader worker) or under no lock. It performs ALL the disk I/O and
    /// PNG decode + legacy migration that `ensure_page_loaded` used to do inline, and returns the OWNED
    /// [`DecodedPagePayload`] to hand to [`Self::insert_decoded_page`].
    ///
    /// `legacy_text_dir` is the un-migrated legacy `text_images/` directory, appended as the LAST text
    /// source (after `primary_dir` and `fallback_dir`), or `None` once the chapter's inline manifest text
    /// has migrated. It feeds BOTH the `text_info.json` read and the overlay-PNG lookups, so a
    /// never-migrated legacy chapter's text becomes visible in the shared doc (hence the PS editor).
    /// A uid-less legacy entry receives a deterministic uid via [`text_payload::stable_overlay_uid`],
    /// matching the typing loader so the same overlay never double-renders.
    ///
    /// `page_sizes` MUST be the FULL chapter page-size map (`page_idx -> [w, h]`): the absolute-ribbon
    /// legacy migration recovers a chapter-wide ribbon scale from EVERY page's aspect ratio, so a
    /// partial map corrupts the geometry of legacy chapters. The loaded page's own size is
    /// `page_sizes[page_idx]` (defaulting to `[1, 1]` when absent).
    ///
    /// # Errors
    /// Returns the persist-layer error string if `load_page_rasters` or `load_page_text_nodes` fails.
    pub fn decode_page_payload(
        page_idx: usize,
        primary_dir: &Path,
        fallback_dir: Option<&Path>,
        legacy_text_dir: Option<&Path>,
        page_sizes: &HashMap<usize, [usize; 2]>,
    ) -> Result<DecodedPagePayload, String> {
        let _span = ms_log::trace_scope!(cat::PERSIST, "decode_page_payload page={}", page_idx);
        // Pixel size of the page being loaded (page-relative uv → page px). The FULL chapter map is
        // required for the cross-entry ribbon migration below — see the comment there.
        let page_size = page_sizes.get(&page_idx).copied().unwrap_or([1, 1]);

        let rasters = persist::load_page_rasters(primary_dir, fallback_dir, page_idx)?;
        let bands = persist::load_page_bands(primary_dir, fallback_dir, page_idx);
        ms_log::trace_log!(
            cat::PERSIST,
            "decode_page_payload page={} rasters={} groups={} bands={}",
            page_idx,
            rasters.layers.len(),
            rasters.groups.len(),
            bands.len()
        );

        // uid -> band z, for the raster bands.
        let mut z_by_uid: HashMap<String, u32> = HashMap::new();
        for band in &bands {
            if let Band::Raster { uid, z } = band {
                z_by_uid.insert(uid.clone(), *z);
            }
        }

        let mut nodes: Vec<LayerNode> = Vec::with_capacity(rasters.layers.len());
        for (idx, layer) in rasters.layers.into_iter().enumerate() {
            let z = z_by_uid.get(&layer.uid).copied().unwrap_or(idx as u32);
            nodes.push(LayerNode {
                uid: layer.uid,
                name: layer.name,
                kind: NodeKind::Raster,
                z,
                visible: layer.visible,
                opacity: layer.opacity,
                group_uid: layer.group_uid,
                text_layer_idx: None,
                text_pinned: false,
                text_pinned_by_group: false,
                transform: layer.transform,
                deform: layer.deform,
                generation: 0,
                pixels_dirty: false,
                body: NodeBody::Raster {
                    base_image: layer.base_image,
                    display_image: layer.image,
                    effects: layer.effects,
                    base_file: layer.base_file,
                    mask_clip: layer.mask_clip,
                },
            });
        }
        // Text nodes: metadata comes from `layers.json` text nodes; geometry/pixels/render params
        // come from the `text_info.json` overlay payload keyed by the same uid.
        let mut text_dirs: Vec<&Path> = vec![primary_dir];
        if let Some(fb) = fallback_dir {
            text_dirs.push(fb);
        }
        // The un-migrated legacy `text_images/` dir is the LAST text source: a migrated chapter whose
        // `text_info.json` lives in `layers/` still wins over any stale `text_images/text_info.json`,
        // while a never-migrated chapter (which only has `text_images/`) still resolves its overlays.
        if let Some(legacy) = legacy_text_dir {
            text_dirs.push(legacy);
        }
        // Normalize the cross-entry legacy families (ribbon x/y, top-left u/v) to modern
        // `img_u`/`img_v` BEFORE per-entry decode, identically to the typing tab's loader — so an old
        // chapter resolves the same in both tabs and migrates to the inline v3 payload without snapping
        // text to page-center. The absolute-ribbon family recovers the chapter's SHARED ribbon scale
        // from every page's aspect ratio, so the caller must pass the FULL chapter page-size map (not
        // just this page) — otherwise the missing pages default to a square aspect and the solve, hence
        // every page's resulting `img_u`/`img_v`, is wrong. The PNG footprint (top-left case) is
        // resolved from the text dirs.
        let raw_entries = text_payload::read_overlay_entries(&text_dirs);
        let overlay_entries =
            text_payload::migrate_overlay_entries(&raw_entries, page_sizes, |obj| {
                obj.get("file")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    // Overlay PNGs live FLAT in the text dirs, but a legacy `file` may carry a
                    // directory prefix. Resolve by the final component only, exactly as the other two
                    // `migrate_overlay_entries` callers do (`migrate::overlay_png_path`, the typing
                    // loader in `tab/helpers.rs`) — otherwise the same entry gets a footprint in two
                    // callers and none here, and its migrated centre would differ between them.
                    .and_then(|file| Path::new(file).file_name().and_then(|n| n.to_str()))
                    .and_then(|name| {
                        text_dirs.iter().find_map(|dir| {
                            image::image_dimensions(dir.join(name))
                                .ok()
                                .map(|(w, h)| (w as f32, h as f32))
                        })
                    })
                    .unwrap_or((0.0, 0.0))
            });

        // Pinned-text band Z by uid; text-group band Z by layer_idx — for the unified text node Z.
        let mut pinned_z_by_uid: HashMap<String, u32> = HashMap::new();
        let mut group_z_by_layer_idx: HashMap<u32, u32> = HashMap::new();
        for band in &bands {
            match band {
                Band::PinnedText { uid, z } => {
                    pinned_z_by_uid.insert(uid.clone(), *z);
                }
                Band::TextGroup { layer_idx, z, .. } => {
                    group_z_by_layer_idx.insert(*layer_idx, *z);
                }
                Band::Raster { .. } => {}
            }
        }
        let fallback_text_z = bands.len() as u32;

        // Overlay sources are unioned by uid: a schema-v3 `layers.json` text node carries its FULL
        // inline payload (render_data + geometry + rendered PNG + mask_clip) and is self-sufficient —
        // it builds the text node directly and WINS over any legacy entry of the same uid. A legacy
        // (v2) node carries only PS-owned overrides (visibility / opacity / group / pin-Z); its
        // authoritative payload still comes from the `text_info.json` overlay entry. So: build every
        // inline node first, then iterate the overlay entries skipping uids already built inline. This
        // keeps old chapters (no inline payload) working while letting v3 chapters list+build text
        // without `text_info.json`. (Iterating only the nodes would drop every plain legacy overlay.)
        let node_meta: HashMap<String, persist::TextNodeIn> =
            persist::load_page_text_nodes(primary_dir, fallback_dir, page_idx)?
                .into_iter()
                .map(|n| (n.uid.clone(), n))
                .collect();
        let mut texts: Vec<LayerNode> = Vec::new();
        let mut built_inline: HashMap<String, ()> = HashMap::new();
        let (mut text_n, mut image_n) = (0usize, 0usize);

        // 1) v3 inline text nodes: build straight from `layers.json` (no `text_info.json` needed).
        for meta in node_meta.values() {
            let Some(inline) = &meta.inline else {
                continue;
            };
            // The rendered PNG is named by `rendered_file`; decode it from the layers dir(s). A node
            // with no rendered image yet (None / missing PNG) is skipped with a warning, mirroring the
            // legacy path's "missing overlay image" handling.
            let image = inline
                .rendered_file
                .as_deref()
                .and_then(|f| text_dirs.iter().find_map(|dir| read_png(&dir.join(f))));
            let Some(image) = image else {
                ms_log::runtime_log::log_warn(format!(
                    "[layer_doc] inline text node '{}' page {page_idx} has no rendered image; skipping",
                    meta.uid
                ));
                continue;
            };
            let layer_idx = meta.layer_idx;
            // Round-trip the persisted node name (stable across reloads). Fall back to a generated
            // "Текст {n}" only when the stored name is empty (e.g. an older inline node).
            let name = if meta.name.trim().is_empty() {
                text_n += 1;
                format!("Текст {text_n}")
            } else {
                meta.name.clone()
            };
            let z = pinned_z_by_uid
                .get(&meta.uid)
                .copied()
                .or_else(|| group_z_by_layer_idx.get(&layer_idx).copied())
                .unwrap_or(fallback_text_z);
            // Geometry from the inline node; fall back to image-centered affine if absent.
            let transform = inline.transform.unwrap_or(TransformRec {
                cx: image.size[0] as f32 * 0.5,
                cy: image.size[1] as f32 * 0.5,
                rotation: 0.0,
                scale: 1.0,
            });
            texts.push(LayerNode {
                uid: meta.uid.clone(),
                name,
                kind: NodeKind::Text,
                z,
                visible: meta.visible,
                opacity: meta.opacity,
                group_uid: meta.group_uid.clone(),
                text_layer_idx: Some(layer_idx),
                text_pinned: meta.pinned,
                text_pinned_by_group: meta.pinned_by_group,
                transform,
                deform: inline.deform.clone(),
                generation: 0,
                pixels_dirty: false,
                body: NodeBody::Text {
                    render_data: inline.render_data.clone(),
                    image,
                    is_image: inline.is_image,
                    payload_uid: meta.payload_uid.clone(),
                    mask_clip: inline.mask_clip,
                    // Schema v4 carries the centers alongside the PNG they describe; a pre-v4 node
                    // yields all-`None` and the first assist-enabled render fills them in.
                    extra_centers: extra_centers_from_rec(inline.text_centers),
                    centering_frame: inline.centering_frame,
                },
            });
            built_inline.insert(meta.uid.clone(), ());
        }

        // A page is MIGRATED once it carries any inline text node: `write_page_text_payload` always
        // writes the page's FULL text set inline at once, so "any inline node" ⇒ "all text is inline".
        // For a migrated page the legacy `text_info.json` is ignored entirely — otherwise an overlay
        // that was DELETED (or rasterized) from the inline set would resurrect from the stale legacy
        // file. A page with no inline nodes is still pure-legacy and reads `text_info.json` below.
        let page_is_migrated = !built_inline.is_empty();
        let legacy_entries: &[Value] = if page_is_migrated {
            &[]
        } else {
            &overlay_entries
        };

        // 2) Legacy overlays from `text_info.json` (migration-on-read), skipping uids already inline.
        for entry in legacy_entries {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            // Entries without `img_idx` default to page 0 (mirrors the legacy PS overlay loader).
            let entry_page = obj.get("img_idx").and_then(Value::as_u64).unwrap_or(0) as usize;
            if entry_page != page_idx {
                continue;
            }
            // `file` is read FIRST: it seeds the deterministic uid for uid-less legacy entries and is
            // reused below for the PNG read. Trimmed to match the typing codec's `str::trim`
            // (`normalize_overlay_storage_entry` / `decode_overlay_from_storage_entry`), so a name with
            // surrounding whitespace still resolves its PNG AND seeds the SAME uid on both paths.
            let Some(file) = obj
                .get("file")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            // Legacy `text_info.json` has no `uid` for pre-uid overlays; mint one deterministically from
            // the PNG file name (matching the typing loader) instead of dropping the entry.
            let uid = obj
                .get("uid")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| text_payload::stable_overlay_uid(file));
            // A v3 inline node of the same uid already won; skip the legacy entry.
            if built_inline.contains_key(&uid) {
                continue;
            }
            let Some(image) = text_dirs.iter().find_map(|dir| read_png(&dir.join(file))) else {
                ms_log::runtime_log::log_warn(format!(
                    "[layer_doc] text overlay image '{file}' not found for page {page_idx}; skipping"
                ));
                continue;
            };
            let layer_idx = obj.get("layer_idx").and_then(Value::as_u64).unwrap_or(0) as u32;
            let is_image = obj.get("overlay_type").and_then(Value::as_str) == Some("image");
            let meta = node_meta.get(&uid);
            // Prefer the persisted layers.json node name (so it round-trips, F4); fall back to a
            // generated "Текст/Картинка {n}" only when there is no node name. The counters advance
            // regardless so generated fallbacks stay unique.
            let generated = if is_image {
                image_n += 1;
                format!("Картинка {image_n}")
            } else {
                text_n += 1;
                format!("Текст {text_n}")
            };
            let name = meta
                .map(|m| m.name.clone())
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(generated);
            // Mirror the typing tab's `overlay_band_z`: pinned-text band Z, else text-group band Z by
            // layer_idx, else the past-the-top fallback.
            let z = pinned_z_by_uid
                .get(&uid)
                .copied()
                .or_else(|| group_z_by_layer_idx.get(&layer_idx).copied())
                .unwrap_or(fallback_text_z);
            let visible = meta.is_none_or(|m| m.visible);
            let opacity = meta.map_or(1.0, |m| m.opacity);
            let group_uid = meta.and_then(|m| m.group_uid.clone());
            // PS pin meta from the overlay's v2 `layers.json` node when it has one (exactly what the PS
            // editor used to read back from disk). An overlay with no node is pinned but not by a group:
            // the text writer always emits text pinned-with-explicit-Z, so that is what PS saw on disk.
            let (text_pinned, text_pinned_by_group) = meta.map_or((true, false), |m| (m.pinned, m.pinned_by_group));
            // Decode legacy geometry through the shared codec (page-relative uv → page px needs the
            // PAGE size, not the overlay PNG size). `_image` size is only used as the render image.
            let placement = text_payload::decode_overlay_placement(obj, page_size);
            let render_data = obj.get("render_data").cloned().unwrap_or(Value::Null);
            // Carry the legacy chapter's clip flag so it is not lost on migration-on-read; it then
            // persists into the inline payload on the next flush.
            let mask_clip = obj.get("mask_clip_enabled").and_then(Value::as_bool);

            texts.push(LayerNode {
                uid: uid.clone(),
                name,
                kind: NodeKind::Text,
                z,
                visible,
                opacity,
                group_uid,
                text_layer_idx: Some(layer_idx),
                text_pinned,
                text_pinned_by_group,
                transform: placement.transform,
                deform: placement.deform,
                generation: 0,
                pixels_dirty: false,
                body: NodeBody::Text {
                    render_data,
                    image,
                    is_image,
                    payload_uid: uid,
                    mask_clip,
                    // A legacy (pre-v3) overlay entry has no centering-assist state at all; the first
                    // assist-enabled render fills the centers in and the frame is created lazily.
                    extra_centers: RenderedTextExtraInfo::default(),
                    centering_frame: None,
                },
            });
        }

        // Rasters first, then texts, then a STABLE sort by z: this keeps a raster before a text at the
        // same z, matching the live render tiebreak.
        ms_log::trace_log!(
            cat::PERSIST,
            "decode_page_payload page={} text_nodes={} inline_built={} migrated={}",
            page_idx,
            texts.len(),
            built_inline.len(),
            page_is_migrated
        );
        nodes.extend(texts);

        // FULLY-MANUAL UNIFIED Z (retire auto-Y): every node — raster AND text — gets a UNIQUE explicit
        // band-Z. Legacy text-group members loaded above all share their group's band Z (a collision);
        // flatten them here, ON READ, into per-text bands in the SAME visual order the group produced —
        // ascending page-Y within the group (lower on the page = higher in the stack, matching the old
        // `overlay_stack_cmp` tiebreak). Order is PRESERVED exactly; each text just becomes individually
        // movable. The doc flush then persists each text pinned at its own Z (see `write_page_text` +
        // `write_page_text_payload`), so groups dissolve on the next save. Idempotent: once every node has
        // a distinct Z this is a stable no-op re-rank.
        nodes.sort_by(|a, b| {
            a.z.cmp(&b.z)
                // Same band Z: rasters below texts (matches the live render `(z, kind)` tiebreak)...
                .then_with(|| {
                    let rank = |n: &LayerNode| matches!(n.kind, NodeKind::Raster) as u8;
                    rank(a).cmp(&rank(b))
                })
                // ...then by text-group (`layer_idx`) so a degenerate equal-band-Z manifest with MULTIPLE
                // groups still reproduces the old `(layer_idx, page-Y)` sub-order exactly (insurance; not
                // user-reachable since distinct groups normally carry distinct band Z)...
                .then_with(|| a.text_layer_idx.cmp(&b.text_layer_idx))
                // ...and finally within a group (same Z, same layer_idx), order by page-Y (ascending →
                // lower-on-page higher in the stack, the legacy `overlay_stack_cmp` sub-order).
                .then_with(|| a.transform.cy.total_cmp(&b.transform.cy))
        });
        for (i, node) in nodes.iter_mut().enumerate() {
            node.z = i as u32;
        }

        ms_log::trace_log!(
            cat::PERSIST,
            "decode_page_payload page={} total_nodes={} (final z-order assigned)",
            page_idx,
            nodes.len()
        );
        Ok(DecodedPagePayload {
            nodes,
            groups: rasters.groups,
        })
    }

    #[must_use]
    pub fn page(&self, page_idx: usize) -> Option<&DocPage> {
        self.pages.get(&page_idx)
    }

    /// Returns whether `page_idx` has ANY persistence that is enqueued/edited but not yet
    /// acknowledged-durable — the correct "do not evict, an edit could be lost" signal.
    ///
    /// True iff `latest_epoch` holds an entry for `(page_idx, Raster | Text | Structural)`, OR the
    /// resident page still has a `pixels_dirty` node. A `latest_epoch` entry is created the moment a
    /// page is touched — by a pixel edit (`bump_dirty_epoch`) OR by ANY persisting enqueue
    /// (`next_save_epoch`, which every whole-page/text/effects save reserves, so GEOMETRY-only edits
    /// like `set_transform`/`set_deform`/visibility/opacity/group that persist via a whole-page
    /// enqueue are covered too) — and is removed only by a matching successful ack in `poll_save_acks`
    /// or by `evict_page`. The `pixels_dirty` fallback is belt-and-suspenders for the brief window
    /// between a pixel edit and its enqueue. Lock-free and cheap: only `HashMap`/slice lookups.
    #[must_use]
    pub fn page_has_pending_save(&self, page_idx: usize) -> bool {
        if [SaveKind::Raster, SaveKind::Text, SaveKind::Structural].iter().any(|kind| self.latest_epoch.contains_key(&(page_idx, *kind))) {
            return true;
        }
        self.pages
            .get(&page_idx)
            .is_some_and(|page| page.nodes.iter().any(|node| node.pixels_dirty))
    }

    /// Whether ANY page has a persisting save that is enqueued or edited but not yet acknowledged
    /// durable (a `latest_epoch` entry exists). Document-wide counterpart of
    /// [`Self::page_has_pending_save`] without its per-page `pixels_dirty` fallback; cheap (no I/O,
    /// no ack-map lock — call [`Self::poll_save_acks`] first for a fresh answer).
    #[must_use]
    pub fn has_pending_saves(&self) -> bool {
        !self.latest_epoch.is_empty()
    }

    /// Page indices currently resident (loaded) in the doc. A resident page had its text loaded by
    /// `ensure_page_loaded` (both tabs load text on page load), so the doc's view of that page's text
    /// is authoritative — including deletions. The save-to-project flush iterates these so every OWNED
    /// page's effective on-disk text (staging, else committed) equals the doc's — the saver skips a
    /// write that would not change it — and the unsaved→committed merge treats them as owned (whole-page
    /// replace); a page NOT resident was never loaded this session, so the merge preserves its committed
    /// text instead of dropping it.
    #[must_use]
    pub fn resident_pages(&self) -> Vec<usize> {
        self.pages.keys().copied().collect()
    }

    /// Drops a page from memory (e.g. when the shared page index moves away).
    pub fn evict_page(&mut self, page_idx: usize) {
        let evicted = self.pages.remove(&page_idx).is_some();
        // Drop the page's pending save epochs together with its memory: an evicted page has no in-memory
        // dirty left to settle, and a page later re-inserted must NOT be settled by a pre-eviction ack (an
        // in-flight ack whose epoch still equaled a lingering `latest_epoch` would otherwise clear the
        // reloaded page's dirty). Removing the entries also stops the map growing unbounded across a long
        // session of page visits.
        self.latest_epoch.remove(&(page_idx, SaveKind::Raster));
        self.latest_epoch.remove(&(page_idx, SaveKind::Text));
        self.latest_epoch.remove(&(page_idx, SaveKind::Structural));
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "evict_page page={} evicted={}",
            page_idx,
            evicted
        );
        if evicted {
            self.bump_page(page_idx);
        }
    }

    // Test-only read accessor; production code reads nodes via `page().nodes`. Exists only in test
    // builds so it needs no dead-code allow in the production build; `test-support` extends "test
    // builds" to the dependent crates whose tests use it (the typing tab's, in the binary).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn node(&self, page_idx: usize, uid: &str) -> Option<&LayerNode> {
        self.pages
            .get(&page_idx)?
            .nodes
            .iter()
            .find(|n| n.uid == uid)
    }

    /// Raw mutable access to a resident node. Does NOT bump: a caller that changes anything through it
    /// must call [`Self::mark_page_changed`]`(page_idx)` afterwards, or no tab re-projects the page.
    pub fn node_mut(&mut self, page_idx: usize, uid: &str) -> Option<&mut LayerNode> {
        self.pages
            .get_mut(&page_idx)?
            .nodes
            .iter_mut()
            .find(|n| n.uid == uid)
    }

    /// Clones the pre-effects base pixels of a resident raster node, if one exists at
    /// `(page_idx, uid)`. The effects worker prefers this in-memory source over the on-disk base PNG
    /// so a freshly created raster (not yet flushed to disk) can still be effected. `None` when the
    /// page is not resident, the uid is absent, or the node is not a raster.
    #[must_use]
    pub fn raster_base_image(&self, page_idx: usize, uid: &str) -> Option<ColorImage> {
        let node = self
            .pages
            .get(&page_idx)?
            .nodes
            .iter()
            .find(|n| n.uid == uid)?;
        match &node.body {
            NodeBody::Raster { base_image, .. } => Some(base_image.clone()),
            NodeBody::Text { .. } => None,
        }
    }

    /// Sets a node's affine placement. Geometry only — no generation bump (pixels are unchanged).
    pub fn set_transform(&mut self, page_idx: usize, uid: &str, transform: TransformRec) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_transform page={} uid={} cx={} cy={} rot={} scale={}",
            page_idx,
            uid,
            transform.cx,
            transform.cy,
            transform.rotation,
            transform.scale
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.transform = transform;
            self.bump_page(page_idx);
        }
    }

    /// Sets (or clears) a node's mesh-deform grid. Geometry only — no generation bump (pixels are
    /// unchanged); bumps the version so cross-tab listeners re-project. Applies to raster and text
    /// nodes alike: when `Some`, the node is positioned by the mesh and its affine transform no
    /// longer applies (mirrors the deformed-text rendering rule).
    pub fn set_deform(&mut self, page_idx: usize, uid: &str, deform: Option<DeformRec>) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_deform page={} uid={} has_deform={}",
            page_idx,
            uid,
            deform.is_some()
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.deform = deform;
            self.bump_page(page_idx);
        }
    }

    /// Sets a RASTER node's mask-clip flag and bumps its generation (so the projecting tab re-clips and
    /// re-uploads its texture). No-op if the page/node is absent or the node is not a raster. Persisted
    /// on `flush_page` via `LayerRec.mask_clip`.
    pub fn set_raster_mask_clip(&mut self, page_idx: usize, uid: &str, mask_clip: Option<bool>) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_raster_mask_clip page={} uid={} mask_clip={:?}",
            page_idx,
            uid,
            mask_clip
        );
        if let Some(node) = self.node_mut(page_idx, uid)
            && let NodeBody::Raster { mask_clip: m, .. } = &mut node.body
        {
            *m = mask_clip;
            node.bump_generation();
            self.bump_page(page_idx);
        }
    }

    /// Replaces a raster node's effects chain and post-effects display image, bumping its generation.
    /// Pure in-memory; persistence happens on `flush_page`.
    pub fn set_effects(
        &mut self,
        page_idx: usize,
        uid: &str,
        effects: Vec<Value>,
        display_image: ColorImage,
    ) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_effects page={} uid={} effects_len={} img={}x{}",
            page_idx,
            uid,
            effects.len(),
            display_image.size[0],
            display_image.size[1]
        );
        let mut changed = false;
        if let Some(node) = self.node_mut(page_idx, uid)
            && let NodeBody::Raster {
                effects: e,
                display_image: d,
                ..
            } = &mut node.body
        {
            *e = effects;
            *d = display_image;
            node.bump_generation();
            self.bump_page(page_idx);
            changed = true;
        }
        if changed {
            self.bump_dirty_epoch(page_idx, SaveKind::Raster);
        }
    }

    /// Adds a node to a resident page on top of the stack: its `z` becomes one above the current
    /// maximum (or 0 if the page is empty). Re-sorts `nodes` by `z`. Returns false if the page is not
    /// resident.
    pub fn add_node(&mut self, page_idx: usize, mut node: LayerNode) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            ms_log::trace_log!(
                cat::LAYER_MODEL,
                "add_node page={} uid={} result=false (page not resident)",
                page_idx,
                node.uid
            );
            return false;
        };
        let top_z = page.nodes.iter().map(|n| n.z).max();
        node.z = top_z.map_or(0, |z| z + 1);
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "add_node page={} uid={} kind={:?} z={} count={}",
            page_idx,
            node.uid,
            node.kind,
            node.z,
            page.nodes.len() + 1
        );
        // Capture the node's dirty kind BEFORE moving it into the page: a pushed node that carries
        // `pixels_dirty` (a fresh text overlay with no on-disk PNG, or a PS undo re-add of a raster whose
        // base PNG was pruned) must advance its `(page, kind)` dirty epoch, upholding the invariant that
        // every site making a doc node dirty bumps the epoch — so no stale-but-current-epoch ack settles it.
        let dirty_kind = node.pixels_dirty.then_some(node.kind);
        page.nodes.push(node);
        page.nodes.sort_by_key(|n| n.z);
        self.bump_page(page_idx);
        if let Some(kind) = dirty_kind {
            let save_kind = match kind {
                NodeKind::Raster => SaveKind::Raster,
                NodeKind::Text => SaveKind::Text,
            };
            self.bump_dirty_epoch(page_idx, save_kind);
        }
        true
    }

    /// Adds a node to a resident page at an EXPLICIT `z`, re-sorting `nodes` bottom-to-top. Unlike
    /// [`Self::add_node`] (which always places the node on top), this restores a node at a precise Z —
    /// used by the PS-editor undo/redo to re-insert a previously deleted layer at its exact prior
    /// position. The caller is responsible for `z` not colliding with an existing node's (removal
    /// never renumbers, so an immediate undo re-inserts into a still-free slot). Returns false if the
    /// page is not resident.
    pub fn add_node_at_z(&mut self, page_idx: usize, mut node: LayerNode, z: u32) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            ms_log::trace_log!(
                cat::LAYER_MODEL,
                "add_node_at_z page={} uid={} result=false (page not resident)",
                page_idx,
                node.uid
            );
            return false;
        };
        node.z = z;
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "add_node_at_z page={} uid={} kind={:?} z={} count={}",
            page_idx,
            node.uid,
            node.kind,
            node.z,
            page.nodes.len() + 1
        );
        // Capture the node's dirty kind BEFORE moving it in (see `add_node`): a re-inserted dirty node
        // must advance its `(page, kind)` dirty epoch so a stale-but-current-epoch ack cannot settle it.
        let dirty_kind = node.pixels_dirty.then_some(node.kind);
        page.nodes.push(node);
        page.nodes.sort_by_key(|n| n.z);
        self.bump_page(page_idx);
        if let Some(kind) = dirty_kind {
            let save_kind = match kind {
                NodeKind::Raster => SaveKind::Raster,
                NodeKind::Text => SaveKind::Text,
            };
            self.bump_dirty_epoch(page_idx, save_kind);
        }
        true
    }

    /// Removes the node with `uid` from a resident page. Returns whether a node was removed.
    pub fn remove_node(&mut self, page_idx: usize, uid: &str) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return false;
        };
        let before = page.nodes.len();
        page.nodes.retain(|n| n.uid != uid);
        let removed = page.nodes.len() != before;
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "remove_node page={} uid={} removed={} count={}",
            page_idx,
            uid,
            removed,
            page.nodes.len()
        );
        if removed {
            self.bump_page(page_idx);
        }
        removed
    }

    /// Sets a node's visibility. No-op if the page or node is absent.
    pub fn set_visibility(&mut self, page_idx: usize, uid: &str, visible: bool) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_visibility page={} uid={} visible={}",
            page_idx,
            uid,
            visible
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.visible = visible;
            self.bump_page(page_idx);
        }
    }

    /// Sets a node's opacity, clamped to `0.0..=1.0`. No-op if the page or node is absent.
    pub fn set_opacity(&mut self, page_idx: usize, uid: &str, opacity: f32) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_opacity page={} uid={} opacity={}",
            page_idx,
            uid,
            opacity.clamp(0.0, 1.0)
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.opacity = opacity.clamp(0.0, 1.0);
            self.bump_page(page_idx);
        }
    }

    /// Sets a node's group membership (`None` to ungroup). No-op if the page or node is absent.
    pub fn set_group(&mut self, page_idx: usize, uid: &str, group_uid: Option<String>) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_group page={} uid={} group_uid={:?}",
            page_idx,
            uid,
            group_uid
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.group_uid = group_uid;
            self.bump_page(page_idx);
        }
    }

    /// Sets a TEXT node's PS pin meta (`text_pinned` / `text_pinned_by_group`), bumping the version
    /// when either value changed. The PS editor calls this alongside each band-order / grouping /
    /// pin edit so the doc mirrors what the enqueued structural edit writes to disk. Persistence is
    /// NOT triggered here (the structural job writes the on-disk flags; a later text flush writes the
    /// doc's `text_pinned_by_group`). Returns `false` when the page, the node, or a TEXT node with that
    /// uid is absent (nothing changed).
    pub fn set_text_pin_meta(&mut self, page_idx: usize, uid: &str, pinned: bool, pinned_by_group: bool) -> bool {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_text_pin_meta page={} uid={} pinned={} pinned_by_group={}",
            page_idx,
            uid,
            pinned,
            pinned_by_group
        );
        let Some(node) = self.node_mut(page_idx, uid).filter(|node| matches!(node.kind, NodeKind::Text)) else {
            return false;
        };
        let changed = node.text_pinned != pinned || node.text_pinned_by_group != pinned_by_group;
        node.text_pinned = pinned;
        node.text_pinned_by_group = pinned_by_group;
        if changed {
            self.bump_page(page_idx);
        }
        true
    }

    /// Adds a `GroupMeta` to a resident page, ignoring it if a group with the same uid already exists.
    /// No-op if the page is absent.
    pub fn add_group(&mut self, page_idx: usize, group: GroupMeta) {
        if let Some(page) = self.pages.get_mut(&page_idx) {
            if page.groups.iter().any(|g| g.uid == group.uid) {
                ms_log::trace_log!(
                    cat::LAYER_MODEL,
                    "add_group page={} uid={} result=skip (exists)",
                    page_idx,
                    group.uid
                );
                return;
            }
            ms_log::trace_log!(
                cat::LAYER_MODEL,
                "add_group page={} uid={} groups={}",
                page_idx,
                group.uid,
                page.groups.len() + 1
            );
            page.groups.push(group);
            self.bump_page(page_idx);
        }
    }

    /// Sets the composite meta (`visible`, `opacity` clamped to `0.0..=1.0`) of the PS unified group
    /// `group_uid` on a resident page, bumping the version when either value changed. This is the
    /// group fold every composite consumer reads (typing canvas, typing export, PS composite), so a
    /// group toggle made in one tab reaches the others through the page revision. Persistence is NOT
    /// triggered here: the page's groups are written by the next doc page save (`flush_page` /
    /// `enqueue_page_save` pass `DocPage::groups` to `save_page_rasters`), in the unchanged manifest
    /// format. The panel-only `collapsed` flag is untouched. Returns `false` when the page or the
    /// group is absent, or when `opacity` is not finite (nothing changed).
    pub fn set_group_meta(&mut self, page_idx: usize, group_uid: &str, visible: bool, opacity: f32) -> bool {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_group_meta page={} uid={} visible={} opacity={}",
            page_idx,
            group_uid,
            visible,
            opacity
        );
        // `f32::clamp` would pass a NaN through into the persisted manifest; refuse it instead.
        if !opacity.is_finite() {
            return false;
        }
        let opacity = opacity.clamp(0.0, 1.0);
        let Some(group) = self
            .pages
            .get_mut(&page_idx)
            .and_then(|page| page.groups.iter_mut().find(|g| g.uid == group_uid))
        else {
            return false;
        };
        let changed = group.visible != visible || group.opacity.to_bits() != opacity.to_bits();
        group.visible = visible;
        group.opacity = opacity;
        if changed {
            self.bump_page(page_idx);
        }
        true
    }

    /// Sets the panel-only `collapsed` flag of the PS unified group `group_uid` on a resident page,
    /// bumping the version when it changed (the PS editor re-projects its stack groups from the doc,
    /// which is the single in-session owner of the flag). Like [`Self::set_group_meta`] it is persisted
    /// by the page's next doc save (`save_page_rasters`' `groups`). It does not affect compositing.
    /// Returns `false` when the page or the group is absent.
    pub fn set_group_collapsed(&mut self, page_idx: usize, group_uid: &str, collapsed: bool) -> bool {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_group_collapsed page={} uid={} collapsed={}",
            page_idx,
            group_uid,
            collapsed
        );
        let Some(group) = self
            .pages
            .get_mut(&page_idx)
            .and_then(|page| page.groups.iter_mut().find(|g| g.uid == group_uid))
        else {
            return false;
        };
        let changed = group.collapsed != collapsed;
        group.collapsed = collapsed;
        if changed {
            self.bump_page(page_idx);
        }
        true
    }

    /// Removes the `GroupMeta` with `group_uid` from a resident page and clears `group_uid` on every
    /// member node (ungroups them). No-op if the page is absent.
    pub fn remove_group(&mut self, page_idx: usize, group_uid: &str) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "remove_group page={} uid={}",
            page_idx,
            group_uid
        );
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return;
        };
        page.groups.retain(|g| g.uid != group_uid);
        for node in &mut page.nodes {
            if node.group_uid.as_deref() == Some(group_uid) {
                node.group_uid = None;
            }
        }
        self.bump_page(page_idx);
    }

    /// Replaces a RASTER node's base image, display image, and effects chain, sets `pixels_dirty`, and
    /// bumps its generation. No-op if the page/node is absent or the node is not a raster.
    pub fn set_raster_pixels(
        &mut self,
        page_idx: usize,
        uid: &str,
        base_image: ColorImage,
        display_image: ColorImage,
        effects: Vec<Value>,
        pixels_dirty: bool,
    ) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_raster_pixels page={} uid={} base={}x{} display={}x{} effects_len={} dirty={}",
            page_idx,
            uid,
            base_image.size[0],
            base_image.size[1],
            display_image.size[0],
            display_image.size[1],
            effects.len(),
            pixels_dirty
        );
        let mut changed = false;
        if let Some(node) = self.node_mut(page_idx, uid)
            && let NodeBody::Raster {
                base_image: b,
                display_image: d,
                effects: e,
                ..
            } = &mut node.body
        {
            *b = base_image;
            *d = display_image;
            *e = effects;
            node.pixels_dirty = pixels_dirty;
            node.bump_generation();
            self.bump_page(page_idx);
            changed = pixels_dirty;
        }
        if changed {
            self.bump_dirty_epoch(page_idx, SaveKind::Raster);
        }
    }

    /// Replaces a TEXT node's render params, rendered image and centering-assist centers, sets
    /// `pixels_dirty` (so the next flush re-encodes the rendered PNG — mirrors `set_raster_pixels`),
    /// and bumps its generation. No-op if the page/node is absent or the node is not text.
    /// Persistence happens on `flush_page`; since schema v4 `extra_centers` IS written (into
    /// `LayerRec.text_centers`), alongside the rendered PNG it describes.
    ///
    /// `extra_centers` MUST be the centers the renderer measured for THIS `image` (all-`None` when
    /// they were not requested). They are stored in the same mutation as the pixels so a later
    /// projection — or a later flush — can never pair one render's centers with another render's
    /// pixels. That invariant is what makes persisting them safe: the stored value always describes
    /// the stored PNG. The typing tab keeps them fresh by requesting the centers on every render of a
    /// layer that already has them (the "sticky centers" rule), so an assist-off edit does not erase
    /// a previously measured center.
    pub fn set_text_render(
        &mut self,
        page_idx: usize,
        uid: &str,
        render_data: Value,
        image: ColorImage,
        extra_centers: RenderedTextExtraInfo,
    ) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_text_render page={} uid={} img={}x{}",
            page_idx,
            uid,
            image.size[0],
            image.size[1]
        );
        let mut changed = false;
        if let Some(node) = self.node_mut(page_idx, uid)
            && let NodeBody::Text {
                render_data: r,
                image: i,
                extra_centers: c,
                ..
            } = &mut node.body
        {
            *r = render_data;
            *i = image;
            *c = extra_centers;
            node.pixels_dirty = true;
            node.bump_generation();
            self.bump_page(page_idx);
            changed = true;
        }
        if changed {
            self.bump_dirty_epoch(page_idx, SaveKind::Text);
        }
    }

    /// Moves the node one step in `z` among the page's nodes by swapping its `z` with the neighbor in
    /// the current bottom-to-top order (`up == true` moves toward the top). Keeps `nodes` sorted by
    /// `z`. Pure in-memory. Returns true if the order changed.
    pub fn reorder_node_one(&mut self, page_idx: usize, uid: &str, up: bool) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return false;
        };
        // `nodes` is maintained sorted by z, so positions are the bottom-to-top order.
        let Some(pos) = page.nodes.iter().position(|n| n.uid == uid) else {
            return false;
        };
        let other = if up {
            if pos + 1 >= page.nodes.len() {
                return false;
            }
            pos + 1
        } else {
            if pos == 0 {
                return false;
            }
            pos - 1
        };
        let z_a = page.nodes[pos].z;
        let z_b = page.nodes[other].z;
        page.nodes[pos].z = z_b;
        page.nodes[other].z = z_a;
        page.nodes.sort_by_key(|n| n.z);
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "reorder_node_one page={} uid={} dir={} z={}->{} result=true",
            page_idx,
            uid,
            if up { "up" } else { "down" },
            z_a,
            z_b
        );
        self.bump_page(page_idx);
        true
    }

    /// Moves an entire group's block of nodes one step in `z`, swapping it past the neighbouring block
    /// (a group run or a single ungrouped node) while preserving the group's intra-order. Mirrors the
    /// PS panel's group-block move. `up == true` moves the block toward the top.
    ///
    /// The page's `nodes` are kept sorted by `z`; group members may be non-contiguous in `z` only if
    /// the disk band order was, but the PS editor anchors a group's bands together, so in practice a
    /// group occupies a contiguous run. This walks the current bottom-to-top order, finds the group's
    /// run, swaps it with the adjacent block, then reassigns contiguous `z` by the new position.
    /// Returns true if the order changed.
    pub fn reorder_group_block(&mut self, page_idx: usize, group_uid: &str, up: bool) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return false;
        };
        // `nodes` is maintained sorted by z → positions are the bottom-to-top order. Segment the order
        // into maximal blocks: a run of consecutive nodes sharing the same (non-None) group_uid, or a
        // single ungrouped/other-group node.
        let n = page.nodes.len();
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for i in 0..n {
            let gi = page.nodes[i].group_uid.clone();
            if let Some((_, hi)) = blocks.last_mut()
                && gi.is_some()
                && page.nodes[*hi].group_uid == gi
            {
                *hi = i;
            } else {
                blocks.push((i, i));
            }
        }
        // Find the target group's block.
        let Some(bi) = blocks
            .iter()
            .position(|(lo, _)| page.nodes[*lo].group_uid.as_deref() == Some(group_uid))
        else {
            return false;
        };
        let target = if up { bi + 1 } else { bi.wrapping_sub(1) };
        if target >= blocks.len() {
            return false;
        }
        // Rebuild the node order with the two blocks swapped, then reassign contiguous z.
        let mut order_blocks: Vec<std::ops::RangeInclusive<usize>> =
            blocks.iter().map(|(lo, hi)| *lo..=*hi).collect();
        order_blocks.swap(bi, target);
        let new_index: Vec<usize> = order_blocks.into_iter().flatten().collect();
        // `new_index[new_pos] = old_pos`; reorder the Vec accordingly.
        let mut reordered: Vec<LayerNode> = Vec::with_capacity(n);
        // Drain in the new order by swapping out via indices: build by taking owned nodes.
        let mut slots: Vec<Option<LayerNode>> = page.nodes.drain(..).map(Some).collect();
        for (new_z, &old_pos) in new_index.iter().enumerate() {
            let mut node = slots[old_pos].take().expect("each old slot taken once");
            node.z = new_z as u32;
            reordered.push(node);
        }
        page.nodes = reordered;
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "reorder_group_block page={} group_uid={} dir={} result=true",
            page_idx,
            group_uid,
            if up { "up" } else { "down" }
        );
        self.bump_page(page_idx);
        true
    }

    /// Reassigns every node's `z` to its position in `order` (a bottom-to-top list of node uids), then
    /// re-sorts. Nodes whose uid is absent from `order` keep their relative order after the listed
    /// ones (appended in their prior order). The PS structure ops compute the authoritative band order
    /// and persist it to disk; this applies that SAME order to the in-memory doc so cross-tab listeners
    /// re-project without a disk round-trip. No-op if the page is absent.
    pub fn set_z_order(&mut self, page_idx: usize, order: &[String]) {
        ms_log::trace_log!(
            cat::LAYER_MODEL,
            "set_z_order page={} order_len={}",
            page_idx,
            order.len()
        );
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return;
        };
        let rank: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, u)| (u.as_str(), i))
            .collect();
        // Listed nodes get their explicit rank; unlisted nodes sort after, keeping their prior z order.
        let unlisted_base = order.len();
        let mut next_unlisted = unlisted_base;
        // Assign z: listed by rank, unlisted by a running counter seeded from their current z order.
        // First snapshot current bottom-to-top order for stable unlisted placement.
        let prior: Vec<String> = {
            let mut v: Vec<&LayerNode> = page.nodes.iter().collect();
            v.sort_by_key(|n| n.z);
            v.into_iter().map(|n| n.uid.clone()).collect()
        };
        let mut unlisted_rank: HashMap<&str, usize> = HashMap::new();
        for uid in &prior {
            if !rank.contains_key(uid.as_str()) {
                unlisted_rank.insert(uid.as_str(), next_unlisted);
                next_unlisted += 1;
            }
        }
        for node in &mut page.nodes {
            node.z = rank
                .get(node.uid.as_str())
                .copied()
                .or_else(|| unlisted_rank.get(node.uid.as_str()).copied())
                .unwrap_or(unlisted_base) as u32;
        }
        page.nodes.sort_by_key(|n| n.z);
        self.bump_page(page_idx);
    }

    /// Persists a page's RASTER nodes back to disk via `persist::ManifestTxn::save_page_rasters` seeded
    /// from `fallback_dir` (passing each node's base image + `pixels_dirty` and the page's groups,
    /// bottom-to-top by current `z`; an emptied page stays PRESENT-but-EMPTY in staging), then
    /// re-writes the effects chain + rendered PNG for any raster node with a non-empty chain via
    /// `persist::update_raster_effects` (the doc is authoritative over the rasters it holds). After a
    /// successful flush, every raster node's `pixels_dirty` is cleared.
    pub fn flush_page(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        self.flush_page_inner(page_idx, layers_dir, fallback_dir, &[])
    }

    /// Like [`flush_page`], but also DROPS `removed_raster_uids` from the manifest (and prunes their
    /// PNGs). Without this, `save_page_rasters` PRESERVES a manifest raster not in the flushed set as
    /// "owned by another tab" — so a raster the typing tab deleted would resurrect on disk. The deleter
    /// passes the removed uid here. (The PS editor uses `record_raster_deletion` for the same effect.)
    pub fn flush_page_dropping_raster(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        removed_uid: &str,
    ) -> Result<(), String> {
        self.flush_page_inner(
            page_idx,
            layers_dir,
            fallback_dir,
            &[removed_uid.to_string()],
        )
    }

    fn flush_page_inner(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        removed_raster_uids: &[String],
    ) -> Result<(), String> {
        let _span = ms_log::trace_scope!(
            cat::PERSIST,
            "flush_page_inner page={} dir={} removed={}",
            page_idx,
            layers_dir.display(),
            removed_raster_uids.len()
        );
        let Some(page) = self.pages.get_mut(&page_idx) else {
            ms_log::trace_log!(
                cat::PERSIST,
                "flush_page_inner page={} skipped (not resident)",
                page_idx
            );
            return Ok(());
        };

        // Pass rasters bottom-to-top by current z (nodes are kept sorted, but be explicit).
        let mut raster_indices: Vec<usize> = page
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_raster())
            .map(|(i, _)| i)
            .collect();
        raster_indices.sort_by_key(|&i| page.nodes[i].z);

        let mut outs: Vec<RasterLayerOut> = Vec::with_capacity(raster_indices.len());
        for &i in &raster_indices {
            let node = &page.nodes[i];
            let NodeBody::Raster {
                base_image,
                mask_clip,
                ..
            } = &node.body
            else {
                continue;
            };
            outs.push(RasterLayerOut {
                uid: node.uid.clone(),
                name: node.name.clone(),
                visible: node.visible,
                opacity: node.opacity,
                transform: node.transform,
                deform: node.deform.clone(),
                group_uid: node.group_uid.clone(),
                // The synchronous flush always carries the pixels (borrowed, so free), which keeps a
                // clean raster whose PNG went missing writable without a retry.
                image: Some(base_image),
                image_size: base_image.size,
                pixels_dirty: node.pixels_dirty,
                mask_clip: *mask_clip,
            });
        }

        ms_log::trace_log!(
            cat::PERSIST,
            "flush_page_inner page={} rasters={} groups={}",
            page_idx,
            outs.len(),
            page.groups.len()
        );
        // Through a transaction with the committed `fallback_dir`, so a page staged nowhere yet is seeded
        // first and an emptied page is recorded PRESENT-but-EMPTY (an explicit override of committed)
        // instead of being dropped back to "absent == committed", which resurrected deleted rasters.
        let mut txn = persist::ManifestTxn::begin(layers_dir)?;
        txn.save_page_rasters(page_idx, &outs, &page.groups, removed_raster_uids, fallback_dir)?;
        txn.commit()?;

        // Text flush (schema v3): write every TEXT node's inline payload (see `write_page_text`).
        Self::write_page_text(page, page_idx, layers_dir, fallback_dir)?;

        // Re-write the doc's effects chains so the on-disk chain + rendered PNG match the doc, which
        // is authoritative. A non-dirty `save_page_rasters` preserves the on-disk chain, but the doc
        // may have changed effects in memory, so always reconcile here.
        for &i in &raster_indices {
            let node = &page.nodes[i];
            if let NodeBody::Raster {
                effects,
                display_image,
                ..
            } = &node.body
                && !effects.is_empty()
            {
                persist::update_raster_effects(
                    layers_dir,
                    page_idx,
                    &node.uid,
                    effects,
                    Some(display_image),
                    fallback_dir,
                )?;
            }
        }

        for &i in &raster_indices {
            page.nodes[i].pixels_dirty = false;
        }
        Ok(())
    }

    /// Persists ONLY a page's TEXT nodes (their inline v3 payload + rendered PNGs), leaving raster
    /// nodes untouched on disk. The typing tab calls this on every text edit/placement save so the doc
    /// is the sole text writer without re-saving rasters each time; `flush_page` calls the same writer
    /// for whole-page (save-to-project) flushes. No-op if the page is not resident.
    pub fn flush_page_text(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        let _span = ms_log::trace_scope!(
            cat::PERSIST,
            "flush_page_text page={} dir={}",
            page_idx,
            layers_dir.display()
        );
        let Some(page) = self.pages.get_mut(&page_idx) else {
            ms_log::trace_log!(
                cat::PERSIST,
                "flush_page_text page={} skipped (not resident)",
                page_idx
            );
            return Ok(());
        };
        Self::write_page_text(page, page_idx, layers_dir, fallback_dir)
    }

    /// Writes every TEXT node of `page` into `layers.json` with its FULL inline payload (render_data +
    /// geometry + rendered PNG name + mask_clip) via `persist::write_page_text_payload`, which
    /// preserves raster nodes (kind-filter) and rebuilds the text-group bands. The rendered text PNG is
    /// (re)written only when the in-memory image is dirty (a
    /// text re-render this session) OR the deterministic file is not yet on disk — an unchanged text
    /// PNG is never re-encoded (mirrors the raster `pixels_dirty` rule). After a successful write every
    /// text node's `pixels_dirty` is cleared. Nodes are emitted bottom-to-top by `z`.
    fn write_page_text(
        page: &mut DocPage,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        let _span = ms_log::trace_scope!(cat::PERSIST, "write_page_text page={}", page_idx);
        let mut text_indices: Vec<usize> = page
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.kind, NodeKind::Text))
            .map(|(i, _)| i)
            .collect();
        text_indices.sort_by_key(|&i| page.nodes[i].z);

        let mut pngs_written = 0usize;
        let mut text_outs: Vec<persist::TextPayloadOut> = Vec::with_capacity(text_indices.len());
        for &i in &text_indices {
            let node = &page.nodes[i];
            let NodeBody::Text {
                render_data,
                image,
                is_image,
                payload_uid,
                mask_clip,
                // Schema v4: both centering-assist values ARE part of the on-disk text payload.
                extra_centers,
                centering_frame,
            } = &node.body
            else {
                continue;
            };
            // Determine the rendered PNG: rewrite when dirty or when the deterministic file is missing;
            // otherwise reuse the existing on-disk file without re-encoding.
            let file_name = persist::text_image_file_name(page_idx, &node.uid);
            let present = layers_dir.join(&file_name).is_file()
                || fallback_dir.is_some_and(|d| d.join(&file_name).is_file());
            let rendered_file = if node.pixels_dirty || !present {
                pngs_written += 1;
                ms_log::trace_log!(
                    cat::PERSIST,
                    "write_page_text page={} uid={} rewriting PNG (dirty={} present={})",
                    page_idx,
                    node.uid,
                    node.pixels_dirty,
                    present
                );
                Some(persist::write_text_image(
                    layers_dir, page_idx, &node.uid, image,
                )?)
            } else {
                Some(file_name)
            };
            text_outs.push(persist::TextPayloadOut {
                uid: node.uid.clone(),
                name: node.name.clone(),
                z: node.z,
                // The text-group axis (Группа текста N): the node's carried `text_layer_idx` (set by the
                // typing tab on create / projected from disk on load) is authoritative. Falls back to
                // group 0 when unknown.
                layer_idx: node.text_layer_idx.unwrap_or(0),
                pinned: false,
                visible: node.visible,
                opacity: node.opacity,
                group_uid: node.group_uid.clone(),
                // Doc-owned (mirrored from every PS grouping edit), so the node's value is written.
                pinned_by_group: node.text_pinned_by_group,
                payload_uid: payload_uid.clone(),
                render_data: render_data.clone(),
                is_image: *is_image,
                transform: node.transform,
                deform: node.deform.clone(),
                rendered_file,
                mask_clip: *mask_clip,
                // An all-`None` measurement is normalized to `None` so the JSON key is omitted.
                text_centers: text_centers_rec_from(extra_centers),
                centering_frame: *centering_frame,
            });
        }
        ms_log::trace_log!(
            cat::PERSIST,
            "write_page_text page={} text_nodes={} pngs_written={}",
            page_idx,
            text_outs.len(),
            pngs_written
        );
        persist::write_page_text_payload(layers_dir, fallback_dir, page_idx, &text_outs)?;
        for &i in &text_indices {
            page.nodes[i].pixels_dirty = false;
        }
        Ok(())
    }

    // Async-save API (`enable_background_saver` … `clear_page_text_dirty`): wired into the GUI tabs
    // (PS per-edit/raster flushes, typing text flushes) and the save-to-project / app-close barriers.
    /// Enables the background (coalescing) saver on this doc, if not already enabled. After this,
    /// `enqueue_page_save` / `enqueue_page_text_save` route persistence off-thread. Idempotent:
    /// a second call is a no-op (the existing saver + its worker thread are kept).
    ///
    /// `gate` is the project's autosave gate: with `Some`, the saver HOLDS jobs until the gate is due
    /// (barriers and shutdown still force every held job out); `None` writes after every drain
    /// (immediate mode). Every async funnel below reports one action to it per call.
    pub fn enable_background_saver(&mut self, gate: Option<Arc<AutosaveGate>>) {
        if self.saver.is_none() {
            let saver = LayerSaver::new(gate);
            self.save_acks = Some(saver.ack_map());
            self.saver = Some(saver);
            ms_log::trace_log!(cat::PERSIST, "enable_background_saver: saver enabled");
        }
    }

    /// A cheap-clone handle to the background saver, if one is enabled, so a merge worker can enqueue
    /// jobs / run a barrier without locking the doc. `None` when no saver is enabled.
    #[must_use]
    pub fn saver_handle(&self) -> Option<LayerSaverHandle> {
        self.saver.as_ref().map(LayerSaver::handle)
    }

    /// Shuts the background saver down (sentinel + join), if one is enabled, flushing its queue first.
    /// Called by `Drop`; safe to call explicitly. A no-op when no saver is enabled.
    pub fn shutdown_saver(&mut self) {
        if let Some(saver) = self.saver.take() {
            saver.shutdown();
        }
        self.save_acks = None;
    }

    /// Shuts the background saver down WITHOUT writing: jobs still queued or held by the autosave gate
    /// are dropped, then the worker is joined (a write already in progress completes first). For the
    /// discard path only, which deletes the staging dir those jobs target. A no-op without a saver.
    pub fn shutdown_saver_discarding(&mut self) {
        if let Some(saver) = self.saver.take() {
            saver.shutdown_discarding();
        }
        self.save_acks = None;
    }

    /// Reports one save gesture to the saver's autosave gate. A no-op without a saver or in immediate
    /// mode. The async funnels call it themselves; a caller that enqueues through a raw
    /// [`LayerSaverHandle`] (the PS editor's page save) calls it once per enqueue, AFTER sending (see
    /// `LayerSaver::note_action` for why the order matters).
    /// Deliberately NOT called from dirty-marking (`bump_dirty_epoch`): pixel marks can fire per frame,
    /// the enqueue is the gesture.
    pub fn note_autosave_action(&self) {
        if let Some(saver) = &self.saver {
            saver.note_action();
        }
    }

    /// Reserves the current page-kind generation for a background job and returns its epoch.
    #[must_use]
    pub fn next_save_epoch(&mut self, page_idx: usize, kind: SaveKind) -> u64 {
        self.save_epoch = self.save_epoch.wrapping_add(1);
        self.latest_epoch.insert((page_idx, kind), self.save_epoch);
        self.save_epoch
    }

    /// Advances a dirty page-kind generation so older in-flight acknowledgements cannot settle it.
    fn bump_dirty_epoch(&mut self, page_idx: usize, kind: SaveKind) {
        let _epoch = self.next_save_epoch(page_idx, kind);
    }

    /// Applies completed worker acknowledgements without doing persistence or holding the ack lock
    /// while mutating resident document pages.
    pub fn poll_save_acks(&mut self) {
        let completions = self.save_acks.as_ref().and_then(|acks| match acks.lock() {
            Ok(mut ack) => Some(ack.take()),
            Err(_) => {
                ms_log::runtime_log::log_error("[layer_model] save acknowledgement map lock poisoned");
                None
            }
        }).unwrap_or_default();
        for (page_idx, kind, ack_epoch, ok) in completions {
            if self.latest_epoch.get(&(page_idx, kind)) != Some(&ack_epoch) {
                continue;
            }
            // A page that is not resident has no dirty flags to settle, but a current SUCCESSFUL ack
            // still retires its epoch — otherwise a disk-targeted save for a never-loaded page (a
            // structural edit, an effects-only update) would read as pending forever.
            if self.pages.contains_key(&page_idx) {
                match (kind, ok) {
                    (SaveKind::Raster, true) => self.clear_page_dirty(page_idx),
                    (SaveKind::Text, true) => self.clear_page_text_dirty(page_idx),
                    // A failed raster write may have been the "pixels not carried" case (a clean raster
                    // whose PNG the manifest no longer records), so the retry must carry every raster's
                    // pixels. The epoch stays pending, so the page still reports an unsaved change.
                    (SaveKind::Raster, false) => self.mark_page_rasters_dirty(page_idx),
                    // Structural edits carry no dirty flags; a failure stays pending (an unsaved change).
                    (SaveKind::Text | SaveKind::Structural, false) | (SaveKind::Structural, true) => {}
                }
            }
            if ok {
                self.latest_epoch.remove(&(page_idx, kind));
            }
        }
    }

    /// ASYNC whole-page save: when the background saver is enabled, builds the page's owned save job
    /// and enqueues it (coalescing, off-thread); otherwise falls back to the synchronous [`flush_page`]
    /// so behavior is safe either way. No-op (returns `Ok`) if the page is not resident.
    ///
    /// Mirrors `flush_page`'s contract: callers that need the bytes ON DISK before reading them must
    /// `barrier_blocking` the saver handle (the enqueue is fire-and-forget).
    ///
    /// # Errors
    /// Only the synchronous fallback path can return an error (the async path enqueues and returns
    /// `Ok`); it matches [`flush_page`]'s error propagation.
    pub fn enqueue_page_save(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        if self.saver.is_some() {
            if let Some(mut job) = self.build_page_save_job(page_idx, layers_dir, fallback_dir, &[]) {
                job.raster_epoch = Some(self.next_save_epoch(page_idx, SaveKind::Raster));
                job.text_epoch = Some(self.next_save_epoch(page_idx, SaveKind::Text));
                if let Some(saver) = &self.saver {
                    saver.enqueue(job);
                    saver.note_action();
                }
            }
            Ok(())
        } else {
            self.flush_page(page_idx, layers_dir, fallback_dir)
        }
    }

    /// ASYNC text-only save: when the background saver is enabled, builds a TEXT-ONLY owned save job
    /// and enqueues it; otherwise falls back to the synchronous [`flush_page_text`]. No-op (returns
    /// `Ok`) if the page is not resident.
    ///
    /// # Errors
    /// Only the synchronous fallback can error; the async path enqueues and returns `Ok`.
    pub fn enqueue_page_text_save(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        if self.saver.is_some() {
            if let Some(text) = self.build_text_save_part(page_idx) {
                let text_epoch = self.next_save_epoch(page_idx, SaveKind::Text);
                let job = PageSaveJob {
                    page_idx,
                    layers_dir: layers_dir.to_path_buf(),
                    fallback_dir: fallback_dir.map(Path::to_path_buf),
                    raster: None,
                    raster_epoch: None,
                    text: Some(text),
                    text_epoch: Some(text_epoch),
                    effects: Vec::new(),
                    structural: Vec::new(),
                    structural_epoch: None,
                };
                if let Some(saver) = &self.saver {
                    saver.enqueue(job);
                    saver.note_action();
                }
            }
            Ok(())
        } else {
            self.flush_page_text(page_idx, layers_dir, fallback_dir)
        }
    }

    /// ASYNC text-only save of SEVERAL pages as one saver batch (`LayerSaverHandle::enqueue_batch`):
    /// every job is built first and sent as ONE message, so the worker writes them in one drain pass
    /// — one manifest commit — instead of possibly one pass per page. Without a saver, each page falls
    /// back to the synchronous [`flush_page_text`]. Non-resident pages are skipped (reported `Ok`,
    /// matching [`enqueue_page_text_save`]).
    ///
    /// Returns one result per entry of `pages`, in order: with a saver every page is `Ok` (the write
    /// outcome arrives through the acknowledgements and the barrier); on the synchronous fallback, a
    /// page's own flush error.
    pub fn enqueue_pages_text_save(
        &mut self,
        pages: &[usize],
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Vec<(usize, Result<(), String>)> {
        if self.saver.is_none() {
            return pages.iter().map(|&page_idx| (page_idx, self.flush_page_text(page_idx, layers_dir, fallback_dir))).collect();
        }
        let mut jobs = Vec::with_capacity(pages.len());
        for &page_idx in pages {
            let Some(text) = self.build_text_save_part(page_idx) else {
                continue;
            };
            let text_epoch = self.next_save_epoch(page_idx, SaveKind::Text);
            jobs.push(PageSaveJob {
                page_idx,
                layers_dir: layers_dir.to_path_buf(),
                fallback_dir: fallback_dir.map(Path::to_path_buf),
                raster: None,
                raster_epoch: None,
                text: Some(text),
                text_epoch: Some(text_epoch),
                effects: Vec::new(),
                structural: Vec::new(),
                structural_epoch: None,
            });
        }
        if let Some(saver) = &self.saver
            && !jobs.is_empty()
        {
            // The whole batch is one gesture (e.g. a tab-leave text flush).
            saver.enqueue_batch(jobs);
            saver.note_action();
        }
        pages.iter().map(|&page_idx| (page_idx, Ok(()))).collect()
    }

    /// ASYNC targeted effects update for a SINGLE raster: when the background saver is enabled,
    /// enqueues an effects-only job that reconciles only this raster's chain + rendered PNG (never a
    /// whole-page raster rewrite); otherwise falls back to the synchronous `persist::update_raster_effects`
    /// so behavior is safe either way. `display` is the post-effects render (`None` ⇒ CLEAR the chain
    /// and delete any rendered PNG). The bytes written are identical to the direct sync call; only the
    /// PNG encode moves off the GUI thread.
    ///
    /// Unlike a whole-page save this does NOT depend on the page being resident in the doc — it targets
    /// the on-disk node by `uid` (mirroring the callers' existing direct `update_raster_effects`).
    ///
    /// # Errors
    /// Only the synchronous fallback can error; the async path enqueues and returns `Ok`.
    pub fn enqueue_raster_effects(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        uid: &str,
        effects: &[serde_json::Value],
        display: Option<&ColorImage>,
    ) -> Result<(), String> {
        if self.saver.is_some() {
            let raster_epoch = self.next_save_epoch(page_idx, SaveKind::Raster);
            let Some(saver) = &self.saver else {
                return Ok(());
            };
                        saver.enqueue(PageSaveJob {
                page_idx,
                layers_dir: layers_dir.to_path_buf(),
                fallback_dir: fallback_dir.map(Path::to_path_buf),
                raster: None,
                raster_epoch: Some(raster_epoch),
                text: None,
                text_epoch: None,
                effects: vec![EffectsSaveItem {
                    uid: uid.to_string(),
                    effects: effects.to_vec(),
                    display_image: display.cloned(),
                }],
                structural: Vec::new(),
                structural_epoch: None,
            });
            saver.note_action();
            Ok(())
        } else {
            persist::update_raster_effects(
                layers_dir,
                page_idx,
                uid,
                effects,
                display,
                fallback_dir,
            )
        }
    }

    /// ASYNC whole-page save that also DROPS `removed_uid` from the manifest — the queued counterpart
    /// of [`Self::flush_page_dropping_raster`]. The job's `removed_uids` survive coalescing with later
    /// jobs for the page (`saver::merge_raster_parts` unions them), so the deletion is enqueued eagerly
    /// and becomes durable at the saver's next pass / barrier. Without a saver, falls back to the
    /// synchronous `flush_page_dropping_raster`. No-op (`Ok`) if the page is not resident.
    ///
    /// # Errors
    /// Only the synchronous fallback can error; the async path enqueues and returns `Ok`.
    pub fn enqueue_page_save_dropping_raster(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        removed_uid: &str,
    ) -> Result<(), String> {
        if self.saver.is_none() {
            return self.flush_page_dropping_raster(page_idx, layers_dir, fallback_dir, removed_uid);
        }
        let Some(mut job) = self.build_page_save_job(page_idx, layers_dir, fallback_dir, &[removed_uid.to_string()]) else {
            return Ok(());
        };
        job.raster_epoch = Some(self.next_save_epoch(page_idx, SaveKind::Raster));
        job.text_epoch = Some(self.next_save_epoch(page_idx, SaveKind::Text));
        if let Some(saver) = &self.saver {
            saver.enqueue(job);
            saver.note_action();
        }
        Ok(())
    }

    /// ASYNC unified band reorder of a page (`order` bottom-to-top): enqueues a structural-only job
    /// that the saver applies AFTER every earlier-enqueued raster / text / effects part of the page
    /// (same pass or an earlier one), so a raster added just before has its node when the order lands.
    /// Does not require the page to be resident (it targets the manifest record). The caller mirrors
    /// the order into the doc separately (`set_z_order` / `set_text_pin_meta`). Reserves a
    /// [`SaveKind::Structural`] epoch, so the page reads as having a pending save until acknowledged.
    /// Without a saver, falls back to the synchronous `persist::save_page_band_order` (no committed
    /// seeding — the historical behaviour).
    ///
    /// # Errors
    /// Only the synchronous fallback can error; the async path enqueues and returns `Ok`.
    pub fn enqueue_page_band_order(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        order: Vec<BandRef>,
    ) -> Result<(), String> {
        if self.saver.is_none() {
            return persist::save_page_band_order(layers_dir, page_idx, &order);
        }
        self.enqueue_structural(page_idx, layers_dir, fallback_dir, StructuralEdit::BandOrder(order));
        Ok(())
    }

    /// ASYNC group-structure edit of a page: like [`Self::enqueue_page_band_order`] but for a
    /// [`GroupingEdit`] (membership, group create/remove, collapse, visibility, opacity, order, and
    /// group-owned pins). Because it is applied after the page's earlier raster part, a freshly added
    /// raster's membership can no longer be silently skipped. Without a saver, falls back to the
    /// synchronous `persist::save_page_grouping`.
    ///
    /// # Errors
    /// Only the synchronous fallback can error; the async path enqueues and returns `Ok`.
    pub fn enqueue_page_grouping(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        edit: GroupingEdit,
    ) -> Result<(), String> {
        if self.saver.is_none() {
            return persist::save_page_grouping(layers_dir, page_idx, &edit);
        }
        self.enqueue_structural(page_idx, layers_dir, fallback_dir, StructuralEdit::Grouping(edit));
        Ok(())
    }

    /// Enqueues a job carrying only `edit`, with a fresh [`SaveKind::Structural`] epoch. Callers have
    /// checked that a saver is enabled.
    fn enqueue_structural(&mut self, page_idx: usize, layers_dir: &Path, fallback_dir: Option<&Path>, edit: StructuralEdit) {
        let structural_epoch = self.next_save_epoch(page_idx, SaveKind::Structural);
        if let Some(saver) = &self.saver {
                        saver.enqueue(PageSaveJob {
                page_idx,
                layers_dir: layers_dir.to_path_buf(),
                fallback_dir: fallback_dir.map(Path::to_path_buf),
                raster: None,
                raster_epoch: None,
                text: None,
                text_epoch: None,
                effects: Vec::new(),
                structural: vec![edit],
                structural_epoch: Some(structural_epoch),
            });
            saver.note_action();
        }
    }

    /// Builds the OWNED whole-page save job for the background saver, CLONING all data the persist call
    /// needs so the worker holds no borrow into the doc. The field selection mirrors `flush_page_inner`
    /// (rasters bottom-to-top by `z`, base image + `pixels_dirty` + effects/display) and `write_page_text`
    /// (text nodes bottom-to-top by `z`) EXACTLY, so the worker's `persist::*` calls produce the same
    /// bytes the synchronous flush would. `removed_raster_uids` matches `flush_page_dropping_raster`'s
    /// drop set. Returns `None` if the page is not resident.
    #[must_use]
    fn build_page_save_job(
        &self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        removed_raster_uids: &[String],
    ) -> Option<PageSaveJob> {
        let page = self.pages.get(&page_idx)?;
        let raster = Self::build_raster_save_part(page, removed_raster_uids);
        let text = Self::build_text_save_part_from(page);
        Some(PageSaveJob {
            page_idx,
            layers_dir: layers_dir.to_path_buf(),
            fallback_dir: fallback_dir.map(Path::to_path_buf),
            raster: Some(raster),
            raster_epoch: None,
            text: Some(text),
            text_epoch: None,
            effects: Vec::new(),
            structural: Vec::new(),
            structural_epoch: None,
        })
    }

    /// Builds ONLY the owned text part for a resident page (for the text-only async path). `None` if
    /// the page is not resident.
    #[must_use]
    fn build_text_save_part(&self, page_idx: usize) -> Option<TextSavePart> {
        let page = self.pages.get(&page_idx)?;
        Some(Self::build_text_save_part_from(page))
    }

    /// Gathers the owned RASTER part of a page save, mirroring `flush_page_inner`'s `outs` build
    /// (bottom-to-top by `z`; base image + `pixels_dirty` + mask-clip; the effects chain + display
    /// image so the worker can reconcile effects exactly like the sync flush). Unlike the sync flush,
    /// base pixels are carried only for `pixels_dirty` rasters.
    #[must_use]
    fn build_raster_save_part(page: &DocPage, removed_raster_uids: &[String]) -> RasterSavePart {
        let mut raster_indices: Vec<usize> = page
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_raster())
            .map(|(i, _)| i)
            .collect();
        raster_indices.sort_by_key(|&i| page.nodes[i].z);

        let mut layers: Vec<OwnedRasterLayer> = Vec::with_capacity(raster_indices.len());
        for &i in &raster_indices {
            let node = &page.nodes[i];
            let NodeBody::Raster {
                base_image,
                display_image,
                effects,
                mask_clip,
                ..
            } = &node.body
            else {
                continue;
            };
            // The display image is only needed when there is a non-empty effects chain to reconcile
            // (the sync flush's `update_raster_effects` is gated on a non-empty chain). Cloning it
            // unconditionally would waste a copy on the common no-effects raster.
            let display = if effects.is_empty() {
                None
            } else {
                Some(display_image.clone())
            };
            layers.push(OwnedRasterLayer {
                uid: node.uid.clone(),
                name: node.name.clone(),
                visible: node.visible,
                opacity: node.opacity,
                transform: node.transform,
                deform: node.deform.clone(),
                group_uid: node.group_uid.clone(),
                // Pixels are carried only when they must be written: a clean raster's PNG is already
                // recorded, so cloning it would only grow the queued job (a held job may wait long).
                // If the write finds no record after all, it fails the raster kind and
                // `poll_save_acks` re-dirties the page so the retry carries the pixels.
                base_image: node.pixels_dirty.then(|| base_image.clone()),
                image_size: base_image.size,
                pixels_dirty: node.pixels_dirty,
                mask_clip: *mask_clip,
                display_image: display,
                effects: effects.clone(),
            });
        }
        RasterSavePart {
            layers,
            groups: page.groups.clone(),
            removed_uids: removed_raster_uids.to_vec(),
        }
    }

    /// Gathers the owned TEXT part of a page save, mirroring `write_page_text`'s per-node out build
    /// (bottom-to-top by `z`; full inline payload + rendered image + `pixels_dirty`). The worker
    /// reproduces the "rewrite PNG iff dirty or missing" rule from the carried `pixels_dirty`.
    #[must_use]
    fn build_text_save_part_from(page: &DocPage) -> TextSavePart {
        let mut text_indices: Vec<usize> = page
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.kind, NodeKind::Text))
            .map(|(i, _)| i)
            .collect();
        text_indices.sort_by_key(|&i| page.nodes[i].z);

        let mut nodes: Vec<OwnedTextNode> = Vec::with_capacity(text_indices.len());
        for &i in &text_indices {
            let node = &page.nodes[i];
            let NodeBody::Text {
                render_data,
                image,
                is_image,
                payload_uid,
                mask_clip,
                // Schema v4: both centering-assist values ARE part of the on-disk text payload.
                extra_centers,
                centering_frame,
            } = &node.body
            else {
                continue;
            };
            nodes.push(OwnedTextNode {
                uid: node.uid.clone(),
                name: node.name.clone(),
                z: node.z,
                layer_idx: node.text_layer_idx.unwrap_or(0),
                visible: node.visible,
                opacity: node.opacity,
                group_uid: node.group_uid.clone(),
                payload_uid: payload_uid.clone(),
                render_data: render_data.clone(),
                is_image: *is_image,
                transform: node.transform,
                deform: node.deform.clone(),
                mask_clip: *mask_clip,
                text_centers: text_centers_rec_from(extra_centers),
                centering_frame: *centering_frame,
                pinned_by_group: node.text_pinned_by_group,
                image: image.clone(),
                pixels_dirty: node.pixels_dirty,
            });
        }
        TextSavePart { nodes }
    }

    /// Sets `pixels_dirty` on every RASTER node of a resident page after its current raster epoch
    /// FAILED, so the next save carries (and rewrites) every raster's pixels. Does not bump the page
    /// revision or the version: nothing visible changed.
    fn mark_page_rasters_dirty(&mut self, page_idx: usize) {
        if let Some(page) = self.pages.get_mut(&page_idx) {
            for node in &mut page.nodes {
                if matches!(node.kind, NodeKind::Raster) {
                    node.pixels_dirty = true;
                }
            }
        }
    }

    /// Clears `pixels_dirty` on every RASTER node of a resident page after its current raster epoch
    /// is durably acknowledged, leaving text dirty flags untouched.
    fn clear_page_dirty(&mut self, page_idx: usize) {
        if let Some(page) = self.pages.get_mut(&page_idx) {
            for node in &mut page.nodes {
                if matches!(node.kind, NodeKind::Raster) {
                    node.pixels_dirty = false;
                }
            }
        }
    }

    /// Clears `pixels_dirty` on every TEXT node of a resident page after its current text epoch is
    /// durably acknowledged, leaving raster dirty flags untouched.
    fn clear_page_text_dirty(&mut self, page_idx: usize) {
        if let Some(page) = self.pages.get_mut(&page_idx) {
            for node in &mut page.nodes {
                if matches!(node.kind, NodeKind::Text) {
                    node.pixels_dirty = false;
                }
            }
        }
    }
}

impl Drop for LayerDoc {
    /// Shuts the background saver down (flush queue + join the worker) if one was enabled. A doc with
    /// no saver (the default — most unit tests) does nothing here, spawning/joining no thread.
    fn drop(&mut self) {
        self.shutdown_saver();
    }
}

#[cfg(test)]
mod tests;
