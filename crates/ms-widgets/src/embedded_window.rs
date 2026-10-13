/*
File: crates/ms-widgets/src/embedded_window.rs

Purpose:
`EmbeddedWindow`: one secondary window of the application (New Project, PSD import, the Wiki
guide, batch processing) whose content is shown EITHER as a separate OS window (an immediate egui
viewport) OR embedded in the window it was opened from. Embedded, it has no frame and fills the
whole viewport: a title strip on top (title, optional collapse button, close X) and the content
below. While an embedded window is open, nothing beneath it is drawn at all.

Key structures:
- EmbeddedWindowState    : caller-owned open / collapsed state of one window
- EmbeddedWindow         : per-pass builder; `show(ui, content, children)`
- EmbeddedWindowResponse : what the pass produced (content result, drawn, close requested)

Key functions:
- effective_mode() : the ONE decision Separate vs Embedded (user setting + eframe's runtime gate)
- resolve_mode()   : its pure core
- covers_host()    : the host's pre-check "an embedded window covers this viewport"

Notes:
- Stacking: every viewport has one stack of embedded windows (`embedded_window/stack.rs`), kept
  once per `Context` in egui temp data. Only the topmost non-collapsed window draws; the windows
  below it and the host draw nothing. The covering window is decided once per pass (the first
  `covers_host` / `show` of the pass), so at most one window draws per viewport per pass: a
  window opened in pass P first draws in P+1, and a close / collapse / restore takes effect in
  the next pass.
- Embedded mode NEVER calls the viewport API. egui would otherwise wrap the content in a
  floating `egui::Window` (egui-0.36.2/src/context.rs:4124-4127, `show_embedded_viewport` at
  :4174-4185), which is the double-window bug this widget replaces.
- Content state survives covering, collapsing and mode switches: the content `Ui` always has the
  explicit id `Id::new((window id, "content"))` (egui-0.36.2/src/ui.rs:251-252). egui memory
  that is per viewport (areas, focus, popups) resets on a mode switch.
- Keyboard focus: egui drops a focused widget's focus one pass after it stops being drawn
  (egui-0.36.2/src/memory/mod.rs:630-637). The widget drops focus itself whenever the covering
  window of a viewport changes, so a new top window never starts with a hidden widget's focus.
*/

mod stack;

use std::sync::Arc;

use egui::{Id, ViewportId};
use ms_config::secondary_windows::{self, WindowMode};
use ms_log::runtime_log;

use stack::{WindowStacks, escape_closes};

/// egui temp-data key of the per-`Context` [`WindowStacks`]: the one owner of every stack.
const STACKS_KEY: &str = "ms_embedded_window_stacks";

/// Salt of a window's separate-mode `ViewportId` (`(salt, window id)`): an identity, never shown.
const SEPARATE_VIEWPORT_SALT: &str = "ms_embedded_window";

/// Close button glyph of the title strip. A shape, not a word: the localized tooltip carries the
/// meaning (same convention as `CLOSE_GLYPH` in `src/single_image/controller.rs`).
const CLOSE_GLYPH: &str = "✕";

/// Collapse button glyph of the title strip (a "minimize" bar; meaning in the tooltip).
const COLLAPSE_GLYPH: &str = "—";

/// Pure core of [`effective_mode`]: `embed_viewports == true` (no multi-window backend, wasm,
/// Android's runtime fallback) forces `Embedded`; otherwise the requested mode stands.
#[must_use]
pub fn resolve_mode(requested: WindowMode, embed_viewports: bool) -> WindowMode {
    if embed_viewports { WindowMode::Embedded } else { requested }
}

/// Where secondary windows open in this pass: the user setting resolved for the platform
/// (`ms_config::secondary_windows::requested_mode`) combined with `ctx.embed_viewports()`
/// (egui-0.36.2/src/context.rs:4001). The ONE owner of this decision; evaluated every pass, so
/// a setting change or eframe's fallback switches open windows on the next pass.
#[must_use]
pub fn effective_mode(ctx: &egui::Context) -> WindowMode {
    resolve_mode(secondary_windows::requested_mode(), ctx.embed_viewports())
}

/// Pre-check for a HOST (the root UI of a viewport, or the content of a separate window): true
/// iff an embedded window covers the current viewport (`ctx.viewport_id()`) in this pass. A
/// covered host draws nothing of its own (no panels, `Area`s, popups, hotkeys, tutorial) but
/// still polls its workers and still calls `show` on every window it owns.
#[must_use]
pub fn covers_host(ctx: &egui::Context) -> bool {
    decide_current(ctx).is_some()
}

/// Runs `f` on the `Context`'s window stacks (egui temp data; the closure must not touch `ctx`,
/// which is locked while it runs).
fn with_stacks<R>(ctx: &egui::Context, f: impl FnOnce(&mut WindowStacks) -> R) -> R {
    ctx.data_mut(|data| f(data.get_temp_mut_or_default::<WindowStacks>(Id::new(STACKS_KEY))))
}

/// The covering window of the current viewport for this pass (decided at the first call of the
/// pass). When it differs from the previous pass's, keyboard focus is dropped: the focused widget
/// belongs to a window (or host) that no longer draws, and egui would keep it focused for one more
/// pass (egui-0.36.2/src/memory/mod.rs:630-637), eating the new top window's first Escape.
fn decide_current(ctx: &egui::Context) -> Option<Id> {
    let viewport = ctx.viewport_id();
    let pass = ctx.cumulative_pass_nr();
    let outcome = with_stacks(ctx, |stacks| stacks.decide(viewport, pass));
    if outcome.changed {
        ctx.memory_mut(egui::Memory::stop_text_input);
    }
    outcome.covering
}

/// Where a window was last shown, so state changes made outside `show` reach the stack at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    /// On the stack of `viewport`.
    Embedded { viewport: ViewportId },
    /// As the OS window `viewport`, opened from `parent`.
    Separate { viewport: ViewportId, parent: ViewportId },
}

/// A shown window's link back to its `Context` (an `Arc` handle) and stack identity.
#[derive(Clone)]
struct Registration {
    ctx: egui::Context,
    id: Id,
    placement: Placement,
}

impl Registration {
    /// Takes the window off screen: an embedded window leaves its stack together with its
    /// children; a separate window forgets its own viewport's stack (egui restarts the pass
    /// counter of a viewport it recreates). Requests the repaint that shows the result.
    fn unregister(&self) {
        match self.placement {
            Placement::Embedded { viewport } => {
                with_stacks(&self.ctx, |stacks| stacks.remove(viewport, self.id));
                self.ctx.request_repaint_of(viewport);
            }
            Placement::Separate { viewport, parent } => {
                with_stacks(&self.ctx, |stacks| stacks.drop_viewport(viewport));
                self.ctx.request_repaint_of(parent);
            }
        }
    }
}

/// Caller-owned state of one [`EmbeddedWindow`]: whether it is open and collapsed. Closed by
/// default. The caller keeps it across frames (and across mode switches).
#[derive(Default)]
pub struct EmbeddedWindowState {
    open: bool,
    /// Embedded mode only: the window keeps its stack place but draws nothing.
    collapsed: bool,
    /// A widget of this window held keyboard focus at the end of its last drawn pass.
    focus_held: bool,
    /// The "separate window embedded by egui" warning was logged for this opening.
    warned_embedded_class: bool,
    /// Set by `show`, cleared by `close`.
    registration: Option<Registration>,
}

impl std::fmt::Debug for EmbeddedWindowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedWindowState")
            .field("open", &self.open)
            .field("collapsed", &self.collapsed)
            .field("focus_held", &self.focus_held)
            .field("placement", &self.registration.as_ref().map(|registration| registration.placement))
            .finish_non_exhaustive()
    }
}

impl EmbeddedWindowState {
    /// A closed window.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the window is open (collapsed windows are open).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the window is open but collapsed (embedded mode). The CREATOR then draws its own
    /// restore button that calls [`Self::restore`].
    #[must_use]
    pub fn is_collapsed(&self) -> bool {
        self.open && self.collapsed
    }

    /// Opens the window (it appears from the next pass). Opening an already open window
    /// restores it: un-collapses it and brings it to the top, or focuses its OS window.
    pub fn open(&mut self, ctx: &egui::Context) {
        if !self.open {
            self.open = true;
            self.warned_embedded_class = false;
        }
        self.restore(ctx);
    }

    /// Closes the window and, embedded, every window opened from its `children`. Takes effect
    /// on the stack at once, so the window beneath (or the host) draws in the next pass. The
    /// caller's own child states stay as they are; closing them is the caller's job.
    pub fn close(&mut self) {
        self.open = false;
        self.collapsed = false;
        self.focus_held = false;
        if let Some(registration) = self.registration.take() {
            registration.unregister();
        }
    }

    /// Collapses an open window. Embedded: it stops covering from the next pass and keeps its
    /// stack place. Separate: the OS window is minimized instead (`ViewportCommand::Minimized`,
    /// egui-0.36.2/src/viewport.rs:1134) and the state stays uncollapsed.
    pub fn collapse(&mut self) {
        if !self.open {
            return;
        }
        match self.registration.as_ref().map(|registration| (registration, registration.placement)) {
            Some((registration, Placement::Separate { viewport, .. })) => {
                registration.ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Minimized(true));
            }
            Some((registration, Placement::Embedded { viewport })) => {
                self.collapsed = true;
                with_stacks(&registration.ctx, |stacks| stacks.set_collapsed(viewport, registration.id, true));
                registration.ctx.request_repaint_of(viewport);
            }
            None => self.collapsed = true,
        }
    }

    /// Un-collapses the window and raises it (with its own children) above every window of its
    /// viewport; it covers from the next pass. Separate: un-minimizes and focuses the OS window
    /// (`Minimized(false)` + `Focus`, egui-0.36.2/src/viewport.rs:1134, :1169).
    pub fn restore(&mut self, ctx: &egui::Context) {
        self.collapsed = false;
        match self.registration.as_ref().map(|registration| (registration.id, registration.placement)) {
            Some((id, Placement::Embedded { viewport })) => {
                with_stacks(ctx, |stacks| {
                    stacks.set_collapsed(viewport, id, false);
                    stacks.raise(viewport, id);
                });
                ctx.request_repaint_of(viewport);
            }
            Some((_, Placement::Separate { viewport, .. })) => {
                ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::Focus);
            }
            None => ctx.request_repaint(),
        }
    }
}

/// What one `show` pass produced.
#[derive(Debug)]
pub struct EmbeddedWindowResponse<R> {
    /// The content closure's result; `None` when the content did not run this pass.
    pub inner: Option<R>,
    /// Whether the window's content ran this pass (it is the visible top of its viewport).
    pub drawn: bool,
    /// The user asked to close the window (X, Escape, or the OS close button). A report only:
    /// the caller closes the state, or keeps it open to veto the close.
    pub close_requested: bool,
}

impl<R> EmbeddedWindowResponse<R> {
    /// A pass in which the window showed nothing.
    fn hidden() -> Self {
        Self { inner: None, drawn: false, close_requested: false }
    }
}

/// One secondary window, built every pass and shown with [`EmbeddedWindow::show`].
#[must_use = "an EmbeddedWindow does nothing until `show` is called"]
pub struct EmbeddedWindow<'a> {
    id: &'static str,
    title: String,
    state: &'a mut EmbeddedWindowState,
    viewport: egui::ViewportBuilder,
    collapsible: bool,
    maximize_on_open: bool,
    close_on_escape: bool,
    content_style: Option<Arc<egui::Style>>,
}

impl<'a> EmbeddedWindow<'a> {
    /// A window identified by `id` (a literal identity, never a localized text: it keys the
    /// stack, the content `Ui` and the separate viewport) with the localized `title`.
    pub fn new(id: &'static str, title: impl Into<String>, state: &'a mut EmbeddedWindowState) -> Self {
        Self {
            id,
            title: title.into(),
            state,
            viewport: egui::ViewportBuilder::default(),
            collapsible: false,
            maximize_on_open: false,
            close_on_escape: false,
            content_style: None,
        }
    }

    /// OS window attributes for separate mode (size, minimum size, app id, icon, buttons). The
    /// title is always the window's `title`.
    pub fn viewport(mut self, builder: egui::ViewportBuilder) -> Self {
        self.viewport = builder;
        self
    }

    /// Embedded mode: shows a collapse button in the title strip. The creator must then offer a
    /// restore button while [`EmbeddedWindowState::is_collapsed`]. Separate windows use the OS
    /// minimize button instead.
    pub fn collapsible(mut self, on: bool) -> Self {
        self.collapsible = on;
        self
    }

    /// Separate mode: open the OS window maximized. Windows misplaces a window created
    /// maximized, so there the maximize is sent on the window's first frame instead.
    pub fn maximize_on_open(mut self, on: bool) -> Self {
        self.maximize_on_open = on;
        self
    }

    /// Escape closes the window (reported as `close_requested`) while it is the visible top,
    /// no popup is open and no widget holds keyboard focus. Off by default.
    pub fn close_on_escape(mut self, on: bool) -> Self {
        self.close_on_escape = on;
        self
    }

    /// The content draws under this style: its `Ui`, and through
    /// `ms_theme::with_scoped_global_style` the popups and `Area`s it opens. The title strip
    /// keeps the host's style.
    pub fn content_style(mut self, style: Arc<egui::Style>) -> Self {
        self.content_style = Some(style);
        self
    }

    /// Shows the window for this pass on `ui`, the root `Ui` of the viewport it was opened in.
    ///
    /// `content` runs only when the window is visible (`drawn`). `children` runs on every pass
    /// the window is open, after `content` and also while the window is covered or collapsed; it
    /// is where windows opened FROM this one are shown, so they stack above it. A closed window
    /// runs neither. Must be called every pass while the state is open (a window not shown for a
    /// pass drops off its stack).
    pub fn show<R>(self, ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R, children: impl FnOnce(&mut egui::Ui)) -> EmbeddedWindowResponse<R> {
        if !self.state.open {
            if let Some(registration) = self.state.registration.take() {
                registration.unregister();
            }
            return EmbeddedWindowResponse::hidden();
        }
        let ctx = ui.ctx().clone();
        match effective_mode(&ctx) {
            WindowMode::Embedded => self.show_embedded(ui, &ctx, content, children),
            WindowMode::Separate => self.show_separate(&ctx, content, children),
        }
    }

    /// Embedded pass: register on the current viewport's stack, draw only when this window is
    /// the pass's covering window, then run `children` inside this window's parent scope.
    fn show_embedded<R>(self, ui: &mut egui::Ui, ctx: &egui::Context, content: impl FnOnce(&mut egui::Ui) -> R, children: impl FnOnce(&mut egui::Ui)) -> EmbeddedWindowResponse<R> {
        let Self { id, title, state, collapsible, close_on_escape, content_style, .. } = self;
        let viewport = ctx.viewport_id();
        let window_id = Id::new(id);
        let placement = Placement::Embedded { viewport };
        if let Some(previous) = state.registration.take_if(|registration| registration.placement != placement) {
            runtime_log::log_info(format!("[embedded-window] '{id}' now shows embedded (was {:?})", previous.placement));
            previous.unregister();
        }
        // Decide BEFORE touching: a window that enters the stack in this pass never covers in
        // this pass, so whatever already drew (the host, or the parent's content) is not drawn
        // over; the push requests the pass in which it shows.
        let covering = decide_current(ctx);
        let pass = ctx.cumulative_pass_nr();
        let collapsed = state.collapsed;
        if with_stacks(ctx, |stacks| stacks.touch(viewport, window_id, collapsed, pass)) {
            ctx.request_repaint_of(viewport);
        }
        state.registration = Some(Registration { ctx: ctx.clone(), id: window_id, placement });

        let mut response = EmbeddedWindowResponse::hidden();
        if covering == Some(window_id) && !collapsed {
            // Read before the content runs: egui already dropped the focus on this pass's Escape
            // (egui-0.36.2/src/memory/mod.rs:599-602), so the previous pass's focus decides
            // whether this Escape only left a text field.
            let escape_allowed = !state.focus_held && ctx.memory(|memory| memory.focused().is_none()) && !egui::Popup::is_any_open(ctx);
            let chrome = draw_title_strip(ui, window_id, &title, collapsible);
            let fill = content_style.as_deref().map_or(ui.visuals().panel_fill, |style| style.visuals.panel_fill);
            let content_id = content_ui_id(id);
            let inner = egui::CentralPanel::default().frame(egui::Frame::new().fill(fill)).show(ui, |ui| run_content(ctx, ui, content_id, content_style, content)).inner;
            response.inner = Some(inner);
            response.drawn = true;
            response.close_requested = chrome.close_clicked || escape_pressed(ctx, close_on_escape, escape_allowed);
            state.focus_held = ctx.memory(|memory| memory.focused().is_some());
            if chrome.collapse_clicked {
                state.collapse();
            }
        } else {
            state.focus_held = false;
        }

        with_stacks(ctx, |stacks| stacks.push_parent(viewport, window_id));
        children(ui);
        with_stacks(ctx, |stacks| stacks.pop_parent(viewport));
        response
    }

    /// Separate pass: an immediate viewport (state is not `Send + Sync`, so not deferred). Inside
    /// it the window is the HOST of its own viewport: its content runs unless an embedded window
    /// covers that viewport, and `children` always runs there.
    fn show_separate<R>(self, ctx: &egui::Context, content: impl FnOnce(&mut egui::Ui) -> R, children: impl FnOnce(&mut egui::Ui)) -> EmbeddedWindowResponse<R> {
        let Self { id, title, state, viewport: builder, maximize_on_open, close_on_escape, content_style, .. } = self;
        let own_viewport = ViewportId::from_hash_of((SEPARATE_VIEWPORT_SALT, id));
        let placement = Placement::Separate { viewport: own_viewport, parent: ctx.viewport_id() };
        let previous = state.registration.take();
        if let Some(previous) = previous.as_ref().filter(|registration| !matches!(registration.placement, Placement::Separate { .. })) {
            runtime_log::log_info(format!("[embedded-window] '{id}' now shows as a separate window (was {:?})", previous.placement));
            previous.unregister();
        }
        let first_frame = !previous.is_some_and(|registration| matches!(registration.placement, Placement::Separate { .. }));
        // An embedded collapse has no meaning for an OS window: it shows normally (the OS
        // minimize button replaces the collapse button).
        state.collapsed = false;
        state.registration = Some(Registration { ctx: ctx.clone(), id: Id::new(id), placement });

        let mut builder = builder.with_title(title);
        // Windows misplaces a window created maximized, so there the maximize is deferred to the
        // window's first frame (below).
        let defer_maximize = cfg!(target_os = "windows");
        if maximize_on_open && !defer_maximize {
            builder = builder.with_maximized(true);
        }
        let content_id = content_ui_id(id);
        let mut content = Some(content);
        let mut children = Some(children);
        let mut response = EmbeddedWindowResponse::hidden();
        ctx.show_viewport_immediate(own_viewport, builder, |ui, class| {
            let child_ctx = ui.ctx().clone();
            if maximize_on_open && defer_maximize && first_frame {
                child_ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                child_ctx.request_repaint();
            }
            match class {
                egui::ViewportClass::EmbeddedWindow => {
                    // `effective_mode` said Separate, but the backend has no immediate-viewport
                    // renderer, so egui wrapped us in a floating `Window` anyway
                    // (egui-0.36.2/src/context.rs:4130-4136). Draw the content straight into that
                    // wrapper (a `CentralPanel` there would fill the whole root viewport).
                    if !state.warned_embedded_class {
                        state.warned_embedded_class = true;
                        runtime_log::log_warn(format!("[embedded-window] '{id}' was asked for a separate window, but the backend embedded it in an egui Window; drawing it there"));
                    }
                    if let Some(content) = content.take() {
                        response.inner = Some(run_content(&child_ctx, ui, content_id, content_style.clone(), content));
                        response.drawn = true;
                    }
                    state.focus_held = false;
                }
                egui::ViewportClass::Root | egui::ViewportClass::Deferred | egui::ViewportClass::Immediate => {
                    response.close_requested = child_ctx.input(|input| input.viewport().close_requested());
                    if covers_host(&child_ctx) {
                        state.focus_held = false;
                    } else if let Some(content) = content.take() {
                        let escape_allowed = !state.focus_held && child_ctx.memory(|memory| memory.focused().is_none()) && !egui::Popup::is_any_open(&child_ctx);
                        let fill = content_style.as_deref().map_or(ui.visuals().panel_fill, |style| style.visuals.panel_fill);
                        let inner = egui::CentralPanel::default().frame(egui::Frame::new().fill(fill)).show(ui, |ui| run_content(&child_ctx, ui, content_id, content_style.clone(), content)).inner;
                        response.inner = Some(inner);
                        response.drawn = true;
                        response.close_requested |= escape_pressed(&child_ctx, close_on_escape, escape_allowed);
                        state.focus_held = child_ctx.memory(|memory| memory.focused().is_some());
                    }
                }
            }
            if let Some(children) = children.take() {
                children(ui);
            }
        });
        response
    }
}

/// The explicit id of a window's content `Ui`, identical in both modes and every stack position.
fn content_ui_id(window_id: &'static str) -> Id {
    Id::new((window_id, "content"))
}

/// Runs `content` in a child `Ui` with the explicit [`content_ui_id`], under `style` when given
/// (on the `Ui`, and as the scoped global style for the popups and `Area`s it opens).
fn run_content<R>(ctx: &egui::Context, ui: &mut egui::Ui, content_id: Id, style: Option<Arc<egui::Style>>, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.scope_builder(egui::UiBuilder::new().id(content_id), |ui| match style {
        Some(style) => ms_theme::with_scoped_global_style(ctx, Arc::clone(&style), || {
            ui.set_style(style);
            content(ui)
        }),
        None => content(ui),
    })
    .inner
}

/// Consumes this pass's Escape for the window when [`escape_closes`] allows it. Runs after the
/// content, so a content widget that consumed Escape itself keeps it.
fn escape_pressed(ctx: &egui::Context, enabled: bool, escape_allowed: bool) -> bool {
    escape_closes(true, enabled, escape_allowed) && ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

/// Clicks on the embedded title strip's buttons.
#[derive(Debug, Default, Clone, Copy)]
struct ChromeClicks {
    close_clicked: bool,
    collapse_clicked: bool,
}

/// Draws the embedded title strip (a top panel in the host's style): close X and the optional
/// collapse button on the right, the title truncated into the rest.
fn draw_title_strip(ui: &mut egui::Ui, window_id: Id, title: &str, collapsible: bool) -> ChromeClicks {
    let mut clicks = ChromeClicks::default();
    egui::Panel::top(window_id.with("ms_embedded_window_title")).show(ui, |ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            clicks.close_clicked = ui.button(CLOSE_GLYPH).on_hover_text(t!("widgets.embedded_window.close_tooltip")).clicked();
            if collapsible {
                clicks.collapse_clicked = ui.button(COLLAPSE_GLYPH).on_hover_text(t!("widgets.embedded_window.collapse_tooltip")).clicked();
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(egui::RichText::new(title).strong()).truncate());
            });
        });
    });
    clicks
}

#[cfg(test)]
mod tests;
