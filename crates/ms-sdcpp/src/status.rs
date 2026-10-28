/*
File: crates/ms-sdcpp/src/status.rs

Purpose:
The ONE process-global answer to "is sd.cpp ready on this machine": which build is selected,
whether the platform has the runtime, whether that build is installed, whether the host meets
its requirements, what the last probe of it found, and whether a worker process runs.

Key items:
- SdCppStatus        : the snapshot (cheap clone), with a `revision` that changes on every update
- SdCppStatus::ready : the device-independent "can a run be offered" answer (installed and
                       the host meets the requirements); `Option`, unknown before a refresh
- snapshot()         : read it (GUI thread safe: one short lock, no I/O)
- ready()            : `SdCppStatus::ready` of the current status without cloning it
- request_refresh()  : re-read the choice from `user_config` and re-probe install +
                       requirements on ONE worker (a request during a run schedules one more run)
- publish_probe() / publish_worker_running() : facts the worker client reports

Notes:
- The `--no-ai` switch is NOT part of this answer: consumers combine it with
  `ms_local_ai::local_ai::AiRouteKind::SdCpp.availability(..)` (this crate does not depend on
  `ms-local-ai`).
- A probe result belongs to one build: it is dropped when the selection moves to another build
  or that build is removed.
- Process-global state by design (the same shape as `ms_local_ai::backend_health`): one mutex,
  held only to copy or replace small values, never across I/O.
*/

use std::sync::{Mutex, PoisonError};

use ms_log::runtime_log;
use ms_platform::{PlatformFeature, Unavailable};

use crate::catalog::{SdCppBuild, build_from_choice, row_here};
use crate::error::SdCppError;
use crate::platform_check::{RequirementProblem, check_requirements};
use crate::request::ProbeReport;

/// Snapshot of the sd.cpp runtime's readiness.
#[derive(Debug, Clone, PartialEq)]
pub struct SdCppStatus {
    /// The build the persisted choice selects here (`None`: unavailable platform, nothing
    /// offered — see [`Self::catalog_problem`] — or no refresh finished yet).
    pub selected: Option<SdCppBuild>,
    /// Whether the platform has the sd.cpp runtime at all.
    pub platform: Result<(), Unavailable>,
    /// Whether `selected` is installed; `None` until a refresh answered.
    pub installed: Option<bool>,
    /// Whether the host meets `selected`'s requirements; `None` until a refresh answered.
    pub requirements: Option<Result<(), RequirementProblem>>,
    /// The last probe of `selected` (devices, version) or why it failed; `None` until the
    /// worker client published one.
    pub probe: Option<Result<ProbeReport, SdCppError>>,
    /// Whether an sd.cpp worker process is running.
    pub worker_running: bool,
    /// Whether a refresh is in flight.
    pub refreshing: bool,
    /// Increases on every change, so a reader can skip work when nothing changed.
    pub revision: u64,
}

impl SdCppStatus {
    /// Why sd.cpp cannot be offered here at all although the platform has the runtime: the
    /// catalog has no package for this OS / architecture (`NoBuildForPlatform`). A refreshed
    /// status then keeps `selected == None` for good — a reader shows this error instead of
    /// waiting for a selection. Pure (a catalog lookup), safe on the GUI thread.
    #[must_use]
    pub fn catalog_problem(&self) -> Option<SdCppError> {
        (self.platform.is_ok() && !crate::catalog::offers_a_build_here()).then_some(SdCppError::NoBuildForPlatform)
    }

    /// Whether sd.cpp is ready here, independent of devices and probes: `Some(true)` when the
    /// selected build is installed and the host meets its requirements; `Some(false)` when the
    /// platform has no runtime, the catalog offers no build ([`Self::catalog_problem`]), the
    /// build is not installed, or a requirement is not met; `None` while no refresh answered
    /// yet. The ONE owner of this decision: UI gates and finer readiness states build on it.
    /// The `--no-ai` switch is not part of it (see the file header). Pure, GUI-thread safe.
    #[must_use]
    pub fn ready(&self) -> Option<bool> {
        if self.platform.is_err() || self.catalog_problem().is_some() || self.installed == Some(false) || matches!(self.requirements, Some(Err(_))) {
            return Some(false);
        }
        (self.installed == Some(true) && matches!(self.requirements, Some(Ok(())))).then_some(true)
    }

    /// The state before any refresh: only the platform answer is known.
    const fn initial() -> Self {
        Self { selected: None, platform: PlatformFeature::SdCppRuntime.require(), installed: None, requirements: None, probe: None, worker_running: false, refreshing: false, revision: 0 }
    }
}

/// What one refresh run found.
#[derive(Debug, Clone, PartialEq)]
struct RefreshFacts {
    platform: Result<(), Unavailable>,
    selected: Option<SdCppBuild>,
    installed: Option<bool>,
    requirements: Option<Result<(), RequirementProblem>>,
}

/// The whole guarded state.
#[derive(Debug)]
struct State {
    status: SdCppStatus,
    /// The build the stored probe belongs to.
    probe_build: Option<SdCppBuild>,
    /// A refresh worker runs.
    refresh_running: bool,
    /// Another refresh was requested while one ran.
    refresh_again: bool,
}

impl State {
    const fn initial() -> Self {
        Self { status: SdCppStatus::initial(), probe_build: None, refresh_running: false, refresh_again: false }
    }

    fn bump(&mut self) {
        self.status.revision = self.status.revision.wrapping_add(1);
    }

    /// Stores a refresh result; a probe of another build than the new selection is dropped.
    fn apply(&mut self, facts: RefreshFacts) {
        if self.probe_build.is_some() && self.probe_build != facts.selected {
            self.status.probe = None;
            self.probe_build = None;
        }
        self.status.platform = facts.platform;
        self.status.selected = facts.selected;
        self.status.installed = facts.installed;
        self.status.requirements = facts.requirements;
        self.bump();
    }

    /// Stores a probe of `build` when it is the selected build; otherwise ignores it.
    fn set_probe(&mut self, build: SdCppBuild, result: Result<ProbeReport, SdCppError>) -> bool {
        if self.status.selected != Some(build) {
            return false;
        }
        self.status.probe = Some(result);
        self.probe_build = Some(build);
        self.bump();
        true
    }

    /// Drops the stored probe if it belongs to `build`.
    fn forget_probe(&mut self, build: SdCppBuild) {
        if self.probe_build == Some(build) {
            self.status.probe = None;
            self.probe_build = None;
            self.bump();
        }
    }
}

static STATE: Mutex<State> = Mutex::new(State::initial());

/// Runs `body` on the locked state. A poisoned lock still holds a consistent state: every
/// mutation is a set of plain field stores that cannot panic halfway.
fn with_state<T>(body: impl FnOnce(&mut State) -> T) -> T {
    let mut state = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    body(&mut state)
}

/// The current status (a clone; one short lock, no I/O — safe on the GUI thread).
#[must_use]
pub fn snapshot() -> SdCppStatus {
    with_state(|state| state.status.clone())
}

/// [`SdCppStatus::ready`] of the current status (one short lock, no clone, no I/O — cheap
/// enough for a per-frame button gate on the GUI thread).
#[must_use]
pub fn ready() -> Option<bool> {
    with_state(|state| state.status.ready())
}

/// Starts a refresh on a worker thread: reads the persisted choice from `user_config`
/// (read-only), selects the build, and re-probes whether it is installed and whether the host
/// meets its requirements. At most one refresh runs; a request during a run schedules exactly
/// one more run after it. Never blocks the caller.
pub fn request_refresh() {
    let start = with_state(|state| {
        if state.refresh_running {
            state.refresh_again = true;
            return false;
        }
        state.refresh_running = true;
        state.status.refreshing = true;
        state.bump();
        true
    });
    if !start {
        return;
    }
    let spawned = ms_thread::Builder::new().name("sdcpp-status".to_owned()).spawn(|| {
        loop {
            let facts = gather_facts();
            let again = with_state(|state| {
                state.apply(facts);
                if state.refresh_again {
                    state.refresh_again = false;
                    return true;
                }
                state.refresh_running = false;
                state.status.refreshing = false;
                state.bump();
                false
            });
            if !again {
                break;
            }
        }
    });
    if let Err(err) = spawned {
        runtime_log::log_error(format!("[sdcpp] cannot start the status refresh thread: {err}"));
        with_state(|state| {
            state.refresh_running = false;
            state.refresh_again = false;
            state.status.refreshing = false;
            state.bump();
        });
    }
}

/// Publishes the result of probing `build` (from the worker client). Stored only when `build`
/// is the selected build; a stale probe is logged and dropped.
pub fn publish_probe(build: SdCppBuild, result: Result<ProbeReport, SdCppError>) {
    if !with_state(|state| state.set_probe(build, result)) {
        runtime_log::log_info(format!("[sdcpp] probe of build '{}' ignored: it is not the selected build", build.wire()));
    }
}

/// Publishes whether an sd.cpp worker process is running (from the worker client).
pub fn publish_worker_running(running: bool) {
    with_state(|state| {
        if state.status.worker_running != running {
            state.status.worker_running = running;
            state.bump();
        }
    });
}

/// Drops the stored probe of `build` (its package was removed).
pub(crate) fn forget_probe(build: SdCppBuild) {
    with_state(|state| state.forget_probe(build));
}

/// One refresh run's blocking work (worker thread).
fn gather_facts() -> RefreshFacts {
    let platform = PlatformFeature::SdCppRuntime.require();
    if platform.is_err() {
        return RefreshFacts { platform, selected: None, installed: None, requirements: None };
    }
    let choice = match ms_config::load_raw_user_settings_for_startup() {
        Ok(settings) => ms_config::sdcpp_choice_from_user_settings(&settings),
        Err(err) => {
            runtime_log::log_warn(format!("[sdcpp] cannot read user_config for the sd.cpp choice; using defaults: {err:#}"));
            ms_config::SdCppChoice::default()
        }
    };
    let selected = build_from_choice(&choice);
    let row = selected.and_then(row_here);
    let installed = selected.map(crate::install::is_installed);
    let requirements = row.map(check_requirements);
    if let (Some(build), Some(Err(problem))) = (selected, requirements.as_ref()) {
        runtime_log::log_info(format!("[sdcpp] build '{}' requirement not met: {problem:?}", build.wire()));
    }
    RefreshFacts { platform, selected, installed, requirements }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(selected: Option<SdCppBuild>) -> RefreshFacts {
        RefreshFacts { platform: Ok(()), selected, installed: Some(true), requirements: Some(Ok(())) }
    }

    fn report() -> ProbeReport {
        ProbeReport { sd_version: "v".into(), sd_commit: "be48649".into(), devices: Vec::new(), physical_cores: 6 }
    }

    #[test]
    fn apply_stores_facts_and_bumps_revision() {
        let mut state = State::initial();
        let before = state.status.revision;
        state.apply(facts(Some(SdCppBuild::Vulkan)));
        assert_eq!(state.status.selected, Some(SdCppBuild::Vulkan));
        assert_eq!(state.status.installed, Some(true));
        assert_eq!(state.status.requirements, Some(Ok(())));
        assert!(state.status.revision > before);
    }

    #[test]
    fn probe_is_kept_for_its_build_only() {
        let mut state = State::initial();
        assert!(!state.set_probe(SdCppBuild::Vulkan, Ok(report())), "no selection yet");
        state.apply(facts(Some(SdCppBuild::Vulkan)));
        assert!(state.set_probe(SdCppBuild::Vulkan, Ok(report())));
        assert!(!state.set_probe(SdCppBuild::Cpu, Err(SdCppError::Busy)));
        state.apply(facts(Some(SdCppBuild::Vulkan)));
        assert_eq!(state.status.probe, Some(Ok(report())), "same build keeps its probe");
        state.apply(facts(Some(SdCppBuild::Cpu)));
        assert_eq!(state.status.probe, None, "a new selection drops the old probe");
    }

    #[test]
    fn forget_probe_drops_only_that_builds_probe() {
        let mut state = State::initial();
        state.apply(facts(Some(SdCppBuild::Cpu)));
        assert!(state.set_probe(SdCppBuild::Cpu, Ok(report())));
        state.forget_probe(SdCppBuild::Vulkan);
        assert!(state.status.probe.is_some());
        state.forget_probe(SdCppBuild::Cpu);
        assert!(state.status.probe.is_none());
    }

    #[test]
    fn offers_a_build_here_matches_the_build_list() {
        assert_eq!(crate::catalog::offers_a_build_here(), !crate::catalog::builds_here().is_empty());
    }

    #[test]
    fn catalog_problem_follows_the_platform_and_the_rows() {
        let status = SdCppStatus::initial();
        let expected = (status.platform.is_ok() && crate::catalog::builds_here().is_empty()).then_some(SdCppError::NoBuildForPlatform);
        assert_eq!(status.catalog_problem(), expected);
        // Wherever the platform has the runtime, the catalog offers a build (x86_64 rows).
        if status.platform.is_ok() {
            assert_eq!(status.catalog_problem(), None);
        }
    }

    /// A status on a platform with the runtime (the other branches are covered by
    /// `ready_is_false_without_the_platform`).
    fn status_with(installed: Option<bool>, requirements: Option<Result<(), RequirementProblem>>) -> SdCppStatus {
        SdCppStatus { selected: Some(SdCppBuild::Cpu), platform: Ok(()), installed, requirements, ..SdCppStatus::initial() }
    }

    #[test]
    fn ready_follows_install_and_requirements() {
        // The catalog offers a build wherever the platform has the runtime (x86_64 rows); on a
        // host without rows `catalog_problem` answers `Some(false)` first.
        if crate::catalog::builds_here().is_empty() {
            assert_eq!(status_with(Some(true), Some(Ok(()))).ready(), Some(false));
            return;
        }
        assert_eq!(status_with(Some(true), Some(Ok(()))).ready(), Some(true));
        assert_eq!(status_with(Some(false), Some(Ok(()))).ready(), Some(false), "not installed");
        assert_eq!(status_with(Some(false), None).ready(), Some(false), "not installed, requirements unknown");
        let problem = RequirementProblem::VulkanLoaderMissing { library: "libvulkan.so.1" };
        assert_eq!(status_with(Some(true), Some(Err(problem.clone()))).ready(), Some(false), "requirement not met");
        assert_eq!(status_with(None, Some(Err(problem))).ready(), Some(false), "requirement not met, install unknown");
        assert_eq!(status_with(None, None).ready(), None, "no refresh answered");
        assert_eq!(status_with(Some(true), None).ready(), None, "requirements unknown");
        assert_eq!(status_with(None, Some(Ok(()))).ready(), None, "install unknown");
    }

    #[test]
    fn ready_is_false_without_the_platform() {
        let off = SdCppStatus { platform: Err(Unavailable::new(PlatformFeature::SdCppRuntime, ms_platform::UnavailableReason::NotOnThisPlatform)), installed: Some(true), requirements: Some(Ok(())), ..SdCppStatus::initial() };
        assert_eq!(off.ready(), Some(false));
    }

    #[test]
    fn initial_status_knows_only_the_platform() {
        let status = SdCppStatus::initial();
        assert_eq!(status.platform, PlatformFeature::SdCppRuntime.require());
        assert_eq!(status.selected, None);
        assert_eq!(status.installed, None);
        assert!(!status.worker_running && !status.refreshing);
    }
}
