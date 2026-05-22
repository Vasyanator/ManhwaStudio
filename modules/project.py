import os, json
from config import (DEFAULT_PROJECT, BUBBLES_FILE, 
                    SRC_DIR, SAVED_DIR, CLEAN_LAYERS_DIR, ALT_VERS_DIR, CLEANED_DIR, TEXT_IMAGES_DIR, NOTES_FILE, 
                    CHARACTERS_DIR, TERMS_FILE, PROJECT_SETTINGS_FILE, PROJECT_CONFIG_DEFAULTS, BaseUserConfig)


import shutil
from PIL import Image, UnidentifiedImageError

class Project:
    def __init__(self, path=None):
        # path — путь до ПАПКИ ГЛАВЫ: projects/<title>/<chapter>
        if not path and DEFAULT_PROJECT:
            rooted = os.path.normpath(DEFAULT_PROJECT)
        elif not path:
            raise ValueError("Путь до проекта не указан")
        else:
            rooted = os.path.normpath(path)
        self.path = rooted
        # Путь к тайтлу (родитель папки главы)
        self.title_path = os.path.dirname(rooted)
        # translation_notes.txt живёт в папке тайтла
        self.notes_file = os.path.join(self.title_path, NOTES_FILE)

        self.bubbles_path = os.path.join(self.path, BUBBLES_FILE)
        self.src_dir = os.path.join(self.path, SRC_DIR)
        if not os.path.isdir(self.src_dir):
            legacy_scr_dir = os.path.join(self.path, "scr")
            if os.path.isdir(legacy_scr_dir):
                os.rename(legacy_scr_dir, os.path.join(self.path, "src"))
                self.src_dir = os.path.join(self.path, "src")
        self.clean_layers_dir = os.path.join(self.path, CLEAN_LAYERS_DIR)
        self.cleaned_dir = os.path.join(self.path, CLEANED_DIR)
        self.alt_vers_dir = os.path.join(self.path, ALT_VERS_DIR)
        self.saved_dir = os.path.join(self.path, SAVED_DIR)
        self.text_images = os.path.join(self.path, TEXT_IMAGES_DIR)
        self.char_dir = os.path.join(self.title_path, CHARACTERS_DIR)
        self.terms_file = os.path.join(self.title_path, TERMS_FILE)
        self.bubbles = []
        self.settings = BaseUserConfig(os.path.join(self.title_path, PROJECT_SETTINGS_FILE), PROJECT_CONFIG_DEFAULTS)
        self.bubble_type = "aside"
        self.aside_min_width_px = 450
        self.aside_max_width_px = 550
        self.page_spacing_px = 200
        self.vertical_edge_margin_px = 200
        try:
            canvas_settings = getattr(self.settings, "canvas", None)
            if canvas_settings and getattr(canvas_settings, "bubble_type", None):
                self.bubble_type = canvas_settings.bubble_type
            elif getattr(self.settings, "bubble_type", None):
                self.bubble_type = self.settings.bubble_type
            if canvas_settings and getattr(canvas_settings, "aside_min_width_px", None) is not None:
                self.aside_min_width_px = int(canvas_settings.aside_min_width_px)
            if canvas_settings and getattr(canvas_settings, "aside_max_width_px", None) is not None:
                self.aside_max_width_px = int(canvas_settings.aside_max_width_px)
            if canvas_settings and getattr(canvas_settings, "page_spacing_px", None) is not None:
                self.page_spacing_px = max(0, int(canvas_settings.page_spacing_px))
            if canvas_settings and getattr(canvas_settings, "vertical_edge_margin_px", None) is not None:
                self.vertical_edge_margin_px = max(0, int(canvas_settings.vertical_edge_margin_px))
        except Exception:
            self.bubble_type = "aside"

    def exists(self):
        return os.path.isdir(self.path)

    def load(self):
        # если нет json — оставляем пустым
        if os.path.isfile(self.bubbles_path):
            with open(self.bubbles_path, 'r', encoding='utf-8') as f:
                self.bubbles = json.load(f)
        else:
            self.bubbles = []

    def autosave(self):
        with open(self.bubbles_path, 'w', encoding='utf-8') as f:
            json.dump(self.bubbles, f, ensure_ascii=False, indent=2)

    def _unique_png_path(self, dst_dir: str, stem: str) -> str:
        """Подбирает уникальный путь вида <stem>.png, <stem>-1.png, <stem>-2.png, ..."""
        base = os.path.join(dst_dir, f"{stem}.png")
        if not os.path.exists(base):
            return base
        i = 1
        while True:
            candidate = os.path.join(dst_dir, f"{stem}-{i}.png")
            if not os.path.exists(candidate):
                return candidate
            i += 1

    def _to_png_compatible_mode(self, im: Image.Image) -> Image.Image:
        """
        Приводит изображение к режиму, который корректно сохранится в PNG.
        - Сохраняем альфу, если она есть.
        - P (палитровые) и LA → RGBA, CMYK → RGB, прочие без альфы → RGB.
        """
        if im.mode in ("RGBA", "LA"):
            return im.convert("RGBA")
        if im.mode == "P":
            # у палитровых может быть альфа — конвертируем в RGBA
            return im.convert("RGBA")
        if im.mode == "CMYK":
            return im.convert("RGB")
        if "A" in im.getbands():
            return im.convert("RGBA")
        if im.mode != "RGB":
            return im.convert("RGB")
        return im

    def ensure_saved(self, progress_callback=None):
        """
        Конвертирует/копирует изображения из src → cleaned (PNG).

        Args:
            progress_callback: функция(current, total, filename) для отображения прогресса
        """
        
        os.makedirs(self.cleaned_dir, exist_ok=True)
        if os.listdir(self.cleaned_dir):
            return  # уже заполнено — выходим

        # Собираем список файлов для обработки
        files_to_process = []
        for fn in os.listdir(self.src_dir):
            src_path = os.path.join(self.src_dir, fn)
            if os.path.isfile(src_path):
                files_to_process.append((fn, src_path))

        total_files = len(files_to_process)

        for idx, (fn, src) in enumerate(files_to_process, start=1):
            stem, ext = os.path.splitext(fn)
            ext_lower = ext.lower()

            # Уведомляем о прогрессе
            if progress_callback:
                progress_callback(idx, total_files, fn)

            try:
                # Если уже PNG — просто копируем без конвертации
                if ext_lower == '.png':
                    dst = self._unique_png_path(self.cleaned_dir, stem)
                    shutil.copy2(src, dst)
                    continue

                # Для других форматов — конвертируем
                with Image.open(src) as im:
                    im.load()  # прогрузим, чтобы не тянуть файл после закрытия
                    im = self._to_png_compatible_mode(im)

                    dst = self._unique_png_path(self.cleaned_dir, stem)
                    # Примечание: PNG не поддерживает анимацию «из коробки» (APNG Pillow не пишет),
                    # поэтому для GIF/WebP с мультикадрами сохраняем первый кадр.
                    im.save(dst, format="PNG", optimize=True)
            except UnidentifiedImageError:
                # Не изображение — пропускаем
                continue
            except Exception as e:
                # Логируем ошибку, но продолжаем обработку остальных файлов
                print(f"Не удалось обработать {src}: {e}")


    def ensure_clean_layers_dir(self):
        """
        Проверяет наличие и содержимое self.clean_layers_dir.
        Если папка отсутствует или пуста, и при этом self.cleaned_dir существует
        и не пуста — копирует содержимое self.cleaned_dir в self.clean_layers_dir.
        """
        clean_layers_exists = os.path.isdir(self.clean_layers_dir)
        clean_layers_has_files = (
            clean_layers_exists and any(os.scandir(self.clean_layers_dir))
        )

        if clean_layers_has_files:
            return

        cleaned_exists = os.path.isdir(self.cleaned_dir)
        cleaned_has_files = (
            cleaned_exists and any(os.scandir(self.cleaned_dir))
        )

        if not cleaned_has_files:
            return

        os.makedirs(self.clean_layers_dir, exist_ok=True)

        for item in os.listdir(self.cleaned_dir):
            src = os.path.join(self.cleaned_dir, item)
            dst = os.path.join(self.clean_layers_dir, item)

            if os.path.isdir(src):
                shutil.copytree(src, dst, dirs_exist_ok=True)
            else:
                shutil.copy2(src, dst)

    def ensure_translation_notes(self):
        os.makedirs(self.title_path, exist_ok=True)
        if not os.path.isfile(self.notes_path):
            # создаём пустой файл, если его ещё нет
            with open(self.notes_path, 'a', encoding='utf-8'):
                pass
