# manhwa_merge.py
# -*- coding: utf-8 -*-
import os
import sys
import math
import glob
import shutil
from pathlib import Path
from typing import List, Tuple, Dict

import cv2
import numpy as np


# --------------------------- Утилиты сортировки/ввода ---------------------------

def natural_key(s: str):
    """Натуральная сортировка: '2' < '10', '0001' < '0010'."""
    import re
    return [int(text) if text.isdigit() else text.lower()
            for text in re.split(r'(\d+)', s)]


def ask_path() -> Path:
    p = input("Укажите путь к папке с изображениями (отн. или абсолютный): ").strip().strip('"').strip("'")
    if not p:
        print("Путь не указан.")
        sys.exit(1)
    path = Path(p).expanduser().resolve()
    if not path.exists() or not path.is_dir():
        print("Папка не найдена:", path)
        sys.exit(1)
    return path


# --------------------------- Загрузка и нормализация ---------------------------

IMG_EXT = {".jpg", ".jpeg", ".png", ".bmp", ".webp", ".tif", ".tiff"}

def list_images_sorted(folder: Path) -> List[Path]:
    files = [p for p in folder.iterdir() if p.suffix.lower() in IMG_EXT]
    if not files:
        # пробуем рекурсивно
        files = [Path(fp) for ext in IMG_EXT for fp in glob.glob(str(folder / f"**/*{ext}"), recursive=True)]
    if not files:
        print("В папке нет изображений поддерживаемых форматов.")
        sys.exit(1)
    files = sorted(files, key=lambda p: natural_key(p.name))
    return files


def read_bgr(path: Path) -> np.ndarray:
    img = cv2.imread(str(path), cv2.IMREAD_UNCHANGED)
    if img is None:
        raise RuntimeError(f"Не удалось прочитать {path}")
    return img


def mode_width(widths: List[int]) -> int:
    vals, counts = np.unique(widths, return_counts=True)
    return int(vals[np.argmax(counts)])


def unify_widths(images: List[np.ndarray]) -> Tuple[List[np.ndarray], int]:
    """Привести все картинки к одной ширине (к наиболее часто встречающейся)."""
    ws = [im.shape[1] for im in images]
    target_w = mode_width(ws)
    out = []
    for im in images:
        h, w = im.shape[:2]
        if w == target_w:
            out.append(im)
        else:
            scale = target_w / float(w)
            new_h = max(1, int(round(h * scale)))
            interp = cv2.INTER_AREA if scale < 1.0 else cv2.INTER_CUBIC
            out.append(cv2.resize(im, (target_w, new_h), interpolation=interp))
    return out, target_w


# --------------------------- Сшивка по содержимому ---------------------------

def top_template(gray: np.ndarray, max_h: int) -> np.ndarray:
    tpl_h = min(gray.shape[0] // 3, max(64, max_h))
    tpl_h = min(tpl_h, 300)
    return gray[0:tpl_h, :]


def find_vertical_alignment(a_gray: np.ndarray, b_gray: np.ndarray) -> Tuple[bool, int, float, int]:
    """
    Возвращает: (ok, yA, score, overlap_h)
    ok=True только если:
      - высокий NCC и резкий пик (max/second >= 1.08)
      - двусторонняя проверка даёт согласованное перекрытие (расхождение ≤ 6 px)
      - MAD мал, SSIM достаточен на зоне реального перекрытия
    """
    Ha, Wa = a_gray.shape
    Hb, Wb = b_gray.shape
    if Wa != Wb:
        return False, 0, 0.0, 0

    # 1) базовые ограничения на поисковое окно/шаблон
    Omax = int(0.18 * min(Ha, Hb))
    tpl_h = min(max(64, int(0.12 * min(Ha, Hb))), 260)
    tpl = b_gray[:tpl_h, :]

    # слишком «плоский» шаблон — сразу выходим
    g = cv2.Sobel(tpl, cv2.CV_32F, 1, 1, ksize=3)
    if float(np.mean(np.abs(g))) < 2.0:
        return False, 0, 0.0, 0

    # 2) поиск внизу A
    search_h = min(Ha, tpl.shape[0] + Omax)
    y0 = Ha - search_h
    roiA = a_gray[y0:Ha, :]
    resAB = cv2.matchTemplate(roiA, tpl, method=cv2.TM_CCOEFF_NORMED)
    minVal, maxVal, minLoc, maxLoc = cv2.minMaxLoc(resAB)
    second = _second_best_value(resAB, (maxLoc[0], maxLoc[1]), win=12)
    peak_ratio = (maxVal / max(second, 1e-6))

    y_in_roi = maxLoc[1]
    yA = y0 + y_in_roi
    overlap_h = Ha - yA

    # 3) жёсткие пороги по совпадению и разумности высоты перекрытия
    if not (maxVal >= 0.72 and peak_ratio >= 1.08):
        return False, 0, float(maxVal), int(overlap_h)
    if not (16 <= overlap_h <= min(int(0.5*Ha)+40, int(0.5*Hb)+40, tpl_h + Omax)):
        return False, 0, float(maxVal), int(overlap_h)

    # 4) двусторонняя проверка: берем нижнюю полосу A высотой overlap_h и ищем вверху B
    ov = int(overlap_h)
    bandA = a_gray[Ha-ov:Ha, :]
    # ищем bandA в верхе B (в разумном окне)
    search_h_B = min(Hb, ov + Omax)
    roiB = b_gray[:search_h_B, :]
    resBA = cv2.matchTemplate(roiB, bandA, method=cv2.TM_CCOEFF_NORMED)
    _, maxVal2, _, maxLoc2 = cv2.minMaxLoc(resBA)
    # ожидаем, что лучшее совпадение начинаются около y=0
    # то есть top B выравнивается ровно под bandA
    yB_best = maxLoc2[1]
    # согласованность: верх B должен «садиться» на yB≈0 (с погрешностью)
    if abs(yB_best - 0) > 6:
        return False, 0, float(maxVal), int(overlap_h)
    if maxVal2 < 0.70:
        return False, 0, float(maxVal), int(overlap_h)

    # 5) финальная валидация «это действительно одно и то же»:
    # сравним перекрывающиеся полосы
    bandB = b_gray[:ov, :]
    mad = _mad(bandA, bandB)
    ssim = _ssim_gray(bandA, bandB)
    # эмпирические пороги: MAD должен быть небольшим, SSIM — не слишком низким
    if not (mad <= 6.5 and ssim >= 0.60):
        return False, 0, float(maxVal), int(overlap_h)

    return True, int(yA), float(maxVal), int(overlap_h)


def blend_vertical(a: np.ndarray, b: np.ndarray, yA: int, overlap_h: int) -> np.ndarray:
    """
    Линейное смешивание в зоне перекрытия. Предполагается одинаковая ширина.
    """
    Ha, Wa = a.shape[:2]
    Hb, Wb = b.shape[:2]
    out_h = max(Ha, yA + Hb)
    out = np.zeros((out_h, Wa, 3), dtype=np.uint8)

    # Копируем A целиком
    out[0:Ha, :, :] = a

    ov = int(overlap_h)
    if ov > 0:
        # Срезы перекрытия в float32
        a_slice = out[yA:yA + ov, :, :].astype(np.float32)
        b_slice = b[0:ov, :, :].astype(np.float32)

        # ВЕС ДОЛЖЕН БЫТЬ (ov, 1, 1), чтобы растянуться на ширину и на 3 канала
        w = np.linspace(0.0, 1.0, ov, dtype=np.float32)[:, None, None]  # <-- ключевая правка

        mix = a_slice * (1.0 - w) + b_slice * w
        out[yA:yA + ov, :, :] = np.clip(mix, 0, 255).astype(np.uint8)

    # Хвост B
    tail = b[ov:, :, :]
    if tail.size > 0:
        out[yA + ov:yA + ov + tail.shape[0], :, :] = tail

    return out

def _compute_offsets(superframes):
    offs = [0]
    for img in superframes:
        offs.append(offs[-1] + img.shape[0])
    return offs  # длина = len(superframes)+1

def _get_row(superframes, offsets, y_global):
    """Вернуть одну горизонтальную строку (H=1, W, 3) по глобальной координате y."""
    # находим суперкадр
    import bisect
    si = bisect.bisect_right(offsets, y_global) - 1
    si = max(0, min(si, len(superframes) - 1))
    y_local = y_global - offsets[si]
    row = superframes[si][y_local:y_local+1, :, :]  # (1, W, 3)
    return row

def _row_is_uniform(row, tol=2):
    """
    row: (1, W, 3) uint8. Возвращает True, если все пиксели одной строки ≈ одного цвета.
    Критерий: для каждого канала (max-min) ≤ tol.
    """
    # приводим к (W, 3)
    r = row.reshape(-1, row.shape[2])
    ptp = r.max(axis=0).astype(np.int32) - r.min(axis=0).astype(np.int32)
    return int(ptp.max()) <= int(tol)

def _has_uniform_band(superframes, offsets, y0, band_rows=4, tol=2, total_height=None):
    """
    Проверить, что строки [y0, y0+band_rows) существуют и КАЖДАЯ строка одноцветна.
    """
    if total_height is None:
        total_height = offsets[-1]
    if y0 < 0 or y0 + band_rows > total_height:
        return False
    for yy in range(y0, y0 + band_rows):
        row = _get_row(superframes, offsets, yy)
        if not _row_is_uniform(row, tol=tol):
            return False
    return True

def refine_cuts_to_uniform_bands(superframes, cuts, Hmax,
                                 band_rows=4, tol=2, search_radius=1500, prefer_up_first=True):
    """
    Для каждого внутреннего разреза ищем ближайшую «полосу» из band_rows одноцветных строк.
    Ищем в обе стороны от исходной позиции; вниз ограничиваемся prev+Hmax.
    Возвращает новый список cuts.
    """
    offsets = _compute_offsets(superframes)
    total_height = offsets[-1]
    new_cuts = [cuts[0]]

    for i in range(1, len(cuts) - 1):
        prev = new_cuts[-1]
        target = cuts[i]
        # Вниз не заходим за лимит Hmax
        max_down = min(prev + Hmax, total_height - band_rows)

        best = target
        found = False

        # Поиск вокруг target с нарастающим радиусом
        for d in range(0, search_radius + 1):
            cand_up = target - d
            cand_dn = target + d

            # Порядок проверки: приоритезируем вверх (как правило безопаснее), можно поменять
            order = [(cand_up, True), (cand_dn, False)] if prefer_up_first else [(cand_dn, False), (cand_up, True)]

            for cand, is_up in order:
                if cand < prev + 1:  # нельзя «перепрыгнуть» назад
                    continue
                if cand > max_down:  # нельзя превысить Hmax для текущего сегмента
                    continue
                if _has_uniform_band(superframes, offsets, cand, band_rows=band_rows, tol=tol, total_height=total_height):
                    best = int(cand)
                    found = True
                    break
            if found:
                break

        if not found:
            # Не нашли полосу — оставляем исходный разрез
            best = target

        if best - prev > Hmax:
            # на всякий случай: отрезали слишком длинно — притянем к максимуму
            best = prev + Hmax
        # перед new_cuts.append(best)
        print(f"Cut {i}: {target} -> {best} (shift {best-target:+d}){' [uniform]' if found else 'Место не найдено'}")


        new_cuts.append(best)

    new_cuts.append(cuts[-1])
    return new_cuts



def stitch_sequence(images: List[np.ndarray]) -> List[np.ndarray]:
    """
    Склеиваем подряд идущие изображения, где есть подтверждённое продолжение контента.
    Если валидация провалилась — конкатенируем без смешивания.
    """
    stitched = []
    i = 0
    while i < len(images):
        cur = images[i]
        i += 1
        while i < len(images):
            a = cur
            b = images[i]
            a_gray = cv2.cvtColor(a, cv2.COLOR_BGR2GRAY)
            b_gray = cv2.cvtColor(b, cv2.COLOR_BGR2GRAY)

            ok, yA, score, ov = find_vertical_alignment(a_gray, b_gray)
            if not ok:
                break

            # Доп. быстрая проверка на реальную идентичность в цвете (не только в gray)
            bandA = a[yA:yA+ov, :, :]
            bandB = b[:ov, :, :]
            # средняя абсолютная разница по цвету
            col_mad = float(np.median(np.abs(bandA.astype(np.int16) - bandB.astype(np.int16))))
            if col_mad <= 7.0:
                # действительно одинаковые полосы -> делаем мягкое смешивание (как раньше)
                cur = blend_vertical(a, b, yA, ov)
            else:
                # полосы отличаются — шоклей без смешивания, просто «обрезаем» дублирующую часть у B
                out_h = max(a.shape[0], yA + b.shape[0])
                out = np.zeros((yA + b.shape[0], a.shape[1], 3), dtype=np.uint8)
                out[:a.shape[0]] = a
                out[yA:yA + b.shape[0]] = b
                cur = out
            i += 1
        stitched.append(cur)
    return stitched


# --------------------------- Карта «безопасных» строк ---------------------------

def row_cost_map(gray: np.ndarray) -> Tuple[np.ndarray, np.ndarray, np.ndarray]:
    """
    Возвращает:
      grad_mean_norm (по строкам), bright_mean_norm (по строкам), итоговая cost в [0..1]
    """
    # Градиенты
    gx = cv2.Sobel(gray, cv2.CV_32F, 1, 0, ksize=3)
    gy = cv2.Sobel(gray, cv2.CV_32F, 0, 1, ksize=3)
    mag = cv2.magnitude(gx, gy)
    grad_row = mag.mean(axis=1)

    # Нормировки по перцентилям (устойчиво к выбросам)
    def robust_norm(v):
        v = v.astype(np.float32)
        hi = np.percentile(v, 95.0)
        if hi <= 1e-6:
            hi = 1.0
        vv = np.clip(v / hi, 0.0, 1.0)
        return vv

    grad_norm = robust_norm(grad_row)

    bright_mean = gray.mean(axis=1)  # [0..255]
    bright_norm = np.clip(bright_mean / 255.0, 0.0, 1.0)

    # Стоимость: много границ + тёмно → дорого; белые пустоты → дёшево
    w_grad = 0.75
    w_dark = 0.25
    cost = np.clip(w_grad * grad_norm + w_dark * (1.0 - bright_norm), 0.0, 1.0)
    return grad_norm, bright_norm, cost


def safe_rows(gray: np.ndarray, stride: int = 32) -> Tuple[np.ndarray, np.ndarray]:
    """
    Находит «безопасные» строки для разреза.
    Возвращает индексы безопасных строк и локальную стоимость у них.
    """
    _, bright_norm, cost = row_cost_map(gray)

    # Признак «белой прокладки»: очень светло и слабо меняется по ширине
    row_std = gray.std(axis=1)
    white_pad = (bright_norm >= 0.95) & (row_std <= 3.0)

    # Порог: 30-й перцентиль по стоимости
    tau = float(np.quantile(cost, 0.30))
    safe_mask = (cost <= tau) | white_pad

    idx = np.flatnonzero(safe_mask)
    if idx.size == 0:
        # крайний случай: просто равномерная сетка
        idx = np.arange(0, gray.shape[0], stride, dtype=np.int32)

    # Редуцируем частоту (stride), но оставляем «супер-безопасные» (белые прокладки)
    strong = np.flatnonzero(white_pad)
    keep = set(int(x) for x in strong.tolist())
    out_idx = []
    for y in idx:
        if (y % stride) == 0 or (y in keep):
            out_idx.append(int(y))
    out_idx = np.array(sorted(set(out_idx)), dtype=np.int32)

    # локальная стоимость в этих точках
    local_cost = cost[out_idx]
    return out_idx, local_cost


# --------------------------- Глобальная разметка и разбиение ---------------------------

def build_candidates(superframes: List[np.ndarray]) -> Tuple[np.ndarray, np.ndarray, Dict[int, float]]:
    """
    Строим глобальный список кандидатных позиций для разреза и карту локальных стоимостей.
    Возвращает:
      positions (int, px от 0 до L),
      sf_index_of_position (на какой суперкадр приходится позиция),
      cost_at_pos (dict: глобальная_строка -> стоимость)
    """
    positions = [0]
    sf_map = [0]
    cost_map: Dict[int, float] = {0: 0.0}
    offset = 0
    for si, img in enumerate(superframes):
        gray = cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)
        idx, locost = safe_rows(gray, stride=32)
        for y, c in zip(idx.tolist(), locost.tolist()):
            positions.append(offset + y)
            sf_map.append(si)
            cost_map[offset + y] = float(c)
        # обязательно добавляем границу между суперкадрами как безопасную
        offset += img.shape[0]
        positions.append(offset)
        sf_map.append(si)
        cost_map[offset] = 0.0

    positions = np.array(sorted(set(positions)), dtype=np.int32)
    # перестроим карту индексов суперкадра для каждой позиции
    sf_index_of_position = np.zeros_like(positions)
    offset = 0
    si = 0
    for j, p in enumerate(positions):
        while si < len(superframes) and p > offset + superframes[si].shape[0]:
            offset += superframes[si].shape[0]
            si += 1
        sf_index_of_position[j] = min(si, len(superframes) - 1)
    return positions, sf_index_of_position, cost_map


def greedy_cut_positions(positions: np.ndarray,
                         cost_map: Dict[int, float],
                         total_height: int,
                         K: int,
                         Hmax: int) -> List[int]:
    """
    Жадное разбиение: ставим границы близко к идеальным (кратно T), но не позже, чем Hmax,
    выбираем среди безопасных позиций с минимальным (β*cost + α*отклонение).
    """
    assert positions[0] == 0 and positions[-1] == total_height
    cuts = [0]
    T = total_height / float(K)
    alpha = 1.0
    beta = 0.6
    delta = 3000  # можно двигаться на ±delta от идеала

    # Быстрый доступ к ближайшим позициям в диапазоне
    def range_indices(lo, hi):
        # индексы позиций, попадающих в [lo, hi]
        left = int(np.searchsorted(positions, lo, side='left'))
        right = int(np.searchsorted(positions, hi, side='right'))
        return range(left, right)

    for part in range(1, K):
        prev = cuts[-1]
        ideal = int(round(part * T))
        lo = int(prev + max(0.5 * T, 1))  # минимальная длина сегмента ~50% T
        hi = min(prev + Hmax, total_height)

        # сначала ищем в окне вокруг идеала, затем расширяем
        win_lo = max(lo, ideal - delta)
        win_hi = min(hi, ideal + delta)

        candidates = list(range_indices(win_lo, win_hi))
        if not candidates:
            candidates = list(range_indices(lo, hi))
        if not candidates:
            # крайний случай: ставим ровно Hmax или до конца
            next_cut = min(prev + Hmax, total_height)
            cuts.append(next_cut)
            continue

        best_j = None
        best_score = float('inf')
        for j in candidates:
            pos = int(positions[j])
            if pos <= prev or pos > hi:
                continue
            length = pos - prev
            if length <= 0:
                continue
            # функция стоимости
            c = cost_map.get(pos, 0.0)
            score = beta * c + alpha * (abs(pos - ideal) / max(T, 1.0))
            if score < best_score:
                best_score = score
                best_j = j

        if best_j is None:
            # fallback
            next_cut = min(prev + Hmax, total_height)
        else:
            next_cut = int(positions[best_j])

        cuts.append(next_cut)

    # последний — конец
    if cuts[-1] != total_height:
        cuts.append(total_height)
    return cuts


# --------------------------- Сохранение сегментов ---------------------------

def save_segments(superframes: List[np.ndarray], cuts: List[int], out_dir: Path):
    """
    Вырезает и сохраняет сегменты по глобальным координатам cuts (высоты в px).
    """
    out_dir.mkdir(parents=True, exist_ok=True)
    # очистим старые файлы merged_*.png
    for p in out_dir.glob("merged_*.png"):
        try:
            p.unlink()
        except Exception:
            pass

    # кумулятивные смещения суперкадров
    offsets = [0]
    for img in superframes:
        offsets.append(offsets[-1] + img.shape[0])

    def grab_slice(y0: int, y1: int) -> np.ndarray:
        """Вернёт вертикальный срез (y0: y1) из списка суперкадров (склеит при необходимости)."""
        parts = []
        for si, img in enumerate(superframes):
            sf_start = offsets[si]
            sf_end = offsets[si + 1]
            # пересечение диапазонов
            yy0 = max(y0, sf_start)
            yy1 = min(y1, sf_end)
            if yy1 > yy0:
                a = yy0 - sf_start
                b = yy1 - sf_start
                parts.append(img[a:b, :, :])
        if not parts:
            # пустое — должно быть невозможно
            return np.zeros((1, superframes[0].shape[1], 3), dtype=np.uint8)
        if len(parts) == 1:
            return parts[0].copy()
        return np.vstack(parts)

    # сохранить
    n_parts = len(cuts) - 1
    width = superframes[0].shape[1]
    for i in range(n_parts):
        y0, y1 = cuts[i], cuts[i + 1]
        # пропускаем нулевую высоту
        if y1 <= y0:
            continue
        seg = grab_slice(y0, y1)
        # защита на неверные ширины
        if seg.shape[1] != width:
            seg = cv2.resize(seg, (width, seg.shape[0]), interpolation=cv2.INTER_AREA)
        out_path = out_dir / f"{i + 1:02d}.png"
        cv2.imwrite(str(out_path), seg, [cv2.IMWRITE_PNG_COMPRESSION, 3])
        print(f"Сохранено: {out_path.name} (h={seg.shape[0]})")
def _second_best_value(res: np.ndarray, best_xy: Tuple[int,int], win: int = 15) -> float:
    """
    Возвращает второй максимум карты сопоставления, исключая окрестность (win) вокруг лучшего.
    """
    res2 = res.copy()
    x, y = best_xy[0], best_xy[1]
    y0, y1 = max(0, y - win), min(res2.shape[0], y + win + 1)
    x0, x1 = max(0, x - win), min(res2.shape[1], x + win + 1)
    res2[y0:y1, x0:x1] = -1.0  # TM_CCOEFF_NORMED ∈ [-1..1]
    return float(res2.max())

def _mad(a: np.ndarray, b: np.ndarray) -> float:
    """Median Absolute Deviation между двумя uint8 срезами одинаковой формы."""
    d = (a.astype(np.int16) - b.astype(np.int16))
    return float(np.median(np.abs(d)))

def _ssim_gray(a_gray: np.ndarray, b_gray: np.ndarray) -> float:
    """Приближённый SSIM для серых изображений одинаковой формы (без внешних библиотек)."""
    # упрощённый вариант: нормируем, метрика по яркости/контрасту/ковариации
    a = a_gray.astype(np.float32)
    b = b_gray.astype(np.float32)
    c1, c2 = 6.5025, 58.5225
    mu_a = a.mean(); mu_b = b.mean()
    sig_a = a.var();  sig_b = b.var()
    cov   = ((a - mu_a) * (b - mu_b)).mean()
    num   = (2*mu_a*mu_b + c1) * (2*cov + c2)
    den   = (mu_a**2 + mu_b**2 + c1) * (sig_a + sig_b + c2)
    return float(num / den) if den > 1e-6 else 0.0

# --------------------------- Главный сценарий ---------------------------
def main_process(
    images: list,                     # List[np.ndarray] BGR
    K: int = None,                    # Кол-во частей; если None — подберётся автоматически
    *,
    Hmax: int = 19000,                # Жёсткий лимит высоты одной части (как TARGET в main)
    band_rows: int = 4,               # Полоска «одноцветности» (строк)
    tol: int = 15,                    # Допуск одноцветности для refine
    search_radius: int = 5500,        # Радиус поиска при refine
    prefer_up_first: bool = True,     # Сначала пытаться вверх при refine
    verbose: bool = True              # Печатать шаги
) -> list:
    """
    Обрабатывает изображения «в памяти»: выравнивает ширины, сшивает контент,
    выбирает безопасные разрезы и возвращает список сегментов как np.ndarray (BGR).
    Никаких операций с файлами.
    """

    if not images:
        return []

    # 1) Приводим к общей ширине (к моде ширин)
    images, width = unify_widths(images)
    if verbose:
        print(f"[main_process] Приведённая ширина: {width}px")

    # 2) Сшивка подряд идущих страниц, где подтверждено продолжение контента
    if verbose:
        print("[main_process] Сшивка по содержимому...")
    superframes = stitch_sequence(images)

    # 3) Статистика
    heights = [im.shape[0] for im in superframes]
    total_height = int(sum(heights))
    if verbose:
        print(f"[main_process] Суперкадров: {len(superframes)}; высоты: {heights}")
        print(f"[main_process] Общая высота: {total_height} px")

    # 4) Выбор K (если не задан): как в main — по Hmax
    if K is None:
        # рекомендация ≈ ceil(total_height / Hmax)
        K = max(1, int(math.ceil(total_height / float(Hmax))))
        if verbose:
            print(f"[main_process] Авто-выбор K={K} при Hmax={Hmax}")

    # Жёсткая проверка по лимиту высоты
    min_required = int(math.ceil(total_height / float(Hmax)))
    if K < min_required:
        if verbose:
            print(f"[main_process] Предупреждение: K={K} < необходимого {min_required} "
                  f"для лимита {Hmax}px. Увеличиваю K.")
        K = min_required

    # 5) Кандидаты и жадное разбиение
    if verbose:
        print("[main_process] Поиск безопасных мест для разрезов...")
    positions, sf_map, cost_map = build_candidates(superframes)
    cuts = greedy_cut_positions(positions, cost_map, total_height, K, Hmax)

    # 6) Коррекция: подрезаем, чтобы ни один сегмент (кроме последнего) не превышал Hmax
    fixed = [cuts[0]]
    for i in range(1, len(cuts)):
        prev = fixed[-1]
        cur = cuts[i]
        is_last = (i == len(cuts) - 1)
        if (not is_last) and (cur - prev > Hmax):
            hi = prev + Hmax
            idxs = np.where((positions > prev) & (positions <= hi))[0]
            if idxs.size:
                best_j = idxs[np.argmin([cost_map.get(int(positions[j]), 0.0) for j in idxs])]
                cur = int(positions[int(best_j)])
            else:
                cur = hi
        fixed.append(cur)
    cuts = fixed
    # 7) Притягивание к одноцветным полосам (уточнение разрезов)
    if verbose:
        print(f"[main_process] Уточняю разрезы до одноцветных полос "
              f"({band_rows} строк; tol={tol}; R={search_radius})...")
    cuts = refine_cuts_to_uniform_bands(
        superframes,
        cuts,
        Hmax=Hmax,
        band_rows=band_rows,
        tol=tol,
        search_radius=search_radius,
        prefer_up_first=prefer_up_first
    )

    # 8) Вырезаем сегменты (без сохранения на диск)
    #    Логика соответствует save_segments, но возвращаем список np.ndarray.
    offsets = [0]
    for img in superframes:
        offsets.append(offsets[-1] + img.shape[0])

    def grab_slice(y0: int, y1: int) -> np.ndarray:
        parts = []
        for si, img in enumerate(superframes):
            sf_start = offsets[si]
            sf_end = offsets[si + 1]
            yy0 = max(y0, sf_start)
            yy1 = min(y1, sf_end)
            if yy1 > yy0:
                a = yy0 - sf_start
                b = yy1 - sf_start
                parts.append(img[a:b, :, :])
        if not parts:
            return np.zeros((1, superframes[0].shape[1], 3), dtype=np.uint8)
        if len(parts) == 1:
            return parts[0].copy()
        return np.vstack(parts)

    segments = []
    n_parts = len(cuts) - 1
    for i in range(n_parts):
        y0, y1 = cuts[i], cuts[i + 1]
        if y1 <= y0:
            continue
        seg = grab_slice(y0, y1)
        # защита на неверные ширины
        if seg.shape[1] != width:
            seg = cv2.resize(seg, (width, seg.shape[0]), interpolation=cv2.INTER_AREA)
        if verbose:
            print(f"[main_process] Сегмент {i+1:02d}: h={seg.shape[0]}")
        segments.append(seg)

    if verbose:
        print("[main_process] Готово.")
    return segments

def main():
    folder = ask_path()
    files = list_images_sorted(folder)
    print(f"Найдено файлов: {len(files)}")
    images = [read_bgr(p) for p in files]
    images, width = unify_widths(images)
    print(f"Ширина приведена к: {width}px")

    # Сшивка последовательности
    print("Сшивка по содержимому...")
    superframes = stitch_sequence(images)

    # Статистика
    heights = [im.shape[0] for im in superframes]
    total_height = int(sum(heights))
    print(f"Суперкадров после сшивки: {len(superframes)}")
    print("Высоты суперкадров:", heights)
    print(f"Общая высота главы: {total_height} px")

    # Рекомендация по количеству частей
    TARGET = 19000  # «удобная» высота и жёсткий лимит
    suggest_K = max(1, int(round(total_height / float(TARGET)))) or 1
    if suggest_K * TARGET < total_height:  # на всякий случай
        suggest_K = int(math.ceil(total_height / float(TARGET)))
    try:
        txt = input(f"На сколько частей разбить? (Enter = {suggest_K} ≈ {TARGET}px каждая): ").strip()
    except EOFError:
        txt = ""
    if txt:
        try:
            K = int(txt)
        except ValueError:
            print("Некорректное число. Использую рекомендацию.")
            K = suggest_K
    else:
        K = suggest_K

    # Жёсткая проверка по лимиту высоты
    Hmax = TARGET
    min_required = int(math.ceil(total_height / float(Hmax)))
    if K < min_required:
        print(f"Предупреждение: при K={K} не уложиться в лимит {Hmax}px. "
              f"Увеличиваю до K={min_required}.")
        K = min_required

    # Подготовка кандидатов и разбиение
    print("Поиск безопасных мест для разрезов...")
    positions, sf_map, cost_map = build_candidates(superframes)
    cuts = greedy_cut_positions(positions, cost_map, total_height, K, Hmax)

    # Коррекция последнего отрезка (на всякий случай не превышать Hmax)
        # Коррекция: ограничиваем Hmax все сегменты, КРОМЕ последнего
    fixed = [cuts[0]]
    for i in range(1, len(cuts)):
        prev = fixed[-1]
        cur = cuts[i]
        is_last = (i == len(cuts) - 1)
        if (not is_last) and (cur - prev > Hmax):
            hi = prev + Hmax
            idxs = np.where((positions > prev) & (positions <= hi))[0]
            if idxs.size:
                best_j = idxs[np.argmin([cost_map.get(int(positions[j]), 0.0) for j in idxs])]
                cur = int(positions[int(best_j)])
            else:
                cur = hi
        fixed.append(cur)

    # >>> ВАЖНО: использовать исправленные разрезы <<<
    cuts = fixed


    # Уточняем разрезы: притягиваем к полосам из 4 одноцветных строк
    print("Уточняю разрезы до ближайших одноцветных полос (4 строки)...")
    cuts = refine_cuts_to_uniform_bands(
        superframes,
        cuts,
        Hmax=Hmax,
        band_rows=4,
        tol=15,
        search_radius=5500,
        prefer_up_first=True
    )

    # Сохранение
    out_dir = folder / "merged"
    print(f"Сохраняю части в: {out_dir}")
    save_segments(superframes, cuts, out_dir)

    print("Готово.")


if __name__ == "__main__":
    main()
