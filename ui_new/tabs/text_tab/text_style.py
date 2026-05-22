from __future__ import annotations

from dataclasses import dataclass, replace
from typing import Callable, Dict, Optional

from PyQt6.QtGui import QColor


def _rgba_to_qcolor(rgba: Optional[tuple[int, int, int, int]]) -> Optional[QColor]:
    if rgba is None:
        return None
    r, g, b, a = rgba
    return QColor(int(r), int(g), int(b), int(a))


def _rgba_list(value: Optional[tuple[int, int, int, int]]) -> Optional[list[int]]:
    return list(value) if value is not None else None


@dataclass(frozen=True)
class TextStyle:
    """Все визуальные параметры текста в одном месте."""

    font_family: str = "Arial"
    font_size: int = 24
    font_color_rgba: tuple[int, int, int, int] = (0, 0, 0, 255)
    line_spacing: int = 4
    line_spacing_percent: int = 50
    extra_vpadding: int = 2
    align: str = "center"
    reflect: Optional[str] = None

    stroke_width: int = 0
    stroke_color_rgba: Optional[tuple[int, int, int, int]] = None

    glow_radius: int = 0
    glow_softness: int = 5
    glow_color_rgba: Optional[tuple[int, int, int, int]] = None

    shadow_dx: int = 0
    shadow_dy: int = 0
    shadow_color_rgba: Optional[tuple[int, int, int, int]] = None

    grad2_c1_rgba: Optional[tuple[int, int, int, int]] = None
    grad2_c2_rgba: Optional[tuple[int, int, int, int]] = None
    grad_angle_deg: float = 90.0

    grad4_tl_rgba: Optional[tuple[int, int, int, int]] = None
    grad4_tr_rgba: Optional[tuple[int, int, int, int]] = None
    grad4_bl_rgba: Optional[tuple[int, int, int, int]] = None
    grad4_br_rgba: Optional[tuple[int, int, int, int]] = None

    text_shape: str = "rectangle"
    shake_enabled: bool = False
    shake_angle_deg: float = 90.0
    shake_up: int = 0
    shake_down: int = 40
    shake_steps: int = 12
    shake_base_fade: float = 0.30
    shake_decay: float = 0.15
    shake_blur: int = 2

    def with_updates(self, **patch) -> "TextStyle":
        """Создать новую версию со смешением значений."""
        return replace(self, **patch)

    def ensure_exclusive_gradients(self) -> "TextStyle":
        """Убирает несовместимые значения (grad2 vs grad4)."""
        if any([self.grad4_tl_rgba, self.grad4_tr_rgba, self.grad4_bl_rgba, self.grad4_br_rgba]):
            return replace(self, grad2_c1_rgba=None, grad2_c2_rgba=None)
        return self

    def to_renderer_kwargs(self, *, text: str, width_px: int) -> Dict:
        """Подготовить kwargs для Renderer.big_renderer."""
        style = self.ensure_exclusive_gradients()
        gradient = None
        if style.grad2_c1_rgba and style.grad2_c2_rgba:
            gradient = (_rgba_to_qcolor(style.grad2_c1_rgba), _rgba_to_qcolor(style.grad2_c2_rgba))

        gradient4 = None
        if all([style.grad4_tl_rgba, style.grad4_tr_rgba, style.grad4_bl_rgba, style.grad4_br_rgba]):
            gradient4 = {
                "tl": _rgba_to_qcolor(style.grad4_tl_rgba),
                "tr": _rgba_to_qcolor(style.grad4_tr_rgba),
                "bl": _rgba_to_qcolor(style.grad4_bl_rgba),
                "br": _rgba_to_qcolor(style.grad4_br_rgba),
            }

        shake = None
        if style.shake_enabled:
            shake = {
                "angle_deg": float(style.shake_angle_deg),
                "up": int(style.shake_up),
                "down": int(style.shake_down),
                "steps": int(style.shake_steps),
                "base_fade": float(style.shake_base_fade),
                "decay": float(style.shake_decay),
                "blur": int(style.shake_blur),
            }

        return dict(
            text=text,
            width=int(width_px),
            font_family=style.font_family,
            font_px=int(style.font_size),
            color=_rgba_to_qcolor(style.font_color_rgba),
            align=style.align,
            line_spacing_px=int(style.line_spacing),
            line_spacing_percent=float(style.line_spacing_percent),
            stroke_color=_rgba_to_qcolor(style.stroke_color_rgba),
            stroke_width=int(style.stroke_width),
            glow_color=_rgba_to_qcolor(style.glow_color_rgba),
            glow_radius=int(style.glow_radius),
            glow_softness=int(style.glow_softness),
            shadow_offset=((int(style.shadow_dx), int(style.shadow_dy)) if style.shadow_color_rgba else None),
            shadow_color=_rgba_to_qcolor(style.shadow_color_rgba),
            gradient=gradient,
            gradient_angle_deg=float(style.grad_angle_deg),
            gradient4=gradient4,
            extra_vpadding=int(style.extra_vpadding),
            reflect=style.reflect,
            text_shape=style.text_shape,
            shake=shake,
        )

    def to_dict(self) -> Dict:
        """Внутреннее представление для UI/панелей."""
        return {
            "font_family": self.font_family,
            "font_size": self.font_size,
            "font_color_rgba": tuple(self.font_color_rgba),
            "line_spacing": self.line_spacing,
            "line_spacing_percent": self.line_spacing_percent,
            "extra_vpadding": self.extra_vpadding,
            "align": self.align,
            "reflect": self.reflect,
            "stroke_width": self.stroke_width,
            "stroke_color_rgba": tuple(self.stroke_color_rgba) if self.stroke_color_rgba else None,
            "glow_radius": self.glow_radius,
            "glow_softness": self.glow_softness,
            "glow_color_rgba": tuple(self.glow_color_rgba) if self.glow_color_rgba else None,
            "shadow_dx": self.shadow_dx,
            "shadow_dy": self.shadow_dy,
            "shadow_color_rgba": tuple(self.shadow_color_rgba) if self.shadow_color_rgba else None,
            "grad2_c1_rgba": tuple(self.grad2_c1_rgba) if self.grad2_c1_rgba else None,
            "grad2_c2_rgba": tuple(self.grad2_c2_rgba) if self.grad2_c2_rgba else None,
            "grad_angle_deg": self.grad_angle_deg,
            "grad4_tl_rgba": tuple(self.grad4_tl_rgba) if self.grad4_tl_rgba else None,
            "grad4_tr_rgba": tuple(self.grad4_tr_rgba) if self.grad4_tr_rgba else None,
            "grad4_bl_rgba": tuple(self.grad4_bl_rgba) if self.grad4_bl_rgba else None,
            "grad4_br_rgba": tuple(self.grad4_br_rgba) if self.grad4_br_rgba else None,
            "text_shape": self.text_shape,
            "shake_enabled": self.shake_enabled,
            "shake_angle_deg": self.shake_angle_deg,
            "shake_up": self.shake_up,
            "shake_down": self.shake_down,
            "shake_steps": self.shake_steps,
            "shake_base_fade": self.shake_base_fade,
            "shake_decay": self.shake_decay,
            "shake_blur": self.shake_blur,
        }

    def to_json(self) -> Dict:
        """Готово к сериализации (RGBA -> list)."""
        d = self.to_dict()
        for key in list(d.keys()):
            if key.endswith("_rgba") and d[key] is not None:
                d[key] = _rgba_list(d[key])
        return d

    @staticmethod
    def _tuple_or_none(value):
        if value is None:
            return None
        return tuple(value)

    @classmethod
    def from_dict(cls, data: Dict) -> "TextStyle":
        """Создать стиль из словаря (RGBA могут быть tuple/list)."""
        def rgba(key):
            v = data.get(key)
            return tuple(v) if v is not None else None

        return cls(
            font_family=data.get("font_family", "Arial"),
            font_size=int(data.get("font_size", data.get("size", 24))),
            font_color_rgba=tuple(data.get("font_color_rgba") or data.get("color_rgba") or data.get("color", (0, 0, 0, 255))),
            line_spacing=int(data.get("line_spacing", 4)),
            line_spacing_percent=int(data.get("line_spacing_percent", 50)),
            extra_vpadding=int(data.get("extra_vpadding", 2)),
            align=data.get("align", "center"),
            reflect=data.get("reflect", None),
            stroke_width=int(data.get("stroke_width", 0)),
            stroke_color_rgba=rgba("stroke_color_rgba"),
            glow_radius=int(data.get("glow_radius", 0)),
            glow_softness=int(data.get("glow_softness", 5)),
            glow_color_rgba=rgba("glow_color_rgba"),
            shadow_dx=int(data.get("shadow_dx", 0)),
            shadow_dy=int(data.get("shadow_dy", 0)),
            shadow_color_rgba=rgba("shadow_color_rgba"),
            grad2_c1_rgba=rgba("grad2_c1_rgba"),
            grad2_c2_rgba=rgba("grad2_c2_rgba"),
            grad_angle_deg=float(data.get("grad_angle_deg", 90.0)),
            grad4_tl_rgba=rgba("grad4_tl_rgba"),
            grad4_tr_rgba=rgba("grad4_tr_rgba"),
            grad4_bl_rgba=rgba("grad4_bl_rgba"),
            grad4_br_rgba=rgba("grad4_br_rgba"),
            text_shape=data.get("text_shape", "rectangle"),
            shake_enabled=bool(data.get("shake_enabled", False)),
            shake_angle_deg=float(data.get("shake_angle_deg", 90.0)),
            shake_up=int(data.get("shake_up", 0)),
            shake_down=int(data.get("shake_down", 40)),
            shake_steps=int(data.get("shake_steps", 12)),
            shake_base_fade=float(data.get("shake_base_fade", 0.30)),
            shake_decay=float(data.get("shake_decay", 0.15)),
            shake_blur=int(data.get("shake_blur", 2)),
        ).ensure_exclusive_gradients()


class StyleBinding:
    """Упрощённая «проводка» между панелью и хранилищем стиля."""

    def __init__(self, get_style: Callable[[], TextStyle], on_change: Callable[[Dict], None]):
        self._get_style = get_style
        self._on_change = on_change

    def current(self) -> TextStyle:
        return self._get_style()

    def emit(self, **patch) -> None:
        self._on_change(patch)
