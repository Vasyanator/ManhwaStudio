# API index: `epaint` 0.35.0

GENERATED FILE — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`. Extracted from rustdoc JSON of the exact crate source in the local cargo registry, so every signature and line number below is real.

**If a name is not in this file, it does not exist in our version of the crate.** Grep here before writing egui code from memory.

Items are listed under the path callers actually write (the public re-export, e.g. `egui::Panel`, `egui::Color32`), not where they happen to be defined. Citations point into the crate that owns the item, so a type `egui` re-exports from `epaint` cites `epaint-0.35.0/src/…`.

## `epaint`

### `HAS_RAYON` (constant) — `epaint-0.35.0/src/lib.rs:161`

Was epaint compiled with the `rayon` feature?

### `WHITE_UV` (constant) — `epaint-0.35.0/src/lib.rs:88`

The UV coordinate of a white region of the texture mesh.

### `ColorMode` (enum) — `epaint-0.35.0/src/color.rs:9`

How paths will be colored.

Variants:

- `ColorMode::Solid` — The entire path is one solid color, this is the default.
- `ColorMode::UV` — Provide a callback which takes in the path's bounding box and a position and converts it to a color…

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`

### `Direction` (enum) — `epaint-0.35.0/src/direction.rs:4`

A cardinal direction, one of [`LeftToRight`](Direction::LeftToRight), [`RightToLeft`](Direction::RightToLeft), [`TopDown`](Direction::TopDown), [`BottomUp`](Direction::B…

Variants:

- `Direction::LeftToRight`
- `Direction::RightToLeft`
- `Direction::TopDown`
- `Direction::BottomUp`

Methods:

- `fn is_horizontal(self) -> bool` — `epaint-0.35.0/src/direction.rs:13`
- `fn is_vertical(self) -> bool` — `epaint-0.35.0/src/direction.rs:21`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `FontColorTransferFunction` (enum) — `epaint-0.35.0/src/image.rs:370`

How to convert font coverage values into alpha and color values.

Variants:

- `FontColorTransferFunction::Off` — Use the raw RGBA values from the font rasterizer, without any conversion.
- `FontColorTransferFunction::Gamma` — `alpha = coverage^gamma`.
- `FontColorTransferFunction::TwoCoverageMinusCoverageSq` — `alpha = 2 * coverage - coverage^2`

Methods:

- `fn alpha_from_coverage(self, coverage: f32) -> f32` — `epaint-0.35.0/src/image.rs:423`
  Convert coverage to alpha.
- `fn color_from_coverage(self, coverage: f32) -> Color32` — `epaint-0.35.0/src/image.rs:433`
- `fn to_atlas_color(self, input_color: Color32) -> Color32` — `epaint-0.35.0/src/image.rs:403`
  How to convert a white color written by the font rasterizer into a color to be written into the font atlas.
- `fn to_gamma(self) -> f32` — `epaint-0.35.0/src/image.rs:439`
  Convert this into the closest gamma exponent

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `FontFamily` (enum) — `epaint-0.35.0/src/text/fonts.rs:80`

Font of unknown size.

Variants:

- `FontFamily::Proportional` — A font where some characters are wider than other (e.g. 'w' is wider than 'i').
- `FontFamily::Monospace` — A font where each character is the same width (`w` is the same width as `i`).
- `FontFamily::Name` — One of the names in [`FontDefinitions::families`].

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Display`, `Eq`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `ImageData` (enum) — `epaint-0.35.0/src/image.rs:16`

An image stored in RAM.

Variants:

- `ImageData::Color` — RGBA image.

Methods:

- `fn bytes_per_pixel(&self) -> usize` — `epaint-0.35.0/src/image.rs:36`
- `fn height(&self) -> usize` — `epaint-0.35.0/src/image.rs:32`
- `fn size(&self) -> [usize; 2]` — `epaint-0.35.0/src/image.rs:22`
- `fn width(&self) -> usize` — `epaint-0.35.0/src/image.rs:28`

Implements: `Clone`, `Deserialize<'de>`, `Eq`, `From<Arc<ColorImage>>`, `From<ColorImage>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Primitive` (enum) — `epaint-0.35.0/src/lib.rs:153`

A rendering primitive - either a [`Mesh`] or a [`PaintCallback`].

Variants:

- `Primitive::Mesh`
- `Primitive::Callback`

Implements: `Clone`, `Debug`

### `Shape` (enum) — `epaint-0.35.0/src/shapes/shape.rs:27`

A paint primitive such as a circle or a piece of text. Coordinates are all screen space points (not physical pixels).

Variants:

- `Shape::Noop` — Paint nothing. This can be useful as a placeholder.
- `Shape::Vec` — Recursively nest more shapes - sometimes a convenience to be able to do. For performance reasons it…
- `Shape::Circle` — Circle with optional outline and fill.
- `Shape::Ellipse` — Ellipse with optional outline and fill.
- `Shape::LineSegment` — A line between two points.
- `Shape::Path` — A series of lines between points. The path can have a stroke and/or fill (if closed).
- `Shape::Rect` — Rectangle with optional outline and fill.
- `Shape::Text` — Text.
- `Shape::Mesh` — A general triangle mesh.
- `Shape::QuadraticBezier` — A quadratic [Bézier Curve](https://en.wikipedia.org/wiki/B%C3%A9zier_curve).
- `Shape::CubicBezier` — A cubic [Bézier Curve](https://en.wikipedia.org/wiki/B%C3%A9zier_curve).
- `Shape::Callback` — Backend-specific painting.

Methods:

- `fn circle_filled(center: Pos2, radius: f32, fill_color: impl Into<Color32>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:260`
- `fn circle_stroke(center: Pos2, radius: f32, stroke: impl Into<Stroke>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:265`
- `fn closed_line(points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:153`
  A line that closes back to the start point again.
- `fn convex_polygon(points: Vec<Pos2>, fill: impl Into<Color32>, stroke: impl Into<PathStroke>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:251`
  A convex polygon with a fill and optional stroke.
- `fn dashed_line(path: &[Pos2], stroke: impl Into<Stroke>, dash_length: f32, gap_length: f32) -> Vec<Self>` — `epaint-0.35.0/src/shapes/shape.rs:170`
  Turn a line into dashes.
- `fn dashed_line_many(points: &[Pos2], stroke: impl Into<Stroke>, dash_length: f32, gap_length: f32, shapes: &mut Vec<Self>)` — `epaint-0.35.0/src/shapes/shape.rs:210`
  Turn a line into dashes. If you need to create many dashed lines use this instead of [`Self::dashed_line`].
- `fn dashed_line_many_with_offset(points: &[Pos2], stroke: impl Into<Stroke>, dash_lengths: &[f32], gap_lengths: &[f32], dash_offset: f32, shapes: &mut Vec<Self>)` — `epaint-0.35.0/src/shapes/shape.rs:229`
  Turn a line into dashes with different dash/gap lengths and a start offset. If you need to create many dashed…
- `fn dashed_line_with_offset(path: &[Pos2], stroke: impl Into<Stroke>, dash_lengths: &[f32], gap_lengths: &[f32], dash_offset: f32) -> Vec<Self>` — `epaint-0.35.0/src/shapes/shape.rs:189`
  Turn a line into dashes with different dash/gap lengths and a start offset.
- `fn dotted_line(path: &[Pos2], color: impl Into<Color32>, spacing: f32, radius: f32) -> Vec<Self>` — `epaint-0.35.0/src/shapes/shape.rs:158`
  Turn a line into equally spaced dots.
- `fn ellipse_filled(center: Pos2, radius: Vec2, fill_color: impl Into<Color32>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:270`
- `fn ellipse_stroke(center: Pos2, radius: Vec2, stroke: impl Into<Stroke>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:275`
- `fn galley(pos: Pos2, galley: Arc<Galley>, fallback_color: Color32) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:344`
  Any uncolored parts of the [`Galley`] (using [`Color32::PLACEHOLDER`]) will be replaced with the given color.
- `fn galley_with_override_text_color(pos: Pos2, galley: Arc<Galley>, text_color: Color32) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:350`
  All text color in the [`Galley`] will be replaced with the given color.
- `fn gradient_rect(rect: Rect, direction: Direction, [from, to]: [Color32; 2]) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:306`
  Paints a gradient rectangle that transitions from `color_from` to `color_to` along the given `direction`.
- `fn hline(x: impl Into<Rangef>, y: f32, stroke: impl Into<Stroke>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:126`
  A horizontal line.
- `fn image(texture_id: TextureId, rect: Rect, uv: Rect, tint: Color32) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:373`
  An image at the given position.
- `fn line(points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:147`
  A line through many points.
- `fn line_segment(points: [Pos2; 2], stroke: impl Into<Stroke>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:118`
  A line between two points. More efficient than calling [`Self::line`].
- `fn mesh(mesh: impl Into<Arc<Mesh>>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:361`
- `fn rect_filled(rect: Rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:281`
  See also [`Self::rect_stroke`].
- `fn rect_stroke(rect: Rect, corner_radius: impl Into<CornerRadius>, stroke: impl Into<Stroke>, stroke_kind: StrokeKind) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:291`
  See also [`Self::rect_filled`].
- `fn scale(&mut self, factor: f32)` — `epaint-0.35.0/src/shapes/shape.rs:427`
  Scale the shape by `factor`, in-place.
- `fn text(fonts: &mut FontsView<'_>, pos: Pos2, anchor: Align2, text: impl ToString, font_id: FontId, color: Color32) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:327`
- `fn texture_id(&self) -> TextureId` — `epaint-0.35.0/src/shapes/shape.rs:413`
- `fn transform(&mut self, transform: TSTransform)` — `epaint-0.35.0/src/shapes/shape.rs:443`
  Transform (move/scale) the shape in-place.
- `fn translate(&mut self, delta: Vec2)` — `epaint-0.35.0/src/shapes/shape.rs:435`
  Move the shape by `delta`, in-place.
- `fn visual_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/shape.rs:380`
  The visual bounding rectangle (includes stroke widths)
- `fn vline(x: f32, y: impl Into<Rangef>, stroke: impl Into<Stroke>) -> Self` — `epaint-0.35.0/src/shapes/shape.rs:135`
  A vertical line.

Implements: `Clone`, `Debug`, `From<Arc<Mesh>>`, `From<CircleShape>`, `From<CubicBezierShape>`, `From<EllipseShape>`, `From<Mesh>`, `From<PaintCallback>`, `From<PathShape>`, `From<QuadraticBezierShape>`, `From<RectShape>`, `From<TextShape>`, `From<Vec<Shape>>`, `PartialEq`, `StructuralPartialEq`

### `StrokeKind` (enum) — `epaint-0.35.0/src/stroke.rs:101`

Describes how the stroke of a shape should be painted.

Variants:

- `StrokeKind::Inside` — The stroke should be painted entirely inside of the shape
- `StrokeKind::Middle` — The stroke should be painted right on the edge of the shape, half inside and half outside.
- `StrokeKind::Outside` — The stroke should be painted entirely outside of the shape

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextureId` (enum) — `epaint-0.35.0/src/lib.rs:95`

What texture to use in a [`Mesh`] mesh.

Variants:

- `TextureId::Managed` — Textures allocated using [`TextureManager`].
- `TextureId::User` — Your own texture, defined in any which way you want. The backend renderer will presumably use this…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<&TextureHandle>`, `From<&mut TextureHandle>`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `pos2` — `emath-0.35.0/src/pos2.rs:29`

```rust
const fn pos2(x: f32, y: f32) -> Pos2
```

`pos2(x, y) == Pos2::new(x, y)`

### `vec2` — `emath-0.35.0/src/vec2.rs:26`

```rust
const fn vec2(x: f32, y: f32) -> Vec2
```

`vec2(x, y) == Vec2::new(x, y)`

### `Brush` (struct) — `epaint-0.35.0/src/brush.rs:6`

Controls texturing of a [`crate::RectShape`].

Public fields:

- `fill_texture_id: TextureId` — If the rect should be filled with a texture, which one?
- `uv: Rect` — What UV coordinates to use for the texture?

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `CircleShape` (struct) — `epaint-0.35.0/src/shapes/circle_shape.rs:6`

How to paint a circle.

Public fields:

- `center: Pos2`
- `radius: f32`
- `fill: Color32`
- `stroke: Stroke`

Methods:

- `fn filled(center: Pos2, radius: f32, fill_color: impl Into<Color32>) -> Self` — `epaint-0.35.0/src/shapes/circle_shape.rs:15`
- `fn stroke(center: Pos2, radius: f32, stroke: impl Into<Stroke>) -> Self` — `epaint-0.35.0/src/shapes/circle_shape.rs:25`
- `fn visual_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/circle_shape.rs:35`
  The visual bounding rectangle (includes stroke width)

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `From<CircleShape>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ClippedPrimitive` (struct) — `epaint-0.35.0/src/lib.rs:142`

A [`Mesh`] or [`PaintCallback`] within a clip rectangle.

Public fields:

- `clip_rect: Rect` — Clip / scissor rectangle. Only show the part of the [`Mesh`] that falls within this.
- `primitive: Primitive` — What to paint - either a [`Mesh`] or a [`PaintCallback`].

Implements: `Clone`, `Debug`

### `ClippedShape` (struct) — `epaint-0.35.0/src/lib.rs:117`

A [`Shape`] within a clip rectangle.

Public fields:

- `clip_rect: Rect` — Clip / scissor rectangle. Only show the part of the [`Shape`] that falls within this.
- `shape: Shape` — The shape

Methods:

- `fn transform(&mut self, transform: TSTransform)` — `epaint-0.35.0/src/lib.rs:131`
  Transform (move/scale) the shape in-place.

Implements: `Clone`, `Debug`, `PartialEq`, `StructuralPartialEq`

### `Color32` (struct) — `ecolor-0.35.0/src/color32.rs:31`

This format is used for space-efficient color representation (32 bits).

Methods:

- `const fn a(&self) -> u8` — `ecolor-0.35.0/src/color32.rs:231`
  Alpha (opacity).
- `const fn additive(self) -> Self` — `ecolor-0.35.0/src/color32.rs:243`
  Returns an additive version of self
- `const fn b(&self) -> u8` — `ecolor-0.35.0/src/color32.rs:225`
  Blue component multiplied by alpha.
- `const fn from_additive_luminance(l: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:202`
  Additive white.
- `const fn from_black_alpha(a: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:190`
  Black with the given opacity.
- `const fn from_gray(l: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:184`
  Opaque gray.
- `const fn from_rgb(r: u8, g: u8, b: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:108`
  From RGB with alpha of 255 (opaque).
- `const fn from_rgb_additive(r: u8, g: u8, b: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:114`
  From RGB into an additive color (will make everything it blend with brighter).
- `const fn from_rgba_premultiplied(r: u8, g: u8, b: u8, a: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:122`
  From `sRGBA` with premultiplied alpha.
- `const fn from_rgba_unmultiplied_const(r: u8, g: u8, b: u8, a: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:164`
  Same as [`Self::from_rgba_unmultiplied`], but can be used in a const context.
- `const fn g(&self) -> u8` — `ecolor-0.35.0/src/color32.rs:219`
  Green component multiplied by alpha.
- `const fn is_opaque(&self) -> bool` — `ecolor-0.35.0/src/color32.rs:207`
- `const fn r(&self) -> u8` — `ecolor-0.35.0/src/color32.rs:213`
  Red component multiplied by alpha.
- `const fn to_array(&self) -> [u8; 4]` — `ecolor-0.35.0/src/color32.rs:256`
  Premultiplied RGBA
- `const fn to_tuple(&self) -> (u8, u8, u8, u8)` — `ecolor-0.35.0/src/color32.rs:262`
  Premultiplied RGBA
- `fn blend(self, on_top: Self) -> Self` — `ecolor-0.35.0/src/color32.rs:368`
  Blend two colors in gamma space, so that `self` is behind the argument.
- `fn from_hex(hex: &str) -> Result<Self, ParseHexColorError>` — `ecolor-0.35.0/src/hex_color_runtime.rs:143`
  Parses a color from a hex string.
- `fn from_rgba_unmultiplied(r: u8, g: u8, b: u8, a: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:133`
  From `sRGBA` with separate alpha.
- `fn from_white_alpha(a: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:196`
  White with the given opacity.
- `fn gamma_multiply(self, factor: f32) -> Self` — `ecolor-0.35.0/src/color32.rs:294`
  Multiply with 0.5 to make color half as opaque, perceptually.
- `fn gamma_multiply_u8(self, factor: u8) -> Self` — `ecolor-0.35.0/src/color32.rs:314`
  Multiply with 127 to make color half as opaque, perceptually.
- `fn intensity(&self) -> f32` — `ecolor-0.35.0/src/color32.rs:376`
  Intensity of the color.
- `fn is_additive(self) -> bool` — `ecolor-0.35.0/src/color32.rs:250`
  Is the alpha=0 ?
- `fn lerp_to_gamma(&self, other: Self, t: f32) -> Self` — `ecolor-0.35.0/src/color32.rs:356`
  Lerp this color towards `other` by `t` in gamma space.
- `fn linear_multiply(self, factor: f32) -> Self` — `ecolor-0.35.0/src/color32.rs:330`
  Multiply with 0.5 to make color half as opaque in linear space.
- `fn to_hex(&self) -> String` — `ecolor-0.35.0/src/hex_color_runtime.rs:162`
  Formats the color as a hex string.
- `fn to_normalized_gamma_f32(self) -> [f32; 4]` — `ecolor-0.35.0/src/color32.rs:345`
  Converts to floating point values in the range 0-1 without any gamma space conversion.
- `fn to_opaque(self) -> Self` — `ecolor-0.35.0/src/color32.rs:237`
  Returns an opaque version of self
- `fn to_srgba_unmultiplied(&self) -> [u8; 4]` — `ecolor-0.35.0/src/color32.rs:273`
  Convert to a normal "unmultiplied" RGBA color (i.e. with separate alpha).

Implements: `Add`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<Color32>`, `From<Hsva>`, `From<HsvaGamma>`, `From<Rgba>`, `Hash`, `Index<usize>`, `IndexMut<usize>`, `Mul`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Zeroable`

### `ColorImage` (struct) — `epaint-0.35.0/src/image.rs:48`

A 2D RGBA color image in RAM.

Public fields:

- `size: [usize; 2]` — width, height in texels.
- `source_size: Vec2` — Size of the original SVG image (if any), or just the texel size of the image.
- `pixels: Vec<Color32>` — The pixels, row by row, from top to bottom.

Methods:

- `fn as_raw(&self) -> &[u8]` — `epaint-0.35.0/src/image.rs:177`
  A view of the underlying data as `&[u8]`
- `fn as_raw_mut(&mut self) -> &mut [u8]` — `epaint-0.35.0/src/image.rs:183`
  A view of the underlying data as `&mut [u8]`
- `fn example() -> Self` — `epaint-0.35.0/src/image.rs:209`
  An example color image, useful for tests.
- `fn filled(size: [usize; 2], color: Color32) -> Self` — `epaint-0.35.0/src/image.rs:75`
  Create an image filled with the given color.
- `fn from_gray(size: [usize; 2], gray: &[u8]) -> Self` — `epaint-0.35.0/src/image.rs:146`
  Create a [`ColorImage`] from flat opaque gray data.
- `fn from_gray_iter(size: [usize; 2], gray_iter: impl Iterator<Item = u8>) -> Self` — `epaint-0.35.0/src/image.rs:163`
  Alternative method to `from_gray`. Create a [`ColorImage`] from iterator over flat opaque gray data.
- `fn from_rgb(size: [usize; 2], rgb: &[u8]) -> Self` — `epaint-0.35.0/src/image.rs:193`
  Create a [`ColorImage`] from flat RGB data.
- `fn from_rgba_premultiplied(size: [usize; 2], rgba: &[u8]) -> Self` — `epaint-0.35.0/src/image.rs:128`
- `fn from_rgba_unmultiplied(size: [usize; 2], rgba: &[u8]) -> Self` — `epaint-0.35.0/src/image.rs:113`
  Create a [`ColorImage`] from flat un-multiplied RGBA data.
- `fn height(&self) -> usize` — `epaint-0.35.0/src/image.rs:238`
- `fn new(size: [usize; 2], pixels: Vec<Color32>) -> Self` — `epaint-0.35.0/src/image.rs:61`
  Create an image filled with the given color.
- `fn region(&self, region: &Rect, pixels_per_point: Option<f32>) -> Self` — `epaint-0.35.0/src/image.rs:249`
  Create a new image from a patch of the current image.
- `fn region_by_pixels(&self, [x, y]: [usize; 2], [w, h]: [usize; 2]) -> Self` — `epaint-0.35.0/src/image.rs:273`
  Clone a sub-region as a new image.
- `fn width(&self) -> usize` — `epaint-0.35.0/src/image.rs:233`
- `fn with_source_size(self, source_size: Vec2) -> Self` — `epaint-0.35.0/src/image.rs:227`
  Set the source size of e.g. the original SVG image.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<ColorImage>`, `Index<(usize, usize)>`, `IndexMut<(usize, usize)>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `CornerRadius` (struct) — `epaint-0.35.0/src/corner_radius.rs:13`

How rounded the corners of things should be.

Public fields:

- `nw: u8` — Radius of the rounding of the North-West (left top) corner.
- `ne: u8` — Radius of the rounding of the North-East (right top) corner.
- `sw: u8` — Radius of the rounding of the South-West (left bottom) corner.
- `se: u8` — Radius of the rounding of the South-East (right bottom) corner.

Methods:

- `const fn same(radius: u8) -> Self` — `epaint-0.35.0/src/corner_radius.rs:59`
  Same rounding on all four corners.
- `fn at_least(self, min: u8) -> Self` — `epaint-0.35.0/src/corner_radius.rs:76`
  Make sure each corner has a rounding of at least this.
- `fn at_most(self, max: u8) -> Self` — `epaint-0.35.0/src/corner_radius.rs:87`
  Make sure each corner has a rounding of at most this.
- `fn average(&self) -> f32` — `epaint-0.35.0/src/corner_radius.rs:97`
  Average rounding of the corners.
- `fn is_same(self) -> bool` — `epaint-0.35.0/src/corner_radius.rs:70`
  Do all corners have the same rounding?

Implements: `Add`, `Add<u8>`, `AddAssign`, `AddAssign<u8>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Div<f32>`, `DivAssign<f32>`, `Eq`, `From<CornerRadius>`, `From<CornerRadiusF32>`, `From<f32>`, `From<u8>`, `Hash`, `Mul<f32>`, `MulAssign<f32>`, `PartialEq`, `Serialize`, `StructuralPartialEq`, `Sub`, `Sub<u8>`, `SubAssign`, `SubAssign<u8>`

### `CornerRadiusF32` (struct) — `epaint-0.35.0/src/corner_radius_f32.rs:8`

How rounded the corners of things should be, in `f32`.

Public fields:

- `nw: f32` — Radius of the rounding of the North-West (left top) corner.
- `ne: f32` — Radius of the rounding of the North-East (right top) corner.
- `sw: f32` — Radius of the rounding of the South-West (left bottom) corner.
- `se: f32` — Radius of the rounding of the South-East (right bottom) corner.

Methods:

- `const fn same(radius: f32) -> Self` — `epaint-0.35.0/src/corner_radius_f32.rs:76`
  Same rounding on all four corners.
- `fn at_least(&self, min: f32) -> Self` — `epaint-0.35.0/src/corner_radius_f32.rs:93`
  Make sure each corner has a rounding of at least this.
- `fn at_most(&self, max: f32) -> Self` — `epaint-0.35.0/src/corner_radius_f32.rs:104`
  Make sure each corner has a rounding of at most this.
- `fn is_same(&self) -> bool` — `epaint-0.35.0/src/corner_radius_f32.rs:87`
  Do all corners have the same rounding?

Implements: `Add`, `AddAssign`, `AddAssign<f32>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Div<f32>`, `DivAssign<f32>`, `From<CornerRadius>`, `From<CornerRadiusF32>`, `From<f32>`, `Mul<f32>`, `MulAssign<f32>`, `PartialEq`, `Serialize`, `StructuralPartialEq`, `Sub`, `SubAssign`, `SubAssign<f32>`

### `CubicBezierShape` (struct) — `epaint-0.35.0/src/shapes/bezier_shape.rs:15`

A cubic [Bézier Curve](https://en.wikipedia.org/wiki/B%C3%A9zier_curve).

Public fields:

- `points: [Pos2; 4]` — The first point is the starting point and the last one is the ending point of the curve.…
- `closed: bool`
- `fill: Color32`
- `stroke: PathStroke`

Methods:

- `fn find_cross_t(&self, epsilon: f32) -> Option<f32>` — `epaint-0.35.0/src/shapes/bezier_shape.rs:232`
  Find out the t value for the point where the curve is intersected with the base line. The base line is the li…
- `fn flatten(&self, tolerance: Option<f32>) -> Vec<Pos2>` — `epaint-0.35.0/src/shapes/bezier_shape.rs:299`
  find a set of points that approximate the cubic Bézier curve. the number of points is determined by the toler…
- `fn flatten_closed(&self, tolerance: Option<f32>, epsilon: Option<f32>) -> Vec<Vec<Pos2>>` — `epaint-0.35.0/src/shapes/bezier_shape.rs:315`
  find a set of points that approximate the cubic Bézier curve. the number of points is determined by the toler…
- `fn for_each_flattened_with_t<F>(&self, tolerance: f32, callback: &mut F)` — `epaint-0.35.0/src/shapes/bezier_shape.rs:366`
  Iterates through the curve invoking a callback at each point.
- `fn from_points_stroke(points: [Pos2; 4], closed: bool, fill: Color32, stroke: impl Into<PathStroke>) -> Self` — `epaint-0.35.0/src/shapes/bezier_shape.rs:30`
  Creates a cubic Bézier curve based on 4 points and stroke.
- `fn logical_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/bezier_shape.rs:88`
  Logical bounding rectangle (ignoring stroke width)
- `fn num_quadratics(&self, tolerance: f32) -> u32` — `epaint-0.35.0/src/shapes/bezier_shape.rs:179`
- `fn sample(&self, t: f32) -> Pos2` — `epaint-0.35.0/src/shapes/bezier_shape.rs:278`
  Calculate the point (x,y) at t based on the cubic Bézier curve equation. t is in [0.0,1.0] [Bézier Curve](htt…
- `fn split_range(&self, t_range: Range<f32>) -> Self` — `epaint-0.35.0/src/shapes/bezier_shape.rs:142`
  split the original cubic curve into a new one within a range.
- `fn to_path_shapes(&self, tolerance: Option<f32>, epsilon: Option<f32>) -> Vec<PathShape>` — `epaint-0.35.0/src/shapes/bezier_shape.rs:63`
  Convert the cubic Bézier curve to one or two [`PathShape`]'s. When the curve is closed and it has to intersec…
- `fn transform(&self, transform: &RectTransform) -> Self` — `epaint-0.35.0/src/shapes/bezier_shape.rs:45`
  Transform the curve with the given transform.
- `fn visual_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/bezier_shape.rs:79`
  The visual bounding rectangle (includes stroke width)

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `From<CubicBezierShape>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `EllipseShape` (struct) — `epaint-0.35.0/src/shapes/ellipse_shape.rs:6`

How to paint an ellipse.

Public fields:

- `center: Pos2`
- `radius: Vec2` — Radius is the vector (a, b) where the width of the Ellipse is 2a and the height is 2b
- `fill: Color32`
- `stroke: Stroke`
- `angle: f32` — Rotate ellipse by this many radians clockwise around its center.

Methods:

- `fn filled(center: Pos2, radius: Vec2, fill_color: impl Into<Color32>) -> Self` — `epaint-0.35.0/src/shapes/ellipse_shape.rs:20`
- `fn stroke(center: Pos2, radius: Vec2, stroke: impl Into<Stroke>) -> Self` — `epaint-0.35.0/src/shapes/ellipse_shape.rs:31`
- `fn visual_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/ellipse_shape.rs:59`
  The visual bounding rectangle (includes stroke width)
- `fn with_angle(self, angle: f32) -> Self` — `epaint-0.35.0/src/shapes/ellipse_shape.rs:44`
  Set the rotation of the ellipse (in radians, clockwise). The ellipse rotates around its center.
- `fn with_angle_and_pivot(self, angle: f32, pivot: Pos2) -> Self` — `epaint-0.35.0/src/shapes/ellipse_shape.rs:51`
  Set the rotation of the ellipse (in radians, clockwise) around a custom pivot point.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `From<EllipseShape>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `FontId` (struct) — `epaint-0.35.0/src/text/fonts.rs:27`

How to select a sized font.

Public fields:

- `size: f32` — Height in points.
- `family: FontFamily` — What font family to use.

Methods:

- `const fn monospace(size: f32) -> Self` — `epaint-0.35.0/src/text/fonts.rs:58`
- `const fn new(size: f32, family: FontFamily) -> Self` — `epaint-0.35.0/src/text/fonts.rs:48`
- `const fn proportional(size: f32) -> Self` — `epaint-0.35.0/src/text/fonts.rs:53`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Fonts` (struct) — `epaint-0.35.0/src/text/fonts.rs:713`

The collection of fonts used by `epaint`.

Public fields:

- `fonts: FontsImpl`

Methods:

- `fn begin_pass(&mut self, options: TextOptions)` — `epaint-0.35.0/src/text/fonts.rs:734`
  Call at the start of each frame with the latest known [`TextOptions`].
- `fn definitions(&self) -> &FontDefinitions` — `epaint-0.35.0/src/text/fonts.rs:762`
- `fn font_atlas_fill_ratio(&self) -> f32` — `epaint-0.35.0/src/text/fonts.rs:802`
  How full is the font atlas?
- `fn font_image_delta(&mut self) -> Option<ImageDelta>` — `epaint-0.35.0/src/text/fonts.rs:752`
  Call at the end of each frame (before painting) to get the change to the font texture since last call.
- `fn font_image_size(&self) -> [usize; 2]` — `epaint-0.35.0/src/text/fonts.rs:780`
  Current size of the font image. Pass this to [`crate::Tessellator`].
- `fn has_glyph(&mut self, font_id: &FontId, c: char) -> bool` — `epaint-0.35.0/src/text/fonts.rs:785`
  Can we display this glyph?
- `fn has_glyphs(&mut self, font_id: &FontId, s: &str) -> bool` — `epaint-0.35.0/src/text/fonts.rs:790`
  Can we display all the glyphs in this text?
- `fn image(&self) -> ColorImage` — `epaint-0.35.0/src/text/fonts.rs:774`
  The full font atlas image.
- `fn new(options: TextOptions, definitions: FontDefinitions) -> Self` — `epaint-0.35.0/src/text/fonts.rs:721`
  Create a new [`Fonts`] for text layout. This call is expensive, so only create one [`Fonts`] and then reuse i…
- `fn num_galleys_in_cache(&self) -> usize` — `epaint-0.35.0/src/text/fonts.rs:794`
- `fn options(&self) -> &TextOptions` — `epaint-0.35.0/src/text/fonts.rs:757`
- `fn texture_atlas(&self) -> &TextureAtlas` — `epaint-0.35.0/src/text/fonts.rs:768`
  The font atlas. Pass this to [`crate::Tessellator`].
- `fn with_pixels_per_point(&mut self, pixels_per_point: f32) -> FontsView<'_>` — `epaint-0.35.0/src/text/fonts.rs:807`
  Returns a [`FontsView`] with the given `pixels_per_point` that can be used to do text layout.

### `FontsView` (struct) — `epaint-0.35.0/src/text/fonts.rs:819`

The context's collection of fonts, with this context's `pixels_per_point`. This is what you use to do text layout.

Public fields:

- `fonts: &'a mut FontsImpl`

Methods:

- `fn definitions(&self) -> &FontDefinitions` — `epaint-0.35.0/src/text/fonts.rs:832`
- `fn families(&self) -> Vec<FontFamily>` — `epaint-0.35.0/src/text/fonts.rs:884`
  List of all known font families.
- `fn font_atlas_fill_ratio(&self) -> f32` — `epaint-0.35.0/src/text/fonts.rs:914`
  How full is the font atlas?
- `fn font_image_size(&self) -> [usize; 2]` — `epaint-0.35.0/src/text/fonts.rs:844`
  Current size of the font image. Pass this to [`crate::Tessellator`].
- `fn glyph_width(&mut self, font_id: &FontId, c: char) -> f32` — `epaint-0.35.0/src/text/fonts.rs:851`
  Width of this character in points.
- `fn has_glyph(&mut self, font_id: &FontId, c: char) -> bool` — `epaint-0.35.0/src/text/fonts.rs:858`
  Can we display this glyph?
- `fn has_glyphs(&mut self, font_id: &FontId, s: &str) -> bool` — `epaint-0.35.0/src/text/fonts.rs:863`
  Can we display all the glyphs in this text?
- `fn image(&self) -> ColorImage` — `epaint-0.35.0/src/text/fonts.rs:838`
  The full font atlas image.
- `fn layout(&mut self, text: String, font_id: FontId, color: Color32, wrap_width: f32) -> Arc<Galley>` — `epaint-0.35.0/src/text/fonts.rs:922`
  Will wrap text at the given width and line break at `\n`.
- `fn layout_delayed_color(&mut self, text: String, font_id: FontId, wrap_width: f32) -> Arc<Galley>` — `epaint-0.35.0/src/text/fonts.rs:951`
  Like [`Self::layout`], made for when you want to pick a color for the text later.
- `fn layout_job(&mut self, job: LayoutJob) -> Arc<Galley>` — `epaint-0.35.0/src/text/fonts.rs:896`
  Layout some text.
- `fn layout_no_wrap(&mut self, text: String, font_id: FontId, color: Color32) -> Arc<Galley>` — `epaint-0.35.0/src/text/fonts.rs:937`
  Will line break at `\n`.
- `fn num_galleys_in_cache(&self) -> usize` — `epaint-0.35.0/src/text/fonts.rs:906`
- `fn options(&self) -> &TextOptions` — `epaint-0.35.0/src/text/fonts.rs:827`
- `fn row_height(&mut self, font_id: &FontId) -> f32` — `epaint-0.35.0/src/text/fonts.rs:871`
  Height of one row of text in points.

### `Galley` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:729`

Text that has been laid out, ready for painting.

Public fields:

- `job: Arc<LayoutJob>` — The job that this galley is the result of. Contains the original string and style section…
- `rows: Vec<PlacedRow>` — Rows of text, from top to bottom, and their offsets.
- `elided: bool` — Set to true the text was truncated due to [`TextWrapping::max_rows`].
- `rect: Rect` — Bounding rect.
- `mesh_bounds: Rect` — Tight bounding box around all the meshes in all the rows. Can be used for culling.
- `num_vertices: usize` — Total number of vertices in all the row meshes.
- `num_indices: usize` — Total number of indices in all the row meshes.
- `pixels_per_point: f32` — The number of physical pixels for each logical point. Since this affects the layout, we k…

Methods:

- `fn begin(&self) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1228`
  Cursor to the first character.
- `fn clamp_cursor(&self, cursor: &CCursor) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1335`
- `fn concat(job: Arc<LayoutJob>, galleys: &[Arc<Self>], pixels_per_point: f32) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:1058`
  Append each galley under the previous one.
- `fn cursor_begin_of_paragraph(&self, cursor: &CCursor) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1406`
- `fn cursor_begin_of_row(&self, cursor: &CCursor) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1390`
- `fn cursor_down_one_row(&self, cursor: &CCursor, h_pos: Option<f32>) -> (CCursor, Option<f32>)` — `epaint-0.35.0/src/text/text_layout_types.rs:1364`
- `fn cursor_end_of_paragraph(&self, cursor: &CCursor) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1431`
- `fn cursor_end_of_row(&self, cursor: &CCursor) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1398`
- `fn cursor_from_pos(&self, pos: Vec2) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1174`
  Cursor at the given position within the galley.
- `fn cursor_left_one_character(&self, cursor: &CCursor) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1317`
- `fn cursor_right_one_character(&self, cursor: &CCursor) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1328`
- `fn cursor_up_one_row(&self, cursor: &CCursor, h_pos: Option<f32>) -> (CCursor, Option<f32>)` — `epaint-0.35.0/src/text/text_layout_types.rs:1339`
- `fn end(&self) -> CCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1233`
  Cursor to one-past last character.
- `fn intrinsic_size(&self) -> Vec2` — `epaint-0.35.0/src/text/text_layout_types.rs:1019`
  This is the size that a non-wrapped, non-truncated, non-justified version of the text would have.
- `fn is_empty(&self) -> bool` — `epaint-0.35.0/src/text/text_layout_types.rs:999`
- `fn layout_from_cursor(&self, cursor: CCursor) -> LayoutCursor` — `epaint-0.35.0/src/text/text_layout_types.rs:1252`
- `fn pos_from_cursor(&self, cursor: CCursor) -> Rect` — `epaint-0.35.0/src/text/text_layout_types.rs:1163`
  Returns a 0-width Rect.
- `fn pos_from_layout_cursor(&self, layout_cursor: &LayoutCursor) -> Rect` — `epaint-0.35.0/src/text/text_layout_types.rs:1153`
  Returns a 0-width Rect.
- `fn size(&self) -> Vec2` — `epaint-0.35.0/src/text/text_layout_types.rs:1010`
- `fn text(&self) -> &str` — `epaint-0.35.0/src/text/text_layout_types.rs:1005`
  The full, non-elided text of the input job.

Implements: `AsRef<str>`, `Borrow<str>`, `Clone`, `Debug`, `Deref`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Hsva` (struct) — `ecolor-0.35.0/src/hsva.rs:9`

Hue, saturation, value, alpha. All in the range [0, 1]. No premultiplied alpha.

Public fields:

- `h: f32` — hue 0-1
- `s: f32` — saturation 0-1
- `v: f32` — value 0-1
- `a: f32` — alpha 0-1. A negative value signifies an additive color (and alpha is ignored).

Methods:

- `fn from_additive_rgb(rgb: [f32; 3]) -> Self` — `ecolor-0.35.0/src/hsva.rs:66`
- `fn from_additive_srgb([r, g, b]: [u8; 3]) -> Self` — `ecolor-0.35.0/src/hsva.rs:77`
- `fn from_rgb(rgb: [f32; 3]) -> Self` — `ecolor-0.35.0/src/hsva.rs:86`
- `fn from_rgba_premultiplied(r: f32, g: f32, b: f32, a: f32) -> Self` — `ecolor-0.35.0/src/hsva.rs:43`
  From linear RGBA with premultiplied alpha
- `fn from_rgba_unmultiplied(r: f32, g: f32, b: f32, a: f32) -> Self` — `ecolor-0.35.0/src/hsva.rs:59`
  From linear RGBA without premultiplied alpha
- `fn from_srgb([r, g, b]: [u8; 3]) -> Self` — `ecolor-0.35.0/src/hsva.rs:92`
- `fn from_srgba_premultiplied([r, g, b, a]: [u8; 4]) -> Self` — `ecolor-0.35.0/src/hsva.rs:31`
  From `sRGBA` with premultiplied alpha
- `fn from_srgba_unmultiplied([r, g, b, a]: [u8; 4]) -> Self` — `ecolor-0.35.0/src/hsva.rs:37`
  From `sRGBA` without premultiplied alpha
- `fn new(h: f32, s: f32, v: f32, a: f32) -> Self` — `ecolor-0.35.0/src/hsva.rs:25`
- `fn to_opaque(self) -> Self` — `ecolor-0.35.0/src/hsva.rs:103`
- `fn to_rgb(&self) -> [f32; 3]` — `ecolor-0.35.0/src/hsva.rs:108`
- `fn to_rgba_premultiplied(&self) -> [f32; 4]` — `ecolor-0.35.0/src/hsva.rs:123`
- `fn to_rgba_unmultiplied(&self) -> [f32; 4]` — `ecolor-0.35.0/src/hsva.rs:137`
  To linear space rgba in 0-1 range.
- `fn to_srgb(&self) -> [u8; 3]` — `ecolor-0.35.0/src/hsva.rs:113`
- `fn to_srgba_premultiplied(&self) -> [u8; 4]` — `ecolor-0.35.0/src/hsva.rs:144`
- `fn to_srgba_unmultiplied(&self) -> [u8; 4]` — `ecolor-0.35.0/src/hsva.rs:150`
  To gamma-space 0-255.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `From<Color32>`, `From<Hsva>`, `From<HsvaGamma>`, `From<Rgba>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `HsvaGamma` (struct) — `ecolor-0.35.0/src/hsva_gamma.rs:6`

Like Hsva but with the `v` value (brightness) being gamma corrected so that it is somewhat perceptually even.

Public fields:

- `h: f32` — hue 0-1
- `s: f32` — saturation 0-1
- `v: f32` — value 0-1, in gamma-space (~perceptually even)
- `a: f32` — alpha 0-1. A negative value signifies an additive color (and alpha is ignored).

Implements: `Clone`, `Copy`, `Debug`, `Default`, `From<Color32>`, `From<Hsva>`, `From<HsvaGamma>`, `From<Rgba>`, `PartialEq`, `StructuralPartialEq`

### `ImageDelta` (struct) — `epaint-0.35.0/src/image.rs:456`

A change to an image.

Public fields:

- `image: ImageData` — What to set the texture to.
- `options: TextureOptions`
- `pos: Option<[usize; 2]>` — If `None`, set the whole texture to [`Self::image`].

Methods:

- `fn full(image: impl Into<ImageData>, options: TextureOptions) -> Self` — `epaint-0.35.0/src/image.rs:474`
  Update the whole texture.
- `fn is_whole(&self) -> bool` — `epaint-0.35.0/src/image.rs:493`
  Is this affecting the whole texture? If `false`, this is a partial (sub-region) update.
- `fn partial(pos: [usize; 2], image: impl Into<ImageData>, options: TextureOptions) -> Self` — `epaint-0.35.0/src/image.rs:483`
  Update a sub-region of an existing texture.

Implements: `Clone`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Margin` (struct) — `epaint-0.35.0/src/margin.rs:15`

A value for all four sides of a rectangle, often used to express padding or spacing.

Public fields:

- `left: i8`
- `right: i8`
- `top: i8`
- `bottom: i8`

Methods:

- `const fn bottomf(self) -> f32` — `epaint-0.35.0/src/margin.rs:73`
  Bottom margin, as `f32`
- `const fn is_same(self) -> bool` — `epaint-0.35.0/src/margin.rs:96`
  Are the margin on every side the same?
- `const fn left_top(self) -> Vec2` — `epaint-0.35.0/src/margin.rs:84`
- `const fn leftf(self) -> f32` — `epaint-0.35.0/src/margin.rs:55`
  Left margin, as `f32`
- `const fn right_bottom(self) -> Vec2` — `epaint-0.35.0/src/margin.rs:89`
- `const fn rightf(self) -> f32` — `epaint-0.35.0/src/margin.rs:61`
  Right margin, as `f32`
- `const fn same(margin: i8) -> Self` — `epaint-0.35.0/src/margin.rs:33`
  The same margin on every side.
- `const fn symmetric(x: i8, y: i8) -> Self` — `epaint-0.35.0/src/margin.rs:44`
  Margins with the same size on opposing sides
- `const fn topf(self) -> f32` — `epaint-0.35.0/src/margin.rs:67`
  Top margin, as `f32`
- `fn sum(self) -> Vec2` — `epaint-0.35.0/src/margin.rs:79`
  Total margins on both sides

Implements: `Add`, `Add<Margin>`, `Add<i8>`, `AddAssign<Margin>`, `AddAssign<i8>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Div<f32>`, `DivAssign<f32>`, `Eq`, `From<Margin>`, `From<MarginF32>`, `From<Vec2>`, `From<f32>`, `From<i8>`, `Mul<f32>`, `MulAssign<f32>`, `PartialEq`, `Serialize`, `StructuralPartialEq`, `Sub`, `Sub<Margin>`, `Sub<i8>`, `SubAssign<Margin>`, `SubAssign<i8>`

### `MarginF32` (struct) — `epaint-0.35.0/src/margin_f32.rs:13`

A value for all four sides of a rectangle, often used to express padding or spacing.

Public fields:

- `left: f32`
- `right: f32`
- `top: f32`
- `bottom: f32`

Methods:

- `const fn left_top(&self) -> Vec2` — `epaint-0.35.0/src/margin_f32.rs:82`
- `const fn right_bottom(&self) -> Vec2` — `epaint-0.35.0/src/margin_f32.rs:87`
- `const fn same(margin: f32) -> Self` — `epaint-0.35.0/src/margin_f32.rs:55`
  The same margin on every side.
- `const fn symmetric(x: f32, y: f32) -> Self` — `epaint-0.35.0/src/margin_f32.rs:66`
  Margins with the same size on opposing sides
- `fn is_same(&self) -> bool` — `epaint-0.35.0/src/margin_f32.rs:94`
  Are the margin on every side the same?
- `fn sum(&self) -> Vec2` — `epaint-0.35.0/src/margin_f32.rs:77`
  Total margins on both sides

Implements: `Add`, `Add<MarginF32>`, `Add<f32>`, `AddAssign<MarginF32>`, `AddAssign<f32>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Div<f32>`, `DivAssign<f32>`, `From<Margin>`, `From<MarginF32>`, `From<Vec2>`, `From<f32>`, `Mul<f32>`, `MulAssign<f32>`, `PartialEq`, `Serialize`, `StructuralPartialEq`, `Sub`, `Sub<MarginF32>`, `Sub<f32>`, `SubAssign<MarginF32>`, `SubAssign<f32>`

### `Mesh` (struct) — `epaint-0.35.0/src/mesh.rs:60`

Textured triangles in two dimensions.

Public fields:

- `indices: Vec<u32>` — Draw as triangles (i.e. the length is always multiple of three).
- `vertices: Vec<Vertex>` — The vertex data indexed by `indices`.
- `texture_id: TextureId` — The texture to use when drawing these triangles.

Methods:

- `fn add_colored_rect(&mut self, rect: Rect, color: Color32)` — `epaint-0.35.0/src/mesh.rs:231`
  Uniformly colored rectangle.
- `fn add_rect_with_uv(&mut self, rect: Rect, uv: Rect, color: Color32)` — `epaint-0.35.0/src/mesh.rs:199`
  Rectangle with a texture and color.
- `fn add_triangle(&mut self, a: u32, b: u32, c: u32)` — `epaint-0.35.0/src/mesh.rs:179`
  Add a triangle.
- `fn append(&mut self, other: Self)` — `epaint-0.35.0/src/mesh.rs:132`
  Append all the indices and vertices of `other` to `self`.
- `fn append_ref(&mut self, other: &Self)` — `epaint-0.35.0/src/mesh.rs:147`
  Append all the indices and vertices of `other` to `self` without taking ownership.
- `fn bytes_used(&self) -> usize` — `epaint-0.35.0/src/mesh.rs:92`
  Returns the amount of memory used by the vertices and indices.
- `fn calc_bounds(&self) -> Rect` — `epaint-0.35.0/src/mesh.rs:121`
  Calculate a bounding rectangle.
- `fn clear(&mut self)` — `epaint-0.35.0/src/mesh.rs:85`
  Restore to default state, but without freeing memory.
- `fn colored_vertex(&mut self, pos: Pos2, color: Color32)` — `epaint-0.35.0/src/mesh.rs:169`
  Add a colored vertex.
- `fn is_empty(&self) -> bool` — `epaint-0.35.0/src/mesh.rs:109`
- `fn is_valid(&self) -> bool` — `epaint-0.35.0/src/mesh.rs:99`
  Are all indices within the bounds of the contained vertices?
- `fn reserve_triangles(&mut self, additional_triangles: usize)` — `epaint-0.35.0/src/mesh.rs:186`
  Make room for this many additional triangles (will reserve 3x as many indices). See also `reserve_vertices`.
- `fn reserve_vertices(&mut self, additional: usize)` — `epaint-0.35.0/src/mesh.rs:193`
  Make room for this many additional vertices. See also `reserve_triangles`.
- `fn rotate(&mut self, rot: Rot2, origin: Pos2)` — `epaint-0.35.0/src/mesh.rs:325`
  Rotate by some angle about an origin, in-place.
- `fn split_to_u16(self) -> Vec<Mesh16>` — `epaint-0.35.0/src/mesh.rs:243`
  This is for platforms that only support 16-bit index buffers.
- `fn transform(&mut self, transform: TSTransform)` — `epaint-0.35.0/src/mesh.rs:316`
  Transform the mesh in-place with the given transform.
- `fn translate(&mut self, delta: Vec2)` — `epaint-0.35.0/src/mesh.rs:309`
  Translate location by this much, in-place
- `fn triangles(&self) -> impl Iterator<Item = [u32; 3]> + '_` — `epaint-0.35.0/src/mesh.rs:114`
  Iterate over the triangles of this mesh, returning vertex indices.
- `fn with_texture(texture_id: TextureId) -> Self` — `epaint-0.35.0/src/mesh.rs:77`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<Mesh>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Mesh16` (struct) — `epaint-0.35.0/src/mesh.rs:337`

A version of [`Mesh`] that uses 16-bit indices.

Public fields:

- `indices: Vec<u16>` — Draw as triangles (i.e. the length is always multiple of three).
- `vertices: Vec<Vertex>` — The vertex data indexed by `indices`.
- `texture_id: TextureId` — The texture to use when drawing these triangles.

Methods:

- `fn is_valid(&self) -> bool` — `epaint-0.35.0/src/mesh.rs:352`
  Are all indices within the bounds of the contained vertices?

### `PaintCallback` (struct) — `epaint-0.35.0/src/shapes/paint_callback.rs:59`

If you want to paint some 3D shapes inside an egui region, you can use this.

Public fields:

- `rect: Rect` — Where to paint.
- `callback: Arc<dyn Any + Send + Sync>` — Paint something custom (e.g. 3D stuff).

Implements: `Clone`, `Debug`, `From<PaintCallback>`, `PartialEq`

### `PaintCallbackInfo` (struct) — `epaint-0.35.0/src/shapes/paint_callback.rs:6`

Information passed along with [`PaintCallback`] ([`Shape::Callback`]).

Public fields:

- `viewport: Rect` — Viewport in points.
- `clip_rect: Rect` — Clip rectangle in points.
- `pixels_per_point: f32` — Pixels per point.
- `screen_size_px: [u32; 2]` — Full size of the screen, in pixels.

Methods:

- `fn clip_rect_in_pixels(&self) -> ViewportInPixels` — `epaint-0.35.0/src/shapes/paint_callback.rs:50`
  The "scissor" or "clip" rectangle. This is what you would use in e.g. `glScissor`.
- `fn viewport_in_pixels(&self) -> ViewportInPixels` — `epaint-0.35.0/src/shapes/paint_callback.rs:45`
  The viewport rectangle. This is what you would use in e.g. `glViewport`.

### `PaintStats` (struct) — `epaint-0.35.0/src/stats.rs:160`

Collected allocation statistics for shapes and meshes.

Public fields:

- `shapes: AllocInfo`
- `shape_text: AllocInfo`
- `shape_path: AllocInfo`
- `shape_mesh: AllocInfo`
- `shape_vec: AllocInfo`
- `num_callbacks: usize`
- `text_shape_vertices: AllocInfo`
- `text_shape_indices: AllocInfo`
- `clipped_primitives: AllocInfo` — Number of separate clip rectangles
- `vertices: AllocInfo`
- `indices: AllocInfo`

Methods:

- `fn from_shapes(shapes: &[ClippedShape]) -> Self` — `epaint-0.35.0/src/stats.rs:178`
- `fn with_clipped_primitives(self, clipped_primitives: &[ClippedPrimitive]) -> Self` — `epaint-0.35.0/src/stats.rs:227`

Implements: `Clone`, `Copy`, `Default`

### `PathShape` (struct) — `epaint-0.35.0/src/shapes/path_shape.rs:6`

A path which can be stroked and/or filled (if closed).

Public fields:

- `points: Vec<Pos2>` — Filled paths should prefer clockwise order.
- `closed: bool` — If true, connect the first and last of the points together. This is required if `fill !=…
- `fill: Color32` — Fill is only supported for convex polygons.
- `stroke: PathStroke` — Color and thickness of the line.

Methods:

- `fn closed_line(points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> Self` — `epaint-0.35.0/src/shapes/path_shape.rs:39`
  A line that closes back to the start point again.
- `fn convex_polygon(points: Vec<Pos2>, fill: impl Into<Color32>, stroke: impl Into<PathStroke>) -> Self` — `epaint-0.35.0/src/shapes/path_shape.rs:52`
  A convex polygon with a fill and optional stroke.
- `fn line(points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> Self` — `epaint-0.35.0/src/shapes/path_shape.rs:28`
  A line through many points.
- `fn visual_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/path_shape.rs:67`
  The visual bounding rectangle (includes stroke width)

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `From<PathShape>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `PathStroke` (struct) — `epaint-0.35.0/src/stroke.rs:117`

Describes the width and color of paths. The color can either be solid or provided by a callback. For more information, see [`ColorMode`]

Public fields:

- `width: f32`
- `color: ColorMode`
- `kind: StrokeKind`

Methods:

- `fn inside(self) -> Self` — `epaint-0.35.0/src/stroke.rs:187`
  Set the stroke to be painted entirely inside of the shape
- `fn is_empty(&self) -> bool` — `epaint-0.35.0/src/stroke.rs:196`
  True if width is zero or color is solid and transparent
- `fn middle(self) -> Self` — `epaint-0.35.0/src/stroke.rs:169`
  Set the stroke to be painted right on the edge of the shape, half inside and half outside.
- `fn new(width: f32, color: impl Into<Color32>) -> Self` — `epaint-0.35.0/src/stroke.rs:139`
- `fn new_uv(width: f32, callback: impl Fn(Rect, Pos2) -> Color32 + Send + Sync + 'static) -> Self` — `epaint-0.35.0/src/stroke.rs:151`
  Create a new `PathStroke` with a UV function
- `fn outside(self) -> Self` — `epaint-0.35.0/src/stroke.rs:178`
  Set the stroke to be painted entirely outside of the shape
- `fn with_kind(self, kind: StrokeKind) -> Self` — `epaint-0.35.0/src/stroke.rs:163`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `From<(f32, Color)>`, `From<Stroke>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Pos2` (struct) — `emath-0.35.0/src/pos2.rs:18`

A position on screen.

Public fields:

- `x: f32` — How far to the right.
- `y: f32` — How far down.

Methods:

- `const fn new(x: f32, y: f32) -> Self` — `emath-0.35.0/src/pos2.rs:128`
- `fn any_nan(self) -> bool` — `emath-0.35.0/src/pos2.rs:175`
  True if any member is NaN.
- `fn ceil(self) -> Self` — `emath-0.35.0/src/pos2.rs:163`
- `fn clamp(self, min: Self, max: Self) -> Self` — `emath-0.35.0/src/pos2.rs:193`
- `fn distance(self, other: Self) -> f32` — `emath-0.35.0/src/pos2.rs:143`
- `fn distance_sq(self, other: Self) -> f32` — `emath-0.35.0/src/pos2.rs:148`
- `fn floor(self) -> Self` — `emath-0.35.0/src/pos2.rs:153`
- `fn is_finite(self) -> bool` — `emath-0.35.0/src/pos2.rs:169`
  True if all members are also finite.
- `fn lerp(&self, other: Self, t: f32) -> Self` — `emath-0.35.0/src/pos2.rs:201`
  Linearly interpolate towards another point, so that `0.0 => self, 1.0 => other`.
- `fn max(self, other: Self) -> Self` — `emath-0.35.0/src/pos2.rs:187`
- `fn min(self, other: Self) -> Self` — `emath-0.35.0/src/pos2.rs:181`
- `fn round(self) -> Self` — `emath-0.35.0/src/pos2.rs:158`
- `fn to_vec2(self) -> Vec2` — `emath-0.35.0/src/pos2.rs:135`
  The vector from origin to this position. `p.to_vec2()` is equivalent to `p - Pos2::default()`.

Implements: `Add<Vec2>`, `AddAssign<Vec2>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Display`, `Div<f32>`, `Eq`, `From<&(f32, f32)>`, `From<&Pos2>`, `From<&[f32; 2]>`, `From<(f32, f32)>`, `From<Pos2>`, `From<[f32; 2]>`, `GuiRounding`, `Index<usize>`, `IndexMut<usize>`, `Mul<Pos2>`, `Mul<f32>`, `MulAssign<f32>`, `NumExt`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Sub`, `Sub<Vec2>`, `SubAssign<Vec2>`, `Zeroable`

### `QuadraticBezierShape` (struct) — `epaint-0.35.0/src/shapes/bezier_shape.rs:385`

A quadratic [Bézier Curve](https://en.wikipedia.org/wiki/B%C3%A9zier_curve).

Public fields:

- `points: [Pos2; 3]` — The first point is the starting point and the last one is the ending point of the curve.…
- `closed: bool`
- `fill: Color32`
- `stroke: PathStroke`

Methods:

- `fn flatten(&self, tolerance: Option<f32>) -> Vec<Pos2>` — `epaint-0.35.0/src/shapes/bezier_shape.rs:522`
  find a set of points that approximate the quadratic Bézier curve. the number of points is determined by the t…
- `fn for_each_flattened_with_t<F>(&self, tolerance: f32, callback: &mut F)` — `epaint-0.35.0/src/shapes/bezier_shape.rs:540`
  Compute a flattened approximation of the curve, invoking a callback at each step.
- `fn from_points_stroke(points: [Pos2; 3], closed: bool, fill: Color32, stroke: impl Into<PathStroke>) -> Self` — `epaint-0.35.0/src/shapes/bezier_shape.rs:401`
  Create a new quadratic Bézier shape based on the 3 points and stroke.
- `fn logical_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/bezier_shape.rs:451`
  Logical bounding rectangle (ignoring stroke width)
- `fn sample(&self, t: f32) -> Pos2` — `epaint-0.35.0/src/shapes/bezier_shape.rs:503`
  Calculate the point (x,y) at t based on the quadratic Bézier curve equation. t is in [0.0,1.0] [Bézier Curve]…
- `fn to_path_shape(&self, tolerance: Option<f32>) -> PathShape` — `epaint-0.35.0/src/shapes/bezier_shape.rs:431`
  Convert the quadratic Bézier curve to one [`PathShape`]. The `tolerance` will be used to control the max dist…
- `fn transform(&self, transform: &RectTransform) -> Self` — `epaint-0.35.0/src/shapes/bezier_shape.rs:416`
  Transform the curve with the given transform.
- `fn visual_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/bezier_shape.rs:442`
  The visual bounding rectangle (includes stroke width)

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `From<QuadraticBezierShape>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Rect` (struct) — `emath-0.35.0/src/rect.rs:25`

A rectangular region of space.

Public fields:

- `min: Pos2` — One of the corners of the rectangle, usually the left top one.
- `max: Pos2` — The other corner, opposing [`Self::min`]. Usually the right bottom one.

Methods:

- `const fn from_min_max(min: Pos2, max: Pos2) -> Self` — `emath-0.35.0/src/rect.rs:73`
- `fn any_nan(self) -> bool` — `emath-0.35.0/src/rect.rs:530`
  True if any member is NaN.
- `fn area(&self) -> f32` — `emath-0.35.0/src/rect.rs:381`
  This is never negative, and instead returns zero for negative rectangles.
- `fn aspect_ratio(&self) -> f32` — `emath-0.35.0/src/rect.rs:362`
  Width / height
- `fn bottom(&self) -> f32` — `emath-0.35.0/src/rect.rs:593`
  `max.y`
- `fn bottom_mut(&mut self) -> &mut f32` — `emath-0.35.0/src/rect.rs:599`
  `max.y`
- `fn bottom_up_range(&self) -> Rangef` — `emath-0.35.0/src/rect.rs:506`
- `fn center(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:332`
- `fn center_bottom(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:643`
- `fn center_top(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:616`
- `fn clamp(&self, p: Pos2) -> Pos2` — `emath-0.35.0/src/rect.rs:286`
  Return the given points clamped to be inside the rectangle Panics if [`Self::is_negative`].
- `fn contains(&self, p: Pos2) -> bool` — `emath-0.35.0/src/rect.rs:274`
- `fn contains_rect(&self, other: Self) -> bool` — `emath-0.35.0/src/rect.rs:279`
- `fn distance_sq_to_pos(&self, pos: Pos2) -> f32` — `emath-0.35.0/src/rect.rs:401`
  The distance from the rect to the position, squared.
- `fn distance_to_pos(&self, pos: Pos2) -> f32` — `emath-0.35.0/src/rect.rs:391`
  The distance from the rect to the position.
- `fn everything_above(bottom_y: f32) -> Self` — `emath-0.35.0/src/rect.rs:157`
  A [`Rect`] that contains every point above a certain y coordinate
- `fn everything_below(top_y: f32) -> Self` — `emath-0.35.0/src/rect.rs:149`
  A [`Rect`] that contains every point below a certain y coordinate
- `fn everything_left_of(right_x: f32) -> Self` — `emath-0.35.0/src/rect.rs:141`
  A [`Rect`] that contains every point to the left of the given X coordinate.
- `fn everything_right_of(left_x: f32) -> Self` — `emath-0.35.0/src/rect.rs:133`
  A [`Rect`] that contains every point to the right of the given X coordinate.
- `fn expand(self, amnt: f32) -> Self` — `emath-0.35.0/src/rect.rs:193`
  Expand by this much in each direction, keeping the center
- `fn expand2(self, amnt: Vec2) -> Self` — `emath-0.35.0/src/rect.rs:199`
  Expand by this much in each direction, keeping the center
- `fn extend_with(&mut self, p: Pos2)` — `emath-0.35.0/src/rect.rs:291`
- `fn extend_with_x(&mut self, x: f32)` — `emath-0.35.0/src/rect.rs:298`
  Expand to include the given x coordinate
- `fn extend_with_y(&mut self, y: f32)` — `emath-0.35.0/src/rect.rs:305`
  Expand to include the given y coordinate
- `fn from_center_size(center: Pos2, size: Vec2) -> Self` — `emath-0.35.0/src/rect.rs:87`
- `fn from_min_size(min: Pos2, size: Vec2) -> Self` — `emath-0.35.0/src/rect.rs:79`
  left-top corner plus a size (stretching right-down).
- `fn from_points(points: &[Pos2]) -> Self` — `emath-0.35.0/src/rect.rs:123`
  Bounding-box around the points.
- `fn from_pos(point: Pos2) -> Self` — `emath-0.35.0/src/rect.rs:115`
  A zero-sized rect at a specific point.
- `fn from_two_pos(a: Pos2, b: Pos2) -> Self` — `emath-0.35.0/src/rect.rs:106`
  Returns the bounding rectangle of the two points.
- `fn from_x_y_ranges(x_range: impl Into<Rangef>, y_range: impl Into<Rangef>) -> Self` — `emath-0.35.0/src/rect.rs:95`
- `fn height(&self) -> f32` — `emath-0.35.0/src/rect.rs:353`
  Note: this can be negative.
- `fn intersect(self, other: Self) -> Self` — `emath-0.35.0/src/rect.rs:324`
  The intersection of two [`Rect`], i.e. the area covered by both.
- `fn intersects(self, other: Self) -> bool` — `emath-0.35.0/src/rect.rs:250`
- `fn intersects_ray(&self, o: Pos2, d: Vec2) -> bool` — `emath-0.35.0/src/rect.rs:682`
  Does this Rect intersect the given ray (where `d` is normalized)?
- `fn intersects_ray_from_center(&self, d: Vec2) -> Pos2` — `emath-0.35.0/src/rect.rs:714`
  Where does a ray from the center intersect the rectangle?
- `fn is_finite(&self) -> bool` — `emath-0.35.0/src/rect.rs:524`
  True if all members are also finite.
- `fn is_negative(&self) -> bool` — `emath-0.35.0/src/rect.rs:512`
  `width < 0 || height < 0`
- `fn is_positive(&self) -> bool` — `emath-0.35.0/src/rect.rs:518`
  `width > 0 && height > 0`
- `fn left(&self) -> f32` — `emath-0.35.0/src/rect.rs:539`
  `min.x`
- `fn left_bottom(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:638`
- `fn left_center(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:627`
- `fn left_mut(&mut self) -> &mut f32` — `emath-0.35.0/src/rect.rs:545`
  `min.x`
- `fn left_top(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:611`
- `fn lerp_inside(&self, t: impl Into<Vec2>) -> Pos2` — `emath-0.35.0/src/rect.rs:452`
  Linearly interpolate so that `[0, 0]` is [`Self::min`] and `[1, 1]` is [`Self::max`].
- `fn lerp_towards(&self, other: &Self, t: f32) -> Self` — `emath-0.35.0/src/rect.rs:462`
  Linearly self towards other rect.
- `fn range_along(&self, axis: usize) -> Rangef` — `emath-0.35.0/src/rect.rs:486`
  The extent along the given axis: `0` for x, `1` for y.
- `fn right(&self) -> f32` — `emath-0.35.0/src/rect.rs:557`
  `max.x`
- `fn right_bottom(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:649`
- `fn right_center(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:632`
- `fn right_mut(&mut self) -> &mut f32` — `emath-0.35.0/src/rect.rs:563`
  `max.x`
- `fn right_top(&self) -> Pos2` — `emath-0.35.0/src/rect.rs:622`
- `fn rotate_bb(self, rot: Rot2) -> Self` — `emath-0.35.0/src/rect.rs:236`
  Rotate the bounds (will expand the [`Rect`])
- `fn scale_from_center(self, scale_factor: f32) -> Self` — `emath-0.35.0/src/rect.rs:205`
  Scale up by this factor in each direction, keeping the center
- `fn scale_from_center2(self, scale_factor: Vec2) -> Self` — `emath-0.35.0/src/rect.rs:211`
  Scale up by this factor in each direction, keeping the center
- `fn set_bottom(&mut self, y: f32)` — `emath-0.35.0/src/rect.rs:605`
  `max.y`
- `fn set_center(&mut self, center: Pos2)` — `emath-0.35.0/src/rect.rs:268`
  Keep size
- `fn set_height(&mut self, h: f32)` — `emath-0.35.0/src/rect.rs:263`
  keep min
- `fn set_left(&mut self, x: f32)` — `emath-0.35.0/src/rect.rs:551`
  `min.x`
- `fn set_right(&mut self, x: f32)` — `emath-0.35.0/src/rect.rs:569`
  `max.x`
- `fn set_top(&mut self, y: f32)` — `emath-0.35.0/src/rect.rs:587`
  `min.y`
- `fn set_width(&mut self, w: f32)` — `emath-0.35.0/src/rect.rs:258`
  keep min
- `fn shrink(self, amnt: f32) -> Self` — `emath-0.35.0/src/rect.rs:217`
  Shrink by this much in each direction, keeping the center
- `fn shrink2(self, amnt: Vec2) -> Self` — `emath-0.35.0/src/rect.rs:223`
  Shrink by this much in each direction, keeping the center
- `fn signed_distance_to_pos(&self, pos: Pos2) -> f32` — `emath-0.35.0/src/rect.rs:438`
  Signed distance to the edge of the box.
- `fn size(&self) -> Vec2` — `emath-0.35.0/src/rect.rs:341`
  `rect.size() == Vec2 { x: rect.width(), y: rect.height() }`
- `fn size_along(&self, axis: usize) -> f32` — `emath-0.35.0/src/rect.rs:501`
  The size along the given axis: `0` for x (width), `1` for y (height).
- `fn split_left_right_at_fraction(&self, t: f32) -> (Self, Self)` — `emath-0.35.0/src/rect.rs:654`
  Split rectangle in left and right halves. `t` is expected to be in the (0,1) range.
- `fn split_left_right_at_x(&self, split_x: f32) -> (Self, Self)` — `emath-0.35.0/src/rect.rs:659`
  Split rectangle in left and right halves at the given `x` coordinate.
- `fn split_top_bottom_at_fraction(&self, t: f32) -> (Self, Self)` — `emath-0.35.0/src/rect.rs:666`
  Split rectangle in top and bottom halves. `t` is expected to be in the (0,1) range.
- `fn split_top_bottom_at_y(&self, split_y: f32) -> (Self, Self)` — `emath-0.35.0/src/rect.rs:671`
  Split rectangle in top and bottom halves at the given `y` coordinate.
- `fn square_proportions(&self) -> Vec2` — `emath-0.35.0/src/rect.rs:369`
  `[2, 1]` for wide screen, and `[1, 2]` for portrait, etc. At least one dimension = 1, the other >= 1 Returns…
- `fn top(&self) -> f32` — `emath-0.35.0/src/rect.rs:575`
  `min.y`
- `fn top_mut(&mut self) -> &mut f32` — `emath-0.35.0/src/rect.rs:581`
  `min.y`
- `fn translate(self, amnt: Vec2) -> Self` — `emath-0.35.0/src/rect.rs:229`
- `fn union(self, other: Self) -> Self` — `emath-0.35.0/src/rect.rs:314`
  The union of two bounding rectangle, i.e. the minimum [`Rect`] that contains both input rectangles.
- `fn width(&self) -> f32` — `emath-0.35.0/src/rect.rs:347`
  Note: this can be negative.
- `fn with_max_x(self, max_x: f32) -> Self` — `emath-0.35.0/src/rect.rs:179`
- `fn with_max_y(self, max_y: f32) -> Self` — `emath-0.35.0/src/rect.rs:186`
- `fn with_min_x(self, min_x: f32) -> Self` — `emath-0.35.0/src/rect.rs:165`
- `fn with_min_y(self, min_y: f32) -> Self` — `emath-0.35.0/src/rect.rs:172`
- `fn x_range(&self) -> Rangef` — `emath-0.35.0/src/rect.rs:470`
- `fn y_range(&self) -> Rangef` — `emath-0.35.0/src/rect.rs:475`

Implements: `BitOr`, `BitOrAssign`, `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Display`, `Div<f32>`, `Eq`, `From<[Pos2; 2]>`, `GuiRounding`, `Mul<Rect>`, `Mul<f32>`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Zeroable`

### `RectShape` (struct) — `epaint-0.35.0/src/shapes/rect_shape.rs:8`

How to paint a rectangle.

Public fields:

- `rect: Rect`
- `corner_radius: CornerRadius` — How rounded the corners of the rectangle are.
- `fill: Color32` — How to fill the rectangle.
- `stroke: Stroke` — The thickness and color of the outline.
- `stroke_kind: StrokeKind` — Is the stroke on the inside, outside, or centered on the rectangle?
- `round_to_pixels: Option<bool>` — Snap the rectangle to pixels?
- `blur_width: f32` — If larger than zero, the edges of the rectangle (for both fill and stroke) will be blurre…
- `brush: Option<Arc<Brush>>` — Controls texturing, if any.
- `angle: f32` — Rotate rectangle by this many radians clockwise around its center.

Methods:

- `fn fill_texture_id(&self) -> TextureId` — `epaint-0.35.0/src/shapes/rect_shape.rs:211`
  The texture to use when painting this rectangle, if any.
- `fn filled(rect: Rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:99`
- `fn new(rect: Rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>, stroke: impl Into<Stroke>, stroke_kind: StrokeKind) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:78`
  See also [`Self::filled`] and [`Self::stroke`].
- `fn stroke(rect: Rect, corner_radius: impl Into<CornerRadius>, stroke: impl Into<Stroke>, stroke_kind: StrokeKind) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:114`
- `fn visual_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/rect_shape.rs:185`
  The visual bounding rectangle (includes stroke width)
- `fn with_angle(self, angle: f32) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:167`
  Set the rotation of the rectangle (in radians, clockwise). The rectangle rotates around its center.
- `fn with_angle_and_pivot(self, angle: f32, pivot: Pos2) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:174`
  Set the rotation of the rectangle (in radians, clockwise) around a custom pivot point.
- `fn with_blur_width(self, blur_width: f32) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:149`
  If larger than zero, the edges of the rectangle (for both fill and stroke) will be blurred.
- `fn with_round_to_pixels(self, round_to_pixels: bool) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:137`
  Snap the rectangle to pixels?
- `fn with_stroke_kind(self, stroke_kind: StrokeKind) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:126`
  Set if the stroke is on the inside, outside, or centered on the rectangle.
- `fn with_texture(self, fill_texture_id: TextureId, uv: Rect) -> Self` — `epaint-0.35.0/src/shapes/rect_shape.rs:156`
  Set the texture to use when painting this rectangle, if any.

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `From<RectShape>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Rgba` (struct) — `ecolor-0.35.0/src/rgba.rs:10`

0-1 linear space `RGBA` color with premultiplied alpha.

Methods:

- `const fn from_gray(l: f32) -> Self` — `ecolor-0.35.0/src/rgba.rs:86`
- `const fn from_rgb(r: f32, g: f32, b: f32) -> Self` — `ecolor-0.35.0/src/rgba.rs:80`
- `const fn from_rgba_premultiplied(r: f32, g: f32, b: f32, a: f32) -> Self` — `ecolor-0.35.0/src/rgba.rs:60`
- `fn a(&self) -> f32` — `ecolor-0.35.0/src/rgba.rs:160`
- `fn additive(self) -> Self` — `ecolor-0.35.0/src/rgba.rs:122`
  Return an additive version of this color (alpha = 0)
- `fn b(&self) -> f32` — `ecolor-0.35.0/src/rgba.rs:155`
- `fn blend(self, on_top: Self) -> Self` — `ecolor-0.35.0/src/rgba.rs:217`
  Blend two colors in linear space, so that `self` is behind the argument.
- `fn from_black_alpha(a: f32) -> Self` — `ecolor-0.35.0/src/rgba.rs:105`
  Transparent black
- `fn from_luminance_alpha(l: f32, a: f32) -> Self` — `ecolor-0.35.0/src/rgba.rs:91`
- `fn from_rgba_unmultiplied(r: f32, g: f32, b: f32, a: f32) -> Self` — `ecolor-0.35.0/src/rgba.rs:65`
- `fn from_srgba_premultiplied(r: u8, g: u8, b: u8, a: u8) -> Self` — `ecolor-0.35.0/src/rgba.rs:70`
- `fn from_srgba_unmultiplied(r: u8, g: u8, b: u8, a: u8) -> Self` — `ecolor-0.35.0/src/rgba.rs:75`
- `fn from_white_alpha(a: f32) -> Self` — `ecolor-0.35.0/src/rgba.rs:115`
  Transparent white
- `fn g(&self) -> f32` — `ecolor-0.35.0/src/rgba.rs:150`
- `fn intensity(&self) -> f32` — `ecolor-0.35.0/src/rgba.rs:166`
  How perceptually intense (bright) is the color?
- `fn is_additive(self) -> bool` — `ecolor-0.35.0/src/rgba.rs:129`
  Is the alpha=0 ?
- `fn multiply(self, alpha: f32) -> Self` — `ecolor-0.35.0/src/rgba.rs:135`
  Multiply with e.g. 0.5 to make us half transparent
- `fn r(&self) -> f32` — `ecolor-0.35.0/src/rgba.rs:145`
- `fn to_array(&self) -> [f32; 4]` — `ecolor-0.35.0/src/rgba.rs:188`
  Premultiplied RGBA
- `fn to_opaque(&self) -> Self` — `ecolor-0.35.0/src/rgba.rs:172`
  Returns an opaque version of self
- `fn to_rgba_unmultiplied(&self) -> [f32; 4]` — `ecolor-0.35.0/src/rgba.rs:200`
  unmultiply the alpha
- `fn to_srgba_unmultiplied(&self) -> [u8; 4]` — `ecolor-0.35.0/src/rgba.rs:212`
  unmultiply the alpha
- `fn to_tuple(&self) -> (f32, f32, f32, f32)` — `ecolor-0.35.0/src/rgba.rs:194`
  Premultiplied RGBA

Implements: `Add`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `From<Color32>`, `From<Hsva>`, `From<HsvaGamma>`, `From<Rgba>`, `Hash`, `Index<usize>`, `IndexMut<usize>`, `Mul`, `Mul<Rgba>`, `Mul<f32>`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Zeroable`

### `Shadow` (struct) — `epaint-0.35.0/src/shadow.rs:10`

The color and fuzziness of a fuzzy shape.

Public fields:

- `offset: [i8; 2]` — Move the shadow by this much.
- `blur: u8` — The width of the blur, i.e. the width of the fuzzy penumbra.
- `spread: u8` — Expand the shadow in all directions by this much.
- `color: Color32` — Color of the opaque center of the shadow.

Methods:

- `fn as_shape(&self, rect: Rect, corner_radius: impl Into<CornerRadius>) -> RectShape` — `epaint-0.35.0/src/shadow.rs:48`
  The argument is the rectangle of the shadow caster.
- `fn margin(&self) -> MarginF32` — `epaint-0.35.0/src/shadow.rs:68`
  How much larger than the parent rect are we in each direction?

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Stroke` (struct) — `epaint-0.35.0/src/stroke.rs:12`

Describes the width and color of a line.

Public fields:

- `width: f32`
- `color: Color32`

Methods:

- `fn is_empty(&self) -> bool` — `epaint-0.35.0/src/stroke.rs:34`
  True if width is zero or color is transparent
- `fn new(width: f32, color: impl Into<Color32>) -> Self` — `epaint-0.35.0/src/stroke.rs:25`
- `fn round_center_to_pixel(&self, pixels_per_point: f32, coord: &mut f32)` — `epaint-0.35.0/src/stroke.rs:40`
  For vertical or horizontal lines: round the stroke center to produce a sharp, pixel-aligned line.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `From<(f32, Color)>`, `From<Stroke>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TessellationOptions` (struct) — `epaint-0.35.0/src/tessellator.rs:656`

Tessellation quality options

Public fields:

- `feathering: bool` — Use "feathering" to smooth out the edges of shapes as a form of anti-aliasing.
- `feathering_size_in_pixels: f32` — The size of the feathering, in physical pixels.
- `coarse_tessellation_culling: bool` — If `true` (default) cull certain primitives before tessellating them. This likely makes
- `prerasterized_discs: bool` — If `true`, small filled circled will be optimized by using pre-rasterized circled from th…
- `round_text_to_pixels: bool` — If `true` (default) align text to the physical pixel grid. This makes the text sharper on…
- `round_line_segments_to_pixels: bool` — If `true` (default), align right-angled line segments to the physical pixel grid.
- `round_rects_to_pixels: bool` — If `true` (default), align rectangles to the physical pixel grid.
- `debug_paint_clip_rects: bool` — Output the clip rectangles to be painted.
- `debug_paint_text_rects: bool` — Output the text-containing rectangles.
- `debug_ignore_clip_rects: bool` — If true, no clipping will be done.
- `bezier_tolerance: f32` — The maximum distance between the original curve and the flattened curve.
- `epsilon: f32` — The default value will be 1.0e-5, it will be used during float compare.
- `parallel_tessellation: bool` — If `rayon` feature is activated, should we parallelize tessellation?
- `validate_meshes: bool` — If `true`, invalid meshes will be silently ignored. If `false`, invalid meshes will cause…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Tessellator` (struct) — `epaint-0.35.0/src/tessellator.rs:1301`

Converts [`Shape`]s into triangles ([`Mesh`]).

Methods:

- `fn new(pixels_per_point: f32, options: TessellationOptions, font_tex_size: [usize; 2], prepared_discs: Vec<PreparedDisc>) -> Self` — `epaint-0.35.0/src/tessellator.rs:1327`
  Create a new [`Tessellator`].
- `fn set_clip_rect(&mut self, clip_rect: Rect)` — `epaint-0.35.0/src/tessellator.rs:1352`
  Set the [`Rect`] to use for culling.
- `fn tessellate_circle(&mut self, shape: CircleShape, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:1484`
  Tessellate a single [`CircleShape`] into a [`Mesh`].
- `fn tessellate_clipped_shape(&mut self, clipped_shape: ClippedShape, out_primitives: &mut Vec<ClippedPrimitive>)` — `epaint-0.35.0/src/tessellator.rs:1357`
  Tessellate a clipped shape into a list of primitives.
- `fn tessellate_cubic_bezier(&mut self, cubic_shape: &CubicBezierShape, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:2145`
  Tessellate a single [`CubicBezierShape`] into a [`Mesh`].
- `fn tessellate_ellipse(&mut self, shape: EllipseShape, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:1540`
  Tessellate a single [`EllipseShape`] into a [`Mesh`].
- `fn tessellate_line_segment(&mut self, points: [Pos2; 2], stroke: impl Into<Stroke>, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:1635`
  Tessellate a line segment between the two points with the given stroke into a [`Mesh`].
- `fn tessellate_mesh(&self, mesh: &Mesh, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:1616`
  Tessellate a single [`Mesh`] into a [`Mesh`].
- `fn tessellate_path(&mut self, path_shape: &PathShape, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:1710`
  Tessellate a single [`PathShape`] into a [`Mesh`].
- `fn tessellate_quadratic_bezier(&mut self, quadratic_shape: &QuadraticBezierShape, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:2116`
  Tessellate a single [`QuadraticBezierShape`] into a [`Mesh`].
- `fn tessellate_rect(&mut self, rect_shape: &RectShape, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:1755`
  Tessellate a single [`Rect`] into a [`Mesh`].
- `fn tessellate_shape(&mut self, shape: Shape, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:1420`
  Tessellate a single [`Shape`] into a [`Mesh`].
- `fn tessellate_shapes(&mut self, shapes: Vec<ClippedShape>) -> Vec<ClippedPrimitive>` — `epaint-0.35.0/src/tessellator.rs:2218`
  Turns [`Shape`]:s into sets of triangles.
- `fn tessellate_text(&mut self, text_shape: &TextShape, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:1991`
  Tessellate a single [`TextShape`] into a [`Mesh`]. * `text_shape`: the text to tessellate. * `out`: triangles…

Implements: `Clone`

### `TextOptions` (struct) — `epaint-0.35.0/src/text/mod.rs:27`

Controls how we render text

Public fields:

- `max_texture_side: usize` — Maximum size of the font texture.
- `color_transfer_function: FontColorTransferFunction` — Controls how to convert glyph colors when writing to the font atlas.
- `font_hinting: bool` — Whether to enable font hinting
- `subpixel_binning: bool` — Enable sub-pixel binning for glyphs.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextShape` (struct) — `epaint-0.35.0/src/shapes/text_shape.rs:12`

How to paint some text on screen.

Public fields:

- `pos: Pos2` — Where the origin of [`Self::galley`] is.
- `galley: Arc<Galley>` — The laid out text, from [`FontsView::layout_job`].
- `underline: Stroke` — Add this underline to the whole text. You can also set an underline when creating the gal…
- `fallback_color: Color32` — Any [`Color32::PLACEHOLDER`] in the galley will be replaced by the given color. Affects e…
- `override_text_color: Option<Color32>` — If set, the text color in the galley will be ignored and replaced with the given color.
- `opacity_factor: f32` — If set, the text will be rendered with the given opacity in gamma space Affects everythin…
- `angle: f32` — Rotate text by this many radians clockwise. The pivot is `pos` (the upper left corner of…

Methods:

- `fn new(pos: Pos2, galley: Arc<Galley>, fallback_color: Color32) -> Self` — `epaint-0.35.0/src/shapes/text_shape.rs:49`
  The given fallback color will be used for any uncolored part of the galley (using [`Color32::PLACEHOLDER`]).
- `fn transform(&mut self, transform: TSTransform)` — `epaint-0.35.0/src/shapes/text_shape.rs:110`
  Move the shape by this many points, in-place.
- `fn visual_bounding_rect(&self) -> Rect` — `epaint-0.35.0/src/shapes/text_shape.rs:63`
  The visual bounding rectangle
- `fn with_angle(self, angle: f32) -> Self` — `epaint-0.35.0/src/shapes/text_shape.rs:86`
  Set text rotation to `angle` radians clockwise. The pivot is `pos` (the upper left corner of the text).
- `fn with_angle_and_anchor(self, angle: f32, anchor: Align2) -> Self` — `epaint-0.35.0/src/shapes/text_shape.rs:94`
  Set the text rotation to the `angle` radians clockwise. The pivot is determined by the given `anchor` point o…
- `fn with_opacity_factor(self, opacity_factor: f32) -> Self` — `epaint-0.35.0/src/shapes/text_shape.rs:104`
  Render text with this opacity in gamma space
- `fn with_override_text_color(self, override_text_color: Color32) -> Self` — `epaint-0.35.0/src/shapes/text_shape.rs:78`
  Use the given color for the text, regardless of what color is already in the galley.
- `fn with_underline(self, underline: Stroke) -> Self` — `epaint-0.35.0/src/shapes/text_shape.rs:71`

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `From<TextShape>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextureAtlas` (struct) — `epaint-0.35.0/src/texture_atlas.rs:60`

Contains font data in an atlas, where each character occupied a small rectangle.

Methods:

- `fn allocate(&mut self, (w, h): (usize, usize)) -> ((usize, usize), &mut ColorImage)` — `epaint-0.35.0/src/texture_atlas.rs:220`
  Returns the coordinates of where the rect ended up, and invalidates the region.
- `fn fill_ratio(&self) -> f32` — `epaint-0.35.0/src/texture_atlas.rs:181`
  When this get high, it might be time to clear and start over!
- `fn image(&self) -> &ColorImage` — `epaint-0.35.0/src/texture_atlas.rs:197`
  The full font atlas image.
- `fn new(size: [usize; 2], options: TextOptions) -> Self` — `epaint-0.35.0/src/texture_atlas.rs:82`
- `fn options(&self) -> &TextOptions` — `epaint-0.35.0/src/texture_atlas.rs:142`
- `fn prepared_discs(&self) -> Vec<PreparedDisc>` — `epaint-0.35.0/src/texture_atlas.rs:151`
  Returns the locations and sizes of pre-rasterized discs (filled circles) in this atlas.
- `fn size(&self) -> [usize; 2]` — `epaint-0.35.0/src/texture_atlas.rs:146`
- `fn take_delta(&mut self) -> Option<ImageDelta>` — `epaint-0.35.0/src/texture_atlas.rs:202`
  Call to get the change to the image since last call.
- `fn texture_options() -> TextureOptions` — `epaint-0.35.0/src/texture_atlas.rs:191`
  The texture options suitable for a font texture

Implements: `Clone`

### `TextureHandle` (struct) — `epaint-0.35.0/src/texture_handle.rs:20`

Used to paint images.

Methods:

- `fn aspect_ratio(&self) -> f32` — `epaint-0.35.0/src/texture_handle.rs:112`
  width / height
- `fn byte_size(&self) -> usize` — `epaint-0.35.0/src/texture_handle.rs:104`
  `width x height x bytes_per_pixel`
- `fn id(&self) -> TextureId` — `epaint-0.35.0/src/texture_handle.rs:64`
- `fn name(&self) -> String` — `epaint-0.35.0/src/texture_handle.rs:118`
  Debug-name.
- `fn new(tex_mngr: Arc<RwLock<TextureManager>>, id: TextureId) -> Self` — `epaint-0.35.0/src/texture_handle.rs:59`
  If you are using egui, use `egui::Context::load_texture` instead.
- `fn set(&mut self, image: impl Into<ImageData>, options: TextureOptions)` — `epaint-0.35.0/src/texture_handle.rs:70`
  Assign a new image to an existing texture.
- `fn set_partial(&mut self, pos: [usize; 2], image: impl Into<ImageData>, options: TextureOptions)` — `epaint-0.35.0/src/texture_handle.rs:78`
  Assign a new image to a subregion of the whole texture.
- `fn size(&self) -> [usize; 2]` — `epaint-0.35.0/src/texture_handle.rs:90`
  width x height
- `fn size_vec2(&self) -> Vec2` — `epaint-0.35.0/src/texture_handle.rs:98`
  width x height

Implements: `Clone`, `Drop`, `Eq`, `From<&TextureHandle>`, `From<&mut TextureHandle>`, `Hash`, `PartialEq`

### `TextureManager` (struct) — `epaint-0.35.0/src/textures.rs:9`

Low-level manager for allocating textures.

Methods:

- `fn alloc(&mut self, name: String, image: ImageData, options: TextureOptions) -> TextureId` — `epaint-0.35.0/src/textures.rs:31`
  Allocate a new texture.
- `fn allocated(&self) -> impl ExactSizeIterator<Item = (&TextureId, &TextureMeta)>` — `epaint-0.35.0/src/textures.rs:111`
  Get meta-data about all allocated textures in some arbitrary order.
- `fn free(&mut self, id: TextureId)` — `epaint-0.35.0/src/textures.rs:71`
  Free an existing texture.
- `fn meta(&self, id: TextureId) -> Option<&TextureMeta>` — `epaint-0.35.0/src/textures.rs:106`
  Get meta-data about a specific texture.
- `fn num_allocated(&self) -> usize` — `epaint-0.35.0/src/textures.rs:116`
  Total number of allocated textures.
- `fn retain(&mut self, id: TextureId)` — `epaint-0.35.0/src/textures.rs:87`
  Increase the retain-count of the given texture.
- `fn set(&mut self, id: TextureId, delta: ImageDelta)` — `epaint-0.35.0/src/textures.rs:49`
  Assign a new image to an existing texture, or update a region of it.
- `fn take_delta(&mut self) -> TexturesDelta` — `epaint-0.35.0/src/textures.rs:101`
  Take and reset changes since last frame.

Implements: `Default`

### `Vec2` (struct) — `emath-0.35.0/src/vec2.rs:16`

A vector has a direction and length. A [`Vec2`] is often used to represent a size.

Public fields:

- `x: f32` — Rightwards. Width.
- `y: f32` — Downwards. Height.

Methods:

- `const fn new(x: f32, y: f32) -> Self` — `emath-0.35.0/src/vec2.rs:148`
- `const fn splat(v: f32) -> Self` — `emath-0.35.0/src/vec2.rs:154`
  Set both `x` and `y` to the same value.
- `fn abs(self) -> Self` — `emath-0.35.0/src/vec2.rs:257`
- `fn angle(self) -> f32` — `emath-0.35.0/src/vec2.rs:216`
  Measures the angle of the vector.
- `fn angled(angle: f32) -> Self` — `emath-0.35.0/src/vec2.rs:232`
  Create a unit vector with the given CW angle (in radians). * An angle of zero gives the unit X axis. * An ang…
- `fn any_nan(self) -> bool` — `emath-0.35.0/src/vec2.rs:269`
  True if any member is NaN.
- `fn ceil(self) -> Self` — `emath-0.35.0/src/vec2.rs:251`
- `fn clamp(self, min: Self, max: Self) -> Self` — `emath-0.35.0/src/vec2.rs:317`
- `fn dot(self, other: Self) -> f32` — `emath-0.35.0/src/vec2.rs:287`
  The dot-product of two vectors.
- `fn floor(self) -> Self` — `emath-0.35.0/src/vec2.rs:239`
- `fn is_finite(self) -> bool` — `emath-0.35.0/src/vec2.rs:263`
  True if all members are also finite.
- `fn is_normalized(self) -> bool` — `emath-0.35.0/src/vec2.rs:178`
  Checks if `self` has length `1.0` up to a precision of `1e-6`.
- `fn length(self) -> f32` — `emath-0.35.0/src/vec2.rs:190`
- `fn length_sq(self) -> f32` — `emath-0.35.0/src/vec2.rs:195`
- `fn max(self, other: Self) -> Self` — `emath-0.35.0/src/vec2.rs:281`
- `fn max_elem(self) -> f32` — `emath-0.35.0/src/vec2.rs:301`
  Returns the maximum of `self.x` and `self.y`.
- `fn min(self, other: Self) -> Self` — `emath-0.35.0/src/vec2.rs:275`
- `fn min_elem(self) -> f32` — `emath-0.35.0/src/vec2.rs:294`
  Returns the minimum of `self.x` and `self.y`.
- `fn normalized(self) -> Self` — `emath-0.35.0/src/vec2.rs:171`
  Safe normalize: returns zero if input is zero.
- `fn rot90(self) -> Self` — `emath-0.35.0/src/vec2.rs:185`
  Rotates the vector by 90°, i.e positive X to positive Y (clockwise in egui coordinates).
- `fn round(self) -> Self` — `emath-0.35.0/src/vec2.rs:245`
- `fn to_pos2(self) -> Pos2` — `emath-0.35.0/src/vec2.rs:161`
  Treat this vector as a position. `v.to_pos2()` is equivalent to `Pos2::default() + v`.
- `fn yx(self) -> Self` — `emath-0.35.0/src/vec2.rs:308`
  Swizzle the axes.

Implements: `Add`, `Add<Vec2>`, `AddAssign`, `AddAssign<Vec2>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Display`, `Div`, `Div<f32>`, `DivAssign<f32>`, `Eq`, `From<&(f32, f32)>`, `From<&Vec2>`, `From<&[f32; 2]>`, `From<(f32, f32)>`, `From<Vec2>`, `From<Vec2b>`, `From<[f32; 2]>`, `GuiRounding`, `Index<usize>`, `IndexMut<usize>`, `Mul`, `Mul<Vec2>`, `Mul<f32>`, `MulAssign<f32>`, `Neg`, `NumExt`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Sub`, `Sub<Vec2>`, `SubAssign`, `SubAssign<Vec2>`, `Zeroable`

### `Vertex` (struct) — `epaint-0.35.0/src/mesh.rs:12`

The 2D vertex type.

Public fields:

- `pos: Pos2` — Logical pixel coordinates (points). (0,0) is the top left corner of the screen.
- `uv: Pos2` — Normalized texture coordinates. (0, 0) is the top left corner of the texture. (1, 1) is t…
- `color: Color32` — sRGBA with premultiplied alpha

Methods:

- `fn untextured(pos: Pos2, color: Color32) -> Self` — `epaint-0.35.0/src/mesh.rs:29`
  An untextured vertex

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Zeroable`

### `ViewportInPixels` (struct) — `epaint-0.35.0/src/viewport.rs:4`

Size of the viewport in whole, physical pixels.

Public fields:

- `left_px: i32` — Physical pixel offset for left side of the viewport.
- `top_px: i32` — Physical pixel offset for top side of the viewport.
- `from_bottom_px: i32` — Physical pixel offset for bottom side of the viewport.
- `width_px: i32` — Viewport width in physical pixels.
- `height_px: i32` — Viewport height in physical pixels.

Methods:

- `fn from_points(rect: &Rect, pixels_per_point: f32, screen_size_px: [u32; 2]) -> Self` — `epaint-0.35.0/src/viewport.rs:25`
  Convert from ui points.


## `epaint::mutex`

### `Mutex` (struct) — `epaint-0.35.0/src/mutex.rs:15`

Provides interior mutability.

Methods:

- `fn lock(&self) -> MutexGuard<'_, T>` — `epaint-0.35.0/src/mutex.rs:32`
  Try to acquire the lock.
- `fn new(val: T) -> Self` — `epaint-0.35.0/src/mutex.rs:22`

Implements: `Clone`, `Default`

### `RwLock` (struct) — `epaint-0.35.0/src/mutex.rs:62`

Provides interior mutability.

Methods:

- `fn new(val: T) -> Self` — `epaint-0.35.0/src/mutex.rs:66`
- `fn read(&self) -> RwLockReadGuard<'_, T>` — `epaint-0.35.0/src/mutex.rs:78`
  Try to acquire read-access to the lock.
- `fn write(&self) -> RwLockWriteGuard<'_, T>` — `epaint-0.35.0/src/mutex.rs:98`
  Try to acquire write-access to the lock.

Implements: `Default`


## `epaint::shape_transform`

### `adjust_colors` — `epaint-0.35.0/src/shape_transform.rs:9`

```rust
fn adjust_colors(shape: &mut Shape, adjust_color: impl Fn(&mut Color32) + Send + Sync + Copy + 'static)
```

Remember to handle [`Color32::PLACEHOLDER`] specially!


## `epaint::stats`

### `AllocInfo` (struct) — `epaint-0.35.0/src/stats.rs:16`

Aggregate information about a bunch of allocations.

Methods:

- `fn format(&self, what: &str) -> String` — `epaint-0.35.0/src/stats.rs:128`
- `fn from_galley(galley: &Galley) -> Self` — `epaint-0.35.0/src/stats.rs:83`
- `fn from_mesh(mesh: &Mesh) -> Self` — `epaint-0.35.0/src/stats.rs:93`
- `fn from_slice<T>(slice: &[T]) -> Self` — `epaint-0.35.0/src/stats.rs:97`
- `fn megabytes(&self) -> String` — `epaint-0.35.0/src/stats.rs:124`
- `fn num_allocs(&self) -> usize` — `epaint-0.35.0/src/stats.rs:116`
- `fn num_bytes(&self) -> usize` — `epaint-0.35.0/src/stats.rs:120`
- `fn num_elements(&self) -> usize` — `epaint-0.35.0/src/stats.rs:108`

Implements: `Add`, `AddAssign`, `Clone`, `Copy`, `Default`, `From<&[T]>`, `PartialEq`, `StructuralPartialEq`, `Sum`


## `epaint::tessellator`

### `PathType` (enum) — `epaint-0.35.0/src/tessellator.rs:647`

Variants:

- `PathType::Open`
- `PathType::Closed`

Implements: `Clone`, `Copy`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `Path` (struct) — `epaint-0.35.0/src/tessellator.rs:324`

A connected line (without thickness or gaps) which can be tessellated to either to a stroke (with thickness) or a filled convex area. Used as a scratch-pad during tessel…

Methods:

- `fn add_circle(&mut self, center: Pos2, radius: f32)` — `epaint-0.35.0/src/tessellator.rs:342`
- `fn add_line_loop(&mut self, points: &[Pos2])` — `epaint-0.35.0/src/tessellator.rs:429`
- `fn add_line_segment(&mut self, points: [Pos2; 2])` — `epaint-0.35.0/src/tessellator.rs:376`
- `fn add_open_points(&mut self, points: &[Pos2])` — `epaint-0.35.0/src/tessellator.rs:383`
- `fn add_point(&mut self, pos: Pos2, normal: Vec2)` — `epaint-0.35.0/src/tessellator.rs:338`
- `fn clear(&mut self)` — `epaint-0.35.0/src/tessellator.rs:328`
- `fn fill(&mut self, feathering: f32, color: Color32, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:516`
  The path is taken to be closed (i.e. returning to the start again).
- `fn fill_and_stroke(&mut self, feathering: f32, fill: Color32, stroke: &PathStroke, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:482`
  The path is taken to be closed (i.e. returning to the start again).
- `fn fill_with_uv(&mut self, feathering: f32, color: Color32, texture_id: TextureId, uv_from_pos: impl Fn(Pos2) -> Pos2, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:523`
  Like [`Self::fill`] but with texturing.
- `fn reserve(&mut self, additional: usize)` — `epaint-0.35.0/src/tessellator.rs:333`
- `fn stroke(&mut self, feathering: f32, path_type: PathType, stroke: &PathStroke, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:502`
- `fn stroke_closed(&mut self, feathering: f32, stroke: &PathStroke, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:498`
  A closed path (returning to the first point).
- `fn stroke_open(&mut self, feathering: f32, stroke: &PathStroke, out: &mut Mesh)` — `epaint-0.35.0/src/tessellator.rs:493`
  Open-ended.

Implements: `Clone`, `Debug`, `Default`


## `epaint::text`

### `PASSWORD_REPLACEMENT_CHAR` (constant) — `epaint-0.35.0/src/text/mod.rs:22`

Suggested character to use to replace those in password text fields.

### `FontPriority` (enum) — `epaint-0.35.0/src/text/fonts.rs:474`

Variants:

- `FontPriority::Highest` — Prefer this font before all existing ones.
- `FontPriority::Lowest` — Use this font as a fallback, after all existing ones.

Implements: `Clone`, `Debug`

### `HintingTarget` (enum) — `epaint-0.35.0/src/text/fonts.rs:303`

How to *hint* glyph outlines, i.e. how aggressively to nudge them onto the pixel grid before rasterizing. Mirrors [`skrifa::outline::Target`].

Variants:

- `HintingTarget::Mono` — Strongest hinting, designed for aliased 1-bit (black & white) rendering.
- `HintingTarget::Smooth` — Hinting tuned for anti-aliased rendering. This is what you normally want, and what egui uses by def…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<HintingTarget>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextWrapMode` (enum) — `epaint-0.35.0/src/text/text_layout_types.rs:587`

How to wrap and elide text.

Variants:

- `TextWrapMode::Extend` — The text should expand the `Ui` size when reaching its boundary.
- `TextWrapMode::Wrap` — The text should wrap to the next line when reaching the `Ui` boundary.
- `TextWrapMode::Truncate` — The text should be elided using "…" when reaching the `Ui` boundary.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `layout` — `epaint-0.35.0/src/text/text_layout.rs:101`

```rust
fn layout(fonts: &mut FontsImpl, pixels_per_point: f32, job: Arc<LayoutJob>) -> Galley
```

Layout text into a [`Galley`].

### `ByteIndex` (struct) — `epaint-0.35.0/src/text/index.rs:19`

A byte offset into a UTF-8 string.

Methods:

- `fn saturating_add(self, rhs: usize) -> Self` — `epaint-0.35.0/src/text/index.rs:133`
  Saturating integer addition.
- `fn saturating_sub(self, rhs: usize) -> Self` — `epaint-0.35.0/src/text/index.rs:133`
  Saturating integer subtraction.

Implements: `Add`, `Add<usize>`, `AddAssign`, `AddAssign<usize>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Display`, `Eq`, `From<ByteIndex>`, `From<usize>`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`, `Sub`, `Sub<usize>`, `SubAssign<usize>`

### `CharIndex` (struct) — `epaint-0.35.0/src/text/index.rs:31`

A character (Unicode scalar) offset into a string.

Methods:

- `fn saturating_add(self, rhs: usize) -> Self` — `epaint-0.35.0/src/text/index.rs:134`
  Saturating integer addition.
- `fn saturating_sub(self, rhs: usize) -> Self` — `epaint-0.35.0/src/text/index.rs:134`
  Saturating integer subtraction.

Implements: `Add`, `Add<CharIndex>`, `Add<usize>`, `AddAssign`, `AddAssign<usize>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Display`, `Eq`, `From<CharIndex>`, `From<usize>`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`, `Sub`, `Sub<CharIndex>`, `Sub<usize>`, `SubAssign<usize>`

### `FontData` (struct) — `epaint-0.35.0/src/text/fonts.rs:118`

A `.ttf` or `.otf` file and a font face index.

Public fields:

- `font: Cow<'static, [u8]>` — The content of a `.ttf` or `.otf` file.
- `index: u32` — Which font face in the file to use. When in doubt, use `0`.
- `tweak: FontTweak` — Extra scale and vertical tweak to apply to all text of this font.

Methods:

- `fn from_owned(font: Vec<u8>) -> Self` — `epaint-0.35.0/src/text/fonts.rs:139`
- `fn from_static(font: &'static [u8]) -> Self` — `epaint-0.35.0/src/text/fonts.rs:131`
- `fn tweak(self, tweak: FontTweak) -> Self` — `epaint-0.35.0/src/text/fonts.rs:147`
- `fn variation_axes(&self) -> Vec<FontVariationAxis>` — `epaint-0.35.0/src/text/fonts.rs:159`
  The variation axes of this font, e.g. `wght` (weight) and `wdth` (width).

Implements: `AsRef<[u8]>`, `Clone`, `Debug`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `FontDefinitions` (struct) — `epaint-0.35.0/src/text/fonts.rs:437`

Describes the font data and the sizes to use.

Public fields:

- `font_data: BTreeMap<String, Arc<FontData>>` — List of font names and their definitions.
- `families: BTreeMap<FontFamily, Vec<String>>` — Which fonts (names) to use for each [`FontFamily`].

Methods:

- `fn builtin_font_names() -> &'static [&'static str]` — `epaint-0.35.0/src/text/fonts.rs:580`
  List of all the builtin font names used by `epaint`.
- `fn empty() -> Self` — `epaint-0.35.0/src/text/fonts.rs:567`
  No fonts.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `FontInsert` (struct) — `epaint-0.35.0/src/text/fonts.rs:453`

Public fields:

- `name: String` — Font name
- `data: FontData` — A `.ttf` or `.otf` file and a font face index.
- `families: Vec<InsertFontFamily>` — Sets the font family and priority

Methods:

- `fn new(name: &str, data: FontData, families: Vec<InsertFontFamily>) -> Self` — `epaint-0.35.0/src/text/fonts.rs:487`

Implements: `Clone`, `Debug`

### `FontTweak` (struct) — `epaint-0.35.0/src/text/fonts.rs:214`

Extra scale and vertical tweak to apply to all text of a certain font.

Public fields:

- `scale: f32` — Scale the font's glyphs by this much. this is only a visual effect and does not affect th…
- `y_offset_factor: f32` — Shift font's glyphs downwards by this fraction of the font size (in points). this is only…
- `y_offset: f32` — Shift font's glyphs downwards by this amount of logical points. this is only a visual eff…
- `hinting: Option<bool>` — Override the global font hinting setting for this specific font.
- `hinting_target: HintingTarget` — How to grid-fit the glyph outlines when hinting is enabled.
- `subpixel_binning: Option<bool>` — Override the global sub-pixel binning setting for this specific font.
- `coords: VariationCoords` — Override the font's default variation coordinates for its axes ("wght", etc.).
- `thin_space_width: f32` — Width of a thin space (`\u{2009}`) and narrow no-break space (`\u{202F}`), as a fraction…
- `tab_size: f32` — Width of a tab character (`\t`), measured in number of space widths.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `FontVariationAxis` (struct) — `epaint-0.35.0/src/text/fonts.rs:186`

A single variation axis of a variable font, e.g. weight (`wght`) or width (`wdth`).

Public fields:

- `tag: Tag` — The axis tag, e.g. `wght` or `wdth`.
- `name: Option<String>` — Human-readable axis name, if the font provides one (e.g. "Weight").
- `range: Rangef` — Valid range of values for this axis, `min..=max`.
- `default: f32` — The value used when the axis is not overridden.
- `hidden: bool` — Whether the font recommends hiding this axis from user interfaces.

Implements: `Clone`, `Debug`, `PartialEq`, `StructuralPartialEq`

### `FontsImpl` (struct) — `epaint-0.35.0/src/text/fonts.rs:966`

The collection of fonts used by `epaint`.

Methods:

- `fn font(&mut self, family: &FontFamily) -> Font<'_>` — `epaint-0.35.0/src/text/fonts.rs:1027`
  Get the right font implementation from [`FontFamily`].
- `fn new(options: TextOptions, definitions: FontDefinitions) -> Self` — `epaint-0.35.0/src/text/fonts.rs:980`
  Create a new [`FontsImpl`] for text layout. This call is expensive, so only create one [`FontsImpl`] and then…
- `fn options(&self) -> &TextOptions` — `epaint-0.35.0/src/text/fonts.rs:1012`
- `fn return_shape_buffer(&mut self, buffer: UnicodeBuffer)` — `epaint-0.35.0/src/text/fonts.rs:1022`
  Return a shaping buffer for reuse.
- `fn take_shape_buffer(&mut self) -> UnicodeBuffer` — `epaint-0.35.0/src/text/fonts.rs:1017`
  Take the recycled shaping buffer (or create a new one if already taken).

### `Glyph` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:879`

Public fields:

- `chr: char` — The character this glyph represents.
- `pos: Pos2` — Baseline position, relative to the row. Logical position: pos.y is the same for all chars…
- `advance_width: f32` — Logical width of the glyph.
- `line_height: f32` — Height of this row of text.
- `font_ascent: f32` — The ascent of this font.
- `font_height: f32` — The row/line height of this font.
- `font_face_ascent: f32` — The ascent of the sub-font within the font (`FontFace`).
- `font_face_height: f32` — The row/line height of the sub-font within the font (`FontFace`).
- `uv_rect: UvRect` — Position and size of the glyph in the font texture, in texels.
- `first_vertex: u32` — Which is our first vertex in [`RowVisuals::mesh`].

Methods:

- `fn logical_rect(&self) -> Rect` — `epaint-0.35.0/src/text/text_layout_types.rs:935`
  Same y range for all characters with the same [`TextFormat`].
- `fn max_x(&self) -> f32` — `epaint-0.35.0/src/text/text_layout_types.rs:929`
- `fn size(&self) -> Vec2` — `epaint-0.35.0/src/text/text_layout_types.rs:924`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `InsertFontFamily` (struct) — `epaint-0.35.0/src/text/fonts.rs:465`

Public fields:

- `family: FontFamily` — Font family
- `priority: FontPriority` — Fallback or Primary font

Implements: `Clone`, `Debug`

### `LayoutJob` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:49`

Describes the task of laying out text.

Public fields:

- `text: String` — The complete text of this job, referenced by [`LayoutSection`].
- `sections: Vec<LayoutSection>` — The different section, which can have different fonts, colors, etc.
- `wrap: TextWrapping` — Controls the text wrapping and elision.
- `first_row_min_height: f32` — The first row must be at least this high. This is in case we lay out text that is the con…
- `break_on_newline: bool` — If `true`, all `\n` characters will result in a new _paragraph_, starting on a new row.
- `halign: Align` — How to horizontally align the text (`Align::LEFT`, `Align::Center`, `Align::RIGHT`).
- `justify: bool` — Justify text so that word-wrapped rows fill the whole [`TextWrapping::max_width`].
- `round_output_to_gui: bool` — Round output sizes using [`emath::GuiRounding`], to avoid rounding errors in layout code.
- `keep_trailing_whitespace: bool` — If `false` (default), trailing whitespace is ignored when computing horizontal alignment…

Methods:

- `fn append(&mut self, text: &str, leading_space: f32, format: TextFormat)` — `epaint-0.35.0/src/text/text_layout_types.rs:193`
  Helper for adding a new section when building a [`LayoutJob`].
- `fn debug_sanity_check(&self)` — `epaint-0.35.0/src/text/text_layout_types.rs:236`
  Check the [`Self::sections`] invariant: the sections are ordered and together cover the whole of [`Self::text…
- `fn effective_wrap_width(&self) -> f32` — `epaint-0.35.0/src/text/text_layout_types.rs:288`
  The wrap with, with a small margin in some cases.
- `fn font_height(&self, fonts: &mut FontsView<'_>) -> f32` — `epaint-0.35.0/src/text/text_layout_types.rs:279`
  The height of the tallest font used in the job.
- `fn format_at_byte(&self, byte_idx: ByteIndex) -> &TextFormat` — `epaint-0.35.0/src/text/text_layout_types.rs:221`
  The [`TextFormat`] of the section containing the character starting at the given byte index.
- `fn is_empty(&self) -> bool` — `epaint-0.35.0/src/text/text_layout_types.rs:183`
- `fn simple(text: String, font_id: FontId, color: Color32, wrap_width: f32) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:119`
  Break on `\n` and at the given wrap width.
- `fn simple_format(text: String, format: TextFormat) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:138`
  Break on `\n`
- `fn simple_singleline(text: String, font_id: FontId, color: Color32) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:153`
  Does not break on `\n`, but shows the replacement character instead.
- `fn single_section(text: String, format: TextFormat) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:168`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `LayoutSection` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:340`

A contiguous range of [`LayoutJob::text`] that shares the same [`TextFormat`].

Public fields:

- `leading_space: f32` — Can be used for first row indentation.
- `byte_range: ByteRange` — Range into [`LayoutJob::text`].
- `format: TextFormat` — How to format the text in this section (font, color, etc).

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `PlacedRow` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:777`

Public fields:

- `pos: Pos2` — The position of this [`Row`] relative to the galley.
- `row: Arc<Row>` — The underlying unpositioned [`Row`].
- `ends_with_newline: bool` — If true, this [`PlacedRow`] came from a paragraph ending with a `\n`. The `\n` itself is…

Methods:

- `fn char_count_including_newline(&self) -> CharIndex` — `epaint-0.35.0/src/text/text_layout_types.rs:992`
  Includes the implicit `\n` after the [`PlacedRow`], if any.
- `fn max_y(&self) -> f32` — `epaint-0.35.0/src/text/text_layout_types.rs:986`
- `fn min_y(&self) -> f32` — `epaint-0.35.0/src/text/text_layout_types.rs:981`
- `fn rect(&self) -> Rect` — `epaint-0.35.0/src/text/text_layout_types.rs:798`
  Logical bounding rectangle on font heights etc.
- `fn rect_without_leading_space(&self) -> Rect` — `epaint-0.35.0/src/text/text_layout_types.rs:803`
  Same as [`Self::rect`] but excluding the `LayoutSection::leading_space`.

Implements: `Clone`, `Debug`, `Deref`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Row` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:823`

Public fields:

- `glyphs: Vec<Glyph>` — One for each `char`.
- `size: Vec2` — Logical size based on font heights etc. Includes leading and trailing whitespace.
- `visuals: RowVisuals` — The mesh, ready to be rendered.

Methods:

- `fn char_at(&self, desired_x: f32) -> CharIndex` — `epaint-0.35.0/src/text/text_layout_types.rs:956`
  Closest char at the desired x coordinate in row-relative coordinates. Returns something in the range `[0, cha…
- `fn char_count_excluding_newline(&self) -> CharIndex` — `epaint-0.35.0/src/text/text_layout_types.rs:950`
  Excludes the implicit `\n` after the [`Row`], if any.
- `fn height(&self) -> f32` — `epaint-0.35.0/src/text/text_layout_types.rs:974`
- `fn text(&self) -> String` — `epaint-0.35.0/src/text/text_layout_types.rs:944`
  The text on this row, excluding the implicit `\n` if any.
- `fn x_offset(&self, column: CharIndex) -> f32` — `epaint-0.35.0/src/text/text_layout_types.rs:965`

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `RowVisuals` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:845`

The tessellated output of a row.

Public fields:

- `mesh: Mesh` — The tessellated text, using non-normalized (texel) UV coordinates. That is, you need to d…
- `mesh_bounds: Rect` — Bounds of the mesh, and can be used for culling. Does NOT include leading or trailing whi…
- `glyph_index_start: usize` — The number of triangle indices added before the first glyph triangle.
- `glyph_vertex_range: Range<usize>` — The range of vertices in the mesh that contain glyphs (as opposed to background, underlin…

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `SmoothHinting` (struct) — `epaint-0.35.0/src/text/fonts.rs:347`

Tuning for [`HintingTarget::Smooth`], mirroring `skrifa`'s `Target::Smooth`.

Public fields:

- `light: bool` — Hint only lightly: snap stems vertically but leave horizontal shapes alone (`FreeType`'s…
- `symmetric_rendering: bool` — Render a glyph the same way regardless of its sub-pixel position.
- `preserve_linear_metrics: bool` — Keep advance widths independent of hinting (don't grid-fit horizontally in a way that cha…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextFormat` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:471`

Formatting option for a section of text.

Public fields:

- `font_id: FontId`
- `extra_letter_spacing: f32` — Extra spacing between letters, in points.
- `line_height: Option<f32>` — Explicit line height of the text in points.
- `color: Color32` — Text color
- `background: Color32`
- `expand_bg: f32` — Amount to expand background fill by.
- `coords: VariationCoords`
- `italics: bool`
- `underline: Stroke`
- `strikethrough: Stroke`
- `valign: Align` — If you use a small font and [`Align::TOP`] you can get the effect of raised text.

Methods:

- `fn simple(font_id: FontId, color: Color32) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:571`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextWrapping` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:603`

Controls the text wrapping and elision of a [`LayoutJob`].

Public fields:

- `max_width: f32` — Wrap text so that no row is wider than this.
- `max_rows: usize` — Maximum amount of rows the text galley should have.
- `break_anywhere: bool` — If `true`: Allow breaking between any characters. If `false` (default): prefer breaking b…
- `overflow_character: Option<char>` — Character to use to represent elided text.

Methods:

- `fn from_wrap_mode_and_width(mode: TextWrapMode, max_width: f32) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:676`
  Create a [`TextWrapping`] from a [`TextWrapMode`] and an available width.
- `fn no_max_width() -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:685`
  A row can be as long as it need to be.
- `fn truncate_at_width(max_width: f32) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:701`
  Elide text that doesn't fit within the given width, replaced with `…`.
- `fn wrap_at_width(max_width: f32) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:693`
  A row can be at most `max_width` wide but can wrap in any number of lines.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `VariationCoords` (struct) — `epaint-0.35.0/src/text/text_layout_types.rs:411`

List of font variation coordinates by axis tag. If more than one coordinate for a given axis is provided, the last one added is used.

Methods:

- `fn clear(&mut self)` — `epaint-0.35.0/src/text/text_layout_types.rs:440`
- `fn new<T>(values: impl IntoIterator<Item = (T, f32)>) -> Self` — `epaint-0.35.0/src/text/text_layout_types.rs:425`
  Create a list of variation coordinates from a sequence of (tag, value) pairs.
- `fn push(&mut self, tag: impl IntoTag, coord: f32)` — `epaint-0.35.0/src/text/text_layout_types.rs:431`
  Add a variation coordinate to the list.
- `fn remove(&mut self, index: usize)` — `epaint-0.35.0/src/text/text_layout_types.rs:436`
  Remove the coordinate at the given index.

Implements: `AsMut<[(Tag, f32)]>`, `AsRef<[(Tag, f32)]>`, `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ByteRangeExt` (trait) — `epaint-0.35.0/src/text/index.rs:143`

Extension methods for a [`ByteRange`].

Required/provided items:

- `fn full(text: &str) -> Self` — `epaint-0.35.0/src/text/index.rs:145`
  The full byte range covering `text`, i.e. `0..text.len()`.
- `fn as_usize(&self) -> Range<usize>` — `epaint-0.35.0/src/text/index.rs:148`
  The `start..end` byte range as plain `usize`, for slicing a [`str`].
- `fn slice(&self, text: &'s str) -> &'s str` — `epaint-0.35.0/src/text/index.rs:151`
  Slice the given string by this byte range.

### `CharRangeExt` (trait) — `epaint-0.35.0/src/text/index.rs:172`

Extension methods for a [`CharRange`].

Required/provided items:

- `fn full(text: &str) -> Self` — `epaint-0.35.0/src/text/index.rs:174`
  The full character range covering `text`, i.e. `0..text.chars().count()`.

### `IntoTag` (trait) — `epaint-0.35.0/src/text/text_layout_types.rs:368`

Helper trait for all types that can be parsed as a [`font_types::Tag`].

Required/provided items:

- `fn into_tag(self) -> Tag` — `epaint-0.35.0/src/text/text_layout_types.rs:369`

### `ByteRange` (type_alias) — `epaint-0.35.0/src/text/index.rs:137`

A range of [`ByteIndex`], i.e. a byte range into a [`str`].

### `CharRange` (type_alias) — `epaint-0.35.0/src/text/index.rs:140`

A range of [`CharIndex`], i.e. a character range into a [`str`].


## `epaint::textures`

### `TextureFilter` (enum) — `epaint-0.35.0/src/textures.rs:241`

How the texture texels are filtered.

Variants:

- `TextureFilter::Nearest` — Show the nearest pixel value.
- `TextureFilter::Linear` — Linearly interpolate the nearest neighbors, creating a smoother look when zooming in and out.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextureWrapMode` (enum) — `epaint-0.35.0/src/textures.rs:255`

Defines how textures are wrapped around objects when texture coordinates fall outside the [0, 1] range.

Variants:

- `TextureWrapMode::ClampToEdge` — Stretches the edge pixels to fill beyond the texture's bounds.
- `TextureWrapMode::Repeat` — Tiles the texture across the surface, repeating it horizontally and vertically.
- `TextureWrapMode::MirroredRepeat` — Mirrors the texture with each repetition, creating symmetrical tiling.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextureMeta` (struct) — `epaint-0.35.0/src/textures.rs:123`

Meta-data about an allocated texture.

Public fields:

- `name: String` — A human-readable name useful for debugging.
- `size: [usize; 2]` — width x height
- `bytes_per_pixel: usize` — 4 or 1
- `retain_count: usize` — Free when this reaches zero.
- `options: TextureOptions` — The texture filtering mode to use when rendering.

Methods:

- `fn bytes_used(&self) -> usize` — `epaint-0.35.0/src/textures.rs:143`
  Size in bytes. width x height x [`Self::bytes_per_pixel`].

Implements: `Clone`, `Debug`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `TextureOptions` (struct) — `epaint-0.35.0/src/textures.rs:153`

How the texture texels are filtered.

Public fields:

- `magnification: TextureFilter` — How to filter when magnifying (when texels are larger than pixels).
- `minification: TextureFilter` — How to filter when minifying (when texels are smaller than pixels).
- `wrap_mode: TextureWrapMode` — How to wrap the texture when the texture coordinates are outside the [0, 1] range.
- `mipmap_mode: Option<TextureFilter>` — How to filter between texture mipmaps.

Methods:

- `const fn with_mipmap_mode(self, mipmap_mode: Option<TextureFilter>) -> Self` — `epaint-0.35.0/src/textures.rs:223`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TexturesDelta` (struct) — `epaint-0.35.0/src/textures.rs:277`

What has been allocated and freed during the last period.

Public fields:

- `set: Vec<(TextureId, ImageDelta)>` — New or changed textures. Apply before painting.
- `free: Vec<TextureId>` — Textures to free after painting.

Methods:

- `fn append(&mut self, newer: Self)` — `epaint-0.35.0/src/textures.rs:290`
- `fn clear(&mut self)` — `epaint-0.35.0/src/textures.rs:295`
- `fn is_empty(&self) -> bool` — `epaint-0.35.0/src/textures.rs:286`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`


## `epaint::util`

### `hash` — `epaint-0.35.0/src/util/mod.rs:3`

```rust
fn hash(value: impl Hash) -> u64
```

Hash the given value with a predictable hasher.

### `hash_with` — `epaint-0.35.0/src/util/mod.rs:9`

```rust
fn hash_with(value: impl Hash, hasher: impl Hasher) -> u64
```

Hash the given value with the given hasher.


## `epaint::tessellator::path`

### `add_circle_quadrant` — `epaint-0.35.0/src/tessellator.rs:604`

```rust
fn add_circle_quadrant(path: &mut Vec<Pos2>, center: Pos2, radius: f32, quadrant: f32)
```

Add one quadrant of a circle

### `rounded_rectangle` — `epaint-0.35.0/src/tessellator.rs:541`

```rust
fn rounded_rectangle(path: &mut Vec<Pos2>, rect: Rect, cr: CornerRadiusF32)
```

overwrites existing points


## `epaint::text::cursor`

### `CCursor` (struct) — `epaint-0.35.0/src/text/cursor.rs:10`

Character cursor.

Public fields:

- `index: CharIndex` — Character offset (NOT byte offset!).
- `prefer_next_row: bool` — If this cursors sits right at the border of a wrapped row break (NOT paragraph break) do…

Methods:

- `fn new(index: impl Into<CharIndex>) -> Self` — `epaint-0.35.0/src/text/cursor.rs:23`

Implements: `Add<CharIndex>`, `Add<usize>`, `AddAssign<usize>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `Sub<CharIndex>`, `Sub<usize>`, `SubAssign<usize>`

### `LayoutCursor` (struct) — `epaint-0.35.0/src/text/cursor.rs:101`

Row/column cursor.

Public fields:

- `row: usize` — 0 is first row, and so on. Note that a single paragraph can span multiple rows. (a paragr…
- `column: CharIndex` — Character based (NOT bytes). It is fine if this points to something beyond the end of the…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`


