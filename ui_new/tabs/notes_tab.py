import json
from pathlib import Path
from typing import Optional, List, Tuple

from PyQt6.QtCore import Qt, QFileSystemWatcher, QTimer
from PyQt6.QtGui import QGuiApplication
from PyQt6.QtWidgets import (
    QWidget, QVBoxLayout, QHBoxLayout, QLabel, QTextEdit,
    QPushButton, QCheckBox, QMessageBox, QTabWidget, QFrame
)


class TranslationNotesTabQt(QWidget):
    """
    Вкладка «Заметки перевода» для PyQt6.

    Теперь содержит ДВЕ под-вкладки:
      1) «Собранный промпт» — конечный текст c подстановкой {charas}/{terms},
         возможностью отключить разделы и быстрым копированием.
      2) «Шаблон (notes_file)» — редактор исходного файла. Если в шаблоне
         нет плейсхолдеров, показывает подсказку с кнопками «Вставить {charas}»
         и «Вставить {terms}».

    Шаблон подсказки хранится в project.notes_file.
    Персонажи берутся из <project.char_dir>/characters.json.
    Термины — из project.terms_file (JSON-массив).

    Публичные методы:
      - refresh_from_project(): пересобрать текст и синхронизировать редактор.
    """

    def __init__(self, project, charas_tab=None, terms_tab=None, parent: Optional[QWidget] = None):
        super().__init__(parent)
        self.project = project
        self.charas_tab = charas_tab
        self.terms_tab = terms_tab

        # Пути
        self.template_path = Path(getattr(self.project, "notes_file", "") or "")
        self.char_json_path = Path(getattr(self.project, "char_dir", "") or "") / "characters.json"
        self.terms_json_path = Path(getattr(self.project, "terms_file", "") or "")

        # --- Корневой UI ---
        root = QVBoxLayout(self)
        path_lbl = QLabel(self._ui_path_text())
        path_lbl.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        root.addWidget(path_lbl)

        # TabWidget
        self.tabs = QTabWidget()
        root.addWidget(self.tabs, 1)

        # ---------------- Tab 1: Собранный промпт ----------------
        wrap1 = QWidget()
        tab1 = QVBoxLayout(wrap1)

        controls = QHBoxLayout()
        self.cb_charas = QCheckBox("Вставлять персонажей")
        self.cb_terms = QCheckBox("Вставлять термины")
        self.cb_charas.setChecked(True)
        self.cb_terms.setChecked(True)
        controls.addWidget(self.cb_charas)
        controls.addWidget(self.cb_terms)
        controls.addStretch(1)
        self.btn_refresh = QPushButton("Обновить")
        self.btn_copy = QPushButton("Скопировать")
        controls.addWidget(self.btn_refresh)
        controls.addWidget(self.btn_copy)
        tab1.addLayout(controls)

        hint = QLabel(
            "Шаблон берётся из файла и может содержать {charas} и {terms}.\n"
            "Если плейсхолдер отсутствует, соответствующий раздел будет добавлен в конец."
        )
        hint.setStyleSheet("color: #666;")
        tab1.addWidget(hint)

        self.preview = QTextEdit()
        self.preview.setReadOnly(True)
        self.preview.setMinimumHeight(280)
        tab1.addWidget(self.preview, 1)

        self.tabs.addTab(wrap1, "Собранный промпт")

        # ---------------- Tab 2: Редактор шаблона ----------------
        wrap2 = QWidget()
        tab2 = QVBoxLayout(wrap2)

        # Полоска-подсказка о плейсхолдержах
        banner = QFrame()
        banner.setFrameShape(QFrame.Shape.StyledPanel)
        banner_l = QHBoxLayout(banner)
        self.banner_label = QLabel(
            "В шаблоне не найдены плейсхолдеры. Рекомендуется вставить {charas} и/или {terms}."
        )
        self.banner_label.setStyleSheet("color:#b45309;")
        self.btn_ins_charas = QPushButton("Вставить {charas}")
        self.btn_ins_terms = QPushButton("Вставить {terms}")
        banner_l.addWidget(self.banner_label)
        banner_l.addStretch(1)
        banner_l.addWidget(self.btn_ins_charas)
        banner_l.addWidget(self.btn_ins_terms)
        tab2.addWidget(banner)
        self.banner = banner

        # Сам редактор и панель действий
        self.editor = QTextEdit()
        self.editor.setAcceptRichText(False)
        self.editor.setPlaceholderText("Здесь редактируется содержимое notes_file…")
        tab2.addWidget(self.editor, 1)

        editor_actions = QHBoxLayout()
        editor_actions.addStretch(1)
        self.btn_save_template = QPushButton("Сохранить шаблон")
        editor_actions.addWidget(self.btn_save_template)
        tab2.addLayout(editor_actions)

        self.tabs.addTab(wrap2, "Шаблон (notes_file)")

        # Сигналы
        self.cb_charas.toggled.connect(self.refresh_from_project)
        self.cb_terms.toggled.connect(self.refresh_from_project)
        self.btn_refresh.clicked.connect(self.refresh_from_project)
        self.btn_copy.clicked.connect(self._copy_to_clipboard)

        self.btn_ins_charas.clicked.connect(lambda: self._insert_placeholder("{charas}"))
        self.btn_ins_terms.clicked.connect(lambda: self._insert_placeholder("{terms}"))
        self.btn_save_template.clicked.connect(self._save_editor_to_file)
        self.editor.textChanged.connect(self._on_editor_changed)

        # Дебаунсер для автообновления
        self._debounce_timer = QTimer(self)
        self._debounce_timer.setInterval(200)
        self._debounce_timer.setSingleShot(True)
        self._debounce_timer.timeout.connect(self.refresh_from_project)

        # Вотчер файлов
        self._watcher = QFileSystemWatcher(self)
        self._install_watches()
        self._watcher.fileChanged.connect(self._on_file_changed)

        # Флаги редактора
        self._editor_dirty = False

        # Первичная сборка
        self._load_template_to_editor()
        self.refresh_from_project()

    # ---------------------------- UI helpers ----------------------------
    def _ui_path_text(self) -> str:
        t = str(self.template_path) if self.template_path else "(не задан)"
        c = str(self.char_json_path) if self.char_json_path else "(не задан)"
        m = str(self.terms_json_path) if self.terms_json_path else "(не задан)"
        return f"Шаблон: {t}\nПерсонажи: {c}\nТермины: {m}"

    def _install_watches(self) -> None:
        paths: List[str] = []
        for p in (self.template_path, self.char_json_path, self.terms_json_path):
            try:
                if p and Path(p).exists():
                    paths.append(str(p))
            except Exception:
                pass
        if paths:
            try:
                if self._watcher.files():
                    self._watcher.removePaths(self._watcher.files())
            except Exception:
                pass
            self._watcher.addPaths(paths)

    def _on_file_changed(self, path: str) -> None:
        # Если изменился сам шаблон и редактор не грязный — перезагрузим в редактор
        if Path(path) == self.template_path and not self._editor_dirty:
            self._load_template_to_editor()
        # Небольшой дебаунс, чтобы дождаться полного сохранения файла
        self._debounce_timer.start()

    # ---------------------------- Public API ----------------------------
    def refresh_from_project(self) -> None:
        """Перечитать шаблон/данные и пересобрать текст предпросмотра.
        Также обновляет баннер про плейсхолдеры."""
        try:
            template = self._read_text_fallback(self.template_path)
            charas_block = self._build_characters_block()
            terms_block = self._build_terms_block()

            text = template if template is not None else ""

            if self.cb_charas.isChecked():
                text, used_c = self._safe_replace(text, "{charas}", charas_block)
            else:
                text, used_c = self._safe_replace(text, "{charas}", "")

            if self.cb_terms.isChecked():
                text, used_t = self._safe_replace(text, "{terms}", terms_block)
            else:
                text, used_t = self._safe_replace(text, "{terms}", "")

            tail_parts: List[str] = []
            if self.cb_charas.isChecked() and not used_c and charas_block:
                tail_parts.append(charas_block)
            if self.cb_terms.isChecked() and not used_t and terms_block:
                tail_parts.append(terms_block)
            if tail_parts:
                if text and not text.endswith("\n"):
                    text = text.rstrip() + "\n"
                text += "\n".join(p.strip() for p in tail_parts if p.strip()) + "\n"

            self.preview.setPlainText(text)

            # Обновим баннер редактора
            self._update_placeholder_banner(self.editor.toPlainText())
        except Exception as e:
            QMessageBox.warning(self, "Заметки перевода", f"Не удалось собрать текст:\n{e}")

    # ---------------------------- Builders ----------------------------
    def _build_characters_block(self) -> str:
        items: List[Tuple[str, str, List[str]]] = []
        try:
            if self.char_json_path and self.char_json_path.is_file():
                with open(self.char_json_path, "r", encoding="utf-8") as f:
                    data = json.load(f) or []
                for it in data:
                    name = (it.get("name") or "").strip()
                    desc = (it.get("description") or "").strip()
                    groups = [g for g in (it.get("group") or []) if (g or "").strip()]
                    if name:
                        items.append((name, desc, groups))
        except Exception:
            return ""
        items.sort(key=lambda t: t[0].lower())
        if not items:
            return ""

        lines = ["## **Персонажи**", ""]
        for name, desc, groups in items:
            tag = f" _(группы: {', '.join(groups)})_" if groups else ""
            lines.append(f"**{name}**{tag}")
            lines.append(desc or "(без описания)")
            lines.append("")
        return "\n".join(lines).rstrip() + "\n"

    def _build_terms_block(self) -> str:
        items: List[Tuple[str, str, str, List[str]]] = []
        try:
            if self.terms_json_path and self.terms_json_path.is_file():
                with open(self.terms_json_path, "r", encoding="utf-8") as f:
                    data = json.load(f) or []
                for it in data:
                    name = (it.get("name") or "").strip()
                    orig = (it.get("orig_name") or "").strip()
                    desc = (it.get("description") or "").strip()
                    tags = [t for t in (it.get("tags") or []) if (t or "").strip()]
                    if name:
                        items.append((name, orig, desc, tags))
        except Exception:
            return ""
        items.sort(key=lambda t: t[0].lower())
        if not items:
            return ""

        lines = ["## **Термины**", ""]
        for name, orig, desc, tags in items:
            lines.append(f"**{name}**")
            if orig:
                lines.append(f"Оригинальное название: {orig}")
            if desc:
                lines.append(f"Описание: {desc}")
            if tags:
                lines.append(f"Теги: {', '.join(tags)}")
            lines.append("")
        return "\n".join(lines).rstrip() + "\n"

    # ---------------------------- Template editor ----------------------------
    def _load_template_to_editor(self) -> None:
        text = self._read_text_fallback(self.template_path)
        self.editor.blockSignals(True)
        self.editor.setPlainText(text or "")
        self.editor.blockSignals(False)
        self._editor_dirty = False
        self._update_placeholder_banner(text or "")

    def _save_editor_to_file(self) -> None:
        if not self.template_path:
            QMessageBox.warning(self, "Шаблон", "Путь к notes_file не задан в проекте.")
            return
        try:
            self.template_path.parent.mkdir(parents=True, exist_ok=True)
            self.template_path.write_text(self.editor.toPlainText(), encoding="utf-8")
            self._editor_dirty = False
            self.btn_save_template.setText("Сохранено!")
            QTimer.singleShot(900, lambda: self.btn_save_template.setText("Сохранить шаблон"))
            # После сохранения обновим предпросмотр
            self.refresh_from_project()
        except Exception as e:
            QMessageBox.critical(self, "Сохранение шаблона", f"Не удалось сохранить файл:\n{e}")

    def _on_editor_changed(self) -> None:
        self._editor_dirty = True
        self._update_placeholder_banner(self.editor.toPlainText())

    def _update_placeholder_banner(self, text: str) -> None:
        has_c = "{charas}" in (text or "")
        has_t = "{terms}" in (text or "")
        show = not (has_c or has_t)
        self.banner.setVisible(show)
        self.btn_ins_charas.setEnabled(True)
        self.btn_ins_terms.setEnabled(True)
        if show:
            self.banner_label.setText(
                "В шаблоне не найдены плейсхолдеры. Рекомендуется вставить {charas} и/или {terms}."
            )
        else:
            # Раз плейсхолдеры есть — скрываем баннер
            self.banner.setVisible(False)

    def _insert_placeholder(self, ph: str) -> None:
        cursor = self.editor.textCursor()
        if cursor and cursor.hasSelection():
            cursor.insertText(ph)
        else:
            self.editor.insertPlainText(ph)
        self._editor_dirty = True
        self._update_placeholder_banner(self.editor.toPlainText())

    # ---------------------------- Utils ----------------------------
    @staticmethod
    def _read_text_fallback(path: Path | str | None) -> Optional[str]:
        if not path:
            return ""
        p = Path(path)
        if not p.exists():
            return ""
        for enc in ("utf-8", "cp1251", "latin-1"):
            try:
                return p.read_text(encoding=enc)
            except UnicodeDecodeError:
                continue
        return p.read_bytes().decode("utf-8", errors="replace")

    @staticmethod
    def _safe_replace(text: str, placeholder: str, block: str) -> tuple[str, bool]:
        if placeholder in text:
            return text.replace(placeholder, block or ""), True
        return text, False

    # ---------------------------- Actions ----------------------------
    def _copy_to_clipboard(self) -> None:
        text = self.preview.toPlainText()
        QGuiApplication.clipboard().setText(text)
        self.btn_copy.setText("Скопировано!")
        QTimer.singleShot(800, lambda: self.btn_copy.setText("Скопировать"))
