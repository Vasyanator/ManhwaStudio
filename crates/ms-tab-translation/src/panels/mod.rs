/*
File: crates/ms-tab-translation/src/panels/mod.rs

Purpose:
Module declarations of the Translation tab's side panels (including `ocr_model_download`,
the OCR panel's external-model download block), plus the one helper shared by more than one
panel.

Key functions:
- section_header_button(): the "▶ / ▼ title" header button of the panels' hand-rolled
  collapsible sections (MT AI API accordion, OCR AI API connection section).
*/

pub mod bubbles;
pub mod composition;
pub mod machine_translation;
pub mod ocr;
pub mod ocr_langs;
pub mod ocr_model_download;
pub mod text_detector;

/// Draws a collapsible-section header as a button labelled `"▼ {title}"` when `expanded`, else
/// `"▶ {title}"`, and returns `true` when it was clicked. It owns only the look; the caller
/// decides what a click does (the MT accordion opens that section, the OCR connection section
/// toggles). A plain `ui.button`, so it creates no stored widget state and needs no `id_salt`.
pub(crate) fn section_header_button(ui: &mut egui::Ui, expanded: bool, title: &str) -> bool {
    let prefix = if expanded { "▼" } else { "▶" };
    ui.button(format!("{prefix} {title}")).clicked()
}
