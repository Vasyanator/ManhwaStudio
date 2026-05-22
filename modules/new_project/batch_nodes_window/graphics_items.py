from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/graphics_items.py
Графические элементы node-editor: сокеты, соединения и блоки узлов.

Main items:
- `NodeSocketItem`: круглый порт узла (exec/data) с хранением соединений.
- `NodeConnectionItem`: кривая Безье между сокетами (временная и финальная).
- `NodeBlockItem`: базовый блок узла с заголовком/описанием/сокетами/параметрами.
  Поддерживает динамическую перестройку списка сокетов через `rebuild_sockets`.
  Держит метаданные шаблона (`template_key`) и доступ к виджету параметров.
- `VariableNodeBlockItem`: специализированный блок read/write переменной.
"""

from typing import Callable, Optional

from PyQt6 import QtCore, QtGui, QtWidgets

from .constants import DATA_TYPE_LABELS, KIND_DATA, KIND_EXEC, TYPE_STR, socket_color
from .models import SocketSpec, VariableDefinition
from .widgets import VariableSelectorWidget


class NodeSocketItem(QtWidgets.QGraphicsEllipseItem):
    RADIUS = 7.0

    def __init__(self, parent: QtWidgets.QGraphicsItem, spec: SocketSpec):
        super().__init__(-self.RADIUS, -self.RADIUS, self.RADIUS * 2.0, self.RADIUS * 2.0, parent)
        self.spec = spec
        self.connections: set[NodeConnectionItem] = set()
        self.setZValue(30)
        self.setPen(QtGui.QPen(QtGui.QColor("#111827"), 1.4))
        self.refresh_visual()
        self.setToolTip(self._tooltip_text())

    def _tooltip_text(self) -> str:
        side = "Вход" if self.spec.direction == "in" else "Выход"
        if self.spec.kind == KIND_EXEC:
            return f"{side} выполнения: {self.spec.name}"
        if self.spec.accepted_data_types:
            labels = [DATA_TYPE_LABELS.get(tp, tp) for tp in self.spec.accepted_data_types]
            tp = " / ".join(labels)
        else:
            tp = DATA_TYPE_LABELS.get(self.spec.data_type or "", self.spec.data_type or "data")
        return f"{side} данных ({tp}): {self.spec.name}"

    def refresh_visual(self) -> None:
        self.setBrush(QtGui.QBrush(socket_color(self.spec.kind, self.spec.data_type)))
        self.setToolTip(self._tooltip_text())

    def scene_anchor(self) -> QtCore.QPointF:
        return self.mapToScene(QtCore.QPointF(0.0, 0.0))

    def refresh_connections(self) -> None:
        for conn in tuple(self.connections):
            conn.refresh_pen()
            conn.refresh_path()


class NodeConnectionItem(QtWidgets.QGraphicsPathItem):
    def __init__(self, start_socket: NodeSocketItem):
        super().__init__()
        self.start_socket = start_socket
        self.out_socket: Optional[NodeSocketItem] = None
        self.in_socket: Optional[NodeSocketItem] = None
        self._temp_target: Optional[QtCore.QPointF] = None
        self.setZValue(8)
        self.start_socket.connections.add(self)
        self.refresh_pen()
        self.refresh_path()

    @property
    def kind(self) -> str:
        if self.out_socket is not None and self.in_socket is not None:
            return self.out_socket.spec.kind
        return self.start_socket.spec.kind

    @property
    def data_type(self) -> Optional[str]:
        if self.out_socket is not None and self.in_socket is not None:
            return self.out_socket.spec.data_type
        return self.start_socket.spec.data_type

    def refresh_pen(self) -> None:
        color = socket_color(self.kind, self.data_type)
        pen = QtGui.QPen(color, 2.2)
        if self.kind == KIND_EXEC:
            pen.setStyle(QtCore.Qt.PenStyle.DashLine)
        self.setPen(pen)

    def set_temp_target(self, pos: QtCore.QPointF) -> None:
        self._temp_target = pos
        self.refresh_path()

    def attach(self, out_socket: NodeSocketItem, in_socket: NodeSocketItem) -> None:
        self.out_socket = out_socket
        self.in_socket = in_socket
        self._temp_target = None
        self.out_socket.connections.add(self)
        self.in_socket.connections.add(self)
        self.refresh_pen()
        self.refresh_path()

    def detach(self) -> None:
        self.start_socket.connections.discard(self)
        if self.out_socket is not None:
            self.out_socket.connections.discard(self)
        if self.in_socket is not None:
            self.in_socket.connections.discard(self)
        self.out_socket = None
        self.in_socket = None
        self._temp_target = None

    def refresh_path(self) -> None:
        if self.out_socket is not None and self.in_socket is not None:
            start = self.out_socket.scene_anchor()
            end = self.in_socket.scene_anchor()
        else:
            start = self.start_socket.scene_anchor()
            end = self._temp_target if self._temp_target is not None else start

        dx = end.x() - start.x()
        ctrl = max(70.0, abs(dx) * 0.5)
        c1 = QtCore.QPointF(start.x() + ctrl, start.y())
        c2 = QtCore.QPointF(end.x() - ctrl, end.y())

        path = QtGui.QPainterPath(start)
        path.cubicTo(c1, c2, end)
        self.setPath(path)


class NodeBlockItem(QtWidgets.QGraphicsRectItem):
    ROW_H = 28.0

    def __init__(
        self,
        title: str,
        sockets: list[SocketSpec],
        *,
        template_key: str = "",
        params_widget: Optional[QtWidgets.QWidget] = None,
        description: str = "",
        width: float = 290.0,
    ):
        super().__init__()
        self._template_key = template_key
        self._width = width
        self._sockets_top = 0.0
        self._sockets: list[NodeSocketItem] = []
        self._socket_labels: dict[NodeSocketItem, QtWidgets.QGraphicsTextItem] = {}

        self.setFlags(
            QtWidgets.QGraphicsItem.GraphicsItemFlag.ItemIsMovable
            | QtWidgets.QGraphicsItem.GraphicsItemFlag.ItemIsSelectable
            | QtWidgets.QGraphicsItem.GraphicsItemFlag.ItemSendsGeometryChanges
        )
        self.setPen(QtGui.QPen(QtGui.QColor("#475569"), 1.4))
        self.setBrush(QtGui.QBrush(QtGui.QColor("#0f172a")))
        self.setZValue(14)

        self._title_item = QtWidgets.QGraphicsTextItem(title, self)
        self._title_item.setDefaultTextColor(QtGui.QColor("#f8fafc"))
        self._title_item.setPos(12.0, 6.0)
        title_font = self._title_item.font()
        title_font.setBold(True)
        self._title_item.setFont(title_font)

        header_h = 32.0
        if description:
            self._desc_item = QtWidgets.QGraphicsTextItem(description, self)
            self._desc_item.setDefaultTextColor(QtGui.QColor("#94a3b8"))
            self._desc_item.setPos(12.0, 25.0)
            self._desc_item.setTextWidth(width - 24.0)
            header_h = 54.0
        else:
            self._desc_item = None

        params_h = 0.0
        if params_widget is not None:
            self._params_proxy = QtWidgets.QGraphicsProxyWidget(self)
            self._params_proxy.setWidget(params_widget)
            params_widget.adjustSize()
            params_h = float(params_widget.sizeHint().height()) + 6.0
            self._params_proxy.setPos(10.0, header_h)
        else:
            self._params_proxy = None

        self._sockets_top = header_h + params_h + 12.0
        self.rebuild_sockets(sockets)

    def _create_socket_with_label(
        self,
        spec: SocketSpec,
        index: int,
        rows: int,
        sockets_top: float,
        *,
        is_input: bool,
    ) -> None:
        y = sockets_top + index * self.ROW_H + self.ROW_H * 0.5
        x = 0.0 if is_input else self._width

        socket = NodeSocketItem(self, spec)
        socket.setPos(x, y)
        self._sockets.append(socket)

        text = QtWidgets.QGraphicsTextItem(self._socket_label_text(socket), self)
        text.setDefaultTextColor(QtGui.QColor("#cbd5e1"))
        font = text.font()
        font.setPointSize(max(8, font.pointSize() - 1))
        text.setFont(font)
        self._socket_labels[socket] = text
        self._reposition_socket_label(socket)

    def _socket_label_text(self, socket: NodeSocketItem) -> str:
        spec = socket.spec
        if spec.kind == KIND_EXEC:
            return f"[exec] {spec.name}"
        if spec.accepted_data_types:
            type_label = " / ".join(DATA_TYPE_LABELS.get(tp, tp) for tp in spec.accepted_data_types)
        else:
            type_label = DATA_TYPE_LABELS.get(spec.data_type or "", spec.data_type or "data")
        return f"{spec.name} ({type_label})"

    def _reposition_socket_label(self, socket: NodeSocketItem) -> None:
        text = self._socket_labels[socket]
        y = socket.y() - 10.0
        if socket.spec.direction == "in":
            text.setPos(14.0, y)
        else:
            text_w = text.boundingRect().width()
            text.setPos(self._width - 14.0 - text_w, y)

    def set_title(self, title: str) -> None:
        self._title_item.setPlainText(title)

    def title(self) -> str:
        return self._title_item.toPlainText()

    def set_template_key(self, template_key: str) -> None:
        self._template_key = template_key

    def template_key(self) -> str:
        return self._template_key

    def params_widget(self) -> Optional[QtWidgets.QWidget]:
        if self._params_proxy is None:
            return None
        return self._params_proxy.widget()

    def rebuild_sockets(self, sockets: list[SocketSpec]) -> None:
        self._clear_sockets()

        inputs = [spec for spec in sockets if spec.direction == "in"]
        outputs = [spec for spec in sockets if spec.direction == "out"]
        rows = max(1, max(len(inputs), len(outputs)))
        height = self._sockets_top + rows * self.ROW_H + 12.0
        self.setRect(0.0, 0.0, self._width, height)

        for index, spec in enumerate(inputs):
            self._create_socket_with_label(spec, index, rows, self._sockets_top, is_input=True)
        for index, spec in enumerate(outputs):
            self._create_socket_with_label(spec, index, rows, self._sockets_top, is_input=False)

    def _clear_sockets(self) -> None:
        scene = self.scene()
        removed_connections: set[NodeConnectionItem] = set()
        for socket in self._sockets:
            for conn in list(socket.connections):
                if conn in removed_connections:
                    continue
                conn.detach()
                if scene is not None:
                    scene.removeItem(conn)
                removed_connections.add(conn)

        for socket, text in list(self._socket_labels.items()):
            if scene is not None and text.scene() is scene:
                scene.removeItem(text)
            text.setParentItem(None)

            if scene is not None and socket.scene() is scene:
                scene.removeItem(socket)
            socket.setParentItem(None)

        self._sockets.clear()
        self._socket_labels.clear()

    def sockets(self) -> list[NodeSocketItem]:
        return list(self._sockets)

    def socket_by_name(self, name: str) -> Optional[NodeSocketItem]:
        for socket in self._sockets:
            if socket.spec.name == name:
                return socket
        return None

    def set_socket_data_type(self, socket: NodeSocketItem, data_type: str) -> None:
        if socket.spec.kind != KIND_DATA:
            return
        if socket.spec.data_type == data_type:
            return
        socket.spec.data_type = data_type
        socket.refresh_visual()
        text = self._socket_labels[socket]
        text.setPlainText(self._socket_label_text(socket))
        self._reposition_socket_label(socket)
        socket.refresh_connections()

    def itemChange(self, change: QtWidgets.QGraphicsItem.GraphicsItemChange, value):
        if change == QtWidgets.QGraphicsItem.GraphicsItemChange.ItemPositionHasChanged:
            for socket in self._sockets:
                socket.refresh_connections()
        return super().itemChange(change, value)


class VariableNodeBlockItem(NodeBlockItem):
    def __init__(
        self,
        *,
        mode: str,  # "read" | "write"
        variable_resolver: Callable[[str], Optional[VariableDefinition]],
        initial_variables: list[VariableDefinition],
        selected_variable: Optional[str] = None,
    ):
        self._mode = mode
        self._variable_resolver = variable_resolver
        self._selector = VariableSelectorWidget("Переменная:", None)
        self._value_socket_name = "Значение"

        initial_var = None
        if selected_variable:
            initial_var = next((v for v in initial_variables if v.name == selected_variable), None)
        if initial_var is None and initial_variables:
            initial_var = initial_variables[0]

        initial_type = initial_var.data_type if initial_var is not None else TYPE_STR
        sockets = (
            [
                SocketSpec("Вход", "in", KIND_EXEC, allow_multiple=True),
                SocketSpec(self._value_socket_name, "in", KIND_DATA, data_type=initial_type),
                SocketSpec("Далее", "out", KIND_EXEC),
            ]
            if mode == "write"
            else [SocketSpec(self._value_socket_name, "out", KIND_DATA, data_type=initial_type)]
        )
        base_title = "Переменная (запись)" if mode == "write" else "Переменная (чтение)"
        super().__init__(
            base_title,
            sockets,
            params_widget=self._selector,
            description="Сохраняет/читает значение переменной",
            width=300.0,
        )

        self._value_socket = self.socket_by_name(self._value_socket_name)
        self._selector.variable_changed.connect(self._on_variable_changed)
        self.set_variable_options(initial_variables, selected_variable)

    def set_variable_options(
        self,
        variables: list[VariableDefinition],
        selected_name: Optional[str] = None,
    ) -> None:
        self._selector.set_variables(variables, selected_name)

    def selected_variable_name(self) -> Optional[str]:
        return self._selector.current_variable_name()

    def _on_variable_changed(self, variable_name: str) -> None:
        name = variable_name.strip()
        var = self._variable_resolver(name) if name else None
        if var is None:
            self._selector.set_persist_flag(None)
            self.set_title("Переменная (запись)" if self._mode == "write" else "Переменная (чтение)")
            return

        self._selector.set_persist_flag(var.persist_between_cycles)
        self.set_title(
            f"{'Переменная (запись)' if self._mode == 'write' else 'Переменная (чтение)'}: {var.name}"
        )
        if self._value_socket is not None:
            self.set_socket_data_type(self._value_socket, var.data_type)
