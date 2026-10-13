//! Headless egui tests of `EmbeddedWindow`: mode resolution and the stacking contract on a
//! `Context::default()`, whose `embed_viewports` is true (egui-0.36.2/src/context.rs:4001), so
//! every window there is embedded.

use std::cell::Cell;

use egui::{Id, RawInput};
use ms_config::secondary_windows::WindowMode;

use super::{EmbeddedWindow, EmbeddedWindowState, content_ui_id, covers_host, effective_mode, resolve_mode};

/// Runs one headless pass. The font atlas upload of the first pass is never applied without a
/// renderer, and epaint panics when an unapplied `TexturesDelta` is dropped, so it is cleared.
fn run_pass(ctx: &egui::Context, input: RawInput, run_ui: impl FnMut(&mut egui::Ui)) {
    ctx.run_ui(input, run_ui).textures_delta.clear();
}

/// What one pass of the parent/child scene drew.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Drawn {
    host: bool,
    parent: bool,
    child: bool,
    parent_children_ran: bool,
}

/// One pass of a host with a parent window whose `children` closure shows a child window.
fn scene_pass(ctx: &egui::Context, parent: &mut EmbeddedWindowState, child: &mut EmbeddedWindowState, input: RawInput) -> Drawn {
    let host = Cell::new(false);
    let parent_drawn = Cell::new(false);
    let child_drawn = Cell::new(false);
    let children_ran = Cell::new(false);
    run_pass(ctx, input, |ui| {
        if !covers_host(ui.ctx()) {
            host.set(true);
        }
        EmbeddedWindow::new("test.embedded_window.parent", "Parent", &mut *parent).collapsible(true).show(
            ui,
            |_| parent_drawn.set(true),
            |ui| {
                children_ran.set(true);
                EmbeddedWindow::new("test.embedded_window.child", "Child", &mut *child).collapsible(true).show(ui, |_| child_drawn.set(true), |_| {});
            },
        );
    });
    Drawn { host: host.get(), parent: parent_drawn.get(), child: child_drawn.get(), parent_children_ran: children_ran.get() }
}

fn idle(ctx: &egui::Context, parent: &mut EmbeddedWindowState, child: &mut EmbeddedWindowState) -> Drawn {
    scene_pass(ctx, parent, child, RawInput::default())
}

fn escape_input() -> RawInput {
    RawInput {
        events: vec![egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }],
        ..RawInput::default()
    }
}

#[test]
fn resolve_mode_forces_embedded_without_native_viewports() {
    assert_eq!(resolve_mode(WindowMode::Separate, false), WindowMode::Separate);
    assert_eq!(resolve_mode(WindowMode::Embedded, false), WindowMode::Embedded);
    assert_eq!(resolve_mode(WindowMode::Separate, true), WindowMode::Embedded);
    assert_eq!(resolve_mode(WindowMode::Embedded, true), WindowMode::Embedded);
    assert_eq!(effective_mode(&egui::Context::default()), WindowMode::Embedded, "a headless context embeds viewports");
}

#[test]
fn closed_windows_draw_nothing_and_leave_the_host_alone() {
    let ctx = egui::Context::default();
    let (mut parent, mut child) = (EmbeddedWindowState::new(), EmbeddedWindowState::new());
    let drawn = idle(&ctx, &mut parent, &mut child);
    assert_eq!(drawn, Drawn { host: true, ..Drawn::default() }, "a closed parent runs neither content nor children");
}

#[test]
fn only_the_topmost_window_draws_and_children_always_run() {
    let ctx = egui::Context::default();
    let (mut parent, mut child) = (EmbeddedWindowState::new(), EmbeddedWindowState::new());
    parent.open(&ctx);
    // The pass that opens a window still belongs to what was drawn before it.
    assert_eq!(idle(&ctx, &mut parent, &mut child), Drawn { host: true, parent_children_ran: true, ..Drawn::default() });
    assert_eq!(idle(&ctx, &mut parent, &mut child), Drawn { parent: true, parent_children_ran: true, ..Drawn::default() });

    child.open(&ctx);
    assert_eq!(idle(&ctx, &mut parent, &mut child), Drawn { parent: true, parent_children_ran: true, ..Drawn::default() });
    let covered = idle(&ctx, &mut parent, &mut child);
    assert_eq!(covered, Drawn { child: true, parent_children_ran: true, ..Drawn::default() }, "the covered parent draws nothing, its children still run");

    // Closing the child uncovers exactly one level.
    child.close();
    assert_eq!(idle(&ctx, &mut parent, &mut child), Drawn { parent: true, parent_children_ran: true, ..Drawn::default() });
    parent.close();
    assert_eq!(idle(&ctx, &mut parent, &mut child), Drawn { host: true, ..Drawn::default() });
}

#[test]
fn collapsing_the_child_uncovers_the_parent_and_restore_raises_it() {
    let ctx = egui::Context::default();
    let (mut parent, mut child) = (EmbeddedWindowState::new(), EmbeddedWindowState::new());
    parent.open(&ctx);
    child.open(&ctx);
    idle(&ctx, &mut parent, &mut child);
    assert!(idle(&ctx, &mut parent, &mut child).child);

    child.collapse();
    assert!(child.is_collapsed() && child.is_open());
    let collapsed = idle(&ctx, &mut parent, &mut child);
    assert_eq!(collapsed, Drawn { parent: true, parent_children_ran: true, ..Drawn::default() });

    child.restore(&ctx);
    assert!(!child.is_collapsed());
    assert!(idle(&ctx, &mut parent, &mut child).child);
}

#[test]
fn closing_the_parent_prunes_the_child_and_reopening_restores_the_stack() {
    let ctx = egui::Context::default();
    let (mut parent, mut child) = (EmbeddedWindowState::new(), EmbeddedWindowState::new());
    parent.open(&ctx);
    child.open(&ctx);
    idle(&ctx, &mut parent, &mut child);
    assert!(idle(&ctx, &mut parent, &mut child).child);

    parent.close();
    assert_eq!(idle(&ctx, &mut parent, &mut child), Drawn { host: true, ..Drawn::default() }, "the child left the stack with its parent");
    assert!(child.is_open(), "the child's own state is the caller's to close");

    parent.open(&ctx);
    idle(&ctx, &mut parent, &mut child);
    assert_eq!(idle(&ctx, &mut parent, &mut child), Drawn { child: true, parent_children_ran: true, ..Drawn::default() });
}

#[test]
fn content_ui_has_the_same_explicit_id_in_every_position_and_mode() {
    let expected = content_ui_id("test.embedded_window.id");
    assert_eq!(expected, Id::new(("test.embedded_window.id", "content")));
    let ctx = egui::Context::default();
    let mut state = EmbeddedWindowState::new();
    state.open(&ctx);
    let seen = Cell::new(None);
    for _ in 0..2 {
        run_pass(&ctx, RawInput::default(), |ui| {
            EmbeddedWindow::new("test.embedded_window.id", "Id", &mut state).show(ui, |ui| seen.set(Some(ui.id())), |_| {});
        });
    }
    assert_eq!(seen.get(), Some(expected), "embedded");

    // With `embed_viewports` off a desktop requests Separate, but a headless context has no
    // immediate-viewport renderer, so egui falls back to its embedded `Window`
    // (egui-0.36.2/src/context.rs:4130-4136); the content id must not change either way.
    ctx.set_embed_viewports(false);
    seen.set(None);
    for _ in 0..2 {
        run_pass(&ctx, RawInput::default(), |ui| {
            EmbeddedWindow::new("test.embedded_window.id", "Id", &mut state).show(ui, |ui| seen.set(Some(ui.id())), |_| {});
        });
    }
    assert_eq!(seen.get(), Some(expected), "{:?} mode", effective_mode(&ctx));
}

#[test]
fn escape_closes_the_top_window_only_after_a_focused_field_let_go() {
    let ctx = egui::Context::default();
    let mut state = EmbeddedWindowState::new();
    state.open(&ctx);
    let field_id = Id::new("test.embedded_window.field");
    let mut text = String::new();
    let mut pass = |input: RawInput| {
        let close_requested = Cell::new(false);
        run_pass(&ctx, input, |ui| {
            let response = EmbeddedWindow::new("test.embedded_window.esc", "Esc", &mut state).close_on_escape(true).show(
                ui,
                |ui| {
                    ui.add(egui::TextEdit::singleline(&mut text).id(field_id));
                },
                |_| {},
            );
            close_requested.set(response.close_requested);
        });
        close_requested.get()
    };
    pass(RawInput::default());
    assert!(!pass(RawInput::default()));
    ctx.memory_mut(|memory| memory.request_focus(field_id));
    assert!(!pass(RawInput::default()));
    assert!(!pass(escape_input()), "the first Escape only leaves the focused field");
    assert!(pass(escape_input()), "the second Escape closes the window");
}

#[test]
fn escape_is_ignored_unless_enabled() {
    let ctx = egui::Context::default();
    let mut state = EmbeddedWindowState::new();
    state.open(&ctx);
    let mut pass = |input: RawInput| {
        let outcome = Cell::new((false, false));
        run_pass(&ctx, input, |ui| {
            let response = EmbeddedWindow::new("test.embedded_window.no_esc", "No Esc", &mut state).show(ui, |_| {}, |_| {});
            outcome.set((response.drawn, response.close_requested));
        });
        outcome.get()
    };
    pass(RawInput::default());
    assert_eq!(pass(escape_input()), (true, false), "close_on_escape defaults to off");
}
