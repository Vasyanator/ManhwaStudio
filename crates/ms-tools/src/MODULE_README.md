# Module: crates/ms-tools/src (crate `ms-tools`)

## Purpose
This directory contains shared tool code that is useful across UI tools but does not belong to a
single tab: low-level primitives (mask brush, polygon rasterization, the SOR kernel, the overlay
pixel solver) and, in `patch/`, a whole host-neutral TOOL that several tabs can drive.

## Architecture
`lib.rs` is the crate root and the public boundary. It exports `MaskBrush` from `mask_brush.rs`, `fill_polygon_spans`
from `polygon_mask.rs`, `red_black_sor_sweeps` from `sor.rs` and `overlay_pixel_for_final_color`
from `overlay_pixel.rs`, and re-exports `patch` as a public submodule.

The primitives are leaves: they take buffers and geometry and know nothing about tabs. `patch/` is
the one exception in shape, not in principle — it is a complete tool, but it reaches its
surroundings only through the `PatchHost` trait it defines, so it still depends on no tab.

`fill_polygon_spans` is buffer-agnostic: it rasterizes a closed polygon and hands each filled
horizontal span to a caller-supplied closure, so the same geometry can be written into a `u8`
selection mask, a `Vec<bool>` region mask, or anything else without duplicating the scanline code.

`MaskBrush` stores only brush configuration and short-lived wheel gesture state. It handles brush
radius changes from Shift+wheel and size hotkeys, draws a circle cursor in an egui image viewport,
and paints continuous stroke segments by stamping filled circles along the segment.

The painting functions are synchronous pixel-buffer helpers. Callers are responsible for invoking
them only on appropriately scoped buffers, typically small masks, scratch overlays, or worker-owned
images. They perform bounds clipping and return early on invalid binary-mask dimensions.

## Files and submodules
- `lib.rs`: crate root; shared-tool exports and the `ms-i18n` macro mount.
- `mask_brush.rs`: `MaskBrush`, internal ColorImage painting helpers, binary-mask painting helpers,
  radius input handling, and cursor drawing.
- `polygon_mask.rs`: `fill_polygon_spans`, the even-odd scanline polygon rasterizer shared by the
  PS-editor lasso selection and the cleaning tools.
- `sor.rs`: `red_black_sor_sweeps`, the ONE red-black SOR kernel of the project. Consumers are the
  cleaning tab's gradient fill and `patch/membrane.rs`.
- `overlay_pixel.rs`: `overlay_pixel_for_final_color`, which solves the DENSE overlay pixel that
  reproduces a desired final colour over a known, opaque backdrop at an alpha of at least the given
  coverage. Consumers are the cleaning tab's brushes and stamp (through `base::`) and BOTH patch
  hosts — the cleaning tab's and the PS editor's.
- `patch/`: the host-neutral core of the «Заплатка» (patch) tool, driven by a host through the
  `PatchHost` trait. Own `MODULE_README.md`.

## Contracts and invariants
- `MaskBrush` is UI/tool state, not durable project state. Do not serialize it into project files.
- `paint_mask_segment` writes transparent pixels when erasing and white pixels when painting.
- `paint_binary_mask_segment` writes `0` when erasing and `255` when painting. The caller must pass
  a buffer whose logical length is at least `mask_width * mask_height`; invalid dimensions are a
  no-op.
- `fill_polygon_spans` emits `span(y, x0, x1)` with an INCLUSIVE `x0..=x1`, already clamped to the
  caller's `width`/`height`, in increasing `y`, and never inverted. Insideness is sampled at the
  scanline centre `y + 0.5` under the even-odd rule. These sampling rules are a CONTRACT, not an
  implementation detail: callers rasterizing the same polygon into differently sized buffers must
  agree pixel for pixel, so changing them means updating every caller and its tests.
- Public helpers operate in image pixel coordinates. Callers must convert scene/screen/UV
  coordinates before calling them.
- These helpers must not perform file I/O, model access, backend calls, or shared-model mutation.
- `red_black_sor_sweeps` updates only the INTERIOR of its grid: a caller must pad its region by at
  least one pixel, and `rw`/`rh` below 3 make the call a no-op. `lam[i] == 0` is a free
  (membrane-like) cell, a large `lam[i]` a soft Dirichlet pin. A second SOR implementation anywhere
  in the project is a defect.
- `overlay_pixel_for_final_color` returns the DENSE solution, not the minimum-alpha one; its
  declaration comment carries the renderer-level reason, which is not a property of any one tool.
- Keep this directory independent of tab-specific state. If behavior needs project paths, canvas
  state, or cleaning/typing-specific policy, it belongs in that tab module — or, for `patch/`,
  behind `PatchHost`.

## Editing map
- To change shared brush radius controls, cursor rendering, or stroke stamping, edit
  `mask_brush.rs`.
- To change lasso/polygon fill semantics (both the PS-editor selection and the cleaning tools),
  edit `polygon_mask.rs`.
- To change the Laplacian/screened-Poisson solve shared by the gradient fill and the patch
  membrane, edit `sor.rs`; both consumers must be re-verified.
- To change how a desired final colour becomes an overlay pixel, edit `overlay_pixel.rs`.
- To change the patch tool's gesture, geometry or maths — or what it needs from a host — edit
  `patch/`; to change how a patch is stored, edit the host instead.
- To expose another low-level reusable tool primitive, add its module here and re-export only the
  narrow API needed by callers.
- To change cleaning-specific mask editor behavior, edit `crates/ms-tab-cleaning/src/tools/base.rs` instead.
