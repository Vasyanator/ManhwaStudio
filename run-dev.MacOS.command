#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# run-dev.MacOS.command - launcher for the dev build of ManhwaStudio.
#
# Contains no logic: the algorithm lives in tools/run-dev/run-dev.sh, which is
# shared with Linux. The .command extension is what lets Finder execute it on a
# double click; the path is resolved from $0 because Finder starts the process
# with the working directory set to the user's home, not to the project.
#
# All arguments are forwarded, e.g.:  ./run-dev.MacOS.command --no-update
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
