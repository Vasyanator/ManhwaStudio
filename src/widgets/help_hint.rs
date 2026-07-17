/*
FILE HEADER (widgets/help_hint.rs)
- Purpose: a small light-gray "?" icon whose hover tooltip explains a control
  with a localized text line, a streaming animated WebP hint (`ms_gifs::Hint`),
  or both.
- Key items:
  - `HelpHint`: the public widget. Three modes, selected by the constructors:
    `animated(hint)` (animation only), `text(text)` (text only), and either one
    completed by `with_text` / `with_animation` (text above the animation).
    `show(ui)` draws the icon and attaches the tooltip.
  - `draw_icon` / `tooltip_text_ui`: the icon and the width-capped text line.
  - `AnimationPlan` + `resolve_animation` / `draw_animation`: the animation body
    of the tooltip, split so that every cache access (including the paths that
    blacklist a hint) happens while resolving, and painting is then a pure
    function of the resolved plan.
  - `HelpHintCache`: process-wide single-slot playback cache stored in egui
    temp memory behind an `Arc<Mutex<..>>`.
  - `spawn_playback_thread`: background streaming decode and playback via
    `ms_thread`; the GUI thread only uploads the newest published frame.
- Mode contract: a hint without an animation never touches the playback cache
  and never starts a worker, so the text-only mode is fully independent of
  `ms-gifs`. A hint whose animation is blacklisted still shows its text.
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
/// Wrap width for the tooltip text line, in logical points.
///
/// The animation may be up to `TOOLTIP_MAX_IMAGE_SIZE_PT.x` wide, and the tooltip is
/// sized to its widest child; without this cap a one-line text would be laid out
/// against that width and produce an unreadably wide tooltip.
const TOOLTIP_MAX_TEXT_WIDTH_PT: f32 = 320.0;
/// Vertical gap between the tooltip text line and the animation below it.
const TOOLTIP_TEXT_GAP_PT: f32 = 4.0;
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

/// Light-gray circled "?" icon explaining a control through its hover tooltip.
///
/// The tooltip carries a localized text line, a streaming animation, or both (text
/// above the animation). At least one of the two is always set by the constructors.
#[derive(Debug, Clone)]
pub struct HelpHint {
    animation: Option<ms_gifs::Hint>,
    text: Option<String>,
}

impl HelpHint {
    /// Icon whose tooltip shows only `hint`'s animation.
    ///
    /// No frame is decoded here: playback starts on the first hover and stops once the
    /// tooltip is no longer rendered.
    #[must_use]
    pub fn animated(hint: ms_gifs::Hint) -> Self {
        Self {
            animation: Some(hint),
            text: None,
        }
    }

    /// Icon whose tooltip shows only `text`, which must already be localized.
    ///
    /// This mode touches neither the playback cache nor `ms-gifs`, and never starts a
    /// decoder worker.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            animation: None,
            text: Some(text.into()),
        }
    }

    /// Adds the localized `text` line above the tooltip's animation.
    ///
    /// Replaces any text set earlier on this builder.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Adds `hint`'s animation below the tooltip's text line.
    ///
    /// Replaces any animation set earlier on this builder.
    #[must_use]
    pub fn with_animation(mut self, hint: ms_gifs::Hint) -> Self {
        self.animation = Some(hint);
        self
    }

    /// Draws the icon and attaches its hover tooltip.
    ///
    /// The tooltip is attached only when it would have something to show. A hint with no
    /// animation never reaches the playback cache; a hint whose animation is blacklisted
    /// for the session still shows its text, and drops the tooltip entirely only when it
    /// has no text either.
    pub fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let response = draw_icon(ui);

        // Text-only mode must stay free of `ms-gifs`: no cache handle, no worker.
        let Some(hint) = self.animation else {
            return match self.text {
                Some(text) => {
                    response.on_hover_ui(|tooltip_ui| tooltip_text_ui(tooltip_ui, &text))
                }
                None => response,
            };
        };

        let cache = cache_handle(ui.ctx());
        // Blacklisting is permanent for the session, so this read only ever goes from
        // "available" to "unavailable". It gates whether a tooltip is worth attaching at
        // all; what the tooltip actually paints is decided inside the closure, because a
        // worker can blacklist the hint after this point.
        if lock_cache(&cache).failed.contains(&hint.name()) && self.text.is_none() {
            return response;
        }

        let text = self.text;
        response.on_hover_ui(|tooltip_ui| {
            // The animation is resolved before the text is laid out, and the gap below is
            // then driven by that same resolution. Deciding the gap from a separate,
            // earlier read would leave a window in which the worker blacklists the hint in
            // between: the gap would be committed for an animation that then refuses to
            // paint, leaving the text trailed by a stray space. One decision cannot
            // disagree with itself.
            let plan = resolve_animation(tooltip_ui, hint, &cache);
            if let Some(text) = text.as_deref() {
                tooltip_text_ui(tooltip_ui, text);
                if plan.paints() {
                    tooltip_ui.add_space(TOOLTIP_TEXT_GAP_PT);
                }
            }
            draw_animation(tooltip_ui, plan);
        })
    }
}

/// What the animation half of the tooltip will paint this frame.
///
/// Produced by `resolve_animation`, which performs every cache access — including the
/// failure paths that blacklist a hint — so that `draw_animation` needs no further
/// lookup and cannot contradict the layout decisions taken from `paints`.
enum AnimationPlan {
    /// The hint is unusable (blacklisted, or it failed while resolving): paint nothing.
    Unavailable,
    /// No frame published yet: paint a spinner.
    Pending,
    /// Paint this newest published frame.
    Frame(egui::TextureHandle),
}

impl std::fmt::Debug for AnimationPlan {
    /// Hand-written because `egui::TextureHandle` is not `Debug`; a diagnostic only needs
    /// the variant and the texture identity, never the pixels behind it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("Unavailable"),
            Self::Pending => f.write_str("Pending"),
            Self::Frame(handle) => f.debug_tuple("Frame").field(&handle.id()).finish(),
        }
    }
}

impl AnimationPlan {
    /// Whether `draw_animation` will occupy any space for this plan.
    ///
    /// Callers size the gap above the animation from this, so it must stay derived from
    /// the plan itself rather than from a second read of the blacklist.
    const fn paints(&self) -> bool {
        match self {
            Self::Unavailable => false,
            Self::Pending | Self::Frame(_) => true,
        }
    }
}

/// Draws the circled "?" glyph and returns its single hover-sensing response.
fn draw_icon(ui: &mut egui::Ui) -> egui::Response {
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
    response
}

/// Renders the tooltip text line, wrapped at `TOOLTIP_MAX_TEXT_WIDTH_PT`.
///
/// The cap is applied to a child ui rather than to the tooltip itself: the child still
/// reports only the width it actually used, so a short line keeps the tooltip narrow
/// while the animation below stays free to use its full width.
fn tooltip_text_ui(ui: &mut egui::Ui, text: &str) {
    ui.scope(|ui| {
        ui.set_max_width(TOOLTIP_MAX_TEXT_WIDTH_PT);
        ui.add(egui::Label::new(text).wrap());
    });
}

/// Decides what `hint` will paint this frame, advancing the worker visibility heartbeat
/// and uploading the newest published frame on the way.
///
/// This is where every blacklist read and write for the tooltip body happens, so the
/// returned plan is the single authority on the animation for this frame. The cache lock
/// is released before any widget is added, and no worker is started for a blacklisted hint.
fn resolve_animation(
    ui: &egui::Ui,
    hint: ms_gifs::Hint,
    cache: &SharedHelpHintCache,
) -> AnimationPlan {
    let name = hint.name();
    let mut spawn = false;
    let mut exchange = None;
    let mut expected_len = 0;
    let mut size = [0, 0];
    let mut texture = None;

    {
        let mut guard = lock_cache(cache);
        // Read the blacklist under the same lock that hands out the active playback: a
        // hint that has already failed can never resolve to anything drawable.
        if guard.failed.contains(&name) {
            return AnimationPlan::Unavailable;
        }
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
        if guard.active.is_none() && guard.worker.is_none() {
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
        spawn_playback_thread(ui.ctx().clone(), Arc::clone(cache), hint);
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
                // This blacklists the hint mid-resolve, so the plan must report the hint
                // as gone rather than fall through to a spinner the next line would paint.
                return AnimationPlan::Unavailable;
            }
        }
    }

    match texture {
        Some(handle) => AnimationPlan::Frame(handle),
        None => AnimationPlan::Pending,
    }
}

/// Paints the resolved animation.
///
/// Pure with respect to the cache: the plan already carries the decision, so this cannot
/// disagree with the gap that `AnimationPlan::paints` sized above it.
fn draw_animation(ui: &mut egui::Ui, plan: AnimationPlan) {
    match plan {
        AnimationPlan::Unavailable => {}
        AnimationPlan::Pending => {
            ui.spinner();
        }
        AnimationPlan::Frame(handle) => {
            ui.add(
                egui::Image::from_texture(&handle)
                    .fit_to_original_size(1.0)
                    .max_size(TOOLTIP_MAX_IMAGE_SIZE_PT),
            );
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

#[cfg(test)]
mod tests {
    use super::{AnimationPlan, HelpHint};

    /// Any embedded hint works as a constructor fixture: these tests only inspect the
    /// stored identity, so no asset is ever opened or decoded.
    const HINT: ms_gifs::Hint = ms_gifs::typing::ALIGNMENT;

    #[test]
    fn animated_stores_only_the_animation() {
        let hint = HelpHint::animated(HINT);
        assert_eq!(hint.animation, Some(HINT));
        assert_eq!(hint.text, None);
    }

    #[test]
    fn text_stores_only_the_text() {
        let hint = HelpHint::text("explain me");
        assert_eq!(hint.animation, None);
        assert_eq!(hint.text.as_deref(), Some("explain me"));
    }

    #[test]
    fn with_text_completes_an_animated_hint() {
        let hint = HelpHint::animated(HINT).with_text("explain me");
        assert_eq!(hint.animation, Some(HINT));
        assert_eq!(hint.text.as_deref(), Some("explain me"));
    }

    #[test]
    fn with_animation_completes_a_text_hint() {
        let hint = HelpHint::text("explain me").with_animation(HINT);
        assert_eq!(hint.animation, Some(HINT));
        assert_eq!(hint.text.as_deref(), Some("explain me"));
    }

    #[test]
    fn builder_calls_replace_the_previous_value() {
        let hint = HelpHint::text("first")
            .with_text("second")
            .with_animation(ms_gifs::typing::KERNING)
            .with_animation(HINT);
        assert_eq!(hint.text.as_deref(), Some("second"));
        assert_eq!(hint.animation, Some(HINT));
    }

    /// The tooltip gap is sized from `paints`, so an unavailable animation must be the
    /// only plan that reserves no space; anything else would strand a gap under the text.
    ///
    /// `AnimationPlan::Frame` cannot be built here — a `TextureHandle` requires a live
    /// egui `Context` — but it shares the `paints` arm with `Pending`, and `draw_animation`
    /// matches the enum exhaustively, so a new variant cannot silently skip this contract.
    #[test]
    fn only_an_unavailable_plan_reserves_no_space() {
        assert!(!AnimationPlan::Unavailable.paints());
        assert!(AnimationPlan::Pending.paints());
    }
}
