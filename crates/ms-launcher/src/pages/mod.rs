/*
File: src/launcher/pages/mod.rs

Purpose:
Launcher page module tree.

Main responsibilities:
- expose shared page shell/animation helpers;
- register concrete launcher pages rendered inside the animated page stack;
- register the settings page's System registration tab (Windows and Linux only: the OS
  records it shows exist there alone).
*/

pub mod base;
pub mod export_page;
pub mod import_page;
pub mod open_page;
pub mod settings_page;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod system_registration;
