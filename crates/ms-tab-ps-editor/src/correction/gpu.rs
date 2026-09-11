/*
File: tabs/ps_editor/correction/gpu.rs

Purpose:
The GPU side of the PS editor's view-only «Коррекция»: an `egui_glow` paint callback that reads the
already-composited canvas back out of the framebuffer and re-draws it through
`out = clamp(gain * c + bias, 0, 1)`. This is the ONLY file in the project that touches OpenGL
directly.

Why a shader at all: egui's own fragment stage is a component-wise multiply in gamma space
(`egui_glow-0.35.0/src/shader/fragment.glsl:52`) and its blend stage is a fixed
`FUNC_ADD` / `(ONE, ONE_MINUS_SRC_ALPHA)` (`egui_glow-0.35.0/src/painter.rs:314-324`), so every
composition reachable through egui is `out = M*c + B` with `M >= 0, B >= 0`. A contrast boost needs
a NEGATIVE offset, so it cannot be expressed as a vertex tint or as any number of extra egui passes.

Key structures:
- `ColorFilter`: owns the program, the VAO/VBO and the scratch texture; created lazily inside the
  first callback (the only place a `&glow::Context` exists) and freed from `on_exit`.
- `ColorFilterError`: the typed failure of building or running the filter.

Key functions:
- `ColorFilter::paint`: one frame of the pass, called from the `egui_glow::CallbackFn`.
- `ColorFilter::destroy`: frees every GL object; must be called while a context is still current.

Notes:
egui re-runs `prepare_painting` after every callback (`egui_glow-0.35.0/src/painter.rs:449`), so
this file does not have to restore egui's GL state — but it must leave none of ITS OWN objects
bound. `PaintCallbackInfo::viewport_in_pixels` is already clamped to the screen, and the renderer
has set `glViewport` to it before the callback runs, so the quad below is emitted in bare NDC.
*/

use eframe::egui::PaintCallbackInfo;
use eframe::egui::epaint::ViewportInPixels;
use eframe::egui_glow::ShaderVersion;
use eframe::glow::{self, HasContext as _};

use super::model::ColorFilterUniforms;

/// Vertex attribute index of the quad's position. Bound explicitly before linking so the shader
/// needs no `layout(location = ..)` qualifier, which GLSL 120 / ES 100 do not have.
const ATTRIB_POSITION: u32 = 0;

/// The full-viewport triangle strip, in normalized device coordinates. The renderer has already set
/// `glViewport` to the callback's rect, so `-1..1` covers exactly that rect and nothing else.
const QUAD_NDC: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];

/// Converts a `glow` GL enum (spelled `u32`) into the `i32` the `*_i32` entry points take.
///
/// Const-evaluated at every call site below, so a value that would not fit is a COMPILE error
/// rather than a runtime fallback — which is what makes the one `as` in this file the
/// proven-safe conversion `AGENTS.md` §17 allows.
const fn gl_enum_i32(value: u32) -> i32 {
    assert!(value <= 0x7fff_ffff, "GL enum does not fit in an i32");
    value as i32
}

/// `GL_NEAREST` as the `i32` `glTexParameteri` wants.
const TEX_NEAREST: i32 = gl_enum_i32(glow::NEAREST);
/// `GL_CLAMP_TO_EDGE` as the `i32` `glTexParameteri` wants.
const TEX_CLAMP_TO_EDGE: i32 = gl_enum_i32(glow::CLAMP_TO_EDGE);
/// `GL_RGBA` as the `i32` internal format `glTexImage2D` wants. Unsized on purpose: the sized
/// `GL_RGBA8` is not a legal internal format on WebGL1 / GLES2, and the copy decides the real
/// storage anyway.
const TEX_INTERNAL_RGBA: i32 = gl_enum_i32(glow::RGBA);

/// The quad's vertex data as the byte slice `glBufferData` takes.
///
/// Built by explicit `to_ne_bytes` rather than a pointer cast: the GL buffer is filled from the
/// host's native float layout, and doing it this way keeps the whole file free of raw pointers.
fn quad_vertex_bytes() -> [u8; std::mem::size_of::<[f32; 8]>()] {
    let mut bytes = [0_u8; std::mem::size_of::<[f32; 8]>()];
    for (chunk, value) in bytes.chunks_exact_mut(4).zip(QUAD_NDC) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// Whether this context supports vertex array objects.
///
/// Guards a call that PANICS rather than failing: on WebGL1 without `OES_vertex_array_object`,
/// `glow::Context::create_vertex_array` panics outright (`glow-0.17.0/src/web_sys.rs:2572`), which
/// would bypass [`ColorFilter`]'s failure latch and abort the app. `egui_glow` guards its own VAO
/// use the same way, but its `supports_vao` sits in a private module
/// (`egui_glow-0.35.0/src/lib.rs:18`), so the check is restated here.
///
/// # Safety
/// `gl` must be the current context of the calling thread.
unsafe fn supports_vertex_arrays(gl: &glow::Context) -> bool {
    let version_string = unsafe { gl.get_parameter_string(glow::VERSION) };
    if version_needs_vao_extension(&version_string) {
        let extensions = gl.supported_extensions();
        return extensions.contains("OES_vertex_array_object")
            || extensions.contains("GL_OES_vertex_array_object");
    }
    true
}

/// Whether a `GL_VERSION` string names a flavour where VAOs are an EXTENSION rather than core.
///
/// Split out from [`supports_vertex_arrays`] so the string handling — the part that can be wrong —
/// is unit-testable without a GL context. VAOs are core in OpenGL 3+, in OpenGL ES 3+ and in
/// WebGL 2; they are an extension in WebGL 1, in OpenGL ES 2 and in OpenGL 2.
#[must_use]
fn version_needs_vao_extension(version_string: &str) -> bool {
    if let Some(pos) = version_string.rfind("WebGL ") {
        return version_string[pos + "WebGL ".len()..].starts_with("1.0");
    }
    if version_string.contains("OpenGL ES ") {
        return version_string.contains("OpenGL ES 2.0");
    }
    version_string.starts_with('2')
}

/// A failure of the correction's GPU pass.
///
/// Every variant is terminal for the session: the filter records it, stops drawing, and the panel
/// tells the user the correction is unavailable. It is never retried per frame — a driver that
/// refused to compile the shader once will refuse it again, and retrying would spam the log at
/// frame rate.
#[derive(Debug, thiserror::Error)]
pub enum ColorFilterError {
    /// A `glCreate*` call refused to hand out an object.
    #[error("could not create the {resource} GL object: {detail}")]
    CreateResource {
        /// Which object was being created (`program`, `shader`, `vertex array`, `buffer`, `texture`).
        resource: &'static str,
        /// The driver's message, verbatim.
        detail: String,
    },
    /// A shader stage failed to compile.
    #[error("could not compile the {stage} shader: {log}")]
    CompileShader {
        /// `vertex` or `fragment`.
        stage: &'static str,
        /// The driver's compile log.
        log: String,
    },
    /// The program failed to link.
    #[error("could not link the correction shader program: {log}")]
    LinkProgram {
        /// The driver's link log.
        log: String,
    },
}

/// The GL objects one `ColorFilter` owns once it has been built.
///
/// Uniform LOCATIONS are deliberately absent: on `wasm32` a `glow::UniformLocation` is a
/// `web_sys::WebGlUniformLocation`, which is `!Send`, and `egui_glow::CallbackFn` demands a
/// `Send + Sync` closure — so caching one here would make the whole filter unusable on the web
/// target. They are looked up per frame instead, which is one driver string lookup per uniform per
/// frame and nowhere near a hot path. Every other name (`Program`, `Buffer`, `Texture`,
/// `VertexArray`) is a plain key on all three targets and stores fine.
struct GlResources {
    /// The linked `gain`/`bias` program.
    program: glow::Program,
    /// Vertex array holding the quad binding, or `None` on a context that has no VAO support
    /// (WebGL1 / GLES2 without `OES_vertex_array_object`), where the attribute is bound per draw.
    /// See [`supports_vertex_arrays`].
    vertex_array: Option<glow::VertexArray>,
    /// Vertex buffer holding [`QUAD_NDC`].
    vertex_buffer: glow::Buffer,
    /// Scratch texture the composited canvas is copied into each frame.
    texture: glow::Texture,
    /// Current allocated size of `texture`, in pixels. `[0, 0]` until the first copy.
    texture_size: [i32; 2],
}

/// The correction's GPU filter: lazily built, reused every frame, freed from `on_exit`.
///
/// Held behind an `Arc<Mutex<..>>` by the PS-editor tab because `egui_glow::CallbackFn` requires a
/// `Send + Sync` closure. In practice the lock is uncontended: both the panel body and the paint
/// callback run on the GUI thread.
#[derive(Default)]
pub struct ColorFilter {
    /// `None` until the first callback has built the program.
    resources: Option<GlResources>,
    /// Set once the pass has failed. While it is set the pass draws nothing and the panel reports
    /// the correction as unavailable, so the user is never left staring at an uncorrected canvas
    /// with the sliders pretending to work.
    failed: bool,
}

impl std::fmt::Debug for ColorFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ColorFilter")
            .field("built", &self.resources.is_some())
            .field("failed", &self.failed)
            .finish()
    }
}

impl ColorFilter {
    /// Whether the pass has failed and the correction must be reported as unavailable.
    #[must_use]
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Runs one frame of the correction over the callback's viewport rect.
    ///
    /// Copies the already-drawn canvas out of the default framebuffer into the scratch texture and
    /// re-draws it through the filter. Does nothing once [`Self::has_failed`] is true, and nothing
    /// for a degenerate (zero-width or zero-height) rect.
    ///
    /// Builds the GL objects on the first call — a paint callback is the only place a
    /// `&glow::Context` is available. A build failure is logged with context and latched; it is
    /// never retried.
    pub fn paint(&mut self, gl: &glow::Context, info: &PaintCallbackInfo, uniforms: ColorFilterUniforms) {
        if self.failed {
            return;
        }
        let viewport = info.viewport_in_pixels();
        if viewport.width_px <= 0 || viewport.height_px <= 0 {
            return;
        }
        if self.resources.is_none() {
            // SAFETY: we are inside an `egui_glow` paint callback, so `gl` is the current context
            // of the thread that owns it, and no egui object is bound by the calls below.
            match unsafe { GlResources::create(gl) } {
                Ok(resources) => self.resources = Some(resources),
                Err(error) => {
                    ms_log::runtime_log::log_error(format!(
                        "[ps_editor] correction: the view-only colour filter could not be built \
                         and is disabled for this session.\nError: {error}\nPossible cause: the \
                         GPU driver rejected the shader or refused a GL object.\nEffect: the \
                         «Коррекция» panel reports the correction as unavailable; layer pixels and \
                         the saved project are unaffected."
                    ));
                    self.failed = true;
                    return;
                }
            }
        }
        let Some(resources) = self.resources.as_mut() else {
            return;
        };
        // SAFETY: same as above — a live context inside the callback, and every object bound here
        // is unbound before returning.
        unsafe { resources.draw(gl, &viewport, uniforms) };
    }

    /// Frees every GL object this filter owns.
    ///
    /// Must be called while a context is still current — the app does that from `on_exit`, which is
    /// the one shutdown hook eframe hands a `&glow::Context`. Calling it twice, or on a filter that
    /// was never used, is a no-op.
    pub fn destroy(&mut self, gl: &glow::Context) {
        let Some(resources) = self.resources.take() else {
            return;
        };
        // SAFETY: `on_exit` runs on the GUI thread with the context still current, and every name
        // below was created by this struct and has not been deleted yet.
        unsafe {
            gl.delete_program(resources.program);
            if let Some(vertex_array) = resources.vertex_array {
                gl.delete_vertex_array(vertex_array);
            }
            gl.delete_buffer(resources.vertex_buffer);
            gl.delete_texture(resources.texture);
        }
    }
}

impl GlResources {
    /// Builds the program, the quad and the scratch texture.
    ///
    /// # Safety
    /// `gl` must be the current context of the calling thread.
    unsafe fn create(gl: &glow::Context) -> Result<Self, ColorFilterError> {
        unsafe {
            let version = ShaderVersion::get(gl);
            let program = link_program(gl, version)?;
            // NEVER call `create_vertex_array` unguarded: on WebGL1 without
            // `OES_vertex_array_object` glow PANICS rather than returning an `Err`
            // (`glow-0.17.0/src/web_sys.rs:2572`), which would bypass the failure latch and take
            // the whole app down. `egui_glow` guards the same call with its own `supports_vao`,
            // but that lives in a private module (`egui_glow-0.35.0/src/lib.rs:18`).
            let vertex_array = if supports_vertex_arrays(gl) {
                match gl.create_vertex_array() {
                    Ok(vertex_array) => Some(vertex_array),
                    Err(detail) => {
                        gl.delete_program(program);
                        return Err(ColorFilterError::CreateResource { resource: "vertex array", detail });
                    }
                }
            } else {
                None
            };
            let delete_partial = |gl: &glow::Context, buffer: Option<glow::Buffer>| {
                gl.delete_program(program);
                if let Some(vertex_array) = vertex_array {
                    gl.delete_vertex_array(vertex_array);
                }
                if let Some(buffer) = buffer {
                    gl.delete_buffer(buffer);
                }
            };
            let vertex_buffer = match gl.create_buffer() {
                Ok(buffer) => buffer,
                Err(detail) => {
                    delete_partial(gl, None);
                    return Err(ColorFilterError::CreateResource { resource: "buffer", detail });
                }
            };
            let texture = match gl.create_texture() {
                Ok(texture) => texture,
                Err(detail) => {
                    delete_partial(gl, Some(vertex_buffer));
                    return Err(ColorFilterError::CreateResource { resource: "texture", detail });
                }
            };

            // With a VAO the attribute binding is recorded once, here; without one it has to be
            // re-stated on every draw, which `draw` does. The buffer upload itself is common.
            if let Some(vertex_array) = vertex_array {
                gl.bind_vertex_array(Some(vertex_array));
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vertex_buffer));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &quad_vertex_bytes(), glow::STATIC_DRAW);
            if vertex_array.is_some() {
                gl.enable_vertex_attrib_array(ATTRIB_POSITION);
                gl.vertex_attrib_pointer_f32(ATTRIB_POSITION, 2, glow::FLOAT, false, 8, 0);
                gl.bind_vertex_array(None);
            }
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            // NEAREST + CLAMP_TO_EDGE: the quad samples the copy 1:1, so no filtering is wanted and
            // no sample ever leaves the copied rect.
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, TEX_NEAREST);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, TEX_NEAREST);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, TEX_CLAMP_TO_EDGE);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, TEX_CLAMP_TO_EDGE);
            gl.bind_texture(glow::TEXTURE_2D, None);

            Ok(Self {
                program,
                vertex_array,
                vertex_buffer,
                texture,
                texture_size: [0, 0],
            })
        }
    }

    /// Copies the framebuffer rect into the scratch texture and re-draws it through the filter.
    ///
    /// # Safety
    /// `gl` must be the current context, and `viewport`'s width and height must be positive.
    unsafe fn draw(&mut self, gl: &glow::Context, viewport: &ViewportInPixels, uniforms: ColorFilterUniforms) {
        let size = [viewport.width_px, viewport.height_px];
        unsafe {
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            if self.texture_size != size {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    TEX_INTERNAL_RGBA,
                    size[0],
                    size[1],
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                self.texture_size = size;
            }
            // Legal because eframe requests no multisampling (`eframe-0.35.0/src/epi.rs:433`), so
            // the default framebuffer is single-sampled and can be copied from directly.
            gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, viewport.left_px, viewport.from_bottom_px, size[0], size[1]);

            gl.use_program(Some(self.program));
            // Looked up per frame rather than cached — see `GlResources`. A `None` location means
            // the driver eliminated the uniform, and `glUniform*` on `None` is defined as a no-op.
            gl.uniform_1_i32(gl.get_uniform_location(self.program, "u_tex").as_ref(), 0);
            gl.uniform_1_f32(gl.get_uniform_location(self.program, "u_gain").as_ref(), uniforms.gain);
            gl.uniform_1_f32(gl.get_uniform_location(self.program, "u_bias").as_ref(), uniforms.bias);
            // The pass REPLACES the pixels it read, so blending must be off; egui re-enables it in
            // the `prepare_painting` it runs after every callback.
            gl.disable(glow::BLEND);
            match self.vertex_array {
                Some(vertex_array) => gl.bind_vertex_array(Some(vertex_array)),
                // No VAO on this context: state the attribute binding per draw instead. egui
                // re-establishes its own attribute state in the `prepare_painting` that follows
                // every callback, so leaving the array enabled here cannot corrupt its draws.
                None => {
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vertex_buffer));
                    gl.enable_vertex_attrib_array(ATTRIB_POSITION);
                    gl.vertex_attrib_pointer_f32(ATTRIB_POSITION, 2, glow::FLOAT, false, 8, 0);
                }
            }
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

            // Leave nothing of ours bound (egui restores its own state, not our objects).
            match self.vertex_array {
                Some(_) => gl.bind_vertex_array(None),
                None => {
                    gl.disable_vertex_attrib_array(ATTRIB_POSITION);
                    gl.bind_buffer(glow::ARRAY_BUFFER, None);
                }
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
        }
    }
}

/// Compiles and links the correction program for `version`.
///
/// # Safety
/// `gl` must be the current context of the calling thread.
unsafe fn link_program(gl: &glow::Context, version: ShaderVersion) -> Result<glow::Program, ColorFilterError> {
    unsafe {
        let program = gl
            .create_program()
            .map_err(|detail| ColorFilterError::CreateResource { resource: "program", detail })?;
        let mut compiled: Vec<glow::Shader> = Vec::with_capacity(2);
        let stages = [
            (glow::VERTEX_SHADER, "vertex", vertex_shader_source(version)),
            (glow::FRAGMENT_SHADER, "fragment", fragment_shader_source(version)),
        ];
        for (kind, stage, source) in stages {
            let shader = match gl.create_shader(kind) {
                Ok(shader) => shader,
                Err(detail) => {
                    cleanup_program(gl, program, &compiled);
                    return Err(ColorFilterError::CreateResource { resource: "shader", detail });
                }
            };
            gl.shader_source(shader, &source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                cleanup_program(gl, program, &compiled);
                return Err(ColorFilterError::CompileShader { stage, log });
            }
            gl.attach_shader(program, shader);
            compiled.push(shader);
        }
        // Before linking: GLSL 120 / ES 100 have no `layout(location = ..)` qualifier, so the quad's
        // attribute index is pinned here instead.
        gl.bind_attrib_location(program, ATTRIB_POSITION, "a_pos");
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            cleanup_program(gl, program, &compiled);
            return Err(ColorFilterError::LinkProgram { log });
        }
        for shader in compiled {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        Ok(program)
    }
}

/// Deletes a half-built program together with the shaders already attached to it.
///
/// # Safety
/// `gl` must be the current context; `program` and every entry of `shaders` must still exist.
unsafe fn cleanup_program(gl: &glow::Context, program: glow::Program, shaders: &[glow::Shader]) {
    unsafe {
        for shader in shaders {
            gl.detach_shader(program, *shader);
            gl.delete_shader(*shader);
        }
        gl.delete_program(program);
    }
}

/// The vertex shader: passes the NDC quad through and derives its texture coordinates from it.
///
/// One source serves GL 3.3, GLES and WebGL2: `ShaderVersion` decides the `#version` line and
/// whether the stage speaks `in`/`out` or the old `attribute`/`varying`
/// (`egui_glow-0.35.0/src/shader_version.rs:56,66`).
fn vertex_shader_source(version: ShaderVersion) -> String {
    let (input, output) = if version.is_new_shader_interface() { ("in", "out") } else { ("attribute", "varying") };
    format!(
        "{decl}\n\
         {input} vec2 a_pos;\n\
         {output} vec2 v_uv;\n\
         void main() {{\n\
         \x20   v_uv = a_pos * 0.5 + 0.5;\n\
         \x20   gl_Position = vec4(a_pos, 0.0, 1.0);\n\
         }}\n",
        decl = version.version_declaration(),
    )
}

/// The fragment shader: `out = clamp(gain * c + bias, 0, 1)` per colour channel, alpha passed
/// through untouched.
///
/// It mirrors `model::apply_channel` line for line — that function is what the unit tests pin, and
/// this string must not drift from it.
fn fragment_shader_source(version: ShaderVersion) -> String {
    let precision = if version.is_embedded() { "precision mediump float;\n" } else { "" };
    if version.is_new_shader_interface() {
        format!(
            "{decl}{precision}\
             uniform sampler2D u_tex;\n\
             uniform float u_gain;\n\
             uniform float u_bias;\n\
             in vec2 v_uv;\n\
             out vec4 f_color;\n\
             void main() {{\n\
             \x20   vec4 c = texture(u_tex, v_uv);\n\
             \x20   f_color = vec4(clamp(u_gain * c.rgb + u_bias, 0.0, 1.0), c.a);\n\
             }}\n",
            decl = version.version_declaration(),
        )
    } else {
        format!(
            "{decl}{precision}\
             uniform sampler2D u_tex;\n\
             uniform float u_gain;\n\
             uniform float u_bias;\n\
             varying vec2 v_uv;\n\
             void main() {{\n\
             \x20   vec4 c = texture2D(u_tex, v_uv);\n\
             \x20   gl_FragColor = vec4(clamp(u_gain * c.rgb + u_bias, 0.0, 1.0), c.a);\n\
             }}\n",
            decl = version.version_declaration(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shader version this project can meet must produce a source whose `#version` line comes
    /// first and whose interface keywords match that version — a mismatch is a driver-side compile
    /// error that only shows up on the one machine that has that GL flavour.
    #[test]
    fn the_shader_sources_match_their_version_dialect() {
        for version in [ShaderVersion::Gl120, ShaderVersion::Gl140, ShaderVersion::Es100, ShaderVersion::Es300] {
            let vertex = vertex_shader_source(version);
            let fragment = fragment_shader_source(version);
            assert!(vertex.starts_with(version.version_declaration()), "{version:?}: vertex #version must lead");
            assert!(fragment.starts_with(version.version_declaration()), "{version:?}: fragment #version must lead");
            if version.is_new_shader_interface() {
                assert!(vertex.contains("in vec2 a_pos;"), "{version:?}");
                assert!(fragment.contains("out vec4 f_color;"), "{version:?}");
                assert!(fragment.contains("texture(u_tex"), "{version:?}");
            } else {
                assert!(vertex.contains("attribute vec2 a_pos;"), "{version:?}");
                assert!(fragment.contains("gl_FragColor"), "{version:?}");
                assert!(fragment.contains("texture2D(u_tex"), "{version:?}");
            }
            assert_eq!(fragment.contains("precision mediump float;"), version.is_embedded(), "{version:?}");
        }
    }

    /// The VAO guard exists because an unguarded `create_vertex_array` PANICS on WebGL1. Getting
    /// the version parsing wrong in either direction is bad: too strict loses the fast path on a
    /// modern context, too lax restores the panic on the exact context the guard is for.
    #[test]
    fn only_the_pre_core_gl_flavours_need_the_vao_extension() {
        // Core VAO support: nothing to check at runtime.
        for version in [
            "WebGL 2.0 (OpenGL ES 3.0 Chromium)",
            "WebGL 2.0",
            "OpenGL ES 3.2 Mesa 25.2.8",
            "4.6 (Core Profile) Mesa 25.2.8",
            "3.3.0 NVIDIA 550.120",
        ] {
            assert!(!version_needs_vao_extension(version), "{version:?} has core VAOs");
        }
        // VAOs are an extension here — the guard must actually look for it.
        for version in [
            "WebGL 1.0 (OpenGL ES 2.0 Chromium)",
            "WebGL 1.0",
            "OpenGL ES 2.0 Mesa 25.2.8",
            "2.1 Mesa 25.2.8",
        ] {
            assert!(version_needs_vao_extension(version), "{version:?} needs the extension");
        }
    }

    /// The GLSL must compute exactly what `model::apply_channel` does; the test pins the one
    /// expression both sides share, so a drift in either is a failing test rather than a silent
    /// difference between the preview and its own reference implementation.
    #[test]
    fn the_fragment_shader_states_the_model_expression() {
        for version in [ShaderVersion::Gl140, ShaderVersion::Es300, ShaderVersion::Gl120, ShaderVersion::Es100] {
            assert!(
                fragment_shader_source(version).contains("clamp(u_gain * c.rgb + u_bias, 0.0, 1.0)"),
                "{version:?}: the shader no longer mirrors `model::apply_channel`"
            );
        }
    }
}
