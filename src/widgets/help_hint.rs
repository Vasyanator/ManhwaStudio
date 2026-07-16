/*
FILE HEADER (widgets/help_hint.rs)
- Purpose: a small light-gray "?" icon that shows an animated WebP hint
  (an `ms_gifs::Hint` asset) inside its hover tooltip. The tooltip contains
  ONLY the animation (no text), so the widget needs no localization keys.
- Key items:
  - `HelpHint`: the public widget (`new(hint)` + `show(ui)`).
  - `HelpHintCache`: process-wide single-slot animation cache stored in egui
    temp memory behind an `Arc<Mutex<..>>`, shared by every `HelpHint`
    instance (the same parameter row is drawn from several panels, so the
    cache cannot live in one panel's state).
  - `spawn_decode_thread`: background decode via `ms_thread` (never on the
    GUI thread — a decoded hint is up to ~106 MB / up to ~175 frames).
- Memory contract: at most ONE decoded animation is resident (CPU frames +
  one `TextureHandle` updated in place per frame via `TextureHandle::set`);
  it is evicted a few seconds after the tooltip was last visible. Eviction is
  guaranteed by `maintain(ctx)`, which the app root (`MangaApp::ui`) calls
  once per frame: icons alone cannot guarantee it, because no `HelpHint` is
  drawn after the user leaves the panel. Without the root hook the cache
  would stay resident until the next icon draw (or forever, if a decode
  finishes after the user left the tab for good).
- Drawing invariant: the icon allocates exactly one `Sense::hover()` rect;
  the circle and the "?" glyph are painter-only (same rule as ai_button.rs:
  a second interactive rect would carve a hole in the hitbox).
*/

use eframe::egui;
use egui::{Align2, FontId, Sense, Stroke, Vec2};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::runtime_log;
use ms_thread as thread;

/// Side of the square hover area allocated for the icon, in logical points.
const ICON_SIDE_PT: f32 = 14.0;
/// Font size of the "?" glyph inside the circle.
const ICON_FONT_PT: f32 = 9.0;
/// How long the decoded animation (CPU frames + GPU texture) stays cached
/// after its tooltip was last visible, in seconds. 5 s absorbs the common
/// "re-read the hint" hover pattern while bounding the idle cost of the
/// largest asset (~106 MB RAM + one ~1.5 MB texture) to a short window.
const CACHE_RETENTION_SECS: f64 = 5.0;
/// Lower bound for scheduled repaints, so zero-delay animation frames cannot
/// degenerate into a busy repaint loop (caps the animation at 120 fps).
const MIN_REPAINT_DELAY_SECS: f64 = 1.0 / 120.0;
/// Upper bound (logical points) for the animation inside the tooltip. The
/// animation renders 1:1 (texel = point) and is only scaled DOWN, uniformly,
/// when it exceeds this box — never stretched up to the tooltip width.
const TOOLTIP_MAX_IMAGE_SIZE_PT: Vec2 = Vec2::new(500.0, 400.0);

/// Shared handle to the process-wide hint-animation cache.
type SharedHelpHintCache = Arc<Mutex<HelpHintCache>>;

/// Single-slot cache for hint animations plus per-session decode bookkeeping.
///
/// `active` holds at most one decoded animation; `decoding` is the hint name a
/// background worker is currently decoding (at most one in flight); `failed`
/// lists hints whose decode failed this session, so they are never re-tried
/// (and their tooltip is simply not attached).
#[derive(Default)]
struct HelpHintCache {
    active: Option<ActiveAnimation>,
    decoding: Option<&'static str>,
    failed: Vec<&'static str>,
}

/// A decoded, currently resident hint animation and its playback state.
struct ActiveAnimation {
    /// Stable hint id (`ms_gifs::Hint::name`).
    name: &'static str,
    animation: ms_gifs::Animation,
    /// Texture size in texels; validated against every frame buffer at decode time.
    size: [usize; 2],
    /// Cumulative end time (seconds) of each frame within one loop; same length
    /// as `animation.frames`, monotonically non-decreasing.
    frame_ends: Vec<f64>,
    /// Duration of one full loop in seconds (last element of `frame_ends`).
    total_secs: f64,
    /// The single GPU texture, re-uploaded in place on frame change. Created
    /// lazily on the GUI thread; `None` until the tooltip first shows.
    texture: Option<egui::TextureHandle>,
    /// Frame index currently uploaded into `texture`.
    uploaded_frame: Option<usize>,
    /// `egui::InputState::time` when the tooltip last showed this animation.
    /// `None` means "decoded but not yet seen": the retention clock starts at
    /// first sight instead of evicting a slow decode result immediately.
    last_visible_time: Option<f64>,
}

/// Light-gray circled "?" icon that plays an animated hint in its hover tooltip.
///
/// Draws a single `Sense::hover()` rect; the tooltip shows only the animation
/// (a spinner while it decodes, nothing at all if decoding failed). All decode
/// work runs on a background thread; the GUI thread only uploads one frame at
/// a time into a reused texture.
#[derive(Debug, Clone, Copy)]
pub struct HelpHint {
    hint: ms_gifs::Hint,
}

impl HelpHint {
    /// Creates the icon for `hint`. Cheap: no decode or I/O happens here.
    #[must_use]
    pub fn new(hint: ms_gifs::Hint) -> Self {
        Self { hint }
    }

    /// Draws the icon, attaches the animated tooltip, and performs cache
    /// housekeeping (eviction of the idle animation). Returns the icon's
    /// hover `Response`.
    pub fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(ICON_SIDE_PT), Sense::hover());
        if ui.is_rect_visible(rect) {
            // Painter-only visuals over the single hitbox. Theme-derived light
            // gray at rest, stronger on hover as the affordance cue.
            let color = if response.hovered() {
                ui.visuals().strong_text_color()
            } else {
                ui.visuals().weak_text_color()
            };
            let painter = ui.painter_at(rect);
            let radius = ICON_SIDE_PT / 2.0 - 1.0;
            painter.circle_stroke(rect.center(), radius, Stroke::new(1.0, color));
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "?",
                FontId::proportional(ICON_FONT_PT),
                color,
            );
        }

        let cache = cache_handle(ui.ctx());
        // Read the clock BEFORE any repaint call: `ctx.input` holds a lock, so
        // nothing else on the Context may be called from inside its closure.
        let now = ui.ctx().input(|input| input.time);

        if let Some(delay) = housekeep_cache(&cache, now) {
            // One scheduled wakeup at cache expiry, so the idle animation is
            // evicted even when the UI otherwise stops repainting. The actual
            // eviction on that woken frame is performed by `maintain` (called
            // from the app root), so it happens even if no icon draws then.
            ui.ctx().request_repaint_after(delay);
        }
        if lock_cache(&cache).failed.contains(&self.hint.name()) {
            // Decode already failed this session: keep the icon but attach no
            // tooltip, and never hit the worker again for this hint.
            return response;
        }

        response.on_hover_ui(|tooltip_ui| self.tooltip_ui(tooltip_ui, &cache))
    }

    /// Tooltip body: runs every frame while the tooltip is visible, which is
    /// what advances the animation (frame index is derived from wall time).
    fn tooltip_ui(self, ui: &mut egui::Ui, cache: &SharedHelpHintCache) {
        let name = self.hint.name();
        let now = ui.ctx().input(|input| input.time);

        let mut spawn_decode = false;
        let mut repaint_after: Option<Duration> = None;
        let mut texture: Option<egui::TextureHandle> = None;
        {
            let mut guard = lock_cache(cache);
            if guard.active.as_ref().is_some_and(|active| active.name != name) {
                // Single-slot cache: hovering a different hint frees the
                // previous animation's frames and GPU texture right away.
                guard.active = None;
            }
            if guard.active.is_none() && guard.decoding.is_none() && !guard.failed.contains(&name) {
                guard.decoding = Some(name);
                spawn_decode = true;
            }
            if let Some(active) = guard.active.as_mut() {
                active.last_visible_time = Some(now);
                let (frame_idx, until_next) =
                    frame_at_time(&active.frame_ends, active.total_secs, now);
                if active.uploaded_frame != Some(frame_idx)
                    && let Some(frame) = active.animation.frames.get(frame_idx)
                {
                    // ms-gifs frames are straight (non-premultiplied) RGBA, so
                    // `from_rgba_unmultiplied` is the correct conversion. The
                    // buffer length was validated at decode time, so this
                    // cannot panic. The upload is a bounded memcpy (up to
                    // ~1.5 MB, i.e. well under a millisecond), not long work,
                    // so doing it under the cache lock is fine.
                    let image =
                        egui::ColorImage::from_rgba_unmultiplied(active.size, &frame.rgba);
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
                    active.uploaded_frame = Some(frame_idx);
                }
                // Cheap refcounted clone; the cache keeps the texture alive
                // past this frame (a handle dropped at frame end would free
                // the GPU texture).
                texture = active.texture.clone();
                if active.frame_ends.len() > 1 && active.total_secs > 0.0 {
                    // Repaint exactly at the next frame boundary — and only
                    // while the tooltip is actually visible, so a closed
                    // tooltip never keeps the whole viewport repainting.
                    repaint_after =
                        Some(Duration::from_secs_f64(until_next.max(MIN_REPAINT_DELAY_SECS)));
                }
            }
        }

        if spawn_decode {
            spawn_decode_thread(ui.ctx().clone(), Arc::clone(cache), self.hint);
        }
        if let Some(delay) = repaint_after {
            ui.ctx().request_repaint_after(delay);
        }
        match texture {
            Some(handle) => {
                // 1:1 texel-to-point rendering (user decision): egui's default
                // `shrink_to_fit` would STRETCH small animations to the tooltip
                // width (~488 pt) and blur them. `fit_to_original_size(1.0)`
                // keeps the native size; `max_size` scales oversized assets
                // DOWN uniformly (`maintain_aspect_ratio` defaults to true).
                ui.add(
                    egui::Image::from_texture(&handle)
                        .fit_to_original_size(1.0)
                        .max_size(TOOLTIP_MAX_IMAGE_SIZE_PT),
                );
            }
            None => {
                // Decode in flight: egui's spinner requests its own repaints
                // (egui-0.35.0/src/widgets/spinner.rs:40), so the tooltip wakes
                // up to poll the cache without an explicit repaint loop here.
                ui.spinner();
            }
        }
    }
}

/// Runs the periodic cache maintenance: evicts the resident animation once it
/// has been idle past [`CACHE_RETENTION_SECS`] and schedules exactly one
/// wakeup for the remaining retention while one is still cached.
///
/// Must be called once per frame from an always-drawn root (the app calls it
/// from `MangaApp::ui`). This is what makes eviction unconditional: the icons
/// stop drawing the moment the user leaves the panel, and a decode can even
/// finish after that — with no root hook, that animation (tens of MB) would
/// stay resident until the next icon draw, potentially for the whole session.
///
/// Cheap no-op while no `HelpHint` has ever been shown: it only READS the
/// cache slot and never creates it, so calling it every frame does not insert
/// anything into egui memory.
pub fn maintain(ctx: &egui::Context) {
    let Some(cache) = ctx.data(|data| data.get_temp::<SharedHelpHintCache>(cache_slot_id()))
    else {
        return;
    };
    // Read the clock BEFORE the repaint call: `ctx.input` holds a lock, so
    // nothing else on the Context may be called from inside its closure.
    let now = ctx.input(|input| input.time);
    if let Some(delay) = housekeep_cache(&cache, now) {
        ctx.request_repaint_after(delay);
    }
}

/// Id of the process-wide cache slot in egui temp memory.
fn cache_slot_id() -> egui::Id {
    egui::Id::new("help_hint_animation_cache")
}

/// Returns the process-wide cache handle stored in egui temp memory, creating
/// it on first use.
///
/// A deliberate global slot (like `wheel_input_guard`): the same hint icon is
/// drawn from several panels, and threading a cache through their signatures
/// would touch many call sites for purely internal state.
fn cache_handle(ctx: &egui::Context) -> SharedHelpHintCache {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_insert_with(cache_slot_id(), || {
            Arc::new(Mutex::new(HelpHintCache::default()))
        })
        .clone()
    })
}

/// Locks the cache, accepting a poisoned mutex: the cached data is regenerable
/// (worst case the animation is decoded again), so a panicking worker must not
/// disable help hints for the rest of the session.
fn lock_cache(cache: &SharedHelpHintCache) -> MutexGuard<'_, HelpHintCache> {
    match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Evicts the resident animation once it has been idle for
/// [`CACHE_RETENTION_SECS`]. While an animation is still resident, returns the
/// delay until its expiry so the caller can schedule the eviction wakeup.
fn housekeep_cache(cache: &SharedHelpHintCache, now: f64) -> Option<Duration> {
    let mut guard = lock_cache(cache);
    let mut keepalive = None;
    if let Some(active) = guard.active.as_mut() {
        // A decode that finished while the pointer was already gone starts its
        // retention clock at first sight instead of being evicted instantly.
        let last_seen = *active.last_visible_time.get_or_insert(now);
        let idle_secs = now - last_seen;
        if idle_secs > CACHE_RETENTION_SECS {
            // Dropping `ActiveAnimation` frees the CPU frames now and the GPU
            // texture through `TextureHandle`'s `Drop`.
            guard.active = None;
        } else {
            keepalive = Some(Duration::from_secs_f64(
                (CACHE_RETENTION_SECS - idle_secs).max(MIN_REPAINT_DELAY_SECS),
            ));
        }
    }
    keepalive
}

/// RAII release of the single `decoding` slot: on drop, clears the slot if it
/// still names this worker's hint. The slot MUST be released through drop, not
/// inline code, so that a panicking decode worker (whose thread dies before
/// any cleanup line) cannot leave `decoding` occupied forever — that would
/// permanently reduce every hint tooltip to a spinner, since a new worker is
/// only spawned while the slot is free.
struct DecodingSlotGuard {
    cache: SharedHelpHintCache,
    name: &'static str,
}

impl Drop for DecodingSlotGuard {
    fn drop(&mut self) {
        let mut guard = lock_cache(&self.cache);
        if guard.decoding == Some(self.name) {
            guard.decoding = None;
        }
    }
}

/// Decodes `hint` on a background thread and publishes the result.
///
/// The decode itself runs without the cache lock (project rule: never hold a
/// mutex during long work); the lock is taken only for the final bookkeeping.
/// On failure the error is logged once and the hint is blacklisted for the
/// session so the worker is never re-spawned every frame. The `decoding` slot
/// is released via [`DecodingSlotGuard`] even if the worker panics.
fn spawn_decode_thread(ctx: egui::Context, cache: SharedHelpHintCache, hint: ms_gifs::Hint) {
    thread::spawn(move || {
        let name = hint.name();
        // RAII: held for its drop effect — releases the `decoding` slot on any
        // exit from this closure, including an unwinding panic.
        let _decode_slot = DecodingSlotGuard {
            cache: Arc::clone(&cache),
            name,
        };
        let prepared = ms_gifs::decode(hint)
            .map_err(|error| error.to_string())
            .and_then(|animation| prepare_animation(name, animation));

        let mut guard = lock_cache(&cache);
        match prepared {
            Ok(active) => guard.active = Some(active),
            Err(message) => {
                runtime_log::log_error(format!(
                    "[widgets::help_hint] failed to decode hint animation '{name}': {message}"
                ));
                if !guard.failed.contains(&name) {
                    guard.failed.push(name);
                }
            }
        }
        drop(guard);
        // Wake the UI so an open tooltip swaps its spinner for the animation
        // without waiting for the next input event.
        ctx.request_repaint();
    });
}

/// Validates a decoded animation and builds its playback state.
///
/// Checks (checked arithmetic, no panics): non-zero dimensions, at least one
/// frame, and every frame buffer exactly `width * height * 4` bytes — the
/// preconditions `ColorImage::from_rgba_unmultiplied` would otherwise assert.
///
/// # Errors
/// Returns a human-readable description of the violated invariant.
fn prepare_animation(
    name: &'static str,
    animation: ms_gifs::Animation,
) -> Result<ActiveAnimation, String> {
    let width = usize::try_from(animation.width)
        .map_err(|_| format!("width {} does not fit usize", animation.width))?;
    let height = usize::try_from(animation.height)
        .map_err(|_| format!("height {} does not fit usize", animation.height))?;
    if width == 0 || height == 0 {
        return Err(format!("degenerate animation size {width}x{height}"));
    }
    let expected_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| format!("animation size {width}x{height} overflows the buffer length"))?;
    if animation.frames.is_empty() {
        return Err("animation has no frames".to_owned());
    }
    for (idx, frame) in animation.frames.iter().enumerate() {
        if frame.rgba.len() != expected_len {
            return Err(format!(
                "frame {idx} buffer length {} does not match {width}x{height}x4 = {expected_len}",
                frame.rgba.len()
            ));
        }
    }
    let (frame_ends, total_secs) = cumulative_frame_ends(&animation.frames);
    Ok(ActiveAnimation {
        name,
        animation,
        size: [width, height],
        frame_ends,
        total_secs,
        texture: None,
        uploaded_frame: None,
        last_visible_time: None,
    })
}

/// Cumulative per-frame end times (seconds within one loop) and the loop's
/// total duration. Output length equals `frames.len()`.
fn cumulative_frame_ends(frames: &[ms_gifs::Frame]) -> (Vec<f64>, f64) {
    let mut total = 0.0_f64;
    let ends = frames
        .iter()
        .map(|frame| {
            total += frame.delay.as_secs_f64();
            total
        })
        .collect();
    (ends, total)
}

/// Maps wall-clock time onto a looping animation: returns the frame index to
/// display at `now` and the seconds until the next frame boundary.
///
/// `frame_ends` must be the cumulative ends from [`cumulative_frame_ends`].
/// A zero/negative `total_secs` (all-zero delays) yields a static frame 0.
fn frame_at_time(frame_ends: &[f64], total_secs: f64, now: f64) -> (usize, f64) {
    if frame_ends.is_empty() || total_secs <= 0.0 {
        return (0, 0.0);
    }
    let t = now.rem_euclid(total_secs);
    let last = frame_ends.len() - 1;
    // First frame whose end lies strictly after `t`; clamp guards against
    // float round-off placing `t` at/after the final cumulative end.
    let idx = frame_ends.partition_point(|&end| end <= t).min(last);
    let until_next = (frame_ends[idx] - t).max(0.0);
    (idx, until_next)
}

#[cfg(test)]
mod tests {
    use super::{cumulative_frame_ends, frame_at_time, prepare_animation};
    use std::time::Duration;

    /// Builds a frame with an arbitrary buffer length and delay in milliseconds.
    fn frame(len: usize, delay_ms: u64) -> ms_gifs::Frame {
        ms_gifs::Frame {
            rgba: vec![0; len],
            delay: Duration::from_millis(delay_ms),
        }
    }

    #[test]
    fn cumulative_ends_accumulate_delays() {
        let frames = [frame(8, 100), frame(8, 200), frame(8, 300)];
        let (ends, total) = cumulative_frame_ends(&frames);
        assert_eq!(ends.len(), 3);
        assert!((ends[0] - 0.1).abs() < 1e-9);
        assert!((ends[1] - 0.3).abs() < 1e-9);
        assert!((ends[2] - 0.6).abs() < 1e-9);
        assert!((total - 0.6).abs() < 1e-9);
    }

    #[test]
    fn frame_at_time_selects_and_loops() {
        let ends = [0.1, 0.3, 0.6];
        // Start of the loop.
        let (idx, until) = frame_at_time(&ends, 0.6, 0.0);
        assert_eq!(idx, 0);
        assert!((until - 0.1).abs() < 1e-9);
        // Inside the second frame.
        let (idx, until) = frame_at_time(&ends, 0.6, 0.15);
        assert_eq!(idx, 1);
        assert!((until - 0.15).abs() < 1e-9);
        // Exactly on a boundary advances to the next frame.
        let (idx, _) = frame_at_time(&ends, 0.6, 0.1);
        assert_eq!(idx, 1);
        // Past one full loop wraps around.
        let (idx, _) = frame_at_time(&ends, 0.6, 0.65);
        assert_eq!(idx, 0);
        // Many loops later still lands inside the loop.
        let (idx, _) = frame_at_time(&ends, 0.6, 60.35);
        assert_eq!(idx, 2);
    }

    #[test]
    fn frame_at_time_zero_total_is_static() {
        assert_eq!(frame_at_time(&[0.0, 0.0], 0.0, 5.0), (0, 0.0));
        assert_eq!(frame_at_time(&[], 1.0, 5.0), (0, 0.0));
    }

    #[test]
    fn prepare_animation_accepts_consistent_buffers() {
        let animation = ms_gifs::Animation {
            width: 2,
            height: 1,
            frames: vec![frame(8, 50), frame(8, 50)],
        };
        let active = prepare_animation("test", animation).expect("valid animation");
        assert_eq!(active.size, [2, 1]);
        assert_eq!(active.frame_ends.len(), 2);
        assert!((active.total_secs - 0.1).abs() < 1e-9);
    }

    #[test]
    fn prepare_animation_rejects_bad_input() {
        // Wrong buffer length.
        let animation = ms_gifs::Animation {
            width: 2,
            height: 1,
            frames: vec![frame(7, 50)],
        };
        assert!(prepare_animation("test", animation).is_err());
        // No frames.
        let animation = ms_gifs::Animation {
            width: 2,
            height: 1,
            frames: Vec::new(),
        };
        assert!(prepare_animation("test", animation).is_err());
        // Degenerate size.
        let animation = ms_gifs::Animation {
            width: 0,
            height: 1,
            frames: vec![frame(0, 50)],
        };
        assert!(prepare_animation("test", animation).is_err());
    }
}
