# 02 — Painting: Painter, Shape, Mesh, textures

Target version: **egui / epaint / emath / ecolor 0.35.0** (`Cargo.toml:63`, `Cargo.lock` `egui 0.35.0`).
Every API claim below is cited to the vendored crate source under
`~/.cargo/registry/src/index.crates.io-*/`. Do not write egui code from memory: 0.32–0.35
renamed or removed several things older models still emit (`Rounding`, `ColorImage::new(size, color)`,
`Painter::rect_stroke` without `StrokeKind`).

## Painter: how to get one

```rust
let painter = ui.painter();                 // egui-0.35.0/src/ui.rs:457   -> &Painter (clipped to the ui)
let painter = ui.painter_at(rect);          // egui-0.35.0/src/ui.rs:619   -> Painter clipped to `rect`
let painter = painter.with_clip_rect(rect); // egui-0.35.0/src/painter.rs:71 -> new Painter, clip = rect
painter.set_clip_rect(rect);                // egui-0.35.0/src/painter.rs:183 (&mut self)
```

`with_clip_rect` returns a *new* `Painter`; it does not mutate. Repo examples:
`src/launcher/new_project/window.rs:2605`, `src/tabs/ps_editor/mod.rs:3758`.

All painter coordinates are **screen-space logical points**, not widget-local.

## Painter primitives — exact 0.35 signatures

```rust
// egui-0.35.0/src/painter.rs:213
pub fn add(&self, shape: impl Into<Shape>) -> ShapeIdx;
// egui-0.35.0/src/painter.rs:226
pub fn extend<I: IntoIterator<Item = Shape>>(&self, shapes: I);

// egui-0.35.0/src/painter.rs:397  — NOTE: CornerRadius, not `Rounding`
pub fn rect_filled(&self, rect: Rect, corner_radius: impl Into<CornerRadius>,
                   fill_color: impl Into<Color32>) -> ShapeIdx;
// egui-0.35.0/src/painter.rs:406  — NOTE: extra `StrokeKind` argument
pub fn rect_stroke(&self, rect: Rect, corner_radius: impl Into<CornerRadius>,
                   stroke: impl Into<Stroke>, stroke_kind: StrokeKind) -> ShapeIdx;
// egui-0.35.0/src/painter.rs:380  — fill + stroke in one shape
pub fn rect(&self, rect: Rect, corner_radius: impl Into<CornerRadius>,
            fill_color: impl Into<Color32>, stroke: impl Into<Stroke>,
            stroke_kind: StrokeKind) -> ShapeIdx;

// egui-0.35.0/src/painter.rs:318
pub fn line_segment(&self, points: [Pos2; 2], stroke: impl Into<Stroke>) -> ShapeIdx;
// egui-0.35.0/src/painter.rs:327  — polyline; takes PathStroke, not Stroke
pub fn line(&self, points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> ShapeIdx;
// egui-0.35.0/src/painter.rs:332 / :337
pub fn hline(&self, x: impl Into<Rangef>, y: f32, stroke: impl Into<Stroke>) -> ShapeIdx;
pub fn vline(&self, x: f32, y: impl Into<Rangef>, stroke: impl Into<Stroke>) -> ShapeIdx;

// egui-0.35.0/src/painter.rs:356 / :370 / :341
pub fn circle_filled(&self, center: Pos2, radius: f32, fill_color: impl Into<Color32>) -> ShapeIdx;
pub fn circle_stroke(&self, center: Pos2, radius: f32, stroke: impl Into<Stroke>) -> ShapeIdx;
pub fn circle(&self, center: Pos2, radius: f32, fill_color: impl Into<Color32>,
              stroke: impl Into<Stroke>) -> ShapeIdx;

// egui-0.35.0/src/painter.rs:417  — returns ()
pub fn arrow(&self, origin: Pos2, vec: Vec2, stroke: impl Into<Stroke>);

// egui-0.35.0/src/painter.rs:447  — raw textured quad
pub fn image(&self, texture_id: epaint::TextureId, rect: Rect, uv: Rect, tint: Color32) -> ShapeIdx;

// egui-0.35.0/src/painter.rs:469  — lays out + paints, returns the Rect it occupied
pub fn text(&self, pos: Pos2, anchor: Align2, text: impl ToString,
            font_id: FontId, text_color: Color32) -> Rect;
// egui-0.35.0/src/painter.rs:529 / :541
pub fn galley(&self, pos: Pos2, galley: Arc<Galley>, fallback_color: Color32);
pub fn galley_with_override_text_color(&self, pos: Pos2, galley: Arc<Galley>, text_color: Color32);
// Build the galley first: painter.layout(..) / layout_no_wrap(..) / layout_job(..)
// egui-0.35.0/src/painter.rs:488 / :503 / :517 -> Arc<Galley>
```

`galley()` only substitutes the fallback color into parts painted with `Color32::PLACEHOLDER`
(`egui-0.35.0/src/painter.rs:521-528`); any explicitly-colored run keeps its color. Use
`galley_with_override_text_color` to force one color.

### CornerRadius (was `Rounding`)

`epaint-0.35.0/src/corner_radius.rs:13` — struct with `u8` fields `nw, ne, sw, se`;
`From<u8>` (:34) and `From<f32>` (:41); `CornerRadius::ZERO` (:50), `CornerRadius::same(radius: u8)` (:59).
Repo usage: `src/launcher/pages/settings_page.rs:520`.

### StrokeKind (new required arg)

`epaint-0.35.0/src/stroke.rs:101` — `Inside` / `Middle` / `Outside`: whether the stroke is painted
inside the rect, centered on its edge, or outside it. Repo usage:
`src/launcher/new_project/window.rs:2610` (`Inside`), `src/launcher/pages/settings_page.rs:530` (`Middle`).
`Stroke::new(width, color)` — `epaint-0.35.0/src/stroke.rs:25`. `PathStroke::new` — same file, :139.

## Shape

`epaint-0.35.0/src/shapes/shape.rs:27` — variants:
`Noop`, `Vec(Vec<Shape>)` (:33), `Circle(CircleShape)` (:36), `Ellipse` (:39),
`LineSegment { points: [Pos2; 2], stroke: Stroke }` (:42), `Path(PathShape)` (:46),
`Rect(RectShape)` (:49), `Text(TextShape)` (:54), **`Mesh(Arc<Mesh>)`** (:61),
`QuadraticBezier` (:64), `CubicBezier` (:67), `Callback(PaintCallback)` (:70).

Constructors that matter:

```rust
Shape::mesh(mesh: impl Into<Arc<Mesh>>) -> Shape      // shapes/shape.rs:361 (debug_asserts mesh.is_valid())
Shape::convex_polygon(points: Vec<Pos2>, fill: impl Into<Color32>,
                      stroke: impl Into<PathStroke>) -> Shape   // :251 (clockwise winding is fastest)
Shape::dashed_line(path: &[Pos2], stroke: impl Into<Stroke>,
                   dash_length: f32, gap_length: f32) -> Vec<Shape>   // :170  — returns a Vec!
Shape::dashed_line_with_offset(path, stroke, dash_lengths: &[f32],
                               gap_lengths: &[f32], dash_offset: f32) -> Vec<Shape>  // :189
Shape::dotted_line(path: &[Pos2], color: impl Into<Color32>,
                   spacing: f32, radius: f32) -> Vec<Shape>     // :158
Shape::line(points: Vec<Pos2>, stroke: impl Into<PathStroke>)   // :147
Shape::closed_line(...)                                          // :153
Shape::image(texture_id, rect, uv, tint) -> Shape                // :373 (builds a Mesh internally)
Shape::rect_filled / rect_stroke                                 // :281 / :291 (same args as Painter)
```

`dashed_line` returns `Vec<Shape>` — feed it to `painter.extend(..)`, not `painter.add(..)`.

## Mesh and Vertex — custom textured geometry

**`Mesh::new` does not exist** (`epaint-0.35.0/src/mesh.rs` has no `fn new`). Construct with
`Mesh::default()` or `Mesh::with_texture(id)` (`epaint-0.35.0/src/mesh.rs:77`).

```rust
// epaint-0.35.0/src/mesh.rs:12 (default build; a `unity` feature reorders the fields, :43)
pub struct Vertex {
    pub pos: Pos2,      // logical points, screen space, (0,0) = top-left of the screen
    pub uv: Pos2,       // normalized [0,1]^2 texture coords, (0,0) = top-left texel
    pub color: Color32, // sRGBA, PREMULTIPLIED alpha
}

// epaint-0.35.0/src/mesh.rs:60
pub struct Mesh {
    pub indices: Vec<u32>,   // length must be a multiple of 3; triangle list
    pub vertices: Vec<Vertex>,
    pub texture_id: TextureId,
}
```

Contracts:
- `indices.len() % 3 == 0`, every index `< vertices.len()`; `Mesh::is_valid()` checks the bound
  (`mesh.rs:99`) and `Shape::mesh` `debug_assert!`s it (`shapes/shape.rs:363`).
- **Winding order is not consistent in egui — backface culling must be off** (`mesh.rs:66-68`).
- `colored_vertex()` panics (debug-asserts) if the mesh has a texture (`mesh.rs:169-176`); with a
  texture, push `Vertex` values directly or use `add_rect_with_uv` (`mesh.rs:199`).
- Untextured geometry must use `WHITE_UV` (`Vertex::untextured`, `mesh.rs:28`).
- Helpers: `add_triangle(a,b,c)` (:179), `reserve_vertices` (:193), `reserve_triangles` (:186),
  `append`/`append_ref` (:132/:147), `split_to_u16` (:243) for 16-bit-index backends.

Repo does exactly this for the deformable typing-text overlay and for rotated raster layers:

- `src/tabs/typing/tab/mesh_geometry.rs:724-745` — `build_textured_deform_mesh`: `Mesh::with_texture`,
  a row-major `cols × rows` grid of `egui::epaint::Vertex { pos, uv: (s, t), color: tint }`,
  two triangles per cell, painted via `egui::Shape::mesh(mesh)` (`mesh_geometry.rs:704`).
- `src/tabs/typing/tab/doc_layers.rs:805-826` — rotated quad: 4 vertices, uv corners
  `(0,0) (1,0) (1,1) (0,1)`, `add_triangle(0,1,2)` + `add_triangle(0,2,3)`, tint from layer opacity via
  `Color32::from_white_alpha(..)`.
- `src/launcher/app.rs:924-935` — blurred background mesh.

## Colors — premultiplied alpha is the contract

`Color32` is **sRGBA u8 with premultiplied alpha** (`ecolor-0.35.0/src/color32.rs:8`, struct at :31).

```rust
Color32::from_rgb(r, g, b)                       // color32.rs:108  (const, a = 255)
Color32::from_rgba_premultiplied(r, g, b, a)     // color32.rs:122  (const) — raw bytes, no math
Color32::from_rgba_unmultiplied(r, g, b, a)      // color32.rs:133  — multiplies for you (LUT)
Color32::from_rgba_unmultiplied_const(r,g,b,a)   // color32.rs:164  — const-usable, slower
Color32::from_white_alpha(a) == [a,a,a,a]        // color32.rs:196  — premultiplied white tint
color.to_array() -> [u8; 4]                      // color32.rs:256  — PREMULTIPLIED bytes
color.to_srgba_unmultiplied() -> [u8; 4]         // color32.rs:273  — un-premultiplied (lossy round-trip)
color.gamma_multiply(f32)                        // color32.rs:294
Color32::PLACEHOLDER                             // color32.rs:104  — "fill in the fallback color here"
```

Where premultiplication bites:
- `to_array()` gives premultiplied bytes. Exporting a semi-transparent overlay with `to_array()` and
  then re-importing it with `from_rgba_unmultiplied` double-darkens it — the repo has a regression test
  for exactly this (`src/tabs/typing/tab/tests.rs:480`).
- Any RGBA coming from the `image` crate is **un-premultiplied**, so it must go through
  `ColorImage::from_rgba_unmultiplied` (repo: `src/app.rs:1747`, `src/app.rs:3159`).

`Rgba` (`ecolor-0.35.0/src/rgba.rs:10`) is linear-space `[f32; 4]`, also premultiplied;
`From<Color32> for Rgba` / `From<Rgba> for Color32` at `ecolor-0.35.0/src/lib.rs:51` and `:74`.
Scalar helpers: `linear_f32_from_gamma_u8` (`ecolor-0.35.0/src/lib.rs:97`),
`gamma_u8_from_linear_f32` (:114), `hsv_from_rgb`/`rgb_from_hsv` (`ecolor-0.35.0/src/hsva.rs:191/:215`).

## ColorImage in 0.35 — the shape changed

```rust
// epaint-0.35.0/src/image.rs:48
pub struct ColorImage {
    pub size: [usize; 2],     // [width, height] in texels
    pub source_size: Vec2,    // original SVG size, else texel size  (NEW vs older egui)
    pub pixels: Vec<Color32>, // row-major, top-to-bottom
}
```

- `ColorImage::new(size, pixels: Vec<Color32>)` — `image.rs:61`. **This is not the old API.** In 0.31 it
  was `new(size: [usize; 2], color: Color32)` (`epaint-0.31.1/src/image.rs:59`). The 0.35 equivalent of
  the old call is `ColorImage::filled(size, color)` (`image.rs:75`, repo: `src/tabs/typing/tab/tests.rs:22`).
- `from_rgba_unmultiplied(size, &[u8])` — `image.rs:113` (the normal path from `image::RgbaImage`).
- `from_rgba_premultiplied` (:128), `from_rgb` (:193), `from_gray` (:146), `as_raw`/`as_raw_mut` (:177/:183),
  `region`/`region_by_pixels` (:249/:273), `with_source_size` (:227).
- `ImageData` (`image.rs:16`) is a one-variant enum `Color(Arc<ColorImage>)`; `load_texture` takes
  `impl Into<ImageData>`, so passing a `ColorImage` directly works.

**Repo rule (README_AGENT.md:184, :711):** clean overlays keep a **dual CPU representation** —
`egui::ColorImage` for UI/canvas *and* `Arc<image::RgbaImage>` for export/save/tools. Dropping either
side silently breaks export. Never "simplify" one away.

## Textures: handle lifecycle

```rust
// egui-0.35.0/src/context.rs:2322
pub fn load_texture(&self, name: impl Into<String>, image: impl Into<ImageData>,
                    options: TextureOptions) -> TextureHandle;
```

**Trap: `TextureHandle` frees the GPU texture on `Drop`** (`epaint-0.35.0/src/texture_handle.rs:25-29`
— `impl Drop { self.tex_mngr.write().free(self.id) }`). Store the handle in your app/tab state for as
long as you paint with it; a handle created inside `fn ui(..)` and dropped at the end of the frame
produces a blank/garbage texture. Cloning the handle is cheap and refcounts the texture
(`texture_handle.rs:31`). Repo keeps handles in tile structs: `src/app.rs:1751-1758`.

Other handle methods: `id() -> TextureId` (:64), `set` (:70), `set_partial` (:78), `size()` (:90),
`byte_size` (:104).

`TextureId` (`epaint-0.35.0/src/lib.rs:95`): `Managed(u64)` (allocated via `load_texture`; `Managed(0)`
is the font atlas) or `User(u64)` (custom renderer texture).

`TextureOptions` (`epaint-0.35.0/src/textures.rs:153`): fields `magnification`, `minification`,
`wrap_mode`, `mipmap_mode`. Constants: `LINEAR` (:176), `NEAREST` (:184), `LINEAR_REPEAT` (:192),
`NEAREST_REPEAT` (:208). Repo: `TextureOptions::LINEAR` for page tiles (`src/app.rs:1755`),
`NEAREST` for masks (`src/tabs/translation/tab.rs:254`, `src/tabs/cleaning/tab.rs:1591`).

Displaying a texture:

```rust
ui.image(source)                       // egui-0.35.0/src/ui.rs:2033 -> Response; = Image::new(source).ui(self)
egui::Image::new(source)               // egui-0.35.0/src/widgets/image.rs:63
```
`ImageSource` (`egui-0.35.0/src/widgets/image.rs:570`): `Uri`, `Texture(SizedTexture)`, `Bytes { .. }`.
`SizedTexture { id, size }` (`egui-0.35.0/src/load.rs:444`), `SizedTexture::from_handle(&TextureHandle)`
(`load.rs:461`); any `Into<SizedTexture>` converts into `ImageSource` (`widgets/image.rs:789`), so
`ui.image(&texture_handle)` works. For manual placement inside a canvas, prefer
`painter.image(id, rect, uv, tint)` — that is what the repo does.

## Getting a Rect to paint into

```rust
// egui-0.35.0/src/ui.rs:1150
pub fn allocate_exact_size(&mut self, desired_size: Vec2, sense: Sense) -> (Rect, Response);
// egui-0.35.0/src/ui.rs:1256 — you already know the rect
pub fn allocate_rect(&mut self, rect: Rect, sense: Sense) -> Response;   // Response::rect == rect
// egui-0.35.0/src/ui.rs:1138
pub fn allocate_response(&mut self, desired_size: Vec2, sense: Sense) -> Response;
// egui-0.35.0/src/ui.rs:1187 — no interaction
pub fn allocate_space(&mut self, desired_size: Vec2) -> (Id, Rect);
```
Repo: `src/launcher/pages/settings_page.rs:511` (`allocate_exact_size`),
`src/launcher/psd_import_window.rs:672` (`allocate_rect`). Then paint into `response.rect` with
`ui.painter_at(rect)` so nothing bleeds outside.

## Coordinates: points vs pixels

Everything in `Painter`/`Shape`/`Vertex`/`Rect` is in **logical points**. Physical pixels =
points × `pixels_per_point`.

- `Context::pixels_per_point()` — `egui-0.35.0/src/context.rs:2220`; `set_pixels_per_point` (:2228).
- `InputState::pixels_per_point` field — `egui-0.35.0/src/input_state/mod.rs:265`.
- `emath::Rect` (`emath-0.35.0/src/rect.rs:25`) — `from_min_max` (:73, const), `from_min_size` (:79),
  `from_center_size` (:87). `Pos2` (`emath-0.35.0/src/pos2.rs:18`), `Vec2` (`emath-0.35.0/src/vec2.rs:16`).
- Y grows **downwards**; `Rect::min` is top-left.
- Textures are sized in **texels**; the mapping texel→point is yours (the repo carries an explicit
  page-px ↔ scene-point transform, e.g. `scene_from_page_px` in `src/tabs/typing/tab/doc_layers.rs`).

## Editing map

- Custom textured geometry / deform meshes: `src/tabs/typing/tab/mesh_geometry.rs`
  (`build_textured_deform_mesh`, `draw_textured_deform_mesh`).
- Layer/raster quads on the typing canvas: `src/tabs/typing/tab/doc_layers.rs`.
- Page tile texture upload (frame-budgeted, `ColorImage` → `TextureHandle`): `src/app.rs`
  (`upload_textures_incremental`, :1718).
- Mask textures (`NEAREST`): `src/tabs/cleaning/tools/base.rs`, `src/tabs/translation/tab.rs`.
- Anything that must also be exported: keep the `Arc<image::RgbaImage>` side alive (README_AGENT.md:711).
