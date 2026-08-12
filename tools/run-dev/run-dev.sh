#!/usr/bin/env bash
#
# File: tools/run-dev/run-dev.sh
#
# Purpose:
# POSIX core of `run-dev` (Linux + macOS): bring the working copy up to date with
# origin, provision a Rust toolchain satisfying the crate MSRV, then run the app
# with `cargo run --bin manhwastudio_rs --release`.
#
# Main responsibilities:
# - Stage 1 (git): locate git, adopt a non-repository ZIP copy, fetch, and merge
#   local changes automatically when they do not truly conflict; detect that the
#   update rewrote run-dev itself and ask for a restart instead of continuing.
# - Stage 2 (rust): read the MSRV from Cargo.toml, pick a system or managed
#   toolchain, provision an isolated one under installer_files/ when needed.
# - Stage 3: two sequential cargo runs — an environment check that only shows a
#   GUI when something is missing, then the application itself.
#
# Key functions:
# - git_stage(), adopt_repository(), update_with_local_changes()
# - normalize_self_eol(), capture_self_fingerprints(), self_changed_files(),
#   self_updated(), check_self_update()
# - is_object_id(), to_count(), stash_top(), quote_args(), restart_command()
# - rust_stage(), required_msrv(), ensure_c_toolchain()
# - cargo_run_app(), check_environment(), run_stage()
#
# Notes:
# The algorithm, its rationale, and every failure path are specified in
# `dev-docs/run_dev_plan.md`. Windows is a separate implementation (run-dev.ps1).
# Written for bash 3.2 so it runs on the bash macOS still ships: no associative
# arrays, no `mapfile`, no `${var^^}`.
# INVARIANT: the single call `main "$@"` is the LAST statement in the file, after
# every definition, and it is followed by `exit`. bash reads a script
# incrementally and parses a function body only when it reaches that definition —
# so what makes this file safe against Stage 1 rewriting it mid-run is the
# placement, not the use of functions as such: by the time `main` starts, the
# reader has consumed the file to EOF, and the trailing `exit` guarantees it
# never comes back for more. Do not add executable statements after that call.
# User-facing output is Russian by project convention; code comments are English.

set -uo pipefail

# --- Constants ---------------------------------------------------------------

# Overridable so `tools/run-dev/test_run_dev.sh` can point the git stage at a
# local throwaway repository, and so a fork can retarget without editing code.
ORIGIN_URL="${MS_RUN_DEV_ORIGIN:-https://github.com/Vasyanator/ManhwaStudio.git}"
BRANCH="${MS_RUN_DEV_BRANCH:-master}"
APP_BIN="manhwastudio_rs"
# `git stash push` (used by every rollback path) landed in git 2.13.
GIT_MIN="2.13.0"

# Files that are *executed* by a run-dev launch. When the git stage rewrites any
# of them the running scripts no longer match what is on disk, so the run stops
# and asks for a restart. Test/doc files of the module are deliberately absent:
# changing them cannot affect an in-flight run.
SELF_FILES="tools/run-dev/run-dev.sh tools/run-dev/run-dev.ps1 run-dev.Linux.sh run-dev.MacOS.command run-dev.Windows.bat"

# Exit codes; see dev-docs/run_dev_plan.md.
EXIT_GENERIC=1
EXIT_NO_GIT=2
EXIT_MANUAL_MERGE=3
EXIT_NO_RUST=4
EXIT_NO_CC=5
EXIT_ABORTED=6
EXIT_VENV_NOT_READY=7
EXIT_RESTART_REQUIRED=8

# --- Options -----------------------------------------------------------------

DO_UPDATE=1
OFFLINE=0
DISCARD_LOCAL=0
KEEP_LOCAL=0
RELEASE=1
ASSUME_YES=0
APP_ARGS=""

# --- State -------------------------------------------------------------------

REPO_ROOT=""
GIT=""
CARGO=""
MS_OS=""
ADOPTED=0
MANAGED_RUST=0
# Fingerprints of SELF_FILES taken before the git stage touched the tree.
SELF_BEFORE=""
# The command line this run was started with, echoed back in the restart hint.
RUN_DEV_ARGV=""

# --- Output helpers ----------------------------------------------------------

if [ -t 1 ]; then
    C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_RED=$'\033[31m'
    C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_BLUE=$'\033[36m'
else
    C_RESET=""; C_BOLD=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_BLUE=""
fi

say()   { printf '%s\n' "$*"; }
step()  { printf '\n%s==> %s%s\n' "$C_BOLD$C_BLUE" "$*" "$C_RESET"; }
ok()    { printf '%s  ok%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn()  { printf '%s  ! %s%s\n' "$C_YELLOW" "$*" "$C_RESET" >&2; }
info()  { printf '    %s\n' "$*"; }

# Prints a framed block titled `$3` in colour `$2` and exits with `$1`.
# Remaining arguments are the body lines.
banner_exit() {
    local code="$1" color="$2" title="$3"; shift 3
    printf '\n%s' "$color$C_BOLD"
    printf '============================================================\n'
    printf '  %s\n' "$title"
    printf '============================================================%s\n' "$C_RESET"
    local line
    for line in "$@"; do printf '  %s\n' "$line"; done
    printf '\n'
    exit "$code"
}

# Prints a framed error block and exits with `$1`.
die() {
    local code="$1"; shift
    banner_exit "$code" "$C_RED" "ОШИБКА" "$@"
}

# Prints a framed informational block (not a failure) and exits with `$1`.
notice_exit() {
    local code="$1" title="$2"; shift 2
    banner_exit "$code" "$C_YELLOW" "$title" "$@"
}

usage() {
    cat <<'EOF'
Запуск dev-версии ManhwaStudio из исходников.

  run-dev [опции] [-- аргументы приложения]

Опции:
  --no-update       Не обновляться из git, сразу собрать и запустить.
  --offline         Не обращаться к сети вообще (проверка окружения пропускается).
  --discard-local   Убрать локальные изменения (в git stash) и обновиться.
  --keep-local      Никогда не трогать локальные изменения.
  --debug           Собрать без --release (быстрее сборка, медленнее работа).
  --yes             Не задавать вопросов, брать варианты по умолчанию.
  -h, --help        Показать эту справку.

Всё, что указано после `--`, передаётся приложению без изменений.
EOF
}

# --- Option parsing ----------------------------------------------------------

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --no-update)     DO_UPDATE=0 ;;
            --offline)       OFFLINE=1 ;;
            --discard-local) DISCARD_LOCAL=1 ;;
            --keep-local)    KEEP_LOCAL=1 ;;
            --debug)         RELEASE=0 ;;
            --yes|-y)        ASSUME_YES=1 ;;
            -h|--help)       usage; exit 0 ;;
            --)              shift; APP_ARGS="$*"; break ;;
            *) die "$EXIT_GENERIC" "Неизвестная опция: $1" \
                                   "Список опций: run-dev --help" ;;
        esac
        shift
    done

    if [ "$DISCARD_LOCAL" = 1 ] && [ "$KEEP_LOCAL" = 1 ]; then
        die "$EXIT_GENERIC" "--discard-local и --keep-local взаимоисключающие."
    fi
}

# --- Small utilities ---------------------------------------------------------

have() { command -v "$1" >/dev/null 2>&1; }

# Reads one line from the controlling terminal. Echoes `$1` (the default) when
# running non-interactively or under --yes, so no path can block on a prompt.
ask() {
    local default="$1" answer
    if [ "$ASSUME_YES" = 1 ] || [ ! -r /dev/tty ]; then
        printf '%s\n' "$default"
        return 0
    fi
    if ! IFS= read -r answer </dev/tty; then
        printf '%s\n' "$default"
        return 0
    fi
    if [ -z "$answer" ]; then answer="$default"; fi
    printf '%s\n' "$answer"
}

# Normalises "rustc 1.96.1 (hash date)" / "git version 2.43.0" style output to a
# bare `major.minor.patch`, dropping any -nightly / -beta.N suffix.
extract_version() {
    printf '%s\n' "$1" \
        | tr ' ' '\n' \
        | grep -E '^[0-9]+\.[0-9]+' \
        | head -n1 \
        | sed -E 's/[-+].*$//'
}

# True when `$1` is a bare git object id: 40 hex characters (SHA-1) or 64
# (SHA-256 repositories). Anything else — empty output, a warning, a truncated
# line — is not a usable fingerprint and must not be compared as one.
is_object_id() {
    case "${#1}" in
        40|64) ;;
        *) return 1 ;;
    esac
    case "$1" in
        *[!0-9a-f]*) return 1 ;;
    esac
    return 0
}

# Prints `$1` when it is a bare non-negative number, and 0 otherwise.
# Strictly whole-string on purpose: fishing a digit group out of arbitrary text
# would turn any stray message containing a number into a commit count, and
# "behind = 1" opens the update branch on a repository with nothing incoming.
to_count() {
    case "$1" in
        ""|*[!0-9]*) printf '0\n' ;;
        *) printf '%s\n' "$1" ;;
    esac
}

# True when version `$1` >= version `$2`. Missing components count as 0.
version_ge() {
    local a b i av bv
    for i in 1 2 3; do
        a=$(printf '%s\n' "$1" | cut -d. -f"$i"); b=$(printf '%s\n' "$2" | cut -d. -f"$i")
        av=$((${a:-0} + 0)); bv=$((${b:-0} + 0))
        if [ "$av" -gt "$bv" ]; then return 0; fi
        if [ "$av" -lt "$bv" ]; then return 1; fi
    done
    return 0
}

detect_os() {
    case "$(uname -s)" in
        Linux)  MS_OS="linux" ;;
        Darwin) MS_OS="macos" ;;
        *) die "$EXIT_GENERIC" \
               "Этот скрипт рассчитан на Linux и macOS, а система определилась как: $(uname -s)." \
               "На Windows используйте run-dev.Windows.bat." ;;
    esac
}

# Prints the package-manager command that installs `$1` on this machine.
install_hint() {
    local pkg_linux="$1"
    if [ "$MS_OS" = "macos" ]; then
        if have brew; then printf 'brew install %s\n' "$2"
        else printf 'xcode-select --install\n'; fi
        return
    fi
    if   have apt-get; then printf 'sudo apt-get install %s\n' "$pkg_linux"
    elif have dnf;     then printf 'sudo dnf install %s\n' "$pkg_linux"
    elif have pacman;  then printf 'sudo pacman -S %s\n' "$pkg_linux"
    elif have zypper;  then printf 'sudo zypper install %s\n' "$pkg_linux"
    elif have apk;     then printf 'sudo apk add %s\n' "$pkg_linux"
    else printf 'установите пакет: %s\n' "$pkg_linux"; fi
}

# Downloads `$1` to `$2` using whichever of curl/wget exists.
download() {
    local url="$1" dest="$2"
    if have curl; then
        curl --proto '=https' --tlsv1.2 -fsSL "$url" -o "$dest"
    elif have wget; then
        wget -q "$url" -O "$dest"
    else
        return 127
    fi
}

# =============================================================================
# Stage 1 — git
# =============================================================================

locate_git() {
    if ! have git; then
        local hint
        hint=$(install_hint "git" "git")
        die "$EXIT_NO_GIT" \
            "В системе не найден git — без него нельзя обновиться из репозитория." \
            "" \
            "Установите его командой:" \
            "    $hint" \
            "" \
            "После установки запустите этот скрипт снова." \
            "Либо запустите без обновления:  run-dev --no-update"
    fi
    GIT="git"

    local ver
    ver=$(extract_version "$($GIT --version 2>/dev/null)")
    if [ -n "$ver" ] && ! version_ge "$ver" "$GIT_MIN"; then
        die "$EXIT_NO_GIT" \
            "Версия git слишком старая: $ver (нужна $GIT_MIN или новее)." \
            "Обновите git и запустите снова, либо: run-dev --no-update"
    fi
}

git_q() { $GIT "$@" >/dev/null 2>&1; }

is_repository() { git_q rev-parse --git-dir; }

# Grafts history onto a working copy that came from a source ZIP, WITHOUT
# writing anything into the working tree. `update-ref` + `symbolic-ref` + a bare
# `reset --mixed` is used instead of `checkout -B`, which would refuse to run (or
# would overwrite files) on an already-populated directory.
adopt_repository() {
    step "Рабочая копия не является git-репозиторием (похоже, распакована из архива)"
    if [ "$OFFLINE" = 1 ]; then
        warn "Режим --offline: подключить репозиторий нельзя, обновление пропущено."
        return 1
    fi
    info "Подключаю историю из $ORIGIN_URL, файлы на диске при этом не трогаются."

    if ! git_q init; then
        die "$EXIT_GENERIC" "Не удалось выполнить git init в $REPO_ROOT."
    fi
    if ! git_q remote add origin "$ORIGIN_URL"; then
        git_q remote set-url origin "$ORIGIN_URL" || true
    fi
    if ! $GIT fetch -q origin; then
        die "$EXIT_GENERIC" \
            "Не удалось скачать историю репозитория." \
            "Проверьте интернет-соединение и доступность $ORIGIN_URL." \
            "Запустить без обновления: run-dev --no-update"
    fi
    if ! git_q rev-parse --verify "refs/remotes/origin/$BRANCH"; then
        die "$EXIT_GENERIC" "В репозитории нет ветки origin/$BRANCH."
    fi

    $GIT update-ref "refs/heads/$BRANCH" "refs/remotes/origin/$BRANCH"
    $GIT symbolic-ref HEAD "refs/heads/$BRANCH"
    $GIT reset --mixed -q               # index := HEAD tree; working tree untouched
    git_q branch --set-upstream-to="origin/$BRANCH" "$BRANCH" || true

    ADOPTED=1
    ok "История подключена, текущая ветка: $BRANCH"
    return 0
}

# After adoption the local files are whatever the ZIP contained. Git cannot tell
# a stale release file from a deliberate edit, so this never guesses silently.
handle_adopted_tree() {
    local changed
    changed=$($GIT status --porcelain --untracked-files=no | wc -l | tr -d ' ')

    if [ "$changed" = "0" ]; then
        ok "Файлы на диске полностью совпадают с origin/$BRANCH."
        return 0
    fi

    step "Файлы из архива отличаются от актуальной версии: $changed шт."
    if [ "$KEEP_LOCAL" = 1 ]; then
        warn "Указан --keep-local: оставляю файлы как есть."
        return 0
    fi
    info "Скорее всего архив просто старее репозитория."
    info "Непроиндексированные файлы (проекты, модели, настройки) не затрагиваются."
    say ""
    say "  [1] Взять актуальную версию файлов  (текущее содержимое сохранится в git stash)"
    say "  [2] Оставить файлы как есть"
    say "  [3] Выйти"
    printf 'Ваш выбор [1]: '
    local choice; choice=$(ask "1")

    case "$choice" in
        1) stash_local "run-dev: содержимое архива $(date '+%Y-%m-%d %H:%M')"
           ok "Файлы обновлены до origin/$BRANCH." ;;
        2) warn "Файлы оставлены без изменений." ;;
        *) die "$EXIT_ABORTED" "Отменено пользователем." ;;
    esac
}

# Prints the object id of the top stash entry, or nothing when the stash is
# empty. Used to tell "push created an entry" from "push had nothing to save",
# which `git stash push` reports with the same exit code 0.
stash_top() {
    local top
    top=$($GIT rev-parse -q --verify refs/stash 2>/dev/null) || top=""
    is_object_id "$top" || top=""
    printf '%s\n' "$top"
}

# Prints the `stash@{N}` selector of the entry whose commit id is `$1`, or
# nothing when that entry is not in the stash any more.
#
# The stash is a stack shared with everything else on the machine — an IDE, a
# GUI client, a second terminal. Between our push and our pop somebody else may
# have pushed (their entry is now on top) or dropped one (indices shifted), so an
# entry must always be addressed by identity, never by position.
stash_ref_for() {
    local id="$1"
    is_object_id "$id" || return 1
    $GIT stash list --format='%H %gd' 2>/dev/null \
        | awk -v id="$id" '$1 == id { print $2; exit }'
}

# Pops exactly the stash entry with commit id `$1`. Returns non-zero when that
# entry is gone or when the pop failed (a conflicting pop keeps the entry, so it
# stays addressable by the same id).
pop_stash_entry() {
    local ref
    ref=$(stash_ref_for "$1") || return 1
    [ -n "$ref" ] || return 1
    $GIT stash pop -q "$ref"
}

# Restores the entry `$1` created by this run, warning instead of failing when it
# cannot. Used on rollback paths, where the caller is already on its way to `die`
# with its own message: losing the entry must be reported, not hidden, but it
# must not replace the primary error either.
restore_stash_entry() {
    [ -n "$1" ] || return 0
    if pop_stash_entry "$1"; then return 0; fi
    warn "Локальные изменения остались в git stash — посмотрите: git stash list"
    return 1
}

# Saves tracked modifications into the stash. Untracked files are NEVER included:
# the working directory holds the user's projects, models and configs.
# Returns 0 when an entry was actually created, 1 when there was nothing to save
# (which `git stash push` also reports as success) — telling the user their work
# is recoverable with `git stash pop` when no entry exists would point them at
# somebody else's older stash.
stash_local() {
    local before after ref
    before=$(stash_top)
    if ! $GIT stash push -q -m "$1"; then
        die "$EXIT_GENERIC" "Не удалось сохранить локальные изменения в git stash."
    fi
    after=$(stash_top)
    if [ "$after" = "$before" ]; then
        warn "Сохранять было нечего: git не нашёл изменений в отслеживаемых файлах."
        return 1
    fi
    # Name the entry explicitly: a bare `git stash pop` takes whatever is on top,
    # which may belong to another process by the time the user runs it.
    ref=$(stash_ref_for "$after")
    if [ -n "$ref" ]; then
        info "Прежнее содержимое сохранено. Вернуть: git stash pop $ref"
    else
        info "Прежнее содержимое сохранено. Найти: git stash list"
    fi
    return 0
}

# Runs the merge appropriate for the ahead/behind relationship. Returns non-zero
# on conflict, leaving no merge in progress.
do_merge() {
    local ahead="$1"
    if [ "$ahead" = "0" ]; then
        $GIT merge --ff-only -q "origin/$BRANCH" && return 0
    else
        $GIT merge --no-edit -q "origin/$BRANCH" && return 0
    fi
    git_q merge --abort || true
    return 1
}

# The core of the tool: update a working copy that has local modifications.
# Rolls the tree back to exactly its pre-update state on any failure.
update_with_local_changes() {
    local ahead="$1"
    local pre_head; pre_head=$($GIT rev-parse HEAD)

    # Three-dot diff = what the INCOMING commits touch (from the merge base),
    # which is the set that can actually collide with local edits.
    local local_files remote_files overlap
    local_files=$($GIT diff --name-only HEAD)
    remote_files=$($GIT diff --name-only "HEAD...origin/$BRANCH")
    overlap=$(printf '%s\n%s\n' "$local_files" "$remote_files" \
              | grep -v '^$' | sort | uniq -d)

    if [ -n "$overlap" ]; then
        info "Локальные правки затрагивают те же файлы, что и новые коммиты:"
        printf '%s\n' "$overlap" | sed 's/^/      /'
        info "Пробую слить автоматически — git часто справляется сам."
    else
        info "Локальные правки не пересекаются с новыми коммитами."
    fi

    # A stash push that saves nothing also exits 0, and the stash is shared with
    # every other git client on the machine. So this records the identity of the
    # entry it created and every restore below pops THAT entry: popping the
    # current top instead would apply a stranger's work to the tree, or nothing
    # at all — corruption, not a cosmetic bug.
    local stash_before our_stash
    stash_before=$(stash_top)
    if ! $GIT stash push -q -m "run-dev: автосохранение перед обновлением"; then
        die "$EXIT_GENERIC" "Не удалось временно сохранить локальные изменения."
    fi
    our_stash=$(stash_top)
    if [ "$our_stash" = "$stash_before" ]; then our_stash=""; fi

    if ! do_merge "$ahead"; then
        $GIT reset --hard -q "$pre_head"
        restore_stash_entry "$our_stash" || true
        die "$EXIT_MANUAL_MERGE" \
            "Не удалось объединить локальные коммиты с новой версией." \
            "Рабочая копия возвращена в исходное состояние, ничего не потеряно." \
            "" \
            "Слейте вручную:  git merge origin/$BRANCH"
    fi

    if [ -z "$our_stash" ]; then
        # Nothing was stashed, so nothing has to come back.
        ok "Обновлено до актуальной версии."
        return 0
    fi

    if ! pop_stash_entry "$our_stash"; then
        if [ -z "$(stash_ref_for "$our_stash")" ]; then
            # The entry is not merely conflicting — it is gone, taken by another
            # process. There is nothing safe left to apply, and guessing at the
            # current top is exactly what must not happen here.
            die "$EXIT_MANUAL_MERGE" \
                "Обновление применено, но вернуть локальные изменения не удалось:" \
                "созданная run-dev запись в git stash исчезла — её мог забрать" \
                "другой git-клиент (IDE, второй терминал)." \
                "" \
                "Посмотрите список сохранённого:  git stash list" \
                "Вернуть нужную запись:           git stash pop stash@{N}"
        fi
        # `stash pop` keeps the stash entry when it conflicts, so the work is
        # safe. `reset --hard` clears the conflicted index, the working tree and
        # the merge commit at once; the pop then replays onto the original base
        # and therefore applies cleanly.
        $GIT reset --hard -q "$pre_head"
        restore_stash_entry "$our_stash" || true
        die "$EXIT_MANUAL_MERGE" \
            "Локальные изменения конфликтуют с новой версией — нужно ручное слияние." \
            "Рабочая копия возвращена в исходное состояние, ничего не потеряно." \
            "" \
            "Конфликтующие файлы:" \
            "$(printf '%s' "$overlap" | tr '\n' ' ')" \
            "" \
            "Варианты:" \
            "  1) слить вручную:            git merge origin/$BRANCH" \
            "  2) убрать локальные правки:  run-dev --discard-local" \
            "     (они сохранятся в git stash, вернуть можно через git stash pop)"
    fi

    ok "Обновлено, локальные изменения сохранены."
}

# --- Self-update detection ---------------------------------------------------
#
# Stage 1 updates the working copy, and run-dev is part of that working copy: a
# run started with the old scripts can end up executing a mix of old and new
# logic. The fix is not to be clever about reloading, it is to notice and ask for
# a restart.

# Prints "<path> <hash>" for every SELF_FILES entry. `git hash-object` is used
# because git is already located by the time this runs and it works outside a
# repository too.
#
# `--no-filters` is mandatory, not a detail: without it git applies the path's
# clean/eol filter, so under `core.autocrlf=true` a CRLF and an LF copy of the
# same file hash identically. An update that rewrites run-dev.Windows.bat only in
# its line endings would then pass unnoticed — and that is exactly the byte-level
# change cmd.exe cannot survive mid-run. It also stops a change to .gitattributes
# from appearing as a change to every file at once.
#
# A file absent on this platform is recorded as "-" (it exists in the repository
# regardless of the OS, so its appearance is a real change); anything that is not
# a well-formed object id — a failed command, empty output, a stray line — is
# recorded as "?", which self_changed_files treats as "unknown, not changed".
# The shape is validated rather than trusted: a fingerprint is only useful if it
# can be compared, and text that merely happens to be non-empty compares unequal
# to itself on the next run.
capture_self_fingerprints() {
    local f h
    for f in $SELF_FILES; do
        if [ -f "$REPO_ROOT/$f" ]; then
            h=$($GIT hash-object --no-filters -- "$REPO_ROOT/$f" 2>/dev/null)
            is_object_id "$h" || h="?"
        else
            h="-"
        fi
        printf '%s %s\n' "$f" "$h"
    done
}

# Restores the self-files whose working copy differs from the index in line
# endings ONLY, before the "before" snapshot is taken.
#
# Why such files exist: `.gitattributes` pins each of them to a specific `eol`,
# while a working copy created earlier (a different `core.autocrlf`, a source ZIP,
# an editor) may hold the other convention. Git does not repair that by itself —
# but `git stash push` does, because it checks the file back out through the
# attribute. So a plain "update with local changes" silently rewrites the bytes of
# scripts that are executing right now, which the fingerprints then report as a
# self-update: a restart request for an update that changed nothing, repeated on
# every run because the state never settles.
#
# Why it cannot lose work — three conditions, all required, because this function
# overwrites a file from the index and any doubt means "leave it alone":
#   1. `git status` must SUCCEED. Its empty output is only meaningful when the
#      command actually ran.
#   2. The porcelain state must be exactly one line of plain unstaged
#      modification (" M "). Merge conflicts and every other XY state are none of
#      this function's business.
#   3. Emptiness of the diff is decided by the EXIT CODE of
#      `git diff --quiet --no-ext-diff`, never by captured output. A diff that
#      fails — a configured-and-broken GIT_EXTERNAL_DIFF, a broken diff driver —
#      writes nothing to stdout, and reading that as "no differences" would
#      destroy the user's unsaved edits. `--no-ext-diff` keeps an external
#      differ out of the decision in the first place.
# Exit code 0 then means: identical to the index once filters are applied, i.e.
# the difference is exactly the byte encoding.
#
# Rewriting a file that is being executed is safe here for the same structural
# reason the update itself is (see the header): every interpreter has already
# read past the point it would need to re-read. It is also idempotent — after one
# pass the state matches the attribute and later runs find nothing to do.
normalize_self_eol() {
    local f path st rc
    for f in $SELF_FILES; do
        path="$REPO_ROOT/$f"
        [ -f "$path" ] || continue

        st=$($GIT status --porcelain --untracked-files=no -- "$path" 2>/dev/null)
        rc=$?
        [ "$rc" = 0 ] || continue
        [ -n "$st" ] || continue
        [ "$(printf '%s\n' "$st" | wc -l | tr -d ' ')" = "1" ] || continue
        case "$st" in
            " M "*) ;;
            *) continue ;;
        esac

        $GIT diff --quiet --no-ext-diff -- "$path" 2>/dev/null || continue

        git_q checkout -- "$path" || true
    done
}

# Prints the fingerprint recorded for file `$2` inside snapshot `$1`, or nothing.
fingerprint_of() {
    printf '%s\n' "$1" | awk -v f="$2" '$1 == f { print $2; exit }'
}

# Prints, one per line, the SELF_FILES entries whose fingerprint differs between
# snapshot `$1` (taken before the update) and snapshot `$2` (taken after it).
# A "?" on EITHER side means the hash was unknown at that moment, which is never
# evidence of a change: a git hiccup between the two snapshots must not fabricate
# a restart request for an update that touched nothing.
self_changed_files() {
    local f a b
    for f in $SELF_FILES; do
        a=$(fingerprint_of "$1" "$f")
        b=$(fingerprint_of "$2" "$f")
        if [ "$a" = "?" ] || [ "$b" = "?" ]; then continue; fi
        if [ "$a" != "$b" ]; then printf '%s\n' "$f"; fi
    done
}

# True when the update rewrote at least one file this run is executing.
self_updated() {
    [ -n "$(self_changed_files "$1" "$2")" ]
}

# Renders the given arguments as one copy-pasteable command line, single-quoting
# any argument that is empty or contains whitespace. Display only — the result is
# never re-executed, so it does not try to be a general shell quoter.
quote_args() {
    local out="" a
    for a in "$@"; do
        case "$a" in
            ""|*[[:space:]]*) a="'$a'" ;;
        esac
        if [ -z "$out" ]; then out="$a"; else out="$out $a"; fi
    done
    printf '%s\n' "$out"
}

# The command that starts run-dev again on this platform, with the same options.
restart_command() {
    # detect_os only ever yields linux or macos; the Linux launcher is the
    # default so the variable can never end up unset under `set -u`.
    local launcher="./run-dev.Linux.sh"
    if [ "$MS_OS" = "macos" ]; then launcher="./run-dev.MacOS.command"; fi
    if [ -n "$RUN_DEV_ARGV" ]; then
        printf '%s %s\n' "$launcher" "$RUN_DEV_ARGV"
    else
        printf '%s\n' "$launcher"
    fi
}

# Stops the run when Stage 1 rewrote run-dev itself. Nothing is rolled back: the
# update is applied and correct, only the running scripts are stale.
check_self_update() {
    [ -n "$SELF_BEFORE" ] || return 0
    local after
    after=$(capture_self_fingerprints)
    self_updated "$SELF_BEFORE" "$after" || return 0

    # One positional argument per line: banner_exit indents each argument it is
    # given, so passing the file list as a single multi-line string would indent
    # only its first line. The here-document (not a pipe) keeps `set --` in this
    # shell instead of a subshell.
    set -- "Обновление затронуло сам скрипт запуска run-dev." "" "Обновлены файлы:"
    local f
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        set -- "$@" "    $f"
    done <<EOF
$(self_changed_files "$SELF_BEFORE" "$after")
EOF

    notice_exit "$EXIT_RESTART_REQUIRED" "НУЖЕН ПЕРЕЗАПУСК" "$@" \
        "" \
        "Обновление уже применено, откатывать ничего не нужно." \
        "Запустите run-dev ещё раз, чтобы дальше работала новая версия:" \
        "" \
        "    $(restart_command)"
}

git_stage() {
    step "Проверка обновлений"
    locate_git
    # Settle any line-ending-only mismatch first, otherwise `git stash push`
    # would settle it mid-update and the fingerprints would read that as a
    # self-update. Then snapshot: from here on, any change to these files really
    # did come from the update.
    normalize_self_eol
    SELF_BEFORE=$(capture_self_fingerprints)

    if ! is_repository; then
        if ! adopt_repository; then return 0; fi
        handle_adopted_tree
        return 0
    fi

    if ! git_q remote get-url origin; then
        info "Удалённый репозиторий не настроен, добавляю origin."
        git_q remote add origin "$ORIGIN_URL" || true
    fi

    if [ "$OFFLINE" = 1 ]; then
        warn "Режим --offline: проверка обновлений пропущена."
        return 0
    fi

    if ! $GIT fetch --prune -q origin "$BRANCH" 2>/dev/null; then
        # Being offline must never block running the app.
        warn "Не удалось связаться с репозиторием — обновление пропущено."
        return 0
    fi

    local behind ahead
    behind=$(to_count "$($GIT rev-list --count "HEAD..origin/$BRANCH" 2>/dev/null)")
    ahead=$(to_count "$($GIT rev-list --count "origin/$BRANCH..HEAD" 2>/dev/null)")

    if [ "$behind" = "0" ]; then
        ok "Установлена актуальная версия."
        return 0
    fi

    info "Доступно новых коммитов: $behind"
    [ "$ahead" != "0" ] && info "Локальных коммитов, которых нет в origin: $ahead"

    local dirty
    dirty=$($GIT status --porcelain --untracked-files=no)

    if [ -z "$dirty" ]; then
        if do_merge "$ahead"; then
            ok "Обновлено до актуальной версии."
        else
            die "$EXIT_MANUAL_MERGE" \
                "Локальные коммиты конфликтуют с новой версией." \
                "Слияние отменено, рабочая копия не изменена." \
                "" \
                "Слейте вручную:  git merge origin/$BRANCH"
        fi
        return 0
    fi

    local changed
    changed=$(printf '%s\n' "$dirty" | wc -l | tr -d ' ')
    info "Изменённых файлов в рабочей копии: $changed"

    local choice
    if   [ "$DISCARD_LOCAL" = 1 ]; then choice="2"
    elif [ "$KEEP_LOCAL" = 1 ];    then choice="3"
    else
        say ""
        say "  [1] Попробовать слить автоматически            (по умолчанию)"
        say "  [2] Убрать локальные изменения и обновиться    (сохранятся в git stash)"
        say "  [3] Пропустить обновление и запустить как есть"
        say "  [4] Выйти"
        printf 'Ваш выбор [1]: '
        choice=$(ask "1")
    fi

    case "$choice" in
        1) update_with_local_changes "$ahead" ;;
        2) stash_local "run-dev: отброшенные изменения $(date '+%Y-%m-%d %H:%M')"
           if do_merge "$ahead"; then
               ok "Обновлено. Локальные изменения лежат в git stash."
           else
               die "$EXIT_MANUAL_MERGE" "Не удалось обновиться даже после очистки рабочей копии."
           fi ;;
        3) warn "Обновление пропущено по вашему выбору." ;;
        *) die "$EXIT_ABORTED" "Отменено пользователем." ;;
    esac
}

# =============================================================================
# Stage 2 — rust
# =============================================================================

# Reads `rust-version` from [package] in Cargo.toml. There is deliberately no
# fallback constant: a silently wrong MSRV turns into a confusing mid-build type
# error instead of a clear message.
required_msrv() {
    local v
    v=$(grep -E '^[[:space:]]*rust-version[[:space:]]*=' "$REPO_ROOT/Cargo.toml" \
        | head -n1 | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -z "$v" ]; then
        die "$EXIT_GENERIC" \
            "В Cargo.toml не найдено поле rust-version — не с чем сравнивать версию Rust." \
            "Это ошибка в самом проекте, а не в вашей системе."
    fi
    printf '%s\n' "$v"
}

# Prints the version of the rustc belonging to the given cargo, or nothing.
rustc_version_for() {
    local cargo_bin="$1" rustc_bin
    rustc_bin="$(dirname "$cargo_bin")/rustc"
    [ -x "$rustc_bin" ] || rustc_bin="rustc"
    have "$rustc_bin" || return 1
    extract_version "$("$rustc_bin" --version 2>/dev/null)"
}

rust_root()   { printf '%s/installer_files/rust\n' "$REPO_ROOT"; }
managed_cargo() { printf '%s/cargo/bin/cargo\n' "$(rust_root)"; }

use_managed_rust() {
    export RUSTUP_HOME="$(rust_root)/rustup"
    export CARGO_HOME="$(rust_root)/cargo"
    export PATH="$CARGO_HOME/bin:$PATH"
    CARGO="$(managed_cargo)"
    MANAGED_RUST=1
}

# Installs an isolated toolchain under installer_files/. Never writes to
# ~/.cargo, ~/.rustup or any shell profile (--no-modify-path).
install_managed_rust() {
    if [ "$OFFLINE" = 1 ]; then
        die "$EXIT_NO_RUST" \
            "Rust нужной версии не найден, а режим --offline запрещает скачивание." \
            "Запустите без --offline, либо установите Rust самостоятельно: https://rustup.rs"
    fi

    step "Устанавливаю Rust (изолированно, в installer_files/rust)"
    info "Системный Rust и настройки вашей оболочки не затрагиваются."
    info "Скачивается около 250 МБ, это займёт несколько минут."

    local dl_dir="$REPO_ROOT/installer_files/downloads"
    mkdir -p "$dl_dir" "$(rust_root)"
    local init="$dl_dir/rustup-init.sh"

    # `$?` must be captured from `download` itself, not from inside an `if !`
    # branch (where it would be the status of the negation, always 0).
    local rc
    download "https://sh.rustup.rs" "$init"; rc=$?
    if [ "$rc" != "0" ]; then
        if [ "$rc" = "127" ]; then
            die "$EXIT_NO_RUST" \
                "Для скачивания Rust нужен curl или wget, но ни один не найден." \
                "    $(install_hint "curl" "curl")"
        fi
        die "$EXIT_NO_RUST" \
            "Не удалось скачать установщик Rust с https://sh.rustup.rs" \
            "Проверьте интернет-соединение."
    fi

    export RUSTUP_HOME="$(rust_root)/rustup"
    export CARGO_HOME="$(rust_root)/cargo"
    if ! sh "$init" -y --no-modify-path --profile minimal --default-toolchain stable; then
        die "$EXIT_NO_RUST" \
            "Установка Rust завершилась с ошибкой." \
            "Подробности — в выводе выше."
    fi
    rm -f "$init"

    if [ ! -x "$(managed_cargo)" ]; then
        die "$EXIT_NO_RUST" "Rust установился, но cargo не найден по пути $(managed_cargo)."
    fi
    use_managed_rust
    ok "Rust установлен: $(rustc_version_for "$CARGO")"
}

# `aws-lc-sys` (translators/genai -> reqwest -> rustls -> aws-lc-rs) compiles C
# and assembly with `cc` on every native target. Probing here turns a wall of
# linker errors 200 crates into the build, into one clear message before it.
ensure_c_toolchain() {
    if have cc || have gcc || have clang; then return 0; fi
    if [ "$MS_OS" = "macos" ] && xcrun --find cc >/dev/null 2>&1; then return 0; fi

    local hint
    if [ "$MS_OS" = "macos" ]; then
        hint="xcode-select --install"
    elif have apt-get; then hint="sudo apt-get install build-essential pkg-config"
    elif have dnf;     then hint="sudo dnf groupinstall \"Development Tools\""
    elif have pacman;  then hint="sudo pacman -S base-devel"
    elif have zypper;  then hint="sudo zypper install -t pattern devel_basis"
    elif have apk;     then hint="sudo apk add build-base"
    else hint="установите компилятор C (gcc или clang)"; fi

    die "$EXIT_NO_CC" \
        "Не найден компилятор C — без него проект не соберётся." \
        "Он нужен зависимости aws-lc-sys (шифрование в сетевых запросах)," \
        "которая собирает исходники на C и ассемблере." \
        "" \
        "Установите его командой:" \
        "    $hint"
}

rust_stage() {
    step "Проверка Rust"
    local msrv; msrv=$(required_msrv)
    info "Проекту требуется Rust $msrv или новее."

    # 1) A previously provisioned managed toolchain wins: it exists because a
    #    past run decided the system one was unusable.
    if [ -x "$(managed_cargo)" ]; then
        use_managed_rust
        local mv; mv=$(rustc_version_for "$CARGO")
        if [ -n "$mv" ] && version_ge "$mv" "$msrv"; then
            ok "Используется установленный проектом Rust $mv"
            ensure_c_toolchain
            return 0
        fi
        info "Установленный проектом Rust ${mv:-неизвестной версии} устарел, обновляю."
        if "$(rust_root)/cargo/bin/rustup" update stable >/dev/null 2>&1; then
            mv=$(rustc_version_for "$CARGO")
            if [ -n "$mv" ] && version_ge "$mv" "$msrv"; then
                ok "Rust обновлён до $mv"
                ensure_c_toolchain
                return 0
            fi
        fi
        warn "Обновить не удалось, переустанавливаю."
        install_managed_rust
        ensure_c_toolchain
        return 0
    fi

    # 2) A current system toolchain is used as-is — no second copy downloaded.
    if have cargo; then
        local sv; sv=$(extract_version "$(rustc --version 2>/dev/null)")
        if [ -n "$sv" ] && version_ge "$sv" "$msrv"; then
            CARGO="cargo"
            ok "Используется системный Rust $sv"
            ensure_c_toolchain
            return 0
        fi
        info "Системный Rust ${sv:-не определён} не подходит (нужен $msrv+)."
    else
        info "Rust в системе не найден."
    fi

    # 3) Provision.
    install_managed_rust
    ensure_c_toolchain
}

# =============================================================================
# Stage 3 — run
# =============================================================================

# Runs `cargo run --bin <APP_BIN> [--release] -- <$1>` synchronously and returns
# cargo's exit code. `$1` is a flat argument string for the application; it is
# word-split on purpose. Synchronous by contract: on Windows a running .exe
# cannot be relinked, so the two Stage 3 phases must never overlap.
cargo_run_app() {
    local extra="$1"
    set -- run --bin "$APP_BIN"
    [ "$RELEASE" = 1 ] && set -- "$@" --release
    set -- "$@" --
    if [ -n "$extra" ]; then
        # shellcheck disable=SC2086
        set -- "$@" $extra
    fi
    "$CARGO" "$@"
}

# Phase 1 of Stage 3: let the binary itself decide whether the Python
# environment is usable. `--check-venv` opens the installer *only* when something
# is missing and exits 0 without any GUI when everything is in place, so this is
# also what compiles the project — phase 2 then starts instantly.
check_environment() {
    if [ "$OFFLINE" = 1 ]; then
        # --offline is an explicit "do not touch the network" request, and this
        # check may download uv, Python or Torch wheels. Skipping it is the only
        # honest reading; the app reports a broken environment on its own later.
        warn "Режим --offline: проверка окружения Python пропущена."
        info "Если venv или пакетов не хватает, приложение сообщит об этом само."
        return 0
    fi

    step "Проверка окружения приложения"
    info "Сейчас проект собирается — на чистой машине это самая долгая часть."
    info "Окно установки откроется, только если чего-то не хватает."

    say ""
    cargo_run_app "--check-venv --ignore-installed"
    local rc=$?
    if [ "$rc" != "0" ]; then
        die "$EXIT_VENV_NOT_READY" \
            "Окружение приложения не готово, запуск отменён (код $rc)." \
            "" \
            "Причина — одна из двух:" \
            "  1) проект не собрался. Тогда выше видны сообщения компилятора," \
            "     и разбирать нужно именно их: другими флагами это не обходится." \
            "  2) установка окружения была отменена или завершилась с ошибкой." \
            "     Запустите run-dev снова и доведите установку до конца." \
            "" \
            "Во втором случае проверку можно и пропустить: режим --offline её не" \
            "выполняет. Но учтите, что он же отключает обновление из git и" \
            "установку Rust — при отсутствующем Rust запуск завершится ошибкой."
    fi
    ok "Окружение готово."
}

run_stage() {
    step "Сборка и запуск"
    if [ "$RELEASE" = 1 ]; then
        info "Сборка в режиме release. Первый запуск на чистой машине компилирует"
        info "весь проект целиком — это может занять 10-30 минут. Это не зависание."
    else
        info "Сборка в режиме debug (--debug)."
    fi

    # build.rs starts a codesign worker for Windows targets and PROMPTS on the
    # terminal for a .p12 password when .secret/build_config.json is absent.
    # Signing is a release concern; a dev run must never stop on that prompt.
    # An explicitly exported value is respected, so a signed dev build is still
    # possible.
    if [ -z "${MS_DISABLE_BUILD_CODESIGN:-}" ]; then
        export MS_DISABLE_BUILD_CODESIGN=1
    fi

    check_environment

    # Phase 2. `--ignore-installed` goes first: the environment was just checked,
    # so the application must not repeat that check at startup.
    local extra="--ignore-installed"
    if [ -n "$APP_ARGS" ]; then extra="$extra $APP_ARGS"; fi

    say ""
    cargo_run_app "$extra"
    local rc=$?
    if [ "$rc" != "0" ]; then
        warn "Приложение завершилось с кодом $rc."
        if [ -r /dev/tty ] && [ "$ASSUME_YES" != 1 ]; then
            printf 'Нажмите Enter для выхода...'
            IFS= read -r _ </dev/tty || true
        fi
    fi
    exit "$rc"
}

# =============================================================================

main() {
    RUN_DEV_ARGV=$(quote_args "$@")
    parse_args "$@"
    detect_os

    # The repo root is two levels above this script (tools/run-dev/).
    local script_dir
    script_dir=$(cd "$(dirname "$0")" && pwd)
    REPO_ROOT=$(cd "$script_dir/../.." && pwd)
    cd "$REPO_ROOT" || die "$EXIT_GENERIC" "Не удалось перейти в каталог проекта: $REPO_ROOT"

    if [ ! -f "$REPO_ROOT/Cargo.toml" ]; then
        die "$EXIT_GENERIC" \
            "В каталоге $REPO_ROOT нет Cargo.toml." \
            "Скрипт должен лежать в tools/run-dev/ внутри проекта ManhwaStudio."
    fi

    say "${C_BOLD}ManhwaStudio — запуск dev-версии${C_RESET}"
    info "Каталог проекта: $REPO_ROOT"

    if [ "$DO_UPDATE" = 1 ]; then
        git_stage
        # Must come before Stage 2/3: continuing with half-old scripts is exactly
        # what this check exists to prevent.
        check_self_update
    else
        info "Обновление пропущено (--no-update)."
    fi
    rust_stage
    run_stage
}

# `test_run_dev.sh` sources this file to exercise the git stage in isolation
# against a throwaway repository; sourcing must not run the app.
# This call must stay the last statement in the file: bash reads a script
# incrementally, so reaching it means the whole file has been read and parsed,
# and the trailing `exit` guarantees bash never returns to a byte offset that
# Stage 1 may have rewritten in the meantime.
if [ -z "${MS_RUN_DEV_SOURCE_ONLY:-}" ]; then
    main "$@"
    exit $?
fi
