/*
File: crates/ms-browser-ext/build.rs

Purpose:
Build script of `ms-browser-ext`, the only crate that runs node/npm. Generates two files in
OUT_DIR that `src/` includes:
- `ids.rs`: `CHROMIUM_EXTENSION_ID`, derived from `browser-extension/manifest.json` `key`
  (always; needs no node). A manifest without a `key` fails the build: the id is a contract.
- `bundle.rs`: `EMBEDDED`, either the built extension as an `include_bytes!` file list or
  `NotIncluded { reason }`.

Bundle steps (any failure -> `NotIncluded` + `cargo:warning`, never a failed build):
1. `MS_SKIP_BROWSER_EXTENSION=1` skips the bundle.
2. `node` and `npm` (`npm.cmd` on a Windows host) must answer `--version`.
3. Exclusive lock on `browser-extension/node_modules.lock` (`cargo check-all` runs this script
   once per target, possibly concurrently).
4. `npm ci --no-audit --no-fund --ignore-scripts` only when `node_modules/.ms-lock-sha256`
   differs from sha256(`package-lock.json`); the stamp is written after success.
5. `node build.mjs --out OUT_DIR/ext-staging` (type check + esbuild).
6. `build_id` = `derive::build_id_of` over the staged files; `build-info.json`
   `{build_id, protocol}` is added; the reserved names are refused.

Notes:
- `rerun-if-changed` only for paths that exist, never for `node_modules` or PATH: after
  installing node, touch `browser-extension/package.json` to rebuild with the extension.
- Shares `src/derive.rs` (id and build_id rules) and `src/names.rs` with the crate via `#[path]`.
*/

#[path = "src/derive.rs"]
mod derive;
#[path = "src/names.rs"]
mod names;

use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

/// The extension's TypeScript project, relative to this crate's manifest directory.
const EXTENSION_DIR: &str = "../../browser-extension";
/// Environment switch that builds the app without the extension.
const SKIP_ENV: &str = "MS_SKIP_BROWSER_EXTENSION";
/// The bridge's pairing file (`ms_browser_bridge::CONFIG_FILE_NAME`); never part of a bundle.
/// A test of the bridge consumer checks that the two spellings agree.
const BRIDGE_CONFIG_FILE_NAME: &str = "bridge.json";
/// Marker `build.mjs` leaves in its output directory; not part of the extension.
const OUT_MARKER: &str = ".ms-ext-out";
/// Stamp of the lockfile hash `node_modules` was installed from.
const LOCK_STAMP: &str = ".ms-lock-sha256";
/// Inputs of the extension build that exist in the source tree.
const WATCHED: [&str; 7] = ["src", "manifest.json", "package.json", "package-lock.json", "tsconfig.json", "build.mjs", "icons"];
/// Lines of a failed command's output repeated as warnings.
const TAIL_LINES: usize = 20;

fn main() -> Result<(), String> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|e| format!("CARGO_MANIFEST_DIR: {e}"))?);
    let out_dir = PathBuf::from(env::var("OUT_DIR").map_err(|e| format!("OUT_DIR: {e}"))?);
    let ext_dir = manifest_dir.join(EXTENSION_DIR);

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/derive.rs");
    println!("cargo:rerun-if-changed=src/names.rs");
    println!("cargo:rerun-if-env-changed={SKIP_ENV}");
    for watched in WATCHED {
        let path = ext_dir.join(watched);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    let id = extension_id(&ext_dir)?;
    write(&out_dir.join("ids.rs"), &format!("/// Chromium extension id, derived from `browser-extension/manifest.json` `key` at build time.\npub const CHROMIUM_EXTENSION_ID: &str = {id:?};\n"))?;

    let code = match build_bundle(&ext_dir, &out_dir) {
        Ok(code) => code,
        Err(failure) => {
            println!("cargo:warning=browser extension not included: {}", failure.reason);
            for line in &failure.details {
                println!("cargo:warning=  {line}");
            }
            format!("static EMBEDDED: Result<Bundle, NotIncluded> = Err(NotIncluded {{ reason: {:?} }});\n", failure.reason)
        }
    };
    write(&out_dir.join("bundle.rs"), &code)
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Extension id from the manifest `key`; a missing manifest or key fails the build.
fn extension_id(ext_dir: &Path) -> Result<String, String> {
    let manifest_path = ext_dir.join("manifest.json");
    let text = fs::read_to_string(&manifest_path).map_err(|e| format!("reading {}: {e}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", manifest_path.display()))?;
    let key = manifest
        .get("key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{} has no \"key\": the extension id is a fixed contract", manifest_path.display()))?;
    let der = base64::engine::general_purpose::STANDARD.decode(key).map_err(|e| format!("manifest key is not base64: {e}"))?;
    Ok(derive::extension_id_from_public_key(&der))
}

/// Why the bundle is missing: a short reason (embedded in the binary) and log details.
struct Failure {
    reason: &'static str,
    details: Vec<String>,
}

impl Failure {
    fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Self { reason, details: vec![detail.into()] }
    }

    fn from_output(reason: &'static str, output: &Output) -> Self {
        let mut lines: Vec<String> = String::from_utf8_lossy(&output.stdout).lines().chain(String::from_utf8_lossy(&output.stderr).lines()).map(str::to_owned).collect();
        let skip = lines.len().saturating_sub(TAIL_LINES);
        lines.drain(..skip);
        lines.insert(0, format!("exit status: {}", output.status));
        Self { reason, details: lines }
    }
}

/// `npm` executable name on the HOST (a build script runs on the host, so `cfg!` is the host).
fn npm_program() -> &'static str {
    if cfg!(windows) { "npm.cmd" } else { "npm" }
}

fn run(program: &str, args: &[&str], cwd: &Path, reason: &'static str) -> Result<Output, Failure> {
    let output = Command::new(program).args(args).current_dir(cwd).output().map_err(|e| Failure::new(reason, format!("{program}: {e}")))?;
    if output.status.success() { Ok(output) } else { Err(Failure::from_output(reason, &output)) }
}

/// Builds the extension and returns the generated `bundle.rs` source.
fn build_bundle(ext_dir: &Path, out_dir: &Path) -> Result<String, Failure> {
    if env::var(SKIP_ENV).is_ok_and(|value| value == "1") {
        return Err(Failure::new("MS_SKIP_BROWSER_EXTENSION=1", "the app is built without the browser extension"));
    }
    run("node", &["--version"], ext_dir, "node was not found")?;
    let npm = npm_program();
    run(npm, &["--version"], ext_dir, "npm was not found")?;

    let lock_path = ext_dir.join("node_modules.lock");
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path).map_err(|e| Failure::new("could not open the npm lock file", format!("{}: {e}", lock_path.display())))?;
    lock.lock().map_err(|e| Failure::new("could not lock the npm lock file", format!("{}: {e}", lock_path.display())))?;

    ensure_node_modules(ext_dir, npm)?;

    let staging = out_dir.join("ext-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|e| Failure::new("could not clear the staging directory", format!("{}: {e}", staging.display())))?;
    }
    let staging_arg = staging.to_str().ok_or_else(|| Failure::new("OUT_DIR is not valid UTF-8", staging.display().to_string()))?;
    run("node", &["build.mjs", "--out", staging_arg], ext_dir, "the extension build (build.mjs) failed")?;
    // Everything this script reads from node_modules is done; release the lock early.
    drop(lock);

    bundle_source(ext_dir, &staging)
}

/// Runs `npm ci` when `node_modules` was not installed from the current `package-lock.json`.
fn ensure_node_modules(ext_dir: &Path, npm: &str) -> Result<(), Failure> {
    let lockfile = ext_dir.join("package-lock.json");
    let lock_bytes = fs::read(&lockfile).map_err(|e| Failure::new("package-lock.json is unreadable", format!("{}: {e}", lockfile.display())))?;
    let wanted = hex::encode(Sha256::digest(&lock_bytes));
    let stamp_path = ext_dir.join("node_modules").join(LOCK_STAMP);
    if fs::read_to_string(&stamp_path).is_ok_and(|current| current.trim() == wanted) {
        return Ok(());
    }
    run(npm, &["ci", "--no-audit", "--no-fund", "--ignore-scripts"], ext_dir, "npm ci failed")?;
    fs::write(&stamp_path, format!("{wanted}\n")).map_err(|e| Failure::new("could not write the npm lock stamp", format!("{}: {e}", stamp_path.display())))
}

/// Every file under `dir` as (`/`-separated relative path, absolute path), sorted by path.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<(), Failure> {
    let entries = fs::read_dir(dir).map_err(|e| Failure::new("could not read the staging directory", format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| Failure::new("could not read the staging directory", format!("{}: {e}", dir.display())))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| Failure::new("could not read the staging directory", format!("{}: {e}", path.display())))?;
        if file_type.is_dir() {
            collect_files(root, &path, out)?;
            continue;
        }
        let relative = path.strip_prefix(root).map_err(|e| Failure::new("staging path outside the staging directory", format!("{}: {e}", path.display())))?;
        let parts: Option<Vec<&str>> = relative.components().map(|c| c.as_os_str().to_str()).collect();
        let parts = parts.ok_or_else(|| Failure::new("a staged path is not valid UTF-8", path.display().to_string()))?;
        let relative = parts.join("/");
        if relative != OUT_MARKER {
            out.push((relative, path));
        }
    }
    Ok(())
}

/// `PROTOCOL_VERSION` as written in `src/core/protocol.ts`.
fn protocol_version(ext_dir: &Path) -> Result<u32, Failure> {
    let path = ext_dir.join("src").join("core").join("protocol.ts");
    let text = fs::read_to_string(&path).map_err(|e| Failure::new("protocol.ts is unreadable", format!("{}: {e}", path.display())))?;
    text.lines()
        .find_map(|line| line.strip_prefix("export const PROTOCOL_VERSION = "))
        .and_then(|rest| rest.trim_end().strip_suffix(';'))
        .and_then(|number| number.trim().parse().ok())
        .ok_or_else(|| Failure::new("PROTOCOL_VERSION not found in protocol.ts", path.display().to_string()))
}

/// Hashes the staged files, adds `build-info.json` and renders `bundle.rs`.
fn bundle_source(ext_dir: &Path, staging: &Path) -> Result<String, Failure> {
    let mut files = Vec::new();
    collect_files(staging, staging, &mut files)?;
    for reserved in [BRIDGE_CONFIG_FILE_NAME, names::BUILD_INFO_FILE_NAME, names::STAMP_FILE_NAME] {
        if files.iter().any(|(relative, _)| relative == reserved) {
            return Err(Failure::new("the extension build produced a reserved file", reserved));
        }
    }
    if !files.iter().any(|(relative, _)| relative == "manifest.json") {
        return Err(Failure::new("the extension build produced no manifest.json", staging.display().to_string()));
    }
    let mut contents = Vec::with_capacity(files.len());
    for (relative, path) in &files {
        let bytes = fs::read(path).map_err(|e| Failure::new("could not read a staged file", format!("{}: {e}", path.display())))?;
        contents.push((relative.as_str(), bytes));
    }
    let borrowed: Vec<(&str, &[u8])> = contents.iter().map(|(relative, bytes)| (*relative, bytes.as_slice())).collect();
    let build_id = derive::build_id_of(&borrowed);

    let info_path = staging.join(names::BUILD_INFO_FILE_NAME);
    let info = serde_json::json!({ "build_id": build_id, "protocol": protocol_version(ext_dir)? });
    fs::write(&info_path, format!("{info}\n")).map_err(|e| Failure::new("could not write build-info.json", format!("{}: {e}", info_path.display())))?;
    files.push((names::BUILD_INFO_FILE_NAME.to_owned(), info_path));
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut code = format!("static EMBEDDED: Result<Bundle, NotIncluded> = Ok(Bundle {{\n    build_id: {build_id:?},\n    files: &[\n");
    for (relative, path) in &files {
        let absolute = path.to_str().ok_or_else(|| Failure::new("a staged path is not valid UTF-8", path.display().to_string()))?;
        // `{:?}` renders a valid Rust string literal (backslashes of Windows paths escaped).
        code.push_str(&format!("        ({relative:?}, include_bytes!({absolute:?}).as_slice()),\n"));
    }
    code.push_str("    ],\n});\n");
    Ok(code)
}
