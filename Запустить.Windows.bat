@echo off
setlocal

REM Переходим в папку со скриптом
cd /D "%~dp0"

REM Имя запускаемого python-файла (можно поменять)
set "APP_SCRIPT=mainV2.py"

REM 1) Вариант с portable-окружением (если есть)
if exist "portable_env\python.exe" (
    .\portable_env\python.exe "%APP_SCRIPT%" %*
    exit /b %errorlevel%
)

REM 2) Вариант с conda/Miniforge окружением
set "INSTALL_ENV_DIR=%cd%\installer_files\env"
set "CONDA_ROOT_PREFIX=%cd%\installer_files\conda"

REM Проверяем, что окружение существует
if not exist "%INSTALL_ENV_DIR%\python.exe" (
    echo [ERROR] Python окружение не найдено: "%INSTALL_ENV_DIR%"
    echo Создайте его и запустите скрипт снова.
    pause
    exit /b 1
)

REM Активируем окружение
call "%CONDA_ROOT_PREFIX%\condabin\conda.bat" activate "%INSTALL_ENV_DIR%"
if errorlevel 1 (
    echo [ERROR] Не удалось активировать окружение: "%INSTALL_ENV_DIR%"
    pause
    exit /b 1
)

REM Запуск приложения
python "%APP_SCRIPT%" %*
set "rc=%errorlevel%"

REM (необязательно) деактивация окружения
(call conda deactivate) >nul 2>&1

exit /b %rc%
