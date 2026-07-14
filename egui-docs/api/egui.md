# API index: `egui` 0.35.0

GENERATED FILE — do not edit by hand. Regenerate with `tools/egui_docs/build.sh`. Extracted from rustdoc JSON of the exact crate source in the local cargo registry, so every signature and line number below is real.

**If a name is not in this file, it does not exist in our version of the crate.** Grep here before writing egui code from memory.

Items are listed under the path callers actually write (the public re-export, e.g. `egui::Panel`, `egui::Color32`), not where they happen to be defined. Citations point into the crate that owns the item, so a type `egui` re-exports from `epaint` cites `epaint-0.35.0/src/…`.

## `egui`

### `NUM_POINTER_BUTTONS` (constant) — `egui-0.35.0/src/data/input/pointer_button.rs:23`

Number of pointer buttons supported by egui, i.e. the number of possible states of [`PointerButton`].

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

### `AtomKind` (enum) — `egui-0.35.0/src/atomics/atom_kind.rs:26`

The different kinds of [`crate::Atom`]s.

Variants:

- `AtomKind::Empty` — Empty, that can be used with [`crate::AtomExt::atom_grow`] to reserve space.
- `AtomKind::Text` — Text atom.
- `AtomKind::Image` — Image atom.
- `AtomKind::Closure` — A custom closure that produces a sized atom.
- `AtomKind::Layout` — A nested [`AtomLayout`], letting you embed an atom-based widget as a single atom inside another [`A…

Methods:

- `fn closure(func: impl FnOnce(&Ui, IntoSizedArgs) -> IntoSizedResult<'static> + 'a) -> Self` — `egui-0.35.0/src/atomics/atom_kind.rs:116`
  See [`Self::Closure`]
- `fn image(image: impl Into<Image<'a>>) -> Self` — `egui-0.35.0/src/atomics/atom_kind.rs:111`
  See [`Self::Image`]
- `fn into_sized(self, ui: &Ui, _: IntoSizedArgs) -> IntoSizedResult<'a>` — `egui-0.35.0/src/atomics/atom_kind.rs:124`
  Turn this [`AtomKind`] into a [`SizedAtomKind`].
- `fn text(text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/atomics/atom_kind.rs:106`
  See [`Self::Text`]

Implements: `Clone`, `Debug`, `Default`, `From<AtomLayout<'a>>`, `From<Image<'a>>`, `From<ImageSource<'a>>`, `From<T>`

### `CursorGrab` (enum) — `egui-0.35.0/src/viewport.rs:1046`

Variants:

- `CursorGrab::None`
- `CursorGrab::Confined`
- `CursorGrab::Locked`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `CursorIcon` (enum) — `egui-0.35.0/src/data/output.rs:322`

A mouse cursor icon.

Variants:

- `CursorIcon::Default` — Normal cursor icon, whatever that is.
- `CursorIcon::None` — Show no cursor
- `CursorIcon::ContextMenu` — A context menu is available
- `CursorIcon::Help` — Question mark
- `CursorIcon::PointingHand` — Pointing hand, used for e.g. web links
- `CursorIcon::Progress` — Shows that processing is being done, but that the program is still interactive.
- `CursorIcon::Wait` — Not yet ready, try later.
- `CursorIcon::Cell` — Hover a cell in a table
- `CursorIcon::Crosshair` — For precision work
- `CursorIcon::Text` — Text caret, e.g. "Click here to edit text"
- `CursorIcon::VerticalText` — Vertical text caret, e.g. "Click here to edit vertical text"
- `CursorIcon::Alias` — Indicated an alias, e.g. a shortcut
- `CursorIcon::Copy` — Indicate that a copy will be made
- `CursorIcon::Move` — Omnidirectional move icon (e.g. arrows in all cardinal directions)
- `CursorIcon::NoDrop` — Can't drop here
- `CursorIcon::NotAllowed` — Forbidden
- `CursorIcon::Grab` — The thing you are hovering can be grabbed
- `CursorIcon::Grabbing` — You are grabbing the thing you are hovering
- `CursorIcon::AllScroll` — Something can be scrolled in any direction (panned).
- `CursorIcon::ResizeHorizontal` — Horizontal resize `-` to make something wider or more narrow (left to/from right)
- `CursorIcon::ResizeNeSw` — Diagonal resize `/` (right-up to/from left-down)
- `CursorIcon::ResizeNwSe` — Diagonal resize `\` (left-up to/from right-down)
- `CursorIcon::ResizeVertical` — Vertical resize `|` (up-down or down-up)
- `CursorIcon::ResizeEast` — Resize something rightwards (e.g. when dragging the right-most edge of something)
- `CursorIcon::ResizeSouthEast` — Resize something down and right (e.g. when dragging the bottom-right corner of something)
- `CursorIcon::ResizeSouth` — Resize something downwards (e.g. when dragging the bottom edge of something)
- `CursorIcon::ResizeSouthWest` — Resize something down and left (e.g. when dragging the bottom-left corner of something)
- `CursorIcon::ResizeWest` — Resize something leftwards (e.g. when dragging the left edge of something)
- `CursorIcon::ResizeNorthWest` — Resize something up and left (e.g. when dragging the top-left corner of something)
- `CursorIcon::ResizeNorth` — Resize something up (e.g. when dragging the top edge of something)
- `CursorIcon::ResizeNorthEast` — Resize something up and right (e.g. when dragging the top-right corner of something)
- `CursorIcon::ResizeColumn` — Resize a column
- `CursorIcon::ResizeRow` — Resize a row
- `CursorIcon::ZoomIn` — Enhance!
- `CursorIcon::ZoomOut` — Let's get a better overview

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

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

### `Event` (enum) — `egui-0.35.0/src/data/input/event.rs:17`

An input event generated by the integration.

Variants:

- `Event::Copy` — The integration detected a "copy" event (e.g. Cmd+C).
- `Event::Cut` — The integration detected a "cut" event (e.g. Cmd+X).
- `Event::Paste` — The integration detected a "paste" event (e.g. Cmd+V).
- `Event::Text` — Text input, e.g. via keyboard.
- `Event::Key` — A key was pressed or released.
- `Event::PointerMoved` — The mouse or touch moved to a new place.
- `Event::MouseMoved` — The mouse moved, the units are unspecified. Represents the actual movement of the mouse, without ac…
- `Event::PointerButton` — A mouse button was pressed or released (or a touch started or stopped).
- `Event::PointerGone` — The mouse left the screen, or the last/primary touch input disappeared.
- `Event::Zoom` — Zoom scale factor this frame (e.g. from a pinch gesture).
- `Event::Rotate` — Rotation in radians this frame, measuring clockwise (e.g. from a rotation gesture).
- `Event::Ime` — IME Event
- `Event::Touch` — On touch screens, report this *in addition to* [`Self::PointerMoved`], [`Self::PointerButton`], [`S…
- `Event::MouseWheel` — A raw mouse wheel event as sent by the backend.
- `Event::WindowFocused` — The native window gained or lost focused (e.g. the user clicked alt-tab).
- `Event::AccessKitActionRequest` — An assistive technology (e.g. screen reader) requested an action.
- `Event::Screenshot` — The reply of a screenshot requested with [`crate::ViewportCommand::Screenshot`].

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `FocusDirection` (enum) — `egui-0.35.0/src/memory/mod.rs:151`

A direction in which to move the keyboard focus.

Variants:

- `FocusDirection::Up` — Select the widget closest above the current focused widget.
- `FocusDirection::Right` — Select the widget to the right of the current focused widget.
- `FocusDirection::Down` — Select the widget below the current focused widget.
- `FocusDirection::Left` — Select the widget to the left of the current focused widget.
- `FocusDirection::Previous` — Select the previous widget that had focus.
- `FocusDirection::Next` — Select the next widget that wants focus.
- `FocusDirection::None` — Don't change focus.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `FontFamily` (enum) — `epaint-0.35.0/src/text/fonts.rs:80`

Font of unknown size.

Variants:

- `FontFamily::Proportional` — A font where some characters are wider than other (e.g. 'w' is wider than 'i').
- `FontFamily::Monospace` — A font where each character is the same width (`w` is the same width as `i`).
- `FontFamily::Name` — One of the names in [`FontDefinitions::families`].

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Display`, `Eq`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `FontSelection` (enum) — `egui-0.35.0/src/style.rs:126`

A way to select [`FontId`], either by picking one directly or by using a [`TextStyle`].

Variants:

- `FontSelection::Default` — Default text style - will use [`TextStyle::Body`], unless [`Style::override_font_id`] or [`Style::o…
- `FontSelection::FontId` — Directly select size and font family
- `FontSelection::Style` — Use a [`TextStyle`] to look up the [`FontId`] in [`Style::text_styles`].

Methods:

- `fn resolve(self, style: &Style) -> FontId` — `egui-0.35.0/src/style.rs:150`
  Resolve to a [`FontId`].
- `fn resolve_with_fallback(self, style: &Style, fallback: Self) -> FontId` — `egui-0.35.0/src/style.rs:157`
  Resolve with a final fallback.

Implements: `Clone`, `Debug`, `Default`, `From<FontId>`, `From<TextStyle>`

### `IMEPurpose` (enum) — `egui-0.35.0/src/viewport.rs:1028`

Variants:

- `IMEPurpose::Normal`
- `IMEPurpose::Password`
- `IMEPurpose::Terminal`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `IdSource` (enum) — `egui-0.35.0/src/ui_builder.rs:36`

Is this [`Ui`] a root or a child of another [`Ui`]?

Variants:

- `IdSource::Explicit` — Explicitly use this [`Id`]
- `IdSource::Child` — Salt the parent [`Id`] with this.

Implements: `Clone`

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

### `ImageFit` (enum) — `egui-0.35.0/src/widgets/image.rs:453`

This type determines how the image should try to fit within the UI.

Variants:

- `ImageFit::Original` — Fit the image to its original srce size, scaled by some factor.
- `ImageFit::Fraction` — Fit the image to a fraction of the available size.
- `ImageFit::Exact` — Fit the image to an exact size.

Methods:

- `fn resolve(self, available_size: Vec2, image_size: Vec2) -> Vec2` — `egui-0.35.0/src/widgets/image.rs:472`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Serialize`

### `ImageSource` (enum) — `egui-0.35.0/src/widgets/image.rs:570`

This type tells the [`Ui`] how to load an image.

Variants:

- `ImageSource::Uri` — Load the image from a URI, e.g. `https://example.com/image.png`.
- `ImageSource::Texture` — Load the image from an existing texture.
- `ImageSource::Bytes` — Load the image from some raw bytes.

Methods:

- `fn load(self, ctx: &Context, texture_options: TextureOptions, size_hint: SizeHint) -> TextureLoadResult` — `egui-0.35.0/src/widgets/image.rs:631`
  # Errors Failure to load the texture.
- `fn texture_size(&self) -> Option<Vec2>` — `egui-0.35.0/src/widgets/image.rs:622`
  Size of the texture, if known.
- `fn uri(&self) -> Option<&str>` — `egui-0.35.0/src/widgets/image.rs:650`
  Get the `uri` that this image was constructed from.

Implements: `Clone`, `Debug`, `From<&'a Cow<'a, str>>`, `From<&'a String>`, `From<&'a str>`, `From<(&'static str, T)>`, `From<(Cow<'static, str>, T)>`, `From<(String, T)>`, `From<Cow<'a, str>>`, `From<ImageSource<'a>>`, `From<String>`, `From<T>`

### `ImeEvent` (enum) — `egui-0.35.0/src/data/input/ime_event.rs:6`

IME event.

Variants:

- `ImeEvent::Enabled` — Notifies when the IME was enabled.
- `ImeEvent::Preedit` — A new IME candidate is being suggested.
- `ImeEvent::Commit` — IME composition ended with this final result.
- `ImeEvent::Disabled` — Notifies when the IME was disabled.

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Key` (enum) — `egui-0.35.0/src/data/key.rs:7`

Keyboard keys.

Variants:

- `Key::ArrowDown`
- `Key::ArrowLeft`
- `Key::ArrowRight`
- `Key::ArrowUp`
- `Key::Escape`
- `Key::Tab`
- `Key::Backspace`
- `Key::Enter`
- `Key::Space`
- `Key::Insert`
- `Key::Delete`
- `Key::Home`
- `Key::End`
- `Key::PageUp`
- `Key::PageDown`
- `Key::Copy`
- `Key::Cut`
- `Key::Paste`
- `Key::Colon` — `:`
- `Key::Comma` — `,`
- `Key::Backslash` — `\`
- `Key::Slash` — `/`
- `Key::Pipe` — `|`, a vertical bar
- `Key::Questionmark` — `?`
- `Key::Exclamationmark`
- `Key::OpenBracket`
- `Key::CloseBracket`
- `Key::OpenCurlyBracket`
- `Key::CloseCurlyBracket`
- `Key::Backtick` — Also known as "backquote" or "grave"
- `Key::Minus` — `-`
- `Key::Period` — `.`
- `Key::Plus` — `+`
- `Key::Equals` — `=`
- `Key::Semicolon` — `;`
- `Key::Quote` — `'`
- `Key::Num0` — `0` (from main row or numpad)
- `Key::Num1` — `1` (from main row or numpad)
- `Key::Num2` — `2` (from main row or numpad)
- `Key::Num3` — `3` (from main row or numpad)
- `Key::Num4` — `4` (from main row or numpad)
- `Key::Num5` — `5` (from main row or numpad)
- `Key::Num6` — `6` (from main row or numpad)
- `Key::Num7` — `7` (from main row or numpad)
- `Key::Num8` — `8` (from main row or numpad)
- `Key::Num9` — `9` (from main row or numpad)
- `Key::A`
- `Key::B`
- `Key::C`
- `Key::D`
- `Key::E`
- `Key::F`
- `Key::G`
- `Key::H`
- `Key::I`
- `Key::J`
- `Key::K`
- `Key::L`
- `Key::M`
- `Key::N`
- `Key::O`
- `Key::P`
- `Key::Q`
- `Key::R`
- `Key::S`
- `Key::T`
- `Key::U`
- `Key::V`
- `Key::W`
- `Key::X`
- `Key::Y`
- `Key::Z`
- `Key::F1`
- `Key::F2`
- `Key::F3`
- `Key::F4`
- `Key::F5`
- `Key::F6`
- `Key::F7`
- `Key::F8`
- `Key::F9`
- `Key::F10`
- `Key::F11`
- `Key::F12`
- `Key::F13`
- `Key::F14`
- `Key::F15`
- `Key::F16`
- `Key::F17`
- `Key::F18`
- `Key::F19`
- `Key::F20`
- `Key::F21`
- `Key::F22`
- `Key::F23`
- `Key::F24`
- `Key::F25`
- `Key::F26`
- `Key::F27`
- `Key::F28`
- `Key::F29`
- `Key::F30`
- `Key::F31`
- `Key::F32`
- `Key::F33`
- `Key::F34`
- `Key::F35`
- `Key::BrowserBack` — Back navigation key from multimedia keyboard. Android sends this key on Back button press. Does not…
- `Key::ShiftLeft` — Left Shift key.
- `Key::ShiftRight` — Right Shift key.
- `Key::ControlLeft` — Left Control key.
- `Key::ControlRight` — Right Control key.
- `Key::AltLeft` — Left Alt / Option key.
- `Key::AltRight` — Right Alt / `AltGr` / Option key.
- `Key::SuperLeft` — Left Super / Meta / Command / Windows key.
- `Key::SuperRight` — Right Super / Meta / Command / Windows key.
- `Key::IntlBackslash` — ISO 102nd key: physically located between the left Shift and Z on ISO layouts. On French AZERTY it…

Methods:

- `fn from_name(key: &str) -> Option<Self>` — `egui-0.35.0/src/data/key.rs:377`
  Converts `"A"` to `Key::A`, `Space` to `Key::Space`, etc.
- `fn name(self) -> &'static str` — `egui-0.35.0/src/data/key.rs:545`
  Human-readable English name.
- `fn symbol_or_name(self) -> &'static str` — `egui-0.35.0/src/data/key.rs:512`
  Emoji or name representing the key

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `MouseWheelUnit` (enum) — `egui-0.35.0/src/data/input/mouse_wheel_unit.rs:4`

The unit associated with the numeric value of a mouse wheel event

Variants:

- `MouseWheelUnit::Point` — Number of ui points (logical pixels)
- `MouseWheelUnit::Line` — Number of lines
- `MouseWheelUnit::Page` — Number of pages

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Order` (enum) — `egui-0.35.0/src/layers.rs:10`

Different layer categories

Variants:

- `Order::Background` — Painted behind all floating windows
- `Order::Middle` — Normal moveable windows that you reorder by click
- `Order::Foreground` — Popups, menus etc that should always be painted on top of windows Foreground objects can also have…
- `Order::Tooltip` — Things floating on top of everything else, like tooltips. You cannot interact with these.
- `Order::Debug` — Debug layer, always painted last / on top

Methods:

- `fn allow_interaction(&self) -> bool` — `egui-0.35.0/src/layers.rs:41`
- `fn short_debug_format(&self) -> &'static str` — `egui-0.35.0/src/layers.rs:50`
  Short and readable summary

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `OutputCommand` (enum) — `egui-0.35.0/src/data/output.rs:96`

Commands that the egui integration should execute at the end of a frame.

Variants:

- `OutputCommand::CopyText` — Put this text to the system clipboard.
- `OutputCommand::CopyImage` — Put this image to the system clipboard.
- `OutputCommand::OpenUrl` — Open this url in a browser.

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `PointerButton` (enum) — `egui-0.35.0/src/data/input/pointer_button.rs:4`

Mouse button (or similar for touch input)

Variants:

- `PointerButton::Primary` — The primary mouse button is usually the left one.
- `PointerButton::Secondary` — The secondary mouse button is usually the right one, and most often used for context menus or other…
- `PointerButton::Middle` — The tertiary mouse button is usually the middle mouse button (e.g. clicking the scroll wheel).
- `PointerButton::Extra1` — The first extra mouse button on some mice. In web typically corresponds to the Browser back button.
- `PointerButton::Extra2` — The second extra mouse button on some mice. In web typically corresponds to the Browser forward but…

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `PopupAnchor` (enum) — `egui-0.35.0/src/containers/popup.rs:24`

What should we anchor the popup to?

Variants:

- `PopupAnchor::ParentRect` — Show the popup relative to some parent [`Rect`].
- `PopupAnchor::Pointer` — Show the popup relative to the mouse pointer.
- `PopupAnchor::PointerFixed` — Remember the mouse position and show the popup relative to that (like a context menu).
- `PopupAnchor::Position` — Show the popup relative to a specific position.

Methods:

- `fn rect(self, popup_id: Id, ctx: &Context) -> Option<Rect>` — `egui-0.35.0/src/containers/popup.rs:65`
  Get the rect the popup should be shown relative to. Returns `Rect::from_pos` for [`PopupAnchor::Pointer`], [`…

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `From<&Response>`, `From<Pos2>`, `From<Rect>`, `PartialEq`, `StructuralPartialEq`

### `PopupCloseBehavior` (enum) — `egui-0.35.0/src/containers/popup.rs:77`

Determines popup's close behavior

Variants:

- `PopupCloseBehavior::CloseOnClick` — Popup will be closed on click anywhere, inside or outside the popup.
- `PopupCloseBehavior::CloseOnClickOutside` — Popup will be closed if the click happened somewhere else but in the popup's body
- `PopupCloseBehavior::IgnoreClicks` — Clicks will be ignored. Popup might be closed manually by calling [`Popup::close_all`] or by pressi…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `PopupKind` (enum) — `egui-0.35.0/src/containers/popup.rs:137`

Is the popup a popup, tooltip or menu?

Variants:

- `PopupKind::Popup`
- `PopupKind::Tooltip`
- `PopupKind::Menu`

Methods:

- `fn order(self) -> Order` — `egui-0.35.0/src/containers/popup.rs:145`
  Returns the order to be used with this kind.

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `From<PopupKind>`, `PartialEq`, `StructuralPartialEq`

### `ResizeDirection` (enum) — `egui-0.35.0/src/viewport.rs:1055`

Variants:

- `ResizeDirection::North`
- `ResizeDirection::South`
- `ResizeDirection::East`
- `ResizeDirection::West`
- `ResizeDirection::NorthEast`
- `ResizeDirection::SouthEast`
- `ResizeDirection::NorthWest`
- `ResizeDirection::SouthWest`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `SetOpenCommand` (enum) — `egui-0.35.0/src/containers/popup.rs:94`

Variants:

- `SetOpenCommand::Bool` — Set the open state to the given value
- `SetOpenCommand::Toggle` — Toggle the open state

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `From<bool>`, `PartialEq`, `StructuralPartialEq`

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

### `SizeHint` (enum) — `egui-0.35.0/src/load.rs:148`

Given as a hint for image loading requests.

Variants:

- `SizeHint::Scale` — Scale original size by some factor, keeping the original aspect ratio.
- `SizeHint::Width` — Scale to exactly this pixel width, keeping the original aspect ratio.
- `SizeHint::Height` — Scale to exactly this pixel height, keeping the original aspect ratio.
- `SizeHint::Size` — Scale to this pixel size.

Methods:

- `fn scale_by(self, factor: f32) -> Self` — `egui-0.35.0/src/load.rs:177`
  Multiply size hint by a factor.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `StructuralPartialEq`

### `SizedAtomKind` (enum) — `egui-0.35.0/src/atomics/sized_atom_kind.rs:8`

A sized [`crate::AtomKind`].

Variants:

- `SizedAtomKind::Empty`
- `SizedAtomKind::Text`
- `SizedAtomKind::Image`
- `SizedAtomKind::Layout`

Methods:

- `fn size(&self) -> Vec2` — `egui-0.35.0/src/atomics/sized_atom_kind.rs:23`
  Get the calculated size.

Implements: `Clone`, `Debug`, `Default`

### `SliderClamping` (enum) — `egui-0.35.0/src/widgets/slider.rs:59`

Specifies how values in a [`Slider`] are clamped.

Variants:

- `SliderClamping::Never` — Values are not clamped.
- `SliderClamping::Edits` — Users cannot enter new values that are outside the range.
- `SliderClamping::Always` — Always clamp values, even existing ones.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `SliderOrientation` (enum) — `egui-0.35.0/src/widgets/slider.rs:51`

Specifies the orientation of a [`Slider`].

Variants:

- `SliderOrientation::Horizontal`
- `SliderOrientation::Vertical`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `StrokeKind` (enum) — `epaint-0.35.0/src/stroke.rs:101`

Describes how the stroke of a shape should be painted.

Variants:

- `StrokeKind::Inside` — The stroke should be painted entirely inside of the shape
- `StrokeKind::Middle` — The stroke should be painted right on the edge of the shape, half inside and half outside.
- `StrokeKind::Outside` — The stroke should be painted entirely outside of the shape

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `SurrenderFocusOn` (enum) — `egui-0.35.0/src/input_state/mod.rs:27`

Variants:

- `SurrenderFocusOn::Presses` — Surrender focus if the user _presses_ somewhere outside the focused widget.
- `SurrenderFocusOn::Clicks` — Surrender focus if the user _clicks_ somewhere outside the focused widget.
- `SurrenderFocusOn::Never` — Never surrender focus.

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/input_state/mod.rs:40`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `SystemTheme` (enum) — `egui-0.35.0/src/viewport.rs:1037`

Variants:

- `SystemTheme::SystemDefault`
- `SystemTheme::Light`
- `SystemTheme::Dark`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextStyle` (enum) — `egui-0.35.0/src/style.rs:71`

Alias for a [`FontId`] (font of a certain size).

Variants:

- `TextStyle::Small` — Used when small text is needed.
- `TextStyle::Body` — Normal labels. Easily readable, doesn't take up too much space.
- `TextStyle::Monospace` — Same size as [`Self::Body`], but used when monospace is important (for code snippets, aligning numb…
- `TextStyle::Button` — Buttons. Maybe slightly bigger than [`Self::Body`].
- `TextStyle::Heading` — Heading. Probably larger than [`Self::Body`].
- `TextStyle::Name` — A user-chosen style, found in [`Style::text_styles`]. ``` egui::TextStyle::Name("footing".into());…

Methods:

- `fn resolve(&self, style: &Style) -> FontId` — `egui-0.35.0/src/style.rs:111`
  Look up this [`TextStyle`] in [`Style::text_styles`].

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `Display`, `Eq`, `From<TextStyle>`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `TextWrapMode` (enum) — `epaint-0.35.0/src/text/text_layout_types.rs:587`

How to wrap and elide text.

Variants:

- `TextWrapMode::Extend` — The text should expand the `Ui` size when reaching its boundary.
- `TextWrapMode::Wrap` — The text should wrap to the next line when reaching the `Ui` boundary.
- `TextWrapMode::Truncate` — The text should be elided using "…" when reaching the `Ui` boundary.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextureFilter` (enum) — `epaint-0.35.0/src/textures.rs:241`

How the texture texels are filtered.

Variants:

- `TextureFilter::Nearest` — Show the nearest pixel value.
- `TextureFilter::Linear` — Linearly interpolate the nearest neighbors, creating a smoother look when zooming in and out.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextureId` (enum) — `epaint-0.35.0/src/lib.rs:95`

What texture to use in a [`Mesh`] mesh.

Variants:

- `TextureId::Managed` — Textures allocated using [`TextureManager`].
- `TextureId::User` — Your own texture, defined in any which way you want. The backend renderer will presumably use this…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<&TextureHandle>`, `From<&mut TextureHandle>`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `TextureWrapMode` (enum) — `epaint-0.35.0/src/textures.rs:255`

Defines how textures are wrapped around objects when texture coordinates fall outside the [0, 1] range.

Variants:

- `TextureWrapMode::ClampToEdge` — Stretches the edge pixels to fill beyond the texture's bounds.
- `TextureWrapMode::Repeat` — Tiles the texture across the surface, repeating it horizontally and vertically.
- `TextureWrapMode::MirroredRepeat` — Mirrors the texture with each repetition, creating symmetrical tiling.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Theme` (enum) — `egui-0.35.0/src/memory/theme.rs:6`

Dark or Light theme.

Variants:

- `Theme::Dark` — Dark mode: light text on a dark background.
- `Theme::Light` — Light mode: dark text on a light background.

Methods:

- `fn default_style(self) -> Style` — `egui-0.35.0/src/memory/theme.rs:24`
  Default style for this theme.
- `fn default_visuals(self) -> Visuals` — `egui-0.35.0/src/memory/theme.rs:16`
  Default visuals for this theme.
- `fn from_dark_mode(dark_mode: bool) -> Self` — `egui-0.35.0/src/memory/theme.rs:32`
  Chooses between [`Self::Dark`] or [`Self::Light`] based on a boolean value.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `From<Theme>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ThemePreference` (enum) — `egui-0.35.0/src/memory/theme.rs:67`

The user's theme preference.

Variants:

- `ThemePreference::Dark` — Dark mode: light text on a dark background.
- `ThemePreference::Light` — Light mode: dark text on a light background.
- `ThemePreference::System` — Follow the system's theme preference.

Methods:

- `fn radio_buttons(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/memory/theme.rs:90`
  Show radio-buttons to switch between light mode, dark mode and following the system theme.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<Theme>`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TouchPhase` (enum) — `egui-0.35.0/src/data/input/touch.rs:16`

In what phase a touch event is in.

Variants:

- `TouchPhase::Start` — User just placed a touch point on the touch surface
- `TouchPhase::Move` — User moves a touch point along the surface. This event is also sent when any attributes (position,…
- `TouchPhase::End` — User lifted the finger or pen from the surface, or slid off the edge of the surface
- `TouchPhase::Cancel` — Touch operation has been disrupted by something (various reasons are possible, maybe a pop-up alert…

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `UiKind` (enum) — `egui-0.35.0/src/ui_stack.rs:11`

What kind is this [`crate::Ui`]?

Variants:

- `UiKind::Window` — A [`crate::Window`].
- `UiKind::CentralPanel` — A [`crate::CentralPanel`].
- `UiKind::LeftPanel` — A left [`crate::Panel`].
- `UiKind::RightPanel` — A right [`crate::Panel`].
- `UiKind::TopPanel` — A top [`crate::Panel`].
- `UiKind::BottomPanel` — A bottom [`crate::Panel`].
- `UiKind::Modal` — A modal [`crate::Modal`].
- `UiKind::Frame` — A [`crate::Frame`].
- `UiKind::ScrollArea` — A [`crate::ScrollArea`].
- `UiKind::Resize` — A [`crate::Resize`].
- `UiKind::Menu` — The content of a regular menu.
- `UiKind::Popup` — The content of a popup menu.
- `UiKind::Tooltip` — A tooltip, as shown by e.g. [`crate::Response::on_hover_ui`].
- `UiKind::Picker` — A picker, such as color picker.
- `UiKind::TableCell` — A table cell (from the `egui_extras` crate).
- `UiKind::GenericArea` — An [`crate::Area`] that is not of any other kind.
- `UiKind::Collapsible` — A collapsible container, e.g. a [`crate::CollapsingHeader`].

Methods:

- `fn is_area(&self) -> bool` — `egui-0.35.0/src/ui_stack.rs:80`
  Is this any kind of [`crate::Area`]?
- `fn is_panel(&self) -> bool` — `egui-0.35.0/src/ui_stack.rs:67`
  Is this any kind of panel?

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `From<PopupKind>`, `PartialEq`, `StructuralPartialEq`

### `UserAttentionType` (enum) — `egui-0.35.0/src/data/output.rs:271`

Types of attention to request from a user when a native window is not in focus.

Variants:

- `UserAttentionType::Critical` — Request an elevated amount of animations and flair for the window and the task bar or dock icon.
- `UserAttentionType::Informational` — Request a standard amount of attention-grabbing actions.
- `UserAttentionType::Reset` — Reset the attention request and interrupt related animations and flashes.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ViewportClass` (enum) — `egui-0.35.0/src/viewport.rs:83`

The different types of viewports supported by egui.

Variants:

- `ViewportClass::Root` — The root viewport; i.e. the original window.
- `ViewportClass::Deferred` — A viewport run independently from the parent viewport.
- `ViewportClass::Immediate` — A viewport run inside the parent viewport.
- `ViewportClass::EmbeddedWindow` — The fallback, when the egui integration doesn't support viewports, or [`crate::Context::embed_viewp…

Implements: `Clone`, `Copy`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ViewportCommand` (enum) — `egui-0.35.0/src/viewport.rs:1080`

An output [viewport](crate::viewport)-command from egui to the backend, e.g. to change the window title or size.

Variants:

- `ViewportCommand::Close` — Request this viewport to be closed.
- `ViewportCommand::CancelClose` — Cancel the closing that was signaled by [`crate::ViewportInfo::close_requested`].
- `ViewportCommand::Title` — Set the window title.
- `ViewportCommand::Transparent` — Turn the window transparent or not.
- `ViewportCommand::Visible` — Set the visibility of the window.
- `ViewportCommand::StartDrag` — Moves the window with the left mouse button until the button is released.
- `ViewportCommand::OuterPosition` — Set the outer position of the viewport, i.e. moves the window.
- `ViewportCommand::InnerSize` — Should be bigger than 0
- `ViewportCommand::MinInnerSize` — Should be bigger than 0
- `ViewportCommand::MaxInnerSize` — Should be bigger than 0
- `ViewportCommand::ResizeIncrements` — Should be bigger than 0
- `ViewportCommand::BeginResize` — Begin resizing the viewport with the left mouse button until the button is released.
- `ViewportCommand::Resizable` — Can the window be resized?
- `ViewportCommand::EnableButtons` — Set which window buttons are enabled
- `ViewportCommand::Minimized`
- `ViewportCommand::Maximized` — Maximize or unmaximize window.
- `ViewportCommand::Fullscreen` — Turn borderless fullscreen on/off.
- `ViewportCommand::SetMonitor` — Move the window to borderless fullscreen on the monitor at the given index.
- `ViewportCommand::Decorations` — Show window decorations, i.e. the chrome around the content with the title bar, close buttons, resi…
- `ViewportCommand::WindowLevel` — Set window to be always-on-top, always-on-bottom, or neither.
- `ViewportCommand::Icon` — The window icon.
- `ViewportCommand::IMERect` — Set the IME cursor editing area.
- `ViewportCommand::IMEAllowed`
- `ViewportCommand::IMEPurpose`
- `ViewportCommand::Focus` — Bring the window into focus (native only).
- `ViewportCommand::RequestUserAttention` — If the window is unfocused, attract the user's attention (native only).
- `ViewportCommand::SetTheme`
- `ViewportCommand::ContentProtected`
- `ViewportCommand::CursorPosition` — Will probably not work as expected!
- `ViewportCommand::CursorGrab`
- `ViewportCommand::CursorVisible`
- `ViewportCommand::MousePassthrough` — Enable mouse pass-through: mouse clicks pass through the window, used for non-interactable overlays.
- `ViewportCommand::Screenshot` — Take a screenshot of the next frame after this.
- `ViewportCommand::RequestCut` — Request cut of the current selection
- `ViewportCommand::RequestCopy` — Request a copy of the current selection.
- `ViewportCommand::RequestPaste` — Request a paste from the clipboard to the current focused `TextEdit` if any.

Methods:

- `fn center_on_screen(ctx: &Context) -> Option<Self>` — `egui-0.35.0/src/viewport.rs:1220`
  Construct a command to center the viewport on the monitor, if possible.
- `fn requires_parent_repaint(&self) -> bool` — `egui-0.35.0/src/viewport.rs:1236`
  This command requires the parent viewport to repaint.

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ViewportEvent` (enum) — `egui-0.35.0/src/data/input/viewport_info.rs:6`

An input event from the backend into egui, about a specific [viewport](crate::viewport).

Variants:

- `ViewportEvent::Close` — The user clicked the close-button on the window, or similar.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `WidgetText` (enum) — `egui-0.35.0/src/widget_text.rs:509`

This is how you specify text for a widget.

Variants:

- `WidgetText::Text` — Plain unstyled text.
- `WidgetText::RichText` — Text and optional style choices for it.
- `WidgetText::LayoutJob` — Use this [`LayoutJob`] when laying out the text.
- `WidgetText::Galley` — Use exactly this galley when painting the text.

Methods:

- `fn background_color(self, background_color: impl Into<Color32>) -> Self` — `egui-0.35.0/src/widget_text.rs:690`
  Prefer using [`RichText`] directly!
- `fn code(self) -> Self` — `egui-0.35.0/src/widget_text.rs:636`
  Prefer using [`RichText`] directly!
- `fn color(self, color: impl Into<Color32>) -> Self` — `egui-0.35.0/src/widget_text.rs:618`
  Override text color if, and only if, this is a [`RichText`].
- `fn fallback_text_style(self, text_style: TextStyle) -> Self` — `egui-0.35.0/src/widget_text.rs:610`
  Set the [`TextStyle`] unless it has already been set
- `fn heading(self) -> Self` — `egui-0.35.0/src/widget_text.rs:624`
  Prefer using [`RichText`] directly!
- `fn into_galley(self, ui: &Ui, wrap_mode: Option<TextWrapMode>, available_width: f32, fallback_font: impl Into<FontSelection>) -> Arc<Galley>` — `egui-0.35.0/src/widget_text.rs:723`
  Layout with wrap mode based on the containing [`Ui`].
- `fn into_galley_impl(self, ctx: &Context, style: &Style, text_wrapping: TextWrapping, fallback_font: FontSelection, default_valign: Align) -> Arc<Galley>` — `egui-0.35.0/src/widget_text.rs:739`
- `fn into_layout_job(self, style: &Style, fallback_font: FontSelection, default_valign: Align) -> Arc<LayoutJob>` — `egui-0.35.0/src/widget_text.rs:694`
- `fn is_empty(&self) -> bool` — `egui-0.35.0/src/widget_text.rs:562`
- `fn italics(self) -> Self` — `egui-0.35.0/src/widget_text.rs:666`
  Prefer using [`RichText`] directly!
- `fn monospace(self) -> Self` — `egui-0.35.0/src/widget_text.rs:630`
  Prefer using [`RichText`] directly!
- `fn raised(self) -> Self` — `egui-0.35.0/src/widget_text.rs:684`
  Prefer using [`RichText`] directly!
- `fn small(self) -> Self` — `egui-0.35.0/src/widget_text.rs:672`
  Prefer using [`RichText`] directly!
- `fn small_raised(self) -> Self` — `egui-0.35.0/src/widget_text.rs:678`
  Prefer using [`RichText`] directly!
- `fn strikethrough(self) -> Self` — `egui-0.35.0/src/widget_text.rs:660`
  Prefer using [`RichText`] directly!
- `fn strong(self) -> Self` — `egui-0.35.0/src/widget_text.rs:642`
  Prefer using [`RichText`] directly!
- `fn text(&self) -> &str` — `egui-0.35.0/src/widget_text.rs:572`
- `fn text_style(self, text_style: TextStyle) -> Self` — `egui-0.35.0/src/widget_text.rs:602`
  Override the [`TextStyle`] if, and only if, this is a [`RichText`].
- `fn underline(self) -> Self` — `egui-0.35.0/src/widget_text.rs:654`
  Prefer using [`RichText`] directly!
- `fn weak(self) -> Self` — `egui-0.35.0/src/widget_text.rs:648`
  Prefer using [`RichText`] directly!

Implements: `Clone`, `Debug`, `Default`, `From<&Box<str>>`, `From<&String>`, `From<&str>`, `From<Arc<Galley>>`, `From<Arc<LayoutJob>>`, `From<Arc<RichText>>`, `From<Box<str>>`, `From<Cow<'_, str>>`, `From<LayoutJob>`, `From<RichText>`, `From<String>`

### `WidgetType` (enum) — `egui-0.35.0/src/lib.rs:623`

The different types of built-in widgets in egui

Variants:

- `WidgetType::Label`
- `WidgetType::Link` — e.g. a hyperlink
- `WidgetType::TextEdit`
- `WidgetType::Button`
- `WidgetType::Checkbox`
- `WidgetType::RadioButton`
- `WidgetType::RadioGroup` — A group of radio buttons.
- `WidgetType::SelectableLabel`
- `WidgetType::ComboBox`
- `WidgetType::Slider`
- `WidgetType::DragValue`
- `WidgetType::ColorButton`
- `WidgetType::Image`
- `WidgetType::CollapsingHeader`
- `WidgetType::Panel`
- `WidgetType::ProgressIndicator`
- `WidgetType::Window`
- `WidgetType::ResizeHandle`
- `WidgetType::ScrollBar`
- `WidgetType::Other` — If you cannot fit any of the above slots.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `WindowDrag` (enum) — `egui-0.35.0/src/containers/window.rs:17`

Where the user can drag to move a [`Window`].

Variants:

- `WindowDrag::Off` — Window cannot be moved by dragging.
- `WindowDrag::Anywhere` — The user can drag the window from anywhere on its surface.
- `WindowDrag::TitleBar` — Only the title bar accepts the move-drag gesture.
- `WindowDrag::OnTouch` — [`Self::Anywhere`] when a touch screen is detected (see [`crate::InputState::has_touch_screen`]); […

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `WindowLevel` (enum) — `egui-0.35.0/src/viewport.rs:964`

For winit platform compatibility, see [`winit::WindowLevel` documentation](https://docs.rs/winit/latest/winit/window/enum.WindowLevel.html#platform-specific)

Variants:

- `WindowLevel::Normal`
- `WindowLevel::AlwaysOnBottom`
- `WindowLevel::AlwaysOnTop`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `X11WindowType` (enum) — `egui-0.35.0/src/viewport.rs:973`

Variants:

- `X11WindowType::Normal` — This is a normal, top-level window.
- `X11WindowType::Desktop` — A desktop feature. This can include a single window containing desktop icons with the same dimensio…
- `X11WindowType::Dock` — A dock or panel feature. Typically a Window Manager would keep such windows on top of all other win…
- `X11WindowType::Toolbar` — Toolbar windows. "Torn off" from the main application.
- `X11WindowType::Menu` — Pinnable menu windows. "Torn off" from the main application.
- `X11WindowType::Utility` — A small persistent utility window, such as a palette or toolbox.
- `X11WindowType::Splash` — The window is a splash screen displayed as an application is starting up.
- `X11WindowType::Dialog` — This is a dialog window.
- `X11WindowType::DropdownMenu` — A dropdown menu that usually appears when the user clicks on an item in a menu bar. This property i…
- `X11WindowType::PopupMenu` — A popup menu that usually appears when the user right clicks on an object. This property is typical…
- `X11WindowType::Tooltip` — A tooltip window. Usually used to show additional information when hovering over an object with the…
- `X11WindowType::Notification` — The window is a notification. This property is typically used on override-redirect windows.
- `X11WindowType::Combo` — This should be used on the windows that are popped up by combo boxes. This property is typically us…
- `X11WindowType::Dnd` — This indicates the window is being dragged. This property is typically used on override-redirect wi…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `__run_test_ctx` — `egui-0.35.0/src/lib.rs:673`

```rust
fn __run_test_ctx(run_ui: impl FnMut(&Context))
```

For use in tests; especially doctests.

### `__run_test_ui` — `egui-0.35.0/src/lib.rs:682`

```rust
fn __run_test_ui(add_contents: impl FnMut(&mut Ui))
```

For use in tests; especially doctests.

### `accesskit_root_id` — `egui-0.35.0/src/lib.rs:690`

```rust
fn accesskit_root_id() -> Id
```

### `decode_animated_image_uri` — `egui-0.35.0/src/widgets/image.rs:898`

```rust
fn decode_animated_image_uri(uri: &str) -> Result<(&str, usize), String>
```

Extracts uri and frame index # Errors Will return `Err` if `uri` does not match pattern {uri}-{frame_index}

### `global_theme_preference_buttons` — `egui-0.35.0/src/widgets/mod.rs:131`

```rust
fn global_theme_preference_buttons(ui: &mut Ui)
```

Show larger buttons for switching between light and dark mode (globally).

### `global_theme_preference_switch` — `egui-0.35.0/src/widgets/mod.rs:124`

```rust
fn global_theme_preference_switch(ui: &mut Ui)
```

Show a small button to switch to/from dark/light mode (globally).

### `has_gif_magic_header` — `egui-0.35.0/src/widgets/image.rs:940`

```rust
fn has_gif_magic_header(bytes: &[u8]) -> bool
```

Checks if bytes are gifs

### `has_webp_header` — `egui-0.35.0/src/widgets/image.rs:950`

```rust
fn has_webp_header(bytes: &[u8]) -> bool
```

Checks if bytes are webp

### `lerp` — `emath-0.35.0/src/lib.rs:106`

```rust
fn lerp<R, T>(range: impl Into<RangeInclusive<R>>, t: T) -> R
```

Linear interpolation.

### `paint_texture_at` — `egui-0.35.0/src/widgets/image.rs:839`

```rust
fn paint_texture_at(painter: &Painter, rect: Rect, options: &ImageOptions, texture: &SizedTexture)
```

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

### `reset_button` — `egui-0.35.0/src/widgets/mod.rs:104`

```rust
fn reset_button<T>(ui: &mut Ui, value: &mut T, text: &str)
```

Show a button to reset a value to its default. The button is only enabled if the value does not already have its original value.

### `reset_button_with` — `egui-0.35.0/src/widgets/mod.rs:112`

```rust
fn reset_button_with<T>(ui: &mut Ui, value: &mut T, text: &str, reset_value: T)
```

Show a button to reset a value to its default. The button is only enabled if the value does not already have its original value.

### `vec2` — `emath-0.35.0/src/vec2.rs:26`

```rust
const fn vec2(x: f32, y: f32) -> Vec2
```

`vec2(x, y) == Vec2::new(x, y)`

### `warn_if_debug_build` — `egui-0.35.0/src/lib.rs:502`

```rust
fn warn_if_debug_build(ui: &mut Ui)
```

Helper function that adds a label when compiling with debug assertions enabled.

### `generate_loader_id` (macro) — `egui-0.35.0/src/load.rs:303`

Used to get a unique ID when implementing one of the loader traits: [`BytesLoader::id`], [`ImageLoader::id`], and [`TextureLoader::id`].

### `github_link_file` (macro) — `egui-0.35.0/src/lib.rs:567`

Create a [`Hyperlink`] to the current [`file!()`] on github.

### `github_link_file_line` (macro) — `egui-0.35.0/src/lib.rs:552`

Create a [`Hyperlink`] to the current [`file!()`] (and line) on Github

### `include_image` (macro) — `egui-0.35.0/src/lib.rs:535`

Include an image in the binary.

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

### `AllocatedAtomLayout` (struct) — `egui-0.35.0/src/atomics/atom_layout.rs:499`

Instructions for painting an [`AtomLayout`].

Public fields:

- `sized: SizedAtomLayout<'a>` — The measured layout.
- `response: Response`

Methods:

- `fn paint(self, ui: &Ui) -> AtomLayoutResponse` — `egui-0.35.0/src/atomics/atom_layout.rs:691`
  Paint the [`Frame`] and individual [`crate::Atom`]s at the allocated [`Response`]'s rect.

Implements: `Clone`, `Debug`, `Deref`, `DerefMut`

### `Area` (struct) — `egui-0.35.0/src/containers/area.rs:107`

An area on the screen that can be moved by dragging.

Methods:

- `fn anchor(self, align: Align2, offset: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/area.rs:334`
  Set anchor and distance.
- `fn constrain(self, constrain: bool) -> Self` — `egui-0.35.0/src/containers/area.rs:287`
  Constrains this area to [`Context::content_rect`]?
- `fn constrain_to(self, constrain_rect: Rect) -> Self` — `egui-0.35.0/src/containers/area.rs:296`
  Constrain the movement of the window to the given rectangle.
- `fn current_pos(self, current_pos: impl Into<Pos2>) -> Self` — `egui-0.35.0/src/containers/area.rs:317`
  Positions the window but you can still move it.
- `fn default_height(self, default_height: f32) -> Self` — `egui-0.35.0/src/containers/area.rs:270`
  See [`Self::default_size`].
- `fn default_pos(self, default_pos: impl Into<Pos2>) -> Self` — `egui-0.35.0/src/containers/area.rs:241`
- `fn default_size(self, default_size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/area.rs:256`
  The size used for the [`Ui::max_rect`] the first frame.
- `fn default_width(self, default_width: f32) -> Self` — `egui-0.35.0/src/containers/area.rs:263`
  See [`Self::default_size`].
- `fn enabled(self, enabled: bool) -> Self` — `egui-0.35.0/src/containers/area.rs:191`
  If false, no content responds to click and widgets will be shown grayed out. You won't be able to move the wi…
- `fn fade_in(self, fade_in: bool) -> Self` — `egui-0.35.0/src/containers/area.rs:351`
  If `true`, quickly fade in the area.
- `fn fixed_pos(self, fixed_pos: impl Into<Pos2>) -> Self` — `egui-0.35.0/src/containers/area.rs:277`
  Positions the window and prevents it from being moved
- `fn id(self, id: Id) -> Self` — `egui-0.35.0/src/containers/area.rs:159`
  Let's you change the `id` that you assigned in [`Self::new`].
- `fn info(self, info: UiStackInfo) -> Self` — `egui-0.35.0/src/containers/area.rs:177`
  Set the [`UiStackInfo`] of the area's [`Ui`].
- `fn interactable(self, interactable: bool) -> Self` — `egui-0.35.0/src/containers/area.rs:218`
  If false, clicks goes straight through to what is behind us.
- `fn is_enabled(&self) -> bool` — `egui-0.35.0/src/containers/area.rs:204`
- `fn is_movable(&self) -> bool` — `egui-0.35.0/src/containers/area.rs:208`
- `fn kind(self, kind: UiKind) -> Self` — `egui-0.35.0/src/containers/area.rs:168`
  Change the [`UiKind`] of the arena.
- `fn layer(&self) -> LayerId` — `egui-0.35.0/src/containers/area.rs:182`
- `fn layout(self, layout: Layout) -> Self` — `egui-0.35.0/src/containers/area.rs:358`
  Set the layout for the child Ui.
- `fn movable(self, movable: bool) -> Self` — `egui-0.35.0/src/containers/area.rs:198`
  Moveable by dragging the area?
- `fn new(id: Id) -> Self` — `egui-0.35.0/src/containers/area.rs:133`
  The `id` must be globally unique.
- `fn order(self, order: Order) -> Self` — `egui-0.35.0/src/containers/area.rs:235`
  `order(Order::Foreground)` for an Area that should always be on top
- `fn pivot(self, pivot: Align2) -> Self` — `egui-0.35.0/src/containers/area.rs:310`
  Where the "root" of the area is.
- `fn sense(self, sense: Sense) -> Self` — `egui-0.35.0/src/containers/area.rs:228`
  Explicitly set a sense.
- `fn show<R>(self, ctx: &Context, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/area.rs:406`
- `fn sizing_pass(self, resize: bool) -> Self` — `egui-0.35.0/src/containers/area.rs:379`
  While true, a sizing pass will be done. This means the area will be invisible and the contents will be laid o…

Implements: `Clone`, `Debug`, `WidgetWithState`

### `AreaState` (struct) — `egui-0.35.0/src/containers/area.rs:18`

State of an [`Area`] that is persisted between frames.

Public fields:

- `pivot_pos: Option<Pos2>` — Last known position of the pivot.
- `pivot: Align2` — The anchor point of the area, i.e. where on the area the [`Self::pivot_pos`] refers to.
- `size: Option<Vec2>` — Last known size.
- `interactable: bool` — If false, clicks goes straight through to what is behind us. Useful for tooltips etc.
- `last_became_visible_at: Option<f64>` — At what time was this area first shown?

Methods:

- `fn left_top_pos(&self) -> Pos2` — `egui-0.35.0/src/containers/area.rs:64`
  The left top positions of the area.
- `fn load(ctx: &Context, id: Id) -> Option<Self>` — `egui-0.35.0/src/containers/area.rs:58`
  Load the state of an [`Area`] from memory.
- `fn rect(&self) -> Rect` — `egui-0.35.0/src/containers/area.rs:84`
  Where the area is on screen.
- `fn set_left_top_pos(&mut self, pos: Pos2)` — `egui-0.35.0/src/containers/area.rs:75`
  Move the left top positions of the area.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Serialize`

### `Atom` (struct) — `egui-0.35.0/src/atomics/atom.rs:32`

A low-level ui building block.

Public fields:

- `id: Option<Id>` — See [`crate::AtomExt::atom_id`]
- `size: Option<Vec2>` — See [`crate::AtomExt::atom_size`]
- `max_size: Vec2` — See [`crate::AtomExt::atom_max_size`]
- `grow: bool` — See [`crate::AtomExt::atom_grow`]
- `shrink: bool` — See [`crate::AtomExt::atom_shrink`]
- `align: Align2` — See [`crate::AtomExt::atom_align`]
- `kind: AtomKind<'a>` — The atom type / content

Methods:

- `fn custom(id: Id, size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/atomics/atom.rs:97`
  Create an [`AtomKind::Empty`] with a specific size.
- `fn grow() -> Self` — `egui-0.35.0/src/atomics/atom.rs:74`
  Create an empty [`Atom`] marked as `grow`.
- `fn into_sized(self, ui: &Ui, available_size: Vec2, wrap_mode: Option<TextWrapMode>, fallback_font: FontSelection) -> SizedAtom<'a>` — `egui-0.35.0/src/atomics/atom.rs:118`
  Turn this into a [`SizedAtom`].
- `fn layout(layout: AtomLayout<'a>) -> Self` — `egui-0.35.0/src/atomics/atom.rs:110`
  Nest an [`AtomLayout`] (e.g. an atom-based widget) as a single atom.

Implements: `Clone`, `Debug`, `Default`, `From<T>`

### `AtomLayout` (struct) — `egui-0.35.0/src/atomics/atom_layout.rs:60`

Intra-widget layout utility.

Public fields:

- `atoms: Atoms<'a>`

Methods:

- `fn align2(self, align2: Align2) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:215`
  Set the [`Align2`].
- `fn allocate(self, ui: &mut Ui) -> AllocatedAtomLayout<'a>` — `egui-0.35.0/src/atomics/atom_layout.rs:434`
  Calculate sizes, create [`Galley`]s and allocate a [`Response`].
- `fn direction(self, direction: Direction) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:229`
  Set the [`Direction`] the [`crate::Atom`]s are laid out along.
- `fn fallback_font(self, font: impl Into<FontSelection>) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:147`
  Set the fallback (default) font.
- `fn fallback_text_color(self, color: Color32) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:140`
  Set the fallback (default) text color.
- `fn frame(self, frame: Frame) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:112`
  Set the [`Frame`].
- `fn gap(self, gap: f32) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:105`
  Set the gap between atoms.
- `fn id(self, id: Id) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:191`
  Set the [`Id`] used to allocate a [`Response`].
- `fn max_height(self, height: f32) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:184`
  Set the maximum height of the Widget.
- `fn max_size(self, size: Vec2) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:166`
  Set the maximum size of the Widget.
- `fn max_width(self, width: f32) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:175`
  Set the maximum width of the Widget.
- `fn measure(self, ui: &Ui, available_size: Vec2) -> SizedAtomLayout<'a>` — `egui-0.35.0/src/atomics/atom_layout.rs:250`
  Measure the atoms (sizing only), without allocating space or interacting.
- `fn min_size(self, size: Vec2) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:157`
  Set the minimum size of the Widget.
- `fn new(atoms: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:83`
- `fn selectable(self, selectable: bool) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:131`
  Make the text in this layout selectable with the mouse.
- `fn sense(self, sense: Sense) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:119`
  Set the [`Sense`] used when allocating the [`Response`].
- `fn show(self, ui: &mut Ui) -> AtomLayoutResponse` — `egui-0.35.0/src/atomics/atom_layout.rs:235`
  [`AtomLayout::allocate`] and [`AllocatedAtomLayout::paint`] in one go.
- `fn wrap_mode(self, wrap_mode: TextWrapMode) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:202`
  Set the [`TextWrapMode`] for the [`crate::Atom`] marked as `shrink`.

Implements: `Clone`, `Default`, `Deref`, `DerefMut`, `From<AtomLayout<'a>>`, `Widget`

### `AtomLayoutResponse` (struct) — `egui-0.35.0/src/atomics/atom_layout.rs:701`

Response from a [`AtomLayout::show`] or [`AllocatedAtomLayout::paint`].

Public fields:

- `response: Response`

Methods:

- `fn custom_rects(&self) -> impl Iterator<Item = (Id, Rect)> + '_` — `egui-0.35.0/src/atomics/atom_layout.rs:715`
- `fn empty(response: Response) -> Self` — `egui-0.35.0/src/atomics/atom_layout.rs:708`
- `fn rect(&self, id: Id) -> Option<Rect>` — `egui-0.35.0/src/atomics/atom_layout.rs:722`
  Use this together with [`crate::Atom::custom`] to add custom painting / child widgets.

Implements: `Clone`, `Debug`, `Deref`, `DerefMut`

### `Atoms` (struct) — `egui-0.35.0/src/atomics/atoms.rs:16`

A list of [`Atom`]s.

Methods:

- `fn any_shrink(&self) -> bool` — `egui-0.35.0/src/atomics/atoms.rs:77`
  Do any of the atoms have shrink set to `true`?
- `fn extend_left(&mut self, atoms: Self)` — `egui-0.35.0/src/atomics/atoms.rs:43`
  Extend the list of atoms by prepending more atoms to the left side.
- `fn extend_right(&mut self, atoms: Self)` — `egui-0.35.0/src/atomics/atoms.rs:31`
  Extend the list of atoms by appending more atoms to the right side.
- `fn iter_images(&self) -> impl Iterator<Item = &Image<'a>>` — `egui-0.35.0/src/atomics/atoms.rs:89`
- `fn iter_images_mut(&mut self) -> impl Iterator<Item = &mut Image<'a>>` — `egui-0.35.0/src/atomics/atoms.rs:99`
- `fn iter_kinds(&self) -> impl Iterator<Item = &AtomKind<'a>>` — `egui-0.35.0/src/atomics/atoms.rs:81`
- `fn iter_kinds_mut(&mut self) -> impl Iterator<Item = &mut AtomKind<'a>>` — `egui-0.35.0/src/atomics/atoms.rs:85`
- `fn iter_texts(&self) -> impl Iterator<Item = &WidgetText> + ?` — `egui-0.35.0/src/atomics/atoms.rs:109`
- `fn iter_texts_mut(&mut self) -> impl Iterator<Item = &mut WidgetText> + ?` — `egui-0.35.0/src/atomics/atoms.rs:119`
- `fn map_atoms(&mut self, f: impl FnMut(Atom<'a>) -> Atom<'a>)` — `egui-0.35.0/src/atomics/atoms.rs:129`
- `fn map_images<F>(&mut self, f: F)` — `egui-0.35.0/src/atomics/atoms.rs:143`
- `fn map_kind<F>(&mut self, f: F)` — `egui-0.35.0/src/atomics/atoms.rs:134`
- `fn map_texts<F>(&mut self, f: F)` — `egui-0.35.0/src/atomics/atoms.rs:156`
- `fn new(atoms: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/atomics/atoms.rs:19`
- `fn push_left(&mut self, atom: impl Into<Atom<'a>>)` — `egui-0.35.0/src/atomics/atoms.rs:36`
  Insert a new [`Atom`] at the beginning of the list (left side).
- `fn push_right(&mut self, atom: impl Into<Atom<'a>>)` — `egui-0.35.0/src/atomics/atoms.rs:24`
  Insert a new [`Atom`] at the end of the list (right side).
- `fn text(&self) -> Option<Cow<'_, str>>` — `egui-0.35.0/src/atomics/atoms.rs:51`
  Concatenate and return the text contents.

Implements: `Clone`, `Debug`, `Default`, `Deref`, `DerefMut`, `From<&[T]>`, `From<Vec<T>>`, `FromIterator<Item>`, `IntoAtoms<'a>`, `IntoIterator`

### `Button` (struct) — `egui-0.35.0/src/widgets/button.rs:29`

Clickable button with text.

Methods:

- `fn atom_ui(self, ui: &mut Ui) -> AtomLayoutResponse` — `egui-0.35.0/src/widgets/button.rs:290`
  Show the button and return a [`AtomLayoutResponse`] for painting custom contents.
- `fn atoms(&self) -> &Atoms<'a>` — `egui-0.35.0/src/widgets/button.rs:285`
  Output the button's [`Atoms`].
- `fn corner_radius(self, corner_radius: impl Into<CornerRadius>) -> Self` — `egui-0.35.0/src/widgets/button.rs:200`
  Set the rounding of the button.
- `fn fill(self, fill: impl Into<Color32>) -> Self` — `egui-0.35.0/src/widgets/button.rs:143`
  Override background fill color. Note that this will override any on-hover effects. Calling this will also tur…
- `fn frame(self, frame: bool) -> Self` — `egui-0.35.0/src/widgets/button.rs:166`
  Turn off the frame
- `fn frame_when_inactive(self, frame_when_inactive: bool) -> Self` — `egui-0.35.0/src/widgets/button.rs:178`
  If `false`, the button will not have a frame when inactive.
- `fn gap(self, gap: f32) -> Self` — `egui-0.35.0/src/widgets/button.rs:277`
  Set the gap between atoms.
- `fn image(image: impl Into<Image<'a>>) -> Self` — `egui-0.35.0/src/widgets/button.rs:89`
  Creates a button with an image. The size of the image as displayed is defined by the provided size.
- `fn image_and_text(image: impl Into<Image<'a>>, text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/widgets/button.rs:97`
  Creates a button with an image to the left of the text.
- `fn image_tint_follows_text_color(self, image_tint_follows_text_color: bool) -> Self` — `egui-0.35.0/src/widgets/button.rs:212`
  If true, the tint of the image is multiplied by the widget text color.
- `fn left_text(self, left_text: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/button.rs:241`
  Show some text on the left side of the button.
- `fn min_size(self, min_size: Vec2) -> Self` — `egui-0.35.0/src/widgets/button.rs:193`
  Set the minimum size of the button.
- `fn new(atoms: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/button.rs:45`
- `fn opt_image_and_text(image: Option<Image<'a>>, text: Option<WidgetText>) -> Self` — `egui-0.35.0/src/widgets/button.rs:105`
  Create a button with an optional image and optional text.
- `fn right_text(self, right_text: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/button.rs:253`
  Show some text on the right side of the button.
- `fn selectable(selected: bool, atoms: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/button.rs:78`
  Show a selectable button.
- `fn selected(self, selected: bool) -> Self` — `egui-0.35.0/src/widgets/button.rs:270`
  If `true`, mark this button as "selected".
- `fn sense(self, sense: Sense) -> Self` — `egui-0.35.0/src/widgets/button.rs:186`
  By default, buttons senses clicks. Change this to a drag-button with `Sense::drag()`.
- `fn shortcut_text(self, shortcut_text: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/button.rs:225`
  Show some text on the right side of the button, in weak color.
- `fn small(self) -> Self` — `egui-0.35.0/src/widgets/button.rs:159`
  Make this a small button, suitable for embedding into text.
- `fn stroke(self, stroke: impl Into<Stroke>) -> Self` — `egui-0.35.0/src/widgets/button.rs:151`
  Override button stroke. Note that this will override any on-hover effects. Calling this will also turn on the…
- `fn truncate(self) -> Self` — `egui-0.35.0/src/widgets/button.rs:136`
  Set [`Self::wrap_mode`] to [`TextWrapMode::Truncate`].
- `fn wrap(self) -> Self` — `egui-0.35.0/src/widgets/button.rs:130`
  Set [`Self::wrap_mode`] to [`TextWrapMode::Wrap`].
- `fn wrap_mode(self, wrap_mode: TextWrapMode) -> Self` — `egui-0.35.0/src/widgets/button.rs:123`
  Set the wrap mode for the text.

Implements: `HasClasses`, `Widget`

### `CentralPanel` (struct) — `egui-0.35.0/src/containers/panel.rs:1039`

A panel that covers the remainder of the screen, i.e. whatever area is left after adding other panels.

Methods:

- `fn default_margins() -> Self` — `egui-0.35.0/src/containers/panel.rs:1052`
  A central panel with a background color and some inner margins
- `fn frame(self, frame: Frame) -> Self` — `egui-0.35.0/src/containers/panel.rs:1058`
  Change the background color, margins, etc.
- `fn no_frame() -> Self` — `egui-0.35.0/src/containers/panel.rs:1045`
  A central panel with no margin or background color
- `fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/panel.rs:1064`
  Show the panel inside a [`Ui`].
- `fn show_inside<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/panel.rs:1070`
  Renamed to [`Self::show`].
  **DEPRECATED**: Renamed to `show`

Implements: `Default`

### `Checkbox` (struct) — `egui-0.35.0/src/widgets/checkbox.rs:23`

Boolean on/off control with text label.

Methods:

- `fn atoms(&self) -> &Atoms<'a>` — `egui-0.35.0/src/widgets/checkbox.rs:47`
  Output the checkbox's [`Atoms`].
- `fn indeterminate(self, indeterminate: bool) -> Self` — `egui-0.35.0/src/widgets/checkbox.rs:56`
  Display an indeterminate state (neither checked nor unchecked)
- `fn new(checked: &'a mut bool, atoms: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/checkbox.rs:31`
- `fn without_text(checked: &'a mut bool) -> Self` — `egui-0.35.0/src/widgets/checkbox.rs:40`

Implements: `HasClasses`, `Widget`

### `ClippedPrimitive` (struct) — `epaint-0.35.0/src/lib.rs:142`

A [`Mesh`] or [`PaintCallback`] within a clip rectangle.

Public fields:

- `clip_rect: Rect` — Clip / scissor rectangle. Only show the part of the [`Mesh`] that falls within this.
- `primitive: Primitive` — What to paint - either a [`Mesh`] or a [`PaintCallback`].

Implements: `Clone`, `Debug`

### `ClosableTag` (struct) — `egui-0.35.0/src/containers/close_tag.rs:12`

A tag to mark a container as closable.

Public fields:

- `close: AtomicBool`

Methods:

- `fn set_close(&self)` — `egui-0.35.0/src/containers/close_tag.rs:20`
  Set close to `true`
- `fn should_close(&self) -> bool` — `egui-0.35.0/src/containers/close_tag.rs:25`
  Returns `true` if [`ClosableTag::set_close`] has been called.

Implements: `Debug`, `Default`

### `CollapsingHeader` (struct) — `egui-0.35.0/src/containers/collapsing_header.rs:377`

A header which can be collapsed/expanded, revealing a contained [`Ui`] region.

Methods:

- `fn default_open(self, open: bool) -> Self` — `egui-0.35.0/src/containers/collapsing_header.rs:415`
  By default, the [`CollapsingHeader`] is collapsed. Call `.default_open(true)` to change this.
- `fn enabled(self, enabled: bool) -> Self` — `egui-0.35.0/src/containers/collapsing_header.rs:443`
  If you set this to `false`, the [`CollapsingHeader`] will be grayed out and un-clickable.
- `fn icon(self, icon_fn: impl FnOnce(&mut Ui, f32, &Response) + 'static) -> Self` — `egui-0.35.0/src/containers/collapsing_header.rs:480`
  Use the provided function to render a different [`CollapsingHeader`] icon. Defaults to a triangle that animat…
- `fn id_salt(self, id_salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/containers/collapsing_header.rs:434`
  Explicitly set the source of the [`Id`] of this widget, instead of using title label. This is useful if the t…
- `fn new(text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/containers/collapsing_header.rs:396`
  The [`CollapsingHeader`] starts out collapsed unless you call `default_open`.
- `fn open(self, open: Option<bool>) -> Self` — `egui-0.35.0/src/containers/collapsing_header.rs:426`
  Calling `.open(Some(true))` will make the collapsing header open this frame (or stay open).
- `fn show<R>(self, ui: &mut Ui, add_body: impl FnOnce(&mut Ui) -> R) -> CollapsingResponse<R>` — `egui-0.35.0/src/containers/collapsing_header.rs:609`
- `fn show_background(self, show_background: bool) -> Self` — `egui-0.35.0/src/containers/collapsing_header.rs:457`
  Should the [`CollapsingHeader`] show a background behind it? Default: `false`.
- `fn show_unindented<R>(self, ui: &mut Ui, add_body: impl FnOnce(&mut Ui) -> R) -> CollapsingResponse<R>` — `egui-0.35.0/src/containers/collapsing_header.rs:618`

### `CollapsingResponse` (struct) — `egui-0.35.0/src/containers/collapsing_header.rs:672`

The response from showing a [`CollapsingHeader`].

Public fields:

- `header_response: Response` — Response of the actual clickable header.
- `body_response: Option<Response>` — None iff collapsed.
- `body_returned: Option<R>` — None iff collapsed.
- `openness: f32` — 0.0 if fully closed, 1.0 if fully open, and something in-between while animating.

Methods:

- `fn fully_closed(&self) -> bool` — `egui-0.35.0/src/containers/collapsing_header.rs:688`
  Was the [`CollapsingHeader`] fully closed (and not being animated)?
- `fn fully_open(&self) -> bool` — `egui-0.35.0/src/containers/collapsing_header.rs:693`
  Was the [`CollapsingHeader`] fully open (and not being animated)?

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

### `ComboBox` (struct) — `egui-0.35.0/src/containers/combo_box.rs:40`

A drop-down selection menu with a descriptive label.

Methods:

- `fn close_behavior(self, close_behavior: PopupCloseBehavior) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:189`
  Controls the close behavior for the popup.
- `fn from_id_salt(id_salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:85`
  Without label.
- `fn from_label(label: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:69`
  Label shown next to the combo box
- `fn height(self, height: f32) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:112`
  Set the maximum outer height of the menu.
- `fn icon(self, icon_fn: impl FnOnce(&Ui, Rect, &WidgetVisuals, bool) + 'static) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:155`
  Use the provided function to render a different [`ComboBox`] icon. Defaults to a triangle that expands when t…
- `fn is_open(ctx: &Context, id: Id) -> bool` — `egui-0.35.0/src/containers/combo_box.rs:309`
  Check if the [`ComboBox`] with the given id has its popup menu currently opened.
- `fn new(id_salt: impl AsIdSalt, label: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:54`
  Create new [`ComboBox`] with id and label
- `fn popup_style(self, popup_style: StyleModifier) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:199`
  Set the style of the popup menu.
- `fn selected_text(self, selected_text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:119`
  What we show as the currently selected value
- `fn show_index<Text>(self, ui: &mut Ui, selected: &mut usize, len: usize, get: impl Fn(usize) -> Text) -> Response` — `egui-0.35.0/src/containers/combo_box.rs:280`
  Show a list of items with the given selected index.
- `fn show_ui<R>(self, ui: &mut Ui, menu_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<Option<R>>` — `egui-0.35.0/src/containers/combo_box.rs:207`
  Show the combo box, with the given ui code for the menu contents.
- `fn truncate(self) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:180`
  Set [`Self::wrap_mode`] to [`TextWrapMode::Truncate`].
- `fn width(self, width: f32) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:103`
  Set the outer width of the button and menu.
- `fn wrap(self) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:173`
  Set [`Self::wrap_mode`] to [`TextWrapMode::Wrap`].
- `fn wrap_mode(self, wrap_mode: TextWrapMode) -> Self` — `egui-0.35.0/src/containers/combo_box.rs:166`
  Controls the wrap mode used for the selected text.

### `Context` (struct) — `egui-0.35.0/src/context.rs:710`

Your handle to egui.

Methods:

- `fn accesskit_node_builder<R>(&self, id: Id, writer: impl FnOnce(&mut Node) -> R) -> Option<R>` — `egui-0.35.0/src/context.rs:3582`
  If AccessKit support is active for the current frame, get or create a node builder with the specified ID and…
- `fn add_bytes_loader(&self, loader: Arc<dyn BytesLoader + Send + Sync + 'static>)` — `egui-0.35.0/src/context.rs:3636`
  Add a new bytes loader.
- `fn add_font(&self, new_font: FontInsert)` — `egui-0.35.0/src/context.rs:2061`
  Tell `egui` which fonts to use.
- `fn add_image_loader(&self, loader: Arc<dyn ImageLoader + Send + Sync + 'static>)` — `egui-0.35.0/src/context.rs:3645`
  Add a new image loader.
- `fn add_plugin(&self, plugin: impl Plugin + 'static)` — `egui-0.35.0/src/context.rs:1979`
  Register a [`Plugin`](plugin::Plugin)
- `fn add_texture_loader(&self, loader: Arc<dyn TextureLoader + Send + Sync + 'static>)` — `egui-0.35.0/src/context.rs:3654`
  Add a new texture loader.
- `fn all_styles_mut(&self, mutate_style: impl FnMut(&mut Style))` — `egui-0.35.0/src/context.rs:2145`
  Mutate the [`Style`]s used by all subsequent popups, menus, etc. in both dark and light mode.
- `fn animate_bool(&self, id: Id, value: bool) -> f32` — `egui-0.35.0/src/context.rs:3089`
  Returns a value in the range [0, 1], to indicate "how on" this thing is.
- `fn animate_bool_responsive(&self, id: Id, value: bool) -> f32` — `egui-0.35.0/src/context.rs:3099`
  Like [`Self::animate_bool`], but uses an easing function that makes the value move quickly in the beginning a…
- `fn animate_bool_with_easing(&self, id: Id, value: bool, easing: fn(f32) -> f32) -> f32` — `egui-0.35.0/src/context.rs:3105`
  Like [`Self::animate_bool`] but allows you to control the easing function.
- `fn animate_bool_with_time(&self, id: Id, target_value: bool, animation_time: f32) -> f32` — `egui-0.35.0/src/context.rs:3112`
  Like [`Self::animate_bool`] but allows you to control the animation time.
- `fn animate_bool_with_time_and_easing(&self, id: Id, target_value: bool, animation_time: f32, easing: fn(f32) -> f32) -> f32` — `egui-0.35.0/src/context.rs:3129`
  Like [`Self::animate_bool`] but allows you to control the animation time and easing function.
- `fn animate_value_with_time(&self, id: Id, target_value: f32, animation_time: f32) -> f32` — `egui-0.35.0/src/context.rs:3162`
  Smoothly animate an `f32` value.
- `fn any_popup_open(&self) -> bool` — `egui-0.35.0/src/context.rs:2910`
  Is a popup or (context) menu open?
- `fn begin_pass(&self, new_input: RawInput)` — `egui-0.35.0/src/context.rs:896`
  An alternative to calling [`Self::run_ui`].
- `fn check_for_id_clash(&self, id: Id, new_rect: Rect, what: &str)` — `egui-0.35.0/src/context.rs:1097`
  If the given [`Id`] has been used previously the same pass at different position, then an error will be print…
- `fn clear_animations(&self)` — `egui-0.35.0/src/context.rs:3180`
  Clear memory of any animations.
- `fn content_rect(&self) -> Rect` — `egui-0.35.0/src/context.rs:2805`
  Returns the position and size of the egui area that is safe for content rendering.
- `fn copy_image(&self, image: ColorImage)` — `egui-0.35.0/src/context.rs:1627`
  Copy the given image to the system clipboard.
- `fn copy_text(&self, text: String)` — `egui-0.35.0/src/context.rs:1618`
  Copy the given text to the system clipboard.
- `fn cumulative_frame_nr(&self) -> u64` — `egui-0.35.0/src/context.rs:1683`
  The total number of completed frames.
- `fn cumulative_frame_nr_for(&self, id: ViewportId) -> u64` — `egui-0.35.0/src/context.rs:1692`
  The total number of completed frames.
- `fn cumulative_pass_nr(&self) -> u64` — `egui-0.35.0/src/context.rs:1713`
  The total number of completed passes (usually there is one pass per rendered frame).
- `fn cumulative_pass_nr_for(&self, id: ViewportId) -> u64` — `egui-0.35.0/src/context.rs:1720`
  The total number of completed passes (usually there is one pass per rendered frame).
- `fn current_pass_index(&self) -> usize` — `egui-0.35.0/src/context.rs:1736`
  The index of the current pass in the current frame, starting at zero.
- `fn data<R>(&self, reader: impl FnOnce(&IdTypeMap) -> R) -> R` — `egui-0.35.0/src/context.rs:961`
  Read-only access to [`IdTypeMap`], which stores superficial widget state.
- `fn data_mut<R>(&self, writer: impl FnOnce(&mut IdTypeMap) -> R) -> R` — `egui-0.35.0/src/context.rs:967`
  Read-write access to [`IdTypeMap`], which stores superficial widget state.
- `fn debug_on_hover(&self) -> bool` — `egui-0.35.0/src/context.rs:3066`
  Whether or not to debug widget layout on hover.
- `fn debug_painter(&self) -> Painter` — `egui-0.35.0/src/context.rs:1525`
  Paint on top of _everything_ else (even on top of tooltips and popups).
- `fn debug_text(&self, text: impl Into<WidgetText>)` — `egui-0.35.0/src/context.rs:1543`
  Print this text next to the cursor at the end of the pass.
- `fn disable_accesskit(&self)` — `egui-0.35.0/src/context.rs:3604`
  Disable generation of AccessKit tree updates in all future frames.
- `fn drag_started_id(&self) -> Option<Id>` — `egui-0.35.0/src/context.rs:4118`
  This widget just started being dragged this pass.
- `fn drag_stopped_id(&self) -> Option<Id>` — `egui-0.35.0/src/context.rs:4123`
  This widget was being dragged, but was released this pass.
- `fn dragged_id(&self) -> Option<Id>` — `egui-0.35.0/src/context.rs:4101`
  The widget currently being dragged, if any.
- `fn dragging_something_else(&self, not_this: Id) -> bool` — `egui-0.35.0/src/context.rs:4160`
  Is something else being dragged?
- `fn egui_is_using_pointer(&self) -> bool` — `egui-0.35.0/src/context.rs:2879`
  Is egui currently using the pointer position (e.g. dragging a slider)?
- `fn egui_wants_keyboard_input(&self) -> bool` — `egui-0.35.0/src/context.rs:2884`
  If `true`, egui is currently listening on text input (e.g. typing text in a [`crate::TextEdit`]).
- `fn egui_wants_pointer_input(&self) -> bool` — `egui-0.35.0/src/context.rs:2871`
  True if egui is currently interested in the pointer (mouse or touch).
- `fn embed_viewports(&self) -> bool` — `egui-0.35.0/src/context.rs:3899`
  If `true`, [`Self::show_viewport_deferred`] and [`Self::show_viewport_immediate`] will embed the new viewport…
- `fn enable_accesskit(&self)` — `egui-0.35.0/src/context.rs:3599`
  Enable generation of AccessKit tree updates in all future frames.
- `fn end_pass(&self) -> FullOutput` — `egui-0.35.0/src/context.rs:2375`
  Call at the end of each frame if you called [`Context::begin_pass`].
- `fn fonts<R>(&self, reader: impl FnOnce(&FontsView<'_>) -> R) -> R` — `egui-0.35.0/src/context.rs:1031`
  Read-only access to [`Fonts`].
- `fn fonts_mut<R>(&self, reader: impl FnOnce(&mut FontsView<'_>) -> R) -> R` — `egui-0.35.0/src/context.rs:1048`
  Read-write access to [`Fonts`].
- `fn forget_all_images(&self)` — `egui-0.35.0/src/context.rs:3684`
  Release all memory and textures related to images used in [`Ui::image`] or [`crate::Image`].
- `fn forget_image(&self, uri: &str)` — `egui-0.35.0/src/context.rs:3662`
  Release all memory and textures related to the given image URI.
- `fn format_modifiers(&self, modifiers: Modifiers) -> String` — `egui-0.35.0/src/context.rs:1651`
  Format the given modifiers in a human-readable way (e.g. `Ctrl+Shift+X`).
- `fn format_shortcut(&self, shortcut: &KeyboardShortcut) -> String` — `egui-0.35.0/src/context.rs:1666`
  Format the given shortcut in a human-readable way (e.g. `Ctrl+Shift+X`).
- `fn global_style(&self) -> Arc<Style>` — `egui-0.35.0/src/context.rs:2107`
  The currently active [`Style`] used by all subsequent popups, menus, etc.
- `fn global_style_mut(&self, mutate_style: impl FnOnce(&mut Style))` — `egui-0.35.0/src/context.rs:2121`
  Mutate the currently active [`Style`] used by all subsequent popups, menus, etc. Use [`Self::all_styles_mut`]…
- `fn globally_used_rect(&self) -> Rect` — `egui-0.35.0/src/context.rs:2824`
  How much space is used by windows and the top-level [`Ui`].
- `fn graphics<R>(&self, reader: impl FnOnce(&GraphicLayers) -> R) -> R` — `egui-0.35.0/src/context.rs:979`
  Read-only access to [`GraphicLayers`], where painted [`crate::Shape`]s are written to.
- `fn graphics_mut<R>(&self, writer: impl FnOnce(&mut GraphicLayers) -> R) -> R` — `egui-0.35.0/src/context.rs:973`
  Read-write access to [`GraphicLayers`], where painted [`crate::Shape`]s are written to.
- `fn has_pending_images(&self) -> bool` — `egui-0.35.0/src/context.rs:3832`
  Returns `true` if any image is currently being loaded.
- `fn has_requested_repaint(&self) -> bool` — `egui-0.35.0/src/context.rs:1866`
  Has a repaint been requested for the current viewport?
- `fn has_requested_repaint_for(&self, viewport_id: &ViewportId) -> bool` — `egui-0.35.0/src/context.rs:1872`
  Has a repaint been requested for the given viewport?
- `fn highlight_widget(&self, id: Id)` — `egui-0.35.0/src/context.rs:2903`
  Highlight this widget, to make it look like it is hovered, even if it isn't.
- `fn include_bytes(&self, uri: impl Into<Cow<'static, str>>, bytes: impl Into<Bytes>)` — `egui-0.35.0/src/context.rs:3617`
  Associate some static bytes with a `uri`.
- `fn input<R>(&self, reader: impl FnOnce(&InputState) -> R) -> R` — `egui-0.35.0/src/context.rs:925`
  Read-only access to [`InputState`].
- `fn input_for<R>(&self, id: ViewportId, reader: impl FnOnce(&InputState) -> R) -> R` — `egui-0.35.0/src/context.rs:931`
  This will create a `InputState::default()` if there is no input state for that viewport
- `fn input_mut<R>(&self, writer: impl FnOnce(&mut InputState) -> R) -> R` — `egui-0.35.0/src/context.rs:937`
  Read-write access to [`InputState`].
- `fn input_mut_for<R>(&self, id: ViewportId, writer: impl FnOnce(&mut InputState) -> R) -> R` — `egui-0.35.0/src/context.rs:943`
  This will create a `InputState::default()` if there is no input state for that viewport
- `fn inspection_ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/context.rs:3223`
  Show the state of egui, including its input and output.
- `fn interaction_snapshot<R>(&self, reader: impl FnOnce(&InteractionSnapshot) -> R) -> R` — `egui-0.35.0/src/context.rs:4090`
  Read which widgets are currently being interacted with.
- `fn interactive_rects_last_pass(&self) -> Vec<Rect>` — `egui-0.35.0/src/context.rs:1321`
  Rectangles that could receive pointer input in the last completed pass.
- `fn is_being_dragged(&self, id: Id) -> bool` — `egui-0.35.0/src/context.rs:4111`
  Is this specific widget being dragged?
- `fn is_loader_installed(&self, id: &str) -> bool` — `egui-0.35.0/src/context.rs:3623`
  Returns `true` if the chain of bytes, image, or texture loaders contains a loader with the given `id`.
- `fn is_pointer_over_egui(&self) -> bool` — `egui-0.35.0/src/context.rs:2841`
  Is the pointer (mouse/touch) over any egui area?
- `fn layer_id_at(&self, pos: Pos2) -> Option<LayerId>` — `egui-0.35.0/src/context.rs:3002`
  Top-most layer at the given position.
- `fn layer_painter(&self, layer_id: LayerId) -> Painter` — `egui-0.35.0/src/context.rs:1519`
  Get a full-screen painter for a new or existing layer
- `fn layer_transform_from_global(&self, layer_id: LayerId) -> Option<TSTransform>` — `egui-0.35.0/src/context.rs:2983`
  Return how to transform the graphics of the global coordinate system into the local coordinate system of the…
- `fn layer_transform_to_global(&self, layer_id: LayerId) -> Option<TSTransform>` — `egui-0.35.0/src/context.rs:2976`
  Return how to transform the graphics of the given layer into the global coordinate system.
- `fn load_texture(&self, name: impl Into<String>, image: impl Into<ImageData>, options: TextureOptions) -> TextureHandle` — `egui-0.35.0/src/context.rs:2322`
  Allocate a texture.
- `fn loaders(&self) -> Arc<Loaders>` — `egui-0.35.0/src/context.rs:3827`
  The loaders of bytes, images, and textures.
- `fn loaders_ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/context.rs:3410`
  Show stats about different image loaders.
- `fn memory<R>(&self, reader: impl FnOnce(&Memory) -> R) -> R` — `egui-0.35.0/src/context.rs:949`
  Read-only access to [`Memory`].
- `fn memory_mut<R>(&self, writer: impl FnOnce(&mut Memory) -> R) -> R` — `egui-0.35.0/src/context.rs:955`
  Read-write access to [`Memory`].
- `fn memory_ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/context.rs:3477`
  Shows the contents of [`Self::memory`].
- `fn move_to_top(&self, layer_id: LayerId)` — `egui-0.35.0/src/context.rs:3009`
  Moves the given area to the top in its [`Order`].
- `fn multi_touch(&self) -> Option<MultiTouchInfo>` — `egui-0.35.0/src/context.rs:2946`
  Calls [`InputState::multi_touch`].
- `fn native_pixels_per_point(&self) -> Option<f32>` — `egui-0.35.0/src/context.rs:2239`
  The number of physical pixels for each logical point on this monitor.
- `fn on_begin_pass(&self, debug_name: &'static str, cb: ContextCallback)` — `egui-0.35.0/src/context.rs:1958`
  Call the given callback at the start of each pass of each viewport.
- `fn on_end_pass(&self, debug_name: &'static str, cb: ContextCallback)` — `egui-0.35.0/src/context.rs:1967`
  Call the given callback at the end of each pass of each viewport.
- `fn open_url(&self, open_url: OpenUrl)` — `egui-0.35.0/src/context.rs:1609`
  Open an URL in a browser.
- `fn options<R>(&self, reader: impl FnOnce(&Options) -> R) -> R` — `egui-0.35.0/src/context.rs:1063`
  Read-only access to [`Options`].
- `fn options_mut<R>(&self, writer: impl FnOnce(&mut Options) -> R) -> R` — `egui-0.35.0/src/context.rs:1069`
  Read-write access to [`Options`].
- `fn os(&self) -> OperatingSystem` — `egui-0.35.0/src/context.rs:1559`
  What operating system are we running on?
- `fn output<R>(&self, reader: impl FnOnce(&PlatformOutput) -> R) -> R` — `egui-0.35.0/src/context.rs:992`
  Read-only access to [`PlatformOutput`].
- `fn output_mut<R>(&self, writer: impl FnOnce(&mut PlatformOutput) -> R) -> R` — `egui-0.35.0/src/context.rs:998`
  Read-write access to [`PlatformOutput`].
- `fn parent_viewport_id(&self) -> ViewportId` — `egui-0.35.0/src/context.rs:3856`
  Return the `ViewportId` of his parent.
- `fn pixels_per_point(&self) -> f32` — `egui-0.35.0/src/context.rs:2220`
  The number of physical pixels for each logical point.
- `fn plugin<T>(&self) -> TypedPluginHandle<T>` — `egui-0.35.0/src/context.rs:2004`
  Get a handle to the plugin of type `T`.
- `fn plugin_opt<T>(&self) -> Option<TypedPluginHandle<T>>` — `egui-0.35.0/src/context.rs:2013`
  Get a handle to the plugin of type `T`, if it was registered.
- `fn plugin_or_default<T>(&self) -> TypedPluginHandle<T>` — `egui-0.35.0/src/context.rs:2019`
  Get a handle to the plugin of type `T`, or insert its default.
- `fn pointer_hover_pos(&self) -> Option<Pos2>` — `egui-0.35.0/src/context.rs:2931`
  If it is a good idea to show a tooltip, where is pointer?
- `fn pointer_interact_pos(&self) -> Option<Pos2>` — `egui-0.35.0/src/context.rs:2941`
  If you detect a click or drag and want to know where it happened, use this.
- `fn pointer_latest_pos(&self) -> Option<Pos2>` — `egui-0.35.0/src/context.rs:2925`
  Latest reported pointer position.
- `fn read_response(&self, id: Id) -> Option<Response>` — `egui-0.35.0/src/context.rs:1287`
  Read the response of some widget, which may be called _before_ creating the widget (!).
- `fn rect_contains_pointer(&self, layer_id: LayerId, rect: Rect) -> bool` — `egui-0.35.0/src/context.rs:3036`
  Does the given rectangle contain the mouse pointer?
- `fn register_widget_info(&self, id: Id, make_info: impl Fn() -> WidgetInfo)` — `egui-0.35.0/src/context.rs:1504`
  This is called by [`Response::widget_info`], but can also be called directly.
- `fn repaint_causes(&self) -> Vec<RepaintCause>` — `egui-0.35.0/src/context.rs:1879`
  Why are we repainting?
- `fn request_discard(&self, reason: impl Into<Cow<'static, str>>)` — `egui-0.35.0/src/context.rs:1924`
  Request to discard the visual output of this pass, and to immediately do another one.
- `fn request_repaint(&self)` — `egui-0.35.0/src/context.rs:1753`
  Call this if there is need to repaint the UI, i.e. if you are showing an animation.
- `fn request_repaint_after(&self, duration: Duration)` — `egui-0.35.0/src/context.rs:1804`
  Request repaint after at most the specified duration elapses.
- `fn request_repaint_after_for(&self, duration: Duration, id: ViewportId)` — `egui-0.35.0/src/context.rs:1847`
  Request repaint after at most the specified duration elapses.
- `fn request_repaint_after_secs(&self, seconds: f32)` — `egui-0.35.0/src/context.rs:1812`
  Repaint after this many seconds.
- `fn request_repaint_of(&self, id: ViewportId)` — `egui-0.35.0/src/context.rs:1770`
  Call this if there is need to repaint the UI, i.e. if you are showing an animation.
- `fn requested_repaint_last_pass(&self) -> bool` — `egui-0.35.0/src/context.rs:1854`
  Was a repaint requested last pass for the current viewport?
- `fn requested_repaint_last_pass_for(&self, viewport_id: &ViewportId) -> bool` — `egui-0.35.0/src/context.rs:1860`
  Was a repaint requested last pass for the given viewport?
- `fn run_ui(&self, new_input: RawInput, run_ui: impl FnMut(&mut Ui)) -> FullOutput` — `egui-0.35.0/src/context.rs:780`
  Run the ui code for one frame.
- `fn send_cmd(&self, cmd: OutputCommand)` — `egui-0.35.0/src/context.rs:1597`
  Add a command to [`PlatformOutput::commands`], for the integration to execute at the end of the frame.
- `fn send_viewport_cmd(&self, command: ViewportCommand)` — `egui-0.35.0/src/context.rs:3914`
  Send a command to the current viewport.
- `fn send_viewport_cmd_to(&self, id: ViewportId, command: ViewportCommand)` — `egui-0.35.0/src/context.rs:3921`
  Send a command to a specific viewport.
- `fn set_cursor_icon(&self, cursor_icon: CursorIcon)` — `egui-0.35.0/src/context.rs:1578`
  Set the cursor icon.
- `fn set_cursor_image(&self, image: Option<CustomCursorImage>)` — `egui-0.35.0/src/context.rs:1591`
  Request that the integration display this RGBA bitmap as the OS cursor for the next frame, instead of the sta…
- `fn set_debug_on_hover(&self, debug_on_hover: bool)` — `egui-0.35.0/src/context.rs:3072`
  Turn on/off whether or not to debug widget layout on hover.
- `fn set_dragged_id(&self, id: Id)` — `egui-0.35.0/src/context.rs:4128`
  Set which widget is being dragged.
- `fn set_embed_viewports(&self, value: bool)` — `egui-0.35.0/src/context.rs:3907`
  If `true`, [`Self::show_viewport_deferred`] and [`Self::show_viewport_immediate`] will embed the new viewport…
- `fn set_fonts(&self, font_definitions: FontDefinitions)` — `egui-0.35.0/src/context.rs:2038`
  Tell `egui` which fonts to use.
- `fn set_global_style(&self, style: impl Into<Arc<Style>>)` — `egui-0.35.0/src/context.rs:2132`
  The currently active [`Style`] used by all new popups, menus, etc.
- `fn set_immediate_viewport_renderer(callback: impl Fn(&Self, ImmediateViewport<'a>) + 'static)` — `egui-0.35.0/src/context.rs:3886`
  For integrations: Set this to render a sync viewport.
- `fn set_os(&self, os: OperatingSystem)` — `egui-0.35.0/src/context.rs:1567`
  Set the operating system we are running on.
- `fn set_pixels_per_point(&self, pixels_per_point: f32)` — `egui-0.35.0/src/context.rs:2228`
  Set the number of physical pixels for each logical point. Will become active at the start of the next pass.
- `fn set_request_repaint_callback(&self, callback: impl Fn(RequestRepaintInfo) + Send + Sync + 'static)` — `egui-0.35.0/src/context.rs:1893`
  For integrations: this callback will be called when an egui user calls [`Self::request_repaint`] or [`Self::r…
- `fn set_style_of(&self, theme: Theme, style: impl Into<Arc<Style>>)` — `egui-0.35.0/src/context.rs:2182`
  The [`Style`] used by all new popups, menus, etc. Use [`Self::set_theme`] to choose between dark and light mo…
- `fn set_sublayer(&self, parent: LayerId, child: LayerId)` — `egui-0.35.0/src/context.rs:3020`
  Mark the `child` layer as a sublayer of `parent`.
- `fn set_theme(&self, theme_preference: impl Into<ThemePreference>)` — `egui-0.35.0/src/context.rs:2102`
  The [`Theme`] used to select between dark and light [`Self::global_style`] as the active style used by all su…
- `fn set_transform_layer(&self, layer_id: LayerId, transform: TSTransform)` — `egui-0.35.0/src/context.rs:2963`
  Transform the graphics of the given layer.
- `fn set_visuals(&self, visuals: Visuals)` — `egui-0.35.0/src/context.rs:2212`
  The [`crate::Visuals`] used by all subsequent popups, menus, etc.
- `fn set_visuals_of(&self, theme: Theme, visuals: Visuals)` — `egui-0.35.0/src/context.rs:2199`
  The [`crate::Visuals`] used by all subsequent popups, menus, etc.
- `fn set_zoom_factor(&self, zoom_factor: f32)` — `egui-0.35.0/src/context.rs:2269`
  Sets zoom factor of the UI. Will become active at the start of the next pass.
- `fn settings_ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/context.rs:3187`
  Show a ui for settings (style and tessellation options).
- `fn show_viewport_deferred(&self, new_viewport_id: ViewportId, viewport_builder: ViewportBuilder, viewport_ui_cb: impl Fn(&mut Ui, ViewportClass) + Send + Sync + 'static)` — `egui-0.35.0/src/context.rs:3960`
  Show a deferred viewport, creating a new native window, if possible.
- `fn show_viewport_immediate<T>(&self, new_viewport_id: ViewportId, builder: ViewportBuilder, viewport_ui_cb: impl FnMut(&mut Ui, ViewportClass) -> T) -> T` — `egui-0.35.0/src/context.rs:4014`
  Show an immediate viewport, creating a new native window, if possible.
- `fn stop_dragging(&self)` — `egui-0.35.0/src/context.rs:4143`
  Stop dragging any widget.
- `fn style_mut_of(&self, theme: Theme, mutate_style: impl FnOnce(&mut Style))` — `egui-0.35.0/src/context.rs:2169`
  Mutate the [`Style`] used by all subsequent popups, menus, etc.
- `fn style_of(&self, theme: Theme) -> Arc<Style>` — `egui-0.35.0/src/context.rs:2153`
  The [`Style`] used by all subsequent popups, menus, etc.
- `fn style_ui(&self, ui: &mut Ui, theme: Theme)` — `egui-0.35.0/src/context.rs:3564`
  Edit the [`Style`].
- `fn system_theme(&self) -> Option<Theme>` — `egui-0.35.0/src/context.rs:2084`
  Does the OS use dark or light mode? This is used when the theme preference is set to [`crate::ThemePreference…
- `fn tessellate(&self, shapes: Vec<ClippedShape>, pixels_per_point: f32) -> Vec<ClippedPrimitive>` — `egui-0.35.0/src/context.rs:2757`
  Tessellate the given shapes into triangle meshes.
- `fn tessellation_options<R>(&self, reader: impl FnOnce(&TessellationOptions) -> R) -> R` — `egui-0.35.0/src/context.rs:1075`
  Read-only access to [`TessellationOptions`].
- `fn tessellation_options_mut<R>(&self, writer: impl FnOnce(&mut TessellationOptions) -> R) -> R` — `egui-0.35.0/src/context.rs:1081`
  Read-write access to [`TessellationOptions`].
- `fn tex_manager(&self) -> Arc<RwLock<TextureManager>>` — `egui-0.35.0/src/context.rs:2349`
  Low-level texture manager.
- `fn text_edit_focused(&self) -> bool` — `egui-0.35.0/src/context.rs:2889`
  Is the currently focused widget a text edit?
- `fn texture_ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/context.rs:3349`
  Show stats about the allocated textures.
- `fn theme(&self) -> Theme` — `egui-0.35.0/src/context.rs:2090`
  The [`Theme`] used to select the appropriate [`Style`] (dark or light) used by all subsequent popups, menus,…
- `fn time(&self) -> f64` — `egui-0.35.0/src/context.rs:1548`
  Current time in seconds, relative to some unknown epoch.
- `fn top_layer_id(&self) -> Option<LayerId>` — `egui-0.35.0/src/context.rs:3025`
  Retrieve the [`LayerId`] of the top level windows.
- `fn transform_layer_shapes(&self, layer_id: LayerId, transform: TSTransform)` — `egui-0.35.0/src/context.rs:2995`
  Transform all the graphics at the given layer.
- `fn try_load_bytes(&self, uri: &str) -> BytesLoadResult` — `egui-0.35.0/src/context.rs:3721`
  Try loading the bytes from the given uri using any available bytes loaders.
- `fn try_load_image(&self, uri: &str, size_hint: SizeHint) -> ImageLoadResult` — `egui-0.35.0/src/context.rs:3759`
  Try loading the image from the given uri using any available image loaders.
- `fn try_load_texture(&self, uri: &str, texture_options: TextureOptions, size_hint: SizeHint) -> TextureLoadResult` — `egui-0.35.0/src/context.rs:3804`
  Try loading the texture from the given uri using any available texture loaders.
- `fn viewport<R>(&self, reader: impl FnOnce(&ViewportState) -> R) -> R` — `egui-0.35.0/src/context.rs:3861`
  Read the state of the current viewport.
- `fn viewport_for<R>(&self, viewport_id: ViewportId, reader: impl FnOnce(&ViewportState) -> R) -> R` — `egui-0.35.0/src/context.rs:3866`
  Read the state of a specific current viewport.
- `fn viewport_id(&self) -> ViewportId` — `egui-0.35.0/src/context.rs:3847`
  Return the `ViewportId` of the current viewport.
- `fn viewport_rect(&self) -> Rect` — `egui-0.35.0/src/context.rs:2819`
  Returns the position and size of the full area available to egui
- `fn will_discard(&self) -> bool` — `egui-0.35.0/src/context.rs:1943`
  Will the visual output of this pass be discarded?
- `fn with_plugin<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R>` — `egui-0.35.0/src/context.rs:1992`
  Call the provided closure with the plugin of type `T`, if it was registered.
- `fn zoom_factor(&self) -> f32` — `egui-0.35.0/src/context.rs:2251`
  Global zoom factor of the UI.

Implements: `Clone`, `Debug`, `Default`, `PartialEq`

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

### `CustomCursorImage` (struct) — `egui-0.35.0/src/data/output.rs:299`

A bitmap cursor pushed to the integration via [`PlatformOutput::cursor_image`].

Public fields:

- `rgba: Arc<[u8]>`
- `size: [u16; 2]`
- `hotspot: [u16; 2]`

Implements: `Clone`, `Debug`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `DragAndDrop` (struct) — `egui-0.35.0/src/drag_and_drop.rs:22`

Plugin for tracking drag-and-drop payload.

Methods:

- `fn clear_payload(ctx: &Context)` — `egui-0.35.0/src/drag_and_drop.rs:86`
  Clears the payload, setting it to `None`.
- `fn has_any_payload(ctx: &Context) -> bool` — `egui-0.35.0/src/drag_and_drop.rs:133`
  Are we carrying a payload?
- `fn has_payload_of_type<Payload>(ctx: &Context) -> bool` — `egui-0.35.0/src/drag_and_drop.rs:122`
  Are we carrying a payload of the given type?
- `fn payload<Payload>(ctx: &Context) -> Option<Arc<Payload>>` — `egui-0.35.0/src/drag_and_drop.rs:96`
  Retrieve the payload, if any.
- `fn set_payload<Payload>(ctx: &Context, payload: Payload)` — `egui-0.35.0/src/drag_and_drop.rs:78`
  Set a drag-and-drop payload.
- `fn take_payload<Payload>(ctx: &Context) -> Option<Arc<Payload>>` — `egui-0.35.0/src/drag_and_drop.rs:111`
  Retrieve and clear the payload, if any.

Implements: `Clone`, `Default`, `Plugin`

### `DragPanButtons` (struct) — `egui-0.35.0/src/containers/scene.rs:55`

Specifies which pointer buttons can be used to pan the scene by dragging.

Methods:

- `const fn all() -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  Get a flags value with all known bits set.
- `const fn bits(&self) -> u8` — `egui-0.35.0/src/containers/scene.rs:57`
  Get the underlying bits value.
- `const fn complement(self) -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  The bitwise negation (`!`) of the bits in `self`, truncating the result.
- `const fn contains(&self, other: Self) -> bool` — `egui-0.35.0/src/containers/scene.rs:57`
  Whether all set bits in `other` are also set in `self`.
- `const fn difference(self, other: Self) -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  The intersection of `self` with the complement of `other` (`&!`).
- `const fn empty() -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  Get a flags value with all bits unset.
- `const fn from_bits(bits: u8) -> Option<Self>` — `egui-0.35.0/src/containers/scene.rs:57`
  Convert from a bits value.
- `const fn from_bits_retain(bits: u8) -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  Convert from a bits value exactly.
- `const fn from_bits_truncate(bits: u8) -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  Convert from a bits value, unsetting any unknown bits.
- `const fn intersection(self, other: Self) -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  The bitwise and (`&`) of the bits in `self` and `other`.
- `const fn intersects(&self, other: Self) -> bool` — `egui-0.35.0/src/containers/scene.rs:57`
  Whether any set bits in `other` are also set in `self`.
- `const fn is_all(&self) -> bool` — `egui-0.35.0/src/containers/scene.rs:57`
  Whether all known bits in this flags value are set.
- `const fn is_empty(&self) -> bool` — `egui-0.35.0/src/containers/scene.rs:57`
  Whether all bits in `self` are unset.
- `const fn iter(&self) -> Iter<DragPanButtons>` — `egui-0.35.0/src/containers/scene.rs:57`
  Yield a set of contained flags values.
- `const fn iter_names(&self) -> IterNames<DragPanButtons>` — `egui-0.35.0/src/containers/scene.rs:57`
  Yield a set of contained named flags values.
- `const fn symmetric_difference(self, other: Self) -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  The bitwise exclusive-or (`^`) of the bits in `self` and `other`.
- `const fn union(self, other: Self) -> Self` — `egui-0.35.0/src/containers/scene.rs:57`
  The bitwise or (`|`) of the bits in `self` and `other`.
- `fn from_name(name: &str) -> Option<Self>` — `egui-0.35.0/src/containers/scene.rs:57`
  Get a flags value with the bits of a flag with the given name set.
- `fn insert(&mut self, other: Self)` — `egui-0.35.0/src/containers/scene.rs:57`
  The bitwise or (`|`) of the bits in `self` and `other`.
- `fn remove(&mut self, other: Self)` — `egui-0.35.0/src/containers/scene.rs:57`
  The intersection of `self` with the complement of `other` (`&!`).
- `fn set(&mut self, other: Self, value: bool)` — `egui-0.35.0/src/containers/scene.rs:57`
  Call `insert` when `value` is `true` or `remove` when `value` is `false`.
- `fn toggle(&mut self, other: Self)` — `egui-0.35.0/src/containers/scene.rs:57`
  The bitwise exclusive-or (`^`) of the bits in `self` and `other`.

Implements: `Binary`, `BitAnd`, `BitAndAssign`, `BitOr`, `BitOrAssign`, `BitXor`, `BitXorAssign`, `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `Extend<DragPanButtons>`, `Flags`, `FromIterator<DragPanButtons>`, `IntoIterator`, `LowerHex`, `Not`, `Octal`, `PartialEq`, `StructuralPartialEq`, `Sub`, `SubAssign`, `UpperHex`

### `DragValue` (struct) — `egui-0.35.0/src/widgets/drag_value.rs:37`

A numeric value that you can change by dragging the number. More compact than a [`crate::Slider`].

Methods:

- `fn atoms(&self) -> &Atoms<'a>` — `egui-0.35.0/src/widgets/drag_value.rs:416`
  Output the [`DragValue`]'s [`Atoms`].
- `fn binary(self, min_width: usize, twos_complement: bool) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:309`
  Set `custom_formatter` and `custom_parser` to display and parse numbers as binary integers. Floating point nu…
- `fn clamp_existing_to_range(self, clamp_existing_to_range: bool) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:145`
  If set to `true`, existing values will be clamped to [`Self::range`].
- `fn custom_formatter(self, formatter: impl 'a + Fn(f64, RangeInclusive<usize>) -> String) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:240`
  Set custom formatter defining how numbers are converted into text.
- `fn custom_parser(self, parser: impl 'a + Fn(&str) -> Option<f64>) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:285`
  Set custom parser defining how the text input is parsed into a number.
- `fn fixed_decimals(self, num_decimals: usize) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:196`
  Set an exact number of decimals to display. Values will also be rounded to this number of decimals. Normally…
- `fn from_get_set(get_set_value: impl 'a + FnMut(Option<f64>) -> f64) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:68`
- `fn hexadecimal(self, min_width: usize, twos_complement: bool, upper: bool) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:379`
  Set `custom_formatter` and `custom_parser` to display and parse numbers as hexadecimal integers. Floating poi…
- `fn max_decimals(self, max_decimals: usize) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:180`
  Set a maximum number of decimals to display. Values will also be rounded to this number of decimals. Normally…
- `fn max_decimals_opt(self, max_decimals: Option<usize>) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:186`
- `fn min_decimals(self, min_decimals: usize) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:169`
  Set a minimum number of decimals to display. Normally you don't need to pick a precision, as the slider will…
- `fn new<Num>(value: &'a mut Num) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:53`
- `fn octal(self, min_width: usize, twos_complement: bool) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:344`
  Set `custom_formatter` and `custom_parser` to display and parse numbers as octal integers. Floating point num…
- `fn prefix(self, prefix: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:152`
  Show a prefix before the number, e.g. "x: "
- `fn range<Num>(self, range: RangeInclusive<Num>) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:99`
  Sets valid range for dragging the value.
- `fn speed(self, speed: impl Into<f64>) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:89`
  How much the value changes when dragged one point (logical pixel).
- `fn suffix(self, suffix: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:159`
  Add a suffix to the number, this can be e.g. a unit ("°" or " m")
- `fn update_while_editing(self, update: bool) -> Self` — `egui-0.35.0/src/widgets/drag_value.rs:408`
  Update the value on each key press when text-editing the value.

Implements: `Widget`

### `DroppedFile` (struct) — `egui-0.35.0/src/data/input/dropped_file.rs:4`

A file dropped into egui.

Public fields:

- `path: Option<PathBuf>` — Set by the `egui-winit` backend.
- `name: String` — Name of the file. Set by the `eframe` web backend.
- `mime: String` — With the `eframe` web backend, this is set to the mime-type of the file (if available).
- `last_modified: Option<SystemTime>` — Set by the `eframe` web backend.
- `bytes: Option<Arc<[u8]>>` — Set by the `eframe` web backend.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `EventFilter` (struct) — `egui-0.35.0/src/data/input/event_filter.rs:11`

Controls which events that a focused widget will have exclusive access to.

Public fields:

- `tab: bool` — If `true`, pressing tab will act on the widget, and NOT move focus away from the focused…
- `horizontal_arrows: bool` — If `true`, pressing horizontal arrows will act on the widget, and NOT move focus away fro…
- `vertical_arrows: bool` — If `true`, pressing vertical arrows will act on the widget, and NOT move focus away from…
- `escape: bool` — If `true`, pressing escape will act on the widget, and NOT surrender focus from the focus…

Methods:

- `fn matches(&self, event: &Event) -> bool` — `egui-0.35.0/src/data/input/event_filter.rs:50`

Implements: `Clone`, `Copy`, `Debug`, `Default`

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

### `Frame` (struct) — `egui-0.35.0/src/containers/frame.rs:96`

A frame around some content, including margin, colors, etc.

Public fields:

- `inner_margin: Margin` — Margin within the painted frame.
- `fill: Color32` — The background fill color of the frame, within the [`Self::stroke`].
- `stroke: Stroke` — The width and color of the outline around the frame.
- `corner_radius: CornerRadius` — The rounding of the _outer_ corner of the [`Self::stroke`] (or, if there is no stroke, th…
- `outer_margin: Margin` — Margin outside the painted frame.
- `shadow: Shadow` — Optional drop-shadow behind the frame.

Methods:

- `const fn new() -> Self` — `egui-0.35.0/src/containers/frame.rs:173`
  No colors, no margins, no border.
- `fn begin(self, ui: &mut Ui) -> Prepared` — `egui-0.35.0/src/containers/frame.rs:378`
  Begin a dynamically colored frame.
- `fn canvas(style: &Style) -> Self` — `egui-0.35.0/src/containers/frame.rs:227`
  A canvas to draw on.
- `fn central_panel(style: &Style) -> Self` — `egui-0.35.0/src/containers/frame.rs:191`
- `fn corner_radius(self, corner_radius: impl Into<CornerRadius>) -> Self` — `egui-0.35.0/src/containers/frame.rs:277`
  The rounding of the _outer_ corner of the [`Self::stroke`] (or, if there is no stroke, the outer corner of [`…
- `fn dark_canvas(style: &Style) -> Self` — `egui-0.35.0/src/containers/frame.rs:236`
  A dark canvas to draw on.
- `fn fill(self, fill: Color32) -> Self` — `egui-0.35.0/src/containers/frame.rs:258`
  The background fill color of the frame, within the [`Self::stroke`].
- `fn fill_rect(&self, content_rect: Rect) -> Rect` — `egui-0.35.0/src/containers/frame.rs:336`
  Calculate the `fill_rect` from the `content_rect`.
- `fn group(style: &Style) -> Self` — `egui-0.35.0/src/containers/frame.rs:178`
  For when you want to group a few widgets together within a frame.
- `fn inner_margin(self, inner_margin: impl Into<Margin>) -> Self` — `egui-0.35.0/src/containers/frame.rs:248`
  Margin within the painted frame.
- `fn menu(style: &Style) -> Self` — `egui-0.35.0/src/containers/frame.rs:205`
- `fn multiply_with_opacity(self, opacity: f32) -> Self` — `egui-0.35.0/src/containers/frame.rs:313`
  Opacity multiplier in gamma space.
- `fn outer_margin(self, outer_margin: impl Into<Margin>) -> Self` — `egui-0.35.0/src/containers/frame.rs:296`
  Margin outside the painted frame.
- `fn outer_rect(&self, content_rect: Rect) -> Rect` — `egui-0.35.0/src/containers/frame.rs:350`
  Calculate the `outer_rect` from the `content_rect`.
- `fn paint(&self, content_rect: Rect) -> Shape` — `egui-0.35.0/src/containers/frame.rs:423`
  Paint this frame as a shape.
- `fn popup(style: &Style) -> Self` — `egui-0.35.0/src/containers/frame.rs:214`
- `fn shadow(self, shadow: Shadow) -> Self` — `egui-0.35.0/src/containers/frame.rs:303`
  Optional drop-shadow behind the frame.
- `fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/frame.rs:404`
  Show the given ui surrounded by this frame.
- `fn show_dyn<R>(self, ui: &mut Ui, add_contents: Box<dyn FnOnce(&mut Ui) -> R + 'c>) -> InnerResponse<R>` — `egui-0.35.0/src/containers/frame.rs:411`
  Show using dynamic dispatch.
- `fn side_top_panel(style: &Style) -> Self` — `egui-0.35.0/src/containers/frame.rs:185`
- `fn stroke(self, stroke: impl Into<Stroke>) -> Self` — `egui-0.35.0/src/containers/frame.rs:267`
  The width and color of the outline around the frame.
- `fn total_margin(&self) -> MarginF32` — `egui-0.35.0/src/containers/frame.rs:327`
  How much extra space the frame uses up compared to the content.
- `fn widget_rect(&self, content_rect: Rect) -> Rect` — `egui-0.35.0/src/containers/frame.rs:343`
  Calculate the `widget_rect` from the `content_rect`.
- `fn window(style: &Style) -> Self` — `egui-0.35.0/src/containers/frame.rs:196`
  The default frame for an [`crate::Window`].

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`, `Widget`

### `FrameDurations` (struct) — `egui-0.35.0/src/widgets/image.rs:878`

Stores the durations between each frame of an animated image

Methods:

- `fn all(&self) -> Iter<'_, Duration>` — `egui-0.35.0/src/widgets/image.rs:885`
- `fn new(durations: Vec<Duration>) -> Self` — `egui-0.35.0/src/widgets/image.rs:881`

Implements: `Clone`, `Debug`, `Default`, `Eq`, `Hash`, `PartialEq`, `StructuralPartialEq`

### `FullOutput` (struct) — `egui-0.35.0/src/data/output.rs:13`

What egui emits each frame from [`crate::Context::run_ui`].

Public fields:

- `platform_output: PlatformOutput` — Non-rendering related output.
- `textures_delta: TexturesDelta` — Texture changes since last frame (including the font texture).
- `shapes: Vec<ClippedShape>` — What to paint.
- `pixels_per_point: f32` — The number of physical pixels per logical ui point, for the viewport that was updated.
- `viewport_output: OrderedViewportIdMap<ViewportOutput>` — All the active viewports, including the root.

Methods:

- `fn append(&mut self, newer: Self)` — `egui-0.35.0/src/data/output.rs:44`
  Add on new output.

Implements: `Clone`, `Default`

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

### `Grid` (struct) — `egui-0.35.0/src/grid.rs:314`

A simple grid layout.

Methods:

- `fn max_col_width(self, max_col_width: f32) -> Self` — `egui-0.35.0/src/grid.rs:390`
  Set soft maximum width (wrapping width) of each column.
- `fn min_col_width(self, min_col_width: f32) -> Self` — `egui-0.35.0/src/grid.rs:375`
  Set minimum width of each column. Default: [`crate::style::Spacing::interact_size`]`.x`.
- `fn min_row_height(self, min_row_height: f32) -> Self` — `egui-0.35.0/src/grid.rs:383`
  Set minimum height of each row. Default: [`crate::style::Spacing::interact_size`]`.y`.
- `fn new(id_salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/grid.rs:327`
  Create a new [`Grid`] with a locally unique identifier.
- `fn num_columns(self, num_columns: usize) -> Self` — `egui-0.35.0/src/grid.rs:352`
  Setting this will allow the last column to expand to take up the rest of the space of the parent [`Ui`].
- `fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/grid.rs:413`
- `fn spacing(self, spacing: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/grid.rs:398`
  Set spacing between columns/rows. Default: [`crate::style::Spacing::item_spacing`].
- `fn start_row(self, start_row: usize) -> Self` — `egui-0.35.0/src/grid.rs:406`
  Change which row number the grid starts on. This can be useful when you have a large [`crate::Grid`] inside o…
- `fn striped(self, striped: bool) -> Self` — `egui-0.35.0/src/grid.rs:361`
  If `true`, add a subtle background color to every other row.
- `fn with_row_color<F>(self, color_picker: F) -> Self` — `egui-0.35.0/src/grid.rs:342`
  Setting this will allow for dynamic coloring of rows of the grid object

### `HoveredFile` (struct) — `egui-0.35.0/src/data/input/hovered_file.rs:4`

A file about to be dropped into egui.

Public fields:

- `path: Option<PathBuf>` — Set by the `egui-winit` backend.
- `mime: String` — With the `eframe` web backend, this is set to the mime-type of the file (if available).

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Hyperlink` (struct) — `egui-0.35.0/src/widgets/hyperlink.rs:92`

A clickable hyperlink, e.g. to `"https://github.com/emilk/egui"`.

Methods:

- `fn from_label_and_url(text: impl Into<WidgetText>, url: impl ToString) -> Self` — `egui-0.35.0/src/widgets/hyperlink.rs:110`
- `fn new(url: impl ToString) -> Self` — `egui-0.35.0/src/widgets/hyperlink.rs:100`
- `fn open_in_new_tab(self, new_tab: bool) -> Self` — `egui-0.35.0/src/widgets/hyperlink.rs:120`
  Always open this hyperlink in a new browser tab.

Implements: `Widget`

### `IconData` (struct) — `egui-0.35.0/src/viewport.rs:184`

Image data for an application icon.

Public fields:

- `rgba: Vec<u8>` — RGBA pixels, with separate/unmultiplied alpha.
- `width: u32` — Image width. This should be a multiple of 4.
- `height: u32` — Image height. This should be a multiple of 4.

Methods:

- `fn is_empty(&self) -> bool` — `egui-0.35.0/src/viewport.rs:197`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<&IconData>`, `From<IconData>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Id` (struct) — `egui-0.35.0/src/id.rs:44`

egui tracks widgets frame-to-frame using [`Id`]s.

Methods:

- `fn accesskit_id(&self) -> NodeId` — `egui-0.35.0/src/id.rs:103`
- `fn new(source: impl AsId) -> Self` — `egui-0.35.0/src/id.rs:67`
  Generate a new root [`Id`] by hashing some source (e.g. a string or integer).
- `fn short_debug_format(&self) -> String` — `egui-0.35.0/src/id.rs:91`
  Short and readable summary
- `fn value(&self) -> u64` — `egui-0.35.0/src/id.rs:99`
  The inner value of the [`Id`].
- `fn with(self, salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/id.rs:77`
  Generate a child [`Id`] by salting the parent [`Id`] with the given argument.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `From<&'static str>`, `From<String>`, `From<ViewportId>`, `Hash`, `IsEnabled`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `IdSalt` (struct) — `egui-0.35.0/src/id_salt.rs:26`

Uniquely identifies a child widget within a parent widget.

Methods:

- `fn new(source: impl AsIdSalt) -> Self` — `egui-0.35.0/src/id_salt.rs:32`
  Create a new [`IdSalt`] by hashing some source (e.g. a string or integer).
- `fn value(&self) -> u64` — `egui-0.35.0/src/id_salt.rs:55`
  The inner value of the [`IdSalt`].

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `IsEnabled`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Image` (struct) — `egui-0.35.0/src/widgets/image.rs:51`

A widget which displays an image.

Methods:

- `fn alt_text(self, label: impl Into<String>) -> Self` — `egui-0.35.0/src/widgets/image.rs:272`
  Set alt text for the image. This will be shown when the image fails to load.
- `fn bg_fill(self, bg_fill: impl Into<Color32>) -> Self` — `egui-0.35.0/src/widgets/image.rs:216`
  A solid color to put behind the image. Useful for transparent images.
- `fn calc_size(&self, available_size: Vec2, image_source_size: Option<Vec2>) -> Vec2` — `egui-0.35.0/src/widgets/image.rs:287`
  Returns the size the image will occupy in the final UI.
- `fn corner_radius(self, corner_radius: impl Into<CornerRadius>) -> Self` — `egui-0.35.0/src/widgets/image.rs:251`
  Round the corners of the image.
- `fn fit_to_exact_size(self, size: Vec2) -> Self` — `egui-0.35.0/src/widgets/image.rs:176`
  Fit the image to an exact size.
- `fn fit_to_fraction(self, fraction: Vec2) -> Self` — `egui-0.35.0/src/widgets/image.rs:185`
  Fit the image to a fraction of the available space.
- `fn fit_to_original_size(self, scale: f32) -> Self` — `egui-0.35.0/src/widgets/image.rs:167`
  Fit the image to its original size with some scaling.
- `fn from_bytes(uri: impl Into<Cow<'static, str>>, bytes: impl Into<Bytes>) -> Self` — `egui-0.35.0/src/widgets/image.rs:110`
  Load the image from some raw bytes.
- `fn from_texture(texture: impl Into<SizedTexture>) -> Self` — `egui-0.35.0/src/widgets/image.rs:101`
  Load the image from an existing texture.
- `fn from_uri(uri: impl Into<Cow<'a, str>>) -> Self` — `egui-0.35.0/src/widgets/image.rs:94`
  Load the image from a URI.
- `fn image_options(&self) -> &ImageOptions` — `egui-0.35.0/src/widgets/image.rs:320`
- `fn load_and_calc_size(&self, ui: &Ui, available_size: Vec2) -> Option<Vec2>` — `egui-0.35.0/src/widgets/image.rs:292`
- `fn load_for_size(&self, ctx: &Context, available_size: Vec2) -> TextureLoadResult` — `egui-0.35.0/src/widgets/image.rs:349`
  Load the image from its [`Image::source`], returning the resulting [`SizedTexture`].
- `fn maintain_aspect_ratio(self, value: bool) -> Self` — `egui-0.35.0/src/widgets/image.rs:153`
  Whether or not the [`ImageFit`] should maintain the image's original aspect ratio.
- `fn max_height(self, height: f32) -> Self` — `egui-0.35.0/src/widgets/image.rs:137`
  Set the max height of the image.
- `fn max_size(self, size: Vec2) -> Self` — `egui-0.35.0/src/widgets/image.rs:146`
  Set the max size of the image.
- `fn max_width(self, width: f32) -> Self` — `egui-0.35.0/src/widgets/image.rs:128`
  Set the max width of the image.
- `fn new(source: impl Into<ImageSource<'a>>) -> Self` — `egui-0.35.0/src/widgets/image.rs:63`
  Load the image from some source.
- `fn paint_at(&self, ui: &Ui, rect: Rect)` — `egui-0.35.0/src/widgets/image.rs:368`
  Paint the image in the given rectangle.
- `fn rotate(self, angle: f32, origin: Vec2) -> Self` — `egui-0.35.0/src/widgets/image.rs:238`
  Rotate the image about an origin by some angle
- `fn sense(self, sense: Sense) -> Self` — `egui-0.35.0/src/widgets/image.rs:202`
  Make the image respond to clicks and/or drags.
- `fn show_loading_spinner(self, show: bool) -> Self` — `egui-0.35.0/src/widgets/image.rs:263`
  Show a spinner when the image is loading.
- `fn shrink_to_fit(self) -> Self` — `egui-0.35.0/src/widgets/image.rs:196`
  Fit the image to 100% of its available size, shrinking it if necessary.
- `fn size(&self) -> Option<Vec2>` — `egui-0.35.0/src/widgets/image.rs:298`
- `fn source(&'a self, ctx: &Context) -> ImageSource<'a>` — `egui-0.35.0/src/widgets/image.rs:325`
- `fn texture_options(self, texture_options: TextureOptions) -> Self` — `egui-0.35.0/src/widgets/image.rs:119`
  Texture options used when creating the texture.
- `fn tint(self, tint: impl Into<Color32>) -> Self` — `egui-0.35.0/src/widgets/image.rs:223`
  Multiply image color with this. Default is WHITE (no tint).
- `fn uri(&self) -> Option<&str>` — `egui-0.35.0/src/widgets/image.rs:309`
  Returns the URI of the image.
- `fn uv(self, uv: impl Into<Rect>) -> Self` — `egui-0.35.0/src/widgets/image.rs:209`
  Select UV range. Default is (0,0) in top-left, (1,1) bottom right.

Implements: `Clone`, `Debug`, `From<Image<'a>>`, `From<T>`, `Widget`

### `ImageOptions` (struct) — `egui-0.35.0/src/widgets/image.rs:797`

Public fields:

- `uv: Rect` — Select UV range. Default is (0,0) in top-left, (1,1) bottom right.
- `bg_fill: Color32` — A solid color to put behind the image. Useful for transparent images.
- `tint: Color32` — Multiply image color with this. Default is WHITE (no tint).
- `rotation: Option<(Rot2, Vec2)>` — Rotate the image about an origin by some angle
- `corner_radius: CornerRadius` — Round the corners of the image.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Serialize`

### `ImageSize` (struct) — `egui-0.35.0/src/widgets/image.rs:427`

This type determines the constraints on how the size of an image should be calculated.

Public fields:

- `maintain_aspect_ratio: bool` — Whether or not the final size should maintain the original aspect ratio.
- `max_size: Vec2` — Determines the maximum size of the image.
- `fit: ImageFit` — Determines how the image should shrink/expand/stretch/etc. to fit within its allocated sp…

Methods:

- `fn calc_size(&self, available_size: Vec2, image_source_size: Vec2) -> Vec2` — `egui-0.35.0/src/widgets/image.rs:515`
  Calculate the final on-screen size in points.
- `fn hint(&self, available_size: Vec2, pixels_per_point: f32) -> SizeHint` — `egui-0.35.0/src/widgets/image.rs:483`
  Size hint for e.g. rasterizing an svg.

Implements: `Clone`, `Copy`, `Debug`, `Default`

### `ImmediateViewport` (struct) — `egui-0.35.0/src/viewport.rs:1304`

Viewport for immediate rendering.

Public fields:

- `ids: ViewportIdPair` — Id of us and our parent.
- `builder: ViewportBuilder`
- `viewport_ui_cb: Box<dyn FnMut(&mut Ui) + 'a>` — The user-code that shows the GUI.

### `InnerResponse` (struct) — `egui-0.35.0/src/response.rs:1139`

Returned when we wrap some ui-code and want to return both the results of the inner function and the ui as a whole, e.g.:

Public fields:

- `inner: R` — What the user closure returned.
- `response: Response` — The response of the area.

Methods:

- `fn new(inner: R, response: Response) -> Self` — `egui-0.35.0/src/response.rs:1149`

Implements: `Debug`

### `InputOptions` (struct) — `egui-0.35.0/src/input_state/mod.rs:59`

Options for input state handling.

Public fields:

- `line_scroll_speed: f32` — Multiplier for the scroll speed when reported in [`crate::MouseWheelUnit::Line`]s.
- `scroll_zoom_speed: f32` — Controls the speed at which we zoom in when doing ctrl/cmd + scroll.
- `max_click_dist: f32` — After a pointer-down event, if the pointer moves more than this, it won't become a click.
- `max_click_duration: f64` — If the pointer is down for longer than this it will no longer register as a click.
- `max_double_click_delay: f64` — The new pointer press must come within this many seconds from previous pointer release fo…
- `zoom_modifier: Modifiers` — When this modifier is down, all scroll events are treated as zoom events.
- `horizontal_scroll_modifier: Modifiers` — When this modifier is down, all scroll events are treated as horizontal scrolls, and when…
- `vertical_scroll_modifier: Modifiers` — When this modifier is down, all scroll events are treated as vertical scrolls, and when c…
- `surrender_focus_on: SurrenderFocusOn` — When should we surrender focus from the focused widget?

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/input_state/mod.rs:128`
  Show the options in the ui.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `InputState` (struct) — `egui-0.35.0/src/input_state/mod.rs:215`

Input state that egui updates each frame.

Public fields:

- `raw: RawInput` — The raw input we got this frame from the backend.
- `pointer: PointerState` — State of the mouse or simple touch gestures which can be mapped to mouse operations.
- `smooth_scroll_delta: Vec2` — How many points the user scrolled, smoothed over a few frames.
- `pixels_per_point: f32` — Also known as device pixel ratio, > 1 for high resolution screens.
- `max_texture_side: usize` — Maximum size of one side of a texture.
- `time: f64` — Time in seconds. Relative to whatever. Used for animation.
- `unstable_dt: f32` — Time since last frame, in seconds.
- `predicted_dt: f32` — Estimated time until next frame (provided we repaint right away).
- `stable_dt: f32` — Time since last frame (in seconds), but gracefully handles the first frame after sleeping…
- `focused: bool` — The native window has the keyboard focus (i.e. is receiving key presses).
- `modifiers: Modifiers` — Which modifier keys are down at the start of the frame?
- `keys_down: HashSet<Key>` — The keys that are currently being held down.
- `events: Vec<Event>` — In-order events received this frame

Methods:

- `fn accesskit_action_requests(&self, id: Id, action: Action) -> impl Iterator<Item = &ActionRequest>` — `egui-0.35.0/src/input_state/mod.rs:858`
- `fn aim_radius(&self) -> f32` — `egui-0.35.0/src/input_state/mod.rs:799`
  How imprecise do we expect the mouse/touch input to be? Returns imprecision in points.
- `fn any_touches(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:837`
  True if there currently are any fingers touching egui.
- `fn begin_pass(self, new: RawInput, requested_immediate_repaint_prev_frame: bool, pixels_per_point: f32, options: InputOptions) -> Self` — `egui-0.35.0/src/input_state/mod.rs:367`
- `fn consume_accesskit_action_requests(&mut self, id: Id, consume: impl FnMut(&ActionRequest) -> bool)` — `egui-0.35.0/src/input_state/mod.rs:876`
- `fn consume_key(&mut self, modifiers: Modifiers, logical_key: Key) -> bool` — `egui-0.35.0/src/input_state/mod.rs:719`
  Check for a key press. If found, `true` is returned and the key pressed is consumed, so that this will only r…
- `fn consume_shortcut(&mut self, shortcut: &KeyboardShortcut) -> bool` — `egui-0.35.0/src/input_state/mod.rs:732`
  Check if the given shortcut has been pressed.
- `fn content_rect(&self) -> Rect` — `egui-0.35.0/src/input_state/mod.rs:507`
  Returns the region of the screen that is safe for content rendering
- `fn count_and_consume_key(&mut self, modifiers: Modifiers, logical_key: Key) -> usize` — `egui-0.35.0/src/input_state/mod.rs:688`
  Count presses of a key. If non-zero, the presses are consumed, so that this will only return non-zero once.
- `fn filtered_events(&self, filter: &EventFilter) -> Vec<Event>` — `egui-0.35.0/src/input_state/mod.rs:902`
  Get all events that matches the given filter.
- `fn has_accesskit_action_request(&self, id: Id, action: Action) -> bool` — `egui-0.35.0/src/input_state/mod.rs:893`
- `fn has_touch_screen(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:842`
  True if we have ever received a touch event.
- `fn is_scrolling(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:635`
  True if there is an active scroll action that might scroll more when using [`Self::smooth_scroll_delta`].
- `fn key_down(&self, desired_key: Key) -> bool` — `egui-0.35.0/src/input_state/mod.rs:766`
  Is the given key currently held down?
- `fn key_pressed(&self, desired_key: Key) -> bool` — `egui-0.35.0/src/input_state/mod.rs:743`
  Was the given key pressed this frame?
- `fn key_released(&self, desired_key: Key) -> bool` — `egui-0.35.0/src/input_state/mod.rs:771`
  Was the given key released this frame?
- `fn multi_touch(&self) -> Option<MultiTouchInfo>` — `egui-0.35.0/src/input_state/mod.rs:831`
  Returns details about the currently ongoing multi-touch gesture, if any. Note that this method returns `None`…
- `fn num_accesskit_action_requests(&self, id: Id, action: Action) -> usize` — `egui-0.35.0/src/input_state/mod.rs:897`
- `fn num_presses(&self, desired_key: Key) -> usize` — `egui-0.35.0/src/input_state/mod.rs:750`
  How many times was the given key pressed this frame?
- `fn physical_pixel_size(&self) -> f32` — `egui-0.35.0/src/input_state/mod.rs:792`
  Size of a physical pixel in logical gui coordinates (points).
- `fn pixels_per_point(&self) -> f32` — `egui-0.35.0/src/input_state/mod.rs:786`
  Also known as device pixel ratio, > 1 for high resolution screens.
- `fn rotation_delta(&self) -> f32` — `egui-0.35.0/src/input_state/mod.rs:614`
  Rotation in radians this frame, measuring clockwise (e.g. from a rotation gesture).
- `fn safe_area_insets(&self) -> SafeAreaInsets` — `egui-0.35.0/src/input_state/mod.rs:531`
  Get the safe area insets.
- `fn smooth_scroll_delta(&self) -> Vec2` — `egui-0.35.0/src/input_state/mod.rs:547`
  How many points the user scrolled, smoothed over a few frames.
- `fn time_since_last_scroll(&self) -> f32` — `egui-0.35.0/src/input_state/mod.rs:641`
  How long has it been (in seconds) since the last scroll event?
- `fn translation_delta(&self) -> Vec2` — `egui-0.35.0/src/input_state/mod.rs:627`
  Panning translation in pixels this frame (e.g. from scrolling or a pan gesture)
- `fn ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/input_state/mod.rs:1573`
- `fn viewport(&self) -> &ViewportInfo` — `egui-0.35.0/src/input_state/mod.rs:495`
  Info about the active viewport
- `fn viewport_rect(&self) -> Rect` — `egui-0.35.0/src/input_state/mod.rs:521`
  Returns the full area available to egui, including parts that might be partially covered, for example, by the…
- `fn zoom_delta(&self) -> f32` — `egui-0.35.0/src/input_state/mod.rs:559`
  Uniform zoom scale factor this frame (e.g. from ctrl-scroll or pinch gesture). * `zoom = 1`: no change * `zoo…
- `fn zoom_delta_2d(&self) -> Vec2` — `egui-0.35.0/src/input_state/mod.rs:582`
  2D non-proportional zoom scale factor this frame (e.g. from ctrl-scroll or pinch gesture).

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Serialize`

### `InteractOptions` (struct) — `egui-0.35.0/src/widget_rect.rs:76`

How to handle multiple calls to [`crate::Response::interact`] and [`crate::Ui::interact_opt`].

Public fields:

- `move_to_top: bool` — If we call interact on the same widget multiple times, should we move it to the top on su…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `IntoSizedArgs` (struct) — `egui-0.35.0/src/atomics/atom_kind.rs:7`

Args passed when sizing an [`super::Atom`]

Public fields:

- `available_size: Vec2`
- `wrap_mode: TextWrapMode`
- `fallback_font: FontSelection`

### `IntoSizedResult` (struct) — `egui-0.35.0/src/atomics/atom_kind.rs:14`

Result returned when sizing an [`super::Atom`]

Public fields:

- `intrinsic_size: Vec2`
- `sized: SizedAtomKind<'a>`

### `KeyboardShortcut` (struct) — `egui-0.35.0/src/data/input/keyboard_shortcut.rs:11`

A keyboard shortcut, e.g. `Ctrl+Alt+W`.

Public fields:

- `modifiers: Modifiers`
- `logical_key: Key`

Methods:

- `const fn new(modifiers: Modifiers, logical_key: Key) -> Self` — `egui-0.35.0/src/data/input/keyboard_shortcut.rs:18`
- `fn format(&self, names: &ModifierNames<'_>, is_mac: bool) -> String` — `egui-0.35.0/src/data/input/keyboard_shortcut.rs:25`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Label` (struct) — `egui-0.35.0/src/widgets/label.rs:25`

Static text.

Methods:

- `fn extend(self) -> Self` — `egui-0.35.0/src/widgets/label.rs:79`
  Set [`Self::wrap_mode`] to [`TextWrapMode::Extend`], disabling wrapping and truncating, and instead expanding…
- `fn halign(self, align: Align) -> Self` — `egui-0.35.0/src/widgets/label.rs:86`
  Sets the horizontal alignment of the Label to the given `Align` value.
- `fn layout_in_ui(self, ui: &mut Ui) -> (Pos2, Arc<Galley>, Response)` — `egui-0.35.0/src/widgets/label.rs:140`
  Do layout and position the galley in the ui, without painting it or adding widget info.
- `fn new(text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/widgets/label.rs:35`
- `fn selectable(self, selectable: bool) -> Self` — `egui-0.35.0/src/widgets/label.rs:95`
  Can the user select the text with the mouse?
- `fn sense(self, sense: Sense) -> Self` — `egui-0.35.0/src/widgets/label.rs:115`
  Make the label respond to clicks and/or drags.
- `fn show_tooltip_when_elided(self, show: bool) -> Self` — `egui-0.35.0/src/widgets/label.rs:132`
  Show the full text when hovered, if the text was elided.
- `fn text(&self) -> &str` — `egui-0.35.0/src/widgets/label.rs:46`
- `fn truncate(self) -> Self` — `egui-0.35.0/src/widgets/label.rs:71`
  Set [`Self::wrap_mode`] to [`TextWrapMode::Truncate`].
- `fn wrap(self) -> Self` — `egui-0.35.0/src/widgets/label.rs:63`
  Set [`Self::wrap_mode`] to [`TextWrapMode::Wrap`].
- `fn wrap_mode(self, wrap_mode: TextWrapMode) -> Self` — `egui-0.35.0/src/widgets/label.rs:56`
  Set the wrap mode for the text.

Implements: `Widget`

### `LayerId` (struct) — `egui-0.35.0/src/layers.rs:65`

An identifier for a paint layer. Also acts as an identifier for [`crate::Area`]:s.

Public fields:

- `order: Order`
- `id: Id`

Methods:

- `fn background() -> Self` — `egui-0.35.0/src/layers.rs:82`
- `fn debug() -> Self` — `egui-0.35.0/src/layers.rs:75`
- `fn new(order: Order, id: Id) -> Self` — `egui-0.35.0/src/layers.rs:71`
- `fn short_debug_format(&self) -> String` — `egui-0.35.0/src/layers.rs:90`
  Short and readable summary

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Layout` (struct) — `egui-0.35.0/src/layout.rs:102`

The layout of a [`Ui`][`crate::Ui`], e.g. "vertical & centered".

Public fields:

- `main_dir: Direction` — Main axis direction
- `main_wrap: bool` — If true, wrap around when reading the end of the main direction. For instance, for `main_…
- `main_align: Align` — How to align things on the main axis.
- `main_justify: bool` — Justify the main axis?
- `cross_align: Align` — How to align things on the cross axis. For vertical layouts: put things to left, center o…
- `cross_justify: bool` — Justify the cross axis? For vertical layouts justify mean all widgets get maximum width.…

Methods:

- `fn align_size_within_rect(&self, size: Vec2, outer: Rect) -> Rect` — `egui-0.35.0/src/layout.rs:374`
- `fn bottom_up(halign: Align) -> Self` — `egui-0.35.0/src/layout.rs:192`
  Place elements vertically, bottom up.
- `fn centered_and_justified(main_dir: Direction) -> Self` — `egui-0.35.0/src/layout.rs:220`
  For when you want to add a single widget to a layout, and that widget should use up all available space.
- `fn cross_align(&self) -> Align` — `egui-0.35.0/src/layout.rs:297`
- `fn cross_justify(&self) -> bool` — `egui-0.35.0/src/layout.rs:302`
- `fn from_main_dir_and_cross_align(main_dir: Direction, cross_align: Align) -> Self` — `egui-0.35.0/src/layout.rs:204`
- `fn horizontal_align(&self) -> Align` — `egui-0.35.0/src/layout.rs:333`
  e.g. for when aligning text within a button.
- `fn horizontal_justify(&self) -> bool` — `egui-0.35.0/src/layout.rs:355`
- `fn horizontal_placement(&self) -> Align` — `egui-0.35.0/src/layout.rs:324`
  e.g. for adjusting the placement of something. * in horizontal layout: left or right? * in vertical layout: s…
- `fn is_horizontal(&self) -> bool` — `egui-0.35.0/src/layout.rs:307`
- `fn is_vertical(&self) -> bool` — `egui-0.35.0/src/layout.rs:312`
- `fn left_to_right(valign: Align) -> Self` — `egui-0.35.0/src/layout.rs:141`
  Place elements horizontally, left to right.
- `fn main_dir(&self) -> Direction` — `egui-0.35.0/src/layout.rs:287`
- `fn main_wrap(&self) -> bool` — `egui-0.35.0/src/layout.rs:292`
- `fn prefer_right_to_left(&self) -> bool` — `egui-0.35.0/src/layout.rs:316`
- `fn right_to_left(valign: Align) -> Self` — `egui-0.35.0/src/layout.rs:156`
  Place elements horizontally, right to left.
- `fn top_down(halign: Align) -> Self` — `egui-0.35.0/src/layout.rs:171`
  Place elements vertically, top to bottom.
- `fn top_down_justified(halign: Align) -> Self` — `egui-0.35.0/src/layout.rs:184`
  Top-down layout justified so that buttons etc fill the full available width.
- `fn vertical_align(&self) -> Align` — `egui-0.35.0/src/layout.rs:342`
  e.g. for when aligning text within a button.
- `fn vertical_justify(&self) -> bool` — `egui-0.35.0/src/layout.rs:363`
- `fn with_cross_align(self, cross_align: Align) -> Self` — `egui-0.35.0/src/layout.rs:251`
  The alignment to use on the cross axis.
- `fn with_cross_justify(self, cross_justify: bool) -> Self` — `egui-0.35.0/src/layout.rs:276`
  Justify widgets along the cross axis?
- `fn with_main_align(self, main_align: Align) -> Self` — `egui-0.35.0/src/layout.rs:242`
  The alignment to use on the main axis.
- `fn with_main_justify(self, main_justify: bool) -> Self` — `egui-0.35.0/src/layout.rs:262`
  Justify widgets on the main axis?
- `fn with_main_wrap(self, main_wrap: bool) -> Self` — `egui-0.35.0/src/layout.rs:236`
  Wrap widgets when we overflow the main axis?

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `Link` (struct) — `egui-0.35.0/src/widgets/hyperlink.rs:27`

Clickable text, that looks like a hyperlink.

Methods:

- `fn new(text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/widgets/hyperlink.rs:32`

Implements: `Widget`

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

### `Memory` (struct) — `egui-0.35.0/src/memory/mod.rs:30`

The data that egui persists between frames.

Public fields:

- `options: Options` — Global egui options.
- `data: IdTypeMap` — This map stores some superficial state for all widgets with custom [`Id`]s.
- `caches: CacheStorage` — Can be used to cache computations from one frame to another.
- `to_global: HashMap<LayerId, TSTransform>` — Transforms per layer.

Methods:

- `fn allows_interaction(&self, layer_id: LayerId) -> bool` — `egui-0.35.0/src/memory/mod.rs:940`
  Does this layer allow interaction? Returns true if - the layer is not behind a modal layer - the [`Order`] al…
- `fn area_rect(&self, id: impl Into<Id>) -> Option<Rect>` — `egui-0.35.0/src/memory/mod.rs:999`
  Obtain the previous rectangle of an area.
- `fn areas(&self) -> &Areas` — `egui-0.35.0/src/memory/mod.rs:810`
  Access memory of the [`Area`](crate::containers::area::Area)s, such as `Window`s.
- `fn areas_mut(&mut self) -> &mut Areas` — `egui-0.35.0/src/memory/mod.rs:817`
  Access memory of the [`Area`](crate::containers::area::Area)s, such as `Window`s.
- `fn everything_is_visible(&self) -> bool` — `egui-0.35.0/src/memory/mod.rs:1132`
  If true, all windows, menus, tooltips, etc., will be visible at once.
- `fn focused(&self) -> Option<Id>` — `egui-0.35.0/src/memory/mod.rs:877`
  Which widget has keyboard focus?
- `fn had_focus_last_frame(&self, id: Id) -> bool` — `egui-0.35.0/src/memory/mod.rs:838`
  Check if the layer had focus last frame. returns `true` if the layer had focus last frame, but not this one.
- `fn has_focus(&self, id: Id) -> bool` — `egui-0.35.0/src/memory/mod.rs:872`
  Does this widget have keyboard focus?
- `fn interested_in_focus(&mut self, id: Id, layer_id: LayerId)` — `egui-0.35.0/src/memory/mod.rs:956`
  Register this widget as being interested in getting keyboard focus. This will allow the user to select it wit…
- `fn interrupt_ime(&mut self)` — `egui-0.35.0/src/memory/mod.rs:1035`
  Interrupt the current IME composition, if any.
- `fn is_above_modal_layer(&self, layer_id: LayerId) -> bool` — `egui-0.35.0/src/memory/mod.rs:925`
  Returns true if - this layer is the top-most modal layer or above it - there is no modal layer
- `fn layer_id_at(&self, pos: Pos2) -> Option<LayerId>` — `egui-0.35.0/src/memory/mod.rs:822`
  Top-most layer at the given position.
- `fn layer_ids(&self) -> impl ExactSizeIterator<Item = LayerId> + '_` — `egui-0.35.0/src/memory/mod.rs:832`
  An iterator over all layers. Back-to-front, top is last.
- `fn move_focus(&mut self, direction: FocusDirection)` — `egui-0.35.0/src/memory/mod.rs:918`
  Move keyboard focus in a specific direction.
- `fn owns_ime_events(&self, id: Id) -> bool` — `egui-0.35.0/src/memory/mod.rs:1026`
  Check if the widget owns IME events.
- `fn request_focus(&mut self, id: Id)` — `egui-0.35.0/src/memory/mod.rs:902`
  Give keyboard focus to a specific widget. See also [`crate::Response::request_focus`].
- `fn reset_areas(&mut self)` — `egui-0.35.0/src/memory/mod.rs:991`
  Forget window positions, sizes etc. Can be used to auto-layout windows.
- `fn set_everything_is_visible(&mut self, value: bool)` — `egui-0.35.0/src/memory/mod.rs:1141`
  If true, all windows, menus, tooltips etc are to be visible at once.
- `fn set_focus_lock_filter(&mut self, id: Id, event_filter: EventFilter)` — `egui-0.35.0/src/memory/mod.rs:887`
  Set an event filter for a widget.
- `fn set_modal_layer(&mut self, layer_id: LayerId)` — `egui-0.35.0/src/memory/mod.rs:965`
  Limit focus to widgets on the given layer and above. If this is called multiple times per frame, the top laye…
- `fn stop_text_input(&mut self)` — `egui-0.35.0/src/memory/mod.rs:985`
  Stop editing the active [`TextEdit`](crate::TextEdit) (if any).
- `fn surrender_focus(&mut self, id: Id)` — `egui-0.35.0/src/memory/mod.rs:910`
  Surrender keyboard focus for a specific widget. See also [`crate::Response::surrender_focus`].
- `fn top_modal_layer(&self) -> Option<LayerId>` — `egui-0.35.0/src/memory/mod.rs:979`
  Get the top modal layer (from the previous frame).

Implements: `Clone`, `Debug`, `Default`

### `MenuBar` (struct) — `egui-0.35.0/src/containers/menu.rs:217`

Horizontal menu bar where you can add [`MenuButton`]s.

Methods:

- `fn config(self, config: MenuConfig) -> Self` — `egui-0.35.0/src/containers/menu.rs:250`
  Set the config for submenus.
- `fn new() -> Self` — `egui-0.35.0/src/containers/menu.rs:232`
- `fn style(self, style: impl Into<StyleModifier>) -> Self` — `egui-0.35.0/src/containers/menu.rs:241`
  Set the style for buttons in the menu bar.
- `fn ui<R>(self, ui: &mut Ui, content: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/menu.rs:257`
  Show the menu bar.

Implements: `Clone`, `Debug`, `Default`

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

### `Modal` (struct) — `egui-0.35.0/src/containers/modal.rs:16`

A modal dialog.

Public fields:

- `area: Area`
- `backdrop_color: Color32`
- `frame: Option<Frame>`

Methods:

- `fn area(self, area: Area) -> Self` — `egui-0.35.0/src/containers/modal.rs:71`
  Set the area of the modal.
- `fn backdrop_color(self, color: Color32) -> Self` — `egui-0.35.0/src/containers/modal.rs:62`
  Set the backdrop color of the modal.
- `fn default_area(id: Id) -> Area` — `egui-0.35.0/src/containers/modal.rs:40`
  Returns an area customized for a modal.
- `fn frame(self, frame: Frame) -> Self` — `egui-0.35.0/src/containers/modal.rs:53`
  Set the frame of the modal.
- `fn new(id: Id) -> Self` — `egui-0.35.0/src/containers/modal.rs:26`
  Create a new Modal.
- `fn show<T>(self, ctx: &Context, content: impl FnOnce(&mut Ui) -> T) -> ModalResponse<T>` — `egui-0.35.0/src/containers/modal.rs:77`
  Show the modal.

### `ModalResponse` (struct) — `egui-0.35.0/src/containers/modal.rs:124`

The response of a modal dialog.

Public fields:

- `response: Response` — The response of the modal contents
- `backdrop_response: Response` — The response of the modal backdrop.
- `inner: T` — The inner response from the content closure
- `is_top_modal: bool` — Is this the topmost modal?
- `any_popup_open: bool` — Is there any popup open? We need to check this before the modal contents are shown, so we…

Methods:

- `fn should_close(&self) -> bool` — `egui-0.35.0/src/containers/modal.rs:151`
  Should the modal be closed? Returns true if: - the backdrop was clicked - this is the topmost modal, no popup…

### `ModifierNames` (struct) — `egui-0.35.0/src/data/input/modifier_names.rs:7`

Names of different modifier keys.

Public fields:

- `is_short: bool`
- `alt: &'a str`
- `ctrl: &'a str`
- `shift: &'a str`
- `mac_cmd: &'a str`
- `mac_alt: &'a str`
- `concat: &'a str` — What goes between the names

Methods:

- `fn format(&self, modifiers: &Modifiers, is_mac: bool) -> String` — `egui-0.35.0/src/data/input/modifier_names.rs:45`

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `Modifiers` (struct) — `egui-0.35.0/src/data/input/modifiers.rs:19`

State of the modifier keys. These must be fed to egui.

Public fields:

- `alt: bool` — Either of the alt keys are down (option ⌥ on Mac).
- `ctrl: bool` — Either of the control keys are down. When checking for keyboard shortcuts, consider using…
- `shift: bool` — Either of the shift keys are down.
- `mac_cmd: bool` — The Mac ⌘ Command key. Should always be set to `false` on other platforms.
- `command: bool` — On Windows and Linux, set this to the same value as `ctrl`. On Mac, this should be set wh…

Methods:

- `const fn plus(self, rhs: Self) -> Self` — `egui-0.35.0/src/data/input/modifiers.rs:139`
  ``` # use egui::Modifiers; assert_eq!( Modifiers::CTRL | Modifiers::ALT, Modifiers { ctrl: true, alt: true, .…
- `fn all(&self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:160`
- `fn any(&self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:155`
- `fn cmd_ctrl_matches(&self, pattern: Self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:300`
  Checks only cmd/ctrl, not alt/shift.
- `fn command_only(&self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:172`
  true if only [`Self::ctrl`] or only [`Self::mac_cmd`] is pressed.
- `fn contains(&self, query: Self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:345`
  Whether another set of modifiers is contained in this set of modifiers with proper handling of [`Self::comman…
- `fn is_none(&self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:150`
- `fn matches_any(&self, pattern: Self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:274`
  Check if any of the modifiers match exactly.
- `fn matches_exact(&self, pattern: Self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:253`
  Check for equality but with proper handling of [`Self::command`].
- `fn matches_logically(&self, pattern: Self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:211`
  Checks that the `ctrl/cmd` matches, and that the `shift/alt` of the argument is a subset of the pressed key (…
- `fn shift_only(&self) -> bool` — `egui-0.35.0/src/data/input/modifiers.rs:166`
  Is shift the only pressed button?
- `fn ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/data/input/modifiers.rs:407`

Implements: `BitOr`, `BitOrAssign`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `MultiTouchInfo` (struct) — `egui-0.35.0/src/input_state/touch_state.rs:11`

All you probably need to know about a multi-touch gesture.

Public fields:

- `start_time: f64` — Point in time when the gesture started.
- `start_pos: Pos2` — Position of the pointer at the time the gesture started.
- `center_pos: Pos2` — Center position of the current gesture (average of all touch points).
- `num_touches: usize` — Number of touches (fingers) on the surface. Value is ≥ 2 since for a single touch no [`Mu…
- `zoom_delta: f32` — Proportional zoom factor (pinch gesture). * `zoom = 1`: no change * `zoom < 1`: pinch tog…
- `zoom_delta_2d: Vec2` — 2D non-proportional zoom factor (pinch gesture).
- `rotation_delta: f32` — Rotation in radians. Moving fingers around each other will change this value. This is a r…
- `translation_delta: Vec2` — Relative movement (comparing previous frame and current frame) of the average position of…
- `force: f32` — Current force of the touch (average of the forces of the individual fingers). This is a v…

Implements: `Clone`, `Copy`, `Debug`, `PartialEq`, `StructuralPartialEq`

### `OpenUrl` (struct) — `egui-0.35.0/src/data/output.rs:237`

What URL to open, and how.

Public fields:

- `url: String`
- `new_tab: bool` — If `true`, open the url in a new tab. If `false` open it in the same tab. Only matters wh…

Methods:

- `fn new_tab(url: impl ToString) -> Self` — `egui-0.35.0/src/data/output.rs:256`
- `fn same_tab(url: impl ToString) -> Self` — `egui-0.35.0/src/data/output.rs:248`

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Options` (struct) — `egui-0.35.0/src/memory/mod.rs:193`

Some global options that you can read and write.

Public fields:

- `dark_style: Arc<Style>` — The default style for new [`Ui`](crate::Ui):s in dark mode.
- `light_style: Arc<Style>` — The default style for new [`Ui`](crate::Ui):s in light mode.
- `theme_preference: ThemePreference` — Preference for selection between dark and light [`crate::Context::global_style`] as the a…
- `fallback_theme: Theme` — Which theme to use in case [`Self::theme_preference`] is [`ThemePreference::System`] and…
- `zoom_factor: f32` — Global zoom factor of the UI.
- `zoom_with_keyboard: bool` — If `true`, egui will change the scale of the ui ([`crate::Context::zoom_factor`]) when th…
- `quit_shortcuts: Vec<KeyboardShortcut>` — Keyboard shortcuts to close the application.
- `tessellation_options: TessellationOptions` — Controls the tessellator.
- `repaint_on_widget_change: bool` — If any widget moves or changes id, repaint everything.
- `max_passes: NonZeroUsize` — Maximum number of passes to run in one frame.
- `screen_reader: bool` — This is a signal to any backend that we want the [`crate::PlatformOutput::events`] read o…
- `warn_on_id_clash: bool` — Check reusing of [`Id`]s, and show a visual warning on screen when one is found.
- `input_options: InputOptions` — Options related to input state handling.
- `reduce_texture_memory: bool` — If `true`, `egui` will discard the loaded image data after the texture is loaded onto the…

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/memory/mod.rs:375`
  Show the options in the ui.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

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

### `Painter` (struct) — `egui-0.35.0/src/painter.rs:21`

Helper to paint shapes and text to a specific region on a specific layer.

Methods:

- `fn add(&self, shape: impl Into<Shape>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:213`
  It is up to the caller to make sure there is room for this. Can be used for free painting. NOTE: all coordina…
- `fn arrow(&self, origin: Pos2, vec: Vec2, stroke: impl Into<Stroke>)` — `egui-0.35.0/src/painter.rs:417`
  Show an arrow starting at `origin` and going in the direction of `vec`, with the length `vec.length()`.
- `fn circle(&self, center: Pos2, radius: f32, fill_color: impl Into<Color32>, stroke: impl Into<Stroke>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:341`
- `fn circle_filled(&self, center: Pos2, radius: f32, fill_color: impl Into<Color32>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:356`
- `fn circle_stroke(&self, center: Pos2, radius: f32, stroke: impl Into<Stroke>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:370`
- `fn clip_rect(&self) -> Rect` — `egui-0.35.0/src/painter.rs:163`
  Everything painted in this [`Painter`] will be clipped against this. This means nothing outside of this recta…
- `fn ctx(&self) -> &Context` — `egui-0.35.0/src/painter.rs:128`
  Get a reference to the parent [`Context`].
- `fn debug_rect(&self, rect: Rect, color: Color32, text: impl ToString)` — `egui-0.35.0/src/painter.rs:266`
- `fn debug_text(&self, pos: Pos2, anchor: Align2, color: Color32, text: impl ToString) -> Rect` — `egui-0.35.0/src/painter.rs:292`
  Text with a background.
- `fn error(&self, pos: Pos2, text: impl Display) -> Rect` — `egui-0.35.0/src/painter.rs:283`
- `fn extend<I>(&self, shapes: I)` — `egui-0.35.0/src/painter.rs:226`
  Add many shapes at once.
- `fn fonts<R>(&self, reader: impl FnOnce(&FontsView<'_>) -> R) -> R` — `egui-0.35.0/src/painter.rs:142`
  Read-only access to the shared [`FontsView`].
- `fn fonts_mut<R>(&self, reader: impl FnOnce(&mut FontsView<'_>) -> R) -> R` — `egui-0.35.0/src/painter.rs:150`
  Read-write access to the shared [`FontsView`].
- `fn for_each_shape(&self, reader: impl FnMut(&ClippedShape))` — `egui-0.35.0/src/painter.rs:252`
  Access all shapes added this frame.
- `fn galley(&self, pos: Pos2, galley: Arc<Galley>, fallback_color: Color32)` — `egui-0.35.0/src/painter.rs:529`
  Paint text that has already been laid out in a [`Galley`].
- `fn galley_with_override_text_color(&self, pos: Pos2, galley: Arc<Galley>, text_color: Color32)` — `egui-0.35.0/src/painter.rs:541`
  Paint text that has already been laid out in a [`Galley`].
- `fn hline(&self, x: impl Into<Rangef>, y: f32, stroke: impl Into<Stroke>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:332`
  Paints a horizontal line.
- `fn image(&self, texture_id: TextureId, rect: Rect, uv: Rect, tint: Color32) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:447`
  An image at the given position.
- `fn is_visible(&self) -> bool` — `egui-0.35.0/src/painter.rs:117`
  If `false`, nothing you paint will show up.
- `fn layer_id(&self) -> LayerId` — `egui-0.35.0/src/painter.rs:156`
  Where we paint
- `fn layout(&self, text: String, font_id: FontId, color: Color32, wrap_width: f32) -> Arc<Galley>` — `egui-0.35.0/src/painter.rs:488`
  Will wrap text at the given width and line break at `\n`.
- `fn layout_job(&self, layout_job: LayoutJob) -> Arc<Galley>` — `egui-0.35.0/src/painter.rs:517`
  Lay out this text layut job in a galley.
- `fn layout_no_wrap(&self, text: String, font_id: FontId, color: Color32) -> Arc<Galley>` — `egui-0.35.0/src/painter.rs:503`
  Will line break at `\n`.
- `fn line(&self, points: Vec<Pos2>, stroke: impl Into<PathStroke>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:327`
  Paints a line connecting the points. NOTE: all coordinates are screen coordinates!
- `fn line_segment(&self, points: [Pos2; 2], stroke: impl Into<Stroke>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:318`
  Paints a line from the first point to the second.
- `fn multiply_opacity(&mut self, opacity: f32)` — `egui-0.35.0/src/painter.rs:100`
  Like [`Self::set_opacity`], but multiplies the given value with the current opacity.
- `fn new(ctx: Context, layer_id: LayerId, clip_rect: Rect) -> Self` — `egui-0.35.0/src/painter.rs:47`
  Create a painter to a specific layer within a certain clip rectangle.
- `fn opacity(&self) -> f32` — `egui-0.35.0/src/painter.rs:110`
  Read the current opacity of the underlying painter.
- `fn pixels_per_point(&self) -> f32` — `egui-0.35.0/src/painter.rs:134`
  Number of physical pixels for each logical UI point.
- `fn rect(&self, rect: Rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>, stroke: impl Into<Stroke>, stroke_kind: StrokeKind) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:380`
  See also [`Self::rect_filled`] and [`Self::rect_stroke`].
- `fn rect_filled(&self, rect: Rect, corner_radius: impl Into<CornerRadius>, fill_color: impl Into<Color32>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:397`
- `fn rect_stroke(&self, rect: Rect, corner_radius: impl Into<CornerRadius>, stroke: impl Into<Stroke>, stroke_kind: StrokeKind) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:406`
- `fn round_to_pixel_center(&self, point: f32) -> f32` — `egui-0.35.0/src/painter.rs:189`
  Useful for pixel-perfect rendering of lines that are one pixel wide (or any odd number of pixels).
- `fn set(&self, idx: ShapeIdx, shape: impl Into<Shape>)` — `egui-0.35.0/src/painter.rs:242`
  Modify an existing [`Shape`].
- `fn set_clip_rect(&mut self, clip_rect: Rect)` — `egui-0.35.0/src/painter.rs:183`
  Everything painted in this [`Painter`] will be clipped against this. This means nothing outside of this recta…
- `fn set_invisible(&mut self)` — `egui-0.35.0/src/painter.rs:122`
  If `false`, nothing added to the painter will be visible
- `fn set_layer_id(&mut self, layer_id: LayerId)` — `egui-0.35.0/src/painter.rs:81`
  Redirect where you are painting.
- `fn set_opacity(&mut self, opacity: f32)` — `egui-0.35.0/src/painter.rs:91`
  Set the opacity (alpha multiplier) of everything painted by this painter from this point forward.
- `fn shrink_clip_rect(&mut self, new_clip_rect: Rect)` — `egui-0.35.0/src/painter.rs:173`
  Constrain the rectangle in which we can paint.
- `fn text(&self, pos: Pos2, anchor: Align2, text: impl ToString, font_id: FontId, text_color: Color32) -> Rect` — `egui-0.35.0/src/painter.rs:469`
  Lay out and paint some text.
- `fn vline(&self, x: f32, y: impl Into<Rangef>, stroke: impl Into<Stroke>) -> ShapeIdx` — `egui-0.35.0/src/painter.rs:337`
  Paints a vertical line.
- `fn with_clip_rect(&self, rect: Rect) -> Self` — `egui-0.35.0/src/painter.rs:71`
  Create a painter for a sub-region of this [`Painter`].
- `fn with_layer_id(self, layer_id: LayerId) -> Self` — `egui-0.35.0/src/painter.rs:62`
  Redirect where you are painting.

Implements: `Clone`

### `Panel` (struct) — `egui-0.35.0/src/containers/panel.rs:180`

A panel that covers an entire side ([`left`](Panel::left), [`right`](Panel::right), [`top`](Panel::top) or [`bottom`](Panel::bottom)) of a [`Ui`] or screen.

Methods:

- `fn bottom(id: impl Into<Id>) -> Self` — `egui-0.35.0/src/containers/panel.rs:247`
  Create a bottom panel.
- `fn default_size(self, default_size: f32) -> Self` — `egui-0.35.0/src/containers/panel.rs:310`
  The initial wrapping width of the [`Panel`], including margins.
- `fn exact_size(self, size: f32) -> Self` — `egui-0.35.0/src/containers/panel.rs:346`
  Enforce this exact size, including margins.
- `fn frame(self, frame: Frame) -> Self` — `egui-0.35.0/src/containers/panel.rs:354`
  Change the background color, margins, etc.
- `fn left(id: impl Into<Id>) -> Self` — `egui-0.35.0/src/containers/panel.rs:222`
  Create a left panel.
- `fn max_size(self, max_size: f32) -> Self` — `egui-0.35.0/src/containers/panel.rs:328`
  Maximum size of the panel, including margins.
- `fn min_size(self, min_size: f32) -> Self` — `egui-0.35.0/src/containers/panel.rs:321`
  Minimum size of the panel, including margins.
- `fn resizable(self, resizable: bool) -> Self` — `egui-0.35.0/src/containers/panel.rs:294`
  Can panel be resized by dragging the edge of it?
- `fn right(id: impl Into<Id>) -> Self` — `egui-0.35.0/src/containers/panel.rs:229`
  Create a right panel.
- `fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/panel.rs:363`
  Show the panel inside a [`Ui`].
- `fn show_animated_between_inside<R>(ui: &mut Ui, is_expanded: bool, collapsed_panel: Self, expanded_panel: Self, add_contents: impl FnOnce(&mut Ui, f32) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/panel.rs:594`
  Renamed to [`Self::show_switched`].
  **DEPRECATED**: Renamed to `show_switched`
- `fn show_animated_inside<R>(self, ui: &mut Ui, is_expanded: bool, add_contents: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<R>>` — `egui-0.35.0/src/containers/panel.rs:434`
  Renamed to [`Self::show_collapsible`].
  **DEPRECATED**: Renamed to `show_collapsible`
- `fn show_collapsible<R>(self, ui: &mut Ui, is_expanded: &mut bool, add_contents: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<R>>` — `egui-0.35.0/src/containers/panel.rs:389`
  Show the panel if `*is_expanded` is `true`, otherwise hide it, with a slide animation in between.
- `fn show_inside<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/panel.rs:369`
  Renamed to [`Self::show`].
  **DEPRECATED**: Renamed to `show`
- `fn show_separator_line(self, show_separator_line: bool) -> Self` — `egui-0.35.0/src/containers/panel.rs:303`
  Show a separator line, even when not interacting with it?
- `fn show_switched<R>(ui: &mut Ui, is_expanded: &mut bool, collapsed_panel: Self, expanded_panel: Self, add_contents: impl FnOnce(&mut Ui, bool) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/panel.rs:500`
  Show either a collapsed or expanded panel, with a nice slide animation between.
- `fn size_range(self, size_range: impl Into<Rangef>) -> Self` — `egui-0.35.0/src/containers/panel.rs:335`
  The allowable size range for the panel, including margins.
- `fn top(id: impl Into<Id>) -> Self` — `egui-0.35.0/src/containers/panel.rs:238`
  Create a top panel.

### `PanelState` (struct) — `egui-0.35.0/src/containers/panel.rs:32`

State regarding panels.

Public fields:

- `outer_rect: Rect` — The _outer_ rect of the panel, i.e. including the [`Frame`] margin & border.

Methods:

- `fn load(ctx: &Context, bar_id: Id) -> Option<Self>` — `egui-0.35.0/src/containers/panel.rs:42`
- `fn size(&self) -> Vec2` — `egui-0.35.0/src/containers/panel.rs:48`
  The _outer_ size of the panel (from previous frame), i.e. including the [`Frame`] margin & border.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Serialize`

### `PlatformOutput` (struct) — `egui-0.35.0/src/data/output.rs:116`

The non-rendering part of what egui emits each frame.

Public fields:

- `commands: Vec<OutputCommand>` — Commands that the egui integration should execute at the end of a frame.
- `cursor_icon: CursorIcon` — Set the cursor to this icon.
- `cursor_image: Option<CustomCursorImage>` — If set, the integration should display this RGBA image as the OS cursor (via e.g. `winit:…
- `events: Vec<OutputEvent>` — Events that may be useful to e.g. a screen reader.
- `mutable_text_under_cursor: bool` — Is there a mutable [`TextEdit`](crate::TextEdit) under the cursor? Use by `eframe` web to…
- `ime: Option<IMEOutput>` — This is set if, and only if, the user is currently editing text.
- `accesskit_update: Option<TreeUpdate>` — The difference in the widget tree since last frame.
- `num_completed_passes: usize` — How many ui passes is this the sum of?
- `request_discard_reasons: Vec<RepaintCause>` — Was [`crate::Context::request_discard`] called during the latest pass?

Methods:

- `fn append(&mut self, newer: Self)` — `egui-0.35.0/src/data/output.rs:190`
  Add on new output.
- `fn events_description(&self) -> String` — `egui-0.35.0/src/data/output.rs:172`
  This can be used by a text-to-speech system to describe the events (if any).
- `fn requested_discard(&self) -> bool` — `egui-0.35.0/src/data/output.rs:227`
  Was [`crate::Context::request_discard`] called?
- `fn take(&mut self) -> Self` — `egui-0.35.0/src/data/output.rs:219`
  Take everything ephemeral (everything except `cursor_icon` and `cursor_image` currently)

Implements: `Clone`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `PointerState` (struct) — `egui-0.35.0/src/input_state/mod.rs:984`

Mouse or touch state.

Methods:

- `fn any_click(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1416`
  Were there any type of click this frame?
- `fn any_down(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1411`
  Is any pointer button currently down?
- `fn any_pressed(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1365`
  Was any pointer button pressed (`!down -> down`) this frame?
- `fn any_released(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1370`
  Was any pointer button released (`down -> !down`) this frame?
- `fn button_clicked(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1426`
  Was the given pointer button given clicked this frame?
- `fn button_double_clicked(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1433`
  Was the button given double clicked this frame?
- `fn button_down(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1478`
  Is this button currently down?
- `fn button_pressed(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1375`
  Was the button given pressed this frame?
- `fn button_released(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1382`
  Was the button given released this frame?
- `fn button_triple_clicked(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1446`
  Was the button given triple clicked this frame?
- `fn could_any_button_be_click(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1485`
  If the pointer button is down, will it register as a click when released?
- `fn delta(&self) -> Vec2` — `egui-0.35.0/src/input_state/mod.rs:1256`
  How much the pointer moved compared to last frame, in points.
- `fn direction(&self) -> Vec2` — `egui-0.35.0/src/input_state/mod.rs:1281`
  Current direction of the pointer.
- `fn has_pointer(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1331`
  Do we have a pointer?
- `fn hover_pos(&self) -> Option<Pos2>` — `egui-0.35.0/src/input_state/mod.rs:1313`
  If it is a good idea to show a tooltip, where is pointer?
- `fn interact_pos(&self) -> Option<Pos2>` — `egui-0.35.0/src/input_state/mod.rs:1323`
  If you detect a click or drag and wants to know where it happened, use this.
- `fn is_decidedly_dragging(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1512`
  Just because the mouse is down doesn't mean we are dragging. We could be at the start of a click. But if the…
- `fn is_moving(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1345`
  Is the pointer currently moving? This is smoothed so a few frames of stillness is required before this return…
- `fn is_moving_towards_rect(&self, rect: &Rect) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1557`
  Is the mouse moving in the direction of the given rect?
- `fn is_still(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1338`
  Is the pointer currently still? This is smoothed so a few frames of stillness is required before this returns…
- `fn latest_pos(&self) -> Option<Pos2>` — `egui-0.35.0/src/input_state/mod.rs:1307`
  Latest reported pointer position. When tapping a touch screen, this will be `None`.
- `fn middle_down(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1552`
  Is the middle button currently down?
- `fn motion(&self) -> Option<Vec2>` — `egui-0.35.0/src/input_state/mod.rs:1264`
  How much the mouse moved since the last frame, in unspecified units. Represents the actual movement of the mo…
- `fn press_origin(&self) -> Option<Pos2>` — `egui-0.35.0/src/input_state/mod.rs:1288`
  Where did the current click/drag originate? `None` if no mouse button is down.
- `fn press_start_time(&self) -> Option<f64>` — `egui-0.35.0/src/input_state/mod.rs:1300`
  When did the current click/drag originate? `None` if no mouse button is down.
- `fn primary_clicked(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1462`
  Was the primary button clicked this frame?
- `fn primary_down(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1536`
  Is the primary button currently down?
- `fn primary_pressed(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1389`
  Was the primary button pressed this frame?
- `fn primary_released(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1399`
  Was the primary button released this frame?
- `fn secondary_clicked(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1470`
  Was the secondary button clicked this frame?
- `fn secondary_down(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1544`
  Is the secondary button currently down?
- `fn secondary_pressed(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1394`
  Was the secondary button pressed this frame?
- `fn secondary_released(&self) -> bool` — `egui-0.35.0/src/input_state/mod.rs:1404`
  Was the secondary button released this frame?
- `fn time_since_last_click(&self) -> f32` — `egui-0.35.0/src/input_state/mod.rs:1357`
  How long has it been (in seconds) since the pointer was clicked?
- `fn time_since_last_movement(&self) -> f32` — `egui-0.35.0/src/input_state/mod.rs:1351`
  How long has it been (in seconds) since the pointer was last moved?
- `fn total_drag_delta(&self) -> Option<Vec2>` — `egui-0.35.0/src/input_state/mod.rs:1293`
  How far has the pointer moved since the start of the drag (if any)?
- `fn ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/input_state/mod.rs:1652`
- `fn velocity(&self) -> Vec2` — `egui-0.35.0/src/input_state/mod.rs:1273`
  Current velocity of pointer.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Serialize`

### `Popup` (struct) — `egui-0.35.0/src/containers/popup.rs:165`

A popup container.

Methods:

- `fn align(self, position_align: RectAlign) -> Self` — `egui-0.35.0/src/containers/popup.rs:278`
  Set the [`RectAlign`] of the popup relative to the [`PopupAnchor`]. This is the default position, and will be…
- `fn align_alternatives(self, alternatives: &'a [RectAlign]) -> Self` — `egui-0.35.0/src/containers/popup.rs:287`
  Set alternative positions to try if the default one doesn't fit. Set to an empty slice to always use the posi…
- `fn anchor(self, anchor: impl Into<PopupAnchor>) -> Self` — `egui-0.35.0/src/containers/popup.rs:353`
  Show the popup relative to the given [`PopupAnchor`].
- `fn at_pointer(self) -> Self` — `egui-0.35.0/src/containers/popup.rs:331`
  Show the popup relative to the pointer.
- `fn at_pointer_fixed(self) -> Self` — `egui-0.35.0/src/containers/popup.rs:339`
  Remember the pointer position at the time of opening the popup, and show the popup relative to that.
- `fn at_position(self, position: Pos2) -> Self` — `egui-0.35.0/src/containers/popup.rs:346`
  Show the popup relative to a specific position.
- `fn close_all(ctx: &Context)` — `egui-0.35.0/src/containers/popup.rs:677`
  Close all currently open popups.
- `fn close_behavior(self, close_behavior: PopupCloseBehavior) -> Self` — `egui-0.35.0/src/containers/popup.rs:324`
  Set the close behavior of the popup.
- `fn close_id(ctx: &Context, popup_id: Id)` — `egui-0.35.0/src/containers/popup.rs:684`
  Close the given popup, if it is open.
- `fn context_menu(response: &Response) -> Self` — `egui-0.35.0/src/containers/popup.rs:246`
  Show a context menu when the widget was secondary clicked. Sets the layout to `Layout::top_down_justified(Ali…
- `fn ctx(&self) -> &Context` — `egui-0.35.0/src/containers/popup.rs:412`
  Get the [`Context`]
- `fn default_response_id(response: &Response) -> Id` — `egui-0.35.0/src/containers/popup.rs:639`
  The default ID when constructing a popup from the [`Response`] of e.g. a button.
- `fn frame(self, frame: Frame) -> Self` — `egui-0.35.0/src/containers/popup.rs:367`
  Set the frame of the popup.
- `fn from_response(response: &Response) -> Self` — `egui-0.35.0/src/containers/popup.rs:215`
  Show a popup relative to some widget. The popup will be always open.
- `fn from_toggle_button_response(button_response: &Response) -> Self` — `egui-0.35.0/src/containers/popup.rs:228`
  Show a popup relative to some widget, toggling the open state based on the widget's click state.
- `fn gap(self, gap: f32) -> Self` — `egui-0.35.0/src/containers/popup.rs:360`
  Set the gap between the anchor and the popup.
- `fn get_anchor(&self) -> PopupAnchor` — `egui-0.35.0/src/containers/popup.rs:417`
  Return the [`PopupAnchor`] of the popup.
- `fn get_anchor_rect(&self) -> Option<Rect>` — `egui-0.35.0/src/containers/popup.rs:424`
  Return the anchor rect of the popup.
- `fn get_best_align(&self) -> RectAlign` — `egui-0.35.0/src/containers/popup.rs:463`
  Calculate the best alignment for the popup, based on the last size and screen rect.
- `fn get_expected_size(&self) -> Option<Vec2>` — `egui-0.35.0/src/containers/popup.rs:458`
  Get the expected size of the popup.
- `fn get_id(&self) -> Id` — `egui-0.35.0/src/containers/popup.rs:443`
  Get the id of the popup.
- `fn get_popup_rect(&self) -> Option<Rect>` — `egui-0.35.0/src/containers/popup.rs:432`
  Get the expected rect the popup will be shown in.
- `fn id(self, id: Id) -> Self` — `egui-0.35.0/src/containers/popup.rs:395`
  Set the id of the Area.
- `fn info(self, info: UiStackInfo) -> Self` — `egui-0.35.0/src/containers/popup.rs:269`
  Set the [`UiStackInfo`] of the popup's [`Ui`].
- `fn is_any_open(ctx: &Context) -> bool` — `egui-0.35.0/src/containers/popup.rs:660`
  Is any popup open?
- `fn is_id_open(ctx: &Context, popup_id: Id) -> bool` — `egui-0.35.0/src/containers/popup.rs:653`
  Is the given popup open?
- `fn is_open(&self) -> bool` — `egui-0.35.0/src/containers/popup.rs:448`
  Is the popup open?
- `fn kind(self, kind: PopupKind) -> Self` — `egui-0.35.0/src/containers/popup.rs:262`
  Set the kind of the popup. Used for [`Area::kind`] and [`Area::order`].
- `fn layout(self, layout: Layout) -> Self` — `egui-0.35.0/src/containers/popup.rs:381`
  Set the layout of the popup.
- `fn menu(button_response: &Response) -> Self` — `egui-0.35.0/src/containers/popup.rs:235`
  Show a popup when the widget was clicked. Sets the layout to `Layout::top_down_justified(Align::Min)`.
- `fn new(id: Id, ctx: Context, anchor: impl Into<PopupAnchor>, layer_id: LayerId) -> Self` — `egui-0.35.0/src/containers/popup.rs:190`
  Create a new popup
- `fn open(self, open: bool) -> Self` — `egui-0.35.0/src/containers/popup.rs:294`
  Force the popup to be open or closed.
- `fn open_bool(self, open: &'a mut bool) -> Self` — `egui-0.35.0/src/containers/popup.rs:315`
  Store the open state via a mutable bool.
- `fn open_id(ctx: &Context, popup_id: Id)` — `egui-0.35.0/src/containers/popup.rs:665`
  Open the given popup and close all others.
- `fn open_memory(self, set_state: impl Into<Option<SetOpenCommand>>) -> Self` — `egui-0.35.0/src/containers/popup.rs:306`
  Store the open state via [`crate::Memory`]. You can set the state via the first [`SetOpenCommand`] param.
- `fn position_of_id(ctx: &Context, popup_id: Id) -> Option<Pos2>` — `egui-0.35.0/src/containers/popup.rs:689`
  Get the position for this popup, if it is open.
- `fn sense(self, sense: Sense) -> Self` — `egui-0.35.0/src/containers/popup.rs:374`
  Set the sense of the popup.
- `fn show<R>(self, content: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<R>>` — `egui-0.35.0/src/containers/popup.rs:497`
  Show the popup.
- `fn style(self, style: impl Into<StyleModifier>) -> Self` — `egui-0.35.0/src/containers/popup.rs:406`
  Set the style for the popup contents.
- `fn toggle_id(ctx: &Context, popup_id: Id)` — `egui-0.35.0/src/containers/popup.rs:672`
  Toggle the given popup between closed and open.
- `fn width(self, width: f32) -> Self` — `egui-0.35.0/src/containers/popup.rs:388`
  The width that will be passed to [`Area::default_width`].

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

### `ProgressBar` (struct) — `egui-0.35.0/src/widgets/progress_bar.rs:15`

A simple progress bar.

Methods:

- `fn animate(self, animate: bool) -> Self` — `egui-0.35.0/src/widgets/progress_bar.rs:82`
  Whether to display a loading animation when progress `< 1`. Note that this will cause the UI to be redrawn. D…
- `fn corner_radius(self, corner_radius: impl Into<CornerRadius>) -> Self` — `egui-0.35.0/src/widgets/progress_bar.rs:93`
  Set the rounding of the progress bar.
- `fn desired_height(self, desired_height: f32) -> Self` — `egui-0.35.0/src/widgets/progress_bar.rs:48`
  The desired height of the bar. Will use the default interaction size if not set.
- `fn desired_width(self, desired_width: f32) -> Self` — `egui-0.35.0/src/widgets/progress_bar.rs:41`
  The desired width of the bar. Will use all horizontal space if not set.
- `fn fill(self, color: Color32) -> Self` — `egui-0.35.0/src/widgets/progress_bar.rs:55`
  The fill color of the bar.
- `fn new(progress: f32) -> Self` — `egui-0.35.0/src/widgets/progress_bar.rs:27`
  Progress in the `[0, 1]` range, where `1` means "completed".
- `fn show_percentage(self) -> Self` — `egui-0.35.0/src/widgets/progress_bar.rs:69`
  Show the progress in percent on the progress bar.
- `fn text(self, text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/widgets/progress_bar.rs:62`
  A custom text to display on the progress bar.

Implements: `Widget`

### `RadioButton` (struct) — `egui-0.35.0/src/widgets/radio_button.rs:26`

One out of several alternatives, either selected or not.

Methods:

- `fn atoms(&self) -> &Atoms<'a>` — `egui-0.35.0/src/widgets/radio_button.rs:42`
  Output the [`RadioButton`]'s [`Atoms`].
- `fn new(checked: bool, atoms: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/widgets/radio_button.rs:32`

Implements: `Widget`

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

### `RawInput` (struct) — `egui-0.35.0/src/data/input/raw_input.rs:18`

What the integrations provides to egui at the start of each frame.

Public fields:

- `viewport_id: ViewportId` — The id of the active viewport.
- `viewports: ViewportIdMap<ViewportInfo>` — Information about all egui viewports.
- `safe_area_insets: Option<SafeAreaInsets>` — The insets used to only render content in a mobile safe area
- `screen_rect: Option<Rect>` — Position and size of the area that egui should use, in points. Usually you would set this…
- `max_texture_side: Option<usize>` — Maximum size of one side of the font texture.
- `time: Option<f64>` — Monotonically increasing time, in seconds. Relative to whatever. Used for animations. If…
- `predicted_dt: f32` — Should be set to the expected time between frames when painting at vsync speeds. The defa…
- `modifiers: Modifiers` — Which modifier keys are down at the start of the frame?
- `events: Vec<Event>` — In-order events received this frame.
- `hovered_files: Vec<HoveredFile>` — Dragged files hovering over egui.
- `dropped_files: Vec<DroppedFile>` — Dragged files dropped into egui.
- `focused: bool` — The native window has the keyboard focus (i.e. is receiving key presses).
- `system_theme: Option<Theme>` — Does the OS use dark or light mode?

Methods:

- `fn append(&mut self, newer: Self)` — `egui-0.35.0/src/data/input/raw_input.rs:140`
  Add on new input.
- `fn take(&mut self) -> Self` — `egui-0.35.0/src/data/input/raw_input.rs:117`
  Helper: move volatile (deltas and events), clone the rest.
- `fn ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/data/input/raw_input.rs:174`
- `fn viewport(&self) -> &ViewportInfo` — `egui-0.35.0/src/data/input/raw_input.rs:109`
  Info about the active viewport

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

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

### `RepaintCause` (struct) — `egui-0.35.0/src/context.rs:250`

What called [`Context::request_repaint`] or [`Context::request_discard`]?

Public fields:

- `file: &'static str` — What file had the call that requested the repaint?
- `line: u32` — What line number of the call that requested the repaint?
- `reason: Cow<'static, str>` — Explicit reason; human readable.

Methods:

- `fn new() -> Self` — `egui-0.35.0/src/context.rs:277`
  Capture the file and line number of the call site.
- `fn new_reason(reason: impl Into<Cow<'static, str>>) -> Self` — `egui-0.35.0/src/context.rs:289`
  Capture the file and line number of the call site, as well as add a reason.

Implements: `Clone`, `Debug`, `Display`, `Eq`, `Hash`, `PartialEq`, `StructuralPartialEq`

### `RequestRepaintInfo` (struct) — `egui-0.35.0/src/context.rs:49`

Information given to the backend about when it is time to repaint the ui.

Public fields:

- `viewport_id: ViewportId` — This is used to specify what viewport that should repaint.
- `delay: Duration` — Repaint after this duration. If zero, repaint as soon as possible.
- `current_cumulative_pass_nr: u64` — The number of fully completed passes, of the entire lifetime of the [`Context`].

Implements: `Clone`, `Copy`, `Debug`

### `Resize` (struct) — `egui-0.35.0/src/containers/resize.rs:42`

A region that can be resized by dragging the bottom right corner.

Methods:

- `fn auto_sized(self) -> Self` — `egui-0.35.0/src/containers/resize.rs:177`
  Not manually resizable, just takes the size of its contents. Text will not wrap, but will instead make your w…
- `fn default_height(self, height: f32) -> Self` — `egui-0.35.0/src/containers/resize.rs:106`
  Preferred / suggested height. Actual height will depend on contents.
- `fn default_size(self, default_size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/resize.rs:112`
- `fn default_width(self, width: f32) -> Self` — `egui-0.35.0/src/containers/resize.rs:93`
  Preferred / suggested width. Actual width will depend on contents.
- `fn fixed_size(self, size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/resize.rs:184`
- `fn id(self, id: Id) -> Self` — `egui-0.35.0/src/containers/resize.rs:74`
  Assign an explicit and globally unique id.
- `fn id_salt(self, id_salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/containers/resize.rs:81`
  A source for the unique [`Id`], e.g. `.id_salt("second_resize_area")` or `.id_salt(loop_index)`.
- `fn is_resizable(&self) -> Vec2b` — `egui-0.35.0/src/containers/resize.rs:171`
- `fn max_height(self, max_height: f32) -> Self` — `egui-0.35.0/src/containers/resize.rs:154`
  Won't expand to larger than this
- `fn max_size(self, max_size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/resize.rs:140`
  Won't expand to larger than this
- `fn max_width(self, max_width: f32) -> Self` — `egui-0.35.0/src/containers/resize.rs:147`
  Won't expand to larger than this
- `fn min_height(self, min_height: f32) -> Self` — `egui-0.35.0/src/containers/resize.rs:133`
  Won't shrink to smaller than this
- `fn min_size(self, min_size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/resize.rs:119`
  Won't shrink to smaller than this
- `fn min_width(self, min_width: f32) -> Self` — `egui-0.35.0/src/containers/resize.rs:126`
  Won't shrink to smaller than this
- `fn resizable(self, resizable: impl Into<Vec2b>) -> Self` — `egui-0.35.0/src/containers/resize.rs:165`
  Can you resize it with the mouse?
- `fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R` — `egui-0.35.0/src/containers/resize.rs:323`
- `fn with_stroke(self, with_stroke: bool) -> Self` — `egui-0.35.0/src/containers/resize.rs:194`

Implements: `Clone`, `Copy`, `Debug`, `Default`

### `Response` (struct) — `egui-0.35.0/src/response.rs:23`

The result of adding a widget to a [`Ui`].

Public fields:

- `ctx: Context` — Used for optionally showing a tooltip and checking for more interactions.
- `layer_id: LayerId` — Which layer the widget is part of.
- `id: Id` — The [`Id`] of the widget/area this response pertains.
- `rect: Rect` — The area of the screen we are talking about.
- `interact_rect: Rect` — The rectangle sensing interaction.
- `sense: Sense` — The senses (click and/or drag) that the widget was interested in (if any).

Methods:

- `fn changed(&self) -> bool` — `egui-0.35.0/src/response.rs:593`
  Was the underlying data changed?
- `fn clicked(&self) -> bool` — `egui-0.35.0/src/response.rs:183`
  Returns true if this widget was clicked this frame by the primary button.
- `fn clicked_by(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/response.rs:196`
  Returns true if this widget was clicked this frame by the given mouse button.
- `fn clicked_elsewhere(&self) -> bool` — `egui-0.35.0/src/response.rs:274`
  `true` if there was a click *outside* the rect of this widget.
- `fn clicked_with_open_in_background(&self) -> bool` — `egui-0.35.0/src/response.rs:264`
  Was this widget middle-clicked or clicked while holding down a modifier key?
- `fn contains_pointer(&self) -> bool` — `egui-0.35.0/src/response.rs:326`
  Returns true if the pointer is contained by the response rect, and no other widget is covering it.
- `fn context_menu(&self, add_contents: impl FnOnce(&mut Ui)) -> Option<InnerResponse<()>>` — `egui-0.35.0/src/response.rs:1008`
  Response to secondary clicks (right-clicks) by showing the given menu.
- `fn context_menu_opened(&self) -> bool` — `egui-0.35.0/src/response.rs:1015`
  Returns whether a context menu is currently open for this widget.
- `fn dnd_hover_payload<Payload>(&self) -> Option<Arc<Payload>>` — `egui-0.35.0/src/response.rs:499`
  Drag-and-Drop: Return what is being held over this widget, if any.
- `fn dnd_release_payload<Payload>(&self) -> Option<Arc<Payload>>` — `egui-0.35.0/src/response.rs:515`
  Drag-and-Drop: Return what is being dropped onto this widget, if any.
- `fn dnd_set_drag_payload<Payload>(&self, payload: Payload)` — `egui-0.35.0/src/response.rs:482`
  If the user started dragging this widget this frame, store the payload for drag-and-drop.
- `fn double_clicked(&self) -> bool` — `egui-0.35.0/src/response.rs:236`
  Returns true if this widget was double-clicked this frame by the primary button.
- `fn double_clicked_by(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/response.rs:248`
  Returns true if this widget was double-clicked this frame by the given button.
- `fn drag_delta(&self) -> Vec2` — `egui-0.35.0/src/response.rs:439`
  If dragged, how many points were we dragged in since last frame?
- `fn drag_motion(&self) -> Vec2` — `egui-0.35.0/src/response.rs:471`
  If dragged, how far did the mouse move since last frame?
- `fn drag_started(&self) -> bool` — `egui-0.35.0/src/response.rs:386`
  Did a drag on this widget begin this frame?
- `fn drag_started_by(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/response.rs:397`
  Did a drag on this widget by the button begin this frame?
- `fn drag_stopped(&self) -> bool` — `egui-0.35.0/src/response.rs:428`
  The widget was being dragged, but now it has been released.
- `fn drag_stopped_by(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/response.rs:433`
  The widget was being dragged by the button, but now it has been released.
- `fn dragged(&self) -> bool` — `egui-0.35.0/src/response.rs:416`
  The widget is being dragged.
- `fn dragged_by(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/response.rs:422`
  See [`Self::dragged`].
- `fn enabled(&self) -> bool` — `egui-0.35.0/src/response.rs:304`
  Was the widget enabled? If false, there was no interaction attempted and the widget should be drawn in a gray…
- `fn gained_focus(&self) -> bool` — `egui-0.35.0/src/response.rs:347`
  True if this widget has keyboard focus this frame, but didn't last frame.
- `fn has_focus(&self) -> bool` — `egui-0.35.0/src/response.rs:342`
  This widget has the keyboard focus (i.e. is receiving key presses).
- `fn highlight(self) -> Self` — `egui-0.35.0/src/response.rs:723`
  Highlight this widget, to make it look like it is hovered, even if it isn't.
- `fn hover_pos(&self) -> Option<Pos2>` — `egui-0.35.0/src/response.rs:556`
  If it is a good idea to show a tooltip, where is pointer?
- `fn hovered(&self) -> bool` — `egui-0.35.0/src/response.rs:313`
  The pointer is hovering above this widget or the widget was clicked/tapped this frame.
- `fn interact(&self, sense: Sense) -> Self` — `egui-0.35.0/src/response.rs:783`
  Sense more interactions (e.g. sense clicks on a [`Response`] returned from a label).
- `fn interact_pointer_pos(&self) -> Option<Pos2>` — `egui-0.35.0/src/response.rs:529`
  Where the pointer (mouse/touch) were when this widget was clicked or dragged.
- `fn intrinsic_size(&self) -> Option<Vec2>` — `egui-0.35.0/src/response.rs:541`
  The intrinsic / desired size of the widget.
- `fn is_pointer_button_down_on(&self) -> bool` — `egui-0.35.0/src/response.rs:575`
  Is the pointer button currently down on this widget?
- `fn is_tooltip_open(&self) -> bool` — `egui-0.35.0/src/response.rs:684`
  Was the tooltip open last frame?
- `fn labelled_by(self, id: Id) -> Self` — `egui-0.35.0/src/response.rs:983`
  Associate a label with a control for accessibility.
- `fn long_touched(&self) -> bool` — `egui-0.35.0/src/response.rs:218`
  Was this long-pressed on a touch screen?
- `fn lost_focus(&self) -> bool` — `egui-0.35.0/src/response.rs:365`
  The widget had keyboard focus and lost it, either because the user pressed tab or clicked somewhere else, or…
- `fn mark_changed(&mut self)` — `egui-0.35.0/src/response.rs:605`
  Report the data shown by this widget changed.
- `fn middle_clicked(&self) -> bool` — `egui-0.35.0/src/response.rs:230`
  Returns true if this widget was clicked this frame by the middle mouse button.
- `fn on_disabled_hover_text(self, text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/response.rs:730`
  Show this text when hovering if the widget is disabled.
- `fn on_disabled_hover_ui(self, add_contents: impl FnOnce(&mut Ui)) -> Self` — `egui-0.35.0/src/response.rs:651`
  Show this UI when hovering if the widget is disabled.
- `fn on_hover_and_drag_cursor(self, cursor: CursorIcon) -> Self` — `egui-0.35.0/src/response.rs:751`
  When hovered or dragged, use this icon for the mouse cursor.
- `fn on_hover_cursor(self, cursor: CursorIcon) -> Self` — `egui-0.35.0/src/response.rs:742`
  When hovered, use this icon for the mouse cursor.
- `fn on_hover_text(self, text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/response.rs:707`
  Show this text if the widget was hovered (i.e. a tooltip).
- `fn on_hover_text_at_pointer(self, text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/response.rs:690`
  Like `on_hover_text`, but show the text next to cursor.
- `fn on_hover_ui(self, add_contents: impl FnOnce(&mut Ui)) -> Self` — `egui-0.35.0/src/response.rs:645`
  Show this UI if the widget was hovered (i.e. a tooltip).
- `fn on_hover_ui_at_pointer(self, add_contents: impl FnOnce(&mut Ui)) -> Self` — `egui-0.35.0/src/response.rs:657`
  Like `on_hover_ui`, but show the ui next to cursor.
- `fn output_event(&self, event: OutputEvent)` — `egui-0.35.0/src/response.rs:877`
- `fn paint_debug_info(&self)` — `egui-0.35.0/src/response.rs:1029`
  Draw a debug rectangle over the response displaying the response's id and whether it is enabled and/or hovere…
- `fn parent_id(&self) -> Id` — `egui-0.35.0/src/response.rs:157`
  The [`Id`] of the parent [`crate::Ui`] that hosts this widget.
- `fn request_focus(&self)` — `egui-0.35.0/src/response.rs:370`
  Request that this widget get keyboard focus.
- `fn scroll_to_me(&self, align: Option<Align>)` — `egui-0.35.0/src/response.rs:822`
  Adjust the scroll position until this UI becomes visible.
- `fn scroll_to_me_animation(&self, align: Option<Align>, animation: ScrollAnimation)` — `egui-0.35.0/src/response.rs:827`
  Like [`Self::scroll_to_me`], but allows you to specify the [`crate::style::ScrollAnimation`].
- `fn secondary_clicked(&self) -> bool` — `egui-0.35.0/src/response.rs:210`
  Returns true if this widget was clicked this frame by the secondary mouse button (e.g. the right mouse button…
- `fn set_close(&mut self)` — `egui-0.35.0/src/response.rs:620`
  Set the [`Flags::CLOSE`] flag.
- `fn set_intrinsic_size(&mut self, size: Vec2)` — `egui-0.35.0/src/response.rs:548`
  Set the intrinsic / desired size of the widget.
- `fn should_close(&self) -> bool` — `egui-0.35.0/src/response.rs:613`
  Should the container be closed?
- `fn show_tooltip_text(&self, text: impl Into<WidgetText>)` — `egui-0.35.0/src/response.rs:677`
  Always show this tooltip, even if disabled and the user isn't hovering it.
- `fn show_tooltip_ui(&self, add_contents: impl FnOnce(&mut Ui))` — `egui-0.35.0/src/response.rs:668`
  Always show this tooltip, even if disabled and the user isn't hovering it.
- `fn surrender_focus(&self)` — `egui-0.35.0/src/response.rs:375`
  Surrender keyboard focus for this widget.
- `fn total_drag_delta(&self) -> Option<Vec2>` — `egui-0.35.0/src/response.rs:453`
  If dragged, how many points have we been dragged since the start of the drag?
- `fn triple_clicked(&self) -> bool` — `egui-0.35.0/src/response.rs:242`
  Returns true if this widget was triple-clicked this frame by the primary button.
- `fn triple_clicked_by(&self, button: PointerButton) -> bool` — `egui-0.35.0/src/response.rs:255`
  Returns true if this widget was triple-clicked this frame by the given button.
- `fn union(&self, other: Self) -> Self` — `egui-0.35.0/src/response.rs:1051`
  A logical "or" operation. For instance `a.union(b).hovered` means "was either a or b hovered?".
- `fn widget_info(&self, make_info: impl Fn() -> WidgetInfo)` — `egui-0.35.0/src/response.rs:849`
  For accessibility.
- `fn widget_state(&self) -> WidgetState` — `egui-0.35.0/src/widget_style.rs:105`
- `fn with_new_rect(self, rect: Rect) -> Self` — `egui-0.35.0/src/response.rs:1079`
  Returns a response with a modified [`Self::rect`].

Implements: `BitOr`, `BitOrAssign`, `Clone`, `Debug`, `From<&Response>`

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

### `RichText` (struct) — `egui-0.35.0/src/widget_text.rs:26`

Text and optional style choices for it.

Methods:

- `fn append_to(self, layout_job: &mut LayoutJob, style: &Style, fallback_font: FontSelection, default_valign: Align)` — `egui-0.35.0/src/widget_text.rs:372`
  Append to an existing [`LayoutJob`]
- `fn background_color(self, background_color: impl Into<Color32>) -> Self` — `egui-0.35.0/src/widget_text.rs:310`
  Fill-color behind the text.
- `fn code(self) -> Self` — `egui-0.35.0/src/widget_text.rs:245`
  Monospace label with different background color.
- `fn color(self, color: impl Into<Color32>) -> Self` — `egui-0.35.0/src/widget_text.rs:320`
  Override text color.
- `fn extra_letter_spacing(self, extra_letter_spacing: f32) -> Self` — `egui-0.35.0/src/widget_text.rs:160`
  Extra spacing between letters, in points.
- `fn fallback_text_style(self, text_style: TextStyle) -> Self` — `egui-0.35.0/src/widget_text.rs:226`
  Set the [`TextStyle`] unless it has already been set
- `fn family(self, family: FontFamily) -> Self` — `egui-0.35.0/src/widget_text.rs:185`
  Select the font family.
- `fn font(self, font_id: FontId) -> Self` — `egui-0.35.0/src/widget_text.rs:193`
  Select the font and size. This overrides the value from [`Self::text_style`].
- `fn font_height(&self, fonts: &mut FontsView<'_>, style: &Style) -> f32` — `egui-0.35.0/src/widget_text.rs:328`
  Read the font height of the selected text style.
- `fn heading(self) -> Self` — `egui-0.35.0/src/widget_text.rs:233`
  Use [`TextStyle::Heading`].
- `fn is_empty(&self) -> bool` — `egui-0.35.0/src/widget_text.rs:136`
- `fn italics(self) -> Self` — `egui-0.35.0/src/widget_text.rs:284`
  Tilt the characters to the right.
- `fn line_height(self, line_height: Option<f32>) -> Self` — `egui-0.35.0/src/widget_text.rs:174`
  Explicit line height of the text in points.
- `fn monospace(self) -> Self` — `egui-0.35.0/src/widget_text.rs:239`
  Use [`TextStyle::Monospace`].
- `fn new(text: impl Into<String>) -> Self` — `egui-0.35.0/src/widget_text.rs:128`
- `fn raised(self) -> Self` — `egui-0.35.0/src/widget_text.rs:303`
  Align text to top. Only applicable together with [`Self::small()`].
- `fn size(self, size: f32) -> Self` — `egui-0.35.0/src/widget_text.rs:148`
  Select the font size (in points). This overrides the value from [`Self::text_style`].
- `fn small(self) -> Self` — `egui-0.35.0/src/widget_text.rs:291`
  Smaller text.
- `fn small_raised(self) -> Self` — `egui-0.35.0/src/widget_text.rs:297`
  For e.g. exponents.
- `fn strikethrough(self) -> Self` — `egui-0.35.0/src/widget_text.rs:277`
  Draw a line through the text, crossing it out.
- `fn strong(self) -> Self` — `egui-0.35.0/src/widget_text.rs:252`
  Extra strong text (stronger color).
- `fn text(&self) -> &str` — `egui-0.35.0/src/widget_text.rs:141`
- `fn text_style(self, text_style: TextStyle) -> Self` — `egui-0.35.0/src/widget_text.rs:219`
  Override the [`TextStyle`].
- `fn underline(self) -> Self` — `egui-0.35.0/src/widget_text.rs:268`
  Draw a line under the text.
- `fn variation(self, tag: impl IntoTag, coord: f32) -> Self` — `egui-0.35.0/src/widget_text.rs:202`
  Add a variation coordinate.
- `fn variations<T>(self, variations: impl IntoIterator<Item = (T, f32)>) -> Self` — `egui-0.35.0/src/widget_text.rs:209`
  Override the variation coordinates completely.
- `fn weak(self) -> Self` — `egui-0.35.0/src/widget_text.rs:259`
  Extra weak text (fainter color).

Implements: `Clone`, `Debug`, `Default`, `From<&Box<str>>`, `From<&String>`, `From<&mut Box<str>>`, `From<&mut String>`, `From<&str>`, `From<Box<str>>`, `From<Cow<'_, str>>`, `From<RichText>`, `From<String>`, `PartialEq`, `StructuralPartialEq`

### `SafeAreaInsets` (struct) — `egui-0.35.0/src/data/input/safe_area_insets.rs:11`

The 'safe area' insets of the screen

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`, `Sub<SafeAreaInsets>`

### `Scene` (struct) — `egui-0.35.0/src/containers/scene.rs:46`

A container that allows you to zoom and pan.

Methods:

- `fn drag_pan_buttons(self, flags: DragPanButtons) -> Self` — `egui-0.35.0/src/containers/scene.rs:128`
  Specify which pointer buttons can be used to pan by clicking and dragging.
- `fn max_inner_size(self, max_inner_size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/scene.rs:119`
  Set the maximum size of the inner [`Ui`] that will be created.
- `fn new() -> Self` — `egui-0.35.0/src/containers/scene.rs:89`
- `fn register_pan_and_zoom(&self, ui: &Ui, resp: &mut Response, to_global: &mut TSTransform)` — `egui-0.35.0/src/containers/scene.rs:229`
  Helper function to handle pan and zoom interactions on a response.
- `fn sense(self, sense: Sense) -> Self` — `egui-0.35.0/src/containers/scene.rs:99`
  Specify what type of input the scene should respond to.
- `fn show<R>(&self, parent_ui: &mut Ui, scene_rect: &mut Rect, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/containers/scene.rs:140`
  `scene_rect` contains the view bounds of the inner [`Ui`].
- `fn zoom_range(self, zoom_range: impl Into<Rangef>) -> Self` — `egui-0.35.0/src/containers/scene.rs:112`
  Set the allowed zoom range.

Implements: `Clone`, `Debug`, `Default`

### `ScrollArea` (struct) — `egui-0.35.0/src/containers/scroll_area.rs:338`

Add vertical and/or horizontal scrolling to a contained [`Ui`].

Methods:

- `fn animated(self, animated: bool) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:614`
  Should the scroll area animate `scroll_to_*` functions?
- `fn auto_shrink(self, auto_shrink: impl Into<Vec2b>) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:605`
  For each axis, should the containing area shrink if the content is small?
- `fn both() -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:381`
  Create a bi-directional (horizontal and vertical) scroll area.
- `fn content_margin(self, margin: impl Into<Margin>) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:631`
  Extra margin added around the contents.
- `fn horizontal() -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:369`
  Create a horizontal scroll area.
- `fn horizontal_scroll_offset(self, offset: f32) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:520`
  Set the horizontal scroll offset position.
- `fn hscroll(self, hscroll: bool) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:551`
  Turn on/off scrolling on the horizontal axis.
- `fn id_salt(self, id_salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:482`
  A source for the unique [`Id`], e.g. `.id_salt("second_scroll_area")` or `.id_salt(loop_index)`.
- `fn max_height(self, max_height: f32) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:432`
  The maximum height of the outer frame of the scroll area.
- `fn max_width(self, max_width: f32) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:421`
  The maximum width of the outer frame of the scroll area.
- `fn min_scrolled_height(self, min_scrolled_height: f32) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:456`
  The minimum height of a vertical scroll area which requires scroll bars.
- `fn min_scrolled_width(self, min_scrolled_width: f32) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:444`
  The minimum width of a horizontal scroll area which requires scroll bars.
- `fn neither() -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:388`
  Create a scroll area where both direction of scrolling is disabled. It's unclear why you would want to do thi…
- `fn new(direction_enabled: impl Into<Vec2b>) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:394`
  Create a scroll area where you decide which axis has scrolling enabled. For instance, `ScrollArea::new([true,…
- `fn on_drag_cursor(self, cursor: CursorIcon) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:544`
  Set the cursor used when the [`ScrollArea`] is being dragged.
- `fn on_hover_cursor(self, cursor: CursorIcon) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:532`
  Set the cursor used when the mouse pointer is hovering over the [`ScrollArea`].
- `fn scroll(self, direction_enabled: impl Into<Vec2b>) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:567`
  Turn on/off scrolling on the horizontal/vertical axes.
- `fn scroll_bar_rect(self, scroll_bar_rect: Rect) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:475`
  Specify within which screen-space rectangle to show the scroll bars.
- `fn scroll_bar_visibility(self, scroll_bar_visibility: ScrollBarVisibility) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:465`
  Set the visibility of both horizontal and vertical scroll bars.
- `fn scroll_offset(self, offset: Vec2) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:495`
  Set the horizontal and vertical scroll offset position.
- `fn scroll_source(self, scroll_source: ScrollSource) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:582`
  Control the scrolling behavior.
- `fn show<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> ScrollAreaOutput<R>` — `egui-0.35.0/src/containers/scroll_area.rs:960`
  Show the [`ScrollArea`], and add the contents to the viewport.
- `fn show_rows<R>(self, ui: &mut Ui, row_height_sans_spacing: f32, total_rows: usize, add_contents: impl FnOnce(&mut Ui, Range<usize>) -> R) -> ScrollAreaOutput<R>` — `egui-0.35.0/src/containers/scroll_area.rs:984`
  Efficiently show only the visible part of a large number of rows.
- `fn show_viewport<R>(self, ui: &mut Ui, add_contents: impl FnOnce(&mut Ui, Rect) -> R) -> ScrollAreaOutput<R>` — `egui-0.35.0/src/containers/scroll_area.rs:1021`
  This can be used to only paint the visible part of the contents.
- `fn stick_to_bottom(self, stick: bool) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:655`
  The scroll handle will stick to the bottom position even while the content size changes dynamically. This can…
- `fn stick_to_right(self, stick: bool) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:643`
  The scroll handle will stick to the rightmost position even while the content size changes dynamically. This…
- `fn vertical() -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:375`
  Create a vertical scroll area.
- `fn vertical_scroll_offset(self, offset: f32) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:508`
  Set the vertical scroll offset position.
- `fn vscroll(self, vscroll: bool) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:558`
  Turn on/off scrolling on the vertical axis.
- `fn wheel_scroll_multiplier(self, multiplier: Vec2) -> Self` — `egui-0.35.0/src/containers/scroll_area.rs:593`
  The scroll amount caused by a mouse wheel scroll is multiplied by this amount.

Implements: `Clone`, `Debug`

### `Sense` (struct) — `egui-0.35.0/src/sense.rs:4`

What sort of interaction is a widget sensitive to?

Methods:

- `const fn all() -> Self` — `egui-0.35.0/src/sense.rs:6`
  Get a flags value with all known bits set.
- `const fn bits(&self) -> u8` — `egui-0.35.0/src/sense.rs:6`
  Get the underlying bits value.
- `const fn complement(self) -> Self` — `egui-0.35.0/src/sense.rs:6`
  The bitwise negation (`!`) of the bits in `self`, truncating the result.
- `const fn contains(&self, other: Self) -> bool` — `egui-0.35.0/src/sense.rs:6`
  Whether all set bits in `other` are also set in `self`.
- `const fn difference(self, other: Self) -> Self` — `egui-0.35.0/src/sense.rs:6`
  The intersection of `self` with the complement of `other` (`&!`).
- `const fn empty() -> Self` — `egui-0.35.0/src/sense.rs:6`
  Get a flags value with all bits unset.
- `const fn from_bits(bits: u8) -> Option<Self>` — `egui-0.35.0/src/sense.rs:6`
  Convert from a bits value.
- `const fn from_bits_retain(bits: u8) -> Self` — `egui-0.35.0/src/sense.rs:6`
  Convert from a bits value exactly.
- `const fn from_bits_truncate(bits: u8) -> Self` — `egui-0.35.0/src/sense.rs:6`
  Convert from a bits value, unsetting any unknown bits.
- `const fn intersection(self, other: Self) -> Self` — `egui-0.35.0/src/sense.rs:6`
  The bitwise and (`&`) of the bits in `self` and `other`.
- `const fn intersects(&self, other: Self) -> bool` — `egui-0.35.0/src/sense.rs:6`
  Whether any set bits in `other` are also set in `self`.
- `const fn is_all(&self) -> bool` — `egui-0.35.0/src/sense.rs:6`
  Whether all known bits in this flags value are set.
- `const fn is_empty(&self) -> bool` — `egui-0.35.0/src/sense.rs:6`
  Whether all bits in `self` are unset.
- `const fn iter(&self) -> Iter<Sense>` — `egui-0.35.0/src/sense.rs:6`
  Yield a set of contained flags values.
- `const fn iter_names(&self) -> IterNames<Sense>` — `egui-0.35.0/src/sense.rs:6`
  Yield a set of contained named flags values.
- `const fn symmetric_difference(self, other: Self) -> Self` — `egui-0.35.0/src/sense.rs:6`
  The bitwise exclusive-or (`^`) of the bits in `self` and `other`.
- `const fn union(self, other: Self) -> Self` — `egui-0.35.0/src/sense.rs:6`
  The bitwise or (`|`) of the bits in `self` and `other`.
- `fn click() -> Self` — `egui-0.35.0/src/sense.rs:60`
  Sense clicks and hover, but not drags, and make the widget focusable.
- `fn click_and_drag() -> Self` — `egui-0.35.0/src/sense.rs:81`
  Sense both clicks, drags and hover (e.g. a slider or window), and make the widget focusable.
- `fn drag() -> Self` — `egui-0.35.0/src/sense.rs:68`
  Sense drags and hover, but not clicks. Make the widget focusable.
- `fn focusable_noninteractive() -> Self` — `egui-0.35.0/src/sense.rs:52`
  Senses no clicks or drags, but can be focused with the keyboard. Used for labels that can be focused for the…
- `fn from_name(name: &str) -> Option<Self>` — `egui-0.35.0/src/sense.rs:6`
  Get a flags value with the bits of a flag with the given name set.
- `fn hover() -> Self` — `egui-0.35.0/src/sense.rs:45`
  Senses no clicks or drags. Only senses mouse hover.
- `fn insert(&mut self, other: Self)` — `egui-0.35.0/src/sense.rs:6`
  The bitwise or (`|`) of the bits in `self` and `other`.
- `fn interactive(&self) -> bool` — `egui-0.35.0/src/sense.rs:87`
  Returns true if we sense either clicks or drags.
- `fn is_focusable(&self) -> bool` — `egui-0.35.0/src/sense.rs:102`
- `fn remove(&mut self, other: Self)` — `egui-0.35.0/src/sense.rs:6`
  The intersection of `self` with the complement of `other` (`&!`).
- `fn senses_click(&self) -> bool` — `egui-0.35.0/src/sense.rs:92`
- `fn senses_drag(&self) -> bool` — `egui-0.35.0/src/sense.rs:97`
- `fn set(&mut self, other: Self, value: bool)` — `egui-0.35.0/src/sense.rs:6`
  Call `insert` when `value` is `true` or `remove` when `value` is `false`.
- `fn toggle(&mut self, other: Self)` — `egui-0.35.0/src/sense.rs:6`
  The bitwise exclusive-or (`^`) of the bits in `self` and `other`.

Implements: `Binary`, `BitAnd`, `BitAndAssign`, `BitOr`, `BitOrAssign`, `BitXor`, `BitXorAssign`, `Clone`, `Copy`, `Debug`, `Eq`, `Extend<Sense>`, `Flags`, `FromIterator<Sense>`, `IntoIterator`, `LowerHex`, `Not`, `Octal`, `PartialEq`, `StructuralPartialEq`, `Sub`, `SubAssign`, `UpperHex`

### `Separator` (struct) — `egui-0.35.0/src/widgets/separator.rs:18`

A visual separator. A horizontal or vertical line (depending on [`crate::Layout`]).

Methods:

- `fn grow(self, extra: f32) -> Self` — `egui-0.35.0/src/widgets/separator.rs:76`
  Extend each end of the separator line by this much.
- `fn horizontal(self) -> Self` — `egui-0.35.0/src/widgets/separator.rs:55`
  Explicitly ask for a horizontal line.
- `fn shrink(self, shrink: f32) -> Self` — `egui-0.35.0/src/widgets/separator.rs:87`
  Contract each end of the separator line by this much.
- `fn spacing(self, spacing: f32) -> Self` — `egui-0.35.0/src/widgets/separator.rs:45`
  How much space we take up. The line is painted in the middle of this.
- `fn vertical(self) -> Self` — `egui-0.35.0/src/widgets/separator.rs:65`
  Explicitly ask for a vertical line.

Implements: `Default`, `HasClasses`, `Widget`

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

### `Sides` (struct) — `egui-0.35.0/src/containers/sides.rs:45`

Put some widgets on the left and right sides of a ui.

Methods:

- `fn extend(self) -> Self` — `egui-0.35.0/src/containers/sides.rs:111`
  Extend the left and right sides to fill the available space.
- `fn height(self, height: f32) -> Self` — `egui-0.35.0/src/containers/sides.rs:71`
  The minimum height of the sides.
- `fn new() -> Self` — `egui-0.35.0/src/containers/sides.rs:62`
- `fn show<RetL, RetR>(self, ui: &mut Ui, add_left: impl FnOnce(&mut Ui) -> RetL, add_right: impl FnOnce(&mut Ui) -> RetR) -> (RetL, RetR)` — `egui-0.35.0/src/containers/sides.rs:145`
- `fn shrink_left(self) -> Self` — `egui-0.35.0/src/containers/sides.rs:91`
  Try to shrink widgets on the left side.
- `fn shrink_right(self) -> Self` — `egui-0.35.0/src/containers/sides.rs:101`
  Try to shrink widgets on the right side.
- `fn spacing(self, spacing: f32) -> Self` — `egui-0.35.0/src/containers/sides.rs:81`
  The horizontal spacing between the left and right UIs.
- `fn truncate(self) -> Self` — `egui-0.35.0/src/containers/sides.rs:130`
  Truncate the text on the shrinking side.
- `fn wrap(self) -> Self` — `egui-0.35.0/src/containers/sides.rs:140`
  Wrap the text on the shrinking side.
- `fn wrap_mode(self, wrap_mode: TextWrapMode) -> Self` — `egui-0.35.0/src/containers/sides.rs:120`
  The text wrap mode for the shrinking side.

Implements: `Clone`, `Copy`, `Debug`, `Default`

### `SizedAtom` (struct) — `egui-0.35.0/src/atomics/sized_atom.rs:6`

A [`crate::Atom`] which has been sized.

Public fields:

- `id: Option<Id>`
- `size: Vec2` — The size of the atom.
- `intrinsic_size: Vec2` — Intrinsic size of the atom. This is used to calculate `Response::intrinsic_size`.
- `align: Align2` — How will the atom be aligned in its available space?
- `kind: SizedAtomKind<'a>`

Methods:

- `fn is_grow(&self) -> bool` — `egui-0.35.0/src/atomics/sized_atom.rs:28`
  Was this [`crate::Atom`] marked as `grow`?

Implements: `Clone`, `Debug`

### `SizedAtomLayout` (struct) — `egui-0.35.0/src/atomics/atom_layout.rs:451`

A measured [`AtomLayout`], ready to be painted at a [`Rect`].

Public fields:

- `frame: Frame` — The [`Frame`] painted around the contents.
- `fallback_text_color: Color32` — Set the fallback (default) text color.

Methods:

- `fn iter_images(&self) -> impl Iterator<Item = &Image<'atom>>` — `egui-0.35.0/src/atomics/atom_layout.rs:515`
- `fn iter_images_mut(&mut self) -> impl Iterator<Item = &mut Image<'atom>>` — `egui-0.35.0/src/atomics/atom_layout.rs:525`
- `fn iter_kinds(&self) -> impl Iterator<Item = &SizedAtomKind<'atom>>` — `egui-0.35.0/src/atomics/atom_layout.rs:507`
- `fn iter_kinds_mut(&mut self) -> impl Iterator<Item = &mut SizedAtomKind<'atom>>` — `egui-0.35.0/src/atomics/atom_layout.rs:511`
- `fn iter_texts(&self) -> impl Iterator<Item = &Arc<Galley>> + ?` — `egui-0.35.0/src/atomics/atom_layout.rs:535`
- `fn iter_texts_mut(&mut self) -> impl Iterator<Item = &mut Arc<Galley>> + ?` — `egui-0.35.0/src/atomics/atom_layout.rs:545`
- `fn map_images<F>(&mut self, f: F)` — `egui-0.35.0/src/atomics/atom_layout.rs:564`
- `fn map_kind<F>(&mut self, f: F)` — `egui-0.35.0/src/atomics/atom_layout.rs:555`
- `fn paint_at(self, ui: &Ui, rect: Rect, response: Response) -> AtomLayoutResponse` — `egui-0.35.0/src/atomics/atom_layout.rs:585`
  Paint the [`Frame`] and individual [`crate::Atom`]s within `rect`.

Implements: `Clone`, `Debug`, `Deref`, `DerefMut`

### `Slider` (struct) — `egui-0.35.0/src/widgets/slider.rs:98`

Control a number with a slider.

Methods:

- `fn binary(self, min_width: usize, twos_complement: bool) -> Self` — `egui-0.35.0/src/widgets/slider.rs:497`
  Set `custom_formatter` and `custom_parser` to display and parse numbers as binary integers. Floating point nu…
- `fn clamping(self, clamping: SliderClamping) -> Self` — `egui-0.35.0/src/widgets/slider.rs:290`
  Controls when the values will be clamped to the range.
- `fn custom_formatter(self, formatter: impl 'a + Fn(f64, RangeInclusive<usize>) -> String) -> Self` — `egui-0.35.0/src/widgets/slider.rs:429`
  Set custom formatter defining how numbers are converted into text.
- `fn custom_parser(self, parser: impl 'a + Fn(&str) -> Option<f64>) -> Self` — `egui-0.35.0/src/widgets/slider.rs:473`
  Set custom parser defining how the text input is parsed into a number.
- `fn drag_value_speed(self, drag_value_speed: f64) -> Self` — `egui-0.35.0/src/widgets/slider.rs:324`
  When dragging the value, how fast does it move?
- `fn fixed_decimals(self, num_decimals: usize) -> Self` — `egui-0.35.0/src/widgets/slider.rs:364`
  Set an exact number of decimals to display.
- `fn from_get_set(range: RangeInclusive<f64>, get_set_value: impl 'a + FnMut(Option<f64>) -> f64) -> Self` — `egui-0.35.0/src/widgets/slider.rs:144`
- `fn handle_shape(self, handle_shape: HandleShape) -> Self` — `egui-0.35.0/src/widgets/slider.rs:387`
  Change the shape of the slider handle
- `fn hexadecimal(self, min_width: usize, twos_complement: bool, upper: bool) -> Self` — `egui-0.35.0/src/widgets/slider.rs:567`
  Set `custom_formatter` and `custom_parser` to display and parse numbers as hexadecimal integers. Floating poi…
- `fn integer(self) -> Self` — `egui-0.35.0/src/widgets/slider.rs:594`
  Helper: equivalent to `self.precision(0).smallest_positive(1.0)`. If you use one of the integer constructors…
- `fn largest_finite(self, largest_finite: f64) -> Self` — `egui-0.35.0/src/widgets/slider.rs:247`
  For logarithmic sliders, the largest positive value we are interested in before the slider switches to `INFIN…
- `fn logarithmic(self, logarithmic: bool) -> Self` — `egui-0.35.0/src/widgets/slider.rs:229`
  Make this a logarithmic slider. This is great for when the slider spans a huge range, e.g. from one to a mill…
- `fn max_decimals(self, max_decimals: usize) -> Self` — `egui-0.35.0/src/widgets/slider.rs:347`
  Set a maximum number of decimals to display.
- `fn max_decimals_opt(self, max_decimals: Option<usize>) -> Self` — `egui-0.35.0/src/widgets/slider.rs:353`
- `fn min_decimals(self, min_decimals: usize) -> Self` — `egui-0.35.0/src/widgets/slider.rs:335`
  Set a minimum number of decimals to display.
- `fn new<Num>(value: &'a mut Num, range: impl Into<RangeInclusive<Num>>) -> Self` — `egui-0.35.0/src/widgets/slider.rs:128`
  Creates a new horizontal slider.
- `fn octal(self, min_width: usize, twos_complement: bool) -> Self` — `egui-0.35.0/src/widgets/slider.rs:532`
  Set `custom_formatter` and `custom_parser` to display and parse numbers as octal integers. Floating point num…
- `fn orientation(self, orientation: SliderOrientation) -> Self` — `egui-0.35.0/src/widgets/slider.rs:212`
  Vertical or horizontal slider? The default is horizontal.
- `fn prefix(self, prefix: impl ToString) -> Self` — `egui-0.35.0/src/widgets/slider.rs:185`
  Show a prefix before the number, e.g. "x: "
- `fn show_value(self, show_value: bool) -> Self` — `egui-0.35.0/src/widgets/slider.rs:178`
  Control whether or not the slider shows the current value. Default: `true`.
- `fn smallest_positive(self, smallest_positive: f64) -> Self` — `egui-0.35.0/src/widgets/slider.rs:238`
  For logarithmic sliders that includes zero: what is the smallest positive value you want to be able to select…
- `fn smart_aim(self, smart_aim: bool) -> Self` — `egui-0.35.0/src/widgets/slider.rs:298`
  Turn smart aim on/off. Default is ON. There is almost no point in turning this off.
- `fn step_by(self, step: f64) -> Self` — `egui-0.35.0/src/widgets/slider.rs:310`
  Sets the minimal change of the value.
- `fn suffix(self, suffix: impl ToString) -> Self` — `egui-0.35.0/src/widgets/slider.rs:192`
  Add a suffix to the number, this can be e.g. a unit ("°" or " m")
- `fn text(self, text: impl Into<WidgetText>) -> Self` — `egui-0.35.0/src/widgets/slider.rs:199`
  Show a text next to the slider (e.g. explaining what the slider controls).
- `fn text_color(self, text_color: Color32) -> Self` — `egui-0.35.0/src/widgets/slider.rs:205`
- `fn trailing_fill(self, trailing_fill: bool) -> Self` — `egui-0.35.0/src/widgets/slider.rs:377`
  Display trailing color behind the slider's circle. Default is OFF.
- `fn update_while_editing(self, update: bool) -> Self` — `egui-0.35.0/src/widgets/slider.rs:642`
  Update the value on each key press when text-editing the value.
- `fn vertical(self) -> Self` — `egui-0.35.0/src/widgets/slider.rs:219`
  Make this a vertical slider.

Implements: `Widget`

### `Spacing` (struct) — `egui-0.35.0/src/style.rs:384`

Controls the sizes and distances between widgets.

Public fields:

- `item_spacing: Vec2` — Horizontal and vertical spacing between widgets.
- `window_margin: Margin` — Horizontal and vertical margins within a window frame.
- `button_padding: Vec2` — Button size is text size plus this on each side
- `menu_margin: Margin` — Horizontal and vertical margins within a menu frame.
- `indent: f32` — Indent collapsing regions etc by this much.
- `interact_size: Vec2` — Minimum size of a [`DragValue`], color picker button, and other small widgets. `interact_…
- `slider_width: f32` — Default width of a [`Slider`].
- `slider_rail_height: f32` — Default rail height of a [`Slider`].
- `combo_width: f32` — Default (minimum) width of a [`ComboBox`].
- `text_edit_width: f32` — Default width of a [`crate::TextEdit`].
- `icon_width: f32` — Checkboxes, radio button and collapsing headers have an icon at the start. This is the wi…
- `icon_width_inner: f32` — Checkboxes, radio button and collapsing headers have an icon at the start. This is the wi…
- `icon_spacing: f32` — Checkboxes, radio button and collapsing headers have an icon at the start. This is the sp…
- `default_area_size: Vec2` — The size used for the [`Ui::max_rect`] the first frame.
- `tooltip_width: f32` — Width of a tooltip (`on_hover_ui`, `on_hover_text` etc).
- `menu_width: f32` — The default wrapping width of a menu.
- `menu_spacing: f32` — Horizontal distance between a menu and a submenu.
- `indent_ends_with_horizontal_line: bool` — End indented regions with a horizontal line
- `combo_height: f32` — Height of a combo-box before showing scroll bars.
- `scroll: ScrollStyle` — Controls the spacing of a [`crate::ScrollArea`].

Methods:

- `fn icon_rectangles(&self, rect: Rect) -> (Rect, Rect)` — `egui-0.35.0/src/style.rs:466`
  Returns small icon rectangle and big icon rectangle
- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:1936`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Spinner` (struct) — `egui-0.35.0/src/widgets/spinner.rs:10`

A spinner widget used to indicate loading.

Methods:

- `fn color(self, color: impl Into<Color32>) -> Self` — `egui-0.35.0/src/widgets/spinner.rs:32`
  Sets the spinner's color.
- `fn new() -> Self` — `egui-0.35.0/src/widgets/spinner.rs:18`
  Create a new spinner that uses the style's `interact_size` unless changed.
- `fn paint_at(&self, ui: &Ui, rect: Rect)` — `egui-0.35.0/src/widgets/spinner.rs:38`
  Paint the spinner in the given rectangle.
- `fn size(self, size: f32) -> Self` — `egui-0.35.0/src/widgets/spinner.rs:25`
  Sets the spinner's size. The size sets both the height and width, as the spinner is always square. If the siz…

Implements: `Default`, `Widget`

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

### `Style` (struct) — `egui-0.35.0/src/style.rs:243`

Specifies the look and feel of egui.

Public fields:

- `override_text_style: Option<TextStyle>` — If set this will change the default [`TextStyle`] for all widgets.
- `override_font_id: Option<FontId>` — If set this will change the font family and size for all widgets.
- `override_text_valign: Option<Align>` — How to vertically align text.
- `text_styles: BTreeMap<TextStyle, FontId>` — The [`FontFamily`] and size you want to use for a specific [`TextStyle`].
- `drag_value_text_style: TextStyle` — The style to use for [`DragValue`] text.
- `number_formatter: NumberFormatter` — How to format numbers as strings, e.g. in a [`crate::DragValue`].
- `wrap_mode: Option<TextWrapMode>` — If set, labels, buttons, etc. will use this to determine whether to wrap or truncate the…
- `spacing: Spacing` — Sizes and distances between widgets
- `interaction: Interaction` — How and when interaction happens.
- `visuals: Visuals` — Colors etc.
- `animation_time: f32` — How many seconds a typical animation should last.
- `debug: DebugOptions` — Options to help debug why egui behaves strangely.
- `explanation_tooltips: bool` — Show tooltips explaining [`DragValue`]:s etc when hovered.
- `url_in_tooltip: bool` — Show the URL of hyperlinks in a tooltip when hovered.
- `always_scroll_the_only_direction: bool` — If true and scrolling is enabled for only one direction, allow horizontal scrolling witho…
- `scroll_animation: ScrollAnimation` — The animation that should be used when scrolling a [`crate::ScrollArea`] using e.g. [`Ui:…
- `compact_menu_style: bool` — Use a more compact style for menus.

Methods:

- `fn button_style(&self, classes: &Classes, state: WidgetState) -> ButtonStyle` — `egui-0.35.0/src/widget_style.rs:146`
  The dedicated button style. The style is computed according to the classes and state of the widget. It depend…
- `fn checkbox_style(&self, classes: &Classes, state: WidgetState) -> CheckboxStyle` — `egui-0.35.0/src/widget_style.rs:174`
  The dedicated checkbox style. The style is computed according to the classes and state of the widget. It depe…
- `fn interact(&self, response: &Response) -> &WidgetVisuals` — `egui-0.35.0/src/style.rs:354`
  Use this style for interactive things. Note that you must already have a response, i.e. you must allocate spa…
- `fn interact_selectable(&self, response: &Response, selected: bool) -> WidgetVisuals` — `egui-0.35.0/src/style.rs:358`
- `fn label_style(&self, classes: &Classes, state: WidgetState) -> LabelStyle` — `egui-0.35.0/src/widget_style.rs:194`
  The dedicated label style. The style is computed according to the classes and state of the widget. It depend…
- `fn noninteractive(&self) -> &WidgetVisuals` — `egui-0.35.0/src/style.rs:370`
  Style to use for non-interactive widgets.
- `fn separator_style(&self, _classes: &Classes, _state: WidgetState) -> SeparatorStyle` — `egui-0.35.0/src/widget_style.rs:212`
  The dedicated separator style. The style is computed according to the classes and state of the widget. It dep…
- `fn text_styles(&self) -> Vec<TextStyle>` — `egui-0.35.0/src/style.rs:375`
  All known text styles.
- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:1778`
- `fn widget_style(&self, _classes: &Classes, state: WidgetState) -> WidgetStyle` — `egui-0.35.0/src/widget_style.rs:120`
  The general widget style. The style is computed according to the classes and state of the widget.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `From<Style>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `TextEdit` (struct) — `egui-0.35.0/src/widgets/text_edit/builder.rs:69`

A text region that the user can edit the contents of.

Methods:

- `fn background_color(self, color: Color32) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:230`
  Set the background color of the [`TextEdit`]. The default is [`crate::Visuals::text_edit_bg_color`].
- `fn char_limit(self, limit: usize) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:372`
  Sets the limit for the amount of characters can be entered
- `fn clip_text(self, b: bool) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:360`
  When `true` (default), overflowing text will be clipped.
- `fn code_editor(self) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:161`
  Build a [`TextEdit`] focused on code editing. By default it comes with: - monospaced font - focus lock (tab w…
- `fn cursor_at_end(self, b: bool) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:349`
  When `true` (default), the cursor will initially be placed at the end of the text.
- `fn desired_rows(self, desired_height_rows: usize) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:329`
  Set the number of rows to show by default. The default for singleline text is `1`. The default for multiline…
- `fn desired_width(self, desired_width: f32) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:320`
  Set to 0.0 to keep as small as possible. Set to [`f32::INFINITY`] to take up all available space (i.e. disabl…
- `fn font(self, font_selection: impl Into<FontSelection>) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:244`
  Pick a [`crate::FontId`] or [`TextStyle`].
- `fn frame(self, frame: Frame) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:305`
  Customize the [`Frame`] around the text edit.
- `fn hint_text(self, hint_text: impl IntoAtoms<'static>) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:208`
  Show a faint hint text when the text field is empty.
- `fn horizontal_align(self, align: Align) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:379`
  Set the horizontal align of the inner text.
- `fn id(self, id: Id) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:167`
  Use if you want to set an explicit [`Id`] for this widget.
- `fn id_salt(self, id_salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:180`
  A source for the unique [`Id`], e.g. `.id_salt("second_text_edit_field")` or `.id_salt(loop_index)`.
- `fn id_source(self, id_salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:174`
  A source for the unique [`Id`], e.g. `.id_source("second_text_edit_field")` or `.id_source(loop_index)`.
- `fn interactive(self, interactive: bool) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:298`
  Default is `true`. If set to `false` then you cannot interact with the text (neither edit or select it).
- `fn layouter(self, layouter: &'t mut dyn FnMut(&Ui, &dyn TextBuffer, f32) -> Arc<Galley>) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:285`
  Override how text is being shown inside the [`TextEdit`].
- `fn load_state(ctx: &Context, id: Id) -> Option<TextEditState>` — `egui-0.35.0/src/widgets/text_edit/builder.rs:101`
- `fn lock_focus(self, tab_will_indent: bool) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:340`
  When `false` (default), pressing TAB will move focus to the next widget.
- `fn margin(self, margin: impl Into<Margin>) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:312`
  Set margin of text. Default is `Margin::symmetric(4.0, 2.0)`
- `fn min_size(self, min_size: Vec2) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:393`
  Set the minimum size of the [`TextEdit`].
- `fn multiline(text: &'t mut dyn TextBuffer) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:122`
  A [`TextEdit`] for multiple lines. Pressing enter key will create a new line by default (can be changed with…
- `fn password(self, password: bool) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:237`
  If true, hide the letters from view and prevent copying from the field.
- `fn prefix(self, prefix: impl IntoAtoms<'static>) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:215`
  Add a prefix to the text edit. This will always be shown before the editable text.
- `fn return_key(self, return_key: impl Into<Option<KeyboardShortcut>>) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:405`
  Set the return key combination.
- `fn show(self, ui: &mut Ui) -> TextEditOutput` — `egui-0.35.0/src/widgets/text_edit/builder.rs:435`
  Show the [`TextEdit`], returning a rich [`TextEditOutput`].
- `fn singleline(text: &'t mut dyn TextBuffer) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:112`
  No newlines (`\n`) allowed. Pressing enter key will result in the [`TextEdit`] losing focus (`response.lost_f…
- `fn store_state(ctx: &Context, id: Id, state: TextEditState)` — `egui-0.35.0/src/widgets/text_edit/builder.rs:105`
- `fn suffix(self, suffix: impl IntoAtoms<'static>) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:222`
  Add a suffix to the text edit. This will always be shown after the editable text.
- `fn text_color(self, text_color: Color32) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:250`
- `fn text_color_opt(self, text_color: Option<Color32>) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:256`
- `fn vertical_align(self, align: Align) -> Self` — `egui-0.35.0/src/widgets/text_edit/builder.rs:386`
  Set the vertical align of the inner text.

Implements: `Widget`, `WidgetWithState`

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

### `Tooltip` (struct) — `egui-0.35.0/src/containers/tooltip.rs:8`

Public fields:

- `popup: Popup<'a>`

Methods:

- `fn always_open(ctx: Context, parent_layer: LayerId, parent_widget: Id, anchor: impl Into<PopupAnchor>) -> Self` — `egui-0.35.0/src/containers/tooltip.rs:20`
  Show a tooltip that is always open.
- `fn at_pointer(self) -> Self` — `egui-0.35.0/src/containers/tooltip.rs:72`
  Show the tooltip at the pointer position.
- `fn for_disabled(response: &Response) -> Self` — `egui-0.35.0/src/containers/tooltip.rs:62`
  Show a tooltip when hovering a disabled widget.
- `fn for_enabled(response: &Response) -> Self` — `egui-0.35.0/src/containers/tooltip.rs:53`
  Show a tooltip when hovering an enabled widget.
- `fn for_widget(response: &Response) -> Self` — `egui-0.35.0/src/containers/tooltip.rs:39`
  Show a tooltip for a widget. Always open (as long as this function is called).
- `fn gap(self, gap: f32) -> Self` — `egui-0.35.0/src/containers/tooltip.rs:81`
  Set the gap between the tooltip and the anchor
- `fn layout(self, layout: Layout) -> Self` — `egui-0.35.0/src/containers/tooltip.rs:88`
  Set the layout of the tooltip
- `fn next_tooltip_id(ctx: &Context, widget_id: Id) -> Id` — `egui-0.35.0/src/containers/tooltip.rs:181`
  What is the id of the next tooltip for this widget?
- `fn seconds_since_last_tooltip(ctx: &Context) -> f32` — `egui-0.35.0/src/containers/tooltip.rs:163`
- `fn should_show_tooltip(response: &Response, allow_interactive_tooltip: bool) -> bool` — `egui-0.35.0/src/containers/tooltip.rs:199`
  Should we show a tooltip for this response?
- `fn show<R>(self, content: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<R>>` — `egui-0.35.0/src/containers/tooltip.rs:101`
  Show the tooltip
- `fn tooltip_id(widget_id: Id, tooltip_count: usize) -> Id` — `egui-0.35.0/src/containers/tooltip.rs:191`
- `fn was_tooltip_open_last_frame(ctx: &Context, widget_id: Id) -> bool` — `egui-0.35.0/src/containers/tooltip.rs:378`
  Was this tooltip visible last frame?
- `fn width(self, width: f32) -> Self` — `egui-0.35.0/src/containers/tooltip.rs:95`
  Set the width of the tooltip

### `TouchDeviceId` (struct) — `egui-0.35.0/src/data/input/touch.rs:4`

this is a `u64` as values of this kind can always be obtained by hashing

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `TouchId` (struct) — `egui-0.35.0/src/data/input/touch.rs:11`

Unique identification of a touch occurrence (finger or pen or …). A Touch ID is valid until the finger is lifted. A new ID is used for the next touch.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `From<i32>`, `From<u32>`, `From<u64>`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `Ui` (struct) — `egui-0.35.0/src/ui.rs:29`

This is what you use to place widgets.

Methods:

- `fn add(&mut self, widget: impl Widget) -> Response` — `egui-0.35.0/src/ui.rs:1520`
  Add a [`Widget`] to this [`Ui`] at a location dependent on the current [`Layout`].
- `fn add_enabled(&mut self, enabled: bool, widget: impl Widget) -> Response` — `egui-0.35.0/src/ui.rs:1587`
  Add a single [`Widget`] that is possibly disabled, i.e. greyed out and non-interactive.
- `fn add_enabled_ui<R>(&mut self, enabled: bool, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:1619`
  Add a section that is possibly disabled, i.e. greyed out and non-interactive.
- `fn add_sized(&mut self, max_size: impl Into<Vec2>, widget: impl Widget) -> Response` — `egui-0.35.0/src/ui.rs:1537`
  Add a [`Widget`] to this [`Ui`] with a given size. The widget will attempt to fit within the given size, but…
- `fn add_space(&mut self, amount: f32)` — `egui-0.35.0/src/ui.rs:1674`
  Add extra space before the next widget.
- `fn add_visible(&mut self, visible: bool, widget: impl Widget) -> Response` — `egui-0.35.0/src/ui.rs:1646`
  Add a single [`Widget`] that is possibly invisible.
- `fn advance_cursor_after_rect(&mut self, rect: Rect) -> Id` — `egui-0.35.0/src/ui.rs:1263`
  Allocate a rect without interacting with it.
- `fn allocate_at_least(&mut self, desired_size: Vec2, sense: Sense) -> (Rect, Response)` — `egui-0.35.0/src/ui.rs:1161`
  Allocate at least as much space as needed, and interact with that rect.
- `fn allocate_exact_size(&mut self, desired_size: Vec2, sense: Sense) -> (Rect, Response)` — `egui-0.35.0/src/ui.rs:1150`
  Returns a [`Rect`] with exactly what you asked for.
- `fn allocate_painter(&mut self, desired_size: Vec2, sense: Sense) -> (Response, Painter)` — `egui-0.35.0/src/ui.rs:1370`
  Convenience function to get a region to paint on.
- `fn allocate_rect(&mut self, rect: Rect, sense: Sense) -> Response` — `egui-0.35.0/src/ui.rs:1256`
  Allocate a specific part of the [`Ui`].
- `fn allocate_response(&mut self, desired_size: Vec2, sense: Sense) -> Response` — `egui-0.35.0/src/ui.rs:1138`
  Allocate space for a widget and check for interaction in the space. Returns a [`Response`] which contains a r…
- `fn allocate_space(&mut self, desired_size: Vec2) -> (Id, Rect)` — `egui-0.35.0/src/ui.rs:1187`
  Reserve this much space and move the cursor. Returns where to put the widget.
- `fn allocate_ui<R>(&mut self, desired_size: Vec2, add_contents: impl FnOnce(&mut Self) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:1308`
  Allocated the given space and then adds content to that space. If the contents overflow, more space will be a…
- `fn allocate_ui_with_layout<R>(&mut self, desired_size: Vec2, layout: Layout, add_contents: impl FnOnce(&mut Self) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:1321`
  Allocated the given space and then adds content to that space. If the contents overflow, more space will be a…
- `fn auto_id_with(&self, id_salt: impl AsIdSalt) -> Id` — `egui-0.35.0/src/ui.rs:893`
  Same as `ui.next_auto_id().with(id_salt)`
- `fn available_height(&self) -> f32` — `egui-0.35.0/src/ui.rs:861`
  The available height at the moment, given the current cursor.
- `fn available_rect_before_wrap(&self) -> Rect` — `egui-0.35.0/src/ui.rs:875`
  In case of a wrapping layout, how much space is left on this row/column?
- `fn available_size(&self) -> Vec2` — `egui-0.35.0/src/ui.rs:847`
  The available space at the moment, given the current cursor.
- `fn available_size_before_wrap(&self) -> Vec2` — `egui-0.35.0/src/ui.rs:868`
  In case of a wrapping layout, how much space is left on this row/column?
- `fn available_width(&self) -> f32` — `egui-0.35.0/src/ui.rs:854`
  The available width at the moment, given the current cursor.
- `fn button(&mut self, atoms: impl IntoAtoms<'a>) -> Response` — `egui-0.35.0/src/ui.rs:1847`
  Usage: `if ui.button("Click me").clicked() { … }`
- `fn centered_and_justified<R>(&mut self, add_contents: impl FnOnce(&mut Self) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2480`
  This will make the next added widget centered and justified in the available space.
- `fn checkbox(&mut self, checked: &'a mut bool, atoms: impl IntoAtoms<'a>) -> Response` — `egui-0.35.0/src/ui.rs:1865`
  Show a checkbox.
- `fn clip_rect(&self) -> Rect` — `egui-0.35.0/src/ui.rs:639`
  Screen-space rectangle for clipping what we paint in this ui. This is used, for instance, to avoid painting o…
- `fn close(&self)` — `egui-0.35.0/src/ui.rs:1039`
  Find and close the first closable parent.
- `fn close_kind(&self, ui_kind: UiKind)` — `egui-0.35.0/src/ui.rs:1063`
  Find and close the first closable parent of a specific [`UiKind`].
- `fn code(&mut self, text: impl Into<RichText>) -> Response` — `egui-0.35.0/src/ui.rs:1727`
  Show text as monospace with a gray background.
- `fn code_editor<S>(&mut self, text: &mut S) -> Response` — `egui-0.35.0/src/ui.rs:1823`
  A [`TextEdit`] for code editing.
- `fn collapsing<R>(&mut self, heading: impl Into<WidgetText>, add_contents: impl FnOnce(&mut Ui) -> R) -> CollapsingResponse<R>` — `egui-0.35.0/src/ui.rs:2220`
  A [`CollapsingHeader`] that starts out collapsed.
- `fn color_edit_button_hsva(&mut self, hsva: &mut Hsva) -> Response` — `egui-0.35.0/src/ui.rs:2050`
  Shows a button with the given color.
- `fn color_edit_button_rgb(&mut self, rgb: &mut [f32; 3]) -> Response` — `egui-0.35.0/src/ui.rs:2066`
  Shows a button with the given color.
- `fn color_edit_button_rgba_premultiplied(&mut self, rgba_premul: &mut [f32; 4]) -> Response` — `egui-0.35.0/src/ui.rs:2098`
  Shows a button with the given color.
- `fn color_edit_button_rgba_unmultiplied(&mut self, rgba_unmul: &mut [f32; 4]) -> Response` — `egui-0.35.0/src/ui.rs:2119`
  Shows a button with the given color.
- `fn color_edit_button_srgb(&mut self, srgb: &mut [u8; 3]) -> Response` — `egui-0.35.0/src/ui.rs:2058`
  Shows a button with the given color.
- `fn color_edit_button_srgba(&mut self, srgba: &mut Color32) -> Response` — `egui-0.35.0/src/ui.rs:2043`
  Shows a button with the given color.
- `fn color_edit_button_srgba_premultiplied(&mut self, srgba: &mut [u8; 4]) -> Response` — `egui-0.35.0/src/ui.rs:2074`
  Shows a button with the given color.
- `fn color_edit_button_srgba_unmultiplied(&mut self, srgba: &mut [u8; 4]) -> Response` — `egui-0.35.0/src/ui.rs:2086`
  Shows a button with the given color.
- `fn colored_label(&mut self, color: impl Into<Color32>, text: impl Into<RichText>) -> Response` — `egui-0.35.0/src/ui.rs:1702`
  Show colored text.
- `fn columns<R>(&mut self, num_columns: usize, add_contents: impl FnOnce(&mut [Self]) -> R) -> R` — `egui-0.35.0/src/ui.rs:2525`
  Temporarily split a [`Ui`] into several columns.
- `fn columns_const<NUM_COL, R>(&mut self, add_contents: impl FnOnce(&mut [Self; NUM_COL]) -> R) -> R` — `egui-0.35.0/src/ui.rs:2592`
  Temporarily split a [`Ui`] into several columns.
- `fn ctx(&self) -> &Context` — `egui-0.35.0/src/ui.rs:451`
  Get a reference to the parent [`Context`].
- `fn cursor(&self) -> Rect` — `egui-0.35.0/src/ui.rs:1290`
  Where the next widget will be put.
- `fn debug_paint_cursor(&self)` — `egui-0.35.0/src/ui.rs:2883`
  Shows where the next widget is going to be placed
- `fn disable(&mut self)` — `egui-0.35.0/src/ui.rs:496`
  Calling `disable()` will cause the [`Ui`] to deny all future interaction and all the widgets will draw with a…
- `fn dnd_drag_source<Payload, R>(&mut self, id: Id, payload: Payload, add_contents: impl FnOnce(&mut Self) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2641`
  Create something that can be drag-and-dropped.
- `fn dnd_drop_zone<Payload, R>(&mut self, frame: Frame, add_contents: impl FnOnce(&mut Ui) -> R) -> (InnerResponse<R>, Option<Arc<Payload>>)` — `egui-0.35.0/src/ui.rs:2693`
  Surround the given ui with a frame which changes colors when you can drop something onto it.
- `fn drag_angle(&mut self, radians: &mut f32) -> Response` — `egui-0.35.0/src/ui.rs:1970`
  Modify an angle. The given angle should be in radians, but is shown to the user in degrees. The angle is NOT…
- `fn drag_angle_tau(&mut self, radians: &mut f32) -> Response` — `egui-0.35.0/src/ui.rs:1986`
  Modify an angle. The given angle should be in radians, but is shown to the user in fractions of one Tau (i.e.…
- `fn end_row(&mut self)` — `egui-0.35.0/src/ui.rs:2504`
  Move to the next row in a grid layout or wrapping layout. Otherwise does nothing.
- `fn expand_to_include_rect(&mut self, rect: Rect)` — `egui-0.35.0/src/ui.rs:796`
  Expand the `min_rect` and `max_rect` of this ui to include a child at the given rect.
- `fn expand_to_include_x(&mut self, x: f32)` — `egui-0.35.0/src/ui.rs:828`
  Ensure we are big enough to contain the given x-coordinate. This is sometimes useful to expand a ui to stretc…
- `fn expand_to_include_y(&mut self, y: f32)` — `egui-0.35.0/src/ui.rs:834`
  Ensure we are big enough to contain the given y-coordinate. This is sometimes useful to expand a ui to stretc…
- `fn group<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2146`
  Put into a [`Frame::group`], visually grouping the contents together
- `fn heading(&mut self, text: impl Into<RichText>) -> Response` — `egui-0.35.0/src/ui.rs:1713`
  Show large text.
- `fn horizontal<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2314`
  Start a ui with horizontal layout. After you have called this, the function registers the contents as any oth…
- `fn horizontal_centered<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2319`
  Like [`Self::horizontal`], but allocates the full vertical height and then centers elements vertically.
- `fn horizontal_top<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2334`
  Like [`Self::horizontal`], but aligns content with top.
- `fn horizontal_wrapped<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2363`
  Start a ui with horizontal layout that wraps to a new row when it reaches the right edge of the `max_size`. A…
- `fn hyperlink(&mut self, url: impl ToString) -> Response` — `egui-0.35.0/src/ui.rs:1781`
  Link to a web page.
- `fn hyperlink_to(&mut self, label: impl Into<WidgetText>, url: impl ToString) -> Response` — `egui-0.35.0/src/ui.rs:1794`
  Shortcut for `add(Hyperlink::from_label_and_url(label, url))`.
- `fn id(&self) -> Id` — `egui-0.35.0/src/ui.rs:344`
  Generated based on id of parent ui together with an optional id salt.
- `fn image(&mut self, source: impl Into<ImageSource<'a>>) -> Response` — `egui-0.35.0/src/ui.rs:2033`
  Show an image available at the given `uri`.
- `fn indent<R>(&mut self, id_salt: impl AsIdSalt, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2233`
  Create a child ui which is indented to the right.
- `fn interact(&self, rect: Rect, id: Id, sense: Sense) -> Response` — `egui-0.35.0/src/ui.rs:906`
  Check for clicks, drags and/or hover on a specific region of this [`Ui`].
- `fn interact_opt(&self, rect: Rect, id: Id, sense: Sense, options: InteractOptions) -> Response` — `egui-0.35.0/src/ui.rs:911`
  Check for clicks, drags and/or hover on a specific region of this [`Ui`].
- `fn is_enabled(&self) -> bool` — `egui-0.35.0/src/ui.rs:470`
  If `false`, the [`Ui`] does not allow any interaction and the widgets in it will draw with a gray look.
- `fn is_rect_visible(&self, rect: Rect) -> bool` — `egui-0.35.0/src/ui.rs:666`
  Can be used for culling: if `false`, then no part of `rect` will be visible on screen.
- `fn is_sizing_pass(&self) -> bool` — `egui-0.35.0/src/ui.rs:327`
  Set to true in special cases where we do one frame where we size up the contents of the Ui, without actually…
- `fn is_tooltip(&self) -> bool` — `egui-0.35.0/src/ui.rs:439`
  Is this [`Ui`] in a tooltip?
- `fn is_visible(&self) -> bool` — `egui-0.35.0/src/ui.rs:509`
  If `false`, any widgets added to the [`Ui`] will be invisible and non-interactive.
- `fn label(&mut self, text: impl Into<WidgetText>) -> Response` — `egui-0.35.0/src/ui.rs:1695`
  Show some text.
- `fn layer_id(&self) -> LayerId` — `egui-0.35.0/src/ui.rs:625`
  Use this to paint stuff within this [`Ui`].
- `fn layout(&self) -> &Layout` — `egui-0.35.0/src/ui.rs:581`
  Read the [`Layout`].
- `fn link(&mut self, text: impl Into<WidgetText>) -> Response` — `egui-0.35.0/src/ui.rs:1766`
  Looks like a hyperlink.
- `fn make_persistent_id(&self, id_salt: impl AsIdSalt) -> Id` — `egui-0.35.0/src/ui.rs:883`
  Use this to generate widget ids for widgets that have persistent state in [`Memory`].
- `fn max_rect(&self) -> Rect` — `egui-0.35.0/src/ui.rs:697`
  New widgets will *try* to fit within this rectangle.
- `fn menu_button<R>(&mut self, atoms: impl IntoAtoms<'a>, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<Option<R>>` — `egui-0.35.0/src/ui.rs:2787`
  Create a menu button that when clicked will show the given menu.
- `fn menu_image_button<R>(&mut self, image: impl Into<Image<'a>>, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<Option<R>>` — `egui-0.35.0/src/ui.rs:2821`
  Create a menu button with an image that when clicked will show the given menu.
- `fn menu_image_text_button<R>(&mut self, image: impl Into<Image<'a>>, title: impl Into<WidgetText>, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<Option<R>>` — `egui-0.35.0/src/ui.rs:2858`
  Create a menu button with an image and a text that when clicked will show the given menu.
- `fn min_rect(&self) -> Rect` — `egui-0.35.0/src/ui.rs:681`
  Where and how large the [`Ui`] is already. All widgets that have been added to this [`Ui`] fits within this r…
- `fn min_size(&self) -> Vec2` — `egui-0.35.0/src/ui.rs:686`
  Size of content; same as `min_rect().size()`
- `fn monospace(&mut self, text: impl Into<RichText>) -> Response` — `egui-0.35.0/src/ui.rs:1720`
  Show monospace (fixed width) text.
- `fn multiply_opacity(&mut self, opacity: f32)` — `egui-0.35.0/src/ui.rs:567`
  Like [`Self::set_opacity`], but multiplies the given value with the current opacity.
- `fn new(ctx: Context, id: Id, ui_builder: UiBuilder) -> Self` — `egui-0.35.0/src/ui.rs:108`
  Create a new top-level [`Ui`].
- `fn new_child(&mut self, ui_builder: UiBuilder) -> Self` — `egui-0.35.0/src/ui.rs:208`
  Create a child `Ui` with the properties of the given builder.
- `fn next_auto_id(&self) -> Id` — `egui-0.35.0/src/ui.rs:888`
  This is the `Id` that will be assigned to the next widget added to this `Ui`.
- `fn next_widget_position(&self) -> Pos2` — `egui-0.35.0/src/ui.rs:1299`
  Where do we expect a zero-sized widget to be placed?
- `fn opacity(&self) -> f32` — `egui-0.35.0/src/ui.rs:575`
  Read the current opacity of the underlying painter.
- `fn painter(&self) -> &Painter` — `egui-0.35.0/src/ui.rs:457`
  Use this to paint stuff within this [`Ui`].
- `fn painter_at(&self, rect: Rect) -> Painter` — `egui-0.35.0/src/ui.rs:619`
  Create a painter for a sub-region of this Ui.
- `fn pixels_per_point(&self) -> f32` — `egui-0.35.0/src/ui.rs:463`
  Number of physical pixels for each logical UI point.
- `fn place(&mut self, max_rect: Rect, widget: impl Widget) -> Response` — `egui-0.35.0/src/ui.rs:1552`
  Add a [`Widget`] to this [`Ui`] at a specific location (manual layout) without affecting this [`Ui`]s cursor.
- `fn push_id<R>(&mut self, id_salt: impl AsIdSalt, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2163`
  Create a child Ui with an explicit [`Id`].
- `fn put(&mut self, max_rect: Rect, widget: impl Widget) -> Response` — `egui-0.35.0/src/ui.rs:1565`
  Add a [`Widget`] to this [`Ui`] at a specific location (manual layout) and advance the cursor after the widge…
- `fn radio(&mut self, selected: bool, atoms: impl IntoAtoms<'a>) -> Response` — `egui-0.35.0/src/ui.rs:1887`
  Show a [`RadioButton`]. Often you want to use [`Self::radio_value`] instead.
- `fn radio_value<Value>(&mut self, current_value: &mut Value, alternative: Value, atoms: impl IntoAtoms<'a>) -> Response` — `egui-0.35.0/src/ui.rs:1910`
  Show a [`RadioButton`]. It is selected if `*current_value == selected_value`. If clicked, `selected_value` is…
- `fn rect_contains_pointer(&self, rect: Rect) -> bool` — `egui-0.35.0/src/ui.rs:1003`
  Is the pointer (mouse/touch) above this rectangle in this [`Ui`]?
- `fn reset_style(&mut self)` — `egui-0.35.0/src/ui.rs:391`
  Reset to the default style set in [`Context`].
- `fn response(&self) -> Response` — `egui-0.35.0/src/ui.rs:943`
  Read the [`Ui`]'s background [`Response`]. Its [`Sense`] will be based on the [`UiBuilder::sense`] used to cr…
- `fn scope<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2185`
  Create a scoped child ui.
- `fn scope_builder<R>(&mut self, ui_builder: UiBuilder, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2193`
  Create a scoped child ui, inheriting properties from the parent as specified by the [`UiBuilder`]. In contras…
- `fn scope_dyn<R>(&mut self, ui_builder: UiBuilder, add_contents: Box<dyn FnOnce(&mut Ui) -> R + 'c>) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2202`
  [`Self::scope_builder`] but with dynamic dispatch.
- `fn scroll_to_cursor(&self, align: Option<Align>)` — `egui-0.35.0/src/ui.rs:1441`
  Adjust the scroll position of any parent [`crate::ScrollArea`] so that the cursor (where the next widget goes…
- `fn scroll_to_cursor_animation(&self, align: Option<Align>, animation: ScrollAnimation)` — `egui-0.35.0/src/ui.rs:1446`
  Same as [`Self::scroll_to_cursor`], but allows you to specify the [`style::ScrollAnimation`].
- `fn scroll_to_rect(&self, rect: Rect, align: Option<Align>)` — `egui-0.35.0/src/ui.rs:1399`
  Adjust the scroll position of any parent [`crate::ScrollArea`] so that the given [`Rect`] becomes visible.
- `fn scroll_to_rect_animation(&self, rect: Rect, align: Option<Align>, animation: ScrollAnimation)` — `egui-0.35.0/src/ui.rs:1404`
  Same as [`Self::scroll_to_rect`], but allows you to specify the [`style::ScrollAnimation`].
- `fn scroll_with_delta(&self, delta: Vec2)` — `egui-0.35.0/src/ui.rs:1490`
  Scroll this many points in the given direction, in the parent [`crate::ScrollArea`].
- `fn scroll_with_delta_animation(&self, delta: Vec2, animation: ScrollAnimation)` — `egui-0.35.0/src/ui.rs:1495`
  Same as [`Self::scroll_with_delta`], but allows you to specify the [`style::ScrollAnimation`].
- `fn selectable_label(&mut self, checked: bool, text: impl IntoAtoms<'a>) -> Response` — `egui-0.35.0/src/ui.rs:1928`
  Show a label which can be selected or not.
- `fn selectable_value<Value>(&mut self, current_value: &mut Value, selected_value: Value, text: impl IntoAtoms<'a>) -> Response` — `egui-0.35.0/src/ui.rs:1938`
  Show selectable text. It is selected if `*current_value == selected_value`. If clicked, `selected_value` is a…
- `fn separator(&mut self) -> Response` — `egui-0.35.0/src/ui.rs:1956`
  Shortcut for `add(Separator::default())`
- `fn set_clip_rect(&mut self, clip_rect: Rect)` — `egui-0.35.0/src/ui.rs:658`
  Screen-space rectangle for clipping what we paint in this ui. This is used, for instance, to avoid painting o…
- `fn set_height(&mut self, height: f32)` — `egui-0.35.0/src/ui.rs:821`
  Set both the minimum and maximum height.
- `fn set_height_range(&mut self, height: impl Into<Rangef>)` — `egui-0.35.0/src/ui.rs:808`
  `ui.set_height_range(min..=max);` is equivalent to `ui.set_min_height(min); ui.set_max_height(max);`.
- `fn set_invisible(&mut self)` — `egui-0.35.0/src/ui.rs:537`
  Calling `set_invisible()` will cause all further widgets to be invisible, yet still allocate space.
- `fn set_max_height(&mut self, height: f32)` — `egui-0.35.0/src/ui.rs:723`
  Set the maximum height of the ui. You won't be able to shrink it below the current minimum size.
- `fn set_max_size(&mut self, size: Vec2)` — `egui-0.35.0/src/ui.rs:710`
  Set the maximum size of the ui. You won't be able to shrink it below the current minimum size.
- `fn set_max_width(&mut self, width: f32)` — `egui-0.35.0/src/ui.rs:717`
  Set the maximum width of the ui. You won't be able to shrink it below the current minimum size.
- `fn set_min_height(&mut self, height: f32)` — `egui-0.35.0/src/ui.rs:748`
  Set the minimum height of the ui. This can't shrink the ui, only make it larger.
- `fn set_min_size(&mut self, size: Vec2)` — `egui-0.35.0/src/ui.rs:731`
  Set the minimum size of the ui. This can't shrink the ui, only make it larger.
- `fn set_min_width(&mut self, width: f32)` — `egui-0.35.0/src/ui.rs:738`
  Set the minimum width of the ui. This can't shrink the ui, only make it larger.
- `fn set_opacity(&mut self, opacity: f32)` — `egui-0.35.0/src/ui.rs:560`
  Make the widget in this [`Ui`] semi-transparent.
- `fn set_row_height(&mut self, height: f32)` — `egui-0.35.0/src/ui.rs:2510`
  Set row height in horizontal wrapping layout.
- `fn set_style(&mut self, style: impl Into<Arc<Style>>)` — `egui-0.35.0/src/ui.rs:386`
  Changes apply to this [`Ui`] and its subsequent children.
- `fn set_width(&mut self, width: f32)` — `egui-0.35.0/src/ui.rs:815`
  Set both the minimum and maximum width.
- `fn set_width_range(&mut self, width: impl Into<Rangef>)` — `egui-0.35.0/src/ui.rs:801`
  `ui.set_width_range(min..=max);` is equivalent to `ui.set_min_width(min); ui.set_max_width(max);`.
- `fn should_close(&self) -> bool` — `egui-0.35.0/src/ui.rs:1091`
  Was [`Ui::close`] called on this [`Ui`] or any of its children? Only works if the [`Ui`] was created with [`U…
- `fn shrink_clip_rect(&mut self, new_clip_rect: Rect)` — `egui-0.35.0/src/ui.rs:649`
  Constrain the rectangle in which we can paint.
- `fn shrink_height_to_current(&mut self)` — `egui-0.35.0/src/ui.rs:791`
  Helper: shrinks the max height to the current height, so further widgets will try not to be taller than previ…
- `fn shrink_width_to_current(&mut self)` — `egui-0.35.0/src/ui.rs:785`
  Helper: shrinks the max width to the current width, so further widgets will try not to be wider than previous…
- `fn skip_ahead_auto_ids(&mut self, count: usize)` — `egui-0.35.0/src/ui.rs:898`
  Pretend like `count` widgets have been allocated.
- `fn small(&mut self, text: impl Into<RichText>) -> Response` — `egui-0.35.0/src/ui.rs:1734`
  Show small text.
- `fn small_button(&mut self, atoms: impl IntoAtoms<'a>) -> Response` — `egui-0.35.0/src/ui.rs:1857`
  A button as small as normal body text.
- `fn spacing(&self) -> &Spacing` — `egui-0.35.0/src/ui.rs:398`
  The current spacing options for this [`Ui`]. Short for `ui.style().spacing`.
- `fn spacing_mut(&mut self) -> &mut Spacing` — `egui-0.35.0/src/ui.rs:411`
  Mutably borrow internal [`Spacing`]. Changes apply to this [`Ui`] and its subsequent children.
- `fn spinner(&mut self) -> Response` — `egui-0.35.0/src/ui.rs:1964`
  Shortcut for `add(Spinner::new())`
- `fn stack(&self) -> &Arc<UiStack>` — `egui-0.35.0/src/ui.rs:445`
  Get a reference to this [`Ui`]'s [`UiStack`].
- `fn strong(&mut self, text: impl Into<RichText>) -> Response` — `egui-0.35.0/src/ui.rs:1741`
  Show text that stand out a bit (e.g. slightly brighter).
- `fn style(&self) -> &Arc<Style>` — `egui-0.35.0/src/ui.rs:364`
  Style options for this [`Ui`] and its children.
- `fn style_mut(&mut self) -> &mut Style` — `egui-0.35.0/src/ui.rs:379`
  Mutably borrow internal [`Style`]. Changes apply to this [`Ui`] and its subsequent children.
- `fn take_available_height(&mut self)` — `egui-0.35.0/src/ui.rs:776`
  Makes the ui always fill up the available space in the y axis.
- `fn take_available_space(&mut self)` — `egui-0.35.0/src/ui.rs:760`
  Makes the ui always fill up the available space.
- `fn take_available_width(&mut self)` — `egui-0.35.0/src/ui.rs:768`
  Makes the ui always fill up the available space in the x axis.
- `fn text_edit_multiline<S>(&mut self, text: &mut S) -> Response` — `egui-0.35.0/src/ui.rs:1811`
  A [`TextEdit`] for multiple lines. Pressing enter key will create a new line.
- `fn text_edit_singleline<S>(&mut self, text: &mut S) -> Response` — `egui-0.35.0/src/ui.rs:1801`
  No newlines (`\n`) allowed. Pressing enter key will result in the [`TextEdit`] losing focus (`response.lost_f…
- `fn text_style_height(&self, style: &TextStyle) -> f32` — `egui-0.35.0/src/ui.rs:632`
  The height of text of this text style.
- `fn text_valign(&self) -> Align` — `egui-0.35.0/src/ui.rs:609`
  How to vertically align text
- `fn toggle_value(&mut self, selected: &mut bool, atoms: impl IntoAtoms<'a>) -> Response` — `egui-0.35.0/src/ui.rs:1874`
  Acts like a checkbox, but looks like a [`Button::selectable`].
- `fn ui_contains_pointer(&self) -> bool` — `egui-0.35.0/src/ui.rs:1015`
  Is the pointer (mouse/touch) above the current [`Ui`]?
- `fn unique_id(&self) -> Id` — `egui-0.35.0/src/ui.rs:356`
  This is a globally unique ID of this `Ui`, based on where in the hierarchy of widgets this Ui is in.
- `fn vertical<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2404`
  Start a ui with vertical layout. Widgets will be left-justified.
- `fn vertical_centered<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2423`
  Start a ui with vertical layout. Widgets will be horizontally centered.
- `fn vertical_centered_justified<R>(&mut self, add_contents: impl FnOnce(&mut Ui) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2444`
  Start a ui with vertical layout. Widgets will be horizontally centered and justified (fill full width).
- `fn visuals(&self) -> &Visuals` — `egui-0.35.0/src/ui.rs:418`
  The current visuals settings of this [`Ui`]. Short for `ui.style().visuals`.
- `fn visuals_mut(&mut self) -> &mut Visuals` — `egui-0.35.0/src/ui.rs:433`
  Mutably borrow internal `visuals`. Changes apply to this [`Ui`] and its subsequent children.
- `fn weak(&mut self, text: impl Into<RichText>) -> Response` — `egui-0.35.0/src/ui.rs:1748`
  Show text that is weaker (fainter color).
- `fn will_parent_close(&self) -> bool` — `egui-0.35.0/src/ui.rs:1105`
  Will this [`Ui`] or any of its parents close this frame?
- `fn with_layout<R>(&mut self, layout: Layout, add_contents: impl FnOnce(&mut Self) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2469`
  The new layout will take up all available space.
- `fn with_visual_transform<R>(&mut self, transform: TSTransform, add_contents: impl FnOnce(&mut Self) -> R) -> InnerResponse<R>` — `egui-0.35.0/src/ui.rs:2745`
  Create a new Scope and transform its contents via a [`emath::TSTransform`]. This only affects visuals, inputs…
- `fn wrap_mode(&self) -> TextWrapMode` — `egui-0.35.0/src/ui.rs:588`
  Which wrap mode should the text use in this [`Ui`]?

Implements: `Deref`, `Drop`

### `UiBuilder` (struct) — `egui-0.35.0/src/ui_builder.rs:19`

The properties specified when creating a top-level or child [`Ui`].

Public fields:

- `id_source: Option<IdSource>`
- `ui_stack_info: UiStackInfo`
- `layer_id: Option<LayerId>`
- `max_rect: Option<Rect>`
- `layout: Option<Layout>`
- `disabled: bool`
- `invisible: bool`
- `sizing_pass: bool`
- `style: Option<Arc<Style>>`
- `sense: Option<Sense>`
- `accessibility_parent: Option<Id>`
- `classes: Classes`

Methods:

- `fn accessibility_parent(self, parent_id: Id) -> Self` — `egui-0.35.0/src/ui_builder.rs:193`
  Set the accessibility parent for this [`Ui`].
- `fn closable(self) -> Self` — `egui-0.35.0/src/ui_builder.rs:181`
  Make this [`Ui`] closable.
- `fn disabled(self) -> Self` — `egui-0.35.0/src/ui_builder.rs:123`
  Make the new `Ui` disabled, i.e. grayed-out and non-interactive.
- `fn id(self, id: Id) -> Self` — `egui-0.35.0/src/ui_builder.rs:71`
  Set an id of the new `Ui` that is independent of the parent `Ui`. This way child widgets can be moved in the…
- `fn id_salt(self, id_salt: impl AsIdSalt) -> Self` — `egui-0.35.0/src/ui_builder.rs:56`
  Seed the child `Ui` with this `id_salt`, which will be mixed with the [`Ui::id`] of the parent.
- `fn invisible(self) -> Self` — `egui-0.35.0/src/ui_builder.rs:134`
  Make the contents invisible.
- `fn layer_id(self, layer_id: LayerId) -> Self` — `egui-0.35.0/src/ui_builder.rs:85`
  Show the [`Ui`] in a different [`LayerId`] from its parent.
- `fn layout(self, layout: Layout) -> Self` — `egui-0.35.0/src/ui_builder.rs:112`
  Override the layout.
- `fn max_rect(self, max_rect: Rect) -> Self` — `egui-0.35.0/src/ui_builder.rs:103`
  Set the max rectangle, within which widgets will go.
- `fn new() -> Self` — `egui-0.35.0/src/ui_builder.rs:46`
- `fn sense(self, sense: Sense) -> Self` — `egui-0.35.0/src/ui_builder.rs:168`
  Set if you want sense clicks and/or drags. Default is [`Sense::hover`].
- `fn sizing_pass(self) -> Self` — `egui-0.35.0/src/ui_builder.rs:146`
  Set to true in special cases where we do one frame where we size up the contents of the Ui, without actually…
- `fn style(self, style: impl Into<Arc<Style>>) -> Self` — `egui-0.35.0/src/ui_builder.rs:155`
  Override the style.
- `fn ui_stack_info(self, ui_stack_info: UiStackInfo) -> Self` — `egui-0.35.0/src/ui_builder.rs:78`
  Provide some information about the new `Ui` being built.

Implements: `Clone`, `Default`, `HasClasses`

### `UiStack` (struct) — `egui-0.35.0/src/ui_stack.rs:208`

Information about a [`crate::Ui`] and its parents.

Public fields:

- `id: Id`
- `info: UiStackInfo`
- `layout_direction: Direction`
- `min_rect: Rect`
- `max_rect: Rect`
- `parent: Option<Arc<Self>>`
- `classes: Classes`

Methods:

- `fn bg_color(&self) -> Color32` — `egui-0.35.0/src/ui_stack.rs:266`
  The background color of this [`crate::Ui`].
- `fn contained_in(&self, kind: UiKind) -> bool` — `egui-0.35.0/src/ui_stack.rs:290`
  Check if this node is or is contained in a [`crate::Ui`] of a specific kind.
- `fn frame(&self) -> &Frame` — `egui-0.35.0/src/ui_stack.rs:227`
- `fn has_visible_frame(&self) -> bool` — `egui-0.35.0/src/ui_stack.rs:257`
  This this [`crate::Ui`] a [`crate::Frame`] with a visible stroke?
- `fn is_area_ui(&self) -> bool` — `egui-0.35.0/src/ui_stack.rs:245`
  Is this [`crate::Ui`] an [`crate::Area`]?
- `fn is_panel_ui(&self) -> bool` — `egui-0.35.0/src/ui_stack.rs:239`
  Is this [`crate::Ui`] a panel?
- `fn is_root_ui(&self) -> bool` — `egui-0.35.0/src/ui_stack.rs:251`
  Is this a root [`crate::Ui`], i.e. created with [`crate::Ui::new()`]?
- `fn iter(&self) -> UiStackIterator<'_>` — `egui-0.35.0/src/ui_stack.rs:285`
  Return an iterator that walks the stack from this node to the root.
- `fn kind(&self) -> Option<UiKind>` — `egui-0.35.0/src/ui_stack.rs:222`
- `fn tags(&self) -> &UiTags` — `egui-0.35.0/src/ui_stack.rs:233`
  User tags.

Implements: `Debug`

### `UiStackInfo` (struct) — `egui-0.35.0/src/ui_stack.rs:108`

Information about a [`crate::Ui`] to be included in the corresponding [`UiStack`].

Public fields:

- `kind: Option<UiKind>`
- `frame: Frame`
- `tags: UiTags`

Methods:

- `fn new(kind: UiKind) -> Self` — `egui-0.35.0/src/ui_stack.rs:117`
  Create a new [`UiStackInfo`] with the given kind and an empty frame.
- `fn with_frame(self, frame: Frame) -> Self` — `egui-0.35.0/src/ui_stack.rs:125`
- `fn with_tag(self, key: impl Into<String>) -> Self` — `egui-0.35.0/src/ui_stack.rs:132`
  Insert a tag with no value.
- `fn with_tag_value(self, key: impl Into<String>, value: impl Any + Send + Sync + 'static) -> Self` — `egui-0.35.0/src/ui_stack.rs:139`
  Insert a tag with some value.

Implements: `Clone`, `Debug`, `Default`

### `UiStackIterator` (struct) — `egui-0.35.0/src/ui_stack.rs:300`

Iterator that walks up a stack of `StackFrame`s.

Implements: `FusedIterator`, `Iterator`

### `UiTags` (struct) — `egui-0.35.0/src/ui_stack.rs:161`

User-chosen tags.

Methods:

- `fn contains(&self, key: &str) -> bool` — `egui-0.35.0/src/ui_stack.rs:174`
- `fn get_any(&self, key: &str) -> Option<&Arc<dyn Any + Send + Sync + 'static>>` — `egui-0.35.0/src/ui_stack.rs:183`
  Get the value of a tag.
- `fn get_downcast<T>(&self, key: &str) -> Option<&T>` — `egui-0.35.0/src/ui_stack.rs:191`
  Get the value of a tag.
- `fn insert(&mut self, key: impl Into<String>, value: Option<Arc<dyn Any + Send + Sync + 'static>>)` — `egui-0.35.0/src/ui_stack.rs:165`

Implements: `Clone`, `Debug`, `Default`

### `UserData` (struct) — `egui-0.35.0/src/data/user_data.rs:6`

A wrapper around `dyn Any`, used for passing custom user data to [`crate::ViewportCommand::Screenshot`].

Public fields:

- `data: Option<Arc<dyn Any + Send + Sync>>` — A user value given to the screenshot command, that will be returned in [`crate::Event::Sc…

Methods:

- `fn new(user_info: impl Any + Send + Sync) -> Self` — `egui-0.35.0/src/data/user_data.rs:14`
  You can also use [`Self::default`].

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`

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

### `ViewportBuilder` (struct) — `egui-0.35.0/src/viewport.rs:285`

Control the building of a new egui viewport (i.e. native window).

Public fields:

- `title: Option<String>` — The title of the viewport. `eframe` will use this as the title of the native window.
- `app_id: Option<String>` — This is wayland only. See [`Self::with_app_id`].
- `position: Option<Pos2>` — The desired outer position of the window.
- `inner_size: Option<Vec2>`
- `min_inner_size: Option<Vec2>`
- `max_inner_size: Option<Vec2>`
- `clamp_size_to_monitor_size: Option<bool>` — Whether clamp the window's size to monitor's size. The default is `true` on linux, otherw…
- `fullscreen: Option<bool>`
- `maximized: Option<bool>`
- `resizable: Option<bool>`
- `transparent: Option<bool>`
- `decorations: Option<bool>`
- `icon: Option<Arc<IconData>>`
- `active: Option<bool>`
- `visible: Option<bool>`
- `fullsize_content_view: Option<bool>`
- `movable_by_window_background: Option<bool>`
- `title_shown: Option<bool>`
- `titlebar_buttons_shown: Option<bool>`
- `titlebar_shown: Option<bool>`
- `has_shadow: Option<bool>`
- `drag_and_drop: Option<bool>`
- `taskbar: Option<bool>`
- `close_button: Option<bool>`
- `minimize_button: Option<bool>`
- `maximize_button: Option<bool>`
- `window_level: Option<WindowLevel>`
- `mouse_passthrough: Option<bool>`
- `window_type: Option<X11WindowType>`
- `override_redirect: Option<bool>`
- `monitor: Option<usize>` — Target monitor index for borderless fullscreen.

Methods:

- `fn patch(&mut self, new_vp_builder: Self) -> (Vec<ViewportCommand>, bool)` — `egui-0.35.0/src/viewport.rs:713`
  Update this `ViewportBuilder` with a delta, returning a list of commands and a bool indicating if the window…
- `fn with_active(self, active: bool) -> Self` — `egui-0.35.0/src/viewport.rs:450`
  Whether the window will be initially focused or not.
- `fn with_always_on_top(self) -> Self` — `egui-0.35.0/src/viewport.rs:664`
  This window is always on top
- `fn with_app_id(self, app_id: impl Into<String>) -> Self` — `egui-0.35.0/src/viewport.rs:646`
  ### On Wayland On Wayland this sets the Application ID for the window.
- `fn with_clamp_size_to_monitor_size(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:567`
  Sets whether clamp the window's size to monitor's size. The default is `true` on linux, otherwise it is `fals…
- `fn with_close_button(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:574`
  Does not work on X11.
- `fn with_decorations(self, decorations: bool) -> Self` — `egui-0.35.0/src/viewport.rs:367`
  Sets whether the window should have a border, a title bar, etc.
- `fn with_drag_and_drop(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:601`
  On Windows: enable drag and drop support. Drag and drop can not be disabled on other platforms.
- `fn with_fullscreen(self, fullscreen: bool) -> Self` — `egui-0.35.0/src/viewport.rs:379`
  Sets whether the window should be put into fullscreen upon creation.
- `fn with_fullsize_content_view(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:471`
  macOS: Makes the window content appear behind the titlebar.
- `fn with_has_shadow(self, has_shadow: bool) -> Self` — `egui-0.35.0/src/viewport.rs:513`
  macOS: Set to `false` to make the window render without a drop shadow.
- `fn with_icon(self, icon: impl Into<Arc<IconData>>) -> Self` — `egui-0.35.0/src/viewport.rs:435`
  The application icon, e.g. in the Windows task bar or the alt-tab menu.
- `fn with_inner_size(self, size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/viewport.rs:532`
  Requests the window to be of specific dimensions.
- `fn with_max_inner_size(self, size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/viewport.rs:558`
  Sets the maximum dimensions a window can have.
- `fn with_maximize_button(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:588`
  Does not work on X11.
- `fn with_maximized(self, maximized: bool) -> Self` — `egui-0.35.0/src/viewport.rs:390`
  Request that the window is maximized upon creation.
- `fn with_min_inner_size(self, size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/viewport.rs:545`
  Sets the minimum dimensions a window can have.
- `fn with_minimize_button(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:581`
  Does not work on X11.
- `fn with_monitor(self, index: usize) -> Self` — `egui-0.35.0/src/viewport.rs:705`
  Place the window in borderless fullscreen on the monitor at `index`.
- `fn with_mouse_passthrough(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:673`
  On desktop: mouse clicks pass through the window, used for non-interactable overlays.
- `fn with_movable_by_background(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:479`
  macOS: Set to `true` to allow the window to be moved by dragging the background. Enabling this feature can re…
- `fn with_override_redirect(self, value: bool) -> Self` — `egui-0.35.0/src/viewport.rs:691`
  ### On X11 This sets the override-redirect flag. When this is set to true the window type should be specified…
- `fn with_position(self, pos: impl Into<Pos2>) -> Self` — `egui-0.35.0/src/viewport.rs:618`
  The initial "outer" position of the window, i.e. where the top-left corner of the frame/chrome should be.
- `fn with_resizable(self, resizable: bool) -> Self` — `egui-0.35.0/src/viewport.rs:401`
  Sets whether the window is resizable or not.
- `fn with_taskbar(self, show: bool) -> Self` — `egui-0.35.0/src/viewport.rs:520`
  windows: Whether show or hide the window icon in the taskbar.
- `fn with_title(self, title: impl Into<String>) -> Self` — `egui-0.35.0/src/viewport.rs:356`
  Sets the initial title of the window in the title bar.
- `fn with_title_shown(self, title_shown: bool) -> Self` — `egui-0.35.0/src/viewport.rs:486`
  macOS: Set to `false` to hide the window title.
- `fn with_titlebar_buttons_shown(self, titlebar_buttons_shown: bool) -> Self` — `egui-0.35.0/src/viewport.rs:493`
  macOS: Set to `false` to hide the titlebar button (close, minimize, maximize)
- `fn with_titlebar_shown(self, shown: bool) -> Self` — `egui-0.35.0/src/viewport.rs:500`
  macOS: Set to `false` to make the titlebar transparent, allowing the content to appear behind it.
- `fn with_transparent(self, transparent: bool) -> Self` — `egui-0.35.0/src/viewport.rs:425`
  Sets whether the background of the window should be transparent.
- `fn with_visible(self, visible: bool) -> Self` — `egui-0.35.0/src/viewport.rs:461`
  Sets whether the window will be initially visible or hidden.
- `fn with_window_level(self, level: WindowLevel) -> Self` — `egui-0.35.0/src/viewport.rs:655`
  Control if window is always-on-top, always-on-bottom, or neither.
- `fn with_window_type(self, value: X11WindowType) -> Self` — `egui-0.35.0/src/viewport.rs:682`
  ### On X11 This sets the window type. Maps directly to [`_NET_WM_WINDOW_TYPE`](https://specifications.freedes…

Implements: `Clone`, `Debug`, `Default`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `ViewportId` (struct) — `egui-0.35.0/src/viewport.rs:119`

A unique identifier of a viewport.

Methods:

- `fn from_hash_of(source: impl AsId) -> Self` — `egui-0.35.0/src/viewport.rs:153`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `From<ViewportId>`, `Hash`, `IsEnabled`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `ViewportIdPair` (struct) — `egui-0.35.0/src/viewport.rs:240`

A pair of [`ViewportId`], used to identify a viewport and its parent.

Public fields:

- `this: ViewportId`
- `parent: ViewportId`

Methods:

- `fn from_self_and_parent(this: ViewportId, parent: ViewportId) -> Self` — `egui-0.35.0/src/viewport.rs:260`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `Hash`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ViewportInfo` (struct) — `egui-0.35.0/src/data/input/viewport_info.rs:28`

Information about the current viewport, given as input each frame.

Public fields:

- `parent: Option<ViewportId>` — Parent viewport, if known.
- `title: Option<String>` — Name of the viewport, if known.
- `events: Vec<ViewportEvent>`
- `native_pixels_per_point: Option<f32>` — The OS native pixels-per-point.
- `monitor_size: Option<Vec2>` — Current monitor size in egui points.
- `inner_rect: Option<Rect>` — The inner rectangle of the native window, in monitor space and ui points scale.
- `outer_rect: Option<Rect>` — The outer rectangle of the native window, in monitor space and ui points scale.
- `minimized: Option<bool>` — Are we minimized?
- `maximized: Option<bool>` — Are we maximized?
- `fullscreen: Option<bool>` — Are we in fullscreen mode?
- `focused: Option<bool>` — Is the window focused and able to receive input?
- `occluded: Option<bool>` — Is the window fully occluded (completely covered) by another window?

Methods:

- `fn close_requested(&self) -> bool` — `egui-0.35.0/src/data/input/viewport_info.rs:111`
  This viewport has been told to close.
- `fn take(&mut self) -> Self` — `egui-0.35.0/src/data/input/viewport_info.rs:116`
  Helper: move [`Self::events`], clone the other fields.
- `fn ui(&self, ui: &mut Ui)` — `egui-0.35.0/src/data/input/viewport_info.rs:133`
- `fn visible(&self) -> Option<bool>` — `egui-0.35.0/src/data/input/viewport_info.rs:95`
  Is the window considered visible for rendering purposes?

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ViewportOutput` (struct) — `egui-0.35.0/src/viewport.rs:1248`

Describes a viewport, i.e. a native window.

Public fields:

- `parent: ViewportId` — Id of our parent viewport.
- `class: ViewportClass` — What type of viewport are we?
- `builder: ViewportBuilder` — The window attributes such as title, position, size, etc.
- `viewport_ui_cb: Option<Arc<DeferredViewportUiCallback>>` — The user-code that shows the GUI, used for deferred viewports.
- `commands: Vec<ViewportCommand>` — Commands to change the viewport, e.g. window title and size.
- `repaint_delay: Duration` — Schedule a repaint of this viewport after this delay.

Methods:

- `fn append(&mut self, newer: Self)` — `egui-0.35.0/src/viewport.rs:1284`
  Add on new output.

Implements: `Clone`

### `Visuals` (struct) — `egui-0.35.0/src/style.rs:985`

Controls the visual style (colors etc) of egui.

Public fields:

- `dark_mode: bool` — If true, the visuals are overall dark with light text. If false, the visuals are overall…
- `text_options: TextOptions` — Controls how we render text.
- `override_text_color: Option<Color32>` — Override default text color for all text.
- `weak_text_alpha: f32` — How strong "weak" text is.
- `weak_text_color: Option<Color32>` — Color of "weak" text.
- `widgets: Widgets` — Visual styles of widgets
- `selection: Selection`
- `ime_composition: ImeComposition`
- `hyperlink_color: Color32` — The color used for [`crate::Hyperlink`],
- `faint_bg_color: Color32` — Something just barely different from the background color. Used for [`crate::Grid::stripe…
- `extreme_bg_color: Color32` — Very dark or light color (for corresponding theme). Used as the background of text edits,…
- `text_edit_bg_color: Option<Color32>` — The background color of [`crate::TextEdit`].
- `code_bg_color: Color32` — Background color behind code-styled monospaced labels.
- `warn_fg_color: Color32` — A good color for warning text (e.g. orange).
- `error_fg_color: Color32` — A good color for error text (e.g. red).
- `window_corner_radius: CornerRadius`
- `window_shadow: Shadow`
- `window_fill: Color32`
- `window_stroke: Stroke`
- `window_highlight_topmost: bool` — Highlight the topmost window.
- `menu_corner_radius: CornerRadius`
- `panel_fill: Color32` — Panel background color
- `popup_shadow: Shadow`
- `resize_corner_size: f32`
- `text_cursor: TextCursorStyle` — How the text cursor acts.
- `clip_rect_margin: f32` — Allow widgets to paint this much outside the scroll area rect.
- `button_frame: bool` — Show a background behind buttons.
- `collapsing_header_frame: bool` — Show a background behind collapsing headers.
- `indent_has_left_vline: bool` — Draw a vertical line left of indented region, in e.g. [`crate::CollapsingHeader`].
- `striped: bool` — Whether or not Grids and Tables should be striped by default (have alternating rows diffe…
- `slider_trailing_fill: bool` — Show trailing color behind the circle of a [`Slider`]. Default is OFF.
- `handle_shape: HandleShape` — Shape of the handle for sliders and similar widgets.
- `interact_cursor: Option<CursorIcon>` — Should the cursor change when the user hovers over an interactive/clickable item?
- `image_loading_spinners: bool` — Show a spinner when loading an image.
- `numeric_color_space: NumericColorSpace` — How to display numeric color values.
- `disabled_alpha: f32` — How much to modify the alpha of a disabled widget.

Methods:

- `fn dark() -> Self` — `egui-0.35.0/src/style.rs:1490`
  Default dark theme.
- `fn disable(&self, color: Color32) -> Color32` — `egui-0.35.0/src/style.rs:1172`
  Returns a "disabled" version of the given color.
- `fn disabled_alpha(&self) -> f32` — `egui-0.35.0/src/style.rs:1163`
  Disabled widgets have their alpha modified by this.
- `fn gray_out(&self, color: Color32) -> Color32` — `egui-0.35.0/src/style.rs:1179`
  Returns a "grayed out" version of the given color.
- `fn light() -> Self` — `egui-0.35.0/src/style.rs:1557`
  Default light theme.
- `fn noninteractive(&self) -> &WidgetVisuals` — `egui-0.35.0/src/style.rs:1125`
- `fn strong_text_color(&self) -> Color32` — `egui-0.35.0/src/style.rs:1141`
- `fn text_color(&self) -> Color32` — `egui-0.35.0/src/style.rs:1130`
- `fn text_edit_bg_color(&self) -> Color32` — `egui-0.35.0/src/style.rs:1146`
  The background color of [`crate::TextEdit`].
- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:2266`
- `fn weak_text_color(&self) -> Color32` — `egui-0.35.0/src/style.rs:1135`
- `fn window_fill(&self) -> Color32` — `egui-0.35.0/src/style.rs:1152`
  Window background color.
- `fn window_stroke(&self) -> Stroke` — `egui-0.35.0/src/style.rs:1157`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `WidgetInfo` (struct) — `egui-0.35.0/src/data/output.rs:538`

Describes a widget such as a [`crate::Button`] or a [`crate::TextEdit`].

Public fields:

- `typ: WidgetType` — The type of widget this is.
- `enabled: bool` — Whether the widget is enabled.
- `label: Option<String>` — The text on labels, buttons, checkboxes etc.
- `current_text_value: Option<String>` — The contents of some editable text (for [`TextEdit`](crate::TextEdit) fields).
- `prev_text_value: Option<String>` — The previous text value.
- `selected: Option<bool>` — The current value of checkboxes and radio buttons.
- `value: Option<f64>` — The current value of sliders etc.
- `text_selection: Option<Range<CharIndex>>` — Selected range of characters in [`Self::current_text_value`].
- `hint_text: Option<String>` — The hint text for text edit fields.

Methods:

- `fn description(&self) -> String` — `egui-0.35.0/src/data/output.rs:710`
  This can be used by a text-to-speech system to describe the widget.
- `fn drag_value(enabled: bool, value: f64) -> Self` — `egui-0.35.0/src/data/output.rs:652`
- `fn labeled(typ: WidgetType, enabled: bool, label: impl ToString) -> Self` — `egui-0.35.0/src/data/output.rs:633`
- `fn new(typ: WidgetType) -> Self` — `egui-0.35.0/src/data/output.rs:618`
- `fn selected(typ: WidgetType, enabled: bool, selected: bool, label: impl ToString) -> Self` — `egui-0.35.0/src/data/output.rs:643`
  checkboxes, radio-buttons etc
- `fn slider(enabled: bool, value: f64, label: impl ToString) -> Self` — `egui-0.35.0/src/data/output.rs:661`
- `fn text_edit(enabled: bool, prev_text_value: impl ToString, text_value: impl ToString, hint_text: impl ToString) -> Self` — `egui-0.35.0/src/data/output.rs:672`
- `fn text_selection_changed(enabled: bool, text_selection: Range<CharIndex>, current_text_value: impl ToString) -> Self` — `egui-0.35.0/src/data/output.rs:696`

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `WidgetRect` (struct) — `egui-0.35.0/src/widget_rect.rs:9`

Used to store each widget's [Id], [Rect] and [Sense] each frame.

Public fields:

- `id: Id` — The globally unique widget id.
- `parent_id: Id` — The [`Id`] of the parent [`crate::Ui`] that hosts this widget.
- `layer_id: LayerId` — What layer the widget is on.
- `rect: Rect` — The full widget rectangle, in local layer coordinates.
- `interact_rect: Rect` — Where the widget is, in local layer coordinates.
- `sense: Sense` — How the widget responds to interaction.
- `enabled: bool` — Is the widget enabled?

Methods:

- `fn transform(self, transform: TSTransform) -> Self` — `egui-0.35.0/src/widget_rect.rs:52`

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `WidgetRects` (struct) — `egui-0.35.0/src/widget_rect.rs:94`

Stores the [`WidgetRect`]s of all widgets generated during a single egui update/frame.

Methods:

- `fn clear(&mut self)` — `egui-0.35.0/src/widget_rect.rs:148`
  Clear the contents while retaining allocated memory.
- `fn contains(&self, id: Id) -> bool` — `egui-0.35.0/src/widget_rect.rs:137`
- `fn get(&self, id: Id) -> Option<&WidgetRect>` — `egui-0.35.0/src/widget_rect.rs:127`
- `fn get_layer(&self, layer_id: LayerId) -> impl Iterator<Item = &WidgetRect> + '_` — `egui-0.35.0/src/widget_rect.rs:143`
  All widgets in this layer, sorted back-to-front.
- `fn info(&self, id: Id) -> Option<&WidgetInfo>` — `egui-0.35.0/src/widget_rect.rs:230`
- `fn insert(&mut self, layer_id: LayerId, widget_rect: WidgetRect, options: InteractOptions)` — `egui-0.35.0/src/widget_rect.rs:166`
  Insert the given widget rect in the given layer.
- `fn layer_ids(&self) -> impl ExactSizeIterator<Item = LayerId> + '_` — `egui-0.35.0/src/widget_rect.rs:116`
  All known layers with widgets.
- `fn layers(&self) -> impl Iterator<Item = (&LayerId, &[WidgetRect])> + '_` — `egui-0.35.0/src/widget_rect.rs:120`
- `fn order(&self, id: Id) -> Option<(LayerId, usize)>` — `egui-0.35.0/src/widget_rect.rs:132`
  In which layer, and in which order in that layer?
- `fn set_info(&mut self, id: Id, info: WidgetInfo)` — `egui-0.35.0/src/widget_rect.rs:226`

Implements: `Clone`, `Default`, `PartialEq`

### `Window` (struct) — `egui-0.35.0/src/containers/window.rs:82`

Builder for a floating window which can be dragged, closed, collapsed, resized and scrolled (off by default).

Methods:

- `fn anchor(self, align: Align2, offset: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/window.rs:383`
  Set anchor and distance.
- `fn auto_sized(self) -> Self` — `egui-0.35.0/src/containers/window.rs:478`
  Not resizable, just takes the size of its contents. Also disabled scrolling. Text will not wrap, but will ins…
- `fn collapsible(self, collapsible: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:461`
  Can the window be collapsed by clicking on its title?
- `fn constrain(self, constrain: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:344`
  Constrains this window to [`Context::content_rect`].
- `fn constrain_to(self, constrain_rect: Rect) -> Self` — `egui-0.35.0/src/containers/window.rs:353`
  Constrain the movement of the window to the given rectangle.
- `fn current_pos(self, current_pos: impl Into<Pos2>) -> Self` — `egui-0.35.0/src/containers/window.rs:319`
  Set current position of the window. If the window is movable it is up to you to keep track of where it moved…
- `fn default_height(self, default_height: f32) -> Self` — `egui-0.35.0/src/containers/window.rs:417`
  Set initial height of the window.
- `fn default_open(self, default_open: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:390`
  Set initial collapsed state of the window
- `fn default_pos(self, default_pos: impl Into<Pos2>) -> Self` — `egui-0.35.0/src/containers/window.rs:326`
  Set initial position of the window.
- `fn default_rect(self, rect: Rect) -> Self` — `egui-0.35.0/src/containers/window.rs:434`
  Set initial position and size of the window.
- `fn default_size(self, default_size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/window.rs:400`
  Set initial size of the window.
- `fn default_width(self, default_width: f32) -> Self` — `egui-0.35.0/src/containers/window.rs:409`
  Set initial width of the window.
- `fn drag_area(self, drag_area: WindowDrag) -> Self` — `egui-0.35.0/src/containers/window.rs:213`
  Where the user can grab the window to move it.
- `fn drag_to_scroll(self, drag_to_scroll: DragScroll) -> Self` — `egui-0.35.0/src/containers/window.rs:514`
  Controls scrolling the window by dragging the contents with the pointer.
- `fn enabled(self, enabled: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:178`
  If `false` the window will be grayed out and non-interactive.
- `fn fade_in(self, fade_in: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:229`
  If `true`, quickly fade in the `Window` when it first appears.
- `fn fade_out(self, fade_out: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:240`
  If `true`, quickly fade out the `Window` when it closes.
- `fn fixed_pos(self, pos: impl Into<Pos2>) -> Self` — `egui-0.35.0/src/containers/window.rs:333`
  Sets the window position and prevents it from being dragged around.
- `fn fixed_rect(self, rect: Rect) -> Self` — `egui-0.35.0/src/containers/window.rs:439`
  Sets the window pos and size and prevents it from being moved and resized by dragging its edges.
- `fn fixed_size(self, size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/window.rs:428`
  Sets the window size and prevents it from being resized by dragging its edges.
- `fn frame(self, frame: Frame) -> Self` — `egui-0.35.0/src/containers/window.rs:263`
  Change the background color, margins, etc.
- `fn from_viewport(id: ViewportId, viewport: ViewportBuilder) -> Self` — `egui-0.35.0/src/containers/window.rs:124`
  Construct a [`Window`] that follows the given viewport.
- `fn hscroll(self, hscroll: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:496`
  Enable/disable horizontal scrolling. `false` by default.
- `fn id(self, id: Id) -> Self` — `egui-0.35.0/src/containers/window.rs:160`
  Assign a unique id to the Window. Required if the title changes, or is shared with another window.
- `fn interactable(self, interactable: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:189`
  If false, clicks goes straight through to what is behind us.
- `fn max_height(self, max_height: f32) -> Self` — `egui-0.35.0/src/containers/window.rs:301`
  Set maximum height of the window.
- `fn max_size(self, max_size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/window.rs:311`
  Set maximum size of the window, equivalent to calling both `max_width` and `max_height`.
- `fn max_width(self, max_width: f32) -> Self` — `egui-0.35.0/src/containers/window.rs:294`
  Set maximum width of the window.
- `fn min_height(self, min_height: f32) -> Self` — `egui-0.35.0/src/containers/window.rs:277`
  Set minimum height of the window.
- `fn min_size(self, min_size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/containers/window.rs:287`
  Set minimum size of the window, equivalent to calling both `min_width` and `min_height`.
- `fn min_width(self, min_width: f32) -> Self` — `egui-0.35.0/src/containers/window.rs:270`
  Set minimum width of the window.
- `fn movable(self, movable: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:199`
  If `false` the window will be immovable.
- `fn mutate(self, mutate: impl Fn(&mut Self)) -> Self` — `egui-0.35.0/src/containers/window.rs:248`
  Usage: `Window::new(…).mutate(|w| w.resize = w.resize.auto_expand_width(true))`
- `fn new(title: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/containers/window.rs:101`
  The window title is used as a unique [`Id`] and must be unique, and should not change. This is true even if y…
- `fn open(self, open: &'a mut bool) -> Self` — `egui-0.35.0/src/containers/window.rs:171`
  Call this to add a close-button to the window title bar.
- `fn order(self, order: Order) -> Self` — `egui-0.35.0/src/containers/window.rs:220`
  `order(Order::Foreground)` for a Window that should always be on top
- `fn pivot(self, pivot: Align2) -> Self` — `egui-0.35.0/src/containers/window.rs:366`
  Where the "root" of the window is.
- `fn resizable(self, resizable: impl Into<Vec2b>) -> Self` — `egui-0.35.0/src/containers/window.rs:453`
  Can the user resize the window by dragging its edges?
- `fn resize(self, mutate: impl Fn(Resize) -> Resize) -> Self` — `egui-0.35.0/src/containers/window.rs:256`
  Usage: `Window::new(…).resize(|r| r.auto_expand_width(true))`
- `fn scroll(self, scroll: impl Into<Vec2b>) -> Self` — `egui-0.35.0/src/containers/window.rs:489`
  Enable/disable horizontal/vertical scrolling. `false` by default.
- `fn scroll_bar_visibility(self, visibility: ScrollBarVisibility) -> Self` — `egui-0.35.0/src/containers/window.rs:524`
  Sets the [`ScrollBarVisibility`] of the window.
- `fn show<R>(self, ctx: &Context, add_contents: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<Option<R>>>` — `egui-0.35.0/src/containers/window.rs:534`
  Returns `None` if the window is not open (if [`Window::open`] was called with `&mut false`). Returns `Some(In…
- `fn title_bar(self, title_bar: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:469`
  Show title bar on top of the window? If `false`, the window will not be collapsible nor have a close-button.
- `fn vscroll(self, vscroll: bool) -> Self` — `egui-0.35.0/src/containers/window.rs:503`
  Enable/disable vertical scrolling. `false` by default.

### `AsId` (trait) — `egui-0.35.0/src/id.rs:11`

Types that can be converted to an [`Id`].

Required/provided items:


### `AsIdSalt` (trait) — `egui-0.35.0/src/id_salt.rs:7`

Types that can be converted to an [`IdSalt`].

Required/provided items:


### `AtomExt` (trait) — `egui-0.35.0/src/atomics/atom_ext.rs:7`

A trait for conveniently building [`Atom`]s.

Required/provided items:

- `fn atom_id(self, id: Id) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:12`
  Set the [`Id`] for custom rendering.
- `fn atom_size(self, size: Vec2) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:23`
  Set the atom to a fixed size.
- `fn atom_grow(self, grow: bool) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:32`
  Grow this atom to the available space.
- `fn atom_shrink(self, shrink: bool) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:43`
  Shrink this atom if there isn't enough space.
- `fn atom_max_size(self, max_size: Vec2) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:49`
  Set the maximum size of this atom.
- `fn atom_max_width(self, max_width: f32) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:55`
  Set the maximum width of this atom.
- `fn atom_max_height(self, max_height: f32) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:58`
  Set the maximum height of this atom.
- `fn atom_max_height_font_size(self, ui: &Ui) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:63`
  Set the max height of this atom to match the font size.
- `fn atom_align(self, align: Align2) -> Atom<'a>` — `egui-0.35.0/src/atomics/atom_ext.rs:76`
  Sets the [`emath::Align2`] of a single atom within its available space.

### `IntoAtoms` (trait) — `egui-0.35.0/src/atomics/atoms.rs:209`

Trait for turning a tuple of [`Atom`]s into [`Atoms`].

Required/provided items:

- `fn collect(self, atoms: &mut Atoms<'a>)` — `egui-0.35.0/src/atomics/atoms.rs:210`
- `fn into_atoms(self) -> Atoms<'a>` — `egui-0.35.0/src/atomics/atoms.rs:212`

### `NumExt` (trait) — `emath-0.35.0/src/lib.rs:321`

Extends `f32`, [`Vec2`] etc with `at_least` and `at_most` as aliases for `max` and `min`.

Required/provided items:

- `fn at_least(self, lower_limit: Self) -> Self` — `emath-0.35.0/src/lib.rs:324`
  More readable version of `self.max(lower_limit)`
- `fn at_most(self, upper_limit: Self) -> Self` — `emath-0.35.0/src/lib.rs:328`
  More readable version of `self.min(upper_limit)`

### `Plugin` (trait) — `egui-0.35.0/src/plugin.rs:13`

A plugin to extend egui.

Required/provided items:

- `fn debug_name(&self) -> &'static str` — `egui-0.35.0/src/plugin.rs:17`
  Plugin name.
- `fn setup(&mut self, ctx: &Context)` — `egui-0.35.0/src/plugin.rs:22`
  Called once, when the plugin is registered.
- `fn on_begin_pass(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/plugin.rs:27`
  Called at the start of each pass.
- `fn on_end_pass(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/plugin.rs:32`
  Called at the end of each pass.
- `fn input_hook(&mut self, ctx: &Context, input: &mut RawInput)` — `egui-0.35.0/src/plugin.rs:38`
  Called just before the input is processed.
- `fn output_hook(&mut self, ctx: &Context, output: &mut FullOutput)` — `egui-0.35.0/src/plugin.rs:44`
  Called just before the output is passed to the backend.
- `fn on_widget_under_pointer(&mut self, ctx: &Context, widget: &WidgetRect)` — `egui-0.35.0/src/plugin.rs:51`
  Called when a widget is created and is under the pointer.

### `TextBuffer` (trait) — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:25`

Trait constraining what types [`crate::TextEdit`] may use as an underlying buffer.

Required/provided items:

- `fn is_mutable(&self) -> bool` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:27`
  Can this text be edited?
- `fn as_str(&self) -> &str` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:30`
  Returns this buffer as a `str`.
- `fn insert_text(&mut self, text: &str, char_index: CharIndex) -> usize` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:39`
  Inserts text `text` into this buffer at character index `char_index`.
- `fn delete_char_range(&mut self, char_range: Range<CharIndex>)` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:45`
  Deletes a range of text `char_range` from this buffer.
- `fn char_range(&self, char_range: Range<CharIndex>) -> &str` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:48`
  Reads the given character range.
- `fn byte_index_from_char_index(&self, char_index: CharIndex) -> ByteIndex` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:52`
- `fn char_index_from_byte_index(&self, byte_index: ByteIndex) -> CharIndex` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:56`
- `fn clear(&mut self)` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:61`
  Clears all characters in this buffer
- `fn replace_with(&mut self, text: &str)` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:66`
  Replaces all contents of this string with `text`
- `fn take(&mut self) -> String` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:72`
  Clears all characters in this buffer and returns a string of the contents.
- `fn insert_text_at(&mut self, ccursor: &mut CCursor, text_to_insert: &str, char_limit: usize)` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:78`
- `fn decrease_indentation(&mut self, ccursor: &mut CCursor)` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:95`
- `fn delete_selected(&mut self, cursor_range: &CCursorRange) -> CCursor` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:120`
- `fn delete_selected_ccursor_range(&mut self, [min, max]: [CCursor; 2]) -> CCursor` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:125`
- `fn delete_previous_char(&mut self, ccursor: CCursor) -> CCursor` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:133`
- `fn delete_next_char(&mut self, ccursor: CCursor) -> CCursor` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:143`
- `fn delete_previous_word(&mut self, max_ccursor: CCursor) -> CCursor` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:147`
- `fn delete_next_word(&mut self, min_ccursor: CCursor) -> CCursor` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:152`
- `fn delete_paragraph_before_cursor(&mut self, galley: &Galley, cursor_range: &CCursorRange) -> CCursor` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:157`
- `fn delete_paragraph_after_cursor(&mut self, galley: &Galley, cursor_range: &CCursorRange) -> CCursor` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:171`
- `fn type_id(&self) -> TypeId` — `egui-0.35.0/src/widgets/text_edit/text_buffer.rs:216`
  Returns a unique identifier for the implementing type.

### `Widget` (trait) — `egui-0.35.0/src/widgets/mod.rs:57`

Anything implementing Widget can be added to a [`Ui`] with [`Ui::add`].

Required/provided items:

- `fn ui(self, ui: &mut Ui) -> Response` — `egui-0.35.0/src/widgets/mod.rs:65`
  Allocate space, interact, paint, and return a [`Response`].

### `WidgetWithState` (trait) — `egui-0.35.0/src/widgets/mod.rs:94`

Helper so that you can do e.g. `TextEdit::State::load`.

Required/provided items:


### `AtomClosure` (type_alias) — `egui-0.35.0/src/atomics/atom_kind.rs:22`

See [`AtomKind::Closure`]

### `DeferredViewportUiCallback` (type_alias) — `egui-0.35.0/src/viewport.rs:266`

The user-code that shows the ui in the viewport, used for deferred viewports.

### `IconPainter` (type_alias) — `egui-0.35.0/src/containers/combo_box.rs:15`

A function that paints the [`ComboBox`] icon

### `IdMap` (type_alias) — `egui-0.35.0/src/id.rs:163`

`IdMap<V>` is a `HashMap<Id, V>` optimized by knowing that [`Id`] has good entropy, and doesn't need more hashing.

### `IdSet` (type_alias) — `egui-0.35.0/src/id.rs:160`

`IdSet` is a `HashSet<Id>` optimized by knowing that [`Id`] has good entropy, and doesn't need more hashing.

### `ImmediateViewportRendererCallback` (type_alias) — `egui-0.35.0/src/viewport.rs:269`

Render the given viewport, calling the given ui callback.

### `OrderedViewportIdMap` (type_alias) — `egui-0.35.0/src/viewport.rs:174`

An order map from [`ViewportId`] to `T`.

### `ViewportIdMap` (type_alias) — `egui-0.35.0/src/viewport.rs:171`

A fast hash map from [`ViewportId`] to `T`.

### `ViewportIdSet` (type_alias) — `egui-0.35.0/src/viewport.rs:168`

A fast hash set of [`ViewportId`].


## `egui::cache`

### `CacheStorage` (struct) — `egui-0.35.0/src/cache/cache_storage.rs:25`

A typemap of many caches, all implemented with [`CacheTrait`].

Methods:

- `fn cache<Cache>(&mut self) -> &mut Cache` — `egui-0.35.0/src/cache/cache_storage.rs:30`
- `fn update(&mut self)` — `egui-0.35.0/src/cache/cache_storage.rs:48`
  Call once per frame to evict cache.

Implements: `Clone`, `Debug`, `Default`

### `FrameCache` (struct) — `egui-0.35.0/src/cache/frame_cache.rs:12`

Caches the results of a computation for one frame. If it is still used next frame, it is not recomputed. If it is not used next frame, it is evicted from the cache to sa…

Methods:

- `fn evict_cache(&mut self)` — `egui-0.35.0/src/cache/frame_cache.rs:37`
  Must be called once per frame to clear the cache.
- `fn get<Key>(&mut self, key: Key) -> &Value` — `egui-0.35.0/src/cache/frame_cache.rs:49`
  Get from cache (if the same key was used last frame) or recompute and store in the cache.
- `fn new(computer: Computer) -> Self` — `egui-0.35.0/src/cache/frame_cache.rs:28`

Implements: `CacheTrait`, `Default`

### `FramePublisher` (struct) — `egui-0.35.0/src/cache/frame_publisher.rs:6`

Stores a key:value pair for the duration of this frame and the next.

Methods:

- `fn evict_cache(&mut self)` — `egui-0.35.0/src/cache/frame_publisher.rs:36`
  Must be called once per frame to clear the cache.
- `fn get(&self, key: &Key) -> Option<&Value>` — `egui-0.35.0/src/cache/frame_publisher.rs:31`
  Retrieve a value if it was published this or the previous frame.
- `fn new() -> Self` — `egui-0.35.0/src/cache/frame_publisher.rs:18`
- `fn set(&mut self, key: Key, value: Value)` — `egui-0.35.0/src/cache/frame_publisher.rs:26`
  Publish the value. It will be available for the duration of this and the next frame.

Implements: `CacheTrait`, `Default`

### `CacheTrait` (trait) — `egui-0.35.0/src/cache/cache_trait.rs:3`

A cache, storing some value for some length of time.

Required/provided items:

- `fn update(&mut self)` — `egui-0.35.0/src/cache/cache_trait.rs:5`
  Call once per frame to evict cache.
- `fn len(&self) -> usize` — `egui-0.35.0/src/cache/cache_trait.rs:8`
  Number of values currently in the cache.

### `ComputerMut` (trait) — `egui-0.35.0/src/cache/frame_cache.rs:5`

Something that does an expensive computation that we want to cache to save us from recomputing it each frame.

Required/provided items:

- `fn compute(&mut self, key: Key) -> Value` — `egui-0.35.0/src/cache/frame_cache.rs:6`


## `egui::collapsing_header`

### `paint_default_icon` — `egui-0.35.0/src/containers/collapsing_header.rs:336`

```rust
fn paint_default_icon(ui: &mut Ui, openness: f32, response: &Response)
```

Paint the arrow icon that indicated if the region is open or not

### `CollapsingState` (struct) — `egui-0.35.0/src/containers/collapsing_header.rs:25`

This is a a building block for building collapsing regions.

Methods:

- `fn id(&self) -> Id` — `egui-0.35.0/src/containers/collapsing_header.rs:46`
- `fn is_open(&self) -> bool` — `egui-0.35.0/src/containers/collapsing_header.rs:60`
- `fn load(ctx: &Context, id: Id) -> Option<Self>` — `egui-0.35.0/src/containers/collapsing_header.rs:31`
- `fn load_with_default_open(ctx: &Context, id: Id, default_open: bool) -> Self` — `egui-0.35.0/src/containers/collapsing_header.rs:50`
- `fn openness(&self, ctx: &Context) -> f32` — `egui-0.35.0/src/containers/collapsing_header.rs:74`
  0 for closed, 1 for open, with tweening
- `fn remove(&self, ctx: &Context)` — `egui-0.35.0/src/containers/collapsing_header.rs:42`
- `fn set_open(&mut self, open: bool)` — `egui-0.35.0/src/containers/collapsing_header.rs:64`
- `fn show_body_indented<R>(&mut self, header_response: &Response, ui: &mut Ui, add_body: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<R>>` — `egui-0.35.0/src/containers/collapsing_header.rs:156`
  Show body if we are open, with a nice animation between closed and open. Indent the body to show it belongs t…
- `fn show_body_unindented<R>(&mut self, ui: &mut Ui, add_body: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<R>>` — `egui-0.35.0/src/containers/collapsing_header.rs:175`
  Show body if we are open, with a nice animation between closed and open. Will also store the state.
- `fn show_header<HeaderRet>(self, ui: &mut Ui, add_header: impl FnOnce(&mut Ui) -> HeaderRet) -> HeaderResponse<'_, HeaderRet>` — `egui-0.35.0/src/containers/collapsing_header.rs:129`
  Shows header and body (if expanded).
- `fn show_toggle_button(&mut self, ui: &mut Ui, icon_fn: impl FnOnce(&mut Ui, f32, &Response) + 'static) -> Response` — `egui-0.35.0/src/containers/collapsing_header.rs:265`
  Paint this [`CollapsingState`]'s toggle button. Takes an [`IconPainter`] as the icon. ``` # egui::__run_test_…
- `fn store(&self, ctx: &Context)` — `egui-0.35.0/src/containers/collapsing_header.rs:38`
- `fn toggle(&mut self, ui: &Ui)` — `egui-0.35.0/src/containers/collapsing_header.rs:68`

Implements: `Clone`, `Debug`

### `HeaderResponse` (struct) — `egui-0.35.0/src/containers/collapsing_header.rs:276`

From [`CollapsingState::show_header`].

Methods:

- `fn body<BodyRet>(self, add_body: impl FnOnce(&mut Ui) -> BodyRet) -> (Response, InnerResponse<HeaderRet>, Option<InnerResponse<BodyRet>>)` — `egui-0.35.0/src/containers/collapsing_header.rs:297`
  Returns the response of the collapsing button, the custom header, and the custom body.
- `fn body_unindented<BodyRet>(self, add_body: impl FnOnce(&mut Ui) -> BodyRet) -> (Response, InnerResponse<HeaderRet>, Option<InnerResponse<BodyRet>>)` — `egui-0.35.0/src/containers/collapsing_header.rs:316`
  Returns the response of the collapsing button, the custom header, and the custom body, without indentation.
- `fn is_open(&self) -> bool` — `egui-0.35.0/src/containers/collapsing_header.rs:284`
- `fn set_open(&mut self, open: bool)` — `egui-0.35.0/src/containers/collapsing_header.rs:288`
- `fn toggle(&mut self)` — `egui-0.35.0/src/containers/collapsing_header.rs:292`

### `IconPainter` (type_alias) — `egui-0.35.0/src/containers/collapsing_header.rs:359`

A function that paints an icon indicating if the region is open or not


## `egui::color_picker`

### `Alpha` (enum) — `egui-0.35.0/src/widgets/color_picker.rs:267`

What options to show for alpha

Variants:

- `Alpha::Opaque` — Set alpha to 1.0, and show no option for it.
- `Alpha::OnlyBlend` — Only show normal blend options for alpha.
- `Alpha::BlendOrAdditive` — Show both blend and additive options.

Implements: `Clone`, `Copy`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `color_edit_button_hsva` — `egui-0.35.0/src/widgets/color_picker.rs:518`

```rust
fn color_edit_button_hsva(ui: &mut Ui, hsva: &mut Hsva, alpha: Alpha) -> Response
```

### `color_edit_button_rgb` — `egui-0.35.0/src/widgets/color_picker.rs:575`

```rust
fn color_edit_button_rgb(ui: &mut Ui, rgb: &mut [f32; 3]) -> Response
```

Shows a button with the given color. If the user clicks the button, a full color picker is shown.

### `color_edit_button_rgba` — `egui-0.35.0/src/widgets/color_picker.rs:565`

```rust
fn color_edit_button_rgba(ui: &mut Ui, rgba: &mut Rgba, alpha: Alpha) -> Response
```

Shows a button with the given color. If the user clicks the button, a full color picker is shown.

### `color_edit_button_srgb` — `egui-0.35.0/src/widgets/color_picker.rs:554`

```rust
fn color_edit_button_srgb(ui: &mut Ui, srgb: &mut [u8; 3]) -> Response
```

Shows a button with the given color. If the user clicks the button, a full color picker is shown. The given color is in `sRGB` space.

### `color_edit_button_srgba` — `egui-0.35.0/src/widgets/color_picker.rs:543`

```rust
fn color_edit_button_srgba(ui: &mut Ui, srgba: &mut Color32, alpha: Alpha) -> Response
```

Shows a button with the given color. If the user clicks the button, a full color picker is shown.

### `color_picker_color32` — `egui-0.35.0/src/widgets/color_picker.rs:510`

```rust
fn color_picker_color32(ui: &mut Ui, srgba: &mut Color32, alpha: Alpha) -> bool
```

Shows a color picker where the user can change the given [`Color32`] color.

### `color_picker_hsva_2d` — `egui-0.35.0/src/widgets/color_picker.rs:493`

```rust
fn color_picker_hsva_2d(ui: &mut Ui, hsva: &mut Hsva, alpha: Alpha) -> bool
```

Shows a color picker where the user can change the given [`Hsva`] color.

### `show_color` — `egui-0.35.0/src/widgets/color_picker.rs:57`

```rust
fn show_color(ui: &mut Ui, color: impl Into<Color32>, desired_size: Vec2) -> Response
```

Show a color with background checkers to demonstrate transparency (if any).

### `show_color_at` — `egui-0.35.0/src/widgets/color_picker.rs:70`

```rust
fn show_color_at(painter: &Painter, color: Color32, rect: Rect)
```

Show a color with background checkers to demonstrate transparency (if any).


## `egui::debug_text`

### `print` — `egui-0.35.0/src/debug_text.rs:24`

```rust
fn print(ctx: &Context, text: impl Into<WidgetText>)
```

Print this text next to the cursor at the end of the pass.

### `DebugTextPlugin` (struct) — `egui-0.35.0/src/debug_text.rs:50`

A plugin for easily showing debug-text on-screen.

Implements: `Clone`, `Default`, `Plugin`


## `egui::frame`

### `Prepared` (struct) — `egui-0.35.0/src/containers/frame.rs:357`

Public fields:

- `frame: Frame` — The frame that was prepared.
- `content_ui: Ui` — Add your widgets to this UI so it ends up within the frame.

Methods:

- `fn allocate_space(&self, ui: &mut Ui) -> Response` — `egui-0.35.0/src/containers/frame.rs:466`
  Allocate the space that was used by [`Self::content_ui`].
- `fn end(self, ui: &mut Ui) -> Response` — `egui-0.35.0/src/containers/frame.rs:486`
  Convenience for calling [`Self::allocate_space`] and [`Self::paint`].
- `fn paint(&self, ui: &Ui)` — `egui-0.35.0/src/containers/frame.rs:473`
  Paint the frame.


## `egui::gui_zoom`

### `zoom_in` — `egui-0.35.0/src/gui_zoom.rs:52`

```rust
fn zoom_in(ctx: &Context)
```

Make everything larger by increasing [`Context::zoom_factor`].

### `zoom_menu_buttons` — `egui-0.35.0/src/gui_zoom.rs:72`

```rust
fn zoom_menu_buttons(ui: &mut Ui)
```

Show buttons for zooming the ui.

### `zoom_out` — `egui-0.35.0/src/gui_zoom.rs:61`

```rust
fn zoom_out(ctx: &Context)
```

Make everything smaller by decreasing [`Context::zoom_factor`].


## `egui::introspection`

### `font_family_ui` — `egui-0.35.0/src/introspection.rs:7`

```rust
fn font_family_ui(ui: &mut Ui, font_family: &mut FontFamily)
```

### `font_id_ui` — `egui-0.35.0/src/introspection.rs:17`

```rust
fn font_id_ui(ui: &mut Ui, font_id: &mut FontId)
```


## `egui::layers`

### `GraphicLayers` (struct) — `egui-0.35.0/src/layers.rs:193`

This is where painted [`Shape`]s end up during a frame.

Methods:

- `fn drain(&mut self, area_order: &[LayerId], to_global: &HashMap<LayerId, TSTransform>) -> Vec<ClippedShape>` — `egui-0.35.0/src/layers.rs:213`
- `fn entry(&mut self, layer_id: LayerId) -> &mut PaintList` — `egui-0.35.0/src/layers.rs:197`
  Get or insert the [`PaintList`] for the given [`LayerId`].
- `fn get(&self, layer_id: LayerId) -> Option<&PaintList>` — `egui-0.35.0/src/layers.rs:204`
  Get the [`PaintList`] for the given [`LayerId`].
- `fn get_mut(&mut self, layer_id: LayerId) -> Option<&mut PaintList>` — `egui-0.35.0/src/layers.rs:209`
  Get the [`PaintList`] for the given [`LayerId`].

Implements: `Clone`, `Default`

### `PaintList` (struct) — `egui-0.35.0/src/layers.rs:113`

A list of [`Shape`]s paired with a clip rectangle.

Methods:

- `fn add(&mut self, clip_rect: Rect, shape: Shape) -> ShapeIdx` — `egui-0.35.0/src/layers.rs:127`
  Returns the index of the new [`Shape`] that can be used with `PaintList::set`.
- `fn all_entries(&self) -> impl ExactSizeIterator<Item = &ClippedShape>` — `egui-0.35.0/src/layers.rs:186`
  Read-only access to all held shapes.
- `fn extend<I>(&mut self, clip_rect: Rect, shapes: I)` — `egui-0.35.0/src/layers.rs:133`
- `fn is_empty(&self) -> bool` — `egui-0.35.0/src/layers.rs:117`
- `fn mutate_shape(&mut self, idx: ShapeIdx, f: impl FnOnce(&mut ClippedShape))` — `egui-0.35.0/src/layers.rs:165`
  Mutate the shape at the given index, if any.
- `fn next_idx(&self) -> ShapeIdx` — `egui-0.35.0/src/layers.rs:121`
- `fn reset_shape(&mut self, idx: ShapeIdx)` — `egui-0.35.0/src/layers.rs:160`
  Set the given shape to be empty (a `Shape::Noop`).
- `fn set(&mut self, idx: ShapeIdx, clip_rect: Rect, shape: Shape)` — `egui-0.35.0/src/layers.rs:149`
  Modify an existing [`Shape`].
- `fn transform(&mut self, transform: TSTransform)` — `egui-0.35.0/src/layers.rs:170`
  Transform each [`Shape`] and clip rectangle by this much, in-place
- `fn transform_range(&mut self, start: ShapeIdx, end: ShapeIdx, transform: TSTransform)` — `egui-0.35.0/src/layers.rs:178`
  Transform each [`Shape`] and clip rectangle in range by this much, in-place

Implements: `Clone`, `Default`

### `ShapeIdx` (struct) — `egui-0.35.0/src/layers.rs:109`

A unique identifier of a specific [`Shape`] in a [`PaintList`].

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `PartialEq`, `StructuralPartialEq`


## `egui::load`

### `Bytes` (enum) — `egui-0.35.0/src/load.rs:206`

Represents a byte buffer.

Variants:

- `Bytes::Static`
- `Bytes::Shared`

Implements: `AsRef<[u8]>`, `Clone`, `Debug`, `Deref`, `From<&'static [u8; N]>`, `From<&'static [u8]>`, `From<Arc<[u8]>>`, `From<Vec<u8>>`

### `BytesPoll` (enum) — `egui-0.35.0/src/load.rs:273`

Represents bytes which are currently being loaded.

Variants:

- `BytesPoll::Pending` — Bytes are being loaded.
- `BytesPoll::Ready` — Bytes are loaded.

Implements: `Clone`

### `ImagePoll` (enum) — `egui-0.35.0/src/load.rs:372`

Represents an image which is currently being loaded.

Variants:

- `ImagePoll::Pending` — Image is loading.
- `ImagePoll::Ready` — Image is loaded.

Implements: `Clone`

### `LoadError` (enum) — `egui-0.35.0/src/load.rs:76`

Represents a failed attempt at loading an image.

Variants:

- `LoadError::NoImageLoaders` — Programmer error: There are no image loaders installed.
- `LoadError::NotSupported` — A specific loader does not support this scheme or protocol.
- `LoadError::FormatNotSupported` — A specific loader does not support the format of the image.
- `LoadError::NoMatchingBytesLoader` — Programmer error: Failed to find the bytes for this image because there was no [`BytesLoader`] supp…
- `LoadError::NoMatchingImageLoader` — Programmer error: Failed to parse the bytes as an image because there was no [`ImageLoader`] suppor…
- `LoadError::NoMatchingTextureLoader` — Programmer error: no matching [`TextureLoader`]. Because of the [`DefaultTextureLoader`], this erro…
- `LoadError::Loading` — Runtime error: Loading was attempted, but failed (e.g. "File not found").

Methods:

- `fn byte_size(&self) -> usize` — `egui-0.35.0/src/load.rs:104`
  Returns the (approximate) size of the error message in bytes.

Implements: `Clone`, `Debug`, `Display`, `Eq`, `Error`, `PartialEq`, `StructuralPartialEq`

### `TexturePoll` (enum) — `egui-0.35.0/src/load.rs:490`

Represents a texture is currently being loaded.

Variants:

- `TexturePoll::Pending` — Texture is loading.
- `TexturePoll::Ready` — Texture is loaded.

Methods:

- `fn is_pending(&self) -> bool` — `egui-0.35.0/src/load.rs:522`
- `fn is_ready(&self) -> bool` — `egui-0.35.0/src/load.rs:527`
- `fn size(&self) -> Option<Vec2>` — `egui-0.35.0/src/load.rs:506`
  Point size of the original SVG, or the size of the image in texels.
- `fn texture_id(&self) -> Option<TextureId>` — `egui-0.35.0/src/load.rs:514`

Implements: `Clone`, `Copy`

### `DefaultBytesLoader` (struct) — `egui-0.35.0/src/load/bytes_loader.rs:10`

Maps URI:s to [`Bytes`], e.g. found with `include_bytes!`.

Methods:

- `fn insert(&self, uri: impl Into<Cow<'static, str>>, bytes: impl Into<Bytes>)` — `egui-0.35.0/src/load/bytes_loader.rs:15`

Implements: `BytesLoader`, `Default`

### `DefaultTextureLoader` (struct) — `egui-0.35.0/src/load/texture_loader.rs:33`

Implements: `Default`, `TextureLoader`

### `Loaders` (struct) — `egui-0.35.0/src/load.rs:601`

The loaders of bytes, images, and textures.

Public fields:

- `include: Arc<DefaultBytesLoader>`
- `bytes: Mutex<Vec<Arc<dyn BytesLoader + Send + Sync + 'static>>>`
- `image: Mutex<Vec<Arc<dyn ImageLoader + Send + Sync + 'static>>>`
- `texture: Mutex<Vec<Arc<dyn TextureLoader + Send + Sync + 'static>>>`

Methods:

- `fn end_pass(&self, pass_index: u64)` — `egui-0.35.0/src/load.rs:623`
  The given pass has just ended.

Implements: `Clone`, `Default`

### `SizedTexture` (struct) — `egui-0.35.0/src/load.rs:444`

A texture with a known size.

Public fields:

- `id: TextureId`
- `size: Vec2` — Point size of the original SVG, or the size of the image in texels.

Methods:

- `fn from_handle(handle: &TextureHandle) -> Self` — `egui-0.35.0/src/load.rs:461`
  Fetch the [id][`SizedTexture::id`] and [size][`SizedTexture::size`] from a [`TextureHandle`].
- `fn new(id: impl Into<TextureId>, size: impl Into<Vec2>) -> Self` — `egui-0.35.0/src/load.rs:453`
  Create a [`SizedTexture`] from a texture `id` with a specific `size`.

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `From<&'a TextureHandle>`, `From<(TextureId, Vec2)>`, `PartialEq`, `StructuralPartialEq`

### `BytesLoader` (trait) — `egui-0.35.0/src/load.rs:319`

Represents a loader capable of loading raw unstructured bytes from somewhere, e.g. from disk or network.

Required/provided items:

- `fn id(&self) -> &str` — `egui-0.35.0/src/load.rs:323`
  Unique ID of this loader.
- `fn load(&self, ctx: &Context, uri: &str) -> BytesLoadResult` — `egui-0.35.0/src/load.rs:337`
  Try loading the bytes from the given uri.
- `fn forget(&self, uri: &str)` — `egui-0.35.0/src/load.rs:343`
  Forget the given `uri`.
- `fn forget_all(&self)` — `egui-0.35.0/src/load.rs:349`
  Forget all URIs ever given to this loader.
- `fn end_pass(&self, pass_index: u64)` — `egui-0.35.0/src/load.rs:353`
  Implementations may use this to perform work at the end of a frame, such as evicting unused entries from a ca…
- `fn byte_size(&self) -> usize` — `egui-0.35.0/src/load.rs:358`
  If the loader caches any data, this should return the size of that cache.
- `fn has_pending(&self) -> bool` — `egui-0.35.0/src/load.rs:361`
  Returns `true` if some data is currently being loaded.

### `ImageLoader` (trait) — `egui-0.35.0/src/load.rs:390`

An `ImageLoader` decodes raw bytes into a [`ColorImage`].

Required/provided items:

- `fn id(&self) -> &str` — `egui-0.35.0/src/load.rs:397`
  Unique ID of this loader.
- `fn load(&self, ctx: &Context, uri: &str, size_hint: SizeHint) -> ImageLoadResult` — `egui-0.35.0/src/load.rs:411`
  Try loading the image from the given uri.
- `fn forget(&self, uri: &str)` — `egui-0.35.0/src/load.rs:417`
  Forget the given `uri`.
- `fn forget_all(&self)` — `egui-0.35.0/src/load.rs:423`
  Forget all URIs ever given to this loader.
- `fn end_pass(&self, pass_index: u64)` — `egui-0.35.0/src/load.rs:427`
  Implementations may use this to perform work at the end of a pass, such as evicting unused entries from a cac…
- `fn byte_size(&self) -> usize` — `egui-0.35.0/src/load.rs:432`
  If the loader caches any data, this should return the size of that cache.
- `fn has_pending(&self) -> bool` — `egui-0.35.0/src/load.rs:437`
  Returns `true` if some image is currently being loaded.

### `TextureLoader` (trait) — `egui-0.35.0/src/load.rs:544`

A `TextureLoader` uploads a [`ColorImage`] to the GPU, returning a [`SizedTexture`].

Required/provided items:

- `fn id(&self) -> &str` — `egui-0.35.0/src/load.rs:551`
  Unique ID of this loader.
- `fn load(&self, ctx: &Context, uri: &str, texture_options: TextureOptions, size_hint: SizeHint) -> TextureLoadResult` — `egui-0.35.0/src/load.rs:565`
  Try loading the texture from the given uri.
- `fn forget(&self, uri: &str)` — `egui-0.35.0/src/load.rs:577`
  Forget the given `uri`.
- `fn forget_all(&self)` — `egui-0.35.0/src/load.rs:583`
  Forget all URIs ever given to this loader.
- `fn end_pass(&self, pass_index: u64)` — `egui-0.35.0/src/load.rs:587`
  Implementations may use this to perform work at the end of a pass, such as evicting unused entries from a cac…
- `fn byte_size(&self) -> usize` — `egui-0.35.0/src/load.rs:592`
  If the loader caches any data, this should return the size of that cache.

### `BytesLoadResult` (type_alias) — `egui-0.35.0/src/load.rs:310`

### `ImageLoadResult` (type_alias) — `egui-0.35.0/src/load.rs:385`

### `Result` (type_alias) — `egui-0.35.0/src/load.rs:141`

### `TextureLoadResult` (type_alias) — `egui-0.35.0/src/load.rs:532`


## `egui::menu`

### `find_menu_root` — `egui-0.35.0/src/containers/menu.rs:32`

```rust
fn find_menu_root(ui: &Ui) -> &UiStack
```

Find the root [`UiStack`] of the menu.

### `is_in_menu` — `egui-0.35.0/src/containers/menu.rs:47`

```rust
fn is_in_menu(ui: &Ui) -> bool
```

Is this Ui part of a menu?

### `menu_style` — `egui-0.35.0/src/containers/menu.rs:22`

```rust
fn menu_style(style: &mut Style)
```

Apply a menu style to the [`Style`].

### `MenuButton` (struct) — `egui-0.35.0/src/containers/menu.rs:290`

A thin wrapper around a [`Button`] that shows a [`Popup::menu`] when clicked.

Public fields:

- `button: Button<'a>`
- `config: Option<MenuConfig>`

Methods:

- `fn config(self, config: MenuConfig) -> Self` — `egui-0.35.0/src/containers/menu.rs:302`
  Set the config for the menu.
- `fn from_button(button: Button<'a>) -> Self` — `egui-0.35.0/src/containers/menu.rs:309`
  Create a new menu button from a [`Button`].
- `fn new(atoms: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/containers/menu.rs:296`
- `fn ui<R>(self, ui: &mut Ui, content: impl FnOnce(&mut Ui) -> R) -> (Response, Option<InnerResponse<R>>)` — `egui-0.35.0/src/containers/menu.rs:317`
  Show the menu button.

### `MenuConfig` (struct) — `egui-0.35.0/src/containers/menu.rs:65`

Configuration and style for menus.

Public fields:

- `close_behavior: PopupCloseBehavior` — If the user clicks, should we close the menu?
- `style: StyleModifier` — Override the menu style.

Methods:

- `fn close_behavior(self, close_behavior: PopupCloseBehavior) -> Self` — `egui-0.35.0/src/containers/menu.rs:98`
  If the user clicks, should we close the menu?
- `fn find(ui: &Ui) -> Self` — `egui-0.35.0/src/containers/menu.rs:124`
  Find the config for the current menu.
- `fn new() -> Self` — `egui-0.35.0/src/containers/menu.rs:92`
- `fn style(self, style: impl Into<StyleModifier>) -> Self` — `egui-0.35.0/src/containers/menu.rs:107`
  Override the menu style.

Implements: `Clone`, `Debug`, `Default`

### `MenuState` (struct) — `egui-0.35.0/src/containers/menu.rs:136`

Holds the state of the menu.

Public fields:

- `open_item: Option<Id>` — The currently open sub menu in this menu.

Methods:

- `fn from_id<R>(ctx: &Context, id: Id, f: impl FnOnce(&mut Self) -> R) -> R` — `egui-0.35.0/src/containers/menu.rs:152`
  Get the state via the menus root [`Ui`] id
- `fn from_ui<R>(ui: &Ui, f: impl FnOnce(&mut Self, &UiStack) -> R) -> R` — `egui-0.35.0/src/containers/menu.rs:146`
  Find the root of the menu and get the state
- `fn is_deepest_open_sub_menu(ctx: &Context, id: Id) -> bool` — `egui-0.35.0/src/containers/menu.rs:188`
  Is the menu with this id the deepest sub menu? (-> no child sub menu is open)
- `fn mark_shown(ctx: &Context, id: Id)` — `egui-0.35.0/src/containers/menu.rs:178`

Implements: `Clone`

### `SubMenu` (struct) — `egui-0.35.0/src/containers/menu.rs:399`

Show a submenu in a menu.

Methods:

- `fn config(self, config: MenuConfig) -> Self` — `egui-0.35.0/src/containers/menu.rs:412`
  Set the config for the submenu.
- `fn id_from_widget_id(widget_id: Id) -> Id` — `egui-0.35.0/src/containers/menu.rs:418`
  Get the id for the submenu from the widget/response id.
- `fn new() -> Self` — `egui-0.35.0/src/containers/menu.rs:404`
- `fn show<R>(self, ui: &Ui, button_response: &Response, content: impl FnOnce(&mut Ui) -> R) -> Option<InnerResponse<R>>` — `egui-0.35.0/src/containers/menu.rs:426`
  Show the submenu.

Implements: `Clone`, `Debug`, `Default`

### `SubMenuButton` (struct) — `egui-0.35.0/src/containers/menu.rs:337`

A submenu button that shows a [`SubMenu`] if a [`Button`] is hovered.

Public fields:

- `button: Button<'a>`
- `sub_menu: SubMenu`

Methods:

- `fn config(self, config: MenuConfig) -> Self` — `egui-0.35.0/src/containers/menu.rs:365`
  Set the config for the submenu.
- `fn from_button(button: Button<'a>) -> Self` — `egui-0.35.0/src/containers/menu.rs:354`
  Create a new submenu button from a [`Button`].
- `fn new(atoms: impl IntoAtoms<'a>) -> Self` — `egui-0.35.0/src/containers/menu.rs:346`
- `fn ui<R>(self, ui: &mut Ui, content: impl FnOnce(&mut Ui) -> R) -> (Response, Option<InnerResponse<R>>)` — `egui-0.35.0/src/containers/menu.rs:371`
  Show the submenu button.


## `egui::os`

### `OperatingSystem` (enum) — `egui-0.35.0/src/os.rs:4`

An `enum` of common operating systems.

Variants:

- `OperatingSystem::Unknown` — Unknown OS - could be wasm
- `OperatingSystem::Android` — Android OS
- `OperatingSystem::IOS` — Apple iPhone OS
- `OperatingSystem::Nix` — Linux or Unix other than Android
- `OperatingSystem::Mac` — macOS
- `OperatingSystem::Windows` — Windows

Methods:

- `const fn from_target_os() -> Self` — `egui-0.35.0/src/os.rs:32`
  Uses the compile-time `target_arch` to identify the OS.
- `fn from_user_agent(user_agent: &str) -> Self` — `egui-0.35.0/src/os.rs:56`
  Helper: try to guess from the user-agent of a browser.
- `fn is_mac(&self) -> bool` — `egui-0.35.0/src/os.rs:80`
  Are we either macOS or iOS?

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `Hash`, `PartialEq`, `StructuralPartialEq`


## `egui::output`

### `OutputEvent` (enum) — `egui-0.35.0/src/data/output.rs:489`

Things that happened during this frame that the integration may be interested in.

Variants:

- `OutputEvent::Clicked` — A widget was clicked.
- `OutputEvent::DoubleClicked` — A widget was double-clicked.
- `OutputEvent::TripleClicked` — A widget was triple-clicked.
- `OutputEvent::FocusGained` — A widget gained keyboard focus (by tab key).
- `OutputEvent::TextSelectionChanged` — Text selection was updated.
- `OutputEvent::ValueChanged` — A widget's value changed.

Methods:

- `fn widget_info(&self) -> &WidgetInfo` — `egui-0.35.0/src/data/output.rs:510`

Implements: `Clone`, `Debug`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `IMEOutput` (struct) — `egui-0.35.0/src/data/output.rs:78`

Information about text being edited.

Public fields:

- `rect: Rect` — Where the [`crate::TextEdit`] is located on screen.
- `cursor_rect: Rect` — Where the primary cursor is.
- `should_interrupt_composition: bool` — Whether any ongoing IME composition should be interrupted.

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`


## `egui::plugin`

### `TypedPluginGuard` (struct) — `egui-0.35.0/src/plugin.rs:86`

A guard that provides access to a [`Plugin`].

Implements: `Deref`, `DerefMut`

### `TypedPluginHandle` (struct) — `egui-0.35.0/src/plugin.rs:61`

A typed handle to a registered [`Plugin`].

Methods:

- `fn lock(&self) -> TypedPluginGuard<'_, P>` — `egui-0.35.0/src/plugin.rs:77`
  Lock the plugin for access.

### `ContextCallback` (type_alias) — `egui-0.35.0/src/plugin.rs:224`

Generic event callback.


## `egui::scroll_area`

### `DragScroll` (enum) — `egui-0.35.0/src/containers/scroll_area.rs:147`

When [`ScrollArea`] should let the user scroll by dragging the content.

Variants:

- `DragScroll::Never` — Never scroll on pointer drag.
- `DragScroll::OnTouch` — Only allow drag-to-scroll when a touch screen is detected (see [`crate::InputState::has_touch_scree…
- `DragScroll::Always` — Always allow drag-to-scroll, even with a mouse.

Methods:

- `fn enabled(self, ctx: &Context) -> bool` — `egui-0.35.0/src/containers/scroll_area.rs:165`
  Whether drag-to-scroll is currently active.

Implements: `BitOr`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ScrollBarVisibility` (enum) — `egui-0.35.0/src/containers/scroll_area.rs:110`

Indicate whether the horizontal and vertical scroll bars must be always visible, hidden or visible when needed.

Variants:

- `ScrollBarVisibility::AlwaysHidden` — Hide scroll bar even if they are needed.
- `ScrollBarVisibility::VisibleWhenNeeded` — Show scroll bars only when the content size exceeds the container, i.e. when there is any need to s…
- `ScrollBarVisibility::AlwaysVisible` — Always show the scroll bar, even if the contents fit in the container and there is no need to scrol…

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ScrollAreaOutput` (struct) — `egui-0.35.0/src/containers/scroll_area.rs:89`

Public fields:

- `inner: R` — What the user closure returned.
- `id: Id` — [`Id`] of the [`ScrollArea`].
- `state: State` — The current state of the scroll area.
- `content_size: Vec2` — The size of the content. If this is larger than [`Self::inner_rect`], then there was need…
- `inner_rect: Rect` — Where on the screen the content is (excludes scroll bars).

### `ScrollSource` (struct) — `egui-0.35.0/src/containers/scroll_area.rs:192`

What is the source of scrolling for a [`ScrollArea`].

Public fields:

- `scroll_bar: bool` — Scroll the area by dragging a scroll bar.
- `drag: DragScroll` — Scroll the area by dragging the contents.
- `mouse_wheel: bool` — Scroll the area by scrolling (or shift scrolling) the mouse wheel with the mouse cursor o…

Methods:

- `fn any(&self) -> bool` — `egui-0.35.0/src/containers/scroll_area.rs:257`
  Is anything enabled?
- `fn is_all(&self) -> bool` — `egui-0.35.0/src/containers/scroll_area.rs:263`
  Is everything enabled?
- `fn is_none(&self) -> bool` — `egui-0.35.0/src/containers/scroll_area.rs:251`
  Is everything disabled?

Implements: `Add`, `AddAssign`, `BitOr`, `BitOrAssign`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `State` (struct) — `egui-0.35.0/src/containers/scroll_area.rs:26`

Public fields:

- `offset: Vec2` — Positive offset means scrolling down/right

Methods:

- `fn load(ctx: &Context, id: Id) -> Option<Self>` — `egui-0.35.0/src/containers/scroll_area.rs:75`
- `fn store(self, ctx: &Context, id: Id)` — `egui-0.35.0/src/containers/scroll_area.rs:79`
- `fn velocity(&self) -> Vec2` — `egui-0.35.0/src/containers/scroll_area.rs:84`
  Get the current kinetic scrolling velocity.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Serialize`


## `egui::special_emojis`

### `GIT` (constant) — `egui-0.35.0/src/lib.rs:615`

The word `git`.

### `GITHUB` (constant) — `egui-0.35.0/src/lib.rs:612`

The Github logo.

### `OS_ANDROID` (constant) — `egui-0.35.0/src/lib.rs:606`

The Android logo.

### `OS_APPLE` (constant) — `egui-0.35.0/src/lib.rs:609`

The Apple logo.

### `OS_LINUX` (constant) — `egui-0.35.0/src/lib.rs:600`

Tux, the Linux penguin.

### `OS_WINDOWS` (constant) — `egui-0.35.0/src/lib.rs:603`

The Windows logo.


## `egui::style`

### `HandleShape` (enum) — `egui-0.35.0/src/style.rs:1229`

Shape of the handle for sliders and similar widgets.

Variants:

- `HandleShape::Circle` — Circular handle
- `HandleShape::Rect` — Rectangular handle

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:2706`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `NumericColorSpace` (enum) — `egui-0.35.0/src/style.rs:2726`

How to display numeric color values.

Variants:

- `NumericColorSpace::GammaByte` — RGB is 0-255 in gamma space.
- `NumericColorSpace::Linear` — 0-1 in linear space.

Methods:

- `fn toggle_button_ui(&mut self, ui: &mut Ui) -> Response` — `egui-0.35.0/src/style.rs:2738`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `Display`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `default_text_styles` — `egui-0.35.0/src/style.rs:1408`

```rust
fn default_text_styles() -> BTreeMap<TextStyle, FontId>
```

The default text styles of the default egui theme.

### `font_tweak_ui` — `egui-0.35.0/src/style.rs:2999`

```rust
fn font_tweak_ui(ui: &mut Ui, tweak: &mut FontTweak, axes: &[FontVariationAxis]) -> Response
```

Show a UI for editing a [`FontTweak`].

### `DebugOptions` (struct) — `egui-0.35.0/src/style.rs:1327`

Options for help debug egui by adding extra visualization

Public fields:

- `debug_on_hover: bool` — Always show callstack to ui on hover.
- `debug_on_hover_with_all_modifiers: bool` — Show callstack for the current widget on hover if all modifier keys are pressed down.
- `hover_shows_next: bool` — If we show the hover ui, include where the next widget is placed.
- `show_expand_width: bool` — Show which widgets make their parent wider
- `show_expand_height: bool` — Show which widgets make their parent higher
- `show_resize: bool`
- `show_interactive_widgets: bool` — Show an overlay on all interactive widgets.
- `show_widget_hits: bool` — Show interesting widgets under the mouse cursor.
- `warn_if_rect_changes_id: bool` — Show a warning if the same `Rect` had different `Id` and the same parent `Id` on the prev…
- `show_unaligned: bool` — If true, highlight widgets that are not aligned to [`emath::GUI_ROUNDING`].
- `show_focused_widget: bool` — Highlight the currently focused widget.

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:2625`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Eq`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ImeComposition` (struct) — `egui-0.35.0/src/style.rs:1200`

Visual style for IME composition.

Public fields:

- `active_underline_stroke: Stroke` — Stroke used to underline the actively composed segment.
- `inactive_underline_stroke: Stroke` — Stroke used to underline those non-active segments.
- `legacy_visuals: bool` — If `true`, IME (Input Method Editor) composition (preedit) text is rendered the legacy wa…

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:2192`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Interaction` (struct) — `egui-0.35.0/src/style.rs:907`

How and when interaction happens.

Public fields:

- `interact_radius: f32` — How close a widget must be to the mouse to have a chance to register as a click or drag.
- `resize_grab_radius_side: f32` — Radius of the interactive area of the side of a window during drag-to-resize.
- `resize_grab_radius_corner: f32` — Radius of the interactive area of the corner of a window during drag-to-resize.
- `show_tooltips_only_when_still: bool` — If `false`, tooltips will show up anytime you hover anything, even if mouse is still movi…
- `tooltip_delay: f32` — Delay in seconds before showing tooltips after the mouse stops moving
- `tooltip_grace_time: f32` — If you have waited for a tooltip and then hover some other widget within this many second…
- `selectable_labels: bool` — Can you select the text on a [`crate::Label`] by default?
- `multi_widget_text_select: bool` — Can the user select text that span multiple labels?

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:2067`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `NumberFormatter` (struct) — `egui-0.35.0/src/style.rs:20`

How to format numbers in e.g. a [`crate::DragValue`].

Methods:

- `fn format(&self, value: f64, decimals: RangeInclusive<usize>) -> String` — `egui-0.35.0/src/style.rs:45`
  Format the given number with the given number of decimals.
- `fn new(formatter: impl 'static + Sync + Send + Fn(f64, RangeInclusive<usize>) -> String) -> Self` — `egui-0.35.0/src/style.rs:30`
  The first argument is the number to be formatted. The second argument is the range of the number of decimals…

Implements: `Clone`, `Debug`, `PartialEq`

### `ScrollAnimation` (struct) — `egui-0.35.0/src/style.rs:827`

Scroll animation configuration, used when programmatically scrolling somewhere (e.g. with `[crate::Ui::scroll_to_cursor]`).

Public fields:

- `points_per_second: f32` — With what speed should we scroll? (Default: 1000.0)
- `duration: Rangef` — The min / max scroll duration.

Methods:

- `fn duration(t: f32) -> Self` — `egui-0.35.0/src/style.rs:862`
  Scroll with a fixed duration, regardless of distance.
- `fn new(points_per_second: f32, duration: Rangef) -> Self` — `egui-0.35.0/src/style.rs:846`
  New scroll animation
- `fn none() -> Self` — `egui-0.35.0/src/style.rs:854`
  No animation, scroll instantly.
- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:869`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ScrollFadeStyle` (struct) — `egui-0.35.0/src/style.rs:780`

Controls if and how to fade out the sides of a [`crate::ScrollArea`] to indicate there is more there if you scroll.

Public fields:

- `strength: f32` — Opacity of the fade effect at the outer edge, in 0.0-1.0.
- `size: f32` — Size of the fade-area (height for vertical scrolling, width for horizontal scrolling).

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:801`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `ScrollStyle` (struct) — `egui-0.35.0/src/style.rs:491`

Controls the spacing and visuals of a [`crate::ScrollArea`].

Public fields:

- `floating: bool` — If `true`, scroll bars float above the content, partially covering it.
- `content_margin: Margin` — Extra margin added around the contents of a [`crate::ScrollArea`].
- `bar_width: f32` — The width of the scroll bars at it largest.
- `handle_min_length: f32` — Make sure the scroll handle is at least this big
- `bar_inner_margin: f32` — Margin between contents and scroll bar.
- `bar_outer_margin: f32` — Margin between scroll bar and the outer container (e.g. right of a vertical scroll bar).…
- `floating_width: f32` — The thin width of floating scroll bars that the user is NOT hovering.
- `floating_allocated_width: f32` — How much space is allocated for a floating scroll bar?
- `foreground_color: bool` — If true, use colors with more contrast. Good for floating scroll bars.
- `dormant_background_opacity: f32` — The opaqueness of the background when the user is neither scrolling nor hovering the scro…
- `active_background_opacity: f32` — The opaqueness of the background when the user is hovering the scroll area, but not the s…
- `interact_background_opacity: f32` — The opaqueness of the background when the user is hovering over the scroll bars.
- `dormant_handle_opacity: f32` — The opaqueness of the handle when the user is neither scrolling nor hovering the scroll a…
- `active_handle_opacity: f32` — The opaqueness of the handle when the user is hovering the scroll area, but not the scrol…
- `interact_handle_opacity: f32` — The opaqueness of the handle when the user is hovering over the scroll bars.
- `fade: ScrollFadeStyle`

Methods:

- `fn allocated_width(&self) -> f32` — `egui-0.35.0/src/style.rs:652`
  Width of a solid vertical scrollbar, or height of a horizontal scroll bar, when it is at its widest.
- `fn details_ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:673`
- `fn floating() -> Self` — `egui-0.35.0/src/style.rs:639`
  No scroll bars until you hover the scroll area, at which time they appear faintly, and then expand when you h…
- `fn solid() -> Self` — `egui-0.35.0/src/style.rs:589`
  Solid scroll bars that always use up space
- `fn thin() -> Self` — `egui-0.35.0/src/style.rs:615`
  Thin scroll bars that expand on hover
- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:660`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Selection` (struct) — `egui-0.35.0/src/style.rs:1188`

Selected text, selected elements etc

Public fields:

- `bg_fill: Color32` — Background color behind selected text and other selectable buttons.
- `stroke: Stroke` — Color of selected text.

Methods:

- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:2175`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `StyleModifier` (struct) — `egui-0.35.0/src/style.rs:193`

Utility to modify a [`Style`] in some way. Constructed via [`StyleModifier::from`] from a `Fn(&mut Style)` or a [`Style`].

Methods:

- `fn apply(&self, style: &mut Style)` — `egui-0.35.0/src/style.rs:224`
  Apply the modification to the given [`Style`]. Usually used with [`Ui::style_mut`].
- `fn new(f: impl Fn(&mut Style) + Send + Sync + 'static) -> Self` — `egui-0.35.0/src/style.rs:218`
  Create a new [`StyleModifier`] from a function.

Implements: `Clone`, `Debug`, `Default`, `From<Style>`, `From<T>`

### `TextCursorStyle` (struct) — `egui-0.35.0/src/style.rs:947`

Look and feel of the text cursor.

Public fields:

- `stroke: Stroke` — The color and width of the text cursor
- `preview: bool` — Show where the text cursor would be if you clicked?
- `blink: bool` — Should the cursor blink?
- `on_duration: f32` — When blinking, this is how long the cursor is visible.
- `off_duration: f32` — When blinking, this is how long the cursor is invisible.

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `WidgetVisuals` (struct) — `egui-0.35.0/src/style.rs:1284`

bg = background, fg = foreground.

Public fields:

- `bg_fill: Color32` — Background color of widgets that must have a background fill, such as the slider backgrou…
- `weak_bg_fill: Color32` — Background color of widgets that can _optionally_ have a background fill, such as buttons.
- `bg_stroke: Stroke` — For surrounding rectangle of things that need it, like buttons, the box of the checkbox,…
- `corner_radius: CornerRadius` — Button frames etc.
- `fg_stroke: Stroke` — Stroke and text color of the interactive part of a component (button text, slider grab, c…
- `expansion: f32` — Make the frame this much larger.

Methods:

- `fn text_color(&self) -> Color32` — `egui-0.35.0/src/style.rs:1318`
- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:2220`

Implements: `Clone`, `Copy`, `Debug`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Widgets` (struct) — `egui-0.35.0/src/style.rs:1244`

The visuals of widgets for different states of interaction.

Public fields:

- `noninteractive: WidgetVisuals` — The style of a widget that you cannot interact with. * `noninteractive.bg_stroke` is the…
- `inactive: WidgetVisuals` — The style of an interactive widget, such as a button, at rest.
- `hovered: WidgetVisuals` — The style of an interactive widget while you hover it, or when it is highlighted.
- `active: WidgetVisuals` — The style of an interactive widget as you are clicking or dragging it.
- `open: WidgetVisuals` — The style of a button that has an open menu beneath it (e.g. a combo-box)

Methods:

- `fn dark() -> Self` — `egui-0.35.0/src/style.rs:1673`
- `fn light() -> Self` — `egui-0.35.0/src/style.rs:1718`
- `fn state(&self, state: WidgetState) -> &WidgetVisuals` — `egui-0.35.0/src/widget_style.rs:94`
  The widget visuals according to the state
- `fn style(&self, response: &Response) -> &WidgetVisuals` — `egui-0.35.0/src/style.rs:1267`
- `fn ui(&mut self, ui: &mut Ui)` — `egui-0.35.0/src/style.rs:2138`

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`


## `egui::text`

### `FontFamily` (enum) — `epaint-0.35.0/src/text/fonts.rs:80`

Font of unknown size.

Variants:

- `FontFamily::Proportional` — A font where some characters are wider than other (e.g. 'w' is wider than 'i').
- `FontFamily::Monospace` — A font where each character is the same width (`w` is the same width as `i`).
- `FontFamily::Name` — One of the names in [`FontDefinitions::families`].

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Display`, `Eq`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`

### `ByteIndex` (struct) — `epaint-0.35.0/src/text/index.rs:19`

A byte offset into a UTF-8 string.

Methods:

- `fn saturating_add(self, rhs: usize) -> Self` — `epaint-0.35.0/src/text/index.rs:133`
  Saturating integer addition.
- `fn saturating_sub(self, rhs: usize) -> Self` — `epaint-0.35.0/src/text/index.rs:133`
  Saturating integer subtraction.

Implements: `Add`, `Add<usize>`, `AddAssign`, `AddAssign<usize>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `Display`, `Eq`, `From<ByteIndex>`, `From<usize>`, `Hash`, `Ord`, `PartialEq`, `PartialOrd`, `Serialize`, `StructuralPartialEq`, `Sub`, `Sub<usize>`, `SubAssign<usize>`

### `CCursor` (struct) — `epaint-0.35.0/src/text/cursor.rs:10`

Character cursor.

Public fields:

- `index: CharIndex` — Character offset (NOT byte offset!).
- `prefer_next_row: bool` — If this cursors sits right at the border of a wrapped row break (NOT paragraph break) do…

Methods:

- `fn new(index: impl Into<CharIndex>) -> Self` — `epaint-0.35.0/src/text/cursor.rs:23`

Implements: `Add<CharIndex>`, `Add<usize>`, `AddAssign<usize>`, `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `Sub<CharIndex>`, `Sub<usize>`, `SubAssign<usize>`

### `CCursorRange` (struct) — `egui-0.35.0/src/text_selection/cursor_range.rs:12`

A selected text range (could be a range of length zero).

Public fields:

- `primary: CCursor` — When selecting with a mouse, this is where the mouse was released. When moving with e.g.…
- `secondary: CCursor` — When selecting with a mouse, this is where the mouse was first pressed. This part of the…
- `h_pos: Option<f32>` — Saved horizontal position of the cursor.

Methods:

- `fn as_sorted_char_range(&self) -> Range<CharIndex>` — `egui-0.35.0/src/text_selection/cursor_range.rs:52`
  The range of selected character indices.
- `fn contains(&self, other: Self) -> bool` — `egui-0.35.0/src/text_selection/cursor_range.rs:67`
  Is `self` a super-set of the other range?
- `fn is_empty(&self) -> bool` — `egui-0.35.0/src/text_selection/cursor_range.rs:62`
  True if the selected range contains no characters.
- `fn is_sorted(&self) -> bool` — `egui-0.35.0/src/text_selection/cursor_range.rs:84`
- `fn on_event(&mut self, os: OperatingSystem, event: &Event, galley: &Galley, _widget_id: Id) -> bool` — `egui-0.35.0/src/text_selection/cursor_range.rs:172`
  Check for events that modify the cursor range.
- `fn on_key_press(&mut self, os: OperatingSystem, galley: &Galley, modifiers: &Modifiers, key: Key) -> bool` — `egui-0.35.0/src/text_selection/cursor_range.rs:108`
  Check for key presses that are moving the cursor.
- `fn one(ccursor: CCursor) -> Self` — `egui-0.35.0/src/text_selection/cursor_range.rs:29`
  The empty range.
- `fn select_all(galley: &Galley) -> Self` — `egui-0.35.0/src/text_selection/cursor_range.rs:47`
  Select all the text in a galley
- `fn single(&self) -> Option<CCursor>` — `egui-0.35.0/src/text_selection/cursor_range.rs:75`
  If there is a selection, None is returned. If the two ends are the same, that is returned.
- `fn slice_str(&self, text: &'s str) -> &'s str` — `egui-0.35.0/src/text_selection/cursor_range.rs:100`
- `fn sorted_cursors(&self) -> [CCursor; 2]` — `egui-0.35.0/src/text_selection/cursor_range.rs:92`
  returns the two ends ordered
- `fn two(min: impl Into<CCursor>, max: impl Into<CCursor>) -> Self` — `egui-0.35.0/src/text_selection/cursor_range.rs:38`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `From<CCursorRange>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

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

### `ByteRange` (type_alias) — `epaint-0.35.0/src/text/index.rs:137`

A range of [`ByteIndex`], i.e. a byte range into a [`str`].

### `CharRange` (type_alias) — `epaint-0.35.0/src/text/index.rs:140`

A range of [`CharIndex`], i.e. a character range into a [`str`].


## `egui::text_edit`

### `TextCursorState` (struct) — `egui-0.35.0/src/text_selection/text_cursor_state.rs:16`

The state of a text cursor selection.

Methods:

- `fn char_range(&self) -> Option<CCursorRange>` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:34`
  The currently selected range of characters.
- `fn is_empty(&self) -> bool` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:29`
- `fn pointer_interaction(&mut self, ui: &Ui, response: &Response, cursor_at_pointer: CCursor, galley: &Galley, is_being_dragged: bool) -> bool` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:58`
  Handle clicking and/or dragging text.
- `fn range(&self, galley: &Galley) -> Option<CCursorRange>` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:40`
  The currently selected range of characters, clamped within the character range of the given [`Galley`].
- `fn set_char_range(&mut self, ccursor_range: Option<CCursorRange>)` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:49`
  Sets the currently selected range of characters.

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Deserialize<'de>`, `From<CCursorRange>`, `Serialize`

### `TextEditOutput` (struct) — `egui-0.35.0/src/widgets/text_edit/output.rs:6`

The output from a [`TextEdit`](crate::TextEdit).

Public fields:

- `response: AtomLayoutResponse` — The interaction response.
- `galley: Arc<Galley>` — How the text was displayed.
- `galley_pos: Pos2` — Where the text in [`Self::galley`] ended up on the screen.
- `text_clip_rect: Rect` — The text was clipped to this rectangle when painted.
- `state: TextEditState` — The state we stored after the run.
- `cursor_range: Option<CCursorRange>` — Where the text cursor is.

### `TextEditState` (struct) — `egui-0.35.0/src/widgets/text_edit/state.rs:38`

The text edit state stored between frames.

Public fields:

- `cursor: TextCursorState` — Controls the text selection.

Methods:

- `fn clear_undoer(&mut self)` — `egui-0.35.0/src/widgets/text_edit/state.rs:79`
- `fn load(ctx: &Context, id: Id) -> Option<Self>` — `egui-0.35.0/src/widgets/text_edit/state.rs:62`
- `fn set_undoer(&mut self, undoer: Undoer<(CCursorRange, String)>)` — `egui-0.35.0/src/widgets/text_edit/state.rs:75`
- `fn store(self, ctx: &Context, id: Id)` — `egui-0.35.0/src/widgets/text_edit/state.rs:66`
- `fn undoer(&self) -> Undoer<(CCursorRange, String)>` — `egui-0.35.0/src/widgets/text_edit/state.rs:70`

Implements: `Clone`, `Default`, `Deserialize<'de>`, `Serialize`


## `egui::text_selection`

### `LabelSelectionState` (struct) — `egui-0.35.0/src/text_selection/label_text_selection.rs:85`

Handles text selection in labels (NOT in [`crate::TextEdit`])s.

Methods:

- `fn clear_selection(&mut self)` — `egui-0.35.0/src/text_selection/label_text_selection.rs:168`
  Clear all label text selections in all viewports.
- `fn has_selection(&self) -> bool` — `egui-0.35.0/src/text_selection/label_text_selection.rs:161`
  Is there a label text selection in any viewport?
- `fn label_text_selection(ui: &Ui, response: &Response, galley_pos: Pos2, galley: Arc<Galley>, fallback_color: Color32, underline: Stroke)` — `egui-0.35.0/src/text_selection/label_text_selection.rs:174`
  Handle text selection state for a label or similar widget. This also takes care of painting the galley.

Implements: `Clone`, `Debug`, `Default`, `Plugin`


## `egui::util`

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

### `IdTypeMap` (struct) — `egui-0.35.0/src/util/id_type_map.rs:406`

Stores values identified by an [`Id`] AND the [`std::any::TypeId`] of the value.

Methods:

- `fn clear(&mut self)` — `egui-0.35.0/src/util/id_type_map.rs:609`
- `fn count<T>(&self) -> usize` — `egui-0.35.0/src/util/id_type_map.rs:645`
  Count the number of values are stored with the given type.
- `fn count_serialized(&self) -> usize` — `egui-0.35.0/src/util/id_type_map.rs:637`
  Count how many values are stored but not yet deserialized.
- `fn get_persisted<T>(&mut self, id: Id) -> Option<T>` — `egui-0.35.0/src/util/id_type_map.rs:479`
  Read a value, optionally deserializing it if available.
- `fn get_persisted_mut_or<T>(&mut self, id: Id, or_insert: T) -> &mut T` — `egui-0.35.0/src/util/id_type_map.rs:497`
- `fn get_persisted_mut_or_default<T>(&mut self, id: Id) -> &mut T` — `egui-0.35.0/src/util/id_type_map.rs:510`
- `fn get_persisted_mut_or_insert_with<T>(&mut self, id: Id, insert_with: impl FnOnce() -> T) -> &mut T` — `egui-0.35.0/src/util/id_type_map.rs:536`
- `fn get_temp<T>(&self, id: Id) -> Option<T>` — `egui-0.35.0/src/util/id_type_map.rs:447`
  Read a value without trying to deserialize a persisted value.
- `fn get_temp_mut_or<T>(&mut self, id: Id, or_insert: T) -> &mut T` — `egui-0.35.0/src/util/id_type_map.rs:488`
- `fn get_temp_mut_or_default<T>(&mut self, id: Id) -> &mut T` — `egui-0.35.0/src/util/id_type_map.rs:502`
- `fn get_temp_mut_or_insert_with<T>(&mut self, id: Id, insert_with: impl FnOnce() -> T) -> &mut T` — `egui-0.35.0/src/util/id_type_map.rs:514`
- `fn get_temp_raw(&self, raw: RawKey) -> Option<&dyn Any + Send + Sync>` — `egui-0.35.0/src/util/id_type_map.rs:455`
  Gets a reference to a value for a given raw key.
- `fn get_temp_raw_mut(&mut self, raw: RawKey) -> Option<&mut dyn Any + Send + Sync>` — `egui-0.35.0/src/util/id_type_map.rs:465`
  Gets a mutable reference to a value for a given raw key.
- `fn insert_persisted<T>(&mut self, id: Id, value: T)` — `egui-0.35.0/src/util/id_type_map.rs:436`
  Insert a value that will be persisted next time you start the app.
- `fn insert_temp<T>(&mut self, id: Id, value: T) -> RawKey` — `egui-0.35.0/src/util/id_type_map.rs:424`
  Insert a value that will not be persisted.
- `fn is_empty(&self) -> bool` — `egui-0.35.0/src/util/id_type_map.rs:614`
- `fn len(&self) -> usize` — `egui-0.35.0/src/util/id_type_map.rs:619`
- `fn max_bytes_per_type(&self) -> usize` — `egui-0.35.0/src/util/id_type_map.rs:669`
  The maximum number of bytes that will be used to store the persisted state of a single widget type.
- `fn remove<T>(&mut self, id: Id)` — `egui-0.35.0/src/util/id_type_map.rs:571`
  Remove the state of this type and id.
- `fn remove_by_type<T>(&mut self)` — `egui-0.35.0/src/util/id_type_map.rs:600`
  Note all state of the given type.
- `fn remove_temp<T>(&mut self, id: Id) -> Option<T>` — `egui-0.35.0/src/util/id_type_map.rs:578`
  Remove and fetch the state of this type and id.
- `fn remove_temp_raw(&mut self, raw: RawKey) -> Option<Box<dyn Any + Send + Sync>>` — `egui-0.35.0/src/util/id_type_map.rs:587`
  Remove a temporary value given a raw key.
- `fn set_max_bytes_per_type(&mut self, max_bytes_per_type: usize)` — `egui-0.35.0/src/util/id_type_map.rs:674`
  See [`Self::max_bytes_per_type`].
- `fn temp_keys(&self) -> impl Iterator<Item = RawKey>` — `egui-0.35.0/src/util/id_type_map.rs:628`
  Returns all [`RawKey`]s to values in this map.

Implements: `Clone`, `Debug`, `Default`


## `egui::widget_style`

### `ROOT_CLASS` (constant) — `egui-0.35.0/src/widget_style.rs:222`

The root class is a special class present on every top-level [`crate::Ui`].

### `SELECTED_CLASS` (constant) — `egui-0.35.0/src/widget_style.rs:225`

The selected class is a special class present on selected [`crate::Button`].

### `WidgetState` (enum) — `egui-0.35.0/src/widget_style.rs:84`

The different state of a widget can be

Variants:

- `WidgetState::Noninteractive`
- `WidgetState::Inactive`
- `WidgetState::Hovered`
- `WidgetState::Active`

Implements: `Clone`, `Copy`, `Debug`, `Default`, `Eq`, `PartialEq`, `StructuralPartialEq`

### `ButtonStyle` (struct) — `egui-0.35.0/src/widget_style.rs:35`

Dedicated button style

Public fields:

- `frame: Frame`
- `text_style: TextVisuals`

### `CheckboxStyle` (struct) — `egui-0.35.0/src/widget_style.rs:41`

Dedicated checkbox style

Public fields:

- `frame: Frame` — Frame around
- `text_style: TextVisuals` — Text next to it
- `checkbox_size: f32` — Checkbox size
- `check_size: f32` — Checkmark size
- `checkbox_frame: Frame` — Frame of the checkbox itself
- `check_stroke: Stroke` — Checkmark stroke

### `Classes` (struct) — `egui-0.35.0/src/widget_style.rs:235`

Classes are string identifier that can be set on widget/Ui.

Implements: `Clone`, `Debug`, `Default`, `Display`, `HasClasses`

### `LabelStyle` (struct) — `egui-0.35.0/src/widget_style.rs:62`

Dedicated label style

Public fields:

- `frame: Frame` — Frame around
- `text: TextVisuals` — Text style
- `wrap_mode: TextWrapMode` — Wrap mode used

### `SeparatorStyle` (struct) — `egui-0.35.0/src/widget_style.rs:74`

Dedicated separator style

Public fields:

- `spacing: f32` — How much space is allocated in the layout direction
- `stroke: Stroke` — How to paint it

### `TextVisuals` (struct) — `egui-0.35.0/src/widget_style.rs:13`

General text style

Public fields:

- `font_id: FontId` — Font used
- `color: Color32` — Font color
- `underline: Stroke` — Text decoration
- `strikethrough: Stroke`

### `WidgetStyle` (struct) — `egui-0.35.0/src/widget_style.rs:26`

General widget style

Public fields:

- `frame: Frame`
- `text: TextVisuals`
- `stroke: Stroke`

### `HasClasses` (trait) — `egui-0.35.0/src/widget_style.rs:269`

Any widgets supporting [`Classes`] must implement this trait

Required/provided items:

- `fn classes(&self) -> &Classes` — `egui-0.35.0/src/widget_style.rs:270`
- `fn classes_mut(&mut self) -> &mut Classes` — `egui-0.35.0/src/widget_style.rs:272`
- `fn with_class(self, class: impl Into<ClassName>) -> Self` — `egui-0.35.0/src/widget_style.rs:276`
  Add the given class by consuming [`self`]
- `fn with_class_if(self, class: impl Into<ClassName>, condition: bool) -> Self` — `egui-0.35.0/src/widget_style.rs:286`
  Add the given class by consuming [`self`] if the condition is true
- `fn add_class(&mut self, class: impl Into<ClassName>) -> &mut Self` — `egui-0.35.0/src/widget_style.rs:296`
  Add the given class in-place
- `fn add_class_if(&mut self, class: impl Into<ClassName>, condition: bool) -> &mut Self` — `egui-0.35.0/src/widget_style.rs:306`
  Add the given class in-place if the condition is true
- `fn has(&self, class: impl Into<ClassName>) -> bool` — `egui-0.35.0/src/widget_style.rs:315`
  True if the class is present

### `ClassName` (type_alias) — `egui-0.35.0/src/widget_style.rs:228`

A class is a static string identifier.


## `egui::gui_zoom::kb_shortcuts`

### `ZOOM_IN` (constant) — `egui-0.35.0/src/gui_zoom.rs:10`

Primary keyboard shortcut for zooming in (`Cmd` + `+`).

### `ZOOM_IN_SECONDARY` (constant) — `egui-0.35.0/src/gui_zoom.rs:18`

Secondary keyboard shortcut for zooming in (`Cmd` + `=`).

### `ZOOM_OUT` (constant) — `egui-0.35.0/src/gui_zoom.rs:22`

Keyboard shortcut for zooming in (`Cmd` + `-`).

### `ZOOM_RESET` (constant) — `egui-0.35.0/src/gui_zoom.rs:25`

Keyboard shortcut for resetting zoom in (`Cmd` + `0`).


## `egui::text_selection::accesskit_text`

### `update_accesskit_for_text_widget` — `egui-0.35.0/src/text_selection/accesskit_text.rs:30`

```rust
fn update_accesskit_for_text_widget(ctx: &Context, widget_id: Id, cursor_range: Option<CCursorRange>, role: Role, global_from_galley: TSTransform, galley: &Galley)
```

Update accesskit with the current text state.


## `egui::text_selection::text_cursor_state`

### `byte_index_from_char_index` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:276`

```rust
fn byte_index_from_char_index(s: &str, char_index: CharIndex) -> ByteIndex
```

### `ccursor_next_word` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:166`

```rust
fn ccursor_next_word(text: &str, ccursor: CCursor) -> CCursor
```

### `ccursor_previous_word` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:180`

```rust
fn ccursor_previous_word(text: &str, ccursor: CCursor) -> CCursor
```

### `char_index_from_byte_index` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:285`

```rust
fn char_index_from_byte_index(input: &str, byte_index: ByteIndex) -> CharIndex
```

### `cursor_rect` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:310`

```rust
fn cursor_rect(galley: &Galley, cursor: &CCursor, row_height: f32) -> Rect
```

The thin rectangle of one end of the selection, e.g. the primary cursor, in local galley coordinates.

### `find_line_start` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:264`

```rust
fn find_line_start(text: &str, current_index: CCursor) -> CCursor
```

Accepts and returns character offset (NOT byte offset!).

### `is_word_char` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:255`

```rust
fn is_word_char(c: char) -> bool
```

### `slice_char_range` — `egui-0.35.0/src/text_selection/text_cursor_state.rs:297`

```rust
fn slice_char_range(s: &str, char_range: Range<CharIndex>) -> &str
```


## `egui::text_selection::visuals`

### `paint_cursor_end` — `egui-0.35.0/src/text_selection/visuals.rs:267`

```rust
fn paint_cursor_end(painter: &Painter, visuals: &Visuals, cursor_rect: Rect)
```

Paint one end of the selection, e.g. the primary cursor.

### `paint_text_cursor` — `egui-0.35.0/src/text_selection/visuals.rs:291`

```rust
fn paint_text_cursor(ui: &Ui, painter: &Painter, primary_cursor_rect: Rect, time_since_last_interaction: f64)
```

Paint one end of the selection, e.g. the primary cursor, with blinking (if enabled).

### `paint_text_selection` — `egui-0.35.0/src/text_selection/visuals.rs:25`

```rust
fn paint_text_selection(galley: &mut Arc<Galley>, visuals: &Visuals, cursor_range: &CCursorRange, new_vertex_indices: Option<&mut Vec<RowVertexIndices>>)
```

Adds text selection rectangles to the galley.

### `RowVertexIndices` (struct) — `egui-0.35.0/src/text_selection/visuals.rs:19`

Public fields:

- `row: usize`
- `vertex_indices: [u32; 6]`

Implements: `Clone`, `Debug`


## `egui::util::id_type_map`

### `RawKey` (struct) — `egui-0.35.0/src/util/id_type_map.rs:341`

The key used in [`IdTypeMap`], which is a combination of an [`Id`] and a [`TypeId`].

Methods:

- `fn new<T>(id: Id) -> Self` — `egui-0.35.0/src/util/id_type_map.rs:359`
  Create a new key for the given type.

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `Hash`, `IsEnabled`, `PartialEq`, `StructuralPartialEq`

### `TypeId` (struct) — `egui-0.35.0/src/util/id_type_map.rs:12`

Like [`std::any::TypeId`], but can be serialized and deserialized.

Methods:

- `fn of<T>() -> Self` — `egui-0.35.0/src/util/id_type_map.rs:16`

Implements: `Clone`, `Copy`, `Debug`, `Eq`, `From<TypeId>`, `Hash`, `IsEnabled`, `PartialEq`, `StructuralPartialEq`

### `SerializableAny` (trait) — `egui-0.35.0/src/util/id_type_map.rs:50`

Required/provided items:



## `egui::util::undoer`

### `Settings` (struct) — `egui-0.35.0/src/util/undoer.rs:5`

Public fields:

- `max_undos: usize` — Maximum number of undos. If your state is resource intensive, you should keep this low.
- `stable_time: f32` — When that state hasn't changed for this many seconds, create a new undo point (if one is…
- `auto_save_interval: f32` — If the state is changing so often that we never get to `stable_time`, then still create a…

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `PartialEq`, `Serialize`, `StructuralPartialEq`

### `Undoer` (struct) — `egui-0.35.0/src/util/undoer.rs:52`

Automatic undo system.

Methods:

- `fn add_undo(&mut self, current_state: &State)` — `egui-0.35.0/src/util/undoer.rs:165`
  Add an undo point if, and only if, there has been a change since the latest undo point.
- `fn feed_state(&mut self, current_time: f64, current_state: &State)` — `egui-0.35.0/src/util/undoer.rs:179`
  Call this as often as you want (e.g. every frame) and [`Undoer`] will determine if a new undo point should be…
- `fn has_redo(&self, current_state: &State) -> bool` — `egui-0.35.0/src/util/undoer.rs:124`
- `fn has_undo(&self, current_state: &State) -> bool` — `egui-0.35.0/src/util/undoer.rs:116`
  Do we have an undo point different from the given state?
- `fn is_in_flux(&self) -> bool` — `egui-0.35.0/src/util/undoer.rs:129`
  Return true if the state is currently changing
- `fn redo(&mut self, current_state: &State) -> Option<&State>` — `egui-0.35.0/src/util/undoer.rs:151`
- `fn undo(&mut self, current_state: &State) -> Option<&State>` — `egui-0.35.0/src/util/undoer.rs:133`
- `fn with_settings(settings: Settings) -> Self` — `egui-0.35.0/src/util/undoer.rs:108`
  Create a new [`Undoer`] with the given [`Settings`].

Implements: `Clone`, `Debug`, `Default`, `Deserialize<'de>`, `Serialize`


