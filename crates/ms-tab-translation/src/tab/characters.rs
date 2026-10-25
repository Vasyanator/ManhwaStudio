/*
File: crates/ms-tab-translation/src/tab/characters.rs

Purpose:
The footer character data of the Translation tab: the character-name autocomplete base (project
roster + chapter-typed names + built-in names), the `characters.json` mtime watch, footer field
tracking and debounced footer patches, the recent-character shortcuts and the image-bubble file
pick.

Key functions:
- TranslationTabState::ensure_character_names_loaded / reload_character_names /
  rebuild_character_names / sync_chapter_character_names / maybe_refresh_character_names_by_watch.
- sync_footer_tracking / init_last_footer_values / apply_defaults_for_new_bubble: footer
  bookkeeping over the canvas bubbles.
- queue_footer_patch / flush_footer_patches: debounced footer writes through
  `CanvasView::patch_bubble_extra_fields`.
- create_bubble_at_pointer_shortcut / create_hint_bubble_at_pointer_shortcut (pub, host input)
  and apply_recent_character_rank_to_latest_bubble: the recent-character shortcuts.
- build_translation_character_names, footer_autocomplete_candidates,
  collect_chapter_character_names, collect_recent_character_history,
  footer_tracking_should_recompute, characters_file_signature: pure helpers.
*/

use ms_canvas::{BubbleClass, CanvasView};
use ms_project::{Bubble, ProjectData};
use ms_tabs_simple::characters::load_character_names;
use crate::panels::bubbles::{BubbleFooterState, bubble_extra_string, bubble_footer_state_from_record, footer_no_character, footer_no_characters, footer_state_for_bubble};
use eframe::egui;
use egui::Pos2;
use ms_theme::Severity;
use serde_json::{Map, Value};
use std::collections::{HashSet, VecDeque};
use super::{CHARACTER_NAMES_WATCH_CHECK_SECS, FOOTER_PATCH_DEBOUNCE_SECS, RECENT_CHARACTER_HISTORY_LIMIT, RecentCharacterEntry, TranslationTabState, footer_additional_character_names};
use super::canvas_hooks::{copy_external_image_bubble, project_relative_path};

impl TranslationTabState {
    pub(super) fn ensure_character_names_loaded(&mut self, project: &ProjectData) {
        let chars_dir = project.paths.characters_dir.clone();
        let needs_load = match self.characters_loaded_for.as_ref() {
            Some(loaded) => *loaded != chars_dir,
            None => true,
        };
        if needs_load {
            self.reload_character_names(project);
        }
    }

    /// Re-reads the project character roster from `characters.json` and rebuilds the footer
    /// autocomplete base around it. The chapter-collected names are kept as they are: they are
    /// derived from bubbles, not from the roster file.
    pub(super) fn reload_character_names(&mut self, project: &ProjectData) {
        // A single-image session has no title, hence no roster: reading it would probe (and the
        // characters loader may create) a `characters/` folder inside the scratch directory.
        // The footer still offers the built-in and chapter-collected names.
        if !self.session_caps.title_documents {
            self.character_names_project = Vec::new();
            self.rebuild_character_names();
            self.characters_loaded_for = Some(project.paths.characters_dir.clone());
            self.characters_file_signature = None;
            return;
        }
        // A read failure must be visible: the assembled list still looks populated (built-ins plus
        // the names typed in this chapter), so a silently empty roster is easy to miss in the UI.
        // Not per-frame spam — this runs only on a project/chapter switch, on an observed
        // `characters.json` mtime change, or on an explicit `notify_characters_changed`.
        self.character_names_project = match load_character_names(project) {
            Ok(names) => names,
            Err(error) => {
                ms_log::runtime_log::log_warn(format!(
                    "[translation] could not read the character roster; the footer autocomplete \
                     falls back to built-in and chapter-collected names only. Dir: {} Error: {}",
                    project.paths.characters_dir.display(),
                    error
                ));
                Vec::new()
            }
        };
        self.rebuild_character_names();
        self.characters_loaded_for = Some(project.paths.characters_dir.clone());
        self.characters_file_signature = characters_file_signature(project);
    }

    /// Reassembles `character_names` from its two inputs (`characters.json` roster and the names
    /// typed in this chapter). Called only when one of them actually changed — both consumers
    /// (the bubbles panel whole, the footer via `footer_autocomplete_candidates`) borrow the list
    /// as a slice every frame, so rebuilding it per frame would be pure waste.
    pub(super) fn rebuild_character_names(&mut self) {
        self.character_names = build_translation_character_names(
            &self.character_names_project,
            &self.character_names_chapter,
        );
    }

    /// Refreshes the chapter-collected part of the footer autocomplete base from `bubbles`.
    ///
    /// `bubbles` must be the WHOLE chapter (all pages), not the current page: a name typed on any
    /// page is offered on every other one.
    ///
    /// Cost, precisely: the full chapter scan ALWAYS runs when this is called — only the
    /// reassembly of `character_names` is skipped, and only when the collected list is equal to
    /// the stored one. The caller's revision gate is what keeps the scan off the per-frame path;
    /// note that the revision moves on every footer patch flush, i.e. roughly every
    /// `FOOTER_PATCH_DEBOUNCE_SECS` while a name is being typed, so the scan must stay linear and
    /// allocation-light (see `collect_chapter_character_names`).
    ///
    /// A name just typed into the footer reaches the model through that same debounced
    /// `pending_footer_patches` queue, so it enters the base one flush later, not on the
    /// keystroke. That lag is accepted deliberately: the collection runs off the bubble revision,
    /// and the flush is what moves it.
    pub(super) fn sync_chapter_character_names(&mut self, bubbles: &[Bubble]) {
        let collected = collect_chapter_character_names(bubbles);
        if collected == self.character_names_chapter {
            return;
        }
        self.character_names_chapter = collected;
        self.rebuild_character_names();
    }

    pub(super) fn maybe_refresh_character_names_by_watch(&mut self, project: &ProjectData, now_s: f64) {
        if !self.session_caps.title_documents
            || now_s - self.character_names_watch_last_check_s < CHARACTER_NAMES_WATCH_CHECK_SECS
        {
            return;
        }
        self.character_names_watch_last_check_s = now_s;
        let signature = characters_file_signature(project);
        if signature != self.characters_file_signature {
            self.pending_characters_refresh = true;
        }
    }

    /// Tracks bubble lifecycle and prunes footer caches, applying footer defaults to newly
    /// detected bubbles.
    ///
    /// Also refreshes the chapter-collected half of the footer character autocomplete base
    /// (`sync_chapter_character_names`), which is derived from the same snapshot.
    ///
    /// Runs every frame from `draw_canvas_overlay_top_left`, so the expensive recompute (full
    /// bubble snapshot clone + id `HashSet` + recent-character history + chapter names) is gated on
    /// `CanvasView::hook_bubbles_key(project)`: when the key is unchanged and footer tracking was
    /// already bootstrapped, the call returns early and reuses the cached footer state. The key is
    /// the `project.bubbles` allocation plus `hook_bubbles_revision` — the revision alone could
    /// cache a snapshot of a stale `project.bubbles` (the model moved earlier in the frame, the
    /// studio hands over the new `ProjectData` next frame) and then never recompute. A
    /// newly-created bubble (even one living only in `runtime_bubbles`) bumps the revision, so the
    /// "apply defaults for new bubble" path still fires. The first call always recomputes.
    pub(super) fn sync_footer_tracking(&mut self, canvas: &CanvasView, project: &ProjectData) {
        let current_key = canvas.hook_bubbles_key(project);
        if !footer_tracking_should_recompute(
            self.footer_tracking_synced_key.as_ref(),
            &current_key,
            self.footer_bootstrapped,
        ) {
            return;
        }
        self.footer_tracking_synced_key = Some(current_key);

        let live_bubbles = canvas.hook_bubbles_snapshot(project);
        // Reuse the snapshot that is already built here — it is the whole chapter plus the
        // runtime-only bubbles, and `hook_bubbles_snapshot` clones the entire vector, so a second
        // call for the character names would double the cost of every revision change.
        //
        // Deliberate asymmetry with `collect_recent_character_history` below, which reads
        // `project.bubbles` (no runtime-only bubbles): a name on a just-created bubble that has
        // not reached the model yet is offered by the autocomplete but is not yet a card in
        // «Последние персонажи». Suggestions should be as eager as possible; the six ranked cards
        // are a digit-addressed history and should not reshuffle around a bubble that may still
        // vanish before it is ever flushed.
        self.sync_chapter_character_names(&live_bubbles);
        let known_now = live_bubbles
            .iter()
            .map(|bubble| bubble.id)
            .collect::<HashSet<_>>();
        if !self.footer_bootstrapped {
            self.footer_known_ids = known_now;
            self.init_last_footer_values(&live_bubbles);
            self.footer_bootstrapped = true;
            return;
        }

        for bubble in &live_bubbles {
            if self.footer_known_ids.insert(bubble.id) {
                self.apply_defaults_for_new_bubble(bubble, canvas.state.auto_insert_last_character);
            }
        }

        self.footer_known_ids.retain(|bid| known_now.contains(bid));
        self.footer_overrides
            .retain(|bid, _| self.footer_known_ids.contains(bid));
        self.pending_footer_patches
            .retain(|bid, _| self.footer_known_ids.contains(bid));
        self.pending_footer_patch_changed_at
            .retain(|bid, _| self.footer_known_ids.contains(bid));
        self.footer_character_autocomplete
            .retain(|bid, _| self.footer_known_ids.contains(bid));
        self.recent_characters = collect_recent_character_history(project.bubbles.as_ref());
    }

    fn init_last_footer_values(&mut self, bubbles: &[Bubble]) {
        self.recent_characters = collect_recent_character_history(bubbles);
        let Some(last) = bubbles.iter().max_by_key(|bubble| bubble.id) else {
            return;
        };
        let state = bubble_footer_state_from_record(last);
        self.last_is_known_character = state.is_known_character;
        self.last_character_name = state.character_name;
        self.last_clarification = state.clarification;
        self.last_page_idx = last.img_idx as i64;
        self.last_bubble_order = state.bubble_order;
    }

    fn apply_defaults_for_new_bubble(&mut self, bubble: &Bubble, auto_insert_last_character: bool) {
        let mut state = bubble_footer_state_from_record(bubble);
        let mut patch = Map::new();
        if !bubble.extra.contains_key("translation_status") {
            patch.insert(
                "translation_status".to_string(),
                Value::String("untranslated".to_string()),
            );
        }
        if auto_insert_last_character && !bubble.extra.contains_key("is_known_character") {
            state.is_known_character = self.last_is_known_character;
            patch.insert(
                "is_known_character".to_string(),
                Value::Bool(self.last_is_known_character),
            );
        }
        if auto_insert_last_character && !bubble.extra.contains_key("character_name") {
            state.character_name = self.last_character_name.clone();
            patch.insert(
                "character_name".to_string(),
                Value::String(state.character_name.clone()),
            );
        }
        if auto_insert_last_character && !bubble.extra.contains_key("clarification") {
            state.clarification = self.last_clarification.clone();
            patch.insert(
                "clarification".to_string(),
                Value::String(state.clarification.clone()),
            );
        }
        if !bubble.extra.contains_key("bubble_order") {
            let order =
                if self.last_page_idx == bubble.img_idx as i64 && self.last_bubble_order >= 0 {
                    self.last_bubble_order.saturating_add(1)
                } else {
                    0
                };
            state.bubble_order = order;
            patch.insert("bubble_order".to_string(), Value::Number(order.into()));
        }
        if !patch.is_empty() {
            self.pending_footer_patches.insert(bubble.id, patch);
            self.footer_overrides.insert(bubble.id, state.clone());
        }

        if auto_insert_last_character
            || bubble.extra.contains_key("is_known_character")
            || bubble.extra.contains_key("character_name")
            || bubble.extra.contains_key("clarification")
        {
            self.last_is_known_character = state.is_known_character;
            self.last_character_name = state.character_name.clone();
            self.last_clarification = state.clarification.clone();
        }
        self.last_page_idx = bubble.img_idx as i64;
        self.last_bubble_order = state.bubble_order;
    }

    fn recent_character_entry_for_rank(&self, rank: usize) -> Option<&RecentCharacterEntry> {
        rank.checked_sub(1)
            .and_then(|idx| self.recent_characters.get(idx))
    }

    fn latest_bubble_id(&self, canvas: &CanvasView, project: &ProjectData) -> Option<i64> {
        canvas
            .hook_bubbles_snapshot(project)
            .into_iter()
            .max_by_key(|bubble| bubble.id)
            .map(|bubble| bubble.id)
    }

    fn apply_recent_character_rank_to_bubble(
        &mut self,
        canvas: &mut CanvasView,
        project: &ProjectData,
        bubble_id: i64,
        rank: usize,
    ) -> bool {
        let Some(entry) = self.recent_character_entry_for_rank(rank).cloned() else {
            return false;
        };
        let mut patch = Map::new();
        patch.insert(
            "is_known_character".to_string(),
            Value::Bool(entry.is_known_character),
        );
        patch.insert(
            "character_name".to_string(),
            Value::String(entry.character_name.clone()),
        );
        patch.insert(
            "clarification".to_string(),
            Value::String(entry.clarification.clone()),
        );
        if !canvas.patch_bubble_extra_fields(project, bubble_id, &patch) {
            return false;
        }
        self.footer_overrides.insert(
            bubble_id,
            BubbleFooterState {
                bubble_order: self.last_bubble_order.max(0),
                is_known_character: entry.is_known_character,
                character_name: entry.character_name.clone(),
                clarification: entry.clarification.clone(),
            },
        );
        self.last_is_known_character = entry.is_known_character;
        self.last_character_name = entry.character_name;
        self.last_clarification = entry.clarification;
        true
    }

    pub(super) fn apply_recent_character_rank_to_latest_bubble(
        &mut self,
        canvas: &mut CanvasView,
        project: &ProjectData,
        rank: usize,
    ) -> bool {
        let Some(bubble_id) = self.latest_bubble_id(canvas, project) else {
            return false;
        };
        self.apply_recent_character_rank_to_bubble(canvas, project, bubble_id, rank)
    }

    /// Creates a text bubble under the pointer and, with `recent_rank` (the recent-character
    /// parameter of the create command), seeds it with that recent character. Returns whether a
    /// bubble was created.
    pub fn create_bubble_at_pointer_shortcut(
        &mut self,
        canvas: &mut CanvasView,
        project: &ProjectData,
        pointer_pos: Pos2,
        recent_rank: Option<usize>,
    ) -> bool {
        if !canvas.create_bubble_at_pointer_shortcut(pointer_pos) {
            return false;
        }
        if let Some(rank) = recent_rank {
            let _ = self.apply_recent_character_rank_to_latest_bubble(canvas, project, rank);
        }
        true
    }

    /// Creates a hint bubble under the pointer; returns whether one was created.
    ///
    /// Unlike [`Self::create_bubble_at_pointer_shortcut`] there is no recent-character seeding:
    /// a hint carries no character. Executed for `crate::input::CREATE_HINT_BUBBLE`.
    pub fn create_hint_bubble_at_pointer_shortcut(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        pointer_pos: Pos2,
    ) -> bool {
        canvas.create_hint_bubble_at_pointer_shortcut(ctx, pointer_pos)
    }

    /// Resolves the image-bubble «choose file» dialog once it closed: copies the picked file into
    /// the staging image-bubble folder and queues the `image_path` footer patch. A cancel is a
    /// no-op; a dialog or copy failure becomes an error toast.
    pub(super) fn poll_image_bubble_pick(&mut self, ctx: &egui::Context, project: &ProjectData, now_s: f64) {
        let Some(pick) = self.image_bubble_pick.as_mut() else {
            return;
        };
        let Some(result) = pick.poll() else {
            return;
        };
        let bubble_id = pick.bubble_id();
        self.image_bubble_pick = None;
        let source = match result {
            Ok(Some(picked)) => match picked.into_file() {
                Some(file) => file.path,
                // `OpenFile` always answers with one file (`Picked` contract); anything else is
                // a platform-layer bug, logged and treated as a cancel.
                None => {
                    ms_log::runtime_log::log_error(format!("[translation] image bubble {bubble_id}: file dialog returned no file for an OpenFile request"));
                    return;
                }
            },
            Ok(None) => return,
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[translation] image bubble {bubble_id}: file dialog failed: {err:?}"));
                self.push_toast(ctx, err.user_message(), Severity::Error, 3.0);
                return;
            }
        };
        match copy_external_image_bubble(project, bubble_id, &source) {
            Ok(saved) => self.queue_footer_patch(bubble_id, "image_path", Value::String(project_relative_path(project, &saved)), now_s),
            Err(err) => self.push_toast(ctx, err, Severity::Error, 3.0),
        }
    }

    pub(super) fn queue_footer_patch(&mut self, bubble_id: i64, field: &str, value: Value, now_s: f64) {
        self.pending_footer_patches
            .entry(bubble_id)
            .or_default()
            .insert(field.to_string(), value);
        self.pending_footer_patch_changed_at
            .insert(bubble_id, now_s);
    }

    pub(super) fn flush_footer_patches(&mut self, canvas: &mut CanvasView, project: &ProjectData, now_s: f64) {
        if self.pending_footer_patches.is_empty() {
            return;
        }
        let mut done = Vec::new();
        for (bubble_id, patch) in &self.pending_footer_patches {
            let changed_at = self
                .pending_footer_patch_changed_at
                .get(bubble_id)
                .copied()
                .unwrap_or(0.0);
            if now_s - changed_at < FOOTER_PATCH_DEBOUNCE_SECS {
                continue;
            }
            if canvas.patch_bubble_extra_fields(project, *bubble_id, patch) {
                done.push(*bubble_id);
            }
        }
        for bubble_id in done {
            self.pending_footer_patches.remove(&bubble_id);
            self.pending_footer_patch_changed_at.remove(&bubble_id);
        }
        self.footer_overrides
            .retain(|bid, _| self.pending_footer_patches.contains_key(bid));
    }

    pub(super) fn footer_state_for(&self, bubble: &Bubble) -> BubbleFooterState {
        footer_state_for_bubble(&self.footer_overrides, bubble)
    }

    pub(super) fn bubble_has_character_for_status(&self, bubble: &Bubble) -> bool {
        let character_name = self.footer_state_for(bubble).character_name;
        let trimmed = character_name.trim();
        !trimmed.is_empty() && trimmed != footer_no_character() && trimmed != footer_no_characters()
    }
}

/// Assembles the footer character autocomplete base out of its four sources.
///
/// The returned order IS the suggestion priority the widget shows: the "no character" sentinel
/// first, then the project roster from `characters.json` (`project_names`, in file order), then the
/// free-form names typed on this chapter's bubbles (`chapter_names`, in bubble order), then the
/// built-in service names. Entries are trimmed, empties dropped, and duplicates removed
/// case-insensitively keeping the FIRST spelling — so a chapter name that already exists in the
/// roster does not appear twice and keeps the roster's spelling.
#[must_use]
pub(super) fn build_translation_character_names(
    project_names: &[String],
    chapter_names: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push_unique = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        if seen.insert(trimmed.to_lowercase()) {
            out.push(trimmed.to_string());
        }
    };

    push_unique(footer_no_character());
    for name in project_names {
        push_unique(name);
    }
    for name in chapter_names {
        push_unique(name);
    }
    for name in footer_additional_character_names() {
        push_unique(name);
    }
    out
}

/// The suggestion slice the FOOTER's free-input field gets: `names` without a leading
/// `footer_no_character()` sentinel.
///
/// The sentinel belongs to the bubbles panel alone, where the character field is a combo box and
/// "(не указан)" is a selectable row (`panels/bubbles.rs` uses `character_names[0]` as its
/// fallback value). In the footer the same list feeds a free-text field, and `AutocompleteLine`
/// may splice the TAIL of a candidate from a word boundary: with the sentinel present, typing
/// "ук" matches inside "(не указан)" and silently writes `указан)` into the field with
/// `changed = true`. The debounced patch then stores that fragment as a real character name — it
/// matches no sentinel, so nothing downstream (chapter names, recent-character cards, MT grouping,
/// composition) would ever filter it out again. Keeping the sentinel out of the widget's input is
/// the fix; the widget itself is right to offer tails.
///
/// The check is explicit, never a blind `[1..]`: a list that does not start with the sentinel (an
/// empty one, or a future assembly order) is returned untouched. Returns a borrowed slice — the
/// footer must not clone this per frame.
#[must_use]
pub(super) fn footer_autocomplete_candidates(names: &[String]) -> &[String] {
    match names.split_first() {
        Some((first, rest)) if first == footer_no_character() => rest,
        _ => names,
    }
}

/// Collects every character name actually entered on `bubbles`, in bubble-id order.
///
/// `bubbles` is the whole chapter; the function does not care which page a bubble is on. The name
/// is read from the same `extra` key, with the same reader, that
/// `bubble_footer_state_from_record` uses for the footer field (`bubble_extra_string(..,
/// "character_name")`), so what is collected is byte-for-byte what the user sees — but without
/// building the whole `BubbleFooterState`, whose `clarification` `String` this function would
/// allocate and immediately drop for every bubble in the chapter. The name is collected regardless
/// of `is_known_character` — free-form input is the point of this list.
///
/// Dropped: bubbles of the `Image` and `Hint` classes (neither has a character field, so a stale
/// `character_name` left in their `extra` by a class switch is not a name anyone typed there),
/// blank names, and the two display sentinels `footer_no_character()` / `footer_no_characters()`.
/// The sentinel filter is REQUIRED, not defensive: `panels/bubbles.rs` writes
/// `footer_no_character()` into `Bubble.extra` through the footer patch queue when the "И.П."
/// checkbox is switched on for a bubble whose name is not in the list, so the sentinel really does
/// reach persisted data and would otherwise come back as a "character" named "(не указан)".
/// Duplicates are removed case-insensitively, keeping the first spelling; names appear in
/// ascending `Bubble.id` order of the bubble that introduced them, so the result is deterministic
/// whatever order `bubbles` arrives in.
#[must_use]
pub(super) fn collect_chapter_character_names(bubbles: &[Bubble]) -> Vec<String> {
    let mut ordered = bubbles.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|bubble| bubble.id);

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for bubble in ordered {
        match bubble.bubble_class.as_deref().map(BubbleClass::from_str) {
            Some(BubbleClass::Image | BubbleClass::Hint) => continue,
            Some(BubbleClass::Text) | None => {}
        }
        let name = bubble_extra_string(&bubble.extra, "character_name");
        let trimmed = name.trim();
        if trimmed.is_empty()
            || trimmed == footer_no_character()
            || trimmed == footer_no_characters()
        {
            continue;
        }
        if seen.insert(trimmed.to_lowercase()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn recent_character_entry_from_state(state: &BubbleFooterState) -> Option<RecentCharacterEntry> {
    let trimmed = state.character_name.trim();
    if trimmed.is_empty() || trimmed == footer_no_character() || trimmed == footer_no_characters() {
        return None;
    }
    Some(RecentCharacterEntry {
        is_known_character: state.is_known_character,
        character_name: trimmed.to_string(),
        clarification: state.clarification.clone(),
    })
}

/// Decides whether `sync_footer_tracking` must run a full recompute this frame.
///
/// `cached` is the bubble-set key the last recompute was performed for (`None` before the first
/// recompute), `current` is the live key (`CanvasView::hook_bubbles_key` in production), and
/// `bootstrapped` is whether footer tracking has already been initialized. Returns `true` when not
/// yet bootstrapped (the first frame must always recompute and bootstrap) or when the key differs
/// from the cached one (the bubble set changed); returns `false` only when bootstrapped and the key
/// is unchanged, so the cached footer state can be reused.
#[must_use]
pub(super) fn footer_tracking_should_recompute<K: PartialEq>(cached: Option<&K>, current: &K, bootstrapped: bool) -> bool {
    if !bootstrapped {
        return true;
    }
    cached != Some(current)
}

fn collect_recent_character_history(bubbles: &[Bubble]) -> VecDeque<RecentCharacterEntry> {
    let mut recent: VecDeque<RecentCharacterEntry> = VecDeque::new();
    let mut ordered = bubbles.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|bubble| bubble.id);
    for bubble in ordered.into_iter().rev() {
        let state = bubble_footer_state_from_record(bubble);
        let Some(entry) = recent_character_entry_from_state(&state) else {
            continue;
        };
        if recent
            .iter()
            .any(|item: &RecentCharacterEntry| item.character_name == entry.character_name)
        {
            continue;
        }
        recent.push_back(entry);
        if recent.len() >= RECENT_CHARACTER_HISTORY_LIMIT {
            break;
        }
    }
    recent
}

/// Cheap change probe of the title `characters.json` through the document store; `None`
/// when it is absent or cannot be stated (the latter is logged).
fn characters_file_signature(project: &ProjectData) -> Option<ms_docstore::Signature> {
    let path = project.paths.characters_dir.join("characters.json");
    match ms_docstore::signature(&ms_docstore::DocRef::new(&path, ms_docstore::DocKind::Characters)) {
        Ok(signature) => signature,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[translation::characters_watch] failed to probe characters file; path={} error={err}",
                path.display()
            ));
            None
        }
    }
}
