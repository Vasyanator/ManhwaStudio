# Публичный API модуля translation_tab
# Поддерживает обратную совместимость с прежним монолитным импортом:
# from ui_new.tabs.translation_tab import TranslationTab
# from ui_new.tabs.translation_tab import TranslationCanvasView

from .tab import TranslationTab
from .canvas import TranslationCanvasView
from .utils import ImageLike

__all__ = ["TranslationTab", "TranslationCanvasView", "ImageLike"]
