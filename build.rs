/*
FILE OVERVIEW: build.rs
Build script for platform-specific executable metadata and the compile-time version.

Main responsibilities:
- Emits `cargo:rustc-env=MS_APP_VERSION=<composed version>` — the human-facing
  application version, derived from `CARGO_PKG_VERSION` plus the git state of the
  source tree. `CARGO_PKG_VERSION` itself is NEVER overridden: it stays the
  machine-facing value used by every comparison and cross-process parse, and it is
  also what `winresource` reads from this script's own environment for the numeric
  Windows `FILEVERSION` (which has no encoding for a hash suffix anyway).
  The composition itself lives in `src/version_format.rs` and is pulled in with
  `include!` so that the exact code this script runs is covered by `cargo test`.
  Every git failure — no `git` on PATH, no repository, no tags, a shallow clone,
  a non-zero exit — degrades silently to the plain `Cargo.toml` version. That is the
  normal state of a GitHub source ZIP, so it must never produce a `cargo:warning=`.
- Emits the `cargo:rerun-if-changed` watches that keep that version fresh. This is
  NOT optional bookkeeping: this script already emitted `rerun-if-changed`, which
  REPLACES cargo's default "re-run when any package file changed", so without the
  watches below a git-derived value would go stale on the very next build. Watched:
  `.git/HEAD`, `.git/index`, `.git/packed-refs`, `.git/refs` (commits, staging,
  branch switches, tag changes) and the `src` / `crates` directories (which is what
  makes the dirty marker track reality — editing a file without staging it does not
  touch `.git/index`). When `.git` is a FILE (linked worktree or submodule checkout)
  the ref watches are skipped rather than guessed at.
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
  makes this script re-run on every source change, so EVERY Windows-target build or
  check that does not set `MS_DISABLE_BUILD_CODESIGN=1` re-spawns the detached
  `osslsigncode` worker below. `cargo check-all` is exactly such an invocation — it
  cross-checks the windows-gnu target and sets nothing — so the routine post-change
  check now leaves a signer waiting up to `SIGN_WAIT_SECONDS` (300 s) per executable
  for `.exe` files that a `cargo check` never produces. Set
  `MS_DISABLE_BUILD_CODESIGN=1` for checks to avoid it. Do NOT change the signing
  behaviour here to work around this: the release workflow is the owner's call.
- On Windows, embeds `app_icon.ico` into the PE resources so the produced `.exe`
  has the correct file icon in Explorer and shell surfaces.
- On Windows, starts a detached post-build `osslsigncode` worker that waits for
  known bin `.exe` files and signs them with a PKCS#12 certificate, unless
  `MS_DISABLE_BUILD_CODESIGN=1` disables the background signer.
- Codesign credentials (key path + password) are NEVER hardcoded. They are read
  from `.secret/build_config.json` (git/hg-ignored). When that file is missing or
  incomplete the build prompts on the controlling terminal to either enter a key
  path + password (saved back into `.secret/build_config.json`) or build unsigned.
  `MS_CODESIGN_P12` / `MS_CODESIGN_PASSWORD` env vars override the file, for CI.
- On non-Windows targets, performs no-op to keep builds fast and portable.
*/

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// The version composition is shared verbatim with the crate (`mod version_format;` in
// `src/main.rs`). `include!` rather than a copy, so that the code this script runs is the
// code `cargo test` covers; the file's `#[cfg(test)] mod tests` is inert here.
include!("src/version_format.rs");

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
        if let Err(err) = resource.compile() {
            panic!("failed to embed Windows executable icon: {err}");
        }
        if env::var_os(DISABLE_BUILD_CODESIGN_ENV).as_deref() != Some("1".as_ref()) {
            spawn_windows_signer();
        }
    }
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
/// that keep it fresh. See this file's header for the rerun policy and its accepted
/// side effect on the Windows codesign worker.
///
/// Never fails: any git problem degrades to the plain `CARGO_PKG_VERSION`, silently and
/// without a `cargo:warning=` (a source ZIP has no repository, and that is normal).
fn emit_app_version() {
    let manifest = manifest_dir();
    emit_version_rerun_watches(&manifest);

    let base_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    // The probes run with `current_dir(manifest)` and git searches UPWARD for a
    // repository. Without this gate a `.git`-less source tree unpacked inside some other
    // repository — the GitHub-ZIP case this feature exists to degrade gracefully on —
    // would silently adopt the ENCLOSING repository's tag, distance and hash. That is a
    // wrong version rather than a missing one, and a permanently stale one, since no
    // `.git/*` watch was emitted for a directory that is not there. `.exists()`, not
    // `.is_dir()`: a linked worktree stores `.git` as a file and is still a repository.
    let git_info = if manifest.join(".git").exists() { git_describe_info(&manifest) } else { None };
    // The dirty flag is meaningless without a commit to attach it to, so the probe is
    // skipped entirely when `git describe` produced nothing (see `compose_app_version`).
    let dirty = git_info.is_some() && git_source_tree_is_dirty(&manifest);
    let version = compose_app_version(
        &base_version,
        git_info.as_ref().map(|(hash, distance)| (hash.as_str(), *distance)),
        dirty,
    );
    println!("cargo:rustc-env=MS_APP_VERSION={version}");
}

/// Emits one `cargo:rerun-if-changed` per EXISTING path that can change the composed
/// version. Missing paths are skipped on purpose: cargo treats a watch on a missing path
/// as "always changed" and would re-run this script on every build, which is precisely
/// the wrong outcome in a `.git`-less source ZIP.
fn emit_version_rerun_watches(manifest: &Path) {
    // Source directories: cargo walks a watched directory recursively. These are what the
    // dirty marker depends on — an unstaged edit never touches anything under `.git/`.
    for source_dir in ["src", "crates"] {
        if manifest.join(source_dir).is_dir() {
            println!("cargo:rerun-if-changed={source_dir}");
        }
    }

    // A linked worktree or a submodule checkout stores `.git` as a FILE pointing at the
    // real git directory. Resolving that indirection is out of scope; skip the ref
    // watches rather than watch paths that do not exist.
    let git_dir = manifest.join(".git");
    if !git_dir.is_dir() {
        return;
    }
    // `HEAD` catches branch switches and detached-HEAD moves, `index` catches staging,
    // `packed-refs` and `refs/` catch commits and tag creation/packing.
    for git_rel in [".git/HEAD", ".git/index", ".git/packed-refs", ".git/refs"] {
        if manifest.join(git_rel).exists() {
            println!("cargo:rerun-if-changed={git_rel}");
        }
    }
}

/// Runs `git` in `manifest` and returns its trimmed stdout, or `None` on any failure.
///
/// stderr is discarded so that it can never end up in parsed data, and
/// `GIT_OPTIONAL_LOCKS=0` forbids git from taking the index lock: without it a plain
/// `git status` may rewrite `.git/index` while refreshing it, and `.git/index` is one of
/// the paths this script watches — the build would then re-run the script forever.
///
/// `GIT_DIR`, `GIT_WORK_TREE` and `GIT_INDEX_FILE` are removed from the child environment:
/// inherited, any of them would point the probe at a repository other than the one being
/// built (a git hook or a wrapper script is enough to set them), which is the same wrong
/// answer the `.git` gate in `emit_app_version` exists to prevent.
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

/// Returns `(short_hash, commits_since_tag)` from `git describe --tags --long`, or `None`
/// when there is no git, no repository, no reachable tag, or the output does not parse.
///
/// `git describe --long` prints `<tag>-<distance>-g<hash>`, and a tag name may itself
/// contain `-`, so the two trailing fields are split off from the RIGHT. Anything that
/// does not parse into a numeric distance and a hex hash is rejected instead of guessed.
fn git_describe_info(manifest: &Path) -> Option<(String, u32)> {
    let described = run_git(manifest, &["describe", "--tags", "--long", "--abbrev=7"])?;
    let (head, hash_field) = described.rsplit_once('-')?;
    let (_tag, distance_field) = head.rsplit_once('-')?;

    let short_hash = hash_field.strip_prefix('g')?;
    if short_hash.is_empty() || !short_hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let distance = distance_field.parse::<u32>().ok()?;
    Some((short_hash.to_string(), distance))
}

/// Reports whether `src/` or `crates/` carry uncommitted changes (modified, staged or
/// untracked). The scope is those two directory trees ENTIRELY, documentation included:
/// `src/MODULE_README.md` and the per-module readmes live inside `src/`, so editing one
/// marks the build dirty. Everything outside the two trees — `wiki/`, `dev-docs/`,
/// `user_config.json`, logs, `README_AGENT.md` — does not. Any git failure reads as
/// "clean".
fn git_source_tree_is_dirty(manifest: &Path) -> bool {
    run_git(manifest, &["status", "--porcelain", "--", "src", "crates"])
        .is_some_and(|status| !status.is_empty())
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

    let exe_paths: Vec<PathBuf> = discover_bin_names()
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

fn discover_bin_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let package_name = env::var("CARGO_PKG_NAME").unwrap_or_default();
    if !package_name.is_empty() {
        names.insert(package_name);
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    let bin_dir = manifest_dir.join("src").join("bin");
    if let Ok(entries) = fs::read_dir(bin_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                names.insert(stem.to_owned());
            }
        }
    }

    names
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
