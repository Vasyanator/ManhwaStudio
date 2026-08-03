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
rem All arguments are forwarded, e.g.:  run-dev.Windows.bat -NoUpdate
rem ---------------------------------------------------------------------------

setlocal
cd /D "%~dp0"

set "PS1=%~dp0tools\run-dev\run-dev.ps1"

if not exist "%PS1%" (
    echo [ERROR] Not found: %PS1%
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

powershell -NoProfile -ExecutionPolicy Bypass -File "%PS1%" %*
set "RC=%ERRORLEVEL%"

rem The PowerShell script pauses on its own failures; this catches the cases
rem where it could not start at all and the window would vanish silently.
if not "%RC%"=="0" (
    echo.
    echo [exit code %RC%]
    pause
)

exit /b %RC%
