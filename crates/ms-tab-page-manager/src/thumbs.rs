/*
File: crates/ms-tab-page-manager/src/thumbs.rs

Purpose:
Background worker + GUI-side LRU caches for the page-manager tab: page thumbnail
decode/downscale, the larger page previews the stitch window draws, clean-overlay
thumbnails downscaled from the in-memory `CleanOverlaysModel`, and the
`layers.json` layer-count scan — all off the GUI thread.

Key structures:
- ThumbRuntime: worker channels, in-flight tracking, and the three caches.
- ThumbCache<T, K>: generic LRU cache (payload- and key-agnostic so the eviction
  logic is unit-testable without GPU textures); keyed by path for files, by page
  index for model-sourced clean thumbnails.
- ModelCleanThumb: what a card may draw for a page's model-sourced clean.
- ThumbJob / ThumbEvent: worker protocol.

Key functions:
- ThumbRuntime::request_thumb_if_needed(): dedup + capped job submission with
  mtime-based revalidation.
- ThumbRuntime::forget_path_thumb(): forces a full re-decode of one file (a rename
  keeps the mtime, so mtime revalidation alone cannot notice it).
- ThumbRuntime::request_preview_if_needed() / preview_state(): the same pair for
  the stitch window's page previews.
- ThumbRuntime::request_model_clean_thumb() / model_clean_thumb(): revision-keyed,
  stale-while-revalidate thumbnails of the model's clean overlays.
- ThumbRuntime::poll(): drains worker events, uploads textures, returns layer scans.
- scan_layer_counts(): merges saved/unsaved `layers.json` into per-page layer counts.

Notes:
The worker mirrors the thumbnail thread of `src/tabs/characters.rs`. Cache key
semantics are (path, mtime): an entry is reused only while the file's mtime is
unchanged; revalidation is triggered by bumping the generation counter
(`PageManagerTabState::notify_pages_changed`).
Thumbnails, previews and model clean thumbnails share the worker, the cancel flag,
the epoch counter and the in-flight cap, but live in SEPARATE caches: a handful of
megapixel-sized previews must never evict the card grid's thumbnails, and
neither may the model clean thumbnails. The two card LRUs start at 64 entries and
grow (never shrink) to twice the cards the grid draws in one frame
(`ensure_visible_capacity`), so a very large viewport cannot thrash them.
Model clean thumbnails receive only an `Arc<RgbaImage>` the caller cloned under a
short model lock; this module never locks the model and never calls
`CleanOverlaysModel::take_delta` (that drain belongs to the canvas). The worker
drops the `Arc` as soon as the downscale is done, because the model
copy-on-writes (`Arc::make_mut`) any page someone else still holds.
*/

use ms_thread::{self as thread, JoinHandle};
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::SystemTime;

use eframe::egui;
use image::RgbaImage;

/// Long side of a decoded thumbnail, in pixels.
pub(super) const THUMB_LONG_SIDE_PX: u32 = 192;
/// Maximum number of thumbnail entries kept in the GUI-side LRU cache.
const THUMB_CACHE_CAPACITY: usize = 64;
/// Maximum thumbnail jobs allowed in flight at once; visible cards above the cap
/// simply retry on a later frame (the tab requests repaints while jobs are pending).
const MAX_IN_FLIGHT_THUMB_JOBS: usize = 8;
/// Default long side of a page preview, in pixels: large enough for the stitch
/// window's zoomed-out board, far below a full-resolution decode.
pub(super) const PREVIEW_LONG_SIDE_PX: u32 = 1024;
/// Long side of the page preview the SPLIT window asks for.
///
/// That window shows ONE page and the user must be able to see a seam to place a
/// cut on it, so 1024 px (~7.8 source px per texel on an 8000 px ribbon) is too
/// coarse. It stays well below a full decode: the worst case is a square page,
/// whose 2048x2048 RGBA texture costs ~16 MB, and a bigger cached preview also
/// answers the stitch window's smaller request (`cached_preview_answers`), so the
/// two windows never fight over the same entry.
pub(super) const SPLIT_PREVIEW_LONG_SIDE_PX: u32 = 2048;
/// Maximum number of preview entries kept in the GUI-side LRU cache. Previews are
/// ~25x the pixels of a thumbnail, so the cache is deliberately small and separate.
///
/// The stitch board draws at most this many live previews at once
/// (`stitch.rs::MAX_LIVE_PREVIEWS` is defined FROM this constant): requesting more
/// than the LRU holds would evict and re-decode them every frame.
pub(super) const PREVIEW_CACHE_CAPACITY: usize = 6;
/// Maximum number of model-sourced clean thumbnails kept in their own LRU, so a
/// grid full of clean cards never evicts the page thumbnails (and vice versa).
const MODEL_CLEAN_CACHE_CAPACITY: usize = 64;

/// Capacity the card-thumbnail LRUs need when one frame draws `drawn_cards` thumbnail-bearing
/// cards (`GridLayout::card_count` of the visible rows plus the prefetch row): twice that, so the
/// band just scrolled out survives a scroll back, and never below the default capacities. Page
/// thumbnails and FILE-sourced clean thumbnails share one path-keyed LRU; if it held fewer entries
/// than one frame requests, every frame would evict and re-decode visible cards in a loop.
#[must_use]
pub(super) fn visible_thumb_capacity(drawn_cards: usize) -> usize {
    drawn_cards.saturating_mul(2).max(THUMB_CACHE_CAPACITY.max(MODEL_CLEAN_CACHE_CAPACITY))
}

/// Which decode a path is currently queued for. Part of the in-flight key so a
/// page can have a thumbnail and a preview pending at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum JobKind {
    Thumb,
    Preview,
}

/// A job for the background worker.
enum ThumbJob {
    /// Decode (or revalidate) the thumbnail of the page image at `path`.
    Thumb {
        path: PathBuf,
        /// mtime of the cache entry the GUI already holds, if any. When the file's
        /// current mtime matches, the worker answers `Unchanged` without decoding.
        known_mtime: Option<SystemTime>,
        /// Generation the request was made for; echoed back so stale entries can be
        /// marked verified.
        generation: u64,
        epoch: u64,
    },
    /// Decode a page preview: the same image downscaled to `long_side_px`, for the
    /// stitch window. Never revalidated by mtime — previews are requested while a
    /// modal dialog is open and the whole cache is dropped when pages change.
    Preview {
        path: PathBuf,
        /// Long side of the produced preview, in pixels.
        long_side_px: u32,
        generation: u64,
        epoch: u64,
    },
    /// Downscale page `page_idx`'s clean overlay, as held by the in-memory model,
    /// to a thumbnail. `rgba` is the model's own `Arc`: the worker drops it right
    /// after the downscale so the model's next edit need not copy the page.
    ModelClean {
        page_idx: usize,
        rgba: Arc<RgbaImage>,
        /// `CleanOverlaysModel::revision` the pixels were taken at; echoed back
        /// as the cache entry's validity stamp.
        revision: u64,
        epoch: u64,
        /// Value of the model-clean epoch at request time; a reply from before
        /// `clear_model_clean_thumbs` refers to pre-reload page indices.
        model_epoch: u64,
    },
    /// Read the saved + unsaved `layers.json` manifests and count layers per page.
    ScanLayers {
        epoch: u64,
        saved_manifest: PathBuf,
        unsaved_manifest: PathBuf,
    },
    /// Terminate the worker loop.
    Stop,
}

/// A worker reply.
enum ThumbEvent {
    /// The file's mtime matches the cached one; the cached thumbnail is still valid.
    Unchanged { path: PathBuf, generation: u64, epoch: u64 },
    /// Freshly decoded thumbnail plus the full image dimensions.
    Loaded {
        path: PathBuf,
        mtime: Option<SystemTime>,
        full_size: (u32, u32),
        thumb_width: usize,
        thumb_height: usize,
        thumb_rgba: Vec<u8>,
        generation: u64,
        epoch: u64,
    },
    /// Decode failed; the error is logged worker-side, the GUI shows a placeholder.
    Failed {
        path: PathBuf,
        mtime: Option<SystemTime>,
        generation: u64,
        epoch: u64,
    },
    /// Freshly decoded page preview plus the full image dimensions.
    PreviewLoaded {
        path: PathBuf,
        mtime: Option<SystemTime>,
        full_size: (u32, u32),
        width: usize,
        height: usize,
        rgba: Vec<u8>,
        /// Long side the preview was produced for; a later request for a bigger
        /// preview of the same page must not be served from this entry.
        long_side_px: u32,
        generation: u64,
        epoch: u64,
    },
    /// Preview decode failed; the error is logged worker-side.
    PreviewFailed {
        path: PathBuf,
        mtime: Option<SystemTime>,
        generation: u64,
        epoch: u64,
    },
    /// Downscaled model clean overlay plus its full dimensions.
    ModelCleanLoaded {
        page_idx: usize,
        revision: u64,
        full_size: (u32, u32),
        width: usize,
        height: usize,
        rgba: Vec<u8>,
        epoch: u64,
        model_epoch: u64,
    },
    /// The model clean overlay could not be downscaled (empty image); logged
    /// worker-side, final for its revision.
    ModelCleanFailed {
        page_idx: usize,
        revision: u64,
        epoch: u64,
        model_epoch: u64,
    },
    /// Per-page layer counts merged from the saved + unsaved manifests.
    LayersScanned {
        epoch: u64,
        counts: HashMap<usize, usize>,
    },
}

/// Visual payload of a cache entry as used by the tab.
pub(super) enum ThumbVisual {
    /// Uploaded texture, ready to draw (thumbnail-sized).
    Ready(egui::TextureHandle),
    /// Decode failed; draw an error placeholder instead of retrying every frame.
    Failed,
}

/// Visual payload of a cached page preview.
pub(super) enum PreviewVisual {
    /// Uploaded texture plus the long side it was decoded for.
    Ready {
        texture: egui::TextureHandle,
        long_side_px: u32,
    },
    /// Decode failed; the caller draws a placeholder instead of retrying every frame.
    Failed,
}

/// What the stitch window can do with a page preview this frame.
pub(super) enum PreviewState {
    /// No entry yet: a job is pending (or was just submitted).
    Pending,
    /// The page could not be decoded.
    Failed,
    /// Ready to draw.
    Ready {
        texture: egui::TextureId,
        /// Size of the preview texture, in points.
        size: egui::Vec2,
        /// Full source image dimensions, when known.
        full_size: Option<(u32, u32)>,
    },
}

/// Maps a cached preview entry to what the caller may draw this frame.
///
/// A missing entry is [`PreviewState::Pending`]: whether a decode is actually in
/// flight is the caller's business (it knows whether it requested one).
fn preview_state_of(entry: Option<&ThumbEntry<PreviewVisual>>) -> PreviewState {
    match entry {
        Some(entry) => match &entry.visual {
            PreviewVisual::Ready { texture, .. } => PreviewState::Ready {
                texture: texture.id(),
                size: texture.size_vec2(),
                full_size: entry.full_size,
            },
            PreviewVisual::Failed => PreviewState::Failed,
        },
        None => PreviewState::Pending,
    }
}

/// Visual payload of a cached model clean thumbnail, stamped with the model
/// revision its pixels were taken at.
pub(crate) struct ModelCleanVisual {
    /// Uploaded texture or a final failure for `revision`.
    pub visual: ThumbVisual,
    /// `CleanOverlaysModel::revision` of the pixels; the entry answers a request
    /// only for exactly this revision.
    pub revision: u64,
}

/// What a clean card can draw for a page's model-sourced clean this frame.
///
/// Stale-while-revalidate: after the model revision moves on, the entry for the
/// OLD revision keeps being returned until the new downscale arrives, so a card
/// never blinks back to a placeholder while its thumbnail is refreshed. A
/// `Failed` entry carries its revision so a caller can tell a stale failure
/// (a newer downscale is on its way) from a current one.
pub(crate) enum ModelCleanThumb {
    /// Nothing cached yet for this page (a downscale may be pending).
    Pending,
    /// The model image could not be downscaled at `revision`.
    Failed { revision: u64 },
    /// Ready to draw.
    Ready {
        texture: egui::TextureId,
        /// Size of the thumbnail texture, in points.
        size: egui::Vec2,
        /// Full overlay dimensions in pixels, `(width, height)`.
        full_size: (u32, u32),
    },
}

/// Maps a cached model clean entry to what the caller may draw this frame.
fn model_clean_thumb_of(entry: Option<&ThumbEntry<ModelCleanVisual>>) -> ModelCleanThumb {
    let Some(entry) = entry else {
        return ModelCleanThumb::Pending;
    };
    let revision = entry.visual.revision;
    match (&entry.visual.visual, entry.full_size) {
        (ThumbVisual::Ready(texture), Some(full_size)) => ModelCleanThumb::Ready {
            texture: texture.id(),
            size: texture.size_vec2(),
            full_size,
        },
        // A texture is only ever inserted together with its full size; a missing
        // one would be a bookkeeping bug, reported as a failure rather than drawn
        // with an invented size.
        (ThumbVisual::Ready(_) | ThumbVisual::Failed, _) => ModelCleanThumb::Failed { revision },
    }
}

/// Whether a model clean thumbnail must be (re)requested: nothing cached for
/// exactly `revision` and no downscale of that page queued yet. At most ONE job
/// per page is queued; a job for an older revision is waited out (its reply is
/// shown stale-while-revalidate, and the next request then submits the new one),
/// which bounds how many model `Arc`s the queue holds.
fn model_clean_request_needed(cached_revision: Option<u64>, in_flight: bool, revision: u64) -> bool {
    !in_flight && cached_revision != Some(revision)
}

/// One cached thumbnail record. `T` is the visual payload (`ThumbVisual` in
/// production, a unit type in the LRU tests).
pub(super) struct ThumbEntry<T> {
    pub visual: T,
    /// mtime the visual was decoded from; part of the (path, mtime) cache key.
    /// Always `None` for model clean thumbnails, which carry a revision instead.
    pub mtime: Option<SystemTime>,
    /// Full source image dimensions, used as a fallback when `page_infos` has no
    /// geometry for the page yet.
    pub full_size: Option<(u32, u32)>,
    /// Last generation this entry was verified against the file's mtime.
    pub verified_generation: u64,
    /// LRU tick of the last access.
    last_used: u64,
}

/// LRU cache keyed by `K` (a page path for file-backed entries, a page index for
/// model clean thumbnails). Capacity-bounded: inserting beyond capacity evicts
/// the least recently used entry (its texture is dropped with it).
pub(super) struct ThumbCache<T, K = PathBuf> {
    entries: HashMap<K, ThumbEntry<T>>,
    tick: u64,
    capacity: usize,
}

impl<T, K: Eq + Hash + Clone> ThumbCache<T, K> {
    /// Creates an empty cache holding at most `capacity` entries.
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            tick: 0,
            capacity,
        }
    }

    /// Raises the capacity to at least `capacity`; never lowers it, so no entry is evicted.
    fn grow_capacity(&mut self, capacity: usize) {
        self.capacity = self.capacity.max(capacity);
    }

    /// Returns the entry for `key`, marking it as most recently used.
    pub(super) fn touch_and_get<Q>(&mut self, key: &Q) -> Option<&ThumbEntry<T>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let entry = self.entries.get_mut(key)?;
        entry.last_used = tick;
        Some(entry)
    }

    /// Returns the entry without touching LRU order (for metadata peeks).
    pub(super) fn peek<Q>(&self, key: &Q) -> Option<&ThumbEntry<T>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.get(key)
    }

    /// Mutable access without touching LRU order.
    fn peek_mut<Q>(&mut self, key: &Q) -> Option<&mut ThumbEntry<T>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.get_mut(key)
    }

    /// Removes the entry for `key`, if any (its texture is dropped with it).
    fn remove<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.remove(key);
    }

    /// Inserts or replaces the entry for `key` and evicts the least recently
    /// used entries while the cache exceeds its capacity.
    pub(super) fn insert(
        &mut self,
        key: K,
        visual: T,
        mtime: Option<SystemTime>,
        full_size: Option<(u32, u32)>,
        verified_generation: u64,
    ) {
        self.tick = self.tick.wrapping_add(1);
        self.entries.insert(
            key,
            ThumbEntry {
                visual,
                mtime,
                full_size,
                verified_generation,
                last_used: self.tick,
            },
        );
        while self.entries.len() > self.capacity {
            // O(n) min-scan is fine at these capacities (64, grown to a few hundred on very large
            // viewports) and only runs on insert overflow.
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    /// Drops every entry (textures are released with their handles).
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Number of cached entries.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether `key` is currently cached.
    #[cfg(test)]
    fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.contains_key(key)
    }
}

/// Worker handle + thumbnail cache owned by the page-manager tab.
pub(super) struct ThumbRuntime {
    tx: Sender<ThumbJob>,
    rx: Receiver<ThumbEvent>,
    worker: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    pub(super) cache: ThumbCache<ThumbVisual>,
    /// Separate, much smaller LRU for the stitch window's page previews.
    preview_cache: ThumbCache<PreviewVisual>,
    /// Separate LRU of model-sourced clean thumbnails, keyed by page index.
    model_clean_cache: ThumbCache<ModelCleanVisual, usize>,
    in_flight: HashSet<(JobKind, PathBuf)>,
    /// Queued model clean downscales: page index -> requested revision. Counts
    /// toward the shared in-flight cap; at most one job per page.
    model_in_flight: HashMap<usize, u64>,
    /// Bumped by `clear_model_clean_thumbs`: page indices may have shifted, so the
    /// worker skips older model jobs and the GUI drops their replies. Shared with
    /// the worker so skipped jobs release their model `Arc` without downscaling.
    model_clean_epoch: Arc<AtomicU64>,
    /// Replies to be dropped because `forget_path_thumb` untracked their job:
    /// (kind, path) -> number of such outstanding jobs. The worker is FIFO, so
    /// these replies arrive before the reply of any job re-issued afterwards.
    discard_replies: HashMap<(JobKind, PathBuf), u32>,
    texture_serial: u64,
}

impl Default for ThumbRuntime {
    fn default() -> Self {
        let (tx_job, rx_job) = mpsc::channel::<ThumbJob>();
        let (tx_event, rx_event) = mpsc::channel::<ThumbEvent>();
        let cancel = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let model_clean_epoch = Arc::new(AtomicU64::new(0));
        let worker_cancel = Arc::clone(&cancel);
        let worker_epoch = Arc::clone(&epoch);
        let worker_model_epoch = Arc::clone(&model_clean_epoch);
        let worker = thread::spawn(move || {
            run_worker(&rx_job, &tx_event, &worker_cancel, &worker_epoch, &worker_model_epoch);
        });
        Self {
            tx: tx_job,
            rx: rx_event,
            worker: Some(worker),
            cancel,
            epoch,
            cache: ThumbCache::new(THUMB_CACHE_CAPACITY),
            preview_cache: ThumbCache::new(PREVIEW_CACHE_CAPACITY),
            model_clean_cache: ThumbCache::new(MODEL_CLEAN_CACHE_CAPACITY),
            in_flight: HashSet::new(),
            model_in_flight: HashMap::new(),
            model_clean_epoch,
            discard_replies: HashMap::new(),
            texture_serial: 0,
        }
    }
}

impl Drop for ThumbRuntime {
    fn drop(&mut self) {
        // Cancellation makes queued decode/scan work cheap to abandon before the Stop sentinel,
        // so the remaining Drop join waits at most for the one job already executing.
        self.cancel.store(true, Ordering::Release);
        let _ = self.tx.send(ThumbJob::Stop);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl ThumbRuntime {
    /// Grows the card-thumbnail LRUs (the path-keyed one and the model-clean one) to
    /// [`visible_thumb_capacity`]`(drawn_cards)`. Grow-only: a smaller viewport later keeps the
    /// larger capacity, so resident memory is bounded by the largest viewport shown so far. The
    /// preview cache is unaffected. Call before the frame's thumbnail requests.
    pub(super) fn ensure_visible_capacity(&mut self, drawn_cards: usize) {
        let capacity = visible_thumb_capacity(drawn_cards);
        self.cache.grow_capacity(capacity);
        self.model_clean_cache.grow_capacity(capacity);
    }

    /// Requests a thumbnail for `path` unless a valid cache entry for
    /// `generation` already exists, the path is already in flight, or the
    /// in-flight cap is reached. Returns `true` when the caller should keep
    /// requesting repaints (a job is pending or was just submitted).
    pub(super) fn request_thumb_if_needed(&mut self, path: &Path, generation: u64) -> bool {
        if self.is_in_flight(JobKind::Thumb, path) {
            return true;
        }
        let known_mtime = match self.cache.peek(path) {
            Some(entry) if entry.verified_generation >= generation => return false,
            Some(entry) => entry.mtime,
            None => None,
        };
        if self.in_flight_count() >= MAX_IN_FLIGHT_THUMB_JOBS {
            // Over the cap: retry on a later frame once some jobs complete.
            return true;
        }
        self.in_flight.insert((JobKind::Thumb, path.to_path_buf()));
        let epoch = self.epoch.load(Ordering::Acquire);
        let _ = self.tx.send(ThumbJob::Thumb {
            path: path.to_path_buf(),
            known_mtime,
            generation,
            epoch,
        });
        true
    }

    /// Requests a `long_side_px` preview of `path` unless a preview at least that
    /// large is already cached for `generation`, the path is already in flight, or
    /// the shared in-flight cap is reached. Returns `true` when the caller should
    /// keep requesting repaints (a job is pending or was just submitted).
    ///
    /// Pair it with [`Self::preview_state`] exactly as the card grid pairs
    /// [`Self::request_thumb_if_needed`] with the thumbnail cache lookup.
    pub(super) fn request_preview_if_needed(
        &mut self,
        path: &Path,
        long_side_px: u32,
        generation: u64,
    ) -> bool {
        if self.is_in_flight(JobKind::Preview, path) {
            return true;
        }
        if let Some(entry) = self.preview_cache.peek(path) {
            let cached_long_side = match &entry.visual {
                PreviewVisual::Ready { long_side_px, .. } => Some(*long_side_px),
                PreviewVisual::Failed => None,
            };
            if cached_preview_answers(
                cached_long_side,
                entry.verified_generation,
                long_side_px,
                generation,
            ) {
                return false;
            }
        }
        if self.in_flight_count() >= MAX_IN_FLIGHT_THUMB_JOBS {
            return true;
        }
        self.in_flight.insert((JobKind::Preview, path.to_path_buf()));
        let epoch = self.epoch.load(Ordering::Acquire);
        let _ = self.tx.send(ThumbJob::Preview {
            path: path.to_path_buf(),
            long_side_px,
            generation,
            epoch,
        });
        true
    }

    /// Returns the drawable state of `path`'s preview, marking it most recently
    /// used. Does NOT submit a job — call [`Self::request_preview_if_needed`] first.
    pub(super) fn preview_state(&mut self, path: &Path) -> PreviewState {
        preview_state_of(self.preview_cache.touch_and_get(path))
    }

    /// Same answer as [`Self::preview_state`], but WITHOUT touching LRU order.
    ///
    /// For a page the caller is not allowed to request a preview for (the stitch
    /// board caps live previews at `PREVIEW_CACHE_CAPACITY`): an entry that is
    /// still cached may be drawn, yet promoting it would let a capped page evict
    /// one of the pages that is actually being previewed.
    pub(super) fn preview_state_cached(&self, path: &Path) -> PreviewState {
        preview_state_of(self.preview_cache.peek(path))
    }

    /// Forces the next [`Self::request_thumb_if_needed`] (and preview request) for
    /// `path` to decode the file from scratch.
    ///
    /// Needed after a rename moved a DIFFERENT file onto `path`: a rename keeps
    /// the file's mtime, so bumping the generation alone would let mtime
    /// revalidation answer `Unchanged` with the old image whenever the two files
    /// happen to share an mtime. Drops the cached thumbnail and preview; a job
    /// already queued for `path` is untracked and its reply discarded, because it
    /// may have read the file before the rename.
    pub(crate) fn forget_path_thumb(&mut self, path: &Path) {
        self.cache.remove(path);
        self.preview_cache.remove(path);
        for kind in [JobKind::Thumb, JobKind::Preview] {
            if self.is_in_flight(kind, path) {
                self.clear_in_flight(kind, path);
                *self.discard_replies.entry((kind, path.to_path_buf())).or_insert(0) += 1;
            }
        }
    }

    /// Consumes one pending discard for a `kind` reply of `path`; `true` means the
    /// reply belongs to a job `forget_path_thumb` untracked and must be dropped
    /// WITHOUT touching the in-flight set (a re-issued job may own that slot).
    fn take_discard(&mut self, kind: JobKind, path: &Path) -> bool {
        if self.discard_replies.is_empty() {
            return false;
        }
        let key = (kind, path.to_path_buf());
        match self.discard_replies.get_mut(&key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                true
            }
            Some(_) => {
                self.discard_replies.remove(&key);
                true
            }
            None => false,
        }
    }

    /// Whether a model clean thumbnail for `page_idx` must be requested for
    /// `revision` (see [`Self::request_model_clean_thumb`]).
    ///
    /// Lets the caller skip locking the model to clone the page's `Arc` on every
    /// frame: clone it only when this returns `true`.
    pub(crate) fn model_clean_thumb_wanted(&self, page_idx: usize, revision: u64) -> bool {
        model_clean_request_needed(
            self.model_clean_cache.peek(&page_idx).map(|entry| entry.visual.revision),
            self.model_in_flight.contains_key(&page_idx),
            revision,
        )
    }

    /// Requests a thumbnail of page `page_idx`'s clean overlay as held by the
    /// in-memory `CleanOverlaysModel`, taken at model `revision`.
    ///
    /// `rgba` is the model's own `Arc`, cloned by the caller under a short lock;
    /// this module never locks the model and never calls `take_delta`. No-op
    /// (and `rgba` is dropped immediately) when an entry for exactly `revision`
    /// is cached, a downscale of this page is already queued, or the shared
    /// in-flight cap is reached. The worker drops `rgba` right after the
    /// downscale. Returns `true` while the caller should keep requesting
    /// repaints (a job is pending or was just submitted).
    pub(crate) fn request_model_clean_thumb(&mut self, page_idx: usize, rgba: Arc<RgbaImage>, revision: u64) -> bool {
        if self.model_in_flight.contains_key(&page_idx) {
            return true;
        }
        if !self.model_clean_thumb_wanted(page_idx, revision) {
            return false;
        }
        if self.in_flight_count() >= MAX_IN_FLIGHT_THUMB_JOBS {
            return true;
        }
        self.model_in_flight.insert(page_idx, revision);
        let _ = self.tx.send(ThumbJob::ModelClean {
            page_idx,
            rgba,
            revision,
            epoch: self.epoch.load(Ordering::Acquire),
            model_epoch: self.model_clean_epoch.load(Ordering::Acquire),
        });
        true
    }

    /// Returns what the card may draw for page `page_idx`'s model clean, marking
    /// the entry most recently used. Stale-while-revalidate: an entry for an
    /// older revision keeps being returned until the new one arrives. Does NOT
    /// submit a job — call [`Self::request_model_clean_thumb`] first.
    pub(crate) fn model_clean_thumb(&mut self, page_idx: usize) -> ModelCleanThumb {
        model_clean_thumb_of(self.model_clean_cache.touch_and_get(&page_idx))
    }

    /// Drops every model clean thumbnail and invalidates queued downscales.
    ///
    /// The entries are keyed by page INDEX, which a structural page operation or
    /// a reload shifts; the caller must therefore call this from
    /// `PageManagerTabState::notify_pages_changed` (and when a different overlays
    /// model is wired). [`Self::reset`] already includes it.
    pub(crate) fn clear_model_clean_thumbs(&mut self) {
        self.model_clean_epoch.fetch_add(1, Ordering::AcqRel);
        self.model_clean_cache.clear();
        self.model_in_flight.clear();
    }

    /// Number of queued decode/downscale jobs of every kind (the shared cap's
    /// measure). Layer scans are not counted: they are rare and uncapped.
    fn in_flight_count(&self) -> usize {
        self.in_flight.len() + self.model_in_flight.len()
    }

    /// Whether a job of `kind` is already queued for `path`.
    ///
    /// Scans linearly on purpose: the set never exceeds `MAX_IN_FLIGHT_THUMB_JOBS`
    /// entries, so this is cheaper than allocating an owned key to hash.
    fn is_in_flight(&self, kind: JobKind, path: &Path) -> bool {
        self.in_flight
            .iter()
            .any(|(job_kind, job_path)| *job_kind == kind && job_path == path)
    }

    /// Marks the `kind` job of `path` as finished. Same linear-scan rationale as
    /// [`Self::is_in_flight`]: no owned key has to be built to look it up.
    fn clear_in_flight(&mut self, kind: JobKind, path: &Path) {
        self.in_flight
            .retain(|(job_kind, job_path)| *job_kind != kind || job_path != path);
    }

    /// Submits a layer-count scan of the two `layers.json` manifests.
    pub(super) fn request_layers_scan(
        &self,
        epoch: u64,
        saved_manifest: PathBuf,
        unsaved_manifest: PathBuf,
    ) {
        let _ = self.tx.send(ThumbJob::ScanLayers {
            epoch,
            saved_manifest,
            unsaved_manifest,
        });
    }

    /// Whether any thumbnail, preview or model clean job is still in flight.
    pub(super) fn has_in_flight(&self) -> bool {
        self.in_flight_count() > 0
    }

    /// Drains worker events: uploads finished thumbnails as textures and returns
    /// completed layer scans as `(epoch, counts)` pairs for the tab to filter.
    pub(super) fn poll(&mut self, ctx: &egui::Context) -> Vec<(u64, HashMap<usize, usize>)> {
        let mut scans = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(ThumbEvent::Unchanged { path, generation, epoch }) => {
                    if epoch != self.epoch.load(Ordering::Acquire) { continue; }
                    if self.take_discard(JobKind::Thumb, &path) { continue; }
                    self.clear_in_flight(JobKind::Thumb, &path);
                    if let Some(entry) = self.cache.peek_mut(&path) {
                        entry.verified_generation = entry.verified_generation.max(generation);
                    }
                }
                Ok(ThumbEvent::Loaded {
                    path,
                    mtime,
                    full_size,
                    thumb_width,
                    thumb_height,
                    thumb_rgba,
                    generation,
                    epoch,
                }) => {
                    if epoch != self.epoch.load(Ordering::Acquire) { continue; }
                    if self.take_discard(JobKind::Thumb, &path) { continue; }
                    self.clear_in_flight(JobKind::Thumb, &path);
                    let color = egui::ColorImage::from_rgba_unmultiplied(
                        [thumb_width, thumb_height],
                        &thumb_rgba,
                    );
                    self.texture_serial = self.texture_serial.wrapping_add(1);
                    let texture = ctx.load_texture(
                        format!("page-manager-thumb-{}", self.texture_serial),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    self.cache.insert(
                        path,
                        ThumbVisual::Ready(texture),
                        mtime,
                        Some(full_size),
                        generation,
                    );
                }
                Ok(ThumbEvent::PreviewLoaded {
                    path,
                    mtime,
                    full_size,
                    width,
                    height,
                    rgba,
                    long_side_px,
                    generation,
                    epoch,
                }) => {
                    if epoch != self.epoch.load(Ordering::Acquire) { continue; }
                    if self.take_discard(JobKind::Preview, &path) { continue; }
                    self.clear_in_flight(JobKind::Preview, &path);
                    let color = egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba);
                    self.texture_serial = self.texture_serial.wrapping_add(1);
                    let texture = ctx.load_texture(
                        format!("page-manager-preview-{}", self.texture_serial),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    self.preview_cache.insert(
                        path,
                        PreviewVisual::Ready {
                            texture,
                            long_side_px,
                        },
                        mtime,
                        Some(full_size),
                        generation,
                    );
                }
                Ok(ThumbEvent::PreviewFailed {
                    path,
                    mtime,
                    generation,
                    epoch,
                }) => {
                    if epoch != self.epoch.load(Ordering::Acquire) { continue; }
                    if self.take_discard(JobKind::Preview, &path) { continue; }
                    self.clear_in_flight(JobKind::Preview, &path);
                    self.preview_cache
                        .insert(path, PreviewVisual::Failed, mtime, None, generation);
                }
                Ok(ThumbEvent::Failed {
                    path,
                    mtime,
                    generation,
                    epoch,
                }) => {
                    if epoch != self.epoch.load(Ordering::Acquire) { continue; }
                    if self.take_discard(JobKind::Thumb, &path) { continue; }
                    self.clear_in_flight(JobKind::Thumb, &path);
                    self.cache
                        .insert(path, ThumbVisual::Failed, mtime, None, generation);
                }
                Ok(ThumbEvent::ModelCleanLoaded {
                    page_idx,
                    revision,
                    full_size,
                    width,
                    height,
                    rgba,
                    epoch,
                    model_epoch,
                }) => {
                    if !self.accept_model_clean_reply(page_idx, revision, epoch, model_epoch) { continue; }
                    let color = egui::ColorImage::from_rgba_unmultiplied([width, height], &rgba);
                    self.texture_serial = self.texture_serial.wrapping_add(1);
                    let texture = ctx.load_texture(
                        format!("page-manager-model-clean-{}", self.texture_serial),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    // Replacing the entry only now is what makes the cache
                    // stale-while-revalidate: the old texture stayed drawable
                    // for the whole downscale.
                    self.model_clean_cache.insert(
                        page_idx,
                        ModelCleanVisual { visual: ThumbVisual::Ready(texture), revision },
                        None,
                        Some(full_size),
                        0,
                    );
                }
                Ok(ThumbEvent::ModelCleanFailed { page_idx, revision, epoch, model_epoch }) => {
                    if !self.accept_model_clean_reply(page_idx, revision, epoch, model_epoch) { continue; }
                    self.model_clean_cache.insert(
                        page_idx,
                        ModelCleanVisual { visual: ThumbVisual::Failed, revision },
                        None,
                        None,
                        0,
                    );
                }
                Ok(ThumbEvent::LayersScanned { epoch, counts }) => {
                    scans.push((epoch, counts));
                }
                Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        scans
    }

    /// Whether a model clean reply is current: from the active worker epoch and
    /// model-clean epoch. A current reply releases its page's in-flight slot
    /// (only when it answers the queued revision, which it always does while at
    /// most one job per page is queued).
    fn accept_model_clean_reply(&mut self, page_idx: usize, revision: u64, epoch: u64, model_epoch: u64) -> bool {
        if epoch != self.epoch.load(Ordering::Acquire) || model_epoch != self.model_clean_epoch.load(Ordering::Acquire) {
            return false;
        }
        if self.model_in_flight.get(&page_idx) == Some(&revision) {
            self.model_in_flight.remove(&page_idx);
        }
        true
    }

    /// Drops every cache and invalidates queued/in-flight replies by epoch.
    pub(super) fn reset(&mut self) {
        // Invalidates queued and already-produced replies without uploading stale textures.
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.cache.clear();
        self.preview_cache.clear();
        self.in_flight.clear();
        // Every discarded reply carries the old epoch now, so none is left to count.
        self.discard_replies.clear();
        self.clear_model_clean_thumbs();
    }
}

/// Worker loop: sequentially (FIFO) serves thumbnail decodes, model clean
/// downscales and manifest scans until `Stop` is received or the job channel
/// disconnects. `active_model_epoch` lets it skip model clean jobs queued before
/// `ThumbRuntime::clear_model_clean_thumbs`.
fn run_worker(
    rx_job: &Receiver<ThumbJob>,
    tx_event: &Sender<ThumbEvent>,
    cancel: &AtomicBool,
    active_epoch: &AtomicU64,
    active_model_epoch: &AtomicU64,
) {
    while let Ok(job) = rx_job.recv() {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        match job {
            ThumbJob::Stop => break,
            ThumbJob::Thumb {
                path,
                known_mtime,
                generation,
                epoch,
            } => {
                if epoch != active_epoch.load(Ordering::Acquire) { continue; }
                let mtime = std::fs::metadata(&path)
                    .ok()
                    .and_then(|meta| meta.modified().ok());
                if known_mtime.is_some() && mtime.is_some() && known_mtime == mtime {
                    let _ = tx_event.send(ThumbEvent::Unchanged { path, generation, epoch });
                    continue;
                }
                match decode_downscaled(&path, THUMB_LONG_SIDE_PX) {
                    Ok(decoded) => {
                        let _ = tx_event.send(ThumbEvent::Loaded {
                            path,
                            mtime,
                            full_size: decoded.full_size,
                            thumb_width: decoded.width,
                            thumb_height: decoded.height,
                            thumb_rgba: decoded.rgba,
                            generation,
                            epoch,
                        });
                    }
                    Err(err) => {
                        ms_log::runtime_log::log_warn(format!(
                            "[page_manager] thumbnail decode failed\nPath: {}\nError: {err}",
                            path.display()
                        ));
                        let _ = tx_event.send(ThumbEvent::Failed {
                            path,
                            mtime,
                            generation,
                            epoch,
                        });
                    }
                }
            }
            ThumbJob::Preview {
                path,
                long_side_px,
                generation,
                epoch,
            } => {
                if epoch != active_epoch.load(Ordering::Acquire) { continue; }
                let mtime = std::fs::metadata(&path)
                    .ok()
                    .and_then(|meta| meta.modified().ok());
                match decode_downscaled(&path, long_side_px) {
                    Ok(decoded) => {
                        let _ = tx_event.send(ThumbEvent::PreviewLoaded {
                            path,
                            mtime,
                            full_size: decoded.full_size,
                            width: decoded.width,
                            height: decoded.height,
                            rgba: decoded.rgba,
                            long_side_px,
                            generation,
                            epoch,
                        });
                    }
                    Err(err) => {
                        ms_log::runtime_log::log_warn(format!(
                            "[page_manager] page preview decode failed\nPath: {}\nLong side: {long_side_px} px\nError: {err}",
                            path.display()
                        ));
                        let _ = tx_event.send(ThumbEvent::PreviewFailed {
                            path,
                            mtime,
                            generation,
                            epoch,
                        });
                    }
                }
            }
            ThumbJob::ModelClean {
                page_idx,
                rgba,
                revision,
                epoch,
                model_epoch,
            } => {
                // A skipped job releases the model's Arc here, at the end of the arm.
                if epoch != active_epoch.load(Ordering::Acquire)
                    || model_epoch != active_model_epoch.load(Ordering::Acquire)
                {
                    continue;
                }
                let full_size = rgba.dimensions();
                let result = downscale_rgba(&rgba, THUMB_LONG_SIDE_PX);
                // Release the model's page before the send: every edit made while
                // this clone is alive would force the model to copy the whole page.
                drop(rgba);
                match result {
                    Ok(decoded) => {
                        let _ = tx_event.send(ThumbEvent::ModelCleanLoaded {
                            page_idx,
                            revision,
                            full_size,
                            width: decoded.width,
                            height: decoded.height,
                            rgba: decoded.rgba,
                            epoch,
                            model_epoch,
                        });
                    }
                    Err(err) => {
                        ms_log::runtime_log::log_warn(format!(
                            "[page_manager] clean overlay thumbnail downscale failed\nPage index: {page_idx}\nModel revision: {revision}\nOverlay size: {}x{}\nError: {err}",
                            full_size.0, full_size.1
                        ));
                        let _ = tx_event.send(ThumbEvent::ModelCleanFailed {
                            page_idx,
                            revision,
                            epoch,
                            model_epoch,
                        });
                    }
                }
            }
            ThumbJob::ScanLayers {
                epoch,
                saved_manifest,
                unsaved_manifest,
            } => {
                let counts = scan_layer_counts(&saved_manifest, &unsaved_manifest);
                let _ = tx_event.send(ThumbEvent::LayersScanned { epoch, counts });
            }
        }
    }
}

/// Whether a cached preview entry already answers a request.
///
/// `cached_long_side` is the long side the entry was decoded at, or `None` for a
/// cached decode FAILURE — which is a final answer for its generation, so the
/// window shows a placeholder instead of re-queueing a doomed decode every frame.
/// An entry verified for an older generation never answers: `notify_pages_changed`
/// bumps the generation exactly because the files may have moved underneath.
fn cached_preview_answers(
    cached_long_side: Option<u32>,
    verified_generation: u64,
    requested_long_side: u32,
    generation: u64,
) -> bool {
    if verified_generation < generation {
        return false;
    }
    match cached_long_side {
        None => true,
        // A bigger preview serves a smaller request; a smaller one must be redecoded.
        Some(cached) => cached >= requested_long_side,
    }
}

/// Result of a successful decode: the FULL source dimensions plus the downscaled
/// RGBA buffer and its size in pixels.
struct DecodedImageData {
    full_size: (u32, u32),
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

/// Decodes the image at `path` and scales it so its long side is `long_side_px`,
/// preserving the aspect ratio. Alpha is kept (the buffer is straight RGBA), so a
/// transparent clean file stays transparent.
///
/// Note that `DynamicImage::thumbnail` scales by `min(long/w, long/h)` with no
/// clamp to 1 (image-0.25.10 `src/math/utils.rs` `resize_dimensions`), so an image
/// smaller than the bound is UPSCALED to it by the same box/nearest sampler.
///
/// # Errors
/// Returns the decode error message when the file cannot be opened or decoded.
fn decode_downscaled(path: &Path, long_side_px: u32) -> Result<DecodedImageData, String> {
    let img = image::open(path).map_err(|err| err.to_string())?;
    let full_size = (img.width(), img.height());
    let scaled = img.thumbnail(long_side_px, long_side_px).to_rgba8();
    let width = usize::try_from(scaled.width()).map_err(|err| err.to_string())?;
    let height = usize::try_from(scaled.height()).map_err(|err| err.to_string())?;
    Ok(DecodedImageData {
        full_size,
        width,
        height,
        rgba: scaled.into_raw(),
    })
}

/// Downscales an in-memory RGBA image exactly the way [`decode_downscaled`]
/// downscales a decoded file, WITHOUT copying the source: the same target size
/// ([`thumbnail_dimensions`]) and the same sampler (`imageops::thumbnail`, which
/// `DynamicImage::thumbnail` delegates to).
///
/// # Errors
/// Returns a message when the image is empty (0 width or height).
fn downscale_rgba(image: &RgbaImage, long_side_px: u32) -> Result<DecodedImageData, String> {
    let (width, height) = image.dimensions();
    let (target_width, target_height) = thumbnail_dimensions(width, height, long_side_px)
        .ok_or_else(|| format!("empty image ({width}x{height})"))?;
    let scaled = image::imageops::thumbnail(image, target_width, target_height);
    let out_width = usize::try_from(scaled.width()).map_err(|err| err.to_string())?;
    let out_height = usize::try_from(scaled.height()).map_err(|err| err.to_string())?;
    Ok(DecodedImageData {
        full_size: (width, height),
        width: out_width,
        height: out_height,
        rgba: scaled.into_raw(),
    })
}

/// Target size of a thumbnail whose long side is `long_side_px`: a restatement of
/// image's private `resize_dimensions(w, h, long, long, fill = false)`
/// (image-0.25.10 `src/math/utils.rs`), which `DynamicImage::thumbnail` uses, so
/// model clean thumbnails come out the same size as file thumbnails. Returns
/// `None` for an empty image or a zero bound.
fn thumbnail_dimensions(width: u32, height: u32, long_side_px: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || long_side_px == 0 {
        return None;
    }
    let ratio = f64::min(
        f64::from(long_side_px) / f64::from(width),
        f64::from(long_side_px) / f64::from(height),
    );
    // `ratio <= long/dim`, so `dim * ratio` rounds into `0..=long_side_px` and the
    // casts below can neither truncate nor lose a sign (the `max(1)` mirrors image).
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "value proven within 0..=long_side_px above")]
    let scale = |dim: u32| ((f64::from(dim) * ratio).round() as u32).clamp(1, long_side_px);
    Some((scale(width), scale(height)))
}

/// Reads the saved and unsaved `layers.json` manifests and returns the layer
/// count (`tree.len()`) per page index. Unsaved page entries override saved ones
/// (page-granular staging, matching how the layer loader resolves pages). A
/// missing manifest contributes nothing; a corrupt one is logged and skipped.
fn scan_layer_counts(saved_manifest: &Path, unsaved_manifest: &Path) -> HashMap<usize, usize> {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for (path, is_unsaved) in [(saved_manifest, false), (unsaved_manifest, true)] {
        match ms_models::layer_model::compat::read_manifest(path) {
            Ok(Some(manifest)) => {
                for page in &manifest.pages {
                    // Later (unsaved) entries replace earlier (saved) ones per page.
                    counts.insert(page.img_idx, page.tree.len());
                }
            }
            Ok(None) => {}
            Err(err) => {
                ms_log::runtime_log::log_warn(format!(
                    "[page_manager] failed to read layers manifest (unsaved={is_unsaved})\nPath: {}\nError: {err}",
                    path.display()
                ));
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(cache: &mut ThumbCache<()>, name: &str) {
        cache.insert(PathBuf::from(name), (), None, None, 0);
    }

    #[test]
    fn visible_capacity_covers_twice_the_drawn_cards_and_never_shrinks() {
        assert_eq!(visible_thumb_capacity(0), THUMB_CACHE_CAPACITY);
        assert_eq!(visible_thumb_capacity(10), THUMB_CACHE_CAPACITY);
        assert_eq!(visible_thumb_capacity(90), 180);
        assert_eq!(visible_thumb_capacity(usize::MAX), usize::MAX);

        let mut cache: ThumbCache<()> = ThumbCache::new(2);
        cache.grow_capacity(3);
        insert(&mut cache, "a");
        insert(&mut cache, "b");
        insert(&mut cache, "c");
        assert!(cache.peek(Path::new("a")).is_some(), "grown capacity holds all three");
        // Growing to a smaller value is a no-op: nothing is evicted.
        cache.grow_capacity(1);
        insert(&mut cache, "c");
        assert_eq!(cache.entries.len(), 3);
        insert(&mut cache, "d");
        assert!(cache.peek(Path::new("a")).is_none(), "LRU evicted at the grown capacity");
        assert_eq!(cache.entries.len(), 3);
    }

    #[test]
    fn lru_evicts_least_recently_used_on_overflow() {
        let mut cache: ThumbCache<()> = ThumbCache::new(2);
        insert(&mut cache, "a");
        insert(&mut cache, "b");
        // Touch "a" so "b" becomes the LRU entry.
        assert!(cache.touch_and_get(Path::new("a")).is_some());
        insert(&mut cache, "c");
        assert_eq!(cache.len(), 2);
        assert!(cache.contains(Path::new("a")));
        assert!(!cache.contains(Path::new("b")));
        assert!(cache.contains(Path::new("c")));
    }

    #[test]
    fn reinsert_replaces_without_growth() {
        let mut cache: ThumbCache<()> = ThumbCache::new(2);
        insert(&mut cache, "a");
        insert(&mut cache, "a");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cached_preview_answers_only_a_current_and_large_enough_entry() {
        // Same generation, decoded at least as large: reuse.
        assert!(cached_preview_answers(Some(1024), 3, 1024, 3));
        assert!(cached_preview_answers(Some(2048), 3, 1024, 3));
        // Decoded smaller than requested: redecode.
        assert!(!cached_preview_answers(Some(512), 3, 1024, 3));
        // Stale generation: redecode even if the size fits.
        assert!(!cached_preview_answers(Some(2048), 2, 1024, 3));
        // A cached failure is final for its generation, but not across one.
        assert!(cached_preview_answers(None, 3, 1024, 3));
        assert!(!cached_preview_answers(None, 2, 1024, 3));
    }

    #[test]
    fn touch_missing_returns_none() {
        let mut cache: ThumbCache<()> = ThumbCache::new(2);
        assert!(cache.touch_and_get(Path::new("missing")).is_none());
    }

    use std::time::{Duration, Instant};

    /// Polls `runtime` until `done` holds, failing the test after 10 s.
    fn poll_until(runtime: &mut ThumbRuntime, ctx: &egui::Context, mut done: impl FnMut(&mut ThumbRuntime) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let _scans = runtime.poll(ctx);
            if done(runtime) {
                return;
            }
            assert!(Instant::now() < deadline, "worker reply did not arrive in time");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Opaque-red test image of `width` x `height` with alpha 0 in the top-left pixel.
    fn test_rgba(width: u32, height: u32) -> RgbaImage {
        let mut image = RgbaImage::from_pixel(width, height, image::Rgba([255, 0, 0, 255]));
        image.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        image
    }

    /// `(revision, full_size)` of a Ready model clean entry, `None` otherwise. The revision is
    /// read from the cache entry the Ready answer was built from.
    fn ready_model_clean(runtime: &mut ThumbRuntime, page_idx: usize) -> Option<(u64, (u32, u32))> {
        match runtime.model_clean_thumb(page_idx) {
            ModelCleanThumb::Ready { full_size, .. } => runtime.model_clean_cache.peek(&page_idx).map(|entry| (entry.visual.revision, full_size)),
            ModelCleanThumb::Pending | ModelCleanThumb::Failed { .. } => None,
        }
    }

    #[test]
    fn model_clean_request_is_keyed_by_exact_revision_and_deduplicated() {
        // Nothing cached: request.
        assert!(model_clean_request_needed(None, false, 1));
        // Cached for exactly this revision: valid, no request.
        assert!(!model_clean_request_needed(Some(1), false, 1));
        // Any other revision (newer, or older after a model swap) is invalid.
        assert!(model_clean_request_needed(Some(1), false, 2));
        assert!(model_clean_request_needed(Some(3), false, 2));
        // One job per page: an in-flight downscale is waited out.
        assert!(!model_clean_request_needed(None, true, 1));
        assert!(!model_clean_request_needed(Some(1), true, 2));
    }

    #[test]
    fn thumbnail_dimensions_match_dynamic_image_thumbnail() {
        for (width, height) in [(1000, 300), (300, 1000), (193, 97), (7, 3), (100, 50), (1, 5000), (192, 192)] {
            let expected = image::DynamicImage::new_rgba8(width, height).thumbnail(THUMB_LONG_SIDE_PX, THUMB_LONG_SIDE_PX);
            assert_eq!(
                thumbnail_dimensions(width, height, THUMB_LONG_SIDE_PX),
                Some((expected.width(), expected.height())),
                "{width}x{height}"
            );
        }
        assert_eq!(thumbnail_dimensions(0, 10, THUMB_LONG_SIDE_PX), None);
        assert_eq!(thumbnail_dimensions(10, 10, 0), None);
    }

    #[test]
    fn downscale_rgba_matches_file_thumbnail_and_keeps_alpha() {
        let source = test_rgba(600, 400);
        let decoded = downscale_rgba(&source, THUMB_LONG_SIDE_PX).expect("non-empty image downscales");
        let expected = image::DynamicImage::ImageRgba8(source.clone()).thumbnail(THUMB_LONG_SIDE_PX, THUMB_LONG_SIDE_PX).to_rgba8();
        assert_eq!(decoded.full_size, (600, 400));
        assert_eq!((decoded.width, decoded.height), (192, 128));
        assert_eq!(decoded.rgba, expected.into_raw());
        // The transparent corner survives the box filter only partially, but
        // alpha must not be flattened to opaque.
        assert!(decoded.rgba[3] < 255);
        assert!(downscale_rgba(&RgbaImage::new(0, 0), THUMB_LONG_SIDE_PX).is_err());
    }

    /// Scratch directory unique to this test process and `name`.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ms_pm_thumbs_{}_{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn file_thumbnail_decode_keeps_png_alpha() {
        let dir = scratch_dir("alpha");
        let path = dir.join("clean.png");
        RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 0])).save(&path).expect("write png");
        let decoded = decode_downscaled(&path, THUMB_LONG_SIDE_PX).expect("decode png");
        assert!(decoded.rgba.chunks_exact(4).all(|px| px[3] == 0), "transparent PNG must stay transparent");
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }

    #[test]
    fn model_clean_thumb_is_stale_while_revalidate_and_deduplicated() {
        let ctx = egui::Context::default();
        let mut runtime = ThumbRuntime::default();
        assert!(matches!(runtime.model_clean_thumb(0), ModelCleanThumb::Pending));

        assert!(runtime.request_model_clean_thumb(0, Arc::new(test_rgba(40, 20)), 1));
        poll_until(&mut runtime, &ctx, |rt| ready_model_clean(rt, 0).is_some());
        assert_eq!(ready_model_clean(&mut runtime, 0), Some((1, (40, 20))));
        assert!(!runtime.has_in_flight());
        // Same long side as a page thumbnail (a small overlay is scaled up to it,
        // exactly like `DynamicImage::thumbnail` does for files).
        match runtime.model_clean_thumb(0) {
            ModelCleanThumb::Ready { texture, size, .. } => {
                assert!(matches!(texture, egui::TextureId::Managed(_)));
                assert_eq!(size, egui::vec2(192.0, 96.0));
            }
            ModelCleanThumb::Pending | ModelCleanThumb::Failed { .. } => panic!("thumbnail must be ready"),
        }

        // Same revision: valid, nothing is submitted and the Arc is released.
        let same = Arc::new(test_rgba(40, 20));
        assert!(!runtime.model_clean_thumb_wanted(0, 1));
        assert!(!runtime.request_model_clean_thumb(0, Arc::clone(&same), 1));
        assert_eq!(Arc::strong_count(&same), 1);
        assert!(!runtime.has_in_flight());

        // New revision: queued once; a second request for it is deduplicated.
        let newer = Arc::new(test_rgba(30, 60));
        assert!(runtime.request_model_clean_thumb(0, Arc::clone(&newer), 2));
        assert!(runtime.request_model_clean_thumb(0, Arc::clone(&newer), 2));
        assert_eq!(runtime.model_in_flight.len(), 1);
        // Until the reply is polled, the old revision stays drawable.
        assert_eq!(ready_model_clean(&mut runtime, 0), Some((1, (40, 20))));
        poll_until(&mut runtime, &ctx, |rt| ready_model_clean(rt, 0).is_some_and(|(revision, _)| revision == 2));
        assert_eq!(ready_model_clean(&mut runtime, 0), Some((2, (30, 60))));
        // The worker dropped its clone as soon as the downscale was done.
        assert_eq!(Arc::strong_count(&newer), 1);

        // An empty overlay is a final failure for its revision, not a retry loop.
        assert!(runtime.request_model_clean_thumb(1, Arc::new(RgbaImage::new(0, 0)), 2));
        poll_until(&mut runtime, &ctx, |rt| matches!(rt.model_clean_thumb(1), ModelCleanThumb::Failed { revision: 2 }));
        assert!(!runtime.model_clean_thumb_wanted(1, 2));
    }

    #[test]
    fn stale_model_clean_replies_are_dropped_after_reset_or_clear() {
        let ctx = egui::Context::default();
        let mut runtime = ThumbRuntime::default();

        // Worker epoch: a reply queued before `reset` never lands.
        assert!(runtime.request_model_clean_thumb(0, Arc::new(test_rgba(10, 10)), 1));
        runtime.reset();
        assert!(!runtime.has_in_flight());
        assert!(runtime.request_model_clean_thumb(0, Arc::new(test_rgba(20, 10)), 1));
        poll_until(&mut runtime, &ctx, |rt| ready_model_clean(rt, 0).is_some());
        assert_eq!(ready_model_clean(&mut runtime, 0), Some((1, (20, 10))));

        // Model-clean epoch: after `clear_model_clean_thumbs` the SAME revision is
        // requested again (indices shifted) and only the new pixels may land.
        assert!(runtime.request_model_clean_thumb(3, Arc::new(test_rgba(10, 10)), 7));
        runtime.clear_model_clean_thumbs();
        assert!(matches!(runtime.model_clean_thumb(0), ModelCleanThumb::Pending));
        assert!(runtime.model_clean_thumb_wanted(3, 7));
        assert!(runtime.request_model_clean_thumb(3, Arc::new(test_rgba(50, 25)), 7));
        poll_until(&mut runtime, &ctx, |rt| ready_model_clean(rt, 3).is_some());
        assert_eq!(ready_model_clean(&mut runtime, 3), Some((7, (50, 25))));
        assert!(!runtime.has_in_flight());
    }

    #[test]
    fn model_clean_cache_is_separate_from_page_thumbnails() {
        let mut runtime = ThumbRuntime::default();
        for idx in 0..THUMB_CACHE_CAPACITY {
            runtime.cache.insert(PathBuf::from(format!("page_{idx}.png")), ThumbVisual::Failed, None, None, 0);
        }
        for idx in 0..MODEL_CLEAN_CACHE_CAPACITY + 5 {
            runtime.model_clean_cache.insert(idx, ModelCleanVisual { visual: ThumbVisual::Failed, revision: 1 }, None, None, 0);
        }
        assert_eq!(runtime.cache.len(), THUMB_CACHE_CAPACITY);
        assert!(runtime.cache.contains(Path::new("page_0.png")));
        assert_eq!(runtime.model_clean_cache.len(), MODEL_CLEAN_CACHE_CAPACITY);
        assert!(!runtime.model_clean_cache.contains(&0));

        // Clearing model thumbnails leaves the page thumbnails alone.
        runtime.clear_model_clean_thumbs();
        assert_eq!(runtime.cache.len(), THUMB_CACHE_CAPACITY);
        assert_eq!(runtime.model_clean_cache.len(), 0);
    }

    #[test]
    fn model_clean_jobs_count_toward_the_shared_in_flight_cap() {
        let mut runtime = ThumbRuntime::default();
        for idx in 0..MAX_IN_FLIGHT_THUMB_JOBS {
            runtime.model_in_flight.insert(idx, 1);
        }
        let image = Arc::new(test_rgba(4, 4));
        // Over the cap: not submitted (caller retries), the Arc is not retained.
        assert!(runtime.request_model_clean_thumb(100, Arc::clone(&image), 1));
        assert!(!runtime.model_in_flight.contains_key(&100));
        assert_eq!(Arc::strong_count(&image), 1);
        assert!(runtime.request_thumb_if_needed(Path::new("page.png"), 0));
        assert!(!runtime.is_in_flight(JobKind::Thumb, Path::new("page.png")));
    }

    #[test]
    fn forget_path_thumb_forces_redecode_after_a_rename_keeping_mtime() {
        let ctx = egui::Context::default();
        let mut runtime = ThumbRuntime::default();
        let dir = scratch_dir("rename");
        let path = dir.join("003.png");
        let other = dir.join("003_detached.png");
        RgbaImage::new(4, 2).save(&path).expect("write first png");
        let mtime = std::fs::metadata(&path).and_then(|meta| meta.modified()).expect("read mtime");
        let full_size = |rt: &ThumbRuntime| rt.cache.peek(Path::new(&path)).and_then(|entry| entry.full_size);

        assert!(runtime.request_thumb_if_needed(&path, 0));
        poll_until(&mut runtime, &ctx, |rt| !rt.has_in_flight());
        assert_eq!(full_size(&runtime), Some((4, 2)));

        // A different file with the same mtime is renamed onto the path.
        RgbaImage::new(8, 8).save(&other).expect("write second png");
        std::fs::File::options().write(true).open(&other).and_then(|file| file.set_modified(mtime)).expect("set mtime");
        std::fs::rename(&other, &path).expect("rename");

        // A generation bump alone answers `Unchanged`: mtime revalidation is blind to it.
        assert!(runtime.request_thumb_if_needed(&path, 1));
        poll_until(&mut runtime, &ctx, |rt| !rt.has_in_flight());
        assert_eq!(full_size(&runtime), Some((4, 2)));

        // Forgetting forces a full decode, even with a job already queued: that
        // job's reply is discarded and the re-issued one lands.
        assert!(runtime.request_thumb_if_needed(&path, 2));
        runtime.forget_path_thumb(&path);
        assert!(runtime.cache.peek(Path::new(&path)).is_none());
        assert!(runtime.request_thumb_if_needed(&path, 2));
        poll_until(&mut runtime, &ctx, |rt| !rt.has_in_flight());
        assert_eq!(full_size(&runtime), Some((8, 8)));
        assert!(runtime.discard_replies.is_empty());
        std::fs::remove_dir_all(&dir).expect("remove scratch dir");
    }
}
