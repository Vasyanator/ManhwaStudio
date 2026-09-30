# Module: crates/ms-launcher/src/new_project/batch_processing

## Purpose
Standalone visual batch-processing graph editor used from the launcher "New Project" window. It lets
users compose repeated download, browser, stitch/split, waifu2x, variable, template, and save steps
without opening a project.

## Architecture
The module is split between an egui editor and a worker-thread executor. `window.rs` owns the graph
window state, toolbar, palette, variables panel, canvas, save/load dialogs, and executor polling.
`canvas.rs` draws the graph directly with `egui::Painter` and emits structural actions such as
socket connection, node deletion, and a request to open a node's path dialog. `graph.rs`, `node_defs.rs`, and `types.rs` define the typed
model, sockets, node registry, parameters, runtime values, and JSON compatibility format.

Execution starts when `window.rs` snapshots the live `GraphModel` into `executor::GraphSnapshot` and
calls `spawn_executor`. The worker evaluates start nodes cycle-by-cycle, propagates data edges,
queues exec edges, waits for required join inputs, and streams `ExecutorEvent` progress back to the
UI. Browser nodes lazily start the Python Selenium helper through `python_manager`, consume its
startup `ready` event, and talk JSON-RPC over stdio for the duration of the run.

## Files and submodules
- `mod.rs`: module declarations and public `BatchProcessingWindowState` re-export.
- `types.rs`: `DataType`, `SocketKind`, `SocketSpec`, `DataValue`, `BrowserKind`, and typed
  `NodeParams` variants for supported node kinds.
- `node_defs.rs`: `NodeDefs` registry, palette metadata, socket layouts, and dynamic sockets for
  template and variable nodes.
- `graph.rs`: `GraphModel`, `GraphNode`, `GraphEdge`, `GraphVariable`, connection validation, graph
  mutation helpers, and version-1 JSON serialization/deserialization compatible with the Python
  graph format.
- `canvas.rs`: painter-based node canvas with pan, zoom, node dragging, selection, socket hit
  testing, Bezier connection drawing, embedded node parameter controls, and `CanvasAction` events.
  Also owns the canvas transform, the canvas ground and grid, the canvas palette, and the
  canvas-local node z-order; see "Canvas interaction model" below before changing any gesture or
  any colour. Opens no dialog and touches no filesystem — see "File dialogs" below.
- `executor.rs`: worker-thread executor, graph snapshot types, exec/data propagation, cancellation,
  browser JSON-RPC daemon lifecycle, quick image download, folder save, stitch/split, and waifu2x
  node execution.
- `window.rs`: batch window root state, toolbar, palette and variables panels, graph save/load,
  run/stop controls, executor event polling, canvas action handling, and the single owner of
  every file dialog and filesystem access of this window.

## File dialogs
- **Only `window.rs` opens a dialog or touches the filesystem, and never on the GUI thread.**
  A native dialog blocks until answered and a graph read/write is I/O, both forbidden on the
  GUI thread (`CLAUDE.md` §5). `spawn_graph_save` / `spawn_graph_load` / `spawn_node_path_pick`
  run on a worker and answer with one `FileTaskEvent`; `poll_file_task` drains that channel
  once per frame and is what applies the result to the model. The window holds **at most one**
  such task (`file_task_rx`): the Save/Load buttons disable while it is in flight and a node's
  "..." request is dropped with a log line, so two native dialogs can never stack.
- **A node's path parameter is picked through `CanvasAction::PickNodePath`.** The canvas runs on
  the paint path, so its "..." button only emits the request, naming the parameter with
  `NodePathPurpose` rather than borrowing it. By the time the dialog answers, the node may have
  been deleted or retyped; `apply_picked_node_path` therefore re-checks the parameter and
  refuses a stale result instead of writing it into whatever the node became.
- egui repaints on demand, so `poll_file_task` re-arms a `request_repaint_after` while a task is
  pending; without it a finished dialog would sit unread until the user moved the mouse.
- **A task never outlives the window session that started it.** The parent keeps this state
  struct alive between openings, so `on_window_closed` — called on BOTH close paths — drops the
  in-flight receiver. The worker's `send` then fails (already handled as a log line) instead of
  parking a result that the next opening would apply to a different graph, and the reopened
  window is not stuck showing the "dialog open" spinner with Save/Load disabled.
- On the web build there is no `rfd`, so `pick_path` answers with a ready-made
  `FileTaskEvent::Failed` carrying a `*_web_unsupported` message: the caller has one code path
  and the missing capability is reported rather than silently dropped. The filesystem half is
  not gated — a save with a remembered `save_path` skips the dialog entirely and still reaches
  `std::fs::write`, reporting `save_file_error` if that fails.

## Widgets
- Node parameter editors and the variables panel use the project's `Wheel*` widgets
  (`WheelComboBox`, `WheelSpinBox`); stock `egui::ComboBox` / `DragValue` / `Slider` are
  forbidden in product UI (`egui-docs/04-widgets.md` §0.2). They are sized from the ambient
  `Style`, which is what lets them follow the node body's zoom scaling.
- Every widget whose label is localized carries a stable `id_salt` so its state survives a
  language switch (`egui-docs/05-ids-and-i18n.md` §2). The palette's collapsing groups have no
  stable key in `node_defs.rs`, so they are salted with the group's FIRST node template key —
  template keys are the English node identity and are unique per group
  (`palette_group_id_salt`).

## Canvas interaction model
- **One owner for the canvas transform.** `CanvasState::world_to_screen` / `screen_to_world` /
  `world_to_screen_len` / `world_to_screen_size` / `screen_to_world_size` are the only places
  that may apply `pan`/`zoom` to a MODEL-space position or extent — a node position, a node
  size, a node drag delta, a grid coordinate, the spawn point and its stagger, the
  pointer-anchored zoom. The convention is `screen = canvas_origin + pan + world * zoom`, with
  `canvas_origin` the canvas rect's top-left as a vector.
  The exemption, and it is the whole exemption: a constant declared so it keeps a fixed
  APPARENT size — a font size, a margin, a corner radius, a painted stroke width, the socket
  radius — is per-widget `Style`/decoration scaling (`apply_zoomed_node_style`,
  `socket_hit_rect`) and multiplies by `zoom` where it is used. If a quantity comes out of the
  model or out of egui's layout, it goes through an owner function.
- **Every gesture comes from an egui `Response`, never from raw pointer hit-testing.** Each node
  body is registered with `Ui::interact` *before* its inner widgets are drawn, because egui
  breaks same-layer hit-test ties in favour of the LAST registered widget; registering the body
  first hands the pointer to the node's own widgets, and to the socket rects registered after
  them. Popups live in a higher `Order`, so egui drops the whole canvas layer beneath them and
  an open combo list can no longer drag the node under it. The one remaining raw pointer read
  is the zoom anchor position, taken only after `Response::contains_pointer()` has answered the
  occlusion question.
- **Wheel-zoom is read AFTER the node pass and gated on `ms_widgets::combo_popup_open`.** The
  `Wheel*` widgets inside a node step from the raw `Event::MouseWheel` stream and only then zero
  `smooth_scroll_delta`, so a read placed before they are drawn can never see that consumption —
  one notch over a node's numeric field would both step the value and zoom the canvas. The gate
  is mandatory for any canvas reading the raw wheel delta (`ms-widgets/src/wheel_input_guard.rs`;
  same shape in `ms-tab-page-manager/src/split.rs` and `crop.rs`), because an open combo list
  owns the wheel and its popup layer consumes nothing on the canvas's behalf. Consequence: a new
  scale lands on the NEXT frame, the same one-frame convention node-captured pan already follows,
  and `request_repaint` keeps that at exactly one frame.
- **One owner for socket geometry.** `socket_hit_rect` defines both where a socket is and how
  big its hit target is. Do not re-derive a second region for exclusion checks.
- **Node z-order is canvas-local.** `CanvasState::node_order` drives paint order and hit
  priority, and clicking a node raises it there. `GraphModel::nodes` order is graph data (it is
  part of the saved JSON) and must never be reshuffled for the UI.
- **Node height has one owner: the parameter editor.** Node rects are measured by egui and
  cached per node id in world units for the next frame (`CanvasState::estimated_node_world_size`),
  with a node-kind-independent estimate on the first frame only. Do not reintroduce a per-kind
  row-count table — the previous one silently drifted from what the editor drew. The cache is
  evicted against the live node set every frame, so it stays bounded by the graph.
- **Edges paint behind nodes.** One shape slot per edge is reserved with `Painter::add` before
  the node pass and filled with `Painter::set` afterwards, once socket positions are known. The
  in-progress rubber-band connection is added after the node pass and therefore stays on top.
- **Keyboard shortcuts are gated on `Context::egui_wants_keyboard_input()`**, because `TextEdit`
  consumes no key events in egui 0.36 (it reads `InputState::filtered_events` without consuming,
  `egui-0.36.2/src/widgets/text_edit/builder.rs:1098`).
- **The canvas owns its ground, and the grid is anchored in graph space.** `visuals.panel_fill`
  is transparent launcher-wide, so the canvas fills its own rect (`COL_GROUND`) before painting
  anything; nothing else supplies a background for it. The grid is then drawn from WORLD-space
  line indices derived from the visible world rect and pushed through the transform owner, which
  is what keeps a node on the same intersection under any pan and zoom, and what bounds the
  per-frame line count to the viewport instead of to `|pan| / step`. A line's position comes
  only from `CanvasState::grid_line_screen_x` / `_y` — `draw_grid` decides WHICH lines to emit,
  never WHERE — and indices are bounded by `GRID_INDEX_LIMIT` (2^24, where `index as f32` stops
  being exact), past which no grid is painted at all. Two levels (fine every
  `GRID_STEP` world units, coarse every `GRID_COARSE_MULTIPLE` fine cells) with the fine level
  ramped out by screen spacing so a zoom gesture never pops a whole level. Stroke width stays
  1 point at every zoom — scaling it thickens the grid as the user zooms in.
- **Grid and canvas colours are opaque and never white-alpha.** `Color32` is premultiplied, so a
  bright tint behind a low alpha is additive: the fine level fades by lerping toward the ground,
  not by alpha. Shared neutral tones come from `crate::theme`; a canvas-local colour is declared
  only where the theme has no equivalent and says why. `DataType`/`SocketKind` colours stay in
  `types.rs` — they are type information, not styling.
- **New nodes are placed through the transform owner too.** `window.rs` asks
  `CanvasState::visible_spawn_origin()` for the world point to spawn at rather than using a fixed
  world coordinate, so a node added after panning still lands on screen.

## Contracts and invariants
- The GUI thread must only render, mutate the in-memory graph, and poll worker events. Network
  requests, browser automation, image processing, file saving, and waifu2x execution stay in
  `executor.rs` or reused worker-safe helpers; file dialogs and graph file I/O stay in
  `window.rs`'s file-task workers (see "File dialogs").
- `GraphSnapshot` is the boundary between UI state and execution. The executor must not borrow or
  mutate the live `GraphModel`.
- Graph JSON remains version `1` and compatible with the Python format. Socket `name` fields
  (`"Вход"`, `"Далее"`, `"Индекс"`, `"Строка"`, `"Картинки"`, `"Путь"`, `"Значение"`) are graph JSON
  keys resolved by name (`socket_spec`), so they are **wire identifiers, not UI labels**: they must
  stay raw Russian literals and must never be routed through the i18n `t!`/`tf!` catalog (see
  `docs/i18n_exclusions.md` §A2). Renaming them requires a migration or explicit compatibility
  handling. The painted label is a **separate** field: `SocketSpec.label_key: Option<&'static str>`
  (a `launcher.batch.socket.*` catalog key), resolved by `SocketSpec::display_label()` and painted in
  `canvas.rs`. Fixed template sockets carry a key and are localized; genuinely user-authored sockets
  (`string_template` placeholders) have `label_key: None` and paint their raw `name`. `SocketSpec.name`
  is a `Cow<'static, str>` so dynamic sockets own their name instead of leaking a `&'static str` on the
  per-frame draw path. By contrast node titles/descriptions and palette-group names (`types.rs`,
  `node_defs.rs`) are display-only — node identity is the English `template_key()` — and connection/load
  error messages (`graph.rs`) are localized message values; both go through `t!`/`tf!`.
- Connections must be validated through `GraphModel::add_edge`; direction, exec/data kind, data
  type, and single-input fan-in rules must not be bypassed.
- `NodeParams` is the typed source of truth for node behavior. Do not infer a node's capabilities
  from display labels or filenames.
- Large image lists use shared ownership (`Arc`) to avoid hidden full-image clones during execution.
- Cancellation is cooperative through the shared stop flag; long node handlers should check it at
  practical boundaries and return `ExecutorEvent::Cancelled` through the worker path.
- Browser nodes must use `python_manager` and the existing Selenium helper protocol, including the
  startup `ready` handshake; do not start ad hoc Python or browser commands from UI code.

## Editing map
- To add a new node kind, update `NodeParams` in `types.rs`, the registry in `node_defs.rs`, JSON
  conversion in `graph.rs` if needed, UI controls in `canvas.rs`, and execution in `executor.rs`.
- To change graph persistence or compatibility with Python files, edit `graph.rs` and check
  `window.rs` save/load handling.
- To change connection rules or graph mutation, edit `graph.rs`.
- To change pan/zoom, node interaction, socket hit testing, embedded parameter UI, the canvas
  ground, the grid, or any canvas colour, edit `canvas.rs` — and read "Canvas interaction model"
  above first; each of those has exactly one owner there.
- To change where a newly created node appears, edit `CanvasState::visible_spawn_origin`
  (`canvas.rs`) and `BatchProcessingWindowState::next_spawn_pos` (`window.rs`).
- To change run/stop UX, palette layout, variable editing, or graph file dialogs, edit `window.rs`.
- To change pipeline semantics, browser automation, quick download, save folder, stitch/split, or
  waifu2x execution, edit `executor.rs` and the reused controller/helper module when behavior is
  shared with the main new-project window.
