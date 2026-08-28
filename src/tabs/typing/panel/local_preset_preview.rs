/*
File: panel/local_preset_preview.rs

Purpose:
Off-GUI-thread renderer + texture cache for the LOCAL PRESET combo of the typing
create panel (`dev-docs/local_presets_plan.md` §8). One row of that combo shows an
image produced by the PROJECT'S MAIN renderer with the preset's full parameter set
and full effect chain, drawing the preset's NAME as the text — one line, capped at
`PREVIEW_NAME_MAX_CHARS` characters.

Main responsibilities:
- turn a stored preset profile (`{"text_params": {…}, "effects": […]}`) into
  `TextRenderParams` through the tab codec, overriding only what the ONE-LINE
  contract requires;
- run every render on ONE long-lived worker thread, never on the GUI thread, with a
  hard cap on how many renders may be outstanding at a time;
- downscale the result IN THE WORKER so the egui texture atlas never receives a
  full-size render;
- upload the downscaled RGBA as an egui texture on the GUI tick and hand it out to
  the row that asked for it;
- keep the cache bounded and self-invalidating: the key is a hash of everything the
  render depends on, so an edited preset gets a new key and the old texture is freed.

Key structures:
- `LocalPresetPreviewCache`: the whole cache + worker, owned by
  `TypingCreatePanelState`.
- `LocalPresetPreview`: what one row gets for the CURRENT frame
  (`Ready` / `Pending` / `Failed`).

Key functions:
- `LocalPresetPreviewCache::poll`: drain worker results, upload textures (GUI tick).
- `LocalPresetPreviewCache::preview`: the per-row accessor; requests a render on a
  cache miss.
- `preview_label`: the 35-character cap, also used by the caller as the text fallback.
- `preview_target_size`: the pure downscale math.
- `preview_backdrop` / `choose_preview_backdrop` / `last_visible_outline_color`: which of
  the three flat greys the row paints under the image, decided from the preset's OWN
  colours ONCE per requested render.

Notes:
THE GUI THREAD NEVER RENDERS AND NEVER READS A FILE HERE (`CLAUDE.md` §5), AND IT DOES
NOT SERIALIZE JSON EITHER. The cache key folds in the CACHED `LocalPreset::profile_hash`
(computed where the snapshot is written), because `preview` runs once per drawn row per
frame; the GUI tick then only sends a request and calls `Context::load_texture`, which
needs the `Context` and therefore must run here.

Dropping a cache entry frees its GPU texture: `epaint::TextureHandle` frees the
texture in `Drop` (`epaint-0.35.0/src/texture_handle.rs:25-29`). Eviction and key
invalidation are therefore the only "free" this module needs.

The font provider is deliberately NOT part of the cache key — an `Arc<dyn FontProvider>`
has no cheap stable identity. A caller that REPLACES its provider (the panel's font
reload) must call `clear`, otherwise previews rendered with the previous font set stay
on screen.

Untestable here, and deliberately so: anything that needs a real render (a `FontProvider`
with real font bytes and a leased `FontSystem`) or a GPU/`egui::Context` texture upload.
The unit tests at the bottom cover only the pure parts — the key hash, the character cap
and the downscale target math.
*/

use super::ui_helpers::value_as_u8;
use crate::runtime_log;
use crate::tabs::typing::render_next::FontProvider;
use crate::tabs::typing::render_next::render_text_to_image;
use crate::tabs::typing::render_next::types::{TextRenderParams, TextWrapMode};
use crate::tabs::typing::tab::render_store;
use eframe::egui;
use image::RgbaImage;
use image::imageops::FilterType;
use ms_thread as thread;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

/// Maximum number of CHARACTERS (not bytes) of a preset name that the preview draws.
///
/// Fixed by `dev-docs/local_presets_plan.md` §2.6. Counted in `char`s so a Cyrillic or
/// emoji name is cut where the user sees 35 symbols, not 35 bytes.
pub(super) const PREVIEW_NAME_MAX_CHARS: usize = 35;

/// Widest preview texture, in pixels, regardless of the row height.
///
/// A preset with a large font size and 35 characters renders an extremely wide strip;
/// scaled to the row height alone it would still be thousands of pixels wide. When this
/// cap binds, the preview is scaled by WIDTH instead and ends up SHORTER than the row —
/// the aspect ratio is always preserved.
const MAX_PREVIEW_WIDTH_PX: u32 = 320;

/// How many renders may be outstanding (queued at the worker or being rendered) at once.
///
/// The renderer leases a `FontSystem` from a process-global pool that holds
/// `MAX_POOLED_SYSTEMS_PER_MODE = 8` of them; letting the popup queue one render per
/// preset would build and drop whole `FontSystem`s. A row that cannot be requested this
/// frame is answered `Pending` and asks again next frame, so nothing is lost.
const MAX_RENDERS_IN_FLIGHT: usize = 4;

/// How many preview slots the cache keeps before evicting the least recently requested.
///
/// Bounds both RAM and the egui texture atlas: a preset list may grow without limit, the
/// cache may not. Evicting a `Ready` slot drops its `TextureHandle`, which frees the GPU
/// texture; the row re-requests the render when it is drawn again.
const MAX_CACHED_PREVIEWS: usize = 64;

/// Name of the single worker thread that runs preview renders.
const WORKER_THREAD_NAME: &str = "typing-local-preset-preview";

/// How much heavier the OUTLINE colour weighs than the main text colour when a backdrop is
/// scored ([`choose_preview_backdrop`]).
///
/// The outermost outline is what the eye meets first on a preset row, so the user's stated
/// priority is "the last outline layer's colour first, the main text colour second". Four
/// makes the outline dominate outright while still letting the main colour decide between
/// two greys the outline is equally happy with.
const BACKDROP_OUTLINE_WEIGHT: f32 = 4.0;

/// Luminance distance (in the 0..255 scale of
/// [`render_store::shape_variant_luminance`](crate::tabs::typing::tab::render_store::shape_variant_luminance))
/// beyond which more contrast no longer counts as "better".
///
/// Saturating each contrast term at this value is what lets the MEDIUM grey win when both
/// extremes are excellent for one colour and bad for the other: past "good enough" the
/// surplus stops buying anything, so a backdrop that is merely good for BOTH beats one that
/// is perfect for one and invisible for the other. 80 of 255 is roughly the point at which
/// a flat grey and a glyph colour stop being confusable at preview size.
const BACKDROP_CONTRAST_ENOUGH: f32 = 80.0;

/// The flat grey a local-preset preview row is painted on.
///
/// A preview is a TRANSPARENT render, so the row needs a backdrop of its own. It is a FLAT
/// grey rather than a transparency checkerboard: the row exists to show what the preset
/// looks like, and a patterned board competes with the preset's own colours instead of
/// setting them off. Which of the three is used is decided by [`preview_backdrop`] from the
/// preset's colours, ONCE per requested render, and carried on the cache slot — the row
/// must never parse JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreviewBackdrop {
    /// The lightest grey: for a DARK preset.
    Light,
    /// The middle grey: for a preset that mixes a light and a dark colour.
    Medium,
    /// The darkest grey: for a LIGHT preset.
    Dark,
}

impl PreviewBackdrop {
    /// The three greys, in the order [`Self::ALL`] scores them.
    ///
    /// Each is a NEUTRAL grey, so its Rec.709 luminance is exactly this level (the three
    /// coefficients sum to 1) and the contrast arithmetic needs no colour conversion. The
    /// two extremes are the values the typing tab's transparency checkerboard already uses
    /// for its light and dark variants, so the preview rows sit in the same visual family.
    const LIGHT_LEVEL: u8 = 232;
    const MEDIUM_LEVEL: u8 = 128;
    const DARK_LEVEL: u8 = 64;

    /// Every backdrop, in the order [`choose_preview_backdrop`] considers them. Ties are
    /// broken by this order, so the lightest grey wins an exact tie.
    const ALL: [Self; 3] = [Self::Light, Self::Medium, Self::Dark];

    /// The grey level of this backdrop, which is also its Rec.709 luminance.
    #[must_use]
    const fn level(self) -> u8 {
        match self {
            Self::Light => Self::LIGHT_LEVEL,
            Self::Medium => Self::MEDIUM_LEVEL,
            Self::Dark => Self::DARK_LEVEL,
        }
    }

    /// This backdrop's luminance on the same 0..255 scale as
    /// `render_store::shape_variant_luminance`.
    #[must_use]
    fn luminance(self) -> f32 {
        f32::from(self.level())
    }

    /// The opaque fill a preset row is painted with.
    #[must_use]
    pub(super) const fn fill(self) -> egui::Color32 {
        let level = self.level();
        egui::Color32::from_rgb(level, level, level)
    }

    /// The 1 px border drawn around a preset row, so the row still reads as its own strip
    /// against the popup background. It steps AWAY from the fill: darker under the light
    /// grey, lighter under the other two.
    #[must_use]
    pub(super) const fn border(self) -> egui::Color32 {
        match self {
            Self::Light => egui::Color32::from_rgb(150, 150, 150),
            Self::Medium => egui::Color32::from_rgb(176, 176, 176),
            Self::Dark => egui::Color32::from_rgb(115, 115, 115),
        }
    }
}

/// What one local-preset row gets for the CURRENT frame.
///
/// `Pending` is not an error: the render is queued or in flight off the GUI thread, and a
/// repaint has already been requested. `Failed` is terminal for this key — the row draws
/// the preset name as plain text instead (a preset whose font is not installed lands here,
/// the renderer returning `Err("шрифт '…' не найден…")`). Editing the preset changes the
/// key, which is what re-tries a failed render.
pub(super) enum LocalPresetPreview<'a> {
    /// The texture is uploaded and can be painted this frame; `size` is its pixel size.
    Ready {
        texture: &'a egui::TextureHandle,
        size: egui::Vec2,
        /// Which of the three flat greys this preview must be drawn on. Decided ONCE,
        /// when the profile is decoded, and carried here so the row never parses JSON.
        backdrop: PreviewBackdrop,
    },
    /// No image yet — draw the name as text and try again next frame.
    Pending,
    /// This preset cannot be previewed; draw the name as text.
    Failed,
}

impl fmt::Debug for LocalPresetPreview<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready {
                size, backdrop, ..
            } => write!(f, "Ready {{ size: {size:?}, backdrop: {backdrop:?} }}"),
            Self::Pending => f.write_str("Pending"),
            Self::Failed => f.write_str("Failed"),
        }
    }
}

/// The stored state of one preview key. `TextureHandle` is not `Debug`, hence no derive.
enum PreviewState {
    /// Requested; the worker has not answered yet.
    Pending,
    /// Uploaded texture plus its pixel size.
    Ready {
        texture: egui::TextureHandle,
        size: egui::Vec2,
    },
    /// The render failed; already reported once to the log.
    Failed,
}

/// One cache slot: its state plus the request tick that last asked for it.
struct CacheEntry {
    state: PreviewState,
    /// Value of `LocalPresetPreviewCache::tick` at the last `preview` call for this key.
    /// Drives least-recently-requested eviction.
    last_used: u64,
    /// Which flat grey this preview belongs on ([`LocalPresetPreview::Ready`]).
    ///
    /// Lives on the SLOT rather than on the `Ready` state so it survives the `Pending` ->
    /// `Ready` transition: it is derived from the decoded profile when the render is
    /// requested, which is the one place the profile is read at all.
    backdrop: PreviewBackdrop,
}

/// One render handed to the worker thread. Not `Debug`: `Arc<dyn FontProvider>` is not.
struct RenderRequest {
    key: u64,
    params: TextRenderParams,
    fonts: Arc<dyn FontProvider>,
    max_width_px: u32,
    max_height_px: u32,
}

/// A downscaled preview ready for `ColorImage::from_rgba_unmultiplied`.
///
/// `rgba` is UNMULTIPLIED RGBA8 and is guaranteed to be exactly `width * height * 4`
/// bytes long — the worker rejects anything else rather than letting egui's length
/// assertion panic on the GUI thread.
#[derive(Debug)]
struct PreviewImage {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// One worker answer. `Err` carries the renderer's message for the log.
#[derive(Debug)]
struct RenderResponse {
    key: u64,
    result: Result<PreviewImage, String>,
}

/// The worker thread's two channel ends, created together on the first request.
#[derive(Debug)]
struct Worker {
    requests: Sender<RenderRequest>,
    results: Receiver<RenderResponse>,
}

/// Cache of rendered local-preset previews, plus the worker thread that produces them.
///
/// Owned by `TypingCreatePanelState`. Call [`poll`](Self::poll) once per frame before
/// drawing, then [`preview`](Self::preview) for every row DRAWN this frame — a row that
/// is not drawn must not be asked for, otherwise a long preset list would queue renders
/// nobody looks at.
///
/// Dropping the cache drops the request `Sender`, which ends the worker loop; the thread
/// finishes its current render and exits.
#[derive(Default)]
pub(super) struct LocalPresetPreviewCache {
    entries: HashMap<u64, CacheEntry>,
    worker: Option<Worker>,
    /// `true` once spawning the worker failed, so the failure is reported once and the
    /// cache stops trying on every frame.
    worker_unavailable: bool,
    /// Renders sent to the worker and not yet answered.
    in_flight: usize,
    /// Monotonic request counter feeding `CacheEntry::last_used`.
    tick: u64,
}

impl fmt::Debug for LocalPresetPreviewCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalPresetPreviewCache")
            .field("entries", &self.entries.len())
            .field("in_flight", &self.in_flight)
            .field("worker_started", &self.worker.is_some())
            .field("worker_unavailable", &self.worker_unavailable)
            .finish()
    }
}

impl LocalPresetPreviewCache {
    /// An empty cache. No thread is spawned until the first render is requested, so a
    /// panel whose local-preset combo is never opened pays nothing.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Drops every cached preview (freeing its texture) and forgets the failures.
    ///
    /// The caller MUST use this when it replaces its `FontProvider` — the provider is not
    /// part of the cache key, so previews drawn with the old font set would otherwise
    /// survive a font reload. Renders already in flight are still counted and their
    /// answers are discarded (their keys no longer have a slot).
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drains the worker's answers and uploads the finished images as egui textures.
    ///
    /// Call once per frame, before the rows are drawn, so an image that arrived since the
    /// last frame is visible in THIS one. Requests a repaint while renders are still
    /// outstanding, so a popup that receives no input still refreshes when they land.
    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        // Collected first: `try_recv` borrows `self.worker`, while handling a response
        // needs `&mut self` for the entry map.
        let mut responses = Vec::new();
        if let Some(worker) = self.worker.as_ref() {
            while let Ok(response) = worker.results.try_recv() {
                responses.push(response);
            }
        }
        for response in responses {
            self.in_flight = self.in_flight.saturating_sub(1);
            self.apply_response(ctx, response);
        }
        if self.in_flight > 0 {
            ctx.request_repaint();
        }
    }

    /// The preview for one local preset, requesting a render when the cache has none.
    ///
    /// `name` is the preset's verbatim name (capped to [`PREVIEW_NAME_MAX_CHARS`] here),
    /// `profile` its stored render-data snapshot, `profile_hash` that snapshot's CACHED
    /// hash (`LocalPreset::profile_hash` — this runs once per drawn row per frame, so the
    /// key must not re-serialize the JSON), `fonts` the panel's font provider and
    /// `row_height_px` the height of the combo row in physical pixels — the preview is
    /// never taller than that and never wider than [`MAX_PREVIEW_WIDTH_PX`].
    ///
    /// CALL ONLY FOR A ROW DRAWN THIS FRAME. Every call marks the key as recently used and
    /// may issue a render.
    pub(super) fn preview(
        &mut self,
        name: &str,
        profile: &Value,
        profile_hash: u64,
        fonts: &Arc<dyn FontProvider>,
        row_height_px: f32,
    ) -> LocalPresetPreview<'_> {
        let label = preview_label(name);
        let key = preview_key(label.as_str(), profile_hash, row_height_px);
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        match self.entries.get_mut(&key) {
            Some(entry) => entry.last_used = tick,
            None => self.request(key, label.as_str(), profile, fonts, row_height_px),
        }
        // Re-looked up rather than kept from above: `request` mutates the map, and the
        // returned borrow must come from the final state of it.
        match self.entries.get(&key) {
            Some(entry) => match &entry.state {
                PreviewState::Ready { texture, size } => LocalPresetPreview::Ready {
                    texture,
                    size: *size,
                    backdrop: entry.backdrop,
                },
                PreviewState::Pending => LocalPresetPreview::Pending,
                PreviewState::Failed => LocalPresetPreview::Failed,
            },
            // No slot: the in-flight cap refused this request. The row asks again next
            // frame, which is why nothing is recorded here.
            None => LocalPresetPreview::Pending,
        }
    }

    /// Uploads a finished image, or records the failure, for one worker answer.
    ///
    /// A response whose key no longer has a slot (evicted, or `clear`ed while in flight)
    /// is dropped without uploading anything.
    fn apply_response(&mut self, ctx: &egui::Context, response: RenderResponse) {
        let RenderResponse { key, result } = response;
        if !self.entries.contains_key(&key) {
            return;
        }
        match result {
            Ok(image) => {
                let Some(texture) = upload_preview(ctx, key, &image) else {
                    self.set_failed(key, "preview image has an inconsistent buffer length");
                    return;
                };
                let size = texture.size_vec2();
                if let Some(entry) = self.entries.get_mut(&key) {
                    entry.state = PreviewState::Ready { texture, size };
                }
            }
            Err(err) => self.set_failed(key, err.as_str()),
        }
    }

    /// Marks one key as permanently unpreviewable and reports the reason ONCE.
    ///
    /// The `Failed` slot is sticky, so the row never re-requests the same render in a
    /// tight loop; an edit to the preset produces a different key and is tried again.
    fn set_failed(&mut self, key: u64, reason: &str) {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.state = PreviewState::Failed;
        }
        runtime_log::log_warn(format!(
            "typing::local_preset_preview: render failed, the row falls back to its name. \
             key={key:016x} reason={reason}"
        ));
    }

    /// Decodes the profile and sends one render to the worker, inserting the slot.
    ///
    /// Inserts `Failed` when the profile cannot be decoded into render parameters (a
    /// snapshot naming no font at all) and inserts NOTHING when the in-flight cap is
    /// reached or the worker could not be started — in both of those cases the row is
    /// answered `Pending` and asks again next frame.
    fn request(
        &mut self,
        key: u64,
        label: &str,
        profile: &Value,
        fonts: &Arc<dyn FontProvider>,
        row_height_px: f32,
    ) {
        // The cap is checked BEFORE decoding: a capped row is re-asked on every frame while
        // the popup is open, and decoding its profile each time would be GUI-thread work
        // thrown away.
        if self.in_flight >= MAX_RENDERS_IN_FLIGHT {
            return;
        }
        let Some(params) = preview_params(profile, label) else {
            self.evict_to_fit();
            // A `Failed` row draws its name as text and never reaches a backdrop, so the
            // choice is arbitrary here; the light grey is the neutral default.
            self.insert(key, PreviewState::Failed, PreviewBackdrop::Light);
            runtime_log::log_warn(format!(
                "typing::local_preset_preview: the preset profile names no usable font, so it \
                 cannot be rendered; the row falls back to its name. key={key:016x}"
            ));
            return;
        };
        // Decided HERE, off the row's per-frame path: the profile is already decoded and in
        // hand, so the backdrop choice costs one walk of the effect array, while doing it in
        // the row would parse JSON once per drawn row per frame on the GUI thread.
        let backdrop = preview_backdrop(profile, &params);
        let max_height_px = row_height_to_max_px(row_height_px);
        let request = RenderRequest {
            key,
            params,
            fonts: Arc::clone(fonts),
            max_width_px: MAX_PREVIEW_WIDTH_PX,
            max_height_px,
        };
        if self.ensure_worker().is_none() {
            return;
        }
        // The send result is taken by value first: reacting to a failure needs `&mut self`,
        // which must not overlap the borrow of `self.worker`.
        let send_result = match self.worker.as_ref() {
            Some(worker) => worker.requests.send(request),
            None => return,
        };
        if let Err(err) = send_result {
            // The worker thread is gone (it can only end by this very sender being
            // dropped, so this is a genuine anomaly). Stop using it and report once.
            runtime_log::log_error(format!(
                "typing::local_preset_preview: the preview worker is gone, previews are \
                 disabled for this session. err={err}"
            ));
            self.worker = None;
            self.worker_unavailable = true;
            return;
        }
        self.in_flight += 1;
        self.evict_to_fit();
        self.insert(key, PreviewState::Pending, backdrop);
    }

    /// The worker thread's channel ends, starting the thread on first use.
    ///
    /// `None` once a spawn has failed; the failure is reported once and never retried,
    /// so a machine that cannot spawn threads does not log on every frame.
    fn ensure_worker(&mut self) -> Option<&Worker> {
        if self.worker.is_none() {
            if self.worker_unavailable {
                return None;
            }
            let (request_tx, request_rx) = mpsc::channel::<RenderRequest>();
            let (result_tx, result_rx) = mpsc::channel::<RenderResponse>();
            let spawned = thread::Builder::new()
                .name(WORKER_THREAD_NAME.to_string())
                .spawn(move || run_preview_worker(&request_rx, &result_tx));
            match spawned {
                Ok(_handle) => {
                    // The handle is intentionally dropped: the thread is detached and ends
                    // when `request_tx` (owned by this cache) is dropped.
                    self.worker = Some(Worker {
                        requests: request_tx,
                        results: result_rx,
                    });
                }
                Err(err) => {
                    self.worker_unavailable = true;
                    runtime_log::log_error(format!(
                        "typing::local_preset_preview: could not start the `{WORKER_THREAD_NAME}` \
                         worker, previews are disabled for this session. err={err}"
                    ));
                    return None;
                }
            }
        }
        self.worker.as_ref()
    }

    /// Inserts a slot for `key` with the current request tick.
    ///
    /// `backdrop` is the grey the finished preview will be drawn on; it is fixed here
    /// because this is where the profile has just been decoded.
    fn insert(&mut self, key: u64, state: PreviewState, backdrop: PreviewBackdrop) {
        let last_used = self.tick;
        self.entries.insert(
            key,
            CacheEntry {
                state,
                last_used,
                backdrop,
            },
        );
    }

    /// Evicts least-recently-requested slots until one more fits under
    /// [`MAX_CACHED_PREVIEWS`].
    ///
    /// `Pending` slots are never evicted — their worker answer would then be discarded and
    /// the row would re-request the same render forever. At most
    /// [`MAX_RENDERS_IN_FLIGHT`] slots can be pending, so this can never stall the cache.
    fn evict_to_fit(&mut self) {
        while self.entries.len() >= MAX_CACHED_PREVIEWS {
            let victim = self
                .entries
                .iter()
                .filter(|(_, entry)| !matches!(entry.state, PreviewState::Pending))
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key);
            let Some(key) = victim else {
                break;
            };
            // Removing the entry drops its `TextureHandle`, which frees the GPU texture.
            self.entries.remove(&key);
        }
    }
}

/// The text a preview draws for `name`: at most [`PREVIEW_NAME_MAX_CHARS`] characters, on
/// one line.
///
/// The cap counts CHARACTERS, so a multi-byte name is never cut mid-character. Line
/// separators inside a name are replaced with spaces, because the preview's contract is a
/// single line and a name is user data that must not be silently truncated at the break.
/// Exposed so the caller draws exactly the same string as the text fallback for a
/// `Pending` or `Failed` row.
#[must_use]
pub(super) fn preview_label(name: &str) -> String {
    name.chars()
        .take(PREVIEW_NAME_MAX_CHARS)
        .map(|ch| {
            if matches!(ch, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

/// The cache key of one preview: a 64-bit hash of everything the render depends on.
///
/// The inputs are the CAPPED label, the preset's CACHED profile hash
/// ([`super::local_preset_profile_hash`], which folds in every text parameter AND the whole
/// effect chain), and the target row height. Changing any single parameter or any effect
/// changes the profile hash and therefore the key — which is exactly what invalidates the
/// cached texture.
///
/// THE PROFILE IS NOT SERIALIZED HERE. This runs on the GUI thread once per drawn combo row
/// per frame; re-serializing every visible preset's JSON to derive a key that only changes
/// when the preset is edited was pure per-frame waste, so the hash is computed where the
/// snapshot is written (`LocalPreset::set_profile`).
///
/// The key is in-process only; it is never persisted and never compared across runs.
#[must_use]
fn preview_key(label: &str, profile_hash: u64, row_height_px: f32) -> u64 {
    let mut hasher = DefaultHasher::new();
    label.hash(&mut hasher);
    profile_hash.hash(&mut hasher);
    row_height_px.to_bits().hash(&mut hasher);
    hasher.finish()
}

/// The maximum preview HEIGHT in pixels for a combo row of `row_height_px`.
///
/// A non-finite or non-positive row height (a layout not measured yet) falls back to one
/// pixel rather than producing a nonsensical target.
#[must_use]
fn row_height_to_max_px(row_height_px: f32) -> u32 {
    if !row_height_px.is_finite() || row_height_px < 1.0 {
        return 1;
    }
    // Clamped before the conversion: a "row" taller than 65535 px is not a combo row, and
    // bounding it here is what makes the cast below provably exact.
    let clamped = row_height_px.min(f32::from(u16::MAX)).floor();
    // Justified numeric cast (`CLAUDE.md` §17): `clamped` is finite, >= 1, integral and
    // <= 65535 by the two lines above, so no truncation or data loss is possible.
    let px = clamped as u32;
    px.max(1)
}

/// The render parameters of one preview: the preset's own parameters, forced onto one line
/// and drawing `label` instead of the preset's stored text.
///
/// Returns `None` when the profile carries no decodable `text_params` (in practice: no
/// font name at all), which the caller turns into a `Failed` slot.
///
/// Exactly four fields are overridden, and each one is a way the renderer would otherwise
/// produce more than one line:
/// - `text` — the drawn string is the preset NAME, not the preset's stored text;
/// - `text_wrap_mode` -> `None` — automatic wrapping (`crates/ms-text-render/src/types.rs:415`);
/// - `new_line_after_sentence` -> `false` — `prepare_source_text` inserts a break after
///   every sentence end (`crates/ms-text-render/src/pipeline.rs:2616`);
/// - `enable_inline_style_tags` -> `false` — with tags on, a `<br>` inside a NAME parses
///   into `'\n'` (`crates/ms-text-render/src/inline_styles.rs:325`), and every other tag
///   spelling would be silently eaten out of the displayed name.
///
/// Everything else — font, size, colour, faux bold/italic, shape, layout mode, anti-
/// aliasing and the entire effect chain (`effects_json`) — is left exactly as the preset
/// stores it: showing those IS the point of the preview.
#[must_use]
fn preview_params(profile: &Value, label: &str) -> Option<TextRenderParams> {
    let mut params = crate::tabs::typing::tab::codec::text_render_params_from_render_data(profile)?;
    params.text = label.to_string();
    params.text_wrap_mode = TextWrapMode::None;
    params.new_line_after_sentence = false;
    params.enable_inline_style_tags = false;
    Some(params)
}

/// Which of the three flat greys one preview must be drawn on.
///
/// THE RULE, in one place. The two colours that decide it, in the user's stated order of
/// priority, are the preset's LAST VISIBLE OUTLINE colour (weight
/// [`BACKDROP_OUTLINE_WEIGHT`]) and its MAIN text colour (weight 1). The main colour comes
/// from the DECODED [`TextRenderParams::text_color`] and never from the raw JSON: schema 2
/// STRIPS `text_color` when it equals the frozen default `[0, 0, 0, 255]`
/// (`panel/text_params_schema.rs`), so the raw key is absent for exactly the commonest
/// preset there is.
///
/// Called once per REQUESTED render, never per drawn row: it walks the effect array.
#[must_use]
fn preview_backdrop(profile: &Value, params: &TextRenderParams) -> PreviewBackdrop {
    let main_luminance = render_store::shape_variant_luminance(params.text_color);
    let outline_luminance =
        last_visible_outline_color(profile).map(render_store::shape_variant_luminance);
    choose_preview_backdrop(main_luminance, outline_luminance)
}

/// Picks the backdrop that contrasts best with a preset's own colours.
///
/// THE FORMULA. All luminances are Rec.709 over white on a 0..255 scale
/// (`render_store::shape_variant_luminance`) and `contrast(grey, colour)` is
/// `|luminance(grey) - luminance(colour)|`. Writing `T` for
/// [`BACKDROP_CONTRAST_ENOUGH`] and `W` for [`BACKDROP_OUTLINE_WEIGHT`], each candidate
/// grey is scored as the PAIR
///
/// ```text
/// ( W * min(contrast_outline, T) + min(contrast_main, T) ,
///   W * contrast_outline         + contrast_main         )
/// ```
///
/// compared LEXICOGRAPHICALLY, and the highest wins. When the preset has no visible
/// outline (`outline_luminance` is `None`) the two `W * …` terms are dropped entirely
/// rather than defaulted, so an outline-less preset is judged on its text colour alone.
///
/// The saturation at `T` in the FIRST component is what the whole design turns on: past
/// "good enough" extra contrast buys nothing, so a grey that is merely good for BOTH
/// colours beats one that is perfect for a single colour and invisible for the other —
/// which is how white-text-with-a-black-outline lands on the MEDIUM grey instead of on an
/// extreme that would swallow one of its two colours. The unsaturated SECOND component
/// only breaks ties between greys the first component rates equally.
///
/// Ties are settled by the order of [`PreviewBackdrop::ALL`] (lightest first).
#[must_use]
fn choose_preview_backdrop(
    main_luminance: f32,
    outline_luminance: Option<f32>,
) -> PreviewBackdrop {
    let score = |backdrop: PreviewBackdrop| -> (f32, f32) {
        let grey = backdrop.luminance();
        let main = (grey - main_luminance).abs();
        match outline_luminance {
            Some(outline_luminance) => {
                let outline = (grey - outline_luminance).abs();
                (
                    BACKDROP_OUTLINE_WEIGHT * outline.min(BACKDROP_CONTRAST_ENOUGH)
                        + main.min(BACKDROP_CONTRAST_ENOUGH),
                    BACKDROP_OUTLINE_WEIGHT * outline + main,
                )
            }
            None => (main.min(BACKDROP_CONTRAST_ENOUGH), main),
        }
    };
    let mut best = PreviewBackdrop::ALL[0];
    let mut best_score = score(best);
    for candidate in PreviewBackdrop::ALL.into_iter().skip(1) {
        let candidate_score = score(candidate);
        // `partial_cmp` on the tuple is the lexicographic comparison the formula asks for.
        // Only a strict `Greater` replaces the incumbent, which is what makes the order of
        // `ALL` the documented tie-breaker; a `None` (impossible for finite inputs) also
        // keeps the incumbent rather than picking arbitrarily.
        if matches!(
            candidate_score.partial_cmp(&best_score),
            Some(std::cmp::Ordering::Greater)
        ) {
            best = candidate;
            best_score = candidate_score;
        }
    }
    best
}

/// The colour of the LAST VISIBLE outline-like effect of a stored preset profile.
///
/// The `effects` array is applied FRONT TO BACK and every outline-like effect composites
/// UNDER the source (`crates/ms-text-render/src/effects/stroke_shadow.rs`,
/// `.../glow.rs`), so each later one wraps around everything before it: the LAST matching
/// element is the outermost band a viewer actually sees. Hence the reverse iteration.
///
/// Returns `None` when the profile has no `effects` array or no element of it is a visible
/// outline. A missing array is normal — `create_render_data` omits `effects` when the chain
/// is empty.
#[must_use]
fn last_visible_outline_color(profile: &Value) -> Option<[u8; 4]> {
    profile
        .get("effects")?
        .as_array()?
        .iter()
        .rev()
        .find_map(visible_outline_color)
}

/// The colour of ONE effect element if it is a VISIBLE outline, else `None`.
///
/// Outline-like kinds are `stroke`, `glow_v1`, `glow_v2` and `soft_glow` (plus the
/// renderer's read aliases `glow` and `glow_soft`, `effects/parse.rs:352-372`). `shadow` is
/// deliberately NOT one of them: it is an OFFSET drop shadow, not a contour, so it does not
/// wrap the glyphs and is not what the user means by "the last outline layer".
///
/// An element is skipped when it cannot be seen, by exactly the gates the renderer itself
/// applies: `"enabled": false` (`parse.rs:338-341`), a PREPROCESS stage
/// (`parse.rs:342-343`, `:434`), a stroke of non-positive `width`
/// (`stroke_shadow.rs:22-25`), `transparency` at 100 percent, or a colour whose alpha is 0.
#[must_use]
fn visible_outline_color(effect: &Value) -> Option<[u8; 4]> {
    let object = effect.as_object()?;
    if object.get("enabled").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    if is_preprocess_stage(object) {
        return None;
    }
    let kind = object
        .get("effect")
        .or_else(|| object.get("type"))
        .and_then(Value::as_str)?
        .trim()
        .to_ascii_lowercase();
    match kind.as_str() {
        "stroke" => {
            // A zero-width stroke is a no-op in the renderer, so it is not a visible band.
            if object.get("width").and_then(Value::as_f64).unwrap_or(0.0) <= 0.0 {
                return None;
            }
        }
        "glow_v1" | "glow_v2" | "glow" | "soft_glow" | "glow_soft" => {}
        _ => return None,
    }
    // `transparency` is a PERCENT and `opacity` its complement; both are written, so only
    // one of them needs to be read. `soft_glow` carries neither — its alpha lives in the
    // colour — and a missing key therefore means "fully opaque".
    if object
        .get("transparency")
        .and_then(Value::as_f64)
        .is_some_and(|transparency| transparency >= 100.0)
    {
        return None;
    }
    let color = effect_color_rgba(object.get("color")?)?;
    if color[3] == 0 {
        return None;
    }
    Some(color)
}

/// Whether one effect element runs BEFORE the post-effect pipeline and therefore never
/// paints a band around the glyphs.
///
/// Mirrors `crates/ms-text-render/src/effects/parse.rs:434 parse_effect_stage`, including
/// its alias spellings; an unknown or missing stage is a POST effect there and here.
#[must_use]
fn is_preprocess_stage(object: &serde_json::Map<String, Value>) -> bool {
    object
        .get("effect_type")
        .or_else(|| object.get("stage"))
        .and_then(Value::as_str)
        .is_some_and(|stage| {
            matches!(
                stage.trim().to_ascii_lowercase().as_str(),
                "preprocess"
                    | "pre_processor"
                    | "pre-processing"
                    | "text_preprocess"
                    | "base_render"
                    | "base-render"
                    | "base"
                    | "render"
            )
        })
}

/// Straight-alpha RGBA8 of an effect's `"color"` value.
///
/// The panel always writes a 4-element array (`panel/effect_cards.rs`), but old and
/// hand-edited profiles also reach the renderer as `[r, g, b]` or `{"r":…,"g":…,"b":…}`,
/// which it accepts (`effects/parse.rs:1444-1486`); a missing alpha is opaque. Returns
/// `None` for any other shape, which the caller treats as "not a usable outline".
#[must_use]
fn effect_color_rgba(value: &Value) -> Option<[u8; 4]> {
    if let Some(array) = value.as_array() {
        if array.len() != 3 && array.len() != 4 {
            return None;
        }
        let r = value_as_u8(array.first()?)?;
        let g = value_as_u8(array.get(1)?)?;
        let b = value_as_u8(array.get(2)?)?;
        let a = array.get(3).and_then(value_as_u8).unwrap_or(255);
        return Some([r, g, b, a]);
    }
    if let Some(object) = value.as_object() {
        let r = value_as_u8(object.get("r")?)?;
        let g = value_as_u8(object.get("g")?)?;
        let b = value_as_u8(object.get("b")?)?;
        let a = object.get("a").and_then(value_as_u8).unwrap_or(255);
        return Some([r, g, b, a]);
    }
    None
}

/// The worker loop: renders one request at a time until the request channel closes.
///
/// Runs on the `typing-local-preset-preview` thread. A send failure means the cache was
/// dropped, which ends the loop. Errors are carried back in the response, never panicked.
fn run_preview_worker(requests: &Receiver<RenderRequest>, results: &Sender<RenderResponse>) {
    while let Ok(request) = requests.recv() {
        let key = request.key;
        let result = render_preview(request);
        if results.send(RenderResponse { key, result }).is_err() {
            break;
        }
    }
}

/// Renders one preview and downscales it to the row's target box.
///
/// Runs OFF the GUI thread. The full effect chain is inside `render_text_to_image`
/// (`crates/ms-text-render/src/pipeline.rs:1241`), which also trims the result to its
/// alpha bounds, so the returned image is tight around the drawn ink.
///
/// # Errors
/// Returns the renderer's own message (a missing font reads as `шрифт '…' не найден…`),
/// or a description of an inconsistent render buffer.
fn render_preview(request: RenderRequest) -> Result<PreviewImage, String> {
    let RenderRequest {
        key: _,
        params,
        fonts,
        max_width_px,
        max_height_px,
    } = request;
    let rendered = render_text_to_image(&params, fonts.as_ref(), None)?;
    let src_width = rendered.width;
    let src_height = rendered.height;
    if src_width == 0 || src_height == 0 {
        return Err("the renderer produced an empty image".to_string());
    }
    let expected_len = pixel_buffer_len(src_width, src_height)
        .ok_or_else(|| "the rendered image is too large to address".to_string())?;
    if rendered.rgba.len() != expected_len {
        return Err(format!(
            "the rendered buffer is {} bytes for {src_width}x{src_height} (expected {expected_len})",
            rendered.rgba.len()
        ));
    }
    let (dst_width, dst_height) =
        preview_target_size(src_width, src_height, max_width_px, max_height_px);
    if dst_width == src_width && dst_height == src_height {
        return Ok(PreviewImage {
            width: src_width,
            height: src_height,
            rgba: rendered.rgba,
        });
    }
    let source = RgbaImage::from_raw(src_width, src_height, rendered.rgba)
        .ok_or_else(|| "the rendered buffer does not fit its declared size".to_string())?;
    // Resampling must happen in PREMULTIPLIED space: the renderer returns unmultiplied
    // RGBA whose fully transparent pixels are [0, 0, 0, 0], so a straight Triangle filter
    // would drag black into the edges of light-coloured glyphs.
    let mut premultiplied = source;
    premultiply_in_place(&mut premultiplied);
    let mut resized = image::imageops::resize(
        &premultiplied,
        dst_width,
        dst_height,
        FilterType::Triangle,
    );
    unpremultiply_in_place(&mut resized);
    Ok(PreviewImage {
        width: dst_width,
        height: dst_height,
        rgba: resized.into_raw(),
    })
}

/// Byte length of a `width * height` RGBA8 buffer, or `None` on overflow.
#[must_use]
fn pixel_buffer_len(width: u32, height: u32) -> Option<usize> {
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)
}

/// Scales `src` down so it fits inside `max_width` x `max_height`, preserving the aspect
/// ratio; never scales UP.
///
/// Returns the source size unchanged when it already fits. Both returned dimensions are at
/// least 1, so an extremely wide strip degenerates to a one-pixel-tall image rather than to
/// an empty one. The arithmetic is integer-only (`u128` products) so no rounding cast is
/// involved and the result cannot overflow.
#[must_use]
fn preview_target_size(src_width: u32, src_height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    let src_width = src_width.max(1);
    let src_height = src_height.max(1);
    let max_width = max_width.max(1);
    let max_height = max_height.max(1);
    // The scale factor is kept as the exact fraction `num / den`, starting at 1/1 and
    // tightened by whichever of the two limits binds harder.
    let (mut num, mut den) = (1u128, 1u128);
    if src_height > max_height {
        num = u128::from(max_height);
        den = u128::from(src_height);
    }
    // The width limit binds harder when `max_width / src_width < num / den`, cross-multiplied
    // to stay in integers.
    if u128::from(max_width) * den < num * u128::from(src_width) {
        num = u128::from(max_width);
        den = u128::from(src_width);
    }
    let width = (u128::from(src_width) * num / den).max(1);
    let height = (u128::from(src_height) * num / den).max(1);
    (
        u32::try_from(width).unwrap_or(src_width),
        u32::try_from(height).unwrap_or(src_height),
    )
}

/// Multiplies every colour channel by its own alpha, in place.
fn premultiply_in_place(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = u16::from(pixel.0[3]);
        for channel in &mut pixel.0[..3] {
            // Rounded 8-bit multiply; the product is at most 255*255+127 < u16::MAX and the
            // quotient is at most 255, so the `u8` conversion below cannot fail.
            let scaled = (u16::from(*channel) * alpha + 127) / 255;
            *channel = u8::try_from(scaled).unwrap_or(u8::MAX);
        }
    }
}

/// Divides every colour channel by its own alpha, in place — the inverse of
/// [`premultiply_in_place`], applied after resampling.
///
/// A fully transparent pixel keeps its (meaningless) colour channels: dividing by zero has
/// no defined answer and the channels are invisible anyway.
fn unpremultiply_in_place(image: &mut RgbaImage) {
    for pixel in image.pixels_mut() {
        let alpha = u16::from(pixel.0[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel.0[..3] {
            let restored = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = u8::try_from(restored.min(255)).unwrap_or(u8::MAX);
        }
    }
}

/// Uploads one finished preview as an egui texture, or `None` when the buffer length does
/// not match the declared size.
///
/// The length is re-checked HERE because `ColorImage::from_rgba_unmultiplied`
/// (`epaint-0.35.0/src/image.rs:113`) asserts on a mismatch, and an assertion on the GUI
/// thread would take the whole app down for a bad preview.
#[must_use]
fn upload_preview(
    ctx: &egui::Context,
    key: u64,
    image: &PreviewImage,
) -> Option<egui::TextureHandle> {
    let width = usize::try_from(image.width).ok()?;
    let height = usize::try_from(image.height).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    if image.rgba.len() != pixel_buffer_len(image.width, image.height)? {
        return None;
    }
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied([width, height], image.rgba.as_slice());
    Some(ctx.load_texture(
        format!("typing_local_preset_preview_{key:016x}"),
        color_image,
        egui::TextureOptions::LINEAR,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preview_label_caps_by_characters_not_bytes() {
        // 40 Cyrillic characters = 80 bytes; the cap must count characters.
        let name: String = "я".repeat(40);
        let label = preview_label(name.as_str());
        assert_eq!(label.chars().count(), PREVIEW_NAME_MAX_CHARS);
        assert_eq!(label.len(), PREVIEW_NAME_MAX_CHARS * 2);
    }

    #[test]
    fn preview_label_keeps_short_names_verbatim() {
        assert_eq!(preview_label(" Рао-кун "), " Рао-кун ");
        assert_eq!(preview_label(""), "");
    }

    #[test]
    fn preview_label_flattens_line_breaks_to_spaces() {
        assert_eq!(preview_label("одна\nдве\r\nтри"), "одна две  три");
    }

    #[test]
    fn preview_key_changes_with_every_input() {
        let profile = json!({"text_params": {"schema": 2, "font": "Test"}});
        let hash = super::super::local_preset_profile_hash(&profile);
        let base = preview_key("Имя", hash, 24.0);
        assert_eq!(base, preview_key("Имя", hash, 24.0));
        assert_ne!(base, preview_key("Имя2", hash, 24.0));
        assert_ne!(base, preview_key("Имя", hash, 25.0));
        let other_param = json!({"text_params": {"schema": 2, "font": "Test", "font_size_px": 40}});
        assert_ne!(
            base,
            preview_key(
                "Имя",
                super::super::local_preset_profile_hash(&other_param),
                24.0
            )
        );
    }

    /// The CACHED hash keeps the contract the key used to get from re-serializing the
    /// profile on every frame: it changes whenever the rendered pixels would. An effect
    /// added to the chain is the case a text-parameter-only hash would miss.
    #[test]
    fn preview_key_changes_when_only_an_effect_changes() {
        let without = json!({"text_params": {"schema": 2, "font": "Test"}, "effects": []});
        let with = json!({
            "text_params": {"schema": 2, "font": "Test"},
            "effects": [{"type": "stroke", "width": 3}],
        });
        assert_ne!(
            preview_key(
                "Имя",
                super::super::local_preset_profile_hash(&without),
                24.0
            ),
            preview_key("Имя", super::super::local_preset_profile_hash(&with), 24.0)
        );
    }

    /// The cached hash a `LocalPreset` carries IS the value the key must be built from, and
    /// it follows the snapshot through a rewrite — the invariant `set_profile` maintains.
    #[test]
    fn a_local_preset_carries_the_hash_of_its_current_snapshot() {
        let first = json!({"text_params": {"schema": 2, "font_size_px": 10}});
        let second = json!({"text_params": {"schema": 2, "font_size_px": 11}});
        let mut preset = super::super::LocalPreset::new("П".to_string(), first.clone());
        assert_eq!(
            preset.profile_hash(),
            super::super::local_preset_profile_hash(&first)
        );

        preset.set_profile(second.clone());

        assert_eq!(preset.profile(), &second);
        assert_eq!(
            preset.profile_hash(),
            super::super::local_preset_profile_hash(&second),
            "a rewritten snapshot must re-hash, or the preview would keep the old texture",
        );
    }

    #[test]
    fn preview_target_size_keeps_a_small_image_untouched() {
        assert_eq!(preview_target_size(100, 20, 320, 24), (100, 20));
    }

    #[test]
    fn preview_target_size_scales_by_height_when_height_binds() {
        // 200x48 into 320x24 -> the height halves, the width follows.
        assert_eq!(preview_target_size(200, 48, 320, 24), (100, 24));
    }

    #[test]
    fn preview_target_size_scales_by_width_when_width_binds() {
        // 1280x24 into 320x24 -> the width is the tighter limit, so the result is SHORTER
        // than the row rather than wider than the cap.
        assert_eq!(preview_target_size(1280, 24, 320, 24), (320, 6));
    }

    #[test]
    fn preview_target_size_never_returns_zero() {
        assert_eq!(preview_target_size(10_000, 1, 320, 24), (320, 1));
        assert_eq!(preview_target_size(0, 0, 320, 24), (1, 1));
    }

    #[test]
    fn row_height_to_max_px_rejects_nonsense() {
        assert_eq!(row_height_to_max_px(f32::NAN), 1);
        assert_eq!(row_height_to_max_px(-4.0), 1);
        assert_eq!(row_height_to_max_px(0.5), 1);
        assert_eq!(row_height_to_max_px(24.0), 24);
        assert_eq!(row_height_to_max_px(24.9), 24);
    }

    #[test]
    fn preview_params_force_a_single_line() {
        let profile = json!({
            "text_params": {
                "schema": 2,
                "font": "Test",
                "text": "совсем другой текст",
                "text_wrap_mode": "aggressive",
                "new_line_after_sentence": true,
                "enable_inline_style_tags": true,
            },
            "effects": [],
        });
        let Some(params) = preview_params(&profile, "Имя пресета") else {
            panic!("a schema-2 profile that names a font must decode");
        };
        assert_eq!(params.text, "Имя пресета");
        assert_eq!(params.text_wrap_mode, TextWrapMode::None);
        assert!(!params.new_line_after_sentence);
        assert!(!params.enable_inline_style_tags);
    }

    #[test]
    fn preview_params_keep_the_effect_chain_and_the_font() {
        let profile = json!({
            "text_params": {"schema": 2, "font": "Test", "font_size_px": 48},
            "effects": [{"type": "stroke", "width": 3}],
        });
        let Some(params) = preview_params(&profile, "Имя") else {
            panic!("a schema-2 profile that names a font must decode");
        };
        assert_eq!(params.font_name, "Test");
        assert!((params.font_size_px - 48.0).abs() < f32::EPSILON);
        assert!(params.effects_json.contains("stroke"));
    }

    #[test]
    fn preview_params_reject_a_profile_without_text_params() {
        assert!(preview_params(&json!({"effects": []}), "Имя").is_none());
    }

    /// Luminance of an opaque colour, used to phrase the backdrop cases in the same 0..255
    /// scale the formula works in.
    fn luma(color: [u8; 4]) -> f32 {
        render_store::shape_variant_luminance(color)
    }

    /// THE FOUR ACCEPTANCE CASES of the backdrop formula, in one place. They are what pin
    /// the saturation at `BACKDROP_CONTRAST_ENOUGH`: drop it and the third case collapses
    /// onto an extreme that swallows one of its two colours.
    #[test]
    fn the_backdrop_formula_answers_the_four_acceptance_cases() {
        let black = luma([0, 0, 0, 255]);
        let white = luma([255, 255, 255, 255]);
        let mid = luma([128, 128, 128, 255]);

        assert_eq!(
            choose_preview_backdrop(black, None),
            PreviewBackdrop::Light,
            "black text without an outline belongs on the LIGHT grey"
        );
        assert_eq!(
            choose_preview_backdrop(white, None),
            PreviewBackdrop::Dark,
            "white text without an outline belongs on the DARK grey"
        );
        assert_eq!(
            choose_preview_backdrop(white, Some(black)),
            PreviewBackdrop::Medium,
            "white text in a black outline needs a grey that shows BOTH"
        );
        assert_ne!(
            choose_preview_backdrop(mid, None),
            PreviewBackdrop::Medium,
            "mid-grey text must be pushed onto an extreme, never onto its own value"
        );
    }

    /// The outline OUTWEIGHS the main colour, which is the user's stated priority: adding an
    /// outline FLIPS a choice the text colour alone had already made.
    ///
    /// Black text alone asks for the LIGHT grey. Wrap it in a light-grey outline — light
    /// enough to be lost on the light backdrop, dark enough that the MEDIUM grey does not
    /// show it either — and the answer moves to the DARK grey, which is the only one that
    /// shows the outline. The two colours must NOT be the two extremes here: black and white
    /// together are exactly the case the MEDIUM grey exists for, so they would prove nothing
    /// about the weighting.
    #[test]
    fn the_outline_outweighs_the_main_colour() {
        let black = luma([0, 0, 0, 255]);
        let light_grey_outline = luma([160, 160, 160, 255]);
        assert_eq!(
            choose_preview_backdrop(black, None),
            PreviewBackdrop::Light,
            "the text colour alone asks for the light grey"
        );
        assert_eq!(
            choose_preview_backdrop(black, Some(light_grey_outline)),
            PreviewBackdrop::Dark,
            "the outline must override that"
        );
    }

    /// A fully transparent colour is judged as what the user will see - the white the
    /// preview is composited over - so it behaves exactly like white ink.
    #[test]
    fn a_transparent_preset_colour_is_judged_as_light_ink() {
        let profile = json!({
            "text_params": {"schema": 2, "font": "Test", "text_color": [0, 0, 0, 0]},
            "effects": [],
        });
        let Some(params) = preview_params(&profile, "Preset") else {
            panic!("a schema-2 profile that names a font must decode");
        };
        assert_eq!(
            preview_backdrop(&profile, &params),
            PreviewBackdrop::Dark,
            "an invisible colour is judged as the white it is composited over"
        );
    }

    /// The LAST visible outline wins, because the effect array is applied front to back and
    /// every outline composites UNDER the source.
    #[test]
    fn the_last_visible_outline_wins() {
        let profile = json!({
            "effects": [
                {"effect": "stroke", "enabled": true, "width": 4, "color": [255, 0, 0, 255]},
                {"effect": "soft_glow", "enabled": true, "color": [0, 255, 0, 255]},
            ],
        });
        assert_eq!(last_visible_outline_color(&profile), Some([0, 255, 0, 255]));
    }

    /// Every gate the renderer applies is applied here too, so an outline the user cannot
    /// see never decides the backdrop. Each element below is invisible for a different
    /// reason, so the only one left is the stroke at the front.
    #[test]
    fn invisible_outlines_are_skipped() {
        let profile = json!({
            "effects": [
                {"effect": "stroke", "enabled": true, "width": 3, "color": [10, 20, 30, 255]},
                {"effect": "stroke", "enabled": false, "width": 3, "color": [1, 1, 1, 255]},
                {"effect": "stroke", "enabled": true, "width": 0, "color": [2, 2, 2, 255]},
                {"effect": "stroke", "enabled": true, "width": 3, "color": [3, 3, 3, 0]},
                {"effect": "glow_v2", "enabled": true, "transparency": 100, "color": [4, 4, 4, 255]},
                {"effect": "text_shake", "effect_type": "preprocess", "enabled": true, "color": [5, 5, 5, 255]},
                {"effect": "shadow", "enabled": true, "color": [6, 6, 6, 255]},
                {"effect": "blur", "enabled": true, "radius": 4},
            ],
        });
        assert_eq!(last_visible_outline_color(&profile), Some([10, 20, 30, 255]));
    }

    /// A profile with no effect chain at all is the common case: `create_render_data` omits
    /// the key entirely when the chain is empty.
    #[test]
    fn a_profile_without_effects_has_no_outline() {
        assert_eq!(last_visible_outline_color(&json!({})), None);
        assert_eq!(last_visible_outline_color(&json!({"effects": []})), None);
        assert_eq!(
            last_visible_outline_color(&json!({"effects": "not an array"})),
            None
        );
    }

    /// The three colour shapes the renderer tolerates on read all decode; anything else is
    /// refused rather than guessed at.
    #[test]
    fn effect_colours_accept_every_shape_the_renderer_reads() {
        assert_eq!(effect_color_rgba(&json!([1, 2, 3, 4])), Some([1, 2, 3, 4]));
        assert_eq!(effect_color_rgba(&json!([1, 2, 3])), Some([1, 2, 3, 255]));
        assert_eq!(
            effect_color_rgba(&json!({"r": 1, "g": 2, "b": 3, "a": 4})),
            Some([1, 2, 3, 4])
        );
        assert_eq!(
            effect_color_rgba(&json!({"r": 1, "g": 2, "b": 3})),
            Some([1, 2, 3, 255])
        );
        assert_eq!(effect_color_rgba(&json!([1, 2])), None);
        assert_eq!(effect_color_rgba(&json!("#ff0000")), None);
    }

    /// The three greys are NEUTRAL, so each one's luminance IS its level - the property the
    /// contrast arithmetic relies on to skip a colour conversion.
    #[test]
    fn every_backdrop_grey_is_neutral() {
        for backdrop in PreviewBackdrop::ALL {
            let fill = backdrop.fill().to_srgba_unmultiplied();
            let level = backdrop.level();
            assert_eq!(fill, [level, level, level, 255]);
            assert!(
                (luma(fill) - backdrop.luminance()).abs() < 0.5,
                "{backdrop:?}: {} vs {}",
                luma(fill),
                backdrop.luminance()
            );
        }
    }
}
