/*
File: src/main.rs

Purpose:
Thin process entry of the `manhwastudio_rs` binary. Everything else — the module tree, the
`crate::x` shims, CLI parsing and the whole startup flow — lives in the package's library
(`src/lib.rs`), so a platform build that needs a library target (Android) reaches the same code.

Key functions:
- main() (native): `manhwastudio_rs::native_main()`; an `Err` becomes a non-zero exit code.
- main() (wasm32): `manhwastudio_rs::web_main()`.

Notes:
The Windows `windows_subsystem` attribute must stay here: it only takes effect in the root of a
binary crate (no console window unless the `active-logs` feature is on).
*/
#![cfg_attr(
    all(target_os = "windows", not(feature = "active-logs")),
    windows_subsystem = "windows"
)]

/// Web entry: hands over to the library's eframe `WebRunner` boot.
#[cfg(target_arch = "wasm32")]
fn main() {
    manhwastudio_rs::web_main();
}

/// Native entry: runs the library's startup flow; a returned error is reported by the standard
/// `Termination` impl and exits non-zero.
#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    manhwastudio_rs::native_main()
}
