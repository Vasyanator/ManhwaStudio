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
- array returns                         -> 0/1/many survive the call boundary
- self-update detection                 -> git names the run-dev paths an update
                                           touched; exit 8 only when it did
- stash guard                           -> a pop never restores somebody else's entry
- root binary: cargo JSON parsing       -> the right bin target, unescaped path
- root binary: copy predicate and copy  -> skipped when identical, .part never left
- root binary: leftovers                -> .part and every .old-* aside swept
- root binary: launch                   -> arguments survive spaces, real exit code
- gcc version directory                 -> discovered, ambiguity yields nothing
- a space in the toolchain path         -> without one, only PATH is touched and
                                           never duplicated; on Windows a path
                                           WITH one yields GCC_EXEC_PREFIX and
                                           LIBRARY_PATH in 8.3 form, pointing at
                                           directories that exist

Run:  pwsh -NoProfile -File tools/run-dev/test_run_dev.ps1

Notes:
Dot-sources run-dev.ps1 with MS_RUN_DEV_SOURCE_ONLY=1 so `Invoke-Main` does not
execute, then drives its functions directly. No network, no cargo, no contact
with the user's repository. Runs on any platform with pwsh + git; Stage 2/3 are
Windows-only and are deliberately not exercised.
#>

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Continue'
# Russian test names are unreadable in the console's OEM code page otherwise.
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch { }

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

    # run-dev.ps1 sets 'Stop' at script scope, and dot-sourcing brings that here.
    # Under 'Stop' every native command that writes to stderr — `git clone` on an
    # empty repository, for one — becomes a terminating error and kills the run.
    $ErrorActionPreference = 'Continue'

    # Silence the script's own chatter; tests assert on state, not on prose.
    function Say  { param([string] $Text = '') }
    function Step { param([string] $Text) }
    function Ok   { param([string] $Text) }
    function Warn { param([string] $Text) }
    function Info { param([string] $Text) }

    # `Die` / `Exit-WithBanner` must not kill the test process; record the code and
    # unwind instead. Both are stubbed: the restart notice goes through the banner
    # directly, not through Die.
    function Die {
        param([int] $Code, [string[]] $Lines)
        $script:LastDieCode = $Code
        throw "DIE:$Code"
    }
    function Exit-WithBanner {
        param([int] $Code, [string] $Title, [System.ConsoleColor] $Color, [string[]] $Lines)
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
    Note 'Возврат массива из функции: 0, 1 и много элементов'
    # ---------------------------------------------------------------------

    # Регрессия на исходную причину ложных перезапусков: `return ,$a` отдаёт
    # ВНУТРЕННИЙ массив одним объектом, поэтому @(вызов) всегда даёт Count=1 —
    # даже для пустого массива. Конвенция проекта: возвращать массив как есть,
    # принимать через @(...). См. tools/run-dev/MODULE_README.md.
    function Test-ReturnEmpty { $a = @();        return $a }
    function Test-ReturnOne   { $a = @('x');     return $a }
    function Test-ReturnMany  { $a = @('x','y'); return $a }
    function Test-ReturnComma { $a = @();        return ,$a }

    Check 'пустой массив -> Count 0'   (@(Test-ReturnEmpty)).Count 0
    Check 'один элемент  -> Count 1'   (@(Test-ReturnOne)).Count   1
    Check 'много         -> Count 2'   (@(Test-ReturnMany)).Count  2
    Check 'элемент остаётся строкой'   (@(Test-ReturnOne))[0] 'x'
    # Именно так выглядел баг: запрещённая форма даёт Count=1 на пустом массиве.
    Check 'запрещённая форма ,$a ломает пустой случай' (@(Test-ReturnComma)).Count 1

    # ---------------------------------------------------------------------
    Note 'Какие файлы run-dev затронуло обновление — спрашиваем у git'
    # ---------------------------------------------------------------------

    # Механизм: HEAD до обновления сравнивается с HEAD после, и git сам называет
    # затронутые пути. Пути задаются в git-нотации (прямые слэши).
    $selfwc = Join-Path $Sandbox 'selfupd'
    [void](New-Item -ItemType Directory -Path (Join-Path $selfwc 'tools/run-dev') -Force)
    Set-Location $selfwc
    git init -q
    Set-Content 'tools/run-dev/run-dev.sh'  'core'     -NoNewline
    Set-Content 'tools/run-dev/run-dev.ps1' 'windows'  -NoNewline
    Set-Content 'run-dev.Linux.sh'          'launcher' -NoNewline
    Set-Content 'src.txt'                   'other'    -NoNewline
    git add -A; git commit -qm 'base'

    $script:RepoRoot        = $selfwc
    $script:Git             = 'git'
    $script:AdoptedReplaced = $false
    $script:PreHead         = (git rev-parse HEAD).Trim()

    Check 'HEAD не сдвинулся — список пуст' (@(Get-ChangedSelfPaths)).Count 0

    Set-Content 'src.txt' 'other changed' -NoNewline
    git commit -qam 'чужой коммит'
    Check 'обновление мимо run-dev — список пуст' (@(Get-ChangedSelfPaths)).Count 0

    Set-Content 'tools/run-dev/run-dev.sh' 'core updated' -NoNewline
    git commit -qam 'правка run-dev'
    $changed = @(Get-ChangedSelfPaths)
    Check 'затронут файл run-dev — git называет его' $changed.Count 1
    Check 'путь в git-нотации' $changed[0] 'tools/run-dev/run-dev.sh'

    Set-Content 'run-dev.Linux.sh'          'launcher updated' -NoNewline
    Set-Content 'tools/run-dev/run-dev.ps1' 'windows updated'  -NoNewline
    git commit -qam 'правка нескольких лаунчеров'
    Check 'перечислены все затронутые файлы' `
          ((@(Get-ChangedSelfPaths) | Sort-Object) -join ' ') `
          'run-dev.Linux.sh tools/run-dev/run-dev.ps1 tools/run-dev/run-dev.sh'

    # Runs Assert-NoSelfUpdate, returning the code Exit-WithBanner would exit with.
    function Invoke-SelfCheck {
        $script:LastDieCode = 0
        try { Assert-NoSelfUpdate | Out-Null } catch {
            if ("$_" -notlike '*DIE:*') { throw }
        }
        return $script:LastDieCode
    }

    Check 'run-dev обновился -> код 8' (Invoke-SelfCheck) 8

    $script:PreHead = (git rev-parse HEAD).Trim()
    Check 'HEAD на месте -> продолжаем' (Invoke-SelfCheck) 0

    Set-Content 'src.txt' 'other changed again' -NoNewline
    git commit -qam 'снова чужой коммит'
    Check 'обновление мимо run-dev -> продолжаем' (Invoke-SelfCheck) 0

    $script:PreHead = ''
    Check 'HEAD не запоминался (-NoUpdate) -> продолжаем' (Invoke-SelfCheck) 0

    # Ветка adoption: базового коммита нет, решение принимает сама стадия.
    $script:PreHead         = (git rev-parse HEAD).Trim()
    $script:AdoptedReplaced = $true
    Check 'adoption заменил файлы -> код 8' (Invoke-SelfCheck) 8
    $script:AdoptedReplaced = $false


    # ---------------------------------------------------------------------
    Note 'Загрузка: .part, проверка размера и контрольной суммы'
    # ---------------------------------------------------------------------

    # Сеть здесь не нужна: проверяются те части контракта, которые работают с
    # локальным файлом — имя .part, критерий завершённости и сверка SHA-256.
    $dl = Join-Path $Sandbox 'dl'
    [void](New-Item -ItemType Directory -Path $dl -Force)
    $target = Join-Path $dl 'asset.zip'
    $part   = "$target.part"

    Check 'размер форматируется по-человечески' (Format-Bytes 274029684) '261 МБ'
    Check 'мелкий размер тоже'                  (Format-Bytes 130)       '130 Б'

    Set-Content -LiteralPath $part -Value 'полный файл' -NoNewline -Encoding Ascii
    $len = (Get-Item $part).Length
    Check 'точный размер -> завершено'   (Test-DownloadComplete -Part $part -ExpectedSize $len) $true
    Check 'короче ожидаемого -> нет'     (Test-DownloadComplete -Part $part -ExpectedSize ($len + 1)) $false
    Check 'длиннее ожидаемого -> нет'    (Test-DownloadComplete -Part $part -ExpectedSize ($len - 1)) $false
    Check 'размер неизвестен -> хватает непустого' (Test-DownloadComplete -Part $part) $true
    Check 'нет файла -> не завершено'    (Test-DownloadComplete -Part (Join-Path $dl 'нет.part')) $false

    # Сверка суммы: сайдкар отдаём через file:// — сеть не нужна, код тот же.
    $hash = (Get-FileHash -LiteralPath $part -Algorithm SHA256).Hash.ToLower()
    $side = Join-Path $dl 'asset.sha256'
    Set-Content -LiteralPath $side -Value "$hash  asset.zip" -Encoding Ascii
    $sideUrl = ([Uri]$side).AbsoluteUri

    $okHash = $true
    try { Assert-Sha256 -Part $part -Sha256Url $sideUrl } catch { $okHash = $false }
    Check 'совпавшая сумма не мешает'  $okHash $true
    Check 'файл при этом цел'          (Test-Path $part) $true

    Set-Content -LiteralPath $side -Value ('0' * 64 + '  asset.zip') -Encoding Ascii
    $badHash = $false
    try { Assert-Sha256 -Part $part -Sha256Url $sideUrl } catch { $badHash = $true }
    Check 'несовпавшая сумма -> ошибка'        $badHash $true
    Check 'битый .part удалён, а не докачан'   (Test-Path $part) $false

    # Сайдкара нет — это не ошибка, проверка просто пропускается.
    Set-Content -LiteralPath $part -Value 'снова файл' -NoNewline -Encoding Ascii
    $missing = $true
    try { Assert-Sha256 -Part $part -Sha256Url ([Uri](Join-Path $dl 'нетсайдкара.sha256')).AbsoluteUri }
    catch { $missing = $false }
    Check 'нет сайдкара -> не ошибка'  $missing $true
    Check 'файл не пострадал'          (Test-Path $part) $true

    # Выбор загрузчика: curl.exe должен искаться как ПРИЛОЖЕНИЕ, потому что в
    # Windows PowerShell `curl` — это алиас Invoke-WebRequest.
    $curlApp = @(Get-Command 'curl.exe' -CommandType Application -ErrorAction SilentlyContinue)
    $curlAny = @(Get-Command 'curl' -ErrorAction SilentlyContinue)
    if ($curlAny.Count -gt 0) {
        Check 'curl без .exe может оказаться алиасом IWR' `
              ($curlAny[0].CommandType -eq 'Application') `
              ($PSVersionTable.PSVersion.Major -ge 6)
    }
    if ($curlApp.Count -gt 0) {
        Check 'curl.exe разрешается в приложение' $curlApp[0].CommandType 'Application'
    }

    # ---------------------------------------------------------------------
    Note 'Готовый файл в корне: путь берётся у cargo, а не угадывается'
    # ---------------------------------------------------------------------

    # Поток --message-format=json как его отдаёт cargo: строка не-JSON перед ним,
    # артефакт библиотеки с "executable":null, артефакт ДРУГОГО bin-таргета,
    # сообщение без полей target/executable вовсе (под StrictMode обращение к
    # отсутствующему полю — ошибка) и завершающий build-finished.
    $cargoJson = @(
        'Compiling manhwastudio_rs v3.6.0 (C:\proj)',
        '{"reason":"compiler-artifact","target":{"name":"egui","kind":["lib"]},"executable":null}',
        '{"reason":"build-script-executed","package_id":"aws-lc-sys 0.1.0"}',
        '{"reason":"compiler-artifact","target":{"name":"render_gallery","kind":["bin"]},"executable":"C:\\proj\\target\\x86_64-pc-windows-gnu\\release\\render_gallery.exe"}',
        '{"reason":"compiler-artifact","target":{"name":"manhwastudio_rs","kind":["bin"]},"executable":"C:\\proj\\target\\x86_64-pc-windows-gnu\\release\\manhwastudio_rs.exe"}',
        '{"reason":"build-finished","success":true}'
    ) -join "`n"

    $wantExe = 'C:\proj\target\x86_64-pc-windows-gnu\release\manhwastudio_rs.exe'
    Check 'взят путь нужного bin-таргета' `
          (Get-BuiltBinaryFromJson -Json $cargoJson -TargetName 'manhwastudio_rs') $wantExe
    Check 'обратные слэши развёрнуты из JSON-экранирования' `
          ((Get-BuiltBinaryFromJson -Json $cargoJson -TargetName 'manhwastudio_rs') -like '*\release\*') $true
    Check 'соседний bin-таргет не перепутан' `
          (Get-BuiltBinaryFromJson -Json $cargoJson -TargetName 'render_gallery') `
          'C:\proj\target\x86_64-pc-windows-gnu\release\render_gallery.exe'
    Check 'нужного таргета нет -> пусто' `
          (Get-BuiltBinaryFromJson -Json $cargoJson -TargetName 'tutorial_test') ''
    Check 'пустой ввод -> пусто'  (Get-BuiltBinaryFromJson -Json '' -TargetName 'manhwastudio_rs') ''
    Check 'мусор вместо JSON -> пусто' `
          (Get-BuiltBinaryFromJson -Json "error: could not compile`nfatal" -TargetName 'manhwastudio_rs') ''

    # Побеждает ПОСЛЕДНИЙ артефакт — пересборка может выдать несколько.
    $twice = @(
        '{"reason":"compiler-artifact","target":{"name":"manhwastudio_rs","kind":["bin"]},"executable":"C:\\old.exe"}',
        '{"reason":"compiler-artifact","target":{"name":"manhwastudio_rs","kind":["bin"]},"executable":"C:\\new.exe"}'
    ) -join "`n"
    Check 'из нескольких артефактов берётся последний' `
          (Get-BuiltBinaryFromJson -Json $twice -TargetName 'manhwastudio_rs') 'C:\new.exe'

    # ...но "последний" не отменяет требования непустого executable: артефакт
    # библиотеки с тем же именем не должен затирать найденный путь.
    $libLast = @(
        '{"reason":"compiler-artifact","target":{"name":"manhwastudio_rs","kind":["bin"]},"executable":"C:\\app.exe"}',
        '{"reason":"compiler-artifact","target":{"name":"manhwastudio_rs","kind":["lib"]},"executable":null}'
    ) -join "`n"
    Check 'артефакт с executable:null не затирает найденный путь' `
          (Get-BuiltBinaryFromJson -Json $libLast -TargetName 'manhwastudio_rs') 'C:\app.exe'

    # ---------------------------------------------------------------------
    Note 'Готовый файл в корне: когда копировать и как'
    # ---------------------------------------------------------------------

    $rb = Join-Path $Sandbox 'rootbin'
    [void](New-Item -ItemType Directory -Path $rb -Force)
    $srcExe = Join-Path $rb 'built.bin'
    $dstExe = Join-Path $rb 'root.bin'
    Set-Content -LiteralPath $srcExe -Value 'built payload' -NoNewline -Encoding Ascii

    # Продакшн работает под 'Stop', а этот файл — под 'Continue'. Копирование
    # проверяем в боевом режиме: иначе непойманная ошибка Copy-Item/Move-Item
    # молча превратилась бы в «успех».
    $eapPrev = $ErrorActionPreference
    $ErrorActionPreference = 'Stop'
    try {

    Check 'в корне ничего нет -> копировать' `
          (Test-RootBinaryNeedsCopy -Source $srcExe -Destination $dstExe) $true

    Install-RootBinary -Source $srcExe -Destination $dstExe
    Check 'файл появился в корне'      (Test-Path -LiteralPath $dstExe) $true
    Check 'временный .part не остался' (Test-Path -LiteralPath "$dstExe.part") $false
    Check 'копия .old-* не осталась'   (@(Get-ChildItem -LiteralPath $rb -Filter 'root.bin.old-*' -File)).Count 0
    Check 'содержимое совпадает'       (Get-Content -LiteralPath $dstExe -Raw) 'built payload'
    # Ровно на этом держится предикат: Copy-Item сохраняет время изменения, иначе
    # каждый запуск копировал бы файл заново. Сравниваем сами DateTime, а не их
    # строковый вид: строка округляется до секунд и скрыла бы расхождение в
    # миллисекундах — то самое, из-за которого предикат начал бы копировать всегда.
    Check 'время изменения сохранено копированием' `
          ((Get-Item -LiteralPath $dstExe).LastWriteTimeUtc -eq (Get-Item -LiteralPath $srcExe).LastWriteTimeUtc) $true
    Check 'та же сборка -> копировать не нужно' `
          (Test-RootBinaryNeedsCopy -Source $srcExe -Destination $dstExe) $false

    # Другой размер при том же времени изменения.
    $stamp = (Get-Item -LiteralPath $srcExe).LastWriteTimeUtc
    Set-Content -LiteralPath $dstExe -Value 'built payload + tail' -NoNewline -Encoding Ascii
    $dstItem = Get-Item -LiteralPath $dstExe
    $dstItem.LastWriteTimeUtc = $stamp
    Check 'другой размер -> копировать' `
          (Test-RootBinaryNeedsCopy -Source $srcExe -Destination $dstExe) $true

    # Тот же размер, но другое время изменения.
    Install-RootBinary -Source $srcExe -Destination $dstExe
    Check 'повторная установка перезаписывает файл' `
          (Test-RootBinaryNeedsCopy -Source $srcExe -Destination $dstExe) $false
    $dstItem = Get-Item -LiteralPath $dstExe
    $dstItem.LastWriteTimeUtc = $stamp.AddMinutes(-5)
    Check 'другое время изменения -> копировать' `
          (Test-RootBinaryNeedsCopy -Source $srcExe -Destination $dstExe) $true

    # Подметание хвостов: они остаются и тогда, когда копировать ничего не нужно,
    # поэтому чистка живёт в Publish-RootBinary, а не внутри Install-RootBinary.
    Install-RootBinary -Source $srcExe -Destination $dstExe
    Set-Content -LiteralPath "$dstExe.part"          -Value 'обрывок' -NoNewline
    Set-Content -LiteralPath "$dstExe.old-deadbeef"  -Value 'старое'  -NoNewline
    Set-Content -LiteralPath "$dstExe.old-cafebabe"  -Value 'старое'  -NoNewline
    Clear-RootBinaryLeftovers -Destination $dstExe
    Check 'хвост .part подметён'   (Test-Path -LiteralPath "$dstExe.part") $false
    Check 'все .old-* подметены'   (@(Get-ChildItem -LiteralPath $rb -Filter 'root.bin.old-*' -File)).Count 0
    Check 'сама программа не тронута' (Test-Path -LiteralPath $dstExe) $true
    Check 'исходник не тронут'         (Test-Path -LiteralPath $srcExe) $true

    } finally { $ErrorActionPreference = $eapPrev }

    # ---------------------------------------------------------------------
    Note 'Запуск файла из корня: аргументы и код возврата'
    # ---------------------------------------------------------------------

    # Заглушка вместо приложения: пишет каждый полученный аргумент отдельной
    # строкой в MS_ARGS_FILE и возвращает код из MS_FAKE_RC. Пути передаются
    # через переменные среды, чтобы тело заглушки осталось чистым ASCII и не
    # зависело от кодовой страницы.
    #
    # ЧЕГО ЭТОТ ТЕСТ НЕ ПРОВЕРЯЕТ: заглушка — консольная программа, а настоящее
    # приложение на Windows собрано для GUI-подсистемы, и оператор вызова таких
    # процессов НЕ ждёт. Ожидание обеспечивает конвейер `| Out-Host` в
    # Invoke-RootBinary; проверить это можно только настоящим GUI-бинарником.
    $argsFile = Join-Path $rb 'appargs.txt'
    if ($env:OS -eq 'Windows_NT') {
        $fakeApp = Join-Path $rb 'fakeapp.cmd'
        Set-Content -LiteralPath $fakeApp -Encoding Ascii -Value @(
            '@echo off',
            'if exist "%MS_ARGS_FILE%" del "%MS_ARGS_FILE%"',
            ':loop',
            'if "%~1"=="" goto done',
            '>>"%MS_ARGS_FILE%" echo %~1',
            'shift',
            'goto loop',
            ':done',
            'exit /b %MS_FAKE_RC%')
    } else {
        $fakeApp = Join-Path $rb 'fakeapp.sh'
        Set-Content -LiteralPath $fakeApp -Encoding Ascii -Value @(
            '#!/bin/sh',
            ': > "$MS_ARGS_FILE"',
            'for a in "$@"; do printf ''%s\n'' "$a" >> "$MS_ARGS_FILE"; done',
            'exit "${MS_FAKE_RC:-0}"')
        $chmod = Get-Command chmod -ErrorAction SilentlyContinue
        if ($chmod) { & $chmod.Source '+x' $fakeApp }
    }

    $env:MS_ARGS_FILE = $argsFile
    $env:MS_FAKE_RC   = '0'
    $script:RootExe     = $fakeApp
    $script:LastRunCode = -1

    # Аргумент с пробелом — главный риск регрессии: Start-Process -ArgumentList
    # склеил бы массив без кавычек и разорвал бы его на два аргумента.
    Invoke-RootBinary -ApplicationArgs @('--ignore-installed', '--project', 'C:\my projects\ch 01')
    $passed = @(Get-Content -LiteralPath $argsFile)
    Check 'аргументов ровно столько, сколько передали' $passed.Count 3
    Check 'первый аргумент дошёл'   $passed[0] '--ignore-installed'
    Check 'второй аргумент дошёл'   $passed[1] '--project'
    Check 'аргумент с пробелом не распался на два' $passed[2] 'C:\my projects\ch 01'
    Check 'нулевой код возврата'    $script:LastRunCode 0

    $env:MS_FAKE_RC = '42'
    Invoke-RootBinary -ApplicationArgs @('--ignore-installed')
    Check 'код возврата приложения пробрасывается' $script:LastRunCode 42
    Check 'а не остаётся от прошлой команды' `
          (@(Get-Content -LiteralPath $argsFile))[0] '--ignore-installed'

    $env:MS_FAKE_RC = '0'
    Invoke-RootBinary -ApplicationArgs @()
    Check 'пустой список аргументов не ломает запуск' $script:LastRunCode 0

    # Незапускаемый файл: код 126, как на POSIX-стороне, а не стек-трейс.
    $script:RootExe     = Join-Path $rb 'no-such-file.bin'
    $script:LastRunCode = -1
    Invoke-RootBinary -ApplicationArgs @('--ignore-installed')
    Check 'незапускаемый файл -> код 126' $script:LastRunCode 126

    Remove-Item Env:MS_ARGS_FILE -ErrorAction SilentlyContinue
    Remove-Item Env:MS_FAKE_RC   -ErrorAction SilentlyContinue

    # ---------------------------------------------------------------------
    # Пробел в пути к C-тулчейну
    # ---------------------------------------------------------------------
    Note 'Пробел в пути к C-тулчейну'

    Check 'короткое имя: несуществующий путь -> $null' `
          (Get-ShortPathName -Path (Join-Path $Sandbox 'nope')) $null
    Check 'короткое имя: пустая строка -> $null' (Get-ShortPathName -Path '') $null

    # Версия GCC определяется по дереву тулчейна, а не константой: winlibs
    # обновляется, и зашитый номер молча указывал бы в никуда.
    $mg = Join-Path $Sandbox 'mg'
    $verParent = Join-Path $mg 'lib\gcc\x86_64-w64-mingw32'
    Check 'версия GCC: дерева нет -> $null' (Get-MingwGccVersionDir -Dir $mg) $null
    [void](New-Item -ItemType Directory -Path (Join-Path $verParent '16.2.0') -Force)
    Check 'версия GCC: одна папка -> её имя' (Get-MingwGccVersionDir -Dir $mg) '16.2.0'
    [void](New-Item -ItemType Directory -Path (Join-Path $verParent '15.1.0') -Force)
    Check 'версия GCC: неоднозначно -> $null' (Get-MingwGccVersionDir -Dir $mg) $null
    Remove-Item -LiteralPath (Join-Path $verParent '15.1.0') -Recurse -Force

    # Путь БЕЗ пробела: трогается только PATH, переменные gcc не выставляются.
    $plain = Join-Path $Sandbox 'mingw64'
    if ($plain -match ' ') {
        Write-Host '  SKIP временный каталог машины содержит пробел' -ForegroundColor Yellow
    } else {
        $mgBin = Join-Path $plain 'bin'
        [void](New-Item -ItemType Directory -Path $mgBin -Force)

        $pathBefore = $env:PATH
        Remove-Item Env:GCC_EXEC_PREFIX, Env:LIBRARY_PATH -ErrorAction SilentlyContinue
        Set-MingwEnvironment -Dir $plain
        Check 'PATH начинается с bin тулчейна' (@($env:PATH -split ';')[0]) $mgBin
        Check 'без пробела GCC_EXEC_PREFIX не нужен' $env:GCC_EXEC_PREFIX $null
        Check 'без пробела LIBRARY_PATH не нужен'    $env:LIBRARY_PATH    $null

        # Повторный вызов: скрипт зовёт функцию и из Assert-CToolchain, и из
        # Install-Mingw.
        $lenOnce = $env:PATH.Length
        Set-MingwEnvironment -Dir $plain
        Check 'повторный вызов не дублирует запись в PATH' $env:PATH.Length $lenOnce
        $env:PATH = $pathBefore
    }

    # Ранняя проверка: корень без пробела -> вопрос не стоит, Die не вызывается.
    $rootBefore = $script:RepoRoot
    $script:RepoRoot = $Sandbox
    $script:LastDieCode = 0
    try { Assert-ToolchainPathUsable } catch { }
    Check 'корень без пробела: ранняя проверка молчит' $script:LastDieCode 0

    # Путь С пробелом: на Windows должны появиться пути поиска в форме 8.3.
    # Именно они, а не способ вызова gcc, лечат разрыв spec-путей — проверено на
    # winlibs GCC 16.2.0: короткий путь запуска, subst и junction не помогают.
    if ($env:OS -eq 'Windows_NT') {
        $spacedRoot = Join-Path $Sandbox 'dir with space'
        $spaced     = Join-Path $spacedRoot 'mingw64'
        [void](New-Item -ItemType Directory -Path (Join-Path $spaced 'bin') -Force)
        [void](New-Item -ItemType Directory -Path (Join-Path $spaced 'x86_64-w64-mingw32\lib') -Force)
        [void](New-Item -ItemType Directory -Path (Join-Path $spaced 'lib\gcc\x86_64-w64-mingw32\16.2.0') -Force)

        $pathBefore = $env:PATH
        Remove-Item Env:GCC_EXEC_PREFIX, Env:LIBRARY_PATH -ErrorAction SilentlyContinue
        $script:LastDieCode = 0
        try { Set-MingwEnvironment -Dir $spaced } catch { }
        Check 'путь с пробелом не завершает работу' $script:LastDieCode 0
        Check 'GCC_EXEC_PREFIX выставлен' ($null -ne $env:GCC_EXEC_PREFIX) $true
        Check 'в GCC_EXEC_PREFIX нет пробела' ($env:GCC_EXEC_PREFIX -match ' ') $false
        Check 'GCC_EXEC_PREFIX кончается разделителем' ($env:GCC_EXEC_PREFIX -match '\\$') $true
        Check 'GCC_EXEC_PREFIX указывает на существующий каталог' `
              (Test-Path -LiteralPath $env:GCC_EXEC_PREFIX) $true
        Check 'LIBRARY_PATH выставлен' ($null -ne $env:LIBRARY_PATH) $true
        Check 'в LIBRARY_PATH нет пробела' ($env:LIBRARY_PATH -match ' ') $false
        $libDirs = @($env:LIBRARY_PATH -split ';')
        Check 'LIBRARY_PATH: обе ветки поиска' $libDirs.Count 2
        Check 'LIBRARY_PATH: каталоги существуют' `
              (@($libDirs | Where-Object { -not (Test-Path -LiteralPath $_) }).Count) 0
        Check 'версия попала в LIBRARY_PATH' ($env:LIBRARY_PATH -match '16\.2\.0') $true

        $env:PATH = $pathBefore
        Remove-Item Env:GCC_EXEC_PREFIX, Env:LIBRARY_PATH -ErrorAction SilentlyContinue
    }
    $script:RepoRoot = $rootBefore

} finally {
    Set-Location ([System.IO.Path]::GetTempPath())
    Remove-Item -Recurse -Force $Sandbox -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host "Итого: $($script:Pass) пройдено, $($script:Fail) провалено"
if ($script:Fail -ne 0) { exit 1 }
