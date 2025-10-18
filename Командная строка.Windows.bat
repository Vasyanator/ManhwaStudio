@echo off
REM Переходим в папку, где лежит сам скрипт
cd /d "%~dp0"

REM Открываем новое окно cmd в этой папке
start cmd /K "cd /d %~dp0"