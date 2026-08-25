/*
File: tabs/page_manager/thumbs.rs

Purpose:
Background worker + GUI-side LRU caches for the page-manager tab: page thumbnail
decode/downscale, the larger page previews the stitch window draws, and the
`layers.json` layer-count scan — all off the GUI thread.

Key structures:
- ThumbRuntime: worker channels, in-flight tracking, and the two caches.
- ThumbCache<T>: generic LRU cache keyed by page path (payload-agnostic so the
  eviction logic is unit-testable without GPU textures).
- ThumbJob / ThumbEvent: worker protocol.

Key functions:
- ThumbRuntime::request_thumb_if_needed(): dedup + capped job submission with
  mtime-based revalidation.
- ThumbRuntime::request_preview_if_needed() / preview_state(): the same pair for
  the stitch window's page previews.
- ThumbRuntime::poll(): drains worker events, uploads textures, returns layer scans.
- scan_layer_counts(): merges saved/unsaved `layers.json` into per-page layer counts.

Notes:
The worker mirrors the thumbnail thread of `src/tabs/characters.rs`. Cache key
semantics are (path, mtime): an entry is reused only while the file's mtime is
unchanged; revalidation is triggered by bumping the generation counter
(`PageManagerTabState::notify_pages_changed`).
Thumbnails and previews share the worker, the cancel flag, the epoch counter and
the in-flight cap, but live in SEPARATE caches: a handful of megapixel-sized
previews must never evict the card grid's 64 thumbnails.
*/

use ms_thread::{self as thread, JoinHandle};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::SystemTime;

use eframe::egui;

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

/// One cached thumbnail record. `T` is the visual payload (`ThumbVisual` in
/// production, a unit type in the LRU tests).
pub(super) struct ThumbEntry<T> {
    pub visual: T,
    /// mtime the visual was decoded from; part of the (path, mtime) cache key.
    pub mtime: Option<SystemTime>,
    /// Full source image dimensions, used as a fallback when `page_infos` has no
    /// geometry for the page yet.
    pub full_size: Option<(u32, u32)>,
    /// Last generation this entry was verified against the file's mtime.
    pub verified_generation: u64,
    /// LRU tick of the last access.
    last_used: u64,
}

/// LRU cache keyed by page path. Capacity-bounded: inserting beyond capacity
/// evicts the least recently used entry (its texture is dropped with it).
pub(super) struct ThumbCache<T> {
    entries: HashMap<PathBuf, ThumbEntry<T>>,
    tick: u64,
    capacity: usize,
}

impl<T> ThumbCache<T> {
    /// Creates an empty cache holding at most `capacity` entries.
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            tick: 0,
            capacity,
        }
    }

    /// Returns the entry for `path`, marking it as most recently used.
    pub(super) fn touch_and_get(&mut self, path: &Path) -> Option<&ThumbEntry<T>> {
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let entry = self.entries.get_mut(path)?;
        entry.last_used = tick;
        Some(entry)
    }

    /// Returns the entry without touching LRU order (for metadata peeks).
    pub(super) fn peek(&self, path: &Path) -> Option<&ThumbEntry<T>> {
        self.entries.get(path)
    }

    /// Mutable access without touching LRU order.
    fn peek_mut(&mut self, path: &Path) -> Option<&mut ThumbEntry<T>> {
        self.entries.get_mut(path)
    }

    /// Inserts or replaces the entry for `path` and evicts the least recently
    /// used entries while the cache exceeds its capacity.
    pub(super) fn insert(
        &mut self,
        path: PathBuf,
        visual: T,
        mtime: Option<SystemTime>,
        full_size: Option<(u32, u32)>,
        verified_generation: u64,
    ) {
        self.tick = self.tick.wrapping_add(1);
        self.entries.insert(
            path,
            ThumbEntry {
                visual,
                mtime,
                full_size,
                verified_generation,
                last_used: self.tick,
            },
        );
        while self.entries.len() > self.capacity {
            // O(n) min-scan is fine at capacity 64 and only runs on insert overflow.
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(p, _)| p.clone());
            match oldest {
                Some(p) => {
                    self.entries.remove(&p);
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

    /// Whether `path` is currently cached.
    #[cfg(test)]
    fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
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
    in_flight: HashSet<(JobKind, PathBuf)>,
    texture_serial: u64,
}

impl Default for ThumbRuntime {
    fn default() -> Self {
        let (tx_job, rx_job) = mpsc::channel::<ThumbJob>();
        let (tx_event, rx_event) = mpsc::channel::<ThumbEvent>();
        let cancel = Arc::new(AtomicBool::new(false));
        let epoch = Arc::new(AtomicU64::new(0));
        let worker_cancel = Arc::clone(&cancel);
        let worker_epoch = Arc::clone(&epoch);
        let worker = thread::spawn(move || run_worker(&rx_job, &tx_event, &worker_cancel, &worker_epoch));
        Self {
            tx: tx_job,
            rx: rx_event,
            worker: Some(worker),
            cancel,
            epoch,
            cache: ThumbCache::new(THUMB_CACHE_CAPACITY),
            preview_cache: ThumbCache::new(PREVIEW_CACHE_CAPACITY),
            in_flight: HashSet::new(),
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
        if self.in_flight.len() >= MAX_IN_FLIGHT_THUMB_JOBS {
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
        if self.in_flight.len() >= MAX_IN_FLIGHT_THUMB_JOBS {
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

    /// Whether any thumbnail or preview job is still in flight.
    pub(super) fn has_in_flight(&self) -> bool {
        !self.in_flight.is_empty()
    }

    /// Drains worker events: uploads finished thumbnails as textures and returns
    /// completed layer scans as `(epoch, counts)` pairs for the tab to filter.
    pub(super) fn poll(&mut self, ctx: &egui::Context) -> Vec<(u64, HashMap<usize, usize>)> {
        let mut scans = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(ThumbEvent::Unchanged { path, generation, epoch }) => {
                    if epoch != self.epoch.load(Ordering::Acquire) { continue; }
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
                    self.clear_in_flight(JobKind::Thumb, &path);
                    self.cache
                        .insert(path, ThumbVisual::Failed, mtime, None, generation);
                }
                Ok(ThumbEvent::LayersScanned { epoch, counts }) => {
                    scans.push((epoch, counts));
                }
                Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        scans
    }

    /// Drops both caches and invalidates queued/in-flight replies by epoch.
    pub(super) fn reset(&mut self) {
        // Invalidates queued and already-produced replies without uploading stale textures.
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.cache.clear();
        self.preview_cache.clear();
        self.in_flight.clear();
    }
}

/// Worker loop: sequentially serves thumbnail decodes and manifest scans until
/// `Stop` is received or the job channel disconnects.
fn run_worker(
    rx_job: &Receiver<ThumbJob>,
    tx_event: &Sender<ThumbEvent>,
    cancel: &AtomicBool,
    active_epoch: &AtomicU64,
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
                        crate::runtime_log::log_warn(format!(
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
                        crate::runtime_log::log_warn(format!(
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

/// Decodes the page image at `path` and downsizes it so neither side exceeds
/// `long_side_px`, preserving the aspect ratio. An image already smaller than the
/// bound is returned untouched (`DynamicImage::thumbnail` never upscales), so the
/// produced size is at most `long_side_px` on the long side, never exactly it.
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

/// Reads the saved and unsaved `layers.json` manifests and returns the layer
/// count (`tree.len()`) per page index. Unsaved page entries override saved ones
/// (page-granular staging, matching how the layer loader resolves pages). A
/// missing manifest contributes nothing; a corrupt one is logged and skipped.
fn scan_layer_counts(saved_manifest: &Path, unsaved_manifest: &Path) -> HashMap<usize, usize> {
    let mut counts: HashMap<usize, usize> = HashMap::new();
    for (path, is_unsaved) in [(saved_manifest, false), (unsaved_manifest, true)] {
        match crate::models::layer_model::compat::read_manifest(path) {
            Ok(Some(manifest)) => {
                for page in &manifest.pages {
                    // Later (unsaved) entries replace earlier (saved) ones per page.
                    counts.insert(page.img_idx, page.tree.len());
                }
            }
            Ok(None) => {}
            Err(err) => {
                crate::runtime_log::log_warn(format!(
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
}
