/*
File: settings/typesetting/font_properties_window.rs

Purpose:
Per-font "properties" window opened from the "Настройки шрифтов" block in the settings
"Тайп" pane (from `font_settings.rs`). Shows a single font's identity, lets the user set a
display-name override, previews arbitrary text in the font's own typeface, and lists the
font's supported glyphs and non-default kerning pairs.

Main responsibilities:
- own the open-window state (`FontPropertiesState`) for exactly one font at a time;
- render the floating `egui::Window` (identity header, display-name editor, a "Группы"
  section for this font's virtual-group membership, live preview, collapsible glyph grid,
  collapsible built-in kerning list, custom-kerning-pair section) without blocking the GUI
  thread;
- own the sibling custom-kerning EDITOR window (two characters + an advance offset in
  thousandths of an em, plus a preview-only context on either side and a preview-only font
  size) and persist its result through `font_admin::set_custom_kerning`;
- analyze the font file OFF the GUI thread (glyph inventory + kerning extraction via
  `ttf-parser`) and deliver the result over an `mpsc` channel the window polls;
- wire the display-name editor to `crate::tabs::typing::font_admin::set_display_name_override`
  (which bumps the shared revision, so the category lists reload automatically).

Key types:
- `FontPropertiesState` (owned by `FontSettingsEditorState`)
- `FontAnalysis` / `KerningPairInfo` (off-thread analysis result)
- `CustomKerningEditorState` / `KerningEditorAction` (the sibling editor window)

Key functions:
- `show` (renders the window, returns whether it stays open)
- `analyze_font_bytes` (pure ttf-parser analysis over `&[u8]`, unit-tested via helpers)
- `displayable_char` / `build_reverse_glyph_map` / `finalize_kerning` (pure, unit-tested)
- `design_units_to_per_mille` / `clamp_to_single_char` / `conflicting_pair_index` /
  `apply_pair_edit` (pure custom-kerning logic, unit-tested)
- `build_preview_run` / `live_custom_offset_per_mille` / `resolve_run_kern_per_mille` /
  `layout_preview_run` (pure editor-preview layout, unit-tested)

Notes:
The font MODEL is reached ONLY through the `crate::tabs::typing::font_admin` facade. Every
per-font SETTING it mutates (display-name override, virtual-group membership and aliases) is
keyed by the font's IDENTITY; the file path is kept purely as the byte source of the
glyph/kerning analysis and the own-typeface preview. egui own-typeface registration reuses
`crate::widgets::request_font_family` (ADD-ONLY, cached per family, bytes read OFF the GUI
thread), exactly like the category rows. The heavy ttf-parser work never runs on the GUI
thread either; the window only polls the channel and repaints while pending.

Custom kerning pairs are USER data, not font data: they live in `fonts_data.json` under the
font's identity and are measured in THOUSANDTHS OF AN EM, so one pair behaves identically at
every rendered size and across fonts with different `units_per_em`. The font FILE is never
touched. A custom pair OVERRIDES the font's own kerning for the same two characters, and an
offset of `0.0` is a meaningful value that cancels a built-in pair.

The editor's "Символы до" / "Символы после" fields AND its preview font size are a VISUAL
DEBUGGING AID and are never persisted: they live on `CustomKerningEditorState` alone, are absent from `CustomKerningPair`,
and die with the editor window. They extend the live preview from the bare pair to the run
`before + left + right + after`, laid out glyph by glyph (egui 0.35 applies no kerning in text
layout, so a single-glyph galley's width IS that glyph's advance). Each gap of that run is
kerned by strict precedence — the editor's LIVE custom value, else another of the user's custom
pairs, else the font's built-in value, else zero — with a custom value always REPLACING the
built-in one, matching the renderer. The one honest limit: the built-in list is capped at
`MAX_KERNING_PAIRS`, so a context pair beyond the cap renders unkerned here while the renderer
kerns it; the editor says so whenever `kerning_truncated` is set and a context is typed.
The preview SIZE follows the same contract and starts at `PAIR_PREVIEW_FONT_SIZE`. It cannot
change what the pair does — the offset is in thousandths of an em, so every gap is converted
against the same live size the glyphs are painted at and the run stays a faithful scale model
at any size. The strip's height is derived from that size (`PAIR_PREVIEW_HEIGHT_RATIO`), and
the offset row's px read-out follows it too, so the number describes the strip on screen.
*/

use crate::tabs::typing::font_admin::{self, CustomKerningPair, FontEntry};
use crate::widgets::{PreviewFontFamily, WheelComboBox, WheelSpinBox, request_font_family};
use ms_thread as thread;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};

// --- Layout constants -------------------------------------------------------

/// Side length (points) of a single glyph cell in the virtualized grid.
const GLYPH_CELL_SIZE: f32 = 34.0;
/// Font size (points) used to draw each glyph inside its cell.
const GLYPH_PREVIEW_FONT_SIZE: f32 = 24.0;
/// Font size (points) used for the free-text preview line and the kerning px scaling.
const PREVIEW_FONT_SIZE: f32 = 30.0;
/// Maximum height (points) of the scrollable glyph grid before it scrolls internally.
const GLYPH_GRID_MAX_HEIGHT: f32 = 300.0;
/// Maximum height (points) of the scrollable kerning list before it scrolls internally.
const KERNING_LIST_MAX_HEIGHT: f32 = 220.0;
/// Fixed row height (points) of one kerning-list row (own-typeface headroom included).
const KERNING_ROW_HEIGHT: f32 = 30.0;
/// Fixed row height (points) of one CUSTOM kerning row. Separate from `KERNING_ROW_HEIGHT`
/// because the row is a clickable button-like strip with its own padding, and `show_rows`
/// requires the height it is given to be the height every row actually occupies.
const CUSTOM_KERNING_ROW_HEIGHT: f32 = 32.0;
/// Maximum height (points) of the scrollable custom-kerning list before it scrolls internally.
const CUSTOM_KERNING_LIST_MAX_HEIGHT: f32 = 200.0;
/// Width (points) of the own-typeface pair column inside one custom-kerning row.
const CUSTOM_KERNING_PAIR_COL_WIDTH: f32 = 64.0;
/// DEFAULT font size (points) the editor's live pair preview is painted at. The size itself is
/// adjustable per editor session (`CustomKerningEditorState::preview_font_size`); this is only
/// where a freshly opened editor starts.
const PAIR_PREVIEW_FONT_SIZE: f32 = 120.0;
/// Inclusive bounds (points) the preview-size control accepts. The lower bound keeps the glyphs
/// legible enough to judge a gap at all; the upper one keeps a pair inside the strip's width on
/// an ordinary window before the run has to be clipped.
const PAIR_PREVIEW_FONT_SIZE_MIN: f32 = 12.0;
const PAIR_PREVIEW_FONT_SIZE_MAX: f32 = 400.0;
/// Height of the preview strip as a multiple of the preview font size. The strip has to grow
/// with the glyphs or a larger size would simply paint outside it; the ratio is the one the
/// fixed 48 pt / 76 pt pair used before the size became adjustable.
const PAIR_PREVIEW_HEIGHT_RATIO: f32 = 76.0 / 48.0;
/// Upper bound on how many characters EACH preview-context field contributes to the editor's
/// preview run. The run is laid out and painted glyph by glyph on the GUI thread every frame
/// inside a fixed-width strip, so an unbounded paste would both walk off the strip and pay for
/// layout nobody can see. Enforced twice: by the field widget's `char_limit`, and again by
/// `build_preview_run` so the per-frame work is bounded independently of the widget.
const CUSTOM_KERNING_CONTEXT_CHAR_LIMIT: usize = 16;
/// Inclusive bound (thousandths of an em) the offset editor accepts in either direction.
/// One em of extra space either way already covers every sane typographic correction; the
/// bound exists so a mis-scrolled wheel cannot push a glyph off the balloon entirely.
const CUSTOM_KERNING_LIMIT_PER_MILLE: f32 = 1000.0;

// --- Extraction bounds ------------------------------------------------------

/// Upper bound on the number of kerning pairs SHOWN. Extraction beyond this is dropped and
/// the UI states that the list was truncated (see `finalize_kerning`).
const MAX_KERNING_PAIRS: usize = 2000;
/// Upper bound on how many char-mapped glyphs are probed as first/second glyphs when
/// enumerating GPOS pair kerning. GPOS pair enumeration is O(N^2) in the probe set, so a
/// huge CJK font (20k+ glyphs) is capped here; exceeding the cap marks the list truncated.
const MAX_KERN_PROBE_GLYPHS: usize = 1500;
/// Safety bound on RAW collected pairs before dedup/cap, to bound worst-case memory on a
/// pathological font. Hitting it marks the list truncated.
const MAX_RAW_KERN_PAIRS: usize = 20_000;
/// Global budget on GPOS pair-adjustment PROBE operations (individual second-glyph lookups)
/// across ALL subtables and lookups combined. `MAX_RAW_KERN_PAIRS` only stops on COLLECTED
/// pairs, so a hostile font with many zero-yield pair subtables could otherwise burn a core
/// scanning O(N^2) probes without ever collecting enough to trip that cap. Exhausting this
/// budget stops the walk and marks the list truncated.
const MAX_KERN_PROBE_OPS: u64 = 10_000_000;

// --- Analysis result types --------------------------------------------------

/// One non-default kerning pair as displayed: the two characters and the raw kerning value
/// in font design units (negative tightens, positive loosens).
#[derive(Debug, Clone, PartialEq, Eq)]
struct KerningPairInfo {
    /// Left (first) glyph's character.
    left: char,
    /// Right (second) glyph's character.
    right: char,
    /// Kerning adjustment in font design units.
    value_units: i16,
}

/// Off-thread analysis of a font file: its supported characters and non-default kerning.
#[derive(Debug, Clone)]
struct FontAnalysis {
    /// Supported characters (sorted, control codepoints filtered, each confirmed via
    /// `glyph_index`).
    codepoints: Vec<char>,
    /// Non-default kerning pairs, deduped and capped at `MAX_KERNING_PAIRS`.
    kerning: Vec<KerningPairInfo>,
    /// True when the kerning list was truncated (probe cap, raw cap, or display cap).
    kerning_truncated: bool,
    /// Font units-per-em, used to scale kerning values to preview pixels. Never zero when
    /// parsed from a valid face; guarded before division anyway.
    units_per_em: u16,
}

/// A raw kerning pair collected before dedup/sort/cap.
#[derive(Debug, Clone, Copy)]
struct RawKerningPair {
    left: char,
    right: char,
    value: i16,
}

// --- Custom-kerning editor --------------------------------------------------

/// The values that identify ONE font to the own-typeface preview helpers: the registration
/// key (identity + content hash) plus the file and face its bytes come from.
///
/// Bundled because every own-typeface draw site needs all four and nothing else; passing
/// them one by one pushed helper signatures past the point clippy (and a reader) tolerates.
#[derive(Debug, Clone, Copy)]
struct TypefaceRef<'a> {
    /// Canonical font identity (`FontEntry::render_identity_name`).
    identity: &'a str,
    /// Hash of the representative file's bytes (`0` = unknown).
    content_hash: u64,
    /// Representative font FILE (byte source only).
    path: &'a Path,
    /// Representative face index within `path`.
    rep_face: usize,
}

impl TypefaceRef<'_> {
    /// The egui family drawing this font, or `None` while its bytes are still being read
    /// off the GUI thread (and permanently when it cannot be registered).
    fn family(&self, ctx: &egui::Context) -> Option<egui::FontFamily> {
        own_typeface_family(ctx, self.identity, self.content_hash, self.path, self.rep_face)
    }
}

/// State of the sibling "custom kerning pair" editor window: the pair being created or
/// edited, plus the validation message shown under its controls.
///
/// The edited pair is addressed by its stored KEY, never by a list index: the store can be
/// rewritten from elsewhere (another settings pane, the typing panel) while this window is
/// open, and an index would then silently rewrite a different row.
#[derive(Debug)]
struct CustomKerningEditorState {
    /// `(left, right)` of the pair being EDITED, or `None` when creating a new one.
    original: Option<(char, char)>,
    /// Buffer of the first character (clamped to one `char` every frame).
    left_buf: String,
    /// Buffer of the second character (clamped to one `char` every frame).
    right_buf: String,
    /// Advance delta in thousandths of an em (positive widens, negative tightens).
    offset_per_mille: f32,
    /// PREVIEW-ONLY text painted BEFORE the pair, so the user can judge the pair inside real
    /// words instead of in isolation. Several characters allowed.
    ///
    /// It is a VISUAL DEBUGGING AID and nothing else: never validated, never part of
    /// `CustomKerningPair`, never handed to `font_admin::set_custom_kerning`, never written to
    /// `fonts_data.json`. It dies with the editor, exactly like the buffers above — there is
    /// no per-pair "sample text" concept in the stored model, and inventing one here would
    /// leak a UI convenience into persisted user data.
    context_before: String,
    /// PREVIEW-ONLY text painted AFTER the pair. Same contract as `context_before`.
    context_after: String,
    /// PREVIEW-ONLY font size (points) the run is painted at, starting at
    /// `PAIR_PREVIEW_FONT_SIZE`. Same contract as the context fields: never validated against
    /// the font, never part of `CustomKerningPair`, never persisted, gone when the editor
    /// closes. It CANNOT change what the pair does — the offset is stored in thousandths of an
    /// em, so the gap scales with whatever size it is viewed at; this only decides how large
    /// the user inspects it.
    preview_font_size: f32,
    /// Localized validation message shown in the error color, cleared on the next attempt.
    error: Option<String>,
}

impl CustomKerningEditorState {
    /// Editor for a NEW pair: both character buffers empty, no offset, no preview context.
    fn creating() -> Self {
        Self {
            original: None,
            left_buf: String::new(),
            right_buf: String::new(),
            offset_per_mille: 0.0,
            context_before: String::new(),
            context_after: String::new(),
            preview_font_size: PAIR_PREVIEW_FONT_SIZE,
            error: None,
        }
    }

    /// Editor prefilled from an existing stored `pair`, keyed by its current characters. The
    /// preview context starts empty even here: it is not stored, so there is nothing to
    /// prefill it from.
    fn editing(pair: &CustomKerningPair) -> Self {
        Self {
            original: Some((pair.left, pair.right)),
            left_buf: pair.left.to_string(),
            right_buf: pair.right.to_string(),
            offset_per_mille: pair.offset_per_mille,
            context_before: String::new(),
            context_after: String::new(),
            preview_font_size: PAIR_PREVIEW_FONT_SIZE,
            error: None,
        }
    }
}

/// What one frame of the custom-kerning editor asks its owner to do. The owner holds the
/// stored list, so validation and persistence stay out of the window body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KerningEditorAction {
    /// Nothing was pressed: keep the editor open unchanged.
    Keep,
    /// Close without touching the stored list.
    Close,
    /// Validate the buffers and, if they pass, commit them and close.
    Save,
    /// Remove the edited pair and close (offered only while editing an existing pair).
    Delete,
}

// --- Window state -----------------------------------------------------------

/// State for the currently-open font-properties window (at most one at a time). Built from
/// a `FontEntry` snapshot so it never aliases the live category lists. Owned by
/// `FontSettingsEditorState`.
pub(super) struct FontPropertiesState {
    /// Representative font FILE path. ONLY the byte source of the glyph/kerning analysis
    /// and the own-typeface preview — every per-font SETTING is keyed by `identity`.
    path: PathBuf,
    /// Representative face index within `path` (0 for single-face files).
    rep_face: usize,
    /// Real family/name read from the font file (shown in the identity header).
    original_name: String,
    /// Canonical IDENTITY of the font (`FontEntry::render_identity_name`): the
    /// representative face's PostScript name, `%hash`-suffixed on a content contest. This
    /// is what the project persists and what the renderer resolves, so the window shows it
    /// next to the family name; it also keys the own-typeface registrations below.
    identity: String,
    /// Hash of the representative file's bytes (`FontEntry::content_hash`, `0` = unknown).
    /// Not shown: it is the byte discriminant of the own-typeface registrations below, so a
    /// font file replaced under the same PostScript name is previewed from its NEW bytes.
    content_hash: u64,
    /// File name of `path` (shown in the identity header).
    file_name: String,
    /// Representative face label, shown only for multi-face files (`None` otherwise).
    face_label: Option<String>,
    /// Base label used as the display-name hint and as the effective name when the
    /// override buffer is blank.
    default_label: String,
    /// Editable display-name buffer (prefilled from the current override).
    name_buf: String,
    /// Free-text preview buffer.
    preview: String,
    /// Off-thread analysis result; `None` until the worker completes. `Err` carries a
    /// human-readable reason for a parse/read failure.
    analysis: Option<Result<FontAnalysis, String>>,
    /// In-flight analysis load, if any.
    analysis_rx: Option<mpsc::Receiver<Result<FontAnalysis, String>>>,
    /// Virtual groups THIS font belongs to, as `(group name, per-group alias)`. Cached and
    /// refreshed when the shared font-config revision advances.
    member_of: Vec<(String, Option<String>)>,
    /// All virtual-group names (to offer the font's non-member groups for adding). Cached
    /// alongside `member_of`.
    all_group_names: Vec<String>,
    /// Store revision at which the group caches were built; `None` until the first refresh.
    groups_revision: Option<u64>,
    /// Per-group alias edit buffers, keyed by group name.
    group_alias_bufs: HashMap<String, String>,
    /// `WheelComboBox` selection index for the add-to-group control (over the non-member
    /// group list computed each frame).
    add_group_index: usize,
    /// This font's user-authored kerning pairs, in stored order. Cached and refreshed when
    /// the shared font-config revision advances.
    custom_kerning: Vec<CustomKerningPair>,
    /// Store revision at which `custom_kerning` was read; `None` forces a reload.
    custom_kerning_revision: Option<u64>,
    /// The open custom-kerning editor window, if any. Living on this state is what makes
    /// closing the properties window discard an unsaved edit with it.
    custom_kerning_editor: Option<CustomKerningEditorState>,
}

impl FontPropertiesState {
    /// Builds the window state from a `FontEntry` snapshot. Reads the current display-name
    /// override for the font (via `font_admin`) so the editor is prefilled; does not start
    /// the analysis (that begins lazily on the first `show`).
    pub(super) fn new(font: &FontEntry) -> Self {
        let rep_face = font.representative_face_index();
        let file_name = font
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| font.path().to_string_lossy().into_owned());
        let face_label = font.representative_face_label();
        let name_buf =
            font_admin::display_name_override(&font.render_identity_name()).unwrap_or_default();
        let default_label = super::font_settings::clean_font_display_name(font.label());
        Self {
            path: font.path().to_path_buf(),
            rep_face,
            original_name: font.original_name().to_string(),
            identity: font.render_identity_name(),
            content_hash: font.content_hash(),
            file_name,
            face_label,
            default_label,
            name_buf,
            preview: t!("typing.font_settings.properties_preview_sample").to_string(),
            analysis: None,
            analysis_rx: None,
            member_of: Vec::new(),
            all_group_names: Vec::new(),
            groups_revision: None,
            group_alias_bufs: HashMap::new(),
            add_group_index: 0,
            custom_kerning: Vec::new(),
            custom_kerning_revision: None,
            custom_kerning_editor: None,
        }
    }

    /// The four values every own-typeface draw site in this window needs.
    fn typeface(&self) -> TypefaceRef<'_> {
        TypefaceRef {
            identity: self.identity.as_str(),
            content_hash: self.content_hash,
            path: self.path.as_path(),
            rep_face: self.rep_face,
        }
    }

    /// Effective display name shown in the window title and used when applying: the trimmed
    /// override buffer when non-blank, else the base label.
    fn effective_display_name(&self) -> String {
        let trimmed = self.name_buf.trim();
        if trimmed.is_empty() {
            self.default_label.clone()
        } else {
            trimmed.to_string()
        }
    }

    /// Applies the current display-name buffer as the override (blank = reset to default).
    /// Bumps the shared store revision, so the settings category lists and typing panels
    /// reload automatically.
    fn apply_display_name(&mut self) {
        let trimmed = self.name_buf.trim();
        let value = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        font_admin::set_display_name_override(&self.identity, value);
    }

    /// Starts the off-thread font analysis if none is cached or in flight. Reads the font
    /// file and runs ttf-parser on a worker thread; the GUI thread only polls.
    fn maybe_start_analysis(&mut self) {
        if self.analysis.is_some() || self.analysis_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let path = self.path.clone();
        let face_index = self.rep_face;
        match thread::Builder::new()
            .name("font-properties-analyze".to_string())
            .spawn(move || {
                // A disconnected receiver only means the window was closed; ignore.
                let _ = tx.send(analyze_font_file(&path, face_index));
            }) {
            Ok(_handle) => self.analysis_rx = Some(rx),
            Err(err) => {
                crate::runtime_log::log_error(format!(
                    "[settings] failed to start font-properties analyze thread; error={err}"
                ));
                // Cache an error so the window shows a message instead of spinning forever.
                self.analysis =
                    Some(Err(t!("typing.font_settings.properties_analyze_error").to_string()));
            }
        }
    }

    /// Polls the in-flight analysis; caches the result when ready and repaints while pending.
    fn poll_analysis(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.analysis_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.analysis = Some(result);
                self.analysis_rx = None;
            }
            Err(TryRecvError::Empty) => ctx.request_repaint(),
            Err(TryRecvError::Disconnected) => {
                self.analysis_rx = None;
                self.analysis =
                    Some(Err(t!("typing.font_settings.properties_analyze_error").to_string()));
                crate::runtime_log::log_error(
                    "[settings] font-properties analyze thread ended without sending a result",
                );
            }
        }
    }

    /// Renders the whole window body (identity header, editor, preview, glyph grid, kerning).
    fn draw_body(&mut self, ui: &mut egui::Ui) {
        ui.label(tf!(
            "typing.font_settings.properties_original_name",
            name = self.original_name
        ));
        // The identity is the name the project stores and the renderer resolves; the file
        // path below it answers the different question "where does this font live".
        ui.label(tf!(
            "typing.font_settings.properties_identity",
            name = self.identity
        ));
        match &self.face_label {
            Some(face) => ui.label(tf!(
                "typing.font_settings.properties_file_face",
                file = self.file_name,
                face = face
            )),
            None => ui.label(tf!(
                "typing.font_settings.properties_file",
                file = self.file_name
            )),
        };
        ui.add_space(6.0);
        ui.separator();

        self.draw_display_name_editor(ui);
        ui.add_space(6.0);
        ui.separator();

        self.draw_groups_section(ui);
        ui.add_space(6.0);
        ui.separator();

        self.draw_preview(ui);
        ui.add_space(6.0);
        ui.separator();

        match &self.analysis {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("typing.font_settings.properties_analyzing_status"));
                });
            }
            Some(Err(err)) => {
                let color = ui.visuals().error_fg_color;
                ui.colored_label(color, t!("typing.font_settings.properties_analyze_error"));
                ui.small(err.as_str());
            }
            Some(Ok(analysis)) => {
                Self::draw_glyph_grid(
                    ui,
                    &self.identity,
                    self.content_hash,
                    &self.path,
                    self.rep_face,
                    analysis,
                );
                ui.add_space(6.0);
                Self::draw_kerning_section(
                    ui,
                    &self.identity,
                    self.content_hash,
                    &self.path,
                    self.rep_face,
                    analysis,
                );
            }
        }
        // Drawn OUTSIDE the analysis match on purpose: custom pairs are user data read from
        // `fonts_data.json`, so the section stays usable even when the font file itself
        // could not be analyzed. The separator is the TOP-LEVEL rhythm of this body (every
        // section above is closed by one); the two analysis subsections nest inside a single
        // such block and are spaced apart instead.
        ui.add_space(6.0);
        ui.separator();
        self.draw_custom_kerning_section(ui);
    }

    /// Renders the display-name editor: a text field prefilled with the override, plus an
    /// explicit apply button. Applying also happens on Enter while the field has focus.
    fn draw_display_name_editor(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("typing.font_settings.properties_display_name_label"));
        let (response, apply_clicked) = ui
            .horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.name_buf)
                        .id_salt("typing.font_settings.properties_display_name_edit")
                        .desired_width(280.0)
                        .hint_text(self.default_label.as_str()),
                );
                let apply_clicked = ui
                    .button(t!("typing.font_settings.properties_apply_button"))
                    .clicked();
                (response, apply_clicked)
            })
            .inner;
        // Apply on Enter (field committed) or the explicit button.
        let submitted =
            response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if submitted || apply_clicked {
            self.apply_display_name();
        }
        ui.small(t!("typing.font_settings.properties_display_name_hint"));
    }

    /// Reloads the cached virtual-group membership for this font when the shared font-config
    /// revision advances, seeding any missing alias buffers. Cheap in-memory reads, so this is
    /// GUI-thread safe.
    fn refresh_groups(&mut self) {
        let current = font_admin::fonts_revision();
        if self.groups_revision == Some(current) {
            return;
        }
        self.member_of = font_admin::virtual_groups_for_font(&self.identity);
        self.all_group_names = font_admin::list_virtual_groups()
            .into_iter()
            .map(|group| group.name)
            .collect();
        self.groups_revision = Some(current);
        // Prune buffers for groups this font no longer belongs to (removed/renamed elsewhere),
        // so a stale alias edit cannot linger and reappear if the font rejoins a group.
        self.group_alias_bufs
            .retain(|name, _| self.member_of.iter().any(|(group, _)| group == name));
        // Seed alias buffers only when missing so in-progress edits survive a refresh.
        for (name, alias) in &self.member_of {
            self.group_alias_bufs
                .entry(name.clone())
                .or_insert_with(|| alias.clone().unwrap_or_default());
        }
    }

    /// Renders the "Группы" section: the groups this font belongs to (with per-group alias
    /// editing and removal) plus an add-to-group control for its non-member groups.
    fn draw_groups_section(&mut self, ui: &mut egui::Ui) {
        self.refresh_groups();
        egui::CollapsingHeader::new(t!("typing.font_settings.properties_groups_header"))
            .id_salt("typing.font_settings.properties_groups_header")
            .default_open(false)
            .show(ui, |ui| {
                self.draw_groups_body(ui);
            });
    }

    /// Body of the "Группы" section. Membership mutations are collected and applied after the
    /// row loop so no store mutation happens mid-iteration.
    fn draw_groups_body(&mut self, ui: &mut egui::Ui) {
        // Group membership is keyed by the font's IDENTITY, never by its file.
        let identity = self.identity.clone();

        if self.member_of.is_empty() {
            ui.small(t!("typing.font_settings.properties_groups_none_hint"));
        } else {
            // Clone the membership list so the row closures can mutate the alias buffers.
            let member_of = self.member_of.clone();
            let mut alias_to_apply: Option<(String, String)> = None;
            let mut remove_from: Option<String> = None;
            for (name, _alias) in &member_of {
                let buf = self.group_alias_bufs.entry(name.clone()).or_default();
                ui.horizontal(|ui| {
                    ui.label(name.as_str());
                    let response = ui.add(
                        egui::TextEdit::singleline(buf)
                            .id_salt((
                                "typing.font_settings.properties_group_alias_edit",
                                name.as_str(),
                            ))
                            .desired_width(160.0)
                            .hint_text(t!(
                                "typing.font_settings.group_member_alias_placeholder"
                            )),
                    );
                    let submitted = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if ui
                        .button(t!("typing.font_settings.properties_apply_button"))
                        .clicked()
                        || submitted
                    {
                        alias_to_apply = Some((name.clone(), buf.clone()));
                    }
                    if ui
                        .small_button("✕")
                        .on_hover_text(t!(
                            "typing.font_settings.group_member_remove_tooltip"
                        ))
                        .clicked()
                    {
                        remove_from = Some(name.clone());
                    }
                });
            }
            if let Some((name, alias)) = alias_to_apply {
                let trimmed = alias.trim();
                // Blank clears the alias (reset to the font's own label).
                let value = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                font_admin::set_virtual_group_member_alias(&name, &identity, value);
            }
            if let Some(name) = remove_from {
                font_admin::remove_virtual_group_member(&name, &identity);
                self.group_alias_bufs.remove(&name);
            }
        }

        // Non-member groups: offer adding this font to one of them.
        let non_member: Vec<String> = self
            .all_group_names
            .iter()
            .filter(|name| {
                !self
                    .member_of
                    .iter()
                    .any(|(member_name, _)| member_name == *name)
            })
            .cloned()
            .collect();
        if !non_member.is_empty() {
            ui.add_space(4.0);
            if self.add_group_index >= non_member.len() {
                self.add_group_index = 0;
            }
            ui.horizontal(|ui| {
                ui.label(t!("typing.font_settings.properties_add_to_group_label"));
                WheelComboBox::from_id_salt("typing.font_settings.properties_add_to_group_combo")
                    .width(180.0)
                    .show_index(
                        ui,
                        &mut self.add_group_index,
                        non_member.len(),
                        |index| non_member[index].as_str(),
                    );
                if ui
                    .button(t!("typing.font_settings.add_button"))
                    .clicked()
                    && let Some(name) = non_member.get(self.add_group_index)
                {
                    font_admin::add_virtual_group_member(name, &identity);
                }
            });
        }
    }

    /// Renders the free-text preview: an editable line plus the same text drawn below in the
    /// font's own typeface. Restores the previous style font override afterward.
    fn draw_preview(&mut self, ui: &mut egui::Ui) {
        ui.label(t!("typing.font_settings.properties_preview_label"));
        ui.add(
            egui::TextEdit::singleline(&mut self.preview)
                .id_salt("typing.font_settings.properties_preview_edit")
                .desired_width(f32::INFINITY),
        );
        let prev_override = ui.style().override_font_id.clone();
        if let PreviewFontFamily::Ready(family) = request_font_family(
            ui.ctx(),
            &self.identity,
            self.content_hash,
            &self.path,
            self.rep_face,
        ) {
            ui.style_mut().override_font_id = Some(egui::FontId::new(PREVIEW_FONT_SIZE, family));
        }
        ui.label(self.preview.as_str());
        ui.style_mut().override_font_id = prev_override;
    }

    /// Renders the COLLAPSIBLE glyph section (collapsed by default): the supported-character
    /// count in the header, and inside it the virtualized grid of fixed-size cells, each
    /// drawing one character in the font's typeface with a `U+XXXX` hover tooltip. Only
    /// visible rows run. A full CJK inventory is thousands of cells, so the section starts
    /// closed and the kerning sections below it stay reachable without scrolling past it.
    fn draw_glyph_grid(
        ui: &mut egui::Ui,
        identity: &str,
        content_hash: u64,
        path: &Path,
        rep_face: usize,
        analysis: &FontAnalysis,
    ) {
        let count = analysis.codepoints.len();
        egui::CollapsingHeader::new(tf!(
            "typing.font_settings.properties_glyphs_header",
            count = count
        ))
        .id_salt("typing.font_settings.properties_glyphs_header")
        .default_open(false)
        .show(ui, |ui| {
            Self::draw_glyph_grid_body(ui, identity, content_hash, path, rep_face, analysis);
        });
    }

    /// Body of the glyph section: the virtualized grid itself, or the "no glyphs" note.
    fn draw_glyph_grid_body(
        ui: &mut egui::Ui,
        identity: &str,
        content_hash: u64,
        path: &Path,
        rep_face: usize,
        analysis: &FontAnalysis,
    ) {
        let count = analysis.codepoints.len();
        if count == 0 {
            ui.small(t!("typing.font_settings.properties_no_glyphs_status"));
            return;
        }
        let spacing_x = ui.spacing().item_spacing.x;
        // Columns from available width; mirrors the page-manager virtualized grid math.
        let columns = usize::max(
            1,
            ((ui.available_width() + spacing_x) / (GLYPH_CELL_SIZE + spacing_x)).floor() as usize,
        );
        let rows = count.div_ceil(columns);
        let family = own_typeface_family(ui.ctx(), identity, content_hash, path, rep_face);
        egui::ScrollArea::vertical()
            .id_salt("typing.font_settings.properties_glyph_grid")
            .max_height(GLYPH_GRID_MAX_HEIGHT)
            .auto_shrink([false, false])
            .show_rows(ui, GLYPH_CELL_SIZE, rows, |ui, row_range| {
                for row in row_range {
                    ui.horizontal(|ui| {
                        let start = row * columns;
                        let end = usize::min(start + columns, count);
                        for idx in start..end {
                            let Some(&ch) = analysis.codepoints.get(idx) else {
                                continue;
                            };
                            draw_glyph_cell(ui, ch, family.as_ref());
                        }
                    });
                }
            });
    }

    /// Renders the collapsible kerning-pair list (virtualized). Shows the pair in the font's
    /// own typeface, the two characters, the value in font units, and the value scaled to the
    /// preview font size in pixels. Notes truncation when the list was capped.
    fn draw_kerning_section(
        ui: &mut egui::Ui,
        identity: &str,
        content_hash: u64,
        path: &Path,
        rep_face: usize,
        analysis: &FontAnalysis,
    ) {
        let count = analysis.kerning.len();
        egui::CollapsingHeader::new(tf!(
            "typing.font_settings.properties_kerning_header",
            count = count
        ))
        .id_salt("typing.font_settings.properties_kerning_header")
        .default_open(false)
        .show(ui, |ui| {
            if count == 0 {
                // A partial scan that collected nothing must still say so, otherwise a
                // truncated probe reads as "this font has no kerning".
                if analysis.kerning_truncated {
                    ui.small(tf!(
                        "typing.font_settings.properties_kerning_truncated_note",
                        cap = MAX_KERNING_PAIRS
                    ));
                } else {
                    ui.small(t!("typing.font_settings.properties_no_kerning_status"));
                }
                return;
            }
            if analysis.kerning_truncated {
                ui.small(tf!(
                    "typing.font_settings.properties_kerning_truncated_note",
                    cap = MAX_KERNING_PAIRS
                ));
            }
            let family = own_typeface_family(ui.ctx(), identity, content_hash, path, rep_face);
            let units_per_em = analysis.units_per_em;
            egui::ScrollArea::vertical()
                .id_salt("typing.font_settings.properties_kerning_list")
                .max_height(KERNING_LIST_MAX_HEIGHT)
                .auto_shrink([false, true])
                .show_rows(ui, KERNING_ROW_HEIGHT, count, |ui, range| {
                    for row in range {
                        let Some(pair) = analysis.kerning.get(row) else {
                            continue;
                        };
                        draw_kerning_row(ui, pair, family.as_ref(), units_per_em);
                    }
                });
        });
    }

    /// Reloads the cached custom kerning pairs when the shared font-config revision advances.
    /// Cheap in-memory read, so this is GUI-thread safe.
    fn refresh_custom_kerning(&mut self) {
        let current = font_admin::fonts_revision();
        if self.custom_kerning_revision == Some(current) {
            return;
        }
        self.custom_kerning = font_admin::custom_kerning(&self.identity);
        self.custom_kerning_revision = Some(current);
    }

    /// Renders the "Кастомные кернинговые пары" section: the user's own pairs for this font
    /// plus the "+ Создать" button that opens the editor window.
    ///
    /// Open by DEFAULT (unlike its collapsed siblings): it carries the feature's only entry
    /// point, and a collapsed-by-default header would hide it. Clicking a row opens the same
    /// editor prefilled with that pair.
    fn draw_custom_kerning_section(&mut self, ui: &mut egui::Ui) {
        self.refresh_custom_kerning();
        // `Some(None)` = create a new pair, `Some(Some(index))` = edit the stored pair at
        // that index. Collected inside the row loop and acted on after it, so the editor is
        // never swapped while the list it was launched from is still borrowed.
        let mut open_editor: Option<Option<usize>> = None;
        let count = self.custom_kerning.len();
        let pairs = self.custom_kerning.as_slice();
        let typeface = self.typeface();
        egui::CollapsingHeader::new(tf!(
            "typing.font_settings.properties_custom_kerning_header",
            count = count
        ))
        .id_salt("typing.font_settings.properties_custom_kerning_header")
        .default_open(true)
        .show(ui, |ui| {
            if ui
                .button(t!("typing.font_settings.properties_custom_kerning_create_button"))
                .clicked()
            {
                open_editor = Some(None);
            }
            if count == 0 {
                ui.small(t!("typing.font_settings.properties_custom_kerning_empty_hint"));
                return;
            }
            let family = typeface.family(ui.ctx());
            egui::ScrollArea::vertical()
                .id_salt("typing.font_settings.properties_custom_kerning_list")
                .max_height(CUSTOM_KERNING_LIST_MAX_HEIGHT)
                .auto_shrink([false, true])
                .show_rows(ui, CUSTOM_KERNING_ROW_HEIGHT, count, |ui, range| {
                    for row in range {
                        let Some(pair) = pairs.get(row) else {
                            continue;
                        };
                        if draw_custom_kerning_row(ui, pair, family.as_ref()) {
                            open_editor = Some(Some(row));
                        }
                    }
                });
        });
        if let Some(target) = open_editor {
            self.open_custom_kerning_editor(target);
        }
    }

    /// Opens the editor window: empty for `None`, prefilled from the stored pair at `Some`.
    /// An index that no longer exists (the store changed under the list) opens a blank editor
    /// rather than doing nothing, which is the reading the user can act on.
    fn open_custom_kerning_editor(&mut self, index: Option<usize>) {
        let editor = match index.and_then(|index| self.custom_kerning.get(index)) {
            Some(pair) => CustomKerningEditorState::editing(pair),
            None => CustomKerningEditorState::creating(),
        };
        self.custom_kerning_editor = Some(editor);
    }

    /// Renders the custom-kerning editor window when open and applies its outcome.
    ///
    /// A SIBLING window of the properties window (never nested inside its `show` closure),
    /// mirroring the group editor in `font_groups.rs`. Validation lives here because only the
    /// owner holds the stored list; a rejected save keeps the editor open with a localized
    /// message. A committed edit is written through `font_admin::set_custom_kerning`, which
    /// persists off the GUI thread and bumps the revision the renderer reloads on.
    fn draw_custom_kerning_editor(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.custom_kerning_editor.take() else {
            return;
        };
        // The built-in kerning is only known once the off-thread analysis landed; until then
        // the override read-out simply says nothing rather than guessing.
        let builtin = match &self.analysis {
            Some(Ok(analysis)) => Some(analysis),
            None | Some(Err(_)) => None,
        };
        let typeface = self.typeface();
        // The other custom pairs the preview run must honour. `editor` is already OUT of
        // `self`, so this immutable borrow of the cached list is free of conflict; the entry
        // the editor was opened on is filtered back out by `live_custom_offset_per_mille`.
        let stored = self.custom_kerning.as_slice();
        let mut window_open = true;
        let mut action = KerningEditorAction::Keep;
        egui::Window::new(t!(
            "typing.font_settings.properties_custom_kerning_editor_title"
        ))
        // The title is localized, so pin a stable id (05-ids-and-i18n.md).
        .id(egui::Id::new(
            "typing.font_settings.properties_custom_kerning_editor_window",
        ))
        .open(&mut window_open)
        .collapsible(false)
        .resizable(false)
        .default_width(380.0)
        .show(ctx, |ui| {
            action = draw_custom_kerning_editor_body(ui, &mut editor, typeface, builtin, stored);
        });
        if !window_open {
            // The window's own close button is a cancel: nothing is written.
            return;
        }
        match action {
            KerningEditorAction::Keep => self.custom_kerning_editor = Some(editor),
            KerningEditorAction::Close => {}
            KerningEditorAction::Save => self.commit_custom_kerning_editor(editor),
            KerningEditorAction::Delete => self.delete_edited_custom_kerning(&editor),
        }
    }

    /// Validates `editor` against the stored list and, when it passes, commits the pair and
    /// closes the editor. A failure re-arms the editor with a localized message instead.
    fn commit_custom_kerning_editor(&mut self, mut editor: CustomKerningEditorState) {
        let (Some(left), Some(right)) = (
            editor.left_buf.chars().next(),
            editor.right_buf.chars().next(),
        ) else {
            editor.error = Some(
                t!("typing.font_settings.properties_custom_kerning_chars_required_error")
                    .to_string(),
            );
            self.custom_kerning_editor = Some(editor);
            return;
        };
        // A second entry for the same two characters would be silently dropped on save (the
        // store keeps the first), so it is refused here where the user can still fix it.
        if conflicting_pair_index(&self.custom_kerning, left, right, editor.original).is_some() {
            editor.error = Some(
                t!("typing.font_settings.properties_custom_kerning_duplicate_error").to_string(),
            );
            self.custom_kerning_editor = Some(editor);
            return;
        }
        let pair = CustomKerningPair { left, right, offset_per_mille: editor.offset_per_mille };
        apply_pair_edit(&mut self.custom_kerning, editor.original, pair);
        self.store_custom_kerning();
    }

    /// Removes the pair `editor` was opened on and closes the editor. Creating a new pair has
    /// nothing to delete, so that case writes nothing.
    fn delete_edited_custom_kerning(&mut self, editor: &CustomKerningEditorState) {
        let Some(key) = editor.original else {
            return;
        };
        self.custom_kerning
            .retain(|pair| (pair.left, pair.right) != key);
        self.store_custom_kerning();
    }

    /// Persists the cached list for this font and invalidates the cache so the next frame
    /// reads back what was actually STORED (the store sanitizes what it is given).
    fn store_custom_kerning(&mut self) {
        font_admin::set_custom_kerning(&self.identity, self.custom_kerning.clone());
        self.custom_kerning_revision = None;
    }
}

/// Renders the font-properties window for `state`. Returns `false` when the user closed it
/// (via the window's close button), so the caller drops the state. Non-blocking: the glyph
/// and kerning analysis runs on a worker thread; the window shows a loading state until it
/// completes.
pub(super) fn show(ctx: &egui::Context, state: &mut FontPropertiesState) -> bool {
    state.maybe_start_analysis();
    state.poll_analysis(ctx);

    let mut window_open = true;
    let title = tf!(
        "typing.font_settings.properties_window_title",
        name = state.effective_display_name()
    );
    egui::Window::new(title)
        // The title changes with the display name, so pin a stable id (05-ids-and-i18n.md).
        .id(egui::Id::new("typing.font_settings.properties_window"))
        .open(&mut window_open)
        .collapsible(false)
        .resizable(true)
        .default_size([560.0, 640.0])
        // Sections carry their own bounded scroll areas so both the glyph grid and the
        // kerning list stay reachable; the outer window must not add a second vscroll.
        .vscroll(false)
        .show(ctx, |ui| {
            state.draw_body(ui);
        });

    // A SIBLING window, drawn at the top level of this function — never inside the closure
    // above. Nesting a `Window` inside another window's body is what the group editor in
    // `font_groups.rs` avoids for the same reason: the inner window would inherit the outer
    // one's layer and clip rect instead of floating on its own.
    state.draw_custom_kerning_editor(ctx);

    window_open
}

/// The egui family that draws this font's own typeface, or `None` while its bytes are
/// still being read off the GUI thread (and permanently when it cannot be registered).
///
/// A thin adapter over `widgets::request_font_family` for the two grids that take an
/// `Option<&egui::FontFamily>` per cell: their cells fall back to the interface font,
/// which is exactly what `Pending` and `Unavailable` both mean for them.
fn own_typeface_family(
    ctx: &egui::Context,
    identity: &str,
    content_hash: u64,
    path: &Path,
    rep_face: usize,
) -> Option<egui::FontFamily> {
    match request_font_family(ctx, identity, content_hash, path, rep_face) {
        PreviewFontFamily::Ready(family) => Some(family),
        PreviewFontFamily::Pending | PreviewFontFamily::Unavailable => None,
    }
}

/// Draws one glyph cell of fixed footprint: the character centered in the font's typeface,
/// a faint hover highlight, and a `U+XXXX` + character hover tooltip. Painter-only text so
/// the cell keeps a stable size regardless of the glyph's intrinsic metrics.
fn draw_glyph_cell(ui: &mut egui::Ui, ch: char, family: Option<&egui::FontFamily>) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(GLYPH_CELL_SIZE, GLYPH_CELL_SIZE),
        egui::Sense::hover(),
    );
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.bg_fill);
    }
    let font_id = match family {
        Some(fam) => egui::FontId::new(GLYPH_PREVIEW_FONT_SIZE, fam.clone()),
        None => egui::FontId::proportional(GLYPH_PREVIEW_FONT_SIZE),
    };
    let color = ui.visuals().text_color();
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        ch.to_string(),
        font_id,
        color,
    );
    response.on_hover_text(tf!(
        "typing.font_settings.properties_glyph_tooltip",
        code = format!("U+{:04X}", u32::from(ch)),
        ch = ch
    ));
}

/// Draws one kerning-list row: the pair rendered in the font's own typeface, the two
/// characters as plain text, the value in font units, and the value scaled to `PREVIEW_FONT_SIZE`
/// pixels (only when `units_per_em` is non-zero).
fn draw_kerning_row(
    ui: &mut egui::Ui,
    pair: &KerningPairInfo,
    family: Option<&egui::FontFamily>,
    units_per_em: u16,
) {
    ui.horizontal(|ui| {
        let prev_override = ui.style().override_font_id.clone();
        if let Some(fam) = family {
            ui.style_mut().override_font_id = Some(egui::FontId::new(20.0, fam.clone()));
        }
        ui.label(format!("{}{}", pair.left, pair.right));
        ui.style_mut().override_font_id = prev_override;

        ui.separator();
        ui.label(tf!(
            "typing.font_settings.properties_kerning_pair_chars",
            left = pair.left,
            right = pair.right
        ));
        ui.separator();
        ui.label(tf!(
            "typing.font_settings.properties_kerning_units",
            value = pair.value_units
        ));
        if units_per_em > 0 {
            let px = f32::from(pair.value_units) * PREVIEW_FONT_SIZE / f32::from(units_per_em);
            ui.label(tf!(
                "typing.font_settings.properties_kerning_px",
                px = format!("{px:.1}")
            ));
        }
    });
}

// --- Custom kerning: rows, editor window, pure logic ------------------------

/// Draws one custom-kerning row as a single clickable strip and returns whether it was
/// clicked (the caller then opens the editor on that pair).
///
/// Painter-drawn rather than assembled from widgets so the row keeps exactly
/// `CUSTOM_KERNING_ROW_HEIGHT` regardless of the glyphs' intrinsic metrics — `show_rows`
/// virtualizes on that height and a taller row would drift out of its slot.
fn draw_custom_kerning_row(
    ui: &mut egui::Ui,
    pair: &CustomKerningPair,
    family: Option<&egui::FontFamily>,
) -> bool {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, CUSTOM_KERNING_ROW_HEIGHT),
        egui::Sense::click(),
    );
    let visuals = ui.visuals().clone();
    let body_font = egui::TextStyle::Body.resolve(ui.style());
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(rect, 4.0, visuals.widgets.hovered.bg_fill);
    }
    let color = visuals.text_color();
    let pair_font = match family {
        Some(fam) => egui::FontId::new(20.0, fam.clone()),
        None => egui::FontId::proportional(20.0),
    };
    // The pair itself is always shown in the font it belongs to; the columns beside it are
    // metadata and stay in the interface font.
    painter.text(
        egui::pos2(rect.left() + 6.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{}{}", pair.left, pair.right),
        pair_font,
        color,
    );
    painter.text(
        egui::pos2(rect.left() + CUSTOM_KERNING_PAIR_COL_WIDTH, rect.center().y),
        egui::Align2::LEFT_CENTER,
        tf!(
            "typing.font_settings.properties_kerning_pair_chars",
            left = pair.left,
            right = pair.right
        ),
        body_font.clone(),
        color,
    );
    painter.text(
        egui::pos2(rect.right() - 6.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        tf!(
            "typing.font_settings.properties_custom_kerning_value",
            value = format_per_mille(pair.offset_per_mille)
        ),
        body_font,
        color,
    );
    response
        .on_hover_text(t!(
            "typing.font_settings.properties_custom_kerning_row_tooltip"
        ))
        .clicked()
}

/// Renders the body of the custom-kerning editor window and reports what the user asked for.
///
/// `builtin` is the font's own analysis when it finished, used for the read-out that tells the
/// user which built-in pair the custom value replaces AND for kerning the preview run's
/// context pairs. `stored` is the font's other custom pairs, so the context renders with the
/// user's whole custom kerning applied and not just the pair being edited. Writes nothing:
/// every store mutation is the owner's (`FontPropertiesState`) job.
fn draw_custom_kerning_editor_body(
    ui: &mut egui::Ui,
    editor: &mut CustomKerningEditorState,
    typeface: TypefaceRef<'_>,
    builtin: Option<&FontAnalysis>,
    stored: &[CustomKerningPair],
) -> KerningEditorAction {
    let fields_changed = ui
        .horizontal(|ui| {
            let left_changed = draw_single_char_field(
                ui,
                t!("typing.font_settings.properties_custom_kerning_left_label"),
                "typing.font_settings.properties_custom_kerning_left_edit",
                &mut editor.left_buf,
            );
            let right_changed = draw_single_char_field(
                ui,
                t!("typing.font_settings.properties_custom_kerning_right_label"),
                "typing.font_settings.properties_custom_kerning_right_edit",
                &mut editor.right_buf,
            );
            left_changed || right_changed
        })
        .inner;
    // A validation message is about the characters that were submitted; the moment the user
    // edits them it is stale, so it goes away instead of sitting under a fixed field.
    if fields_changed {
        editor.error = None;
    }
    let left = clamp_to_single_char(&mut editor.left_buf);
    let right = clamp_to_single_char(&mut editor.right_buf);

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        draw_context_field(
            ui,
            t!("typing.font_settings.properties_custom_kerning_context_before_label"),
            "typing.font_settings.properties_custom_kerning_context_before_edit",
            &mut editor.context_before,
        );
        draw_context_field(
            ui,
            t!("typing.font_settings.properties_custom_kerning_context_after_label"),
            "typing.font_settings.properties_custom_kerning_context_after_edit",
            &mut editor.context_after,
        );
    });
    ui.small(t!("typing.font_settings.properties_custom_kerning_context_note"));

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(t!("typing.font_settings.properties_custom_kerning_preview_label"));
        ui.add_space(8.0);
        ui.label(t!(
            "typing.font_settings.properties_custom_kerning_preview_size_label"
        ));
        ui.add(
            WheelSpinBox::new(&mut editor.preview_font_size)
                .range(PAIR_PREVIEW_FONT_SIZE_MIN..=PAIR_PREVIEW_FONT_SIZE_MAX)
                .speed(1.0)
                .wheel_step(2.0)
                .fixed_decimals(0),
        )
        // Bounds are formatted from the constants themselves so the text cannot drift from
        // what the widget actually accepts.
        .on_hover_text(tf!(
            "typing.font_settings.properties_custom_kerning_preview_size_hint",
            min = format!("{PAIR_PREVIEW_FONT_SIZE_MIN:.0}"),
            max = format!("{PAIR_PREVIEW_FONT_SIZE_MAX:.0}")
        ));
    });
    let family = typeface.family(ui.ctx());
    // Snapshot what the kerning closure needs BEFORE it borrows anything: `editor` stays
    // mutably available for the offset wheel below.
    let original = editor.original;
    // The pair under edit contributes its LIVE characters and LIVE offset, which is what makes
    // the preview follow the wheel instead of the stored value.
    let live_pair = left
        .zip(right)
        .map(|(left, right)| (left, right, editor.offset_per_mille));
    let run = build_preview_run(&editor.context_before, left, right, &editor.context_after);
    // The live preview size, snapshotted with the rest: every gap is converted against the SAME
    // size the glyphs are painted at, which is what keeps the preview a faithful scale model of
    // the pair at any size.
    let preview_font_size = editor.preview_font_size;
    let kern_px = |left: char, right: char| {
        resolve_run_kern_per_mille(stored, original, live_pair, builtin, left, right) / 1000.0
            * preview_font_size
    };
    draw_pair_preview(ui, &run, kern_px, family.as_ref(), preview_font_size);
    // A capped built-in kerning list makes the CONTEXT lie: a pair whose own kerning fell
    // outside `MAX_KERNING_PAIRS` renders unkerned here while the renderer kerns it. Same
    // honesty `draw_kerning_section` owes its own list, and only worth saying when there is
    // context to be wrong about.
    let has_context = !editor.context_before.is_empty() || !editor.context_after.is_empty();
    if has_context && builtin.is_some_and(|analysis| analysis.kerning_truncated) {
        ui.small(t!(
            "typing.font_settings.properties_custom_kerning_context_truncated_note"
        ));
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(t!("typing.font_settings.properties_custom_kerning_offset_label"));
        ui.add(
            WheelSpinBox::new(&mut editor.offset_per_mille)
                .range(-CUSTOM_KERNING_LIMIT_PER_MILLE..=CUSTOM_KERNING_LIMIT_PER_MILLE)
                .speed(1.0)
                .wheel_step(1.0)
                .fixed_decimals(0)
                .suffix(t!(
                    "typing.font_settings.properties_custom_kerning_offset_suffix"
                )),
        )
        // `WheelSpinBox` clamps an out-of-range value it was handed (a hand edit in
        // `fonts_data.json`) to the nearest bound, and Save then persists the clamp. The
        // bounds are formatted from the constant itself so the text cannot drift from it.
        .on_hover_text(tf!(
            "typing.font_settings.properties_custom_kerning_offset_range_hint",
            min = format!("{:.0}", -CUSTOM_KERNING_LIMIT_PER_MILLE),
            max = format!("{CUSTOM_KERNING_LIMIT_PER_MILLE:.0}")
        ));
        // The ‰ value is abstract; the px read-out says what it does at the preview size,
        // exactly as the built-in kerning list already shows both. It follows the LIVE preview
        // size, not a fixed one: the strip above is what the number is supposed to describe, so
        // pinning it to some other size would make the read-out describe nothing on screen.
        let px = editor.offset_per_mille / 1000.0 * preview_font_size;
        ui.label(tf!(
            "typing.font_settings.properties_kerning_px",
            px = format!("{px:.1}")
        ));
    });
    ui.small(t!("typing.font_settings.properties_custom_kerning_offset_hint"));

    // The override read-out is only honest once the analysis FINISHED and scanned the whole
    // font: a pending scan knows nothing yet, and a TRUNCATED one finding no pair is not
    // evidence that the font has none — the same trap `draw_kerning_section` guards against.
    // Either way the custom value still applies; there is simply nothing to say about it.
    if let (Some(left), Some(right), Some(analysis)) = (left, right, builtin) {
        match builtin_offset_per_mille(analysis, left, right) {
            Some(value) => {
                ui.small(tf!(
                    "typing.font_settings.properties_custom_kerning_builtin_note",
                    value = format_per_mille(value)
                ));
            }
            None if !analysis.kerning_truncated => {
                ui.small(t!(
                    "typing.font_settings.properties_custom_kerning_no_builtin_note"
                ));
            }
            None => {}
        }
    }

    if let Some(message) = editor.error.as_deref() {
        let color = ui.visuals().error_fg_color;
        ui.colored_label(color, message);
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let mut action = KerningEditorAction::Keep;
        if ui
            .button(t!("typing.font_settings.properties_custom_kerning_save_button"))
            .clicked()
        {
            action = KerningEditorAction::Save;
        }
        if ui
            .button(t!(
                "typing.font_settings.properties_custom_kerning_cancel_button"
            ))
            .clicked()
        {
            action = KerningEditorAction::Close;
        }
        // Only an EXISTING pair can be deleted; a pair being created has nothing stored yet.
        if editor.original.is_some()
            && ui
                .button(t!(
                    "typing.font_settings.properties_custom_kerning_delete_button"
                ))
                .clicked()
        {
            action = KerningEditorAction::Delete;
        }
        action
    })
    .inner
}

/// Draws one labelled single-character field and reports whether the user changed it this
/// frame (the caller uses that to drop a stale validation message).
///
/// `id_salt` is a persistence key, never a caption, so it stays a literal
/// (`dev-docs/i18n_exclusions.md`). `char_limit(1)` keeps typing to one character; a paste is
/// still clamped by the caller, which is the only place that sees the final buffer.
fn draw_single_char_field(ui: &mut egui::Ui, label: &str, id_salt: &str, buf: &mut String) -> bool {
    ui.vertical(|ui| {
        ui.label(label);
        ui.add(
            egui::TextEdit::singleline(buf)
                .id_salt(id_salt)
                .char_limit(1)
                .desired_width(48.0),
        )
        // A pasted grapheme CLUSTER (a combining sequence, an emoji ZWJ sequence) is reduced
        // to its first `char` by the caller's clamp, because a kerning pair is defined
        // between exactly two `char`s. The reduction is correct but invisible, so the hover
        // says it out loud instead of letting the user wonder what was stored.
        .on_hover_text(t!("typing.font_settings.properties_custom_kerning_char_hint"))
        .changed()
    })
    .inner
}

/// Draws one labelled PREVIEW-CONTEXT field: several characters, capped at
/// `CUSTOM_KERNING_CONTEXT_CHAR_LIMIT`.
///
/// `id_salt` is a persistence key, never a caption, so it stays a literal
/// (`dev-docs/i18n_exclusions.md`). Nothing is reported back because nothing depends on the field
/// changing: the content is preview-only, so it neither validates nor invalidates anything.
fn draw_context_field(ui: &mut egui::Ui, label: &str, id_salt: &str, buf: &mut String) {
    ui.vertical(|ui| {
        ui.label(label);
        ui.add(
            egui::TextEdit::singleline(buf)
                .id_salt(id_salt)
                .char_limit(CUSTOM_KERNING_CONTEXT_CHAR_LIMIT)
                .desired_width(130.0),
        )
        // The cap is silent when it bites (a longer paste simply stops), so the hover states
        // it, together with the fact that this text is never saved.
        .on_hover_text(tf!(
            "typing.font_settings.properties_custom_kerning_context_hint",
            limit = CUSTOM_KERNING_CONTEXT_CHAR_LIMIT
        ));
    });
}

/// Assembles the editor's preview run: the "before" context, whichever pair characters are
/// actually typed, then the "after" context.
///
/// Each context side is bounded to `CUSTOM_KERNING_CONTEXT_CHAR_LIMIT` CHARACTERS (never
/// bytes), which keeps the per-frame layout work bounded no matter what the widget let
/// through. A pair character that has not been typed yet is simply absent from the run, so an
/// editor with neither context nor characters yields an EMPTY run and the preview paints only
/// its background strip — the behaviour the two-glyph preview had before the context existed.
fn build_preview_run(
    before: &str,
    left: Option<char>,
    right: Option<char>,
    after: &str,
) -> Vec<char> {
    let mut run: Vec<char> = before
        .chars()
        .take(CUSTOM_KERNING_CONTEXT_CHAR_LIMIT)
        .collect();
    run.extend(left);
    run.extend(right);
    run.extend(after.chars().take(CUSTOM_KERNING_CONTEXT_CHAR_LIMIT));
    run
}

/// The CUSTOM offset (thousandths of an em) binding `left` and `right` under the editor's LIVE
/// state, or `None` when no custom pair binds them.
///
/// `live` is `(left, right, offset)` as the editor holds them RIGHT NOW, so turning the offset
/// wheel moves the preview immediately. `original` is the stored key the editor was opened on;
/// that entry is excluded from `stored`, which is what makes a re-key behave: while `A/V` is
/// being edited and its second character becomes `W`, the live pair is `A/W` and the stored
/// `A/V` stops applying instead of ghosting alongside it.
fn live_custom_offset_per_mille(
    stored: &[CustomKerningPair],
    original: Option<(char, char)>,
    live: Option<(char, char, f32)>,
    left: char,
    right: char,
) -> Option<f32> {
    let from_live = live
        .filter(|&(live_left, live_right, _)| live_left == left && live_right == right)
        .map(|(_, _, offset)| offset);
    from_live.or_else(|| {
        stored
            .iter()
            .find(|pair| {
                pair.left == left && pair.right == right && Some((pair.left, pair.right)) != original
            })
            .map(|pair| pair.offset_per_mille)
    })
}

/// Kerning applied between `left` and `right` in the preview run, in thousandths of an em.
///
/// Strict precedence, chosen to match what the renderer does rather than what is easy here:
/// 1. the custom pair under the editor's live state (`live_custom_offset_per_mille`), which
///    REPLACES the font's own value instead of adding to it — so a custom `0.0` cancels a
///    built-in pair, and the preview cannot disagree with the rendered balloon;
/// 2. else the font's built-in value from `analysis`, converted from design units;
/// 3. else zero.
///
/// `analysis` is `None` until the off-thread scan lands and after a parse failure; step 2 is
/// then skipped and only custom pairs kern the run.
///
/// KNOWN FIDELITY LIMIT: `FontAnalysis::kerning` is deduped, sorted by DESCENDING MAGNITUDE
/// and capped at `MAX_KERNING_PAIRS`, so a context pair whose built-in kerning fell outside the
/// cap renders UNKERNED here while the real renderer kerns it. `kerning_truncated` flags that
/// case and the editor says so out loud.
fn resolve_run_kern_per_mille(
    stored: &[CustomKerningPair],
    original: Option<(char, char)>,
    live: Option<(char, char, f32)>,
    analysis: Option<&FontAnalysis>,
    left: char,
    right: char,
) -> f32 {
    live_custom_offset_per_mille(stored, original, live, left, right)
        .or_else(|| analysis.and_then(|analysis| builtin_offset_per_mille(analysis, left, right)))
        .unwrap_or(0.0)
}

/// Pen positions of one laid-out preview run, in pixels relative to the run's left edge.
#[derive(Debug, Clone, PartialEq)]
struct PreviewRunLayout {
    /// Pen x of each character, one entry per character of the run, in run order.
    offsets: Vec<f32>,
    /// Total extent of the run: the last pen plus that character's own advance. `0.0` for an
    /// empty run. This is what the caller centres on.
    width: f32,
}

/// Lays out `chars` as ONE kerned run: the pen steps from `chars[i]` to `chars[i + 1]` by
/// `advance_of(chars[i]) + kern_px(chars[i], chars[i + 1])`.
///
/// `advance_of` yields a character's own advance in pixels; `kern_px` yields the adjustment for
/// an ORDERED pair, already in pixels. Both are plain lookups, which is what keeps this
/// function free of any egui context and therefore unit-testable.
///
/// An empty run yields no offsets and zero width.
fn layout_preview_run<A, K>(chars: &[char], advance_of: A, kern_px: K) -> PreviewRunLayout
where
    A: Fn(char) -> f32,
    K: Fn(char, char) -> f32,
{
    let mut offsets = Vec::with_capacity(chars.len());
    let mut pen = 0.0_f32;
    for (index, &ch) in chars.iter().enumerate() {
        offsets.push(pen);
        pen += advance_of(ch);
        // The kerning belongs to the GAP, so the last character contributes none.
        if let Some(&next) = chars.get(index + 1) {
            pen += kern_px(ch, next);
        }
    }
    PreviewRunLayout { offsets, width: pen }
}

/// Paints the preview run (context + edited pair + context) at `font_size` points, centred on
/// its own extent, with `kern_px` applied in every gap.
///
/// `font_size` is the editor's live preview size and must be the SAME size `kern_px` converted
/// its thousandths-of-an-em values against, or the gaps would not scale with the glyphs.
///
/// egui 0.35 applies NO kerning during text layout, so each character is painted separately at
/// a pen computed by `layout_preview_run`; a single-glyph galley's width IS that glyph's
/// advance in that layout, which is exactly the base a kerning value adjusts.
///
/// Degenerate inputs, all deliberate: an EMPTY run paints only the background strip (what an
/// editor with nothing typed yet shows, unchanged from the two-glyph preview); a character the
/// font does not contain is painted by egui's own fallback and MEASURED the same way, so the
/// run stays self-consistent rather than silently mis-spaced; `family` is `None` while the
/// font's bytes are still being read off the GUI thread, and permanently when it cannot be
/// registered, in which case the whole run falls back to the interface font exactly as the
/// glyph grid does.
fn draw_pair_preview(
    ui: &mut egui::Ui,
    run: &[char],
    kern_px: impl Fn(char, char) -> f32,
    family: Option<&egui::FontFamily>,
    font_size: f32,
) {
    // The strip grows with the glyphs: a fixed height would let a larger size paint outside it.
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), font_size * PAIR_PREVIEW_HEIGHT_RATIO),
        egui::Sense::hover(),
    );
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
    if run.is_empty() {
        return;
    }
    let font_id = match family {
        Some(fam) => egui::FontId::new(font_size, fam.clone()),
        None => egui::FontId::proportional(font_size),
    };
    let color = visuals.text_color();
    // Galleys are cached by epaint on the layout job, so re-measuring the same bounded set of
    // single characters every frame costs a cache hit, not a shaping pass.
    let advance_of = |ch: char| {
        painter
            .layout_no_wrap(ch.to_string(), font_id.clone(), color)
            .size()
            .x
    };
    let layout = layout_preview_run(run, advance_of, kern_px);
    // Centre the whole run on its own extent so a large negative offset cannot walk it out of
    // the strip.
    let start_x = rect.center().x - layout.width / 2.0;
    let baseline_y = rect.center().y;
    for (&ch, &offset) in run.iter().zip(layout.offsets.iter()) {
        painter.text(
            egui::pos2(start_x + offset, baseline_y),
            egui::Align2::LEFT_CENTER,
            ch.to_string(),
            font_id.clone(),
            color,
        );
    }
}

/// Formats an offset in thousandths of an em for display: whole units, signed.
fn format_per_mille(value: f32) -> String {
    format!("{value:.0}")
}

/// Converts a kerning value in font DESIGN UNITS to thousandths of an em, the unit custom
/// pairs are stored in. `None` when the face declares `units_per_em == 0`, where the ratio
/// has no meaning (and would divide by zero).
fn design_units_to_per_mille(value_units: i16, units_per_em: u16) -> Option<f32> {
    if units_per_em == 0 {
        return None;
    }
    Some(f32::from(value_units) * 1000.0 / f32::from(units_per_em))
}

/// The font's OWN kerning for `(left, right)` in thousandths of an em, or `None` when the
/// analysis holds no such pair. Used only for the "this replaces the built-in pair" read-out.
fn builtin_offset_per_mille(analysis: &FontAnalysis, left: char, right: char) -> Option<f32> {
    let pair = analysis
        .kerning
        .iter()
        .find(|pair| pair.left == left && pair.right == right)?;
    design_units_to_per_mille(pair.value_units, analysis.units_per_em)
}

/// Truncates `buf` to at most one `char` and returns it (`None` when empty).
///
/// Truncation is by CHARACTER, never by byte: `String::truncate` at byte 1 panics on a
/// multi-byte character, and a kerning pair addresses exactly two characters anyway.
fn clamp_to_single_char(buf: &mut String) -> Option<char> {
    let mut chars = buf.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        *buf = first.to_string();
    }
    Some(first)
}

/// Index of the stored pair that already binds `(left, right)`, EXCLUDING the pair the editor
/// was opened on (`original`), which is allowed to keep its own key. `None` = no conflict.
///
/// Stored keys are unique (the store drops a repeated key, keeping the first), so a second
/// entry for the same characters would be silently lost; this is what makes the UI refuse it.
fn conflicting_pair_index(
    pairs: &[CustomKerningPair],
    left: char,
    right: char,
    original: Option<(char, char)>,
) -> Option<usize> {
    pairs.iter().position(|pair| {
        pair.left == left && pair.right == right && Some((pair.left, pair.right)) != original
    })
}

/// Commits `pair` into `pairs`: replaces the entry keyed by `original` IN PLACE — which
/// re-keys it when the characters changed, dropping the old key and keeping the user's row
/// order — or appends when creating, or when `original` no longer exists in the list.
fn apply_pair_edit(
    pairs: &mut Vec<CustomKerningPair>,
    original: Option<(char, char)>,
    pair: CustomKerningPair,
) {
    let slot = original
        .and_then(|key| pairs.iter().position(|stored| (stored.left, stored.right) == key));
    match slot {
        Some(index) => pairs[index] = pair,
        None => pairs.push(pair),
    }
}

// --- Off-thread analysis (pure over bytes) ----------------------------------

/// Reads the font file at `path` and analyzes its representative `face_index`. Returns a
/// human-readable error string on a read or parse failure. Runs on the analysis worker.
fn analyze_font_file(path: &Path, face_index: usize) -> Result<FontAnalysis, String> {
    let bytes = fs::read(path)
        .map_err(|err| format!("cannot read font file {}: {err}", path.display()))?;
    analyze_font_bytes(&bytes, face_index)
}

/// Parses `data` as a font face and extracts its supported characters and non-default
/// kerning. Pure over the byte slice, so it is exercised indirectly by the helper unit
/// tests. Returns an error string when the face cannot be parsed.
fn analyze_font_bytes(data: &[u8], face_index: usize) -> Result<FontAnalysis, String> {
    // ttf-parser takes a u32 face index; a >u32 index cannot exist in a real font file.
    let index = u32::try_from(face_index).unwrap_or(0);
    let face = ttf_parser::Face::parse(data, index)
        .map_err(|err| format!("cannot parse font face {index}: {err}"))?;
    let units_per_em = face.units_per_em();
    let (codepoints, reverse) = collect_codepoints_and_map(&face);
    let (raw, probe_truncated) = extract_kerning(&face, &reverse);
    let (kerning, kerning_truncated) = finalize_kerning(raw, probe_truncated, MAX_KERNING_PAIRS);
    Ok(FontAnalysis {
        codepoints,
        kerning,
        kerning_truncated,
        units_per_em,
    })
}

/// Collects the supported characters from the face's Unicode cmap subtables (confirming each
/// with `glyph_index` and filtering control codepoints) and builds a reverse glyph→char map
/// for kerning display. The character list is sorted; the reverse map keeps the lowest char
/// per glyph.
fn collect_codepoints_and_map(face: &ttf_parser::Face) -> (Vec<char>, BTreeMap<u16, char>) {
    let mut chars: BTreeSet<char> = BTreeSet::new();
    if let Some(cmap) = face.tables().cmap {
        for subtable in cmap.subtables {
            // Only Unicode subtables map codepoints to characters we can display.
            if !subtable.is_unicode() {
                continue;
            }
            subtable.codepoints(|cp| {
                if let Some(ch) = displayable_char(cp) {
                    // A listed codepoint may not actually resolve; confirm it.
                    if face.glyph_index(ch).is_some() {
                        chars.insert(ch);
                    }
                }
            });
        }
    }
    let sorted: Vec<char> = chars.iter().copied().collect();
    let reverse = build_reverse_glyph_map(
        sorted
            .iter()
            .filter_map(|&ch| face.glyph_index(ch).map(|glyph| (glyph.0, ch))),
    );
    (sorted, reverse)
}

/// Maps a raw codepoint to a displayable `char`, or `None` when it is not a valid Unicode
/// scalar or is a control codepoint (C0/C1). Used to filter the cmap inventory.
fn displayable_char(cp: u32) -> Option<char> {
    let ch = char::from_u32(cp)?;
    if ch.is_control() {
        return None;
    }
    Some(ch)
}

/// Builds a glyph-id → char map from `(glyph, char)` entries, keeping the LOWEST char when
/// several characters share a glyph (deterministic display). Pure and unit-tested.
fn build_reverse_glyph_map(entries: impl Iterator<Item = (u16, char)>) -> BTreeMap<u16, char> {
    let mut map: BTreeMap<u16, char> = BTreeMap::new();
    for (glyph, ch) in entries {
        map.entry(glyph)
            .and_modify(|existing| {
                if ch < *existing {
                    *existing = ch;
                }
            })
            .or_insert(ch);
    }
    map
}

/// Collects raw non-default kerning pairs whose BOTH glyphs map to a character in `reverse`.
/// Covers the legacy `kern` table (Format 0) and GPOS `PairAdjustment` (Format 1 and 2).
///
/// GPOS lookups are restricted to those referenced by horizontal-kerning features (`kern`
/// and `dist`), so vertical kerning (`vkrn`) and other pair-adjustment features are not
/// mistaken for horizontal advance kerning. When the font exposes no such feature records the
/// walk falls back to visiting ALL pair-adjustment lookups. Cross-stream `kern` subtables
/// (perpendicular shifts, not advance kerning) are skipped.
///
/// Returns the raw pairs and whether extraction was PARTIAL — i.e. the raw cap or the global
/// probe-operation budget was hit, or GPOS pair subtables were probed with a glyph set capped
/// by `MAX_KERN_PROBE_GLYPHS`. A big legacy-`kern`-only font (enumerated in full) is NOT
/// reported as truncated just because it has many glyphs.
fn extract_kerning(
    face: &ttf_parser::Face,
    reverse: &BTreeMap<u16, char>,
) -> (Vec<RawKerningPair>, bool) {
    let mut collector = KerningCollector::new();

    // GPOS pair enumeration probes candidate first/second glyphs from the char-mapped set;
    // cap it so a huge CJK font stays bounded (O(N^2)).
    let probe: Vec<(ttf_parser::GlyphId, char)> = reverse
        .iter()
        .take(MAX_KERN_PROBE_GLYPHS)
        .map(|(&glyph, &ch)| (ttf_parser::GlyphId(glyph), ch))
        .collect();
    let probe_glyphs_capped = reverse.len() > MAX_KERN_PROBE_GLYPHS;

    // Legacy `kern` table: iterate the stored pair array directly (already bounded).
    if let Some(kern) = face.tables().kern {
        'kern: for subtable in kern.subtables {
            // Only horizontal, non-variable, non-state-machine, non-cross-stream subtables
            // carry plain horizontal advance-kerning pairs. Cross-stream values are
            // perpendicular shifts (e.g. cursive attachment), not advance kerning.
            if !subtable.horizontal
                || subtable.variable
                || subtable.has_state_machine
                || subtable.has_cross_stream
            {
                continue;
            }
            if let ttf_parser::kern::Format::Format0(format0) = subtable.format {
                for kpair in format0.pairs {
                    let (Some(&left), Some(&right)) = (
                        reverse.get(&kpair.left().0),
                        reverse.get(&kpair.right().0),
                    ) else {
                        continue;
                    };
                    if !collector.push(left, right, kpair.value) {
                        break 'kern;
                    }
                }
            }
        }
    }

    // GPOS pair adjustment: only visit horizontal-kerning lookups (see the fn doc). Track
    // whether any pair subtable was actually probed so a bare glyph-count cap on a font
    // WITHOUT GPOS pair kerning does not spuriously report truncation.
    let mut gpos_pair_probed = false;
    if let Some(gpos) = face.tables().gpos {
        match horizontal_kern_lookup_indices(&gpos) {
            // Restrict to the lookups referenced by `kern`/`dist` features.
            Some(indices) => {
                'gpos_selected: for index in indices {
                    let Some(lookup) = gpos.lookups.get(index) else {
                        continue;
                    };
                    if !probe_gpos_lookup(lookup, &probe, &mut collector, &mut gpos_pair_probed) {
                        break 'gpos_selected;
                    }
                }
            }
            // No horizontal-kerning feature records: fall back to every pair-adjustment lookup.
            None => {
                'gpos_all: for lookup in gpos.lookups {
                    if !probe_gpos_lookup(lookup, &probe, &mut collector, &mut gpos_pair_probed) {
                        break 'gpos_all;
                    }
                }
            }
        }
    }

    // Report partial extraction only for real causes: raw cap, budget exhaustion, or a GPOS
    // pair probe run against a glyph set capped by `MAX_KERN_PROBE_GLYPHS`.
    let truncated = collector.truncated
        || collector.budget_exhausted
        || (gpos_pair_probed && probe_glyphs_capped);
    (collector.raw, truncated)
}

/// Collects the GPOS lookup indices referenced by horizontal-kerning features (`kern` and
/// `dist`), deduplicated in first-seen order. Returns `None` when the font exposes NO such
/// feature records, signalling the caller to fall back to visiting every pair-adjustment
/// lookup. Iterates the GPOS `FeatureList` — the canonical set of feature records that the
/// script/language-system tables index into — which is a superset of any single script's
/// feature set and thus never misses a horizontal-kerning lookup.
fn horizontal_kern_lookup_indices(
    gpos: &ttf_parser::opentype_layout::LayoutTable,
) -> Option<Vec<u16>> {
    const KERN_TAG: ttf_parser::Tag = ttf_parser::Tag::from_bytes(b"kern");
    const DIST_TAG: ttf_parser::Tag = ttf_parser::Tag::from_bytes(b"dist");

    let mut indices: Vec<u16> = Vec::new();
    let mut seen: HashSet<u16> = HashSet::new();
    let mut any_feature = false;
    for feature in gpos.features {
        if feature.tag != KERN_TAG && feature.tag != DIST_TAG {
            continue;
        }
        any_feature = true;
        for lookup_index in feature.lookup_indices {
            if seen.insert(lookup_index) {
                indices.push(lookup_index);
            }
        }
    }
    // Distinguish "no horizontal-kerning features exist" (fall back to all lookups) from
    // "features exist but reference no lookups" (visit nothing).
    if any_feature { Some(indices) } else { None }
}

/// Probes every GPOS pair-adjustment subtable of one `lookup` over the capped `probe` set,
/// setting `*pair_seen` when a pair subtable is encountered. Returns `false` once a cap or the
/// probe budget stops the whole GPOS walk (the caller must then break out).
fn probe_gpos_lookup(
    lookup: ttf_parser::opentype_layout::Lookup,
    probe: &[(ttf_parser::GlyphId, char)],
    collector: &mut KerningCollector,
    pair_seen: &mut bool,
) -> bool {
    for subtable in lookup
        .subtables
        .into_iter::<ttf_parser::gpos::PositioningSubtable>()
    {
        if let ttf_parser::gpos::PositioningSubtable::Pair(pair) = subtable {
            *pair_seen = true;
            if !extract_pair_adjustment(&pair, probe, collector) {
                return false;
            }
        }
    }
    true
}

/// Accumulates raw kerning pairs, dropping zero-value pairs and enforcing the raw safety cap
/// and the global probe-operation budget.
struct KerningCollector {
    raw: Vec<RawKerningPair>,
    truncated: bool,
    /// Remaining GPOS probe operations before the global budget is exhausted.
    ops_budget: u64,
    /// Set once the probe-operation budget was exhausted mid-walk (partial extraction).
    budget_exhausted: bool,
}

impl KerningCollector {
    /// Fresh collector with a full probe-operation budget.
    fn new() -> Self {
        Self {
            raw: Vec::new(),
            truncated: false,
            ops_budget: MAX_KERN_PROBE_OPS,
            budget_exhausted: false,
        }
    }

    /// Charges one GPOS probe operation against the global budget. Returns `false` once the
    /// budget is exhausted (caller must stop the whole GPOS walk) and records the exhaustion.
    fn spend_op(&mut self) -> bool {
        if self.ops_budget == 0 {
            self.budget_exhausted = true;
            return false;
        }
        self.ops_budget -= 1;
        true
    }

    /// Pushes a pair. Returns `false` once the raw safety cap is hit (caller must stop).
    fn push(&mut self, left: char, right: char, value: i16) -> bool {
        if value == 0 {
            return true;
        }
        if self.raw.len() >= MAX_RAW_KERN_PAIRS {
            self.truncated = true;
            return false;
        }
        self.raw.push(RawKerningPair { left, right, value });
        true
    }
}

/// Extracts pairs from one GPOS `PairAdjustment` subtable over the capped probe set. Uses the
/// first glyph's horizontal advance adjustment (`x_advance`) as the kerning value. Each inner
/// second-glyph probe charges one operation against the collector's global budget. Returns
/// `false` when the raw cap OR the probe budget is hit (caller must stop the whole GPOS walk).
fn extract_pair_adjustment(
    pair: &ttf_parser::gpos::PairAdjustment,
    probe: &[(ttf_parser::GlyphId, char)],
    collector: &mut KerningCollector,
) -> bool {
    match pair {
        ttf_parser::gpos::PairAdjustment::Format1 { coverage, sets } => {
            for &(g1, c1) in probe {
                // Only covered first glyphs have a pair set.
                let Some(cov_idx) = coverage.get(g1) else {
                    continue;
                };
                let Some(pair_set) = sets.get(cov_idx) else {
                    continue;
                };
                for &(g2, c2) in probe {
                    // Charge the O(N^2) probe against the global budget BEFORE the lookup, so a
                    // font with many zero-yield subtables cannot scan unboundedly.
                    if !collector.spend_op() {
                        return false;
                    }
                    if let Some((v1, _v2)) = pair_set.get(g2)
                        && !collector.push(c1, c2, v1.x_advance)
                    {
                        return false;
                    }
                }
            }
        }
        ttf_parser::gpos::PairAdjustment::Format2 {
            coverage,
            classes,
            matrix,
        } => {
            let (class_def1, class_def2) = classes;
            for &(g1, c1) in probe {
                // The first glyph must be covered to participate.
                if coverage.get(g1).is_none() {
                    continue;
                }
                let class1 = class_def1.get(g1);
                for &(g2, c2) in probe {
                    // Charge the O(N^2) probe against the global budget BEFORE the lookup, so a
                    // font with many zero-yield subtables cannot scan unboundedly.
                    if !collector.spend_op() {
                        return false;
                    }
                    let class2 = class_def2.get(g2);
                    if let Some((v1, _v2)) = matrix.get((class1, class2))
                        && !collector.push(c1, c2, v1.x_advance)
                    {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// Deduplicates raw pairs by `(left, right)` (first wins), drops zero values, sorts by
/// descending magnitude (then by pair for stable display), and caps at `cap`. Returns the
/// capped list and whether it was truncated (by the input probe cap OR the display cap).
fn finalize_kerning(
    raw: Vec<RawKerningPair>,
    probe_truncated: bool,
    cap: usize,
) -> (Vec<KerningPairInfo>, bool) {
    let mut seen: HashSet<(char, char)> = HashSet::new();
    let mut deduped: Vec<KerningPairInfo> = Vec::new();
    for pair in raw {
        if pair.value == 0 {
            continue;
        }
        if seen.insert((pair.left, pair.right)) {
            deduped.push(KerningPairInfo {
                left: pair.left,
                right: pair.right,
                value_units: pair.value,
            });
        }
    }
    // Largest-magnitude pairs first; `unsigned_abs` avoids the i16::MIN abs overflow.
    deduped.sort_by(|a, b| {
        b.value_units
            .unsigned_abs()
            .cmp(&a.value_units.unsigned_abs())
            .then_with(|| (a.left, a.right).cmp(&(b.left, b.right)))
    });
    let over_cap = deduped.len() > cap;
    deduped.truncate(cap);
    (deduped, probe_truncated || over_cap)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_pair_edit, build_preview_run, build_reverse_glyph_map, clamp_to_single_char,
        conflicting_pair_index, design_units_to_per_mille, displayable_char, finalize_kerning,
        layout_preview_run, live_custom_offset_per_mille, resolve_run_kern_per_mille,
        builtin_offset_per_mille, CustomKerningPair, FontAnalysis, KerningCollector,
        KerningPairInfo, RawKerningPair, CUSTOM_KERNING_CONTEXT_CHAR_LIMIT,
    };

    /// Builds a custom pair without repeating the field names at every call site.
    fn pair(left: char, right: char, offset_per_mille: f32) -> CustomKerningPair {
        CustomKerningPair { left, right, offset_per_mille }
    }

    /// A finished analysis carrying exactly `kerning`, at 1000 upem so design units and
    /// thousandths of an em coincide and the expectations stay readable.
    fn analysis_with(kerning: Vec<KerningPairInfo>, truncated: bool) -> FontAnalysis {
        FontAnalysis {
            codepoints: Vec::new(),
            kerning,
            kerning_truncated: truncated,
            units_per_em: 1000,
        }
    }

    /// Every character advances by 10 px: the run maths is then readable as
    /// `10 * index + accumulated kerning`.
    fn flat_advance(_ch: char) -> f32 {
        10.0
    }

    #[test]
    fn displayable_char_filters_controls_and_invalid() {
        assert_eq!(displayable_char(u32::from('A')), Some('A'));
        assert_eq!(displayable_char(0x0020), Some(' '));
        // C0 control (newline) and C1 control are rejected.
        assert_eq!(displayable_char(0x000A), None);
        assert_eq!(displayable_char(0x0085), None);
        // Surrogate range is not a valid Unicode scalar value.
        assert_eq!(displayable_char(0xD800), None);
    }

    #[test]
    fn reverse_glyph_map_keeps_lowest_char_per_glyph() {
        // Glyph 5 is shared by 'B' and 'A'; the lowest char ('A') must win.
        let entries = [(5u16, 'B'), (5u16, 'A'), (9u16, 'Z')];
        let map = build_reverse_glyph_map(entries.into_iter());
        assert_eq!(map.get(&5), Some(&'A'));
        assert_eq!(map.get(&9), Some(&'Z'));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn finalize_dedups_drops_zero_and_sorts_by_magnitude() {
        let raw = vec![
            RawKerningPair { left: 'A', right: 'V', value: -40 },
            // Duplicate pair: first occurrence wins, this is dropped.
            RawKerningPair { left: 'A', right: 'V', value: -10 },
            // Zero value is never kept.
            RawKerningPair { left: 'T', right: 'o', value: 0 },
            RawKerningPair { left: 'W', right: 'a', value: 80 },
        ];
        let (pairs, truncated) = finalize_kerning(raw, false, 100);
        assert!(!truncated);
        assert_eq!(
            pairs,
            vec![
                KerningPairInfo { left: 'W', right: 'a', value_units: 80 },
                KerningPairInfo { left: 'A', right: 'V', value_units: -40 },
            ]
        );
    }

    #[test]
    fn finalize_caps_and_flags_truncation() {
        let raw = vec![
            RawKerningPair { left: 'A', right: 'A', value: 10 },
            RawKerningPair { left: 'B', right: 'B', value: 20 },
            RawKerningPair { left: 'C', right: 'C', value: 30 },
        ];
        let (pairs, truncated) = finalize_kerning(raw, false, 2);
        assert_eq!(pairs.len(), 2);
        assert!(truncated, "exceeding the cap must flag truncation");
        // Kept the two largest by magnitude.
        assert_eq!(pairs[0].value_units, 30);
        assert_eq!(pairs[1].value_units, 20);
    }

    #[test]
    fn finalize_propagates_probe_truncation() {
        let raw = vec![RawKerningPair { left: 'A', right: 'V', value: -40 }];
        let (pairs, truncated) = finalize_kerning(raw, true, 100);
        assert_eq!(pairs.len(), 1);
        assert!(truncated, "probe-cap truncation must be reported even under the display cap");
    }

    #[test]
    fn collector_budget_exhaustion_stops_and_flags() {
        // A tiny budget stands in for MAX_KERN_PROBE_OPS so the exhaustion path is testable.
        let mut collector = KerningCollector {
            raw: Vec::new(),
            truncated: false,
            ops_budget: 2,
            budget_exhausted: false,
        };
        assert!(collector.spend_op(), "first op is within budget");
        assert!(collector.spend_op(), "second op consumes the last unit");
        // The third probe exceeds the budget: it must signal "stop" and record exhaustion so
        // the caller marks the extraction truncated (a hostile all-zero-yield font is bounded).
        assert!(!collector.spend_op(), "over-budget op must stop the walk");
        assert!(collector.budget_exhausted, "exhaustion must be flagged for the truncation note");
    }

    #[test]
    fn design_units_convert_to_per_mille_independently_of_units_per_em() {
        // -40 units at 1000 upem and -80 at 2000 upem are the SAME typographic correction,
        // which is exactly why custom pairs are stored in thousandths of an em.
        assert_eq!(design_units_to_per_mille(-40, 1000), Some(-40.0));
        assert_eq!(design_units_to_per_mille(-80, 2000), Some(-40.0));
        assert_eq!(design_units_to_per_mille(1024, 2048), Some(500.0));
        // A face declaring no em square has no ratio to report (and must not divide by zero).
        assert_eq!(design_units_to_per_mille(-40, 0), None);
    }

    #[test]
    fn builtin_offset_is_looked_up_by_pair_and_converted() {
        let analysis = FontAnalysis {
            codepoints: Vec::new(),
            kerning: vec![KerningPairInfo { left: 'A', right: 'V', value_units: -80 }],
            kerning_truncated: false,
            units_per_em: 2000,
        };
        assert_eq!(builtin_offset_per_mille(&analysis, 'A', 'V'), Some(-40.0));
        // An unrelated pair has no built-in value to override.
        assert_eq!(builtin_offset_per_mille(&analysis, 'T', 'o'), None);
        // The pair is ORDERED: (V, A) is not (A, V).
        assert_eq!(builtin_offset_per_mille(&analysis, 'V', 'A'), None);
    }

    #[test]
    fn clamp_keeps_the_first_character_and_never_cuts_bytes() {
        let mut buf = String::from("AV");
        assert_eq!(clamp_to_single_char(&mut buf), Some('A'));
        assert_eq!(buf, "A");
        // A multi-byte character must survive intact: a byte-wise truncate would panic here.
        let mut buf = String::from("Ж");
        assert_eq!(clamp_to_single_char(&mut buf), Some('Ж'));
        assert_eq!(buf, "Ж");
        let mut buf = String::from("Жи");
        assert_eq!(clamp_to_single_char(&mut buf), Some('Ж'));
        assert_eq!(buf, "Ж");
        let mut buf = String::new();
        assert_eq!(clamp_to_single_char(&mut buf), None);
        assert!(buf.is_empty());
    }

    #[test]
    fn duplicate_detection_ignores_the_pair_being_edited() {
        let pairs = vec![pair('A', 'V', -40.0), pair('T', 'o', 20.0)];
        // Creating a pair that collides with a stored one is a conflict.
        assert_eq!(conflicting_pair_index(&pairs, 'T', 'o', None), Some(1));
        // Editing (T, o) and keeping its characters is NOT a conflict with itself.
        assert_eq!(conflicting_pair_index(&pairs, 'T', 'o', Some(('T', 'o'))), None);
        // Re-keying (T, o) onto an existing OTHER pair is a conflict again.
        assert_eq!(conflicting_pair_index(&pairs, 'A', 'V', Some(('T', 'o'))), Some(0));
        // A free combination never conflicts.
        assert_eq!(conflicting_pair_index(&pairs, 'W', 'a', None), None);
    }

    #[test]
    fn apply_edit_appends_replaces_and_rekeys_in_place() {
        let mut pairs = vec![pair('A', 'V', -40.0), pair('T', 'o', 20.0)];
        // Creating appends at the end.
        apply_pair_edit(&mut pairs, None, pair('W', 'a', 10.0));
        assert_eq!(pairs, vec![pair('A', 'V', -40.0), pair('T', 'o', 20.0), pair('W', 'a', 10.0)]);
        // Editing only the offset replaces the entry where it stands.
        apply_pair_edit(&mut pairs, Some(('A', 'V')), pair('A', 'V', -55.0));
        assert_eq!(pairs[0], pair('A', 'V', -55.0));
        assert_eq!(pairs.len(), 3);
        // Changing the characters RE-KEYS in place: the old key is gone, the new one holds
        // the same row, and no duplicate is left behind.
        apply_pair_edit(&mut pairs, Some(('T', 'o')), pair('T', 'a', 5.0));
        assert_eq!(pairs, vec![pair('A', 'V', -55.0), pair('T', 'a', 5.0), pair('W', 'a', 10.0)]);
        // A key that is no longer in the list (the store changed under the editor) appends
        // instead of rewriting an unrelated row.
        apply_pair_edit(&mut pairs, Some(('Z', 'z')), pair('Z', 'z', 1.0));
        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs[3], pair('Z', 'z', 1.0));
    }

    #[test]
    fn preview_run_concatenates_context_around_the_typed_pair() {
        assert_eq!(
            build_preview_run("Ab", Some('A'), Some('V'), "cd"),
            vec!['A', 'b', 'A', 'V', 'c', 'd']
        );
        // No context at all reproduces the bare pair the preview showed before contexts existed.
        assert_eq!(build_preview_run("", Some('A'), Some('V'), ""), vec!['A', 'V']);
        // A pair character that has not been typed yet is simply absent; it is never
        // substituted by a placeholder that would mis-space the rest of the run.
        assert_eq!(build_preview_run("x", None, Some('V'), "y"), vec!['x', 'V', 'y']);
        assert_eq!(build_preview_run("x", Some('A'), None, "y"), vec!['x', 'A', 'y']);
        // Nothing typed anywhere: an empty run, which paints only the background strip.
        assert!(build_preview_run("", None, None, "").is_empty());
        // Each side is capped INDEPENDENTLY, and by character, not by byte (Cyrillic is
        // two bytes per char, so a byte-wise cap would both cut wrong and risk a panic).
        let long = "Ж".repeat(CUSTOM_KERNING_CONTEXT_CHAR_LIMIT + 5);
        let run = build_preview_run(&long, Some('A'), Some('V'), &long);
        assert_eq!(run.len(), CUSTOM_KERNING_CONTEXT_CHAR_LIMIT * 2 + 2);
        assert!(run.iter().all(|&ch| ch == 'Ж' || ch == 'A' || ch == 'V'));
    }

    #[test]
    fn live_custom_offset_prefers_the_edited_pair_over_the_stored_one() {
        let stored = vec![pair('A', 'V', -40.0), pair('T', 'o', 20.0)];
        // The wheel has moved the edited pair to -90 while the store still holds -40: the
        // preview must follow the editor, or turning the wheel would change nothing on screen.
        let live = Some(('A', 'V', -90.0));
        let original = Some(('A', 'V'));
        assert_eq!(
            live_custom_offset_per_mille(&stored, original, live, 'A', 'V'),
            Some(-90.0)
        );
        // An unrelated stored pair still applies inside the context.
        assert_eq!(
            live_custom_offset_per_mille(&stored, original, live, 'T', 'o'),
            Some(20.0)
        );
        // Nothing binds a pair the user never configured.
        assert_eq!(
            live_custom_offset_per_mille(&stored, original, live, 'W', 'a'),
            None
        );
    }

    #[test]
    fn re_keying_the_edited_pair_retires_its_stored_key() {
        let stored = vec![pair('A', 'V', -40.0)];
        let original = Some(('A', 'V'));
        // The user changed the second character to 'W' while editing (A, V). The live pair is
        // now (A, W); the stored (A, V) must stop applying, otherwise the preview would show a
        // kerning the user is in the middle of moving away from.
        let live = Some(('A', 'W', -90.0));
        assert_eq!(
            live_custom_offset_per_mille(&stored, original, live, 'A', 'W'),
            Some(-90.0)
        );
        assert_eq!(
            live_custom_offset_per_mille(&stored, original, live, 'A', 'V'),
            None
        );
        // While CREATING a pair nothing is retired: every stored pair keeps applying.
        assert_eq!(
            live_custom_offset_per_mille(&stored, None, Some(('A', 'W', 5.0)), 'A', 'V'),
            Some(-40.0)
        );
    }

    #[test]
    fn run_kerning_lets_a_custom_pair_replace_the_builtin_one() {
        let analysis = analysis_with(
            vec![
                KerningPairInfo { left: 'A', right: 'V', value_units: -40 },
                KerningPairInfo { left: 'T', right: 'o', value_units: -30 },
            ],
            false,
        );
        let stored = vec![pair('T', 'o', 100.0)];
        let live = Some(('A', 'V', -90.0));
        let original = None;
        // The live pair REPLACES the font's -40 instead of stacking onto it (-130 would be the
        // additive answer, and the renderer does not do that).
        assert_eq!(
            resolve_run_kern_per_mille(&stored, original, live, Some(&analysis), 'A', 'V'),
            -90.0
        );
        // A stored custom pair replaces the built-in one the same way.
        assert_eq!(
            resolve_run_kern_per_mille(&stored, original, live, Some(&analysis), 'T', 'o'),
            100.0
        );
        // A custom 0.0 is MEANINGFUL: it cancels the built-in pair rather than falling through
        // to it.
        let cancelling = vec![pair('T', 'o', 0.0)];
        assert_eq!(
            resolve_run_kern_per_mille(&cancelling, original, None, Some(&analysis), 'T', 'o'),
            0.0
        );
    }

    #[test]
    fn run_kerning_falls_back_to_builtin_then_to_zero() {
        let analysis = analysis_with(
            vec![KerningPairInfo { left: 'A', right: 'V', value_units: -40 }],
            false,
        );
        // No custom pair: the font's own value applies, converted to thousandths of an em.
        assert_eq!(
            resolve_run_kern_per_mille(&[], None, None, Some(&analysis), 'A', 'V'),
            -40.0
        );
        // A pair neither side kerns contributes nothing.
        assert_eq!(
            resolve_run_kern_per_mille(&[], None, None, Some(&analysis), 'x', 'y'),
            0.0
        );
        // Before the off-thread analysis lands there is no built-in step at all, and only
        // custom pairs kern the run.
        assert_eq!(resolve_run_kern_per_mille(&[], None, None, None, 'A', 'V'), 0.0);
        assert_eq!(
            resolve_run_kern_per_mille(&[], None, Some(('A', 'V', 7.0)), None, 'A', 'V'),
            7.0
        );
    }

    #[test]
    fn run_layout_steps_by_advance_plus_kerning_and_ends_at_the_last_advance() {
        // Kern only the (A, V) gap so the accumulation is visible at one known place.
        let kern = |left: char, right: char| if (left, right) == ('A', 'V') { -4.0 } else { 0.0 };
        let run = ['x', 'A', 'V', 'y'];
        let layout = layout_preview_run(&run, flat_advance, kern);
        assert_eq!(layout.offsets, vec![0.0, 10.0, 16.0, 26.0]);
        // Width is the last pen plus that character's OWN advance, which is what the caller
        // centres on; the trailing gap contributes no kerning.
        assert!((layout.width - 36.0).abs() < f32::EPSILON);
    }

    #[test]
    fn run_layout_with_no_context_matches_the_two_glyph_preview() {
        // The pre-context preview placed the right glyph at `advance(left) + offset_px` and
        // sized the run as that plus the right glyph's own width. An empty context must
        // reproduce it exactly, or the feature would have changed existing behaviour.
        let offset_px = -6.0;
        let kern = |_left: char, _right: char| offset_px;
        let layout = layout_preview_run(&['A', 'V'], flat_advance, kern);
        assert_eq!(layout.offsets, vec![0.0, 10.0 + offset_px]);
        assert!((layout.width - (10.0 + offset_px + 10.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn run_layout_handles_the_degenerate_lengths() {
        let kern = |_left: char, _right: char| 5.0;
        // An empty run has nothing to place and no extent to centre.
        let empty = layout_preview_run(&[], flat_advance, kern);
        assert!(empty.offsets.is_empty());
        assert!((empty.width - 0.0).abs() < f32::EPSILON);
        // A single character sits at the origin and contributes no kerning: there is no gap.
        let single = layout_preview_run(&['A'], flat_advance, kern);
        assert_eq!(single.offsets, vec![0.0]);
        assert!((single.width - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_zero_offset_pair_survives_an_edit_round_trip() {
        // `0.0` is MEANINGFUL: it cancels a built-in pair, so nothing in the UI path may
        // treat it as "empty" and drop it.
        let mut pairs = Vec::new();
        apply_pair_edit(&mut pairs, None, pair('A', 'V', 0.0));
        assert_eq!(pairs, vec![pair('A', 'V', 0.0)]);
    }
}
