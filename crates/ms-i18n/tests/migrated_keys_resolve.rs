/*
File: crates/ms-i18n/tests/migrated_keys_resolve.rs

Purpose:
Guard against the `t!`-returns-key-on-miss trap for the migrated UI strings.

`t!(key)` returns the key text itself when the key is absent, so a typo'd or
never-added key looks like a working translation — the user just sees
`tabs.settings.general.projects_dir` on a button instead of a label. The
`key_validation` test proves every used key EXISTS in `en.json`; this test proves a
sample of the pilot-migrated keys actually RESOLVE to their Russian values under the
`ru` catalog and are NOT equal to the key string.

Runs as its own test binary (separate process), so setting the process-global
locale here does not race the crate's other global-state tests.
*/

use ms_i18n::{LocaleTag, lookup, set_locale};

/// A handful of keys migrated in the `src/tabs/settings/` + `src/widgets/` pilot,
/// paired with their expected Russian catalog value.
const SAMPLE: &[(&str, &str)] = &[
    ("settings.canvas_ribbon.heading", "Лента"),
    ("settings.typesetting.script_group_label", "Группа"),
    ("settings.typesetting.language_label", "Язык"),
    (
        "settings.hotkeys.translation_scope_title",
        "Вкладка перевода",
    ),
    ("widgets.ai_button.requires_pytorch", "Требуется PyTorch."),
    ("widgets.viewport_color_selector.eyedropper", "Пипетка"),
    // Keys migrated in the CRUD tabs / canvas / tutorial / ps_editor batch.
    ("characters.list.add_button", "Добавить"),
    ("terms.delete_dialog.title", "Удалить термин"),
    ("notes.heading", "Заметки перевода"),
    ("wiki.no_files", "В папке wiki нет .md файлов."),
    ("canvas.bubble.translate_button", "Перевести"),
    ("tutorial.title.ps_editor", "PS-редактор"),
    ("ps_editor.tools.brush_title", "Кисть"),
    // Keys migrated in the cleaning / translation tab batch.
    ("cleaning.tools.lama.title", "AI удаление (Lama)"),
    ("cleaning.mask_editor.process_button", "Обработать"),
    ("cleaning.common.cancel_button", "Отмена"),
    // Model display name resolved via a runtime accessor (was a `const` literal).
    ("cleaning.tools.lama.model_base", "Базовая"),
    ("translation.ocr.worker_unavailable_error", "OCR worker недоступен."),
    // OCR language label resolved from a `(code, display_key)` catalog tuple.
    ("translation.ocr_langs.japanese", "Японский"),
    ("translation.tab.bubbles_title", "Пузыри"),
    // Keys migrated in the typing tab batch.
    ("typing.effects.stroke_title", "Обводка"),
    ("typing.mask.panel_title", "Маска обрезки"),
    ("typing.presets.none_option", "Нет"),
    ("typing.deform.mode_perspective", "Перспектива"),
    ("typing.params.kerning_metric", "Метрический"),
    // A former `const &str` literal resolved through a runtime accessor.
    ("typing.panel.default_preview_text", "Текст будет выглядеть так"),
    // Keys migrated in the launcher + installer batch.
    ("launcher.new_project.window_title", "Новый проект"),
    ("launcher.common.refresh_button", "Обновить"),
    ("launcher.settings.heading", "Настройки"),
    // Batch-processing node title (display-only; socket names stay raw literals).
    ("launcher.batch.node_end_title", "Конец"),
    ("installer.install.install_button", "Установить"),
    ("installer.common.close_button", "Закрыть"),
    // Keys migrated in the final root-`src/` batch (app.rs, ai_backend*, backend_ipc,
    // onnx_runtime, main.rs, and the shared root modules).
    ("app.tab.translation", "Перевод"),
    ("ai_backend.check_now_button", "Проверить сейчас"),
    (
        "backend_ipc.frame.header_not_object",
        "JSON-заголовок кадра backend должен быть объектом.",
    ),
    ("startup.basic_launcher.heading", "Базовый лаунчер"),
    ("bubble_status.border_solid", "Сплошная"),
    ("memory.profile.minimal", "Минимум"),
    ("settings.general.ui_language_label", "Язык интерфейса:"),
];

#[test]
fn migrated_keys_resolve_to_russian_under_ru_catalog() {
    let ru = LocaleTag::parse("ru").expect("ru is a valid tag");
    set_locale(&ru).expect("ru is an embedded locale");

    for (key, expected_ru) in SAMPLE {
        let resolved = lookup(key);
        assert_eq!(
            resolved,
            Some(*expected_ru),
            "key {key:?} did not resolve to its Russian value under the ru catalog",
        );
        // The whole point: a resolved value must not be the key itself (the miss
        // fallback). If this fires, the key is absent from ru.json.
        assert_ne!(
            resolved,
            Some(*key),
            "key {key:?} resolved to the key text — it is missing from the ru catalog",
        );
    }
}
