#!/bin/bash

# Определяем папку, где лежит скрипт
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Список популярных терминалов
TERMINALS=(
  "gnome-terminal --working-directory"
  "konsole --workdir"
  "xfce4-terminal --working-directory"
  "mate-terminal --working-directory"
  "lxterminal --working-directory"
  "tilix --working-directory"
  "kitty --directory"
  "xterm -e 'cd'"
)

# Перебираем и запускаем первый доступный
for TERM_CMD in "${TERMINALS[@]}"; do
  BIN=$(echo "$TERM_CMD" | awk '{print $1}')
  if command -v "$BIN" &>/dev/null; then
    if [[ "$BIN" == "xterm" ]]; then
      eval "$BIN -e \"cd '$SCRIPT_DIR'; exec bash\" &"
    else
      eval "$TERM_CMD \"$SCRIPT_DIR\" &"
    fi
    exit 0
  fi
done

echo "❌ Не удалось найти подходящий терминал."
exit 1
