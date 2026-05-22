/*
FILE OVERVIEW: build.rs
Build script for platform-specific executable metadata.

Main responsibilities:
- On Windows, embeds `app_icon.ico` into the PE resources so the produced `.exe`
  has the correct file icon in Explorer and shell surfaces.
- On Windows, starts a detached post-build `osslsigncode` worker that waits for
  known bin `.exe` files and signs them with a PKCS#12 certificate, unless
  `MS_DISABLE_BUILD_CODESIGN=1` disables the background signer.
- On non-Windows targets, performs no-op to keep builds fast and portable.
*/

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_CODESIGN_P12: &str = "/home/vasyanator/codesign.p12";
const DEFAULT_CODESIGN_PASSWORD: &str = "reptiloid277";
const SIGN_WAIT_SECONDS: u64 = 300;
const SIGN_TS_URL: &str = "http://timestamp.sectigo.com";
const DISABLE_BUILD_CODESIGN_ENV: &str = "MS_DISABLE_BUILD_CODESIGN";

fn main() {
    println!("cargo:rerun-if-changed=app_icon.ico");
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

fn spawn_windows_signer() {
    let cert_path = env::var("MS_CODESIGN_P12")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CODESIGN_P12));
    let cert_password =
        env::var("MS_CODESIGN_PASSWORD").unwrap_or_else(|_| DEFAULT_CODESIGN_PASSWORD.to_owned());

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
