/*
FILE HEADER (widgets/wheel_input_guard.rs)
- Назначение: общий guard для wheel-aware UI-виджетов.
- Ключевые сущности:
  - `OpenComboPopupGuard`: temp-состояние egui о том, что на текущем кадре открыт
    popup combobox, и последний известный viewport-rect его списка.
- Ключевые функции:
  - `publish_combo_popup_open`: вызывается combobox-виджетом при открытом popup.
  - `publish_combo_popup_rect`: публикует область выпадающего списка.
  - `combo_popup_open`: проверяет, нужно ли нижележащим wheel-виджетам игнорировать колесо.
  - `combo_popup_blocks_pointer`: проверяет, находится ли курсор над списком popup-а.
- Особенности:
  - guard хранится в `egui::Context::data` только для текущего/соседнего кадра;
  - нужен, чтобы wheel-события popup-слоя не попадали в слайдеры и spinbox'ы,
    которые геометрически находятся под выпадающим списком.
*/

use eframe::egui;
use egui::{Id, Rect};

const OPEN_COMBO_POPUP_GUARD_ID: &str = "wheel_input_open_combo_popup_guard";

#[derive(Clone, Copy, Debug)]
struct OpenComboPopupGuard {
    frame_nr: u64,
    rect: Option<Rect>,
}

pub(super) fn publish_combo_popup_open(ctx: &egui::Context) {
    let frame_nr = ctx.cumulative_frame_nr();
    ctx.data_mut(|data| {
        data.insert_temp(
            Id::new(OPEN_COMBO_POPUP_GUARD_ID),
            OpenComboPopupGuard {
                frame_nr,
                rect: None,
            },
        );
    });
}

pub(super) fn publish_combo_popup_rect(ctx: &egui::Context, rect: Rect) {
    let frame_nr = ctx.cumulative_frame_nr();
    ctx.data_mut(|data| {
        data.insert_temp(
            Id::new(OPEN_COMBO_POPUP_GUARD_ID),
            OpenComboPopupGuard {
                frame_nr,
                rect: Some(rect),
            },
        );
    });
}

pub(super) fn combo_popup_open(ctx: &egui::Context) -> bool {
    let Some(guard) =
        ctx.data(|data| data.get_temp::<OpenComboPopupGuard>(Id::new(OPEN_COMBO_POPUP_GUARD_ID)))
    else {
        return false;
    };

    ctx.cumulative_frame_nr().saturating_sub(guard.frame_nr) <= 1
}

pub(super) fn combo_popup_blocks_pointer(ctx: &egui::Context) -> bool {
    let Some(guard) =
        ctx.data(|data| data.get_temp::<OpenComboPopupGuard>(Id::new(OPEN_COMBO_POPUP_GUARD_ID)))
    else {
        return false;
    };

    if ctx.cumulative_frame_nr().saturating_sub(guard.frame_nr) > 1 {
        return false;
    }
    let Some(rect) = guard.rect else {
        return false;
    };

    ctx.input(|input| {
        input
            .pointer
            .hover_pos()
            .or_else(|| input.pointer.interact_pos())
            .is_some_and(|pos| rect.contains(pos))
    })
}
