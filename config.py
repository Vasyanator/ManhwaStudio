"""
FILE OVERVIEW: config.py
Shared runtime configuration for Python launcher/tools.

Main items:
- Global path constants for project assets and model folders.
- `BaseUserConfig` / `NestedConfig`: user settings wrapper over `docstore.py`
  (`user_config.json` in Dev mode, `user_config.db` in Prod mode). Read-only on
  import; every write is a serialized `update(mutator)` of the on-disk document.
- `get_projects_root` / `set_projects_root`: canonical projects folder resolver
  (uses `user_config -> General.projects_dir`, default `{Documents}/manhwastudio_projects`).
"""

import copy
import logging
import os
import threading
from pathlib import Path
from typing import Any, Callable, Dict, Optional, Tuple

import docstore

_log = logging.getLogger(__name__)
VERSION = "3.6.0"


def _default_documents_dir() -> Optional[Path]:
    if os.name == "nt":
        profile = os.environ.get("USERPROFILE")
        if profile:
            return Path(profile) / "Documents"
        return None
    home = os.environ.get("HOME")
    if home:
        return Path(home) / "Documents"
    return None


def default_projects_root() -> Path:
    base = _default_documents_dir() or Path(__file__).resolve().parent
    return base / "manhwastudio_projects"


def normalize_projects_root(raw_path: Any) -> str:
    text = str(raw_path or "").strip()
    if not text:
        return os.fspath(default_projects_root())
    return os.fspath(Path(text))


# Папка, где лежат все проекты (legacy-константа; актуальное значение через get_projects_root())
program_dir = Path(__file__).resolve().parent
script_dir = os.path.dirname(os.path.abspath(__file__))
PROJECTS_ROOT = os.fspath(default_projects_root())
# Проект по умолчанию
DEFAULT_PROJECT = ""#os.path.join(PROJECTS_ROOT, "Сегодня я буду _", "ch20")
DEBUG_CONSOLE = False
# Имена файлов внутри проекта
BUBBLES_FILE = "translation_bubbles.json"
NOTES_FILE = "translation_notes.txt"
SRC_DIR = "src"
CLEANED_DIR = "cleaned"
CLEAN_LAYERS_DIR = "clean_layers"
ALT_VERS_DIR = "alt_vers"
SAVED_DIR = "saved"
TEXT_IMAGES_DIR = "text_images"
CHARACTERS_DIR = "characters"
TERMS_FILE = "terms.json"
PROJECT_SETTINGS_FILE = "settings.json"


# Папки для ИИ моделей, управляемых кодом ManhwaStudio.
# EasyOCR/Surya не перечислены здесь: их модели скачиваются самими библиотеками.
MODELS_DIR = os.path.join(program_dir, "ManhwaStudio_AI_Models")
TORCH_MODELS_DIR = os.path.join(MODELS_DIR, "Torch")
ONNX_MODELS_DIR = os.path.join(MODELS_DIR, "ONNX")
LAMA_DIR = os.path.join(TORCH_MODELS_DIR, "LaMa")
LAMA_MPE_DIR = os.path.join(TORCH_MODELS_DIR, "LaMa_MPE")
AOT_DIR = os.path.join(TORCH_MODELS_DIR, "AOT")
TEXT_DETECTOR_DIR = os.path.join(TORCH_MODELS_DIR, "ComicTextDetector")
TEXT_DETECTOR_ONNX_DIR = os.path.join(ONNX_MODELS_DIR, "ComicTextDetector")
PADDLEOCR_DIR = os.path.join(ONNX_MODELS_DIR, "PaddleOCR")
PADDLEOCR_DET_DIR = os.path.join(PADDLEOCR_DIR, "detection")
PADDLEOCR_REC_DIR = os.path.join(PADDLEOCR_DIR, "languages")
MANGAOCR_DIR = os.path.join(ONNX_MODELS_DIR, "MangaOCR")
# Сторонние крупные модели (качаются по требованию, не из основного репозитория).
SIDE_MODELS_DIR = os.path.join(MODELS_DIR, "side_models")
# FLUX.1-Fill-dev: GGUF-трансформер (квант выбирается) + diffusers-компоненты
# (VAE/CLIP/T5/scheduler) в подпапке components/.
FLUX_FILL_DIR = os.path.join(SIDE_MODELS_DIR, "FLUX.1-Fill-dev-GGUF")
FLUX_FILL_COMPONENTS_DIR = os.path.join(FLUX_FILL_DIR, "components")
# Удаление видимых водяных знаков: веса и загружаемый в рантайме код сетей
# (SLBR / WDNet / SplitNet) в подпапках <модель>/ и <модель>/src/.
WATERMARK_DIR = os.path.join(SIDE_MODELS_DIR, "WatermarkRemoval")
folders = [
    LAMA_DIR,
    os.path.join(LAMA_DIR, "models"),
    LAMA_MPE_DIR,
    AOT_DIR,
    TEXT_DETECTOR_DIR,
    TEXT_DETECTOR_ONNX_DIR,
    PADDLEOCR_DET_DIR,
    PADDLEOCR_REC_DIR,
    MANGAOCR_DIR,
    SIDE_MODELS_DIR,
    FLUX_FILL_DIR,
    FLUX_FILL_COMPONENTS_DIR,
    WATERMARK_DIR,
]
for folder in folders:
    if not os.path.exists(folder):
        os.makedirs(folder)
        print(f"Создана папка: {folder}")

class NestedConfig:
    """Dot-access view of a nested section of a `BaseUserConfig`.

    Reading returns the in-memory value (document merged with defaults). Assigning an
    attribute persists ONLY that key through `BaseUserConfig.update`.
    """

    def __init__(self, root: "BaseUserConfig", data: Dict[str, Any], keys: Tuple[str, ...]):
        object.__setattr__(self, "_root", root)  # owning config, performs the persistence
        object.__setattr__(self, "_data", data)  # in-memory section dict
        object.__setattr__(self, "_keys", keys)  # path of this section from the document root

    def __getattr__(self, item):
        value = self._data.get(item)
        if isinstance(value, dict):
            return NestedConfig(self._root, value, self._keys + (item,))
        return value

    def __setattr__(self, key, value):
        path = self._keys + (key,)
        self._root.update(lambda document: docstore.set_path(document, path, value))

    def __repr__(self):
        return repr(self._data)


class BaseUserConfig:
    """The Python backend's view of the user_config document (`.json` in Dev, `.db` in Prod).

    Contract:
    - Construction only READS: a missing or unreadable document leaves `config` equal to
      the defaults in memory and is logged; nothing is ever written on import.
    - `config` = the document merged with `defaults` (missing keys only), in memory only.
    - Writes go exclusively through `update(mutator)`: a serialized read-modify-write of
      the CURRENT on-disk document (see `docstore.update_document`), after which `config`
      is refreshed from that document. The mutator must touch only its own keys.
    """

    def __init__(self, path: str, defaults: Dict[str, Any]):
        self.path = path
        self.stem = docstore.stem_of(Path(path))
        self.defaults = defaults
        self.config = {}
        self._lock = threading.RLock()

        self._load()
        self._apply_defaults()

    def _load(self):
        """Loads the document into `config`; on absence or any error keeps `{}` (defaults follow)."""
        try:
            document = docstore.read_document(self.stem)
        except docstore.DocStoreError as exc:
            _log.error(
                "Could not read user settings; using built-in defaults for this session. "
                "Stem: %s. Error kind: %s. Error: %s. The file is left untouched.",
                self.stem, exc.kind.value, exc,
            )
            self.config = {}
            return
        if document is None:
            _log.warning("User settings document %s(.json|.db) does not exist; using built-in defaults.", self.stem)
            self.config = {}
        elif not isinstance(document, dict):
            _log.error("User settings document %s is not a JSON object (%s); using built-in defaults.", self.stem, type(document).__name__)
            self.config = {}
        else:
            self.config = document

    def _apply_defaults(self):
        def merge(d, default):
            for k, v in default.items():
                if k not in d:
                    d[k] = copy.deepcopy(v)
                elif isinstance(d[k], dict) and isinstance(v, dict):
                    merge(d[k], v)
        merge(self.config, self.defaults)

    def update(self, mutator: Callable[[Dict[str, Any]], None]) -> None:
        """Persists `mutator` applied to the current on-disk document, then refreshes `config`.

        On a store failure other than a failing mutator, the mutator is still applied to the
        in-memory `config` (so this session honours the choice); every failure is logged and
        `docstore.DocStoreError` is re-raised for the caller to report.
        """
        with self._lock:
            captured: Dict[str, Any] = {}

            def capture(document: Dict[str, Any]) -> None:
                mutator(document)
                captured["document"] = copy.deepcopy(document)

            try:
                docstore.update_document(self.stem, capture)
            except docstore.DocStoreError as exc:
                _log.error(
                    "Could not save user settings. Stem: %s. Error kind: %s. Error: %s. "
                    "The change applies to this session only.",
                    self.stem, exc.kind.value, exc,
                )
                if exc.kind is not docstore.ErrorKind.MUTATOR:
                    mutator(self.config)
                raise
            self.config = captured["document"]
            self._apply_defaults()

    def __getattr__(self, item):
        value = self.config.get(item)
        if isinstance(value, dict):
            return NestedConfig(self, value, (item,))
        return value

    def __setattr__(self, key, value):
        if key in {"path", "stem", "defaults", "config", "_lock"}:
            return super().__setattr__(key, value)

        self.update(lambda document: docstore.set_path(document, (key,), value))

# --------- ГЛОБАЛЬНАЯ КОНФИГУРАЦИЯ ---------
USER_CONFIG_DEFAULTS = {
    "General":{
        "theme": "dark",
        "style": "default",  # "default" - стандартный PyQt стиль
        "projects_dir": os.fspath(default_projects_root()),
        "ai_device": "not-selected",
        "ai_onnx_provider": "not-selected",
        "ai_onnx_device_id": "not-selected",
        "ai_max_loaded_models": 3,
        "open_page_last_title": "",
        "open_page_last_chapter": "",
        "enabled_tabs": {
            "Перевод": True,
            "Клининг": True,
            "Текст": True,
            "Персонажи": True,
            "Термины": True,
            "Заметки перевода": True,
            "Вики": True
        }
    },
    "Canvas": {
        "visible_page_radius": 2,
        "bubble_load_delay_ms": 260,
        "load_all_bubbles": False,
        "opengl_enabled": False,
        "opengl_device": "auto"
    },
    "NewProjectWindow":{ 
        "ImageUrlPrefs": {
            "mto.to": "https://*.mb*.org/media/",
            "Kakao page-edge": "https://page-edge.kakao.com/sdownload/resource*",
            "Naver CDN (generic)": "https://image-comic.pstatic.net/webtoon/*",
            "funbe": "https://funbe*.com/data/file/wtoon/*",
            "rumanhua.com": "https://p*-zhuxiaobang-sign.shimolife.com/*",
            "webtoons.com": "https://webtoon-phinf.pstatic.net/*"
        }
    },
    "TranslarionTab":{
        "TextDetector":{
            "draw_lines": True,
            "draw_mask": True,
            "block_expand_px": 0,
            "merge_close": False,
            "merge_gap_px": 5,
            "params": {
                "device": "cpu",
                "detect_size": 1280,
                "det_rearrange_max_batches": 4,
                "font size multiplier": 1.0,
                "font size max": -1.0,
                "font size min": -1.0,
                "mask dilate size": 2
            }
        },
        "MachineTranslation":{
            "service": "google",
            "source_lang": "auto",
            "target_lang": "ru",
            "threads": 1,
            "params": {
                "google": {},
                "chatgpt": {
                    "api_key": "",
                    "model": "gpt-3.5-turbo",
                    "api_base": ""
                },
                "microsoft": {
                    "api_key": "",
                    "region": ""
                },
                "yandex": {
                    "api_key": "",
                    "format_": "plain"
                },
                "deepl": {
                    "api_key": "",
                    "use_free_api": True
                }
            }
        }
    },
    "CleaningTab":{},
    "TextTab":{
        "use_system_fonts": False
    }
}
PROJECT_CONFIG_DEFAULTS = {
    "bubble_type": "aside",
    "page_spacing_px": 200,
    "visible_page_radius": 2,
    "bubble_load_delay_ms": 260,
    "opengl_enabled": False,
    "opengl_device": "auto",
    "canvas": {
        "bubble_type": "aside",
        "show_bubble_status": False,
        "aside_min_width_px": 450,
        "aside_max_width_px": 550,
        "page_spacing_px": 200,
        "vertical_edge_margin_px": 200,
        "auto_insert_last_character": True,
        "visible_page_radius": 2,
        "bubble_load_delay_ms": 260,
        "opengl_enabled": False,
        "opengl_device": "auto"
    },
    "OCR":{
        "engine": "paddle",
        "params": {
            "easyocr": {
                "langs": "korean",
                "gpu": False
            },
            "paddle": {
                "langs": "korean",
                "gpu": False
            },
            "none": {}
        },
        "join": True,
        "reflect": False,
        "copy": False,
        "bubbles": True
    },
    "composition":{
        "method": "height",
        "source_mode": "original",
        "ignore_translated_lines": True,
        "merge_same_character": True,
        "sep_same_character": "\\n",
        "sep_between": "\\n\\n",
        "replica_prefix": "",
        "nl_replace": " ",
        "nl_replace_enabled": True,
        "wrap_with": "``",
        "wrap_with_enabled": True,
        "limit": 700,
        "limit_enabled": True,
        "use_character_names": True
    },
    "machine_translation":{
        "service": "google",
        "source_lang": "auto",
        "target_lang": "ru",
        "threads": 1,
        "params": {
            "google": {},
            "chatgpt": {
                "api_key": "",
                "model": "gpt-3.5-turbo",
                "api_base": ""
            },
            "microsoft": {
                "api_key": "",
                "region": ""
            },
            "yandex": {
                "api_key": "",
                "format_": "plain"
            },
            "deepl": {
                "api_key": "",
                "use_free_api": True
            }
        }
    }
}
UserConfig = BaseUserConfig("user_config.json", USER_CONFIG_DEFAULTS)


def get_projects_root() -> str:
    try:
        general = getattr(UserConfig, "General", None)
        configured = getattr(general, "projects_dir", None) if general is not None else None
    except Exception:
        configured = None
    return normalize_projects_root(configured)


def set_projects_root(new_path: str) -> str:
    """Persists `General.projects_dir` (normalized) and returns it.

    Raises `docstore.DocStoreError` when the settings document cannot be updated (it is
    never created here); the in-memory value is updated either way.
    """
    normalized = normalize_projects_root(new_path)
    UserConfig.update(lambda document: docstore.set_path(document, ("General", "projects_dir"), normalized))
    return normalized


# Синхронизация legacy-константы для совместимости с кодом, который всё ещё читает PROJECTS_ROOT.
PROJECTS_ROOT = get_projects_root()
