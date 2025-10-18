import os
from pathlib import Path
# Папка, где лежат все проекты
program_dir = Path(__file__).resolve().parent
script_dir = os.path.dirname(os.path.abspath(__file__))
PROJECTS_ROOT = os.path.join(script_dir, "projects")
# Проект по умолчанию
DEFAULT_PROJECT = ""#os.path.join(PROJECTS_ROOT, "Сегодня я буду _", "ch20")
DEBUG_CONSOLE = False
# Имена файлов внутри проекта
BUBBLES_FILE = "translation_bubbles.json"
NOTES_FILE = "translation_notes.txt"
SCR_DIR = "scr"
CLEANED_DIR = "cleaned"
SAVED_DIR = "saved"
TEXT_IMAGES_DIR = "text_images"
CHARACTERS_DIR = "characters"
TERMS_FILE = "terms.json"


# Папки для ИИ моделей
MODELS_DIR = os.path.join(program_dir, "AI_models")
EASYOCR_DIR = os.path.join(MODELS_DIR, "EasyOCR")
LAMA_DIR = os.path.join(MODELS_DIR, "Lama")
PADDLEOCR_DIR = os.path.join(MODELS_DIR, "PaddleOCR")
PADDLEOCR_DET_DIR = os.path.join(PADDLEOCR_DIR, "det")
PADDLEOCR_REC_DIR = os.path.join(PADDLEOCR_DIR, "rec")
PADDLEOCR_CLS_DIR = os.path.join(PADDLEOCR_DIR, "cls")
folders = [EASYOCR_DIR, LAMA_DIR, PADDLEOCR_DET_DIR, PADDLEOCR_REC_DIR, PADDLEOCR_CLS_DIR]
for folder in folders:
    if not os.path.exists(folder):
        os.makedirs(folder)
        print(f"Создана папка: {folder}")
