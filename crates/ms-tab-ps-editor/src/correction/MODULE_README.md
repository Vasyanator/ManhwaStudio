# Module: crates/ms-tab-ps-editor/src/correction

## Purpose
The «Коррекция» panel of the PS editor: a **view-only** correction of what the canvas shows, so
the user can see small colour differences. It is a viewing aid of the same family as «Сглаживание»
and «Сетка пикселей», not an image operation.

**Nothing here writes pixels.** Layer buffers, the shared `LayerDoc`, `layers.json`,
`CleanOverlaysModel` and the saved project are all unaffected, and the correction is not persisted:
it lives for one session, exactly like the two view toggles it sits beside.

## Architecture

```
PsEditorTabState.correction : CorrectionState          (kind + parameters, edited by the panel)
PsEditorTabState.correction_filter : Arc<Mutex<ColorFilter>>   (the GL objects, shared with the callback)

panel body (ui.rs)  -> CorrectionState
draw_canvas         -> CorrectionState::active_uniforms()  -> Option<ColorFilterUniforms>
                    -> egui::Shape::Callback(egui_glow::CallbackFn) over page_rect ∩ canvas rect
callback (gpu.rs)   -> copy the drawn framebuffer rect into a scratch texture
                    -> re-draw it as one quad through `out = clamp(gain * c + bias, 0, 1)`
MangaApp::on_exit   -> ColorFilter::destroy(gl)
```

### Why a GPU pass and not an egui tint
egui's fragment stage is a component-wise multiply in gamma space
(`egui_glow-0.36.2/src/shader/fragment.glsl:52`) and its blend stage is a fixed `FUNC_ADD` with
`(ONE, ONE_MINUS_SRC_ALPHA)` (`egui_glow-0.36.2/src/painter.rs:314-324`). Every composition egui
can reach is therefore `out = M*c + B` with `M >= 0` and `B >= 0`. Contrast pivoted on mid-grey
needs a **negative** offset, so it is unreachable by any number of egui draws — a shader is the
only mechanism, and it is the mechanism the user chose.

### Why the linear (legacy-style) brightness/contrast
The model folds into exactly two uniforms, so the GLSL is a single expression that cannot drift
away from the Rust the unit tests cover. Photoshop's modern CS3+ curve is piecewise and would move
the real maths into a shader nothing in this repository can test, for fidelity a "spot the small
difference" viewing aid does not need. `model.rs::uniforms` states the same in its doc comment.

## Files and submodules
- `model.rs`: `CorrectionKind`, `Correction`, `CorrectionState`, `ColorFilterUniforms`,
  `apply_channel`. **GUI-free and GL-free** — it must not gain an `egui` or `glow` dependency. All
  the unit tests live here.
- `gpu.rs`: `ColorFilter` (program + VAO/VBO + scratch texture) and `ColorFilterError`. The only
  file in the project that calls OpenGL. Edit it when the shader or the GL lifetime changes.
- `ui.rs`: `correction_panel_body` (the «Настройка» section) and `correction_card_controls` (the
  per-kind card). Edit it when the panel grows a section or a kind gains parameters.

## Contracts and invariants
- **View-only.** No code path here may write a layer, the doc, the manifest or the clean overlays.
- **Neutral is free.** `CorrectionState::active_uniforms` returns `None` for «Нет» and for exactly
  neutral parameters, so an untouched panel costs the canvas nothing at all.
- **The shader mirrors `apply_channel`.** `gpu.rs`'s GLSL and `model.rs::apply_channel` compute the
  same expression; a unit test asserts the shader source still states it.
- **One shader source, three targets.** `egui_glow::ShaderVersion` picks the `#version` line and the
  `in`/`out` vs `attribute`/`varying` dialect, so GL 3.3, GLES and WebGL2 share one source.
- **`create_vertex_array` is NEVER called unguarded.** On WebGL1 without `OES_vertex_array_object`
  glow PANICS instead of returning an `Err` (`glow-0.17.0/src/web_sys.rs:2572`), which would bypass
  the failure latch and abort the app — and that context is reachable through eframe's WebGL2 →
  WebGL1 fallback. `supports_vertex_arrays` guards it, and a context without VAOs gets the
  attribute binding re-stated per draw instead. `egui_glow` guards its own VAO use the same way but
  keeps `supports_vao` private, so the check is restated here; its version-string half is a pure,
  unit-tested function.
- **GL lifetime.** The objects are created lazily inside the first paint callback (the only place a
  `&glow::Context` exists) and freed from `MangaApp::on_exit`, which is the one shutdown hook eframe
  hands a context. A `ColorFilter` dropped without `destroy` leaks its GL names for the remaining
  life of the context.
- **Failure is loud.** A shader that will not compile disables the pass for the session, logs the
  driver's message with context, and makes the panel say the correction is unavailable. It never
  silently draws nothing.
- **The card takes no host state.** `correction_card_controls(ui, &mut Correction) -> bool` is the
  reusable unit: a second host (the «Пресеты» section planned next) renders the same controls over
  its own `Correction`. Do not add host parameters to it.
- **No literal user-visible strings.** Every caption is a `ps_editor.correction.*` key present in
  all five `crates/ms-i18n/locales/*.json` catalogs.

## Editing map
- To change the maths or add a parameter, see `model.rs` (and its tests, which pin the contract).
- To add a correction kind: a `CorrectionKind` variant + its `ALL` entry and `title()` arm, a
  `Correction` variant, a `uniforms()` arm, and a `correction_card_controls` arm. Every `match` is
  exhaustive, so the compiler lists the sites.
- To change what the pass covers, see `PsEditorTabState::draw_canvas` in `../mod.rs`: the callback
  is inserted between `draw_composite` and the pixel-grid pass, clipped to `page_rect ∩ canvas rect`.
- To change the GL lifetime, see `ColorFilter::destroy` and `MangaApp::on_exit` (`src/app.rs`).
