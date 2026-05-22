from __future__ import annotations
from typing import List, Dict, Any
import traceback
import re
import os
import zipfile
from xml.sax.saxutils import escape as _xml_escape
from PyQt6.QtCore import QTimer, QSignalBlocker
from PyQt6.QtWidgets import (
    QWidget, QVBoxLayout, QHBoxLayout, QLabel, QPushButton,
    QTextEdit, QSpinBox, QFrame, QLineEdit, QCheckBox, QTabWidget, QRadioButton, QButtonGroup,
    QFileDialog, QMessageBox
)
from PyQt6.QtGui import QGuiApplication

from ..utils import _is_deleted
DEBUG = False    

try:
    from jinja2 import Environment
except Exception:
    Environment = None

class CompositionPanel(QFrame):
    """
    Панель компоновки перевода для вкладки Translation.

    Предоставляет интерфейс для:
    - Автоматической сборки реплик из пузырей со статусом 'untranslated'
    - Группировки реплик по персонажам
    - Копирования готового текста для перевода
    - Обновления списка реплик с учетом лимита символов
    """

    def __init__(self, parent: QWidget, project, canvas, model):
        super().__init__(parent)
        self.setFrameShape(QFrame.Shape.StyledPanel)
        self.setStyleSheet("""
            QFrame { background: #1b1b1b; border: 1px solid #444; color: #ddd; }
            QLabel { color: #ddd; }
            QTextEdit { color: #ddd; background: #2b2b2b; border: 1px solid #555; font-family: monospace; }
            QPushButton { background: #2b2b2b; color: #eee; border: 1px solid #555; padding: 4px 8px; }
            QPushButton:hover { background: #333; }
            QSpinBox { color: #ddd; background: #2b2b2b; border: 1px solid #555; }
        """)

        self.project = project
        self.canvas = canvas
        self.model = model

        root = QVBoxLayout(self)

        # Заголовок с кнопкой закрытия
        hdr = QHBoxLayout()
        lbl = QLabel("Компоновка перевода")
        lbl.setStyleSheet("font-weight: 700; color: #fff;")
        btn_close = QPushButton("✕")
        btn_close.setFixedWidth(28)
        btn_close.clicked.connect(self.hide)
        hdr.addWidget(lbl)

        self._sort_by_order = False

        hdr.addStretch(1)
        hdr.addWidget(btn_close)
        root.addLayout(hdr)

        tabs = QTabWidget()
        root.addWidget(tabs)

        # Вкладка "Текст"
        text_tab = QWidget()
        text_layout = QVBoxLayout(text_tab)

        sort_row = QHBoxLayout()
        sort_lbl = QLabel("Сортировка:")
        self.btn_sort_toggle = QPushButton("⇆")
        self.btn_sort_toggle.setFixedWidth(32)
        self.btn_sort_toggle.setToolTip("Переключить режим сортировки")
        self.btn_sort_toggle.clicked.connect(self._toggle_sort_mode)

        self.sort_mode_label = QLabel("[По высоте]")  # текущий режим
        self.sort_mode_label.setStyleSheet("color: #bbb;")
        sort_row.addWidget(sort_lbl)
        sort_row.addWidget(self.btn_sort_toggle)
        sort_row.addWidget(self.sort_mode_label)
        sort_row.addStretch(1)
        text_layout.addLayout(sort_row)

        # Текстовое поле для отображения скомпонованных реплик
        self.text_edit = QTextEdit()
        self.text_edit.setReadOnly(True)
        self.text_edit.setPlaceholderText("Скомпонованные реплики появятся здесь...")
        text_layout.addWidget(self.text_edit)

        text_buttons = QHBoxLayout()

        btn_copy = QPushButton("Скопировать")
        btn_copy.clicked.connect(self._copy_to_clipboard)
        text_buttons.addWidget(btn_copy)

        btn_refresh = QPushButton("Обновить")
        btn_refresh.clicked.connect(self.rebuild_now)
        text_buttons.addWidget(btn_refresh)

        self.btn_export_txt = QPushButton("Экспорт в txt")
        self.btn_export_txt.clicked.connect(self._export_txt)
        text_buttons.addWidget(self.btn_export_txt)

        self.btn_export_docx = QPushButton("Экспорт в docx")
        self.btn_export_docx.clicked.connect(self._export_docx)
        text_buttons.addWidget(self.btn_export_docx)
        text_buttons.addStretch(1)

        text_layout.addLayout(text_buttons)
        tabs.addTab(text_tab, "Текст")

        # Вкладка "Параметры" (все остальные настройки, построчно)
        params_tab = QWidget()
        params_layout = QVBoxLayout(params_tab)

        row_source = QHBoxLayout()
        source_label = QLabel("Реплики:")
        self.rb_source_original = QRadioButton("Оригинал")
        self.rb_source_translation = QRadioButton("Перевод")
        self.rb_source_original.setChecked(True)
        self.source_group = QButtonGroup(self)
        self.source_group.addButton(self.rb_source_original)
        self.source_group.addButton(self.rb_source_translation)
        self.rb_source_original.toggled.connect(self.rebuild_now)
        self.rb_source_original.toggled.connect(self._save_settings)
        self.rb_source_original.toggled.connect(self._refresh_param_enable_state)
        self.rb_source_translation.toggled.connect(self.rebuild_now)
        self.rb_source_translation.toggled.connect(self._save_settings)
        self.rb_source_translation.toggled.connect(self._refresh_param_enable_state)
        row_source.addWidget(source_label)
        row_source.addWidget(self.rb_source_original)
        row_source.addWidget(self.rb_source_translation)
        row_source.addStretch(1)
        params_layout.addLayout(row_source)

        row_ignore_translated = QHBoxLayout()
        self.cb_ignore_translated_lines = QCheckBox("Игнорировать переведенные строки")
        self.cb_ignore_translated_lines.setChecked(True)
        self.cb_ignore_translated_lines.stateChanged.connect(self.rebuild_now)
        self.cb_ignore_translated_lines.stateChanged.connect(self._save_settings)
        row_ignore_translated.addWidget(self.cb_ignore_translated_lines)
        row_ignore_translated.addStretch(1)
        params_layout.addLayout(row_ignore_translated)

        row_nl = QHBoxLayout()
        nl_label = QLabel("Замена \\n:")
        self.cb_newline_replace_enabled = QCheckBox()
        self.cb_newline_replace_enabled.setChecked(True)
        self.cb_newline_replace_enabled.stateChanged.connect(self.rebuild_now)
        self.cb_newline_replace_enabled.stateChanged.connect(self._save_settings)
        self.cb_newline_replace_enabled.stateChanged.connect(self._refresh_param_enable_state)
        self.newline_input = QLineEdit()
        self.newline_input.setMaxLength(8)  # маленькое поле, но можно несколько символов
        self.newline_input.setFixedWidth(64)
        self.newline_input.setText(" ")  # по умолчанию пробел
        self.newline_input.setPlaceholderText("пробел")
        self.newline_input.setToolTip("Чем заменить символы новой строки внутри реплик")
        # Перестраиваем при изменении
        self.newline_input.textChanged.connect(self.schedule_rebuild)
        self.newline_input.textChanged.connect(self._save_settings)
        row_nl.addWidget(nl_label)
        row_nl.addWidget(self.cb_newline_replace_enabled)
        row_nl.addWidget(self.newline_input)
        row_nl.addStretch(1)
        params_layout.addLayout(row_nl)

        row_wrap = QHBoxLayout()
        wrap_label = QLabel("Оборачивать реплики в:")
        self.cb_wrap_enabled = QCheckBox()
        self.cb_wrap_enabled.setChecked(True)
        self.cb_wrap_enabled.stateChanged.connect(self.rebuild_now)
        self.cb_wrap_enabled.stateChanged.connect(self._save_settings)
        self.cb_wrap_enabled.stateChanged.connect(self._refresh_param_enable_state)
        self.wrap_input = QLineEdit()
        self.wrap_input.setMaxLength(2)
        self.wrap_input.setFixedWidth(64)
        self.wrap_input.setText("``")
        self.wrap_input.setPlaceholderText("``")
        self.wrap_input.setToolTip("Два символа: первый перед репликой, второй после")
        self.wrap_input.textChanged.connect(self.schedule_rebuild)
        self.wrap_input.textChanged.connect(self._save_settings)
        row_wrap.addWidget(wrap_label)
        row_wrap.addWidget(self.cb_wrap_enabled)
        row_wrap.addWidget(self.wrap_input)
        row_wrap.addStretch(1)
        params_layout.addLayout(row_wrap)

        row_prefix = QHBoxLayout()
        prefix_label = QLabel("Префикс реплики:")
        self.replica_prefix_input = QLineEdit()
        self.replica_prefix_input.setText("")
        self.replica_prefix_input.setPlaceholderText("")
        self.replica_prefix_input.textChanged.connect(self.schedule_rebuild)
        self.replica_prefix_input.textChanged.connect(self._save_settings)
        row_prefix.addWidget(prefix_label)
        row_prefix.addWidget(self.replica_prefix_input, 1)
        params_layout.addLayout(row_prefix)

        row_limit = QHBoxLayout()
        limit_label = QLabel("Лимит символов:")
        self.cb_limit_enabled = QCheckBox()
        self.cb_limit_enabled.setChecked(True)
        self.cb_limit_enabled.stateChanged.connect(self.rebuild_now)
        self.cb_limit_enabled.stateChanged.connect(self._save_settings)
        self.cb_limit_enabled.stateChanged.connect(self._refresh_param_enable_state)
        self.spin_limit = QSpinBox()
        self.spin_limit.setMinimum(100)
        self.spin_limit.setMaximum(100000)
        self.spin_limit.setValue(700)
        self.spin_limit.setSingleStep(100)
        self.spin_limit.valueChanged.connect(self.rebuild_now)
        self.spin_limit.valueChanged.connect(self._save_settings)
        row_limit.addWidget(limit_label)
        row_limit.addWidget(self.cb_limit_enabled)
        row_limit.addWidget(self.spin_limit)
        row_limit.addStretch(1)
        params_layout.addLayout(row_limit)

        row_chars = QHBoxLayout()
        self.cb_use_character_names = QCheckBox("Использовать имена персонажей")
        self.cb_use_character_names.setChecked(True)
        self.cb_use_character_names.stateChanged.connect(self.rebuild_now)
        self.cb_use_character_names.stateChanged.connect(self._save_settings)
        row_chars.addWidget(self.cb_use_character_names)
        row_chars.addStretch(1)
        params_layout.addLayout(row_chars)

        row_merge = QHBoxLayout()
        self.cb_merge_same_character = QCheckBox("Объединять реплики одного персонажа")
        self.cb_merge_same_character.setChecked(True)
        self.cb_merge_same_character.stateChanged.connect(self.rebuild_now)
        self.cb_merge_same_character.stateChanged.connect(self._save_settings)
        self.cb_merge_same_character.stateChanged.connect(self._refresh_param_enable_state)
        row_merge.addWidget(self.cb_merge_same_character)
        row_merge.addStretch(1)
        params_layout.addLayout(row_merge)

        row_sep_same = QHBoxLayout()
        sep_same_label = QLabel("Между репликами одного персонажа:")
        self.sep_same_character_input = QLineEdit()
        self.sep_same_character_input.setText("\\n")
        self.sep_same_character_input.setPlaceholderText("\\n")
        self.sep_same_character_input.setToolTip("Разделитель, поддерживает escape-последовательности (например \\n, \\t)")
        self.sep_same_character_input.textChanged.connect(self.schedule_rebuild)
        self.sep_same_character_input.textChanged.connect(self._save_settings)
        row_sep_same.addWidget(sep_same_label)
        row_sep_same.addWidget(self.sep_same_character_input, 1)
        params_layout.addLayout(row_sep_same)

        row_sep_between = QHBoxLayout()
        sep_between_label = QLabel("Между репликами:")
        self.sep_between_input = QLineEdit()
        self.sep_between_input.setText("\\n\\n")
        self.sep_between_input.setPlaceholderText("\\n\\n")
        self.sep_between_input.setToolTip("Разделитель, поддерживает escape-последовательности (например \\n, \\t)")
        self.sep_between_input.textChanged.connect(self.schedule_rebuild)
        self.sep_between_input.textChanged.connect(self._save_settings)
        row_sep_between.addWidget(sep_between_label)
        row_sep_between.addWidget(self.sep_between_input, 1)
        params_layout.addLayout(row_sep_between)

        jinja_block = QFrame()
        jinja_block.setStyleSheet("QFrame { border: 1px solid #444; border-radius: 4px; }")
        jinja_layout = QVBoxLayout(jinja_block)
        jinja_title = QLabel("Jinja2")
        jinja_title.setStyleSheet("font-weight: 700; color: #fff;")
        jinja_layout.addWidget(jinja_title)

        row_jinja_enabled = QHBoxLayout()
        self.cb_jinja2_enabled = QCheckBox("Использовать Jinja2-шаблон")
        self.cb_jinja2_enabled.stateChanged.connect(self.rebuild_now)
        self.cb_jinja2_enabled.stateChanged.connect(self._save_settings)
        self.cb_jinja2_enabled.stateChanged.connect(self._refresh_param_enable_state)
        row_jinja_enabled.addWidget(self.cb_jinja2_enabled)
        row_jinja_enabled.addStretch(1)
        jinja_layout.addLayout(row_jinja_enabled)

        row_jinja_vars = QHBoxLayout()
        jinja_vars_label = QLabel("Доступные переменные:")
        btn_copy_jinja_vars = QPushButton("Копировать")
        btn_copy_jinja_vars.clicked.connect(self._copy_jinja_vars_to_clipboard)
        row_jinja_vars.addWidget(jinja_vars_label)
        row_jinja_vars.addStretch(1)
        row_jinja_vars.addWidget(btn_copy_jinja_vars)
        jinja_layout.addLayout(row_jinja_vars)

        self.jinja2_vars_info = QTextEdit()
        self.jinja2_vars_info.setReadOnly(True)
        self.jinja2_vars_info.setFixedHeight(120)
        self.jinja2_vars_info.setPlainText(self._get_jinja2_vars_info_text())
        jinja_layout.addWidget(self.jinja2_vars_info)

        jinja_template_label = QLabel("Шаблон Jinja2:")
        jinja_layout.addWidget(jinja_template_label)

        self.jinja2_template_input = QTextEdit()
        self.jinja2_template_input.setPlaceholderText(
            "{% for bubble in bubbles %}{{ bubble.id }}: {{ bubble.original_text }}\n{% endfor %}"
        )
        self.jinja2_template_input.setFixedHeight(170)
        self.jinja2_template_input.textChanged.connect(self.schedule_rebuild)
        self.jinja2_template_input.textChanged.connect(self._save_settings)
        jinja_layout.addWidget(self.jinja2_template_input)

        params_layout.addWidget(jinja_block)
        params_layout.addStretch(1)

        tabs.addTab(params_tab, "Параметры")

        # Debounce для перестроения
        self._rebuild_debounce = QTimer(self)
        self._rebuild_debounce.setInterval(30)
        self._rebuild_debounce.setSingleShot(True)
        self._rebuild_debounce.timeout.connect(self.rebuild_now)
        self._connect_settings_signals()
        self._load_from_settings()
        self.rebuild_now()
        self.hide()

    def schedule_rebuild(self):
        """Запланировать обновление списка реплик."""
        if not self._rebuild_debounce.isActive():
            self._rebuild_debounce.start()

    def rebuild_now(self):
        """Немедленно обновить список реплик."""
        try:
            if self._get_jinja2_enabled():
                composed_text = self._compose_translation_text_jinja2()
            else:
                composed_text = self._compose_translation_text(
                    newline_replacement=self._get_newline_replacement(),
                    use_character_names=self._get_use_character_names(),
                    replace_newlines=self._get_newline_replace_enabled(),
                    wrap_replicas=self._get_wrap_enabled(),
                    use_limit=self._get_limit_enabled(),
                    source_mode=self._get_source_mode(),
                    ignore_translated_lines=self._get_ignore_translated_lines(),
                    merge_same_character=self._get_merge_same_character(),
                    sep_same_character=self._get_sep_same_character(),
                    sep_between=self._get_sep_between(),
                    replica_prefix=self._get_replica_prefix(),
                )
            self.text_edit.setPlainText(composed_text)
        except Exception:
            traceback.print_exc()
            self.text_edit.setPlainText("Ошибка при компоновке реплик")

    def _compose_translation_text_jinja2(self) -> str:
        if Environment is None:
            return "Jinja2 не установлен: установите пакет `jinja2`."
        bubbles = list(getattr(self.project, "bubbles", []))
        template_source = self._get_jinja2_template()
        if not template_source.strip():
            return "(шаблон Jinja2 пуст)"
        env = Environment(autoescape=False)
        template = env.from_string(template_source)
        return str(template.render(bubbles=bubbles))

    def _compose_translation_text(
        self,
        newline_replacement: str = " ",
        use_character_names: bool = True,
        replace_newlines: bool = True,
        wrap_replicas: bool = True,
        use_limit: bool = True,
        source_mode: str = "original",
        ignore_translated_lines: bool = True,
        merge_same_character: bool = True,
        sep_same_character: str = "\n",
        sep_between: str = "\n\n",
        replica_prefix: str = "",
    ) -> str:
        """
        Компонует текст из пузырей, у которых есть оригинал и пустой перевод.

        Алгоритм:
        1. Берёт пузыри с непустым original_text и пустым переводом
        2. Сортирует по высоте расположения (img_idx, затем img_v)
        3. Группирует реплики по персонажам
        4. Форматирует в виде: `реплика` - персонаж
        5. Учитывает лимит символов
        """
        bubbles = list(getattr(self.project, "bubbles", []))

        # Фильтруем пузыри по выбранному источнику реплик
        use_original = (str(source_mode).lower() != "translation")
        untranslated = []
        for b in bubbles:
            translation_text = str(b.get('text', '') or '').strip()
            original_text = str(b.get('original_text', '') or '').strip()

            if use_original:
                # Режим "Оригинал": берём original_text, опционально исключаем уже переведённые
                if not original_text:
                    continue
                if ignore_translated_lines and translation_text:
                    continue
            else:
                # Режим "Перевод": берём text
                if not translation_text:
                    continue

            # Проверяем, что пузырь размещён на холсте
            if (b.get('img_idx') is not None and
                b.get('img_v') is not None):
                untranslated.append(b)

        if not untranslated:
            return "(нет реплик для компоновки)"

        # Сортируем по высоте: сначала по индексу изображения, затем по V-координате
        # V-координата увеличивается сверху вниз, поэтому меньшее значение = выше на холсте
        def sort_key_height(b):
            img_idx = b.get('img_idx')
            img_v = b.get('img_v')
            if img_idx is None:
                img_idx = 0
            if img_v is None:
                img_v = 0.0
            return (int(img_idx), float(img_v))

        def sort_key_order(b):
            img_idx = b.get('img_idx')
            bubble_order = b.get('bubble_order', 0)
            img_v = b.get('img_v')
            if img_idx is None:
                img_idx = 0
            # bubble_order по умолчанию 0, чтобы старые записи стабильно сортировались
            try:
                bubble_order = int(bubble_order)
            except Exception:
                bubble_order = 0
            if img_v is None:
                img_v = 0.0
            # стабилизируем сортировку по позиции внутри страницы на случай одинаковых номеров
            return (int(img_idx), bubble_order, float(img_v))

        untranslated.sort(key=sort_key_order if self._sort_by_order else sort_key_height)

        # Отладочный вывод для проверки сортировки
        if DEBUG: print(f"[CompositionPanel] Найдено {len(untranslated)} непереведённых пузырей")
        for b in untranslated:
            if DEBUG: print(
                f"  id={b.get('id')}, img_idx={b.get('img_idx')}, img_v={b.get('img_v'):.4f}, "
                f"order={b.get('bubble_order', 0)}, "
                f"char='{b.get('character_name', '')}', clarification='{b.get('clarification', '')}', "
                f"original_len={len(str(b.get('original_text', '') or ''))}"
            )

        # Компонуем текст с учётом группировки по персонажам
        result_lines = []
        current_length = 0
        limit = self.spin_limit.value()
        prev_character = None
        current_group = []  # Текущая группа реплик от одного персонажа
        wrap_left, wrap_right = self._get_replica_wrap_chars() if wrap_replicas else ("", "")

        def _append_result_item(item_text: str, force: bool = False) -> bool:
            nonlocal current_length
            separator = sep_between if result_lines else ""
            new_length = current_length + len(separator) + len(item_text)
            if use_limit and new_length > limit and result_lines and not force:
                return False
            result_lines.append(item_text)
            current_length = new_length
            return True

        for bubble in untranslated:
            src_text = str(bubble.get('original_text' if use_original else 'text', '')).strip()
            if not src_text:
                continue

            # Нормализуем переводы строк и заменяем на указанный символ
            # Сначала приводим CRLF/CR к LF
            src_text = src_text.replace('\r\n', '\n').replace('\r', '\n')
            # Затем (опционально) заменяем LF на выбранную подстановку
            if replace_newlines:
                src_text = src_text.replace('\n', newline_replacement)
            # Нормализуем только пробельные последовательности (замена не трогается)
            src_text = re.sub(r'[ \t]+', ' ', src_text).strip()

            # Определяем персонажа
            # Если поле is_known_character отсутствует, считаем что это известный персонаж (True)
            is_known = bubble.get('is_known_character', True)
            char_name = bubble.get('character_name', '').strip()
            clarification = bubble.get('clarification', '').strip()

            # Логика определения персонажа:
            # 1. Если есть имя и is_known_character == True (или отсутствует) -> используем имя
            # 2. Если есть имя и is_known_character == False -> это произвольное имя (рассказчик с именем)
            # 3. Если нет имени и is_known_character == True -> неизвестный персонаж
            # 4. Если нет имени и is_known_character == False -> рассказчик
            if char_name:
                # Есть имя - используем его
                character = char_name
            else:
                # Нет имени
                if is_known:
                    character = "(неизвестный персонаж)"
                else:
                    character = "(рассказчик)"

            # Если есть уточнение, добавляем его в скобках к имени персонажа
            # Это также создаёт новую группу (как будто другой персонаж)
            if clarification and is_known:
                character_with_clarification = f"{character} ({clarification})"
            else:
                character_with_clarification = character

            line_text = f"{replica_prefix}{wrap_left}{src_text}{wrap_right}"
            if not use_character_names:
                if not _append_result_item(line_text):
                    break
                continue
            # Проверяем смену персонажа (с учётом уточнения)
            if not merge_same_character:
                single_line = f"{line_text} - {character_with_clarification}"
                if not _append_result_item(single_line):
                    break
                continue

            if prev_character is None:
                # Первая реплика
                current_group = [line_text]
                prev_character = character_with_clarification
            elif character_with_clarification == prev_character:
                # Тот же персонаж (с тем же уточнением или без него)
                current_group.append(line_text)
            else:
                # Смена персонажа - завершаем предыдущую группу
                group_text = sep_same_character.join(current_group) + f" - {prev_character}"
                if not _append_result_item(group_text):
                    # Лимит превышен, добавляем имя к последней группе и выходим
                    break

                # Начинаем новую группу
                current_group = [line_text]
                prev_character = character_with_clarification

        # Добавляем последнюю группу
        # Согласно требованиям: последняя реплика и её персонаж добавятся полностью
        # даже если превышен лимит символов
        if merge_same_character and current_group and prev_character:
            group_text = sep_same_character.join(current_group) + f" - {prev_character}"
            _append_result_item(group_text, force=True)

        return sep_between.join(result_lines)

    def _copy_to_clipboard(self):
        """Копирует скомпонованный текст в буфер обмена."""
        text = self.text_edit.toPlainText()
        if text:
            QGuiApplication.clipboard().setText(text)
            if DEBUG: print("[CompositionPanel] Текст скопирован в буфер обмена")

    def _suggest_export_path(self, ext: str) -> str:
        base_dir = os.getcwd()
        if getattr(self, "project", None) is not None and getattr(self.project, "path", None):
            base_dir = self.project.path
        return os.path.join(base_dir, f"composition_export.{ext}")

    def _export_txt(self):
        text = self.text_edit.toPlainText()
        start_path = self._suggest_export_path("txt")
        file_path, _ = QFileDialog.getSaveFileName(
            self,
            "Сохранить текст",
            start_path,
            "Text files (*.txt);;All files (*)",
        )
        if not file_path:
            return
        try:
            with open(file_path, "w", encoding="utf-8") as f:
                f.write(text)
        except Exception as exc:
            traceback.print_exc()
            QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить TXT:\n{exc}")
            return
        QMessageBox.information(self, "Готово", f"Сохранено:\n{file_path}")

    def _export_docx(self):
        text = self.text_edit.toPlainText()
        start_path = self._suggest_export_path("docx")
        file_path, _ = QFileDialog.getSaveFileName(
            self,
            "Сохранить DOCX",
            start_path,
            "Word document (*.docx);;All files (*)",
        )
        if not file_path:
            return
        if not file_path.lower().endswith(".docx"):
            file_path += ".docx"
        try:
            self._save_simple_docx(file_path, text)
        except Exception as exc:
            traceback.print_exc()
            QMessageBox.critical(self, "Ошибка", f"Не удалось сохранить DOCX:\n{exc}")
            return
        QMessageBox.information(self, "Готово", f"Сохранено:\n{file_path}")

    def _save_simple_docx(self, file_path: str, text: str):
        lines = text.splitlines()
        if not lines:
            lines = [""]

        paragraph_xml = []
        for line in lines:
            escaped = _xml_escape(line)
            if escaped:
                paragraph_xml.append(
                    f"<w:p><w:r><w:t xml:space=\"preserve\">{escaped}</w:t></w:r></w:p>"
                )
            else:
                paragraph_xml.append("<w:p/>")

        document_xml = (
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>"
            "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">"
            "<w:body>"
            + "".join(paragraph_xml)
            + "<w:sectPr/>"
            "</w:body>"
            "</w:document>"
        )
        content_types_xml = (
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>"
            "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">"
            "<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>"
            "<Default Extension=\"xml\" ContentType=\"application/xml\"/>"
            "<Override PartName=\"/word/document.xml\" "
            "ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>"
            "</Types>"
        )
        rels_xml = (
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>"
            "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">"
            "<Relationship Id=\"rId1\" "
            "Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" "
            "Target=\"word/document.xml\"/>"
            "</Relationships>"
        )

        with zipfile.ZipFile(file_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            zf.writestr("[Content_Types].xml", content_types_xml)
            zf.writestr("_rels/.rels", rels_xml)
            zf.writestr("word/document.xml", document_xml)

    def _get_newline_replacement(self) -> str:
        """Возвращает символ(ы) для замены \\n. Пустая строка считается пробелом."""
        s = self.newline_input.text() if hasattr(self, "newline_input") else " "
        return s if s else " "

    def _get_newline_replace_enabled(self) -> bool:
        cb = getattr(self, "cb_newline_replace_enabled", None)
        return True if cb is None else bool(cb.isChecked())

    def _get_source_mode(self) -> str:
        rb = getattr(self, "rb_source_translation", None)
        if rb is not None and rb.isChecked():
            return "translation"
        return "original"

    def _get_ignore_translated_lines(self) -> bool:
        cb = getattr(self, "cb_ignore_translated_lines", None)
        return True if cb is None else bool(cb.isChecked())

    def _get_merge_same_character(self) -> bool:
        cb = getattr(self, "cb_merge_same_character", None)
        return True if cb is None else bool(cb.isChecked())

    def _decode_separator_text(self, raw_value: str, default: str) -> str:
        text = default if raw_value is None else str(raw_value)
        try:
            return bytes(text, "utf-8").decode("unicode_escape")
        except Exception:
            return text

    def _get_sep_same_character(self) -> str:
        raw = self.sep_same_character_input.text() if hasattr(self, "sep_same_character_input") else "\\n"
        return self._decode_separator_text(raw, "\n")

    def _get_sep_between(self) -> str:
        raw = self.sep_between_input.text() if hasattr(self, "sep_between_input") else "\\n\\n"
        return self._decode_separator_text(raw, "\n\n")

    def _get_replica_prefix(self) -> str:
        return self.replica_prefix_input.text() if hasattr(self, "replica_prefix_input") else ""

    def _get_replica_wrap_text(self) -> str:
        """Возвращает строку из двух символов-обёрток. По умолчанию ``."""
        s = self.wrap_input.text() if hasattr(self, "wrap_input") else "``"
        if len(s) >= 2:
            return s[:2]
        if len(s) == 1:
            return s + s
        return "``"

    def _get_replica_wrap_chars(self):
        wrap = self._get_replica_wrap_text()
        return wrap[0], wrap[1]

    def _get_wrap_enabled(self) -> bool:
        cb = getattr(self, "cb_wrap_enabled", None)
        return True if cb is None else bool(cb.isChecked())

    def _get_limit_enabled(self) -> bool:
        cb = getattr(self, "cb_limit_enabled", None)
        return True if cb is None else bool(cb.isChecked())

    def _get_jinja2_enabled(self) -> bool:
        cb = getattr(self, "cb_jinja2_enabled", None)
        return False if cb is None else bool(cb.isChecked())

    def _get_jinja2_template(self) -> str:
        te = getattr(self, "jinja2_template_input", None)
        return "" if te is None else te.toPlainText()

    def _get_jinja2_vars_info_text(self) -> str:
        return (
            "bubbles - список всех пузырей (dict)\n"
            "\n"
            "Параметры bubble:\n"
            "id - уникальный ID пузыря\n"
            "img_idx - индекс страницы/изображения\n"
            "img_u - X-координата пузыря на странице (нормализованная)\n"
            "img_v - Y-координата пузыря на странице (нормализованная)\n"
            "side - сторона страницы (left/right)\n"
            "text - перевод/текущий текст пузыря\n"
            "original_text - исходный текст из OCR\n"
            "translation_status - статус перевода (например untranslated)\n"
            "is_known_character - известный персонаж (True/False)\n"
            "character_name - имя персонажа\n"
            "clarification - уточнение к персонажу\n"
            "bubble_order - порядок реплики внутри страницы\n"
            "\n"
            "Также могут быть дополнительные поля, если они добавлены в bubble.\n"
            "\n"
            "Пример:\n"
            "{% for bubble in bubbles %}\n"
            "{{ bubble.id }} | {{ bubble.img_idx }} | {{ bubble.bubble_order }} | {{ bubble.original_text }} | {{ bubble.text }}\n"
            "{% endfor %}"
        )

    def _copy_jinja_vars_to_clipboard(self):
        QGuiApplication.clipboard().setText(self._get_jinja2_vars_info_text())
    
    def _toggle_sort_mode(self):
        """Переключает режим сортировки и обновляет заголовок."""
        self._sort_by_order = not self._sort_by_order
        self.sort_mode_label.setText("[По номеру реплики]" if self._sort_by_order else "[По высоте]")
        self._save_settings()
        self.rebuild_now()

    def _connect_settings_signals(self):
        """Доп. хук на будущее — сейчас всё подключаем прямо в __init__."""
        pass

    def _current_method(self) -> str:
        """Возвращает способ сортировки для конфигурации."""
        return "order" if self._sort_by_order else "height"

    def _set_sort_method(self, method: str):
        """Применяет способ сортировки из конфигурации к UI."""
        by_order = (str(method).lower() == "order")
        self._sort_by_order = by_order
        self.sort_mode_label.setText("[По номеру реплики]" if by_order else "[По высоте]")

    def _load_from_settings(self):
        """Загружает UI из self.project.settings.composition (если есть)."""
        settings = getattr(self.project, "settings", None)
        comp = getattr(settings, "composition", None) if settings else None
        if not comp:
            return

        # method: "height" | "order"
        method = getattr(comp, "method", "height") or "height"
        self._set_sort_method(method)

        # source mode
        source_mode = str(getattr(comp, "source_mode", "original") or "original").lower()
        with QSignalBlocker(self.rb_source_original):
            self.rb_source_original.setChecked(source_mode != "translation")
        with QSignalBlocker(self.rb_source_translation):
            self.rb_source_translation.setChecked(source_mode == "translation")

        # ignore translated lines
        ignore_translated_lines = bool(getattr(comp, "ignore_translated_lines", True))
        with QSignalBlocker(self.cb_ignore_translated_lines):
            self.cb_ignore_translated_lines.setChecked(ignore_translated_lines)

        merge_same_character = bool(getattr(comp, "merge_same_character", True))
        with QSignalBlocker(self.cb_merge_same_character):
            self.cb_merge_same_character.setChecked(merge_same_character)

        sep_same_raw = str(getattr(comp, "sep_same_character", "\\n"))
        with QSignalBlocker(self.sep_same_character_input):
            self.sep_same_character_input.setText(sep_same_raw)

        sep_between_raw = str(getattr(comp, "sep_between", "\\n\\n"))
        with QSignalBlocker(self.sep_between_input):
            self.sep_between_input.setText(sep_between_raw)

        prefix_raw = str(getattr(comp, "replica_prefix", ""))
        with QSignalBlocker(self.replica_prefix_input):
            self.replica_prefix_input.setText(prefix_raw)

        # nl_replace
        nl = getattr(comp, "nl_replace", " ")
        with QSignalBlocker(self.newline_input):
            self.newline_input.setText(nl if nl != "" else " ")
        nl_enabled = bool(getattr(comp, "nl_replace_enabled", True))
        with QSignalBlocker(self.cb_newline_replace_enabled):
            self.cb_newline_replace_enabled.setChecked(nl_enabled)

        # wrap_with
        wrap_with = getattr(comp, "wrap_with", "``")
        with QSignalBlocker(self.wrap_input):
            self.wrap_input.setText(self._normalize_wrap_with(wrap_with))
        wrap_enabled = bool(getattr(comp, "wrap_with_enabled", True))
        with QSignalBlocker(self.cb_wrap_enabled):
            self.cb_wrap_enabled.setChecked(wrap_enabled)

        # limit
        limit = getattr(comp, "limit", 700)
        try:
            limit = int(limit)
        except Exception:
            limit = 700
        limit = max(self.spin_limit.minimum(), min(self.spin_limit.maximum(), limit))
        with QSignalBlocker(self.spin_limit):
            self.spin_limit.setValue(limit)
        limit_enabled = bool(getattr(comp, "limit_enabled", True))
        with QSignalBlocker(self.cb_limit_enabled):
            self.cb_limit_enabled.setChecked(limit_enabled)
        # use_character_names
        use_names = getattr(comp, "use_character_names", True)
        with QSignalBlocker(self.cb_use_character_names):
            self.cb_use_character_names.setChecked(bool(use_names))

        jinja2_enabled = bool(getattr(comp, "jinja2_enabled", False))
        with QSignalBlocker(self.cb_jinja2_enabled):
            self.cb_jinja2_enabled.setChecked(jinja2_enabled)
        jinja2_template = str(getattr(comp, "jinja2_template", ""))
        with QSignalBlocker(self.jinja2_template_input):
            self.jinja2_template_input.setPlainText(jinja2_template)
        self._refresh_param_enable_state()

    def _save_settings(self):
        """Сохраняет текущие значения UI в self.project.settings.composition."""
        settings = getattr(self.project, "settings", None)
        comp = getattr(settings, "composition", None) if settings else None
        if not comp:
            return

        comp.method = self._current_method()
        comp.source_mode = self._get_source_mode()
        comp.ignore_translated_lines = self._get_ignore_translated_lines()
        comp.merge_same_character = self._get_merge_same_character()
        comp.sep_same_character = self.sep_same_character_input.text() if hasattr(self, "sep_same_character_input") else "\\n"
        comp.sep_between = self.sep_between_input.text() if hasattr(self, "sep_between_input") else "\\n\\n"
        comp.replica_prefix = self._get_replica_prefix()
        comp.nl_replace = self._get_newline_replacement()
        comp.nl_replace_enabled = self._get_newline_replace_enabled()
        comp.wrap_with = self._get_replica_wrap_text()
        comp.wrap_with_enabled = self._get_wrap_enabled()
        comp.limit = int(self.spin_limit.value())
        comp.limit_enabled = self._get_limit_enabled()
        comp.use_character_names = self._get_use_character_names()
        comp.jinja2_enabled = self._get_jinja2_enabled()
        comp.jinja2_template = self._get_jinja2_template()

    def _get_use_character_names(self) -> bool:
        cb = getattr(self, "cb_use_character_names", None)
        return True if cb is None else bool(cb.isChecked())

    def _normalize_wrap_with(self, value) -> str:
        s = str(value or "")
        if len(s) >= 2:
            return s[:2]
        if len(s) == 1:
            return s + s
        return "``"

    def _refresh_param_enable_state(self):
        use_jinja2 = self._get_jinja2_enabled()
        source_original = (self._get_source_mode() == "original")
        if hasattr(self, "cb_ignore_translated_lines"):
            self.cb_ignore_translated_lines.setEnabled(source_original and not use_jinja2)
        merge_same = self._get_merge_same_character()
        if hasattr(self, "sep_same_character_input"):
            self.sep_same_character_input.setEnabled(merge_same and not use_jinja2)
        if hasattr(self, "newline_input"):
            self.newline_input.setEnabled(self._get_newline_replace_enabled() and not use_jinja2)
        if hasattr(self, "wrap_input"):
            self.wrap_input.setEnabled(self._get_wrap_enabled() and not use_jinja2)
        if hasattr(self, "spin_limit"):
            self.spin_limit.setEnabled(self._get_limit_enabled() and not use_jinja2)
        for widget_name in (
            "rb_source_original",
            "rb_source_translation",
            "cb_newline_replace_enabled",
            "cb_wrap_enabled",
            "replica_prefix_input",
            "cb_limit_enabled",
            "cb_use_character_names",
            "cb_merge_same_character",
            "sep_between_input",
        ):
            widget = getattr(self, widget_name, None)
            if widget is not None:
                widget.setEnabled(not use_jinja2)
