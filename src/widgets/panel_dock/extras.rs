/*
File: src/widgets/panel_dock/extras.rs

Purpose:
The dock's generic "extra information" bag: a small piece of UI state that
belongs to a TAB rather than to a panel, is stored by `PanelDockState` and is
persisted next to the arrangement in the `PanelLayout` section of
`user_config.json`.

Main responsibilities:
- hold one drawn tab's named boolean flags for the duration of a frame;
- report whether the body actually CHANGED one of them, so the dock raises its
  `dirty` flag only for a real user change;
- keep the stored footprint minimal: a flag equal to its default leaves no trace.

Key structures:
- `TabExtras`: the bag itself, handed to a tab body as `&mut TabExtras`.

Key functions:
- `TabExtras::flag`, `TabExtras::set_flag`, `TabExtras::changed`,
  `TabExtras::is_empty`.

Notes:
Pure like `model.rs` and `solver.rs`: no egui runtime type, no I/O, no logging,
unit-testable without a window. The first consumer is the expansion state of the
collapsible sections of the typing tab's «Параметры» — state that egui memory
cannot keep for us, because this project builds eframe WITHOUT the `persistence`
feature.
*/

use std::collections::BTreeMap;

/// Extra per-tab UI state the dock stores and persists next to the arrangement.
///
/// A body receives it as `&mut TabExtras` (see `PanelTab::show_with_extras`) and
/// is expected to WRITE what it currently shows on every frame — that is why
/// [`TabExtras::set_flag`] raises [`TabExtras::changed`] only when the stored
/// content really moves.
///
/// The bag holds named booleans only. That is exactly what the feature it exists
/// for needs; another kind of value is an additive sibling field the day one is
/// actually required, not something to generalise for in advance.
#[derive(Debug, Clone, Default)]
pub struct TabExtras {
    /// Flags whose value differs from the default the caller passes in. A flag
    /// sitting at its default is ABSENT here, so an untouched tab stores nothing.
    flags: BTreeMap<String, bool>,
    /// Set by a mutation that really changed [`TabExtras::flags`]. Bookkeeping
    /// for one frame, never content: the dock reads it after the body ran,
    /// raises its own `dirty` flag from it and clears it again.
    changed: bool,
}

/// Compares the CONTENT only.
///
/// Implemented by hand precisely to ignore [`TabExtras::changed`]: that field
/// says whether this frame's body touched the bag, not what the bag holds, and a
/// derived implementation would make the persistence round trip
/// (`stored -> decoded -> encoded`) compare unequal to the live value that fed
/// it for no reason a user could observe.
impl PartialEq for TabExtras {
    fn eq(&self, other: &Self) -> bool {
        self.flags == other.flags
    }
}

impl Eq for TabExtras {}

impl TabExtras {
    /// Builds a bag from already stored flags. Used by persistence on restore;
    /// the result is not marked as changed, because nothing was.
    #[must_use]
    pub(crate) fn from_flags(flags: BTreeMap<String, bool>) -> Self {
        Self {
            flags,
            changed: false,
        }
    }

    /// The flags that differ from their default, for encoding. Never contains a
    /// flag a caller set back to its default.
    #[must_use]
    pub(crate) fn flags(&self) -> &BTreeMap<String, bool> {
        &self.flags
    }

    /// Value of `key`, or `default` when the bag says nothing about it.
    ///
    /// `default` must be the same value the caller passes to
    /// [`TabExtras::set_flag`] for that key: the two together are what keeps a
    /// flag at its default out of the config file.
    #[must_use]
    pub fn flag(&self, key: &str, default: bool) -> bool {
        self.flags.get(key).copied().unwrap_or(default)
    }

    /// Stores `value` under `key` with a MINIMAL footprint: a value equal to
    /// `default` removes the entry instead of writing it.
    ///
    /// [`TabExtras::changed`] is raised only when the stored content really
    /// moved — a write of what is already there changes nothing. Both rules
    /// exist because the expected caller is a widget that calls this every frame
    /// with whatever it currently shows: any other semantics would dirty the
    /// state on the first frame of every session and write the config file for
    /// nothing, and a section returned to its default would keep a stale entry
    /// on disk forever.
    pub fn set_flag(&mut self, key: &str, value: bool, default: bool) {
        if value == default {
            if self.flags.remove(key).is_some() {
                self.changed = true;
            }
            return;
        }
        match self.flags.get_mut(key) {
            Some(stored) if *stored == value => {}
            Some(stored) => {
                *stored = value;
                self.changed = true;
            }
            None => {
                self.flags.insert(key.to_owned(), value);
                self.changed = true;
            }
        }
    }

    /// `true` when a mutation since the last [`TabExtras::clear_changed`] really
    /// changed the content. The dock raises its `dirty` flag from this and from
    /// nothing else, so a body that only READS its extras never causes a write.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.changed
    }

    /// Clears the change flag once the dock has acted on it.
    pub(crate) fn clear_changed(&mut self) {
        self.changed = false;
    }

    /// `true` when nothing is stored — every flag the caller asked about sits at
    /// its default. Such a bag is dropped by the dock and written nowhere.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.flags.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTION: &str = "typing.params.section.font";

    #[test]
    fn a_flag_at_its_default_is_not_stored() {
        let mut extras = TabExtras::default();
        extras.set_flag(SECTION, true, true);
        assert!(extras.is_empty());
        assert!(!extras.changed());
        assert!(extras.flag(SECTION, true));
    }

    #[test]
    fn a_flag_away_from_its_default_is_stored_once() {
        let mut extras = TabExtras::default();
        extras.set_flag(SECTION, false, true);
        assert!(extras.changed());
        assert!(!extras.flag(SECTION, true));
        extras.clear_changed();

        // The widget writes what it shows on every frame; the same value again is
        // not a change and must not wake persistence up.
        extras.set_flag(SECTION, false, true);
        assert!(!extras.changed());
    }

    #[test]
    fn returning_a_flag_to_its_default_removes_it_and_counts_as_a_change() {
        let mut extras = TabExtras::default();
        extras.set_flag(SECTION, false, true);
        extras.clear_changed();

        extras.set_flag(SECTION, true, true);
        assert!(extras.changed());
        assert!(extras.is_empty());
    }

    #[test]
    fn equality_ignores_the_change_flag() {
        let mut written = TabExtras::default();
        written.set_flag(SECTION, false, true);
        let restored = TabExtras::from_flags(written.flags().clone());
        assert!(written.changed());
        assert!(!restored.changed());
        assert_eq!(written, restored);
    }
}
