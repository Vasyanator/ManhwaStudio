from __future__ import annotations

from typing import Optional

from PyQt6.QtWidgets import (
    QTabWidget,
    QWidget,
    QVBoxLayout,
    QHBoxLayout,
    QGroupBox,
    QLabel,
    QCheckBox,
    QComboBox,
    QPushButton,
    QPlainTextEdit,
    QMessageBox,
)

from modules.ai_device import AIDevice


class SettingsTab(QWidget):
    CORE_TABS = ("Перевод", "Клининг", "Текст")
    NOTES_TAB = "Заметки перевода"
    NOTES_DEPS = ("Персонажи", "Термины")

    def __init__(
        self,
        project=None,
        parent: Optional[QWidget] = None,
        ai_device: Optional[AIDevice] = None,
        user_config=None,
        managed_tabs: Optional[list[str]] = None,
    ):
        super().__init__(parent)
        self.project = project
        self.user_config = user_config
        self.ai_device = ai_device
        self.managed_tabs = managed_tabs or [
            "Перевод",
            "Клининг",
            "Текст",
            "Персонажи",
            "Термины",
            "Заметки перевода",
            "Вики",
        ]
        self._tabs_checkboxes: dict[str, QCheckBox] = {}
        self._tabs_ui_sync = False

        root = QVBoxLayout(self)
        self.pages = QTabWidget()
        root.addWidget(self.pages, 1)

        self._build_general_page()
        self._build_ai_page()
        self._load_tabs_from_config()

    def _build_general_page(self) -> None:
        page = QWidget()
        layout = QVBoxLayout(page)

        tabs_group = QGroupBox("Вкладки")
        tabs_layout = QVBoxLayout(tabs_group)
        for name in self.managed_tabs:
            cb = QCheckBox(name)
            cb.toggled.connect(self._on_tabs_checkbox_toggled)
            tabs_layout.addWidget(cb)
            self._tabs_checkboxes[name] = cb

        layout.addWidget(tabs_group)
        self.general_status_label = QLabel("")
        layout.addWidget(self.general_status_label)
        layout.addStretch(1)
        self.pages.addTab(page, "Общее")

    def _build_ai_page(self) -> None:
        page = QWidget()
        root = QVBoxLayout(page)

        title = QLabel("Настройки ИИ устройства")
        root.addWidget(title)

        device_row = QHBoxLayout()
        device_row.addWidget(QLabel("Устройство:"))
        self.device_combo = QComboBox()
        device_row.addWidget(self.device_combo, 1)
        self.btn_refresh = QPushButton("Обновить список")
        device_row.addWidget(self.btn_refresh)
        root.addLayout(device_row)

        actions = QHBoxLayout()
        self.btn_apply = QPushButton("Сохранить (после перезапуска)")
        self.btn_diagnose = QPushButton("Диагностика CUDA/ROCm")
        actions.addWidget(self.btn_apply)
        actions.addWidget(self.btn_diagnose)
        root.addLayout(actions)

        self.status_label = QLabel("")
        root.addWidget(self.status_label)

        self.output = QPlainTextEdit()
        self.output.setReadOnly(True)
        root.addWidget(self.output, 1)
        self.pages.addTab(page, "ИИ")

        self.btn_refresh.clicked.connect(self._reload_devices)
        self.btn_apply.clicked.connect(self._on_apply_device)
        self.btn_diagnose.clicked.connect(self._on_diagnose)

        self._reload_devices()

    def _read_tabs_map(self) -> dict[str, bool]:
        data = getattr(self.user_config, "config", None)
        defaults = {name: True for name in self.managed_tabs}
        if not isinstance(data, dict):
            return defaults

        general = data.get("General")
        if not isinstance(general, dict):
            return defaults

        enabled = general.get("enabled_tabs")
        if not isinstance(enabled, dict):
            return defaults

        merged = {}
        for name in self.managed_tabs:
            merged[name] = bool(enabled.get(name, True))
        return merged

    def _normalize_tabs_map(self, mapping: dict[str, bool]) -> dict[str, bool]:
        out = {name: bool(mapping.get(name, True)) for name in self.managed_tabs}
        if out.get(self.NOTES_TAB, False):
            for dep in self.NOTES_DEPS:
                if dep in out:
                    out[dep] = True

        if not any(out.get(tab, False) for tab in self.CORE_TABS if tab in out):
            if self.CORE_TABS[0] in out:
                out[self.CORE_TABS[0]] = True

        return out

    def _save_tabs_map(self, mapping: dict[str, bool]) -> None:
        data = getattr(self.user_config, "config", None)
        if not isinstance(data, dict):
            return
        general = data.get("General")
        if not isinstance(general, dict):
            general = {}
            data["General"] = general
        general["enabled_tabs"] = {name: bool(mapping.get(name, True)) for name in self.managed_tabs}
        save = getattr(self.user_config, "save", None)
        if callable(save):
            save()

    def _apply_tabs_ui(self, mapping: dict[str, bool]) -> None:
        self._tabs_ui_sync = True
        try:
            for name, cb in self._tabs_checkboxes.items():
                cb.setChecked(bool(mapping.get(name, True)))
                if name in self.NOTES_DEPS:
                    cb.setEnabled(not mapping.get(self.NOTES_TAB, False))
        finally:
            self._tabs_ui_sync = False

    def _load_tabs_from_config(self) -> None:
        mapping = self._normalize_tabs_map(self._read_tabs_map())
        self._apply_tabs_ui(mapping)
        self._save_tabs_map(mapping)
        self.general_status_label.setText("Изменения вкладок применяются после перезапуска программы.")

    def _collect_tabs_from_ui(self) -> dict[str, bool]:
        return {name: cb.isChecked() for name, cb in self._tabs_checkboxes.items()}

    def _on_tabs_checkbox_toggled(self, _checked: bool) -> None:
        if self._tabs_ui_sync:
            return

        attempted = self._collect_tabs_from_ui()
        normalized = self._normalize_tabs_map(attempted)
        if normalized != attempted:
            msg_parts = []
            if attempted.get(self.NOTES_TAB, False):
                for dep in self.NOTES_DEPS:
                    if dep in attempted and not attempted.get(dep, False):
                        msg_parts.append("Для «Заметки перевода» автоматически включены «Персонажи» и «Термины».")
                        break
            if not any(attempted.get(tab, False) for tab in self.CORE_TABS if tab in attempted):
                msg_parts.append("Должна быть включена хотя бы одна вкладка: Перевод, Клининг или Текст.")
            if msg_parts:
                QMessageBox.information(self, "Ограничения вкладок", "\n".join(msg_parts))

        self._apply_tabs_ui(normalized)
        self._save_tabs_map(normalized)
        self.general_status_label.setText("Сохранено. Изменения вступят в силу после перезапуска.")

    def _configured_device(self) -> str:
        if self.ai_device is not None:
            return str(self.ai_device)
        return "cpu"

    def _reload_devices(self) -> None:
        devices = AIDevice.detect_available_devices()
        current = self._configured_device()

        self.device_combo.blockSignals(True)
        self.device_combo.clear()
        for dev in devices:
            self.device_combo.addItem(dev)

        idx = self.device_combo.findText(current)
        if idx >= 0:
            self.device_combo.setCurrentIndex(idx)
        elif self.device_combo.count() > 0:
            self.device_combo.setCurrentIndex(0)
        self.device_combo.blockSignals(False)

        self.status_label.setText(f"Текущее значение в конфиге: {current}")

    def _on_apply_device(self) -> None:
        if self.ai_device is None:
            QMessageBox.warning(self, "Ошибка", "AIDevice не инициализирован.")
            return

        selected = self.device_combo.currentText().strip()
        if not selected:
            QMessageBox.warning(self, "Ошибка", "Выберите устройство.")
            return

        try:
            saved = self.ai_device.change_device(selected)
        except Exception as exc:
            QMessageBox.critical(self, "Ошибка", str(exc))
            return

        self.status_label.setText(f"Текущее значение в конфиге: {saved}")
        self.output.appendPlainText(
            f"Устройство сохранено: {saved}\nИзменение вступит в силу после перезапуска программы.\n"
        )
        QMessageBox.information(
            self,
            "Сохранено",
            f"Устройство '{saved}' сохранено. Перезапустите программу для применения.",
        )

    def _on_diagnose(self) -> None:
        report = AIDevice.diagnose_cuda_rocm()
        self.output.setPlainText(report)
