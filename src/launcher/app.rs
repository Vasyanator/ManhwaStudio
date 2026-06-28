/*
File: src/launcher/app.rs

Purpose:
Root `eframe::App` for the Rust launcher test mode.

Main responsibilities:
- own launcher shell state;
- drive background image plan generation and lazy batch decoding;
- render the animated multi-column background with a separate post-image blur layer plus the central menu card.

Notes:
- every launcher viewport must reuse the same native app metadata so taskbar icons stay consistent
  on Linux and Windows, including detached child windows.
*/

use crate::ai_backend_supervisor::AiBackendHandle;
use crate::config;
use crate::launcher::background::{
    self, BackgroundImageLoadRequest, BackgroundImagePlan, LoadedBackgroundImage,
};
use crate::launcher::main_page;
use crate::launcher::new_project::window::NewProjectWindowState;
use crate::launcher::pages::base::{self, PageLayer, PageNavAction};
use crate::launcher::pages::export_page::ExportPageState;
use crate::launcher::pages::import_page::ImportPageState;
use crate::launcher::pages::open_page::OpenPageState;
use crate::launcher::pages::settings_page::SettingsPageState;
use crate::launcher::psd_import_window::PsdImportWindowState;
use crate::launcher::state::{LauncherOutcome, LauncherPage, LauncherState, UpdateNotification};
use crate::launcher::theme::VEIL_TINT;
use crate::tabs::wiki::WikiTabState;
use eframe::egui::{self, epaint};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc::Receiver};
use std::time::{Duration, Instant};

const BACKGROUND_COLUMNS: usize = 5;
const BACKGROUND_SPACING: f32 = 74.0;
const BACKGROUND_SPEED_PX_PER_SEC: f32 = 22.0;
const BACKGROUND_TILT_DEGREES: f32 = 4.0;
const BACKGROUND_FADE_IN_SECS: f32 = 1.0;
const BACKGROUND_PLACEHOLDER_FILL: egui::Color32 = egui::Color32::TRANSPARENT;
const BACKGROUND_PLACEHOLDER_STROKE: egui::Color32 = egui::Color32::TRANSPARENT;
const NEW_PROJECT_VIEWPORT_ID_SALT: &str = "launcher_new_project_window";
const PSD_IMPORT_VIEWPORT_ID_SALT: &str = "launcher_psd_import_window";
const WIKI_GUIDE_VIEWPORT_ID_SALT: &str = "launcher_wiki_guide_window";

struct BackgroundSlot {
    path: PathBuf,
    column: usize,
    base_y: f32,
    render_height: f32,
    blur_padding: f32,
    texture: Option<egui::TextureHandle>,
    loaded_at: Option<Instant>,
}

struct PendingBackgroundImage {
    rx: Receiver<Option<LoadedBackgroundImage>>,
}

pub struct LauncherApp {
    pub state: LauncherState,
    app_id: String,
    new_project_window: NewProjectWindowState,
    psd_import_window: PsdImportWindowState,
    wiki_guide_tab: WikiTabState,
    wiki_guide_window_open: bool,
    open_page: OpenPageState,
    import_page: ImportPageState,
    export_page: ExportPageState,
    settings_page: SettingsPageState,
    output_outcome: Arc<Mutex<Option<LauncherOutcome>>>,
    pub(crate) update_notification: Option<UpdateNotification>,
    pub(crate) ai_install_type: config::AiInstallType,
    update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
    pending_plan: Option<Receiver<BackgroundImagePlan>>,
    pending_images: Vec<PendingBackgroundImage>,
    background_plan: Option<BackgroundImagePlan>,
    background_slots: Vec<BackgroundSlot>,
    background_pending_slots: Vec<usize>,
    background_column_heights: [f32; BACKGROUND_COLUMNS],
    background_column_width: u32,
    background_decode_parallelism: usize,
    background_started_at: Instant,
    #[cfg(target_os = "windows")]
    maximize_root_window_on_first_frame: bool,
    /// On Windows, the new-project child viewport must be maximised on the first rendered
    /// frame rather than at window creation, to avoid the winit placement bug.
    #[cfg(target_os = "windows")]
    new_project_maximize_on_first_frame: bool,
    /// Same deferred-maximize guard for the PSD-import child viewport.
    #[cfg(target_os = "windows")]
    psd_import_maximize_on_first_frame: bool,
}

impl LauncherApp {
    pub fn new(
        projects_root: PathBuf,
        app_id: String,
        user_settings: &serde_json::Value,
        output_outcome: Arc<Mutex<Option<LauncherOutcome>>>,
        update_check_rx: Option<Receiver<Option<UpdateNotification>>>,
        ai_backend: AiBackendHandle,
    ) -> Self {
        let ai_install_type = config::AiInstallType::from_user_settings(user_settings);
        let mut app = Self {
            state: LauncherState::new(),
            app_id,
            new_project_window: NewProjectWindowState::new(projects_root.clone()),
            psd_import_window: PsdImportWindowState::new(projects_root.clone()),
            wiki_guide_tab: WikiTabState::new(),
            wiki_guide_window_open: false,
            open_page: OpenPageState::new(projects_root.clone(), user_settings),
            import_page: ImportPageState::new(projects_root.clone()),
            export_page: ExportPageState::new(projects_root.clone()),
            settings_page: SettingsPageState::new(projects_root.clone(), ai_install_type, ai_backend),
            output_outcome,
            update_notification: None,
            ai_install_type,
            update_check_rx,
            pending_plan: None,
            pending_images: Vec::new(),
            background_plan: None,
            background_slots: Vec::new(),
            background_pending_slots: Vec::new(),
            background_column_heights: [0.0; BACKGROUND_COLUMNS],
            background_column_width: 0,
            background_decode_parallelism: std::thread::available_parallelism()
                .map(|parallelism| parallelism.get())
                .unwrap_or(1),
            background_started_at: Instant::now(),
            #[cfg(target_os = "windows")]
            maximize_root_window_on_first_frame: true,
            #[cfg(target_os = "windows")]
            new_project_maximize_on_first_frame: true,
            #[cfg(target_os = "windows")]
            psd_import_maximize_on_first_frame: true,
        };
        app.pending_plan = Some(background::spawn_background_plan(projects_root));
        app
    }

    fn poll_workers(&mut self, ctx: &egui::Context, target_width: u32, viewport_height: f32) {
        self.poll_update_check(ctx);
        self.poll_plan(ctx, target_width, viewport_height);
        self.poll_images(ctx);
        self.kick_background_load(target_width, viewport_height);
    }

    fn poll_update_check(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.update_check_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(notification) => {
                self.update_notification = notification;
                self.update_check_rx = None;
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.update_check_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn poll_plan(&mut self, ctx: &egui::Context, target_width: u32, viewport_height: f32) {
        let mut should_clear = false;
        if let Some(rx) = &self.pending_plan {
            match rx.try_recv() {
                Ok(plan) => {
                    should_clear = true;
                    self.background_plan = Some(plan);
                    self.rebuild_background_layout(ctx, target_width, viewport_height);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    should_clear = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        if should_clear {
            self.pending_plan = None;
        }
    }

    fn poll_images(&mut self, ctx: &egui::Context) {
        let pending = std::mem::take(&mut self.pending_images);
        let mut remaining = Vec::with_capacity(pending.len());
        for pending_image in pending {
            match pending_image.rx.try_recv() {
                Ok(Some(image)) => {
                    self.append_loaded_background_image(ctx, image);
                    ctx.request_repaint();
                }
                Ok(None) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
                Err(std::sync::mpsc::TryRecvError::Empty) => remaining.push(pending_image),
            }
        }
        self.pending_images = remaining;
    }

    fn append_loaded_background_image(
        &mut self,
        ctx: &egui::Context,
        image: LoadedBackgroundImage,
    ) {
        let LoadedBackgroundImage {
            slot_index,
            path,
            blur_image,
        } = image;
        let blur_texture = ctx.load_texture(
            format!("launcher-bg-blur-{}", path.display()),
            blur_image,
            egui::TextureOptions::LINEAR,
        );
        let Some(slot) = self.background_slots.get_mut(slot_index) else {
            return;
        };
        slot.texture = Some(blur_texture);
        slot.loaded_at = Some(Instant::now());
    }

    fn rebuild_background_layout(
        &mut self,
        ctx: &egui::Context,
        target_width: u32,
        viewport_height: f32,
    ) {
        self.background_slots.clear();
        self.background_pending_slots.clear();
        self.pending_images.clear();
        self.background_column_heights = [0.0; BACKGROUND_COLUMNS];

        if let Some(plan) = self.background_plan.clone() {
            self.build_background_layout(plan, target_width);
        }

        self.kick_background_load(target_width, viewport_height);
        ctx.request_repaint();
    }

    fn kick_background_load(&mut self, target_width: u32, viewport_height: f32) {
        if target_width == 0 || viewport_height <= 0.0 {
            return;
        }

        while self.pending_images.len() < self.background_decode_parallelism {
            let Some(slot_index) = self.select_next_background_slot(viewport_height) else {
                break;
            };
            let slot = &self.background_slots[slot_index];
            self.pending_images.push(PendingBackgroundImage {
                rx: background::spawn_background_image_load(BackgroundImageLoadRequest {
                    slot_index,
                    path: slot.path.clone(),
                    target_width,
                }),
            });
        }
    }

    fn build_background_layout(&mut self, plan: BackgroundImagePlan, target_width: u32) {
        let mut column_offsets = [0.0; BACKGROUND_COLUMNS];

        for source in plan.entries {
            let Some((_, _, render_height, render_blur_padding)) =
                background::background_render_metrics(
                    target_width,
                    source.source_width,
                    source.source_height,
                )
            else {
                continue;
            };
            let column = shortest_column_index(&column_offsets);
            let base_y = column_offsets[column];
            let slot_index = self.background_slots.len();
            self.background_slots.push(BackgroundSlot {
                path: source.path,
                column,
                base_y,
                render_height,
                blur_padding: render_blur_padding,
                texture: None,
                loaded_at: None,
            });
            self.background_pending_slots.push(slot_index);
            column_offsets[column] += render_height + BACKGROUND_SPACING;
        }

        self.background_column_heights = column_offsets;
    }

    fn select_next_background_slot(&mut self, viewport_height: f32) -> Option<usize> {
        let mut best_list_index = None;
        let mut best_score = None;
        for (list_index, slot_index) in self.background_pending_slots.iter().copied().enumerate() {
            let Some(score) = self.background_slot_priority(slot_index, viewport_height) else {
                continue;
            };
            match best_score {
                Some(current) if score >= current => {}
                _ => {
                    best_score = Some(score);
                    best_list_index = Some(list_index);
                }
            }
        }

        best_list_index.map(|list_index| self.background_pending_slots.swap_remove(list_index))
    }

    fn background_slot_priority(
        &self,
        slot_index: usize,
        viewport_height: f32,
    ) -> Option<(u8, i32, usize)> {
        let slot = self.background_slots.get(slot_index)?;
        let column_total_height = self.background_column_heights[slot.column];
        if column_total_height <= 0.0 {
            return Some((2, i32::MAX, slot_index));
        }

        let offset = self.background_column_offset(slot.column, column_total_height);
        let can_repeat = column_total_height > viewport_height + BACKGROUND_SPACING;
        let cycle_origins = if can_repeat {
            [
                -column_total_height - offset,
                -offset,
                column_total_height - offset,
            ]
        } else {
            [-column_total_height - offset, -offset, f32::NAN]
        };

        let viewport_center = viewport_height * 0.5;
        let mut best = (2u8, i32::MAX, slot_index);
        for cycle_origin_y in cycle_origins {
            if cycle_origin_y.is_nan() {
                continue;
            }
            let top = cycle_origin_y + slot.base_y - slot.blur_padding;
            let bottom = cycle_origin_y + slot.base_y + slot.render_height + slot.blur_padding;
            let intersects = bottom >= 0.0 && top <= viewport_height;
            let center = (top + bottom) * 0.5;
            let distance = (center - viewport_center).abs().round() as i32;
            let score = if intersects {
                (0, distance, slot_index)
            } else if bottom < 0.0 {
                (1, (-bottom).round() as i32, slot_index)
            } else {
                (1, (top - viewport_height).round() as i32, slot_index)
            };
            if score < best {
                best = score;
            }
        }

        Some(best)
    }

    fn background_column_offset(&self, column: usize, column_total_height: f32) -> f32 {
        let elapsed = self.background_started_at.elapsed().as_secs_f32();
        let direction = if column.is_multiple_of(2) { 1.0 } else { -1.0 };
        (elapsed * BACKGROUND_SPEED_PX_PER_SEC * direction + column as f32 * 50.0)
            .rem_euclid(column_total_height.max(1.0))
    }

    fn draw_background(&self, ui: &mut egui::Ui, rect: egui::Rect) {
        ui.painter()
            .rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 18, 20));
        if self.background_slots.is_empty() {
            return;
        }
        let column_width = self.background_column_width.max(1) as f32;
        let total_spacing = BACKGROUND_SPACING * (BACKGROUND_COLUMNS as f32 + 1.0);
        let start_x = rect.left()
            + ((rect.width() - (column_width * BACKGROUND_COLUMNS as f32 + total_spacing))
                .max(0.0)
                * 0.5)
            + BACKGROUND_SPACING;

        for column in 0..BACKGROUND_COLUMNS {
            let clip_rect = egui::Rect::from_min_size(
                egui::pos2(
                    start_x + column as f32 * (column_width + BACKGROUND_SPACING),
                    rect.top(),
                ),
                egui::vec2(column_width, rect.height()),
            );

            let column_slots = self
                .background_slots
                .iter()
                .filter(|slot| slot.column == column)
                .collect::<Vec<_>>();
            let Some(column_total_height) = self.column_total_height(column) else {
                continue;
            };

            let offset = self.background_column_offset(column, column_total_height);

            let max_blur_padding = column_slots
                .iter()
                .map(|slot| slot.blur_padding)
                .fold(0.0, f32::max);
            let painter = ui
                .painter()
                .with_clip_rect(clip_rect.expand(max_blur_padding));
            let tilt_center = clip_rect.center();
            let can_repeat = column_total_height > rect.height() + BACKGROUND_SPACING;

            self.paint_column_cycle(
                &painter,
                &column_slots,
                clip_rect,
                column_width,
                tilt_center,
                -column_total_height - offset,
            );
            self.paint_column_cycle(
                &painter,
                &column_slots,
                clip_rect,
                column_width,
                tilt_center,
                -offset,
            );
            if can_repeat {
                self.paint_column_cycle(
                    &painter,
                    &column_slots,
                    clip_rect,
                    column_width,
                    tilt_center,
                    column_total_height - offset,
                );
            }
        }

        // Global veil between the blurred background stack and the UI card.
        ui.painter().rect_filled(rect, 0.0, VEIL_TINT);
    }

    fn draw_pages(&mut self, ctx: &egui::Context, ui: &mut egui::Ui, rect: egui::Rect) {
        self.state.settle_transition_if_finished();
        let layers = self.visible_page_layers(rect.width());
        let mut nav_action = None;
        for layer in layers {
            let mut page_ui = base::make_layer_ui(ui, rect.shrink2(egui::vec2(24.0, 24.0)), layer);
            let action = match layer.page {
                LauncherPage::Main => main_page::show(self, &mut page_ui),
                LauncherPage::OpenProject => self.open_page.show(&mut page_ui),
                LauncherPage::ImportChapter => self.import_page.show(&mut page_ui),
                LauncherPage::ExportChapter => self.export_page.show(&mut page_ui),
                LauncherPage::Settings => self.settings_page.show(&mut page_ui),
            };
            nav_action = nav_action.or(action);
        }

        if let Some(action) = nav_action {
            self.apply_nav_action(ctx, action);
        }
    }

    fn visible_page_layers(&self, viewport_width: f32) -> Vec<PageLayer> {
        if let Some(transition) = &self.state.page_transition {
            transition
                .visible_layers(viewport_width)
                .into_iter()
                .collect()
        } else {
            vec![PageLayer::stationary(self.state.current_page)]
        }
    }

    fn apply_nav_action(&mut self, ctx: &egui::Context, action: PageNavAction) {
        match action {
            PageNavAction::Open(target) => {
                self.close_settings_resources_if_leaving(target);
                self.state.begin_transition(target);
            }
            PageNavAction::OpenNewProjectWindow => self.state.new_project_window_open = true,
            PageNavAction::BackToMain => {
                self.close_settings_resources_if_leaving(LauncherPage::Main);
                self.state.begin_transition(LauncherPage::Main);
            }
            PageNavAction::ProjectsRootChanged(projects_root) => {
                self.apply_projects_root(ctx, projects_root);
            }
            PageNavAction::AiInstallTypeChanged(install_type) => {
                self.ai_install_type = install_type;
                self.settings_page.set_ai_install_type(install_type);
            }
            PageNavAction::OpenProject(selection) => {
                self.close_settings_resources_if_leaving(LauncherPage::Main);
                if let Ok(mut output) = self.output_outcome.lock() {
                    *output = Some(LauncherOutcome::OpenProject(selection.clone()));
                }
                if let Err(err) = crate::launcher::pages::open_page::persist_last_selection_values(
                    &selection.title,
                    &selection.chapter,
                ) {
                    crate::runtime_log::log_warn(format!(
                        "[launcher-open] failed to persist last selection: {err:#}"
                    ));
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            PageNavAction::StartUpdate => {
                self.close_settings_resources_if_leaving(LauncherPage::Main);
                if let Ok(mut output) = self.output_outcome.lock() {
                    *output = Some(LauncherOutcome::StartUpdate);
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn close_settings_resources_if_leaving(&mut self, target: LauncherPage) {
        if self.state.current_page == LauncherPage::Settings && target != LauncherPage::Settings {
            self.settings_page.close_python_console();
        }
    }

    fn apply_projects_root(&mut self, ctx: &egui::Context, projects_root: PathBuf) {
        self.open_page.set_projects_root(projects_root.clone());
        self.import_page.set_projects_root(projects_root.clone());
        self.export_page.set_projects_root(projects_root.clone());
        self.settings_page.set_projects_root(projects_root.clone());
        self.new_project_window
            .set_projects_root(projects_root.clone());
        self.psd_import_window
            .set_projects_root(projects_root.clone());
        self.pending_plan = Some(background::spawn_background_plan(projects_root));
        self.background_plan = None;
        self.background_slots.clear();
        self.background_pending_slots.clear();
        self.pending_images.clear();
        self.background_column_heights = [0.0; BACKGROUND_COLUMNS];
        ctx.request_repaint();
    }

    fn draw_new_project_window(&mut self, ctx: &egui::Context) {
        if !self.state.new_project_window_open {
            // Reset the flag so the window is maximised again the next time it opens.
            #[cfg(target_os = "windows")]
            {
                self.new_project_maximize_on_first_frame = true;
            }
            return;
        }

        let viewport_id = egui::ViewportId::from_hash_of(NEW_PROJECT_VIEWPORT_ID_SALT);
        let mut keep_open = true;
        let builder = crate::launcher::apply_launcher_window_metadata(
            egui::ViewportBuilder::default()
                .with_title("Новый проект")
                .with_inner_size([1180.0, 760.0])
                .with_min_inner_size([1000.0, 680.0])
                .with_app_id(&self.app_id)
                .with_resizable(true)
                .with_close_button(true)
                .with_minimize_button(true)
                .with_maximize_button(true)
                .with_active(true),
        );
        // On Linux/macOS the native hint is reliable; on Windows it misplaces the window,
        // so maximisation is deferred to the first rendered frame via ViewportCommand.
        #[cfg(not(target_os = "windows"))]
        let builder = builder.with_maximized(true);
        ctx.show_viewport_immediate(viewport_id, builder, |ctx, class| {
            #[cfg(target_os = "windows")]
            if self.new_project_maximize_on_first_frame {
                self.new_project_maximize_on_first_frame = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                ctx.request_repaint();
            }
            keep_open = self.new_project_window.show(ctx, class);
        });
        if let Some(selection) = self.new_project_window.take_open_project_selection() {
            if let Ok(mut output) = self.output_outcome.lock() {
                *output = Some(LauncherOutcome::OpenProject(selection.clone()));
            }
            if let Err(err) = crate::launcher::pages::open_page::persist_last_selection_values(
                &selection.title,
                &selection.chapter,
            ) {
                crate::runtime_log::log_warn(format!(
                    "[launcher-open] failed to persist last selection after save: {err:#}"
                ));
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            keep_open = false;
        }
        if self.new_project_window.take_open_wiki_guide_requested() {
            self.wiki_guide_window_open = true;
        }
        self.state.new_project_window_open = keep_open;
    }

    fn draw_psd_import_window(&mut self, ctx: &egui::Context) {
        if !self.state.psd_import_window_open {
            // Reset the flag so the window is maximised again the next time it opens.
            #[cfg(target_os = "windows")]
            {
                self.psd_import_maximize_on_first_frame = true;
            }
            return;
        }

        let viewport_id = egui::ViewportId::from_hash_of(PSD_IMPORT_VIEWPORT_ID_SALT);
        let mut keep_open = true;
        let builder = crate::launcher::apply_launcher_window_metadata(
            egui::ViewportBuilder::default()
                .with_title("Импорт из PSD")
                .with_inner_size([1360.0, 820.0])
                .with_min_inner_size([1120.0, 720.0])
                .with_app_id(&self.app_id)
                .with_resizable(true)
                .with_close_button(true)
                .with_minimize_button(true)
                .with_maximize_button(true)
                .with_active(true),
        );
        #[cfg(not(target_os = "windows"))]
        let builder = builder.with_maximized(true);
        ctx.show_viewport_immediate(viewport_id, builder, |ctx, class| {
            #[cfg(target_os = "windows")]
            if self.psd_import_maximize_on_first_frame {
                self.psd_import_maximize_on_first_frame = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
                ctx.request_repaint();
            }
            keep_open = self.psd_import_window.show(ctx, class);
        });
        if let Some(selection) = self.psd_import_window.take_open_project_selection() {
            if let Ok(mut output) = self.output_outcome.lock() {
                *output = Some(LauncherOutcome::OpenProject(selection.clone()));
            }
            if let Err(err) = crate::launcher::pages::open_page::persist_last_selection_values(
                &selection.title,
                &selection.chapter,
            ) {
                crate::runtime_log::log_warn(format!(
                    "[launcher-open] failed to persist last selection after psd import: {err:#}"
                ));
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            keep_open = false;
        }
        self.state.psd_import_window_open = keep_open;
    }

    fn draw_wiki_guide_window(&mut self, ctx: &egui::Context) {
        if !self.wiki_guide_window_open {
            return;
        }

        let viewport_id = egui::ViewportId::from_hash_of(WIKI_GUIDE_VIEWPORT_ID_SALT);
        let mut keep_open = true;
        ctx.show_viewport_immediate(
            viewport_id,
            crate::launcher::apply_launcher_window_metadata(
                egui::ViewportBuilder::default()
                    .with_title("Гайд")
                    .with_inner_size([980.0, 760.0])
                    .with_min_inner_size([720.0, 540.0])
                    .with_app_id(&self.app_id)
                    .with_resizable(true)
                    .with_close_button(true)
                    .with_minimize_button(true)
                    .with_maximize_button(true)
                    .with_active(true),
            ),
            |ctx, _class| {
                keep_open = !ctx.input(|input| input.viewport().close_requested());
                ctx.request_repaint_after(Duration::from_millis(100));
                egui::CentralPanel::default().show(ctx, |ui| {
                    self.wiki_guide_tab.draw(ui);
                });
            },
        );
        self.wiki_guide_window_open = keep_open;
    }

    fn column_total_height(&self, column: usize) -> Option<f32> {
        let height = self
            .background_column_heights
            .get(column)
            .copied()
            .unwrap_or(0.0);
        if height > 0.0 { Some(height) } else { None }
    }

    fn paint_column_cycle(
        &self,
        painter: &egui::Painter,
        slots: &[&BackgroundSlot],
        clip_rect: egui::Rect,
        column_width: f32,
        tilt_center: egui::Pos2,
        cycle_origin_y: f32,
    ) {
        for slot in slots {
            let y = clip_rect.top() + cycle_origin_y + slot.base_y;
            let image_rect = egui::Rect::from_min_size(
                egui::pos2(clip_rect.left(), y),
                egui::vec2(column_width, slot.render_height.max(1.0)),
            );
            let blur_rect = image_rect.expand(slot.blur_padding);
            if blur_rect.bottom() < clip_rect.top() || blur_rect.top() > clip_rect.bottom() {
                continue;
            }
            if let Some(texture) = &slot.texture {
                paint_rotated_image(
                    painter,
                    texture.id(),
                    blur_rect,
                    tilt_center,
                    BACKGROUND_TILT_DEGREES.to_radians(),
                    egui::Color32::from_white_alpha(
                        (slot_fade_alpha(slot.loaded_at) * 255.0).round() as u8,
                    ),
                );
            } else {
                paint_rotated_placeholder(
                    painter,
                    blur_rect,
                    tilt_center,
                    BACKGROUND_TILT_DEGREES.to_radians(),
                );
            }
        }
    }
}

fn slot_fade_alpha(loaded_at: Option<Instant>) -> f32 {
    loaded_at
        .map(|loaded_at| {
            (loaded_at.elapsed().as_secs_f32() / BACKGROUND_FADE_IN_SECS).clamp(0.0, 1.0)
        })
        .unwrap_or(1.0)
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "windows")]
        if self.maximize_root_window_on_first_frame {
            self.maximize_root_window_on_first_frame = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            ctx.request_repaint();
        }

        let viewport_rect = ctx.content_rect();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| {
                let rect = viewport_rect;
                let target_width = calc_column_width(rect.width());
                if self.background_column_width != target_width {
                    self.background_column_width = target_width;
                    if self.background_plan.is_some() {
                        self.rebuild_background_layout(ctx, target_width, rect.height());
                    }
                }

                self.poll_workers(ctx, target_width, rect.height());
                self.draw_background(ui, rect);
                self.draw_pages(ctx, ui, rect);
            });
        self.draw_new_project_window(ctx);
        self.draw_psd_import_window(ctx);
        self.draw_wiki_guide_window(ctx);

        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn calc_column_width(total_width: f32) -> u32 {
    let total_spacing = BACKGROUND_SPACING * (BACKGROUND_COLUMNS as f32 + 1.0);
    ((total_width - total_spacing) / BACKGROUND_COLUMNS as f32)
        .max(60.0)
        .round() as u32
}

fn paint_rotated_image(
    painter: &egui::Painter,
    texture_id: egui::TextureId,
    rect: egui::Rect,
    rotation_center: egui::Pos2,
    angle_radians: f32,
    tint: egui::Color32,
) {
    let uv = [
        egui::pos2(0.0, 0.0),
        egui::pos2(1.0, 0.0),
        egui::pos2(1.0, 1.0),
        egui::pos2(0.0, 1.0),
    ];
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];

    let mut mesh = epaint::Mesh::with_texture(texture_id);
    let base = mesh.vertices.len() as u32;
    for (corner, uv) in corners.into_iter().zip(uv) {
        mesh.vertices.push(epaint::Vertex {
            pos: rotate_pos(corner, rotation_center, angle_radians),
            uv,
            color: tint,
        });
    }
    mesh.indices
        .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    painter.add(epaint::Shape::mesh(mesh));
}

fn paint_rotated_placeholder(
    painter: &egui::Painter,
    rect: egui::Rect,
    rotation_center: egui::Pos2,
    angle_radians: f32,
) {
    let corners = [
        rotate_pos(rect.left_top(), rotation_center, angle_radians),
        rotate_pos(rect.right_top(), rotation_center, angle_radians),
        rotate_pos(rect.right_bottom(), rotation_center, angle_radians),
        rotate_pos(rect.left_bottom(), rotation_center, angle_radians),
    ];
    painter.add(epaint::Shape::convex_polygon(
        corners.to_vec(),
        BACKGROUND_PLACEHOLDER_FILL,
        egui::Stroke::new(1.0, BACKGROUND_PLACEHOLDER_STROKE),
    ));
}

fn shortest_column_index(column_offsets: &[f32; BACKGROUND_COLUMNS]) -> usize {
    let mut best_index = 0usize;
    let mut best_height = f32::INFINITY;
    for (index, height) in column_offsets.iter().copied().enumerate() {
        if height < best_height {
            best_height = height;
            best_index = index;
        }
    }
    best_index
}

fn rotate_pos(pos: egui::Pos2, center: egui::Pos2, angle_radians: f32) -> egui::Pos2 {
    let delta = pos - center;
    let (sin, cos) = angle_radians.sin_cos();
    egui::pos2(
        center.x + delta.x * cos - delta.y * sin,
        center.y + delta.x * sin + delta.y * cos,
    )
}
