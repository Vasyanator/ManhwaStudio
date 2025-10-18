@echo off
setlocal

REM Переходим в директорию скрипта
cd /D "%~dp0"

REM Пути к окружению
set "INSTALL_ENV_DIR=%cd%\installer_files\env"
set "CONDA_ROOT_PREFIX=%cd%\installer_files\conda"

REM Проверяем, что окружение существует
if not exist "%INSTALL_ENV_DIR%\python.exe" (
    echo [ERROR] Окружение не найдено: "%INSTALL_ENV_DIR%"
    echo Создайте его перед запуском.
    pause
    exit /b 1
)

REM Проверяем, что conda доступна
if not exist "%CONDA_ROOT_PREFIX%\condabin\conda.bat" (
    echo [ERROR] Не найден файл активации conda: "%CONDA_ROOT_PREFIX%\condabin\conda.bat"
    pause
    exit /b 1
)

REM Активируем окружение и открываем интерактивную консоль
call "%CONDA_ROOT_PREFIX%\condabin\conda.bat" activate "%INSTALL_ENV_DIR%"
if errorlevel 1 (
    echo [ERROR] Не удалось активировать окружение.
    pause
    exit /b 1
)

echo.
echo ============================================================
echo  Окружение успешно активировано:
echo     %INSTALL_ENV_DIR%
echo  Можно использовать python, pip и т.д.
echo ============================================================
echo.

REM Открываем интерактивную командную строку в этом окружении
cmd /K
