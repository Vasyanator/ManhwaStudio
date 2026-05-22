from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/nodes/__init__.py
Реестр узлов: общий список шаблонов и фабрика создания узлов по `template_key`.

Main items:
- `build_templates`: возвращает список шаблонов для палитры.
- `create_node`: создаёт `NodeBlockItem` (в т.ч. variable read/write) по ключу.
- Регистрирует встроенные узлы потока/строк/переменных/I-O/браузера/сшивания.
"""

from typing import Callable, Optional

from ..graphics_items import NodeBlockItem
from ..models import NodeTemplate, VariableDefinition
from . import (
    end,
    fetch_from_browser,
    open_url,
    quick_downloader,
    save_folder,
    scroll_page,
    start_number,
    start_string,
    stitch_split,
    string_template,
    waifu2x,
    variable_read,
    variable_write,
)


TEMPLATES: tuple[NodeTemplate, ...] = (
    start_number.TEMPLATE,
    start_string.TEMPLATE,
    string_template.TEMPLATE,
    end.TEMPLATE,
    quick_downloader.TEMPLATE,
    open_url.TEMPLATE,
    scroll_page.TEMPLATE,
    fetch_from_browser.TEMPLATE,
    stitch_split.TEMPLATE,
    waifu2x.TEMPLATE,
    save_folder.TEMPLATE,
    variable_read.TEMPLATE,
    variable_write.TEMPLATE,
)


def build_templates() -> list[NodeTemplate]:
    return list(TEMPLATES)


def create_node(
    template_key: str,
    *,
    variable_resolver: Callable[[str], Optional[VariableDefinition]],
    variables: list[VariableDefinition],
    preferred_variable: Optional[str] = None,
) -> Optional[NodeBlockItem]:
    if template_key == start_number.TEMPLATE.key:
        return start_number.create_node()
    if template_key == start_string.TEMPLATE.key:
        return start_string.create_node()
    if template_key == string_template.TEMPLATE.key:
        return string_template.create_node()
    if template_key == end.TEMPLATE.key:
        return end.create_node()
    if template_key == quick_downloader.TEMPLATE.key:
        return quick_downloader.create_node()
    if template_key == open_url.TEMPLATE.key:
        return open_url.create_node()
    if template_key == scroll_page.TEMPLATE.key:
        return scroll_page.create_node()
    if template_key == fetch_from_browser.TEMPLATE.key:
        return fetch_from_browser.create_node()
    if template_key == stitch_split.TEMPLATE.key:
        return stitch_split.create_node()
    if template_key == waifu2x.TEMPLATE.key:
        return waifu2x.create_node()
    if template_key == save_folder.TEMPLATE.key:
        return save_folder.create_node()
    if template_key == variable_read.TEMPLATE.key:
        return variable_read.create_node(variable_resolver, variables, preferred_variable)
    if template_key == variable_write.TEMPLATE.key:
        return variable_write.create_node(variable_resolver, variables, preferred_variable)
    return None
