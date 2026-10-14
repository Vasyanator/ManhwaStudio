/*
File: crates/ms-input/src/touch/menu.rs

Purpose:
The single open path of canvas context menus: a mouse/pen secondary click and a finger
long-press (from `TouchOutput::long_press_at`) open the same popup, at the same kind of
anchor, with the same close behaviour.

Key functions:
- `context_menu_popup`: egui's `Popup::context_menu` with the anchor recorded by us.

Notes:
egui's `Popup::context_menu` anchors with `PointerFixed`, which stores the HOVER position
at open through `Memory::open_popup_at` — `pub(crate)` (`egui-0.36.2/src/memory/mod.rs:1112`,
`containers/popup.rs:248-260`, :522-523). A claimed finger has no hover position, so this
helper records the anchor itself in context temp data and uses the public `at_position`.
For a mouse both record the hover position of the opening frame, so the two are identical
(pinned by a characterization test).
*/

use egui::{Pos2, Response};
use egui::{Popup, SetOpenCommand};

/// Builds the canvas context menu for `response`; call `.show(..)` on the result.
///
/// Opens on `response.secondary_clicked()` (anchored at the pointer) or when
/// `long_press_at` is `Some` (anchored there); a primary click on `response` closes it. Pass
/// the router's `TouchOutput::long_press_at` of THIS frame (only when it belongs to this
/// response's area). The popup id is egui's default for `response`
/// (`Popup::default_response_id`), so `Popup::is_id_open` works as for `Popup::context_menu`.
pub fn context_menu_popup(response: &Response, long_press_at: Option<Pos2>) -> Popup<'_> {
    let anchor_id = Popup::default_response_id(response).with("ms_touch_anchor");
    let set_open = if response.secondary_clicked() || long_press_at.is_some() {
        Some(SetOpenCommand::Bool(true))
    } else if response.clicked() {
        // As in `Popup::context_menu`: a click on the widget itself closes the menu.
        Some(SetOpenCommand::Bool(false))
    } else {
        None
    };
    if set_open == Some(SetOpenCommand::Bool(true)) {
        let anchor = long_press_at.or_else(|| response.ctx.pointer_hover_pos());
        response.ctx.data_mut(|data| {
            if let Some(anchor) = anchor {
                data.insert_temp(anchor_id, anchor);
            } else {
                data.remove::<Pos2>(anchor_id);
            }
        });
    }
    let popup = Popup::menu(response).open_memory(set_open);
    match response.ctx.data(|data| data.get_temp::<Pos2>(anchor_id)) {
        Some(anchor) => popup.at_position(anchor),
        // Nothing recorded (never opened through this helper): egui's own behaviour.
        None => popup.at_pointer_fixed(),
    }
}
