import traceback
# models/bubbles_model.py

from PyQt6.QtCore import QObject, QTimer, pyqtSignal

class BubblesModel(QObject):
    bubbleCreated = pyqtSignal(dict, str)   # rec, origin_uid
    bubbleUpdated = pyqtSignal(dict, str)   # rec, origin_uid
    bubbleDeleted = pyqtSignal(int,  str)   # bid, origin_uid
    bubbleUnplaced = pyqtSignal(int,  str)  # bid, origin_uid
    bulkReset     = pyqtSignal(list, str)   # список всех записей, origin_uid
    bubbleTypeChanged = pyqtSignal(str, str)  # bubble_type, origin_uid
    asideWidthLimitsChanged = pyqtSignal(int, int, str)  # min_px, max_px, origin_uid
    pageSpacingChanged = pyqtSignal(int, str)  # spacing_px, origin_uid
    separatePagesChanged = pyqtSignal(bool, str)  # enabled, origin_uid
    verticalEdgeMarginChanged = pyqtSignal(int, str)  # margin_px, origin_uid
    scaleBubblesChanged = pyqtSignal(bool, str)  # enabled, origin_uid
    visiblePageRadiusChanged = pyqtSignal(int, str)  # radius, origin_uid
    bubbleLoadDelayChanged = pyqtSignal(int, str)  # delay_ms, origin_uid
    tabsAutoSyncChanged = pyqtSignal(bool, str)  # enabled, origin_uid
    tabsSyncRequested = pyqtSignal(str)  # origin_uid

    def __init__(self, project, parent=None):
        super().__init__(parent)
        self.project = project
        self.project.bubbles = list(getattr(self.project, "bubbles", []) or [])
        self._autosave_timer = QTimer(self)
        self._autosave_timer.setInterval(800)
        self._autosave_timer.setSingleShot(True)
        self._autosave_timer.timeout.connect(self._autosave_now)

    # ---------- CRUD API ----------
    def create(self, rec: dict, origin: str):
        """Добавить (или заменить по id) и оповестить слушателей."""
        rec = dict(rec)
        rec['id'] = int(rec['id'])
        # если уже существует — обновим inplace
        for i, e in enumerate(self.project.bubbles):
            if int(e.get('id')) == rec['id']:
                self.project.bubbles[i].update(rec)
                self._autosave()
                self.bubbleCreated.emit(self.project.bubbles[i], origin)
                return
        # иначе добавим новую
        self.project.bubbles.append(rec)
        self._autosave()
        self.bubbleCreated.emit(rec, origin)

    def update(self, patch: dict, origin: str):
        """Найти по id и обновить поля, затем оповестить."""
        bid = int(patch['id'])
        for e in self.project.bubbles:
            if int(e.get('id')) == bid:
                e.update({k: v for k, v in patch.items() if k != 'id'})
                self._autosave()
                # можно слать всю запись — так View восстановит контекст
                self.bubbleUpdated.emit(dict(e), origin)
                return

    def delete(self, bid: int, origin: str):
        bid = int(bid)
        for i, e in enumerate(self.project.bubbles):
            if int(e.get('id')) == bid:
                del self.project.bubbles[i]
                self._autosave()
                self.bubbleDeleted.emit(bid, origin)
                return

    def unplace(self, bid: int, origin: str):
        bid = int(bid)
        for e in self.project.bubbles:
            if int(e.get('id')) == bid:
                e.update({'img_idx': None, 'img_u': None, 'img_v': None, 'side': None})
                break
        self._autosave()
        self.bubbleUnplaced.emit(bid, origin)

    def reset(self, records: list[dict], origin: str):
        """(Опционально) Полная замена (импорт/загрузка)."""
        self.project.bubbles = [dict(r) for r in records]
        self._autosave()
        self.bulkReset.emit(self.project.bubbles, origin)

    def set_bubble_type(self, bubble_type: str, origin: str):
        bt = "on_top" if bubble_type == "on_top" else "aside"
        if getattr(self.project, "bubble_type", None) == bt:
            return
        self.project.bubble_type = bt
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
        self._autosave()
        self.bubbleTypeChanged.emit(bt, origin)

    def set_aside_width_limits(self, min_px: int, max_px: int, origin: str):
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

        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        current_min = None
        current_max = None
        try:
            current_min = int(getattr(canvas_settings, "aside_min_width_px", min_px)) if canvas_settings else None
        except Exception:
            current_min = None
        try:
            current_max = int(getattr(canvas_settings, "aside_max_width_px", max_px)) if canvas_settings else None
        except Exception:
            current_max = None
        if current_min == min_px and current_max == max_px:
            return

        if canvas_settings:
            try:
                canvas_settings.aside_min_width_px = min_px
            except Exception:
                pass
            try:
                canvas_settings.aside_max_width_px = max_px
            except Exception:
                pass
        if settings:
            try:
                settings.aside_min_width_px = min_px
            except Exception:
                pass
            try:
                settings.aside_max_width_px = max_px
            except Exception:
                pass
        self.project.aside_min_width_px = min_px
        self.project.aside_max_width_px = max_px

        self._autosave()
        self.asideWidthLimitsChanged.emit(min_px, max_px, origin)

    def set_page_spacing(self, spacing_px: int, origin: str):
        try:
            spacing_px = int(spacing_px)
        except Exception:
            spacing_px = 200
        spacing_px = max(0, spacing_px)

        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        current = None
        try:
            current = int(getattr(canvas_settings, "page_spacing_px", spacing_px)) if canvas_settings else None
        except Exception:
            current = None
        if current == spacing_px:
            return

        if canvas_settings:
            try:
                canvas_settings.page_spacing_px = spacing_px
            except Exception:
                pass
        if settings:
            try:
                settings.page_spacing_px = spacing_px
            except Exception:
                pass
        self.project.page_spacing_px = spacing_px

        self._autosave()
        self.pageSpacingChanged.emit(spacing_px, origin)

    def set_separate_pages(self, enabled: bool, origin: str):
        enabled = bool(enabled)

        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        current = None
        if canvas_settings:
            try:
                val = getattr(canvas_settings, "separate_pages", None)
                if val is not None:
                    current = bool(val)
            except Exception:
                current = None
        if current is None:
            if settings:
                try:
                    val = getattr(settings, "separate_pages", None)
                    if val is not None:
                        current = bool(val)
                except Exception:
                    current = None
        if current is None:
            try:
                val = getattr(self.project, "separate_pages", None)
                if val is not None:
                    current = bool(val)
            except Exception:
                current = None
        if current == enabled:
            return

        if canvas_settings:
            try:
                canvas_settings.separate_pages = enabled
            except Exception:
                pass
        if settings:
            try:
                settings.separate_pages = enabled
            except Exception:
                pass
        self.project.separate_pages = enabled

        self._autosave()
        self.separatePagesChanged.emit(enabled, origin)

    def set_vertical_edge_margin(self, margin_px: int, origin: str):
        try:
            margin_px = int(margin_px)
        except Exception:
            margin_px = 200
        margin_px = max(0, margin_px)

        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        current = None
        try:
            current = int(getattr(canvas_settings, "vertical_edge_margin_px", margin_px)) if canvas_settings else None
        except Exception:
            current = None
        if current is None:
            try:
                current = int(getattr(self.project, "vertical_edge_margin_px", margin_px))
            except Exception:
                current = None
        if current == margin_px:
            return

        if canvas_settings:
            try:
                canvas_settings.vertical_edge_margin_px = margin_px
            except Exception:
                pass
        self.project.vertical_edge_margin_px = margin_px

        self._autosave()
        self.verticalEdgeMarginChanged.emit(margin_px, origin)

    def set_scale_bubbles(self, enabled: bool, origin: str):
        enabled = bool(enabled)

        settings = getattr(self.project, "settings", None)
        canvas_settings = getattr(settings, "canvas", None) if settings else None
        current = None
        try:
            current = bool(getattr(canvas_settings, "scale_bubbles", enabled)) if canvas_settings else None
        except Exception:
            current = None
        if current is None:
            try:
                current = bool(getattr(self.project, "scale_bubbles", enabled))
            except Exception:
                current = None
        if current == enabled:
            return

        if canvas_settings:
            try:
                canvas_settings.scale_bubbles = enabled
            except Exception:
                pass
        if settings:
            try:
                settings.scale_bubbles = enabled
            except Exception:
                pass
        self.project.scale_bubbles = enabled

        self._autosave()
        self.scaleBubblesChanged.emit(enabled, origin)

    def set_visible_page_radius(self, radius: int, origin: str):
        try:
            radius = int(radius)
        except Exception:
            radius = 2
        radius = max(0, min(50, radius))

        current = None
        try:
            current = int(getattr(self.project, "visible_page_radius", radius))
        except Exception:
            current = None
        if current == radius:
            return
        self.project.visible_page_radius = radius
        self.visiblePageRadiusChanged.emit(radius, origin)

    def set_bubble_load_delay_ms(self, delay_ms: int, origin: str):
        try:
            delay_ms = int(delay_ms)
        except Exception:
            delay_ms = 260
        delay_ms = max(0, min(5000, delay_ms))

        current = None
        try:
            current = int(getattr(self.project, "bubble_load_delay_ms", delay_ms))
        except Exception:
            current = None
        if current == delay_ms:
            return
        self.project.bubble_load_delay_ms = delay_ms
        self.bubbleLoadDelayChanged.emit(delay_ms, origin)

    def set_tabs_autosync(self, enabled: bool, origin: str):
        enabled = bool(enabled)
        current = None
        try:
            val = getattr(self.project, "tabs_auto_sync", None)
            if val is not None:
                current = bool(val)
        except Exception:
            current = None
        if current == enabled:
            return
        self.project.tabs_auto_sync = enabled
        self.tabsAutoSyncChanged.emit(enabled, origin)

    def request_tabs_sync(self, origin: str):
        self.tabsSyncRequested.emit(origin)

    # ---------- helpers ----------
    def _autosave(self):
        if self._autosave_timer.isActive():
            self._autosave_timer.start()
            return
        self._autosave_timer.start()

    def _autosave_now(self):
        if hasattr(self.project, "autosave"):
            try:
                self.project.autosave()
            except Exception as e:
                traceback.print_exc()
                print(f"[BubblesModel] autosave failed: {e}")
