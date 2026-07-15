/*
File: panel/effect_defaults.rs

Purpose:
User-configurable DEFAULT parameter values for the typing tab's effect cards. When a
new effect card is added in the "Эффекты" panel, it is pre-filled from a stored per-kind
default if one exists (see `create_sections::draw_effects_section`); otherwise the
built-in `TypingCreatePanelState::default_effect_card` is used.

Main responsibilities:
- own a thread-safe runtime-global store of per-effect-kind default overrides, keyed by
  the effect discriminator string (`effect_kind_key`);
- seed that store from `TextTab.effect_defaults` at startup
  (`seed_effect_defaults_from_config`);
- resolve an add-time default card for a kind (`effect_default_card`);
- provide `EffectDefaultsEditorState`, the dedicated editor widget rendered from the
  settings pane, which edits/resets the defaults and persists them off the GUI thread.

Key types:
- `EffectDefaultsEditorState`

Key functions:
- `effect_default_value` / `all_effect_defaults` / `set_effect_default_value` /
  `clear_effect_default_value` (store access)
- `seed_effect_defaults_from_config`
- `effect_default_card`
- `effect_kind_key`

Notes:
`use super::*;` pulls in the parent `panel` module's types and imports (effect cards,
`parse_effect_cards`, `effect_card_to_value`, `draw_effect_card_controls`, the
`presets_io` load/save helpers, `thread` = `ms_thread`). The store is a plain
`OnceLock<RwLock<..>>`; it is not on any hot path, so no generation cache is needed.
*/

use super::*;
use std::sync::{OnceLock, RwLock};

/// Neutral text color used when materializing a default card in the editor. The editor
/// has no active overlay, so color-follows-text-color fields (DryMedia/Gradient targets)
/// resolve against this fixed color. Serialized cards always carry an explicit color, so
/// this only affects the built-in-fallback card's initial appearance.
const NEUTRAL_EDITOR_COLOR: Color32 = Color32::BLACK;

/// Every effect kind, in the same order the "Добавить эффект" combo lists them.
const ALL_EFFECT_KINDS: [AvailableEffectKind; 14] = [
    AvailableEffectKind::TextShake,
    AvailableEffectKind::Stroke,
    AvailableEffectKind::Shadow,
    AvailableEffectKind::Blur,
    AvailableEffectKind::MotionBlur,
    AvailableEffectKind::DryMedia,
    AvailableEffectKind::Interference,
    AvailableEffectKind::GlowV1,
    AvailableEffectKind::GlowV2,
    AvailableEffectKind::SoftGlow,
    AvailableEffectKind::Gradient2,
    AvailableEffectKind::Gradient4,
    AvailableEffectKind::Reflect,
    AvailableEffectKind::Shake,
];

/// Maps an effect kind to its stable discriminator string. This MUST match the
/// `"effect"` value written by `effect_card_to_value` / recognized by
/// `parse_effect_cards`, so a stored default round-trips. The three glow kinds map to
/// their distinct strings (`glow_v1` / `glow_v2` / `soft_glow`).
pub(super) fn effect_kind_key(kind: AvailableEffectKind) -> &'static str {
    match kind {
        AvailableEffectKind::TextShake => "text_shake",
        AvailableEffectKind::Stroke => "stroke",
        AvailableEffectKind::Shadow => "shadow",
        AvailableEffectKind::Blur => "blur",
        AvailableEffectKind::MotionBlur => "motion_blur",
        AvailableEffectKind::DryMedia => "dry_media",
        AvailableEffectKind::Interference => "interference",
        AvailableEffectKind::GlowV1 => "glow_v1",
        AvailableEffectKind::GlowV2 => "glow_v2",
        AvailableEffectKind::SoftGlow => "soft_glow",
        AvailableEffectKind::Gradient2 => "gradient2",
        AvailableEffectKind::Gradient4 => "gradient4",
        AvailableEffectKind::Reflect => "reflect",
        AvailableEffectKind::Shake => "shake",
    }
}

/// Runtime-global store of per-effect-kind default overrides, keyed by discriminator
/// string, value = the one-card JSON object. Lazily created; not on a hot path.
fn store() -> &'static RwLock<HashMap<String, Value>> {
    static STORE: OnceLock<RwLock<HashMap<String, Value>>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Returns the stored override JSON for one effect kind key, if any. Clones the value.
#[must_use]
pub(super) fn effect_default_value(key: &str) -> Option<Value> {
    let guard = match store().read() {
        Ok(guard) => guard,
        // A poisoned lock still holds valid data; recover it rather than panicking.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.get(key).cloned()
}

/// Returns a full snapshot of the current per-kind overrides (for off-thread persist).
#[must_use]
pub(super) fn all_effect_defaults() -> HashMap<String, Value> {
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clone()
}

/// Sets (or replaces) the override for one effect kind key in the runtime store.
/// Persistence is the caller's responsibility (off the GUI thread).
pub(super) fn set_effect_default_value(key: &str, value: Value) {
    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.insert(key.to_string(), value);
}

/// Removes the override for one effect kind key, reverting that kind to the built-in.
pub(super) fn clear_effect_default_value(key: &str) {
    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.remove(key);
}

/// Seeds the runtime-global store from `TextTab.effect_defaults` at startup. Best-effort:
/// a missing/malformed config yields an empty store (built-in defaults everywhere).
pub(crate) fn seed_effect_defaults_from_config() {
    let loaded = load_text_tab_effect_defaults();
    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = loaded;
}

/// Resolves the add-time default card for `kind`: if a user override is stored, parses it
/// into an `EffectCard`; otherwise returns `None` so the caller falls back to the built-in
/// `default_effect_card`. `text_color` is used for color-follows-text-color fields when a
/// stored value omits them (serialized cards always carry an explicit color).
#[must_use]
pub(super) fn effect_default_card(
    kind: AvailableEffectKind,
    text_color: Color32,
) -> Option<EffectCard> {
    let value = effect_default_value(effect_kind_key(kind))?;
    parse_effect_cards(std::slice::from_ref(&value), text_color)
        .into_iter()
        .next()
}

/// Snapshots the whole override map and persists it off the GUI thread. Errors are logged
/// but not surfaced (best-effort save, matching the other TextTab preset writers).
fn persist_effect_defaults_off_thread() {
    let defaults = all_effect_defaults();
    let _ = thread::Builder::new()
        .name("typing-save-effect-defaults".to_string())
        .spawn(move || {
            if let Err(err) = save_text_tab_effect_defaults(&defaults) {
                eprintln!("ERROR typing::effect_defaults save failed: {err}");
            }
        });
}

/// One editing row in `EffectDefaultsEditorState`: the buffered card for a kind plus
/// whether that kind currently has a user override.
struct EffectDefaultEntry {
    kind: AvailableEffectKind,
    card: EffectCard,
    has_override: bool,
}

/// Dedicated editor widget for the per-effect-kind default parameters. Rendered from the
/// settings pane via `ui`. Holds one buffered `EffectCard` per effect kind, lazily
/// initialized from the runtime store (override → parsed; else built-in). Editing a card
/// stores its override and persists the full map off-thread; resetting clears the override
/// and reverts to the built-in.
#[derive(Default)]
pub(crate) struct EffectDefaultsEditorState {
    /// Whether `entries` has been populated from the store yet.
    initialized: bool,
    /// One buffer per `ALL_EFFECT_KINDS`, in that order.
    entries: Vec<EffectDefaultEntry>,
}

// `EffectCard` (via `ColorField` → `ViewportColorSelector`) is not `Debug`, so the buffer
// cannot derive it; report the structural state instead.
impl std::fmt::Debug for EffectDefaultsEditorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectDefaultsEditorState")
            .field("initialized", &self.initialized)
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl EffectDefaultsEditorState {
    /// Creates an uninitialized editor; the per-kind buffers are populated lazily on the
    /// first `ui` call from the current runtime store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lazily builds one buffer card per effect kind from the runtime store: a stored
    /// override is parsed, otherwise the built-in default card is used.
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }
        self.entries = ALL_EFFECT_KINDS
            .iter()
            .map(|&kind| {
                let key = effect_kind_key(kind);
                match effect_default_value(key) {
                    Some(value) => {
                        // Override present: parse it back into a card. If parsing somehow
                        // yields nothing, fall back to the built-in so the row still edits.
                        let card = parse_effect_cards(
                            std::slice::from_ref(&value),
                            NEUTRAL_EDITOR_COLOR,
                        )
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| {
                            TypingCreatePanelState::default_effect_card(kind, NEUTRAL_EDITOR_COLOR)
                        });
                        EffectDefaultEntry {
                            kind,
                            card,
                            has_override: true,
                        }
                    }
                    None => EffectDefaultEntry {
                        kind,
                        card: TypingCreatePanelState::default_effect_card(
                            kind,
                            NEUTRAL_EDITOR_COLOR,
                        ),
                        has_override: false,
                    },
                }
            })
            .collect();
        self.initialized = true;
    }

    /// Renders the editor: an intro, a global reset, and one collapsible section per
    /// effect kind with its controls and a per-kind reset. Any change is stored in the
    /// runtime global and persisted off the GUI thread.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.ensure_initialized();

        ui.label(
            t!("typing.effect_defaults.description_hint"),
        );
        ui.add_space(4.0);

        let any_override = self.entries.iter().any(|entry| entry.has_override);
        if ui
            .add_enabled(any_override, egui::Button::new(t!("typing.effect_defaults.reset_all_button")))
            .clicked()
        {
            for entry in &mut self.entries {
                clear_effect_default_value(effect_kind_key(entry.kind));
                entry.card =
                    TypingCreatePanelState::default_effect_card(entry.kind, NEUTRAL_EDITOR_COLOR);
                entry.has_override = false;
            }
            persist_effect_defaults_off_thread();
        }
        ui.separator();

        // Persist at most once per frame even if several rows change (only one can).
        let mut store_changed = false;
        for idx in 0..self.entries.len() {
            let kind = self.entries[idx].kind;
            let has_override = self.entries[idx].has_override;
            let header = if has_override {
                format!("{} •", kind.label())
            } else {
                kind.label().to_string()
            };
            egui::CollapsingHeader::new(header)
                .id_salt(("typing_effect_default", effect_kind_key(kind)))
                .show(ui, |ui| {
                    let changed = draw_effect_card_controls(ui, &mut self.entries[idx].card);
                    if changed {
                        let value = effect_card_to_value(&self.entries[idx].card);
                        set_effect_default_value(effect_kind_key(kind), value);
                        self.entries[idx].has_override = true;
                        store_changed = true;
                    }
                    ui.add_space(2.0);
                    if ui
                        .add_enabled(
                            self.entries[idx].has_override,
                            egui::Button::new(t!("typing.effect_defaults.reset_to_builtin_button")),
                        )
                        .clicked()
                    {
                        clear_effect_default_value(effect_kind_key(kind));
                        self.entries[idx].card = TypingCreatePanelState::default_effect_card(
                            kind,
                            NEUTRAL_EDITOR_COLOR,
                        );
                        self.entries[idx].has_override = false;
                        store_changed = true;
                    }
                });
        }

        if store_changed {
            persist_effect_defaults_off_thread();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_card_round_trips_through_value_for_representative_kinds() {
        // Serialize a built-in card, parse it back, re-serialize, and compare the JSON.
        // This proves `effect_card_to_value` and `parse_effect_cards` agree for the kinds
        // the store persists (a color kind, a glow version, an integer/enum kind, a
        // plain-scalar kind).
        for kind in [
            AvailableEffectKind::Stroke,
            AvailableEffectKind::SoftGlow,
            AvailableEffectKind::Shake,
            AvailableEffectKind::Blur,
            AvailableEffectKind::Interference,
        ] {
            let card = TypingCreatePanelState::default_effect_card(kind, NEUTRAL_EDITOR_COLOR);
            let value = effect_card_to_value(&card);
            let reparsed = parse_effect_cards(std::slice::from_ref(&value), NEUTRAL_EDITOR_COLOR)
                .into_iter()
                .next()
                .expect("parse must reproduce the card");
            assert_eq!(
                effect_card_to_value(&reparsed),
                value,
                "round-trip mismatch for {}",
                effect_kind_key(kind)
            );
        }
    }

    #[test]
    fn interference_card_round_trips_all_parameters() {
        let card = EffectCard::Interference(InterferenceEffectCard {
            kind: InterferenceKind::Scanlines,
            seed: 9_876_543_210,
            amount: 0.13,
            scale_px: 63.5,
            density: 0.27,
            monochrome: false,
            alpha_noise: 0.91,
            slice_height_px: 255,
            height_jitter: 0.82,
            max_shift_px: 511.5,
            probability: 0.67,
            rgb_split_px: 63.0,
            autogrow: false,
            offset_px: 62.5,
            angle_deg: -3_599.0,
            per_row_jitter: 0.76,
            line_height_px: 63,
            gap_px: 61,
            darken: 0.24,
            jitter_px: 31.5,
        });
        let value = effect_card_to_value(&card);
        let reparsed = parse_effect_cards(std::slice::from_ref(&value), NEUTRAL_EDITOR_COLOR)
            .into_iter()
            .next();
        assert!(reparsed.is_some(), "parse must reproduce the interference card");
        if let Some(reparsed) = reparsed {
            assert_eq!(effect_card_to_value(&reparsed), value);
        }
    }

    #[test]
    fn store_set_get_clear_roundtrips() {
        let key = "test_effect_kind_store_roundtrip";
        // Absent by default.
        clear_effect_default_value(key);
        assert_eq!(effect_default_value(key), None);
        // Set then get.
        let value = serde_json::json!({ "effect": "blur", "radius": 7.0 });
        set_effect_default_value(key, value.clone());
        assert_eq!(effect_default_value(key), Some(value.clone()));
        assert_eq!(all_effect_defaults().get(key), Some(&value));
        // Clear removes it.
        clear_effect_default_value(key);
        assert_eq!(effect_default_value(key), None);
    }

    #[test]
    fn effect_default_card_uses_override_when_present() {
        let key = effect_kind_key(AvailableEffectKind::Blur);
        // Override the blur radius; the resolved card must reflect it.
        set_effect_default_value(key, serde_json::json!({ "effect": "blur", "radius": 42.0 }));
        let card = effect_default_card(AvailableEffectKind::Blur, NEUTRAL_EDITOR_COLOR)
            .expect("override must resolve to a card");
        if let EffectCard::Blur(blur) = card {
            assert_eq!(blur.radius_px, 42.0);
        } else {
            panic!("expected a blur card");
        }
        clear_effect_default_value(key);
        assert!(effect_default_card(AvailableEffectKind::Blur, NEUTRAL_EDITOR_COLOR).is_none());
    }
}
