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
  section for this font's virtual-group membership, live preview, virtualized glyph grid,
  collapsible kerning list) without blocking the GUI thread;
- analyze the font file OFF the GUI thread (glyph inventory + kerning extraction via
  `ttf-parser`) and deliver the result over an `mpsc` channel the window polls;
- wire the display-name editor to `crate::tabs::typing::font_admin::set_display_name_override`
  (which bumps the shared revision, so the category lists reload automatically).

Key types:
- `FontPropertiesState` (owned by `FontSettingsEditorState`)
- `FontAnalysis` / `KerningPairInfo` (off-thread analysis result)

Key functions:
- `show` (renders the window, returns whether it stays open)
- `analyze_font_bytes` (pure ttf-parser analysis over `&[u8]`, unit-tested via helpers)
- `displayable_char` / `build_reverse_glyph_map` / `finalize_kerning` (pure, unit-tested)

Notes:
The font MODEL (identity, path, display-name key) is reached ONLY through the
`crate::tabs::typing::font_admin` facade; the display-name override API takes a `&Path`, so
the keying scheme stays private to typing. egui own-typeface registration reuses
`crate::widgets::ensure_font_family` (ADD-ONLY, cached per family), exactly like the category
rows. The heavy ttf-parser work never runs on the GUI thread; the window only polls the
channel and repaints while pending.
*/

use crate::tabs::typing::font_admin::{self, FontEntry};
use crate::widgets::{WheelComboBox, ensure_font_family};
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

// --- Window state -----------------------------------------------------------

/// State for the currently-open font-properties window (at most one at a time). Built from
/// a `FontEntry` snapshot so it never aliases the live category lists. Owned by
/// `FontSettingsEditorState`.
pub(super) struct FontPropertiesState {
    /// Representative font FILE path (the file whose glyphs/kerning are analyzed, and the
    /// display-name override KEY when passed to `font_admin`).
    path: PathBuf,
    /// Representative face index within `path` (0 for single-face files).
    rep_face: usize,
    /// Real family/name read from the font file (shown in the identity header).
    original_name: String,
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
        let name_buf = font_admin::display_name_override(font.path()).unwrap_or_default();
        let default_label = super::font_settings::clean_font_display_name(font.label());
        Self {
            path: font.path().to_path_buf(),
            rep_face,
            original_name: font.original_name().to_string(),
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
        font_admin::set_display_name_override(&self.path, value);
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
                Self::draw_glyph_grid(ui, &self.path, self.rep_face, analysis);
                ui.add_space(6.0);
                Self::draw_kerning_section(ui, &self.path, self.rep_face, analysis);
            }
        }
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
        self.member_of = font_admin::virtual_groups_for_font(&self.path);
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
        let path = self.path.clone();

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
                font_admin::set_virtual_group_member_alias(&name, &path, value);
            }
            if let Some(name) = remove_from {
                font_admin::remove_virtual_group_member(&name, &path);
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
                    font_admin::add_virtual_group_member(name, &path);
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
        if let Some(family) = ensure_font_family(ui.ctx(), &self.path, self.rep_face)
        {
            ui.style_mut().override_font_id = Some(egui::FontId::new(PREVIEW_FONT_SIZE, family));
        }
        ui.label(self.preview.as_str());
        ui.style_mut().override_font_id = prev_override;
    }

    /// Renders the virtualized glyph grid: fixed-size cells, each drawing one supported
    /// character in the font's typeface with a `U+XXXX` hover tooltip. Only visible rows run.
    fn draw_glyph_grid(ui: &mut egui::Ui, path: &Path, rep_face: usize, analysis: &FontAnalysis) {
        let count = analysis.codepoints.len();
        ui.label(tf!(
            "typing.font_settings.properties_glyphs_header",
            count = count
        ));
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
        let family = ensure_font_family(ui.ctx(), path, rep_face);
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
            let family = ensure_font_family(ui.ctx(), path, rep_face);
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

    window_open
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
        build_reverse_glyph_map, displayable_char, finalize_kerning, KerningCollector,
        KerningPairInfo, RawKerningPair,
    };

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
}
