from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/save_ops.py
Save/export helpers for "New Project" window.

Main items:
- `prepare_project_dirs` / `save_project`: save current pages as chapter source files.
- `on_save_alt_version`: save current pages into `alt_vers`.
- All project paths resolve from `user_config.json` (`General.projects_dir`).
"""

import os
import subprocess
import sys
from pathlib import Path

from PIL import Image
from PyQt6 import QtCore, QtWidgets

from config import NOTES_FILE, SRC_DIR, get_projects_root


def _resolve_qt_runner(qt_entry: str) -> Path:
    """
    Возвращает абсолютный путь к qt-раннеру.
    По умолчанию ожидаем, что файл лежит рядом с корнем проекта (на уровень выше modules/).
    Можно передать абсолютный путь или относительный к корню проекта.
    """
    if os.path.isabs(qt_entry):
        return Path(qt_entry)
    # этот файл: .../modules/new_project/save_ops.py -> корень: на два уровня выше
    project_root = Path(__file__).resolve().parents[2]
    return project_root / qt_entry


def prepare_project_dirs(title: str, chapter: str) -> tuple[Path, Path]:
    title_dir = Path(get_projects_root()) / title
    project_path = title_dir / chapter
    src_dir = project_path / SRC_DIR
    src_dir.mkdir(parents=True, exist_ok=True)
    notes_path = title_dir / NOTES_FILE
    if not notes_path.exists():
        title_dir.mkdir(parents=True, exist_ok=True)
        with open(notes_path, "a", encoding="utf-8"):
            pass
    return project_path, src_dir


def clear_dir_contents(dst_dir: Path) -> None:
    for p in dst_dir.glob("*"):
        try:
            p.unlink()
        except Exception:
            pass


def confirm_overwrite_nonempty(window, dst_dir: Path) -> bool:
    try:
        has_files = dst_dir.exists() and any(dst_dir.iterdir())
    except Exception:
        has_files = False
    if not has_files:
        return True
    msg = QtWidgets.QMessageBox(window)
    msg.setIcon(QtWidgets.QMessageBox.Icon.Warning)
    msg.setWindowTitle("Внимание")
    msg.setText("Папка не пустая. Перезаписать файлы?")
    btn_overwrite = msg.addButton("Перезаписать", QtWidgets.QMessageBox.ButtonRole.AcceptRole)
    btn_cancel = msg.addButton("Отмена", QtWidgets.QMessageBox.ButtonRole.RejectRole)
    msg.setDefaultButton(btn_cancel)
    msg.exec()
    return msg.clickedButton() == btn_overwrite


def save_canvas_pngs(window, dst_dir: Path) -> int:
    images = window._current_images_pil or window._opened_images_pil
    if not images:
        return 0
    dst_dir.mkdir(parents=True, exist_ok=True)
    count = 0
    images_num = len(images)
    for i, im in enumerate(images, 1):
        try:
            window._set_progress("Сохраняем...", i, images_num)
            im.save(dst_dir / f"{i:03d}.png", format="PNG")
            count += 1
        except Exception:
            pass
    window._set_progress("Готово", 1, 1)
    return count


def load_images_from_dir(dirpath: Path):
    exts = (".png", ".jpg", ".jpeg", ".webp")
    files = sorted([p for p in dirpath.iterdir() if p.suffix.lower() in exts])
    out = []
    for p in files:
        try:
            out.append(Image.open(p).convert("RGB"))
        except Exception:
            continue
    return out


def save_project(window, open_after: bool) -> None:
    title = (window.cmbTitles.currentText() or "").strip()
    chapter = (window.edChapter.text() or "").strip()
    if not title or not chapter:
        QtWidgets.QMessageBox.warning(window, "Внимание", "Укажите тайтл и название главы.")
        return
    if not (window._current_images_pil or window._opened_images_pil):
        QtWidgets.QMessageBox.warning(window, "Нет данных", "На холсте нет изображений для сохранения.")
        return

    try:
        project_path, src_dir = prepare_project_dirs(title, chapter)
    except Exception as e:
        QtWidgets.QMessageBox.critical(window, "Ошибка", f"Не удалось подготовить папки проекта:\n{e}")
        return

    if not confirm_overwrite_nonempty(window, src_dir):
        return

    window._set_progress("Сохранение…", 0, 0, pulse=True)
    QtWidgets.QApplication.setOverrideCursor(QtCore.Qt.CursorShape.BusyCursor)
    try:
        clear_dir_contents(src_dir)
        saved = save_canvas_pngs(window, src_dir)
        if saved == 0:
            raise RuntimeError("Не удалось сохранить изображения (0 файлов).")
    except Exception as e:
        QtWidgets.QApplication.restoreOverrideCursor()
        window._set_progress("", 0, 0)
        QtWidgets.QMessageBox.critical(window, "Ошибка сохранения", str(e))
        return

    QtWidgets.QApplication.restoreOverrideCursor()
    if not open_after:
        window._set_progress("", 0, 0)
        QtWidgets.QMessageBox.information(window, "Готово", "Сохранено в проект.")
        return

    window._set_progress("Открываю проект…", 1, 1)
    qt_runner = _resolve_qt_runner(window._qt_entry)
    if not qt_runner.exists():
        window._set_progress("", 0, 0)
        QtWidgets.QMessageBox.critical(
            window,
            "Ошибка",
            f"Не найден запускной файл:\n{qt_runner}",
        )
        return

    on_open_project = getattr(window, "_on_open_project", None)
    if callable(on_open_project):
        try:
            on_open_project(os.fspath(project_path), window._qt_entry)
        except TypeError:
            on_open_project(os.fspath(project_path))
        except Exception as e:
            window._set_progress("", 0, 0)
            QtWidgets.QMessageBox.critical(window, "Ошибка запуска", str(e))
            return
        window._set_progress("", 0, 0)
        try:
            window.accept()
        except Exception:
            pass
        return

    argv = [sys.executable, os.fspath(qt_runner), "--project", os.fspath(project_path)]
    cwd_path = qt_runner.parent

    print("Launching:", repr(argv), file=sys.stderr, flush=True)
    try:
        popen_kwargs = {"cwd": os.fspath(cwd_path)}
        if os.name == "nt":
            popen_kwargs["close_fds"] = False
        else:
            popen_kwargs["start_new_session"] = True
        subprocess.Popen(argv, **popen_kwargs)
    except Exception as e:
        window._set_progress("", 0, 0)
        QtWidgets.QMessageBox.critical(window, "Ошибка запуска", str(e))
        return
    window._set_progress("", 0, 0)


def on_save_and_open(window) -> None:
    save_project(window, open_after=True)


def on_save_to_project(window) -> None:
    save_project(window, open_after=False)


def on_save_alt_version(window) -> None:
    title = (window.cmbAltTitle.currentText() or "").strip()
    chapter = (window.cmbAltChapter.currentText() or "").strip()
    alt_name = (window.edAltName.text() or "").strip()
    if not title or not chapter or not alt_name:
        QtWidgets.QMessageBox.warning(window, "Внимание", "Укажите тайтл, главу и название альтер-версии.")
        return
    if not (window._current_images_pil or window._opened_images_pil):
        QtWidgets.QMessageBox.warning(window, "Нет данных", "На холсте нет изображений для сохранения.")
        return

    alt_dir = Path(get_projects_root()) / title / chapter / "alt_vers" / alt_name
    try:
        alt_dir.mkdir(parents=True, exist_ok=True)
    except Exception as e:
        QtWidgets.QMessageBox.critical(window, "Ошибка", f"Не удалось создать папку:\n{e}")
        return

    if not confirm_overwrite_nonempty(window, alt_dir):
        return

    window._set_progress("Сохранение…", 0, 0, pulse=True)
    QtWidgets.QApplication.setOverrideCursor(QtCore.Qt.CursorShape.BusyCursor)
    try:
        clear_dir_contents(alt_dir)
        saved = save_canvas_pngs(window, alt_dir)
        if saved == 0:
            raise RuntimeError("Не удалось сохранить изображения (0 файлов).")
    except Exception as e:
        QtWidgets.QApplication.restoreOverrideCursor()
        window._set_progress("", 0, 0)
        QtWidgets.QMessageBox.critical(window, "Ошибка сохранения", str(e))
        return

    QtWidgets.QApplication.restoreOverrideCursor()
    window._set_progress("", 0, 0)
    QtWidgets.QMessageBox.information(window, "Готово", "Сохранено как альтер-версия.")


def on_save_to_folder(window) -> None:
    imgs = window._current_images_pil or window._opened_images_pil
    if not imgs:
        QtWidgets.QMessageBox.information(window, "Нет данных", "На холсте нет изображений для сохранения.")
        return
    folder = QtWidgets.QFileDialog.getExistingDirectory(window, "Выберите папку для сохранения")
    if not folder:
        return

    total = len(imgs)
    window._set_progress("Сохраняю изображения…", 0, total)
    saved = save_canvas_pngs(window, Path(folder))
    window._set_progress("", 0, 0)
    if saved == 0:
        QtWidgets.QMessageBox.warning(window, "Не сохранено", "Не удалось сохранить ни одного изображения.")
    else:
        QtWidgets.QMessageBox.information(window, "Готово", f"Сохранено файлов: {saved}\nПапка: {folder}")


__all__ = [
    "prepare_project_dirs",
    "clear_dir_contents",
    "confirm_overwrite_nonempty",
    "save_canvas_pngs",
    "load_images_from_dir",
    "save_project",
    "on_save_and_open",
    "on_save_to_project",
    "on_save_alt_version",
    "on_save_to_folder",
]
