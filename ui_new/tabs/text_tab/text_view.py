# ui_new/tabs/text_tab/text_view.py
from __future__ import annotations
import os, json
import math
from typing import Optional, List, Dict, Set
from PyQt6.QtCore import Qt, QRectF, QPointF, QPoint, QTimer, QEvent
from PyQt6.QtGui import QFont, QImage, QPainter, QColor, QTransform, QPen, QPainterPath
from PyQt6.QtWidgets import QTextEdit, QGraphicsProxyWidget, QGraphicsRectItem, QFileDialog, QMessageBox, QPushButton, QWidget
from ui_new.canvas_view import CanvasView 
from .text_overlay_item import TextOverlayItem, TextOverlayMeta
from .text_render import Renderer
from .text_style import TextStyle
from PyQt6.QtCore import pyqtSignal, Qt
from PyQt6.QtWidgets import QTextEdit
from PyQt6.QtGui import QFontDatabase
from pathlib import Path
import traceback
DEBUG = False

def _d(*a):
    if DEBUG:
        try:
            print(*a, flush=True)
        except Exception:
            pass

class InlineTextEdit(QTextEdit):
    lostFocus = pyqtSignal()
    commitRequested = pyqtSignal()

    def focusOutEvent(self, ev):
        super().focusOutEvent(ev)
        self.lostFocus.emit()

    def keyPressEvent(self, ev):
        if ev.key() in (Qt.Key.Key_Return, Qt.Key.Key_Enter) and (ev.modifiers() & Qt.KeyboardModifier.ControlModifier):
            self.commitRequested.emit()
        else:
            super().keyPressEvent(ev)


class TextCanvasViewQt(CanvasView):
    HOTKEYS = """ • [ Выделение области для текста: Shift+ЛКМ ] • [ Вращение: Ctrl+колесико (выделенный) ] •
 • [ Масштаб ленты: Ctrl+колесико ] • [ Быстрый размер шрифта: Shift+колесико ] • 
 • [ Удалить: Del ] • [ Зум: Ctrl+колесо/±, Ctrl+0 ] • [ Скейл текста: -/= ]"""
    """
    Вкладка текста на PyQt6:
    - Shift+drag: выделить область
    - Внутри области — появится QTextEdit с автопереносом
    - По выходу из фокуса: текст рендерится в QImage, кладётся как TextOverlayItem
    - Оверлей можно перемещать, вращать (Ctrl+колесо при выделении) и масштабировать (колесо)
    - Сохранение: композит в PNG, + text_info.json
    """
    def __init__(
        self,
        project,
        images: List[str],
        parent=None,
        bubbles_model=None,
        overlays_model=None,
        user_config=None,
    ):
        super().__init__(
            project,
            images,
            editable=False,
            parent=parent,
            bubbles_model=bubbles_model,
            overlays_model=overlays_model,
            user_config=user_config,
        )

        # ---- ТЕКУЩИЙ СТИЛЬ ТЕКСТА (дефолты для новых блоков) ----
        self.current_style: TextStyle = TextStyle()

        self._colorpick_active = False
        self._colorpick_preview_cb = None
        self._colorpick_commit_cb = None
        self._colorpick_cancel_cb = None
        self._renderer = Renderer()
        self._per_font_state: dict[str, dict] = {}
        self._panel = None 
                # служебное
        self._sel_rect_item: Optional[QGraphicsRectItem] = None
        self._sel_start_scene: Optional[QPointF] = None
        self._active_editor: Optional[QGraphicsProxyWidget] = None
        self._overlays: List[TextOverlayItem] = []
        os.makedirs(self.project.text_images, exist_ok=True)
        self._finalizing = False
        self._pending_overlay_updates: Set[TextOverlayItem] = set()
        self._pending_save_requested = False
        self._overlay_update_interval_ms = 50  # ~20 fps для тяжёлых обновлений масок
        self._overlay_update_timer = QTimer(self)
        self._overlay_update_timer.setInterval(self._overlay_update_interval_ms)
        self._overlay_update_timer.timeout.connect(self._flush_overlay_updates)
        self._dragging_overlays: Set[TextOverlayItem] = set()
        # хоткеи уже установлены через super().__init__() -> CanvasView.__init__() -> _install_shortcuts()
        self.font_file_map: dict[str, str] = {}  # basename -> family
        self._fonts_scanned = False
        self._cached_font_files: list[str] = []
        self._cached_font_families: list[str] = []
        self._scan_custom_fonts_once()
        self.custom_font_files = list(self._cached_font_files)
        self.custom_font_families = list(self._cached_font_families)
        # Если стартовый family не из наших файлов — фолбэк на первый доступный
        if self.font_file_map and self.current_style.font_family not in set(self.font_file_map.values()):
            self.current_style = self.current_style.with_updates(font_family=next(iter(self.font_file_map.values())))
        if hasattr(self, "_hotkeysLabel"):
            self._hotkeysLabel.setText(self.HOTKEYS)

        # Кнопка и панель линий обрезки
        from .cut_mask import CutLinesButton, CutLinesPanel, CutLinesManager
        self._cutLinesButton = CutLinesButton(self)
        self._cutLinesPanel = CutLinesPanel(self)
        self._cutLinesButton.set_panel(self._cutLinesPanel)
        self._cutLinesButton.clicked.connect(self._cutLinesButton.toggle_panel)
        self._cutLinesPanel.installEventFilter(self)

        # Менеджер линий обрезки
        self._cutLinesManager = CutLinesManager()
        self._drawing_barrier = False  # флаг рисования барьера
        self._barrier_mode = "brush"  # текущий режим: brush/eraser
        self._current_stroke = []  # текущий штрих

        # Оптимизация для режима заливки - таймер обновления цвета
        self._fill_color_update_timer = QTimer()
        self._fill_color_update_timer.setInterval(500)  # 500 мс = 0.5 сек
        self._fill_color_update_timer.timeout.connect(self._update_fill_color_preview)
        self._last_fill_cursor_pos = None  # последняя позиция курсора
        self._cached_fill_image = None  # кэшированное изображение страницы
        self._cached_fill_img_idx = None  # индекс кэшированной страницы

        # page_bboxes больше не нужны в новой архитектуре барьеров
        # Маски получают page_bbox напрямую из image_bboxes при необходимости

        # Подключаем сигналы от панели
        self._cutLinesPanel.brush_tool.toggled.connect(lambda checked: self._on_tool_changed("brush") if checked else None)
        self._cutLinesPanel.eraser_tool.toggled.connect(lambda checked: self._on_tool_changed("eraser") if checked else None)
        self._cutLinesPanel.fill_tool.toggled.connect(lambda checked: self._on_tool_changed("fill") if checked else None)
        self._cutLinesPanel.clear_button.clicked.connect(self._on_clear_barrier_mask)
        self._cutLinesPanel.hide()  # Скрываем по умолчанию

        # Кнопки перспективной трансформации
        self._transformButton = QPushButton("Трансформация", self)
        self._transformButton.setObjectName("TransformButton")
        self._transformButton.setCursor(Qt.CursorShape.PointingHandCursor)
        self._transformButton.setFixedHeight(26)
        self._transformButton.setStyleSheet("""
            QPushButton#TransformButton {
                background: #555;
                color: white;
                padding: 3px 10px;
                font-weight: 600;
                border-radius: 4px;
                border: none;
            }
            QPushButton#TransformButton:hover { background: #666; }
            QPushButton#TransformButton:pressed { background: #444; }
        """)
        self._transformButton.setEnabled(False)
        self._transformButton.clicked.connect(self._enter_transform_mode_click)

        self._transformExitButton = QPushButton("Выйти из трансформации", self)
        self._transformExitButton.setObjectName("TransformExitButton")
        self._transformExitButton.setCursor(Qt.CursorShape.PointingHandCursor)
        self._transformExitButton.setFixedHeight(26)
        self._transformExitButton.setStyleSheet("""
            QPushButton#TransformExitButton {
                background: #b32727;
                color: white;
                padding: 3px 10px;
                font-weight: 600;
                border-radius: 4px;
                border: none;
            }
            QPushButton#TransformExitButton:hover { background: #c33; }
            QPushButton#TransformExitButton:pressed { background: #921c1c; }
        """)
        self._transformExitButton.clicked.connect(self._exit_transform_mode_click)
        self._transformExitButton.hide()

        self._transformResetButton = QPushButton("Сбросить", self)
        self._transformResetButton.setObjectName("TransformResetButton")
        self._transformResetButton.setCursor(Qt.CursorShape.PointingHandCursor)
        self._transformResetButton.setFixedHeight(26)
        self._transformResetButton.setStyleSheet("""
            QPushButton#TransformResetButton {
                background: #b32727;
                color: white;
                padding: 3px 10px;
                font-weight: 600;
                border-radius: 4px;
                border: none;
            }
            QPushButton#TransformResetButton:hover { background: #c33; }
            QPushButton#TransformResetButton:pressed { background: #921c1c; }
        """)
        self._transformResetButton.clicked.connect(self._reset_transform_click)
        self._transformResetButton.hide()

        # авто-загрузка существующих текстовых оверлеев
        QTimer.singleShot(0, self._load_overlays_from_json)

    def build_bubble_header(self, bid: int) -> List[QWidget]:
        btn = QPushButton("Создать текст")
        btn.setFixedWidth(110)
        btn.setCursor(Qt.CursorShape.PointingHandCursor)
        btn.clicked.connect(lambda checked=False, b=bid: self._create_text_from_bubble(b))
        return [btn]

    def _create_text_from_bubble(self, bid: int) -> None:
        b = self.bubbles.get(bid)
        if not b or not b.rect_coords:
            return
        text = self._collect_bubble_texts(bid).get("text", "").strip()
        if not text:
            return
        rect_scene = self._scene_rect_from_coords(b.img_idx, b.rect_coords).normalized()
        self._create_text_from_rect(rect_scene, text)

    def _create_text_from_rect(self, rect_scene: QRectF, text: str) -> None:
        if getattr(self, "_finalizing", False):
            return
        self._finalizing = True
        try:
            text = (text or "").strip()
            if not text:
                return

            img_idx = self._image_index_by_point(rect_scene.center())
            if img_idx is None:
                return
            rect_w_int = int(rect_scene.width())

            st_for_render = self.current_style.ensure_exclusive_gradients()
            qimg = self._renderer.big_renderer(**st_for_render.to_renderer_kwargs(text=text, width_px=rect_w_int))
            center_x = rect_scene.left() + rect_scene.width() / 2.0
            center_y = rect_scene.top() + rect_scene.height() / 2.0
            u, v = self._uv_from_scene(img_idx, center_x, center_y)
            try:
                _orig_page_img = QImage(self.images[img_idx])
                _orig_w_px = int(_orig_page_img.width()) if not _orig_page_img.isNull() else 0
            except Exception:
                _orig_w_px = 0
            _orig_w_px = max(1, _orig_w_px)
            w_frac = max(0.001, float(qimg.width()) / float(_orig_w_px))

            meta = TextOverlayMeta(
                img_idx=img_idx, u=float(u), v=float(v), w_frac=float(w_frac),
                user_scale=1.0, angle=0.0,
                file=self._unique_text_png_name(img_idx),
                text=text,
                style=st_for_render,
            )

            item = TextOverlayItem(
                meta,
                qimg,
                None,
                on_changed=self._on_item_changed,
                on_drag_state_changed=self._on_overlay_drag_state,
            )
            self._setup_overlay_masks_callback(item)
            item._on_changed_view = self._on_item_changed
            item._apply_pixmap(target_width_px=None)

            pm = item.pixmap()
            left_x = center_x - pm.width() / 2.0
            top_y = center_y - pm.height() / 2.0
            item.setPos(left_x, top_y)
            item.setScale(1.0)
            self.scene.addItem(item)
            self._overlays.append(item)

            qimg.save(os.path.join(self.project.text_images, meta.file))
            self._save_text_info_json()

        finally:
            self._finalizing = False

    def _fonts_dir(self) -> Path:
        """
        Определяем каталог шрифтов:
        - приоритет: корень проекта /fonts
        - запасной вариант: cwd/fonts
        """
        candidates = [
            Path(__file__).resolve().parents[3] / "fonts",
            Path.cwd() / "fonts",
        ]
        for p in candidates:
            if p.is_dir():
                return p
        return candidates[0]

    def _load_custom_fonts_by_file(self) -> list[str]:
        """
        Загружает все шрифты из ./fonts.
        Возвращает список БАЗОВЫХ имён файлов (без расширений),
        а self.font_file_map заполняет как: basename -> QFont family.
        """
        self._scan_custom_fonts_once()
        return list(self._cached_font_files)

    def _load_custom_fonts(self):
        self._scan_custom_fonts_once()
        return list(self._cached_font_families)

    def _scan_custom_fonts_once(self):
        if self._fonts_scanned:
            return
        self._fonts_scanned = True
        self.font_file_map.clear()
        self._cached_font_files = []
        self._cached_font_families = []

        fonts_dir = self._fonts_dir()
        if not fonts_dir.is_dir():
            return

        exts = {".ttf", ".otf", ".ttc", ".otc"}
        family_seen: set[str] = set()
        for path in fonts_dir.rglob("*"):
            if not path.is_file() or path.suffix.lower() not in exts:
                continue
            fid = QFontDatabase.addApplicationFont(str(path))
            if fid < 0:
                continue
            fams = QFontDatabase.applicationFontFamilies(fid)
            if not fams:
                continue
            family = fams[0]
            base = path.stem
            self.font_file_map[base] = family
            self._cached_font_files.append(base)
            if family not in family_seen:
                family_seen.add(family)
                self._cached_font_families.append(family)

    # ---- Работа со стилем ----
    def _set_style(self, style: TextStyle, *, sync_panel: bool = True, refresh_editor: bool = True):
        """Обновить текущий стиль и синхронизировать зависимые элементы."""
        self.current_style = style.ensure_exclusive_gradients()
        if refresh_editor:
            self._refresh_active_editor()
        if sync_panel and self._panel:
            if hasattr(self._panel, "apply_style"):
                self._panel.apply_style(self.current_style)
            elif hasattr(self._panel, "apply_state"):
                self._panel.apply_state(self.current_style.to_dict())

    def _patch_style(self, patch: dict, *, sync_panel: bool = True, refresh_editor: bool = True):
        self._set_style(self.current_style.with_updates(**patch), sync_panel=sync_panel, refresh_editor=refresh_editor)

    def get_clean_overlays_visible(self) -> bool:
        """Вернуть текущую видимость слоёв клина."""
        if getattr(self, "overlays_model", None):
            return bool(self.overlays_model.is_visible())
        return True

    def refresh_current_page_image(self):
        """
        Мягко обновляет картинку текущей страницы без перестроения ленты:
        - НЕ меняем sceneRect, image_bboxes, позиционирование и масштаб
        - Ожидаем то же разрешение; при несовпадении — предупреждаем и выходим
        - Обновляем только pixmap соответствующего QGraphicsPixmapItem
        """
        # 1) Текущий индекс
        current_idx = self._current_page_idx()
        if current_idx < 0 or current_idx >= len(self.images):
            QMessageBox.warning(self, "Ошибка", "Не удалось определить текущую страницу")
            return

        # 2) Путь к файлу и загрузка
        current_path = self.images[current_idx]
        if not isinstance(current_path, str):
            QMessageBox.warning(self, "Ошибка", "Текущее изображение не является файлом")
            return
        if not os.path.exists(current_path):
            QMessageBox.warning(self, "Ошибка", f"Файл не найден:\n{current_path}")
            return

        new_qimg = QImage(current_path)
        if new_qimg.isNull():
            QMessageBox.warning(self, "Ошибка", f"Не удалось загрузить изображение:\n{current_path}")
            return

        # 3) Проверяем, что размер совпадает с ожидаемым из image_bboxes
        if current_idx >= len(self.image_bboxes):
            QMessageBox.warning(self, "Ошибка", "Внутренняя геометрия ленты не готова")
            return

        bbox = self.image_bboxes[current_idx]
        expected_w = int(round(bbox.width()))
        expected_h = int(round(bbox.height()))
        new_w = new_qimg.width()
        new_h = new_qimg.height()

        # Если размеры страницы изменились — ничего не передвигаем и не растягиваем.
        # Просто предупреждаем и выходим.
        if expected_w > 0 and expected_h > 0 and (new_w != expected_w or new_h != expected_h):
            QMessageBox.warning(
                self,
                "Несовпадение размеров",
                f"Ожидалось {expected_w}×{expected_h}, а в файле {new_w}×{new_h}.\n"
                "Страница не обновлена, чтобы не нарушить позиционирование."
            )
            return

        # 4) Обновляем только pixmap у текущего QGraphicsPixmapItem
        if current_idx < len(self.image_items):
            from PyQt6.QtGui import QPixmap
            new_pixmap = QPixmap.fromImage(new_qimg)
            self.image_items[current_idx].setPixmap(new_pixmap)

            # Обновляем кэш для инструмента заливки, если он смотрит на эту страницу
            self._cached_fill_image = new_qimg
            self._cached_fill_img_idx = current_idx

            # Просим перерисовать только viewport (без центрирования/изменения сцены)
            self.viewport().update()
            # НИЧЕГО не двигаем: не трогаем self.image_bboxes, sceneRect, центр и оверлеи
        else:
            QMessageBox.warning(self, "Ошибка", "Индекс страницы вне диапазона")


    def ui_set_line_spacing_percent(self, pct: int): self._patch_style({"line_spacing_percent": max(0, int(pct))})
    def ui_set_extra_vpadding(self, px: int): self._patch_style({"extra_vpadding": max(0, int(px))})
    def ui_set_reflect(self, mode: str | None): self._patch_style({"reflect": mode if mode in ("x", "y") else None})
    def ui_set_text_shape(self, shape: str): self._patch_style({"text_shape": shape if shape in ("rectangle", "oval", "hexagon") else "rectangle"})
    def ui_set_shake(self, shake: dict | None):
        """Установить параметры шлейфа (shake) одним вызовом."""
        if not shake:
            self._patch_style({"shake_enabled": False})
            return
        patch = dict(shake_enabled=True)
        for key in ("angle_deg", "up", "down", "steps", "base_fade", "decay", "blur"):
            if key in shake:
                patch[f"shake_{'angle_deg' if key=='angle_deg' else key}"] = shake[key]
        self._patch_style(patch)

    def ui_set_stroke(self, width: int, rgba: tuple[int,int,int,int] | None):
        self._patch_style({"stroke_width": max(0, int(width)), "stroke_color_rgba": rgba})

    def ui_set_glow(self, radius: int, softness: int, rgba: tuple[int,int,int,int] | None):
        self._patch_style({"glow_radius": max(0, int(radius)), "glow_softness": max(0, int(softness)), "glow_color_rgba": rgba})

    def ui_set_shadow(self, dx: int, dy: int, rgba: tuple[int,int,int,int] | None):
        self._patch_style({"shadow_dx": int(dx), "shadow_dy": int(dy), "shadow_color_rgba": rgba})

    def ui_set_gradient2(self, c1: tuple[int,int,int,int] | None, c2: tuple[int,int,int,int] | None, angle_deg: float):
        self._patch_style({"grad2_c1_rgba": c1, "grad2_c2_rgba": c2, "grad_angle_deg": float(angle_deg), "grad4_tl_rgba": None, "grad4_tr_rgba": None, "grad4_bl_rgba": None, "grad4_br_rgba": None})

    def ui_set_gradient4(self, tl, tr, bl, br):
        self._patch_style({"grad4_tl_rgba": tl, "grad4_tr_rgba": tr, "grad4_bl_rgba": bl, "grad4_br_rgba": br, "grad2_c1_rgba": None, "grad2_c2_rgba": None})

    def ui_set_font_family(self, family: str):
        # сохранить предыдущее состояние под старым семейством
        prev_family = getattr(self.current_style, "font_family", None)
        if prev_family:
            self._per_font_state[prev_family] = self._state_snapshot()

        target_family = family or "Arial"
        st = self._per_font_state.get(target_family)
        if st:
            st = dict(st, font_family=target_family)
            self._apply_state(st, sync_panel=True)
        else:
            self._patch_style({"font_family": target_family})
            if self._panel and hasattr(self._panel, "apply_state"):
                st_now = self._state_snapshot()
                st_now["font_family"] = target_family
                self._panel.apply_state(st_now)

    def ui_set_font_size(self, size: int):
        self._patch_style({"font_size": max(8, int(size))})

    def ui_set_font_color(self, qc: QColor):
        self._patch_style({"font_color_rgba": (qc.red(), qc.green(), qc.blue(), qc.alpha())})

    def ui_set_line_spacing(self, px: int):
        self._patch_style({"line_spacing": max(0, int(px))})

    def ui_set_align(self, align: str):
        if align not in ("left", "center", "right"):
            align = "left"
        self._patch_style({"align": align})

    def attach_panel(self, panel):
        """Позволяем вьюхе дёргать элементы панели (синхронизация виджетов)."""
        self._panel = panel

    def _layout_top_labels(self):
        """Переопределяем метод из CanvasView для добавления кнопки линий обрезки."""
        # Вызываем родительский метод для базового позиционирования
        super()._layout_top_labels()

        next_x = self._scaleLabel.x() + self._scaleLabel.width() + 6
        btn_y = 8

        # Позиционируем кнопку линий обрезки справа от масштаба
        if hasattr(self, '_cutLinesButton'):
            self._cutLinesButton.adjustSize()
            self._cutLinesButton.move(next_x, btn_y)
            next_x = self._cutLinesButton.x() + self._cutLinesButton.width() + 6

        # Кнопки перспективной трансформации
        if hasattr(self, '_transformButton'):
            self._transformButton.adjustSize()
            self._transformButton.move(next_x, btn_y)
        if hasattr(self, '_transformExitButton') and self._transformExitButton.isVisible():
            self._transformExitButton.adjustSize()
            self._transformExitButton.move(next_x, btn_y)
            next_x = self._transformExitButton.x() + self._transformExitButton.width() + 6
        if hasattr(self, '_transformResetButton') and self._transformResetButton.isVisible():
            self._transformResetButton.adjustSize()
            self._transformResetButton.move(next_x, btn_y)

        self._layout_canvas_controls()

        # Перепозиционируем панель, если она видима
        if hasattr(self, '_cutLinesPanel') and self._cutLinesPanel.isVisible():
            self._cutLinesPanel.position_in_parent()

    def _state_snapshot(self) -> dict:
        """Собрать все текущие параметры текста в dict."""
        st = self.current_style.to_dict()
        st.update(
            custom_font_families=self.custom_font_families,
            custom_font_files=self.custom_font_files,
            font_file_map=self.font_file_map,
        )
        return st

    def _apply_state(self, st: dict, *, sync_panel: bool = True):
        """Применить dict параметров к текущему стилю и при желании обновить панель."""
        style = TextStyle.from_dict(st)
        self._set_style(style, sync_panel=sync_panel, refresh_editor=True)


    def _refresh_active_editor(self):
        """Применить текущие настройки к открытому QTextEdit (если он есть)."""
        if not self._active_editor:
            return
        te: InlineTextEdit = self._active_editor.widget()  # type: ignore
        if not te:
            return
        st = self.current_style
        f = QFont(st.font_family, pointSize=st.font_size)
        te.setFont(f)
        # цвет в редакторе — через stylesheet (курсор/выделения не трогаем)
        r, g, b, a = st.font_color_rgba
        te.setStyleSheet(
            f"background: rgba(125,125,125,220); padding: 6px; border-radius: 6px;"
            f"color: rgba({r},{g},{b},{a});"
        )
        # ширина документа — не меняем, её задаём при создании
        # выравнивание/межстрочный учтём в итоговом рендере

    def export_overlays_with_dialog(self, method: str = "scene", oversample: int = 1):
        # спросить папку
        start_dir = getattr(self.project, "saved_dir", None) or os.getcwd()
        out_dir = QFileDialog.getExistingDirectory(self, "Куда сохранить PNG", start_dir)
        if not out_dir:
            return  # пользователь отменил

        try:
            if method == "manual":
                self.save_all_pages(out_dir, oversample=oversample)
            else:
                self._composite_to_directory(out_dir)
            QMessageBox.information(self, "Готово", f"Сохранено в:\n{out_dir}")
        except Exception as e:
            traceback.print_exc()
            QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить:\n{e}")

    def _composite_to_directory(self, out_dir: str):
        if DEBUG: print("Используется рендер _composite_to_directory")
        os.makedirs(out_dir, exist_ok=True)

        # 1) Подготовим QImage'ы оригиналов
        originals: Dict[int, QImage] = {}
        for idx, path in enumerate(self.images):
            img = QImage(path)
            if not img.isNull():
                originals[idx] = img.convertToFormat(QImage.Format.Format_ARGB32_Premultiplied)

        sc = self.scene

        # 2) По страницам: рисуем оверлеи именно так, как их рисует сцена
        for idx, base_img in originals.items():
            page_bbox = self.image_bboxes[idx]  # QRectF в координатах сцены

            # Накладываем слой клина, если он есть и не скрыт
            if getattr(self, "overlays_model", None) and self.overlays_model.is_visible():
                ov = self.overlays_model.get(idx)
                if ov is not None and not ov.isNull():
                    lay_img = ov
                    if lay_img.size() != base_img.size():
                        lay_img = lay_img.scaled(base_img.size(), Qt.AspectRatioMode.IgnoreAspectRatio,
                                                 Qt.TransformationMode.SmoothTransformation)
                    p_base = QPainter(base_img)
                    p_base.setRenderHint(QPainter.RenderHint.Antialiasing, True)
                    p_base.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)
                    p_base.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)
                    p_base.drawImage(0, 0, lay_img)
                    p_base.end()

            # Прозрачный слой под оверлеи, размером с оригинал
            overlay = QImage(base_img.size(), QImage.Format.Format_ARGB32_Premultiplied)
            overlay.fill(Qt.GlobalColor.transparent)

            # Временно прячем всё, кроме наших текстовых оверлеев ЭТОЙ страницы
            toggled = []
            for item in sc.items():
                keep = isinstance(item, TextOverlayItem) and getattr(item, "meta", None) and item.meta.img_idx == idx
                if not keep and item.isVisible():
                    toggled.append(item)
                    item.setVisible(False)

            try:
                # Рендерим только видимые элементы (то есть наши оверлеи) в слой overlay
                p = QPainter(overlay)
                p.setRenderHint(QPainter.RenderHint.Antialiasing, True)
                p.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)
                p.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)

                # ВАЖНО: target = размер оригинала, source = bbox страницы в сцене.
                sc.render(
                    p,
                    QRectF(0, 0, overlay.width(), overlay.height()),
                    page_bbox,
                    Qt.AspectRatioMode.IgnoreAspectRatio,
                )
                p.end()
            finally:
                # Вернём видимость всем, кого прятали
                for it in toggled:
                    it.setVisible(True)

            # 3) Композитим слой с оверлеями на оригинал
            p2 = QPainter(base_img)
            p2.setRenderHint(QPainter.RenderHint.Antialiasing, True)
            p2.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)
            p2.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)
            p2.drawImage(0, 0, overlay)
            p2.end()

            # Сохраняем как 1.png, 2.png, ...
            base_img.save(os.path.join(out_dir, f"{idx}.png"))

        # метаданные
        self._save_text_info_json()


    def start_color_picker(self, on_preview, on_commit, on_cancel=None):
        """Входит в режим пипетки: превью при движении мыши, подтверждение ЛКМ, отмена ПКМ/ESC."""
        if self._colorpick_active:
            return
        self._colorpick_active = True
        self._colorpick_preview_cb = on_preview
        self._colorpick_commit_cb = on_commit
        self._colorpick_cancel_cb = on_cancel
        # курсор «прицела» по желанию:
        self.setCursor(Qt.CursorShape.CrossCursor)

    def _finish_color_picker(self, commit_color=None, cancelled=False):
        self.unsetCursor()
        self._colorpick_active = False
        prev = self._colorpick_preview_cb; com = self._colorpick_commit_cb; can = self._colorpick_cancel_cb
        self._colorpick_preview_cb = self._colorpick_commit_cb = self._colorpick_cancel_cb = None
        if cancelled:
            if can: can()
            return
        if commit_color is not None and com:
            com(commit_color)

    def _sample_color_at_widget_pos(self, pos_widget) -> QColor | None:
        """Быстрый способ: берём текущий снапшот виджета и читаем пиксель под курсором."""
        # Защита от выхода за пределы
        x = int(pos_widget.x()); y = int(pos_widget.y())
        if x < 0 or y < 0 or x >= self.width() or y >= self.height():
            return None
        pm = self.grab()  # QPixmap текущего визуального состояния
        img = pm.toImage()
        c = img.pixelColor(x, y)
        return QColor(c)

    def _schedule_overlay_update(self, it: TextOverlayItem):
        """Ставим обновление масок/отрисовки в очередь с ограничением fps."""
        self._pending_overlay_updates.add(it)
        self._pending_save_requested = True
        if not self._overlay_update_timer.isActive():
            self._overlay_update_timer.start()

    def _flush_overlay_updates(self):
        """Ограниченная по частоте отрисовка масок + отложенное сохранение JSON."""
        items = list(self._pending_overlay_updates)
        self._pending_overlay_updates.clear()
        for it in items:
            try:
                self._cutLinesManager.invalidate_overlay_cache(it)
            except Exception:
                pass
            try:
                it.update()
            except Exception:
                pass
        if items:
            self.viewport().update()

        if self._pending_save_requested and not self._dragging_overlays:
            self._pending_save_requested = False
            self._save_text_info_json()

        if not self._pending_overlay_updates and (not self._pending_save_requested or self._dragging_overlays):
            self._overlay_update_timer.stop()

    def _on_overlay_drag_state(self, active: bool, item: TextOverlayItem):
        """Трекер перетаскивания: блокируем частые сохранения до отпускания мыши."""
        if active:
            self._dragging_overlays.add(item)
        else:
            self._dragging_overlays.discard(item)
            if not self._dragging_overlays:
                self._flush_overlay_updates()

    def _on_item_changed(self, reason: str, it):
        """
        reason in {"pos","scale","transform","angle"}.
        Обновляем u/v и (при переходе на другую страницу) img_idx.
        Потом пересохраняем text_info.json.
        """
        _d(f"\n{'='*80}")
        _d(f"[_on_item_changed] CALLED with reason='{reason}' for id={id(it)}")

        # 1) позиция: пересчёт в uv и, при необходимости, смена страницы
        if reason in ("pos", "transform", "scale", "angle"):
            quad_scene: list[QPointF] = it._current_quad_scene() if hasattr(it, "_current_quad_scene") else []
            if not quad_scene:
                br = it.boundingRect()
                quad_scene = [
                    it.mapToScene(br.topLeft()),
                    it.mapToScene(br.topRight()),
                    it.mapToScene(br.bottomRight()),
                    it.mapToScene(br.bottomLeft()),
                ]

            centroid = QPointF(
                sum(p.x() for p in quad_scene) / len(quad_scene),
                sum(p.y() for p in quad_scene) / len(quad_scene),
            ) if quad_scene else it.mapToScene(QPointF(0, 0))

            _d(f"[CHANGE-{reason}] id={id(it)} centroid=({centroid.x():.3f},{centroid.y():.3f}) "
                f"page={it.meta.img_idx} w_frac={it.meta.w_frac:.6f} scale={it.scale():.6f} angle={it.rotation():.2f}")

            # Какая страница теперь под центром?
            new_idx = self._image_index_by_point(centroid)

            # Если позиция вышла за страницу, ничего не ломаем — просто не сохраняем, но и не падаем
            if new_idx is None:
                new_idx = it.meta.img_idx

            # Если страница изменилась — обновляем индекс и w_frac относительно новой страницы
            if new_idx != it.meta.img_idx:
                it.meta.img_idx = int(new_idx)
                # DPI-aware: пересчёт w_frac под новую страницу
                try:
                    _orig_page_img = QImage(self.images[new_idx])
                    _orig_w_px = int(_orig_page_img.width()) if not _orig_page_img.isNull() else 0
                except Exception:
                    _orig_w_px = 0
                _orig_w_px = max(1, _orig_w_px)
                it.meta.w_frac = max(0.001, float(it._base.width()) / float(_orig_w_px))
                # НЕ пересоздаем pixmap - оставляем размер оверлея без изменений

            qt_scale = float(it.scale())
            if abs(qt_scale - it.meta.user_scale) > 0.001:
                it.meta.user_scale = qt_scale
            it.meta.angle = float(it.rotation())

            if quad_scene:
                quad_uv = [self._uv_from_scene(new_idx, p.x(), p.y()) for p in quad_scene]
                u, v = self._uv_from_scene(new_idx, centroid.x(), centroid.y())
                it.meta.u = float(u)
                it.meta.v = float(v)
                if not it.transform().isIdentity():
                    it.meta.transform_uv = [(float(u1), float(v1)) for u1, v1 in quad_uv]
                else:
                    it.meta.transform_uv = None
                _d(f"[CHANGE-{reason}-uv] id={id(it)} -> uv=({it.meta.u:.6f},{it.meta.v:.6f}) "
                    f"pixmap={it.pixmap().width()}x{it.pixmap().height()}")

        # 2) угол уже пишется в item (mouseReleaseEvent / wheelEvent), JSON сохраняем с троттлингом
        self._schedule_overlay_update(it)
        _d(f"{'='*80}\n")

    # ---------------- хоткеи ----------------
    def _install_shortcuts(self):
        from PyQt6.QtGui import QAction, QKeySequence
        from PyQt6.QtCore import Qt

        def _with_ctx(act):
            act.setShortcutContext(Qt.ShortcutContext.WidgetWithChildrenShortcut)
            self.addAction(act)
            return act

        # Зум холста (НЕ вызываем super(), чтобы избежать конфликтов)
        # Устанавливаем только Ctrl-комбинации для зума холста
        act_zi = _with_ctx(QAction(self))
        act_zi.setShortcuts([QKeySequence("Ctrl++"), QKeySequence("Ctrl+=")])
        act_zi.triggered.connect(lambda: self._zoom_canvas(1.1))

        act_zo = _with_ctx(QAction(self))
        act_zo.setShortcut(QKeySequence("Ctrl+-"))
        act_zo.triggered.connect(lambda: self._zoom_canvas(1/1.1))

        act_z0 = _with_ctx(QAction(self))
        act_z0.setShortcut(QKeySequence("Ctrl+0"))
        act_z0.triggered.connect(lambda: self._set_canvas_scale(1.0))

        # Остальные шорткаты от CanvasView (T, Delete, G) наследуются автоматически
        # act_save = QAction(self); act_save.setShortcut(QKeySequence.StandardKey.Save)
        # act_save.triggered.connect(self.save_all_pages); self.addAction(act_save)

    # ---------------- выделение области ----------------
    def mousePressEvent(self, e):
        if self._colorpick_active:
            # Блокируем стандартное выделение, чтобы пипетка не сбрасывала выбранный оверлей
            e.accept()
            return
        # Обработка панели барьеров
        if self._cutLinesPanel.isVisible():
            tool = self._cutLinesPanel.get_current_tool()

            # ПКМ в режиме заливки
            if e.button() == Qt.MouseButton.RightButton and tool == "fill":
                pos_scene = self.mapToScene(e.pos())
                img_idx = self._image_index_by_point(pos_scene)

                if img_idx is not None and img_idx < len(self.images):
                    try:
                        source_img = self._get_fill_source_image(img_idx)

                        if source_img is not None and not source_img.isNull():
                            page_bbox = self.image_bboxes[img_idx]
                            tolerance = self._cutLinesPanel.tolerance_spinbox.value()

                            # Выполняем заливку
                            overlay = self._get_overlay_at_scene_pos(pos_scene)
                            clip_rect_scene = None
                            if overlay is not None:
                                # Берем boundingRect оверлея в координатах сцены
                                br_item = overlay.boundingRect()
                                pts = [overlay.mapToScene(p) for p in [br_item.topLeft(), br_item.topRight(),
                                                                    br_item.bottomLeft(), br_item.bottomRight()]]
                                xs = [p.x() for p in pts]; ys = [p.y() for p in pts]
                                clip_rect_scene = QRectF(min(xs), min(ys), max(xs)-min(xs), max(ys)-min(ys)).toAlignedRect()

                            success = self._cutLinesManager.flood_fill_from_point(
                                pos_scene.toPoint(),
                                tolerance,
                                source_img,
                                img_idx,
                                page_bbox,
                                clip_rect_scene
                            )

                            if success:
                                # Обновляем все оверлеи на этой странице
                                for overlay in self._overlays:
                                    if overlay.meta.img_idx == img_idx:
                                        overlay.update()

                                # Обновляем viewport
                                self.viewport().update()
                    except Exception as ex:
                        print(f"Ошибка при заливке: {ex}")
                        import traceback
                        traceback.print_exc()
                return

            # ПКМ для рисования барьера или переключения видимости
            if e.button() == Qt.MouseButton.RightButton:
                # Ctrl+ПКМ - переключаем видимость компоненты
                if e.modifiers() & Qt.KeyboardModifier.ControlModifier:
                    pos_scene = self.mapToScene(e.pos())
                    overlay = self._get_overlay_at_scene_pos(pos_scene)
                    if overlay:
                        img_idx = overlay.meta.img_idx
                        page_bbox = self.image_bboxes[img_idx] if 0 <= img_idx < len(self.image_bboxes) else None
                        if page_bbox:
                            toggled = self._cutLinesManager.toggle_component_at_pos(
                                overlay, pos_scene.toPoint(), page_bbox
                            )
                            if toggled:
                                overlay.prepareGeometryChange()
                                overlay.update()
                                self.viewport().update()
                    return

                # ПКМ без Ctrl - рисуем кистью/ластиком
                if tool in ("brush", "eraser"):
                    self._drawing_barrier = True
                    self._barrier_mode = tool
                    pos_scene = self.mapToScene(e.pos())
                    self._current_stroke = [pos_scene.toPoint()]
                    self.viewport().update()
                    return

        # Обработка Shift+ЛКМ для выделения области текста
        if e.modifiers() & Qt.KeyboardModifier.ShiftModifier and e.button() == Qt.MouseButton.LeftButton:
            self._sel_start_scene = self.mapToScene(e.pos())
            if self._sel_rect_item:
                self.scene.removeItem(self._sel_rect_item)
            self._sel_rect_item = self.scene.addRect(QRectF(self._sel_start_scene, self._sel_start_scene),
                                                     pen=Qt.GlobalColor.blue)
            try:
                # Держим рамку поверх оверлеев клина и текста
                self._sel_rect_item.setZValue(50_000)
            except Exception:
                pass
            return
        super().mousePressEvent(e)

    def mouseMoveEvent(self, e):
        if self._colorpick_active:
            qc = self._sample_color_at_widget_pos(e.pos())
            if qc and self._colorpick_preview_cb:
                self._colorpick_preview_cb(qc)
            return

        # Обработка режима заливки - обновляем превью цвета
        if self._cutLinesPanel.isVisible() and self._cutLinesPanel.get_current_tool() == "fill":
            pos_scene = self.mapToScene(e.pos())
            self._last_fill_cursor_pos = pos_scene
            return

        # Обработка рисования барьера кистью/ластиком
        if hasattr(self, '_drawing_barrier') and self._drawing_barrier:
            pos_scene = self.mapToScene(e.pos())
            self._current_stroke.append(pos_scene.toPoint())
            self.viewport().update()
            return

        if self._sel_rect_item and self._sel_start_scene:
            cur = self.mapToScene(e.pos())
            r = QRectF(self._sel_start_scene, cur).normalized()
            self._sel_rect_item.setRect(r)
            return
        super().mouseMoveEvent(e)

    def mouseReleaseEvent(self, e):
        if self._colorpick_active:
            if e.button() == Qt.MouseButton.LeftButton:
                qc = self._sample_color_at_widget_pos(e.pos())
                self._finish_color_picker(commit_color=qc, cancelled=False)
            elif e.button() == Qt.MouseButton.RightButton:
                self._finish_color_picker(cancelled=True)
            return

        # Обработка окончания рисования барьера
        if e.button() == Qt.MouseButton.RightButton and hasattr(self, '_drawing_barrier') and self._drawing_barrier:
            self._drawing_barrier = False

            # Применяем штрих к маске
            if len(self._current_stroke) >= 2:
                pos_scene = self._current_stroke[0]
                img_idx = self._image_index_by_point(QPointF(pos_scene))

                if img_idx is not None:
                    page_bbox = self.image_bboxes[img_idx] if 0 <= img_idx < len(self.image_bboxes) else None

                    if page_bbox:
                        # Получаем или создаём маску
                        mask_w = int(page_bbox.width())
                        mask_h = int(page_bbox.height())
                        mask = self._cutLinesManager.get_or_create_mask(img_idx, mask_w, mask_h)

                        # Преобразуем координаты штриха в локальные координаты страницы
                        local_points = []
                        for pt in self._current_stroke:
                            local_x = int(pt.x() - page_bbox.left())
                            local_y = int(pt.y() - page_bbox.top())
                            local_points.append(QPoint(local_x, local_y))

                        # Применяем штрих
                        brush_size = self._cutLinesPanel.brush_size_spinbox.value()
                        mode = "add" if self._barrier_mode == "brush" else "erase"
                        self._cutLinesManager.draw_on_mask(img_idx, local_points, brush_size, mode)

                        # Обновляем все оверлеи на этой странице
                        for overlay in self._overlays:
                            if overlay.meta.img_idx == img_idx:
                                overlay.update()

            self._current_stroke = []
            self.viewport().update()
            return

        if self._sel_rect_item and self._sel_start_scene:
            rect = self._sel_rect_item.rect().normalized()
            self.scene.removeItem(self._sel_rect_item); self._sel_rect_item = None
            self._sel_start_scene = None
            if rect.width() >= 20 and rect.height() >= 10:
                self._open_text_editor(rect)
            return
        super().mouseReleaseEvent(e)

    # ---------------- ввод текста ----------------
    def _open_text_editor(self, rect_scene: QRectF):
        default = self._default_text_in_rect(rect_scene) if hasattr(self, "_default_text_in_rect") else ""
        te = InlineTextEdit()
        te.setPlainText(default)
        te.setFrameStyle(0)
        te.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        te.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAsNeeded)
        # применяем текущие настройки
        st = self.current_style
        f = QFont(st.font_family, pointSize=st.font_size)
        te.setFont(f)
        r,g,b,a = st.font_color_rgba
        te.setStyleSheet(
            f"background: rgba(125,125,125,220); padding: 6px; border-radius: 6px;"
            f"color: rgba({r},{g},{b},{a});"
        )
        # ширина разметки = ширина выделения
        te.document().setTextWidth(rect_scene.width())
        te.setFixedSize(int(rect_scene.width()), int(rect_scene.height()))

        proxy = self.scene.addWidget(te)
        proxy.setZValue(20000)
        proxy.setPos(rect_scene.topLeft())
        proxy.setPreferredSize(rect_scene.size())
        te.setFocus()

        te.lostFocus.connect(lambda: self._finalize_text_editor(te, proxy, rect_scene))
        te.commitRequested.connect(lambda: self._finalize_text_editor(te, proxy, rect_scene))
        self._active_editor = proxy



    def _default_text_in_rect(self, rect_scene) -> str:
        for b in self.bubbles.values():
            x, y = self._scene_from_uv(b.img_idx, b.img_u, b.img_v)
            if rect_scene.contains(QPointF(x, y)):
                if b.text_widget:
                    return b.text_widget.toPlainText()
                for rec in self.project.bubbles:
                    try:
                        if int(rec.get('id')) == int(b.id):
                            return rec.get('text', '') or ''
                    except Exception:
                        pass
                break
        return ''

    def _finalize_text_editor(self, te: QTextEdit, proxy, rect_scene: QRectF):
        if getattr(self, "_finalizing", False):
            return
        self._finalizing = True
        removed_from_scene = False
        try:
            text = te.toPlainText().strip()
            if not text:
                self.scene.removeItem(proxy)
                removed_from_scene = True
                return

            img_idx = self._image_index_by_point(rect_scene.center())
            if img_idx is None:
                self.scene.removeItem(proxy)
                removed_from_scene = True
                return
            rect_w_int = int(rect_scene.width())
            rect_h_int = int(rect_scene.height())

            st_for_render = self.current_style.ensure_exclusive_gradients()
            qimg = self._renderer.big_renderer(**st_for_render.to_renderer_kwargs(text=text, width_px=rect_w_int))
            #qimg = self._trim_transparent(qimg, alpha_threshold=8, margin=0)
            # метаданные - UV хранятся для ЦЕНТРА элемента
            # Вычисляем центр rect_scene
            center_x = rect_scene.left() + rect_scene.width() / 2.0
            center_y = rect_scene.top() + rect_scene.height() / 2.0
            u, v = self._uv_from_scene(img_idx, center_x, center_y)
            page_w = max(1.0, self.image_bboxes[img_idx].width())
            try:
                _orig_page_img = QImage(self.images[img_idx])
                _orig_w_px = int(_orig_page_img.width()) if not _orig_page_img.isNull() else 0
            except Exception:
                _orig_w_px = 0
            _orig_w_px = max(1, _orig_w_px)
            w_frac = max(0.001, float(qimg.width()) / float(_orig_w_px))

            meta = TextOverlayMeta(
                img_idx=img_idx, u=float(u), v=float(v), w_frac=float(w_frac),
                user_scale=1.0, angle=0.0,
                file=self._unique_text_png_name(img_idx),
                text=text,
                style=st_for_render,
            )


            item = TextOverlayItem(
                meta,
                qimg,
                None,
                on_changed=self._on_item_changed,
                on_drag_state_changed=self._on_overlay_drag_state,
            )
            self._setup_overlay_masks_callback(item)
            item._on_changed_view = self._on_item_changed 
            # При масштабе 1.0 один пиксель оверлея = один пиксель на холсте
            # НЕ масштабируем изображение, используем его как есть
            item._apply_pixmap(target_width_px=None)

            # UV хранятся для ЦЕНТРА элемента, устанавливаем позицию так, чтобы центр совпадал
            pm = item.pixmap()
            # user_scale = 1.0 для нового элемента, поэтому scaled_w = pm.width()
            left_x = center_x - pm.width() / 2.0
            top_y = center_y - pm.height() / 2.0
            item.setPos(left_x, top_y)
            item.setScale(1.0)
            self.scene.addItem(item)
            self._overlays.append(item)

            qimg.save(os.path.join(self.project.text_images, meta.file))
            self._save_text_info_json()

        finally:
            if proxy and proxy.scene() is not None and not removed_from_scene:
                try:
                    self.scene.removeItem(proxy)
                except Exception:
                    pass
            self._active_editor = None
            self._finalizing = False

    def _image_index_by_point(self, pt_scene) -> int | None:
        for i, r in enumerate(self.image_bboxes):
            if r.contains(pt_scene):
                return i
        return None

    def _trim_transparent(self, qimg: QImage, alpha_threshold: int = 1, margin: int = 0) -> QImage:
        """
        Обрезать прозрачные поля по альфа-каналу.
        alpha_threshold: пиксели с alpha >= threshold считаются «непрозрачными».
        margin: сохранить отступ по периметру (в пикселях).
        """
        if qimg.isNull():
            return qimg

        w, h = qimg.width(), qimg.height()
        if w <= 0 or h <= 0:
            return qimg

        # Ищем bbox непустых пикселей
        xmin, ymin, xmax, ymax = w, h, -1, -1

        # В формате ARGB32_Premultiplied alpha корректно читается через pixelColor().alpha()
        for y in range(h):
            # Быстрые «хвостовые» пропуски строк без непрозрачных пикселей
            row_has_ink = False
            for x in range(w):
                if qimg.pixelColor(x, y).alpha() >= alpha_threshold:
                    row_has_ink = True
                    xmin = min(xmin, x)
                    xmax = max(xmax, x)
            if row_has_ink:
                ymin = min(ymin, y)
                ymax = max(ymax, y)

        # Если «чернил» не нашли — вернём минимальную картинку 1×1 с полной прозрачностью
        if xmax < xmin or ymax < ymin:
            res = QImage(1, 1, QImage.Format.Format_ARGB32_Premultiplied)
            res.fill(0)
            return res

        # Применяем отступы и аккуратно ограничиваем рамку
        left   = max(0, xmin - margin)
        top    = max(0, ymin - margin)
        right  = min(w - 1, xmax + margin)
        bottom = min(h - 1, ymax + margin)

        return qimg.copy(left, top, right - left + 1, bottom - top + 1)

    def _unique_text_png_name(self, img_idx: int) -> str:
        base = f"page{img_idx:03d}_text"
        n = 1
        while True:
            fn = f"{base}_{n:03d}.png"
            if not os.path.exists(os.path.join(self.project.text_images, fn)):
                return fn
            n += 1

    def _image_index_by_rect(self, rect: QRectF) -> Optional[int]:
        # ищем страницу, внутри bbox которой лежит левый верхний угол области
        pt = rect.topLeft()
        for i, r in enumerate(self.image_bboxes):
            if r.contains(pt):
                return i
        return None

    def wheelEvent(self, e):
        # Ctrl+колесо: вращаем выделенные текстовые оверлеи.
        # Если выделения нет — отдаём событие CanvasView для зума холста.
        if e.modifiers() & Qt.KeyboardModifier.ControlModifier:
            selected_overlays = self._selected_text_overlays()
            if selected_overlays:
                dy = e.angleDelta().y()
                delta = 2.0 if dy > 0 else -2.0
                for it in selected_overlays:
                    it.meta.angle = (it.meta.angle + delta) % 360.0
                    it.setRotation(it.meta.angle)
                    try:
                        self._on_item_changed("angle", it)
                    except Exception:
                        pass
                e.accept()
                return
            super().wheelEvent(e)
            return

        # Shift+колесо:
        # - если есть выделенный оверлей: пересоздаём его картинку с новым font_size (+/-10%)
        #   и той же пропорцией изменения ширины;
        # - иначе меняем размер шрифта на панели (текущее поведение).
        if e.modifiers() & Qt.KeyboardModifier.ShiftModifier:
            dy = e.angleDelta().y()
            selected_overlays = self._selected_text_overlays()
            if selected_overlays:
                factor = 1.1 if dy > 0 else (1 / 1.1)
                for it in selected_overlays:
                    try:
                        st = it.meta.style if hasattr(it.meta, "style") else None
                        old_size = max(1, int(getattr(st, "font_size", 1)))
                        raw_new = old_size * factor
                        if dy > 0:
                            new_size = max(1, int(math.ceil(raw_new)))
                        else:
                            new_size = max(1, int(math.floor(raw_new)))
                        if new_size == old_size:
                            continue
                        resize_ratio = float(new_size) / float(old_size)
                        it.meta.style = it.meta.style.with_updates(font_size=int(new_size))
                        it.meta.w_frac = max(0.001, float(it.meta.w_frac) * resize_ratio)
                        self.recreate_overlay_item(it)
                    except Exception:
                        traceback.print_exc()
                e.accept()
                return

            step = 1 if dy > 0 else -1
            new_size = max(8, min(200, int(self.current_style.font_size) + step))
            if new_size != self.current_style.font_size:
                self._patch_style({"font_size": new_size})
                # синхронизируем спинбокс панели, если есть
                if self._panel and getattr(self._panel, "size", None):
                    self._panel.size.blockSignals(True)
                    self._panel.size.setValue(new_size)
                    self._panel.size.blockSignals(False)
                # обновляем превью текста на панели
                if hasattr(self.parent(), "top_panel") and hasattr(self.parent().top_panel, "update_preview"):
                    self.parent().top_panel.update_preview()
            e.accept()
            return
        super().wheelEvent(e)

    # Клавиатура: масштаб оверлеев и удаление
    def keyPressEvent(self, ev):
        # Горячие клавиши для выделенных оверлеев
        selected_overlays = self._selected_text_overlays()
        if selected_overlays:
            if ev.key() in (Qt.Key.Key_Return, Qt.Key.Key_Enter):
                parent = self.parent()
                if parent and hasattr(parent, "apply_overlay_changes"):
                    try:
                        parent.apply_overlay_changes()
                    except Exception:
                        traceback.print_exc()
                ev.accept()
                return

            if ev.key() in (Qt.Key.Key_Left, Qt.Key.Key_Right, Qt.Key.Key_Up, Qt.Key.Key_Down):
                step = 5 if (ev.modifiers() & Qt.KeyboardModifier.ShiftModifier) else 1
                dx = dy = 0
                if ev.key() == Qt.Key.Key_Left:
                    dx = -step
                elif ev.key() == Qt.Key.Key_Right:
                    dx = step
                elif ev.key() == Qt.Key.Key_Up:
                    dy = -step
                elif ev.key() == Qt.Key.Key_Down:
                    dy = step
                if dx or dy:
                    self._nudge_selected_overlays(dx, dy)
                    ev.accept()
                    return

        # Без Ctrl: -/=/+ управляют зумом текстовых оверлеев
        if not (ev.modifiers() & Qt.KeyboardModifier.ControlModifier):
            if ev.key() == Qt.Key.Key_Minus:
                self._scale_selected_overlays(1/1.1)
                return
            if ev.key() in (Qt.Key.Key_Equal, Qt.Key.Key_Plus):
                self._scale_selected_overlays(1.1)
                return
            if ev.key() == Qt.Key.Key_Delete:
                self._delete_selected_overlays()
                return

        # Ctrl+/-/=/+/0 обрабатываются QAction шорткатами из CanvasView
        super().keyPressEvent(ev)

    def _selected_text_overlays(self) -> list[TextOverlayItem]:
        """Возвращает список выделенных текстовых оверлеев."""
        return [it for it in self.scene.selectedItems() if isinstance(it, TextOverlayItem)]

    def _nudge_selected_overlays(self, dx: float, dy: float):
        """Сдвигает выделенные оверлеи и сохраняет изменения."""
        moved = False
        for it in self._selected_text_overlays():
            pos = it.pos()
            it.setPos(pos.x() + dx, pos.y() + dy)
            moved = True
            cb = getattr(it, "_on_changed", None)
            if callable(cb):
                cb("pos", it)
            else:
                self._on_item_changed("pos", it)
        if moved:
            self.viewport().update()

    def _scale_selected_overlays(self, factor: float):
        changed = False
        for it in list(self.scene.selectedItems()):
            if isinstance(it, TextOverlayItem):
                new_scale = max(0.1, min(5.0, it.meta.user_scale * factor))
                it.meta.user_scale = new_scale
                it.setScale(float(new_scale))
                changed = True
                # Уведомляем об изменении масштаба
                if hasattr(it, '_on_changed') and callable(it._on_changed):
                    it._on_changed("scale", it)
        if changed:
            # обновим метаданные на диске
            self._save_text_info_json()

    def _delete_selected_overlays(self):
        to_remove = []
        for it in list(self.scene.selectedItems()):
            if isinstance(it, TextOverlayItem):
                to_remove.append(it)
        if not to_remove:
            return
        if any(getattr(it, "isTransformMode", lambda: False)() for it in to_remove):
            self._set_transform_mode(None)
        for it in to_remove:
            # убрать со сцены и из списка
            try:
                self.scene.removeItem(it)
            except Exception:
                pass
            if it in self._overlays:
                self._overlays.remove(it)
            # удалить PNG файла оверлея, если есть
            if getattr(it.meta, "file", None):
                fn = os.path.join(self.project.text_images, it.meta.file)
                if os.path.exists(fn):
                    try:
                        os.remove(fn)
                    except Exception:
                        pass
        self._update_transform_button_state()
        self._save_text_info_json()

    def _get_overlay_at_scene_pos(self, pos_scene: QPointF) -> Optional[TextOverlayItem]:
        """
        Находит TextOverlayItem под указанной позицией в координатах сцены.
        Возвращает первый найденный оверлей или None.
        """
        # Проверяем оверлеи в обратном порядке (верхние первыми)
        for overlay in reversed(self._overlays):
            # Преобразуем позицию сцены в локальные координаты оверлея
            local_pos = overlay.mapFromScene(pos_scene)

            # Проверяем попадание в boundingRect оверлея
            if overlay.boundingRect().contains(local_pos):
                return overlay

        return None

    def _setup_overlay_masks_callback(self, overlay: TextOverlayItem):
        """
        Устанавливает callback для получения масок линий обрезки для оверлея.
        """
        def get_masks(item):
            # Если оверлей не подвержен обрезке — возвращаем пустой список
            if not bool(getattr(item.meta, "cut_enabled", True)):
                return []

            img_idx = item.meta.img_idx
            page_bbox = self.image_bboxes[img_idx] if 0 <= img_idx < len(self.image_bboxes) else None

            if page_bbox:
                return self._cutLinesManager.get_visible_masks(item, page_bbox)
            return []

        overlay._get_masks_callback = get_masks


    # ---------------- загрузка/сохранение ----------------
    def _apply_overlay_geometry_from_meta(self, it: TextOverlayItem):
        """Устанавливает позицию/трансформацию оверлея из meta (с учётом transform_uv)."""
        meta = it.meta
        old_cb = getattr(it, "_on_changed", None)
        it._on_changed = None
        if meta.img_idx < 0 or meta.img_idx >= len(self.image_bboxes):
            it._on_changed = old_cb
            return
        try:
            it.setTransform(QTransform())
            it.setScale(float(meta.user_scale))
            it.setRotation(float(meta.angle))
            quad_uv = getattr(meta, "transform_uv", None)
            if quad_uv and len(quad_uv) == 4:
                quad_scene = [QPointF(*self._scene_from_uv(meta.img_idx, float(u), float(v))) for u, v in quad_uv]
                pos = quad_scene[0]
                quad_parent = [p - pos for p in quad_scene]
                it.setPos(pos)
                it.apply_parent_quad(quad_parent)
                meta.transform_uv = [(float(u), float(v)) for u, v in quad_uv]
                center = QPointF(
                    sum(p.x() for p in quad_scene) / 4.0,
                    sum(p.y() for p in quad_scene) / 4.0,
                )
                cu, cv = self._uv_from_scene(meta.img_idx, center.x(), center.y())
                meta.u = float(cu)
                meta.v = float(cv)
            else:
                center_x, center_y = self._scene_from_uv(meta.img_idx, meta.u, meta.v)
                pm = it.pixmap()
                scaled_w = pm.width() * meta.user_scale
                scaled_h = pm.height() * meta.user_scale
                left_x = center_x - scaled_w / 2.0
                top_y = center_y - scaled_h / 2.0
                it.setPos(left_x, top_y)
        finally:
            it._on_changed = old_cb

    def _save_text_info_json(self):
        _d(f"[_save_text_info_json] START - saving {len(self._overlays)} overlays")
        data = []
        def _add_if_not_none(d: dict, key: str, value, *, as_list_rgba: bool = False):
            if value is not None:
                d[key] = (list(value) if as_list_rgba else value)
        for it in self._overlays:
            m = it.meta
            st = m.style if hasattr(m, "style") else TextStyle()
            e = {
                # обязательные/числовые (в т.ч. 0 сохраняем)
                "img_idx": m.img_idx,
                "u": float(m.u), "v": float(m.v),
                "w_frac": float(m.w_frac),
                "user_scale": float(m.user_scale),
                "angle": float(m.angle),
                "file": m.file,
                "text": m.text,
                # сохраняем флаг «подвержен обрезке»
                "cut_enabled": bool(getattr(m, "cut_enabled", True)),
                # перспектива (опционально)
                **({"transform_uv": [[float(u), float(v)] for u, v in getattr(m, "transform_uv", [])]} if getattr(m, "transform_uv", None) else {}),
                # Новый формат
                "style": st.to_json(),
                # legacy совместимость
                "font": st.font_family,
                "size": int(st.font_size),
                "color": list(st.font_color_rgba),
                "align": getattr(st, "align", "center"),
                "line_spacing": int(getattr(st, "line_spacing", 4)),
                "line_spacing_percent": int(getattr(st, "line_spacing_percent", 50)),
                "extra_vpadding": int(getattr(st, "extra_vpadding", 2)),
            }

            # только если НЕ None:
            _add_if_not_none(e, "reflect", getattr(st, "reflect", None))

            # эффекты
            e["stroke_width"] = int(getattr(st, "stroke_width", 0))
            _add_if_not_none(e, "stroke_color_rgba", getattr(st, "stroke_color_rgba", None), as_list_rgba=True)

            e["glow_radius"] = int(getattr(st, "glow_radius", 0))
            e["glow_softness"] = int(getattr(st, "glow_softness", 5))
            _add_if_not_none(e, "glow_color_rgba", getattr(st, "glow_color_rgba", None), as_list_rgba=True)

            e["shadow_dx"] = int(getattr(st, "shadow_dx", 0))
            e["shadow_dy"] = int(getattr(st, "shadow_dy", 0))
            _add_if_not_none(e, "shadow_color_rgba", getattr(st, "shadow_color_rgba", None), as_list_rgba=True)

            # градиенты (двухцветный/четырёхугольный)
            _add_if_not_none(e, "grad2_c1_rgba", getattr(st, "grad2_c1_rgba", None), as_list_rgba=True)
            _add_if_not_none(e, "grad2_c2_rgba", getattr(st, "grad2_c2_rgba", None), as_list_rgba=True)
            # угол градиента числовой — сохраняем всегда (дефолт 90.0)
            e["grad_angle_deg"] = float(getattr(st, "grad_angle_deg", 90.0))
            _add_if_not_none(e, "grad4_tl_rgba", getattr(st, "grad4_tl_rgba", None), as_list_rgba=True)
            _add_if_not_none(e, "grad4_tr_rgba", getattr(st, "grad4_tr_rgba", None), as_list_rgba=True)
            _add_if_not_none(e, "grad4_bl_rgba", getattr(st, "grad4_bl_rgba", None), as_list_rgba=True)
            _add_if_not_none(e, "grad4_br_rgba", getattr(st, "grad4_br_rgba", None), as_list_rgba=True)

            # форма текста
            e["text_shape"] = getattr(st, "text_shape", "rectangle")
            e["shake_enabled"] = bool(getattr(st, "shake_enabled", False))
            e["shake_angle_deg"] = float(getattr(st, "shake_angle_deg", 90.0))
            e["shake_up"] = int(getattr(st, "shake_up", 0))
            e["shake_down"] = int(getattr(st, "shake_down", 0))
            e["shake_steps"] = int(getattr(st, "shake_steps", 0))
            e["shake_base_fade"] = float(getattr(st, "shake_base_fade", 0.30))
            e["shake_decay"] = float(getattr(st, "shake_decay", 0.15))
            e["shake_blur"] = int(getattr(st, "shake_blur", 0))

            _d(f"  - overlay id={id(it)}: img_idx={m.img_idx} uv=({m.u:.6f},{m.v:.6f}) scale={m.user_scale:.6f} angle={m.angle:.2f}")
            data.append(e)
        json_path = os.path.join(self.project.text_images, "text_info.json")
        _d(f"[_save_text_info_json] Writing to: {json_path}")
        with open(json_path, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, indent=2)
        _d(f"[_save_text_info_json] COMPLETE - file written successfully")

    def _load_overlays_from_json(self):
        path = os.path.join(self.project.text_images, "text_info.json")
        if not os.path.exists(path):
            return

        # === ГАРД ГОТОВНОСТИ ЛЕНТЫ ===
        # Ждём, пока _display_images заполнит валидные bbox'ы (ширина/высота > 1).
        if not self.image_bboxes or any(r.width() <= 1 or r.height() <= 1 for r in self.image_bboxes):
            QTimer.singleShot(0, self._load_overlays_from_json)
            return
        # загрузим маски и подгоним размеры под текущие страницы
        try:
            self._cutLinesManager.load_page_masks(self.project.text_images)
            self._cutLinesManager.ensure_all_masks_sizes(self.image_bboxes)
        except Exception:
            pass
        try:
            with open(path, "r", encoding="utf-8") as f:
                arr = json.load(f)
        except Exception:
            return

        for e in arr:
            fn = os.path.join(self.project.text_images, e.get("file",""))
            qimg = QImage(fn)
            if qimg.isNull():
                continue

            style = TextStyle.from_dict(e.get("style") or e)
            raw_quad = e.get("transform_uv")
            quad_uv = None
            if raw_quad and isinstance(raw_quad, list) and len(raw_quad) == 4:
                try:
                    quad_uv = [(float(p[0]), float(p[1])) for p in raw_quad]
                except Exception:
                    quad_uv = None
            meta = TextOverlayMeta(
                img_idx=int(e.get("img_idx", 0)),
                u=float(e.get("u", 0.0)), v=float(e.get("v", 0.0)),
                w_frac=float(e.get("w_frac", 0.25)),
                user_scale=float(e.get("user_scale", 1.0)),
                angle=float(e.get("angle", 0.0)),
                file=e.get("file", ""),
                text=e.get("text", ""),
                style=style,
                cut_enabled=bool(e.get("cut_enabled", True)),
                transform_uv=quad_uv,
            )

            # ВАЖНО: Передаем None вместо callback, чтобы избежать перезаписи UV при загрузке
            it = TextOverlayItem(
                meta,
                qimg,
                None,
                on_changed=None,
                on_drag_state_changed=self._on_overlay_drag_state,
            )
            self._setup_overlay_masks_callback(it)

            # При масштабе 1.0 один пиксель оверлея = один пиксель на холсте
            # НЕ масштабируем изображение, используем сохраненный размер как есть
            it._apply_pixmap(target_width_px=None)
            pm = it.pixmap()
            page_w = max(1, int(self.image_bboxes[meta.img_idx].width()))
            _d(
                f"[LOAD-pre] id={id(it)} page={meta.img_idx} "
                f"page_w={page_w} w_frac={meta.w_frac:.6f} "
                f"pixmap={pm.width()}x{pm.height()}  "
                f"saved_uv=({meta.u:.6f},{meta.v:.6f})"
            )
            self._apply_overlay_geometry_from_meta(it)

            # Теперь устанавливаем callback для отслеживания будущих изменений
            it._on_changed = self._on_item_changed
            it._on_changed_view = self._on_item_changed  
            pos = it.pos()
            scene00 = it.mapToScene(QPointF(0, 0))
            _d(
                f"[LOAD-post] id={id(it)} scale={it.scale():.6f} angle={it.rotation():.2f} "
                f"pos=({pos.x():.3f},{pos.y():.3f}) mapToScene(0,0)=({scene00.x():.3f},{scene00.y():.3f})"
            )
            self.scene.addItem(it)
            self._overlays.append(it)

        # Один аккуратный рефлоу, чтобы наверняка синхронизировать на уже готовой разметке
        QTimer.singleShot(0, self._reflow_after_resize)

    def _reflow_after_resize(self):
        super()._reflow_after_resize()

        # Обновляем только позиции оверлеев (размеры не меняем)
        # При масштабе 1.0 один пиксель оверлея = один пиксель на холсте
        for it in self._overlays:
            self._apply_overlay_geometry_from_meta(it)

    ## Альтернативная реализация, устарела
    def save_all_pages(self, out_dir: str | None = None, oversample: int = 1):
        if DEBUG: print("Используется рендер save_all_pages")
        """
        Ручной экспорт: чёткая пиксельная отрисовка с опциональным сглаживанием
        через суперсэмплинг (2×/3×). Поворот — Smooth, масштабирование «вниз» — Smooth.
        """
        oversample = 1 if oversample not in (2, 3) else oversample
        out_dir = out_dir or getattr(self.project, "saved_dir", os.getcwd())
        os.makedirs(out_dir, exist_ok=True)

        originals: Dict[int, QImage] = {}
        for idx, path in enumerate(self.images):
            q = QImage(path)
            if not q.isNull():
                originals[idx] = q.convertToFormat(QImage.Format.Format_ARGB32_Premultiplied)

        for idx, qimg in originals.items():
            painter = QPainter(qimg)
            painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
            painter.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)
            painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)

            page_bbox: QRectF = self.image_bboxes[idx]
            # Накладываем клин, если включён
            if getattr(self, "overlays_model", None) and self.overlays_model.is_visible():
                ov = self.overlays_model.get(idx)
                if ov is not None and not ov.isNull():
                    lay_img = ov
                    if lay_img.size() != qimg.size():
                        lay_img = lay_img.scaled(qimg.size(), Qt.AspectRatioMode.IgnoreAspectRatio,
                                                 Qt.TransformationMode.SmoothTransformation)
                    painter.drawImage(0, 0, lay_img)
            sx = qimg.width() / max(1.0, page_bbox.width())
            sy = qimg.height() / max(1.0, page_bbox.height())

            for it in self._overlays:
                if it.meta.img_idx != idx:
                    continue
                base: QImage = it._base
                if base.isNull():
                    continue

                # ---- 1) Геометрия оверлея в дисплейных координатах (как в сцене)

                base: QImage = it._base
                if base.isNull():
                    continue

                # --- 1) Дисплейные величины (ровно как в превью) ---
                page_bbox: QRectF = self.image_bboxes[idx]
                page_w_disp = float(page_bbox.width())

                # В превью мы делали: target_w = int(meta.w_frac * page_w)
                target_w_disp = max(1, round(it.meta.w_frac * page_w_disp))

                # Ресэмплинг превью задаёт точный (целочисленный) размер картинки до user_scale:
                ratio_disp = target_w_disp / max(1.0, float(base.width()))
                scaled_w_disp = target_w_disp                                   # = int(...)
                scaled_h_disp = max(1, int(float(base.height()) * ratio_disp))  # важно: тоже int!

                # Позиция item — левый-верх «немасштабированного» прямоугольника
                x_disp = (it.pos().x() - page_bbox.left())
                y_disp = (it.pos().y() - page_bbox.top())

                # Центр, который в сцене держится инвариантным при user_scale/повороте:
                cx_disp = x_disp + scaled_w_disp * 0.5
                cy_disp = y_disp + scaled_h_disp * 0.5

                # После пользовательского масштаба (как в сцене: setScale(user_scale))
                user_scale = float(it.meta.user_scale)
                w_disp = scaled_w_disp * user_scale
                h_disp = scaled_h_disp * user_scale

                # --- 2) Перевод в пиксели исходной страницы ---
                sx = float(qimg.width())  / max(1.0, page_bbox.width())
                sy = float(qimg.height()) / max(1.0, page_bbox.height())

                cx_px = cx_disp * sx
                cy_px = cy_disp * sy
                w_px  = w_disp  * sx
                h_px  = h_disp  * sy
                angle = float(getattr(it.meta, "angle", 0.0))

                # --- 3) Рисуем базовый image «как есть», весь размер задаёт только трансформ ---
                base_w = max(1.0, float(base.width()))
                base_h = max(1.0, float(base.height()))

                sx_img = w_px / base_w
                sy_img = h_px / base_h

                painter.save()
                t = QTransform()
                t.translate(cx_px, cy_px)
                if abs(angle) > 0.01:
                    t.rotate(angle)
                t.scale(sx_img, sy_img)
                t.translate(-base_w * 0.5, -base_h * 0.5)
                painter.setWorldTransform(t, combine=False)
                painter.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)
                painter.drawImage(QPointF(0, 0), base)  # никакого target_rect и DPR
                painter.restore()

            painter.end()

            base_name = os.path.splitext(os.path.basename(self.images[idx]))[0] + ".png"
            qimg.save(os.path.join(out_dir, base_name))

        self._save_text_info_json()

    # ==================== МЕТОДЫ ДЛЯ РЕДАКТИРОВАНИЯ ОВЕРЛЕЕВ ====================

    def setup_overlay_selection_handler(self, on_overlay_selected_callback):
        """
        Устанавливает колбэк, который вызывается при выделении текстового оверлея.
        on_overlay_selected_callback: функция, принимающая TextOverlayItem
        """
        self._on_overlay_selected = on_overlay_selected_callback
        # Подключаемся к сигналу изменения выделения сцены
        self.scene.selectionChanged.connect(self._handle_selection_change)
        # Отслеживаем смену фокуса элементов, чтобы понимать «активный» оверлей при мультивыделении
        try:
            self.scene.focusItemChanged.connect(self._handle_focus_change)
        except Exception:
            pass

    def _preferred_selected_overlay(self) -> Optional[TextOverlayItem]:
        """Возвращает приоритетный выбранный оверлей (с фокусом или верхний по Z)."""
        selected = [it for it in self.scene.selectedItems() if isinstance(it, TextOverlayItem)]
        if not selected:
            return None
        focused = self.scene.focusItem()
        if isinstance(focused, TextOverlayItem) and focused.isSelected():
            return focused
        selected.sort(key=lambda it: it.zValue())
        return selected[-1]

    def _handle_selection_change(self):
        """Обработчик изменения выделения в сцене"""
        selected = self.scene.selectedItems()

        # Если ничего не выделено - возвращаемся к панели создания
        if not selected:
            if hasattr(self, '_on_overlay_deselected') and callable(self._on_overlay_deselected):
                self._on_overlay_deselected()
            self._set_transform_mode(None)
            self._update_transform_button_state()
            return

        target = self._preferred_selected_overlay()
        if target is None:
            if hasattr(self, '_on_overlay_deselected') and callable(self._on_overlay_deselected):
                self._on_overlay_deselected()
            self._set_transform_mode(None)
            self._update_transform_button_state()
            return
        if target and hasattr(self, '_on_overlay_selected') and callable(self._on_overlay_selected):
            self._on_overlay_selected(target)
        elif hasattr(self, '_on_overlay_deselected') and callable(self._on_overlay_deselected):
            self._on_overlay_deselected()

        active = self._active_transform_overlay()
        if active and active not in selected:
            active.setTransformMode(False)

        self._update_transform_button_state()

    def _handle_focus_change(self, new_item, old_item, reason=None):
        """
        При смене фокуса внутри сцены обновляем панель под активный оверлей,
        даже если выделение не изменилось (например, клик по уже выделенному элементу).
        """
        if isinstance(new_item, TextOverlayItem) and new_item.isSelected():
            if hasattr(self, '_on_overlay_selected') and callable(self._on_overlay_selected):
                self._on_overlay_selected(new_item)
        self._update_transform_button_state()

    def _active_transform_overlay(self) -> Optional[TextOverlayItem]:
        for it in self._overlays:
            if isinstance(it, TextOverlayItem) and it.isTransformMode():
                return it
        return None

    def _set_transform_mode(self, target: Optional[TextOverlayItem]):
        for it in self._overlays:
            try:
                it.setTransformMode(it is target)
            except Exception:
                pass

    def _update_transform_button_state(self):
        if not hasattr(self, "_transformButton"):
            return
        target = self._preferred_selected_overlay()
        active = bool(target and target.isTransformMode())
        if active:
            self._transformButton.hide()
            self._transformExitButton.show()
            self._transformResetButton.show()
            self._transformExitButton.setEnabled(True)
            self._transformResetButton.setEnabled(True)
        else:
            self._transformExitButton.hide()
            self._transformResetButton.hide()
            self._transformButton.show()
            self._transformButton.setEnabled(bool(target))
        self._layout_top_labels()

    def _enter_transform_mode_click(self):
        ov = self._preferred_selected_overlay()
        if ov is None:
            return
        self._set_transform_mode(ov)
        self._update_transform_button_state()

    def _exit_transform_mode_click(self):
        self._set_transform_mode(None)
        self._update_transform_button_state()

    def _reset_transform_click(self):
        ov = self._active_transform_overlay() or self._preferred_selected_overlay()
        if ov is None:
            return
        ov.setTransform(QTransform())
        ov.meta.transform_uv = None
        self._apply_overlay_geometry_from_meta(ov)
        if ov.isTransformMode():
            ov.setTransformMode(True)
        if callable(getattr(ov, "_on_changed", None)):
            ov._on_changed("transform", ov)
        self._update_transform_button_state()

    def recreate_overlay_item(self, overlay_item: TextOverlayItem):
        """
        Пересоздает текстовый оверлей с обновленными параметрами.
        Используется при применении изменений из панели редактирования.
        """
        if not isinstance(overlay_item, TextOverlayItem):
            return

        meta = overlay_item.meta

        # Рендерим новый текст с обновленными параметрами
        from .text_render import Renderer
        renderer = self._renderer if hasattr(self, '_renderer') else Renderer()

        # Получаем bbox страницы для расчета ширины
        if meta.img_idx < len(self.image_bboxes):
            page_bbox = self.image_bboxes[meta.img_idx]
            page_w = int(page_bbox.width())
        else:
            page_w = 1000  # fallback

        # Ширина для рендера (полная, без масштаба)
        render_width = max(1, int(meta.w_frac * page_w))

        # Рендерим текст с эффектами
        shake = None
        if getattr(meta, "shake_enabled", False):
            shake = {
                "angle_deg": getattr(meta, "shake_angle_deg", 90.0),
                "up": getattr(meta, "shake_up", 0),
                "down": getattr(meta, "shake_down", 0),
                "steps": getattr(meta, "shake_steps", 0),
                "base_fade": getattr(meta, "shake_base_fade", 0.30),
                "decay": getattr(meta, "shake_decay", 0.15),
                "blur": getattr(meta, "shake_blur", 0),
            }
        st = meta.style if hasattr(meta, "style") else TextStyle.from_dict({})
        qimg = renderer.big_renderer(**st.ensure_exclusive_gradients().to_renderer_kwargs(text=meta.text, width_px=render_width))

        # Обновляем базовое изображение оверлея
        overlay_item._base = qimg

        # При масштабе 1.0 один пиксель оверлея = один пиксель на холсте
        # НЕ масштабируем изображение, используем его как есть
        overlay_item._apply_pixmap(target_width_px=None)

        # Обновляем w_frac на основе реальной ширины отрендеренного изображения
        # Это гарантирует, что при сохранении и перезагрузке оверлей будет той же ширины
        actual_width = qimg.width()
        meta.w_frac = actual_width / max(1, page_w)

        # Обновляем позицию из UV координат
        center_x, center_y = self._scene_from_uv(meta.img_idx, meta.u, meta.v)
        pm = overlay_item.pixmap()
        scaled_w = pm.width() * meta.user_scale
        scaled_h = pm.height() * meta.user_scale
        left_x = center_x - scaled_w / 2.0
        top_y = center_y - scaled_h / 2.0

        overlay_item.setPos(left_x, top_y)
        overlay_item.setScale(meta.user_scale)
        overlay_item.setRotation(meta.angle)

        # Сохраняем изображение в проект
        import os
        qimg.save(os.path.join(self.project.text_images, meta.file))

        # Сохраняем метаданные
        self._save_text_info_json()

    def delete_overlay_item(self, overlay_item: TextOverlayItem):
        """
        Удаляет текстовый оверлей из сцены и из списка.
        """
        if not isinstance(overlay_item, TextOverlayItem):
            return

        # Удаляем из сцены
        if overlay_item.scene():
            self.scene.removeItem(overlay_item)

        # Удаляем из списка оверлеев
        if overlay_item in self._overlays:
            self._overlays.remove(overlay_item)

        # Удаляем файл изображения
        import os
        img_path = os.path.join(self.project.text_images, overlay_item.meta.file)
        if os.path.exists(img_path):
            try:
                os.remove(img_path)
            except Exception as e:
                print(f"Не удалось удалить файл {img_path}: {e}")

        # Сохраняем обновленные метаданные
        self._save_text_info_json()

    # ==================== МЕТОДЫ ДЛЯ ЛИНИЙ ОБРЕЗКИ ====================

    def _get_fill_source_image(self, img_idx: int) -> Optional[QImage]:
        """Возвращает страницу для заливки с учётом клина (если он включён)."""
        if img_idx is None or img_idx >= len(self.images):
            return None

        # Базовая страница из кэша
        if self._cached_fill_img_idx != img_idx or self._cached_fill_image is None:
            self._cached_fill_image = QImage(self.images[img_idx])
            self._cached_fill_img_idx = img_idx

        base_img = self._cached_fill_image
        if base_img.isNull():
            return None

        # Накладываем клин, только если он есть и показан
        overlay_img = None
        if getattr(self, "overlays_model", None) and self.overlays_model.is_visible():
            overlay_img = self.overlays_model.get(img_idx)
            if overlay_img is None or overlay_img.isNull():
                overlay_img = None

        if overlay_img is None:
            return base_img

        # Делаем копию, чтобы не портить кэш
        if base_img.format() == QImage.Format.Format_ARGB32_Premultiplied:
            composed = base_img.copy()
        else:
            composed = base_img.convertToFormat(QImage.Format.Format_ARGB32_Premultiplied)

        ov = overlay_img
        if ov.size() != composed.size():
            ov = ov.scaled(
                composed.size(),
                Qt.AspectRatioMode.IgnoreAspectRatio,
                Qt.TransformationMode.SmoothTransformation
            )

        painter = QPainter(composed)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)
        painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)
        painter.drawImage(0, 0, ov)
        painter.end()

        return composed

    def _update_fill_color_preview(self):
        """Обновить превью цвета под курсором (вызывается по таймеру)"""
        if not self._cutLinesPanel.isVisible() or self._cutLinesPanel.get_current_tool() != "fill":
            return
        if self._last_fill_cursor_pos is None:
            return

        pos_scene = self._last_fill_cursor_pos
        img_idx = self._image_index_by_point(pos_scene)

        if img_idx is None or img_idx >= len(self.images):
            return

        try:
            source_img = self._get_fill_source_image(img_idx)
            if source_img is None or source_img.isNull():
                return

            page_bbox = self.image_bboxes[img_idx]

            # Преобразуем в координаты оригинального изображения
            local_x = pos_scene.x() - page_bbox.left()
            local_y = pos_scene.y() - page_bbox.top()
            scale_x = float(source_img.width()) / max(1.0, page_bbox.width())
            scale_y = float(source_img.height()) / max(1.0, page_bbox.height())
            orig_x = int(local_x * scale_x)
            orig_y = int(local_y * scale_y)

            # Проверка границ
            if 0 <= orig_x < source_img.width() and 0 <= orig_y < source_img.height():
                color = source_img.pixelColor(orig_x, orig_y)
                self._cutLinesPanel.update_color_preview(color)
        except Exception:
            pass

    def _on_tool_changed(self, tool: str):
        """Обработчик изменения инструмента маски-барьера"""
        if tool == "fill":
            # Включаем режим заливки
            self.setCursor(Qt.CursorShape.CrossCursor)
            # Запускаем таймер обновления цвета
            self._fill_color_update_timer.start()
        elif tool in ("brush", "eraser"):
            # Включаем режим рисования
            self.setCursor(Qt.CursorShape.CrossCursor)
            # Останавливаем таймер заливки
            self._fill_color_update_timer.stop()
            self._cutLinesPanel.update_color_preview(None)
        else:
            self.unsetCursor()
            self._fill_color_update_timer.stop()

        self.viewport().update()

    def _on_clear_barrier_mask(self):
        """Обработчик очистки маски-барьера текущей страницы"""
        # Определяем текущую страницу по центру viewport
        viewport_center = self.viewport().rect().center()
        scene_center = self.mapToScene(viewport_center)
        img_idx = self._image_index_by_point(scene_center)

        if img_idx is not None and 0 <= img_idx < len(self.image_bboxes):
            self._cutLinesManager.clear_mask(img_idx)

            # Обновляем все оверлеи на этой странице
            for overlay in self._overlays:
                if overlay.meta.img_idx == img_idx:
                    overlay.update()

        self.viewport().update()

    def paintEvent(self, event):
        """Переопределяем paintEvent для отрисовки масок-барьеров поверх сцены"""
        # Сначала отрисовываем стандартное содержимое (сцену)
        super().paintEvent(event)

        # Рисуем маски-барьеры только если панель видима
        if hasattr(self, '_cutLinesPanel') and self._cutLinesPanel.isVisible():
            if not hasattr(self, '_cutLinesManager'):
                return

            painter = QPainter(self.viewport())
            painter.setRenderHint(QPainter.RenderHint.Antialiasing)

            color = self._cutLinesManager.get_animated_color()

            # Отрисовываем текущий штрих (если идет рисование)
            if hasattr(self, '_drawing_barrier') and self._drawing_barrier and hasattr(self, '_current_stroke'):
                if len(self._current_stroke) >= 2:
                    pen = QPen(
                        color,
                        self._cutLinesPanel.brush_size_spinbox.value(),
                        Qt.PenStyle.SolidLine,
                        Qt.PenCapStyle.RoundCap,
                        Qt.PenJoinStyle.RoundJoin
                    )
                    painter.setPen(pen)

                    path = QPainterPath()
                    first_point = self.mapFromScene(QPointF(self._current_stroke[0]))
                    path.moveTo(QPointF(first_point))

                    for pt in self._current_stroke[1:]:
                        viewport_pt = self.mapFromScene(QPointF(pt))
                        path.lineTo(QPointF(viewport_pt))

                    painter.drawPath(path)

            # Отрисовываем маски-барьеры для видимых страниц
            if hasattr(self, 'image_bboxes'):
                for img_idx in range(len(self.image_bboxes)):
                    mask = self._cutLinesManager.page_masks.get(img_idx)
                    if mask and not mask.isNull():
                        page_bbox = self.image_bboxes[img_idx]

                        # Преобразуем bbox страницы из сцены в viewport
                        viewport_rect = self.mapFromScene(page_bbox).boundingRect()

                        # Масштабируем маску под размер страницы в viewport
                        scaled_mask = mask.scaled(
                            int(viewport_rect.width()),
                            int(viewport_rect.height()),
                            Qt.AspectRatioMode.IgnoreAspectRatio,
                            Qt.TransformationMode.SmoothTransformation
                        )

                        # Создаем цветную версию маски с анимированным цветом
                        colored_mask = QImage(scaled_mask.size(), QImage.Format.Format_ARGB32)
                        colored_mask.fill(Qt.GlobalColor.transparent)

                        mask_painter = QPainter(colored_mask)
                        # Копируем альфа-канал маски как базу
                        mask_painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_Source)
                        # Конвертируем Alpha8 в ARGB для визуализации
                        temp_mask = QImage(scaled_mask.size(), QImage.Format.Format_ARGB32)
                        temp_mask.fill(Qt.GlobalColor.transparent)
                        temp_painter = QPainter(temp_mask)
                        temp_painter.drawImage(0, 0, scaled_mask)
                        temp_painter.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceIn)
                        temp_painter.fillRect(temp_mask.rect(), color)
                        temp_painter.end()

                        mask_painter.drawImage(0, 0, temp_mask)
                        mask_painter.end()

                        # Рисуем на viewport с полупрозрачностью
                        painter.setOpacity(0.5)
                        painter.drawImage(int(viewport_rect.left()), int(viewport_rect.top()), colored_mask)
                        painter.setOpacity(1.0)

            painter.end()

    def eventFilter(self, obj, event):
        # Отлавливаем закрытие панели барьеров
        if obj is getattr(self, "_cutLinesPanel", None) and event.type() == QEvent.Type.Hide:
            # Запускаем асинхронное сохранение масок
            try:
                self._cutLinesManager.async_save_page_masks(
                    self.project.text_images,
                    on_finished=lambda ok, path: None  # по желанию: показать тост/лог
                )
            except Exception:
                pass
        return super().eventFilter(obj, event)
