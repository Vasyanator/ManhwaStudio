# ui_new/tools/aot_inpaint_tool.py
# Инструмент: AOT-инпейтинг по нарисованной маске внутри выделенного региона.
from __future__ import annotations

import os
import hashlib
from dataclasses import dataclass
from typing import Optional

import numpy as np
import cv2
import torch
import torch.nn as nn
import torch.nn.functional as F

from PyQt6.QtCore import Qt, QPointF, QSize
from PyQt6.QtGui import QImage, QPainter, QPen, QColor, QMouseEvent, QCursor
from PyQt6.QtWidgets import (
    QWidget, QVBoxLayout, QHBoxLayout,
    QPushButton, QLabel, QSizePolicy, QSpinBox, QGroupBox, QDialog, QMessageBox
)
from config import AOT_DIR
from .base import RegionEditTool, RegionEditorDialog

# ---------------------- AOT model (from your snippet, fixed) ----------------------
def relu_nf(x):
    return F.relu(x) * 1.7139588594436646


class LambdaLayer(nn.Module):
    def __init__(self, f):
        super().__init__()
        self.f = f

    def forward(self, x):
        return self.f(x)


class ScaledWSConv2d(nn.Conv2d):
    """Conv2d with Scaled Weight Standardization."""
    def __init__(self, in_channels, out_channels, kernel_size,
                 stride=1, padding=0, dilation=1, groups=1, bias=True,
                 gain=True, eps=1e-4):
        super().__init__(in_channels, out_channels, kernel_size, stride, padding, dilation, groups, bias)
        self.gain = nn.Parameter(torch.ones(out_channels, 1, 1, 1)) if gain else None
        self.eps = eps

    def get_weight(self):
        fan_in = int(np.prod(self.weight.shape[1:]))
        var, mean = torch.var_mean(self.weight, dim=(1, 2, 3), keepdim=True)
        eps_t = torch.tensor(self.eps, device=var.device, dtype=var.dtype)
        scale = torch.rsqrt(torch.max(var * fan_in, eps_t))
        if self.gain is not None:
            scale = scale * self.gain.to(var.device).view_as(var)
        shift = mean * scale
        return self.weight * scale - shift

    def forward(self, x):
        return F.conv2d(x, self.get_weight(), self.bias, self.stride, self.padding, self.dilation, self.groups)


class ScaledWSTransposeConv2d(nn.ConvTranspose2d):
    """ConvTranspose2d with Scaled Weight Standardization."""
    def __init__(self, in_channels, out_channels, kernel_size,
                 stride=1, padding=0, output_padding=0,
                 groups=1, bias=True, dilation=1,
                 gain=True, eps=1e-4):
        super().__init__(
            in_channels, out_channels, kernel_size,
            stride=stride, padding=padding, output_padding=output_padding,
            groups=groups, bias=bias, dilation=dilation, padding_mode="zeros"
        )
        self.gain = nn.Parameter(torch.ones(in_channels, 1, 1, 1)) if gain else None
        self.eps = eps

    def get_weight(self):
        fan_in = int(np.prod(self.weight.shape[1:]))
        var, mean = torch.var_mean(self.weight, dim=(1, 2, 3), keepdim=True)
        eps_t = torch.tensor(self.eps, device=var.device, dtype=var.dtype)
        scale = torch.rsqrt(torch.max(var * fan_in, eps_t))
        if self.gain is not None:
            scale = scale * self.gain.to(var.device).view_as(var)
        shift = mean * scale
        return self.weight * scale - shift

    def forward(self, x, output_size=None):
        output_padding = self._output_padding(
            x, output_size, self.stride, self.padding, self.kernel_size, self.dilation
        )
        return F.conv_transpose2d(
            x, self.get_weight(), self.bias,
            self.stride, self.padding, output_padding,
            self.groups, self.dilation
        )


class GatedWSConvPadded(nn.Module):
    def __init__(self, in_ch, out_ch, ks, stride=1, dilation=1):
        super().__init__()
        self.padding = nn.ReflectionPad2d(((ks - 1) * dilation) // 2)
        self.conv = ScaledWSConv2d(in_ch, out_ch, kernel_size=ks, stride=stride, dilation=dilation)
        self.conv_gate = ScaledWSConv2d(in_ch, out_ch, kernel_size=ks, stride=stride, dilation=dilation)

    def forward(self, x):
        x = self.padding(x)
        signal = self.conv(x)
        gate = torch.sigmoid(self.conv_gate(x))
        return signal * gate * 1.8


class GatedWSTransposeConvPadded(nn.Module):
    def __init__(self, in_ch, out_ch, ks, stride=1):
        super().__init__()
        self.conv = ScaledWSTransposeConv2d(in_ch, out_ch, kernel_size=ks, stride=stride, padding=(ks - 1) // 2)
        self.conv_gate = ScaledWSTransposeConv2d(in_ch, out_ch, kernel_size=ks, stride=stride, padding=(ks - 1) // 2)

    def forward(self, x):
        signal = self.conv(x)
        gate = torch.sigmoid(self.conv_gate(x))
        return signal * gate * 1.8


def _my_layer_norm(feat):
    mean = feat.mean((2, 3), keepdim=True)
    std = feat.std((2, 3), keepdim=True) + 1e-9
    feat = 2 * (feat - mean) / std - 1
    feat = 5 * feat
    return feat


class AOTBlock(nn.Module):
    def __init__(self, dim, rates=(2, 4, 8, 16)):
        super().__init__()
        self.rates = list(rates)
        for i, rate in enumerate(self.rates):
            setattr(
                self,
                f"block{str(i).zfill(2)}",
                nn.Sequential(
                    nn.ReflectionPad2d(rate),
                    nn.Conv2d(dim, dim // 4, 3, padding=0, dilation=rate),
                    nn.ReLU(True),
                ),
            )
        self.fuse = nn.Sequential(nn.ReflectionPad2d(1), nn.Conv2d(dim, dim, 3, padding=0, dilation=1))
        self.gate = nn.Sequential(nn.ReflectionPad2d(1), nn.Conv2d(dim, dim, 3, padding=0, dilation=1))

    def forward(self, x):
        out = [getattr(self, f"block{str(i).zfill(2)}")(x) for i in range(len(self.rates))]
        out = torch.cat(out, 1)
        out = self.fuse(out)
        mask = torch.sigmoid(_my_layer_norm(self.gate(x)))
        return x * (1 - mask) + out * mask


class AOTGenerator(nn.Module):
    def __init__(self, in_ch=4, out_ch=3, ch=32, alpha=0.0):
        super().__init__()
        self.head = nn.Sequential(
            GatedWSConvPadded(in_ch, ch, 3, stride=1),
            LambdaLayer(relu_nf),
            GatedWSConvPadded(ch, ch * 2, 4, stride=2),
            LambdaLayer(relu_nf),
            GatedWSConvPadded(ch * 2, ch * 4, 4, stride=2),
        )
        self.body_conv = nn.Sequential(*[AOTBlock(ch * 4) for _ in range(10)])
        self.tail = nn.Sequential(
            GatedWSConvPadded(ch * 4, ch * 4, 3, 1),
            LambdaLayer(relu_nf),
            GatedWSConvPadded(ch * 4, ch * 4, 3, 1),
            LambdaLayer(relu_nf),
            GatedWSTransposeConvPadded(ch * 4, ch * 2, 4, 2),
            LambdaLayer(relu_nf),
            GatedWSTransposeConvPadded(ch * 2, ch, 4, 2),
            LambdaLayer(relu_nf),
            GatedWSConvPadded(ch, out_ch, 3, stride=1),
        )

    def forward(self, img, mask):
        x = torch.cat([mask, img], dim=1)
        x = self.head(x)
        x = self.body_conv(x)
        x = self.tail(x)
        return x if self.training else torch.clip(x, -1, 1)


def resize_keepasp(im, new_shape=640, scaleup=True, interpolation=cv2.INTER_LINEAR, stride=None):
    shape = im.shape[:2]  # (h, w)
    if new_shape is not None:
        if not isinstance(new_shape, tuple):
            new_shape = (new_shape, new_shape)
    else:
        new_shape = shape

    r = min(new_shape[0] / shape[0], new_shape[1] / shape[1])
    if not scaleup:
        r = min(r, 1.0)

    new_unpad = int(round(shape[1] * r)), int(round(shape[0] * r))  # (w, h)

    if stride is not None:
        w, h = new_unpad
        new_w = w + (stride - (w % stride)) if w % stride != 0 else w
        new_h = h + (stride - (h % stride)) if h % stride != 0 else h
        new_unpad = (new_w, new_h)

    if shape[::-1] != new_unpad:
        im = cv2.resize(im, new_unpad, interpolation=interpolation)
    return im


def load_aot_model(model_path: str, device: str) -> AOTGenerator:
    model = AOTGenerator(in_ch=4, out_ch=3, ch=32, alpha=0.0)
    sd = torch.load(model_path, map_location="cpu")
    model.load_state_dict(sd["model"] if isinstance(sd, dict) and "model" in sd else sd)
    model.eval().to(device)
    return model


# ---------------------- runner (cached) ----------------------
@dataclass
class _AOTConfig:
    device: str = "cpu"
    inpaint_size: int = 2048
    model_path: str = "data/models/aot_inpainter.ckpt"


class _AOTRunner:
    _model: Optional[AOTGenerator] = None
    _loaded_path: Optional[str] = None
    _loaded_device: Optional[str] = None

    def __init__(self, cfg: _AOTConfig):
        self.cfg = cfg

    def _ensure_model(self):
        if not os.path.exists(self.cfg.model_path):
            raise FileNotFoundError(self.cfg.model_path)
        if (_AOTRunner._model is None
            or _AOTRunner._loaded_path != self.cfg.model_path
            or _AOTRunner._loaded_device != self.cfg.device):
            _AOTRunner._model = load_aot_model(self.cfg.model_path, self.cfg.device)
            _AOTRunner._loaded_path = self.cfg.model_path
            _AOTRunner._loaded_device = self.cfg.device

    def _preprocess(self, img_rgb: np.ndarray, mask_u8: np.ndarray):
        img_original = img_rgb.copy()
        mask_original = mask_u8.copy()
        mask_original[mask_original < 127] = 0
        mask_original[mask_original >= 127] = 1
        mask_original = mask_original[:, :, None]  # HxWx1

        new_shape = self.cfg.inpaint_size if max(img_rgb.shape[:2]) > self.cfg.inpaint_size else None
        img = resize_keepasp(img_rgb, new_shape, stride=None)
        mask = resize_keepasp(mask_u8, new_shape, stride=None)

        im_h, im_w = img.shape[:2]
        pad_bottom = 128 - im_h if im_h < 128 else 0
        pad_right = 128 - im_w if im_w < 128 else 0
        if pad_bottom > 0 or pad_right > 0:
            img = cv2.copyMakeBorder(img, 0, pad_bottom, 0, pad_right, cv2.BORDER_REFLECT)
            mask = cv2.copyMakeBorder(mask, 0, pad_bottom, 0, pad_right, cv2.BORDER_REFLECT)

        img_t = torch.from_numpy(img).permute(2, 0, 1).unsqueeze(0).float() / 127.5 - 1.0
        mask_t = torch.from_numpy(mask).unsqueeze(0).unsqueeze(0).float() / 255.0
        mask_t = (mask_t >= 0.5).float()

        if self.cfg.device != "cpu":
            img_t = img_t.to(self.cfg.device)
            mask_t = mask_t.to(self.cfg.device)

        img_t = img_t * (1 - mask_t)
        return img_t, mask_t, img_original, mask_original, pad_bottom, pad_right, (im_h, im_w)

    @torch.no_grad()
    def inpaint(self, img_rgb: np.ndarray, mask_u8: np.ndarray) -> np.ndarray:
        self._ensure_model()
        im_h0, im_w0 = img_rgb.shape[:2]

        img_t, mask_t, img_orig, mask_orig, pad_bottom, pad_right, _ = self._preprocess(img_rgb, mask_u8)
        out_t = _AOTRunner._model(img_t, mask_t)

        out = (out_t.detach().cpu().squeeze(0).permute(1, 2, 0).numpy() + 1.0) * 127.5
        out = np.clip(np.round(out), 0, 255).astype(np.uint8)

        if pad_bottom > 0:
            out = out[:-pad_bottom, :, :]
        if pad_right > 0:
            out = out[:, :-pad_right, :]

        if out.shape[0] != im_h0 or out.shape[1] != im_w0:
            out = cv2.resize(out, (im_w0, im_h0), interpolation=cv2.INTER_LINEAR)

        # композит: инпейнт только по маске
        out = out * mask_orig + img_orig * (1 - mask_orig)
        return out


class AOTInpaintDialog(RegionEditorDialog):
    """Диалог: рисуем маску, обрабатываем AOT и применяем результат."""
    def __init__(self, image: QImage, runner: _AOTRunner, parent: Optional[QWidget] = None):
        self._runner = runner
        super().__init__(image, parent)
        self.setWindowTitle("AOT Inpaint")
        self.set_status("Нарисуйте маску и нажмите «Обработать».")

    def info_text(self) -> str:
        return f"Модель: AOT ({self._runner.cfg.device})"

    def build_params_block(self):
        panel = QGroupBox("Маска")
        row = QHBoxLayout()
        btn_clear = QPushButton("Очистить маску")
        btn_clear.clicked.connect(self.canvas.clear_mask)
        btn_inv = QPushButton("Инвертировать маску")
        btn_inv.clicked.connect(self.canvas.invert_mask)
        row.addWidget(btn_clear)
        row.addWidget(btn_inv)
        row.addStretch(1)
        panel.setLayout(row)
        return panel

    def run(self, base_rgb: np.ndarray, mask_a: np.ndarray):
        if base_rgb.size == 0 or mask_a.size == 0:
            raise ValueError("Пустое изображение/маска.")
        if int(mask_a.max()) == 0:
            raise ValueError("Маска пустая: нечего инпейнтить.")

        out_rgb = self._runner.inpaint(base_rgb, mask_a)
        self.set_status("✅ Готово. Можно дорисовать маску и нажать «Переделать» или «Применить».")
        return out_rgb


class _DisabledAOTDialog(QDialog):
    def exec(self):
        return QDialog.DialogCode.Rejected

    def was_accepted(self) -> bool:
        return False


# ---------------------- Tool: inherits RegionEditTool ----------------------
class AOTInpaintTool(RegionEditTool):
    """
    Наследник RegionEditTool:
      • Shift+ЛКМ выделяет регион.
      • В диалоге рисуем маску (ЛКМ) / стираем (ПКМ).
      • Apply запускает AOT-инпейтинг и вставляет результат в оверлей.
    """
    tool_id = "aot_inpaint"
    title = "AI удаление (AOT)"

    def __init__(self):
        super().__init__()

        self._cfg = _AOTConfig(device="cpu", inpaint_size=2048, model_path=os.path.join(AOT_DIR, "inpainting.ckpt"))
        self._runner = _AOTRunner(self._cfg)
        self._brush_radius = 18
        self._model_available = True

        # простая валидация ckpt по желанию (не обяз.)
        self._expected_sha256: Optional[str] = None  # можно задать строкой, если нужно
        self._model_available = os.path.exists(self._cfg.model_path)
        if not self._model_available:
            QMessageBox.critical(None, "Ошибка", "ИИ модель AOT не скачана. Скачайте её в менеджере моделей, прежде чем использовать этот инструмент.")

    def _ensure_weights_available(self) -> bool:
        if os.path.exists(self._cfg.model_path):
            self._model_available = True
            return True
        self._model_available = False
        QMessageBox.critical(None, "Ошибка", "Отсутствуют веса AOT-модели.")
        return False

    def build_ui(self, parent_layout) -> None:
        parent_layout.addWidget(QLabel("Выделение: Shift + ЛКМ (прямоугольник)"))
        parent_layout.addWidget(QLabel("В диалоге: ЛКМ — маска, ПКМ — стереть"))
        parent_layout.addWidget(QLabel(f"Устройство: {self.get_ai_device_str()}"))

        row = QHBoxLayout()

        sp_size = QSpinBox()
        sp_size.setRange(256, 4096)
        sp_size.setSingleStep(256)
        sp_size.setValue(int(self._cfg.inpaint_size))

        sp_brush = QSpinBox()
        sp_brush.setRange(1, 128)
        sp_brush.setValue(int(self._brush_radius))

        row.addWidget(QLabel("inpaint_size:"))
        row.addWidget(sp_size)
        row.addSpacing(12)
        row.addWidget(QLabel("кисть:"))
        row.addWidget(sp_brush)
        row.addStretch(1)
        row.setContentsMargins(0, 0, 0, 0)
        row.setSpacing(4)

        wrap = QWidget()
        wrap.setLayout(row)
        parent_layout.addWidget(wrap)

        def _on_size(v: int):
            self._cfg.inpaint_size = int(v)

        def _on_brush(v: int):
            self._brush_radius = int(v)

        sp_size.valueChanged.connect(_on_size)
        sp_brush.valueChanged.connect(_on_brush)

    # hooks from RegionEditTool
    def create_editor_dialog(self, image: QImage, parent: Optional[QWidget] = None) -> "AOTInpaintDialog":
        if not self._ensure_weights_available():
            return _DisabledAOTDialog(parent)
        self._cfg.device = self.get_ai_device_str()
        dlg = AOTInpaintDialog(image, self._runner, parent)
        dlg.canvas.set_brush_radius(self._brush_radius)
        dlg.slider.setValue(self._brush_radius)
        dlg.slider.valueChanged.connect(lambda v: setattr(self, "_brush_radius", int(v)))
        return dlg

    def is_editor_accepted(self, dialog: "AOTInpaintDialog") -> bool:
        return dialog.was_accepted()

    def editor_result_image(self, dialog: "AOTInpaintDialog") -> QImage:
        return dialog.edited_image()
