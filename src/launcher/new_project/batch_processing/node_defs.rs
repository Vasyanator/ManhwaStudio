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
Socket `name` fields are Russian identifiers matching the Python originals so that JSON
files produced by the Python implementation load without edge remapping; they are wire
keys, never localized. The painted UI label is a separate `SocketSpec::label_key` (catalog
key `launcher.batch.socket.*`) attached to fixed template sockets via `.with_label(...)`.
Dynamic sockets (string-template placeholders, variable nodes) carry user-authored names,
get no `label_key`, and display their raw `name`.
Variable-node socket layout is dynamic (depends on variable data_type) and handled
via socket_specs_for_node().
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
                title: t!("launcher.batch.node_number_start_title"),
                description: t!("launcher.batch.node_number_start_desc"),
                sockets: vec![
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    SocketSpec::data_out("Индекс", DataType::Int)
                        .with_label("launcher.batch.socket.index"),
                ],
            },
        );

        // ── Старт (строка) ──────────────────────────────────────────────────
        defs.insert(
            "start_string",
            NodeDef {
                title: t!("launcher.batch.node_string_start_title"),
                description: t!("launcher.batch.node_string_start_desc"),
                sockets: vec![
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    SocketSpec::data_out("Строка", DataType::Str)
                        .with_label("launcher.batch.socket.string"),
                ],
            },
        );

        // ── Шаблон строки ───────────────────────────────────────────────────
        defs.insert(
            "string_template",
            NodeDef {
                title: t!("launcher.batch.node_string_template_title"),
                description: t!("launcher.batch.node_string_template_desc"),
                // Actual sockets depend on the placeholders list and are computed dynamically
                // in socket_specs_for_node(); this list is a base with only fixed sockets.
                sockets: vec![
                    SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    SocketSpec::data_out("Строка", DataType::Str)
                        .with_label("launcher.batch.socket.string"),
                ],
            },
        );

        // ── Быстрая загрузка ─────────────────────────────────────────────────
        defs.insert(
            "quick_downloader",
            NodeDef {
                title: t!("launcher.batch.node_quick_download_title"),
                description: t!("launcher.batch.node_quick_download_desc"),
                sockets: vec![
                    SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    // "URL" is already Latin; no label key, painted verbatim.
                    SocketSpec::data_in("URL", DataType::Str),
                    SocketSpec::data_out("Картинки", DataType::ImageList)
                        .with_label("launcher.batch.socket.images"),
                ],
            },
        );

        // ── Открыть URL ──────────────────────────────────────────────────────
        defs.insert(
            "open_url",
            NodeDef {
                title: t!("launcher.batch.node_open_url_title"),
                description: t!("launcher.batch.node_open_url_desc"),
                sockets: vec![
                    SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    // "URL" is already Latin; no label key, painted verbatim.
                    SocketSpec::data_in("URL", DataType::Str),
                ],
            },
        );

        // ── Прокрутить страницу ──────────────────────────────────────────────
        defs.insert("scroll_page", NodeDef {
            title: t!("launcher.batch.node_scroll_page_title"),
            description: t!("launcher.batch.node_scroll_page_desc"),
            sockets: vec![
                SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
            ],
        });

        // ── Получить из браузера ─────────────────────────────────────────────
        defs.insert(
            "fetch_from_browser",
            NodeDef {
                title: t!("launcher.batch.node_browser_fetch_title"),
                description: t!("launcher.batch.node_browser_fetch_desc"),
                sockets: vec![
                    SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    SocketSpec::data_out("Картинки", DataType::ImageList)
                        .with_label("launcher.batch.socket.images"),
                ],
            },
        );

        // ── Склейка / Нарезка ────────────────────────────────────────────────
        defs.insert(
            "stitch_split",
            NodeDef {
                title: t!("launcher.batch.node_stitch_cut_title"),
                description: t!("launcher.batch.node_stitch_cut_desc"),
                sockets: vec![
                    SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    SocketSpec::data_in("Картинки", DataType::ImageList)
                        .with_label("launcher.batch.socket.images"),
                    SocketSpec::data_out("Картинки", DataType::ImageList)
                        .with_label("launcher.batch.socket.images"),
                ],
            },
        );

        // ── Waifu2x ──────────────────────────────────────────────────────────
        defs.insert(
            "waifu2x",
            NodeDef {
                title: "Waifu2x",
                description: t!("launcher.batch.node_waifu2x_desc"),
                sockets: vec![
                    SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    SocketSpec::data_in("Картинки", DataType::ImageList)
                        .with_label("launcher.batch.socket.images"),
                    SocketSpec::data_out("Картинки", DataType::ImageList)
                        .with_label("launcher.batch.socket.images"),
                ],
            },
        );

        // ── Сохранить в папку ────────────────────────────────────────────────
        defs.insert("save_folder", NodeDef {
            title: t!("launcher.batch.node_save_folder_title"),
            description: t!("launcher.batch.node_save_folder_desc"),
            sockets: vec![
                SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                SocketSpec::data_in("Картинки", DataType::ImageList)
                    .with_label("launcher.batch.socket.images"),
                SocketSpec::data_in("Путь", DataType::Str)
                    .with_label("launcher.batch.socket.path"),
            ],
        });

        // ── Чтение переменной ────────────────────────────────────────────────
        // Actual data type of the output socket depends on the referenced variable;
        // handled dynamically in socket_specs_for_node().
        defs.insert(
            "variable_read",
            NodeDef {
                title: t!("launcher.batch.node_read_variable_title"),
                description: t!("launcher.batch.node_read_variable_desc"),
                sockets: vec![
                    SocketSpec::data_out("Значение", DataType::Str), // placeholder; overridden at runtime
                ],
            },
        );

        // ── Запись переменной ────────────────────────────────────────────────
        defs.insert(
            "variable_write",
            NodeDef {
                title: t!("launcher.batch.node_write_variable_title"),
                description: t!("launcher.batch.node_write_variable_desc"),
                sockets: vec![
                    SocketSpec::exec_in("Вход"),
                    SocketSpec::exec_out("Далее"),
                    SocketSpec::data_in("Значение", DataType::Str), // placeholder; overridden at runtime
                ],
            },
        );

        // ── Конец ────────────────────────────────────────────────────────────
        defs.insert("end", NodeDef {
            title: t!("launcher.batch.node_end_title"),
            description: t!("launcher.batch.node_end_desc"),
            sockets: vec![
                SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
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
                        // User-authored placeholder: the name is both the wire identifier and
                        // the painted label (no catalog key). `SocketSpec::name` is a `Cow`, so
                        // we clone the owned `String` instead of leaking a `&'static str` — this
                        // runs every frame on the draw path (canvas.rs), so a leak would grow
                        // memory without bound.
                        sockets.push(SocketSpec::data_in(name.clone(), DataType::Str));
                    }
                }
                // Append fixed sockets.
                sockets.extend(base);
                sockets
            }
            // Variable-node sockets are rebuilt here with the referenced variable's data type.
            // Their NAMES are the same fixed identifiers used everywhere else
            // (`"Вход"`/`"Далее"`/`"Значение"`, see `docs/i18n_exclusions.md` §A2), so they
            // reuse the shared `launcher.batch.socket.*` label keys — only the data type is
            // dynamic, not the label. (Genuinely user-authored socket names exist only for
            // `string_template` placeholders, which carry no key and paint verbatim.)
            "variable_read" => {
                let dt = variable_data_type(node_params, variables).unwrap_or(DataType::Str);
                vec![SocketSpec::data_out("Значение", dt).with_label("launcher.batch.socket.value")]
            }
            "variable_write" => {
                let dt = variable_data_type(node_params, variables).unwrap_or(DataType::Str);
                vec![
                    SocketSpec::exec_in("Вход").with_label("launcher.batch.socket.input"),
                    SocketSpec::exec_out("Далее").with_label("launcher.batch.socket.next"),
                    SocketSpec::data_in("Значение", dt).with_label("launcher.batch.socket.value"),
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
            .and_then(|d| d.sockets.iter().find(|s| s.name.as_ref() == socket_name).cloned())
    }

    /// Return a list of (category, keys) groups for the palette panel.
    pub fn palette_groups() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            (t!("launcher.batch.palette_start"), vec!["start_number", "start_string"]),
            (t!("launcher.batch.palette_strings"), vec!["string_template"]),
            ("I/O", vec!["quick_downloader", "save_folder"]),
            (
                t!("launcher.batch.palette_browser"),
                vec!["open_url", "scroll_page", "fetch_from_browser"],
            ),
            (t!("launcher.batch.palette_processing"), vec!["stitch_split", "waifu2x"]),
            (t!("launcher.batch.palette_variables"), vec!["variable_read", "variable_write"]),
            (t!("launcher.batch.palette_flow"), vec!["end"]),
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

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::NodeDefs;
    use crate::launcher::new_project::batch_processing::types::{NodeParams, SocketKind};
    use std::borrow::Cow;

    #[test]
    fn string_template_yields_one_owned_socket_per_placeholder() {
        let defs = NodeDefs::build();
        let params = NodeParams::StringTemplate {
            template: "{a}/{b}/{c}".to_owned(),
            placeholders: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        };
        let sockets = defs.socket_specs_for_node("string_template", &params, &[]);
        // One data-input socket per placeholder (the fixed Вход/Далее/Строка are exec/output).
        let placeholder_inputs: Vec<_> = sockets
            .iter()
            .filter(|s| s.is_input && matches!(s.kind, SocketKind::Data(_)))
            .collect();
        assert_eq!(placeholder_inputs.len(), 3);
        for (spec, name) in placeholder_inputs.iter().zip(["a", "b", "c"]) {
            // The placeholder text is both the wire identifier and the painted label.
            assert_eq!(spec.name.as_ref(), name);
            assert_eq!(spec.display_label(), name);
            assert!(spec.label_key.is_none());
            // Owned, not a leaked `&'static str`: the socket carries its own `String`.
            assert!(matches!(spec.name, Cow::Owned(_)));
        }
        // Calling again returns fresh owned data. Because `SocketSpec::name` is a `Cow`,
        // repeated draw-path calls cannot leak a growing set of `&'static str` (compile-level
        // guarantee); the owned names are freed with the returned `Vec`.
        let again = defs.socket_specs_for_node("string_template", &params, &[]);
        assert_eq!(again.len(), sockets.len());
    }

    #[test]
    fn socket_label_key_resolves_through_active_catalog() {
        use ms_i18n::{Catalog, LocaleTag};
        // The binary's tests share one process-global catalog slot; serialize against the
        // locale-store tests so an install here cannot race their assertions.
        let _guard = crate::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .expect("locale lock");

        let defs = NodeDefs::build();
        // The "end" node's only socket is the fixed exec-in identifier "Вход", labeled by key.
        let sockets = defs.socket_specs_for_node("end", &NodeParams::End, &[]);
        let exec_in = sockets
            .iter()
            .find(|s| s.is_input && matches!(s.kind, SocketKind::Exec))
            .expect("end node has an exec-in socket");
        assert_eq!(exec_in.label_key, Some("launcher.batch.socket.input"));
        // The wire identifier is never localized, whatever the active language.
        assert_eq!(exec_in.name.as_ref(), "Вход");

        let ru = Catalog::from_json_str(
            &LocaleTag::parse("ru").expect("ru tag"),
            r#"{ "launcher.batch.socket.input": "Вход" }"#,
        )
        .expect("ru catalog parses");
        ms_i18n::install(ru);
        assert_eq!(exec_in.display_label(), "Вход");

        let en = Catalog::from_json_str(
            &LocaleTag::parse("en").expect("en tag"),
            r#"{ "launcher.batch.socket.input": "Input" }"#,
        )
        .expect("en catalog parses");
        ms_i18n::install(en);
        assert_eq!(exec_in.display_label(), "Input");
    }
}
