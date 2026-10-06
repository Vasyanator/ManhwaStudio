/*
File: crates/ms-launcher/src/pages/system_registration.rs

Purpose:
The "System registration" tab of the launcher settings page (Windows and Linux only): lists
every OS record of this program copy (Start-menu shortcut / application-menu entry, installed
programs entry, App Paths, "Open with") as `ms_os_integration::report::probe` judged it, and
runs the actions `ms_os_integration::actions::allowed_actions` offers for each record.

Key structures:
- `SystemRegistrationState`: probe and action worker lifecycle, the inline confirmation of a
  destructive action on a record that may not be this copy's (another copy's, an unreadable
  one, one launching a foreign program), and the outcome of the last action.
- `ActionReport`: what one action batch did (failures, declined UAC prompt, helper failure).

Key functions:
- `SystemRegistrationState::show`: draws the tab; returns `true` when the OS records may have
  changed (an action finished, or the user refreshed), so the caller rechecks the
  registration warnings (`SettingChange::SystemRegistration`).
- `status_view`, `row_buttons`, `split_by_elevation`, `run_actions`: the pure view model and
  the worker body.

Notes:
All OS I/O runs on two named workers, never on the GUI thread: `system-registration-probe`
(`CopyIdentity::current` + `report::probe`) and `system-registration-action` (in-process
`actions::apply` for records this process may write, and on Windows
`windows::elevation::apply_elevated` for all-users records of an unelevated process; a mixed
batch is split). One worker runs at a time and every button is disabled meanwhile. Every
rule (what is broken, which actions exist, which need confirmation or elevation) is consumed
from `ms_os_integration`; this file only presents it. Under `--ignore-installed` the tab is
read-only (`allowed_actions(.., read_only = true)` offers nothing). A repository build
(`CopyIdentity::repo_build_root`) shows a notice naming its repository root; any other copy
without program files next to its exe (`RegistrationReport::dev_copy`) shows the generic one.
*/

use std::sync::mpsc::{self, Receiver, TryRecvError};
#[cfg(target_os = "windows")]
use std::time::Duration;

use egui::{Color32, Grid, Layout, Ui};
use ms_log::runtime_log;
use ms_os_integration::CopyIdentity;
use ms_os_integration::actions::{self, ActionKind, ActionRequest};
use ms_os_integration::report::{self, Defect, DefectSeverity, RecordReport, RecordStatus, RegistrationReport, Scope};
use ms_settings_ui::settings_warnings::{WarningSet, item_warning_badge, registration_key, registration_record};
use ms_thread as thread;

use crate::theme;

/// How long the launcher waits for the elevated helper to finish before reporting that its
/// outcome is unknown. Counted from the helper's start: the time spent on the UAC prompt is not
/// included (`apply_elevated` launches with `SEE_MASK_NOASYNC`).
#[cfg(target_os = "windows")]
const ELEVATED_HELPER_TIMEOUT: Duration = Duration::from_secs(120);

/// Minimum width, in points, of the record grid's status column (three times the width it
/// collapsed to on its own).
const STATUS_COLUMN_MIN_WIDTH: f32 = 180.0;

/// What the probe worker found: the copy it judged and its report.
#[derive(Debug, Clone)]
struct ProbeSnapshot {
    identity: CopyIdentity,
    report: RegistrationReport,
}

/// The probe worker's message: the snapshot, or the localized reason there is none.
type ProbeResult = Result<ProbeSnapshot, String>;

/// One action of a batch that failed, with its localized reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailedAction {
    pub(crate) request: ActionRequest,
    pub(crate) message: String,
}

/// What one action batch did. Empty (`Default`) = every action succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ActionReport {
    /// Actions that ran (in-process or in the helper) and failed.
    pub(crate) failures: Vec<FailedAction>,
    /// The localized "rights were not granted" text when the UAC prompt was declined (the
    /// elevated part changed nothing).
    pub(crate) declined: Option<String>,
    /// The localized failure of the elevated round trip itself (launch, wait, timeout, result
    /// file); the elevated part's outcome is unknown.
    pub(crate) elevation_error: Option<String>,
}

/// The outcome line(s) shown under the tab header after an action.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LastOutcome {
    /// The worker reported its batch.
    Finished(ActionReport),
    /// The action worker ended without a report (it panicked); the records were re-probed.
    WorkerLost,
    /// The action worker could not be started (localized reason); nothing ran.
    NotStarted(String),
}

/// A destructive action on a record that may not be this copy's, waiting for the inline
/// confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingConfirm {
    request: ActionRequest,
    /// Shown in the question: the other copy's executable, the foreign program a record at our
    /// name launches, or the location of a record that could not be read.
    subject: String,
    /// Whose the record is is unknown (unreadable, or a foreign program): the question says so
    /// instead of naming another working copy.
    unknown_owner: bool,
}

/// How a record's status reads in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Missing,
    OursOk,
    OursStale,
    OursBroken,
    OtherAlive,
    OtherDead,
    Unreadable,
}

/// The colour role of a status text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    /// Nothing to do (not registered).
    Muted,
    /// Registered for this copy and correct.
    Good,
    /// Works, but deserves a look (outdated values, another copy's record, unreadable).
    Notice,
    /// Does not work: exactly the records `report::badge_worthy` flags.
    Bad,
}

impl Tone {
    /// The launcher palette colour of the role.
    fn color(self) -> Color32 {
        match self {
            Tone::Muted => theme::TEXT_MUTED,
            Tone::Good => theme::STATUS_SUCCESS,
            Tone::Notice => theme::NOTICE_TEXT,
            Tone::Bad => theme::STATUS_ERROR,
        }
    }
}

/// One action button of a record row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActionButton {
    action: ActionKind,
    /// The click asks for an inline confirmation first (`actions::needs_confirmation`).
    confirm: bool,
    /// The action runs in the elevated helper (`actions::requires_elevation`): the button
    /// carries the administrator hint.
    elevated: bool,
}

/// State of the System registration tab. Created with the settings page; the first probe
/// starts when the tab is first shown.
pub struct SystemRegistrationState {
    /// This build's `version_core` (the Uninstall entry's `DisplayVersion`).
    version_core: &'static str,
    /// `--ignore-installed`: records are shown, no action is offered.
    read_only: bool,
    /// The last successful probe.
    snapshot: Option<ProbeSnapshot>,
    /// The localized reason the last probe produced no snapshot.
    probe_error: Option<String>,
    probe_rx: Option<Receiver<ProbeResult>>,
    /// A probe should start as soon as no worker runs (first show, after an action, refresh).
    probe_requested: bool,
    action_rx: Option<Receiver<ActionReport>>,
    confirm: Option<PendingConfirm>,
    last_outcome: Option<LastOutcome>,
}

impl SystemRegistrationState {
    /// A tab that probes on its first `show`. `version_core` is this build's version core;
    /// `read_only` is `--ignore-installed`.
    #[must_use]
    pub fn new(version_core: &'static str, read_only: bool) -> Self {
        Self {
            version_core,
            read_only,
            snapshot: None,
            probe_error: None,
            probe_rx: None,
            probe_requested: true,
            action_rx: None,
            confirm: None,
            last_outcome: None,
        }
    }

    /// A probe or an action worker is running: every button is disabled.
    fn busy(&self) -> bool {
        self.probe_rx.is_some() || self.action_rx.is_some()
    }

    /// Draws the tab and drives its workers (non-blocking). `warnings` provides the row
    /// badges. Returns `true` when the OS records may have changed since the last check (an
    /// action batch ended, whatever its outcome, or the user pressed refresh): the caller then
    /// rechecks `SettingChange::SystemRegistration`.
    pub fn show(&mut self, ui: &mut Ui, warnings: Option<&WarningSet>) -> bool {
        let mut records_changed = self.poll_action();
        self.poll_probe();
        if self.probe_requested && !self.busy() {
            self.start_probe(ui.ctx());
        }

        ui.add(egui::Label::new(theme::status(t!("launcher.sysreg.intro_label"), theme::TEXT_MUTED)).wrap());
        ui.add_space(10.0);
        if self.show_header(ui) {
            records_changed = true;
        }
        ui.add_space(10.0);

        let width = ui.available_width();
        if self.read_only {
            theme::notice_banner(ui, "launcher.sysreg.read_only_notice_label", width, t!("launcher.sysreg.read_only_notice_label"), None);
            ui.add_space(8.0);
        }
        if let Some(snapshot) = self.snapshot.as_ref() {
            // A repository build gets the precise notice (its records start in the repository
            // root); any other copy without program files next to it gets the generic one.
            if let Some(repo_root) = snapshot.identity.repo_build_root() {
                let text = tf!("launcher.sysreg.repo_build_notice_label", root = repo_root.display());
                theme::notice_banner(ui, "launcher.sysreg.repo_build_notice_label", width, &text, None);
                ui.add_space(8.0);
            } else if snapshot.report.dev_copy {
                let text = tf!("launcher.sysreg.dev_copy_notice_label", exe = snapshot.identity.exe.display());
                theme::notice_banner(ui, "launcher.sysreg.dev_copy_notice_label", width, &text, None);
                ui.add_space(8.0);
            }
        }
        self.show_last_outcome(ui, width);
        if let Some(error) = &self.probe_error {
            ui.label(theme::status(error, theme::STATUS_ERROR));
            ui.add_space(8.0);
        }

        // The rows only borrow the snapshot; a click is collected and handled after the borrow
        // ends, so `self` can then change (confirmation, worker) without cloning the report.
        let Some(snapshot) = &self.snapshot else {
            return records_changed;
        };
        let clicked = self.show_records(ui, &snapshot.report, warnings);
        if let Some((record, button)) = clicked
            && let Some(request) = self.on_action_clicked(&record, button)
        {
            let ctx = ui.ctx().clone();
            self.start_action(&ctx, vec![request]);
        }
        self.show_confirm(ui);
        ui.add_space(12.0);
        if let Some(snapshot) = &self.snapshot {
            show_details(ui, &snapshot.report);
        }
        records_changed
    }

    /// The status line and the refresh button. Returns `true` when refresh was clicked.
    fn show_header(&mut self, ui: &mut Ui) -> bool {
        let busy = self.busy();
        let mut refresh_clicked = false;
        ui.horizontal(|ui| {
            if self.action_rx.is_some() {
                ui.label(theme::status(t!("launcher.sysreg.applying_status"), theme::TEXT_MUTED));
            } else if self.probe_rx.is_some() {
                ui.label(theme::status(t!("launcher.sysreg.probing_status"), theme::TEXT_MUTED));
            }
            ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::launcher_button(ui, t!("launcher.common.refresh_button"), egui::vec2(112.0, 34.0), !busy).clicked() {
                    refresh_clicked = true;
                }
            });
        });
        if refresh_clicked {
            self.probe_requested = true;
            self.confirm = None;
            self.last_outcome = None;
        }
        refresh_clicked
    }

    /// One grid row per probed record: name (+ badge), scope, status, actions. Returns the
    /// clicked button with its record (a clone of that one row), for the caller to route.
    fn show_records(&self, ui: &mut Ui, report: &RegistrationReport, warnings: Option<&WarningSet>) -> Option<(RecordReport, ActionButton)> {
        let busy = self.busy();
        let mut clicked: Option<(RecordReport, ActionButton)> = None;
        Grid::new("launcher.sysreg.records_grid").num_columns(4).spacing([18.0, 10.0]).show(ui, |ui| {
            for record in &report.records {
                ui.horizontal(|ui| {
                    ui.label(registration_record(record.kind).label());
                    // Only a row that is itself broken carries its key's badge: App Paths
                    // shares the program-entry key, and a clean sibling must stay clean.
                    if report::badge_worthy(&record.status) {
                        item_warning_badge(ui, warnings, registration_key(record.kind));
                    }
                });
                ui.label(theme::status(scope_label(record.scope), theme::TEXT_MUTED));
                let (kind, tone) = status_view(&record.status);
                // A wrapping label in an auto-sized grid column shrinks to its narrowest
                // word, so the status column gets an explicit minimum width.
                ui.vertical(|ui| {
                    ui.set_min_width(STATUS_COLUMN_MIN_WIDTH);
                    ui.add(egui::Label::new(theme::status(&status_text(kind, &record.status), tone.color())).wrap())
                        .on_hover_text(&record.location);
                });
                ui.horizontal(|ui| {
                    let buttons = row_buttons(record, report, self.read_only);
                    for button in &buttons {
                        let response = theme::launcher_button_small(ui, action_label(button.action), !busy);
                        let response = if button.elevated { response.on_hover_text(t!("launcher.sysreg.admin_required_hint")) } else { response };
                        if response.clicked() {
                            clicked = Some((record.clone(), *button));
                        }
                    }
                    if buttons.iter().any(|button| button.elevated) {
                        ui.label(theme::status(t!("launcher.sysreg.admin_required_label"), theme::NOTICE_TEXT))
                            .on_hover_text(t!("launcher.sysreg.admin_required_hint"));
                    }
                });
                ui.end_row();
            }
        });
        clicked
    }

    /// Routes a click on an action button: a confirmed action becomes the pending
    /// confirmation (returns `None`), any other one is returned to run now.
    fn on_action_clicked(&mut self, record: &RecordReport, button: ActionButton) -> Option<ActionRequest> {
        let request = ActionRequest { kind: record.kind, scope: record.scope, action: button.action };
        if button.confirm {
            let (subject, unknown_owner) = match &record.status {
                RecordStatus::OtherCopy { exe, .. } => (exe.display().to_string(), false),
                RecordStatus::OursBroken(defects) => {
                    let foreign = defects.iter().find_map(|defect| match defect {
                        Defect::ForeignProgram { found, .. } => Some(found.clone()),
                        Defect::TargetMissing { .. }
                        | Defect::WorkingDirMissing { .. }
                        | Defect::WorkingDirOutdated { .. }
                        | Defect::ValueMissing { .. }
                        | Defect::WrongValue { .. }
                        | Defect::MalformedCommand { .. }
                        | Defect::MissingImageTypes { .. }
                        | Defect::IconMissing
                        | Defect::VersionOutdated { .. } => None,
                    });
                    (foreign.unwrap_or_else(|| record.location.clone()), true)
                }
                RecordStatus::Missing | RecordStatus::OursOk | RecordStatus::OursStale(_) | RecordStatus::Unreadable(_) => (record.location.clone(), true),
            };
            self.confirm = Some(PendingConfirm { request, subject, unknown_owner });
            return None;
        }
        self.confirm = None;
        Some(request)
    }

    /// The inline confirmation row of a pending destructive action.
    fn show_confirm(&mut self, ui: &mut Ui) {
        let Some(pending) = self.confirm.clone() else {
            return;
        };
        let busy = self.busy();
        let question = match (pending.request.action, pending.unknown_owner) {
            (ActionKind::Remove, false) => tf!("launcher.sysreg.remove_confirm_label", path = pending.subject),
            (ActionKind::RePoint | ActionKind::Create | ActionKind::Repair, false) => {
                tf!("launcher.sysreg.repoint_confirm_label", path = pending.subject)
            }
            (ActionKind::Remove, true) => tf!("launcher.sysreg.unknown_remove_confirm_label", path = pending.subject),
            (ActionKind::RePoint | ActionKind::Create | ActionKind::Repair, true) => {
                tf!("launcher.sysreg.unknown_replace_confirm_label", path = pending.subject)
            }
        };
        let mut confirmed = false;
        let mut cancelled = false;
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(theme::status(&question, theme::NOTICE_TEXT));
            confirmed = theme::launcher_button_small(ui, t!("launcher.sysreg.confirm_button"), !busy).clicked();
            cancelled = theme::launcher_button_small(ui, t!("launcher.sysreg.cancel_button"), true).clicked();
        });
        if confirmed {
            self.confirm = None;
            let ctx = ui.ctx().clone();
            self.start_action(&ctx, vec![pending.request]);
        } else if cancelled {
            self.confirm = None;
        }
    }

    /// The outcome of the last action batch.
    fn show_last_outcome(&self, ui: &mut Ui, width: f32) {
        let Some(outcome) = &self.last_outcome else {
            return;
        };
        match outcome {
            LastOutcome::Finished(report) => {
                if let Some(declined) = &report.declined {
                    theme::notice_banner(ui, "launcher.sysreg.elevation_declined_banner", width, declined, None);
                }
                for failure in &report.failures {
                    let text = tf!(
                        "launcher.sysreg.action_failed_error",
                        record = registration_record(failure.request.kind).label(),
                        error = failure.message
                    );
                    ui.add(egui::Label::new(theme::status(&text, theme::STATUS_ERROR)).wrap());
                }
                if let Some(error) = &report.elevation_error {
                    ui.add(egui::Label::new(theme::status(error, theme::STATUS_ERROR)).wrap());
                }
                if report == &ActionReport::default() {
                    ui.label(theme::status(t!("launcher.sysreg.action_done_status"), theme::STATUS_SUCCESS));
                }
            }
            LastOutcome::WorkerLost => {
                ui.label(theme::status(t!("launcher.sysreg.worker_lost_error"), theme::STATUS_ERROR));
            }
            LastOutcome::NotStarted(message) => {
                ui.label(theme::status(message, theme::STATUS_ERROR));
            }
        }
        ui.add_space(8.0);
    }

    /// Starts the probe worker (`CopyIdentity::current` + `report::probe`, both blocking).
    fn start_probe(&mut self, ctx: &egui::Context) {
        self.probe_requested = false;
        let version_core = self.version_core;
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        let spawned = thread::Builder::new().name("system-registration-probe".to_owned()).spawn(move || {
            let result = match CopyIdentity::current(Some(version_core)) {
                Ok(identity) => {
                    let report = report::probe(&identity);
                    Ok(ProbeSnapshot { identity, report })
                }
                Err(error) => {
                    runtime_log::log_error(format!("[launcher-sysreg] could not identify the running copy: {error}"));
                    Err(error.user_message())
                }
            };
            if tx.send(result).is_err() {
                runtime_log::log_warn("[launcher-sysreg] probe result receiver was dropped");
            }
            repaint.request_repaint();
        });
        match spawned {
            Ok(_detached) => self.probe_rx = Some(rx),
            Err(error) => {
                runtime_log::log_error(format!("[launcher-sysreg] could not start the probe worker: {error}"));
                self.probe_error = Some(tf!("launcher.sysreg.worker_start_error", error = error));
            }
        }
    }

    /// Applies a finished probe. A failed probe drops the old snapshot, so no action is ever
    /// offered on stale data; a new snapshot drops a pending confirmation for the same reason.
    fn poll_probe(&mut self) {
        let Some(rx) = &self.probe_rx else {
            return;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                runtime_log::log_error("[launcher-sysreg] the probe worker ended without a result");
                Err(t!("launcher.sysreg.worker_lost_error").to_owned())
            }
        };
        self.probe_rx = None;
        self.confirm = None;
        match result {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.probe_error = None;
            }
            Err(message) => {
                self.snapshot = None;
                self.probe_error = Some(message);
            }
        }
    }

    /// Starts the action worker for `requests` against the last probed copy. Without a
    /// snapshot (no row, so no button) nothing starts.
    fn start_action(&mut self, ctx: &egui::Context, requests: Vec<ActionRequest>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let identity = snapshot.identity.clone();
        let running_elevated = snapshot.report.running_elevated;
        runtime_log::log_info(format!("[launcher-sysreg] applying {requests:?} for '{}'", identity.exe.display()));
        let (tx, rx) = mpsc::channel();
        let repaint = ctx.clone();
        let spawned = thread::Builder::new().name("system-registration-action".to_owned()).spawn(move || {
            let report = run_actions(&identity, &requests, running_elevated);
            if tx.send(report).is_err() {
                runtime_log::log_warn("[launcher-sysreg] action result receiver was dropped");
            }
            repaint.request_repaint();
        });
        match spawned {
            Ok(_detached) => {
                self.action_rx = Some(rx);
                self.last_outcome = None;
            }
            Err(error) => {
                runtime_log::log_error(format!("[launcher-sysreg] could not start the action worker: {error}"));
                self.last_outcome = Some(LastOutcome::NotStarted(tf!("launcher.sysreg.worker_start_error", error = error)));
            }
        }
    }

    /// Applies a finished action batch: records its outcome and requests a re-probe. Returns
    /// `true` for EVERY end of a batch (success, failure, declined prompt, lost worker), since
    /// any of them may have changed records; `false` while it runs or when none ran.
    fn poll_action(&mut self) -> bool {
        let Some(rx) = &self.action_rx else {
            return false;
        };
        let outcome = match rx.try_recv() {
            Ok(report) => LastOutcome::Finished(report),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                runtime_log::log_error("[launcher-sysreg] the action worker ended without a report");
                LastOutcome::WorkerLost
            }
        };
        self.action_rx = None;
        self.last_outcome = Some(outcome);
        self.probe_requested = true;
        true
    }
}

/// The collapsible technical details: where each record lives, whether the system ignores
/// it, its defects, and why an unreadable record could not be read.
fn show_details(ui: &mut Ui, report: &RegistrationReport) {
    egui::CollapsingHeader::new(t!("launcher.sysreg.details_title")).id_salt("launcher.sysreg.details_title").default_open(false).show(ui, |ui| {
        for record in &report.records {
            let heading = tf!(
                "launcher.sysreg.details_record_label",
                record = registration_record(record.kind).label(),
                scope = scope_label(record.scope),
                location = record.location
            );
            ui.add(egui::Label::new(theme::status(&heading, theme::TEXT_MAIN)).wrap());
            // The location is part of the salt: on Linux every `$XDG_DATA_DIRS` copy is its own
            // `Machine` row of the same kind.
            ui.indent(("launcher.sysreg.details_record", record.kind, record.scope, record.location.as_str()), |ui| {
                if record.shadowed {
                    ui.label(theme::status(t!("launcher.sysreg.shadowed_hint"), theme::TEXT_MUTED));
                }
                let defects: &[Defect] = match &record.status {
                    RecordStatus::OursStale(defects) | RecordStatus::OursBroken(defects) | RecordStatus::OtherCopy { defects, .. } => defects,
                    RecordStatus::Unreadable(error) => {
                        // `ProbeError` has no user text: the technical reason is shown as-is.
                        ui.add(egui::Label::new(theme::status(&tf!("launcher.sysreg.details_error_label", error = error), theme::TEXT_MUTED)).wrap());
                        &[]
                    }
                    RecordStatus::Missing | RecordStatus::OursOk => &[],
                };
                for defect in defects {
                    let tone = if defect.severity() == DefectSeverity::Broken { Tone::Bad } else { Tone::Notice };
                    ui.add(egui::Label::new(theme::status(&defect_label(defect), tone.color())).wrap());
                }
            });
            ui.add_space(4.0);
        }
    });
}

/// The status kind and colour role of a record status. The `Bad` role is exactly
/// `report::badge_worthy` (the one owner of "does not work").
fn status_view(status: &RecordStatus) -> (StatusKind, Tone) {
    let kind = match status {
        RecordStatus::Missing => StatusKind::Missing,
        RecordStatus::OursOk => StatusKind::OursOk,
        RecordStatus::OursStale(_) => StatusKind::OursStale,
        RecordStatus::OursBroken(_) => StatusKind::OursBroken,
        RecordStatus::OtherCopy { alive: true, .. } => StatusKind::OtherAlive,
        RecordStatus::OtherCopy { alive: false, .. } => StatusKind::OtherDead,
        RecordStatus::Unreadable(_) => StatusKind::Unreadable,
    };
    let tone = if report::badge_worthy(status) {
        Tone::Bad
    } else {
        match kind {
            StatusKind::Missing => Tone::Muted,
            StatusKind::OursOk => Tone::Good,
            StatusKind::OursStale | StatusKind::OtherAlive | StatusKind::Unreadable => Tone::Notice,
            // Always badge-worthy, handled above; kept explicit so a rule change shows here.
            StatusKind::OursBroken | StatusKind::OtherDead => Tone::Bad,
        }
    };
    (kind, tone)
}

/// The localized status text of `status` (whose kind is `kind`).
fn status_text(kind: StatusKind, status: &RecordStatus) -> String {
    let other_exe = match status {
        RecordStatus::OtherCopy { exe, .. } => exe.display().to_string(),
        RecordStatus::Missing | RecordStatus::OursOk | RecordStatus::OursStale(_) | RecordStatus::OursBroken(_) | RecordStatus::Unreadable(_) => {
            String::new()
        }
    };
    match kind {
        StatusKind::Missing => t!("launcher.sysreg.missing_status").to_owned(),
        StatusKind::OursOk => t!("launcher.sysreg.ours_ok_status").to_owned(),
        StatusKind::OursStale => t!("launcher.sysreg.ours_stale_status").to_owned(),
        StatusKind::OursBroken => tf!("launcher.sysreg.ours_broken_status", detail = broken_summary(status)),
        StatusKind::OtherAlive => tf!("launcher.sysreg.other_alive_status", path = other_exe),
        StatusKind::OtherDead => tf!("launcher.sysreg.other_dead_status", path = other_exe),
        StatusKind::Unreadable => tf!("launcher.sysreg.unreadable_status", section = t!("launcher.sysreg.details_title")),
    }
}

/// The broken defects of `status`, localized and joined (the full list is in the details).
fn broken_summary(status: &RecordStatus) -> String {
    let defects: &[Defect] = match status {
        RecordStatus::OursBroken(defects) | RecordStatus::OursStale(defects) | RecordStatus::OtherCopy { defects, .. } => defects,
        RecordStatus::Missing | RecordStatus::OursOk | RecordStatus::Unreadable(_) => &[],
    };
    defects.iter().filter(|defect| defect.severity() == DefectSeverity::Broken).map(defect_label).collect::<Vec<_>>().join("; ")
}

/// The localized description of one defect. The broken ones share their keys with the
/// registration warning tooltip (`settings_warnings::RegistrationProblem`).
fn defect_label(defect: &Defect) -> String {
    match defect {
        Defect::TargetMissing { path } => tf!("launcher.sysreg.defect.target_missing_label", path = path),
        Defect::WorkingDirMissing { path } => tf!("launcher.sysreg.defect.workdir_missing_label", path = path),
        Defect::WorkingDirOutdated { expected, found } => {
            tf!("launcher.sysreg.defect.workdir_outdated_label", expected = expected, found = found)
        }
        Defect::ValueMissing { value } => tf!("launcher.sysreg.defect.value_missing_label", name = value.name()),
        Defect::WrongValue { value, expected, found } => {
            tf!("launcher.sysreg.defect.wrong_value_label", name = value.name(), expected = expected, found = found)
        }
        Defect::MalformedCommand { command } => tf!("launcher.sysreg.defect.bad_command_label", command = command),
        // The same text as the warning tooltip, which maps it to a wrong value.
        Defect::ForeignProgram { value, expected, found } => {
            tf!("launcher.sysreg.defect.wrong_value_label", name = value.name(), expected = expected, found = found)
        }
        Defect::MissingImageTypes { types } => tf!("launcher.sysreg.defect.missing_types_label", types = types.join(", ")),
        Defect::IconMissing => t!("launcher.sysreg.defect.icon_missing_label").to_owned(),
        Defect::VersionOutdated { found } => tf!("launcher.sysreg.defect.version_outdated_label", found = found),
    }
}

/// The localized scope name.
fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::User => t!("launcher.sysreg.scope_user_label"),
        Scope::Machine => t!("launcher.sysreg.scope_machine_label"),
    }
}

/// The localized caption of an action button.
fn action_label(action: ActionKind) -> &'static str {
    match action {
        ActionKind::Create => t!("launcher.sysreg.create_button"),
        ActionKind::Repair => t!("launcher.sysreg.repair_button"),
        ActionKind::Remove => t!("launcher.sysreg.remove_button"),
        ActionKind::RePoint => t!("launcher.sysreg.repoint_button"),
    }
}

/// The buttons of one record row: `actions::allowed_actions` in its order, each with
/// `actions::needs_confirmation` and `actions::requires_elevation`.
fn row_buttons(record: &RecordReport, report: &RegistrationReport, read_only: bool) -> Vec<ActionButton> {
    let elevated = actions::requires_elevation(record.scope, report.running_elevated);
    actions::allowed_actions(record, report.copy_scope, read_only)
        .into_iter()
        .map(|action| ActionButton { action, confirm: actions::needs_confirmation(record, action), elevated })
        .collect()
}

/// Splits a batch into the requests this process runs itself and those the elevated helper
/// must run (`actions::requires_elevation`), each in request order.
fn split_by_elevation(requests: &[ActionRequest], running_elevated: bool) -> (Vec<ActionRequest>, Vec<ActionRequest>) {
    requests.iter().copied().partition(|request| !actions::requires_elevation(request.scope, running_elevated))
}

/// The action worker's body: runs the in-process part with `actions::apply`, then the
/// elevated part through the helper, and collects every failure. Blocking.
fn run_actions(identity: &CopyIdentity, requests: &[ActionRequest], running_elevated: bool) -> ActionReport {
    let (in_process, elevated) = split_by_elevation(requests, running_elevated);
    let mut report = ActionReport::default();
    if !in_process.is_empty() {
        // `apply` logs every outcome itself.
        for (request, outcome) in actions::apply(identity, &in_process) {
            if let Err(error) = outcome {
                report.failures.push(FailedAction { request, message: error.user_message() });
            }
        }
    }
    if !elevated.is_empty() {
        run_elevated(&elevated, &mut report);
    }
    report
}

/// Runs `requests` in the elevated helper and records the outcome. The helper keeps no log
/// file of its own (it must not rotate the session log of this process), so every failure
/// it reports is logged here with its technical detail.
#[cfg(target_os = "windows")]
fn run_elevated(requests: &[ActionRequest], report: &mut ActionReport) {
    use ms_os_integration::actions::ElevationError;

    match ms_os_integration::windows::elevation::apply_elevated(requests, ELEVATED_HELPER_TIMEOUT) {
        Ok(outcomes) => {
            for (request, outcome) in outcomes {
                match outcome {
                    Ok(()) => runtime_log::log_info(format!("[launcher-sysreg] elevated {request:?} done")),
                    Err(failure) => {
                        runtime_log::log_error(format!(
                            "[launcher-sysreg] elevated {request:?} failed: code={:?} os={:?} detail={}",
                            failure.code, failure.os, failure.detail
                        ));
                        report.failures.push(FailedAction { request, message: failure.message });
                    }
                }
            }
        }
        Err(ElevationError::Declined) => {
            runtime_log::log_info(format!("[launcher-sysreg] administrator prompt declined for {requests:?}"));
            report.declined = Some(ElevationError::Declined.user_message());
        }
        Err(error) => {
            runtime_log::log_error(format!("[launcher-sysreg] elevated batch {requests:?} failed: {error}"));
            report.elevation_error = Some(error.user_message());
        }
    }
}

/// Outside Windows no record requires elevation (`actions::requires_elevation`), so nothing
/// reaches here; a request that does is refused as unsupported instead of being run.
#[cfg(not(target_os = "windows"))]
fn run_elevated(requests: &[ActionRequest], report: &mut ActionReport) {
    for &request in requests {
        let error = actions::ActionError::Unsupported { request };
        runtime_log::log_error(format!("[launcher-sysreg] {request:?} needs elevation, which this system does not have: {error}"));
        report.failures.push(FailedAction { request, message: error.user_message() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_os_integration::report::{ProbeError, RecordKind, RecordValue};
    use std::path::PathBuf;

    fn record(kind: RecordKind, scope: Scope, status: RecordStatus) -> RecordReport {
        RecordReport { kind, scope, location: "/home/u/.local/share/applications/x.desktop".to_owned(), status, shadowed: false }
    }

    fn report_of(records: Vec<RecordReport>, running_elevated: bool) -> RegistrationReport {
        RegistrationReport { records, copy_scope: Scope::User, running_elevated, dev_copy: false }
    }

    fn other(alive: bool, defects: Vec<Defect>) -> RecordStatus {
        RecordStatus::OtherCopy { exe: PathBuf::from("/home/u/other/manhwastudio_rs"), alive, defects }
    }

    /// Every status maps to its own kind, and the `Bad` colour is exactly the badge rule.
    #[test]
    fn status_maps_to_kind_and_tone() {
        let broken = Defect::TargetMissing { path: "/home/u/gone".to_owned() };
        let cases = [
            (RecordStatus::Missing, StatusKind::Missing, Tone::Muted),
            (RecordStatus::OursOk, StatusKind::OursOk, Tone::Good),
            (RecordStatus::OursStale(vec![Defect::IconMissing]), StatusKind::OursStale, Tone::Notice),
            (RecordStatus::OursBroken(vec![broken.clone()]), StatusKind::OursBroken, Tone::Bad),
            (other(true, Vec::new()), StatusKind::OtherAlive, Tone::Notice),
            (other(true, vec![broken.clone()]), StatusKind::OtherAlive, Tone::Bad),
            (other(false, Vec::new()), StatusKind::OtherDead, Tone::Bad),
            (RecordStatus::Unreadable(ProbeError::NoDataHome), StatusKind::Unreadable, Tone::Notice),
        ];
        for (status, kind, tone) in cases {
            assert_eq!(status_view(&status), (kind, tone), "{status:?}");
            assert_eq!(tone == Tone::Bad, report::badge_worthy(&status), "{status:?}");
        }
        assert_eq!(Tone::Bad.color(), theme::STATUS_ERROR);
        assert_eq!(Tone::Good.color(), theme::STATUS_SUCCESS);
        assert_eq!(Tone::Notice.color(), theme::NOTICE_TEXT);
        assert_eq!(Tone::Muted.color(), theme::TEXT_MUTED);
    }

    /// The buttons are `allowed_actions` in order, confirmation only for a live other copy,
    /// and nothing at all in read-only mode.
    #[test]
    fn buttons_follow_allowed_actions() {
        let missing = record(RecordKind::StartMenu, Scope::User, RecordStatus::Missing);
        let report = report_of(vec![missing.clone()], false);
        let buttons = row_buttons(&missing, &report, false);
        assert_eq!(buttons, vec![ActionButton { action: ActionKind::Create, confirm: false, elevated: false }]);
        assert!(row_buttons(&missing, &report, true).is_empty());

        let broken = record(RecordKind::StartMenu, Scope::User, RecordStatus::OursBroken(vec![Defect::ValueMissing { value: RecordValue::DesktopExec }]));
        let actions_of = |buttons: Vec<ActionButton>| buttons.into_iter().map(|button| (button.action, button.confirm)).collect::<Vec<_>>();
        assert_eq!(actions_of(row_buttons(&broken, &report, false)), vec![(ActionKind::Repair, false), (ActionKind::Remove, false)]);

        let alive = record(RecordKind::StartMenu, Scope::User, other(true, Vec::new()));
        assert_eq!(actions_of(row_buttons(&alive, &report, false)), vec![(ActionKind::RePoint, true), (ActionKind::Remove, true)]);
        let dead = record(RecordKind::StartMenu, Scope::User, other(false, Vec::new()));
        assert_eq!(actions_of(row_buttons(&dead, &report, false)), vec![(ActionKind::RePoint, false), (ActionKind::Remove, false)]);

        let unreadable = record(RecordKind::StartMenu, Scope::User, RecordStatus::Unreadable(ProbeError::NoDataHome));
        assert!(row_buttons(&unreadable, &report, false).is_empty());
        // An unreadable record that exists can be overwritten, after a confirmation.
        let garbage = record(RecordKind::StartMenu, Scope::User, RecordStatus::Unreadable(ProbeError::NoOwner { location: "l".to_owned() }));
        assert_eq!(actions_of(row_buttons(&garbage, &report, false)), vec![(ActionKind::Repair, true)]);
    }

    /// Overwriting a record whose owner is unknown names it as such: the foreign program, or the
    /// unreadable record's location.
    #[test]
    fn unknown_owner_confirmations_name_the_record() {
        let mut state = SystemRegistrationState::new("3.0.0", false);
        let foreign = Defect::ForeignProgram { value: RecordValue::ShortcutTarget, expected: "/opt/ms/manhwastudio_rs".to_owned(), found: "/usr/bin/gimp".to_owned() };
        let foreign = record(RecordKind::StartMenu, Scope::User, RecordStatus::OursBroken(vec![foreign]));
        let repair = ActionButton { action: ActionKind::Repair, confirm: true, elevated: false };
        assert_eq!(state.on_action_clicked(&foreign, repair), None);
        let pending = state.confirm.clone().expect("pending");
        assert_eq!((pending.subject.as_str(), pending.unknown_owner), ("/usr/bin/gimp", true));

        let garbage = record(RecordKind::OpenWith, Scope::User, RecordStatus::Unreadable(ProbeError::NoOwner { location: "l".to_owned() }));
        assert_eq!(state.on_action_clicked(&garbage, repair), None);
        let pending = state.confirm.clone().expect("pending");
        assert_eq!((pending.subject.as_str(), pending.unknown_owner), ("/home/u/.local/share/applications/x.desktop", true));
    }

    /// A confirmed action waits for the inline confirmation; any other runs at once.
    #[test]
    fn confirmed_actions_wait_for_the_inline_confirmation() {
        let mut state = SystemRegistrationState::new("3.0.0", false);
        let alive = record(RecordKind::StartMenu, Scope::User, other(true, Vec::new()));
        let button = ActionButton { action: ActionKind::RePoint, confirm: true, elevated: false };
        assert_eq!(state.on_action_clicked(&alive, button), None);
        let pending = state.confirm.clone().expect("a confirmed action is pending");
        assert_eq!(pending.request, ActionRequest { kind: RecordKind::StartMenu, scope: Scope::User, action: ActionKind::RePoint });
        assert_eq!(pending.subject, PathBuf::from("/home/u/other/manhwastudio_rs").display().to_string());
        assert!(!pending.unknown_owner);

        let missing = record(RecordKind::OpenWith, Scope::User, RecordStatus::Missing);
        let create = ActionButton { action: ActionKind::Create, confirm: false, elevated: false };
        assert_eq!(state.on_action_clicked(&missing, create), Some(ActionRequest { kind: RecordKind::OpenWith, scope: Scope::User, action: ActionKind::Create }));
        assert_eq!(state.confirm, None, "a direct action drops a stale confirmation");
    }

    /// Every end of an action batch asks for the warnings recheck and a re-probe: success,
    /// failure, declined prompt, helper failure and a lost worker alike.
    #[test]
    fn every_action_outcome_requests_a_recheck() {
        let request = ActionRequest { kind: RecordKind::StartMenu, scope: Scope::User, action: ActionKind::Create };
        let reports = [
            ActionReport::default(),
            ActionReport { failures: vec![FailedAction { request, message: "denied".to_owned() }], ..ActionReport::default() },
            ActionReport { declined: Some("declined".to_owned()), ..ActionReport::default() },
            ActionReport { elevation_error: Some("timed out".to_owned()), ..ActionReport::default() },
        ];
        for report in reports {
            let mut state = SystemRegistrationState::new("3.0.0", false);
            state.probe_requested = false;
            let (tx, rx) = mpsc::channel();
            state.action_rx = Some(rx);
            assert!(!state.poll_action(), "a running batch is not an outcome");
            tx.send(report.clone()).expect("receiver alive");
            assert!(state.poll_action(), "{report:?} must recheck");
            assert!(state.probe_requested && state.action_rx.is_none());
            assert_eq!(state.last_outcome, Some(LastOutcome::Finished(report)));
        }

        let mut state = SystemRegistrationState::new("3.0.0", false);
        state.probe_requested = false;
        let (tx, rx) = mpsc::channel::<ActionReport>();
        state.action_rx = Some(rx);
        drop(tx);
        assert!(state.poll_action(), "a lost worker must recheck");
        assert!(state.probe_requested);
        assert_eq!(state.last_outcome, Some(LastOutcome::WorkerLost));

        let mut idle = SystemRegistrationState::new("3.0.0", false);
        assert!(!idle.poll_action(), "no batch, no recheck");
    }

    /// A failed probe drops the old snapshot (no action on stale data) and any confirmation.
    #[test]
    fn failed_probe_drops_the_snapshot() {
        let mut state = SystemRegistrationState::new("3.0.0", false);
        state.snapshot = Some(ProbeSnapshot {
            identity: CopyIdentity { exe: PathBuf::from("/home/u/ms/manhwastudio_rs"), program_root: PathBuf::from("/home/u/ms"), version_core: None },
            report: report_of(Vec::new(), false),
        });
        state.confirm = Some(PendingConfirm {
            request: ActionRequest { kind: RecordKind::StartMenu, scope: Scope::User, action: ActionKind::Remove },
            subject: "/home/u/x".to_owned(),
            unknown_owner: false,
        });
        let (tx, rx) = mpsc::channel();
        state.probe_rx = Some(rx);
        tx.send(Err("no exe".to_owned())).expect("receiver alive");
        state.poll_probe();
        assert!(state.snapshot.is_none() && state.confirm.is_none() && state.probe_rx.is_none());
        assert_eq!(state.probe_error.as_deref(), Some("no exe"));
    }

    /// User-scope requests always run in-process; all-users ones go to the helper only on
    /// Windows when not elevated (Linux: never).
    #[test]
    fn mixed_batches_split_by_elevation() {
        let user = ActionRequest { kind: RecordKind::OpenWith, scope: Scope::User, action: ActionKind::Repair };
        let machine = ActionRequest { kind: RecordKind::OpenWith, scope: Scope::Machine, action: ActionKind::Repair };
        let (in_process, elevated) = split_by_elevation(&[machine, user], false);
        if cfg!(target_os = "windows") {
            assert_eq!((in_process, elevated), (vec![user], vec![machine]));
        } else {
            assert_eq!((in_process, elevated), (vec![machine, user], Vec::new()));
        }
        assert_eq!(split_by_elevation(&[machine, user], true), (vec![machine, user], Vec::new()));
    }

    /// On Windows an all-users row of an unelevated process carries the administrator hint.
    #[cfg(target_os = "windows")]
    #[test]
    fn machine_rows_need_elevation_unless_elevated() {
        let missing = record(RecordKind::ProgramEntry, Scope::Machine, RecordStatus::Missing);
        let mut report = report_of(vec![missing.clone()], false);
        report.copy_scope = Scope::Machine;
        assert!(row_buttons(&missing, &report, false).iter().all(|button| button.elevated));
        report.running_elevated = true;
        assert!(row_buttons(&missing, &report, false).iter().all(|button| !button.elevated));
    }
}
