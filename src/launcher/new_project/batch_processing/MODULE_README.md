# Module: src/launcher/new_project/batch_processing

## Purpose
Standalone visual batch-processing graph editor used from the launcher "New Project" window. It lets
users compose repeated download, browser, stitch/split, waifu2x, variable, template, and save steps
without opening a project.

## Architecture
The module is split between an egui editor and a worker-thread executor. `window.rs` owns the graph
window state, toolbar, palette, variables panel, canvas, save/load dialogs, and executor polling.
`canvas.rs` draws the graph directly with `egui::Painter` and emits structural actions such as
socket connection and node deletion. `graph.rs`, `node_defs.rs`, and `types.rs` define the typed
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
- `executor.rs`: worker-thread executor, graph snapshot types, exec/data propagation, cancellation,
  browser JSON-RPC daemon lifecycle, quick image download, folder save, stitch/split, and waifu2x
  node execution.
- `window.rs`: batch window root state, toolbar, palette and variables panels, graph save/load,
  run/stop controls, executor event polling, and canvas action handling.

## Contracts and invariants
- The GUI thread must only render, mutate the in-memory graph, and poll executor events. Network
  requests, browser automation, image processing, file saving, and waifu2x execution stay in
  `executor.rs` or reused worker-safe helpers.
- `GraphSnapshot` is the boundary between UI state and execution. The executor must not borrow or
  mutate the live `GraphModel`.
- Graph JSON remains version `1` and compatible with the Python format. Socket names are persisted
  user-facing strings, so renaming them requires a migration or explicit compatibility handling.
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
- To change pan/zoom, node interaction, socket hit testing, or embedded parameter UI, edit
  `canvas.rs`.
- To change run/stop UX, palette layout, variable editing, or graph file dialogs, edit `window.rs`.
- To change pipeline semantics, browser automation, quick download, save folder, stitch/split, or
  waifu2x execution, edit `executor.rs` and the reused controller/helper module when behavior is
  shared with the main new-project window.
