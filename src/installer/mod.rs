/*
File: mod.rs

Purpose:
Defines the installer module boundary.

Main responsibilities:
- expose the installer UI and service flows used by application startup;
- keep installer backend helpers private to the installer module.
*/

pub mod install;
pub mod update;
pub(crate) mod utils;
