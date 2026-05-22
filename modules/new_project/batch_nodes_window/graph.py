from __future__ import annotations

"""
FILE OVERVIEW: modules/new_project/batch_nodes_window/graph.py
Сцена и view для интерактивного node-редактора (drag соединений, пан/зум, delete).

Main items:
- `NodeGraphScene`: создание/валидация/удаление соединений и узлов.
- `NodeGraphView`: обработка мыши/клавиатуры, рисование сетки, панорамирование и zoom.
  Удаление узлов/связей работает только по `Delete`; текстовые поля не перехватываются.
"""

from typing import Optional

from PyQt6 import QtCore, QtGui, QtWidgets

from .constants import KIND_DATA
from .graphics_items import NodeBlockItem, NodeConnectionItem, NodeSocketItem


class NodeGraphScene(QtWidgets.QGraphicsScene):
    def __init__(self, parent: Optional[QtCore.QObject] = None):
        super().__init__(parent)
        self._drag_connection: Optional[NodeConnectionItem] = None

    def start_drag_connection(self, source: NodeSocketItem, cursor_scene_pos: QtCore.QPointF) -> None:
        self.cancel_drag_connection()
        conn = NodeConnectionItem(source)
        conn.set_temp_target(cursor_scene_pos)
        self.addItem(conn)
        self._drag_connection = conn

    def update_drag_connection(self, cursor_scene_pos: QtCore.QPointF) -> None:
        if self._drag_connection is not None:
            self._drag_connection.set_temp_target(cursor_scene_pos)

    def finish_drag_connection(self, target: Optional[NodeSocketItem]) -> None:
        conn = self._drag_connection
        self._drag_connection = None
        if conn is None:
            return

        start = conn.start_socket
        if target is None or not self._is_valid_pair(start, target):
            self._remove_connection(conn)
            return

        out_socket, in_socket = self._normalize_pair(start, target)
        if not in_socket.spec.allow_multiple and self._has_input_connection(in_socket):
            self._remove_connection(conn)
            return
        if self._is_duplicate(out_socket, in_socket):
            self._remove_connection(conn)
            return

        conn.attach(out_socket, in_socket)

    def cancel_drag_connection(self) -> None:
        if self._drag_connection is not None:
            self._remove_connection(self._drag_connection)
            self._drag_connection = None

    def is_dragging_connection(self) -> bool:
        return self._drag_connection is not None

    def delete_selected_items(self) -> None:
        selected = list(self.selectedItems())
        for item in selected:
            if isinstance(item, NodeConnectionItem):
                self._remove_connection(item)
        for item in selected:
            if isinstance(item, NodeBlockItem):
                self._remove_node(item)

    def _remove_node(self, node: NodeBlockItem) -> None:
        for socket in node.sockets():
            for conn in list(socket.connections):
                self._remove_connection(conn)
        self.removeItem(node)

    def _remove_connection(self, conn: NodeConnectionItem) -> None:
        conn.detach()
        self.removeItem(conn)

    def _normalize_pair(
        self,
        socket_a: NodeSocketItem,
        socket_b: NodeSocketItem,
    ) -> tuple[NodeSocketItem, NodeSocketItem]:
        if socket_a.spec.direction == "out":
            return socket_a, socket_b
        return socket_b, socket_a

    def _is_valid_pair(self, socket_a: NodeSocketItem, socket_b: NodeSocketItem) -> bool:
        if socket_a is socket_b:
            return False
        if socket_a.spec.direction == socket_b.spec.direction:
            return False
        if socket_a.parentItem() is socket_b.parentItem():
            return False

        out_socket, in_socket = self._normalize_pair(socket_a, socket_b)
        if out_socket.spec.direction != "out" or in_socket.spec.direction != "in":
            return False
        if out_socket.spec.kind != in_socket.spec.kind:
            return False
        if out_socket.spec.kind == KIND_DATA and not self._is_data_type_compatible(out_socket, in_socket):
            return False
        return True

    def _is_data_type_compatible(self, out_socket: NodeSocketItem, in_socket: NodeSocketItem) -> bool:
        out_spec = out_socket.spec
        in_spec = in_socket.spec

        out_types = set(out_spec.accepted_data_types)
        if out_spec.data_type:
            out_types.add(out_spec.data_type)

        in_types = set(in_spec.accepted_data_types)
        if in_spec.data_type:
            in_types.add(in_spec.data_type)

        if not out_types or not in_types:
            return False
        return bool(out_types.intersection(in_types))

    def _has_input_connection(self, in_socket: NodeSocketItem) -> bool:
        for conn in in_socket.connections:
            if conn.in_socket is in_socket:
                return True
        return False

    def _is_duplicate(self, out_socket: NodeSocketItem, in_socket: NodeSocketItem) -> bool:
        for conn in out_socket.connections:
            if conn.out_socket is out_socket and conn.in_socket is in_socket:
                return True
        return False


class NodeGraphView(QtWidgets.QGraphicsView):
    def __init__(self, scene: NodeGraphScene, parent: Optional[QtWidgets.QWidget] = None):
        super().__init__(scene, parent)
        self._scene = scene
        self._is_panning = False
        self._pan_start = QtCore.QPoint()

        self.setRenderHint(QtGui.QPainter.RenderHint.Antialiasing, True)
        self.setRenderHint(QtGui.QPainter.RenderHint.TextAntialiasing, True)
        self.setViewportUpdateMode(QtWidgets.QGraphicsView.ViewportUpdateMode.FullViewportUpdate)
        self.setBackgroundBrush(QtGui.QBrush(QtGui.QColor("#0b1020")))
        self.setTransformationAnchor(QtWidgets.QGraphicsView.ViewportAnchor.AnchorUnderMouse)
        self.setResizeAnchor(QtWidgets.QGraphicsView.ViewportAnchor.AnchorViewCenter)
        self.setDragMode(QtWidgets.QGraphicsView.DragMode.NoDrag)
        self.setFocusPolicy(QtCore.Qt.FocusPolicy.StrongFocus)

    def drawBackground(self, painter: QtGui.QPainter, rect: QtCore.QRectF) -> None:
        super().drawBackground(painter, rect)
        painter.save()
        grid = 32
        left = int(rect.left()) - (int(rect.left()) % grid)
        top = int(rect.top()) - (int(rect.top()) % grid)
        lines = []
        x = left
        while x < int(rect.right()):
            lines.append(QtCore.QLineF(x, rect.top(), x, rect.bottom()))
            x += grid
        y = top
        while y < int(rect.bottom()):
            lines.append(QtCore.QLineF(rect.left(), y, rect.right(), y))
            y += grid
        painter.setPen(QtGui.QPen(QtGui.QColor("#111827"), 1))
        painter.drawLines(lines)
        painter.restore()

    def mousePressEvent(self, event: QtGui.QMouseEvent) -> None:
        if event.button() == QtCore.Qt.MouseButton.MiddleButton:
            self._is_panning = True
            self._pan_start = event.pos()
            self.setCursor(QtCore.Qt.CursorShape.ClosedHandCursor)
            event.accept()
            return

        if event.button() == QtCore.Qt.MouseButton.LeftButton:
            socket = self._socket_at(event.pos())
            if socket is not None:
                self._scene.start_drag_connection(socket, self.mapToScene(event.pos()))
                event.accept()
                return

        super().mousePressEvent(event)

    def mouseMoveEvent(self, event: QtGui.QMouseEvent) -> None:
        if self._is_panning:
            delta = event.pos() - self._pan_start
            self._pan_start = event.pos()
            self.horizontalScrollBar().setValue(self.horizontalScrollBar().value() - delta.x())
            self.verticalScrollBar().setValue(self.verticalScrollBar().value() - delta.y())
            event.accept()
            return

        if self._scene.is_dragging_connection():
            self._scene.update_drag_connection(self.mapToScene(event.pos()))
            event.accept()
            return

        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event: QtGui.QMouseEvent) -> None:
        if event.button() == QtCore.Qt.MouseButton.MiddleButton and self._is_panning:
            self._is_panning = False
            self.unsetCursor()
            event.accept()
            return

        if event.button() == QtCore.Qt.MouseButton.LeftButton and self._scene.is_dragging_connection():
            self._scene.finish_drag_connection(self._socket_at(event.pos()))
            event.accept()
            return

        super().mouseReleaseEvent(event)

    def keyPressEvent(self, event: QtGui.QKeyEvent) -> None:
        if event.key() == QtCore.Qt.Key.Key_Delete:
            if self._is_line_edit_focused():
                super().keyPressEvent(event)
                return
            self._scene.delete_selected_items()
            event.accept()
            return
        super().keyPressEvent(event)

    def _is_line_edit_focused(self) -> bool:
        focused = QtWidgets.QApplication.focusWidget()
        if focused is None:
            return False
        if focused.window() is not self.window():
            return False
        return isinstance(focused, QtWidgets.QLineEdit)

    def wheelEvent(self, event: QtGui.QWheelEvent) -> None:
        zoom_in = event.angleDelta().y() > 0
        current_zoom = self.transform().m11()
        if zoom_in and current_zoom > 2.6:
            return
        if not zoom_in and current_zoom < 0.32:
            return
        factor = 1.15 if zoom_in else 1.0 / 1.15
        self.scale(factor, factor)

    def _socket_at(self, view_pos: QtCore.QPoint) -> Optional[NodeSocketItem]:
        for item in self.items(view_pos):
            if isinstance(item, NodeSocketItem):
                return item
        return None
