/*
File: crates/ms-tab-cleaning/src/clean_status.rs

Purpose:
The status area at the bottom of the «Клин» dock tab: warnings about the chapter's clean folders
that the user cannot otherwise see from the Cleaning tab, each dismissible with a cross.
1. The CURRENT page's effective clean `<source stem>.png` (the staged one when present, else the
   committed one) exists but was not loaded because its size differs from the source page.
2. A clean file (committed or staging) whose name is not the canonical clean name of any page.
Both point the user at the page manager, which is where such files are fixed.

Key structures:
- `CleanFolderStatus`: tab-owned state — the last scan result (inventory + its orphan projection),
  the worker receiver with its epoch, the pending-rescan flag, and the session-only dismiss state.
- `CleanStatusView`: the read-only borrow a dock body receives (`CleaningDockCx::clean_status`).
- `CleanStatusMessages` / `SizeMismatchNotice` / `OrphanNotice`: the typed messages to draw.
- `CleanStatusDismiss`: which crosses were clicked this frame (carried out through
  `CleaningDockOut`, applied by `CleaningTabState::apply_dock_out`).

Key functions:
- `CleanFolderStatus::{note_frame, request_rescan, poll, start_scan_if_needed}`: the worker cycle.
- `select_clean_status_messages`: PURE selection of what to show (unit-tested below).
- `draw_clean_status`: draws the messages; draws nothing at all when there are none.

Notes:
- The scan is `ms_models::clean_assign::scan_clean_inventory`; message 2 reads its orphan
  projection (`orphans_from_inventory`, the same list `scan_orphan_cleans` gives the page manager),
  so both tabs agree. The scan performs directory and image-header I/O and therefore only ever runs
  on an `ms_thread` worker.
- Message 1 mirrors the overlay LOADER exactly: it resolves the current page's inventory entry in
  the loader's scope (`LOADER_SCOPE` = `clean_assign::LOADER_CLEAN_SCOPE`, i.e. `StagedOverCommitted`) and reports only a
  `SizeMismatch` of that effective file. A mismatched committed file shadowed by a fitting staged
  twin is therefore not reported, a mismatched staged file is, and a same-stem file with another
  extension is never the page's clean at all.
- Dismiss state lives only as long as this struct (one opened chapter); nothing is persisted.
- No scan runs in a single-image session (`clean_scan_should_start`): its scratch chapter has one
  page whose clean the session writes itself, and both messages point at the page manager, which
  that mode hides.
*/

use ms_log::runtime_log;
use ms_models::clean_assign::{
    is_detached_clean_file, orphans_from_inventory, LOADER_CLEAN_SCOPE, scan_clean_inventory, CleanInventory, CleanTreeScope, OrphanClean, OrphanReason,
    PageCleanEntry, PageCleanResolution,
};
use ms_project::ProjectData;
use ms_thread as thread;
use eframe::egui;
use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// The clean-tree scope the overlay loader (`src/app.rs`, `loadable_clean_overlay`) resolves a
/// page's clean in; message 1 must use the same one, since it reports what the loader skipped.
const LOADER_SCOPE: CleanTreeScope = LOADER_CLEAN_SCOPE;

/// Glyph of the dismiss cross. A literal, not a translation: it is an icon chosen for its shape,
/// the same `✕` every other small close button of the studio draws; its meaning is carried by the
/// localized hover text.
const DISMISS_GLYPH: &str = "✕";

/// One finished scan as the worker sends it back: the epoch it was started under and its result.
type ScanReply = (u64, CleanInventory);

/// Tab-owned state of the «Клин» status area.
///
/// Scans run one at a time. A rescan requested while one is in flight bumps `epoch`, so the
/// in-flight reply is recognised as stale and dropped, and the next scan starts as soon as it
/// arrives. The previous result stays displayed until a current-epoch result replaces it.
#[derive(Debug)]
pub(crate) struct CleanFolderStatus {
    /// Last current-epoch scan result.
    inventory: CleanInventory,
    /// `orphans_from_inventory(&inventory)`, projected once per accepted scan (sorted by path).
    orphans: Vec<OrphanClean>,
    /// Receiver of the scan in flight, if any.
    scan_rx: Option<Receiver<ScanReply>>,
    /// Epoch a result must carry to be accepted; bumped by every rescan request.
    epoch: u64,
    /// A scan must start as soon as none is in flight.
    rescan_pending: bool,
    /// `egui::Context::cumulative_frame_nr` of the last frame the tab was drawn in.
    last_drawn_frame: Option<u64>,
    /// Pages whose size-mismatch message the user dismissed this session.
    dismissed_mismatch_pages: HashSet<usize>,
    /// The orphan-file message was dismissed; it never comes back for this chapter session.
    orphan_notice_dismissed: bool,
}

impl Default for CleanFolderStatus {
    fn default() -> Self {
        Self {
            inventory: CleanInventory::default(),
            orphans: Vec::new(),
            scan_rx: None,
            epoch: 0,
            // Lazy first scan: the first draw of the tab starts it.
            rescan_pending: true,
            last_drawn_frame: None,
            dismissed_mismatch_pages: HashSet::new(),
            orphan_notice_dismissed: false,
        }
    }
}

impl CleanFolderStatus {
    /// Asks for a fresh scan. Any scan already in flight is superseded: its reply is dropped.
    pub(crate) fn request_rescan(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.rescan_pending = true;
    }

    /// Records that the tab is drawn in frame `frame_nr` and requests a rescan when the tab was
    /// NOT drawn in the frame before, i.e. the user has just switched into the Cleaning tab.
    ///
    /// Detecting the entry here rather than in `app.rs` covers every way into the tab (tab bar,
    /// "open page in tab", a return from the page manager, which is where clean files are
    /// attached or trashed) without the app having to track the previous tab for this.
    pub(crate) fn note_frame(&mut self, frame_nr: u64) {
        let entered = self
            .last_drawn_frame
            .is_some_and(|last| frame_nr > last.saturating_add(1));
        if entered {
            self.request_rescan();
        }
        self.last_drawn_frame = Some(frame_nr);
    }

    /// Applies a finished scan, if one arrived. Never blocks.
    pub(crate) fn poll(&mut self) {
        let Some(rx) = self.scan_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((epoch, inventory)) => {
                self.scan_rx = None;
                if epoch == self.epoch {
                    let orphans = orphans_from_inventory(&inventory);
                    runtime_log::log_info(format!(
                        "[cleaning.clean_status] orphan clean scan finished: epoch={epoch}, entries={}",
                        orphans.len()
                    ));
                    self.inventory = inventory;
                    self.orphans = orphans;
                } else {
                    runtime_log::log_info(format!(
                        "[cleaning.clean_status] dropped a superseded orphan clean scan: epoch={epoch}, current={}",
                        self.epoch
                    ));
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // The worker ended without replying (it panicked inside the scan). Keep the last
                // result on screen; do not retry on our own, which would loop on a scan that
                // keeps failing — the next explicit rescan request tries again.
                self.scan_rx = None;
                runtime_log::log_error(format!(
                    "[cleaning.clean_status] orphan clean scan worker exited without a result: epoch={}. \
                     Possible cause: the scan panicked; the previous status stays displayed.",
                    self.epoch
                ));
            }
        }
    }

    /// Starts a scan of `project`'s clean folders on a worker when one is pending and none is in
    /// flight (`clean_scan_should_start`). `ctx` is repainted by the worker once the reply is
    /// sent, so the GUI does not have to poll with continuous repaints.
    pub(crate) fn start_scan_if_needed(&mut self, ctx: &egui::Context, project: &ProjectData) {
        if !clean_scan_should_start(self.rescan_pending, self.scan_rx.is_some(), project.is_single_image()) {
            return;
        }
        self.rescan_pending = false;
        let epoch = self.epoch;
        // Snapshots: the worker must not borrow the live project.
        let paths = project.paths.clone();
        let pages = project.pages.clone();
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel::<ScanReply>();
        self.scan_rx = Some(rx);
        runtime_log::log_info(format!(
            "[cleaning.clean_status] orphan clean scan started: epoch={epoch}, pages={}",
            pages.len()
        ));
        thread::spawn(move || {
            let inventory = scan_clean_inventory(&paths, &pages);
            if tx.send((epoch, inventory)).is_err() {
                // The tab (and its receiver) is gone, or a newer scan replaced it: nothing to show.
                runtime_log::log_info(format!(
                    "[cleaning.clean_status] orphan clean scan result discarded: receiver gone, epoch={epoch}"
                ));
            }
            ctx.request_repaint();
        });
    }

    /// Hides the size-mismatch message of `page_idx` for the rest of this chapter session.
    pub(crate) fn dismiss_mismatch_page(&mut self, page_idx: usize) {
        self.dismissed_mismatch_pages.insert(page_idx);
    }

    /// Hides the orphan-file message for the rest of this chapter session.
    pub(crate) fn dismiss_orphan_notice(&mut self) {
        self.orphan_notice_dismissed = true;
    }

    /// Read-only view of this state for a dock body.
    pub(crate) fn view(&self) -> CleanStatusView<'_> {
        CleanStatusView { status: self }
    }
}

/// Whether a clean-folder scan starts this frame: one is pending, none is in flight, and the
/// project is not a single-image session. A single-image scratch chapter has exactly one page
/// whose clean the session itself writes, so there is no orphan or foreign-size clean to find,
/// and both messages point at the page manager, which that mode does not show.
fn clean_scan_should_start(rescan_pending: bool, scan_in_flight: bool, single_image: bool) -> bool {
    rescan_pending && !scan_in_flight && !single_image
}

/// What a dock body needs to derive the status messages: the tab's status state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CleanStatusView<'a> {
    status: &'a CleanFolderStatus,
}

impl CleanStatusView<'_> {
    /// The messages to show while `current_page_idx` is the canvas' current page.
    pub(crate) fn messages(&self, current_page_idx: usize) -> CleanStatusMessages {
        select_clean_status_messages(
            &self.status.inventory.pages,
            &self.status.orphans,
            current_page_idx,
            &self.status.dismissed_mismatch_pages,
            self.status.orphan_notice_dismissed,
        )
    }
}

/// The current page's effective clean was skipped by the loader because of its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SizeMismatchNotice {
    pub(crate) page_idx: usize,
    /// `[width, height]` of the clean file, in pixels.
    pub(crate) clean_size: [u32; 2],
    /// `[width, height]` of the source page, in pixels.
    pub(crate) page_size: [u32; 2],
}

/// A clean file that belongs to no page, plus how many more such files exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrphanNotice {
    /// File name only (no directory), lossily converted for display.
    pub(crate) file_name: String,
    /// Further orphan files beyond the one named.
    pub(crate) more_count: usize,
}

/// Everything the status area shows this frame; both `None` means it is not drawn at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CleanStatusMessages {
    pub(crate) size_mismatch: Option<SizeMismatchNotice>,
    pub(crate) orphan: Option<OrphanNotice>,
}

/// Crosses clicked this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CleanStatusDismiss {
    /// Page whose size-mismatch message was dismissed.
    pub(crate) mismatch_page: Option<usize>,
    /// The orphan-file message was dismissed.
    pub(crate) orphan: bool,
}

/// Picks the status messages from a scan result. Pure.
///
/// `pages` is the scan's per-page inventory and `orphans` its orphan projection in path order.
/// Message 1 reports what the overlay loader skipped for `current_page_idx`: that page's entry
/// resolved in [`LOADER_SCOPE`] is a `SizeMismatch` (the first entry with that index is used; no
/// entry yields no message 1). It is hidden when that page is in `dismissed_pages`. Message 2 names
/// the first `NoMatchingPage` orphan (either location) and counts the rest, skipping deliberately
/// detached files (`is_detached_clean_file`); `Unreadable` entries are never reported. It is hidden
/// when `orphan_dismissed`.
#[must_use]
pub(crate) fn select_clean_status_messages(
    pages: &[PageCleanEntry],
    orphans: &[OrphanClean],
    current_page_idx: usize,
    dismissed_pages: &HashSet<usize>,
    orphan_dismissed: bool,
) -> CleanStatusMessages {
    let size_mismatch = pages
        .iter()
        .find(|entry| entry.page_idx == current_page_idx)
        .filter(|_| !dismissed_pages.contains(&current_page_idx))
        .and_then(|entry| match entry.resolve(LOADER_SCOPE) {
            PageCleanResolution::SizeMismatch { clean, page, .. } => {
                Some(SizeMismatchNotice { page_idx: current_page_idx, clean_size: clean, page_size: page })
            }
            PageCleanResolution::Absent
            | PageCleanResolution::Bound { .. }
            | PageCleanResolution::CleanUnreadable { .. }
            | PageCleanResolution::PageUnreadable { .. } => None,
        });

    let orphan = if orphan_dismissed {
        None
    } else {
        let mut unmatched = orphans
            .iter()
            // A `*_detached` file was set aside on purpose; it is not an unassigned-file problem.
            .filter(|orphan| matches!(orphan.reason, OrphanReason::NoMatchingPage) && !is_detached_clean_file(&orphan.path));
        unmatched.next().map(|first| OrphanNotice {
            file_name: first
                .path
                .file_name()
                .map_or_else(|| first.path.display().to_string(), |name| name.to_string_lossy().into_owned()),
            more_count: unmatched.count(),
        })
    };

    CleanStatusMessages { size_mismatch, orphan }
}

/// Draws the status area of the «Клин» tab and reports which crosses were clicked.
///
/// Draws NOTHING — no spacing, no separator — when `messages` is empty. Each message is small,
/// warning-coloured text wrapped to the width left of its cross.
pub(crate) fn draw_clean_status(ui: &mut egui::Ui, messages: &CleanStatusMessages) -> CleanStatusDismiss {
    let mut dismiss = CleanStatusDismiss::default();
    if let Some(notice) = &messages.size_mismatch {
        let text = tf!(
            "cleaning.tab.clean_size_mismatch_status",
            clean_width = notice.clean_size[0],
            clean_height = notice.clean_size[1],
            page_width = notice.page_size[0],
            page_height = notice.page_size[1]
        );
        if draw_dismissible_warning(ui, "cleaning.clean_status.size_mismatch", text) {
            dismiss.mismatch_page = Some(notice.page_idx);
        }
    }
    if let Some(notice) = &messages.orphan {
        let text = if notice.more_count == 0 {
            tf!("cleaning.tab.orphan_clean_status", file = notice.file_name)
        } else {
            let more = tp!("cleaning.tab.orphan_clean_more", notice.more_count);
            tf!("cleaning.tab.orphan_clean_status_many", file = notice.file_name, more = more)
        };
        if draw_dismissible_warning(ui, "cleaning.clean_status.orphan", text) {
            dismiss.orphan = true;
        }
    }
    dismiss
}

/// One message row: the cross on the right edge, the text wrapping in the width left of it.
/// Returns whether the cross was clicked. `id_salt` keeps the row's ids independent of the
/// localized text.
fn draw_dismissible_warning(ui: &mut egui::Ui, id_salt: &'static str, text: String) -> bool {
    ui.push_id(id_salt, |ui| {
        // Right-to-left so the cross claims its width first; the text then gets exactly the
        // remaining width. The nested top-down layout left-aligns the wrapped rows, which a label
        // placed straight into the right-to-left layout would right-align.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            let clicked = ui
                .small_button(DISMISS_GLYPH)
                .on_hover_text(t!("cleaning.tab.clean_status_dismiss_tooltip"))
                .clicked();
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(text).small().color(ms_theme::status::WARNING))
                        .wrap(),
                );
            });
            clicked
        })
        .inner
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_models::clean_assign::{CleanFileLocation, CleanFileProbe, UnassignedClean};
    use std::path::PathBuf;

    /// A scan starts only when pending and idle, and never in a single-image session.
    #[test]
    fn a_clean_scan_never_starts_in_a_single_image_session() {
        assert!(clean_scan_should_start(true, false, false));
        assert!(!clean_scan_should_start(true, false, true));
        assert!(!clean_scan_should_start(false, false, false));
        assert!(!clean_scan_should_start(true, true, false));
    }

    const CURRENT: usize = 3;
    const PAGE_SIZE: [u32; 2] = [720, 1080];
    const WRONG_SIZE: [u32; 2] = [800, 1200];

    /// A canonical clean probe of page `page_idx` in `location` with header size `size`.
    fn probe(page_idx: usize, location: CleanFileLocation, size: [u32; 2]) -> CleanFileProbe {
        let dir = match location {
            CleanFileLocation::Committed => "/chapter/clean_layers",
            CleanFileLocation::Unsaved => "/chapter_unsaved/clean_layers",
        };
        CleanFileProbe { path: PathBuf::from(format!("{dir}/{:03}.png", page_idx + 1)), location, size: Ok(size) }
    }

    /// Inventory entry of page `page_idx` (source size [`PAGE_SIZE`]) with the given canonical
    /// clean sizes per tree.
    fn entry(page_idx: usize, committed: Option<[u32; 2]>, staged: Option<[u32; 2]>) -> PageCleanEntry {
        PageCleanEntry {
            page_idx,
            page_size: Ok(PAGE_SIZE),
            committed: committed.map(|size| probe(page_idx, CleanFileLocation::Committed, size)),
            staged: staged.map(|size| probe(page_idx, CleanFileLocation::Unsaved, size)),
        }
    }

    fn mismatch(path: &str, location: CleanFileLocation, page_idx: usize) -> OrphanClean {
        OrphanClean {
            path: PathBuf::from(path),
            location,
            size: WRONG_SIZE,
            reason: OrphanReason::SizeMismatch { page_idx, page_size: PAGE_SIZE },
        }
    }

    fn unmatched(path: &str, location: CleanFileLocation) -> OrphanClean {
        OrphanClean { path: PathBuf::from(path), location, size: [10, 10], reason: OrphanReason::NoMatchingPage }
    }

    fn unreadable(path: &str) -> OrphanClean {
        OrphanClean {
            path: PathBuf::from(path),
            location: CleanFileLocation::Committed,
            size: [0, 0],
            reason: OrphanReason::Unreadable { error: "bad header".to_owned() },
        }
    }

    fn select_mismatch(pages: &[PageCleanEntry], current: usize, dismissed: &HashSet<usize>) -> Option<SizeMismatchNotice> {
        select_clean_status_messages(pages, &[], current, dismissed, false).size_mismatch
    }

    fn select(orphans: &[OrphanClean], dismissed: &HashSet<usize>, orphan_dismissed: bool) -> CleanStatusMessages {
        select_clean_status_messages(&[], orphans, CURRENT, dismissed, orphan_dismissed)
    }

    #[test]
    fn current_page_committed_mismatch_is_shown_with_both_sizes() {
        let pages = [entry(CURRENT, Some(WRONG_SIZE), None)];
        assert_eq!(
            select_mismatch(&pages, CURRENT, &HashSet::new()),
            Some(SizeMismatchNotice { page_idx: CURRENT, clean_size: WRONG_SIZE, page_size: PAGE_SIZE })
        );
    }

    #[test]
    fn other_page_mismatch_is_not_shown() {
        let pages = [entry(CURRENT + 1, Some(WRONG_SIZE), None)];
        assert_eq!(select_mismatch(&pages, CURRENT, &HashSet::new()), None);
    }

    #[test]
    fn staged_mismatch_is_shown_because_the_loader_reads_staging_first() {
        let pages = [entry(CURRENT, None, Some(WRONG_SIZE))];
        assert_eq!(select_mismatch(&pages, CURRENT, &HashSet::new()).map(|notice| notice.clean_size), Some(WRONG_SIZE));
        // A mismatched staged twin shadows a fitting committed file: the loader skips the page.
        let shadowing = [entry(CURRENT, Some(PAGE_SIZE), Some(WRONG_SIZE))];
        assert!(select_mismatch(&shadowing, CURRENT, &HashSet::new()).is_some());
    }

    #[test]
    fn fitting_staged_twin_hides_a_mismatched_committed_file() {
        let pages = [entry(CURRENT, Some(WRONG_SIZE), Some(PAGE_SIZE))];
        assert_eq!(select_mismatch(&pages, CURRENT, &HashSet::new()), None);
    }

    #[test]
    fn message_one_follows_the_inventory_not_the_orphan_list() {
        // A `SizeMismatch` orphan without a mismatched effective file (e.g. the committed copy of a
        // page whose staged twin fits) never produces message 1.
        let orphans = [mismatch("/chapter/clean_layers/004.png", CleanFileLocation::Committed, CURRENT)];
        let pages = [entry(CURRENT, Some(WRONG_SIZE), Some(PAGE_SIZE))];
        assert_eq!(select_clean_status_messages(&pages, &orphans, CURRENT, &HashSet::new(), false), CleanStatusMessages::default());
    }

    #[test]
    fn dismissed_page_is_hidden_while_another_page_still_shows() {
        let pages = [entry(CURRENT, Some(WRONG_SIZE), None), entry(5, Some(WRONG_SIZE), None)];
        let dismissed = HashSet::from([CURRENT]);
        assert_eq!(select_mismatch(&pages, CURRENT, &dismissed), None);
        assert_eq!(select_mismatch(&pages, 5, &dismissed).map(|notice| notice.page_idx), Some(5));
    }

    #[test]
    fn missing_current_page_entry_yields_no_mismatch_message() {
        assert_eq!(select_mismatch(&[], CURRENT, &HashSet::new()), None);
    }

    #[test]
    fn unreadable_clean_or_page_is_not_a_size_mismatch() {
        let mut clean_broken = entry(CURRENT, Some(WRONG_SIZE), None);
        if let Some(committed) = clean_broken.committed.as_mut() {
            committed.size = Err("bad header".to_owned());
        }
        assert_eq!(select_mismatch(&[clean_broken], CURRENT, &HashSet::new()), None);
        let mut page_broken = entry(CURRENT, Some(WRONG_SIZE), None);
        page_broken.page_size = Err("bad page".to_owned());
        assert_eq!(select_mismatch(&[page_broken], CURRENT, &HashSet::new()), None);
    }

    #[test]
    fn first_unmatched_by_path_order_is_named_and_the_rest_counted() {
        let orphans = [
            unmatched("/chapter/_unsaved/clean_layers/aaa.png", CleanFileLocation::Unsaved),
            unmatched("/chapter/clean_layers/extra.png", CleanFileLocation::Committed),
            unmatched("/chapter/clean_layers/zzz.png", CleanFileLocation::Committed),
        ];
        let messages = select(&orphans, &HashSet::new(), false);
        assert_eq!(messages.orphan, Some(OrphanNotice { file_name: "aaa.png".to_owned(), more_count: 2 }));
    }

    #[test]
    fn single_unmatched_has_no_extra_count() {
        let orphans = [unmatched("/chapter/clean_layers/extra.png", CleanFileLocation::Committed)];
        let messages = select(&orphans, &HashSet::new(), false);
        assert_eq!(messages.orphan, Some(OrphanNotice { file_name: "extra.png".to_owned(), more_count: 0 }));
    }

    #[test]
    fn detached_files_are_not_reported_as_unassigned() {
        let orphans = [
            unmatched("/chapter/clean_layers/003_detached.png", CleanFileLocation::Committed),
            unmatched("/chapter/clean_layers/004_DETACHED.png", CleanFileLocation::Committed),
            unmatched("/chapter/clean_layers/extra.png", CleanFileLocation::Committed),
        ];
        let messages = select(&orphans, &HashSet::new(), false);
        assert_eq!(messages.orphan, Some(OrphanNotice { file_name: "extra.png".to_owned(), more_count: 0 }));
        let only_detached = [unmatched("/chapter/clean_layers/003_detached.png", CleanFileLocation::Committed)];
        assert_eq!(select(&only_detached, &HashSet::new(), false), CleanStatusMessages::default());
    }

    #[test]
    fn unreadable_is_ignored() {
        let orphans = [unreadable("/chapter/clean_layers/Thumbs.db"), unreadable("/chapter/clean_layers/004.png")];
        assert_eq!(select(&orphans, &HashSet::new(), false), CleanStatusMessages::default());
    }

    #[test]
    fn dismissed_orphan_notice_stays_hidden() {
        let orphans = [
            unmatched("/chapter/clean_layers/extra.png", CleanFileLocation::Committed),
            mismatch("/chapter/clean_layers/004.png", CleanFileLocation::Committed, CURRENT),
        ];
        let pages = [entry(CURRENT, Some(WRONG_SIZE), None)];
        let messages = select_clean_status_messages(&pages, &orphans, CURRENT, &HashSet::new(), true);
        assert_eq!(messages.orphan, None);
        assert!(messages.size_mismatch.is_some());
    }

    #[test]
    fn rescan_request_supersedes_the_scan_in_flight() {
        let mut status = CleanFolderStatus::default();
        let (tx, rx) = mpsc::channel::<ScanReply>();
        status.scan_rx = Some(rx);
        status.rescan_pending = false;
        let started_epoch = status.epoch;
        status.request_rescan();
        let stale = CleanInventory {
            pages: vec![entry(CURRENT, Some(WRONG_SIZE), None)],
            unassigned: vec![UnassignedClean {
                file_name: "x.png".into(),
                committed: Some(CleanFileProbe {
                    path: PathBuf::from("/c/x.png"),
                    location: CleanFileLocation::Committed,
                    size: Ok([1, 1]),
                }),
                staged: None,
            }],
        };
        assert!(tx.send((started_epoch, stale)).is_ok());
        status.poll();
        assert!(status.orphans.is_empty(), "a superseded reply must not be applied");
        assert!(status.inventory.pages.is_empty());
        assert!(status.scan_rx.is_none());
        assert!(status.rescan_pending);
    }

    #[test]
    fn frame_gap_requests_a_rescan_but_consecutive_frames_do_not() {
        let mut status = CleanFolderStatus { rescan_pending: false, ..CleanFolderStatus::default() };
        status.note_frame(10);
        assert!(!status.rescan_pending, "the first draw is covered by the initial pending flag");
        status.note_frame(11);
        assert!(!status.rescan_pending);
        status.note_frame(11);
        assert!(!status.rescan_pending, "a second pass in the same frame is not an entry");
        status.note_frame(20);
        assert!(status.rescan_pending);
    }
}
