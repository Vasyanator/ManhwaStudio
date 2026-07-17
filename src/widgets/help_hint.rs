/*
FILE HEADER (widgets/help_hint.rs)
- Purpose: a small light-gray "?" icon that shows a streaming animated WebP
  hint (`ms_gifs::Hint`) inside its hover tooltip.
- Key items:
  - `HelpHint`: the public widget (`new(hint)` + `show(ui)`).
  - `HelpHintCache`: process-wide single-slot playback cache stored in egui
    temp memory behind an `Arc<Mutex<..>>`.
  - `spawn_playback_thread`: background streaming decode and playback via
    `ms_thread`; the GUI thread only uploads the newest published frame.
- Memory contract: one decoder canvas plus two reusable RGBA publication
  buffers and one GPU texture. Memory is about one canvas (~1.6 MB for the
  largest asset) per buffer and does not grow with frame count. The worker
  stops when its tooltip is no longer rendered; switching hints stops it
  immediately and drops the old texture.
- Drawing invariant: the icon allocates exactly one `Sense::hover()` rect;
  the circle and "?" glyph are painter-only.
*/

use eframe::egui;
use egui::{Align2, FontId, Sense, Stroke, Vec2};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::runtime_log;
use ms_thread as thread;

/// Side of the square hover area allocated for the icon, in logical points.
const ICON_SIDE_PT: f32 = 14.0;
/// Font size of the "?" glyph inside the circle.
const ICON_FONT_PT: f32 = 9.0;
/// Upper bound for the animation inside the tooltip, in logical points.
const TOOLTIP_MAX_IMAGE_SIZE_PT: Vec2 = Vec2::new(500.0, 400.0);
/// Consecutive frame intervals without a tooltip heartbeat before shutdown.
const MISSED_HEARTBEATS_BEFORE_STOP: u8 = 2;

/// Shared handle to the process-wide hint-animation cache.
type SharedHelpHintCache = Arc<Mutex<HelpHintCache>>;

/// Single-slot streaming playback cache and per-session failure blacklist.
#[derive(Default)]
struct HelpHintCache {
    active: Option<ActivePlayback>,
    worker: Option<WorkerSlot>,
    failed: Vec<&'static str>,
}

/// GUI-owned state for the currently active streaming animation.
struct ActivePlayback {
    name: &'static str,
    size: [usize; 2],
    expected_len: usize,
    frames: Arc<Mutex<FrameExchange>>,
    heartbeat: Arc<AtomicU64>,
    texture: Option<egui::TextureHandle>,
}

/// Reusable double buffer exchanged between the decoder and GUI.
#[derive(Default)]
struct FrameExchange {
    ready: Option<Vec<u8>>,
    free: Option<Vec<u8>>,
}

/// Control state for the single background playback worker.
struct WorkerSlot {
    name: &'static str,
    stop: Arc<AtomicBool>,
}

/// Light-gray circled "?" icon that streams an animated hint in its tooltip.
#[derive(Debug, Clone, Copy)]
pub struct HelpHint {
    hint: ms_gifs::Hint,
}

impl HelpHint {
    /// Creates the icon for `hint`; no frame is decoded here.
    #[must_use]
    pub fn new(hint: ms_gifs::Hint) -> Self {
        Self { hint }
    }

    /// Draws the icon and attaches its animated hover tooltip.
    pub fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(ICON_SIDE_PT), Sense::hover());
        if ui.is_rect_visible(rect) {
            let color = if response.hovered() {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().weak_text_color()
            };
            let painter = ui.painter_at(rect);
            painter.circle_stroke(
                rect.center(),
                ICON_SIDE_PT / 2.0 - 1.0,
                Stroke::new(1.0, color),
            );
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "?",
                FontId::proportional(ICON_FONT_PT),
                color,
            );
        }

        let cache = cache_handle(ui.ctx());
        if lock_cache(&cache).failed.contains(&self.hint.name()) {
            return response;
        }
        response.on_hover_ui(|tooltip_ui| self.tooltip_ui(tooltip_ui, &cache))
    }

    /// Renders the newest ready frame and advances the worker visibility heartbeat.
    fn tooltip_ui(self, ui: &mut egui::Ui, cache: &SharedHelpHintCache) {
        let name = self.hint.name();
        let mut spawn = false;
        let mut exchange = None;
        let mut expected_len = 0;
        let mut size = [0, 0];
        let mut texture = None;

        {
            let mut guard = lock_cache(cache);
            if guard.active.as_ref().is_some_and(|active| active.name != name) {
                if let Some(worker) = guard.worker.as_ref() {
                    worker.stop.store(true, Ordering::Release);
                }
                guard.active = None;
            }
            if let Some(worker) = guard.worker.as_ref()
                && worker.name != name
            {
                worker.stop.store(true, Ordering::Release);
            }
            if guard.active.is_none() && guard.worker.is_none() && !guard.failed.contains(&name) {
                spawn = true;
            }
            if let Some(active) = guard.active.as_mut().filter(|active| active.name == name) {
                active.heartbeat.fetch_add(1, Ordering::Release);
                exchange = Some(Arc::clone(&active.frames));
                expected_len = active.expected_len;
                size = active.size;
                texture = active.texture.clone();
            }
        }

        if spawn {
            spawn_playback_thread(ui.ctx().clone(), Arc::clone(cache), self.hint);
        }

        if let Some(frames) = exchange {
            let ready = lock_frames(&frames).ready.take();
            if let Some(buffer) = ready {
                if buffer.len() == expected_len {
                    let image = egui::ColorImage::from_rgba_unmultiplied(size, &buffer);
                    let mut guard = lock_cache(cache);
                    if let Some(active) = guard.active.as_mut().filter(|active| active.name == name) {
                        match active.texture.as_mut() {
                            Some(handle) => handle.set(image, egui::TextureOptions::LINEAR),
                            None => {
                                active.texture = Some(ui.ctx().load_texture(
                                    format!("help_hint::{name}"),
                                    image,
                                    egui::TextureOptions::LINEAR,
                                ));
                            }
                        }
                        texture = active.texture.clone();
                    }
                    lock_frames(&frames).free = Some(buffer);
                } else {
                    fail_active(cache, name, format!(
                        "published frame buffer length {} does not match expected {expected_len}",
                        buffer.len()
                    ));
                    return;
                }
            }
        }

        match texture {
            Some(handle) => {
                ui.add(
                    egui::Image::from_texture(&handle)
                        .fit_to_original_size(1.0)
                        .max_size(TOOLTIP_MAX_IMAGE_SIZE_PT),
                );
            }
            None => {
                ui.spinner();
            }
        }
    }
}

/// Id of the process-wide cache slot in egui temporary memory.
fn cache_slot_id() -> egui::Id {
    egui::Id::new("help_hint_animation_cache")
}

/// Returns the process-wide cache handle, creating it on first use.
fn cache_handle(ctx: &egui::Context) -> SharedHelpHintCache {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_insert_with(cache_slot_id(), || {
            Arc::new(Mutex::new(HelpHintCache::default()))
        })
        .clone()
    })
}

/// Locks regenerable cache state even after a worker panic poisoned it.
fn lock_cache(cache: &SharedHelpHintCache) -> MutexGuard<'_, HelpHintCache> {
    match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Locks the frame exchange even after a worker panic poisoned it.
fn lock_frames(frames: &Arc<Mutex<FrameExchange>>) -> MutexGuard<'_, FrameExchange> {
    match frames.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Clears the worker slot on every exit, including panic unwinding.
struct WorkerSlotGuard {
    cache: SharedHelpHintCache,
    name: &'static str,
}

impl Drop for WorkerSlotGuard {
    fn drop(&mut self) {
        let mut guard = lock_cache(&self.cache);
        if guard.worker.as_ref().is_some_and(|worker| worker.name == self.name) {
            guard.worker = None;
            // A panic can bypass normal teardown; remove the orphaned GUI slot
            // so the next hover can start a fresh worker instead of spinning forever.
            if guard.active.as_ref().is_some_and(|active| active.name == self.name) {
                guard.active = None;
            }
        }
    }
}

/// Opens and drives one streaming player until its tooltip disappears or it is replaced.
fn spawn_playback_thread(ctx: egui::Context, cache: SharedHelpHintCache, hint: ms_gifs::Hint) {
    let name = hint.name();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut guard = lock_cache(&cache);
        if guard.worker.is_some() || guard.failed.contains(&name) {
            return;
        }
        guard.worker = Some(WorkerSlot {
            name,
            stop: Arc::clone(&stop),
        });
    }

    thread::spawn(move || {
        let _worker_slot = WorkerSlotGuard {
            cache: Arc::clone(&cache),
            name,
        };
        let mut player = match ms_gifs::Player::open(hint) {
            Ok(player) => player,
            Err(error) => {
                fail_active(&cache, name, error.to_string());
                ctx.request_repaint();
                return;
            }
        };
        let width = match usize::try_from(player.width()) {
            Ok(width) => width,
            Err(error) => {
                fail_active(&cache, name, format!("width conversion failed: {error}"));
                ctx.request_repaint();
                return;
            }
        };
        let height = match usize::try_from(player.height()) {
            Ok(height) => height,
            Err(error) => {
                fail_active(&cache, name, format!("height conversion failed: {error}"));
                ctx.request_repaint();
                return;
            }
        };
        let expected_len = player.frame_buffer_len();
        let heartbeat = Arc::new(AtomicU64::new(0));
        let frames = Arc::new(Mutex::new(FrameExchange {
            ready: None,
            free: Some(vec![0; expected_len]),
        }));
        {
            let mut guard = lock_cache(&cache);
            if stop.load(Ordering::Acquire)
                || guard.worker.as_ref().is_none_or(|worker| worker.name != name)
            {
                return;
            }
            guard.active = Some(ActivePlayback {
                name,
                size: [width, height],
                expected_len,
                frames: Arc::clone(&frames),
                heartbeat: Arc::clone(&heartbeat),
                texture: None,
            });
        }

        let mut decode_buffer = vec![0; expected_len];
        let mut observed_heartbeat = heartbeat.load(Ordering::Acquire);
        let mut missed_heartbeats = 0_u8;
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let delay = match player.next_frame(&mut decode_buffer) {
                Ok(delay) => delay,
                Err(error) => {
                    fail_active(&cache, name, error.to_string());
                    ctx.request_repaint();
                    return;
                }
            };
            {
                // Only the buffer swap is locked; decoding and sleeping never hold it.
                let mut slot = lock_frames(&frames);
                if let Some(replacement) = slot.ready.take().or_else(|| slot.free.take()) {
                    slot.ready = Some(std::mem::replace(&mut decode_buffer, replacement));
                }
            }
            ctx.request_repaint();
            thread::sleep(delay);

            if stop.load(Ordering::Acquire) {
                break;
            }
            let current_heartbeat = heartbeat.load(Ordering::Acquire);
            if current_heartbeat == observed_heartbeat {
                missed_heartbeats = missed_heartbeats.saturating_add(1);
                if missed_heartbeats >= MISSED_HEARTBEATS_BEFORE_STOP {
                    break;
                }
            } else {
                observed_heartbeat = current_heartbeat;
                missed_heartbeats = 0;
            }
        }

        let mut guard = lock_cache(&cache);
        if guard.active.as_ref().is_some_and(|active| active.name == name) {
            guard.active = None;
        }
        drop(guard);
        ctx.request_repaint();
    });
}

/// Logs one playback error, blacklists the hint, and drops its active texture.
fn fail_active(cache: &SharedHelpHintCache, name: &'static str, message: String) {
    let mut guard = lock_cache(cache);
    if !guard.failed.contains(&name) {
        runtime_log::log_error(format!(
            "[widgets::help_hint] failed to play hint animation '{name}': {message}"
        ));
        guard.failed.push(name);
    }
    if guard.active.as_ref().is_some_and(|active| active.name == name) {
        guard.active = None;
    }
}
