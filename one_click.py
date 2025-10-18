#!/usr/bin/env python3
# one_click_install.py
#
# Минималистичный инсталлятор: CPU / CUDA 12.6 / ROCm 6.2.4
# 1) Устанавливает зависимости из requirements-common.txt
# 2) Ставит PyTorch под выбранный тип GPU
# 3) Ставит PaddleOCR (CUDA или CPU индекс, для ROCm используется CPU-колесо Paddle)
#
# Подсказки:
# - Можно выбрать режим через переменную окружения GPU_CHOICE: CPU | CUDA | ROCM
# - Для ROCm на Windows будет выход с сообщением (официальных колёс torch ROCm под Win нет)

import os
import sys
import argparse
import subprocess
import json
import signal
import shlex
import re
import time
import shutil
import zipfile
from urllib.request import Request, urlopen
from urllib.error import URLError, HTTPError
import tempfile

GITHUB_LATEST = "https://github.com/nihui/waifu2x-ncnn-vulkan/releases/latest"
LAMA_ZIP_URL = "https://github.com/advimman/lama/archive/refs/heads/main.zip"
TORCH_VERSION = "2.5.1"
CUDA_STREAM = "cu126"          # строго 12.6
ROCM_STREAM = "rocm6.2.4"      # ROCm 6.2.4
STATE_FILE = ".installer_state.json"  # на будущее: не обязателен, но без логики апдейтов
cuda = ""
rocm = ""
def on_sigint(sig, frame):
    print("\nInterrupted. Exiting.")
    sys.exit(130)
signal.signal(signal.SIGINT, on_sigint)

def is_linux():   return sys.platform.startswith("linux")
def is_windows(): return sys.platform.startswith("win")
def is_macos():   return sys.platform.startswith("darwin")

def run(cmd: str, assert_ok=True):
    # Универсальный раннер
    result = subprocess.run(cmd, shell=True)
    if assert_ok and result.returncode != 0:
        sys.exit(result.returncode)
    return result.returncode

def print_big(msg: str):
    msg = msg.strip()
    print("\n" + "*"*67)
    for line in msg.splitlines():
        print("*", line)
    print("*"*67 + "\n")

def get_gpu_choice():
    env = os.environ.get("GPU_CHOICE", "").strip().upper()
    if env in {"CPU", "CUDA", "ROCM"}:
        print_big(f'GPU_CHOICE="{env}" из переменной окружения.')
        return env

    print()
    print("Выберите тип вычислений:\n")
    print("A) NVIDIA CUDA 12.6")
    if is_linux(): print("B) AMD ROCm (Только Linux)")
    print("C) CPU")
    print()
    m = {"A": "CUDA", "B": "ROCM", "C": "CPU"}
    while True:
        choice = input("Input> ").strip().upper()
        if choice in m:
            if m[choice] == "ROCM" and not is_linux():
                print("ROCm пока доступен только на Linux.")
                continue
            return m[choice]
        print("Неверный выбор, попробуй снова.")

def get_cuda_choice():
    print()
    print("Выберите версию CUDA:\n")
    print("A) CUDA 11.8")
    print("B) CUDA 12.1")
    print("C) CUDA 12.4 (Рекомендуется)")
    print()
    m = {"A": "cu118", "B": "cu121", "C": "cu124"}
    while True:
        choice = input("Input> ").strip().upper()
        if choice in m:
            return m[choice]
        print("Неверный выбор, попробуй снова.")

def get_rocm_choice():
    print()
    print("Выберите версию ROCM:\n")
    print("A) ROCM 6.1")
    print("B) ROCM 6.2")
    print()
    m = {"A": "rocm6.1", "B": "rocm6.2"}
    while True:
        choice = input("Input> ").strip().upper()
        if choice in m:
            return m[choice]
        print("Неверный выбор, попробуй снова.")

def pytorch_cmd(gpu_choice: str, whl: str) -> str:
    base = "python -m pip install --upgrade pip && python -m pip install torch==2.5.1 torchvision==0.20.1"
    if gpu_choice == "CUDA":
        # Официальный индекс для CUDA 12.6
        return f'{base} --index-url https://download.pytorch.org/whl/{whl}'
    if gpu_choice == "ROCM":
        # Официальный индекс для ROCm 6.2.4
        return f'{base} --index-url https://download.pytorch.org/whl/{whl}'
    # CPU
    if is_macos():
        return base
    return f'{base} --index-url https://download.pytorch.org/whl/cpu'

def paddle_cmd(gpu_choice: str, whl: str) -> str:
    # PaddleOCR (ядро paddle) — по твоему требованию:
    # CUDA: paddlepaddle-gpu с индексом cu126
    # CPU и ROCm: обычный paddlepaddle с CPU-индексом
    if gpu_choice == "CUDA":
        if whl == "cu118" or whl == "cu124":
            return "python -m pip install paddlepaddle-gpu==3.2.0 -i https://www.paddlepaddle.org.cn/packages/stable/cu118/"
        return "python -m pip install paddlepaddle-gpu==3.2.0 -i https://www.paddlepaddle.org.cn/packages/stable/cu126/"
    else:
        # На ROCm отдельных колёс Paddle под ROCm нет — ставим CPU-колесо.
        return "python -m pip install paddlepaddle==3.2.0 -i https://www.paddlepaddle.org.cn/packages/stable/cpu/"

def copy(source: str, destination: str) -> str:
    """
    Возвращает команду копирования файла или папки
    в зависимости от операционной системы.
    
    :param source: путь к исходному файлу или папке
    :param destination: путь к месту назначения
    :return: строка с командой копирования
    """
    # Экранируем пути, чтобы избежать проблем с пробелами
    src = shlex.quote(source)
    dst = shlex.quote(destination)


    if is_windows:
        # Определяем, файл или папка
        src = src.replace("/", "\\")
        dst = dst.replace("/", "\\")
        if os.path.isdir(source):
            # /E — копировать всё, включая пустые папки
            # /I — если назначения нет, предполагаем, что это папка
            command = f'xcopy {src} {dst} /E /I /Y'
        else:
            command = f'copy {src} {dst}'
    else:
        # Для Linux/macOS
        if os.path.isdir(source):
            command = f'cp -r {src} {dst}'
        else:
            command = f'cp {src} {dst}'
    print(command)
    return command

def _os_tags_and_target():
    """
    Возвращает (список искомых тэгов для имени файла релиза, целевая подпапка).
    Имена архивов у nihui обычно такие:
      - waifu2x-ncnn-vulkan-<ver>-linux.zip
      - waifu2x-ncnn-vulkan-<ver>-windows.zip
      - waifu2x-ncnn-vulkan-<ver>-macos.zip  (иногда '-mac.zip')
    """
    if is_windows():
        return (["windows", "win"], "Win")
    if is_macos():
        return (["macos", "mac", "osx", "darwin"], "Mac")
    # по умолчанию считаем Linux
    return (["linux"], "Lin")

def _download(url: str, dst_file: str, retries: int = 3, timeout: int = 30):
    # Мини-скачивалка с повтором
    last_err = None
    for attempt in range(1, retries + 1):
        try:
            req = Request(url, headers={"User-Agent": "Mozilla/5.0"})
            with urlopen(req, timeout=timeout) as r, open(dst_file, "wb") as f:
                shutil.copyfileobj(r, f)
            return
        except (HTTPError, URLError, TimeoutError, OSError) as e:
            last_err = e
            if attempt < retries:
                time.sleep(1.5 * attempt)
            else:
                raise

def _find_asset_url_from_api(tags: list[str]) -> tuple[str, str] | None:
    """
    Использует GitHub API для получения ассетов последнего релиза waifu2x.
    Возвращает (asset_url, filename) или None.
    """
    api_url = "https://api.github.com/repos/nihui/waifu2x-ncnn-vulkan/releases/latest"
    try:
        req = Request(api_url, headers={"User-Agent": "Mozilla/5.0"})
        with urlopen(req, timeout=30) as resp:
            data = json.load(resp)
    except Exception as e:
        print_big(f"Ошибка при обращении к GitHub API: {e}")
        return None

    lower_tags = tuple(t.lower() for t in tags)
    assets = data.get("assets", [])
    for asset in assets:
        name = asset.get("name", "").lower()
        url = asset.get("browser_download_url")
        if url and name.endswith(".zip") and any(tag in name for tag in lower_tags):
            return url, asset.get("name")
    # fallback — берём первый .zip
    for asset in assets:
        name = asset.get("name", "").lower()
        url = asset.get("browser_download_url")
        if url and name.endswith(".zip"):
            return url, asset.get("name")
    return None


def install_waifu2x():
    """
    Скачивает последний waifu2x-ncnn-vulkan для текущей ОС и распаковывает в waifu2x/(Lin|Mac|Win)
    """
    print_big("Шаг 5/5: Установка waifu2x-ncnn-vulkan")
    tags, target_suffix = _os_tags_and_target()

    # 1) Получаем ссылку на ассет через GitHub API
    found = _find_asset_url_from_api(tags)
    if not found:
        print_big("Не удалось найти zip-ассет для текущей ОС через GitHub API.")
        return
    asset_url, filename = found
    print(f"Найден ассет: {filename}")
    print(f"Ссылка: {asset_url}")

    # 2) Скачиваем
    download_dir = os.path.join(os.getcwd(), ".tmp_downloads")
    os.makedirs(download_dir, exist_ok=True)
    zip_path = os.path.join(download_dir, filename)
    try:
        print(f"Скачивание: {asset_url}")
        _download(asset_url, zip_path, retries=3, timeout=60)
    except Exception as e:
        print_big(f"Ошибка скачивания {filename}: {e}")
        return

    # 3) Распаковка
    target_root = os.path.join(os.getcwd(), "waifu2x", target_suffix)
    os.makedirs(target_root, exist_ok=True)
    try:
        with zipfile.ZipFile(zip_path, "r") as zf:
            # Получаем список всех файлов в архиве
            all_files = zf.namelist()
            
            # Определяем общую корневую папку (если есть)
            if all_files:
                # Берём первый элемент и извлекаем корневую папку
                first_path = all_files[0]
                root_folder = first_path.split('/')[0] + '/'
                
                # Проверяем, что все файлы начинаются с этой папки
                if all(f.startswith(root_folder) for f in all_files):
                    # Распаковываем, убирая корневую папку
                    for file in all_files:
                        if file == root_folder:
                            continue  # Пропускаем саму папку
                        
                        # Убираем корневую папку из пути
                        target_path = file[len(root_folder):]
                        if not target_path:
                            continue
                        
                        target_file = os.path.join(target_root, target_path)
                        
                        # Если это директория
                        if file.endswith('/'):
                            os.makedirs(target_file, exist_ok=True)
                        else:
                            # Создаём родительские директории если нужно
                            os.makedirs(os.path.dirname(target_file), exist_ok=True)
                            # Извлекаем файл
                            with zf.open(file) as source, open(target_file, 'wb') as target:
                                target.write(source.read())
                else:
                    # Если нет общей корневой папки, распаковываем как есть
                    zf.extractall(target_root)
            
        print(f"Распаковано в: {target_root}")
    except zipfile.BadZipFile:
        print_big("Загруженный файл повреждён или не является zip-архивом.")
        return
    except Exception as e:
        print_big(f"Ошибка распаковки: {e}")
        return
    finally:
        try:
            os.remove(zip_path)
            if not os.listdir(download_dir):
                os.rmdir(download_dir)
        except Exception:
            pass

    print_big("waifu2x установлен.")

def install_lama_from_zip():
    """
    Скачивает https://github.com/advimman/lama/archive/refs/heads/main.zip,
    распаковывает и переносит папку lama-main в корень проекта как lama.
    Полностью имитирует `git clone` в папку lama.
    """
    #print_big("Шаг 4/5: Установка репозитория lama (без git)")

    tmp_dir = tempfile.mkdtemp(prefix="lama_dl_")
    zip_path = os.path.join(tmp_dir, "lama-main.zip")

    try:
        # 1) скачать архив
        print(f"Скачивание: {LAMA_ZIP_URL}")
        _download(LAMA_ZIP_URL, zip_path, retries=3, timeout=60)

        # 2) распаковать
        with zipfile.ZipFile(zip_path, "r") as zf:
            zf.extractall(tmp_dir)

        # 3) найти распакованную папку (обычно lama-main)
        candidates = [
            d for d in os.listdir(tmp_dir)
            if os.path.isdir(os.path.join(tmp_dir, d)) and d.lower().startswith("lama")
        ]
        if not candidates:
            raise RuntimeError("Не найдена папка 'lama-main' в распакованном архиве.")

        src_dir = os.path.join(tmp_dir, candidates[0])  # например, .../lama-main
        dst_dir = os.path.join(os.getcwd(), "lama")

        # 4) если папка lama уже существует — удаляем (как при fresh clone)
        if os.path.exists(dst_dir):
            shutil.rmtree(dst_dir, ignore_errors=True)

        # 5) переносим lama-main -> ./lama
        shutil.move(src_dir, dst_dir)
        print(f"Распаковано в: {dst_dir}")

    except Exception as e:
        print_big(f"Ошибка установки lama: {e}")
        sys.exit(4)
    finally:
        # 6) чистим временную директорию
        try:
            shutil.rmtree(tmp_dir, ignore_errors=True)
        except Exception:
            pass

def main():
    parser = argparse.ArgumentParser(description="One-click installer (CPU / CUDA 12.6 / ROCm)")
    parser.add_argument("--no-input", action="store_true", help="Не спрашивать — брать GPU_CHOICE из окружения (или CPU по умолчанию)")
    parser.add_argument("--skip_req", action="store_false", help="Пропустить требования из requirements.txt")
    parser.add_argument("--skip_torch", action="store_false", help="Пропустить установку PyTorch")
    parser.add_argument("--skip_paddle", action="store_false", help="Пропустить установку PaddleOCR")
    args = parser.parse_args()

    if args.no_input and "GPU_CHOICE" not in os.environ:
        os.environ["GPU_CHOICE"] = "CPU"

    gpu_choice = get_gpu_choice()

    # Быстрые проверки платформы
    if gpu_choice == "ROCM" and is_windows():
        print_big("ROCm не поддерживается на Windows. Выбери CPU или CUDA 12.6.")
        sys.exit(2)
    if gpu_choice == "CUDA":
        whl = get_cuda_choice()
    elif gpu_choice == "ROCM":
        whl = get_rocm_choice()
    else:
        whl = None
    # 1) requirements-common.txt
    req_path = os.path.join(os.getcwd(), "requirements.txt")
    if not os.path.isfile(req_path):
        print_big("Файл requirements.txt не найден рядом со скриптом.")
        sys.exit(3)
    steps = 5
    if args.skip_req:
        print_big(f"Шаг 1/{steps}: Установка общих зависимостей из requirements.txt")
        run(f"python -m pip install --upgrade pip wheel setuptools", assert_ok=True)
        run(f"python -m pip install -r \"{req_path}\" --upgrade", assert_ok=True)
    if args.skip_torch:
        # 2) PyTorch
        print_big(f"Шаг 2/{steps}: Установка PyTorch ({gpu_choice}:{whl})")
        run(pytorch_cmd(gpu_choice, whl), assert_ok=True)
    # if args.skip_paddle:
    #     # 3) PaddleOCR runtime (paddle/paddle-gpu)
    #     print_big(f"Шаг 3/{steps}: Установка PaddleOCR runtime (paddle)")
    #     run(paddle_cmd(gpu_choice), assert_ok=True)

    print_big(f"Шаг 4/{steps}: Установка репозитория lama")
    install_lama_from_zip()
    run(copy("lama_files/inpainter.py", "lama/"))
    #run("python -m pip install numpy==1.26.4")
    install_waifu2x()
    # Пост-инфо

    print_big("Установка завершена")

if __name__ == "__main__":
    main()
