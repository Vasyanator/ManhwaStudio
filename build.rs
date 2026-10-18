/*
FILE OVERVIEW: build.rs
Build script for platform-specific executable metadata and the compile-time version.

Main responsibilities:
- Emits `cargo:rustc-env=MS_APP_VERSION=<version>` — THE program version of this build:
  the commit count of the commit it was built from (`git rev-list --count HEAD`, e.g.
  `335`; no hash, no dirty marker). Displayed, printed by `--version`, compared against
  release tags and handed to other processes (`crates/ms-config/src/version_format.rs`).
  `CARGO_PKG_VERSION` is NEVER overridden: it is packaging metadata, read by `winresource`
  from this script's own environment for the numeric Windows `FILEVERSION`.
  The count is parsed by `version_format::parse_commit_count`, pulled in with `include!`
  so that the exact code this script runs is covered by `cargo test`.
  When no count can be obtained — no `.git` in the manifest directory (a GitHub source ZIP),
  no `git` on PATH, a shallow clone (its count would be too small, i.e. a wrong version),
  a non-zero exit or unparsable output — the version is the FULL `CARGO_PKG_VERSION`
  (`3.6.0`, or `N.0.0` after release N), which compares correctly against tags, and a
  `cargo:warning=` says so.
- Emits the `cargo:rerun-if-changed` watches that keep that version fresh. This is
  NOT optional bookkeeping: this script already emitted `rerun-if-changed`, which
  REPLACES cargo's default "re-run when any package file changed", so without the
  watches below a git-derived value would go stale on the very next build. The git
  directories are resolved with `git rev-parse --git-dir --git-common-dir`, so a linked
  worktree or submodule checkout (where `.git` is a FILE) is covered too. Watched: `HEAD`
  of the worktree's own git dir (a branch switch or detached-HEAD move rewrites it) and, in
  the common dir, `refs/` (recursively: a commit moves the checked-out branch's loose ref,
  including one that does not exist yet while the branch is only packed), `packed-refs`
  and `shallow` when present. The `src` / `crates` directories are watched too: the
  count does not depend on them, but they are what re-runs this script — and with it the
  Windows signer below — on every source change, a behaviour the release workflow relies
  on until the owner decides otherwise (see KNOWN CONSEQUENCE).
- **EVERY watch this script emits — the codesign ones at the top of `main` included —
  is emitted ONLY for a path that actually exists, and that rule is load bearing.**
  Cargo treats a `rerun-if-changed` naming a MISSING path as permanently stale, so it
  re-runs the script on every single cargo invocation; and a build-script re-run
  recompiles the dependent crate EVEN WHEN the script's output is byte-identical. The
  second half is the non-obvious one, and together they turn one missing watch into a
  full recompile of this crate on every build, forever. That is not hypothetical: this
  script watched `.secret/build_config.json` unconditionally, that file is git-ignored
  and thus absent from every fresh clone, and a user paid a three-minute relink on each
  and every `run-dev` launch until it was found. A source ZIP has no `.git` either —
  the same trap, which is why the version watches were written this way from the start.
  Verify with `CARGO_LOG=cargo::core::compiler::fingerprint=info cargo build`: cargo
  names the missing file outright ("Dirty …: the file … is missing").
  KNOWN CONSEQUENCE, awaiting a decision by the project owner: watching `src`/`crates`
  (originally for the git dirty marker, now kept only for this effect) makes this script
  re-run on every source change, so EVERY Windows-target build or
  check that does not set `MS_DISABLE_BUILD_CODESIGN=1` re-spawns the detached
  `osslsigncode` worker below. `cargo check-all` is exactly such an invocation — it
  cross-checks the windows-gnu target and sets nothing — so the routine post-change
  check now leaves a signer waiting up to `SIGN_WAIT_SECONDS` (300 s) per executable
  for `.exe` files that a `cargo check` never produces. Set
  `MS_DISABLE_BUILD_CODESIGN=1` for checks to avoid it. Do NOT change the signing
  behaviour here to work around this: the release workflow is the owner's call.
- On Windows, embeds `app_icon.ico` into the PE resources so the produced `.exe`
  has the correct file icon in Explorer and shell surfaces. When the manifest path
  contains a space — the normal case for a source checkout of an installed copy in
  `C:\Program Files\ManhwaStudio` — `CARGO_MANIFEST_DIR` is swapped for its 8.3 short
  form across that one call: `winresource` hands it to `windres` as `-I<dir>` and
  windres forwards it to the preprocessor unquoted, which otherwise fails the build
  with `cc1.exe: fatal error: Files\ManhwaStudio`.
- On Windows, starts a detached post-build `osslsigncode` worker that waits for
  the package binary's `.exe` (`signed_bin_names`) and signs it with a PKCS#12 certificate, unless
  `MS_DISABLE_BUILD_CODESIGN=1` disables the background signer.
- Codesign credentials (key path + password) are NEVER hardcoded. They are read
  from `.secret/build_config.json` (git/hg-ignored). When that file is missing or
  incomplete the build prompts on the controlling terminal to either enter a key
  path + password (saved back into `.secret/build_config.json`) or build unsigned.
  `MS_CODESIGN_P12` / `MS_CODESIGN_PASSWORD` env vars override the file, for CI.
- On non-Windows targets, performs no-op to keep builds fast and portable.
*/

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// The commit-count parser is shared verbatim with the crate: it is a module of
// `ms-config` (re-exported by the binary as `crate::version_format`, see `src/lib.rs`).
// `include!` rather than a copy, so that the code this script runs is the code
// `cargo test` covers; the file's `#[cfg(test)] mod tests` is inert here.
include!("crates/ms-config/src/version_format.rs");

const SIGN_WAIT_SECONDS: u64 = 300;
const SIGN_TS_URL: &str = "http://timestamp.sectigo.com";
const DISABLE_BUILD_CODESIGN_ENV: &str = "MS_DISABLE_BUILD_CODESIGN";
const SECRET_CONFIG_REL: &str = ".secret/build_config.json";

/// Resolved signing credentials. Absence (`None` from `resolve_credentials`)
/// means "build without signing".
struct CodesignCredentials {
    key_path: PathBuf,
    password: String,
}

fn main() {
    // Both of these are watched ONLY when they exist, for the reason spelled out in this
    // file's header: cargo treats a `rerun-if-changed` on a missing path as permanently
    // stale. `.secret/build_config.json` is git-ignored and therefore absent from every
    // fresh clone, so watching it unconditionally re-ran this script on every single
    // cargo invocation and recompiled the crate each time — three minutes per launch on a
    // real user's machine. Losing the watch when the file is absent is the right trade:
    // the file can only start mattering once it exists, and the `src`/`crates` watches
    // below re-run this script on the next source change anyway; the env-var route is
    // covered by the `rerun-if-env-changed` lines.
    let manifest = manifest_dir();
    for watched in ["app_icon.ico", SECRET_CONFIG_REL] {
        if manifest.join(watched).exists() {
            println!("cargo:rerun-if-changed={watched}");
        }
    }
    println!("cargo:rerun-if-env-changed=MS_CODESIGN_P12");
    println!("cargo:rerun-if-env-changed=MS_CODESIGN_PASSWORD");
    println!("cargo:rerun-if-env-changed={DISABLE_BUILD_CODESIGN_ENV}");

    emit_app_version();

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("app_icon.ico");
        let restore = shorten_manifest_dir_for_windres();
        let compiled = resource.compile();
        restore_manifest_dir(restore.as_deref());
        if let Err(err) = compiled {
            panic!("failed to embed Windows executable icon: {err}");
        }
        if env::var_os(DISABLE_BUILD_CODESIGN_ENV).as_deref() != Some("1".as_ref()) {
            spawn_windows_signer();
        }
    }
}

/// Replaces `CARGO_MANIFEST_DIR` with its DOS 8.3 short form for the duration of the
/// `winresource` call, returning the original value when it was changed.
///
/// `winresource` passes the manifest directory to `windres` as `-I<dir>`, and windres
/// forwards that to the C preprocessor as part of an UNQUOTED command string — so a
/// space in the path arrives split and the build fails at the very end with
/// `cc1.exe: fatal error: Files\ManhwaStudio: No such file or directory`. This is the
/// default situation for a source checkout of an installed copy, which lives in
/// `C:\Program Files\ManhwaStudio`. Measured there: the same build script exits 101
/// with the long path and 0 with the short one. Cargo itself cannot be steered around
/// it — it canonicalises the manifest path, so invoking it from the short directory
/// changes nothing.
///
/// Returns `None` (and changes nothing) when the host is not Windows, when the path
/// holds no space, or when no short form exists — a caller must treat that as "the
/// variable was left alone", not as failure.
#[must_use]
fn shorten_manifest_dir_for_windres() -> Option<String> {
    let current = env::var("CARGO_MANIFEST_DIR").ok()?;
    if !current.contains(' ') {
        return None;
    }
    let short = short_path(&current)?;
    if short.contains(' ') {
        return None;
    }
    // SAFETY: build scripts are single-threaded at this point — the only thread this
    // script ever spawns is the detached signer, started after the resource step below.
    unsafe { env::set_var("CARGO_MANIFEST_DIR", &short) };
    Some(current)
}

/// Puts back the `CARGO_MANIFEST_DIR` that `shorten_manifest_dir_for_windres` replaced.
///
/// A `None` means it was never changed, so nothing is done. Restoring matters because
/// the codesign step below resolves `.secret/build_config.json` through the same
/// variable, and a build script's environment should not outlive the one call that
/// needed it changed.
fn restore_manifest_dir(original: Option<&str>) {
    if let Some(value) = original {
        // SAFETY: as above — still single-threaded here.
        unsafe { env::set_var("CARGO_MANIFEST_DIR", value) };
    }
}

/// Returns the DOS 8.3 short form of an existing Windows path, or `None`.
///
/// Asks `cmd` for the `%~s` expansion rather than calling `GetShortPathNameW`: this
/// script is compiled for the HOST, and the project is routinely cross-built for
/// Windows from Linux, where a Windows-only crate would have to be gated in
/// `Cargo.toml` for this single call. Returns `None` on any failure, including 8.3 name
/// generation being disabled on the volume — every caller treats that as "no short form
/// available".
#[cfg(windows)]
fn short_path(path: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;

    // `raw_arg`, not `arg`: building a command line the normal way escapes an inner `"`
    // as `\"`, and cmd.exe does not understand that escaping — measured on Windows 11,
    // it echoes back `C:\"C:\Program Files\ManhwaStudio\"` instead of the short form.
    // The quotes around the path are required in turn, or `for` would split the set on
    // the space. The command deliberately does not start with a quote, so cmd takes the
    // whole remainder as the command to run instead of stripping the outer pair.
    let output = Command::new("cmd")
        .arg("/C")
        .raw_arg(format!("for %I in (\"{path}\") do @echo %~sI"))
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let short = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if short.is_empty() || !Path::new(&short).exists() {
        return None;
    }
    Some(short)
}

/// Non-Windows hosts have no 8.3 names, and no `windres` path problem to solve.
#[cfg(not(windows))]
fn short_path(_path: &str) -> Option<String> {
    None
}

fn manifest_dir() -> PathBuf {
    env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn secret_config_path() -> PathBuf {
    manifest_dir().join(SECRET_CONFIG_REL)
}

/// Emits `cargo:rustc-env=MS_APP_VERSION` together with the `rerun-if-changed` watches
/// that keep it fresh. See this file's header for the version rule, the rerun policy and
/// its accepted side effect on the Windows codesign worker.
///
/// Never fails the build: when no commit count can be obtained the version is the full
/// `CARGO_PKG_VERSION`, announced with one `cargo:warning=` naming the reason.
fn emit_app_version() {
    let manifest = manifest_dir();
    emit_version_rerun_watches(&manifest);

    let version = match git_commit_count(&manifest) {
        Ok(count) => count.to_string(),
        Err(reason) => {
            let fallback = env::var("CARGO_PKG_VERSION").unwrap_or_default();
            println!("cargo:warning=MS_APP_VERSION: no git commit count ({reason}); using the Cargo.toml version {fallback}");
            fallback
        }
    };
    println!("cargo:rustc-env=MS_APP_VERSION={version}");
}

/// Emits one `cargo:rerun-if-changed` per EXISTING path that can change the version (or, for
/// `src` / `crates`, must re-run this script for the signer; see the file header). Missing
/// paths are skipped on purpose: cargo treats a watch on a missing path as "always changed"
/// and would re-run this script on every build, which is precisely the wrong outcome in a
/// `.git`-less source ZIP.
fn emit_version_rerun_watches(manifest: &Path) {
    // Cargo walks a watched directory recursively.
    for source_dir in ["src", "crates"] {
        if manifest.join(source_dir).is_dir() {
            println!("cargo:rerun-if-changed={source_dir}");
        }
    }

    // Same gate as `git_commit_count`: without a `.git` of its own, the source tree must not
    // adopt (and watch) an ENCLOSING repository.
    if !manifest.join(".git").exists() {
        return;
    }
    // `.git` may be a FILE (linked worktree, submodule checkout) pointing elsewhere, so the
    // real directories come from git itself. A failure means no count either (it would fail
    // the same way), so there is nothing to keep fresh.
    let Some(dirs) = run_git(manifest, &["rev-parse", "--git-dir", "--git-common-dir"]) else {
        return;
    };
    let mut lines = dirs.lines();
    let (Some(git_dir), Some(common_dir)) = (lines.next(), lines.next()) else {
        return;
    };
    // Relative answers are relative to the probe's working directory, i.e. `manifest`;
    // `join` keeps an absolute answer as it is.
    let git_dir = manifest.join(git_dir.trim());
    let common_dir = manifest.join(common_dir.trim());
    // `HEAD` is per worktree; refs, packed refs and the shallow marker are shared. Watching
    // the whole `refs/` directory (cargo scans a directory recursively) covers the
    // checked-out branch's loose ref even when a commit creates it for a branch that was
    // only packed so far — a watch on that not-yet-existing file would be a missing path.
    let watched = [git_dir.join("HEAD"), common_dir.join("refs"), common_dir.join("packed-refs"), common_dir.join("shallow")];
    for path in watched.iter().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Runs `git` in `manifest` and returns its trimmed stdout, or `None` on any failure.
///
/// stderr is discarded so that it can never end up in parsed data, and
/// `GIT_OPTIONAL_LOCKS=0` forbids git from taking optional locks (a probe must never write
/// into `.git`, whose files this script watches — a write would re-run the script forever).
///
/// `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` are removed from the child environment:
/// inherited, any of them would point the probe at a repository other than the one being
/// built (a git hook or a wrapper script is enough to set them), which is the same wrong
/// answer the `.git` gate in `git_commit_count` exists to prevent.
fn run_git(manifest: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(manifest)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.trim().to_string())
}

/// Returns the commit count of `HEAD` in the repository at `manifest`, or the reason (English,
/// for the build warning) why none can be trusted.
///
/// The probes run with `current_dir(manifest)` and git searches UPWARD for a repository.
/// Without the `.git` gate a `.git`-less source tree unpacked inside some other repository —
/// the GitHub-ZIP case — would silently adopt the ENCLOSING repository's count: a wrong
/// version rather than a missing one, and a permanently stale one, since no `.git/*` watch was
/// emitted for a directory that is not there. `.exists()`, not `.is_dir()`: a linked worktree
/// stores `.git` as a file and is still a repository. A shallow clone is refused because its
/// history is truncated, so `rev-list --count` would report a smaller, wrong number.
fn git_commit_count(manifest: &Path) -> Result<u64, &'static str> {
    if !manifest.join(".git").exists() {
        return Err("no .git in the source tree");
    }
    match run_git(manifest, &["rev-parse", "--is-shallow-repository"]).as_deref() {
        Some("false") => {}
        Some("true") => return Err("shallow clone"),
        Some(_) | None => return Err("git is unavailable or the repository is unreadable"),
    }
    let output = run_git(manifest, &["rev-list", "--count", "HEAD"]).ok_or("git rev-list --count HEAD failed")?;
    parse_commit_count(&output).ok_or("unparsable git rev-list output")
}

/// Reads `key_path` and `password` from `.secret/build_config.json`. Either may
/// be absent; a malformed file is reported and treated as empty.
fn load_secret_config() -> (Option<String>, Option<String>) {
    let path = secret_config_path();
    let Ok(contents) = fs::read_to_string(&path) else {
        return (None, None);
    };
    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(value) => value,
        Err(err) => {
            println!("cargo:warning={SECRET_CONFIG_REL}: не удалось разобрать JSON ({err})");
            return (None, None);
        }
    };
    let key_path = value
        .get("key_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let password = value
        .get("password")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    (key_path, password)
}

/// Persists entered credentials so subsequent builds don't re-prompt.
///
/// Unix-only, matching its sole caller: the interactive prompt reads `/dev/tty` and is
/// itself `#[cfg(unix)]`, while the non-unix `prompt_for_credentials` is a stub that never
/// prompts and so never has anything to persist. Without this gate the function is dead
/// code on a Windows host and the build script warns there — a warning nobody saw while
/// `build.rs` almost never re-ran.
#[cfg(unix)]
fn save_secret_config(key_path: &str, password: &str) {
    let path = secret_config_path();
    if let Some(dir) = path.parent()
        && let Err(err) = fs::create_dir_all(dir)
    {
        println!("cargo:warning=не удалось создать каталог для {SECRET_CONFIG_REL}: {err}");
        return;
    }
    let body = serde_json::to_string_pretty(&serde_json::json!({
        "key_path": key_path,
        "password": password,
    }))
    .unwrap_or_else(|_| "{}".to_owned());
    if let Err(err) = fs::write(&path, body) {
        println!("cargo:warning=не удалось записать {SECRET_CONFIG_REL}: {err}");
    }
}

/// Resolves signing credentials, in priority order: env vars (CI) → secret
/// config file → interactive prompt. Returns `None` to build without signing.
fn resolve_credentials() -> Option<CodesignCredentials> {
    let (cfg_key, cfg_password) = load_secret_config();

    let key_path = env::var("MS_CODESIGN_P12")
        .ok()
        .filter(|s| !s.is_empty())
        .or(cfg_key);
    let password = env::var("MS_CODESIGN_PASSWORD").ok().or(cfg_password);

    if let (Some(key_path), Some(password)) = (key_path, password) {
        return Some(CodesignCredentials {
            key_path: PathBuf::from(key_path),
            password,
        });
    }

    prompt_for_credentials()
}

#[cfg(unix)]
fn prompt_for_credentials() -> Option<CodesignCredentials> {
    use std::io::{BufRead, BufReader, Write};

    // cargo перехватывает stdin/stdout build-скрипта, поэтому общаемся напрямую
    // с управляющим терминалом. Нет /dev/tty (CI, IDE) → собираем без подписи.
    let Ok(mut out) = fs::OpenOptions::new().write(true).open("/dev/tty") else {
        println!(
            "cargo:warning=подпись пропущена: нет {SECRET_CONFIG_REL} и нет терминала для ввода"
        );
        return None;
    };
    let Ok(input) = fs::File::open("/dev/tty") else {
        println!(
            "cargo:warning=подпись пропущена: нет {SECRET_CONFIG_REL} и нет терминала для ввода"
        );
        return None;
    };
    let mut reader = BufReader::new(input);

    let _ = writeln!(out, "\n=== Подпись Windows-сборки ===");
    let _ = writeln!(out, "{SECRET_CONFIG_REL} отсутствует или неполон.");
    let _ = writeln!(
        out,
        "  [1] ввести путь к ключу (.p12) и пароль (сохранится в {SECRET_CONFIG_REL})"
    );
    let _ = writeln!(out, "  [2] собрать без подписи");
    let _ = write!(out, "Выбор [1/2]: ");
    let _ = out.flush();

    let mut choice = String::new();
    if reader.read_line(&mut choice).is_err() {
        return None;
    }
    if choice.trim() != "1" {
        let _ = writeln!(out, "Собираю без подписи.\n");
        return None;
    }

    let _ = write!(out, "Путь к .p12 ключу: ");
    let _ = out.flush();
    let mut key_path = String::new();
    reader.read_line(&mut key_path).ok()?;
    let key_path = key_path.trim().to_owned();

    let _ = write!(out, "Пароль: ");
    let _ = out.flush();
    let mut password = String::new();
    reader.read_line(&mut password).ok()?;
    // Срезаем только перевод строки — пробелы могут быть частью пароля.
    let password = password.trim_end_matches(['\n', '\r']).to_owned();

    if key_path.is_empty() {
        let _ = writeln!(out, "Пустой путь к ключу — собираю без подписи.\n");
        return None;
    }

    save_secret_config(&key_path, &password);
    let _ = writeln!(
        out,
        "Сохранено в {SECRET_CONFIG_REL}. Продолжаю сборку с подписью.\n"
    );

    Some(CodesignCredentials {
        key_path: PathBuf::from(key_path),
        password,
    })
}

#[cfg(not(unix))]
fn prompt_for_credentials() -> Option<CodesignCredentials> {
    println!(
        "cargo:warning=подпись пропущена: нет {SECRET_CONFIG_REL} (создайте его с полями key_path и password)"
    );
    None
}

fn spawn_windows_signer() {
    let Some(creds) = resolve_credentials() else {
        println!("cargo:warning=windows codesign skipped: сборка без подписи");
        return;
    };
    let cert_path = creds.key_path;
    let cert_password = creds.password;

    if !cert_path.exists() {
        println!(
            "cargo:warning=windows codesign skipped: certificate not found at {}",
            cert_path.display()
        );
        return;
    }

    let target_dir = resolve_target_dir();
    let target_triple = env::var("TARGET").unwrap_or_default();
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_owned());
    let base_out_dir = target_dir.join(target_triple).join(profile);

    let exe_paths: Vec<PathBuf> = signed_bin_names()
        .into_iter()
        .map(|name| base_out_dir.join(format!("{name}.exe")))
        .collect();

    if exe_paths.is_empty() {
        println!("cargo:warning=windows codesign skipped: no candidate executables found");
        return;
    }

    let mut script = format!(
        "set -euo pipefail\nsleep 1\nfor exe in{}\ndo\n  for _ in $(seq 1 {SIGN_WAIT_SECONDS}); do\n    [ -f \"$exe\" ] && break\n    sleep 1\n  done\n  if [ ! -f \"$exe\" ]; then\n    continue\n  fi\n  out=\"$exe.signed\"\n  if osslsigncode sign -pkcs12 '{}' -pass '{}' -h sha256 -ts '{}' -in \"$exe\" -out \"$out\" >/dev/null 2>&1; then\n    mv -f \"$out\" \"$exe\"\n  else\n    rm -f \"$out\"\n  fi\ndone\n",
        shell_quote_list(&exe_paths),
        shell_quote(&cert_path),
        shell_quote_str(&cert_password),
        shell_quote_str(SIGN_TS_URL),
    );

    // Ensure there is always a trailing newline for cleaner diagnostics.
    script.push('\n');

    match Command::new("bash")
        .arg("-lc")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => {
            println!("cargo:warning=windows codesign worker started");
        }
        Err(err) => {
            println!("cargo:warning=windows codesign worker failed to start: {err}");
        }
    }
}

fn resolve_target_dir() -> PathBuf {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    match env::var("CARGO_TARGET_DIR") {
        Ok(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                manifest_dir.join(path)
            }
        }
        Err(_) => manifest_dir.join("target"),
    }
}

/// Names of the executables the codesign worker waits for: the package's single binary
/// target (`src/main.rs`, named after the package). The diagnostic programs are examples
/// of the library crates they exercise, so they are never built here and never signed.
/// Empty only when cargo did not set `CARGO_PKG_NAME` (the caller then skips signing).
fn signed_bin_names() -> Vec<String> {
    match env::var("CARGO_PKG_NAME") {
        Ok(package_name) if !package_name.is_empty() => vec![package_name],
        _ => Vec::new(),
    }
}

fn shell_quote(path: &Path) -> String {
    shell_quote_str(&path.to_string_lossy())
}

fn shell_quote_list(items: &[PathBuf]) -> String {
    let mut out = String::new();
    for item in items {
        out.push(' ');
        out.push_str(&shell_quote(item));
    }
    out
}

fn shell_quote_str(raw: &str) -> String {
    let escaped = raw.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}
