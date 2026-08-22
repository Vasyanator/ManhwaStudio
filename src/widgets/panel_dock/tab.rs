/*
File: src/widgets/panel_dock/tab.rs

Purpose:
Widget #1 of the dockable-panel system: `PanelTab`, the per-frame DECLARATION of
one tab. A tab is not a container the caller nests into — it is a statement of
"this tab exists this frame, here is its title, its size wishes and its body".
The dock decides which panel draws it and when.

Main responsibilities:
- collect one tab's per-frame metadata (title, visibility, transparency mode,
  min/initial size);
- hand the body closure to the dock's queue instead of drawing it immediately.

Key structures:
- `PanelTab`: the builder returned by `PanelDock::tab`.

Key functions:
- `PanelTab::title`, `visible`, `transparent_until_hover`, `min_size`,
  `initial_size`, `show`, `show_with_extras`.

Notes:
`show` does NOT draw. It stores `Box<dyn FnOnce(&mut Ui, &mut C, &mut TabExtras)
+ 'frame>` in the dock, which runs it from `PanelDock::end` in panel order. A
body declared through `show` never sees the third argument — `show_with_extras`
is the declaration for a tab that wants the persisted per-tab state the dock
keeps for it. The body captures
none of the caller's state: it receives the caller's per-frame context `C`, which
`end` lends to one body at a time. That is what lets every tab of one frame reach
the same heavy caller state without cloning it or wrapping it in a `RefCell`.
*/

use egui::Vec2;

use super::extras::TabExtras;
use super::model::TabId;
use super::{PanelDock, TabMeta, TabTitle};

/// One tab declared for the current frame.
///
/// Created by [`PanelDock::tab`] — a declaration only makes sense with the dock
/// it is declared into, so there is no free constructor. Dropping the builder
/// without calling [`PanelTab::show`] declares nothing: the tab keeps whatever
/// slot it already has in the layout and is simply not drawn this frame.
///
/// Lifetimes: `'dock` borrows the dock for the duration of the builder chain,
/// while `'ctx` / `'frame` are the dock's own. `C` is the dock's per-frame
/// context type, which the body receives instead of capturing caller state.
pub struct PanelTab<'dock, 'ctx, 'frame, C> {
    dock: &'dock mut PanelDock<'ctx, 'frame, C>,
    id: TabId,
    title: Option<TabTitle<'frame>>,
    meta: TabMeta,
}

impl<'dock, 'ctx, 'frame, C> PanelTab<'dock, 'ctx, 'frame, C> {
    /// Starts a declaration of `id` into `dock`. Called by [`PanelDock::tab`].
    pub(super) fn new(dock: &'dock mut PanelDock<'ctx, 'frame, C>, id: TabId) -> Self {
        Self {
            dock,
            id,
            title: None,
            meta: TabMeta::default(),
        }
    }

    /// Sets the header caption, produced lazily once per frame.
    ///
    /// The closure is the localisation point: it is evaluated only when the tab
    /// is actually drawn, so a hidden tab costs no catalog lookup. Pass
    /// `|| t!("…")` — anything convertible into `String` is accepted. When no
    /// title is set, the tab's literal key is shown, which is a bug marker, not
    /// a fallback worth relying on.
    #[must_use]
    pub fn title<F, S>(mut self, title: F) -> Self
    where
        F: Fn() -> S + 'frame,
        S: Into<String>,
    {
        self.title = Some(Box::new(move || title().into()));
        self
    }

    /// Declares whether the tab is drawn this frame (default `true`).
    ///
    /// A hidden tab keeps its slot in the layout: its panel remembers it, its
    /// header is not shown, and a panel whose tabs are all hidden is not drawn —
    /// but neither the tab nor the panel is deleted. Deletion is a user action,
    /// never a visibility consequence.
    #[must_use]
    pub fn visible(mut self, visible: bool) -> Self {
        self.meta.visible = visible;
        self
    }

    /// Declares that the panel drawing this tab keeps its CHROME invisible
    /// while the pointer is elsewhere (default `false`).
    ///
    /// What disappears is everything drawn AROUND the content: the panel's own
    /// frame (background, border, shadow), the collapse arrow, the drag grip,
    /// the resize grip and the body's scroll bars. The tab's BODY is drawn
    /// exactly as always — including any scroll area of its own — and a panel
    /// showing more than one caption keeps its captions fully opaque, because
    /// they are then the only thing that says which tab is on screen. Moving the
    /// pointer over the panel fades the chrome back in, and the panel can be
    /// dragged, docked and resized in both states.
    ///
    /// It is a per-frame declaration like [`PanelTab::visible`], not a stored
    /// property: the panel is transparent on the frames the tab it actually
    /// DRAWS asks for it, so a panel switched to another tab is opaque again.
    ///
    /// The mode changes painting only. The panel occupies the same rect, is laid
    /// out identically and keeps intercepting pointer input over that rect —
    /// an invisible panel still shields whatever is underneath it.
    #[must_use]
    pub fn transparent_until_hover(mut self, transparent: bool) -> Self {
        self.meta.transparent_until_hover = transparent;
        self
    }

    /// Lower bound on the OUTER size of the panel while this tab is active,
    /// header and frame margins included, in points.
    ///
    /// The solver never shrinks the panel below this height; the body scrolls
    /// instead.
    #[must_use]
    pub fn min_size(mut self, min_size: Vec2) -> Self {
        self.meta.min_size = Some(min_size);
        self
    }

    /// Outer size, in points, used for the panel before this tab has ever been
    /// measured — the "content defines the initial size" input.
    ///
    /// After the first frame the measured content size takes over, and a manual
    /// resize (`PanelNode::size_override`) takes over from that.
    #[must_use]
    pub fn initial_size(mut self, initial_size: Vec2) -> Self {
        self.meta.initial_size = Some(initial_size);
        self
    }

    /// Queues the tab's body and ends the declaration.
    ///
    /// `body` is NOT called here. It is stored and invoked from
    /// [`PanelDock::end`], with the caller's per-frame context, if — and only if
    /// — this tab is the drawn tab of a drawn, expanded panel. Declaring the
    /// same [`TabId`] twice in one frame
    /// keeps the FIRST declaration and drops this one (with a warning): a
    /// duplicate is a programming error, and silently letting the second
    /// declaration win would make which body runs depend on call order.
    ///
    /// A tab that needs the persisted per-tab state the dock keeps for it
    /// declares its body through [`PanelTab::show_with_extras`] instead.
    pub fn show(self, body: impl FnOnce(&mut egui::Ui, &mut C) + 'frame) {
        self.show_with_extras(move |ui, cx, _extras| body(ui, cx));
    }

    /// Queues the tab's body WITH its [`TabExtras`] and ends the declaration.
    ///
    /// Identical to [`PanelTab::show`] in every respect but the third argument:
    /// the bag of extra per-tab state the dock stores for this tab, in this
    /// program tab, and persists in the `PanelLayout` section of the config next
    /// to the arrangement. Read it with [`TabExtras::flag`] and write what the
    /// body currently shows with [`TabExtras::set_flag`] — writing every frame is
    /// the expected usage, and only a value that really moved marks the dock
    /// dirty.
    ///
    /// The bag is handed to the DRAWN tab of a drawn, expanded panel — the same
    /// condition under which the body runs at all — so a tab that is hidden, not
    /// the active one, or collapsed simply keeps whatever it stored last.
    pub fn show_with_extras(
        self,
        body: impl FnOnce(&mut egui::Ui, &mut C, &mut TabExtras) + 'frame,
    ) {
        let Self {
            dock,
            id,
            title,
            meta,
        } = self;
        let title = title.unwrap_or_else(|| Box::new(move || id.as_str().to_owned()));
        dock.declare(id, meta, title, Box::new(body));
    }
}
