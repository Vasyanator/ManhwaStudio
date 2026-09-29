/*
File: crates/ms-tab-cleaning/src/clean_status.rs

Purpose:
The status area at the bottom of the «Клин» dock tab: warnings about the chapter's clean folders
that the user cannot otherwise see from the Cleaning tab, each dismissible with a cross.
1. The CURRENT page's committed clean `clean_layers/<source stem>.png` exists but was not loaded
   because its size differs from the source page.
2. A clean file (committed or staging) whose stem matches no source page.
Both point the user at the page manager, which is where such files are fixed.

Key structures:
- `CleanFolderStatus`: tab-owned state — the last scan result, the worker receiver with its
  epoch, the pending-rescan flag, and the session-only dismiss state.
- `CleanStatusView`: the read-only borrow a dock body receives (`CleaningDockCx::clean_status`).
- `CleanStatusMessages` / `SizeMismatchNotice` / `OrphanNotice`: the typed messages to draw.
- `CleanStatusDismiss`: which crosses were clicked this frame (carried out through
  `CleaningDockOut`, applied by `CleaningTabState::apply_dock_out`).

Key functions:
- `CleanFolderStatus::{note_frame, request_rescan, poll, start_scan_if_needed}`: the worker cycle.
- `select_clean_status_messages`: PURE selection of what to show (unit-tested below).
- `draw_clean_status`: draws the messages; draws nothing at all when there are none.

Notes:
- The scan is `ms_models::clean_assign::scan_orphan_cleans`, the same owner of "what is an orphan
  or mismatched clean" the page manager uses, so both tabs agree. It performs directory and
  image-header I/O and therefore only ever runs on an `ms_thread` worker.
- Message 1 mirrors the overlay LOADER, not the scan's broader stem match: the loader only reads
  committed `clean_layers/<stem>.png` and skips it only on an exact size mismatch, so a staging
  file or a same-stem file with another extension is never reported as "not loaded".
- Dismiss state lives only as long as this struct (one opened chapter); nothing is persisted.
*/

use ms_log::runtime_log;
use ms_models::clean_assign::{is_detached_clean_file, scan_orphan_cleans, CleanFileLocation, OrphanClean, OrphanReason};
use ms_project::{Page, ProjectData};
use ms_thread as thread;
use eframe::egui;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Glyph of the dismiss cross. A literal, not a translation: it is an icon chosen for its shape,
/// the same `✕` every other small close button of the studio draws; its meaning is carried by the
/// localized hover text.
const DISMISS_GLYPH: &str = "✕";

/// One finished scan as the worker sends it back: the epoch it was started under and its result.
type ScanReply = (u64, Vec<OrphanClean>);

/// Tab-owned state of the «Клин» status area.
///
/// Scans run one at a time. A rescan requested while one is in flight bumps `epoch`, so the
/// in-flight reply is recognised as stale and dropped, and the next scan starts as soon as it
/// arrives. The previous result stays displayed until a current-epoch result replaces it.
#[derive(Debug)]
pub(crate) struct CleanFolderStatus {
    /// Last current-epoch scan result, sorted by path (the scan's own order).
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
            Ok((epoch, orphans)) => {
                self.scan_rx = None;
                if epoch == self.epoch {
                    runtime_log::log_info(format!(
                        "[cleaning.clean_status] orphan clean scan finished: epoch={epoch}, entries={}",
                        orphans.len()
                    ));
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
    /// flight. `ctx` is repainted by the worker once the reply is sent, so the GUI does not have
    /// to poll with continuous repaints.
    pub(crate) fn start_scan_if_needed(&mut self, ctx: &egui::Context, project: &ProjectData) {
        if !self.rescan_pending || self.scan_rx.is_some() {
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
            let orphans = scan_orphan_cleans(&paths, &pages);
            if tx.send((epoch, orphans)).is_err() {
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

    /// Read-only view of this state for a dock body, resolved against the chapter's `pages`.
    pub(crate) fn view<'a>(&'a self, pages: &'a [Page]) -> CleanStatusView<'a> {
        CleanStatusView { status: self, pages }
    }
}

/// What a dock body needs to derive the status messages: the tab's status state and the page list.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CleanStatusView<'a> {
    status: &'a CleanFolderStatus,
    pages: &'a [Page],
}

impl CleanStatusView<'_> {
    /// The messages to show while `current_page_idx` is the canvas' current page.
    pub(crate) fn messages(&self, current_page_idx: usize) -> CleanStatusMessages {
        let current_page_path = self
            .pages
            .iter()
            .find(|page| page.idx == current_page_idx)
            .map(|page| page.path.as_path());
        select_clean_status_messages(
            &self.status.orphans,
            current_page_idx,
            current_page_path,
            &self.status.dismissed_mismatch_pages,
            self.status.orphan_notice_dismissed,
        )
    }
}

/// The current page's committed clean was skipped by the loader because of its size.
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
/// `orphans` is the scan result in its path order. Message 1 takes the orphan that the overlay
/// loader would have skipped for `current_page_idx`: a COMMITTED `SizeMismatch` of that page whose
/// file name is exactly `<stem of current_page_path>.png`; `None` path or a non-UTF-8 stem yields
/// no message 1. It is hidden when that page is in `dismissed_pages`. Message 2 names the first
/// `NoMatchingPage` orphan (either location) and counts the rest, skipping deliberately detached
/// files (`is_detached_clean_file`); `Unreadable` entries are never reported. It is hidden when `orphan_dismissed`.
#[must_use]
pub(crate) fn select_clean_status_messages(
    orphans: &[OrphanClean],
    current_page_idx: usize,
    current_page_path: Option<&Path>,
    dismissed_pages: &HashSet<usize>,
    orphan_dismissed: bool,
) -> CleanStatusMessages {
    let expected_clean_name = current_page_path
        .and_then(Path::file_stem)
        .and_then(OsStr::to_str)
        .map(|stem| format!("{stem}.png"));
    let size_mismatch = expected_clean_name
        .filter(|_| !dismissed_pages.contains(&current_page_idx))
        .and_then(|expected| {
            orphans.iter().find_map(|orphan| match orphan.reason {
                OrphanReason::SizeMismatch { page_idx, page_size }
                    if page_idx == current_page_idx
                        && orphan.location == CleanFileLocation::Committed
                        && orphan.path.file_name() == Some(OsStr::new(&expected)) =>
                {
                    Some(SizeMismatchNotice { page_idx, clean_size: orphan.size, page_size })
                }
                OrphanReason::SizeMismatch { .. }
                | OrphanReason::NoMatchingPage
                | OrphanReason::Unreadable { .. } => None,
            })
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
    use std::path::PathBuf;

    const CURRENT: usize = 3;

    fn source_path() -> PathBuf {
        PathBuf::from("/chapter/src/004.jpg")
    }

    fn mismatch(path: &str, location: CleanFileLocation, page_idx: usize) -> OrphanClean {
        OrphanClean {
            path: PathBuf::from(path),
            location,
            size: [800, 1200],
            reason: OrphanReason::SizeMismatch { page_idx, page_size: [720, 1080] },
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

    fn select(orphans: &[OrphanClean], dismissed: &HashSet<usize>, orphan_dismissed: bool) -> CleanStatusMessages {
        let source = source_path();
        select_clean_status_messages(orphans, CURRENT, Some(&source), dismissed, orphan_dismissed)
    }

    #[test]
    fn current_page_committed_mismatch_is_shown_with_both_sizes() {
        let orphans = [mismatch("/chapter/clean_layers/004.png", CleanFileLocation::Committed, CURRENT)];
        let messages = select(&orphans, &HashSet::new(), false);
        assert_eq!(
            messages.size_mismatch,
            Some(SizeMismatchNotice { page_idx: CURRENT, clean_size: [800, 1200], page_size: [720, 1080] })
        );
        assert_eq!(messages.orphan, None);
    }

    #[test]
    fn other_page_mismatch_is_not_shown() {
        let orphans = [mismatch("/chapter/clean_layers/005.png", CleanFileLocation::Committed, CURRENT + 1)];
        assert_eq!(select(&orphans, &HashSet::new(), false), CleanStatusMessages::default());
    }

    #[test]
    fn staging_mismatch_is_not_shown() {
        let orphans = [mismatch("/chapter/_unsaved/clean_layers/004.png", CleanFileLocation::Unsaved, CURRENT)];
        assert_eq!(select(&orphans, &HashSet::new(), false).size_mismatch, None);
    }

    #[test]
    fn same_stem_with_other_extension_is_not_shown() {
        let orphans = [
            mismatch("/chapter/clean_layers/004.jpg", CleanFileLocation::Committed, CURRENT),
            mismatch("/chapter/clean_layers/004.PNG", CleanFileLocation::Committed, CURRENT),
        ];
        assert_eq!(select(&orphans, &HashSet::new(), false).size_mismatch, None);
    }

    #[test]
    fn dismissed_page_is_hidden_while_another_page_still_shows() {
        let orphans = [
            mismatch("/chapter/clean_layers/004.png", CleanFileLocation::Committed, CURRENT),
            mismatch("/chapter/clean_layers/006.png", CleanFileLocation::Committed, 5),
        ];
        let dismissed = HashSet::from([CURRENT]);
        assert_eq!(select(&orphans, &dismissed, false).size_mismatch, None);
        let other_source = PathBuf::from("/chapter/src/006.webp");
        let other = select_clean_status_messages(&orphans, 5, Some(&other_source), &dismissed, false);
        assert_eq!(other.size_mismatch.map(|notice| notice.page_idx), Some(5));
    }

    #[test]
    fn missing_current_page_path_yields_no_mismatch_message() {
        let orphans = [mismatch("/chapter/clean_layers/004.png", CleanFileLocation::Committed, CURRENT)];
        let messages = select_clean_status_messages(&orphans, CURRENT, None, &HashSet::new(), false);
        assert_eq!(messages.size_mismatch, None);
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
        let messages = select(&orphans, &HashSet::new(), true);
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
        assert!(tx.send((started_epoch, vec![unmatched("/c/x.png", CleanFileLocation::Committed)])).is_ok());
        status.poll();
        assert!(status.orphans.is_empty(), "a superseded reply must not be applied");
        assert!(status.scan_rx.is_none());
        assert!(status.rescan_pending);
    }

    #[test]
    fn frame_gap_requests_a_rescan_but_consecutive_frames_do_not() {
        let mut status = CleanFolderStatus::default();
        status.rescan_pending = false;
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
