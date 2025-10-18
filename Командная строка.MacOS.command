#!/bin/bash

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Если есть iTerm2 — используем его
if open -Ra "iTerm"; then
  osascript <<EOF
tell application "iTerm"
  create window with default profile
  tell current session of current window
    write text "cd \"$SCRIPT_DIR\""
  end tell
end tell
EOF
  exit 0
fi

# Если нет — используем стандартный Terminal.app
osascript <<EOF
tell application "Terminal"
  do script "cd \"$SCRIPT_DIR\""
  activate
end tell
EOF
