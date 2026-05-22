from __future__ import annotations

import shutil
import subprocess
import sys
import traceback
from pathlib import Path
from typing import Optional

import numpy as np
from PIL import Image
from PyQt6 import QtCore, QtWidgets

from . import save_ops

_W2X_CLASS = None
_W2X_PY_MODULES = []
_HAS_W2X_PY = False


def w2xpy_make_engine(noise: int, scale: int, tile: int, gpuid: int = 0, model: str | None = None):
    if not _HAS_W2X_PY:
        raise RuntimeError("waifu2x-ncnn-py не найден")

    last_err = None
    for m in _W2X_PY_MODULES:
        try:
            if hasattr(m, "Waifu2x"):
                ctor_variants = (
                    dict(gpuid=gpuid, noise=noise, scale=scale, tilesize=tile, model=model or "models-cunet"),
                    dict(noise=noise, scale=scale, tilesize=tile),
                    dict(n=noise, s=scale, t=tile, gpuid=gpuid),
                    dict(noise=noise, scale=scale, tile=tile, gpuid=gpuid),
                )
                for ctor_kwargs in ctor_variants:
                    try:
                        eng = m.Waifu2x(**ctor_kwargs)
                        if hasattr(eng, "process_pil"):
                            return eng, lambda e, im: e.process_pil(im)
                        if hasattr(eng, "process"):
                            return eng, lambda e, im: e.process(im)
                        if hasattr(eng, "__call__"):
                            return eng, lambda e, im: e(im)
                    except TypeError as te:
                        last_err = te
                        continue
        except Exception as e:
            last_err = e

        for fname in ("process_pil", "process", "upscale", "run"):
            fn = getattr(m, fname, None)
            if callable(fn):
                def _wrap(_e, im, _fn=fn):
                    return _fn(im, noise=noise, scale=scale, tilesize=tile)

                return object(), _wrap

    raise RuntimeError(f"Не удалось инициализировать waifu2x-ncnn-py ({last_err})")


def waifu2x_python_run_list(
    pil_list: list[Image.Image],
    noise: int,
    scale: int,
    tile: int,
    progress_cb: Optional[callable] = None,
) -> list[Image.Image]:
    if not pil_list:
        return []
    eng, call = w2xpy_make_engine(noise=noise, scale=scale, tile=tile)
    out = []
    total = len(pil_list)
    for i, im in enumerate(pil_list, 1):
        try:
            res = call(eng, im.convert("RGB"))
            if isinstance(res, Image.Image):
                out.append(res.convert("RGB"))
            else:
                arr = np.array(res)
                if arr.ndim == 2:
                    arr = np.stack([arr, arr, arr], axis=-1)
                out.append(Image.fromarray(arr.astype("uint8")).convert("RGB"))
        except Exception:
            traceback.print_exc()
        if callable(progress_cb):
            progress_cb(i, total)
    return out


def waifu2x_exec_path(window) -> Path:
    if sys.platform.startswith("win"):
        p = window._program_dir / "waifu2x" / "Win" / "waifu2x-ncnn-vulkan.exe"
    elif sys.platform.startswith("darwin"):
        p = window._program_dir / "waifu2x" / "Mac" / "waifu2x-ncnn-vulkan"
    else:
        p = window._program_dir / "waifu2x" / "Lin" / "waifu2x-ncnn-vulkan"
    return p


def ensure_waifu_temp_dirs(window, clean: bool = True) -> None:
    window._waifu_in.mkdir(parents=True, exist_ok=True)
    window._waifu_out.mkdir(parents=True, exist_ok=True)
    if clean:
        for d in (window._waifu_in, window._waifu_out):
            for p in d.iterdir():
                try:
                    p.unlink() if p.is_file() else shutil.rmtree(p, ignore_errors=True)
                except Exception:
                    pass


def on_run_waifu2x(window) -> None:
    if not window._current_images_pil and window._opened_images_pil:
        window._current_images_pil = list(window._opened_images_pil)
    if not window._current_images_pil:
        QtWidgets.QMessageBox.warning(window, "Нет данных", "Сначала откройте или скачайте изображения.")
        return

    try:
        n = int(window.cmbW2xN.currentText())
        assert n in (-1, 0, 1, 2, 3)
        s = int(window.cmbW2xS.currentText())
        assert s in (1, 2, 4, 8, 16, 32)
        t = int(window.edW2xT.text())
        assert t == 0 or t >= 32
    except Exception:
        QtWidgets.QMessageBox.critical(window, "Параметры", "Проверьте -n, -s и -t (t == 0 или t >= 32).")
        return

    if _HAS_W2X_PY:
        window._set_progress("waifu2x (Python) обрабатывает…", 0, len(window._current_images_pil))
        QtWidgets.QApplication.setOverrideCursor(QtCore.Qt.CursorShape.BusyCursor)
        try:
            def pcb(i, total):
                QtCore.QMetaObject.invokeMethod(
                    window,
                    "_set_progress",
                    QtCore.Qt.ConnectionType.QueuedConnection,
                    QtCore.Q_ARG(str, f"waifu2x (Python) {i}/{total}"),
                    QtCore.Q_ARG(int, i),
                    QtCore.Q_ARG(int, total),
                    QtCore.Q_ARG(bool, False),
                )

            out = waifu2x_python_run_list(window._current_images_pil, noise=n, scale=s, tile=t, progress_cb=pcb)
            if not out:
                QtWidgets.QMessageBox.warning(window, "waifu2x", "Библиотека не вернула изображения.")
                return
            window._current_images_pil = out
            window.viewer.set_images(window._current_images_pil)
            window._set_progress("Готово", 1, 1)
        except Exception as e:
            traceback.print_exc()
            QtWidgets.QMessageBox.critical(window, "waifu2x (Python)", str(e))
        finally:
            QtWidgets.QApplication.restoreOverrideCursor()
        return

    exec_path = Path(window.edW2xPath.text().strip()) if window.edW2xPath.text().strip() else waifu2x_exec_path(window)
    if not exec_path.exists():
        exec_path = waifu2x_exec_path(window)
    window.edW2xPath.setText(str(exec_path))
    if not exec_path.exists():
        QtWidgets.QMessageBox.critical(window, "waifu2x", f"Не найден исполняемый файл:\n{exec_path}")
        return

    ensure_waifu_temp_dirs(window, clean=True)
    if save_ops.save_canvas_pngs(window, window._waifu_in) == 0:
        QtWidgets.QMessageBox.warning(window, "waifu2x", "Не удалось сохранить входные изображения.")
        return

    window._set_progress("waifu2x (exe) обрабатывает…", 0, 0, pulse=True)
    window.setCursor(QtCore.Qt.CursorShape.BusyCursor)
    QtWidgets.QApplication.processEvents()

    cmd = [
        str(exec_path),
        "-i",
        str(window._waifu_in),
        "-o",
        str(window._waifu_out),
        "-n",
        str(n),
        "-s",
        str(s),
        "-t",
        str(t),
    ]
    try:
        _proc = subprocess.run(cmd, capture_output=True, text=True, check=True)
    except subprocess.CalledProcessError as e:
        window.unsetCursor()
        window._set_progress("", 0, 0)
        err = (e.stderr or e.stdout or str(e)).strip()
        QtWidgets.QMessageBox.critical(window, "waifu2x (ошибка выполнения)", err[:4000])
        ensure_waifu_temp_dirs(window, clean=True)
        return
    except Exception as e:
        window.unsetCursor()
        window._set_progress("", 0, 0)
        QtWidgets.QMessageBox.critical(window, "waifu2x", str(e))
        ensure_waifu_temp_dirs(window, clean=True)
        return

    try:
        window._set_progress("Загружаем...", 0, 1)
        pil_out = save_ops.load_images_from_dir(window._waifu_out)
        if not pil_out:
            raise RuntimeError("Папка out пуста: waifu2x не сгенерировал изображения.")
        window._current_images_pil = pil_out
        window.viewer.set_images(window._current_images_pil)
        window._set_progress("Готово", 1, 1)
    except Exception as e:
        QtWidgets.QMessageBox.critical(window, "waifu2x (загрузка результата)", str(e))
    finally:
        ensure_waifu_temp_dirs(window, clean=True)
        window.unsetCursor()


__all__ = [
    "_HAS_W2X_PY",
    "w2xpy_make_engine",
    "waifu2x_python_run_list",
    "waifu2x_exec_path",
    "ensure_waifu_temp_dirs",
    "on_run_waifu2x",
]
