# README_AGENT

## Назначение файла

Краткая архитектурная шпаргалка по актуальной версии проекта. Основная реализация — Rust-код в `src/`. Файл нужен, чтобы быстро восстановить:

- какие есть основные слои;
- где хранится и как течёт состояние;
- какие фоновые пайплайны уже существуют;
- какие контракты нельзя ломать при правках.

Это не changelog и не каталог UI-деталей.

Legacy Python UI in `qt_runner.py` and `ui_new/` is the old 2.x implementation. It has been
fully rewritten in Rust and must not be used as architecture reference, modified, or searched for
current CanvasView behavior unless the user explicitly asks to work on the legacy 2.x Python UI.
New functionality belongs in Rust under `src/`.

---

## Documentation folders: `dev-docs/` vs `docs/`

Two folders, two audiences — do not mix them up:

- **`dev-docs/`** — scratch space for development-time documents: plans, design notes,
  investigation and audit reports, changelogs, migration notes, i18n exclusion lists.
  Written by and for agents/maintainers, may be stale or half-finished. **Not tracked by git**
  (`.gitignore` is a publication allowlist and `dev-docs/` is not on it), so it never ships.
  Any new plan/report an agent produces goes here, not into the repo root and not into `docs/`.
- **`docs/`** — real, published documentation **for humans/users**, including the localized
  `README.md` translations (to come). **Tracked by git** and part of the public repository:
  everything here is user-facing and must be accurate.

Agent-facing architecture docs stay where they are (`README_AGENT.md`, per-directory
`MODULE_README.md`, `egui-docs/`); they are neither `docs/` nor `dev-docs/`.

---

## Стек технологий

- **GUI**: `eframe` / `egui` **0.35** (upstream crates.io, без форка и `[patch]`). 0.35 переименовал/удалил значительную часть API, который модели помнят по 0.27–0.31 (`App::update`, `SidePanel`/`TopBottomPanel`, `screen_rect`, `Rounding`, `id_source`, `raw_scroll_delta` — ничего из этого здесь нет). **Перед любой правкой UI: `egui-docs/README.md`**; существование API проверять грепом по `egui-docs/api/symbols.txt`, а не по памяти
- **Рендер текста**: `cosmic-text` (typing tab)
- **Изображения**: `image` crate (RGBA), `egui::ColorImage` (GPU upload)
- **IPC к Python backend**: framed IPC via `crate::backend_ipc` over a pluggable transport — AF_UNIX by default (Linux/Windows, default path from `backend_ipc::backend_socket_path()`) with a loopback WebSocket fallback on Windows; the transport-agnostic frame codec is `[u32 BE header_len][header_json][u32 BE blob_len][blob]`. Rust-side app-managed model downloads use the official `hf-hub` crate to resolve repository URLs, then stream files directly into `ManhwaStudio_AI_Models` without HF cache blobs or symlinks
- **Потоки**: `std::thread`, `tokio` (async где нужно), `rayon`
- **Сериализация**: `serde_json`, `serde`
- **Python AI backend**: a separate `ai_backend.py` process that serves the framed IPC over a pluggable transport — AF_UNIX by default (path from `backend_ipc::backend_socket_path()`), loopback WebSocket fallback on Windows. The `--socket` argument is optional and defaults to the same standard path; the Rust process manager passes it explicitly when starting the backend. Backend imports `config.VERSION` and publishes it as `backend_version` in `/health`; Rust compares it with `CARGO_PKG_VERSION` and warns when the Studio binary and backend package are out of sync. Backend включает выбор PyTorch device, ONNX provider/device и лимита одновременно резидентных AI-моделей через `/device`; отсутствующий пользовательский выбор хранится как `not-selected`, но backend сразу резолвит его в runtime default (сначала GPU, затем CPU), сообщает Rust UI о незаданном выборе, когда Torch+CUDA впервые становятся доступны, а ONNX на Windows предпочитает DirectML и просит Rust UI выбрать конкретный DirectML adapter, если их несколько. Health snapshot публикует `is_torch_available`, а Rust зеркалит это в глобальный capability-slot для UI и runtime-гейтов. Backend держит общий LRU-style `LoadedModelManager`, который считает и выгружает idle PyTorch-модели и ONNX `InferenceSession` перед загрузкой новых. Модели, управляемые кодом ManhwaStudio, лежат в `ManhwaStudio_AI_Models/Torch` и `ManhwaStudio_AI_Models/ONNX`; Rust-side calls to app-managed backend models must first pass through `src/ai_models.rs`, which lazily downloads only the required files from `Vasyanator2/ManhwaStudio_AI_Models` directly into the app model tree. PaddleOCR OCR/detector выполняются через Python ONNX Runtime и модели из `ManhwaStudio_AI_Models/ONNX/PaddleOCR`; MangaOCR OCR поддерживает два локальных ONNX-export каталога `ManhwaStudio_AI_Models/ONNX/MangaOCR/base` / `ManhwaStudio_AI_Models/ONNX/MangaOCR/2025`, а также отдельный ленивый PyTorch-вариант через пакет `manga_ocr`, который импортируется только при явном выборе этого режима. EasyOCR, Surya и PaddleOCR-VL используют model-cache своих библиотек (PaddleOCR-VL — Hugging Face hub cache, грузится через Transformers с `trust_remote_code`, только при выборе этого движка). Если Torch недоступен, torch-dependent backend endpoints отвечают ошибкой, а ONNX-маршруты продолжают работать. Compiled ONNX cache хранится в `ManhwaStudio_AI_Models/.cache`. Ограничение: при выборе `MIGraphXExecutionProvider` detection-модель принудительно запускается на CPU, а MiGraphX-специфичные width-bucket/fixed-batch обходы применяются только к recognizer.
- **Цели**: `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`

---

## Крейты workspace

Проект — cargo workspace: главный бинарник `manhwastudio_rs` (`src/`) плюс
GUI-free крейты под `crates/`. Слой логики извлечён из `src/`, чтобы правки вкладок
не пересобирали тяжёлый рендер текста и чтобы границы держал компилятор.

- **`ms-log`** (`crates/ms-log`) — `runtime_log` + `trace`. Pure std, конфиг не читает:
  каталог логов передаётся параметром. Экспортирует макросы `trace_log!` / `trace_scope!`.
- **`ms-i18n`** (`crates/ms-i18n`) — GUI-free слой локализации UI-строк. `LocaleTag`
  (ОТКРЫТЫЙ тег каталога: любой валидный `<tag>.json`) отделён от `PluralRules`
  (ЗАКРЫТЫЙ enum `En/Ru/Es/Fr/Pt` с CLDR-правилами); мост `plural_rules_for_tag`
  возвращает наблюдаемый `PluralRulesResolution::{Matched, FellBackToEnglish}`.
  Пять каталогов встроены через `include_str!` (`locales/{ru,en,es,fr,pt}.json`,
  таблица `EMBEDDED`); `en` — эталон и fallback. Активный каталог живёт за `ArcSwap`,
  строки — `Box::leak`'нутые `&'static str`, поэтому `t!` wait-free и без аллокаций
  (утечка ограничена числом переключений языка за процесс). `tf!` (именованные
  плейсхолдеры) и `tp!` (плюрали) возвращают `String`.
  **Сообщение, называющее кнопку, обязано подставлять её ключ через `{button}`**, а не
  дублировать подпись литералом: переводят их разные люди, и текст расходится.
- **`ms-text-util`** (`crates/ms-text-util`) — `text_punctuation` (набор висячей
  пунктуации) + `segmentation` + `language`. Config-free: набор по умолчанию —
  `DEFAULT_HANGING_PUNCTUATION`, приложение засевает пользовательское значение на
  старте через `set_hanging_punctuation`
  (`main.rs::seed_hanging_punctuation_from_config`). `language` держит
  `TextLanguage` (13 языков) / `ScriptGroup` (4 группы) и process-global
  `set_text_language` / `text_language`, засеваемый из
  `main.rs::seed_text_language_from_config`.
- **`ms-actions`** (`crates/ms-actions`) — чистый GUI-free движок undo/redo (Фаза 0
  единой системы действий, см. `dev-docs/unified_action_system.md`): трейт
  `ReversibleAction` (само-обратимая команда в стиле Koharu — `apply` захватывает
  `prev`) и in-memory `ActionHistory<A>` (`apply` драйвит мутацию; `record` — для
  observer-style мест, где мутация уже произошла). Без egui/tokio/image и без доменных
  типов. Первый конкретный op — `canvas/bubble_action.rs::BubbleSnapshotOp` (Фаза 1,
  пузыри); растровые diff'ы появятся в поздних фазах.
- **`ms-text-render`** (`crates/ms-text-render`) — продовый рендер текста вкладки typing
  (бывший `src/tabs/typing/render_next`). Зависит от `ms-log`, `ms-text-util`; внешне
  cosmic-text/swash/zeno/image/rayon.

Бинарник держит стабильные пути через реэкспорт-шимы: `crate::runtime_log` / `crate::trace`
/ `crate::text_punctuation` = соответствующие крейты; `crate::tabs::typing::render_next` =
`ms_text_render`, `crate::tabs::typing::segmentation` = `ms_text_util::segmentation`
(см. `src/main.rs` и `src/tabs/typing/mod.rs`). Крейты используют существующий, но ранее
незадействованный каталог `crates/` (там же лежат `ag-psd` и `puffin_egui`).

- **`puffin_egui`** (`crates/puffin_egui`) — vendored fork of upstream `puffin_egui` 0.30.0,
  ported to egui 0.35 (upstream has no egui-0.34/0.35 release). Only compiled behind the optional
  `profiling` feature; provides the in-app flamegraph window. Keep changes minimal and re-sync from
  upstream when an egui-0.35 release ships.

---

## Общая схема слоёв

```
main.rs → ProjectData → MangaApp
             ↓
    shared models (Arc<Mutex<…>>)
             ↓
  CanvasView  ←→  CanvasHooks  ←  Tab State
             ↓
  background workers / AI backend
```

1. **`src/main.rs`** — CLI (clap: `--project`, `--no-ai`, `--update`, hidden install/update/uninstall), startup mode (direct / direct update window / update continuation / installer prompt / Rust launcher), `scr→src` нормализация. Без `--project` проверяет Python env: если env найден — открывает Rust launcher из `src/launcher/`; если env не найден на Windows — сначала проверяет реестр и стандартные install-path на уже установленную копию и показывает окно действий (запуск, ярлыки, переустановка), а затем при отсутствии найденной копии предлагает установку; Windows uninstall и ручное создание Start Menu shortcut для установки в `Program Files` сначала делают elevated relaunch с hidden continuation-флагом и только затем выполняют protected-операцию; `--test-launcher` оставляет явный тестовый вход в тот же runtime.
2. **`src/config.rs`** — глобальный config/runtime-path слой: `program_dir()` и `data_dir()` привязаны к launch working directory с fallback на каталог exe; от этого корня берутся bundled ресурсы, Python env/backend, `user_config.json`, `ManhwaStudio_AI_Models`, `spell_check`, `last.log`; `General.ai_install_type` tracks the installed AI dependency level as `None`, `Base` (ONNX-only), or `Full` (Torch-capable). If the key is absent at startup, `src/ai_install_probe.rs` detects it through the same activated Python shell package probe used by launcher settings and persists the result.
   **`src/memory_manager.rs`** — safe Rust image-memory policy layer. It owns `MemoryProfile` (`Minimal`/`Low`/`Medium`/`Maximum`), pressure classification, profile budgets, and typed cache eviction requests/reports, but never owns pixels or `TextureHandle`s. Runtime cache owners expose snapshots/eviction methods and keep storage local; the editor root shares a hot-applicable `MemoryManager` handle with settings and future cache owners. `General.memory_profile` is the global persisted profile; legacy missing values are derived only from user `Canvas.cache_pages` (`false` → `Low`, `true` → `Medium`) without changing project settings.
   **`src/python_manager.rs`** — единый Rust-side Python runtime слой: обнаруживает app-local `installer_files/venv`, `venv`, `installer_files/env` и `installer_files/python`; строит UTF-8/hidden-window `Command` для Python scripts/daemons/inline probes; отдаёт shell activation snippets для launcher settings console/probe. Long-lived Python children that must not outlive the app use this module's managed spawn path; on Windows it assigns them to a kill-on-close Job Object. Любой Rust-код, который ищет Python или запускает Python-процесс, должен идти через этот модуль.
   **`src/gpu_utils.rs`** — общий системный probe-слой GPU/accelerator detection для установщика и runtime-кода: NVIDIA/AMD presence, CUDA/ROCm version, NVIDIA SM, Linux amdgpu/ROCm state, AMD LLVM target validation для ROCm 7.2 и Windows DirectML-capable adapter scan. Все вызовы выполняются через короткоживущие системные команды/FS-проверки и должны запускаться вне GUI thread.
3. **`src/project.rs`** — `ProjectData` (страницы, пузыри, пути, настройки), `ProjectPaths`, `Bubble`, `CanvasSettings`, `ComicType` (Pages/Ribbon/Custom); при загрузке проекта нормализует небольшие расхождения имён `clean_layers/*.png` относительно `src/` (например `1.png` → `001.png`) только при однозначном совпадении.
4. **`src/app.rs`** — `MangaApp`: создаёт shared-модели, запускает единый loader pool (source pages + clean overlays, interleaved по page order), хранит source-page geometry metadata separately from source GPU texture residency, инкрементальная GPU upload, dispatch глобальных хоткеев, координация AI backend health. Окно студии стартует как `StudioBootstrapApp` (`src/studio_bootstrap.rs`): фоновая `ProjectData::load*` за loading screen, затем swap-in `MangaApp` (delegates `ui`/`on_exit`).
5. **`src/launcher/*`** — новый Rust launcher runtime: `mod.rs` запускает launcher и возвращает выбранный `project_dir` обратно в startup flow, `app.rs` держит `eframe::App` и синхронно прокидывает смену `projects_root` во все launcher-страницы/окна, `background.rs` в фоне собирает пул меню-картинок и лениво декодирует/blur-ит их батчами, `main_page.rs` рисует тёмную главную страницу, `pages/base.rs` даёт slide/fade runtime и общий shell полноэкранных страниц, `pages/open_page.rs` асинхронно сканирует тайтлы/главы и валидирует выбранный проект перед открытием, `pages/import_page.rs` и `pages/export_page.rs` держат worker-driven `.mschapter` import/export flow без блокировки UI, `pages/settings_page.rs` рендерит общий виджет `crate::general_settings_panel` (projects_dir + memory profile, тот же, что в studio-настройках) и возвращает смену корня как `PageNavAction::ProjectsRootChanged`, в фоне проверяет системную информацию через `gpu_utils`, PyTorch/ONNX Runtime через тот же shell path, что и консоль: активирует app-local env, если он найден, иначе использует inherited `PATH`, и держит фоновую shell-консоль для этого окружения; если в активном env нет команды `pip`, консоль локально перенаправляет её на `python -m pip` или `uv pip`, `psd_import_window.rs` держит отдельное native egui-окно импорта простых PSD через Rust crate `psd`, хранит загруженные документы в памяти и предупреждает о группах/сложных PSD, `new_project/window.rs` отвечает за UI отдельного окна/viewport "Новый проект" и только поллит фоновые контроллеры, держит внутреннее egui-окно неразрушающей обрезки текущего ribbon-page с прокруткой и draggable crop-рамкой, `new_project/open_source.rs` держит picker + worker-пайплайн импорта папки/файла (saved HTML, image folders, archives, single images) и стримит staged progress обратно в UI, `new_project/project_io.rs` держит фоновые сканы projects root и save/export pipeline в `src` / `alt_vers` / arbitrary folder, `new_project/quick_download.rs` держит site-specific quick downloader для поддерживаемых chapter URL и стримит прогресс загрузки обратно в UI, `new_project/advanced_download.rs` owns the Python browser-daemon bridge for stateful advanced download; Selenium uses `adv_fetch_cli.py`, CloakBrowser uses `adv_fetch_cloak_cli.py`, and both helpers keep one JSON protocol, persistent profiles under `modules/browser_profiles`, startup `downloader_version` checking, pattern/auto link fetch, background link collection, canvas snapshot, and canvas intercept transfer back to the ribbon, `new_project/stitching.rs` держит worker-пайплайн склейки/автонарезки/ручной нарезки, стримит staged progress и переносит эвристику старого Python stitch/split, `new_project/waifu2x.rs` держит worker-пайплайн waifu2x поверх лениво загружаемой dll/so, при отсутствии runtime скачивает и распаковывает платформенный архив из GitHub release в фоне с прогрессом UI, кэширует runtime+модель на время жизни окна "Новый проект" и выгружает их при закрытии окна, `new_project/reline.rs` writes ribbon pages to temporary PNG files and calls the already running Python AI backend `/reline/process`, `new_project/ribbon.rs` хранит state текущей и исходной ленты, non-destructive original image + crop metadata для каждой страницы и tiled preview без downscale, `state.rs` хранит UI state окна и page enum, `theme.rs` задаёт стиль, `new_project/batch_processing/` — визуальный нод-редактор массовой обработки: `types.rs` (DataType/SocketKind/DataValue/NodeParams typed enum), `graph.rs` (GraphModel + JSON сериализация, совместимая с Python version=1), `node_defs.rs` (13 шаблонов нод + динамические сокеты для string_template/variable_*), `canvas.rs` (egui Painter pan/zoom/drag без retained items), `executor.rs` (worker-thread BatchExecutor с exec-queue join-wait, BrowserDaemon stdio JSON-RPC к adv_fetch_cli.py), `window.rs` (BatchProcessingWindowState: viewport + palette + variables + param_editor popup).
6. **`src/canvas/*`** — общий canvas-движок (сцена, пузыри, overlay, zoom, input).
7. **`src/models/*`** — shared state между вкладками.
8. **`src/tabs/*`** — логика конкретных вкладок; canvas расширяется через `CanvasHooks`.
   Исключение — `src/tabs/ps_editor/` (вкладка «PS-подобный редактор»): самостоятельный
   одностраничный слоёвый редактор, который **не** является `CanvasView`. У него собственная
   камера pan/zoom (`viewport.rs`), стек слоёв с двумя заблокированными базовыми слоями (Исходник —
   из `CleanOverlaysModel::cached_page_rgba`, Клин — из `overlay_rgba`, оба read-only, можно скрыть,
   нельзя удалить), пользовательские raster-слои поверх них (session-scoped в памяти, per-page; без
   дискового сохранения в этой фазе), набор инструментов через trait `PsTool` (прямоугольное/лассо
   выделение, цветная кисть), фоновый `page_loader` и потайловый GPU-кэш `TiledTexture`. Базовые
   слои никогда не пишутся обратно в `CleanOverlaysModel`. Вкладка исключена из shared-canvas
   viewport sync и source-page window в `app.rs` (`active_tab_is_canvas` для неё false).
   Undo/redo (фаза 3a, только мазки кисти) — через `ms-actions` `RasterDiff`: `edit_op::PsEditOp`
   (`ReversibleAction<Ctx = PsEditorTabState>`) хранится в per-page `ActionHistory`, очищаемой при
   смене страницы; Ctrl/Cmd+Z / Ctrl+Shift+Z(Ctrl+Y).
   Во вкладке `typing` продовый рендер идёт через `render_next::render_text_to_image(&params, &dyn FontProvider, cancel)`: шрифты доходят до рендерера ПО ИМЕНИ (`TextRenderParams.font_name` + inline `<font=...>`), которое резолвится через `render_next::FontProvider`, а не через путь. Вкладка `typing` владеет загрузкой шрифтов и строит провайдер (`panel::TabFontProvider`, ключ — нормализованный label, ленивое чтение байт + кэш content-id), хранит его на панелях и в слое таба и передаёт `Arc<dyn FontProvider>` в каждый фоновый render-запрос. `FontEntry.original_name` (реальное имя семейства) сохраняется как `font_original_name` и предпочитается PSD-экспортом. Path-based `render_next::load_selected_font_from_path` остаётся тонкой compat-обёрткой для forms-metric пути. `render_next/` держит публичные типы в `types.rs`, изолированные horizontal и vertical `wrap`/`layout` path'ы, `inline_styles` (tag parsing/remap + attrs-level rich-text + glyph-level color/kerning/stretch/offset/line-spacing overrides; parameterized `<b=...>`/`<i=...>` request FAUX bold/italic — geometric outline thickening/baseline shear on the Regular face via `TextRenderParams.faux_bold`/`faux_italic_slant_deg`, active only together with `force_bold`/`force_italic`; bare `<b>`/`<i>` keep the real faces — see `crates/ms-text-render/src/MODULE_README.md`), `font_registry` для selected/inline fonts, общий `raster`-слой для swash/RGBA helper'ов и вынесенный `effects/` пакет с отдельным JSON parser'ом, text preprocess stage до inline parsing (`effect_type=preprocess`), image helper-слоем и split-модулями `stroke_shadow` / `blur` / `glow` / `gradients` / `reflect_shake` / `dry_media` / `interference`; отсутствие `effect_type` в effects JSON означает post-effect для совместимости.
   `TextRenderParams.compare_shape_with` — opt-in pre-raster сравнение `layout_text` с другим набором shape/wrap-параметров (`width_px`, `text_wrap_mode`, `shape_min_width_percent`); при совпадении формы рендер может быть пропущен до растра или продолжен с предупреждением.

---

## Точка входа

`src/main.rs`:
- запуск с `--project PATH` → прямой старт без launcher;
- запуск с `--update` → прямое открытие Rust update window, которое повторно проверяет релиз, скачивает платформенный `manhwastudio_rs(.exe)`, заменяет executable и запускает `--continue-update`;
- запуск с hidden `--continue-update` → продолжает обновление: uv-managed venv, optional PyTorch refresh for Full installs, missing embedded Python dependencies, then `ManhwaStudio.zip` extraction;
- existing-install and custom-folder update actions query the target executable with `--version`, compare GitHub releases, replace that target binary, then launch it with `--continue-update`;
- `--test-ver-check` forces an available update in both the launcher notice and the direct update window;
- запуск с `--test-launcher` → тестовый вход в Rust launcher без открытия проекта;
- без `--project`:
  - Python env найден → Rust launcher;
  - Python env не найден → prompt на установку, при отказе всё равно Rust launcher;
- `scr → src` нормализуется для legacy-проектов;
- `--no-ai` отключает Python backend и PaddleOCR.

---

## Что такое проект

`src/project.rs`:

- **`ProjectData`** — корень главы: список `Page` (idx, path), список `Bubble`, `ProjectPaths`, `CanvasSettings`, `ComicType`.
- **`ProjectPaths`** — нормализованные пути к `src/`, `clean_layers/`, `cleaned/`, `text_detection/`, `characters.json`, `terms.json`, `notes_file`, `bubbles.json`, `settings.json`, `wiki/`, `fonts/`, `alt_vers/`, `text_images/`, `image_bubbles/`, and unsaved staging mirrors.
  At load time `load_bubbles` migrates the very old bubble format (absolute Tkinter ribbon `x`/`y`, no `img_u`/`img_v`): `LegacyRibbonGeometry` recovers the shared continuous-ribbon scale/offset from all bubbles plus page sizes, converts each to `img_u`/`img_v`, and `persist_migrated_bubbles` rewrites the file once (original backed up to `*_legacy_xy.json`).
- **`Bubble`** — `id`, `img_idx`, `img_u`, `img_v`, `side` (Left/Right), `bubble_class` (`text`/`image`, legacy missing means `text`), text-bubble `bubble_type` (Default/Aside/OnTop), `text`, `original_text`, `extra: Map<String,Value>`. `ImageBubble` metadata lives in `extra`: `image_source_type` (`external`/`page_crop`), optional `image_path` under `image_bubbles/` (resolved from unsaved staging first, then the saved chapter folder), optional `crop_page_idx` + `crop_rect`, and `description`. An `ImageBubble` is a *group* of text areas: `extra["text_areas"]` is an array of `{rect:[x1,y1,x2,y2], anchor:[u,v], original, description, translation}` (image-normalized geometry; every area — including area 0 — is an independent sub-box clamped inside the red image area `rect_coords`, each `anchor` inside its own `rect`). The red `rect_coords` is the single resizable image-area rectangle (owned and drawn by the canvas), not a text area. For page-crop bubbles it equals the crop region: `crop_rect` is kept synced to `rect_coords` on save and `image_area_rect_from_bubble` resolves the crop region as the image-area rect, so there is exactly one red rectangle (the translation tab no longer draws a separate crop overlay). In external-image mode the same red rect is shown but does not crop. Area colors follow a reverse rainbow from blue (`image_area_palette`). Area 0 keeps its text in the legacy `text`/`original_text`/`extra.description` fields (canonical primary, so MT/OCR/status keep one source); the array stores geometry for all areas plus text for areas ≥1. The bubble side is the sign of `Σ(anchor_u − 0.5)`. Read-only, an ImageBubble splits into one ordinary aside bubble per area.
- **`ComicType`** — `Pages` / `Ribbon` / `Custom`; маппится в `aside_compact_mode` + `separate_pages`.
- **`CanvasSettings`** — поля canvas (типы пузырей, compact/fixed side для aside, spacing, spellcheck, autosync/cache); часть переопределяется из `user_config.json`.

---

## Корневой runtime

`src/app.rs` — `MangaApp` (`eframe::App`):

- создаёт `BubblesModel`, `CleanOverlaysModel`, `TextMaskModel` (все `Arc<Mutex<…>>`);
- `spawn_loader_thread`: unified multi-worker decode pool for source pages AND clean overlays (interleaved in page order; overlays bypass ordered promotion) → ordered page promotion → incremental GPU upload (budget per frame);
- `upload_textures_incremental`: tiled GPU upload с per-frame budget;
- `poll_loader_events` + `promote_decoded_pages_in_order`: строгое сохранение порядка страниц;
- `dispatch_hotkeys` + `execute_hotkey_command`: глобальная маршрутизация хоткеев;
- AI backend health probe (`AiBackendHealthSnapshot`) поднимается один раз и шарится в Translation и Settings.

### Hotkey система

- **`src/input_manager_v2.rs`** (current): `InputManagerV2` — поддерживает `KeyboardShortcut` и `ModifierOnly` (Ctrl/Alt/Shift) бинды; overrides из `user_config.json → Hotkeys`; `save_hotkey_override` / `clear_hotkey_override`. `KeyboardShortcut` команды срабатывают строго по фронту нажатия (`collect_triggered` запоминает held-состояние per command): удержание не повторяет команду, для следующего срабатывания нужно отпустить и нажать клавишу заново. Перехват `Q`/`Shift+Q` (ImageBubble) живёт в Translation tab с отдельным armed-гейтом, чтобы `Q`, оставшийся зажатым после `Shift+Q`, не создавал лишний пузырь.

---

## Shared-модели

### `BubblesModel` (`src/models/bubbles_model.rs`)

- хранит `Vec<Bubble>`, `revision`, `canvas_revision`, `SharedCanvasSettings`;
- `touch_and_save()` инкрементирует revision и отправляет в `saver_thread`;
- `saver_thread` (`spawn_bubbles_saver_thread`) коалесцирует — пишет последний snapshot;
- `SharedCanvasSettings` (~20 полей) — cross-tab настройки canvas, сериализуется в `settings.json` и `user_config.json`.

### `CleanOverlaysModel` (`src/models/clean_overlays_model.rs`)

- двойное CPU-хранилище: `egui::ColorImage` (UI/canvas) + `Arc<image::RgbaImage>` (export/save/tools);
- `OverlayDelta` — инкрементальные уведомления об изменениях;
- undo/redo в Cleaning — движок `ms-actions` (`history: ActionHistory<CleanOverlayDiffOp>`, Фаза 2b):
  каждый commit — тайловый zstd-сжатый обратимый straight-RGBA `RasterDiff`; ограничен счётчиком (128)
  И побайтовым бюджетом на профиль памяти (`MemoryBudget::clean_overlay_undo_bytes`); новый commit после
  undo отрезает redo-ветку; `apply_raster_diff` синхронно обновляет ОБА представления (RGBA-кэш →
  ColorImage через `from_rgba_unmultiplied`); full-page-конструкция дельт (`apply_overlay_snapshot`)
  пока синхронна на потоке вызова (перенос на воркер — Фаза 2c);
- `replace_region()` / `replace_prepared_overlay()` — частичный blit + model sync;
- `take_delta()` — drain dirty indexes для подписчиков;
- `save_all()` — запись `clean_layers/` на диск;
- держит page-cache (декодированные `RgbaImage` страниц для инструментов).

### `TextMaskModel` (`src/models/text_mask_model.rs`)

- `HashMap<usize, TextMaskPage>` (page_idx → mask_alpha: Vec<u8>);
- `revision` — отслеживание изменений;
- детектор пишет маски → Cleaning читает их для quick-clean;
- `edit_page_mask()` — closure-based in-place редактирование.

---

## Canvas

`src/canvas/` разбит на модули:

| Файл | Ответственность |
|---|---|
| `mod.rs` | `CanvasView` фасад, константы, `CanvasHooks` trait |
| `scene.rs` | `CanvasSceneState`, scroll/zoom, page strip layout, hit-testing |
| `bubble_runtime.rs` | `BubbleRuntimeState`: pending upsert/delete, undo/redo history, debounced flush |
| `overlay_runtime.rs` | `OverlayRuntimeState`: tiled overlay upload, dirty tracking, prepare pipeline |
| `settings.rs` | `CanvasSettingsRuntime`: revision sync, background saver |
| `workers.rs` | `spawn_overlay_prepare_thread`, `spawn_canvas_settings_saver_thread` |
| `helpers.rs` | stateless helpers: bubble fingerprint, blit, text measure, clipboard sanitize |
| `bubble_aside_ui.rs` | aside bubble columns (Full/CompactDual/CompactSingle), drag lifecycle, fixed/auto side layout |
| `bubble_on_top_ui.rs` | on-top bubble widgets: move handle, 8-point resize, focus |
| `types.rs` | `CanvasState`, `RuntimeBubble`, `BubbleAction`, `BubbleType`, `BubbleMode`, etc. |

**Canvas geometry**: страницы раскладываются в стабильных world-координатах (unscaled source size + fixed content width), а screen `image_rect` получается только через camera-like `zoom`; directed zoom всегда якорится на world-точку под курсором/в центре viewport и корректирует оба scroll offset-а без ветвления "курсор над страницей / вне страницы".
Source page dimensions come from `PageImageInfo`; source page `PageTexture` handles are cache residency and may be dropped. NEAREST source textures are lazy and scoped to high-zoom pixel inspection near the active page window.
**Tiling**: `OVERLAY_TILE_SIDE` — размер тайла overlay; `TEXT_DETECTOR_MASK_TILE_SIDE` — тайл маски детектора.
**Debounce**: `TEXT_UPSERT_DEBOUNCE_SECS` — задержка flush bubble upsert → model.
**Bottom-hint (keyboard-shortcut подсказка)**: content опционален и per-tab. Строки теперь ФИКСИРОВАННЫЕ, hand-authored и локализованные (`app.rs::build_translation_hint_rows` / `build_typing_hint_rows`, ключи `canvas.bottom_hint.{translation,typing}.*` через `t!`), НЕ из hotkey-реестра; они строятся каждый кадр, так что смена языка в рантайме их ре-локализует. Bottom-hint есть только у Translation и Typing; у Cleaning он отключён (canvas `bottom_hint` остаётся `None`). Свёрнутое состояние живёт per-tab в `user_config.json → Canvas.{translation,typing}_hint_collapsed` (default `false` = развёрнуто): засевается один раз в `MangaApp::new` и пишется ОДИН раз в `MangaApp::on_exit` (единственная точка сохранения — срабатывает и на закрытие, и на возврат в лаунчер), а не на каждый тумблер. Устаревший `cleaning_hint_collapsed` может остаться в старом конфиге — он не читается и не пишется (без миграции).
**Undo**: пузыри используют движок `ms-actions` (`bubble_runtime.rs::bubble_history: ActionHistory<BubbleSnapshotOp>`, op в `canvas/bubble_action.rs`, Фаза 1) — behavior-preserving full-snapshot op (`Arc<Vec<Bubble>>` before/after, откат через `BubblesModel::reset`), observer-style: мутация идёт напрямую в модель, история записывается после через `capture_bubble_history_before_mutation` → staged `pending_history_before` → `finalize_pending_history`, дедуп по revision. `BUBBLE_HISTORY_LIMIT` — лимит истории.
**Spellcheck**: bubble text использует `SpellcheckedTextEdit` из `src/widgets/`; worker грузит Hunspell-совместимые словари из app-local `spell_check/`. Активный словарь следует за ЯЗЫКОМ ВЁРСТКИ (`ms_text_util::language::text_language`, как переносы и покрытие шрифта), НЕ за языком интерфейса: worker сравнивает `text_language()` каждый батч и докачивает словарь активного языка по таблице `dictionary_spec` (LibreOffice, кроме `fr`/`pl`/`sl` из wooorm — у LibreOffice нет `fr_FR`, а `pl`/`sl` там в `ISO8859-2`), по одному разу на язык (`HashSet<TextLanguage>`, неудача одного языка не блокирует другой). Сопоставление по СЛОВУ — language-first, script-second: слово в письменности активного языка судит только словарь этого языка (оставшийся на диске словарь другого языка той же письменности не голосует), слово другой письменности судит любой словарь этой письменности (смешанный текст); при отсутствии словаря активного языка слова остаются не подчёркнутыми. Кэш ключуется языком и полностью чистится при смене набора словарей, чтобы вердикт не пережил переключение. Читает/пишет пользовательские слова через `custom.aff` + `custom.dic` в той же папке, дополнительно подмешивает project-local исключения из `settings.json → canvas.project_custom_spellcheck_words` и кэширует per-word результаты; глобальные флаги живут в `SharedCanvasSettings`, локальные overrides строки — в `Bubble.extra`.

### Контракт `CanvasHooks`

Вкладка расширяет canvas через `CanvasHooks`, не форкая canvas-логику:

| Метод | Назначение |
|---|---|
| `draw_canvas_mask_overlay_on_page` | маска-оверлей (жёлтый) поверх страницы |
| `draw_canvas_overlay_on_page` | дополнительные элементы поверх страницы |
| `draw_canvas_overlay_top_left` | UI overlay в левом верхнем углу (панели, кнопки, polling) |
| `build_bubble_header` | header UI пузыря |
| `build_bubble_footer` | footer UI пузыря (персонаж, тип, язык) |
| `on_bubble_action` | реакция на события пузыря |
| `draw_canvas_page_context_menu` | контекстное меню страницы |
| `suppress_canvas_page_context_menu` | подавление стандартного контекстного меню |
| `wants_canvas_shift_drag_selection` | перехват Shift+drag (OCR selection) |
| `should_hide_on_top_bubble` | скрыть on-top bubble (например, под курсором кисти) |
| `has_bubble_header` | показывать ли header пузыря |
| `bubble_status_style` | стиль рамки пузыря по статусу |

---

## Bubble status (`src/bubble_status.rs`)

- **`BubbleStatusCondition`** — рекурсивное логическое выражение: `Empty` / `Field(BubbleStatusField)` / `All` / `Any` / `Not`.
- **`BubbleStatusField`** — `TranslationFilled` / `OriginalFilled` / `CharacterFilled`.
- **`BubbleBorderKind`** — `Solid` / `Dashed` / `Dotted` / `Wavy`.
- `evaluate_bubble_status_rules()` — first-match-wins по списку правил.
- `paint_bubble_status_border()` — рисует стиль рамки на Rect.
- Правила сохраняются в `SharedCanvasSettings`; редактируются в Settings → Лента.

---

## Вкладки

### Page Manager (`src/tabs/page_manager/`) + `src/page_ops/`

- **Page Manager tab** — first tab in `AppTab::ALL` (default active tab stays Translation).
  NOT a CanvasView and not a view tab (excluded from viewport sync): virtualized card grid
  (background thumbnails, clean/bubble/layer badges), multi-select, dialogs for inserting
  image files, creating a blank page (size defaults from the neighbor page, solid fill color)
  and deletion. `draw` returns `PageManagerAction` (`RequestOp(PageOpKind)` / `OpenPageIn`),
  executed by `app.rs`.
- **`src/page_ops/`** — GUI-free engine for STRUCTURAL page operations (Move / InsertFiles /
  CreateBlank / Delete). Pages are position-keyed everywhere, so an operation is a journaled
  crash-safe transaction that consistently remaps ALL page-keyed artifacts in BOTH trees
  (committed chapter dir and `_unsaved` staging): `src/{idx:03}`, `clean_layers/`,
  `layers/layers.json` + `ps_p{idx:04}_*` PNGs, `translation_bubbles.json` (`img_idx`,
  `crop_page_idx`), legacy `text_info.json` + `mask_page_{idx}.png`, `text_detection/`.
  Two-slot A/B journal (`page_ops_journal*.json`, durable via fsync), phase A = reversible
  renames to temp names, phase B = roll-forward only; deleted artifacts go to
  `{chapter}/.pageop_trash/{id}/`, never destroyed. `recover_pending_page_op` runs at the very
  start of `ProjectData::load_internal`. `alt_vers/` is intentionally NOT remapped
  (position-matched by sorted filename, no reliable per-file key — see its MODULE_README).
- **Runtime contract (`app.rs::start_page_op`)**: structural ops are NOT staged — they apply
  immediately to both trees and are not undone by discarding unsaved changes. Before the
  transaction the app quiesces every chapter writer (flush PS/typing layers + pending bubble
  upserts, layer-saver barrier, bubble-saver write-gate + synchronous staging snapshot,
  overlay-autosave shutdown+join — all inside the worker thread); save/export/page-op gate
  each other, including the completion frame (`pending_project_reload`). After success the
  whole app is rebuilt through `StudioBootstrapApp` (project reload behind the loading screen,
  user settings re-read from disk, active tab restored to Page Manager); on failure a modal
  error dialog offers reload (journal recovery finishes or rolls back the transaction).

### Translation (`src/tabs/translation/`)

**Submodules:**

| Модуль | Роль |
|---|---|
| `tab.rs` | `TranslationTabState`: главный оркестратор, реализует `CanvasHooks` |
| `panels/` | UI боковых панелей (OCR, Bubbles, MT, TextDetector, Composition) |
| `ocr.rs` | `TranslationOcrController`: state machine (NotLoaded/Loading/Ready/Error), 4 engine |
| `text_detector.rs` | `TranslationTextDetectorController`: Classic/PaddleOcr/AiCtd режимы |
| `machine_translation.rs` | `TranslationMtController`: per-run thread с AtomicBool cancel |
| `adv_rec.rs` | `AdvancedRecognitionWindow`: floating window с crop loader, paint overlay, zoom |
| `backend_health.rs` | `AiBackendHealthSnapshot`, `spawn_ai_backend_probe` (poll every 2s) |
| `machine_translators/` | Google / Yandex / DeepL backends (trait `MachineTranslatorBackend`) |

**OCR engines**: MangaOCR, EasyOCR, PaddleOCR (Python ONNX backend), PaddleOCR-VL (Python PyTorch/Transformers backend, vision-language OCR with no text detection and no language selection, `trust_remote_code` model from the Hugging Face cache; optional hard writing-system restriction — korean/chinese/japanese — via a stateful UTF-8 `prefix_allowed_tokens_fn` to curb hallucination on messy text), Surya OCR (Python backend, task-based recognition without boxes / block mode / with internal detection), and AI API OCR through Rust `genai` multimodal chat calls. AI API OCR stores provider API keys only in the OS credential store (Windows Credential Manager / Linux keyring); project settings may persist only non-secret provider choice, model id, and system instruction.

**Text detector modes**:
- **Classic**: Otsu + binary dilation + connected components (area/density/aspect фильтры).
- **PaddleOcr**: POST `/textdetector/paddle/detect` к Python backend; Paddle det-модель возвращает блоки и `mask_png_base64`.
- **AiCtd**: POST `/textdetector/ctd/detect` к Python backend, парсинг `mask_png_base64`; endpoint принимает либо `page_path`, либо `image_base64` PNG, поэтому тот же CTD-маршрут используется и translation-детектором, и region-mask генерацией в cleaning tools.
- **Surya**: POST `/textdetector/surya/detect` к Python backend; отдельный Surya detection-only runtime лениво грузит/скачивает detector checkpoint, строит бинарную маску из low-level heatmap и возвращает line-like blocks без OCR-обёртки.

**MT**: каждый run в своём thread; `Arc<AtomicBool>` cancel; detached threads reaped lazily.
AI API MT uses Rust `genai` from the MT worker thread, reuses the same OS credential-store API key
contract as AI API OCR, sends grouped JSON batches with bubble IDs/characters, keeps text-only
conversation history across batches, can include the current bubble translation as optional
context, can attach ImageBubble images for multimodal models only in the active API request so the
model returns original text plus translation, reports context usage/pruned replica progress, and
prunes old chat turns when the configured context-fill budget is exceeded. A multi-area ImageBubble
is one MT item (`MtImageInput.areas`): the request lists each text area (description, current
original, and a bbox relative to the sent image) and the model returns
`{id, areas:[{original_text, translation}, ...]}`; the result is applied per area via
`CanvasView::apply_machine_translation_areas`. Single-area image bubbles keep the
`{id, original_text, translation}` shape.

**OCR selection modes**: Shift (быстрый) → immediate OCR; Alt (продвинутый) → `adv_rec` floating window с crop в фоне.

**BackendHttpClient**: persistent keep-alive `backend_ipc::BackendStream` (pluggable transport: AF_UNIX default / loopback-WS fallback on Windows), retry при disconnect.
**PageImageCache**: LRU max 8 items / 256 MB для декодированных страниц.

**Characters footer sync**: TranslationTabState мониторит `characters.json` (mtime watch), кэширует имена персонажей, применяет дефолты для новых пузырей, дебаунсирует flush footer patches в CanvasView.

### Cleaning (`src/tabs/cleaning/`)

- `CleaningTabState`: холст + набор инструментов + text mask overlay.
- **Инструменты** (`tools/`):
  - **`ZamazkaTool`**: кисть/ластик/пипетка/прямоугольник; scratch-буфер, commit на stroke_end; eyedropper грузит base-page в фоне.
  - **`StampTool`**: копирование из `alt_vers/`; brush/rect/erase; lazy alt-page decode.
  - **`GradientFillTool`**: RGB→Lab, scanline-angle gradient fill, screened-poisson L согласование.
  - **`TextureSynthesisInpaintTool`**: texture synthesis (Rust, локально).
  - **`SdxlInpaintTool`**: POST `/inpaint/sdxl` к Python backend; два режима — 9-канальная inpaint-модель (полный denoise) и обычная 4-канальная SDXL с префиллом LaMa (модель из `lama.rs`). Параметры обоих режимов хранятся в отдельном `sdxl_inpaint_settings.json`.
  - **`LamaInpaintTool`**: POST `/inpaint/lama_v2` к Python backend.
  - **`LamaMpeInpaintTool`**: POST `/inpaint/lama_v2` с MPE (multi-pass expansion).
  - **`RegionMaskInpaintToolBase`**: общий region-editor для inpaint-инструментов; генерация маски внутри окна может ходить либо в ComicTextDetector (`/textdetector/ctd/detect`), либо в PaddleOCR (`/textdetector/paddle/detect`) через shared helpers из translation text detector.
  - **`AotInpaintTool`**: POST `/inpaint/aot` к Python backend.
- **`CleaningTool` trait** (`base.rs`): контракт инструмента (UI, stroke, hotkeys, overlay windows, cursor).
- **`BrushToolBase`**: scratch-буфер + tiled dirty-tile preview без giant texture.
- **`RegionEditToolBase`**: выделение региона на canvas, фоновая загрузка composite (base+overlay из `CleanOverlaysModel`), floating region editor window с zoom/scroll, вставка результата назад в overlay.
- **`RegionMaskInpaintToolBase`**: mask overlay (жёлтый) + `MaskBrush`, кнопки Обработать/Переделать/Вернуть/Отмена/Применить; run выполняется в worker-thread.
- Quick text clean: многопоточная обработка страниц по `TextMaskModel` → patch в `CleanOverlaysModel`.
- Zoom CanvasView блокируется пока открыт region editor или активен Ctrl+ЛКМ режим.

### Typing (`src/tabs/typing/`)

- `TypingTabState`: read-only canvas, text/image overlays, inline text editor, export.
- **Text overlay**: PNG-файлы в `text_images/`; для custom raster line layout рядом может лежать `*_layout.png`; placement хранит `deform_mesh` (high-res surface в UV); `text_info.json` — метаданные. При загрузке `migrate_legacy_text_overlays` приводит старые форматы placement к современному center-anchor: абсолютные ленточные `x`/`y` (без `img_idx`/`u`/`v`) конвертируются через `project::LegacyRibbonGeometry`, top-left `u`/`v` сдвигаются в центр; нормализованный `text_info.json` перезаписывается один раз.
- **Render** (`render_next/`): `types.rs` хранит `TextRenderParams`/`RenderedTextImage` и связанные enum'ы; `pipeline.rs` рендерит `cosmic-text` → RGBA, поддерживает shape (Free/Rect/Oval/Hex), line mode (Horizontal/Vertical), formula layout (x/y/rotation по параметрической кривой), custom raster/vector line layouts через общий `drawn_lines.rs` path-normalizer, effects pipeline (stroke/shadow/blur/glow/gradient/reflect/shake/interference) и hyphenation через `hyphenation` crate; inline `offset` хранится как расширенная span-модель с глобальным смещением, path-смещением и rotation-overrides. Отрисовка глифов теперь vector-first: монохромные глифы растеризуются из векторных outlines через `vector.rs` (zeno coverage-mask) с общим pivot-хелпером `glyph_blit.rs` на всех трёх путях (horizontal, vertical, on-path/formula); shaping/layout/font-matching остаются на `cosmic-text`, а bitmap (`SwashCache::get_image`) сохранён только для цветных/emoji глифов, placement/bounds-бокса и измерения ink. Детали и отложённый Phase 4 — в `render_next/VECTOR_ENGINE_REFACTOR.md`. `render_text_to_image` берёт `FontSystem` из process-global пула (`font_system_pool.rs`, `with_leased_font_system`) вместо создания нового на каждый рендер (создание запускало полный системный скан шрифтов); загрузка шрифтов дедуплицируется через `FontFaceCache`, поэтому переиспользование системы даёт byte-identical результат. Прогрев из фонового потока — `render_next::prewarm_font_system_pool()`.
- Shape-variant preview рендерит 3x3 варианты формы из фонового job через отдельные worker-потоки для каждой плитки; GUI получает только собранные RGBA-результаты.
- **Panel** (`panel.rs`): вертикальная основная панель с вкладками `Параметры`/`Эффекты`, отдельная панель `Действия` и preview-окно для Create; режимы Create/Edit; именованные пресеты шрифтов + формульные пресеты; live-render при редактировании (latest-wins cancellation).
- Layout-editor mode для выделенного text overlay — runtime-состояние `TypingTextOverlayLayer`: в edit-режиме скрывает сам оверлей, рисует resizable frame и панели линий; vector-preview записывает `custom_vector_lines` в `render_data.text_params.vector_lines_layout` и запускает обычный фоновый edit-render; размер vector-области хранится в `vector_lines_layout.width_px/height_px` отдельно от фактического alpha-bounds PNG, а строки несут свои параметры сглаживания, направления текста, режима расстояния и переворота glyph'ов.
- **Auto-typing** (`auto_typing.rs`): оптический центр оверлея → region-growing bubble detection на composited-странице из `CleanOverlaysModel` cache → центрирование оверлея по найденному пузырю.
- **Mask** (`mask.rs`): бинарная маска обрезки (`mask_page_{idx}.png`); кисть draw/erase; clipping текстовых PNG.
- Деформация: Perspective / Изгиб / кистевые warp (Выпуклость, Впуклость, Сдвиг, Закрутка, Восстановление, Разгладить, Растянуть, Складка) — все модифицируют одну `deform_mesh`, не хранят отдельные параметры.
- Legacy `transform_uv` и low-res mesh конвертируются в dense surface при загрузке.
- Ctrl+колесо над выделенным text-оверлеем поворачивает его; режим выбирается app-wide через `crate::tabs::typing::rotation_ctrl_wheel` (пишется из Settings «Тайп», читается в Ctrl+wheel-хендлере typing): `Raster` — placement-поворот растра (`angle_deg`/deform mesh, legacy), `Vector` (default) — поворот render-параметра `global_rotation_deg` с фоновым ре-рендером (резче), с graceful fallback на raster для не-vector оверлеев.
- Стандартные параметры карточек эффектов: `panel/effect_defaults.rs` держит runtime-global store (`TextTab.effect_defaults`, ключ — дискриминатор эффекта → одно-карточный JSON штатного codec `effects_value_array`/`parse_effect_cards`), seeded на старте (`main.rs`). При добавлении карточки `create_sections.rs` берёт override, иначе встроенный `default_effect_card`. Редактор (`EffectDefaultsEditorState`) переиспользует `draw_effect_card_controls` и рендерится из Settings «Тайп» (double-interface, модель эффектов не экспортируется наружу).
- Экспорт: фоновое наложение `src + clean overlay + text overlays` с перспективной трансформацией и маской.
- `text_info.json` сохраняется отложенно через worker-поток после снятия выделения.

### Characters (`src/tabs/characters.rs`)

- CRUD справочника персонажей (`characters.json`).
- `CharacterEntry`: name, description, groups (String или Vec<String> в wire-формате).
- Thumbnail 192px — decode в background thread.
- `CharactersTabAction::CharactersChanged` → Translation перечитывает имена.

### Terms (`src/tabs/terms.rs`)

- CRUD справочника терминов (`terms.json`).
- `TermEntry`: name, orig_name, description, tags.
- Поддерживает legacy wire-формат tags (string или array).

### Notes (`src/tabs/notes.rs`)

- Два sub-tab: «Собранный промпт» (template + `{charas}/{terms}`) и «Шаблон» (редактор `notes_file`).
- Prompt assembly в background worker thread; `characters.json` + `terms.json` читаются через `load_characters_for_notes` / `load_terms_for_notes`.
- mtime-watch с интервалом 600ms для автообновления.

### Wiki (`src/tabs/wiki.rs`)

- Localized wiki tree: `wiki/<lang>/*.md` (one folder per UI language) plus a single shared
  `wiki/images/` tree that the pages link as `../images/...` (screenshots are NOT duplicated
  per language). The folder is selected from the ACTIVE UI locale's language subtag:
  `wiki/<lang>` -> `wiki/en` -> `wiki/` itself; switching the interface language re-scans.
  Both the studio Wiki tab and the launcher guide window use the same `WikiTabState`.
- `WikiBlock`: headings, paragraphs, lists, images, code; inline-сегменты (bold/code/plain).
- `spawn_scan_thread` / `spawn_document_load_thread` / `spawn_image_load_thread` — все фоновые.
- Image cache: pending/ready/failed состояния.

### Settings (`src/tabs/settings/`)

Вкладка имеет пять pane:

| Pane | Ответственность |
|---|---|
| **General** | projects_dir + memory profile через общий виджет `crate::general_settings_panel` (тот же, что в лаунчере; синхронная запись под `config::lock_user_config_write()`); enforcement вертикального typing-panel layout остаётся studio-only |
| **CanvasRibbon** | SharedCanvasSettings (лента), ComicType, BubbleStatus rules |
| **Typesetting** («Тайп») | text-typesetting options: висящая пунктуация (`TextTab.hanging_punctuation`, live через `crate::text_punctuation`), режим «Поворот Ctrl+колесо» (`TextTab.rotation_ctrl_wheel_mode`, live через `crate::tabs::typing::rotation_ctrl_wheel`) и редактор стандартных параметров карточек эффектов (`TextTab.effect_defaults`, self-contained typing-panel widget `EffectDefaultsEditorState`) |
| **AiBackend** | запуск/остановка `ai_backend.py`, device, CUDA/ONNX diagnostics |
| **Hotkeys** | редактирование binds через `InputManagerV2` |

- `AiBackendProcessRuntime`: background worker (`spawn_ai_backend_process_worker`) управляет `ai_backend.py` (start/stop/restart/autostart); читает stdout/stderr в отдельных reader-threads; пишет в `runtime_log`.
- `CanvasSettingsRuntime`: coalescing saver → `settings.json` + `user_config.json`.
- Python resolve order: `installer_files/venv/`, `venv/`, `installer_files/env/`, `installer_files/python/` (nested scan); mini-installer раскладывает `uv` в `installer_files/uv/` и создаёт managed Python/venv через него.
- `apply_windows_no_window`: `CREATE_NO_WINDOW` на Windows.

## Python AI backend

- The backend is a separate `ai_backend.py` process that serves the framed IPC over a pluggable
  transport — AF_UNIX by default (socket path from `backend_ipc::backend_socket_path()`), loopback
  WebSocket fallback on Windows. The `--socket` argument is optional and defaults to the same standard
  path; Settings process management passes it explicitly when starting the backend. All Rust clients
  reach the backend through `crate::backend_ipc`.
- **Endpoints**: `/health`, `/device`, `/device/set`, `/device/cuda_diagnostics`, `/inpaint/lama_v2`, `/inpaint/aot`, `/inpaint/sdxl` (+ `/unload`), OCR endpoints, `/textdetector/ctd/detect`, `/textdetector/paddle/detect`.
- **Health probe** (`backend_health.rs`): `spawn_ai_backend_probe()` — poll every 2s; `AiBackendHealthSnapshot` шарится между Translation и Settings.
- `ensure_ai_backend_healthy()` — gate, возвращает unified error string.
- Управление процессом — из Settings tab (не из Translation).
- На ROCm-сборке Torch backend при старте (`ai_backend.py` → `rocm_runtime.configure_rocm_runtime()`)
  переводит MIOpen в immediate-режим (`MIOPEN_FIND_MODE=FAST`), отключает cudnn benchmark и
  закрепляет MIOpen-кэш в `ManhwaStudio_AI_Models/.cache/miopen`, чтобы Torch-инференс (LaMa и др.)
  не платил повторную per-shape компиляцию/тюнинг ядер. На CUDA/CPU/MPS — no-op.

---

## Нативный ONNX-рантайм (native OCR path)

Помимо Python backend есть in-process нативный ONNX-путь, выбираемый через
`General.ai_runtime` (`native` по умолчанию / `backend`). Эффективный рантайм
резолвит `AiRuntime::from_user_settings`: native применяется всегда, кроме случая,
когда пользователь ЯВНО выбрал backend через переключатель (это фиксируется флагом
`General.ai_runtime_configured=true`, который пишет `save_ai_runtime`). Конфиги без
этого флага (свежая установка или апгрейд) мигрируют на native, игнорируя старый
хранимый токен. Гейтинг по SIGILL-guard — отдельный слой (см. ниже):

- **`ms-onnx`** (`crates/ms-onnx`) — обёртка над `ort` (load-dynamic): `OrtRuntime::load`
  (dlopen + commit процесс-глобального ort-env, EP по `ExecutionProvider`), `MangaOcrEngine`
  (encoder+decoder инференс MangaOCR) и PaddleOCR (`PaddleDetector` — детекция + glyph-mask,
  `PaddleRecognizer`, `PaddleOcrEngine` — полный detect→recognize).
- **`src/onnx_runtime`** — ленивый загрузчик: probe/скачивание/verify/extract официального
  onnxruntime dylib в `data_dir()/onnxruntime/<provider>/<ver>/` (worker-thread only). Версия —
  ПО-ЗАПИСИ, не глобальная (`provider_version(provider)`), `ORT_VERSION` — только legacy-default.
  Manifest (`ort_manifest.json`) ключуется `(os,arch,provider)`; запись держит собственный `version`,
  primary `dylib_member`, `extra_members` (несколько DLL в один каталог) и `additional_sources`
  (доп. архивы в тот же каталог). CPU — GitHub-релизы; **DirectML** (Windows) —
  DLL из PyPI-wheel `onnxruntime-directml` (`onnxruntime.dll`+`DirectML.dll`+`onnxruntime_providers_shared.dll`
  рядом); **CoreML** (macOS) — тот же osx-архив, что CPU (EP встроен); **CUDA** (Windows/Linux) —
  официальный onnxruntime GPU-build (провайдер-либы `onnxruntime_providers_cuda`+`_shared` рядом с
  primary), зависит от СИСТЕМНЫХ CUDA 12.x/cuDNN 9.x (не бандлится); **WebGPU** (Windows/Linux/macOS) —
  wheel `onnxruntime-webgpu` **1.27.0** (Dawn D3D12/Vulkan/Metal, статически слинкован в основную либу;
  Windows-wheel дополнительно даёт `dxil.dll`+`dxcompiler.dll`) — отдельный GPU-бэкенд, НЕ считается
  байт-идентичным CPU. CPU/DirectML/CUDA/CoreML остаются на **1.20.1**. CUDA/WebGPU-хэши для GPU-архивов
  могут быть `null` (архивы сотни МБ) → logged-unverified путь, removal condition в MODULE_README.
- **`src/native_runtime`** — процесс-глобальный ленивый менеджер: держит один `OrtRuntime`, ОДИН
  общий `PaddleDetector` (always-resident, шарится detector-оп и всеми PaddleOCR-языками через
  `ms_onnx::paddle_recognize`) и LRU-ограниченный кэш движков (`MangaOcrEngine` Base/2025 +
  per-lang `PaddleRecognizer`), ёмкость = `General.ai_max_loaded_models` (читается один раз,
  clamp ≥1, default 3; LRU-eviction по least-recently-used, детектор не считается). Провайдер +
  индекс адаптера читаются один раз из ЕДИНОЙ ONNX-настройки `General.ai_onnx_provider` (ORT-токен
  `CPUExecutionProvider`/`DmlExecutionProvider`/`CUDAExecutionProvider`/`CoreMLExecutionProvider`/
  `WebGpuExecutionProvider`) + `General.ai_onnx_device_id` — ТЕ ЖЕ ключи, что использует Python-бэкенд,
  поэтому один выбор управляет обоими рантаймами (`native_runtime::execution_provider_from_ort_token`
  мапит токен → `ms_onnx::ExecutionProvider`; неизвестный/`not-selected` → CPU) — и фиксируются на процесс
  (недоступный для ОС провайдер → лог + fallback на CPU). При выборе CUDA системный
  probe CUDA 12.x/cuDNN 9.x (`gpu_utils::native_cuda_runtime_available`, вне GUI) решает: доступно →
  CUDA, иначе → DirectML (Windows с DirectML-адаптером) или CPU (лог, не неверный результат, не
  hard-fail). При выборе WebGPU лёгкий probe (`gpu_utils::native_webgpu_runtime_available`, вне GUI:
  Windows — DX12/DirectML-адаптер, Linux — Vulkan-loader + `/dev/dri`, macOS — Metal всегда) решает:
  доступно → WebGPU, иначе → CPU; реальный backstop — `error_on_failure` при регистрации EP.
  `run_guarded` разделяет SIGILL-guard-последовательность между manga/paddle/detector;
  `native_load_scope_key()` отдаёт scope `provider[:device]@version`, где version — ФАКТИЧЕСКАЯ версия
  провайдера (`onnx_runtime::provider_version`, например `webgpu@1.27.0` против `cpu@1.20.1`), так что
  SIGILL на одном провайдере/версии не блокирует другой (используется роутерами OCR/детекции и кнопкой
  сброса).
  Вызывается из НЕСКОЛЬКИХ worker-потоков (OCR, text-detector, cleaning-tools), никогда из GUI.
  Глобальный STATE-лок только на O(1) state; load/inference — без него. Все PaddleOCR detector-оп
  (detect + recognize, шарят единственный резидентный `PaddleDetector`) сериализованы через
  `PADDLE_OP_LOCK`: второй вызов БЛОКИРУЕТСЯ на первом, а не находит детектор `take()`нутым (что
  давало ложный fallback на backend) и не строит дублирующую сессию.
- **Выбор провайдера/устройства в UI** (`ai_backend_panel`): список ONNX-провайдеров — это ОБЪЕДИНЕНИЕ
  локального native-набора и провайдеров, о которых сообщил бэкенд. Native-набор строится ОФФЛАЙН из
  ОС + `gpu_utils` (CPU везде; DirectML на Windows — доступен при наличии адаптера, устройства =
  `detect_directml_accelerators_windows()`; CoreML на macOS; CUDA на не-macOS — доступен при
  `native_cuda_runtime_available()`; **WebGPU** на всех трёх десктоп-ОС — доступен при
  `native_webgpu_runtime_available()`, label «WebGPU (GPU)»); к нему при подключённом бэкенде добавляются его
  `available_onnx_providers` (dedup по ORT-токену, CPU один раз). Backend-only провайдеры без native EP
  (например `MIGraphXExecutionProvider`/ROCm на AMD) остаются в списке и ВЫБИРАЕМЫ — это чинит регрессию,
  когда AMD/ROCm-пользователь не мог выбрать MIGraphX. Каждый пункт помечается по АКТИВНОМУ рантайму
  (`General.ai_runtime`): при Native — usable только native-capable + локально доступные, иначе
  `(недоступно)`, а backend-only — `(только ИИ-бэкенд)` (native откатывается на CPU); при Backend —
  usable, если бэкенд подключён и сообщил провайдер, оффлайн показывается только native-набор. Устройства:
  для native-capable — локальный probe, для backend-only — `onnx_devices_by_provider[token]` (fallback
  `onnx_device_options`). Комбобоксы работают БЕЗ поднятого backend: выбор пишется в
  `General.ai_onnx_provider`/`ai_onnx_device_id` (+ `*_configured`) через
  `settings::save_onnx_provider_device` off-thread (нативный путь читает это при загрузке), а если
  backend поднят — дополнительно шлётся `device.set`. Лимит моделей (`ai_max_loaded_models`) тоже
  редактируется оффлайн (`settings::save_max_loaded_models`). PyTorch-устройство остаётся
  backend-gated (Torch всегда на бэкенде). Авто-скачивание dylib onnxruntime в фоне
  (`onnx_runtime::resolve_or_download_ort_dylib`) идёт только при Native и только для native-capable +
  доступного провайдера; для backend-only (MIGraphX) и недоступного — не скачивается.
- **SIGILL crash-guard** (per `provider[:device]@version`): состояние в `General.ort_load_state`. Перед первым
  dlopen пишется fsync'd маркер `attempted` (`settings::mark_ort_load_attempted`); `succeeded`
  ставится только после ПЕРВОГО успешного инференса. Крах во время load или первого инференса
  оставляет `succeeded=false` → следующий запуск читает scope как `Suspect` и не трогает ORT
  (fallback на backend). Graceful-ошибка (процесс выжил → это не SIGILL) сбрасывает маркер, НО
  только когда процесс-глобальный in-flight-счётчик (`native_runtime::IN_FLIGHT`) вернулся к 0 —
  иначе другой guarded-оп ещё может SIGILL'нуть, и маркер должен остаться. Все `save_*`/маркер-
  writer'ы `user_config.json` сериализованы `settings::USER_CONFIG_WRITE_LOCK` (RMW не теряет
  `attempted`-маркер под гонкой); маркер-write ещё fsync'ает родительский каталог при первом создании
  файла (Unix).
- **Роутинг**: OCR — `tabs/translation/ocr.rs::ocr_route` (чистая функция): Native при
  `ai_runtime==native` И guard не `Suspect` И (движок MangaOCR с ONNX-вариантом `base_onnx`/`2025_onnx`,
  НЕ `base_torch` → `NativeManga`; ЛИБО движок PaddleOCR → `NativePaddle`, любой язык). Детекция —
  `tabs/translation/text_detector.rs::detector_native_route`: Native при `ai_runtime==native` И guard
  не `Suspect` для PaddleOCR-детекции (`detect_page_paddle_ocr` и inline `detect_paddle_mask_for_image`).
  Нативно покрыты: OCR MangaOCR+PaddleOCR и детекция текста PaddleOCR; прочие операции — Python backend.
  При любой ошибке нативного пути — явный лог и fallback на backend (пользователь получает результат).
- **Без Python backend при `ai_runtime==native`**: warmup/readiness-гейт роут-осведомлён. Для нативного
  роута OCR (`ocr.rs::warmup_ocr_engine`) и детекции PaddleOCR (`text_detector.rs::run_detect_batch`)
  Python backend НЕ поднимается — рантайм/модель грузятся лениво при первом использовании, состояние
  сразу переходит в Ready. Backend требуется только для backend-роутов. Контракт native→backend
  сохранён: при ошибке нативного инференса fallback идёт на backend, ЕСЛИ он поднят (fallback-путь сам
  перепроверяет готовность), но native больше не требует поднятого backend, чтобы работать.

---

## Локализация

> ### ⛔ ОБЯЗАТЕЛЬНОЕ ПРАВИЛО ДЛЯ АГЕНТА: НИКАКОГО ТЕКСТА В КОДЕ
>
> **Запрещено писать пользовательский текст литералом в `.rs`.** Любая новая строка,
> которую увидит пользователь — подпись, кнопка, тултип, заголовок окна, статус,
> сообщение об ошибке — добавляется КЛЮЧОМ в каталог и вызывается через
> `t!` / `tf!` / `tp!`.
>
> **Ключ обязан появиться СРАЗУ ВО ВСЕХ существующих файлах `crates/ms-i18n/locales/*.json`**
> (сегодня это `ru.json` и `en.json`; когда появятся `es`/`fr`/`pt` — и в них тоже).
> Нельзя добавить ключ только в `ru.json`: `en` — эталон и fallback, и пропуск ключа
> роняет тест `key_validation`.
>
> **Если непонятно, как перевести строку — СПРОСИ ПОЛЬЗОВАТЕЛЯ**, что она означает и в
> каком контексте показывается. Не выдумывай перевод наугад и не оставляй заглушку молча.
>
> Ключи — семантические английские: `<area>.<screen_or_module>.<meaning>` с ролевым
> суффиксом (`_label`, `_hint`, `_button`, `_title`, `_error`, `_tooltip`, `_status`).
> Не кодировать в ключе имя функции и не кодировать сам текст.
>
> Исключения — только то, что НЕ является подписью: логи (`runtime_log::log_*`,
> `trace_log!`), протокольные идентификаторы, ключи персистентности, имена, уходящие на
> диск, и измерительные зонды. Полный список — `dev-docs/i18n_exclusions.md`. Если строка
> попадает под исключение, оставь литерал и напиши рядом комментарий с причиной.
>
> Локализуя `ComboBox::from_label` / `WheelComboBox::from_label` / `Window::new` /
> `CollapsingHeader::new` / `ui.collapsing`, обязательно добавь стабильный `id_salt`:
> egui выводит `Id` из текста подписи, иначе смена языка сбрасывает состояние виджета.

Два НЕЗАВИСИМЫХ языка, оба в `user_config.json`. Их нельзя смешивать.

| Настройка | Ключ | Тип | Что определяет |
|---|---|---|---|
| Язык интерфейса | `General.ui_language` | `ms_i18n::LocaleTag` (открытый тег) | подписи, кнопки, user-facing ошибки |
| Язык вёрстки | `TextTab.text_language` | `ms_text_util::language::TextLanguage` | движок переноса, требуемый набор глифов шрифта |

Тайпер может держать испанский интерфейс и верстать русскую главу: переносы и
проверка полноты шрифта идут по языку ТЕКСТА, не по языку UI.

Язык вёрстки выбирается из ДВУХ мест: Settings «Тайп» (`tabs/settings/typesetting.rs`) и общий
виджет `general_settings_panel.rs` (studio-вкладка «Общие» + страница настроек лаунчера). Оба
читают/пишут один process-global `ms_text_util::language` и один `save_text_language`, поэтому
разъехаться не могут; состояния выбора ни один из них не хранит.

- **Каталог на диске**: `src/locale_store.rs` (native-only, `cfg(not(wasm32))`) на старте
  распаковывает встроенные локали в `config::data_dir()/locale` и сверяет ключи:
  отсутствующие ДОБАВЛЯЮТСЯ, существующие значения НИКОГДА не перезаписываются,
  лишние ключи НИКОГДА не удаляются. Кастомный `<tag>.json` добивается значениями из
  `en.json`. Плюральный объект копируется целиком (переводчик вправе добавить `few`/`many`).
  Ядро — чистая функция `reconcile_locale_map(user, reference) -> ReconcileOutcome`;
  запись атомарна (temp + rename) и происходит ТОЛЬКО при реальном изменении.
  Недоступная на запись `locale/` — предупреждение в лог + работа на встроенном каталоге
  (ограниченная деградация, не тихий фолбэк: функциональность полная, теряется только
  правка файлов). Битый JSON — файл на диске НЕ трогается.
- **Вызов**: `run_main` после инициализации логов и после ранних выходов Windows-инсталлятора,
  до `load_user_settings_for_startup()`. На wasm диск-слой скомпилирован прочь.
- **Эталон — английский**. Неизвестный/битый тег → английский каталог + лог, НЕ русский.
  Неизвестный тег получает английские правила плюрализации, и это сообщается один раз
  на install через `FellBackToEnglish`, а не молча.
- **Сегментация**: `with_default_segmenter()` (`ms-text-util`) сохраняет сигнатуру и
  диспатчится по process-global языку; один `impl Segmenter` на `ScriptGroup`
  (`cyrillic_slavic` / `romance` / `english` / `latin_slavic`), паттерны TeX берутся из
  crate `hyphenation` (фича `embed_all`, все языки уже вшиты).
  `HyphenationDictionaries::for_language` кэширует словари per-language thread-locally.
- **Покрытие шрифта**: `src/tabs/typing/panel/font_coverage.rs` классифицирует шрифт как
  `Full`/`Partial`/`Unsupported` через `swash` cmap. `script_chars` берутся от `ScriptGroup`,
  `extra_chars` — от конкретного `TextLanguage`. Кэш на `FontEntry` самолечится:
  `panel/facade.rs` сравнивает `text_language()` с языком кэша и перезапускает классификацию
  в фоне при расхождении.

**Строки-идентификаторы.** Часть русских литералов — НЕ подписи, а ключи персистентности
(`TextTab.formula_presets`, имена сокетов нод batch-processing, `General.enabled_tabs`).
Их перевод ломает сохранённые данные пользователя. Исчерпывающий список — `dev-docs/i18n_exclusions.md`;
инструмент миграции `tools/i18n_extract.py` обязан их пропускать. Отдельный класс —
egui `Id`, выводимый из текста подписи (`ComboBox::from_label`, `Window::new`,
`CollapsingHeader::new`): при локализации такого сайта обязателен стабильный `id_salt`.

---

## Runtime log (`src/runtime_log.rs`)

- Ротация: `last.log` → `previous.log` при старте.
- Background writer thread через mpsc channel.
- `log_info` / `log_warn` / `log_error` / `log_ai_backend`.
- Panic hook устанавливается один раз через `OnceLock`.

---

## Конфигурация (`src/config.rs`)

- Константы путей: `BUBBLES_FILE`, `SRC_DIR`, `CLEAN_LAYERS_DIR`, etc.
- `JsonConfig`: load/merge/save с `merge_missing` (default backfill).
- `user_config_defaults()`: полное дерево дефолтов (General, Canvas, Hotkeys, TranslationTab, TextTab с formula presets).
- `project_config_defaults()`: per-project дефолты.
- `USER_CONFIG_FILE` — имя `user_config.json`; `program_dir()` — launch working directory с fallback на директорию exe.

---

## Кастомные виджеты (`src/widgets/`)

| Виджет | Назначение |
|---|---|
| `WheelSlider` | Slider с wheel-step, гасит scroll родителя |
| `WheelComboBox` | ComboBox с wheel-переключением, гасит scroll |
| `WheelSpinBox` | DragValue с wheel-behaviour |
| `AutocompleteLine` | однострочный ввод с dropdown автодополнения |
| `SpellcheckedTextEdit` | многострочный `TextEdit` с фоновой орфографией через pure-Rust Hunspell backend и подчёркиванием ошибок |
| `ViewportColorSelector` | color picker с пипеткой из viewport (egui screenshot event) |

**Правило**: не использовать `egui::Slider`, `egui::ComboBox` напрямую в продуктовом UI — использовать Wheel-версии.

## Shared инструменты (`src/tools/`)

- `MaskBrush`: переиспользуемая кисть для рисования бинарной маски в `egui::ColorImage` (радиус, Shift+wheel смена размера, hotkeys, cursor overlay).
- `paint_line_with_brush`: helper штриха по ColorImage для круглой кисти.

---

## Tutorial system (`src/tutorial/`)

**Gated behind the `tutorial` cargo feature (off by default).** With the feature
off, `mod tutorial`, the `TutorialController` fields/`mark` sites, all
autoplay/`sync`/`render` calls, and the "Обучение" settings panes (launcher +
studio) are all compiled out via `#[cfg(feature = "tutorial")]`; nothing tutorial
renders and no dead code remains. Enable with `--features tutorial` to restore the
tours. The `src/tutorial/` engine still compiles regardless because the demo bin
`src/bin/tutorial_test/` mounts `engine.rs` directly via `#[path]`.

Shared onboarding layer. One overlay engine (`engine.rs`) dims the viewport
except a spotlighted target (dashed outline + arrow + text callout) and absorbs
**all** input beneath it via a single full-viewport hitbox on `Order::Middle`
(pure z-order occlusion, not per-widget disabling — a widget that reads the raw
pointer instead of `Response::contains_pointer`/`hovered` leaks under the dim).

- **`TutorialId`** — central enum, the stable persistence key (`id.rs`);
  `is_available` gates which ids show in the replay UI.
- **`TutorialProgress`** (+ `TutorialProgressHandle`) — completed-set + autoplay,
  persisted to the `user_config.json` `Tutorials` section on a background thread.
- **`TutorialController<C>`** — per-surface runner: registry + active `Tutorial<C>`
  + catalog (`TutorialId → steps fn`) + shared progress handle. Enforces one
  active tutorial per surface, autoplays unseen tutorials on entry (caller
  edge-triggers `maybe_autoplay`), records completion on the finish/skip edge.
  `C` is the surface context a step's `on_enter` mutates to navigate the UI.
- **`draw_tutorials_pane`** (`settings_pane.rs`) — surface-agnostic "Обучение"
  replay pane reused by the studio Settings tab and the launcher settings page
  (double interface, like `ai_backend_panel`); depends only on the progress
  handle, so resetting a tutorial re-arms its autoplay on the owning surface.

Steps support **branching** (`.choice(label, goto_id)` + `StepNext::Goto/Finish`,
navigated by a history stack so "Назад" is correct across jumps) and **waiting**
(`.await_gate` auto-advances when a `Fn(&GateCtx<C>)->bool` holds; `.gated` just
disables "Далее"). A target-less step (`TutorialStep::message`) dims the whole
viewport with a centred callout. When a step must trigger something the surface
guards behind `&mut self`, `C` is a command-sink + state-snapshot: `on_enter`
queues commands the surface drains after `sync`, gates read the snapshot.

Per-surface step scripts live next to their UI (e.g. `src/launcher/tutorial.rs`),
not in the shared module. The demo bin `src/bin/tutorial_test/` mounts `engine.rs`
via `#[path]` so demo and production never diverge. Currently wired: the launcher
main-menu tour (`LauncherMain`) and the branching new-project tour (`NewProject`,
`src/launcher/new_project/tutorial.rs` — its visual branch drives a real
download→stitch→waifu2x pipeline, waiting on each op); studio surfaces are
reserved ids for later phases (`dev-docs/tutorial-plan.md`).

---

## Фоновые пайплайны

| Пайплайн | Где |
|---|---|
| Page + clean-overlay decode pool (interleaved, page order) → ordered page promotion → GPU upload; overlays → model as they arrive | `app.rs` loader thread |
| Studio project load (behind loading screen) | `studio_bootstrap.rs` `spawn_project_load_thread` |
| Bubble saver (coalescing) | `BubblesModel::spawn_bubbles_saver_thread` |
| Layer persistence (coalescing) | `layer_model/saver.rs` |
| PS page-switch layer DECODE (off-thread) | `ps_editor/page_loader.rs` worker → `LayerDoc::decode_page_payload` (lock-free); GUI thread only `insert_decoded_page` |
| Canvas settings saver | `canvas/workers.rs` / `settings/mod.rs` |
| Overlay tile prepare | `canvas/workers.rs spawn_overlay_prepare_thread` |
| OCR run | `translation/ocr.rs` background thread per request |
| Text detection run | `translation/text_detector.rs` command/event bridge |
| MT run (per bubble) | `translation/machine_translation.rs` per-run thread |
| adv_rec crop loader | `translation/adv_rec.rs` background thread |
| AI backend probe | `translation/backend_health.rs` probe thread |
| AI backend process manager | `settings/mod.rs spawn_ai_backend_process_worker` |
| Typing text render (live) | `typing/tab.rs` latest-wins render thread |
| PS editor raster effects render | `ps_editor/mod.rs` `render_ps_raster_effects` worker (latest-wins) |
| Typing save text_info.json | `typing/tab.rs` deferred worker |
| Typing export | `typing/tab.rs` background export thread |
| Region loader (cleaning tools) | `cleaning/tools/base.rs spawn_region_loader_thread` |
| Cleaning inpaint run | `cleaning/tools/base.rs RegionMaskInpaintToolBase` worker thread |
| Quick text clean | `cleaning/tab.rs` multithread per-page job |
| Characters thumbnail decode | `characters.rs` background thread |
| Notes prompt assembly | `notes.rs` background worker thread |
| Wiki scan / load / image decode | `wiki.rs` per-operation threads |
| Version check | `main.rs` pre-window thread |

**Принцип**: любая новая тяжёлая операция должна быть вынесена в worker-thread с явным lifecycle (запуск, poll, cancel, error log).

---

## Verifying UI changes (egui MCP)

The `/egui-mcp` skill can drive the live app (click, type, screenshot). Do **not** reach for it
on your own initiative: launching the app hijacks a real window on the user's desktop, a session
of blind clicking is slow and easily lands in the wrong project/page, and an agent cannot tell
"looks right" from "is right" for this product's UI.

Default contract for a UI change:

1. Finish the change, build it, and describe **what** should now be visible/clickable and **where**.
2. **Ask the user to test it in the running app** — state the exact steps and the expected result,
   so the answer is a yes/no, not an investigation.
3. Drive the app over egui MCP only when the user explicitly asks for it ("проверь сам",
   "погоняй через MCP", "сделай скриншот"), or when they hand back a reproduction that
   needs an interactive bisect and they agreed to the app being taken over.

The user's report is the source of truth for UI behavior. Do not claim a UI fix is verified
because a screenshot looked plausible.

---

## Что важно не ломать

- **GUI-поток** — никакого I/O, декодирования изображений, сети, длительных вычислений.
- **Shared state** — только через `Arc<Mutex<…>>` модели с `revision`; не копировать состояние вручную между вкладками.
- **CanvasView** — общий движок; логика вкладки добавляется через `CanvasHooks` и отдельные runtime-слои, не форком canvas-кода.
- **CleanOverlaysModel** — держать двойное представление (ColorImage + RgbaImage); одностороннее разрушает export и инструменты.
- **Сохранение** — через saver-thread с coalescing; sync-запись из GUI-потока — ошибка архитектуры.
- **Слои (layer_model)** — запись на диск асинхронна и коалесцируется через `layer_model/saver.rs`
  (`LayerDoc::enable_background_saver`, включается один раз в `app.rs`). PS per-edit/raster и typing
  text/effects flush'и ENQUEUE'ят задания (`enqueue_page_save` / `enqueue_page_text_save` /
  `enqueue_raster_effects`; sync-fallback при выключенном saver).
  Dirty flags clear only on a successful per-kind acknowledgement whose epoch matches the latest
  edit/enqueue; stale and failed acknowledgements keep dirty set, and barrier ownership considers
  only TEXT failures. Гарантия «байты на диске» даётся
  ТОЛЬКО барьером: `barrier_blocking` в merge-воркере save-to-project (ДО `merge_unsaved_into_project`)
  и drain (`barrier_blocking` + `shutdown_saver`) в eframe `on_exit` и на exit-cleanup. БАРЬЕР НИКОГДА
  не выполняется в GUI-потоке. Контракт удаления растров (`removed_uids` в `persist_current_page`) и
  ownership owned-page merge не менять — async меняет ТОЛЬКО где/когда происходит запись, не сами байты.
  Исключение, остающееся синхронным: `flush_target_page_text_to_staging` (воркер raster-create читает
  staging сразу — async race resurrect'ил бы удалённый текст).
- **Python backend** — сбои не должны зависать GUI; всегда проверять `ensure_ai_backend_healthy()` перед запросами.
- **App-managed AI models** — Rust worker paths call `src/ai_models.rs` before Python backend
  model initialization; EasyOCR and Surya remain library-cache managed.
- **BubbleStatus rules** — хранятся в `SharedCanvasSettings`; редактируются только через Settings tab, применяются через `CanvasHooks::bubble_status_style`.
- **Локализация** — **никакого пользовательского текста литералом в `.rs`**: новая строка
  идёт ключом во ВСЕ файлы `crates/ms-i18n/locales/*.json` сразу и вызывается через
  `t!`/`tf!`/`tp!`; неясен смысл строки для перевода — спросить пользователя, а не
  выдумывать (см. раздел «Локализация»). Язык UI и язык вёрстки независимы; не связывать
  их. `t!` обязан оставаться wait-free и без аллокаций (`ArcSwap` + `&'static str`); не
  вводить в него `String`-ключи. Сверка `locale/` не перезаписывает значения и не удаляет
  ключи. Эталонная локаль и любой fallback — английские, никогда не русские.
- **Строки-ключи** — литералы из `dev-docs/i18n_exclusions.md` не переводить: они являются
  ключами в `user_config.json` и в JSON графов batch-processing.

---

## Когда обновлять этот файл

- появляется новый архитектурный слой или shared-модель;
- меняется поток данных между слоями;
- меняется контракт `CanvasView` / `CanvasHooks`;
- меняется интеграция Rust ↔ Python backend или app-managed AI model gating;
- меняется способ координации фоновых worker-ов.

Не добавлять: историю рефакторинга, списки UI-кнопок/хоткеев, временные багфиксы, нестабильные детали интерфейса, секреты и локальные пути.

- Новый режим висячей пунктуации: компенсация
- Не переносить частички Не, Же
- 
