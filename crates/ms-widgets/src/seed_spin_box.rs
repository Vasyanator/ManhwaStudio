/*
FILE HEADER (widgets/seed_spin_box.rs)
- Назначение: компактный виджет редактирования seed-значения.
- Ключевые сущности:
  - `SeedSpinBox`: горизонтальный блок из числового `DragValue` и кнопки `Случайный`.
- Ключевые функции:
  - `SeedSpinBox::new`: создаёт виджет для `u64` seed по mutable-ссылке.
  - `random_seed`: генерирует новый seed из системного времени и локального hash-mix.
- Особенности:
  - виджет хранит стабильное значение, пока пользователь не меняет его вручную
    или кнопкой `Случайный`;
  - не зависит от внешних random-crate, чтобы не расширять dependency surface UI.
*/

use eframe::egui;
use std::sync::atomic::{AtomicU64, Ordering};
use web_time::{SystemTime, UNIX_EPOCH};

static SEED_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct SeedSpinBox<'a> {
    value: &'a mut u64,
    prefix: String,
}

impl<'a> SeedSpinBox<'a> {
    pub fn new(value: &'a mut u64) -> Self {
        Self {
            value,
            prefix: String::new(),
        }
    }

    #[inline]
    pub fn prefix(mut self, prefix: impl ToString) -> Self {
        self.prefix = prefix.to_string();
        self
    }

    pub fn draw(self, ui: &mut egui::Ui) -> egui::Response {
        let mut changed = false;
        let inner = ui.horizontal(|ui| {
            let drag = ui.add(
                egui::DragValue::new(self.value)
                    .speed(0.25)
                    .range(0..=u64::MAX)
                    .prefix(self.prefix.as_str()),
            );
            changed |= drag.changed();
            let random_button = ui.button(t!("widgets.seed_spin_box.random"));
            if random_button.clicked() {
                *self.value = random_seed();
                changed = true;
            }
        });
        let mut response = inner.response;
        if changed {
            response.mark_changed();
        }
        response
    }
}

#[must_use]
pub fn random_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let folded = u64::try_from(nanos ^ (nanos >> 64)).unwrap_or(u64::MAX);
    let counter = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    splitmix64(folded ^ counter)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}
