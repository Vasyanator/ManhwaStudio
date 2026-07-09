/*
File: web_entry.rs

Purpose:
Browser (wasm32) entry point. Boots the eframe `WebRunner` with the ManhwaStudio
launcher, installs the in-memory storage backend (the web session store), seeds
an always-available generated demo chapter into it, and swaps to the editor
(`MangaApp`) when the launcher requests opening a project.

The web build is intentionally backend-independent (targets static hosting such
as GitHub Pages): no AI backend, no downloaders — only local file/archive import
and the bundled demo chapter. Cross-origin isolation for threads is retrofitted
by `web/coi-serviceworker.js`.

Key items:
- start(): wasm `fn main` calls this; starts the eframe web app.
- WebApp: Launcher | Editor phases behind one eframe::App.
- seed_test_chapter(): writes the generated demo chapter into storage.

Notes:
Only compiled on `wasm32`. Native startup lives in `main.rs`.
*/

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use wasm_bindgen::JsCast as _;

/// DOM id of the `<canvas>` the app renders into (see `web/index.html`).
const CANVAS_ID: &str = "the_canvas_id";
/// Virtual projects root inside the storage backend.
const PROJECTS_ROOT: &str = "/projects";

/// Boots the web app: panic hook → storage backend → demo chapter → launcher.
pub fn start() {
    console_error_panic_hook::set_once();

    if crate::storage::install(Arc::new(ms_storage::MemStorage::new())).is_err() {
        console_error("storage backend was already installed");
    }
    // There is no editable `locale/` folder in the browser, so the on-disk locale
    // layer (`locale_store`) is compiled out on wasm; install the embedded catalog
    // for the default UI locale directly. `"ru"` is a valid tag by construction, so
    // the parse only errors on a build bug; both failure paths are logged.
    match ms_i18n::LocaleTag::parse("ru") {
        Ok(tag) => {
            if let Err(err) = ms_i18n::set_locale(&tag) {
                console_error(&format!("failed to install embedded locale: {err}"));
            }
        }
        Err(err) => console_error(&format!("failed to build default locale tag: {err}")),
    }
    if let Err(err) = seed_test_chapter() {
        console_error(&format!("failed to seed demo chapter: {err}"));
    }

    let web_options = eframe::WebOptions::default();
    wasm_bindgen_futures::spawn_local(async move {
        let Some(canvas) = canvas_by_id(CANVAS_ID) else {
            console_error(&format!("canvas element #{CANVAS_ID} not found in the page"));
            return;
        };
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| {
                    cc.egui_ctx.set_theme(egui::Theme::Dark);
                    crate::launcher::theme::configure_context(&cc.egui_ctx);
                    Ok(Box::new(build_web_launcher()))
                }),
            )
            .await;
        if let Err(err) = result {
            console_error(&format!("eframe web start failed: {err:?}"));
        }
    });
}

/// Resolves the render canvas from the DOM, or `None` if absent / not a canvas.
fn canvas_by_id(id: &str) -> Option<web_sys::HtmlCanvasElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id(id)?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .ok()
}

/// Logs an error to the browser devtools console.
fn console_error(message: &str) {
    web_sys::console::error_1(&message.into());
}

/// Top-level web app: the launcher until the user opens a project, then the full
/// `MangaApp`. eframe drives a single `App`, so the two phases live behind this
/// enum and swap in place.
#[allow(clippy::large_enum_variant)]
enum WebApp {
    Launcher {
        app: Box<crate::launcher::app::LauncherApp>,
        outcome: Arc<Mutex<Option<crate::launcher::state::LauncherOutcome>>>,
    },
    Editor(Box<crate::app::MangaApp>),
}

impl eframe::App for WebApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // egui 0.35 replaced `App::update(&Context, …)` with `App::ui(&mut Ui, …)`. WebApp is a
        // thin phase switch, so it forwards the same root `Ui` to whichever inner app is active
        // (their own `App::ui`), while keeping a borrowed `Context` handle for the theme swap on
        // the launcher→editor transition.
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        match self {
            WebApp::Launcher { app, outcome } => {
                app.ui(ui, frame);
                // Poll the launcher's outcome; on "open project" swap to the editor.
                let selection = match outcome.lock() {
                    Ok(mut guard) => match guard.take() {
                        Some(crate::launcher::state::LauncherOutcome::OpenProject(sel)) => Some(sel),
                        // No update flow on web; ignore.
                        Some(crate::launcher::state::LauncherOutcome::StartUpdate) | None => None,
                    },
                    Err(_) => None,
                };
                if let Some(sel) = selection {
                    match build_editor_from_selection(&sel) {
                        Ok(editor) => {
                            ctx.set_theme(egui::Theme::Dark);
                            *self = WebApp::Editor(Box::new(editor));
                        }
                        Err(err) => console_error(&format!("open project failed: {err}")),
                    }
                }
            }
            WebApp::Editor(app) => app.ui(ui, frame),
        }
    }
}

/// Constructs the launcher rooted at the virtual projects folder, AI disabled.
fn build_web_launcher() -> WebApp {
    let _ = crate::storage::storage().create_dir_all(PROJECTS_ROOT);
    let settings = serde_json::json!({ "General": { "projects_dir": PROJECTS_ROOT } });
    let outcome = Arc::new(Mutex::new(None));
    let app = crate::launcher::app::LauncherApp::new(
        std::path::PathBuf::from(PROJECTS_ROOT),
        "manhwastudio_web".to_string(),
        &settings,
        Arc::clone(&outcome),
        None,
        crate::ai_backend_supervisor::AiBackendHandle::disabled(),
    );
    WebApp::Launcher {
        app: Box::new(app),
        outcome,
    }
}

/// Loads the selected project and constructs the editor (AI disabled).
///
/// # Errors
/// Returns a message if `ProjectData::load` fails.
fn build_editor_from_selection(
    sel: &crate::launcher::state::OpenProjectSelection,
) -> std::result::Result<crate::app::MangaApp, String> {
    let settings = serde_json::Value::Object(serde_json::Map::new());
    let project = if sel.resume_unsaved {
        crate::project::ProjectData::load_resume_unsaved(&sel.project_dir, &settings)
    } else {
        crate::project::ProjectData::load(&sel.project_dir, &settings)
    }
    .map_err(|e| format!("ProjectData::load: {e:#}"))?;
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Ok(crate::app::MangaApp::new(
        project,
        crate::ai_backend_supervisor::AiBackendHandle::disabled(),
        flag,
    ))
}

/// Writes the generated demo chapter into storage if it is not already present,
/// so the launcher always has one openable chapter.
///
/// # Errors
/// Returns a message if a storage write or PNG encode fails.
fn seed_test_chapter() -> std::result::Result<(), String> {
    const PAGES: u32 = 4;
    let store = crate::storage::storage();
    let src = format!("{PROJECTS_ROOT}/Демо/Тестовая глава/src");
    store.create_dir_all(&src).map_err(|e| e.to_string())?;
    for idx in 1..=PAGES {
        let path = format!("{src}/{idx:03}.png");
        if store.exists(&path) {
            continue;
        }
        let img = render_test_page(idx);
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        store.write(&path, &buf).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Renders one distinguishable demo page (light page, dark frame, colored header
/// band cycling by page, and `idx` filled squares as a visual page marker).
fn render_test_page(idx: u32) -> image::RgbaImage {
    const W: u32 = 800;
    const H: u32 = 1200;
    const BORDER: u32 = 6;
    let mut img = image::RgbaImage::from_pixel(W, H, image::Rgba([245, 245, 245, 255]));

    let dark = image::Rgba([40, 40, 40, 255]);
    // Frame.
    for y in 0..H {
        for x in 0..W {
            if x < BORDER || x >= W - BORDER || y < BORDER || y >= H - BORDER {
                img.put_pixel(x, y, dark);
            }
        }
    }
    // Header band, colour cycles per page.
    let bands = [
        [80, 140, 220],
        [220, 120, 80],
        [90, 190, 120],
        [190, 110, 200],
    ];
    let c = bands[(idx as usize - 1) % bands.len()];
    let band = image::Rgba([c[0], c[1], c[2], 255]);
    for y in BORDER..(BORDER + 120) {
        for x in BORDER..(W - BORDER) {
            img.put_pixel(x, y, band);
        }
    }
    // `idx` filled squares as a page marker.
    let (sq, gap, y0) = (60u32, 20u32, BORDER + 160);
    for k in 0..idx {
        let x0 = BORDER + 40 + k * (sq + gap);
        if x0 + sq >= W - BORDER {
            break;
        }
        for y in y0..y0 + sq {
            for x in x0..x0 + sq {
                img.put_pixel(x, y, dark);
            }
        }
    }
    img
}
