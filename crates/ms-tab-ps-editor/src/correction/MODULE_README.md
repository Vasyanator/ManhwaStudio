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

panel body (ui.rs)  -> CorrectionState ; CorrectionAvailability::check(ctx) -> «unavailable» notice
draw_canvas         -> CorrectionState::active_params()  -> Option<BrightnessContrastParams>
                    -> shader::paint_correction_layer: egui_shader_layers::ShaderLayer over
                       page_rect ∩ canvas rect, chain = presets::brightness_contrast(offset, gain)
binary (studio_bootstrap) -> install_glow at window creation, destroy_glow in on_exit
```

The GPU work is done by the `egui-shader-layers` crate (crates.io, glow backend only): its layer
captures what is already painted beneath its rect and redraws it through the preset's WGSL. This
directory owns no GL object and calls no GL function.

### Why a GPU pass and not an egui tint
egui's fragment stage is a component-wise multiply in gamma space
(`egui_glow-0.36.2/src/shader/fragment.glsl:50`) and its blend stage is a fixed `FUNC_ADD` with
`(ONE, ONE_MINUS_SRC_ALPHA)` (`egui_glow-0.36.2/src/painter.rs:314-324`). Every composition egui
can reach is therefore `out = M*c + B` with `M >= 0` and `B >= 0`. Contrast pivoted on mid-grey
needs a **negative** offset, so it is unreachable by any number of egui draws — a shader is the
only mechanism.

### Why the preset maths equals the model
`presets::brightness_contrast(brightness, contrast)` computes
`clamp((c - 0.5) * contrast + 0.5 + brightness, 0, 1)` on unpremultiplied colour. With
`contrast = contrast_gain` and `brightness = brightness_offset` that is the legacy linear model
`gain * c + bias` with `bias = 0.5 - 0.5 * gain + brightness_offset` (the test
`the_preset_form_equals_the_linear_gain_bias_model` pins the identity). It is EXACT on screen only
because the framebuffer alpha under the layer is 1: `draw_canvas` pre-fills the canvas rect
opaque and the page checkerboard is opaque, so the preset's unpremultiply is a no-op there. The
old hand-written shader worked on premultiplied colour; the two would differ only over
translucent framebuffer pixels, which the layer's rect never covers. The linear model (not
Photoshop's piecewise CS3+ curve) is kept because a stock preset renders it and `apply_channel`
states it in testable Rust.

## Files and submodules
- `model.rs`: `CorrectionKind`, `Correction`, `CorrectionState`, `BrightnessContrastParams`,
  `apply_channel`. **GUI-free, GL-free and shader-library-free** — it must not gain an `egui`,
  `glow` or `egui-shader-layers` dependency. The maths tests live here.
- `shader.rs`: `correction_pass` / `paint_correction_layer` (the preset as a `ShaderLayer`) and
  `CorrectionAvailability` (the library's `status` + `failed_effects`, with the one-shot log latch). Edit it when the pass
  changes shape (another preset, a chain, a mask).
- `ui.rs`: `correction_panel_body` (the «Настройка» section) and `correction_card_controls` (the
  per-kind card). Edit it when the panel grows a section or a kind gains parameters.

## Contracts and invariants
- **View-only.** No code path here may write a layer, the doc, the manifest or the clean overlays.
- **Neutral is free.** `CorrectionState::active_params` returns `None` for «Нет» and for exactly
  neutral parameters, so an untouched panel paints no shader layer at all.
- **`apply_channel` restates the preset.** It is the Rust form of the preset's WGSL; the test in
  `shader.rs` pins that the model's two values land in the preset's `params(0).x` / `.y`. The WGSL
  itself is private to the library and is not re-tested here.
- **GL lifetime belongs to the library contract.** The binary calls
  `egui_shader_layers::install_glow` once from the eframe app creator (`cc.gl`) and
  `destroy_glow` from `StudioBootstrapApp::on_exit`; the backend creates its GL objects lazily on
  the first painted layer. Nothing in this crate may hold GL objects of its own.
- **Failure is loud.** A missing backend, a failed backend (e.g. WebGL1 / GL < 3.3) or a driver
  rejecting the preset's translated shader makes `CorrectionAvailability::check` true, and the panel then
  shows `ps_editor.correction.gpu_unavailable_error`; the layer draws nothing extra rather than a
  wrong picture. The library reports its own failures only through the `log` facade, which the app
  routes nowhere, so `shader::CorrectionAvailability` (held in `PsEditorTabState`, checked by the
  panel and by the canvas pass) writes the backend `status` plus every `failed_effects` error of
  the preset's effect to `runtime_log` ONCE per session. An install failure is additionally logged
  by the binary's `install_shader_layers`.
- **The card takes no host state.** `correction_card_controls(ui, &mut Correction) -> bool` is the
  reusable unit: a second host (the «Пресеты» section planned next) renders the same controls over
  its own `Correction`. Do not add host parameters to it.
- **No literal user-visible strings.** Every caption is a `ps_editor.correction.*` key present in
  all five `crates/ms-i18n/locales/*.json` catalogs.

## Editing map
- To change the maths or add a parameter, see `model.rs` (and its tests, which pin the contract).
- To add a correction kind: a `CorrectionKind` variant + its `ALL` entry and `title()` arm, a
  `Correction` variant, a `params()` arm (and a params type if the new kind uses another preset),
  a `shader.rs` pass for it, and a `correction_card_controls` arm. Every `match` is exhaustive,
  so the compiler lists the sites.
- To change what the pass covers, see `PsEditorTabState::draw_correction_pass` in `../lib.rs`: the
  layer is painted between `draw_composite` and the pixel-grid pass, on a painter clipped to
  `page_rect ∩ canvas rect`.
- To change backend installation or teardown, see `install_shader_layers` and
  `StudioBootstrapApp::on_exit` (`src/studio_bootstrap.rs`) and the web app creator in
  `src/web_entry.rs`.
