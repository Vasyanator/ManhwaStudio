# Конспект по вкладке текста (ui_new/tabs/text_tab)
> Напоминание себе: при любых изменениях кода в этой папке обновляй этот файл.

## Карта модулей
- `text_tab.py` — обёртка вкладки `TextEditorTabQt`: собирает верхнюю ленту, переключает панель создания/редактирования, держит `TextCanvasViewQt`.
- `text_view.py` — основной Canvas: загрузка страниц, хоткеи, создание оверлеев из выделения, пипетка, экспорт PNG, слои клина, барьеры обрезки, перспективная трансформация.
- `text_panel.py` — UI блоки. `TextFontPanelQt` (универсальный набор контролов стиля), `CreationTextPanelQt` (лента создания), `EditTextPanelQt` (отдельная лента при выделении оверлея), вспомогательный контейнер `TextRibbonPanelQt`.
- `text_overlay_item.py` — `QGraphicsPixmapItem` для текста + `TextOverlayMeta` (geom + стиль). Поддержка перемещения, вращения, масштабирования, projective трансформации, ручка поворота.
- `text_style.py` — неизменяемая модель стиля + сериализация/переиспользование между UI и рендером.
- `text_render.py` — `Renderer.big_renderer`: верстает текст с переносами, формами (rectangle/oval/hexagon), эффектами (stroke/glow/shadow/gradients/reflect/shake), возвращает `QImage`.
- `cut_mask.py` — панель и логика линий обрезки/заливки для работы с клином поверх страниц.

## Поток работы
- При инициализации `TextEditorTabQt` формирует порядок страниц, создаёт `TextCanvasViewQt` и привязывает панели через `StyleBinding`:
  - Панель создания (`CreationTextPanelQt`) всегда первая; её binding патчит `view.current_style`.
  - При выделении оверлея `on_overlay_selected` → `show_edit_panel`: вытягивает `TextOverlayMeta`, создаёт копию стиля (`_edit_style`), подставляет ширину в px из `w_frac`, инициализирует `EditTextPanelQt` с колбэками.
  - Колбэки редактирования записывают черновые изменения в `temp_edit_changes`, а масштаб/угол применяются сразу к item и сохраняются.
- Применение правок: `apply_overlay_changes()` берёт отрендеренное превью из edit-панели (через её `render_fn`), обновляет PNG в `project.text_images`, пересчитывает `w_frac`, обновляет геометрию/маски и сериализует `text_info.json`. Черновые правки очищаются.
- Переключение панелей сохраняет/восстанавливает состояние панели создания (`_saved_creation_state` vs `_default_creation_state`), чтобы настройки стиля не терялись.

## Как хранится/грузится
- Все текстовые PNG лежат в `project.text_images`. Метаданные — `text_info.json` с `TextOverlayMeta` (+ вложенный `TextStyle` как JSON).
- Загрузка (`_load_overlays_from_json`) ждёт готовности `image_bboxes`, подгоняет маски клина, создаёт `TextOverlayItem` без пересчёта UV, применяет позицию/трансформацию из meta, потом вешает `_on_changed` → `_save_text_info_json`.
- `_save_text_info_json` пишет полный список оверлеев, включая эффекты, UV/transform_uv, `cut_enabled`. Градиенты взаимоисключаемы (grad2 против grad4).

## Особенности Canvas (`TextCanvasViewQt`)
- Хоткеи: вынесены в `CanvasView`; дополнительные — Shift+ЛКМ выделение, Ctrl+колесо поворот, колесо масштаб текста, Shift+колесо быстрый font size, Delete удаление, Ctrl±/0 зум холста.
- Inline-редактор: Shift+drag выделяет прямоугольник → `InlineTextEdit`; при потере фокуса рендерится `QImage` через `_renderer.big_renderer` и создаётся `TextOverlayItem`.
- Поддержка кастомных шрифтов: `_load_custom_fonts_by_file` собирает из `./fonts`, ведёт `font_file_map` (basename → family) и список `custom_font_files/families` для панелей.
- Линии обрезки/заливка (`CutLines*`): отдельная кнопка/панель, хранит маски по страницам, invalidate на перемещении оверлеев; `_get_masks_callback` в item подмешивает маски в отрисовку.
- Перспективная трансформация: кнопки `Трансформация/Выйти/Сбросить`, режим переключает `TextOverlayItem.setTransformMode`; UV сохраняются в `transform_uv` и применяются в `_apply_overlay_geometry_from_meta`.
- Экспорт: `export_overlays_with_dialog` → `_composite_to_directory` (по умолчанию) или `save_all_pages` (ручной, с oversample). Перед записью композитит клин, затем оверлеи по сцене.

## Связки и колбэки
- `StyleBinding` (`text_style.py`): `current()`/`emit()`/`subscribe()` оборачивает стейт стиля. Панели вызывают `emit`, `TextCanvasViewQt` патчит `current_style` и синхронизирует активный редактор.
- `TextOverlayItem._on_changed` вызывается при pos/scale/transform/angle, пересчитывает UV, при смене страницы обновляет `img_idx`/`w_frac`, инвалидирует маски, потом сохраняет JSON.
- Панель редактирования переподключает сигналы при каждом выделении (`update_for_overlay`), чтобы колбэки попадали в актуальный оверлей.

## Что помнить
- Размер оверлея в превью = `width_px` из meta (page width * `w_frac`); конечный PNG сохраняется отрендеренным с новой шириной и уже учтённым user_scale.
- Маски клина должны инвалидироваться при перемещении/трансформации оверлеев и пересчитываться через `_cutLinesManager`.
- Перетаскивание оверлеев троттлится: инвалидация масок/перерисовка не чаще ~20 fps, а `text_info.json` сохраняется после отпускания мыши (или по таймеру, если не идёт drag).
- При добавлении новых эффектов/полей нужно:
  1) Прописать их в `TextStyle`, to_dict/from_dict, ensure_exclusive_gradients/renderer kwargs.
  2) Обновить `TextFontPanelQt` + колбэки binding.
  3) Добавить сериализацию в `_save_text_info_json` и загрузку в `_load_overlays_from_json`.
  4) Обновить этот файл.
