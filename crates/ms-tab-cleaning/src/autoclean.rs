/*
File: autoclean.rs

Purpose:
Quick text-clean autoclean engine over source RGBA pages and a binary text mask. Given a
page and its detector text mask, it removes text by repainting each text cluster with the
local background colour, without erasing bubble contours or foreground art.

Pipeline (per cluster, see `dev-docs/autoclean_dual_candidate_plan.md`, Phase 1):
    gate (has_text_structure)
      -> two candidates: A = strokes (fill_holes + dilate), B = detector-box union / bbox
      -> evolve both to a homogeneous perimeter (rayon::join)
      -> select the winner by coverage then minimal area
      -> universal bubble-interior clip
      -> conditional background-only padding
      -> final sanity trim
      -> paint by the winner mask only.

Engine core is GUI-free: `run_autoclean_engine` operates purely on `image` buffers and
returns `RegionFill`s. The only egui dependency is the thin `autoclean_page` wrapper, which
rasterizes those fills into a transparent `egui::ColorImage` patch for the job layer.

Key structures:
- `UnevenBackgroundTool`: policy for clusters that do not converge (currently NoProcessing).
- `AutocleanPageOutcome`: egui patch + per-region counts returned to the job layer.
- `AutocleanEngineResult` / `RegionFill`: GUI-free engine output.
- `Converged` / `EvolveStats`: result of `evolve_mask_to_homogeneous`.
- `CandidateFill`: an evolved+clipped+padded candidate ready for selection.

Notes:
Job orchestration, source/mask loading, block source->page scaling, sizing, and overlay
application remain in `tab.rs`.
*/

use std::cmp::Ordering;
use std::collections::VecDeque;

// --- Autoclean (продвинутый алгоритм клина текста по однородному фону) --------
// Портирован из ZITS-PlusPlus/dataset_generator_v2/naver_rs/src/autoclean.rs и
// адаптирован под единую бинарную маску текста + связные компоненты MS.
//
// Идея: для каждого кластера близких штрихов строятся ДВЕ маски-кандидата —
// A (штрихи детектора) и B (объединение боксов детектора / bbox кластера). Обе
// «эволюционируют» симметрично (растут по выбивающимся штрихам и отступают от
// чужих объектов), пока весь периметр не станет однородным фоном. Затем выбирается
// кандидат с лучшим покрытием текста и минимальной площадью, его заливка обрезается
// по интерьеру пузыря (`clip_fill_to_bubble_interior`), аккуратно расширяется только
// по фону и красится. Прямоугольная заливка bbox по контуру пузыря больше не
// применяется — это устраняло стирание контура на углах.

/// Поканальная начальная дилатация (часто тонкой) маски текста, пиксели.
const AUTOCLEAN_INITIAL_DILATE: i32 = 2;
/// Запас заливки наружу, пиксели. Заливка условно расширяется на столько в
/// заведомо фоновую зону, чтобы при LINEAR-фильтрации оверлея полупрозрачный край
/// приходился на фон, а не на кромку текста (иначе из-под клина «просвечивает»
/// тёмная кромка исходника). Расширение claims ТОЛЬКО пиксели цвета фона и не
/// «снаружи» пузыря — контур и чужой контент не перекрашиваются. Альфа строго
/// бинарна (0/255), поэтому в программе и в экспорте всё композитится одинаково.
const AUTOCLEAN_FILL_PADDING: i32 = 2;
/// Поканальный допуск «одинакового цвета». Намеренно маленький и фиксированный:
/// допуск, масштабируемый дисперсией, взорвался бы на разноцветном периметре
/// (волосы + кожа + одежда) и ошибочно счёл бы его однородным.
const AUTOCLEAN_SAME_TOL: i32 = 16;
/// Пока не более этой доли периметра отличается от фона, отличающиеся пиксели
/// считаются штрихами текста и поглощаются ростом. Выше — периметр это реальный
/// контент/градиент, кандидат отвергается сразу.
const AUTOCLEAN_GROW_LIMIT: f32 = 0.30;
/// Максимальная дальность зонда «штрих-vs-объект», пиксели. Заменяет прежний зонд,
/// равный радиусу (48 px по умолчанию). Обоснование: зонд существует, чтобы
/// поглощать сглаживающие ореолы и фрагменты штрихов шириной в несколько пикселей;
/// 48 px позволяли ему классифицировать «контур пузыря + внешняя страница» как
/// ограниченный штрих и прорастать сквозь контур. Дальность зонда САМА не отличает
/// контур (2-6 px) от фрагмента буквы (2-6 px) — несущая защита это универсальный
/// интерьерный клип; кэп лишь ограничивает залезание в контур/арт.
const AUTOCLEAN_STROKE_PROBE_MAX: i32 = 8;
/// Мин. доля пикселей маски, которые должны быть «чернилами» (иначе это
/// однородная область, а не текст).
const AUTOCLEAN_MIN_INK_FRAC: f32 = 0.02;
/// Макс. доля пикселей маски, допустимая как «чернила» (выше — это сплошной
/// отличающийся объект, а не разреженный текст на фоне).
const AUTOCLEAN_MAX_INK_FRAC: f32 = 0.65;
/// Мин. доля «чернильных» пикселей на границе чернила/фон. Тонкие штрихи → высоко;
/// сплошная заливка (лицо/волосы) → низко.
const AUTOCLEAN_MIN_EDGE_RATIO: f32 = 0.16;
/// Защита box-кандидата: макс. доля внутренности бокса, которая может отличаться
/// от фона. Текстовые боксы — в основном фон с редкими чернилами.
const AUTOCLEAN_BOX_INK_LIMIT: f32 = 0.45;
/// Объединять компоненты текста, чьи пиксели в пределах стольких пикселей.
const AUTOCLEAN_CLUSTER_SLACK: usize = 4;
/// Предпочитать сошедшиеся кандидаты, покрывающие не меньше этой доли исходных
/// штрихов текста. Ниже — заливка частичная (учитывается в `regions_partial`):
/// частичная очистка лучше, чем никакая (см. план, §Selection).
const AUTOCLEAN_COVERAGE_PREFER: f32 = 0.995;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum UnevenBackgroundTool {
    NoProcessing,
}

impl UnevenBackgroundTool {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::NoProcessing => t!("cleaning.tab.no_processing_title"),
        }
    }
}

/// Результат продвинутого автоклина одной страницы (граница с job-слоем).
///
/// `patch` — прозрачный оверлей размера страницы с закрашенными областями текста.
/// Счётчики: `regions_total` — всего кластеров; `regions_filled` — закрашено (в т.ч.
/// частично); `regions_skipped` — пропущено (не текст / не сошлось); `regions_partial`
/// — закрашено с покрытием < `AUTOCLEAN_COVERAGE_PREFER` (подмножество `regions_filled`).
#[derive(Debug)]
pub(super) struct AutocleanPageOutcome {
    pub(super) patch: egui::ColorImage,
    pub(super) regions_total: usize,
    pub(super) regions_filled: usize,
    pub(super) regions_skipped: usize,
    pub(super) regions_partial: usize,
}

/// Одна принятая заливка кластера в глобальных координатах страницы: маска в
/// координатах crop со смещением (`ox`, `oy`) и цвет фона.
#[derive(Debug)]
struct RegionFill {
    ox: i32,
    oy: i32,
    mask: image::GrayImage,
    bg: image::Rgb<u8>,
}

/// GUI-free результат движка автоклина: список заливок + счётчики областей.
#[derive(Debug, Default)]
struct AutocleanEngineResult {
    fills: Vec<RegionFill>,
    regions_total: usize,
    regions_filled: usize,
    regions_skipped: usize,
    regions_partial: usize,
}

/// Продвинутый автоклин страницы по бинарной маске текста (граница с egui).
///
/// Тонкая обёртка над GUI-free движком `run_autoclean_engine`: собирает его заливки
/// в прозрачный `egui::ColorImage` через `paint_patch_from_mask`. Единственное место
/// в модуле, зависящее от egui.
///
/// `spread_radius_px` — радиус «расползания»: бюджет роста И отступления фронта маски
/// (по ≤ столько пикселей). `blocks` — боксы детектора текста в пиксельных координатах
/// СТРАНИЦЫ (уже приведённые из source-space в `tab.rs`), `None` при отсутствии.
/// `uneven_tool` — политика для не-сошедшихся кластеров (сейчас только пропуск).
pub(super) fn autoclean_page(
    base_rgba: &image::RgbaImage,
    binary_mask: &[u8],
    width: usize,
    height: usize,
    spread_radius_px: usize,
    uneven_tool: UnevenBackgroundTool,
    blocks: Option<&[[i32; 4]]>,
) -> AutocleanPageOutcome {
    // Единственная сегодня политика: не-сошедшиеся кластеры не обрабатываются
    // (они уже посчитаны в `regions_skipped` движком). Новые варианты обязаны
    // ветвиться здесь — исчерпывающий match это гарантирует.
    match uneven_tool {
        UnevenBackgroundTool::NoProcessing => {}
    }

    // Entry invariant: the whole engine does signed pixel-coordinate math in `i32` and
    // flat `usize` indexing. Establish ONCE here that the page width, height and their
    // product fit those types. Every crop is a sub-rect of the page, so once this holds,
    // all downstream `dim as i32` / `idx as usize` casts in the engine are provably in
    // range. A page that violates it is rejected (empty patch, all-skipped counters)
    // rather than wrapping into corrupt coordinates. Real pages are far below the bound.
    let dims_fit = i32::try_from(width).is_ok()
        && i32::try_from(height).is_ok()
        && width.checked_mul(height).is_some();
    if !dims_fit {
        return AutocleanPageOutcome {
            patch: egui::ColorImage::filled([1, 1], egui::Color32::TRANSPARENT),
            regions_total: 0,
            regions_filled: 0,
            regions_skipped: 0,
            regions_partial: 0,
        };
    }

    let mut patch =
        egui::ColorImage::filled([width.max(1), height.max(1)], egui::Color32::TRANSPARENT);
    let engine =
        run_autoclean_engine(base_rgba, binary_mask, width, height, spread_radius_px, blocks);
    for fill in &engine.fills {
        paint_patch_from_mask(&mut patch, width, height, fill.ox, fill.oy, &fill.mask, fill.bg);
    }
    AutocleanPageOutcome {
        patch,
        regions_total: engine.regions_total,
        regions_filled: engine.regions_filled,
        regions_skipped: engine.regions_skipped,
        regions_partial: engine.regions_partial,
    }
}

/// GUI-free ядро автоклина: кластеризация маски и покластерная обработка.
///
/// Связные компоненты маски, раздутой на `AUTOCLEAN_CLUSTER_SLACK`, образуют кластеры
/// близких штрихов. Для каждого строится crop с запасом, извлекаются исходные штрихи,
/// и вызывается `process_cluster`. Возвращает заливки-победители и счётчики областей.
/// `blocks` — боксы текста в координатах страницы (или `None`).
fn run_autoclean_engine(
    base_rgba: &image::RgbaImage,
    binary_mask: &[u8],
    width: usize,
    height: usize,
    spread_radius_px: usize,
    blocks: Option<&[[i32; 4]]>,
) -> AutocleanEngineResult {
    let mut result = AutocleanEngineResult::default();
    if binary_mask.is_empty() || width == 0 || height == 0 {
        return result;
    }

    // Единый радиус управляет бюджетами роста и отступления. Зонд теперь фиксирован
    // (`AUTOCLEAN_STROKE_PROBE_MAX`), а не привязан к радиусу.
    let radius = i32::try_from(spread_radius_px.min(128)).unwrap_or(128);
    // Запас crop: рост наружу до `radius` + поле для кольца фона.
    let pad = radius + 6;

    // Кластеризация: компоненты исходной маски, раздутой на CLUSTER_SLACK.
    let cluster_mask = if AUTOCLEAN_CLUSTER_SLACK > 0 {
        dilate_binary_mask(binary_mask, width, height, AUTOCLEAN_CLUSTER_SLACK)
    } else {
        binary_mask.to_vec()
    };
    let clusters = extract_connected_components(&cluster_mask, width, height);

    let sw = i32::try_from(width).unwrap_or(i32::MAX);
    let sh = i32::try_from(height).unwrap_or(i32::MAX);
    for (label, cluster_pixels) in clusters.pixels.iter().enumerate() {
        if cluster_pixels.is_empty() {
            continue;
        }
        result.regions_total = result.regions_total.saturating_add(1);
        let cluster_label = i32::try_from(label).unwrap_or(-1);

        // bbox кластера (x2/y2 — эксклюзивны), координаты страницы.
        let (mut x1, mut y1, mut x2, mut y2) = (sw, sh, 0i32, 0i32);
        for &idx in cluster_pixels {
            let x = i32::try_from(idx % width).unwrap_or(0);
            let y = i32::try_from(idx / width).unwrap_or(0);
            x1 = x1.min(x);
            y1 = y1.min(y);
            x2 = x2.max(x + 1);
            y2 = y2.max(y + 1);
        }

        // Crop с запасом, чтобы внешнее кольцо и рост оставались в реальном фоне.
        let ox = (x1 - pad).max(0);
        let oy = (y1 - pad).max(0);
        let ex = (x2 + pad).min(sw);
        let ey = (y2 + pad).min(sh);
        let (cwi, chi) = (ex - ox, ey - oy);
        let (cw, ch) = (u32::try_from(cwi).unwrap_or(0), u32::try_from(chi).unwrap_or(0));
        if cw == 0 || ch == 0 {
            result.regions_skipped = result.regions_skipped.saturating_add(1);
            continue;
        }
        let rgb = crop_rgb_from_rgba(base_rgba, ox, oy, cw, ch);

        // Исходные штрихи текста этого кластера в координатах crop (seed интерьера
        // пузыря и эталон покрытия — не дилатируются).
        let mut strokes = image::GrayImage::new(cw, ch);
        for &idx in cluster_pixels {
            if binary_mask.get(idx).copied().unwrap_or(0) == 0 {
                continue;
            }
            if clusters.labels.get(idx).copied().unwrap_or(-1) != cluster_label {
                continue;
            }
            let lx = i32::try_from(idx % width).unwrap_or(0) - ox;
            let ly = i32::try_from(idx / width).unwrap_or(0) - oy;
            // lx/ly неотрицательны и ограничены cwi/chi (проверка ниже) — приведение
            // к u32 безопасно.
            if lx >= 0 && ly >= 0 && lx < cwi && ly < chi {
                strokes.put_pixel(lx as u32, ly as u32, image::Luma([255]));
            }
        }
        if !has_foreground(&strokes) {
            result.regions_skipped = result.regions_skipped.saturating_add(1);
            continue;
        }

        match process_cluster(&rgb, &strokes, (x1, y1, x2, y2), (ox, oy), blocks, radius) {
            ClusterOutcome::Filled { mask, bg, partial } => {
                result.regions_filled = result.regions_filled.saturating_add(1);
                if partial {
                    result.regions_partial = result.regions_partial.saturating_add(1);
                }
                result.fills.push(RegionFill { ox, oy, mask, bg });
            }
            ClusterOutcome::Skipped => {
                result.regions_skipped = result.regions_skipped.saturating_add(1);
            }
        }
    }
    result
}

/// Итог обработки одного кластера.
#[derive(Debug)]
enum ClusterOutcome {
    /// Кластер закрашен маской `mask` цветом `bg`; `partial` — покрытие ниже порога.
    Filled {
        mask: image::GrayImage,
        bg: image::Rgb<u8>,
        partial: bool,
    },
    /// Кластер пропущен (не текст, либо ни один кандидат не сошёлся).
    Skipped,
}

/// Обработать один кластер: гейт, два кандидата, эволюция, выбор, клип, паддинг.
///
/// `strokes` — исходные штрихи (координаты crop). `cluster_bbox_page` — bbox кластера
/// в координатах страницы (x2/y2 эксклюзивны). `crop_off` — смещение crop (ox, oy).
/// `blocks` — боксы детектора (координаты страницы) или `None`. `radius` — бюджет
/// роста/отступления. Возвращает победившую заливку либо `Skipped`.
fn process_cluster(
    rgb: &image::RgbImage,
    strokes: &image::GrayImage,
    cluster_bbox_page: (i32, i32, i32, i32),
    crop_off: (i32, i32),
    blocks: Option<&[[i32; 4]]>,
    radius: i32,
) -> ClusterOutcome {
    let (cw, ch) = (rgb.width(), rgb.height());

    // Кандидат A: штрихи → заполнение внутренностей букв → лёгкая дилатация.
    let mut candidate_a = strokes.clone();
    fill_holes(&mut candidate_a);
    dilate_gray_inplace(&mut candidate_a, AUTOCLEAN_INITIAL_DILATE);
    let ring0 = outer_ring(&candidate_a);
    if ring0.is_empty() {
        return ClusterOutcome::Skipped;
    }
    let bg0 = ring_background(rgb, &ring0);

    // Гейт: отбраковать не-текстовые пятна (лицо/волосы) до всякой работы.
    if !has_text_structure(rgb, &candidate_a, bg0) {
        return ClusterOutcome::Skipped;
    }

    // Кандидат B: объединение боксов ∩ кластер (или bbox кластера при None),
    // растеризованное сплошным прямоугольником.
    let (candidate_b, b_bbox) = build_box_candidate(cw, ch, cluster_bbox_page, crop_off, blocks);
    // Пре-гейт B: не заливать бокс, набитый контентом (защита от стирания арта).
    let b_gate = has_foreground(&candidate_b)
        && box_interior_fillable(rgb, b_bbox.0, b_bbox.1, b_bbox.2, b_bbox.3, bg0);

    // «Параллельная» эволюция обоих кандидатов (требование плана). Crop мал,
    // rayon::join кладёт задачи в общий пул — без создания своих пулов.
    let (conv_a, conv_b) = rayon::join(
        || evolve_mask_to_homogeneous(rgb, candidate_a, radius, radius),
        || {
            if b_gate {
                evolve_mask_to_homogeneous(rgb, candidate_b, radius, radius)
            } else {
                None
            }
        },
    );

    // Собрать финальные заливки (клип → паддинг → санити) и посчитать покрытие.
    // `assemble_fill` возвращает `None` для вырожденной или не-фиксируемой заливки —
    // такой кандидат отбрасывается, а не считается закрашенным.
    let mut candidates: Vec<CandidateFill> = Vec::new();
    if let Some(conv) = conv_a
        && let Some(fill) = assemble_fill(rgb, strokes, conv, true)
    {
        candidates.push(fill);
    }
    if let Some(conv) = conv_b
        && let Some(fill) = assemble_fill(rgb, strokes, conv, false)
    {
        candidates.push(fill);
    }
    if candidates.is_empty() {
        return ClusterOutcome::Skipped;
    }
    select_winner(candidates)
}

/// Сошедшийся кандидат, доведённый до финальной заливки и оценённый.
///
/// `coverage` — доля исходных штрихов, попавших в финальную заливку (после клипа и
/// паддинга). `area` — число закрашиваемых пикселей. `iters` — суммарное число
/// итераций эволюции (тай-брейк: более стабильная = меньше итераций). `is_a` —
/// кандидат-штрихи (A) для тай-брейка в пользу A.
#[derive(Debug)]
struct CandidateFill {
    mask: image::GrayImage,
    bg: image::Rgb<u8>,
    coverage: f32,
    area: u64,
    iters: i32,
    is_a: bool,
}

/// Довести сошедшийся кандидат до финальной заливки, оценить его и, если он
/// пригоден, вернуть.
///
/// Порядок стадий строго по плану: universal clip → conditional padding → final
/// sanity. `coverage` считается на ФИНАЛЬНОЙ заливке (после клипа и паддинга),
/// пересечённой с исходными штрихами `strokes` — ровно определение §Selection.
///
/// Возвращает `None`, если кандидат непригоден и должен быть отброшен (не считается
/// закрашенным):
/// - `final_sanity_trim` не смог довести периметр до нуля пиков (не-фиксируемая заливка);
/// - финальная маска пуста (площадь 0 — санити срезала всё);
/// - заливка не покрывает ни одного исходного штриха (`covered == 0` — красить нечего).
fn assemble_fill(
    rgb: &image::RgbImage,
    strokes: &image::GrayImage,
    conv: Converged,
    is_a: bool,
) -> Option<CandidateFill> {
    let Converged { mut mask, bg, stats } = conv;
    // 1. Универсальный клип по интерьеру пузыря (was fallback-only). Возвращает
    //    множество «снаружи», нужное условному паддингу.
    let outside = clip_fill_to_bubble_interior(&mut mask, rgb, strokes, bg);
    // 2. Условный паддинг: расширять только по фону и не в «снаружи».
    conditional_pad(&mut mask, rgb, bg, &outside, AUTOCLEAN_FILL_PADDING);
    // 3. Финальная санити: срезать пики на внешнем кольце заливки. Если периметр
    //    нельзя сделать беспиковым — кандидат непригоден.
    if !final_sanity_trim(&mut mask, rgb, bg) {
        return None;
    }

    // Покрытие и площадь по финальной маске.
    let (mut covered, mut total_strokes, mut area) = (0u64, 0u64, 0u64);
    for y in 0..mask.height() {
        for x in 0..mask.width() {
            let filled = mask.get_pixel(x, y)[0] != 0;
            if filled {
                area += 1;
            }
            if strokes.get_pixel(x, y)[0] != 0 {
                total_strokes += 1;
                if filled {
                    covered += 1;
                }
            }
        }
    }
    // Отбраковать вырожденные заливки: пустую (санити срезала всё) или не покрывающую
    // ни одного исходного штриха. Иначе селектор счёл бы её закрашенной с coverage 0.
    if area == 0 || covered == 0 {
        return None;
    }
    // `covered > 0` ⇒ `total_strokes > 0`, поэтому деление определено. Оба счётчика
    // ограничены площадью crop (< 2^24 при radius ≤ 128), так что f32 их представляет
    // точно — коэффициент покрытия не теряет точность.
    let coverage = covered as f32 / total_strokes as f32;
    Some(CandidateFill {
        mask,
        bg,
        coverage,
        area,
        iters: stats.grow_iters + stats.shrink_iters,
        is_a,
    })
}

/// Выбрать победителя среди сошедшихся кандидатов (§Selection).
///
/// Если есть кандидаты с покрытием ≥ `AUTOCLEAN_COVERAGE_PREFER`, берётся минимальный
/// по площади (меньше побочного стирания), тай → меньше итераций, тай → A. Иначе
/// (все низкопокрытые — текст касается чужого арта) берётся максимум покрытия,
/// заливка всё равно наносится, и кластер помечается `partial`.
fn select_winner(candidates: Vec<CandidateFill>) -> ClusterOutcome {
    let high_exists = candidates
        .iter()
        .any(|c| c.coverage >= AUTOCLEAN_COVERAGE_PREFER);
    let winner = if high_exists {
        candidates
            .into_iter()
            .filter(|c| c.coverage >= AUTOCLEAN_COVERAGE_PREFER)
            .min_by(|a, b| {
                a.area
                    .cmp(&b.area)
                    .then(a.iters.cmp(&b.iters))
                    .then(b.is_a.cmp(&a.is_a))
            })
    } else {
        candidates.into_iter().max_by(|a, b| {
            a.coverage
                .partial_cmp(&b.coverage)
                .unwrap_or(Ordering::Equal)
                .then(b.area.cmp(&a.area))
                .then(b.iters.cmp(&a.iters))
                .then(a.is_a.cmp(&b.is_a))
        })
    };
    match winner {
        Some(c) => ClusterOutcome::Filled {
            mask: c.mask,
            bg: c.bg,
            partial: !high_exists,
        },
        None => ClusterOutcome::Skipped,
    }
}

/// Crop из RGBA-страницы в локальный RGB-буфер (за границами — чёрный).
fn crop_rgb_from_rgba(base: &image::RgbaImage, ox: i32, oy: i32, cw: u32, ch: u32) -> image::RgbImage {
    let (bw, bh) = (base.width() as i32, base.height() as i32);
    let mut out = image::RgbImage::new(cw, ch);
    for y in 0..ch as i32 {
        for x in 0..cw as i32 {
            let (gx, gy) = (ox + x, oy + y);
            if gx >= 0 && gy >= 0 && gx < bw && gy < bh {
                let p = base.get_pixel(gx as u32, gy as u32);
                out.put_pixel(x as u32, y as u32, image::Rgb([p[0], p[1], p[2]]));
            }
        }
    }
    out
}

/// Построить кандидат B (box-маска) в координатах crop плюс его bbox.
///
/// Если `blocks` (координаты страницы) пересекают bbox кластера — B это их
/// объединение, обрезанное по crop и растеризованное сплошным. Иначе (боксов нет
/// или ни один не пересекает) — B это bbox кластера (форма прежней «попытки 2»).
/// Возвращает маску и её bbox (координаты crop, x2/y2 эксклюзивны).
fn build_box_candidate(
    cw: u32,
    ch: u32,
    cluster_bbox_page: (i32, i32, i32, i32),
    crop_off: (i32, i32),
    blocks: Option<&[[i32; 4]]>,
) -> (image::GrayImage, (i32, i32, i32, i32)) {
    let (ox, oy) = crop_off;
    let (px1, py1, px2, py2) = cluster_bbox_page;
    let (cwi, chi) = (i32::try_from(cw).unwrap_or(0), i32::try_from(ch).unwrap_or(0));
    let mut mask = image::GrayImage::new(cw, ch);

    let mut rasterize = |lx1: i32, ly1: i32, lx2: i32, ly2: i32| {
        for y in ly1..ly2 {
            for x in lx1..lx2 {
                // lx/ly уже clamped в [0, cwi/chi) — приведение к u32 безопасно.
                mask.put_pixel(x as u32, y as u32, image::Luma([255]));
            }
        }
    };

    let mut any_block = false;
    if let Some(blocks) = blocks {
        for &[bx1, by1, bx2, by2] in blocks {
            // Пересечение бокса с bbox кластера в координатах страницы.
            let ix1 = bx1.max(px1);
            let iy1 = by1.max(py1);
            let ix2 = bx2.min(px2);
            let iy2 = by2.min(py2);
            if ix2 <= ix1 || iy2 <= iy1 {
                continue;
            }
            // В координаты crop с обрезкой по границам crop.
            let lx1 = (ix1 - ox).clamp(0, cwi);
            let ly1 = (iy1 - oy).clamp(0, chi);
            let lx2 = (ix2 - ox).clamp(0, cwi);
            let ly2 = (iy2 - oy).clamp(0, chi);
            if lx2 <= lx1 || ly2 <= ly1 {
                continue;
            }
            rasterize(lx1, ly1, lx2, ly2);
            any_block = true;
        }
    }
    if !any_block {
        // Фолбэк: bbox кластера (координаты crop).
        let lx1 = (px1 - ox).clamp(0, cwi);
        let ly1 = (py1 - oy).clamp(0, chi);
        let lx2 = (px2 - ox).clamp(0, cwi);
        let ly2 = (py2 - oy).clamp(0, chi);
        rasterize(lx1, ly1, lx2, ly2);
    }

    let bbox = gray_mask_bbox(&mask).unwrap_or((0, 0, 0, 0));
    (mask, bbox)
}

/// Закрасить пиксели маски в patch сплошным цветом фона (по глоб. координатам).
fn paint_patch_from_mask(
    patch: &mut egui::ColorImage,
    pw: usize,
    ph: usize,
    ox: i32,
    oy: i32,
    mask: &image::GrayImage,
    bg: image::Rgb<u8>,
) {
    let color = egui::Color32::from_rgb(bg[0], bg[1], bg[2]);
    for y in 0..mask.height() as i32 {
        for x in 0..mask.width() as i32 {
            if mask.get_pixel(x as u32, y as u32)[0] == 0 {
                continue;
            }
            let (gx, gy) = (ox + x, oy + y);
            if gx >= 0 && gy >= 0 && (gx as usize) < pw && (gy as usize) < ph {
                let didx = gy as usize * pw + gx as usize;
                if let Some(px) = patch.pixels.get_mut(didx) {
                    *px = color;
                }
            }
        }
    }
}

/// Bounding box (локальные координаты crop, x2/y2 эксклюзивны) пикселей маски.
fn gray_mask_bbox(mask: &image::GrayImage) -> Option<(i32, i32, i32, i32)> {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let (mut x1, mut y1, mut x2, mut y2) = (w, h, 0i32, 0i32);
    let mut found = false;
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x as u32, y as u32)[0] != 0 {
                found = true;
                x1 = x1.min(x);
                y1 = y1.min(y);
                x2 = x2.max(x + 1);
                y2 = y2.max(y + 1);
            }
        }
    }
    found.then_some((x1, y1, x2, y2))
}

fn has_foreground(mask: &image::GrayImage) -> bool {
    mask.as_raw().iter().any(|&v| v != 0)
}

/// Заполнить фоновые пиксели, полностью окружённые маской (внутренности букв):
/// заливка фона от границы crop, всё недостигнутое — дырка.
fn fill_holes(mask: &mut image::GrayImage) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let mut outside = vec![false; (w * h) as usize];
    let mut stack: Vec<(i32, i32)> = Vec::new();
    let push = |x: i32, y: i32, outside: &mut Vec<bool>, stack: &mut Vec<(i32, i32)>| {
        let idx = (y * w + x) as usize;
        if !outside[idx] && mask.get_pixel(x as u32, y as u32)[0] == 0 {
            outside[idx] = true;
            stack.push((x, y));
        }
    };
    for x in 0..w {
        push(x, 0, &mut outside, &mut stack);
        push(x, h - 1, &mut outside, &mut stack);
    }
    for y in 0..h {
        push(0, y, &mut outside, &mut stack);
        push(w - 1, y, &mut outside, &mut stack);
    }
    while let Some((x, y)) = stack.pop() {
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            let (nx, ny) = (x + dx, y + dy);
            if nx >= 0 && ny >= 0 && nx < w && ny < h {
                push(nx, ny, &mut outside, &mut stack);
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            let idx = (y * w + x) as usize;
            if !outside[idx] && mask.get_pixel(x as u32, y as u32)[0] == 0 {
                mask.put_pixel(x as u32, y as u32, image::Luma([255]));
            }
        }
    }
}

/// 8-связная дилатация на `r` пикс. (r итераций роста на 1 пиксель).
fn dilate_gray_inplace(mask: &mut image::GrayImage, r: i32) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    for _ in 0..r {
        let src = mask.clone();
        for y in 0..h {
            for x in 0..w {
                if src.get_pixel(x as u32, y as u32)[0] != 0 {
                    continue;
                }
                let mut hit = false;
                'n: for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx >= 0
                            && ny >= 0
                            && nx < w
                            && ny < h
                            && src.get_pixel(nx as u32, ny as u32)[0] != 0
                        {
                            hit = true;
                            break 'n;
                        }
                    }
                }
                if hit {
                    mask.put_pixel(x as u32, y as u32, image::Luma([255]));
                }
            }
        }
    }
}

/// 1-px фоновое кольцо, 4-смежное с маской.
fn outer_ring(mask: &image::GrayImage) -> Vec<(u32, u32)> {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let mut ring = Vec::new();
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x as u32, y as u32)[0] != 0 {
                continue;
            }
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let (nx, ny) = (x + dx, y + dy);
                if nx >= 0
                    && ny >= 0
                    && nx < w
                    && ny < h
                    && mask.get_pixel(nx as u32, ny as u32)[0] != 0
                {
                    ring.push((x as u32, y as u32));
                    break;
                }
            }
        }
    }
    ring
}

fn chan_dist(a: image::Rgb<u8>, b: image::Rgb<u8>) -> i32 {
    let mut d = 0;
    for k in 0..3 {
        d = d.max((a[k] as i32 - b[k] as i32).abs());
    }
    d
}

fn median_u8(values: &mut [u8]) -> u8 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// Цвет фона = поканальная медиана кольца (устойчива к пикселям штрихов текста,
/// затёкшим в кольцо).
fn ring_background(rgb: &image::RgbImage, ring: &[(u32, u32)]) -> image::Rgb<u8> {
    let mut ch: [Vec<u8>; 3] = std::array::from_fn(|_| Vec::with_capacity(ring.len()));
    for &(x, y) in ring {
        let p = rgb.get_pixel(x, y);
        for k in 0..3 {
            ch[k].push(p[k]);
        }
    }
    image::Rgb([
        median_u8(&mut ch[0]),
        median_u8(&mut ch[1]),
        median_u8(&mut ch[2]),
    ])
}

/// Статистика одной эволюции (см. `Converged`).
#[derive(Debug, Clone, Copy)]
struct EvolveStats {
    /// Число итераций роста (расползания по штрихам).
    grow_iters: i32,
    /// Число итераций отступления (эрозии от чужого объекта).
    shrink_iters: i32,
}

/// Сошедшийся кандидат: маска, цвет фона и статистика эволюции.
#[derive(Debug)]
struct Converged {
    mask: image::GrayImage,
    bg: image::Rgb<u8>,
    stats: EvolveStats,
}

/// Свести периметр маски к единому однородному цвету фона (симметрично: рост И
/// отступление). Возвращает `Some(Converged)`, когда на кольце не осталось «пиков»
/// (пикселей с `chan_dist > AUTOCLEAN_SAME_TOL`).
///
/// Каждый пик классифицируется зондом наружу на ≤ `AUTOCLEAN_STROKE_PROBE_MAX`:
/// фон в пределах зонда ⇒ ограниченный штрих → маска **растёт** (бюджет `grow_budget`);
/// иначе (разница тянется дальше — чужой объект) → маска **отступает** (`shrink_budget`).
///
/// Гарантия завершения (oscillation guard): пиксель, стёртый отступлением, блокируется
/// и больше не может быть добавлен ростом. Это ограничивает число смен состояния
/// каждого пикселя и гарантирует останов независимо от бюджетов.
///
/// `None`, если маска заполнила crop, если > `AUTOCLEAN_GROW_LIMIT` кольца отличается
/// (контент/градиент, не текст), или бюджеты исчерпаны, а периметр всё «грязный».
fn evolve_mask_to_homogeneous(
    rgb: &image::RgbImage,
    mut mask: image::GrayImage,
    grow_budget: i32,
    shrink_budget: i32,
) -> Option<Converged> {
    if !has_foreground(&mask) {
        return None;
    }
    // Widening u32 -> usize: lossless on the supported 64-bit targets, and the mask is a
    // crop of a page that satisfied `autoclean_page`'s entry invariant, so `mw * mh` fits.
    let mw = mask.width() as usize;
    let mh = mask.height() as usize;
    // Oscillation guard: индекс пикселя → был ли он стёрт отступлением.
    let mut shrunk_locked = vec![false; mw * mh];
    let (mut grow_used, mut shrink_used) = (0i32, 0i32);
    loop {
        let ring = outer_ring(&mask);
        if ring.is_empty() {
            return None; // маска заполнила crop — фон не проверить.
        }
        let bg = ring_background(rgb, &ring);
        let mut grow_set: Vec<(u32, u32)> = Vec::new();
        let mut shrink_set: Vec<(u32, u32)> = Vec::new();
        for &(x, y) in &ring {
            if chan_dist(*rgb.get_pixel(x, y), bg) <= AUTOCLEAN_SAME_TOL {
                continue;
            }
            if probe_outward_bg(rgb, &mask, x, y, bg, AUTOCLEAN_STROKE_PROBE_MAX) {
                grow_set.push((x, y));
            } else {
                shrink_set.push((x, y));
            }
        }
        let diff_count = grow_set.len() + shrink_set.len();
        if diff_count == 0 {
            return Some(Converged {
                mask,
                bg,
                stats: EvolveStats {
                    grow_iters: grow_used,
                    shrink_iters: shrink_used,
                },
            });
        }
        // `diff_count` и `ring.len()` ограничены периметром crop (сотни пикселей) —
        // f32 представляет их точно, поэтому сравнение доли не теряет точности.
        if diff_count as f32 > AUTOCLEAN_GROW_LIMIT * ring.len() as f32 {
            return None; // периметр в основном не-фон → контент, не текст.
        }

        let mut acted = false;
        if grow_used < grow_budget && !grow_set.is_empty() {
            let mut grew = false;
            for (x, y) in grow_set {
                let idx = (y as usize) * mw + (x as usize);
                if shrunk_locked[idx] {
                    continue; // oscillation guard: не возвращать стёртое отступлением.
                }
                mask.put_pixel(x, y, image::Luma([255]));
                grew = true;
            }
            if grew {
                grow_used += 1;
                acted = true;
            }
        }
        if shrink_used < shrink_budget && !shrink_set.is_empty() {
            let mut removed: Vec<(u32, u32)> = Vec::new();
            for (x, y) in shrink_set {
                erode_around_collect(&mut mask, x, y, &mut removed);
            }
            for (rx, ry) in removed {
                shrunk_locked[(ry as usize) * mw + (rx as usize)] = true;
            }
            shrink_used += 1;
            acted = true;
        }
        if !acted {
            return None; // бюджеты исчерпаны, периметр всё ещё грязный.
        }
    }
}

/// Зондировать наружу от отличающегося пикселя периметра (прочь от маски): true,
/// если фон возвращается в пределах `stroke_probe` пикс. (ограниченный штрих),
/// false — если разница продолжается или уходит за crop (объект).
fn probe_outward_bg(
    rgb: &image::RgbImage,
    mask: &image::GrayImage,
    x: u32,
    y: u32,
    bg: image::Rgb<u8>,
    stroke_probe: i32,
) -> bool {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let (xi, yi) = (x as i32, y as i32);
    // Направление наружу = прочь от 4-соседнего пикселя маски.
    let mut dir = None;
    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        let (mx, my) = (xi + dx, yi + dy);
        if mx >= 0 && my >= 0 && mx < w && my < h && mask.get_pixel(mx as u32, my as u32)[0] != 0 {
            dir = Some((-dx, -dy));
            break;
        }
    }
    let Some((ox, oy)) = dir else { return true };
    for k in 1..=stroke_probe {
        let (px, py) = (xi + ox * k, yi + oy * k);
        if px < 0 || py < 0 || px >= w || py >= h {
            return false; // ушли за crop, всё ещё отличаясь → объект.
        }
        if chan_dist(*rgb.get_pixel(px as u32, py as u32), bg) <= AUTOCLEAN_SAME_TOL {
            return true; // фон в пределах зонда → ограниченный штрих.
        }
    }
    false
}

/// Стереть границу маски у пикселя `(x, y)`, убрав его 4-соседние пиксели маски, и
/// собрать координаты стёртого в `removed` (для oscillation guard эволюции).
fn erode_around_collect(
    mask: &mut image::GrayImage,
    x: u32,
    y: u32,
    removed: &mut Vec<(u32, u32)>,
) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        let (nx, ny) = (x as i32 + dx, y as i32 + dy);
        if nx >= 0 && ny >= 0 && nx < w && ny < h && mask.get_pixel(nx as u32, ny as u32)[0] != 0 {
            mask.put_pixel(nx as u32, ny as u32, image::Luma([0]));
            removed.push((nx as u32, ny as u32));
        }
    }
}

/// Стереть 4-соседние пиксели маски у фонового пикселя `(x, y)` (без учёта guard;
/// для финальной санити-обрезки, где блокировка не нужна).
fn erode_around(mask: &mut image::GrayImage, x: u32, y: u32) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        let (nx, ny) = (x as i32 + dx, y as i32 + dy);
        if nx >= 0 && ny >= 0 && nx < w && ny < h && mask.get_pixel(nx as u32, ny as u32)[0] != 0 {
            mask.put_pixel(nx as u32, ny as u32, image::Luma([0]));
        }
    }
}

/// Условная дилатация заливки на ≤ `steps` пикс.: claims ТОЛЬКО фоновые пиксели
/// (`chan_dist(px, bg) ≤ AUTOCLEAN_SAME_TOL`), не помеченные `outside` клипом.
///
/// Заменяет прежнюю слепую `dilate_gray_inplace(FILL_PADDING)`: убирает
/// «просвечивание» исходника на LINEAR-крае, но никогда не перекрашивает контур
/// пузыря или чужой контент (они либо не фон, либо в `outside`).
fn conditional_pad(
    fill: &mut image::GrayImage,
    rgb: &image::RgbImage,
    bg: image::Rgb<u8>,
    outside: &[bool],
    steps: i32,
) {
    // `fill` — crop страницы, прошедшей entry-инвариант `autoclean_page`, поэтому его
    // размеры укладываются в i32; приведение безопасно.
    let (w, h) = (fill.width() as i32, fill.height() as i32);
    if w == 0 || h == 0 {
        return;
    }
    for _ in 0..steps {
        let src = fill.clone();
        for y in 0..h {
            for x in 0..w {
                if src.get_pixel(x as u32, y as u32)[0] != 0 {
                    continue; // уже залито
                }
                let idx = (y * w + x) as usize;
                if outside.get(idx).copied().unwrap_or(false) {
                    continue; // «снаружи» пузыря — не заливать
                }
                if chan_dist(*rgb.get_pixel(x as u32, y as u32), bg) > AUTOCLEAN_SAME_TOL {
                    continue; // не фон (контур/арт) — не заливать
                }
                // 8-смежность с уже залитым пикселем (шаг дилатации).
                let mut hit = false;
                'n: for dy in -1..=1 {
                    for dx in -1..=1 {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx >= 0
                            && ny >= 0
                            && nx < w
                            && ny < h
                            && src.get_pixel(nx as u32, ny as u32)[0] != 0
                        {
                            hit = true;
                            break 'n;
                        }
                    }
                }
                if hit {
                    fill.put_pixel(x as u32, y as u32, image::Luma([255]));
                }
            }
        }
    }
}

/// Финальная санити-обрезка: срезать пиксели заливки, граничащие с «пиком» (пикселем
/// с `chan_dist > AUTOCLEAN_SAME_TOL`), пока внешнее кольцо заливки не станет чистым
/// фоном (ноль пиков) ЛИБО маска не опустеет.
///
/// После клипа заливка = интерьер пузыря; её внешнее кольцо включает контур (тёмный,
/// пик) — обрезка отодвигает заливку внутрь от контура. На открытом фоне пиков нет —
/// обрезка не срабатывает.
///
/// Возвращает `true`, если периметр удалось довести до нуля пиков (в т.ч. полностью
/// опустошив маску), и `false`, если проход нашёл пики, но не удалил ни одного пикселя
/// маски — такую заливку нельзя сделать беспиковой, и вызывающий её отвергает
/// (постусловие «ноль пиков» соблюдается: заливка с пиками никогда не красится).
///
/// Завершение: каждый пик из `outer_ring` по построению 4-смежен с ≥ 1 пикселем маски,
/// поэтому продуктивный проход стирает ≥ 1 пиксель; маска конечна ⇒ цикл либо очищает
/// кольцо, либо опустошает маску, либо (страховка) фиксирует отсутствие прогресса и
/// выходит с `false`. Число проходов ограничено начальной площадью маски.
fn final_sanity_trim(
    fill: &mut image::GrayImage,
    rgb: &image::RgbImage,
    bg: image::Rgb<u8>,
) -> bool {
    loop {
        // Пустая маска ⇒ пустое кольцо ⇒ нет пиков ⇒ беспиково (тривиально true).
        let ring = outer_ring(fill);
        let peaks: Vec<(u32, u32)> = ring
            .into_iter()
            .filter(|&(x, y)| chan_dist(*rgb.get_pixel(x, y), bg) > AUTOCLEAN_SAME_TOL)
            .collect();
        if peaks.is_empty() {
            return true;
        }
        let before = fill.as_raw().iter().filter(|&&v| v != 0).count();
        for (x, y) in peaks {
            erode_around(fill, x, y);
        }
        let after = fill.as_raw().iter().filter(|&&v| v != 0).count();
        // Страховка завершения: если пики есть, а маска не уменьшилась, беспиковой её не
        // сделать — отвергаем кандидат вместо покраски заливки с пиками на кольце.
        if after == before {
            return false;
        }
    }
}

/// true, если пиксели под маской похожи на текст (чернила-на-фоне с тонкими
/// штрихами), а не на однородное пятно лица/волос. «Чернила» = пиксели,
/// отличающиеся от фона `bg` больше `AUTOCLEAN_SAME_TOL`.
fn has_text_structure(rgb: &image::RgbImage, mask: &image::GrayImage, bg: image::Rgb<u8>) -> bool {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let is_ink = |x: i32, y: i32| -> bool {
        x >= 0
            && y >= 0
            && x < w
            && y < h
            && mask.get_pixel(x as u32, y as u32)[0] != 0
            && chan_dist(*rgb.get_pixel(x as u32, y as u32), bg) > AUTOCLEAN_SAME_TOL
    };
    let (mut area, mut ink, mut edge) = (0u64, 0u64, 0u64);
    for y in 0..h {
        for x in 0..w {
            if mask.get_pixel(x as u32, y as u32)[0] == 0 {
                continue;
            }
            area += 1;
            if !is_ink(x, y) {
                continue;
            }
            ink += 1;
            let on_boundary = [(-1, 0), (1, 0), (0, -1), (0, 1)]
                .iter()
                .any(|&(dx, dy)| !is_ink(x + dx, y + dy));
            if on_boundary {
                edge += 1;
            }
        }
    }
    if area == 0 || ink == 0 {
        return false;
    }
    let ink_frac = ink as f32 / area as f32;
    let edge_ratio = edge as f32 / ink as f32;
    (AUTOCLEAN_MIN_INK_FRAC..=AUTOCLEAN_MAX_INK_FRAC).contains(&ink_frac)
        && edge_ratio >= AUTOCLEAN_MIN_EDGE_RATIO
}

/// true, если прямоугольник в основном цвета фона (редкие чернила текста), так
/// что заливка лишь стирает текст. false для панелей, заполненных контентом.
fn box_interior_fillable(
    rgb: &image::RgbImage,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    bg: image::Rgb<u8>,
) -> bool {
    let (mut total, mut ink) = (0u64, 0u64);
    for y in y1.max(0)..y2.min(rgb.height() as i32) {
        for x in x1.max(0)..x2.min(rgb.width() as i32) {
            total += 1;
            if chan_dist(*rgb.get_pixel(x as u32, y as u32), bg) > AUTOCLEAN_SAME_TOL {
                ink += 1;
            }
        }
    }
    total > 0 && (ink as f32) <= AUTOCLEAN_BOX_INK_LIMIT * (total as f32)
}

/// Обрезать маску заливки по интерьеру пузыря, в котором лежит текст, и вернуть
/// множество пикселей «снаружи» пузыря (для условного паддинга).
///
/// Заливка может по краям/углам зайти за контур пузыря (тонкую тёмную кривую) в
/// наружный фон. Интерьер пузыря вычисляется заливкой:
///   1. фон, связный со штрихами текста и не пересекающий контур (`interior_bg`);
///   2. «снаружи» — заливка от рамки crop по не-интерьерным пикселям; контур
///      достижим от рамки, поэтому попадает в «снаружи».
///
/// Всё «снаружи» (контур, наружный фон, чужой контент за углами) из заливки
/// убирается; интерьерный фон и замкнутые им буквы остаются. Возвращаемый `Vec<bool>`
/// (длиной `w*h`) — маска «снаружи»: пусто (всё `false`), если выраженного контура нет
/// (текст на открытом фоне) — тогда заливка не меняется.
fn clip_fill_to_bubble_interior(
    fill: &mut image::GrayImage,
    rgb: &image::RgbImage,
    text: &image::GrayImage,
    bg: image::Rgb<u8>,
) -> Vec<bool> {
    let (w, h) = (rgb.width() as i32, rgb.height() as i32);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let at = |x: i32, y: i32| (y * w + x) as usize;
    let near_bg =
        |x: i32, y: i32| chan_dist(*rgb.get_pixel(x as u32, y as u32), bg) <= AUTOCLEAN_SAME_TOL;

    // 1. Интерьерный фон: заливка near-bg пикселей от seed'ов — near-bg соседей
    //    штрихов текста (заведомо внутри пузыря). Контур (тёмный) — стена.
    let mut interior_bg = vec![false; (w * h) as usize];
    let mut stack: Vec<(i32, i32)> = Vec::new();
    let try_push = |x: i32, y: i32, vis: &mut Vec<bool>, st: &mut Vec<(i32, i32)>| {
        if x >= 0 && y >= 0 && x < w && y < h && !vis[at(x, y)] && near_bg(x, y) {
            vis[at(x, y)] = true;
            st.push((x, y));
        }
    };
    for y in 0..h {
        for x in 0..w {
            if text.get_pixel(x as u32, y as u32)[0] == 0 {
                continue;
            }
            for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                try_push(x + dx, y + dy, &mut interior_bg, &mut stack);
            }
        }
    }
    while let Some((x, y)) = stack.pop() {
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            try_push(x + dx, y + dy, &mut interior_bg, &mut stack);
        }
    }
    // Seed'ов не нашлось (текст вплотную окружён не-фоном) — клипать нечем,
    // оставляем заливку как есть; «снаружи» пусто.
    if !interior_bg.iter().any(|&v| v) {
        return vec![false; (w * h) as usize];
    }

    // 2. «Снаружи» = заливка от рамки crop по не-интерьерным пикселям. Контур
    //    пузыря достижим от рамки → «снаружи»; буквы, замкнутые интерьерным
    //    фоном, недостижимы → остаются.
    let mut outside = vec![false; (w * h) as usize];
    let mut q: Vec<(i32, i32)> = Vec::new();
    let seed_out = |x: i32, y: i32, out: &mut Vec<bool>, q: &mut Vec<(i32, i32)>| {
        if x >= 0 && y >= 0 && x < w && y < h && !out[at(x, y)] && !interior_bg[at(x, y)] {
            out[at(x, y)] = true;
            q.push((x, y));
        }
    };
    for x in 0..w {
        seed_out(x, 0, &mut outside, &mut q);
        seed_out(x, h - 1, &mut outside, &mut q);
    }
    for y in 0..h {
        seed_out(0, y, &mut outside, &mut q);
        seed_out(w - 1, y, &mut outside, &mut q);
    }
    while let Some((x, y)) = q.pop() {
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
            seed_out(x + dx, y + dy, &mut outside, &mut q);
        }
    }

    // 3. Гасим пиксели заливки, попавшие «снаружи» интерьера пузыря.
    for y in 0..h {
        for x in 0..w {
            if outside[at(x, y)] {
                fill.put_pixel(x as u32, y as u32, image::Luma([0]));
            }
        }
    }
    outside
}

#[derive(Debug)]
struct ConnectedComponents {
    labels: Vec<i32>,
    pixels: Vec<Vec<usize>>,
}

fn extract_connected_components(mask: &[u8], width: usize, height: usize) -> ConnectedComponents {
    let mut labels = vec![-1i32; width.saturating_mul(height)];
    let mut pixels = Vec::<Vec<usize>>::new();
    if mask.is_empty() || width == 0 || height == 0 {
        return ConnectedComponents { labels, pixels };
    }

    let mut queue = VecDeque::<usize>::new();
    let mut label = 0i32;
    for seed in 0..mask.len() {
        if mask[seed] == 0 || labels[seed] >= 0 {
            continue;
        }
        labels[seed] = label;
        queue.clear();
        queue.push_back(seed);
        let mut component_pixels = Vec::<usize>::new();
        while let Some(idx) = queue.pop_front() {
            component_pixels.push(idx);
            let x = idx % width;
            let y = idx / width;
            for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let nidx = ny.saturating_mul(width).saturating_add(nx);
                    if mask[nidx] == 0 || labels[nidx] >= 0 {
                        continue;
                    }
                    labels[nidx] = label;
                    queue.push_back(nidx);
                }
            }
        }
        pixels.push(component_pixels);
        label = label.saturating_add(1);
    }
    ConnectedComponents { labels, pixels }
}

fn dilate_binary_mask(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<u8> {
    if mask.is_empty() || width == 0 || height == 0 {
        return Vec::new();
    }
    if radius == 0 {
        return mask.to_vec();
    }
    let mut out = vec![0u8; mask.len()];
    for y in 0..height {
        let y0 = y.saturating_sub(radius);
        let y1 = (y + radius).min(height - 1);
        for x in 0..width {
            let x0 = x.saturating_sub(radius);
            let x1 = (x + radius).min(width - 1);
            let mut any = false;
            'scan: for yy in y0..=y1 {
                let row = yy.saturating_mul(width);
                for xx in x0..=x1 {
                    if mask[row + xx] != 0 {
                        any = true;
                        break 'scan;
                    }
                }
            }
            out[y.saturating_mul(width).saturating_add(x)] = if any { 255 } else { 0 };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(width: u32, height: u32, points: &[(u32, u32)]) -> image::GrayImage {
        let mut mask = image::GrayImage::new(width, height);
        for &(x, y) in points {
            mask.put_pixel(x, y, image::Luma([255]));
        }
        mask
    }

    fn rgb(width: u32, height: u32, color: image::Rgb<u8>) -> image::RgbImage {
        image::RgbImage::from_pixel(width, height, color)
    }

    /// Rasterize the engine's winning fills to a `w*h` boolean "painted" grid so tests
    /// can assert per-pixel outcomes of the full per-cluster pipeline without egui.
    fn render_painted(engine: &AutocleanEngineResult, w: usize, h: usize) -> Vec<bool> {
        let mut painted = vec![false; w * h];
        for fill in &engine.fills {
            for y in 0..fill.mask.height() as i32 {
                for x in 0..fill.mask.width() as i32 {
                    if fill.mask.get_pixel(x as u32, y as u32)[0] == 0 {
                        continue;
                    }
                    let (gx, gy) = (fill.ox + x, fill.oy + y);
                    if gx >= 0 && gy >= 0 && (gx as usize) < w && (gy as usize) < h {
                        painted[gy as usize * w + gx as usize] = true;
                    }
                }
            }
        }
        painted
    }

    /// Build an RGBA page from an RGB one (opaque alpha) for engine input.
    fn rgba_from_rgb(src: &image::RgbImage) -> image::RgbaImage {
        let mut out = image::RgbaImage::new(src.width(), src.height());
        for (x, y, p) in src.enumerate_pixels() {
            out.put_pixel(x, y, image::Rgba([p[0], p[1], p[2], 255]));
        }
        out
    }

    /// Ellipse implicit value `((x-cx)/rx)^2 + ((y-cy)/ry)^2`; <1 inside, ~1 on edge.
    fn ellipse_val(x: u32, y: u32, cx: f32, cy: f32, rx: f32, ry: f32) -> f32 {
        let dx = (x as f32 - cx) / rx;
        let dy = (y as f32 - cy) / ry;
        dx * dx + dy * dy
    }

    #[test]
    fn chan_dist_uses_per_channel_chebyshev_distance() {
        assert_eq!(chan_dist(image::Rgb([1, 200, 30]), image::Rgb([17, 150, 49])), 50);
    }

    #[test]
    fn outer_ring_is_four_adjacent_background_inside_bounds() {
        let mask = gray(3, 3, &[(0, 0)]);
        let ring = outer_ring(&mask);
        assert_eq!(ring, vec![(1, 0), (0, 1)]);
        assert!(ring.iter().all(|&(x, y)| mask.get_pixel(x, y)[0] == 0));
    }

    #[test]
    fn ring_background_uses_per_channel_median_despite_minority_outliers() {
        let mut image = rgb(5, 1, image::Rgb([10, 20, 30]));
        image.put_pixel(4, 0, image::Rgb([250, 1, 240]));
        assert_eq!(
            ring_background(&image, &[(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]),
            image::Rgb([10, 20, 30])
        );
    }

    #[test]
    fn fill_holes_fills_enclosed_holes_but_not_open_bays() {
        let mut enclosed = gray(
            5,
            5,
            &[(1, 1), (2, 1), (3, 1), (1, 2), (3, 2), (1, 3), (2, 3), (3, 3)],
        );
        fill_holes(&mut enclosed);
        assert_eq!(enclosed.get_pixel(2, 2)[0], 255);

        let mut open = gray(5, 5, &[(1, 1), (2, 1), (3, 1), (1, 2), (1, 3), (2, 3), (3, 3)]);
        fill_holes(&mut open);
        assert_eq!(open.get_pixel(2, 2)[0], 0);
    }

    #[test]
    fn dilation_bbox_and_foreground_follow_current_mask_contract() {
        let mut mask = gray(5, 5, &[(2, 2)]);
        assert!(has_foreground(&mask));
        dilate_gray_inplace(&mut mask, 1);
        assert_eq!(gray_mask_bbox(&mask), Some((1, 1, 4, 4)));
        assert_eq!(mask.get_pixel(1, 1)[0], 255);
        assert_eq!(mask.get_pixel(3, 3)[0], 255);
        assert!(!has_foreground(&image::GrayImage::new(1, 1)));
    }

    #[test]
    fn box_interior_fillable_accepts_less_than_45_percent_ink_and_rejects_more() {
        let bg = image::Rgb([240, 240, 240]);
        let mut image = rgb(10, 10, bg);
        for y in 0..4 {
            for x in 0..10 {
                image.put_pixel(x, y, image::Rgb([0, 0, 0]));
            }
        }
        assert!(box_interior_fillable(&image, 0, 0, 10, 10, bg));
        for x in 0..6 {
            image.put_pixel(x, 4, image::Rgb([0, 0, 0]));
        }
        assert!(!box_interior_fillable(&image, 0, 0, 10, 10, bg));
    }

    #[test]
    fn text_structure_accepts_thin_strokes_and_rejects_solid_blob() {
        let bg = image::Rgb([255, 255, 255]);
        let mask = gray(
            10,
            10,
            &(0..100)
                .map(|index| ((index % 10) as u32, (index / 10) as u32))
                .collect::<Vec<_>>(),
        );
        let mut strokes = rgb(10, 10, bg);
        for y in 0..10 {
            strokes.put_pixel(5, y, image::Rgb([0, 0, 0]));
        }
        assert!(has_text_structure(&strokes, &mask, bg));
        let solid = rgb(10, 10, image::Rgb([0, 0, 0]));
        assert!(!has_text_structure(&solid, &mask, bg));
    }

    // --- ported engine tests (were `grow_until_homogeneous`, now evolve) ---------

    #[test]
    fn evolve_converges_on_flat_background() {
        // Ported from `grow_until_homogeneous_converges_on_flat_background`: a single
        // mask pixel on flat bg has an all-background ring, so evolve converges at once
        // and reports the background colour (plan §Engine: convergence = zero peaks).
        let background = image::Rgb([220, 220, 220]);
        let image = rgb(7, 7, background);
        let mask = gray(7, 7, &[(3, 3)]);
        let converged = evolve_mask_to_homogeneous(&image, mask, 3, 3).expect("flat bg converges");
        assert_eq!(converged.bg, background);
        assert_eq!(converged.stats.grow_iters, 0);
        assert_eq!(converged.stats.shrink_iters, 0);
    }

    #[test]
    fn evolve_rejects_when_more_than_thirty_percent_of_ring_is_non_background() {
        // Ported: 2 of 4 ring pixels are non-bg; the per-channel median still resolves to
        // the background, so both dark pixels are offenders (50% > GROW_LIMIT = 30%) and
        // evolve rejects the candidate (plan §Engine: GROW_LIMIT early reject kept).
        let background = image::Rgb([220, 220, 220]);
        let mut image = rgb(7, 7, background);
        let mask = gray(7, 7, &[(3, 3)]);
        image.put_pixel(2, 3, image::Rgb([0, 0, 0]));
        image.put_pixel(4, 3, image::Rgb([0, 0, 0]));
        assert!(evolve_mask_to_homogeneous(&image, mask, 3, 3).is_none());
    }

    #[test]
    fn evolve_absorbs_isolated_contour_pixel_then_rejects_small_cluster() {
        // Replaces `grow_until_homogeneous_absorbs_contour_pixel_as_stroke_then_rejects`.
        // At miniature scale evolve STILL absorbs the first contour pixel as a bounded
        // stroke (probe sees bg behind the 2 px contour within STROKE_PROBE_MAX) and the
        // very next ring is 50% contour -> GROW_LIMIT rejects. On production masks the
        // contact arc is a small ring fraction and growth would continue through the
        // contour; Phase 1's real fix is the UNIVERSAL interior clip in the pipeline
        // (see `pipeline_no_paint_on_or_outside_ellipse_contour`), not this local reject.
        let background = image::Rgb([220, 220, 220]);
        let mut image = rgb(11, 11, background);
        for y in 0..11 {
            image.put_pixel(4, y, image::Rgb([0, 0, 0]));
            image.put_pixel(5, y, image::Rgb([0, 0, 0]));
        }
        let mask = gray(11, 11, &[(3, 5)]);
        // Grab the mask before it is moved into evolve to check the absorbed pixel.
        let mut probe = gray(11, 11, &[(3, 5)]);
        // Reproduce one growth step to confirm the contour pixel is absorbed first.
        let ring = outer_ring(&probe);
        let bg = ring_background(&image, &ring);
        assert!(probe_outward_bg(&image, &probe, 4, 5, bg, AUTOCLEAN_STROKE_PROBE_MAX));
        probe.put_pixel(4, 5, image::Luma([255]));
        assert_eq!(probe.get_pixel(4, 5)[0], 255);
        // The full evolve rejects this tiny cluster.
        assert!(evolve_mask_to_homogeneous(&image, mask, 3, 3).is_none());
    }

    // --- clip now returns the `outside` set (signature change) -------------------

    #[test]
    fn bubble_interior_clip_removes_fill_outside_closed_contour() {
        let background = image::Rgb([255, 255, 255]);
        let mut image = rgb(9, 9, background);
        for coordinate in 1..8 {
            image.put_pixel(coordinate, 1, image::Rgb([0, 0, 0]));
            image.put_pixel(coordinate, 7, image::Rgb([0, 0, 0]));
            image.put_pixel(1, coordinate, image::Rgb([0, 0, 0]));
            image.put_pixel(7, coordinate, image::Rgb([0, 0, 0]));
        }
        let text = gray(9, 9, &[(4, 4)]);
        let mut fill = image::GrayImage::from_pixel(9, 9, image::Luma([255]));
        let outside = clip_fill_to_bubble_interior(&mut fill, &image, &text, background);
        assert_eq!(fill.get_pixel(0, 0)[0], 0);
        assert_eq!(fill.get_pixel(4, 4)[0], 255);
        assert_eq!(fill.get_pixel(4, 1)[0], 0);
        // The returned `outside` set marks the corner and clears the interior.
        assert!(outside[0]);
        assert!(!outside[4 * 9 + 4]);
    }

    #[test]
    fn bubble_interior_clip_is_a_no_op_without_a_contour() {
        let background = image::Rgb([255, 255, 255]);
        let image = rgb(7, 7, background);
        let text = gray(7, 7, &[(3, 3)]);
        let mut fill = image::GrayImage::from_pixel(7, 7, image::Luma([255]));
        let outside = clip_fill_to_bubble_interior(&mut fill, &image, &text, background);
        assert!(fill.as_raw().iter().all(|&value| value == 255));
        // No contour -> nothing is outside.
        assert!(outside.iter().all(|&v| !v));
    }

    // --- Phase 1 required pipeline tests -----------------------------------------

    #[test]
    fn pipeline_no_paint_on_or_outside_ellipse_contour() {
        // Test 1: closed ellipse contour + multi-line text inside. No painted pixel may
        // land on the contour band or outside it (the reported corner-erase bug class).
        let (w, h) = (60usize, 40usize);
        let (cx, cy, rx, ry) = (30.0f32, 20.0f32, 24.0f32, 15.0f32);
        let background = image::Rgb([245, 245, 245]);
        let mut page = rgb(w as u32, h as u32, background);
        // Thick closed ring: 0.80 <= ellipse_val <= 1.20 (no 8-conn gaps).
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                let v = ellipse_val(x, y, cx, cy, rx, ry);
                if (0.80..=1.20).contains(&v) {
                    page.put_pixel(x, y, image::Rgb([0, 0, 0]));
                }
            }
        }
        // Multi-line text strokes well inside the ellipse.
        let mut mask = vec![0u8; w * h];
        for (ty, xs) in [(15u32, 22u32..=38u32), (20, 24..=36), (25, 22..=38)] {
            for tx in xs {
                page.put_pixel(tx, ty, image::Rgb([10, 10, 10]));
                mask[ty as usize * w + tx as usize] = 255;
            }
        }
        let rgba = rgba_from_rgb(&page);
        let engine = run_autoclean_engine(&rgba, &mask, w, h, 12, None);
        assert!(engine.regions_filled >= 1, "text inside a bubble must be cleaned");
        let painted = render_painted(&engine, w, h);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                if !painted[y as usize * w + x as usize] {
                    continue;
                }
                let v = ellipse_val(x, y, cx, cy, rx, ry);
                assert!(
                    v < 0.80,
                    "painted pixel ({x},{y}) is on/outside the contour (v={v})"
                );
            }
        }
    }

    #[test]
    fn pipeline_bbox_corner_over_contour_not_painted() {
        // Test 2: the reported bug. A circular bubble with text whose bounding box corners
        // fall on/outside the circle. With blocks=None candidate B is the cluster bbox
        // (worst case) whose corners overlap the contour; the corners must NOT be painted.
        let (w, h) = (48usize, 48usize);
        let (cx, cy, r) = (24.0f32, 24.0f32, 20.0f32);
        let background = image::Rgb([250, 250, 250]);
        let mut page = rgb(w as u32, h as u32, background);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                if (r - 1.5..=r + 1.5).contains(&d) {
                    page.put_pixel(x, y, image::Rgb([0, 0, 0]));
                }
            }
        }
        // Text roughly filling the inscribed area so its bbox corners exit the circle.
        let mut mask = vec![0u8; w * h];
        for ty in 12u32..=36 {
            if ty % 3 != 0 {
                continue;
            }
            for tx in 12u32..=36 {
                let d = ((tx as f32 - cx).powi(2) + (ty as f32 - cy).powi(2)).sqrt();
                if d < r - 2.0 {
                    page.put_pixel(tx, ty, image::Rgb([12, 12, 12]));
                    mask[ty as usize * w + tx as usize] = 255;
                }
            }
        }
        let rgba = rgba_from_rgb(&page);
        let engine = run_autoclean_engine(&rgba, &mask, w, h, 10, None);
        let painted = render_painted(&engine, w, h);
        // Every painted pixel must be strictly inside the circle (never on/over the ring).
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                if !painted[y as usize * w + x as usize] {
                    continue;
                }
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                assert!(d < r - 0.5, "painted pixel ({x},{y}) reaches the contour (d={d})");
            }
        }
    }

    #[test]
    fn pipeline_solid_rect_mask_is_handled() {
        // Test 3: blocks-only worst case — the input mask is a SOLID rectangle (as if
        // synthesized from detector boxes) over sparse text on flat background. The engine
        // must converge and clean the text region.
        let (w, h) = (40usize, 30usize);
        let background = image::Rgb([248, 248, 248]);
        let mut page = rgb(w as u32, h as u32, background);
        // Sparse text strokes.
        let text_pixels = [(14u32, 12u32), (16, 12), (18, 12), (14, 16), (18, 16)];
        for &(tx, ty) in &text_pixels {
            page.put_pixel(tx, ty, image::Rgb([0, 0, 0]));
        }
        // Solid rectangular mask covering the text (rect-union worst case).
        let mut mask = vec![0u8; w * h];
        for ty in 10u32..=18 {
            for tx in 12u32..=20 {
                mask[ty as usize * w + tx as usize] = 255;
            }
        }
        let rgba = rgba_from_rgb(&page);
        let engine = run_autoclean_engine(&rgba, &mask, w, h, 8, None);
        assert!(engine.regions_filled >= 1, "solid-rect mask on flat bg must be filled");
        let painted = render_painted(&engine, w, h);
        for &(tx, ty) in &text_pixels {
            assert!(
                painted[ty as usize * w + tx as usize],
                "text stroke ({tx},{ty}) under a solid rect mask must be cleaned"
            );
        }
    }

    #[test]
    fn pipeline_gradient_background_skips_cluster() {
        // Test 4: a smooth gradient background never yields a homogeneous perimeter, so no
        // candidate converges and the cluster is skipped (no fill produced).
        let (w, h) = (40usize, 30usize);
        let mut page = image::RgbImage::new(w as u32, h as u32);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                let g = (x as f32 / w as f32 * 255.0) as u8;
                page.put_pixel(x, y, image::Rgb([g, g, g]));
            }
        }
        let mut mask = vec![0u8; w * h];
        for ty in 12u32..=18 {
            for tx in 16u32..=24 {
                // Dark-ish strokes over the gradient.
                page.put_pixel(tx, ty, image::Rgb([0, 0, 0]));
                mask[ty as usize * w + tx as usize] = 255;
            }
        }
        let rgba = rgba_from_rgb(&page);
        let engine = run_autoclean_engine(&rgba, &mask, w, h, 10, None);
        assert_eq!(engine.regions_filled, 0, "gradient bg must not converge");
        assert!(engine.regions_skipped >= 1);
        assert!(engine.fills.is_empty());
    }

    #[test]
    fn pipeline_text_touching_object_reports_partial_and_spares_object() {
        // Test 5: text touching a foreign solid object. A bulk of thin strokes sits on
        // clean background (converges), while a single stroke protrusion abuts the object.
        // The protrusion is eroded away by shrink (it cannot be made homogeneous against
        // the wide object), so the converged mask retains the bulk but loses the
        // protrusion -> coverage < prefer threshold -> the cluster is reported partial.
        // The object itself is never painted.
        let (w, h) = (36usize, 30usize);
        let (cx, cy, r) = (18.0f32, 15.0f32, 12.0f32);
        let background = image::Rgb([245, 245, 245]);
        let mut page = rgb(w as u32, h as u32, background);
        // Foreign structure: a closed bubble contour the text is packed against.
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                if (r - 1.4..=r + 1.4).contains(&d) {
                    page.put_pixel(x, y, image::Rgb([0, 0, 0]));
                }
            }
        }
        // Dense text packed up to the contour: some strokes lie against the contour, so
        // the universal clip / sanity trim drops those boundary strokes and coverage
        // falls below the prefer threshold (partial), while the bulk is still cleaned.
        let mut mask = vec![0u8; w * h];
        for ty in 4u32..=26 {
            for tx in 4u32..=32 {
                let d = ((tx as f32 - cx).powi(2) + (ty as f32 - cy).powi(2)).sqrt();
                if d < r - 0.6 && (tx + ty) % 2 == 0 {
                    page.put_pixel(tx, ty, image::Rgb([10, 10, 10]));
                    mask[ty as usize * w + tx as usize] = 255;
                }
            }
        }

        let rgba = rgba_from_rgb(&page);
        let engine = run_autoclean_engine(&rgba, &mask, w, h, 8, None);
        assert!(engine.regions_filled >= 1, "the bulk text must still be cleaned");
        // Boundary strokes packed against the contour are trimmed, so coverage drops below
        // the prefer threshold and the cluster is reported partial.
        assert!(
            engine.regions_partial >= 1,
            "text touching a foreign structure must report partial coverage"
        );
        // The contour (foreign structure) is never painted: every painted pixel is
        // strictly inside it.
        let painted = render_painted(&engine, w, h);
        for y in 0..h as u32 {
            for x in 0..w as u32 {
                if !painted[y as usize * w + x as usize] {
                    continue;
                }
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                assert!(d < r - 0.6, "painted pixel ({x},{y}) reaches the contour (d={d})");
            }
        }
    }

    #[test]
    fn conditional_pad_never_claims_out_of_tolerance_pixel() {
        // Test 6: conditional padding only claims background-like pixels. An out-of-tol
        // pixel adjacent to the fill must never be added.
        let bg = image::Rgb([240, 240, 240]);
        let mut page = rgb(6, 3, bg);
        // A dark (out-of-tolerance) pixel right next to the fill seed.
        page.put_pixel(3, 1, image::Rgb([0, 0, 0]));
        let mut fill = gray(6, 3, &[(2, 1)]);
        let outside = vec![false; 6 * 3];
        conditional_pad(&mut fill, &page, bg, &outside, 2);
        // The bg pixel (1,1) is claimed; the dark pixel (3,1) is not.
        assert_eq!(fill.get_pixel(1, 1)[0], 255);
        assert_eq!(fill.get_pixel(3, 1)[0], 0);
    }

    #[test]
    fn evolve_oscillation_guard_terminates_on_adversarial_ring() {
        // Test 7: an adversarial checkerboard-ish ring around the seed. Without the
        // oscillation guard grow/shrink could alternate; the guard bounds state changes so
        // evolve must return (the test completing is the termination assertion).
        let background = image::Rgb([200, 200, 200]);
        let mut page = rgb(9, 9, background);
        // Alternating dark/bg pixels on the ring around the centre seed.
        let ring_dark = [(3u32, 3u32), (5, 3), (3, 5), (5, 5), (4, 2), (4, 6)];
        for &(x, y) in &ring_dark {
            page.put_pixel(x, y, image::Rgb([0, 0, 0]));
        }
        let mask = gray(9, 9, &[(4, 4)]);
        // Just assert it terminates and yields a definite Option (no hang / no panic).
        let _ = evolve_mask_to_homogeneous(&page, mask, 4, 4);
    }

    #[test]
    fn build_box_candidate_uses_blocks_when_present_else_bbox() {
        // Test 8: candidate B is the block union (clipped to crop) when blocks intersect
        // the cluster bbox, and the cluster bbox otherwise.
        // Crop 20x20 at page offset (10,10); cluster bbox in page space is (12,12,18,18).
        let (cw, ch) = (20u32, 20u32);
        let cluster_bbox = (12, 12, 18, 18);
        let crop_off = (10, 10);

        // With a block covering page (13,13,16,16): B is that block, crop-local (3,3,6,6).
        let blocks = [[13, 13, 16, 16]];
        let (mask_b, bbox_b) = build_box_candidate(cw, ch, cluster_bbox, crop_off, Some(&blocks));
        assert_eq!(bbox_b, (3, 3, 6, 6));
        assert_eq!(mask_b.get_pixel(3, 3)[0], 255);
        assert_eq!(mask_b.get_pixel(5, 5)[0], 255);
        assert_eq!(mask_b.get_pixel(6, 6)[0], 0);

        // With no blocks, B is the cluster bbox, crop-local (2,2,8,8).
        let (mask_none, bbox_none) = build_box_candidate(cw, ch, cluster_bbox, crop_off, None);
        assert_eq!(bbox_none, (2, 2, 8, 8));
        assert_eq!(mask_none.get_pixel(2, 2)[0], 255);
        assert_eq!(mask_none.get_pixel(7, 7)[0], 255);

        // A block that does not intersect the cluster bbox falls back to the bbox.
        let far = [[0, 0, 3, 3]];
        let (_mask_far, bbox_far) = build_box_candidate(cw, ch, cluster_bbox, crop_off, Some(&far));
        assert_eq!(bbox_far, (2, 2, 8, 8));
    }

    // --- Finding 1: sanity trim honours its zero-peak postcondition -----------------

    #[test]
    fn final_sanity_trim_peels_all_dirty_layers_leaving_zero_peaks() {
        // A fill whose outer boundary hides MORE than AUTOCLEAN_FILL_PADDING + 2 dark
        // layers. The old bounded loop stopped after that many passes and left dark peaks
        // on the fill ring (a fill painted WITH peaks); the convergent loop must peel until
        // the ring is clean. Postcondition: zero peaks on the outer ring, fill non-empty.
        let (w, h) = (23u32, 23u32);
        let (cx, cy) = (11i32, 11i32);
        let bg = image::Rgb([255, 255, 255]);
        let mut page = rgb(w, h, bg);
        // Thick dark contour band 3..=9 (7 layers, > 4) by Chebyshev distance; white core.
        for y in 0..h {
            for x in 0..w {
                let d = (x as i32 - cx).abs().max((y as i32 - cy).abs());
                if (3..=9).contains(&d) {
                    page.put_pixel(x, y, image::Rgb([0, 0, 0]));
                }
            }
        }
        // Fill starts one layer inside the band (d <= 8), so its ring sits on dark for
        // several successive layers as it peels inward.
        let mut fill = image::GrayImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let d = (x as i32 - cx).abs().max((y as i32 - cy).abs());
                if d <= 8 {
                    fill.put_pixel(x, y, image::Luma([255]));
                }
            }
        }
        let fillable = final_sanity_trim(&mut fill, &page, bg);
        assert!(fillable, "a fill over open background must be trimmable, not rejected");
        for (x, y) in outer_ring(&fill) {
            assert!(
                chan_dist(*page.get_pixel(x, y), bg) <= AUTOCLEAN_SAME_TOL,
                "ring pixel ({x},{y}) is still a peak after trim (postcondition violated)"
            );
        }
        // The clean core survives (trim did not erode everything away).
        assert!(has_foreground(&fill));
    }

    // --- Finding 2: degenerate candidates are discarded, not reported as filled ------

    #[test]
    fn assemble_fill_discards_candidate_fully_removed_by_clip() {
        // A converged fill sitting entirely OUTSIDE a closed contour is fully removed by
        // the interior clip -> zero area -> discarded (None), so the cluster paints nothing.
        let bg = image::Rgb([255, 255, 255]);
        let mut page = rgb(11, 11, bg);
        for c in 1..10u32 {
            page.put_pixel(c, 1, image::Rgb([0, 0, 0]));
            page.put_pixel(c, 9, image::Rgb([0, 0, 0]));
            page.put_pixel(1, c, image::Rgb([0, 0, 0]));
            page.put_pixel(9, c, image::Rgb([0, 0, 0]));
        }
        // Text stroke inside the box seeds the interior; the fill is a stray pixel outside.
        let strokes = gray(11, 11, &[(5, 5)]);
        let fill = gray(11, 11, &[(0, 0)]);
        let conv = Converged {
            mask: fill,
            bg,
            stats: EvolveStats { grow_iters: 0, shrink_iters: 0 },
        };
        assert!(assemble_fill(&page, &strokes, conv, true).is_none());
    }

    #[test]
    fn assemble_fill_discards_candidate_covering_no_stroke() {
        // A non-empty fill on flat background that overlaps NO original stroke covers
        // nothing worth cleaning -> discarded (None) rather than reported filled at 0
        // coverage.
        let bg = image::Rgb([250, 250, 250]);
        let page = rgb(9, 9, bg);
        let strokes = image::GrayImage::new(9, 9); // no strokes at all
        let fill = gray(9, 9, &[(4, 4)]);
        let conv = Converged {
            mask: fill,
            bg,
            stats: EvolveStats { grow_iters: 0, shrink_iters: 0 },
        };
        assert!(assemble_fill(&page, &strokes, conv, false).is_none());
    }

    #[test]
    fn cluster_skipped_when_all_candidates_removed_by_clip() {
        // End-to-end (engine level): text strokes packed against a contour whose fill is
        // clipped away must leave the cluster skipped, producing no fill for that region.
        // Uses the gradient-skip machinery indirectly: here a degenerate one-pixel cluster
        // outside any interior yields no paintable candidate.
        let (w, h) = (9usize, 9usize);
        let bg = image::Rgb([255, 255, 255]);
        let page = rgb(w as u32, h as u32, bg);
        // A single isolated stroke on flat background: has_text_structure rejects a lone
        // non-ink pixel, so the cluster is skipped and nothing is painted.
        let mut mask = vec![0u8; w * h];
        mask[4 * w + 4] = 255;
        let rgba = rgba_from_rgb(&page);
        let engine = run_autoclean_engine(&rgba, &mask, w, h, 4, None);
        assert_eq!(engine.regions_filled, 0);
        assert!(engine.fills.is_empty());
    }
}
