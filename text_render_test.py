# -*- coding: utf-8 -*-
# mini-gui for _render_text_image_fx_layout debugging
# python -m pip install PyQt6
import sys

from PyQt6.QtCore import Qt
from PyQt6.QtGui import (
    QImage, QColor, QFontDatabase,
    QPainter, QPen, QAction, QPixmap
)
from PyQt6.QtWidgets import (
    QApplication, QWidget, QLabel, QTextEdit, QPushButton, QSpinBox,
    QDoubleSpinBox, QCheckBox, QComboBox, QGridLayout, QHBoxLayout, QVBoxLayout,
    QFileDialog, QColorDialog, QScrollArea, QSizePolicy, QDoubleSpinBox
)

# ---------------------------------------------------------------------------
# Small helper: a button that shows current color and opens QColorDialog
# ---------------------------------------------------------------------------
class ColorButton(QPushButton):
    def __init__(self, title="Pick", color=QColor("white"), allow_none=False, parent=None):
        super().__init__(title, parent)
        self._color = QColor(color)
        self._allow_none = allow_none
        self.clicked.connect(self.choose)
        self.setMinimumWidth(80)
        self._update_style()

        if allow_none:
            self.setContextMenuPolicy(Qt.ContextMenuPolicy.ActionsContextMenu)
            act_clear = QAction("None", self)
            act_clear.triggered.connect(self.clear_color)
            self.addAction(act_clear)

    def color(self):
        return self._color

    def setColor(self, c: QColor | None):
        self._color = QColor(c) if c is not None else None
        self._update_style()

    def clear_color(self):
        if self._allow_none:
            self._color = None
            self._update_style()

    def choose(self):
        start = self._color if isinstance(self._color, QColor) else QColor("white")
        c = QColorDialog.getColor(start, self, "Choose color", QColorDialog.ColorDialogOption.ShowAlphaChannel)
        if c.isValid():
            self._color = c
            self._update_style()

    def _update_style(self):
        if isinstance(self._color, QColor):
            col = self._color
            self.setText(col.name() + f"  a={col.alpha()}")
            self.setStyleSheet(f"ColorButton {{ background: {col.name()}; }}")
        else:
            self.setText("None")
            self.setStyleSheet("ColorButton { background: #444; color: white; }")


# ---------------------------------------------------------------------------
# Renderer host: вставьте сюда СВОЮ функцию _render_text_image_fx_layout
# ---------------------------------------------------------------------------
from ui_new.tabs.text_tab.text_render import Renderer
    
# ---------------------------------------------------------------------------
# Main GUI
# ---------------------------------------------------------------------------
class MainWidget(QWidget):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("Text FX Renderer (PyQt6)")
        self.renderer = Renderer()

        # ==== Controls ====
        self.txt_input = QTextEdit()
        self.txt_input.setPlainText("Привет, мир!\nЭто тест длинной строки для переносов и эффектов.")

        # font family
        self.cb_font = QComboBox()
        self._init_fonts_from_dir("fonts")

        # font size
        self.sb_font_px = QSpinBox()
        self.sb_font_px.setRange(1, 512)
        self.sb_font_px.setValue(48)

        # width
        self.sb_width = QSpinBox()
        self.sb_width.setRange(50, 4000)
        self.sb_width.setValue(800)

        # line spacing
        self.sb_line_spacing = QSpinBox()
        self.sb_line_spacing.setRange(0, 200)
        self.sb_line_spacing.setValue(6)

        # line spacing %
        self.sb_line_spacing_percent = QSpinBox()
        self.sb_line_spacing_percent.setRange(0, 200)
        self.sb_line_spacing_percent.setValue(50)

        # align
        self.cb_align = QComboBox()
        self.cb_align.addItems(["left", "center", "right", "justify"])

        # extra_vpadding
        self.sb_vpad = QSpinBox()
        self.sb_vpad.setRange(0, 200)
        self.sb_vpad.setValue(2)

        # reflect
        self.cb_reflect = QComboBox()
        self.cb_reflect.addItems(["None", "x", "y"])

        # base color
        self.btn_color = ColorButton("color", QColor(255, 255, 255, 255))

        # stroke
        self.ck_stroke = QCheckBox("stroke")
        self.btn_stroke_color = ColorButton("stroke color", QColor(0, 0, 0, 255), allow_none=True)
        self.sb_stroke_w = QSpinBox()
        self.sb_stroke_w.setRange(0, 64)
        self.sb_stroke_w.setValue(0)

        # glow
        self.ck_glow = QCheckBox("glow")
        self.btn_glow_color = ColorButton("glow color", QColor(0, 255, 255, 180), allow_none=True)
        self.sb_glow_r = QSpinBox()
        self.sb_glow_r.setRange(0, 128)
        self.sb_glow_r.setValue(0)
        self.sb_glow_softness = QSpinBox()
        self.sb_glow_softness.setRange(0, 64)
        self.sb_glow_softness.setValue(0)

        # shadow
        self.ck_shadow = QCheckBox("shadow")
        self.btn_shadow_color = ColorButton("shadow color", QColor(0, 0, 0, 160), allow_none=True)
        self.sb_shadow_dx = QSpinBox(); self.sb_shadow_dx.setRange(-200, 200); self.sb_shadow_dx.setValue(0)
        self.sb_shadow_dy = QSpinBox(); self.sb_shadow_dy.setRange(-200, 200); self.sb_shadow_dy.setValue(0)

        # gradient
        self.ck_gradient = QCheckBox("gradient")
        self.btn_grad_c1 = ColorButton("c1", QColor("#ffcc00"), allow_none=False)
        self.btn_grad_c2 = ColorButton("c2", QColor("#ff0066"), allow_none=False)

        self.sb_grad_angle = QDoubleSpinBox()
        self.sb_grad_angle.setRange(0.0, 360.0)
        self.sb_grad_angle.setDecimals(1)
        self.sb_grad_angle.setSingleStep(5.0)
        self.sb_grad_angle.setValue(90.0)

        # --- NEW: 4-point gradient ---
        self.ck_gradient4 = QCheckBox("gradient4")
        self.btn_grad4_tl = ColorButton("TL", QColor("#ffcc00"), allow_none=False)
        self.btn_grad4_tr = ColorButton("TR", QColor("#ff0066"), allow_none=False)
        self.btn_grad4_bl = ColorButton("BL", QColor("#00ccff"), allow_none=False)
        self.btn_grad4_br = ColorButton("BR", QColor("#66ff66"), allow_none=False)

        # DEBUG
        self.ck_debug = QCheckBox("DEBUG prints")
        self.ck_debug.setChecked(True)

        # Render button
        self.btn_render = QPushButton("Render")
        self.btn_render.clicked.connect(self.on_render)

        # Save button
        self.btn_save = QPushButton("Save PNG…")
        self.btn_save.clicked.connect(self.on_save)

        # ==== Layout for controls ====
        grid = QGridLayout()
        r = 0
        grid.addWidget(QLabel("Text:"), r, 0, 1, 1); grid.addWidget(self.txt_input, r, 1, 1, 5); r += 1

        grid.addWidget(QLabel("Font family:"), r, 0); grid.addWidget(self.cb_font, r, 1)
        grid.addWidget(QLabel("Font px:"), r, 2); grid.addWidget(self.sb_font_px, r, 3)
        grid.addWidget(QLabel("Width px:"), r, 4); grid.addWidget(self.sb_width, r, 5); r += 1

        grid.addWidget(QLabel("Line spacing px:"), r, 0); grid.addWidget(self.sb_line_spacing, r, 1)
        grid.addWidget(QLabel("Align:"), r, 2); grid.addWidget(self.cb_align, r, 3)
        grid.addWidget(QLabel("Extra vpadding:"), r, 4); grid.addWidget(self.sb_vpad, r, 5); r += 1

        grid.addWidget(QLabel("Line spacing %:"), r, 0); grid.addWidget(self.sb_line_spacing_percent, r, 1); r += 1

        grid.addWidget(QLabel("Reflect:"), r, 0); grid.addWidget(self.cb_reflect, r, 1)
        grid.addWidget(QLabel("Fill color:"), r, 2); grid.addWidget(self.btn_color, r, 3)
        grid.addWidget(self.ck_debug, r, 4); r += 1


        # stroke row
        grid.addWidget(self.ck_stroke, r, 0)
        grid.addWidget(self.btn_stroke_color, r, 1)
        grid.addWidget(QLabel("width:"), r, 2)
        grid.addWidget(self.sb_stroke_w, r, 3); r += 1

        # glow row
        grid.addWidget(self.ck_glow, r, 0)
        grid.addWidget(self.btn_glow_color, r, 1)
        grid.addWidget(QLabel("radius:"), r, 2)
        grid.addWidget(self.sb_glow_r, r, 3)
        grid.addWidget(QLabel("softness:"), r, 4)
        grid.addWidget(self.sb_glow_softness, r, 5); r += 1

        # shadow row
        grid.addWidget(self.ck_shadow, r, 0)
        grid.addWidget(self.btn_shadow_color, r, 1)
        grid.addWidget(QLabel("dx:"), r, 2); grid.addWidget(self.sb_shadow_dx, r, 3)
        grid.addWidget(QLabel("dy:"), r, 4); grid.addWidget(self.sb_shadow_dy, r, 5); r += 1

        # gradient row
        grid.addWidget(self.ck_gradient, r, 0)
        grid.addWidget(QLabel("gradient c1:"), r, 1); grid.addWidget(self.btn_grad_c1, r, 2)
        grid.addWidget(QLabel("c2:"), r, 3); grid.addWidget(self.btn_grad_c2, r, 4); r += 1
        # --- NEW: gradient angle row ---
        grid.addWidget(QLabel("Grad angle (deg):"), r, 1)
        grid.addWidget(self.sb_grad_angle, r, 2)
        r += 1

        # --- NEW: 4-point gradient row ---
        grid.addWidget(self.ck_gradient4, r, 0)
        grid.addWidget(QLabel("TL:"), r, 1); grid.addWidget(self.btn_grad4_tl, r, 2)
        grid.addWidget(QLabel("TR:"), r, 3); grid.addWidget(self.btn_grad4_tr, r, 4); r += 1

        grid.addWidget(QLabel("BL:"), r, 1); grid.addWidget(self.btn_grad4_bl, r, 2)
        grid.addWidget(QLabel("BR:"), r, 3); grid.addWidget(self.btn_grad4_br, r, 4); r += 1

        # ==== Preview ====
        self.lbl = PreviewLabel()
        self.lbl.setMinimumSize(200, 200)
        self.ck_show_border = QCheckBox("Показывать границы изображения")
        self.ck_show_border.setChecked(True)
        self.ck_show_border.toggled.connect(self.lbl.setShowBorder)

        # buttons
        hb = QHBoxLayout()
        hb.addWidget(self.btn_render)
        hb.addWidget(self.btn_save)
        hb.addWidget(self.ck_show_border)
        hb.addStretch(1)

        left = QVBoxLayout()
        left.addLayout(grid)
        left.addLayout(hb)



        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setWidget(self.lbl)

        root = QHBoxLayout(self)
        root.addLayout(left, 0)
        root.addWidget(scroll, 1)

        # initial render
        self.on_render()

    def _init_fonts_from_dir(self, dir_path: str):
        """Загружает шрифты только из указанной папки и заполняет комбобокс
        отображаемыми именами = названиями файлов (без расширения).
        Поддерживаются .ttf/.otf/.ttc. Для .ttc берем первую family."""
        import os
        from PyQt6.QtGui import QFontDatabase

        self.font_display_to_family: dict[str, str] = {}
        self.cb_font.clear()

        if not os.path.isdir(dir_path):
            # Папки нет — оставим пусто (рендерер все равно сможет упасть на дефолт)
            return

        # Собираем список файлов шрифтов
        exts = {".ttf", ".otf", ".ttc"}
        font_files = []
        for root, _, files in os.walk(dir_path):
            for f in files:
                if os.path.splitext(f.lower())[1] in exts:
                    font_files.append(os.path.join(root, f))

        # Отсортируем для стабильности
        font_files.sort()

        # Грузим в приложение и наполняем комбобокс
        for path in font_files:
            font_id = QFontDatabase.addApplicationFont(path)
            if font_id == -1:
                continue  # не загрузился
            families = QFontDatabase.applicationFontFamilies(font_id)
            if not families:
                continue
            family = families[0]  # берем первую; для TTC их может быть несколько

            display = os.path.splitext(os.path.basename(path))[0]  # имя файла без расширения

            # если такое отображаемое имя уже есть, делаем уникальным
            base = display
            n = 2
            while display in self.font_display_to_family:
                display = f"{base} ({n})"
                n += 1

            self.font_display_to_family[display] = family
            self.cb_font.addItem(display)

        # если ничего не загрузилось — комбобокс останется пустым


    # ---------- helpers to gather params ----------
    def _get_color_or_none(self, btn: ColorButton, enabled_flag: bool, allow_none=True):
        if not enabled_flag:
            return None
        col = btn.color()
        if isinstance(col, QColor):
            return col
        return None if allow_none else QColor("white")

    def on_render(self):
        global DEBUG
        DEBUG = bool(self.ck_debug.isChecked())
        self.renderer.DEBUG = DEBUG

        text = self.txt_input.toPlainText()
        display_name = self.cb_font.currentText()
        font_family = self.font_display_to_family.get(display_name, display_name)
        font_px = self.sb_font_px.value()
        width = self.sb_width.value()
        line_spacing_px = self.sb_line_spacing.value()
        line_spacing_percent = self.sb_line_spacing_percent.value()
        align = self.cb_align.currentText()
        extra_vpadding = self.sb_vpad.value()

        color = self.btn_color.color() or QColor("white")

        stroke_color = self._get_color_or_none(self.btn_stroke_color, self.ck_stroke.isChecked())
        stroke_width = self.sb_stroke_w.value()

        glow_color = self._get_color_or_none(self.btn_glow_color, self.ck_glow.isChecked())
        glow_radius = self.sb_glow_r.value()
        glow_softness = self.sb_glow_softness.value()

        shadow_color = self._get_color_or_none(self.btn_shadow_color, self.ck_shadow.isChecked())
        shadow_offset = None
        if self.ck_shadow.isChecked():
            shadow_offset = (self.sb_shadow_dx.value(), self.sb_shadow_dy.value())

        gradient = None
        if self.ck_gradient.isChecked():
            c1 = self.btn_grad_c1.color()
            c2 = self.btn_grad_c2.color()
            if isinstance(c1, QColor) and isinstance(c2, QColor):
                gradient = (c1, c2)
        # --- gradient angle ---
        gradient_angle_deg = float(self.sb_grad_angle.value())

        # --- gradient4 ---
        gradient4 = None
        if self.ck_gradient4.isChecked():
            tl = self.btn_grad4_tl.color()
            tr = self.btn_grad4_tr.color()
            bl = self.btn_grad4_bl.color()
            br = self.btn_grad4_br.color()
            if all(isinstance(c, QColor) for c in (tl, tr, bl, br)):
                gradient4 = {"tl": tl, "tr": tr, "bl": bl, "br": br}
            gradient = None
        reflect = self.cb_reflect.currentText()
        reflect = None if reflect == "None" else reflect

        # call user's renderer
        img: QImage = self.renderer._render_text_image_fx_layout(
            text=text,
            font_family=font_family,
            font_px=font_px,
            color=color,
            width=width,
            line_spacing_px=line_spacing_px,
            line_spacing_percent=line_spacing_percent,
            align=align,
            stroke_color=stroke_color,
            stroke_width=stroke_width,
            glow_color=glow_color,
            glow_radius=glow_radius,
            glow_softness=glow_softness,
            shadow_offset=shadow_offset,
            shadow_color=shadow_color,
            gradient=gradient,
            gradient_angle_deg=gradient_angle_deg,   # NEW
            gradient4=gradient4,
            extra_vpadding=extra_vpadding,
            reflect=reflect,
        )

        if not isinstance(img, QImage) or img.isNull():
            self.lbl.setText("Rendering failed (null image).")
            return

        from PyQt6.QtGui import QPixmap
        self.lbl.setPixmap(QPixmap.fromImage(img))
        self.lbl.resize(img.size())

    def on_save(self):
        pm = self.lbl.pixmap()
        if not pm:
            return
        fn, _ = QFileDialog.getSaveFileName(self, "Save PNG", "render.png", "PNG Images (*.png)")
        if fn:
            pm.toImage().save(fn, "PNG")

class PreviewLabel(QLabel):
    """Лейбл, который рисует пиксмап и рамку границ QImage (без изменения рендера)."""
    def __init__(self, parent=None):
        super().__init__(parent)
        self.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.setBackgroundRole(self.backgroundRole())
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
        self._show_border = True
        self._pm = None
        self._checker = True  # сделаем шахматный фон, удобно видеть прозрачность

    def setShowBorder(self, on: bool):
        self._show_border = bool(on)
        self.update()

    def setPixmap(self, pm: "QPixmap | None"):
        self._pm = pm
        super().setPixmap(pm)
        self.update()

    def paintEvent(self, ev):
        # базовая заливка
        p = QPainter(self)
        p.fillRect(self.rect(), self.palette().brush(self.backgroundRole()))

        # шахматный фон для прозрачности (необязательно, но удобно)
        if self._checker:
            tile = 10
            c1 = QColor(220, 220, 220)
            c2 = QColor(190, 190, 190)
            for y in range(0, self.height(), tile):
                for x in range(0, self.width(), tile):
                    p.fillRect(x, y, tile, tile, c1 if ((x//tile + y//tile) % 2 == 0) else c2)

        if not self._pm or self._pm.isNull():
            # нет картинки — просто пишем текст
            p.setPen(Qt.GlobalColor.black)
            p.drawText(self.rect(), Qt.AlignmentFlag.AlignCenter, "No image")
            p.end()
            return

        # рисуем саму картинку по центру без масштабирования
        img_w, img_h = self._pm.width(), self._pm.height()
        x = (self.width() - img_w) // 2
        y = (self.height() - img_h) // 2
        p.drawPixmap(x, y, self._pm)

        # рамка по границе QImage
        if self._show_border:
            pen = QPen(QColor(255, 0, 0))
            pen.setWidth(1)
            p.setPen(pen)
            p.setBrush(Qt.BrushStyle.NoBrush)
            p.drawRect(x, y, img_w - 1, img_h - 1)  # -1 чтобы рамка была внутри пикселей

        p.end()


def main():
    app = QApplication(sys.argv)
    w = MainWidget()
    w.resize(1200, 700)
    w.show()
    sys.exit(app.exec())

if __name__ == "__main__":
    main()