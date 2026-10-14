/*
File: web_entry.rs

Purpose:
Browser (wasm32) entry point. Boots the eframe `WebRunner` with the ManhwaStudio
launcher, installs the in-memory storage backend (the web session store), seeds
an always-available generated demo chapter into it, and swaps to the editor
(`StudioApp`) when the launcher requests opening a project.

The web build is intentionally backend-independent (targets static hosting such
as GitHub Pages): no AI backend, no downloaders — only local file/archive import
and the bundled demo chapter. Cross-origin isolation for threads is retrofitted
by `web/coi-serviceworker.js`.

Key items:
- start(): wasm `fn main` calls this; pins the owned-document format to JSON (web is
  Dev-only) and starts the eframe web app.
- WebApp: Launcher | Editor phases behind one eframe::App.
- seed_test_chapter(): writes the generated demo chapter into storage.

Notes:
Only compiled on `wasm32`. Native startup lives in `lib.rs` and the main window in `shell/`; the
web entry shares only the per-screen style (`shell::style`). Porting the full shell here (screen
state machine, teardown worker) is deferred: the web load is synchronous against virtual storage
and `StudioApp` is dropped in place without its teardown. The studio's outbound queue
(`StudioApp::take_outputs`) is drained every frame and ignored (logged): a close or reload does
nothing here.
*/

use std::io::Cursor;
use std::sync::Arc;

use wasm_bindgen::JsCast as _;

/// DOM id of the `<canvas>` the app renders into (see `web/index.html`).
const CANVAS_ID: &str = "the_canvas_id";
/// Virtual projects root inside the storage backend.
const PROJECTS_ROOT: &str = "/projects";

/// Boots the web app: panic hook → document format (JSON) → storage backend → demo chapter →
/// launcher.
pub fn start() {
    console_error_panic_hook::set_once();
    // The web build is Dev-only (no Prod storage mode): owned documents are always JSON.
    // JSON is already `ms_docstore`'s default; setting it explicitly pins the contract so
    // a future default change cannot silently switch the browser build to a store it
    // does not support.
    ms_docstore::set_default_format(ms_docstore::DocFormat::Json);

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
                    // The launcher screen's complete style, built from scratch (`shell::style`).
                    crate::shell::style::apply(&cc.egui_ctx, crate::shell::style::StyleScreen::Launcher);
                    // GPU shader layers (the PS editor's «Коррекция») on WebGL2. WebGL1 is
                    // refused by the library; the panel then reports the correction as
                    // unavailable. No teardown: the browser frees the WebGL context with the page.
                    match cc.gl.as_deref() {
                        Some(gl) => {
                            if let Err(error) = egui_shader_layers::install_glow(&cc.egui_ctx, gl) {
                                console_error(&format!("shader layers disabled: {error}"));
                            }
                        }
                        None => console_error("no glow context: shader layers are disabled"),
                    }
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
/// `StudioApp`. eframe drives a single `App`, so the two phases live behind this
/// enum and swap in place.
#[allow(clippy::large_enum_variant)]
enum WebApp {
    Launcher(Box<crate::launcher::app::LauncherApp>),
    Editor(Box<crate::studio::StudioApp>),
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
            WebApp::Launcher(app) => {
                app.show(ui);
                // Take the launcher's exit; on "open project" swap to the editor.
                let selection = match app.take_outcome() {
                    Some(crate::launcher::state::LauncherOutcome::OpenProject(sel)) => Some(sel),
                    // No update flow on web; ignore.
                    Some(crate::launcher::state::LauncherOutcome::StartUpdate) | None => None,
                    // Image-set mode has no web entry point (plan D10) and the web launcher
                    // has no button producing it; log the impossible outcome.
                    Some(crate::launcher::state::LauncherOutcome::OpenImages(paths)) => {
                        console_error(&format!("open images is unavailable on web; ignored {} path(s)", paths.len()));
                        None
                    }
                };
                if let Some(sel) = selection {
                    match build_editor_from_selection(&sel) {
                        Ok(editor) => {
                            // The studio's complete style replaces the launcher's wholesale.
                            crate::shell::style::apply(ctx, crate::shell::style::StyleScreen::Studio);
                            // The editor's only UI zoom is the interface-scale setting.
                            ctx.options_mut(|options| options.zoom_with_keyboard = false);
                            *self = WebApp::Editor(Box::new(editor));
                        }
                        Err(err) => console_error(&format!("open project failed: {err}")),
                    }
                }
            }
            WebApp::Editor(app) => {
                app.ui(ui, frame);
                // The web entry has no screen switching (see the file notes): a title is applied
                // like the native shell does; close, reload and decline are drained and ignored,
                // logged (S-037).
                for output in app.take_outputs() {
                    match output {
                        crate::studio::StudioOutput::Title(title) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Title(title)),
                        other => crate::runtime_log::log_info(format!("[web] studio output {other:?} ignored: the web entry has no window host")),
                    }
                }
            }
        }
    }
}

/// Constructs the launcher rooted at the virtual projects folder, AI disabled.
fn build_web_launcher() -> WebApp {
    let _ = crate::storage::storage().create_dir_all(PROJECTS_ROOT);
    let settings = serde_json::json!({ "General": { "projects_dir": PROJECTS_ROOT } });
    // The web storage is in memory, so preparing the entry on the page thread does not block
    // on real I/O.
    let entry = crate::launcher::prepare_entry_with_settings(
        settings,
        crate::ai_backend_supervisor::AiBackendHandle::disabled(),
        // The web build has no installed copy and no OS records (no System registration tab);
        // the version core is still reduced exactly as `startup::launcher_host` does.
        crate::launcher::state::LauncherHost { ignore_installed: false, version_core: crate::version_format::version_core(env!("CARGO_PKG_VERSION")) },
        false,
    );
    WebApp::Launcher(Box::new(crate::launcher::app::LauncherApp::new(entry)))
}

/// Loads the selected project and constructs the editor (AI disabled). Resume vs fresh is
/// decided by `shell::chapter_load`, the one owner shared with the native studio.
///
/// # Errors
/// Returns a message if the `ProjectData` load fails.
fn build_editor_from_selection(
    sel: &crate::launcher::state::OpenProjectSelection,
) -> std::result::Result<crate::studio::StudioApp, String> {
    let settings = serde_json::Value::Object(serde_json::Map::new());
    let project = crate::shell::chapter_load::load_chapter(&sel.project_dir, &settings)
        .map_err(|e| format!("ProjectData::load: {e:#}"))?;
    // Single-threaded page with an in-memory store: the studio's pre-reads run inline here,
    // where the native shell runs them on its open worker.
    let bootstrap = crate::studio::StudioBootstrap::build_blocking(&project);
    Ok(crate::studio::StudioApp::new(project, bootstrap, crate::ai_backend_supervisor::AiBackendHandle::disabled()))
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
