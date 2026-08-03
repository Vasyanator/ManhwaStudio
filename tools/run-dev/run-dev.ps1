<#
File: tools/run-dev/run-dev.ps1

Purpose:
Windows core of `run-dev`: bring the working copy up to date with origin,
provision Git / a Rust toolchain / a C toolchain when the machine has none, then
run the app with `cargo run --bin manhwastudio_rs --release`.

Main responsibilities:
- Stage 1 (git): provision portable MinGit when git is absent, adopt a working
  copy unpacked from a source ZIP, fetch, and merge local changes automatically
  when they do not truly conflict.
- Stage 2 (rust): read the MSRV from Cargo.toml, pick a system or managed
  toolchain, provision an isolated one plus MinGW-w64 under installer_files/.
- Stage 3: invoke cargo.

Key functions:
- Invoke-GitStage, Install-MinGit, Invoke-RepositoryAdoption, Update-WithLocalChanges
- Invoke-RustStage, Install-ManagedRust, Install-Mingw, Assert-CToolchain

Notes:
The algorithm, its rationale, and every failure path are specified in
`dev-docs/run_dev_plan.md`. Linux/macOS are a separate implementation
(run-dev.sh); the git algorithm is intentionally identical in both.
Entered through run-dev.Windows.bat, which bypasses the execution policy.
This file MUST stay UTF-8 with BOM: Windows PowerShell 5.1 reads a BOM-less
script as the system ANSI code page and would mangle every Russian message.
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

# Exit codes; see dev-docs/run_dev_plan.md.
$ExitGeneric     = 1
$ExitNoGit       = 2
$ExitManualMerge = 3
$ExitNoRust      = 4
$ExitNoCc        = 5
$ExitAborted     = 6

# --- State -------------------------------------------------------------------

$script:RepoRoot    = $null
$script:Git         = $null
$script:Cargo       = $null
$script:Adopted     = $false
$script:ManagedRust = $false

# --- Output helpers ----------------------------------------------------------

function Say  { param([string] $Text = '') Write-Host $Text }
function Step { param([string] $Text) Write-Host ''; Write-Host "==> $Text" -ForegroundColor Cyan }
function Ok   { param([string] $Text) Write-Host "  ok " -ForegroundColor Green -NoNewline; Write-Host $Text }
function Warn { param([string] $Text) Write-Host "  ! $Text" -ForegroundColor Yellow }
function Info { param([string] $Text) Write-Host "    $Text" }

<#
.SYNOPSIS
Prints a framed error block and terminates the process with the given code.
Never returns.
#>
function Die {
    param([int] $Code, [string[]] $Lines)
    Write-Host ''
    Write-Host '============================================================' -ForegroundColor Red
    Write-Host '  ОШИБКА' -ForegroundColor Red
    Write-Host '============================================================' -ForegroundColor Red
    foreach ($l in $Lines) { Write-Host "  $l" }
    Write-Host ''
    if (-not $Yes) { Wait-ForKey }
    exit $Code
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
  -Offline        Не обращаться к сети вообще.
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
Parses a git `--count` output into an int, yielding 0 for empty or non-numeric
text. Guards against `[int]''` throwing when a git command failed.
#>
function Convert-ToCount {
    param([string] $Text)
    if ([string]::IsNullOrWhiteSpace($Text)) { return 0 }
    $m = [regex]::Match($Text, '\d+')
    if (-not $m.Success) { return 0 }
    return [int]$m.Value
}

# --- Download / archive helpers ----------------------------------------------

function Get-DownloadDir {
    $d = Join-Path $script:RepoRoot 'installer_files\downloads'
    if (-not (Test-Path $d)) { [void](New-Item -ItemType Directory -Path $d -Force) }
    return $d
}

<#
.SYNOPSIS
Downloads $Url to $Destination over TLS 1.2+. Throws on failure.
#>
function Get-RemoteFile {
    param([string] $Url, [string] $Destination)
    # Windows PowerShell 5.1 still defaults to TLS 1.0 on older builds, which
    # github.com and static.rust-lang.org both refuse.
    try {
        [Net.ServicePointManager]::SecurityProtocol =
            [Net.SecurityProtocolType]::Tls12 -bor [Net.ServicePointManager]::SecurityProtocol
    } catch { }
    Invoke-WebRequest -Uri $Url -OutFile $Destination -UseBasicParsing `
        -Headers @{ 'User-Agent' = 'ManhwaStudio-run-dev' }
}

<#
.SYNOPSIS
Resolves the browser_download_url of the first asset on a GitHub "latest release"
whose name matches $Pattern. Returns $null when the API is unreachable or no
asset matches — the caller must then fail loudly rather than invent a URL.
#>
function Resolve-GithubAsset {
    param([string] $ApiUrl, [string] $Pattern)
    try {
        try {
            [Net.ServicePointManager]::SecurityProtocol =
                [Net.SecurityProtocolType]::Tls12 -bor [Net.ServicePointManager]::SecurityProtocol
        } catch { }
        $release = Invoke-RestMethod -Uri $ApiUrl -UseBasicParsing `
            -Headers @{ 'User-Agent' = 'ManhwaStudio-run-dev' }
    } catch {
        return $null
    }
    $asset = $release.assets | Where-Object { $_.name -match $Pattern } | Select-Object -First 1
    if (-not $asset) { return $null }
    return $asset.browser_download_url
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
Runs the resolved git with the given arguments, returning an object with
ExitCode and Output. Never throws on a non-zero git exit.
#>
function Invoke-Git {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]] $Arguments)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $out = & $script:Git @Arguments 2>&1
        return [pscustomobject]@{ ExitCode = $LASTEXITCODE; Output = ($out -join "`n") }
    } finally {
        $ErrorActionPreference = $prev
    }
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
    $url = Resolve-GithubAsset -ApiUrl $GitApiMinGit -Pattern $pattern
    if (-not $url) {
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
        Info "Загрузка: $url"
        Get-RemoteFile -Url $url -Destination $zip
        Expand-ZipTo -ZipPath $zip -Destination $dst
    } catch {
        Die $ExitNoGit @(
            'Не удалось скачать или распаковать Git.',
            "Причина: $($_.Exception.Message)",
            'Установите Git вручную: https://git-scm.com/download/win')
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
            Save-LocalChanges "run-dev: содержимое архива $(Get-Date -Format 'yyyy-MM-dd HH:mm')"
            Ok "Файлы обновлены до origin/$Branch."
        }
        '2' { Warn 'Файлы оставлены без изменений.' }
        default { Die $ExitAborted @('Отменено пользователем.') }
    }
}

<#
.SYNOPSIS
Saves tracked modifications into the stash. Untracked files are NEVER included:
the working directory holds the user's projects, models and configs.
#>
function Save-LocalChanges {
    param([string] $Message)
    if (-not (Test-GitOk 'stash' 'push' '-q' '-m' $Message)) {
        Die $ExitGeneric @('Не удалось сохранить локальные изменения в git stash.')
    }
    Info 'Прежнее содержимое сохранено. Вернуть: git stash pop'
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
    $preHead = Get-GitOut 'rev-parse' 'HEAD'

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

    if (-not (Test-GitOk 'stash' 'push' '-q' '-m' 'run-dev: автосохранение перед обновлением')) {
        Die $ExitGeneric @('Не удалось временно сохранить локальные изменения.')
    }

    if (-not (Invoke-Merge -Ahead $Ahead)) {
        [void](Invoke-Git 'reset' '--hard' '-q' $preHead)
        [void](Invoke-Git 'stash' 'pop')
        Die $ExitManualMerge @(
            'Не удалось объединить локальные коммиты с новой версией.',
            'Рабочая копия возвращена в исходное состояние, ничего не потеряно.',
            '',
            "Слейте вручную:  git merge origin/$Branch")
    }

    if (-not (Test-GitOk 'stash' 'pop' '-q')) {
        # `stash pop` keeps the stash entry when it conflicts, so the work is
        # safe. `reset --hard` clears the conflicted index, the working tree and
        # the merge commit at once; the pop then replays onto the original base
        # and therefore applies cleanly.
        [void](Invoke-Git 'reset' '--hard' '-q' $preHead)
        [void](Invoke-Git 'stash' 'pop')
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

function Invoke-GitStage {
    Step 'Проверка обновлений'
    Find-Git

    if (-not (Test-GitOk 'rev-parse' '--git-dir')) {
        if (-not (Invoke-RepositoryAdoption)) { return }
        Resolve-AdoptedTree
        return
    }

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
            Save-LocalChanges "run-dev: отброшенные изменения $(Get-Date -Format 'yyyy-MM-dd HH:mm')"
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
        Get-RemoteFile -Url $RustupUrlGnu -Destination $init
    } catch {
        Die $ExitNoRust @(
            'Не удалось скачать установщик Rust.',
            "Причина: $($_.Exception.Message)",
            'Проверьте интернет-соединение.')
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

    Step 'Компилятор C не найден — скачиваю портативный MinGW-w64'
    Info 'Он нужен зависимости aws-lc-sys (шифрование в сетевых запросах),'
    Info 'которая собирает исходники на C и ассемблере.'
    Info 'Устанавливается только внутрь installer_files\mingw64.'

    $url = Resolve-GithubAsset -ApiUrl $GitApiMingw -Pattern '^winlibs-x86_64-.*mingw-w64.*\.zip$'
    if (-not $url) {
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
        Info "Загрузка: $url"
        Get-RemoteFile -Url $url -Destination $zip
        Expand-ZipTo -ZipPath $zip -Destination $tmp
    } catch {
        Die $ExitNoCc @(
            'Не удалось скачать или распаковать MinGW-w64.',
            "Причина: $($_.Exception.Message)",
            'Установите его вручную: https://winlibs.com/')
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

    $env:PATH = "$dest\bin;$($env:PATH)"
    Ok 'MinGW-w64 установлен.'
}

<#
.SYNOPSIS
Ensures a C compiler matching the active toolchain's host exists. aws-lc-sys
compiles C and assembly on every native target, so this turns a wall of linker
errors 200 crates into the build into one clear message before it.
#>
function Assert-CToolchain {
    $mingwBin = Join-Path (Get-MingwDir) 'bin'
    if (Test-Path (Join-Path $mingwBin 'gcc.exe')) {
        if ($env:PATH -notlike "*$mingwBin*") { $env:PATH = "$mingwBin;$($env:PATH)" }
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

    $cargoArgs = @('run', '--bin', $AppBin)
    if (-not $DebugBuild) { $cargoArgs += '--release' }
    if ($AppArgs -and $AppArgs.Count -gt 0) { $cargoArgs += '--'; $cargoArgs += $AppArgs }

    Say ''
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $script:Cargo @cargoArgs
    $code = $LASTEXITCODE
    $ErrorActionPreference = $prev

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

    if ($NoUpdate) { Info 'Обновление пропущено (-NoUpdate).' } else { Invoke-GitStage }
    Invoke-RustStage
    Invoke-RunStage
}

# `test_run_dev.ps1` dot-sources this file to exercise the git stage in isolation
# against a throwaway repository; sourcing must not run the app.
if (-not $env:MS_RUN_DEV_SOURCE_ONLY) {
    Invoke-Main
}
