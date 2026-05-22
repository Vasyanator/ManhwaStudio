from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/fetch_from_browser.py
Узел `fetch_from_browser` для массовой обработки.

Main items:
- `FetchFromBrowserParamsWidget`: выбор шаблона (пресет сайта + wildcard-префикс).
- `create_node`: фабрика узла `Выкачать из браузера` (exec -> exec + image_list).
"""

from typing import Optional

from PyQt6 import QtWidgets

from config import UserConfig

from ...downloaders import _DEFAULT_LINK_PREFIX
from ..constants import KIND_DATA, KIND_EXEC, TYPE_IMAGE_LIST
from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, SocketSpec


TEMPLATE = NodeTemplate(
    "fetch_from_browser",
    "Выкачать из браузера",
    "Браузер",
    "Скачивает картинки из текущей вкладки по шаблону ссылок, как продвинутый выкачиватель.",
)


class FetchFromBrowserParamsWidget(QtWidgets.QWidget):
    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        layout = QtWidgets.QFormLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        self.site_combo = QtWidgets.QComboBox(self)
        self.pattern_edit = QtWidgets.QLineEdit(self)
        self.pattern_edit.setPlaceholderText("Префикс или wildcard-шаблон ссылок")
        self.pattern_edit.setText(_DEFAULT_LINK_PREFIX)

        layout.addRow("Сайт (пресет):", self.site_combo)
        layout.addRow("Шаблон:", self.pattern_edit)

        self._reload_site_presets()
        self.site_combo.currentTextChanged.connect(self._on_site_changed)

    def selected_pattern(self) -> str:
        return (self.pattern_edit.text() or "").strip()

    def _reload_site_presets(self) -> None:
        self.site_combo.clear()
        self.site_combo.addItem("")
        self.site_combo.addItem("Авто (jpg/png/webp)")
        nested = self._prefs_nested()
        if nested is None:
            return
        for name in sorted(self._cfg_keys(nested)):
            self.site_combo.addItem(name)

    def _on_site_changed(self, site_name: str) -> None:
        site_name = (site_name or "").strip()
        if not site_name:
            return
        if site_name == "Авто (jpg/png/webp)":
            self.pattern_edit.clear()
            return
        nested = self._prefs_nested()
        if nested is None:
            return
        pref = self._cfg_get(nested, site_name, "")
        if pref:
            self.pattern_edit.setText(pref)

    @staticmethod
    def _prefs_nested():
        try:
            return UserConfig.NewProjectWindow.ImageUrlPrefs
        except Exception:
            return None

    @staticmethod
    def _cfg_keys(nested) -> list[str]:
        try:
            if hasattr(nested, "_data") and isinstance(nested._data, dict):
                return list(nested._data.keys())
        except Exception:
            pass
        try:
            return [k for k in dir(nested) if not k.startswith("_") and isinstance(getattr(nested, k), (str, bytes))]
        except Exception:
            return []

    @staticmethod
    def _cfg_get(nested, key: str, default: str = "") -> str:
        try:
            if hasattr(nested, "_data") and isinstance(nested._data, dict):
                value = nested._data.get(key, default)
                if isinstance(value, str):
                    return value
                return default
        except Exception:
            pass
        try:
            if hasattr(nested, key):
                value = getattr(nested, key)
                if isinstance(value, str):
                    return value
        except Exception:
            pass
        try:
            value = nested[key]  # type: ignore[index]
            if isinstance(value, str):
                return value
        except Exception:
            pass
        return default


def create_node() -> NodeBlockItem:
    return NodeBlockItem(
        "Выкачать из браузера",
        [
            SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
            SocketSpec("Далее", "out", KIND_EXEC),
            SocketSpec("Картинки", "out", KIND_DATA, data_type=TYPE_IMAGE_LIST),
        ],
        params_widget=FetchFromBrowserParamsWidget(),
        description="Скачивает картинки из текущей вкладки браузера",
        width=360.0,
    )
