# Module: tools/run-dev

## Purpose

The `run-dev` entry point: take a machine that may have nothing installed, bring the working copy
up to date with `origin`, provision a Rust toolchain that satisfies the crate's MSRV, and start the
application binary from source.

This is a **developer/source-user** tool, not the shipped installer. Python, AI models, and the
ONNX runtime are provisioned by `src/installer/`; nothing here does that work. Stage 3 does *start*
the binary with `--check-venv` so the application can provision its own environment before the real
launch, but the script only reads that process's exit code — its responsibility still ends the
moment the Rust binary starts.

The algorithm, every branch of it, and the rationale for each decision are specified in
`dev-docs/run_dev_plan.md`. That document is the contract; these scripts implement it.

## Architecture

Three stages, in order, identical on all platforms:

```
Stage 1 (git)   locate git -> fingerprint the run-dev files -> adopt a non-repo ZIP copy
                -> fetch -> merge
   (in main)    re-fingerprint: stop with exit 8 if run-dev itself changed. Deliberately at
                the call site, not inside the stage: Stage 1 has many early returns
Stage 2 (rust)  read MSRV from Cargo.toml -> pick/provision toolchain -> check C compiler
Stage 3 (run)   phase 1: cargo run ... -- --check-venv --ignore-installed   (builds; GUI only
                         when the environment is incomplete; non-zero -> exit 7)
                phase 2: cargo run ... -- --ignore-installed [user args]
```

Two implementations, not three: Linux and macOS share `run-dev.sh` and differ only in three leaf
decisions (git-missing message, C-compiler probe, install hints), each behind one `case "$MS_OS"`.
Windows needs a genuinely different implementation (`run-dev.ps1`) because it provisions Git and a
C toolchain that the POSIX platforms get from a package manager.

The three launchers in the project root (`run-dev.Linux.sh`, `run-dev.MacOS.command`,
`run-dev.Windows.bat`) contain **no logic** — they only resolve their own directory and hand off.
Do not add behavior to them. Their structure (`exec` as the last statement in the shell launchers,
one `( … )` block in the `.bat`) is not logic but protection against being rewritten mid-run; see
the invariants below.

Everything provisioned lands in `installer_files/`, which is `.gitignore`d, so it can never be
committed and never shows up as a local change during Stage 1:

| Path | Contents |
|---|---|
| `installer_files/git/` | portable MinGit (Windows only) |
| `installer_files/mingw64/` | portable MinGW-w64 / GCC (Windows only) |
| `installer_files/rust/rustup/` | `RUSTUP_HOME` |
| `installer_files/rust/cargo/` | `CARGO_HOME` |
| `installer_files/downloads/` | download scratch, shared with `src/installer/utils.rs` |

## Files

- `run-dev.sh`: POSIX core (Linux + macOS). Written for **bash 3.2**, the version macOS still
  ships — no associative arrays, no `mapfile`, no `${var^^}`. Edit here for anything affecting
  Linux or macOS.
- `run-dev.ps1`: Windows core. Must stay **UTF-8 with BOM**: Windows PowerShell 5.1 reads a
  BOM-less script as the system ANSI code page and mangles every Russian message. Edit here for
  anything Windows-specific, including MinGit and MinGW-w64 provisioning.
- `test_run_dev.sh`: contract tests for the git stage, the version helpers, and the self-update
  fingerprints. Sources `run-dev.sh` with `MS_RUN_DEV_SOURCE_ONLY=1` and drives its functions
  against throwaway repositories in a temp dir. No network, no cargo, no contact with the user's
  repository. Run: `bash tools/run-dev/test_run_dev.sh`.

## Contracts and invariants

- **Untracked files are never touched.** No path stashes, moves, or deletes them, and `git clean`
  appears nowhere. The working directory holds the user's projects, downloaded models, logs and
  `user_config.json` — all untracked or ignored.
- **Every failure leaves the tree exactly as it was found.** Stage 1 takes a `PRE_HEAD` rollback
  point before it modifies anything; both failure positions (merge, `stash pop`) restore through
  `git reset --hard <PRE_HEAD>` followed by `git stash pop`. Exit code 3 means "restored, needs
  manual merge", never "half-updated".
- **"Убрать локальные изменения" means stash, never `reset --hard`.** The changes must stay
  recoverable via `git stash list` for a user who mis-clicked.
- **The MSRV lives in `Cargo.toml` (`rust-version`), nowhere else.** Both scripts parse it and
  **fail** when it is absent — no fallback constant, which would silently drift and turn into a
  confusing mid-build type error.
- **Nothing outside `installer_files/` is written.** No shell profile, no `PATH`, no registry, no
  system packages. `rustup-init` always runs with `--no-modify-path`; `PATH` changes are
  process-local.
- **`MS_DISABLE_BUILD_CODESIGN=1` is set before cargo** unless the caller already exported it.
  `build.rs` otherwise starts a codesign worker for Windows targets that *prompts on the terminal*
  for a `.p12` password when `.secret/build_config.json` is missing — in a double-clicked window
  that reads as a hang.
- **Being offline never blocks running the app.** A failed `git fetch` is a warning; Stage 2 and 3
  proceed.
- **No invented download URLs.** Asset URLs are resolved from the GitHub releases API at run time.
  When the API is unreachable the scripts fail with the manual download page, rather than falling
  back to a guessed or pinned-stale URL.
- **The C toolchain is probed before cargo runs.** `aws-lc-sys` (`translators`/`genai` → `reqwest`
  → `rustls` → `aws-lc-rs`) compiles C and assembly on every native target, so it is a real
  prerequisite; probing converts a wall of linker errors 200 crates deep into one clear message.
- **An update that rewrites run-dev stops the run (exit 8), it does not continue.** The executed
  files are fingerprinted with `git hash-object --no-filters` before Stage 1 touches the tree and
  again after it; any difference means the running process is stale. Nothing is rolled back — the
  update is applied and correct, only a restart is needed. The fingerprinted set is the five
  executed files, not the whole module: changing `test_run_dev.sh` or this document cannot affect a
  run in flight.
- **`--no-filters` on `git hash-object` is not optional.** With the filter applied, `core.autocrlf`
  (true by default on Git for Windows) makes a CRLF and an LF copy of the same file hash
  identically, so a line-ending-only rewrite of `run-dev.Windows.bat` — the one file where that is
  fatal — would be invisible. A `?` fingerprint (hash could not be computed) means "unknown" on
  either side of the comparison and never counts as a change, so a transient git failure cannot
  fabricate a restart request.
- **The scripts must survive being rewritten while they execute.** Structural, not incidental:
  every statement in `run-dev.sh` lives in a function and the file ends with `main "$@"; exit $?`;
  the root shell launchers end with `exec`; `run-dev.Windows.bat` wraps its whole tail in one
  `( … )` block (with `enabledelayedexpansion` + `!RC!`, because `%VAR%` inside a block is
  substituted at parse time — at the cost of `!` in paths and forwarded arguments) and keeps
  **CRLF** line endings; `run-dev.ps1` relies on PowerShell parsing the file in full before
  execution. See `dev-docs/run_dev_plan.md`, "Updating files that are being executed", before
  restructuring any of them.
- **The line endings are enforced by the root `.gitattributes`, not by convention.**
  `run-dev.Windows.bat` is `text eol=crlf`; the POSIX launchers and `tools/run-dev/*.sh` are
  `text eol=lf` (a CRLF shebang is not executable). `git archive` honours `eol`, so a GitHub ZIP
  gets the same treatment as a clone. `.gitattributes` is allowlisted in the root `.gitignore` —
  that file is a publication allowlist, so a new dotfile without an explicit `!` rule would never
  be published.
- **Stage 3's two phases are strictly sequential.** Phase 1 (`--check-venv`) has fully exited
  before phase 2 starts. On Windows a running `.exe` cannot be relinked, so overlapping them would
  break the rebuild. Both are plain `cargo run`; there is no separate `cargo build` and no guessed
  `target/…` path.
- **Phase 1 is skipped under `--offline`/`-Offline`.** The flag means "no network at all", and the
  check may download uv, Python or Torch wheels. The application reports a broken environment on
  its own later.

## Editing map

- To change the update/merge algorithm, edit `update_with_local_changes` (sh) /
  `Update-WithLocalChanges` (ps1) — and keep the two identical, `test_run_dev.sh` only covers the sh
  side.
- To change how a ZIP copy is adopted, see `adopt_repository` / `Invoke-RepositoryAdoption`.
- To change toolchain selection or provisioning, see `rust_stage` / `Invoke-RustStage`.
- To change which files force a restart after an update, edit `SELF_FILES` / `$SelfFiles` (keep the
  two lists in the same order) and the detection in `capture_self_fingerprints` /
  `Get-SelfFingerprints`. A file added there whose line endings matter needs a rule in the root
  `.gitattributes` as well.
- To change what is passed to the application, see `run_stage` + `cargo_run_app` (sh) /
  `Invoke-RunStage` + `Invoke-CargoRun` (ps1). The environment check is `check_environment` /
  `Assert-AppEnvironment`; the flags it relies on (`--check-venv`, `--ignore-installed`) are
  defined in `src/args.rs`.
- To change the Windows host triple or why GNU is used, see `dev-docs/run_dev_plan.md` §2.4 first.
- To raise the required Rust version, edit `rust-version` in the root `Cargo.toml`. Do not touch
  the scripts.
- To retarget a fork, set `MS_RUN_DEV_ORIGIN` / `MS_RUN_DEV_BRANCH` rather than editing constants.

## Testing status

`test_run_dev.sh` covers the git stage — the part that can destroy a user's work — including the
restore-after-conflict path, and the self-update fingerprints down to `check_self_update` returning
exit 8. It does **not** cover Stage 2 or 3, and does not cover `run-dev.ps1` at all: provisioning
asserts against real downloads, running Stage 3 means a full cargo build plus a GUI, and there is
no PowerShell test harness in this repository. Those paths are verified by hand.

Before trusting a change, on each platform: fresh ZIP without git, clean repo behind origin, dirty
repo with non-overlapping edits, dirty repo with overlapping edits that merge, dirty repo with
edits that genuinely conflict (verify the tree is restored), `--offline` with no network, an update
whose commits touch `tools/run-dev/*` (expect exit 8, no build, and a correct restart command), a
run with a complete venv (expect no installer window between the build and the app), and a run with
the venv removed (expect the installer, then the application).
