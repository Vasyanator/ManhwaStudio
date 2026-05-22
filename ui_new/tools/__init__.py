from __future__ import annotations
import pkgutil, importlib, inspect
from typing import Dict, Type
from .base import BaseTool

active_tools = (
    "zamazka",
    "stamp",
    "region_edit_ai",
    "region_edit_opencv",
    "gradient_fill",
    "region_edit_lama_mpe",
    "aot_inpaint_tool",
    "region_clipboard",
)

def load_all_tools() -> Dict[str, Type[BaseTool]]:
    """
    Находит и импортирует только модули из active_tools,
    собирает классы-наследники BaseTool, ОПРЕДЕЛЁННЫЕ в этих модулях.
    Возвращает {tool_id: ToolClass}.
    """
    tools: Dict[str, Type[BaseTool]] = {}
    pkg = __name__

    for modinfo in pkgutil.iter_modules(__path__):  # type: ignore[name-defined]
        name = modinfo.name
        if name not in active_tools:
            continue

        module = importlib.import_module(f"{pkg}.{name}")

        for _, obj in inspect.getmembers(module, inspect.isclass):
            # 1) класс должен быть именно из этого модуля, а не «подтянут» импортом
            if getattr(obj, "__module__", None) != module.__name__:
                continue
            # 2) это наследник BaseTool, но не сам BaseTool
            if not issubclass(obj, BaseTool) or obj is BaseTool:
                continue
            # 3) пропускаем «абстрактные» каркасы
            if getattr(obj, "is_abstract", False):
                continue

            tool_id = getattr(obj, "tool_id", None)
            if tool_id:
                tools[tool_id] = obj

    return tools
