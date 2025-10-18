#!/usr/bin/env bash
# Открыть окружение (Linux, Conda)
# Открывает терминал с активированным conda окружением для выполнения команд

set -e
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Пути к окружению
INSTALL_ENV_DIR="$SCRIPT_DIR/installer_files/env"
CONDA_ROOT_PREFIX="$SCRIPT_DIR/installer_files/conda"

# Проверяем, что окружение существует
if [[ ! -f "$INSTALL_ENV_DIR/bin/python" ]]; then
    echo "[ERROR] Окружение не найдено: $INSTALL_ENV_DIR"
    echo "Создайте его перед запуском (запустите Установить.Linux.sh)."
    read -r -p "Нажмите Enter для выхода..." _
    exit 1
fi

# Проверяем, что conda доступна
if [[ ! -f "$CONDA_ROOT_PREFIX/bin/conda" ]]; then
    echo "[ERROR] Не найден файл conda: $CONDA_ROOT_PREFIX/bin/conda"
    read -r -p "Нажмите Enter для выхода..." _
    exit 1
fi

# Блок команд для выполнения в новом терминале
RUN_BLOCK="
set -e
cd '$SCRIPT_DIR'

# Инициализируем conda для bash
eval \"\$('$CONDA_ROOT_PREFIX/bin/conda' shell.bash hook)\"

# Активируем окружение
conda activate '$INSTALL_ENV_DIR'

if [[ \$? -ne 0 ]]; then
    echo '[ERROR] Не удалось активировать окружение.'
    read -r -p 'Нажмите Enter для выхода...' _
    exit 1
fi

echo
echo '============================================================'
echo '  Окружение успешно активировано:'
echo \"     $INSTALL_ENV_DIR\"
echo '  Можно использовать python, pip и т.д.'
echo '============================================================'
echo

# Открываем интерактивную оболочку в этом окружении
exec \$SHELL -l
"

# Функция для запуска в новом терминале
launch_in_terminal() {
    local cmd="$1"
    # Пытаемся найти доступный терминал
    for term in x-terminal-emulator gnome-terminal konsole xfce4-terminal kitty alacritty mate-terminal lxterminal tilix urxvt xterm; do
        if command -v "$term" >/dev/null 2>&1; then
            case "$term" in
                gnome-terminal|mate-terminal|xfce4-terminal|tilix)
                    "$term" -- bash -c "$cmd"
                    return 0;;
                konsole)
                    "$term" -e bash -c "$cmd"
                    return 0;;
                kitty|alacritty|lxterminal|urxvt|xterm|x-terminal-emulator)
                    "$term" -e bash -c "$cmd"
                    return 0;;
            esac
        fi
    done
    return 1
}

# Если уже в интерактивном терминале, просто выполняем блок здесь
if [[ -t 1 ]] && [[ -n "${TERM-}" ]]; then
    bash -c "$RUN_BLOCK"
    exit
fi

# Иначе пробуем открыть новое окно эмулятора
if ! launch_in_terminal "$RUN_BLOCK"; then
    # Фолбэк: выполнить в текущем процессе
    bash -c "$RUN_BLOCK"
fi
