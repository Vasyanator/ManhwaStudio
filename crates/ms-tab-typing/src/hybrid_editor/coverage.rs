/*
File: hybrid_editor/coverage.rs

Purpose:
The editor's view of the draft's character coverage (`ms_custom_fonts::coverage_report`,
computed on the preview worker after every draft change) and its one-line summary.

Key structures:
- `CoverageView`: unknown / pending / ready / failed.

Key functions:
- `CoverageView::summary`: the localized line under the policy controls.

Notes:
The counts come from the compiler's own character plan (decision D9), so the summary can
never disagree with the face a commit builds.
*/

use ms_custom_fonts::CoverageReport;

/// The draft's coverage as the editor shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum CoverageView {
    /// No valid draft (nothing to count).
    #[default]
    Unknown,
    /// Being counted for the newest draft (no earlier numbers to keep showing).
    Pending,
    /// The newest counted report.
    Ready(CoverageReport),
    /// Counting failed (a source could not be read; logged on the worker).
    Failed,
}

impl CoverageView {
    /// The localized summary line, or `None` when there is nothing to say. GUI thread (`t!`).
    #[must_use]
    pub(crate) fn summary(&self) -> Option<String> {
        match self {
            Self::Unknown | Self::Failed => None,
            Self::Pending => Some(t!("typing.hybrid_font.coverage_pending_status").to_owned()),
            Self::Ready(report) => Some(tf!(
                "typing.hybrid_font.coverage_summary_label",
                total = report.total_chars,
                partial = report.missing_in_some,
                disabled = report.disabled
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_names_all_three_counts() {
        // The active catalog starts empty in a test process (every lookup returns the key):
        // install the embedded reference catalog under the shared locale lock, like the panel
        // tests (the catalog slot is process-global, so tests serialize on it).
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");
        let report = CoverageReport { total_chars: 120, missing_in_some: 7, disabled: 3, disabled_sample: vec!['ж'] };
        let line = CoverageView::Ready(report).summary().expect("a ready report has a summary");
        for count in ["120", "7", "3"] {
            assert!(line.contains(count), "{line}");
        }
        assert!(CoverageView::Unknown.summary().is_none());
        assert!(CoverageView::Failed.summary().is_none());
        assert!(CoverageView::Pending.summary().is_some());
    }
}
