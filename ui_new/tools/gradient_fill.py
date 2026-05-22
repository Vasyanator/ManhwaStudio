# ui_new/tools/gradient_fill.py
# Инструмент градиентной заливки на основе region_edit_base
from __future__ import annotations
from typing import Optional

import numpy as np

from PyQt6.QtGui import QImage
from PyQt6.QtWidgets import QLabel, QVBoxLayout, QHBoxLayout, QPushButton, QGroupBox, QWidget

from .base import RegionEditTool, RegionEditorDialog


# ---------- ЦВЕТ: sRGB <-> CIE Lab ----------
def _srgb_to_linear(c: np.ndarray) -> np.ndarray:
    c = c.astype(np.float32) / 255.0
    return np.where(c <= 0.04045, c / 12.92, ((c + 0.055) / 1.055) ** 2.4)


def _linear_to_srgb(c: np.ndarray) -> np.ndarray:
    """Линейный RGB -> sRGB с гаммой."""
    c = c.astype(np.float32)
    out = np.empty_like(c, dtype=np.float32)

    mask = c <= 0.0031308
    out[mask] = 12.92 * c[mask]

    pos = ~mask
    cp = np.clip(c[pos], 0.0, None)
    out[pos] = 1.055 * np.power(cp, 1.0 / 2.4, dtype=np.float32) - 0.055
    return out


def rgb_to_lab(rgb: np.ndarray):
    """RGB uint8 -> (L,a,b) float32 (Lab, D65)."""
    r_lin, g_lin, b_lin = [_srgb_to_linear(rgb[..., i]) for i in range(3)]
    # sRGB (D65) -> XYZ
    X = 0.4124564 * r_lin + 0.3575761 * g_lin + 0.1804375 * b_lin
    Y = 0.2126729 * r_lin + 0.7151522 * g_lin + 0.0721750 * b_lin
    Z = 0.0193339 * r_lin + 0.1191920 * g_lin + 0.9503041 * b_lin
    # Нормализация белой точки D65
    Xn, Yn, Zn = 0.95047, 1.0, 1.08883
    x, y, z = X / Xn, Y / Yn, Z / Zn

    eps = 216 / 24389
    kappa = 24389 / 27

    def f(t):
        return np.where(t > eps, np.cbrt(t), (kappa * t + 16.0) / 116.0)

    fx, fy, fz = f(x), f(y), f(z)
    L = 116.0 * fy - 16.0
    a = 500.0 * (fx - fy)
    b = 200.0 * (fy - fz)
    return L.astype(np.float32), a.astype(np.float32), b.astype(np.float32)


def lab_to_rgb(L: np.ndarray, a: np.ndarray, b: np.ndarray,
               dither_eps: float = 0.0, dither_mask: Optional[np.ndarray] = None) -> np.ndarray:
    """(L,a,b) float32 -> RGB uint8 (sRGB, D65) + опциональный дизеринг."""
    L = L.astype(np.float32)
    a = a.astype(np.float32)
    b = b.astype(np.float32)

    fy = (L + 16.0) / 116.0
    fx = fy + a / 500.0
    fz = fy - b / 200.0

    eps = 216 / 24389
    kappa = 24389 / 27

    def invf(f):
        return np.where(f**3 > eps, f**3, (116.0 * f - 16.0) / kappa)

    x, y, z = invf(fx), invf(fy), invf(fz)
    Xn, Yn, Zn = 0.95047, 1.0, 1.08883
    X, Y, Z = x * Xn, y * Yn, z * Zn

    # XYZ -> линейный RGB
    r_lin =  3.2404542 * X - 1.5371385 * Y - 0.4985314 * Z
    g_lin = -0.9692660 * X + 1.8760108 * Y + 0.0415560 * Z
    b_lin =  0.0556434 * X - 0.2040259 * Y + 1.0572252 * Z

    rgb_lin = np.stack([r_lin, g_lin, b_lin], axis=-1).astype(np.float32)
    rgb = _linear_to_srgb(rgb_lin)
    rgb = np.clip(rgb, 0.0, 1.0)

    # Дизеринг
    if dither_eps and dither_eps > 0.0:
        noise = (np.random.rand(*rgb.shape).astype(np.float32) - 0.5) * (2.0 * (dither_eps / 255.0))
        if dither_mask is not None:
            noise *= dither_mask[..., None].astype(np.float32)
        rgb = np.clip(rgb + noise, 0.0, 1.0)

    return (rgb * 255.0 + 0.5).astype(np.uint8)


# ---------- АЛГОРИТМ ГРАДИЕНТНОЙ ЗАЛИВКИ ----------
def _dilate(mask: np.ndarray, iters: int = 3) -> np.ndarray:
    """Морфологическое расширение маски."""
    m = mask.astype(bool)
    for _ in range(int(iters)):
        p = m
        m = (
            p |
            np.r_[p[1:], p[-1:]] | np.r_[p[:1], p[:-1]] |
            np.c_[p[:,1:], p[:,-1:]] | np.c_[p[:,:1], p[:,:-1]] |
            np.c_[np.r_[p[1:,1:], p[-1:,1:]], np.r_[p[1:,-1:], p[-1:,-1:]]] |
            np.c_[np.r_[p[1:,:-1], p[-1:,:-1]], np.r_[p[1:, :1], p[-1:, :1]]] |
            np.c_[np.r_[p[:-1,1:], p[:1,1:]], np.r_[p[:-1,-1:], p[:1,-1:]]] |
            np.c_[np.r_[p[:-1,:-1], p[:1,:-1]], np.r_[p[:-1,:1], p[:1,:1]]]
        )
    return m


def _ring_mask(mask: np.ndarray, inner: int = 1, outer: int = 3) -> np.ndarray:
    """Узкое кольцо вокруг маски для измерения статистик по границе."""
    inner = max(1, int(inner))
    outer = max(inner, int(outer))
    m_in = _dilate(mask, iters=inner-1) if inner > 1 else mask.astype(bool)
    m_out = _dilate(mask, iters=outer)
    ring = (m_out & (~m_in))
    return ring


def screened_poisson_refine(channel_pred: np.ndarray, channel_orig: np.ndarray,
                            mask: np.ndarray, pad: int = 10,
                            lam_in: float = 1.0, lam_out: float = 80.0,
                            iters: int = 200, omega: float = 1.95) -> np.ndarray:
    """Узкополосное согласование по Пуассону."""
    H, W = channel_pred.shape
    ys, xs = np.where(mask)
    if xs.size == 0:
        return channel_pred

    x0 = max(0, xs.min() - pad)
    x1 = min(W, xs.max() + pad + 1)
    y0 = max(0, ys.min() - pad)
    y1 = min(H, ys.max() + pad + 1)
    roi = np.s_[y0:y1, x0:x1]

    m = mask[roi]
    u0 = np.where(m, channel_pred[roi], channel_orig[roi]).astype(np.float32)
    lam = np.where(m, float(lam_in), float(lam_out)).astype(np.float32)

    u = u0.copy()
    denom = (4.0 + lam).astype(np.float32)

    for _ in range(int(iters)):
        for parity in (0, 1):
            for y in range(1, u.shape[0]-1):
                xstart = 1 + ((parity - y) & 1)
                xs_ = slice(xstart, u.shape[1]-1, 2)

                nbr = (u[y, slice(xstart-1, -2, 2)] + u[y, slice(xstart+1, None, 2)] +
                       u[y-1, xs_] + u[y+1, xs_])
                rhs = nbr + lam[y, xs_] * u0[y, xs_]
                u[y, xs_] = u[y, xs_] + omega * (rhs / denom[y, xs_] - u[y, xs_])

    out = channel_pred.copy()
    out[roi] = u
    return out


def _fill_constant_from_ring(base_rgb: np.ndarray, mask: np.ndarray,
                             L: np.ndarray, A: np.ndarray, B: np.ndarray,
                             ring: np.ndarray) -> np.ndarray:
    """Заливка константным цветом из кольца."""
    Lm = np.median(L[ring]).astype(np.float32)
    Am = np.median(A[ring]).astype(np.float32)
    Bm = np.median(B[ring]).astype(np.float32)

    L_hat = L.copy()
    A_hat = A.copy()
    B_hat = B.copy()
    L_hat[mask] = Lm
    A_hat[mask] = Am
    B_hat[mask] = Bm

    L_hat = screened_poisson_refine(L_hat.astype(np.float32), L.astype(np.float32), mask,
                                    pad=12, lam_in=1.0, lam_out=120.0, iters=200)

    rgb_hat = lab_to_rgb(L_hat, A_hat, B_hat, dither_eps=0.6, dither_mask=mask)
    out = base_rgb.copy()
    out[mask] = rgb_hat[mask]
    return out


def _smoothstep(x: np.ndarray) -> np.ndarray:
    """Классическая smoothstep: 3x^2 - 2x^3."""
    x = np.clip(x.astype(np.float32), 0.0, 1.0)
    return x*x*(3.0 - 2.0*x)


def _nearest_fill_in_mask(arr: np.ndarray, M: np.ndarray, passes: int = 2):
    """Заполнение пробелов ближайшими значениями."""
    for _ in range(passes):
        nan = np.isnan(arr) & M
        if not nan.any():
            return
        v = np.nan_to_num(arr, nan=0.0).astype(np.float32)
        w = (~np.isnan(arr)).astype(np.float32)

        vsum = (np.r_[v[1:], v[-1:]] + np.r_[v[:1], v[:-1]] +
                np.c_[v[:,1:], v[:,-1:]] + np.c_[v[:,:1], v[:,:-1]])
        wsum = (np.r_[w[1:], w[-1:]] + np.r_[w[:1], w[:-1]] +
                np.c_[w[:,1:], w[:,-1:]] + np.c_[w[:,:1], w[:,:-1]])

        upd = np.divide(vsum, wsum, out=np.full_like(arr, np.nan, dtype=np.float32), where=wsum>0)
        arr[nan] = upd[nan]


def _angle_consistency_score(L: np.ndarray, A: np.ndarray, B: np.ndarray,
                             M: np.ndarray, theta_deg: float,
                             deltaE_cap: float = 10.0, v_step: int = 2,
                             t_step: float = 1.0) -> float:
    """Оценка согласованности цветов на границах вдоль направления θ."""
    pairs = _collect_boundary_pairs(L, A, B, M, theta_deg, v_step=v_step, t_step=t_step, max_pairs=300)
    if not pairs:
        return -1e9
    score = 0.0
    for (Lin, Ain, Bin, Lout, Aout, Bout) in pairs:
        dL = Lin - Lout
        da = Ain - Aout
        db = Bin - Bout
        de = float(np.sqrt(dL*dL + da*da + db*db))
        score -= min(de, deltaE_cap)
    score += 0.25 * len(pairs)
    return score


def _collect_boundary_pairs(L: np.ndarray, A: np.ndarray, B: np.ndarray,
                            M: np.ndarray, theta_deg: float,
                            v_step: int = 2, t_step: float = 1.0,
                            max_pairs: int = 500):
    """Сбор пар цветов на противоположных сторонах маски."""
    H, W = M.shape
    theta = np.deg2rad(theta_deg)
    c, s = np.cos(theta), np.sin(theta)

    corners = np.array([[0,0],[0,H-1],[W-1,0],[W-1,H-1]], dtype=np.float32)
    u_c = corners[:,0]*c + corners[:,1]*s
    v_c = -corners[:,0]*s + corners[:,1]*c
    umin, umax = float(u_c.min()), float(u_c.max())
    vmin, vmax = float(v_c.min()), float(v_c.max())

    pairs = []
    v = vmin
    while v <= vmax and len(pairs) < max_pairs:
        inside = []
        t = umin
        last_state = None
        while t <= umax:
            x = c*t - s*v
            y = s*t + c*v
            xi, yi = int(round(x)), int(round(y))
            if 0 <= xi < W and 0 <= yi < H:
                cur = bool(M[yi, xi])
                if last_state is None:
                    last_state = cur
                else:
                    if cur != last_state:
                        inside.append((t, cur))
                        last_state = cur
            t += t_step

        if len(inside) >= 2:
            t_in, t_out = None, None
            cur_state = bool(M[int(round(s*umin + c*v)) % H, int(round(c*umin - s*v)) % W])
            for (tk, st) in inside:
                if (not cur_state) and st:
                    t_in = tk
                if cur_state and (not st):
                    t_out = tk
                cur_state = st
            if t_in is not None and t_out is not None and (t_out - t_in) >= (2.0 * t_step):
                tin = t_in - 1.0
                tout = t_out + 1.0
                x_in = c*tin - s*v
                y_in = s*tin + c*v
                x_out = c*tout - s*v
                y_out = s*tout + c*v
                xi, yi = int(round(x_in)), int(round(y_in))
                xo, yo = int(round(x_out)), int(round(y_out))
                if (0 <= xi < W and 0 <= yi < H and not M[yi, xi]) and (0 <= xo < W and 0 <= yo < H and not M[yo, xo]):
                    pairs.append((float(L[yi, xi]), float(A[yi, xi]), float(B[yi, xi]),
                                  float(L[yo, xo]), float(A[yo, xo]), float(B[yo, xo])))
        v += float(v_step)
    return pairs


def _scan_fill_lines(L: np.ndarray, A: np.ndarray, B: np.ndarray,
                     M: np.ndarray, theta_deg: float,
                     v_step: int, t_step: float, deltaE_thr: float,
                     outL: np.ndarray, outA: np.ndarray, outB: np.ndarray):
    """Окраска линиями под углом theta_deg."""
    H, W = M.shape
    theta = np.deg2rad(theta_deg)
    c, s = np.cos(theta), np.sin(theta)
    corners = np.array([[0,0],[0,H-1],[W-1,0],[W-1,H-1]], dtype=np.float32)
    u_c = corners[:,0]*c + corners[:,1]*s
    v_c = -corners[:,0]*s + corners[:,1]*c
    umin, umax = float(u_c.min()), float(u_c.max())
    vmin, vmax = float(v_c.min()), float(v_c.max())

    v = vmin
    while v <= vmax:
        ts = np.arange(umin, umax + 0.5*t_step, t_step, dtype=np.float32)
        xs = c*ts - s*v
        ys = s*ts + c*v
        xi = np.round(xs).astype(np.int32)
        yi = np.round(ys).astype(np.int32)
        valid = (xi >= 0) & (xi < W) & (yi >= 0) & (yi < H)
        if not np.any(valid):
            v += float(v_step)
            continue

        xi = xi[valid]
        yi = yi[valid]
        ts = ts[valid]
        inside = M[yi, xi]
        if inside.sum() == 0:
            v += float(v_step)
            continue

        idx = np.where(inside)[0]
        t0, t1 = int(idx[0]), int(idx[-1])

        pre_idx = max(t0 - 1, 0)
        post_idx = min(t1 + 1, len(ts)-1)
        xpre, ypre = xi[pre_idx], yi[pre_idx]
        xpost, ypost = xi[post_idx], yi[post_idx]
        if M[ypre, xpre] or M[ypost, xpost]:
            v += float(v_step)
            continue

        Lin, Ain, Bin = float(L[ypre, xpre]), float(A[ypre, xpre]), float(B[ypre, xpre])
        Lout, Aout, Bout = float(L[ypost, xpost]), float(A[ypost, xpost]), float(B[ypost, xpost])

        dL = Lin - Lout
        da = Ain - Aout
        db = Bin - Bout
        deltaE = float(np.sqrt(dL*dL + da*da + db*db))

        x_in = xi[t0:t1+1]
        y_in = yi[t0:t1+1]
        seg_len = max(1, (t1 - t0))
        alphas = np.linspace(0.0, 1.0, seg_len+1, dtype=np.float32)

        if deltaE <= deltaE_thr:
            Lc = 0.5*(Lin + Lout)
            Ac = 0.5*(Ain + Aout)
            Bc = 0.5*(Bin + Bout)
            outL[y_in, x_in] = Lc
            outA[y_in, x_in] = Ac
            outB[y_in, x_in] = Bc
        else:
            w = _smoothstep(alphas)
            Ls = (1.0 - w)*Lin + w*Lout
            As = (1.0 - w)*Ain + w*Aout
            Bs = (1.0 - w)*Bin + w*Bout
            outL[y_in, x_in] = Ls
            outA[y_in, x_in] = As
            outB[y_in, x_in] = Bs

        v += float(v_step)


def _scanlines_parallel_lab(base_rgb: np.ndarray, mask_a: np.ndarray) -> np.ndarray:
    """Восстановление под маской тонкими параллельными линиями."""
    H, W, _ = base_rgb.shape
    mask = (mask_a > 0)
    if not np.any(mask):
        return base_rgb

    L, A, B = rgb_to_lab(base_rgb)

    ys, xs = np.where(mask)
    pad = 16
    x0 = max(0, xs.min() - pad)
    x1 = min(W, xs.max() + pad + 1)
    y0 = max(0, ys.min() - pad)
    y1 = min(H, ys.max() + pad + 1)
    roi = np.s_[y0:y1, x0:x1]
    if (x1 - x0) < 2 or (y1 - y0) < 2:
        return base_rgb

    M = mask[roi]
    Lr, Ar, Br = L[roi], A[roi], B[roi]

    # Поиск угла
    angle_step = 3
    deltaE_thr = 2.5
    best_score, best_theta = -1e18, 0.0

    gLy, gLx = np.gradient(L.astype(np.float32))
    ring = _ring_mask(mask, inner=1, outer=3)
    if np.any(ring):
        gv = np.stack([gLx[ring], gLy[ring]], axis=-1).astype(np.float32)
        if gv.size:
            v_hint = gv.mean(axis=0)
            if np.linalg.norm(v_hint) > 1e-6:
                t = np.array([-v_hint[1], v_hint[0]], dtype=np.float32)
                ang = float(np.degrees(np.arctan2(t[1], t[0])) % 180.0)
                primary_angles = [((ang + d) % 180.0) for d in (-9, -6, -3, 0, 3, 6, 9)]
            else:
                primary_angles = []
        else:
            primary_angles = []
    else:
        primary_angles = []

    tested = set()
    def try_angles(seq):
        nonlocal best_score, best_theta
        for ang in seq:
            a = int(round(ang)) % 180
            if a in tested:
                continue
            tested.add(a)
            score = _angle_consistency_score(Lr, Ar, Br, M, theta_deg=a,
                                            deltaE_cap=10.0, v_step=2, t_step=1.0)
            if score > best_score:
                best_score, best_theta = score, float(a)

    try_angles(primary_angles)
    try_angles(range(0, 180, angle_step))

    # Заливка линиями
    v_step = 1
    t_step = 1.0
    fill_L = np.full_like(Lr, np.nan, dtype=np.float32)
    fill_A = np.full_like(Ar, np.nan, dtype=np.float32)
    fill_B = np.full_like(Br, np.nan, dtype=np.float32)

    _scan_fill_lines(Lr, Ar, Br, M, best_theta, v_step, t_step, deltaE_thr,
                     outL=fill_L, outA=fill_A, outB=fill_B)

    _nearest_fill_in_mask(fill_L, M)
    _nearest_fill_in_mask(fill_A, M)
    _nearest_fill_in_mask(fill_B, M)

    if np.isnan(fill_L[M]).all():
        return _fill_constant_from_ring(base_rgb, mask, L, A, B, ring if ring.any() else _ring_mask(mask, 1, 4))

    lam_in = 1.0
    lam_out = 120.0
    iters = 220

    L_ref = L.copy()
    L_ref[roi][M] = fill_L[M]
    L_ref = screened_poisson_refine(L_ref.astype(np.float32), L.astype(np.float32), mask,
                                    pad=pad+4, lam_in=lam_in, lam_out=lam_out, iters=iters)

    A_ref = A.copy()
    B_ref = B.copy()
    A_ref[roi][M] = fill_A[M]
    B_ref[roi][M] = fill_B[M]
    rgb_hat = lab_to_rgb(L_ref, A_ref, B_ref, dither_eps=0.6, dither_mask=mask)

    out = base_rgb.copy()
    out[mask] = rgb_hat[mask]
    return out


# ---------- ДИАЛОГ РЕДАКТОРА С ГРАДИЕНТНОЙ ЗАЛИВКОЙ ----------
class GradientFillEditorDialog(RegionEditorDialog):
    """Диалог редактирования с градиентной заливкой."""
    def __init__(self, image: QImage, parent: Optional[QWidget] = None):
        super().__init__(image, parent)
        self.setWindowTitle("Градиентная заливка")

    def process_button_text(self) -> str:
        return "Обработать градиентом"

    def info_text(self) -> str:
        return "Градиентная заливка по маске"

    def build_params_block(self) -> QWidget:
        panel = QGroupBox("Маска")
        v = QVBoxLayout(panel)

        hint = QLabel("ЛКМ — рисовать маску, ПКМ — стирать. Shift+Колесо — радиус кисти.")
        hint.setStyleSheet("color:#666;")
        v.addWidget(hint)

        mask_row = QHBoxLayout()
        btn_clear = QPushButton("Очистить маску")
        btn_clear.clicked.connect(self.canvas.clear_mask)
        btn_inv = QPushButton("Инвертировать маску")
        btn_inv.clicked.connect(self.canvas.invert_mask)
        mask_row.addWidget(btn_clear)
        mask_row.addWidget(btn_inv)
        v.addLayout(mask_row)
        v.addStretch(1)
        return panel

    def run(self, base_rgb: np.ndarray, mask_a: np.ndarray):
        result_rgb = _scanlines_parallel_lab(base_rgb, mask_a)
        self.set_status("✅ Градиент рассчитан")
        return result_rgb


# ---------- ИНСТРУМЕНТ ----------
class GradientFillTool(RegionEditTool):
    """
    Инструмент градиентной заливки.

    Жест:
      • Shift + ЛКМ — прямоугольное выделение на одной картинке.
      • Откроется редактор с возможностью рисования маски и градиентной заливки.
    """
    tool_id = "gradient_fill"
    title = "Градиент"

    def create_editor_dialog(self, image: QImage, parent: Optional[QWidget] = None) -> GradientFillEditorDialog:
        """Создаёт диалог градиентной заливки."""
        return GradientFillEditorDialog(image, parent)
