/*
FILE HEADER (widgets/hangul_keyboard.rs)
- Purpose: an on-screen Korean jamo keyboard. It lets the user assemble one
  modern-Hangul syllable from its parts (L = choseong, V = jungseong,
  T = jongseong) or emit single compatibility jamo directly, and reports what the
  user asked for as a typed outcome.
- Key items:
  - `HangulKeyboardMode`: `Compose` (latching keys + explicit Insert) or `Direct`
    (momentary keys that emit one compatibility jamo per click).
  - `HangulKeyboardState`: the persistent latch/mode/placement state one keyboard
    instance owns; pre-latched from an existing syllable with `load_syllable`.
  - `HangulInsertPlacement`: whether the Insert action appends a new syllable or
    replaces the character before the caret — chosen by the user with an explicit
    toggle, reported back but never acted on by this widget.
  - `HangulKeyboardOutcome`: per-frame result (`insert`, `replace_previous`).
  - `show_hangul_keyboard`: draws the keyboard CONTENT into a `Ui`.
- Contract: this widget DRAWS ONLY. It never mutates a string, never touches
  `egui::TextEditState`, and never decides where the text goes — the consumer
  owns the window/area around it, the target field, and the caret. That is what
  makes it reusable by the canvas, the typing tab, or a test binary alike.
- Jamo arithmetic and the caption tables live in `ms_text_util::hangul`; this file
  adds no Hangul knowledge of its own.
- The jamo captions (`ㄱ`, `ㅏ`, …) and the `∅` "no final consonant" marker are
  Unicode DATA, not UI text: they stay literals on purpose and are recorded in
  `dev-docs/i18n_exclusions.md`. Every other caption goes through `t!`.
*/

use eframe::egui;
use ms_text_util::hangul;

/// Size of a single jamo key, in points. The key grid therefore has a fixed
/// width, and the longest row label of any catalog stays comfortably under it, so
/// the panel width does not move when the interface language changes.
const KEY_SIZE: egui::Vec2 = egui::Vec2::new(30.0, 26.0);

/// Number of jamo keys per grid row. 10 keeps the widest row (28 finals) at three
/// rows and the panel narrow enough for a floating window.
const KEYS_PER_ROW: usize = 10;

/// Gap between adjacent jamo keys, in points.
const KEY_SPACING: egui::Vec2 = egui::Vec2::new(2.0, 2.0);

/// Font size of the composed-syllable preview, in points.
const PREVIEW_FONT_SIZE: f32 = 34.0;

/// Caption of the "no final consonant" key (T index 0). A mathematical
/// empty-set sign, not translatable text — see the file header.
const NO_FINAL_CAPTION: &str = "∅";

/// Compatibility jamo that dominate manhwa onomatopoeia, offered as a quick row
/// in [`HangulKeyboardMode::Direct`]. Data, not UI text.
const FREQUENT_JAMO: [char; 8] = ['ㅋ', 'ㅎ', 'ㅠ', 'ㅜ', 'ㅡ', 'ㅅ', 'ㅇ', 'ㄷ'];

/// How the keyboard turns key presses into text.
///
/// `Compose` latches one key per L/V/T row and inserts a single precomposed
/// syllable when the user presses Insert. `Direct` ignores the latches and emits
/// the clicked key's compatibility jamo immediately, which is what manhwa sound
/// text (`ㅋㅋㅋ`, `ㅠㅠ`) is actually made of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HangulKeyboardMode {
    #[default]
    Compose,
    Direct,
}

/// Where the Insert button places the composed syllable relative to the caret.
///
/// `Append` inserts a new syllable at the caret; `ReplacePrevious` asks the
/// consumer to overwrite the character immediately before the caret. The user
/// picks between them with an explicit toggle in the Compose action row; the
/// widget only reports the choice and never computes a text range itself.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HangulInsertPlacement {
    #[default]
    Append,
    ReplacePrevious,
}

/// Persistent UI state of one keyboard instance, owned by the consumer.
///
/// Holds the latched jamo indices, the mode, and the user-selected insert
/// placement (append vs. replace-previous). Switching modes never clears the
/// latches, and the placement changes only through `load_syllable`, `clear`, the
/// action-row toggle, or `set_placement`.
#[derive(Debug, Default)]
pub struct HangulKeyboardState {
    mode: HangulKeyboardMode,
    /// Latched choseong index (`0..hangul::CHOSEONG_COUNT`).
    lead: Option<usize>,
    /// Latched jungseong index (`0..hangul::JUNGSEONG_COUNT`).
    vowel: Option<usize>,
    /// Latched jongseong index (`1..hangul::JONGSEONG_COUNT`). `None` means "no
    /// final consonant", i.e. T index 0 — the index is never stored as `Some(0)`
    /// so that "unlatched" and "explicitly no final" cannot disagree.
    tail: Option<usize>,
    /// Where the next Compose-mode Insert places the syllable. Chosen by the user
    /// via the action-row toggle; `load_syllable` presets it to `ReplacePrevious`.
    placement: HangulInsertPlacement,
}

impl HangulKeyboardState {
    /// Pre-latches the L/V/T rows from a precomposed syllable and sets the insert
    /// placement to [`HangulInsertPlacement::ReplacePrevious`], because opening the
    /// keyboard on an existing syllable means the user is editing it.
    ///
    /// Returns `false` and leaves the state untouched when `c` is not a modern
    /// Hangul syllable (compatibility jamo and Latin are rejected). Ignoring the
    /// returned flag silently treats an untouched state as pre-latched, so it must
    /// be checked.
    #[must_use]
    pub fn load_syllable(&mut self, c: char) -> bool {
        let Some((lead, vowel, tail)) = hangul::decompose(c) else {
            return false;
        };
        self.lead = Some(lead);
        self.vowel = Some(vowel);
        // T index 0 is "no final consonant" and is stored as `None`.
        self.tail = (tail != 0).then_some(tail);
        self.placement = HangulInsertPlacement::ReplacePrevious;
        true
    }

    /// Clears every latch and resets the placement to the default
    /// ([`HangulInsertPlacement::Append`]), returning the state to "nothing
    /// composed yet". The mode is deliberately preserved.
    pub fn clear(&mut self) {
        self.lead = None;
        self.vowel = None;
        self.tail = None;
        self.placement = HangulInsertPlacement::default();
    }

    /// The syllable the latches currently compose, or `None` while the lead
    /// consonant or the vowel is missing (a final consonant alone composes
    /// nothing).
    #[must_use]
    pub fn preview(&self) -> Option<char> {
        let lead = self.lead?;
        let vowel = self.vowel?;
        hangul::compose(lead, vowel, self.tail.unwrap_or(0))
    }

    /// The insert placement the user has selected, i.e. whether a Compose-mode
    /// Insert appends a new syllable or replaces the character before the caret.
    #[must_use]
    pub fn placement(&self) -> HangulInsertPlacement {
        self.placement
    }

    /// Sets the insert placement. Used by the action-row toggle and by consumers
    /// that want to preset the choice.
    pub fn set_placement(&mut self, placement: HangulInsertPlacement) {
        self.placement = placement;
    }

    /// The active input mode.
    #[must_use]
    pub fn mode(&self) -> HangulKeyboardMode {
        self.mode
    }

    /// Switches the input mode. The latches survive the switch.
    pub fn set_mode(&mut self, mode: HangulKeyboardMode) {
        self.mode = mode;
    }

    /// Applies a latching click on the choseong row: a new index latches, the
    /// already-latched index unlatches. The placement is left untouched.
    fn toggle_lead(&mut self, index: usize) {
        self.lead = toggle_latch(self.lead, index);
    }

    /// Applies a latching click on the jungseong row (same toggle rule as the
    /// choseong row).
    fn toggle_vowel(&mut self, index: usize) {
        self.vowel = toggle_latch(self.vowel, index);
    }

    /// Applies a latching click on the jongseong row. Index 0 is the explicit "no
    /// final consonant" key and always clears the latch; any other index toggles.
    fn toggle_tail(&mut self, index: usize) {
        self.tail = if index == 0 {
            None
        } else {
            toggle_latch(self.tail, index)
        };
    }
}

/// Toggle rule shared by the three latch rows: clicking the latched index clears
/// it, clicking any other index moves the latch there.
fn toggle_latch(current: Option<usize>, index: usize) -> Option<usize> {
    if current == Some(index) {
        None
    } else {
        Some(index)
    }
}

/// What the user asked the keyboard for during one frame.
///
/// At most one `insert` is produced per frame. The consumer decides where the
/// text goes; the widget never writes it anywhere.
#[derive(Debug, Default)]
pub struct HangulKeyboardOutcome {
    /// Text the consumer should put into its field, if any.
    pub insert: Option<String>,
    /// True when the user asked to REPLACE the character before the caret; false
    /// to append. Direct-mode jamo always report false. Meaningful only together
    /// with `insert`.
    pub replace_previous: bool,
}

/// Draws the jamo keyboard CONTENT into `ui` and reports what the user asked for.
///
/// The consumer owns the window, area, or panel around this call, and owns the
/// text: this function draws only. It never mutates a string and never touches
/// `egui::TextEditState`.
///
/// Layout: mode toggle, syllable preview, the three latch rows (L/V/T), the
/// frequent-jamo row (Direct mode only), and the action row.
///
/// The action row is drawn in BOTH modes, but its contents differ: `Clear` is
/// always present, `Insert` only in Compose mode. The latches survive a mode
/// switch and the preview keeps showing them, so Direct mode must still offer a
/// way to clear what the preview displays; `Insert` is Compose-only because in
/// Direct mode a key click IS the insert.
///
/// On a Compose insert the widget reports the syllable but does NOT clear its
/// latches — the consumer clears via [`HangulKeyboardState::clear`] after it has
/// applied the text, so a dropped insert (e.g. no target) does not lose the
/// composition. Direct-mode jamo emission likewise never clears the latches.
#[must_use]
pub fn show_hangul_keyboard(
    ui: &mut egui::Ui,
    state: &mut HangulKeyboardState,
) -> HangulKeyboardOutcome {
    let mut outcome = HangulKeyboardOutcome::default();

    draw_mode_row(ui, state);
    ui.separator();
    draw_preview_row(ui, state);
    ui.separator();
    draw_jamo_rows(ui, state, &mut outcome);

    match state.mode {
        HangulKeyboardMode::Compose => {}
        HangulKeyboardMode::Direct => {
            ui.separator();
            draw_frequent_row(ui, &mut outcome);
        }
    }
    // Drawn in both modes: the latches (and therefore the preview) survive a mode
    // switch, so Direct mode needs `Clear` to stay able to explain and empty it.
    ui.separator();
    draw_action_row(ui, state, &mut outcome);

    outcome
}

/// Draws the Compose/Direct toggle. Two selectable labels rather than a combo
/// box: `egui::ComboBox` is forbidden in product UI (`egui-docs/04-widgets.md` §0).
fn draw_mode_row(ui: &mut egui::Ui, state: &mut HangulKeyboardState) {
    ui.horizontal(|ui| {
        let compose = ui.selectable_label(
            state.mode == HangulKeyboardMode::Compose,
            t!("widgets.hangul_keyboard.mode_compose"),
        );
        if compose.clicked() {
            state.set_mode(HangulKeyboardMode::Compose);
        }
        let direct = ui.selectable_label(
            state.mode == HangulKeyboardMode::Direct,
            t!("widgets.hangul_keyboard.mode_direct"),
        );
        if direct.clicked() {
            state.set_mode(HangulKeyboardMode::Direct);
        }
    });
}

/// Draws the preview row: the composed syllable in a large font, or — while the
/// lead consonant or the vowel is missing — the latched jamo greyed out, so the
/// user still sees what is currently held.
fn draw_preview_row(ui: &mut egui::Ui, state: &HangulKeyboardState) {
    ui.vertical_centered(|ui| {
        if let Some(syllable) = state.preview() {
            ui.label(egui::RichText::new(syllable.to_string()).size(PREVIEW_FONT_SIZE));
            return;
        }
        let mut partial = String::new();
        if let Some(caption) = state.lead.and_then(|i| hangul::CHOSEONG_COMPAT.get(i)) {
            partial.push(*caption);
        }
        if let Some(caption) = state.vowel.and_then(|i| hangul::JUNGSEONG_COMPAT.get(i)) {
            partial.push(*caption);
        }
        if let Some(caption) = state
            .tail
            .and_then(|i| hangul::JONGSEONG_COMPAT.get(i))
            .copied()
            .flatten()
        {
            partial.push(caption);
        }
        if partial.is_empty() {
            ui.label(egui::RichText::new(t!("widgets.hangul_keyboard.preview_empty_hint")).weak());
        } else {
            ui.label(egui::RichText::new(partial).size(PREVIEW_FONT_SIZE).weak());
        }
    });
}

/// Draws the three jamo rows and applies their clicks: latching in Compose mode,
/// momentary (one compatibility jamo inserted per click) in Direct mode.
fn draw_jamo_rows(
    ui: &mut egui::Ui,
    state: &mut HangulKeyboardState,
    outcome: &mut HangulKeyboardOutcome,
) {
    let direct = state.mode == HangulKeyboardMode::Direct;

    ui.label(t!("widgets.hangul_keyboard.lead_row_label"));
    let lead_click = draw_key_grid(
        ui,
        "widgets.hangul_keyboard.lead_grid",
        hangul::CHOSEONG_COUNT,
        if direct { None } else { state.lead },
        |index| hangul::CHOSEONG_COMPAT.get(index).copied(),
    );
    if let Some(index) = lead_click {
        if direct {
            emit_jamo(outcome, hangul::CHOSEONG_COMPAT.get(index).copied());
        } else {
            state.toggle_lead(index);
        }
    }

    ui.label(t!("widgets.hangul_keyboard.vowel_row_label"));
    let vowel_click = draw_key_grid(
        ui,
        "widgets.hangul_keyboard.vowel_grid",
        hangul::JUNGSEONG_COUNT,
        if direct { None } else { state.vowel },
        |index| hangul::JUNGSEONG_COMPAT.get(index).copied(),
    );
    if let Some(index) = vowel_click {
        if direct {
            emit_jamo(outcome, hangul::JUNGSEONG_COMPAT.get(index).copied());
        } else {
            state.toggle_vowel(index);
        }
    }

    ui.label(t!("widgets.hangul_keyboard.tail_row_label"));
    // In Compose mode the "no final" key (index 0) is shown as selected exactly
    // when no final consonant is latched, so the row always has one lit key.
    let tail_selected = if direct {
        None
    } else {
        Some(state.tail.unwrap_or(0))
    };
    let tail_click = draw_key_grid(
        ui,
        "widgets.hangul_keyboard.tail_grid",
        hangul::JONGSEONG_COUNT,
        tail_selected,
        |index| hangul::JONGSEONG_COMPAT.get(index).copied().flatten(),
    );
    if let Some(index) = tail_click {
        if direct {
            // The "no final" key has no jamo to emit, so a Direct-mode click on
            // it is a no-op rather than an empty insert.
            emit_jamo(
                outcome,
                hangul::JONGSEONG_COMPAT.get(index).copied().flatten(),
            );
        } else {
            state.toggle_tail(index);
        }
    }
}

/// Draws the Direct-mode quick row of frequent onomatopoeia jamo and emits the
/// clicked one (see [`emit_jamo`]).
fn draw_frequent_row(ui: &mut egui::Ui, outcome: &mut HangulKeyboardOutcome) {
    ui.label(t!("widgets.hangul_keyboard.frequent_row_label"));
    let click = draw_key_grid(
        ui,
        "widgets.hangul_keyboard.frequent_grid",
        FREQUENT_JAMO.len(),
        None,
        |index| FREQUENT_JAMO.get(index).copied(),
    );
    if let Some(index) = click {
        emit_jamo(outcome, FREQUENT_JAMO.get(index).copied());
    }
}

/// Draws the action row. `Clear` is always shown — in Direct mode it is the only
/// way to empty the latches the preview still displays. The replace-previous
/// toggle and `Insert` are Compose-only (a Direct key click already inserts):
/// the toggle chooses whether Insert appends or overwrites the character before
/// the caret, and `Insert` is disabled while the latches compose nothing. On
/// click `Insert` emits the composed syllable plus the chosen placement but does
/// NOT clear the state: the consumer clears via [`HangulKeyboardState::clear`]
/// after it has applied the text, so a dropped insert (e.g. no target) does not
/// lose the composition.
fn draw_action_row(
    ui: &mut egui::Ui,
    state: &mut HangulKeyboardState,
    outcome: &mut HangulKeyboardOutcome,
) {
    ui.horizontal(|ui| {
        if ui
            .button(t!("widgets.hangul_keyboard.clear_button"))
            .clicked()
        {
            state.clear();
        }
        match state.mode {
            HangulKeyboardMode::Compose => {}
            HangulKeyboardMode::Direct => return,
        }
        // Explicit placement toggle: the user, not a heuristic, decides whether
        // Insert replaces the previous character or adds a new one. A checkbox is
        // the clearest binary control here (Slider/ComboBox/DragValue are
        // forbidden in product UI; see egui-docs/04-widgets.md §0).
        let mut is_replace =
            state.placement() == HangulInsertPlacement::ReplacePrevious;
        let toggle = ui
            .checkbox(
                &mut is_replace,
                t!("widgets.hangul_keyboard.replace_previous_toggle"),
            )
            .on_hover_text(t!("widgets.hangul_keyboard.replace_previous_toggle_tooltip"));
        if toggle.changed() {
            state.set_placement(if is_replace {
                HangulInsertPlacement::ReplacePrevious
            } else {
                HangulInsertPlacement::Append
            });
        }
        let preview = state.preview();
        let insert = ui.add_enabled(
            preview.is_some(),
            egui::Button::new(t!("widgets.hangul_keyboard.insert_button")),
        );
        let insert =
            insert.on_disabled_hover_text(t!("widgets.hangul_keyboard.insert_disabled_tooltip"));
        // At most one insert per frame: this button exists only in Compose mode,
        // where no key click emits, so nothing else filled `insert`. The latches
        // are deliberately NOT cleared here: the consumer clears them only after
        // it has applied the text, so a dropped insert keeps the composition.
        if insert.clicked()
            && let Some(syllable) = preview
            && outcome.insert.is_none()
        {
            *outcome = compose_insert_outcome(syllable, state.placement());
        }
    });
}

/// Builds the Compose-mode Insert outcome for a composed `syllable` under the
/// selected `placement`. Pure: it neither mutates nor clears the state — the
/// consumer applies the text and then clears. Split out so the
/// placement→outcome mapping is unit-testable without a `Ui`.
fn compose_insert_outcome(
    syllable: char,
    placement: HangulInsertPlacement,
) -> HangulKeyboardOutcome {
    HangulKeyboardOutcome {
        insert: Some(syllable.to_string()),
        replace_previous: placement == HangulInsertPlacement::ReplacePrevious,
    }
}

/// Records a Direct-mode jamo insertion, keeping the "at most one insert per
/// frame" contract. A `None` caption (the "no final" slot) inserts nothing.
///
/// A Direct-mode jamo always appends: it reports `replace_previous = false`. It
/// does not touch the keyboard state, so the latched syllable the user was
/// building survives a direct insertion.
fn emit_jamo(outcome: &mut HangulKeyboardOutcome, jamo: Option<char>) {
    let Some(jamo) = jamo else {
        return;
    };
    if outcome.insert.is_some() {
        return;
    }
    outcome.insert = Some(jamo.to_string());
    outcome.replace_previous = false;
}

/// Draws one grid of fixed-size jamo keys and returns the index clicked this
/// frame, if any.
///
/// `grid_salt` must be a stable literal (never a localized string — it is a
/// widget identity, not a caption). `selected` lights exactly one key; pass
/// `None` for a momentary row. `caption_of` yields the compatibility jamo for an
/// index, or `None` for the "no final consonant" slot, which is drawn as `∅`
/// with an explanatory tooltip.
fn draw_key_grid(
    ui: &mut egui::Ui,
    grid_salt: &str,
    key_count: usize,
    selected: Option<usize>,
    caption_of: impl Fn(usize) -> Option<char>,
) -> Option<usize> {
    let mut clicked = None;
    egui::Grid::new(grid_salt)
        .spacing(KEY_SPACING)
        .min_col_width(KEY_SIZE.x)
        .show(ui, |ui| {
            for index in 0..key_count {
                let jamo = caption_of(index);
                // `encode_utf8` keeps the caption on the stack: the grids are
                // redrawn every frame and a `String` per key would allocate ~68
                // times per frame for nothing.
                let mut buffer = [0u8; 4];
                let caption: &str = match jamo.as_ref() {
                    Some(c) => c.encode_utf8(&mut buffer),
                    None => NO_FINAL_CAPTION,
                };
                let key = egui::Button::selectable(selected == Some(index), caption);
                let mut response = ui.add_sized(KEY_SIZE, key);
                if jamo.is_none() {
                    response =
                        response.on_hover_text(t!("widgets.hangul_keyboard.no_final_tooltip"));
                }
                if response.clicked() {
                    clicked = Some(index);
                }
                if (index + 1) % KEYS_PER_ROW == 0 {
                    ui.end_row();
                }
            }
        });
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latch_click_selects_then_unselects() {
        let mut state = HangulKeyboardState::default();
        state.toggle_lead(3);
        assert_eq!(state.lead, Some(3));
        state.toggle_lead(3);
        assert_eq!(state.lead, None, "clicking the latched key must unlatch it");
        state.toggle_lead(3);
        state.toggle_lead(5);
        assert_eq!(state.lead, Some(5), "another key moves the latch");
    }

    #[test]
    fn tail_zero_key_means_no_final_consonant() {
        let mut state = HangulKeyboardState::default();
        state.toggle_tail(21);
        assert_eq!(state.tail, Some(21));
        state.toggle_tail(0);
        assert_eq!(state.tail, None, "index 0 always clears the final consonant");
        // Index 0 is idempotent: it never latches as `Some(0)`.
        state.toggle_tail(0);
        assert_eq!(state.tail, None);
    }

    #[test]
    fn preview_needs_both_lead_and_vowel() {
        let mut state = HangulKeyboardState::default();
        assert_eq!(state.preview(), None);
        state.toggle_lead(0);
        assert_eq!(state.preview(), None, "a lead alone composes nothing");
        state.clear();
        state.toggle_vowel(0);
        assert_eq!(state.preview(), None, "a vowel alone composes nothing");
        state.toggle_tail(1);
        assert_eq!(state.preview(), None, "a final alone composes nothing");
        state.toggle_lead(0);
        // ㄱ + ㅏ + ㄱ
        assert_eq!(state.preview(), Some('각'));
    }

    #[test]
    fn preview_without_tail_composes_the_open_syllable() {
        let mut state = HangulKeyboardState::default();
        state.toggle_lead(0);
        state.toggle_vowel(0);
        assert_eq!(state.preview(), Some('가'));
    }

    #[test]
    fn load_syllable_prelatches_and_selects_replace_previous() {
        let mut state = HangulKeyboardState::default();
        assert!(state.load_syllable('각'));
        assert_eq!(
            state.placement(),
            HangulInsertPlacement::ReplacePrevious,
            "opening on an existing syllable means the user is editing it"
        );
        assert_eq!(state.lead, Some(0));
        assert_eq!(state.vowel, Some(0));
        assert_eq!(state.tail, Some(1));
        assert_eq!(state.preview(), Some('각'));
    }

    #[test]
    fn load_syllable_without_final_leaves_the_tail_unlatched() {
        let mut state = HangulKeyboardState::default();
        assert!(state.load_syllable('가'));
        assert_eq!(state.tail, None, "T index 0 must not be stored as Some(0)");
        assert_eq!(state.preview(), Some('가'));
    }

    #[test]
    fn load_syllable_rejects_non_syllables() {
        let mut state = HangulKeyboardState::default();
        state.toggle_lead(2);
        // A compatibility jamo and a Latin letter are not syllables.
        assert!(!state.load_syllable('ㄱ'));
        assert!(!state.load_syllable('A'));
        assert_eq!(
            state.placement(),
            HangulInsertPlacement::Append,
            "a rejected syllable must not preset the placement"
        );
        assert_eq!(
            state.lead,
            Some(2),
            "a rejected syllable must not touch latches"
        );
    }

    #[test]
    fn editing_the_latches_does_not_touch_the_placement() {
        let mut state = HangulKeyboardState::default();
        assert!(state.load_syllable('각'));
        state.toggle_tail(21);
        assert_eq!(
            state.placement(),
            HangulInsertPlacement::ReplacePrevious,
            "changing the final must not silently flip the placement"
        );
        // ㄱ + ㅏ + ㅇ
        assert_eq!(state.preview(), Some('강'));
    }

    #[test]
    fn set_placement_and_placement_round_trip() {
        let mut state = HangulKeyboardState::default();
        assert_eq!(state.placement(), HangulInsertPlacement::Append);
        state.set_placement(HangulInsertPlacement::ReplacePrevious);
        assert_eq!(state.placement(), HangulInsertPlacement::ReplacePrevious);
        state.set_placement(HangulInsertPlacement::Append);
        assert_eq!(state.placement(), HangulInsertPlacement::Append);
    }

    #[test]
    fn clear_resets_latches_and_the_placement() {
        let mut state = HangulKeyboardState::default();
        assert!(state.load_syllable('각'));
        state.clear();
        assert_eq!(state.lead, None);
        assert_eq!(state.vowel, None);
        assert_eq!(state.tail, None);
        assert_eq!(
            state.placement(),
            HangulInsertPlacement::Append,
            "clear must reset the placement to the default"
        );
        assert_eq!(state.preview(), None);
    }

    #[test]
    fn clear_preserves_the_mode() {
        let mut state = HangulKeyboardState::default();
        state.set_mode(HangulKeyboardMode::Direct);
        state.clear();
        assert_eq!(state.mode(), HangulKeyboardMode::Direct);
    }

    #[test]
    fn mode_switch_does_not_destroy_the_latches() {
        let mut state = HangulKeyboardState::default();
        assert!(state.load_syllable('각'));
        state.set_mode(HangulKeyboardMode::Direct);
        assert_eq!(state.mode(), HangulKeyboardMode::Direct);
        assert_eq!(state.preview(), Some('각'));
        state.set_mode(HangulKeyboardMode::Compose);
        assert_eq!(state.preview(), Some('각'));
        assert_eq!(state.placement(), HangulInsertPlacement::ReplacePrevious);
    }

    #[test]
    fn default_state_is_compose_empty_and_appends() {
        let state = HangulKeyboardState::default();
        assert_eq!(state.mode(), HangulKeyboardMode::Compose);
        assert_eq!(state.preview(), None);
        assert_eq!(state.placement(), HangulInsertPlacement::Append);
    }

    #[test]
    fn emit_jamo_keeps_at_most_one_insert_per_frame() {
        let mut outcome = HangulKeyboardOutcome::default();
        emit_jamo(&mut outcome, Some('ㅋ'));
        emit_jamo(&mut outcome, Some('ㅎ'));
        assert_eq!(outcome.insert.as_deref(), Some("ㅋ"));
        assert!(
            !outcome.replace_previous,
            "a direct jamo always appends, never replaces"
        );
    }

    #[test]
    fn emit_jamo_ignores_the_empty_final_slot() {
        let mut outcome = HangulKeyboardOutcome::default();
        emit_jamo(&mut outcome, None);
        assert_eq!(outcome.insert, None);
    }

    #[test]
    fn compose_insert_outcome_reports_the_selected_placement() {
        let append = compose_insert_outcome('각', HangulInsertPlacement::Append);
        assert_eq!(append.insert.as_deref(), Some("각"));
        assert!(!append.replace_previous, "Append must report replace=false");

        let replace = compose_insert_outcome('각', HangulInsertPlacement::ReplacePrevious);
        assert_eq!(replace.insert.as_deref(), Some("각"));
        assert!(
            replace.replace_previous,
            "ReplacePrevious must report replace=true"
        );
    }

    #[test]
    fn frequent_jamo_are_compatibility_jamo() {
        for jamo in FREQUENT_JAMO {
            let code = u32::from(jamo);
            assert!(
                (0x3131..=0x3163).contains(&code),
                "{jamo:?} (U+{code:04X}) is not a compatibility jamo"
            );
            assert!(!hangul::is_syllable(jamo));
        }
    }
}
