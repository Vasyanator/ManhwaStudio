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
#   local changes automatically when they do not truly conflict.
# - Stage 2 (rust): read the MSRV from Cargo.toml, pick a system or managed
#   toolchain, provision an isolated one under installer_files/ when needed.
# - Stage 3: exec cargo.
#
# Key functions:
# - git_stage(), adopt_repository(), update_with_local_changes()
# - rust_stage(), required_msrv(), ensure_c_toolchain()
#
# Notes:
# The algorithm, its rationale, and every failure path are specified in
# `dev-docs/run_dev_plan.md`. Windows is a separate implementation (run-dev.ps1).
# Written for bash 3.2 so it runs on the bash macOS still ships: no associative
# arrays, no `mapfile`, no `${var^^}`.
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

# Exit codes; see dev-docs/run_dev_plan.md.
EXIT_GENERIC=1
EXIT_NO_GIT=2
EXIT_MANUAL_MERGE=3
EXIT_NO_RUST=4
EXIT_NO_CC=5
EXIT_ABORTED=6

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

# Prints a framed error block and exits with `$1`.
die() {
    local code="$1"; shift
    printf '\n%s' "$C_RED$C_BOLD"
    printf '============================================================\n'
    printf '  ОШИБКА\n'
    printf '============================================================%s\n' "$C_RESET"
    local line
    for line in "$@"; do printf '  %s\n' "$line"; done
    printf '\n'
    exit "$code"
}

usage() {
    cat <<'EOF'
Запуск dev-версии ManhwaStudio из исходников.

  run-dev [опции] [-- аргументы приложения]

Опции:
  --no-update       Не обновляться из git, сразу собрать и запустить.
  --offline         Не обращаться к сети вообще.
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

# Saves tracked modifications into the stash. Untracked files are NEVER included:
# the working directory holds the user's projects, models and configs.
stash_local() {
    if ! $GIT stash push -q -m "$1"; then
        die "$EXIT_GENERIC" "Не удалось сохранить локальные изменения в git stash."
    fi
    info "Прежнее содержимое сохранено. Вернуть: git stash pop"
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

    if ! $GIT stash push -q -m "run-dev: автосохранение перед обновлением"; then
        die "$EXIT_GENERIC" "Не удалось временно сохранить локальные изменения."
    fi

    if ! do_merge "$ahead"; then
        $GIT reset --hard -q "$pre_head"
        git_q stash pop || true
        die "$EXIT_MANUAL_MERGE" \
            "Не удалось объединить локальные коммиты с новой версией." \
            "Рабочая копия возвращена в исходное состояние, ничего не потеряно." \
            "" \
            "Слейте вручную:  git merge origin/$BRANCH"
    fi

    if ! $GIT stash pop -q; then
        # `stash pop` keeps the stash entry when it conflicts, so the work is
        # safe. `reset --hard` clears the conflicted index, the working tree and
        # the merge commit at once; the pop then replays onto the original base
        # and therefore applies cleanly.
        $GIT reset --hard -q "$pre_head"
        git_q stash pop || true
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

git_stage() {
    step "Проверка обновлений"
    locate_git

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
    behind=$($GIT rev-list --count "HEAD..origin/$BRANCH" 2>/dev/null || echo 0)
    ahead=$($GIT rev-list --count "origin/$BRANCH..HEAD" 2>/dev/null || echo 0)

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

    set -- run --bin "$APP_BIN"
    [ "$RELEASE" = 1 ] && set -- "$@" --release
    if [ -n "$APP_ARGS" ]; then
        # APP_ARGS is a flat string; word-splitting it here is the intent.
        # shellcheck disable=SC2086
        set -- "$@" -- $APP_ARGS
    fi

    say ""
    "$CARGO" "$@"
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

    if [ "$DO_UPDATE" = 1 ]; then git_stage; else info "Обновление пропущено (--no-update)."; fi
    rust_stage
    run_stage
}

# `test_run_dev.sh` sources this file to exercise the git stage in isolation
# against a throwaway repository; sourcing must not run the app.
if [ -z "${MS_RUN_DEV_SOURCE_ONLY:-}" ]; then
    main "$@"
fi
