/*
FILE HEADER (tabs/cleaning/tab.rs)
- Назначение: состояние вкладки Cleaning и координация `CanvasView` + активного cleaning-инструмента.
- Ключевые поля `CleaningTabState`:
  - `canvas`: холст с overlay-слоями клина. The Cleaning tab has NO bottom-hint (the canvas
    `bottom_hint` stays `None`, so the overlay is not drawn) and does not persist a collapsed flag.
  - `tools` / `active_tool_idx`: набор инструментов и выбранный инструмент.
  - `stroke_active` / `last_stroke_point`: состояние текущего штриха.
  - `panel_rects`: прямоугольники панелей ДОКА, нарисованных в этом кадре (добавляются в список
    после `canvas.draw`), для фильтрации ввода. Своих плавающих окон у вкладки больше нет.
  - `text_mask_model`: shared-модель маски текста для mask-layer overlay в cleaning-canvas.
  - `quick_text_mask_panel_open`: видимость вкладки дока «Быстрый клин найденного текста».
    Единственный источник истины: он же гейтит оверлей текстовой маски на холсте.
  - `text_mask_textures`: tile-кэш текстовой маски для оверлея в cleaning-canvas with LRU metadata
    for memory-pressure eviction.
  - `text_mask_load_*`: асинхронная подзагрузка масок из `text_detection`, если в shared-модели ещё нет данных.
  - `save_job_*`: фоновое сохранение clean_layers без блокировки GUI.
- `quick_clean_*`: состояние быстрого клина по маске текста (UI-параметры, фоновые job-события, прогресс).
- `overlays_model`: shared clean-overlay model; committed edits land there and use its diff-based undo/redo history.
- Ключевые методы:
  - `draw`: кадр вкладки (гейты input, рендер canvas, UI панелей, overlay UI инструмента);
    все входы кадра приходят одним `CleaningDrawParams`, среди них `panel_dock` — состояние
    панельного дока, которым владеет приложение и которое одалживается на кадр.
  - `draw_canvas_overlay_top_left` (в `CleaningHooks`): единственное место, где эта вкладка
    гоняет док; объявляет «Ленту» через `canvas::declare_ribbon_tab` и пять собственных вкладок —
    «Клин» (`CLEANING_CLEAN_TAB`), «Инструменты клина» (`CLEANING_TOOLS_TAB`), «Выбранный
    инструмент» (`CLEANING_ACTIVE_TOOL_TAB`), «Быстрый клин найденного текста»
    (`CLEANING_QUICK_CLEAN_TAB`, видима по `quick_text_mask_panel_open`) и «Редактор области»
    (`CLEANING_AREA_EDITOR_TAB`, видима по `CleaningTool::wants_main_panel` активного
    инструмента). Раскладка по умолчанию — собственная (`cleaning_default_dock_layout`).
  - `draw_clean_tab_body` / `draw_tools_tab_body` / `draw_active_tool_tab_body` /
    `draw_quick_clean_tab_body` / `draw_area_editor_tab_body`: тела этих пяти вкладок. Всё, что требует `&mut CleaningTabState`
    (правки оверлея, запуск фоновых job-ов, смена инструмента), они не делают сами — идут внутри
    `canvas.draw` — а выставляют флаги `CleaningDockOut`, которые `apply_dock_out` применяет уже
    после `canvas.draw` в том же порядке, в каком это делали снесённые плавающие поверхности.
  - `active_cursor_occluder`: вычисляет scene-область активного курсора кисти для скрытия on_top/aside пузырей.
  - `start_text_mask_load_job_if_needed/poll_text_mask_load_job`: фоновые загрузка и применение масок.
  - `start_quick_text_clean_job/poll_quick_text_clean_job`: многопоточная обработка страниц по маске текста
    с прогрессом и применением patch-ов в `CleanOverlaysModel`; pixel-level autoclean algorithm lives in
    `autoclean.rs`.
  - `handle_history_hotkeys`: Ctrl+Z / Ctrl+Shift+Z для committed overlay-дельт из shared history.
  - `handle_active_tool_input/hotkeys/wheel`: маршрутизация ввода в активный инструмент.
  - `canvas_pointer_occluded`: общий гейт ввода, когда pointer занят floating UI/popup/dialog поверх canvas.
  - `zoom_by_shortcut/reset_zoom_shortcut`: прокси zoom-hotkeys CanvasView с учётом блокировок от инструмента.
  - `viewport_snapshot/apply_viewport_snapshot`: bridge для общего viewport sync в `MangaApp`.
- Важно: если активный инструмент возвращает `block_canvas_zoom() = true` (например, открыт region editor),
  zoom CanvasView блокируется, чтобы Ctrl/Z-комбинации обрабатывались только инструментом.
  Для инструментов, которым нужен `Ctrl+ЛКМ` (например, `Замазка` для прямоугольника),
  zoom также блокируется адресно на эту комбинацию.
*/
use super::autoclean::{autoclean_page, UnevenBackgroundTool};
use super::tools::{
    AiEditorTool, AotInpaintTool, CleaningCursorOccluder, CleaningTool, Flux2KleinTool,
    FluxFillInpaintTool, GradientFillTool, LamaInpaintTool, LamaMpeInpaintTool, SdxlInpaintTool,
    StampTool, StrokeModifiers, StrokePoint, TextureSynthesisInpaintTool, WatermarkRemovalTool,
    ZamazkaTool,
};
use crate::app::{PageImageInfo, PageTexture};
use crate::canvas::{
    self, CanvasDrawParams, CanvasHooks, CanvasUiStatus, CanvasView, CanvasViewportSnapshot,
    SourceTextureUploadBudget,
};
use crate::memory_manager::{
    CacheEvictionReport, CacheEvictionRequest, CacheReloadCost, CacheResourceInfo,
    CacheResourceKind, select_eviction_candidates,
};
use crate::models::bubbles_model::BubblesModel;
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::models::text_mask_model::TextMaskModel;
use crate::project::ProjectData;
use crate::tabs::AppTab;
use crate::tabs::translation::backend_health::AiBackendHealthSnapshot;
use crate::widgets::panel_dock::{
    DockArea, DockEdge, DockLayout, HostId, PanelAnchor, PanelDock, PanelDockState, PanelId,
    PanelNode, TabId,
};
use crate::widgets::{AiButton, AiCaps, AiRequirement, WheelComboBox, WheelSlider};
use eframe::egui;
use egui::{Align, Color32, Layout, Pos2, Rect, Vec2};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use ms_thread as thread;

const STROKE_OVERLAY_UPLOAD_MIN_INTERVAL_S: f64 = 1.0 / 30.0;
const TEXT_MASK_TILE_SIDE: usize = 1024;
const TEXT_MASK_VISUAL_ALPHA_MAX: u8 = 96;
/// Runtime (not `const`) because `t!` is not const; resolves the active catalog value.
#[must_use]
fn save_hint_text() -> &'static str {
    t!("cleaning.tab.saving_status")
}

// Dock tabs of the «Клининг» program tab. Every id is a stable, non-localised
// literal: it is the identity the persisted layout and every egui id of the
// owning panel derive from (`dev-docs/i18n_exclusions.md` §A9).
/// «Клин» — clean-layer visibility, the clear/save actions, the quick-clean entry
/// point and the save status.
const CLEANING_CLEAN_TAB: TabId = TabId::new("cleaning.clean");
/// «Инструменты клина» — the tool picker alone.
const CLEANING_TOOLS_TAB: TabId = TabId::new("cleaning.tools");
/// «Выбранный инструмент» — the active tool's own UI (`CleaningTool::draw_ui`).
const CLEANING_ACTIVE_TOOL_TAB: TabId = TabId::new("cleaning.active_tool");
/// «Быстрый клин найденного текста» — the quick-clean parameters, its two run
/// buttons and its progress. Shown only while `quick_text_mask_panel_open`.
const CLEANING_QUICK_CLEAN_TAB: TabId = TabId::new("cleaning.quick_clean");
/// «Редактор области» — the main interface of a tool that edits a region ON the
/// canvas (`CleaningTool::draw_main_panel`). Shown only while the active tool asks
/// for it through `CleaningTool::wants_main_panel`.
const CLEANING_AREA_EDITOR_TAB: TabId = TabId::new("cleaning.area_editor");

/// Runtime marker painted on the badge of every Torch-backed tool button. A
/// runtime NAME rather than prose, so it stays a literal
/// (`dev-docs/i18n_exclusions.md` §A10).
const CLEANING_AI_TOOL_MARKER: &str = "Torch";

/// Smallest outer HEIGHT, in points, the dock may shrink the «Клин» panel to. Its
/// body scrolls, so the floor only has to keep the two control rows on screen. The
/// WIDTH half of the floor is measured per frame from the captions
/// ([`cleaning_clean_tab_size_bounds`]).
const CLEANING_CLEAN_TAB_MIN_HEIGHT_PX: f32 = 110.0;
/// Outer HEIGHT, in points, the «Клин» panel starts at: two control rows, the two
/// hint lines and the save status.
const CLEANING_CLEAN_TAB_INITIAL_HEIGHT_PX: f32 = 150.0;

/// Smallest outer HEIGHT, in points, the dock may shrink the «Инструменты клина»
/// panel to. The width half of that floor is measured per frame from the widest
/// tool button ([`cleaning_tab_outer_width`]); this is the height that keeps one
/// button row plus its category label visible.
const CLEANING_TOOLS_TAB_MIN_HEIGHT_PX: f32 = 96.0;
/// Outer size, in points, the «Инструменты клина» panel starts at — the width the
/// tool window used before the migration, which fits three buttons per row.
const CLEANING_TOOLS_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(352.0, 220.0);

/// Smallest outer HEIGHT, in points, the dock may shrink the «Быстрый клин
/// найденного текста» panel to: the two parameter rows and the run buttons. Its
/// width floor is measured per frame ([`cleaning_quick_clean_tab_size_bounds`]).
const CLEANING_QUICK_CLEAN_TAB_MIN_HEIGHT_PX: f32 = 120.0;
/// Outer HEIGHT, in points, the «Быстрый клин найденного текста» panel starts at:
/// the two parameter rows, the run buttons, the progress bar and two status lines,
/// which is what it shows while a run is in flight.
const CLEANING_QUICK_CLEAN_TAB_INITIAL_HEIGHT_PX: f32 = 230.0;

/// Smallest outer size, in points, the dock may shrink the «Выбранный инструмент»
/// panel to. The per-tool UIs differ wildly (the watermark tool's is by far the
/// largest) and the body scrolls, so the floor is a usable strip, not a fit.
const CLEANING_ACTIVE_TOOL_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(240.0, 140.0);
/// Outer size, in points, the «Выбранный инструмент» panel starts at: the tool
/// window's own width, and enough height for the larger tool UIs to show their
/// first controls without scrolling on the first frame.
const CLEANING_ACTIVE_TOOL_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(352.0, 360.0);

/// Smallest outer size, in points, the dock may shrink the «Редактор области» panel
/// to, and the size it starts at. FIXED numbers rather than caption-derived ones, for
/// the same reason as «Выбранный инструмент»: the body is opaque per-tool UI, so this
/// tab cannot measure the captions it is about to draw. The body scrolls, so the floor
/// only has to keep a usable strip on screen.
const CLEANING_AREA_EDITOR_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(240.0, 160.0);
/// Outer size, in points, the «Редактор области» panel starts at: the geometry lines,
/// the constraint lines, one row per mask layer, the run button and two status lines.
const CLEANING_AREA_EDITOR_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(320.0, 300.0);

/// Extra width, in points, added to every measured tool-button caption before the
/// «Инструменты клина» minimum is folded out of them.
///
/// [`cleaning_tool_button_width`] reconstructs egui's own sizing from the caption
/// galley and `Spacing::button_padding`, but the real frame margin also carries
/// the widget's expansion and its border stroke
/// (`egui-0.35.0/src/widget_style.rs:161-165`), both of which vary with the
/// interaction state. Erring HIGH costs a few points of panel width; erring low
/// clips the caption the floor exists to protect. Same rationale and value as
/// `settings::typesetting::font_groups::BUTTON_WIDTH_SLACK`.
const CLEANING_TOOL_BUTTON_WIDTH_SLACK_PX: f32 = 4.0;

/// Width, in points, a dock panel spends on its own chrome before its body — the
/// `Frame::popup` inner margin on both sides plus its border stroke on both sides.
///
/// A tab's `min_size` is an OUTER size (`panel_dock/MODULE_README.md`), so a floor
/// derived from content width has to include it. Measured at 14 pt in the default
/// style (`Margin::same(6)` per side, `egui-0.35.0/src/style.rs:1451`, plus a 1 pt
/// `window_stroke` per side, `:1519`); rounded up because both are style-dependent
/// and a floor that is a little too wide costs nothing while one that is too
/// narrow clips a caption.
const CLEANING_PANEL_CHROME_WIDTH_PX: f32 = 16.0;

const BRUSH_TOOL_INDICES: [usize; 2] = [0, 1];
const MASK_REMOVAL_TOOL_INDICES: [usize; 5] = [2, 3, 4, 5, 6];
// Инструменты редактирования области (SDXL, FLUX.1 Fill, удаление водяных знаков,
// FLUX.2 klein, ИИ-редактор области) — отдельной строкой. Индекс, отсутствующий в
// этих массивах, зарегистрирован, но не рисуется ни в одной группе панели
// инструментов.
const AREA_EDIT_TOOL_INDICES: [usize; 5] = [7, 8, 9, 10, 11];

/// Every tool index the «Инструменты клина» tab draws a button for, in draw order.
///
/// The three groups are separate constants because they are drawn under separate
/// category labels; this is the one place that needs them as a whole — the tab's
/// width floor is the maximum over ALL of them, and a tool missing here would be
/// a tool whose caption the floor does not protect.
///
/// No `#[must_use]`: `impl Iterator` already carries one, and repeating it is
/// `clippy::double_must_use`.
fn drawn_tool_indices() -> impl Iterator<Item = usize> {
    BRUSH_TOOL_INDICES
        .into_iter()
        .chain(MASK_REMOVAL_TOOL_INDICES)
        .chain(AREA_EDIT_TOOL_INDICES)
}

/// Builds the default dock arrangement of the «Клининг» program tab.
///
/// Six panels reproducing where the migrated surfaces floated: the canvas' own
/// «Лента» flush with the left edge, «Редактор области» docked UNDER it, «Клин» to
/// the ribbon's right where the island sat, «Быстрый клин найденного текста» under
/// «Клин» — the button that opens it lives there, and the left column is where the
/// vertical room is, while hanging it off the right column would put an on-demand
/// panel under a tool UI that is already the tallest thing on screen —,
/// «Инструменты клина» flush with the right edge where the tool window sat, and
/// «Выбранный инструмент» docked under it. All six are content-sized: their width
/// is driven by their tabs' own `min_size`, and pinning a size here would only make
/// the first solve fight it.
///
/// «Редактор области» is a LEAF of the arrangement: nothing is anchored to it, so a
/// frame in which it is hidden — every tool but the area editor — simply drops it
/// and leaves the other five exactly where they are, with no anchor inherited by
/// anyone (`hiding_the_area_editor_leaves_every_other_panel_untouched`). Anchoring
/// it under the ribbon rather than beside it is what makes that leaf position
/// possible: «Лента» is itself `ViewportEdge::Left`, so a panel anchored to its
/// `Left` would land outside the dock area and the solver's whole-chain translation
/// would un-flush the ribbon.
///
/// No two panels share a `target` + `edge` + `align`: the three `Bottom` anchors name
/// different targets («Лента», «Клин» and «Инструменты клина»), which is what keeps
/// the solver — a total function of whatever layout it is given — from laying one
/// panel exactly on top of another.
///
/// Every canvas program tab needs a builder of ITS own — there is no shared
/// ribbon-only one left — because the default layout doubles as the DICTIONARY the
/// persistence layer resolves stored tab keys against: a `TabId` missing from it is
/// dropped from the user's arrangement on every load
/// (`panel_dock/persist.rs::known_tabs`).
///
/// Used only when no layout exists yet for this program tab; a restored one always
/// wins. Handed to the app-owned dock state as a plain `fn` pointer, both when the
/// persisted layouts are restored before the first frame and by
/// `ensure_default_layout` on every frame this tab draws. A model refusal is
/// logged and skipped, never panicked on: the dock then creates a panel for the
/// orphaned tab on its own, which is a degraded arrangement rather than a lost tab.
#[must_use]
pub(crate) fn cleaning_default_dock_layout() -> DockLayout {
    let mut layout = DockLayout::new();
    let ribbon = PanelId::new(0);
    let clean = PanelId::new(1);
    let tools = PanelId::new(2);
    // A new id rather than a renumbering: «Лента» keeps id 0 so a user who already
    // arranged it under an earlier build finds their panel where they left it.
    let area_editor = PanelId::new(5);
    let panels = [
        // Insertion order is anchor order: `insert_panel` rejects an anchor whose
        // target does not exist yet, so the ribbon comes before both panels that
        // hang off it — «Редактор области» below and «Клин» to its right.
        (
            ribbon,
            vec![canvas::CANVAS_RIBBON_TAB],
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0,
            },
        ),
        (
            area_editor,
            vec![CLEANING_AREA_EDITOR_TAB],
            PanelAnchor::Panel {
                target: ribbon,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
        (
            clean,
            vec![CLEANING_CLEAN_TAB],
            PanelAnchor::Panel {
                target: ribbon,
                edge: DockEdge::Right,
                align: 0.0,
            },
        ),
        (
            tools,
            vec![CLEANING_TOOLS_TAB],
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Right,
                along: 0.0,
            },
        ),
        (
            PanelId::new(3),
            vec![CLEANING_ACTIVE_TOOL_TAB],
            PanelAnchor::Panel {
                target: tools,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
        (
            PanelId::new(4),
            vec![CLEANING_QUICK_CLEAN_TAB],
            PanelAnchor::Panel {
                target: clean,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
    ];
    for (id, tabs, anchor) in panels {
        let node = match PanelNode::new(id, HostId::MainWindow, tabs) {
            Ok(mut node) => {
                node.anchor = anchor;
                node
            }
            Err(error) => {
                crate::runtime_log::log_warn(format!(
                    "[cleaning] default dock layout: could not build panel {id} ({error}); \
                     the dock will create one per orphaned tab on its own"
                ));
                continue;
            }
        };
        if let Err(error) = layout.insert_panel(node) {
            crate::runtime_log::log_warn(format!(
                "[cleaning] default dock layout: could not insert panel {id} ({error}); \
                 the dock will create one per orphaned tab on its own"
            ));
        }
    }
    layout
}

/// Width, in points, the caption `label` lays out to in the button text style.
///
/// Text LAYOUT only — no painting and no I/O — so it is safe on the GUI thread, and
/// egui caches galleys, so re-measuring a fixed caption every frame costs a lookup.
#[must_use]
fn cleaning_caption_width(ctx: &egui::Context, style: &egui::Style, label: &str) -> f32 {
    let font_id = egui::TextStyle::Button.resolve(style);
    // The colour is irrelevant to the measurement; the galley is never painted.
    ctx.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(label.to_string(), font_id, Color32::WHITE)
            .size()
            .x
    })
}

/// Width, in points, `label` needs as a cleaning tool button under `style`.
///
/// The caption's laid-out width plus the style's horizontal button padding on both
/// sides plus [`CLEANING_TOOL_BUTTON_WIDTH_SLACK_PX`]. For a `badged` (Torch-gated)
/// button it also adds the marker badge's overhang, which is painted OUTSIDE the
/// button's own rect (`widgets::marker_badge_overhang`).
///
/// What that badge term buys is narrow and worth stating exactly: it keeps the
/// badge inside the panel for a button that starts a row at the tab's WIDTH FLOOR,
/// which is the case the floor is computed for. It is not a general "the badge is
/// never clipped" guarantee — at any wider width a wrapped row can still end with a
/// badged button flush against the body's right edge, and there the badge is drawn
/// over whatever is beside it exactly as it was before this migration.
///
/// The whole number is an ESTIMATE by construction: egui 0.35 derives the real
/// button padding from `Style::button_style`, where it varies with the widget's
/// interaction state, so this deliberately errs high.
#[must_use]
fn cleaning_tool_button_width(
    ctx: &egui::Context,
    style: &egui::Style,
    label: &str,
    badged: bool,
) -> f32 {
    let badge = if badged {
        crate::widgets::marker_badge_overhang(ctx, style, CLEANING_AI_TOOL_MARKER)
    } else {
        0.0
    };
    cleaning_caption_width(ctx, style, label)
        + style.spacing.button_padding.x * 2.0
        + CLEANING_TOOL_BUTTON_WIDTH_SLACK_PX
        + badge
}

/// Width, in points, `label` needs as a `ui.checkbox` under `style`.
///
/// egui 0.35 lays a checkbox out as an `AtomLayout` of an icon square plus the
/// caption, in a frame with NO padding of its own
/// (`Style::checkbox_style`, `egui-0.35.0/src/widget_style.rs:174-189`;
/// `egui-0.35.0/src/widgets/checkbox.rs:85-100`): `Spacing::icon_width` for the
/// square, `Spacing::icon_spacing` for the gap
/// (`egui-0.35.0/src/atomics/atom_layout.rs:302`), and a floor of
/// `Spacing::interact_size.y`. The same slack as a button is added on top, for the
/// same reason.
#[must_use]
fn cleaning_checkbox_width(ctx: &egui::Context, style: &egui::Style, label: &str) -> f32 {
    let spacing = &style.spacing;
    let content = spacing.icon_width
        + spacing.icon_spacing
        + cleaning_caption_width(ctx, style, label)
        + CLEANING_TOOL_BUTTON_WIDTH_SLACK_PX;
    content.max(spacing.interact_size.y)
}

/// Natural width, in points, of `widths` laid out side by side in one row.
///
/// One `item_spacing.x` per gap, none after the last widget. An empty row is `0.0`.
#[must_use]
fn cleaning_row_width(widths: &[f32], item_spacing_x: f32) -> f32 {
    let gaps = widths.len().saturating_sub(1);
    // `usize -> f32` through `u16`: a row holds a handful of controls, so the
    // saturating conversion is exact here and cannot lose precision even if it
    // ever saturated.
    let gap_total = item_spacing_x * f32::from(u16::try_from(gaps).unwrap_or(u16::MAX));
    widths.iter().copied().filter(|w| w.is_finite()).sum::<f32>() + gap_total
}

/// Folds measured CONTENT widths into an OUTER panel width: the widest one plus the
/// panel's own chrome and the body scrollbar's reserve.
///
/// Non-finite and non-positive measurements are ignored rather than propagated (a
/// `min_size` reaching the solver as `NaN` would poison the whole layout); with no
/// usable measurement at all the result is the chrome and the reserve alone, and the
/// solver's own `PANEL_MIN_WIDTH` still applies underneath.
#[must_use]
fn cleaning_tab_outer_width(
    content_widths: impl IntoIterator<Item = f32>,
    scrollbar_reserve: f32,
) -> f32 {
    content_widths
        .into_iter()
        .filter(|width| width.is_finite() && *width > 0.0)
        .fold(0.0_f32, f32::max)
        + CLEANING_PANEL_CHROME_WIDTH_PX
        + scrollbar_reserve
}

/// Width, in points, a dock panel's body must keep clear on its right for the
/// vertical scroll bar.
///
/// Verified against the LIVE style rather than taken from the dock's readme: nothing
/// in this repo assigns `Spacing::scroll`, so it is egui's `ScrollStyle::default()`,
/// which is `floating()` (`egui-0.35.0/src/style.rs:581-585`). A floating bar
/// allocates NO width from the content (`floating_allocated_width: 0.0`, `:644`;
/// `allocated_width()`, `:652-658`) — but it PAINTS over the rightmost `bar_width`
/// points of the body when it is visible, and at the width floor that strip is
/// exactly where a badged button's marker sits. So the reserve is the wider of what
/// the bar TAKES and what it COVERS, which keeps this correct if the style is ever
/// switched to a solid preset (where the two coincide).
#[must_use]
fn cleaning_scrollbar_reserve(style: &egui::Style) -> f32 {
    let scroll = &style.spacing.scroll;
    scroll
        .allocated_width()
        .max(scroll.bar_width + scroll.bar_outer_margin)
}

/// Width, in points, a `WheelSlider` with a value box needs under `style`.
///
/// It wraps `egui::Slider`, whose width is the rail (`Spacing::slider_width`) plus
/// one item gap plus the value `DragValue`, and a `DragValue`'s minimum size is
/// `Spacing::interact_size` (`egui-0.35.0/src/style.rs:406-411`). The caption is not
/// part of it here: this tab draws the label as a separate widget beside the slider.
#[must_use]
fn cleaning_slider_width(style: &egui::Style) -> f32 {
    let spacing = &style.spacing;
    spacing.slider_width
        + spacing.item_spacing.x
        + spacing.interact_size.x
        + CLEANING_TOOL_BUTTON_WIDTH_SLACK_PX
}

/// Width, in points, a `WheelComboBox` showing `selected_text` needs under `style`.
///
/// It wraps `egui::ComboBox`, for which `Spacing::combo_width` is a MINIMUM rather
/// than a fixed width (`egui-0.35.0/src/containers/combo_box.rs:345-347`): the box
/// grows to fit its selected text plus the button padding and the drop-down arrow,
/// which is an icon of `Spacing::icon_width` one `icon_spacing` away.
#[must_use]
fn cleaning_combo_width(ctx: &egui::Context, style: &egui::Style, selected_text: &str) -> f32 {
    let spacing = &style.spacing;
    let content = cleaning_caption_width(ctx, style, selected_text)
        + spacing.button_padding.x * 2.0
        + spacing.icon_spacing
        + spacing.icon_width
        + CLEANING_TOOL_BUTTON_WIDTH_SLACK_PX;
    content.max(spacing.combo_width)
}

/// Smallest and starting OUTER size of the «Клин» tab under `style`, measured from
/// the captions it is about to draw.
///
/// Both bounds are per-locale by necessity. The dock never re-measures a tab's
/// WIDTH — it stores the width the panel ASKED for (`panel_dock/mod.rs`,
/// `PanelPlan::assumed_size`) — so `initial_size.x` is the panel's width until the
/// user drags it, and a fixed number sized for Russian opens the French panel on a
/// permanent horizontal scrollbar.
///
/// - the MINIMUM is the widest SINGLE control, because the three controls of the
///   first row wrap ([`draw_clean_tab_body`]); the second row does not wrap, so its
///   button counts as a single control here too;
/// - the START is the widest natural ROW, so the first frame shows the first row
///   unwrapped, exactly as the island did.
#[must_use]
fn cleaning_clean_tab_size_bounds(ctx: &egui::Context, style: &egui::Style) -> (Vec2, Vec2) {
    let first_row = [
        cleaning_checkbox_width(ctx, style, t!("cleaning.tab.show_layer_button")),
        cleaning_tool_button_width(
            ctx,
            style,
            t!("cleaning.tab.clear_current_layer_button"),
            false,
        ),
        cleaning_tool_button_width(ctx, style, t!("cleaning.tab.save_clean_button"), false),
    ];
    let quick_clean = cleaning_tool_button_width(
        ctx,
        style,
        t!("cleaning.tab.quick_clean_heading"),
        false,
    );
    let reserve = cleaning_scrollbar_reserve(style);
    let min_width = cleaning_tab_outer_width(
        first_row.into_iter().chain(std::iter::once(quick_clean)),
        reserve,
    );
    let initial_width = cleaning_tab_outer_width(
        [
            cleaning_row_width(&first_row, style.spacing.item_spacing.x),
            quick_clean,
        ],
        reserve,
    );
    (
        Vec2::new(min_width, CLEANING_CLEAN_TAB_MIN_HEIGHT_PX),
        Vec2::new(
            initial_width.max(min_width),
            CLEANING_CLEAN_TAB_INITIAL_HEIGHT_PX,
        ),
    )
}

/// Smallest and starting OUTER size of the «Быстрый клин найденного текста» tab
/// under `style`, measured from the controls it is about to draw.
///
/// Same rule and the same reasons as [`cleaning_clean_tab_size_bounds`]: all three
/// of this body's rows wrap, so the MINIMUM is the widest single control, while the
/// START is the widest natural row so nothing wraps on the first frame. Its width is
/// caption-driven like the other tabs' — the two parameter labels and the two run
/// buttons are the widest things in it, and they are the ones that grow in Spanish
/// and French. The progress bar and the status lines below are not measured: a
/// `ProgressBar` takes the width it is given and a `ui.small` wraps.
#[must_use]
fn cleaning_quick_clean_tab_size_bounds(ctx: &egui::Context, style: &egui::Style) -> (Vec2, Vec2) {
    let spread_row = [
        cleaning_caption_width(ctx, style, t!("cleaning.tab.mask_spread_radius_label")),
        cleaning_slider_width(style),
    ];
    let background_row = [
        cleaning_caption_width(ctx, style, t!("cleaning.tab.uneven_background_tool_label")),
        // The widest option the combo can show, not the one selected right now: the
        // floor must not move when the user picks another entry.
        cleaning_combo_width(ctx, style, UnevenBackgroundTool::NoProcessing.title()),
    ];
    let run_row = [
        cleaning_tool_button_width(
            ctx,
            style,
            t!("cleaning.tab.clean_current_page_button"),
            false,
        ),
        cleaning_tool_button_width(ctx, style, t!("cleaning.tab.clean_all_pages_button"), false),
    ];
    let reserve = cleaning_scrollbar_reserve(style);
    let gap = style.spacing.item_spacing.x;
    let min_width = cleaning_tab_outer_width(
        spread_row
            .into_iter()
            .chain(background_row)
            .chain(run_row),
        reserve,
    );
    let initial_width = cleaning_tab_outer_width(
        [
            cleaning_row_width(&spread_row, gap),
            cleaning_row_width(&background_row, gap),
            cleaning_row_width(&run_row, gap),
        ],
        reserve,
    );
    (
        Vec2::new(min_width, CLEANING_QUICK_CLEAN_TAB_MIN_HEIGHT_PX),
        Vec2::new(
            initial_width.max(min_width),
            CLEANING_QUICK_CLEAN_TAB_INITIAL_HEIGHT_PX,
        ),
    )
}

#[derive(Clone)]
struct TextMaskTextureTile {
    texture: egui::TextureHandle,
    origin_px: [usize; 2],
    size_px: [usize; 2],
}

#[derive(Clone)]
struct TextMaskTexturePage {
    size: [usize; 2],
    tiles: Vec<TextMaskTextureTile>,
    last_used_frame: u64,
    // Sampling mode the tiles were uploaded with. When the active pixel
    // inspection mode flips, the page is rebuilt so the mask matches the
    // source/overlay sampling instead of staying fixed at one filter.
    texture_options: egui::TextureOptions,
}

#[derive(Debug, Clone)]
struct TextMaskLoadPage {
    page_idx: usize,
    /// Source-page pixel size `[w, h]` — the space `blocks` live in. Used to scale
    /// detector boxes into page space for autoclean; `[0, 0]` when unknown.
    source_size: [u32; 2],
    mask_size: [u32; 2],
    mask_alpha: Vec<u8>,
    /// Detector text boxes `[x1, y1, x2, y2]` in source-page pixel space, or `None`
    /// when unknown (manual mask edit, or the disk fallback which has no blocks JSON).
    blocks: Option<Vec<[i32; 4]>>,
}

#[derive(Debug)]
struct TextMaskLoadResult {
    pages: Vec<TextMaskLoadPage>,
    loaded: usize,
    missing: usize,
    failed: usize,
}

#[derive(Debug, Clone)]
struct QuickTextCleanTask {
    page_idx: usize,
    page_path: PathBuf,
    mask_path: PathBuf,
    mask_from_model: Option<TextMaskLoadPage>,
}

#[derive(Debug)]
struct QuickTextCleanPageResult {
    page_idx: usize,
    patch: Option<egui::ColorImage>,
    regions_total: usize,
    regions_filled: usize,
    regions_skipped: usize,
    regions_partial: usize,
    error: Option<String>,
    missing_mask: bool,
}

#[derive(Debug)]
enum QuickTextCleanJobEvent {
    Started { total_pages: usize },
    PageProcessed(QuickTextCleanPageResult),
    Finished,
}

#[derive(Debug, Default, Clone)]
struct QuickTextCleanProgress {
    total_pages: usize,
    done_pages: usize,
    regions_total: usize,
    regions_filled: usize,
    regions_skipped: usize,
    regions_partial: usize,
    failed_pages: usize,
    missing_masks: usize,
}

/// Everything one frame of [`CleaningTabState::draw`] needs.
///
/// A parameter struct rather than a parameter list: the app-owned `panel_dock`
/// borrow raised the call over clippy's `too_many_arguments` budget, and the
/// shape mirrors `CanvasDrawParams`, which this tab already builds from these
/// very fields.
pub struct CleaningDrawParams<'a> {
    pub ctx: &'a egui::Context,
    pub ui: &'a mut egui::Ui,
    pub project: &'a ProjectData,
    pub page_infos: &'a HashMap<usize, PageImageInfo>,
    pub texture_cache: &'a mut HashMap<usize, PageTexture>,
    pub status: CanvasUiStatus,
    /// The studio window's dock state, owned by `MangaApp` and LENT for this
    /// frame. It is not a field of this tab because one studio window has exactly
    /// one dock state, shared by every program tab that hosts panels
    /// (`src/widgets/panel_dock/MODULE_README.md`); the lent-in borrow is also
    /// what keeps it disjoint from the fields the tab bodies touch.
    pub panel_dock: &'a mut PanelDockState,
}

pub struct CleaningTabState {
    canvas: CanvasView,
    tools: Vec<Box<dyn CleaningTool>>,
    active_tool_idx: usize,
    stroke_active: bool,
    last_stroke_point: Option<StrokePoint>,
    active_stroke_page_idx: Option<usize>,
    panel_rects: Vec<egui::Rect>,
    text_mask_model: Option<Arc<Mutex<TextMaskModel>>>,
    quick_text_mask_panel_open: bool,
    text_mask_textures: HashMap<usize, TextMaskTexturePage>,
    text_mask_synced_revision: u64,
    text_mask_load_in_progress: bool,
    text_mask_load_rx: Option<Receiver<Result<TextMaskLoadResult, String>>>,
    text_mask_load_status: Option<String>,
    overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    save_job_in_progress: bool,
    save_job_rx: Option<Receiver<Result<(), String>>>,
    save_status_text: Option<String>,
    quick_clean_spread_radius_px: i32,
    quick_clean_uneven_background_tool: UnevenBackgroundTool,
    quick_clean_job_in_progress: bool,
    quick_clean_job_rx: Option<Receiver<QuickTextCleanJobEvent>>,
    quick_clean_progress: QuickTextCleanProgress,
    quick_clean_status_text: Option<String>,
    ai_backend_health: Option<Arc<Mutex<AiBackendHealthSnapshot>>>,
}

impl Default for CleaningTabState {
    fn default() -> Self {
        let mut canvas = CanvasView::default();
        canvas.editable = false;

        let tools: Vec<Box<dyn CleaningTool>> = vec![
            Box::<ZamazkaTool>::default(),
            Box::<StampTool>::default(),
            Box::<GradientFillTool>::default(),
            Box::<TextureSynthesisInpaintTool>::default(),
            Box::<LamaInpaintTool>::default(),
            Box::<LamaMpeInpaintTool>::default(),
            Box::<AotInpaintTool>::default(),
            Box::<SdxlInpaintTool>::default(),
            Box::<FluxFillInpaintTool>::default(),
            Box::<WatermarkRemovalTool>::default(),
            Box::<Flux2KleinTool>::default(),
            Box::<AiEditorTool>::default(),
        ];
        let mut state = Self {
            canvas,
            tools,
            active_tool_idx: 0,
            stroke_active: false,
            last_stroke_point: None,
            active_stroke_page_idx: None,
            // The six default dock panels, which is the most this tab puts on
            // screen at once. Nothing else is added: every floating surface of this
            // tab is a dock panel now.
            panel_rects: Vec::with_capacity(6),
            text_mask_model: None,
            quick_text_mask_panel_open: false,
            text_mask_textures: HashMap::new(),
            text_mask_synced_revision: 0,
            text_mask_load_in_progress: false,
            text_mask_load_rx: None,
            text_mask_load_status: None,
            overlays_model: None,
            save_job_in_progress: false,
            save_job_rx: None,
            save_status_text: None,
            quick_clean_spread_radius_px: 48,
            quick_clean_uneven_background_tool: UnevenBackgroundTool::NoProcessing,
            quick_clean_job_in_progress: false,
            quick_clean_job_rx: None,
            quick_clean_progress: QuickTextCleanProgress::default(),
            quick_clean_status_text: None,
            ai_backend_health: None,
        };
        state.activate_tool(0);
        state
    }
}

impl CleaningTabState {
    pub fn set_bubbles_model(&mut self, model: Arc<Mutex<BubblesModel>>) {
        self.canvas.set_bubbles_model(model);
    }

    pub fn set_overlays_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        self.canvas.set_overlays_model(Arc::clone(&model));
        self.overlays_model = Some(model);
    }

    pub fn set_text_mask_model(&mut self, model: Arc<Mutex<TextMaskModel>>) {
        self.text_mask_model = Some(model);
        self.text_mask_synced_revision = 0;
        self.text_mask_textures.clear();
        self.text_mask_load_status = None;
    }

    pub fn set_ai_backend_health(&mut self, snapshot: Arc<Mutex<AiBackendHealthSnapshot>>) {
        self.ai_backend_health = Some(snapshot);
    }

    pub fn set_canvas_scroll_area_id_salt(&mut self, id_salt: &'static str) {
        self.canvas.set_scroll_area_id_salt(id_salt);
    }

    pub fn viewport_snapshot(&self) -> CanvasViewportSnapshot {
        self.canvas.viewport_snapshot()
    }

    pub fn apply_viewport_snapshot(&mut self, snapshot: CanvasViewportSnapshot) {
        self.canvas.apply_viewport_snapshot(snapshot);
    }

    pub fn current_page_local_view_center(&self) -> Option<(usize, egui::Vec2)> {
        self.canvas.current_page_local_view_center()
    }

    pub fn focus_page(&mut self, page_idx: usize, center_px: Option<egui::Vec2>, zoom: f32) {
        self.canvas.focus_page(page_idx, center_px, zoom);
    }

    pub fn cleaning_mask_gpu_memory_snapshot(
        &self,
        pinned_pages: &BTreeSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        self.text_mask_textures
            .iter()
            .map(|(page_idx, page_tex)| CacheResourceInfo {
                id: format!("cleaning-mask-gpu:{page_idx}"),
                kind: CacheResourceKind::CleaningMaskGpu,
                page_idx: Some(*page_idx),
                estimated_bytes: text_mask_texture_page_estimated_bytes(page_tex),
                last_used_frame: page_tex.last_used_frame,
                reload_cost: CacheReloadCost::RebuildFromModel,
                dirty: false,
                visible: pinned_pages.contains(page_idx),
                reconstructable: true,
            })
            .collect()
    }

    pub fn evict_cleaning_mask_gpu_cache(
        &mut self,
        request: &CacheEvictionRequest,
    ) -> CacheEvictionReport {
        let snapshot = self.cleaning_mask_gpu_memory_snapshot(&request.pinned_pages);
        let candidates = select_eviction_candidates(&snapshot, request);
        let mut evicted = Vec::new();
        let mut freed = 0_u64;
        for resource in candidates.resources {
            let Some(page_idx) = resource.page_idx else {
                continue;
            };
            if self.text_mask_textures.remove(&page_idx).is_some() {
                freed = freed.saturating_add(resource.estimated_bytes);
                evicted.push(resource);
            }
        }
        CacheEvictionReport {
            resources: evicted,
            estimated_freed_bytes: freed,
        }
    }

    pub fn evict_clean_overlay_gpu_cache(
        &mut self,
        request: &CacheEvictionRequest,
    ) -> CacheEvictionReport {
        self.canvas.evict_clean_overlay_gpu_cache(request)
    }

    pub fn active_source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        self.canvas.active_source_page_window(neighbor_radius)
    }

    pub fn source_pixel_inspection_active(&self) -> bool {
        self.canvas.source_pixel_inspection_active()
    }

    pub fn zoom_by_shortcut(&mut self, factor: f32) -> bool {
        if self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.block_canvas_zoom())
        {
            return false;
        }
        self.canvas.zoom_by_shortcut(factor)
    }

    pub fn reset_zoom_shortcut(&mut self) -> bool {
        if self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.block_canvas_zoom())
        {
            return false;
        }
        self.canvas.reset_zoom_shortcut()
    }

    /// Draws one frame of the «Клининг» tab into `params.ui`.
    ///
    /// The panel-dock state is LENT by the application for the frame (see
    /// [`CleaningDrawParams::panel_dock`]); this tab owns none of it.
    pub fn draw(&mut self, params: CleaningDrawParams<'_>) {
        let CleaningDrawParams {
            ctx,
            ui,
            project,
            page_infos,
            texture_cache,
            status,
            panel_dock,
        } = params;
        if ctx.input(|i| i.pointer.primary_released()) {
            self.finish_stroke();
        }
        // Both of these ran between `canvas.draw` and the two floating surfaces that
        // are dock tabs now, and both feed values those tabs READ. The dock runs
        // inside `canvas.draw`, so they have to move ahead of it or the tabs would
        // show last frame's answer: the «Клин» spinner would outlive the save by a
        // frame (and only self-heal because `egui::Spinner` asks for a repaint,
        // which it does not do while the panel is clipped out of view), and
        // «Инструменты клина» would show a tool that is no longer available as
        // selected. Both are safe here: `poll_save_job` touches only the save
        // receiver and its two status fields, and `ensure_active_tool_available`
        // does what the frame's own `primary_released` branch above already may do
        // — commit the stroke in flight and swap the active tool — before anything
        // has been drawn.
        self.poll_save_job();
        self.ensure_active_tool_available();
        let canvas_rect = ui.max_rect();
        let history_hotkeys_handled = self.handle_history_hotkeys(ctx);
        let hotkeys_handled = self.handle_active_tool_hotkeys(ctx, canvas_rect);
        let tool_blocks_canvas_zoom = self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.block_canvas_zoom());
        let (primary_down, secondary_down, space_down, modifiers, z_down) = ctx.input(|i| {
            (
                i.pointer.primary_down(),
                i.pointer.secondary_down(),
                i.key_down(egui::Key::Space),
                i.modifiers,
                i.key_down(egui::Key::Z),
            )
        });
        let zoom_modifier_down = z_down || modifiers.ctrl || modifiers.command;
        let tool_blocks_ctrl_primary_zoom = primary_down
            && zoom_modifier_down
            && self
                .tools
                .get(self.active_tool_idx)
                .is_some_and(|tool| tool.block_canvas_zoom_on_ctrl_primary());
        let wheel_blocked = self.handle_active_tool_wheel(ctx, canvas_rect) || self.stroke_active;
        self.canvas.set_wheel_scroll_blocked(wheel_blocked);
        self.canvas.set_zoom_blocked(
            self.stroke_active || tool_blocks_canvas_zoom || tool_blocks_ctrl_primary_zoom,
        );
        let suppress_overlay_render = self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.suppress_base_overlay_render());
        self.canvas
            .set_overlay_render_suppressed(suppress_overlay_render);

        let space_pan_active = space_down;
        if let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) {
            active_tool.set_space_pan_active(space_pan_active);
        }
        let block_drag_scroll = self.tools.get(self.active_tool_idx).is_some_and(|tool| {
            (primary_down && tool.block_canvas_drag_scroll_on_primary())
                || (secondary_down && tool.block_canvas_drag_scroll_on_secondary())
        });
        self.canvas.set_drag_scroll_blocked(block_drag_scroll);
        self.canvas
            .set_overlay_upload_min_interval_s(if self.stroke_active {
                STROKE_OVERLAY_UPLOAD_MIN_INTERVAL_S
            } else {
                0.0
            });
        // NEAREST sampling and the pixel grid switch together from one
        // DPI-correct magnification threshold (device px per source px).
        let pixel_inspection_enabled = self.canvas.pixel_inspection_recommended(ctx);
        self.canvas
            .set_pixel_sampling_nearest(pixel_inspection_enabled);
        self.canvas.set_pixel_grid_visible(pixel_inspection_enabled);

        self.poll_text_mask_load_job();
        self.poll_quick_text_clean_job();
        let cursor_occluder = self.active_cursor_occluder(ctx, canvas_rect);
        let mut hooks = CleaningHooks {
            quick_text_mask_panel_open: self.quick_text_mask_panel_open,
            text_mask_model: self.text_mask_model.as_ref().cloned(),
            text_mask_textures: &mut self.text_mask_textures,
            text_mask_synced_revision: &mut self.text_mask_synced_revision,
            cursor_occluder,
            dock_panel_rects: Vec::new(),
            tools: &mut self.tools,
            active_tool_idx: self.active_tool_idx,
            save_job_in_progress: self.save_job_in_progress,
            save_status_text: self.save_status_text.as_deref(),
            quick_clean_spread_radius_px: &mut self.quick_clean_spread_radius_px,
            quick_clean_uneven_background_tool: &mut self.quick_clean_uneven_background_tool,
            quick_clean_job_in_progress: self.quick_clean_job_in_progress,
            quick_clean_progress: &self.quick_clean_progress,
            quick_clean_status_text: self.quick_clean_status_text.as_deref(),
            text_mask_load_in_progress: self.text_mask_load_in_progress,
            text_mask_load_status: self.text_mask_load_status.as_deref(),
            dock_out: CleaningDockOut::default(),
        };
        let mut source_upload_budget = SourceTextureUploadBudget::source_page_reupload_default();
        self.canvas.draw(CanvasDrawParams {
            ctx,
            ui,
            project,
            page_infos,
            texture_cache,
            status,
            source_upload_budget: &mut source_upload_budget,
            hooks: &mut hooks,
            panel_dock,
        });
        // Taken while `hooks` is still alive and BEFORE the `&mut self` calls below, which would
        // otherwise conflict with the field borrows `hooks` holds.
        let dock_panel_rects = std::mem::take(&mut hooks.dock_panel_rects);
        let dock_out = std::mem::take(&mut hooks.dock_out);
        self.panel_rects.clear();
        // The dock drew its panels DURING `canvas.draw`, i.e. before this frame's `clear`, so its
        // rects are re-added here: `canvas_pointer_occluded` gates the active tool on this
        // same-frame list. They are the WHOLE list now — every floating surface of this tab is a
        // dock panel, so nothing else is ever pushed into it.
        //
        // Its z-order term (`ctx.layer_id_at` == `Order::Foreground`) does cover a dock panel on
        // its own, including the very first frame one appears — `Areas::set_state` records the
        // panel's `Area` during `canvas.draw`, and `Areas::is_visible` accepts the CURRENT frame's
        // set, so the lookup that runs later in the same frame already sees it. The explicit rects
        // are kept anyway: they are this tab's uniform statement of "a floating surface of mine is
        // here", and they do not depend on the panel widget staying an interactable `Area` on that
        // order. They are MAIN-WINDOW rects by construction — `drawn_panels` never reports a panel
        // the user detached into a sub-window, whose rect would otherwise blank out this window's
        // top-left corner (`PanelDockOutput`).
        self.panel_rects.extend(dock_panel_rects);
        self.apply_dock_out(dock_out, project);
        self.handle_active_tool_input(ctx, canvas_rect, project);
        let ai_backend_available = self.ai_backend_available();
        let ai_backend_torch_available = self.ai_backend_torch_available();
        if let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) {
            active_tool.set_ai_backend_available(ai_backend_available);
            active_tool.set_ai_backend_torch_available(ai_backend_torch_available);
            // `panel_rects` was refilled from the dock a few lines above and describes THIS
            // frame, so a tool that draws on the canvas can cut the panels out of the
            // viewport before `draw_overlay_ui` places anything against it.
            active_tool.set_panel_rects(&self.panel_rects);
            active_tool.draw_overlay_ui(ctx, &mut self.canvas, project);
        }
        self.draw_active_tool_cursor(ctx, ui, canvas_rect);
        self.canvas.draw_pixel_grid_overlay(ui);
        // Request a repaint only on real activity. A merely open quick-clean panel
        // must not force 60 fps: egui already repaints on panel interaction (drag,
        // resize, hover), and its spinners/progress are gated on the in-progress
        // flags below, so an idle open panel has nothing to animate.
        if self.save_job_in_progress
            || hotkeys_handled
            || history_hotkeys_handled
            || self.text_mask_load_in_progress
            || self.quick_clean_job_in_progress
        {
            ctx.request_repaint();
        }
    }

    fn ai_backend_available(&self) -> bool {
        let Some(snapshot) = self.ai_backend_health.as_ref() else {
            return false;
        };
        match snapshot.lock() {
            Ok(guard) => guard.connected,
            Err(poisoned) => poisoned.into_inner().connected,
        }
    }

    fn ai_backend_torch_available(&self) -> bool {
        let Some(snapshot) = self.ai_backend_health.as_ref() else {
            return false;
        };
        match snapshot.lock() {
            Ok(guard) => guard.is_torch_available.unwrap_or(true),
            Err(poisoned) => poisoned.into_inner().is_torch_available.unwrap_or(true),
        }
    }

    fn tool_available(&self, idx: usize) -> bool {
        // PyTorch tools gate on the process-global Torch capability (strict), the
        // same signal their `AiButton` selection buttons use, so the active-tool
        // auto-switch and the button enabled state stay in agreement.
        self.tools.get(idx).is_some_and(|tool| {
            !tool.pytorch_required() || AiRequirement::Torch.satisfied(&AiCaps::current())
        })
    }

    fn first_available_tool_idx(&self) -> Option<usize> {
        self.tools
            .iter()
            .enumerate()
            .find_map(|(idx, _)| self.tool_available(idx).then_some(idx))
    }

    fn ensure_active_tool_available(&mut self) {
        if self.tool_available(self.active_tool_idx) {
            return;
        }
        if let Some(idx) = self.first_available_tool_idx() {
            self.activate_tool(idx);
        }
    }

    fn activate_tool(&mut self, idx: usize) {
        if idx >= self.tools.len() {
            return;
        }

        self.finish_stroke();

        if let Some(current) = self.tools.get_mut(self.active_tool_idx) {
            current.deactivate(&mut self.canvas);
        }

        self.active_tool_idx = idx;

        if let Some(active) = self.tools.get_mut(self.active_tool_idx) {
            active.activate(&mut self.canvas);
        }
    }

    fn finish_stroke(&mut self) {
        if !self.stroke_active {
            self.last_stroke_point = None;
            self.active_stroke_page_idx = None;
            return;
        }
        self.stroke_active = false;
        self.last_stroke_point = None;
        self.active_stroke_page_idx = None;
        if let Some(active) = self.tools.get_mut(self.active_tool_idx) {
            active.stroke_end(&mut self.canvas);
            active.set_temporary_erase(false);
        }
    }

    /// Applies the deferred results of this frame's dock tab bodies, in the order
    /// the two migrated floating surfaces applied theirs.
    ///
    /// The bodies run INSIDE `CanvasView::draw` — the dock is driven from
    /// [`CleaningHooks::draw_canvas_overlay_top_left`] — where `&mut self` is not
    /// reachable at all and where committing an overlay edit would land in the
    /// middle of the canvas' own frame. A body therefore only RAISES a flag, and
    /// every mutation happens here, at the point in the frame the island and the
    /// tool window used to run: same calls, same order, same observable behaviour.
    ///
    /// `project` is needed by the two background-job starters and by nothing else.
    fn apply_dock_out(&mut self, out: CleaningDockOut, project: &ProjectData) {
        if let Some(visible) = out.set_overlays_visible
            && visible != self.canvas.clean_overlays_visible()
        {
            self.canvas.set_clean_overlays_visible(visible);
        }

        if out.clear_current_layer {
            self.canvas
                .clear_overlay_index(self.canvas.current_page_idx());
        }

        if out.request_save {
            self.start_save_job(project);
        }

        if out.toggle_quick_clean_panel {
            let next_open = !self.quick_text_mask_panel_open;
            self.quick_text_mask_panel_open = next_open;
            if next_open {
                self.start_text_mask_load_job_if_needed(project);
            }
        }

        // `ensure_active_tool_available` ran at the top of the tool window's own
        // draw, i.e. AHEAD of this click; it now runs at the top of the frame
        // instead (see [`CleaningTabState::draw`]), which keeps that order.
        if let Some(idx) = out.activate_tool_idx
            && idx != self.active_tool_idx
            && self.tool_available(idx)
        {
            self.activate_tool(idx);
        }

        // Last, because the quick-clean window ran last among the migrated surfaces.
        // Each run re-checks the mask load first, exactly as it did there; the job
        // orchestration itself stays in `start_quick_text_clean_job`.
        if out.run_quick_clean_current_page {
            self.start_text_mask_load_job_if_needed(project);
            self.start_quick_text_clean_job(project, vec![self.canvas.current_page_idx()]);
        }
        if out.run_quick_clean_all_pages {
            self.start_text_mask_load_job_if_needed(project);
            let page_indices: Vec<usize> = project.pages.iter().map(|page| page.idx).collect();
            self.start_quick_text_clean_job(project, page_indices);
        }
    }

    fn start_text_mask_load_job_if_needed(&mut self, project: &ProjectData) {
        if self.text_mask_load_in_progress {
            return;
        }
        let Some(model) = self.text_mask_model.as_ref().cloned() else {
            return;
        };
        let mut missing_indices = Vec::<usize>::new();
        if let Ok(model) = model.lock() {
            for page in &project.pages {
                if model.page(page.idx).is_none() {
                    missing_indices.push(page.idx);
                }
            }
        } else {
            return;
        }
        if missing_indices.is_empty() {
            self.text_mask_load_status = Some(t!("cleaning.tab.mask_already_loaded_status").to_string());
            return;
        }

        let storage_dir = project.paths.text_detection_dir.clone();
        let (tx, rx) = mpsc::channel::<Result<TextMaskLoadResult, String>>();
        self.text_mask_load_rx = Some(rx);
        self.text_mask_load_in_progress = true;
        self.text_mask_load_status =
            Some(t!("cleaning.tab.mask_load_attempt_status").to_string());
        thread::spawn(move || {
            let _ = tx.send(load_text_masks_from_storage(&storage_dir, &missing_indices));
        });
    }

    fn poll_text_mask_load_job(&mut self) {
        let Some(rx) = self.text_mask_load_rx.as_ref() else {
            return;
        };
        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.text_mask_load_in_progress = false;
                self.text_mask_load_rx = None;
                self.text_mask_load_status =
                    Some(t!("cleaning.tab.mask_load_aborted_status").to_string());
                return;
            }
        };
        self.text_mask_load_in_progress = false;
        self.text_mask_load_rx = None;

        match event {
            Ok(result) => {
                let mut applied = 0usize;
                if let Some(model) = self.text_mask_model.as_ref()
                    && let Ok(mut model) = model.lock()
                {
                    for page in result.pages {
                        model.set_page(
                            page.page_idx,
                            page.mask_size,
                            page.mask_size,
                            page.mask_alpha,
                        );
                        applied = applied.saturating_add(1);
                    }
                }
                self.text_mask_load_status = Some(tf!("cleaning.tab.mask_load_progress_status", applied = applied, total = result
                        .loaded
                        .saturating_add(result.missing)
                        .saturating_add(result.failed), loaded = result.loaded, missing = result.missing, failed = result.failed));
            }
            Err(error) => {
                self.text_mask_load_status = Some(tf!("cleaning.tab.mask_load_error", error = error));
            }
        }
    }

    fn start_save_job(&mut self, project: &ProjectData) {
        if self.save_job_in_progress {
            return;
        }
        let Some(model) = self.overlays_model.as_ref().cloned() else {
            self.save_status_text =
                Some(t!("cleaning.tab.save_unavailable_no_model_error").to_string());
            return;
        };
        let save_dir = project.paths.clean_layers_dir.clone();
        let overlay_snapshots = match model.lock() {
            Ok(locked) => locked.save_snapshots(),
            Err(_) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text =
                    Some(t!("cleaning.tab.overlay_model_lock_error").to_string());
                return;
            }
        };
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        self.save_job_rx = Some(rx);
        self.save_job_in_progress = true;
        self.save_status_text = Some(t!("cleaning.tab.saving_clean_status").to_string());

        thread::spawn(move || {
            let result = save_clean_overlay_snapshots(&save_dir, &overlay_snapshots);
            let _ = tx.send(result);
        });
    }

    fn poll_save_job(&mut self) {
        let Some(rx) = self.save_job_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text = Some(t!("cleaning.tab.clean_saved_status").to_string());
            }
            Ok(Err(err)) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text = Some(tf!("cleaning.tab.save_clean_error", err = err));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.save_job_in_progress = false;
                self.save_job_rx = None;
                self.save_status_text = Some(t!("cleaning.tab.save_aborted_status").to_string());
            }
        }
    }

    fn start_quick_text_clean_job(&mut self, project: &ProjectData, page_indices: Vec<usize>) {
        if self.quick_clean_job_in_progress {
            return;
        }
        if page_indices.is_empty() {
            self.quick_clean_status_text = Some(t!("cleaning.tab.no_pages_error").to_string());
            return;
        }
        if self.overlays_model.is_none() {
            self.quick_clean_status_text =
                Some(t!("cleaning.tab.quick_clean_unavailable_no_model_error").to_string());
            return;
        }
        let text_mask_model = self.text_mask_model.as_ref().cloned();
        let mut tasks = Vec::new();
        for page_idx in page_indices {
            let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) else {
                continue;
            };
            let mask_from_model = text_mask_model
                .as_ref()
                .and_then(|model| model.lock().ok())
                .and_then(|model| model.page(page_idx).cloned())
                .map(|page| TextMaskLoadPage {
                    page_idx,
                    source_size: page.source_size,
                    mask_size: page.mask_size,
                    mask_alpha: page.mask_alpha,
                    blocks: page.blocks,
                });
            tasks.push(QuickTextCleanTask {
                page_idx,
                page_path: page.path.clone(),
                mask_path: text_detection_mask_file_path(
                    &project.paths.text_detection_dir,
                    page_idx,
                ),
                mask_from_model,
            });
        }
        if tasks.is_empty() {
            self.quick_clean_status_text = Some(t!("cleaning.tab.no_available_pages_error").to_string());
            return;
        }

        let spread_radius_px = self.quick_clean_spread_radius_px.clamp(0, 128) as usize;
        let uneven_tool = self.quick_clean_uneven_background_tool;
        let (tx, rx) = mpsc::channel::<QuickTextCleanJobEvent>();
        self.quick_clean_job_rx = Some(rx);
        self.quick_clean_job_in_progress = true;
        self.quick_clean_progress = QuickTextCleanProgress::default();
        self.quick_clean_status_text = Some(t!("cleaning.tab.quick_clean_started_status").to_string());

        thread::spawn(move || {
            let _ = tx.send(QuickTextCleanJobEvent::Started {
                total_pages: tasks.len(),
            });
            let worker_count = thread::available_parallelism()
                .map(|count| count.get().saturating_sub(1).max(1))
                .unwrap_or(1)
                .min(tasks.len().max(1));

            let (task_tx, task_rx) = mpsc::channel::<QuickTextCleanTask>();
            let task_rx = Arc::new(Mutex::new(task_rx));
            let (result_tx, result_rx) = mpsc::channel::<QuickTextCleanPageResult>();
            let mut workers = Vec::with_capacity(worker_count);
            for _ in 0..worker_count {
                let worker_rx = Arc::clone(&task_rx);
                let worker_tx = result_tx.clone();
                workers.push(thread::spawn(move || {
                    loop {
                        let task = {
                            let Ok(rx) = worker_rx.lock() else {
                                break;
                            };
                            match rx.recv() {
                                Ok(task) => task,
                                Err(_) => break,
                            }
                        };
                        let result =
                            run_quick_text_clean_on_page(task, spread_radius_px, uneven_tool);
                        if worker_tx.send(result).is_err() {
                            break;
                        }
                    }
                }));
            }
            drop(result_tx);

            for task in tasks {
                if task_tx.send(task).is_err() {
                    break;
                }
            }
            drop(task_tx);

            while let Ok(result) = result_rx.recv() {
                let _ = tx.send(QuickTextCleanJobEvent::PageProcessed(result));
            }
            for worker in workers {
                let _ = worker.join();
            }
            let _ = tx.send(QuickTextCleanJobEvent::Finished);
        });
    }

    fn poll_quick_text_clean_job(&mut self) {
        loop {
            let event = {
                let Some(rx) = self.quick_clean_job_rx.as_ref() else {
                    return;
                };
                rx.try_recv()
            };
            match event {
                Ok(QuickTextCleanJobEvent::Started { total_pages }) => {
                    self.quick_clean_progress = QuickTextCleanProgress {
                        total_pages,
                        ..QuickTextCleanProgress::default()
                    };
                    self.quick_clean_status_text =
                        Some(t!("cleaning.tab.quick_clean_reading_status").to_string());
                }
                Ok(QuickTextCleanJobEvent::PageProcessed(result)) => {
                    self.quick_clean_progress.done_pages =
                        self.quick_clean_progress.done_pages.saturating_add(1);
                    self.quick_clean_progress.regions_total = self
                        .quick_clean_progress
                        .regions_total
                        .saturating_add(result.regions_total);
                    self.quick_clean_progress.regions_filled = self
                        .quick_clean_progress
                        .regions_filled
                        .saturating_add(result.regions_filled);
                    self.quick_clean_progress.regions_skipped = self
                        .quick_clean_progress
                        .regions_skipped
                        .saturating_add(result.regions_skipped);
                    self.quick_clean_progress.regions_partial = self
                        .quick_clean_progress
                        .regions_partial
                        .saturating_add(result.regions_partial);
                    if result.missing_mask {
                        self.quick_clean_progress.missing_masks =
                            self.quick_clean_progress.missing_masks.saturating_add(1);
                    }
                    if result.error.is_some() {
                        self.quick_clean_progress.failed_pages =
                            self.quick_clean_progress.failed_pages.saturating_add(1);
                    }
                    if let Some(patch) = result.patch {
                        self.apply_quick_text_patch_to_overlay(result.page_idx, patch);
                    }
                    self.quick_clean_status_text = Some(tf!("cleaning.tab.quick_clean_page_done_status", page = result.page_idx, regions = result.regions_total, filled = result.regions_filled, skipped = result.regions_skipped, partial = result.regions_partial));
                }
                Ok(QuickTextCleanJobEvent::Finished) => {
                    self.quick_clean_job_in_progress = false;
                    self.quick_clean_job_rx = None;
                    self.quick_clean_status_text = Some(tf!("cleaning.tab.quick_clean_finished_status", done = self.quick_clean_progress.done_pages, total = self.quick_clean_progress.total_pages, filled = self.quick_clean_progress.regions_filled, skipped = self.quick_clean_progress.regions_skipped, partial = self.quick_clean_progress.regions_partial, errors = self.quick_clean_progress.failed_pages, missing = self.quick_clean_progress.missing_masks));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.quick_clean_job_in_progress = false;
                    self.quick_clean_job_rx = None;
                    self.quick_clean_status_text =
                        Some(t!("cleaning.tab.quick_clean_aborted_status").to_string());
                    break;
                }
            }
        }
    }

    fn apply_quick_text_patch_to_overlay(&mut self, page_idx: usize, patch: egui::ColorImage) {
        if patch.size[0] == 0 || patch.size[1] == 0 {
            return;
        }
        let Some(model) = self.overlays_model.as_ref() else {
            return;
        };
        let Ok(mut model) = model.lock() else {
            return;
        };
        let mut base = model
            .get(page_idx)
            .cloned()
            .unwrap_or_else(|| egui::ColorImage::filled(patch.size, egui::Color32::TRANSPARENT));
        if base.size != patch.size {
            base = resize_color_image_nearest(&base, patch.size[0], patch.size[1]);
        }
        let mut applied = false;
        for (dst, src) in base.pixels.iter_mut().zip(patch.pixels.iter()) {
            if src.a() == 0 {
                continue;
            }
            *dst = *src;
            applied = true;
        }
        if applied {
            model.replace(page_idx, &base);
        }
    }

    fn active_tool_captures_pointer(&self, pointer_pos: egui::Pos2) -> bool {
        self.tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.captures_canvas_pointer(pointer_pos))
    }

    fn pointer_in_any_panel(&self, pointer_pos: egui::Pos2) -> bool {
        self.panel_rects
            .iter()
            .any(|panel_rect| panel_rect.contains(pointer_pos))
    }

    fn canvas_pointer_occluded(&self, ctx: &egui::Context, pointer_pos: egui::Pos2) -> bool {
        ctx.any_popup_open()
            || self.pointer_in_any_panel(pointer_pos)
            || self.canvas.pointer_over_scrollbar(pointer_pos)
            || self.active_tool_captures_pointer(pointer_pos)
            || ctx.layer_id_at(pointer_pos).is_some_and(|layer| {
                matches!(
                    layer.order,
                    egui::Order::Middle
                        | egui::Order::Foreground
                        | egui::Order::Tooltip
                        | egui::Order::Debug
                )
            })
    }

    fn handle_active_tool_input(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
        project: &ProjectData,
    ) {
        let (
            pointer_pos,
            primary_pressed,
            primary_down,
            primary_released,
            secondary_pressed,
            modifiers,
            z_down,
        ) = ctx.input(|i| {
            (
                i.pointer.interact_pos(),
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.primary_released(),
                i.pointer.secondary_pressed(),
                i.modifiers,
                i.key_down(egui::Key::Z),
            )
        });

        if primary_released {
            self.finish_stroke();
            return;
        }

        let Some(pointer_pos) = pointer_pos else {
            return;
        };

        let zoom_modifier_down = z_down || modifiers.ctrl || modifiers.command;
        if zoom_modifier_down && primary_down {
            let tool_consumes_ctrl_primary = self
                .tools
                .get(self.active_tool_idx)
                .is_some_and(|tool| tool.block_canvas_zoom_on_ctrl_primary());
            if !tool_consumes_ctrl_primary {
                self.finish_stroke();
                return;
            }
        }

        if self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.space_pan_active())
        {
            self.finish_stroke();
            return;
        }

        if !canvas_rect.contains(pointer_pos) {
            self.finish_stroke();
            return;
        }

        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            self.finish_stroke();
            return;
        }

        let page_idx = if let Some(idx) = self.active_stroke_page_idx {
            if self.canvas.page_contains_scene_pos(idx, pointer_pos) {
                Some(idx)
            } else {
                self.canvas.page_index_at_scene_pos(pointer_pos)
            }
        } else {
            self.canvas.page_index_at_scene_pos(pointer_pos)
        };
        let Some(page_idx) = page_idx else {
            return;
        };

        let point = StrokePoint {
            page_idx,
            scene_pos: pointer_pos,
            modifiers: StrokeModifiers {
                shift: modifiers.shift,
                ctrl: modifiers.ctrl || modifiers.command,
            },
        };

        if secondary_pressed
            && let Some(active_tool) = self.tools.get_mut(self.active_tool_idx)
            && active_tool.secondary_click(&mut self.canvas, project, point)
        {
            ctx.request_repaint();
            return;
        }

        if !primary_down {
            return;
        }

        if let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) {
            active_tool.set_temporary_erase(point.modifiers.shift);

            if !self.stroke_active || primary_pressed {
                if !active_tool.wants_primary_stroke(point) {
                    return;
                }
                self.stroke_active = true;
                self.last_stroke_point = Some(point);
                self.active_stroke_page_idx = Some(page_idx);
                active_tool.stroke_begin(&mut self.canvas, point);
                ctx.request_repaint();
                return;
            }

            if let Some(prev) = self.last_stroke_point {
                if prev.scene_pos == point.scene_pos {
                    return;
                }
                if prev.page_idx == point.page_idx {
                    active_tool.stroke_update(&mut self.canvas, prev, point);
                    self.last_stroke_point = Some(point);
                    self.active_stroke_page_idx = Some(point.page_idx);
                    ctx.request_repaint();
                } else {
                    active_tool.stroke_end(&mut self.canvas);
                    active_tool.stroke_begin(&mut self.canvas, point);
                    self.last_stroke_point = Some(point);
                    self.active_stroke_page_idx = Some(point.page_idx);
                    ctx.request_repaint();
                }
            }
        }
    }

    fn handle_active_tool_hotkeys(&mut self, ctx: &egui::Context, canvas_rect: egui::Rect) -> bool {
        let (pointer_pos, modifiers, z_down) =
            ctx.input(|i| (i.pointer.hover_pos(), i.modifiers, i.key_down(egui::Key::Z)));
        let wants_keyboard_input = ctx.egui_wants_keyboard_input();
        if wants_keyboard_input {
            return false;
        }
        if modifiers.ctrl || modifiers.command || z_down {
            return false;
        }
        let Some(pointer_pos) = pointer_pos else {
            return false;
        };
        if !canvas_rect.contains(pointer_pos) {
            return false;
        }
        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            return false;
        }
        let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) else {
            return false;
        };
        active_tool.on_key_event(ctx)
    }

    fn handle_history_hotkeys(&mut self, ctx: &egui::Context) -> bool {
        if ctx.egui_wants_keyboard_input() || self.stroke_active {
            return false;
        }
        if self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.block_canvas_zoom())
        {
            return false;
        }
        let command_shift_mods = egui::Modifiers {
            shift: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        let (redo, undo) = ctx.input_mut(|input| {
            (
                input.consume_key(command_shift_mods, egui::Key::Z),
                input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z),
            )
        });
        let Some(model) = self.overlays_model.as_ref() else {
            return false;
        };
        let Ok(mut model) = model.lock() else {
            return false;
        };
        if redo && model.redo_overlay_history() {
            return true;
        }
        if undo && model.undo_overlay_history() {
            return true;
        }
        false
    }

    fn handle_active_tool_wheel(&mut self, ctx: &egui::Context, canvas_rect: egui::Rect) -> bool {
        let (pointer_pos, modifiers, r_down, scroll_delta) = ctx.input(|i| {
            (
                i.pointer.hover_pos(),
                i.modifiers,
                i.key_down(egui::Key::R),
                i.smooth_scroll_delta,
            )
        });
        let Some(pointer_pos) = pointer_pos else {
            return false;
        };
        if !canvas_rect.contains(pointer_pos) {
            return false;
        }
        if !modifiers.shift && !r_down {
            return false;
        }
        // With Shift some platforms remap wheel into horizontal scrolling,
        // so fallback to X when Y is near zero.
        let mut wheel_delta = scroll_delta.y;
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = scroll_delta.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            return false;
        }
        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            return false;
        }
        let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) else {
            return false;
        };
        let handled = active_tool.on_wheel_event_with_keys(wheel_delta, modifiers, r_down);
        if handled {
            ctx.request_repaint();
        }
        handled
    }

    fn draw_active_tool_cursor(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        canvas_rect: egui::Rect,
    ) {
        let pointer_pos = ctx.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));
        let pointer_pos = pointer_pos.or_else(|| self.last_stroke_point.map(|p| p.scene_pos));
        let Some(pointer_pos) = pointer_pos else {
            return;
        };
        if !canvas_rect.contains(pointer_pos) {
            return;
        }
        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            return;
        }
        let page_idx = self.canvas.page_index_at_scene_pos(pointer_pos);
        let Some(active_tool) = self.tools.get_mut(self.active_tool_idx) else {
            return;
        };
        if let Some(page_idx) = page_idx {
            let modifiers = ctx.input(|i| i.modifiers);
            active_tool.ensure_hover_overlay(
                &mut self.canvas,
                StrokePoint {
                    page_idx,
                    scene_pos: pointer_pos,
                    modifiers: StrokeModifiers {
                        shift: modifiers.shift,
                        ctrl: modifiers.ctrl || modifiers.command,
                    },
                },
            );
        }
        active_tool.draw_cursor(ui, &self.canvas, Some(pointer_pos));
    }

    fn active_cursor_occluder(
        &self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
    ) -> Option<CleaningCursorOccluder> {
        let pointer_pos = ctx.input(|i| i.pointer.interact_pos().or(i.pointer.hover_pos()));
        let pointer_pos = pointer_pos.or_else(|| self.last_stroke_point.map(|p| p.scene_pos));
        let pointer_pos = pointer_pos?;
        if !canvas_rect.contains(pointer_pos) {
            return None;
        }
        if self.canvas_pointer_occluded(ctx, pointer_pos) {
            return None;
        }
        self.tools
            .get(self.active_tool_idx)
            .and_then(|tool| tool.bubble_occluder(&self.canvas, Some(pointer_pos)))
    }
}

/// What one frame of the «Клининг» dock tab bodies decided, drained by
/// [`CleaningTabState::apply_dock_out`] once `CanvasView::draw` has returned.
///
/// The bodies cannot mutate the tab: they run inside the canvas draw, behind the
/// dock's per-frame context, and the calls they stand for (`start_save_job`,
/// `start_text_mask_load_job_if_needed`, `activate_tool`, the canvas' own overlay
/// edits) all need `&mut CleaningTabState` and must not land mid-canvas-frame.
/// Every field is therefore a REQUEST, applied later in the frame in the order the
/// migrated floating surfaces applied theirs.
#[derive(Debug, Default)]
struct CleaningDockOut {
    /// New clean-overlay visibility asked for by the «Клин» checkbox, if the user
    /// touched it this frame.
    set_overlays_visible: Option<bool>,
    /// «Очистить текущий слой» was pressed.
    clear_current_layer: bool,
    /// «Сохранить клин» was pressed.
    request_save: bool,
    /// «Быстрый клин найденного текста» was pressed: flip the quick-clean window
    /// and, when it opens, start the mask load.
    toggle_quick_clean_panel: bool,
    /// Tool index the «Инструменты клина» buttons ended the frame on. `None` while
    /// the tab was not drawn at all, which must not be read as "activate tool 0".
    activate_tool_idx: Option<usize>,
    /// «Заклинить текущую страницу» was pressed.
    run_quick_clean_current_page: bool,
    /// «Заклинить все страницы» was pressed.
    run_quick_clean_all_pages: bool,
}

/// Per-frame context the panel dock hands to one «Клининг» tab body at a time.
///
/// The bodies capture nothing of their own: «Лента» needs the shared `CanvasView`,
/// «Инструменты клина» and «Выбранный инструмент» both need the tool list (the
/// second one mutably), and «Клин» needs the save state. Closures capturing those
/// borrows directly could not coexist in the dock's queue; exclusive, sequential
/// access through this context lets every body reach exactly what it needs.
///
/// Every field is a borrow of a DISJOINT field of [`CleaningHooks`] or a plain
/// copy, and none of them is the `PanelDockState` the dock itself borrows for the
/// frame.
struct CleaningDockCx<'a> {
    /// Owner of the «Лента» body — the hook's own `canvas` parameter, which is
    /// borrow-independent of the tab state the hook was called on.
    canvas: &'a mut CanvasView,
    /// Project page count, shown by the «Лента» tab's page counter.
    total_pages: usize,
    /// The registered cleaning tools: captions and Torch requirement for
    /// «Инструменты клина», `draw_ui` for «Выбранный инструмент».
    tools: &'a mut [Box<dyn CleaningTool>],
    /// Index of the tool that is active as this frame's dock runs. The tools tab
    /// starts its own optimistic selection from it (see [`CleaningDockOut`]).
    active_tool_idx: usize,
    /// Whether a clean save is in flight: disables «Сохранить клин» and drives the
    /// spinner next to it.
    save_job_in_progress: bool,
    /// Last save status line, shown by «Клин» while no save is running.
    save_status_text: Option<&'a str>,
    /// Whether the quick-clean tab is open. Read by «Клин», whose button is the one
    /// affordance that toggles it, so the button can show itself as pressed.
    quick_clean_panel_open: bool,
    /// Quick-clean spread radius, in page pixels — the tab's own `WheelSlider`.
    quick_clean_spread_radius_px: &'a mut i32,
    /// Quick-clean uneven-background tool — the tab's own `WheelComboBox`.
    quick_clean_uneven_background_tool: &'a mut UnevenBackgroundTool,
    /// Whether a quick-clean run is in flight: disables both run buttons and drives
    /// the running spinner.
    quick_clean_job_in_progress: bool,
    /// Progress of the quick-clean run in flight, shown as a bar plus a line of
    /// counters once it has a page total.
    quick_clean_progress: &'a QuickTextCleanProgress,
    /// Last quick-clean status line.
    quick_clean_status_text: Option<&'a str>,
    /// Whether a text-mask load is in flight, shown above the run buttons.
    text_mask_load_in_progress: bool,
    /// Last text-mask load status line.
    text_mask_load_status: Option<&'a str>,
    /// Everything the bodies decided this frame; drained after the canvas draw.
    out: &'a mut CleaningDockOut,
}

/// Draws the «Клин» tab body: clean-layer visibility, the clear/save actions, the
/// quick-clean entry point, the in-flight save spinner and the two hint lines.
///
/// Mutates nothing but [`CleaningDockOut`] — see there for why.
///
/// The first row WRAPS, on the same rule as the tool rows: three controls side by
/// side are ~460 pt in Russian and ~490 in French, and a panel the user narrows
/// below that must break the row rather than hide a button behind a horizontal
/// scrollbar. The second row is one button plus a status strip that fills whatever
/// is left, so it has nothing to wrap.
fn draw_clean_tab_body(ui: &mut egui::Ui, cx: &mut CleaningDockCx<'_>) {
    let mut overlays_visible = cx.canvas.clean_overlays_visible();
    ui.vertical(|ui| {
        ui.horizontal_wrapped(|ui| {
            // Inside a wrapping layout egui defaults widget text to
            // `TextWrapMode::Wrap` (`egui-0.35.0/src/ui.rs:588-600`), which breaks a
            // caption over two lines instead of moving its control to the next row.
            // Same fix, and same reason, as `draw_tool_button_rows`.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            if ui
                .checkbox(&mut overlays_visible, t!("cleaning.tab.show_layer_button"))
                .changed()
            {
                cx.out.set_overlays_visible = Some(overlays_visible);
            }
            if ui
                .button(t!("cleaning.tab.clear_current_layer_button"))
                .clicked()
            {
                cx.out.clear_current_layer = true;
            }
            if ui
                .add_enabled(
                    !cx.save_job_in_progress,
                    egui::Button::new(t!("cleaning.tab.save_clean_button")),
                )
                .clicked()
            {
                cx.out.request_save = true;
            }
        });
        ui.horizontal(|ui| {
            // Shown PRESSED while the quick-clean tab is open. That look is this
            // button's whole close affordance: the panel used to be an
            // `egui::Window` with a title-bar ✕, and a dock tab deliberately has
            // none (a tab is only ever MOVED), so the one control that opens it has
            // to read as the one that closes it again.
            if ui
                .add(
                    egui::Button::new(t!("cleaning.tab.quick_clean_heading"))
                        .selected(cx.quick_clean_panel_open),
                )
                .clicked()
            {
                cx.out.toggle_quick_clean_panel = true;
            }
            let status_height = ui.spacing().interact_size.y;
            let status_width = ui.available_width().max(0.0);
            ui.allocate_ui_with_layout(
                egui::vec2(status_width, status_height),
                Layout::left_to_right(Align::Center),
                |ui| {
                    if cx.save_job_in_progress {
                        ui.spinner();
                        ui.label(save_hint_text());
                    }
                },
            );
        });

        ui.small(t!("cleaning.tab.paint_erase_hint"));
        ui.small(t!("cleaning.tab.scroll_brush_hint"));
        if !cx.save_job_in_progress
            && let Some(status) = cx.save_status_text
        {
            ui.small(status);
        }
    });
}

/// Draws the «Инструменты клина» tab body: the three tool groups and nothing else.
///
/// Selection is OPTIMISTIC within the frame — a click marks its button selected
/// straight away, and the actual `activate_tool` runs after the canvas draw — so
/// the running selection is carried in [`CleaningDockOut::activate_tool_idx`]
/// rather than read back from the tab state.
fn draw_tools_tab_body(ui: &mut egui::Ui, cx: &mut CleaningDockCx<'_>) {
    let mut activate_tool_idx = cx.out.activate_tool_idx.unwrap_or(cx.active_tool_idx);
    draw_tool_button_group(
        ui,
        t!("cleaning.tab.brushes_label"),
        cx.tools,
        &BRUSH_TOOL_INDICES,
        &mut activate_tool_idx,
    );
    ui.add_space(6.0);
    draw_tool_button_group(
        ui,
        t!("cleaning.tab.mask_removal_label"),
        cx.tools,
        &MASK_REMOVAL_TOOL_INDICES,
        &mut activate_tool_idx,
    );
    // The area-edit tools (SDXL, FLUX.1 Fill, watermark removal) deliberately carry
    // no category label of their own — they read as a continuation of the list.
    draw_tool_button_rows(ui, cx.tools, &AREA_EDIT_TOOL_INDICES, &mut activate_tool_idx);
    cx.out.activate_tool_idx = Some(activate_tool_idx);
}

/// Draws the «Быстрый клин найденного текста» tab body: the two parameters, the two
/// run buttons, the mask-load and run status, and the progress of a run in flight.
///
/// Mutates the two parameter values it owns directly — they are plain settings the
/// widgets edit in place — and defers the two RUNS, which start worker jobs through
/// `&mut CleaningTabState` and would otherwise fire in the middle of the canvas'
/// own frame ([`CleaningDockOut`]).
///
/// All three rows wrap on the same rule as the rest of this tab's bodies.
fn draw_quick_clean_tab_body(ui: &mut egui::Ui, cx: &mut CleaningDockCx<'_>) {
    ui.horizontal_wrapped(|ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        ui.label(t!("cleaning.tab.mask_spread_radius_label"))
            .on_hover_text(t!("cleaning.tab.mask_spread_radius_hint"));
        ui.add(
            WheelSlider::new(cx.quick_clean_spread_radius_px, 0..=128)
                .suffix(t!("cleaning.tab.pixels_suffix")),
        );
    });
    ui.horizontal_wrapped(|ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        ui.label(t!("cleaning.tab.uneven_background_tool_label"));
        WheelComboBox::from_id_salt("quick-clean-uneven-bg-tool")
            .selected_text(cx.quick_clean_uneven_background_tool.title())
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    cx.quick_clean_uneven_background_tool,
                    UnevenBackgroundTool::NoProcessing,
                    UnevenBackgroundTool::NoProcessing.title(),
                );
            });
    });
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        if ui
            .add_enabled(
                !cx.quick_clean_job_in_progress,
                egui::Button::new(t!("cleaning.tab.clean_current_page_button")),
            )
            .clicked()
        {
            cx.out.run_quick_clean_current_page = true;
        }
        if ui
            .add_enabled(
                !cx.quick_clean_job_in_progress,
                egui::Button::new(t!("cleaning.tab.clean_all_pages_button")),
            )
            .clicked()
        {
            cx.out.run_quick_clean_all_pages = true;
        }
    });
    if cx.text_mask_load_in_progress {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.small(t!("cleaning.tab.mask_loading_status"));
        });
    } else if let Some(status) = cx.text_mask_load_status {
        ui.small(status);
    }
    if cx.quick_clean_job_in_progress {
        ui.separator();
        ui.horizontal(|ui| {
            ui.spinner();
            ui.small(t!("cleaning.tab.quick_clean_running_status"));
        });
    }
    let progress = cx.quick_clean_progress;
    if progress.total_pages > 0 {
        // Page counts through `u32` (total, so an absurd count saturates rather than
        // wrapping) and then `f64`, which represents every `u32` exactly. `total` is
        // non-zero inside this branch, and the quotient is clamped into `0..=1`, so
        // the one narrowing step below cannot lose anything that matters.
        let done = f64::from(u32::try_from(progress.done_pages).unwrap_or(u32::MAX));
        let total = f64::from(u32::try_from(progress.total_pages).unwrap_or(u32::MAX));
        let ratio = (done / total.max(1.0)).clamp(0.0, 1.0) as f32;
        ui.add(egui::ProgressBar::new(ratio).text(tf!(
            "cleaning.tab.pages_progress_status",
            done = progress.done_pages,
            total = progress.total_pages
        )));
        ui.small(tf!(
            "cleaning.tab.regions_progress_status",
            filled = progress.regions_filled,
            skipped = progress.regions_skipped,
            partial = progress.regions_partial,
            page_errors = progress.failed_pages,
            missing = progress.missing_masks
        ));
    }
    if let Some(status) = cx.quick_clean_status_text {
        ui.small(status);
    }
}

/// Draws the «Выбранный инструмент» tab body: the active tool's own UI, with no
/// heading of its own — the tab caption is the heading.
fn draw_active_tool_tab_body(ui: &mut egui::Ui, cx: &mut CleaningDockCx<'_>) {
    if let Some(tool) = cx.tools.get_mut(cx.active_tool_idx) {
        tool.draw_ui(ui);
    }
}

/// Draws the «Редактор области» tab body: the MAIN interface of a tool that edits a
/// region on the canvas.
///
/// Dispatched exactly like `draw_active_tool_tab_body`, and reached only while the
/// active tool asked for this panel — the tab is hidden otherwise, so a tool that
/// draws nothing here can never be the reason the panel appears.
fn draw_area_editor_tab_body(ui: &mut egui::Ui, cx: &mut CleaningDockCx<'_>) {
    if let Some(tool) = cx.tools.get_mut(cx.active_tool_idx) {
        tool.draw_main_panel(ui);
    }
}

/// Draws one labelled group of tool buttons.
fn draw_tool_button_group(
    ui: &mut egui::Ui,
    title: &str,
    tools: &[Box<dyn CleaningTool>],
    tool_indices: &[usize],
    activate_tool_idx: &mut usize,
) {
    ui.label(egui::RichText::new(title).strong());
    draw_tool_button_rows(ui, tools, tool_indices, activate_tool_idx);
}

/// Lays the buttons of `tool_indices` out in rows that wrap to the available width.
///
/// A button never splits across rows and a row always holds at least one button,
/// however narrow the panel: egui breaks a wrapping row only when the cursor has
/// already left the row start (`egui-0.35.0/src/layout.rs:516-518`), so a button
/// wider than the whole row still gets a row of its own instead of looping.
fn draw_tool_button_rows(
    ui: &mut egui::Ui,
    tools: &[Box<dyn CleaningTool>],
    tool_indices: &[usize],
    activate_tool_idx: &mut usize,
) {
    ui.horizontal_wrapped(|ui| {
        // Inside a wrapping horizontal layout egui defaults every widget's text to
        // `TextWrapMode::Wrap` (`Ui::wrap_mode`, `egui-0.35.0/src/ui.rs:588-600`),
        // which would break a long caption over two LINES instead of moving its
        // button to the next ROW. `Extend` restores "the caption keeps its natural
        // width, the row wraps", which is what the panel's own width floor is
        // measured against (`cleaning_tab_outer_width`). Scoped to this child
        // `Ui`, so the category labels above keep egui's own wrapping.
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        for &idx in tool_indices {
            let Some(tool) = tools.get(idx) else {
                continue;
            };
            // Resolved at draw time, not cached: a caption cached once at
            // construction would keep the language the app started in.
            let label = tool.title();
            let is_selected = *activate_tool_idx == idx;
            // AI tools use a self-gating `AiButton` (Torch requirement, framed to
            // match the plain tool buttons) with a "Torch" runtime marker; it
            // disables itself and shows the reason when Torch is unavailable.
            // Non-AI tools stay plain always-enabled buttons.
            let clicked = if tool.pytorch_required() {
                AiButton::new(label, AiRequirement::Torch)
                    .selected(is_selected)
                    .marker(CLEANING_AI_TOOL_MARKER)
                    .draw(ui)
                    .response
                    .clicked()
            } else {
                ui.add(egui::Button::new(label).selected(is_selected))
                    .clicked()
            };
            if clicked {
                *activate_tool_idx = idx;
            }
        }
    });
}

struct CleaningHooks<'a> {
    quick_text_mask_panel_open: bool,
    text_mask_model: Option<Arc<Mutex<TextMaskModel>>>,
    text_mask_textures: &'a mut HashMap<usize, TextMaskTexturePage>,
    text_mask_synced_revision: &'a mut u64,
    cursor_occluder: Option<CleaningCursorOccluder>,
    /// The registered cleaning tools, lent to the two tool tab bodies.
    tools: &'a mut [Box<dyn CleaningTool>],
    /// Active tool index as of the start of this frame.
    active_tool_idx: usize,
    /// Whether a clean save is in flight, read by the «Клин» body.
    save_job_in_progress: bool,
    /// Last save status line, read by the «Клин» body.
    save_status_text: Option<&'a str>,
    /// Quick-clean parameters, lent to the «Быстрый клин найденного текста» body.
    quick_clean_spread_radius_px: &'a mut i32,
    quick_clean_uneven_background_tool: &'a mut UnevenBackgroundTool,
    /// Quick-clean and mask-load progress, read by that body.
    quick_clean_job_in_progress: bool,
    quick_clean_progress: &'a QuickTextCleanProgress,
    quick_clean_status_text: Option<&'a str>,
    text_mask_load_in_progress: bool,
    text_mask_load_status: Option<&'a str>,
    /// What the dock tab bodies decided this frame. Collected here for the same
    /// reason `dock_panel_rects` is: the hook runs inside `canvas.draw`, and every
    /// mutation it stands for needs `&mut CleaningTabState` after that call
    /// returns ([`CleaningTabState::apply_dock_out`]).
    dock_out: CleaningDockOut,
    /// Outer rects of the dock panels drawn this frame in THIS window, collected
    /// by `draw_canvas_overlay_top_left` and drained by `CleaningTabState::draw`
    /// into `panel_rects`. It cannot be written straight into that field: the
    /// hook runs inside `canvas.draw`, and the tab clears `panel_rects` after
    /// that call returns.
    ///
    /// Never holds a sub-window's rect: `PanelDockOutput::drawn_panels` reports
    /// main-window panels only, and a detached panel's rect — taken in that
    /// window's own frame — would occlude a corner of the canvas here.
    dock_panel_rects: Vec<Rect>,
}

impl CleaningHooks<'_> {
    fn draw_text_mask_overlay_on_page_if_enabled(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        page_rect: Rect,
        pixel_inspection_nearest: bool,
    ) {
        if !self.quick_text_mask_panel_open {
            return;
        }
        let Some(model) = self.text_mask_model.as_ref() else {
            return;
        };
        let clip_rect = ui.clip_rect().intersect(page_rect);
        if !clip_rect.is_positive() {
            return;
        }
        let painter = ui.painter().with_clip_rect(clip_rect);
        let guard = match model.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };

        let revision = guard.revision();
        if revision != *self.text_mask_synced_revision {
            *self.text_mask_synced_revision = revision;
            self.text_mask_textures.clear();
        }

        let Some(mask_page) = guard.page(page_idx) else {
            return;
        };
        if mask_page.mask_alpha.is_empty() {
            return;
        }
        let texture_options = if pixel_inspection_nearest {
            egui::TextureOptions::NEAREST
        } else {
            egui::TextureOptions::LINEAR
        };
        draw_text_mask_overlay_on_page(TextMaskOverlayDrawParams {
            textures: self.text_mask_textures,
            ctx,
            painter: &painter,
            page_idx,
            page_rect,
            mask_size: mask_page.mask_size,
            mask_alpha: &mask_page.mask_alpha,
            current_frame: ctx.cumulative_frame_nr(),
            texture_options,
        });
    }
}

impl CanvasHooks for CleaningHooks<'_> {
    /// Runs the «Клининг» tab's panel dock: the canvas' own «Лента» plus this tab's
    /// five — «Клин», «Инструменты клина», «Выбранный инструмент», «Быстрый клин
    /// найденного текста» and «Редактор области».
    ///
    /// Implemented here, rather than after `canvas.draw` returns, for two reasons:
    /// the «Лента» body edits canvas settings and must land BEFORE
    /// `publish_canvas_settings` (which runs at the end of `CanvasView::draw`), and
    /// all three canvas tabs then run their dock at the same point of the frame with
    /// the same dock-area rule. It must also run on EVERY frame this tab is active:
    /// the dock's detached sub-windows are immediate viewports kept alive by
    /// `PanelDock::end` (`app.rs::tab_hosts_panel_dock`).
    fn draw_canvas_overlay_top_left(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        canvas: &mut CanvasView,
        _project: &ProjectData,
        status: CanvasUiStatus,
        panel_dock: &mut PanelDockState,
    ) {
        panel_dock.ensure_default_layout(AppTab::Cleaning.key(), cleaning_default_dock_layout);
        // Measured BEFORE the context takes the tool list: the tab's width floor is
        // a property of the captions it is about to draw, and `min_size` is re-read
        // fresh every frame, so a language switch moves it with no invalidation path
        // of its own.
        let style = ctx.style_of(ctx.theme());
        let scrollbar_reserve = cleaning_scrollbar_reserve(&style);
        let tools_min_size = egui::vec2(
            cleaning_tab_outer_width(
                drawn_tool_indices().filter_map(|idx| {
                    self.tools.get(idx).map(|tool| {
                        cleaning_tool_button_width(
                            ctx,
                            &style,
                            tool.title(),
                            tool.pytorch_required(),
                        )
                    })
                }),
                scrollbar_reserve,
            ),
            CLEANING_TOOLS_TAB_MIN_HEIGHT_PX,
        );
        let (clean_min_size, clean_initial_size) = cleaning_clean_tab_size_bounds(ctx, &style);
        let (quick_clean_min_size, quick_clean_initial_size) =
            cleaning_quick_clean_tab_size_bounds(ctx, &style);
        let quick_clean_panel_open = self.quick_text_mask_panel_open;
        // Asked BEFORE the context borrows the tool list, like every other per-frame
        // measurement above: `wants_main_panel` is a property of the active tool and
        // is re-read every frame, so a tool switch moves the panel with no
        // invalidation path of its own.
        let area_editor_panel_wanted = self
            .tools
            .get(self.active_tool_idx)
            .is_some_and(|tool| tool.wants_main_panel());
        let mut cx = CleaningDockCx {
            canvas,
            total_pages: status.total_pages,
            tools: &mut *self.tools,
            active_tool_idx: self.active_tool_idx,
            save_job_in_progress: self.save_job_in_progress,
            save_status_text: self.save_status_text,
            quick_clean_panel_open,
            quick_clean_spread_radius_px: &mut *self.quick_clean_spread_radius_px,
            quick_clean_uneven_background_tool: &mut *self.quick_clean_uneven_background_tool,
            quick_clean_job_in_progress: self.quick_clean_job_in_progress,
            quick_clean_progress: self.quick_clean_progress,
            quick_clean_status_text: self.quick_clean_status_text,
            text_mask_load_in_progress: self.text_mask_load_in_progress,
            text_mask_load_status: self.text_mask_load_status,
            out: &mut self.dock_out,
        };
        let mut dock = PanelDock::begin(
            ctx,
            panel_dock,
            DockArea {
                rect: canvas::dock_area_rect(canvas_rect),
                layout_key: AppTab::Cleaning.key(),
            },
        );
        // The canvas owns this declaration; «Клининг» only says where its context
        // keeps the canvas and the page count.
        canvas::declare_ribbon_tab(&mut dock, |cx: &mut CleaningDockCx<'_>| {
            (&mut *cx.canvas, cx.total_pages)
        });
        dock.tab(CLEANING_CLEAN_TAB)
            .title(|| t!("cleaning.tab.clean_tab"))
            .min_size(clean_min_size)
            .initial_size(clean_initial_size)
            .show(draw_clean_tab_body);
        dock.tab(CLEANING_TOOLS_TAB)
            .title(|| t!("cleaning.tab.tools_heading"))
            .min_size(tools_min_size)
            .initial_size(CLEANING_TOOLS_TAB_INITIAL_SIZE_PX)
            .show(draw_tools_tab_body);
        dock.tab(CLEANING_ACTIVE_TOOL_TAB)
            .title(|| t!("cleaning.tab.active_tool_tab"))
            .min_size(CLEANING_ACTIVE_TOOL_TAB_MIN_SIZE_PX)
            .initial_size(CLEANING_ACTIVE_TOOL_TAB_INITIAL_SIZE_PX)
            .show(draw_active_tool_tab_body);
        // Declared on EVERY frame, visible only while the toggle is on: a hidden tab
        // keeps its slot in the layout and only its panel is skipped, so closing and
        // reopening it returns it to wherever the user put it. Skipping the
        // declaration instead would make the dock treat it as another program tab's
        // and, on the next open, seed it a fresh panel.
        dock.tab(CLEANING_QUICK_CLEAN_TAB)
            .title(|| t!("cleaning.tab.quick_clean_heading"))
            .visible(quick_clean_panel_open)
            .min_size(quick_clean_min_size)
            .initial_size(quick_clean_initial_size)
            .show(draw_quick_clean_tab_body);
        // Declared on EVERY frame for the same reason as the quick-clean tab, and
        // visible only while the active tool asks for it. Hiding it is what makes the
        // default arrangement identical to a five-panel one: the solver drops the
        // panel and hands its own `ViewportEdge::Left` anchor down to «Лента».
        dock.tab(CLEANING_AREA_EDITOR_TAB)
            .title(|| t!("cleaning.tab.area_editor_tab"))
            .visible(area_editor_panel_wanted)
            .min_size(CLEANING_AREA_EDITOR_TAB_MIN_SIZE_PX)
            .initial_size(CLEANING_AREA_EDITOR_TAB_INITIAL_SIZE_PX)
            .show(draw_area_editor_tab_body);
        // MAIN-WINDOW panels only, by construction: `drawn_panels` never reports a
        // panel the user detached into a sub-window, whose rect lives in that
        // window's own frame and would carve a dead zone out of this window's
        // top-left corner (`PanelDockOutput`).
        let out = dock.end(&mut cx);
        self.dock_panel_rects
            .extend(out.drawn_panels().map(|(_, rect)| rect));
    }

    fn draw_canvas_mask_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        // Mask sampling follows pixel inspection so a magnified source pixel
        // looks identical across source, clean overlay, and text mask.
        let pixel_inspection_nearest =
            crate::canvas::pixel_inspection_recommended_for(zoom, ctx.pixels_per_point());
        self.draw_text_mask_overlay_on_page_if_enabled(
            ui,
            ctx,
            page_idx,
            image_rect,
            pixel_inspection_nearest,
        );
    }

    fn should_hide_on_top_bubble(
        &mut self,
        page_idx: usize,
        _bubble: &crate::project::Bubble,
        bubble_rect: Rect,
    ) -> bool {
        self.cursor_occluder.is_some_and(|occluder| {
            occluder.page_idx == page_idx
                && circle_intersects_rect(
                    occluder.center_scene_pos,
                    occluder.radius_scene,
                    bubble_rect,
                )
        })
    }

    fn should_hide_aside_bubble_line(
        &mut self,
        page_idx: usize,
        _bubble: &crate::project::Bubble,
        line_start: Pos2,
        line_end: Pos2,
    ) -> bool {
        self.cursor_occluder.is_some_and(|occluder| {
            occluder.page_idx == page_idx
                && circle_intersects_segment(
                    occluder.center_scene_pos,
                    occluder.radius_scene,
                    line_start,
                    line_end,
                )
        })
    }
}

fn circle_intersects_rect(center: Pos2, radius: f32, rect: Rect) -> bool {
    let closest = Pos2::new(
        center.x.clamp(rect.left(), rect.right()),
        center.y.clamp(rect.top(), rect.bottom()),
    );
    center.distance_sq(closest) <= radius * radius
}

fn circle_intersects_segment(center: Pos2, radius: f32, start: Pos2, end: Pos2) -> bool {
    distance_sq_to_segment(center, start, end) <= radius * radius
}

fn distance_sq_to_segment(point: Pos2, start: Pos2, end: Pos2) -> f32 {
    let segment = end - start;
    let segment_len_sq = segment.length_sq();
    if segment_len_sq <= f32::EPSILON {
        return point.distance_sq(start);
    }
    let t = ((point - start).dot(segment) / segment_len_sq).clamp(0.0, 1.0);
    let projection = start + segment * t;
    point.distance_sq(projection)
}

fn run_quick_text_clean_on_page(
    task: QuickTextCleanTask,
    spread_radius_px: usize,
    uneven_tool: UnevenBackgroundTool,
) -> QuickTextCleanPageResult {
    let page_idx = task.page_idx;
    match run_quick_text_clean_on_page_impl(task, spread_radius_px, uneven_tool) {
        Ok(result) => result,
        Err(error) => QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            regions_partial: 0,
            error: Some(error),
            missing_mask: false,
        },
    }
}

fn run_quick_text_clean_on_page_impl(
    task: QuickTextCleanTask,
    spread_radius_px: usize,
    uneven_tool: UnevenBackgroundTool,
) -> Result<QuickTextCleanPageResult, String> {
    let page_idx = task.page_idx;
    let base_rgba = image::open(&task.page_path)
        .map_err(|err| {
            tf!("cleaning.tab.open_page_error", path = task.page_path.display(), err = err)
        })?
        .to_rgba8();
    // `image` dimensions are u32; widening to usize is lossless on the supported 64-bit
    // targets. try_from keeps the narrowing-back path (below) honest and degrades a
    // pathological dimension to 0, which the engine treats as an empty page.
    let width = usize::try_from(base_rgba.width()).unwrap_or(0);
    let height = usize::try_from(base_rgba.height()).unwrap_or(0);
    let Some(mask_page) = resolve_quick_clean_mask_page(&task) else {
        return Ok(QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            regions_partial: 0,
            error: None,
            missing_mask: true,
        });
    };
    if mask_page.mask_alpha.is_empty() {
        return Ok(QuickTextCleanPageResult {
            page_idx,
            patch: None,
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            regions_partial: 0,
            error: None,
            missing_mask: true,
        });
    }

    // Detector blocks live in source-page pixel space (`mask_page.source_size`).
    // Autoclean operates in page-pixel space (`width`/`height` of the opened page),
    // which is the SAME space the mask is resized into below. Blocks must therefore be
    // scaled by the source->page transform, NOT by the mask->page resize (mask_size ->
    // page). When source_size already equals the page size (the normal case) this is the
    // identity. See the plan's "Data plumbing" note.
    let blocks_page_space = mask_page.blocks.as_ref().map(|blocks| {
        scale_blocks_source_to_page(blocks, mask_page.source_size, width, height)
    });

    // Narrow the page size back to u32 for the mask-size comparison. `width`/`height`
    // originate from a u32 image dimension, so try_from cannot actually fail here; the
    // saturating fallback only guards against a future oversized source.
    let page_size_u32 = [
        u32::try_from(width).unwrap_or(u32::MAX),
        u32::try_from(height).unwrap_or(u32::MAX),
    ];
    let mut binary_mask = mask_page.mask_alpha;
    if mask_page.mask_size != page_size_u32 {
        binary_mask = resize_binary_mask_nearest(
            &binary_mask,
            mask_page.mask_size[0] as usize,
            mask_page.mask_size[1] as usize,
            width,
            height,
        );
    }
    for value in &mut binary_mask {
        *value = if *value > 0 { 255 } else { 0 };
    }

    let outcome = autoclean_page(
        &base_rgba,
        &binary_mask,
        width,
        height,
        spread_radius_px,
        uneven_tool,
        blocks_page_space.as_deref(),
    );
    let has_patch = outcome.patch.pixels.iter().any(|px| px.a() > 0);
    Ok(QuickTextCleanPageResult {
        page_idx,
        patch: has_patch.then_some(outcome.patch),
        regions_total: outcome.regions_total,
        regions_filled: outcome.regions_filled,
        regions_skipped: outcome.regions_skipped,
        regions_partial: outcome.regions_partial,
        error: None,
        missing_mask: false,
    })
}

/// Scale detector blocks from source-page pixel space to page-pixel space.
///
/// `blocks` are `[x1, y1, x2, y2]` in `source_size` pixel space. When `source_size`
/// already matches `page_w`/`page_h` (the normal case) the boxes are returned
/// unchanged; otherwise each edge is scaled by the source->page ratio, flooring the
/// min corner and ceiling the max corner to preserve a covering rect. This is the
/// source->page transform only — the mask nearest-resize (mask_size -> page) never
/// touches blocks.
///
/// Returns NO blocks (an empty vector) when the source size is degenerate (zero width
/// or height) — a zero dimension defines no scale factor, so the caller must fall back
/// to the cluster bbox rather than pass the boxes through unscaled. Individual boxes
/// whose scaled edges are non-finite or fall outside the `i32` range are dropped (the
/// same graceful degrade); an all-dropped set also falls back to the cluster bbox.
fn scale_blocks_source_to_page(
    blocks: &[[i32; 4]],
    source_size: [u32; 2],
    page_w: usize,
    page_h: usize,
) -> Vec<[i32; 4]> {
    let (sw, sh) = (source_size[0], source_size[1]);
    // A degenerate source size cannot define a scale factor. Drop the blocks (return
    // none, NOT the unscaled set) so candidate B falls back to the cluster bbox.
    if sw == 0 || sh == 0 {
        return Vec::new();
    }
    // Identity fast path ONLY for an exact nonzero size match.
    if usize::try_from(sw) == Ok(page_w) && usize::try_from(sh) == Ok(page_h) {
        return blocks.to_vec();
    }
    // Page dims come from a decoded image (<= u32::MAX). If they somehow do not fit u32
    // we cannot form a scale — drop the blocks (graceful: cluster-bbox fallback).
    let (Ok(pw), Ok(ph)) = (u32::try_from(page_w), u32::try_from(page_h)) else {
        return Vec::new();
    };
    let (fx, fy) = (f64::from(pw) / f64::from(sw), f64::from(ph) / f64::from(sh));
    blocks
        .iter()
        .filter_map(|&[x1, y1, x2, y2]| {
            Some([
                scale_edge_to_i32(f64::from(x1) * fx, false)?,
                scale_edge_to_i32(f64::from(y1) * fy, false)?,
                scale_edge_to_i32(f64::from(x2) * fx, true)?,
                scale_edge_to_i32(f64::from(y2) * fy, true)?,
            ])
        })
        .collect()
}

/// Round a scaled block edge to `i32`, rejecting values that cannot be represented.
///
/// `ceil` rounds up (max corner) versus down (min corner) so the integer rect still
/// covers the float rect. Returns `None` when `value` is non-finite or its rounded form
/// falls outside the `i32` range, so the caller drops that block instead of wrapping a
/// stray coordinate into a valid-looking box.
fn scale_edge_to_i32(value: f64, ceil: bool) -> Option<i32> {
    if !value.is_finite() {
        return None;
    }
    let rounded = if ceil { value.ceil() } else { value.floor() };
    if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
        return None;
    }
    // Bounds-checked, integral f64 -> i32: exact, no truncation.
    Some(rounded as i32)
}


fn resolve_quick_clean_mask_page(task: &QuickTextCleanTask) -> Option<TextMaskLoadPage> {
    if let Some(mask) = task.mask_from_model.as_ref() {
        return Some(mask.clone());
    }
    if !task.mask_path.exists() {
        return None;
    }
    let mask_img = image::open(&task.mask_path).ok()?.to_luma8();
    let w = mask_img.width() as usize;
    let h = mask_img.height() as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let mut alpha = Vec::with_capacity(w.saturating_mul(h));
    for px in mask_img.into_raw() {
        alpha.push(if px > 0 { 255 } else { 0 });
    }
    Some(TextMaskLoadPage {
        page_idx: task.page_idx,
        // Disk fallback has no source_size/blocks JSON yet (remaining Phase 2 work):
        // report the mask raster size and no blocks, so candidate B uses the cluster
        // bbox fallback. `blocks` are only scaled against source_size, so its value is
        // inert while blocks is None.
        source_size: [w as u32, h as u32],
        mask_size: [w as u32, h as u32],
        mask_alpha: alpha,
        blocks: None,
    })
}

fn resize_binary_mask_nearest(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 || src.is_empty() {
        return vec![0u8; dst_w.saturating_mul(dst_h)];
    }
    let mut out = vec![0u8; dst_w.saturating_mul(dst_h)];
    for y in 0..dst_h {
        let sy = y.saturating_mul(src_h) / dst_h;
        for x in 0..dst_w {
            let sx = x.saturating_mul(src_w) / dst_w;
            let sidx = sy.saturating_mul(src_w).saturating_add(sx);
            let didx = y.saturating_mul(dst_w).saturating_add(x);
            out[didx] = src.get(sidx).copied().unwrap_or(0);
        }
    }
    out
}

fn resize_color_image_nearest(
    src: &egui::ColorImage,
    dst_w: usize,
    dst_h: usize,
) -> egui::ColorImage {
    if src.size[0] == 0 || src.size[1] == 0 || dst_w == 0 || dst_h == 0 {
        return egui::ColorImage::filled([dst_w.max(1), dst_h.max(1)], egui::Color32::TRANSPARENT);
    }
    let src_w = src.size[0];
    let src_h = src.size[1];
    let mut out = egui::ColorImage::filled([dst_w, dst_h], egui::Color32::TRANSPARENT);
    for y in 0..dst_h {
        let sy = y.saturating_mul(src_h) / dst_h;
        for x in 0..dst_w {
            let sx = x.saturating_mul(src_w) / dst_w;
            let sidx = sy.saturating_mul(src_w).saturating_add(sx);
            let didx = y.saturating_mul(dst_w).saturating_add(x);
            if let (Some(src_px), Some(dst_px)) = (src.pixels.get(sidx), out.pixels.get_mut(didx)) {
                *dst_px = *src_px;
            }
        }
    }
    out
}

fn load_text_masks_from_storage(
    storage_dir: &Path,
    page_indices: &[usize],
) -> Result<TextMaskLoadResult, String> {
    if !storage_dir.exists() {
        return Ok(TextMaskLoadResult {
            pages: Vec::new(),
            loaded: 0,
            missing: page_indices.len(),
            failed: 0,
        });
    }

    let mut pages = Vec::<TextMaskLoadPage>::new();
    let mut loaded = 0usize;
    let mut missing = 0usize;
    let mut failed = 0usize;

    for page_idx in page_indices {
        let path = text_detection_mask_file_path(storage_dir, *page_idx);
        if !path.exists() {
            missing = missing.saturating_add(1);
            continue;
        }
        match image::open(&path) {
            Ok(img) => {
                let luma = img.to_luma8();
                let w = luma.width();
                let h = luma.height();
                if w == 0 || h == 0 {
                    failed = failed.saturating_add(1);
                    continue;
                }
                let mut alpha = Vec::with_capacity((w as usize).saturating_mul(h as usize));
                for px in luma.into_raw() {
                    alpha.push(if px > 0 { 255 } else { 0 });
                }
                pages.push(TextMaskLoadPage {
                    page_idx: *page_idx,
                    // Display-cache load: mask raster only, no detector blocks.
                    source_size: [w, h],
                    mask_size: [w, h],
                    mask_alpha: alpha,
                    blocks: None,
                });
                loaded = loaded.saturating_add(1);
            }
            Err(_) => {
                failed = failed.saturating_add(1);
            }
        }
    }

    Ok(TextMaskLoadResult {
        pages,
        loaded,
        missing,
        failed,
    })
}

fn save_clean_overlay_snapshots(
    save_dir: &std::path::Path,
    snapshots: &[(String, Arc<image::RgbaImage>)],
) -> Result<(), String> {
    std::fs::create_dir_all(save_dir)
        .map_err(|err| tf!("cleaning.tab.create_dir_error", dir = save_dir.display(), err = err))?;
    for (stem, image) in snapshots {
        let dst = save_dir.join(format!("{stem}.png"));
        image
            .save(&dst)
            .map_err(|err| tf!("cleaning.tab.save_clean_file_error", path = dst.display(), err = err))?;
    }
    Ok(())
}

fn text_detection_mask_file_path(dir: &Path, page_idx: usize) -> PathBuf {
    dir.join(format!("{page_idx:05}_mask.png"))
}

struct TextMaskOverlayDrawParams<'a> {
    textures: &'a mut HashMap<usize, TextMaskTexturePage>,
    ctx: &'a egui::Context,
    painter: &'a egui::Painter,
    page_idx: usize,
    page_rect: Rect,
    mask_size: [u32; 2],
    mask_alpha: &'a [u8],
    current_frame: u64,
    texture_options: egui::TextureOptions,
}

fn draw_text_mask_overlay_on_page(params: TextMaskOverlayDrawParams<'_>) {
    let TextMaskOverlayDrawParams {
        textures,
        ctx,
        painter,
        page_idx,
        page_rect,
        mask_size,
        mask_alpha,
        current_frame,
        texture_options,
    } = params;
    if mask_alpha.is_empty() {
        return;
    }
    let mask_w = mask_size[0] as usize;
    let mask_h = mask_size[1] as usize;
    if mask_w == 0 || mask_h == 0 {
        return;
    }
    let expected_len = mask_w.saturating_mul(mask_h);
    if expected_len == 0 || expected_len != mask_alpha.len() {
        return;
    }

    // Rebuild when size changes or when the active sampling mode flips, so the
    // mask matches source/overlay sampling (mirror of the overlay runtime).
    let needs_rebuild = textures
        .get(&page_idx)
        .map(|page| page.size != [mask_w, mask_h] || page.texture_options != texture_options)
        .unwrap_or(true);
    if needs_rebuild {
        let page_tex = build_text_mask_texture_page(
            ctx,
            page_idx,
            [mask_w, mask_h],
            mask_alpha,
            texture_options,
        );
        textures.insert(page_idx, page_tex);
    }

    let Some(page_tex) = textures.get_mut(&page_idx) else {
        return;
    };
    page_tex.last_used_frame = current_frame;
    let src_w = page_tex.size[0] as f32;
    let src_h = page_tex.size[1] as f32;
    if src_w <= 0.0 || src_h <= 0.0 {
        return;
    }
    // Viewport cull: the painter is already clipped to the visible page region,
    // so skip tiles whose destination rect falls outside it. `intersects`
    // keeps partially-visible edge tiles.
    let viewport_rect = painter.clip_rect();
    for tile in &page_tex.tiles {
        let ox = tile.origin_px[0] as f32;
        let oy = tile.origin_px[1] as f32;
        let tw = tile.size_px[0] as f32;
        let th = tile.size_px[1] as f32;
        if tw <= 0.0 || th <= 0.0 {
            continue;
        }
        let dst = Rect::from_min_size(
            egui::pos2(
                page_rect.left() + page_rect.width() * (ox / src_w),
                page_rect.top() + page_rect.height() * (oy / src_h),
            ),
            egui::vec2(
                page_rect.width() * (tw / src_w),
                page_rect.height() * (th / src_h),
            ),
        );
        if !dst.intersects(viewport_rect) {
            continue;
        }
        painter.image(
            tile.texture.id(),
            dst,
            Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

fn build_text_mask_texture_page(
    ctx: &egui::Context,
    page_idx: usize,
    size: [usize; 2],
    alpha: &[u8],
    texture_options: egui::TextureOptions,
) -> TextMaskTexturePage {
    let w = size[0];
    let h = size[1];
    if w == 0 || h == 0 {
        return TextMaskTexturePage {
            size,
            tiles: Vec::new(),
            last_used_frame: 0,
            texture_options,
        };
    }

    let mut tiles = Vec::new();
    let mut y = 0usize;
    while y < h {
        let mut x = 0usize;
        while x < w {
            let tw = (w - x).min(TEXT_MASK_TILE_SIDE);
            let th = (h - y).min(TEXT_MASK_TILE_SIDE);
            let tile_img = build_text_mask_tile_image(size, alpha, x, y, tw, th);
            let texture = ctx.load_texture(
                format!("cleaning-text-mask-{page_idx}-{x}-{y}"),
                tile_img,
                texture_options,
            );
            tiles.push(TextMaskTextureTile {
                texture,
                origin_px: [x, y],
                size_px: [tw, th],
            });
            x += TEXT_MASK_TILE_SIDE;
        }
        y += TEXT_MASK_TILE_SIDE;
    }
    TextMaskTexturePage {
        size,
        tiles,
        last_used_frame: 0,
        texture_options,
    }
}

fn text_mask_texture_page_estimated_bytes(page_tex: &TextMaskTexturePage) -> u64 {
    let bytes = page_tex
        .tiles
        .iter()
        .map(|tile| {
            tile.size_px[0]
                .saturating_mul(tile.size_px[1])
                .saturating_mul(4)
        })
        .fold(0usize, usize::saturating_add);
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

fn build_text_mask_tile_image(
    size: [usize; 2],
    alpha: &[u8],
    origin_x: usize,
    origin_y: usize,
    tile_w: usize,
    tile_h: usize,
) -> egui::ColorImage {
    let full_w = size[0];
    let mut raw = vec![0u8; tile_w.saturating_mul(tile_h).saturating_mul(4)];
    for ty in 0..tile_h {
        let sy = origin_y + ty;
        let row_off = sy.saturating_mul(full_w);
        for tx in 0..tile_w {
            let sx = origin_x + tx;
            let src_idx = row_off.saturating_add(sx);
            let dst_idx = ty
                .saturating_mul(tile_w)
                .saturating_add(tx)
                .saturating_mul(4);
            let src_alpha = alpha.get(src_idx).copied().unwrap_or(0);
            let a = ((src_alpha as u16 * TEXT_MASK_VISUAL_ALPHA_MAX as u16) / 255) as u8;
            raw[dst_idx] = a;
            raw[dst_idx + 1] = 0;
            raw[dst_idx + 2] = 0;
            raw[dst_idx + 3] = a;
        }
    }
    egui::ColorImage::from_rgba_premultiplied([tile_w, tile_h], &raw)
}

#[cfg(test)]
mod tests {
    use super::{
        AREA_EDIT_TOOL_INDICES, BRUSH_TOOL_INDICES, CLEANING_ACTIVE_TOOL_TAB,
        CLEANING_AREA_EDITOR_TAB, CLEANING_CLEAN_TAB, CLEANING_PANEL_CHROME_WIDTH_PX,
        CLEANING_QUICK_CLEAN_TAB, CLEANING_TOOLS_TAB,
        CleaningTabState, MASK_REMOVAL_TOOL_INDICES, cleaning_default_dock_layout,
        cleaning_row_width, cleaning_tab_outer_width, drawn_tool_indices,
        scale_blocks_source_to_page, scale_edge_to_i32,
    };
    use crate::canvas::CANVAS_RIBBON_TAB;
    use crate::widgets::panel_dock::{DockEdge, PanelAnchor, PanelId};
    use egui::{Pos2, Rect, Vec2};
    use std::collections::BTreeSet;

    /// The default arrangement is the one the dock is handed on a first run AND the
    /// dictionary `panel_dock::persist` resolves stored tab keys against, so it has
    /// to be well-formed and to name every tab this program tab can declare.
    #[test]
    fn the_default_dock_layout_places_the_six_cleaning_panels() {
        let layout = cleaning_default_dock_layout();
        assert_eq!(layout.validate(), Ok(()));
        assert_eq!(layout.panels().len(), 6);

        // «Лента» holds the LEFT viewport edge, and keeps the id every canvas program
        // tab's builder gives it, so a user who already arranged the ribbon under an
        // earlier build finds their panel where they left it.
        let ribbon = layout
            .panel(PanelId::new(0))
            .expect("the ribbon panel exists");
        assert_eq!(ribbon.tabs, vec![CANVAS_RIBBON_TAB]);
        assert_eq!(
            ribbon.anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0,
            }
        );

        // «Редактор области» hangs UNDER the ribbon. Anchoring it to the ribbon's
        // `Left` instead would place it outside the dock area and un-flush the whole
        // chain, and giving it the viewport edge would make the ribbon depend on a
        // panel that is hidden for every tool but one.
        let area_editor = layout
            .panel(PanelId::new(5))
            .expect("the area-editor panel exists");
        assert_eq!(area_editor.tabs, vec![CLEANING_AREA_EDITOR_TAB]);
        assert_eq!(
            area_editor.anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );

        // «Клин» reproduces the island, which floated to the RIGHT of the canvas
        // controls the ribbon replaced.
        let clean = layout
            .panel(PanelId::new(1))
            .expect("the clean panel exists");
        assert_eq!(clean.tabs, vec![CLEANING_CLEAN_TAB]);
        assert_eq!(
            clean.anchor,
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Right,
                align: 0.0,
            }
        );

        // The two tool tabs reproduce the tool window at the right edge, one under
        // the other — where the `ui.separator()` used to split it.
        let tools = layout
            .panel(PanelId::new(2))
            .expect("the tools panel exists");
        assert_eq!(tools.tabs, vec![CLEANING_TOOLS_TAB]);
        assert_eq!(
            tools.anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Right,
                along: 0.0,
            }
        );
        let active_tool = layout
            .panel(PanelId::new(3))
            .expect("the active-tool panel exists");
        assert_eq!(active_tool.tabs, vec![CLEANING_ACTIVE_TOOL_TAB]);
        assert_eq!(
            active_tool.anchor,
            PanelAnchor::Panel {
                target: PanelId::new(2),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );

        // The quick-clean panel hangs off «Клин», where the button that opens it
        // lives. It is the only conditional panel here, and it must not share an
        // anchor with the other `Bottom` one: identical target+edge+align would lay
        // the two on exactly the same rect.
        let quick_clean = layout
            .panel(PanelId::new(4))
            .expect("the quick-clean panel exists");
        assert_eq!(quick_clean.tabs, vec![CLEANING_QUICK_CLEAN_TAB]);
        assert_eq!(
            quick_clean.anchor,
            PanelAnchor::Panel {
                target: PanelId::new(1),
                edge: DockEdge::Bottom,
                align: 0.0,
            }
        );
        assert_ne!(
            quick_clean.anchor, active_tool.anchor,
            "the three Bottom anchors must name different targets"
        );
        assert_ne!(quick_clean.anchor, area_editor.anchor);
        assert_ne!(active_tool.anchor, area_editor.anchor);

        // Every panel is content-sized: they take their width from their tabs' own
        // `min_size`, which is measured per frame from the captions.
        for id in [0, 1, 2, 3, 4, 5].map(PanelId::new) {
            let panel = layout.panel(id).expect("a default panel exists");
            assert_eq!(panel.size_override, None, "{id} must stay content-sized");
        }

        // A `TabId` missing here is dropped from the user's stored arrangement on
        // every load (`panel_dock::persist::known_tabs`), so this list is the whole
        // set this program tab can declare.
        for tab in [
            CANVAS_RIBBON_TAB,
            CLEANING_CLEAN_TAB,
            CLEANING_TOOLS_TAB,
            CLEANING_ACTIVE_TOOL_TAB,
            CLEANING_QUICK_CLEAN_TAB,
            CLEANING_AREA_EDITOR_TAB,
        ] {
            assert!(layout.panel_of_tab(tab).is_some(), "{tab} has no panel");
        }
    }

    /// «Редактор области» is hidden for every tool but the area editor, so it must be
    /// a LEAF: dropping it may not touch the other five, in the model or on screen.
    ///
    /// `panel_dock::frame_layout` drops a panel with nothing to draw by calling
    /// `DockLayout::remove_panel`, which re-anchors every dependant to the REMOVED
    /// panel's own anchor. Nothing is anchored to this one, so that re-anchoring must
    /// have no dependants to reach — which is exactly what the anchor comparison
    /// below checks, and the solve after it checks that the five surviving panels are
    /// laid out at the very same rects with the tab gone.
    #[test]
    fn hiding_the_area_editor_leaves_every_other_panel_untouched() {
        const SURVIVORS: [PanelId; 5] = [
            PanelId::new(0),
            PanelId::new(1),
            PanelId::new(2),
            PanelId::new(3),
            PanelId::new(4),
        ];
        let full = cleaning_default_dock_layout();
        let before: Vec<(PanelId, PanelAnchor)> = SURVIVORS
            .iter()
            .map(|id| {
                let panel = full.panel(*id).expect("a default panel exists");
                (*id, panel.anchor)
            })
            .collect();

        let mut hidden = full.clone();
        hidden
            .remove_panel(PanelId::new(5))
            .expect("the area-editor panel can be dropped for a frame");
        assert_eq!(hidden.validate(), Ok(()));
        assert_eq!(hidden.panels().len(), 5);
        for (id, anchor) in &before {
            let panel = hidden.panel(*id).expect("a survivor keeps its panel");
            assert_eq!(
                panel.anchor, *anchor,
                "{id} inherited an anchor from the dropped area editor"
            );
        }
        assert_eq!(
            hidden
                .panel(PanelId::new(0))
                .expect("the ribbon survives the drop")
                .anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0,
            },
            "the ribbon keeps the left viewport edge whether or not the tab is shown"
        );

        // The same five rects with the tab gone: the model being unchanged is only
        // half the property the user sees.
        let area = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1600.0, 1000.0));
        let with = solve_default_layout(&full, area, CLEAN_TAB_WIDEST_LOCALE_WIDTH_PX);
        let without = solve_default_layout(&hidden, area, CLEAN_TAB_WIDEST_LOCALE_WIDTH_PX);
        let survivors: Vec<(PanelId, Rect)> = with
            .into_iter()
            .filter(|(id, _)| *id != PanelId::new(5))
            .collect();
        assert_eq!(
            survivors, without,
            "showing the area editor moved a panel that does not depend on it"
        );
    }

    /// What the user asked for: «Редактор области» opens directly UNDER «Лента», left
    /// edges flush, and never beside it or on top of it.
    #[test]
    fn the_area_editor_opens_directly_under_the_ribbon() {
        let area = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1600.0, 1000.0));
        let solved = solve_default_layout(
            &cleaning_default_dock_layout(),
            area,
            CLEAN_TAB_WIDEST_LOCALE_WIDTH_PX,
        );
        let rect_of = |id: PanelId| {
            solved
                .iter()
                .find(|(other, _)| *other == id)
                .map(|(_, rect)| *rect)
                .expect("every default panel is placed")
        };
        let ribbon = rect_of(PanelId::new(0));
        let area_editor = rect_of(PanelId::new(5));
        assert!(
            (area_editor.left() - ribbon.left()).abs() < 1e-3,
            "left edges must line up: ribbon {ribbon:?} vs area editor {area_editor:?}"
        );
        assert!(
            area_editor.top() >= ribbon.bottom() - 1e-3,
            "the area editor must start below the ribbon: {area_editor:?} vs {ribbon:?}"
        );
        assert!(
            !area_editor.intersects(ribbon),
            "the two must not overlap: {area_editor:?} vs {ribbon:?}"
        );
        // Nothing may be laid out in the gap between them, or "directly under" is a
        // claim about ids rather than about what the user sees.
        // Built through `max` so a sub-tolerance overlap cannot produce an INVERTED
        // rect, whose `intersects` would answer nonsense instead of failing here.
        let between = Rect::from_min_max(
            Pos2::new(ribbon.left(), ribbon.bottom()),
            Pos2::new(ribbon.right(), area_editor.top().max(ribbon.bottom())),
        );
        for (id, rect) in &solved {
            if *id == PanelId::new(0) || *id == PanelId::new(5) {
                continue;
            }
            assert!(
                !rect.intersects(between),
                "panel {id} sits between the ribbon and the area editor: {rect:?}"
            );
        }
    }

    /// Solving matters as much as being well-formed: the default arrangement is what
    /// the user SEES on a first run, and a panel laid out on top of another one is
    /// unreachable — the buried one cannot even be dragged out.
    ///
    /// Four frames are checked, because two of the six panels are conditional:
    /// «Быстрый клин найденного текста» follows its toggle and «Редактор области»
    /// follows the active tool's `wants_main_panel`. `panel_dock::frame_layout` —
    /// built on `DockLayout::remove_panel` — drops a hidden panel for the frame, so
    /// each combination is a layout the user really gets.
    ///
    /// The two tool panels hang off the RIGHT viewport edge while «Лента», «Клин»,
    /// «Редактор области» and the quick-clean panel hang off the LEFT one, so
    /// they form two independent chains that the solver clamps independently and
    /// nothing stops from meeting in the middle of a narrow area (see the threshold
    /// assertion below, measured for the ordinary frame in which the area editor is
    /// hidden).
    #[test]
    fn the_default_dock_layout_solves_into_disjoint_panels() {
        // A maximised 1080p studio window: the canvas area of a 1920-wide window
        // less the page-manager column and the scrollbar reserve. Deliberately not
        // the largest possible area — the arrangement has to fit an ordinary
        // window, not only a wide one.
        let area = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1600.0, 1000.0));
        let all = cleaning_default_dock_layout();
        let mut editor_hidden = all.clone();
        editor_hidden
            .remove_panel(PanelId::new(5))
            .expect("the area-editor panel can be dropped for a frame");
        // The ordinary frame: neither conditional panel is shown. This is the
        // arrangement the pinned overlap band below was measured on.
        let open = editor_hidden.clone();
        let mut closed = editor_hidden;
        closed
            .remove_panel(PanelId::new(4))
            .expect("the quick-clean panel can be dropped for a frame");
        let mut all_but_quick_clean = all.clone();

        all_but_quick_clean
            .remove_panel(PanelId::new(4))
            .expect("the quick-clean panel can be dropped for a frame");
        for (frame, layout, expected_panels) in [
            ("quick clean open", &open, 5),
            ("quick clean closed", &closed, 4),
            ("area editor open", &all_but_quick_clean, 5),
            ("both conditional panels open", &all, 6),
        ] {
            let solved = solve_default_layout(layout, area, CLEAN_TAB_WIDEST_LOCALE_WIDTH_PX);
            assert_eq!(solved.len(), expected_panels, "{frame}: every panel is placed");
            for (id, rect) in &solved {
                assert!(
                    area.contains_rect(*rect),
                    "{frame}: panel {id} is laid out outside the dock area: {rect:?}"
                );
            }
            for (i, (id_a, rect_a)) in solved.iter().enumerate() {
                for (id_b, rect_b) in &solved[i + 1..] {
                    assert!(
                        !rect_a.intersects(*rect_b),
                        "{frame}: panels {id_a} and {id_b} overlap: {rect_a:?} vs {rect_b:?}"
                    );
                }
            }
        }

        // The two chains DO meet once the area is narrow enough — that is a property
        // of two opposite viewport edges, not of this arrangement, and the surfaces
        // this replaced overlapped in exactly the same way (the island and the
        // quick-clean window sat at fixed offsets from the left edge, the tool window
        // at a fixed offset from the right one). It is pinned here so the width it
        // happens at cannot drift silently: measured at 1216 pt for the widest locale
        // (French), i.e. the arrangement holds on any window at least ~1220 pt wide
        // and the chains meet below that, where the solver has already shrunk both to
        // their floors. The quick-clean panel does not move it — it is under «Клин»,
        // not beside it.
        let first_overlap = (400..=1600)
            .rev()
            .map(|width| f32::from(u16::try_from(width).unwrap_or(u16::MAX)))
            .find(|width| {
                let narrow = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(*width, 1000.0));
                let panels = solve_default_layout(&open, narrow, CLEAN_TAB_WIDEST_LOCALE_WIDTH_PX);
                panels.iter().enumerate().any(|(i, (_, rect_a))| {
                    panels[i + 1..]
                        .iter()
                        .any(|(_, rect_b)| rect_a.intersects(*rect_b))
                })
            })
            .expect("the two chains meet at some width");
        assert!(
            (1100.0..1300.0).contains(&first_overlap),
            "the chains first overlap at {first_overlap} pt, outside the pinned band"
        );
    }

    /// Width the «Клин» tab opens at in the widest of the five shipped locales.
    ///
    /// A fixed number here because the real one is measured from font galleys, which
    /// need a live `Context`, and the arrangement has to hold for the WIDEST locale.
    /// Measured with `cleaning_clean_tab_size_bounds` under egui's default font
    /// stack: fr 464, ru 413, pt 408, es 389, en 335 — French is the widest, and the
    /// value here clears it, since the app's own font stack is wider than egui's.
    const CLEAN_TAB_WIDEST_LOCALE_WIDTH_PX: f32 = 500.0;

    /// Solves `layout` inside `area` with the sizes the tabs declare, and returns the
    /// drawn rects in panel order.
    fn solve_default_layout(
        layout: &crate::widgets::panel_dock::DockLayout,
        area: Rect,
        clean_width: f32,
    ) -> Vec<(PanelId, Rect)> {
        use crate::canvas::{CANVAS_RIBBON_TAB_INITIAL_SIZE_PX, CANVAS_RIBBON_TAB_MIN_SIZE_PX};
        use crate::widgets::panel_dock::{HostId, PanelChrome, PanelSizes, solve};

        // What the first frame feeds the solver: each panel's largest tab's
        // `initial_size`, and its `min_size` as the floor. The two measured widths
        // stand in for what the captions produce at runtime.
        let sizes: PanelSizes = [
            (PanelId::new(0), CANVAS_RIBBON_TAB_INITIAL_SIZE_PX),
            (
                PanelId::new(1),
                Vec2::new(clean_width, super::CLEANING_CLEAN_TAB_INITIAL_HEIGHT_PX),
            ),
            (PanelId::new(2), super::CLEANING_TOOLS_TAB_INITIAL_SIZE_PX),
            (
                PanelId::new(3),
                super::CLEANING_ACTIVE_TOOL_TAB_INITIAL_SIZE_PX,
            ),
            (
                PanelId::new(4),
                Vec2::new(
                    clean_width,
                    super::CLEANING_QUICK_CLEAN_TAB_INITIAL_HEIGHT_PX,
                ),
            ),
            (
                PanelId::new(5),
                super::CLEANING_AREA_EDITOR_TAB_INITIAL_SIZE_PX,
            ),
        ]
        .into_iter()
        .collect();
        let mins: PanelSizes = [
            (PanelId::new(0), CANVAS_RIBBON_TAB_MIN_SIZE_PX),
            (
                PanelId::new(1),
                Vec2::new(clean_width, super::CLEANING_CLEAN_TAB_MIN_HEIGHT_PX),
            ),
            (
                PanelId::new(2),
                Vec2::new(200.0, super::CLEANING_TOOLS_TAB_MIN_HEIGHT_PX),
            ),
            (
                PanelId::new(3),
                super::CLEANING_ACTIVE_TOOL_TAB_MIN_SIZE_PX,
            ),
            (
                PanelId::new(4),
                Vec2::new(300.0, super::CLEANING_QUICK_CLEAN_TAB_MIN_HEIGHT_PX),
            ),
            (
                PanelId::new(5),
                super::CLEANING_AREA_EDITOR_TAB_MIN_SIZE_PX,
            ),
        ]
        .into_iter()
        .collect();
        let solved = solve(
            layout,
            HostId::MainWindow,
            area,
            &sizes,
            &mins,
            PanelChrome::default(),
        );
        let mut placed: Vec<(PanelId, Rect)> =
            solved.iter().map(|(id, panel)| (id, panel.rect)).collect();
        placed.sort_by_key(|(id, _)| *id);
        placed
    }

    /// Every registered tool must appear in exactly one button group, or it is
    /// unreachable: the picker draws the three index groups and nothing else, so a
    /// 13th tool pushed onto `CleaningTabState::default`'s list without touching a
    /// group constant would simply never be shown.
    #[test]
    fn every_registered_tool_is_drawn_by_exactly_one_button_group() {
        let state = CleaningTabState::default();
        let drawn: Vec<usize> = drawn_tool_indices().collect();
        let unique: BTreeSet<usize> = drawn.iter().copied().collect();
        assert_eq!(
            drawn.len(),
            unique.len(),
            "a tool index appears in more than one group: {drawn:?}"
        );
        let expected: BTreeSet<usize> = (0..state.tools.len()).collect();
        assert_eq!(
            unique, expected,
            "the button groups {:?} do not cover the {} registered tools",
            unique,
            state.tools.len()
        );
        // Guards the three constants themselves against a silent re-partition.
        let group_total = BRUSH_TOOL_INDICES.len()
            + MASK_REMOVAL_TOOL_INDICES.len()
            + AREA_EDIT_TOOL_INDICES.len();
        assert_eq!(group_total, state.tools.len());
    }

    /// The width floor is the WIDEST single control plus the panel's chrome and the
    /// scrollbar reserve: the rows wrap, so one control per row is still usable, and
    /// anything narrower clips a caption no row break can rescue.
    #[test]
    fn cleaning_tab_outer_width_takes_the_widest_control_plus_chrome() {
        assert_eq!(
            cleaning_tab_outer_width([80.0, 137.5, 40.0], 10.0),
            137.5 + CLEANING_PANEL_CHROME_WIDTH_PX + 10.0
        );
    }

    /// A `NaN` minimum would poison the whole solve, and a non-positive one is not a
    /// measurement — both are skipped rather than propagated. With nothing usable
    /// left the floor is the chrome and the reserve alone; the solver's own
    /// `PANEL_MIN_WIDTH` still applies underneath it.
    #[test]
    fn cleaning_tab_outer_width_skips_unusable_measurements() {
        assert_eq!(
            cleaning_tab_outer_width([f32::NAN, 60.0, -10.0, f32::INFINITY, 0.0], 0.0),
            60.0 + CLEANING_PANEL_CHROME_WIDTH_PX
        );
        assert_eq!(
            cleaning_tab_outer_width([], 0.0),
            CLEANING_PANEL_CHROME_WIDTH_PX
        );
    }

    /// A row is the sum of its controls plus one gap BETWEEN each pair — the number
    /// the «Клин» tab opens at, so an off-by-one gap here is a permanent scrollbar.
    #[test]
    fn cleaning_row_width_counts_one_gap_between_each_pair() {
        assert_eq!(cleaning_row_width(&[100.0, 50.0, 25.0], 8.0), 191.0);
        assert_eq!(cleaning_row_width(&[100.0], 8.0), 100.0);
        assert_eq!(cleaning_row_width(&[], 8.0), 0.0);
    }

    #[test]
    fn scale_blocks_identity_passthrough_on_exact_size_match() {
        let blocks = [[1, 2, 3, 4], [10, 20, 30, 40]];
        // Nonzero source size equal to the page size returns the blocks unchanged.
        let out = scale_blocks_source_to_page(&blocks, [100, 200], 100, 200);
        assert_eq!(out, blocks.to_vec());
    }

    #[test]
    fn scale_blocks_degenerate_source_drops_blocks() {
        let blocks = [[1, 2, 3, 4]];
        // Finding 5: a zero source dimension yields NO blocks (not the unscaled set), so
        // candidate B falls back to the cluster bbox.
        assert!(scale_blocks_source_to_page(&blocks, [0, 200], 100, 200).is_empty());
        assert!(scale_blocks_source_to_page(&blocks, [100, 0], 100, 200).is_empty());
    }

    #[test]
    fn scale_blocks_scales_and_rounds_outward() {
        let blocks = [[10, 20, 30, 40]];
        // Source 100x200 -> page 200x400 doubles every edge; min floored, max ceiled.
        let out = scale_blocks_source_to_page(&blocks, [100, 200], 200, 400);
        assert_eq!(out, vec![[20, 40, 60, 80]]);
    }

    #[test]
    fn scale_edge_rejects_non_finite_and_out_of_range() {
        assert_eq!(scale_edge_to_i32(f64::NAN, false), None);
        assert_eq!(scale_edge_to_i32(f64::INFINITY, true), None);
        assert_eq!(scale_edge_to_i32(f64::from(i32::MAX) + 1.0, true), None);
        assert_eq!(scale_edge_to_i32(f64::from(i32::MIN) - 1.0, false), None);
        assert_eq!(scale_edge_to_i32(2.3, false), Some(2));
        assert_eq!(scale_edge_to_i32(2.3, true), Some(3));
    }
}


