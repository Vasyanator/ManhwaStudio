/*
File: crates/ms-ai-api/src/generation_view.rs

Purpose:
The shared "generation" status widget of an LLM request in flight: line 1 a spinner and the
phase ("Thinking…" / "Answering…"), for a second attempt a warning line saying why the request
is sent again (`GenerationSnapshot::retry`), then the output characters received so far and a
"Stop" button. Used by the translation tab's AI API OCR and machine-translation panels, drawn
outside their collapsible sections.

Key functions:
- draw_generation_status() : draws the widget for a `GenerationSnapshot`, returns the Stop click.
- output_chars_text()      : private, the localized char-count line.
- retry_text()             : private, the localized retry line of a `RetryReason`.

Notes:
Draws nothing for an inactive snapshot. While it is drawn, the spinner (`egui::Spinner`) requests
a repaint every frame, which also keeps the counters live. It never spawns work: the consumer
turns the Stop click into `GenerationTracker::cancel_run(snapshot.run_id)` (or its own run
cancel). No ids are created (only auto-id buttons and labels), so it needs no salt.
*/

use crate::generation::{GenerationPhase, GenerationSnapshot, RetryReason};

/// Draws the status of the generation in `snapshot` into `ui` and returns `true` when the user
/// clicked "Stop" (the click is about `snapshot.run_id`). Draws nothing and returns `false` when
/// `snapshot.active` is `false`.
#[must_use]
pub fn draw_generation_status(ui: &mut egui::Ui, snapshot: &GenerationSnapshot) -> bool {
    if !snapshot.active {
        return false;
    }
    let (phase_text, phase_color) = match snapshot.phase {
        GenerationPhase::Thinking => (t!("ai_api.generation.thinking_status"), ms_theme::status::INFO),
        GenerationPhase::Answering => (t!("ai_api.generation.answering_status"), ms_theme::status::SUCCESS),
    };
    ui.horizontal(|ui| {
        // The spinner also requests the per-frame repaint that keeps the counters below live.
        ui.spinner();
        ui.colored_label(phase_color, phase_text);
    });
    if let Some(reason) = snapshot.retry {
        ui.colored_label(ms_theme::status::WARNING, retry_text(reason));
    }
    let mut stop_clicked = false;
    ui.horizontal_wrapped(|ui| {
        ui.label(output_chars_text(snapshot));
        stop_clicked = ui.button(t!("ai_api.generation.stop_button")).on_hover_text(t!("ai_api.generation.stop_tooltip")).clicked();
    });
    stop_clicked
}

/// "Output: N chars", with the reasoning / answer split when any reasoning text was received.
fn output_chars_text(snapshot: &GenerationSnapshot) -> String {
    if snapshot.reasoning_chars > 0 {
        tf!("ai_api.generation.output_chars_split_label", total = snapshot.total_chars(), reasoning = snapshot.reasoning_chars, answer = snapshot.answer_chars)
    } else {
        tf!("ai_api.generation.output_chars_label", total = snapshot.answer_chars)
    }
}

/// The warning line of a second attempt, e.g. "Retry: the answer was not in the required format
/// (2 of 10 missing)".
fn retry_text(reason: RetryReason) -> String {
    match reason {
        RetryReason::Malformed => t!("ai_api.generation.retry_malformed_status").to_string(),
        RetryReason::Incomplete { missing, expected } => tf!("ai_api.generation.retry_incomplete_status", missing = missing, expected = expected),
        RetryReason::Truncated => t!("ai_api.generation.retry_truncated_status").to_string(),
        RetryReason::Empty => t!("ai_api.generation.retry_empty_status").to_string(),
        RetryReason::StructuredOutputUnsupported => t!("ai_api.generation.retry_structured_unsupported_status").to_string(),
    }
}
