/*
File: src/launcher/new_project/batch_processing/mod.rs

Purpose:
Module root for the node-based batch processing system.

Main responsibilities:
- Re-export the public API surface used by new_project/window.rs
- Declare submodule visibility

Key submodules:
- types     — shared data types (DataType, DataValue, NodeParams, …)
- graph     — graph model + JSON serialisation (GraphModel, GraphEdge, …)
- node_defs — static node definitions (sockets, palette metadata)
- canvas    — egui Painter-based interactive canvas
- executor  — background pipeline executor
- window    — BatchProcessingWindowState (main UI entry point)
*/

pub mod canvas;
pub mod executor;
pub mod graph;
pub mod node_defs;
pub mod types;
pub mod window;

pub use window::BatchProcessingWindowState;
