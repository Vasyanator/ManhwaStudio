from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/window.py
Главное окно node-editor для массовой обработки (оркестрация scene/view, dock-панелей и узлов).

Main items:
- `BatchProcessingNodesWindow`: создаёт UI, управляет переменными и спавном узлов.
- `_on_save_pipeline_requested` / `_on_load_pipeline_requested`: JSON-сериализация пайплайна.
- `_BatchGraphRunWorker`: фоновое реальное исполнение pipeline-графа.
- `_on_stop_requested`: мягкая остановка исполнения по кнопке `Остановить процесс`.
- `_variables`: словарь переменных (тип + persist) для variable read/write узлов.
- `_variable_nodes`: живой список variable-узлов для обновления после CRUD переменных.
- `_collect_node_params`: извлечение/валидация runtime-параметров узлов перед запуском.
"""

import threading
import json
from pathlib import Path
from typing import Optional

from PyQt6 import QtCore, QtWidgets

from .constants import KIND_DATA, KIND_EXEC, TYPE_INT, TYPE_STR
from .executor import (
    BatchPipelineExecutor,
    DataEdgeSpec,
    ExecEdgeSpec,
    GraphRuntimeSnapshot,
    NodeRuntimeSpec,
    PipelineExecutionStopped,
    VariableRuntimeSpec,
)
from .graph import NodeGraphScene, NodeGraphView
from .graphics_items import NodeBlockItem, NodeConnectionItem, VariableNodeBlockItem
from .models import VariableDefinition
from .nodes import build_templates, create_node
from .nodes.fetch_from_browser import FetchFromBrowserParamsWidget
from .nodes.open_url import OpenUrlParamsWidget
from .nodes.stitch_split import StitchSplitParamsWidget
from .nodes.string_template import StringTemplateParamsWidget
from .nodes.waifu2x import Waifu2xParamsWidget
from .panels import NodesPalettePanel, VariablesPanel
from .widgets import NumberStartParamsWidget, StringStartParamsWidget


class _BatchGraphRunWorker(QtCore.QObject):
    progress = QtCore.pyqtSignal(str)
    failed = QtCore.pyqtSignal(str)
    finished = QtCore.pyqtSignal(str)

    def __init__(self, snapshot: GraphRuntimeSnapshot):
        super().__init__()
        self._snapshot = snapshot
        self._stop_event = threading.Event()

    @QtCore.pyqtSlot()
    def request_stop(self) -> None:
        self._stop_event.set()

    @QtCore.pyqtSlot()
    def run(self) -> None:
        try:
            executor = BatchPipelineExecutor(
                self._snapshot,
                progress_callback=self.progress.emit,
                should_stop_callback=self._stop_event.is_set,
            )
            self.finished.emit(executor.execute())
        except PipelineExecutionStopped as exc:
            self.finished.emit(str(exc))
        except Exception as exc:
            self.failed.emit(f"{type(exc).__name__}: {exc}")


class BatchProcessingNodesWindow(QtWidgets.QMainWindow):
    def __init__(self, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(parent)
        self.setWindowTitle("Массовая обработка")
        self.resize(1360, 840)

        self._templates = build_templates()
        self._variables: dict[str, VariableDefinition] = {}
        self._variable_nodes: list[VariableNodeBlockItem] = []
        self._node_spawn_counter = 0
        self._run_thread: Optional[QtCore.QThread] = None
        self._run_worker: Optional[_BatchGraphRunWorker] = None

        self._scene = NodeGraphScene(self)
        self._scene.setSceneRect(-1800.0, -1200.0, 3600.0, 2400.0)
        self._view = NodeGraphView(self._scene, self)

        info = QtWidgets.QLabel(
            "Линии выполнения (жёлтые, пунктир) задают порядок: ветвление = параллель, "
            "узел с несколькими входами ждёт все ветки. "
            "Линии данных (цветные) передают int/str/список картинок и требуют совпадения типов.",
            self,
        )
        info.setWordWrap(True)
        info.setStyleSheet("color: #cbd5e1;")

        center = QtWidgets.QWidget(self)
        center_layout = QtWidgets.QVBoxLayout(center)
        center_layout.setContentsMargins(8, 8, 8, 8)
        center_layout.setSpacing(6)
        center_layout.addWidget(info)
        center_layout.addWidget(self._view, 1)
        self.setCentralWidget(center)

        self._setup_left_palette_dock()
        self._setup_right_variables_dock()
        self._setup_toolbar()
        self.statusBar().showMessage("Готово")

        self._add_variable_internal("download_url", TYPE_STR, True)
        self._add_variable_internal("page_index", TYPE_INT, False)

        self._create_demo_nodes()
        self._refresh_variables_ui()

    def _setup_left_palette_dock(self) -> None:
        self._palette_panel = NodesPalettePanel(self._templates, self)
        self._palette_panel.add_node_requested.connect(self._on_add_node_requested)

        self._nodes_dock = QtWidgets.QDockWidget("Узлы", self)
        self._nodes_dock.setWidget(self._palette_panel)
        self._nodes_dock.setFeatures(
            QtWidgets.QDockWidget.DockWidgetFeature.DockWidgetClosable
            | QtWidgets.QDockWidget.DockWidgetFeature.DockWidgetMovable
            | QtWidgets.QDockWidget.DockWidgetFeature.DockWidgetFloatable
        )
        self.addDockWidget(QtCore.Qt.DockWidgetArea.LeftDockWidgetArea, self._nodes_dock)

    def _setup_right_variables_dock(self) -> None:
        self._variables_panel = VariablesPanel(self)
        self._variables_panel.variable_add_requested.connect(self._on_variable_add_requested)
        self._variables_panel.variable_remove_requested.connect(self._on_variable_remove_requested)
        self._variables_panel.add_read_node_requested.connect(
            lambda name: self._on_add_node_requested("variable_read", preferred_variable=name)
        )
        self._variables_panel.add_write_node_requested.connect(
            lambda name: self._on_add_node_requested("variable_write", preferred_variable=name)
        )

        self._variables_dock = QtWidgets.QDockWidget("Переменные", self)
        self._variables_dock.setWidget(self._variables_panel)
        self._variables_dock.setFeatures(
            QtWidgets.QDockWidget.DockWidgetFeature.DockWidgetClosable
            | QtWidgets.QDockWidget.DockWidgetFeature.DockWidgetMovable
            | QtWidgets.QDockWidget.DockWidgetFeature.DockWidgetFloatable
        )
        self.addDockWidget(QtCore.Qt.DockWidgetArea.RightDockWidgetArea, self._variables_dock)

    def _setup_toolbar(self) -> None:
        toolbar = self.addToolBar("Панели")
        toolbar.setMovable(False)
        toolbar.addAction(self._nodes_dock.toggleViewAction())
        toolbar.addAction(self._variables_dock.toggleViewAction())
        toolbar.addSeparator()
        self._save_pipeline_action = toolbar.addAction("Сохранить пайплайн")
        self._save_pipeline_action.triggered.connect(self._on_save_pipeline_requested)
        self._load_pipeline_action = toolbar.addAction("Загрузить пайплайн")
        self._load_pipeline_action.triggered.connect(self._on_load_pipeline_requested)
        toolbar.addSeparator()
        self._run_action = toolbar.addAction("Запустить процесс")
        self._run_action.triggered.connect(self._on_run_requested)
        self._stop_action = toolbar.addAction("Остановить процесс")
        self._stop_action.setEnabled(False)
        self._stop_action.triggered.connect(self._on_stop_requested)

    def _create_demo_nodes(self) -> None:
        self._spawn_node("start_number", pos=QtCore.QPointF(-1050.0, -190.0))
        self._spawn_node("start_string", pos=QtCore.QPointF(-1050.0, 160.0))
        self._spawn_node("quick_downloader", pos=QtCore.QPointF(-570.0, -10.0))
        self._spawn_node("save_folder", pos=QtCore.QPointF(-110.0, -10.0))
        self._spawn_node("end", pos=QtCore.QPointF(350.0, -10.0))

    def _variable_list_sorted(self) -> list[VariableDefinition]:
        return [self._variables[k] for k in sorted(self._variables.keys())]

    def _get_variable(self, name: str) -> Optional[VariableDefinition]:
        return self._variables.get(name)

    def _on_add_node_requested(self, template_key: str, preferred_variable: Optional[str] = None) -> None:
        center = self._view.mapToScene(self._view.viewport().rect().center())
        offset = QtCore.QPointF((self._node_spawn_counter % 6) * 32.0, (self._node_spawn_counter % 5) * 22.0)
        self._node_spawn_counter += 1
        self._spawn_node(template_key, pos=center + offset, preferred_variable=preferred_variable)

    def _spawn_node(
        self,
        template_key: str,
        *,
        pos: QtCore.QPointF,
        preferred_variable: Optional[str] = None,
    ) -> Optional[NodeBlockItem]:
        node = self._create_node_item(template_key, preferred_variable=preferred_variable)
        if node is None:
            return None
        self._scene.addItem(node)
        node.setPos(pos)
        return node

    def _create_node_item(self, template_key: str, preferred_variable: Optional[str] = None) -> Optional[NodeBlockItem]:
        node = create_node(
            template_key,
            variable_resolver=self._get_variable,
            variables=self._variable_list_sorted(),
            preferred_variable=preferred_variable,
        )
        if node is None:
            QtWidgets.QMessageBox.warning(self, "Узлы", f"Неизвестный тип узла: {template_key}")
            return None

        node.set_template_key(template_key)
        if isinstance(node, VariableNodeBlockItem):
            self._variable_nodes.append(node)
        return node

    def _on_variable_add_requested(self, name: str, data_type: str, persist: bool) -> None:
        if any(ch.isspace() for ch in name):
            QtWidgets.QMessageBox.warning(self, "Переменные", "Имя переменной не должно содержать пробелы.")
            return
        if name in self._variables:
            QtWidgets.QMessageBox.warning(self, "Переменные", f"Переменная '{name}' уже существует.")
            return
        self._add_variable_internal(name, data_type, persist)
        self._refresh_variables_ui()

    def _on_variable_remove_requested(self, name: str) -> None:
        if name not in self._variables:
            return
        self._variables.pop(name, None)
        self._refresh_variables_ui()

    def _add_variable_internal(self, name: str, data_type: str, persist: bool) -> None:
        self._variables[name] = VariableDefinition(
            name=name,
            data_type=data_type,
            persist_between_cycles=persist,
        )

    def _refresh_variables_ui(self) -> None:
        variables = self._variable_list_sorted()
        self._variables_panel.set_variables(variables)
        self._refresh_variable_nodes(variables)

    def _refresh_variable_nodes(self, variables: list[VariableDefinition]) -> None:
        alive_nodes: list[VariableNodeBlockItem] = []
        for node in self._variable_nodes:
            if node.scene() is None:
                continue
            selected = node.selected_variable_name()
            node.set_variable_options(variables, selected_name=selected)
            alive_nodes.append(node)
        self._variable_nodes = alive_nodes

    def _on_save_pipeline_requested(self) -> None:
        try:
            payload = self._serialize_pipeline_payload()
        except Exception as exc:
            QtWidgets.QMessageBox.critical(self, "Сохранение пайплайна", str(exc))
            return

        path, _ = QtWidgets.QFileDialog.getSaveFileName(
            self,
            "Сохранить пайплайн",
            "pipeline.json",
            "JSON files (*.json);;All files (*)",
        )
        if not path:
            return
        out_path = Path(path)
        if out_path.suffix.lower() != ".json":
            out_path = out_path.with_suffix(".json")
        try:
            out_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
        except Exception as exc:
            QtWidgets.QMessageBox.critical(self, "Сохранение пайплайна", f"Не удалось сохранить файл:\n{exc}")
            return
        self.statusBar().showMessage(f"Пайплайн сохранён: {out_path}", 5000)

    def _on_load_pipeline_requested(self) -> None:
        if self._run_thread is not None and self._run_thread.isRunning():
            QtWidgets.QMessageBox.warning(self, "Загрузка пайплайна", "Сначала остановите текущий запуск.")
            return

        path, _ = QtWidgets.QFileDialog.getOpenFileName(
            self,
            "Загрузить пайплайн",
            "",
            "JSON files (*.json);;All files (*)",
        )
        if not path:
            return
        in_path = Path(path)
        try:
            payload = json.loads(in_path.read_text(encoding="utf-8"))
        except Exception as exc:
            QtWidgets.QMessageBox.critical(self, "Загрузка пайплайна", f"Не удалось прочитать JSON:\n{exc}")
            return

        try:
            self._load_pipeline_payload(payload)
        except Exception as exc:
            QtWidgets.QMessageBox.critical(self, "Загрузка пайплайна", str(exc))
            return
        self.statusBar().showMessage(f"Пайплайн загружен: {in_path}", 5000)

    def _serialize_pipeline_payload(self) -> dict[str, object]:
        node_items = [item for item in self._scene.items() if isinstance(item, NodeBlockItem)]
        node_items.sort(key=lambda node: (node.pos().x(), node.pos().y(), id(node)))
        node_ids: dict[NodeBlockItem, int] = {node: idx + 1 for idx, node in enumerate(node_items)}

        nodes_payload: list[dict[str, object]] = []
        for node in node_items:
            template_key = node.template_key().strip()
            if not template_key:
                continue
            data_inputs = [
                socket.spec.name
                for socket in node.sockets()
                if socket.spec.kind == KIND_DATA and socket.spec.direction == "in"
            ]
            params = self._collect_node_params(node, template_key, data_inputs)
            nodes_payload.append(
                {
                    "id": node_ids[node],
                    "template_key": template_key,
                    "x": float(node.pos().x()),
                    "y": float(node.pos().y()),
                    "params": params,
                }
            )

        edges_payload: list[dict[str, object]] = []
        for item in self._scene.items():
            if not isinstance(item, NodeConnectionItem):
                continue
            if item.out_socket is None or item.in_socket is None:
                continue
            src_node = item.out_socket.parentItem()
            dst_node = item.in_socket.parentItem()
            if not isinstance(src_node, NodeBlockItem) or not isinstance(dst_node, NodeBlockItem):
                continue
            src_id = node_ids.get(src_node)
            dst_id = node_ids.get(dst_node)
            if src_id is None or dst_id is None:
                continue
            edges_payload.append(
                {
                    "kind": item.kind,
                    "src_node_id": src_id,
                    "src_socket": item.out_socket.spec.name,
                    "dst_node_id": dst_id,
                    "dst_socket": item.in_socket.spec.name,
                }
            )

        edges_payload.sort(
            key=lambda edge: (
                str(edge.get("kind", "")),
                int(edge.get("src_node_id", 0)),
                str(edge.get("src_socket", "")),
                int(edge.get("dst_node_id", 0)),
                str(edge.get("dst_socket", "")),
            )
        )

        variables_payload = [
            {
                "name": var.name,
                "data_type": var.data_type,
                "persist_between_cycles": bool(var.persist_between_cycles),
            }
            for var in self._variable_list_sorted()
        ]

        return {
            "version": 1,
            "variables": variables_payload,
            "nodes": nodes_payload,
            "edges": edges_payload,
        }

    def _load_pipeline_payload(self, payload: object) -> None:
        if not isinstance(payload, dict):
            raise RuntimeError("JSON пайплайна должен быть объектом.")

        nodes_data = payload.get("nodes", [])
        edges_data = payload.get("edges", [])
        variables_data = payload.get("variables", [])
        if not isinstance(nodes_data, list) or not isinstance(edges_data, list) or not isinstance(variables_data, list):
            raise RuntimeError("JSON пайплайна повреждён: ожидаются массивы 'variables', 'nodes', 'edges'.")

        self._scene.clear()
        self._variable_nodes = []
        self._node_spawn_counter = 0

        self._variables.clear()
        for raw_var in variables_data:
            if not isinstance(raw_var, dict):
                continue
            name = str(raw_var.get("name", "") or "").strip()
            if not name:
                continue
            data_type = str(raw_var.get("data_type", TYPE_STR) or TYPE_STR).strip() or TYPE_STR
            persist = bool(raw_var.get("persist_between_cycles", False))
            self._add_variable_internal(name, data_type, persist)
        self._refresh_variables_ui()

        node_by_id: dict[int, NodeBlockItem] = {}
        for raw_node in nodes_data:
            if not isinstance(raw_node, dict):
                continue
            try:
                node_id = int(raw_node.get("id", 0))
            except Exception:
                continue
            template_key = str(raw_node.get("template_key", "") or "").strip()
            if not template_key:
                continue
            params = raw_node.get("params", {})
            if not isinstance(params, dict):
                params = {}
            preferred_variable = None
            if template_key in ("variable_read", "variable_write"):
                preferred_variable = str(params.get("variable_name", "") or "").strip() or None

            node = self._create_node_item(template_key, preferred_variable=preferred_variable)
            if node is None:
                continue
            self._scene.addItem(node)
            try:
                x = float(raw_node.get("x", 0.0))
                y = float(raw_node.get("y", 0.0))
            except Exception:
                x = 0.0
                y = 0.0
            node.setPos(QtCore.QPointF(x, y))
            self._apply_loaded_node_params(node, template_key, params)
            node_by_id[node_id] = node

        self._refresh_variables_ui()
        self._node_spawn_counter = len(node_by_id)

        for raw_edge in edges_data:
            if not isinstance(raw_edge, dict):
                continue
            kind = str(raw_edge.get("kind", "") or "").strip()
            if kind not in (KIND_EXEC, KIND_DATA):
                continue
            try:
                src_node_id = int(raw_edge.get("src_node_id", 0))
                dst_node_id = int(raw_edge.get("dst_node_id", 0))
            except Exception:
                continue
            src_socket_name = str(raw_edge.get("src_socket", "") or "").strip()
            dst_socket_name = str(raw_edge.get("dst_socket", "") or "").strip()
            if not src_socket_name or not dst_socket_name:
                continue

            src_node = node_by_id.get(src_node_id)
            dst_node = node_by_id.get(dst_node_id)
            if src_node is None or dst_node is None:
                continue
            out_socket = src_node.socket_by_name(src_socket_name)
            in_socket = dst_node.socket_by_name(dst_socket_name)
            if out_socket is None or in_socket is None:
                continue
            if out_socket.spec.direction != "out" or in_socket.spec.direction != "in":
                continue
            if out_socket.spec.kind != kind or in_socket.spec.kind != kind:
                continue
            if out_socket.parentItem() is in_socket.parentItem():
                continue

            if kind == KIND_DATA:
                out_types = set(out_socket.spec.accepted_data_types)
                in_types = set(in_socket.spec.accepted_data_types)
                if out_socket.spec.data_type:
                    out_types.add(out_socket.spec.data_type)
                if in_socket.spec.data_type:
                    in_types.add(in_socket.spec.data_type)
                if not out_types.intersection(in_types):
                    continue

            if not in_socket.spec.allow_multiple:
                if any(conn.in_socket is in_socket for conn in in_socket.connections):
                    continue
            if any(conn.out_socket is out_socket and conn.in_socket is in_socket for conn in out_socket.connections):
                continue

            conn = NodeConnectionItem(out_socket)
            self._scene.addItem(conn)
            conn.attach(out_socket, in_socket)

    def _apply_loaded_node_params(
        self,
        node: NodeBlockItem,
        template_key: str,
        params: dict[str, object],
    ) -> None:
        widget = node.params_widget()

        if template_key == "start_number" and isinstance(widget, NumberStartParamsWidget):
            if "start" in params:
                widget.start_spin.setValue(int(params.get("start", 0)))
            if "step" in params:
                widget.step_spin.setValue(int(params.get("step", 1)))
            if "end" in params:
                widget.end_spin.setValue(int(params.get("end", 0)))
            return

        if template_key == "start_string" and isinstance(widget, StringStartParamsWidget):
            widget.path_edit.setText(str(params.get("path", "") or ""))
            return

        if template_key == "string_template" and isinstance(widget, StringTemplateParamsWidget):
            widget.set_template_text(str(params.get("template", "") or ""))
            return

        if template_key in ("variable_read", "variable_write") and isinstance(node, VariableNodeBlockItem):
            selected = str(params.get("variable_name", "") or "").strip() or None
            node.set_variable_options(self._variable_list_sorted(), selected_name=selected)
            return

        if template_key == "stitch_split" and isinstance(widget, StitchSplitParamsWidget):
            k_val = params.get("K")
            widget.k_edit.setText("" if k_val in (None, "") else str(k_val))
            widget.hmax_edit.setText(str(params.get("Hmax", 19000)))
            widget.band_edit.setText(str(params.get("band_rows", 4)))
            widget.tol_edit.setText(str(params.get("tol", 15)))
            widget.radius_edit.setText(str(params.get("search_radius", 5500)))
            widget.prefer_up_checkbox.setChecked(bool(params.get("prefer_up_first", True)))
            return

        if template_key == "open_url" and isinstance(widget, OpenUrlParamsWidget):
            browser = str(params.get("browser", "") or "").strip()
            if browser:
                idx = widget.browser_combo.findText(browser)
                if idx < 0:
                    widget.browser_combo.addItem(browser)
                    idx = widget.browser_combo.findText(browser)
                if idx >= 0:
                    widget.browser_combo.setCurrentIndex(idx)
            return

        if template_key == "fetch_from_browser" and isinstance(widget, FetchFromBrowserParamsWidget):
            widget.pattern_edit.setText(str(params.get("pattern", "") or ""))
            return

        if template_key == "waifu2x" and isinstance(widget, Waifu2xParamsWidget):
            noise = str(params.get("noise", widget.noise()))
            scale = str(params.get("scale", widget.scale()))
            n_idx = widget.noise_combo.findText(noise)
            if n_idx >= 0:
                widget.noise_combo.setCurrentIndex(n_idx)
            s_idx = widget.scale_combo.findText(scale)
            if s_idx >= 0:
                widget.scale_combo.setCurrentIndex(s_idx)
            widget.tile_edit.setText(str(params.get("tile", widget.tile())))
            widget.exec_path_edit.setText(str(params.get("exec_path", "") or ""))
            return

    def _on_run_requested(self) -> None:
        if self._run_thread is not None and self._run_thread.isRunning():
            return

        try:
            snapshot = self._build_runtime_snapshot()
        except Exception as exc:
            QtWidgets.QMessageBox.warning(self, "Запуск", str(exc))
            return

        self._run_worker = _BatchGraphRunWorker(snapshot)
        self._run_thread = QtCore.QThread(self)
        self._run_worker.moveToThread(self._run_thread)

        self._run_thread.started.connect(self._run_worker.run)
        self._run_worker.progress.connect(self._on_run_progress)
        self._run_worker.finished.connect(self._on_run_finished)
        self._run_worker.failed.connect(self._on_run_failed)
        self._run_worker.finished.connect(self._run_thread.quit)
        self._run_worker.failed.connect(self._run_thread.quit)
        self._run_worker.finished.connect(self._run_worker.deleteLater)
        self._run_worker.failed.connect(self._run_worker.deleteLater)
        self._run_thread.finished.connect(self._on_run_worker_thread_finished)
        self._run_thread.finished.connect(self._run_thread.deleteLater)

        self._run_action.setEnabled(False)
        self._stop_action.setEnabled(True)
        self._save_pipeline_action.setEnabled(False)
        self._load_pipeline_action.setEnabled(False)
        self.statusBar().showMessage("Выполнение пайплайна...")
        self._run_thread.start()

    def _on_stop_requested(self) -> None:
        if self._run_thread is None or not self._run_thread.isRunning() or self._run_worker is None:
            return
        self._stop_action.setEnabled(False)
        self.statusBar().showMessage("Остановка выполнения...")
        # Воркер может долго исполнять Python-код без Qt event loop, поэтому queued-вызов
        # request_stop может обработаться слишком поздно. threading.Event потокобезопасен,
        # флаг ставим сразу из GUI-потока.
        self._run_worker.request_stop()

    def _build_runtime_snapshot(self) -> GraphRuntimeSnapshot:
        node_items = [item for item in self._scene.items() if isinstance(item, NodeBlockItem)]
        if not node_items:
            raise RuntimeError("Граф пуст. Добавьте узлы перед запуском.")

        node_items.sort(key=lambda node: (node.pos().x(), node.pos().y(), id(node)))
        node_ids: dict[NodeBlockItem, int] = {node: idx + 1 for idx, node in enumerate(node_items)}
        nodes: dict[int, NodeRuntimeSpec] = {}

        for node, node_id in node_ids.items():
            template_key = node.template_key().strip()
            if not template_key:
                raise RuntimeError(f"Узел '{node.title()}' не имеет template_key.")

            data_inputs = [
                socket.spec.name
                for socket in node.sockets()
                if socket.spec.kind == KIND_DATA and socket.spec.direction == "in"
            ]
            data_outputs = [
                socket.spec.name
                for socket in node.sockets()
                if socket.spec.kind == KIND_DATA and socket.spec.direction == "out"
            ]
            params = self._collect_node_params(node, template_key, data_inputs)
            nodes[node_id] = NodeRuntimeSpec(
                node_id=node_id,
                key=template_key,
                title=node.title(),
                params=params,
                data_inputs=data_inputs,
                data_outputs=data_outputs,
            )

        exec_edges: list[ExecEdgeSpec] = []
        data_edges: list[DataEdgeSpec] = []
        exec_edge_id = 1
        for item in self._scene.items():
            if not isinstance(item, NodeConnectionItem):
                continue
            if item.out_socket is None or item.in_socket is None:
                continue

            src_node = item.out_socket.parentItem()
            dst_node = item.in_socket.parentItem()
            if not isinstance(src_node, NodeBlockItem) or not isinstance(dst_node, NodeBlockItem):
                continue

            src_id = node_ids.get(src_node)
            dst_id = node_ids.get(dst_node)
            if src_id is None or dst_id is None:
                continue

            if item.kind == KIND_EXEC:
                exec_edges.append(
                    ExecEdgeSpec(
                        edge_id=exec_edge_id,
                        src_node_id=src_id,
                        src_socket=item.out_socket.spec.name,
                        dst_node_id=dst_id,
                        dst_socket=item.in_socket.spec.name,
                    )
                )
                exec_edge_id += 1
            elif item.kind == KIND_DATA:
                data_edges.append(
                    DataEdgeSpec(
                        src_node_id=src_id,
                        src_socket=item.out_socket.spec.name,
                        dst_node_id=dst_id,
                        dst_socket=item.in_socket.spec.name,
                    )
                )

        incoming_exec_counts: dict[int, int] = {}
        for edge in exec_edges:
            incoming_exec_counts[edge.dst_node_id] = incoming_exec_counts.get(edge.dst_node_id, 0) + 1
        for node in nodes.values():
            node.required_exec_inputs = incoming_exec_counts.get(node.node_id, 0)

        variable_specs = [
            VariableRuntimeSpec(
                name=var.name,
                data_type=var.data_type,
                persist_between_cycles=var.persist_between_cycles,
            )
            for var in self._variable_list_sorted()
        ]
        return GraphRuntimeSnapshot(
            nodes=list(nodes.values()),
            exec_edges=exec_edges,
            data_edges=data_edges,
            variables=variable_specs,
        )

    def _collect_node_params(
        self,
        node: NodeBlockItem,
        template_key: str,
        data_inputs: list[str],
    ) -> dict[str, object]:
        params: dict[str, object] = {}
        widget = node.params_widget()

        if template_key == "start_number":
            if not isinstance(widget, NumberStartParamsWidget):
                raise RuntimeError("Узел 'Старт (число)' имеет некорректный виджет параметров.")
            params["start"] = int(widget.start_spin.value())
            params["step"] = int(widget.step_spin.value())
            params["end"] = int(widget.end_spin.value())
            return params

        if template_key == "start_string":
            if not isinstance(widget, StringStartParamsWidget):
                raise RuntimeError("Узел 'Старт (строка)' имеет некорректный виджет параметров.")
            params["path"] = (widget.path_edit.text() or "").strip()
            return params

        if template_key in ("variable_read", "variable_write"):
            if not isinstance(node, VariableNodeBlockItem):
                raise RuntimeError("Переменный узел имеет некорректный тип.")
            params["variable_name"] = (node.selected_variable_name() or "").strip()
            return params

        if template_key == "string_template":
            if not isinstance(widget, StringTemplateParamsWidget):
                raise RuntimeError("Узел 'Шаблонизатор строки' имеет некорректный виджет параметров.")
            params["template"] = widget.template_text()
            params["placeholders"] = list(data_inputs)
            return params

        if template_key == "stitch_split":
            if not isinstance(widget, StitchSplitParamsWidget):
                raise RuntimeError("Узел 'Склейка/резка' имеет некорректный виджет параметров.")
            txt_k = (widget.k_edit.text() or "").strip()
            if txt_k:
                try:
                    params["K"] = int(txt_k)
                    if int(params["K"]) <= 0:
                        raise ValueError()
                except Exception:
                    raise RuntimeError("Узел 'Склейка/резка': K должно быть положительным целым или пустым.")
            else:
                params["K"] = None

            try:
                params["Hmax"] = int((widget.hmax_edit.text() or "").strip())
                params["band_rows"] = int((widget.band_edit.text() or "").strip())
                params["tol"] = int((widget.tol_edit.text() or "").strip())
                params["search_radius"] = int((widget.radius_edit.text() or "").strip())
            except Exception:
                raise RuntimeError(
                    "Узел 'Склейка/резка': Hmax/band_rows/tol/search_radius должны быть целыми числами."
                )

            for key in ("Hmax", "band_rows", "tol", "search_radius"):
                if int(params[key]) <= 0:
                    raise RuntimeError(
                        "Узел 'Склейка/резка': Hmax/band_rows/tol/search_radius должны быть > 0."
                    )
            params["prefer_up_first"] = bool(widget.prefer_up_checkbox.isChecked())
            return params

        if template_key == "open_url":
            if not isinstance(widget, OpenUrlParamsWidget):
                raise RuntimeError("Узел 'Открыть URL' имеет некорректный виджет параметров.")
            params["browser"] = widget.selected_browser()
            return params

        if template_key == "fetch_from_browser":
            if not isinstance(widget, FetchFromBrowserParamsWidget):
                raise RuntimeError("Узел 'Выкачать из браузера' имеет некорректный виджет параметров.")
            params["pattern"] = widget.selected_pattern()
            return params

        if template_key == "waifu2x":
            if not isinstance(widget, Waifu2xParamsWidget):
                raise RuntimeError("Узел 'waifu2x' имеет некорректный виджет параметров.")
            try:
                params["noise"] = int(widget.noise())
                params["scale"] = int(widget.scale())
                params["tile"] = int(widget.tile())
            except Exception:
                raise RuntimeError("Узел 'waifu2x': параметры -n/-s/-t должны быть целыми.")
            if int(params["noise"]) not in (-1, 0, 1, 2, 3):
                raise RuntimeError("Узел 'waifu2x': допустимые значения -n: -1, 0, 1, 2, 3.")
            if int(params["scale"]) not in (1, 2, 4, 8, 16, 32):
                raise RuntimeError("Узел 'waifu2x': допустимые значения -s: 1, 2, 4, 8, 16, 32.")
            if int(params["tile"]) != 0 and int(params["tile"]) < 32:
                raise RuntimeError("Узел 'waifu2x': -t должно быть 0 или >= 32.")
            params["exec_path"] = widget.exec_path()
            return params

        return params

    def _on_run_progress(self, message: str) -> None:
        self.statusBar().showMessage(message)

    def _on_run_finished(self, message: str) -> None:
        self.statusBar().showMessage(message, 6000)
        QtWidgets.QMessageBox.information(self, "Запуск", message)

    def _on_run_failed(self, message: str) -> None:
        self.statusBar().showMessage(message, 6000)
        QtWidgets.QMessageBox.critical(self, "Ошибка запуска", message)

    def _on_run_worker_thread_finished(self) -> None:
        self._run_worker = None
        self._run_thread = None
        self._run_action.setEnabled(True)
        self._stop_action.setEnabled(False)
        self._save_pipeline_action.setEnabled(True)
        self._load_pipeline_action.setEnabled(True)
