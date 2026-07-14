# API index: `emath` 0.35.0

GENERATED FILE — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`. Extracted from rustdoc JSON of the exact crate source in the local cargo registry, so every signature and line number below is real.

**If a name is not in this file, it does not exist in our version of the crate.** Grep here before writing egui code from memory.

Items are listed under the path callers actually write (the public re-export, e.g. `egui::Panel`, `egui::Color32`), not where they happen to be defined. Citations point into the crate that owns the item, so a type `egui` re-exports from `epaint` cites `epaint-0.35.0/src/…`.

## `emath`

### `GUI_ROUNDING` (constant) — `emath-0.35.0/src/gui_rounding.rs:18`

We (sometimes) round sizes and coordinates to an even multiple of this value.

### `Align` (enum) — `emath-0.35.0/src/align.rs:8`

left/center/right or top/center/bottom alignment for e.g. anchors and layouts.

Variants:

- `Align::Min` — Left or top.
- `Align::Center` — Horizontal or vertical center.
- `Align::Max` — Right or bottom.

Methods:

- `fn align_size_within_range(self, size: f32, range: impl Into<Rangef>) -> Rangef` — `emath-0.35.0/src/align.rs:123`
  Returns a range of given size within a specified range.
- `fn flip(self) -> Self` — `emath-0.35.0/src/align.rs:55`
  Returns the inverse alignment. `Min` becomes `Max`, `Center` stays the same, `Max` becomes `Min`.
- `fn to_factor(self) -> f32` — `emath-0.35.0/src/align.rs:35`
  Convert `Min => 0.0`, `Center => 0.5` or `Max => 1.0`.
- `fn to_sign(self) -> f32` — `emath-0.35.0/src/align.rs:45`
  Convert `Min => -1.0`, `Center => 0.0` or `Max => 1.0`.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `almost_equal` — `emath-0.35.0/src/lib.rs:255`

```rust
fn almost_equal(a: f32, b: f32, epsilon: f32) -> bool
```

Return true when arguments are the same within some rounding error.

### `ease_in_ease_out` — `emath-0.35.0/src/lib.rs:460`

```rust
fn ease_in_ease_out(t: f32) -> f32
```

Ease in, ease out.

### `exponential_smooth_factor` — `emath-0.35.0/src/lib.rs:411`

```rust
fn exponential_smooth_factor(reach_this_fraction: f32, in_this_many_seconds: f32, dt: f32) -> f32
```

Calculate a lerp-factor for exponential smoothing using a time step.

### `fast_midpoint` — `emath-0.35.0/src/lib.rs:122`

```rust
fn fast_midpoint<R>(a: R, b: R) -> R
```

This is a faster version of [`f32::midpoint`] which doesn't handle overflow.

### `format_with_decimals_in_range` — `emath-0.35.0/src/lib.rs:217`

```rust
fn format_with_decimals_in_range(value: f64, decimal_range: RangeInclusive<usize>) -> String
```

Use as few decimals as possible to show the value accurately, but within the given range.

### `format_with_minimum_decimals` — `emath-0.35.0/src/lib.rs:210`

```rust
fn format_with_minimum_decimals(value: f64, decimals: usize) -> String
```

### `interpolation_factor` — `emath-0.35.0/src/lib.rs:439`

```rust
fn interpolation_factor((start_time, end_time): (f64, f64), current_time: f64, dt: f32, easing: impl Fn(f32) -> f32) -> f32
```

If you have a value animating over time, how much towards its target do you need to move it this frame?

### `inverse_lerp` — `emath-0.35.0/src/lib.rs:145`

```rust
fn inverse_lerp<R>(range: RangeInclusive<R>, value: R) -> Option<R>
```

Where in the range is this value? Returns 0-1 if within the range.

### `lerp` — `emath-0.35.0/src/lib.rs:106`

```rust
fn lerp<R, T>(range: impl Into<RangeInclusive<R>>, t: T) -> R
```

Linear interpolation.

### `normalized_angle` — `emath-0.35.0/src/lib.rs:367`

```rust
fn normalized_angle(angle: f32) -> f32
```

Wrap angle to `[-PI, PI]` range.

### `pos2` — `emath-0.35.0/src/pos2.rs:29`

```rust
const fn pos2(x: f32, y: f32) -> Pos2
```

`pos2(x, y) == Pos2::new(x, y)`

### `remap` — `emath-0.35.0/src/lib.rs:161`

```rust
fn remap<T>(x: T, from: impl Into<RangeInclusive<T>>, to: impl Into<RangeInclusive<T>>) -> T
```

Linearly remap a value from one range to another, so that when `x == from.start()` returns `to.start()` and when `x == from.end()` returns `to.end()`.

### `remap_clamp` — `emath-0.35.0/src/lib.rs:176`

```rust
fn remap_clamp<T>(x: T, from: impl Into<RangeInclusive<T>>, to: impl Into<RangeInclusive<T>>) -> T
```

Like [`remap`], but also clamps the value so that the returned value is always in the `to` range.

### `round_to_decimals` — `emath-0.35.0/src/lib.rs:205`

```rust
fn round_to_decimals(value: f64, decimal_places: usize) -> f64
```

Round a value to the given number of decimal places.

### `vec2` — `emath-0.35.0/src/vec2.rs:26`

```rust
const fn vec2(x: f32, y: f32) -> Vec2
```

`vec2(x, y) == Vec2::new(x, y)`

### `Align2` (struct) — `emath-0.35.0/src/align.rs:151`

Two-dimension alignment, e.g. [`Align2::LEFT_TOP`].

Methods:

- `fn align_size_within_rect(self, size: Vec2, frame: Rect) -> Rect` — `emath-0.35.0/src/align.rs:235`
  e.g. center a size within a given frame
- `fn anchor_rect(self, rect: Rect) -> Rect` — `emath-0.35.0/src/align.rs:203`
  Used e.g. to anchor a piece of text to a part of the rectangle. Give a position within the rect, specified by…
- `fn anchor_size(self, pos: Pos2, size: Vec2) -> Rect` — `emath-0.35.0/src/align.rs:220`
  Use this anchor to position something around `pos`, e.g. [`Self::RIGHT_TOP`] means the right-top of the rect…
- `fn flip(self) -> Self` — `emath-0.35.0/src/align.rs:197`
  Flip on both axes e.g. `TOP_LEFT` -> `BOTTOM_RIGHT`
- `fn flip_x(self) -> Self` — `emath-0.35.0/src/align.rs:185`
  Flip on the x-axis e.g. `TOP_LEFT` -> `TOP_RIGHT`
- `fn flip_y(self) -> Self` — `emath-0.35.0/src/align.rs:191`
  Flip on the y-axis e.g. `TOP_LEFT` -> `BOTTOM_LEFT`
- `fn pos_in_rect(self, frame: &Rect) -> Pos2` — `emath-0.35.0/src/align.rs:261`
  Returns the point on the rect's frame or in the center of a rect according to the alignments of this object.
- `fn to_sign(self) -> Vec2` — `emath-0.35.0/src/align.rs:179`
  -1, 0, or +1 for each axis
- `fn x(self) -> Align` — `emath-0.35.0/src/align.rs:168`
  Returns an alignment by the X (horizontal) axis
- `fn y(self) -> Align` — `emath-0.35.0/src/align.rs:174`
  Returns an alignment by the Y (vertical) axis

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `Index<usize>`, `IndexMut<usize>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `History` (struct) — `emath-0.35.0/src/history.rs:20`

This struct tracks recent values of some time series.

Methods:

- `fn add(&mut self, now: f64, value: T)` — `emath-0.35.0/src/history.rs:127`
  Values must be added with a monotonically increasing time, or at least not decreasing.
- `fn average(&self) -> Option<T>` — `emath-0.35.0/src/history.rs:188`
- `fn bandwidth(&self) -> Option<T>` — `emath-0.35.0/src/history.rs:208`
  Average times rate. If you are keeping track of individual sizes of things (e.g. bytes), this will estimate t…
- `fn clear(&mut self)` — `emath-0.35.0/src/history.rs:122`
- `fn duration(&self) -> f32` — `emath-0.35.0/src/history.rs:102`
  Amount of time contained from start to end in this [`History`].
- `fn flush(&mut self, now: f64)` — `emath-0.35.0/src/history.rs:159`
  Remove samples that are too old.
- `fn is_empty(&self) -> bool` — `emath-0.35.0/src/history.rs:76`
- `fn iter(&self) -> impl ExactSizeIterator<Item = (f64, T)> + '_` — `emath-0.35.0/src/history.rs:113`
  `(time, value)` pairs Time difference between values can be zero, but never negative.
- `fn latest(&self) -> Option<T>` — `emath-0.35.0/src/history.rs:93`
- `fn latest_mut(&mut self) -> Option<&mut T>` — `emath-0.35.0/src/history.rs:97`
- `fn len(&self) -> usize` — `emath-0.35.0/src/history.rs:82`
  Current number of values kept in history
- `fn max_age(&self) -> f32` — `emath-0.35.0/src/history.rs:71`
- `fn max_len(&self) -> usize` — `emath-0.35.0/src/history.rs:66`
- `fn mean_time_interval(&self) -> Option<f32>` — `emath-0.35.0/src/history.rs:140`
  Mean time difference between values in this [`History`].
- `fn new(length_range: Range<usize>, max_age: f32) -> Self` — `emath-0.35.0/src/history.rs:55`
  Example: ``` # use emath::History; # fn now() -> f64 { 0.0 } // Drop events that are older than one second, /…
- `fn rate(&self) -> Option<f32>` — `emath-0.35.0/src/history.rs:154`
- `fn sum(&self) -> T` — `emath-0.35.0/src/history.rs:184`
- `fn total_count(&self) -> u64` — `emath-0.35.0/src/history.rs:89`
  Total number of values seen. Includes those that have been discarded due to `max_len` or `max_age`.
- `fn values(&self) -> impl ExactSizeIterator<Item = T> + '_` — `emath-0.35.0/src/history.rs:117`
- `fn velocity(&self) -> Option<Vel>` — `emath-0.35.0/src/history.rs:221`
  Calculate a smooth velocity (per second) over the entire time span. Calculated as the last value minus the fi…

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `Serialize`

### `OrderedFloat` (struct) — `emath-0.35.0/src/ordered_float.rs:15`

Wraps a floating-point value to add total order and hash. Possible types for `T` are `f32` and `f64`.

Methods:

- `fn into_inner(self) -> T` — `emath-0.35.0/src/ordered_float.rs:19`

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `From<T>`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`

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

### `Rangef` (struct) — `emath-0.35.0/src/range.rs:8`

Inclusive range of floats, i.e. `min..=max`, but more ergonomic than [`RangeInclusive`].

Public fields:

- `min: f32`
- `max: f32`

Methods:

- `fn as_positive(self) -> Self` — `emath-0.35.0/src/range.rs:73`
  Flip `min` and `max` if needed, so that `min <= max` after.
- `fn center(self) -> f32` — `emath-0.35.0/src/range.rs:54`
  The center of the range
- `fn clamp(self, x: f32) -> f32` — `emath-0.35.0/src/range.rs:67`
  Equivalent to `x.clamp(min, max)`
- `fn contains(self, x: f32) -> bool` — `emath-0.35.0/src/range.rs:60`
- `fn expand(self, amnt: f32) -> Self` — `emath-0.35.0/src/range.rs:93`
  Expand by this much on each side, keeping the center
- `fn flip(self) -> Self` — `emath-0.35.0/src/range.rs:103`
  Flip the min and the max
- `fn intersection(self, other: Self) -> Self` — `emath-0.35.0/src/range.rs:122`
  The overlap of two ranges, i.e. the range that is contained by both.
- `fn intersects(self, other: Self) -> bool` — `emath-0.35.0/src/range.rs:140`
  Do the two ranges intersect?
- `fn new(min: f32, max: f32) -> Self` — `emath-0.35.0/src/range.rs:34`
- `fn point(min_and_max: f32) -> Self` — `emath-0.35.0/src/range.rs:39`
- `fn shrink(self, amnt: f32) -> Self` — `emath-0.35.0/src/range.rs:83`
  Shrink by this much on each side, keeping the center
- `fn span(self) -> f32` — `emath-0.35.0/src/range.rs:48`
  The length of the range, i.e. `max - min`.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `From<&RangeFrom<f32>>`, `From<&RangeFull>`, `From<&RangeInclusive<f32>>`, `From<&Rangef>`, `From<RangeFrom<f32>>`, `From<RangeFull>`, `From<RangeInclusive<f32>>`, `From<RangeToInclusive<f32>>`, `From<Rangef>`, `PartialEq`, `PartialEq<RangeInclusive<f32>>`, `PartialEq<Rangef>`, `Pod`, `Serialize`, `StructuralPartialEq`, `Zeroable`

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

### `RectAlign` (struct) — `emath-0.35.0/src/rect_align.rs:30`

Position a child [`Rect`] relative to a parent [`Rect`].

Public fields:

- `parent: Align2` — The alignment in the parent (original) rect.
- `child: Align2` — The alignment in the child (new) rect.

Methods:

- `fn align_rect(&self, parent_rect: &Rect, size: Vec2, gap: f32) -> Rect` — `emath-0.35.0/src/rect_align.rs:169`
  Calculate the child rect based on a size and some optional gap.
- `fn anchor(&self, parent_rect: &Rect, gap: f32) -> Pos2` — `emath-0.35.0/src/rect_align.rs:200`
  Calculator the anchor point for the child rect, based on the parent rect and an optional gap.
- `fn child(&self) -> Align2` — `emath-0.35.0/src/rect_align.rs:140`
  Align in the child rect.
- `fn find_best_align(values_to_try: impl Iterator<Item = Self>, content_rect: Rect, parent_rect: Rect, gap: f32, expected_size: Vec2) -> Option<Self>` — `emath-0.35.0/src/rect_align.rs:247`
  Look for the first alternative [`RectAlign`] that allows the child rect to fit inside the `content_rect`.
- `fn flip(self) -> Self` — `emath-0.35.0/src/rect_align.rs:225`
  Flip the alignment on both axes.
- `fn flip_x(self) -> Self` — `emath-0.35.0/src/rect_align.rs:209`
  Flip the alignment on the x-axis.
- `fn flip_y(self) -> Self` — `emath-0.35.0/src/rect_align.rs:217`
  Flip the alignment on the y-axis.
- `fn from_align2(align: Align2) -> Self` — `emath-0.35.0/src/rect_align.rs:145`
  Convert an [`Align2`] to an [`RectAlign`], positioning the child rect inside the parent.
- `fn gap_vector(&self) -> Vec2` — `emath-0.35.0/src/rect_align.rs:182`
  Returns a sign vector (-1, 0 or 1 in each direction) that can be used as an offset to the child rect, creatin…
- `fn outside(align: Align2) -> Self` — `emath-0.35.0/src/rect_align.rs:161`
  Position the child rect outside the parent rect.
- `fn over_corner(align: Align2) -> Self` — `emath-0.35.0/src/rect_align.rs:153`
  The center of the child rect will be aligned to a corner of the parent rect.
- `fn parent(&self) -> Align2` — `emath-0.35.0/src/rect_align.rs:135`
  Align in the parent rect.
- `fn pivot_pos(&self, parent_rect: &Rect, gap: f32) -> (Align2, Pos2)` — `emath-0.35.0/src/rect_align.rs:176`
  Returns a [`Align2`] and a [`Pos2`] that you can e.g. use with `Area::fixed_pos` and `Area::pivot` to align a…
- `fn symmetries(self) -> [Self; 3]` — `emath-0.35.0/src/rect_align.rs:234`
  Returns the 3 alternative [`RectAlign`]s that are flipped in various ways, for use with [`RectAlign::find_bes…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `RectTransform` (struct) — `emath-0.35.0/src/rect_transform.rs:10`

Linearly transforms positions from one [`Rect`] to another.

Methods:

- `fn from(&self) -> &Rect` — `emath-0.35.0/src/rect_transform.rs:24`
- `fn from_to(from: Rect, to: Rect) -> Self` — `emath-0.35.0/src/rect_transform.rs:20`
- `fn identity(from_and_to: Rect) -> Self` — `emath-0.35.0/src/rect_transform.rs:16`
- `fn inverse(&self) -> Self` — `emath-0.35.0/src/rect_transform.rs:37`
- `fn scale(&self) -> Vec2` — `emath-0.35.0/src/rect_transform.rs:33`
  The scale factors.
- `fn to(&self) -> &Rect` — `emath-0.35.0/src/rect_transform.rs:28`
- `fn transform_pos(&self, pos: Pos2) -> Pos2` — `emath-0.35.0/src/rect_transform.rs:42`
  Transforms the given coordinate in the `from` space to the `to` space.
- `fn transform_pos_clamped(&self, pos: Pos2) -> Pos2` — `emath-0.35.0/src/rect_transform.rs:59`
  Transforms the given coordinate in the `from` space to the `to` space, clamping if necessary.
- `fn transform_rect(&self, rect: Rect) -> Rect` — `emath-0.35.0/src/rect_transform.rs:50`
  Transforms the given rectangle in the `in`-space to a rectangle in the `out`-space.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Mul<Pos2>`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Zeroable`

### `Rot2` (struct) — `emath-0.35.0/src/rot2.rs:20`

Represents a rotation in the 2D plane.

Methods:

- `fn angle(self) -> f32` — `emath-0.35.0/src/rot2.rs:50`
- `fn from_angle(angle: f32) -> Self` — `emath-0.35.0/src/rot2.rs:44`
  Angle is clockwise in radians. A 𝞃/4 = 90° rotation means rotating the X axis to the Y axis.
- `fn inverse(self) -> Self` — `emath-0.35.0/src/rot2.rs:72`
- `fn is_finite(self) -> bool` — `emath-0.35.0/src/rot2.rs:66`
- `fn length(self) -> f32` — `emath-0.35.0/src/rot2.rs:56`
  The factor by which vectors will be scaled.
- `fn length_squared(self) -> f32` — `emath-0.35.0/src/rot2.rs:61`
- `fn normalized(self) -> Self` — `emath-0.35.0/src/rot2.rs:81`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Div<f32>`, `Mul`, `Mul<Rot2>`, `Mul<Vec2>`, `Mul<f32>`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Zeroable`

### `TSTransform` (struct) — `emath-0.35.0/src/ts_transform.rs:11`

Linearly transforms positions via a translation, then a scaling.

Public fields:

- `scaling: f32` — Scaling applied first, scaled around (0, 0).
- `translation: Vec2` — Translation amount, applied after scaling.

Methods:

- `fn from_scaling(scaling: f32) -> Self` — `emath-0.35.0/src/ts_transform.rs:50`
- `fn from_translation(translation: Vec2) -> Self` — `emath-0.35.0/src/ts_transform.rs:45`
- `fn inverse(&self) -> Self` — `emath-0.35.0/src/ts_transform.rs:73`
  Inverts the transform.
- `fn is_valid(&self) -> bool` — `emath-0.35.0/src/ts_transform.rs:55`
  Is this a valid, invertible transform?
- `fn mul_pos(&self, pos: Pos2) -> Pos2` — `emath-0.35.0/src/ts_transform.rs:88`
  Transforms the given coordinate.
- `fn mul_rect(&self, rect: Rect) -> Rect` — `emath-0.35.0/src/ts_transform.rs:103`
  Transforms the given rectangle.
- `fn new(translation: Vec2, scaling: f32) -> Self` — `emath-0.35.0/src/ts_transform.rs:37`
  Creates a new translation that first scales points around `(0, 0)`, then translates them.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Mul`, `Mul<Pos2>`, `Mul<Rect>`, `PartialEq`, `Pod`, `Serialize`, `StructuralPartialEq`, `Zeroable`

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

### `Vec2b` (struct) — `emath-0.35.0/src/vec2b.rs:6`

Two bools, one for each axis (X and Y).

Public fields:

- `x: bool`
- `y: bool`

Methods:

- `fn all(&self) -> bool` — `emath-0.35.0/src/vec2b.rs:27`
  Are both `x` and `y` true?
- `fn and(&self, other: impl Into<Self>) -> Self` — `emath-0.35.0/src/vec2b.rs:32`
- `fn any(&self) -> bool` — `emath-0.35.0/src/vec2b.rs:21`
- `fn new(x: bool, y: bool) -> Self` — `emath-0.35.0/src/vec2b.rs:16`
- `fn or(&self, other: impl Into<Self>) -> Self` — `emath-0.35.0/src/vec2b.rs:41`
- `fn to_vec2(self) -> Vec2` — `emath-0.35.0/src/vec2b.rs:51`
  Convert to a float `Vec2` where the components are 1.0 for `true` and 0.0 for `false`.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<Vec2b>`, `From<[bool; 2]>`, `From<bool>`, `Index<usize>`, `IndexMut<usize>`, `Not`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Float` (trait) — `emath-0.35.0/src/ordered_float.rs:88`

Extension trait to provide `ord()` method.

Required/provided items:

- `fn ord(self) -> OrderedFloat<Self>` — `emath-0.35.0/src/ordered_float.rs:90`
  Type to provide total order, useful as key in sorted contexts.

### `GuiRounding` (trait) — `emath-0.35.0/src/gui_rounding.rs:23`

Trait for rounding coordinates and sizes to align with either .

Required/provided items:

- `fn round_ui(self) -> Self` — `emath-0.35.0/src/gui_rounding.rs:31`
  Rounds floating point numbers to an even multiple of the GUI rounding factor, [`crate::GUI_ROUNDING`].
- `fn floor_ui(self) -> Self` — `emath-0.35.0/src/gui_rounding.rs:34`
  Like [`Self::round_ui`], but always rounds towards negative infinity.
- `fn round_to_pixels(self, pixels_per_point: f32) -> Self` — `emath-0.35.0/src/gui_rounding.rs:43`
  Round a size or position to an even multiple of the physical pixel size.
- `fn round_to_pixel_center(self, pixels_per_point: f32) -> Self` — `emath-0.35.0/src/gui_rounding.rs:53`
  Will round the position to be in the center of a pixel.

### `NumExt` (trait) — `emath-0.35.0/src/lib.rs:321`

Extends `f32`, [`Vec2`] etc with `at_least` and `at_most` as aliases for `max` and `min`.

Required/provided items:

- `fn at_least(self, lower_limit: Self) -> Self` — `emath-0.35.0/src/lib.rs:324`
  More readable version of `self.max(lower_limit)`
- `fn at_most(self, upper_limit: Self) -> Self` — `emath-0.35.0/src/lib.rs:328`
  More readable version of `self.min(upper_limit)`

### `Numeric` (trait) — `emath-0.35.0/src/numeric.rs:2`

Implemented for all builtin numeric types

Required/provided items:

- `fn to_f64(self) -> f64` — `emath-0.35.0/src/numeric.rs:12`
- `fn from_f64(num: f64) -> Self` — `emath-0.35.0/src/numeric.rs:14`

### `One` (trait) — `emath-0.35.0/src/lib.rs:65`

Helper trait to implement [`lerp`] and [`remap`].

Required/provided items:


### `Real` (trait) — `emath-0.35.0/src/lib.rs:78`

Helper trait to implement [`lerp`] and [`remap`].

Required/provided items:



## `emath::align`

### `center_size_in_rect` — `emath-0.35.0/src/align.rs:298`

```rust
fn center_size_in_rect(size: Vec2, frame: Rect) -> Rect
```

Allocates a rectangle of the specified `size` inside the `frame` rectangle around of its center.


## `emath::easing`

### `back_in` — `emath-0.35.0/src/easing.rs:159`

```rust
fn back_in(t: f32) -> f32
```

<https://easings.net/#easeInBack>

### `back_in_out` — `emath-0.35.0/src/easing.rs:172`

```rust
fn back_in_out(t: f32) -> f32
```

<https://easings.net/#easeInOutBack>

### `back_out` — `emath-0.35.0/src/easing.rs:165`

```rust
fn back_out(t: f32) -> f32
```

<https://easings.net/#easeOutBack>

### `bounce_in` — `emath-0.35.0/src/easing.rs:186`

```rust
fn bounce_in(t: f32) -> f32
```

<https://easings.net/#easeInBounce>

### `bounce_in_out` — `emath-0.35.0/src/easing.rs:220`

```rust
fn bounce_in_out(t: f32) -> f32
```

<https://easings.net/#easeInOutBounce>

### `bounce_out` — `emath-0.35.0/src/easing.rs:194`

```rust
fn bounce_out(t: f32) -> f32
```

<https://easings.net/#easeOutBounce>

### `circular_in` — `emath-0.35.0/src/easing.rs:101`

```rust
fn circular_in(t: f32) -> f32
```

<https://easings.net/#easeInCirc>

### `circular_in_out` — `emath-0.35.0/src/easing.rs:115`

```rust
fn circular_in_out(t: f32) -> f32
```

<https://easings.net/#easeInOutCirc>

### `circular_out` — `emath-0.35.0/src/easing.rs:109`

```rust
fn circular_out(t: f32) -> f32
```

<https://easings.net/#easeOutCirc>

### `cubic_in` — `emath-0.35.0/src/easing.rs:51`

```rust
fn cubic_in(t: f32) -> f32
```

<https://easings.net/#easeInCubic>

### `cubic_in_out` — `emath-0.35.0/src/easing.rs:64`

```rust
fn cubic_in_out(t: f32) -> f32
```

<https://easings.net/#easeInOutCubic>

### `cubic_out` — `emath-0.35.0/src/easing.rs:57`

```rust
fn cubic_out(t: f32) -> f32
```

<https://easings.net/#easeOutCubic>

### `exponential_in` — `emath-0.35.0/src/easing.rs:127`

```rust
fn exponential_in(t: f32) -> f32
```

<https://easings.net/#easeInExpo>

### `exponential_in_out` — `emath-0.35.0/src/easing.rs:147`

```rust
fn exponential_in_out(t: f32) -> f32
```

<https://easings.net/#easeInOutExpo>

### `exponential_out` — `emath-0.35.0/src/easing.rs:139`

```rust
fn exponential_out(t: f32) -> f32
```

<https://easings.net/#easeOutExpo>

### `linear` — `emath-0.35.0/src/easing.rs:17`

```rust
fn linear(t: f32) -> f32
```

No easing, just `y = x`

### `quadratic_in` — `emath-0.35.0/src/easing.rs:25`

```rust
fn quadratic_in(t: f32) -> f32
```

<https://easings.net/#easeInQuad>

### `quadratic_in_out` — `emath-0.35.0/src/easing.rs:39`

```rust
fn quadratic_in_out(t: f32) -> f32
```

<https://easings.net/#easeInOutQuad>

### `quadratic_out` — `emath-0.35.0/src/easing.rs:33`

```rust
fn quadratic_out(t: f32) -> f32
```

<https://easings.net/#easeOutQuad>

### `sin_in` — `emath-0.35.0/src/easing.rs:77`

```rust
fn sin_in(t: f32) -> f32
```

<https://easings.net/#easeInSine>

### `sin_in_out` — `emath-0.35.0/src/easing.rs:93`

```rust
fn sin_in_out(t: f32) -> f32
```

<https://easings.net/#easeInOutSine>

### `sin_out` — `emath-0.35.0/src/easing.rs:85`

```rust
fn sin_out(t: f32) -> f32
```

<https://easings.net/#easeOuSine>


## `emath::smart_aim`

### `best_in_range_f64` — `emath-0.35.0/src/smart_aim.rs:12`

```rust
fn best_in_range_f64(min: f64, max: f64) -> f64
```

Find the "simplest" number in a closed range [min, max], i.e. the one with the fewest decimal digits.


