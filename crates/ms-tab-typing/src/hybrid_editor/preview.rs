/*
File: hybrid_editor/preview.rs

Purpose:
The hybrid editor's live preview (decision D5): ONE worker thread per editor renders the
sample text in a SUBSET hybrid of the current draft (only the sample's characters are
compiled) and in each chosen source font for comparison, and computes the draft's coverage
report. The GUI thread only schedules requests, drains results and uploads textures.

Key structures:
- `PreviewEngine`: per-editor scheduler + result slots (owned by `HybridEditorState`).
- `PreviewRequest` / `PreviewResponse`: the worker protocol.
- `LatestWins`: the pure generation rule (a response older than the one shown is dropped).
- `PreviewSlot`: what one image area shows this frame (texture / pending / failed / none).

Key functions:
- `PreviewEngine::tick`: per frame — drain results, upload textures, send a request when the
  draft changed and the coalescing delay passed, schedule repaints.
- `preview_chars`: the subset compiled for a sample text.
- `run_worker` / `process`: the worker side (resolve sources, subset compile through
  `custom_fonts::service`, render with `ms-text-render`, coverage report).

Notes:
- Latest-wins: every request gets a higher generation; sending a new one raises the previous
  request's cancel flag (stops its subset compile) and the shared generation counter stops
  its renders; the worker also skips queued requests that are not the newest. The previous
  image stays on screen until a newer one arrives.
- Dropping the engine (its editor closed) raises the in-flight request's cancel flag, so an
  abandoned subset compile stops instead of running to a result nobody reads.
- Edits are coalesced (`COALESCE_DELAY`), so dragging a weight does not queue one compile per
  frame.
- Renders are white glyphs at the display pixel size (no resampling); the GUI tints them
  with the text colour, so a theme change needs no re-render.
- The preview never registers an egui font (egui cannot unload one) and never compiles a
  full face; the subset compile is uncached by design (`service::subset_preview_blocking`).
- Source comparison renders are memoized on the worker by (font bytes, text, size), so a
  weight edit re-renders only the hybrid.
- The hybrid's OWN user kerning pairs (a saved font's) are not applied to the preview; the
  sources' effective pairs are, inside the subset face and on the comparison rows.
*/

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use eframe::egui;
use ms_custom_fonts::{CoverageReport, CustomFontRecipe, coverage_report};
use ms_log::runtime_log;
use web_time::{Duration, Instant};

use super::coverage::CoverageView;
use crate::custom_fonts::service::service;
use crate::custom_fonts::sources::{ResolvedSource, SourceResolver};
use crate::render_next::types::{
    AntiAliasingMode, HorizontalAlign, KerningMode, LinePlacementReference, RenderExtraInfoRequest, TextDrawnLinesLayoutParams,
    TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams, TextShape, TextVectorLinesLayoutParams, TextWrapMode,
    VerticalLineDirection,
};
use crate::render_next::{CustomKerningTable, FontContent, FontContentSet, render_text_to_image};

/// Quiet time after the last edit before a preview request is sent.
pub(crate) const COALESCE_DELAY: Duration = Duration::from_millis(120);
/// How often the GUI re-checks while waiting (coalescing, or a commit without a wake-up).
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Point size of the preview glyphs.
pub(crate) const PREVIEW_FONT_PT: f32 = 26.0;
/// The wrap width is quantized to this many pixels, so resizing a panel re-renders only
/// when the width really changed.
const WIDTH_QUANTUM_PX: u32 = 64;
/// Most comparison renders the worker keeps memoized.
const SOURCE_MEMO_CAP: usize = 8;

/// A rendered preview ready for upload (unpremultiplied RGBA, white glyphs).
#[derive(Debug)]
pub(crate) struct PreviewImage {
    /// Width in pixels.
    pub(crate) width: u32,
    /// Height in pixels.
    pub(crate) height: u32,
    /// `width * height * 4` bytes.
    pub(crate) rgba: Vec<u8>,
}

/// The outcome of one render.
#[derive(Debug, Clone)]
pub(crate) enum RenderOutcome {
    /// Rendered.
    Image(Arc<PreviewImage>),
    /// Failed (logged on the worker).
    Failed,
}

/// One comparison row's render, keyed so the GUI reuses an uploaded texture.
#[derive(Debug, Clone)]
pub(crate) struct SourceRender {
    /// Hash of (font bytes, text, size): equal key = identical image.
    pub(crate) key: u64,
    /// The render.
    pub(crate) outcome: RenderOutcome,
}

/// One preview job.
pub(crate) struct PreviewRequest {
    /// Generation of this request (strictly increasing per editor).
    generation: u64,
    /// The draft as a recipe, or `None` when the draft is not valid yet (no hybrid render,
    /// no coverage).
    recipe: Option<Arc<CustomFontRecipe>>,
    /// Each row's chosen identity (`None` = empty slot), in row order.
    rows: Vec<Option<String>>,
    /// The sample text.
    text: String,
    /// Resolves the identities (the host's catalog).
    resolver: Arc<dyn SourceResolver>,
    /// Glyph size in pixels.
    font_px: f32,
    /// Wrap width in pixels.
    width_px: u32,
    /// Raised when a newer request supersedes this one (stops the subset compile).
    cancel: Arc<AtomicBool>,
    /// The editor's newest generation (stops renders of a superseded request).
    latest: Arc<AtomicU64>,
    /// Woken when the response is sent.
    ctx: egui::Context,
}

/// The worker's answer to one request.
#[derive(Debug)]
pub(crate) struct PreviewResponse {
    /// Generation of the request answered.
    generation: u64,
    /// The hybrid render, `None` when the draft is not valid or the text is blank.
    hybrid: Option<RenderOutcome>,
    /// One entry per row: `None` for an empty slot.
    sources: Vec<Option<SourceRender>>,
    /// The draft's coverage report (`None` when the draft is not valid; `Err` = failed, logged).
    coverage: Option<Result<CoverageReport, ()>>,
}

/// The generation rule: requests are numbered upwards; a response is shown only when it is
/// newer than the one on screen and not newer than the last request sent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LatestWins {
    /// Generation of the newest request sent.
    sent: u64,
    /// Generation of the response on screen.
    shown: u64,
}

impl LatestWins {
    /// Numbers the next request.
    pub(crate) fn next_generation(&mut self) -> u64 {
        self.sent += 1;
        self.sent
    }

    /// Whether a response of `generation` replaces what is shown (and records it).
    pub(crate) fn accept(&mut self, generation: u64) -> bool {
        if generation > self.shown && generation <= self.sent {
            self.shown = generation;
            true
        } else {
            false
        }
    }

    /// Whether the newest request has not been answered yet.
    pub(crate) fn waiting(self) -> bool {
        self.shown < self.sent
    }
}

/// What one image area shows this frame.
#[derive(Default)]
pub(crate) enum PreviewSlot {
    /// Nothing (empty slot, invalid draft, blank text).
    #[default]
    None,
    /// The texture of the newest accepted render, with the key it was rendered for.
    Ready {
        /// Render key (comparison rows reuse textures by it; 0 for the hybrid).
        key: u64,
        /// The uploaded texture (freed on drop).
        texture: egui::TextureHandle,
    },
    /// The render failed.
    Failed,
}

impl std::fmt::Debug for PreviewSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Ready { key, .. } => f.debug_struct("Ready").field("key", key).finish_non_exhaustive(),
            Self::Failed => f.write_str("Failed"),
        }
    }
}

/// What a request depends on besides its generation; a change schedules a new request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RequestKey {
    /// Draft / preview-text revision.
    revision: u64,
    /// Catalog serial (a reloaded font list).
    catalog: u64,
    /// Glyph size in pixels (bits).
    font_px_bits: u32,
    /// Quantized wrap width.
    width_px: u32,
}

/// The worker handle.
struct Worker {
    /// Requests to the worker; dropping it ends the worker.
    requests: Sender<PreviewRequest>,
    /// Responses from the worker.
    responses: Receiver<PreviewResponse>,
}

/// The per-editor preview scheduler and result slots.
#[derive(Default)]
pub(crate) struct PreviewEngine {
    /// The worker, spawned on the first request.
    worker: Option<Worker>,
    /// Spawning the worker failed (reported once, never retried).
    spawn_failed: bool,
    /// The generation rule.
    order: LatestWins,
    /// Shared newest generation (render cancellation on the worker).
    latest: Arc<AtomicU64>,
    /// Cancel flag of the newest request sent.
    in_flight_cancel: Option<Arc<AtomicBool>>,
    /// What the newest request was sent for.
    sent_key: Option<RequestKey>,
    /// The key wanted now and since when (coalescing).
    wanted: Option<(RequestKey, Instant)>,
    /// The hybrid render.
    pub(crate) hybrid: PreviewSlot,
    /// One comparison slot per row.
    pub(crate) sources: Vec<PreviewSlot>,
    /// The draft's coverage.
    pub(crate) coverage: CoverageView,
}

impl std::fmt::Debug for PreviewEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewEngine")
            .field("worker", &self.worker.is_some())
            .field("order", &self.order)
            .field("hybrid", &self.hybrid)
            .field("coverage", &self.coverage)
            .finish_non_exhaustive()
    }
}

/// Everything `PreviewEngine::tick` needs from the editor this frame.
pub(crate) struct TickInput<'a> {
    /// Draft / text revision.
    pub(crate) revision: u64,
    /// The draft as a recipe when valid.
    pub(crate) recipe: Option<Arc<CustomFontRecipe>>,
    /// Row identities.
    pub(crate) rows: Vec<Option<String>>,
    /// Sample text.
    pub(crate) text: &'a str,
    /// The host's catalog resolver and its serial.
    pub(crate) resolver: &'a Arc<dyn SourceResolver>,
    /// Catalog serial.
    pub(crate) catalog_serial: u64,
    /// Available width in points.
    pub(crate) width_pt: f32,
}

impl Drop for PreviewEngine {
    /// Cancels the in-flight request: its editor is gone, so nobody reads the answer. The
    /// worker itself ends when `requests` drops with the engine.
    fn drop(&mut self) {
        if let Some(cancel) = self.in_flight_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl PreviewEngine {
    /// Whether the newest request is still unanswered (the area shows "rendering…").
    #[must_use]
    pub(crate) fn pending(&self) -> bool {
        self.order.waiting() || self.wanted.is_some()
    }

    /// One GUI frame: drain worker results (uploading textures), then send a request when
    /// the inputs changed and `COALESCE_DELAY` passed. Never blocks.
    pub(crate) fn tick(&mut self, ctx: &egui::Context, input: TickInput<'_>) {
        self.drain(ctx, input.rows.len());
        let pixels_per_point = ctx.pixels_per_point();
        let font_px = PREVIEW_FONT_PT * pixels_per_point;
        let key = RequestKey {
            revision: input.revision,
            catalog: input.catalog_serial,
            font_px_bits: font_px.to_bits(),
            width_px: quantized_width_px(input.width_pt * pixels_per_point),
        };
        if self.sent_key == Some(key) {
            self.wanted = None;
        } else {
            let now = Instant::now();
            let since = match self.wanted {
                Some((wanted, since)) if wanted == key => since,
                // The very first request goes out at once; later edits wait for quiet.
                _ if self.sent_key.is_none() => now.checked_sub(COALESCE_DELAY).unwrap_or(now),
                _ => now,
            };
            self.wanted = Some((key, since));
            if now.duration_since(since) >= COALESCE_DELAY {
                self.send(ctx, key, input, font_px);
            } else {
                ctx.request_repaint_after(COALESCE_DELAY);
            }
        }
        if self.order.waiting() {
            // The worker wakes the GUI when it answers; this only covers a lost wake-up.
            ctx.request_repaint_after(POLL_INTERVAL * 5);
        }
    }

    /// Sends the request for `key` (spawning the worker on first use).
    fn send(&mut self, ctx: &egui::Context, key: RequestKey, input: TickInput<'_>, font_px: f32) {
        self.wanted = None;
        self.sent_key = Some(key);
        if self.ensure_worker().is_none() {
            self.hybrid = PreviewSlot::Failed;
            return;
        }
        if let Some(previous) = self.in_flight_cancel.take() {
            previous.store(true, Ordering::Relaxed);
        }
        let generation = self.order.next_generation();
        self.latest.store(generation, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.in_flight_cancel = Some(Arc::clone(&cancel));
        if input.recipe.is_some() && !matches!(self.coverage, CoverageView::Ready(_)) {
            self.coverage = CoverageView::Pending;
        }
        let request = PreviewRequest {
            generation,
            recipe: input.recipe,
            rows: input.rows,
            text: input.text.to_owned(),
            resolver: Arc::clone(input.resolver),
            font_px,
            width_px: key.width_px,
            cancel,
            latest: Arc::clone(&self.latest),
            ctx: ctx.clone(),
        };
        let sent = self.worker.as_ref().map(|worker| worker.requests.send(request));
        if let Some(Err(err)) = sent {
            runtime_log::log_error(format!("typing: the hybrid font preview worker stopped unexpectedly: {err}"));
            self.worker = None;
            self.spawn_failed = true;
            self.hybrid = PreviewSlot::Failed;
        }
    }

    /// The worker, spawning it on first use; `None` once spawning failed (reported once).
    fn ensure_worker(&mut self) -> Option<&Worker> {
        if self.worker.is_none() && !self.spawn_failed {
            let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
            let (response_tx, response_rx) = mpsc::channel::<PreviewResponse>();
            let spawned = ms_thread::Builder::new()
                .name("typing-hybrid-editor-preview".to_owned())
                .spawn(move || run_worker(&request_rx, &response_tx));
            match spawned {
                Ok(_) => self.worker = Some(Worker { requests: request_tx, responses: response_rx }),
                Err(err) => {
                    runtime_log::log_error(format!(
                        "typing: could not start the hybrid font preview worker: {err}; the preview stays empty"
                    ));
                    self.spawn_failed = true;
                }
            }
        }
        self.worker.as_ref()
    }

    /// Applies every response the worker sent (newest accepted wins).
    fn drain(&mut self, ctx: &egui::Context, row_count: usize) {
        self.sources.resize_with(row_count, PreviewSlot::default);
        loop {
            let received = match self.worker.as_ref().map(|worker| worker.responses.try_recv()) {
                Some(Ok(response)) => response,
                Some(Err(TryRecvError::Empty)) | None => return,
                Some(Err(TryRecvError::Disconnected)) => {
                    runtime_log::log_error("typing: the hybrid font preview worker ended unexpectedly");
                    self.worker = None;
                    self.spawn_failed = true;
                    return;
                }
            };
            if self.order.accept(received.generation) {
                self.apply(ctx, received);
            }
        }
    }

    /// Uploads one accepted response into the slots.
    fn apply(&mut self, ctx: &egui::Context, response: PreviewResponse) {
        self.hybrid = match response.hybrid {
            None => PreviewSlot::None,
            Some(outcome) => slot_for(ctx, "typing_hybrid_editor_preview", 0, &outcome, None),
        };
        for (slot, render) in self.sources.iter_mut().zip(response.sources) {
            let previous = std::mem::take(slot);
            *slot = match render {
                None => PreviewSlot::None,
                Some(render) => slot_for(ctx, "typing_hybrid_editor_source_preview", render.key, &render.outcome, Some(previous)),
            };
        }
        self.coverage = match response.coverage {
            None => CoverageView::Unknown,
            Some(Ok(report)) => CoverageView::Ready(report),
            Some(Err(())) => CoverageView::Failed,
        };
    }
}

/// The slot for one render outcome; a comparison row whose key did not change keeps its
/// uploaded texture.
fn slot_for(ctx: &egui::Context, name: &str, key: u64, outcome: &RenderOutcome, previous: Option<PreviewSlot>) -> PreviewSlot {
    if let Some(PreviewSlot::Ready { key: previous_key, texture }) = previous
        && previous_key == key
        && key != 0
    {
        return PreviewSlot::Ready { key, texture };
    }
    match outcome {
        RenderOutcome::Image(image) => upload(ctx, name, image).map_or(PreviewSlot::Failed, |texture| PreviewSlot::Ready { key, texture }),
        RenderOutcome::Failed => PreviewSlot::Failed,
    }
}

/// Uploads a render as a texture; `None` when its buffer does not match its size (checked
/// here because `ColorImage::from_rgba_unmultiplied` asserts on a mismatch, and an assert on
/// the GUI thread would take the app down for a bad preview).
fn upload(ctx: &egui::Context, name: &str, image: &PreviewImage) -> Option<egui::TextureHandle> {
    let width = usize::try_from(image.width).ok()?;
    let height = usize::try_from(image.height).ok()?;
    let expected = width.checked_mul(height)?.checked_mul(4)?;
    if width == 0 || height == 0 || image.rgba.len() != expected {
        return None;
    }
    let color_image = egui::ColorImage::from_rgba_unmultiplied([width, height], &image.rgba);
    Some(ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR))
}

/// The characters a subset compile needs for `text`: every distinct character except
/// control characters (line breaks are layout, not glyphs). Spaces are kept: their advance
/// is part of the hybrid's spacing.
#[must_use]
pub(crate) fn preview_chars(text: &str) -> BTreeSet<char> {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

/// A wrap width in pixels, rounded down to [`WIDTH_QUANTUM_PX`] (at least one quantum).
#[must_use]
pub(crate) fn quantized_width_px(width_px: f32) -> u32 {
    if !width_px.is_finite() || width_px < 1.0 {
        return WIDTH_QUANTUM_PX;
    }
    let clamped = width_px.min(f32::from(u16::MAX)).floor();
    // Justified numeric cast (`CLAUDE.md` §17): `clamped` is finite, integral and within
    // 1..=65535 by the two lines above, so the conversion is exact.
    let px = clamped as u32;
    (px / WIDTH_QUANTUM_PX).max(1) * WIDTH_QUANTUM_PX
}

/// The worker loop: always processes the NEWEST queued request, skipping older ones.
fn run_worker(requests: &Receiver<PreviewRequest>, responses: &Sender<PreviewResponse>) {
    let mut memo = SourceMemo::default();
    let mut last_error: Option<String> = None;
    while let Ok(mut request) = requests.recv() {
        while let Ok(newer) = requests.try_recv() {
            request = newer;
        }
        let ctx = request.ctx.clone();
        let Some(response) = process(&request, &mut memo, &mut last_error) else {
            continue;
        };
        if responses.send(response).is_err() {
            break;
        }
        ctx.request_repaint();
    }
}

/// Memo of comparison renders, by key, oldest first.
#[derive(Default)]
struct SourceMemo {
    /// `(key, render)`, at most [`SOURCE_MEMO_CAP`].
    entries: VecDeque<(u64, Arc<PreviewImage>)>,
}

impl SourceMemo {
    /// The memoized render of `key`.
    fn get(&self, key: u64) -> Option<Arc<PreviewImage>> {
        self.entries.iter().find(|(entry, _)| *entry == key).map(|(_, image)| Arc::clone(image))
    }

    /// Remembers a render, evicting the oldest beyond the cap.
    fn put(&mut self, key: u64, image: Arc<PreviewImage>) {
        self.entries.push_back((key, image));
        while self.entries.len() > SOURCE_MEMO_CAP {
            self.entries.pop_front();
        }
    }
}

/// Logs `message` unless it repeats the previous worker error (a missing source would
/// otherwise log on every edit).
fn log_once(last_error: &mut Option<String>, message: String) {
    if last_error.as_deref() != Some(message.as_str()) {
        runtime_log::log_warn(&message);
        *last_error = Some(message);
    }
}

/// Answers one request; `None` when it was superseded (nothing is sent).
fn process(request: &PreviewRequest, memo: &mut SourceMemo, last_error: &mut Option<String>) -> Option<PreviewResponse> {
    let superseded = || request.latest.load(Ordering::Relaxed) != request.generation || request.cancel.load(Ordering::Relaxed);
    if superseded() {
        return None;
    }
    let mut resolved: HashMap<&str, Option<ResolvedSource>> = HashMap::new();
    for identity in request.rows.iter().flatten() {
        resolved.entry(identity.as_str()).or_insert_with(|| match request.resolver.resolve_source(identity) {
            Ok(source) => Some(source),
            Err(err) => {
                log_once(last_error, format!("typing: hybrid font preview: {err}"));
                None
            }
        });
    }
    let blank = request.text.trim().is_empty();

    let mut sources = Vec::with_capacity(request.rows.len());
    for row in &request.rows {
        // A blank sample draws nothing anywhere; an empty slot has no comparison row.
        let render = row.as_deref().filter(|_| !blank).map(|identity| match resolved.get(identity).and_then(Option::as_ref) {
            None => SourceRender { key: 0, outcome: RenderOutcome::Failed },
            Some(source) => source_render(request, source, memo, last_error),
        });
        if superseded() {
            return None;
        }
        sources.push(render);
    }

    let (hybrid, coverage) = match &request.recipe {
        None => (None, None),
        Some(recipe) => {
            let recipe_sources: Option<Vec<ResolvedSource>> = recipe
                .source_identities()
                .into_iter()
                .map(|identity| resolved.get(identity).cloned().flatten())
                .collect();
            match recipe_sources {
                None => (Some(RenderOutcome::Failed), Some(Err(()))),
                Some(recipe_sources) => {
                    let coverage = recipe_coverage(recipe, &recipe_sources, last_error);
                    let hybrid = if blank { None } else { Some(hybrid_render(request, recipe, &recipe_sources, last_error)?) };
                    (hybrid, Some(coverage))
                }
            }
        }
    };
    if superseded() {
        return None;
    }
    Some(PreviewResponse { generation: request.generation, hybrid, sources, coverage })
}

/// The coverage report of the whole draft (cmap lookups only).
fn recipe_coverage(recipe: &CustomFontRecipe, sources: &[ResolvedSource], last_error: &mut Option<String>) -> Result<CoverageReport, ()> {
    let views = sources.iter().map(ResolvedSource::as_source_font).collect::<Result<Vec<_>, _>>();
    let result = views.map_err(|err| err.to_string()).and_then(|views| coverage_report(recipe, &views).map_err(|err| err.to_string()));
    result.map_err(|err| log_once(last_error, format!("typing: hybrid font coverage failed: {err}. Recipe: {}", recipe.to_json())))
}

/// Compiles the subset hybrid of the sample's characters and renders the sample with it.
/// `None` when the request was superseded mid-compile.
fn hybrid_render(
    request: &PreviewRequest,
    recipe: &CustomFontRecipe,
    sources: &[ResolvedSource],
    last_error: &mut Option<String>,
) -> Option<RenderOutcome> {
    let chars = preview_chars(&request.text);
    let compiled = match service().subset_preview_blocking(recipe, sources, &chars, &request.cancel) {
        Ok(compiled) => compiled,
        Err(crate::custom_fonts::service::CustomFontError::Cancelled) => return None,
        Err(err) => {
            log_once(last_error, format!("typing: hybrid font preview could not be compiled: {err}. Recipe: {}", recipe.to_json()));
            return Some(RenderOutcome::Failed);
        }
    };
    let content = FontContent {
        name: compiled.postscript_name.clone(),
        original_name: compiled.family_name.clone(),
        content_id: crate::render_next::font_content_id(&compiled.bytes),
        data: Arc::new(compiled.bytes),
        face_index: 0,
        custom_kerning: None,
    };
    Some(render(request, content, last_error).map_or(RenderOutcome::Failed, |image| RenderOutcome::Image(Arc::new(image))))
}

/// Renders the sample in one source font (memoized by bytes, text and size).
fn source_render(request: &PreviewRequest, source: &ResolvedSource, memo: &mut SourceMemo, last_error: &mut Option<String>) -> SourceRender {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.content_id.hash(&mut hasher);
    source.face_index.hash(&mut hasher);
    request.text.hash(&mut hasher);
    request.font_px.to_bits().hash(&mut hasher);
    request.width_px.hash(&mut hasher);
    for pair in source.user_kerning.iter() {
        (pair.left, pair.right, pair.em.to_bits()).hash(&mut hasher);
    }
    // 0 is reserved for "no texture reuse".
    let key = hasher.finish().max(1);
    if let Some(image) = memo.get(key) {
        return SourceRender { key, outcome: RenderOutcome::Image(image) };
    }
    let kerning = (!source.user_kerning.is_empty())
        .then(|| Arc::new(CustomKerningTable::from_pairs(source.user_kerning.iter().map(|pair| (pair.left, pair.right, pair.em)))));
    let content = FontContent {
        name: source.identity.clone(),
        original_name: source.identity.clone(),
        content_id: source.content_id,
        data: Arc::clone(&source.data),
        face_index: source.face_index,
        custom_kerning: kerning,
    };
    match render(request, content, last_error) {
        Some(image) => {
            let image = Arc::new(image);
            memo.put(key, Arc::clone(&image));
            SourceRender { key, outcome: RenderOutcome::Image(image) }
        }
        None => SourceRender { key, outcome: RenderOutcome::Failed },
    }
}

/// Renders the sample text with the one font in `content`; `None` on failure (logged).
fn render(request: &PreviewRequest, content: FontContent, last_error: &mut Option<String>) -> Option<PreviewImage> {
    let params = preview_params(&request.text, &content.name, request.font_px, request.width_px);
    let provider = FontContentSet::new(vec![content]);
    match render_text_to_image(&params, &provider, Some((&request.latest, request.generation))) {
        Ok(rendered) if rendered.width > 0 && rendered.height > 0 => {
            Some(PreviewImage { width: rendered.width, height: rendered.height, rgba: rendered.rgba })
        }
        Ok(_) => None,
        Err(err) => {
            if request.latest.load(Ordering::Relaxed) == request.generation {
                log_once(last_error, format!("typing: hybrid font preview render failed: {err}"));
            }
            None
        }
    }
}

/// Plain horizontal render parameters: white glyphs at `font_px`, wrapped by words at
/// `width_px`, no effects, no inline tags.
#[must_use]
fn preview_params(text: &str, font: &str, font_px: f32, width_px: u32) -> TextRenderParams {
    TextRenderParams {
        text: text.to_owned(),
        text_color: [255, 255, 255, 255],
        font_name: font.to_owned(),
        font_size_px: font_px,
        line_spacing_px: 0.0,
        line_spacing_percent: 20.0,
        kerning_mode: KerningMode::Auto,
        kerning_px: 0.0,
        kerning_percent: 0.0,
        glyph_height_percent: 100.0,
        glyph_width_percent: 100.0,
        width_px,
        align: HorizontalAlign::LEFT,
        selected_face_index: 0,
        force_bold: false,
        force_italic: false,
        faux_bold: None,
        faux_italic_slant_deg: None,
        uppercase_text: false,
        trim_extra_spaces: false,
        replace_ellipsis_with_dots: false,
        force_remove_ellipsis_glyph: false,
        hanging_punctuation: 0.0,
        new_line_after_sentence: false,
        enable_inline_style_tags: false,
        text_wrap_mode: TextWrapMode::WholeWords,
        text_shape: TextShape::Free,
        shape_min_width_percent: 55.0,
        shape_variant: 5,
        compare_shape_with: None,
        allow_moderate_trees: false,
        text_line_mode: TextLineMode::Horizontal,
        vertical_line_direction: VerticalLineDirection::RightToLeft,
        text_layout_mode: TextLayoutMode::Normal,
        formula_layout: TextFormulaLayoutParams::default(),
        drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
        vector_lines_layout: TextVectorLinesLayoutParams::default(),
        effects_json: String::new(),
        anti_aliasing: AntiAliasingMode::Strong,
        global_rotation_deg: 0.0,
        line_placement_percent: 0.0,
        line_placement_reference: LinePlacementReference::GlyphHeight,
        raster_transform: None,
        extra_info: RenderExtraInfoRequest::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_newer_response_supersedes_and_an_older_one_is_dropped() {
        let mut order = LatestWins::default();
        assert!(!order.waiting());
        let first = order.next_generation();
        let second = order.next_generation();
        assert!(order.waiting());
        assert!(order.accept(second));
        assert!(!order.accept(first), "an older answer never replaces a newer image");
        assert!(!order.accept(second), "the same answer is applied once");
        assert!(!order.accept(second + 1), "an answer to an unsent request is ignored");
        assert!(!order.waiting());
        let third = order.next_generation();
        assert!(order.waiting());
        assert!(order.accept(third));
    }

    #[test]
    fn dropping_the_engine_cancels_the_in_flight_request() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut engine = PreviewEngine::default();
        engine.in_flight_cancel = Some(Arc::clone(&cancel));
        drop(engine);
        assert!(cancel.load(Ordering::Relaxed), "an abandoned subset compile must stop");
    }

    #[test]
    fn preview_chars_dedup_and_keep_spaces_but_not_line_breaks() {
        let chars = preview_chars("Аа а\nб\tб");
        assert_eq!(chars, ['А', 'а', ' ', 'б'].into_iter().collect());
        assert!(preview_chars("").is_empty());
    }

    #[test]
    fn the_wrap_width_is_quantized() {
        assert_eq!(quantized_width_px(0.0), WIDTH_QUANTUM_PX);
        assert_eq!(quantized_width_px(f32::NAN), WIDTH_QUANTUM_PX);
        assert_eq!(quantized_width_px(63.9), WIDTH_QUANTUM_PX);
        assert_eq!(quantized_width_px(130.0), 2 * WIDTH_QUANTUM_PX);
        assert_eq!(quantized_width_px(1e12), (u32::from(u16::MAX) / WIDTH_QUANTUM_PX) * WIDTH_QUANTUM_PX);
    }

    #[test]
    fn the_source_memo_is_bounded_and_keeps_the_newest() {
        let mut memo = SourceMemo::default();
        let image = || Arc::new(PreviewImage { width: 1, height: 1, rgba: vec![0; 4] });
        for key in 1..=u64::try_from(SOURCE_MEMO_CAP).unwrap_or(8) + 2 {
            memo.put(key, image());
        }
        assert_eq!(memo.entries.len(), SOURCE_MEMO_CAP);
        assert!(memo.get(1).is_none());
        assert!(memo.get(u64::try_from(SOURCE_MEMO_CAP).unwrap_or(8) + 2).is_some());
    }
}
