/*
File: src/launcher/pages/mod.rs

Purpose:
Launcher page module tree.

Main responsibilities:
- expose shared page shell/animation helpers;
- register concrete launcher pages rendered inside the animated page stack;
- register the settings page's System registration tab (Windows and Linux only: the OS
  records it shows exist there alone);
- register the settings page's System info tab (native only: its probes and the local AI
  runtimes it reports do not exist on web).
*/

pub mod base;
pub mod export_page;
pub mod import_page;
pub mod open_page;
pub mod settings_page;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod system_info;
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod system_registration;
