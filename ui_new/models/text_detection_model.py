from __future__ import annotations

from typing import Dict, List, Optional, Tuple

from PyQt6.QtCore import QObject, pyqtSignal


class TextDetectionModel(QObject):
    """
    Лёгкая in-memory модель для результатов детектора текста.
    Хранит маску, список блоков/линий и базовый размер страницы по индексу.
    """

    resultChanged = pyqtSignal(int)  # idx
    cleared = pyqtSignal(int)        # idx
    reset = pyqtSignal()             # все страницы обновлены/очищены
    optionsChanged = pyqtSignal(dict)  # {'block_expand_px': int, 'merge_gap_px': int, 'merge_nearby': bool}

    def __init__(self, project):
        super().__init__()
        self.project = project
        self._pages: List[Optional[Dict]] = []
        self._options: Dict[str, object] = {
            "block_expand_px": 0,
            "merge_gap_px": 0,
            "merge_nearby": False,
        }

    def init_from_images(self, image_paths: List[str]) -> None:
        """Привязываем количество страниц и сбрасываем данные детектора."""
        self._pages = [None] * len(image_paths)
        self.reset.emit()

    def count(self) -> int:
        return len(self._pages)

    def set_result(self, idx: int, mask=None, blocks=None, size: Optional[Tuple[int, int]] = None) -> None:
        """Сохраняет результаты детекции для страницы idx."""
        if not (0 <= idx < len(self._pages)):
            return
        self._pages[idx] = {
            "mask": mask,
            "blocks": blocks,
            "size": size,
        }
        self.resultChanged.emit(idx)

    def clear(self, idx: int) -> None:
        """Удаляет результаты детекции для страницы idx."""
        if not (0 <= idx < len(self._pages)):
            return
        self._pages[idx] = None
        self.cleared.emit(idx)

    def clear_all(self) -> None:
        """Очищает все страницы, сохраняя длину списка."""
        if not self._pages:
            return
        self._pages = [None] * len(self._pages)
        self.reset.emit()

    def get(self, idx: int) -> Optional[Dict]:
        if 0 <= idx < len(self._pages):
            return self._pages[idx]
        return None

    def as_dict(self) -> Dict[int, Dict]:
        """Возвращает словарь {idx: result} только для непустых страниц."""
        return {i: v for i, v in enumerate(self._pages) if v}

    # ------ опции отображения ------
    def set_options(self, *, block_expand_px: Optional[int] = None, merge_gap_px: Optional[int] = None,
                    merge_nearby: Optional[bool] = None) -> None:
        updated = False
        if block_expand_px is not None:
            try:
                v = max(0, int(block_expand_px))
                if v != self._options["block_expand_px"]:
                    self._options["block_expand_px"] = v
                    updated = True
            except Exception:
                pass
        if merge_gap_px is not None:
            try:
                v = max(0, int(merge_gap_px))
                if v != self._options["merge_gap_px"]:
                    self._options["merge_gap_px"] = v
                    updated = True
            except Exception:
                pass
        if merge_nearby is not None:
            v = bool(merge_nearby)
            if v != self._options["merge_nearby"]:
                self._options["merge_nearby"] = v
                updated = True
        if updated:
            self.optionsChanged.emit(self.get_options())

    def get_options(self) -> Dict[str, object]:
        return dict(self._options)
