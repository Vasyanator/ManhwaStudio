/*
File: crates/ms-text-render/src/wrap/forms/shape_match.rs

Purpose:
Shape predicates over a sequence of line widths (with a width tolerance): mountain,
christmas tree, lens, and the incremental per-preset phase machine the DFS uses to
prune a branch as soon as it can no longer match.

Key functions:
- width_cmp(), is_mountain(), is_christmas_tree(), is_lens(), sequence_matches()
- advance_step(), `PhaseState` (impl)
*/

use super::{PhaseState, Step, TextFormPreset, WidthCmp};

#[must_use]
fn width_cmp(a: u32, b: u32, tol: u32) -> WidthCmp {
    if a > b && a - b > tol {
        WidthCmp::Greater
    } else if b > a && b - a > tol {
        WidthCmp::Less
    } else {
        WidthCmp::Equal
    }
}

/// Унимодальная «гора»: неубывающая, затем невозрастающая (монотонные тоже).
/// Долина (спад, затем подъём) горой не является — это «ёлка».
#[must_use]
pub(super) fn is_mountain(widths: &[u32], tol: u32) -> bool {
    let mut descending = false;
    for pair in widths.windows(2) {
        match width_cmp(pair[1], pair[0], tol) {
            WidthCmp::Greater => {
                if descending {
                    return false;
                }
            }
            WidthCmp::Less => descending = true,
            WidthCmp::Equal => {}
        }
    }
    true
}

/// «Ёлка» — любая последовательность ширин, которая не является горой
/// (то есть имеет внутреннюю долину).
#[must_use]
pub fn is_christmas_tree(widths: &[u32], tol: u32) -> bool {
    !is_mountain(widths, tol)
}

/// Линза «(  )»: гора, у которой пик строго внутри — есть и подъём, и спуск.
#[must_use]
pub(super) fn is_lens(widths: &[u32], tol: u32) -> bool {
    if !is_mountain(widths, tol) {
        return false;
    }
    let mut ascended = false;
    let mut descended = false;
    for pair in widths.windows(2) {
        match width_cmp(pair[1], pair[0], tol) {
            WidthCmp::Greater => ascended = true,
            WidthCmp::Less => descended = true,
            WidthCmp::Equal => {}
        }
    }
    ascended && descended
}

/// Соответствует ли последовательность ширин выбранной форме.
#[must_use]
pub fn sequence_matches(widths: &[u32], preset: TextFormPreset, tol: u32) -> bool {
    match preset {
        TextFormPreset::Widen => widths
            .windows(2)
            .all(|pair| width_cmp(pair[1], pair[0], tol) != WidthCmp::Less),
        TextFormPreset::Narrow => widths
            .windows(2)
            .all(|pair| width_cmp(pair[1], pair[0], tol) != WidthCmp::Greater),
        TextFormPreset::FreeNoTree => is_mountain(widths, tol),
        TextFormPreset::Lens => is_lens(widths, tol),
    }
}

impl PhaseState {
    pub(super) const START: Self = Self {
        last_width: None,
        descending: false,
        ascended: false,
        descended: false,
    };
}

/// Проверяет переход от предыдущей строки к новой шириной `width` (с допуском `tol`).
pub(super) fn advance_step(preset: TextFormPreset, phase: PhaseState, width: u32, tol: u32) -> Step {
    let Some(last) = phase.last_width else {
        return Step::Ok(PhaseState {
            last_width: Some(width),
            descending: false,
            ascended: false,
            descended: false,
        });
    };
    let cmp = width_cmp(width, last, tol);
    match preset {
        TextFormPreset::Widen => match cmp {
            WidthCmp::Less => Step::SkipEnd,
            WidthCmp::Equal | WidthCmp::Greater => Step::Ok(PhaseState {
                last_width: Some(width),
                descending: false,
                ascended: phase.ascended || cmp == WidthCmp::Greater,
                descended: false,
            }),
        },
        TextFormPreset::Narrow => match cmp {
            WidthCmp::Greater => Step::PruneRest,
            WidthCmp::Equal | WidthCmp::Less => Step::Ok(PhaseState {
                last_width: Some(width),
                descending: false,
                ascended: false,
                descended: phase.descended || cmp == WidthCmp::Less,
            }),
        },
        TextFormPreset::FreeNoTree | TextFormPreset::Lens => match cmp {
            WidthCmp::Greater => {
                if phase.descending {
                    // Подъём после спуска = долина (ёлка) — ветка мертва.
                    Step::PruneRest
                } else {
                    Step::Ok(PhaseState {
                        last_width: Some(width),
                        descending: false,
                        ascended: true,
                        descended: phase.descended,
                    })
                }
            }
            WidthCmp::Equal => Step::Ok(PhaseState {
                last_width: Some(width),
                ..phase
            }),
            WidthCmp::Less => {
                // Строка начинает (или продолжает) спуск.
                if !phase.descending && preset == TextFormPreset::Lens && !phase.ascended {
                    // В линзе нельзя начать спуск, ещё не поднявшись — пробуем шире.
                    Step::SkipEnd
                } else {
                    Step::Ok(PhaseState {
                        last_width: Some(width),
                        descending: true,
                        ascended: phase.ascended,
                        descended: true,
                    })
                }
            }
        },
    }
}
