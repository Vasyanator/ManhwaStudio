#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# run-dev.Linux.sh - launcher for the dev build of ManhwaStudio.
#
# Contains no logic: the algorithm lives in tools/run-dev/run-dev.sh, which is
# shared with macOS. This file exists so the entry point sits in the project
# root, where a user looks for it.
#
# All arguments are forwarded, e.g.:  ./run-dev.Linux.sh --no-update
# ---------------------------------------------------------------------------

set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
CORE="$DIR/tools/run-dev/run-dev.sh"

if [ ! -f "$CORE" ]; then
    echo "[ОШИБКА] Не найден $CORE"
    echo "Этот файл должен лежать в корне проекта ManhwaStudio."
    exit 1
fi

exec bash "$CORE" "$@"
