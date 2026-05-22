# ui_new/tabs/text_tab/text_panel.py
from __future__ import annotations
from typing import Callable
from PyQt6.QtCore import Qt
from PyQt6.QtGui import QColor, QFont, QPixmap, QImage, QFontDatabase
from PyQt6.QtWidgets import (
    QWidget, QVBoxLayout, QGroupBox, QFormLayout, QSpinBox, QColorDialog, QPushButton, QHBoxLayout,
    QToolButton, QButtonGroup, QLabel, QDoubleSpinBox, QCheckBox, QRadioButton, QSizePolicy, QComboBox, QScrollArea, QFrame, QTextEdit,
    
)
from .text_style import StyleBinding, TextStyle

class TextRibbonPanelQt(QWidget):
    """
    Контейнер панели: слева фиксированное превью, справа — горизонтально прокручиваемая лента секций.
    Вызывает внешний render_fn(width_px)->QImage для перерисовки превью.
    """
    def __init__(self, *, columns: list[list[QWidget]], render_fn: Callable[[int], "QImage"], parent=None,
                 preview_size=(260, 180), actions_widget: QWidget | None = None):
        super().__init__(parent)
        self._render_fn = render_fn
        self._actions_widget = actions_widget

        root = QVBoxLayout(self)
        root.setContentsMargins(8, 8, 8, 4)
        root.setSpacing(6)

        # Ряд: превью + скроллируемая лента
        row = QWidget(self)
        row_lay = QHBoxLayout(row)
        row_lay.setContentsMargins(0, 0, 0, 0)
        row_lay.setSpacing(12)

        # Превью
        self.preview_label = QLabel(row)
        self.preview_label.setObjectName("TextPreview")
        self.preview_label.setFixedSize(*preview_size)
        self.preview_label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.preview_label.setStyleSheet(
            "background: transparent; border: 1px solid rgba(0,0,0,40); border-radius: 8px;"
        )

        # Собираем ленту по колонкам
        ribbon = QWidget(row)
        ribbon_lay = QHBoxLayout(ribbon)
        ribbon_lay.setContentsMargins(0, 0, 0, 0)
        ribbon_lay.setSpacing(12)

        for col_widgets in columns:
            col = QWidget(ribbon)
            col_lay = QVBoxLayout(col)
            col_lay.setContentsMargins(0, 0, 0, 0)
            col_lay.setSpacing(8)
            for w in col_widgets:
                if w is None: 
                    continue
                w.setSizePolicy(QSizePolicy.Policy.Maximum, QSizePolicy.Policy.Preferred)
                col_lay.addWidget(w)
            ribbon_lay.addWidget(col)

        # Горизонтальный скролл, без вертикального
        ribbon_scroll = QScrollArea(row)
        ribbon_scroll.setWidgetResizable(True)
        ribbon_scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAsNeeded)
        ribbon_scroll.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        ribbon_scroll.setFrameShape(QFrame.Shape.NoFrame)
        ribbon_scroll.setWidget(ribbon)
        h = self.preview_label.sizeHint().height()
        ribbon_scroll.setFixedHeight(h)
        ribbon.setMinimumHeight(h)

        # Сборка
        row_lay.addWidget(self.preview_label, 0)
        row_lay.addWidget(ribbon_scroll, 1)
        if actions_widget:
            actions_widget.setSizePolicy(QSizePolicy.Policy.Maximum, QSizePolicy.Policy.Preferred)
            row_lay.addWidget(actions_widget, 0)
        root.addWidget(row, 0)

    def update_preview(self):
        if not callable(self._render_fn):
            return
        # Фиксированная ширина для рендера текста (чтобы текст помещался в 2 строки)
        render_width = 240
        qimg = self._render_fn(int(render_width))
        if qimg and not qimg.isNull():
            pm = QPixmap.fromImage(qimg)
            # Масштабируем превью для отображения, сохраняя пропорции
            # Вычисляем целевой размер, чтобы картинка вписалась в фиксированную область панели
            max_preview_width = 260
            max_preview_height = 180

            # Вычисляем масштаб для вписывания в доступное пространство
            scale_w = max_preview_width / pm.width() if pm.width() > max_preview_width else 1.0
            scale_h = max_preview_height / pm.height() if pm.height() > max_preview_height else 1.0
            scale = min(scale_w, scale_h)

            # Применяем масштаб
            target_w = int(pm.width() * scale)
            target_h = int(pm.height() * scale)

            # Устанавливаем размер метки под картинку
            self.preview_label.setFixedSize(max_preview_width, max_preview_height)

            pm_scaled = pm.scaled(target_w, target_h, Qt.AspectRatioMode.KeepAspectRatio,
                                  Qt.TransformationMode.SmoothTransformation)
            self.preview_label.setPixmap(pm_scaled)


class CreationTextPanelQt(TextRibbonPanelQt):
    """
    Панель СОЗДАНИЯ текста: общие блоки + 'Действия' (в отдельной колонке справа).
    """
    def __init__(self, *, font_panel: TextFontPanelQt, render_fn, parent=None):
        g = font_panel.get_groups()
        columns = [
            [g["text"]],
            [g["color"], g["align"]],
            [g["more"]],
            [g["stroke"]],
            [g["glow"]],
            [g["shake"]],
            [g["shadow"]],
            [g["grad_enable"], g["grad_kind_box"]],
            [g["grad2"]],
            [g["grad4"]],
        ]
        super().__init__(columns=columns, render_fn=render_fn, parent=parent, actions_widget=g["actions"])


class EditTextPanelQt(TextRibbonPanelQt):
    """
    Панель РЕДАКТИРОВАНИЯ текста: собственная независимая панель с параметрами оверлея.
    Появляется при выделении текстового оверлея. Не делит виджеты с панелью создания.
    """
    def __init__(self, *,
                 overlay_meta: dict,  # данные TextOverlayMeta в виде словаря
                 style_binding: StyleBinding,
                 on_text_changed: Callable[[str], None],
                 on_width_changed: Callable[[float], None],
                 on_scale_changed: Callable[[float], None],
                 on_angle_changed: Callable[[float], None],
                 on_apply: Callable[[], None],
                 on_delete: Callable[[], None],
                 eyedropper_starter: Callable,
                 render_fn,
                 parent=None):

        # Создаём собственный независимый TextFontPanelQt для редактирования
        self.edit_font_panel = TextFontPanelQt(
            binding=style_binding,
            custom_font_files=overlay_meta.get("custom_font_files"),
            font_file_map=overlay_meta.get("font_file_map"),
            custom_font_families=overlay_meta.get("custom_font_families"),
            on_export=None,  # не нужен в режиме редактирования
            on_toggle_clean_overlays=None,
            get_clean_overlays_visible=None,
            parent=parent,
            assemble_layout=False  # не собираем layout, берём только группы
        )
        self.edit_font_panel.set_eyedropper_starter(eyedropper_starter)

        g = self.edit_font_panel.get_groups()

        # === Блок редактирования содержимого ===
        grp_edit = QGroupBox("Текст", parent)
        v_edit = QVBoxLayout(grp_edit)
        v_edit.setSpacing(6)

        # Большое текстовое поле с переносом строк
        self.edit_text = QTextEdit(grp_edit)
        self.edit_text.setPlaceholderText("Введите текст…")
        self.edit_text.setPlainText(overlay_meta.get("text", ""))
        self.edit_text.setMinimumHeight(100)
        self.edit_text.setMaximumHeight(150)
        self.edit_text.setMinimumWidth(300)  # Увеличено в 2 раза (со 150 до 300)
        self.edit_text.textChanged.connect(lambda: on_text_changed(self.edit_text.toPlainText()))

        v_edit.addWidget(self.edit_text)

        # === Блок параметров ===
        grp_params = QGroupBox("Параметры", parent)
        form_params = QFormLayout(grp_params)

        # Ширина оверлея в пикселях
        self.width_spin = QSpinBox(grp_params)
        self.width_spin.setRange(10, 5000)
        self.width_spin.setSingleStep(10)
        self.width_spin.setValue(int(overlay_meta.get("width_px", 300)))
        self.width_spin.valueChanged.connect(on_width_changed)

        form_params.addRow("Ширина (px):", self.width_spin)

        # === Блок позиционирования ===
        grp_pos = QGroupBox("Позиционирование", parent)
        form_pos = QFormLayout(grp_pos)

        self.scale_spin = QDoubleSpinBox(grp_pos)
        self.scale_spin.setRange(0.1, 10.0)
        self.scale_spin.setSingleStep(0.1)
        self.scale_spin.setDecimals(2)
        self.scale_spin.setValue(overlay_meta.get("user_scale", 1.0))
        self.scale_spin.valueChanged.connect(on_scale_changed)

        self.angle_spin = QDoubleSpinBox(grp_pos)
        self.angle_spin.setRange(-360.0, 360.0)
        self.angle_spin.setSingleStep(1.0)
        self.angle_spin.setDecimals(1)
        self.angle_spin.setValue(overlay_meta.get("angle", 0.0))
        self.angle_spin.valueChanged.connect(on_angle_changed)

        form_pos.addRow("Масштаб:", self.scale_spin)
        form_pos.addRow("Угол (°):", self.angle_spin)

        # === Блок действий ===
        grp_actions = QGroupBox("Действия", parent)
        v_actions = QVBoxLayout(grp_actions)
        v_actions.setSpacing(6)

        self.btn_apply = QPushButton("Применить", grp_actions)
        self.btn_apply.clicked.connect(on_apply)

        self.btn_center_width = QPushButton("Центрировать по ширине", grp_actions)
        self.btn_center_width.setEnabled(False)  # TODO: реализовать

        self.btn_center_bubble = QPushButton("Центрировать по пузырю", grp_actions)
        self.btn_center_bubble.setEnabled(False)  # TODO: реализовать

        self.btn_delete = QPushButton("Удалить", grp_actions)
        self.btn_delete.clicked.connect(on_delete)

        v_actions.addWidget(self.btn_apply)
        v_actions.addWidget(self.btn_center_width)
        v_actions.addWidget(self.btn_center_bubble)
        v_actions.addWidget(self.btn_delete)

        # Общие блоки без 'actions' и 'save'
        columns = [
            [grp_edit],
            [grp_params, grp_pos],
            [g["text"]],
            [g["color"], g["align"]],
            [g["more"]],
            [g["stroke"]],
            [g["glow"]],
            [g["shake"]],
            [g["shadow"]],
            [g["grad_enable"], g["grad_kind_box"]],
            [g["grad2"]],
            [g["grad4"]],
        ]
        super().__init__(columns=columns, render_fn=render_fn, parent=parent, actions_widget=grp_actions)

        # Сохраняем ссылки на коллбэки для возможности переподключения
        self._on_text_changed = on_text_changed
        self._on_width_changed = on_width_changed
        self._on_scale_changed = on_scale_changed
        self._on_angle_changed = on_angle_changed
        self._on_apply = on_apply
        self._on_delete = on_delete

    def update_for_overlay(self, overlay_meta: dict, on_text_changed, on_width_changed,
                           on_scale_changed, on_angle_changed, on_apply, on_delete, render_fn):
        """Обновить панель для нового оверлея без пересоздания виджетов"""
        # Обновляем значения виджетов
        self.edit_text.blockSignals(True)
        self.edit_text.setPlainText(overlay_meta.get("text", ""))
        self.edit_text.blockSignals(False)

        self.width_spin.blockSignals(True)
        self.width_spin.setValue(int(overlay_meta.get("width_px", 300)))
        self.width_spin.blockSignals(False)

        self.scale_spin.blockSignals(True)
        self.scale_spin.setValue(overlay_meta.get("user_scale", 1.0))
        self.scale_spin.blockSignals(False)

        self.angle_spin.blockSignals(True)
        self.angle_spin.setValue(overlay_meta.get("angle", 0.0))
        self.angle_spin.blockSignals(False)

        # Переподключаем сигналы к новым коллбэкам
        try:
            self.edit_text.textChanged.disconnect()
            self.width_spin.valueChanged.disconnect()
            self.scale_spin.valueChanged.disconnect()
            self.angle_spin.valueChanged.disconnect()
            self.btn_apply.clicked.disconnect()
            self.btn_delete.clicked.disconnect()
        except:
            pass  # Игнорируем, если сигналы уже отключены

        self.edit_text.textChanged.connect(lambda: on_text_changed(self.edit_text.toPlainText()))
        self.width_spin.valueChanged.connect(on_width_changed)
        self.scale_spin.valueChanged.connect(on_scale_changed)
        self.angle_spin.valueChanged.connect(on_angle_changed)
        self.btn_apply.clicked.connect(on_apply)
        self.btn_delete.clicked.connect(on_delete)

        # Обновляем функцию рендера
        self._render_fn = render_fn

        # Обновляем состояние font_panel
        self.edit_font_panel.apply_state(overlay_meta)

        # Сохраняем ссылки
        self._on_text_changed = on_text_changed
        self._on_width_changed = on_width_changed
        self._on_scale_changed = on_scale_changed
        self._on_angle_changed = on_angle_changed
        self._on_apply = on_apply
        self._on_delete = on_delete

class ColorLine(QWidget):
    """Универсальная строка выбора цвета: превью, 'Выбрать…', 'Пипетка'."""
    def __init__(self, *, initial_rgba=(0,0,0,255),
                 on_color_qcolor: Callable[[QColor], None],
                 on_start_eyedropper: Callable[[Callable[[QColor],None], Callable[[QColor],None], Callable[[],None]], None],
                 label_text: str = "Цвет:",
                 parent: QWidget | None = None):
        super().__init__(parent)
        self._on_color = on_color_qcolor
        self._start_eyedropper = on_start_eyedropper

        h = QHBoxLayout(self); h.setContentsMargins(0,0,0,0); h.setSpacing(6)
        self.preview = QLabel(); self.preview.setFixedSize(40, 20)
        self._set_preview_rgba(initial_rgba)
        self._last_qcolor = QColor(*initial_rgba)
        btn_pick = QPushButton("Выбрать…")
        btn_eye  = QPushButton("Пипетка")

        def pick():
            qc = QColorDialog.getColor(
                self._last_qcolor, self, label_text,
                QColorDialog.ColorDialogOption.ShowAlphaChannel
            )
            if qc.isValid():
                self._set_preview_rgba((qc.red(), qc.green(), qc.blue(), qc.alpha()))
                self._last_qcolor = qc
                self._on_color(qc)
        def eyedrop():
            # on_preview — временно обновляем превью; on_commit — ставим окончательно
            self._start_eyedropper(
                lambda q: (
                    setattr(self, "_last_qcolor", q),
                    self._set_preview_rgba((q.red(), q.green(), q.blue(), q.alpha()))
                ),
                lambda q: (
                    setattr(self, "_last_qcolor", q),
                    self._set_preview_rgba((q.red(), q.green(), q.blue(), q.alpha())),
                    self._on_color(q)
                ),
                lambda: None
            )
        btn_pick.clicked.connect(pick)
        btn_eye.clicked.connect(eyedrop)
        h.addWidget(self.preview)
        h.addWidget(btn_pick)
        h.addWidget(btn_eye)
    
    def set_rgba(self, rgba, *, emit: bool = True):
        """Обновить превью и внутреннее состояние; при emit=True вызвать on_color_qcolor()."""
        r, g, b, a = rgba
        qc = QColor(r, g, b, a)
        self._last_qcolor = qc
        self._set_preview_rgba(rgba)
        if emit and self._on_color:
            self._on_color(qc)

    def _set_preview_rgba(self, rgba):
        r,g,b,a = rgba
        self.preview.setStyleSheet(
            f"background-color: rgba({r},{g},{b},{a}); border: 1px solid #000;"
        )

class TextFontPanelQt(QWidget):
    """
    Простая правая панель настройки текста для вкладки PyQt6.
    Управляет активным редактором и дефолтами для новых блоков.
    """
    def __init__(self,
                *,
                binding: StyleBinding,
                on_export=None,
                on_toggle_clean_overlays=None,
                get_clean_overlays_visible=None,
                custom_font_files=None,
                font_file_map=None,
                custom_font_families=None,
                parent=None,
                assemble_layout: bool = True):
        super().__init__(parent)
        self.setObjectName("TextFontPanelQt")
        self.setMinimumWidth(260)
        self._assemble = bool(assemble_layout)
        if not self._assemble:
            # делаем контейнер полностью «невидимым»
            try:
                from PyQt6.QtCore import Qt as _Qt
                self.setAttribute(_Qt.WidgetAttribute.WA_DontShowOnScreen, True)
            except Exception:
                pass
            self.hide()
            self.setFixedSize(0, 0)

        self._binding = binding

        st = binding.current().to_dict() if binding else {}
        init_family = st.get("font_family", "Arial")
        init_size   = int(st.get("font_size", 24))
        # поддерживаем как новое имя font_color_rgba, так и старое color_rgba из метаданных оверлея
        init_color  = st.get("font_color_rgba") or st.get("color_rgba") or (0,0,0,255)
        init_ls     = int(st.get("line_spacing", 4))
        init_align  = st.get("align", "left")
        init_shake_enabled = bool(st.get("shake_enabled", False))
        init_shake_angle = float(st.get("shake_angle_deg", 90.0))
        init_shake_up = int(st.get("shake_up", 0))
        init_shake_down = int(st.get("shake_down", 40))
        init_shake_steps = int(st.get("shake_steps", 12))
        init_shake_base_fade = float(st.get("shake_base_fade", 0.30))
        init_shake_decay = float(st.get("shake_decay", 0.15))
        init_shake_blur = int(st.get("shake_blur", 2))
        # список доступных шрифтов может приезжать из вьюхи (кастомные) или из состояния
        file_items = custom_font_files if custom_font_files is not None else st.get("custom_font_files", [])
        file2family = font_file_map if font_file_map is not None else st.get("font_file_map", {})
        file_items = list(file_items or [])
        file2family = dict(file2family or {})
        self._custom_font_families = custom_font_families if custom_font_families is not None else st.get("custom_font_families", [])

        def _emit(patch: dict):
            if self._binding:
                self._binding.emit(**patch)

        self._cb_linesp_pct = lambda v: _emit({"line_spacing_percent": int(v)})
        self._cb_vpad = lambda v: _emit({"extra_vpadding": int(v)})
        self._cb_reflect = lambda mode: _emit({"reflect": mode})
        self._cb_stroke = lambda w, qc: _emit({"stroke_width": int(w), "stroke_color_rgba": (qc.red(), qc.green(), qc.blue(), qc.alpha()) if qc else None})
        self._cb_glow = lambda r, s, qc: _emit({"glow_radius": int(r), "glow_softness": int(s), "glow_color_rgba": (qc.red(), qc.green(), qc.blue(), qc.alpha()) if qc else None})
        self._cb_shadow = lambda dx, dy, qc: _emit({"shadow_dx": int(dx), "shadow_dy": int(dy), "shadow_color_rgba": (qc.red(), qc.green(), qc.blue(), qc.alpha()) if qc else None})
        self._cb_grad2 = lambda c1_rgba, c2_rgba, ang: _emit({"grad2_c1_rgba": c1_rgba, "grad2_c2_rgba": c2_rgba, "grad_angle_deg": float(ang), "grad4_tl_rgba": None, "grad4_tr_rgba": None, "grad4_bl_rgba": None, "grad4_br_rgba": None})
        self._cb_grad4 = lambda tl, tr, bl, br: _emit({"grad4_tl_rgba": tl, "grad4_tr_rgba": tr, "grad4_bl_rgba": bl, "grad4_br_rgba": br, "grad2_c1_rgba": None, "grad2_c2_rgba": None})
        self._cb_shape = lambda shape: _emit({"text_shape": shape})
        def _emit_shake_patch(shake: dict | None):
            if not self._binding:
                return
            if not shake:
                self._binding.emit(shake_enabled=False)
                return
            patch = {
                "shake_enabled": True,
                "shake_angle_deg": float(shake.get("angle_deg", 90.0)),
                "shake_up": int(shake.get("up", 0)),
                "shake_down": int(shake.get("down", 0)),
                "shake_steps": int(shake.get("steps", 0)),
                "shake_base_fade": float(shake.get("base_fade", 0.30)),
                "shake_decay": float(shake.get("decay", 0.15)),
                "shake_blur": int(shake.get("blur", 0)),
            }
            self._binding.emit(**patch)
        self._cb_shake = _emit_shake_patch
        lay = None
        if self._assemble:
            lay = QVBoxLayout(self)
            lay.setContentsMargins(10, 10, 10, 10)
            lay.setSpacing(12)

        # ---- Группа: Текст ----
        grp_text = QGroupBox("Текст", self)
        lay_text = QFormLayout(grp_text)
        lay_text.setLabelAlignment(Qt.AlignmentFlag.AlignLeft)

        # Семейство шрифта
        self.family = QComboBox(grp_text)
        self.family.addItems(file_items)
        self._file_items = list(file_items)
        self._file2family = dict(file2family)
        # применяем шрифт к каждой строке выпадающего списка
        for i, base in enumerate(file_items):
            fam = file2family.get(base)
            if fam:
                self.family.setItemData(i, QFont(fam), Qt.ItemDataRole.FontRole)

        # выбрать стартовый (если приходил init_family как QFont family — подберём по мапе)
        init_family = st.get("font_family", "Arial")
        start_file = next((b for b, fam in file2family.items() if fam == init_family), None)
        if start_file in file_items:
            self.family.setCurrentText(start_file)
        elif file_items:
            self.family.setCurrentIndex(0)

        def _on_family_changed(basename: str):
            fam = file2family.get(basename, basename)
            if self._binding:
                self._binding.emit(font_family=fam)

        self.family.currentTextChanged.connect(_on_family_changed)
        lay_text.addRow("Шрифт:", self.family)  # <-- ОСТАВИТЬ ОДИН РАЗ

        # Чекбокс "Системные шрифты"
        self.use_system_fonts = QCheckBox("Системные шрифты", grp_text)
        # Загружаем сохранённое состояние из конфига
        try:
            from config import UserConfig
            self.use_system_fonts.setChecked(bool(UserConfig.TextTab.use_system_fonts))
        except Exception:
            self.use_system_fonts.setChecked(False)

        self.use_system_fonts.toggled.connect(self._on_system_fonts_toggled)
        lay_text.addRow("", self.use_system_fonts)
        # Применяем стартовое состояние чекбокса (добавит системные шрифты при включённом флаге)
        self._on_system_fonts_toggled(self.use_system_fonts.isChecked())

        # Размер
        self.size = QSpinBox(grp_text)
        self.size.setRange(8, 200)
        self.size.setSingleStep(1)
        self.size.setValue(init_size)
        self.size.valueChanged.connect(lambda v: self._binding.emit(font_size=int(v)) if self._binding else None)
        lay_text.addRow("Размер:", self.size)

        # Межстрочный интервал (px)
        self.linesp = QSpinBox(grp_text)
        self.linesp.setRange(0, 120)
        self.linesp.setSingleStep(1)
        self.linesp.setValue(init_ls)
        self.linesp.valueChanged.connect(lambda v: self._binding.emit(line_spacing=int(v)) if self._binding else None)
        lay_text.addRow("Межстрочный:", self.linesp)

        # Выпадающий список "Форма"
        self.shape_combo = QComboBox(grp_text)
        self.shape_combo.addItem("[  ]", userData="rectangle")
        self.shape_combo.addItem("(  )", userData="oval")
        self.shape_combo.addItem("<  >", userData="hexagon")
        self.shape_combo.setCurrentIndex(0)  # По умолчанию "rectangle"
        self.shape_combo.currentIndexChanged.connect(
            lambda _: self._cb_shape(self.shape_combo.currentData() or "rectangle")
        )
        lay_text.addRow("Форма:", self.shape_combo)

        if lay: lay.addWidget(grp_text)

        # ---- Группа: Цвет ----
        grp_color = QGroupBox("Цвет", self)
        grp_color.setObjectName("grp_color_main")
        v_color = QVBoxLayout(grp_color)
        self.color_line = ColorLine(
            initial_rgba=init_color,
            on_color_qcolor=lambda qc: self._binding.emit(font_color_rgba=(qc.red(), qc.green(), qc.blue(), qc.alpha())) if self._binding else None,
            on_start_eyedropper=lambda on_prev, on_commit, on_cancel:
                self._start_eyedropper_bridge(on_prev, on_commit, on_cancel)
        )
        v_color.addWidget(self.color_line)
        if lay: lay.addWidget(grp_color)

        # ---- Группа: Выравнивание ----
        grp_align = QGroupBox("Выравнивание", self)
        grp_align.setObjectName("grp_align") 
        h = QHBoxLayout(grp_align)
        self.btnL = QToolButton(grp_align); self.btnL.setText("⟸")
        self.btnC = QToolButton(grp_align); self.btnC.setText("⇔")
        self.btnR = QToolButton(grp_align); self.btnR.setText("⟹")
        self.btnL.setCheckable(True); self.btnC.setCheckable(True); self.btnR.setCheckable(True)
        self.grp = QButtonGroup(self)
        self.grp.addButton(self.btnL); self.grp.addButton(self.btnC); self.grp.addButton(self.btnR)
        self.grp.setExclusive(True)
        m = {"left": self.btnL, "center": self.btnC, "right": self.btnR}
        (m.get(init_align) or self.btnL).setChecked(True)
        self.btnL.clicked.connect(lambda: self._binding.emit(align="left") if self._binding else None)
        self.btnC.clicked.connect(lambda: self._binding.emit(align="center") if self._binding else None)
        self.btnR.clicked.connect(lambda: self._binding.emit(align="right") if self._binding else None)
        h.addWidget(self.btnL); h.addWidget(self.btnC); h.addWidget(self.btnR)
        if lay: lay.addWidget(grp_align)

        # ---- Группа: Интерлиньяж / Паддинг / Отражение ----
        grp_more = QGroupBox("Верстка/геометрия", self)
        lay_more = QFormLayout(grp_more)

        self.linesp_pct = QSpinBox(grp_more); self.linesp_pct.setRange(0, 200); self.linesp_pct.setValue(35)
        self.vpad = QSpinBox(grp_more); self.vpad.setRange(0, 200); self.vpad.setValue(2)
        self.reflect = QComboBox(grp_more); self.reflect.addItems(["None","x","y"])

        self.linesp_pct.valueChanged.connect(lambda v: self._cb_linesp_pct(int(v)))
        self.vpad.valueChanged.connect(lambda v: self._cb_vpad(int(v)))
        self.reflect.currentTextChanged.connect(
            lambda t: self._cb_reflect(None if t == "None" else t)
        )
        lay_more.addRow("Межстрочный %:", self.linesp_pct)
        lay_more.addRow("Верт. паддинг:", self.vpad)
        lay_more.addRow("Отражение:", self.reflect)
        if lay: lay.addWidget(grp_more)

        # ---- Группа: Stroke ----
        grp_stroke = QGroupBox("Обводка", self)
        v_stroke = QVBoxLayout(grp_stroke)   # вертикальная компоновка для всего блока

        # --- верхняя строка: вкл + толщина
        h_stroke_top = QHBoxLayout()
        self.stroke_enable = QCheckBox("Вкл", grp_stroke)
        self.stroke_enable.setChecked(False)
        self.stroke_w = QSpinBox(grp_stroke)
        self.stroke_w.setRange(0, 64)
        self.stroke_w.setValue(0)
        h_stroke_top.addWidget(self.stroke_enable)
        h_stroke_top.addWidget(QLabel("Толщина:"))
        h_stroke_top.addWidget(self.stroke_w)

        # --- вторая строка: цвет
        h_stroke_color = QHBoxLayout()
        self.stroke_color = ColorLine(
            initial_rgba=init_color,
            on_color_qcolor=lambda qc: self._cb_stroke(self.stroke_w.value(), qc),
            on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c)
        )
        h_stroke_color.addWidget(QLabel("Цвет:"))
        h_stroke_color.addWidget(self.stroke_color)

        # --- третья строка: кнопка обмена цветов
        h_stroke_swap = QHBoxLayout()
        self.stroke_swap_btn = QPushButton("Поменять с основным цветом", grp_stroke)
        self.stroke_swap_btn.clicked.connect(self._swap_stroke_with_main_color)
        h_stroke_swap.addWidget(self.stroke_swap_btn)

        # соединяем сигналы
        self.stroke_w.valueChanged.connect(lambda w: self._cb_stroke(int(w), self._last_qcolor(self.stroke_color)))
        self.stroke_w.valueChanged.connect(lambda w: self._emit_stroke())
        self.stroke_enable.toggled.connect(self._toggle_stroke_ui)

        # складываем в общий layout группы
        v_stroke.addLayout(h_stroke_top)
        v_stroke.addLayout(h_stroke_color)
        v_stroke.addLayout(h_stroke_swap)

        if lay: lay.addWidget(grp_stroke)

        # ---- Группа: Glow ----
        grp_glow = QGroupBox("Свечение", self)
        v_glow = QVBoxLayout(grp_glow)   # вертикальная компоновка

        # --- верхняя строка: вкл + радиус
        h_glow_top = QHBoxLayout()
        self.glow_enable = QCheckBox("Вкл", grp_glow)
        self.glow_enable.setChecked(False)
        self.glow_r = QSpinBox(grp_glow)
        self.glow_r.setRange(0, 128)
        self.glow_r.setValue(0)
        h_glow_top.addWidget(self.glow_enable)
        h_glow_top.addWidget(QLabel("Радиус:"))
        h_glow_top.addWidget(self.glow_r)

        # --- вторая строка: мягкость (softness)
        h_glow_soft = QHBoxLayout()
        self.glow_softness = QSpinBox(grp_glow)
        self.glow_softness.setRange(0, 64)
        self.glow_softness.setValue(5)
        h_glow_soft.addWidget(QLabel("Мягкость:"))
        h_glow_soft.addWidget(self.glow_softness)

        # --- третья строка: цвет
        h_glow_color = QHBoxLayout()
        self.glow_color = ColorLine(
            initial_rgba=(0, 255, 255, 180),
            on_color_qcolor=lambda qc: self._emit_glow(),
            on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c)
        )
        h_glow_color.addWidget(QLabel("Цвет:"))
        h_glow_color.addWidget(self.glow_color)

        # сигналы
        self.glow_r.valueChanged.connect(lambda r: self._emit_glow())
        self.glow_softness.valueChanged.connect(lambda s: self._emit_glow())
        self.glow_enable.toggled.connect(self._toggle_glow_ui)

        # собираем
        v_glow.addLayout(h_glow_top)
        v_glow.addLayout(h_glow_soft)
        v_glow.addLayout(h_glow_color)

        if lay: lay.addWidget(grp_glow)
        # ---- Группа: Shadow ----
        grp_shadow = QGroupBox("Тень", self)
        form_shadow = QFormLayout(grp_shadow)
        self.shadow_enable = QCheckBox("Вкл", grp_shadow); self.shadow_enable.setChecked(False)        
        self.shadow_dx = QSpinBox(); self.shadow_dx.setRange(-200,200)
        self.shadow_dy = QSpinBox(); self.shadow_dy.setRange(-200,200)
        self.shadow_color = ColorLine(initial_rgba=(0,0,0,160), on_color_qcolor=lambda qc: self._cb_shadow(self.shadow_dx.value(), self.shadow_dy.value(), qc),
                                    on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c))
        self.shadow_dx.valueChanged.connect(lambda _: self._emit_shadow())
        self.shadow_dy.valueChanged.connect(lambda _: self._emit_shadow())
        self.shadow_enable.toggled.connect(self._toggle_shadow_ui)
        form_shadow.addRow(self.shadow_enable)
        form_shadow.addRow("dx:", self.shadow_dx); form_shadow.addRow("dy:", self.shadow_dy); form_shadow.addRow("Цвет:", self.shadow_color)
        if lay: lay.addWidget(grp_shadow)

        # ---- Группа: Shake ----
        grp_shake = QGroupBox("Шлейф (shake)", self)
        form_shake = QFormLayout(grp_shake)

        self.shake_enable = QCheckBox("Вкл", grp_shake); self.shake_enable.setChecked(init_shake_enabled)
        self.shake_angle = QDoubleSpinBox(grp_shake); self.shake_angle.setRange(0.0, 360.0); self.shake_angle.setDecimals(1); self.shake_angle.setSingleStep(1.0); self.shake_angle.setValue(init_shake_angle)
        self.shake_up = QSpinBox(grp_shake); self.shake_up.setRange(0, 500); self.shake_up.setValue(init_shake_up)
        self.shake_down = QSpinBox(grp_shake); self.shake_down.setRange(0, 500); self.shake_down.setValue(init_shake_down)
        self.shake_steps = QSpinBox(grp_shake); self.shake_steps.setRange(0, 300); self.shake_steps.setValue(init_shake_steps)
        self.shake_base_fade = QDoubleSpinBox(grp_shake); self.shake_base_fade.setRange(0.0, 1.0); self.shake_base_fade.setDecimals(3); self.shake_base_fade.setSingleStep(0.05); self.shake_base_fade.setValue(init_shake_base_fade)
        self.shake_decay = QDoubleSpinBox(grp_shake); self.shake_decay.setRange(0.0, 1.0); self.shake_decay.setDecimals(3); self.shake_decay.setSingleStep(0.05); self.shake_decay.setValue(init_shake_decay)
        self.shake_blur = QSpinBox(grp_shake); self.shake_blur.setRange(0, 50); self.shake_blur.setValue(init_shake_blur)

        row_up_down = QWidget(grp_shake)
        row_up_down_lay = QHBoxLayout(row_up_down); row_up_down_lay.setContentsMargins(0,0,0,0)
        row_up_down_lay.addWidget(QLabel("Вверх:")); row_up_down_lay.addWidget(self.shake_up)
        row_up_down_lay.addWidget(QLabel("Вниз:")); row_up_down_lay.addWidget(self.shake_down)

        row_fade = QWidget(grp_shake)
        row_fade_lay = QHBoxLayout(row_fade); row_fade_lay.setContentsMargins(0,0,0,0)
        row_fade_lay.addWidget(QLabel("База:")); row_fade_lay.addWidget(self.shake_base_fade)
        row_fade_lay.addWidget(QLabel("Затух.:")); row_fade_lay.addWidget(self.shake_decay)

        form_shake.addRow(self.shake_enable)
        form_shake.addRow("Угол (°):", self.shake_angle)
        form_shake.addRow("Шаги:", self.shake_steps)
        form_shake.addRow("Смещение:", row_up_down)
        form_shake.addRow("Прозрачность:", row_fade)
        form_shake.addRow("Блюр:", self.shake_blur)

        self.shake_enable.toggled.connect(self._toggle_shake_ui)
        self.shake_angle.valueChanged.connect(lambda _: self._emit_shake())
        self.shake_up.valueChanged.connect(lambda _: self._emit_shake())
        self.shake_down.valueChanged.connect(lambda _: self._emit_shake())
        self.shake_steps.valueChanged.connect(lambda _: self._emit_shake())
        self.shake_base_fade.valueChanged.connect(lambda _: self._emit_shake())
        self.shake_decay.valueChanged.connect(lambda _: self._emit_shake())
        self.shake_blur.valueChanged.connect(lambda _: self._emit_shake())

        if lay: lay.addWidget(grp_shake)

        # Группа «Градиент»
        self.grad_enable = QCheckBox("Градиент: Вкл", self); self.grad_enable.setChecked(False)
        if lay: lay.addWidget(self.grad_enable)

        # переключатель варианта градиента
        grad_kind_box = QGroupBox("Режим градиента", self)
        hk = QHBoxLayout(grad_kind_box)
        self.rb_grad2 = QRadioButton("2 цвета", grad_kind_box)
        self.rb_grad4 = QRadioButton("4 угла", grad_kind_box)
        self.rb_grad2.setChecked(True)
        hk.addWidget(self.rb_grad2); hk.addWidget(self.rb_grad4)
        if lay: lay.addWidget(grad_kind_box)

        grp_grad2 = QGroupBox("Градиент (2 цвета)", self)
        form_g2 = QFormLayout(grp_grad2)
        self.grad2_c1 = ColorLine(initial_rgba=(255,204,0,255), on_color_qcolor=lambda _: self._emit_grad2(),
                                on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c))
        self.grad2_c2 = ColorLine(initial_rgba=(255,0,102,255), on_color_qcolor=lambda _: self._emit_grad2(),
                                on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c))
        self.grad_angle = QDoubleSpinBox(); self.grad_angle.setRange(0.0,360.0); self.grad_angle.setDecimals(1); self.grad_angle.setSingleStep(5.0); self.grad_angle.setValue(90.0)
        self.grad_angle.valueChanged.connect(lambda _: self._emit_grad2())
        form_g2.addRow("C1:", self.grad2_c1); form_g2.addRow("C2:", self.grad2_c2); form_g2.addRow("Угол (°):", self.grad_angle)
        if lay: lay.addWidget(grp_grad2)

        # ---- Группа: Градиент 4-угольный ----
        grp_grad4 = QGroupBox("Градиент 4-угольный", self)
        form_g4 = QFormLayout(grp_grad4)
        self.g4_tl = ColorLine(initial_rgba=(255,204,0,255), on_color_qcolor=lambda _: self._emit_grad4(),
                            on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c))
        self.g4_tr = ColorLine(initial_rgba=(255,0,102,255), on_color_qcolor=lambda _: self._emit_grad4(),
                            on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c))
        self.g4_bl = ColorLine(initial_rgba=(0,204,255,255), on_color_qcolor=lambda _: self._emit_grad4(),
                            on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c))
        self.g4_br = ColorLine(initial_rgba=(102,255,102,255), on_color_qcolor=lambda _: self._emit_grad4(),
                            on_start_eyedropper=lambda a,b,c: self._start_eyedropper_bridge(a,b,c))
        form_g4.addRow("TL:", self.g4_tl); form_g4.addRow("TR:", self.g4_tr); form_g4.addRow("BL:", self.g4_bl); form_g4.addRow("BR:", self.g4_br)
        if lay: lay.addWidget(grp_grad4)
        # связи включения/переключения
        self.grad_enable.toggled.connect(self._toggle_gradient_ui)
        self.rb_grad2.toggled.connect(self._toggle_gradient_kind)
        self._toggle_stroke_ui(False)
        self._toggle_glow_ui(False)
        self._toggle_shadow_ui(False)
        self._toggle_shake_ui(self.shake_enable.isChecked())
        self._toggle_gradient_ui(False)
        self._toggle_gradient_kind()  # применить стартовое состояние

        grp_actions = QGroupBox("Действия", self)
        v_actions = QVBoxLayout(grp_actions); v_actions.setSpacing(6)

        btn_recreate = QPushButton("Пересоздать последний текст", grp_actions)

        self.show_clean_overlays_checkbox = QCheckBox("Видимость клина", grp_actions)
        if get_clean_overlays_visible and callable(get_clean_overlays_visible):
            try:
                self.show_clean_overlays_checkbox.setChecked(bool(get_clean_overlays_visible()))
            except Exception:
                self.show_clean_overlays_checkbox.setChecked(True)
        else:
            self.show_clean_overlays_checkbox.setChecked(True)

        # пока нерабочие — просто дизейблим/подписываемся заглушками
        btn_recreate.setEnabled(False)
        if on_toggle_clean_overlays and callable(on_toggle_clean_overlays):
            self.show_clean_overlays_checkbox.toggled.connect(lambda checked: on_toggle_clean_overlays(checked))
        else:
            self.show_clean_overlays_checkbox.setEnabled(False)

        btn_recreate.clicked.connect(lambda: None)

        v_actions.addWidget(btn_recreate)
        v_actions.addWidget(self.show_clean_overlays_checkbox)
        btn_overlay_save = QPushButton("Наложить и сохранить", grp_actions)

        # пока нерабочие — просто дизейблим/подписываемся заглушками
        btn_overlay_save.setEnabled(True)
        if on_export:
            btn_overlay_save.clicked.connect(lambda: on_export())
        else:
            btn_overlay_save.clicked.connect(lambda: None)

        v_actions.addWidget(btn_overlay_save)

        # Сохраняем ссылки на группы, чтобы собирать «ленту» снаружи
        self._groups = {
            "text": grp_text,
            "color": grp_color,
            "align": grp_align,
            "more": grp_more,
            "stroke": grp_stroke,
            "glow": grp_glow,
            "shadow": grp_shadow,
            "shake": grp_shake,
            "grad_enable": self.grad_enable,
            "grad_kind_box": grad_kind_box,
            "grad2": grp_grad2,
            "grad4": grp_grad4,
            "actions": grp_actions,
        }


    def get_groups(self) -> dict[str, QWidget]:
        """Вернёт словарь со всеми секциями/группами для свободной сборки ленты."""
        return dict(self._groups)

    def _last_qcolor(self, colorLineWidget):
        # ColorLine в on_color_qcolor(qc) можно дописать: setattr(widget, "_last_qc", qc)
        return getattr(colorLineWidget, "_last_qcolor", None)

    def _emit_grad2(self):
        if not self.grad_enable.isChecked() or not self.rb_grad2.isChecked():
            # если градиент выключен или выбран не этот режим — обнуляем
            self._cb_grad2(None, None, float(self.grad_angle.value()))
            return
        c1 = self._last_qcolor(self.grad2_c1)
        c2 = self._last_qcolor(self.grad2_c2)
        ang = float(self.grad_angle.value())
        c1_rgba = (c1.red(), c1.green(), c1.blue(), c1.alpha()) if c1 else None
        c2_rgba = (c2.red(), c2.green(), c2.blue(), c2.alpha()) if c2 else None
        self._cb_grad2(c1_rgba, c2_rgba, ang)

    def _emit_grad4(self):
        def as_rgba(qc):
            return (qc.red(), qc.green(), qc.blue(), qc.alpha()) if qc else None
        if not self.grad_enable.isChecked() or not self.rb_grad4.isChecked():
            self._cb_grad4(None, None, None, None)
            return
        tl = as_rgba(self._last_qcolor(self.g4_tl))
        tr = as_rgba(self._last_qcolor(self.g4_tr))
        bl = as_rgba(self._last_qcolor(self.g4_bl))
        br = as_rgba(self._last_qcolor(self.g4_br))
        self._cb_grad4(tl, tr, bl, br)

    def _emit_stroke(self):
        if not self.stroke_enable.isChecked():
            self._cb_stroke(0, None)
            return
        qc = self._last_qcolor(self.stroke_color)
        self._cb_stroke(int(self.stroke_w.value()), qc)

    def _emit_glow(self):
        if not self.glow_enable.isChecked():
            self._cb_glow(0, 0, None)
            return
        qc = self._last_qcolor(self.glow_color)
        self._cb_glow(int(self.glow_r.value()), int(self.glow_softness.value()), qc)

    def _emit_shadow(self):
        if not self.shadow_enable.isChecked():
            self._cb_shadow(0, 0, None)
            return
        qc = self._last_qcolor(self.shadow_color)
        self._cb_shadow(int(self.shadow_dx.value()), int(self.shadow_dy.value()), qc)

    def _emit_shake(self):
        if not self.shake_enable.isChecked():
            self._cb_shake(None)
            return
        shake = {
            "angle_deg": float(self.shake_angle.value()),
            "up": int(self.shake_up.value()),
            "down": int(self.shake_down.value()),
            "steps": int(self.shake_steps.value()),
            "base_fade": float(self.shake_base_fade.value()),
            "decay": float(self.shake_decay.value()),
            "blur": int(self.shake_blur.value()),
        }
        self._cb_shake(shake)

    def _swap_stroke_with_main_color(self):
        """
        Меняет местами цвет обводки и основной цвет текста.
        """
        # Получаем текущие цвета
        main_qc = self._last_qcolor(self.color_line)
        stroke_qc = self._last_qcolor(self.stroke_color)

        if not main_qc or not stroke_qc:
            return

        # Извлекаем RGBA
        main_rgba = (main_qc.red(), main_qc.green(), main_qc.blue(), main_qc.alpha())
        stroke_rgba = (stroke_qc.red(), stroke_qc.green(), stroke_qc.blue(), stroke_qc.alpha())

        # Меняем местами
        self.color_line.set_rgba(stroke_rgba, emit=True)
        self.stroke_color.set_rgba(main_rgba, emit=True)

    # ====== toggle helpers ======
    def _toggle_group_widgets(self, enabled, widgets):
        for w in widgets:
            w.setEnabled(enabled)

    def _toggle_stroke_ui(self, enabled):
        self._toggle_group_widgets(enabled, [self.stroke_w, self.stroke_color, self.stroke_swap_btn])
        self._emit_stroke()  # актуализировать состояние эффекта

    def _toggle_glow_ui(self, enabled):
        self._toggle_group_widgets(enabled, [self.glow_r, self.glow_softness, self.glow_color])
        self._emit_glow()

    def _toggle_shadow_ui(self, enabled):
        self._toggle_group_widgets(enabled, [self.shadow_dx, self.shadow_dy, self.shadow_color])
        self._emit_shadow()

    def _toggle_shake_ui(self, enabled):
        widgets = [
            self.shake_angle, self.shake_up, self.shake_down,
            self.shake_steps, self.shake_base_fade, self.shake_decay,
            self.shake_blur
        ]
        self._toggle_group_widgets(enabled, widgets)
        self._emit_shake()

    def _toggle_gradient_ui(self, enabled):
        # мастер-чекбокс включает/выключает обе подгруппы и радиокнопки
        for w in [self.rb_grad2, self.rb_grad4, self.grad2_c1, self.grad2_c2, self.grad_angle,
                  self.g4_tl, self.g4_tr, self.g4_bl, self.g4_br]:
            w.setEnabled(enabled)
        # при выключении обнуляем оба варианта
        if not enabled:
            self._cb_grad2(None, None, float(self.grad_angle.value()))
            self._cb_grad4(None, None, None, None)
            #print("Не включено _toggle_gradient_ui")
        else:
            # при включении — отправляем активный вариант
            if self.rb_grad2.isChecked():
                self._emit_grad2()
            else:
                self._emit_grad4()

    def _toggle_gradient_kind(self):
        is_g2 = self.rb_grad2.isChecked()
        for w in [self.grad2_c1, self.grad2_c2, self.grad_angle]:
            w.setVisible(is_g2)
        for w in [self.g4_tl, self.g4_tr, self.g4_bl, self.g4_br]:
            w.setVisible(not is_g2)
        # при смене режима — обнуляем неактивный, отправляем активный
        if self.grad_enable.isChecked():
            if is_g2:
                self._cb_grad4(None, None, None, None)
                self._emit_grad2()
            else:
                self._cb_grad2(None, None, float(self.grad_angle.value()))
                self._emit_grad4()
        
    # small helper
    def _apply_preview_rgba(self, rgba: tuple[int,int,int,int]):
        r,g,b,a = rgba
        self.color_preview.setStyleSheet(
            f"background-color: rgba({r},{g},{b},{a}); border: 1px solid #000;"
        )

    # --- хелперы внутри класса панели ---
    def _qcolor_from_colorline(self, cl: ColorLine):
        # ColorLine отдаёт цвет только через on_color_qcolor; упрощённый вариант:
        # забираем из QLabel styleSheet обратно не будем — достаточно хранить последнее в on_* колбэках
        # здесь просто верни None (колбэки уже доставят нужный QColor)
        return None

    def _grad2_values(self):
        c1 = self._last_qcolor(self.grad2_c1); c2 = self._last_qcolor(self.grad2_c2); ang = float(self.grad_angle.value())
        return (self._rgba(c1), self._rgba(c2), ang)

    def _grad4_values(self):
        tl = self._rgba(self._last_qcolor(self.g4_tl)); tr = self._rgba(self._last_qcolor(self.g4_tr))
        bl = self._rgba(self._last_qcolor(self.g4_bl)); br = self._rgba(self._last_qcolor(self.g4_br))
        return (tl, tr, bl, br)

    def _last_qcolor(self, cl: ColorLine):  # сохрани последнюю QColor в lambdas on_color_qcolor
        return getattr(cl, "_last_qcolor", None)

    def _rgba(self, q: QColor | None):
        return (q.red(), q.green(), q.blue(), q.alpha()) if isinstance(q, QColor) else None

    # и ещё в ColorLine.pick/eyedrop — запоминай последний QColor:
    # внутри lambdas on_color_qcolor: setattr(self, "_last_qcolor", qc)

    def set_eyedropper_starter(self, starter_callable):
        """starter_callable(on_preview_qcolor, on_commit_qcolor, on_cancel) -> None"""
        self._eyedropper_starter = starter_callable

    def _start_eyedropper_bridge(self, on_preview, on_commit, on_cancel):
        if hasattr(self, "_eyedropper_starter") and self._eyedropper_starter:
            self._eyedropper_starter(on_preview, on_commit, on_cancel)

    def _on_system_fonts_toggled(self, checked: bool):
        """Обработчик переключения чекбокса системных шрифтов."""
        # Сохраняем состояние в конфиг
        try:
            from config import UserConfig
            UserConfig.TextTab.use_system_fonts = checked
        except Exception:
            pass

        # Сохраняем текущий выбранный шрифт
        current_text = self.family.currentText()
        current_family = self._file2family.get(current_text, current_text)

        # Очищаем комбобокс
        self.family.blockSignals(True)
        self.family.clear()

        # Добавляем кастомные шрифты
        for base in self._file_items:
            fam = self._file2family.get(base)
            idx = self.family.count()
            self.family.addItem(base)
            if fam:
                self.family.setItemData(idx, QFont(fam), Qt.ItemDataRole.FontRole)

        # Если включены системные шрифты, добавляем их
        if checked:
            system_families = QFontDatabase.families()
            for fam in sorted(system_families):
                # Добавляем только если не дублирует кастомные
                if fam not in self._file2family.values():
                    idx = self.family.count()
                    self.family.addItem(fam)
                    self.family.setItemData(idx, QFont(fam), Qt.ItemDataRole.FontRole)
                    # Системные шрифты используют имя как ключ
                    self._file2family[fam] = fam
        else:
            # Отключая системные шрифты — очищаем их из мапы, оставляя только кастомные
            self._file2family = {base: self._file2family.get(base, base) for base in self._file_items}

        # Пытаемся восстановить выбор
        # Сначала ищем по basename (кастомные шрифты)
        if current_text in [self.family.itemText(i) for i in range(self.family.count())]:
            self.family.setCurrentText(current_text)
        # Потом по family name (системные шрифты)
        elif current_family in [self.family.itemText(i) for i in range(self.family.count())]:
            self.family.setCurrentText(current_family)
        elif self.family.count() > 0:
            self.family.setCurrentIndex(0)

        self.family.blockSignals(False)

    def apply_state(self, st: dict):
        """Установить значения контролов без лишнего шума, затем дернуть нужные эмиттеры."""
        if isinstance(st, TextStyle):
            st = st.to_dict()
        fam = st.get("font_family")
        if fam and hasattr(self, "family") and hasattr(self, "_file2family"):
            # найти basename, соответствующий этому семейству
            basename = next((b for b, f in self._file2family.items() if f == fam), None)
            if basename:
                self.family.blockSignals(True)
                self.family.setCurrentText(basename)
                self.family.blockSignals(False)
        # Размер
        if hasattr(self, "size"):
            self.size.blockSignals(True)
            self.size.setValue(int(st.get("font_size", self.size.value())))
            self.size.blockSignals(False)
        # Межстрочный
        if hasattr(self, "linesp"):
            self.linesp.blockSignals(True)
            self.linesp.setValue(int(st.get("line_spacing", self.linesp.value())))
            self.linesp.blockSignals(False)
        # Цвет
        if hasattr(self, "color_line"):
            _clr = st.get("font_color_rgba") or st.get("color_rgba")
            if _clr:
                self.color_line.set_rgba(tuple(_clr), emit=True)

        # Выравнивание
        align = st.get("align")
        if align and hasattr(self, "grp"):
            btn = {"left": self.btnL, "center": self.btnC, "right": self.btnR}.get(align)
            if btn:
                was = btn.isChecked()
                btn.setChecked(True)
                if not was:
                    btn.click()  # вызовет on_align

        # Верстка/геометрия
        if hasattr(self, "linesp_pct"):
            self.linesp_pct.blockSignals(True)
            self.linesp_pct.setValue(int(st.get("line_spacing_percent", self.linesp_pct.value())))
            self.linesp_pct.blockSignals(False)
        if hasattr(self, "vpad"):
            self.vpad.blockSignals(True)
            self.vpad.setValue(int(st.get("extra_vpadding", self.vpad.value())))
            self.vpad.blockSignals(False)
        if hasattr(self, "reflect"):
            cur = st.get("reflect", None) or "None"
            self.reflect.blockSignals(True)
            idx = max(0, self.reflect.findText(cur))
            self.reflect.setCurrentIndex(idx)
            self.reflect.blockSignals(False)

        # Stroke
        sw = int(st.get("stroke_width", 0))
        sc = st.get("stroke_color_rgba")
        if hasattr(self, "stroke_enable"):
            self.stroke_enable.blockSignals(True)
            self.stroke_enable.setChecked(sw > 0 and bool(sc))
            self.stroke_enable.blockSignals(False)
        if hasattr(self, "stroke_w"):
            self.stroke_w.blockSignals(True)
            self.stroke_w.setValue(sw)
            self.stroke_w.blockSignals(False)
        if hasattr(self, "stroke_color") and sc:
            self.stroke_color.set_rgba(tuple(sc), emit=True)

        # Glow
        gr = int(st.get("glow_radius", 0))
        gs = int(st.get("glow_softness", 5))
        gc = st.get("glow_color_rgba")
        if hasattr(self, "glow_enable"):
            self.glow_enable.blockSignals(True)
            self.glow_enable.setChecked(gr > 0 and bool(gc))
            self.glow_enable.blockSignals(False)
        if hasattr(self, "glow_r"):
            self.glow_r.blockSignals(True)
            self.glow_r.setValue(gr)
            self.glow_r.blockSignals(False)
        if hasattr(self, "glow_softness"):
            self.glow_softness.blockSignals(True)
            self.glow_softness.setValue(gs)
            self.glow_softness.blockSignals(False)
        if hasattr(self, "glow_color") and gc:
            self.glow_color.set_rgba(tuple(gc), emit=True)

        # Shadow
        if hasattr(self, "shadow_enable"):
            self.shadow_enable.blockSignals(True)
            self.shadow_enable.setChecked(bool(st.get("shadow_color_rgba")))
            self.shadow_enable.blockSignals(False)
        if hasattr(self, "shadow_dx"):
            self.shadow_dx.blockSignals(True)
            self.shadow_dx.setValue(int(st.get("shadow_dx", self.shadow_dx.value())))
            self.shadow_dx.blockSignals(False)
        if hasattr(self, "shadow_dy"):
            self.shadow_dy.blockSignals(True)
            self.shadow_dy.setValue(int(st.get("shadow_dy", self.shadow_dy.value())))
            self.shadow_dy.blockSignals(False)
        if hasattr(self, "shadow_color") and st.get("shadow_color_rgba"):
            self.shadow_color.set_rgba(tuple(st["shadow_color_rgba"]), emit=True)

        # Shake
        if hasattr(self, "shake_enable"):
            self.shake_enable.blockSignals(True)
            self.shake_enable.setChecked(bool(st.get("shake_enabled", False)))
            self.shake_enable.blockSignals(False)
        if hasattr(self, "shake_angle"):
            self.shake_angle.blockSignals(True)
            self.shake_angle.setValue(float(st.get("shake_angle_deg", self.shake_angle.value())))
            self.shake_angle.blockSignals(False)
        if hasattr(self, "shake_up"):
            self.shake_up.blockSignals(True)
            self.shake_up.setValue(int(st.get("shake_up", self.shake_up.value())))
            self.shake_up.blockSignals(False)
        if hasattr(self, "shake_down"):
            self.shake_down.blockSignals(True)
            self.shake_down.setValue(int(st.get("shake_down", self.shake_down.value())))
            self.shake_down.blockSignals(False)
        if hasattr(self, "shake_steps"):
            self.shake_steps.blockSignals(True)
            self.shake_steps.setValue(int(st.get("shake_steps", self.shake_steps.value())))
            self.shake_steps.blockSignals(False)
        if hasattr(self, "shake_base_fade"):
            self.shake_base_fade.blockSignals(True)
            self.shake_base_fade.setValue(float(st.get("shake_base_fade", self.shake_base_fade.value())))
            self.shake_base_fade.blockSignals(False)
        if hasattr(self, "shake_decay"):
            self.shake_decay.blockSignals(True)
            self.shake_decay.setValue(float(st.get("shake_decay", self.shake_decay.value())))
            self.shake_decay.blockSignals(False)
        if hasattr(self, "shake_blur"):
            self.shake_blur.blockSignals(True)
            self.shake_blur.setValue(int(st.get("shake_blur", self.shake_blur.value())))
            self.shake_blur.blockSignals(False)

        # Gradient
        g2c1 = st.get("grad2_c1_rgba"); g2c2 = st.get("grad2_c2_rgba")
        g4 = any(st.get(k) for k in ("grad4_tl_rgba","grad4_tr_rgba","grad4_bl_rgba","grad4_br_rgba"))
        en = bool(g2c1 and g2c2) or g4
        if hasattr(self, "grad_enable"):
            self.grad_enable.blockSignals(True)
            self.grad_enable.setChecked(en)
            self.grad_enable.blockSignals(False)
        if hasattr(self, "rb_grad2") and hasattr(self, "rb_grad4"):
            if g4:
                self.rb_grad4.setChecked(True)
            else:
                self.rb_grad2.setChecked(True)

        if hasattr(self, "grad2_c1") and g2c1:
            self.grad2_c1.set_rgba(tuple(g2c1), emit=True)
        if hasattr(self, "grad2_c2") and g2c2:
            self.grad2_c2.set_rgba(tuple(g2c2), emit=True)
        if hasattr(self, "grad_angle") and st.get("grad_angle_deg") is not None:
            self.grad_angle.blockSignals(True)
            self.grad_angle.setValue(float(st.get("grad_angle_deg", self.grad_angle.value())))
            self.grad_angle.blockSignals(False)

        if hasattr(self, "g4_tl") and st.get("grad4_tl_rgba"): self.g4_tl.set_rgba(tuple(st["grad4_tl_rgba"]), emit=True)
        if hasattr(self, "g4_tr") and st.get("grad4_tr_rgba"): self.g4_tr.set_rgba(tuple(st["grad4_tr_rgba"]), emit=True)
        if hasattr(self, "g4_bl") and st.get("grad4_bl_rgba"): self.g4_bl.set_rgba(tuple(st["grad4_bl_rgba"]), emit=True)
        if hasattr(self, "g4_br") and st.get("grad4_br_rgba"): self.g4_br.set_rgba(tuple(st["grad4_br_rgba"]), emit=True)
        if hasattr(self, "stroke_enable"):  self._toggle_stroke_ui(self.stroke_enable.isChecked())
        if hasattr(self, "glow_enable"):    self._toggle_glow_ui(self.glow_enable.isChecked())
        if hasattr(self, "shadow_enable"):  self._toggle_shadow_ui(self.shadow_enable.isChecked())
        if hasattr(self, "shake_enable"):   self._toggle_shake_ui(self.shake_enable.isChecked())

        if hasattr(self, "grad_enable"):
            self._toggle_gradient_ui(self.grad_enable.isChecked())
            # чтобы корректно показать активный вариант 2ц/4уг
            self._toggle_gradient_kind()

        # Форма текста
        if hasattr(self, "shape_combo"):
            shape = st.get("text_shape", "rectangle")
            shape_map = {"rectangle": 0, "oval": 1, "hexagon": 2}
            idx = shape_map.get(shape, 0)
            self.shape_combo.blockSignals(True)
            self.shape_combo.setCurrentIndex(idx)
            self.shape_combo.blockSignals(False)

    def apply_style(self, style: TextStyle):
        self.apply_state(style)
