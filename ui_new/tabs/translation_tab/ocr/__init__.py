from .easy import EasyOcrEngine
from .manga import MangaOcrEngine
from .paddle import PaddleOcrEngine


def create_engines(canvas):
    engines = [EasyOcrEngine(canvas), PaddleOcrEngine(canvas), MangaOcrEngine(canvas)]
    return {e.key: e for e in engines}
