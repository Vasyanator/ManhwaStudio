# Ids, `id_salt`, and why localization breaks widget state

Target: egui **0.35.0**. Every egui claim is cited as `egui-0.35.0/src/<path>:<line>`.

## 1. How egui derives an `Id`

```rust
// egui-0.35.0/src/id.rs:67
pub fn new(source: impl AsId) -> Self
// egui-0.35.0/src/id.rs:77
pub fn with(self, salt: impl AsIdSalt) -> Self
```

`Id::new` hashes a root source; `Id::with` hashes `parent_id ⊕ salt` (`id.rs:78-83`). Every
container/widget id in egui is one of those two. A `Ui` exposes the same:

- `Ui::make_persistent_id(id_salt)` = `self.id.with(id_salt)` (`egui-0.35.0/src/ui.rs:883-885`).
- `Ui::push_id(id_salt, add_contents)` — a child `Ui` with a salted id
  (`egui-0.35.0/src/ui.rs:2163`); the doc there literally shows it as the fix for a loop of
  identically-titled `ui.collapsing` headers (`ui.rs:2155-2162`).
- `Ui::scope_builder(UiBuilder, ...)` (`ui.rs:2193`) — `UiBuilder::id_salt(...)`
  (`ui_builder.rs:56`) for a salted child, `UiBuilder::id(...)` (`ui_builder.rs:72`) for an
  explicit id.

Builders that take a salt:

| Item | Cite |
|---|---|
| `ComboBox::new(id_salt, label)` / `from_id_salt(id_salt)` | `containers/combo_box.rs:54`, `:85` |
| `ComboBox::from_label(label)` — id comes from the **label text** | `containers/combo_box.rs:69` |
| `CollapsingHeader::new(text)` — id from the text; `.id_salt(...)` overrides | `containers/collapsing_header.rs:396`, `:434` |
| `ScrollArea::id_salt(...)` | `containers/scroll_area.rs:482` |
| `Grid::new(id_salt)` | `grid.rs:327` |
| `Resize::id_salt(...)` | `containers/resize.rs:81` |
| `TextEdit::id(Id)` / `.id_salt(...)` | `widgets/text_edit/builder.rs:167`, `:180` |
| `Area::new(id: Id)` — takes a **full `Id`**, not a salt | `containers/area.rs:133` |
| `Window::new(title)` — id from the **title text**; `.id(Id)` overrides | `containers/window.rs:101`, `:160` |

`Window`'s own doc states the trap (`containers/window.rs:98-100`):

> The window title is used as a unique `Id` and must be unique, and should not change. […]
> If you need a changing title, you must call `window.id(…)` with a fixed id.

and the implementation confirms it: `Area::new(Id::new(title.text()))`
(`containers/window.rs:103`).

### `id_source` — the accurate status

`id_source` is **almost** gone, but not entirely: `TextEdit::id_source` still exists in 0.35
as a thin alias that just forwards to `id_salt`:

```rust
// egui-0.35.0/src/widgets/text_edit/builder.rs:172-177
/// A source for the unique [`Id`], e.g. `.id_source("second_text_edit_field")` …
#[inline]
pub fn id_source(self, id_salt: impl AsIdSalt) -> Self {
    self.id_salt(id_salt)
}
```

That is the **only** `id_source` *builder method* left (`grep -rn "id_source" egui-0.35.0/src`
otherwise hits the public `UiBuilder.id_source` **field** at `ui_builder.rs:20` — set through
`.id_salt()` / `.id()`, not by name — plus the debug-only `id_source` module at `id.rs:172` and
`panel.rs:207`'s private `resize_id_source`).
`ComboBox::from_id_source`, `ScrollArea::id_source`, `CollapsingHeader::id_source`,
`Grid::new(id_source)`-as-a-name — all gone. **Write `id_salt` everywhere.**

## 2. THE RULE: localized label ⇒ mandatory `id_salt`

The rule (`README_AGENT.md:532-534`): **when the label of a `ComboBox::from_label` /
`WheelComboBox::from_label` / `Window::new` / `CollapsingHeader::new` /
`ui.collapsing` widget is a translated string, you MUST pin a stable `id_salt`.** egui hashes
the label *text* into the `Id`, so the id changes with the UI language — and everything egui
stores under that id (combo open/closed, collapsing open/closed, scroll offset, `TextEdit`
cursor/selection, window position and size) is silently dropped on a language switch, or on
any dynamic label change.

Wrong — id follows the translation:

```rust
// The Id is hash("Режим строки") in ru and hash("Line mode") in en: two different widgets.
WheelComboBox::from_label(t!("typing.advanced.line_mode_combo_label"))
    .show_index(ui, &mut idx, len, |i| labels[i]);
```

Right — visible text localized, id pinned (real site:
`src/tabs/typing/panel/create_advanced.rs:38`):

```rust
WheelComboBox::from_label(t!("typing.advanced.line_mode_combo_label"))
    .id_salt("typing.advanced.line_mode_combo_label")
    .show_index(ui, &mut idx, len, |i| labels[i]);
```

Convention in this repo: **use the i18n key itself as the salt.** It is stable, unique, and
greppable. The same applies to `egui::CollapsingHeader::new(t!(...)).id_salt("…")`,
`egui::Window::new(t!(...)).id(egui::Id::new("…"))`, and any `ScrollArea`/`Grid` whose salt
would otherwise be derived from a label. `src/` currently carries ~212 `id_salt` sites; follow
the majority.

Corollary for `ui.collapsing(text, …)` (`egui-0.35.0/src/ui.rs:2220`): it has **no** salt
parameter — its id is the text. Wrap it: `ui.push_id("stable_key", |ui| ui.collapsing(t!(…), …))`
(`ui.rs:2163`), or use `CollapsingHeader::new(t!(…)).id_salt("stable_key")`.

## 3. The other mandatory i18n rule: no literal user-visible strings

`README_AGENT.md:508-513` states it as an absolute: **no user-visible text may be written as a
literal in `.rs`.** Every such string is added to the catalog as a KEY and read back through
`t!` / `tf!` / `tp!`.

- Macros: `t!` (`crates/ms-i18n/src/lib.rs:106`), `tf!` (formatted, `:119`), `tp!` (plural,
  `:136`).
- Catalogs: `crates/ms-i18n/locales/{en,ru,es,fr,pt}.json`. A new key must land in **all** of
  them at once — `en` is the reference/fallback and a missing key fails the `key_validation`
  test (`README_AGENT.md:515-518`).
- Key shape: `<area>.<screen_or_module>.<meaning>` with a role suffix (`_label`, `_hint`,
  `_button`, `_title`, `_error`, `_tooltip`, `_status`) (`README_AGENT.md:523-525`).
- Exceptions (logs, protocol identifiers, persistence keys, on-disk names, probes) are listed
  in `docs/i18n_exclusions.md` (`README_AGENT.md:527-530`). **An `id_salt` is one of these
  exclusions**: it is a persistence key, not a caption — keep it a literal.

Idiom:

```rust
ui.label(t!("settings.general.projects_dir_label"));          // src/general_settings_panel.rs:188
let btn = ui.button(t!("widgets.seed_spin_box.random"));      // src/widgets/seed_spin_box.rs:50
```

## 4. Per-widget state: where it lives, and how ids collide

- `Context::data(|d| …)` / `Context::data_mut(|d| …)` (`egui-0.35.0/src/context.rs:961`, `:967`)
  give an `IdTypeMap`: `insert_temp` / `get_temp` for frame-scoped state,
  `insert_persisted` / `get_persisted` for state serialized across runs.
- `Context::memory(...)` / `memory_mut(...)` (`context.rs:949`, `:955`) hold focus, open
  popups, area order, and the per-widget `Memory` state of built-in widgets.
- Key the map with `ui.make_persistent_id("something")` (`ui.rs:883`) — parent-scoped — not
  with a bare `Id::new("something")` unless you *want* a process-global slot.

Worked example in this repo: the wheel guard is a deliberate **global** temp slot,
`Id::new("wheel_input_open_combo_popup_guard")` written with `data_mut(insert_temp)` and read
with `data(get_temp)` (`src/widgets/wheel_input_guard.rs:21-70`).

**The collision failure mode.** Two widgets that resolve to the same `Id` share one state
slot: one steals the other's open/closed flag, scroll offset, or text cursor; focus ping-pongs
between them; a `Window` snaps to the other's position. It is not a crash — it is a
mysterious, intermittent UI bug. It happens when:

1. Two `from_label`/`Window::new`/`collapsing` widgets carry the same caption (or the same
   translation of two different captions) — fix with distinct `id_salt`s.
2. A widget is built in a loop without `ui.push_id(i, …)` (`ui.rs:2155-2166`).
3. A localized label is used as a salt and two languages collapse two labels onto one string.

In debug builds egui records the id source, so `{:?}` on an `Id` prints the original source
string instead of a hash (`egui-0.35.0/src/id.rs:133`, guarded by `#[cfg(debug_assertions)]`) —
use it when chasing a collision.

## 5. Viewport ids

`ViewportId::from_hash_of(source)` (`egui-0.35.0/src/viewport.rs:153`) is the same
hash-a-source pattern one level up: a deferred/immediate child viewport gets its identity from
whatever you hash. The same rule applies — **never hash a localized title into a
`ViewportId`**; hash a stable key. See `01-app-shell.md` for the viewport lifecycle and how
this app opens child viewports.

## Editing map

- To localize an existing widget label: add the key to **all** of
  `crates/ms-i18n/locales/*.json`, wrap the caption in `t!`, and if the widget is a
  `from_label` / `Window::new` / `CollapsingHeader::new` / `ui.collapsing`, add
  `.id_salt("<the same key>")` in the same edit.
- To add a widget inside a loop: `ui.push_id(index, |ui| …)`.
- To store per-widget state: `ui.make_persistent_id(...)` + `ctx.data_mut(...)`; global,
  frame-scoped signals follow `src/widgets/wheel_input_guard.rs`.
- To debug a state-loss-on-language-switch bug: grep the widget's construction for
  `from_label` / `Window::new` / `collapsing` without a neighbouring `id_salt` / `.id(`.
