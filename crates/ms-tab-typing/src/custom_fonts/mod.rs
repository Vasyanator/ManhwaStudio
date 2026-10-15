/*
File: crates/ms-tab-typing/src/custom_fonts/mod.rs

Purpose:
The typing tab's runtime side of custom fonts (recipes compiled by `ms-custom-fonts`): the
identity namespace, the source-resolution seam, the process-global compile service with its
memory LRU and on-disk cache, and the ad-hoc recipe interner, and the PSD export's face resolution. See `MODULE_README.md`.
*/

pub(crate) mod identity;
pub(crate) mod adhoc;
pub(crate) mod disk_cache;
pub(crate) mod export;
pub(crate) mod service;
pub(crate) mod sources;
