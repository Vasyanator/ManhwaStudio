<#
File: tools/run-dev/test_run_dev.ps1

Purpose:
Contract tests for the git stage and version helpers of `run-dev.ps1`. The git
stage is the part that can destroy a user's work, and the Windows implementation
must not silently diverge from the POSIX one, so the same scenarios are asserted
here as in `test_run_dev.sh`.

What is covered:
- Get-VersionNumber / Test-VersionAtLeast / Convert-ToCount / Get-RequiredMsrv
- clean tree behind origin              -> fast-forward
- dirty tree, edits do NOT overlap      -> automatic merge, edits kept
- dirty tree, edits overlap but merge   -> automatic merge, edits kept
- dirty tree, edits truly conflict      -> exit 3 AND the tree is restored
- "discard local" option                -> updated, changes recoverable in stash
- non-repository (ZIP) adoption         -> history grafted, files untouched
- untracked files are never touched by any path
- Invoke-Git                            -> stderr never lands in Output
- Test-ObjectId / Convert-ToCount       -> only well-formed values are accepted
- self-fingerprints                     -> contaminated output degrades to '?',
                                           '?' on either side is not a change
- EOL repair pass                       -> never touches a file with real edits
- stash guard                           -> a pop never restores somebody else's entry

Run:  pwsh -NoProfile -File tools/run-dev/test_run_dev.ps1

Notes:
Dot-sources run-dev.ps1 with MS_RUN_DEV_SOURCE_ONLY=1 so `Invoke-Main` does not
execute, then drives its functions directly. No network, no cargo, no contact
with the user's repository. Runs on any platform with pwsh + git; Stage 2/3 are
Windows-only and are deliberately not exercised.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Continue'

$SelfDir = Split-Path -Parent $PSCommandPath
$script:Pass = 0
$script:Fail = 0

function Note  { param([string] $T) Write-Host ''; Write-Host $T -ForegroundColor White }
function Check {
    param([string] $Desc, $Actual, $Expected)
    if ("$Actual" -eq "$Expected") {
        Write-Host '  PASS ' -ForegroundColor Green -NoNewline; Write-Host $Desc
        $script:Pass++
    } else {
        Write-Host '  FAIL ' -ForegroundColor Red -NoNewline
        Write-Host "$Desc (получено «$Actual», ожидалось «$Expected»)"
        $script:Fail++
    }
}

$Sandbox = Join-Path ([System.IO.Path]::GetTempPath()) ("run-dev-test-" + [guid]::NewGuid().ToString('N'))
[void](New-Item -ItemType Directory -Path $Sandbox -Force)

$env:MS_RUN_DEV_SOURCE_ONLY = '1'
$env:MS_RUN_DEV_BRANCH      = 'master'
$env:GIT_CONFIG_GLOBAL      = Join-Path $Sandbox 'gitconfig'
$env:GIT_CONFIG_NOSYSTEM    = '1'
git config --file $env:GIT_CONFIG_GLOBAL user.email 'test@example.com'
git config --file $env:GIT_CONFIG_GLOBAL user.name  'run-dev test'
git config --file $env:GIT_CONFIG_GLOBAL init.defaultBranch master

try {
    . (Join-Path $SelfDir 'run-dev.ps1')

    # Silence the script's own chatter; tests assert on state, not on prose.
    function Say  { param([string] $Text = '') }
    function Step { param([string] $Text) }
    function Ok   { param([string] $Text) }
    function Warn { param([string] $Text) }
    function Info { param([string] $Text) }

    # `Die` must not kill the test process; record the code and unwind instead.
    function Die {
        param([int] $Code, [string[]] $Lines)
        $script:LastDieCode = $Code
        throw "DIE:$Code"
    }

    # ---------------------------------------------------------------------
    # Fixtures
    # ---------------------------------------------------------------------

    function New-RepoPair {
        param([string] $Name)
        $up = Join-Path $Sandbox "$Name.git"
        $wc = Join-Path $Sandbox $Name
        [void](New-Item -ItemType Directory -Path $up -Force)
        git -C $up init -q --bare
        git clone -q $up $wc 2>$null
        Push-Location $wc
        try {
            Set-Content -Path 'upstream.txt' -Value 'base' -NoNewline
            Set-Content -Path 'shared.txt' -Value "line1`nline2`nline3`nline4`nline5`nline6" -NoNewline
            Set-Content -Path 'local.txt' -Value 'mine' -NoNewline
            Set-Content -Path 'Cargo.toml' -Value 'rust-version = "1.92"' -NoNewline
            git add -A; git commit -qm 'base'; git push -q origin master
            Set-Content -Path 'upstream.txt' -Value 'updated upstream' -NoNewline
            Set-Content -Path 'shared.txt' -Value "line1-CHANGED`nline2`nline3`nline4`nline5`nline6" -NoNewline
            git add -A; git commit -qm 'upstream work'; git push -q origin master
            git reset -q --hard HEAD~1
        } finally { Pop-Location }
        return $wc
    }

    function Use-Repo {
        param([string] $Wc, [string] $Origin)
        $script:RepoRoot = $Wc
        Set-Variable -Name OriginUrl -Scope Script -Value $Origin
        Set-Location $Wc
        $script:Git     = 'git'
        $script:Adopted = $false
    }

    # Runs Invoke-GitStage, returning its effective exit code (0, or the code Die
    # was called with).
    function Invoke-Stage {
        $script:LastDieCode = 0
        try { Invoke-GitStage | Out-Null } catch {
            if ("$_" -notlike '*DIE:*') { throw }
        }
        return $script:LastDieCode
    }

    # Switch flags the stage reads. They are script params, so shadow them.
    $script:Offline = $false
    $script:Yes     = $true

    # ---------------------------------------------------------------------
    Note 'Версии'
    # ---------------------------------------------------------------------

    Check 'Get-VersionNumber rustc'   (Get-VersionNumber 'rustc 1.96.1 (31fca3adb 2026-06-26)') '1.96.1'
    Check 'Get-VersionNumber nightly' (Get-VersionNumber 'rustc 1.93.0-nightly (abc)') '1.93.0'
    Check 'Get-VersionNumber git'     (Get-VersionNumber 'git version 2.43.0') '2.43.0'
    Check 'Get-VersionNumber две части' (Get-VersionNumber 'rustc 1.92') '1.92.0'

    Check '1.96.1 >= 1.92' (Test-VersionAtLeast '1.96.1' '1.92') $true
    Check '1.92.0 >= 1.92' (Test-VersionAtLeast '1.92.0' '1.92') $true
    Check '1.91.9 <  1.92' (Test-VersionAtLeast '1.91.9' '1.92') $false
    Check '1.9 < 1.92 (не строковое сравнение)' (Test-VersionAtLeast '1.9' '1.92') $false
    Check 'пустая версия не проходит' (Test-VersionAtLeast $null '1.92') $false

    Check 'Convert-ToCount "3"'   (Convert-ToCount '3') 3
    Check 'Convert-ToCount пусто' (Convert-ToCount '') 0
    Check 'Convert-ToCount мусор' (Convert-ToCount 'fatal: bad revision') 0
    Check 'Convert-ToCount мусор с цифрой'   (Convert-ToCount 'warning: 3 files') 0
    Check 'Convert-ToCount строка с хвостом' (Convert-ToCount '3 files') 0

    Check 'Test-ObjectId 40 hex' (Test-ObjectId '0123456789abcdef0123456789abcdef01234567') $true
    Check 'Test-ObjectId 64 hex' (Test-ObjectId ('0' * 64)) $true
    Check 'Test-ObjectId 39 hex' (Test-ObjectId '0123456789abcdef0123456789abcdef0123456') $false
    Check 'Test-ObjectId пусто'  (Test-ObjectId '') $false
    Check 'Test-ObjectId предупреждение' (Test-ObjectId 'warning: LF will be replaced by CRLF') $false
    Check 'Test-ObjectId верхний регистр' (Test-ObjectId '0123456789ABCDEF0123456789abcdef01234567') $false

    $script:RepoRoot = (Resolve-Path (Join-Path $SelfDir '../..')).Path
    Check 'Get-RequiredMsrv читает Cargo.toml проекта' (Get-RequiredMsrv) '1.92'

    # ---------------------------------------------------------------------
    Note 'Чистая рабочая копия, отставшая от origin'
    # ---------------------------------------------------------------------

    $wc = New-RepoPair 'clean'
    Use-Repo $wc (Join-Path $Sandbox 'clean.git')
    Check 'код возврата'         (Invoke-Stage) 0
    Check 'обновилась до origin' (Get-Content 'upstream.txt' -Raw).Trim() 'updated upstream'
    Check 'нет отставания'       (git rev-list --count HEAD..origin/master) '0'

    # ---------------------------------------------------------------------
    Note 'Локальные правки НЕ пересекаются с новыми коммитами'
    # ---------------------------------------------------------------------

    $wc = New-RepoPair 'disjoint'
    Use-Repo $wc (Join-Path $Sandbox 'disjoint.git')
    Set-Content 'local.txt' 'my local edit' -NoNewline
    Set-Content 'untracked-data.bin' 'user data' -NoNewline
    Check 'код возврата'               (Invoke-Stage) 0
    Check 'обновление применено'       (Get-Content 'upstream.txt' -Raw).Trim() 'updated upstream'
    Check 'локальная правка сохранена' (Get-Content 'local.txt' -Raw).Trim() 'my local edit'
    Check 'untracked-файл не тронут'   (Get-Content 'untracked-data.bin' -Raw).Trim() 'user data'
    Check 'stash пуст'                 (@(git stash list).Count) 0

    # ---------------------------------------------------------------------
    Note 'Правки в ТОМ ЖЕ файле, но git способен слить'
    # ---------------------------------------------------------------------

    $wc = New-RepoPair 'overlap_ok'
    Use-Repo $wc (Join-Path $Sandbox 'overlap_ok.git')
    Set-Content 'shared.txt' "line1`nline2`nline3`nline4`nline5`nline6-MINE" -NoNewline
    Check 'код возврата'                  (Invoke-Stage) 0
    Check 'правка upstream применена'     ((Get-Content 'shared.txt')[0]) 'line1-CHANGED'
    Check 'правка пользователя сохранена' ((Get-Content 'shared.txt')[5]) 'line6-MINE'

    # ---------------------------------------------------------------------
    Note 'Настоящий конфликт: та же строка'
    # ---------------------------------------------------------------------

    $wc = New-RepoPair 'conflict'
    Use-Repo $wc (Join-Path $Sandbox 'conflict.git')
    Set-Content 'shared.txt' "line1-MINE`nline2`nline3`nline4`nline5`nline6" -NoNewline
    Set-Content 'untracked-data.bin' 'user data' -NoNewline
    $beforeHead = (git rev-parse HEAD)
    $beforeFile = (Get-Content 'shared.txt' -Raw)
    Check 'код возврата = 3 (нужно ручное слияние)' (Invoke-Stage) 3
    Check 'HEAD возвращён на место'        (git rev-parse HEAD) $beforeHead
    Check 'файл возвращён в исходный вид'  (Get-Content 'shared.txt' -Raw) $beforeFile
    Check 'нет маркеров конфликта'         (@(Select-String -Path 'shared.txt' -Pattern '<<<<<<<').Count) 0
    Check 'не осталось слияния в процессе' (Test-Path '.git/MERGE_HEAD') $false
    Check 'untracked-файл не тронут'       (Get-Content 'untracked-data.bin' -Raw).Trim() 'user data'
    Check 'stash не оставлен'              (@(git stash list).Count) 0

    # ---------------------------------------------------------------------
    Note 'Вариант «убрать локальные изменения»'
    # ---------------------------------------------------------------------

    $wc = New-RepoPair 'discard'
    Use-Repo $wc (Join-Path $Sandbox 'discard.git')
    Set-Content 'shared.txt' "line1-MINE`nline2`nline3`nline4`nline5`nline6" -NoNewline
    $script:DiscardLocal = $true
    Check 'код возврата'                       (Invoke-Stage) 0
    Check 'обновление применено'               ((Get-Content 'shared.txt')[0]) 'line1-CHANGED'
    Check 'изменения не уничтожены, а в stash' (@(git stash list).Count) 1
    $script:DiscardLocal = $false

    # ---------------------------------------------------------------------
    Note 'Копия из архива: git-репозитория нет'
    # ---------------------------------------------------------------------

    $src = New-RepoPair 'zipsrc'
    $zip = Join-Path $Sandbox 'zipcopy'
    Copy-Item -Recurse $src $zip
    Remove-Item -Recurse -Force (Join-Path $zip '.git')
    Set-Content (Join-Path $zip 'untracked-data.bin') 'user data' -NoNewline
    Use-Repo $zip (Join-Path $Sandbox 'zipsrc.git')
    Check 'код возврата'             (Invoke-Stage) 0
    Check 'репозиторий подключён'    (Test-Path (Join-Path $zip '.git')) $true
    Check 'ветка master'             (git rev-parse --abbrev-ref HEAD) 'master'
    Check 'файлы доведены до origin' (Get-Content (Join-Path $zip 'upstream.txt') -Raw).Trim() 'updated upstream'
    Check 'untracked-файл не тронут' (Get-Content (Join-Path $zip 'untracked-data.bin') -Raw).Trim() 'user data'
    Check 'upstream настроен'        (git rev-parse --abbrev-ref 'master@{upstream}') 'origin/master'

    # ---------------------------------------------------------------------
    Note 'Уже актуальная копия'
    # ---------------------------------------------------------------------

    $wc = New-RepoPair 'current'
    Use-Repo $wc (Join-Path $Sandbox 'current.git')
    git merge -q --ff-only origin/master
    Check 'код возврата'        (Invoke-Stage) 0
    Check 'ничего не сломалось' (git rev-list --count HEAD..origin/master) '0'

    # ---------------------------------------------------------------------
    Note 'Invoke-Git: stderr не попадает в Output'
    # ---------------------------------------------------------------------

    # Любая строка git на stderr, попав в Output, становится «хэшем», «числом
    # коммитов» или «изменённым файлом» — отсюда и ложные перезапуски.
    $outside = Join-Path $Sandbox 'not-a-repo'
    [void](New-Item -ItemType Directory -Path $outside -Force)
    Set-Location $outside
    $probe = Invoke-Git 'status' '--porcelain'
    Check 'команда провалилась'          ($probe.ExitCode -ne 0) $true
    Check 'stdout пуст'                  ([string]::IsNullOrWhiteSpace($probe.Output)) $true
    Check 'stderr сохранён отдельно'     ([string]::IsNullOrWhiteSpace($probe.Error)) $false
    Check 'Get-GitOut не отдаёт stderr'  ([string]::IsNullOrWhiteSpace((Get-GitOut 'status' '--porcelain'))) $true

    # ---------------------------------------------------------------------
    Note 'Чужой stash не всплывает при пустом сохранении'
    # ---------------------------------------------------------------------

    # `git stash push` на чистом дереве завершается кодом 0, ничего не создав.
    # Последующий pop вернул бы ЧУЖУЮ, более старую запись поверх рабочей копии.
    $wc = New-RepoPair 'stashguard'
    Use-Repo $wc (Join-Path $Sandbox 'stashguard.git')
    Set-Content 'local.txt' 'содержимое чужого stash' -NoNewline
    git stash push -q -m 'чужой stash'
    $script:LastDieCode = 0
    try { Update-WithLocalChanges -Ahead 0 | Out-Null } catch {
        if ("$_" -notlike '*DIE:*') { throw }
    }
    Check 'код возврата'                 $script:LastDieCode 0
    Check 'обновление применено'         (Get-Content 'upstream.txt' -Raw).Trim() 'updated upstream'
    Check 'чужой stash не применён'      (Get-Content 'local.txt' -Raw).Trim() 'mine'
    Check 'чужой stash остался на месте' (@(git stash list).Count) 1
    Check 'Get-StashTop видит запись'    (Test-ObjectId (Get-StashTop)) $true

    Check 'Save-LocalChanges сообщает о пустом сохранении' `
          (Save-LocalChanges 'пустая попытка') $false
    Check 'лишней записи не появилось'   (@(git stash list).Count) 1

    # ---------------------------------------------------------------------
    Note 'Возвращается ИМЕННО своя запись stash, а не верхушка стека'
    # ---------------------------------------------------------------------

    # Стек stash общий со всей машиной: пока run-dev работает, IDE или второй
    # терминал может положить свою запись сверху.
    $wc = New-RepoPair 'stashown'
    Use-Repo $wc (Join-Path $Sandbox 'stashown.git')
    Set-Content 'local.txt' 'моя правка' -NoNewline
    git stash push -q -m 'наша запись'
    $ourStash = (git rev-parse refs/stash).Trim()
    Set-Content 'local.txt' 'чужая правка' -NoNewline
    git stash push -q -m 'чужая запись'

    Check 'своя запись найдена по id, а не по позиции' (Get-StashRefFor $ourStash) 'stash@{1}'
    Check 'поп своей записи удался'      (Invoke-StashPop $ourStash) $true
    Check 'применена именно своя правка' (Get-Content 'local.txt' -Raw).Trim() 'моя правка'
    Check 'чужая запись осталась в стеке' (@(git stash list).Count) 1
    Check 'и это именно чужая запись'    (@(git stash list --format='%s' | Select-String 'чужая').Count) 1

    Check 'несуществующая запись не попается' `
          (Invoke-StashPop '0123456789abcdef0123456789abcdef01234567') $false
    Check 'стек stash при этом не тронут' (@(git stash list).Count) 1

    # ---------------------------------------------------------------------
    Note 'Починка концов строк не трогает настоящие правки'
    # ---------------------------------------------------------------------

    # Repair-SelfEol переписывает файл только когда git считает его изменённым,
    # а diff пуст (чистое несоответствие концов строк). Настоящая правка обязана
    # остаться нетронутой.
    # Only root-level self-files are asserted on: $SelfFiles spells the nested
    # ones with backslashes, which resolve to a real path on Windows only, and
    # this file is meant to run under pwsh on either platform.
    $eol = Join-Path $Sandbox 'eolrepo'
    [void](New-Item -ItemType Directory -Path $eol -Force)
    Set-Location $eol
    git init -q
    Set-Content 'run-dev.Linux.sh'    'launcher' -NoNewline
    Set-Content 'run-dev.Windows.bat' 'bat'      -NoNewline
    git add -A; git commit -qm 'self files'
    $script:RepoRoot = $eol
    $script:Git      = 'git'
    Set-Content 'run-dev.Windows.bat' 'НАСТОЯЩАЯ ПРАВКА' -NoNewline
    Repair-SelfEol
    Check 'файл с настоящей правкой не тронут' `
          (Get-Content 'run-dev.Windows.bat' -Raw).Trim() 'НАСТОЯЩАЯ ПРАВКА'
    Check 'нетронутый файл остался прежним' `
          (Get-Content 'run-dev.Linux.sh' -Raw).Trim() 'launcher'
    Repair-SelfEol
    Check 'проход идемпотентен' `
          (Get-Content 'run-dev.Windows.bat' -Raw).Trim() 'НАСТОЯЩАЯ ПРАВКА'

    # Сломанный внешний diff: git пишет только в stderr и выходит ненулевым кодом.
    # Пустой stdout НЕ должен читаться как «diff пуст» — иначе checkout уничтожит
    # несохранённые правки пользователя.
    $env:GIT_EXTERNAL_DIFF = Join-Path $Sandbox 'no-such-diff-tool'
    Repair-SelfEol
    Check 'сломанный GIT_EXTERNAL_DIFF не приводит к затиранию' `
          (Get-Content 'run-dev.Windows.bat' -Raw).Trim() 'НАСТОЯЩАЯ ПРАВКА'
    Remove-Item Env:GIT_EXTERNAL_DIFF
    git checkout -q -- 'run-dev.Windows.bat'

    # Конфликт слияния (XY = UU) — не «несоответствие концов строк», трогать нельзя.
    git checkout -q -b other
    Set-Content 'run-dev.Linux.sh' 'ветка other' -NoNewline
    git commit -q -am 'other'
    git checkout -q master
    Set-Content 'run-dev.Linux.sh' 'ветка master' -NoNewline
    git commit -q -am 'master'
    git merge -q other 2>$null
    Check 'конфликт действительно создан' `
          ((git status --porcelain --untracked-files=no -- 'run-dev.Linux.sh') -replace '^(..).*', '$1') 'UU'
    Repair-SelfEol
    Check 'конфликтующий файл не тронут' `
          (@(Select-String -Path 'run-dev.Linux.sh' -Pattern '<<<<<<<').Count) 1
    git merge --abort 2>$null

    # ---------------------------------------------------------------------
    Note 'Отпечатки самих скриптов run-dev'
    # ---------------------------------------------------------------------

    git checkout -q -- 'run-dev.Windows.bat'
    $fp1 = Get-SelfFingerprints
    Check 'снимок покрывает все исполняемые файлы' $fp1.Count 5
    Check 'отсутствующий файл помечен как -'       $fp1['run-dev.MacOS.command'] '-'
    Check 'присутствующий файл имеет объектный id' (Test-ObjectId $fp1['run-dev.Linux.sh']) $true
    Check 'повторный снимок идентичен' (@(Get-ChangedSelfFiles -Before $fp1 -After (Get-SelfFingerprints))).Count 0

    Set-Content 'run-dev.Linux.sh' 'изменено обновлением' -NoNewline
    $fp2 = Get-SelfFingerprints
    $changed = @(Get-ChangedSelfFiles -Before $fp1 -After $fp2)
    Check 'изменение скрипта замечено'    $changed.Count 1
    Check 'назван именно изменённый файл' $changed[0] 'run-dev.Linux.sh'

    # '?' с ЛЮБОЙ стороны — «неизвестно», а не «изменилось»: сбой git между
    # снимками не должен фабриковать требование перезапуска.
    $unknown = @{}
    foreach ($k in $fp1.Keys) { $unknown[$k] = '?' }
    Check 'сбой git с одной стороны — не изменение' `
          (@(Get-ChangedSelfFiles -Before $fp1 -After $unknown)).Count 0
    Check 'сбой git с другой стороны — тоже не изменение' `
          (@(Get-ChangedSelfFiles -Before $unknown -After $fp1)).Count 0

    # Загрязнённый вывод git не должен становиться «отпечатком»: он не сравним
    # сам с собой на следующем запуске.
    $realInvokeGit = ${function:Invoke-Git}
    function Invoke-Git {
        param([Parameter(ValueFromRemainingArguments = $true)][string[]] $Arguments)
        return [pscustomobject]@{
            ExitCode = 0
            Output   = "warning: in the working copy of 'x', LF will be replaced by CRLF"
            Error    = ''
        }
    }
    $polluted = Get-SelfFingerprints
    ${function:Invoke-Git} = $realInvokeGit
    Check 'текст вместо хэша отбраковывается' $polluted['run-dev.Linux.sh'] '?'
    Check 'мусорный снимок не считается изменением' `
          (@(Get-ChangedSelfFiles -Before $fp1 -After $polluted)).Count 0

} finally {
    Set-Location ([System.IO.Path]::GetTempPath())
    Remove-Item -Recurse -Force $Sandbox -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host "Итого: $($script:Pass) пройдено, $($script:Fail) провалено"
if ($script:Fail -ne 0) { exit 1 }
