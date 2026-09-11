/*
File: src/launcher/new_project/mod.rs

Purpose:
Detached "New Project" launcher window module.

Main responsibilities:
- expose the standalone window state, ribbon worker logic, and source-open helpers;
- keep the migrated Python-style new-project interface outside the launcher page stack.
*/

pub mod advanced_download;
pub mod batch_processing;
pub mod open_source;
pub mod project_io;
pub mod quick_download;
pub mod reline;
pub mod reline_models;
pub mod ribbon;
pub mod stitching;
#[cfg(feature = "tutorial")]
pub mod tutorial;
pub mod waifu2x;
pub mod window;
