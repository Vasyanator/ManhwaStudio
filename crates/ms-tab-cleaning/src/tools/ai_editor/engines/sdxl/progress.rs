/*
File: cleaning/tools/ai_editor/engines/sdxl/progress.rs

Purpose:
The progress state shared between the run worker and the parameter panel, and the progress
bar plus live latent preview drawn from it.

Main responsibilities:
- own `SdxlSharedProgress` and the poison-tolerant lock around it;
- claim and retire a run's GENERATION so a detached worker cannot drive a live bar;
- publish one streamed `progress` frame (`publish_progress_frame`);
- draw the bar and the latest latent preview (`draw_sdxl_progress_ui`).

Key structures:
- `SdxlSharedProgress`

Key functions:
- `begin_progress_generation()`, `retire_progress_generation()`, `publish_progress_frame()`,
  `draw_sdxl_progress_ui()`

Notes:
`inpaint.sdxl` streams one `progress` frame per diffusion step, carrying `step` / `total` and
— when the backend produced one — a latent preview PNG in the frame's blob. The worker owns
the writing side, the panel owns the reading side, and the `Mutex` is held for the length of
a field copy and nothing more.

The GENERATION stamp is what makes cancellation honest. A cancelled run is DETACHED — the
backend finishes the pass, because the plain streaming call exposes no request id to cancel
by — so its worker keeps producing frames. Every write is therefore gated on the generation
that claimed the bar: writes from a retired run are dropped, and the bar disappears the
moment the user cancels instead of animating for a result nobody will receive.

The preview travels as a TEXTURE only once per frame the worker produced: `preview_seq` is
what the panel compares against, so an unchanged preview is never re-uploaded to the GPU.
*/

use super::*;

/// Live progress of one run, shared between the worker thread and the panel.
///
/// The worker writes `step` / `total` and the latest latent `preview` (bumping `preview_seq`
/// with each new one); the panel reads them every frame while a run is in flight.
#[derive(Default)]
pub(super) struct SdxlSharedProgress {
    /// Identity of the run that currently owns this state. A write stamped with any other
    /// value is dropped — see the file header.
    pub(super) generation: u64,
    /// Whether a run owns the bar right now.
    pub(super) active: bool,
    /// Diffusion steps completed, as last reported by the backend.
    pub(super) step: u32,
    /// Total diffusion steps of the pass. `0` while the backend has not said yet, which is
    /// what the panel shows as «подготовка модели» rather than as 0 %.
    pub(super) total: u32,
    /// The most recent decoded latent preview, `None` until the first one arrives.
    pub(super) preview: Option<egui::ColorImage>,
    /// Bumped on every change of `preview`, including its clearing. The panel uploads a new
    /// texture only when this differs from what it last uploaded.
    pub(super) preview_seq: u64,
}

/// Locks the shared progress, recovering the inner value if a worker panicked while holding
/// it.
///
/// The data is plain progress metadata — a step counter and a preview image — so a partially
/// written value is not unsound and not worth propagating a panic for; the alternative is a
/// GUI thread that panics because a worker did.
pub(super) fn lock_progress(
    progress: &Mutex<SdxlSharedProgress>,
) -> MutexGuard<'_, SdxlSharedProgress> {
    match progress.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// Claims the bar for a new run and returns its generation stamp.
///
/// Called on the GUI thread when the run starts, not on the worker: the claim then happens in
/// the order the user pressed the button, whatever order the threads start in. The counter
/// wraps rather than saturating — a saturated counter would stop being unique and let a
/// stale worker write into a live run again, and 2^64 runs is not a reachable session.
pub(super) fn begin_progress_generation(
    progress: &Mutex<SdxlSharedProgress>,
    total_steps: u32,
) -> u64 {
    let mut guard = lock_progress(progress);
    guard.generation = guard.generation.wrapping_add(1);
    guard.active = true;
    guard.step = 0;
    // The configured step count, so the bar has a scale before the first streamed frame
    // refines it with what the backend actually scheduled.
    guard.total = total_steps;
    guard.preview = None;
    guard.preview_seq = guard.preview_seq.wrapping_add(1);
    guard.generation
}

/// Retires the current generation: the bar disappears at once and every later write from the
/// abandoned worker is ignored.
pub(super) fn retire_progress_generation(progress: &Mutex<SdxlSharedProgress>) {
    let mut guard = lock_progress(progress);
    guard.generation = guard.generation.wrapping_add(1);
    guard.active = false;
    guard.step = 0;
    guard.total = 0;
    guard.preview = None;
    guard.preview_seq = guard.preview_seq.wrapping_add(1);
}

/// Writes one streamed `progress` frame into the shared state under `generation`.
///
/// A frame from a retired or superseded run is dropped. A frame carrying no preview leaves
/// the previous one on screen — the backend sends previews at its own cadence, and blanking
/// the image between them would make it flicker.
pub(super) fn publish_progress_frame(
    progress: &Mutex<SdxlSharedProgress>,
    generation: u64,
    step: u32,
    total: u32,
    preview: Option<egui::ColorImage>,
) {
    let mut guard = lock_progress(progress);
    if guard.generation != generation {
        return;
    }
    guard.step = step;
    guard.total = total;
    if let Some(preview) = preview {
        guard.preview = Some(preview);
        guard.preview_seq = guard.preview_seq.wrapping_add(1);
    }
}

/// Marks the run of `generation` finished, leaving its last preview on screen.
///
/// The preview survives on purpose: it is the closest thing to the result the panel can show
/// while the host is still placing the real image into the frame.
pub(super) fn finish_progress_generation(progress: &Mutex<SdxlSharedProgress>, generation: u64) {
    let mut guard = lock_progress(progress);
    if guard.generation != generation {
        return;
    }
    guard.active = false;
}

/// Fraction of the step counter for the progress bar: clamped into `0.0..=1.0`, and `0.0`
/// when the total is unknown (`0`) rather than a division by zero.
#[must_use]
pub(super) fn sdxl_progress_fraction(step: u32, total: u32) -> f32 {
    if total == 0 {
        return 0.0;
    }
    // Cast justification: both counters are diffusion step numbers, at most the low
    // hundreds, so `f32` represents them exactly; the result is a bar fraction either way.
    (step as f32 / total as f32).clamp(0.0, 1.0)
}

/// Draws the step progress bar and the latest live latent preview.
///
/// Draws nothing at all while no run owns the bar and no preview has ever been uploaded, so
/// an idle panel spends no vertical space on it. A new preview is uploaded as a texture only
/// when the worker produced a frame newer than `preview_uploaded_seq`; a generation that was
/// retired clears the texture instead, so a cancelled run leaves no stale image behind.
pub(super) fn draw_sdxl_progress_ui(
    ui: &mut egui::Ui,
    progress: &Mutex<SdxlSharedProgress>,
    preview_texture: &mut Option<egui::TextureHandle>,
    preview_uploaded_seq: &mut u64,
) {
    let (active, step, total, new_preview, seq) = {
        let guard = lock_progress(progress);
        let fresh = guard.preview_seq != *preview_uploaded_seq;
        let new_preview = if fresh { Some(guard.preview.clone()) } else { None };
        (guard.active, guard.step, guard.total, new_preview, guard.preview_seq)
    };

    // `Some(None)` is a real answer and not "nothing to do": the shared preview was cleared
    // (a new run claimed the bar, or a cancelled one retired it), so the texture from the
    // previous run must go rather than linger under the next run's progress bar.
    if let Some(preview) = new_preview {
        *preview_texture = preview.map(|image| {
            ui.ctx()
                .load_texture("sdxl_latent_preview", image, egui::TextureOptions::LINEAR)
        });
        *preview_uploaded_seq = seq;
    }

    if !active && preview_texture.is_none() {
        return;
    }

    if total > 0 {
        ui.add(
            egui::ProgressBar::new(sdxl_progress_fraction(step, total))
                .text(tf!("cleaning.common.step_progress_status", step = step, total = total)),
        );
    } else if active {
        ui.add(
            egui::ProgressBar::new(0.0).text(t!("cleaning.tools.sdxl.preparing_model_status")),
        );
    }

    if let Some(handle) = preview_texture.as_ref() {
        ui.label(t!("cleaning.tools.sdxl.latent_preview_label"));
        let size = handle.size_vec2();
        // Never blown up: a 64x64 latent preview scaled to the panel width would be a blur
        // that says less than the small image does.
        let scale = if size.x > 0.0 { (SDXL_PREVIEW_MAX_WIDTH_PT / size.x).min(1.0) } else { 1.0 };
        ui.add(
            egui::Image::new(egui::load::SizedTexture::from_handle(handle))
                .fit_to_exact_size(size * scale),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write stamped with a retired generation is dropped, which is the whole point of the
    /// stamp: a cancelled run's worker keeps streaming frames, and none of them may move the
    /// bar of whatever runs next.
    #[test]
    fn a_retired_generation_can_no_longer_write() {
        let progress = Mutex::new(SdxlSharedProgress::default());
        let first = begin_progress_generation(&progress, 30);
        publish_progress_frame(&progress, first, 7, 30, None);
        {
            let guard = lock_progress(&progress);
            assert!(guard.active);
            assert_eq!((guard.step, guard.total), (7, 30));
        }

        retire_progress_generation(&progress);
        publish_progress_frame(&progress, first, 8, 30, None);
        {
            let guard = lock_progress(&progress);
            assert!(!guard.active, "the bar disappears the moment the run is retired");
            assert_eq!(guard.step, 0, "the detached worker's frame was dropped");
        }

        // And the next run owns the bar alone.
        let second = begin_progress_generation(&progress, 12);
        assert_ne!(first, second);
        publish_progress_frame(&progress, first, 99, 30, None);
        publish_progress_frame(&progress, second, 3, 12, None);
        let guard = lock_progress(&progress);
        assert_eq!((guard.step, guard.total), (3, 12));
    }

    /// An unknown total is `0.0`, not a division by zero, and a step past the total cannot
    /// overfill the bar.
    #[test]
    fn the_bar_fraction_is_clamped_and_survives_an_unknown_total() {
        assert!((sdxl_progress_fraction(0, 0) - 0.0).abs() < f32::EPSILON);
        assert!((sdxl_progress_fraction(7, 0) - 0.0).abs() < f32::EPSILON);
        assert!((sdxl_progress_fraction(15, 30) - 0.5).abs() < f32::EPSILON);
        assert!((sdxl_progress_fraction(30, 30) - 1.0).abs() < f32::EPSILON);
        assert!((sdxl_progress_fraction(99, 30) - 1.0).abs() < f32::EPSILON);
    }

    /// A new run clears the previous run's preview and bumps the sequence, so the panel drops
    /// the stale texture instead of showing it under the new bar.
    #[test]
    fn claiming_the_bar_clears_the_previous_preview() {
        let progress = Mutex::new(SdxlSharedProgress::default());
        let first = begin_progress_generation(&progress, 4);
        publish_progress_frame(
            &progress,
            first,
            1,
            4,
            Some(egui::ColorImage::filled([2, 2], egui::Color32::WHITE)),
        );
        let seq_with_preview = {
            let guard = lock_progress(&progress);
            assert!(guard.preview.is_some());
            guard.preview_seq
        };

        begin_progress_generation(&progress, 4);
        let guard = lock_progress(&progress);
        assert!(guard.preview.is_none());
        assert_ne!(guard.preview_seq, seq_with_preview, "the panel must notice the clearing");
    }

    /// A finished run keeps its last preview but releases the bar; a finish stamped with an
    /// old generation cannot stop a live run's bar.
    #[test]
    fn finishing_releases_the_bar_and_keeps_the_last_preview() {
        let progress = Mutex::new(SdxlSharedProgress::default());
        let generation = begin_progress_generation(&progress, 2);
        publish_progress_frame(
            &progress,
            generation,
            2,
            2,
            Some(egui::ColorImage::filled([1, 1], egui::Color32::WHITE)),
        );
        finish_progress_generation(&progress, generation);
        {
            let guard = lock_progress(&progress);
            assert!(!guard.active);
            assert!(guard.preview.is_some(), "the last preview stays on screen");
        }

        let next = begin_progress_generation(&progress, 2);
        finish_progress_generation(&progress, generation);
        let guard = lock_progress(&progress);
        assert!(guard.active, "a stale finish must not stop the live run");
        assert_eq!(guard.generation, next);
    }
}
