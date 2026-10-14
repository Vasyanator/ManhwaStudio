/*
File: src/studio/busy.rs

Purpose:
The one place where the studio decides "is a persistence operation busy". `StudioApp` and the
project session (`studio/session/project/`) ask this question at several sites (page-op gate, save
gate, save button, typing export gate, page manager, page-op veil, discard idempotence); each site
calls a named predicate here instead of spelling its own OR of fields.

Key structures:
- `PersistenceActivity`: a `Copy` snapshot of the busy flags, taken once per decision.
- `PersistenceJobs`: the session-owned half of it (`ProjectSession::busy_jobs`; all clear in an
  image-set session).

Key functions:
- `StudioApp::persistence_activity()` / `PersistenceActivity::new`: build the snapshot from the
  session's jobs and two tab getters.
- `PersistenceActivity::{structural_op_active, save_busy, blocks_page_op, clean_op_in_flight,
  exit_cleanup_running}`: one predicate per subset of flags in use today.
- `PersistenceActivity::save_blocker` + `SaveBlocker`: why a project save cannot start, one cause.

Notes:
A save is blocked by the page-op set (`blocks_page_op`) plus the clean worker (`save_blocker`): it
never starts while the DISCARD delete job or a typing export runs. `save_busy` is the narrower set the typing
export gate reads. A predicate is chosen per SUBSET, so two consumers that must diverge later get
a new predicate rather than an edited shared one.
*/

use super::StudioApp;

/// Snapshot of every flag a "persistence busy" decision reads.
///
/// Built by [`StudioApp::persistence_activity`]; it is a plain value, so taking it ends every
/// borrow of the app before tab states are borrowed for drawing. All flags are evaluated eagerly
/// (the call sites short-circuited before), which is safe because every source is a side-effect
/// free field read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PersistenceActivity {
    /// A save-to-project worker is running (`ProjectSession`).
    save_running: bool,
    /// A save waits for the whole-project layer preload (`ProjectSession`).
    save_pending_preload: bool,
    /// A structural page-op worker is running (`ProjectSession`).
    page_op_running: bool,
    /// A structural page op failed and its dialog is open (`ProjectSession`).
    page_op_failed: bool,
    /// A project reload after a page op is pending (`ProjectSession`).
    reload_pending: bool,
    /// The DISCARD exit's staging delete job runs (`ProjectSession`).
    exit_cleanup_running: bool,
    /// The typing tab's export worker runs (`TypingTabState::export_in_progress`).
    export_in_progress: bool,
    /// The page manager's clean attach/detach worker runs
    /// (`PageManagerTabState::clean_op_in_flight`).
    clean_op_in_flight: bool,
}

impl PersistenceActivity {
    /// A structural page operation owns the session: its worker runs, its failure dialog is open,
    /// or the project reload it requested is pending.
    ///
    /// Consumers: the page-op veil (`draw_page_op_overlays`), the first save-request gate and the
    /// save button's "blocked by page operation" hover text.
    #[must_use]
    pub(crate) const fn structural_op_active(self) -> bool {
        self.page_op_running || self.page_op_failed || self.reload_pending
    }

    /// A project save cannot start or is already under way: [`Self::structural_op_active`], or a
    /// save runs or waits on the layer preload.
    ///
    /// Consumer: the typing tab's export gate (`TypingTabState::set_save_busy`).
    #[must_use]
    pub(crate) const fn save_busy(self) -> bool {
        self.save_running || self.save_pending_preload || self.structural_op_active()
    }

    /// A structural page operation or a project save must not start: [`Self::save_busy`], the
    /// discard delete job, or a running typing export. The clean-layer worker is a separate gate
    /// ([`Self::clean_op_in_flight`]) with its own log line.
    ///
    /// Consumers: `start_page_op`'s first gate and the page manager's `op_in_progress`. Saves read
    /// the same set plus the clean worker, through [`Self::save_blocker`].
    #[must_use]
    pub(crate) const fn blocks_page_op(self) -> bool {
        self.save_busy() || self.exit_cleanup_running || self.export_in_progress
    }

    /// The page manager's clean attach/detach worker holds page indices and the current overlay
    /// model.
    ///
    /// Consumers: the second gate of `start_page_op` and of the save request.
    #[must_use]
    pub(crate) const fn clean_op_in_flight(self) -> bool {
        self.clean_op_in_flight
    }

    /// The DISCARD exit's staging delete job runs.
    ///
    /// Consumer: `start_exit_cleanup`'s idempotence guard.
    #[must_use]
    pub(crate) const fn exit_cleanup_running(self) -> bool {
        self.exit_cleanup_running
    }

    /// Why a project save cannot start now, one cause in a fixed priority (page operation, clean
    /// operation, discard cleanup, export, a save already under way); `None` = it may start.
    /// Some(_) exactly when [`Self::blocks_page_op`] or [`Self::clean_op_in_flight`] holds.
    ///
    /// Consumers: every save trigger (`request_save_to_project`: status text, or a silent no-op for
    /// `SaveInFlight`), the «save project» button (disabled on any blocker, with its hover text)
    /// and the exit dialog's Save button.
    #[must_use]
    pub(crate) const fn save_blocker(self) -> Option<SaveBlocker> {
        if self.structural_op_active() {
            Some(SaveBlocker::PageOperation)
        } else if self.clean_op_in_flight {
            Some(SaveBlocker::CleanOperation)
        } else if self.exit_cleanup_running {
            Some(SaveBlocker::ExitCleanup)
        } else if self.export_in_progress {
            Some(SaveBlocker::Export)
        } else if self.save_running || self.save_pending_preload {
            Some(SaveBlocker::SaveInFlight)
        } else {
            None
        }
    }
}

/// Why a project save cannot start ([`PersistenceActivity::save_blocker`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SaveBlocker {
    /// A structural page op runs, failed, or its reload is pending
    /// ([`PersistenceActivity::structural_op_active`]).
    PageOperation,
    /// The page manager's clean attach/detach worker runs.
    CleanOperation,
    /// The DISCARD exit's staging delete job runs: a save would write into a tree being deleted.
    ExitCleanup,
    /// The typing tab's export worker runs.
    Export,
    /// A save already runs or waits on the layer preload: a new trigger is a no-op.
    SaveInFlight,
}

/// The session's own persistence jobs: the half of [`PersistenceActivity`] that a project session
/// owns (`ProjectSession::busy_jobs`). An image-set session runs none of them (`Default`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PersistenceJobs {
    /// A save-to-project worker is running.
    pub(super) save_running: bool,
    /// A save waits for the whole-project layer preload.
    pub(super) save_pending_preload: bool,
    /// A structural page-op worker is running.
    pub(super) page_op_running: bool,
    /// A structural page op failed and its dialog is open.
    pub(super) page_op_failed: bool,
    /// A project reload after a page op is pending.
    pub(super) reload_pending: bool,
    /// The DISCARD exit's staging delete job runs.
    pub(super) exit_cleanup_running: bool,
}

impl PersistenceActivity {
    /// The snapshot of the session's `jobs` plus the two tab facts: the typing export worker
    /// (`export_in_progress`) and the page manager's clean attach/detach worker
    /// (`clean_op_in_flight`).
    #[must_use]
    pub(super) const fn new(jobs: PersistenceJobs, export_in_progress: bool, clean_op_in_flight: bool) -> Self {
        Self {
            save_running: jobs.save_running,
            save_pending_preload: jobs.save_pending_preload,
            page_op_running: jobs.page_op_running,
            page_op_failed: jobs.page_op_failed,
            reload_pending: jobs.reload_pending,
            exit_cleanup_running: jobs.exit_cleanup_running,
            export_in_progress,
            clean_op_in_flight,
        }
    }
}

impl StudioApp {
    /// Snapshots the persistence flags of this frame. Cheap (field reads only); take it right
    /// where a decision is made, before tab states are borrowed for drawing.
    #[must_use]
    pub(super) fn persistence_activity(&self) -> PersistenceActivity {
        PersistenceActivity::new(
            self.session.busy_jobs(),
            self.tabs.typing.export_in_progress(),
            self.tabs.page_manager.clean_op_in_flight(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{PersistenceActivity, SaveBlocker};

    /// Every flag, in the order of the expectation columns below.
    const FLAGS: [&str; 8] = [
        "save_running",
        "save_pending_preload",
        "page_op_running",
        "page_op_failed",
        "reload_pending",
        "exit_cleanup_running",
        "export_in_progress",
        "clean_op_in_flight",
    ];

    /// An activity with exactly the named flag set.
    fn only(flag: &str) -> PersistenceActivity {
        let mut activity = PersistenceActivity::default();
        let slot = match flag {
            "save_running" => &mut activity.save_running,
            "save_pending_preload" => &mut activity.save_pending_preload,
            "page_op_running" => &mut activity.page_op_running,
            "page_op_failed" => &mut activity.page_op_failed,
            "reload_pending" => &mut activity.reload_pending,
            "exit_cleanup_running" => &mut activity.exit_cleanup_running,
            "export_in_progress" => &mut activity.export_in_progress,
            "clean_op_in_flight" => &mut activity.clean_op_in_flight,
            other => panic!("unknown flag {other}"),
        };
        *slot = true;
        activity
    }

    /// Checks `predicate` against a table: `expected[i]` is its answer when only `FLAGS[i]` is
    /// set. Every predicate is an OR of flags, so single-flag rows plus the all-clear row pin its
    /// exact subset.
    fn assert_table(name: &str, predicate: fn(PersistenceActivity) -> bool, expected: [bool; 8]) {
        assert!(!predicate(PersistenceActivity::default()), "{name}: all clear must not be busy");
        for (flag, want) in FLAGS.iter().zip(expected) {
            assert_eq!(predicate(only(flag)), want, "{name} with only {flag} set");
        }
    }

    #[test]
    fn structural_op_active_reads_page_op_failure_and_reload() {
        assert_table("structural_op_active", PersistenceActivity::structural_op_active, [false, false, true, true, true, false, false, false]);
    }

    #[test]
    fn save_busy_adds_save_running_and_preload() {
        assert_table("save_busy", PersistenceActivity::save_busy, [true, true, true, true, true, false, false, false]);
    }

    #[test]
    fn blocks_page_op_adds_exit_cleanup_and_export_but_not_clean_op() {
        assert_table("blocks_page_op", PersistenceActivity::blocks_page_op, [true, true, true, true, true, true, true, false]);
    }

    #[test]
    fn clean_op_in_flight_reads_only_its_flag() {
        assert_table("clean_op_in_flight", PersistenceActivity::clean_op_in_flight, [false, false, false, false, false, false, false, true]);
    }

    #[test]
    fn exit_cleanup_running_reads_only_its_flag() {
        assert_table("exit_cleanup_running", PersistenceActivity::exit_cleanup_running, [false, false, false, false, false, true, false, false]);
    }

    /// The save's one cause per single flag (saves are blocked by the page-op set).
    #[test]
    fn save_blocker_names_one_cause_per_flag() {
        use SaveBlocker::{CleanOperation, Export, ExitCleanup, PageOperation, SaveInFlight};
        assert_eq!(PersistenceActivity::default().save_blocker(), None);
        let expected = [Some(SaveInFlight), Some(SaveInFlight), Some(PageOperation), Some(PageOperation), Some(PageOperation), Some(ExitCleanup), Some(Export), Some(CleanOperation)];
        for (flag, want) in FLAGS.iter().zip(expected) {
            assert_eq!(only(flag).save_blocker(), want, "save_blocker with only {flag} set");
        }
    }

    /// Over every combination of flags: a save is blocked exactly when a page op is or the clean
    /// worker runs, and the cause follows the fixed priority.
    #[test]
    fn save_blocker_matches_the_page_op_set_on_every_combination() {
        for bits in 0u16..(1 << FLAGS.len()) {
            // Bit `i` sets `FLAGS[i]`.
            let set = |index: usize| bits & (1 << index) != 0;
            let activity = PersistenceActivity {
                save_running: set(0),
                save_pending_preload: set(1),
                page_op_running: set(2),
                page_op_failed: set(3),
                reload_pending: set(4),
                exit_cleanup_running: set(5),
                export_in_progress: set(6),
                clean_op_in_flight: set(7),
            };
            let blocker = activity.save_blocker();
            assert_eq!(blocker.is_some(), activity.blocks_page_op() || activity.clean_op_in_flight(), "flags {bits:#010b}");
            if activity.structural_op_active() {
                assert_eq!(blocker, Some(SaveBlocker::PageOperation), "flags {bits:#010b}");
            }
        }
    }
}
