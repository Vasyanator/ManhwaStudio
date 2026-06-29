/*
FILE OVERVIEW: build.rs
Build script for platform-specific executable metadata.

Main responsibilities:
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
    println!("cargo:rerun-if-changed=app_icon.ico");
    println!("cargo:rerun-if-changed={SECRET_CONFIG_REL}");
    println!("cargo:rerun-if-env-changed=MS_CODESIGN_P12");
    println!("cargo:rerun-if-env-changed=MS_CODESIGN_PASSWORD");
    println!("cargo:rerun-if-env-changed={DISABLE_BUILD_CODESIGN_ENV}");

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
fn save_secret_config(key_path: &str, password: &str) {
    let path = secret_config_path();
    if let Some(dir) = path.parent() {
        if let Err(err) = fs::create_dir_all(dir) {
            println!("cargo:warning=не удалось создать каталог для {SECRET_CONFIG_REL}: {err}");
            return;
        }
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
