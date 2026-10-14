/*
File: crates/ms-models/src/session_caps.rs

Purpose:
`SessionCapabilities`: what the open studio session OFFERS, as named, typed facts the tab
crates read instead of asking "is this an image set?" themselves. One decision per field, each
with a named consumer; the session kind is mapped onto them in exactly one place, the root's
`SessionMode::capabilities` (`src/studio/session/mod.rs`), which picks one of the two constants.

Key structures:
- `SessionCapabilities` (`Copy`): seven booleans, all `true` in a project chapter, all `false`
  in an image set (whose scratch chapter has no title, no series folder and is deleted on exit).

Key items:
- `SessionCapabilities::PROJECT` / `IMAGE_SET`: the two values a session can have.

Notes:
The root (`StudioApp::new`) computes the value once per studio session — the session kind never
changes during a `StudioApp` lifetime — and pushes it into every tab through one
`set_session_capabilities` setter per tab. `Default` is `PROJECT`, the behaviour every tab had
before a value is pushed. GUI-free plain data.
*/

/// The features the open studio session offers. Each field names one decision and its consumer;
/// `true` means the feature is offered. The root maps the session kind onto [`Self::PROJECT`] or
/// [`Self::IMAGE_SET`] in one place (`SessionMode::capabilities`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionCapabilities {
    /// Title documents (characters, terms, notes template) exist and may be read: the machine
    /// translation project-context toggles and run blocks, and the translation footer's
    /// character roster (`ms-tab-translation`).
    pub title_documents: bool,
    /// Chapter artifacts that must outlive the session may be stored: the translation tab's
    /// `text_detection/` storage (Save and the lazy auto-load).
    pub chapter_storage: bool,
    /// The cleaning tab's clean-folder scan (orphan and foreign-size cleans, whose messages
    /// point at the page manager) runs.
    pub clean_scan: bool,
    /// The watermark tool's chapter mode and its library, keyed by the series folder, are
    /// offered (`ms-tab-cleaning` watermark removal).
    pub series_watermark_library: bool,
    /// Alternative page versions (`alt_vers` folders) exist: the stamp tool's alt-version mode.
    pub alt_versions: bool,
    /// The typing tab's project export (format row, re-pagination block, export button).
    pub project_export: bool,
    /// Project-level settings are editable: the Settings canvas pane's comic-type preset and
    /// project word list, and the project spellcheck dictionary target.
    pub project_settings: bool,
}

impl SessionCapabilities {
    /// A regular chapter of a title on disk: every feature is offered.
    pub const PROJECT: Self = Self {
        title_documents: true,
        chapter_storage: true,
        clean_scan: true,
        series_watermark_library: true,
        alt_versions: true,
        project_export: true,
        project_settings: true,
    };

    /// An image set edited through a throwaway scratch chapter: no title, no series folder, no
    /// lasting chapter, so none of the chapter- or title-level features is offered.
    pub const IMAGE_SET: Self = Self {
        title_documents: false,
        chapter_storage: false,
        clean_scan: false,
        series_watermark_library: false,
        alt_versions: false,
        project_export: false,
        project_settings: false,
    };
}

impl Default for SessionCapabilities {
    /// [`Self::PROJECT`]: what every tab offers before a value is pushed.
    fn default() -> Self {
        Self::PROJECT
    }
}

#[cfg(test)]
mod tests {
    use super::SessionCapabilities;

    /// Before a value is pushed a tab behaves as in a project, and the two sessions differ in
    /// every field (each one is a chapter- or title-level feature).
    #[test]
    fn the_default_is_the_project_session_and_an_image_set_offers_nothing() {
        assert_eq!(SessionCapabilities::default(), SessionCapabilities::PROJECT);
        let SessionCapabilities { title_documents, chapter_storage, clean_scan, series_watermark_library, alt_versions, project_export, project_settings } = SessionCapabilities::PROJECT;
        assert!(title_documents && chapter_storage && clean_scan && series_watermark_library && alt_versions && project_export && project_settings);
        let SessionCapabilities { title_documents, chapter_storage, clean_scan, series_watermark_library, alt_versions, project_export, project_settings } = SessionCapabilities::IMAGE_SET;
        assert!(!(title_documents || chapter_storage || clean_scan || series_watermark_library || alt_versions || project_export || project_settings));
    }
}
