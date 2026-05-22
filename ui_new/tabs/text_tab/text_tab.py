# ui_new/tabs/text_tab/text_tab.py
from __future__ import annotations
import os
from PyQt6.QtWidgets import QWidget, QVBoxLayout, QStackedWidget
from .text_view import TextCanvasViewQt
from .text_panel import TextFontPanelQt, CreationTextPanelQt, EditTextPanelQt
from .text_overlay_item import TextOverlayItem
from PyQt6.QtGui import QImage
from dataclasses import asdict
from .text_style import TextStyle, StyleBinding

class TextEditorTabQt(QWidget):
    def __init__(
        self,
        project,
        parent=None,
        bubbles_model=None,
        source_images=None,
        overlays_model=None,
        text_detection_model=None,
        user_config=None,
    ):
        super().__init__(parent)
        # ensure_saved() и load() уже вызваны в qt_runner.py:heavy_init()
        # Дублирующий вызов удалён для ускорения запуска

        cleaned_paths = [
            os.path.join(project.src_dir, fn)
            for fn in os.listdir(project.src_dir)
            if os.path.isfile(os.path.join(project.src_dir, fn))
        ]

        if source_images:
            # Сохраняем порядок как у исходников, чтобы индексы совпадали с CleanOverlaysModel
            by_base = {os.path.basename(p): p for p in cleaned_paths}
            ordered = []
            for src in source_images:
                base = os.path.basename(src)
                if base in by_base:
                    ordered.append(by_base.pop(base))
            imgs = ordered + sorted(by_base.values())
        else:
            imgs = sorted(cleaned_paths)

        self.model = bubbles_model
        self._default_creation_state = None  # сохраним дефолтное состояние панели создания
        self.clean_overlays_model = overlays_model
        self.text_detection_model = text_detection_model
        # Поля состояния редактирования (должны существовать до создания binding/panel)
        self.editing_overlay = None  # текущий редактируемый оверлей
        self.temp_edit_changes = {}  # временное хранилище изменений
        self._pending_style_updates: dict = {}  # несохранённые правки стиля для текущего оверлея

        # === ВЕРТИКАЛЬНАЯ КОМПОНОВКА: [Верхняя лента] + [CanvasView] ===
        root = QVBoxLayout(self)
        root.setContentsMargins(0, 0, 0, 0)
        root.setSpacing(0)

        # --- Верхняя полоса с заголовком и сворачиваемой «лентой» ---
        top_bar = QWidget(self)
        top_bar.setObjectName("TextTopBar")
        top_bar_lay = QVBoxLayout(top_bar)
        top_bar_lay.setContentsMargins(8, 8, 8, 4)
        top_bar_lay.setSpacing(6)

        self.preview_label = None
        # --- Лента (горизонтальная): переносим все группы из TextFontPanelQt бок-о-бок ---
        self.view = TextCanvasViewQt(
            project,
            imgs,
            parent=self,
            bubbles_model=bubbles_model,
            overlays_model=overlays_model,
            user_config=user_config,
        )
        def _apply_creation_patch(patch: dict):
            # Шрифт обрабатываем отдельно, чтобы сохранять/восстанавливать настройки по семействам.
            if "font_family" in patch:
                target_family = patch["font_family"]
                self.view.ui_set_font_family(target_family)
                # Остальные изменения применяем уже к новому состоянию.
                rest = {k: v for k, v in patch.items() if k != "font_family"}
                if rest:
                    self.view._patch_style(rest, sync_panel=False)
            else:
                self.view._patch_style(patch, sync_panel=False)
            self._update_preview()

        self.creation_binding = StyleBinding(
            get_style=lambda: self.view.current_style,
            on_change=_apply_creation_patch
        )

        src_panel = TextFontPanelQt(
            binding=self.creation_binding,
            custom_font_files=self.view.custom_font_files,
            font_file_map=self.view.font_file_map,
            custom_font_families=self.view.custom_font_families,
            on_export=self.view.export_overlays_with_dialog,
            on_toggle_clean_overlays=(self.view.set_clean_overlays_visible if overlays_model else None),
            get_clean_overlays_visible=(self.view.get_clean_overlays_visible if overlays_model else None),
            parent=self,
            assemble_layout=False
        )
        self.view.attach_panel(src_panel)
        # Пипетка через CanvasView
        src_panel.set_eyedropper_starter(
            lambda on_prev, on_commit, on_cancel: self.view.start_color_picker(on_prev, on_commit, on_cancel)
        )

        # --- РЕНДЕР превью: отдаём функцию в панель ---
        def _render_preview(width_px: int):
            text = "Текст будет\nвыглядеть так"
            st_dict = self.view._state_snapshot()
            style = TextStyle.from_dict(st_dict)
            return self.view._renderer.big_renderer(**style.to_renderer_kwargs(text=text, width_px=int(width_px)))

        # Сохраняем функцию рендера для переиспользования
        self._creation_render_fn = _render_preview

        # === ПАНЕЛИ: создание и редактирование (переключаемые) ===
        self.panel_stack = QStackedWidget(top_bar)

        # 1) Панель СОЗДАНИЯ (индекс 0) - всегда видима
        self.creation_panel = CreationTextPanelQt(font_panel=src_panel, render_fn=self._creation_render_fn, parent=self.panel_stack)
        self.panel_stack.addWidget(self.creation_panel)

        # 2) Панель РЕДАКТИРОВАНИЯ (индекс 1) - создаётся один раз, скрыта по умолчанию
        # Создаём заглушки для коллбэков (будут обновлены при выделении оверлея)
        self._edit_style: TextStyle = TextStyle.from_dict(self.view._state_snapshot())
        self.edit_binding = StyleBinding(
            get_style=lambda: self._edit_style,
            on_change=lambda patch: self._on_edit_style_changed(patch)
        )

        dummy_meta = dict(self._edit_style.to_dict())
        dummy_meta.update({
            "width_px": 300,
            "user_scale": 1.0,
            "angle": 0.0,
            "text": "",
            "custom_font_files": self.view.custom_font_files,
            "font_file_map": self.view.font_file_map,
            "custom_font_families": self.view.custom_font_families,
        })

        # Обёртки для коллбэков панели редактирования - будем вызывать метод update_for_overlay
        def _wrap_edit(cb_name):
            def _inner(*args, **kwargs):
                if hasattr(self, '_edit_callbacks') and cb_name in self._edit_callbacks:
                    self._edit_callbacks[cb_name](*args, **kwargs)
            return _inner

        self._edit_callbacks = {}  # Словарь для динамических коллбэков

        self.edit_panel = EditTextPanelQt(
            overlay_meta=dummy_meta,
            style_binding=self.edit_binding,
            on_text_changed=_wrap_edit('on_text_changed'),
            on_width_changed=_wrap_edit('on_width_changed'),
            on_scale_changed=_wrap_edit('on_scale_changed'),
            on_angle_changed=_wrap_edit('on_angle_changed'),
            on_apply=_wrap_edit('on_apply'),
            on_delete=_wrap_edit('on_delete'),
            eyedropper_starter=lambda on_prev, on_commit, on_cancel: self.view.start_color_picker(on_prev, on_commit, on_cancel),
            render_fn=lambda w: None,
            parent=self.panel_stack
        )
        self.panel_stack.addWidget(self.edit_panel)

        # Устанавливаем панель создания по умолчанию
        self.panel_stack.setCurrentIndex(0)
        self.top_panel = self.creation_panel  # для обратной совместимости

        top_bar_lay.addWidget(self.panel_stack)

        # Внизу — CanvasView
        root.addWidget(top_bar, 0)
        root.addWidget(self.view, 1)

        # Подключаем обработчики выделения/снятия выделения оверлеев
        self.view.setup_overlay_selection_handler(self.on_overlay_selected)
        self.view._on_overlay_deselected = self.on_overlay_deselected

        # Сохраняем дефолтное состояние панели создания для восстановления
        self._default_creation_state = self.view._state_snapshot()

        self._update_preview()

    def _update_preview(self):
        if hasattr(self, "top_panel"):
            self.top_panel.update_preview()

    def _on_edit_style_changed(self, patch: dict):
        """Обновление копии стиля для выбранного оверлея."""
        self._edit_style = self._edit_style.with_updates(**patch)
        if getattr(self, "editing_overlay", None):
            # Запоминаем несохранённые изменения, чтобы не потерять их при повторной инициализации панели
            self._pending_style_updates.update(patch)
        if getattr(self, "edit_panel", None):
            self.edit_panel.update_preview()

    def resizeEvent(self, ev):
        super().resizeEvent(ev)
        self._update_preview()

    # ==================== МЕТОДЫ УПРАВЛЕНИЯ ПАНЕЛЯМИ ====================

    def on_overlay_selected(self, overlay_item: TextOverlayItem):
        """Обработчик выделения текстового оверлея"""
        self.show_edit_panel(overlay_item)

    def on_overlay_deselected(self):
        """Обработчик снятия выделения с оверлея"""
        self.show_creation_panel()

    def show_creation_panel(self):
        """Показать панель создания текста"""
        # Просто переключаем на панель создания (индекс 0)
        self.panel_stack.setCurrentIndex(0)
        self.top_panel = self.creation_panel
        self.editing_overlay = None
        self.temp_edit_changes.clear()
        self._pending_style_updates.clear()

        # Восстанавливаем сохранённое состояние панели создания
        # Приоритет: сохранённое состояние > дефолтное состояние
        if hasattr(self, '_saved_creation_state') and self._saved_creation_state and hasattr(self.view, '_panel'):
            self.view._panel.apply_state(self._saved_creation_state)
        elif self._default_creation_state and hasattr(self.view, '_panel'):
            self.view._panel.apply_state(self._default_creation_state)

        self._update_preview()

    def show_edit_panel(self, overlay_item: TextOverlayItem):
        """Показать панель редактирования для выбранного оверлея"""
        if not isinstance(overlay_item, TextOverlayItem):
            return

        same_overlay = overlay_item is self.editing_overlay
        if not same_overlay:
            # Новое выделение — сбрасываем накопленные черновые правки стиля
            self._pending_style_updates.clear()

        self.editing_overlay = overlay_item
        meta_dict = asdict(overlay_item.meta)
        self.temp_edit_changes.clear()

        base_style = self._edit_style if (same_overlay and self._pending_style_updates) else None
        if base_style is None:
            base_style = TextStyle.from_dict(meta_dict.get("style") or meta_dict)
        self._edit_style = base_style

        # Сохраняем текущее состояние панели создания перед переключением
        if hasattr(self.view, '_panel'):
            self._saved_creation_state = self.view._state_snapshot()

        # Вычисляем ширину в пикселях из w_frac
        if meta_dict.get("img_idx", 0) < len(self.view.image_bboxes):
            page_w = int(self.view.image_bboxes[meta_dict["img_idx"]].width())
            width_px = int(meta_dict.get("w_frac", 0.3) * page_w)
        else:
            width_px = 300

        meta_dict["width_px"] = width_px
        # Добавляем недостающие поля для TextFontPanelQt
        meta_dict["custom_font_families"] = self.view.custom_font_families
        meta_dict["custom_font_files"] = self.view.custom_font_files
        meta_dict["font_file_map"] = self.view.font_file_map

        # Колбэки для изменения параметров
        def on_text_changed(text: str):
            self.temp_edit_changes["text"] = text
            if self.edit_panel:
                self.edit_panel.update_preview()

        def on_width_changed(width_px: int):
            self.temp_edit_changes["width_px"] = width_px
            if self.edit_panel:
                self.edit_panel.update_preview()

        def on_scale_changed(scale: float):
            # Масштаб применяется сразу к оверлею
            self.editing_overlay.meta.user_scale = scale
            self.editing_overlay.setScale(scale)
            self.view._save_text_info_json()

        def on_angle_changed(angle: float):
            # Угол применяется сразу к оверлею
            self.editing_overlay.meta.angle = angle
            self.editing_overlay.setRotation(angle)
            self.view._save_text_info_json()

        def on_apply():
            self.apply_overlay_changes()

        def on_delete():
            self.delete_current_overlay()

        # Функция рендера превью - рендерит точно так же, как итоговый оверлей
        def render_edit_preview(preview_area_width: int):
            # Игнорируем preview_area_width, используем реальную ширину оверлея
            # ВАЖНО: всегда берём актуальные данные из self.editing_overlay.meta
            if not self.editing_overlay:
                return QImage()

            current_meta = asdict(self.editing_overlay.meta)
            # Добавляем width_px
            if current_meta.get("img_idx", 0) < len(self.view.image_bboxes):
                page_w = int(self.view.image_bboxes[current_meta["img_idx"]].width())
                current_meta["width_px"] = int(current_meta.get("w_frac", 0.3) * page_w)
            else:
                current_meta["width_px"] = 300

            text = self.temp_edit_changes.get("text", current_meta.get("text", ""))
            render_width = self.temp_edit_changes.get("width_px", current_meta.get("width_px", 300))
            return self.view._renderer.big_renderer(**self._edit_style.ensure_exclusive_gradients().to_renderer_kwargs(
                text=text,
                width_px=int(render_width)
            ))

        # Регистрируем коллбэки в словаре для динамического вызова
        self._edit_callbacks['on_text_changed'] = on_text_changed
        self._edit_callbacks['on_width_changed'] = on_width_changed
        self._edit_callbacks['on_scale_changed'] = on_scale_changed
        self._edit_callbacks['on_angle_changed'] = on_angle_changed
        self._edit_callbacks['on_apply'] = on_apply
        self._edit_callbacks['on_delete'] = on_delete


        # Обновляем существующую панель редактирования новыми параметрами
        panel_state = dict(self._edit_style.to_dict())
        panel_state.update({
            "text": meta_dict.get("text", ""),
            "width_px": width_px,
            "user_scale": meta_dict.get("user_scale", 1.0),
            "angle": meta_dict.get("angle", 0.0),
            "custom_font_files": self.view.custom_font_files,
            "font_file_map": self.view.font_file_map,
            "custom_font_families": self.view.custom_font_families,
        })

        self.edit_panel.update_for_overlay(
            overlay_meta=panel_state,
            on_text_changed=on_text_changed,
            on_width_changed=on_width_changed,
            on_scale_changed=on_scale_changed,
            on_angle_changed=on_angle_changed,
            on_apply=on_apply,
            on_delete=on_delete,
            render_fn=render_edit_preview
        )

        # Показываем панель редактирования
        self.panel_stack.setCurrentWidget(self.edit_panel)
        self.top_panel = self.edit_panel

        # Подключаем обновление панели при изменении параметров оверлея
        self._setup_overlay_update_handlers(overlay_item)

        # Обновляем превью
        self.edit_panel.update_preview()

    def apply_overlay_changes(self):
        """Применить изменения к текущему редактируемому оверлею"""
        if not self.editing_overlay:
            return

        # Применяем все изменения к метаданным (ширина/текст/стиль)
        width_val = self.temp_edit_changes.get("width_px")
        if width_val is not None and self.editing_overlay.meta.img_idx < len(self.view.image_bboxes):
            page_w = self.view.image_bboxes[self.editing_overlay.meta.img_idx].width()
            self.editing_overlay.meta.w_frac = width_val / max(1, page_w)

        if "text" in self.temp_edit_changes:
            self.editing_overlay.meta.text = self.temp_edit_changes["text"]

        self.editing_overlay.meta.style = self._edit_style.ensure_exclusive_gradients()

        # Получаем уже отрендеренное изображение из превью панели редактирования
        # Превью рендерит текст с учётом всех изменений (включая новую ширину)
        if self.edit_panel and hasattr(self.edit_panel, '_render_fn') and callable(self.edit_panel._render_fn):
            # Рендерим с реальной шириной (не для показа превью, а для применения)
            qimg = self.edit_panel._render_fn(0)  # передаём 0, чтобы функция использовала реальную ширину

            if qimg and not qimg.isNull():
                # Обновляем базовое изображение оверлея
                self.editing_overlay._base = qimg

                # Применяем новый pixmap
                self.editing_overlay._apply_pixmap(target_width_px=None)

                # Обновляем w_frac на основе реальной ширины отрендеренного изображения
                if self.editing_overlay.meta.img_idx < len(self.view.image_bboxes):
                    page_w = self.view.image_bboxes[self.editing_overlay.meta.img_idx].width()
                    self.editing_overlay.meta.w_frac = qimg.width() / max(1, page_w)

                # Обновляем позицию оверлея (центр должен остаться на месте)
                self.view._apply_overlay_geometry_from_meta(self.editing_overlay)

                # ВАЖНО: Пересчитываем маски для оверлея, так как его размер изменился
                if hasattr(self.view, '_cutLinesManager') and self.editing_overlay.meta.img_idx < len(self.view.image_bboxes):
                    # Инвалидируем кэш
                    self.view._cutLinesManager.invalidate_overlay_cache(self.editing_overlay)
                    # Явно пересчитываем компоненты с новыми размерами
                    page_bbox = self.view.image_bboxes[self.editing_overlay.meta.img_idx]
                    self.view._cutLinesManager.compute_overlay_components(self.editing_overlay, page_bbox)

                # Сохраняем изображение в проект
                import os
                qimg.save(os.path.join(self.view.project.text_images, self.editing_overlay.meta.file))

                # Сохраняем метаданные
                self.view._save_text_info_json()

                # Обновляем отображение оверлея
                self.editing_overlay.update()
            else:
                # Fallback: если превью не удалось отрендерить, используем старый метод
                self.view.recreate_overlay_item(self.editing_overlay)
        else:
            # Fallback: если render_fn недоступен, используем старый метод
            self.view.recreate_overlay_item(self.editing_overlay)

        # Очищаем временные изменения
        self.temp_edit_changes.clear()

        # Обновляем панель редактирования актуальными данными оверлея
        updated_meta = dict(self._edit_style.to_dict())
        updated_meta["text"] = getattr(self.editing_overlay.meta, "text", "")
        updated_meta["user_scale"] = getattr(self.editing_overlay.meta, "user_scale", 1.0)
        updated_meta["angle"] = getattr(self.editing_overlay.meta, "angle", 0.0)
        # Добавляем width_px
        if self.editing_overlay.meta.img_idx < len(self.view.image_bboxes):
            page_w = int(self.view.image_bboxes[self.editing_overlay.meta.img_idx].width())
            updated_meta["width_px"] = int(getattr(self.editing_overlay.meta, "w_frac", 0.3) * page_w)
        else:
            updated_meta["width_px"] = 300

        # Обновляем виджеты панели редактирования
        self.edit_panel.edit_font_panel.apply_style(self._edit_style)

        # Обновляем виджеты блока редактирования текста
        self.edit_panel.edit_text.blockSignals(True)
        self.edit_panel.edit_text.setPlainText(updated_meta.get("text", ""))
        self.edit_panel.edit_text.blockSignals(False)

        self.edit_panel.width_spin.blockSignals(True)
        self.edit_panel.width_spin.setValue(int(updated_meta.get("width_px", 300)))
        self.edit_panel.width_spin.blockSignals(False)

        # После применения правок сбрасываем черновые накопленные изменения
        self._pending_style_updates.clear()

        # Обновляем виджеты масштаба и угла (они могли быть изменены программно)
        self.edit_panel.scale_spin.blockSignals(True)
        self.edit_panel.scale_spin.setValue(updated_meta.get("user_scale", 1.0))
        self.edit_panel.scale_spin.blockSignals(False)

        self.edit_panel.angle_spin.blockSignals(True)
        self.edit_panel.angle_spin.setValue(updated_meta.get("angle", 0.0))
        self.edit_panel.angle_spin.blockSignals(False)

        # Обновляем превью
        self.edit_panel.update_preview()

    def delete_current_overlay(self):
        """Удалить текущий редактируемый оверлей"""
        if not self.editing_overlay:
            return

        self.view.delete_overlay_item(self.editing_overlay)
        self.editing_overlay = None
        self.show_creation_panel()

    def _setup_overlay_update_handlers(self, overlay_item: TextOverlayItem):
        """Настраивает обработчики для синхронизации панели редактирования с изменениями оверлея"""
        if not self.edit_panel:
            return

        # Создаем обработчик изменений оверлея
        def on_overlay_changed(property_name: str, item: TextOverlayItem):
            if item != self.editing_overlay or not self.edit_panel:
                return

            # Обновляем соответствующие виджеты на панели без вызова сигналов
            if property_name == "angle":
                self.edit_panel.angle_spin.blockSignals(True)
                self.edit_panel.angle_spin.setValue(item.meta.angle)
                self.edit_panel.angle_spin.blockSignals(False)
            elif property_name == "scale":
                self.edit_panel.scale_spin.blockSignals(True)
                self.edit_panel.scale_spin.setValue(item.meta.user_scale)
                self.edit_panel.scale_spin.blockSignals(False)

        # взять базовый "сценовый" обработчик; если его нет, используем текущий
        base_cb = getattr(overlay_item, "_on_changed_view", None) or getattr(overlay_item, "_on_changed", None)

        def _combined_on_changed(property_name: str, item):
            # 1) сначала логика сцены: пересчёт u/v, сохранение JSON и т.д.
            if callable(base_cb):
                base_cb(property_name, item)
            # 2) затем — обновление панели редактирования
            on_overlay_changed(property_name, item)

        overlay_item._on_changed = _combined_on_changed
