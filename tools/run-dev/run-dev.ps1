<#
File: tools/run-dev/run-dev.ps1

Purpose:
Windows core of `run-dev`: bring the working copy up to date with origin,
provision Git / a Rust toolchain / a C toolchain when the machine has none, then
build the app, put the built binary in the project root and run it from there.

Main responsibilities:
- Stage 1 (git): provision portable MinGit when git is absent, adopt a working
  copy unpacked from a source ZIP, fetch, and merge local changes automatically
  when they do not truly conflict; ask git whether the update changed run-dev
  itself and request a restart instead of continuing.
- Stage 2 (rust): read the MSRV from Cargo.toml, pick a system or managed
  toolchain, provision an isolated one plus MinGW-w64 under installer_files/,
  and make sure gcc is reached through a path with no space in it — the project
  is normally installed under `C:\Program Files\ManhwaStudio`, and gcc splits
  its own unquoted spec paths on that space.
- Stage 3, strictly sequential: an environment check (`cargo run --check-venv`)
  that only shows a GUI when something is missing and is also what builds the
  project; publishing the built binary into the project root; then running that
  root binary. Publishing is a convenience — any failure falls back to the
  `cargo run` launch and never stops the run.

Key functions:
- Invoke-GitStage, Install-MinGit, Invoke-RepositoryAdoption, Update-WithLocalChanges
- Get-ChangedSelfPaths, Assert-NoSelfUpdate
- Invoke-Git (stdout and stderr are never mixed)
- Get-StashTop, Get-StashRefFor, Invoke-StashPop, Restore-StashEntry
- Invoke-RustStage, Install-ManagedRust, Install-Mingw, Assert-CToolchain
- Get-ShortPathName, Get-SubstMap, Get-FreeDriveLetters, Get-ExistingSubstDrive,
  New-SubstDrive, Remove-SubstDrive, Resolve-SpaceFreePath,
  Assert-SpaceFreeToolchainPath, Set-MingwEnvironment
- Invoke-CargoRun, Assert-AppEnvironment, Invoke-RunStage
- Get-BuiltBinaryFromJson, Resolve-BuiltBinary, Test-RootBinaryNeedsCopy,
  Install-RootBinary, Clear-RootBinaryLeftovers, Publish-RootBinary,
  Invoke-RootBinary

Notes:
The algorithm, its rationale, and every failure path are specified in
`dev-docs/run_dev_plan.md`. Linux/macOS are a separate implementation
(run-dev.sh); the git algorithm is intentionally identical in both.
Entered through run-dev.Windows.bat, which bypasses the execution policy.
This file MUST stay UTF-8 with BOM: Windows PowerShell 5.1 reads a BOM-less
script as the system ANSI code page and would mangle every Russian message.
PowerShell parses a script file into an AST in full before executing any of it,
so Stage 1 rewriting this file mid-run cannot corrupt the running program — the
only consequence is that the running code is stale, which Assert-NoSelfUpdate
catches. That property is relied upon; do not restructure the file around
dynamic dot-sourcing.
User-facing output is Russian by project convention; code comments are English.
#>

[CmdletBinding()]
param(
    [switch] $NoUpdate,
    [switch] $Offline,
    [switch] $DiscardLocal,
    [switch] $KeepLocal,
    # Not `-Debug`: that name is reserved by [CmdletBinding()]'s common parameters.
    [switch] $DebugBuild,
    [switch] $Yes,
    [switch] $Help,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $AppArgs = @()
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
# Invoke-WebRequest's progress bar makes large downloads several times slower on
# Windows PowerShell 5.1.
$ProgressPreference = 'SilentlyContinue'

# --- Constants ---------------------------------------------------------------

# Overridable so a fork can retarget without editing code.
$OriginUrl = if ($env:MS_RUN_DEV_ORIGIN) { $env:MS_RUN_DEV_ORIGIN } else { 'https://github.com/Vasyanator/ManhwaStudio.git' }
$Branch    = if ($env:MS_RUN_DEV_BRANCH) { $env:MS_RUN_DEV_BRANCH } else { 'master' }
$AppBin    = 'manhwastudio_rs'
$GitMin    = '2.13.0'   # `git stash push`, used by every rollback path

$GitApiMinGit = 'https://api.github.com/repos/git-for-windows/git/releases/latest'
$GitApiMingw  = 'https://api.github.com/repos/brechtsanders/winlibs_mingw/releases/latest'
$RustupUrlGnu = 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-gnu/rustup-init.exe'

# Files that are *executed* by a run-dev launch, in GIT notation: repository
# relative, forward slashes, because these strings are pathspecs handed to git and
# nothing else. Windows path separators would silently match nothing. When an
# update changes any of them the running scripts no longer match what is on disk,
# so the run stops and asks for a restart. Test/doc files of the module are
# deliberately absent: changing them cannot affect an in-flight run. Kept in the
# same order as SELF_PATHS in run-dev.sh.
$SelfGitPaths = @(
    'tools/run-dev/run-dev.sh',
    'tools/run-dev/run-dev.ps1',
    'run-dev.Linux.sh',
    'run-dev.MacOS.command',
    'run-dev.Windows.bat')

# Exit codes; see dev-docs/run_dev_plan.md.
$ExitGeneric         = 1
$ExitNoGit           = 2
$ExitManualMerge     = 3
$ExitNoRust          = 4
$ExitNoCc            = 5
$ExitAborted         = 6
$ExitVenvNotReady    = 7
$ExitRestartRequired = 8

# --- State -------------------------------------------------------------------

$script:RepoRoot    = $null
$script:Git         = $null
$script:Cargo       = $null
$script:Adopted     = $false
# Set by the adoption branch when it replaced the files on disk with the
# repository's version — the one case where there is no "before" commit to diff.
$script:AdoptedReplaced = $false
$script:ManagedRust = $false
# HEAD as it was before Stage 1 touched anything. It is both the rollback point of
# Update-WithLocalChanges and the left side of the "what did the update change"
# diff; there is deliberately only one such value.
$script:PreHead     = ''
# Exit code of the most recent Invoke-CargoRun; see that function for why the
# code is passed through state instead of being returned.
$script:LastRunCode = 0
# Full path of the binary published into the project root, or '' when publishing
# did not happen. Carried through state for the same reason as $script:LastRunCode:
# Publish-RootBinary's output stream belongs to cargo, so a returned path would
# arrive glued to the build log. Empty means "phase 2 goes through cargo run".
$script:RootExe     = ''
# Drive letter created by this run with `subst` so gcc can be reached through a
# path without a space (see "A space-free path to the C toolchain"), or '' when
# none was created. Released by Invoke-Main; a mapping that already existed is
# reused and never recorded here, because removing somebody else's is not ours
# to do.
$script:SubstDrive  = ''

# --- Output helpers ----------------------------------------------------------

function Say  { param([string] $Text = '') Write-Host $Text }
function Step { param([string] $Text) Write-Host ''; Write-Host "==> $Text" -ForegroundColor Cyan }
function Ok   { param([string] $Text) Write-Host "  ok " -ForegroundColor Green -NoNewline; Write-Host $Text }
function Warn { param([string] $Text) Write-Host "  ! $Text" -ForegroundColor Yellow }
function Info { param([string] $Text) Write-Host "    $Text" }

<#
.SYNOPSIS
Prints a framed block with the given title and colour, then terminates the
process with $Code. Waits for a key first unless -Yes, so a double-clicked window
does not vanish before the message can be read. Never returns.
#>
function Exit-WithBanner {
    param([int] $Code, [string] $Title, [System.ConsoleColor] $Color, [string[]] $Lines)
    Write-Host ''
    Write-Host '============================================================' -ForegroundColor $Color
    Write-Host "  $Title" -ForegroundColor $Color
    Write-Host '============================================================' -ForegroundColor $Color
    foreach ($l in $Lines) { Write-Host "  $l" }
    Write-Host ''
    if (-not $Yes) { Wait-ForKey }
    exit $Code
}

<#
.SYNOPSIS
Prints a framed error block and terminates the process with the given code.
Never returns.
#>
function Die {
    param([int] $Code, [string[]] $Lines)
    Exit-WithBanner -Code $Code -Title 'ОШИБКА' -Color Red -Lines $Lines
}

function Wait-ForKey {
    if ([Environment]::UserInteractive) {
        Write-Host 'Нажмите Enter для выхода...' -NoNewline
        try { [void](Read-Host) } catch { }
    }
}

function Show-Usage {
    Say @'
Запуск dev-версии ManhwaStudio из исходников.

  run-dev.Windows.bat [опции] [аргументы приложения]

Опции:
  -NoUpdate       Не обновляться из git, сразу собрать и запустить.
  -Offline        Не обращаться к сети вообще (проверка окружения пропускается).
  -DiscardLocal   Убрать локальные изменения (в git stash) и обновиться.
  -KeepLocal      Никогда не трогать локальные изменения.
  -DebugBuild     Собрать без --release (быстрее сборка, медленнее работа).
  -Yes            Не задавать вопросов, брать варианты по умолчанию.
  -Help           Показать эту справку.
'@
}

<#
.SYNOPSIS
Reads one line from the console, returning `$Default` when running under -Yes or
non-interactively, so no path can block on a prompt.
#>
function Read-Choice {
    param([string] $Default)
    if ($Yes -or -not [Environment]::UserInteractive) { return $Default }
    try {
        $answer = Read-Host
    } catch {
        return $Default
    }
    if ([string]::IsNullOrWhiteSpace($answer)) { return $Default }
    return $answer.Trim()
}

# --- Version helpers ---------------------------------------------------------

<#
.SYNOPSIS
Normalises "rustc 1.96.1 (hash date)" / "git version 2.43.0" to a bare
major.minor.patch, dropping any -nightly / -beta.N suffix. Returns $null when the
text carries no version.
#>
function Get-VersionNumber {
    param([string] $Text)
    if ([string]::IsNullOrWhiteSpace($Text)) { return $null }
    $m = [regex]::Match($Text, '(\d+)\.(\d+)(?:\.(\d+))?')
    if (-not $m.Success) { return $null }
    $patch = if ($m.Groups[3].Success) { $m.Groups[3].Value } else { '0' }
    return "$($m.Groups[1].Value).$($m.Groups[2].Value).$patch"
}

<#
.SYNOPSIS
True when version $Have is at least version $Want. Missing components count as 0,
so "1.9" correctly compares below "1.92".
#>
function Test-VersionAtLeast {
    param([string] $Have, [string] $Want)
    if (-not $Have) { return $false }
    $a = @($Have -split '\.'); $b = @($Want -split '\.')
    for ($i = 0; $i -lt 3; $i++) {
        $av = if ($i -lt $a.Count) { [int]($a[$i] -replace '\D.*$', '') } else { 0 }
        $bv = if ($i -lt $b.Count) { [int]($b[$i] -replace '\D.*$', '') } else { 0 }
        if ($av -gt $bv) { return $true }
        if ($av -lt $bv) { return $false }
    }
    return $true
}

<#
.SYNOPSIS
Parses a git `--count` output into an int, yielding 0 for anything that is not a
bare number. Guards against `[int]''` throwing when a git command failed.

Strictly whole-string on purpose: fishing the first digit group out of arbitrary
text would turn any stray message that happens to contain a number into a commit
count, and "behind = 1" opens the update branch on a repository with nothing
incoming. Only `rev-list --count` output — one line, digits only — is accepted.
#>
function Convert-ToCount {
    param([string] $Text)
    if ([string]::IsNullOrWhiteSpace($Text)) { return 0 }
    $t = $Text.Trim()
    if ($t -notmatch '^\d+$') { return 0 }
    return [int]$t
}

<#
.SYNOPSIS
True when $Text is a bare git object id: 40 hex characters (SHA-1) or 64
(SHA-256 repositories). Anything else — empty output, a warning, a truncated
line — is not a usable fingerprint.
#>
function Test-ObjectId {
    param([string] $Text)
    if ([string]::IsNullOrWhiteSpace($Text)) { return $false }
    # -cmatch, not -match: PowerShell's -match is case-INSENSITIVE, which would
    # accept upper-case hex that git never produces and that the POSIX
    # implementation rejects. The two must agree byte for byte.
    return ($Text.Trim() -cmatch '^[0-9a-f]{40}$|^[0-9a-f]{64}$')
}

# --- Download / archive helpers ----------------------------------------------

function Get-DownloadDir {
    $d = Join-Path $script:RepoRoot 'installer_files\downloads'
    if (-not (Test-Path $d)) { [void](New-Item -ItemType Directory -Path $d -Force) }
    return $d
}

$UserAgent = 'ManhwaStudio-run-dev'

<#
.SYNOPSIS
Enables TLS 1.2 for this process. Windows PowerShell 5.1 still defaults to TLS
1.0 on older builds, which github.com and static.rust-lang.org both refuse.
#>
function Set-TlsDefaults {
    try {
        [Net.ServicePointManager]::SecurityProtocol =
            [Net.SecurityProtocolType]::Tls12 -bor [Net.ServicePointManager]::SecurityProtocol
    } catch { }
}

<#
.SYNOPSIS
Formats a byte count as a human-readable size for progress and error messages.
#>
function Format-Bytes {
    param([long] $Bytes)
    if ($Bytes -ge 1073741824) { return ('{0:N1} ГБ' -f ($Bytes / 1073741824)) }
    if ($Bytes -ge 1048576)    { return ('{0:N0} МБ' -f ($Bytes / 1048576)) }
    if ($Bytes -ge 1024)       { return ('{0:N0} КБ' -f ($Bytes / 1024)) }
    return "$Bytes Б"
}

<#
.SYNOPSIS
Downloads $Url into the partial file $Part with curl.exe, resuming whatever is
already there. Throws when curl exits non-zero.

.DESCRIPTION
`-C -` is the resume; `--retry` handles transient errors on its own; `-f` turns an
HTTP error status into a non-zero exit instead of a saved error page. curl is
tried first because it retries and resumes without any help from us — but it is
NOT trusted to be available or even to work: on the machine this was debugged on,
curl reaches the release CDN and gets an empty reply (exit 52) while the .NET
stack downloads the same URL fine. Hence Get-RemoteFile falls through to the
other implementation on ANY failure, not only when curl.exe is missing.
#>
function Invoke-CurlDownload {
    param([string] $CurlPath, [string] $Url, [string] $Part)
    $curlArgs = @(
        '--fail', '--location', '--show-error',
        '--retry', '5', '--retry-delay', '2',
        '--connect-timeout', '30',
        '--user-agent', $UserAgent,
        '--continue-at', '-',
        '--progress-bar',
        '--output', $Part, $Url)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $CurlPath @curlArgs
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev
    if ($code -ne 0) { throw "curl завершился с кодом $code" }
}

<#
.SYNOPSIS
Downloads $Url into the partial file $Part with the .NET stack, resuming from the
bytes already on disk and retrying while the transfer keeps making progress.
Throws when it cannot finish.

.DESCRIPTION
This is not a fallback stub: on a link that drops mid-transfer — the failure that
motivated this loader, a 274 MB archive dying with "connection was closed
unexpectedly" — it is the implementation that gets the file. Each attempt sends a
Range header for the current file length and appends; an attempt that transferred
anything resets the failure counter, so a flaky link makes progress instead of
starting over. A server that answers 200 instead of 206 does not support ranges,
so the partial file is discarded rather than appended to (that would corrupt it).
#>
function Invoke-DotNetDownload {
    param([string] $Url, [string] $Part, [long] $ExpectedSize = 0)
    $maxStall = 4
    $stall = 0
    while ($true) {
        $offset = if (Test-Path $Part) { (Get-Item $Part).Length } else { 0 }
        if ($ExpectedSize -gt 0 -and $offset -ge $ExpectedSize) { return }
        $before = $offset
        $resp = $null; $in = $null; $out = $null
        try {
            $req = [Net.HttpWebRequest]::Create($Url)
            $req.UserAgent = $UserAgent
            $req.Timeout = 60000
            $req.ReadWriteTimeout = 300000
            if ($offset -gt 0) { $req.AddRange($offset) }
            $resp = $req.GetResponse()

            # 200 to a ranged request means "ranges not supported": start over
            # rather than append to bytes that are about to be repeated.
            $mode = [IO.FileMode]::Append
            if ($offset -gt 0 -and $resp.StatusCode -ne [Net.HttpStatusCode]::PartialContent) {
                $mode = [IO.FileMode]::Create
                $offset = 0
            }
            $total = if ($ExpectedSize -gt 0) { $ExpectedSize } else { $offset + $resp.ContentLength }

            $in  = $resp.GetResponseStream()
            $out = New-Object IO.FileStream($Part, $mode, [IO.FileAccess]::Write, [IO.FileShare]::None)
            $buffer = New-Object byte[] 262144
            $done = $offset
            $tick = [Diagnostics.Stopwatch]::StartNew()
            while (($read = $in.Read($buffer, 0, $buffer.Length)) -gt 0) {
                $out.Write($buffer, 0, $read)
                $done += $read
                if ($tick.Elapsed.TotalSeconds -ge 2) {
                    Write-Progress-Line -Done $done -Total $total
                    $tick.Restart()
                }
            }
            Write-Progress-Line -Done $done -Total $total -Final
            return
        } catch {
            $now = if (Test-Path $Part) { (Get-Item $Part).Length } else { 0 }
            if ($now -gt $before) { $stall = 0 } else { $stall++ }
            if ($stall -ge $maxStall) { throw }
            Start-Sleep -Seconds 2
        } finally {
            if ($out)  { $out.Dispose() }
            if ($in)   { $in.Dispose() }
            if ($resp) { $resp.Close() }
        }
    }
}

<#
.SYNOPSIS
Prints one in-place progress line. $ProgressPreference is deliberately off (it
makes Invoke-WebRequest several times slower on 5.1), so a long download would
otherwise look like a hang.
#>
function Write-Progress-Line {
    param([long] $Done, [long] $Total, [switch] $Final)
    $text = if ($Total -gt 0) {
        '    Загружено: {0} из {1} ({2:N0}%)' -f (Format-Bytes $Done), (Format-Bytes $Total), (100 * $Done / $Total)
    } else {
        '    Загружено: {0}' -f (Format-Bytes $Done)
    }
    Write-Host ("`r" + $text.PadRight(60)) -NoNewline
    if ($Final) { Write-Host '' }
}

<#
.SYNOPSIS
Downloads $Url to $Destination: resumable, verified, and never leaving a partial
file at the destination path. Throws with a Russian, actionable message when the
file could not be completed.

.DESCRIPTION
Contract, deliberately the same as `src/installer/utils.rs::download_asset`, so
the project has one principle rather than two:
- the bytes go to `<Destination>.part` and are renamed into place only once the
  file is complete, so $Destination never holds a truncated download;
- an interrupted run leaves the `.part` file alone, and the next run CONTINUES
  from it instead of starting over;
- $ExpectedSize (0 = unknown) is checked against the finished file — a short file
  is an error, not something to hand on to the unzipper;
- $Sha256Url (empty = none published) is fetched and verified; a mismatch deletes
  the `.part` file, because resuming corrupt bytes can never succeed.
Two implementations are tried in order and the second runs whenever the first
fails, not only when it is absent; see Invoke-CurlDownload.
#>
function Get-RemoteFile {
    param(
        [string] $Url,
        [string] $Destination,
        [long]   $ExpectedSize = 0,
        [string] $Sha256Url = ''
    )
    Set-TlsDefaults
    $part = "$Destination.part"

    $have = if (Test-Path $part) { (Get-Item $part).Length } else { 0 }
    if ($ExpectedSize -gt 0 -and $have -gt $ExpectedSize) {
        # Longer than the asset itself: not a resumable prefix, whatever it is.
        Remove-Item -Force $part -ErrorAction SilentlyContinue
        $have = 0
    }
    if ($have -gt 0) { Info "Продолжаю прерванную загрузку, уже есть $(Format-Bytes $have)" }

    $problems = @()
    $curl = Get-Command 'curl.exe' -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
    if ($curl -and -not $env:MS_RUN_DEV_NO_CURL) {
        try { Invoke-CurlDownload -CurlPath $curl.Source -Url $Url -Part $part }
        catch { $problems += "curl: $($_.Exception.Message)" }
    }
    if (-not (Test-DownloadComplete -Part $part -ExpectedSize $ExpectedSize)) {
        if ($problems.Count -gt 0) {
            # curl leaves its own progress bar on screen before failing, so say
            # plainly that the run is not over and something else is taking over.
            Warn 'Загрузка через curl не удалась, пробую встроенный загрузчик.'
        }
        try { Invoke-DotNetDownload -Url $Url -Part $part -ExpectedSize $ExpectedSize }
        catch { $problems += ".NET: $($_.Exception.Message)" }
    }

    if (-not (Test-DownloadComplete -Part $part -ExpectedSize $ExpectedSize)) {
        $got = if (Test-Path $part) { (Get-Item $part).Length } else { 0 }
        $detail = if ($ExpectedSize -gt 0) {
            "Получено $(Format-Bytes $got) из $(Format-Bytes $ExpectedSize)."
        } else {
            "Получено $(Format-Bytes $got)."
        }
        throw ("$detail " + ($problems -join '; '))
    }

    if ($Sha256Url) { Assert-Sha256 -Part $part -Sha256Url $Sha256Url }

    if (Test-Path $Destination) { Remove-Item -Force $Destination }
    Move-Item -LiteralPath $part -Destination $Destination -Force
}

<#
.SYNOPSIS
True when the partial file is finished: the exact expected size, or — when no
size is known — simply present and non-empty.
#>
function Test-DownloadComplete {
    param([string] $Part, [long] $ExpectedSize = 0)
    if (-not (Test-Path $Part)) { return $false }
    $len = (Get-Item $Part).Length
    if ($ExpectedSize -gt 0) { return ($len -eq $ExpectedSize) }
    return ($len -gt 0)
}

<#
.SYNOPSIS
Verifies $Part against the checksum published next to the asset. Throws (and
deletes the partial file) on a mismatch; a sidecar that cannot be fetched is a
warning, never a failure — the checksum is a bonus, not a prerequisite.
#>
function Assert-Sha256 {
    param([string] $Part, [string] $Sha256Url)
    $tmp = "$Part.sha256"
    try {
        Invoke-WebRequest -Uri $Sha256Url -OutFile $tmp -UseBasicParsing `
            -Headers @{ 'User-Agent' = $UserAgent }
        $text = (Get-Content -LiteralPath $tmp -Raw)
    } catch {
        Warn 'Не удалось получить контрольную сумму, проверка пропущена.'
        return
    } finally {
        Remove-Item -Force $tmp -ErrorAction SilentlyContinue
    }

    $m = [regex]::Match($text, '[0-9a-fA-F]{64}')
    if (-not $m.Success) {
        Warn 'Файл контрольной суммы не распознан, проверка пропущена.'
        return
    }
    $actual = (Get-FileHash -LiteralPath $Part -Algorithm SHA256).Hash
    if ($actual -ne $m.Value) {
        Remove-Item -Force $Part -ErrorAction SilentlyContinue
        throw 'контрольная сумма не совпала, файл повреждён (скачается заново)'
    }
    Info 'Контрольная сумма совпала.'
}

<#
.SYNOPSIS
Resolves the first asset of a GitHub "latest release" whose name matches
$Pattern, as an object with Url, Size and Sha256Url. Returns $null when the API
is unreachable or nothing matches — the caller must then fail loudly rather than
invent a URL.

.DESCRIPTION
Size comes from the API rather than from the transfer, so the downloader can tell
"finished" from "the connection closed near the end". Sha256Url points at the
`<asset>.sha256` sidecar when the release publishes one (winlibs does) and is
empty otherwise; a missing sidecar skips verification, it never blocks an install.
#>
function Resolve-GithubAsset {
    param([string] $ApiUrl, [string] $Pattern)
    Set-TlsDefaults
    try {
        $release = Invoke-RestMethod -Uri $ApiUrl -UseBasicParsing `
            -Headers @{ 'User-Agent' = $UserAgent }
    } catch {
        return $null
    }
    $asset = $release.assets | Where-Object { $_.name -match $Pattern } | Select-Object -First 1
    if (-not $asset) { return $null }

    $sidecarName = $asset.name + '.sha256'
    $sidecar = $release.assets | Where-Object { $_.name -eq $sidecarName } | Select-Object -First 1
    return [pscustomobject]@{
        Name      = $asset.name
        Url       = $asset.browser_download_url
        Size      = [long]$asset.size
        Sha256Url = if ($sidecar) { $sidecar.browser_download_url } else { '' }
    }
}

<#
.SYNOPSIS
Extracts a zip into $Destination. Uses System.IO.Compression directly because
Expand-Archive is markedly slower on the ~100 MB toolchain archives.
#>
function Expand-ZipTo {
    param([string] $ZipPath, [string] $Destination)
    if (Test-Path $Destination) { Remove-Item -Recurse -Force $Destination }
    [void](New-Item -ItemType Directory -Path $Destination -Force)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory($ZipPath, $Destination)
}

# =============================================================================
# Stage 1 — git
# =============================================================================

<#
.SYNOPSIS
Runs the resolved git with the given arguments and returns an object with
ExitCode, Output (stdout only) and Error (stderr only). Never throws on a
non-zero git exit.

.DESCRIPTION
Keeping the two streams apart is a correctness requirement, not tidiness. git
writes advisory text to stderr on perfectly successful commands — "warning: in
the working copy of 'x', LF will be replaced by CRLF", ambiguous-refname notes,
background-gc chatter — and every consumer here parses Output as data: a
fingerprint, a commit count, a porcelain status. A warning merged into Output
becomes a bogus hash, a bogus count, or a bogus "dirty tree". The POSIX
implementation gets this for free by sending stderr to /dev/null (run-dev.sh);
this is the same contract, with the text kept for diagnostics instead of dropped.

Separation is by object type rather than by order: with `2>&1` a native command's
stderr lines arrive as ErrorRecord objects and its stdout lines as strings, so the
split does not depend on how the two streams interleave — which is exactly the
thing that is not guaranteed between runs.
#>
function Invoke-Git {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]] $Arguments)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $captured = @(& $script:Git @Arguments 2>&1)
        $code = $LASTEXITCODE
        $out = @($captured | Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] })
        $err = @($captured | Where-Object { $_ -is  [System.Management.Automation.ErrorRecord] })
        return [pscustomobject]@{
            ExitCode = $code
            Output   = (($out | ForEach-Object { [string]$_ }) -join "`n")
            Error    = (($err | ForEach-Object { [string]$_ }) -join "`n")
        }
    } finally {
        $ErrorActionPreference = $prev
    }
}

<#
.SYNOPSIS
Flattens the stderr of an Invoke-Git result into one line for a failure message.
Callers keep the result object rather than re-running the command — a second
`stash push` would not be free of consequences.
#>
function Format-GitError {
    param([pscustomobject] $Result)
    return ($Result.Error -replace "`r?`n", ' ').Trim()
}

function Test-GitOk {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]] $Arguments)
    return (Invoke-Git @Arguments).ExitCode -eq 0
}

function Get-GitOut {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]] $Arguments)
    return (Invoke-Git @Arguments).Output.Trim()
}

<#
.SYNOPSIS
Downloads the official MinGit redistributable into installer_files\git.
MinGit is the minimal Git for Windows package: no installer, no admin rights,
and it carries everything fetch/merge/stash need.
#>
function Install-MinGit {
    if ($Offline) {
        Die $ExitNoGit @(
            'Git не найден, а режим -Offline запрещает скачивание.',
            'Установите Git вручную: https://git-scm.com/download/win',
            'Либо запустите без обновления: run-dev.Windows.bat -NoUpdate')
    }

    Step 'Git не найден — скачиваю портативную версию'
    Info 'Устанавливается только внутрь installer_files\git.'
    Info 'Система и настройки Windows не затрагиваются.'

    $pattern = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
        '^MinGit-.*-arm64\.zip$'
    } else {
        '^MinGit-.*-64-bit\.zip$'
    }
    $asset = Resolve-GithubAsset -ApiUrl $GitApiMinGit -Pattern $pattern
    if (-not $asset) {
        Die $ExitNoGit @(
            'Не удалось узнать адрес загрузки Git с GitHub.',
            'Проверьте интернет-соединение, либо установите Git вручную:',
            '    https://git-scm.com/download/win',
            '',
            'После установки запустите этот скрипт снова.')
    }

    $zip = Join-Path (Get-DownloadDir) 'MinGit.zip'
    $dst = Join-Path $script:RepoRoot 'installer_files\git'
    try {
        Info "Загрузка: $($asset.Name)  ($(Format-Bytes $asset.Size))"
        Get-RemoteFile -Url $asset.Url -Destination $zip `
                       -ExpectedSize $asset.Size -Sha256Url $asset.Sha256Url
    } catch {
        Die $ExitNoGit @(
            'Не удалось скачать Git.',
            "Причина: $($_.Exception.Message)",
            '',
            'Запустите run-dev ещё раз: загрузка продолжится с того места,',
            'на котором оборвалась, а не начнётся заново.',
            'Либо установите Git вручную: https://git-scm.com/download/win')
    }
    try {
        Expand-ZipTo -ZipPath $zip -Destination $dst
    } catch {
        Remove-Item -Force $zip -ErrorAction SilentlyContinue
        Die $ExitNoGit @(
            'Архив с Git скачался, но распаковать его не удалось.',
            "Причина: $($_.Exception.Message)",
            'Проверьте, хватает ли места на диске, и запустите run-dev снова.')
    } finally {
        if (Test-Path $zip) { Remove-Item -Force $zip -ErrorAction SilentlyContinue }
    }

    $exe = Join-Path $dst 'cmd\git.exe'
    if (-not (Test-Path $exe)) {
        Die $ExitNoGit @(
            "Git распакован, но $exe не найден.",
            'Удалите папку installer_files\git и попробуйте снова.')
    }
    $script:Git = $exe
    Ok "Git установлен: $(Get-VersionNumber (Get-GitOut '--version'))"
}

function Find-Git {
    # A previously provisioned MinGit wins: it exists because a past run needed it.
    $managed = Join-Path $script:RepoRoot 'installer_files\git\cmd\git.exe'
    if (Test-Path $managed) {
        $script:Git = $managed
    } else {
        $sys = Get-Command git -ErrorAction SilentlyContinue
        if ($sys) { $script:Git = $sys.Source }
    }

    if (-not $script:Git) { Install-MinGit; return }

    $probe = Invoke-Git '--version'
    if ($probe.ExitCode -ne 0) { Install-MinGit; return }

    $ver = Get-VersionNumber $probe.Output
    if ($ver -and -not (Test-VersionAtLeast $ver $GitMin)) {
        Warn "Версия git слишком старая: $ver (нужна $GitMin или новее)."
        Install-MinGit
    }
}

<#
.SYNOPSIS
Grafts history onto a working copy unpacked from a source ZIP, WITHOUT writing
anything into the working tree. update-ref + symbolic-ref + a bare reset --mixed
is used instead of `checkout -B`, which would refuse to run (or would overwrite
files) on an already-populated directory. Returns $false when it could not run.
#>
function Invoke-RepositoryAdoption {
    Step 'Рабочая копия не является git-репозиторием (похоже, распакована из архива)'
    if ($Offline) {
        Warn 'Режим -Offline: подключить репозиторий нельзя, обновление пропущено.'
        return $false
    }
    Info "Подключаю историю из $OriginUrl, файлы на диске при этом не трогаются."

    if (-not (Test-GitOk 'init')) {
        Die $ExitGeneric @("Не удалось выполнить git init в $script:RepoRoot.")
    }
    if (-not (Test-GitOk 'remote' 'add' 'origin' $OriginUrl)) {
        [void](Invoke-Git 'remote' 'set-url' 'origin' $OriginUrl)
    }
    if (-not (Test-GitOk 'fetch' '-q' 'origin')) {
        Die $ExitGeneric @(
            'Не удалось скачать историю репозитория.',
            "Проверьте интернет-соединение и доступность $OriginUrl.",
            'Запустить без обновления: run-dev.Windows.bat -NoUpdate')
    }
    if (-not (Test-GitOk 'rev-parse' '--verify' "refs/remotes/origin/$Branch")) {
        Die $ExitGeneric @("В репозитории нет ветки origin/$Branch.")
    }

    [void](Invoke-Git 'update-ref' "refs/heads/$Branch" "refs/remotes/origin/$Branch")
    [void](Invoke-Git 'symbolic-ref' 'HEAD' "refs/heads/$Branch")
    [void](Invoke-Git 'reset' '--mixed' '-q')   # index := HEAD tree; work tree untouched
    [void](Invoke-Git 'branch' "--set-upstream-to=origin/$Branch" $Branch)

    $script:Adopted = $true
    Ok "История подключена, текущая ветка: $Branch"
    return $true
}

<#
.SYNOPSIS
After adoption the files are whatever the ZIP contained. Git cannot tell a stale
release file from a deliberate edit, so this asks instead of guessing.
#>
function Resolve-AdoptedTree {
    $dirty = Get-GitOut 'status' '--porcelain' '--untracked-files=no'
    if ([string]::IsNullOrWhiteSpace($dirty)) {
        Ok "Файлы на диске полностью совпадают с origin/$Branch."
        return
    }
    $count = @($dirty -split "`n").Count

    Step "Файлы из архива отличаются от актуальной версии: $count шт."
    if ($KeepLocal) { Warn 'Указан -KeepLocal: оставляю файлы как есть.'; return }
    Info 'Скорее всего архив просто старее репозитория.'
    Info 'Непроиндексированные файлы (проекты, модели, настройки) не затрагиваются.'
    Say ''
    Say '  [1] Взять актуальную версию файлов  (текущее содержимое сохранится в git stash)'
    Say '  [2] Оставить файлы как есть'
    Say '  [3] Выйти'
    Write-Host 'Ваш выбор [1]: ' -NoNewline
    switch (Read-Choice '1') {
        '1' {
            # [void]: Save-LocalChanges returns a flag, which would otherwise be
            # printed to the console by this statement.
            [void](Save-LocalChanges "run-dev: содержимое архива $(Get-Date -Format 'yyyy-MM-dd HH:mm')")
            # Taking the repository's version replaces every tracked file, run-dev
            # included, and there is no "before" commit to diff against here — so
            # the branch records what it did for Assert-NoSelfUpdate.
            $script:AdoptedReplaced = $true
            Ok "Файлы обновлены до origin/$Branch."
        }
        '2' { Warn 'Файлы оставлены без изменений.' }
        default { Die $ExitAborted @('Отменено пользователем.') }
    }
}

<#
.SYNOPSIS
Returns the object id of the top stash entry, or '' when the stash is empty.
Used to tell "push created an entry" from "push had nothing to save", which
`git stash push` reports with the same exit code 0.
#>
function Get-StashTop {
    $probe = Invoke-Git 'rev-parse' '-q' '--verify' 'refs/stash'
    if ($probe.ExitCode -ne 0) { return '' }
    if (-not (Test-ObjectId $probe.Output)) { return '' }
    return $probe.Output.Trim()
}

<#
.SYNOPSIS
Returns the `stash@{N}` selector of the entry whose commit id is $Id, or '' when
that entry is no longer in the stash.

.DESCRIPTION
The stash is a stack shared with everything else on the machine — an IDE,
TortoiseGit, a second terminal. Between our push and our pop somebody else may
have pushed (their entry is now on top) or dropped one (indices shifted), so an
entry must always be addressed by identity, never by position.
#>
function Get-StashRefFor {
    param([string] $Id)
    if (-not (Test-ObjectId $Id)) { return '' }
    $list = Invoke-Git 'stash' 'list' '--format=%H %gd'
    if ($list.ExitCode -ne 0) { return '' }
    foreach ($line in ($list.Output -split "`r?`n")) {
        $parts = $line.Trim() -split '\s+', 2
        # -ceq: object ids are compared byte-exactly, as on the POSIX side.
        if ($parts.Count -eq 2 -and $parts[0] -ceq $Id.Trim()) { return $parts[1] }
    }
    return ''
}

<#
.SYNOPSIS
Pops exactly the stash entry with commit id $Id. Returns $false when that entry
is gone or when the pop failed — a conflicting pop keeps the entry, so it stays
addressable by the same id.
#>
function Invoke-StashPop {
    param([string] $Id)
    $ref = Get-StashRefFor $Id
    if (-not $ref) { return $false }
    return (Test-GitOk 'stash' 'pop' '-q' $ref)
}

<#
.SYNOPSIS
Restores the entry created by this run, warning instead of failing when it
cannot. Used on rollback paths, where the caller is already on its way to Die
with its own message: a lost entry must be reported, not hidden, and must not
replace the primary error either.
#>
function Restore-StashEntry {
    param([string] $Id)
    if (-not $Id) { return }
    if (Invoke-StashPop $Id) { return }
    Warn 'Локальные изменения остались в git stash — посмотрите: git stash list'
}

<#
.SYNOPSIS
Saves tracked modifications into the stash. Untracked files are NEVER included:
the working directory holds the user's projects, models and configs. Returns
$true when an entry was actually created.

A push that saves nothing still exits 0, so the caller is told the difference:
claiming "recoverable via git stash pop" when no entry exists would point the
user at somebody else's older stash.
#>
function Save-LocalChanges {
    param([string] $Message)
    $before = Get-StashTop
    $push = Invoke-Git 'stash' 'push' '-q' '-m' $Message
    if ($push.ExitCode -ne 0) {
        $detail = Format-GitError $push
        $lines = @('Не удалось сохранить локальные изменения в git stash.')
        if ($detail) { $lines += "Git сообщает: $detail" }
        Die $ExitGeneric $lines
    }
    $after = Get-StashTop
    if ($after -eq $before) {
        Warn 'Сохранять было нечего: git не нашёл изменений в отслеживаемых файлах.'
        return $false
    }
    # Name the entry explicitly: a bare `git stash pop` takes whatever is on top,
    # which may belong to another process by the time the user runs it.
    $ref = Get-StashRefFor $after
    if ($ref) {
        Info "Прежнее содержимое сохранено. Вернуть: git stash pop $ref"
    } else {
        Info 'Прежнее содержимое сохранено. Найти: git stash list'
    }
    return $true
}

<#
.SYNOPSIS
Runs the merge appropriate for the ahead/behind relationship. Returns $true on
success; on conflict it aborts the merge and returns $false, leaving no merge in
progress.
#>
function Invoke-Merge {
    param([int] $Ahead)
    $ok = if ($Ahead -eq 0) {
        Test-GitOk 'merge' '--ff-only' '-q' "origin/$Branch"
    } else {
        Test-GitOk 'merge' '--no-edit' '-q' "origin/$Branch"
    }
    if ($ok) { return $true }
    [void](Invoke-Git 'merge' '--abort')
    return $false
}

<#
.SYNOPSIS
The core of the tool: update a working copy that has local modifications.
Restores the tree to exactly its pre-update state on any failure.
#>
function Update-WithLocalChanges {
    param([int] $Ahead)
    # $script:PreHead is the single pre-update HEAD of this run: the rollback
    # point here and the left side of the self-update diff later. Invoke-GitStage
    # sets it; the fallback keeps the function usable on its own (tests drive it
    # directly).
    if (-not $script:PreHead) { $script:PreHead = (Get-GitOut 'rev-parse' 'HEAD').Trim() }
    $preHead = $script:PreHead

    # Three-dot diff = what the INCOMING commits touch (from the merge base),
    # which is the set that can actually collide with local edits.
    $localFiles  = @((Get-GitOut 'diff' '--name-only' 'HEAD') -split "`n" | Where-Object { $_ })
    $remoteFiles = @((Get-GitOut 'diff' '--name-only' "HEAD...origin/$Branch") -split "`n" | Where-Object { $_ })
    $overlap     = @($localFiles | Where-Object { $remoteFiles -contains $_ })

    if ($overlap.Count -gt 0) {
        Info 'Локальные правки затрагивают те же файлы, что и новые коммиты:'
        foreach ($f in $overlap) { Info "    $f" }
        Info 'Пробую слить автоматически — git часто справляется сам.'
    } else {
        Info 'Локальные правки не пересекаются с новыми коммитами.'
    }

    # A stash push that saves nothing also exits 0, and the stash is shared with
    # every other git client on the machine. So this records the identity of the
    # entry it created and every restore below pops THAT entry: popping the
    # current top instead would apply a stranger's work to the tree, or nothing
    # at all — corruption, not a cosmetic bug.
    $stashBefore = Get-StashTop
    $push = Invoke-Git 'stash' 'push' '-q' '-m' 'run-dev: автосохранение перед обновлением'
    if ($push.ExitCode -ne 0) {
        $detail = Format-GitError $push
        $lines = @('Не удалось временно сохранить локальные изменения.')
        if ($detail) { $lines += "Git сообщает: $detail" }
        Die $ExitGeneric $lines
    }
    $ourStash = Get-StashTop
    if ($ourStash -eq $stashBefore) { $ourStash = '' }

    if (-not (Invoke-Merge -Ahead $Ahead)) {
        [void](Invoke-Git 'reset' '--hard' '-q' $preHead)
        Restore-StashEntry $ourStash
        Die $ExitManualMerge @(
            'Не удалось объединить локальные коммиты с новой версией.',
            'Рабочая копия возвращена в исходное состояние, ничего не потеряно.',
            '',
            "Слейте вручную:  git merge origin/$Branch")
    }

    if (-not $ourStash) {
        # Nothing was stashed, so nothing has to come back.
        Ok 'Обновлено до актуальной версии.'
        return
    }

    if (-not (Invoke-StashPop $ourStash)) {
        if (-not (Get-StashRefFor $ourStash)) {
            # The entry is not merely conflicting — it is gone, taken by another
            # process. There is nothing safe left to apply, and guessing at the
            # current top is exactly what must not happen here.
            Die $ExitManualMerge @(
                'Обновление применено, но вернуть локальные изменения не удалось:',
                'созданная run-dev запись в git stash исчезла — её мог забрать',
                'другой git-клиент (IDE, второй терминал).',
                '',
                'Посмотрите список сохранённого:  git stash list',
                'Вернуть нужную запись:           git stash pop stash@{N}')
        }
        # `stash pop` keeps the stash entry when it conflicts, so the work is
        # safe. `reset --hard` clears the conflicted index, the working tree and
        # the merge commit at once; the pop then replays onto the original base
        # and therefore applies cleanly.
        [void](Invoke-Git 'reset' '--hard' '-q' $preHead)
        Restore-StashEntry $ourStash
        Die $ExitManualMerge @(
            'Локальные изменения конфликтуют с новой версией — нужно ручное слияние.',
            'Рабочая копия возвращена в исходное состояние, ничего не потеряно.',
            '',
            'Конфликтующие файлы:',
            "  $($overlap -join ', ')",
            '',
            'Варианты:',
            "  1) слить вручную:            git merge origin/$Branch",
            '  2) убрать локальные правки:  run-dev.Windows.bat -DiscardLocal',
            '     (они сохранятся в git stash, вернуть можно через git stash pop)')
    }

    Ok 'Обновлено, локальные изменения сохранены.'
}

# --- Self-update detection ---------------------------------------------------
#
# Stage 1 updates the working copy, and run-dev is part of that working copy: a
# run started with the old scripts can end up executing a mix of old and new
# logic. The fix is not to be clever about reloading, it is to notice and ask for
# a restart.

<#
.SYNOPSIS
Returns the run-dev paths the update actually changed: exactly what
`git diff --name-only <before> <after>` reports for $SelfGitPaths. Always an
array, possibly empty.

.DESCRIPTION
Asking git which paths a commit range touched is the whole mechanism. The
previous implementation hashed the files before and after and compared the bytes,
which made the answer depend on line endings, clean/smudge filters and the
incidental rewrites `git stash push` performs — none of which have anything to do
with "did the update change run-dev". git already knows the answer.

Returns a plain array and is consumed as `@(Get-ChangedSelfPaths)`; see the array
convention in tools/run-dev/MODULE_README.md.
#>
function Get-ChangedSelfPaths {
    if (-not $script:PreHead) { return @() }
    # Not $args: that name is an automatic variable inside a function.
    $gitArgs = @('diff', '--name-only', $script:PreHead, 'HEAD', '--') + $SelfGitPaths
    $probe = Invoke-Git @gitArgs
    if ($probe.ExitCode -ne 0) { return @() }
    return @($probe.Output -split "`r?`n" | Where-Object { $_.Trim() } | ForEach-Object { $_.Trim() })
}

<#
.SYNOPSIS
The command that starts run-dev again with the same options, printed in the
restart message.
#>
function Get-RestartCommand {
    $parts = @('run-dev.Windows.bat')
    if ($NoUpdate)     { $parts += '-NoUpdate' }
    if ($Offline)      { $parts += '-Offline' }
    if ($DiscardLocal) { $parts += '-DiscardLocal' }
    if ($KeepLocal)    { $parts += '-KeepLocal' }
    if ($DebugBuild)   { $parts += '-DebugBuild' }
    if ($Yes)          { $parts += '-Yes' }
    foreach ($a in $AppArgs) {
        # Display only: an argument with spaces must still read as one argument.
        if ([string]::IsNullOrWhiteSpace($a) -or $a -match '\s') {
            $parts += """$a"""
        } else {
            $parts += $a
        }
    }
    return ($parts -join ' ')
}

<#
.SYNOPSIS
Stops the run when Stage 1 replaced run-dev itself. Nothing is rolled back: the
update is applied and correct, only the running scripts are stale. Returns
normally when the update did not touch them.

.DESCRIPTION
Two sources of truth, no hashing:
- a normal update: HEAD moved, so ask git which of $SelfGitPaths the commit range
  touched. HEAD unchanged means nothing was updated at all — nothing to check.
- an adopted ZIP copy: there is no "before" commit to diff against, and the
  adoption may have replaced every tracked file at once. That branch reports what
  it did through $script:AdoptedReplaced, so the answer comes from the stage
  itself rather than from a guess.
#>
function Assert-NoSelfUpdate {
    if ($script:AdoptedReplaced) {
        Exit-WithBanner -Code $ExitRestartRequired -Title 'НУЖЕН ПЕРЕЗАПУСК' -Color Yellow -Lines @(
            'Файлы проекта, включая сам скрипт запуска run-dev, заменены',
            'на актуальную версию из репозитория.',
            '',
            'Обновление уже применено, откатывать ничего не нужно.',
            'Запустите run-dev ещё раз, чтобы дальше работала новая версия:',
            '',
            "    $(Get-RestartCommand)")
    }

    if (-not $script:PreHead) { return }
    $headNow = Get-GitOut 'rev-parse' 'HEAD'
    if (-not (Test-ObjectId $headNow)) { return }
    if ($headNow.Trim() -ceq $script:PreHead) { return }

    $changed = @(Get-ChangedSelfPaths)
    if ($changed.Count -eq 0) { return }

    $lines = @(
        'Обновление затронуло сам скрипт запуска run-dev.',
        '',
        'Обновлены файлы:')
    foreach ($f in $changed) { $lines += "    $f" }
    $lines += @(
        '',
        'Обновление уже применено, откатывать ничего не нужно.',
        'Запустите run-dev ещё раз, чтобы дальше работала новая версия:',
        '',
        "    $(Get-RestartCommand)")
    Exit-WithBanner -Code $ExitRestartRequired -Title 'НУЖЕН ПЕРЕЗАПУСК' `
                    -Color Yellow -Lines $lines
}

function Invoke-GitStage {
    Step 'Проверка обновлений'
    Find-Git

    if (-not (Test-GitOk 'rev-parse' '--git-dir')) {
        if (-not (Invoke-RepositoryAdoption)) { return }
        Resolve-AdoptedTree
        return
    }

    # The pre-update HEAD: rollback point for the merge paths and left side of
    # the "did the update change run-dev itself" diff. Taken before anything can
    # move it. An empty repository has no HEAD; then there is nothing to compare
    # and nothing to update either.
    $head = Get-GitOut 'rev-parse' 'HEAD'
    $script:PreHead = if (Test-ObjectId $head) { $head.Trim() } else { '' }

    if (-not (Test-GitOk 'remote' 'get-url' 'origin')) {
        Info 'Удалённый репозиторий не настроен, добавляю origin.'
        [void](Invoke-Git 'remote' 'add' 'origin' $OriginUrl)
    }

    if ($Offline) { Warn 'Режим -Offline: проверка обновлений пропущена.'; return }

    if (-not (Test-GitOk 'fetch' '--prune' '-q' 'origin' $Branch)) {
        # Being offline must never block running the app.
        Warn 'Не удалось связаться с репозиторием — обновление пропущено.'
        return
    }

    # `rev-list` prints nothing if it fails; `[int]''` would throw under StrictMode.
    $behind = Convert-ToCount (Get-GitOut 'rev-list' '--count' "HEAD..origin/$Branch")
    $ahead  = Convert-ToCount (Get-GitOut 'rev-list' '--count' "origin/$Branch..HEAD")

    if ($behind -eq 0) { Ok 'Установлена актуальная версия.'; return }

    Info "Доступно новых коммитов: $behind"
    if ($ahead -ne 0) { Info "Локальных коммитов, которых нет в origin: $ahead" }

    $dirty = Get-GitOut 'status' '--porcelain' '--untracked-files=no'

    if ([string]::IsNullOrWhiteSpace($dirty)) {
        if (Invoke-Merge -Ahead $ahead) {
            Ok 'Обновлено до актуальной версии.'
        } else {
            Die $ExitManualMerge @(
                'Локальные коммиты конфликтуют с новой версией.',
                'Слияние отменено, рабочая копия не изменена.',
                '',
                "Слейте вручную:  git merge origin/$Branch")
        }
        return
    }

    Info "Изменённых файлов в рабочей копии: $(@($dirty -split "`n").Count)"

    $choice = if ($DiscardLocal) { '2' } elseif ($KeepLocal) { '3' } else {
        Say ''
        Say '  [1] Попробовать слить автоматически            (по умолчанию)'
        Say '  [2] Убрать локальные изменения и обновиться    (сохранятся в git stash)'
        Say '  [3] Пропустить обновление и запустить как есть'
        Say '  [4] Выйти'
        Write-Host 'Ваш выбор [1]: ' -NoNewline
        Read-Choice '1'
    }

    switch ($choice) {
        '1' { Update-WithLocalChanges -Ahead $ahead }
        '2' {
            # [void]: the returned flag must not reach the console.
            [void](Save-LocalChanges "run-dev: отброшенные изменения $(Get-Date -Format 'yyyy-MM-dd HH:mm')")
            if (Invoke-Merge -Ahead $ahead) {
                Ok 'Обновлено. Локальные изменения лежат в git stash.'
            } else {
                Die $ExitManualMerge @('Не удалось обновиться даже после очистки рабочей копии.')
            }
        }
        '3' { Warn 'Обновление пропущено по вашему выбору.' }
        default { Die $ExitAborted @('Отменено пользователем.') }
    }
}

# =============================================================================
# Stage 2 — rust
# =============================================================================

<#
.SYNOPSIS
Reads `rust-version` from [package] in Cargo.toml. There is deliberately no
fallback constant: a silently wrong MSRV turns into a confusing mid-build type
error instead of a clear message.
#>
function Get-RequiredMsrv {
    $cargoToml = Join-Path $script:RepoRoot 'Cargo.toml'
    $line = Select-String -Path $cargoToml -Pattern '^\s*rust-version\s*=' |
            Select-Object -First 1
    if ($line) {
        $m = [regex]::Match($line.Line, '"([^"]+)"')
        if ($m.Success) { return $m.Groups[1].Value }
    }
    Die $ExitGeneric @(
        'В Cargo.toml не найдено поле rust-version — не с чем сравнивать версию Rust.',
        'Это ошибка в самом проекте, а не в вашей системе.')
}

function Get-RustRoot     { Join-Path $script:RepoRoot 'installer_files\rust' }
function Get-ManagedCargo { Join-Path (Get-RustRoot) 'cargo\bin\cargo.exe' }
function Get-MingwDir     { Join-Path $script:RepoRoot 'installer_files\mingw64' }

function Use-ManagedRust {
    $env:RUSTUP_HOME = Join-Path (Get-RustRoot) 'rustup'
    $env:CARGO_HOME  = Join-Path (Get-RustRoot) 'cargo'
    $env:PATH        = "$($env:CARGO_HOME)\bin;$($env:PATH)"
    $script:Cargo       = Get-ManagedCargo
    $script:ManagedRust = $true
}

<#
.SYNOPSIS
Returns the rustc version paired with the given cargo, or $null.
#>
function Get-RustcVersion {
    param([string] $CargoPath)
    $rustc = if ($CargoPath -and (Test-Path $CargoPath)) {
        Join-Path (Split-Path $CargoPath -Parent) 'rustc.exe'
    } else { 'rustc' }
    if ($rustc -ne 'rustc' -and -not (Test-Path $rustc)) { return $null }
    try {
        $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
        $out = & $rustc --version 2>&1
        $ErrorActionPreference = $prev
        return Get-VersionNumber ($out -join ' ')
    } catch { return $null }
}

<#
.SYNOPSIS
Installs an isolated toolchain under installer_files\rust. Uses the GNU host
(see dev-docs/run_dev_plan.md §2.4): it matches the shipped release triple,
needs no Visual Studio, and needs no NASM for aws-lc-sys. Never writes to
%USERPROFILE%\.cargo, %USERPROFILE%\.rustup or PATH (--no-modify-path).
#>
function Install-ManagedRust {
    if ($Offline) {
        Die $ExitNoRust @(
            'Rust нужной версии не найден, а режим -Offline запрещает скачивание.',
            'Запустите без -Offline, либо установите Rust самостоятельно: https://rustup.rs')
    }
    if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
        Die $ExitNoRust @(
            'Автоматическая установка Rust реализована только для 64-битной x86 Windows.',
            'На ARM64 установите Rust вручную: https://rustup.rs',
            'После установки запустите этот скрипт снова.')
    }

    Step 'Устанавливаю Rust (изолированно, в installer_files\rust)'
    Info 'Системный Rust и переменные среды Windows не затрагиваются.'
    Info 'Скачивается около 250 МБ, это займёт несколько минут.'

    $init = Join-Path (Get-DownloadDir) 'rustup-init.exe'
    try {
        # No published size or checksum for this one: it is a plain static URL,
        # not a GitHub release asset. Resume and retries still apply.
        Get-RemoteFile -Url $RustupUrlGnu -Destination $init
    } catch {
        Die $ExitNoRust @(
            'Не удалось скачать установщик Rust.',
            "Причина: $($_.Exception.Message)",
            'Проверьте интернет-соединение и запустите run-dev ещё раз —',
            'загрузка продолжится с того места, на котором оборвалась.')
    }

    $env:RUSTUP_HOME = Join-Path (Get-RustRoot) 'rustup'
    $env:CARGO_HOME  = Join-Path (Get-RustRoot) 'cargo'
    [void](New-Item -ItemType Directory -Path (Get-RustRoot) -Force)

    $p = Start-Process -FilePath $init -Wait -NoNewWindow -PassThru -ArgumentList @(
        '-y', '--no-modify-path', '--profile', 'minimal',
        '--default-toolchain', 'stable', '--default-host', 'x86_64-pc-windows-gnu')
    Remove-Item -Force $init -ErrorAction SilentlyContinue

    if ($p.ExitCode -ne 0) {
        Die $ExitNoRust @(
            "Установка Rust завершилась с ошибкой (код $($p.ExitCode)).",
            'Подробности — в выводе выше.')
    }
    if (-not (Test-Path (Get-ManagedCargo))) {
        Die $ExitNoRust @("Rust установился, но cargo не найден: $(Get-ManagedCargo)")
    }

    Use-ManagedRust
    Ok "Rust установлен: $(Get-RustcVersion $script:Cargo)"
}

# --- A space-free path to the C toolchain ------------------------------------
#
# GCC computes the location of its own internals — `default-manifest.o`, its
# library search directories — from where `gcc.exe` itself lives, and hands them
# to `ld.exe` through spec strings that are concatenated WITHOUT quoting. A space
# anywhere in that path therefore reaches ld split in two, and every link fails:
#     ld.exe: cannot find C:/Program: No such file or directory
# The paths rustc passes are quoted and survive intact; only the toolchain's own
# location matters. So the project may sit under `C:\Program Files\ManhwaStudio`
# — where the Windows installer puts it — as long as gcc is *reached* through a
# path with no space in it. That is what the helpers below arrange, in order of
# preference: the path as it is, its DOS 8.3 short form, a `subst` drive.

<#
.SYNOPSIS
Returns the DOS 8.3 short form of an existing directory, or $null.

`$Path` must name a directory that exists. Returns $null when it does not, when
8.3 name generation is disabled on the volume, or when the COM object is
unavailable (any non-Windows host) — never throws, because every caller treats
"no short form" as one more strategy that did not work.
#>
function Get-ShortPathName {
    param([string] $Path)
    if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path -LiteralPath $Path)) { return $null }
    try {
        $fso   = New-Object -ComObject Scripting.FileSystemObject
        $short = $fso.GetFolder($Path).ShortPath
    } catch {
        return $null
    }
    if ([string]::IsNullOrWhiteSpace($short)) { return $null }
    return $short
}

<#
.SYNOPSIS
Parses the output of `subst` into a hashtable of drive letter -> target path.

Pure on purpose — the caller supplies the lines — so the parser is testable off
Windows. `subst` prints one mapping per line as `Z:\: => C:\some\dir`; anything
that does not match that shape is ignored rather than becoming data. Letters are
keyed upper-case without the colon.
#>
function Get-SubstMap {
    param([string[]] $Lines)
    $map = @{}
    foreach ($line in @($Lines)) {
        if ($line -match '^\s*([A-Za-z]):\\?:\s*=>\s*(.+?)\s*$') {
            $map[$matches[1].ToUpperInvariant()] = $matches[2]
        }
    }
    return $map
}

<#
.SYNOPSIS
Returns every drive letter from Z down to E that is currently unused, as `X:`.

A-D are skipped: they are conventionally floppy/removable/system and a `subst`
there confuses more than it helps. Returns a plain array — wrap the call in
`@(...)`, per the module's array-return convention.
#>
function Get-FreeDriveLetters {
    $free = @()
    foreach ($code in 90..69) {          # Z .. E
        $letter = [char] $code
        if (-not (Test-Path -LiteralPath "${letter}:\")) { $free += "${letter}:" }
    }
    return $free
}

<#
.SYNOPSIS
Returns the existing `subst` drive already mapped to `$Path` (as `X:`), or $null.

Reusing a mapping matters because a run killed mid-flight leaves its drive
behind: without this the letters would accumulate one per crashed run. A mapping
found this way was not created by us, so it is left in place on exit.
#>
function Get-ExistingSubstDrive {
    param([string] $Path)
    try {
        $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
        $out  = @(& subst 2>$null)
        $ErrorActionPreference = $prev
    } catch {
        return $null
    }
    $map = Get-SubstMap -Lines $out
    foreach ($letter in $map.Keys) {
        if ($map[$letter].TrimEnd('\') -eq $Path.TrimEnd('\')) { return "${letter}:" }
    }
    return $null
}

<#
.SYNOPSIS
Maps `$Path` to a free drive letter with `subst` and returns it as `X:`, or $null.

Needs no privileges, copies nothing, and lasts until the mapping is removed or
the machine reboots. Letters are tried in turn because a free one can be taken
between the check and the call.
#>
function New-SubstDrive {
    param([string] $Path)
    foreach ($drive in @(Get-FreeDriveLetters)) {
        try {
            $prev = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
            & subst $drive $Path 2>$null | Out-Null
            $rc = $LASTEXITCODE
            $ErrorActionPreference = $prev
        } catch {
            return $null   # subst.exe itself is missing; trying more letters is pointless
        }
        if ($rc -eq 0 -and (Test-Path -LiteralPath "$drive\")) { return $drive }
    }
    return $null
}

<#
.SYNOPSIS
Removes a `subst` mapping created by this run. Failures are deliberately ignored:
the mapping is a convenience and disappears on reboot regardless.
#>
function Remove-SubstDrive {
    param([string] $Drive)
    if ([string]::IsNullOrWhiteSpace($Drive)) { return }
    try { & subst $Drive /D 2>$null | Out-Null } catch { }
}

<#
.SYNOPSIS
Returns a path that reaches `$Dir` without a space in it, or $null when none can
be arranged.

Strategies in order: the path as given, its 8.3 short form, an existing `subst`
drive, a new one. A drive created here is recorded in `$script:SubstDrive` so
`Invoke-Main` can release it; one that already existed is reused and left alone.
#>
function Resolve-SpaceFreePath {
    param([string] $Dir)
    if ($Dir -notmatch ' ') { return $Dir }

    $short = Get-ShortPathName -Path $Dir
    if ($short -and $short -notmatch ' ') { return $short }

    $existing = Get-ExistingSubstDrive -Path $Dir
    if ($existing) { return "$existing\" }

    $created = New-SubstDrive -Path $Dir
    if ($created) {
        $script:SubstDrive = $created
        return "$created\"
    }
    return $null
}

<#
.SYNOPSIS
Returns the lines explaining that no space-free path to the toolchain exists and
what the user can do about it. One text, two call sites (the early check and the
apply step), so the two can never drift apart.
#>
function Get-SpaceFreePathHelp {
    return @(
        'Проект лежит по пути с пробелом, и обойти это не удалось.',
        "Каталог проекта: $script:RepoRoot",
        '',
        'Компилятор MinGW-w64 подставляет пути к своим внутренним файлам',
        'в командную строку компоновщика без кавычек. Пробел разрывает такой',
        'путь надвое, и сборка падает на первом же связывании:',
        '    ld.exe: cannot find C:/Program: No such file or directory',
        '',
        'Обычно это обходится короткими именами 8.3 или подстановкой диска,',
        'но на этой машине недоступно ни то, ни другое.',
        '',
        'Что можно сделать:',
        '  1) включить короткие имена и перезагрузиться (нужны права',
        '     администратора):  fsutil 8dot3name set 0',
        '  2) либо перенести проект в путь без пробелов, например',
        '     C:\ManhwaStudio. Перед переносом удалите папки target и',
        '     installer_files\mingw64 — в них записаны абсолютные пути.')
}

<#
.SYNOPSIS
Fails with exit code 5 when the toolchain could not be reached without a space,
BEFORE the 261 MB download rather than after it plus a build that cannot link.

The MinGW directory does not exist yet at this point, so the question is asked of
the repository root, which does: 8.3 applies per path component, so a space-free
root can only produce a space-free child. Returns normally when a space is not a
problem at all.
#>
function Assert-SpaceFreeToolchainPath {
    $dir = Get-MingwDir
    if ($dir -notmatch ' ') { return }

    $short = Get-ShortPathName -Path $script:RepoRoot
    if ($short -and $short -notmatch ' ') { return }
    if (Get-ExistingSubstDrive -Path $dir) { return }
    if (@(Get-FreeDriveLetters).Count -gt 0) { return }

    Die $ExitNoCc @(Get-SpaceFreePathHelp)
}

<#
.SYNOPSIS
Points the build at the MinGW toolchain in `$Dir` through a space-free path.

Prepends its `bin\` to the process PATH and pins the linker, C compiler and
archiver by absolute path. Pinning is the load-bearing half: rustc invokes the
linker by bare name, leaving gcc to recover its own location from however the
PATH lookup resolved — an absolute path removes that dependency. The compiler
variables are read by the `cc` crate, which is what builds `aws-lc-sys`. All of
it is process-local; nothing is written to the user's environment.

Terminates with exit code 5 when no space-free path can be arranged.
#>
function Set-MingwEnvironment {
    param([string] $Dir)

    $base = Resolve-SpaceFreePath -Dir $Dir
    if (-not $base) { Die $ExitNoCc @(Get-SpaceFreePathHelp) }

    $bin = Join-Path $base 'bin'
    # Split rather than -like: a path is a literal here, and -like would read
    # any [ ] in it as a wildcard set.
    if (@($env:PATH -split ';') -notcontains $bin) { $env:PATH = "$bin;$($env:PATH)" }

    $gcc = Join-Path $bin 'x86_64-w64-mingw32-gcc.exe'
    $ar  = Join-Path $bin 'x86_64-w64-mingw32-ar.exe'
    if (Test-Path -LiteralPath $gcc) {
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = $gcc
        $env:CC_x86_64_pc_windows_gnu                  = $gcc
    }
    if (Test-Path -LiteralPath $ar) { $env:AR_x86_64_pc_windows_gnu = $ar }
}

<#
.SYNOPSIS
Provisions portable MinGW-w64 (WinLibs) into installer_files\mingw64 and prepends
its bin\ to the PATH of this process only. Windows is the one platform with no
one-line system command for a C compiler, and this toolchain is a plain zip that
needs no privileges.
#>
function Install-Mingw {
    if ($Offline) {
        Die $ExitNoCc @(
            'Не найден компилятор C, а режим -Offline запрещает скачивание.',
            'Установите MinGW-w64 вручную: https://winlibs.com/')
    }

    # Before the 261 MB download, not after it: a toolchain that cannot be reached
    # without a space in its path links nothing, and the user would find that out
    # only at the end of the first build.
    Assert-SpaceFreeToolchainPath

    Step 'Компилятор C не найден — скачиваю портативный MinGW-w64'
    Info 'Он нужен зависимости aws-lc-sys (шифрование в сетевых запросах),'
    Info 'которая собирает исходники на C и ассемблере.'
    Info 'Устанавливается только внутрь installer_files\mingw64.'

    $asset = Resolve-GithubAsset -ApiUrl $GitApiMingw -Pattern '^winlibs-x86_64-.*mingw-w64.*\.zip$'
    if (-not $asset) {
        Die $ExitNoCc @(
            'Не удалось узнать адрес загрузки MinGW-w64 с GitHub.',
            'Проверьте интернет-соединение, либо установите его вручную:',
            '    https://winlibs.com/',
            'и добавьте папку bin в PATH.')
    }

    $zip  = Join-Path (Get-DownloadDir) 'winlibs.zip'
    $tmp  = Join-Path $script:RepoRoot 'installer_files\_mingw_tmp'
    $dest = Get-MingwDir
    try {
        Info "Загрузка: $($asset.Name)"
        Info "Размер: $(Format-Bytes $asset.Size) — это надолго, окно не зависло."
        Get-RemoteFile -Url $asset.Url -Destination $zip `
                       -ExpectedSize $asset.Size -Sha256Url $asset.Sha256Url
    } catch {
        Die $ExitNoCc @(
            'Не удалось скачать MinGW-w64.',
            "Причина: $($_.Exception.Message)",
            '',
            'Запустите run-dev ещё раз: загрузка продолжится с того места,',
            'на котором оборвалась, а не начнётся заново.',
            'Если связь рвётся постоянно — скачайте архив вручную:',
            '    https://winlibs.com/',
            'и распакуйте его в installer_files\mingw64.')
    }
    try {
        Expand-ZipTo -ZipPath $zip -Destination $tmp
    } catch {
        Remove-Item -Force $zip -ErrorAction SilentlyContinue
        Die $ExitNoCc @(
            'Архив MinGW-w64 скачался, но распаковать его не удалось.',
            "Причина: $($_.Exception.Message)",
            'Чаще всего это нехватка места на диске: архив весит',
            "$(Format-Bytes $asset.Size), а распакованный тулчейн — заметно больше.",
            'Освободите место и запустите run-dev снова.')
    } finally {
        if (Test-Path $zip) { Remove-Item -Force $zip -ErrorAction SilentlyContinue }
    }

    # The archive wraps everything in a single top-level folder (`mingw64`), but
    # its exact name has changed across releases — locate gcc instead of assuming.
    $gcc = Get-ChildItem -Path $tmp -Filter 'gcc.exe' -Recurse -File -ErrorAction SilentlyContinue |
           Select-Object -First 1
    if (-not $gcc) {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
        Die $ExitNoCc @('В скачанном архиве MinGW-w64 не найден gcc.exe.')
    }
    $root = Split-Path (Split-Path $gcc.FullName -Parent) -Parent
    if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
    Move-Item -Path $root -Destination $dest
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue

    Set-MingwEnvironment -Dir $dest
    Ok 'MinGW-w64 установлен.'
}

<#
.SYNOPSIS
Ensures a C compiler matching the active toolchain's host exists. aws-lc-sys
compiles C and assembly on every native target, so this turns a wall of linker
errors 200 crates into the build into one clear message before it.
#>
function Assert-CToolchain {
    $mingwDir = Get-MingwDir
    if (Test-Path (Join-Path $mingwDir 'bin\gcc.exe')) {
        Set-MingwEnvironment -Dir $mingwDir
        return
    }
    if (Get-Command gcc -ErrorAction SilentlyContinue) { return }

    # A system Rust normally has the MSVC host, which uses cl.exe rather than gcc.
    if (-not $script:ManagedRust) {
        if (Get-Command cl -ErrorAction SilentlyContinue) { return }
        Die $ExitNoCc @(
            'Не найден компилятор C — без него проект не соберётся.',
            'Он нужен зависимости aws-lc-sys (шифрование в сетевых запросах),',
            'которая собирает исходники на C и ассемблере.',
            '',
            'Вариант 1 — установить средства сборки Microsoft:',
            '    winget install Microsoft.VisualStudio.2022.BuildTools',
            '    (в установщике выберите "Разработка классических приложений на C++")',
            '',
            'Вариант 2 — дать скрипту установить всё изолированно:',
            '    удалите Rust из системы или временно уберите его из PATH,',
            '    и запустите run-dev снова — он поставит свой Rust и MinGW',
            '    внутрь installer_files, ничего не трогая в системе.')
    }

    Install-Mingw
}

function Invoke-RustStage {
    Step 'Проверка Rust'
    $msrv = Get-RequiredMsrv
    Info "Проекту требуется Rust $msrv или новее."

    # 1) A previously provisioned managed toolchain wins.
    if (Test-Path (Get-ManagedCargo)) {
        Use-ManagedRust
        $mv = Get-RustcVersion $script:Cargo
        if (Test-VersionAtLeast $mv $msrv) {
            Ok "Используется установленный проектом Rust $mv"
            Assert-CToolchain
            return
        }
        Info "Установленный проектом Rust $mv устарел, обновляю."
        $rustup = Join-Path (Get-RustRoot) 'cargo\bin\rustup.exe'
        if (Test-Path $rustup) {
            $p = Start-Process -FilePath $rustup -ArgumentList @('update', 'stable') `
                               -Wait -NoNewWindow -PassThru
            if ($p.ExitCode -eq 0) {
                $mv = Get-RustcVersion $script:Cargo
                if (Test-VersionAtLeast $mv $msrv) {
                    Ok "Rust обновлён до $mv"
                    Assert-CToolchain
                    return
                }
            }
        }
        Warn 'Обновить не удалось, переустанавливаю.'
        Install-ManagedRust
        Assert-CToolchain
        return
    }

    # 2) A current system toolchain is used as-is — no second copy downloaded.
    $sysCargo = Get-Command cargo -ErrorAction SilentlyContinue
    if ($sysCargo) {
        $sv = Get-RustcVersion $sysCargo.Source
        if (Test-VersionAtLeast $sv $msrv) {
            $script:Cargo = $sysCargo.Source
            Ok "Используется системный Rust $sv"
            Assert-CToolchain
            return
        }
        Info "Системный Rust $sv не подходит (нужен $msrv+)."
    } else {
        Info 'Rust в системе не найден.'
    }

    # 3) Provision.
    Install-ManagedRust
    Assert-CToolchain
}

# =============================================================================
# Stage 3 — run
# =============================================================================

<#
.SYNOPSIS
Runs `cargo run --bin manhwastudio_rs [--release] -- <ApplicationArgs>` and
records cargo's exit code in $script:LastRunCode.

The exit code deliberately travels through script state rather than a return
value: cargo's own output is this function's output stream, and returning a value
would make the caller capture the build log along with it. Synchronous by
contract — on Windows a running .exe cannot be relinked, so the two Stage 3
phases must never overlap.
#>
function Invoke-CargoRun {
    param([string[]] $ApplicationArgs)
    $cargoArgs = @('run', '--bin', $AppBin)
    if (-not $DebugBuild) { $cargoArgs += '--release' }
    $cargoArgs += '--'
    $cargoArgs += $ApplicationArgs

    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $script:Cargo @cargoArgs
    $script:LastRunCode = $LASTEXITCODE
    $ErrorActionPreference = $prev
}

<#
.SYNOPSIS
Phase 1 of Stage 3: let the binary itself decide whether the Python environment
is usable. `--check-venv` opens the installer *only* when something is missing
and exits 0 without any GUI when everything is in place, so this is also what
compiles the project — Publish-RootBinary then only has to ask cargo where the
result is. Never returns when the environment could not be prepared.
#>
function Assert-AppEnvironment {
    if ($Offline) {
        # -Offline is an explicit "do not touch the network" request, and this
        # check may download uv, Python or Torch wheels. Skipping it is the only
        # honest reading; the app reports a broken environment on its own later.
        Warn 'Режим -Offline: проверка окружения Python пропущена.'
        Info 'Если venv или пакетов не хватает, приложение сообщит об этом само.'
        return
    }

    Step 'Проверка окружения приложения'
    Info 'Сейчас проект собирается — на чистой машине это самая долгая часть.'
    Info 'Окно установки откроется, только если чего-то не хватает.'

    Say ''
    Invoke-CargoRun -ApplicationArgs @('--check-venv', '--ignore-installed')
    if ($script:LastRunCode -ne 0) {
        Die $ExitVenvNotReady @(
            "Окружение приложения не готово, запуск отменён (код $($script:LastRunCode)).",
            '',
            'Причина — одна из двух:',
            '  1) проект не собрался. Тогда выше видны сообщения компилятора,',
            '     и разбирать нужно именно их: другими флагами это не обходится.',
            '  2) установка окружения была отменена или завершилась с ошибкой.',
            '     Запустите run-dev снова и доведите установку до конца.',
            '',
            'Во втором случае проверку можно и пропустить: режим -Offline её не',
            'выполняет. Но учтите, что он же отключает обновление из git и',
            'установку Rust — при отсутствующем Rust запуск завершится ошибкой.')
    }
    Ok 'Окружение готово.'
}

# --- Publishing the built binary into the project root ------------------------
#
# The point is that the user can start the application afterwards WITHOUT this
# script and without any git update: `manhwastudio_rs.exe` in the project root is
# a complete launch. Running it from there is safe — config::resolve_runtime_root
# finds the project root both from the current directory and from the
# executable's own directory, and no DLL is loaded from beside the executable.
# The whole step is a convenience: every failure degrades to the old `cargo run`
# launch and no new exit code exists for it.

<#
.SYNOPSIS
Returns the path of the executable that cargo built for bin target $TargetName,
taken from a `--message-format=json` stream, or '' when the stream names none.

.DESCRIPTION
$Json is cargo's stdout, one JSON object per line. The LAST `compiler-artifact`
message whose `target.name` equals $TargetName and whose `executable` is non-null
wins: a stream can carry several artifacts for one target, and the final one
describes what is on disk now. Filtering by target name is not optional — the
package declares several bin targets besides this one, and `src/bin/` adds more
automatically — and the non-null test is what skips library artifacts, which
report `"executable":null`.

Pure: reads nothing, writes nothing, and is safe to call on arbitrary text. Lines
that are not JSON objects are skipped rather than throwing, because cargo is free
to put plain text on stdout, and every field is probed before it is read: under
Set-StrictMode -Version Latest reading an absent property is a terminating error,
and most cargo messages (build-script-executed, build-finished, compiler-message)
carry no `target` or `executable` at all.
#>
function Get-BuiltBinaryFromJson {
    param([string] $Json, [string] $TargetName)
    if ([string]::IsNullOrWhiteSpace($Json)) { return '' }
    $found = ''
    foreach ($line in ($Json -split "`r?`n")) {
        $text = $line.Trim()
        if (-not $text.StartsWith('{')) { continue }
        $msg = $null
        try { $msg = ConvertFrom-Json -InputObject $text } catch { continue }
        if ($null -eq $msg) { continue }

        $fields = @($msg.PSObject.Properties.Name)
        if ($fields -notcontains 'reason') { continue }
        # -cne: the project compares machine-readable tokens case-sensitively.
        if ("$($msg.reason)" -cne 'compiler-artifact') { continue }
        if ($fields -notcontains 'executable' -or $fields -notcontains 'target') { continue }

        # A lib artifact reports "executable":null; only a bin has a path here.
        $exe = "$($msg.executable)"
        if ([string]::IsNullOrWhiteSpace($exe)) { continue }

        $target = $msg.target
        if ($null -eq $target) { continue }
        if (@($target.PSObject.Properties.Name) -notcontains 'name') { continue }
        if ("$($target.name)" -cne $TargetName) { continue }

        $found = $exe
    }
    return $found
}

<#
.SYNOPSIS
Asks cargo where the binary of $AppBin is, by building it and reading the path
out of the JSON message stream. Returns '' when cargo failed or named no
executable; never throws for that reason and never guesses a `target\...` path.

.DESCRIPTION
Normally free: phase 1 has already built the project, so this call is cache-warm
and only prints the JSON. Under -Offline phase 1 is skipped and THIS call performs
the real build.

`json-render-diagnostics` rather than plain `json`: it keeps the human-readable
compiler errors going to stderr, which plain `json` would swallow into the
machine-readable stream and hide from the user. stderr is deliberately NOT merged
into the captured output — cargo's progress and diagnostics must stay on the
terminal, and merging streams into parsed data is the exact bug class the
Invoke-Git contract exists to prevent.
#>
function Resolve-BuiltBinary {
    $cargoArgs = @('build', '--bin', $AppBin)
    if (-not $DebugBuild) { $cargoArgs += '--release' }
    $cargoArgs += '--message-format=json-render-diagnostics'

    $prev = $ErrorActionPreference
    # A native command that is allowed to fail: under 'Stop' anything cargo writes
    # to stderr becomes a terminating error. Same treatment as Invoke-CargoRun.
    $ErrorActionPreference = 'Continue'
    $captured = & $script:Cargo @cargoArgs
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev

    if ($code -ne 0) {
        Warn "cargo build завершился с кодом $code."
        return ''
    }
    $text = ((@($captured) | ForEach-Object { [string]$_ }) -join "`n")
    return (Get-BuiltBinaryFromJson -Json $text -TargetName $AppBin)
}

<#
.SYNOPSIS
True when $Destination has to be (re)written from $Source: it is missing, or its
size or last-write time differ. $Source must exist.

.DESCRIPTION
Exact timestamp equality is the right test, not "older than": Copy-Item preserves
LastWriteTime, so a destination produced by Install-RootBinary from this very
source carries the source's stamp byte for byte. Comparing UTC avoids the DST
shifts a local-time comparison would invent twice a year.
#>
function Test-RootBinaryNeedsCopy {
    param([string] $Source, [string] $Destination)
    if (-not (Test-Path -LiteralPath $Destination)) { return $true }
    $src = Get-Item -LiteralPath $Source
    $dst = Get-Item -LiteralPath $Destination
    if ($src.Length -ne $dst.Length) { return $true }
    return ($src.LastWriteTimeUtc -ne $dst.LastWriteTimeUtc)
}

<#
.SYNOPSIS
Copies $Source onto $Destination atomically. Throws with a Russian message when
the destination could not be replaced; the caller treats that as non-fatal.

.DESCRIPTION
Same `.part` principle as Get-RemoteFile: the bytes go to `<Destination>.part`
and are renamed into place only once they are all there, so $Destination never
holds a half-written executable.

The real hazard is Windows-specific: $Destination may be a *running* executable
left over from a previous session, and Windows refuses to overwrite or delete
one. Renaming it, however, is allowed even while it runs — that is the standard
updater trick — so the move is retried after the old file is renamed aside.

The aside name carries a random suffix instead of being a fixed `<Destination>.old`:
an aside that is still locked by a live process cannot be deleted, and a fixed name
would then block every future publish and make the error message blame that
leftover rather than the file actually in the way. Deleting the aside is
best-effort; Clear-RootBinaryLeftovers sweeps whatever survived on a later run.
#>
function Install-RootBinary {
    param([string] $Source, [string] $Destination)
    $part = "$Destination.part"
    $old  = "$Destination.old-" + [guid]::NewGuid().ToString('N').Substring(0, 8)

    Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $Source -Destination $part -Force

    $firstError = ''
    try {
        Move-Item -LiteralPath $part -Destination $Destination -Force
        return
    } catch {
        $firstError = $_.Exception.Message
    }

    # The destination is locked — almost always a copy of the app still running.
    try {
        Move-Item -LiteralPath $Destination -Destination $old -Force
    } catch {
        Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
        throw "файл занят и не переименовывается: $firstError"
    }

    try {
        Move-Item -LiteralPath $part -Destination $Destination -Force
    } catch {
        $moveError = $_.Exception.Message
        Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
        # Put the previous binary back, so a failure leaves the root as it was.
        # When even that fails the root holds NO binary at all — the one outcome
        # that must never hide behind the original error message.
        $restored = $false
        try {
            Move-Item -LiteralPath $old -Destination $Destination -Force
            $restored = $true
        } catch { }
        if (-not $restored) {
            throw "$moveError. Вернуть прежний файл не удалось: в корне проекта сейчас нет программы, она лежит рядом под именем $old"
        }
        throw $moveError
    }
    Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
}

<#
.SYNOPSIS
Removes what a previous publish may have left next to $Destination:
`<Destination>.part` and every `<Destination>.old-*` aside. Best-effort and
silent — an aside still locked by a running copy of the application is normal,
and the next run tries again.

.DESCRIPTION
Called unconditionally, not only when a copy happens: Install-RootBinary is
skipped whenever the root binary is already identical, so a leftover from an
interrupted or blocked run would otherwise survive forever.
#>
function Clear-RootBinaryLeftovers {
    param([string] $Destination)
    Remove-Item -LiteralPath "$Destination.part" -Force -ErrorAction SilentlyContinue
    $dir  = Split-Path -Parent $Destination
    $leaf = Split-Path -Leaf   $Destination
    if (-not $dir -or -not $leaf) { return }
    # -LiteralPath: a project path containing [ ] would be read as a wildcard by -Path.
    $asides = @(Get-ChildItem -LiteralPath $dir -Filter "$leaf.old-*" -File -ErrorAction SilentlyContinue)
    foreach ($aside in $asides) {
        Remove-Item -LiteralPath $aside.FullName -Force -ErrorAction SilentlyContinue
    }
}

<#
.SYNOPSIS
Puts the freshly built binary into the project root and records its path in
$script:RootExe, so phase 2 can launch it directly. Sets $script:RootExe to ''
and warns — never dies — when anything about that fails.

.DESCRIPTION
Deliberately non-fatal and without an exit code of its own: publishing the binary
is a convenience, running the application is the point of the script. Everything
this step can fail at (cargo not naming the path, a locked destination, a full
disk) leaves phase 2 working exactly as it did before, through Invoke-CargoRun.
#>
function Publish-RootBinary {
    $script:RootExe = ''

    Step 'Готовлю запускаемый файл в корне проекта'
    if ($Offline) {
        Info 'Режим -Offline: проверка окружения пропущена, поэтому проект собирается сейчас.'
        Info 'На чистой машине это самая долгая часть, окно не зависло.'
    } else {
        Info 'Проект уже собран на предыдущем шаге — это займёт мгновение.'
    }
    Say ''

    $built = ''
    try {
        $built = Resolve-BuiltBinary
    } catch {
        Warn "Не удалось узнать путь к собранному файлу: $($_.Exception.Message)"
        Info 'Запуск продолжится обычным способом, через cargo run.'
        return
    }
    if (-not $built) {
        Warn 'Cargo не сообщил путь к собранному файлу, копирование в корень пропущено.'
        Info 'Запуск продолжится обычным способом, через cargo run.'
        return
    }
    if (-not (Test-Path -LiteralPath $built)) {
        Warn "Собранный файл не найден: $built"
        Info 'Запуск продолжится обычным способом, через cargo run.'
        return
    }

    # The name is taken from the built file, not hard-coded: it carries the .exe
    # suffix on Windows and would not on any other host.
    $dest = Join-Path $script:RepoRoot (Split-Path -Leaf $built)
    # Unconditional, before the copy is even decided: the copy below is skipped
    # whenever the root binary is already current, and its own cleanup with it.
    Clear-RootBinaryLeftovers -Destination $dest

    try {
        if (Test-RootBinaryNeedsCopy -Source $built -Destination $dest) {
            Install-RootBinary -Source $built -Destination $dest
            Ok "Программа скопирована в корень проекта: $dest"
            Info 'Дальше её можно запускать напрямую — без run-dev и без обновления.'
        } else {
            Ok "В корне проекта уже лежит эта же сборка: $dest"
        }
    } catch {
        Warn "Не удалось скопировать файл в корень проекта: $($_.Exception.Message)"
        Info 'Запуск продолжится обычным способом, через cargo run.'
        return
    }

    $script:RootExe = $dest
}

<#
.SYNOPSIS
Runs the published root binary with $ApplicationArgs, WAITS for it, and records
its exit code in $script:LastRunCode. Requires $script:RootExe to be set. Records
126 — the POSIX "found but could not be executed" code, which run-dev.sh yields on
this path — when the file cannot be started at all.

.DESCRIPTION
The counterpart of Invoke-CargoRun for the published binary, and it keeps that
function's two contracts: the exit code travels through script state because this
function's output stream belongs to the application, and the call is synchronous.
The working directory is the project root — Invoke-Main sets it and nothing moves
it afterwards — which is what the application's own root resolution expects.
#>
function Invoke-RootBinary {
    param([string[]] $ApplicationArgs)
    $prev = $ErrorActionPreference
    # A native command that is allowed to fail; same treatment as Invoke-CargoRun.
    $ErrorActionPreference = 'Continue'
    try {
        # `| Out-Host` is load-bearing. Do not "simplify" it away.
        #
        # On Windows the application is a GUI-subsystem binary: src/main.rs applies
        # windows_subsystem = "windows" unless the active-logs feature is on, and
        # run-dev passes no --features. PowerShell's call operator does NOT wait for
        # a GUI-subsystem process — it starts it and returns immediately. Without a
        # pipeline this function would therefore return at once, $LASTEXITCODE would
        # still hold the code of the PREVIOUS command (cargo build's 0), the launcher
        # window would close while the application was still starting, and a startup
        # crash would show the user nothing at all. `cargo run` never had this
        # problem: cargo.exe is a console application, so PowerShell waited for it.
        #
        # A pipeline forces PowerShell to redirect the process's stdout and read it
        # to EOF, which cannot happen before the process exits. That makes the call
        # synchronous again, and $LASTEXITCODE — read on the next line, with no other
        # command in between — is then the application's own exit code.
        #
        # Start-Process -Wait -PassThru would also wait and is this file's idiom
        # elsewhere (Install-ManagedRust), but Windows PowerShell 5.1 joins its
        # -ArgumentList into one string WITHOUT quoting, so every argument containing
        # a space would arrive split. dev-docs/run_dev_plan.md documents argument
        # fidelity as a property of the Windows side, so the array is splatted through
        # the call operator instead, exactly as Invoke-CargoRun does.
        #
        # Out-Host rather than Out-Null: the application's own output stays visible.
        & $script:RootExe @ApplicationArgs | Out-Host
        $script:LastRunCode = $LASTEXITCODE
    } catch {
        # A file that cannot be started at all (missing, quarantined by antivirus,
        # not an executable) throws instead of returning a code. Untreated it would
        # end the run with a raw PowerShell stack trace, outside the exit codes in
        # dev-docs/run_dev_plan.md.
        Warn "Не удалось запустить программу из корня проекта: $($_.Exception.Message)"
        Info "Файл: $script:RootExe"
        Info 'Удалите его и запустите run-dev снова — он соберёт и положит новый.'
        $script:LastRunCode = 126
    } finally {
        $ErrorActionPreference = $prev
    }
}

<#
.SYNOPSIS
Stage 3: check the environment (which also builds), publish the built binary into
the project root, then run the application. Never returns — it exits with the
application's own exit code.
#>
function Invoke-RunStage {
    Step 'Сборка и запуск'
    if ($DebugBuild) {
        Info 'Сборка в режиме debug (-DebugBuild).'
    } else {
        Info 'Сборка в режиме release. Первый запуск на чистой машине компилирует'
        Info 'весь проект целиком — это может занять 10-30 минут. Это не зависание.'
    }

    # build.rs starts a codesign worker for Windows targets and PROMPTS on the
    # terminal for a .p12 password when .secret\build_config.json is absent.
    # Signing is a release concern; a dev run must never stop on that prompt.
    # An explicitly set value is respected, so a signed dev build is still possible.
    if (-not $env:MS_DISABLE_BUILD_CODESIGN) { $env:MS_DISABLE_BUILD_CODESIGN = '1' }

    Assert-AppEnvironment

    # Between the check and the run: put the built binary in the project root, so
    # the user can start the application later without this script. Sets
    # $script:RootExe on success and only warns on failure.
    Publish-RootBinary

    # Phase 2. `--ignore-installed` goes first: the environment was just checked,
    # so the application must not repeat that check at startup. It is also what
    # keeps the self-updater from overwriting the root binary in place, so it is
    # passed on both launch paths.
    $appArguments = @('--ignore-installed')
    if ($AppArgs -and $AppArgs.Count -gt 0) { $appArguments += $AppArgs }

    Say ''
    if ($script:RootExe) {
        Invoke-RootBinary -ApplicationArgs $appArguments
    } else {
        Invoke-CargoRun -ApplicationArgs $appArguments
    }
    $code = $script:LastRunCode

    if ($code -ne 0) {
        Warn "Приложение завершилось с кодом $code."
        if (-not $Yes) { Wait-ForKey }
    }
    exit $code
}

# =============================================================================

function Invoke-Main {
    if ($Help) { Show-Usage; exit 0 }
    if ($DiscardLocal -and $KeepLocal) {
        Die $ExitGeneric @('-DiscardLocal и -KeepLocal взаимоисключающие.')
    }

    try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch { }

    # The repo root is two levels above this script (tools\run-dev\).
    $scriptDir = Split-Path -Parent $PSCommandPath
    $script:RepoRoot = (Resolve-Path (Join-Path $scriptDir '..\..')).Path
    Set-Location $script:RepoRoot

    if (-not (Test-Path (Join-Path $script:RepoRoot 'Cargo.toml'))) {
        Die $ExitGeneric @(
            "В каталоге $script:RepoRoot нет Cargo.toml.",
            'Скрипт должен лежать в tools\run-dev\ внутри проекта ManhwaStudio.')
    }

    Write-Host 'ManhwaStudio — запуск dev-версии'
    Info "Каталог проекта: $script:RepoRoot"

    if ($NoUpdate) {
        Info 'Обновление пропущено (-NoUpdate).'
    } else {
        Invoke-GitStage
        # Must come before Stage 2/3: continuing with half-old scripts is exactly
        # what this check exists to prevent.
        Assert-NoSelfUpdate
    }
    try {
        Invoke-RustStage
        Invoke-RunStage
    } finally {
        # `exit` inside the stages is a terminating condition, so this still runs:
        # a subst drive left mapped would hold its letter until the next reboot.
        if ($script:SubstDrive) {
            Remove-SubstDrive -Drive $script:SubstDrive
            $script:SubstDrive = ''
        }
    }
}

# `test_run_dev.ps1` dot-sources this file to exercise the git stage in isolation
# against a throwaway repository; sourcing must not run the app.
if (-not $env:MS_RUN_DEV_SOURCE_ONLY) {
    Invoke-Main
}
