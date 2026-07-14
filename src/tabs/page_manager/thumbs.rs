/*
File: tabs/page_manager/thumbs.rs

Purpose:
Background worker + GUI-side LRU cache for the page-manager tab: page thumbnail
decode/downscale and the `layers.json` layer-count scan, both off the GUI thread.

Key structures:
- ThumbRuntime: worker channels, in-flight tracking, and the thumbnail cache.
- ThumbCache<T>: generic LRU cache keyed by page path (payload-agnostic so the
  eviction logic is unit-testable without GPU textures).
- ThumbJob / ThumbEvent: worker protocol.

Key functions:
- ThumbRuntime::request_thumb_if_needed(): dedup + capped job submission with
  mtime-based revalidation.
- ThumbRuntime::poll(): drains worker events, uploads textures, returns layer scans.
- scan_layer_counts(): merges saved/unsaved `layers.json` into per-page layer counts.

Notes:
The worker mirrors the thumbnail thread of `src/tabs/characters.rs`. Cache key
semantics are (path, mtime): an entry is reused only while the file's mtime is
unchanged; revalidation is triggered by bumping the generation counter
(`PageManagerTabState::notify_pages_changed`).
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
    in_flight: HashSet<PathBuf>,
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
        if self.in_flight.contains(path) {
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
        self.in_flight.insert(path.to_path_buf());
        let epoch = self.epoch.load(Ordering::Acquire);
        let _ = self.tx.send(ThumbJob::Thumb {
            path: path.to_path_buf(),
            known_mtime,
            generation,
            epoch,
        });
        true
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

    /// Whether any thumbnail job is still in flight.
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
                    self.in_flight.remove(&path);
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
                    self.in_flight.remove(&path);
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
                Ok(ThumbEvent::Failed {
                    path,
                    mtime,
                    generation,
                    epoch,
                }) => {
                    if epoch != self.epoch.load(Ordering::Acquire) { continue; }
                    self.in_flight.remove(&path);
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

    /// Drops the cache and invalidates queued/in-flight thumbnail replies by epoch.
    pub(super) fn reset(&mut self) {
        // Invalidates queued and already-produced replies without uploading stale textures.
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.cache.clear();
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
                match decode_thumb(&path) {
                    Ok(decoded) => {
                        let _ = tx_event.send(ThumbEvent::Loaded {
                            path,
                            mtime,
                            full_size: decoded.full_size,
                            thumb_width: decoded.thumb_width,
                            thumb_height: decoded.thumb_height,
                            thumb_rgba: decoded.thumb_rgba,
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

/// Result of a successful thumbnail decode: the FULL source dimensions plus the
/// downscaled RGBA buffer.
struct DecodedThumbData {
    full_size: (u32, u32),
    thumb_width: usize,
    thumb_height: usize,
    thumb_rgba: Vec<u8>,
}

/// Decodes the page image at `path` and downsizes it so the long side is at most
/// [`THUMB_LONG_SIDE_PX`].
///
/// # Errors
/// Returns the decode error message when the file cannot be opened or decoded.
fn decode_thumb(path: &Path) -> Result<DecodedThumbData, String> {
    let img = image::open(path).map_err(|err| err.to_string())?;
    let full_size = (img.width(), img.height());
    let thumb = img
        .thumbnail(THUMB_LONG_SIDE_PX, THUMB_LONG_SIDE_PX)
        .to_rgba8();
    let thumb_width = thumb.width() as usize;
    let thumb_height = thumb.height() as usize;
    Ok(DecodedThumbData {
        full_size,
        thumb_width,
        thumb_height,
        thumb_rgba: thumb.into_raw(),
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
    fn touch_missing_returns_none() {
        let mut cache: ThumbCache<()> = ThumbCache::new(2);
        assert!(cache.touch_and_get(Path::new("missing")).is_none());
    }
}
