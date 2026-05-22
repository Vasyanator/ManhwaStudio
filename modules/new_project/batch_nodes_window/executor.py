from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/executor.py
Исполнитель графа массовой обработки (без GUI), запускаемый в background-потоке.

Main items:
- `GraphRuntimeSnapshot`: сериализуемый снимок узлов/связей/переменных.
- `BatchPipelineExecutor`: интерпретатор exec/data графа с поддержкой join-узлов.
- Реализация node-handlers: start_number/start_string/string_template/variable_read/write/
  quick_downloader/open_url/scroll_page/fetch_from_browser/stitch_split/waifu2x/save_folder/end.
- `_scroll_page` / `_scroll_by_percent_steps` / `_read_scroll_state`: постепенная прокрутка
  страницы по шагам 0/40/80/100% и циклами вверх+вниз до стабилизации `document.body.scrollHeight`.
- `_collect_browser_candidates`: сбор ссылок из DOM (src/currentSrc/srcset/data-src/href)
  для устойчивой выкачки из браузера, включая lazy-load атрибуты.
"""

from collections import defaultdict, deque
from dataclasses import dataclass, field
from io import BytesIO
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Callable, Optional
from urllib.parse import quote, unquote, urljoin, urlparse, urlunparse

import requests
from PIL import Image

from ..common import compile_wildcard_prefixes

_CONTROL = {c: None for c in range(0x00, 0x20)} | {0x7F: None}


@dataclass
class VariableRuntimeSpec:
    name: str
    data_type: str
    persist_between_cycles: bool


@dataclass
class NodeRuntimeSpec:
    node_id: int
    key: str
    title: str
    params: dict[str, object] = field(default_factory=dict)
    data_inputs: list[str] = field(default_factory=list)
    data_outputs: list[str] = field(default_factory=list)
    required_exec_inputs: int = 0


@dataclass(frozen=True)
class ExecEdgeSpec:
    edge_id: int
    src_node_id: int
    src_socket: str
    dst_node_id: int
    dst_socket: str


@dataclass(frozen=True)
class DataEdgeSpec:
    src_node_id: int
    src_socket: str
    dst_node_id: int
    dst_socket: str


@dataclass
class GraphRuntimeSnapshot:
    nodes: list[NodeRuntimeSpec]
    exec_edges: list[ExecEdgeSpec]
    data_edges: list[DataEdgeSpec]
    variables: list[VariableRuntimeSpec]


@dataclass(frozen=True)
class _ExecTask:
    cycle_id: int
    node_id: int
    edge_id: int


class PipelineExecutionStopped(RuntimeError):
    pass


class BatchPipelineExecutor:
    def __init__(
        self,
        snapshot: GraphRuntimeSnapshot,
        progress_callback: Optional[Callable[[str], None]] = None,
        should_stop_callback: Optional[Callable[[], bool]] = None,
    ):
        self._snapshot = snapshot
        self._progress = progress_callback or (lambda _msg: None)
        self._should_stop = should_stop_callback or (lambda: False)

        self._nodes_by_id = {node.node_id: node for node in snapshot.nodes}
        self._exec_out_by_node: dict[int, list[ExecEdgeSpec]] = defaultdict(list)
        self._data_out_by_node: dict[int, list[DataEdgeSpec]] = defaultdict(list)
        self._data_in_by_socket: dict[tuple[int, str], list[DataEdgeSpec]] = defaultdict(list)

        for edge in snapshot.exec_edges:
            self._exec_out_by_node[edge.src_node_id].append(edge)
        for edge in snapshot.data_edges:
            self._data_out_by_node[edge.src_node_id].append(edge)
            self._data_in_by_socket[(edge.dst_node_id, edge.dst_socket)].append(edge)

        self._variable_defaults = {spec.name: None for spec in snapshot.variables}
        self._persistent_variables = {
            spec.name: spec.persist_between_cycles for spec in snapshot.variables
        }

        self._stats_cycles = 0
        self._stats_nodes_executed = 0
        self._stats_end_hits = 0
        self._stats_downloaded_images = 0
        self._stats_saved_images = 0
        self._browser_driver = None
        self._browser_tmp_profile_dir: Optional[str] = None
        self._browser_name = ""

    def execute(self) -> str:
        try:
            self._check_cancelled()
            if not self._nodes_by_id:
                raise RuntimeError("Граф пуст. Добавьте узлы перед запуском.")

            start_nodes = [
                node
                for node in self._nodes_by_id.values()
                if node.key in ("start_number", "start_string")
            ]
            if not start_nodes:
                raise RuntimeError("В графе нет стартовых узлов (Старт (число) / Старт (строка)).")

            variable_values = dict(self._variable_defaults)
            data_values: dict[tuple[int, int, str], object] = {}
            exec_arrivals: dict[tuple[int, int], set[int]] = defaultdict(set)
            exec_queue: deque[_ExecTask] = deque()
            cycle_id_counter = 0
            max_steps = 200_000
            steps = 0

            for start_node in sorted(start_nodes, key=lambda x: x.node_id):
                self._check_cancelled()
                for start_outputs in self._iterate_start_node(start_node):
                    self._check_cancelled()
                    cycle_id_counter += 1
                    self._stats_cycles += 1
                    self._reset_non_persistent_variables(variable_values)
                    self._propagate_data_outputs(
                        start_node.node_id, cycle_id_counter, start_outputs, data_values
                    )
                    self._enqueue_exec_outputs(start_node.node_id, cycle_id_counter, exec_queue)

                    while exec_queue:
                        self._check_cancelled()
                        steps += 1
                        if steps > max_steps:
                            raise RuntimeError(
                                "Превышен лимит шагов исполнения. Проверьте граф на зацикливание."
                            )
                        task = exec_queue.popleft()
                        node = self._nodes_by_id.get(task.node_id)
                        if node is None:
                            continue

                        arrived = exec_arrivals[(task.cycle_id, task.node_id)]
                        arrived.add(task.edge_id)
                        required = max(1, node.required_exec_inputs)
                        if len(arrived) < required:
                            continue
                        arrived.clear()

                        inputs = {
                            socket_name: self._resolve_data_input(
                                cycle_id=task.cycle_id,
                                node_id=task.node_id,
                                socket_name=socket_name,
                                data_values=data_values,
                                variable_values=variable_values,
                            )
                            for socket_name in node.data_inputs
                        }
                        outputs = self._execute_node(
                            node=node,
                            inputs=inputs,
                            variable_values=variable_values,
                            cycle_id=task.cycle_id,
                        )
                        self._stats_nodes_executed += 1

                        self._propagate_data_outputs(node.node_id, task.cycle_id, outputs, data_values)
                        self._enqueue_exec_outputs(node.node_id, task.cycle_id, exec_queue)

            return (
                f"Выполнение завершено. Циклов: {self._stats_cycles}, "
                f"узлов выполнено: {self._stats_nodes_executed}, "
                f"конечных узлов: {self._stats_end_hits}, "
                f"скачано изображений: {self._stats_downloaded_images}, "
                f"сохранено файлов: {self._stats_saved_images}."
            )
        finally:
            self._shutdown_browser()

    def _iterate_start_node(self, node: NodeRuntimeSpec):
        if node.key == "start_number":
            start = int(node.params.get("start", 0))
            step = int(node.params.get("step", 1))
            end = int(node.params.get("end", 0))
            if step == 0:
                raise RuntimeError(f"Узел '{node.title}': шаг не может быть 0.")

            value = start
            if step > 0:
                while value <= end:
                    self._check_cancelled()
                    self._progress(f"{node.title}: индекс {value}")
                    yield {"Индекс": value}
                    value += step
            else:
                while value >= end:
                    self._check_cancelled()
                    self._progress(f"{node.title}: индекс {value}")
                    yield {"Индекс": value}
                    value += step
            return

        if node.key == "start_string":
            file_path = str(node.params.get("path", "") or "").strip()
            if not file_path:
                raise RuntimeError(f"Узел '{node.title}': не указан путь к txt-файлу.")

            lines = self._read_text_lines(file_path)
            for line_idx, line in enumerate(lines, start=1):
                self._check_cancelled()
                self._progress(f"{node.title}: строка {line_idx}")
                yield {"Строка": line}
            return

        raise RuntimeError(f"Узел '{node.title}' не является стартовым.")

    def _execute_node(
        self,
        *,
        node: NodeRuntimeSpec,
        inputs: dict[str, object],
        variable_values: dict[str, object],
        cycle_id: int,
    ) -> dict[str, object]:
        self._check_cancelled()
        self._progress(f"[Цикл {cycle_id}] {node.title}")

        if node.key == "string_template":
            template = str(node.params.get("template", "") or "")
            placeholders = node.params.get("placeholders", [])
            if not isinstance(placeholders, list):
                placeholders = []
            result = template
            for name in placeholders:
                if not isinstance(name, str):
                    continue
                value = inputs.get(name)
                replacement = "" if value is None else str(value)
                result = result.replace("{" + name + "}", replacement)
            return {"Строка": result}

        if node.key == "variable_write":
            variable_name = str(node.params.get("variable_name", "") or "").strip()
            if variable_name:
                variable_values[variable_name] = inputs.get("Значение")
            return {}

        if node.key == "variable_read":
            return self._execute_variable_read(node, variable_values)

        if node.key == "quick_downloader":
            source = str(inputs.get("Ссылка", "") or "").strip()
            if not source:
                raise RuntimeError(f"Узел '{node.title}': вход 'Ссылка' пустой.")
            images = self._download_images(source)
            self._stats_downloaded_images += len(images)
            return {"Картинки": images}

        if node.key == "open_url":
            source = str(inputs.get("URL", "") or "").strip()
            if not source:
                raise RuntimeError(f"Узел '{node.title}': вход 'URL' пустой.")
            browser = str(node.params.get("browser", "") or "").strip()
            self._open_url_in_browser(browser, source)
            return {}

        if node.key == "scroll_page":
            self._scroll_page()
            return {}

        if node.key == "fetch_from_browser":
            pattern = str(node.params.get("pattern", "") or "").strip()
            images = self._fetch_from_browser(pattern)
            self._stats_downloaded_images += len(images)
            return {"Картинки": images}

        if node.key == "stitch_split":
            source_images = self._coerce_image_list(inputs.get("Картинки"))
            stitched_images = self._stitch_split_images(
                source_images,
                K=node.params.get("K"),
                Hmax=node.params.get("Hmax"),
                band_rows=node.params.get("band_rows"),
                tol=node.params.get("tol"),
                search_radius=node.params.get("search_radius"),
                prefer_up_first=node.params.get("prefer_up_first"),
            )
            return {"Картинки": stitched_images}

        if node.key == "waifu2x":
            source_images = self._coerce_image_list(inputs.get("Картинки"))
            waifu_images = self._run_waifu2x(
                source_images,
                noise=node.params.get("noise"),
                scale=node.params.get("scale"),
                tile=node.params.get("tile"),
                exec_path=node.params.get("exec_path"),
            )
            return {"Картинки": waifu_images}

        if node.key == "save_folder":
            image_list = inputs.get("Картинки")
            folder = str(inputs.get("Путь", "") or "").strip()
            if not folder:
                raise RuntimeError(f"Узел '{node.title}': вход 'Путь' пустой.")
            saved = self._save_images_to_folder(image_list, folder)
            self._stats_saved_images += saved
            return {}

        if node.key == "end":
            self._stats_end_hits += 1
            return {}

        if node.key in ("start_number", "start_string"):
            return {}

        raise RuntimeError(f"Нет обработчика для узла '{node.title}' ({node.key}).")

    def _execute_variable_read(
        self,
        node: NodeRuntimeSpec,
        variable_values: dict[str, object],
    ) -> dict[str, object]:
        variable_name = str(node.params.get("variable_name", "") or "").strip()
        return {"Значение": variable_values.get(variable_name)}

    def _resolve_data_input(
        self,
        *,
        cycle_id: int,
        node_id: int,
        socket_name: str,
        data_values: dict[tuple[int, int, str], object],
        variable_values: dict[str, object],
    ) -> object:
        key = (cycle_id, node_id, socket_name)
        if key in data_values:
            return data_values[key]

        incoming_edges = self._data_in_by_socket.get((node_id, socket_name), [])
        for edge in incoming_edges:
            source_node = self._nodes_by_id.get(edge.src_node_id)
            if source_node is None:
                continue
            if source_node.key == "variable_read":
                outputs = self._execute_variable_read(source_node, variable_values)
                value = outputs.get(edge.src_socket)
                data_values[key] = value
                return value

        return None

    def _propagate_data_outputs(
        self,
        src_node_id: int,
        cycle_id: int,
        outputs: dict[str, object],
        data_values: dict[tuple[int, int, str], object],
    ) -> None:
        if not outputs:
            return
        for edge in self._data_out_by_node.get(src_node_id, []):
            if edge.src_socket not in outputs:
                continue
            data_values[(cycle_id, edge.dst_node_id, edge.dst_socket)] = outputs[edge.src_socket]

    def _enqueue_exec_outputs(
        self,
        src_node_id: int,
        cycle_id: int,
        queue: deque[_ExecTask],
    ) -> None:
        for edge in self._exec_out_by_node.get(src_node_id, []):
            queue.append(_ExecTask(cycle_id=cycle_id, node_id=edge.dst_node_id, edge_id=edge.edge_id))

    def _reset_non_persistent_variables(self, variable_values: dict[str, object]) -> None:
        for name, keep_value in self._persistent_variables.items():
            if not keep_value:
                variable_values[name] = None

    def _check_cancelled(self) -> None:
        if self._should_stop():
            raise PipelineExecutionStopped("Выполнение остановлено пользователем.")

    def _read_text_lines(self, file_path: str) -> list[str]:
        path = Path(file_path).expanduser()
        if not path.exists() or not path.is_file():
            raise RuntimeError(f"Файл не найден: {file_path}")

        encodings = ("utf-8-sig", "utf-8", "cp1251")
        content = None
        for encoding in encodings:
            try:
                content = path.read_text(encoding=encoding)
                break
            except UnicodeDecodeError:
                continue
        if content is None:
            content = path.read_text(encoding="latin-1")

        lines = [line.rstrip("\r\n") for line in content.splitlines()]
        if not lines:
            raise RuntimeError(f"Файл '{file_path}' не содержит строк.")
        return lines

    def _download_images(self, source: str) -> list[Image.Image]:
        self._check_cancelled()
        local_path = self._extract_local_path(source)
        if local_path is not None:
            with Image.open(local_path) as image:
                return [image.convert("RGB")]

        downloader_error = None
        try:
            from modules.downloader import download_webtoon_images

            def _cb(step: str, current: int, total: int) -> None:
                self._check_cancelled()
                self._progress(f"Загрузка изображений: {step} {current}/{total}")

            images = download_webtoon_images(source, progress_callback=_cb)
            if images:
                return [img.convert("RGB") if img.mode != "RGB" else img for img in images]
        except PipelineExecutionStopped:
            raise
        except Exception as exc:
            downloader_error = exc

        # Fallback: source как прямой URL изображения.
        try:
            self._check_cancelled()
            response = requests.get(source, timeout=30)
            response.raise_for_status()
            image = Image.open(BytesIO(response.content)).convert("RGB")
            return [image]
        except PipelineExecutionStopped:
            raise
        except Exception as fallback_exc:
            if downloader_error is not None:
                raise RuntimeError(
                    f"Не удалось скачать изображения по ссылке '{source}'. "
                    f"Downloader: {downloader_error}. Fallback: {fallback_exc}"
                ) from fallback_exc
            raise RuntimeError(
                f"Не удалось скачать изображения по ссылке '{source}': {fallback_exc}"
            ) from fallback_exc

    @staticmethod
    def _extract_local_path(source: str) -> Optional[Path]:
        parsed = urlparse(source)
        if parsed.scheme == "file":
            p = Path(unquote(parsed.path))
            if p.exists() and p.is_file():
                return p
            return None

        p = Path(source).expanduser()
        if p.exists() and p.is_file():
            return p
        return None

    def _coerce_image_list(self, image_list: object) -> list[Image.Image]:
        if not isinstance(image_list, list):
            raise RuntimeError("Ожидается список картинок.")
        out: list[Image.Image] = []
        for item in image_list:
            if isinstance(item, Image.Image):
                out.append(item.convert("RGB"))
        if not out:
            raise RuntimeError("Список картинок пуст или не содержит валидных изображений.")
        return out

    def _stitch_split_images(
        self,
        image_list: list[Image.Image],
        *,
        K: object,
        Hmax: object,
        band_rows: object,
        tol: object,
        search_radius: object,
        prefer_up_first: object,
    ) -> list[Image.Image]:
        try:
            from modules.manhwa_merge import main_process
            from modules.new_project.stitching import bgr_to_pil, pil_to_bgr
        except Exception as exc:
            raise RuntimeError(f"Не удалось загрузить модуль сшивания: {exc}") from exc

        k_value: Optional[int]
        if K in (None, ""):
            k_value = None
        else:
            try:
                k_value = int(K)
            except Exception as exc:
                raise RuntimeError("Параметр K должен быть целым или пустым.") from exc
            if k_value <= 0:
                raise RuntimeError("Параметр K должен быть > 0.")

        try:
            hmax_val = int(Hmax if Hmax is not None else 19_000)
            band_val = int(band_rows if band_rows is not None else 4)
            tol_val = int(tol if tol is not None else 15)
            radius_val = int(search_radius if search_radius is not None else 5_500)
        except Exception as exc:
            raise RuntimeError("Параметры Hmax/band_rows/tol/search_radius должны быть целыми.") from exc

        if min(hmax_val, band_val, tol_val, radius_val) <= 0:
            raise RuntimeError("Параметры Hmax/band_rows/tol/search_radius должны быть > 0.")

        self._check_cancelled()
        self._progress("Склейка/резка: подготовка изображений")
        bgr_list = [pil_to_bgr(im) for im in image_list]

        segments_bgr = main_process(
            bgr_list,
            K=k_value,
            Hmax=hmax_val,
            band_rows=band_val,
            tol=tol_val,
            search_radius=radius_val,
            prefer_up_first=bool(prefer_up_first),
            verbose=False,
        )
        self._check_cancelled()
        result = [bgr_to_pil(arr).convert("RGB") for arr in segments_bgr]
        if not result:
            raise RuntimeError("Склейка/резка не вернула сегменты.")
        return result

    def _open_url_in_browser(self, browser_name: str, source_url: str) -> None:
        self._check_cancelled()
        driver = self._ensure_browser(browser_name)
        normalized = self._normalize_http_url(source_url)
        self._progress(f"Открытие URL: {normalized}")
        driver.get(normalized)
        self._wait_for_page_loaded(driver, timeout=60)

    def _scroll_page(self) -> None:
        self._check_cancelled()
        driver = self._require_browser()
        step_delay_s = 0.3
        down_steps = (0, 40, 80, 100)
        up_steps = tuple(reversed(down_steps))
        max_cycles = 40
        prev_body_height = self._read_body_scroll_height(driver)

        self._progress("Промотка страницы: первый проход вниз (0/40/80/100%)")
        self._scroll_by_percent_steps(driver, down_steps, step_delay_s=step_delay_s)

        for cycle_idx in range(1, max_cycles + 1):
            self._check_cancelled()
            current_body_height = self._read_body_scroll_height(driver)
            if current_body_height <= prev_body_height + 2:
                self._progress(
                    f"Промотка страницы: высота стабилизировалась "
                    f"({current_body_height}px), остановка."
                )
                return

            self._progress(
                f"Промотка страницы: обнаружена догрузка "
                f"({prev_body_height}px -> {current_body_height}px), цикл {cycle_idx}"
            )
            prev_body_height = current_body_height

            self._scroll_by_percent_steps(driver, up_steps, step_delay_s=step_delay_s)
            self._scroll_by_percent_steps(driver, down_steps, step_delay_s=step_delay_s)

        self._progress(
            "Промотка страницы: достигнут лимит циклов стабилизации, "
            "страница оставлена внизу."
        )

    def _scroll_by_percent_steps(
        self,
        driver,
        steps_percent: tuple[int, ...],
        *,
        step_delay_s: float,
    ) -> None:
        for step_percent in steps_percent:
            self._check_cancelled()
            current_y, max_y, _viewport_h = self._read_scroll_state(driver)
            clamped_percent = max(0, min(100, int(step_percent)))
            target_y = int(round(max(0, max_y) * (clamped_percent / 100.0)))
            delta_px = target_y - int(current_y)
            duration_ms = max(120, int(step_delay_s * 1000.0) - 40)
            self._smooth_scroll(driver, delta_px, duration_ms)
            self._sleep_with_cancel(step_delay_s)

    def _sleep_with_cancel(self, seconds: float) -> None:
        remaining = max(0.0, float(seconds))
        while remaining > 0.0:
            self._check_cancelled()
            chunk = min(0.05, remaining)
            time.sleep(chunk)
            remaining -= chunk

    def _read_body_scroll_height(self, driver) -> int:
        raw = driver.execute_script(
            """
            const doc = document.documentElement || document.body;
            const body = document.body || doc;
            const bodyHeight = Math.max(
                (body && body.scrollHeight) || 0,
                (doc && doc.scrollHeight) || 0,
                0
            );
            return bodyHeight;
            """
        )
        try:
            return max(0, int(float(raw)))
        except Exception:
            return 0

    def _scroll_to_page_edge(self, driver, *, direction: int, max_steps: int = 1600) -> None:
        if direction not in (-1, 1):
            raise RuntimeError("Внутренняя ошибка: направление прокрутки должно быть -1 или 1.")

        stalled_steps = 0
        edge_hits = 0
        for _ in range(max(1, int(max_steps))):
            self._check_cancelled()
            current_y, max_y, viewport_h = self._read_scroll_state(driver)
            target_y = max_y if direction > 0 else 0
            remaining = target_y - current_y
            if abs(remaining) <= 2:
                edge_hits += 1
                if direction > 0 and self._wait_for_scroll_growth(
                    driver, baseline_max_y=max_y, timeout_s=0.55
                ):
                    edge_hits = 0
                    continue
                if edge_hits >= 2:
                    return
                time.sleep(0.05)
                continue

            edge_hits = 0
            chunk = max(90, int(viewport_h * 0.25))
            step_px = min(abs(int(remaining)), chunk)
            if step_px <= 0:
                return

            delta_px = step_px if direction > 0 else -step_px
            duration_ms = max(220, min(760, int(step_px * 2.0)))
            self._smooth_scroll(driver, delta_px, duration_ms)

            self._check_cancelled()
            new_y, new_max_y, _ = self._read_scroll_state(driver)
            moved = abs(new_y - current_y)
            if moved < 1:
                stalled_steps += 1
            else:
                stalled_steps = 0

            if stalled_steps >= 5:
                return
            if direction > 0 and new_y >= new_max_y - 2:
                if self._wait_for_scroll_growth(driver, baseline_max_y=new_max_y, timeout_s=0.55):
                    stalled_steps = 0
                    continue
                return
            if direction < 0 and new_y <= 2:
                return

            time.sleep(0.03)

    def _wait_for_scroll_growth(self, driver, *, baseline_max_y: int, timeout_s: float) -> bool:
        deadline = time.monotonic() + max(0.1, float(timeout_s))
        while time.monotonic() < deadline:
            self._check_cancelled()
            time.sleep(0.08)
            _current_y, max_y, _viewport_h = self._read_scroll_state(driver)
            if max_y > int(baseline_max_y) + 2:
                return True
        return False

    def _read_scroll_state(self, driver) -> tuple[int, int, int]:
        raw = driver.execute_script(
            """
            const doc = document.documentElement || document.body;
            const body = document.body || doc;
            const scrolling = document.scrollingElement || doc || body;
            const viewport = Math.max(
                window.innerHeight || 0,
                (doc && doc.clientHeight) || 0,
                320
            );
            const scrollHeight = Math.max(
                (doc && doc.scrollHeight) || 0,
                (scrolling && scrolling.scrollHeight) || 0,
                (body && body.scrollHeight) || 0,
                viewport
            );
            const maxY = Math.max(0, scrollHeight - viewport);
            const currentY = Math.max(
                (scrolling && scrolling.scrollTop) || 0,
                window.scrollY || window.pageYOffset || 0,
                0
            );
            return [currentY, maxY, viewport];
            """
        )
        if isinstance(raw, (list, tuple)) and len(raw) >= 3:
            try:
                y = int(float(raw[0]))
                max_y = int(float(raw[1]))
                viewport_h = max(320, int(float(raw[2])))
                return y, max_y, viewport_h
            except Exception:
                pass
        return 0, 0, 800

    def _set_scroll_y(self, driver, target_y: int) -> int:
        raw = driver.execute_script(
            """
            const target = Math.max(0, Number(arguments[0]) || 0);
            const doc = document.documentElement || document.body;
            const body = document.body || doc;
            const scrolling = document.scrollingElement || doc || body;
            const viewport = Math.max(
                window.innerHeight || 0,
                (doc && doc.clientHeight) || 0,
                320
            );
            const scrollHeight = Math.max(
                (doc && doc.scrollHeight) || 0,
                (scrolling && scrolling.scrollHeight) || 0,
                (body && body.scrollHeight) || 0,
                viewport
            );
            const maxY = Math.max(0, scrollHeight - viewport);
            const clamped = Math.max(0, Math.min(target, maxY));
            if (scrolling) {
                scrolling.scrollTop = clamped;
            }
            window.scrollTo(0, clamped);
            const currentY = Math.max(
                (scrolling && scrolling.scrollTop) || 0,
                window.scrollY || window.pageYOffset || 0,
                0
            );
            return currentY;
            """,
            int(target_y),
        )
        try:
            return int(float(raw))
        except Exception:
            return int(target_y)

    def _smooth_scroll(self, driver, delta_px: int, duration_ms: int) -> None:
        if int(delta_px) == 0:
            return
        duration_ms = max(120, min(1200, int(duration_ms)))
        delta_px = int(delta_px)

        try:
            driver.execute_async_script(
                """
                const done = arguments[arguments.length - 1];
                const delta = Number(arguments[0]) || 0;
                const durationMs = Math.max(120, Number(arguments[1]) || 220);
                if (!Number.isFinite(delta) || Math.abs(delta) < 1) {
                    done(0);
                    return;
                }

                const doc = document.documentElement || document.body;
                const body = document.body || doc;
                const scrolling = document.scrollingElement || doc || body;
                const viewport = Math.max(
                    window.innerHeight || 0,
                    (doc && doc.clientHeight) || 0,
                    320
                );
                const scrollHeight = Math.max(
                    (doc && doc.scrollHeight) || 0,
                    (scrolling && scrolling.scrollHeight) || 0,
                    (body && body.scrollHeight) || 0,
                    viewport
                );
                const maxY = Math.max(0, scrollHeight - viewport);
                const startY = Math.max(
                    (scrolling && scrolling.scrollTop) || 0,
                    window.scrollY || window.pageYOffset || 0,
                    0
                );
                const targetY = Math.max(0, Math.min(maxY, startY + delta));
                const distance = targetY - startY;
                if (Math.abs(distance) < 1) {
                    done(startY);
                    return;
                }

                const easeInOutCubic = (t) => {
                    if (t < 0.5) return 4 * t * t * t;
                    return 1 - Math.pow(-2 * t + 2, 3) / 2;
                };
                const startTs = performance.now();
                const applyScroll = (y) => {
                    if (scrolling) scrolling.scrollTop = y;
                    window.scrollTo(0, y);
                };
                const tick = (now) => {
                    const t = Math.min(1, (now - startTs) / durationMs);
                    const y = Math.round(startY + distance * easeInOutCubic(t));
                    applyScroll(y);
                    if (t < 1) {
                        window.requestAnimationFrame(tick);
                        return;
                    }
                    const currentY = Math.max(
                        (scrolling && scrolling.scrollTop) || 0,
                        window.scrollY || window.pageYOffset || 0,
                        0
                    );
                    done(currentY);
                };
                window.requestAnimationFrame(tick);
                """,
                delta_px,
                duration_ms,
            )
            return
        except Exception:
            pass

        # Fallback: синхронные шаги, если async script / rAF не сработали.
        start_y, _max_y, _viewport_h = self._read_scroll_state(driver)
        duration_s = max(0.06, float(duration_ms) / 1000.0)
        frame_dt = 1.0 / 60.0
        frames = max(4, min(120, int(duration_s / frame_dt)))

        def ease_in_out_cubic(t: float) -> float:
            if t < 0.5:
                return 4.0 * t * t * t
            return 1.0 - ((-2.0 * t + 2.0) ** 3) / 2.0

        for i in range(1, frames + 1):
            self._check_cancelled()
            p = i / frames
            eased = ease_in_out_cubic(p)
            target = start_y + int(round(int(delta_px) * eased))
            self._set_scroll_y(driver, target)
            if i < frames:
                time.sleep(frame_dt)

    def _fetch_from_browser(self, pattern: str) -> list[Image.Image]:
        self._check_cancelled()
        driver = self._require_browser()
        self._wait_for_page_loaded(driver, timeout=25)

        try:
            from selenium.webdriver.common.by import By
            from selenium.webdriver.support import expected_conditions as EC
            from selenium.webdriver.support.ui import WebDriverWait
        except Exception as exc:
            raise RuntimeError(f"Selenium недоступен для выкачивания из браузера: {exc}") from exc

        try:
            WebDriverWait(driver, 10).until(EC.presence_of_all_elements_located((By.CSS_SELECTOR, "img, a")))
        except Exception:
            pass

        page_url = driver.current_url
        candidates: list[str] = self._collect_browser_candidates(driver)

        # Fallback на прямой Selenium-обход, если JS-сбор вернул пусто.
        if not candidates:
            for el in driver.find_elements(By.TAG_NAME, "img"):
                self._check_cancelled()
                try:
                    src = el.get_attribute("src") or ""
                    if src:
                        candidates.append(src)
                except Exception:
                    continue
            for el in driver.find_elements(By.TAG_NAME, "a"):
                self._check_cancelled()
                try:
                    href = el.get_attribute("href") or ""
                    if href:
                        candidates.append(href)
                except Exception:
                    continue

        absolute_candidates: list[str] = []
        for candidate in candidates:
            try:
                absolute_candidates.append(urljoin(page_url, candidate))
            except Exception:
                continue

        matcher = compile_wildcard_prefixes(pattern) if pattern else None
        seen: set[str] = set()
        filtered: list[str] = []
        if matcher is not None:
            for item in absolute_candidates:
                if item in seen:
                    continue
                if matcher.search(item):
                    seen.add(item)
                    filtered.append(item)
        else:
            for item in absolute_candidates:
                if item in seen:
                    continue
                if re.search(r"\.(?:jpe?g|png|webp)(?:\?|$)", item, re.IGNORECASE):
                    seen.add(item)
                    filtered.append(item)

        if not filtered and matcher is None:
            for item in absolute_candidates:
                if item in seen:
                    continue
                try:
                    parsed = urlparse(item)
                except Exception:
                    continue
                if parsed.scheme not in ("http", "https", "file"):
                    continue
                seen.add(item)
                filtered.append(item)

        if not filtered:
            raise RuntimeError("В текущей вкладке не найдены ссылки для выкачивания.")

        try:
            from modules.browser_f import browserlike_headers, get_origin, transfer_cookies_from_selenium
        except Exception as exc:
            raise RuntimeError(f"Не удалось загрузить browser helpers: {exc}") from exc

        session = requests.Session()
        headers = browserlike_headers(driver)
        transfer_cookies_from_selenium(driver, session)

        page_origin = get_origin(page_url)
        out: list[Image.Image] = []
        total = len(filtered)
        for idx, link in enumerate(filtered, start=1):
            self._check_cancelled()
            self._progress(f"Выкачивание из браузера: {idx}/{total}")
            try:
                req_headers = dict(headers)
                req_headers["Referer"] = page_origin + "/"
                response = session.get(link, headers=req_headers, timeout=60)
                if not response.ok:
                    continue
                image = Image.open(BytesIO(response.content)).convert("RGB")
                if image.width > 0 and image.height > 0:
                    out.append(image)
            except Exception:
                continue

        if not out:
            raise RuntimeError("Не удалось скачать изображения из найденных ссылок текущей вкладки.")
        return out

    def _collect_browser_candidates(self, driver) -> list[str]:
        self._check_cancelled()
        raw = driver.execute_script(
            """
            const out = [];
            const seen = new Set();
            const add = (v) => {
                if (typeof v !== "string") return;
                const val = v.trim();
                if (!val || seen.has(val)) return;
                seen.add(val);
                out.push(val);
            };
            const addSrcSet = (text) => {
                if (typeof text !== "string") return;
                for (const part of text.split(",")) {
                    const token = part.trim().split(/\\s+/)[0] || "";
                    add(token);
                }
            };

            for (const img of document.querySelectorAll("img")) {
                add(img.currentSrc || "");
                add(img.src || "");
                add(img.getAttribute("src"));
                add(img.getAttribute("data-src"));
                add(img.getAttribute("data-lazy-src"));
                add(img.getAttribute("data-original"));
                add(img.getAttribute("data-url"));
                addSrcSet(img.getAttribute("srcset") || "");
                addSrcSet(img.getAttribute("data-srcset") || "");
            }
            for (const source of document.querySelectorAll("source")) {
                add(source.src || "");
                add(source.getAttribute("src"));
                addSrcSet(source.srcset || "");
                addSrcSet(source.getAttribute("srcset") || "");
                addSrcSet(source.getAttribute("data-srcset") || "");
            }
            for (const a of document.querySelectorAll("a[href]")) {
                add(a.getAttribute("href"));
            }
            return out;
            """
        )
        if not isinstance(raw, list):
            return []
        out: list[str] = []
        for item in raw:
            self._check_cancelled()
            if isinstance(item, str):
                value = item.strip()
                if value:
                    out.append(value)
        return out

    def _resolve_browser_name(self, requested_name: str) -> str:
        name = (requested_name or "").strip()
        if name:
            return name
        try:
            from modules.new_project.downloaders import detect_available_browsers

            available = detect_available_browsers()
            if available:
                return available[0]
        except Exception:
            pass
        return "Firefox"

    def _ensure_browser(self, requested_name: str):
        browser_name = self._resolve_browser_name(requested_name)
        if self._browser_driver is not None:
            need_restart = False
            if self._browser_name.strip().lower() != browser_name.strip().lower():
                need_restart = True
            else:
                try:
                    _ = self._browser_driver.current_url
                except Exception:
                    need_restart = True
            if need_restart:
                self._shutdown_browser()

        if self._browser_driver is not None:
            return self._browser_driver

        try:
            from modules.browser_f import build_browser
        except Exception as exc:
            raise RuntimeError(f"Не удалось загрузить модуль браузера: {exc}") from exc

        try:
            self._browser_driver, self._browser_tmp_profile_dir = build_browser(True, browser_name)
            self._browser_name = browser_name
            return self._browser_driver
        except Exception as exc:
            self._shutdown_browser()
            raise RuntimeError(f"Не удалось запустить браузер '{browser_name}': {exc}") from exc

    def _require_browser(self):
        if self._browser_driver is None:
            raise RuntimeError("Браузер не открыт. Добавьте узел 'Открыть URL' перед текущим узлом.")
        try:
            _ = self._browser_driver.current_url
        except Exception:
            self._shutdown_browser()
            raise RuntimeError("Сессия браузера недоступна. Выполните узел 'Открыть URL' заново.")
        return self._browser_driver

    def _wait_for_page_loaded(self, driver, timeout: int) -> None:
        deadline = time.monotonic() + max(1, int(timeout))
        while time.monotonic() < deadline:
            self._check_cancelled()
            try:
                state = str(driver.execute_script("return document.readyState || ''")).lower()
                has_body = bool(driver.execute_script("return !!document.body;"))
                if state == "complete" and has_body:
                    return
            except Exception:
                pass
            time.sleep(0.10)
        raise RuntimeError(f"Страница не загрузилась полностью за {timeout} сек.")

    def _shutdown_browser(self) -> None:
        if self._browser_driver is not None:
            try:
                self._browser_driver.quit()
            except Exception:
                pass
        self._browser_driver = None

        if self._browser_tmp_profile_dir:
            try:
                from modules.browser_f import cleanup_browser_runtime

                cleanup_browser_runtime(self._browser_name, self._browser_tmp_profile_dir)
            except Exception:
                pass
        self._browser_tmp_profile_dir = None
        self._browser_name = ""

    @staticmethod
    def _normalize_http_url(raw: str) -> str:
        value = str(raw or "")
        value = value.translate(_CONTROL).strip()
        value = value.replace("\\", "/")

        if re.match(r"^[a-zA-Z]:/[^?]*", value):
            try:
                return Path(value).as_uri()
            except Exception:
                pass

        has_scheme = re.match(r"^[a-zA-Z][a-zA-Z0-9+.\-]*://", value) is not None
        if not has_scheme:
            if value.startswith("www."):
                value = "https://" + value
            elif re.match(r"^[\w\-\.]+\.[a-zA-Z]{2,}(/|$)", value):
                value = "https://" + value

        parsed = urlparse(value)
        if parsed.scheme not in ("http", "https", "file"):
            raise RuntimeError("Поддерживаются только URL схемы http/https/file.")
        if parsed.scheme in ("http", "https") and not parsed.netloc:
            raise RuntimeError("В URL отсутствует домен.")

        safe_path = quote(parsed.path or "/", safe="/%:@&=+$,;~*'()")
        safe_query = parsed.query.replace(" ", "%20")
        safe_frag = parsed.fragment.replace(" ", "%20")
        return urlunparse((parsed.scheme, parsed.netloc, safe_path, parsed.params, safe_query, safe_frag))

    @staticmethod
    def _default_waifu2x_exec_path() -> Path:
        root = Path(__file__).resolve().parents[3]
        if sys.platform.startswith("win"):
            return root / "waifu2x" / "Win" / "waifu2x-ncnn-vulkan.exe"
        if sys.platform.startswith("darwin"):
            return root / "waifu2x" / "Mac" / "waifu2x-ncnn-vulkan"
        return root / "waifu2x" / "Lin" / "waifu2x-ncnn-vulkan"

    def _run_waifu2x(
        self,
        image_list: list[Image.Image],
        *,
        noise: object,
        scale: object,
        tile: object,
        exec_path: object,
    ) -> list[Image.Image]:
        self._check_cancelled()
        try:
            n_val = int(noise if noise is not None else 3)
            s_val = int(scale if scale is not None else 1)
            t_val = int(tile if tile is not None else 384)
        except Exception as exc:
            raise RuntimeError("Параметры waifu2x (-n/-s/-t) должны быть целыми.") from exc

        if n_val not in (-1, 0, 1, 2, 3):
            raise RuntimeError("Параметр waifu2x -n должен быть одним из: -1, 0, 1, 2, 3.")
        if s_val not in (1, 2, 4, 8, 16, 32):
            raise RuntimeError("Параметр waifu2x -s должен быть одним из: 1, 2, 4, 8, 16, 32.")
        if t_val != 0 and t_val < 32:
            raise RuntimeError("Параметр waifu2x -t должен быть 0 или >= 32.")

        custom_exec = str(exec_path or "").strip()
        waifu_exec = Path(custom_exec).expanduser() if custom_exec else self._default_waifu2x_exec_path()
        if not waifu_exec.exists():
            raise RuntimeError(f"Не найден waifu2x исполняемый файл: {waifu_exec}")

        self._progress("waifu2x: подготовка входных изображений")
        with tempfile.TemporaryDirectory(prefix="mf_w2x_") as temp_dir:
            in_dir = Path(temp_dir) / "in"
            out_dir = Path(temp_dir) / "out"
            in_dir.mkdir(parents=True, exist_ok=True)
            out_dir.mkdir(parents=True, exist_ok=True)

            for idx, image in enumerate(image_list, start=1):
                self._check_cancelled()
                image.convert("RGB").save(in_dir / f"{idx:04d}.png", format="PNG")

            cmd = [
                str(waifu_exec),
                "-i",
                str(in_dir),
                "-o",
                str(out_dir),
                "-n",
                str(n_val),
                "-s",
                str(s_val),
                "-t",
                str(t_val),
            ]
            self._progress("waifu2x: обработка")
            proc = None
            try:
                proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                while proc.poll() is None:
                    self._check_cancelled()
                    time.sleep(0.12)
                stdout_text, stderr_text = proc.communicate(timeout=5)
                if proc.returncode != 0:
                    err_text = (stderr_text or stdout_text or "").strip()
                    raise RuntimeError(f"waifu2x завершился с ошибкой: {err_text[:4000]}")
            except PipelineExecutionStopped:
                if proc is not None and proc.poll() is None:
                    try:
                        proc.terminate()
                        proc.wait(timeout=2)
                    except Exception:
                        try:
                            proc.kill()
                        except Exception:
                            pass
                raise
            except subprocess.CalledProcessError as exc:
                err_text = (exc.stderr or exc.stdout or str(exc)).strip()
                raise RuntimeError(f"waifu2x завершился с ошибкой: {err_text[:4000]}") from exc
            except Exception as exc:
                raise RuntimeError(f"Ошибка запуска waifu2x: {exc}") from exc

            out_files = sorted(
                [
                    p
                    for p in out_dir.iterdir()
                    if p.is_file() and p.suffix.lower() in (".png", ".jpg", ".jpeg", ".webp")
                ]
            )
            if not out_files:
                raise RuntimeError("waifu2x не вернул выходные изображения.")

            out_images: list[Image.Image] = []
            for out_file in out_files:
                self._check_cancelled()
                try:
                    out_images.append(Image.open(out_file).convert("RGB"))
                except Exception:
                    continue
            if not out_images:
                raise RuntimeError("Не удалось прочитать результаты waifu2x.")
            return out_images

    def _save_images_to_folder(self, image_list: object, folder: str) -> int:
        if not isinstance(image_list, list):
            raise RuntimeError("Вход 'Картинки' не содержит список изображений.")

        out_dir = Path(folder).expanduser()
        out_dir.mkdir(parents=True, exist_ok=True)

        saved = 0
        for idx, item in enumerate(image_list, start=1):
            self._check_cancelled()
            if not isinstance(item, Image.Image):
                continue
            output_path = out_dir / f"{idx:03d}.png"
            item.save(output_path, format="PNG")
            saved += 1

        if saved == 0:
            raise RuntimeError("На входе 'Картинки' нет валидных изображений для сохранения.")

        return saved
