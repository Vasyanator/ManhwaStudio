/*
File: models/layer_model/saver.rs

Purpose:
Off-thread, coalescing persistence for the unified layer model. Lets a caller (the shared doc, or a
save-to-project merge worker) hand a fully OWNED page-save job to a background thread so the GUI /
holder never blocks on PNG encode + manifest read-modify-write, and never holds the doc lock during
I/O.

The worker mirrors the EXACT persist sequence of `LayerDoc::flush_page` / `flush_page_text`
(rasters → text → effects per page). Jobs are bucketed per page index and the LATEST data for each
kind (rasters / text / per-uid effects) is kept (coalesced), so a burst of edits to one page collapses
into a single write while a Full + a TextOnly job for the same page MERGE (neither kind's data is
dropped). Every drain pass then writes each target manifest ONCE: all its pages are applied to one
`persist::ManifestTxn` and committed together (one docstore commit per pass, not per page), and a
text write that would not change the page effective on disk is elided.

Error contract: a per-page failure (PNG encode, raster write, seeding) fails only that page's kind; a
failed commit fails EVERY job of the pass. Results feed the barrier's failed-text set and the
per-kind acknowledgement map.

Key types:
- `OwnedRasterLayer` / `RasterSavePart` / `TextSavePart` — owned mirrors of the inputs to
  `persist::save_page_rasters` / `persist::update_raster_effects` / `persist::write_page_text_payload`,
  so the worker holds no borrow into the doc.
- `PageSaveJob` — one page's owned save payload (its dirs + optional raster part + optional text part).
- `SaverMsg` — the worker mailbox protocol (`Job` / `Jobs` (one-pass batch) / `Barrier` / `Shutdown`).
- `LayerSaver` — owns the worker thread + its `Sender` and `JoinHandle`.
- `LayerSaverHandle` — a cheap-clone `Sender` wrapper so a merge worker can enqueue / barrier without
  locking the doc.

Key functions:
- `worker_loop` → `run_bucket` (group a pass by manifest) → `run_manifest_pass` (prepare text PNGs,
  apply every job to one transaction, commit once).

Notes:
The whole point is that `PageSaveJob` carries OWNED `ColorImage`s, not borrows, so the worker can run
the real `persist::*` write path while the doc is free for the GUI thread.
*/

use ms_thread::{self as thread, JoinHandle};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui::ColorImage;
use serde_json::Value;

use super::manifest::{CenteringFrameRec, DeformRec, TextCentersRec, TransformRec};
use super::persist::{self, GroupMeta, RasterLayerOut};
use ms_log::runtime_log;
use ms_log::trace::cat;

/// The independently acknowledged persistence kinds. Effects are part of the raster contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SaveKind {
    Raster,
    Text,
}

/// Latest completed worker epoch per page and save kind, shared with the document for acknowledgement.
#[derive(Debug, Default)]
pub struct SaveAckMap {
    done: HashMap<(usize, SaveKind), (u64, bool)>,
}

impl SaveAckMap {
    /// Records a completed epoch unless a newer completion is already present. Equal-epoch retries
    /// replace the prior outcome.
    pub fn record(&mut self, page: usize, kind: SaveKind, epoch: u64, ok: bool) {
        if self.done.get(&(page, kind)).is_none_or(|(stored_epoch, _)| epoch >= *stored_epoch) {
            self.done.insert((page, kind), (epoch, ok));
        }
    }

    /// Takes all unconsumed completions, leaving the shared map empty.
    #[must_use]
    pub fn take(&mut self) -> Vec<(usize, SaveKind, u64, bool)> {
        std::mem::take(&mut self.done)
            .into_iter()
            .map(|((page, kind), (epoch, ok))| (page, kind, epoch, ok))
            .collect()
    }
}

/// One owned raster layer for an off-thread save. Mirrors `persist::RasterLayerOut` but OWNS its base
/// image (`RasterLayerOut.image` borrows a `&ColorImage`), so the worker holds no borrow into the doc.
/// Converted back to the borrowed form at write time via [`OwnedRasterLayer::as_out`].
#[derive(Debug, Clone)]
pub struct OwnedRasterLayer {
    pub uid: String,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub transform: TransformRec,
    pub deform: Option<DeformRec>,
    pub group_uid: Option<String>,
    /// Pre-effects base pixels (owned). Written as the base PNG only when `pixels_dirty` — same rule
    /// as the synchronous flush.
    pub base_image: ColorImage,
    pub pixels_dirty: bool,
    pub mask_clip: Option<bool>,
    /// The post-effects display image, present only when `effects` is non-empty (so the worker can run
    /// `persist::update_raster_effects` exactly as the sync flush does). `None` ⇒ no effects to write.
    pub display_image: Option<ColorImage>,
    /// The non-destructive effects chain (empty ⇒ no effects reconcile for this layer).
    pub effects: Vec<Value>,
}

impl OwnedRasterLayer {
    /// Borrows this owned layer as a `persist::RasterLayerOut` for the real write path, so the worker
    /// calls the identical `persist::save_page_rasters` the sync flush calls. Also used by the PS
    /// editor's synchronous fallback (no saver) so the owned and borrowed paths stay byte-identical.
    #[must_use]
    pub fn as_out(&self) -> RasterLayerOut<'_> {
        RasterLayerOut {
            uid: self.uid.clone(),
            name: self.name.clone(),
            visible: self.visible,
            opacity: self.opacity,
            transform: self.transform,
            deform: self.deform.clone(),
            group_uid: self.group_uid.clone(),
            image: &self.base_image,
            pixels_dirty: self.pixels_dirty,
            mask_clip: self.mask_clip,
        }
    }
}

/// The owned raster half of a page save: the layers (bottom-to-top), the page's groups, and the uids
/// the writer explicitly removed this session (so `save_page_rasters` drops them instead of preserving
/// them as another tab's). Mirrors the inputs of `LayerDoc::flush_page_inner`.
#[derive(Debug, Clone)]
pub struct RasterSavePart {
    pub layers: Vec<OwnedRasterLayer>,
    pub groups: Vec<GroupMeta>,
    pub removed_uids: Vec<String>,
}

/// One owned text node for an off-thread save: its full inline payload plus the rendered image and a
/// dirty flag, so the worker reproduces `LayerDoc::write_page_text`'s "rewrite PNG iff dirty or
/// missing" rule before calling `persist::write_page_text_payload`.
#[derive(Debug, Clone)]
pub struct OwnedTextNode {
    pub uid: String,
    pub name: String,
    pub z: u32,
    pub layer_idx: u32,
    pub visible: bool,
    pub opacity: f32,
    pub group_uid: Option<String>,
    pub payload_uid: String,
    pub render_data: Value,
    /// Whether this text node represents a placed PNG image overlay.
    pub is_image: bool,
    pub transform: TransformRec,
    pub deform: Option<DeformRec>,
    pub mask_clip: Option<bool>,
    /// Schema v4: the renderer-measured centering-assist centers of `image`, already normalized
    /// (`None` when neither center was measured). Replayed verbatim into `TextPayloadOut`.
    pub text_centers: Option<TextCentersRec>,
    /// Schema v4: the centering-assist guide rectangle bound to this node.
    pub centering_frame: Option<CenteringFrameRec>,
    /// The rendered text image (owned). Encoded to `ps_p{page:04}_{uid}_text.png` only when
    /// `pixels_dirty` or the deterministic file is missing — same rule as the sync flush.
    pub image: ColorImage,
    pub pixels_dirty: bool,
}

/// The owned text half of a page save: every text node bottom-to-top by `z`.
#[derive(Debug, Clone)]
pub struct TextSavePart {
    pub nodes: Vec<OwnedTextNode>,
}

/// One owned TARGETED effects update for a single raster, mirroring the inputs of
/// `persist::update_raster_effects`. Unlike a `RasterSavePart` this NEVER rewrites the page's raster
/// set — it only reconciles one raster's `effects` chain + rendered PNG, so a tab that changed only
/// effects does not clobber other rasters. A `None` `display_image` is the CLEAR case (empty chain),
/// which the whole-page raster path cannot express (it skips empty chains).
#[derive(Debug, Clone)]
pub struct EffectsSaveItem {
    pub uid: String,
    /// The non-destructive effects chain. Empty ⇒ clear (with `display_image: None`).
    pub effects: Vec<Value>,
    /// The post-effects rendered image (owned). `None` ⇒ clear the effects + delete any rendered PNG.
    pub display_image: Option<ColorImage>,
}

/// One page's owned save payload. Either or both halves may be present: a whole-page flush sets both,
/// a text-only flush sets only `text`. When two jobs for the same page are coalesced, each present
/// half REPLACES the corresponding half of the queued job (latest wins per kind) while the other
/// half is preserved — so a Full then TextOnly (or vice-versa) never drops a kind's data.
#[derive(Debug, Clone)]
pub struct PageSaveJob {
    pub page_idx: usize,
    pub layers_dir: PathBuf,
    pub fallback_dir: Option<PathBuf>,
    pub raster: Option<RasterSavePart>,
    pub raster_epoch: Option<u64>,
    pub text: Option<TextSavePart>,
    pub text_epoch: Option<u64>,
    /// Targeted per-raster effects updates (effects-only path; never rewrites the raster set). Kept as
    /// a per-uid list so two effects updates to DIFFERENT rasters in one coalescing pass both survive
    /// (latest-per-uid wins). Empty for jobs that carry no effects-only update.
    pub effects: Vec<EffectsSaveItem>,
}

impl PageSaveJob {
    /// Merges `next` (a newer job for the SAME page) into `self`: each present half of `next` replaces
    /// the corresponding half of `self`, the other half is kept, and the dirs adopt `next`'s (the
    /// freshest target). This is the per-kind coalescing that keeps a Full + a TextOnly job from
    /// dropping either kind. `debug_assert`s the page indices match.
    fn merge_in_place(&mut self, next: PageSaveJob) {
        debug_assert_eq!(
            self.page_idx, next.page_idx,
            "merge_in_place requires matching page indices"
        );
        self.layers_dir = next.layers_dir;
        self.fallback_dir = next.fallback_dir;
        if next.raster.is_some() {
            self.raster = next.raster;
            self.raster_epoch = next.raster_epoch;
        }
        if next.text.is_some() {
            self.text = next.text;
            self.text_epoch = next.text_epoch;
        }
        if !next.effects.is_empty() {
            // An effects-only job contributes to the raster acknowledgement, so adopt its epoch — but
            // only when it actually carries one. Never null a still-valid `raster_epoch` (e.g. from a
            // prior raster half of this coalesced job) with a `None` from an effects-only merge.
            if let Some(re) = next.raster_epoch {
                self.raster_epoch = Some(re);
            }
        }
        // Effects coalesce per-uid: a newer update for a uid REPLACES the older one (latest wins),
        // while updates to other uids are preserved. This keeps two effects edits to different rasters
        // in one drain pass from dropping either, and matches the per-kind "latest wins" rule.
        for item in next.effects {
            if let Some(existing) = self.effects.iter_mut().find(|e| e.uid == item.uid) {
                *existing = item;
            } else {
                self.effects.push(item);
            }
        }
    }

    /// Encodes the text PNGs this job's text part needs, reproducing `LayerDoc::write_page_text`'s
    /// "rewrite PNG iff dirty or missing" rule, and builds the payload for the single text writer.
    /// Runs OUTSIDE the manifest lock (PNG encoding is the slow part). `None` when the job carries no
    /// text part.
    fn prepare_text(&self) -> Option<Result<PreparedText, String>> {
        let text = self.text.as_ref()?;
        let layers_dir = self.layers_dir.as_path();
        let fallback_dir = self.fallback_dir.as_deref();
        Some((|| {
            let mut wrote_png = false;
            let mut outs: Vec<persist::TextPayloadOut> = Vec::with_capacity(text.nodes.len());
            for node in &text.nodes {
                let file_name = persist::text_image_file_name(self.page_idx, &node.uid);
                // Presence check via the storage seam (was `Path::is_file`): a deterministic text-PNG
                // name is either present or not, and storage `exists` answers the same question.
                let primary_path = layers_dir.join(&file_name);
                let present = ms_storage::global::storage().exists(primary_path.to_string_lossy().as_ref())
                    || fallback_dir.is_some_and(|d| ms_storage::global::storage().exists(d.join(&file_name).to_string_lossy().as_ref()));
                let rendered_file = if node.pixels_dirty || !present {
                    wrote_png = true;
                    Some(persist::write_text_image(layers_dir, self.page_idx, &node.uid, &node.image)?)
                } else {
                    Some(file_name)
                };
                outs.push(persist::TextPayloadOut {
                    uid: node.uid.clone(),
                    name: node.name.clone(),
                    z: node.z,
                    layer_idx: node.layer_idx,
                    pinned: false,
                    visible: node.visible,
                    opacity: node.opacity,
                    group_uid: node.group_uid.clone(),
                    pinned_by_group: false,
                    payload_uid: node.payload_uid.clone(),
                    render_data: node.render_data.clone(),
                    is_image: node.is_image,
                    transform: node.transform,
                    deform: node.deform.clone(),
                    rendered_file,
                    mask_clip: node.mask_clip,
                    text_centers: node.text_centers,
                    centering_frame: node.centering_frame,
                });
            }
            Ok(PreparedText { outs, wrote_png })
        })())
    }

    /// Applies this job to the pass's open manifest transaction, mirroring the EXACT sequence of
    /// `LayerDoc::flush_page` / `flush_page_text`:
    /// 1. rasters via `ManifestTxn::save_page_rasters` (when a raster part is present),
    /// 2. text via `ManifestTxn::write_page_text_payload` with the payload from [`Self::prepare_text`],
    ///    elided when it would not change the page (and no text PNG was written for it),
    /// 3. effects reconcile via `ManifestTxn::update_raster_effects` for every raster with a non-empty
    ///    chain, then the targeted effects-only items — as ONE unit that is rolled back as a whole
    ///    when any item fails.
    ///
    /// Each kind is atomic within the transaction, so a failing kind never leaves a partial page
    /// edit for the others' commit. Nothing is on disk until the pass commits.
    fn apply(&self, txn: &mut persist::ManifestTxn, text: Option<Result<PreparedText, String>>) -> KindResults {
        let page = self.page_idx;
        let fallback_dir = self.fallback_dir.as_deref();

        let raster = self.raster.as_ref().map(|raster| {
            let outs: Vec<RasterLayerOut<'_>> = raster.layers.iter().map(OwnedRasterLayer::as_out).collect();
            txn.save_page_rasters(page, &outs, &raster.groups, &raster.removed_uids, fallback_dir)
        });

        let text = text.map(|prepared| {
            let prepared = prepared?;
            // A text PNG written into staging for this page makes the manifest write load-bearing
            // (it must name the fresh file in THIS tree), so only a PNG-free write may be elided.
            txn.write_page_text_payload(fallback_dir, page, &prepared.outs, !prepared.wrote_png).map(|_changed| ())
        });

        let checkpoint = txn.checkpoint(page);
        let effects = (|| {
            if let Some(raster) = &self.raster {
                for layer in raster.layers.iter().filter(|l| !l.effects.is_empty()) {
                    txn.update_raster_effects(page, &layer.uid, &layer.effects, layer.display_image.as_ref(), fallback_dir)?;
                }
            }
            // Targeted effects-only updates: reconcile a single raster's chain WITHOUT a whole-page
            // raster rewrite. The ONLY path that can express the CLEAR case (empty chain +
            // `display_image: None`), which the raster reconcile loop above skips.
            for item in &self.effects {
                txn.update_raster_effects(page, &item.uid, &item.effects, item.display_image.as_ref(), fallback_dir)?;
            }
            Ok(())
        })();
        if effects.is_err() {
            txn.rollback(checkpoint);
        }
        KindResults { raster, text, effects }
    }

    /// All present kinds failed with `err` (a panic, or a transaction that could not begin).
    fn failed_results(&self, err: &str) -> KindResults {
        KindResults {
            raster: self.raster.as_ref().map(|_| Err(err.to_string())),
            text: self.text.as_ref().map(|_| Err(err.to_string())),
            effects: if self.effects.is_empty() { Ok(()) } else { Err(err.to_string()) },
        }
    }
}

/// A job's text payload after its PNGs were encoded; `wrote_png` records whether any PNG was written.
#[derive(Debug)]
struct PreparedText {
    outs: Vec<persist::TextPayloadOut>,
    wrote_png: bool,
}

/// Independent persist results for one job within a pass. Effects contribute to the raster
/// acknowledgement.
#[derive(Debug)]
struct KindResults {
    raster: Option<Result<(), String>>,
    text: Option<Result<(), String>>,
    effects: Result<(), String>,
}

impl KindResults {
    /// `(raster_ok, text_ok)` once the pass's single manifest commit returned `commit_ok`: a failed
    /// commit persisted nothing, so every kind the job carried is reported failed.
    fn acks(&self, commit_ok: bool) -> (bool, bool) {
        let raster_ok = commit_ok && self.raster.as_ref().is_none_or(Result::is_ok) && self.effects.is_ok();
        let text_ok = commit_ok && self.text.as_ref().is_none_or(Result::is_ok);
        (raster_ok, text_ok)
    }

    fn log_errors(&self, page: usize) {
        if let Some(Err(err)) = self.raster.as_ref() {
            runtime_log::log_error(format!("[layer_model::saver] failed to persist page {page} raster: {err}"));
        }
        if let Some(Err(err)) = self.text.as_ref() {
            runtime_log::log_error(format!("[layer_model::saver] failed to persist page {page} text: {err}"));
        }
        if let Err(err) = &self.effects {
            runtime_log::log_error(format!("[layer_model::saver] failed to persist page {page} effects: {err}"));
        }
    }
}

/// The background saver's mailbox protocol.
///
/// `clippy::large_enum_variant` fires on the `x86_64-pc-windows-gnu` target only (the
/// `PageSaveJob` payload lands just past clippy's 200-byte threshold there, and stays under
/// it on Linux), and boxing the payload would buy nothing: `Job` is the variant that is
/// actually sent, in a burst per page save, so a `Box` would add one heap allocation per
/// message to shrink a channel slot that only ever holds one message at a time. The pre-existing
/// wording of the same trade-off is `src/web_entry.rs:97`.
#[allow(clippy::large_enum_variant)]
pub enum SaverMsg {
    /// Persist one page (coalesced per page in the worker).
    Job(PageSaveJob),
    /// Persist several pages that the sender built together (e.g. every resident page's text at
    /// save-to-project). One message guarantees they land in the SAME drain pass — hence one
    /// manifest commit — whereas separate `Job`s sent with work in between can each wake the worker
    /// into its own pass.
    Jobs(Vec<PageSaveJob>),
    /// Process every currently-queued job, then signal completion on the sender. Used by
    /// `barrier_blocking` so a caller can be sure all prior enqueued jobs are on disk (e.g. before a
    /// save-to-project merge reads the staging files). The reply reports pages whose latest write
    /// TEXT write failed, so a merge can preserve their committed text without raster coupling.
    Barrier(Sender<HashSet<usize>>),
    /// Drain any remaining queued jobs, then stop the worker.
    Shutdown,
}

/// A cheap-clone handle to the background saver's `Sender`. Lets a merge worker enqueue jobs and run a
/// barrier without holding the doc lock (it carries only the channel, not the doc). Cloning is a
/// `Sender` clone; dropping a handle does NOT stop the worker (the `LayerSaver` owns shutdown).
#[derive(Clone)]
pub struct LayerSaverHandle {
    tx: Sender<SaverMsg>,
}

impl LayerSaverHandle {
    /// Enqueues a page-save job. A send failure (worker gone) is logged and dropped — the synchronous
    /// flush fallback on the doc remains available, so a lost background save is recoverable, never a
    /// panic.
    pub fn enqueue(&self, job: PageSaveJob) {
        if self.tx.send(SaverMsg::Job(job)).is_err() {
            runtime_log::log_error(
                "[layer_model::saver] enqueue failed: background saver thread is gone",
            );
        }
    }

    /// Enqueues several page-save jobs as ONE message, so the worker applies them in one drain pass
    /// (one manifest commit per target manifest). An empty batch sends nothing. A send failure is
    /// logged and dropped, like [`Self::enqueue`].
    pub fn enqueue_batch(&self, jobs: Vec<PageSaveJob>) {
        if jobs.is_empty() {
            return;
        }
        if self.tx.send(SaverMsg::Jobs(jobs)).is_err() {
            runtime_log::log_error(
                "[layer_model::saver] batch enqueue failed: background saver thread is gone",
            );
        }
    }

    /// Blocks until every job enqueued BEFORE this call has completed, returning pages whose latest
    /// TEXT write failed. Returns an empty set if the worker is gone; the loss is logged.
    #[must_use]
    pub fn barrier_blocking(&self) -> HashSet<usize> {
        let (done_tx, done_rx) = mpsc::channel::<HashSet<usize>>();
        if self.tx.send(SaverMsg::Barrier(done_tx)).is_err() {
            runtime_log::log_error(
                "[layer_model::saver] barrier failed: background saver thread is gone",
            );
            return HashSet::new();
        }
        // `recv` returns `Err` only if the worker dropped the sender without replying (it panicked
        // mid-drain); treat that as "barrier could not complete" and proceed rather than hang.
        match done_rx.recv() {
            Ok(failed_pages) => failed_pages,
            Err(_) => {
                runtime_log::log_error(
                    "[layer_model::saver] barrier sender dropped without reply (worker stopped)",
                );
                HashSet::new()
            }
        }
    }
}

/// Owns the background saver thread, its `Sender`, and its `JoinHandle`. Created with
/// [`LayerSaver::new`]; shut down explicitly with [`LayerSaver::shutdown`] (sentinel + join) or
/// implicitly via the holder's `Drop`.
pub struct LayerSaver {
    tx: Sender<SaverMsg>,
    handle: Option<JoinHandle<()>>,
    ack_map: Arc<Mutex<SaveAckMap>>,
}

impl LayerSaver {
    /// Spawns the background saver thread and returns the owner.
    ///
    /// The worker loop blocks on `recv`, then drains every immediately-available message with
    /// `try_recv`, BUCKETING `Job`s per `page_idx` (coalescing — the latest data per kind wins). A
    /// `Barrier` flushes the current bucket then replies; a `Shutdown` flushes the bucket then breaks.
    #[must_use]
    pub fn new() -> LayerSaver {
        let (tx, rx) = mpsc::channel::<SaverMsg>();
        let ack_map = Arc::new(Mutex::new(SaveAckMap::default()));
        let worker_ack_map = Arc::clone(&ack_map);
        let handle = thread::spawn(move || worker_loop(&rx, &worker_ack_map));
        LayerSaver {
            tx,
            handle: Some(handle),
            ack_map,
        }
    }

    /// A cheap-clone handle a merge worker can use to enqueue / barrier without locking the doc.
    #[must_use]
    pub fn handle(&self) -> LayerSaverHandle {
        LayerSaverHandle {
            tx: self.tx.clone(),
        }
    }

    /// Returns the acknowledgement map shared by the worker and its owning document.
    #[must_use]
    pub fn ack_map(&self) -> Arc<Mutex<SaveAckMap>> {
        Arc::clone(&self.ack_map)
    }

    /// Enqueues a page-save job (see [`LayerSaverHandle::enqueue`]).
    pub fn enqueue(&self, job: PageSaveJob) {
        self.handle().enqueue(job);
    }

    /// Enqueues several jobs into one drain pass (see [`LayerSaverHandle::enqueue_batch`]).
    pub fn enqueue_batch(&self, jobs: Vec<PageSaveJob>) {
        self.handle().enqueue_batch(jobs);
    }

    /// Blocks until every previously enqueued job completes, returning pages whose latest TEXT write failed
    /// (see [`LayerSaverHandle::barrier_blocking`]).
    ///
    /// Production code barriers via a cloned [`LayerSaverHandle`] (the merge worker and app-close
    /// drain hold a handle, not the owner), so this owner-side convenience wrapper has no non-test
    /// caller. It is the symmetric counterpart to [`LayerSaver::enqueue`] and is exercised by this
    /// module's unit tests; the `dead_code` lint is a false-positive for "API completeness used only
    /// in tests" (CLAUDE.md §17 permits an allow when the lint is inapplicable for a stated reason).
    #[allow(dead_code)]
    #[must_use]
    pub fn barrier_blocking(&self) -> HashSet<usize> {
        self.handle().barrier_blocking()
    }

    /// Shuts the worker down: sends `Shutdown` (so the worker drains its queue first) and joins the
    /// thread. A send/join failure is logged, never panicked.
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    /// Sends the shutdown sentinel and joins the worker. Idempotent: a second call (e.g. `Drop` after
    /// an explicit `shutdown`) finds no handle and is a no-op.
    fn shutdown_inner(&mut self) {
        if self.tx.send(SaverMsg::Shutdown).is_err() {
            // The worker already stopped; nothing queued can be lost beyond what it already drained.
            runtime_log::log_warn(
                "[layer_model::saver] shutdown: background saver thread already gone",
            );
        }
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            runtime_log::log_error(
                "[layer_model::saver] background saver thread panicked during shutdown",
            );
        }
    }
}

impl Default for LayerSaver {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LayerSaver {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

/// The background worker body: `recv` then `try_recv`-drain, bucketing `Job`s per page (latest data
/// per kind), running each bucket, honoring `Barrier`/`Shutdown`. A persist error is logged and tracked
/// (the next page still runs) — a single bad page must not stall the saver.
fn worker_loop(rx: &Receiver<SaverMsg>, ack_map: &Arc<Mutex<SaveAckMap>>) {
    let mut failed_raster_pages = HashSet::new();
    let mut failed_text_pages = HashSet::new();
    while let Ok(first) = rx.recv() {
        // Per-page coalescing bucket for this drain pass. Insertion order is preserved by tracking the
        // page sequence so writes happen in a deterministic order.
        let mut bucket: HashMap<usize, PageSaveJob> = HashMap::new();
        let mut order: Vec<usize> = Vec::new();
        let mut pending_barriers: Vec<Sender<HashSet<usize>>> = Vec::new();
        let mut shutdown = false;

        // Fold the first message, then drain everything immediately available.
        let mut msg = Some(first);
        let mut fold = |job: PageSaveJob| {
            let page = job.page_idx;
            match bucket.get_mut(&page) {
                Some(existing) => existing.merge_in_place(job),
                None => {
                    order.push(page);
                    bucket.insert(page, job);
                }
            }
        };
        loop {
            match msg.take() {
                Some(SaverMsg::Job(job)) => fold(job),
                Some(SaverMsg::Jobs(jobs)) => jobs.into_iter().for_each(&mut fold),
                Some(SaverMsg::Barrier(done)) => pending_barriers.push(done),
                Some(SaverMsg::Shutdown) => {
                    shutdown = true;
                    // Keep draining queued jobs after a shutdown so nothing already enqueued is lost.
                }
                None => {}
            }
            match rx.try_recv() {
                Ok(next) => msg = Some(next),
                Err(_) => break,
            }
        }

        // Run every coalesced page in insertion order, then release any barriers waiting on this pass.
        run_bucket(&bucket, &order, &mut failed_raster_pages, &mut failed_text_pages, ack_map);
        for done in pending_barriers {
            // The waiter may have given up (timed out / dropped); ignore a closed receiver.
            done.send(failed_text_pages.clone()).ok();
        }
        if shutdown {
            break;
        }
    }
}

/// Runs every job in `bucket` following `order` (deterministic write order). Jobs are grouped by
/// target manifest (`layers_dir`, first-seen order) and each group is ONE manifest transaction
/// ([`run_manifest_pass`]): a drain pass that coalesced K pages costs one document commit, not K.
/// Per-kind results are logged — not propagated — and fold into the failed-page sets and acks, so one
/// bad page does not stall the others.
fn run_bucket(
    bucket: &HashMap<usize, PageSaveJob>,
    order: &[usize],
    failed_raster_pages: &mut HashSet<usize>,
    failed_text_pages: &mut HashSet<usize>,
    ack_map: &Arc<Mutex<SaveAckMap>>,
) {
    let mut groups: Vec<(&std::path::Path, Vec<&PageSaveJob>)> = Vec::new();
    for job in order.iter().filter_map(|page| bucket.get(page)) {
        match groups.iter_mut().find(|(dir, _)| *dir == job.layers_dir.as_path()) {
            Some((_, jobs)) => jobs.push(job),
            None => groups.push((job.layers_dir.as_path(), vec![job])),
        }
    }
    for (layers_dir, jobs) in groups {
        for (job, (raster_ok, text_ok)) in jobs.iter().zip(run_manifest_pass(layers_dir, &jobs)) {
            update_failed_set(failed_raster_pages, job.page_idx, job.raster_epoch.is_some(), raster_ok);
            update_failed_set(failed_text_pages, job.page_idx, job.text_epoch.is_some(), text_ok);
            record_job_acks(ack_map, job, raster_ok, text_ok);
        }
    }
}

/// Persists `jobs` (all targeting the manifest in `layers_dir`) with ONE manifest write and returns
/// `(raster_ok, text_ok)` per job, in order.
///
/// 1. Per-page preparation outside the manifest lock: each job's text PNGs are encoded. A failure
///    (or panic) there fails only THAT page's text.
/// 2. One `persist::ManifestTxn`: every job is applied to the in-memory manifest; a failing or
///    panicking job is rolled back to its checkpoint and fails alone.
/// 3. One commit. If it fails, nothing was persisted, so EVERY job of the pass reports every kind it
///    carried as failed (the doc then keeps them dirty for retry; the barrier revokes text ownership).
///
/// Panics are caught at each step (`catch_unwind`): a panic must not unwind out of the worker loop
/// and kill the saver thread — that would silently drop all later enqueues and leave every future
/// `barrier_blocking` unable to complete.
fn run_manifest_pass(layers_dir: &std::path::Path, jobs: &[&PageSaveJob]) -> Vec<(bool, bool)> {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    // `AssertUnwindSafe` throughout: jobs are read-only, and the transaction's page state is restored
    // from its checkpoint after a caught panic, so no observer ever sees a half-mutated value.
    let prepared: Vec<Option<Result<PreparedText, String>>> = jobs
        .iter()
        .map(|job| {
            catch_unwind(AssertUnwindSafe(|| job.prepare_text()))
                .unwrap_or_else(|_| Some(Err("panic while encoding the text PNGs".to_string())))
        })
        .collect();

    let mut txn = match catch_unwind(AssertUnwindSafe(|| persist::ManifestTxn::begin(layers_dir))) {
        Ok(Ok(txn)) => txn,
        Ok(Err(err)) => return fail_whole_pass(layers_dir, jobs, &format!("read manifest: {err}")),
        Err(_) => return fail_whole_pass(layers_dir, jobs, "panic while reading the manifest"),
    };
    let mut results: Vec<KindResults> = Vec::with_capacity(jobs.len());
    for (job, text) in jobs.iter().zip(prepared) {
        let checkpoint = txn.checkpoint(job.page_idx);
        let result = match catch_unwind(AssertUnwindSafe(|| job.apply(&mut txn, text))) {
            Ok(result) => result,
            Err(_) => {
                txn.rollback(checkpoint);
                runtime_log::log_error(format!(
                    "[layer_model::saver] PANIC while persisting page {}; saver thread continues",
                    job.page_idx
                ));
                job.failed_results("panic while applying the page")
            }
        };
        result.log_errors(job.page_idx);
        results.push(result);
    }

    let commit_ok = match catch_unwind(AssertUnwindSafe(|| txn.commit())) {
        Ok(Ok(wrote)) => {
            ms_log::trace_log!(
                cat::PERSIST,
                "saver pass {} jobs={} manifest_written={}",
                layers_dir.display(),
                jobs.len(),
                wrote
            );
            true
        }
        Ok(Err(err)) => {
            log_pass_failure(layers_dir, jobs, &format!("write manifest: {err}"));
            false
        }
        Err(_) => {
            log_pass_failure(layers_dir, jobs, "panic while writing the manifest");
            false
        }
    };
    results.iter().map(|r| r.acks(commit_ok)).collect()
}

/// Logs a pass-wide failure (every job in it lost its writes) and reports all jobs failed.
fn fail_whole_pass(layers_dir: &std::path::Path, jobs: &[&PageSaveJob], err: &str) -> Vec<(bool, bool)> {
    log_pass_failure(layers_dir, jobs, err);
    vec![(false, false); jobs.len()]
}

fn log_pass_failure(layers_dir: &std::path::Path, jobs: &[&PageSaveJob], err: &str) {
    let pages: Vec<usize> = jobs.iter().map(|job| job.page_idx).collect();
    runtime_log::log_error(format!(
        "[layer_model::saver] failed to persist pages {pages:?} of {}: {err}. None of these pages was saved; they stay dirty for retry.",
        layers_dir.display()
    ));
}

fn update_failed_set(failed: &mut HashSet<usize>, page: usize, present: bool, ok: bool) {
    if present && ok {
        failed.remove(&page);
    } else if present {
        failed.insert(page);
    }
}

fn record_job_acks(ack_map: &Arc<Mutex<SaveAckMap>>, job: &PageSaveJob, raster_ok: bool, text_ok: bool) {
    let Ok(mut ack) = ack_map.lock() else {
        runtime_log::log_error("[layer_model::saver] acknowledgement map lock poisoned");
        return;
    };
    if let Some(epoch) = job.raster_epoch {
        ack.record(job.page_idx, SaveKind::Raster, epoch, raster_ok);
    }
    if let Some(epoch) = job.text_epoch {
        ack.record(job.page_idx, SaveKind::Text, epoch, text_ok);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh unique temp dir for a test (no `tempfile` dev-dep, mirroring the other module tests).
    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ms_layer_saver_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn img(size: [usize; 2], c: Color32) -> ColorImage {
        ColorImage::filled(size, c)
    }

    fn tf(cx: f32, cy: f32) -> TransformRec {
        TransformRec {
            cx,
            cy,
            rotation: 0.0,
            scale: 1.0,
        }
    }

    fn raster(uid: &str, c: Color32) -> OwnedRasterLayer {
        OwnedRasterLayer {
            uid: uid.to_string(),
            name: uid.to_string(),
            visible: true,
            opacity: 1.0,
            transform: tf(1.0, 1.0),
            deform: None,
            group_uid: None,
            base_image: img([2, 2], c),
            pixels_dirty: true,
            mask_clip: None,
            display_image: None,
            effects: Vec::new(),
        }
    }

    fn raster_part(layers: Vec<OwnedRasterLayer>) -> RasterSavePart {
        RasterSavePart {
            layers,
            groups: Vec::new(),
            removed_uids: Vec::new(),
        }
    }

    fn text_node(uid: &str, c: Color32) -> OwnedTextNode {
        OwnedTextNode {
            uid: uid.to_string(),
            name: uid.to_string(),
            z: 0,
            layer_idx: 0,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            payload_uid: uid.to_string(),
            render_data: Value::Null,
            is_image: false,
            transform: tf(5.0, 5.0),
            deform: None,
            mask_clip: None,
            text_centers: None,
            centering_frame: None,
            image: img([2, 2], c),
            pixels_dirty: true,
        }
    }

    fn full_job(page: usize, dir: &Path, rasters: Vec<OwnedRasterLayer>) -> PageSaveJob {
        PageSaveJob {
            page_idx: page,
            layers_dir: dir.to_path_buf(),
            fallback_dir: None,
            raster: Some(raster_part(rasters)),
            raster_epoch: Some(1),
            text: None,
            text_epoch: None,
            effects: Vec::new(),
        }
    }

    #[test]
    fn ack_map_keeps_newest_completion_and_take_clears() {
        let mut acks = SaveAckMap::default();
        acks.record(4, SaveKind::Text, 8, true);
        acks.record(4, SaveKind::Text, 7, false);
        acks.record(4, SaveKind::Text, 8, false);
        let taken = acks.take();
        assert_eq!(taken, vec![(4, SaveKind::Text, 8, false)]);
        assert!(acks.take().is_empty(), "take clears consumed acknowledgements");
    }

    /// Three Full jobs for the same page coalesce to the LATEST on-disk state (last writer wins per
    /// kind). The final manifest must reflect only the third job's layers.
    #[test]
    fn per_page_coalescing_keeps_latest() {
        let dir = temp_dir("coalesce");
        let saver = LayerSaver::new();
        // Three distinct raster sets for page 5; only the last must survive.
        saver.enqueue(full_job(5, &dir, vec![raster("a", Color32::RED)]));
        saver.enqueue(full_job(5, &dir, vec![raster("b", Color32::GREEN)]));
        saver.enqueue(full_job(
            5,
            &dir,
            vec![raster("c", Color32::BLUE), raster("d", Color32::WHITE)],
        ));
        assert!(saver.barrier_blocking().is_empty());

        let page = persist::load_page_rasters(&dir, None, 5).unwrap();
        let mut uids: Vec<&str> = page.layers.iter().map(|l| l.uid.as_str()).collect();
        uids.sort_unstable();
        assert_eq!(
            uids,
            vec!["c", "d"],
            "only the latest job's rasters persisted"
        );

        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `barrier_blocking` returns only AFTER queued jobs are written: right after it returns, the files
    /// must already exist on disk.
    #[test]
    fn barrier_blocks_until_written() {
        let dir = temp_dir("barrier");
        let saver = LayerSaver::new();
        saver.enqueue(full_job(0, &dir, vec![raster("r", Color32::RED)]));
        assert!(saver.barrier_blocking().is_empty());

        // Immediately readable — the barrier guarantees the write completed.
        assert!(
            dir.join("layers.json").is_file(),
            "manifest written before barrier returned"
        );
        let page = persist::load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(page.layers.len(), 1);
        assert_eq!(page.layers[0].uid, "r");

        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shutdown drains a pending job before exiting: a job enqueued just before `shutdown` must still
    /// land on disk.
    #[test]
    fn shutdown_drains_pending_job() {
        let dir = temp_dir("shutdown");
        let saver = LayerSaver::new();
        saver.enqueue(full_job(3, &dir, vec![raster("z", Color32::GREEN)]));
        // No barrier: rely on Shutdown draining the queue.
        saver.shutdown();

        let page = persist::load_page_rasters(&dir, None, 3).unwrap();
        assert_eq!(page.layers.len(), 1, "pending job drained on shutdown");
        assert_eq!(page.layers[0].uid, "z");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Full job and a TextOnly job for the SAME page must MERGE: the rasters from the Full job and
    /// the text from the TextOnly job both survive (neither kind erases the other). Verified against
    /// the real `persist` round-trip.
    #[test]
    fn full_and_text_coalesce_without_dropping_either() {
        let dir = temp_dir("merge");
        let saver = LayerSaver::new();
        // Full job: a raster, no text.
        saver.enqueue(full_job(2, &dir, vec![raster("rast", Color32::RED)]));
        // TextOnly job for the same page: text, no raster part.
        saver.enqueue(PageSaveJob {
            page_idx: 2,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: None,
            raster_epoch: None,
            text: Some(TextSavePart {
                nodes: vec![text_node("txt", Color32::BLUE)],
            }),
            text_epoch: Some(1),
            effects: Vec::new(),
        });
        assert!(saver.barrier_blocking().is_empty());

        // Raster survives (text-only half did not erase it).
        let rasters = persist::load_page_rasters(&dir, None, 2).unwrap();
        assert_eq!(
            rasters.layers.len(),
            1,
            "raster preserved through text merge"
        );
        assert_eq!(rasters.layers[0].uid, "rast");

        // Text survives (full half did not erase it). The rendered text PNG exists too.
        let texts = persist::load_page_text_nodes(&dir, None, 2).unwrap();
        assert_eq!(texts.len(), 1, "text preserved through raster merge");
        assert_eq!(texts[0].uid, "txt");
        let text_png = dir.join(persist::text_image_file_name(2, "txt"));
        assert!(text_png.is_file(), "rendered text PNG written");

        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reverse order (TextOnly enqueued first, then Full) must also keep both kinds — the merge is
    /// symmetric.
    #[test]
    fn text_then_full_coalesce_without_dropping_either() {
        let dir = temp_dir("merge_rev");
        let saver = LayerSaver::new();
        saver.enqueue(PageSaveJob {
            page_idx: 7,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: None,
            raster_epoch: None,
            text: Some(TextSavePart {
                nodes: vec![text_node("t", Color32::BLUE)],
            }),
            text_epoch: Some(1),
            effects: Vec::new(),
        });
        saver.enqueue(full_job(7, &dir, vec![raster("r", Color32::RED)]));
        assert!(saver.barrier_blocking().is_empty());

        let rasters = persist::load_page_rasters(&dir, None, 7).unwrap();
        assert_eq!(rasters.layers.len(), 1);
        assert_eq!(rasters.layers[0].uid, "r");
        let texts = persist::load_page_text_nodes(&dir, None, 7).unwrap();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].uid, "t");

        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The PRODUCTION text write path is the off-thread saver, not `LayerDoc::flush_page`: this test
    /// pins that `PageSaveJob::run` replays the schema-v4 centering-assist state (`text_centers` +
    /// `centering_frame`) from the owned node into the manifest, so dropping that replay can no longer
    /// break persistence with a green suite.
    #[test]
    fn text_job_persists_centering_state_through_the_saver() {
        let dir = temp_dir("centering");
        let saver = LayerSaver::new();
        let centers = TextCentersRec {
            mean: Some([12.5, -3.25]),
            median: Some([11.0, -2.5]),
        };
        let frame = CenteringFrameRec {
            cx: 400.0,
            cy: 250.5,
            half_w: 80.25,
            half_h: 45.75,
        };
        let mut node = text_node("txt", Color32::BLUE);
        // A real payload is required for the node to load back as a self-sufficient INLINE node: the
        // helper's `Value::Null` round-trips through `Option<Value>` as absent, which is the
        // image-overlay representation, not an inline text payload.
        node.render_data = serde_json::json!({ "text": "txt" });
        node.text_centers = Some(centers);
        node.centering_frame = Some(frame);
        saver.enqueue(PageSaveJob {
            page_idx: 3,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: None,
            raster_epoch: None,
            text: Some(TextSavePart { nodes: vec![node] }),
            text_epoch: Some(1),
            effects: Vec::new(),
        });
        assert!(saver.barrier_blocking().is_empty());

        let texts = persist::load_page_text_nodes(&dir, None, 3).expect("text page loads back");
        assert_eq!(texts.len(), 1, "the text node persisted");
        let inline = texts[0]
            .inline
            .as_ref()
            .expect("saver wrote a self-sufficient inline payload");
        assert_eq!(
            inline.text_centers,
            Some(centers),
            "measured centers survived the off-thread write"
        );
        assert_eq!(
            inline.centering_frame,
            Some(frame),
            "guide frame survived the off-thread write"
        );

        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An effects-only job sets a single raster's chain WITHOUT rewriting the page rasters; a later
    /// effects-only CLEAR job (empty chain, no rendered) zeroes it — the case the whole-page raster
    /// reconcile loop cannot express. Verifies both directions against the persist round-trip and that
    /// other rasters on the page are untouched.
    #[test]
    fn effects_only_job_sets_then_clears_without_touching_rasters() {
        let dir = temp_dir("fx_only");
        let saver = LayerSaver::new();
        // Seed two rasters on the page (whole-page save, no effects).
        saver.enqueue(full_job(
            4,
            &dir,
            vec![raster("a", Color32::RED), raster("b", Color32::GREEN)],
        ));
        // Effects-only update for ONLY "a": set a non-empty chain + a rendered display.
        let chain = vec![serde_json::json!({"effect_type": "blur", "radius": 2})];
        saver.enqueue(PageSaveJob {
            page_idx: 4,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: None,
            raster_epoch: Some(2),
            text: None,
            text_epoch: None,
            effects: vec![EffectsSaveItem {
                uid: "a".to_string(),
                effects: chain.clone(),
                display_image: Some(img([2, 2], Color32::BLUE)),
            }],
        });
        assert!(saver.barrier_blocking().is_empty());

        let page = persist::load_page_rasters(&dir, None, 4).unwrap();
        assert_eq!(
            page.layers.len(),
            2,
            "both rasters preserved (no rewrite drop)"
        );
        let a = page.layers.iter().find(|l| l.uid == "a").unwrap();
        assert_eq!(a.effects, chain, "effects-only job set the chain");
        let b = page.layers.iter().find(|l| l.uid == "b").unwrap();
        assert!(
            b.effects.is_empty(),
            "other raster untouched by targeted effects update"
        );

        // Effects-only CLEAR for "a": empty chain + no rendered. The raster reconcile loop would skip
        // this (it gates on a non-empty chain), so only the effects-only path can express it.
        saver.enqueue(PageSaveJob {
            page_idx: 4,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: None,
            raster_epoch: Some(3),
            text: None,
            text_epoch: None,
            effects: vec![EffectsSaveItem {
                uid: "a".to_string(),
                effects: Vec::new(),
                display_image: None,
            }],
        });
        assert!(saver.barrier_blocking().is_empty());

        let page = persist::load_page_rasters(&dir, None, 4).unwrap();
        let a = page.layers.iter().find(|l| l.uid == "a").unwrap();
        assert!(a.effects.is_empty(), "effects-only CLEAR zeroed the chain");

        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two effects-only updates to DIFFERENT rasters in one coalescing pass both survive (per-uid
    /// latest-wins), so the merge never drops one raster's effects in favor of another's.
    #[test]
    fn effects_only_coalesces_per_uid() {
        let dir = temp_dir("fx_coalesce");
        let saver = LayerSaver::new();
        saver.enqueue(full_job(
            1,
            &dir,
            vec![raster("x", Color32::RED), raster("y", Color32::GREEN)],
        ));
        let cx = vec![serde_json::json!({"effect_type": "blur"})];
        let cy = vec![serde_json::json!({"effect_type": "glow"})];
        saver.enqueue(PageSaveJob {
            page_idx: 1,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: None,
            raster_epoch: Some(2),
            text: None,
            text_epoch: None,
            effects: vec![EffectsSaveItem {
                uid: "x".to_string(),
                effects: cx.clone(),
                display_image: Some(img([2, 2], Color32::BLUE)),
            }],
        });
        saver.enqueue(PageSaveJob {
            page_idx: 1,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: None,
            raster_epoch: Some(3),
            text: None,
            text_epoch: None,
            effects: vec![EffectsSaveItem {
                uid: "y".to_string(),
                effects: cy.clone(),
                display_image: Some(img([2, 2], Color32::WHITE)),
            }],
        });
        assert!(saver.barrier_blocking().is_empty());

        let page = persist::load_page_rasters(&dir, None, 1).unwrap();
        let x = page.layers.iter().find(|l| l.uid == "x").unwrap();
        let y = page.layers.iter().find(|l| l.uid == "y").unwrap();
        assert_eq!(x.effects, cx, "x effects survived the coalesce");
        assert_eq!(
            y.effects, cy,
            "y effects survived the coalesce (different uid not dropped)"
        );

        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed persist is reported by the barrier until a later successful retry for that page
    /// replaces the failure outcome.
    #[test]
    fn barrier_reports_failed_page_until_later_success() {
        let invalid_parent = temp_dir("barrier_failed_parent");
        std::fs::write(&invalid_parent, b"not a directory").unwrap();
        let valid_dir = temp_dir("barrier_failed_recovery");
        let saver = LayerSaver::new();
        saver.enqueue(PageSaveJob {
            page_idx: 9,
            layers_dir: invalid_parent.join("layers"),
            fallback_dir: None,
            raster: None,
            raster_epoch: None,
            text: Some(TextSavePart { nodes: vec![text_node("t", Color32::RED)] }),
            text_epoch: Some(1),
            effects: Vec::new(),
        });

        assert!(
            saver.barrier_blocking().contains(&9),
            "barrier reports the page whose latest persist failed"
        );

        saver.enqueue(PageSaveJob {
            page_idx: 9,
            layers_dir: valid_dir.clone(),
            fallback_dir: None,
            raster: None,
            raster_epoch: None,
            text: Some(TextSavePart { nodes: vec![text_node("t", Color32::GREEN)] }),
            text_epoch: Some(2),
            effects: Vec::new(),
        });
        assert!(
            !saver.barrier_blocking().contains(&9),
            "a later successful persist clears the page failure"
        );

        saver.shutdown();
        let _ = std::fs::remove_file(&invalid_parent);
        let _ = std::fs::remove_dir_all(&valid_dir);
    }

    #[test]
    fn raster_failure_does_not_report_successful_text_as_failed() {
        let dir = temp_dir("per_kind_failure");
        let saver = LayerSaver::new();
        let mut broken_raster = raster("r", Color32::RED);
        broken_raster.base_image.pixels.truncate(1);
        saver.enqueue(PageSaveJob {
            page_idx: 6,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: Some(raster_part(vec![broken_raster])),
            raster_epoch: Some(10),
            text: Some(TextSavePart { nodes: vec![text_node("t", Color32::WHITE)] }),
            text_epoch: Some(11),
            effects: Vec::new(),
        });
        assert!(saver.barrier_blocking().is_empty(), "raster failure does not contaminate text barrier");
        let ack_map = saver.ack_map();
        let mut completions = ack_map.lock().unwrap().take();
        completions.sort_by_key(|(_, kind, _, _)| match kind {
            SaveKind::Raster => 0,
            SaveKind::Text => 1,
        });
        assert_eq!(completions, vec![
            (6, SaveKind::Raster, 10, false),
            (6, SaveKind::Text, 11, true),
        ]);
        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- One manifest transaction per pass (batched commits) --------------------------------------

    /// Failed raster pages, failed text pages, and the acknowledgements `(page, kind, epoch, ok)`.
    type PassResult = (HashSet<usize>, HashSet<usize>, Vec<(usize, SaveKind, u64, bool)>);

    /// Runs ONE drain pass over `jobs` exactly as the worker does (`run_bucket`), on the calling
    /// thread, so a test controls what one pass contains. Returns the failed raster / text page sets
    /// and the recorded acknowledgements.
    fn run_pass(jobs: Vec<PageSaveJob>) -> PassResult {
        let mut bucket = HashMap::new();
        let mut order = Vec::new();
        for job in jobs {
            order.push(job.page_idx);
            bucket.insert(job.page_idx, job);
        }
        let ack_map = Arc::new(Mutex::new(SaveAckMap::default()));
        let (mut failed_raster, mut failed_text) = (HashSet::new(), HashSet::new());
        run_bucket(&bucket, &order, &mut failed_raster, &mut failed_text, &ack_map);
        let mut acks = ack_map.lock().expect("ack map").take();
        acks.sort_by_key(|(page, kind, _, _)| (*page, matches!(kind, SaveKind::Text)));
        (failed_raster, failed_text, acks)
    }

    fn text_job(page: usize, layers_dir: &Path, fallback_dir: Option<&Path>, nodes: Vec<OwnedTextNode>, epoch: u64) -> PageSaveJob {
        PageSaveJob {
            page_idx: page,
            layers_dir: layers_dir.to_path_buf(),
            fallback_dir: fallback_dir.map(Path::to_path_buf),
            raster: None,
            raster_epoch: None,
            text: Some(TextSavePart { nodes }),
            text_epoch: Some(epoch),
            effects: Vec::new(),
        }
    }

    /// A text node positioned at `(cx, cy)`, with an explicit z so the payload round-trips exactly.
    fn text_at(uid: &str, z: u32, cx: f32, cy: f32, pixels_dirty: bool) -> OwnedTextNode {
        let render_data = serde_json::json!({ "text": uid });
        OwnedTextNode { z, transform: tf(cx, cy), pixels_dirty, render_data, ..text_node(uid, Color32::WHITE) }
    }

    /// Committed + staging layer dirs of a chapter `ch` under `root`, with the committed manifest
    /// created (empty) in `format`, so a NEW staging manifest joins that format (rule B.3).
    fn chapter(root: &Path, format: ms_docstore::DocFormat) -> (PathBuf, PathBuf) {
        let committed = root.join("ch").join("layers");
        let staging = root.join("ch_unsaved").join("layers");
        std::fs::create_dir_all(&committed).expect("mkdir committed");
        let doc = ms_docstore::DocRef::new(committed.join("layers.json"), ms_docstore::DocKind::Layers).with_new_format(format);
        ms_docstore::write_value(&doc, &serde_json::json!({"schema_version": 4, "pages": []}), ms_docstore::WriteOptions::default()).expect("seed committed");
        (committed, staging)
    }

    fn staging_doc(staging: &Path) -> ms_docstore::DocRef {
        ms_docstore::DocRef::new(staging.join("layers.json"), ms_docstore::DocKind::Layers)
    }

    /// Number of atomic JSON replacements (temp + rename) of the staging manifest so far.
    fn json_writes(staging: &Path) -> usize {
        ms_docstore::recorded_steps(&staging.join("layers.json")).iter().filter(|s| **s == ms_docstore::WriteStep::Renamed).count()
    }

    fn page_transform_cx(primary: &Path, fallback: Option<&Path>, page: usize, uid: &str) -> Option<f32> {
        persist::load_page_text_nodes(primary, fallback, page)
            .expect("load")
            .into_iter()
            .find(|n| n.uid == uid)
            .and_then(|n| n.inline)
            .and_then(|i| i.transform)
            .map(|t| t.cx)
    }

    /// A pass that coalesced K pages makes exactly ONE commit to a `.db` staging manifest (its
    /// revision advances by one), and every page lands.
    #[test]
    fn k_pages_in_one_pass_are_one_db_commit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (committed, staging) = chapter(tmp.path(), ms_docstore::DocFormat::Db);
        let pages = 0..5usize;
        let (_, failed, _) = run_pass(pages.clone().map(|p| text_job(p, &staging, Some(&committed), vec![text_at("t", 0, 1.0, 1.0, true)], 1)).collect());
        assert!(failed.is_empty());
        assert_eq!(ms_docstore::actual_format(&staging_doc(&staging)).expect("format"), Some(ms_docstore::DocFormat::Db));
        let before = ms_docstore::revision(&staging_doc(&staging)).expect("revision").expect("db revision");

        let (_, failed, acks) = run_pass(pages.clone().map(|p| text_job(p, &staging, Some(&committed), vec![text_at("t", 0, 40.0, 1.0, false)], 2)).collect());
        assert!(failed.is_empty());
        assert!(acks.iter().all(|(_, _, _, ok)| *ok));
        let after = ms_docstore::revision(&staging_doc(&staging)).expect("revision").expect("db revision");
        assert_eq!(after, before + 1, "five changed pages in one pass must be one commit");
        for p in pages {
            assert_eq!(page_transform_cx(&staging, None, p, "t"), Some(40.0));
        }
    }

    /// The same on a JSON chapter: one atomic replacement per pass, and a staging manifest is written
    /// without a directory fsync (it is scratch data; see `chapter_doc_durability`).
    #[test]
    fn k_pages_in_one_pass_are_one_json_write() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (committed, staging) = chapter(tmp.path(), ms_docstore::DocFormat::Json);
        let (_, failed, _) = run_pass((0..6).map(|p| text_job(p, &staging, Some(&committed), vec![text_at("t", 0, 1.0, 1.0, true)], 1)).collect());
        assert!(failed.is_empty());
        assert_eq!(json_writes(&staging), 1, "six pages in one pass must be one manifest write");
        assert!(
            !ms_docstore::recorded_steps(&staging.join("layers.json")).contains(&ms_docstore::WriteStep::DirectoryDurable),
            "a staging manifest is never directory-fsynced"
        );
        assert!(staging.join("layers.json").is_file());
        assert!(!staging.join("layers.db").exists(), "a JSON chapter's staging stays JSON");
    }

    /// When the single manifest write fails, NOTHING of the pass was persisted, so every job reports
    /// every kind it carried as failed — never a silent partial success.
    #[test]
    fn failed_commit_fails_every_job_of_the_pass() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (committed, staging) = chapter(tmp.path(), ms_docstore::DocFormat::Json);
        let mut jobs: Vec<PageSaveJob> = (0..3).map(|p| text_job(p, &staging, Some(&committed), vec![text_at("t", 0, 1.0, 1.0, true)], 10 + p as u64)).collect();
        jobs.push(PageSaveJob { raster_epoch: Some(20), ..full_job(3, &staging, vec![raster("r", Color32::RED)]) });
        ms_docstore::arm_fault(Some(ms_docstore::FaultPoint::TempWrite));
        let (failed_raster, failed_text, acks) = run_pass(jobs);
        ms_docstore::arm_fault(None);
        assert_eq!(failed_text, HashSet::from([0, 1, 2]));
        assert_eq!(failed_raster, HashSet::from([3]));
        assert_eq!(acks.len(), 4);
        assert!(acks.iter().all(|(_, _, _, ok)| !*ok), "every job of a failed commit is reported failed: {acks:?}");
        assert!(!staging.join("layers.json").exists(), "no manifest was written");
    }

    /// A per-page failure (a text PNG that cannot be encoded, a raster that cannot be written) fails
    /// only that page; the rest of the pass still commits, once.
    #[test]
    fn per_page_failure_is_isolated_within_the_pass() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (committed, staging) = chapter(tmp.path(), ms_docstore::DocFormat::Json);
        let mut broken_text = text_at("bad", 0, 1.0, 1.0, true);
        broken_text.image.pixels.truncate(1);
        let mut broken_raster = raster("bad_r", Color32::RED);
        broken_raster.base_image.pixels.truncate(1);
        let jobs = vec![
            text_job(0, &staging, Some(&committed), vec![text_at("t0", 0, 1.0, 1.0, true)], 1),
            text_job(1, &staging, Some(&committed), vec![broken_text], 2),
            text_job(2, &staging, Some(&committed), vec![text_at("t2", 0, 1.0, 1.0, true)], 3),
            PageSaveJob { raster_epoch: Some(4), ..full_job(3, &staging, vec![broken_raster]) },
        ];
        let (failed_raster, failed_text, _) = run_pass(jobs);
        assert_eq!(failed_text, HashSet::from([1]));
        assert_eq!(failed_raster, HashSet::from([3]));
        assert_eq!(json_writes(&staging), 1, "the surviving pages still share one write");
        let manifest = crate::layer_model::compat::read_manifest(&staging.join("layers.json")).expect("read").expect("manifest");
        let pages: Vec<usize> = manifest.pages.iter().map(|p| p.img_idx).collect();
        assert_eq!(pages, vec![0, 2], "failed pages leave no partial record");
    }

    /// Unchanged pages cost no write at all; a delete-only and a placement-only change are written,
    /// and the loader + save-to-project merge see exactly the doc's text for every page.
    #[test]
    fn unchanged_pages_are_not_written_changed_ones_are() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (committed, staging) = chapter(tmp.path(), ms_docstore::DocFormat::Json);
        let nodes = |p: usize| vec![text_at("a", 0, 10.0 + p as f32, 5.0, false), text_at("b", 1, 20.0, 5.0, false)];
        // The committed chapter holds the text (and its PNGs) as the last save left it.
        let committed_jobs = (0..4)
            .map(|p| text_job(p, &committed, None, nodes(p).into_iter().map(|n| OwnedTextNode { pixels_dirty: true, ..n }).collect(), 1))
            .collect();
        assert!(run_pass(committed_jobs).1.is_empty());

        // Save-to-project enqueues every resident page; none changed → no staging manifest at all.
        let (_, failed, acks) = run_pass((0..4).map(|p| text_job(p, &staging, Some(&committed), nodes(p), 2)).collect());
        assert!(failed.is_empty() && acks.iter().all(|(_, _, _, ok)| *ok), "an elided page is a successful save");
        assert!(!staging.join("layers.json").exists() && !staging.join("layers.db").exists(), "unchanged pages write nothing");
        assert_eq!(json_writes(&staging), 0);

        // Page 1: delete-only (b removed). Page 2: placement-only (a moved). Pages 0 and 3 unchanged.
        let jobs = vec![
            text_job(0, &staging, Some(&committed), nodes(0), 3),
            text_job(1, &staging, Some(&committed), vec![nodes(1).remove(0)], 3),
            text_job(2, &staging, Some(&committed), vec![text_at("a", 0, 99.0, 5.0, false), nodes(2).remove(1)], 3),
            text_job(3, &staging, Some(&committed), nodes(3), 3),
        ];
        assert!(run_pass(jobs).1.is_empty());
        assert_eq!(json_writes(&staging), 1);
        let manifest = crate::layer_model::compat::read_manifest(&staging.join("layers.json")).expect("read").expect("manifest");
        let pages: Vec<usize> = manifest.pages.iter().map(|p| p.img_idx).collect();
        assert_eq!(pages, vec![1, 2], "only the changed pages are staged");

        let uids = |dir: &Path, fb: Option<&Path>, p: usize| -> Vec<String> {
            let mut u: Vec<String> = persist::load_page_text_nodes(dir, fb, p).expect("load").into_iter().map(|n| n.uid).collect();
            u.sort();
            u
        };
        assert_eq!(uids(&staging, Some(&committed), 0), vec!["a", "b"]);
        assert_eq!(uids(&staging, Some(&committed), 1), vec!["a"], "the deletion is visible");
        assert_eq!(page_transform_cx(&staging, Some(&committed), 2, "a"), Some(99.0), "the move is visible");

        let owned: HashSet<usize> = (0..4).collect();
        persist::merge_unsaved_layers_into_committed(&committed, &staging, &owned).expect("merge");
        assert_eq!(uids(&committed, None, 0), vec!["a", "b"]);
        assert_eq!(uids(&committed, None, 1), vec!["a"]);
        assert_eq!(page_transform_cx(&committed, None, 2, "a"), Some(99.0));
        assert_eq!(uids(&committed, None, 3), vec!["a", "b"]);
    }

    /// A text job that had to write a PNG into staging is never elided, even when its payload equals
    /// the committed page: the staging manifest must name the fresh file in its own tree.
    #[test]
    fn text_job_that_wrote_a_png_is_not_elided() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (committed, staging) = chapter(tmp.path(), ms_docstore::DocFormat::Json);
        assert!(run_pass(vec![text_job(0, &committed, None, vec![text_at("a", 0, 1.0, 1.0, true)], 1)]).1.is_empty());
        assert!(run_pass(vec![text_job(0, &staging, Some(&committed), vec![text_at("a", 0, 1.0, 1.0, true)], 2)]).1.is_empty());
        assert_eq!(json_writes(&staging), 1, "a re-rendered text is staged");
        assert!(staging.join(persist::text_image_file_name(0, "a")).is_file());
    }

    /// Deleting EVERY raster of a text-less committed page through the saver must persist, both for
    /// the doc's whole-page job (raster + empty text in one transaction — the text half must not
    /// re-seed the committed rasters) and for the PS editor's raster-only job with explicit
    /// `removed_uids`. Then save-to-project must not bring the rasters back.
    #[test]
    fn deleting_every_raster_of_a_textless_page_persists_through_the_saver() {
        for with_text_half in [true, false] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let (committed, staging) = chapter(tmp.path(), ms_docstore::DocFormat::Json);
            assert!(run_pass(vec![full_job(0, &committed, vec![raster("r1", Color32::RED), raster("r2", Color32::BLUE)])]).0.is_empty());
            let rasters = |dir: &Path, fb: Option<&Path>| -> Vec<String> {
                persist::load_page_rasters(dir, fb, 0).expect("load").layers.into_iter().map(|l| l.uid).collect()
            };
            assert_eq!(rasters(&committed, None), vec!["r1", "r2"], "seed");

            let job = PageSaveJob {
                page_idx: 0,
                layers_dir: staging.clone(),
                fallback_dir: Some(committed.clone()),
                raster: Some(RasterSavePart { layers: Vec::new(), groups: Vec::new(), removed_uids: vec!["r1".into(), "r2".into()] }),
                raster_epoch: Some(1),
                text: with_text_half.then(|| TextSavePart { nodes: Vec::new() }),
                text_epoch: with_text_half.then_some(1),
                effects: Vec::new(),
            };
            let (failed_raster, failed_text, _) = run_pass(vec![job]);
            assert!(failed_raster.is_empty() && failed_text.is_empty(), "text half {with_text_half}");
            assert!(rasters(&staging, Some(&committed)).is_empty(), "text half {with_text_half}: staging view must not fall back to committed");

            let owned: HashSet<usize> = [0].into_iter().collect();
            persist::merge_unsaved_layers_into_committed(&committed, &staging, &owned).expect("merge");
            assert!(rasters(&committed, None).is_empty(), "text half {with_text_half}: the deleted rasters must not come back");
        }
    }
}
