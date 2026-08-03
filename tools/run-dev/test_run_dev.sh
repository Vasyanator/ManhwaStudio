#!/usr/bin/env bash
#
# File: tools/run-dev/test_run_dev.sh
#
# Purpose:
# Contract tests for the git stage and version helpers of `run-dev.sh`. The git
# stage is the part that can destroy a user's work, so every branch of it is
# exercised against throwaway repositories under a temp dir.
#
# What is covered:
# - version_ge / extract_version / required_msrv
# - clean tree behind origin              -> fast-forward
# - dirty tree, edits do NOT overlap      -> silent automatic merge, edits kept
# - dirty tree, edits overlap but merge   -> automatic merge, edits kept
# - dirty tree, edits truly conflict      -> exit 3 AND the tree is restored
# - "discard local" option                -> updated, changes recoverable in stash
# - non-repository (ZIP) adoption         -> history grafted, files untouched
# - untracked files are never touched by any path
#
# Run: bash tools/run-dev/test_run_dev.sh
#
# Notes:
# Sources run-dev.sh with MS_RUN_DEV_SOURCE_ONLY=1 so `main` does not execute,
# then drives its functions directly. No network, no cargo, no user repository.

set -uo pipefail

SELF_DIR=$(cd "$(dirname "$0")" && pwd)
PASS=0
FAIL=0

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAIL=$((FAIL + 1)); }
note() { printf '\n\033[1m%s\033[0m\n' "$1"; }

check() { # check <desc> <actual> <expected>
    if [ "$2" = "$3" ]; then pass "$1"; else fail "$1 (получено «$2», ожидалось «$3»)"; fi
}

SANDBOX=$(mktemp -d)
trap 'rm -rf "$SANDBOX"' EXIT

export MS_RUN_DEV_SOURCE_ONLY=1
export MS_RUN_DEV_BRANCH="master"
export GIT_CONFIG_GLOBAL="$SANDBOX/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
git config --file "$SANDBOX/gitconfig" user.email "test@example.com"
git config --file "$SANDBOX/gitconfig" user.name  "run-dev test"
git config --file "$SANDBOX/gitconfig" init.defaultBranch master

# shellcheck source=./run-dev.sh
. "$SELF_DIR/run-dev.sh"

# Silence the script's own chatter; tests assert on state, not on prose.
say() { :; }; step() { :; }; ok() { :; }; warn() { :; }; info() { :; }

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

# Creates an "upstream" repo plus a clone that is 1 commit behind it.
# Upstream's new commit edits `upstream.txt` and `shared.txt`.
make_pair() { # make_pair <name>
    local up="$SANDBOX/$1.git" wc="$SANDBOX/$1"
    rm -rf "$up" "$wc"
    mkdir -p "$up"
    (
        set -e
        cd "$up"
        git init -q --bare
    )
    git clone -q "$up" "$wc" 2>/dev/null   # "cloned an empty repository" is expected here
    (
        set -e
        cd "$wc"
        printf 'base\n'   > upstream.txt
        printf 'line1\nline2\nline3\nline4\nline5\nline6\n' > shared.txt
        printf 'mine\n'   > local.txt
        printf 'rust-version = "1.92"\n' > Cargo.toml
        git add -A && git commit -qm "base"
        git push -q origin master
        # The commit the working copy will be behind by.
        printf 'updated upstream\n' > upstream.txt
        printf 'line1-CHANGED\nline2\nline3\nline4\nline5\nline6\n' > shared.txt
        git add -A && git commit -qm "upstream work"
        git push -q origin master
        git reset -q --hard HEAD~1
    )
    printf '%s\n' "$wc"
}

# Points the sourced script's globals at a given working copy.
use_repo() {
    REPO_ROOT="$1"
    ORIGIN_URL="$2"
    cd "$REPO_ROOT" || exit 1
    GIT="git"
    ADOPTED=0
    DISCARD_LOCAL=0; KEEP_LOCAL=0; OFFLINE=0; ASSUME_YES=1
}

# Runs git_stage in a subshell so a `die` cannot abort the test run.
run_git_stage() { ( git_stage >/dev/null 2>&1 ); printf '%s\n' "$?"; }

# ---------------------------------------------------------------------------
note "Версии"
# ---------------------------------------------------------------------------

check "extract_version rustc"   "$(extract_version 'rustc 1.96.1 (31fca3adb 2026-06-26)')" "1.96.1"
check "extract_version nightly" "$(extract_version 'rustc 1.93.0-nightly (abc 2026-01-01)')" "1.93.0"
check "extract_version beta"    "$(extract_version 'rustc 1.92.0-beta.3 (abc 2026-01-01)')" "1.92.0"
check "extract_version git"     "$(extract_version 'git version 2.43.0')" "2.43.0"

version_ge "1.96.1" "1.92";  check "1.96.1 >= 1.92"  "$?" "0"
version_ge "1.92.0" "1.92";  check "1.92.0 >= 1.92"  "$?" "0"
version_ge "1.91.9" "1.92";  check "1.91.9 <  1.92"  "$?" "1"
version_ge "1.9"    "1.92";  check "1.9 < 1.92 (не строковое сравнение)" "$?" "1"
version_ge "2.13.0" "2.13.0"; check "2.13.0 >= 2.13.0" "$?" "0"

REPO_ROOT="$SELF_DIR/../.."
check "required_msrv читает Cargo.toml проекта" "$(required_msrv)" "1.92"

# ---------------------------------------------------------------------------
note "Чистая рабочая копия, отставшая от origin"
# ---------------------------------------------------------------------------

WC=$(make_pair clean)
use_repo "$WC" "$SANDBOX/clean.git"
RC=$(run_git_stage)
check "код возврата"            "$RC" "0"
check "обновилась до origin"    "$(cat upstream.txt)" "updated upstream"
check "нет отставания"          "$(git rev-list --count HEAD..origin/master)" "0"

# ---------------------------------------------------------------------------
note "Локальные правки НЕ пересекаются с новыми коммитами"
# ---------------------------------------------------------------------------

WC=$(make_pair disjoint)
use_repo "$WC" "$SANDBOX/disjoint.git"
printf 'my local edit\n' > local.txt
printf 'user data\n'     > untracked-data.bin
RC=$(run_git_stage)
check "код возврата"                  "$RC" "0"
check "обновление применено"          "$(cat upstream.txt)" "updated upstream"
check "локальная правка сохранена"    "$(cat local.txt)" "my local edit"
check "untracked-файл не тронут"      "$(cat untracked-data.bin)" "user data"
check "stash пуст (изменения на месте)" "$(git stash list | wc -l | tr -d ' ')" "0"

# ---------------------------------------------------------------------------
note "Правки в ТОМ ЖЕ файле, но git способен слить"
# ---------------------------------------------------------------------------

WC=$(make_pair overlap_ok)
use_repo "$WC" "$SANDBOX/overlap_ok.git"
# Upstream changed line1 of shared.txt; the user changes line6 of the same file.
printf 'line1\nline2\nline3\nline4\nline5\nline6-MINE\n' > shared.txt
RC=$(run_git_stage)
check "код возврата"                 "$RC" "0"
check "правка upstream применена"    "$(sed -n 1p shared.txt)" "line1-CHANGED"
check "правка пользователя сохранена" "$(sed -n 6p shared.txt)" "line6-MINE"

# ---------------------------------------------------------------------------
note "Настоящий конфликт: та же строка"
# ---------------------------------------------------------------------------

WC=$(make_pair conflict)
use_repo "$WC" "$SANDBOX/conflict.git"
printf 'line1-MINE\nline2\nline3\nline4\nline5\nline6\n' > shared.txt
printf 'user data\n' > untracked-data.bin
BEFORE_HEAD=$(git rev-parse HEAD)
BEFORE_FILE=$(cat shared.txt)
RC=$(run_git_stage)
check "код возврата = 3 (нужно ручное слияние)" "$RC" "3"
check "HEAD возвращён на место"          "$(git rev-parse HEAD)" "$BEFORE_HEAD"
check "файл возвращён в исходный вид"    "$(cat shared.txt)" "$BEFORE_FILE"
check "нет маркеров конфликта"           "$(grep -c '<<<<<<<' shared.txt)" "0"
check "не осталось слияния в процессе"   "$(test -e .git/MERGE_HEAD && echo yes || echo no)" "no"
check "untracked-файл не тронут"         "$(cat untracked-data.bin)" "user data"
check "stash не оставлен"                "$(git stash list | wc -l | tr -d ' ')" "0"

# ---------------------------------------------------------------------------
note "Вариант «убрать локальные изменения»"
# ---------------------------------------------------------------------------

WC=$(make_pair discard)
use_repo "$WC" "$SANDBOX/discard.git"
printf 'line1-MINE\nline2\nline3\nline4\nline5\nline6\n' > shared.txt
DISCARD_LOCAL=1
RC=$(run_git_stage)
check "код возврата"                    "$RC" "0"
check "обновление применено"            "$(sed -n 1p shared.txt)" "line1-CHANGED"
check "изменения не уничтожены, а в stash" "$(git stash list | wc -l | tr -d ' ')" "1"

# ---------------------------------------------------------------------------
note "Копия из архива: git-репозитория нет"
# ---------------------------------------------------------------------------

SRC=$(make_pair zipsrc)
ZIP="$SANDBOX/zipcopy"
cp -r "$SRC" "$ZIP"
rm -rf "$ZIP/.git"
printf 'user data\n' > "$ZIP/untracked-data.bin"
use_repo "$ZIP" "$SANDBOX/zipsrc.git"
RC=$(run_git_stage)
check "код возврата"                 "$RC" "0"
check "репозиторий подключён"        "$(test -d "$ZIP/.git" && echo yes || echo no)" "yes"
check "ветка master"                 "$(git rev-parse --abbrev-ref HEAD)" "master"
check "файлы доведены до origin"     "$(cat "$ZIP/upstream.txt")" "updated upstream"
check "untracked-файл не тронут"     "$(cat "$ZIP/untracked-data.bin")" "user data"
check "upstream настроен"            "$(git rev-parse --abbrev-ref master@{upstream} 2>/dev/null)" "origin/master"

# ---------------------------------------------------------------------------
note "Уже актуальная копия"
# ---------------------------------------------------------------------------

WC=$(make_pair current)
use_repo "$WC" "$SANDBOX/current.git"
git merge -q --ff-only origin/master
RC=$(run_git_stage)
check "код возврата"        "$RC" "0"
check "ничего не сломалось" "$(git rev-list --count HEAD..origin/master)" "0"

# ---------------------------------------------------------------------------

printf '\n\033[1mИтого: %d пройдено, %d провалено\033[0m\n' "$PASS" "$FAIL"
[ "$FAIL" = "0" ] || exit 1
