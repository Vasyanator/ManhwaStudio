/*
File: src/launcher/new_project/batch_processing/node_defs.rs

Purpose:
Static definitions (socket layouts, palette metadata) for all 13 supported node types.

Main responsibilities:
- Provide NodeDefs: a lookup table from template_key → node definition
- Return the list of socket specs for any node (used by canvas rendering and connection validation)
- Expose the palette list used to populate the left panel categories

Key structures:
- NodeDef       — one entry: category, description, sockets
- NodeDefs      — the registry

Notes:
Socket names are Russian strings matching the Python originals so that JSON files
produced by the Python implementation can be loaded without edge remapping.
Variable-node socket layout is dynamic (depends on variable data_type) and handled
via socket_spec_for_variable_node().
*/

use super::types::{DataType, SocketSpec};
use std::collections::HashMap;

// ─── Node definition ──────────────────────────────────────────────────────────

pub struct NodeDef {
    pub title: &'static str,
    pub description: &'static str,
    /// Sockets in display order (inputs then outputs, or interleaved).
    pub sockets: Vec<SocketSpec>,
}

// ─── Registry ─────────────────────────────────────────────────────────────────

pub struct NodeDefs {
    defs: HashMap<&'static str, NodeDef>,
}

impl NodeDefs {
    pub fn build() -> Self {
        let mut defs: HashMap<&'static str, NodeDef> = HashMap::new();

        // ── Старт (число) ───────────────────────────────────────────────────
        defs.insert(
            "start_number",
            NodeDef {
                title: "Старт (число)",
                description: "Генерирует целые числа в диапазоне [start, end] с шагом step.",
                sockets: vec![
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_out("Индекс", DataType::Int),
                ],
            },
        );

        // ── Старт (строка) ──────────────────────────────────────────────────
        defs.insert(
            "start_string",
            NodeDef {
                title: "Старт (строка)",
                description: "Читает строки из txt-файла и выдаёт их по одной за цикл.",
                sockets: vec![
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_out("Строка", DataType::Str),
                ],
            },
        );

        // ── Шаблон строки ───────────────────────────────────────────────────
        defs.insert(
            "string_template",
            NodeDef {
                title: "Шаблон строки",
                description: "Подставляет значения переменных в шаблон с {placeholder} метками.",
                // Actual sockets depend on the placeholders list and are computed dynamically
                // in socket_specs_for_node(); this list is a base with only fixed sockets.
                sockets: vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_out("Строка", DataType::Str),
                ],
            },
        );

        // ── Быстрая загрузка ─────────────────────────────────────────────────
        defs.insert(
            "quick_downloader",
            NodeDef {
                title: "Быстрая загрузка",
                description: "Скачивает изображения по URL с поддерживаемых сайтов.",
                sockets: vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_in("URL", DataType::Str),
                    SocketSpec::data_out("Картинки", DataType::ImageList),
                ],
            },
        );

        // ── Открыть URL ──────────────────────────────────────────────────────
        defs.insert(
            "open_url",
            NodeDef {
                title: "Открыть URL",
                description: "Открывает URL в браузере через Selenium и ждёт загрузки страницы.",
                sockets: vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_in("URL", DataType::Str),
                ],
            },
        );

        // ── Прокрутить страницу ──────────────────────────────────────────────
        defs.insert("scroll_page", NodeDef {
            title: "Прокрутить страницу",
            description: "Медленно прокручивает страницу вниз и вверх для загрузки lazy-контента.",
            sockets: vec![
                SocketSpec::exec_in("Вход"),
                SocketSpec::exec_out("Далее"),
            ],
        });

        // ── Получить из браузера ─────────────────────────────────────────────
        defs.insert(
            "fetch_from_browser",
            NodeDef {
                title: "Получить из браузера",
                description: "Собирает картинки из текущей страницы браузера по шаблону URL.",
                sockets: vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_out("Картинки", DataType::ImageList),
                ],
            },
        );

        // ── Склейка / Нарезка ────────────────────────────────────────────────
        defs.insert(
            "stitch_split",
            NodeDef {
                title: "Склейка / Нарезка",
                description: "Склеивает страницы вертикально и нарезает по безопасным строкам.",
                sockets: vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_in("Картинки", DataType::ImageList),
                    SocketSpec::data_out("Картинки", DataType::ImageList),
                ],
            },
        );

        // ── Waifu2x ──────────────────────────────────────────────────────────
        defs.insert(
            "waifu2x",
            NodeDef {
                title: "Waifu2x",
                description: "Увеличивает изображения с помощью waifu2x-ncnn-vulkan.",
                sockets: vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_in("Картинки", DataType::ImageList),
                    SocketSpec::data_out("Картинки", DataType::ImageList),
                ],
            },
        );

        // ── Сохранить в папку ────────────────────────────────────────────────
        defs.insert("save_folder", NodeDef {
            title: "Сохранить в папку",
            description: "Записывает список картинок в указанную папку с числовыми именами. Сокет «Путь» переопределяет папку из параметров.",
            sockets: vec![
                SocketSpec::exec_in("Вход"),
                SocketSpec::exec_out("Далее"),
                SocketSpec::data_in("Картинки", DataType::ImageList),
                SocketSpec::data_in("Путь", DataType::Str),
            ],
        });

        // ── Чтение переменной ────────────────────────────────────────────────
        // Actual data type of the output socket depends on the referenced variable;
        // handled dynamically in socket_specs_for_node().
        defs.insert(
            "variable_read",
            NodeDef {
                title: "Чтение переменной",
                description: "Читает значение переменной и передаёт его в граф.",
                sockets: vec![
                    SocketSpec::data_out("Значение", DataType::Str), // placeholder; overridden at runtime
                ],
            },
        );

        // ── Запись переменной ────────────────────────────────────────────────
        defs.insert(
            "variable_write",
            NodeDef {
                title: "Запись переменной",
                description: "Сохраняет входное значение в переменную.",
                sockets: vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_in("Значение", DataType::Str), // placeholder; overridden at runtime
                ],
            },
        );

        // ── Конец ────────────────────────────────────────────────────────────
        defs.insert("end", NodeDef {
            title: "Конец",
            description: "Точка синхронизации: ждёт все входящие exec-сигналы перед завершением цикла.",
            sockets: vec![
                SocketSpec {
                    name: "Вход",
                    is_input: true,
                    kind: super::types::SocketKind::Exec,
                    allow_multiple: true,
                },
            ],
        });

        Self { defs }
    }

    /// Look up a node definition by template_key.
    pub fn get(&self, key: &str) -> Option<&NodeDef> {
        self.defs.get(key)
    }

    /// Compute the effective socket list for a node instance.
    /// This is the primary function used by the canvas and connection validator.
    pub fn socket_specs_for_node(
        &self,
        template_key: &str,
        node_params: &super::types::NodeParams,
        variables: &[super::graph::GraphVariable],
    ) -> Vec<SocketSpec> {
        match template_key {
            "string_template" => {
                // Dynamic: add one data-input socket per placeholder.
                let base = self
                    .defs
                    .get("string_template")
                    .map(|d| d.sockets.clone())
                    .unwrap_or_default();
                let mut sockets = Vec::new();
                if let super::types::NodeParams::StringTemplate { placeholders, .. } = node_params {
                    for name in placeholders {
                        // Use a leaked string so the lifetime matches &'static str requirement.
                        // In practice, placeholders live as long as the graph.
                        let name_static: &'static str = Box::leak(name.clone().into_boxed_str());
                        sockets.push(SocketSpec::data_in(name_static, DataType::Str));
                    }
                }
                // Append fixed sockets.
                sockets.extend(base);
                sockets
            }
            "variable_read" => {
                let dt = variable_data_type(node_params, variables).unwrap_or(DataType::Str);
                vec![SocketSpec::data_out("Значение", dt)]
            }
            "variable_write" => {
                let dt = variable_data_type(node_params, variables).unwrap_or(DataType::Str);
                vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_in("Значение", dt),
                ]
            }
            key => self
                .defs
                .get(key)
                .map(|d| d.sockets.clone())
                .unwrap_or_default(),
        }
    }

    /// Look up one socket spec by (template_key, socket_name).
    /// Returns the first matching socket.
    pub fn socket_spec(&self, template_key: &str, socket_name: &str) -> Option<SocketSpec> {
        // For the common case we can use the static list.
        self.defs
            .get(template_key)
            .and_then(|d| d.sockets.iter().find(|s| s.name == socket_name).cloned())
    }

    /// Return a list of (category, keys) groups for the palette panel.
    pub fn palette_groups() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("Старт", vec!["start_number", "start_string"]),
            ("Строки", vec!["string_template"]),
            ("I/O", vec!["quick_downloader", "save_folder"]),
            (
                "Браузер",
                vec!["open_url", "scroll_page", "fetch_from_browser"],
            ),
            ("Обработка", vec!["stitch_split", "waifu2x"]),
            ("Переменные", vec!["variable_read", "variable_write"]),
            ("Поток", vec!["end"]),
        ]
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn variable_data_type(
    params: &super::types::NodeParams,
    variables: &[super::graph::GraphVariable],
) -> Option<DataType> {
    let name = match params {
        super::types::NodeParams::VariableRead { variable_name }
        | super::types::NodeParams::VariableWrite { variable_name } => variable_name.as_str(),
        _ => return None,
    };
    variables
        .iter()
        .find(|v| v.name == name)
        .map(|v| v.data_type)
}
