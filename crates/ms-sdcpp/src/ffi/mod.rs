/*
File: crates/ms-sdcpp/src/ffi/mod.rs

Purpose:
The hand-written bindings to the stable-diffusion.cpp C API of the pinned release:
`types` (field-exact `#[repr(C)]` mirrors of the vendored header), `api` (loading the ggml
and sd.cpp libraries of an installed package, resolving every symbol, the ABI check) and
`layout_tests` (the measured C layout and the header hash).

Notes:
Crate-private: only the sd.cpp worker process (which runs the library out of the app's
process) uses it. The library is never loaded into the app process itself.
*/

pub(crate) mod api;
pub(crate) mod types;

#[cfg(test)]
mod layout_tests;
