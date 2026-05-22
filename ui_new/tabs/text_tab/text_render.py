from PyQt6.QtCore import Qt, QPointF, QBuffer, QIODevice
from PyQt6.QtGui import (
    QImage, QColor, QFont, QTextOption, QTextLayout, QTextLine,
    QPainter, QPainterPath, QPen, QBrush, QLinearGradient, QTransform, QFontMetricsF, QTextDocument
)
from PIL import Image, ImageFilter, ImageChops
import math
import io
from PIL import Image, ImageFilter
import re
from modules.smart_hyphenate import smart_hyphenate
SOFT_HYPHEN = "\u00AD"
class Renderer:
    DEBUG = False  # можно переключать из GUI

    def big_renderer(
        self,
        text: str,
        font_family: str,
        font_px: int,
        color: QColor,
        width: int,
        line_spacing_px: int,
        line_spacing_percent: float = 0.0,
        *,
        align: str = "center",  # left|center|right|justify
        stroke_color: QColor | None = None,
        stroke_width: int = 0,
        glow_color: QColor | None = None,
        glow_radius: int = 0,
        glow_softness: int = 0,   
        shadow_offset: tuple[int, int] | None = None,
        shadow_color: QColor | None = None,
        gradient: tuple[QColor, QColor] | None = None,
        gradient_angle_deg: float = 90.0,
        gradient4: dict[str, QColor] | None = None,
        extra_vpadding: int = 2,
        reflect: str | None = None,  # None | "x" | "y"
        text_shape: str = "rectangle", # "rectangle" | "oval" | "hexagon"
        shake: dict[str, float] | None = None,   # {"angle_deg": 90, "up": 0, "down": 40, "steps": 12, "base_fade": 0.30, "decay": 0.15, "blur": 2}
        effects_order: list[str] | tuple[str, ...] | None = None,  # sequence of effects: shadow|glow|stroke|fill|shake
    ) -> QImage:
        # DEBUG флаг: либо глобальный, либо self.DEBUG
        try:
            DEBUG = bool(globals().get("DEBUG", getattr(self, "DEBUG", False)))
        except Exception:
            DEBUG = False


        if DEBUG:
            print("\n=== big_renderer ===")
            print(f"text[:80]={repr((text or '')[:80])}  len={len(text or '')}")
            print(f"font_family={font_family!r}  font_px={font_px}  width={width}  line_spacing_px={line_spacing_px}")
            print(f"align={align!r}  reflect={reflect!r}")
            try: print(f"color={color.name()} a={color.alpha()}")
            except Exception: pass
            if stroke_color:  print(f"stroke_color={stroke_color.name()}  stroke_width={stroke_width}")
            if glow_color:    print(f"glow_color={glow_color.name()}  glow_radius={glow_radius}")
            if shadow_offset or shadow_color:
                print(f"shadow_offset={shadow_offset}  shadow_color={shadow_color.name() if shadow_color else None}")
            if gradient:      print(f"gradient=({gradient[0].name()}, {gradient[1].name()})")
            print(f"extra_vpadding={extra_vpadding}")
            print(f"text_shape={text_shape!r}" )

        # 1) Шрифт в ПИКСЕЛЯХ для корректной верстки
        qfont = QFont(font_family)
        qfont.setPixelSize(int(max(1, font_px)))

        fm = QFontMetricsF(qfont)

        # --- Верстка текста ---
        opt = QTextOption()
        opt.setWrapMode(QTextOption.WrapMode.WordWrap)

        # Выравнивание: justify отдаём layout'у; left/center/right делаем сами через x0
        if align == "justify":
            opt.setAlignment(Qt.AlignmentFlag.AlignJustify)
        else:
            opt.setAlignment(Qt.AlignmentFlag.AlignLeft)

        def _compute_line_widths(n: int, base_w: int, shape: str, min_ratio: float = 0.5) -> list[float]:
            """
            Возвращает список целевых ширин для n строк.
            rectangle — все одинаковые.
            hexagon   — линейно шире к центру.
            oval      — эллиптический профиль: r = min + (1-min)*sqrt(1 - u^2),
                        где u∈[0..1] — «удалённость от центра» (0 в центре, 1 на краю).
            """
            if n <= 0:
                return []

            if shape not in ("oval", "hexagon"):
                return [float(base_w)] * n

            if n == 1:
                return [float(base_w)]

            widths: list[float] = []
            half = (n - 1) / 2.0

            for i in range(n):
                # u: 0 в центре, 1 на краю (верх/низ)
                u = abs(i - half) / half if half > 0 else 0.0

                if shape == "hexagon":
                    # линейный переход (шестиугольный силуэт)
                    ratio = 1.0 - (1.0 - min_ratio) * u
                else:
                    # эллипс: скруглённые края
                    ratio = min_ratio + (1.0 - min_ratio) * (1.0 - (u * u)) ** 0.5

                widths.append(max(1.0, float(base_w) * float(ratio)))

            return widths

        # Подготовим исходный текст для layout один раз
        text_for_layout = self._soft_hyphenate_overlong(text or "", fm, float(width))
        text_for_layout = text_for_layout.replace("\n", "\u2028")

        # Итеративная раскладка: сначала прямоугольная, далее уточняем форму по факту количества строк
        final_layout: QTextLayout | None = None
        final_lines: list["QTextLine"] = []
        final_content_h: float = 0.0

        max_passes = 3
        prev_target_widths: list[float] | None = None

        for pass_idx in range(max_passes):
            layout = QTextLayout(text_for_layout, qfont)
            layout.setTextOption(opt)
            layout.setCacheEnabled(True)

            layout.beginLayout()
            lines_pass: list["QTextLine"] = []
            y_top = 0.0
            content_h = 0.0

            # Если это не первый проход и есть вычисленные целевые ширины — используем их.
            # Иначе — все строки равны base width.
            # Сами целевые ширины зависят от кол-ва строк, которого мы ещё не знаем,
            # поэтому на первом проходе используем rectangle, далее адаптируем.
            target_widths = prev_target_widths

            li = 0
            while True:
                ln = layout.createLine()
                if not ln.isValid():
                    break

                # Важно: ширина строки должна быть установлена до позиционирования,
                # т.к. от неё зависит перенос слов в этой строке.
                if target_widths is None or li >= len(target_widths):
                    w_line = float(max(1, width))
                else:
                    w_line = float(max(1, int(target_widths[li])))

                ln.setLineWidth(w_line)

                if DEBUG and text_shape != "rectangle":
                    try:
                        print(f"[pass {pass_idx}] line {li}: target_width={w_line:.1f}")
                    except Exception:
                        pass

                # Отключаем встроенный leading и позиционируем вручную
                ln.setLeadingIncluded(False)
                ln.setPosition(QPointF(0.0, y_top))
                lines_pass.append(ln)

                # Высота ядра строки
                core_h_line = ln.ascent() + ln.descent()

                # Пользовательский зазор
                extra_gap = (core_h_line * (line_spacing_percent / 100.0)) if line_spacing_percent else 0.0
                gap = max(0.0, float(line_spacing_px) + float(extra_gap))

                content_h = max(content_h, y_top + core_h_line)
                y_top += gap
                li += 1

            layout.endLayout()

            # Подготовим widths для следующего прохода (если нужно)
            n_lines = len(lines_pass)
            if DEBUG:
                print(f"[pass {pass_idx}] lines={n_lines} content_h={content_h:.2f}")

            if text_shape in ("oval", "hexagon"):
                computed = _compute_line_widths(n_lines, width, text_shape, min_ratio=0.5)
            else:
                computed = [float(width)] * n_lines

            # Критерий останова: если это rectangle, либо ширины стабилизировались
            if text_shape == "rectangle" or (prev_target_widths == computed):
                final_layout = layout
                final_lines = lines_pass
                final_content_h = content_h
                break

            # Иначе — запомним и повторим, чтобы учесть возможное увеличение числа строк
            prev_target_widths = computed

            # Храним последний вариант как fallback на случай выхода по max_passes
            final_layout = layout
            final_lines = lines_pass
            final_content_h = content_h

        # Используем результаты финального прохода
        layout = final_layout
        lines = final_lines
        content_h = final_content_h

        if DEBUG:
            print(f"lines={len(lines)}  content_h={content_h:.2f}")
            if lines:
                first = lines[0]
                print(f"first_line: y={first.y():.2f}  height={first.height():.2f}  ascent={first.ascent():.2f}  descent={first.descent():.2f}  naturalTextWidth={first.naturalTextWidth():.2f}")

        # Паддинги сверху/снизу/слева/справа, чтобы эффекты не обрезались
        # Для glow учитываем radius + softness с коэффициентом запаса
        # При дилатации (MaxFilter 3x3 за glow_radius итераций) + размытие (GaussianBlur)
        # эффект расширяется примерно на: radius * 1.5 + softness * 2
        glow_expand = int((glow_radius * 1.5 + glow_softness * 2.5)) if (glow_color and glow_radius > 0) else 0
        vp_effect = max(glow_expand, stroke_width // 2)
        hp_effect = max(glow_expand, stroke_width // 2)

        if shadow_offset:
            dx, dy = shadow_offset
            vp_effect = max(vp_effect, abs(dy))
            hp_effect = max(hp_effect, abs(dx))

        pad_top = pad_bot = int(max(0, extra_vpadding) + vp_effect)
        pad_left = pad_right = int(hp_effect)

        total_w = max(1, int(width) + pad_left + pad_right)

        if DEBUG:
            print(f"pad_top={pad_top} pad_bot={pad_bot} pad_left={pad_left} pad_right={pad_right}  total_w={total_w} (h TBD)")

        # Подсчёт смещения по X для выравнивания (с учетом левого паддинга)
        def line_x_offset(ln) -> float:
            ntw = ln.naturalTextWidth()
            if align == "center":
                return pad_left + max(0.0, (width - ntw) * 0.5)
            elif align == "right":
                return pad_left + max(0.0, width - ntw)
            return float(pad_left)  # left/justify

        # Собираем общий контур
        full_path = QPainterPath()
        block_top = pad_top
        block_bottom = pad_top + content_h


        total_runs = 0
        total_glyphs = 0
        empty_paths = 0

        for li, ln in enumerate(lines):
            x0 = line_x_offset(ln)
            y_baseline = ln.y() + ln.ascent()
            if DEBUG:
                print(f"[line {li}] x0={x0:.2f}  y_baseline={y_baseline:.2f}")

            for ri, run in enumerate(ln.glyphRuns()):
                total_runs += 1
                raw = run.rawFont()
                if not raw.isValid():
                    if DEBUG: print(f"  [run {ri}] rawFont invalid -> skip")
                    continue

                # Синхронизируем пиксельный размер raw с макетом
                if raw.pixelSize() <= 0:
                    try:
                        raw.setPixelSize(float(max(1, font_px)))
                    except Exception:
                        pass

                units_per_em = raw.unitsPerEm() or 1000.0
                pixel_size = raw.pixelSize() or float(max(1, font_px))

                glyph_indexes = run.glyphIndexes()
                positions = run.positions()

                # --- Авто-детект координатной системы pathForGlyph ---
                test_bh = None
                for g_test in glyph_indexes:
                    p_test = raw.pathForGlyph(int(g_test))
                    if not p_test.isEmpty():
                        br_test = p_test.boundingRect()
                        test_bh = br_test.height()
                        break

                # Heвристика:
                # - если высота контура >> pixel_size → это design-units (нужно масштабировать и флипать Y)
                # - иначе считаем, что контур уже в пикселях с Y-вниз (масштаб 1:1 и без флипа)
                if test_bh is None:
                    coord_mode = "unknown->assume_design_units"
                    scale_x = pixel_size / units_per_em
                    scale_y = -scale_x
                    y_from_pos = lambda py: (pad_top + y_baseline - py)  # после флипа pos.y() вычитаем
                else:
                    if test_bh > pixel_size * 2.5:
                        coord_mode = "design-units"
                        scale_x = pixel_size / units_per_em
                        scale_y = -scale_x          # флип Y (y-вверх -> y-вниз)
                        y_from_pos = lambda py: (pad_top + y_baseline - py)
                    else:
                        coord_mode = "pixels"
                        scale_x = 1.0
                        scale_y = 1.0               # без флипа (уже y-вниз)
                        y_from_pos = lambda py: (pad_top + y_baseline + py)

                if DEBUG:
                    print(f"  [run {ri}] glyphs={len(glyph_indexes)} unitsPerEm={units_per_em} "
                        f"pixelSize={pixel_size} test_bh={test_bh} coord_mode={coord_mode} "
                        f"scale=({scale_x:.6f}, {scale_y:.6f})")

                scale_xform = QTransform()
                scale_xform.scale(scale_x, scale_y)

                for g, pos in zip(glyph_indexes, positions):
                    total_glyphs += 1
                    path = raw.pathForGlyph(int(g))
                    if path.isEmpty():
                        empty_paths += 1
                        continue
                    tpath = scale_xform.map(path)
                    moved = QPainterPath(tpath)
                    moved.translate(x0 + pos.x(), y_from_pos(pos.y()))
                    full_path.addPath(moved)

        if DEBUG:
            print(f"total_runs={total_runs}  total_glyphs={total_glyphs}  empty_paths={empty_paths}")
            br = full_path.boundingRect()
            print(f"full_path.isEmpty()={full_path.isEmpty()}  bounds="
                f"{(br.x(), br.y(), br.width(), br.height())}")
            out_left   = br.right() < 0
            out_top    = br.bottom() < 0
            out_right  = br.left() > total_w
            out_bottom = br.bottom() > block_bottom + pad_bot   # <<< FIX bottom check
            print(f"out_of_bounds: left={out_left} top={out_top} right={out_right} bottom={out_bottom}")

        # Если нечего рисовать — фолбэк
        if full_path.isEmpty():
            if DEBUG: print("full_path empty -> fallback to _render_rich_text_image")
            return self._render_rich_text_image(
                text=text,
                width_px=width,
                font_family=font_family,
                font_px=font_px,
                color=color,
                align=align if align in ("left", "center", "right") else "left",
                line_spacing_px=line_spacing_px,
            )

        # --- Дополнительное отражение по запросу пользователя ---
        if reflect in ("x", "y"):
            br = full_path.boundingRect()
            cx = br.x() + br.width() * 0.5
            cy = br.y() + br.height() * 0.5
            t = QTransform()
            # Перенос в центр → масштаб с флипом → обратно
            t.translate(cx, cy)
            if reflect == "x":
                # отражение по оси X (вертикальный флип)
                t.scale(1.0, -1.0)
            elif reflect == "y":
                # отражение по оси Y (горизонтальный флип)
                t.scale(-1.0, 1.0)
            t.translate(-cx, -cy)
            full_path = t.map(full_path)
            if DEBUG:
                br2 = full_path.boundingRect()
                print(f"reflect={reflect!r}  old_bounds={(br.x(), br.y(), br.width(), br.height())} "
                    f"new_bounds={(br2.x(), br2.y(), br2.width(), br2.height())}")
                
        br = full_path.boundingRect()
        dy = pad_top - br.top()
        if abs(dy) > 0.01:
            full_path.translate(0, dy)
            br = full_path.boundingRect()
        grad_left, grad_right = br.left(), br.right()
        grad_top,  grad_bottom = br.top(),  br.bottom()
        grad_w  = max(1.0, grad_right - grad_left)
        grad_h  = max(1.0, grad_bottom - grad_top)
        grad_cx = (grad_left + grad_right) * 0.5
        grad_cy = (grad_top  + grad_bottom) * 0.5

        grad_bounds = {
            "left": grad_left,
            "right": grad_right,
            "top": grad_top,
            "bottom": grad_bottom,
            "width": grad_w,
            "height": grad_h,
            "cx": grad_cx,
            "cy": grad_cy,
        }

        # 2) Высота картинки по факту контура + нижний паддинг
        total_h = int(max(1, round(max(br.bottom() + pad_bot, block_bottom + pad_bot))))
        if DEBUG:
            print(f"normalized bounds top={br.top():.2f} bottom={br.bottom():.2f}  -> total_h={total_h}")

        effect_sequence = self._normalize_effects_order(effects_order)

        # Теперь можно создавать картинку
        img = QImage(total_w, total_h, QImage.Format.Format_ARGB32_Premultiplied)
        img.fill(0)
        # Рендер контуров + эффектов
        p = QPainter(img)
        p.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        p.setRenderHint(QPainter.RenderHint.TextAntialiasing, True)

        for effect in effect_sequence:
            if effect == "shadow":
                self._apply_shadow(p, full_path, shadow_offset, shadow_color, DEBUG)
            elif effect == "glow":
                self._apply_glow(p, full_path, total_w, total_h, glow_color, glow_radius, glow_softness, DEBUG)
            elif effect == "stroke":
                self._apply_stroke(p, full_path, stroke_color, stroke_width, DEBUG)
            elif effect == "fill":
                self._apply_fill(
                    p,
                    full_path,
                    color,
                    gradient,
                    gradient4,
                    gradient_angle_deg,
                    grad_bounds,
                    block_top,
                    block_bottom,
                    total_w,
                    total_h,
                    DEBUG,
                )
            elif effect == "shake":
                p.end()
                img = self._apply_shake(img, shake, DEBUG)
                p = None
                break

        if p is not None and p.isActive():
            p.end()

        if DEBUG:
            print("render done, returning QImage")
        return img


    def _normalize_effects_order(self, effects_order):
        """
        Возвращает итоговую последовательность эффектов с гарантированным наличием fill.
        shake всегда переносится в конец, чтобы не портить координаты последующих эффектов.
        """
        default_order = ["shadow", "glow", "stroke", "fill", "shake"]
        allowed = {"shadow", "glow", "stroke", "fill", "shake"}

        if not effects_order:
            seq = default_order.copy()
        else:
            seq = [str(e).lower() for e in effects_order if str(e).lower() in allowed]
            if not seq:
                seq = default_order.copy()

        # fill должен быть хотя бы один раз
        if "fill" not in seq:
            seq.append("fill")

        # shake рендерит финальное изображение, поэтому его нужно выполнять последним
        if "shake" in seq:
            seq = [e for e in seq if e != "shake"]
            seq.append("shake")

        # убираем дубликаты, оставляя первое появление
        seen = set()
        uniq = []
        for e in seq:
            if e in seen:
                continue
            uniq.append(e)
            seen.add(e)
        return uniq

    def _apply_shadow(self, painter: QPainter, full_path: QPainterPath, shadow_offset, shadow_color, debug: bool):
        if not (shadow_offset and shadow_color):
            return
        dx, dy = shadow_offset
        if dx or dy:
            if debug:
                print(f"draw shadow: dx={dx} dy={dy} color={shadow_color.name()}")
            painter.save()
            painter.translate(dx, dy)
            painter.fillPath(full_path, QBrush(shadow_color))
            painter.restore()

    def _apply_glow(
        self,
        painter: QPainter,
        full_path: QPainterPath,
        total_w: int,
        total_h: int,
        glow_color: QColor | None,
        glow_radius: int,
        glow_softness: int,
        debug: bool,
    ):
        if not (glow_color and glow_radius > 0):
            return
        if debug:
            print(f"draw glow: radius={glow_radius} softness={glow_softness} color={glow_color.name()}")

        temp_img = QImage(total_w, total_h, QImage.Format.Format_ARGB32_Premultiplied)
        temp_img.fill(0)
        temp_p = QPainter(temp_img)
        temp_p.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        temp_p.fillPath(full_path, QBrush(QColor(0, 0, 0, 255)))  # временно чёрным
        temp_p.end()

        pil_img = self._qimage_to_pil_rgba(temp_img)
        alpha = pil_img.getchannel("A")

        dilated = alpha
        if glow_radius > 0:
            mf = ImageFilter.MaxFilter(3)
            for _ in range(glow_radius):
                dilated = dilated.filter(mf)

        outline = ImageChops.subtract(dilated, alpha)

        if glow_softness > 0:
            outline = outline.filter(ImageFilter.GaussianBlur(glow_softness))

        r, g, b = glow_color.red(), glow_color.green(), glow_color.blue()
        glow_layer = Image.new("RGBA", pil_img.size, (r, g, b, 0))
        glow_layer.putalpha(outline)

        glow_qimg = self._pil_to_qimage_rgba(glow_layer)
        painter.drawImage(0, 0, glow_qimg)

    def _apply_stroke(
        self,
        painter: QPainter,
        full_path: QPainterPath,
        stroke_color: QColor | None,
        stroke_width: int,
        debug: bool,
    ):
        if not (stroke_color and stroke_width > 0):
            return
        if debug:
            print(f"draw stroke: width={stroke_width} color={stroke_color.name()}")
        pen = QPen(stroke_color)
        pen.setWidth(stroke_width)
        pen.setJoinStyle(Qt.PenJoinStyle.RoundJoin)
        painter.strokePath(full_path, pen)

    def _apply_fill(
        self,
        painter: QPainter,
        full_path: QPainterPath,
        color: QColor,
        gradient: tuple[QColor, QColor] | None,
        gradient4: dict[str, QColor] | None,
        gradient_angle_deg: float,
        grad_bounds: dict[str, float],
        block_top: float,
        block_bottom: float,
        image_w: int,
        image_h: int,
        debug: bool,
    ):
        grad_left = grad_bounds["left"]
        grad_right = grad_bounds["right"]
        grad_top = grad_bounds["top"]
        grad_bottom = grad_bounds["bottom"]
        grad_w = grad_bounds["width"]
        grad_h = grad_bounds["height"]
        grad_cx = grad_bounds["cx"]
        grad_cy = grad_bounds["cy"]

        if debug:
            print(gradient4, gradient)
        if gradient4:
            if debug:
                print("Включен градиент-4")
            c_tl = gradient4["tl"]
            c_tr = gradient4["tr"]
            c_bl = gradient4["bl"]
            c_br = gradient4["br"]

            if debug:
                print(
                    "fill 4-corner gradient:",
                    f"BR={c_br.name()} BL={c_bl.name()} TR={c_tr.name()} TL={c_tl.name()}",
                    f"block_top={block_top} block_bottom={block_bottom}",
                )

            h_span = grad_w
            v_span = grad_h

            def lerp(a: float, b: float, t: float) -> float:
                return a + (b - a) * t

            def lerp_color(c1: QColor, c2: QColor, t: float) -> QColor:
                return QColor(
                    int(lerp(c1.red(), c2.red(), t)),
                    int(lerp(c1.green(), c2.green(), t)),
                    int(lerp(c1.blue(), c2.blue(), t)),
                    int(lerp(c1.alpha(), c2.alpha(), t)),
                )

            grad_img = QImage(int(image_w), int(image_h), QImage.Format.Format_ARGB32_Premultiplied)
            grad_img.fill(0)
            ptr = grad_img.bits()
            ptr.setsize(grad_img.sizeInBytes())
            row_stride = grad_img.bytesPerLine()

            def clamp01(t: float) -> float:
                return 0.0 if t < 0.0 else (1.0 if t > 1.0 else t)

            for y in range(int(grad_top), int(grad_bottom)):
                ty = clamp01((y - grad_top) / v_span)

                c_left = lerp_color(c_tl, c_bl, ty)
                c_right = lerp_color(c_tr, c_br, ty)

                scanline_bytes = bytearray(image_w * 4)
                for x in range(image_w):
                    tx = clamp01((x - grad_left) / h_span)
                    c = lerp_color(c_left, c_right, tx)
                    rgba = int(c.rgba()).to_bytes(4, "little")
                    off = x * 4
                    scanline_bytes[off:off+4] = rgba

                row_start = y * row_stride
                memoryview(ptr)[row_start : row_start + image_w * 4] = scanline_bytes

            painter.save()
            painter.setClipPath(full_path)
            painter.drawImage(0, 0, grad_img)
            painter.restore()

        elif gradient:
            if debug:
                print("Включен градиент")
            c1, c2 = gradient

            cx, cy = grad_cx, grad_cy
            w, h = grad_w, grad_h

            theta = math.radians(gradient_angle_deg % 360.0)
            dx, dy = math.cos(theta), math.sin(theta)

            L = 0.5 * (abs(dx) * w + abs(dy) * h) + 1.0

            x0, y0 = cx - dx * L, cy - dy * L
            x1, y1 = cx + dx * L, cy + dy * L

            if debug:
                print(
                    f"fill 2-color gradient: {c1.name()} -> {c2.name()}  "
                    f"angle={gradient_angle_deg:.1f}°  "
                    f"line=({x0:.1f},{y0:.1f})→({x1:.1f},{y1:.1f})"
                )

            grad = QLinearGradient(x0, y0, x1, y1)
            grad.setColorAt(0.0, c1)
            grad.setColorAt(1.0, c2)
            painter.fillPath(full_path, QBrush(grad))
        else:
            if debug:
                print(f"fill solid: {color.name()} a={color.alpha()}")
            painter.fillPath(full_path, QBrush(color))

    def _apply_shake(self, img: QImage, shake: dict[str, float] | None, debug: bool) -> QImage:
        if not shake:
            return img

        angle_deg = float(shake.get("angle_deg", 90) or 90.0)
        up_amt = float(shake.get("up", 0) or 0.0)
        down_amt = float(shake.get("down", 0) or 0.0)
        steps = int(shake.get("steps", 10) or 10)
        base_fade = float(shake.get("base_fade", 0.30) or 0.30)
        decay = float(shake.get("decay", 0.15) or 0.15)
        blur_r = int(shake.get("blur", 0) or 0)
        autogrow = bool(shake.get("autogrow", True))
        grow_margin = int(shake.get("grow_margin", 0) or 0)

        base_fade = max(0.0, min(1.0, base_fade))
        decay = max(0.0, min(1.0, decay))
        steps = max(0, steps)

        theta = math.radians(angle_deg % 360.0)
        ux, uy = math.cos(theta), math.sin(theta)

        if steps > 0 and (up_amt > 0 or down_amt > 0):
            src = img.copy()

            offsets: list[tuple[int, int]] = []

            def add_series(sign: int, amount: float):
                if amount <= 0:
                    return
                for i in range(1, steps + 1):
                    t = i / steps
                    dx = int(round(sign * ux * (amount * t)))
                    dy = int(round(sign * uy * (amount * t)))
                    offsets.append((dx, dy))

            add_series(+1, down_amt)
            add_series(-1, up_amt)

            if offsets:
                min_dx = min(dx for dx, _ in offsets)
                max_dx = max(dx for dx, _ in offsets)
                min_dy = min(dy for _, dy in offsets)
                max_dy = max(dy for _, dy in offsets)
            else:
                min_dx = max_dx = min_dy = max_dy = 0

            blur_pad = int(math.ceil(blur_r * 3)) if blur_r > 0 else 0
            extra_pad = blur_pad + max(0, grow_margin)

            left_pad = max(0, -min_dx + extra_pad) if autogrow else 0
            right_pad = max(0, max_dx + extra_pad) if autogrow else 0
            top_pad = max(0, -min_dy + extra_pad) if autogrow else 0
            bottom_pad = max(0, max_dy + extra_pad) if autogrow else 0

            if any((left_pad, right_pad, top_pad, bottom_pad)):
                new_w = img.width() + left_pad + right_pad
                new_h = img.height() + top_pad + bottom_pad
                base = QImage(new_w, new_h, QImage.Format.Format_ARGB32_Premultiplied)
                base.fill(0)
                bp = QPainter(base)
                bp.setRenderHint(QPainter.RenderHint.Antialiasing, True)
                bp.setRenderHint(QPainter.RenderHint.TextAntialiasing, True)
                bp.drawImage(left_pad, top_pad, img)
                bp.end()
                img = base
            else:
                left_pad = top_pad = 0

            trail = QImage(img.width(), img.height(), QImage.Format.Format_ARGB32_Premultiplied)
            trail.fill(0)
            tp = QPainter(trail)
            tp.setRenderHint(QPainter.RenderHint.Antialiasing, True)
            tp.setRenderHint(QPainter.RenderHint.TextAntialiasing, True)

            op1 = 1.0 - base_fade
            step_factor = (1.0 - decay)

            if down_amt > 0:
                for i in range(1, steps + 1):
                    t = i / steps
                    dx = int(round(ux * (down_amt * t)))
                    dy = int(round(uy * (down_amt * t)))
                    alpha = max(0.0, min(1.0, op1 * (step_factor ** (i - 1))))
                    tp.setOpacity(alpha)
                    tp.drawImage(left_pad + dx, top_pad + dy, src)

            if up_amt > 0:
                for i in range(1, steps + 1):
                    t = i / steps
                    dx = int(round(-ux * (up_amt * t)))
                    dy = int(round(-uy * (up_amt * t)))
                    alpha = max(0.0, min(1.0, op1 * (step_factor ** (i - 1))))
                    tp.setOpacity(alpha)
                    tp.drawImage(left_pad + dx, top_pad + dy, src)

            tp.end()

            if blur_r > 0:
                pil = self._qimage_to_pil_rgba(trail)
                pil = pil.filter(ImageFilter.GaussianBlur(blur_r))
                trail = self._pil_to_qimage_rgba(pil)

            p2 = QPainter(img)
            p2.setRenderHint(QPainter.RenderHint.Antialiasing, True)
            p2.setRenderHint(QPainter.RenderHint.TextAntialiasing, True)
            p2.setOpacity(1.0)
            p2.drawImage(0, 0, trail)
            p2.end()

        if debug:
            print("shake applied")
        return img

    def _qimage_to_pil_rgba(self, img: QImage) -> Image.Image:
        """
        Быстрая конверсия QImage -> PIL RGBA без PNG-кодирования.
        Приводим к ARGB32 (не premultiplied), затем читаем как BGRA.
        """
        qimg = img.convertToFormat(QImage.Format.Format_ARGB32)
        w = qimg.width()
        h = qimg.height()
        ptr = qimg.bits()
        ptr.setsize(qimg.sizeInBytes())
        data = bytes(ptr)
        return Image.frombuffer("RGBA", (w, h), data, "raw", "BGRA", 0, 1).copy()

    def _pil_to_qimage_rgba(self, pil_img: Image.Image) -> QImage:
        """
        Быстрая конверсия PIL RGBA -> QImage ARGB32 без PNG-кодирования.
        """
        if pil_img.mode != "RGBA":
            pil_img = pil_img.convert("RGBA")
        w, h = pil_img.size
        data = pil_img.tobytes("raw", "BGRA")
        qimg = QImage(data, w, h, w * 4, QImage.Format.Format_ARGB32)
        return qimg.copy()

    def _render_rich_text_image(
        self,
        text: str,
        width_px: int,
        font_family: str,
        font_px: int,
        color: QColor,
        align: str = "left",
        line_spacing_px: int = 0,
    ) -> QImage:
        """
        Рендер через QTextDocument с CSS:
        - точная ширина (wrap)
        - цвет
        - выравнивание
        - line-height (кегль + межстрочный пикселями)
        """


        doc = QTextDocument()
        f = QFont(font_family, pointSize=font_px)
        doc.setDefaultFont(f)

        # line-height: через CSS задаём абсолютное значение
        lh_px = max(1, int(font_px + max(0, line_spacing_px)))
        text_align_css = {"left": "left", "center": "center", "right": "right"}.get(align, "left")
        css = (
            "body {"
            f" color: rgba({color.red()},{color.green()},{color.blue()},{color.alpha()});"
            f" text-align: {text_align_css};"
            f" line-height: {lh_px}px;"
            "}"
        )
        doc.setDefaultStyleSheet(css)
        html = "<body>" + text.replace("\n", "<br>") + "</body>"
        doc.setHtml(html)
        doc.setTextWidth(width_px)

        size = doc.size()
        w = max(1, int(size.width()))
        h = max(1, int(size.height()))
        img = QImage(w, h, QImage.Format.Format_ARGB32_Premultiplied)
        img.fill(0)
        p = QPainter(img); doc.drawContents(p); p.end()
        return img
    
    def _insert_soft_hyphens(self, s: str) -> str:
        """
        Вставляет U+00AD в длинные слова по словарным точкам переноса,
        используя ваш smart_hyphenate. Если модуль не вернул точек — слово остаётся как есть.
        """
        def hyph_word(w: str) -> str:
            # пропускаем уже «склеенные»/техн. строки
            if len(w) < 6 or any(ch.isdigit() for ch in w):
                return w
            # не трогаем URL/почту/слова с дефисом уже внутри
            if "://" in w or "@" in w or "-" in w:
                return w
            try:
                # ожидаем, что smart_hyphenate(word) вернёт строку с дефисами в точках переноса.
                # Если у вас нужен другой вызов (например, smart_hyphenate(word, all=True)),
                # поменяйте эту строку соответствующим образом.
                hw = smart_hyphenate(w)
                if isinstance(hw, str) and "-" in hw:
                    return hw.replace("-", SOFT_HYPHEN)
            except Exception:
                pass
            return w

        # Заменяем только «слова» (буквенные последовательности); пробелы/знаки препинания сохраняем
        return re.sub(r"\w{6,}", lambda m: hyph_word(m.group(0)), s)
    

    SOFT_HYPHEN = "\u00AD"
    def _soft_hyphenate_overlong(self, s: str, fm: QFontMetricsF, max_w_px: float) -> str:
        """
        Ставит U+00AD только в словах, чья ширина в пикселях больше max_w_px.
        Слова считаем как \w-последовательности (рус/латин), цифры/URL/существующие дефисы не трогаем.
        """
        word_re = re.compile(r"\w{4,}", flags=re.UNICODE)

        def repl(m: re.Match) -> str:
            w = m.group(0)
            # не трогаем URL/почту/слова с дефисами
            if "://" in w or "@" in w or "-" in w:
                return w
            try:
                if fm.horizontalAdvance(w) > max_w_px:
                    # отметить все законные точки переноса мягким переносом
                    return smart_hyphenate(w, all_positions=True, hyphen_char=SOFT_HYPHEN)
            except Exception:
                pass
            return w

        return word_re.sub(repl, s)
