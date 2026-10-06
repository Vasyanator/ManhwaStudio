/*
File: crates/ms-os-integration/src/actions.rs

Purpose:
What the user may do with a probed OS record and how it is done: the action matrix
(`allowed_actions`), the elevation rule (`requires_elevation`), the in-process executor
(`apply`, ONE desktop refresh per batch), and the protocol of the elevated helper process (the
action tokens on its command line and the JSON result file it writes back).

Key items:
- `ActionKind`, `ActionRequest`: one action on one record (kind + scope).
- `allowed_actions()`, `needs_confirmation()`, `requires_elevation()`: pure rules.
- `apply()` (Windows and Linux): the writes and deletes, through the record writers of
  `windows::{registry, shortcut, values}` and `linux::xdg`; never a second copy of a value table.
- `encode_actions()` / `decode_actions()`: the helper's `--system-registration-apply` tokens
  (`<kind>:<scope>:<action>`, comma-separated), `Machine` scope only.
- `ActionError` (in-process failure), `ActionFailure` (its serializable form, as the helper
  reports it), `ElevationError` (the helper round trip itself failed).
- Helper result file: `helper_result_json()`, `parse_helper_result()`, `create_result_file()`,
  `write_helper_result()`, `read_helper_result()`, `validate_result_path()`, `new_result_path()`.
- `run_helper_protocol()`: the helper's ordering (validate, decode, create the result file,
  THEN run the actions, write into the open file).

Notes:
The pure model, the codec and the result-file protocol are compiled into native host test
builds on every host; `apply` exists on Windows and Linux only. The elevated launch and the
helper entry point live in `windows/elevation.rs`. The helper accepts `Machine`-scope actions
only: an over-the-shoulder UAC prompt runs it as ANOTHER user, whose `HKCU` / `%APPDATA%` are not
the requesting user's, so `User`-scope actions always run in-process, unelevated.
*/

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(any(target_os = "windows", target_os = "linux"))]
use crate::copy_identity::CopyIdentity;
use crate::report::{RecordKind, RecordReport, RecordStatus, Scope};
use crate::IntegrationError;

/// Command-line flag carrying the helper's action tokens ([`encode_actions`]). The binary's
/// clap field spells the same flag; its test pins the two together.
pub const SYSTEM_REGISTRATION_APPLY_FLAG: &str = "--system-registration-apply";

/// Command-line flag carrying the helper's result file path ([`new_result_path`]).
pub const SYSTEM_REGISTRATION_RESULT_FLAG: &str = "--system-registration-result";

/// File-name prefix of every helper result file; the helper refuses any other name, so an
/// elevated process can never be pointed at an arbitrary file.
pub const HELPER_RESULT_FILE_PREFIX: &str = "manhwastudio-sysreg-";

/// File-name suffix of every helper result file.
const HELPER_RESULT_FILE_SUFFIX: &str = ".json";

/// Version of the result-file JSON; a file with another version is unreadable.
const HELPER_RESULT_VERSION: u32 = 1;

/// Helper exit code: every action succeeded; the result file was written (or, rarely, its write
/// failed after the actions ran — the parent then finds it unreadable).
pub const HELPER_EXIT_ALL_OK: i32 = 0;
/// Helper exit code: at least one action failed (also: not elevated); the result file was
/// written (same rare exception as [`HELPER_EXIT_ALL_OK`]).
pub const HELPER_EXIT_SOME_FAILED: i32 = 1;
/// Helper exit code: unusable command line or result path, or the result file could not be
/// created; NO action ran and there is no result file.
pub const HELPER_EXIT_USAGE: i32 = 2;

/// What to do with a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// Write the record for this copy where none exists.
    Create,
    /// Rewrite this copy's record with the full expected value set.
    Repair,
    /// Delete the record (this copy's, or another copy's after confirmation).
    Remove,
    /// Overwrite another copy's record so it starts this copy.
    RePoint,
}

/// One action on one record of the running copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionRequest {
    pub kind: RecordKind,
    pub scope: Scope,
    pub action: ActionKind,
}

/// Whether the running platform uses the Linux record layout (one per-user `.desktop` file
/// carries both the menu and the "Open with" record; system-wide rows are read-only) rather than
/// the Windows one. The pure rules take it as a parameter, so both tables are tested on every host.
#[cfg(any(target_os = "windows", target_os = "linux"))]
const LINUX_LAYOUT: bool = cfg!(target_os = "linux");

/// The actions offered for `record` of the running copy, whose own records belong in
/// `copy_scope`; `read_only` (`--ignore-installed`) offers none.
///
/// Missing -> `Create`, only in `copy_scope`. This copy's record -> `Remove` (+ `Repair` when it
/// has defects). Another copy's record -> `RePoint` (only in `copy_scope` or the user scope, so
/// an all-users record is never pointed at one user's copy) and `Remove`; both need confirmation
/// while that copy exists ([`needs_confirmation`]). Unreadable records and Linux system-wide
/// (`Machine`) rows -> none. On Linux the "Open with" row shares the menu row's file, so it never
/// offers `Remove` (removing the menu entry removes both).
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[must_use]
pub fn allowed_actions(record: &RecordReport, copy_scope: Scope, read_only: bool) -> Vec<ActionKind> {
    allowed_actions_on(LINUX_LAYOUT, record, copy_scope, read_only)
}

/// [`allowed_actions`] for an explicit layout (`linux` = the Linux record layout).
fn allowed_actions_on(linux: bool, record: &RecordReport, copy_scope: Scope, read_only: bool) -> Vec<ActionKind> {
    if read_only || (linux && record.scope == Scope::Machine) {
        return Vec::new();
    }
    let mut actions = match &record.status {
        RecordStatus::Missing if record.scope == copy_scope => vec![ActionKind::Create],
        RecordStatus::Missing | RecordStatus::Unreadable(_) => Vec::new(),
        RecordStatus::OursOk => vec![ActionKind::Remove],
        RecordStatus::OursStale(_) | RecordStatus::OursBroken(_) => vec![ActionKind::Repair, ActionKind::Remove],
        RecordStatus::OtherCopy { .. } if record.scope == copy_scope || record.scope == Scope::User => {
            vec![ActionKind::RePoint, ActionKind::Remove]
        }
        RecordStatus::OtherCopy { .. } => vec![ActionKind::Remove],
    };
    if linux && record.kind == RecordKind::OpenWith {
        actions.retain(|action| *action != ActionKind::Remove);
    }
    actions
}

/// True when `action` on `record` replaces or deletes a record of another copy that still
/// exists: the UI asks for an inline confirmation first.
#[must_use]
pub fn needs_confirmation(record: &RecordReport, action: ActionKind) -> bool {
    matches!(record.status, RecordStatus::OtherCopy { alive: true, .. }) && matches!(action, ActionKind::RePoint | ActionKind::Remove)
}

/// True when an action on a record in `scope` must run in the elevated helper: on Windows a
/// `Machine` record of a process that is not elevated; on Linux never (system-wide rows have no
/// actions).
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[must_use]
pub fn requires_elevation(scope: Scope, running_elevated: bool) -> bool {
    requires_elevation_on(LINUX_LAYOUT, scope, running_elevated)
}

/// [`requires_elevation`] for an explicit layout (`linux` = the Linux record layout).
fn requires_elevation_on(linux: bool, scope: Scope, running_elevated: bool) -> bool {
    !linux && scope == Scope::Machine && !running_elevated
}

/// Each requested action with its in-process outcome, in request order ([`apply`]).
pub type ActionOutcomes = Vec<(ActionRequest, Result<(), ActionError>)>;

/// Each requested action with the outcome the elevated helper reported, in request order.
pub type HelperOutcomes = Vec<(ActionRequest, Result<(), ActionFailure>)>;

/// Why an action failed in-process (or was refused by the helper). `Display` is English, for
/// logs; [`ActionError::user_message`] is the user-facing text.
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    /// A record writer or remover failed.
    #[error(transparent)]
    Integration(#[from] IntegrationError),
    /// Linux: a directory of the desktop entry or icon could not be created.
    #[error("could not create the directory '{}': {source}", path.display())]
    CreateDir { path: PathBuf, source: io::Error },
    /// Linux: the desktop entry or icon could not be written.
    #[error("could not write '{}': {source}", path.display())]
    WriteFile { path: PathBuf, source: io::Error },
    /// Linux: neither `$XDG_DATA_HOME` nor `$HOME` names an absolute directory.
    #[error("no per-user data directory ($XDG_DATA_HOME / $HOME unset or relative)")]
    NoDataHome,
    /// The helper runs without elevation, so it changes nothing (its loop guard: it never
    /// elevates itself).
    #[error("the system-registration helper is not elevated")]
    NotElevated,
    /// The record kind / scope does not exist or is not writable on this platform.
    #[error("action {} is not supported on this system", request_token(.request))]
    Unsupported { request: ActionRequest },
    /// The Uninstall entry needs `DisplayVersion`, but the copy identity carries no version.
    #[error("the program version is unknown; the Uninstall entry cannot be written")]
    MissingVersion,
    /// An action batch with no action.
    #[error("the action list is empty")]
    EmptyBatch,
    /// An action token that is not `<kind>:<scope>:<action>`.
    #[error("malformed action token '{token}'")]
    MalformedToken { token: String },
    /// A `User`-scope action given to (or meant for) the elevated helper.
    #[error("user-scope action '{token}' cannot run elevated")]
    UserScopeNotAllowed { token: String },
}

impl ActionError {
    /// Localized, user-facing text of this error in the active UI locale.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Integration(error) => error.user_message(),
            Self::CreateDir { path, source } => tf!("os_integration.action.create_dir_error", path = path.display(), e = source),
            Self::WriteFile { path, source } => tf!("os_integration.action.write_file_error", path = path.display(), e = source),
            Self::NoDataHome => t!("os_integration.action.no_data_home_error").to_owned(),
            Self::NotElevated => t!("os_integration.action.not_elevated_error").to_owned(),
            Self::Unsupported { .. } => t!("os_integration.action.unsupported_error").to_owned(),
            Self::MissingVersion => t!("os_integration.action.missing_version_error").to_owned(),
            Self::EmptyBatch | Self::MalformedToken { .. } | Self::UserScopeNotAllowed { .. } => {
                tf!("os_integration.action.invalid_request_error", detail = self)
            }
        }
    }

    /// The serializable form of this error, as the helper reports it: a stable code, the OS
    /// error number when there is one, the user text and the log text.
    #[must_use]
    pub fn failure(&self) -> ActionFailure {
        let os = self.os_code();
        let code = match self {
            Self::NotElevated => FailureCode::NotElevated,
            Self::Unsupported { .. } => FailureCode::Unsupported,
            Self::Integration(_)
            | Self::CreateDir { .. }
            | Self::WriteFile { .. }
            | Self::NoDataHome
            | Self::MissingVersion
            | Self::EmptyBatch
            | Self::MalformedToken { .. }
            | Self::UserScopeNotAllowed { .. } => {
                if self.is_access_denied() { FailureCode::AccessDenied } else { FailureCode::Failed }
            }
        };
        ActionFailure { code, os, message: self.user_message(), detail: self.to_string() }
    }

    /// The OS error number carried by this error: a Win32 code, an `HRESULT` (shortcuts), or an
    /// `errno` / Win32 code of an I/O error. `None` when there is none.
    fn os_code(&self) -> Option<i64> {
        match self {
            Self::Integration(error) => integration_os_code(error),
            Self::CreateDir { source, .. } | Self::WriteFile { source, .. } => source.raw_os_error().map(i64::from),
            Self::NoDataHome
            | Self::NotElevated
            | Self::Unsupported { .. }
            | Self::MissingVersion
            | Self::EmptyBatch
            | Self::MalformedToken { .. }
            | Self::UserScopeNotAllowed { .. } => None,
        }
    }

    /// True when the OS refused the write for lack of rights (Win32 5, `E_ACCESSDENIED`,
    /// `PermissionDenied`).
    fn is_access_denied(&self) -> bool {
        match self {
            Self::Integration(error) => integration_access_denied(error),
            Self::CreateDir { source, .. } | Self::WriteFile { source, .. } => source.kind() == io::ErrorKind::PermissionDenied,
            Self::NoDataHome
            | Self::NotElevated
            | Self::Unsupported { .. }
            | Self::MissingVersion
            | Self::EmptyBatch
            | Self::MalformedToken { .. }
            | Self::UserScopeNotAllowed { .. } => false,
        }
    }
}

/// Win32 `ERROR_ACCESS_DENIED`.
const WIN32_ACCESS_DENIED: u32 = 5;

/// `E_ACCESSDENIED` (`0x80070005`) as the signed `HRESULT` the shortcut errors carry.
const HRESULT_ACCESS_DENIED: i32 = i32::from_ne_bytes(0x8007_0005_u32.to_ne_bytes());

/// [`ActionError::os_code`] of an [`IntegrationError`]; `Multiple` reports its first child.
fn integration_os_code(error: &IntegrationError) -> Option<i64> {
    match error {
        IntegrationError::RegistryOpenKey { code, .. } | IntegrationError::RegistrySetValue { code, .. } | IntegrationError::RegistryDeleteKey { code, .. } => {
            Some(i64::from(*code))
        }
        IntegrationError::ShortcutWrite { code, .. } | IntegrationError::ShortcutRead { code, .. } => Some(i64::from(*code)),
        IntegrationError::DetermineExe { source }
        | IntegrationError::CreateShortcutFolder { source, .. }
        | IntegrationError::RemoveFolder { source, .. }
        | IntegrationError::RemoveFile { source, .. } => source.raw_os_error().map(i64::from),
        IntegrationError::Multiple(errors) => errors.first().and_then(integration_os_code),
        IntegrationError::UacDenied | IntegrationError::DesktopNotFound | IntegrationError::StartMenuFolderNotFound | IntegrationError::LauncherExeNotFound { .. } => None,
    }
}

/// [`ActionError::is_access_denied`] of an [`IntegrationError`]; `Multiple` when any child is.
fn integration_access_denied(error: &IntegrationError) -> bool {
    match error {
        IntegrationError::RegistryOpenKey { code, .. } | IntegrationError::RegistrySetValue { code, .. } | IntegrationError::RegistryDeleteKey { code, .. } => {
            *code == WIN32_ACCESS_DENIED
        }
        IntegrationError::ShortcutWrite { code, .. } | IntegrationError::ShortcutRead { code, .. } => *code == HRESULT_ACCESS_DENIED,
        IntegrationError::DetermineExe { source }
        | IntegrationError::CreateShortcutFolder { source, .. }
        | IntegrationError::RemoveFolder { source, .. }
        | IntegrationError::RemoveFile { source, .. } => source.kind() == io::ErrorKind::PermissionDenied,
        IntegrationError::Multiple(errors) => errors.iter().any(integration_access_denied),
        IntegrationError::UacDenied | IntegrationError::DesktopNotFound | IntegrationError::StartMenuFolderNotFound | IntegrationError::LauncherExeNotFound { .. } => false,
    }
}

/// Stable category of an [`ActionFailure`] (snake_case on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// The helper was not elevated and changed nothing.
    NotElevated,
    /// The action does not exist on this platform.
    Unsupported,
    /// The OS refused the write for lack of rights.
    AccessDenied,
    /// Any other failure.
    Failed,
    /// The helper's result named no outcome for this action (it may not have run).
    NotRun,
}

/// A failed action as the elevated helper reports it in its result file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionFailure {
    pub code: FailureCode,
    /// OS error number (Win32 code, `HRESULT` or `errno`), when there is one.
    pub os: Option<i64>,
    /// Localized user-facing text, rendered by the helper in the UI locale of the same user
    /// config (or by the caller for [`FailureCode::NotRun`]).
    pub message: String,
    /// English log text.
    pub detail: String,
}

impl ActionFailure {
    /// The failure of a requested action the helper's result does not mention.
    fn not_run(request: ActionRequest) -> Self {
        Self {
            code: FailureCode::NotRun,
            os: None,
            message: t!("os_integration.action.not_run_error").to_owned(),
            detail: format!("the helper reported no outcome for {}", request_token(&request)),
        }
    }
}

/// Why the elevated round trip itself failed (as opposed to one action inside it). `Display` is
/// English, for logs; [`ElevationError::user_message`] is the user-facing text.
#[derive(Debug, thiserror::Error)]
pub enum ElevationError {
    /// The user declined the UAC prompt; nothing was changed.
    #[error("the administrator prompt was declined")]
    Declined,
    /// The batch cannot be handed to the helper (empty, or a `User`-scope action).
    #[error("invalid helper batch: {0}")]
    InvalidBatch(ActionError),
    /// The helper could not be started (Win32 `code`).
    #[error("the elevated helper could not be started (Windows error {code})")]
    LaunchFailed { code: u32 },
    /// Waiting for the helper or reading its exit code failed (Win32 `code`).
    #[error("waiting for the elevated helper failed (Windows error {code})")]
    WaitFailed { code: u32 },
    /// The helper did not finish within the timeout; its outcome is unknown.
    #[error("the elevated helper did not finish in time")]
    TimedOut,
    /// The helper ended with an exit code that means "no result file".
    #[error("the elevated helper failed with exit code {exit_code}")]
    HelperFailed { exit_code: u32 },
    /// The result file is missing, unreadable or not a valid result.
    #[error("the elevated helper's result is unreadable: {0}")]
    ResultUnreadable(String),
}

impl ElevationError {
    /// Localized, user-facing text of this error in the active UI locale.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Declined => t!("os_integration.elevation.declined_status").to_owned(),
            Self::InvalidBatch(error) => error.user_message(),
            Self::LaunchFailed { code } => tf!("os_integration.elevation.launch_failed_error", e = win32_error_text(*code)),
            Self::WaitFailed { code } => tf!("os_integration.elevation.wait_failed_error", e = win32_error_text(*code)),
            Self::TimedOut => t!("os_integration.elevation.timed_out_error").to_owned(),
            Self::HelperFailed { exit_code } => tf!("os_integration.elevation.helper_failed_error", code = exit_code),
            Self::ResultUnreadable(detail) => tf!("os_integration.elevation.result_unreadable_error", detail = detail),
        }
    }
}

/// System text of a Win32 error code through `std::io::Error` (bit-exact reinterpretation of the
/// unsigned code).
fn win32_error_text(code: u32) -> String {
    io::Error::from_raw_os_error(i32::from_ne_bytes(code.to_ne_bytes())).to_string()
}

impl fmt::Display for ActionKind {
    /// The action's token (`create`, `repair`, `remove`, `repoint`), for logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(action_token(*self))
    }
}

fn kind_token(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::StartMenu => "start-menu",
        RecordKind::ProgramEntry => "program-entry",
        RecordKind::AppPaths => "app-paths",
        RecordKind::OpenWith => "open-with",
    }
}

fn scope_token(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "user",
        Scope::Machine => "machine",
    }
}

fn action_token(action: ActionKind) -> &'static str {
    match action {
        ActionKind::Create => "create",
        ActionKind::Repair => "repair",
        ActionKind::Remove => "remove",
        ActionKind::RePoint => "repoint",
    }
}

/// `<kind>:<scope>:<action>`, e.g. `program-entry:machine:repair`.
fn request_token(request: &ActionRequest) -> String {
    format!("{}:{}:{}", kind_token(request.kind), scope_token(request.scope), action_token(request.action))
}

/// Inverse of [`request_token`]; `None` for anything else.
fn parse_request_token(token: &str) -> Option<ActionRequest> {
    let mut parts = token.split(':');
    let (Some(kind), Some(scope), Some(action), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    let kind = [RecordKind::StartMenu, RecordKind::ProgramEntry, RecordKind::AppPaths, RecordKind::OpenWith].into_iter().find(|k| kind_token(*k) == kind)?;
    let scope = [Scope::User, Scope::Machine].into_iter().find(|s| scope_token(*s) == scope)?;
    let action = [ActionKind::Create, ActionKind::Repair, ActionKind::Remove, ActionKind::RePoint].into_iter().find(|a| action_token(*a) == action)?;
    Some(ActionRequest { kind, scope, action })
}

/// The helper's `--system-registration-apply` value for `requests`: comma-separated tokens in
/// request order. Only lowercase ASCII letters, `-`, `:` and `,`, so it needs no quoting.
///
/// # Errors
/// [`ActionError::EmptyBatch`] for no request; [`ActionError::UserScopeNotAllowed`] for any
/// `User`-scope request (those run in-process only).
pub fn encode_actions(requests: &[ActionRequest]) -> Result<String, ActionError> {
    if requests.is_empty() {
        return Err(ActionError::EmptyBatch);
    }
    if let Some(user) = requests.iter().find(|request| request.scope == Scope::User) {
        return Err(ActionError::UserScopeNotAllowed { token: request_token(user) });
    }
    Ok(requests.iter().map(request_token).collect::<Vec<_>>().join(","))
}

/// Parses the helper's `--system-registration-apply` value. Order is kept.
///
/// # Errors
/// [`ActionError::EmptyBatch`] for an empty value; [`ActionError::MalformedToken`] for a token
/// that is not `<kind>:<scope>:<action>`; [`ActionError::UserScopeNotAllowed`] for a `User`
/// token (the helper may run as another user, see the file header).
pub fn decode_actions(text: &str) -> Result<Vec<ActionRequest>, ActionError> {
    if text.trim().is_empty() {
        return Err(ActionError::EmptyBatch);
    }
    text.split(',')
        .map(|token| {
            let request = parse_request_token(token).ok_or_else(|| ActionError::MalformedToken { token: token.to_owned() })?;
            if request.scope == Scope::User {
                return Err(ActionError::UserScopeNotAllowed { token: token.to_owned() });
            }
            Ok(request)
        })
        .collect()
}

/// One action's outcome in the helper's result file.
#[derive(Debug, Serialize, Deserialize)]
struct HelperResultEntry {
    /// The action's token.
    action: String,
    ok: bool,
    /// Present exactly when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<ActionFailure>,
}

/// The helper's result file: `{"version":1,"results":[…]}`.
#[derive(Debug, Serialize, Deserialize)]
struct HelperResultFile {
    version: u32,
    results: Vec<HelperResultEntry>,
}

/// The result file's JSON for the helper's outcomes, in request order.
///
/// # Errors
/// A `serde_json` serialization error (not expected for this plain data).
pub fn helper_result_json(results: &[(ActionRequest, Result<(), ActionFailure>)]) -> Result<String, serde_json::Error> {
    let file = HelperResultFile {
        version: HELPER_RESULT_VERSION,
        results: results
            .iter()
            .map(|(request, outcome)| HelperResultEntry { action: request_token(request), ok: outcome.is_ok(), error: outcome.clone().err() })
            .collect(),
    };
    serde_json::to_string(&file)
}

/// Pairs the requested actions with the outcomes in the helper's result `text`, in request
/// order. A requested action the result does not mention is a [`FailureCode::NotRun`] failure.
///
/// # Errors
/// [`ElevationError::ResultUnreadable`] for invalid JSON, another version, an unknown token, an
/// outcome for an action that was not requested, or an entry whose `ok` and `error` disagree.
pub fn parse_helper_result(requests: &[ActionRequest], text: &str) -> Result<HelperOutcomes, ElevationError> {
    let file: HelperResultFile = serde_json::from_str(text).map_err(|error| ElevationError::ResultUnreadable(error.to_string()))?;
    if file.version != HELPER_RESULT_VERSION {
        return Err(ElevationError::ResultUnreadable(format!("result version {} (expected {HELPER_RESULT_VERSION})", file.version)));
    }
    let mut reported: Vec<Option<(ActionRequest, Result<(), ActionFailure>)>> = Vec::with_capacity(file.results.len());
    for entry in file.results {
        let request = parse_request_token(&entry.action).ok_or_else(|| ElevationError::ResultUnreadable(format!("unknown action '{}'", entry.action)))?;
        let outcome = match (entry.ok, entry.error) {
            (true, None) => Ok(()),
            (false, Some(failure)) => Err(failure),
            (true, Some(_)) | (false, None) => {
                return Err(ElevationError::ResultUnreadable(format!("inconsistent outcome of '{}'", entry.action)));
            }
        };
        reported.push(Some((request, outcome)));
    }
    let paired: Vec<_> = requests
        .iter()
        .map(|&request| {
            let found = reported.iter_mut().find(|slot| slot.as_ref().is_some_and(|(reported, _)| *reported == request)).and_then(Option::take);
            found.unwrap_or_else(|| (request, Err(ActionFailure::not_run(request))))
        })
        .collect();
    if let Some((extra, _)) = reported.into_iter().flatten().next() {
        return Err(ElevationError::ResultUnreadable(format!("outcome of '{}', which was not requested", request_token(&extra))));
    }
    Ok(paired)
}

/// The helper's exit code for its outcomes: [`HELPER_EXIT_ALL_OK`] or [`HELPER_EXIT_SOME_FAILED`].
#[must_use]
pub fn helper_exit_code(results: &[(ActionRequest, Result<(), ActionFailure>)]) -> i32 {
    if results.iter().all(|(_, outcome)| outcome.is_ok()) { HELPER_EXIT_ALL_OK } else { HELPER_EXIT_SOME_FAILED }
}

/// Checks that `path` may be the helper's result file: absolute, named
/// `manhwastudio-sysreg-*.json`. The elevated helper refuses to write anywhere else.
///
/// # Errors
/// An English description of the violation, for the log.
pub fn validate_result_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("result path '{}' is not absolute", path.display()));
    }
    let name = path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    if name.starts_with(HELPER_RESULT_FILE_PREFIX) && name.ends_with(HELPER_RESULT_FILE_SUFFIX) && name.len() > HELPER_RESULT_FILE_PREFIX.len() + HELPER_RESULT_FILE_SUFFIX.len() {
        Ok(())
    } else {
        Err(format!("result path '{}' is not a {HELPER_RESULT_FILE_PREFIX}*{HELPER_RESULT_FILE_SUFFIX} file", path.display()))
    }
}

/// A fresh result-file path under `dir` (the caller passes `std::env::temp_dir()`), unique per
/// process id, wall-clock nanoseconds and a process-wide counter.
#[must_use]
pub fn new_result_path(dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    // A clock before 1970 only weakens one of three uniqueness components; pid + sequence
    // still differ between calls, and the helper refuses to overwrite an existing file.
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("{HELPER_RESULT_FILE_PREFIX}{}-{nanos}-{sequence}{HELPER_RESULT_FILE_SUFFIX}", std::process::id()))
}

/// Creates the empty result file at `path`, which must not exist yet: an existing file, or a
/// link planted at that name, is never opened or overwritten (`create_new`).
///
/// # Errors
/// The I/O error of the create (`AlreadyExists` when `path` exists).
#[cfg(any(target_os = "windows", test))]
pub fn create_result_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)
}

/// Writes the outcomes as the result JSON into `file`, a fresh file from [`create_result_file`],
/// and syncs it.
///
/// # Errors
/// The I/O error of the write or sync, or an `InvalidData` error when the JSON cannot be built.
#[cfg(any(target_os = "windows", test))]
pub fn write_helper_result(file: &mut std::fs::File, results: &[(ActionRequest, Result<(), ActionFailure>)]) -> io::Result<()> {
    use std::io::Write;
    let json = helper_result_json(results).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    file.write_all(json.as_bytes())?;
    file.sync_all()
}

/// The elevated helper's protocol around its actions, in the order that keeps the exit code
/// truthful: validate `result_file` ([`validate_result_path`]), decode `actions`
/// ([`decode_actions`]), CREATE the result file, and only then call `run` with the decoded
/// requests and write its outcomes into the already-open file. Any refusal before `run` returns
/// [`HELPER_EXIT_USAGE`] and nothing ran. Once `run` ran, the exit code is
/// [`helper_exit_code`] of its outcomes even when the write into the open file fails (logged):
/// the parent then reads an incomplete file and reports the result unreadable, never "the
/// helper failed" over changes that were made.
#[cfg(any(target_os = "windows", test))]
#[must_use]
pub fn run_helper_protocol(actions: &str, result_file: &Path, run: impl FnOnce(&[ActionRequest]) -> HelperOutcomes) -> i32 {
    if let Err(reason) = validate_result_path(result_file) {
        ms_log::runtime_log::log_error(format!("[os-registration] helper refused: {reason}"));
        return HELPER_EXIT_USAGE;
    }
    let requests = match decode_actions(actions) {
        Ok(requests) => requests,
        Err(error) => {
            ms_log::runtime_log::log_error(format!("[os-registration] helper refused '{actions}': {error}"));
            return HELPER_EXIT_USAGE;
        }
    };
    let mut file = match create_result_file(result_file) {
        Ok(file) => file,
        Err(error) => {
            ms_log::runtime_log::log_error(format!("[os-registration] helper could not create '{}': {error}; no action runs", result_file.display()));
            return HELPER_EXIT_USAGE;
        }
    };
    let results = run(&requests);
    if let Err(error) = write_helper_result(&mut file, &results) {
        ms_log::runtime_log::log_error(format!("[os-registration] helper ran its actions but could not write '{}': {error}", result_file.display()));
    }
    helper_exit_code(&results)
}

/// Reads and then deletes the helper's result file at `path`, pairing it with `requests`
/// ([`parse_helper_result`]). A failed delete is logged only (the outcome is already known).
///
/// # Errors
/// [`ElevationError::ResultUnreadable`] when the file is missing, unreadable or invalid.
#[cfg(any(target_os = "windows", test))]
pub fn read_helper_result(path: &Path, requests: &[ActionRequest]) -> Result<HelperOutcomes, ElevationError> {
    let text = std::fs::read_to_string(path).map_err(|error| ElevationError::ResultUnreadable(format!("'{}': {error}", path.display())))?;
    discard_result_file(path);
    parse_helper_result(requests, &text)
}

/// Deletes the result file at `path` if it exists; any other failure is logged only (a stray
/// file in the temp directory is harmless and carries no secret).
#[cfg(any(target_os = "windows", test))]
pub fn discard_result_file(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => ms_log::runtime_log::log_warn(format!("[os-registration] could not delete the helper result '{}': {error}", path.display())),
    }
}

/// Runs `requests` for the copy `identity` in this process, in order, each independently (a
/// failure never skips the next), and refreshes the desktop ONCE at the end when a record that
/// the shell caches was touched (Windows: `SHChangeNotify` after an App Paths or "Open with"
/// action; Linux: `update-desktop-database` after any entry write or delete). Blocking: run it on
/// a worker. Does not elevate: on Windows a `Machine` action of an unelevated process fails with
/// access denied — route those through `windows::elevation::apply_elevated`.
///
/// Create / Repair / RePoint write the full expected value set through the record writers
/// ("Open with" deletes its tree first); Remove deletes the record whoever owns it (the caller
/// confirmed it for another copy's record).
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[must_use]
pub fn apply(identity: &CopyIdentity, requests: &[ActionRequest]) -> ActionOutcomes {
    apply_platform(identity, requests)
}

/// [`apply`] on Linux: the process environment's XDG directories and the real refresh.
#[cfg(target_os = "linux")]
fn apply_platform(identity: &CopyIdentity, requests: &[ActionRequest]) -> ActionOutcomes {
    let dirs = crate::linux::xdg::DesktopDirs::from_process_env();
    apply_linux_in(identity, dirs.as_ref(), requests, &mut crate::linux::xdg::refresh_linux_desktop_database)
}

/// Logs one action's outcome.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn log_outcome(identity: &CopyIdentity, request: &ActionRequest, outcome: &Result<(), ActionError>) {
    match outcome {
        Ok(()) => ms_log::runtime_log::log_info(format!("[os-registration] {} done for '{}'", request_token(request), identity.exe.display())),
        Err(error) => ms_log::runtime_log::log_error(format!("[os-registration] {} failed for '{}': {error}", request_token(request), identity.exe.display())),
    }
}

/// Deletes the file at `path`; a missing file is success.
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn remove_file_if_exists(path: &Path) -> Result<(), IntegrationError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(IntegrationError::RemoveFile { path: path.to_path_buf(), source }),
    }
}

/// [`apply`] on Windows.
#[cfg(target_os = "windows")]
fn apply_platform(identity: &CopyIdentity, requests: &[ActionRequest]) -> ActionOutcomes {
    let mut notify = false;
    let results = requests
        .iter()
        .map(|&request| {
            // A failed write may still have changed part of the key, so an attempt counts.
            notify |= matches!(request.kind, RecordKind::AppPaths | RecordKind::OpenWith);
            let outcome = apply_windows_one(identity, request);
            log_outcome(identity, &request, &outcome);
            (request, outcome)
        })
        .collect();
    if notify {
        crate::windows::registry::notify_shell_association_change();
    }
    results
}

/// One Windows action, without the Explorer refresh.
#[cfg(target_os = "windows")]
fn apply_windows_one(identity: &CopyIdentity, request: ActionRequest) -> Result<(), ActionError> {
    use crate::identity::SHORTCUT_FILE_NAME;
    use crate::windows::probe::registry_root;
    use crate::windows::registry::{reg_delete_tree_if_exists, reg_write_value, write_windows_open_with};
    use crate::windows::shortcut::{ShortcutSpec, windows_start_menu_programs_dir, write_shortcut};
    use crate::windows::values::{app_paths_key, uninstall_and_app_paths_values, uninstall_key, windows_open_with_app_key};

    let root = registry_root(request.scope);
    let install_dir = identity.exe.parent().unwrap_or(&identity.exe);
    let shortcut_path = || {
        windows_start_menu_programs_dir(request.scope == Scope::Machine)
            .map(|dir| dir.join(SHORTCUT_FILE_NAME))
            .ok_or(IntegrationError::StartMenuFolderNotFound)
    };
    // The Uninstall and App Paths values come from ONE writer table; an action writes the
    // rows of its own key only.
    let write_table_rows = |key: &str, display_version: &str| {
        uninstall_and_app_paths_values(root, &install_dir.to_string_lossy(), &identity.exe.to_string_lossy(), display_version)
            .iter()
            .filter(|value| value.key() == key)
            .try_for_each(reg_write_value)
    };
    match (request.kind, request.action) {
        (RecordKind::StartMenu, ActionKind::Remove) => remove_file_if_exists(&shortcut_path()?)?,
        (RecordKind::ProgramEntry, ActionKind::Remove) => {
            reg_delete_tree_if_exists(&uninstall_key(root))?;
        }
        (RecordKind::AppPaths, ActionKind::Remove) => {
            reg_delete_tree_if_exists(&app_paths_key(root))?;
        }
        (RecordKind::OpenWith, ActionKind::Remove) => {
            reg_delete_tree_if_exists(&windows_open_with_app_key(root))?;
        }
        (RecordKind::StartMenu, ActionKind::Create | ActionKind::Repair | ActionKind::RePoint) => {
            let lnk = shortcut_path()?;
            // The working directory is the copy's program root (the repository root of a
            // repository build), the same value the probe expects for this copy.
            let spec = ShortcutSpec::for_copy(identity);
            if let Some(parent) = lnk.parent() {
                std::fs::create_dir_all(parent).map_err(|source| IntegrationError::CreateShortcutFolder { parent: parent.to_path_buf(), source })?;
            }
            write_shortcut(&lnk, &spec)?;
        }
        (RecordKind::ProgramEntry, ActionKind::Create | ActionKind::Repair | ActionKind::RePoint) => {
            let version = identity.version_core.as_deref().ok_or(ActionError::MissingVersion)?;
            write_table_rows(&uninstall_key(root), version)?;
        }
        (RecordKind::AppPaths, ActionKind::Create | ActionKind::Repair | ActionKind::RePoint) => {
            // `DisplayVersion` is an Uninstall row; the App Paths rows never contain it.
            write_table_rows(&app_paths_key(root), "")?;
        }
        (RecordKind::OpenWith, ActionKind::Create | ActionKind::Repair | ActionKind::RePoint) => write_windows_open_with(root, &identity.exe)?,
    }
    Ok(())
}

/// [`apply`] on Linux with the data directories (`None` = no valid data home) and the desktop
/// refresh injected (tests pass temp dirs and a recorder).
#[cfg(target_os = "linux")]
fn apply_linux_in(
    identity: &CopyIdentity,
    dirs: Option<&crate::linux::xdg::DesktopDirs>,
    requests: &[ActionRequest],
    refresh: &mut dyn FnMut(&Path),
) -> ActionOutcomes {
    let mut touched = false;
    let results = requests
        .iter()
        .map(|&request| {
            let outcome = apply_linux_one(identity, dirs, request, &mut touched);
            log_outcome(identity, &request, &outcome);
            (request, outcome)
        })
        .collect();
    if touched && let Some(dirs) = dirs {
        refresh(&dirs.applications_dir());
    }
    results
}

/// One Linux action; sets `touched` when it attempted a write or delete of the entry.
#[cfg(target_os = "linux")]
fn apply_linux_one(identity: &CopyIdentity, dirs: Option<&crate::linux::xdg::DesktopDirs>, request: ActionRequest, touched: &mut bool) -> Result<(), ActionError> {
    use crate::linux::xdg::{DesktopEntryError, write_entry};

    // Only the per-user entry is writable; it carries both the menu and the "Open with" record.
    let writable_kind = matches!(request.kind, RecordKind::StartMenu | RecordKind::OpenWith);
    if request.scope != Scope::User || !writable_kind {
        return Err(ActionError::Unsupported { request });
    }
    let dirs = dirs.ok_or(ActionError::NoDataHome)?;
    *touched = true;
    match request.action {
        // The icon stays: it is named by the product, not the copy, and a system-wide entry may
        // resolve its `Icon=` through the user's icon theme directory.
        ActionKind::Remove => remove_file_if_exists(&dirs.entry_path()).map_err(ActionError::from),
        ActionKind::Create | ActionKind::Repair | ActionKind::RePoint => write_entry(identity, dirs).map_err(|error| match error {
            DesktopEntryError::CreateDir { path, source } => ActionError::CreateDir { path, source },
            DesktopEntryError::Write { path, source } => ActionError::WriteFile { path, source },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Defect, ProbeError};

    fn record(kind: RecordKind, scope: Scope, status: RecordStatus) -> RecordReport {
        RecordReport { kind, scope, location: "loc".to_owned(), status, shadowed: false }
    }

    fn other(alive: bool) -> RecordStatus {
        RecordStatus::OtherCopy { exe: PathBuf::from("/other/manhwastudio_rs"), alive, defects: Vec::new() }
    }

    fn broken() -> RecordStatus {
        RecordStatus::OursBroken(vec![Defect::TargetMissing { path: "p".to_owned() }])
    }

    fn request(kind: RecordKind, scope: Scope, action: ActionKind) -> ActionRequest {
        ActionRequest { kind, scope, action }
    }

    use ActionKind::{Create, RePoint, Remove, Repair};

    /// The Windows matrix: every status, in and out of the copy's scope.
    #[test]
    fn windows_action_matrix() {
        let on = |status: RecordStatus, scope: Scope, copy_scope: Scope| {
            allowed_actions_on(false, &record(RecordKind::ProgramEntry, scope, status), copy_scope, false)
        };
        use Scope::{Machine, User};
        assert_eq!(on(RecordStatus::Missing, Machine, Machine), vec![Create]);
        assert_eq!(on(RecordStatus::Missing, User, User), vec![Create]);
        assert!(on(RecordStatus::Missing, Machine, User).is_empty(), "no all-users record for a per-user copy");
        assert!(on(RecordStatus::Missing, User, Machine).is_empty(), "Create only in the copy's scope");
        assert_eq!(on(RecordStatus::OursOk, Machine, Machine), vec![Remove]);
        assert_eq!(on(RecordStatus::OursStale(vec![Defect::IconMissing]), User, User), vec![Repair, Remove]);
        assert_eq!(on(broken(), Machine, User), vec![Repair, Remove]);
        assert_eq!(on(other(true), Machine, Machine), vec![RePoint, Remove]);
        assert_eq!(on(other(false), User, Machine), vec![RePoint, Remove], "a user record may start an all-users copy");
        assert_eq!(on(other(false), Machine, User), vec![Remove], "an all-users record is never pointed at one user's copy");
        assert!(on(RecordStatus::Unreadable(ProbeError::NoDataHome), User, User).is_empty());
        // Read-only (--ignore-installed) offers nothing anywhere.
        for status in [RecordStatus::Missing, RecordStatus::OursOk, broken(), other(true)] {
            assert!(allowed_actions_on(false, &record(RecordKind::OpenWith, User, status), User, true).is_empty());
        }
        // "Open with" keeps Remove on Windows (its own key).
        assert_eq!(allowed_actions_on(false, &record(RecordKind::OpenWith, User, RecordStatus::OursOk), User, false), vec![Remove]);
    }

    /// The Linux matrix: system-wide rows have no actions, "Open with" never removes.
    #[test]
    fn linux_action_matrix() {
        let on = |kind: RecordKind, scope: Scope, status: RecordStatus| allowed_actions_on(true, &record(kind, scope, status), Scope::User, false);
        assert_eq!(on(RecordKind::StartMenu, Scope::User, RecordStatus::Missing), vec![Create]);
        assert_eq!(on(RecordKind::StartMenu, Scope::User, broken()), vec![Repair, Remove]);
        assert_eq!(on(RecordKind::StartMenu, Scope::User, other(false)), vec![RePoint, Remove]);
        assert_eq!(on(RecordKind::OpenWith, Scope::User, broken()), vec![Repair]);
        assert_eq!(on(RecordKind::OpenWith, Scope::User, other(true)), vec![RePoint]);
        assert!(on(RecordKind::OpenWith, Scope::User, RecordStatus::OursOk).is_empty());
        for status in [RecordStatus::Missing, broken(), other(false), RecordStatus::OursOk] {
            assert!(on(RecordKind::StartMenu, Scope::Machine, status).is_empty(), "system-wide rows are read-only");
        }
    }

    /// Confirmation only for replacing or deleting a record of another copy that still exists.
    #[test]
    fn confirmation_rule() {
        let alive = record(RecordKind::StartMenu, Scope::User, other(true));
        let dead = record(RecordKind::StartMenu, Scope::User, other(false));
        let ours = record(RecordKind::StartMenu, Scope::User, broken());
        assert!(needs_confirmation(&alive, RePoint));
        assert!(needs_confirmation(&alive, Remove));
        assert!(!needs_confirmation(&dead, RePoint));
        assert!(!needs_confirmation(&dead, Remove));
        assert!(!needs_confirmation(&ours, Remove));
        assert!(!needs_confirmation(&ours, Repair));
    }

    #[test]
    fn elevation_rule() {
        assert!(requires_elevation_on(false, Scope::Machine, false));
        assert!(!requires_elevation_on(false, Scope::Machine, true));
        assert!(!requires_elevation_on(false, Scope::User, false));
        for (scope, elevated) in [(Scope::Machine, false), (Scope::User, false), (Scope::Machine, true)] {
            assert!(!requires_elevation_on(true, scope, elevated));
        }
    }

    fn every_machine_request() -> Vec<ActionRequest> {
        let mut requests = Vec::new();
        for kind in [RecordKind::StartMenu, RecordKind::ProgramEntry, RecordKind::AppPaths, RecordKind::OpenWith] {
            for action in [Create, Repair, Remove, RePoint] {
                requests.push(request(kind, Scope::Machine, action));
            }
        }
        requests
    }

    /// Every machine-scope request survives the round trip, in order, and the wire form is the
    /// documented token list.
    #[test]
    fn codec_round_trip() {
        let requests = every_machine_request();
        let encoded = encode_actions(&requests).expect("machine requests encode");
        assert!(encoded.chars().all(|c| c.is_ascii_lowercase() || matches!(c, '-' | ':' | ',')), "{encoded}");
        assert_eq!(decode_actions(&encoded).expect("round trip"), requests);
        let pair = [request(RecordKind::ProgramEntry, Scope::Machine, Repair), request(RecordKind::OpenWith, Scope::Machine, RePoint)];
        assert_eq!(encode_actions(&pair).expect("encodes"), "program-entry:machine:repair,open-with:machine:repoint");
    }

    /// User scope never crosses the elevation boundary, in either direction; garbage is rejected.
    #[test]
    fn codec_rejects_user_scope_and_garbage() {
        let mixed = [request(RecordKind::AppPaths, Scope::Machine, Create), request(RecordKind::OpenWith, Scope::User, Create)];
        assert!(matches!(encode_actions(&mixed), Err(ActionError::UserScopeNotAllowed { token }) if token == "open-with:user:create"));
        assert!(matches!(decode_actions("app-paths:machine:create,open-with:user:create"), Err(ActionError::UserScopeNotAllowed { .. })));
        assert!(matches!(encode_actions(&[]), Err(ActionError::EmptyBatch)));
        assert!(matches!(decode_actions(""), Err(ActionError::EmptyBatch)));
        for garbage in ["open-with:machine", "open-with:machine:create:x", "desktop:machine:create", "open-with:machine:delete", "open-with:machine:create,", " open-with:machine:create"] {
            assert!(matches!(decode_actions(garbage), Err(ActionError::MalformedToken { .. })), "{garbage}");
        }
    }

    fn failure(code: FailureCode) -> ActionFailure {
        ActionFailure { code, os: Some(5), message: "m".to_owned(), detail: "d".to_owned() }
    }

    /// A written result parses back to the same outcomes (all ok, and partial failure).
    #[test]
    fn result_json_round_trip() {
        let a = request(RecordKind::ProgramEntry, Scope::Machine, Repair);
        let b = request(RecordKind::OpenWith, Scope::Machine, Remove);
        let ok = vec![(a, Ok(())), (b, Ok(()))];
        assert_eq!(parse_helper_result(&[a, b], &helper_result_json(&ok).expect("json")).expect("parses"), ok);
        assert_eq!(helper_exit_code(&ok), HELPER_EXIT_ALL_OK);
        let partial = vec![(a, Ok(())), (b, Err(failure(FailureCode::AccessDenied)))];
        let json = helper_result_json(&partial).expect("json");
        assert!(json.contains(r#""action":"open-with:machine:remove","ok":false,"error":{"code":"access_denied","os":5"#), "{json}");
        assert_eq!(parse_helper_result(&[a, b], &json).expect("parses"), partial);
        assert_eq!(helper_exit_code(&partial), HELPER_EXIT_SOME_FAILED);
    }

    /// Missing outcomes become NotRun; garbage, another version, unrequested or inconsistent
    /// outcomes make the whole result unreadable.
    #[test]
    fn result_json_partial_and_garbage() {
        let a = request(RecordKind::ProgramEntry, Scope::Machine, Repair);
        let b = request(RecordKind::AppPaths, Scope::Machine, Repair);
        let only_a = r#"{"version":1,"results":[{"action":"program-entry:machine:repair","ok":true}]}"#;
        let paired = parse_helper_result(&[a, b], only_a).expect("parses");
        assert_eq!(paired[0], (a, Ok(())));
        assert!(matches!(&paired[1], (request, Err(failure)) if *request == b && failure.code == FailureCode::NotRun));
        for garbage in [
            "",
            "not json",
            r#"{"version":2,"results":[]}"#,
            r#"{"version":1,"results":[{"action":"x:y:z","ok":true}]}"#,
            r#"{"version":1,"results":[{"action":"open-with:machine:remove","ok":true}]}"#,
            r#"{"version":1,"results":[{"action":"program-entry:machine:repair","ok":false}]}"#,
            r#"{"version":1,"results":[{"action":"program-entry:machine:repair","ok":true,"error":{"code":"failed","os":null,"message":"m","detail":"d"}}]}"#,
            r#"{"version":1,"results":[{"action":"program-entry:machine:repair","ok":false,"error":{"code":"weird","os":null,"message":"m","detail":"d"}}]}"#,
        ] {
            assert!(matches!(parse_helper_result(&[a, b], garbage), Err(ElevationError::ResultUnreadable(_))), "{garbage}");
        }
    }

    /// The result file is written only at a fresh, well-named path, read back once and deleted;
    /// a missing file is unreadable.
    #[test]
    fn result_file_write_read_and_missing() {
        let dir = std::env::temp_dir();
        let path = new_result_path(&dir);
        assert_ne!(path, new_result_path(&dir), "every call names a new file");
        assert_eq!(validate_result_path(&path), Ok(()));
        let a = request(RecordKind::StartMenu, Scope::Machine, Create);
        let results = vec![(a, Err(failure(FailureCode::NotElevated)))];
        let mut file = create_result_file(&path).expect("first create");
        write_helper_result(&mut file, &results).expect("first write");
        drop(file);
        let again = create_result_file(&path).expect_err("an existing file is never reopened");
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read_helper_result(&path, &[a]).expect("reads"), results);
        assert!(!path.exists(), "the result file is deleted after reading");
        assert!(matches!(read_helper_result(&path, &[a]), Err(ElevationError::ResultUnreadable(_))));
    }

    /// The helper creates its result file BEFORE running anything: when the file cannot be
    /// created (a file already sits at that name), or the path or the tokens are refused, the
    /// actions never run and the exit code is `HELPER_EXIT_USAGE`; otherwise the actions run once
    /// and their outcomes land in the file the parent reads.
    #[test]
    fn helper_protocol_creates_the_result_before_running() {
        let a = request(RecordKind::StartMenu, Scope::Machine, Create);
        let tokens = encode_actions(&[a]).expect("encodes");
        let dir = std::env::temp_dir();
        let ran = std::cell::Cell::new(0_u32);
        let run = |requests: &[ActionRequest]| -> HelperOutcomes {
            ran.set(ran.get() + 1);
            requests.iter().map(|&request| (request, Err(failure(FailureCode::AccessDenied)))).collect()
        };

        let planted = new_result_path(&dir);
        std::fs::write(&planted, b"planted").expect("plants a file at the result name");
        assert_eq!(run_helper_protocol(&tokens, &planted, run), HELPER_EXIT_USAGE);
        assert_eq!(ran.get(), 0, "an uncreatable result file runs nothing");
        assert_eq!(std::fs::read(&planted).expect("planted file stays"), b"planted", "the planted file is never written");
        discard_result_file(&planted);

        assert_eq!(run_helper_protocol(&tokens, &dir.join("evil.json"), run), HELPER_EXIT_USAGE);
        let user_scope = new_result_path(&dir);
        assert_eq!(run_helper_protocol("start-menu:user:create", &user_scope, run), HELPER_EXIT_USAGE);
        assert!(!user_scope.exists(), "a refused batch creates no result file");
        assert_eq!(ran.get(), 0, "refused paths and tokens run nothing");

        let fresh = new_result_path(&dir);
        assert_eq!(run_helper_protocol(&tokens, &fresh, run), HELPER_EXIT_SOME_FAILED);
        assert_eq!(ran.get(), 1);
        let outcomes = read_helper_result(&fresh, &[a]).expect("the parent reads the outcomes");
        assert_eq!(outcomes, vec![(a, Err(failure(FailureCode::AccessDenied)))]);
        assert!(!fresh.exists());
    }

    #[test]
    fn result_path_validation() {
        let tmp = std::env::temp_dir();
        assert!(validate_result_path(Path::new("manhwastudio-sysreg-1.json")).is_err(), "relative");
        assert!(validate_result_path(&tmp.join("manhwastudio-sysreg-.json")).is_err(), "empty unique part");
        assert!(validate_result_path(&tmp.join("evil.dll")).is_err());
        assert!(validate_result_path(&tmp.join("manhwastudio-sysreg-1.txt")).is_err());
        assert!(validate_result_path(&tmp.join("manhwastudio-sysreg-1.json")).is_ok());
    }

    /// Failure codes: the helper's own refusals keep their code; access denied is recognised
    /// for registry, shortcut and file errors; the OS number travels along.
    #[test]
    fn failure_mapping() {
        let tag = ms_i18n::LocaleTag::parse("en").expect("valid embedded tag");
        ms_i18n::set_locale(&tag).expect("embedded catalog installs");
        let not_elevated = ActionError::NotElevated.failure();
        assert_eq!(not_elevated.code, FailureCode::NotElevated);
        assert_eq!(not_elevated.message, t!("os_integration.action.not_elevated_error"));
        assert_ne!(not_elevated.message, "os_integration.action.not_elevated_error", "the key exists in the catalog");
        let unsupported = ActionError::Unsupported { request: request(RecordKind::AppPaths, Scope::User, Create) };
        assert_eq!(unsupported.failure().code, FailureCode::Unsupported);
        assert_eq!(unsupported.to_string(), "action app-paths:user:create is not supported on this system");
        let denied = ActionError::from(IntegrationError::RegistrySetValue { key: r"HKLM\K".to_owned(), value_name: None, code: 5 }).failure();
        assert_eq!((denied.code, denied.os), (FailureCode::AccessDenied, Some(5)));
        let lnk = ActionError::from(IntegrationError::ShortcutWrite { path: PathBuf::from("x"), step: "s", code: HRESULT_ACCESS_DENIED, message: String::new() }).failure();
        assert_eq!(lnk.code, FailureCode::AccessDenied);
        let missing = ActionError::from(IntegrationError::RegistryOpenKey { key: r"HKLM\K".to_owned(), code: 2 }).failure();
        assert_eq!((missing.code, missing.os), (FailureCode::Failed, Some(2)));
        let write = ActionError::WriteFile { path: PathBuf::from("/x"), source: io::Error::from(io::ErrorKind::PermissionDenied) };
        assert_eq!(write.failure().code, FailureCode::AccessDenied);
        for key_text in [
            ActionError::NoDataHome.user_message(),
            ActionError::MissingVersion.user_message(),
            ActionError::EmptyBatch.user_message(),
            ElevationError::Declined.user_message(),
            ElevationError::TimedOut.user_message(),
            ElevationError::LaunchFailed { code: 2 }.user_message(),
            ElevationError::WaitFailed { code: 6 }.user_message(),
            ElevationError::HelperFailed { exit_code: 3 }.user_message(),
            ElevationError::ResultUnreadable("x".to_owned()).user_message(),
            ActionFailure::not_run(request(RecordKind::AppPaths, Scope::Machine, Create)).message,
        ] {
            assert!(!key_text.starts_with("os_integration.") && !key_text.contains('{'), "unrendered catalog text: {key_text}");
        }
    }

    #[cfg(target_os = "linux")]
    mod linux_apply {
        use std::fs;

        use super::*;
        use crate::linux::desktop_entry::linux_desktop_entry_text;
        use crate::linux::xdg::DesktopDirs;

        /// A uniquely named scratch directory under the system temp dir, removed on drop.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new(tag: &str) -> Self {
                let path = std::env::temp_dir().join(format!("ms-os-actions-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()));
                if path.exists() {
                    fs::remove_dir_all(&path).expect("stale scratch dir must be removable");
                }
                fs::create_dir_all(&path).expect("scratch dir must be creatable");
                Self(path)
            }

            fn dirs(&self) -> DesktopDirs {
                DesktopDirs { data_home: self.0.join("data"), data_dirs: vec![self.0.join("system")] }
            }

            fn identity(&self, name: &str) -> CopyIdentity {
                let root = self.0.join(name);
                fs::create_dir_all(&root).expect("copy dir");
                let exe = root.join("manhwastudio_rs");
                fs::write(&exe, b"").expect("fake exe");
                CopyIdentity { exe, program_root: root, version_core: None }
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                if let Err(err) = fs::remove_dir_all(&self.0) {
                    eprintln!("could not remove scratch dir {}: {err}", self.0.display());
                }
            }
        }

        fn run(identity: &CopyIdentity, dirs: Option<&DesktopDirs>, requests: &[ActionRequest]) -> (ActionOutcomes, Vec<PathBuf>) {
            let mut refreshed = Vec::new();
            let results = apply_linux_in(identity, dirs, requests, &mut |dir| refreshed.push(dir.to_path_buf()));
            (results, refreshed)
        }

        /// Create writes entry + icon, RePoint takes over another copy's entry, Remove deletes
        /// the entry and keeps the icon; one refresh per batch.
        #[test]
        fn create_repoint_remove() {
            let scratch = Scratch::new("crr");
            let dirs = scratch.dirs();
            let ours = scratch.identity("ours");
            let theirs = scratch.identity("theirs");

            let create = [request(RecordKind::StartMenu, Scope::User, Create), request(RecordKind::OpenWith, Scope::User, Create)];
            let (results, refreshed) = run(&ours, Some(&dirs), &create);
            assert!(results.iter().all(|(_, outcome)| outcome.is_ok()), "{results:?}");
            assert_eq!(refreshed, vec![dirs.applications_dir()], "exactly one refresh for the batch");
            assert_eq!(fs::read_to_string(dirs.entry_path()).expect("entry"), linux_desktop_entry_text(&ours));
            assert_eq!(fs::read(dirs.icon_path()).expect("icon"), crate::identity::APP_ICON_PNG);

            fs::write(dirs.entry_path(), linux_desktop_entry_text(&theirs)).expect("foreign entry");
            let (results, _) = run(&ours, Some(&dirs), &[request(RecordKind::StartMenu, Scope::User, RePoint)]);
            assert!(results[0].1.is_ok());
            assert_eq!(fs::read_to_string(dirs.entry_path()).expect("entry"), linux_desktop_entry_text(&ours), "explicit repoint replaces the foreign entry");

            let (results, refreshed) = run(&ours, Some(&dirs), &[request(RecordKind::StartMenu, Scope::User, Remove)]);
            assert!(results[0].1.is_ok());
            assert_eq!(refreshed.len(), 1);
            assert!(!dirs.entry_path().exists());
            assert!(dirs.icon_path().exists(), "the icon is kept");
            // Removing what is already gone is success.
            let (results, _) = run(&ours, Some(&dirs), &[request(RecordKind::StartMenu, Scope::User, Remove)]);
            assert!(results[0].1.is_ok());
            assert!(!dirs.data_dirs[0].exists(), "system-wide directories are never written");
        }

        /// Unsupported kinds and scopes are refused without touching anything; no data home is
        /// its own error; a write failure is reported per action and the next one still runs.
        #[test]
        fn refusals_and_failures() {
            let scratch = Scratch::new("refuse");
            let dirs = scratch.dirs();
            let ours = scratch.identity("ours");
            let refused = [
                request(RecordKind::ProgramEntry, Scope::User, Create),
                request(RecordKind::AppPaths, Scope::User, Repair),
                request(RecordKind::StartMenu, Scope::Machine, Create),
            ];
            let (results, refreshed) = run(&ours, Some(&dirs), &refused);
            assert!(results.iter().all(|(_, outcome)| matches!(outcome, Err(ActionError::Unsupported { .. }))), "{results:?}");
            assert!(refreshed.is_empty(), "nothing touched, nothing refreshed");
            assert!(!dirs.data_home.exists());

            let (results, refreshed) = run(&ours, None, &[request(RecordKind::StartMenu, Scope::User, Create)]);
            assert!(matches!(results[0].1, Err(ActionError::NoDataHome)));
            assert!(refreshed.is_empty());

            // A file where the data home should be: the write fails, the following request runs.
            fs::write(&dirs.data_home, b"").expect("blocker");
            let batch = [request(RecordKind::StartMenu, Scope::User, Create), request(RecordKind::AppPaths, Scope::User, Create)];
            let (results, refreshed) = run(&ours, Some(&dirs), &batch);
            assert!(matches!(results[0].1, Err(ActionError::CreateDir { .. })), "{results:?}");
            assert!(matches!(results[1].1, Err(ActionError::Unsupported { .. })));
            assert_eq!(refreshed.len(), 1, "an attempted write still refreshes once");
        }
    }
}
