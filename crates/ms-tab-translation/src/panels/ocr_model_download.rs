/*
File: crates/ms-tab-translation/src/panels/ocr_model_download.rs

Purpose:
The download / verify block of an app-managed external model bundle (`ocr_model_download` bundles:
Baberu OCR, the selected PaddleOCR-VL variant, the EasyOCR model of the selected languages,
the two Surya OCR checkpoints, the Surya text-detection checkpoint; the Cleaning tab draws it
for its SDXL configs): install status, the outcome of the last unsuccessful download, the
"Download (size)" / "Resume download" button, a "Verify files" button once the bundle is
installed (hashes every file against its pin: the repair path for a damaged file the stat-only
probe cannot see; it ends with an intact / N-files-removed notice), a progress bar with Cancel
while a download, verify or automatic adoption (files found on disk without markers) runs
(its text also shows the automatic retry countdown after a network failure, or that found
files are being checked), the outcome of a refused or cancelled adoption, and — while ANOTHER
bundle's job runs — a line naming it with its progress over the disabled button. For every
gated Hugging Face repository the bundle still needs and may not download from (no token,
token rejected, terms not accepted, unknown repository, Hub unreachable, still checking) it
draws a line with the reason and a link to the repository page; the Download button stays
enabled (the download re-checks access itself and ends with that line's reason).
Apart from that link, it only draws and returns a `ModelDownloadAction`; the caller hands the
action to `OcrModelDownloadController`.

Key items:
- `ModelDownloadAction`: what the user asked for this frame.
- `download_block_view()`: pure snapshot -> (lines, progress, button) mapping, unit-tested.
- `draw_ocr_model_download()`: renders that view (`pub`: the Cleaning tab draws it too).
- `load_blocked_reason()`: why a Load / Run button is disabled for the bundle's state.
- `external_model_display_name()`: the user-facing name of every catalog spec (FLUX.2 klein
  parts through the catalog's reverse lookup `flux2_klein_spec_role`).

Notes:
- Download / resume captions come from `ocr_model_download::offered_download_label`, the
  same owner the "model is not downloaded" errors use, so an error names exactly the
  button drawn here.
- The repository link opens through `ms_platform::shell_open::open_url` (the project's one
  owner of opening URLs; a spawn, safe on the GUI thread); a failure is logged and shown under
  the link, kept in egui temp data so the signature stays a pure snapshot -> action call.
*/

use crate::ocr_model_download::{
    DownloadNotice, JobKind, OcrModelDownloadState, OcrModelPanelSnapshot, OtherDownload, access_status_key, download_button_label,
    format_bytes, offered_download_label,
};
use ms_config::{Flux2EncoderSource, Flux2WeightAxis};
use ms_sysprobe::ai_models::easyocr::{EasyOcrScript, easyocr_spec};
use ms_sysprobe::ai_models::external::{ExternalDownloadPhase, ExternalDownloadProgress, ExternalModelSpec};
use ms_sysprobe::ai_models::external_catalog::{
    BABERU_OCR, Flux2SpecRole, PaddleVlVariant, SDXL_CONFIG_BASE, SDXL_CONFIG_INPAINT, SDXL_INPAINT_FP16, SURYA_DETECTION_2025_05_07,
    SURYA_RECOGNITION_2025_09_23, flux2_klein_spec_role,
};
use ms_sysprobe::ai_models::hf_access::HfAccess;

/// The user's request from the download block in this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelDownloadAction {
    /// Nothing clicked.
    #[default]
    None,
    /// Start (or resume) downloading the shown model.
    Download,
    /// Verify the installed files of the shown model against their pins.
    Verify,
    /// Cancel the running download.
    Cancel,
}

/// Colour role of one text line of the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTone {
    Weak,
    Success,
    Warning,
    Error,
}

/// The block's button: caption, the action its click returns, and whether it is enabled.
#[derive(Debug, Clone, PartialEq)]
struct BlockButton {
    caption: String,
    action: ModelDownloadAction,
    enabled: bool,
}

/// One access line: the reason a gated repository refuses (or is being checked), and the
/// repository whose page the link under it opens (`None`: no link while checking).
#[derive(Debug, Clone, PartialEq)]
struct AccessLineView {
    tone: StatusTone,
    text: String,
    link: Option<&'static str>,
}

/// Everything the block shows for one snapshot; built by the pure [`download_block_view`].
#[derive(Debug, Clone, PartialEq)]
struct DownloadBlockView {
    /// Text lines (status, last outcome, other running download) and their colour roles.
    lines: Vec<(StatusTone, String)>,
    /// One line per gated repository that is not granted (a refused download's repository
    /// first), each with its repository link.
    access: Vec<AccessLineView>,
    /// Progress bar fraction (0..=1) and its text.
    progress: Option<(f32, String)>,
    /// The button, if the state offers one.
    button: Option<BlockButton>,
}

/// Display name of an external model spec for user-facing text: the engine's product name
/// for Baberu OCR, the localized variant label for a PaddleOCR-VL variant, "EasyOCR" with the
/// recognizer file of its script, localized names for the Surya checkpoints, the SDXL
/// downloads and the FLUX.2 klein package parts (each names its variant; a GGUF part its quant
/// token, an unquantized encoder its dtype, the uncensored encoder says so); the
/// spec id for a spec this table does not know (logged once per call site would be noise: the
/// id is the diagnostic itself).
#[must_use]
pub fn external_model_display_name(spec: &ExternalModelSpec) -> String {
    if let Some(name) = flux2_klein_display_name(spec) {
        return name;
    }
    if spec.id == BABERU_OCR.id {
        // Product name, not translated (the engine selector shows the same literal).
        return "Baberu OCR".to_owned();
    }
    if let Some(variant) = PaddleVlVariant::ALL.into_iter().find(|variant| variant.spec().id == spec.id) {
        return crate::panels::ocr::paddle_vl_variant_label(variant).to_owned();
    }
    if let Some(script) = EasyOcrScript::ALL.into_iter().find(|script| easyocr_spec(*script).id == spec.id) {
        return tf!("translation.ocr_model.easyocr_model_name", file = script.recognizer_file());
    }
    let key = match spec.id {
        id if id == SURYA_DETECTION_2025_05_07.id => "translation.ocr_model.surya_detection_name",
        id if id == SURYA_RECOGNITION_2025_09_23.id => "translation.ocr_model.surya_recognition_name",
        id if id == SDXL_CONFIG_BASE.id => "translation.ocr_model.sdxl_config_base_name",
        id if id == SDXL_CONFIG_INPAINT.id => "translation.ocr_model.sdxl_config_inpaint_name",
        id if id == SDXL_INPAINT_FP16.id => "translation.ocr_model.sdxl_inpaint_fp16_name",
        other => return other.to_owned(),
    };
    // The key is chosen at runtime from the table above, so `resolve_key` replaces `t!`.
    ms_i18n::resolve_key(key).to_string()
}

/// The display name of a FLUX.2 klein package spec (any variant), `None` for any other spec.
/// The role comes from the catalog's own reverse lookup (`flux2_klein_spec_role`), so a new
/// variant, source or format is named without touching this function. The variant size
/// (`4B` / `9B`), the quant token (`Q8_0`, …) and the dtype (`bf16` / `f16`) are technical
/// literals substituted as is (i18n exclusions).
fn flux2_klein_display_name(spec: &ExternalModelSpec) -> Option<String> {
    let name = match flux2_klein_spec_role(spec.id)? {
        Flux2SpecRole::Base { variant } => tf!("translation.ocr_model.flux2_klein_base_name", variant = variant.size_token()),
        Flux2SpecRole::TextEncoderConfig { variant } => {
            tf!("translation.ocr_model.flux2_klein_text_encoder_config_name", variant = variant.size_token())
        }
        Flux2SpecRole::LegacyTransformer { variant } => {
            tf!("translation.ocr_model.flux2_klein_transformer_legacy_name", variant = variant.size_token())
        }
        Flux2SpecRole::Weights { variant, axis, source, format } => {
            let variant = variant.size_token();
            let dtype = source.unquantized_token();
            match (axis, source, format.quant_token()) {
                // The transformer does not depend on the encoder source.
                (Flux2WeightAxis::Transformer, _, None) => tf!("translation.ocr_model.flux2_klein_transformer_bf16_name", variant = variant),
                (Flux2WeightAxis::Transformer, _, Some(quant)) => {
                    tf!("translation.ocr_model.flux2_klein_transformer_gguf_name", variant = variant, quant = quant)
                }
                (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, None) => {
                    tf!("translation.ocr_model.flux2_klein_text_encoder_bf16_name", variant = variant, dtype = dtype)
                }
                (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Stock, Some(quant)) => {
                    tf!("translation.ocr_model.flux2_klein_text_encoder_gguf_name", variant = variant, quant = quant)
                }
                (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, None) => {
                    tf!("translation.ocr_model.flux2_klein_text_encoder_uncensored_bf16_name", variant = variant, dtype = dtype)
                }
                (Flux2WeightAxis::TextEncoder, Flux2EncoderSource::Uncensored, Some(quant)) => {
                    tf!("translation.ocr_model.flux2_klein_text_encoder_uncensored_gguf_name", variant = variant, quant = quant)
                }
            }
        }
    };
    Some(name)
}

/// The access line of `repo_id`: the localized reason (`None` = still checking) with the
/// repository link, or `None` for a granted repository (nothing to say).
fn access_line_view(repo_id: &'static str, access: Option<&HfAccess>) -> Option<AccessLineView> {
    let Some(access) = access else {
        return Some(AccessLineView { tone: StatusTone::Weak, text: tf!("translation.ocr_model.access_checking_status", repo = repo_id), link: None });
    };
    let key = access_status_key(access)?;
    let tone = match access {
        HfAccess::Granted => return None,
        // The user can fix these: a token, a new token, the terms on the repository page.
        HfAccess::NoToken | HfAccess::TokenRejected | HfAccess::TermsNotAccepted => StatusTone::Warning,
        HfAccess::RepoNotFound | HfAccess::Unreachable { .. } => StatusTone::Error,
    };
    // The key is chosen at runtime (`access_status_key`, one owner of the mapping), so the
    // runtime-key pair replaces `tf!`; the repository id is a language-neutral identifier.
    let text = ms_i18n::interpolate(ms_i18n::resolve_key(key), &[("repo", &repo_id)]);
    Some(AccessLineView { tone, text, link: Some(repo_id) })
}

/// Display name of a bundle: its specs' names joined with " + ".
fn bundle_display_name(specs: &[&'static ExternalModelSpec]) -> String {
    specs.iter().map(|spec| external_model_display_name(spec)).collect::<Vec<_>>().join(" + ")
}

/// The line naming the other bundle's running download / verify with its byte progress.
fn other_download_line(other: &OtherDownload) -> String {
    let name = bundle_display_name(&other.specs);
    let done = format_bytes(other.progress.total_done);
    let total = format_bytes(other.progress.total_bytes);
    match other.kind {
        JobKind::Download => tf!("translation.ocr_model.other_download_status", name = name, done = done, total = total),
        JobKind::Verify => tf!("translation.ocr_model.other_verify_status", name = name, done = done, total = total),
        JobKind::Adopt => tf!("translation.ocr_model.other_adopt_status", name = name, done = done, total = total),
    }
}

/// Maps a bundle's download snapshot to what the block shows. Pure (reads only the active
/// i18n catalog), so the mapping is unit-tested without a `Ui`.
fn download_block_view(snapshot: &OcrModelPanelSnapshot) -> DownloadBlockView {
    let mut lines = Vec::new();
    let status = match &snapshot.state {
        OcrModelDownloadState::Unknown | OcrModelDownloadState::Checking => {
            Some((StatusTone::Weak, t!("translation.ocr_model.checking_status").to_string()))
        }
        OcrModelDownloadState::Missing => Some((StatusTone::Warning, t!("translation.ocr_model.not_downloaded_status").to_string())),
        // A notice (cancelled / failed) already says why the download is unfinished.
        OcrModelDownloadState::Partial { .. } if snapshot.notice.is_some() => None,
        OcrModelDownloadState::Partial { .. } => {
            Some((StatusTone::Warning, t!("translation.ocr_model.partly_present_status").to_string()))
        }
        OcrModelDownloadState::Installed => Some((StatusTone::Success, t!("translation.ocr_model.installed_status").to_string())),
        OcrModelDownloadState::Downloading(_) | OcrModelDownloadState::Verifying(_) => None,
        OcrModelDownloadState::Unavailable(reason) => Some((StatusTone::Error, reason.clone())),
    };
    lines.extend(status);
    let offered = offered_download_label(snapshot);
    match &snapshot.notice {
        Some(DownloadNotice::Cancelled) => lines.push((StatusTone::Weak, t!("translation.ocr_model.cancelled_status").to_string())),
        Some(DownloadNotice::Failed(err)) => lines.push((StatusTone::Error, tf!("translation.ocr_model.failed_error", err = err))),
        Some(DownloadNotice::VerifyIntact) => lines.push((StatusTone::Weak, t!("translation.ocr_model.verify_intact_status").to_string())),
        Some(DownloadNotice::VerifyRemoved { files }) => {
            // Names the button the re-probed state offers (normally "Download (size)").
            let button = offered.clone().unwrap_or_else(|| download_button_label(snapshot.missing_bytes));
            lines.push((StatusTone::Warning, tf!("translation.ocr_model.verify_removed_status", count = files, button = button)));
        }
        Some(DownloadNotice::VerifyCancelled) => {
            lines.push((StatusTone::Weak, t!("translation.ocr_model.verify_cancelled_status").to_string()));
        }
        // The adoption notices name the button the re-probed state offers: a download keeps
        // every matching file and fetches only the rest.
        Some(DownloadNotice::AdoptMismatch { file }) => {
            let button = offered.clone().unwrap_or_else(|| download_button_label(snapshot.missing_bytes));
            lines.push((StatusTone::Warning, tf!("translation.ocr_model.adopt_mismatch_status", file = file, button = button)));
        }
        Some(DownloadNotice::AdoptFailed(err)) => lines.push((StatusTone::Error, tf!("translation.ocr_model.adopt_failed_error", err = err))),
        Some(DownloadNotice::AdoptCancelled) => {
            let button = offered.clone().unwrap_or_else(|| download_button_label(snapshot.missing_bytes));
            lines.push((StatusTone::Weak, tf!("translation.ocr_model.adopt_cancelled_status", button = button)));
        }
        // Drawn with the access lines below (reason + repository link).
        Some(DownloadNotice::Access { .. }) | None => {}
    }
    // A refused download names its repository first; the same repository's line from the
    // access map would repeat it, so it is skipped. The map's answer is never older than the
    // notice (the refusal itself is recorded there too, a later check overwrites it), so a
    // known map answer wins: a newer `Granted` removes the refusal line altogether.
    let refused = match &snapshot.notice {
        Some(DownloadNotice::Access { repo_id, access }) => Some((*repo_id, access)),
        Some(
            DownloadNotice::Cancelled
            | DownloadNotice::Failed(_)
            | DownloadNotice::VerifyIntact
            | DownloadNotice::VerifyRemoved { .. }
            | DownloadNotice::VerifyCancelled
            | DownloadNotice::AdoptMismatch { .. }
            | DownloadNotice::AdoptFailed(_)
            | DownloadNotice::AdoptCancelled,
        )
        | None => None,
    };
    let access: Vec<AccessLineView> = refused
        .and_then(|(repo_id, notice_access)| {
            let newer = snapshot.access.iter().find(|line| line.repo_id == repo_id).and_then(|line| line.access.as_ref());
            access_line_view(repo_id, Some(newer.unwrap_or(notice_access)))
        })
        .into_iter()
        .chain(
            snapshot
                .access
                .iter()
                .filter(|line| refused.is_none_or(|(repo_id, _)| repo_id != line.repo_id))
                .filter_map(|line| access_line_view(line.repo_id, line.access.as_ref())),
        )
        .collect();
    // The button the state offers: download / resume, or verify once installed.
    let idle_button = if let Some(caption) = offered {
        Some((caption, ModelDownloadAction::Download))
    } else if snapshot.state == OcrModelDownloadState::Installed {
        Some((t!("translation.ocr_model.verify_button").to_string(), ModelDownloadAction::Verify))
    } else {
        None
    };
    // Only a state that offers a button is blocked by another bundle's running job.
    if idle_button.is_some()
        && let Some(other) = &snapshot.other_download
    {
        lines.push((StatusTone::Weak, other_download_line(other)));
    }
    let running = match &snapshot.state {
        OcrModelDownloadState::Downloading(progress) | OcrModelDownloadState::Verifying(progress) => Some(progress),
        OcrModelDownloadState::Unknown
        | OcrModelDownloadState::Checking
        | OcrModelDownloadState::Missing
        | OcrModelDownloadState::Partial { .. }
        | OcrModelDownloadState::Installed
        | OcrModelDownloadState::Unavailable(_) => None,
    };
    let (progress, button) = if let Some(progress) = running {
        let cancel = BlockButton {
            caption: t!("translation.common.cancel_button").to_string(),
            action: ModelDownloadAction::Cancel,
            enabled: true,
        };
        (Some((progress_fraction(progress), progress_text(progress))), Some(cancel))
    } else {
        let button = idle_button.map(|(caption, action)| BlockButton { caption, action, enabled: snapshot.other_download.is_none() });
        (None, button)
    };
    DownloadBlockView { lines, access, progress, button }
}

/// Why a Load / Run button is disabled for this snapshot of the bundle with key `bundle_key`
/// (`ocr_model_download::bundle_key`), or `None` when the bundle is installed. A snapshot of
/// another bundle (taken before an engine / variant / language switch in this frame) counts
/// as still checking.
#[must_use]
pub fn load_blocked_reason(bundle_key: &str, snapshot: Option<&OcrModelPanelSnapshot>) -> Option<String> {
    let Some(snapshot) = snapshot.filter(|snapshot| snapshot.bundle_key == bundle_key) else {
        return Some(t!("translation.ocr_model.checking_status").to_string());
    };
    match &snapshot.state {
        OcrModelDownloadState::Installed => None,
        OcrModelDownloadState::Unknown | OcrModelDownloadState::Checking => Some(t!("translation.ocr_model.checking_status").to_string()),
        OcrModelDownloadState::Downloading(_) => Some(t!("translation.common.downloading_model_status").to_string()),
        OcrModelDownloadState::Verifying(_) => Some(t!("translation.ocr_model.verifying_files_status").to_string()),
        OcrModelDownloadState::Missing | OcrModelDownloadState::Partial { .. } => {
            Some(t!("translation.ocr_model.not_downloaded_status").to_string())
        }
        OcrModelDownloadState::Unavailable(reason) => Some(reason.clone()),
    }
}

/// Overall fraction done, `0.0..=1.0`, from whole-model byte counters. Integer per-mille
/// first, so the `f32` comes from a lossless `u16` conversion.
fn progress_fraction(progress: &ExternalDownloadProgress) -> f32 {
    if progress.total_bytes == 0 {
        return 0.0;
    }
    let per_mille = (u128::from(progress.total_done) * 1000 / u128::from(progress.total_bytes)).min(1000);
    // `per_mille <= 1000` after the clamp, so it fits in `u16`.
    let per_mille = u16::try_from(per_mille).unwrap_or(1000);
    f32::from(per_mille) / 1000.0
}

/// Progress bar text: which file (1-based) and the byte counters while downloading, the
/// file being hashed while verifying or adopting found files, the retry number and countdown (then "reconnecting")
/// while the downloader waits out a transient network failure.
fn progress_text(progress: &ExternalDownloadProgress) -> String {
    match progress.phase {
        ExternalDownloadPhase::Downloading => tf!(
            "translation.ocr_model.downloading_status",
            index = progress.file_index.saturating_add(1),
            count = progress.file_count,
            done = format_bytes(progress.total_done),
            total = format_bytes(progress.total_bytes)
        ),
        ExternalDownloadPhase::Verifying => tf!("translation.ocr_model.verifying_status", file = progress.file_path),
        ExternalDownloadPhase::Adopting => tf!("translation.ocr_model.adopting_status", file = progress.file_path),
        ExternalDownloadPhase::Retrying { attempt, max_attempts, delay_secs: 0 } => tf!(
            "translation.ocr_model.reconnecting_status",
            attempt = attempt,
            max = max_attempts,
            done = format_bytes(progress.total_done),
            total = format_bytes(progress.total_bytes)
        ),
        ExternalDownloadPhase::Retrying { attempt, max_attempts, delay_secs } => tf!(
            "translation.ocr_model.retrying_status",
            attempt = attempt,
            max = max_attempts,
            seconds = delay_secs,
            done = format_bytes(progress.total_done),
            total = format_bytes(progress.total_bytes)
        ),
    }
}

/// Draws the download block of a bundle from its `snapshot` and returns the clicked action.
/// The caller draws it only when `snapshot.bundle_key` matches the bundle it currently needs.
pub fn draw_ocr_model_download(ui: &mut egui::Ui, snapshot: &OcrModelPanelSnapshot) -> ModelDownloadAction {
    let view = download_block_view(snapshot);
    let mut action = ModelDownloadAction::None;
    for (tone, text) in &view.lines {
        ui.colored_label(tone_color(ui, *tone), text);
    }
    for line in &view.access {
        ui.colored_label(tone_color(ui, line.tone), &line.text);
        if let Some(repo_id) = line.link {
            draw_repo_link(ui, repo_id);
        }
    }
    if let Some((fraction, text)) = &view.progress {
        ui.add(egui::ProgressBar::new(*fraction).text(text.as_str()));
    }
    if let Some(button) = &view.button
        && ui.add_enabled(button.enabled, egui::Button::new(button.caption.as_str())).clicked()
    {
        action = button.action;
    }
    action
}

/// The colour of a line of the given role.
fn tone_color(ui: &egui::Ui, tone: StatusTone) -> egui::Color32 {
    match tone {
        StatusTone::Weak => ui.visuals().weak_text_color(),
        StatusTone::Success => ms_theme::status::SUCCESS,
        StatusTone::Warning => ms_theme::status::WARNING,
        StatusTone::Error => ms_theme::status::ERROR,
    }
}

/// Draws the "open the repository page" link of `repo_id` and opens the page in the default
/// browser on a click; a failure to start the browser is logged and shown under the link until
/// a later click succeeds (kept in egui temp data under an id scoped to this block and repo).
fn draw_repo_link(ui: &mut egui::Ui, repo_id: &'static str) {
    let error_id = ui.make_persistent_id(("ocr_model_download_repo_link_error", repo_id));
    if ui.link(t!("translation.ocr_model.access_open_repo_link")).clicked() {
        let url = ms_widgets::hf_access_hint::hf_repo_url(repo_id);
        match ms_platform::shell_open::open_url(&url) {
            Ok(()) => ui.ctx().data_mut(|data| data.remove::<String>(error_id)),
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[ocr-model] opening the repository page {url} failed: {err:?}"));
                let message = err.user_message();
                ui.ctx().data_mut(|data| data.insert_temp(error_id, message));
            }
        }
    }
    if let Some(message) = ui.ctx().data(|data| data.get_temp::<String>(error_id)) {
        ui.colored_label(ms_theme::status::ERROR, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr_model_download::{RepoAccessLine, bundle_key};

    const BABERU: &[&ExternalModelSpec] = &[&BABERU_OCR];

    fn progress(phase: ExternalDownloadPhase, total_done: u64) -> ExternalDownloadProgress {
        ExternalDownloadProgress {
            file_index: 1,
            file_count: 5,
            file_path: "onnx/decoder_prefill_int8.onnx",
            phase,
            file_done: 0,
            file_total: 10,
            total_done,
            total_bytes: BABERU_OCR.total_bytes(),
        }
    }

    fn snapshot(state: OcrModelDownloadState) -> OcrModelPanelSnapshot {
        OcrModelPanelSnapshot {
            bundle_key: bundle_key(BABERU),
            state,
            missing_bytes: BABERU_OCR.total_bytes(),
            notice: None,
            other_download: None,
            access: Vec::new(),
        }
    }

    fn download_button(caption: String, enabled: bool) -> Option<BlockButton> {
        Some(BlockButton { caption, action: ModelDownloadAction::Download, enabled })
    }

    #[test]
    fn missing_and_partial_offer_the_owned_download_caption() {
        let _locale = crate::hold_locale_for_test();
        for state in [OcrModelDownloadState::Missing, OcrModelDownloadState::Partial { bytes_present: 1024 }] {
            let shown = snapshot(state.clone());
            let view = download_block_view(&shown);
            let caption = offered_download_label(&shown).expect("a download state");
            assert_eq!(view.button, download_button(caption, true), "{state:?}");
            assert_eq!(view.lines.len(), 1, "{state:?}");
        }
        // A partly present model without a notice says so (an interrupted download or files found
        // without a marker, L2).
        let partial = download_block_view(&snapshot(OcrModelDownloadState::Partial { bytes_present: 1024 }));
        assert_eq!(partial.lines, vec![(StatusTone::Warning, t!("translation.ocr_model.partly_present_status").to_string())]);
    }

    /// M1: after a cancel / failure the block shows the notice next to the re-probed
    /// state's own button, never a separate "Retry" caption.
    #[test]
    fn notices_keep_the_reprobed_button() {
        let _locale = crate::hold_locale_for_test();
        let state = OcrModelDownloadState::Partial { bytes_present: 1024 };
        let caption = offered_download_label(&snapshot(state.clone())).expect("a download state");
        let cancelled = OcrModelPanelSnapshot { notice: Some(DownloadNotice::Cancelled), ..snapshot(state.clone()) };
        let view = download_block_view(&cancelled);
        assert_eq!(view.lines, vec![(StatusTone::Weak, t!("translation.ocr_model.cancelled_status").to_string())]);
        assert_eq!(view.button, download_button(caption, true));

        let failed = OcrModelPanelSnapshot { notice: Some(DownloadNotice::Failed("boom".to_owned())), ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&failed);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Error, tf!("translation.ocr_model.failed_error", err = "boom"))));
        assert_eq!(view.button, download_button(offered_download_label(&failed).expect("missing"), true));
    }

    /// M2: another bundle's download is named with its progress and disables the button.
    #[test]
    fn another_running_download_is_named_and_disables_the_button() {
        let _locale = crate::hold_locale_for_test();
        let other_specs = vec![&SURYA_DETECTION_2025_05_07, &SURYA_RECOGNITION_2025_09_23];
        let other = OtherDownload { specs: other_specs, kind: JobKind::Download, progress: progress(ExternalDownloadPhase::Downloading, 1024) };
        let blocked = OcrModelPanelSnapshot { other_download: Some(other.clone()), ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&blocked);
        let expected_line = tf!(
            "translation.ocr_model.other_download_status",
            name = format!(
                "{} + {}",
                external_model_display_name(&SURYA_DETECTION_2025_05_07),
                external_model_display_name(&SURYA_RECOGNITION_2025_09_23)
            ),
            done = format_bytes(1024),
            total = format_bytes(BABERU_OCR.total_bytes())
        );
        assert_eq!(view.lines.last(), Some(&(StatusTone::Weak, expected_line)));
        let button = view.button.expect("the download button stays visible");
        assert!(!button.enabled);
        assert_eq!(button.action, ModelDownloadAction::Download);
        // An installed model offers Verify, which the running download disables too.
        let installed = OcrModelPanelSnapshot { other_download: Some(other), ..snapshot(OcrModelDownloadState::Installed) };
        let view = download_block_view(&installed);
        assert_eq!(view.lines.len(), 2);
        assert_eq!(view.button.map(|button| (button.action, button.enabled)), Some((ModelDownloadAction::Verify, false)));
    }

    /// An installed bundle offers "Verify files" (disabled while another job runs); a running
    /// verify shows its progress and Cancel and blocks loading.
    #[test]
    fn installed_offers_verify_and_a_running_verify_shows_progress() {
        let _locale = crate::hold_locale_for_test();
        let installed = download_block_view(&snapshot(OcrModelDownloadState::Installed));
        let verify = Some(BlockButton { caption: t!("translation.ocr_model.verify_button").to_string(), action: ModelDownloadAction::Verify, enabled: true });
        assert_eq!(installed.button, verify);
        let other = OtherDownload { specs: vec![&SURYA_DETECTION_2025_05_07], kind: JobKind::Verify, progress: progress(ExternalDownloadPhase::Verifying, 5) };
        let blocked = download_block_view(&OcrModelPanelSnapshot { other_download: Some(other.clone()), ..snapshot(OcrModelDownloadState::Installed) });
        assert_eq!(blocked.button.map(|button| (button.action, button.enabled)), Some((ModelDownloadAction::Verify, false)));
        assert_eq!(blocked.lines.last(), Some(&(StatusTone::Weak, other_download_line(&other))));

        let running = snapshot(OcrModelDownloadState::Verifying(progress(ExternalDownloadPhase::Verifying, 0)));
        let view = download_block_view(&running);
        assert_eq!(view.button.map(|button| button.action), Some(ModelDownloadAction::Cancel));
        assert!(view.progress.is_some());
        assert_eq!(load_blocked_reason(&running.bundle_key, Some(&running)), Some(t!("translation.ocr_model.verifying_files_status").to_string()));
    }

    /// The verify notices: intact (neutral, next to Verify), N files removed naming the
    /// re-probed state's Download button, cancelled.
    #[test]
    fn verify_notices_are_shown_next_to_the_state() {
        let _locale = crate::hold_locale_for_test();
        let intact = OcrModelPanelSnapshot { notice: Some(DownloadNotice::VerifyIntact), ..snapshot(OcrModelDownloadState::Installed) };
        let view = download_block_view(&intact);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Weak, t!("translation.ocr_model.verify_intact_status").to_string())));
        assert_eq!(view.button.map(|button| button.action), Some(ModelDownloadAction::Verify));

        let removed = OcrModelPanelSnapshot { notice: Some(DownloadNotice::VerifyRemoved { files: 2 }), ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&removed);
        let caption = offered_download_label(&removed).expect("missing offers a download");
        assert_eq!(view.lines.last(), Some(&(StatusTone::Warning, tf!("translation.ocr_model.verify_removed_status", count = 2, button = caption.clone()))));
        assert_eq!(view.button, download_button(caption, true));

        let cancelled = OcrModelPanelSnapshot { notice: Some(DownloadNotice::VerifyCancelled), ..snapshot(OcrModelDownloadState::Installed) };
        let view = download_block_view(&cancelled);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Weak, t!("translation.ocr_model.verify_cancelled_status").to_string())));
    }

    #[test]
    fn checking_and_unavailable_offer_no_button() {
        let _locale = crate::hold_locale_for_test();
        for state in [
            OcrModelDownloadState::Checking,
            OcrModelDownloadState::Unknown,
            OcrModelDownloadState::Unavailable("gone".to_owned()),
        ] {
            let view = download_block_view(&snapshot(state.clone()));
            assert_eq!(view.button, None, "{state:?}");
            assert!(view.progress.is_none(), "{state:?}");
        }
    }

    #[test]
    fn downloading_shows_progress_and_cancel() {
        let _locale = crate::hold_locale_for_test();
        let half = BABERU_OCR.total_bytes() / 2;
        let view = download_block_view(&snapshot(OcrModelDownloadState::Downloading(progress(ExternalDownloadPhase::Downloading, half))));
        assert_eq!(
            view.button,
            Some(BlockButton { caption: t!("translation.common.cancel_button").to_string(), action: ModelDownloadAction::Cancel, enabled: true })
        );
        let (fraction, text) = view.progress.expect("progress bar while downloading");
        assert!((fraction - 0.5).abs() < 0.002, "{fraction}");
        assert_eq!(
            text,
            tf!(
                "translation.ocr_model.downloading_status",
                index = 2,
                count = 5,
                done = format_bytes(half),
                total = format_bytes(BABERU_OCR.total_bytes())
            )
        );
        let verifying = progress(ExternalDownloadPhase::Verifying, 0);
        assert_eq!(progress_text(&verifying), tf!("translation.ocr_model.verifying_status", file = "onnx/decoder_prefill_int8.onnx"));
    }

    /// A retry wait shows the attempt and the countdown, then "reconnecting" at zero, with
    /// the byte counters kept; the block still offers Cancel.
    #[test]
    fn retrying_shows_the_attempt_and_the_countdown() {
        let _locale = crate::hold_locale_for_test();
        let waiting = progress(ExternalDownloadPhase::Retrying { attempt: 2, max_attempts: 5, delay_secs: 4 }, 1024);
        let total = format_bytes(BABERU_OCR.total_bytes());
        assert_eq!(
            progress_text(&waiting),
            tf!("translation.ocr_model.retrying_status", attempt = 2, max = 5, seconds = 4, done = format_bytes(1024), total = total.clone())
        );
        let now = progress(ExternalDownloadPhase::Retrying { attempt: 2, max_attempts: 5, delay_secs: 0 }, 1024);
        assert_eq!(
            progress_text(&now),
            tf!("translation.ocr_model.reconnecting_status", attempt = 2, max = 5, done = format_bytes(1024), total = total)
        );
        assert_ne!(progress_text(&waiting), progress_text(&now));
        let view = download_block_view(&snapshot(OcrModelDownloadState::Downloading(waiting)));
        assert_eq!(view.button.map(|button| button.action), Some(ModelDownloadAction::Cancel));
        assert_eq!(view.progress.map(|(_, text)| text), Some(progress_text(&waiting)));
    }

    /// N2 / N3: the Load button's disabled reason follows the state, and a snapshot of a
    /// different bundle (stale for one frame after a switch) never counts as installed.
    #[test]
    fn load_blocked_reason_follows_the_state_and_the_bundle() {
        let _locale = crate::hold_locale_for_test();
        let key = bundle_key(BABERU);
        let checking = t!("translation.ocr_model.checking_status").to_string();
        assert_eq!(load_blocked_reason(&key, None), Some(checking.clone()));
        assert_eq!(load_blocked_reason(&key, Some(&snapshot(OcrModelDownloadState::Installed))), None);
        assert_eq!(load_blocked_reason(&key, Some(&snapshot(OcrModelDownloadState::Checking))), Some(checking.clone()));
        assert_eq!(
            load_blocked_reason(&key, Some(&snapshot(OcrModelDownloadState::Downloading(progress(ExternalDownloadPhase::Downloading, 0))))),
            Some(t!("translation.common.downloading_model_status").to_string())
        );
        assert_eq!(
            load_blocked_reason(&key, Some(&snapshot(OcrModelDownloadState::Missing))),
            Some(t!("translation.ocr_model.not_downloaded_status").to_string())
        );
        let stale = OcrModelPanelSnapshot { bundle_key: bundle_key(&[PaddleVlVariant::Official16.spec()]), ..snapshot(OcrModelDownloadState::Installed) };
        assert_eq!(load_blocked_reason(&key, Some(&stale)), Some(checking));
    }

    /// Every catalog spec a panel can show has its own name, never the raw id.
    #[test]
    fn display_names_cover_every_external_spec() {
        let _locale = crate::hold_locale_for_test();
        assert_eq!(external_model_display_name(&BABERU_OCR), "Baberu OCR");
        for variant in PaddleVlVariant::ALL {
            assert_eq!(external_model_display_name(variant.spec()), crate::panels::ocr::paddle_vl_variant_label(variant));
        }
        for script in EasyOcrScript::ALL {
            let spec = easyocr_spec(script);
            assert_eq!(external_model_display_name(spec), tf!("translation.ocr_model.easyocr_model_name", file = script.recognizer_file()));
        }
        for spec in ms_sysprobe::ai_models::external_catalog::all_specs() {
            assert_ne!(external_model_display_name(spec), spec.id, "{}", spec.id);
        }
    }

    /// Every FLUX.2 klein catalog spec (both variants, both encoder sources, the adopt-only
    /// legacy transformer) has its own name, naming its variant, and its quant token or dtype.
    #[test]
    fn flux2_klein_parts_have_distinct_names_with_their_variant_and_quant() {
        // The names are checked for literal tokens: pin the English catalog, so the test does
        // not depend on another test having installed a locale first.
        let _locale = crate::english_locale_for_test();
        let mut names = Vec::new();
        for spec in ms_sysprobe::ai_models::external_catalog::all_specs() {
            let Some(role) = flux2_klein_spec_role(spec.id) else { continue };
            let name = external_model_display_name(spec);
            let variant = match role {
                Flux2SpecRole::Base { variant } | Flux2SpecRole::TextEncoderConfig { variant } | Flux2SpecRole::LegacyTransformer { variant } => variant,
                Flux2SpecRole::Weights { variant, axis, source, format } => {
                    match (axis, format.quant_token()) {
                        (_, Some(quant)) => assert!(name.contains(quant), "{name}"),
                        (Flux2WeightAxis::TextEncoder, None) => assert!(name.contains(source.unquantized_token()), "{name}"),
                        (Flux2WeightAxis::Transformer, None) => {}
                    }
                    variant
                }
            };
            assert!(name.contains(variant.size_token()), "{name}");
            names.push(name);
        }
        assert_eq!(names.len(), 28, "every FLUX.2 klein catalog spec is named");
        let count = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), count, "names must be distinct: {names:?}");
    }

    /// A gated repository that is not granted gets a line with its reason and the repository
    /// link (checking: a neutral line, no link); granted repositories get none; a refused
    /// download names its repository once, first. The Download button stays enabled.
    #[test]
    fn access_lines_name_the_reason_and_link_the_repository() {
        let _locale = crate::hold_locale_for_test();
        let lines = vec![
            RepoAccessLine { repo_id: "owner/open", access: Some(HfAccess::Granted) },
            RepoAccessLine { repo_id: "owner/terms", access: Some(HfAccess::TermsNotAccepted) },
            RepoAccessLine { repo_id: "owner/wait", access: None },
            RepoAccessLine { repo_id: "owner/gone", access: Some(HfAccess::Unreachable { detail: "english-detail".to_owned() }) },
        ];
        let shown = OcrModelPanelSnapshot { access: lines, ..snapshot(OcrModelDownloadState::Missing) };
        let view = download_block_view(&shown);
        let terms = ms_i18n::interpolate(ms_i18n::resolve_key("translation.ocr_model.access_terms_status"), &[("repo", &"owner/terms")]);
        assert_eq!(
            view.access,
            vec![
                AccessLineView { tone: StatusTone::Warning, text: terms.clone(), link: Some("owner/terms") },
                AccessLineView { tone: StatusTone::Weak, text: tf!("translation.ocr_model.access_checking_status", repo = "owner/wait"), link: None },
                AccessLineView {
                    tone: StatusTone::Error,
                    text: ms_i18n::interpolate(ms_i18n::resolve_key("translation.ocr_model.access_unreachable_status"), &[("repo", &"owner/gone")]),
                    link: Some("owner/gone"),
                },
            ]
        );
        assert!(view.access.iter().all(|line| !line.text.contains("english-detail")));
        assert_eq!(view.button.map(|button| (button.action, button.enabled)), Some((ModelDownloadAction::Download, true)));

        let refused = OcrModelPanelSnapshot {
            notice: Some(DownloadNotice::Access { repo_id: "owner/terms", access: HfAccess::TermsNotAccepted }),
            ..shown
        };
        let view = download_block_view(&refused);
        assert_eq!(view.access.first(), Some(&AccessLineView { tone: StatusTone::Warning, text: terms, link: Some("owner/terms") }));
        assert_eq!(view.access.iter().filter(|line| line.link == Some("owner/terms")).count(), 1);
        assert_eq!(view.access.len(), 3);

        // A newer map answer wins over the notice: the terms were accepted meanwhile, so the
        // refusal line disappears; while the repository is re-checked the notice still shows.
        let mut accepted = refused.clone();
        accepted.access[1].access = Some(HfAccess::Granted);
        let view = download_block_view(&accepted);
        assert!(view.access.iter().all(|line| line.link != Some("owner/terms")), "{:?}", view.access);
        let mut rechecking = refused;
        rechecking.access[1].access = None;
        assert_eq!(download_block_view(&rechecking).access.first().map(|line| line.link), Some(Some("owner/terms")));
    }

    /// Adoption: the progress text says found files are being checked; the notices name the
    /// re-probed state's Download button; another bundle's adoption is named as such.
    #[test]
    fn adoption_progress_and_notices_are_shown() {
        let _locale = crate::hold_locale_for_test();
        let adopting = progress(ExternalDownloadPhase::Adopting, 5);
        assert_eq!(progress_text(&adopting), tf!("translation.ocr_model.adopting_status", file = "onnx/decoder_prefill_int8.onnx"));
        let running = download_block_view(&snapshot(OcrModelDownloadState::Verifying(adopting)));
        assert_eq!(running.button.map(|button| button.action), Some(ModelDownloadAction::Cancel));
        assert_eq!(running.progress.map(|(_, text)| text), Some(progress_text(&adopting)));

        let mismatch = OcrModelPanelSnapshot { notice: Some(DownloadNotice::AdoptMismatch { file: "a.bin" }), ..snapshot(OcrModelDownloadState::Missing) };
        let caption = offered_download_label(&mismatch).expect("missing offers a download");
        let view = download_block_view(&mismatch);
        assert_eq!(view.lines.last(), Some(&(StatusTone::Warning, tf!("translation.ocr_model.adopt_mismatch_status", file = "a.bin", button = caption.clone()))));
        assert_eq!(view.button, download_button(caption.clone(), true));
        let cancelled = OcrModelPanelSnapshot { notice: Some(DownloadNotice::AdoptCancelled), ..snapshot(OcrModelDownloadState::Missing) };
        assert_eq!(download_block_view(&cancelled).lines.last(), Some(&(StatusTone::Weak, tf!("translation.ocr_model.adopt_cancelled_status", button = caption))));
        let failed = OcrModelPanelSnapshot { notice: Some(DownloadNotice::AdoptFailed("boom".to_owned())), ..snapshot(OcrModelDownloadState::Missing) };
        assert_eq!(download_block_view(&failed).lines.last(), Some(&(StatusTone::Error, tf!("translation.ocr_model.adopt_failed_error", err = "boom"))));
        // Adoption notices go stale once the bundle reads installed (not verify notices).
        assert!(!DownloadNotice::AdoptCancelled.is_verify());

        let other = OtherDownload { specs: vec![&BABERU_OCR], kind: JobKind::Adopt, progress: adopting };
        let name = external_model_display_name(&BABERU_OCR);
        assert_eq!(
            other_download_line(&other),
            tf!("translation.ocr_model.other_adopt_status", name = name, done = format_bytes(5), total = format_bytes(BABERU_OCR.total_bytes()))
        );
    }

    #[test]
    fn progress_fraction_is_clamped_and_safe_on_empty_totals() {
        let mut report = progress(ExternalDownloadPhase::Downloading, 0);
        assert!(progress_fraction(&report).abs() < f32::EPSILON);
        report.total_done = report.total_bytes.saturating_mul(2);
        assert!((progress_fraction(&report) - 1.0).abs() < f32::EPSILON);
        report.total_bytes = 0;
        assert!(progress_fraction(&report).abs() < f32::EPSILON);
    }
}
