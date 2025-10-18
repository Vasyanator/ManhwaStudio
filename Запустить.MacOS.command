#!/usr/bin/env bash
# Запустить приложение (macOS, Conda)
# Активирует conda окружение и запускает mainV2.py

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Имя запускаемого python-файла
APP_SCRIPT="mainV2.py"

# 1) Вариант с portable-окружением (если есть)
if [[ -f "portable_env/bin/python" ]]; then
    ./portable_env/bin/python "$APP_SCRIPT" "$@"
    exit $?
fi

# 2) Вариант с conda/Miniforge окружением
INSTALL_ENV_DIR="$SCRIPT_DIR/installer_files/env"
CONDA_ROOT_PREFIX="$SCRIPT_DIR/installer_files/conda"

# Проверяем, что окружение существует
if [[ ! -f "$INSTALL_ENV_DIR/bin/python" ]]; then
    echo "[ERROR] Python окружение не найдено: $INSTALL_ENV_DIR"
    echo "Создайте его и запустите скрипт снова (запустите Установить.MacOS.command)."
    read -r -p "Нажмите Enter для выхода..." _
    exit 1
fi

# Проверяем, что conda доступна
if [[ ! -f "$CONDA_ROOT_PREFIX/bin/conda" ]]; then
    echo "[ERROR] Не найден файл conda: $CONDA_ROOT_PREFIX/bin/conda"
    read -r -p "Нажмите Enter для выхода..." _
    exit 1
fi

# Инициализируем conda для bash/zsh
eval "$("$CONDA_ROOT_PREFIX/bin/conda" shell.bash hook 2>/dev/null || "$CONDA_ROOT_PREFIX/bin/conda" shell.zsh hook 2>/dev/null)"

# Активируем окружение
set +e
conda activate "$INSTALL_ENV_DIR"
rc=$?
set -e

if [[ $rc -ne 0 ]]; then
    echo "[ERROR] Не удалось активировать окружение: $INSTALL_ENV_DIR"
    read -r -p "Нажмите Enter для выхода..." _
    exit 1
fi

# Запуск приложения
set +e
python "$APP_SCRIPT" "$@"
rc=$?
set -e

# (необязательно) деактивация окружения
conda deactivate >/dev/null 2>&1 || true

if [[ $rc -ne 0 ]]; then
    echo
    echo "[x] Приложение завершилось с кодом $rc"
    read -r -p "Нажмите Enter, чтобы закрыть окно..." _
fi

exit $rc
