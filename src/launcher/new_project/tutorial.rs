/*
File: src/launcher/new_project/tutorial.rs

Purpose:
Branching tutorial for the "New project" window (`TutorialId::NewProject`). It
demonstrates the engine's branching (an intro that forks) and gating (steps that
trigger a real pipeline op and wait for it to finish).

The window's pipeline triggers are private `&mut self` methods, so the step
script cannot hold a reference to the window. Instead the tutorial context
`NpTutorialCtx` is a per-frame COMMAND SINK + STATE SNAPSHOT: `on_enter` pushes
`NpTutorialCommand`s (drained and executed by the window after `sync`), and gates
read the snapshot booleans. Keys here MUST match the `mark` calls in `window.rs`.

Two branches:
- Visual: download a test chapter, stitch+cut it, run waifu2x — each step waits
  for its op to finish before advancing.
- Explain: no processing; switch to the full panel and describe each section.
*/

use crate::tutorial::TutorialStep;

/// Per-frame context for the new-project tutorial: a snapshot the gates read plus
/// a command queue `on_enter` writes. Owned (no borrows of the window), so it can
/// be `C` in `TutorialController<NpTutorialCtx>`.
pub struct NpTutorialCtx {
    /// A pipeline op is running (`active_progress.is_some()`); gates wait on this.
    pub busy: bool,
    /// The ribbon has pages (a download/import produced something to process).
    pub ribbon_has_pages: bool,
    /// The waifu2x runtime is available (skip triggering it if not).
    pub waifu_available: bool,
    /// Actions requested this frame, executed by the window after `sync`.
    pub commands: Vec<NpTutorialCommand>,
}

/// An action the tutorial asks the window to perform. The window matches these on
/// `&mut self` after `sync` returns (so the tutorial never borrows the window).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NpTutorialCommand {
    /// Show the step-based simple panel (where the test-chapter button lives).
    SwitchToSimple,
    /// Show the full panel (all sections visible at once for highlighting).
    SwitchToFull,
    /// Download the built-in test chapter.
    StartTestDownload,
    /// Stitch the ribbon and auto-cut it into pages.
    StartStitchAutoCut,
    /// Run the pages through waifu2x.
    StartWaifu2x,
}

// Target keys — must match `window.rs` `mark(...)` sites.
pub const TARGET_MODE_TABS: &str = "np_mode_tabs";
pub const TARGET_TEST_DOWNLOAD: &str = "np_test_download";
pub const TARGET_IMPORT: &str = "np_import";
pub const TARGET_QUICK: &str = "np_quick";
pub const TARGET_STITCH: &str = "np_stitch";
pub const TARGET_WAIFU: &str = "np_waifu";

/// Build the branching new-project tutorial.
#[must_use]
pub fn steps() -> Vec<TutorialStep<NpTutorialCtx>> {
    vec![
        // ---- Intro: fork on how to present the window ----
        TutorialStep::message(
            t!("launcher.new_project.tutorial.intro_title"),
            t!("launcher.new_project.tutorial.intro_message"),
        )
        .id("np_intro")
        .choice(t!("launcher.new_project.tutorial.show_live_choice"), "np_vis_download")
        .choice(t!("launcher.new_project.tutorial.just_tell_choice"), "np_exp_simple"),
        // ================= VISUAL BRANCH =================
        TutorialStep::new(
            [TARGET_TEST_DOWNLOAD],
            t!("launcher.new_project.tutorial.download_title"),
            t!("launcher.new_project.tutorial.download_message"),
        )
        .id("np_vis_download")
        .on_enter(|c: &mut NpTutorialCtx| {
            c.commands.push(NpTutorialCommand::SwitchToSimple);
            c.commands.push(NpTutorialCommand::StartTestDownload);
        })
        .await_gate(|g| !g.ctx.busy),
        TutorialStep::new(
            [TARGET_STITCH],
            t!("launcher.new_project.tutorial.stitch_title"),
            t!("launcher.new_project.tutorial.stitch_message"),
        )
        .id("np_vis_stitch")
        .on_enter(|c: &mut NpTutorialCtx| {
            c.commands.push(NpTutorialCommand::SwitchToFull);
            if c.ribbon_has_pages {
                c.commands.push(NpTutorialCommand::StartStitchAutoCut);
            }
        })
        .await_gate(|g| !g.ctx.busy),
        TutorialStep::new(
            [TARGET_WAIFU],
            t!("launcher.new_project.tutorial.waifu2x_title"),
            t!("launcher.new_project.tutorial.waifu2x_message"),
        )
        .id("np_vis_waifu")
        .on_enter(|c: &mut NpTutorialCtx| {
            if c.waifu_available && c.ribbon_has_pages {
                c.commands.push(NpTutorialCommand::StartWaifu2x);
            }
        })
        .await_gate(|g| !g.ctx.busy),
        TutorialStep::message(
            t!("launcher.new_project.tutorial.done_title"),
            t!("launcher.new_project.tutorial.done_message"),
        )
        .id("np_vis_done")
        .finish(),
        // ================= EXPLAIN BRANCH =================
        TutorialStep::new(
            [TARGET_MODE_TABS],
            t!("launcher.new_project.tutorial.simple_mode_title"),
            t!("launcher.new_project.tutorial.simple_mode_message"),
        )
        .id("np_exp_simple"),
        TutorialStep::new(
            [TARGET_IMPORT],
            t!("launcher.new_project.tutorial.import_title"),
            t!("launcher.new_project.tutorial.import_message"),
        )
        .id("np_exp_import")
        .on_enter(|c: &mut NpTutorialCtx| c.commands.push(NpTutorialCommand::SwitchToFull)),
        TutorialStep::new(
            [TARGET_QUICK],
            t!("launcher.new_project.tutorial.downloaders_title"),
            t!("launcher.new_project.tutorial.downloaders_message"),
        )
        .id("np_exp_quick"),
        TutorialStep::new(
            [TARGET_STITCH],
            t!("launcher.new_project.tutorial.stitch_split_title"),
            t!("launcher.new_project.tutorial.stitch_split_message"),
        )
        .id("np_exp_stitch"),
        TutorialStep::new(
            [TARGET_WAIFU],
            t!("launcher.new_project.tutorial.processing_title"),
            t!("launcher.new_project.tutorial.processing_message"),
        )
        .id("np_exp_process"),
        TutorialStep::message(
            t!("launcher.new_project.tutorial.finish_title"),
            t!("launcher.new_project.tutorial.finish_message"),
        )
        .id("np_exp_done")
        .finish(),
    ]
}
