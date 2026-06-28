"""
FILE OVERVIEW: config.py
Shared runtime configuration for Python launcher/tools.

Main items:
- Global path constants for project assets and model folders.
- `BaseUserConfig` / `NestedConfig`: JSON-backed user settings wrapper.
- `get_projects_root` / `set_projects_root`: canonical projects folder resolver
  (uses `user_config.json -> General.projects_dir`, default `{Documents}/manhwastudio_projects`).
"""

import os
from pathlib import Path
import json
from typing import Any, Dict, Optional
VERSION = "3.5.0"


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
]
for folder in folders:
    if not os.path.exists(folder):
        os.makedirs(folder)
        print(f"Создана папка: {folder}")

class NestedConfig:
    """Обёртка для вложенных словарей с доступом через точку."""

    def __init__(self, root, data):
        self._root = root  # ссылка на UserConfig для сохранения
        self._data = data  # реальный словарь

    def __getattr__(self, item):
        value = self._data.get(item)
        if isinstance(value, dict):
            return NestedConfig(self._root, value)
        return value

    def __setattr__(self, key, value):
        if key in {"_root", "_data"}:
            return super().__setattr__(key, value)

        self._data[key] = value
        self._root.save()

    def __repr__(self):
        return repr(self._data)
    
class BaseUserConfig:
    def __init__(self, path: str, defaults: Dict[str, Any]):
        self.path = path
        self.defaults = defaults
        self.config = {}

        self._load()
        self._apply_defaults()
        self.save()

    def _load(self):
        if os.path.exists(self.path):
            try:
                with open(self.path, 'r', encoding='utf-8') as f:
                    self.config = json.load(f)
            except Exception:
                self.config = {}
        else:
            self.config = {}

    def _apply_defaults(self):
        def merge(d, default):
            for k, v in default.items():
                if k not in d:
                    d[k] = v
                elif isinstance(d[k], dict) and isinstance(v, dict):
                    merge(d[k], v)
        merge(self.config, self.defaults)

    def save(self):
        with open(self.path, 'w', encoding='utf-8') as f:
            json.dump(self.config, f, ensure_ascii=False, indent=4)

    def __getattr__(self, item):
        value = self.config.get(item)
        if isinstance(value, dict):
            return NestedConfig(self, value)
        return value

    def __setattr__(self, key, value):
        if key in {"path", "defaults", "config"}:
            return super().__setattr__(key, value)

        self.config[key] = value
        self.save()

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
    normalized = normalize_projects_root(new_path)
    try:
        UserConfig.General.projects_dir = normalized
    except Exception:
        data = getattr(UserConfig, "config", None)
        if isinstance(data, dict):
            general = data.get("General")
            if not isinstance(general, dict):
                general = {}
                data["General"] = general
            general["projects_dir"] = normalized
            save = getattr(UserConfig, "save", None)
            if callable(save):
                save()
    return normalized


# Синхронизация legacy-константы для совместимости с кодом, который всё ещё читает PROJECTS_ROOT.
PROJECTS_ROOT = get_projects_root()
