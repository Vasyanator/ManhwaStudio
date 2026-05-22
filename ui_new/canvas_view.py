"""
File: canvas_view.py

Purpose:
Базовый QGraphicsView для ленты страниц, пузырей и overlay-слоёв.

Main responsibilities:
- раскладка страниц в сцене;
- zoom/scroll/navigation;
- позиционирование и редактирование пузырей;
- синхронизация со shared-моделями.

Key structures:
- CanvasView
- BubbleRuntime

Key functions:
- _reflow_canvas_layout()
- _display_images()
- _apply_bubble_imgpos()
- _repack_bubbles_for()

Notes:
- sceneRect расширяется по видимому контенту, чтобы горизонтальный scroll
  учитывал пузыри и large-zoom выход за границы вьюпорта.
"""
from __future__ import annotations
import os
import bisect
from dataclasses import dataclass
from typing import Dict, List, Optional, Tuple, Union
from PyQt6.QtGui import QBrush, QColor, QCursor
from PyQt6.QtCore import Qt, QRectF, QEvent, QObject, QTimer, pyqtSignal, QRect
from PyQt6.QtGui import (
    QAction, QImage, QKeySequence, QPainter, QPen, QPixmap, QWheelEvent,
    QGuiApplication, QFontMetrics, QTransform
)
from PyQt6.QtWidgets import (
    QWidget, QGraphicsView, QGraphicsScene, QGraphicsPixmapItem, QGraphicsLineItem,
    QGraphicsProxyWidget, QTextEdit, QLabel, QGraphicsItem, QGraphicsRectItem,
    QGraphicsEllipseItem, QCheckBox, QSlider, QComboBox, QLayout, QSpinBox
)
from PyQt6.QtWidgets import QWidget, QVBoxLayout, QHBoxLayout, QPushButton
from PyQt6.QtGui import QTextOption
from PyQt6.QtWidgets import QSizePolicy
ImageLike = Union[str, QImage, QPixmap]
import uuid
import traceback
import threading

MIN_CANVAS_SCALE = 0.2
MAX_CANVAS_SCALE = 5.0

class TextSlider(QSlider):
    def __init__(self, text: str, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._text = text

    def setText(self, text: str) -> None:
        self._text = text
        self.update()

    def paintEvent(self, event):
        super().paintEvent(event)
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.TextAntialiasing, True)
        p.setPen(QColor("white"))
        p.drawText(self.rect(), Qt.AlignmentFlag.AlignCenter, self._text)

@dataclass
class BubbleRuntime:
    id: int
    img_idx: int
    img_u: float
    img_v: float
    side: str
    anchor_x: float = 0.0
    anchor_y: float = 0.0
    line_item: Optional[QGraphicsLineItem] = None
    proxy_widget: Optional[QGraphicsProxyWidget] = None
    text_widget: Optional[QTextEdit] = None
    original_text_widget: Optional[QTextEdit] = None
    container_widget: Optional[QWidget] = None
    move_btn: Optional["QPushButton"] = None  # кнопка "Переместить"
    max_width: int = 200
    line_x: float = 0.0
    height_px: int = 0
    footer_widget: Optional[QWidget] = None
    rect_coords: Optional[Dict[str, Dict[str, float]]] = None
    rect_item: Optional[QGraphicsRectItem] = None
    rect_item_inner: Optional[QGraphicsRectItem] = None
    rect_handles: Optional[List[QGraphicsEllipseItem]] = None
    header_widget: Optional[QWidget] = None
    header_proxy: Optional[QGraphicsProxyWidget] = None
    original_container: Optional[QWidget] = None
    original_proxy: Optional[QGraphicsProxyWidget] = None
    footer_proxy: Optional[QGraphicsProxyWidget] = None
    measured_layout_key: Optional[Tuple[int, int, int]] = None


class CanvasView(QGraphicsView):
    """
    Вертикальная лента изображений + «текстовые пузыри»,
    хранение в Project.bubbles (load/autosave).
    Хоткеи:
      T — создать пузырь в точке курсора (если клик по картинке)
      Delete — удалить активный
      Ctrl + / Ctrl - — масштаб ленты
      Ctrl 0 — сброс масштаба
    """
    bubblesChanged = pyqtSignal(str, int)  # reason, bubble_id
    _image_cache_lock = threading.Lock()
    _image_cache: Dict[str, Tuple[int, int, QImage]] = {}
    _gpu_viewport_enabled_logged = False
    _gpu_viewport_unavailable_logged = False

    @classmethod
    def _cached_image_for_path(cls, path: str) -> QImage:
        path = os.path.abspath(path)
        try:
            st = os.stat(path)
            stamp_mtime_ns = int(st.st_mtime_ns)
            stamp_size = int(st.st_size)
        except OSError:
            return QImage(path)

        with cls._image_cache_lock:
            cached = cls._image_cache.get(path)
            if cached is not None:
                c_mtime_ns, c_size, c_img = cached
                if c_mtime_ns == stamp_mtime_ns and c_size == stamp_size:
                    return c_img

        img = QImage(path)
        if img.isNull():
            return img

        with cls._image_cache_lock:
            cls._image_cache[path] = (stamp_mtime_ns, stamp_size, img)
        return img

    @classmethod
    def prewarm_image_cache(cls, paths: List[str], max_items: int | None = None) -> None:
        if not paths:
            return
        items = paths if max_items is None else paths[:max(0, int(max_items))]
        for p in items:
            if isinstance(p, str):
                cls._cached_image_for_path(p)

    def _start_image_prewarm(self) -> None:
        """Греем QImage-кэш в фоне, чтобы не держать GUI на первом же reflow."""
        paths = [p for p in self.images if isinstance(p, str)]
        if not paths:
            return

        def _run() -> None:
            try:
                CanvasView.prewarm_image_cache(paths)
            except Exception:
                pass

        th = threading.Thread(target=_run, name="canvas-image-prewarm", daemon=True)
        th.start()

    def __init__(
        self,
        project,
        images,
        editable=True,
        parent=None,
        bubbles_model=None,
        overlays_model=None,
        user_config=None,
    ):
        super().__init__(parent)
        self.uid = str(uuid.uuid4())
        self.project = project
        self.user_config = user_config
        self._gpu_viewport_active = False
        self.model = bubbles_model  # общий для всех вкладок!
        self.overlays_model = overlays_model
        self._opengl_settings_ui_sync = False
        self._opengl_enabled = self._load_opengl_render_enabled()
        self._opengl_enabled_last_committed = self._opengl_enabled
        self._opengl_enabled_dirty = False
        self._opengl_device = self._load_opengl_device()
        self._opengl_device_last_committed = self._opengl_device
        self._opengl_device_dirty = False
        self._opengl_restart_required = False
        # совместимость со старыми инструментами, ожидающими _overlay_images
        self._overlay_images: List[Optional[QImage]] = []
        self._overlay_items: List[Optional[QGraphicsPixmapItem]] = []
        # подписки на модель (ВАЖНО: без лямбд, только bound-методы QObject)
        if self.model:
            self.model.bubbleCreated.connect(self._on_model_created)
            self.model.bubbleUpdated.connect(self._on_model_updated)
            self.model.bubbleDeleted.connect(self._on_model_deleted)
            self.model.bubbleUnplaced.connect(self._on_model_unplaced)
            self.model.bubbleTypeChanged.connect(self._on_model_bubble_type_changed)
            self.model.asideWidthLimitsChanged.connect(self._on_model_aside_width_limits_changed)
            self.model.pageSpacingChanged.connect(self._on_model_page_spacing_changed)
            self.model.separatePagesChanged.connect(self._on_model_separate_pages_changed)
            self.model.verticalEdgeMarginChanged.connect(self._on_model_vertical_edge_margin_changed)
            self.model.scaleBubblesChanged.connect(self._on_model_scale_bubbles_changed)
            self.model.visiblePageRadiusChanged.connect(self._on_model_visible_page_radius_changed)
            self.model.bubbleLoadDelayChanged.connect(self._on_model_bubble_load_delay_changed)
            self.model.tabsAutoSyncChanged.connect(self._on_model_tabs_autosync_changed)
            self.model.tabsSyncRequested.connect(self._on_model_tabs_sync_requested)
        self.setObjectName("CanvasView")
        self._configure_viewport_backend()
        self.setRenderHints(QPainter.RenderHint.Antialiasing | QPainter.RenderHint.SmoothPixmapTransform)
        self.setCacheMode(QGraphicsView.CacheModeFlag.CacheNone)
        self.setOptimizationFlag(QGraphicsView.OptimizationFlag.DontSavePainterState, True)
        self.setViewportUpdateMode(
            QGraphicsView.ViewportUpdateMode.FullViewportUpdate
            if self._gpu_viewport_active
            else QGraphicsView.ViewportUpdateMode.MinimalViewportUpdate
        )
        self.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOn)
        self.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAsNeeded)
        self.setAlignment(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignTop)
        self.setMouseTracking(True)
        self.viewport().setMouseTracking(True)

        # --- Project: bubbles load/save ---
        if not hasattr(self.project, "bubbles") or self.project.bubbles is None:
            if hasattr(self.project, "load"):
                try:
                    self.project.load()
                except Exception as e:
                    print(f"[CanvasView] Project.load() failed: {e}")
                    traceback.print_exc()
        if not hasattr(self.project, "bubbles") or self.project.bubbles is None:
            self.project.bubbles = []
        fixed = []
        for rec in self.project.bubbles:
            try:
                rec = dict(rec)
                rec['id'] = int(rec.get('id'))
            except Exception:
                traceback.print_exc()
                pass
            fixed.append(rec)
        self.project.bubbles = fixed
        # входящие картинки (пути/QImage/QPixmap)
        self.images: List[ImageLike] = images[:] if images else []
        self.editable = bool(editable)

        # сцена
        self.scene = QGraphicsScene(self)
        self.setScene(self.scene)
        self._base_scene_rect = QRectF(0.0, 0.0, 1.0, 1.0)
        self._scene_anchor_center_x = 0.5

        # геометрия ленты
        self.image_items: List[QGraphicsPixmapItem] = []
        self.image_bboxes: List[QRectF] = []

        self.side_margin = 20
        self.canvas_scale = 1.0

        self._resize_debounce = QTimer(self)
        self._resize_debounce.setInterval(60)
        self._resize_debounce.setSingleShot(True)
        self._resize_debounce.timeout.connect(self._reflow_after_resize)

        self._autosave_timer = QTimer(self)
        self._autosave_timer.setInterval(600)  # мс
        self._autosave_timer.setSingleShot(True)
        self._autosave_timer.timeout.connect(self._autosave_now)

        # батч-обновления оверлеев (чтобы не перерисовывать на каждый мазок)
        self._overlay_refresh_pending: set[int] = set()
        self._overlay_refresh_timer = QTimer(self)
        self._overlay_refresh_timer.setInterval(40)
        self._overlay_refresh_timer.setSingleShot(True)
        self._overlay_refresh_timer.timeout.connect(self._flush_overlay_refresh)

        # отложенный repack пузырей (на входящих апдейтах)
        self._repack_pending: set[tuple[int, str]] = set()
        self._repack_timer = QTimer(self)
        self._repack_timer.setInterval(40)
        self._repack_timer.setSingleShot(True)
        self._repack_timer.timeout.connect(self._flush_repack_pending)
        self._scroll_pending_center_idx: Optional[int] = None
        self._scroll_bubble_refresh_timer = QTimer(self)
        self._scroll_bubble_refresh_timer.setInterval(260)
        self._scroll_bubble_refresh_timer.setSingleShot(True)
        self._scroll_bubble_refresh_timer.timeout.connect(self._flush_scroll_bubble_refresh)
        self._scroll_quality_restore_timer = QTimer(self)
        self._scroll_quality_restore_timer.setInterval(140)
        self._scroll_quality_restore_timer.setSingleShot(True)
        self._scroll_quality_restore_timer.timeout.connect(self._restore_smooth_render_quality)
        self._scroll_fast_quality = False
        self._scene_rect_refresh_timer = QTimer(self)
        self._scene_rect_refresh_timer.setInterval(0)
        self._scene_rect_refresh_timer.setSingleShot(True)
        self._scene_rect_refresh_timer.timeout.connect(self._refresh_scene_rect_to_content)

        # пузыри
        self.bubbles_visible = True
        self.bubble_count = max([b.get("id", 0) for b in self.project.bubbles] + [0])
        self.bubbles: Dict[int, BubbleRuntime] = {}
        self.selected_bubble: Optional[int] = None
        self._move_active_bid: Optional[int] = None
        self._active_rect_handle: Optional[Tuple[int, int]] = None
        self._rect_handle_radius = 5

        # таймер для debounce обновления модели при вводе текста
        self._text_update_timer = QTimer(self)
        self._text_update_timer.setInterval(300)  # 300мс задержка
        self._text_update_timer.setSingleShot(True)
        self._pending_text_updates: Dict[int, Dict[str, str]] = {}  # bid -> {'text': ..., 'original_text': ...}
        # отложенные апдейты модели, пока пузырь в фокусе
        self._deferred_model_updates: Dict[int, dict] = {}
        self._post_show_reflow_done = False
        self._active_page_window: set[int] = set()
        self._bubble_cache: Dict[int, BubbleRuntime] = {}
        self._page_tops: List[float] = []
        self._page_bottoms: List[float] = []
        self._project_bubbles_by_page: Dict[int, Dict[int, dict]] = {}
        self._project_bubbles_by_id: Dict[int, dict] = {}
        self._project_bubbles_index_dirty = True
        self._pixmap_cache_by_path: Dict[str, Tuple[int, int, QPixmap]] = {}
        self._pixmap_cache_by_qimage_key: Dict[int, QPixmap] = {}
        self._pending_layout_updates: set[int] = set()
        self._text_layout_timer = QTimer(self)
        self._text_layout_timer.setInterval(33)
        self._text_layout_timer.setSingleShot(True)
        self._text_layout_timer.timeout.connect(self._flush_pending_layout_updates)

        # наклейки
        self._pageLabel = QLabel("0 / 0", self)
        self._pageLabel.setObjectName("PageCounter")
        self._pageLabel.setFixedHeight(26)
        self._pageLabel.setStyleSheet(
            "QLabel#PageCounter{background:#333;color:white;padding:3px 10px;font-weight:600;border-radius:4px;}"
        )
        self._pageLabel.move(8, 8)
        self._scaleLabel = QLabel("1.0×", self)
        self._scaleLabel.setObjectName("ScaleIndicator")
        self._scaleLabel.setFixedHeight(26)
        self._scaleLabel.setStyleSheet(
            "QLabel#ScaleIndicator{background:#333;color:white;padding:3px 10px;font-weight:600;border-radius:4px;}"
        )
        self._hotkeysLabel = QLabel("T — пузырь, Del — удалить, Ctrl± — зум", self)
        self._hotkeysLabel.setObjectName("HotkeysHint")
        self._hotkeysLabel.setStyleSheet("""
            QLabel#HotkeysHint {
                color: white;
                background-color: #3a3a3a;
                border: 1px solid #2a2a2a;
                padding: 4px 8px;
            }
        """)

        self._bubblesCheckbox = QCheckBox("Показывать пузыри", self)
        self._bubblesCheckbox.setChecked(self.bubbles_visible)
        self._bubblesCheckbox.setFixedWidth(154)
        self._bubblesCheckbox.setStyleSheet(
            "QCheckBox{color:white;background:#3a3a3a;padding:4px 8px;border-radius:4px;}"
        )
        self._bubblesCheckbox.toggled.connect(self._on_bubbles_checkbox)

        self._bubbleOpacitySlider = TextSlider("Прозрачность пузырей", Qt.Orientation.Horizontal, self)
        self._bubbleOpacitySlider.setRange(0, 100)
        self._bubbleOpacitySlider.setValue(100)
        self._bubbleOpacitySlider.setFixedHeight(26)
        self._bubbleOpacitySlider.setFixedWidth(154)
        self._bubbleOpacitySlider.setStyleSheet(
            "QSlider{background:#3a3a3a;border-radius:4px;}"
            "QSlider::groove:horizontal{height:26px;background:transparent;border-radius:4px;}"
            "QSlider::sub-page:horizontal{background:#2e7cf6;border-radius:4px;}"
            "QSlider::add-page:horizontal{background:#3a3a3a;border-radius:4px;}"
            "QSlider::handle:horizontal{width:10px;margin:0px;background:transparent;border:none;}"
        )
        self._bubbleOpacitySlider.valueChanged.connect(self._on_bubble_opacity_changed)
        self._bubble_opacity = 1.0
        self._aside_width_spin_sync = False
        self._aside_width_dirty = False
        self._aside_min_width_px, self._aside_max_width_px = self._load_aside_width_limits()
        self._aside_last_committed = (self._aside_min_width_px, self._aside_max_width_px)
        self._page_spacing_spin_sync = False
        self._page_spacing_dirty = False
        self._page_spacing_px = self._load_page_spacing()
        self._page_spacing_last_committed = self._page_spacing_px
        self._separate_pages_checkbox_sync = False
        self._separate_pages_enabled = self._load_separate_pages_enabled()
        self._separate_pages_last_committed = self._separate_pages_enabled
        self._separate_pages_dirty = False
        self._vertical_edge_margin_spin_sync = False
        self._vertical_edge_margin_dirty = False
        self._vertical_edge_margin_px = self._load_vertical_edge_margin()
        self._vertical_edge_margin_last_committed = self._vertical_edge_margin_px
        self._load_all_bubbles_checkbox_sync = False
        self._load_all_bubbles_enabled = self._load_all_bubbles_enabled_setting()
        self._load_all_bubbles_last_committed = self._load_all_bubbles_enabled
        self._load_all_bubbles_dirty = False
        self._visible_page_radius_spin_sync = False
        self._visible_page_radius_dirty = False
        self._visible_page_radius = self._load_visible_page_radius()
        self._visible_page_radius_last_committed = self._visible_page_radius
        self._bubble_load_delay_spin_sync = False
        self._bubble_load_delay_dirty = False
        self._bubble_load_delay_ms = self._load_bubble_load_delay_ms()
        self._bubble_load_delay_last_committed = self._bubble_load_delay_ms
        self._scroll_bubble_refresh_timer.setInterval(int(self._bubble_load_delay_ms))
        self._bubble_type_last_committed = self._bubble_type()
        self._bubble_type_dirty = False
        self._scale_bubbles_checkbox_sync = False
        self._scale_bubbles_enabled = self._load_scale_bubbles()
        self._scale_bubbles_last_committed = self._scale_bubbles_enabled
        self._scale_bubbles_dirty = False
        self._aside_bubble_scale_spin_sync = False
        self._aside_bubble_scale_pct = self._load_aside_bubble_scale_pct()
        self._aside_bubble_scale_last_committed = self._aside_bubble_scale_pct
        self._aside_bubble_scale_dirty = False
        self._tabs_autosync_checkbox_sync = False
        self._tabs_autosync_enabled = self._load_tabs_autosync_enabled()
        self._tabs_autosync_last_committed = self._tabs_autosync_enabled
        self._tabs_autosync_dirty = False

        self._canvasSettingsButton = QPushButton("Настройки ленты", self)
        self._canvasSettingsButton.clicked.connect(self._toggle_canvas_settings_panel)
        self._tabsSyncNowButton = QPushButton("Синхронизировать вкладки", self)
        self._tabsSyncNowButton.clicked.connect(self._on_sync_tabs_now_clicked)
        self._tabsSyncNowButton.setVisible(not self._tabs_autosync_enabled)

        self._canvasSettingsPanel = QWidget(self)
        self._canvasSettingsPanel.setObjectName("CanvasSettingsPanel")
        self._canvasSettingsPanel.setStyleSheet(
            "QWidget#CanvasSettingsPanel{background:#2f2f2f;border:1px solid #444;border-radius:4px;}"
        )
        self._canvasSettingsPanel.setVisible(False)
        panel_layout = QVBoxLayout(self._canvasSettingsPanel)
        panel_layout.setContentsMargins(8, 8, 8, 8)
        panel_layout.setSpacing(6)
        self._canvasSettingsCloseButton = QPushButton("×", self._canvasSettingsPanel)
        self._canvasSettingsCloseButton.setFixedSize(24, 24)
        self._canvasSettingsCloseButton.clicked.connect(self._close_canvas_settings_panel)
        panel_layout.addWidget(self._canvasSettingsCloseButton, alignment=Qt.AlignmentFlag.AlignRight)
        tabs_autosync_row = QHBoxLayout()
        self._tabsAutosyncCheckbox = QCheckBox("Автосинхронизация между вкладками", self._canvasSettingsPanel)
        self._tabsAutosyncCheckbox.setToolTip("Может убрать лаги на слабом железе")
        self._tabsAutosyncCheckbox.setChecked(bool(self._tabs_autosync_enabled))
        self._tabsAutosyncCheckbox.toggled.connect(self._on_tabs_autosync_checkbox_toggled)
        tabs_autosync_row.addWidget(self._tabsAutosyncCheckbox)
        tabs_autosync_row.addStretch(1)
        panel_layout.addLayout(tabs_autosync_row)
        type_row = QHBoxLayout()
        self._bubbleTypeLabel = QLabel("Тип пузырей", self._canvasSettingsPanel)
        self._bubbleTypeCombo = QComboBox(self._canvasSettingsPanel)
        self._bubbleTypeCombo.addItem("Сбоку", "aside")
        self._bubbleTypeCombo.addItem("Поверх", "on_top")
        self._bubbleTypeCombo.currentIndexChanged.connect(self._on_bubble_type_combo_changed)
        type_row.addWidget(self._bubbleTypeLabel)
        type_row.addStretch(1)
        type_row.addWidget(self._bubbleTypeCombo)
        panel_layout.addLayout(type_row)
        scale_bubbles_row = QHBoxLayout()
        self._scaleBubblesCheckbox = QCheckBox("Масштабировать пузыри", self._canvasSettingsPanel)
        self._scaleBubblesCheckbox.setChecked(self._scale_bubbles_enabled)
        self._scaleBubblesCheckbox.toggled.connect(self._on_scale_bubbles_checkbox_toggled)
        scale_bubbles_row.addWidget(self._scaleBubblesCheckbox)
        scale_bubbles_row.addStretch(1)
        panel_layout.addLayout(scale_bubbles_row)
        aside_scale_row = QHBoxLayout()
        self._asideBubbleScaleLabel = QLabel("Размер aside пузырей", self._canvasSettingsPanel)
        self._asideBubbleScaleSpin = QSpinBox(self._canvasSettingsPanel)
        self._asideBubbleScaleSpin.setRange(25, 300)
        self._asideBubbleScaleSpin.setSuffix("%")
        self._asideBubbleScaleSpin.setValue(self._aside_bubble_scale_pct)
        self._asideBubbleScaleSpin.valueChanged.connect(self._on_aside_bubble_scale_spin_value_changed)
        self._asideBubbleScaleSpin.editingFinished.connect(self._on_aside_bubble_scale_spin_editing_finished)
        aside_scale_row.addWidget(self._asideBubbleScaleLabel)
        aside_scale_row.addStretch(1)
        aside_scale_row.addWidget(self._asideBubbleScaleSpin)
        panel_layout.addLayout(aside_scale_row)
        aside_min_row = QHBoxLayout()
        self._asideMinWidthLabel = QLabel("Мин. ширина aside", self._canvasSettingsPanel)
        self._asideMinWidthSpin = QSpinBox(self._canvasSettingsPanel)
        self._asideMinWidthSpin.setRange(40, 5000)
        self._asideMinWidthSpin.setValue(self._aside_min_width_px)
        self._asideMinWidthSpin.valueChanged.connect(self._on_aside_width_spin_value_changed)
        self._asideMinWidthSpin.editingFinished.connect(self._on_aside_width_spin_editing_finished)
        aside_min_row.addWidget(self._asideMinWidthLabel)
        aside_min_row.addStretch(1)
        aside_min_row.addWidget(self._asideMinWidthSpin)
        panel_layout.addLayout(aside_min_row)
        aside_max_row = QHBoxLayout()
        self._asideMaxWidthLabel = QLabel("Макс. ширина aside", self._canvasSettingsPanel)
        self._asideMaxWidthSpin = QSpinBox(self._canvasSettingsPanel)
        self._asideMaxWidthSpin.setRange(40, 5000)
        self._asideMaxWidthSpin.setValue(self._aside_max_width_px)
        self._asideMaxWidthSpin.valueChanged.connect(self._on_aside_width_spin_value_changed)
        self._asideMaxWidthSpin.editingFinished.connect(self._on_aside_width_spin_editing_finished)
        aside_max_row.addWidget(self._asideMaxWidthLabel)
        aside_max_row.addStretch(1)
        aside_max_row.addWidget(self._asideMaxWidthSpin)
        panel_layout.addLayout(aside_max_row)
        separate_pages_row = QHBoxLayout()
        self._separatePagesCheckbox = QCheckBox("Разделять страницы", self._canvasSettingsPanel)
        self._separatePagesCheckbox.setChecked(self._separate_pages_enabled)
        self._separatePagesCheckbox.toggled.connect(self._on_separate_pages_checkbox_toggled)
        separate_pages_row.addWidget(self._separatePagesCheckbox)
        separate_pages_row.addStretch(1)
        panel_layout.addLayout(separate_pages_row)
        page_spacing_row = QHBoxLayout()
        self._pageSpacingLabel = QLabel("Расстояние между страницами", self._canvasSettingsPanel)
        self._pageSpacingSpin = QSpinBox(self._canvasSettingsPanel)
        self._pageSpacingSpin.setRange(0, 5000)
        self._pageSpacingSpin.setValue(self._page_spacing_px)
        self._pageSpacingSpin.valueChanged.connect(self._on_page_spacing_spin_value_changed)
        self._pageSpacingSpin.editingFinished.connect(self._on_page_spacing_spin_editing_finished)
        page_spacing_row.addWidget(self._pageSpacingLabel)
        page_spacing_row.addStretch(1)
        page_spacing_row.addWidget(self._pageSpacingSpin)
        panel_layout.addLayout(page_spacing_row)
        edge_margin_row = QHBoxLayout()
        self._verticalEdgeMarginLabel = QLabel("Расстояние до верт края", self._canvasSettingsPanel)
        self._verticalEdgeMarginSpin = QSpinBox(self._canvasSettingsPanel)
        self._verticalEdgeMarginSpin.setRange(0, 5000)
        self._verticalEdgeMarginSpin.setValue(self._vertical_edge_margin_px)
        self._verticalEdgeMarginSpin.valueChanged.connect(self._on_vertical_edge_margin_spin_value_changed)
        self._verticalEdgeMarginSpin.editingFinished.connect(self._on_vertical_edge_margin_spin_editing_finished)
        edge_margin_row.addWidget(self._verticalEdgeMarginLabel)
        edge_margin_row.addStretch(1)
        edge_margin_row.addWidget(self._verticalEdgeMarginSpin)
        panel_layout.addLayout(edge_margin_row)
        load_all_bubbles_row = QHBoxLayout()
        self._loadAllBubblesCheckbox = QCheckBox("Прогружать все пузыри", self._canvasSettingsPanel)
        self._loadAllBubblesCheckbox.setChecked(self._load_all_bubbles_enabled)
        self._loadAllBubblesCheckbox.toggled.connect(self._on_load_all_bubbles_checkbox_toggled)
        load_all_bubbles_row.addWidget(self._loadAllBubblesCheckbox)
        load_all_bubbles_row.addStretch(1)
        panel_layout.addLayout(load_all_bubbles_row)
        visible_radius_row = QHBoxLayout()
        self._visiblePageRadiusLabel = QLabel("Радиус прогрузки пузырей", self._canvasSettingsPanel)
        self._visiblePageRadiusSpin = QSpinBox(self._canvasSettingsPanel)
        self._visiblePageRadiusSpin.setRange(0, 50)
        self._visiblePageRadiusSpin.setValue(self._visible_page_radius)
        self._visiblePageRadiusSpin.valueChanged.connect(self._on_visible_page_radius_spin_value_changed)
        self._visiblePageRadiusSpin.editingFinished.connect(self._on_visible_page_radius_spin_editing_finished)
        visible_radius_row.addWidget(self._visiblePageRadiusLabel)
        visible_radius_row.addStretch(1)
        visible_radius_row.addWidget(self._visiblePageRadiusSpin)
        panel_layout.addLayout(visible_radius_row)
        bubble_delay_row = QHBoxLayout()
        self._bubbleLoadDelayLabel = QLabel("Задержка прогрузки (мс)", self._canvasSettingsPanel)
        self._bubbleLoadDelaySpin = QSpinBox(self._canvasSettingsPanel)
        self._bubbleLoadDelaySpin.setRange(0, 5000)
        self._bubbleLoadDelaySpin.setSingleStep(10)
        self._bubbleLoadDelaySpin.setValue(self._bubble_load_delay_ms)
        self._bubbleLoadDelaySpin.valueChanged.connect(self._on_bubble_load_delay_spin_value_changed)
        self._bubbleLoadDelaySpin.editingFinished.connect(self._on_bubble_load_delay_spin_editing_finished)
        bubble_delay_row.addWidget(self._bubbleLoadDelayLabel)
        bubble_delay_row.addStretch(1)
        bubble_delay_row.addWidget(self._bubbleLoadDelaySpin)
        panel_layout.addLayout(bubble_delay_row)
        opengl_row = QHBoxLayout()
        self._openglRenderCheckbox = QCheckBox("Рендер OpenGL", self._canvasSettingsPanel)
        self._openglRenderCheckbox.setChecked(bool(self._opengl_enabled))
        self._openglRenderCheckbox.toggled.connect(self._on_opengl_render_checkbox_toggled)
        opengl_row.addWidget(self._openglRenderCheckbox)
        opengl_row.addStretch(1)
        panel_layout.addLayout(opengl_row)
        opengl_device_row = QHBoxLayout()
        self._openglDeviceLabel = QLabel("Устройство OpenGL", self._canvasSettingsPanel)
        self._openglDeviceCombo = QComboBox(self._canvasSettingsPanel)
        self._openglDeviceCombo.addItem("Авто", "auto")
        self._openglDeviceCombo.addItem("Desktop", "desktop")
        self._openglDeviceCombo.addItem("Software", "software")
        self._openglDeviceCombo.currentIndexChanged.connect(self._on_opengl_device_combo_changed)
        opengl_device_row.addWidget(self._openglDeviceLabel)
        opengl_device_row.addStretch(1)
        opengl_device_row.addWidget(self._openglDeviceCombo)
        panel_layout.addLayout(opengl_device_row)
        self._openglRestartLabel = QLabel("Перезапустите для применения", self._canvasSettingsPanel)
        self._openglRestartLabel.setVisible(False)
        self._openglRestartLabel.setStyleSheet("color:#f2c94c; font-weight:600;")
        panel_layout.addWidget(self._openglRestartLabel)
        panel_layout.addStretch(1)
        self._canvasSettingsApplyButton = QPushButton("Применить", self._canvasSettingsPanel)
        self._canvasSettingsApplyButton.clicked.connect(self._on_canvas_settings_apply_clicked)
        self._canvasSettingsApplyButton.setEnabled(False)
        panel_layout.addWidget(self._canvasSettingsApplyButton)
        self._canvasSettingsPanel.setFixedSize(300, 580)

        self._sync_tabs_autosync_checkbox()
        self._sync_tabs_sync_now_button()
        self._sync_bubble_type_combo()
        self._sync_scale_bubbles_checkbox()
        self._sync_aside_bubble_scale_spin()
        self._sync_aside_width_spins()
        self._sync_separate_pages_checkbox()
        self._sync_page_spacing_spin()
        self._sync_vertical_edge_margin_spin()
        self._sync_load_all_bubbles_checkbox()
        self._sync_visible_page_radius_spin()
        self._sync_bubble_load_delay_spin()
        self._sync_opengl_render_controls()
        self._update_canvas_settings_apply_button_state()

        self._install_shortcuts()

        self._sort_images_numeric_first()
        self._start_image_prewarm()
        self._display_images()
        if self.overlays_model:
            # alias на данные модели, чтобы старые инструменты не падали
            try:
                self._overlay_images = self.overlays_model._overlays  # type: ignore[attr-defined]
            except Exception:
                self._overlay_images = []
            # один QGraphicsPixmapItem на страницу, ровно поверх картинки
            n = len(self.images)
            self._overlay_items = [None] * n
            self._ensure_all_overlays_items()
            self._sync_all_overlays_geom()
            self._refresh_all_overlays_pixmaps()
            # подписки на модель (быстрая синхронизация между CanvasView)
            self.overlays_model.overlayReplaced.connect(self._on_overlay_replaced)
            self.overlays_model.overlayCleared.connect(self._on_overlay_cleared)
            self.overlays_model.visibilityChanged.connect(self._on_overlays_visibility_changed)
        self._load_bubbles_from_project()

        # После инициализации выравниваем view по центру сцены
        self._center_view_on_scene()

        self.verticalScrollBar().valueChanged.connect(self._on_scroll_value_changed)
        self._update_page_counter()
        self._sync_hotkeys_label()

        # подключаем таймер для отложенного обновления модели
        self._text_update_timer.timeout.connect(self._flush_pending_text_updates)

    def _normalize_opengl_device(self, value: object) -> str:
        try:
            v = str(value or "").strip().lower()
        except Exception:
            v = ""
        if v in ("desktop", "software"):
            return v
        return "auto"

    def _global_canvas_config(self):
        cfg = getattr(self, "user_config", None)
        if cfg is None:
            return None
        canvas_cfg = getattr(cfg, "Canvas", None)
        return canvas_cfg

    def _load_opengl_render_enabled(self) -> bool:
        if os.environ.get("MANGAFUCKER_DISABLE_GPU_VIEWPORT", "").strip() in ("1", "true", "True"):
            return False
        enabled = False
        has_global_value = False
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                val = getattr(global_canvas, "opengl_enabled", None)
                if val is not None:
                    enabled = bool(val)
                    has_global_value = True
            except Exception:
                pass
        if has_global_value:
            return bool(enabled)
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "opengl_enabled", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "opengl_enabled", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "opengl_enabled", None)
            if proj_val is not None:
                enabled = bool(proj_val)
        except Exception:
            pass
        return bool(enabled)

    def _load_opengl_device(self) -> str:
        device = "auto"
        has_global_value = False
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                val = getattr(global_canvas, "opengl_device", None)
                if val is not None:
                    device = str(val)
                    has_global_value = True
            except Exception:
                pass
        if has_global_value:
            return self._normalize_opengl_device(device)
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "opengl_device", None)
                if val is not None:
                    device = str(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "opengl_device", None)
                if val is not None:
                    device = str(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "opengl_device", None)
            if proj_val is not None:
                device = str(proj_val)
        except Exception:
            pass
        return self._normalize_opengl_device(device)

    def _sync_opengl_render_controls(self) -> None:
        if not hasattr(self, "_openglRenderCheckbox") or not hasattr(self, "_openglDeviceCombo"):
            return
        self._opengl_settings_ui_sync = True
        self._openglRenderCheckbox.blockSignals(True)
        self._openglDeviceCombo.blockSignals(True)
        self._openglRenderCheckbox.setChecked(bool(self._opengl_enabled))
        idx = self._openglDeviceCombo.findData(self._normalize_opengl_device(self._opengl_device))
        if idx < 0:
            idx = self._openglDeviceCombo.findData("auto")
        if idx >= 0:
            self._openglDeviceCombo.setCurrentIndex(idx)
        enabled = bool(self._opengl_enabled)
        self._openglDeviceCombo.setEnabled(enabled)
        self._openglDeviceLabel.setEnabled(enabled)
        self._openglRenderCheckbox.blockSignals(False)
        self._openglDeviceCombo.blockSignals(False)
        self._opengl_settings_ui_sync = False

    def _save_opengl_render_to_config(self) -> None:
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                global_canvas.opengl_enabled = bool(self._opengl_enabled)
            except Exception:
                pass
            try:
                global_canvas.opengl_device = str(self._opengl_device)
            except Exception:
                pass
        self.project.opengl_enabled = bool(self._opengl_enabled)
        self.project.opengl_device = str(self._opengl_device)

    def _set_raster_viewport(self) -> None:
        try:
            self.setViewport(QWidget())
        except Exception:
            pass
        self._gpu_viewport_active = False

    def _configure_viewport_backend(self) -> None:
        if not bool(self._opengl_enabled):
            self._set_raster_viewport()
            return
        self._try_enable_gpu_viewport()
        if not self._gpu_viewport_active:
            self._set_raster_viewport()

    def _apply_opengl_render_settings(
        self,
        enabled: bool,
        device: str,
        *,
        sync_ui: bool,
        save_config: bool,
    ) -> None:
        enabled = bool(enabled)
        device = self._normalize_opengl_device(device)
        self._opengl_enabled = enabled
        self._opengl_device = device
        if sync_ui:
            self._sync_opengl_render_controls()
        if save_config:
            self._save_opengl_render_to_config()
            self._opengl_enabled_last_committed = self._opengl_enabled
            self._opengl_device_last_committed = self._opengl_device
            self._opengl_enabled_dirty = False
            self._opengl_device_dirty = False
            if not self.model:
                self._autosave_timer.start()

    def _try_enable_gpu_viewport(self) -> None:
        """
        Включает OpenGL viewport для ускорения pan/zoom/compositing. Экспериментальная реализация, не является основной.
        При любой ошибке тихо откатываемся на стандартный QWidget viewport.
        """
        if os.environ.get("MANGAFUCKER_DISABLE_GPU_VIEWPORT", "").strip() in ("1", "true", "True"):
            return
        try:
            from PyQt6.QtOpenGLWidgets import QOpenGLWidget
            from PyQt6.QtGui import QSurfaceFormat
            device = self._normalize_opengl_device(getattr(self, "_opengl_device", "auto"))
            if device == "desktop":
                os.environ["QT_OPENGL"] = "desktop"
            elif device == "software":
                os.environ["QT_OPENGL"] = "software"
            else:
                os.environ.pop("QT_OPENGL", None)
            fmt = QSurfaceFormat()
            fmt.setRenderableType(QSurfaceFormat.RenderableType.OpenGL)
            fmt.setProfile(QSurfaceFormat.OpenGLContextProfile.CompatibilityProfile)
            fmt.setVersion(2, 1)
            fmt.setSwapBehavior(QSurfaceFormat.SwapBehavior.DoubleBuffer)
            fmt.setSamples(0)
            gl_viewport = QOpenGLWidget()
            gl_viewport.setFormat(fmt)
            gl_viewport.setUpdateBehavior(QOpenGLWidget.UpdateBehavior.NoPartialUpdate)
            gl_viewport.setAutoFillBackground(True)
            gl_viewport.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground, False)
            # Явный непрозрачный фон, чтобы пустые области не были прозрачными
            self.setBackgroundBrush(QBrush(QColor(43, 43, 43)))
            self.setViewport(gl_viewport)
            self._gpu_viewport_active = True
            if not CanvasView._gpu_viewport_enabled_logged:
                print("[CanvasView] OpenGL viewport enabled")
                CanvasView._gpu_viewport_enabled_logged = True
        except Exception as e:
            self._gpu_viewport_active = False
            if not CanvasView._gpu_viewport_unavailable_logged:
                print(f"[CanvasView] OpenGL viewport unavailable, fallback to raster: {e}")
                CanvasView._gpu_viewport_unavailable_logged = True

    def _mark_item_as_bubble_part(self, item: Optional[QGraphicsItem], bid: int) -> None:
        if item is None:
            return
        try:
            item.setData(1, ("bubble", int(bid)))
        except Exception:
            pass

    def _mark_project_bubbles_index_dirty(self) -> None:
        self._project_bubbles_index_dirty = True

    def _project_bubbles_index(self) -> Dict[int, Dict[int, dict]]:
        if not self._project_bubbles_index_dirty:
            return self._project_bubbles_by_page
        by_page: Dict[int, Dict[int, dict]] = {}
        by_id: Dict[int, dict] = {}
        for rec in list(getattr(self.project, "bubbles", [])):
            try:
                bid = int(rec.get("id"))
            except Exception:
                continue
            by_id[bid] = rec
            if self._is_unplaced(rec):
                continue
            try:
                img_idx = int(rec.get("img_idx"))
            except Exception:
                continue
            page_map = by_page.setdefault(img_idx, {})
            page_map[bid] = rec
        self._project_bubbles_by_page = by_page
        self._project_bubbles_by_id = by_id
        self._project_bubbles_index_dirty = False
        return self._project_bubbles_by_page

    def _record_for_bid(self, bid: int) -> Optional[dict]:
        rec = self._project_bubbles_by_id.get(int(bid))
        if rec is not None:
            return rec
        self._project_bubbles_index()
        return self._project_bubbles_by_id.get(int(bid))

    def _set_bubble_runtime_visible(self, b: "BubbleRuntime", visible: bool) -> None:
        if not b:
            return
        if not visible:
            for item in (b.line_item, b.proxy_widget, b.header_proxy, b.original_proxy, b.footer_proxy, b.rect_item, b.rect_item_inner):
                if item:
                    item.setVisible(False)
            if b.rect_handles:
                for h in b.rect_handles:
                    if h:
                        h.setVisible(False)
            return
        should_show = bool(self.bubbles_visible)
        if b.line_item:
            b.line_item.setVisible(should_show and self._bubble_type() != "on_top")
        for item in (b.proxy_widget, b.header_proxy, b.original_proxy, b.footer_proxy):
            if item:
                item.setVisible(should_show)
        self._refresh_rect_visibility(b.id)
        self._apply_bubble_opacity(b.id)

    def _set_scroll_render_quality(self, fast: bool) -> None:
        if bool(fast) == self._scroll_fast_quality:
            return
        self._scroll_fast_quality = bool(fast)
        mode = Qt.TransformationMode.FastTransformation if fast else Qt.TransformationMode.SmoothTransformation
        for it in self.image_items:
            try:
                it.setTransformationMode(mode)
            except Exception:
                pass
        for it in self._overlay_items:
            if it is None:
                continue
            try:
                it.setTransformationMode(mode)
            except Exception:
                pass

    def _restore_smooth_render_quality(self) -> None:
        self._set_scroll_render_quality(False)

    def _prepare_embedded_widget(self, w: QWidget) -> QWidget:
        """
        Подготовка QWidget перед embed.
        В текущей ветке оставляем поведение как в r243: без доп. флагов.
        """
        return w

    def _add_embedded_proxy_widget(self, widget: QWidget) -> QGraphicsProxyWidget:
        """
        Единая точка embed виджета в сцену.
        Используем стандартный путь Qt без нестабильных флагов, чтобы избежать segfault.
        """
        widget = self._prepare_embedded_widget(widget)
        return self.scene.addWidget(widget)

    def _dispose_proxy_widget(self, proxy: Optional[QGraphicsProxyWidget]) -> None:
        """Безопасно удаляет proxy + embedded widget без временного top-level detatch."""
        if not proxy:
            return
        try:
            proxy.hide()
        except Exception:
            pass
        try:
            self.scene.removeItem(proxy)
        except Exception:
            pass
        try:
            proxy.deleteLater()
        except Exception:
            pass

    # ------------------- хоткеи -------------------
    def _install_shortcuts(self):
        def _with_ctx(act):
            act.setShortcutContext(Qt.ShortcutContext.WidgetWithChildrenShortcut)
            self.addAction(act)
            return act

        act_add = _with_ctx(QAction(self))
        act_add.setShortcut(QKeySequence(Qt.Key.Key_T))
        act_add.triggered.connect(self._on_add_bubble_shortcut)

        act_del = _with_ctx(QAction(self))
        act_del.setShortcut(QKeySequence(Qt.Key.Key_Delete))
        act_del.triggered.connect(self._on_delete_selected)

        act_zi = _with_ctx(QAction(self))
        act_zi.setShortcuts([QKeySequence("Ctrl++"), QKeySequence("Ctrl+=")])
        act_zi.triggered.connect(lambda: self._zoom_canvas(1.1))

        act_zo = _with_ctx(QAction(self))
        act_zo.setShortcut(QKeySequence("Ctrl+-"))
        act_zo.triggered.connect(lambda: self._zoom_canvas(1/1.1))

        act_z0 = _with_ctx(QAction(self))
        act_z0.setShortcut(QKeySequence("Ctrl+0"))
        act_z0.triggered.connect(lambda: self._set_canvas_scale(1.0))

        act_g  = _with_ctx(QAction(self))
        act_g.setShortcut(QKeySequence(Qt.Key.Key_G))
        act_g.triggered.connect(self.toggle_bubbles)
        
    def _on_add_bubble_shortcut(self):
        if not self.editable:
            return
        
        pos = self.mapFromGlobal(QCursor.pos())
        scene_pt = self.mapToScene(pos)
        for idx, bbox in enumerate(self.image_bboxes):
            if bbox.contains(scene_pt):
                self.create_bubble(idx, scene_pt.x(), scene_pt.y())
                return

    def _layout_top_labels(self):
        # страница
        self._pageLabel.adjustSize()
        x, y = 8, 8
        self._pageLabel.move(x, y)

        # масштаб — сразу справа с небольшим зазором
        self._scaleLabel.adjustSize()
        gap = 6
        sx = self._pageLabel.x() + self._pageLabel.width() + gap
        self._scaleLabel.move(sx, y)
        self._layout_canvas_controls()

    def _update_scale_indicator(self):
        val = round(getattr(self, "canvas_scale", 1.0) + 1e-9, 1)
        self._scaleLabel.setText(f"{val:.1f}×")
        self._layout_top_labels()

    def _top_row_height(self) -> int:
        widgets = [self._pageLabel, self._scaleLabel]
        for name in ("_cutLinesButton", "_transformButton", "_transformExitButton", "_transformResetButton"):
            w = getattr(self, name, None)
            if isinstance(w, QWidget) and w.isVisible():
                widgets.append(w)
        return max(w.sizeHint().height() for w in widgets)

    def _layout_canvas_controls(self):
        gap = 6
        x = 8
        top_row_h = self._top_row_height()
        row_y = 8 + top_row_h + gap

        self._bubblesCheckbox.adjustSize()
        self._bubblesCheckbox.move(x, row_y)

        row_y += self._bubblesCheckbox.height() + gap
        if self._bubbleOpacitySlider.isVisible():
            self._bubbleOpacitySlider.move(x, row_y)
            row_y += self._bubbleOpacitySlider.height() + gap

        self._canvasSettingsButton.adjustSize()
        self._canvasSettingsButton.move(x, row_y)
        row_y += self._canvasSettingsButton.height() + gap

        if self._tabsSyncNowButton.isVisible():
            self._tabsSyncNowButton.adjustSize()
            self._tabsSyncNowButton.move(x, row_y)
            row_y += self._tabsSyncNowButton.height() + gap

        if self._canvasSettingsPanel.isVisible():
            self._canvasSettingsPanel.move(x, row_y)

    def _load_tabs_autosync_enabled(self) -> bool:
        enabled = True
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                val = getattr(global_canvas, "auto_sync_tabs", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        return bool(enabled)

    def _save_tabs_autosync_to_config(self) -> None:
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                global_canvas.auto_sync_tabs = bool(self._tabs_autosync_enabled)
            except Exception:
                pass

    def _sync_tabs_autosync_checkbox(self) -> None:
        if not hasattr(self, "_tabsAutosyncCheckbox"):
            return
        self._tabs_autosync_checkbox_sync = True
        self._tabsAutosyncCheckbox.blockSignals(True)
        self._tabsAutosyncCheckbox.setChecked(bool(self._tabs_autosync_enabled))
        self._tabsAutosyncCheckbox.blockSignals(False)
        self._tabs_autosync_checkbox_sync = False

    def _sync_tabs_sync_now_button(self) -> None:
        if not hasattr(self, "_tabsSyncNowButton"):
            return
        self._tabsSyncNowButton.setVisible(not bool(self._tabs_autosync_enabled))
        self._layout_canvas_controls()

    def _apply_tabs_autosync_enabled(
        self,
        enabled: bool,
        *,
        sync_ui: bool,
        save_config: bool,
        broadcast: bool,
    ) -> None:
        self._tabs_autosync_enabled = bool(enabled)
        if sync_ui:
            self._sync_tabs_autosync_checkbox()
            self._sync_tabs_sync_now_button()
        if save_config:
            self._save_tabs_autosync_to_config()
            self._tabs_autosync_last_committed = self._tabs_autosync_enabled
            self._tabs_autosync_dirty = False
        if broadcast and self.model:
            self.model.set_tabs_autosync(bool(self._tabs_autosync_enabled), self.uid)

    def _sync_tabs_from_models(self) -> None:
        self._mark_project_bubbles_index_dirty()
        self._load_bubbles_from_project()
        if self.overlays_model:
            self._ensure_all_overlays_items()
            self._sync_all_overlays_geom()
            self._refresh_all_overlays_pixmaps()

    def _on_sync_tabs_now_clicked(self) -> None:
        self._sync_tabs_from_models()
        if self.model:
            self.model.request_tabs_sync(self.uid)

    def _sync_bubble_type_combo(self):
        if not hasattr(self, "_bubbleTypeCombo"):
            return
        bt = self._bubble_type()
        idx = self._bubbleTypeCombo.findData(bt)
        if idx >= 0:
            self._bubbleTypeCombo.blockSignals(True)
            self._bubbleTypeCombo.setCurrentIndex(idx)
            self._bubbleTypeCombo.blockSignals(False)

    def _sync_scale_bubbles_checkbox(self) -> None:
        if not hasattr(self, "_scaleBubblesCheckbox"):
            return
        self._scale_bubbles_checkbox_sync = True
        self._scaleBubblesCheckbox.blockSignals(True)
        self._scaleBubblesCheckbox.setChecked(bool(self._scale_bubbles_enabled))
        self._scaleBubblesCheckbox.blockSignals(False)
        self._scale_bubbles_checkbox_sync = False

    def _load_scale_bubbles(self) -> bool:
        enabled = False
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "scale_bubbles", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "scale_bubbles", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "scale_bubbles", None)
            if proj_val is not None:
                enabled = bool(proj_val)
        except Exception:
            pass
        return bool(enabled)

    def _normalize_aside_bubble_scale_pct(self, value: int) -> int:
        try:
            value = int(value)
        except Exception:
            value = 100
        return max(25, min(300, value))

    def _load_aside_bubble_scale_pct(self) -> int:
        scale_pct = 100
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "aside_bubble_scale_pct", None)
                if val is not None:
                    scale_pct = int(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "aside_bubble_scale_pct", None)
                if val is not None:
                    scale_pct = int(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "aside_bubble_scale_pct", None)
            if proj_val is not None:
                scale_pct = int(proj_val)
        except Exception:
            pass
        return self._normalize_aside_bubble_scale_pct(scale_pct)

    def _sync_aside_bubble_scale_spin(self) -> None:
        if not hasattr(self, "_asideBubbleScaleSpin"):
            return
        self._aside_bubble_scale_spin_sync = True
        self._asideBubbleScaleSpin.blockSignals(True)
        self._asideBubbleScaleSpin.setValue(int(self._aside_bubble_scale_pct))
        self._asideBubbleScaleSpin.blockSignals(False)
        self._aside_bubble_scale_spin_sync = False

    def _save_aside_bubble_scale_to_config(self) -> None:
        settings = getattr(self.project, "settings", None)
        if settings:
            try:
                settings.canvas.aside_bubble_scale_pct = int(self._aside_bubble_scale_pct)
            except Exception:
                pass
            try:
                settings.aside_bubble_scale_pct = int(self._aside_bubble_scale_pct)
            except Exception:
                pass
        self.project.aside_bubble_scale_pct = int(self._aside_bubble_scale_pct)

    def _aside_bubble_scale_factor(self) -> float:
        if self._bubble_type() == "on_top":
            return 1.0
        return max(0.25, min(3.0, float(self._aside_bubble_scale_pct) / 100.0))

    def _save_scale_bubbles_to_config(self) -> None:
        settings = getattr(self.project, "settings", None)
        if settings:
            try:
                settings.canvas.scale_bubbles = bool(self._scale_bubbles_enabled)
            except Exception:
                pass
            try:
                settings.scale_bubbles = bool(self._scale_bubbles_enabled)
            except Exception:
                pass
        self.project.scale_bubbles = bool(self._scale_bubbles_enabled)

    def _should_scale_aside_bubbles(self) -> bool:
        return self._bubble_type() != "on_top" and bool(self._scale_bubbles_enabled)

    def _normalize_aside_width_limits(self, min_px: int, max_px: int) -> Tuple[int, int]:
        try:
            min_px = int(min_px)
        except Exception:
            min_px = 450
        try:
            max_px = int(max_px)
        except Exception:
            max_px = 550
        min_px = max(40, min_px)
        max_px = max(min_px, max_px)
        return min_px, max_px

    def _load_aside_width_limits(self) -> Tuple[int, int]:
        min_px = 450
        max_px = 550
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "aside_min_width_px", None)
                if val is not None:
                    min_px = int(val)
            except Exception:
                pass
            try:
                val = getattr(canvas_settings, "aside_max_width_px", None)
                if val is not None:
                    max_px = int(val)
            except Exception:
                pass
        return self._normalize_aside_width_limits(min_px, max_px)

    def _sync_aside_width_spins(self) -> None:
        if not hasattr(self, "_asideMinWidthSpin") or not hasattr(self, "_asideMaxWidthSpin"):
            return
        self._aside_width_spin_sync = True
        self._asideMinWidthSpin.blockSignals(True)
        self._asideMaxWidthSpin.blockSignals(True)
        self._asideMinWidthSpin.setValue(int(self._aside_min_width_px))
        self._asideMaxWidthSpin.setValue(int(self._aside_max_width_px))
        self._asideMinWidthSpin.blockSignals(False)
        self._asideMaxWidthSpin.blockSignals(False)
        self._aside_width_spin_sync = False

    def _normalize_page_spacing(self, spacing_px: int) -> int:
        try:
            spacing_px = int(spacing_px)
        except Exception:
            spacing_px = 200
        return max(0, spacing_px)

    def _load_page_spacing(self) -> int:
        spacing_px = 200
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "page_spacing_px", None)
                if val is not None:
                    spacing_px = int(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "page_spacing_px", None)
                if val is not None:
                    spacing_px = int(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "page_spacing_px", None)
            if proj_val is not None:
                spacing_px = int(proj_val)
        except Exception:
            pass
        return self._normalize_page_spacing(spacing_px)

    def _sync_page_spacing_spin(self) -> None:
        if not hasattr(self, "_pageSpacingSpin"):
            return
        self._page_spacing_spin_sync = True
        self._pageSpacingSpin.blockSignals(True)
        self._pageSpacingSpin.setValue(int(self._page_spacing_px))
        self._pageSpacingSpin.blockSignals(False)
        self._page_spacing_spin_sync = False

    def _load_separate_pages_enabled(self) -> bool:
        enabled = True
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "separate_pages", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "separate_pages", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "separate_pages", None)
            if proj_val is not None:
                enabled = bool(proj_val)
        except Exception:
            pass
        return bool(enabled)

    def _sync_separate_pages_checkbox(self) -> None:
        if not hasattr(self, "_separatePagesCheckbox"):
            return
        self._separate_pages_checkbox_sync = True
        self._separatePagesCheckbox.blockSignals(True)
        self._separatePagesCheckbox.setChecked(bool(self._separate_pages_enabled))
        self._separatePagesCheckbox.blockSignals(False)
        self._separate_pages_checkbox_sync = False
        enabled = bool(self._separate_pages_enabled)
        if hasattr(self, "_pageSpacingSpin"):
            self._pageSpacingSpin.setEnabled(enabled)
        if hasattr(self, "_pageSpacingLabel"):
            self._pageSpacingLabel.setEnabled(enabled)

    def _effective_page_spacing_px(self) -> int:
        return int(self._page_spacing_px) if self._separate_pages_enabled else 0

    def _normalize_vertical_edge_margin(self, margin_px: int) -> int:
        try:
            margin_px = int(margin_px)
        except Exception:
            margin_px = 200
        return max(0, margin_px)

    def _load_all_bubbles_enabled_setting(self) -> bool:
        enabled = False
        has_global_value = False
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                val = getattr(global_canvas, "load_all_bubbles", None)
                if val is not None:
                    enabled = bool(val)
                    has_global_value = True
            except Exception:
                pass
        if has_global_value:
            return bool(enabled)
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "load_all_bubbles", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "load_all_bubbles", None)
                if val is not None:
                    enabled = bool(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "load_all_bubbles", None)
            if proj_val is not None:
                enabled = bool(proj_val)
        except Exception:
            pass
        return bool(enabled)

    def _normalize_visible_page_radius(self, radius: int) -> int:
        try:
            radius = int(radius)
        except Exception:
            radius = 2
        return max(0, min(50, radius))

    def _normalize_bubble_load_delay_ms(self, delay_ms: int) -> int:
        try:
            delay_ms = int(delay_ms)
        except Exception:
            delay_ms = 260
        return max(0, min(5000, delay_ms))

    def _load_vertical_edge_margin(self) -> int:
        margin_px = 200
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "vertical_edge_margin_px", None)
                if val is not None:
                    margin_px = int(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "vertical_edge_margin_px", None)
                if val is not None:
                    margin_px = int(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "vertical_edge_margin_px", None)
            if proj_val is not None:
                margin_px = int(proj_val)
        except Exception:
            pass
        return self._normalize_vertical_edge_margin(margin_px)

    def _load_visible_page_radius(self) -> int:
        radius = 2
        has_global_value = False
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                val = getattr(global_canvas, "visible_page_radius", None)
                if val is not None:
                    radius = int(val)
                    has_global_value = True
            except Exception:
                pass
        if has_global_value:
            return self._normalize_visible_page_radius(radius)
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "visible_page_radius", None)
                if val is not None:
                    radius = int(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "visible_page_radius", None)
                if val is not None:
                    radius = int(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "visible_page_radius", None)
            if proj_val is not None:
                radius = int(proj_val)
        except Exception:
            pass
        return self._normalize_visible_page_radius(radius)

    def _load_bubble_load_delay_ms(self) -> int:
        delay_ms = 260
        has_global_value = False
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                val = getattr(global_canvas, "bubble_load_delay_ms", None)
                if val is not None:
                    delay_ms = int(val)
                    has_global_value = True
            except Exception:
                pass
        if has_global_value:
            return self._normalize_bubble_load_delay_ms(delay_ms)
        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "bubble_load_delay_ms", None)
                if val is not None:
                    delay_ms = int(val)
            except Exception:
                pass
        if settings:
            try:
                val = getattr(settings, "bubble_load_delay_ms", None)
                if val is not None:
                    delay_ms = int(val)
            except Exception:
                pass
        try:
            proj_val = getattr(self.project, "bubble_load_delay_ms", None)
            if proj_val is not None:
                delay_ms = int(proj_val)
        except Exception:
            pass
        return self._normalize_bubble_load_delay_ms(delay_ms)

    def _sync_vertical_edge_margin_spin(self) -> None:
        if not hasattr(self, "_verticalEdgeMarginSpin"):
            return
        self._vertical_edge_margin_spin_sync = True
        self._verticalEdgeMarginSpin.blockSignals(True)
        self._verticalEdgeMarginSpin.setValue(int(self._vertical_edge_margin_px))
        self._verticalEdgeMarginSpin.blockSignals(False)
        self._vertical_edge_margin_spin_sync = False

    def _sync_load_all_bubbles_checkbox(self) -> None:
        if not hasattr(self, "_loadAllBubblesCheckbox"):
            return
        self._load_all_bubbles_checkbox_sync = True
        self._loadAllBubblesCheckbox.blockSignals(True)
        self._loadAllBubblesCheckbox.setChecked(bool(self._load_all_bubbles_enabled))
        self._loadAllBubblesCheckbox.blockSignals(False)
        self._load_all_bubbles_checkbox_sync = False
        radius_enabled = not bool(self._load_all_bubbles_enabled)
        if hasattr(self, "_visiblePageRadiusSpin"):
            self._visiblePageRadiusSpin.setEnabled(radius_enabled)
        if hasattr(self, "_visiblePageRadiusLabel"):
            self._visiblePageRadiusLabel.setEnabled(radius_enabled)

    def _sync_visible_page_radius_spin(self) -> None:
        if not hasattr(self, "_visiblePageRadiusSpin"):
            return
        self._visible_page_radius_spin_sync = True
        self._visiblePageRadiusSpin.blockSignals(True)
        self._visiblePageRadiusSpin.setValue(int(self._visible_page_radius))
        self._visiblePageRadiusSpin.blockSignals(False)
        self._visible_page_radius_spin_sync = False

    def _sync_bubble_load_delay_spin(self) -> None:
        if not hasattr(self, "_bubbleLoadDelaySpin"):
            return
        self._bubble_load_delay_spin_sync = True
        self._bubbleLoadDelaySpin.blockSignals(True)
        self._bubbleLoadDelaySpin.setValue(int(self._bubble_load_delay_ms))
        self._bubbleLoadDelaySpin.blockSignals(False)
        self._bubble_load_delay_spin_sync = False

    def _save_aside_width_limits_to_config(self) -> None:
        settings = getattr(self.project, "settings", None)
        if settings:
            try:
                settings.canvas.aside_min_width_px = int(self._aside_min_width_px)
            except Exception:
                pass
            try:
                settings.canvas.aside_max_width_px = int(self._aside_max_width_px)
            except Exception:
                pass
            try:
                settings.aside_min_width_px = int(self._aside_min_width_px)
            except Exception:
                pass
            try:
                settings.aside_max_width_px = int(self._aside_max_width_px)
            except Exception:
                pass
        self.project.aside_min_width_px = int(self._aside_min_width_px)
        self.project.aside_max_width_px = int(self._aside_max_width_px)

    def _save_page_spacing_to_config(self) -> None:
        settings = getattr(self.project, "settings", None)
        if settings:
            try:
                settings.canvas.page_spacing_px = int(self._page_spacing_px)
            except Exception:
                pass
            try:
                settings.page_spacing_px = int(self._page_spacing_px)
            except Exception:
                pass
        self.project.page_spacing_px = int(self._page_spacing_px)

    def _save_separate_pages_to_config(self) -> None:
        settings = getattr(self.project, "settings", None)
        if settings:
            try:
                settings.canvas.separate_pages = bool(self._separate_pages_enabled)
            except Exception:
                pass
            try:
                settings.separate_pages = bool(self._separate_pages_enabled)
            except Exception:
                pass
        self.project.separate_pages = bool(self._separate_pages_enabled)

    def _save_vertical_edge_margin_to_config(self) -> None:
        settings = getattr(self.project, "settings", None)
        if settings:
            try:
                settings.canvas.vertical_edge_margin_px = int(self._vertical_edge_margin_px)
            except Exception:
                pass
        self.project.vertical_edge_margin_px = int(self._vertical_edge_margin_px)

    def _save_load_all_bubbles_to_config(self) -> None:
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                global_canvas.load_all_bubbles = bool(self._load_all_bubbles_enabled)
            except Exception:
                pass
        self.project.load_all_bubbles = bool(self._load_all_bubbles_enabled)

    def _save_visible_page_radius_to_config(self) -> None:
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                global_canvas.visible_page_radius = int(self._visible_page_radius)
            except Exception:
                pass
        self.project.visible_page_radius = int(self._visible_page_radius)

    def _save_bubble_load_delay_to_config(self) -> None:
        global_canvas = self._global_canvas_config()
        if global_canvas is not None:
            try:
                global_canvas.bubble_load_delay_ms = int(self._bubble_load_delay_ms)
            except Exception:
                pass
        self.project.bubble_load_delay_ms = int(self._bubble_load_delay_ms)

    def _apply_scale_bubbles_enabled(
        self,
        enabled: bool,
        *,
        sync_ui: bool,
        save_config: bool,
        broadcast: bool,
        reflow: bool,
    ) -> None:
        enabled = bool(enabled)
        changed = enabled != self._scale_bubbles_enabled
        self._scale_bubbles_enabled = enabled
        if sync_ui:
            self._sync_scale_bubbles_checkbox()
        if reflow and changed and self._bubble_type() != "on_top":
            self._reflow_aside_bubbles()
        if save_config:
            self._save_scale_bubbles_to_config()
            self._scale_bubbles_last_committed = enabled
            self._scale_bubbles_dirty = False
        if broadcast and self.model:
            self.model.set_scale_bubbles(enabled, self.uid)
        elif save_config and not self.model:
            self._autosave_timer.start()

    def _apply_aside_bubble_scale_pct(
        self,
        scale_pct: int,
        *,
        sync_ui: bool,
        save_config: bool,
        reflow: bool,
    ) -> None:
        scale_pct = self._normalize_aside_bubble_scale_pct(scale_pct)
        changed = scale_pct != self._aside_bubble_scale_pct
        self._aside_bubble_scale_pct = scale_pct
        if sync_ui:
            self._sync_aside_bubble_scale_spin()
        if reflow and changed and self._bubble_type() != "on_top":
            self._reflow_aside_bubbles()
        if save_config:
            self._save_aside_bubble_scale_to_config()
            self._aside_bubble_scale_last_committed = scale_pct
            self._aside_bubble_scale_dirty = False
            if not self.model:
                self._autosave_timer.start()

    def _reflow_aside_bubbles(self) -> None:
        if self._bubble_type() == "on_top":
            return
        for bid, b in list(self.bubbles.items()):
            if 0 <= b.img_idx < len(self.image_bboxes):
                self._apply_bubble_imgpos(bid, b.img_idx, b.img_u, b.img_v, b.side, broadcast=False, repack=False)
        for i in self._visible_page_indexes():
            self._repack_bubbles_for(i, "left")
            self._repack_bubbles_for(i, "right")

    def _apply_aside_width_limits(
        self,
        min_px: int,
        max_px: int,
        *,
        sync_ui: bool,
        save_config: bool,
        broadcast: bool,
        reflow: bool,
    ) -> None:
        min_px, max_px = self._normalize_aside_width_limits(min_px, max_px)
        changed = (min_px, max_px) != (self._aside_min_width_px, self._aside_max_width_px)
        self._aside_min_width_px = min_px
        self._aside_max_width_px = max_px
        if sync_ui:
            self._sync_aside_width_spins()
        if reflow and changed and self._bubble_type() != "on_top":
            self._reflow_aside_bubbles()
        if save_config:
            self._save_aside_width_limits_to_config()
            self._aside_last_committed = (min_px, max_px)
            self._aside_width_dirty = False
        if broadcast and self.model:
            self.model.set_aside_width_limits(min_px, max_px, self.uid)
        elif save_config and not self.model:
            self._autosave_timer.start()

    def _reflow_canvas_layout(self) -> None:
        self._display_images()
        self._refresh_visible_bubbles(force_repack=False)
        for bid, b in list(self.bubbles.items()):
            if 0 <= b.img_idx < len(self.image_bboxes):
                self._apply_bubble_imgpos(
                    bid, b.img_idx, b.img_u, b.img_v, b.side, broadcast=False, repack=False
                )
        if self._bubble_type() != "on_top":
            for i in self._visible_page_indexes():
                self._repack_bubbles_for(i, "left")
                self._repack_bubbles_for(i, "right")
        self._refresh_scene_rect_to_content()
        self._update_page_counter()
        if self.overlays_model:
            self._sync_all_overlays_geom()

    def _apply_page_spacing(
        self,
        spacing_px: int,
        *,
        sync_ui: bool,
        save_config: bool,
        broadcast: bool,
        reflow: bool,
    ) -> None:
        spacing_px = self._normalize_page_spacing(spacing_px)
        changed = spacing_px != self._page_spacing_px
        self._page_spacing_px = spacing_px
        if sync_ui:
            self._sync_page_spacing_spin()
        if reflow and changed:
            self._reflow_canvas_layout()
        if save_config:
            self._save_page_spacing_to_config()
            self._page_spacing_last_committed = spacing_px
            self._page_spacing_dirty = False
        if broadcast and self.model:
            self.model.set_page_spacing(spacing_px, self.uid)
        elif save_config and not self.model:
            self._autosave_timer.start()

    def _apply_separate_pages_enabled(
        self,
        enabled: bool,
        *,
        sync_ui: bool,
        save_config: bool,
        broadcast: bool,
        reflow: bool,
    ) -> None:
        enabled = bool(enabled)
        changed = enabled != self._separate_pages_enabled
        self._separate_pages_enabled = enabled
        if sync_ui:
            self._sync_separate_pages_checkbox()
        if reflow and changed:
            self._reflow_canvas_layout()
        if save_config:
            self._save_separate_pages_to_config()
            self._separate_pages_last_committed = enabled
            self._separate_pages_dirty = False
        if broadcast and self.model:
            self.model.set_separate_pages(enabled, self.uid)
        elif save_config and not self.model:
            self._autosave_timer.start()

    def _apply_vertical_edge_margin(
        self,
        margin_px: int,
        *,
        sync_ui: bool,
        save_config: bool,
        broadcast: bool,
        reflow: bool,
    ) -> None:
        margin_px = self._normalize_vertical_edge_margin(margin_px)
        changed = margin_px != self._vertical_edge_margin_px
        self._vertical_edge_margin_px = margin_px
        if sync_ui:
            self._sync_vertical_edge_margin_spin()
        if reflow and changed:
            self._reflow_canvas_layout()
        if save_config:
            self._save_vertical_edge_margin_to_config()
            self._vertical_edge_margin_last_committed = margin_px
            self._vertical_edge_margin_dirty = False
        if broadcast and self.model:
            self.model.set_vertical_edge_margin(margin_px, self.uid)
        elif save_config and not self.model:
            self._autosave_timer.start()

    def _apply_load_all_bubbles_enabled(
        self,
        enabled: bool,
        *,
        sync_ui: bool,
        save_config: bool,
    ) -> None:
        enabled = bool(enabled)
        changed = enabled != self._load_all_bubbles_enabled
        self._load_all_bubbles_enabled = enabled
        if sync_ui:
            self._sync_load_all_bubbles_checkbox()
        if changed:
            self._refresh_visible_bubbles(force_repack=False)
        if save_config:
            self._save_load_all_bubbles_to_config()
            self._load_all_bubbles_last_committed = enabled
            self._load_all_bubbles_dirty = False
            if not self.model:
                self._autosave_timer.start()

    def _apply_visible_page_radius(
        self,
        radius: int,
        *,
        sync_ui: bool,
        save_config: bool,
        broadcast: bool,
    ) -> None:
        radius = self._normalize_visible_page_radius(radius)
        changed = radius != self._visible_page_radius
        self._visible_page_radius = radius
        if sync_ui:
            self._sync_visible_page_radius_spin()
        if changed:
            self._refresh_visible_bubbles(force_repack=False)
        if save_config:
            self._save_visible_page_radius_to_config()
            self._visible_page_radius_last_committed = radius
            self._visible_page_radius_dirty = False
        if broadcast and self.model:
            self.model.set_visible_page_radius(radius, self.uid)
        elif save_config and not self.model:
            self._autosave_timer.start()

    def _apply_bubble_load_delay_ms(
        self,
        delay_ms: int,
        *,
        sync_ui: bool,
        save_config: bool,
        broadcast: bool,
    ) -> None:
        delay_ms = self._normalize_bubble_load_delay_ms(delay_ms)
        changed = delay_ms != self._bubble_load_delay_ms
        self._bubble_load_delay_ms = delay_ms
        self._scroll_bubble_refresh_timer.setInterval(int(delay_ms))
        if sync_ui:
            self._sync_bubble_load_delay_spin()
        if save_config:
            self._save_bubble_load_delay_to_config()
            self._bubble_load_delay_last_committed = delay_ms
            self._bubble_load_delay_dirty = False
        if broadcast and self.model:
            self.model.set_bubble_load_delay_ms(delay_ms, self.uid)
        elif save_config and not self.model:
            self._autosave_timer.start()
        elif changed:
            # no-op branch for symmetry/readability
            pass

    def _on_aside_width_spin_value_changed(self, _value: int) -> None:
        if self._aside_width_spin_sync:
            return
        min_px = int(self._asideMinWidthSpin.value())
        max_px = int(self._asideMaxWidthSpin.value())
        if min_px > max_px:
            if self.sender() is self._asideMinWidthSpin:
                max_px = min_px
            else:
                min_px = max_px
        min_px, max_px = self._normalize_aside_width_limits(min_px, max_px)
        self._aside_width_spin_sync = True
        self._asideMinWidthSpin.blockSignals(True)
        self._asideMaxWidthSpin.blockSignals(True)
        self._asideMinWidthSpin.setValue(min_px)
        self._asideMaxWidthSpin.setValue(max_px)
        self._asideMinWidthSpin.blockSignals(False)
        self._asideMaxWidthSpin.blockSignals(False)
        self._aside_width_spin_sync = False
        self._aside_width_dirty = (min_px, max_px) != self._aside_last_committed
        self._update_canvas_settings_apply_button_state()

    def _on_aside_bubble_scale_spin_value_changed(self, _value: int) -> None:
        if self._aside_bubble_scale_spin_sync:
            return
        scale_pct = self._normalize_aside_bubble_scale_pct(int(self._asideBubbleScaleSpin.value()))
        self._aside_bubble_scale_spin_sync = True
        self._asideBubbleScaleSpin.blockSignals(True)
        self._asideBubbleScaleSpin.setValue(scale_pct)
        self._asideBubbleScaleSpin.blockSignals(False)
        self._aside_bubble_scale_spin_sync = False
        self._aside_bubble_scale_dirty = scale_pct != self._aside_bubble_scale_last_committed
        self._update_canvas_settings_apply_button_state()

    def _on_aside_bubble_scale_spin_editing_finished(self) -> None:
        self._on_aside_bubble_scale_spin_value_changed(0)

    def _on_aside_width_spin_editing_finished(self) -> None:
        self._on_aside_width_spin_value_changed(0)

    def _on_page_spacing_spin_value_changed(self, _value: int) -> None:
        if self._page_spacing_spin_sync:
            return
        spacing_px = self._normalize_page_spacing(int(self._pageSpacingSpin.value()))
        self._page_spacing_spin_sync = True
        self._pageSpacingSpin.blockSignals(True)
        self._pageSpacingSpin.setValue(spacing_px)
        self._pageSpacingSpin.blockSignals(False)
        self._page_spacing_spin_sync = False
        self._page_spacing_dirty = spacing_px != self._page_spacing_last_committed
        self._update_canvas_settings_apply_button_state()

    def _on_page_spacing_spin_editing_finished(self) -> None:
        self._on_page_spacing_spin_value_changed(0)

    def _on_separate_pages_checkbox_toggled(self, checked: bool) -> None:
        if self._separate_pages_checkbox_sync:
            return
        self._separate_pages_dirty = bool(checked) != self._separate_pages_last_committed
        if hasattr(self, "_pageSpacingSpin"):
            self._pageSpacingSpin.setEnabled(bool(checked))
        if hasattr(self, "_pageSpacingLabel"):
            self._pageSpacingLabel.setEnabled(bool(checked))
        self._update_canvas_settings_apply_button_state()

    def _on_vertical_edge_margin_spin_value_changed(self, _value: int) -> None:
        if self._vertical_edge_margin_spin_sync:
            return
        margin_px = self._normalize_vertical_edge_margin(int(self._verticalEdgeMarginSpin.value()))
        self._vertical_edge_margin_spin_sync = True
        self._verticalEdgeMarginSpin.blockSignals(True)
        self._verticalEdgeMarginSpin.setValue(margin_px)
        self._verticalEdgeMarginSpin.blockSignals(False)
        self._vertical_edge_margin_spin_sync = False
        self._vertical_edge_margin_dirty = margin_px != self._vertical_edge_margin_last_committed
        self._update_canvas_settings_apply_button_state()

    def _on_vertical_edge_margin_spin_editing_finished(self) -> None:
        self._on_vertical_edge_margin_spin_value_changed(0)

    def _on_load_all_bubbles_checkbox_toggled(self, checked: bool) -> None:
        if self._load_all_bubbles_checkbox_sync:
            return
        self._load_all_bubbles_dirty = bool(checked) != self._load_all_bubbles_last_committed
        radius_enabled = not bool(checked)
        if hasattr(self, "_visiblePageRadiusSpin"):
            self._visiblePageRadiusSpin.setEnabled(radius_enabled)
        if hasattr(self, "_visiblePageRadiusLabel"):
            self._visiblePageRadiusLabel.setEnabled(radius_enabled)
        self._update_canvas_settings_apply_button_state()

    def _on_visible_page_radius_spin_value_changed(self, _value: int) -> None:
        if self._visible_page_radius_spin_sync:
            return
        radius = self._normalize_visible_page_radius(int(self._visiblePageRadiusSpin.value()))
        self._visible_page_radius_spin_sync = True
        self._visiblePageRadiusSpin.blockSignals(True)
        self._visiblePageRadiusSpin.setValue(radius)
        self._visiblePageRadiusSpin.blockSignals(False)
        self._visible_page_radius_spin_sync = False
        self._visible_page_radius_dirty = radius != self._visible_page_radius_last_committed
        self._update_canvas_settings_apply_button_state()

    def _on_visible_page_radius_spin_editing_finished(self) -> None:
        self._on_visible_page_radius_spin_value_changed(0)

    def _on_bubble_load_delay_spin_value_changed(self, _value: int) -> None:
        if self._bubble_load_delay_spin_sync:
            return
        delay_ms = self._normalize_bubble_load_delay_ms(int(self._bubbleLoadDelaySpin.value()))
        self._bubble_load_delay_spin_sync = True
        self._bubbleLoadDelaySpin.blockSignals(True)
        self._bubbleLoadDelaySpin.setValue(delay_ms)
        self._bubbleLoadDelaySpin.blockSignals(False)
        self._bubble_load_delay_spin_sync = False
        self._bubble_load_delay_dirty = delay_ms != self._bubble_load_delay_last_committed
        self._update_canvas_settings_apply_button_state()

    def _on_bubble_load_delay_spin_editing_finished(self) -> None:
        self._on_bubble_load_delay_spin_value_changed(0)

    def _on_tabs_autosync_checkbox_toggled(self, checked: bool) -> None:
        if self._tabs_autosync_checkbox_sync:
            return
        enabled = bool(checked)
        if enabled == self._tabs_autosync_last_committed:
            return
        self._apply_tabs_autosync_enabled(
            enabled,
            sync_ui=True,
            save_config=True,
            broadcast=True,
        )
        self._tabs_autosync_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_opengl_render_checkbox_toggled(self, checked: bool) -> None:
        if self._opengl_settings_ui_sync:
            return
        enabled = bool(checked)
        self._opengl_enabled_dirty = enabled != self._opengl_enabled_last_committed
        if hasattr(self, "_openglDeviceCombo"):
            self._openglDeviceCombo.setEnabled(enabled)
        if hasattr(self, "_openglDeviceLabel"):
            self._openglDeviceLabel.setEnabled(enabled)
        self._update_canvas_settings_apply_button_state()

    def _on_opengl_device_combo_changed(self) -> None:
        if self._opengl_settings_ui_sync:
            return
        if not hasattr(self, "_openglDeviceCombo"):
            return
        device = self._normalize_opengl_device(self._openglDeviceCombo.currentData())
        self._opengl_device_dirty = device != self._opengl_device_last_committed
        self._update_canvas_settings_apply_button_state()

    def _on_bubble_type_combo_changed(self):
        bt = self._bubbleTypeCombo.currentData()
        if not isinstance(bt, str):
            return
        bt = "on_top" if bt == "on_top" else "aside"
        self._bubble_type_dirty = bt != self._bubble_type_last_committed
        self._update_canvas_settings_apply_button_state()

    def _on_scale_bubbles_checkbox_toggled(self, checked: bool) -> None:
        if self._scale_bubbles_checkbox_sync:
            return
        self._scale_bubbles_dirty = bool(checked) != self._scale_bubbles_last_committed
        self._update_canvas_settings_apply_button_state()

    def _update_canvas_settings_apply_button_state(self) -> None:
        if not hasattr(self, "_canvasSettingsApplyButton"):
            return
        has_changes = bool(
            self._bubble_type_dirty
            or self._scale_bubbles_dirty
            or self._aside_bubble_scale_dirty
            or self._aside_width_dirty
            or self._page_spacing_dirty
            or self._separate_pages_dirty
            or self._vertical_edge_margin_dirty
            or self._load_all_bubbles_dirty
            or self._visible_page_radius_dirty
            or self._bubble_load_delay_dirty
            or self._tabs_autosync_dirty
            or self._opengl_enabled_dirty
            or self._opengl_device_dirty
        )
        self._canvasSettingsApplyButton.setEnabled(has_changes)

    def _on_canvas_settings_apply_clicked(self) -> None:
        bubble_type = self._bubbleTypeCombo.currentData()
        if not isinstance(bubble_type, str):
            bubble_type = self._bubble_type_last_committed
        bubble_type = "on_top" if bubble_type == "on_top" else "aside"
        scale_bubbles = bool(self._scaleBubblesCheckbox.isChecked())
        aside_bubble_scale_pct = self._normalize_aside_bubble_scale_pct(int(self._asideBubbleScaleSpin.value()))
        aside_min, aside_max = self._normalize_aside_width_limits(
            int(self._asideMinWidthSpin.value()),
            int(self._asideMaxWidthSpin.value()),
        )
        separate_pages = bool(self._separatePagesCheckbox.isChecked())
        page_spacing = self._normalize_page_spacing(int(self._pageSpacingSpin.value()))
        edge_margin = self._normalize_vertical_edge_margin(int(self._verticalEdgeMarginSpin.value()))
        load_all_bubbles = bool(self._loadAllBubblesCheckbox.isChecked())
        visible_page_radius = self._normalize_visible_page_radius(int(self._visiblePageRadiusSpin.value()))
        bubble_load_delay_ms = self._normalize_bubble_load_delay_ms(int(self._bubbleLoadDelaySpin.value()))
        tabs_autosync = bool(self._tabsAutosyncCheckbox.isChecked())
        opengl_enabled = bool(self._openglRenderCheckbox.isChecked())
        opengl_device = self._normalize_opengl_device(self._openglDeviceCombo.currentData())
        need_reflow_canvas = False
        need_reflow_aside = False

        if bubble_type != self._bubble_type_last_committed:
            self._set_bubble_type(bubble_type)
            self._bubble_type_last_committed = bubble_type
            self._bubble_type_dirty = False

        if scale_bubbles != self._scale_bubbles_last_committed:
            self._apply_scale_bubbles_enabled(
                scale_bubbles,
                sync_ui=True,
                save_config=True,
                broadcast=True,
                reflow=False,
            )
            need_reflow_aside = True

        if aside_bubble_scale_pct != self._aside_bubble_scale_last_committed:
            self._apply_aside_bubble_scale_pct(
                aside_bubble_scale_pct,
                sync_ui=True,
                save_config=True,
                reflow=False,
            )
            need_reflow_aside = True

        if (aside_min, aside_max) != self._aside_last_committed:
            self._apply_aside_width_limits(
                aside_min,
                aside_max,
                sync_ui=True,
                save_config=True,
                broadcast=True,
                reflow=False,
            )
            need_reflow_aside = True

        if separate_pages != self._separate_pages_last_committed:
            self._apply_separate_pages_enabled(
                separate_pages,
                sync_ui=True,
                save_config=True,
                broadcast=True,
                reflow=False,
            )
            need_reflow_canvas = True

        if page_spacing != self._page_spacing_last_committed:
            self._apply_page_spacing(
                page_spacing,
                sync_ui=True,
                save_config=True,
                broadcast=True,
                reflow=False,
            )
            need_reflow_canvas = True

        if edge_margin != self._vertical_edge_margin_last_committed:
            self._apply_vertical_edge_margin(
                edge_margin,
                sync_ui=True,
                save_config=True,
                broadcast=True,
                reflow=False,
            )
            need_reflow_canvas = True

        if load_all_bubbles != self._load_all_bubbles_last_committed:
            self._apply_load_all_bubbles_enabled(
                load_all_bubbles,
                sync_ui=True,
                save_config=True,
            )

        if visible_page_radius != self._visible_page_radius_last_committed:
            self._apply_visible_page_radius(
                visible_page_radius,
                sync_ui=True,
                save_config=True,
                broadcast=True,
            )

        if bubble_load_delay_ms != self._bubble_load_delay_last_committed:
            self._apply_bubble_load_delay_ms(
                bubble_load_delay_ms,
                sync_ui=True,
                save_config=True,
                broadcast=True,
            )

        if tabs_autosync != self._tabs_autosync_last_committed:
            self._apply_tabs_autosync_enabled(
                tabs_autosync,
                sync_ui=True,
                save_config=True,
                broadcast=True,
            )

        if (
            opengl_enabled != self._opengl_enabled_last_committed
            or opengl_device != self._opengl_device_last_committed
        ):
            self._apply_opengl_render_settings(
                opengl_enabled,
                opengl_device,
                sync_ui=True,
                save_config=True,
            )
            self._opengl_restart_required = True
            if hasattr(self, "_openglRestartLabel"):
                self._openglRestartLabel.setVisible(True)

        if need_reflow_canvas:
            self._reflow_canvas_layout()
        elif need_reflow_aside and self._bubble_type() != "on_top":
            self._reflow_aside_bubbles()

        self._update_canvas_settings_apply_button_state()

    def _set_bubble_type(self, bubble_type: str):
        bt = "on_top" if bubble_type == "on_top" else "aside"
        if bt == self._bubble_type():
            return
        if self.model:
            self.model.set_bubble_type(bt, self.uid)
        else:
            settings = getattr(self.project, "settings", None)
            if settings:
                try:
                    settings.canvas.bubble_type = bt
                except Exception:
                    pass
                try:
                    settings.bubble_type = bt
                except Exception:
                    pass
            self.project.bubble_type = bt
        self._rebuild_bubbles_for_type_change()
        self._layout_canvas_controls()

    def _on_model_bubble_type_changed(self, bubble_type: str, origin_uid: str):
        if not self._tabs_autosync_enabled:
            return
        if origin_uid == self.uid:
            return
        settings = getattr(self.project, "settings", None)
        if settings:
            try:
                settings.canvas.bubble_type = bubble_type
            except Exception:
                pass
            try:
                settings.bubble_type = bubble_type
            except Exception:
                pass
        self.project.bubble_type = bubble_type
        self._bubble_type_last_committed = "on_top" if bubble_type == "on_top" else "aside"
        self._bubble_type_dirty = False
        self._sync_bubble_type_combo()
        self._update_canvas_settings_apply_button_state()
        self._rebuild_bubbles_for_type_change()
        self._layout_canvas_controls()

    def _on_model_aside_width_limits_changed(self, min_px: int, max_px: int, origin_uid: str):
        if not self._tabs_autosync_enabled:
            return
        if origin_uid == self.uid:
            return
        self._apply_aside_width_limits(
            int(min_px),
            int(max_px),
            sync_ui=True,
            save_config=False,
            broadcast=False,
            reflow=True,
        )
        self._aside_last_committed = (self._aside_min_width_px, self._aside_max_width_px)
        self._aside_width_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_model_page_spacing_changed(self, spacing_px: int, origin_uid: str):
        if not self._tabs_autosync_enabled:
            return
        if origin_uid == self.uid:
            return
        self._apply_page_spacing(
            int(spacing_px),
            sync_ui=True,
            save_config=False,
            broadcast=False,
            reflow=True,
        )
        self._page_spacing_last_committed = self._page_spacing_px
        self._page_spacing_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_model_separate_pages_changed(self, enabled: bool, origin_uid: str):
        if not self._tabs_autosync_enabled:
            return
        if origin_uid == self.uid:
            return
        self._apply_separate_pages_enabled(
            bool(enabled),
            sync_ui=True,
            save_config=False,
            broadcast=False,
            reflow=True,
        )
        self._separate_pages_last_committed = self._separate_pages_enabled
        self._separate_pages_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_model_vertical_edge_margin_changed(self, margin_px: int, origin_uid: str):
        if not self._tabs_autosync_enabled:
            return
        if origin_uid == self.uid:
            return
        self._apply_vertical_edge_margin(
            int(margin_px),
            sync_ui=True,
            save_config=False,
            broadcast=False,
            reflow=True,
        )
        self._vertical_edge_margin_last_committed = self._vertical_edge_margin_px
        self._vertical_edge_margin_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_model_scale_bubbles_changed(self, enabled: bool, origin_uid: str):
        if not self._tabs_autosync_enabled:
            return
        if origin_uid == self.uid:
            return
        self._apply_scale_bubbles_enabled(
            bool(enabled),
            sync_ui=True,
            save_config=False,
            broadcast=False,
            reflow=True,
        )
        self._scale_bubbles_last_committed = self._scale_bubbles_enabled
        self._scale_bubbles_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_model_visible_page_radius_changed(self, radius: int, origin_uid: str):
        if not self._tabs_autosync_enabled:
            return
        if origin_uid == self.uid:
            return
        self._apply_visible_page_radius(
            int(radius),
            sync_ui=True,
            save_config=False,
            broadcast=False,
        )
        self._visible_page_radius_last_committed = self._visible_page_radius
        self._visible_page_radius_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_model_bubble_load_delay_changed(self, delay_ms: int, origin_uid: str):
        if not self._tabs_autosync_enabled:
            return
        if origin_uid == self.uid:
            return
        self._apply_bubble_load_delay_ms(
            int(delay_ms),
            sync_ui=True,
            save_config=False,
            broadcast=False,
        )
        self._bubble_load_delay_last_committed = self._bubble_load_delay_ms
        self._bubble_load_delay_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_model_tabs_autosync_changed(self, enabled: bool, origin_uid: str):
        if origin_uid == self.uid:
            return
        self._apply_tabs_autosync_enabled(
            bool(enabled),
            sync_ui=True,
            save_config=True,
            broadcast=False,
        )
        self._tabs_autosync_last_committed = self._tabs_autosync_enabled
        self._tabs_autosync_dirty = False
        self._update_canvas_settings_apply_button_state()

    def _on_model_tabs_sync_requested(self, origin_uid: str):
        if origin_uid == self.uid:
            return
        self._sync_tabs_from_models()

    def _rebuild_bubbles_for_type_change(self):
        prev_visible = self.bubbles_visible
        prev_selected = self.selected_bubble
        for b in list(self.bubbles.values()):
            self._teardown_bubble_graphics(b)
        for b in list(self._bubble_cache.values()):
            self._teardown_bubble_graphics(b)
        self._bubble_cache.clear()
        self.bubbles.clear()
        self.selected_bubble = None
        self._load_bubbles_from_project()
        self._set_bubbles_visible(prev_visible)
        if prev_selected in self.bubbles:
            self._set_selected_bubble(prev_selected)
        if self._bubble_type() != "on_top":
            for i in self._visible_page_indexes():
                self._repack_bubbles_for(i, "left")
                self._repack_bubbles_for(i, "right")
        self._update_page_counter()

    def _on_bubbles_checkbox(self, checked: bool):
        self._set_bubbles_visible(bool(checked))

    def _set_bubbles_visible(self, visible: bool):
        self.bubbles_visible = bool(visible)
        for b in self.bubbles.values():
            self._set_bubble_runtime_visible(b, True)
            if self.bubbles_visible and self._bubble_type() == "on_top":
                self._layout_on_top_bubble(b.id)
        if hasattr(self, "_bubblesCheckbox"):
            self._bubblesCheckbox.blockSignals(True)
            self._bubblesCheckbox.setChecked(self.bubbles_visible)
            self._bubblesCheckbox.blockSignals(False)

    def _visible_page_indexes(self, center_idx: Optional[int] = None) -> List[int]:
        total = len(self.image_bboxes)
        if total <= 0:
            return []
        if self._load_all_bubbles_enabled:
            return list(range(total))
        if center_idx is None:
            center_idx = self._current_page_idx()
        center_idx = max(0, min(int(center_idx), total - 1))
        start = max(0, center_idx - int(self._visible_page_radius))
        end = min(total - 1, center_idx + int(self._visible_page_radius))
        return list(range(start, end + 1))

    def _is_page_in_active_window(self, img_idx: int) -> bool:
        return int(img_idx) in self._active_page_window

    def _refresh_visible_bubbles(self, *, center_idx: Optional[int] = None, force_repack: bool = True) -> None:
        target_pages = set(self._visible_page_indexes(center_idx=center_idx))
        if target_pages == self._active_page_window and not force_repack:
            return
        self._active_page_window = target_pages

        target_records: Dict[int, dict] = {}
        page_index = self._project_bubbles_index()
        for img_idx in target_pages:
            for bid, rec in page_index.get(int(img_idx), {}).items():
                target_records[bid] = rec

        mounted_ids = set(self.bubbles.keys())
        target_ids = set(target_records.keys())

        for bid in mounted_ids - target_ids:
            if self.selected_bubble == bid:
                self._set_selected_bubble(None)
            if self._move_active_bid == bid:
                self._move_active_bid = None
            b = self.bubbles.pop(bid, None)
            if b:
                self._set_bubble_runtime_visible(b, False)
                self._bubble_cache[bid] = b

        pages_to_repack: set[tuple[int, str]] = set()
        for bid in target_ids - mounted_ids:
            rec = target_records.get(bid)
            if rec:
                cached = self._bubble_cache.pop(bid, None)
                if cached:
                    self.bubbles[bid] = cached
                    self._restore_cached_bubble(rec, cached)
                    if self._bubble_type() != "on_top":
                        try:
                            pages_to_repack.add((int(rec.get("img_idx")), "left" if str(rec.get("side")) == "left" else "right"))
                        except Exception:
                            pass
                else:
                    self._create_bubble_widget(rec, repack_on_init=False)
                    if self._bubble_type() != "on_top":
                        try:
                            pages_to_repack.add((int(rec.get("img_idx")), "left" if str(rec.get("side")) == "left" else "right"))
                        except Exception:
                            pass

        self._set_bubbles_visible(self.bubbles_visible)

        if force_repack and self._bubble_type() != "on_top":
            for img_idx in sorted(target_pages):
                self._repack_bubbles_for(img_idx, "left")
                self._repack_bubbles_for(img_idx, "right")
        elif self._bubble_type() != "on_top":
            for img_idx, side in pages_to_repack:
                self._schedule_repack(img_idx, side)

    def _restore_cached_bubble(self, rec: dict, b: "BubbleRuntime") -> None:
        """Лёгкое восстановление parked-пузыря без тяжёлого _apply_model_update."""
        try:
            if b.original_text_widget and "original_text" in rec:
                new_original = rec.get("original_text", "")
                if b.original_text_widget.toPlainText() != new_original:
                    b.original_text_widget.blockSignals(True)
                    b.original_text_widget.setPlainText(new_original)
                    b.original_text_widget.blockSignals(False)
            if b.text_widget and "text" in rec:
                new_text = rec.get("text", "")
                if b.text_widget.toPlainText() != new_text:
                    b.text_widget.blockSignals(True)
                    b.text_widget.setPlainText(new_text)
                    b.text_widget.blockSignals(False)
        except Exception:
            traceback.print_exc()

        coords = None
        if "rect_coords" in rec:
            coords = self._normalize_rect_coords(rec.get("rect_coords"))
        if coords:
            b.rect_coords = coords

        if all(k in rec for k in ("img_idx", "img_u", "img_v", "side")) and not self._is_unplaced(rec):
            self._apply_bubble_imgpos(
                b.id,
                rec["img_idx"],
                rec["img_u"],
                rec["img_v"],
                rec["side"],
                broadcast=False,
                repack=False,
            )
        self._update_bubble_rect_visual(b.id)
        self._set_bubble_runtime_visible(b, True)

    def _on_bubble_opacity_changed(self, value: int):
        self._set_bubble_opacity(float(value) / 100.0)

    def _set_bubble_opacity(self, value: float):
        self._bubble_opacity = max(0.0, min(float(value), 1.0))
        for bid in self.bubbles.keys():
            self._apply_bubble_opacity(bid)

    def _bubble_display_opacity(self, bid: int, *, force: Optional[float] = None) -> float:
        if force is not None:
            return max(0.0, min(float(force), 1.0))
        if self._bubble_has_focus(bid):
            return 1.0
        return self._bubble_opacity

    def _apply_bubble_opacity(self, bid: int, *, force: Optional[float] = None) -> None:
        b = self.bubbles.get(bid)
        if not b:
            return
        opacity = self._bubble_display_opacity(bid, force=force)
        for item in (b.line_item, b.proxy_widget, b.header_proxy, b.original_proxy, b.footer_proxy):
            if item:
                item.setOpacity(opacity)

    def _refresh_bubble_opacity(self, bid: int) -> None:
        self._apply_bubble_opacity(bid)

    def _toggle_canvas_settings_panel(self):
        self._canvasSettingsPanel.setVisible(not self._canvasSettingsPanel.isVisible())
        if self._canvasSettingsPanel.isVisible():
            self._sync_tabs_autosync_checkbox()
            self._canvasSettingsPanel.raise_()
            self._sync_bubble_type_combo()
            self._sync_scale_bubbles_checkbox()
            self._sync_aside_bubble_scale_spin()
            self._sync_separate_pages_checkbox()
            self._sync_load_all_bubbles_checkbox()
            self._sync_visible_page_radius_spin()
            self._sync_bubble_load_delay_spin()
            self._sync_opengl_render_controls()
            if hasattr(self, "_openglRestartLabel"):
                self._openglRestartLabel.setVisible(bool(self._opengl_restart_required))
            self._update_canvas_settings_apply_button_state()
        self._layout_canvas_controls()

    def _close_canvas_settings_panel(self):
        self._canvasSettingsPanel.setVisible(False)

    def _on_delete_selected(self):
        if self.selected_bubble is not None:
            self.delete_bubble_by_id(self.selected_bubble)

    def _text_doc_height(self, te: QTextEdit, width: int, visual_lines_hint: Optional[int] = None) -> int:
        """
        Возвращает высоту QTextEdit с учётом переносов на заданной ширине.
        Если передан visual_lines_hint — поддерживаем минимум N «визуальных строк» (55 сим/стр).
        """
        if width <= 0:
            width = 1

        # гарантируем режим переноса для самого QTextEdit
        te.setWordWrapMode(QTextOption.WrapMode.WrapAtWordBoundaryOrAnywhere)

        doc = te.document()
        doc.setDocumentMargin(2)
        # ширина области верстки документа = ширина виджета минус рамки
        doc.setTextWidth(max(1.0, float(width - 2 * te.frameWidth())))

        # ВАЖНО: создаём QTextOption без аргументов и задаём wrapMode отдельно
        opt = QTextOption()
        opt.setWrapMode(QTextOption.WrapMode.WrapAtWordBoundaryOrAnywhere)
        doc.setDefaultTextOption(opt)

        doc_sz = doc.size().toSize()
        content_h = doc_sz.height()

        if visual_lines_hint is not None:
            fm = QFontMetrics(te.font())
            line_h = fm.height()
            min_h = visual_lines_hint * line_h + 8
            content_h = max(content_h, min_h)

        h = int(content_h + 2 * te.frameWidth())
        return max(1, h)


    def _visual_lines_55(self, text: str) -> int:
        """Сколько 'визуальных' строк по правилу: каждые 55 символов — это 1 строка."""
        total = 0
        for line in text.split("\n"):
            total += max(1, (len(line) + 54) // 55)
        return max(1, total)

    # ------------------- изображения -------------------
    def _sort_images_numeric_first(self):
        """
        Сортируем строго по числу в basename (без расширения):
        0.png, 1.png, 2.png, ...
        Если имя не является числом — такие файлы идут после числовых,
        лексикографически. При одинаковом номере приоритет у .png.
        """
        def ext_weight(ext: str) -> int:
            ext = ext.lower().lstrip(".")
            if ext == "png":
                return 0
            if ext in ("jpg", "jpeg"):
                return 1
            return 2

        def keyfn(item):
            # Ожидаем пути-строки; другие типы ставим в конец, сохраняя порядок
            if not isinstance(item, str):
                return (2, 0, 0, "")
            base = os.path.basename(item)
            stem, ext = os.path.splitext(base)
            if stem.isdigit():
                return (0, int(stem), ext_weight(ext), base.lower())
            # нечисловые имена — после числовых
            return (1, stem.lower(), ext_weight(ext), base.lower())

        self.images.sort(key=keyfn)

    def _qimage_from(self, item: ImageLike) -> QImage:
        if isinstance(item, QImage):
            return item
        if isinstance(item, QPixmap):
            return item.toImage()
        if not isinstance(item, str):
            return QImage()
        return self._cached_image_for_path(item)

    def _pixmap_from(self, item: ImageLike) -> QPixmap:
        if isinstance(item, QPixmap):
            return item
        if isinstance(item, QImage):
            key = int(item.cacheKey())
            cached = self._pixmap_cache_by_qimage_key.get(key)
            if cached is not None and not cached.isNull():
                return cached
            pix = QPixmap.fromImage(item)
            self._pixmap_cache_by_qimage_key[key] = pix
            return pix
        if not isinstance(item, str):
            return QPixmap()

        path = os.path.abspath(item)
        try:
            st = os.stat(path)
            stamp_mtime_ns = int(st.st_mtime_ns)
            stamp_size = int(st.st_size)
        except OSError:
            return QPixmap.fromImage(self._cached_image_for_path(path))

        cached = self._pixmap_cache_by_path.get(path)
        if cached is not None:
            c_mtime_ns, c_size, c_pix = cached
            if c_mtime_ns == stamp_mtime_ns and c_size == stamp_size and not c_pix.isNull():
                return c_pix

        qimg = self._cached_image_for_path(path)
        if qimg.isNull():
            return QPixmap()
        pix = QPixmap.fromImage(qimg)
        self._pixmap_cache_by_path[path] = (stamp_mtime_ns, stamp_size, pix)
        return pix

    def _display_images(self):
        # раньше мы подгоняли под target_w; теперь кладём пиксмапы в нативном размере
        # НЕ очищаем всю сцену — иначе потеряем пузыри и их кнопки

        # Создаём сцену с запасом по ширине для масштабов 0.5-1.0
        # При масштабе 0.5 видимая область сцены в 2 раза шире viewport
        # Делаем сцену шириной = viewport / 0.5 = viewport * 2
        viewport_width = max(1, self.viewport().width())
        min_scale = MIN_CANVAS_SCALE  # минимальный масштаб, при котором не будет смещения
        scene_width = viewport_width / min_scale
        x_center = scene_width / 2.0

        new_pixmaps: List[QPixmap] = []

        edge_margin = float(getattr(self, "_vertical_edge_margin_px", 200))
        y_off = edge_margin


        # Конвертация QImage->QPixmap кэшируется, поэтому повторные reflow дешёвые.
        for src in self.images:
            pix = self._pixmap_from(src)
            if pix.isNull():
                new_pixmaps.append(QPixmap())  # placeholder
            else:
                new_pixmaps.append(pix)
        # добавим недостающие items, удалим лишние, а существующие — обновим
        # добавить
        while len(self.image_items) < len(new_pixmaps):
            it = QGraphicsPixmapItem()
            cache_mode = (
                QGraphicsItem.CacheMode.DeviceCoordinateCache
                if self._gpu_viewport_active
                else QGraphicsItem.CacheMode.NoCache
            )
            it.setCacheMode(cache_mode)
            # Явно говорим, как интерполировать при view-скейле:
            #   - SmoothTransformation — сглаженно (без «лестницы пикселей»)
            #   - FastTransformation — ближний сосед (даёт «крупные пиксели»)
            # Вы жаловались на «пиксельные» — включаем сглаживание:
            try:
                it.setTransformationMode(Qt.TransformationMode.SmoothTransformation)  # Qt 5/6: у QGraphicsPixmapItem есть такое свойство
            except Exception:
                pass 

            self.scene.addItem(it)
            self.image_items.append(it)
            self.image_bboxes.append(QRectF())  # заполним ниже

        # удалить лишние
        while len(self.image_items) > len(new_pixmaps):
            it = self.image_items.pop()
            self.scene.removeItem(it)
            self.image_bboxes.pop()

        # обновить все
        page_spacing = float(self._effective_page_spacing_px())
        self._page_tops = []
        self._page_bottoms = []
        for i, pix in enumerate(new_pixmaps):
            if i > 0 and page_spacing > 0.0:
                y_off += page_spacing
            it = self.image_items[i]
            it.setPixmap(pix)
            w = pix.width(); h = pix.height()
            if w <= 0 or h <= 0:
                # пустышки просто пропускаем, но сохраняем вертикальный оффсет
                bbox = QRectF(x_center, y_off, 1, 1)
                it.setPos(bbox.left(), bbox.top())
                self.image_bboxes[i] = bbox
                self._page_tops.append(float(y_off))
                self._page_bottoms.append(float(y_off + 1.0))
                y_off += bbox.height()
                continue

            x_left = x_center - w / 2
            it.setPos(x_left, y_off)
            self.image_bboxes[i] = QRectF(x_left, y_off, w, h)
            self._page_tops.append(float(y_off))
            self._page_bottoms.append(float(y_off + h))
            y_off += h

        if new_pixmaps:
            y_off += edge_margin

        # обновим размеры сцены (используем расширенную ширину для поддержки масштаба 0.5)
        self.scene.setSceneRect(QRectF(0, 0, max(scene_width, 1), y_off))

        # счётчик страниц после layout
        QTimer.singleShot(0, self._update_page_counter)


    def resizeEvent(self, e):
        super().resizeEvent(e)
        self._sync_hotkeys_label()
        self._layout_top_labels()
        self._resize_debounce.start()

    def showEvent(self, e):
        super().showEvent(e)
        # Первый layout вкладки/окна может происходить после стартового расчёта пузырей.
        # Делаем одноразовый reflow уже по реальной геометрии viewport.
        if not self._post_show_reflow_done:
            self._post_show_reflow_done = True
            QTimer.singleShot(0, self._reflow_after_resize)
            QTimer.singleShot(80, self._reflow_after_resize)

    def _reflow_after_resize(self):
        if not self.image_items:
            return
        self._reflow_canvas_layout()
        self._center_view_on_scene()

    # ------------------- счётчик страниц -------------------
    def _current_page_idx(self) -> int:
        if not self._page_tops or not self._page_bottoms:
            return 0
        vy0 = self.mapToScene(0, 0).y()
        vy1 = self.mapToScene(0, self.viewport().height()).y()
        mid = (vy0 + vy1) / 2.0
        i = bisect.bisect_right(self._page_tops, float(mid)) - 1
        if i < 0:
            return 0
        last = len(self._page_tops) - 1
        if i >= last:
            return last
        if self._page_tops[i] <= mid <= self._page_bottoms[i]:
            return i
        ni = i + 1
        d_cur = min(abs(mid - self._page_tops[i]), abs(mid - self._page_bottoms[i]))
        d_next = min(abs(mid - self._page_tops[ni]), abs(mid - self._page_bottoms[ni]))
        return i if d_cur <= d_next else ni

    def _update_page_counter(self, cur_idx: Optional[int] = None):
        total = len(self.image_bboxes) or len(self.images)
        if cur_idx is None:
            cur_idx = self._current_page_idx()
        cur = min(max(1, int(cur_idx) + 1), max(1, total))
        new_text = f"{cur} / {total}"
        if self._pageLabel.text() != new_text:
            self._pageLabel.setText(new_text)
            self._layout_top_labels()
        return cur - 1

    def _on_scroll_value_changed(self, _value: int) -> None:
        cur_page = self._current_page_idx()
        self._update_page_counter(cur_page)
        self._scroll_pending_center_idx = cur_page
        self._set_scroll_render_quality(True)
        self._scroll_quality_restore_timer.start()
        self._scroll_bubble_refresh_timer.start()

    def _flush_scroll_bubble_refresh(self) -> None:
        center_idx = self._scroll_pending_center_idx
        self._scroll_pending_center_idx = None
        self._refresh_visible_bubbles(center_idx=center_idx, force_repack=False)
        if self.overlays_model:
            self._refresh_all_overlays_pixmaps()

    def _sync_hotkeys_label(self):
        self._hotkeysLabel.adjustSize()
        self._hotkeysLabel.move(
            (self.viewport().width() - self._hotkeysLabel.width()) // 2,
            self.viewport().height() - self._hotkeysLabel.height() - 8
        )

    def _center_view_on_scene(self):
        """Выравнивает view по горизонтальному центру сцены."""
        scene_rect = self.scene.sceneRect()
        scene_center_x = scene_rect.center().x()
        # Центрируем view на горизонтальном центре сцены, сохраняя вертикальную позицию
        self.centerOn(scene_center_x, self.mapToScene(0, 0).y())

    # ------------------- зум -------------------
    def _set_canvas_scale(self, value: float):

        # ограничиваем фактор (можно уменьшать до 0.5 без смещения)
        new_scale = max(MIN_CANVAS_SCALE, min(float(value), MAX_CANVAS_SCALE))
        new_scale = round(new_scale + 1e-9, 2)
        if abs(new_scale - getattr(self, "canvas_scale", 1.0)) < 1e-6:
            return
        self.canvas_scale = new_scale

        # 1) запоминаем текущий центр экрана в координатах сцены
        vp = self.viewport().rect()
        scene_center_before = self.mapToScene(vp.center())
        scene_center_x = self.scene.sceneRect().center().x()

        # 2) применяем новую матрицу без пересоздания картинок
        self.resetTransform()
        self.scale(self.canvas_scale, self.canvas_scale)

        # 3) восстанавливаем вертикальную позицию, горизонтально держим по центру ленты
        self.centerOn(scene_center_x, scene_center_before.y())

        # 4) Для aside всегда пересчитываем позиции при зуме:
        #    even при фиксированной ширине пузырей (scale_bubbles=False) их scene-координаты
        #    зависят от текущего S (отступы/ширина в сцене), иначе появляется дрейф.
        if self._bubble_type() != "on_top":
            for bid, b in list(self.bubbles.items()):
                if 0 <= b.img_idx < len(self.image_bboxes):
                    self._apply_bubble_imgpos(
                        bid, b.img_idx, b.img_u, b.img_v, b.side, broadcast=False, repack=False
                    )
            for i in self._visible_page_indexes():
                self._repack_bubbles_for(i, "left")
                self._repack_bubbles_for(i, "right")
        self._update_page_counter()
        self._update_scale_indicator()
        self._layout_top_labels()

    def _zoom_canvas(self, factor: float):
        self._set_canvas_scale(self.canvas_scale * float(factor))

    # ------------------- u,v ↔ scene -------------------
    @staticmethod
    def _clip01(v: float) -> float:
        return 0.0 if v < 0.0 else (1.0 if v > 1.0 else v)

    def _view_scene_scale(self) -> float:
        return float(self.transform().m11() or self.canvas_scale or 1.0)

    def _snap_scene_value(self, value: float) -> float:
        # Привязываем scene-координаты к пиксельной сетке экрана:
        # это убирает «плавание» толщины/мерцание линий в OpenGL при зуме.
        s = max(1e-6, abs(self._view_scene_scale()))
        return round(float(value) * s) / s

    def _snap_scene_point(self, x: float, y: float) -> Tuple[float, float]:
        return self._snap_scene_value(x), self._snap_scene_value(y)

    def _bubble_type(self) -> str:
        bt = None
        try:
            settings = getattr(self.project, "settings", None)
            if settings:
                canvas_settings = getattr(settings, "canvas", None)
                bt = getattr(canvas_settings, "bubble_type", None) if canvas_settings else None
                if not bt:
                    bt = getattr(settings, "bubble_type", None)
        except Exception:
            bt = None
        bt = bt or getattr(self.project, "bubble_type", None) or "aside"
        if bt == "side":
            bt = "aside"
        return bt

    def _bubble_is_active(self, bid: int) -> bool:
        return self.selected_bubble == bid or self._bubble_has_focus(bid)

    def _scene_from_uv(self, img_idx: int, u: float, v: float) -> Tuple[float, float]:
        r = self.image_bboxes[img_idx]
        x = r.left() + self._clip01(u) * r.width()
        y = r.top() + self._clip01(v) * r.height()
        return x, y

    def _uv_from_scene(self, img_idx: int, x: float, y: float) -> Tuple[float, float]:
        r = self.image_bboxes[img_idx]
        w, h = max(1.0, r.width()), max(1.0, r.height())
        u = self._clip01((x - r.left()) / w)
        v = self._clip01((y - r.top()) / h)
        return u, v
    
    def _default_rect_coords(self, img_idx: int, u: float, v: float) -> Dict[str, Dict[str, float]]:
        r = self.image_bboxes[img_idx]
        du = 100.0 / max(1.0, float(r.width()))
        dv = 100.0 / max(1.0, float(r.height()))
        p1u = self._clip01(u - du)
        p1v = self._clip01(v - dv)
        p2u = self._clip01(u + du)
        p2v = self._clip01(v + dv)
        return {
            'p1': {'img_u': p1u, 'img_v': p1v},
            'p2': {'img_u': p2u, 'img_v': p2v},
        }

    def _normalize_rect_coords(self, coords: Optional[dict]) -> Optional[Dict[str, Dict[str, float]]]:
        if not coords or not isinstance(coords, dict):
            return None
        p1 = coords.get('p1')
        p2 = coords.get('p2')
        if not isinstance(p1, dict) or not isinstance(p2, dict):
            return None
        try:
            u1 = float(p1.get('img_u', 0.0))
            v1 = float(p1.get('img_v', 0.0))
            u2 = float(p2.get('img_u', 0.0))
            v2 = float(p2.get('img_v', 0.0))
        except Exception:
            return None
        u1 = self._clip01(u1); v1 = self._clip01(v1)
        u2 = self._clip01(u2); v2 = self._clip01(v2)
        return {
            'p1': {'img_u': min(u1, u2), 'img_v': min(v1, v2)},
            'p2': {'img_u': max(u1, u2), 'img_v': max(v1, v2)},
        }

    @staticmethod
    def _rect_center_uv(coords: Dict[str, Dict[str, float]]) -> Tuple[float, float]:
        p1 = coords.get('p1', {})
        p2 = coords.get('p2', {})
        u = (float(p1.get('img_u', 0.0)) + float(p2.get('img_u', 0.0))) / 2.0
        v = (float(p1.get('img_v', 0.0)) + float(p2.get('img_v', 0.0))) / 2.0
        return u, v

    def _ensure_rect_coords(self, rec: dict, img_idx: int, u: float, v: float) -> Dict[str, Dict[str, float]]:
        coords = self._normalize_rect_coords(rec.get('rect_coords'))
        if coords is None:
            coords = self._default_rect_coords(img_idx, u, v)
            rec['rect_coords'] = coords
        return coords

    def _set_selected_bubble(self, bid: Optional[int]) -> None:
        if self.selected_bubble == bid:
            return
        prev = self.selected_bubble
        self.selected_bubble = bid
        if prev is not None:
            self._set_rect_visible(prev, False)
            self._layout_on_top_bubble(prev)
        if bid is not None:
            self._set_rect_visible(bid, self._should_show_rect(bid))
            self._layout_on_top_bubble(bid)

    def _focus_in_widget(self, focus_widget: QWidget, root: Optional[QWidget]) -> bool:
        if not focus_widget or not root:
            return False
        if focus_widget is root:
            return True
        try:
            return root.isAncestorOf(focus_widget)
        except Exception:
            return False

    def _bubble_has_focus(self, bid: int) -> bool:
        b = self.bubbles.get(bid)
        if not b:
            return False
        if b.text_widget and b.text_widget.hasFocus():
            return True
        if b.original_text_widget and b.original_text_widget.hasFocus():
            return True
        if self._bubble_type() == "on_top" and self.selected_bubble == bid:
            return True
        fw = QGuiApplication.focusObject()
        if isinstance(fw, QWidget):
            if self._focus_in_widget(fw, b.container_widget):
                return True
            if self._focus_in_widget(fw, b.footer_widget):
                return True
            if self._focus_in_widget(fw, b.header_widget):
                return True
            if self._focus_in_widget(fw, b.original_container):
                return True
        return False

    def _is_bubble_item(self, item: Optional[QGraphicsItem]) -> bool:
        if item is None:
            return False
        cur = item
        while cur is not None:
            marker = cur.data(1)
            if isinstance(marker, tuple) and len(marker) == 2 and marker[0] == "bubble":
                return True
            data = cur.data(0)
            if isinstance(data, tuple) and len(data) == 3 and data[0] == "rect_handle":
                return True
            cur = cur.parentItem()
        return False

    def _clear_bubble_focus(self) -> None:
        try:
            fw = QGuiApplication.focusObject()
            if isinstance(fw, QTextEdit):
                fw.clearFocus()
        except Exception:
            pass

    def _should_show_rect(self, bid: int) -> bool:
        if not self.editable or not self.bubbles_visible:
            return False
        if self.selected_bubble != bid:
            return False
        if self._active_rect_handle and self._active_rect_handle[0] == bid:
            return True
        return self._bubble_has_focus(bid)

    def _refresh_rect_visibility(self, bid: int) -> None:
        self._set_rect_visible(bid, self._should_show_rect(bid))
        self._layout_on_top_bubble(bid)
        if self._bubble_type() != "on_top":
            self._refresh_bubble_opacity(bid)

    def _set_rect_visible(self, bid: int, visible: bool) -> None:
        b = self.bubbles.get(bid)
        if not b:
            return
        if b.rect_item:
            b.rect_item.setVisible(visible)
        if b.rect_item_inner:
            b.rect_item_inner.setVisible(visible)
        if b.rect_handles:
            for h in b.rect_handles:
                h.setVisible(visible)

    def _ensure_bubble_rect_items(self, b: BubbleRuntime) -> None:
        if b.rect_item and b.rect_item_inner and b.rect_handles:
            return
        if not self.editable:
            return
        if not b.rect_item:
            outer = QGraphicsRectItem()
            pen_outer = QPen(QColor(245, 245, 245))
            pen_outer.setWidthF(3.0)
            pen_outer.setCosmetic(False)
            outer.setPen(pen_outer)
            outer.setBrush(QBrush(Qt.BrushStyle.NoBrush))
            outer.setZValue(900.0)
            self.scene.addItem(outer)
            b.rect_item = outer
        if not b.rect_item_inner:
            inner = QGraphicsRectItem()
            pen_inner = QPen(QColor(0, 120, 215))
            pen_inner.setWidthF(1.0)
            pen_inner.setCosmetic(False)
            inner.setPen(pen_inner)
            inner.setBrush(QBrush(Qt.BrushStyle.NoBrush))
            inner.setZValue(901.0)
            self.scene.addItem(inner)
            b.rect_item_inner = inner
        if b.rect_handles is None:
            handles: List[QGraphicsEllipseItem] = []
            pen = QPen(QColor(0, 120, 215))
            pen.setWidth(1)
            pen.setCosmetic(True)
            brush = QBrush(QColor(255, 255, 255))
            for idx in range(8):
                h = QGraphicsEllipseItem()
                h.setPen(pen)
                h.setBrush(brush)
                h.setZValue(905.0)
                h.setFlag(QGraphicsItem.GraphicsItemFlag.ItemIgnoresTransformations, True)
                h.setData(0, ("rect_handle", b.id, idx))
                h.setAcceptedMouseButtons(Qt.MouseButton.LeftButton)
                handles.append(h)
                self.scene.addItem(h)
            cursors = [
                Qt.CursorShape.SizeFDiagCursor,
                Qt.CursorShape.SizeVerCursor,
                Qt.CursorShape.SizeBDiagCursor,
                Qt.CursorShape.SizeHorCursor,
                Qt.CursorShape.SizeFDiagCursor,
                Qt.CursorShape.SizeVerCursor,
                Qt.CursorShape.SizeBDiagCursor,
                Qt.CursorShape.SizeHorCursor,
            ]
            for h, c in zip(handles, cursors):
                h.setCursor(c)
            b.rect_handles = handles

    def _scene_rect_from_coords(self, img_idx: int, coords: Dict[str, Dict[str, float]]) -> QRectF:
        p1 = coords.get('p1', {})
        p2 = coords.get('p2', {})
        x1, y1 = self._scene_from_uv(img_idx, float(p1.get('img_u', 0.0)), float(p1.get('img_v', 0.0)))
        x2, y2 = self._scene_from_uv(img_idx, float(p2.get('img_u', 0.0)), float(p2.get('img_v', 0.0)))
        return QRectF(x1, y1, x2 - x1, y2 - y1).normalized()

    def _update_bubble_rect_visual(self, bid: int) -> None:
        b = self.bubbles.get(bid)
        if not b or not b.rect_coords:
            return
        if b.img_idx < 0 or b.img_idx >= len(self.image_bboxes):
            return
        self._ensure_bubble_rect_items(b)
        if not b.rect_item or not b.rect_item_inner or not b.rect_handles:
            return
        rect = self._scene_rect_from_coords(b.img_idx, b.rect_coords)
        # Для OpenGL cosmetic-pen часто мерцает/пропадает на дробных координатах.
        # Держим толщину как screen-px через пересчёт в scene-units.
        S = self._view_scene_scale()
        outer_pen = b.rect_item.pen()
        outer_pen.setCosmetic(False)
        outer_pen.setWidthF(max(0.6, 3.0 / max(1.0, S)))
        b.rect_item.setPen(outer_pen)
        inner_pen = b.rect_item_inner.pen()
        inner_pen.setCosmetic(False)
        inner_pen.setWidthF(max(0.4, 1.0 / max(1.0, S)))
        b.rect_item_inner.setPen(inner_pen)
        b.rect_item.setRect(rect)
        b.rect_item_inner.setRect(rect)
        if self._bubble_type() == "on_top":
            z = 1300.0 if self._bubble_is_active(bid) else 1100.0
            b.rect_item.setZValue(z)
            b.rect_item_inner.setZValue(z + 0.5)
            for h in b.rect_handles:
                h.setZValue(z + 1.0)
        cx = rect.center().x()
        cy = rect.center().y()
        pts = [
            (rect.left(), rect.top()),
            (cx, rect.top()),
            (rect.right(), rect.top()),
            (rect.right(), cy),
            (rect.right(), rect.bottom()),
            (cx, rect.bottom()),
            (rect.left(), rect.bottom()),
            (rect.left(), cy),
        ]
        r = float(self._rect_handle_radius)
        for h, (x, y) in zip(b.rect_handles, pts):
            h.setRect(x - r, y - r, r * 2.0, r * 2.0)
        self._set_rect_visible(bid, self._should_show_rect(bid))

    def _set_bubble_rect_coords(self, bid: int, coords: Dict[str, Dict[str, float]], *, update_model: bool) -> None:
        b = self.bubbles.get(bid)
        if not b:
            return
        norm = self._normalize_rect_coords(coords)
        if not norm:
            return
        b.rect_coords = norm
        rec = self._record_for_bid(int(bid))
        if rec is not None:
            rec['rect_coords'] = b.rect_coords
        center_u, center_v = self._rect_center_uv(b.rect_coords)
        self._apply_bubble_imgpos(
            bid,
            b.img_idx,
            center_u,
            center_v,
            b.side,
            broadcast=update_model,
            move_rect=False,
            repack=update_model,
        )
        if update_model and not self.model:
            self._autosave_timer.start()

    def _layout_on_top_bubble(self, bid: int) -> None:
        if self._bubble_type() != "on_top":
            return
        b = self.bubbles.get(bid)
        if not b or not b.rect_coords:
            return
        if b.img_idx < 0 or b.img_idx >= len(self.image_bboxes):
            return
        rect = self._scene_rect_from_coords(b.img_idx, b.rect_coords)
        if rect.isNull():
            return

        S = self._view_scene_scale()
        width_px = max(1, int(rect.width() * S))
        height_px = max(1, int(rect.height() * S))

        if b.container_widget and b.text_widget:
            b.container_widget.setFixedSize(width_px, height_px)
            b.text_widget.setFixedSize(width_px, height_px)
        if b.proxy_widget:
            b.proxy_widget.setScale(1.0)
            b.proxy_widget.resize(width_px, height_px)
            px, py = self._snap_scene_point(rect.left(), rect.top())
            b.proxy_widget.setPos(px, py)
            b.proxy_widget.setVisible(self.bubbles_visible)
        b.max_width = width_px
        b.height_px = height_px
        b.anchor_y = rect.center().y()

        if b.line_item:
            b.line_item.setVisible(False)

        if not self.bubbles_visible:
            if b.header_proxy:
                b.header_proxy.setVisible(False)
            if b.original_proxy:
                b.original_proxy.setVisible(False)
            if b.footer_proxy:
                b.footer_proxy.setVisible(False)
            return

        active = self.editable and self._bubble_is_active(bid)
        has_focus = self.editable and self._bubble_has_focus(bid)
        if has_focus:
            self._apply_bubble_opacity(bid, force=1.0)
        gap_sc = 20.0 / max(1.0, S)

        header_h_px = 0
        if b.header_widget and b.header_proxy:
            b.header_widget.setFixedWidth(width_px)
            b.header_widget.adjustSize()
            header_h_px = max(1, int(b.header_widget.sizeHint().height()))
            b.header_proxy.resize(width_px, header_h_px)

        header_h_sc = header_h_px / max(1.0, S) if header_h_px else 0.0

        orig_h_px = 0
        if b.original_container and b.original_text_widget and b.original_proxy:
            if has_focus:
                visual_hint = self._visual_lines_55(b.original_text_widget.toPlainText())
                orig_h_px = self._text_doc_height(b.original_text_widget, width_px, visual_hint)
                b.original_text_widget.setFixedHeight(orig_h_px)
                b.original_container.setFixedWidth(width_px)
                b.original_container.adjustSize()
                b.original_proxy.resize(width_px, orig_h_px)
                b.original_proxy.setVisible(True)
            else:
                b.original_proxy.setVisible(False)

        orig_h_sc = orig_h_px / max(1.0, S) if orig_h_px else 0.0

        if b.header_proxy:
            if has_focus and orig_h_px:
                header_y = rect.top() - gap_sc - orig_h_sc - header_h_sc
            else:
                header_y = rect.top() - header_h_sc
            hx, hy = self._snap_scene_point(rect.left(), header_y)
            b.header_proxy.setPos(hx, hy)
            b.header_proxy.setVisible(True)

        if b.original_proxy and has_focus and orig_h_px:
            orig_y = rect.top() - gap_sc - orig_h_sc
            ox, oy = self._snap_scene_point(rect.left(), orig_y)
            b.original_proxy.setPos(ox, oy)

        if b.footer_proxy and b.footer_widget:
            if has_focus and self.editable:
                footer_w_px = 580
                b.footer_widget.setFixedWidth(footer_w_px)
                b.footer_widget.adjustSize()
                footer_h_px = max(1, int(b.footer_widget.sizeHint().height()))
                b.footer_proxy.resize(footer_w_px, footer_h_px)
                b.footer_proxy.setVisible(True)

                width_sc = footer_w_px / max(1.0, S)
                center_x = rect.center().x()
                x = center_x - width_sc / 2.0
                vp_w = self.viewport().width()
                top_left_sc = self.mapToScene(0, 0)
                top_right_sc = self.mapToScene(max(0, vp_w - 1), 0)
                scene_left = min(top_left_sc.x(), top_right_sc.x())
                scene_right = max(top_left_sc.x(), top_right_sc.x())
                margin_sc = 4.0 / max(1.0, S)
                x = min(max(x, scene_left + margin_sc), scene_right - margin_sc - width_sc)
                y = rect.bottom() + gap_sc
                fx, fy = self._snap_scene_point(x, y)
                b.footer_proxy.setPos(fx, fy)
            else:
                b.footer_proxy.setVisible(False)

        if not has_focus:
            self._apply_bubble_opacity(bid)

        base_z = 1000.0
        z = 1200.0 if active else base_z
        if b.proxy_widget:
            b.proxy_widget.setZValue(z)
        if b.header_proxy:
            b.header_proxy.setZValue(z + 2.0)
        if b.original_proxy:
            b.original_proxy.setZValue(z + 1.0)
        if b.footer_proxy:
            b.footer_proxy.setZValue(z + 1.0)

    def _resize_rect_by_handle(self, bid: int, idx: int, x: float, y: float, *, update_model: bool) -> None:
        b = self.bubbles.get(bid)
        if not b or not b.rect_coords:
            return
        rect = self._scene_rect_from_coords(b.img_idx, b.rect_coords)
        if rect.isNull():
            return
        left = rect.left()
        right = rect.right()
        top = rect.top()
        bottom = rect.bottom()
        S = self._view_scene_scale()
        min_sc = 8.0 / max(1.0, S)
        if idx in (0, 6, 7):
            left = min(x, right - min_sc)
        if idx in (2, 3, 4):
            right = max(x, left + min_sc)
        if idx in (0, 1, 2):
            top = min(y, bottom - min_sc)
        if idx in (4, 5, 6):
            bottom = max(y, top + min_sc)
        rect = QRectF(left, top, right - left, bottom - top).normalized()
        u1, v1 = self._uv_from_scene(b.img_idx, rect.left(), rect.top())
        u2, v2 = self._uv_from_scene(b.img_idx, rect.right(), rect.bottom())
        coords = {
            'p1': {'img_u': min(u1, u2), 'img_v': min(v1, v2)},
            'p2': {'img_u': max(u1, u2), 'img_v': max(v1, v2)},
        }
        self._set_bubble_rect_coords(bid, coords, update_model=update_model)

    # ------------------- пузыри -------------------
    def toggle_bubbles(self):
        self._set_bubbles_visible(not self.bubbles_visible)

    def create_bubble(self, img_idx: int, x: float, y: float) -> int:
        bid = self.bubble_count + 1
        r = self.image_bboxes[img_idx]
        side = "left" if x < (r.left()+r.right())/2.0 else "right"
        u, v = self._uv_from_scene(img_idx, x, y)
        rect_coords = self._default_rect_coords(img_idx, u, v)
        rec = {
            'id': bid, 'img_idx': img_idx, 'img_u': float(u), 'img_v': float(v), 'side': side, 'text': '',
            'original_text': '',
            'translation_status': 'untranslated',  # 'untranslated' или 'translated'
            'bubble_order': 0, # Порядок реплик, если не сверху вниз
            'is_known_character': True,  # по умолчанию True
            'character_name': '',  # имя персонажа
            'rect_coords': rect_coords
        }

        # 1) сначала модель (разошлёт сигнал всем вкладкам)
        if self.model:
            self.model.create(rec, self.uid)
        else:
            # fallback к старому поведению (одиночный CanvasView)
            self.project.bubbles.append(rec)
            self._mark_project_bubbles_index_dirty()

        # 2) локально тоже создаём (если мы источник, то on_model_created проигнорирует)
        self._create_bubble_widget(rec)
        self.bubble_count = bid
        return bid

    def delete_bubble_by_id(self, bid: int):
        bid = int(bid)

        # сообщаем в модель ДО удаления локальных виджетов (как и раньше)
        if self.model:
            self.model.delete(bid, self.uid)
        else:
            self._mark_project_bubbles_index_dirty()

        # если переносили этот пузырь — сбросить состояние
        if self._move_active_bid == bid:
            self._reset_move_button(bid)
            self._move_active_bid = None

        # достаём runtime и удаляем безопасно
        b = self.bubbles.pop(bid, None)
        if b:
            # ВАЖНО: переносим фактический демонтаж на следующий тик,
            # чтобы выйти из текущего слота кнопки/сигнала внутри пузыря.
            QTimer.singleShot(0, lambda bb=b: self._teardown_bubble_graphics(bb))
        bc = self._bubble_cache.pop(bid, None)
        if bc:
            QTimer.singleShot(0, lambda bb=bc: self._teardown_bubble_graphics(bb))

        if self.selected_bubble == bid:
            self._set_selected_bubble(None)

    def unplace_bubble_by_id(self, bid: int):
        rec = self._record_for_bid(int(bid))
        if rec is not None:
            rec.update({'img_idx': None, 'img_u': None, 'img_v': None, 'side': None})
            self._mark_project_bubbles_index_dirty()
        self.delete_bubble_by_id(bid)
        self.bubblesChanged.emit("unplace", bid)

    def _load_bubbles_from_project(self):
        self._refresh_visible_bubbles(force_repack=True)

    def _is_unplaced(self, rec: dict) -> bool:
        img_idx = rec.get('img_idx')
        u = rec.get('img_u'); v = rec.get('img_v'); side = rec.get('side')
        if img_idx is None or u is None or v is None or side is None:
            return True
        if not isinstance(img_idx, int) or img_idx < 0 or img_idx >= len(self.image_bboxes):
            return True
        return False

    def _find_bubble_record(self, bid: int) -> Optional[dict]:
        return self._record_for_bid(int(bid))

    def _place_unplaced_bubble(self, bid: int, img_idx: int, u: float, v: float, side: str) -> None:
        rec = self._find_bubble_record(bid)
        if not rec:
            return
        rec.update({'img_idx': int(img_idx), 'img_u': float(u), 'img_v': float(v), 'side': side})
        self._mark_project_bubbles_index_dirty()
        self._ensure_rect_coords(rec, int(img_idx), float(u), float(v))
        if bid not in self.bubbles:
            self._create_bubble_widget(rec)
        self._apply_bubble_imgpos(bid, int(img_idx), float(u), float(v), side, broadcast=True)
        self.bubblesChanged.emit("place", bid)

    def _apply_bubble_imgpos(
        self,
        bid: int,
        img_idx: int,
        u: float,
        v: float,
        side: str,
        *,
        broadcast: bool = True,
        move_rect: bool = True,
        repack: bool = True,
    ):
        b = self.bubbles.get(bid)
        if not b or img_idx < 0 or img_idx >= len(self.image_bboxes):
            return
        if self._bubble_type() == "on_top":
            du = float(u) - float(b.img_u)
            dv = float(v) - float(b.img_v)
            if move_rect:
                if b.rect_coords:
                    b.rect_coords = {
                        'p1': {
                            'img_u': self._clip01(b.rect_coords['p1']['img_u'] + du),
                            'img_v': self._clip01(b.rect_coords['p1']['img_v'] + dv),
                        },
                        'p2': {
                            'img_u': self._clip01(b.rect_coords['p2']['img_u'] + du),
                            'img_v': self._clip01(b.rect_coords['p2']['img_v'] + dv),
                        },
                    }
                else:
                    b.rect_coords = self._ensure_rect_coords({}, img_idx, u, v)
            elif not b.rect_coords:
                b.rect_coords = self._ensure_rect_coords({}, img_idx, u, v)

            b.img_idx = int(img_idx); b.img_u = float(u); b.img_v = float(v); b.side = side
            rec = self._record_for_bid(bid)
            if rec is not None:
                rec.update({
                    'img_idx': img_idx, 'img_u': b.img_u, 'img_v': b.img_v, 'side': side,
                    'rect_coords': b.rect_coords,
                })
                self._mark_project_bubbles_index_dirty()

            self._update_bubble_rect_visual(bid)
            self._layout_on_top_bubble(bid)

            if broadcast and self.model:
                payload = {
                    'id': bid, 'img_idx': img_idx, 'img_u': u, 'img_v': v, 'side': side,
                    'rect_coords': b.rect_coords,
                }
                payload.update(self._collect_bubble_texts(bid))
                self.model.update(payload, self.uid)
            if broadcast and not self.model:
                self._autosave_timer.start()
            return
        du = float(u) - float(b.img_u)
        dv = float(v) - float(b.img_v)
        scale_factor = self._aside_bubble_scale_factor()

        x, y = self._scene_from_uv(img_idx, u, v)
        r = self.image_bboxes[img_idx]

        # ТЕКУЩИЙ масштаб вида (1 scene unit => S экранных пикселей)
        S = float(self.transform().m11() or self.canvas_scale or 1.0)

        # Видимая полоса сцены по горизонтали (в координатах сцены!)
        vp_w = self.viewport().width()
        top_left_sc  = self.mapToScene(0, 0)
        top_right_sc = self.mapToScene(max(0, vp_w - 1), 0)
        scene_left   = min(top_left_sc.x(), top_right_sc.x())
        scene_right  = max(top_left_sc.x(), top_right_sc.x())

        # Поля от краёв экрана/изображения в КООРДИНАТАХ СЦЕНЫ
        margin_sc = float(self.side_margin) / S

        if side == "left":
            # Левая зона: от левого края экрана до левого края картинки (минус отступ)
            region_start_sc = scene_left + margin_sc
            line_x_sc = r.left() - margin_sc
            region_end_sc = line_x_sc

            # Доступная ширина зоны в СЦЕНЕ
            span_sc = max(0.0, region_end_sc - region_start_sc)

            # Итоговая ширина контейнера — в пикселях ЭКРАНА (прокси игнорирует трансформации)
            # Ограничиваем по min/max aside: при min пузырь может выйти за экран, при max — отдалиться от края.
            if self._scale_bubbles_enabled:
                raw_available_w_px = int(max(0.0, span_sc * S))
                available_w_px = max(self._aside_min_width_px, min(raw_available_w_px, self._aside_max_width_px))
            else:
                available_w_px = int(self._aside_min_width_px)

            # Ставим ПРАВЫЙ край пузыря к краю изображения (зеркально правой стороне).
            win_x_sc  = region_end_sc - ((available_w_px * scale_factor) / S)

        else:  # side == "right"
            # Правая зона: от правого края картинки (плюс отступ) до правого края экрана
            region_start_sc = (r.right() + margin_sc)
            region_end_sc   = scene_right - margin_sc

            # Не даём зоне «перешагнуть» левую грань экрана
            region_start_sc = max(region_start_sc, scene_left + margin_sc)

            span_sc = max(0.0, region_end_sc - region_start_sc)
            if self._scale_bubbles_enabled:
                raw_available_w_px = int(max(0.0, span_sc * S))
                available_w_px = max(self._aside_min_width_px, min(raw_available_w_px, self._aside_max_width_px))
            else:
                available_w_px = int(self._aside_min_width_px)

            # Ставим ЛЕВЫЙ край пузыря к началу зоны
            win_x_sc  = region_start_sc

            # Линия к краю изображения
            line_x_sc = r.right() + margin_sc

        # Прокси-виджет имеет ItemIgnoresTransformations, поэтому его ширина в сцене = px / S.
        bubble_w_sc = (available_w_px * scale_factor) / S
        visible_left_sc = scene_left + margin_sc
        visible_right_sc = scene_right - margin_sc - bubble_w_sc
        if side == "left":
            # Для левой стороны приоритет как у правой: не заезжать на картинку.
            # Если одновременно не помещается в viewport, допускаем выход за левую границу.
            max_x_sc = region_end_sc - bubble_w_sc
            if max_x_sc >= visible_left_sc:
                win_x_sc = min(max(win_x_sc, visible_left_sc), max_x_sc)
            else:
                win_x_sc = max_x_sc
        else:
            if visible_right_sc >= region_start_sc:
                win_x_sc = min(max(win_x_sc, region_start_sc), visible_right_sc)
            else:
                win_x_sc = region_start_sc

        # Вертикальное центрирование вокруг якорной точки
        block_cy = y

        # Сохраняем рабочие параметры
        b.anchor_y  = block_cy
        b.max_width = available_w_px      # ширина контейнера — в экранных пикселях
        b.line_x    = line_x_sc

        # 1) промеряем высоту контейнера на текущей ширине (px)
        h = self._measure_container(b, available_w_px)
        h_scaled = h * scale_factor

        # 2) позиция прокси-виджета — в КООРДИНАТАХ СЦЕНЫ
        if b.proxy_widget:
            b.proxy_widget.setScale(scale_factor)
            px, py = self._snap_scene_point(win_x_sc, block_cy - h_scaled / 2.0)
            b.proxy_widget.setPos(px, py)

        # 3) линия — в КООРДИНАТАХ СЦЕНЫ
        if b.line_item:
            pen = b.line_item.pen()
            pen.setCosmetic(False)
            pen.setWidthF(max(0.6, 2.0 / max(1.0, S)))
            b.line_item.setPen(pen)
            x1, y1 = self._snap_scene_point(x, y)
            x2, y2 = self._snap_scene_point(line_x_sc, block_cy)
            b.line_item.setLine(x1, y1, x2, y2)

        # Обновление полей пузыря как у тебя было...
        b.img_idx = int(img_idx); b.img_u = float(u); b.img_v = float(v); b.side = side
        if move_rect:
            if b.rect_coords:
                b.rect_coords = {
                    'p1': {
                        'img_u': self._clip01(b.rect_coords['p1']['img_u'] + du),
                        'img_v': self._clip01(b.rect_coords['p1']['img_v'] + dv),
                    },
                    'p2': {
                        'img_u': self._clip01(b.rect_coords['p2']['img_u'] + du),
                        'img_v': self._clip01(b.rect_coords['p2']['img_v'] + dv),
                    },
                }
            else:
                b.rect_coords = self._ensure_rect_coords({}, img_idx, u, v)
        elif not b.rect_coords:
            b.rect_coords = self._ensure_rect_coords({}, img_idx, u, v)
        # синхронизируем с self.project.bubbles (как было)
        rec = self._record_for_bid(bid)
        if rec is not None:
            rec.update({
                'img_idx': img_idx, 'img_u': b.img_u, 'img_v': b.img_v, 'side': side,
                'rect_coords': b.rect_coords,
            })
            self._mark_project_bubbles_index_dirty()
        self._update_bubble_rect_visual(bid)
        # <-- ВАЖНО: шлём в модель только если broadcast = True
        if broadcast and self.model:
            payload = {
                'id': bid, 'img_idx': img_idx, 'img_u': u, 'img_v': v, 'side': side,
                'rect_coords': b.rect_coords,
            }
            payload.update(self._collect_bubble_texts(bid))
            self.model.update(payload, self.uid)
        if repack:
            self._repack_bubbles_for(img_idx, side)

    def _schedule_repack(self, img_idx: int, side: str) -> None:
        if self._bubble_type() == "on_top":
            return
        if img_idx < 0 or img_idx >= len(self.image_bboxes):
            return
        side = "left" if side == "left" else "right"
        self._repack_pending.add((int(img_idx), side))
        if not self._repack_timer.isActive():
            self._repack_timer.start()

    def _flush_repack_pending(self) -> None:
        if not self._repack_pending:
            return
        pending = list(self._repack_pending)
        self._repack_pending.clear()
        for img_idx, side in pending:
            self._repack_bubbles_for(img_idx, side)
            
    def _on_model_created(self, rec: dict, origin: str):
        if not self._tabs_autosync_enabled:
            return
        self._mark_project_bubbles_index_dirty()
        if origin == self.uid:
            return
        if self._is_unplaced(rec):
            return
        bid = int(rec['id'])
        if bid in self.bubbles:
            self._apply_bubble_imgpos(bid, rec['img_idx'], rec['img_u'], rec['img_v'], rec['side'], broadcast=False)
            b = self.bubbles[bid]
            needs_adjust = False
            if b.original_text_widget and rec.get('original_text') is not None:
                b.original_text_widget.blockSignals(True)
                b.original_text_widget.setPlainText(rec.get('original_text', ''))
                b.original_text_widget.blockSignals(False)
                needs_adjust = True
            if b.text_widget and rec.get('text') is not None:
                b.text_widget.blockSignals(True)
                b.text_widget.setPlainText(rec.get('text', ''))
                b.text_widget.blockSignals(False)
                needs_adjust = True
            if needs_adjust:
                self._adjust_box(bid, update_model=False)  # не обновляем модель повторно
            return
        try:
            if not self._is_page_in_active_window(int(rec.get('img_idx'))):
                return
        except Exception:
            return
        self._create_bubble_widget(rec)

    def _on_model_updated(self, rec: dict, origin: str):
        if not self._tabs_autosync_enabled:
            return
        self._mark_project_bubbles_index_dirty()
        # игнорим собственные апдейты — локально мы уже всё сделали
        if origin == self.uid:
            return
        bid = int(rec['id'])
        if self._bubble_has_focus(bid):
            # не тянем апдейты в активный пузырь — применим после потери фокуса
            self._deferred_model_updates[bid] = rec
            return
        self._apply_model_update(rec, origin)

    def _on_model_deleted(self, bid: int, origin: str):
        if not self._tabs_autosync_enabled:
            return
        self._mark_project_bubbles_index_dirty()
        # даже если мы источник — повторное удаление безопасно
        b = self.bubbles.pop(int(bid), None)
        if not b:
            bc = self._bubble_cache.pop(int(bid), None)
            if bc:
                QTimer.singleShot(0, lambda bb=bc: self._teardown_bubble_graphics(bb))
            return
        # делаем то же безопасное сворачивание, что и в delete_bubble_by_id
        QTimer.singleShot(0, lambda bb=b: self._teardown_bubble_graphics(bb))
        bc = self._bubble_cache.pop(int(bid), None)
        if bc:
            QTimer.singleShot(0, lambda bb=bc: self._teardown_bubble_graphics(bb))
        if self.selected_bubble == bid:
            self._set_selected_bubble(None)

    def _on_model_unplaced(self, bid: int, origin: str):
        if not self._tabs_autosync_enabled:
            return
        # аналогично delete, но запись остаётся в модели с None-полями
        self._on_model_deleted(bid, origin)

    def _sanitize_paste(self, s: str) -> str:
        s = s.replace('\u2026', '...')
        body = s.strip()
        n = len(body)
        if n % 2 == 0 and n > 0:
            mid = n // 2
            if body[:mid] == body[mid:]:
                lead = s[:len(s) - len(s.lstrip())]
                tail = s[len(s.rstrip()):]
                return lead + body[:mid] + tail
        return s

    def _collect_bubble_texts(self, bid: int) -> Dict[str, str]:
        """Возвращает текущие строки перевода и оригинала для пузыря."""
        b = self.bubbles.get(bid)
        translation = ''
        original = ''
        if b and b.text_widget:
            translation = b.text_widget.toPlainText()
        else:
            rec = self._record_for_bid(int(bid))
            if rec is not None:
                translation = rec.get('text', '')

        if b and b.original_text_widget:
            original = b.original_text_widget.toPlainText()
        else:
            rec = self._record_for_bid(int(bid))
            if rec is not None:
                original = rec.get('original_text', '')
        return {'text': translation, 'original_text': original}
    
    def _on_copy_bubble(self, bid: int):
        b = self.bubbles.get(bid)
        if not b:
            return
        # Копируем строку оригинала, если она есть; иначе — перевод
        if b.original_text_widget:
            QGuiApplication.clipboard().setText(b.original_text_widget.toPlainText())
        elif b.text_widget:
            QGuiApplication.clipboard().setText(b.text_widget.toPlainText())

    def _on_replace_bubble(self, bid: int):
        b = self.bubbles.get(bid)
        if not b or not b.text_widget:
            return
        text = QGuiApplication.clipboard().text() or ""
        text = self._sanitize_paste(text)
        b.text_widget.blockSignals(True)
        b.text_widget.setPlainText(text)
        b.text_widget.blockSignals(False)

        self._adjust_box(bid, update_model=True)  # пересчитать размеры/линию + синхронизировать модель

    def _on_delete_bubble(self, bid: int):
        # если сейчас активен перенос этого пузыря — выключим, вернём текст кнопки
        if self._move_active_bid == bid:
            self._reset_move_button(bid)
            self._move_active_bid = None
        self.delete_bubble_by_id(bid)

    def _on_unplace_bubble(self, bid: int):
        if self._move_active_bid == bid:
            self._reset_move_button(bid)
            self._move_active_bid = None

        # увести фокус сразу, до изменения JSON/удаления
        try:
            fw = QGuiApplication.focusObject()
            if hasattr(fw, "clearFocus"):
                fw.clearFocus()  # type: ignore[attr-defined]
        except Exception:
            traceback.print_exc()
            pass

        # очистить координаты в JSON и убрать визуально
        rec = self._record_for_bid(int(bid))
        if rec is not None:
            rec.update({'img_idx': None, 'img_u': None, 'img_v': None, 'side': None})
            self._mark_project_bubbles_index_dirty()

        self.delete_bubble_by_id(bid)  # удаляет со сцены отложенно и безопасно
        self.bubblesChanged.emit("unplace", bid)

    def _on_translate_bubble(self, bid: int):
        """Хук перевода для конкретного пузыря; в базовом классе не реализован."""
        return

    def _reset_move_button(self, bid: Optional[int]):
        if bid is None:
            return
        b = self.bubbles.get(bid)
        if b and b.move_btn:
            b.move_btn.setText("Переместить")

    def _measure_container(self, b: BubbleRuntime, width: int) -> int:
        if not b or not b.proxy_widget or not b.container_widget:
            return 0

        translation = b.text_widget.toPlainText() if b.text_widget else ""
        original = b.original_text_widget.toPlainText() if b.original_text_widget else ""
        layout_key = (int(width), hash(translation), hash(original))
        if b.measured_layout_key == layout_key and b.height_px > 0:
            b.proxy_widget.resize(width, int(b.height_px))
            return int(b.height_px)

        w = b.container_widget
        w.setFixedWidth(width)
        # тянем QTextEdit по реальной высоте документа
        if b.original_text_widget:
            visual_hint = self._visual_lines_55(b.original_text_widget.toPlainText())
            ote_h = self._text_doc_height(b.original_text_widget, width, visual_hint)
            b.original_text_widget.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Fixed)
            b.original_text_widget.setFixedHeight(ote_h)
        if b.text_widget:
            visual_hint = self._visual_lines_55(b.text_widget.toPlainText())
            te_h = self._text_doc_height(b.text_widget, width, visual_hint)
            b.text_widget.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Fixed)
            b.text_widget.setFixedHeight(te_h)

        w.adjustSize()
        h = max(1, w.sizeHint().height())
        b.proxy_widget.resize(width, h)
        b.height_px = int(h)
        b.measured_layout_key = layout_key
        return h
    
    def _create_bubble_widget(self, rec: dict, repack_on_init: bool = True):
        try:
            bid = int(rec.get('id'))
        except Exception:
            #traceback.print_exc()
            bid = rec.get('id') 
        img_idx = int(rec.get('img_idx', 0))
        u = float(rec.get('img_u', 0.5)); v = float(rec.get('img_v', 0.5))
        side = rec.get('side', 'right')
        original_text = rec.get('original_text', '')
        text = rec.get('text', '')
        rect_coords = self._ensure_rect_coords(rec, img_idx, u, v)
        u, v = self._rect_center_uv(rect_coords)

        # линия
        line = QGraphicsLineItem()
        pen = QPen(); pen.setWidth(2)
        line.setPen(pen)
        self.scene.addItem(line)
        try:
            line.setZValue(1000.0)
        except Exception:
            pass
        if self._bubble_type() == "on_top":
            line.setVisible(False)
        self._mark_item_as_bubble_part(line, bid)
        bubble_type = self._bubble_type()
        header_widgets: List[QWidget] = []
        try:
            header_widgets = self.build_bubble_header(bid)
        except Exception:
            traceback.print_exc()

        # контейнер пузыря
        container = self._prepare_embedded_widget(QWidget())
        vbox = QVBoxLayout(container)
        vbox.setContentsMargins(0, 0, 0, 0)
        vbox.setSpacing(4)

        header_widget = None
        header_proxy = None
        original_te: Optional[QTextEdit] = None
        original_container = None
        original_proxy = None
        footer = None
        footer_proxy = None
        move_btn = None

        if bubble_type == "on_top":
            footer_widgets: List[QWidget] = []
            try:
                footer_widgets = self.build_bubble_footer(bid)
            except Exception:
                traceback.print_exc()
            if header_widgets:
                header_widget = self._prepare_embedded_widget(QWidget())
                header_layout = QVBoxLayout(header_widget)
                header_layout.setContentsMargins(0, 0, 0, 0)
                header_layout.setSpacing(4)
                header_layout.setSizeConstraint(QLayout.SizeConstraint.SetFixedSize)
                for w in header_widgets:
                    header_layout.addWidget(w, alignment=Qt.AlignmentFlag.AlignCenter)
                header_widget.adjustSize()
                header_proxy = self._add_embedded_proxy_widget(header_widget)
                header_proxy.setFlag(QGraphicsItem.GraphicsItemFlag.ItemIgnoresTransformations, True)
                self._mark_item_as_bubble_part(header_proxy, bid)
                try:
                    header_proxy.setZValue(1000.0)
                except Exception:
                    pass

            if self.editable:
                original_te = self._prepare_embedded_widget(QTextEdit())
                original_te.setPlainText(original_text)
                original_te.setLineWrapMode(QTextEdit.LineWrapMode.WidgetWidth)
                original_te.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
                original_te.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
                original_te.setStyleSheet("QTextEdit{padding:4px;margin:0px;border:1px solid #999;}")
                original_te.setPlaceholderText("Оригинал")
                original_container = self._prepare_embedded_widget(QWidget())
                orig_layout = QVBoxLayout(original_container)
                orig_layout.setContentsMargins(0, 0, 0, 0)
                orig_layout.setSpacing(0)
                orig_layout.addWidget(original_te)
                original_proxy = self._add_embedded_proxy_widget(original_container)
                original_proxy.setFlag(QGraphicsItem.GraphicsItemFlag.ItemIgnoresTransformations, True)
                self._mark_item_as_bubble_part(original_proxy, bid)
                try:
                    original_proxy.setZValue(1000.0)
                except Exception:
                    pass

            te = self._prepare_embedded_widget(QTextEdit())
            te.setPlainText(text)
            te.setLineWrapMode(QTextEdit.LineWrapMode.WidgetWidth)
            te.setAlignment(Qt.AlignmentFlag.AlignCenter)
            te.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
            te.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
            te.setStyleSheet("QTextEdit{padding:4px;margin:0px;border:1px solid #999;}")
            te.setPlaceholderText("Перевод")
            vbox.addWidget(te)

            if self.editable:
                footer_buttons = self._create_bubble_footer_buttons(bid)
            else:
                footer_buttons = None

            if footer_widgets or footer_buttons is not None:
                footer = self._prepare_embedded_widget(QWidget())
                footer_layout = QVBoxLayout(footer)
                footer_layout.setContentsMargins(0, 0, 0, 0)
                footer_layout.setSpacing(4)
                for w in footer_widgets:
                    footer_layout.addWidget(w)
                if footer_buttons is not None:
                    footer_layout.addWidget(footer_buttons)
                    move_btn = getattr(footer_buttons, "_move_btn", None)
                footer_proxy = self._add_embedded_proxy_widget(footer)
                footer_proxy.setFlag(QGraphicsItem.GraphicsItemFlag.ItemIgnoresTransformations, True)
                self._mark_item_as_bubble_part(footer_proxy, bid)
                try:
                    footer_proxy.setZValue(1000.0)
                except Exception:
                    pass
        else:
            # ДОП. виджеты от наследника — над текстовыми полями
            for w in header_widgets:
                vbox.addWidget(w)

            # Оригинальный текст (показываем только в режиме редактирования)
            if self.editable:
                original_te = self._prepare_embedded_widget(QTextEdit())
                original_te.setPlainText(original_text)
                original_te.setLineWrapMode(QTextEdit.LineWrapMode.WidgetWidth)
                original_te.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
                original_te.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
                original_te.setStyleSheet("QTextEdit{padding:4px;margin:0px;border:1px solid #999;}")
                original_te.setPlaceholderText("Оригинал")
                vbox.addWidget(original_te)

            te = self._prepare_embedded_widget(QTextEdit())
            te.setPlainText(text)
            te.setLineWrapMode(QTextEdit.LineWrapMode.WidgetWidth)
            te.setVerticalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
            te.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
            te.setStyleSheet("QTextEdit{padding:4px;margin:0px;border:1px solid #999;}")
            te.setPlaceholderText("Перевод")
            vbox.addWidget(te)

            # ДОП. виджеты от наследника — под текстом, до футера
            try:
                for w in self.build_bubble_footer(bid):
                    vbox.addWidget(w, alignment=Qt.AlignmentFlag.AlignCenter)
            except Exception:
                traceback.print_exc()

            # Футер (ряд кнопок) — можно переопределить или убрать
            footer = self._create_bubble_footer_buttons(bid)
            if footer is not None:
                vbox.addWidget(footer)
                # попытаемся вытащить move_btn, если футер стандартный
                move_btn = getattr(footer, "_move_btn", None)

        proxy = self._add_embedded_proxy_widget(container)
        proxy.setFlag(QGraphicsItem.GraphicsItemFlag.ItemIgnoresTransformations, True)
        self._mark_item_as_bubble_part(proxy, bid)
        try:
            proxy.setZValue(1000.0)
        except Exception:
            pass
        # выбор по фокусу
        def _select_this():
            self._set_selected_bubble(bid)
            self._refresh_rect_visibility(bid)
        orig_focus_in = te.focusInEvent
        def _focus_in(ev):
            if orig_focus_in:
                orig_focus_in(ev)
            QTimer.singleShot(0, _select_this)
        te.focusInEvent = _focus_in  # type: ignore[assignment]
        orig_focus_out = te.focusOutEvent
        def _focus_out(ev):
            if orig_focus_out:
                orig_focus_out(ev)
            QTimer.singleShot(0, lambda b=bid: self._refresh_rect_visibility(b))
            QTimer.singleShot(0, lambda b=bid: self._flush_pending_layout_update_for(b))
            QTimer.singleShot(0, lambda b=bid: self._flush_pending_text_update_for(b))
            QTimer.singleShot(0, lambda b=bid: self._apply_deferred_model_update_for(b))
        te.focusOutEvent = _focus_out  # type: ignore[assignment]
        if original_te:
            orig_focus_in_original = original_te.focusInEvent
            def _focus_in_original(ev):
                if orig_focus_in_original:
                    orig_focus_in_original(ev)
                QTimer.singleShot(0, _select_this)
            original_te.focusInEvent = _focus_in_original  # type: ignore[assignment]
            orig_focus_out_original = original_te.focusOutEvent
            def _focus_out_original(ev):
                if orig_focus_out_original:
                    orig_focus_out_original(ev)
                QTimer.singleShot(0, lambda b=bid: self._refresh_rect_visibility(b))
                QTimer.singleShot(0, lambda b=bid: self._flush_pending_layout_update_for(b))
                QTimer.singleShot(0, lambda b=bid: self._flush_pending_text_update_for(b))
                QTimer.singleShot(0, lambda b=bid: self._apply_deferred_model_update_for(b))
            original_te.focusOutEvent = _focus_out_original  # type: ignore[assignment]

        self.bubbles[bid] = BubbleRuntime(
            id=bid, img_idx=img_idx, img_u=u, img_v=v, side=side,
            line_item=line, proxy_widget=proxy, text_widget=te, original_text_widget=original_te,
            container_widget=container, move_btn=move_btn, footer_widget=footer,
            rect_coords=rect_coords,
            header_widget=header_widget,
            header_proxy=header_proxy,
            original_container=original_container,
            original_proxy=original_proxy,
            footer_proxy=footer_proxy,
        )
        self._apply_bubble_opacity(bid)

        # стартовое позиционирование
        self._apply_bubble_imgpos(bid, img_idx, u, v, side, broadcast=False, repack=bool(repack_on_init))
        self._update_bubble_rect_visual(bid)

        if self.editable:
            if original_te:
                original_te.textChanged.connect(lambda b=bid: self._on_original_changed(b))
            te.textChanged.connect(lambda b=bid: self._on_translation_changed(b))

    def _on_translation_changed(self, bid: int):
        self._on_any_text_changed(bid, is_original=False)

    def _on_original_changed(self, bid: int):
        self._on_any_text_changed(bid, is_original=True)

    def _schedule_layout_update(self, bid: int) -> None:
        self._pending_layout_updates.add(int(bid))
        if not self._text_layout_timer.isActive():
            self._text_layout_timer.start()

    def _flush_pending_layout_update_for(self, bid: int) -> None:
        if int(bid) not in self._pending_layout_updates:
            return
        self._pending_layout_updates.discard(int(bid))
        self._adjust_box(int(bid), update_model=False, repack=False)

    def _flush_pending_layout_updates(self) -> None:
        if not self._pending_layout_updates:
            return
        pending = list(self._pending_layout_updates)
        self._pending_layout_updates.clear()
        for bid in pending:
            self._adjust_box(int(bid), update_model=False, repack=False)
            b = self.bubbles.get(int(bid))
            if b and self._bubble_type() != "on_top":
                self._schedule_repack(b.img_idx, b.side)

    def _on_any_text_changed(self, bid: int, *, is_original: bool):
        b = self.bubbles.get(bid)
        if not b:
            return
        if is_original and not b.original_text_widget:
            return
        if (not is_original) and not b.text_widget:
            return
        # Визуальный пересчёт идёт батчем, чтобы не тормозить при наборе.
        self._schedule_layout_update(bid)
        # но обновление модели откладываем (debounce)
        self._pending_text_updates[bid] = self._collect_bubble_texts(bid)
        self._text_update_timer.start()  # перезапустит таймер на 300мс
        if not self.model:
            self._autosave_timer.start()
    def _flush_pending_text_updates(self):
        """Отправляем накопленные обновления текста в модель (если пузырь не в фокусе)."""
        if not self.model:
            return
        still_pending: Dict[int, Dict[str, str]] = {}
        for bid, texts in list(self._pending_text_updates.items()):
            if self._bubble_has_focus(bid):
                still_pending[bid] = texts
                continue
            b = self.bubbles.get(bid)
            if b:
                rec = {
                    'id': bid,
                    'text': texts.get('text', ''),
                    'original_text': texts.get('original_text', ''),
                    'img_idx': b.img_idx, 'img_u': b.img_u, 'img_v': b.img_v, 'side': b.side,
                }
                self.model.update(rec, self.uid)
        self._pending_text_updates = still_pending

    def _flush_pending_text_update_for(self, bid: int) -> None:
        if not self.model:
            return
        if self._bubble_has_focus(bid):
            return
        texts = self._pending_text_updates.pop(bid, None)
        if not texts:
            return
        b = self.bubbles.get(bid)
        if not b:
            return
        rec = {
            'id': bid,
            'text': texts.get('text', ''),
            'original_text': texts.get('original_text', ''),
            'img_idx': b.img_idx, 'img_u': b.img_u, 'img_v': b.img_v, 'side': b.side,
        }
        self.model.update(rec, self.uid)

    def _apply_deferred_model_update_for(self, bid: int) -> None:
        if self._bubble_has_focus(bid):
            return
        rec = self._deferred_model_updates.pop(bid, None)
        if not rec:
            return
        self._apply_model_update(rec, origin="deferred")

    def _apply_model_update(self, rec: dict, origin: str) -> None:
        bid = int(rec['id'])
        b = self.bubbles.get(bid)
        if not b:
            return self._on_model_created(rec, origin)
        coords = None
        if 'rect_coords' in rec:
            coords = self._normalize_rect_coords(rec.get('rect_coords'))
        if coords:
            self._set_bubble_rect_coords(bid, coords, update_model=False)
        elif all(k in rec for k in ('img_idx','img_u','img_v','side')) and not self._is_unplaced(rec):
            self._apply_bubble_imgpos(
                bid,
                rec['img_idx'],
                rec['img_u'],
                rec['img_v'],
                rec['side'],
                broadcast=False,
                repack=False,
            )
            self._schedule_repack(int(rec['img_idx']), str(rec['side']))
        needs_adjust = False
        if 'original_text' in rec and b.original_text_widget:
            b.original_text_widget.blockSignals(True)
            b.original_text_widget.setPlainText(rec.get('original_text', ''))
            b.original_text_widget.blockSignals(False)
            needs_adjust = True
        if 'text' in rec and b.text_widget:
            b.text_widget.blockSignals(True)
            b.text_widget.setPlainText(rec.get('text', ''))
            b.text_widget.blockSignals(False)
            needs_adjust = True
        if needs_adjust:
            self._adjust_box(bid, update_model=False)  # ВАЖНО: не обновляем модель повторно!
    
    def _adjust_box(self, bid: int, update_model: bool = True, repack: bool = True):
        b = self.bubbles.get(bid)
        if not b or not b.text_widget or not b.proxy_widget:
            return

        if self._bubble_type() == "on_top":
            self._layout_on_top_bubble(bid)
            self._update_bubble_rect_visual(bid)
        else:
            # пересчитать размеры текста/контейнера на текущей max_width
            h = self._measure_container(b, b.max_width)
            scale_factor = self._aside_bubble_scale_factor()

            # центрировать контейнер по сохранённой опорной Y-координате
            cur = b.proxy_widget.pos()
            b.proxy_widget.setScale(scale_factor)
            b.proxy_widget.setPos(cur.x(), b.anchor_y - (h * scale_factor) / 2.0)

            # линия остаётся до центра контейнера (anchor_y — это центр)
            if b.line_item:
                bx, by = self._scene_from_uv(b.img_idx, b.img_u, b.img_v)
                b.line_item.setLine(bx, by, b.line_x, b.anchor_y)
            self._update_bubble_rect_visual(bid)

        # сохранить тексты + autosave (как было)
        texts = self._collect_bubble_texts(bid)
        rec = self._record_for_bid(int(bid))
        if rec is not None:
            rec['text'] = texts.get('text', '')
            rec['original_text'] = texts.get('original_text', '')

        # обновляем модель при необходимости (для кнопок Заменить/OCR)
        if update_model and self.model:
            rec = {'id': bid,
                   'text': texts.get('text', ''),
                   'original_text': texts.get('original_text', ''),
                   'img_idx': b.img_idx, 'img_u': b.img_u, 'img_v': b.img_v, 'side': b.side}
            self.model.update(rec, self.uid)

        if update_model or not self._bubble_has_focus(bid):
            self.bubblesChanged.emit("text", bid)
        if repack and self._bubble_type() != "on_top":
            self._repack_bubbles_for(b.img_idx, b.side)

    # выбор/перенос
    def eventFilter(self, obj: QObject, ev: QEvent) -> bool:
        if ev.type() == QEvent.Type.MouseButtonPress:
            # если кликнули не в QTextEdit, снимаем выбор
            if not isinstance(obj, QTextEdit):
                self._set_selected_bubble(None)
        return super().eventFilter(obj, ev)

    def mousePressEvent(self, e):
        item = None
        if self.editable:
            item = self.itemAt(e.pos())
            if item:
                data = item.data(0)
                if isinstance(data, tuple) and len(data) == 3 and data[0] == "rect_handle":
                    bid, idx = int(data[1]), int(data[2])
                    self._active_rect_handle = (bid, idx)
                    self._set_selected_bubble(bid)
                    self._refresh_rect_visibility(bid)
                    b = self.bubbles.get(bid)
                    if b:
                        if b.text_widget:
                            b.text_widget.setFocus(Qt.FocusReason.MouseFocusReason)
                        elif b.original_text_widget:
                            b.original_text_widget.setFocus(Qt.FocusReason.MouseFocusReason)
                    e.accept()
                    return
        if self._move_active_bid is None:
            if self.editable and not self._is_bubble_item(item):
                self._clear_bubble_focus()
                self._set_selected_bubble(None)
            return super().mousePressEvent(e)

        scene_pt = self.mapToScene(e.pos())
        target_idx = None
        for i, r in enumerate(self.image_bboxes):
            if r.contains(scene_pt):
                target_idx = i
                break
        if target_idx is None:
            # не по картинке — не завершаем перенос
            return super().mousePressEvent(e)

        r = self.image_bboxes[target_idx]
        side = 'left' if scene_pt.x() < (r.left() + r.right()) / 2 else 'right'
        u, v = self._uv_from_scene(target_idx, scene_pt.x(), scene_pt.y())
        bid = self._move_active_bid
        if bid not in self.bubbles:
            self._place_unplaced_bubble(bid, target_idx, u, v, side)
        else:
            self._apply_bubble_imgpos(bid, target_idx, u, v, side)  # здесь оставляем broadcast=True (дефолт)
        self._reset_move_button(bid)
        self._move_active_bid = None
        e.accept()

    def mouseMoveEvent(self, e):
        if self._active_rect_handle:
            bid, idx = self._active_rect_handle
            scene_pt = self.mapToScene(e.pos())
            self._resize_rect_by_handle(bid, idx, scene_pt.x(), scene_pt.y(), update_model=False)
            e.accept()
            return
        return super().mouseMoveEvent(e)

    def mouseReleaseEvent(self, e):
        if self._active_rect_handle:
            bid, idx = self._active_rect_handle
            self._active_rect_handle = None
            scene_pt = self.mapToScene(e.pos())
            self._resize_rect_by_handle(bid, idx, scene_pt.x(), scene_pt.y(), update_model=True)
            e.accept()
            return
        return super().mouseReleaseEvent(e)

    def _teardown_bubble_graphics(self, b: "BubbleRuntime"):
        """Безопасно убрать графические элементы пузыря (фокус/детач/удаление)."""
        if not b:
            return
        # 1) увести фокус
        try:
            if b.text_widget and b.text_widget.hasFocus():
                b.text_widget.blockSignals(True)
                b.text_widget.clearFocus()
                b.text_widget.blockSignals(False)
            if b.original_text_widget and b.original_text_widget.hasFocus():
                b.original_text_widget.blockSignals(True)
                b.original_text_widget.clearFocus()
                b.original_text_widget.blockSignals(False)
        except Exception:
            #traceback.print_exc()
            pass
        try:
            if b.container_widget and isinstance(b.container_widget, QWidget):
                b.container_widget.clearFocus()
        except Exception:
            #traceback.print_exc()
            pass
        try:
            self.viewport().setFocus()  # переведём фокус на сам вид
        except Exception:
            #traceback.print_exc()
            pass

        # 2) убрать proxy-виджеты со сцены (без setWidget(None), чтобы не вспыхивали top-level окна)
        self._dispose_proxy_widget(b.proxy_widget)
        self._dispose_proxy_widget(b.header_proxy)
        self._dispose_proxy_widget(b.original_proxy)
        self._dispose_proxy_widget(b.footer_proxy)

        try:
            if b.line_item:
                self.scene.removeItem(b.line_item)
        except Exception:
            pass
        try:
            if b.rect_item:
                self.scene.removeItem(b.rect_item)
        except Exception:
            pass
        try:
            if b.rect_item_inner:
                self.scene.removeItem(b.rect_item_inner)
        except Exception:
            pass
        try:
            if b.rect_handles:
                for h in b.rect_handles:
                    try:
                        self.scene.removeItem(h)
                    except Exception:
                        pass
        except Exception:
            pass


    def toggle_move_mode(self, bid: Optional[int]):
        # если кликнули по уже активному — выключаем
        if self._move_active_bid == bid:
            self._reset_move_button(bid)
            self._move_active_bid = None
            return

        # если был активен другой — вернуть его кнопку
        if self._move_active_bid is not None and self._move_active_bid in self.bubbles:
            self._reset_move_button(self._move_active_bid)

        # включаем новый
        self._move_active_bid = bid
        if bid is not None:
            b = self.bubbles.get(bid)
            if b and b.move_btn:
                b.move_btn.setText("Отменить перемещение")

    def wheelEvent(self, e: QWheelEvent):
        if e.modifiers() & Qt.KeyboardModifier.ControlModifier:
            factor = 1.1 if e.angleDelta().y() > 0 else (1/1.1)
            self._zoom_canvas(factor)
            e.accept()
            return
        super().wheelEvent(e)

    def changeEvent(self, e):
        super().changeEvent(e)
        if e.type() in (
            QEvent.Type.WindowActivate,
            QEvent.Type.WindowDeactivate,
            QEvent.Type.ActivationChange,
        ):
            if self._bubble_type() == "on_top":
                for b in list(self.bubbles.values()):
                    self._layout_on_top_bubble(b.id)

    # ---------- расширение пузырей (API/хуки) ----------
    def build_bubble_header(self, bid: int) -> List[QWidget]:
        """
        Хук для наследников: вернуть список виджетов,
        которые будут добавлены НАД двумя QTextEdit.
        По умолчанию — пусто.
        """
        return []

    def build_bubble_footer(self, bid: int) -> List[QWidget]:
        """
        Хук для наследников: вернуть список виджетов,
        которые будут добавлены ПОД QTextEdit, но ПЕРЕД футером-кнопками.
        По умолчанию — пусто.
        """
        return []

    def _create_bubble_footer_buttons(self, bid: int) -> Optional[QWidget]:
        """
        Создаёт «футер» пузыря (ряд кнопок). Можно переопределить в наследнике,
        вернуть None — если футер не нужен.
        """
        if not self.editable:
            return None

        row = self._prepare_embedded_widget(QWidget())
        h = QHBoxLayout(row)
        h.setContentsMargins(0, 0, 0, 0)
        h.setSpacing(6)

        btn_copy = QPushButton("Копировать")
        btn_copy.clicked.connect(lambda checked=False, b=bid: self._on_copy_bubble(b))
        h.addWidget(btn_copy)

        btn_replace = QPushButton("Заменить")
        btn_replace.clicked.connect(lambda checked=False, b=bid: self._on_replace_bubble(b))
        h.addWidget(btn_replace)

        move_btn = QPushButton("Переместить")
        move_btn.clicked.connect(lambda checked=False, b=bid: self.toggle_move_mode(b))
        h.addWidget(move_btn)

        btn_translate = QPushButton("Перевести")
        btn_translate.clicked.connect(lambda checked=False, b=bid: self._on_translate_bubble(b))
        h.addWidget(btn_translate)

        btn_delete = QPushButton("Удалить")
        btn_delete.clicked.connect(lambda checked=False, b=bid: self._on_delete_bubble(b))
        h.addWidget(btn_delete)

        # сохраним move_btn в runtime позже, когда runtime создадим
        row._move_btn = move_btn  # type: ignore[attr-defined]
        return row

    def add_bubble_block(self, bid: int, widget: QWidget, *, before_footer: bool = True) -> None:
        """
        Публичный метод: добавить кастомный виджет в конец «тела» пузыря.
        Если before_footer=True — вставить перед футером (если футер есть).
        """
        b = self.bubbles.get(bid)
        if not b or not b.container_widget:
            return
        vbox = b.container_widget.layout()
        if not isinstance(vbox, QVBoxLayout):
            return

        if before_footer and b.footer_widget is not None:
            # вставить перед футером
            idx = vbox.indexOf(b.footer_widget)
            if idx >= 0:
                vbox.insertWidget(idx, widget)
                return
        # иначе — просто в конец
        vbox.addWidget(widget)

    def _repack_bubbles_for(self, img_idx: int, side: str):
        """
        Развести пузыри на одной картинке и одной стороне без наложений.
        Сдвиги минимальные, внутри вертикальных границ изображения.
        """
        if self._bubble_type() == "on_top":
            return
        if img_idx < 0 or img_idx >= len(self.image_bboxes):
            return
        r = self.image_bboxes[img_idx]

        # Текущий масштаб (scene→device)
        S = float(self.transform().m11() or self.canvas_scale or 1.0)
        if S <= 0:
            S = 1.0

        # Поля от краёв картинки в координатах сцены
        margin_sc = float(self.side_margin) / S
        top_bound = r.top() + margin_sc
        bot_bound = r.bottom() - margin_sc

        # зазор между блоками (px->scene)
        gap_px = 8
        gap_sc = gap_px / S

        # собрать группу
        group: List[BubbleRuntime] = [
            b for b in self.bubbles.values()
            if b.img_idx == img_idx and b.side == side and b.proxy_widget is not None
        ]
        if not group:
            return

        # обеспечить измеренную высоту в px и в scene
        items = []
        scale_factor = self._aside_bubble_scale_factor()
        for b in group:
            # гарантия измерения (на случай, если не мерили)
            if b.container_widget and b.max_width:
                h_px = self._measure_container(b, b.max_width)
                b.height_px = int(h_px)
            else:
                b.height_px = max(1, int(getattr(b, "height_px", 0)) or 1)
            h_sc = (b.height_px * scale_factor) / S
            desired_cy = float(b.anchor_y)
            items.append([b, desired_cy, h_sc])

        # сортируем по желаемым центрам
        items.sort(key=lambda t: t[1])

        # прямой проход — сдвигаем вниз при необходимости
        cur_top = top_bound
        for i in range(len(items)):
            b, desired_cy, h_sc = items[i]
            top = max(desired_cy - h_sc / 2.0, cur_top)
            cy = top + h_sc / 2.0
            items[i][1] = cy  # новый центр
            cur_top = top + h_sc + gap_sc

        # обратный проход — подтягиваем вверх, чтобы влезть в низ
        cur_bottom = bot_bound
        for i in reversed(range(len(items))):
            b, cy, h_sc = items[i]
            bottom = min(cy + h_sc / 2.0, cur_bottom)
            top = bottom - h_sc
            cy = top + h_sc / 2.0
            items[i][1] = cy
            cur_bottom = top - gap_sc

        # применяем: позиция прокси и линия к новому центру
        for (b, cy, h_sc) in items:
            # x позиция уже рассчитана в _apply_bubble_imgpos; меняем только y
            if b.proxy_widget:
                # ВАЖНО: proxy позиционируется в СЦЕНЕ, а высота — в px
                b.proxy_widget.setScale(scale_factor)
                b.proxy_widget.setPos(b.proxy_widget.pos().x(), cy - ((b.height_px * scale_factor) / 2.0))
            if b.line_item:
                bx, by = self._scene_from_uv(b.img_idx, b.img_u, b.img_v)
                b.line_item.setLine(bx, by, b.line_x, cy)
            self._update_bubble_rect_visual(b.id)

    # --------- хелперы для инструментов клининга (совместимость) ---------
    def _image_bbox(self, idx: int) -> QRectF:
        if 0 <= idx < len(self.image_bboxes):
            return self.image_bboxes[idx]
        return QRectF()

    def get_original_chunk(self, idx: int, scene_rect) -> QImage:
        """
        Вырезает кусок исходного изображения по сценовому прямоугольнику.
        Используется инструментами клининга.
        """
        if not (0 <= idx < len(self.images)):
            return QImage()
        bbox = self._image_bbox(idx)
        r = QRectF(scene_rect).normalized().intersected(bbox)
        if r.isEmpty():
            return QImage()
        img = self._qimage_from(self.images[idx])
        if img.isNull() or bbox.width() <= 0 or bbox.height() <= 0:
            return QImage()
        sx = img.width() / max(1.0, bbox.width())
        sy = img.height() / max(1.0, bbox.height())
        x = int(round((r.left() - bbox.left()) * sx))
        y = int(round((r.top()  - bbox.top())  * sy))
        w = int(round(r.width()  * sx))
        h = int(round(r.height() * sy))
        x = max(0, min(x, img.width()))
        y = max(0, min(y, img.height()))
        w = max(0, min(w, img.width()  - x))
        h = max(0, min(h, img.height() - y))
        if w <= 0 or h <= 0:
            return QImage()
        return img.copy(x, y, w, h)

    def _scene_to_overlay_xy(self, idx: int, scene_pt):
        """Alias для старых инструментов."""
        return self.scene_point_to_overlay_xy(idx, scene_pt)

    def paste_chunk_to_overlay(self, idx: int, scene_rect, chunk: QImage) -> None:
        """Совместимость: раньше было в DrawingCanvasView."""
        self.replace_overlay_region(idx, scene_rect, chunk)

    def _autosave_now(self):
        if hasattr(self.project, "autosave"):
            try:
                self.project.autosave()
            except Exception as e:
                print(f"[CanvasView] autosave failed: {e}")

    def _ensure_overlay_item(self, idx: int):
        if not (0 <= idx < len(self.images)):
            return
        if self._overlay_items[idx] is None:
            it = QGraphicsPixmapItem()
            # Оверлей должен быть МЕЖДУ изображениями и пузырями
            try:
                it.setZValue(100.0)
            except Exception:
                pass
            try:
                it.setTransformationMode(Qt.TransformationMode.SmoothTransformation)
            except Exception:
                pass
            it.setCacheMode(QGraphicsItem.CacheMode.DeviceCoordinateCache)
            try:
                it.setAcceptedMouseButtons(Qt.MouseButton.NoButton)
            except Exception:
                pass
            self.scene.addItem(it)
            self._overlay_items[idx] = it

    def _ensure_all_overlays_items(self):
        for i in range(len(self.images)):
            self._ensure_overlay_item(i)

    def _apply_overlay_geom(self, idx: int):
        if not (0 <= idx < len(self.images)):
            return
        it = self._overlay_items[idx]
        if it is None:
            return
        if idx >= len(self.image_bboxes):
            return
        bbox = self.image_bboxes[idx]
        it.setPos(bbox.left(), bbox.top())
        # Масштабируем айтем, НЕ изменяя растровые данные
        ov = self.overlays_model.get(idx) if self.overlays_model else None
        if ov is None or ov.isNull() or bbox.width() <= 0 or bbox.height() <= 0:
            it.setTransform(QTransform())
            return
        sx = bbox.width()  / max(1.0, ov.width())
        sy = bbox.height() / max(1.0, ov.height())
        it.setTransform(QTransform().scale(sx, sy))

    def _sync_all_overlays_geom(self):
        if not self.overlays_model:
            return
        for i in self._visible_page_indexes():
            self._apply_overlay_geom(i)

    def _refresh_overlay_pixmap(self, idx: int):
        if not self.overlays_model or not (0 <= idx < len(self.images)):
            return
        it = self._overlay_items[idx]
        if it is None:
            return
        ov = self.overlays_model.get(idx)
        if ov is None or ov.isNull():
            it.setPixmap(QPixmap())  # пусто
        else:
            it.setPixmap(QPixmap.fromImage(ov))
        self._apply_overlay_geom(idx)
        # видимость
        vis = self.overlays_model.is_visible() if self.overlays_model else True
        it.setVisible(bool(vis))

    def _refresh_all_overlays_pixmaps(self):
        if not self.overlays_model:
            return
        for i in self._visible_page_indexes():
            self._refresh_overlay_pixmap(i)

    def _schedule_overlay_refresh(self, idx: int) -> None:
        if not self.overlays_model:
            return
        if not (0 <= idx < len(self.images)):
            return
        self._overlay_refresh_pending.add(int(idx))
        if not self._overlay_refresh_timer.isActive():
            self._overlay_refresh_timer.start()

    def _flush_overlay_refresh(self) -> None:
        if not self._overlay_refresh_pending:
            return
        pending = list(self._overlay_refresh_pending)
        self._overlay_refresh_pending.clear()
        for idx in pending:
            self._refresh_overlay_pixmap(idx)

    # --- слоты модели ---
    def _on_overlay_replaced(self, idx: int):
        if not self._tabs_autosync_enabled and not self.isVisible():
            return
        self._schedule_overlay_refresh(int(idx))

    def _on_overlay_cleared(self, idx: int):
        if not self._tabs_autosync_enabled and not self.isVisible():
            return
        self._schedule_overlay_refresh(int(idx))

    def _on_overlays_visibility_changed(self, vis: bool):
        if not self._tabs_autosync_enabled and not self.isVisible():
            return
        for it in self._overlay_items:
            if it is not None:
                it.setVisible(bool(vis))

    # --- публичный API: быстро скрыть/показать слои ---
    def set_clean_overlays_visible(self, visible: bool) -> None:
        if self.overlays_model:
            self.overlays_model.set_visible(bool(visible))

    # ------------------- overlay helpers (public) -------------------
    def overlay_image(self, idx: int) -> Optional[QImage]:
        """Вернуть QImage-слой из модели (без копии). Не изменяйте его напрямую — используйте методы ниже."""
        if not self.overlays_model:
            return None
        return self.overlays_model.get(int(idx))

    def scene_point_to_overlay_xy(self, idx: int, scene_pt) -> Tuple[int, int]:
        """
        Преобразовать координату сцены в координату слоя (px слоя).
        Основано на bbox картинки и реальном размере QImage-оверлея.
        """
        if not self.overlays_model or not (0 <= idx < len(self.image_bboxes)):
            return (0, 0)
        ov = self.overlays_model.get(idx)
        bbox = self.image_bboxes[idx]
        if ov is None or ov.isNull() or bbox.width() <= 0 or bbox.height() <= 0:
            return (0, 0)
        lx = (scene_pt.x() - bbox.left()) / max(1.0, bbox.width()) * ov.width()
        ly = (scene_pt.y() - bbox.top())  / max(1.0, bbox.height()) * ov.height()
        return int(round(lx)), int(round(ly))

    def scene_rect_to_overlay_rect(self, idx: int, scene_rect) -> "QRect":
        """
        Проецирует прямоугольник сцены в координаты QImage-оверлея (px).
        Возвращает QRect; пустой, если нет пересечения/данных.
        """
        from PyQt6.QtCore import QRect, QRectF
        if not self.overlays_model or not (0 <= idx < len(self.image_bboxes)):
            return QRect()
        ov = self.overlays_model.get(idx)
        bbox: QRectF = self.image_bboxes[idx]
        if ov is None or ov.isNull() or bbox.width() <= 0 or bbox.height() <= 0:
            return QRect()
        r = QRectF(scene_rect).normalized().intersected(bbox)
        if r.isEmpty():
            return QRect()
        sx = ov.width()  / bbox.width()
        sy = ov.height() / bbox.height()
        x = int(round((r.left() - bbox.left()) * sx))
        y = int(round((r.top()  - bbox.top())  * sy))
        w = int(round(r.width()  * sx))
        h = int(round(r.height() * sy))
        # подрезаем в границах
        x = max(0, min(x, ov.width()))
        y = max(0, min(y, ov.height()))
        w = max(0, min(w, ov.width() - x))
        h = max(0, min(h, ov.height() - y))
        return QRect(x, y, w, h)

    def replace_overlay_region(self, idx: int, scene_rect, chunk: "QImage") -> None:
        """
        Вставить изображение chunk в область scene_rect (координаты сцены) слоя idx.
        Безопасно (делает копию слоя и заменяет через модель).
        """
        from PyQt6.QtGui import QPainter, QImage
        if not self.overlays_model or chunk is None or chunk.isNull():
            return
        ov = self.overlays_model.get(idx)
        if ov is None or ov.isNull():
            return
        r = self.scene_rect_to_overlay_rect(idx, scene_rect)
        if r.isEmpty():
            return
        # подгоняем по ректу
        qimg = chunk
        if qimg.size() != r.size():
            from PyQt6.QtCore import Qt
            qimg = qimg.scaled(r.size(), Qt.AspectRatioMode.IgnoreAspectRatio,
                               Qt.TransformationMode.SmoothTransformation)
        # рисуем в копию, затем заменяем через модель (emit)
        layer = ov.copy()
        p = QPainter(layer)
        p.setCompositionMode(QPainter.CompositionMode.CompositionMode_Source)
        p.drawImage(r.topLeft(), qimg)
        p.end()
        self.overlays_model.replace(idx, layer)
        if self.overlays_model and self.overlays_model.updates_locked():
            self._refresh_overlay_pixmap(idx)

    def paint_overlay_segment(self, idx: int, sp0, sp1, *, color, radius: int, erase: bool = False) -> None:
        """
        Нарисовать отрезок на слое idx. sp0, sp1 — точки сцены (QPointF).
        Рисование производится в копию слоя, затем model.replace(...).
        """
        from PyQt6.QtGui import QPainter, QPen, QColor
        if not self.overlays_model:
            return
        ov = self.overlays_model.get(idx)
        if ov is None or ov.isNull():
            return
        x0, y0 = self.scene_point_to_overlay_xy(idx, sp0)
        x1, y1 = self.scene_point_to_overlay_xy(idx, sp1)
        layer = ov.copy()
        p = QPainter(layer)
        p.setRenderHints(QPainter.RenderHint.Antialiasing | QPainter.RenderHint.SmoothPixmapTransform)
        if erase:
            p.setCompositionMode(QPainter.CompositionMode.CompositionMode_Clear)
            pen = QPen(QColor(0, 0, 0, 0), max(1, int(radius) * 2),
                       Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap, Qt.PenJoinStyle.RoundJoin)
        else:
            p.setCompositionMode(QPainter.CompositionMode.CompositionMode_SourceOver)
            pen = QPen(color, max(1, int(radius) * 2),
                       Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap, Qt.PenJoinStyle.RoundJoin)
        p.setPen(pen)
        p.drawLine(x0, y0, x1, y1)
        p.end()
        self.overlays_model.replace(idx, layer)
        if self.overlays_model and self.overlays_model.updates_locked():
            self._refresh_overlay_pixmap(idx)

    def clear_overlay_index(self, idx: int) -> None:
        """Полностью очистить слой idx (через модель)."""
        if self.overlays_model:
            self.overlays_model.clear(int(idx))
