from __future__ import annotations

from typing import Callable, Dict, Any
import traceback

from PyQt6.QtCore import QObject
from PyQt6.QtWidgets import QWidget


class OcrEngineBase(QObject):
    """
    Базовый класс для OCR движков во вкладке перевода.

    Движок отвечает за собственный UI, загрузку моделей и распознавание.
    SettingsPanel только вставляет его UI и вызывает validate/load/recognize.
    """

    key: str = "base"
    title: str = "Base"
    checkbox_label: str = "Base"

    def __init__(self, canvas):
        super().__init__()
        self.canvas = canvas
        self._ui: QWidget | None = None
        self._on_change: Callable[[], None] | None = None
        self._loaded: bool = False

    # --- UI -----------------------------------------------------
    def build_ui(self, parent: QWidget, on_change: Callable[[], None]) -> QWidget:
        """
        Создает и возвращает корневой виджет с настройками движка.
        Реализация должна вызвать on_change при любом изменении параметров.
        """
        raise NotImplementedError

    def ui(self, parent: QWidget, on_change: Callable[[], None]) -> QWidget:
        """
        Возвращает (и при необходимости создает) UI движка.
        """
        if self._ui is None:
            self._ui = self.build_ui(parent, on_change)
            self._on_change = on_change
        return self._ui

    # --- Сохранение / загрузка параметров -----------------------
    def read_ui_state(self):
        """Читает значения из UI и кладет во внутренние поля."""
        pass

    def validate(self) -> bool:
        """Проверяет корректность настроек (например, язык в списке поддерживаемых)."""
        return True

    def save_settings(self) -> Dict[str, Any]:
        """Сериализация параметров для project_settings.OCR.params."""
        return {}

    def load_settings(self, data: Dict[str, Any]):
        """Восстановление параметров из project_settings.OCR.params."""
        pass

    def reset(self):
        """Сбрасывает состояние и принудительно пересоздает модели при следующей загрузке."""
        self._loaded = False

    # --- Загрузка и распознавание -------------------------------
    def ensure_loaded(self) -> bool:
        if self._loaded:
            return True
        try:
            self._loaded = bool(self._load_impl())
        except Exception:
            traceback.print_exc()
            self._loaded = False
        return self._loaded

    def _load_impl(self) -> bool:
        """Реальная загрузка модели. Возвращает True при успехе."""
        raise NotImplementedError

    def warmup(self):
        """Опциональный прогрев для ленивого скачивания моделей."""
        pass

    def recognize(self, qimage, join_newlines: bool, reflect_strings: bool) -> str:
        """Распознавание области изображения. Возвращает текст (может быть пустой)."""
        raise NotImplementedError
