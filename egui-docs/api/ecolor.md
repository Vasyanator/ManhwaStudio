# API index: `ecolor` 0.35.0

GENERATED FILE — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`. Extracted from rustdoc JSON of the exact crate source in the local cargo registry, so every signature and line number below is real.

**If a name is not in this file, it does not exist in our version of the crate.** Grep here before writing egui code from memory.

Items are listed under the path callers actually write (the public re-export, e.g. `egui::Panel`, `egui::Color32`), not where they happen to be defined. Citations point into the crate that owns the item, so a type `egui` re-exports from `epaint` cites `epaint-0.35.0/src/…`.

## `ecolor`

### `HexColor` (enum) — `ecolor-0.35.0/src/hex_color_runtime.rs:16`

A wrapper around Color32 that converts to and from a hex-color string

Variants:

- `HexColor::Hex3` — 3 hexadecimal digits, one for each of the r, g, b channels
- `HexColor::Hex4` — 4 hexadecimal digits, one for each of the r, g, b, a channels
- `HexColor::Hex6` — 6 hexadecimal digits, two for each of the r, g, b channels
- `HexColor::Hex8` — 8 hexadecimal digits, one for each of the r, g, b, a channels

Methods:

- `fn color(&self) -> Color32` — `ecolor-0.35.0/src/hex_color_runtime.rs:75`
  Retrieves the inner [`Color32`]
- `fn from_str_without_hash(s: &str) -> Result<Self, ParseHexColorError>` — `ecolor-0.35.0/src/hex_color_runtime.rs:87`
  Parses a string as a hex color without the leading `#` character

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Display`, `Eq`, `FromStr`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ParseHexColorError` (enum) — `ecolor-0.35.0/src/hex_color_runtime.rs:31`

Variants:

- `ParseHexColorError::MissingHash`
- `ParseHexColorError::InvalidLength`
- `ParseHexColorError::InvalidInt`

Implements: `Clone`, `Debug`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `gamma_from_linear` — `ecolor-0.35.0/src/lib.rs:160`

```rust
fn gamma_from_linear(linear: f32) -> f32
```

linear [0, 1] -> gamma [0, 1] (not clamped). Works for numbers outside this range (e.g. negative numbers).

### `gamma_u8_from_linear_f32` — `ecolor-0.35.0/src/lib.rs:114`

```rust
fn gamma_u8_from_linear_f32(l: f32) -> u8
```

linear [0, 1] -> gamma [0, 255] (clamped). Values outside this range will be clamped to the range.

### `hsv_from_rgb` — `ecolor-0.35.0/src/hsva.rs:191`

```rust
fn hsv_from_rgb([r, g, b]: [f32; 3]) -> (f32, f32, f32)
```

All ranges in 0-1, rgb is linear.

### `linear_f32_from_gamma_u8` — `ecolor-0.35.0/src/lib.rs:97`

```rust
fn linear_f32_from_gamma_u8(s: u8) -> f32
```

gamma [0, 255] -> linear [0, 1].

### `linear_f32_from_linear_u8` — `ecolor-0.35.0/src/lib.rs:108`

```rust
const fn linear_f32_from_linear_u8(a: u8) -> f32
```

linear [0, 255] -> linear [0, 1]. Useful for alpha-channel.

### `linear_from_gamma` — `ecolor-0.35.0/src/lib.rs:148`

```rust
fn linear_from_gamma(gamma: f32) -> f32
```

gamma [0, 1] -> linear [0, 1] (not clamped). Works for numbers outside this range (e.g. negative numbers).

### `linear_u8_from_linear_f32` — `ecolor-0.35.0/src/lib.rs:129`

```rust
fn linear_u8_from_linear_f32(a: f32) -> u8
```

linear [0, 1] -> linear [0, 255] (clamped). Useful for alpha-channel.

### `rgb_from_hsv` — `ecolor-0.35.0/src/hsva.rs:215`

```rust
fn rgb_from_hsv((h, s, v): (f32, f32, f32)) -> [f32; 3]
```

All ranges in 0-1, rgb is linear.

### `tint_color_towards` — `ecolor-0.35.0/src/lib.rs:174`

```rust
fn tint_color_towards(color: Color32, target: Color32) -> Color32
```

Cheap and ugly. Made for graying out disabled `Ui`s.

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


