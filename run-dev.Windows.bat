@echo off
rem ---------------------------------------------------------------------------
rem run-dev.Windows.bat - launcher for the dev build of ManhwaStudio.
rem
rem Contains no logic beyond handing control to tools\run-dev\run-dev.ps1 with
rem -ExecutionPolicy Bypass, because the Windows default policy (Restricted /
rem AllRestricted) blocks a double-clicked .ps1 outright. Bypass applies to this
rem process only; the machine's policy is not changed.
rem
rem Deliberately ASCII-only. cmd.exe interprets a batch file in the console code
rem page, so Cyrillic here would render as mojibake depending on the machine.
rem Every user-facing message is printed by the PowerShell script instead, which
rem sets UTF-8 output itself.
rem
rem The whole executable tail is ONE parenthesised block, and that is load
rem bearing. cmd.exe reads a .bat incrementally, by byte offset, while it runs -
rem and run-dev updates the working copy, this file included. A block is parsed
rem and cached in full before its first command executes, so an update landing
rem mid-run cannot make cmd resume at an offset that no longer means anything.
rem Delayed expansion is required inside it: every %VAR% in a block is
rem substituted at parse time, i.e. before powershell has even started, so the
rem exit code must be read as !RC!, not %RC%.
rem
rem Known cost of that setting: exclamation marks are consumed. Delayed
rem expansion is applied to the whole command line after %-substitution, so an
rem exclamation mark is lost both in the project path and in the forwarded
rem arguments - "run-dev.Windows.bat -- --project C:\a!b" does not survive.
rem Neither case is worth reintroducing the corruption the block prevents.
rem
rem This file must keep CRLF line endings; the rule is enforced by the root
rem .gitattributes. cmd.exe is only specified for CRLF batch files, and
rem multi-line blocks are exactly where LF-only files are known to misparse -
rem which is the one thing this layout depends on.
rem
rem All arguments are forwarded, e.g.:  run-dev.Windows.bat -NoUpdate
rem ---------------------------------------------------------------------------

setlocal enabledelayedexpansion

(
    cd /D "%~dp0"

    set "PS1=%~dp0tools\run-dev\run-dev.ps1"

    if not exist "!PS1!" (
        echo [ERROR] Not found: !PS1!
        echo The launcher must stay in the ManhwaStudio project root.
        pause
        exit /b 1
    )

    where powershell >nul 2>&1
    if errorlevel 1 (
        echo [ERROR] Windows PowerShell was not found on this system.
        echo Install PowerShell, or run the project manually with: cargo run --release
        pause
        exit /b 1
    )

    powershell -NoProfile -ExecutionPolicy Bypass -File "!PS1!" %*
    set "RC=!ERRORLEVEL!"

    rem The PowerShell script pauses on its own failures; this catches the cases
    rem where it could not start at all and the window would vanish silently.
    if not "!RC!"=="0" (
        echo.
        echo [exit code !RC!]
        pause
    )

    exit /b !RC!
)
