#!/usr/bin/env bash
# Открыть окружение (macOS, Conda)
# Открывает Terminal.app с активированным conda окружением для выполнения команд

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Пути к окружению
INSTALL_ENV_DIR="$SCRIPT_DIR/installer_files/env"
CONDA_ROOT_PREFIX="$SCRIPT_DIR/installer_files/conda"

# Команды, которые должны выполниться в новом окне Terminal
read -r -d '' CMD <<'EOS'
set -e
cd "__WORKDIR__"

INSTALL_ENV_DIR="__ENV_DIR__"
CONDA_ROOT_PREFIX="__CONDA_ROOT__"

# Проверяем, что окружение существует
if [[ ! -f "$INSTALL_ENV_DIR/bin/python" ]]; then
    echo "[ERROR] Окружение не найдено: $INSTALL_ENV_DIR"
    echo "Создайте его перед запуском (запустите Установить.MacOS.command)."
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
conda activate "$INSTALL_ENV_DIR"

if [[ $? -ne 0 ]]; then
    echo '[ERROR] Не удалось активировать окружение.'
    read -r -p 'Нажмите Enter для выхода...' _
    exit 1
fi

echo
echo '============================================================'
echo '  Окружение успешно активировано:'
echo "     $INSTALL_ENV_DIR"
echo '  Можно использовать python, pip и т.д.'
echo '============================================================'
echo

# Открываем интерактивную оболочку в этом окружении
exec $SHELL -l
EOS

# Подставим пути в команду для osascript
CMD_ESCAPED=$(printf "%s" "$CMD" \
    | sed "s|__WORKDIR__|$SCRIPT_DIR|g" \
    | sed "s|__ENV_DIR__|$INSTALL_ENV_DIR|g" \
    | sed "s|__CONDA_ROOT__|$CONDA_ROOT_PREFIX|g" \
    | sed 's/\\/\\\\/g; s/"/\\"/g')

/usr/bin/osascript <<OSA
tell application "Terminal"
    activate
    do script "bash -c \"$CMD_ESCAPED\""
end tell
OSA
