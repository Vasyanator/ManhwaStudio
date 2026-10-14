/*
File: src/shell/gl.rs

Purpose:
The main window's GPU backend for the `egui-shader-layers` library (the PS editor's
«Коррекция» shader pass): `install` from the eframe app creator, the only place the
`glow::Context` exists before the first frame. The matching `destroy_glow` is the shell's
`on_exit` (`mod.rs`).

Key functions:
- `install(cc)`

Notes:
The body only uses cross-target APIs (`eframe::CreationContext::gl`, `egui_shader_layers`), but
the module is gated native with the shell host because its one caller is the native shell; the
web entry (`src/web_entry.rs`) still installs the backend itself with console logging.
*/

use crate::runtime_log;

/// Installs the `egui-shader-layers` glow backend on the main window's egui context.
///
/// Called once from the eframe app creator, the only place the `glow::Context` is available
/// before the first frame. Failure never aborts startup: the library registers a failed backend,
/// the PS editor's «Коррекция» panel then reports the correction as unavailable, and the cause is
/// logged here with the GL context's version. The matching teardown is the shell's `on_exit`.
pub(crate) fn install(cc: &eframe::CreationContext<'_>) {
    let Some(gl) = cc.gl.as_deref() else {
        runtime_log::log_error(
            "[shell] eframe created the main window without a glow context; shader layers (the PS \
             editor's «Коррекция») are unavailable. Possible cause: a non-glow renderer",
        );
        return;
    };
    if let Err(error) = egui_shader_layers::install_glow(&cc.egui_ctx, gl) {
        use eframe::glow::HasContext as _;
        let version = gl.version();
        runtime_log::log_error(format!(
            "[shell] shader layers disabled: {error}. GL context: {}.{} (embedded: {}). \
             The PS editor's «Коррекция» panel reports the correction as unavailable",
            version.major, version.minor, version.is_embedded
        ));
    }
}
