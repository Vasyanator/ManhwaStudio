from __future__ import annotations

import hashlib
import os
import threading
import traceback
import urllib.request
from typing import Optional

import cv2
import numpy as np
import torch
from PyQt6.QtCore import Qt, pyqtSignal
from PyQt6.QtGui import QImage
from PyQt6.QtWidgets import (
    QFormLayout,
    QGroupBox,
    QLabel,
    QPushButton,
    QSpinBox, QWidget
)

from config import LAMA_MPE_DIR, program_dir
from modules.lama_mpe import load_lama_mpe
from .base import RegionEditTool, RegionEditorDialog, numpy_rgb_to_qimage

# ---------- Модель и загрузка ----------
_INPAINTER = None
_INPAINTER_DEVICE = None
_INPAINTER_LOCK = threading.Lock()

_LAMA_MPE_URL = "https://github.com/zyddnys/manga-image-translator/releases/download/beta-0.3/inpainting_lama_mpe.ckpt"
_LAMA_MPE_SHA = "d625aa1b3e0d0408acfd6928aa84f005867aa8dbb9162480346a4e20660786cc"
_LAMA_MPE_FILENAME = "inpainting_lama_mpe.ckpt"


def _sha256(fname: str) -> str:
    h = hashlib.sha256()
    with open(fname, "rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _download_with_progress(url: str, dst: str):
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    tmp = dst + ".part"
    with urllib.request.urlopen(url) as resp, open(tmp, "wb") as f:
        while True:
            chunk = resp.read(8192)
            if not chunk:
                break
            f.write(chunk)
    os.replace(tmp, dst)


def _ensure_weights() -> str:
    ckpt_path = os.path.join(LAMA_MPE_DIR, _LAMA_MPE_FILENAME)
    if os.path.isfile(ckpt_path):
        try:
            if _sha256(ckpt_path) == _LAMA_MPE_SHA:
                return ckpt_path
        except Exception:
            pass
    _download_with_progress(_LAMA_MPE_URL, ckpt_path)
    if _sha256(ckpt_path) != _LAMA_MPE_SHA:
        raise RuntimeError("Повреждена скачанная lama_mpe.ckpt (SHA256 mismatch)")
    return ckpt_path


def _validate_repo():
    repo_path = os.path.join(program_dir, "lama_modernised")
    if not os.path.isdir(repo_path):
        raise FileNotFoundError("Папка lama_modernised не найдена. Установите репозиторий в корень программы.")


def _get_inpainter(device: str):
    global _INPAINTER
    global _INPAINTER_DEVICE
    with _INPAINTER_LOCK:
        if _INPAINTER is None or _INPAINTER_DEVICE != device:
            if _INPAINTER is not None:
                try:
                    _INPAINTER.unload()
                except Exception:
                    pass
                _INPAINTER = None
            _validate_repo()
            ckpt = _ensure_weights()
            _INPAINTER = LamaMPEWrapper(ckpt, device=device)
            _INPAINTER_DEVICE = device
        return _INPAINTER


def _unload_inpainter():
    global _INPAINTER
    global _INPAINTER_DEVICE
    with _INPAINTER_LOCK:
        if _INPAINTER is None:
            return False
        try:
            _INPAINTER.unload()
        finally:
            _INPAINTER = None
            _INPAINTER_DEVICE = None
        return True


# ---------- Вспомогательные функции обработки ----------
def resize_keepasp(im, new_shape=640, scaleup=True, interpolation=cv2.INTER_LINEAR, stride=None):
    shape = im.shape[:2]
    if new_shape is not None:
        if not isinstance(new_shape, tuple):
            new_shape = (new_shape, new_shape)
    else:
        new_shape = shape
    r = min(new_shape[0] / shape[0], new_shape[1] / shape[1])
    if not scaleup:
        r = min(r, 1.0)
    new_unpad = int(round(shape[1] * r)), int(round(shape[0] * r))
    if stride is not None:
        h, w = new_unpad
        new_h = (stride - (h % stride) + h) if h % stride != 0 else h
        new_w = (stride - (w % stride) + w) if w % stride != 0 else w
        new_unpad = (new_h, new_w)
    if shape[::-1] != new_unpad:
        im = cv2.resize(im, new_unpad, interpolation=interpolation)
    return im


class LamaMPEWrapper:
    def __init__(self, ckpt_path: str, device: str):
        self.device = device
        self.model = load_lama_mpe(ckpt_path, device, use_mpe=True)
        self.model.eval()

    def _prepare_tensors(self, img: np.ndarray, mask: np.ndarray, inpaint_size: int):
        img_original = np.copy(img)
        mask_original = (mask >= 127).astype(np.uint8)
        mask_original = mask_original[:, :, None]

        new_shape = inpaint_size if max(img.shape[:2]) > inpaint_size else None
        img = resize_keepasp(img, new_shape, stride=64)
        mask = resize_keepasp(mask, new_shape, stride=64)

        h, w = mask.shape[:2]
        longer = max(h, w)
        pad_bottom = longer - h if h < longer else 0
        pad_right = longer - w if w < longer else 0
        mask = cv2.copyMakeBorder(mask, 0, pad_bottom, 0, pad_right, cv2.BORDER_REFLECT)
        img = cv2.copyMakeBorder(img, 0, pad_bottom, 0, pad_right, cv2.BORDER_REFLECT)

        img_torch = torch.from_numpy(img).permute(2, 0, 1).unsqueeze(0).float() / 255.0
        mask_torch = torch.from_numpy(mask).unsqueeze(0).unsqueeze(0).float() / 255.0
        mask_torch = (mask_torch >= 0.5).float()

        rel_pos, _, direct = self.model.load_masked_position_encoding(mask_torch[0, 0].cpu().numpy())
        rel_pos = torch.LongTensor(rel_pos).unsqueeze(0)
        direct = torch.LongTensor(direct).unsqueeze(0)

        if self.device != "cpu":
            img_torch = img_torch.to(self.device)
            mask_torch = mask_torch.to(self.device)
            rel_pos = rel_pos.to(self.device)
            direct = direct.to(self.device)
        img_torch *= (1 - mask_torch)
        return (
            img_torch,
            mask_torch,
            rel_pos,
            direct,
            img_original,
            mask_original,
            pad_bottom,
            pad_right,
        )

    @torch.no_grad()
    def __call__(self, img: np.ndarray, mask: np.ndarray, inpaint_size: int) -> np.ndarray:
        (
            img_torch,
            mask_torch,
            rel_pos,
            direct,
            img_original,
            mask_original,
            pad_bottom,
            pad_right,
        ) = self._prepare_tensors(img, mask, inpaint_size)

        if self.device != "cpu":
            device_type = "cuda" if self.device.startswith("cuda") else self.device
            try:
                with torch.autocast(device_type=device_type, dtype=torch.float16):
                    out = self.model(img_torch, mask_torch, rel_pos, direct)
            except Exception:
                out = self.model(img_torch, mask_torch, rel_pos, direct)
        else:
            out = self.model(img_torch, mask_torch, rel_pos, direct)

        img_inpainted = (
            out.to(device="cpu", dtype=torch.float32).squeeze(0).permute(1, 2, 0).numpy() * 255
        )
        img_inpainted = np.clip(np.round(img_inpainted), 0, 255).astype(np.uint8)
        if pad_bottom > 0:
            img_inpainted = img_inpainted[:-pad_bottom]
        if pad_right > 0:
            img_inpainted = img_inpainted[:, :-pad_right]

        im_h, im_w = img_original.shape[:2]
        if img_inpainted.shape[0] != im_h or img_inpainted.shape[1] != im_w:
            img_inpainted = cv2.resize(img_inpainted, (im_w, im_h), interpolation=cv2.INTER_LINEAR)

        return img_inpainted * mask_original + img_original * (1 - mask_original)

    def unload(self):
        try:
            self.model.to("cpu")
        except Exception:
            pass
        del self.model
        if torch.cuda.is_available():
            torch.cuda.empty_cache()

    def get_memory_stats(self):
        stats = {"model_loaded": True, "device": self.device}
        if torch.cuda.is_available() and self.device.startswith("cuda"):
            try:
                idx = int(self.device.split(":")[1])
            except Exception:
                idx = torch.cuda.current_device()
            stats.update(
                {
                    "allocated_mb": torch.cuda.memory_allocated(idx) / 1024**2,
                    "reserved_mb": torch.cuda.memory_reserved(idx) / 1024**2,
                }
            )
        return stats


# ---------- Диалог инпейнта ----------
class InpaintEditorDialog(RegionEditorDialog):
    inpaintDone = pyqtSignal(object, object)

    def __init__(self, image: QImage, ai_device: str, parent: Optional[QWidget] = None):
        self._ai_device = ai_device
        self._inpainter = None
        super().__init__(image, parent)
        self.setWindowTitle("LaMa MPE")
        self.inpaintDone.connect(self._on_inpaint_done, Qt.ConnectionType.QueuedConnection)
        self.set_status("Нарисуйте маску и нажмите «Обработать».")

    def info_text(self) -> str:
        return f"Модель: LaMa MPE ({self._ai_device})"

    def build_params_block(self):
        ctrl_box = QGroupBox("Параметры")
        form = QFormLayout(ctrl_box)

        self.spin_size = QSpinBox()
        self.spin_size.setRange(512, 4096)
        self.spin_size.setSingleStep(128)
        self.spin_size.setValue(2048)
        form.addRow("Inpaint size", self.spin_size)

        btn_clear = QPushButton("Очистить маску")
        btn_clear.clicked.connect(self.canvas.clear_mask)
        form.addRow(btn_clear)
        return ctrl_box

    def run(self, base_rgb: np.ndarray, mask_a: np.ndarray):
        try:
            self._inpainter = _get_inpainter(self._ai_device)
        except Exception as e:
            self.finish_processing(None, e)
            return None

        inpaint_size = int(self.spin_size.value())

        def worker():
            err = None
            result_rgb = None
            try:
                result_rgb = self._inpainter(base_rgb, mask_a, inpaint_size)
            except Exception as exc:  # noqa: BLE001
                traceback.print_exc()
                err = exc
            self.inpaintDone.emit(result_rgb, err)

        threading.Thread(target=worker, daemon=True).start()
        return None

    def _on_inpaint_done(self, result_rgb, err):
        self.finish_processing(result_rgb, err)


# ---------- Инструмент ----------
class RegionEditLamaMPETool(RegionEditTool):
    tool_id = "region_edit_lama_mpe"
    title = "AI удаление (LaMa MPE)"

    def create_editor_dialog(self, image: QImage, parent=None):
        return InpaintEditorDialog(image, ai_device=self.get_ai_device_str(), parent=parent)

    def build_ui(self, parent_layout) -> None:
        hint = QLabel("Shift+ЛКМ — выделить область, затем LaMa MPE удалит по маске.")
        hint.setStyleSheet("color: #666;")
        parent_layout.addWidget(hint)

        btn_unload = QPushButton("🧹 Выгрузить LaMa MPE")
        btn_unload.clicked.connect(self._on_unload_ai)
        parent_layout.addWidget(btn_unload)

        self.memory_label = QLabel("")
        self.memory_label.setStyleSheet("color: #888; font-size: 10px;")
        parent_layout.addWidget(self.memory_label)
        self._update_memory_info()

    def _on_unload_ai(self):
        if _unload_inpainter():
            self.memory_label.setText("✅ Модель выгружена")
        else:
            self.memory_label.setText("Модель не была загружена")

    def _update_memory_info(self):
        try:
            if _INPAINTER is not None:
                stats = _INPAINTER.get_memory_stats()
                if stats.get("model_loaded"):
                    allocated = stats.get("allocated_mb", 0)
                    reserved = stats.get("reserved_mb", 0)
                    self.memory_label.setText(f"💾 GPU: {allocated:.0f}/{reserved:.0f} MB")
                    return
        except Exception:
            pass
        self.memory_label.setText("Модель не загружена")
