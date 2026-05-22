/*
File: src/canvas/types.rs

Purpose:
Пассивные типы canvas-модуля: публичные enum/DTO и внутренние runtime-снимки состояния.

Main responsibilities:
- хранить простые структуры данных без тяжёлой логики;
- задавать базовые enum-ы для bubble/canvas режимов;
- держать внутренние runtime payload-структуры для overlay/bubble/settings.

Key structures:
- CanvasUiStatus
- SourceTextureUploadBudget
- BubbleAction / BubbleType / BubbleMode / BubbleTextField / BubbleCopyPasteTarget
- RectCoords
- RuntimeBubble
- OverlayPreparedTile / OverlayPreparedPage
- CanvasState

Notes:
- Типы, которые используются только внутри canvas runtime, помечены как `pub(crate)`.
- Поведение ограничено небольшими helper-методами без побочных эффектов.
*/

use crate::bubble_status::{BubbleStatusRule, default_bubble_status_rules};
use crate::project::{Bubble, Side};
use eframe::egui;
use egui::{Pos2, Rect, Vec2};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;

const SOURCE_REUPLOAD_TILE_BUDGET_PER_FRAME: usize = 2;
const SOURCE_REUPLOAD_BYTES_BUDGET_PER_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct CanvasUiStatus {
    pub loaded_pages: usize,
    pub total_pages: usize,
    pub load_errors_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceTextureUploadBudget {
    tile_budget: usize,
    bytes_budget: usize,
}

impl SourceTextureUploadBudget {
    #[must_use]
    pub fn new(tile_budget: usize, bytes_budget: usize) -> Self {
        Self {
            tile_budget,
            bytes_budget,
        }
    }

    #[must_use]
    pub fn source_page_reupload_default() -> Self {
        Self::new(
            SOURCE_REUPLOAD_TILE_BUDGET_PER_FRAME,
            SOURCE_REUPLOAD_BYTES_BUDGET_PER_FRAME,
        )
    }

    pub(crate) fn try_consume(&mut self, bytes: usize) -> bool {
        if self.tile_budget == 0 || self.bytes_budget < bytes {
            return false;
        }
        self.tile_budget -= 1;
        self.bytes_budget -= bytes;
        true
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CanvasFrameParams {
    pub(crate) canvas_rect: Rect,
    pub(crate) suppress_wheel_scroll: bool,
    pub(crate) zoom_drag_active: bool,
    pub(crate) hook_claims_shift_drag: bool,
    pub(crate) overlays_enabled: bool,
    pub(crate) space_pan_drag_enabled: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CanvasScenePageFrame {
    pub(crate) page_idx: usize,
    pub(crate) row_rect: Rect,
    pub(crate) image_rect: Rect,
    pub(crate) page_in_view: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingZoomAnchor {
    pub(crate) viewport_local: Vec2,
    pub(crate) world_focus: Vec2,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OverlayUploadBudget {
    pub(crate) tile_budget: usize,
    pub(crate) bytes_budget: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BubbleAction {
    Translate,
    Delete,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BubbleType {
    Default,
    Aside,
    OnTop,
}

impl BubbleType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Aside => "aside",
            Self::OnTop => "on_top",
        }
    }

    pub fn from_str(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("default") {
            Self::Default
        } else if raw.eq_ignore_ascii_case("on_top") {
            Self::OnTop
        } else {
            Self::Aside
        }
    }

    pub fn resolved(self, fallback: BubbleType) -> BubbleType {
        match self {
            Self::Default => match fallback {
                Self::Default => Self::Aside,
                other => other,
            },
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BubbleMode {
    Aside,
    OnTop,
    Hybrid,
}

impl BubbleMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aside => "aside",
            Self::OnTop => "on_top",
            Self::Hybrid => "hybrid",
        }
    }

    pub fn from_str(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("hybrid") {
            Self::Hybrid
        } else if raw.eq_ignore_ascii_case("on_top") {
            Self::OnTop
        } else {
            Self::Aside
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AsideBubbleCompactMode {
    None,
    Moderate,
    Strong,
}

impl AsideBubbleCompactMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Moderate => "moderate",
            Self::Strong => "strong",
        }
    }

    pub fn from_str(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("moderate") {
            Self::Moderate
        } else if raw.eq_ignore_ascii_case("strong") {
            Self::Strong
        } else {
            Self::None
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AsideBubbleSideMode {
    Auto,
    Left,
    Right,
}

impl AsideBubbleSideMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Left => "left",
            Self::Right => "right",
        }
    }

    pub fn from_str(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("left") {
            Self::Left
        } else if raw.eq_ignore_ascii_case("right") {
            Self::Right
        } else {
            Self::Auto
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OnTopFocusMode {
    Around,
    Aside,
}

impl OnTopFocusMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Around => "around",
            Self::Aside => "aside",
        }
    }

    pub fn from_str(raw: &str) -> Self {
        if raw.eq_ignore_ascii_case("aside") {
            Self::Aside
        } else {
            Self::Around
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RectCoords {
    pub p1: Pos2,
    pub p2: Pos2,
}

impl RectCoords {
    pub(crate) fn normalized(self) -> Self {
        Self {
            p1: egui::pos2(self.p1.x.min(self.p2.x), self.p1.y.min(self.p2.y)),
            p2: egui::pos2(self.p1.x.max(self.p2.x), self.p1.y.max(self.p2.y)),
        }
    }

    pub(crate) fn center_uv(self) -> Pos2 {
        egui::pos2((self.p1.x + self.p2.x) * 0.5, (self.p1.y + self.p2.y) * 0.5)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeBubble {
    pub(crate) id: i64,
    pub(crate) img_idx: usize,
    pub(crate) img_u: f32,
    pub(crate) img_v: f32,
    pub(crate) side: Side,
    pub(crate) bubble_type: BubbleType,
    pub(crate) text: String,
    pub(crate) original_text: String,
    pub(crate) rect_coords: RectCoords,
    pub(crate) anchor_y: f32,
    pub(crate) max_width_px: f32,
    pub(crate) height_px: f32,
    pub(crate) line_x: f32,
    pub(crate) mounted: bool,
}

impl RuntimeBubble {
    pub(crate) fn display_text(&self) -> &str {
        let txt = self.text.trim();
        if txt.is_empty() {
            self.original_text.trim()
        } else {
            txt
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum AsideDragTarget {
    BubbleBody,
    RectArea,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AsideDragState {
    pub(crate) bid: i64,
    pub(crate) target: AsideDragTarget,
    pub(crate) last_pointer_pos: Pos2,
    pub(crate) moved: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct OnTopDragState {
    pub(crate) bid: i64,
    pub(crate) last_pointer_pos: Pos2,
    pub(crate) moved: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BubbleTextField {
    Original,
    Translation,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BubbleCopyPasteTarget {
    Original,
    Translation,
    WholeBubble,
}

impl BubbleCopyPasteTarget {
    pub(crate) fn as_text_field(self) -> Option<BubbleTextField> {
        match self {
            Self::Original => Some(BubbleTextField::Original),
            Self::Translation => Some(BubbleTextField::Translation),
            Self::WholeBubble => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CopiedBubbleData {
    pub(crate) bubble_type: BubbleType,
    pub(crate) text: String,
    pub(crate) original_text: String,
    pub(crate) extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingBubblePaste {
    pub(crate) bid: i64,
    pub(crate) field: BubbleTextField,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FocusedBubbleTextInput {
    pub(crate) bid: i64,
    pub(crate) field: BubbleTextField,
    pub(crate) has_selection: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CanvasContextMenuTarget {
    pub(crate) page_idx: usize,
    pub(crate) page_uv: Pos2,
}

#[derive(Clone)]
pub(crate) struct BubbleHistoryEntry {
    pub(crate) bubbles: Vec<Bubble>,
    pub(crate) hash: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct BubbleLink {
    pub(crate) img_u: f32,
    pub(crate) img_v: f32,
    pub(crate) target_x: f32,
    pub(crate) target_y: f32,
}

#[derive(Clone)]
pub(crate) struct OverlayTextureTile {
    pub(crate) texture: egui::TextureHandle,
    pub(crate) origin_px: [usize; 2],
    pub(crate) size_px: [usize; 2],
}

#[derive(Clone)]
pub(crate) struct OverlayTexturePage {
    pub(crate) size: [usize; 2],
    pub(crate) texture_options: egui::TextureOptions,
    pub(crate) tiles: Vec<OverlayTextureTile>,
}

#[derive(Clone)]
pub(crate) struct OverlayPrepareRequest {
    pub(crate) page_idx: usize,
    pub(crate) job_id: u64,
    pub(crate) image: Arc<egui::ColorImage>,
}

#[derive(Clone)]
pub(crate) struct OverlayPreparedTile {
    pub(crate) tile_idx: usize,
    pub(crate) origin_px: [usize; 2],
    pub(crate) size_px: [usize; 2],
    pub(crate) rgba: Vec<u8>,
}

#[derive(Clone)]
pub(crate) struct OverlayPrepareResult {
    pub(crate) page_idx: usize,
    pub(crate) job_id: u64,
    pub(crate) size: [usize; 2],
    pub(crate) tiles: Vec<OverlayPreparedTile>,
}

#[derive(Clone)]
pub(crate) struct OverlayPreparedPage {
    pub(crate) size: [usize; 2],
    pub(crate) tiles: Vec<OverlayPreparedTile>,
    pub(crate) next_upload_tile: usize,
}

#[derive(Clone)]
pub(crate) struct CanvasSettingsSaveRequest {
    pub(crate) project_settings_file: PathBuf,
    pub(crate) user_settings_file: PathBuf,
    pub(crate) snapshot: crate::models::bubbles_model::SharedCanvasSettings,
}

#[derive(Debug, Clone, Copy)]
pub struct OverlayRectPx {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

pub struct CanvasState {
    pub zoom: f32,
    pub bubble_mode: BubbleMode,
    pub hybrid_editable_bubble_type: BubbleType,
    pub hybrid_readonly_bubble_type: BubbleType,
    pub show_bubbles: bool,
    pub show_bubble_status: bool,
    pub bubble_status_rules: Vec<BubbleStatusRule>,
    pub controls_panel_collapsed: bool,
    pub bubble_opacity: f32,
    pub page_spacing: f32,
    pub separate_pages: bool,
    pub edge_margin: f32,
    pub side_margin: f32,
    pub bubble_min_width: f32,
    pub bubble_max_width: f32,
    pub aside_compact_mode: AsideBubbleCompactMode,
    pub aside_side_mode: AsideBubbleSideMode,
    pub on_top_focus_mode: OnTopFocusMode,
    pub scale_bubbles: bool,
    pub aside_scale_pct: i32,
    pub auto_insert_last_character: bool,
    pub spellcheck_original: bool,
    pub spellcheck_translation: bool,
    pub tabs_autosync_enabled: bool,
    pub cache_pages: bool,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            bubble_mode: BubbleMode::Hybrid,
            hybrid_editable_bubble_type: BubbleType::OnTop,
            hybrid_readonly_bubble_type: BubbleType::Aside,
            show_bubbles: true,
            show_bubble_status: false,
            bubble_status_rules: default_bubble_status_rules(),
            controls_panel_collapsed: false,
            bubble_opacity: 1.0,
            page_spacing: 200.0,
            separate_pages: true,
            edge_margin: 200.0,
            side_margin: 20.0,
            bubble_min_width: 500.0,
            bubble_max_width: 550.0,
            aside_compact_mode: AsideBubbleCompactMode::None,
            aside_side_mode: AsideBubbleSideMode::Auto,
            on_top_focus_mode: OnTopFocusMode::Around,
            scale_bubbles: true,
            aside_scale_pct: 100,
            auto_insert_last_character: true,
            spellcheck_original: false,
            spellcheck_translation: true,
            tabs_autosync_enabled: true,
            cache_pages: true,
        }
    }
}
