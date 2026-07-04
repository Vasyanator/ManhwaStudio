/*
File: models/layer_model/saver.rs

Purpose:
Off-thread, coalescing persistence for the unified layer model. Lets a caller (the shared doc, or a
save-to-project merge worker) hand a fully OWNED page-save job to a background thread so the GUI /
holder never blocks on PNG encode + manifest read-modify-write, and never holds the doc lock during
I/O.

The worker mirrors the EXACT persist sequence of `LayerDoc::flush_page` / `flush_page_text` — it adds
no new write LOGIC, it only moves the existing `persist::*` calls off-thread. Jobs are bucketed per
page index and the LATEST data for each kind (rasters / text) is kept (coalesced), so a burst of
edits to one page collapses into a single on-disk write while a Full + a TextOnly job for the same
page MERGE (neither kind's data is dropped).

Key types:
- `OwnedRasterLayer` / `RasterSavePart` / `TextSavePart` — owned mirrors of the inputs to
  `persist::save_page_rasters` / `persist::update_raster_effects` / `persist::write_page_text_payload`,
  so the worker holds no borrow into the doc.
- `PageSaveJob` — one page's owned save payload (its dirs + optional raster part + optional text part).
- `SaverMsg` — the worker mailbox protocol (`Job` / `Barrier` / `Shutdown`).
- `LayerSaver` — owns the worker thread + its `Sender` and `JoinHandle`.
- `LayerSaverHandle` — a cheap-clone `Sender` wrapper so a merge worker can enqueue / barrier without
  locking the doc.

Notes:
The whole point is that `PageSaveJob` carries OWNED `ColorImage`s, not borrows, so the worker can run
the real `persist::*` write path while the doc is free for the GUI thread.
*/

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use ms_thread::{self as thread, JoinHandle};

use eframe::egui::ColorImage;
use serde_json::Value;

use super::manifest::{DeformRec, TransformRec};
use super::persist::{self, GroupMeta, RasterLayerOut};
use crate::runtime_log;

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
    pub transform: TransformRec,
    pub deform: Option<DeformRec>,
    pub mask_clip: Option<bool>,
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
    pub text: Option<TextSavePart>,
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
        }
        if next.text.is_some() {
            self.text = next.text;
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

    /// Runs this job's persist writes on the calling (worker) thread, mirroring the EXACT sequence of
    /// `LayerDoc::flush_page` / `flush_page_text`:
    /// 1. rasters via `persist::save_page_rasters` (when a raster part is present),
    /// 2. text via `persist::write_page_text_payload` (when a text part is present; PNGs re-encoded
    ///    only when dirty/missing, as in `write_page_text`),
    /// 3. effects reconcile via `persist::update_raster_effects` for every raster with a non-empty
    ///    chain (after rasters, before/after text is irrelevant — they touch different fields).
    ///
    /// # Errors
    /// Returns the first persist error string, identical to the synchronous flush's propagation.
    fn run(&self) -> Result<(), String> {
        let layers_dir = self.layers_dir.as_path();
        let fallback_dir = self.fallback_dir.as_deref();

        // 1) Rasters: build the borrowed `RasterLayerOut`s and call the same writer the sync flush uses.
        if let Some(raster) = &self.raster {
            let outs: Vec<RasterLayerOut<'_>> =
                raster.layers.iter().map(OwnedRasterLayer::as_out).collect();
            persist::save_page_rasters(
                layers_dir,
                self.page_idx,
                &outs,
                &raster.groups,
                &raster.removed_uids,
            )?;
        }

        // 2) Text: reproduce `write_page_text`'s "rewrite PNG iff dirty or missing" rule, then the
        // single text writer.
        if let Some(text) = &self.text {
            let mut text_outs: Vec<persist::TextPayloadOut> = Vec::with_capacity(text.nodes.len());
            for node in &text.nodes {
                let file_name = persist::text_image_file_name(self.page_idx, &node.uid);
                // Presence check via the storage seam (was `Path::is_file`): a deterministic text-PNG
                // name is either present or not, and storage `exists` answers the same question.
                let primary_path = layers_dir.join(&file_name);
                let present = crate::storage::storage()
                    .exists(primary_path.to_string_lossy().as_ref())
                    || fallback_dir.is_some_and(|d| {
                        crate::storage::storage().exists(d.join(&file_name).to_string_lossy().as_ref())
                    });
                let rendered_file = if node.pixels_dirty || !present {
                    Some(persist::write_text_image(
                        layers_dir,
                        self.page_idx,
                        &node.uid,
                        &node.image,
                    )?)
                } else {
                    Some(file_name)
                };
                text_outs.push(persist::TextPayloadOut {
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
                    transform: node.transform,
                    deform: node.deform.clone(),
                    rendered_file,
                    mask_clip: node.mask_clip,
                });
            }
            persist::write_page_text_payload(layers_dir, fallback_dir, self.page_idx, &text_outs)?;
        }

        // 3) Effects reconcile: rewrite the chain + rendered PNG for every raster with a non-empty
        // chain, exactly as `flush_page_inner` does after `save_page_rasters`.
        if let Some(raster) = &self.raster {
            for layer in &raster.layers {
                if !layer.effects.is_empty() {
                    persist::update_raster_effects(
                        layers_dir,
                        self.page_idx,
                        &layer.uid,
                        &layer.effects,
                        layer.display_image.as_ref(),
                        fallback_dir,
                    )?;
                }
            }
        }

        // 3b) Targeted effects-only updates: reconcile a single raster's chain WITHOUT a whole-page
        // raster rewrite. This is the ONLY path that can express the CLEAR case (empty chain +
        // `display_image: None`), which the raster reconcile loop above skips. Mirrors each caller's
        // direct `persist::update_raster_effects` call exactly.
        for item in &self.effects {
            persist::update_raster_effects(
                layers_dir,
                self.page_idx,
                &item.uid,
                &item.effects,
                item.display_image.as_ref(),
                fallback_dir,
            )?;
        }
        Ok(())
    }
}

/// The background saver's mailbox protocol.
pub enum SaverMsg {
    /// Persist one page (coalesced per page in the worker).
    Job(PageSaveJob),
    /// Process every currently-queued job, then signal completion on the sender. Used by
    /// `barrier_blocking` so a caller can be sure all prior enqueued jobs are on disk (e.g. before a
    /// save-to-project merge reads the staging files).
    Barrier(Sender<()>),
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

    /// Blocks until every job enqueued BEFORE this call has been written to disk. Returns immediately
    /// if the worker is gone (the barrier cannot be honored, but the caller must not deadlock); the
    /// loss is logged.
    pub fn barrier_blocking(&self) {
        let (done_tx, done_rx) = mpsc::channel::<()>();
        if self.tx.send(SaverMsg::Barrier(done_tx)).is_err() {
            runtime_log::log_error(
                "[layer_model::saver] barrier failed: background saver thread is gone",
            );
            return;
        }
        // `recv` returns `Err` only if the worker dropped the sender without replying (it panicked
        // mid-drain); treat that as "barrier could not complete" and proceed rather than hang.
        if done_rx.recv().is_err() {
            runtime_log::log_error(
                "[layer_model::saver] barrier sender dropped without reply (worker stopped)",
            );
        }
    }
}

/// Owns the background saver thread, its `Sender`, and its `JoinHandle`. Created with
/// [`LayerSaver::new`]; shut down explicitly with [`LayerSaver::shutdown`] (sentinel + join) or
/// implicitly via the holder's `Drop`.
pub struct LayerSaver {
    tx: Sender<SaverMsg>,
    handle: Option<JoinHandle<()>>,
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
        let handle = thread::spawn(move || worker_loop(&rx));
        LayerSaver {
            tx,
            handle: Some(handle),
        }
    }

    /// A cheap-clone handle a merge worker can use to enqueue / barrier without locking the doc.
    #[must_use]
    pub fn handle(&self) -> LayerSaverHandle {
        LayerSaverHandle {
            tx: self.tx.clone(),
        }
    }

    /// Enqueues a page-save job (see [`LayerSaverHandle::enqueue`]).
    pub fn enqueue(&self, job: PageSaveJob) {
        self.handle().enqueue(job);
    }

    /// Blocks until every previously enqueued job is on disk (see
    /// [`LayerSaverHandle::barrier_blocking`]).
    ///
    /// Production code barriers via a cloned [`LayerSaverHandle`] (the merge worker and app-close
    /// drain hold a handle, not the owner), so this owner-side convenience wrapper has no non-test
    /// caller. It is the symmetric counterpart to [`LayerSaver::enqueue`] and is exercised by this
    /// module's unit tests; the `dead_code` lint is a false-positive for "API completeness used only
    /// in tests" (CLAUDE.md §17 permits an allow when the lint is inapplicable for a stated reason).
    #[allow(dead_code)]
    pub fn barrier_blocking(&self) {
        self.handle().barrier_blocking();
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
/// per kind), running each bucket, honoring `Barrier`/`Shutdown`. A persist error is logged (the
/// next page still runs) — a single bad page must not stall the saver.
fn worker_loop(rx: &Receiver<SaverMsg>) {
    while let Ok(first) = rx.recv() {
        // Per-page coalescing bucket for this drain pass. Insertion order is preserved by tracking the
        // page sequence so writes happen in a deterministic order.
        let mut bucket: HashMap<usize, PageSaveJob> = HashMap::new();
        let mut order: Vec<usize> = Vec::new();
        let mut pending_barriers: Vec<Sender<()>> = Vec::new();
        let mut shutdown = false;

        // Fold the first message, then drain everything immediately available.
        let mut msg = Some(first);
        loop {
            match msg.take() {
                Some(SaverMsg::Job(job)) => {
                    let page = job.page_idx;
                    match bucket.get_mut(&page) {
                        Some(existing) => existing.merge_in_place(job),
                        None => {
                            order.push(page);
                            bucket.insert(page, job);
                        }
                    }
                }
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
        run_bucket(&bucket, &order);
        for done in pending_barriers {
            // The waiter may have given up (timed out / dropped); ignore a closed receiver.
            done.send(()).ok();
        }
        if shutdown {
            break;
        }
    }
}

/// Runs every job in `bucket` following `order` (deterministic write order), logging — not
/// propagating — a per-page persist error so one bad page does not stall the others.
///
/// Each `job.run()` is wrapped in `catch_unwind`: a panic inside a persist call (e.g. an OOM in PNG
/// encoding, or a logic bug) must NOT unwind out of the worker loop and kill the saver thread —
/// that would silently drop all later enqueues and leave every future `barrier_blocking` unable to
/// complete. Catching keeps the worker alive so other pages still save and barriers still reply.
fn run_bucket(bucket: &HashMap<usize, PageSaveJob>, order: &[usize]) {
    for page in order {
        let Some(job) = bucket.get(page) else {
            continue;
        };
        // `AssertUnwindSafe`: `PageSaveJob` is consumed read-only by `run` and dropped after; a panic
        // leaves no observer of a half-mutated value (the on-disk write is the only effect, guarded
        // by persist's own error handling), so asserting unwind-safety is sound here.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run())) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => runtime_log::log_error(format!(
                "[layer_model::saver] failed to persist page {page}: {err}"
            )),
            Err(_) => runtime_log::log_error(format!(
                "[layer_model::saver] PANIC while persisting page {page}; saver thread continues"
            )),
        }
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
        let dir = std::env::temp_dir().join(format!(
            "ms_layer_saver_{tag}_{}_{n}",
            std::process::id()
        ));
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
            transform: tf(5.0, 5.0),
            deform: None,
            mask_clip: None,
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
            text: None,
            effects: Vec::new(),
        }
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
        saver.barrier_blocking();

        let page = persist::load_page_rasters(&dir, None, 5).unwrap();
        let mut uids: Vec<&str> = page.layers.iter().map(|l| l.uid.as_str()).collect();
        uids.sort_unstable();
        assert_eq!(uids, vec!["c", "d"], "only the latest job's rasters persisted");

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
        saver.barrier_blocking();

        // Immediately readable — the barrier guarantees the write completed.
        assert!(dir.join("layers.json").is_file(), "manifest written before barrier returned");
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
            text: Some(TextSavePart {
                nodes: vec![text_node("txt", Color32::BLUE)],
            }),
            effects: Vec::new(),
        });
        saver.barrier_blocking();

        // Raster survives (text-only half did not erase it).
        let rasters = persist::load_page_rasters(&dir, None, 2).unwrap();
        assert_eq!(rasters.layers.len(), 1, "raster preserved through text merge");
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
            text: Some(TextSavePart {
                nodes: vec![text_node("t", Color32::BLUE)],
            }),
            effects: Vec::new(),
        });
        saver.enqueue(full_job(7, &dir, vec![raster("r", Color32::RED)]));
        saver.barrier_blocking();

        let rasters = persist::load_page_rasters(&dir, None, 7).unwrap();
        assert_eq!(rasters.layers.len(), 1);
        assert_eq!(rasters.layers[0].uid, "r");
        let texts = persist::load_page_text_nodes(&dir, None, 7).unwrap();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].uid, "t");

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
            text: None,
            effects: vec![EffectsSaveItem {
                uid: "a".to_string(),
                effects: chain.clone(),
                display_image: Some(img([2, 2], Color32::BLUE)),
            }],
        });
        saver.barrier_blocking();

        let page = persist::load_page_rasters(&dir, None, 4).unwrap();
        assert_eq!(page.layers.len(), 2, "both rasters preserved (no rewrite drop)");
        let a = page.layers.iter().find(|l| l.uid == "a").unwrap();
        assert_eq!(a.effects, chain, "effects-only job set the chain");
        let b = page.layers.iter().find(|l| l.uid == "b").unwrap();
        assert!(b.effects.is_empty(), "other raster untouched by targeted effects update");

        // Effects-only CLEAR for "a": empty chain + no rendered. The raster reconcile loop would skip
        // this (it gates on a non-empty chain), so only the effects-only path can express it.
        saver.enqueue(PageSaveJob {
            page_idx: 4,
            layers_dir: dir.clone(),
            fallback_dir: None,
            raster: None,
            text: None,
            effects: vec![EffectsSaveItem {
                uid: "a".to_string(),
                effects: Vec::new(),
                display_image: None,
            }],
        });
        saver.barrier_blocking();

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
            text: None,
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
            text: None,
            effects: vec![EffectsSaveItem {
                uid: "y".to_string(),
                effects: cy.clone(),
                display_image: Some(img([2, 2], Color32::WHITE)),
            }],
        });
        saver.barrier_blocking();

        let page = persist::load_page_rasters(&dir, None, 1).unwrap();
        let x = page.layers.iter().find(|l| l.uid == "x").unwrap();
        let y = page.layers.iter().find(|l| l.uid == "y").unwrap();
        assert_eq!(x.effects, cx, "x effects survived the coalesce");
        assert_eq!(y.effects, cy, "y effects survived the coalesce (different uid not dropped)");

        saver.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
