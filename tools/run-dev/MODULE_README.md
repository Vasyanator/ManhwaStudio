# Module: tools/run-dev

## Purpose

The `run-dev` entry point: take a machine that may have nothing installed, bring the working copy
up to date with `origin`, provision a Rust toolchain that satisfies the crate's MSRV, build the
application, publish the built binary into the project root, and start it from there.

This is a **developer/source-user** tool, not the shipped installer. Python, AI models, and the
ONNX runtime are provisioned by `src/installer/`; nothing here does that work. Stage 3 does *start*
the binary with `--check-venv` so the application can provision its own environment before the real
launch, but the script only reads that process's exit code — its responsibility still ends the
moment the Rust binary starts.

Publishing the binary into the project root is what makes the script optional afterwards: once a
build has succeeded, `manhwastudio_rs` (`.exe` on Windows) in the root is a complete launch, with
no git update and no cargo involved. That is the point of the step, and it is why a failure to
publish is only a warning.

The algorithm, every branch of it, and the rationale for each decision are specified in
`dev-docs/run_dev_plan.md`. That document is the contract; these scripts implement it.

## Architecture

Three stages, in order, identical on all platforms:

```
Stage 1 (git)   locate git -> remember HEAD -> adopt a non-repo ZIP copy -> fetch -> merge
   (in main)    HEAD moved? ask git which run-dev paths the range touched; exit 8 if any.
                Deliberately at the call site, not inside the stage: Stage 1 has many
                early returns
Stage 2 (rust)  read MSRV from Cargo.toml -> pick/provision toolchain -> check C compiler
Stage 3 (run)   phase 1: cargo run ... -- --check-venv --ignore-installed   (builds; GUI only
                         when the environment is incomplete; non-zero -> exit 7)
                publish: cargo build --message-format=json -> read the `executable` path ->
                         copy it into the project root, atomically, skipping an identical
                         copy. Any failure here is a warning, not an exit
                phase 2: <project root>/manhwastudio_rs -- --ignore-installed [user args]
                         (falls back to `cargo run …` when publishing failed)
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
  detection. Sources `run-dev.sh` with `MS_RUN_DEV_SOURCE_ONLY=1` and drives its functions
  against throwaway repositories in a temp dir. No network, no cargo, no contact with the user's
  repository. Run: `bash tools/run-dev/test_run_dev.sh`.
- `test_run_dev.ps1`: the same contract, asserted against `run-dev.ps1` so the two implementations
  cannot silently diverge. Dot-sources with `MS_RUN_DEV_SOURCE_ONLY=1`; needs pwsh + git and nothing
  else. Run: `pwsh -NoProfile -File tools/run-dev/test_run_dev.ps1`. A behavioural change to one
  implementation belongs in both test files.

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
- **Nothing outside `installer_files/` is written — with exactly one deliberate exception.** No
  shell profile, no `PATH`, no registry, no system packages. `rustup-init` always runs with
  `--no-modify-path`; `PATH` changes are process-local. The exception is the published binary
  `<project root>/manhwastudio_rs[.exe]`, which is the whole point of the publishing step; both
  names are already ignored by the root `.gitignore` (line 4 is `*` — the file is a publication
  allowlist), so it can never be committed and never appears as a local change during Stage 1.
- **`MS_DISABLE_BUILD_CODESIGN=1` is set before cargo** unless the caller already exported it.
  `build.rs` otherwise starts a codesign worker for Windows targets that *prompts on the terminal*
  for a `.p12` password when `.secret/build_config.json` is missing — in a double-clicked window
  that reads as a hang.
- **Being offline never blocks running the app.** A failed `git fetch` is a warning; Stage 2 and 3
  proceed.
- **No invented download URLs.** Asset URLs are resolved from the GitHub releases API at run time.
  When the API is unreachable the scripts fail with the manual download page, rather than falling
  back to a guessed or pinned-stale URL.
- **Every download is resumable, verified and atomic.** Bytes go to `<dest>.part` and are renamed
  into place only when complete, so the destination never holds a truncated file; an interrupted run
  keeps the `.part` and the next run continues from it (and the error message says so); the asset
  size from the GitHub API is checked against the finished file; a `<asset>.sha256` sidecar, when
  published, is verified and a mismatch deletes the `.part`. A missing sidecar skips the check — it
  is a bonus, never a prerequisite. Same contract as `src/installer/utils.rs::download_asset`; do not
  add a second one.
- **The fallback downloader is not a stub.** `curl.exe` is tried first (it resumes and retries by
  itself), but `Get-RemoteFile` falls through to the .NET implementation on **any** curl failure, not
  only when curl is absent: on the machine this was debugged on curl gets an empty reply from the
  GitHub CDN (exit 52) while `HttpWebRequest` downloads the same URL fine. The .NET path therefore
  implements `Range` resume and progress reporting for real. Look up `curl.exe` with
  `-CommandType Application`: in Windows PowerShell, `curl` is an alias for `Invoke-WebRequest`.
- **GCC must be reached through a path with no space in it.** It builds the paths of its own
  internals from where `gcc.exe` lives and hands them to `ld.exe` in unquoted spec strings, so
  `C:\Program Files\ManhwaStudio\installer_files\mingw64` reaches the linker split in two and
  every link fails (`ld.exe: cannot find C:/Program`). That path is the *installer's* location, not
  a mistake, and the paths rustc passes are quoted and fine — so the fix is never "move the
  project". `Resolve-SpaceFreePath` tries the path as given, its 8.3 short form, then a `subst`
  drive; `Set-MingwEnvironment` puts the winner on `PATH` and additionally pins the linker, `CC` and
  `AR` by absolute path, because rustc calls the linker by bare name and gcc would otherwise recover
  its own location from the PATH lookup. A drive created here is released by `Invoke-Main`; one that
  already existed is reused and left alone. When nothing works the run stops with exit code 5
  **before** the download — `Assert-SpaceFreeToolchainPath`, called at the top of `Install-Mingw`.
  Rationale and the rejected alternatives are in `dev-docs/run_dev_plan.md` §2.5a.
- **The C toolchain is probed before cargo runs.** `aws-lc-sys` (`translators`/`genai` → `reqwest`
  → `rustls` → `aws-lc-rs`) compiles C and assembly on every native target, so it is a real
  prerequisite; probing converts a wall of linker errors 200 crates deep into one clear message.
- **An update that changes run-dev stops the run (exit 8), it does not continue.** The question is
  put to git, not to the filesystem: HEAD is remembered before Stage 1, and afterwards
  `git diff --name-only <pre-HEAD> HEAD -- <self paths>` names what the update touched. HEAD
  unchanged means nothing was updated and nothing is checked. Nothing is rolled back — the update is
  applied and correct, only a restart is needed. The queried set is the five executed files, not the
  whole module: changing `test_run_dev.sh` or this document cannot affect a run in flight.
- **Never compare the files' bytes to answer that question.** The earlier `git hash-object`
  implementation made the answer depend on line endings, `core.autocrlf`, clean/smudge filters,
  `.gitattributes`, and the rewrites `git stash push` performs through the `eol` attribute — each of
  which reported a change for an update that changed nothing. `SELF_PATHS` / `$SelfGitPaths` are
  **git pathspecs**: repository-relative, forward slashes, in both implementations. Windows
  separators would match nothing at all.
- **The adoption branch answers from what it did, not from inspection.** A ZIP copy has no "before"
  commit. Taking the repository's version replaces every tracked file, so that path sets
  `ADOPTED_REPLACED` / `$script:AdoptedReplaced` and a restart is requested; keeping the files, or a
  ZIP that already matched `origin`, replaces nothing and asks for nothing.
- **git's stderr never reaches captured output, and every captured value is validated by shape.**
  git writes advisory text to stderr on *successful* commands; merged into the output it becomes a
  bogus `rev-list --count` or a bogus dirty tree. `run-dev.sh` redirects stderr to `/dev/null`;
  `Invoke-Git` in `run-dev.ps1` returns `Output` (stdout) and `Error` (stderr) separately, split by
  object type so ordering cannot matter. On top of that, an object id is accepted only if it has the
  right shape (`is_object_id` / `Test-ObjectId`) and a count only if it is a bare number
  (`to_count` / `Convert-ToCount`); anything else degrades to empty or `0` instead of becoming
  data.
- **A function that returns a collection returns a plain array, and every caller wraps the call in
  `@(...)`.** Never `return ,$x`: the unary comma hands the *inner* array over as one object, so
  `@(call).Count` is 1 even for an empty list. That single line is what made the restart banner fire
  on every launch and print `System.Object[]` as the changed file. Verified on PowerShell 5.1 and
  locked down by `test_run_dev.ps1` ("Возврат массива из функции"), which asserts 0/1/many and that
  the forbidden form breaks the empty case.
- **PowerShell string comparison is case-insensitive; object ids are not.** Use `-cmatch` / `-ceq`
  when validating or comparing git object ids, so the two implementations accept exactly the same
  values (`test_run_dev.ps1` caught this on the real machine: `-match '^[0-9a-f]{40}$'` accepts
  upper-case hex, the POSIX `case` glob does not).
- **A stash entry is restored by identity, never by position.** `push` exits 0 having saved nothing,
  and the stash is shared with every other git client — an IDE can push or drop an entry between our
  push and our pop. Both implementations record the commit id their push created and pop exactly
  that `stash@{N}` (`stash_ref_for`/`pop_stash_entry`, `Get-StashRefFor`/`Invoke-StashPop`) on every
  path. If the entry is gone, nothing is popped: the run stops and tells the user where their
  changes are. The messages name the concrete `stash@{N}` too — a bare `git stash pop` is wrong
  advice as soon as anything else touches the stash.
- **The scripts must survive being rewritten while they execute.** Structural, not incidental: in
  `run-dev.sh` the single call `main "$@"` is the **last** statement, after every definition, and is
  followed by `exit $?` — bash parses a function body only when it reaches it, so the placement, not
  the use of functions, is what guarantees the file has been read to EOF before any work starts;
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
- **Stage 3's phases are strictly sequential.** Phase 1 (`--check-venv`) has fully exited before
  the publishing step runs, and that step has finished before phase 2 starts. On Windows a running
  `.exe` can neither be relinked nor overwritten, so overlapping any two of them would break either
  the rebuild or the copy into the root.
- **The binary's path is asked of cargo, never guessed.** The publishing step runs
  `cargo build --bin <APP_BIN> [--release] --message-format=json-render-diagnostics` and reads the
  `executable` field of the last `compiler-artifact` message whose `target.name` matches. The
  profile directory, a `--target` triple and `CARGO_TARGET_DIR` all move the output, so a hand-built
  `target/release/…` path would be wrong on somebody's machine — and `--message-format=json` alone
  would swallow the compiler errors, hence the `-render-diagnostics` variant. Filtering on the
  target name is required: the package has several bin targets, `src/bin/` autodiscovery included.
  Normally the call is cache-warm and free, because phase 1 already built; under
  `--offline`/`-Offline`, where phase 1 is skipped, it is the call that really builds.
- **Publishing the root binary is a convenience and never fatal.** Every failure of it — cargo not
  naming a path, a locked destination, a full disk — is a `warn`, and phase 2 falls back to
  `cargo run` exactly as it worked before the step existed. It has no exit code of its own; adding
  one would turn a lost convenience into a refusal to run the application.
- **The root copy is written the same way a download is: `.part`, then rename.** The bytes go to
  `<dest>.part` and are renamed into place only when complete, so the destination never holds a
  truncated executable — and the user may be running the previous root binary at that very moment.
  On POSIX renaming over a running executable is safe while writing into it is not; on Windows the
  rename is refused outright, so `Install-RootBinary` renames the locked file aside to a unique
  `<dest>.old-<8 hex>` and retries, restoring it if the retry fails. The name is unique on purpose:
  a fixed one left behind and still locked by that same process would block every future publish
  and blame the wrong file. `Clear-RootBinaryLeftovers` sweeps stale `.part`/`.old-*` files on
  every run, before the copy decision, so the "already identical, skip the copy" path cleans up
  too. `cp -p` / `Copy-Item` carry the modification time over, which is what makes that
  identical-skip test work.
- **On Windows phase 2 must be a PIPELINE, and that is not cosmetic.** `src/main.rs` applies
  `windows_subsystem = "windows"` unless the `active-logs` feature is on, and `run-dev` passes no
  `--features`, so the application is a GUI-subsystem binary — and PowerShell's call operator does
  not wait for one. `Invoke-RootBinary` therefore pipes it (`& $exe @args | Out-Host`): a pipeline
  makes PowerShell read the child's stdout to EOF, which cannot happen before it exits. Without
  that, the script would return instantly, report `cargo build`'s stale exit code, and close the
  window while the application was still starting. `cargo run` never had the problem because
  `cargo.exe` is a console application. `Start-Process -Wait` would also wait, but Windows
  PowerShell 5.1 joins its `-ArgumentList` without quoting and would split every argument
  containing a space.
- **Phase 1 is skipped under `--offline`/`-Offline`.** The flag means "no network at all", and the
  check may download uv, Python or Torch wheels. The application reports a broken environment on
  its own later.

## Editing map

- To change the update/merge algorithm, edit `update_with_local_changes` (sh) /
  `Update-WithLocalChanges` (ps1) — and keep the two identical, extending both `test_run_dev.sh` and
  `test_run_dev.ps1`.
- To run a git command from `run-dev.ps1`, go through `Invoke-Git`/`Get-GitOut`/`Test-GitOk`. Never
  call `& $script:Git … 2>&1` directly: that is how stderr gets into parsed data.
- To change how a ZIP copy is adopted, see `adopt_repository` / `Invoke-RepositoryAdoption`.
- To change toolchain selection or provisioning, see `rust_stage` / `Invoke-RustStage`.
- To change which files force a restart after an update, edit `SELF_PATHS` / `$SelfGitPaths` (git
  notation, same order in both) — the detection itself, `self_changed_paths` /
  `Get-ChangedSelfPaths`, needs no changes. A file added there whose line endings matter needs a rule
  in the root `.gitattributes` as well.
- To change what is passed to the application, see `run_stage` + `cargo_run_app` + `run_root_binary`
  (sh) / `Invoke-RunStage` + `Invoke-CargoRun` + `Invoke-RootBinary` (ps1) — the arguments are
  composed once in the stage function and used by both launch paths. The environment check is
  `check_environment` / `Assert-AppEnvironment`; the flags it relies on (`--check-venv`,
  `--ignore-installed`) are defined in `src/args.rs`. `--ignore-installed` is load-bearing on the
  root-binary path specifically: without it the application's self-updater is allowed to overwrite
  the very file it is running from.
- To change how the built binary reaches the project root, see `resolve_built_binary`,
  `root_binary_needs_copy`, `install_root_binary`, `publish_root_binary` (sh) /
  `Resolve-BuiltBinary`, `Test-RootBinaryNeedsCopy`, `Install-RootBinary`, `Publish-RootBinary`
  (ps1). The two parsers of cargo's JSON (`built_binary_from_json` / `Get-BuiltBinaryFromJson`) and
  the two copy predicates are pure functions on purpose, so both test suites can drive them under
  `MS_RUN_DEV_SOURCE_ONLY=1` without cargo.
- To change anything about downloading — retries, resume, verification, progress — edit
  `Get-RemoteFile` and its two primitives (`Invoke-CurlDownload`, `Invoke-DotNetDownload`) in
  `run-dev.ps1` and `download` in `run-dev.sh`; the contract itself is `dev-docs/run_dev_plan.md`
  §2.3a. `Resolve-GithubAsset` is what supplies the expected size and the checksum sidecar.
- To change the Windows host triple or why GNU is used, see `dev-docs/run_dev_plan.md` §2.4 first.
- To change how the C toolchain is located or handed to cargo, see `Set-MingwEnvironment` and
  `Resolve-SpaceFreePath` (plus `Get-ShortPathName`, `Get-SubstMap`, `Get-FreeDriveLetters`,
  `Get-ExistingSubstDrive`, `New-SubstDrive`, `Remove-SubstDrive`). Windows-only: `run-dev.sh` has
  no counterpart, because Linux and macOS take their compiler from a package manager.
- To raise the required Rust version, edit `rust-version` in the root `Cargo.toml`. Do not touch
  the scripts.
- To retarget a fork, set `MS_RUN_DEV_ORIGIN` / `MS_RUN_DEV_BRANCH` rather than editing constants.

## Testing status

`test_run_dev.sh` covers the git stage — the part that can destroy a user's work — including the
restore-after-conflict path, the stash guard, the value parsers, and the self-update detection down
to `check_self_update` returning exit 8. `test_run_dev.ps1` asserts the same contract against
`run-dev.ps1`, plus the stream separation in `Invoke-Git` and the array-return convention. It needs
Windows PowerShell or pwsh, which is not available in every development environment here, so a
change that only runs the sh suite is a change whose Windows half is unverified — say so rather than
implying both were run. Two bugs reached users because that half went unrun, so run it on a real
machine when one is reachable. Both suites also cover the pure halves of the publishing step — the
cargo-JSON parser and the "does the root copy need refreshing?" predicate — plus one real copy into
a temp directory asserting the atomic rename, the preserved modification time and that no `.part`
is left behind.

`test_run_dev.ps1` additionally covers the space-free toolchain path: the `subst` output parser, a
path without a space being left alone, and `Set-MingwEnvironment` pinning `PATH`/linker/`CC`/`AR`
without duplicating the `PATH` entry. Only the Windows branch of that suite exercises the
interesting half — an actual path *with* a space resolving to one without, and the created drive
being released — and it has no `test_run_dev.sh` counterpart because the toolchain provisioning it
guards is Windows-only.

Both also cover the launch of the published binary against a stub program: the arguments reach it
intact (one of them containing a space), its exit code is propagated, and an unlaunchable file
yields 126 rather than a stack trace.

What stays uncovered: the cargo invocation itself, because it means a real build; on Windows, the
locked-`.exe` rename-aside path, which needs a running executable to exercise; and — the important
one — the GUI-subsystem wait described in the invariants, because the stub is a console program and
the suite would pass even with that bug present. Those are verified by hand from the checklist
below. This gap is
recorded rather than closed on purpose: a test that drives cargo would take tens of minutes and
would assert cargo's behaviour, not ours.

Neither suite covers Stage 2: provisioning asserts against real downloads. That path is verified by
hand too.

Before trusting a change, on each platform: fresh ZIP without git, clean repo behind origin, dirty
repo with non-overlapping edits, dirty repo with overlapping edits that merge, dirty repo with
edits that genuinely conflict (verify the tree is restored), `--offline` with no network, an update
whose commits touch `tools/run-dev/*` (expect exit 8, no build, and a correct restart command), a
run with a complete venv (expect no installer window between the build and the app), and a run with
the venv removed (expect the installer, then the application).

For the publishing step specifically: a first run (expect the binary to appear in the project root
and the application to start from it), an immediate second run with no source change (expect
"already the same build" and no copying), a run after an edit (expect the root copy to be
refreshed), a run started while an older root binary is still running (POSIX: expect a clean
replacement; Windows: expect the `.old-*` rename-aside path and, once that process exits and one
more run has happened, no leftover `.old-*`), and a run with the project root made read-only
(expect a warning and a normal launch
through `cargo run`). Finally, launch the root binary by hand from a different working directory —
a double click from a file manager — and confirm it finds the project root.
