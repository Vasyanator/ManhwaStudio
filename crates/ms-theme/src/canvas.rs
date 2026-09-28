/*
File: crates/ms-theme/src/canvas.rs

Purpose:
Colours of the studio's editing chrome drawn OVER content — page images on the canvas, or the
whole viewport for the modal scrim. Brighter and more saturated than the panel status colours in
`status.rs` by design: they must stay readable over an arbitrary page (white paper, black ink,
full-colour art), which egui's widget visuals are not chosen for.

Key constants:
- selection: SELECTION, SELECTION_HOVER
- neutral chrome: NEUTRAL_BORDER, OUTLINE_BACKING
- crop: CROP_FRAME, CROP_HANDLE_STROKE, OUTSIDE_SHADE
- split: CUT_LINE, CUT_HANDLE, CUT_GRIP
- tool state: REFUSED, OCCUPIED, MASK_TINT
- overlays: MODAL_SCRIM

Notes:
Pixel/raster colours baked INTO images and masks (`[u8; 3]` tints, `put_pixel` values) are not
chrome and stay local to their owners.
*/

use egui::Color32;

/// Selected-object accent on the canvas: selected bubble outline, selection handles, the first
/// image-area colour.
pub const SELECTION: Color32 = Color32::from_rgb(0, 120, 215);

/// Hovered state of a [`SELECTION`]-coloured control (lighter, same hue).
pub const SELECTION_HOVER: Color32 = Color32::from_rgb(38, 153, 251);

/// Neutral 1 px border of canvas cards and of the page outline.
pub const NEUTRAL_BORDER: Color32 = Color32::from_gray(90);

/// Dark backing half of a two-tone outline or frame ring; the light/state-coloured half is
/// drawn over it so the outline survives both black ink and white paper.
pub const OUTLINE_BACKING: Color32 = Color32::from_rgb(20, 20, 20);

/// Crop frame stroke and crop handle fill.
pub const CROP_FRAME: Color32 = Color32::from_rgb(255, 190, 60);

/// Stroke around crop handles, separating them from [`CROP_FRAME`]-coloured page content.
pub const CROP_HANDLE_STROKE: Color32 = Color32::from_rgb(40, 30, 10);

/// Veil painted over the part of a page an operation discards (outside the crop frame).
pub const OUTSIDE_SHADE: Color32 = Color32::from_black_alpha(140);

/// Split (cut) line across a page.
pub const CUT_LINE: Color32 = Color32::from_rgb(255, 79, 68);

/// Fill of the draggable handle on a [`CUT_LINE`].
pub const CUT_HANDLE: Color32 = Color32::from_rgb(190, 28, 28);

/// Grip marks drawn on a [`CUT_HANDLE`].
pub const CUT_GRIP: Color32 = Color32::from_rgb(250, 235, 235);

/// "Refused / invalid" tool state on the canvas: a frame whose size the consumer rejects, an
/// outline whose release would be refused, the matching engine error status.
pub const REFUSED: Color32 = Color32::from_rgb(255, 120, 120);

/// "Occupied / ready" tool state on the canvas: a frame holding a mask, a pending result or
/// running work, the matching engine OK status.
pub const OCCUPIED: Color32 = Color32::from_rgb(90, 255, 130);

/// Tint of an editable removal mask shown over the page (and its legend swatch).
pub const MASK_TINT: Color32 = Color32::from_rgb(255, 220, 0);

/// Full-viewport scrim behind a blocking modal or progress veil.
pub const MODAL_SCRIM: Color32 = Color32::from_black_alpha(150);
